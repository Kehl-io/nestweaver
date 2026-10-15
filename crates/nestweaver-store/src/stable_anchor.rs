//! Coordination anchors outside replaceable database/publication trees.
//!
//! Entries name the scope they guard and are pruned by `daemon gc` only under
//! their own exclusive lock (see [`prune_authority_registry_in`]). This is
//! cooperating-process exclusion, not protection from arbitrary same-UID writes:
//! the owner must not modify the registry (including its ancestors). See
//! docs/architecture/filesystem-authority.md for the explicit trust boundary.

use std::fs::File;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct StableAnchor {
    file: File,
    path: PathBuf,
}

/// Match an open descriptor to a non-symlink pathname without opening another
/// descriptor (closing one could release a process-wide POSIX database lock).
pub fn descriptor_matches_path(file: &File, path: &Path) -> bool {
    let Ok(opened) = file.metadata() else {
        return false;
    };
    let Ok(named) = std::fs::symlink_metadata(path) else {
        return false;
    };
    !named.file_type().is_symlink() && opened.dev() == named.dev() && opened.ino() == named.ino()
}

/// Resolve the effective user's persistent state home from the account
/// database, not launch-environment overrides that could split authorities.
fn registry_root(uid: libc::uid_t) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = vec![0_u8; 16 * 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && bytes.len() < 1024 * 1024 {
            bytes.resize(bytes.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(std::io::Error::from_raw_os_error(code));
        }
        if result.is_null() {
            return Err(std::io::Error::other(
                "effective UID has no account home for authority registry",
            ));
        }
        let entry = unsafe { entry.assume_init() };
        if entry.pw_dir.is_null() {
            return Err(std::io::Error::other("effective UID has no account home"));
        }
        let home = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) }.to_bytes();
        let home = Path::new(std::ffi::OsStr::from_bytes(home));
        if !home.is_absolute() {
            return Err(std::io::Error::other(
                "effective UID account home is not absolute",
            ));
        }
        return Ok(home.join(".local/state/nestweaver/authority"));
    }
}

/// The registry this process's anchors live in.
pub fn authority_registry_root() -> std::io::Result<PathBuf> {
    registry_root(unsafe { libc::geteuid() })
}

/// Every domain an anchor is taken in. A new `StableAnchor::acquire` domain
/// must be listed here so `daemon gc` protects it for live databases.
pub const ANCHOR_DOMAINS: [&str; 3] = [
    "database-namespace",
    "publication-root",
    "publication-operation-journal",
];

/// The registry entry name for one anchor: BLAKE3 of `domain NUL path`.
pub fn anchor_key(domain: &str, canonical_path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut hash = blake3::Hasher::new();
    hash.update(domain.as_bytes());
    hash.update(&[0]);
    hash.update(canonical_path.as_os_str().as_bytes());
    hash.finalize().to_hex().to_string()
}

const RECORD_MAGIC: &[u8] = b"nestweaver-authority v1\n";

/// A record names the scope it guards so `daemon gc` can tell when that scope
/// is gone. Records written before this format are empty and unidentifiable.
fn record_bytes(domain: &str, canonical_path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = RECORD_MAGIC.to_vec();
    bytes.extend_from_slice(domain.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(canonical_path.as_os_str().as_bytes());
    bytes
}

fn parse_record(bytes: &[u8]) -> Option<(String, PathBuf)> {
    use std::os::unix::ffi::OsStrExt;
    let body = bytes.strip_prefix(RECORD_MAGIC)?;
    let split = body.iter().position(|byte| *byte == 0)?;
    let domain = std::str::from_utf8(&body[..split]).ok()?.to_string();
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(&body[split + 1..]));
    path.is_absolute().then_some((domain, path))
}

pub struct AuthorityPruneOptions<'a> {
    /// Entry names that belong to a known live database. They are kept even
    /// when the entry itself cannot say what it guards.
    pub protected: &'a std::collections::HashSet<String>,
    /// Minimum age before an unidentifiable (pre-v1, empty) entry may go.
    pub legacy_min_age: std::time::Duration,
    pub dry_run: bool,
}

