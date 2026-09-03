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

/// Whether this process is acquiring or already holds the canonical writer
/// claim for `db_path`.
///
/// This predicate is intentionally armed before any database descriptor is
/// opened and remains armed until every OS authority and diagnostic latch has
/// dropped. Callers that probe POSIX lock state must consult it first: opening
/// and closing another descriptor while it is true can release this process's
/// compatibility lock.
pub fn current_process_claims_write_lease(db_path: &Path) -> bool {
    ProcessDbLeaseClaim::is_held(&canonical_db_path(db_path))
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
    let absolute = if db_path.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(db_path))
            .unwrap_or_else(|_| db_path.to_path_buf())
    } else {
        db_path.to_path_buf()
    };
    // Resolve the deepest ancestor that exists, then retain the unresolved
    // suffix. Canonicalizing only the immediate parent loses aliases whenever
    // more than one component is absent (for example while restore has renamed
    // a data directory), allowing two spellings of one future database to
    // bypass the pre-open process claim.
    for ancestor in absolute.ancestors() {
        if let Ok(mut canonical) = std::fs::canonicalize(ancestor) {
            let suffix = absolute
                .strip_prefix(ancestor)
                .expect("an ancestor must prefix its path");
            canonical.push(suffix);
            return lexical_normalize(&canonical);
        }
    }
    lexical_normalize(&absolute)
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
    // System scratch roots such as /tmp cannot be destructively restored by
    // NestWeaver, so a database directly below one needs only the exact
    // database and sidecar authorities. Every replaceable data directory also
    // holds this shared namespace descriptor.
    _namespace_file: Option<std::fs::File>,
    _db_file: std::fs::File,
    _lease_file: std::fs::File,
    /// Records that this process owns the sidecar flock. This is diagnostic,
    /// not an additional authority: it prevents the corruption classifier's
    /// second descriptor from mistaking our own lease for an external writer.
    ///
    /// Field order is load-bearing. Rust drops fields in declaration order, so
    /// all OS lock descriptors above are closed before this latch is cleared.
    /// A probe in that narrow window therefore sees "locks free, latch set"
    /// rather than "locks held, latch clear", which fails toward reporting a
    /// genuine corruption instead of suppressing it.
    _self_latch: crate::SelfHeldWriteLease,
    db_path: PathBuf,
    lease_path: PathBuf,
    /// True only when this acquisition atomically created the database inode.
    /// This is creation provenance for staged-publication constructors; an
    /// arbitrary pre-existing zero-byte file is not equivalent.
    created_db_file: bool,
    // Declared last so every OS lock descriptor closes before another thread
    // in this process can claim the path.
    _process_claim: ProcessDbLeaseClaim,
}

/// Exclusive authority over creation/removal of databases below one stable
/// data-directory namespace. Destructive directory replacement (restore)
/// takes this before enumeration; ordinary database writers take a shared
/// lock on the same stable per-directory file as part of
/// [`acquire_db_write_lease`].
#[must_use = "the namespace lease must be held across enumeration and cutover"]
#[derive(Debug)]
pub struct DbNamespaceLease {
    _file: std::fs::File,
    data_dir: PathBuf,
}

