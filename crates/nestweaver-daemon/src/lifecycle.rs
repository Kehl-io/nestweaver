//! Utility functions for daemon runtime paths (socket, pidfile, logs).

use std::path::{Path, PathBuf};

/// Derive a stable, short instance ID from a database path.
///
/// Uses an 8-character hex hash of the canonical path to avoid:
/// - Collisions when two DBs share the same parent directory name
/// - macOS 104-byte `sun_path` limit for Unix domain sockets
///
/// Falls back to the parent directory name if available, for human
/// readability in log paths and status output.
pub fn instance_id_from_db_path(db_path: &Path) -> String {
    // Try to canonicalize for a stable hash; fall back to the raw path.
    let canonical = std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());

    // Human-readable prefix from the parent dir name (e.g., "my-brain").
    let prefix = canonical
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nw".to_string());

    // 8-char hex hash of the full canonical path for uniqueness.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash = format!("{:08x}", hasher.finish() & 0xFFFF_FFFF);

    format!("{}-{}", prefix, hash)
}

/// Runtime directory for the daemon socket and pidfile.
///
/// Prefers `$XDG_RUNTIME_DIR/nestweaver-<instance>/`, falling back to
/// `$TMPDIR/nestweaver-<uid>/nestweaver-<instance>/` or
/// `/tmp/nestweaver-<uid>/nestweaver-<instance>/`.
pub fn runtime_dir(instance_id: &str) -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join(format!("nestweaver-{instance_id}"));
    }

    let uid = unsafe { libc::getuid() };
    let base = std::env::var("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));

    base.join(format!("nestweaver-{uid}"))
        .join(format!("nestweaver-{instance_id}"))
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

    #[test]
    fn instance_id_contains_parent_dir_name() {
        let path = Path::new("/home/user/.local/share/nestweaver/my-brain/brain.lbug");
        let id = instance_id_from_db_path(path);
        assert!(
            id.starts_with("my-brain-"),
            "expected 'my-brain-<hash>', got '{id}'"
        );
        assert!(
            id.len() <= 30,
            "instance_id should be short for socket paths"
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
    fn instance_id_fallback() {
        let path = Path::new("brain.lbug");
        let id = instance_id_from_db_path(path);
        assert!(!id.is_empty());
    }

    #[test]
    fn socket_path_is_under_runtime_dir() {
        let sock = socket_path("test");
        assert!(sock.ends_with("daemon.sock"));
        assert!(sock.starts_with(runtime_dir("test")));
    }

    #[test]
    fn log_path_ends_with_daemon_log() {
        let lp = log_path("test");
        assert!(lp.ends_with("daemon.log"));
    }
}
