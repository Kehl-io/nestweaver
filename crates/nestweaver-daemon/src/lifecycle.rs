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
/// Process-local proof that this daemon was started through NestWeaver's
/// managed `daemon start` path. Unlike an environment variable, an external
/// supervisor cannot accidentally inherit or forge this marker. The command
/// sets it only after acquiring or validating the locked spawn descriptor.
static VERIFIED_NESTWEAVER_MANAGED_START: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn mark_verified_nestweaver_managed_start() {
    VERIFIED_NESTWEAVER_MANAGED_START.store(true, std::sync::atomic::Ordering::Release);
}

pub fn clear_verified_nestweaver_managed_start() {
    VERIFIED_NESTWEAVER_MANAGED_START.store(false, std::sync::atomic::Ordering::Release);
}

pub fn is_verified_nestweaver_managed_start() -> bool {
    VERIFIED_NESTWEAVER_MANAGED_START.load(std::sync::atomic::Ordering::Acquire)
}

/// Process-wide lock serializing every test that swaps OR durably reads the
/// XDG/socket-fallback env vars. Defined at module level (not inside the
/// test module) so `server.rs`'s in-process-daemon e2e tests — which resolve
/// runtime/state paths for the daemon's whole lifetime — can serialize
/// against the lifecycle unit tests that flip those vars.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// Canonical path to use when *naming* a brain's local state.
///
/// Identity must survive a publication cutover, so it is anchored to the
/// stable base database rather than the currently-selected slot. See
/// [`nestweaver_engine::publication::instance_anchor_database`] (nw-145).
///
/// Deliberately NOT used by the write-lock probe, which must open the real
/// selected graph, nor by [`legacy_instance_id_from_db_path`], which has to
/// reproduce a historical hash exactly in order to clean up after it.
fn identity_db_path(db_path: &Path) -> PathBuf {
    canonical_db_path(&nestweaver_engine::publication::instance_anchor_database(
        db_path,
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

    let canonical = identity_db_path(db_path);
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

/// The authority responsible for keeping a daemon alive.
///
/// Only `NestweaverManaged` authorizes in-client version replacement. This
/// covers manual and automatic `daemon start`, both of which prove the spawn
/// lock before forking. Foreground and supervisor-owned daemons must be
/// restarted by their owner so a detached process cannot escape it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonLifecycleOwner {
    NestweaverManaged,
    #[default]
    ExternalOrUnknown,
}

/// Versioned daemon-owned live binding between an instance, PID, and its
/// effective configuration. This record is meaningful only while `pid` still
/// identifies the daemon holding the corresponding pidfile lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveConfigBinding {
    pub version: u32,
    pub pid: u32,
    pub effective_config: EffectiveConfigBindingSource,
    #[serde(default)]
    pub lifecycle_owner: DaemonLifecycleOwner,
}

impl EffectiveConfigBinding {
    pub fn new(pid: u32, effective_config: EffectiveConfigBindingSource) -> Self {
        Self::new_with_lifecycle_owner(
            pid,
            effective_config,
            DaemonLifecycleOwner::ExternalOrUnknown,
        )
    }

    pub fn new_with_lifecycle_owner(
        pid: u32,
        effective_config: EffectiveConfigBindingSource,
        lifecycle_owner: DaemonLifecycleOwner,
    ) -> Self {
        Self {
            version: EFFECTIVE_CONFIG_BINDING_VERSION,
            pid,
            effective_config,
            lifecycle_owner,
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
    nestweaver_store::canonical_db_path(db_path)
}

/// Reproduce the PRE-nw-208 canonicalization exactly.
///
/// [`canonical_db_path`] now joins the cwd for an unresolvable relative path,
/// which is the fix for a forked identity. `legacy_instance_id_from_db_path`
/// must NOT inherit that: it exists to recompute a hash a previous release
/// already wrote, and a legacy daemon that registered under the RELATIVE form
/// stays unreachable if we look for it under the absolute one — leaving it
/// holding the write lock, which is precisely the orphan the retirement path
/// exists to clear. This is the same reasoning the module header gives for
/// keeping `identity_db_path` away from that function.
fn legacy_canonical_db_path(db_path: &Path) -> PathBuf {
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
    instance_id_from_canonical_path(&identity_db_path(db_path))
}

/// Hash an ALREADY-canonical path into an instance id.
///
/// Extracted so an identity migration can reproduce a superseded id exactly —
/// `stop_selected_slot_identity_daemon` needs the pre-anchoring hash of the
/// SELECTED path — without duplicating the digest and drifting from it.
fn instance_id_from_canonical_path(canonical: &Path) -> String {
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
    let canonical = identity_db_path(db_path);
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

/// Root of the `/tmp` socket-fallback tree: `/tmp/nw-sock-<uid>`.
///
/// `NESTWEAVER_SOCK_FALLBACK_DIR` overrides the whole root. It exists so
/// tests can point the daemon, the client, AND `daemon gc` at one scratch
/// directory: the real root belongs to the operator's daemons, and a test
/// that swept it could delete a live daemon's fallback socket. Every process
/// that must agree on the socket location reads the same variable, exactly
/// like `XDG_RUNTIME_DIR`.
pub fn socket_fallback_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("NESTWEAVER_SOCK_FALLBACK_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from("/tmp").join(format!("nw-sock-{uid}"))
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
    let fallback_dir = socket_fallback_root();
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

/// Records that THIS process holds a read-write store open on some database
/// without a canonical [`DbWriteLease`]. Set by the daemon after
/// `GraphStore::open_or_create`; read by [`db_write_lock`] alongside the
/// store's per-database self-ownership latch to refuse a probe that would
/// destroy this process's POSIX lock.
static LOCAL_STORE_WRITE_LOCK_HELD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Declare whether this process now holds a read-write store open.
///
/// This exists solely to arm the guard in [`db_write_lock`]. Call it with
/// `true` immediately after a read-write `GraphStore` open.
///
/// Nothing calls it with `false` in production and nothing should need to: the
/// daemon holds its store for the whole process lifetime, and a process that
/// released the store would gain nothing from probing a lock it no longer
/// holds. `false` exists so tests can restore global state, and so a future
/// caller with a genuinely scoped store open has a way to disarm.
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
/// So this is only safe from a process that has neither the store open nor a
/// canonical writer authority for this database — the CLI before it starts
/// anything, or the daemon before its own store open.
///
/// The guard below is a REAL RUNTIME BRANCH, not a `debug_assert`. Release
/// builds disable debug assertions by default (this workspace sets no
/// `[profile.release] debug-assertions` override), so an assertion would have
/// left the shipped binaries — the only ones where this matters — completely
/// unguarded. Returning `Unknown` costs one relaxed atomic load and every
/// caller already fails closed on it.
///
/// (A `/proc/locks` reader would avoid the descriptor entirely on Linux. It can
/// distinguish a failed `stat` from a successful one with no matching row, so
/// the ambiguity is manageable; what kills it is that there is no macOS
/// equivalent, which would leave the hazard live on half the platforms. The
/// runtime guard makes the question moot.)
pub fn db_write_lock(db_path: &Path) -> DbWriteLock {
    db_write_lock_probe(
        local_store_write_lock_held()
            || nestweaver_store::current_process_claims_write_lease(db_path),
        db_path,
    )
}

/// [`db_write_lock`] with the "do I hold the store open?" answer passed in.
///
/// Split out so the guard can be tested by VALUE rather than by mutating
/// `LOCAL_STORE_WRITE_LOCK_HELD`. A test that flips a process-global flag
/// races every sibling test that probes a lock — cargo runs them on parallel
/// threads, and a mutex the siblings do not take guards nothing. That is not a
/// hypothetical: the earlier version of this test failed 2 runs in 60 at
/// `--test-threads=8`, in CI's main job.
fn db_write_lock_probe(local_store_held: bool, db_path: &Path) -> DbWriteLock {
    if local_store_held {
        // Probing from here would close a descriptor to this database and take
        // this process's own POSIX record locks down with it, silently
        // admitting a second writer. Refuse. `Unknown` is the fail-closed
        // answer: no caller treats it as free.
        tracing::error!(
            db = %db_path.display(),
            "refusing to probe the database write lock from a process that holds the store \
             open — the probe would release this process's own lock"
        );
        return DbWriteLock::Unknown;
    }

    fcntl_write_lock_state(&canonical_db_path(db_path))
}

/// `F_GETLK` for a whole-file write lock on one path.
///
/// Factored out of [`db_write_lock_probe`] so [`checkpoint_artifacts`] asks the
/// kernel the same question in the same way rather than restating the `flock`
/// setup — a restated predicate is how the twin class recurs, and this one has
/// three fields that all have to be zeroed correctly to mean anything.
///
/// A path that does not exist is `Free`: there is nothing to hold.
///
/// The same caveat as [`db_write_lock_probe`] applies and is the caller's to
/// respect — this opens and closes a descriptor, which drops every POSIX record
/// lock THIS process holds on that file.
fn fcntl_write_lock_state(path: &Path) -> DbWriteLock {
    use std::os::unix::io::AsRawFd;

    if !path.exists() {
        return DbWriteLock::Free;
    }
    let Ok(file) = std::fs::File::open(path) else {
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

/// nw-367. Why a read-only open is refusing this database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointArtifacts {
    /// Neither `<db>.wal.checkpoint` nor `<db>.shadow` is present, so nothing
    /// here blocks a read-only open.
    Absent,
    /// Artifacts are present and something is, or may be, holding a checkpoint.
    /// Fail closed — see the type-level note on why `Unknown` lands here.
    LiveOrUnknown,
    /// Artifacts are present and NOTHING holds either checkpoint lock or the
    /// database write lock. Debris from a crash during a checkpoint.
    Debris { artifacts: Vec<PathBuf> },
}

/// nw-359 leg (1). Is this database's write-ahead log unreadable?
///
/// `Some(error)` means, and means ONLY, that a probe open classified the log as
/// [`nestweaver_store::CorruptionKind::WalUnreadable`]. Every other answer —
/// including a probe that itself failed for an unrelated reason — is `None`,
/// because a transient probe error must not become an outage.
///
/// # The narrowness is the point, and the excluded cases are not oversights
///
/// * `WalUnreplayed` must still spawn. It is precisely the state where starting
///   a READ-WRITE daemon is the correct remedy: the daemon replays the log. A
///   guard that refused there would be nw-333's unconditional attribution
///   pointing the other way, and `corruption_diagnostic` says so in its own
///   words — "readable and merely owed a replay. Not a damaged file."
/// * nw-367's checkpoint debris must still spawn, for the same reason and more
///   strongly: a read-write open is the PUBLISHED REMEDY for it, so refusing
///   here would break the instruction this release just shipped.
/// * A database that does not exist yet must still spawn — that is a cold start
///   creating one.
///
/// An unreadable log is different in kind: no open replays it, read-only or
/// read-write, however often it is retried. Starting a daemon against it is
/// what produces the crash-restart loop, and step 1 of the runbook the product
/// prints for this state is "stop everything that opens this database".
pub fn db_wal_unreadable(db_path: &Path) -> Option<nestweaver_store::StoreError> {
    if !db_path.exists() {
        return None;
    }
    match nestweaver_store::GraphStore::open_read_only_without_migration(db_path) {
        Ok(_) => None,
        Err(error) => (error.corruption_kind()
            == Some(nestweaver_store::CorruptionKind::WalUnreadable))
        .then(|| error.with_db_path(db_path)),
    }
}

/// nw-367. Distinguish a LIVE checkpoint from the debris a crashed one leaves.
///
/// The engine raises one message —
/// *"Cannot open database in read-only mode while checkpoint is in progress.
/// Please retry later."* — from two sites with opposite remedies
/// (`wal_replayer.cpp`):
///
/// * `tryAcquireReadOnlyCheckpointLock` throws only when a `READ_LOCK` on an
///   EXISTING lock file fails, i.e. a writer genuinely holds it. A lock file
///   that merely exists returns early and is harmless — so the two
///   `.checkpoint.*.lock` files are NOT causal, which is where the item's own
///   description was wrong.
/// * `throwIfReadOnlyCheckpointState` throws on pure FILE EXISTENCE of
///   `<db>.wal.checkpoint` or `<db>.shadow`, consulting no process at all.
///   This is the site an operator actually meets.
///
/// Three properties make the two decidable from outside, and all three are
/// needed:
///
/// 1. The checkpointer takes `F_WRLCK` POSIX record locks
///    (`checkpointer.cpp`, `acquireCheckpointLocks`). **The kernel releases
///    those when the holder dies**, so they cannot go stale: a held write lock
///    PROVES a live checkpointer.
/// 2. `releaseCheckpointLocks` runs LAST — after `clearFrozenWAL()` and
///    `shadowFile.reset()`. So while the locks are held the artifacts may
///    exist, and once the locks are released the artifacts are already gone.
///    There is no benign window in which "no lock held AND artifacts present"
///    describes a healthy checkpoint.
/// 3. Belt and braces for the one window that could not be decided from the
///    engine source — whether any path writes `<db>.wal.checkpoint` BEFORE
///    acquiring the locks — the database write lock must ALSO be provably
///    free. A checkpoint only ever happens inside a process holding that lock,
///    so a free one means no checkpointer exists, full stop.
///
/// `Unknown` on any probe means `LiveOrUnknown`, per the standing rule on
/// [`DbWriteLock`]: callers must treat it as "possibly owned", never as free.
/// Calling a live checkpoint "debris" would be an unconditional attribution
/// pointing the other way, which is the error this whole class exists to avoid.
///
/// # Do not call this from a process holding the store open
///
/// [`db_write_lock`]'s hazard applies verbatim: the probe opens and closes
/// descriptors and would drop this process's own record locks. The guard in
/// [`db_write_lock_probe`] covers the database file, and the answer it returns
/// there (`Unknown`) makes this function fail closed for free.
pub fn checkpoint_artifacts(db_path: &Path) -> CheckpointArtifacts {
    let canonical = canonical_db_path(db_path);
    let sidecar = |suffix: &str| {
        let mut name = canonical.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };

    let artifacts: Vec<PathBuf> = [".wal.checkpoint", ".shadow"]
        .iter()
        .map(|suffix| sidecar(suffix))
        .filter(|path| path.exists())
        .collect();
    if artifacts.is_empty() {
        return CheckpointArtifacts::Absent;
    }

    for suffix in [".checkpoint.intent.lock", ".checkpoint.apply.lock"] {
        if fcntl_write_lock_state(&sidecar(suffix)) != DbWriteLock::Free {
            return CheckpointArtifacts::LiveOrUnknown;
        }
    }
    if !db_write_lock(db_path).is_provably_free() {
        return CheckpointArtifacts::LiveOrUnknown;
    }

    CheckpointArtifacts::Debris { artifacts }
}

/// Canonical writer and destructive-namespace authority primitives.
///
/// The writer lease holds its process-local canonical-path claim, shared data-
/// directory namespace flock (outside system scratch roots), and stable
/// sidecar flock for its full lifetime. At acquisition it also takes lbug's
/// POSIX database-lock class to exclude a pre-upgrade incumbent; that process-
/// scoped compatibility lock can later transfer to, or be shortened by, the
/// store's own descriptor lifecycle without weakening the durable upgraded-
/// writer claims. A diagnostic self-ownership latch is armed after the OS
/// locks are held so the writer's own read-only open is not mistaken for an
/// external writer.
pub use nestweaver_store::{
    DbNamespaceLease, DbWriteLease, WriteLeaseError, acquire_db_namespace_lease,
    acquire_db_write_lease, acquire_db_write_lease_under_namespace, write_lease_path,
};
/// PID of the process on the other end of a connected unix socket, as
/// reported by the kernel. Unlike the pidfile (whose contents can be
/// overwritten while the daemon still holds its flock), this cannot be faked
/// by another process. Integration point: a future daemon self-reported-PID RPC
/// can supersede this once it lands.
#[cfg(target_os = "linux")]
pub fn unix_socket_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<i32> {
    use std::os::unix::io::AsRawFd;
    #[repr(C)]
    struct UCred {
        pid: libc::pid_t,
        uid: libc::uid_t,
        gid: libc::gid_t,
    }
    let mut cred = UCred {
        pid: -1,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<UCred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut UCred as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid)
}

/// Linux companion to [`unix_socket_peer_pid`] that also exposes the peer's
/// uid: one `SO_PEERCRED` call fills one `UCred`, so the uid comes from the
/// same kernel evidence. Adopting a pidfile-less daemon requires BOTH fields
/// — a same-uid, PID-matching socket peer is corroboration an unlinked
/// pidfile cannot erase. Linux-only because no other platform's peer-PID
/// call reports a uid, and no new macOS-only surface may be added from this
/// Linux tree.
#[cfg(target_os = "linux")]
pub fn unix_socket_peer_cred(stream: &std::os::unix::net::UnixStream) -> Option<(i32, u32)> {
    use std::os::unix::io::AsRawFd;
    #[repr(C)]
    struct UCred {
        pid: libc::pid_t,
        uid: libc::uid_t,
        gid: libc::gid_t,
    }
    let mut cred = UCred {
        pid: -1,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<UCred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut UCred as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some((cred.pid, cred.uid))
}

/// macOS equivalent of Linux `SO_PEERCRED` — XNU's `LOCAL_PEERPID`.
#[cfg(target_os = "macos")]
pub fn unix_socket_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<i32> {
    use std::os::unix::io::AsRawFd;
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 0x002;
    let mut pid: libc::pid_t = -1;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            SOL_LOCAL,
            LOCAL_PEERPID,
            &mut pid as *mut libc::pid_t as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0 && pid > 0).then_some(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn unix_socket_peer_pid(_stream: &std::os::unix::net::UnixStream) -> Option<i32> {
    None
}

fn persistent_state_root() -> PathBuf {
    // Honour an explicitly-set $XDG_STATE_HOME on EVERY platform, mirroring
    // what `runtime_dir` already does with $XDG_RUNTIME_DIR a few hundred
    // lines up. `dirs::state_dir()` returns the variable on Linux but `None`
    // on macOS, which has no XDG state concept, so relying on it alone meant
    // the variable was silently ignored there.
    //
    // That inconsistency was not only a portability wart: the daemon GC tests
    // set $XDG_STATE_HOME to a tempdir and seed instance directories under it,
    // so on macOS they wrote into the developer's REAL
    // ~/.local/state/nestweaver/ while asserting against the tempdir. Seven
    // tests failed for that reason, and the fixtures they leaked
    // (`abcdabcd`, `aaaaaaaa`, `ffffffff`, ...) accumulated in the user's
    // state directory — the same directory nw-129 tracks for leaks (nw-194).
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(xdg).join("nestweaver");
    }
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
/// caller has already verified — through the held pidfile flock, or through
/// kernel socket peer credentials when the pidfile was unlinked under a live
/// daemon. This helper does not itself prove process liveness or ownership;
/// PID equality alone is vulnerable to stale files and PID reuse.
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

/// Unlink every per-instance directory a TEMP-database daemon created, on its
/// clean shutdown: the runtime dir (socket, pidfile, spawnlock, live binding),
/// the state dir (daemon logs), and the `/tmp` socket-fallback dir when one
/// was used. Temp databases (tests, throwaway repros — see [`is_temp_db_path`])
/// spawn daemons by the thousand and each one used to leave all three behind
/// for `daemon gc` to find; the sweep stays as the backstop for SIGKILLed
/// daemons, but a clean shutdown of an ephemeral daemon should leave nothing
/// to sweep.
///
/// The gate is the same [`is_temp_db_path`] predicate the sweep and launchd
/// use, so the three can never disagree about what is ephemeral. A daemon
/// serving a REAL database returns immediately: its logs and runtime records
/// are operator diagnostics, not litter.
///
/// Two vetoes, both checked inside. First, a held `daemon.spawnlock`: a
/// client mid-respawn (`daemon restart`, autostart) holds the spawnlock
/// across this daemon's shutdown and hands the locked file to the child,
/// which refuses to start when the path it inherited is gone — so a held
/// spawnlock passes every directory to the successor daemon untouched.
/// Second, the residual race the spawnlock cannot cover — and the veto is a
/// PROBE, not a hold, so it cannot close this: a client that acquires the
/// spawnlock in the probe→removal gap hands its child a locked inode whose
/// PATH this removal then deletes, and the child aborts the restart with
/// "inherited parent spawnlock cannot be matched" — the exact client-visible
/// failure the veto exists to prevent, shrunk to a microseconds window. A
/// successor that simply recreates the runtime dir in that window can
/// likewise lose its files — the same race the caller's socket/pidfile
/// unlinks already have. Accepted for the temp-database scope (test
/// daemons), where an unreachable-daemon recovery path exists and a failed
/// autostart is retried.
///
/// Callers must invoke this only on the clean-shutdown path, after the
/// instance's socket and pidfile are unlinked and before the process exits,
/// while the instance flock is still held.
pub fn remove_instance_dirs_for_temp_db(db_path: &Path, instance_id: &str) {
    if !is_temp_db_path(db_path) {
        return;
    }
    let runtime = runtime_dir(instance_id);
    // A client mid-respawn (`daemon restart`, autostart) holds the instance's
    // spawnlock ACROSS this daemon's shutdown and hands the locked file to the
    // child, which refuses to start when the path it inherited is gone.
    // Unlinking the runtime dir under that handshake kills the respawn, so a
    // held spawnlock vetoes every removal below and the dirs pass to the
    // successor daemon instead. (The pre-change restart test caught exactly
    // this: "inherited parent spawnlock cannot be matched".)
    if pidfile_lock_held_at(&runtime.join("daemon.spawnlock")) {
        tracing::info!(
            instance = instance_id,
            "spawnlock held — a client is mid-spawn; leaving the instance dirs \
             for the successor daemon"
        );
        return;
    }
    for dir in [
        runtime,
        socket_fallback_root().join(instance_id),
        log_dir(instance_id),
    ] {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            // The fallback dir exists only when the sun_path fallback was
            // used, and on macOS the runtime dir IS the state dir.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                dir = %dir.display(),
                error = %error,
                "temp-database shutdown could not remove a directory — daemon gc \
                 remains the backstop"
            ),
        }
    }
}

/// One of the three roots a daemon writes per-instance directories under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GcRoot {
    /// `~/.local/state/nestweaver/<instance>` (or `$XDG_STATE_HOME`) — daemon
    /// logs. Also the identification source for every root: the boot line in
    /// `daemon.log` is the only record tying an instance id back to its
    /// database path.
    PersistentState,
    /// `$XDG_RUNTIME_DIR/nestweaver/<instance>` — socket, pidfile, spawnlock,
    /// live config binding. Distinct from the state root only where
    /// `XDG_RUNTIME_DIR` is set; elsewhere [`runtime_dir`] falls back to the
    /// state root and there is no second root to sweep.
    RuntimeDir,
    /// `/tmp/nw-sock-<uid>/<instance>` — sun_path fallback sockets, created
    /// only when the runtime-dir socket path would exceed 104 bytes.
    SocketFallback,
}

/// The roots [`gc_orphaned_daemon_dirs`] sweeps, resolved once so a test can
/// point every one of them at a scratch directory. Sweeping the operator's
/// real roots is `daemon gc`'s job and no test's.
#[derive(Debug, Clone)]
pub struct DaemonGcRoots {
    /// `persistent_state_root()` — daemon logs, and the identification source
    /// (`daemon.log`) for entries found under the other two roots.
    pub state: PathBuf,
    /// The runtime root when it differs from the state root
    /// (`XDG_RUNTIME_DIR` set); `None` where [`runtime_dir`] falls back to
    /// the state root.
    pub runtime: Option<PathBuf>,
    /// `socket_fallback_root()` — the `/tmp/nw-sock-<uid>` tree.
    pub socket_fallback: PathBuf,
}

impl DaemonGcRoots {
    /// Where an instance's pidfile lives: the runtime root when distinct,
    /// else the state root — mirroring [`pidfile_path`].
    fn pidfile_root(&self) -> &Path {
        self.runtime.as_deref().unwrap_or(&self.state)
    }
}

/// Resolve the sweep roots from this process's environment — what the real
/// `daemon gc` runs against.
pub fn daemon_gc_roots() -> DaemonGcRoots {
    let state = persistent_state_root();
    // Mirror runtime_dir()'s semantics exactly, including its UTF-8 `var`
    // (not `var_os`) read: when XDG_RUNTIME_DIR is unset the runtime dir IS
    // the state root. The same collapse applies when it points AT the state
    // root — filter that too, or the same directory is swept under two root
    // labels (the second pass reports NotFound, so nothing is double-deleted,
    // but the roots must still be reported as one).
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|xdg| PathBuf::from(xdg).join("nestweaver"))
        .filter(|root| *root != state);
    DaemonGcRoots {
        state,
        runtime,
        socket_fallback: socket_fallback_root(),
    }
}

/// Outcome of a [`gc_orphaned_daemon_dirs`] pass.
#[derive(Debug, Default)]
pub struct DaemonGcReport {
    /// Instance directories deleted (database gone or under a temp dir), as
    /// (root, instance) pairs — an instance reclaimed from two roots appears
    /// once per root.
    pub removed: Vec<(GcRoot, String)>,
    /// Bytes reclaimed by those deletions.
    pub reclaimed_bytes: u64,
    /// Instances whose database still exists.
    pub kept: Vec<String>,
    /// Instances spared because something still owns them: the
    /// database write lock, an unreadable database lock state, or the pidfile
    /// flock. The union of the three lists below, deduplicated — a healthy
    /// daemon appears in two of them, because two independent facts are true of
    /// it at once. Sparing applies to EVERY root the instance occupies: a
    /// live daemon's runtime files are never reclaimed.
    pub spared: Vec<String>,
    /// Instances whose database write lock the kernel reports as HELD, with the
    /// holder PID when the platform named one. Proof an `rm -f daemon.pid`
    /// cannot erase, because it lives on the database file itself.
    pub spared_database_write_lock: Vec<(String, Option<i32>)>,
    /// Instances whose database lock state could not be READ. This is not
    /// evidence of ownership; it is a refusal to guess. Reported apart from
    /// [`DaemonGcReport::spared_database_write_lock`] because "a process holds
    /// this database" and "we could not tell" are different facts and must
    /// never share a sentence.
    pub spared_database_unreadable: Vec<String>,
    /// Instances whose pidfile flock is contended. Real evidence in the normal
    /// case, and forgeable by unlinking the pidfile — which is why it is
    /// reported alongside the database answer rather than instead of it.
    pub spared_pidfile_lock: Vec<String>,
    /// Instances left alone because their database could not be identified —
    /// in every root they occupy. Not an error: unidentifiable means
    /// undeletable, by design.
    pub unidentified: Vec<String>,
    /// The socket-fallback root was skipped because it is not a real
    /// directory owned by this user — the squat shape
    /// [`secure_fallback_sock_dir`] refuses at daemon startup. A
    /// foreign-owned dir in sticky `/tmp` can be neither repaired nor removed
    /// by us, so the whole root is left for its owner or root to clear.
    pub socket_fallback_root_untrusted: bool,
}

/// Everything known about who still owns a state directory's instance.
///
/// Both facts, always, because they answer different questions and a healthy
/// daemon makes both true at once. An earlier version returned the first
/// evidence it found and stopped: a perfectly healthy daemon with an intact,
/// flocked pidfile was then reported under a heading that said its pidfile
/// evidence was "absent or unreadable", which was simply false in the normal
/// case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateDirOwnership {
    /// What the kernel says about this database's write lock.
    database: DbWriteLock,
    /// Whether the pidfile flock is contended.
    pidfile_lock_held: bool,
}

impl StateDirOwnership {
    /// Anything but "the database is provably free AND nobody holds the
    /// pidfile" means keep the directory. `Unknown` is not ownership, but it is
    /// not permission to delete either.
    fn is_owned(self) -> bool {
        !self.database.is_provably_free() || self.pidfile_lock_held
    }
}

/// Probe both ownership facts for `instance_id`.
///
/// The pidfile flock alone is not an ownership test. A flock lives on an
/// *inode*: an operator's `rm -f daemon.pid` during recovery leaves the live
/// daemon holding a lock on an unlinked inode, and the next `flock` at that
/// path succeeds against no contention at all. That is the exact forgery
/// behind the runtime-ownership incident, and this sweep — which runs in
/// precisely the situations where the pidfile has been disturbed — used to
/// depend on it alone.
///
/// [`db_write_lock`] is the corroboration: it asks the kernel who holds the
/// database file itself, which is the data, so nobody deletes it during
/// recovery. `Unknown` fails closed exactly as it does for the runtime-removal
/// callers.
///
/// Residual, stated rather than hidden: the database probe is by PATH. If the
/// database was renamed or deleted out from under a running daemon (an
/// unmounted volume, a test that removed its temp dir), the probe finds nothing
/// and only the pidfile flock is left to speak. That corner — database path
/// gone AND pidfile removed — remains undetectable here, and costs the instance
/// its logs, not its socket or its life. There is no socket-peer probe:
/// [`socket_path`] creates and logs on the over-long-path fallback, which is
/// not acceptable to run per candidate across thousands of directories.
///
/// Must not be called from a process holding a read-write store open: the probe
/// would fail closed for every candidate (see [`db_write_lock`]) and the sweep
/// would spare everything. `daemon gc` runs in a CLI process that opens no
/// store.
fn state_dir_ownership(pidfile: &Path, db_path: &Path) -> StateDirOwnership {
    StateDirOwnership {
        database: db_write_lock(db_path),
        pidfile_lock_held: pidfile_lock_held_at(pidfile),
    }
}

/// Is a live daemon holding the lock on the pidfile at `pidfile`?
///
/// The daemon takes the pidfile `flock(LOCK_EX)` BEFORE opening the database
/// (`claim_instance_lock` precedes `GraphStore::open_or_create`), and releases
/// it deterministically at the end of `serve()` while the database write lock
/// — held by `Arc<GraphStore>` clones — can outlive it. A held lock therefore
/// correlates with a live daemon only between those two edges: at startup the
/// flock is held before the DB is open, and during shutdown it is released
/// while the write lock is still held. Fails toward `true` (spare) on any
/// error we cannot interpret.
///
/// Corroborating evidence only — see [`state_dir_ownership`] for why this can never
/// be the sole ownership test.
#[cfg(unix)]
fn pidfile_lock_held_at(pidfile: &Path) -> bool {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pidfile)
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
fn pidfile_lock_held_at(_pidfile: &Path) -> bool {
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
/// The database a daemon booted against, read from its log directory.
///
/// The boot line can land in either of TWO files, and `gc` previously read
/// only the first:
///
///   * `daemon.log` — launchd's stderr redirect, present only when the daemon
///     was started from a plist;
///   * `daemon.log.<YYYY-MM-DD>` — the tracing subscriber's daily rolling
///     appender, which every daemon writes.
///
/// A daemon spawned WITHOUT launchd therefore has no undated `daemon.log` at
/// all. On macOS that is every daemon started without a plist, so `gc` could
/// not identify a single one: it reported live daemons as
/// "database not identifiable from daemon.log" and never reached the
/// write-lock ownership check that would have spared them. `log_hint` already
/// documented this two-file split for operators; the sweep just never used it.
///
/// Dated files are tried newest-first, so a long-lived directory resolves from
/// the most recent boot rather than a stale one.
fn db_path_from_log_dir(log_dir: &Path) -> Option<PathBuf> {
    let undated = std::fs::read_to_string(log_dir.join("daemon.log")).unwrap_or_default();
    if let Some(db_path) = db_path_from_daemon_log(&undated) {
        return Some(db_path);
    }
    let mut dated: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("daemon.log."))
        })
        .collect();
    // Lexicographic order on `daemon.log.YYYY-MM-DD` is chronological.
    dated.sort();
    dated.iter().rev().find_map(|path| {
        db_path_from_daemon_log(&std::fs::read_to_string(path).unwrap_or_default())
    })
}

/// Extract the database path from a daemon boot line.
///
/// TWO formats exist, because two writers record the boot and they do not
/// share a format:
///
///   * `[daemon] starting for <db> (instance <id>)` — written to stderr, which
///     launchd captures into the undated `daemon.log`;
///   * `<ts> INFO nestweaver_daemon::server: daemon process starting db=<db>
///     instance=<id>` — written by the tracing subscriber into
///     `daemon.log.<date>`.
///
/// Only the first was parsed, so `gc` could not identify a daemon that had no
/// launchd stderr sink — on macOS, every daemon started without a plist. It
/// reported them as unidentified and never reached the write-lock ownership
/// check that spares a LIVE daemon.
///
/// Both formats put the path before a fixed suffix, and a path may itself
/// contain that suffix, so both anchor on the LAST occurrence.
fn db_path_from_daemon_log(log: &str) -> Option<PathBuf> {
    log.lines().find_map(parse_daemon_boot_line)
}

fn parse_daemon_boot_line(line: &str) -> Option<PathBuf> {
    if let Some(rest) = line.strip_prefix("[daemon] starting for ") {
        let end = rest.rfind(" (instance ")?;
        return non_empty_path(&rest[..end]);
    }
    // Tracing form: the fields trail the message, so slice between them.
    let start = line.find("daemon process starting db=")?;
    let rest = &line[start + "daemon process starting db=".len()..];
    let end = rest.rfind(" instance=")?;
    non_empty_path(&rest[..end])
}

fn non_empty_path(path: &str) -> Option<PathBuf> {
    (!path.is_empty()).then(|| PathBuf::from(path))
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

/// Gather sweep candidates from one root into `candidates`.
///
/// Shared shape filter for every root: the instance-id allowlist first,
/// before reading anything inside, then `symlink_metadata` — never follow a
/// symlink out of a swept root. A root that was never created is a clean
/// machine, not a failure.
fn collect_gc_candidates(
    root_kind: GcRoot,
    root: &Path,
    candidates: &mut std::collections::BTreeMap<String, Vec<(GcRoot, PathBuf)>>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_instance_dir_name(name) {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => {}
            _ => continue,
        }
        candidates
            .entry(name.to_string())
            .or_default()
            .push((root_kind, path));
    }
    Ok(())
}

/// Gather candidates from the `/tmp` socket-fallback root. Returns `false`
/// when the root is not a real directory owned by `expected_uid`.
///
/// Anyone can create paths in `/tmp`, so this root gets an ownership check
/// BEFORE anything inside it is read — the same judgment
/// [`secure_fallback_sock_dir`] makes at daemon startup. A root owned by
/// another uid is a squat: a sweep must neither trust its contents nor "help"
/// by deleting it (a foreign-owned dir in sticky `/tmp` can be neither
/// repaired nor removed by us), so the whole root is skipped and reported.
///
/// Per entry, only directories owned by `expected_uid` become deletion
/// candidates. Deleting a foreign-owned entry is exactly the over-eager sweep
/// the squatting threat model warns about — and the ownership proof below
/// runs before any deletion regardless, so a live daemon's fallback socket is
/// never reclaimed.
///
/// `expected_uid` is a parameter (like [`secure_fallback_sock_dir`]'s) so the
/// refusal is unit-testable without chown.
#[cfg(unix)]
fn collect_fallback_candidates(
    root: &Path,
    expected_uid: u32,
    candidates: &mut std::collections::BTreeMap<String, Vec<(GcRoot, PathBuf)>>,
) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let root_meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        // No fallback root yet is a clean machine, not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if !root_meta.file_type().is_dir()
        || root_meta.file_type().is_symlink()
        || root_meta.uid() != expected_uid
    {
        tracing::warn!(
            dir = %root.display(),
            owner = root_meta.uid(),
            expected = expected_uid,
            "skipping untrusted /tmp socket fallback root — not a directory owned by \
             this user (the squat shape daemon startup refuses); left for its owner"
        );
        return Ok(false);
    }

    let entries = std::fs::read_dir(root)?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_instance_dir_name(name) {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() && !meta.file_type().is_symlink() => {
                if meta.uid() != expected_uid {
                    tracing::warn!(
                        dir = %path.display(),
                        owner = meta.uid(),
                        expected = expected_uid,
                        "leaving a foreign-owned entry in the socket fallback root \
                         for its owner"
                    );
                    continue;
                }
            }
            _ => continue,
        }
        candidates
            .entry(name.to_string())
            .or_default()
            .push((GcRoot::SocketFallback, path));
    }
    Ok(true)
}