impl DbNamespaceLease {
    pub fn authorizes(&self, db_path: &Path) -> bool {
        data_dir_for_db(db_path).is_some_and(|data_dir| data_dir == self.data_dir)
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

    /// Whether this exact authority atomically created `db_path` while taking
    /// the canonical lease.
    pub fn authorizes_fresh_creation(&self, db_path: &Path) -> bool {
        self.created_db_file && self.authorizes(db_path)
    }

    /// Re-establish the legacy POSIX writer exclusion after another database
    /// descriptor in this process closed. POSIX record locks are process-wide,
    /// so a failed engine open can release `_db_file`'s lock before recovery
    /// inspects or moves crash artifacts.
    pub(crate) fn rearm_legacy_writer_exclusion(&self) -> Result<(), WriteLeaseError> {
        lock_posix_write_nonblocking(&self._db_file)
    }
}

#[derive(Debug)]
pub enum WriteLeaseError {
    Held,
    Unavailable(std::io::Error),
}

/// Acquire the canonical exclusive writer authority without queueing behind a
/// live external canonical owner. Acquisition may briefly retry an inherited
/// `flock` left in the fork-before-exec window of another thread.
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
/// replacement of `data_dir`. The locked file lives in the stable parent and
/// is keyed by the directory being replaced, so renaming `data_dir` cannot
/// swap the lock out from under the operation and unrelated sibling data
/// directories do not block one another.
pub fn acquire_db_namespace_lease(data_dir: &Path) -> Result<DbNamespaceLease, WriteLeaseError> {
    let data_dir = canonical_db_path(data_dir);
    let lease_path = namespace_lease_path(&data_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lease_path)
        .map_err(WriteLeaseError::Unavailable)?;
    // Namespace coordination is an upgraded-writer protocol, so one
    // descriptor-scoped flock is sufficient. Do not layer a POSIX record lock
    // onto this same inode: macOS makes flock/fcntl locks cooperate and
    // explicitly permits only one of those interfaces per file in a process.
    lock_flock_with_inheritance_retry(&file, libc::LOCK_EX)?;
    Ok(DbNamespaceLease {
        _file: file,
        data_dir,
    })
}

fn namespace_lease_path(data_dir: &Path) -> Result<PathBuf, WriteLeaseError> {
    if is_system_scratch_data_dir(data_dir) {
        return Err(WriteLeaseError::Unavailable(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "system scratch directories cannot be destructively replaced as database namespaces",
        )));
    }
    let parent = data_dir.parent().ok_or_else(|| {
        WriteLeaseError::Unavailable(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "database directory has no stable parent namespace",
        ))
    })?;
    let name = data_dir.file_name().ok_or_else(|| {
        WriteLeaseError::Unavailable(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "database directory has no stable namespace name",
        ))
    })?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(name);
    lock_name.push(".nestweaver-write-namespace.lock");
    Ok(parent.join(lock_name))
}

/// Direct scratch databases are valid, but replacing the system scratch root
/// itself is not. Keeping that distinction explicit prevents `/tmp/x.lbug`
/// from trying to create its shared namespace lock in `/`, while every exact
/// database writer still holds the database inode and stable sidecar locks.
///
/// The paths are fixed rather than derived from `TMPDIR`: an untrusted ambient
/// environment variable must not be able to opt an ordinary data directory out
/// of restore coordination. Canonicalisation covers macOS aliases such as
/// `/tmp -> /private/tmp`.
fn is_system_scratch_data_dir(data_dir: &Path) -> bool {
    let data_dir = canonical_db_path(data_dir);
    [Path::new("/tmp"), Path::new("/var/tmp")]
        .into_iter()
        .map(canonical_db_path)
        .any(|scratch| scratch == data_dir)
}

/// Whether the stable parent refused a new entry because the directory itself
/// is not writable by this process, rather than because of a transient or
/// caller-correctable fault. This is deliberately narrow: a full disk, a
/// missing intermediate path, or an I/O error must still fail the acquisition.
fn stable_parent_is_immutable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
    )
}

