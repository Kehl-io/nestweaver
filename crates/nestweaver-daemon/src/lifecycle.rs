//! Utility functions for daemon runtime paths (socket, pidfile, logs).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const EFFECTIVE_CONFIG_BINDING_VERSION: u32 = 1;
const EFFECTIVE_CONFIG_BINDING_FILE: &str = "effective-config.json";
const EFFECTIVE_CONFIG_BINDING_MAX_BYTES: u64 = 64 * 1024;
pub const LAST_SUCCESSFUL_CONFIG_VERSION: u32 = 1;
const LAST_SUCCESSFUL_CONFIG_FILE: &str = "last-successful-config.json";
const LAST_SUCCESSFUL_CONFIG_MAX_BYTES: u64 = 64 * 1024;
static EFFECTIVE_CONFIG_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static LAST_SUCCESSFUL_CONFIG_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn effective_config_temp_path(parent: &Path, sequence: u64) -> PathBuf {
    parent.join(format!(
        ".{EFFECTIVE_CONFIG_BINDING_FILE}.{}.{}.tmp",
        std::process::id(),
        sequence
    ))
}

fn last_successful_config_temp_path(parent: &Path, sequence: u64) -> PathBuf {
    parent.join(format!(
        ".{LAST_SUCCESSFUL_CONFIG_FILE}.{}.{}.tmp",
        std::process::id(),
        sequence
    ))
}

fn last_successful_config_backup_path(parent: &Path, sequence: u64) -> PathBuf {
    parent.join(format!(
        ".{LAST_SUCCESSFUL_CONFIG_FILE}.{}.{}.bak",
        std::process::id(),
        sequence
    ))
}

/// Full, stable identity of a database path for persistent local state.
///
/// Unlike [`instance_id_from_db_path`], this is never truncated for a unix
/// socket limit. Database contents are deliberately excluded: replacing or
/// updating the database at the same canonical path keeps the same local
/// startup intent.
pub fn database_path_fingerprint(db_path: &Path) -> String {
    use sha2::{Digest, Sha256};

    let canonical = canonical_db_path(db_path);
    let canonical = if canonical.is_absolute() {
        canonical
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&canonical))
            .unwrap_or(canonical)
    };
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(canonical.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        write!(&mut fingerprint, "{byte:02x}").expect("writing to a String cannot fail");
    }
    fingerprint
}

/// Daemon-owned persistent startup intent for one canonical database path.
///
/// This is intentionally separate from [`EffectiveConfigBinding`]: the latter
/// is live PID attestation and is removed on exit, while this record survives
/// idle exit and crashes so a later automatic cold start preserves identity
/// and authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSuccessfulConfig {
    pub version: u32,
    pub database_fingerprint: String,
    pub config_path: String,
}

impl LastSuccessfulConfig {
    pub fn new(db_path: &Path, config_path: &Path) -> Result<Self, LastSuccessfulConfigError> {
        let record_path = last_successful_config_path(db_path);
        let canonical = std::fs::canonicalize(config_path).map_err(|source| {
            LastSuccessfulConfigError::Read {
                path: config_path.to_path_buf(),
                source,
            }
        })?;
        let config_path = canonical
            .to_str()
            .ok_or_else(|| LastSuccessfulConfigError::Unsafe {
                path: record_path,
                reason: format!(
                    "canonical config path is not valid UTF-8: {}",
                    canonical.display()
                ),
            })?;
        Ok(Self {
            version: LAST_SUCCESSFUL_CONFIG_VERSION,
            database_fingerprint: database_path_fingerprint(db_path),
            config_path: config_path.to_string(),
        })
    }
}

#[derive(Debug)]
pub enum LastSuccessfulConfigError {
    Absent {
        path: PathBuf,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Corrupt {
        path: PathBuf,
        source: serde_json::Error,
    },
    Unsafe {
        path: PathBuf,
        reason: String,
    },
    TooLarge {
        path: PathBuf,
        size: u64,
        max: u64,
    },
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    FingerprintMismatch {
        path: PathBuf,
        expected: String,
        found: String,
    },
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for LastSuccessfulConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent { path } => write!(
                f,
                "last-successful-config record is absent: {}",
                path.display()
            ),
            Self::Read { path, source } => write!(
                f,
                "failed to read last-successful-config record {}: {source}",
                path.display()
            ),
            Self::Corrupt { path, source } => write!(
                f,
                "last-successful-config record {} is corrupt: {source}",
                path.display()
            ),
            Self::Unsafe { path, reason } => write!(
                f,
                "last-successful-config record {} is unsafe: {reason}",
                path.display()
            ),
            Self::TooLarge { path, size, max } => write!(
                f,
                "last-successful-config record {} is too large ({size} bytes; maximum {max})",
                path.display()
            ),
            Self::UnsupportedVersion {
                path,
                found,
                supported,
            } => write!(
                f,
                "last-successful-config record {} has unsupported version {found} (supported: {supported})",
                path.display()
            ),
            Self::FingerprintMismatch {
                path,
                expected,
                found,
            } => write!(
                f,
                "last-successful-config record {} belongs to database fingerprint {found}, expected {expected}",
                path.display()
            ),
            Self::Serialize { path, source } => write!(
                f,
                "failed to serialize last-successful-config record {}: {source}",
                path.display()
            ),
            Self::Write { path, source } => write!(
                f,
                "failed to publish last-successful-config record {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LastSuccessfulConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Corrupt { source, .. } | Self::Serialize { source, .. } => Some(source),
            Self::Absent { .. }
            | Self::Unsafe { .. }
            | Self::TooLarge { .. }
            | Self::UnsupportedVersion { .. }
            | Self::FingerprintMismatch { .. } => None,
        }
    }
}

/// The configuration source a live daemon actually uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum EffectiveConfigBindingSource {
    Configured { path: String },
    CompiledDefaults,
}

/// Versioned daemon-owned live binding between an instance, PID, and its
/// effective configuration. This record is meaningful only while `pid` still
/// identifies the daemon holding the corresponding pidfile lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveConfigBinding {
    pub version: u32,
    pub pid: u32,
    pub effective_config: EffectiveConfigBindingSource,
}

impl EffectiveConfigBinding {
    pub fn new(pid: u32, effective_config: EffectiveConfigBindingSource) -> Self {
        Self {
            version: EFFECTIVE_CONFIG_BINDING_VERSION,
            pid,
            effective_config,
        }
    }
}

#[derive(Debug)]
pub enum EffectiveConfigBindingError {
    Absent {
        path: PathBuf,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Corrupt {
        path: PathBuf,
        source: serde_json::Error,
    },
    Unsafe {
        path: PathBuf,
        reason: String,
    },
    TooLarge {
        path: PathBuf,
        size: u64,
        max: u64,
    },
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    PidMismatch {
        path: PathBuf,
        expected: u32,
        found: u32,
    },
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for EffectiveConfigBindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent { path } => {
                write!(f, "effective-config binding is absent: {}", path.display())
            }
            Self::Read { path, source } => write!(
                f,
                "failed to read effective-config binding {}: {source}",
                path.display()
            ),
            Self::Corrupt { path, source } => write!(
                f,
                "effective-config binding {} is corrupt: {source}",
                path.display()
            ),
            Self::Unsafe { path, reason } => write!(
                f,
                "effective-config binding {} is unsafe: {reason}",
                path.display()
            ),
            Self::TooLarge { path, size, max } => write!(
                f,
                "effective-config binding {} is too large ({size} bytes; maximum {max})",
                path.display()
            ),
            Self::UnsupportedVersion {
                path,
                found,
                supported,
            } => write!(
                f,
                "effective-config binding {} has unsupported version {found} (supported: {supported})",
                path.display()
            ),
            Self::PidMismatch {
                path,
                expected,
                found,
            } => write!(
                f,
                "effective-config binding {} belongs to PID {found}, expected PID {expected}",
                path.display()
            ),
            Self::Serialize { path, source } => write!(
                f,
                "failed to serialize effective-config binding {}: {source}",
                path.display()
            ),
            Self::Write { path, source } => write!(
                f,
                "failed to publish effective-config binding {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for EffectiveConfigBindingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Corrupt { source, .. } | Self::Serialize { source, .. } => Some(source),
            Self::Absent { .. }
            | Self::Unsafe { .. }
            | Self::TooLarge { .. }
            | Self::UnsupportedVersion { .. }
            | Self::PidMismatch { .. } => None,
        }
    }
}

/// Canonicalize a database path even before the database file exists.
///
/// `std::fs::canonicalize(path)` only succeeds once the file exists. Daemon
/// startup often receives a not-yet-created DB path, so canonicalize the parent
/// directory and append the original filename. This keeps socket IDs stable for
/// paths such as macOS `/tmp/...` and `/private/tmp/...`.
pub fn canonical_db_path(db_path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(db_path) {
        return canonical;
    }

    if let (Some(parent), Some(file_name)) = (db_path.parent(), db_path.file_name())
        && let Ok(canonical_parent) = std::fs::canonicalize(parent)
    {
        return canonical_parent.join(file_name);
    }

    db_path.to_path_buf()
}

/// Derive a stable, short instance ID from a database path.
///
/// Returns ONLY the 8-character hex hash of the canonical path.
/// This keeps socket paths well under the macOS 104-byte `sun_path` limit.
///
/// For a human-readable label (parent-dir + hash), use
/// [`instance_label_from_db_path`] instead.
pub fn instance_id_from_db_path(db_path: &Path) -> String {
    let canonical = canonical_db_path(db_path);
    // Use SHA-256 for a stable hash that won't change across Rust versions.
    // DefaultHasher (SipHash) is explicitly documented as not portable.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    // Take the first 4 bytes (8 hex chars) for a short, stable instance ID.
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3]
    )
}