/// Reclaim orphaned per-instance daemon directories under ALL THREE roots a
/// daemon writes: the persistent state root (`~/.local/state/nestweaver`,
/// daemon logs), the runtime root (`$XDG_RUNTIME_DIR/nestweaver` — socket,
/// pidfile, spawnlock, live binding), and the `/tmp/nw-sock-<uid>` socket
/// fallback.
///
/// Each directory is keyed by a hash of `--db` and created whenever a daemon
/// auto-spawns. Because writes are daemon-routed, nearly any write-path test
/// spawns one against its temp database, and nothing removed the directories
/// when that database went away. The state sweep alone left the other two
/// roots to grow without bound: in three days on one machine, 2,695 runtime
/// dirs (1,682 holding a `daemon.spawnlock`) and 264 socket-fallback dirs
/// accumulated beside 1,885 state dirs, while `daemon gc` reported "clean".
///
/// Identification is resolved ONCE per instance, from the state root's
/// `daemon.log`, BEFORE anything is deleted — deleting an instance's state
/// dir first would destroy the only record tying its runtime and fallback
/// dirs back to their database.
///
/// Fail-closed at every step, under every root. A directory is deleted only
/// when its name matches the instance-id shape, its database was positively
/// identified, that database is temporary or missing, and nothing still owns
/// the instance — neither the database write lock nor the pidfile flock (see
/// [`state_dir_ownership`]). Anything unreadable, unparseable, ambiguous, or
/// foreign-owned is kept. A live daemon's files are never reclaimed, in any
/// root.
///
/// Every instance that reaches the ownership test has a database path: one
/// that cannot be resolved from `daemon.log` is reported as `unidentified`
/// and left alone *before* the test runs, so there is no candidate whose
/// deletion rests on an unprobed database.
///
/// One residual race, stated rather than hidden. The ownership probe and the
/// deletion are not atomic, so a daemon BOOTING a temp or missing database
/// between the two can lose directories mid-boot: a runtime-dir deletion
/// landing between its pidfile claim and its socket bind unlinks the locked
/// pidfile inode out from under it or fails the bind. The worst case is one
/// retryable autostart failure — pidfile claim and socket bind recreate
/// their directories, the database write lock is unaffected, and the client
/// spawns again — and the same race existed for the state-only sweep before
/// this change widened it to the runtime root. The pidfile flock narrows the
/// window: the child claims it before opening the database or binding the
/// socket, and a claimed flock spares the instance outright, so the
/// uncovered span is only the client's pre-claim spawn phase (the sweep does
/// not consult the client-side spawnlock — taking it here would serialize
/// `daemon gc` against every in-flight spawn on the machine, a worse trade
/// for a cleanup pass).
pub fn gc_orphaned_daemon_dirs() -> std::io::Result<DaemonGcReport> {
    gc_orphaned_daemon_dirs_in(&daemon_gc_roots())
}