fn acquire_db_write_lease_inner(
    db_path: &Path,
    namespace: Option<&DbNamespaceLease>,
) -> Result<DbWriteLease, WriteLeaseError> {
    let db_path = canonical_db_path(db_path);
    // This must precede every database open. See PROCESS_DB_LEASES: even a
    // descriptor that never called F_SETLK would release the incumbent
    // process's record lock when the failed acquisition closed it.
    let process_claim = ProcessDbLeaseClaim::acquire(&db_path)?;
    let lease_path = write_lease_path(&db_path);
    let namespace_file = if namespace.is_some() {
        None
    } else {
        let data_dir = data_dir_for_db(&db_path).ok_or_else(|| {
            WriteLeaseError::Unavailable(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "database path has no stable namespace ancestor",
            ))
        })?;
        if is_system_scratch_data_dir(&data_dir) {
            None
        } else {
            if let Some(stable_parent) = data_dir.parent()
                && !stable_parent.exists()
                && let Err(error) = std::fs::create_dir_all(stable_parent)
                && !stable_parent_is_immutable(&error)
            {
                return Err(WriteLeaseError::Unavailable(error));
            }
            let lease_path = namespace_lease_path(&data_dir)?;
            match std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(lease_path)
            {
                Ok(file) => {
                    lock_flock_with_inheritance_retry(&file, libc::LOCK_SH)?;
                    Some(file)
                }
                // The shared namespace lock lives in the stable parent so it
                // survives restore renaming `data_dir`. When that parent
                // refuses new files, NestWeaver cannot destructively restore
                // this directory either -- `acquire_db_namespace_lease` would
                // fail on the identical create -- so there is no cutover for
                // an ordinary writer to coordinate with. Proceed on the exact
                // database and sidecar authorities alone, exactly as a
                // database directly under a system scratch root already does.
                // Only an immutable parent is tolerated; every other error
                // still fails the acquisition.
                Err(error) if stable_parent_is_immutable(&error) => None,
                Err(error) => return Err(WriteLeaseError::Unavailable(error)),
            }
        }
    };
    // The data directory may have been absent, or restore may have renamed it
    // away immediately before this acquisition. Recreate it only after the
    // shared namespace authority is held so a losing writer cannot leave an
    // empty destination that obstructs restore cutover.
    if let Some(parent) = db_path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(WriteLeaseError::Unavailable)?;
    }
    // The database descriptor is part of the authority, not merely a probe.
    // Open it first so every cooperating contender has the same lock order.
    // Atomic create-new preserves provenance for staged publication creation;
    // falling back only on AlreadyExists prevents an arbitrary empty file from
    // masquerading as one this authority created.
    let (db_file, created_db_file) = match std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&db_path)
    {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .truncate(false)
                .open(&db_path)
                .map_err(WriteLeaseError::Unavailable)?;
            (file, false)
        }
        Err(error) => return Err(WriteLeaseError::Unavailable(error)),
    };
    // Every failure from here on must unwind the database inode this call
    // created. Publishing an empty `.lbug` would make the caller's next
    // `open_or_create` see a corrupt-looking database rather than an absent
    // one -- the nw-126 shape, where a zero-length artifact made a live
    // database look unopenable. A database we did NOT create is never removed.
    let remaining = (|| {
        // Take the same whole-file POSIX record-lock class used by lbug itself.
        // This is the compatibility bridge for a live pre-upgrade writer that
        // knows nothing about the sidecar: successful authority acquisition
        // must exclude it, not merely exclude other upgraded NestWeaver
        // processes.
        lock_posix_write_nonblocking(&db_file)?;
        // Never flock this same inode as well. Linux keeps the two lock
        // families independent, while macOS makes them cooperate and a second
        // interface can contend with this process's own record lock. The
        // process claim and the distinct sidecar flock below close the
        // same-process duplicate hole.

        let lease_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lease_path)
            .map_err(WriteLeaseError::Unavailable)?;
        lock_flock_with_inheritance_retry(&lease_file, libc::LOCK_EX)?;
        Ok(lease_file)
    })();
    let lease_file = match remaining {
        Ok(lease_file) => lease_file,
        Err(error) => {
            if created_db_file {
                // Close our descriptor first so the unlink cannot race this
                // process's own record lock on the inode being removed.
                drop(db_file);
                let _ = std::fs::remove_file(&db_path);
            }
            return Err(error);
        }
    };
    // Arm only after every OS authority is held, and keep it in the lease so
    // ownership knowledge cannot become stale or disappear early.
    let self_latch = crate::note_self_held_write_lease(&db_path);
    Ok(DbWriteLease {
        _namespace_file: namespace_file,
        _db_file: db_file,
        _lease_file: lease_file,
        _self_latch: self_latch,
        db_path,
        lease_path,
        created_db_file,
        _process_claim: process_claim,
    })
}

fn data_dir_for_db(db_path: &Path) -> Option<PathBuf> {
    let canonical = canonical_db_path(db_path);
    canonical.parent().map(Path::to_path_buf)
}

