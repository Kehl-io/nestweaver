//! Canonical cross-process authority for database mutations.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// POSIX record locks are process-scoped and closing *any* descriptor for the
/// same file releases every record lock that process holds on it. Reject a
/// same-process duplicate before it can open (and later close) another
/// database descriptor, or that failed attempt could silently discard the
/// incumbent lease's compatibility lock against pre-upgrade writers.
static PROCESS_DB_LEASES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

#[derive(Debug)]
struct ProcessDbLeaseClaim {
    db_path: PathBuf,
}

impl ProcessDbLeaseClaim {
    fn acquire(db_path: &Path) -> Result<Self, WriteLeaseError> {
        let mut claimed = PROCESS_DB_LEASES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !claimed.insert(db_path.to_path_buf()) {
            return Err(WriteLeaseError::Held);
        }
        Ok(Self {
            db_path: db_path.to_path_buf(),
        })
    }

    fn is_held(db_path: &Path) -> bool {
        PROCESS_DB_LEASES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(db_path)
    }
}

impl Drop for ProcessDbLeaseClaim {
    fn drop(&mut self) {
        PROCESS_DB_LEASES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.db_path);
    }
}

/// Canonicalize a database path even before the file exists.
pub fn canonical_db_path(db_path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(db_path) {
        return canonical;
    }
    if let (Some(parent), Some(file_name)) = (db_path.parent(), db_path.file_name())
        && let Ok(canonical_parent) = std::fs::canonicalize(parent)
    {
        return canonical_parent.join(file_name);
    }
    if db_path.is_relative()
        && let Ok(cwd) = std::env::current_dir()
    {
        return lexical_normalize(&cwd.join(db_path));
    }
    db_path.to_path_buf()
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Dedicated per-database lease path.
///
/// The authority also locks the database inode itself. Holding both locks is
/// intentional: the stable sidecar survives publication/restore replacement
/// of the database, while the database lock prevents unlinking and recreating
/// only the sidecar from manufacturing a second authority.
pub fn write_lease_path(db_path: &Path) -> PathBuf {
    let canonical = canonical_db_path(db_path);
    let mut name = canonical.as_os_str().to_owned();
    name.push(".write.lock");
    PathBuf::from(name)
}

/// Held writer authority for one canonical database.
///
/// Acquire this before opening a read-write [`crate::GraphStore`] and keep it
/// alive until after the store is dropped. POSIX record locks are process-
/// scoped: dropping the database descriptor while a store remains open could
/// otherwise shorten the store's own lock lifetime.
#[must_use = "the lease must be held for the complete mutation"]
#[derive(Debug)]
pub struct DbWriteLease {
    _namespace_file: Option<std::fs::File>,
    _db_file: std::fs::File,
    _lease_file: std::fs::File,
    db_path: PathBuf,
    lease_path: PathBuf,
    // Declared last so every OS lock descriptor closes before another thread
    // in this process can claim the path.
    _process_claim: ProcessDbLeaseClaim,
}

/// Exclusive authority over creation/removal of databases below one stable
/// directory namespace. Destructive directory replacement (restore) takes
/// this before enumeration; ordinary database writers take a shared lock on
/// the same ancestor as part of [`acquire_db_write_lease`].
#[must_use = "the namespace lease must be held across enumeration and cutover"]
#[derive(Debug)]
pub struct DbNamespaceLease {
    _file: std::fs::File,
    root: PathBuf,
}

impl DbNamespaceLease {
    pub fn authorizes(&self, db_path: &Path) -> bool {
        namespace_root_for_db(db_path).is_some_and(|root| root == self.root)
    }
}

impl DbWriteLease {
    pub fn path(&self) -> &Path {
        &self.lease_path
    }

    /// Prove this authority belongs to the exact canonical database.
    pub fn authorizes(&self, db_path: &Path) -> bool {
        self.db_path == canonical_db_path(db_path)
    }
}

#[derive(Debug)]
pub enum WriteLeaseError {
    Held,
    Unavailable(std::io::Error),
}

/// Acquire the canonical exclusive writer authority without blocking.
pub fn acquire_db_write_lease(db_path: &Path) -> Result<DbWriteLease, WriteLeaseError> {
    acquire_db_write_lease_inner(db_path, None)
}

/// Acquire one database authority while an exclusive namespace authority is
/// already held. Used by restore so it can close the enumerate/create race
/// without deadlocking against its own ordinary shared namespace lock.
pub fn acquire_db_write_lease_under_namespace(
    db_path: &Path,
    namespace: &DbNamespaceLease,
) -> Result<DbWriteLease, WriteLeaseError> {
    if !namespace.authorizes(db_path) {
        return Err(WriteLeaseError::Unavailable(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "namespace authority does not cover this database",
        )));
    }
    acquire_db_write_lease_inner(db_path, Some(namespace))
}