/// [`gc_orphaned_daemon_dirs`] against explicit roots — the seam that keeps
/// every test on scratch directories and off the operator's real roots.
fn gc_orphaned_daemon_dirs_in(roots: &DaemonGcRoots) -> std::io::Result<DaemonGcReport> {
    let mut report = DaemonGcReport::default();

    // Phase 1: gather candidates from every root BEFORE any deletion, keyed
    // by instance id, so one identification + one ownership test governs all
    // of an instance's directories at once.
    let mut candidates: std::collections::BTreeMap<String, Vec<(GcRoot, PathBuf)>> =
        Default::default();
    collect_gc_candidates(GcRoot::PersistentState, &roots.state, &mut candidates)?;
    if let Some(runtime) = &roots.runtime {
        collect_gc_candidates(GcRoot::RuntimeDir, runtime, &mut candidates)?;
    }
    #[cfg(unix)]
    {
        let trusted = collect_fallback_candidates(
            &roots.socket_fallback,
            unsafe { libc::geteuid() },
            &mut candidates,
        )?;
        report.socket_fallback_root_untrusted = !trusted;
    }
    #[cfg(not(unix))]
    collect_gc_candidates(
        GcRoot::SocketFallback,
        &roots.socket_fallback,
        &mut candidates,
    )?;

    // Phase 2: identify, test ownership, then reap from every root at once.
    for (name, locations) in candidates {
        // The boot line lives in the state root's log directory, wherever the
        // candidate itself was found — in `daemon.log` for a launchd-started
        // daemon, or in `daemon.log.<date>` for any other.
        let Some(db_path) = db_path_from_log_dir(&roots.state.join(&name)) else {
            report.unidentified.push(name);
            continue;
        };

        let pidfile = roots.pidfile_root().join(&name).join("daemon.pid");
        let ownership = state_dir_ownership(&pidfile, &db_path);
        if ownership.is_owned() {
            // Record every fact that is true, not just the first one found.
            match ownership.database {
                DbWriteLock::Held { pid } => {
                    tracing::info!(
                        instance = name.as_str(),
                        db = %db_path.display(),
                        holder = ?pid,
                        "sparing an instance whose database is still held"
                    );
                    report.spared_database_write_lock.push((name.clone(), pid));
                }
                DbWriteLock::Unknown => {
                    tracing::info!(
                        instance = name.as_str(),
                        db = %db_path.display(),
                        "sparing an instance whose database lock state could not be read"
                    );
                    report.spared_database_unreadable.push(name.clone());
                }
                DbWriteLock::Free => {}
            }
            if ownership.pidfile_lock_held {
                report.spared_pidfile_lock.push(name.clone());
            }
            report.spared.push(name);
            continue;
        }

        if !is_temp_db_path(&db_path) && db_path.exists() {
            report.kept.push(name);
            continue;
        }

        for (root_kind, path) in locations {
            let size = directory_size_bytes(&path);
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    report.reclaimed_bytes += size;
                    report.removed.push((root_kind, name.clone()));
                }
                // A directory that vanished under us needs no reporting; anything
                // else stays visible as still present rather than silently counted.
                // A failure is recorded once per INSTANCE, not once per root —
                // the dedup after the loop keeps a multi-root failure from
                // reporting the same instance twice.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => report.kept.push(name.clone()),
            }
        }
    }

    report.removed.sort();
    report.kept.sort();
    report.kept.dedup();
    report.spared.sort();
    report.spared_database_write_lock.sort();
    report.spared_database_unreadable.sort();
    report.spared_pidfile_lock.sort();
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

    // NOT `canonical_db_path`: see `legacy_canonical_db_path`. This must
    // reproduce what a previous release hashed, including its treatment of an
    // unresolvable relative path.
    let canonical = legacy_canonical_db_path(db_path);
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
    if let Err(error) = retire_daemon_under_old_identity(&old_id, &new_id, "hash algorithm upgrade")
    {
        tracing::warn!(%error, "could not retire daemon under the legacy hash identity");
    }
}

