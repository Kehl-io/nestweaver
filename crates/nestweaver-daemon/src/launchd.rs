use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::lifecycle;

/// Directory holding per-instance launch agents (`~/Library/LaunchAgents`).
fn launchd_agents_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library")
        .join("LaunchAgents")
}

/// True if `db_path` lives under a temporary directory (`/tmp`, `/private/tmp`,
/// `/var/folders`, or `$TMPDIR`). Daemons for temp DBs are ephemeral (tests,
/// throwaway repros) and must never receive a persistent launchd agent — that
/// was the source of the leaked, crash-looping `io.kehl.nestweaver.*` agents.
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

/// Extract the `--db <path>` value from a nestweaver launchd plist's
/// ProgramArguments. Returns `None` if not present.
pub fn parse_db_path_from_plist(content: &str) -> Option<PathBuf> {
    let mut strings: Vec<&str> = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<string>") {
        let after = &rest[start + "<string>".len()..];
        match after.find("</string>") {
            Some(end) => {
                strings.push(&after[..end]);
                rest = &after[end..];
            }
            None => break,
        }
    }
    let pos = strings.iter().position(|s| *s == "--db")?;
    strings.get(pos + 1).map(|s| PathBuf::from(*s))
}

/// Result of a [`gc_orphaned_agents`] pass.
pub struct GcReport {
    /// Labels whose plist was booted out and deleted.
    pub removed: Vec<String>,
    /// Labels kept (their `--db` path still exists on a real, non-temp path).
    pub kept: Vec<String>,
    /// Labels SPARED despite a missing `--db`: a live daemon still holds the
    /// pidfile flock (e.g. the DB volume is transiently unmounted), so reaping
    /// would kill a healthy daemon and delete its plist.
    pub spared: Vec<String>,
}

/// What to do with a launch agent, from the facts about its `--db`.
enum GcVerdict {
    Reap,
    Keep,
    Spare,
}

/// Decide an agent's fate. Reaping a LIVE daemon is the catastrophic direction —
/// `bootout` also deletes the plist, so nothing restarts it when a transiently
/// unmounted volume returns — so when the DB path is gone we only reap if no live
/// daemon holds the pidfile flock (a crash-looper dies at DB-open before ever
/// taking the lock). Temp/unparseable DBs are always ephemeral cruft. Probe
/// errors resolve to `daemon_live = true` upstream, biasing toward Spare.
fn gc_verdict(db_parsed: bool, is_temp: bool, db_exists: bool, daemon_live: bool) -> GcVerdict {
    if !db_parsed || is_temp {
        return GcVerdict::Reap;
    }
    if db_exists {
        return GcVerdict::Keep;
    }
    if daemon_live {
        GcVerdict::Spare
    } else {
        GcVerdict::Reap
    }
}

/// Is a live daemon holding the pidfile flock for `label`'s instance? The daemon
/// opens the DB *before* it takes the pidfile `flock(LOCK_EX)` and holds it for
/// its whole lifetime, so a held lock means a healthy daemon while a crash-looper
/// (which dies at DB-open) never acquires it. The pidfile lives off the DB
/// volume, so this is probeable even while that volume is unmounted. Fails toward
/// `true` (spare) on any error we can't interpret.
fn agent_daemon_is_live(label: &str) -> bool {
    let Some(instance_id) = label.strip_prefix("io.kehl.nestweaver.") else {
        return false;
    };
    let pidfile = lifecycle::pidfile_path(instance_id);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pidfile)
    {
        Ok(f) => f,
        // No pidfile → no daemon holds a lock → not live.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        // Any other error (permissions, etc.) → can't tell → fail SPARE.
        Err(_) => return true,
    };
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        // Could not acquire → a live daemon holds it.
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
    }
    // We acquired it → no live daemon. Release immediately.
    unsafe {
        libc::flock(fd, libc::LOCK_UN);
    }
    false
}

/// Remove orphaned nestweaver launch agents. An agent is reaped when its `--db`
/// path is under a temp dir, is unparseable, or is gone AND no live daemon holds
/// its pidfile flock. Agents whose DB exists are kept; agents whose DB is gone
/// but whose daemon is still alive (transient unmount) are spared.
pub fn gc_orphaned_agents() -> Result<GcReport> {
    let dir = launchd_agents_dir();
    let uid = unsafe { libc::getuid() };
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    let mut spared = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => {
            return Ok(GcReport {
                removed,
                kept,
                spared,
            });
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        // Only per-instance daemon agents: io.kehl.nestweaver.<hash>.plist.
        let Some(label) = fname
            .strip_suffix(".plist")
            .filter(|l| l.starts_with("io.kehl.nestweaver."))
        else {
            continue;
        };

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let db = parse_db_path_from_plist(&content);
        let (db_parsed, is_temp, db_exists) = match &db {
            Some(p) => (true, is_temp_db_path(p), p.exists()),
            None => (false, false, false),
        };
        // Only probe liveness when it actually decides the outcome (DB gone,
        // non-temp) — avoids a flock syscall on every kept/temp agent.
        let daemon_live = db_parsed && !is_temp && !db_exists && agent_daemon_is_live(label);

        match gc_verdict(db_parsed, is_temp, db_exists, daemon_live) {
            GcVerdict::Reap => {
                let _ = Command::new("launchctl")
                    .args(["bootout", &format!("gui/{uid}/{label}")])
                    .output();
                let _ = std::fs::remove_file(&path);
                removed.push(label.to_string());
            }
            GcVerdict::Keep => kept.push(label.to_string()),
            GcVerdict::Spare => spared.push(label.to_string()),
        }
    }

    Ok(GcReport {
        removed,
        kept,
        spared,
    })
}