/// Exclusively close the database-creation namespace for a destructive
/// replacement of `data_dir`. The locked inode is the stable parent of the
/// directory being replaced, so renaming `data_dir` cannot swap the lock out
/// from under the operation.
pub fn acquire_db_namespace_lease(data_dir: &Path) -> Result<DbNamespaceLease, WriteLeaseError> {
    let canonical = canonical_db_path(data_dir);
    let root = canonical.parent().ok_or_else(|| {
        WriteLeaseError::Unavailable(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "database directory has no stable parent namespace",
        ))
    })?;
    let file = std::fs::File::open(root).map_err(WriteLeaseError::Unavailable)?;
    lock_flock_nonblocking(&file, libc::LOCK_EX)?;
    Ok(DbNamespaceLease {
        _file: file,
        root: root.to_path_buf(),
    })
}

fn acquire_db_write_lease_inner(
    db_path: &Path,
    namespace: Option<&DbNamespaceLease>,
) -> Result<DbWriteLease, WriteLeaseError> {
    use std::os::unix::io::AsRawFd;

    let db_path = canonical_db_path(db_path);
    // This must precede every database open. See PROCESS_DB_LEASES: even a
    // descriptor that never called F_SETLK would release the incumbent
    // process's record lock when the failed acquisition closed it.
    let process_claim = ProcessDbLeaseClaim::acquire(&db_path)?;
    let lease_path = write_lease_path(&db_path);
    if let Some(parent) = db_path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(WriteLeaseError::Unavailable)?;
    }
    let namespace_file = if namespace.is_some() {
        None
    } else {
        let root = namespace_root_for_db(&db_path).ok_or_else(|| {
            WriteLeaseError::Unavailable(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "database path has no stable namespace ancestor",
            ))
        })?;
        let file = std::fs::File::open(root).map_err(WriteLeaseError::Unavailable)?;
        lock_flock_nonblocking(&file, libc::LOCK_SH)?;
        Some(file)
    };
    // The database descriptor is part of the authority, not merely a probe.
    // Open it first so every cooperating contender has the same lock order.
    // `create(true)` preserves the pre-open use case: callers intentionally
    // claim writer authority before GraphStore creates a new database.
    let db_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&db_path)
        .map_err(WriteLeaseError::Unavailable)?;
    // Take the same whole-file POSIX record-lock class used by lbug itself.
    // This is the compatibility bridge for a live pre-upgrade writer that
    // knows nothing about the sidecar: successful authority acquisition must
    // exclude it, not merely exclude other upgraded NestWeaver processes.
    lock_posix_write_nonblocking(&db_file)?;
    // POSIX locks are process-scoped, so a second descriptor in this process
    // would not conflict with the first. The additional flock is descriptor-
    // scoped and closes that same-process duplicate-authority hole.
    lock_exclusive_nonblocking(&db_file)?;

    let lease_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lease_path)
        .map_err(WriteLeaseError::Unavailable)?;
    if unsafe { libc::flock(lease_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return match error.kind() {
            std::io::ErrorKind::WouldBlock => Err(WriteLeaseError::Held),
            _ => Err(WriteLeaseError::Unavailable(error)),
        };
    }
    Ok(DbWriteLease {
        _namespace_file: namespace_file,
        _db_file: db_file,
        _lease_file: lease_file,
        db_path,
        lease_path,
        _process_claim: process_claim,
    })
}

