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
//! the state directory and the owned `/run/user/<uid>` directory, deduped.
//! That covers a daemon started before the user runtime directory existed,
//! an older release or launchd job using state, and a system unit using the
//! user runtime directory while its client has a different `XDG_RUNTIME_DIR`.
//! The daemon itself binds only the primary directory.

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
/// resolved runtime dir, state dir, and owned user runtime dir, deduped.
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
    let primary = runtime_dir_with(xdg_runtime_dir, user_base.clone(), instance_id);
    let legacy = state_runtime_dir(instance_id);
    let user = user_base.map(|base| base.join("nestweaver").join(instance_id));
    let mut dirs = vec![primary];
    for candidate in std::iter::once(legacy).chain(user) {
        if !dirs.contains(&candidate) {
            dirs.push(candidate);
        }
    }
    dirs
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
    fn different_xdg_keeps_the_owned_user_runtime_candidate() {
        let id = "split829";
        let dirs = runtime_candidate_dirs_with(
            Some("/caller-runtime"),
            Some(PathBuf::from("/run/user/7")),
            id,
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/caller-runtime/nestweaver/split829"),
                state_runtime_dir(id),
                PathBuf::from("/run/user/7/nestweaver/split829"),
            ],
            "a caller-specific XDG must not hide the system-unit socket"
        );
    }

    #[test]
    fn user_runtime_candidate_is_deduped_against_primary_and_state() {
        let id = "dedup829";
        let user = PathBuf::from("/run/user/7/nestweaver/dedup829");
        let same_as_primary = runtime_candidate_dirs_with(
            Some("/run/user/7"),
            Some(PathBuf::from("/run/user/7")),
            id,
        );
        assert_eq!(same_as_primary.iter().filter(|p| *p == &user).count(), 1);
        let state = state_runtime_dir(id);
        let state_base = state.parent().unwrap().parent().unwrap().to_path_buf();
        let same_as_state =
            runtime_candidate_dirs_with(Some("/caller-runtime"), Some(state_base), id);
        assert_eq!(same_as_state.iter().filter(|p| *p == &state).count(), 1);
        assert_eq!(same_as_state.len(), 2);
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