pub fn generate_plist(
    instance_id: &str,
    binary_path: &Path,
    db_path: &Path,
    log_path: &Path,
) -> String {
    let label = lifecycle::launchd_label(instance_id);
    let binary = binary_path.display();
    let db = db_path.display();
    let log = log_path.display();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>daemon</string>
        <string>--db</string>
        <string>{db}</string>
        <string>run</string>
    </array>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>KeepAlive</key>
    <dict>
        <key>Crashed</key>
        <true/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>LowPriorityIO</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
    )
}

pub fn install_and_start(instance_id: &str, plist_content: &str) -> Result<()> {
    let plist_path = lifecycle::launchd_plist_path(instance_id);
    let label = lifecycle::launchd_label(instance_id);
    let uid = unsafe { libc::getuid() };

    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&plist_path, plist_content)
        .with_context(|| format!("write plist: {}", plist_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&plist_path, std::fs::Permissions::from_mode(0o644))?;
    }

    let _ = Command::new("launchctl")
        .args(["enable", &format!("gui/{uid}/{label}")])
        .output();

    // If the label is already bootstrapped, bootout first: bootstrap of an
    // already-loaded label is a no-op, so without this an updated plist (new
    // keys, changed paths) would never take effect on existing installs.
    let already_bootstrapped = Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if already_bootstrapped {
        let bootout = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{label}")])
            .output()
            .context("failed to run launchctl bootout")?;
        if !bootout.status.success() {
            let stderr = String::from_utf8_lossy(&bootout.stderr);
            // Tolerate the job vanishing between the print probe and bootout.
            if !stderr.contains("No such process") && !stderr.contains("Could not find service") {
                anyhow::bail!("launchctl bootout failed: {stderr}");
            }
        }
    }

    let output = Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            &plist_path.to_string_lossy(),
        ])
        .output()
        .context("failed to run launchctl bootstrap")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("already loaded") && !stderr.contains("already bootstrapped") {
            anyhow::bail!("launchctl bootstrap failed: {stderr}");
        }
    }

    // Kickstart the agent — bootstrap only registers, doesn't start without RunAtLoad
    let kick = Command::new("launchctl")
        .args(["kickstart", &format!("gui/{uid}/{label}")])
        .output()
        .context("failed to run launchctl kickstart")?;

    if !kick.status.success() {
        let stderr = String::from_utf8_lossy(&kick.stderr);
        anyhow::bail!("launchctl kickstart failed: {stderr}");
    }

    Ok(())
}

pub fn stop_and_uninstall(instance_id: &str) -> Result<()> {
    let plist_path = lifecycle::launchd_plist_path(instance_id);
    let label = lifecycle::launchd_label(instance_id);
    let uid = unsafe { libc::getuid() };

    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{label}")])
        .output();

    let _ = std::fs::remove_file(&plist_path);

    Ok(())
}

pub fn is_running(instance_id: &str) -> bool {
    let label = lifecycle::launchd_label(instance_id);
    let uid = unsafe { libc::getuid() };

    Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_db_path_from_plist_extracts_db_arg() {
        let plist = generate_plist(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
        );
        assert_eq!(
            parse_db_path_from_plist(&plist),
            Some(PathBuf::from("/Users/k/dev/repo/brain.lbug"))
        );
        assert_eq!(parse_db_path_from_plist("<plist></plist>"), None);
    }

    #[test]
    fn plist_renders_low_priority_io_and_throttle_interval() {
        let plist = generate_plist(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
        );
        // Top-level LowPriorityIO demotes the daemon's disk I/O.
        assert!(
            plist.contains("<key>LowPriorityIO</key>\n    <true/>"),
            "{plist}"
        );
        // Top-level ThrottleInterval damps tight respawn loops after
        // repeated crashes (launchd.plist(5) documents it as a top-level key).
        assert!(
            plist.contains("<key>ThrottleInterval</key>\n    <integer>10</integer>"),
            "{plist}"
        );
        // ProcessType must stay Interactive (Background would throttle harder).
        assert!(
            plist.contains("<key>ProcessType</key>\n    <string>Interactive</string>"),
            "{plist}"
        );
    }

    #[test]
    fn is_temp_db_path_flags_temp_but_not_real_paths() {
        assert!(is_temp_db_path(Path::new("/tmp/ppi2/test.lbug")));
        assert!(is_temp_db_path(Path::new("/private/tmp/x/test.lbug")));
        assert!(is_temp_db_path(Path::new("/var/folders/xx/y/T/test.lbug")));
        assert!(!is_temp_db_path(Path::new(
            "/home/user/.local/share/nestweaver/my-brain/brain.lbug"
        )));
    }

    #[test]
    fn gc_verdict_spares_live_daemon_but_reaps_crash_looper() {
        // args: (db_parsed, is_temp, db_exists, daemon_live)
        // Unparseable plist → reap as cruft.
        assert!(matches!(
            gc_verdict(false, false, false, false),
            GcVerdict::Reap
        ));
        // Temp DB → always reap (ephemeral; the original leak), liveness irrelevant.
        assert!(matches!(
            gc_verdict(true, true, false, true),
            GcVerdict::Reap
        ));
        // Real DB present → keep.
        assert!(matches!(
            gc_verdict(true, false, true, false),
            GcVerdict::Keep
        ));
        // DB gone but a live daemon holds the pidfile flock (transient unmount) → SPARE.
        assert!(matches!(
            gc_verdict(true, false, false, true),
            GcVerdict::Spare
        ));
        // DB gone and no live daemon (crash-looper / orphan) → reap.
        assert!(matches!(
            gc_verdict(true, false, false, false),
            GcVerdict::Reap
        ));
    }
}
