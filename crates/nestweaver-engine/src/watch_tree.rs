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
        match tree.watch(root, RecursiveMode::Recursive) {
            Ok(()) => return Ok(tree),
            Err(error) if is_about_subpath(&error, root) => {
                tracing::warn!(
                    root = %root.display(),
                    %error,
                    "recursive watch failed below the root; watching every readable \
                     directory individually and disclosing the rest"
                );
            }
            Err(error) => return Err(error),
        }
        // inotify left the directories it reached before the error registered
        // as recursive; drop them so the piecewise watches below own the tree.
        let _ = tree.inner.unwatch(root);
        tree.watch_piecewise(root.to_path_buf())?;
        tree.unwatchable.sort();
        tree.unwatchable.dedup_by(|a, b| a.0 == b.0);
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
    /// watched at all. Fails only for the root itself or a non-path error.
    fn watch_piecewise(&mut self, start: PathBuf) -> Result<(), notify::Error> {
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            let error = match self.watch(&dir, RecursiveMode::Recursive) {
                Ok(()) => {
                    self.recursive.insert(dir);
                    continue;
                }
                Err(error) => error,
            };
            if is_about_itself(&error, &dir) && dir != self.root {
                self.unwatchable.push((dir, error_detail(&error)));
                continue;
            }
            if !is_about_subpath(&error, &dir) && dir != self.root {
                return Err(error);
            }
            let _ = self.inner.unwatch(&dir);
            if let Err(error) = self.watch(&dir, RecursiveMode::NonRecursive) {
                if dir == self.root || !is_about_itself(&error, &dir) {
                    return Err(error);
                }
                self.unwatchable.push((dir, error_detail(&error)));
                continue;
            }
            match child_dirs(&dir) {
                Ok(children) => stack.extend(children.into_iter().rev()),
                Err(error) => {
                    if dir == self.root {
                        return Err(notify::Error::io(error).add_path(dir));
                    }
                    self.unwatchable.push((dir.clone(), error.to_string()));
                }
            }
            self.shallow.insert(dir);
        }
        Ok(())
    }

    /// After the fallback, a directory created under a NON-recursively
    /// watched one is not watched by anything. Watch every such directory in
    /// `batch` and return the files already inside it, for the caller to
    /// process with the batch (they may predate the watch). Directories that
    /// vanished are forgotten, so a re-created one is adopted again.
    /// A no-op when the recursive watch succeeded at startup.
    pub(crate) fn adopt_new_dirs(&mut self, batch: &[PathBuf]) -> Vec<PathBuf> {
        if self.shallow.is_empty() {
            return Vec::new();
        }
        let mut found = Vec::new();
        for path in batch {
            if !path
                .parent()
                .is_some_and(|parent| self.shallow.contains(parent))
            {
                continue;
            }
            let is_dir = std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());
            if !is_dir {
                if !path.exists() {
                    self.forget(path);
                }
                continue;
            }
            if self.recursive.contains(path)
                || self.shallow.contains(path)
                || self.unwatchable.iter().any(|(dir, _)| dir == path)
            {
                continue;
            }
            let before = self.unwatchable.len();
            if let Err(error) = self.watch_piecewise(path.clone()) {
                self.unwatchable.push((path.clone(), error_detail(&error)));
            }
            for (dir, error) in &self.unwatchable[before..] {
                tracing::warn!(
                    dir = %dir.display(),
                    %error,
                    "new directory cannot be watched; edits beneath it are not seen live"
                );
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

    /// Records every watch call and fails recursive ones through the seam.
    #[derive(Default)]
    struct Recorder {
        calls: Vec<(PathBuf, RecursiveMode)>,
    }

    impl Watcher for Recorder {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Ok(Self::default())
        }
        fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            self.calls.push((path.to_path_buf(), mode));
            Ok(())
        }
        fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
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
        assert!(
            tree.adopt_new_dirs(&[root.join("fresh")]).is_empty(),
            "an adopted directory is not adopted twice"
        );
    }
}