fn lock_flock_with_inheritance_retry(
    file: &std::fs::File,
    operation: libc::c_int,
) -> Result<(), WriteLeaseError> {
    use std::os::unix::io::AsRawFd;

    // `flock` follows the open file description across fork. Rust marks these
    // descriptors CLOEXEC, but a child created by another thread can retain a
    // just-dropped owner's description until exec completes. Bounded retry
    // covers that transient inheritance window. A live upgraded owner keeps
    // the flock past the bound and is reported as Held; an exact database
    // writer is also excluded first by the non-inherited POSIX lock on the
    // separate database inode.
    for attempt in 0..=100 {
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(WriteLeaseError::Unavailable(error));
        }
        if attempt == 100 {
            return Err(WriteLeaseError::Held);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    unreachable!("bounded flock retry always returns")
}

fn lock_posix_nonblocking(
    file: &std::fs::File,
    lock_type: libc::c_short,
) -> Result<(), WriteLeaseError> {
    use std::os::unix::io::AsRawFd;

    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = lock_type;
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

fn lock_posix_write_nonblocking(file: &std::fs::File) -> Result<(), WriteLeaseError> {
    lock_posix_nonblocking(file, libc::F_WRLCK as libc::c_short)
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
    // The database inode belongs exclusively to the POSIX compatibility
    // protocol. Probing it with flock as well is harmless on Linux, where the
    // lock families are independent, but on macOS that flock can conflict with
    // a POSIX lock held by this very process and falsely report an external
    // writer. Upgraded writers are already visible through the stable sidecar.
    let states = [sidecar_state, probe_posix_write_lock_state(&db_path)];
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

    fn await_write_lease_free(db: &Path) {
        for _ in 0..100 {
            let namespace_free = data_dir_for_db(db).is_some_and(|data_dir| {
                namespace_lease_path(&data_dir)
                    .map(|path| probe_flock_state(&path) == WriteLeaseState::Free)
                    .unwrap_or(true)
            });
            if write_lease_state(db) == WriteLeaseState::Free && namespace_free {
                return;
            }
            // Another parallel test may be between fork and exec. The forked
            // child briefly inherits the open-file description; CLOEXEC drops
            // it at exec, but an immediate probe can observe that real,
            // transient ownership after the parent lease is dropped.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(write_lease_state(db), WriteLeaseState::Free);
        let data_dir = data_dir_for_db(db).expect("database path has a data directory");
        if let Ok(namespace_path) = namespace_lease_path(&data_dir) {
            assert_eq!(probe_flock_state(&namespace_path), WriteLeaseState::Free);
        }
    }

    #[test]
    fn a_database_directly_in_system_scratch_does_not_require_a_root_lock() {
        let scratch_root = Path::new("/tmp");
        if !scratch_root.is_dir() {
            return;
        }
        assert!(is_system_scratch_data_dir(scratch_root));
        assert!(matches!(
            namespace_lease_path(scratch_root),
            Err(WriteLeaseError::Unavailable(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));

        let scratch_db = tempfile::Builder::new()
            .prefix("nestweaver-write-lease-")
            .suffix(".lbug")
            .tempfile_in(scratch_root)
            .unwrap();
        let db_path = scratch_db.path().to_path_buf();
        let sidecar = write_lease_path(&db_path);
        let authority = acquire_db_write_lease(&db_path)
            .expect("an exact scratch database lease must not need write access to /");
        assert!(authority.authorizes(&db_path));
        assert_eq!(write_lease_state(&db_path), WriteLeaseState::Held);

        drop(authority);
        await_write_lease_free(&db_path);
        std::fs::remove_file(sidecar).unwrap();
    }

    /// A data directory whose stable parent is not writable cannot host the
    /// shared namespace lock -- and, by the same token, can never be
    /// destructively restored by NestWeaver, because restore would fail to
    /// create the identical file. An ordinary writer must therefore proceed on
    /// the exact database and sidecar authorities alone, exactly as it already
    /// does for a database directly under a system scratch root. Failing the
    /// whole write would make `--db` unusable under any root-owned prefix.
    #[test]
    fn an_unwritable_stable_parent_does_not_block_an_ordinary_writer() {
        if unsafe { libc::geteuid() } == 0 {
            // root ignores the mode bits, so the precondition cannot be built.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let stable_parent = dir.path().join("root-owned");
        let data_dir = stable_parent.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let db = data_dir.join("brain.lbug");
        std::fs::set_permissions(
            &stable_parent,
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();

        let authority = acquire_db_write_lease(&db)
            .expect("an unwritable stable parent must not block the exact database authority");
        assert!(authority.authorizes(&db));
        assert_eq!(write_lease_state(&db), WriteLeaseState::Held);

        // The counterweight that keeps this from being a silent downgrade:
        // a DESTRUCTIVE namespace acquisition on the same directory must still
        // fail loudly rather than proceed uncoordinated.
        assert!(
            acquire_db_namespace_lease(&data_dir).is_err(),
            "restore must never proceed without the namespace authority"
        );

        drop(authority);
        std::fs::set_permissions(
            &stable_parent,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
    }

    /// A failed acquisition must not leave the database inode it created.
    /// `acquire_db_write_lease_inner` opens the database with `create_new`
    /// BEFORE it can take the sidecar authority, so every failure after that
    /// point would otherwise publish an empty file where the caller's next
    /// `open_or_create` expects either a real database or nothing at all.
    #[test]
    fn a_failed_acquisition_does_not_leave_the_database_inode_it_created() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        // A directory at the sidecar path makes the lease-file open fail with
        // a non-WouldBlock error, which is the generic "cannot take the
        // sidecar authority" arm (read-only mount, ENOSPC, permissions).
        std::fs::create_dir(write_lease_path(&db)).unwrap();

        let error = acquire_db_write_lease(&db)
            .expect_err("an unopenable sidecar must fail the whole acquisition");
        assert!(
            matches!(error, WriteLeaseError::Unavailable(_)),
            "{error:?}"
        );
        assert!(
            !db.exists(),
            "a failed acquisition must not publish the database inode it created"
        );
    }

    /// The counterweight: a database the acquisition did NOT create must
    /// survive the same failure untouched.
    #[test]
    fn a_failed_acquisition_preserves_a_database_it_did_not_create() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"pre-existing bytes").unwrap();
        std::fs::create_dir(write_lease_path(&db)).unwrap();

        acquire_db_write_lease(&db).expect_err("an unopenable sidecar must still fail");
        assert_eq!(
            std::fs::read(&db).unwrap(),
            b"pre-existing bytes",
            "a pre-existing database must never be removed by a failed acquisition"
        );
    }

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
        await_write_lease_free(&db);
    }

    #[test]
    fn canonical_authority_arms_and_clears_the_self_ownership_latch() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let wal_corruption = "Corrupted wal file. Read out invalid WAL record type.";

        assert!(!crate::self_holds_write_lease(&db));
        assert!(!current_process_claims_write_lease(&db));
        let authority = acquire_db_write_lease(&db).unwrap();
        assert!(crate::self_holds_write_lease(&db));
        assert!(current_process_claims_write_lease(&db));
        assert!(
            !crate::live_writer_holds_write_lease(&db),
            "our own canonical authority must not be reported as an external writer"
        );
        assert_eq!(
            crate::error::classify_engine_corruption_for_db(wal_corruption, &db),
            Some(crate::CorruptionKind::WalUnreadable),
            "a self-held lease must not suppress a genuine WAL-corruption verdict"
        );

        drop(authority);
        await_write_lease_free(&db);
        assert!(!crate::self_holds_write_lease(&db));
        assert!(!current_process_claims_write_lease(&db));
        assert!(!crate::live_writer_holds_write_lease(&db));
    }

    #[test]
    fn destructive_namespaces_are_isolated_per_data_directory() {
        let root = tempfile::tempdir().unwrap();
        let first_dir = root.path().join("first");
        let second_dir = root.path().join("second");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();

        let first = acquire_db_namespace_lease(&first_dir).unwrap();
        let second = acquire_db_namespace_lease(&second_dir).unwrap();
        assert!(matches!(
            acquire_db_write_lease(&first_dir.join("blocked.lbug")),
            Err(WriteLeaseError::Held)
        ));
        let second_db = second_dir.join("brain.lbug");
        let second_writer = acquire_db_write_lease_under_namespace(&second_db, &second).unwrap();

        assert!(first.authorizes(&first_dir.join("brain.lbug")));
        assert!(!first.authorizes(&second_db));
        assert!(second_writer.authorizes(&second_db));
    }

    #[test]
    fn blocked_writer_does_not_recreate_a_restore_destination() {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        let renamed = root.path().join("data-before-restore");
        std::fs::create_dir_all(&data_dir).unwrap();

        let namespace = acquire_db_namespace_lease(&data_dir).unwrap();
        std::fs::rename(&data_dir, &renamed).unwrap();
        assert!(matches!(
            acquire_db_write_lease(&data_dir.join("brain.lbug")),
            Err(WriteLeaseError::Held)
        ));

        assert!(namespace.authorizes(&data_dir.join("brain.lbug")));
        assert!(
            !data_dir.exists(),
            "a writer that loses namespace admission must not obstruct restore cutover"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_forked_child_cannot_create_false_writer_contention_after_owner_drop() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fork-inheritance.lbug");
        let authority = acquire_db_write_lease(&db).unwrap();

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork fixture failed");
        if child == 0 {
            // Keep every inherited flock description alive long enough for
            // the parent to drop and reacquire its authority.
            unsafe {
                libc::usleep(50_000);
                libc::_exit(0);
            }
        }

        drop(authority);
        let replacement = acquire_db_write_lease(&db).unwrap();
        assert!(replacement.authorizes(&db));
        drop(replacement);

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_forked_child_cannot_create_false_namespace_contention_after_owner_drop() {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();
        let namespace = acquire_db_namespace_lease(&data_dir).unwrap();

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork fixture failed");
        if child == 0 {
            unsafe {
                libc::usleep(50_000);
                libc::_exit(0);
            }
        }

        drop(namespace);
        let replacement = acquire_db_namespace_lease(&data_dir).unwrap();
        assert!(replacement.authorizes(&data_dir.join("brain.lbug")));
        drop(replacement);

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
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

    #[cfg(unix)]
    #[test]
    fn canonical_aliases_survive_multiple_missing_path_components() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let canonical_db = std::fs::canonicalize(&real)
            .unwrap()
            .join("missing")
            .join("nested")
            .join("brain.lbug");
        let alias_db = alias.join("missing").join("nested").join("brain.lbug");

        assert_eq!(canonical_db_path(&alias_db), canonical_db);
        assert_eq!(write_lease_path(&alias_db), write_lease_path(&canonical_db));
    }

    #[cfg(unix)]
    #[test]
    fn namespace_authority_follows_a_symlinked_database_target() {
        let root = tempfile::tempdir().unwrap();
        let apparent_dir = root.path().join("apparent");
        let target_dir = root.path().join("target");
        std::fs::create_dir(&apparent_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();
        let target_db = target_dir.join("brain.lbug");
        std::fs::write(&target_db, b"database").unwrap();
        let alias_db = apparent_dir.join("brain.lbug");
        std::os::unix::fs::symlink(&target_db, &alias_db).unwrap();

        let apparent = acquire_db_namespace_lease(&apparent_dir).unwrap();
        assert!(!apparent.authorizes(&alias_db));
        let target = acquire_db_namespace_lease(&target_dir).unwrap();
        assert!(target.authorizes(&alias_db));
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
        await_write_lease_free(&db);
        let canonical = canonical_db_path(&db);
        assert!(!ProcessDbLeaseClaim::is_held(&canonical));
        assert_eq!(
            probe_flock_state(&namespace_lease_path(dir.path()).unwrap()),
            WriteLeaseState::Free
        );
        assert_eq!(probe_flock_state(&db), WriteLeaseState::Free);
        assert_eq!(probe_posix_write_lock_state(&db), WriteLeaseState::Free);
        assert_eq!(
            probe_flock_state(&write_lease_path(&db)),
            WriteLeaseState::Free
        );
        let replacement = acquire_db_write_lease(&db).unwrap();
        assert!(replacement.authorizes(&db));
    }

    #[cfg(unix)]
    #[test]
    fn write_lease_state_does_not_cross_probe_a_same_process_posix_lock() {
        use std::os::unix::io::AsRawFd as _;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&db)
            .unwrap();
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) },
            0
        );

        // This synthetic unclaimed lock is not a supported writer lifecycle:
        // closing the probe descriptor also releases same-process POSIX locks.
        // It exists only to prove the state query does not add a cross-family
        // flock probe that self-contends on macOS. Production writers hold the
        // process claim, so write_lease_state returns Held before opening here.
        assert_eq!(write_lease_state(&db), WriteLeaseState::Free);
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
    fn rearming_after_a_same_inode_close_restores_the_posix_compatibility_lock() {
        use std::os::unix::io::AsRawFd as _;

        const CHILD_ENV: &str = "NW_TEST_PROBE_REARMED_POSIX_DB_LOCK";
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
                println!("rearmed-posix-lock-free");
            } else {
                let error = std::io::Error::last_os_error();
                assert!(
                    error
                        .raw_os_error()
                        .is_some_and(|code| code == libc::EACCES || code == libc::EAGAIN),
                    "unexpected POSIX lock error: {error}"
                );
                println!("rearmed-posix-lock-held");
            }
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let authority = acquire_db_write_lease(&db).unwrap();

        // POSIX closes discard all record locks this process holds for the
        // inode, even when the closed descriptor never acquired the lock.
        let same_inode = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&db)
            .unwrap();
        drop(same_inode);
        authority.rearm_legacy_writer_exclusion().unwrap();

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "write_lease::tests::rearming_after_a_same_inode_close_restores_the_posix_compatibility_lock",
                "--nocapture",
            ])
            .env(CHILD_ENV, &db)
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("rearmed-posix-lock-held"),
            "rearming did not restore legacy-writer exclusion: {stdout}"
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