fn namespace_root_for_db(db_path: &Path) -> Option<PathBuf> {
    let canonical = canonical_db_path(db_path);
    canonical.parent()?.parent().map(Path::to_path_buf)
}

fn lock_flock_nonblocking(
    file: &std::fs::File,
    operation: libc::c_int,
) -> Result<(), WriteLeaseError> {
    use std::os::unix::io::AsRawFd;

    if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.kind() {
        std::io::ErrorKind::WouldBlock => Err(WriteLeaseError::Held),
        _ => Err(WriteLeaseError::Unavailable(error)),
    }
}

fn lock_exclusive_nonblocking(file: &std::fs::File) -> Result<(), WriteLeaseError> {
    lock_flock_nonblocking(file, libc::LOCK_EX)
}

fn lock_posix_write_nonblocking(file: &std::fs::File) -> Result<(), WriteLeaseError> {
    use std::os::unix::io::AsRawFd;

    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = 0;
    lock.l_len = 0;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EACCES || code == libc::EAGAIN)
    {
        Err(WriteLeaseError::Held)
    } else {
        Err(WriteLeaseError::Unavailable(error))
    }
}

/// Non-mutating best-effort view of canonical writer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteLeaseState {
    Free,
    Held,
    Unknown,
}

pub fn write_lease_state(db_path: &Path) -> WriteLeaseState {
    let db_path = canonical_db_path(db_path);
    // Never probe the database inode while this process owns its POSIX lock:
    // closing the probe descriptor would release that lock even though the
    // probe did not acquire it. The process-local claim is exact authority for
    // this case and survives sidecar unlink/replacement.
    if ProcessDbLeaseClaim::is_held(&db_path) {
        return WriteLeaseState::Held;
    }
    // Probe the sidecar first. An upgraded writer always holds it, and an
    // early Held return avoids opening/closing the database descriptor. That
    // matters because closing any descriptor drops this process's POSIX record
    // locks on the file.
    let sidecar_state = probe_flock_state(&write_lease_path(&db_path));
    if sidecar_state == WriteLeaseState::Held {
        return WriteLeaseState::Held;
    }
    let states = [
        sidecar_state,
        probe_flock_state(&db_path),
        probe_posix_write_lock_state(&db_path),
    ];
    if states.contains(&WriteLeaseState::Held) {
        WriteLeaseState::Held
    } else if states.contains(&WriteLeaseState::Unknown) {
        WriteLeaseState::Unknown
    } else {
        WriteLeaseState::Free
    }
}

fn probe_flock_state(path: &Path) -> WriteLeaseState {
    use std::os::unix::io::AsRawFd;

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return WriteLeaseState::Free;
        }
        Err(_) => return WriteLeaseState::Unknown,
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        WriteLeaseState::Free
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        WriteLeaseState::Held
    } else {
        WriteLeaseState::Unknown
    }
}