/// Human-readable label for logging: `<parent-dir>-<hash>`.
///
/// Never use this in path construction — use [`instance_id_from_db_path`]
/// (the bare 8-char hash) to keep socket paths short.
pub fn instance_label_from_db_path(db_path: &Path) -> String {
    let canonical = canonical_db_path(db_path);
    let prefix = canonical
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nw".to_string());
    let hash = instance_id_from_db_path(db_path);
    format!("{prefix}-{hash}")
}

/// Runtime directory for the daemon socket and pidfile.
///
/// Prefers `$XDG_RUNTIME_DIR/nestweaver/<instance>/` (Linux with systemd),
/// falling back to `~/.local/state/nestweaver/<instance>/` (macOS and
/// everywhere else).
///
/// **`$TMPDIR` is deliberately NOT consulted.** On macOS, different
/// launchers see different `$TMPDIR` values (per-user
/// `/var/folders/.../T/` from interactive shells vs. `/tmp/` from
/// sanitized subprocess environments like Claude Code's MCP launcher).
/// Using `$TMPDIR` caused a connection loop where two clients for the
/// same DB instance would disagree on the socket location, each trying
/// to spawn its own daemon.
pub fn runtime_dir(instance_id: &str) -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join("nestweaver").join(instance_id);
    }

    // Stable per-user directory that doesn't depend on caller env.
    // Co-located with daemon logs (which already use this path).
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local/state")
        })
        .join("nestweaver")
        .join(instance_id)
}

/// Maximum unix socket path length (sun_path) on macOS/BSD — 104 bytes
/// including the NUL terminator.
const SUN_PATH_LIMIT: usize = 104;

/// Outcome of [`secure_fallback_sock_dir`].
#[derive(Debug, PartialEq, Eq)]
enum FallbackDirStatus {
    /// We created the directory (mode 0700).
    Created,
    /// Already existed with the right owner and mode.
    Verified,
    /// Existed with the right owner but a wider mode — tightened to 0700.
    ModeTightened,
}

/// Create or verify the `/tmp/nw-sock-<uid>` fallback socket dir.
///
/// Anyone can create paths in `/tmp`, so a pre-existing dir owned by ANOTHER
/// user is a squat: binding our socket inside it would let that user swap or
/// delete the socket. We refuse (PermissionDenied) rather than use it — a
/// foreign-owned dir in sticky `/tmp` can be neither repaired nor removed by
/// us. Dirs we own are tightened to mode 0700 so no other user can reach in.
fn secure_fallback_sock_dir(dir: &Path, expected_uid: u32) -> std::io::Result<FallbackDirStatus> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        // create_dir_all honors umask — set 0700 explicitly.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        return Ok(FallbackDirStatus::Created);
    }

    let meta = std::fs::metadata(dir)?;
    if meta.uid() != expected_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "socket fallback dir {} is owned by uid {} (expected {}) — refusing to use \
                 a squatted directory; remove it as its owner or as root",
                dir.display(),
                meta.uid(),
                expected_uid
            ),
        ));
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        return Ok(FallbackDirStatus::ModeTightened);
    }
    Ok(FallbackDirStatus::Verified)
}

/// Path to the Unix domain socket for the given instance.
///
/// LOW (SUN_LEN): a state dir nested too deeply (long `$XDG_RUNTIME_DIR` or
/// home path) would push the socket path past the 104-byte `sun_path` limit
/// and every bind/connect would fail with ENAMETOOLONG. Fall back to a short
/// /tmp-based path and say so. All callers derive the socket through this
/// function, so client and daemon always agree on the fallback.
///
/// The /tmp fallback dir is created mode 0700 and ownership-checked.
/// A dir squatted by another uid is never used — the error is logged and we
/// fall back to the (over-long) runtime-dir path so the bind fails loudly
/// instead of trusting the squatted dir.
pub fn socket_path(instance_id: &str) -> PathBuf {
    let default = runtime_dir(instance_id).join("daemon.sock");
    if default.as_os_str().len() < SUN_PATH_LIMIT {
        return default;
    }
    let uid = unsafe { libc::getuid() };
    let fallback_dir = PathBuf::from("/tmp").join(format!("nw-sock-{uid}"));
    if let Err(e) = secure_fallback_sock_dir(&fallback_dir, uid) {
        tracing::error!(
            error = %e,
            dir = %fallback_dir.display(),
            "refusing squatted /tmp socket fallback dir"
        );
        return default;
    }
    let fallback = fallback_dir.join(instance_id);
    // The bind fails if the parent doesn't exist; create it eagerly since
    // `runtime_dir` (which callers create) is NOT the fallback's parent.
    let _ = std::fs::create_dir_all(&fallback);
    let fallback = fallback.join("daemon.sock");
    tracing::warn!(
        intended = %default.display(),
        fallback = %fallback.display(),
        "socket path exceeds the {SUN_PATH_LIMIT}-byte sun_path limit — using /tmp fallback"
    );
    fallback
}

/// Path to the PID file for the given instance.
pub fn pidfile_path(instance_id: &str) -> PathBuf {
    runtime_dir(instance_id).join("daemon.pid")
}

/// Path to the daemon-owned effective-config live binding for an instance.
pub fn effective_config_binding_path(instance_id: &str) -> PathBuf {
    runtime_dir(instance_id).join(EFFECTIVE_CONFIG_BINDING_FILE)
}

/// Who, if anyone, holds this database's write lock — the ONE ownership proof
/// an operator's `rm` cannot erase.
///
/// The pidfile flock is held on an *inode*, not a path: unlink the pidfile and
/// every path-based owner check is silently defeated, because the next caller
/// creates a brand-new inode whose flock is trivially free. The database write
/// lock has no such hole — it lives on the database file itself, which is the
/// data, so nobody deletes it during recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbWriteLock {
    /// No process holds a write lock (or the database does not exist yet).
    Free,
    /// A process holds the write lock — by definition this instance is NOT
    /// stale. `pid` is the kernel-reported holder when the platform names one.
    Held { pid: Option<i32> },
    /// The lock state could not be determined (unreadable file, `fcntl`
    /// failure). Callers must treat this as "possibly owned", never as free.
    Unknown,
}

impl DbWriteLock {
    /// Is a live process holding the database? `Unknown` is deliberately NOT
    /// counted: callers that destroy runtime state must use
    /// [`DbWriteLock::is_provably_free`] instead.
    pub fn is_held(self) -> bool {
        matches!(self, DbWriteLock::Held { .. })
    }

    /// Only `Free` is safe to act destructively on.
    pub fn is_provably_free(self) -> bool {
        matches!(self, DbWriteLock::Free)
    }
}

/// Records that THIS process holds a read-write store open on some database.
/// Set by the daemon after `GraphStore::open_or_create`; read by
/// [`db_write_lock`] to refuse a probe that would destroy that lock.
static LOCAL_STORE_WRITE_LOCK_HELD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Declare whether this process now holds a read-write store open.
///
/// This exists solely to arm the guard in [`db_write_lock`]. Call it with
/// `true` immediately after a read-write `GraphStore` open and `false` when
/// that store is dropped.
pub fn note_local_store_write_lock(held: bool) {
    LOCAL_STORE_WRITE_LOCK_HELD.store(held, std::sync::atomic::Ordering::SeqCst);
}

/// Does this process hold a read-write store open, as far as
/// [`note_local_store_write_lock`] has been told?
pub fn local_store_write_lock_held() -> bool {
    LOCAL_STORE_WRITE_LOCK_HELD.load(std::sync::atomic::Ordering::SeqCst)
}

/// Probe the database write lock without acquiring it.
///
/// lbug locks the whole database file with a POSIX record lock
/// (`fcntl(F_SETLK, F_WRLCK)`; see `local_file_system.cpp`), so `F_GETLK` asks
/// the kernel who owns it and reports the holder's PID.
///
/// # This probe is NOT free of side effects for the calling process
///
/// POSIX record locks are per *process*, and the kernel drops **all** of a
/// process's record locks on a file the moment that process closes **any**
/// descriptor to it. This function opens the database and closes it again, so:
///
/// * if the caller already holds the lbug write lock on this database, this
///   call **silently releases it**, admitting a second writer;
/// * and it sees `Free` regardless, because a process's own locks never
///   conflict with its own `F_GETLK`.
///
/// So this is only safe from a process that does NOT have the store open — the
/// CLI before it starts anything, or the daemon before its own store open. The
/// `debug_assert` below turns a future in-daemon call into a loud failure
/// instead of a silently unlocked database; arm it with
/// [`note_local_store_write_lock`].
///
/// (A `/proc/locks` reader would avoid the descriptor entirely on Linux, but it
/// cannot distinguish "no lock" from "inode/device match failed" — a false
/// `Free` here re-enables every bug this probe exists to prevent — and it has no
/// macOS equivalent. Not worth the trade today.)
pub fn db_write_lock(db_path: &Path) -> DbWriteLock {
    use std::os::unix::io::AsRawFd;

    debug_assert!(
        !local_store_write_lock_held(),
        "db_write_lock() must not be called while this process holds the store open: closing \
         the probe descriptor drops this process's own POSIX record locks on the database"
    );

    let path = canonical_db_path(db_path);
    if !path.exists() {
        return DbWriteLock::Free;
    }
    let Ok(file) = std::fs::File::open(&path) else {
        return DbWriteLock::Unknown;
    };
    let mut probe: libc::flock = unsafe { std::mem::zeroed() };
    probe.l_type = libc::F_WRLCK as libc::c_short;
    probe.l_whence = libc::SEEK_SET as libc::c_short;
    probe.l_start = 0;
    probe.l_len = 0;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut probe) } != 0 {
        return DbWriteLock::Unknown;
    }
    if probe.l_type == libc::F_UNLCK as libc::c_short {
        return DbWriteLock::Free;
    }
    DbWriteLock::Held {
        pid: (probe.l_pid > 0).then_some(probe.l_pid),
    }
}

