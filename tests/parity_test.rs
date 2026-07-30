//! Integration tests for two bug classes fixed in the blast-radius trust batch:
//!
//! A. CLI→daemon arg-name contract tests — the CLI used to send wrong argument
//!    names to the daemon (`cluster` sent `id_or_name` instead of
//!    `cluster_id`, `bridges` sent `top` instead of `top_n`,
//!    `affected-tests --files` sent `files` instead of `changed_files`).
//!    These tests run the CLI against a LIVE daemon and assert success with
//!    non-empty, non-error output.
//!
//! B. Daemon-vs-direct output parity tests — for each command,
//!    stdout must be byte-identical between direct mode
//!    (`NESTWEAVER_NO_DAEMON=1`) and daemon mode, in both human and `--json`
//!    output, and exit codes must match across modes.
//!
//! Ordering matters: all DIRECT runs happen before the daemon is started (a
//! running daemon may hold the DB lock). All commands exercised here are
//! read-only, so the DB does not change between runs.
//!
//! Run with:
//!   cargo test --test parity_test

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};
use std::time::Duration;
use tempfile::TempDir;

// ─── Helpers (mirrors tests/daemon_test.rs — each tests/*.rs is its own crate,
// so the minimal helpers are duplicated here per project convention) ─────────

/// Helper: build a `Command` for the `nestweaver` binary **without** setting
/// `NESTWEAVER_NO_DAEMON`. This exercises the daemon path.
fn daemon_cmd() -> Command {
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    cmd.env_remove("NESTWEAVER_NO_DAEMON");
    cmd.env_remove("NESTWEAVER_ALLOW_NO_DAEMON");
    #[cfg(not(target_os = "macos"))]
    cmd.env("NESTWEAVER_DAEMON_FORK", "1");
    cmd
}

/// Helper: build a `Command` with `NESTWEAVER_NO_DAEMON=1` for direct-mode
/// (no daemon) execution.
fn no_daemon_cmd() -> Command {
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    cmd.env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1");
    cmd
}

/// Build a daemon subcommand with the correct arg order:
///   `nestweaver daemon --db <path> <action>`
fn daemon_action_cmd(db_path: &Path, action: &str) -> Command {
    let mut cmd = daemon_cmd();
    cmd.args(["daemon", "--db", &db_path.display().to_string(), action]);
    cmd
}

/// Start a daemon and wait until its Unix socket is accepting connections.
fn start_daemon(db_path: &Path) {
    daemon_action_cmd(db_path, "start").assert().success();

    let instance_id = nestweaver_daemon::instance_id_from_db_path(db_path);
    let socket = nestweaver_daemon::socket_path(&instance_id);
    let readiness = wait_for_daemon_readiness(
        Duration::from_secs(10),
        Duration::from_millis(25),
        || std::os::unix::net::UnixStream::connect(&socket).map(drop),
        || stop_daemon(db_path),
    );
    let Err(last_error) = readiness else {
        return;
    };

    let log_path = nestweaver_daemon::log_path(&instance_id);
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|error| format!("<could not read {}: {error}>", log_path.display()));
    panic!(
        "daemon socket {} did not accept connections within 10s (last error: {}); log:\n{}",
        socket.display(),
        last_error,
        log
    );
}

fn wait_for_daemon_readiness(
    timeout: Duration,
    retry_interval: Duration,
    mut connect: impl FnMut() -> std::io::Result<()>,
    cleanup: impl FnOnce(),
) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match connect() {
            Ok(()) => return Ok(()),
            Err(error) if std::time::Instant::now() >= deadline => {
                cleanup();
                return Err(error);
            }
            Err(_) => std::thread::sleep(retry_interval),
        }
    }
}

/// Stop the daemon for a given DB path, ignoring errors (best-effort cleanup).
fn stop_daemon(db_path: &Path) {
    let _ = daemon_action_cmd(db_path, "stop").ok();
}

/// RAII guard that stops the daemon on drop — ensures cleanup even on panic.
struct DaemonGuard {
    db_path: PathBuf,
}

