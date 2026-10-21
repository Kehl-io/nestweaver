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
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
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

/// Harden a `git` invocation against the host's system/global configuration.
///
/// Every server-mode git call clones/fetches *validated* remote URLs, but the
/// host's own git config can subvert that validation AFTER it runs:
/// - a `url.<x>.insteadOf` rewrite could redirect a validated URL to an internal
///   host — and the SSRF IP-pin is keyed to the *original* host:port, so it would
///   not cover the rewrite;
/// - a configured `credential.helper` would be invoked when cloning an
///   attacker-supplied https URL, leaking stored credentials;
/// - an interactive auth prompt could hang the (timeout-bounded, but wasteful)
///   subprocess.
///
/// So we neutralize all of that on every invocation:
/// - `GIT_CONFIG_NOSYSTEM=1` — ignore `/etc/gitconfig`.
/// - `GIT_CONFIG_GLOBAL=/dev/null` — ignore `~/.gitconfig` / XDG global config.
/// - `GIT_TERMINAL_PROMPT=0` — never prompt on the terminal (fail fast instead).
/// - `GIT_CONFIG_COUNT` + `GIT_CONFIG_KEY_0=credential.helper` +
///   `GIT_CONFIG_VALUE_0=` — inject `credential.helper=` (empty), the env-based
///   equivalent of `-c credential.helper=`, which resets/disables any helper the
///   repo-local config might still add. (We use the env form because callers pass
///   a fully-built `Command` whose args already include the git subcommand, so a
///   trailing `-c` arg would land *after* the subcommand and be misparsed.)
/// - `core.fsmonitor=` (empty) and `core.hooksPath=/dev/null` the same way. An
///   indexed repository authors its own `.git/config`, and both name programs
///   Git runs: indexing ran a repository's fsmonitor through `ls-files`. No
///   production call needs either (they read, fetch and clone bare).
/// - `maintenance.auto=false` and `gc.auto=0`: no call should start
///   background maintenance in a repository it only inspects or fetches.
///
/// - `core.askPass=` (empty): an askpass program is run before the terminal
///   prompt, so `GIT_TERMINAL_PROMPT=0` alone does not stop a repository's
///   `core.askPass`. Git skips an empty value.
/// - `log.showSignature=false`: a repository's config would otherwise run
///   `gpg.program` on every `git log`.
/// - `protocol.ext.allow=never`: an `ext::` remote URL is a shell command.
/// - `GIT_PAGER=cat`: outranks `core.pager` and `pager.<cmd>` when a caller
///   leaves stdout on a terminal.
/// - `GIT_SSH_COMMAND=ssh` unless the operator set `GIT_SSH_COMMAND` or
///   `GIT_SSH`: the environment outranks a repository's `core.sshCommand`.
///   (Global config is ignored above, so this changes nothing the operator
///   configured there.)
///
/// Programs a repository names through `.gitattributes` (clean/smudge/process
/// filters, textconv and external diff drivers) are keyed by driver name, so
/// they cannot be disabled here without knowing the repository: use
/// [`isolated_git_in`] for any command that reads the working tree or prints
/// diffs.
///
/// This does NOT touch the caller's `-c` args (the SSRF `http.curloptResolve` /
/// `http.followRedirects` pins and the `http.lowSpeed*` guards): those are
/// command-line args on the `Command` and take precedence over config anyway.
///
/// CALLER CONTRACT: callers must NOT set their own `GIT_CONFIG_*` env on the
/// `Command`. This sets `GIT_CONFIG_COUNT` (plus `KEY_n`/`VALUE_n`)
/// unconditionally, so a caller-supplied entry would either be clobbered or
/// silently ignored — pass config via `-c` args instead. Applying it twice is
/// harmless and keeps the per-repository overrides [`isolated_git_in`] added.
pub fn apply_git_isolation(cmd: &mut Command) {
    apply_isolation_with(cmd, &[]);
}