fn persistent_state_root() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local/state")
        })
        .join("nestweaver")
}

/// Private state directory for one database's persistent startup intent.
pub fn last_successful_config_dir(db_path: &Path) -> PathBuf {
    persistent_state_root()
        .join("config-intent")
        .join(database_path_fingerprint(db_path))
}

/// Path to the daemon-owned persistent startup-intent record.
pub fn last_successful_config_path(db_path: &Path) -> PathBuf {
    last_successful_config_dir(db_path).join(LAST_SUCCESSFUL_CONFIG_FILE)
}

#[cfg(unix)]
fn verify_persistent_config_dir_for_owner(dir: &Path, expected_uid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "persistent config state path {} is not a real directory",
                dir.display()
            ),
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "persistent config state directory {} is owned by uid {} (expected {})",
                dir.display(),
                metadata.uid(),
                expected_uid
            ),
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "persistent config state directory {} has unsafe mode {mode:04o} (expected 0700)",
                dir.display()
            ),
        ));
    }
    Ok(())
}

fn secure_persistent_config_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        match std::fs::symlink_metadata(dir) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(dir)?;
            }
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(dir)?;
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.file_type().is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == expected_uid
            && metadata.permissions().mode() & 0o777 != 0o700
        {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        verify_persistent_config_dir_for_owner(dir, expected_uid)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

#[cfg(unix)]
fn verify_effective_config_runtime_dir_for_owner(
    dir: &Path,
    expected_uid: u32,
) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "effective-config runtime path {} is not a real directory",
                dir.display()
            ),
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "effective-config runtime directory {} is owned by uid {} (expected {})",
                dir.display(),
                metadata.uid(),
                expected_uid
            ),
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "effective-config runtime directory {} has unsafe mode {mode:04o}",
                dir.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn secure_effective_config_runtime_dir_for_owner(
    dir: &Path,
    expected_uid: u32,
) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    match std::fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
        }
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(dir)?;
    if metadata.file_type().is_dir()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == expected_uid
        && metadata.permissions().mode() & 0o777 != 0o700
    {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    verify_effective_config_runtime_dir_for_owner(dir, expected_uid)
}

fn secure_effective_config_runtime_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        secure_effective_config_runtime_dir_for_owner(dir, unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Atomically publish a live effective-config binding beside the pidfile.
///
/// The temporary file is created in the same directory, synced, and renamed
/// over the destination so readers observe either the complete old record or
/// the complete new record. On Unix the record is explicitly mode 0600.
pub fn write_effective_config_binding(
    instance_id: &str,
    binding: &EffectiveConfigBinding,
) -> Result<(), EffectiveConfigBindingError> {
    let path = effective_config_binding_path(instance_id);
    let bytes = serde_json::to_vec_pretty(binding).map_err(|source| {
        EffectiveConfigBindingError::Serialize {
            path: path.clone(),
            source,
        }
    })?;
    let parent = path
        .parent()
        .expect("effective-config binding path always has a runtime directory");
    secure_effective_config_runtime_dir(parent).map_err(|source| {
        EffectiveConfigBindingError::Write {
            path: path.clone(),
            source,
        }
    })?;

    let (temp_path, mut file) = loop {
        let sequence =
            EFFECTIVE_CONFIG_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = effective_config_temp_path(parent, sequence);
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(EffectiveConfigBindingError::Write { path, source });
            }
        }
    };
    let mut published = false;
    let write_result = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        use std::io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, &path)?;
        published = true;
        // Persist the directory entry. If this fails, publication failed even
        // though rename made the file visible, so the error path unlinks it.
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if let Err(source) = write_result {
        let _ = std::fs::remove_file(if published { &path } else { &temp_path });
        return Err(EffectiveConfigBindingError::Write { path, source });
    }
    Ok(())
}

/// Read and validate the schema version of an instance's live binding.
pub fn read_effective_config_binding(
    instance_id: &str,
) -> Result<EffectiveConfigBinding, EffectiveConfigBindingError> {
    let path = effective_config_binding_path(instance_id);
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .expect("effective-config binding path always has a runtime directory");
        if let Err(error) =
            verify_effective_config_runtime_dir_for_owner(parent, unsafe { libc::geteuid() })
        {
            if error.kind() == std::io::ErrorKind::NotFound {
                return Err(EffectiveConfigBindingError::Absent { path });
            }
            return Err(EffectiveConfigBindingError::Unsafe {
                path,
                reason: error.to_string(),
            });
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(EffectiveConfigBindingError::Absent { path });
        }
        #[cfg(unix)]
        Err(source) if source.raw_os_error() == Some(libc::ELOOP) => {
            return Err(EffectiveConfigBindingError::Unsafe {
                path,
                reason: "symbolic links are not accepted".to_string(),
            });
        }
        Err(source) => {
            return Err(EffectiveConfigBindingError::Read { path, source });
        }
    };
    let metadata = file
        .metadata()
        .map_err(|source| EffectiveConfigBindingError::Read {
            path: path.clone(),
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(EffectiveConfigBindingError::Unsafe {
            path,
            reason: "record is not a regular file".to_string(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let owner = metadata.uid();
        let expected_owner = unsafe { libc::geteuid() };
        if owner != expected_owner {
            return Err(EffectiveConfigBindingError::Unsafe {
                path,
                reason: format!("owned by uid {owner}, expected uid {expected_owner}"),
            });
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(EffectiveConfigBindingError::Unsafe {
                path,
                reason: format!("mode {mode:04o} grants group or other access"),
            });
        }
    }
    if metadata.len() > EFFECTIVE_CONFIG_BINDING_MAX_BYTES {
        return Err(EffectiveConfigBindingError::TooLarge {
            path,
            size: metadata.len(),
            max: EFFECTIVE_CONFIG_BINDING_MAX_BYTES,
        });
    }
    use std::io::Read;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(EFFECTIVE_CONFIG_BINDING_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| EffectiveConfigBindingError::Read {
            path: path.clone(),
            source,
        })?;
    if bytes.len() as u64 > EFFECTIVE_CONFIG_BINDING_MAX_BYTES {
        return Err(EffectiveConfigBindingError::TooLarge {
            path,
            size: bytes.len() as u64,
            max: EFFECTIVE_CONFIG_BINDING_MAX_BYTES,
        });
    }
    let binding: EffectiveConfigBinding =
        serde_json::from_slice(&bytes).map_err(|source| EffectiveConfigBindingError::Corrupt {
            path: path.clone(),
            source,
        })?;
    if binding.version != EFFECTIVE_CONFIG_BINDING_VERSION {
        return Err(EffectiveConfigBindingError::UnsupportedVersion {
            path,
            found: binding.version,
            supported: EFFECTIVE_CONFIG_BINDING_VERSION,
        });
    }
    Ok(binding)
}

/// Read a binding and require its integer PID to match a daemon identity the
/// caller has already verified through kernel socket/health evidence plus the
/// held pidfile lock. This helper does not itself prove process liveness or
/// ownership; PID equality alone is vulnerable to stale files and PID reuse.
pub fn read_effective_config_binding_for_verified_pid(
    instance_id: &str,
    expected_pid: u32,
) -> Result<EffectiveConfigBinding, EffectiveConfigBindingError> {
    let binding = read_effective_config_binding(instance_id)?;
    if binding.pid != expected_pid {
        return Err(EffectiveConfigBindingError::PidMismatch {
            path: effective_config_binding_path(instance_id),
            expected: expected_pid,
            found: binding.pid,
        });
    }
    Ok(binding)
}

/// Remove an instance's live binding. Absence is already the desired state.
pub fn remove_effective_config_binding(instance_id: &str) -> std::io::Result<()> {
    match std::fs::remove_file(effective_config_binding_path(instance_id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Atomically publish the configured path that most recently reached daemon
/// readiness for this database.
pub fn write_last_successful_config(
    db_path: &Path,
    config_path: &Path,
) -> Result<LastSuccessfulConfig, LastSuccessfulConfigError> {
    let path = last_successful_config_path(db_path);
    let record = LastSuccessfulConfig::new(db_path, config_path)?;
    let bytes = serde_json::to_vec_pretty(&record).map_err(|source| {
        LastSuccessfulConfigError::Serialize {
            path: path.clone(),
            source,
        }
    })?;
    let parent = path
        .parent()
        .expect("last-successful-config path always has a state directory");
    secure_persistent_config_dir(parent).map_err(|source| LastSuccessfulConfigError::Write {
        path: path.clone(),
        source,
    })?;

    let (temp_path, mut file, sequence) = loop {
        let sequence =
            LAST_SUCCESSFUL_CONFIG_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = last_successful_config_temp_path(parent, sequence);
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => break (candidate, file, sequence),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(LastSuccessfulConfigError::Write { path, source }),
        }
    };

    // Preserve the previous durable inode across any post-rename publication
    // error. A hard-link backup is local to this private directory and avoids
    // copying or parsing the old contents, so even a corrupt-but-safe record
    // can be replaced explicitly without losing the last known bytes if the
    // new directory entry cannot be synced.
    let backup_path = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                let _ = std::fs::remove_file(&temp_path);
                return Err(LastSuccessfulConfigError::Unsafe {
                    path,
                    reason: "existing record is not a regular non-symlink file".to_string(),
                });
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let expected_owner = unsafe { libc::geteuid() };
                let mode = metadata.permissions().mode() & 0o777;
                if metadata.uid() != expected_owner || mode != 0o600 {
                    let _ = std::fs::remove_file(&temp_path);
                    return Err(LastSuccessfulConfigError::Unsafe {
                        path,
                        reason: format!(
                            "existing record owner/mode is unsafe (uid {}, mode {mode:04o})",
                            metadata.uid()
                        ),
                    });
                }
            }
            let backup = last_successful_config_backup_path(parent, sequence);
            let link_result = match std::fs::hard_link(&path, &backup) {
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A process can die after restoring/publishing the
                    // authoritative destination but before removing its
                    // private backup. The destination is authoritative here;
                    // discard only this colliding internal artifact and retry.
                    std::fs::remove_file(&backup).and_then(|()| std::fs::hard_link(&path, &backup))
                }
                result => result,
            };
            if let Err(source) = link_result {
                let _ = std::fs::remove_file(&temp_path);
                return Err(LastSuccessfulConfigError::Write {
                    path: path.clone(),
                    source,
                });
            }
            Some(backup)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(LastSuccessfulConfigError::Read { path, source });
        }
    };

    let mut published = false;
    let write_result = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        use std::io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, &path)?;
        published = true;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if let Err(source) = write_result {
        if published {
            let _ = std::fs::remove_file(&path);
            if let Some(backup) = &backup_path {
                let _ = std::fs::rename(backup, &path);
                let _ = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
            }
        } else {
            let _ = std::fs::remove_file(&temp_path);
        }
        return Err(LastSuccessfulConfigError::Write { path, source });
    }
    if let Some(backup) = backup_path {
        let _ = std::fs::remove_file(backup);
        let _ = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
    }
    Ok(record)
}

