//! Utility functions for daemon runtime paths (socket, pidfile, logs).

use std::path::{Path, PathBuf};

/// Derive a stable instance ID from a database path.
///
/// Extracts the parent directory name — e.g. `kory-brain` from
/// `~/.local/share/nestweaver/kory-brain/brain.lbug`.
pub fn instance_id_from_db_path(db_path: &Path) -> String {
    db_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string())
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
    fn instance_id_extraction() {
        let path = Path::new("/home/user/.local/share/nestweaver/kory-brain/brain.lbug");
        assert_eq!(instance_id_from_db_path(path), "kory-brain");
    }

    #[test]
    fn instance_id_fallback() {
        let path = Path::new("brain.lbug");
        // No parent directory → falls back to "default"
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
