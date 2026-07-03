//! Subprocess execution for `git` with a hard timeout that kills and reaps the
//! whole process group.
//!
//! Server-mode indexing spawns `git` for clone/fetch/ls-remote against arbitrary
//! remotes. A hung remote (network blackhole) must not wedge a worker task or the
//! poll scheduler forever: without a timeout a blocking `Command::output()` blocks
//! indefinitely, so the worker holds its semaphore permit forever (starving the
//! pool) and the poll loop stalls every other repo behind the wedged one.
//!
//! [`run_git_with_timeout`] bounds every invocation. Critically, `git` for a
//! network transfer forks a *helper* subprocess (`git-remote-https`, `ssh`, …)
//! that inherits our stderr pipe and does the actual blocking network read.
//! Killing only the direct `git` pid orphans that helper — it keeps the pipe's
//! write-end open on a blackholed remote, so `read_to_end` never sees EOF and the
//! reader thread's `join()` blocks for minutes (the OS TCP timeout), defeating the
//! whole point. So we put the child in its own process group and, on timeout,
//! `killpg` the group — `git` **and** its helpers die, the pipes close, and the
//! reader threads return promptly. We then `wait()` the direct child to reap it
//! (on Unix a child must be reaped by its parent to free the process-table slot).
//!
//! Unix-only: this is a macOS/Linux developer tool.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Timeout for the network-touching but *lightweight* git operations
/// (fetch/ls-remote/rev-parse/config): they exchange only refs or small deltas,
/// so a healthy call finishes in well under a minute. Override with
/// `NESTWEAVER_GIT_NET_TIMEOUT_SECS`.
pub fn git_net_timeout() -> Duration {
    env_secs("NESTWEAVER_GIT_NET_TIMEOUT_SECS", 60)
}

/// Timeout for the initial blobless bare *clone*, which still transfers the full
/// commit+tree history and can legitimately take minutes over a slow link — a cap
/// as tight as [`git_net_timeout`] would SIGKILL a slow-but-progressing clone and
/// retry it from scratch forever, never converging. Override with
/// `NESTWEAVER_GIT_CLONE_TIMEOUT_SECS`.
pub fn git_clone_timeout() -> Duration {
    env_secs("NESTWEAVER_GIT_CLONE_TIMEOUT_SECS", 600)
}

fn env_secs(var: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(var)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(default),
    )
}

/// How often the wait loop wakes to check for child exit / timeout. Small enough
/// to stay responsive, large enough not to busy-spin.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// SIGKILL the whole process group, reap the direct child, and join both reader
/// threads. Killing the *group* (not just the direct pid) takes down git's
/// network-helper subprocesses too, so their write-end of the pipes closes,
/// `read_to_end` sees EOF, and the joins return promptly instead of blocking on a
/// blackholed remote. Returns the drained stderr so the caller can report where
/// git stalled. (Helpers, once killed, are reparented to init and reaped there.)
fn kill_group_reap_drain(
    pgid: libc::pid_t,
    mut child: std::process::Child,
    stdout_reader: std::thread::JoinHandle<Vec<u8>>,
    stderr_reader: std::thread::JoinHandle<Vec<u8>>,
) -> Vec<u8> {
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
    let _ = stdout_reader.join();
    stderr_reader.join().unwrap_or_default()
}

/// Run a `git` command (any `Command`, really) with a hard timeout.
///
/// Captures stdout/stderr, draining both on dedicated reader threads so a child
/// that writes more than a pipe buffer can hold never deadlocks. If the child (or
/// any of its network-helper children in the same process group) does not exit
/// within `timeout`, the whole group is killed (SIGKILL), the direct child is
/// reaped, and an error is returned — the call never blocks past `timeout` plus
/// one poll interval, even against a blackholed remote.
pub fn run_git_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Put the child in its own process group (pgid == child pid) so that on
    // timeout we can signal git AND every helper subprocess it forked. Safe:
    // `setpgid(0, 0)` is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("failed to spawn git")?;
    let pgid = child.id() as libc::pid_t;

    // Drain the pipes concurrently with the wait. A child that fills the stdout
    // or stderr pipe buffer would block on write if we only polled `try_wait`,
    // deadlocking us against it — so the reads must run on their own threads.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut s) = stdout_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut s) = stderr_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let stderr = kill_group_reap_drain(pgid, child, stdout_reader, stderr_reader);
                    anyhow::bail!(
                        "git command timed out after {timeout:?}: {}",
                        String::from_utf8_lossy(&stderr).trim()
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                // Don't leak the child or the reader threads on a poll error.
                let _ = kill_group_reap_drain(pgid, child, stdout_reader, stderr_reader);
                return Err(e).context("failed to poll git child");
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command that would run for 30s, bounded to 500ms, must return an error
    /// near the timeout (not after the full 30s) — proving the child is killed
    /// rather than waited on. Uses `sleep` so the test is hermetic (no network)
    /// and deterministic.
    #[test]
    fn git_command_times_out_and_kills_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        let result = run_git_with_timeout(cmd, Duration::from_millis(500));
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a hung command must return an error");
        assert!(
            elapsed < Duration::from_secs(5),
            "must return near the 500ms timeout, not after the full 30s \
             (elapsed: {elapsed:?}) — the child must be killed, not awaited"
        );
    }

    /// Reproduces the network-helper case: the direct child (`sh`) forks a
    /// grandchild that inherits and holds the stderr write-end open, then execs
    /// away. Killing only the direct pid would orphan the grandchild, keep the
    /// pipe open, and hang `stderr_reader.join()` for the grandchild's full 30s.
    /// The process-group kill must take down the grandchild too, so the call
    /// returns near the timeout. This FAILS on single-pid-kill code.
    #[test]
    fn group_kill_reaps_orphaned_helper_holding_the_pipe() {
        // `sleep 30 &` backgrounds a grandchild that keeps fd 2 (stderr) open;
        // `exec sleep 30` replaces the shell so the *direct* child is a plain
        // sleep. Both live in the child's process group.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & exec sleep 30");
        let start = Instant::now();
        let result = run_git_with_timeout(cmd, Duration::from_millis(500));
        let elapsed = start.elapsed();

        assert!(result.is_err(), "the hung group must return an error");
        assert!(
            elapsed < Duration::from_secs(5),
            "must return near the 500ms timeout, not hang on the orphaned \
             helper holding the stderr pipe (elapsed: {elapsed:?})"
        );
    }

    /// A fast command completes normally, with stdout and exit status captured.
    #[test]
    fn run_git_captures_output_and_status() {
        let mut cmd = Command::new("git");
        cmd.arg("--version");
        let out = run_git_with_timeout(cmd, git_net_timeout()).expect("git --version should run");
        assert!(out.status.success(), "git --version should exit 0");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("git version"),
            "stdout should carry the version banner"
        );
    }

    /// Env overrides are honoured; unset falls back to the documented defaults.
    #[test]
    fn timeout_defaults_and_overrides() {
        // Defaults (assuming the env vars are unset in the test environment).
        assert_eq!(git_net_timeout(), Duration::from_secs(60));
        assert_eq!(git_clone_timeout(), Duration::from_secs(600));
        // A bespoke var proves the parse/filter path.
        assert_eq!(
            env_secs("NESTWEAVER_GIT_CMD_TEST_UNSET_XYZ", 42),
            Duration::from_secs(42)
        );
    }
}
