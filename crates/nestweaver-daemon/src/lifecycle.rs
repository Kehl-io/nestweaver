//! Utility functions for daemon runtime paths (socket, pidfile, logs).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const EFFECTIVE_CONFIG_BINDING_VERSION: u32 = 1;
const EFFECTIVE_CONFIG_BINDING_FILE: &str = "effective-config.json";
const EFFECTIVE_CONFIG_BINDING_MAX_BYTES: u64 = 64 * 1024;
static EFFECTIVE_CONFIG_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn effective_config_temp_path(parent: &Path, sequence: u64) -> PathBuf {
    parent.join(format!(
        ".{EFFECTIVE_CONFIG_BINDING_FILE}.{}.{}.tmp",
        std::process::id(),
        sequence
    ))
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

/// Directory for daemon log files.
pub fn log_dir(instance_id: &str) -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local/state")
        })
        .join("nestweaver")
        .join(instance_id)
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
}