/// Read and fully validate the persistent startup intent for `db_path`.
pub fn read_last_successful_config(
    db_path: &Path,
) -> Result<LastSuccessfulConfig, LastSuccessfulConfigError> {
    let path = last_successful_config_path(db_path);
    let parent = path
        .parent()
        .expect("last-successful-config path always has a state directory");
    #[cfg(unix)]
    if let Err(error) = verify_persistent_config_dir_for_owner(parent, unsafe { libc::geteuid() }) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Err(LastSuccessfulConfigError::Absent { path });
        }
        return Err(LastSuccessfulConfigError::Unsafe {
            path,
            reason: error.to_string(),
        });
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(LastSuccessfulConfigError::Absent { path });
        }
        #[cfg(unix)]
        Err(source) if source.raw_os_error() == Some(libc::ELOOP) => {
            return Err(LastSuccessfulConfigError::Unsafe {
                path,
                reason: "symbolic links are not accepted".to_string(),
            });
        }
        Err(source) => return Err(LastSuccessfulConfigError::Read { path, source }),
    };
    let metadata = file
        .metadata()
        .map_err(|source| LastSuccessfulConfigError::Read {
            path: path.clone(),
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(LastSuccessfulConfigError::Unsafe {
            path,
            reason: "record is not a regular file".to_string(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let owner = metadata.uid();
        let expected_owner = unsafe { libc::geteuid() };
        if owner != expected_owner {
            return Err(LastSuccessfulConfigError::Unsafe {
                path,
                reason: format!("owned by uid {owner}, expected uid {expected_owner}"),
            });
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(LastSuccessfulConfigError::Unsafe {
                path,
                reason: format!("mode {mode:04o}, expected 0600"),
            });
        }
    }
    if metadata.len() > LAST_SUCCESSFUL_CONFIG_MAX_BYTES {
        return Err(LastSuccessfulConfigError::TooLarge {
            path,
            size: metadata.len(),
            max: LAST_SUCCESSFUL_CONFIG_MAX_BYTES,
        });
    }
    use std::io::Read;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(LAST_SUCCESSFUL_CONFIG_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| LastSuccessfulConfigError::Read {
            path: path.clone(),
            source,
        })?;
    if bytes.len() as u64 > LAST_SUCCESSFUL_CONFIG_MAX_BYTES {
        return Err(LastSuccessfulConfigError::TooLarge {
            path,
            size: bytes.len() as u64,
            max: LAST_SUCCESSFUL_CONFIG_MAX_BYTES,
        });
    }
    let record: LastSuccessfulConfig =
        serde_json::from_slice(&bytes).map_err(|source| LastSuccessfulConfigError::Corrupt {
            path: path.clone(),
            source,
        })?;
    if record.version != LAST_SUCCESSFUL_CONFIG_VERSION {
        return Err(LastSuccessfulConfigError::UnsupportedVersion {
            path,
            found: record.version,
            supported: LAST_SUCCESSFUL_CONFIG_VERSION,
        });
    }
    let expected = database_path_fingerprint(db_path);
    if record.database_fingerprint != expected {
        return Err(LastSuccessfulConfigError::FingerprintMismatch {
            path,
            expected,
            found: record.database_fingerprint,
        });
    }
    if record.config_path.is_empty() || !Path::new(&record.config_path).is_absolute() {
        return Err(LastSuccessfulConfigError::Unsafe {
            path,
            reason: "config path must be a non-empty absolute path".to_string(),
        });
    }
    Ok(record)
}

/// Clear persistent configured intent after a manual default start has become
/// healthy and its live default provenance has been attested.
pub fn remove_last_successful_config(db_path: &Path) -> Result<(), LastSuccessfulConfigError> {
    let path = last_successful_config_path(db_path);
    let parent = path
        .parent()
        .expect("last-successful-config path always has a state directory");
    #[cfg(unix)]
    if let Err(error) = verify_persistent_config_dir_for_owner(parent, unsafe { libc::geteuid() }) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(LastSuccessfulConfigError::Unsafe {
            path,
            reason: error.to_string(),
        });
    }
    match std::fs::remove_file(&path) {
        Ok(()) => std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| LastSuccessfulConfigError::Write { path, source }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LastSuccessfulConfigError::Write { path, source }),
    }
}

/// Directory for daemon log files.
pub fn log_dir(instance_id: &str) -> PathBuf {
    persistent_state_root().join(instance_id)
}

/// Path to the daemon log file.
pub fn log_path(instance_id: &str) -> PathBuf {
    log_dir(instance_id).join("daemon.log")
}

/// Operator-facing pointer to where daemon diagnostics actually live.
///
/// Two writers put daemon output in [`log_dir`], and they do NOT overlap:
/// launchd redirects the process's stderr to the undated `daemon.log`, while
/// the tracing subscriber uses a daily rolling appender that writes structured
/// records to `daemon.log.<YYYY-MM-DD>`. Boot timing, and every
/// `tracing::error!` emitted by a failing startup, land only in the dated file.
///
/// Pointing a stuck operator at `log_path` alone therefore names the one file
/// guaranteed not to hold the tracing error they are looking for, which is the
/// dead end this hint exists to avoid.
pub fn log_hint(instance_id: &str) -> String {
    format!(
        "{} — `daemon.log` is stderr; `daemon.log.<date>` has the structured \
         boot timing and errors",
        log_dir(instance_id).display()
    )
}

/// True if `db_path` lives under a temporary directory (`/tmp`, `/private/tmp`,
/// `/var/folders`, or `$TMPDIR`). Daemons for temp DBs are ephemeral (tests,
/// throwaway repros): they must never receive a persistent launchd agent, and
/// their state directories are always reclaimable.
///
/// Lives here rather than in `launchd` because the predicate is not
/// launchd-specific and `launchd` is macOS-gated, while state directories
/// accumulate on every platform.
pub fn is_temp_db_path(db_path: &Path) -> bool {
    let mut bases: Vec<PathBuf> = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/var/folders"),
        PathBuf::from("/private/var/folders"),
    ];
    if let Some(t) = std::env::var_os("TMPDIR") {
        bases.push(PathBuf::from(t));
    }
    bases.iter().any(|b| db_path.starts_with(b))
}

/// Outcome of a [`gc_orphaned_state_dirs`] pass.
#[derive(Debug, Default)]
pub struct StateDirGcReport {
    /// Instance directories deleted (database gone or under a temp dir).
    pub removed: Vec<String>,
    /// Bytes reclaimed by those deletions.
    pub reclaimed_bytes: u64,
    /// Directories whose database still exists.
    pub kept: Vec<String>,
    /// Directories spared because a live daemon still holds the pidfile lock.
    pub spared: Vec<String>,
    /// Directories left alone because their database could not be identified.
    /// Not an error: unidentifiable means undeletable, by design.
    pub unidentified: Vec<String>,
}

/// Is a live daemon holding `instance_id`'s pidfile lock?
///
/// The daemon opens the DB *before* taking the pidfile `flock(LOCK_EX)` and
/// holds it for its whole lifetime, so a held lock means a healthy daemon.
/// Fails toward `true` (spare) on any error we cannot interpret.
#[cfg(unix)]
fn instance_daemon_is_live(instance_id: &str) -> bool {
    let pidfile = pidfile_path(instance_id);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pidfile)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
    }
    unsafe {
        libc::flock(fd, libc::LOCK_UN);
    }
    false
}