impl DaemonGuard {
    fn new(db_path: &Path) -> Self {
        Self {
            db_path: db_path.to_path_buf(),
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        stop_daemon(&self.db_path);
    }
}

/// Resolve the path to the built `nestweaver` binary.
fn bin_path() -> PathBuf {
    assert_cmd::cargo::cargo_bin("nestweaver")
}

/// Create a git repo populated with the given `(relative_path, contents)` files.
fn write_repo_files(dir: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    StdCommand::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    for (relative_path, contents) in files {
        let path = dir.join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
    StdCommand::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .output()
        .unwrap();
    StdCommand::new("git")
        .args([
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "init",
        ])
        .current_dir(dir)
        .output()
        .unwrap();
}

/// Index a repo, creating the DB. Uses `NESTWEAVER_NO_DAEMON=1`.
fn create_db(repo_dir: &Path, db_path: &Path) {
    no_daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();
}

// ─── Fixture ─────────────────────────────────────────────────────────────────

/// Changed-file list (repo-relative) used by the `affected-tests` tests.
const CHANGED_FILES: &str = "src/a.js,lib/b.js";

struct Fixture {
    // Keeps the tempdir alive for the duration of the test.
    _dir: TempDir,
    db_path: PathBuf,
}

/// Create a scratch DB indexing a small JS repo with enough cross-directory
/// calls to yield at least one cluster and one bridge node.
fn setup_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("db").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    write_repo_files(
        &repo_dir,
        &[
            (
                "src/a.js",
                "import { helperB } from '../lib/b.js';\n\
                 import { helperC } from '../lib/c.js';\n\
                 export function mainA(x) { return helperB(x) + helperC(x); }\n\
                 export function utilA(y) { return helperB(y) * 2; }\n",
            ),
            (
                "lib/b.js",
                "import { helperC } from './c.js';\n\
                 export function helperB(n) { return helperC(n) + 1; }\n\
                 export function extraB(n) { return n - 1; }\n",
            ),
            (
                "lib/c.js",
                "export function helperC(n) { return n * 3; }\n\
                 export function otherC(n) { return n + 7; }\n",
            ),
            (
                "tests/a.test.js",
                "import { mainA } from '../src/a.js';\n\
                 export function testMainA() { return mainA(1); }\n",
            ),
        ],
    );
    create_db(&repo_dir, &db_path);

    Fixture { _dir: dir, db_path }
}

// ─── Mode runners ────────────────────────────────────────────────────────────

/// Run the CLI in DIRECT mode (no daemon), returning the raw process output.
fn run_direct(db_path: &Path, args: &[&str]) -> Output {
    let mut cmd = StdCommand::new(bin_path());
    cmd.env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1");
    cmd.args(args)
        .arg("--db")
        .arg(db_path.display().to_string());
    cmd.output().expect("failed to run nestweaver (direct)")
}

/// Run the CLI in DAEMON mode (daemon must already be running for `db_path`).
fn run_via_daemon(db_path: &Path, args: &[&str]) -> Output {
    let mut cmd = StdCommand::new(bin_path());
    cmd.env_remove("NESTWEAVER_NO_DAEMON")
        .env_remove("NESTWEAVER_ALLOW_NO_DAEMON");
    #[cfg(not(target_os = "macos"))]
    cmd.env("NESTWEAVER_DAEMON_FORK", "1");
    cmd.args(args)
        .arg("--db")
        .arg(db_path.display().to_string());
    cmd.output().expect("failed to run nestweaver (daemon)")
}

// ─── Assertions ──────────────────────────────────────────────────────────────

/// Contract assertion: exit 0 with non-empty, non-error stdout.
fn assert_successful_output(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}: expected exit 0, got {:?}; stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "{context}: expected non-empty stdout"
    );
    assert!(
        !stdout.to_lowercase().contains("error"),
        "{context}: stdout looks like an error response:\n{stdout}"
    );
}

/// Parity assertion: byte-identical stdout and equal exit codes across modes.
fn assert_parity(command: &str, mode_label: &str, direct: &Output, daemon: &Output) {
    assert_eq!(
        direct.status.code(),
        daemon.status.code(),
        "{command} ({mode_label}): exit code diverged — direct={:?} daemon={:?}\n\
         direct stderr:\n{}\ndaemon stderr:\n{}",
        direct.status.code(),
        daemon.status.code(),
        String::from_utf8_lossy(&direct.stderr),
        String::from_utf8_lossy(&daemon.stderr)
    );
    assert_eq!(
        direct.stdout,
        daemon.stdout,
        "{command} ({mode_label}): stdout diverged between direct and daemon mode\n\
         --- direct ---\n{}\n--- daemon ---\n{}",
        String::from_utf8_lossy(&direct.stdout),
        String::from_utf8_lossy(&daemon.stdout)
    );
}