/// Stop a daemon bound to the pre-anchoring, SELECTED-SLOT instance id.
///
/// nw-145 moved instance identity from the selected path to the stable base
/// path. That is the same class of change as the DefaultHasher → SHA-256 move
/// above, and it has the same upgrade hazard: a daemon started by 6.3.0 while a
/// CURRENT pointer existed bound itself to the SLOT-derived id. After the
/// upgrade the new code computes the BASE id, so that daemon is unreachable at
/// the new socket path while still holding the database write lock — and
/// `daemon stop` reports nothing to stop. That is precisely the orphaning
/// nw-145 exists to prevent, reintroduced at the upgrade boundary.
///
/// Resolution is best-effort and deliberately tolerant: a brain that never cut
/// over resolves to its own base path and returns immediately, and an
/// unreadable CURRENT is not a reason to fail a daemon start.
pub fn stop_selected_slot_identity_daemon(db_path: &Path) {
    if let Err(error) = retire_selected_slot_identity_daemon(db_path) {
        tracing::warn!(%error, "could not retire daemon under the selected-slot identity");
    }
}

/// Fallible selected-slot migration for direct lifecycle commands. Unlike the
/// best-effort autostart compatibility hook, this refuses to let a command
/// claim success while a verified old owner is still draining or its identity
/// cannot be proved safely.
pub fn retire_selected_slot_identity_daemon(db_path: &Path) -> anyhow::Result<()> {
    let Some(old_id) = selected_slot_identity_instance_id(db_path) else {
        return Ok(());
    };
    let new_id = instance_id_from_db_path(db_path);
    retire_daemon_under_old_identity(&old_id, &new_id, "publication identity anchoring")
}

/// Return the pre-anchoring selected-slot identity when a publication is
/// active. This read-only probe lets direct lifecycle commands report the same
/// legacy owner that autostart migrates instead of claiming no daemon exists.
pub fn selected_slot_identity_instance_id(db_path: &Path) -> Option<String> {
    let anchor = nestweaver_engine::publication::instance_anchor_database(db_path);
    let root = nestweaver_engine::publication::default_publication_root(&anchor);
    let current = nestweaver_engine::publication::read_current(&root).ok()??;
    // Identity recovery must not open or validate the selected graph: a corrupt
    // slot is exactly when an operator most needs `daemon stop` to find its
    // runtime owner.
    let selected = nestweaver_engine::publication::slot_path(&root, &current.publication_uuid)
        .ok()?
        .join(nestweaver_engine::publication::PUBLICATION_GRAPH_FILE);
    let new_id = instance_id_from_db_path(&anchor);
    // The pre-fix identity: the SELECTED path, hashed WITHOUT the anchor step.
    let old_id = instance_id_from_canonical_path(&canonical_db_path(&selected));
    (old_id != new_id).then_some(old_id)
}

/// The pid of a daemon still running under the pre-anchoring, selected-slot
/// identity, if one exists.
///
/// Read-only counterpart to [`stop_selected_slot_identity_daemon`], so a
/// read-only command (`daemon status`) can REPORT the orphan instead of
/// silently reporting "not running" about a process that still holds the
/// database write lock — the nw-145 symptom, reappearing at the upgrade
/// boundary.
pub fn selected_slot_identity_daemon_pid(db_path: &Path) -> Option<i32> {
    let anchor = nestweaver_engine::publication::instance_anchor_database(db_path);
    let selected = nestweaver_engine::publication::resolve_selected_database(&anchor).ok()?;
    if selected == anchor {
        return None;
    }
    let old_id = instance_id_from_canonical_path(&canonical_db_path(&selected));
    if old_id == instance_id_from_db_path(&anchor) {
        return None;
    }
    let pid = std::fs::read_to_string(pidfile_path(&old_id))
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()?;
    process_is_live(pid).then_some(pid)
}

