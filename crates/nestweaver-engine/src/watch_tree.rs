//! Filesystem subscription for a whole tree that survives unreadable
//! subdirectories (nw-651 / nw-664 on Linux).
//!
//! Both watchers subscribe with `watch(root, RecursiveMode::Recursive)`. On
//! macOS that is ONE FSEvents stream on the root, so an unreadable
//! subdirectory costs nothing. On Linux, notify 8's inotify backend walks the
//! tree and adds one watch per directory, and the first directory it cannot
//! watch (EACCES) fails the WHOLE call — the watcher then never started, so a
//! repo or vault holding a single `chmod 000` directory silently lost every
//! live update. That is the exact case nw-651 / nw-664 promise to handle:
//! the unreadable directory disclosed, everything else kept.
//!
//! What notify 8.2's inotify `add_watch` does on that error (checked in its
//! source): watches added before the failing directory are NOT rolled back
//! (they stay registered as recursive), directories later in the walk are
//! never watched, and the error names the failing directory. A directory
//! created later under a recursively watched one is watched automatically;
//! failures there are dropped silently.
//!
//! [`TreeWatch::start`] therefore tries the recursive watch first — the only
//! path on macOS and on any healthy tree, so neither changes. Only when it
//! fails about a directory BELOW the root does it drop that partial
//! registration and watch the tree piecewise: every directory whose subtree
//! can be watched recursively gets one recursive watch; a directory with an
//! unwatchable directory somewhere beneath it gets a NON-recursive watch and
//! its children are handled the same way; the unwatchable directories are
//! skipped and returned for the watcher to disclose. A failure about the root
//! itself (or of any other kind, e.g. the inotify watch limit) still fails
//! startup, as before.
//!
//! A non-recursive watch does not pick up directories created beneath it, so
//! the watcher hands every batch to [`TreeWatch::adopt_new_dirs`], which
//! watches such a directory and returns the files already inside it — they
//! may have been written before its watch existed, and would otherwise be
//! lost.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use notify::{RecursiveMode, Watcher};

/// A notify watcher subscribed to one tree, plus what the piecewise fallback
/// could not watch.
pub(crate) struct TreeWatch<W: Watcher> {
    inner: W,
    root: PathBuf,
    /// Directories watched NON-recursively because something beneath them is
    /// unwatchable. Empty unless the fallback ran.
    shallow: HashSet<PathBuf>,
    /// Directories watched recursively by the fallback (or adopted since).
    recursive: HashSet<PathBuf>,
    /// Directories that could not be watched, with the error, sorted.
    unwatchable: Vec<(PathBuf, String)>,
    /// Test seam: make every RECURSIVE watch fail the way inotify does when
    /// the subtree holds an unreadable directory — FSEvents (macOS) never
    /// fails that way, so without this the fallback is untestable there.
    #[cfg(test)]
    emulate_inotify: bool,
}

/// Whether `error` is about a directory strictly below `dir`: a subdirectory
/// the recursive walk could not watch (or that vanished mid-walk).
fn is_about_subpath(error: &notify::Error, dir: &Path) -> bool {
    matches!(
        error.kind,
        notify::ErrorKind::Io(_) | notify::ErrorKind::PathNotFound
    ) && !error.paths.is_empty()
        && error
            .paths
            .iter()
            .all(|path| path != dir && path.starts_with(dir))
}

/// Whether `error` is about `dir` itself being unwatchable.
fn is_about_itself(error: &notify::Error, dir: &Path) -> bool {
    matches!(
        error.kind,
        notify::ErrorKind::Io(_) | notify::ErrorKind::PathNotFound
    ) && error.paths.iter().any(|path| path == dir)
}

/// The error text for a disclosure row: the OS error alone, since the row
/// already names the directory.
fn error_detail(error: &notify::Error) -> String {
    match &error.kind {
        notify::ErrorKind::Io(io) => io.to_string(),
        _ => error.to_string(),
    }
}