/// Config every isolated invocation sets, in `GIT_CONFIG_KEY_n` order.
const ISOLATION_CONFIG: &[(&str, &str)] = &[
    ("credential.helper", ""),
    // Empty, not `false`: before Git 2.36 the value is a hook path, and
    // `false` would run /bin/false. Empty disables it on every version.
    ("core.fsmonitor", ""),
    ("core.hooksPath", "/dev/null"),
    // `fetch` starts auto-maintenance, whose worktree-prune task (Git 2.5x)
    // follows a symlink planted in `worktrees/` and deletes the repository
    // it points to.
    ("maintenance.auto", "false"),
    ("gc.auto", "0"),
    ("core.askPass", ""),
    ("log.showSignature", "false"),
    ("protocol.ext.allow", "never"),
];

fn apply_isolation_with(cmd: &mut Command, extra: &[(OsString, OsString)]) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_PAGER", "cat");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh");
    }
    // A second application (`run_git_with_timeout` re-applies isolation to
    // every command) must not drop overrides an earlier one added.
    if extra.is_empty() && isolation_config_count(cmd).is_some() {
        return;
    }
    let mut index = 0usize;
    for (key, value) in ISOLATION_CONFIG
        .iter()
        .map(|(key, value)| (OsStr::new(key), OsStr::new(value)))
        .chain(
            extra
                .iter()
                .map(|(key, value)| (key.as_os_str(), value.as_os_str())),
        )
    {
        cmd.env(format!("GIT_CONFIG_KEY_{index}"), key);
        cmd.env(format!("GIT_CONFIG_VALUE_{index}"), value);
        index += 1;
    }
    cmd.env("GIT_CONFIG_COUNT", index.to_string());
}

/// The `GIT_CONFIG_COUNT` an earlier isolation set on `cmd`, if any.
fn isolation_config_count(cmd: &Command) -> Option<usize> {
    let mut count = None;
    let mut first_key_is_ours = false;
    for (key, value) in cmd.get_envs() {
        let value = value.and_then(|value| value.to_str());
        if key == "GIT_CONFIG_COUNT" {
            count = value.and_then(|value| value.parse().ok());
        } else if key == "GIT_CONFIG_KEY_0" {
            first_key_is_ours = value == Some(ISOLATION_CONFIG[0].0);
        }
    }
    count.filter(|count| *count >= ISOLATION_CONFIG.len() && first_key_is_ours)
}

/// A `git` command with [`apply_git_isolation`] already applied, for local
/// reads of a repository NestWeaver did not write that never look at the
/// working tree's contents (`rev-parse`, `ls-files`, `log` without patches,
/// `cat-file`). Use [`isolated_git_in`] for `status`, `diff` and anything
/// else that may run a clean filter, textconv or external diff.
pub fn isolated_git() -> Command {
    let mut cmd = Command::new("git");
    apply_git_isolation(&mut cmd);
    cmd
}

/// An isolated `git` command for `repo` that also disables every program the
/// repository's attributes can select.
///
/// `.gitattributes` and `$GIT_DIR/info/attributes` map paths to driver names
/// (`filter=x`, `diff=x`, `merge=x`), and the repository's config maps those
/// names to programs: `git status` and `git diff` run `filter.x.clean` (or
/// `.process`) to compare a working-tree file, and patch output runs
/// `diff.x.textconv` / `diff.x.command`. Attributes cannot be overridden
/// (`$GIT_DIR/info/attributes` outranks everything, and `GIT_ATTR_SOURCE`
/// needs Git 2.40 and leaves that file in force), but config can: this lists
/// the repository's own driver definitions without running anything
/// (`git config --get-regexp`, includes followed exactly as the real command
/// will follow them) and resets each program-naming key to empty, which Git
/// treats as "no program". `filter.x.required` is reset to `false` so a
/// disabled filter does not abort the command.
///
/// Effect on results: a working-tree file whose filter would have made it
/// match the index can read as modified. A patch diff of a path with a
/// textconv or external driver fails instead of running it, so callers that
/// print patches should also pass `--no-textconv --no-ext-diff`.
///
/// The caller still sets `current_dir(repo)` (or `-C repo`): this only
/// inspects `repo`. Fails when the configuration cannot be listed, rather
/// than returning a command that might run those programs.
pub fn isolated_git_in(repo: &Path) -> Result<Command> {
    let mut query = Command::new("git");
    query.current_dir(repo);
    let overrides = attribute_program_overrides(query)?;
    let mut cmd = Command::new("git");
    apply_isolation_with(&mut cmd, &overrides);
    Ok(cmd)
}