/// Run one command in both output formats, direct mode FIRST (before the
/// daemon starts, since a running daemon may hold the DB lock), then daemon
/// mode, and assert byte-identical stdout + equal exit codes for both formats.
fn check_parity(db_path: &Path, command: &str, args: &[&str]) {
    let mut json_args: Vec<&str> = args.to_vec();
    json_args.push("--json");

    // Direct mode first — daemon not started yet.
    let direct_human = run_direct(db_path, args);
    let direct_json = run_direct(db_path, &json_args);

    let _guard = DaemonGuard::new(db_path);
    start_daemon(db_path);

    let daemon_human = run_via_daemon(db_path, args);
    let daemon_json = run_via_daemon(db_path, &json_args);

    assert_parity(command, "human", &direct_human, &daemon_human);
    assert_parity(command, "json", &direct_json, &daemon_json);
}

// ─── A. CLI→daemon arg-name contract tests ───────────────────────────────────

/// `cluster` must send `cluster_id` (not `id_or_name`) to the daemon: looking
/// up a cluster by its numeric ID through a live daemon must succeed.
#[test]
fn contract_cluster_by_numeric_id_via_daemon() {
    let fixture = setup_fixture();
    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);

    // Discover a real cluster ID from `clusters --json` on the same DB.
    let clusters = run_via_daemon(&fixture.db_path, &["clusters", "--json"]);
    assert_successful_output(&clusters, "clusters --json (setup)");
    let parsed: serde_json::Value =
        serde_json::from_slice(&clusters.stdout).expect("clusters --json must be valid JSON");
    let cluster_id = parsed["communities"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["id"].as_u64())
        .expect("fixture must produce at least one cluster");

    let id_arg = cluster_id.to_string();
    let output = run_via_daemon(&fixture.db_path, &["cluster", &id_arg]);
    assert_successful_output(&output, "cluster <numeric-id> via daemon");
}

/// Same contract as above, but looking the cluster up by name.
#[test]
fn contract_cluster_by_name_via_daemon() {
    let fixture = setup_fixture();
    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);

    let clusters = run_via_daemon(&fixture.db_path, &["clusters", "--json"]);
    assert_successful_output(&clusters, "clusters --json (setup)");
    let parsed: serde_json::Value =
        serde_json::from_slice(&clusters.stdout).expect("clusters --json must be valid JSON");
    let name = parsed["communities"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["name"].as_str())
        .expect("fixture must produce at least one cluster")
        .to_string();

    let output = run_via_daemon(&fixture.db_path, &["cluster", &name]);
    assert_successful_output(&output, "cluster <name> via daemon");
}

/// `bridges` must send `top_n` (not `top`) to the daemon.
#[test]
fn contract_bridges_top_via_daemon() {
    let fixture = setup_fixture();
    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);

    let output = run_via_daemon(&fixture.db_path, &["bridges", "--top", "3"]);
    assert_successful_output(&output, "bridges --top 3 via daemon");
}

/// `affected-tests --files` must send `changed_files` (not `files`) to the daemon.
#[test]
fn contract_affected_tests_files_via_daemon() {
    let fixture = setup_fixture();
    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);

    let output = run_via_daemon(
        &fixture.db_path,
        &["affected-tests", "--files", CHANGED_FILES],
    );
    assert_successful_output(&output, "affected-tests --files via daemon");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("tests/a.test.js"),
        "affected-tests should map src/a.js,lib/b.js to tests/a.test.js:\n{stdout}"
    );
}

// ─── B. Daemon-vs-direct output parity tests ─────────────────────────

#[test]
fn parity_count_patterns_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "count-patterns",
        &["count-patterns", "helper"],
    );
}

#[test]
fn parity_regex_search_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "regex-search",
        &["regex-search", "helper"],
    );
}