#[cfg(not(unix))]
fn instance_daemon_is_live(_instance_id: &str) -> bool {
    true
}

/// A daemon instance directory is named by the 8-hex instance id.
///
/// This is the **allowlist** the state sweep is built on. `config-intent/` is a
/// sibling under the same root holding the persisted startup intent, and its
/// own children are 64-hex database fingerprints — so neither that directory
/// nor its contents can ever match this shape. Deleting `config-intent/` is the
/// catastrophic failure here (it has happened once for real, via a hand-written
/// `find … -exec rm -rf`), and structural exclusion is what prevents it. Never
/// replace this with a denylist of known-bad names.
fn is_instance_dir_name(name: &str) -> bool {
    name.len() == 8
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() && b <= b'f')
}

/// Recover the database path a daemon instance was serving, from the first line
/// its startup wrote to `daemon.log`:
///
/// ```text
/// [daemon] starting for /path/to/brain.lbug (instance repo-1a2b3c4d)
/// ```
///
/// The instance id is a one-way hash of the database path, so this log line is
/// the only record tying an existing directory back to its database. Returns
/// `None` when the line is absent or unparseable, which makes the directory
/// unidentifiable and therefore undeletable.
fn db_path_from_daemon_log(log: &str) -> Option<PathBuf> {
    let line = log
        .lines()
        .find(|line| line.starts_with("[daemon] starting for "))?;
    let rest = line.strip_prefix("[daemon] starting for ")?;
    // A database path may itself contain " (instance ", so anchor on the LAST
    // occurrence — the suffix this line format always ends with.
    let end = rest.rfind(" (instance ")?;
    let path = &rest[..end];
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

fn directory_size_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => directory_size_bytes(&entry.path()),
            Ok(file_type) if file_type.is_file() => {
                entry.metadata().map(|meta| meta.len()).unwrap_or(0)
            }
            _ => 0,
        })
        .sum()
}

/// Reclaim daemon state directories whose database is gone or was temporary.
///
/// The state directory is keyed by a hash of `--db` and created whenever a
/// daemon auto-spawns. Because writes are daemon-routed, nearly any write-path
/// test spawns one against its temp database, and nothing removed the directory
/// when that database went away — 8,525 directories / 547 MB accumulated on one
/// machine in about a month, exactly one of them live, while `daemon gc`
/// reported "clean".
///
/// Fail-closed at every step. A directory is deleted only when its name matches
/// the instance-id shape, its database was positively identified, that database
/// is temporary or missing, and no live daemon holds its pidfile lock. Anything
/// unreadable, unparseable, or ambiguous is kept.
pub fn gc_orphaned_state_dirs() -> std::io::Result<StateDirGcReport> {
    let root = persistent_state_root();
    let mut report = StateDirGcReport::default();

    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        // No state root yet is a clean machine, not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // Allowlist first, before reading anything inside.
        if !is_instance_dir_name(name) {
            continue;
        }
        // symlink_metadata: never follow a symlink out of the state root.
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => {}
            _ => continue,
        }

        let log = std::fs::read_to_string(path.join("daemon.log")).unwrap_or_default();
        let Some(db_path) = db_path_from_daemon_log(&log) else {
            report.unidentified.push(name.to_string());
            continue;
        };

        if instance_daemon_is_live(name) {
            report.spared.push(name.to_string());
            continue;
        }

        if !(is_temp_db_path(&db_path) || !db_path.exists()) {
            report.kept.push(name.to_string());
            continue;
        }

        let size = directory_size_bytes(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                report.reclaimed_bytes += size;
                report.removed.push(name.to_string());
            }
            // A directory that vanished under us needs no reporting; anything
            // else stays visible as still present rather than silently counted.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => report.kept.push(name.to_string()),
        }
    }

    report.removed.sort();
    report.kept.sort();
    report.spared.sort();
    report.unidentified.sort();
    Ok(report)
}

/// Launchd service label for the given instance.
pub fn launchd_label(instance_id: &str) -> String {
    format!("io.kehl.nestweaver.{instance_id}")
}

/// Path to the launchd plist for the given instance.
pub fn launchd_plist_path(instance_id: &str) -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", launchd_label(instance_id)))
}

/// Compute the legacy instance ID using `DefaultHasher` (SipHash).
///
/// Before the switch to SHA-256, the instance ID was derived from
/// `DefaultHasher`, which is documented as non-portable across Rust
/// versions. This function reproduces the old algorithm so we can
/// detect and clean up old runtime artifacts during upgrades.
pub fn legacy_instance_id_from_db_path(db_path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let canonical = canonical_db_path(db_path);
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:08x}", hasher.finish() & 0xFFFF_FFFF)
}