/// Outcome of [`prune_authority_registry_in`]. In a dry run the `removed_*`
/// counts are what would have been removed.
#[derive(Debug, Default)]
pub struct AuthorityPruneReport {
    /// Self-describing entries whose scope no longer exists.
    pub removed_orphaned: usize,
    /// Unidentifiable entries: old, unheld, and matching no live database.
    pub removed_legacy: usize,
    /// Self-describing entries whose scope still exists.
    pub kept_live: usize,
    /// Entries some process holds a lock on.
    pub kept_held: usize,
    /// Unidentifiable entries matching a known live database.
    pub kept_protected: usize,
    /// Unidentifiable entries younger than the legacy age floor.
    pub kept_recent_legacy: usize,
    /// Names, types, owners or modes this registry never creates.
    pub skipped_foreign: usize,
    pub errors: usize,
}

fn is_entry_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_private_owned(metadata: &std::fs::Metadata, uid: libc::uid_t) -> bool {
    metadata.uid() == uid && metadata.mode() & 0o077 == 0
}

/// Remove registry entries whose scope is gone.
///
/// An entry is unlinked only while this pass holds its exclusive lock and the
/// pathname still names the locked inode, so a held anchor (shared or
/// exclusive) is never displaced. An acquirer that opened the entry just
/// before the unlink observes the replacement and retries onto a fresh entry.
pub fn prune_authority_registry_in(
    root: &Path,
    options: &AuthorityPruneOptions<'_>,
) -> std::io::Result<AuthorityPruneReport> {
    use std::io::Read;
    let mut report = AuthorityPruneReport::default();
    let uid = unsafe { libc::geteuid() };
    let root_meta = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };
    if !root_meta.is_dir() || !is_private_owned(&root_meta, uid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe NestWeaver authority registry: expected private owner directory",
        ));
    }
    let now = std::time::SystemTime::now();
    for entry in std::fs::read_dir(root)? {
        let Ok(entry) = entry else {
            report.errors += 1;
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| is_entry_name(name)) else {
            report.skipped_foreign += 1;
            continue;
        };
        let path = root.join(name);
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                report.skipped_foreign += 1;
                continue;
            }
        };
        let Ok(metadata) = file.metadata() else {
            report.errors += 1;
            continue;
        };
        if !metadata.is_file() || !is_private_owned(&metadata, uid) {
            report.skipped_foreign += 1;
            continue;
        }
        let mut bytes = Vec::new();
        if (&mut file).take(64 * 1024).read_to_end(&mut bytes).is_err() {
            report.errors += 1;
            continue;
        }
        let scope = parse_record(&bytes)
            .filter(|(domain, scope)| anchor_key(domain, scope) == name)
            .map(|(_, scope)| scope);
        let scope_exists = |scope: &Path| !matches!(std::fs::symlink_metadata(scope), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        match &scope {
            Some(scope) if scope_exists(scope) => {
                report.kept_live += 1;
                continue;
            }
            Some(_) => {}
            None if options.protected.contains(name) => {
                report.kept_protected += 1;
                continue;
            }
            None => {
                let modified = metadata.modified().unwrap_or(now);
                if now.duration_since(modified).unwrap_or_default() < options.legacy_min_age {
                    report.kept_recent_legacy += 1;
                    continue;
                }
            }
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                report.kept_held += 1;
                continue;
            }
            Err(_) => {
                report.errors += 1;
                continue;
            }
        }
        // Re-check under the lock: the entry may have been replaced, or its
        // scope recreated, since it was classified.
        if !descriptor_matches_path(&file, &path) {
            continue;
        }
        if scope.as_deref().is_some_and(scope_exists) {
            report.kept_live += 1;
            continue;
        }
        if !options.dry_run {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    report.errors += 1;
                    continue;
                }
            }
        }
        if scope.is_some() {
            report.removed_orphaned += 1;
        } else {
            report.removed_legacy += 1;
        }
    }
    Ok(report)
}

impl StableAnchor {
    pub fn acquire(domain: &str, canonical_path: &Path, exclusive: bool) -> std::io::Result<Self> {
        // A persistent account-bound path, never /tmp or environment overrides:
        // clients and ordinary temporary-file cleaners cannot split authorities.
        let root = authority_registry_root()?;
        Self::acquire_in(&root, domain, canonical_path, exclusive)
    }