fn probe_posix_write_lock_state(path: &Path) -> WriteLeaseState {
    use std::os::unix::io::AsRawFd;

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return WriteLeaseState::Free;
        }
        Err(_) => return WriteLeaseState::Unknown,
    };
    let mut probe: libc::flock = unsafe { std::mem::zeroed() };
    probe.l_type = libc::F_WRLCK as libc::c_short;
    probe.l_whence = libc::SEEK_SET as libc::c_short;
    probe.l_start = 0;
    probe.l_len = 0;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut probe) } != 0 {
        return WriteLeaseState::Unknown;
    }
    if probe.l_type == libc::F_UNLCK as libc::c_short {
        WriteLeaseState::Free
    } else {
        WriteLeaseState::Held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_authority_rejects_a_sibling_and_state_tracks_its_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let sibling = dir.path().join("sibling.lbug");
        assert_eq!(write_lease_state(&db), WriteLeaseState::Free);
        let authority = acquire_db_write_lease(&db).unwrap();
        assert!(authority.authorizes(&db));
        assert!(!authority.authorizes(&sibling));
        assert_eq!(write_lease_state(&db), WriteLeaseState::Held);
        drop(authority);
        assert_eq!(write_lease_state(&db), WriteLeaseState::Free);
    }

    #[cfg(unix)]
    #[test]
    fn canonical_aliases_share_one_authority() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let canonical_db = real.join("brain.lbug");
        let alias_db = alias.join("brain.lbug");
        let authority = acquire_db_write_lease(&alias_db).unwrap();
        assert!(authority.authorizes(&canonical_db));
        assert_eq!(write_lease_path(&alias_db), write_lease_path(&canonical_db));
    }

    #[test]
    fn unlinking_and_recreating_the_sidecar_cannot_mint_a_second_authority() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let authority = acquire_db_write_lease(&db).unwrap();
        std::fs::remove_file(write_lease_path(&db)).unwrap();

        assert_eq!(write_lease_state(&db), WriteLeaseState::Held);
        assert!(matches!(
            acquire_db_write_lease(&db),
            Err(WriteLeaseError::Held)
        ));

        drop(authority);
        let replacement = acquire_db_write_lease(&db).unwrap();
        assert!(replacement.authorizes(&db));
    }

    #[cfg(unix)]
    #[test]
    fn a_rejected_same_process_duplicate_preserves_the_posix_compatibility_lock() {
        use std::os::unix::io::AsRawFd as _;

        const CHILD_ENV: &str = "NW_TEST_PROBE_POSIX_DB_LOCK";
        if let Some(db) = std::env::var_os(CHILD_ENV) {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(db)
                .unwrap();
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as libc::c_short;
            lock.l_whence = libc::SEEK_SET as libc::c_short;
            lock.l_start = 0;
            lock.l_len = 0;
            let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
            if result == 0 {
                println!("legacy-posix-lock-free");
            } else {
                let error = std::io::Error::last_os_error();
                assert!(
                    error
                        .raw_os_error()
                        .is_some_and(|code| code == libc::EACCES || code == libc::EAGAIN),
                    "unexpected POSIX lock error: {error}"
                );
                println!("legacy-posix-lock-held");
            }
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let authority = acquire_db_write_lease(&db).unwrap();
        assert!(matches!(
            acquire_db_write_lease(&db),
            Err(WriteLeaseError::Held)
        ));

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "write_lease::tests::a_rejected_same_process_duplicate_preserves_the_posix_compatibility_lock",
                "--nocapture",
            ])
            .env(CHILD_ENV, &db)
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("legacy-posix-lock-held"),
            "a failed duplicate acquisition released the incumbent POSIX lock: {stdout}"
        );
        drop(authority);
    }

    #[cfg(unix)]
    #[test]
    fn a_pre_upgrade_posix_database_writer_blocks_new_authority() {
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::io::AsRawFd as _;

        const CHILD_ENV: &str = "NW_TEST_LEGACY_DB_LOCK";
        if let Some(db) = std::env::var_os(CHILD_ENV) {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(db)
                .unwrap();
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as libc::c_short;
            lock.l_whence = libc::SEEK_SET as libc::c_short;
            lock.l_start = 0;
            lock.l_len = 0;
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) },
                0
            );
            println!("legacy-lock-held");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(30));
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"legacy database").unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(executable)
            .args([
                "--exact",
                "write_lease::tests::a_pre_upgrade_posix_database_writer_blocks_new_authority",
                "--nocapture",
            ])
            .env(CHILD_ENV, &db)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut transcript = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                panic!("legacy-lock child exited before readiness: {transcript}");
            }
            transcript.push_str(&line);
            if line.contains("legacy-lock-held") {
                break;
            }
        }

        assert!(matches!(
            acquire_db_write_lease(&db),
            Err(WriteLeaseError::Held)
        ));
        let _ = child.kill();
        let _ = child.wait();
    }
}