/// Whether a process id names a process that is still running.
///
/// The one liveness predicate for the whole daemon, so a PID question is
/// answered the same way wherever it is asked — daemon retirement
/// ([`selected_slot_identity_daemon_pid`]) and, since nw-302, watcher
/// ownership.
///
/// `EPERM` counts as ALIVE. `kill(pid, 0)` fails that way when the process
/// EXISTS but belongs to another user; absence of permission is not absence of
/// the process, and treating it as death is how a reclaim steals a watcher from
/// a live owner. A recycled pid likewise reports live, which only ever makes a
/// caller more conservative — never less.
pub fn process_is_live(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Stop and clean up a daemon left behind under a superseded instance id.
///
/// Shared by every identity migration so a new one cannot get the shutdown
/// sequence subtly wrong: verified SIGTERM, bounded drain, unload the launchd
/// plist, then remove runtime artifacts only after ownership is released.
fn retire_daemon_under_old_identity(
    old_id: &str,
    new_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    // If the ids happen to coincide, nothing to migrate.
    if new_id == old_id {
        return Ok(());
    }
    let old_id = old_id.to_string();
    let new_id = new_id.to_string();
    let old_pid_path = pidfile_path(&old_id);
    let old_sock_path = socket_path(&old_id);
    let old_binding_path = effective_config_binding_path(&old_id);
    let old_rt_dir = runtime_dir(&old_id);

    if !old_pid_path.exists() && !old_sock_path.exists() && !old_binding_path.exists() {
        return Ok(());
    }

    // Prefer kernel socket-peer identity; fall back to a pidfile only when its
    // exact inode is still flock-owned and the daemon-owned live binding names
    // the same PID. A numeric pidfile alone may be stale/recycled and never
    // authorizes a signal.
    let socket_pid = std::os::unix::net::UnixStream::connect(&old_sock_path)
        .ok()
        .and_then(|stream| unix_socket_peer_pid(&stream));
    let pidfile_pid = std::fs::read_to_string(&old_pid_path)
        .ok()
        .and_then(|content| content.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0);
    let pidfile_owned = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&old_pid_path)
        .ok()
        .is_some_and(|file| {
            let acquired = unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&file),
                    libc::LOCK_EX | libc::LOCK_NB,
                ) == 0
            };
            if acquired {
                unsafe {
                    libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_UN);
                }
            }
            !acquired
        });
    let bound_pid = pidfile_pid.filter(|pid| {
        pidfile_owned
            && read_effective_config_binding_for_verified_pid(&old_id, *pid as u32).is_ok()
    });
    let verified_pid = socket_pid.or(bound_pid);

    if let Some(pid) = verified_pid {
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        if alive {
            tracing::info!(
                pid,
                old_id = %old_id,
                new_id = %new_id,
                reason,
                "stopping daemon under superseded instance identity"
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
                anyhow::bail!(
                    "daemon PID {pid} under superseded identity {old_id} is still draining after SIGTERM; its runtime ownership was left intact"
                );
            }
        }
    } else if socket_pid.is_some() || pidfile_owned {
        anyhow::bail!(
            "superseded daemon runtime {old_id} is owned but its PID identity could not be verified during {reason}; refusing to signal or clean it"
        );
    }

    // Unload the old launchd plist on macOS if it exists.
    let old_plist = launchd_plist_path(&old_id);
    if old_plist.exists() {
        let label = launchd_label(&old_id);
        tracing::info!(label = %label, "unloading launchd plist for the superseded identity");
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests that mutate environment variables must hold this lock to avoid
    // racing with each other under `cargo test`'s default parallel execution.
    // Alias of the crate-wide lock — server.rs's e2e tests hold the same one.
    use super::TEST_ENV_LOCK as ENV_LOCK;

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

    /// Review finding on nw-145: anchoring identity to the base path is the
    /// same class of change as the DefaultHasher → SHA-256 move, and needs the
    /// same upgrade migration. A daemon started by 6.3.0 while a CURRENT
    /// pointer existed is bound to the SELECTED-SLOT id; after the upgrade the
    /// new code computes the BASE id and cannot reach it, so it keeps holding
    /// the write lock while `daemon stop` reports nothing to stop.
    ///
    /// This pins the two ids the migration must bridge. Without a publication
    /// they coincide and the migration is a no-op, which is what keeps it from
    /// touching an ordinary install.
    #[test]
    fn the_superseded_slot_identity_differs_from_the_anchored_one() {
        let base = Path::new("/data/brain.lbug");
        let selected = PathBuf::from(
            "/data/brain.lbug.publications/slots/550e8400-e29b-41d4-a716-446655440000/graph.lbug",
        );

        // What this build computes, for both paths: the anchored id.
        let anchored = instance_id_from_db_path(base);
        assert_eq!(instance_id_from_db_path(&selected), anchored);

        // What 6.3.0 computed for a selected slot: the un-anchored hash of the
        // SELECTED path. It must differ, or there would be nothing to migrate
        // and an orphaned daemon would be unreachable forever.
        let superseded = instance_id_from_canonical_path(&canonical_db_path(&selected));
        assert_ne!(
            superseded, anchored,
            "the pre-anchoring id must differ, otherwise the migration is dead code"
        );

        // And it must be exactly the un-anchored hash — not some third value —
        // or the migration would look for a daemon that never existed.
        assert_eq!(
            superseded,
            instance_id_from_canonical_path(&canonical_db_path(&selected))
        );

        // A brain that never cut over resolves to itself: no migration, no
        // risk of retiring a healthy daemon on an ordinary install.
        assert_eq!(
            instance_id_from_canonical_path(&canonical_db_path(base)),
            anchored
        );
    }

    #[test]
    fn direct_lifecycle_probe_finds_selected_slot_identity_without_opening_graph() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("brain.lbug");
        let root = nestweaver_engine::publication::default_publication_root(&base);
        std::fs::create_dir_all(&root).unwrap();
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: "11111111-1111-4111-8111-111111111111".to_string(),
            publication_uuid: "22222222-2222-4222-8222-222222222222".to_string(),
        };
        let pointer = nestweaver_engine::publication::CurrentPublicationPointer::new(
            &identity,
            None,
            "a".repeat(64),
        )
        .unwrap();
        std::fs::write(
            nestweaver_engine::publication::current_pointer_path(&root),
            serde_json::to_vec(&pointer).unwrap(),
        )
        .unwrap();

        let selected = nestweaver_engine::publication::slot_path(&root, &identity.publication_uuid)
            .unwrap()
            .join(nestweaver_engine::publication::PUBLICATION_GRAPH_FILE);
        let expected = instance_id_from_canonical_path(&canonical_db_path(&selected));
        assert_eq!(
            selected_slot_identity_instance_id(&base).as_deref(),
            Some(expected.as_str())
        );
        assert!(!selected.exists(), "the probe must not require the graph");
    }

    /// A bare relative db path must resolve to ONE instance id, before and
    /// after the database file exists.
    ///
    /// It did not. `canonicalize` fails on a path that does not exist yet, and
    /// for a BARE filename `parent()` is `Some("")`, which also fails — so the
    /// path was returned relative and hashed to a different id than the same
    /// database gets once created. The first `daemon start` in a fresh
    /// directory therefore bound one identity while every later command looked
    /// for another, leaving a daemon holding the write lock that the CLI could
    /// no longer address.
    ///
    /// Takes ENV_LOCK because it mutates process-global cwd: the test harness
    /// runs this binary's tests on many threads in ONE process, so without it
    /// every concurrent test briefly observes a different working directory.
    /// The cwd is restored by a guard rather than a plain statement, so a panic
    /// between here and the end cannot strand the whole binary in a temp dir
    /// that is about to be deleted.
    #[test]
    fn bare_relative_db_path_identity_is_stable_across_creation() {
        struct CwdGuard(PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }

        let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        // NOT `tempdir_in(".")`: under `cargo test` the cwd is the package
        // root, so that leaves an untracked directory in the source tree when a
        // run is killed. `.gitignore` has no `.tmp*` pattern, so the repo — and
        // the indexer — would pick it up.
        let dir = tempfile::tempdir().unwrap();
        let _cwd = CwdGuard(std::env::current_dir().unwrap());
        std::env::set_current_dir(dir.path()).unwrap();

        let bare = Path::new("nestweaver.lbug");
        let before = instance_id_from_db_path(bare);
        std::fs::write(bare, b"x").unwrap();
        let after = instance_id_from_db_path(bare);

        assert_eq!(
            before, after,
            "a bare relative db path must resolve to ONE instance id before and \
             after the file is created; got {before} then {after}"
        );

        // A relative path with a `..` component, where the parent chain does
        // NOT resolve — `missing/` is never created. This is what reaches
        // `lexically_normalize`.
        //
        // Creating `sub/` first (as this test used to) means
        // `canonicalize("sub/..")` SUCCEEDS, so the pre-existing parent branch
        // handles the path and the new relative branch is never entered. That
        // version passed with `lexically_normalize` replaced by a plain
        // `cwd.join`, i.e. with the bug it was written to catch still present.
        let dotted = Path::new("missing/../nested.lbug");
        let before_dotted = instance_id_from_db_path(dotted);
        std::fs::write("nested.lbug", b"x").unwrap();
        let after_dotted = instance_id_from_db_path(dotted);
        assert_eq!(
            before_dotted, after_dotted,
            "a relative db path containing `..` must also resolve to one id; \
             got {before_dotted} then {after_dotted}"
        );
        // …and to the SAME id as the path it normalizes to, so the `..` form
        // and the plain form are one instance rather than two.
        assert_eq!(
            before_dotted,
            instance_id_from_db_path(Path::new("nested.lbug")),
            "`missing/../nested.lbug` and `nested.lbug` name the same database, \
             so they must not fork the daemon into two instances"
        );
    }

    /// The LEGACY hash must keep reproducing what a previous release wrote,
    /// including for a bare relative path — otherwise `stop_legacy_hash_daemon`
    /// looks for an orphan under an id it never registered under, and the
    /// orphan keeps the write lock. Pins that the nw-208 fix did NOT leak into
    /// the historical algorithm.
    #[test]
    fn legacy_hash_still_reproduces_the_relative_form_it_must_clean_up() {
        struct CwdGuard(PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }

        let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let _cwd = CwdGuard(std::env::current_dir().unwrap());
        std::env::set_current_dir(dir.path()).unwrap();

        let bare = Path::new("nestweaver.lbug");
        // No file on disk: canonicalize fails, which is the only state where
        // the legacy and current canonicalizations differ.
        //
        // The assertion that MATTERS is that the legacy hash still folds the
        // UN-JOINED relative path. Comparing it against
        // `instance_id_from_db_path` proves nothing — legacy is DefaultHasher
        // (SipHash) masked to 32 bits and current is the first four bytes of
        // SHA-256, so the two can never agree no matter which canonicalization
        // legacy uses. That test passed with `legacy_canonical_db_path`
        // swapped for `canonical_db_path`, i.e. with the regression it was
        // written to prevent fully reintroduced.
        //
        // Joining against the cwd is exactly what `canonical_db_path` does to
        // a relative path, so the legacy id must NOT match the id of the
        // joined form.
        let joined = std::env::current_dir().unwrap().join(bare);
        assert_ne!(
            legacy_instance_id_from_db_path(bare),
            legacy_instance_id_from_db_path(&joined),
            "the legacy hash must fold the UN-JOINED relative path; if these \
             agree it has started resolving against the cwd, and
             `stop_legacy_hash_daemon` will look for an orphan under an id it \
             never registered under"
        );
        assert_eq!(
            legacy_instance_id_from_db_path(bare),
            legacy_instance_id_from_db_path(bare),
            "the legacy hash must be stable"
        );
    }

    #[test]
    fn daemon_identity_survives_a_publication_cutover() {
        // nw-145: identity used to be derived from the selected path, so a
        // cutover renamed the daemon. `daemon status`/`daemon stop --db
        // <base>` then computed an id no running daemon had ever bound, and
        // reported "Daemon is not running" while one was demonstrably
        // serving the brain — the tool's own recovery advice was a no-op.
        let base = Path::new("/data/brain.lbug");
        let slot_graph = |uuid: &str| {
            PathBuf::from(format!(
                "/data/brain.lbug.publications/slots/{uuid}/graph.lbug"
            ))
        };
        let before = slot_graph("550e8400-e29b-41d4-a716-446655440000");
        let after = slot_graph("6ba7b810-9dad-11d1-80b4-00c04fd430c8");

        let base_id = instance_id_from_db_path(base);
        assert_eq!(instance_id_from_db_path(&before), base_id);
        assert_eq!(
            instance_id_from_db_path(&after),
            base_id,
            "a cutover to a new slot must not rename the daemon"
        );

        // nw-205: the two `socket_path` calls below each resolve the XDG roots
        // independently, so a sibling test swapping XDG_STATE_HOME /
        // XDG_RUNTIME_DIR between them makes the SAME instance id render under
        // two different roots and this assertion fail — the observed symptom
        // being `~/.local/state/...` on one side and a test tempdir on the
        // other. `instance_id_from_db_path` is a pure hash of the path and
        // needs no lock; only the path rendering reads the environment. Same
        // defect and same fix as `log_hint_names_the_dated_tracing_file_not_just_stderr`
        // below: the writers already take ENV_LOCK, this reader did not.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());

        // The same anchoring applies to every derived local-state path.
        assert_eq!(
            socket_path(&instance_id_from_db_path(&after)),
            socket_path(&base_id)
        );
        assert_eq!(
            database_path_fingerprint(&after),
            database_path_fingerprint(base)
        );

        // A different brain is still a different daemon.
        assert_ne!(
            instance_id_from_db_path(Path::new("/data/other.lbug")),
            base_id
        );
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

    /// Review finding: the previous test only proved the two hashes differ,
    /// which does not show that a migration actually retires anything. This
    /// exercises the shared runtime path — the same one both migrations use —
    /// against seeded artifacts.
    #[test]
    fn retiring_a_superseded_identity_removes_its_runtime_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let old_id = "aaaabbbb";
            let new_id = "ccccdddd";
            let binding =
                EffectiveConfigBinding::new(7, EffectiveConfigBindingSource::CompiledDefaults);
            write_effective_config_binding(old_id, &binding).unwrap();
            let binding_path = effective_config_binding_path(old_id);
            assert!(binding_path.exists(), "fixture must seed the old binding");

            retire_daemon_under_old_identity(old_id, new_id, "test migration").unwrap();

            assert!(
                !binding_path.exists(),
                "the superseded identity's config binding must be removed"
            );
            assert!(
                !runtime_dir(old_id).exists(),
                "the superseded identity's runtime dir must be removed"
            );
        });
    }

    /// Coinciding ids mean there is nothing to migrate, and the helper must
    /// not touch a HEALTHY daemon's artifacts.
    #[test]
    fn retiring_is_a_no_op_when_the_identity_did_not_change() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let id = "eeeeffff";
            let binding =
                EffectiveConfigBinding::new(7, EffectiveConfigBindingSource::CompiledDefaults);
            write_effective_config_binding(id, &binding).unwrap();
            let binding_path = effective_config_binding_path(id);

            retire_daemon_under_old_identity(id, id, "same identity").unwrap();

            assert!(
                binding_path.exists(),
                "an unchanged identity must not have its live artifacts reaped"
            );
        });
    }

    /// `gc` identifies a daemon's database from its boot line, and TWO writers
    /// record that boot in DIFFERENT formats: stderr (captured by launchd into
    /// the undated `daemon.log`) and the tracing subscriber (into
    /// `daemon.log.<date>`). Only the stderr form was parsed, so a daemon with
    /// no launchd stderr sink — on macOS, every daemon started without a plist
    /// — was reported "not identifiable" and never reached the write-lock check
    /// that spares a LIVE daemon.
    #[test]
    fn a_daemon_boot_line_is_read_in_either_format() {
        // stderr / launchd form.
        assert_eq!(
            db_path_from_daemon_log("[daemon] starting for /data/brain.lbug (instance abc)\n"),
            Some(PathBuf::from("/data/brain.lbug"))
        );
        // tracing / rolling-appender form.
        assert_eq!(
            db_path_from_daemon_log(
                "2026-08-21T05:25:56.892743Z  INFO nestweaver_daemon::server: \
                 daemon process starting db=/data/brain.lbug instance=gcenv-44a308df\n"
            ),
            Some(PathBuf::from("/data/brain.lbug"))
        );
        // A path containing the suffix must still resolve: both formats anchor
        // on the LAST occurrence.
        assert_eq!(
            db_path_from_daemon_log(
                "... daemon process starting db=/data/weird instance=x/brain.lbug instance=abc\n"
            ),
            Some(PathBuf::from("/data/weird instance=x/brain.lbug"))
        );
        // Unrelated log content names no database.
        assert_eq!(db_path_from_daemon_log("nothing to see here\n"), None);
        assert_eq!(db_path_from_daemon_log(""), None);
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
        // `log_hint` and `log_dir` each resolve XDG_STATE_HOME independently,
        // so without ENV_LOCK a sibling test swapping that var between the two
        // calls makes them disagree and this assertion fail — on either root,
        // depending on which way the swap lands. Every other test in this
        // module that touches the XDG roots already takes the lock; this one
        // read process-global state without it.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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

    /// Write a runtime directory as a real daemon boot would leave it
    /// (pidfile + a leftover spawnlock), returning its path.
    fn seed_runtime_dir(instance: &str) -> PathBuf {
        let dir = runtime_dir(instance);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("daemon.pid"), b"12345\n").unwrap();
        std::fs::write(dir.join("daemon.spawnlock"), b"").unwrap();
        dir
    }

    /// Write a socket-fallback directory as the sun_path fallback would leave
    /// it (the socket file itself is unlinked at shutdown; the dir survives).
    fn seed_fallback_dir(fallback_root: &Path, instance: &str) -> PathBuf {
        let dir = fallback_root.join(instance);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Sweep roots agreeing with the `with_xdg_state_and_runtime` overrides,
    /// with a SCRATCH socket-fallback root. Tests must never run the sweep
    /// against the operator's real `/tmp/nw-sock-<uid>`.
    /// A path that really exists and is provably outside every temp prefix
    /// [`is_temp_db_path`] recognises, for tests that need a stand-in "real
    /// database" the sweep must keep.
    ///
    /// `CARGO_MANIFEST_DIR` is NOT safe for this: a git worktree checked out
    /// under /tmp — routine when reviewing a branch — makes the manifest
    /// directory itself match `is_temp_db_path`, so the guard assertion failed
    /// for a reason that had nothing to do with the behaviour under test
    /// (nw-194). The sweep only calls `.exists()`, so any stable system file
    /// serves.
    fn stable_non_temp_path() -> PathBuf {
        ["/bin/sh", "/usr/bin/env", "/etc/hosts"]
            .into_iter()
            .map(PathBuf::from)
            .find(|candidate| candidate.exists() && !is_temp_db_path(candidate))
            .expect("no stable non-temp system file found")
    }

    fn scratch_gc_roots(state: &Path, runtime: &Path, fallback: &Path) -> DaemonGcRoots {
        DaemonGcRoots {
            state: state.join("nestweaver"),
            runtime: Some(runtime.join("nestweaver")),
            socket_fallback: fallback.to_path_buf(),
        }
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
        let fallback = tempfile::tempdir().unwrap();
        with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            let db = PathBuf::from("/tmp/gone-forever-12345/test.lbug");
            let intent_dir = last_successful_config_dir(&db);
            std::fs::create_dir_all(&intent_dir).unwrap();
            let intent_file = intent_dir.join("last-successful-config.json");
            std::fs::write(&intent_file, r#"{"version":1}"#).unwrap();

            // An unambiguous orphan sits beside it so the sweep is not a no-op:
            // a test that deletes nothing would "protect" config-intent trivially.
            seed_state_dir("05c1b2b6", "/tmp/gone-forever-12345/test.lbug");

            let report = gc_orphaned_daemon_dirs_in(&scratch_gc_roots(
                state.path(),
                runtime.path(),
                fallback.path(),
            ))
            .unwrap();

            assert_eq!(
                report.removed,
                vec![(GcRoot::PersistentState, "05c1b2b6".to_string())]
            );
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

    /// THE acceptance test for the three-root sweep: orphans are reclaimed
    /// under persistent state, `$XDG_RUNTIME_DIR`, AND the /tmp socket
    /// fallback — including an instance's `daemon.spawnlock` — while live
    /// databases and unidentifiable instances keep every directory they have.
    #[test]
    fn gc_reaps_orphans_under_all_three_roots_keeps_live_and_unidentified() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            // Reaped from ALL THREE roots: database path is gone. This is the
            // exact shape that accumulated 2,695 runtime dirs / 1,682
            // spawnlocks / 264 fallback dirs on the maintainer's machine.
            seed_state_dir("aaaaaaaa", "/nonexistent-root-98765/brain.lbug");
            let rt_a = seed_runtime_dir("aaaaaaaa");
            let fb_a = seed_fallback_dir(fallback.path(), "aaaaaaaa");
            // Reaped: temp database, whether or not it still exists.
            seed_state_dir("bbbbbbbb", "/tmp/ephemeral-54321/test.lbug");
            let rt_b = seed_runtime_dir("bbbbbbbb");
            let fb_b = seed_fallback_dir(fallback.path(), "bbbbbbbb");
            // Kept: the database still exists at a NON-temp path. It must be
            // outside any tempdir — a `tempfile::tempdir()` lives under /tmp, so
            // the temp rule would reap it and this case would prove nothing.
            // The sweep only calls `.exists()`, so any stable real file serves.
            let live_db = stable_non_temp_path();
            seed_state_dir("cccccccc", live_db.to_str().unwrap());
            let rt_c = seed_runtime_dir("cccccccc");
            let fb_c = seed_fallback_dir(fallback.path(), "cccccccc");
            // Kept and REPORTED: no boot line, so the database cannot be
            // named — in EVERY root it occupies. A runtime/fallback entry
            // whose state dir (and daemon.log) was already swept is exactly
            // this case: unidentifiable means undeletable, forever.
            let unknown = log_dir("dddddddd");
            std::fs::create_dir_all(&unknown).unwrap();
            std::fs::write(unknown.join("daemon.log"), "rotated away\n").unwrap();
            let rt_d = seed_runtime_dir("dddddddd");
            let fb_d = seed_fallback_dir(fallback.path(), "dddddddd");

            let report = gc_orphaned_daemon_dirs_in(&scratch_gc_roots(
                state.path(),
                runtime.path(),
                fallback.path(),
            ))
            .unwrap();

            assert_eq!(
                report.removed,
                // Sorted: root kind first (declaration order), then instance.
                vec![
                    (GcRoot::PersistentState, "aaaaaaaa".to_string()),
                    (GcRoot::PersistentState, "bbbbbbbb".to_string()),
                    (GcRoot::RuntimeDir, "aaaaaaaa".to_string()),
                    (GcRoot::RuntimeDir, "bbbbbbbb".to_string()),
                    (GcRoot::SocketFallback, "aaaaaaaa".to_string()),
                    (GcRoot::SocketFallback, "bbbbbbbb".to_string()),
                ]
            );
            assert_eq!(report.kept, vec!["cccccccc".to_string()]);
            assert_eq!(report.unidentified, vec!["dddddddd".to_string()]);
            assert!(
                report.reclaimed_bytes > 0,
                "reclaimed bytes must be reported"
            );
            assert!(!report.socket_fallback_root_untrusted);
            for gone in [
                log_dir("aaaaaaaa"),
                rt_a,
                fb_a,
                log_dir("bbbbbbbb"),
                rt_b,
                fb_b,
            ] {
                assert!(!gone.exists(), "{} must be reclaimed", gone.display());
            }
            for kept_dir in [log_dir("cccccccc"), rt_c, fb_c] {
                assert!(
                    kept_dir.exists(),
                    "a live database keeps its dirs: {}",
                    kept_dir.display()
                );
            }
            for unknown_dir in [unknown, rt_d, fb_d] {
                assert!(
                    unknown_dir.exists(),
                    "unidentifiable means undeletable: {}",
                    unknown_dir.display()
                );
            }
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
        let fallback = tempfile::tempdir().unwrap();
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

            let report = gc_orphaned_daemon_dirs_in(&scratch_gc_roots(
                state.path(),
                runtime.path(),
                fallback.path(),
            ))
            .unwrap();

            assert_eq!(report.spared, vec!["eeeeeeee".to_string()]);
            assert_eq!(report.spared_pidfile_lock, vec!["eeeeeeee".to_string()]);
            assert!(
                report.spared_database_write_lock.is_empty(),
                "the database path is gone, so the database probe must claim nothing"
            );
            assert!(report.spared_database_unreadable.is_empty());
            assert!(report.removed.is_empty());
            assert!(log_dir("eeeeeeee").exists());

            unsafe {
                libc::flock(held.as_raw_fd(), libc::LOCK_UN);
            }
        });
    }

    /// THE gap, at unit scale. An operator removes `daemon.pid` under a live
    /// daemon serving a temp database — the shape `gc` exists to reap. The
    /// pidfile flock is gone with the inode, so the pidfile-only test says
    /// "orphan" and the sweep deletes a LIVE instance's logs. The database
    /// write lock still names the holder, and must veto the deletion — under
    /// EVERY root, including the /tmp socket fallback, where deleting a live
    /// daemon's fallback socket would cut off its clients.
    #[cfg(unix)]
    #[test]
    fn gc_state_dirs_spares_a_live_database_holder_after_the_pidfile_is_removed() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        // Under /tmp, so the temp-database rule makes this a reap candidate —
        // exactly the class `gc` sweeps by the thousand.
        let db = db_dir.path().join("brain.lbug");
        std::fs::write(&db, b"not really a database").unwrap();
        assert!(is_temp_db_path(&db));

        let holder = fork_write_lock_holder(&db);
        let report = with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            seed_state_dir("ffffffff", db.to_str().unwrap());
            // A live fallback socket under the scratch fallback root.
            let fb = seed_fallback_dir(fallback.path(), "ffffffff");
            std::fs::write(fb.join("daemon.sock"), b"").unwrap();
            // No pidfile at all: the strongest form of the forgery, and what an
            // operator's `rm -f daemon.pid` leaves behind.
            assert!(!pidfile_path("ffffffff").exists());
            let report = gc_orphaned_daemon_dirs_in(&scratch_gc_roots(
                state.path(),
                runtime.path(),
                fallback.path(),
            ))
            .unwrap();
            assert!(
                log_dir("ffffffff").exists(),
                "a live database holder must keep its logs"
            );
            assert!(
                fb.join("daemon.sock").exists(),
                "a live database holder must keep its fallback socket"
            );
            report
        });
        reap(holder);

        assert_eq!(report.spared, vec!["ffffffff".to_string()]);
        assert_eq!(
            report.spared_database_write_lock,
            vec![("ffffffff".to_string(), Some(holder))],
            "the kernel must name the holder, and the report must carry the PID"
        );
        assert!(
            report.spared_pidfile_lock.is_empty(),
            "there is no pidfile, so no pidfile proof may be claimed"
        );
        assert!(report.spared_database_unreadable.is_empty());
        assert!(report.removed.is_empty());
    }

    /// The DEFAULT healthy case: a daemon holding both its database write lock
    /// and its pidfile flock. Both facts are true and both must be reported.
    /// Returning the first proof found and stopping made `gc` print that the
    /// pidfile evidence was "absent or unreadable" about a daemon whose pidfile
    /// was present and locked — a false statement in the normal case, which is
    /// worse than the silence it replaced.
    #[cfg(unix)]
    #[test]
    fn gc_state_dirs_reports_both_proofs_for_a_healthy_instance() {
        use std::os::unix::io::AsRawFd;
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let db = db_dir.path().join("brain.lbug");
        std::fs::write(&db, b"not really a database").unwrap();

        let holder = fork_write_lock_holder(&db);
        let report = with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            seed_state_dir("abcdabcd", db.to_str().unwrap());
            let pidfile = pidfile_path("abcdabcd");
            std::fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
            std::fs::write(&pidfile, format!("{holder}\n")).unwrap();
            let held = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pidfile)
                .unwrap();
            assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
            let report = gc_orphaned_daemon_dirs_in(&scratch_gc_roots(
                state.path(),
                runtime.path(),
                fallback.path(),
            ))
            .unwrap();
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) };
            report
        });
        reap(holder);

        assert_eq!(report.spared, vec!["abcdabcd".to_string()]);
        assert_eq!(
            report.spared_database_write_lock,
            vec![("abcdabcd".to_string(), Some(holder))]
        );
        assert_eq!(
            report.spared_pidfile_lock,
            vec!["abcdabcd".to_string()],
            "an intact, flocked pidfile must be reported as the real evidence it is"
        );
        assert!(report.spared_database_unreadable.is_empty());
        assert!(report.removed.is_empty());
    }

    /// The evidence layers are independent: the pidfile flock still spares an
    /// instance whose database path is gone (an unmounted volume), where the
    /// path-based database probe can say nothing.
    #[cfg(unix)]
    #[test]
    fn state_dir_ownership_falls_back_to_the_pidfile_lock_when_the_database_is_gone() {
        use std::os::unix::io::AsRawFd;
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            let missing = PathBuf::from("/nonexistent-root-13579/brain.lbug");
            assert_eq!(db_write_lock(&missing), DbWriteLock::Free);
            let pidfile = pidfile_path("aaaabbbb");
            let unowned = state_dir_ownership(&pidfile, &missing);
            assert!(!unowned.is_owned());
            assert_eq!(unowned.database, DbWriteLock::Free);
            assert!(!unowned.pidfile_lock_held);

            std::fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
            std::fs::write(&pidfile, "12345\n").unwrap();
            let held = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pidfile)
                .unwrap();
            assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);

            let owned = state_dir_ownership(&pidfile, &missing);
            assert!(owned.is_owned());
            assert_eq!(
                owned.database,
                DbWriteLock::Free,
                "a missing database path must not be reported as held"
            );
            assert!(owned.pidfile_lock_held);

            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) };
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

    /// The kernel names the process on the other end of a unix socket — the
    /// unlink-proof evidence that backs both the socket-cleanup guard and the
    /// pidfile-less daemon adoption.
    #[cfg(target_os = "linux")]
    #[test]
    fn unix_socket_peer_cred_reports_pid_and_uid() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("peer.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();

        assert_eq!(
            unix_socket_peer_pid(&stream),
            Some(std::process::id() as i32),
            "SO_PEERCRED must name this process, the listener's owner"
        );
        assert_eq!(
            unix_socket_peer_cred(&stream),
            Some((std::process::id() as i32, unsafe { libc::geteuid() })),
            "the cred variant must report the same PID plus the peer's uid"
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

    /// The self-probe guard must work in RELEASE builds, so it is a runtime
    /// branch rather than a `debug_assert` (this workspace sets no
    /// `[profile.release] debug-assertions` override, so an assertion would
    /// vanish from every shipped binary). Probing while holding the store open
    /// must fail closed, never report `Free`.
    ///
    /// Tested by VALUE through `db_write_lock_probe`. Setting the real
    /// `LOCAL_STORE_WRITE_LOCK_HELD` flag here would make every sibling test
    /// that probes a lock fail intermittently — they run on parallel threads
    /// and take no shared lock — which is exactly what the first version of
    /// this test did.
    #[test]
    fn write_lock_probe_refuses_to_run_against_its_own_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"not really a database").unwrap();

        assert_eq!(db_write_lock_probe(false, &db), DbWriteLock::Free);

        let guarded = db_write_lock_probe(true, &db);
        assert_eq!(
            guarded,
            DbWriteLock::Unknown,
            "a probe from a store-holding process must fail closed, not report Free"
        );
        assert!(!guarded.is_provably_free());
        assert!(!guarded.is_held());
    }

    /// The entry point agrees with the probe in the DISARMED state, and no
    /// test leaves the process-global flag armed for its neighbours.
    ///
    /// Honest scope: this does NOT pin the wiring. Because the global is
    /// already `false` here, it would still pass if someone hardcoded
    /// `db_write_lock_probe(false, db_path)` — which is precisely the rot that
    /// disables the guard. Pinning that direction requires observing the
    /// entry point with the global ARMED, and arming a process-global inside a
    /// parallel test binary is the exact race removed above (2 failures in 60
    /// runs at `--test-threads=8`). Doing it properly needs a single-test
    /// integration binary where nothing else runs concurrently; until then the
    /// armed direction is covered only by
    /// `write_lock_probe_refuses_to_run_against_its_own_store`, which tests the
    /// inner function by value.
    #[test]
    fn write_lock_probe_entry_point_consults_the_local_store_flag() {
        assert!(
            !local_store_write_lock_held(),
            "no test may leave this process-global flag armed"
        );
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"not really a database").unwrap();
        assert_eq!(db_write_lock(&db), db_write_lock_probe(false, &db));
    }

    /// The canonical store-level authority owns a POSIX compatibility lock as
    /// well as its stable sidecar. Its per-database self-ownership latch must
    /// therefore guard every lifecycle probe, including indirect callers such
    /// as checkpoint-artifact classification.
    #[test]
    fn write_lock_probe_entry_point_refuses_a_canonical_self_held_lease() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let authority = acquire_db_write_lease(&db).unwrap();

        assert_eq!(db_write_lock(&db), DbWriteLock::Unknown);
        assert!(nestweaver_store::self_holds_write_lease(&db));

        drop(authority);
        assert!(!nestweaver_store::self_holds_write_lease(&db));
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

    /// A fallback root that does not exist is a clean machine, not an error.
    #[cfg(unix)]
    #[test]
    fn gc_fallback_root_absent_is_clean() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        let absent = fallback.path().join("never-created");
        with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            let roots = DaemonGcRoots {
                state: state.path().join("nestweaver"),
                runtime: Some(runtime.path().join("nestweaver")),
                socket_fallback: absent,
            };
            let report = gc_orphaned_daemon_dirs_in(&roots).unwrap();
            assert!(report.removed.is_empty());
            assert!(
                !report.socket_fallback_root_untrusted,
                "an absent fallback root is clean, not untrusted"
            );
        });
    }

    /// The squatting threat model, at unit scale: a fallback root that is not
    /// a real directory owned by the expected uid is NEVER swept — no entry
    /// inside it is even read for deletion. Tested by VALUE with a foreign
    /// expected uid (the same seam `secure_fallback_sock_dir` uses), because
    /// the test process cannot chown.
    #[cfg(unix)]
    #[test]
    fn gc_fallback_root_foreign_or_symlinked_is_never_swept() {
        let mut candidates = Default::default();
        let our_uid = unsafe { libc::geteuid() };

        // A root owned by someone else (simulated: expected uid is not ours).
        let root = tempfile::tempdir().unwrap();
        let instance = seed_fallback_dir(root.path(), "aaaabbbb");
        let trusted =
            collect_fallback_candidates(root.path(), our_uid + 1, &mut candidates).unwrap();
        assert!(!trusted, "a foreign-owned fallback root must be refused");
        assert!(candidates.is_empty());
        assert!(
            instance.exists(),
            "a refused root's contents are left untouched"
        );

        // A symlinked root is never followed out of /tmp either.
        let target = tempfile::tempdir().unwrap();
        let behind_symlink = seed_fallback_dir(target.path(), "aaaabbbb");
        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("nw-sock-link");
        std::os::unix::fs::symlink(target.path(), &link).unwrap();
        let trusted = collect_fallback_candidates(&link, our_uid, &mut candidates).unwrap();
        assert!(!trusted, "a symlinked fallback root must be refused");
        assert!(candidates.is_empty());
        assert!(behind_symlink.exists());
    }

    /// A squatted fallback root skips ONLY that root: the state and runtime
    /// sweeps still run, and the report says the fallback was not checked.
    #[cfg(unix)]
    #[test]
    fn gc_reports_and_skips_a_squatted_fallback_root() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        // A symlink stands in for the squat (cannot chown in a test).
        let target = tempfile::tempdir().unwrap();
        let behind_symlink = seed_fallback_dir(target.path(), "aaaabbbb");
        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("fallback");
        std::os::unix::fs::symlink(target.path(), &link).unwrap();
        with_xdg_state_and_runtime(state.path(), runtime.path(), || {
            seed_state_dir("aaaabbbb", "/nonexistent-root-98765/brain.lbug");
            let rt = seed_runtime_dir("aaaabbbb");
            let roots = DaemonGcRoots {
                state: state.path().join("nestweaver"),
                runtime: Some(runtime.path().join("nestweaver")),
                socket_fallback: link,
            };

            let report = gc_orphaned_daemon_dirs_in(&roots).unwrap();

            assert!(report.socket_fallback_root_untrusted);
            assert_eq!(
                report.removed,
                vec![
                    (GcRoot::PersistentState, "aaaabbbb".to_string()),
                    (GcRoot::RuntimeDir, "aaaabbbb".to_string()),
                ]
            );
            assert!(!rt.exists());
            assert!(
                behind_symlink.exists(),
                "the untrusted root's contents must survive"
            );
        });
    }

    /// `NESTWEAVER_SOCK_FALLBACK_DIR` relocates the whole fallback root, so
    /// tests (and only tests) keep the sweep off the operator's real
    /// `/tmp/nw-sock-<uid>`.
    #[test]
    fn socket_fallback_root_honors_the_env_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("NESTWEAVER_SOCK_FALLBACK_DIR");
        unsafe {
            std::env::set_var("NESTWEAVER_SOCK_FALLBACK_DIR", "/scratch/nw-sock-test");
        }
        let overridden = socket_fallback_root();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("NESTWEAVER_SOCK_FALLBACK_DIR", value),
                None => std::env::remove_var("NESTWEAVER_SOCK_FALLBACK_DIR"),
            }
        }
        assert_eq!(overridden, PathBuf::from("/scratch/nw-sock-test"));
    }

    /// Set all three env roots under one ENV_LOCK acquisition (the lock is a
    /// plain std Mutex and is not reentrant — no nesting).
    fn with_state_runtime_and_fallback<T>(
        state: &Path,
        runtime: &Path,
        fallback: &Path,
        test: impl FnOnce() -> T,
    ) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous_state = std::env::var_os("XDG_STATE_HOME");
        let previous_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        let previous_fallback = std::env::var_os("NESTWEAVER_SOCK_FALLBACK_DIR");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", state);
            std::env::set_var("XDG_RUNTIME_DIR", runtime);
            std::env::set_var("NESTWEAVER_SOCK_FALLBACK_DIR", fallback);
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
            match previous_fallback {
                Some(value) => std::env::set_var("NESTWEAVER_SOCK_FALLBACK_DIR", value),
                None => std::env::remove_var("NESTWEAVER_SOCK_FALLBACK_DIR"),
            }
        }
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// A clean shutdown of a TEMP-database daemon unlinks all three of its
    /// per-instance directories, so counts stop growing between sweeps.
    #[test]
    fn temp_db_shutdown_unlinks_state_runtime_and_fallback_dirs() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        with_state_runtime_and_fallback(state.path(), runtime.path(), fallback.path(), || {
            let db = db_dir.path().join("brain.lbug");
            std::fs::write(&db, b"not really a database").unwrap();
            assert!(is_temp_db_path(&db));
            seed_state_dir("aaaabbbb", db.to_str().unwrap());
            let rt = seed_runtime_dir("aaaabbbb");
            let fb = seed_fallback_dir(fallback.path(), "aaaabbbb");

            remove_instance_dirs_for_temp_db(&db, "aaaabbbb");

            assert!(!log_dir("aaaabbbb").exists(), "state dir must go");
            assert!(!rt.exists(), "runtime dir (with spawnlock) must go");
            assert!(!fb.exists(), "socket-fallback dir must go");
            // The fallback ROOT is not the daemon's to remove.
            assert!(fallback.path().exists());
        });
    }

    /// The spawnlock veto: a client mid-respawn holds `daemon.spawnlock`
    /// across the old daemon's shutdown and hands the locked file to the
    /// child, which refuses to start when the path it inherited is gone
    /// ("inherited parent spawnlock cannot be matched" — the failure the
    /// pre-change restart test produced). A held spawnlock must pass every
    /// directory to the successor untouched.
    #[cfg(unix)]
    #[test]
    fn temp_db_shutdown_keeps_every_directory_while_the_spawnlock_is_held() {
        use std::os::unix::io::AsRawFd;
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        with_state_runtime_and_fallback(state.path(), runtime.path(), fallback.path(), || {
            let db = db_dir.path().join("brain.lbug");
            std::fs::write(&db, b"not really a database").unwrap();
            assert!(is_temp_db_path(&db));
            seed_state_dir("bbbbcccc", db.to_str().unwrap());
            let rt = seed_runtime_dir("bbbbcccc");
            let fb = seed_fallback_dir(fallback.path(), "bbbbcccc");
            // Hold the spawnlock the way a respawning client does.
            let spawnlock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(rt.join("daemon.spawnlock"))
                .unwrap();
            assert_eq!(
                unsafe { libc::flock(spawnlock.as_raw_fd(), libc::LOCK_EX) },
                0
            );

            remove_instance_dirs_for_temp_db(&db, "bbbbcccc");

            assert!(
                log_dir("bbbbcccc").exists(),
                "state dir passes to the successor"
            );
            assert!(rt.exists(), "runtime dir passes to the successor");
            assert!(fb.exists(), "fallback dir passes to the successor");

            unsafe { libc::flock(spawnlock.as_raw_fd(), libc::LOCK_UN) };
        });
    }

    /// The gate: a daemon serving a REAL database must never delete its state
    /// dir on shutdown. Same predicate the sweep and launchd use.
    #[test]
    fn real_db_shutdown_keeps_every_directory() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        with_state_runtime_and_fallback(state.path(), runtime.path(), fallback.path(), || {
            // Any stable path outside the temp roots serves as a real database.
            let db = stable_non_temp_path();
            seed_state_dir("ccccdddd", db.to_str().unwrap());
            let rt = seed_runtime_dir("ccccdddd");
            let fb = seed_fallback_dir(fallback.path(), "ccccdddd");

            remove_instance_dirs_for_temp_db(&db, "ccccdddd");

            assert!(log_dir("ccccdddd").exists());
            assert!(rt.exists());
            assert!(fb.exists());
        });
    }

    /// nw-245: `EFFECTIVE_CONFIG_BINDING_MAX_BYTES` is `64 * 1024`. Turning
    /// that `*` into a `+` yields 1088 — still large enough for every small
    /// record the other tests write, so nothing noticed. Pin the ceiling from
    /// both sides at literal sizes chosen so multiplication and addition
    /// disagree: an 8 KiB record must be ACCEPTED (1088 would reject it), and
    /// an oversize record must report the real 65536 limit back to the caller.
    #[test]
    fn effective_config_binding_ceiling_accepts_eight_kib_and_reports_sixty_four_kib() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_runtime(temp.path(), || {
            let binding =
                EffectiveConfigBinding::new(4242, EffectiveConfigBindingSource::CompiledDefaults);
            write_effective_config_binding("padded", &binding).unwrap();
            let path = effective_config_binding_path("padded");

            // Trailing whitespace keeps the payload valid JSON, so the only
            // thing 8 KiB changes is whether the size gate lets it through.
            let mut bytes = serde_json::to_vec(&binding).unwrap();
            assert!(
                bytes.len() < 8192,
                "the unpadded record must be smaller than the padding target"
            );
            bytes.resize(8192, b' ');
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);

            let read_back = read_effective_config_binding("padded").unwrap_or_else(|error| {
                panic!(
                    "an 8192-byte binding is under the 64 KiB ceiling and must be read back, \
                     got {error}"
                )
            });
            assert_eq!(read_back, binding);

            // The ceiling the reader hands back must be the real one. A `+`
            // mutant would surface 1088 here.
            std::fs::write(&path, vec![b' '; 70_000]).unwrap();
            match read_effective_config_binding("padded") {
                Err(EffectiveConfigBindingError::TooLarge { size, max, .. }) => {
                    assert_eq!(size, 70_000);
                    assert_eq!(max, 65_536, "the binding ceiling must be 64 * 1024");
                }
                other => panic!("expected TooLarge for a 70000-byte binding, got {other:?}"),
            }
        });
    }

    /// nw-245: same arithmetic mutant on `LAST_SUCCESSFUL_CONFIG_MAX_BYTES`.
    /// `64 + 1024` = 1088 still round-trips the tiny records the existing
    /// tests write, and the existing oversize test sizes its payload FROM the
    /// constant, so it moves with the mutation. Use literal sizes instead.
    #[test]
    fn last_successful_config_ceiling_accepts_eight_kib_and_reports_sixty_four_kib() {
        let temp = tempfile::tempdir().unwrap();
        with_xdg_state(temp.path(), || {
            let db = temp.path().join("brain.lbug");
            let config = temp.path().join("instance.toml");
            std::fs::write(&config, "instance_id = \"test\"\n").unwrap();
            let record = write_last_successful_config(&db, &config).unwrap();
            let path = last_successful_config_path(&db);

            let mut bytes = serde_json::to_vec(&record).unwrap();
            assert!(
                bytes.len() < 8192,
                "the unpadded record must be smaller than the padding target"
            );
            bytes.resize(8192, b' ');
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);

            let read_back = read_last_successful_config(&db).unwrap_or_else(|error| {
                panic!(
                    "an 8192-byte record is under the 64 KiB ceiling and must be read back, \
                     got {error}"
                )
            });
            assert_eq!(read_back, record);

            std::fs::write(&path, vec![b' '; 70_000]).unwrap();
            match read_last_successful_config(&db) {
                Err(LastSuccessfulConfigError::TooLarge { size, max, .. }) => {
                    assert_eq!(size, 70_000);
                    assert_eq!(max, 65_536, "the record ceiling must be 64 * 1024");
                }
                other => panic!("expected TooLarge for a 70000-byte record, got {other:?}"),
            }
        });
    }

    /// nw-245: `mark_verified_nestweaver_managed_start`,
    /// `clear_verified_nestweaver_managed_start`, and
    /// `is_verified_nestweaver_managed_start` are the whole gate behind the
    /// `DaemonLifecycleOwner` a daemon publishes (`server.rs`). Nothing
    /// observed the marker, so gutting `mark`/`clear` into `()` — or pinning
    /// the reader to `false`, which downgrades EVERY daemon to
    /// `ExternalOrUnknown` — changed real behaviour with the suite still
    /// green. Observe the effect: set, read back, clear, read back.
    ///
    /// Holds `ENV_LOCK` because the marker is process-global and `server.rs`'s
    /// in-process daemon e2e tests read it while they run under the same lock.
    #[test]
    fn verified_nestweaver_managed_start_marker_sets_clears_and_reads_back() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = is_verified_nestweaver_managed_start();

        clear_verified_nestweaver_managed_start();
        assert!(
            !is_verified_nestweaver_managed_start(),
            "an unmarked process must not claim a NestWeaver-managed start"
        );

        mark_verified_nestweaver_managed_start();
        assert!(
            is_verified_nestweaver_managed_start(),
            "marking the managed start must be observable; otherwise every daemon \
             publishes DaemonLifecycleOwner::ExternalOrUnknown"
        );

        // A second mark must stay idempotent rather than toggling.
        mark_verified_nestweaver_managed_start();
        assert!(is_verified_nestweaver_managed_start());

        clear_verified_nestweaver_managed_start();
        assert!(
            !is_verified_nestweaver_managed_start(),
            "clearing the marker must be observable; otherwise a failed spawn \
             leaves the daemon falsely claiming NestWeaver management"
        );

        if previous {
            mark_verified_nestweaver_managed_start();
        }
    }
}
