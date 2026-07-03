//! Subprocess execution for `git` with a hard timeout that kills and reaps the
//! child.
//!
//! Server-mode indexing spawns `git` for clone/fetch/ls-remote against arbitrary
//! remotes. A hung remote (network blackhole) must not wedge a worker task or the
//! poll scheduler forever: without a timeout a blocking `Command::output()` blocks
//! indefinitely, so the worker holds its semaphore permit forever (starving the
//! pool) and the poll loop stalls every other repo behind the wedged one.
//!
//! [`run_git_with_timeout`] bounds every invocation. On timeout it sends SIGKILL
//! **and** reaps the zombie (on Unix a child must be reaped by its parent to
//! release the OS process-table slot), then returns an error.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Default timeout for network-touching git operations (clone, fetch,
/// ls-remote). Generous enough for a large blobless clone over a slow link, but
/// bounded so a blackholed remote can't pin a worker permit forever.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the wait loop wakes to check for child exit / timeout. Small enough
/// to stay responsive, large enough not to busy-spin.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run a `git` command (any `Command`, really) with a hard timeout.
///
/// Captures stdout/stderr, draining both on dedicated reader threads so a child
/// that writes more than a pipe buffer can hold never deadlocks. If the child
/// does not exit within `timeout` it is killed (SIGKILL) **and** reaped, and an
/// error is returned — the call never blocks past `timeout` plus one poll
/// interval.
pub fn run_git_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `kill_on_drop` semantics via std: we explicitly kill+wait below. This is a
    // best-effort safety net for early returns (e.g. a `try_wait` error).
    let mut child = cmd.spawn().context("failed to spawn git")?;

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
        match child.try_wait().context("failed to poll git child")? {
            Some(status) => break status,
            None => {
                if start.elapsed() >= timeout {
                    // SIGKILL, then reap the zombie so it doesn't leak a
                    // process-table slot. `kill` is best-effort (the child may
                    // have exited between the poll and here); `wait` reaps.
                    let _ = child.kill();
                    let _ = child.wait();
                    // The reader threads observe EOF once the pipes close on the
                    // child's death, so joining them cannot hang.
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    anyhow::bail!("git command timed out after {timeout:?}");
                }
                std::thread::sleep(POLL_INTERVAL);
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

    /// A fast command completes normally, with stdout and exit status captured.
    #[test]
    fn run_git_captures_output_and_status() {
        let mut cmd = Command::new("git");
        cmd.arg("--version");
        let out = run_git_with_timeout(cmd, GIT_TIMEOUT).expect("git --version should run");
        assert!(out.status.success(), "git --version should exit 0");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("git version"),
            "stdout should carry the version banner"
        );
    }
}
