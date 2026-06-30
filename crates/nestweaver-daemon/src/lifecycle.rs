//! Utility functions for daemon runtime paths (socket, pidfile, logs).

use std::path::{Path, PathBuf};

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

/// Path to the Unix domain socket for the given instance.
pub fn socket_path(instance_id: &str) -> PathBuf {
    runtime_dir(instance_id).join("daemon.sock")
}

/// Path to the PID file for the given instance.
pub fn pidfile_path(instance_id: &str) -> PathBuf {
    runtime_dir(instance_id).join("daemon.pid")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests that mutate environment variables must hold this lock to avoid
    // racing with each other under `cargo test`'s default parallel execution.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    #[test]
    fn log_path_ends_with_daemon_log() {
        let lp = log_path("test1234");
        assert!(lp.ends_with("daemon.log"));
    }
}