/// Config keys that name a program an attribute can select, as `-c`-style
/// overrides that disable it. `query` is a `git` command already pointed at
/// the repository (`current_dir` or `-C`); `config --get-regexp` is appended
/// and run isolated with a bounded output.
pub fn attribute_program_overrides(mut query: Command) -> Result<Vec<(OsString, OsString)>> {
    query.args([
        "config",
        "--null",
        "--get-regexp",
        r"^(filter|diff|merge)\.",
    ]);
    let output = run_git_with_timeout_and_output_limit(query, git_net_timeout(), 1024 * 1024)
        .context("cannot list the repository's attribute drivers")?;
    // Exit 1: no key matches. A repository without drivers runs nothing.
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    if !output.status.success() {
        anyhow::bail!(
            "cannot list the repository's attribute drivers: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_attribute_driver_config(&output.stdout))
}

/// Overrides for the `key\nvalue\0` records of `git config --null`.
///
/// Works on bytes: a driver name is whatever the repository wrote, and an
/// override must name it exactly or the program still runs.
fn parse_attribute_driver_config(stdout: &[u8]) -> Vec<(OsString, OsString)> {
    let mut overrides = std::collections::BTreeSet::new();
    let pair = |parts: &[&[u8]], value: &str| {
        (
            OsString::from_vec(parts.concat()),
            OsString::from(value.to_string()),
        )
    };
    for record in stdout.split(|byte| *byte == 0) {
        let key = record.split(|byte| *byte == b'\n').next().unwrap_or(record);
        let Some(dot) = key.iter().position(|byte| *byte == b'.') else {
            continue;
        };
        let section = key[..dot].to_ascii_lowercase();
        let rest = &key[dot + 1..];
        let Some(last) = rest.iter().rposition(|byte| *byte == b'.') else {
            // Two-level keys: only `diff.external` names a program.
            if section == b"diff" && rest.eq_ignore_ascii_case(b"external") {
                overrides.insert(pair(&[b"diff.external"], ""));
            }
            continue;
        };
        let driver = &rest[..last];
        let variable = rest[last + 1..].to_ascii_lowercase();
        let prefix: &[u8] = &section;
        match (prefix, variable.as_slice()) {
            (b"filter", _) => {
                for name in [&b".clean"[..], b".smudge", b".process"] {
                    overrides.insert(pair(&[b"filter.", driver, name], ""));
                }
                overrides.insert(pair(&[b"filter.", driver, b".required"], "false"));
            }
            (b"diff", b"textconv" | b"command") | (b"merge", b"driver") => {
                overrides.insert(pair(&[prefix, b".", driver, b".", &variable], ""));
            }
            _ => {}
        }
    }
    overrides.into_iter().collect()
}

/// Refuse a revision a caller would splice into a git argument, when git
/// would parse it as an option instead (`--output=<file>` writes a file).
pub fn ensure_revision_arg(revision: &str) -> Result<()> {
    if revision.starts_with('-') {
        anyhow::bail!("'{revision}' is not a revision: it starts with '-'");
    }
    Ok(())
}

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
pub fn run_git_with_timeout(cmd: Command, timeout: Duration) -> Result<Output> {
    run_git_with_output_limit(cmd, timeout, None)
}

/// Bound each output pipe before allocation, failing rather than returning a
/// truncated inventory. The process-group cleanup is identical to timeout.
pub fn run_git_with_timeout_and_output_limit(
    cmd: Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<Output> {
    run_git_with_output_limit(cmd, timeout, Some(max_output_bytes))
}

fn run_git_with_output_limit(
    mut cmd: Command,
    timeout: Duration,
    max_output_bytes: Option<usize>,
) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Isolate every git invocation from the host's system/global config and
    // credential helpers (url.insteadOf rewrites, credential leakage, auth
    // prompts) — the one central place all server-mode git calls route through.
    apply_git_isolation(&mut cmd);
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
    let exceeded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let read_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_failed = read_failed.clone();
    let stdout_exceeded = exceeded.clone();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut s) = stdout_pipe {
            let limit = max_output_bytes.map_or(u64::MAX, |n| n.saturating_add(1) as u64);
            if s.take(limit).read_to_end(&mut buf).is_err() && max_output_bytes.is_some() {
                stdout_failed.store(true, std::sync::atomic::Ordering::Release);
            }
            if max_output_bytes.is_some_and(|max| buf.len() > max) {
                stdout_exceeded.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        buf
    });
    let stderr_failed = read_failed.clone();
    let stderr_exceeded = exceeded.clone();
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut s) = stderr_pipe {
            let limit = max_output_bytes.map_or(u64::MAX, |n| n.saturating_add(1) as u64);
            if s.take(limit).read_to_end(&mut buf).is_err() && max_output_bytes.is_some() {
                stderr_failed.store(true, std::sync::atomic::Ordering::Release);
            }
            if max_output_bytes.is_some_and(|max| buf.len() > max) {
                stderr_exceeded.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        if read_failed.load(std::sync::atomic::Ordering::Acquire) {
            let _ = kill_group_reap_drain(pgid, child, stdout_reader, stderr_reader);
            anyhow::bail!("failed to read bounded git output");
        }
        if exceeded.load(std::sync::atomic::Ordering::Acquire) {
            let _ = kill_group_reap_drain(pgid, child, stdout_reader, stderr_reader);
            anyhow::bail!("git output exceeded its bounded capture limit");
        }
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
    anyhow::ensure!(
        !exceeded.load(std::sync::atomic::Ordering::Acquire),
        "git output exceeded its bounded capture limit"
    );
    anyhow::ensure!(
        !read_failed.load(std::sync::atomic::Ordering::Acquire),
        "failed to read bounded git output"
    );
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// A repository whose config and attributes name a marker program for every
/// attribute-driven hook: clean/smudge/process filters, textconv and external
/// diff, each reached through `.gitattributes`, `$GIT_DIR/info/attributes`
/// and an included config file. Tests across the crate assert that a code
/// path under test leaves the marker directory empty.
#[cfg(test)]
pub(crate) mod hostile_repo {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub(crate) struct HostileRepo {
        pub(crate) repo: tempfile::TempDir,
        pub(crate) markers: tempfile::TempDir,
    }

    impl HostileRepo {
        pub(crate) fn path(&self) -> &Path {
            self.repo.path()
        }

        /// The names of every marker program that ran.
        pub(crate) fn ran(&self) -> Vec<String> {
            let mut ran: Vec<String> = std::fs::read_dir(self.markers.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("ran-"))
                .collect();
            ran.sort();
            ran
        }

        pub(crate) fn clear(&self) {
            for name in self.ran() {
                std::fs::remove_file(self.markers.path().join(name)).unwrap();
            }
        }

        /// A plain, unisolated git (apart from the host's own config).
        pub(crate) fn plain_git(&self, args: &[&str]) -> std::process::Output {
            Command::new("git")
                .args(args)
                .current_dir(self.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
        }
    }

    fn script(markers: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = markers.join(format!("{name}.sh"));
        // A pass-through: textconv and external diff take file arguments,
        // filters stream stdin to stdout.
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ntouch '{}'\nif [ $# -gt 0 ]; then exec cat \"$1\" </dev/null; else exec cat; fi\n",
                markers.join(format!("ran-{name}")).display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Committed `a.rs`, `b.rs`, `c.rs`, then each modified in the working
    /// tree (`a.rs` keeps its size so only content comparison can tell).
    pub(crate) fn hostile_repo() -> HostileRepo {
        let repo = tempfile::tempdir().unwrap();
        let markers = tempfile::tempdir().unwrap();
        let hostile = HostileRepo { repo, markers };
        let git = |args: &[&str]| {
            let output = hostile.plain_git(args);
            assert!(output.status.success(), "{args:?}: {output:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "core.hooksPath", "/dev/null"]);
        let root = hostile.path();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(root.join("c.rs"), "fn c() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        // Attributes from the tree, from info/attributes, and for a driver
        // defined only in an included file.
        std::fs::write(root.join(".gitattributes"), "a.rs filter=evil diff=evil\n").unwrap();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(
            root.join(".git/info/attributes"),
            "b.rs filter=Ev.il diff=Ev.il\nc.rs filter=inc diff=inc\n",
        )
        .unwrap();
        let markers = hostile.markers.path();
        let (clean, smudge, process, textconv, extdiff) = (
            script(markers, "clean"),
            script(markers, "smudge"),
            script(markers, "process"),
            script(markers, "textconv"),
            script(markers, "extdiff"),
        );
        for driver in ["evil", "Ev.il"] {
            git(&["config", &format!("filter.{driver}.clean"), clean.to_str().unwrap()]);
            git(&["config", &format!("filter.{driver}.smudge"), smudge.to_str().unwrap()]);
            git(&["config", &format!("filter.{driver}.required"), "true"]);
            git(&["config", &format!("diff.{driver}.textconv"), textconv.to_str().unwrap()]);
            git(&["config", &format!("diff.{driver}.command"), extdiff.to_str().unwrap()]);
        }
        let included = markers.join("included.config");
        std::fs::write(
            &included,
            format!(
                "[filter \"inc\"]\n\tprocess = {}\n[diff \"inc\"]\n\ttextconv = {}\n[diff]\n\texternal = {}\n",
                process.display(),
                textconv.display(),
                extdiff.display()
            ),
        )
        .unwrap();
        git(&["config", "include.path", included.to_str().unwrap()]);
        // Same sizes, new contents: a stat-dirty comparison must hash them.
        std::fs::write(root.join("a.rs"), "fn x() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn y() {}\n").unwrap();
        std::fs::write(root.join("c.rs"), "fn z() {}\n").unwrap();
        hostile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository's own config names programs Git would run. Plain Git runs
    /// them; an isolated invocation must not.
    #[test]
    fn isolation_disables_repository_fsmonitor_and_hooks() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempfile::tempdir().unwrap();
        let markers = tempfile::tempdir().unwrap();
        let plain = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
        };
        plain(&["init", "-q"]);
        plain(&["config", "user.email", "test@example.com"]);
        plain(&["config", "user.name", "Test"]);
        plain(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        plain(&["add", "a.txt"]);
        plain(&["commit", "-q", "-m", "init"]);
        let fsmonitor = markers.path().join("fsmonitor");
        let hooks = markers.path().join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        let script = |path: &std::path::Path, marker: &str| {
            std::fs::write(
                path,
                format!(
                    "#!/bin/sh\ntouch '{}'\n",
                    markers.path().join(marker).display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        script(&fsmonitor, "fsmonitor-ran");
        script(&hooks.join("pre-commit"), "hook-ran");
        plain(&["config", "core.fsmonitor", fsmonitor.to_str().unwrap()]);
        plain(&["config", "core.hooksPath", hooks.to_str().unwrap()]);

        plain(&["ls-files", "-z", "--cached"]);
        plain(&["commit", "-q", "--allow-empty", "-m", "plain"]);
        assert!(
            markers.path().join("fsmonitor-ran").exists(),
            "baseline fsmonitor"
        );
        assert!(markers.path().join("hook-ran").exists(), "baseline hook");
        std::fs::remove_file(markers.path().join("fsmonitor-ran")).unwrap();
        std::fs::remove_file(markers.path().join("hook-ran")).unwrap();

        for args in [
            &["ls-files", "-z", "--cached"][..],
            &["commit", "-q", "--allow-empty", "-m", "isolated"][..],
        ] {
            let mut command = Command::new("git");
            command.args(args).current_dir(repo.path());
            let output = run_git_with_timeout(command, Duration::from_secs(30)).unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
        }
        assert!(
            !markers.path().join("fsmonitor-ran").exists(),
            "fsmonitor ran"
        );
        assert!(!markers.path().join("hook-ran").exists(), "hook ran");
    }

    /// The fixture really does run every program through plain Git, so the
    /// isolation tests below cannot pass vacuously.
    #[test]
    fn hostile_repo_runs_programs_without_isolation() {
        let repo = hostile_repo::hostile_repo();
        repo.plain_git(&["status", "--porcelain"]);
        repo.plain_git(&["diff", "HEAD"]);
        repo.plain_git(&["diff", "--stat", "HEAD"]);
        let ran = repo.ran();
        for program in ["ran-clean", "ran-extdiff", "ran-process"] {
            assert!(ran.iter().any(|name| name == program), "{program}: {ran:?}");
        }
        repo.clear();
        repo.plain_git(&["add", "-A"]);
        repo.plain_git(&["commit", "-q", "-m", "two"]);
        repo.clear();
        repo.plain_git(&["log", "-1", "-p", "--no-ext-diff"]);
        assert!(repo.ran().iter().any(|name| name == "ran-textconv"));
    }

    /// No attribute-selected program runs through `isolated_git_in`, for
    /// status, name-only/stat/patch diffs, patch logs and checkouts.
    #[test]
    fn isolated_git_in_runs_no_attribute_programs() {
        let repo = hostile_repo::hostile_repo();
        let run = |args: &[&str]| {
            let mut command = isolated_git_in(repo.path()).unwrap();
            command.args(args).current_dir(repo.path());
            run_git_with_timeout(command, Duration::from_secs(30)).unwrap()
        };
        let status = run(&["status", "--porcelain", "-z", "--untracked-files=all"]);
        assert!(status.status.success(), "{status:?}");
        let names = run(&["diff", "--name-only", "-z", "HEAD", "--"]);
        assert!(names.status.success(), "{names:?}");
        let listed = String::from_utf8_lossy(&names.stdout).into_owned();
        for file in ["a.rs", "b.rs", "c.rs"] {
            assert!(listed.contains(file), "{file} must read as changed: {listed:?}");
        }
        run(&["diff", "--stat", "HEAD"]);
        run(&["diff", "--numstat", "HEAD"]);
        run(&["diff", "HEAD"]);
        let commit = run(&["commit", "-q", "-a", "-m", "two"]);
        assert!(commit.status.success(), "{commit:?}");
        run(&["log", "-2", "-p"]);
        run(&["show", "HEAD"]);
        run(&["diff", "HEAD~1", "HEAD"]);
        run(&["checkout", "-q", "HEAD~1", "--", "a.rs"]);
        assert_eq!(repo.ran(), Vec::<String>::new());
    }

    #[test]
    fn attribute_driver_overrides_name_each_program_exactly() {
        let parsed = parse_attribute_driver_config(
            b"filter.Ev.il.clean\n/x\0diff.d.textconv\n/y\0diff.d.xfuncname\n^f\0\
              diff.external\n/z\0merge.m.driver\n/w\0diff.other.binary\ntrue\0",
        );
        let parsed: Vec<(String, String)> = parsed
            .into_iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = [
            ("diff.d.textconv", ""),
            ("diff.external", ""),
            ("filter.Ev.il.clean", ""),
            ("filter.Ev.il.process", ""),
            ("filter.Ev.il.required", "false"),
            ("filter.Ev.il.smudge", ""),
            ("merge.m.driver", ""),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        assert_eq!(parsed, expected);
    }

    /// `run_git_with_timeout` re-applies isolation; that must keep the
    /// per-repository overrides `isolated_git_in` added.
    #[test]
    fn reapplying_isolation_keeps_repository_overrides() {
        let mut command = Command::new("git");
        apply_isolation_with(
            &mut command,
            &[(OsString::from("filter.x.clean"), OsString::new())],
        );
        apply_git_isolation(&mut command);
        let count = command
            .get_envs()
            .find(|(key, _)| *key == "GIT_CONFIG_COUNT")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(count.as_deref(), Some("9"));
    }

    #[test]
    fn bounded_git_output_fails_without_returning_truncated_success() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "while :; do printf 0123456789; done"]);
        let error =
            run_git_with_timeout_and_output_limit(cmd, Duration::from_secs(5), 1024).unwrap_err();
        assert!(error.to_string().contains("output exceeded"));
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf exact"]);
        let output = run_git_with_timeout_and_output_limit(cmd, Duration::from_secs(5), 5).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"exact");
    }

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

    /// Every git invocation must be isolated from the host's system/global
    /// config and credential helpers. Assert the hardening env is present on the
    /// built command (inspecting `get_envs`), including the `GIT_CONFIG_*` trio
    /// that injects an empty `credential.helper` (disables helpers).
    #[test]
    fn git_isolation_env_is_applied() {
        use std::collections::HashMap;
        use std::ffi::OsStr;

        let mut cmd = Command::new("git");
        apply_git_isolation(&mut cmd);

        let envs: HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        let get = |k: &str| {
            envs.get(OsStr::new(k))
                .copied()
                .flatten()
                .map(|v| v.to_string_lossy().into_owned())
        };

        assert_eq!(get("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
        assert_eq!(get("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
        assert_eq!(get("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
        // credential.helper= (empty) injected via the GIT_CONFIG_* env form —
        // the equivalent of `-c credential.helper=`, disabling all helpers.
        assert_eq!(get("GIT_CONFIG_COUNT").as_deref(), Some("8"));
        assert_eq!(
            get("GIT_CONFIG_KEY_0").as_deref(),
            Some("credential.helper")
        );
        assert_eq!(
            get("GIT_CONFIG_VALUE_0").as_deref(),
            Some(""),
            "credential.helper must be reset to empty to disable helpers"
        );
        // Repository config cannot name programs Git runs during inspection.
        assert_eq!(get("GIT_CONFIG_KEY_1").as_deref(), Some("core.fsmonitor"));
        assert_eq!(get("GIT_CONFIG_VALUE_1").as_deref(), Some(""));
        assert_eq!(get("GIT_CONFIG_KEY_2").as_deref(), Some("core.hooksPath"));
        assert_eq!(get("GIT_CONFIG_VALUE_2").as_deref(), Some("/dev/null"));
        assert_eq!(get("GIT_CONFIG_KEY_3").as_deref(), Some("maintenance.auto"));
        assert_eq!(get("GIT_CONFIG_VALUE_3").as_deref(), Some("false"));
        assert_eq!(get("GIT_CONFIG_KEY_4").as_deref(), Some("gc.auto"));
        assert_eq!(get("GIT_CONFIG_VALUE_4").as_deref(), Some("0"));
        assert_eq!(get("GIT_CONFIG_KEY_5").as_deref(), Some("core.askPass"));
        assert_eq!(get("GIT_CONFIG_VALUE_5").as_deref(), Some(""));
        assert_eq!(
            get("GIT_CONFIG_KEY_6").as_deref(),
            Some("log.showSignature")
        );
        assert_eq!(get("GIT_CONFIG_VALUE_6").as_deref(), Some("false"));
        assert_eq!(
            get("GIT_CONFIG_KEY_7").as_deref(),
            Some("protocol.ext.allow")
        );
        assert_eq!(get("GIT_CONFIG_VALUE_7").as_deref(), Some("never"));
        assert_eq!(get("GIT_PAGER").as_deref(), Some("cat"));
    }

    /// The isolation must not clobber a caller's own `-c` args (the SSRF pins and
    /// http.lowSpeed guards): those live as command-line args, separate from the
    /// env hardening. Assert both coexist on the built command.
    #[test]
    fn isolation_preserves_caller_config_args() {
        let mut cmd = Command::new("git");
        cmd.args([
            "-c",
            "http.curloptResolve=example.com:443:93.184.216.34",
            "-c",
            "http.followRedirects=false",
        ]);
        cmd.args(["ls-remote", "origin", "HEAD"]);
        apply_git_isolation(&mut cmd);

        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2).any(
                |w| w[0] == "-c" && w[1] == "http.curloptResolve=example.com:443:93.184.216.34"
            ),
            "SSRF curloptResolve pin arg must be preserved: {args:?}"
        );
        assert!(
            args.contains(&"http.followRedirects=false".to_string()),
            "SSRF followRedirects arg must be preserved: {args:?}"
        );
        // And the hardening env is still applied alongside the args.
        let has_nosystem = cmd
            .get_envs()
            .any(|(k, v)| k == std::ffi::OsStr::new("GIT_CONFIG_NOSYSTEM") && v.is_some());
        assert!(has_nosystem, "isolation env must coexist with -c args");
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