/// Whether `error` is a not-found. notify's inotify backend reports a path
/// that vanished mid-walk as `PathNotFound` WITHOUT naming it.
fn is_not_found(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::PathNotFound => true,
        notify::ErrorKind::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

/// Whether `dir` itself was removed while being watched.
fn vanished(error: &notify::Error, dir: &Path) -> bool {
    is_not_found(error) && std::fs::symlink_metadata(dir).is_err()
}

/// Whether the failure is below `dir`: named there, or an unnamed
/// not-found while `dir` itself still exists (a child removed mid-walk).
fn failed_below(error: &notify::Error, dir: &Path) -> bool {
    is_about_subpath(error, dir) || (is_not_found(error) && error.paths.is_empty() && dir.exists())
}

/// Child directories of `dir`, not following symlinks (a recursive watch of
/// a readable child still follows them, as notify always has).
fn child_dirs(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Every regular file under `dir` that can be listed.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(kind) if kind.is_file() => files.push(entry.path()),
                _ => {}
            }
        }
    }
    files.sort();
    files
}

/// The first directory at or below `dir` that cannot be listed, as inotify
/// would meet it (test seam only).
#[cfg(test)]
fn first_unlistable_dir(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        match child_dirs(&dir) {
            Ok(children) => stack.extend(children.into_iter().rev()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Some(dir);
            }
            Err(_) => {}
        }
    }
    None
}

impl<W: Watcher> TreeWatch<W> {
    /// Subscribe `inner` to `root` recursively, falling back to the piecewise
    /// strategy (module docs) when only directories below the root are
    /// unwatchable.
    pub(crate) fn start(inner: W, root: &Path) -> Result<Self, notify::Error> {
        Self::unsubscribed(inner, root).subscribe()
    }

    /// [`Self::start`] with the inotify-emulating test seam switched on.
    #[cfg(test)]
    pub(crate) fn start_emulating_inotify(inner: W, root: &Path) -> Result<Self, notify::Error> {
        let mut tree = Self::unsubscribed(inner, root);
        tree.emulate_inotify = true;
        tree.subscribe()
    }

    fn unsubscribed(inner: W, root: &Path) -> Self {
        Self {
            inner,
            root: root.to_path_buf(),
            shallow: HashSet::new(),
            recursive: HashSet::new(),
            unwatchable: Vec::new(),
            #[cfg(test)]
            emulate_inotify: false,
        }
    }

    fn subscribe(self) -> Result<Self, notify::Error> {
        let root = self.root.clone();
        let root = root.as_path();
        let mut tree = self;
        let error = match tree.watch(root, RecursiveMode::Recursive) {
            Ok(()) => return Ok(tree),
            Err(error) if failed_below(&error, root) => error,
            Err(error) => return Err(error),
        };
        tracing::warn!(
            root = %root.display(),
            %error,
            "recursive watch failed below the root; watching every readable \
             directory individually and disclosing the rest"
        );
        tree.watch_piecewise(root.to_path_buf(), Some(error))?;
        Ok(tree)
    }

