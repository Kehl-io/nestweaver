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
        false,
    )
}

/// Buffer added to the drain ceiling to derive the plist's `ExitTimeOut`.
///
/// DELIBERATELY LARGER than the CLI's `STOP_GRACE_BUFFER_SECS` (30), and the
/// gap is load-bearing rather than cosmetic. Both are derived from the same
/// drain ceiling, so making them equal made the two deadlines land at the same
/// instant: `daemon stop` would give up watching at ceiling+30 and print "it is
/// still running and still serving reads" about a process launchd had SIGKILLed
/// at ceiling+30. Keeping launchd's deadline strictly later means the CLI's
/// report is about a process that is still alive whenever launchd is the
/// supervisor.
///
/// It does not make the drain safe under launchd — a write still running at
/// `ExitTimeOut` is still SIGKILLed. It makes the CLI stop lying about it.
const LAUNCHD_EXIT_TIMEOUT_BUFFER_SECS: u64 = 60;

/// Render a per-instance launch agent, optionally forwarding an absolute
/// instance configuration path to the foreground `daemon run` process.
///
/// `start_at_login` emits `RunAtLoad`. Without it launchd *registers* the agent
/// at login but never starts it — `install_and_start` compensates with an
/// explicit `kickstart`, which covers install time but not reboot.
pub fn generate_plist_with_config(
    instance_id: &str,
    binary_path: &Path,
    db_path: &Path,
    log_path: &Path,
    index_cpu_percent: Option<&str>,
    config_path: Option<&Path>,
    start_at_login: bool,
) -> String {
    // The ceiling is read from the environment exactly once, here, and passed
    // down explicitly. Keeping the read out of the renderer is what lets the
    // ceiling/ExitTimeOut/baked-env relationship be tested without a test
    // mutating process-wide env — which would race the other plist tests under
    // cargo's parallel execution.
    render_plist(
        instance_id,
        binary_path,
        db_path,
        log_path,
        index_cpu_percent,
        config_path,
        start_at_login,
        nestweaver_schema::drain_ceiling_from_env(),
        std::env::var("NESTWEAVER_DRAIN_TIMEOUT_SECS").is_ok(),
    )
}