#[test]
fn parity_hubs_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(&fixture.db_path, "hubs", &["hubs", "--top", "3"]);
}

#[test]
fn parity_bridges_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(&fixture.db_path, "bridges", &["bridges", "--top", "3"]);
}

#[test]
fn parity_clusters_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(&fixture.db_path, "clusters", &["clusters"]);
}

#[test]
fn parity_affected_tests_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "affected-tests",
        &["affected-tests", "--files", CHANGED_FILES],
    );
}

/// nw-108: `dead-code`'s daemon branch printed the RPC response verbatim with
/// no `if json` guard, so the OUTPUT FORMAT depended on whether a daemon
/// happened to be running rather than on the flag — text standalone, JSON once
/// the daemon was up. The text renderer was never missing; it was simply not
/// reached. This is the nw-097 divergence family, and the reason it survived is
/// that `dead-code` was not in this file.
/// Replace generated bundle IDs with a fixed placeholder.
///
/// `investigate` mints a fresh `bndl_<hex>` per invocation, so its stdout is
/// never byte-identical across two runs — comparing raw bytes would test the ID
/// generator, not the renderer.
fn redact_bundle_ids(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut rest: &str = &text;
    while let Some(pos) = rest.find("bndl_") {
        out.push_str(&rest[..pos]);
        out.push_str("bndl_<redacted>");
        let after = &rest[pos + "bndl_".len()..];
        let end = after
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// nw-108: `investigate` carried the identical defect to `dead-code` — its
/// daemon branch printed the RPC response verbatim, so the format followed
/// process state rather than the flag.
#[test]
fn parity_investigate_human_direct_vs_daemon() {
    let fixture = setup_fixture();
    let args: &[&str] = &["investigate", "alpha"];

    let direct = run_direct(&fixture.db_path, args);

    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);
    let daemon = run_via_daemon(&fixture.db_path, args);

    assert_eq!(
        direct.status.code(),
        daemon.status.code(),
        "investigate (human): exit code diverged"
    );
    assert_eq!(
        redact_bundle_ids(&direct.stdout),
        redact_bundle_ids(&daemon.stdout),
        "investigate (human): stdout diverged between direct and daemon mode"
    );
}

///
/// Scoped to HUMAN mode deliberately. `--json` still diverges between the two
/// paths for reasons that predate this fix and would change a published
/// contract to resolve: the daemon wraps its payload in `_meta`, and it
/// lowercases the confidence ("medium") while the direct payload carries the
/// serde variant name ("Medium"). Both are real and tracked separately; neither
/// is the format flip this test exists to catch. Asserting json parity here
/// would couple this regression guard to that unrelated decision.
#[test]
fn parity_dead_code_human_direct_vs_daemon() {
    let fixture = setup_fixture();
    let args: &[&str] = &["dead-code", "--limit", "5"];

    // Direct mode first — the daemon is not started yet and may hold the lock.
    let direct = run_direct(&fixture.db_path, args);

    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);
    let daemon = run_via_daemon(&fixture.db_path, args);

    assert_parity("dead-code", "human", &direct, &daemon);
}

/// nw-111 (5): `blast-radius` and `flow-trace` are the product's headline
/// capabilities and existed only as MCP tools — absent from the CLI, which is the
/// discovery surface. They are thin wrappers over the SAME
/// `nestweaver_mcp::tools::dispatch` the daemon runs, so both modes must agree.
///
/// Covered from the outset rather than added after a divergence is reported:
/// `dead-code` and `investigate` both shipped emitting JSON or text depending on
/// whether a daemon happened to be running, and survived because they were not in
/// this file (nw-108).
#[test]
fn parity_blast_radius_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "blast-radius",
        &["blast-radius", "--files", CHANGED_FILES, "--depth", "2"],
    );
}

#[test]
fn parity_flow_trace_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "flow-trace",
        &["flow-trace", "alpha", "--max-depth", "2"],
    );
}

/// `stale-check` is a freshness gate: it exits 1 when the index is
/// stale. The fixture here is freshly indexed (not stale), but regardless we
/// only assert stdout equality and equal exit codes across modes — never
/// success.
#[test]
fn parity_stale_check_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(&fixture.db_path, "stale-check", &["stale-check"]);
}
