//! Where a daemon instance keeps its socket, pidfile, and lock — the ONE
//! place that decides it (nw-829).
//!
//! The daemon, the CLI/client, and the MCP stdio server must all agree on
//! this location, and they live in different crates (the MCP crate cannot
//! depend on the daemon crate), so the logic lives in this leaf crate.
//!
//! Resolution order for the primary ("caller") directory:
//! 1. `$XDG_RUNTIME_DIR/nestweaver/<instance>` when the variable is set.
//! 2. Linux only: `/run/user/<uid>/nestweaver/<instance>` when that per-user
//!    directory exists, is a real directory (not a symlink), and is owned by
//!    the current user. A daemon started by a systemd *system* unit has no
//!    `XDG_RUNTIME_DIR`, but an interactive CLI of the same user does, and
//!    its value is this directory.
//! 3. `<state dir>/nestweaver/<instance>` (e.g. `~/.local/state/...`).
//!
//! `$TMPDIR` is deliberately never consulted: different launchers see
//! different values, which made two clients disagree about the socket.
//!
//! Clients probe [`runtime_candidate_dirs`]: the primary directory first, then
//! the state directory when it differs. That covers a daemon started before
//! `/run/user/<uid>` existed (or by an older release) and a launchd job on
//! macOS, neither of which sees the caller's `XDG_RUNTIME_DIR`. The daemon
//! itself binds only the primary directory.

use std::path::{Path, PathBuf};

/// Runtime directory for `instance_id` from this process's environment.
pub fn runtime_dir(instance_id: &str) -> PathBuf {
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    runtime_dir_with(xdg.as_deref(), default_user_runtime_base(), instance_id)
}

/// [`runtime_dir`] with the environment made explicit, so the resolution is
/// testable without touching process state or the host's `/run/user`.
pub fn runtime_dir_with(
    xdg_runtime_dir: Option<&str>,
    user_base: Option<PathBuf>,
    instance_id: &str,
) -> PathBuf {
    if let Some(xdg) = xdg_runtime_dir {
        return PathBuf::from(xdg).join("nestweaver").join(instance_id);
    }
    if let Some(base) = user_base {
        return base.join("nestweaver").join(instance_id);
    }
    state_runtime_dir(instance_id)
}

/// Every directory a client may find this instance in, primary first: the
/// resolved runtime dir, then the state dir when it is a different path.
pub fn runtime_candidate_dirs(instance_id: &str) -> Vec<PathBuf> {
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    runtime_candidate_dirs_with(xdg.as_deref(), default_user_runtime_base(), instance_id)
}

/// [`runtime_candidate_dirs`] with the environment made explicit.
pub fn runtime_candidate_dirs_with(
    xdg_runtime_dir: Option<&str>,
    user_base: Option<PathBuf>,
    instance_id: &str,
) -> Vec<PathBuf> {
    let primary = runtime_dir_with(xdg_runtime_dir, user_base, instance_id);
    let legacy = state_runtime_dir(instance_id);
    if legacy == primary {
        vec![primary]
    } else {
        vec![primary, legacy]
    }
}

/// The directory a launchd-supervised daemon binds: launchd does not pass
/// `XDG_RUNTIME_DIR`, so it always resolves to the state directory.
pub fn launchd_runtime_dir(instance_id: &str) -> PathBuf {
    state_runtime_dir(instance_id)
}

/// Stable per-user state directory that does not depend on the caller's env.
pub fn state_runtime_dir(instance_id: &str) -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local/state")
        })
        .join("nestweaver")
        .join(instance_id)
}

/// `/run/user/<uid>` on Linux when this user's runtime directory exists.
pub fn default_user_runtime_base() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let uid = unsafe { libc::getuid() };
        user_runtime_base_in(Path::new("/run/user"), uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `<root>/<uid>` if it is a real directory (not a symlink) owned by `uid`.
pub fn user_runtime_base_in(root: &Path, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dir = root.join(uid.to_string());
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    (meta.is_dir() && meta.uid() == uid).then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_wins_over_the_user_base() {
        let dir = runtime_dir_with(Some("/x/rt"), Some(PathBuf::from("/run/user/7")), "abcd");
        assert_eq!(dir, PathBuf::from("/x/rt/nestweaver/abcd"));
    }

    #[test]
    fn without_xdg_the_user_base_is_used() {
        // nw-829: daemon (no XDG) must land where the CLI (XDG=/run/user/7) looks.
        let daemon = runtime_dir_with(None, Some(PathBuf::from("/run/user/7")), "abcd");
        let cli = runtime_dir_with(
            Some("/run/user/7"),
            Some(PathBuf::from("/run/user/7")),
            "abcd",
        );
        assert_eq!(daemon, cli);
    }

    #[test]
    fn without_xdg_or_user_base_it_is_the_state_dir() {
        assert_eq!(
            runtime_dir_with(None, None, "abcd"),
            state_runtime_dir("abcd")
        );
    }

    #[test]
    fn candidates_are_primary_then_legacy_state_dir_deduped() {
        let id = "cand1234";
        let both = runtime_candidate_dirs_with(None, Some(PathBuf::from("/run/user/7")), id);
        assert_eq!(
            both,
            vec![
                PathBuf::from("/run/user/7/nestweaver/cand1234"),
                state_runtime_dir(id)
            ]
        );
        // Nothing but the state dir available: a single candidate.
        assert_eq!(
            runtime_candidate_dirs_with(None, None, id),
            vec![state_runtime_dir(id)]
        );
        // XDG pointing at the state root collapses to one entry too.
        let state = state_runtime_dir(id);
        let root = state.parent().unwrap().parent().unwrap().to_str().unwrap();
        assert_eq!(
            runtime_candidate_dirs_with(Some(root), None, id),
            vec![state]
        );
    }

    #[test]
    fn launchd_dir_ignores_xdg() {
        assert_eq!(launchd_runtime_dir("abcd"), state_runtime_dir("abcd"));
    }

    #[test]
    fn user_base_requires_an_owned_real_directory() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("nw-rtp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let uid = std::fs::metadata(&root).unwrap().uid();
        assert_eq!(user_runtime_base_in(&root, uid), None, "missing");
        let ours = root.join(uid.to_string());
        std::fs::create_dir(&ours).unwrap();
        assert_eq!(user_runtime_base_in(&root, uid), Some(ours.clone()));
        let other = uid.wrapping_add(1);
        std::fs::create_dir(root.join(other.to_string())).unwrap();
        assert_eq!(user_runtime_base_in(&root, other), None, "foreign uid");
        let linked = uid.wrapping_add(2);
        std::os::unix::fs::symlink(&ours, root.join(linked.to_string())).unwrap();
        assert_eq!(user_runtime_base_in(&root, linked), None, "symlink");
        let _ = std::fs::remove_dir_all(&root);
    }
}