    fn acquire_in(
        root: &Path,
        domain: &str,
        canonical_path: &Path,
        exclusive: bool,
    ) -> std::io::Result<Self> {
        let uid = unsafe { libc::geteuid() };
        let root = root.to_path_buf();
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
        {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(&root)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe NestWeaver authority registry: expected private owner directory",
            ));
        }
        let path = root.join(anchor_key(domain, canonical_path));
        // `daemon gc` may unlink an unheld entry between our open and lock;
        // the replaced inode is then detected and the open retried.
        for _ in 0..8 {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "unsafe NestWeaver authority registry entry",
                ));
            }
            for attempt in 0..=100 {
                let result = if exclusive {
                    file.try_lock()
                } else {
                    file.try_lock_shared()
                };
                match result {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) if attempt < 100 => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            let anchor = Self {
                file,
                path: path.clone(),
            };
            if !anchor.is_current() {
                continue;
            }
            if metadata.len() == 0 {
                // Best effort: an unwritten entry is merely unidentifiable.
                use std::os::unix::fs::FileExt;
                let _ = anchor
                    .file
                    .write_all_at(&record_bytes(domain, canonical_path), 0);
            }
            return Ok(anchor);
        }
        Err(std::io::Error::other(
            "NestWeaver authority registry entry was replaced",
        ))
    }

    // Duplicate the existing open description so derived authorities retain
    // both exclusion and the ability to detect registry substitution.
    pub(crate) fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            path: self.path.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_path(&self) -> &Path {
        &self.path
    }

    pub fn is_current(&self) -> bool {
        descriptor_matches_path(&self.file, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn registry_entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        root
    }

    fn age(path: &Path, seconds: u64) {
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(then)
            .unwrap();
    }

    fn prune(root: &Path, protected: &HashSet<String>, dry_run: bool) -> AuthorityPruneReport {
        prune_authority_registry_in(
            root,
            &AuthorityPruneOptions {
                protected,
                legacy_min_age: std::time::Duration::from_secs(3600),
                dry_run,
            },
        )
        .unwrap()
    }

    #[test]
    fn repeated_acquisition_of_one_scope_reuses_one_record() {
        let root = private_root();
        let scope = tempfile::tempdir().unwrap();
        for _ in 0..20 {
            drop(StableAnchor::acquire_in(root.path(), "test", scope.path(), true).unwrap());
        }
        assert_eq!(registry_entries(root.path()).len(), 1);
    }

    #[test]
    fn a_record_names_its_domain_and_scope() {
        let root = private_root();
        let scope = tempfile::tempdir().unwrap();
        let anchor = StableAnchor::acquire_in(root.path(), "test", scope.path(), true).unwrap();
        let bytes = std::fs::read(&anchor.path).unwrap();
        let (domain, path) = parse_record(&bytes).expect("self-describing record");
        assert_eq!(domain, "test");
        assert_eq!(path, scope.path());
        assert_eq!(
            anchor.path.file_name().unwrap().to_str().unwrap(),
            anchor_key("test", scope.path())
        );
    }

    /// Scratch, test, CI and QA databases each mint records; once their
    /// scope is gone, gc must return the registry to its baseline instead of
    /// letting it grow without bound.
    #[test]
    fn gc_reclaims_every_record_of_scopes_that_no_longer_exist() {
        let root = private_root();
        let live = tempfile::tempdir().unwrap();
        drop(
            StableAnchor::acquire_in(root.path(), "database-namespace", live.path(), false)
                .unwrap(),
        );
        for _ in 0..25 {
            let scratch = tempfile::tempdir().unwrap();
            drop(
                StableAnchor::acquire_in(root.path(), "database-namespace", scratch.path(), false)
                    .unwrap(),
            );
            drop(
                StableAnchor::acquire_in(root.path(), "publication-root", scratch.path(), true)
                    .unwrap(),
            );
        }
        assert_eq!(registry_entries(root.path()).len(), 51);

        let dry = prune(root.path(), &HashSet::new(), true);
        assert_eq!(dry.removed_orphaned, 50);
        assert_eq!(
            registry_entries(root.path()).len(),
            51,
            "dry run removes nothing"
        );

        let report = prune(root.path(), &HashSet::new(), false);
        assert_eq!(report.removed_orphaned, 50);
        assert_eq!(report.kept_live, 1);
        assert_eq!(
            registry_entries(root.path()),
            vec![anchor_key("database-namespace", live.path())]
        );
    }

    #[test]
    fn gc_never_removes_a_held_record_even_when_its_scope_is_gone() {
        let root = private_root();
        let scratch = tempfile::tempdir().unwrap();
        let gone = scratch.path().to_path_buf();
        let exclusive = StableAnchor::acquire_in(root.path(), "a", &gone, true).unwrap();
        let shared = StableAnchor::acquire_in(root.path(), "b", &gone, false).unwrap();
        drop(scratch);
        let report = prune(root.path(), &HashSet::new(), false);
        assert_eq!(report.kept_held, 2);
        assert_eq!(report.removed_orphaned, 0);
        assert!(exclusive.is_current() && shared.is_current());
    }

    /// Records written before they named their scope cannot be traced to a
    /// database. Only old, unheld ones that match no known live database go.
    #[test]
    fn gc_prunes_unidentifiable_legacy_records_only_when_old_unheld_and_unprotected() {
        let root = private_root();
        let write_legacy = |name: &str, seconds: u64| {
            let path = root.path().join(name);
            std::fs::write(&path, b"").unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .unwrap();
            age(&path, seconds);
            path
        };
        let live_db_key = anchor_key("database-namespace", Path::new("/live/brain"));
        write_legacy(&live_db_key, 30 * 86_400);
        let old = anchor_key("database-namespace", Path::new("/old/scratch"));
        write_legacy(&old, 30 * 86_400);
        let young = anchor_key("database-namespace", Path::new("/young/scratch"));
        write_legacy(&young, 60);
        let held = anchor_key("database-namespace", Path::new("/held/scratch"));
        let held_file = std::fs::File::open(write_legacy(&held, 30 * 86_400)).unwrap();
        held_file.try_lock_shared().unwrap();
        // Foreign names are never touched.
        std::fs::write(root.path().join("README"), b"").unwrap();

        let protected = HashSet::from([live_db_key.clone()]);
        let report = prune(root.path(), &protected, false);
        assert_eq!(report.removed_legacy, 1);
        assert_eq!(report.kept_protected, 1);
        assert_eq!(report.kept_recent_legacy, 1);
        assert_eq!(report.kept_held, 1);
        assert_eq!(report.skipped_foreign, 1);
        let mut expected = vec![live_db_key, young, held, "README".to_string()];
        expected.sort();
        assert_eq!(registry_entries(root.path()), expected);
    }

    /// GC unlinks a record only while holding its lock; an acquirer that
    /// opened the record just before the unlink must retry onto the fresh
    /// record rather than failing or holding the unlinked inode.
    #[test]
    fn acquisition_racing_gc_always_ends_on_the_current_record() {
        let root = private_root();
        let scratch = tempfile::tempdir().unwrap();
        let gone = scratch.path().to_path_buf();
        drop(scratch);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pruner = {
            let root = root.path().to_path_buf();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = prune(&root, &HashSet::new(), false);
                }
            })
        };
        for _ in 0..300 {
            let anchor = StableAnchor::acquire_in(root.path(), "race", &gone, true)
                .expect("acquisition must survive a concurrent gc");
            assert!(anchor.is_current());
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        pruner.join().unwrap();
    }

    #[test]
    fn registry_substitution_revokes_incumbent_authorization() {
        let scope = tempfile::tempdir().unwrap();
        let anchor = StableAnchor::acquire("test", scope.path(), true).unwrap();
        std::fs::rename(&anchor.path, anchor.path.with_extension("displaced")).unwrap();
        std::fs::write(&anchor.path, b"").unwrap();
        assert!(!anchor.is_current());
        // Replacing trusted registry state is outside the exclusion guarantee;
        // an incumbent can nevertheless detect it and must refuse mutation.
        std::fs::remove_file(&anchor.path).unwrap();
        std::fs::rename(anchor.path.with_extension("displaced"), &anchor.path).unwrap();
        assert!(anchor.is_current());
    }
}
