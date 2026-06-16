//! Utility functions for daemon runtime paths (socket, pidfile, logs).

use std::path::{Path, PathBuf};

/// Derive a stable, short instance ID from a database path.
///
/// Returns ONLY the 8-character hex hash of the canonical path.
/// This keeps socket paths well under the macOS 104-byte `sun_path` limit.
///
/// For a human-readable label (parent-dir + hash), use
/// [`instance_label_from_db_path`] instead.
pub fn instance_id_from_db_path(db_path: &Path) -> String {
    let canonical = std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:08x}", hasher.finish() & 0xFFFF_FFFF)
}

/// Human-readable label for logging: `<parent-dir>-<hash>`.
///
/// Never use this in path construction — use [`instance_id_from_db_path`]
/// (the bare 8-char hash) to keep socket paths short.
pub fn instance_label_from_db_path(db_path: &Path) -> String {
    let canonical = std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
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
/// Prefers `$XDG_RUNTIME_DIR/nestweaver/<instance>/`, falling back to
/// `$TMPDIR/nw-<uid>/<instance>/` or `/tmp/nw-<uid>/<instance>/`.
///
/// The short `nw-` prefix (vs the old `nestweaver-`) is intentional:
/// macOS limits `sun_path` to 104 bytes and `$TMPDIR` can be ~51 chars.
pub fn runtime_dir(instance_id: &str) -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join("nestweaver").join(instance_id);
    }

    let uid = unsafe { libc::getuid() };
    let base = std::env::var("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));

    base.join(format!("nw-{uid}")).join(instance_id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests that mutate environment variables must hold this lock to avoid
    // racing with each other under `cargo test`'s default parallel execution.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn instance_id_is_8_hex_chars() {
        let path = Path::new("/home/user/.local/share/nestweaver/kory-brain/brain.lbug");
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

    #[test]
    fn instance_id_fallback_for_bare_filename() {
        let path = Path::new("brain.lbug");
        let id = instance_id_from_db_path(path);
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn instance_label_includes_parent_dir() {
        let path = Path::new("/home/user/.local/share/nestweaver/kory-brain/brain.lbug");
        let label = instance_label_from_db_path(path);
        assert!(
            label.starts_with("kory-brain-"),
            "label should include parent dir name, got '{label}'"
        );
        assert!(label.len() <= 30, "label should be short, got '{label}'");
    }

    #[test]
    fn socket_path_under_sun_len_with_long_tmpdir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let long_tmpdir = "/var/folders/0h/z2kcwz1j0mld0cbrkt15n7w80000gq/T";
        unsafe {
            std::env::set_var("TMPDIR", long_tmpdir);
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
    fn runtime_dir_uses_xdg_when_set() {
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