/// Pure renderer. `drain_ceiling` and `ceiling_was_overridden` are supplied by
/// the caller rather than read here — see `generate_plist_with_config`.
#[allow(clippy::too_many_arguments)]
fn render_plist(
    instance_id: &str,
    binary_path: &Path,
    db_path: &Path,
    log_path: &Path,
    index_cpu_percent: Option<&str>,
    config_path: Option<&Path>,
    start_at_login: bool,
    drain_ceiling: u64,
    ceiling_was_overridden: bool,
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

    // How long launchd waits after SIGTERM before SIGKILLing us. See
    // `LAUNCHD_EXIT_TIMEOUT_BUFFER_SECS` for why the buffer is larger than the
    // CLI's.
    //
    // This key was previously ABSENT, so launchd applied its 20-second default
    // — a hard SIGKILL 20s into a drain that routinely runs for minutes. That
    // is the nw-126 crash (a SIGKILLed daemon left a stale WAL that made a live
    // database look absent) on a 20-second fuse, and it fired regardless of
    // what the daemon or `daemon stop` did, because it is launchd's timer and
    // not ours.
    //
    // This does NOT make the drain guarantee absolute under launchd: a write
    // still running at `exit_timeout` is still SIGKILLed, by launchd, outside
    // our control. It moves the deadline from "20s, always fires" to a bound
    // derived from the ceiling the operator already reasons about. launchd does
    // treat `0` as infinity, but a job that can refuse to die forever is a
    // wedged logout and a worse failure than a bounded one, so it is not used.
    let exit_timeout = drain_ceiling.saturating_add(LAUNCHD_EXIT_TIMEOUT_BUFFER_SECS);

    // launchd jobs don't inherit the invoking shell's environment, so anything
    // the daemon needs at runtime has to be baked in here.
    //
    // NESTWEAVER_DRAIN_TIMEOUT_SECS is baked in for a specific reason:
    // `exit_timeout` above is computed from the ceiling as seen by THIS
    // process — the CLI running `daemon start`. Without baking it, the daemon
    // launchd starts would not inherit it and would run the 660s default while
    // the plist carried a deadline derived from whatever the installing shell
    // happened to export. `NESTWEAVER_DRAIN_TIMEOUT_SECS=60 nestweaver daemon
    // start` produced ExitTimeOut=120 against a daemon draining to 660s — a
    // supervisor deadline SHORTER than the drain it is supposed to outlast,
    // which is the original 20s bug in a new disguise. Baking it makes the
    // plist internally consistent: the daemon and its `ExitTimeOut` read the
    // same number.
    //
    // Baked ONLY when the var is actually set. When it is unset both this
    // process and the daemon fall back to the same compiled default, so the
    // plist stays byte-identical to the pre-`ExitTimeOut` output for the
    // machines that never set it — the property `plist_omits_environment_...`
    // guards. There is no divergence to fix in that case; the divergence only
    // exists when someone exported a value that the daemon would not inherit.
    let mut env_entries: Vec<(&str, String)> = Vec::new();
    if ceiling_was_overridden {
        env_entries.push(("NESTWEAVER_DRAIN_TIMEOUT_SECS", drain_ceiling.to_string()));
    }
    // Index CPU-throttle knob, baked when it was set (and validated) at install
    // time. Value is validated numeric by the caller.
    if let Some(v) = index_cpu_percent {
        env_entries.push(("NESTWEAVER_INDEX_CPU_PERCENT", xml_escape(v.trim())));
    }
    let env_block = if env_entries.is_empty() {
        String::new()
    } else {
        let entries = env_entries
            .iter()
            .map(|(k, v)| format!("        <key>{k}</key>\n        <string>{v}</string>\n"))
            .collect::<String>();
        format!("    <key>EnvironmentVariables</key>\n    <dict>\n{entries}    </dict>\n")
    };

    // Opt-in. Omitted entirely rather than emitted as `<false/>` so a plist
    // from a machine that never opted in is byte-identical to the old output.
    let run_at_load = if start_at_login {
        "    <key>RunAtLoad</key>\n    <true/>\n"
    } else {
        ""
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
    <key>ExitTimeOut</key>
    <integer>{exit_timeout}</integer>
    <key>LowPriorityIO</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
{run_at_load}{env_block}</dict>
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

/// Review follow-up on nw-417 (FIX 1). The bounded wait below is correctly
/// placed and correctly bounded, but its FIRST version discarded the
/// timeout outcome — `stop_and_uninstall` returned `Ok(())` whether the job
/// was confirmed gone or merely never checked again. Both call sites relied
/// on that: `daemon start`'s reinstall-over-an-incumbent path discarded the
/// `Result` outright, and `DaemonAction::Stop` set its own success flag
/// unconditionally after calling this, rather than from the confirmation.
/// That is exactly the class this whole batch exists to close: a recovery
/// command reporting success while the thing it claims to have stopped is
/// still alive, precisely when launchd is wedged — the case this function's
/// own history is about. `stop_and_uninstall` now returns `Err` when it
/// cannot confirm absence within the bound, so every caller's `?` (or an
/// explicit check) surfaces it instead of silently discarding it.
pub fn stop_and_uninstall(instance_id: &str) -> Result<()> {
    let plist_path = lifecycle::launchd_plist_path(instance_id);
    let label = lifecycle::launchd_label(instance_id);
    let uid = unsafe { libc::getuid() };

    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{label}")])
        .output();

    // nw-417. `bootout`'s subprocess exiting only means launchd ACCEPTED the
    // teardown request, not that the job is gone: reproduced live, an
    // immediate `launchctl print` on the SAME label right after `bootout`
    // returns exit 0, `state = SIGTERMed` — the job is still tearing down. A
    // poll confirmed it clears within about a second. `is_running` used to be
    // a single, unretried `print`, so every caller of this function —
    // including this one's own test — was asserting on kernel state the
    // instant after releasing it, which is a race, not a flake. Waiting HERE,
    // in the product, means every caller downstream (the plist removal right
    // below, `daemon start`'s reinstall-over-an-incumbent path, `daemon
    // stop`) observes a genuinely absent job instead of a launchd job mid-exit
    // — fixing the teardown's correctness rather than adding a retry to each
    // caller separately.
    let confirmed_absent = wait_for_launchd_absence(instance_id, std::time::Duration::from_secs(5));

    // Best-effort regardless of confirmation: a stale plist left behind is
    // its own hazard (it would make a later `install_and_start` think an
    // update needs bootstrapping over a job that never existed), and
    // removing it does not itself claim the JOB is gone — that claim is the
    // `anyhow::ensure!` below, which fires independently of this cleanup.
    let _ = std::fs::remove_file(&plist_path);

    anyhow::ensure!(
        confirmed_absent,
        "launchd did not confirm {label} stopped within 5s of `bootout` — it may still be \
         tearing down, or genuinely wedged. Do not treat it as stopped: check with \
         `launchctl print gui/{uid}/{label}` before retrying."
    );
    Ok(())
}

/// Bounded poll for `is_running(instance_id)` to report the job truly gone.
///
/// Returns `true` once absence is confirmed, `false` if `timeout` elapsed
/// with the job still reporting present — never blocks past the bound
/// either way. The caller decides what a timeout means; this function only
/// answers the yes/no question honestly instead of returning early as if it
/// had.
fn wait_for_launchd_absence(instance_id: &str, timeout: std::time::Duration) -> bool {
    wait_for_absence_with_probe(timeout, || !is_running(instance_id))
}

/// The bounded-poll core of [`wait_for_launchd_absence`], parameterized over
/// the presence probe so a test can drive the TIMEOUT path deterministically.
///
/// A real launchd job wedged for the whole bound is not something a test can
/// safely manufacture — this repo's own shared launchd domain holds ~15-20
/// live daemons at any time, and hand-registering a stuck job to race against
/// is the wrong kind of risk for a unit test. A fake probe closure exercises
/// the identical bounded-loop logic (poll, check deadline, sleep) with zero
/// real launchd interaction and zero timing flakiness.
fn wait_for_absence_with_probe(
    timeout: std::time::Duration,
    mut probe: impl FnMut() -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if probe() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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

    /// nw-417 FIX 1. The bound is real: a probe that never reports absence
    /// must not block past `timeout`, and the caller must be TOLD it timed
    /// out (`false`) rather than getting the same `()`-shaped nothing a
    /// success would have produced.
    #[test]
    fn wait_for_absence_with_probe_reports_false_on_timeout() {
        let calls = std::cell::Cell::new(0u32);
        let start = std::time::Instant::now();
        let result = wait_for_absence_with_probe(std::time::Duration::from_millis(150), || {
            calls.set(calls.get() + 1);
            false
        });
        assert!(!result, "a probe that never reports absence must time out");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "must never block substantially past the bound: {:?}",
            start.elapsed()
        );
        assert!(
            calls.get() >= 2,
            "must poll more than once inside the bound, not give up after the first check: {}",
            calls.get()
        );
    }

    /// The counterweight: a probe that reports absence immediately must
    /// return `true` without waiting out the bound at all — otherwise the
    /// timeout test above could pass because EVERY call takes the full
    /// timeout regardless of what the probe says.
    #[test]
    fn wait_for_absence_with_probe_returns_true_immediately_when_already_absent() {
        let start = std::time::Instant::now();
        let result = wait_for_absence_with_probe(std::time::Duration::from_secs(5), || true);
        assert!(result);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "an immediately-true probe must not wait out any part of the bound: {:?}",
            start.elapsed()
        );
    }

    /// The middle case: absence confirmed after a few false polls, well
    /// inside the bound — proves the loop actually re-polls rather than
    /// deciding on the first call alone.
    #[test]
    fn wait_for_absence_with_probe_returns_true_once_the_probe_flips() {
        let calls = std::cell::Cell::new(0u32);
        let result = wait_for_absence_with_probe(std::time::Duration::from_secs(5), || {
            calls.set(calls.get() + 1);
            calls.get() >= 3
        });
        assert!(result);
        assert_eq!(calls.get(), 3);
    }

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

    /// Without this key launchd applied its own 20s default: a hard SIGKILL 20
    /// seconds into a drain that routinely runs for minutes, on launchd's timer
    /// rather than ours. That is the nw-126 crash on a short fuse, and it fired
    /// no matter what `daemon stop` or the SIGTERM drain did.
    ///
    /// The value must track the drain ceiling so the supervised macOS path and
    /// the interactive `daemon stop` path cannot drift apart. This does not
    /// make the drain unbounded under launchd — a write still running at the
    /// timeout is still SIGKILLed, by launchd, outside this process's control.
    #[test]
    fn plist_sets_exit_timeout_from_the_drain_ceiling() {
        let plist = generate_plist(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
            None,
        );
        let expected = nestweaver_schema::drain_ceiling_from_env()
            .saturating_add(LAUNCHD_EXIT_TIMEOUT_BUFFER_SECS);
        assert!(
            plist.contains(&format!(
                "<key>ExitTimeOut</key>\n    <integer>{expected}</integer>"
            )),
            "ExitTimeOut must be present and derived from the drain ceiling, \
             not left to launchd's 20s default: {plist}"
        );
        assert!(
            expected > 20,
            "the whole point is to beat launchd's 20s default"
        );
        // The CLI's own `STOP_GRACE_BUFFER_SECS` is 30 and lives in `src/main.rs`,
        // which this crate cannot reference — so it is pinned by value here.
        // These two were equal, which put both deadlines at the same instant and
        // let `daemon stop` print "still running and still serving reads" about
        // a process launchd had just SIGKILLed. The CLI must give up watching
        // STRICTLY BEFORE launchd kills.
        // A `const` assertion, so violating the invariant fails the BUILD
        // rather than one test run — the value is known at compile time.
        const _: () = assert!(
            LAUNCHD_EXIT_TIMEOUT_BUFFER_SECS > 30,
            "launchd's deadline must be strictly later than the CLI's stop grace \
             (STOP_GRACE_BUFFER_SECS = 30 in src/main.rs), or the CLI reports on \
             a process that is already dead"
        );
    }

    /// `ExitTimeOut` is computed from the ceiling as seen by the CLI process
    /// running `daemon start`, but launchd jobs inherit no shell environment —
    /// so without baking the variable in, the daemon launchd starts runs a
    /// DIFFERENT ceiling from the one its own SIGKILL deadline was derived
    /// from. `NESTWEAVER_DRAIN_TIMEOUT_SECS=60` gave `ExitTimeOut=120` against a
    /// daemon draining to 660s: a supervisor deadline shorter than the drain it
    /// exists to outlast, which is the original 20s bug wearing a new hat.
    #[test]
    fn plist_bakes_the_drain_ceiling_it_derived_exit_timeout_from() {
        let plist = render_plist(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
            None,
            None,
            false,
            60,
            true,
        );

        assert!(
            plist.contains("<key>NESTWEAVER_DRAIN_TIMEOUT_SECS</key>\n        <string>60</string>"),
            "the ceiling ExitTimeOut was derived from must be baked in, or the \
             daemon will not run it: {plist}"
        );
        assert!(
            plist.contains(&format!(
                "<key>ExitTimeOut</key>\n    <integer>{}</integer>",
                60 + LAUNCHD_EXIT_TIMEOUT_BUFFER_SECS
            )),
            "and the deadline must match that same ceiling: {plist}"
        );
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
            false,
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

    /// `RunAtLoad` is what makes the agent *start* at login rather than merely
    /// register. It is opt-in, and when off the key must be absent entirely —
    /// not `<false/>` — so plists on machines that never opted in are unchanged.
    #[test]
    fn plist_omits_run_at_load_unless_start_at_login_is_set() {
        let plist = generate_plist_with_config(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
            None,
            None,
            false,
        );
        assert!(!plist.contains("RunAtLoad"), "{plist}");
        // The 5-arg convenience wrapper must not silently opt a caller in.
        let default_plist = generate_plist(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
            None,
        );
        assert!(!default_plist.contains("RunAtLoad"), "{default_plist}");
    }

    #[test]
    fn plist_emits_run_at_load_when_start_at_login_is_set() {
        let plist = generate_plist_with_config(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
            Some("45"),
            Some(Path::new("/Users/k/dev/repo/nestweaver-instance.toml")),
            true,
        );
        assert!(
            plist.contains("<key>RunAtLoad</key>\n    <true/>"),
            "{plist}"
        );
        // RunAtLoad must sit as a sibling of the other top-level keys, not
        // inside the EnvironmentVariables dict it is rendered next to.
        assert!(
            plist.contains(
                "<key>ProcessType</key>\n    <string>Interactive</string>\n    \
                 <key>RunAtLoad</key>\n    <true/>\n    <key>EnvironmentVariables</key>"
            ),
            "{plist}"
        );
        // Coexists with the other dynamic blocks without disturbing them.
        assert_eq!(
            parse_db_path_from_plist(&plist),
            Some(PathBuf::from("/Users/k/dev/repo/brain.lbug"))
        );
        assert!(plist.contains("<string>--config</string>"), "{plist}");
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

        let plist = generate_plist_with_config(
            &instance_id,
            &binary,
            &db,
            &log,
            Some(&cpu),
            Some(&config),
            false,
        );
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
        let plist = generate_plist_with_config(
            &instance_id,
            &binary,
            &db,
            &log,
            Some(&cpu),
            Some(&config),
            false,
        );
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
        assert_eq!(decoded.get("RunAtLoad"), None);
    }

    /// launchd, not just the XML parser, has to accept `RunAtLoad` where we put
    /// it. A structural string assertion cannot prove the key decodes as a
    /// top-level boolean rather than landing inside a neighboring dict.
    #[cfg(target_os = "macos")]
    #[test]
    fn plist_with_run_at_load_decodes_as_a_top_level_boolean() {
        let plist = generate_plist_with_config(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
            Some("45"),
            Some(Path::new("/Users/k/dev/repo/nestweaver-instance.toml")),
            true,
        );
        let dir = tempfile::tempdir().unwrap();
        let plist_path = dir.path().join("run-at-load.plist");
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
        let decoded: serde_json::Value =
            serde_json::from_slice(&json.stdout).expect("plutil must emit JSON");
        assert_eq!(decoded["RunAtLoad"], serde_json::json!(true));
        // Still a sibling of, not swallowed by, the env dict.
        assert_eq!(
            decoded["EnvironmentVariables"]["NESTWEAVER_INDEX_CPU_PERCENT"],
            serde_json::json!("45")
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