    /// Directories the subscription could not cover (absolute, with the
    /// error). Empty on every healthy tree and on macOS.
    pub(crate) fn unwatchable(&self) -> &[(PathBuf, String)] {
        &self.unwatchable
    }

    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> Result<(), notify::Error> {
        #[cfg(test)]
        if self.emulate_inotify
            && mode == RecursiveMode::Recursive
            && let Some(unreadable) = first_unlistable_dir(path)
        {
            return Err(notify::Error::io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))
            .add_path(unreadable));
        }
        self.inner.watch(path, mode)
    }

    /// Watch `start`'s subtree: recursively where possible, non-recursively
    /// on the way down to anything that cannot be, skipping what cannot be
    /// watched at all. `first` is the recursive watch of `start` that already
    /// failed, if the caller has one (no second walk of the whole tree).
    ///
    /// Fails for the root itself and for any error that names no directory
    /// below the one being watched (the inotify watch limit, say): those are
    /// not an unreadable subdirectory, so they are never skipped over. A
    /// directory that vanished mid-walk is neither watched nor recorded.
    fn watch_piecewise(
        &mut self,
        start: PathBuf,
        mut first: Option<notify::Error>,
    ) -> Result<(), notify::Error> {
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            let attempt = match first.take() {
                Some(error) => Err(error),
                None => self.watch(&dir, RecursiveMode::Recursive),
            };
            let error = match attempt {
                Ok(()) => {
                    self.recursive.insert(dir);
                    continue;
                }
                Err(error) => error,
            };
            let is_root = dir == self.root;
            if !is_root && vanished(&error, &dir) {
                let _ = self.inner.unwatch(&dir);
                continue;
            }
            if !is_root && is_about_itself(&error, &dir) {
                self.unwatchable.push((dir, error_detail(&error)));
                continue;
            }
            // inotify leaves what it reached before the error registered as
            // recursive; drop it so the watches below own this subtree. Also
            // done before propagating, so a failure leaves nothing half-set.
            let _ = self.inner.unwatch(&dir);
            if !failed_below(&error, &dir) {
                return Err(error);
            }
            if let Err(error) = self.watch(&dir, RecursiveMode::NonRecursive) {
                if is_root || !(is_about_itself(&error, &dir) || vanished(&error, &dir)) {
                    return Err(error);
                }
                if !vanished(&error, &dir) {
                    self.unwatchable.push((dir, error_detail(&error)));
                }
                continue;
            }
            match child_dirs(&dir) {
                Ok(children) => stack.extend(children.into_iter().rev()),
                Err(error) if is_root => return Err(notify::Error::io(error).add_path(dir)),
                Err(error) => {
                    let _ = self.inner.unwatch(&dir);
                    if error.kind() != std::io::ErrorKind::NotFound {
                        self.unwatchable.push((dir, error.to_string()));
                    }
                    continue;
                }
            }
            self.shallow.insert(dir);
        }
        self.unwatchable.sort();
        self.unwatchable.dedup_by(|a, b| a.0 == b.0);
        Ok(())
    }

    /// After the fallback, a directory under a NON-recursively watched one is
    /// watched by nothing but this: its parent's watch does not add it. For
    /// EVERY directory path in `batch` whose parent is such a one — created,
    /// re-created (deleted and made again, or renamed away and back, within
    /// one debounce window: notify drops the old watch, and the bookkeeping
    /// here cannot tell), or an unwatchable one whose permissions changed —
    /// the bookkeeping is dropped and the subtree watched again (adding a
    /// watch that exists is harmless), and the files already inside are
    /// returned for the caller to process with the batch (they may predate
    /// the watch). A directory that is gone is forgotten.
    /// A no-op when the recursive watch succeeded at startup.
    pub(crate) fn adopt_new_dirs(&mut self, batch: &[PathBuf]) -> Vec<PathBuf> {
        if self.shallow.is_empty() {
            return Vec::new();
        }
        let mut paths: Vec<&PathBuf> = batch.iter().collect();
        paths.sort();
        paths.dedup();
        let mut found = Vec::new();
        for path in paths {
            if !path
                .parent()
                .is_some_and(|parent| self.shallow.contains(parent))
            {
                continue;
            }
            let is_dir = std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());
            if !is_dir {
                if std::fs::symlink_metadata(path).is_err() {
                    self.forget(path);
                }
                continue;
            }
            let was_unwatchable = self.unwatchable.iter().any(|(dir, _)| dir == path);
            self.forget(path);
            if let Err(error) = self.watch_piecewise(path.clone(), None) {
                self.unwatchable.push((path.clone(), error_detail(&error)));
                self.unwatchable.sort();
            }
            for (dir, error) in self
                .unwatchable
                .iter()
                .filter(|(dir, _)| dir.starts_with(path))
            {
                tracing::warn!(
                    dir = %dir.display(),
                    %error,
                    "directory cannot be watched; edits beneath it are not seen live"
                );
            }
            if was_unwatchable && !self.unwatchable.iter().any(|(dir, _)| dir == path) {
                tracing::info!(dir = %path.display(), "directory is watchable again");
            }
            found.extend(files_under(path));
        }
        found
    }

    /// Drop the bookkeeping for a removed directory and everything under it.
    fn forget(&mut self, path: &Path) {
        self.recursive.retain(|dir| !dir.starts_with(path));
        self.shallow.retain(|dir| !dir.starts_with(path));
        self.unwatchable.retain(|(dir, _)| !dir.starts_with(path));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    type FailFn = Box<dyn FnMut(&Path, RecursiveMode) -> Option<notify::Error>>;

    /// Records every watch/unwatch call; `fail` can refuse a watch.
    #[derive(Default)]
    struct Recorder {
        calls: Vec<(PathBuf, RecursiveMode)>,
        unwatched: Vec<PathBuf>,
        fail: Option<FailFn>,
    }

    impl Watcher for Recorder {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Ok(Self::default())
        }
        fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            self.calls.push((path.to_path_buf(), mode));
            match self.fail.as_mut().and_then(|fail| fail(path, mode)) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.unwatched.push(path.to_path_buf());
            Ok(())
        }
        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::NullWatcher
        }
    }

    fn running_as_root() -> bool {
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    /// The piecewise plan: the root and the chain down to the unreadable
    /// directory are shallow, every clean sibling subtree is ONE recursive
    /// watch, and the unreadable directory is skipped and reported.
    #[test]
    fn piecewise_watch_skips_only_the_unreadable_directory() {
        if running_as_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["src/deep", "a/locked", "a/ok", "b"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let locked = root.join("a/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let tree = TreeWatch::start_emulating_inotify(Recorder::default(), &root);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tree = tree.unwrap();
        let unwatchable: Vec<_> = tree.unwatchable().iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(unwatchable, vec![locked]);
        let mut shallow: Vec<_> = tree.shallow.iter().cloned().collect();
        shallow.sort();
        assert_eq!(shallow, vec![root.clone(), root.join("a")]);
        let mut recursive: Vec<_> = tree.recursive.iter().cloned().collect();
        recursive.sort();
        assert_eq!(
            recursive,
            vec![root.join("a/ok"), root.join("b"), root.join("src")]
        );
    }

    /// Counterweight: a healthy tree keeps the single recursive root watch.
    #[test]
    fn a_readable_tree_is_one_recursive_watch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        let tree = TreeWatch::start_emulating_inotify(Recorder::default(), &root).unwrap();
        assert_eq!(
            tree.inner.calls,
            vec![(root.clone(), RecursiveMode::Recursive)]
        );
        assert!(tree.unwatchable().is_empty());
    }

    /// Counterweight: an unreadable ROOT still fails startup.
    #[test]
    fn an_unreadable_root_still_fails() {
        if running_as_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        let started = TreeWatch::start_emulating_inotify(Recorder::default(), &root);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(started.is_err(), "an unreadable root must fail as before");
    }

    /// A directory created under a shallow one is adopted with its files.
    #[test]
    fn a_new_directory_under_a_shallow_one_is_adopted() {
        if running_as_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("locked")).unwrap();
        let locked = root.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let tree = TreeWatch::start_emulating_inotify(Recorder::default(), &root);
        let mut tree = match tree {
            Ok(tree) => tree,
            Err(error) => {
                std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
                panic!("{error}");
            }
        };
        std::fs::create_dir_all(root.join("fresh/inner")).unwrap();
        std::fs::write(root.join("fresh/inner/x.md"), "x").unwrap();
        let found = tree.adopt_new_dirs(&[root.join("fresh"), root.join("fresh/inner/x.md")]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(found, vec![root.join("fresh/inner/x.md")]);
        assert!(tree.recursive.contains(&root.join("fresh")));
        // Re-adopting is deliberate (a directory re-created in one batch
        // looks the same): it watches again and re-reports the files.
        assert_eq!(
            tree.adopt_new_dirs(&[root.join("fresh")]),
            vec![root.join("fresh/inner/x.md")]
        );
    }

    /// Start the fallback over `root` (whose `locked` child is unreadable),
    /// restoring permissions whatever happens.
    fn fallback_tree(root: &Path, recorder: Recorder) -> TreeWatch<Recorder> {
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let tree = TreeWatch::start_emulating_inotify(recorder, root);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tree = tree.unwrap();
        assert!(tree.shallow.contains(root), "precondition: fallback ran");
        tree
    }

    /// Review of 8adb6a5c: a directory deleted and re-created (or renamed
    /// away and back) within one debounce batch lost its watch — notify drops
    /// it on the delete, the non-recursive parent does not add the new one,
    /// and the bookkeeping still said "watched". Every directory event under
    /// a non-recursive parent re-watches.
    #[test]
    fn a_recreated_directory_is_watched_again() {
        if running_as_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut tree = fallback_tree(&root, Recorder::default());
        let src = root.join("src");
        std::fs::remove_dir_all(&src).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("x.rs"), "x").unwrap();
        tree.inner.calls.clear();
        let found = tree.adopt_new_dirs(std::slice::from_ref(&src));
        assert_eq!(
            tree.inner.calls,
            vec![(src.clone(), RecursiveMode::Recursive)]
        );
        assert_eq!(found, vec![src.join("x.rs")]);
    }

    /// Review of 8adb6a5c: a directory skipped as unwatchable is retried when
    /// an event names it (its permissions changed) and dropped from the list.
    #[test]
    fn an_unwatchable_directory_is_retried_once_readable() {
        if running_as_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut tree = fallback_tree(&root, Recorder::default());
        let locked = root.join("locked");
        assert_eq!(tree.unwatchable().len(), 1);
        tree.adopt_new_dirs(std::slice::from_ref(&locked));
        assert!(tree.unwatchable().is_empty(), "{:?}", tree.unwatchable());
        assert!(tree.recursive.contains(&locked));
    }

    /// Review of 8adb6a5c: a non-path error (the inotify watch limit) while
    /// adopting a directory must not leave its partial registration behind,
    /// and is not an "unwatchable subdirectory" to skip past.
    #[test]
    fn a_watch_limit_while_adopting_unwatches_the_partial_registration() {
        if running_as_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut tree = fallback_tree(&root, Recorder::default());
        let fresh = root.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        let limited = fresh.clone();
        tree.inner.fail = Some(Box::new(move |path, _| {
            (path == limited).then(|| notify::Error::new(notify::ErrorKind::MaxFilesWatch))
        }));
        tree.adopt_new_dirs(std::slice::from_ref(&fresh));
        assert!(
            tree.inner.unwatched.contains(&fresh),
            "{:?}",
            tree.inner.unwatched
        );
        assert!(!tree.recursive.contains(&fresh));
    }

    /// Counterweight: the watch limit on the ROOT fails startup; it is not
    /// an unreadable subdirectory.
    #[test]
    fn a_watch_limit_on_the_root_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let recorder = Recorder {
            fail: Some(Box::new(|_, _| {
                Some(notify::Error::new(notify::ErrorKind::MaxFilesWatch))
            })),
            ..Recorder::default()
        };
        assert!(TreeWatch::start(recorder, &root).is_err());
    }

    /// Review of 8adb6a5c: a directory removed mid-walk (inotify reports an
    /// UNNAMED `PathNotFound`) is neither fatal nor "unwatchable".
    #[test]
    fn a_directory_that_vanishes_mid_walk_is_skipped() {
        if running_as_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let gone = root.join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let vanishing = gone.clone();
        let recorder = Recorder {
            fail: Some(Box::new(move |path, mode| {
                if path == vanishing && mode == RecursiveMode::Recursive {
                    let _ = std::fs::remove_dir_all(path);
                    return Some(notify::Error::path_not_found());
                }
                None
            })),
            ..Recorder::default()
        };
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let tree = TreeWatch::start_emulating_inotify(recorder, &root);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tree = tree.expect("a vanished directory must not fail startup");
        let unwatchable: Vec<_> = tree.unwatchable().iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(unwatchable, vec![locked]);
    }

    /// Review of 8adb6a5c: the root's recursive walk that already failed is
    /// not run a second time by the fallback.
    #[test]
    fn the_failed_root_walk_is_not_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let (fail_root, fail_locked) = (root.clone(), locked.clone());
        let recorder = Recorder {
            fail: Some(Box::new(move |path, mode| {
                let denied = || {
                    notify::Error::io(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                        .add_path(fail_locked.clone())
                };
                ((path == fail_root && mode == RecursiveMode::Recursive) || path == fail_locked)
                    .then(denied)
            })),
            ..Recorder::default()
        };
        let tree = TreeWatch::start(recorder, &root).unwrap();
        let root_walks = tree
            .inner
            .calls
            .iter()
            .filter(|call| **call == (root.clone(), RecursiveMode::Recursive))
            .count();
        assert_eq!(root_walks, 1, "{:?}", tree.inner.calls);
        assert_eq!(tree.unwatchable().len(), 1);
    }
}