/// Stop a daemon running under the legacy (DefaultHasher-based) instance ID.
///
/// When upgrading from a version that used `DefaultHasher` to one that
/// uses SHA-256, the instance ID changes. This leaves the old daemon
/// orphaned — still running, still holding the DB write lock, but
/// unreachable via the new socket path. This function detects the old
/// daemon and shuts it down so the new daemon can start cleanly.
pub fn stop_legacy_hash_daemon(db_path: &Path) {
    let new_id = instance_id_from_db_path(db_path);
    let old_id = legacy_instance_id_from_db_path(db_path);

    // If the hashes happen to collide, nothing to migrate.
    if new_id == old_id {
        return;
    }

    let old_pid_path = pidfile_path(&old_id);
    let old_sock_path = socket_path(&old_id);
    let old_binding_path = effective_config_binding_path(&old_id);
    let old_rt_dir = runtime_dir(&old_id);

    if !old_pid_path.exists() && !old_sock_path.exists() && !old_binding_path.exists() {
        return;
    }

    // Try to read the PID and stop the old daemon gracefully.
    if let Ok(content) = std::fs::read_to_string(&old_pid_path)
        && let Ok(pid) = content.trim().parse::<i32>()
        && pid > 0
    {
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        if alive {
            tracing::info!(
                pid,
                old_id = %old_id,
                new_id = %new_id,
                "stopping legacy daemon (hash algorithm upgrade)"
            );
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            // Wait briefly for graceful shutdown.
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_secs(2) {
                if unsafe { libc::kill(pid, 0) != 0 } {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if unsafe { libc::kill(pid, 0) == 0 } {
                tracing::warn!(
                    pid,
                    "legacy daemon did not exit after SIGTERM, sending SIGKILL"
                );
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }

    // Unload the old launchd plist on macOS if it exists.
    let old_plist = launchd_plist_path(&old_id);
    if old_plist.exists() {
        let label = launchd_label(&old_id);
        tracing::info!(label = %label, "unloading legacy launchd plist");
        let _ = std::process::Command::new("launchctl")
            .args([
                "bootout",
                &format!("gui/{}", unsafe { libc::getuid() }),
                &old_plist.display().to_string(),
            ])
            .output();
        let _ = std::fs::remove_file(&old_plist);
    }

    // Clean up stale runtime artifacts.
    let _ = std::fs::remove_file(&old_sock_path);
    let _ = std::fs::remove_file(&old_pid_path);
    let _ = std::fs::remove_file(&old_binding_path);
    let _ = std::fs::remove_dir(&old_rt_dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests that mutate environment variables must hold this lock to avoid
    // racing with each other under `cargo test`'s default parallel execution.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_xdg_runtime<T>(root: &Path, test: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", root);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn with_xdg_state<T>(root: &Path, test: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("XDG_STATE_HOME");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", root);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    #[test]
    fn last_successful_config_roundtrips_with_full_path_identity() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_state(temp.path(), || {
            let db = temp.path().join("db").join("brain.lbug");
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            let config = temp.path().join("instance.toml");
            std::fs::write(&config, "instance_id = \"test\"\n").unwrap();

            let written = write_last_successful_config(&db, &config).unwrap();
            let read = read_last_successful_config(&db).unwrap();
            assert_eq!(read, written);
            assert_eq!(read.database_fingerprint.len(), 64);
            assert_eq!(
                Path::new(&read.config_path),
                std::fs::canonicalize(&config).unwrap()
            );
            assert!(
                last_successful_config_path(&db)
                    .to_string_lossy()
                    .contains(&read.database_fingerprint),
                "the state path itself must use the full fingerprint"
            );

            let replacement = temp.path().join("replacement.toml");
            std::fs::write(&replacement, "instance_id = \"replacement\"\n").unwrap();
            write_last_successful_config(&db, &replacement).unwrap();
            let entries = std::fs::read_dir(last_successful_config_dir(&db))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(entries.len(), 1, "replacement left a temp/backup artifact");
            assert_eq!(entries[0].file_name(), LAST_SUCCESSFUL_CONFIG_FILE);
        });
    }

    #[test]
    fn last_successful_config_is_isolated_and_survives_live_binding_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_state(temp.path(), || {
            let config = temp.path().join("instance.toml");
            std::fs::write(&config, "instance_id = \"test\"\n").unwrap();
            let first = temp.path().join("first.lbug");
            let second = temp.path().join("second.lbug");
            write_last_successful_config(&first, &config).unwrap();
            write_last_successful_config(&second, &config).unwrap();
            assert_ne!(
                last_successful_config_path(&first),
                last_successful_config_path(&second)
            );

            let instance = instance_id_from_db_path(&first);
            let _ = remove_effective_config_binding(&instance);
            assert!(read_last_successful_config(&first).is_ok());
        });
    }

    #[cfg(unix)]
    #[test]
    fn last_successful_config_enforces_private_modes_and_fingerprint() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        with_xdg_state(temp.path(), || {
            let db = temp.path().join("brain.lbug");
            let config = temp.path().join("instance.toml");
            std::fs::write(&config, "instance_id = \"test\"\n").unwrap();
            write_last_successful_config(&db, &config).unwrap();
            let path = last_successful_config_path(&db);
            let parent = path.parent().unwrap();
            assert_eq!(
                std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );

            let mut record = read_last_successful_config(&db).unwrap();
            record.database_fingerprint = "0".repeat(64);
            std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
            assert!(matches!(
                read_last_successful_config(&db),
                Err(LastSuccessfulConfigError::FingerprintMismatch { .. })
            ));

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            assert!(matches!(
                read_last_successful_config(&db),
                Err(LastSuccessfulConfigError::Unsafe { .. })
            ));
        });
    }

    #[test]
    fn last_successful_config_remove_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_state(temp.path(), || {
            let db = temp.path().join("brain.lbug");
            let config = temp.path().join("instance.toml");
            std::fs::write(&config, "instance_id = \"test\"\n").unwrap();
            write_last_successful_config(&db, &config).unwrap();
            remove_last_successful_config(&db).unwrap();
            remove_last_successful_config(&db).unwrap();
            assert!(matches!(
                read_last_successful_config(&db),
                Err(LastSuccessfulConfigError::Absent { .. })
            ));
        });
    }

    #[cfg(unix)]
    #[test]
    fn last_successful_config_rejects_corrupt_version_oversize_and_symlink() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        with_xdg_state(temp.path(), || {
            let db = temp.path().join("brain.lbug");
            let config = temp.path().join("instance.toml");
            std::fs::write(&config, "instance_id = \"test\"\n").unwrap();
            write_last_successful_config(&db, &config).unwrap();
            let path = last_successful_config_path(&db);

            std::fs::write(&path, b"not-json").unwrap();
            assert!(matches!(
                read_last_successful_config(&db),
                Err(LastSuccessfulConfigError::Corrupt { .. })
            ));

            let record = LastSuccessfulConfig::new(&db, &config).unwrap();
            let unsupported = LastSuccessfulConfig {
                version: LAST_SUCCESSFUL_CONFIG_VERSION + 1,
                ..record
            };
            std::fs::write(&path, serde_json::to_vec(&unsupported).unwrap()).unwrap();
            assert!(matches!(
                read_last_successful_config(&db),
                Err(LastSuccessfulConfigError::UnsupportedVersion { .. })
            ));

            std::fs::write(
                &path,
                vec![b' '; LAST_SUCCESSFUL_CONFIG_MAX_BYTES as usize + 1],
            )
            .unwrap();
            assert!(matches!(
                read_last_successful_config(&db),
                Err(LastSuccessfulConfigError::TooLarge { .. })
            ));

            std::fs::remove_file(&path).unwrap();
            let target = temp.path().join("target.json");
            std::fs::write(&target, b"{}").unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(matches!(
                read_last_successful_config(&db),
                Err(LastSuccessfulConfigError::Unsafe { .. })
            ));

            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, serde_json::to_vec(&unsupported).unwrap()).unwrap();
            std::fs::set_permissions(
                path.parent().unwrap(),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            assert!(matches!(
                read_last_successful_config(&db),
                Err(LastSuccessfulConfigError::Unsafe { .. })
            ));
        });
    }

    #[test]
    fn effective_config_binding_paths_are_isolated_per_instance() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let first = effective_config_binding_path("instance-a");
            let second = effective_config_binding_path("instance-b");
            assert_ne!(first, second);
            assert_eq!(first.file_name().unwrap(), EFFECTIVE_CONFIG_BINDING_FILE);
            assert!(first.starts_with(runtime_dir("instance-a")));
            assert!(second.starts_with(runtime_dir("instance-b")));

            let first_binding =
                EffectiveConfigBinding::new(101, EffectiveConfigBindingSource::CompiledDefaults);
            let second_binding = EffectiveConfigBinding::new(
                202,
                EffectiveConfigBindingSource::Configured {
                    path: "/second.toml".to_string(),
                },
            );
            write_effective_config_binding("instance-a", &first_binding).unwrap();
            write_effective_config_binding("instance-b", &second_binding).unwrap();
            assert_eq!(
                read_effective_config_binding("instance-a").unwrap(),
                first_binding
            );
            assert_eq!(
                read_effective_config_binding("instance-b").unwrap(),
                second_binding
            );
        });
    }

    #[test]
    fn effective_config_binding_roundtrips_configured_and_defaults() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let configured = EffectiveConfigBinding::new(
                41,
                EffectiveConfigBindingSource::Configured {
                    path: "/canonical/instance.toml".to_string(),
                },
            );
            write_effective_config_binding("roundtrip", &configured).unwrap();
            assert_eq!(
                read_effective_config_binding_for_verified_pid("roundtrip", 41).unwrap(),
                configured
            );

            let defaults =
                EffectiveConfigBinding::new(42, EffectiveConfigBindingSource::CompiledDefaults);
            write_effective_config_binding("roundtrip", &defaults).unwrap();
            assert_eq!(
                read_effective_config_binding_for_verified_pid("roundtrip", 42).unwrap(),
                defaults
            );
        });
    }

    #[test]
    fn effective_config_binding_reports_absent_corrupt_version_and_pid_errors() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            assert!(matches!(
                read_effective_config_binding("errors"),
                Err(EffectiveConfigBindingError::Absent { .. })
            ));

            let binding =
                EffectiveConfigBinding::new(7, EffectiveConfigBindingSource::CompiledDefaults);
            write_effective_config_binding("errors", &binding).unwrap();
            let path = effective_config_binding_path("errors");
            std::fs::write(&path, b"not json").unwrap();
            assert!(matches!(
                read_effective_config_binding("errors"),
                Err(EffectiveConfigBindingError::Corrupt { .. })
            ));

            std::fs::write(
                &path,
                br#"{"version":99,"pid":7,"effective_config":{"source":"compiled_defaults"}}"#,
            )
            .unwrap();
            assert!(matches!(
                read_effective_config_binding("errors"),
                Err(EffectiveConfigBindingError::UnsupportedVersion {
                    found: 99,
                    supported: EFFECTIVE_CONFIG_BINDING_VERSION,
                    ..
                })
            ));

            write_effective_config_binding("errors", &binding).unwrap();
            assert!(matches!(
                read_effective_config_binding_for_verified_pid("errors", 8),
                Err(EffectiveConfigBindingError::PidMismatch {
                    expected: 8,
                    found: 7,
                    ..
                })
            ));
        });
    }

    #[test]
    fn effective_config_binding_replacement_leaves_one_complete_record() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            for pid in 1..=20 {
                let binding = EffectiveConfigBinding::new(
                    pid,
                    EffectiveConfigBindingSource::Configured {
                        path: format!("/config/{pid}.toml"),
                    },
                );
                write_effective_config_binding("replace", &binding).unwrap();
                assert_eq!(read_effective_config_binding("replace").unwrap(), binding);
            }
            let parent = effective_config_binding_path("replace")
                .parent()
                .unwrap()
                .to_path_buf();
            let entries = std::fs::read_dir(parent)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(entries.len(), 1, "atomic replacement left a temp artifact");
            assert_eq!(entries[0].file_name(), EFFECTIVE_CONFIG_BINDING_FILE);
        });
    }

    #[test]
    fn effective_config_binding_retries_stale_temp_name_collision() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let path = effective_config_binding_path("collision");
            let parent = path.parent().unwrap();
            std::fs::create_dir_all(parent).unwrap();
            let sequence =
                EFFECTIVE_CONFIG_TEMP_SEQUENCE.load(std::sync::atomic::Ordering::Relaxed);
            let stale_temp = effective_config_temp_path(parent, sequence);
            std::fs::write(&stale_temp, b"orphan from crashed daemon").unwrap();
            let binding =
                EffectiveConfigBinding::new(7, EffectiveConfigBindingSource::CompiledDefaults);

            write_effective_config_binding("collision", &binding).unwrap();

            assert_eq!(read_effective_config_binding("collision").unwrap(), binding);
            assert!(
                stale_temp.exists(),
                "writer must not overwrite stale temp files"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn effective_config_binding_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let binding =
                EffectiveConfigBinding::new(7, EffectiveConfigBindingSource::CompiledDefaults);
            write_effective_config_binding("permissions", &binding).unwrap();
            let mode = std::fs::metadata(effective_config_binding_path("permissions"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        });
    }

    #[cfg(unix)]
    #[test]
    fn effective_config_binding_secures_or_rejects_precreated_runtime_dir() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o777)).unwrap();
        secure_effective_config_runtime_dir_for_owner(&runtime, unsafe { libc::geteuid() })
            .unwrap();
        assert_eq!(
            std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let error = secure_effective_config_runtime_dir_for_owner(
            &runtime,
            unsafe { libc::geteuid() }.saturating_add(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

        let target = temp.path().join("target");
        let linked = temp.path().join("linked");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &linked).unwrap();
        let error =
            secure_effective_config_runtime_dir_for_owner(&linked, unsafe { libc::geteuid() })
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn effective_config_binding_reader_rejects_unsafe_mode_and_oversize() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let binding =
                EffectiveConfigBinding::new(7, EffectiveConfigBindingSource::CompiledDefaults);
            write_effective_config_binding("unsafe", &binding).unwrap();
            let path = effective_config_binding_path("unsafe");
            let parent = path.parent().unwrap();
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(matches!(
                read_effective_config_binding("unsafe"),
                Err(EffectiveConfigBindingError::Unsafe { .. })
            ));
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(
                read_effective_config_binding("unsafe"),
                Err(EffectiveConfigBindingError::Unsafe { .. })
            ));

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::write(
                &path,
                vec![b' '; EFFECTIVE_CONFIG_BINDING_MAX_BYTES as usize + 1],
            )
            .unwrap();
            assert!(matches!(
                read_effective_config_binding("unsafe"),
                Err(EffectiveConfigBindingError::TooLarge { .. })
            ));
        });
    }

    #[test]
    fn legacy_runtime_cleanup_removes_effective_config_binding() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let db_path = temp.path().join("brain.lbug");
            let old_id = legacy_instance_id_from_db_path(&db_path);
            assert_ne!(old_id, instance_id_from_db_path(&db_path));
            let binding =
                EffectiveConfigBinding::new(7, EffectiveConfigBindingSource::CompiledDefaults);
            write_effective_config_binding(&old_id, &binding).unwrap();
            let binding_path = effective_config_binding_path(&old_id);
            assert!(binding_path.exists());

            stop_legacy_hash_daemon(&db_path);

            assert!(!binding_path.exists());
            assert!(!runtime_dir(&old_id).exists());
        });
    }

    #[test]
    fn instance_id_is_8_hex_chars() {
        let path = Path::new("/home/user/.local/share/nestweaver/my-brain/brain.lbug");
        let id = instance_id_from_db_path(path);
        assert_eq!(
            id.len(),
            8,
            "instance_id should be exactly 8 hex chars, got '{id}'"
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "instance_id should be hex only, got '{id}'"
        );
    }

    #[test]
    fn instance_id_different_for_different_paths() {
        let id_a = instance_id_from_db_path(Path::new("/a/foo/brain.lbug"));
        let id_b = instance_id_from_db_path(Path::new("/b/foo/brain.lbug"));
        assert_ne!(
            id_a, id_b,
            "different paths with same dir name must produce different IDs"
        );
    }

    #[test]
    fn instance_id_stable_for_same_path() {
        let path = Path::new("/some/stable/path/brain.lbug");
        let id1 = instance_id_from_db_path(path);
        let id2 = instance_id_from_db_path(path);
        assert_eq!(id1, id2, "same path must produce same ID");
    }

    #[cfg(unix)]
    #[test]
    fn instance_id_canonicalizes_parent_for_missing_db() {
        let tmp = tempfile::tempdir().unwrap();
        let real_parent = tmp.path().join("real");
        let linked_parent = tmp.path().join("linked");
        std::fs::create_dir(&real_parent).unwrap();
        std::os::unix::fs::symlink(&real_parent, &linked_parent).unwrap();

        let via_real = real_parent.join("missing.lbug");
        let via_link = linked_parent.join("missing.lbug");

        assert_eq!(
            canonical_db_path(&via_link),
            canonical_db_path(&via_real),
            "missing DB paths should canonicalize through their parent"
        );
        assert_eq!(
            instance_id_from_db_path(&via_link),
            instance_id_from_db_path(&via_real),
            "socket instance ID should not depend on symlink spelling"
        );
    }

    #[test]
    fn instance_id_fallback_for_bare_filename() {
        let path = Path::new("brain.lbug");
        let id = instance_id_from_db_path(path);
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn instance_label_includes_parent_dir() {
        let path = Path::new("/home/user/.local/share/nestweaver/my-brain/brain.lbug");
        let label = instance_label_from_db_path(path);
        assert!(
            label.starts_with("my-brain-"),
            "label should include parent dir name, got '{label}'"
        );
        assert!(label.len() <= 30, "label should be short, got '{label}'");
    }

    #[test]
    fn socket_path_under_sun_len() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let id = "a1b2c3d4"; // 8-char hash
        let sock = socket_path(id);
        let path_len = sock.as_os_str().len();
        assert!(
            path_len < 104,
            "socket path must be < 104 bytes for macOS, got {path_len}: {}",
            sock.display()
        );
    }

    #[test]
    fn runtime_dir_ignores_tmpdir() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let dir_before = runtime_dir("test1234");
        unsafe {
            std::env::set_var("TMPDIR", "/some/other/tmpdir");
        }
        let dir_after = runtime_dir("test1234");
        unsafe {
            std::env::remove_var("TMPDIR");
        }
        assert_eq!(
            dir_before, dir_after,
            "runtime_dir must not change when TMPDIR changes"
        );
    }

    #[test]
    fn runtime_dir_uses_xdg_when_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        }
        let dir = runtime_dir("abcd1234");
        assert_eq!(dir, PathBuf::from("/run/user/1000/nestweaver/abcd1234"));
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    #[test]
    fn socket_path_is_under_runtime_dir() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let sock = socket_path("test1234");
        assert!(sock.ends_with("daemon.sock"));
        assert!(sock.starts_with(runtime_dir("test1234")));
    }

    /// LOW (SUN_LEN): an over-long runtime dir must not produce an unbindable
    /// socket path — fall back to a short /tmp-based one (and create it).
    #[test]
    fn socket_path_falls_back_to_tmp_when_over_sun_len() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let long = format!("/run/user/{}", "x".repeat(120));
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &long);
        }
        let sock = socket_path("test1234");
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        assert!(
            sock.as_os_str().len() < 104,
            "fallback socket path must fit sun_path: {}",
            sock.display()
        );
        assert!(sock.starts_with("/tmp"), "got {}", sock.display());
        assert!(sock.ends_with("daemon.sock"));
        assert!(sock.parent().is_some_and(|p| p.exists()));
    }

    /// The /tmp fallback dir is created mode 0700 so no other user can
    /// reach in and swap or delete our socket.
    #[test]
    fn fallback_sock_dir_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nw-sock-test");
        let uid = unsafe { libc::getuid() };

        let status = secure_fallback_sock_dir(&dir, uid).unwrap();
        assert_eq!(status, FallbackDirStatus::Created);
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "fallback dir must be mode 0700"
        );
        // Second call verifies without changes.
        let status = secure_fallback_sock_dir(&dir, uid).unwrap();
        assert_eq!(status, FallbackDirStatus::Verified);
    }

    /// A pre-existing dir we own but with a permissive mode is tightened
    /// to 0700 rather than trusted as-is.
    #[test]
    fn fallback_sock_dir_tightens_permissive_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nw-sock-test");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let uid = unsafe { libc::getuid() };

        let status = secure_fallback_sock_dir(&dir, uid).unwrap();
        assert_eq!(status, FallbackDirStatus::ModeTightened);
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "permissive fallback dir must be tightened to 0700"
        );
    }

    /// A dir owned by a DIFFERENT uid (a squat) is refused — never used.
    #[test]
    fn fallback_sock_dir_refuses_foreign_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nw-sock-test");
        std::fs::create_dir(&dir).unwrap();
        let our_uid = unsafe { libc::getuid() };
        // Simulate the squat check from the victim's perspective: the dir is
        // owned by us, but the expected uid is someone else's → mismatch.
        let err = secure_fallback_sock_dir(&dir, our_uid + 1).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn log_path_ends_with_daemon_log() {
        let lp = log_path("test1234");
        assert!(lp.ends_with("daemon.log"));
    }

    /// The boot-failure messages used to hand operators `log_path` alone. That
    /// is the launchd stderr file; the tracing subscriber's daily rolling
    /// appender writes boot timing and startup errors to `daemon.log.<date>`
    /// instead, so the named file never contained the error being hunted.
    #[test]
    fn log_hint_names_the_dated_tracing_file_not_just_stderr() {
        let hint = log_hint("test1234");
        assert!(
            hint.contains("daemon.log.<date>"),
            "hint must point at the dated tracing file: {hint}"
        );
        assert!(
            hint.contains(&log_dir("test1234").display().to_string()),
            "hint must name the directory holding both files: {hint}"
        );
    }

    /// Set BOTH XDG roots under one ENV_LOCK acquisition. Nesting
    /// `with_xdg_state` inside `with_xdg_runtime` would deadlock: ENV_LOCK is a
    /// plain std Mutex and is not reentrant.
    fn with_xdg_state_and_runtime<T>(state: &Path, runtime: &Path, test: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous_state = std::env::var_os("XDG_STATE_HOME");
        let previous_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", state);
            std::env::set_var("XDG_RUNTIME_DIR", runtime);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        unsafe {
            match previous_state {
                Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
            match previous_runtime {
                Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// Write a state directory as a real daemon boot would leave it.
    fn seed_state_dir(instance: &str, db_path: &str) {
        let dir = log_dir(instance);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("daemon.log"),
            format!("[daemon] starting for {db_path} (instance label-{instance})\nDone: 2 notes\n"),
        )
        .unwrap();
    }

    #[test]
    fn instance_dir_allowlist_admits_only_the_instance_id_shape() {
        assert!(is_instance_dir_name("05c1b2b6"));
        assert!(is_instance_dir_name("ffffffff"));
        assert!(is_instance_dir_name("00000000"));
        // The catastrophic one: the persisted-intent directory and its 64-hex
        // database-fingerprint children must be structurally inexpressible.
        assert!(!is_instance_dir_name("config-intent"));
        assert!(!is_instance_dir_name(
            "11f099ee842b018cde34fa942a9b537735ed79ee24a6e31fe59b00315ebee205"
        ));
        // Near misses.
        assert!(
            !is_instance_dir_name("05C1B2B6"),
            "uppercase is not our shape"
        );
        assert!(!is_instance_dir_name("05c1b2b"), "7 chars");
        assert!(!is_instance_dir_name("05c1b2b6a"), "9 chars");
        assert!(!is_instance_dir_name("05c1b2g6"), "g is not hex");
        assert!(!is_instance_dir_name(""));
        assert!(!is_instance_dir_name(".."));
    }

    #[test]
    fn db_path_is_recovered_from_the_daemon_log_boot_line() {
        assert_eq!(
            db_path_from_daemon_log(
                "[daemon] starting for /tmp/x/test.lbug (instance x-05c1b2b6)\nDone: 2 notes\n"
            ),
            Some(PathBuf::from("/tmp/x/test.lbug"))
        );
        // A database path may itself contain the delimiter; the LAST occurrence
        // is the real one.
        assert_eq!(
            db_path_from_daemon_log(
                "[daemon] starting for /tmp/a (instance b)/t.lbug (instance x-05c1b2b6)\n"
            ),
            Some(PathBuf::from("/tmp/a (instance b)/t.lbug"))
        );
        // Unidentifiable inputs yield None, which makes a directory undeletable.
        assert_eq!(db_path_from_daemon_log(""), None);
        assert_eq!(db_path_from_daemon_log("Done: 2 notes\n"), None);
        assert_eq!(
            db_path_from_daemon_log("[daemon] starting for /tmp/x.lbug\n"),
            None
        );
        assert_eq!(
            db_path_from_daemon_log("[daemon] starting for  (instance x)\n"),
            None
        );
    }

    /// The regression guard the hazard section calls non-optional: a populated
    /// `config-intent/` must survive `gc`. A hand-written `find … -exec rm -rf`
    /// once deleted it for real, leaving the daemon with no persisted intent.
    #[test]
    fn gc_state_dirs_never_touches_config_intent() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            let db = PathBuf::from("/tmp/gone-forever-12345/test.lbug");
            let intent_dir = last_successful_config_dir(&db);
            std::fs::create_dir_all(&intent_dir).unwrap();
            let intent_file = intent_dir.join("last-successful-config.json");
            std::fs::write(&intent_file, r#"{"version":1}"#).unwrap();

            // An unambiguous orphan sits beside it so the sweep is not a no-op:
            // a test that deletes nothing would "protect" config-intent trivially.
            seed_state_dir("05c1b2b6", "/tmp/gone-forever-12345/test.lbug");

            let report = gc_orphaned_state_dirs().unwrap();

            assert_eq!(report.removed, vec!["05c1b2b6".to_string()]);
            assert!(
                intent_file.exists(),
                "config-intent must survive gc — deleting it reproduces the \
                 missing-intent failure this sweep is supposed to avoid"
            );
            assert_eq!(
                std::fs::read_to_string(&intent_file).unwrap(),
                r#"{"version":1}"#
            );
        });
    }

    #[test]
    fn gc_state_dirs_reaps_orphans_keeps_live_databases_and_reports_unidentified() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            // Reaped: database path is gone.
            seed_state_dir("aaaaaaaa", "/nonexistent-root-98765/brain.lbug");
            // Reaped: temp database, whether or not it still exists.
            seed_state_dir("bbbbbbbb", "/tmp/ephemeral-54321/test.lbug");
            // Kept: the database still exists at a NON-temp path. It must be
            // outside any tempdir — a `tempfile::tempdir()` lives under /tmp, so
            // the temp rule would reap it and this case would prove nothing.
            // The sweep only calls `.exists()`, so any stable real file serves.
            let live_db = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
            assert!(live_db.exists() && !is_temp_db_path(&live_db));
            seed_state_dir("cccccccc", live_db.to_str().unwrap());
            // Kept and REPORTED: no boot line, so the database cannot be named.
            let unknown = log_dir("dddddddd");
            std::fs::create_dir_all(&unknown).unwrap();
            std::fs::write(unknown.join("daemon.log"), "rotated away\n").unwrap();

            let report = gc_orphaned_state_dirs().unwrap();

            assert_eq!(
                report.removed,
                vec!["aaaaaaaa".to_string(), "bbbbbbbb".to_string()]
            );
            assert_eq!(report.kept, vec!["cccccccc".to_string()]);
            assert_eq!(report.unidentified, vec!["dddddddd".to_string()]);
            assert!(
                report.reclaimed_bytes > 0,
                "reclaimed bytes must be reported"
            );
            assert!(!log_dir("aaaaaaaa").exists());
            assert!(!log_dir("bbbbbbbb").exists());
            assert!(
                log_dir("cccccccc").exists(),
                "a live database keeps its logs"
            );
            assert!(unknown.exists(), "unidentifiable means undeletable");
        });
    }

    /// Never reap a live daemon's directory, even when its database path is
    /// gone — an unmounted volume must not cost a healthy daemon its logs.
    #[cfg(unix)]
    #[test]
    fn gc_state_dirs_spares_a_directory_whose_daemon_holds_the_pidfile_lock() {
        use std::os::unix::io::AsRawFd;
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            seed_state_dir("eeeeeeee", "/nonexistent-root-24680/brain.lbug");
            let pidfile = pidfile_path("eeeeeeee");
            std::fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
            std::fs::write(&pidfile, "12345\n").unwrap();
            let held = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pidfile)
                .unwrap();
            // A separate file description, so the sweep's LOCK_NB attempt
            // genuinely contends the way another process would.
            assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);

            let report = gc_orphaned_state_dirs().unwrap();

            assert_eq!(report.spared, vec!["eeeeeeee".to_string()]);
            assert!(report.removed.is_empty());
            assert!(log_dir("eeeeeeee").exists());

            unsafe {
                libc::flock(held.as_raw_fd(), libc::LOCK_UN);
            }
        });
    }

    /// Fork a child that takes a POSIX write lock (`fcntl(F_SETLK, F_WRLCK)`)
    /// on `path` and holds it until killed. A *separate process* is required:
    /// POSIX record locks never conflict with the calling process, so an
    /// in-process lock would be invisible to `F_GETLK`.
    ///
    /// The child touches only async-signal-safe libc calls after `fork` (the
    /// path CString and the pipe are prepared beforehand), which is what makes
    /// this safe from a multi-threaded test harness.
    fn fork_write_lock_holder(path: &Path) -> i32 {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe {
                libc::close(fds[0]);
                let fd = libc::open(c_path.as_ptr(), libc::O_RDWR);
                if fd < 0 {
                    libc::_exit(11);
                }
                let mut lock: libc::flock = std::mem::zeroed();
                lock.l_type = libc::F_WRLCK as libc::c_short;
                lock.l_whence = libc::SEEK_SET as libc::c_short;
                if libc::fcntl(fd, libc::F_SETLK, &lock) != 0 {
                    libc::_exit(12);
                }
                let ready = [1u8];
                libc::write(fds[1], ready.as_ptr() as *const libc::c_void, 1);
                loop {
                    libc::pause();
                }
            }
        }
        unsafe { libc::close(fds[1]) };
        let mut ready = [0u8; 1];
        let read = unsafe { libc::read(fds[0], ready.as_mut_ptr() as *mut libc::c_void, 1) };
        unsafe { libc::close(fds[0]) };
        assert_eq!(read, 1, "lock-holding child never signalled readiness");
        pid
    }

    fn reap(pid: i32) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }

    /// A database nobody has ever created cannot be owned.
    #[test]
    fn write_lock_probe_reports_free_for_missing_database() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            db_write_lock(&dir.path().join("never-created.lbug")),
            DbWriteLock::Free
        );
    }

    /// An existing but unlocked database is provably free.
    #[test]
    fn write_lock_probe_reports_free_for_unlocked_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"not really a database").unwrap();
        let state = db_write_lock(&db);
        assert_eq!(state, DbWriteLock::Free);
        assert!(state.is_provably_free());
        assert!(!state.is_held());
    }

    /// The whole point: another process holding the database is visible to the
    /// kernel by PID, and stays visible no matter what happens to the pidfile.
    #[test]
    fn write_lock_probe_names_the_holding_process() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"not really a database").unwrap();

        let holder = fork_write_lock_holder(&db);
        let state = db_write_lock(&db);
        assert_eq!(
            state,
            DbWriteLock::Held { pid: Some(holder) },
            "the kernel must name the process holding the database write lock"
        );
        assert!(state.is_held());
        assert!(!state.is_provably_free());

        reap(holder);
        // Once the holder is gone the kernel drops its lock — no file on disk
        // has to be cleaned up for the truth to change.
        assert_eq!(db_write_lock(&db), DbWriteLock::Free);
    }

    /// Ownership survives an operator deleting the pidfile: the flock evidence
    /// disappears with the inode, the database lock does not.
    #[test]
    fn write_lock_probe_survives_pidfile_unlink() {
        use std::os::unix::io::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let pidfile = dir.path().join("daemon.pid");
        std::fs::write(&db, b"not really a database").unwrap();
        std::fs::write(&pidfile, "1\n").unwrap();

        let holder = fork_write_lock_holder(&db);
        std::fs::remove_file(&pidfile).unwrap();

        // A fresh pidfile at the same path flocks trivially — the old proof is
        // gone — while the database lock still names the live owner.
        let fresh = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&pidfile)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(fresh.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "an unlinked-then-recreated pidfile must flock freely (this is the defect)"
        );
        assert_eq!(db_write_lock(&db), DbWriteLock::Held { pid: Some(holder) });

        unsafe { libc::flock(fresh.as_raw_fd(), libc::LOCK_UN) };
        reap(holder);
    }
}
