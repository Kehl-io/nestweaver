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

/// Re-exported so this module's callers (and `main.rs`) keep a stable path.
/// The predicate itself lives in `lifecycle` because it is not
/// launchd-specific and this module is macOS-gated.
pub use crate::lifecycle::is_temp_db_path;

/// Escape a dynamic value for use as XML character data in a plist `<string>`.
fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Decode the XML entities emitted by [`xml_escape`].
///
/// `&amp;` is decoded last so an original literal such as `&lt;` survives one
/// round trip as `&lt;` instead of being decoded twice into `<`.
fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
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
    strings.get(pos + 1).map(|s| PathBuf::from(xml_unescape(s)))
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
    index_cpu_percent: Option<&str>,
) -> String {
    generate_plist_with_config(
        instance_id,
        binary_path,
        db_path,
        log_path,
        index_cpu_percent,
        None,
    )
}

/// Render a per-instance launch agent, optionally forwarding an absolute
/// instance configuration path to the foreground `daemon run` process.
pub fn generate_plist_with_config(
    instance_id: &str,
    binary_path: &Path,
    db_path: &Path,
    log_path: &Path,
    index_cpu_percent: Option<&str>,
    config_path: Option<&Path>,
) -> String {
    let label = xml_escape(&lifecycle::launchd_label(instance_id));
    let binary = xml_escape(&binary_path.display().to_string());
    let db = xml_escape(&db_path.display().to_string());
    let log = xml_escape(&log_path.display().to_string());
    let config_args = config_path
        .map(|path| {
            let path = xml_escape(&path.display().to_string());
            format!("        <string>--config</string>\n        <string>{path}</string>\n")
        })
        .unwrap_or_default();

    // launchd jobs don't inherit the invoking shell's environment, so the
    // index CPU-throttle knob is baked into the plist when it was set (and
    // validated) at install time. Value is validated numeric by the caller.
    let env_block = match index_cpu_percent {
        Some(v) => {
            let value = xml_escape(v.trim());
            format!(
                "    <key>EnvironmentVariables</key>\n    <dict>\n        <key>NESTWEAVER_INDEX_CPU_PERCENT</key>\n        <string>{value}</string>\n    </dict>\n"
            )
        }
        None => String::new(),
    };

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
{config_args}
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
{env_block}</dict>
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
    if is_running(instance_id) {
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
            None,
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
            None,
        );
        // Top-level LowPriorityIO demotes the daemon's disk I/O.
        assert!(
            plist.contains("<key>LowPriorityIO</key>\n    <true/>"),
            "{plist}"
        );
        // Top-level ThrottleInterval pins launchd's default 10s respawn
        // delay explicitly (it equals the default; writing it makes the
        // value deliberate rather than inherited).
        assert!(
            plist.contains("<key>ThrottleInterval</key>\n    <integer>10</integer>"),
            "{plist}"
        );
        // ProcessType must stay Interactive (Background would throttle harder).
        assert!(
            plist.contains("<key>ProcessType</key>\n    <string>Interactive</string>"),
            "{plist}"
        );
        // No EnvironmentVariables block without the CPU knob.
        assert!(!plist.contains("EnvironmentVariables"), "{plist}");
    }

    #[test]
    fn plist_bakes_cpu_percent_into_environment_variables() {
        let plist = generate_plist(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
            Some("45"),
        );
        assert!(
            plist.contains(
                "<key>EnvironmentVariables</key>\n    <dict>\n        <key>NESTWEAVER_INDEX_CPU_PERCENT</key>\n        <string>45</string>\n    </dict>"
            ),
            "{plist}"
        );
        // Still parses back to the same db path with the env block present.
        assert_eq!(
            parse_db_path_from_plist(&plist),
            Some(PathBuf::from("/Users/k/dev/repo/brain.lbug"))
        );
    }

    #[test]
    fn plist_forwards_absolute_config_to_daemon_run() {
        let plist = generate_plist_with_config(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
            None,
            Some(Path::new("/Users/k/dev/repo/nestweaver-instance.toml")),
        );
        assert!(
            plist.contains(
                "<string>run</string>\n        <string>--config</string>\n        \
                 <string>/Users/k/dev/repo/nestweaver-instance.toml</string>"
            ),
            "{plist}"
        );
        assert!(
            !plist.lines().any(|line| line.trim() == "\\"),
            "plist must not contain a literal format-continuation escape:\n{plist}"
        );
    }

    #[test]
    fn plist_escapes_all_dynamic_strings_and_round_trips_db_path() {
        let metacharacters = r#"& < > " '"#;
        let instance_id = format!("instance-{metacharacters}");
        let binary = PathBuf::from(format!("/Applications/Nest {metacharacters}/nestweaver"));
        let db = PathBuf::from(format!("/Users/k/Brain {metacharacters}/brain.lbug"));
        let config = PathBuf::from(format!("/Users/k/Config {metacharacters}/instance.toml"));
        let log = PathBuf::from(format!("/Users/k/Logs {metacharacters}/daemon.log"));
        let cpu = format!("cpu-{metacharacters}");

        let plist =
            generate_plist_with_config(&instance_id, &binary, &db, &log, Some(&cpu), Some(&config));
        let escaped = "&amp; &lt; &gt; &quot; &apos;";

        for expected in [
            format!("<string>io.kehl.nestweaver.instance-{escaped}</string>"),
            format!("<string>/Applications/Nest {escaped}/nestweaver</string>"),
            format!("<string>/Users/k/Brain {escaped}/brain.lbug</string>"),
            format!("<string>/Users/k/Config {escaped}/instance.toml</string>"),
            format!("<string>/Users/k/Logs {escaped}/daemon.log</string>"),
            format!("<string>cpu-{escaped}</string>"),
        ] {
            assert!(
                plist.contains(&expected),
                "missing escaped dynamic value {expected:?}:\n{plist}"
            );
        }
        assert_eq!(parse_db_path_from_plist(&plist), Some(db));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_with_xml_metacharacters_is_valid_and_preserves_values() {
        let metacharacters = r#"& < > " '"#;
        let instance_id = format!("instance-{metacharacters}");
        let binary = PathBuf::from(format!("/Applications/Nest {metacharacters}/nestweaver"));
        let db = PathBuf::from(format!("/Users/k/Brain {metacharacters}/brain.lbug"));
        let config = PathBuf::from(format!("/Users/k/Config {metacharacters}/instance.toml"));
        let log = PathBuf::from(format!("/Users/k/Logs {metacharacters}/daemon.log"));
        let cpu = format!("cpu-{metacharacters}");
        let plist =
            generate_plist_with_config(&instance_id, &binary, &db, &log, Some(&cpu), Some(&config));
        let dir = tempfile::tempdir().unwrap();
        let plist_path = dir.path().join("special-values.plist");
        std::fs::write(&plist_path, &plist).unwrap();

        let lint = Command::new("plutil")
            .arg("-lint")
            .arg(&plist_path)
            .output()
            .expect("plutil must be available on macOS");
        assert!(
            lint.status.success(),
            "plutil rejected generated plist:\n{}",
            String::from_utf8_lossy(&lint.stderr)
        );

        let json = Command::new("plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&plist_path)
            .output()
            .expect("plutil must decode generated plist");
        assert!(
            json.status.success(),
            "plutil JSON conversion failed:\n{}",
            String::from_utf8_lossy(&json.stderr)
        );
        let decoded: serde_json::Value =
            serde_json::from_slice(&json.stdout).expect("plutil must emit JSON");
        assert_eq!(
            decoded["Label"],
            serde_json::json!(format!("io.kehl.nestweaver.{instance_id}"))
        );
        assert_eq!(
            decoded["ProgramArguments"],
            serde_json::json!([
                binary.display().to_string(),
                "daemon",
                "--db",
                db.display().to_string(),
                "run",
                "--config",
                config.display().to_string(),
            ])
        );
        assert_eq!(
            decoded["StandardOutPath"],
            serde_json::json!(log.display().to_string())
        );
        assert_eq!(
            decoded["StandardErrorPath"],
            serde_json::json!(log.display().to_string())
        );
        assert_eq!(
            decoded["EnvironmentVariables"]["NESTWEAVER_INDEX_CPU_PERCENT"],
            serde_json::json!(cpu)
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
