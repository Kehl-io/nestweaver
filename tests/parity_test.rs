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
    /// The indexed repo's checkout root. `read-symbols` resolves each symbol's
    /// repo-relative `file_path` against a root, so a test that omits it reads
    /// against the test process's cwd, finds nothing, and compares two empty
    /// bodies (nw-340).
    repo_dir: PathBuf,
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

    Fixture {
        _dir: dir,
        db_path,
        repo_dir,
    }
}

/// A fixture that carries a real Project.
///
/// nw-218. `project-context` cannot be compared on a database that has none:
/// a NOT_FOUND on both routes is a byte-identical failure that asserts nothing,
/// which is why `parity_project_context_direct_vs_daemon` was DELETED rather
/// than kept. `setup_fixture` indexes four plain `.js` files and indexing never
/// creates a Project node.
///
/// The blocker was smaller than the tombstone implies: projects do not need
/// `materialize-projects` (which needs a live daemon). Three store writers are
/// enough, and all three are already used by fixtures elsewhere in this
/// workspace — `insert_project` in `tests/cli_test.rs` and
/// `tests/daemon_test.rs`, and both batch edge writers in
/// `crates/nestweaver-mcp/src/tools.rs`.
fn setup_project_fixture() -> Fixture {
    let fixture = setup_fixture();
    {
        let store = nestweaver_store::GraphStore::open_or_create(&fixture.db_path).unwrap();
        store
            .insert_project(&nestweaver_schema::Project {
                uid: "proj:parity:demo".to_string(),
                name: "demo".to_string(),
                summary: Some("parity fixture".to_string()),
                instance_id: "default".to_string(),
            })
            .unwrap();
        // Every indexed symbol is a member, so the project has real mass and a
        // small `--token-budget` genuinely truncates. A project whose members
        // all fit is a fixture that cannot observe truncation at all.
        let members: Vec<String> = store
            .list_all_symbols()
            .unwrap()
            .into_iter()
            .map(|symbol| symbol.uid)
            .collect();
        assert!(
            members.len() >= 4,
            "the fixture must have symbol mass, or a budget cannot cut: {}",
            members.len()
        );
        store
            .batch_insert_project_symbol_edges("proj:parity:demo", &members, 1.0)
            .unwrap();
    }
    fixture
}

fn setup_contract_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("ContrÁct-Repo");
    let db_path = dir.path().join("db").join("contracts.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_repo_files(
        &repo_dir,
        &[(
            "openapi.yaml",
            "openapi: 3.0.0\ninfo:\n  title: Contract fixture\n  version: 1.0.0\npaths:\n  /widgets:\n    get:\n      operationId: listWidgets\n      responses:\n        '200':\n          description: ok\n",
        )],
    );
    create_db(&repo_dir, &db_path);
    Fixture {
        _dir: dir,
        db_path,
        repo_dir,
    }
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

/// Run a tool over MCP stdio and return its `structuredContent` payload.
///
/// **This is the third route, and until now this harness had no leg for it.**
/// `run_direct` and `run_via_daemon` compare A against B; the MCP server calls
/// `nestweaver_mcp::tools::dispatch` directly, so every response-shape decision
/// the CLI makes ABOVE that call — provenance, pre-cap counts, truncation
/// disclosure, summary lists, budget accounting — was invisible to this file by
/// construction. That is why a batch of route-parity findings survived a
/// release that spent six commits on route parity, and why a doc comment
/// further down asserts an equivalence it never tested.
///
/// Modelled on `tests/daemon_test.rs:301` (`mcp_tool_call_in_mode`); each
/// `tests/*.rs` is its own crate, so duplicating the minimal harness is this
/// file's stated convention.
fn run_via_mcp(db_path: &Path, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
    use std::io::Write as _;
    use std::process::Stdio;

    let frames = [
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        }),
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }),
    ];
    let input = frames
        .iter()
        .map(|frame| serde_json::to_string(frame).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let mut child = StdCommand::new(bin_path())
        .args(["mcp", "--db", &db_path.display().to_string()])
        // Direct, like `run_direct`: any difference is then layer-shaped rather
        // than transport-shaped.
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn nestweaver mcp");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("failed to read mcp output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    let frame: serde_json::Value = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["id"] == serde_json::json!(2))
        .unwrap_or_else(|| panic!("{tool}: no tools/call frame in MCP stdout:\n{stdout}"));
    assert!(
        frame["result"]["isError"] != serde_json::json!(true),
        "{tool}: MCP returned an error: {}",
        frame["result"]
    );
    frame["result"]["structuredContent"].clone()
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

/// Flatten a miette-rendered error to one lowercase line.
///
/// miette hard-wraps its message body and prefixes continuations with `│`, so
/// a phrase the code emits contiguously — "holds the write lease" — can reach a
/// test as "write\n  │ lease". A substring assertion against the raw bytes then
/// fails for a rendering reason while the message is exactly right, which reads
/// as a product bug and is not one.
fn flatten_miette(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .replace('\u{2502}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

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

fn normalize_regex_diagnostics(stdout: &[u8], mode_label: &str) -> Vec<u8> {
    if mode_label == "json" {
        let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(stdout) else {
            return stdout.to_vec();
        };
        fn zero_timings(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(object) => {
                    if let Some(serde_json::Value::Object(timings)) = object.get_mut("timings") {
                        for key in ["planning_ms", "hydration_ms", "verification_ms", "total_ms"] {
                            if timings.contains_key(key) {
                                timings.insert(key.to_string(), serde_json::Value::from(0));
                            }
                        }
                    }
                    for child in object.values_mut() {
                        zero_timings(child);
                    }
                }
                serde_json::Value::Array(values) => {
                    for child in values {
                        zero_timings(child);
                    }
                }
                _ => {}
            }
        }
        zero_timings(&mut value);
        return serde_json::to_vec(&value).unwrap();
    }

    let Ok(mut text) = String::from_utf8(stdout.to_vec()) else {
        return stdout.to_vec();
    };
    let mut offset = 0;
    while let Some(relative_end) = text[offset..].find(" ms") {
        let end = offset + relative_end;
        let start = text[..end]
            .char_indices()
            .rev()
            .take_while(|(_, character)| character.is_ascii_digit())
            .last()
            .map_or(end, |(index, _)| index);
        if start == end {
            offset = end + 3;
            continue;
        }
        text.replace_range(start..end, "<elapsed>");
        offset = start + "<elapsed> ms".len();
    }
    for marker in ["plan ", "hydrate ", "verify "] {
        let mut offset = 0;
        while let Some(relative) = text[offset..].find(marker) {
            let start = offset + relative + marker.len();
            let end = start
                + text[start..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .map(char::len_utf8)
                    .sum::<usize>();
            if end > start {
                text.replace_range(start..end, "<elapsed>");
            }
            offset = start + "<elapsed>".len();
        }
    }
    text.into_bytes()
}

/// Parity assertion: deterministic stdout and equal exit codes across modes.
/// Regex execution timings are intentionally measured independently by each
/// route, so only those elapsed values are normalized; results and every work
/// counter remain byte-for-byte compared.
/// Both runs must have SUCCEEDED, and the daemon run must actually have used
/// the daemon.
///
/// `assert_parity` compared only exit code and stdout bytes. `main()` sends
/// every error to stderr and exits 1 with stdout untouched — on BOTH routes —
/// so any shared failure produced byte-identical empty stdout and equal codes,
/// and passed. Every parity test in this file was green if the command was
/// completely broken.
///
/// The second half is subtler. When an RPC fails, the CLI falls back to the
/// direct path and warns on STDERR ONLY, which byte-comparison never reads. So
/// a test could pass by running the direct implementation TWICE — the daemon
/// branch never executing is indistinguishable from it agreeing. That is
/// precisely the divergence class these tests exist to catch, so a silent
/// fallback has to be a failure, not a pass.
fn assert_both_ran_for_real(command: &str, mode_label: &str, direct: &Output, daemon: &Output) {
    for (route, output) in [("direct", direct), ("daemon", daemon)] {
        assert!(
            output.status.success(),
            "{command} ({mode_label}, {route}): expected success, got {:?}. Parity between \
             two FAILURES is not parity.\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let daemon_stderr = String::from_utf8_lossy(&daemon.stderr);
    assert!(
        !daemon_stderr.contains("the daemon could not serve"),
        "{command} ({mode_label}): the daemon run FELL BACK to the direct path, so this \
         compared one implementation against itself.\nstderr:\n{daemon_stderr}"
    );
}

/// Key ORDER is not a contract (RFC 8259 §4 — an object is an unordered
/// collection); key PRESENCE is. A missing `truncated` or `total` must fail
/// loudly rather than pass as "just ordering".
fn assert_same_key_sets(command: &str, direct: &Output, daemon: &Output) {
    fn key_paths(value: &serde_json::Value, prefix: &str, into: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let path = format!("{prefix}.{key}");
                    into.push(path.clone());
                    key_paths(child, &path, into);
                }
            }
            // Index-free: array length is data, not shape, and two runs may
            // legitimately return different counts.
            serde_json::Value::Array(items) => {
                for item in items {
                    key_paths(item, &format!("{prefix}[]"), into);
                }
            }
            _ => {}
        }
    }
    let parse = |raw: &Output| -> Option<serde_json::Value> {
        serde_json::from_str(&String::from_utf8_lossy(&raw.stdout)).ok()
    };
    let (Some(direct_json), Some(daemon_json)) = (parse(direct), parse(daemon)) else {
        return;
    };
    let mut direct_keys = Vec::new();
    let mut daemon_keys = Vec::new();
    key_paths(&direct_json, "", &mut direct_keys);
    key_paths(&daemon_json, "", &mut daemon_keys);
    direct_keys.sort();
    direct_keys.dedup();
    daemon_keys.sort();
    daemon_keys.dedup();
    assert_eq!(
        direct_keys, daemon_keys,
        "{command} (json): the two routes emit DIFFERENT KEYS. Order is not a contract; \
         presence is — a field on one route only is exactly the drift these tests exist \
         to catch."
    );
}

fn assert_parity(command: &str, mode_label: &str, direct: &Output, daemon: &Output) {
    assert_both_ran_for_real(command, mode_label, direct, daemon);
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
    let normalize_timings = matches!(command, "regex-search" | "count-patterns");
    let direct_normalized = if normalize_timings {
        normalize_regex_diagnostics(&direct.stdout, mode_label)
    } else {
        direct.stdout.clone()
    };
    let daemon_normalized = if normalize_timings {
        normalize_regex_diagnostics(&daemon.stdout, mode_label)
    } else {
        daemon.stdout.clone()
    };
    assert_eq!(
        direct_normalized,
        daemon_normalized,
        "{command} ({mode_label}): stdout diverged between direct and daemon mode\n\
         --- direct ---\n{}\n--- daemon ---\n{}",
        String::from_utf8_lossy(&direct.stdout),
        String::from_utf8_lossy(&daemon.stdout)
    );
}

/// Run one command in both output formats, direct mode FIRST (before the
/// daemon starts, since a running daemon may hold the DB lock), then daemon
/// mode, and assert byte-identical stdout + equal exit codes for both formats.
/// Like [`check_parity`], but compares the JSON output SEMANTICALLY.
///
/// Byte equality is the default because it is the stronger guarantee and
/// catches formatting drift as well as content drift. It is the wrong
/// assertion only where the two paths serialize the same data through
/// different types: the daemon route round-trips through `serde_json::Value`,
/// whose object is a BTreeMap and therefore alphabetises keys, while a direct
/// path serializing a struct keeps declaration order. JSON object key order
/// carries no meaning, so demanding it would be conforming the product to the
/// test. Field PRESENCE and VALUES are the contract, and those are compared.
fn check_parity_json_semantic(db_path: &Path, command: &str, args: &[&str]) {
    let mut json_args: Vec<&str> = args.to_vec();
    json_args.push("--json");

    let direct_human = run_direct(db_path, args);
    let direct_json = run_direct(db_path, &json_args);

    let _guard = DaemonGuard::new(db_path);
    start_daemon(db_path);

    let daemon_human = run_via_daemon(db_path, args);
    let daemon_json = run_via_daemon(db_path, &json_args);

    assert_parity(command, "human", &direct_human, &daemon_human);
    assert_both_ran_for_real(command, "json", &direct_json, &daemon_json);
    assert_same_key_sets(command, &direct_json, &daemon_json);

    let parse = |label: &str, raw: &Output| -> serde_json::Value {
        let text = String::from_utf8_lossy(&raw.stdout).into_owned();
        serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("{command} ({label}): stdout is not valid JSON: {error}\n{text}")
        })
    };
    assert_eq!(
        parse("direct", &direct_json),
        parse("daemon", &daemon_json),
        "{command} (json): parsed output diverged between direct and daemon mode\n\
         --- direct ---\n{}\n--- daemon ---\n{}",
        String::from_utf8_lossy(&direct_json.stdout),
        String::from_utf8_lossy(&daemon_json.stdout)
    );
    assert_eq!(
        direct_json.status.code(),
        daemon_json.status.code(),
        "{command} (json): exit code diverged"
    );
}

/// [`check_parity`] for commands that have no `--json` flag.
///
/// The default helper appends `--json` unconditionally; for a command that does
/// not accept it, BOTH routes fail with an identical clap usage error and the
/// byte comparison passes on two failures.
/// Both routes must REFUSE the same way.
///
/// Distinct from [`check_parity`], which requires success. A refusal is a real
/// contract too — "msgpack cannot represent the vault" must not depend on
/// whether a daemon is running — but asserting it needs care, because "both
/// failed identically" is exactly the hole that made every parity test vacuous
/// before `assert_both_ran_for_real`.
///
/// The difference is that failure is EXPECTED here and checked: both must fail,
/// with the same code, and each message must name `expected_reason`. A command
/// that failed for an unrelated reason — a typo'd flag, a missing database —
/// does not pass.
fn check_parity_of_refusal(db_path: &Path, command: &str, args: &[&str], expected_reason: &str) {
    let direct = run_direct(db_path, args);
    let _guard = DaemonGuard::new(db_path);
    start_daemon(db_path);
    let daemon = run_via_daemon(db_path, args);

    for (route, output) in [("direct", &direct), ("daemon", &daemon)] {
        assert!(
            !output.status.success(),
            "{command} ({route}): expected a REFUSAL, but it succeeded"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            stderr.contains(&expected_reason.to_lowercase()),
            "{command} ({route}): refused for the wrong reason — expected \
             {expected_reason:?}, got:\n{stderr}"
        );
    }
    assert_eq!(
        direct.status.code(),
        daemon.status.code(),
        "{command}: the two routes refused with DIFFERENT exit codes"
    );
}

fn check_parity_single_format(db_path: &Path, command: &str, args: &[&str]) {
    let direct = run_direct(db_path, args);
    let _guard = DaemonGuard::new(db_path);
    start_daemon(db_path);
    let daemon = run_via_daemon(db_path, args);
    assert_parity(command, "human", &direct, &daemon);
}

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

/// nw-321. `summary`'s text route through the daemon reads the tool's
/// `summaries` field, and this lane changed that field from a "\n"-joined
/// STRING to the structured list the CLI twin has always returned — because an
/// agent receiving prose it has to re-parse, while the human receives records,
/// is the finding. `src/main.rs` is Lane E's file and its reader still expects
/// the string, so it now falls through to the direct path instead.
///
/// That fall-through is a route change, and the whole point of this batch is
/// that a silent route change is a defect. So it is PINNED here rather than
/// assumed benign: the two routes must still produce identical human output and
/// identical exit codes. When Lane E lands D2-E5 (delete the `&& !json`
/// carve-out and read `summaries_text`), this test keeps holding — it asserts
/// the OUTPUT, not which route produced it.
#[test]
fn parity_summary_text_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "summary",
        &["summary", "--level", "file", "--token-budget", "0"],
    );
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

/// `contracts drift` had the nw-097 divergence in its purest form: the daemon
/// branch printed the MCP envelope (totals, `clean`, `limit`, truncated
/// buckets) while the direct branch printed a BARE `DriftReport` — different
/// keys, no verdict, and no truncation at all. Human output matched, so only
/// `--json` exposed it, and this file is where that class of divergence is
/// caught. The fixture declares no specs and no route handlers, so this also
/// pins the healthy-empty case: both modes must agree that it is clean.
#[test]
fn parity_contracts_drift_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(&fixture.db_path, "contracts drift", &["contracts", "drift"]);
}

/// `contracts list` must use its dedicated daemon RPC, not the unrelated
/// symbol-oriented `cross_repo_contracts` tool followed by a direct DB read.
/// This covers both renderers, both repo-filter forms, and finally makes the
/// DB file unreadable after daemon startup: the already-open daemon can still
/// answer, while any direct-store fallback would fail.
#[test]
fn parity_contracts_list_uses_daemon_without_direct_store_fallback() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = setup_contract_fixture();
    let direct_human = run_direct(&fixture.db_path, &["contracts", "list"]);
    let direct_json = run_direct(&fixture.db_path, &["contracts", "list", "--json"]);
    assert_successful_output(&direct_human, "direct contracts list");
    assert_successful_output(&direct_json, "direct contracts list --json");
    let contracts: Vec<serde_json::Value> = serde_json::from_slice(&direct_json.stdout).unwrap();
    assert_eq!(contracts.len(), 1);
    let repo_uid = contracts[0]["repo_uid"].as_str().unwrap().to_string();

    let direct_name = run_direct(
        &fixture.db_path,
        &["contracts", "list", "--repo", "contráct-repo", "--json"],
    );
    let direct_uid = run_direct(
        &fixture.db_path,
        &["contracts", "list", "--repo", &repo_uid, "--json"],
    );

    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);
    let daemon_human = run_via_daemon(&fixture.db_path, &["contracts", "list"]);
    let daemon_json = run_via_daemon(&fixture.db_path, &["contracts", "list", "--json"]);
    let daemon_name = run_via_daemon(
        &fixture.db_path,
        &["contracts", "list", "--repo", "contráct-repo", "--json"],
    );
    let daemon_uid = run_via_daemon(
        &fixture.db_path,
        &["contracts", "list", "--repo", &repo_uid, "--json"],
    );

    assert_parity("contracts list", "human", &direct_human, &daemon_human);
    assert_parity("contracts list", "json", &direct_json, &daemon_json);
    assert_parity(
        "contracts list --repo non-ASCII case-folded name",
        "json",
        &direct_name,
        &daemon_name,
    );
    assert_parity(
        "contracts list --repo uid",
        "json",
        &direct_uid,
        &daemon_uid,
    );
    for output in [&daemon_human, &daemon_json, &daemon_name, &daemon_uid] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("fallback"),
            "unexpected fallback: {stderr}"
        );
        assert!(
            !stderr.contains("cross_repo_contracts"),
            "wrong daemon tool was invoked: {stderr}"
        );
    }

    let original_mode = std::fs::metadata(&fixture.db_path)
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(&fixture.db_path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let daemon_without_disk_access =
        run_via_daemon(&fixture.db_path, &["contracts", "list", "--json"]);
    let daemon_error_without_fallback = run_via_daemon(
        &fixture.db_path,
        &[
            "contracts",
            "list",
            "--repo",
            "definitely-unknown",
            "--json",
        ],
    );
    std::fs::set_permissions(
        &fixture.db_path,
        std::fs::Permissions::from_mode(original_mode),
    )
    .unwrap();
    assert_successful_output(
        &daemon_without_disk_access,
        "daemon contracts list with unreadable DB path",
    );
    assert_eq!(daemon_without_disk_access.stdout, direct_json.stdout);
    assert!(daemon_without_disk_access.stderr.is_empty());
    assert!(!daemon_error_without_fallback.status.success());
    let error = flatten_diagnostic(&daemon_error_without_fallback.stderr);
    assert!(
        error.contains("no indexed repo matches --repo 'definitely-unknown'"),
        "daemon error must name the unmatched repo; got: {error}"
    );
    assert!(
        error.contains("refusing direct-store fallback"),
        "daemon error must refuse the fallback; got: {error}"
    );
    assert!(!error.contains("cross_repo_contracts"));

    let explicit_empty = run_via_daemon(
        &fixture.db_path,
        &["contracts", "list", "--repo", "", "--json"],
    );
    assert!(!explicit_empty.status.success());
    let empty_error = flatten_diagnostic(&explicit_empty.stderr);
    assert!(
        empty_error.contains("no indexed repo matches --repo ''"),
        "explicit-empty error must name the empty repo; got: {empty_error}"
    );
    assert!(
        empty_error.contains("refusing direct-store fallback"),
        "explicit-empty error must refuse the fallback; got: {empty_error}"
    );
}

/// nw-108: `dead-code`'s daemon branch printed the RPC response verbatim with
/// no `if json` guard, so the OUTPUT FORMAT depended on whether a daemon
/// happened to be running rather than on the flag — text standalone, JSON once
/// the daemon was up. The text renderer was never missing; it was simply not
/// reached. This is the nw-097 divergence family, and the reason it survived is
/// that `dead-code` was not in this file.
/// Flatten a rendered `miette` diagnostic for substring assertions.
///
/// The renderer hard-wraps to the terminal width and prefixes continuation
/// lines with a `│` gutter, so a message the code emits as one string arrives
/// split across lines: `"no\n  │ indexed repo matches --repo 'definitely-\n  │
/// unknown'"`. Asserting on the raw bytes therefore tests the renderer's
/// current wrap width rather than the contract under test — the same assertion
/// passes or fails depending on how wide the box happens to be.
///
/// Strip the gutter and collapse all whitespace to single spaces so the
/// assertions below check what they mean to check: that the error names the
/// repo and refuses the fallback.
fn flatten_diagnostic(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace('\u{2502}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drop the semantic-availability note before comparing direct and daemon output.
///
/// That note is a DELIBERATE difference between the two paths, not an accident:
/// the direct path passes no embedding model, so `semantic_applied` is always
/// false there and the note always prints, while the daemon prints it only when
/// its own embedding state says semantic retrieval was unavailable. nw-120 added
/// it precisely so a reader can tell which path produced a ranking.
///
/// Asserting byte-equality across that note therefore tests the daemon's
/// embedding-probe state, not renderer parity. The probe is cached and
/// time-bounded, so the assertion passed only while the fixture happened to
/// leave the daemon without semantic too — a flake confirmed on main, not
/// introduced here.
///
/// Strip the note and keep everything else strict: renderer parity is what this
/// suite is for, and the note has its own coverage below.
fn strip_semantic_note(bytes: &[u8]) -> String {
    redact_bundle_ids(bytes)
        .lines()
        .filter(|line| {
            !line
                .trim_start()
                .starts_with("note: semantic retrieval unavailable")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

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
        strip_semantic_note(&direct.stdout),
        strip_semantic_note(&daemon.stdout),
        "investigate (human): stdout diverged between direct and daemon mode"
    );

    // The note itself is still pinned, on the path whose behaviour is
    // deterministic: the direct path passes no embedding model, so it must
    // always disclose that the ranking was lexical.
    let direct_text = String::from_utf8_lossy(&direct.stdout);
    assert!(
        direct_text.contains("note: semantic retrieval unavailable"),
        "the direct path has no embedding model and must say so; got: {direct_text}"
    );
}

///
/// Now covers `--json` too. It was scoped to human-only because the payloads
/// genuinely diverged: the daemon wrapped its result in `_meta` while the direct
/// path omitted it, and the confidence serialised as "medium" through the daemon
/// but "Medium" direct — the same field disagreeing with itself depending on
/// whether a daemon was running. Both are fixed (nw-117), so json parity is the
/// acceptance test for that fix.
#[test]
fn parity_dead_code_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "dead-code",
        &["dead-code", "--limit", "5"],
    );
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
        &["flow-trace", "mainA", "--max-depth", "2"],
    );
}

/// `export --scope` was honoured by the direct path and silently DROPPED by
/// the daemon path: the CLI never sent `scope` and the daemon never read it.
/// Because the daemon route is the default, the flag did nothing for most
/// users while the engine-level scope tests passed. Parity is the only place
/// that catches a flag lost on the wire.
#[test]
fn parity_export_scope_direct_vs_daemon() {
    let fixture = setup_fixture();
    for scope in ["all", "code", "vault"] {
        // `export` has no `--json`, and `check_parity` appends it — so the
        // json leg failed on BOTH routes and the byte comparison passed on two
        // identical usage errors. Compare only the real output.
        check_parity_single_format(
            &fixture.db_path,
            &format!("export --scope {scope}"),
            &["export", "--format", "graphml", "--scope", scope],
        );
    }
}

/// Coverage sweep. Thirty commands route through the daemon; fourteen had a
/// parity case. Every transport divergence found in the 7.0.0 review was the
/// same shape — right on one path, wrong or absent on the other — and CI
/// caught none of them, because both paths pass their own tests. These are the
/// remaining routed commands whose fixture requirements are already met.
///
/// A command is worth covering here even when it currently agrees: the value
/// is catching the day it stops.
#[test]
fn parity_list_repos_direct_vs_daemon() {
    let fixture = setup_fixture();
    // Semantic: the direct path serializes a struct (declaration order), the
    // daemon path round-trips through `serde_json::Value` (alphabetised). Same
    // fields, same values, different key order — which is not a contract.
    check_parity_json_semantic(&fixture.db_path, "list-repos", &["list-repos"]);
}

/// This one found a real bug the moment the harness stopped passing on two
/// identical failures. `context mainA` diverged three ways between routes:
///
///   1. kind rendering — `Function` vs `[Symbol/Function]`
///   2. header — `Connected (3 symbols, …)` vs `Connected (3 of 3, …)`
///   3. RELEVANCE SCORES DIFFERED — 0.2096/0.2039/0.0208 direct versus
///      0.3117/0.3046/0.0403 via daemon
///
/// (3) was the serious one, and (1) and (2) were downstream of it: the daemon
/// route was answering with `brain_context`, the code+notes hybrid, because
/// `context` had no RPC of its own. The same query returned different rankings
/// depending on whether a daemon happened to be running. `code_context` is the
/// RPC for what this command means, and both routes now build a
/// `ContextResult` and render through one function — so this is a byte
/// comparison again, and it holds.
#[test]
fn parity_brain_context_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "context",
        // Seeds are POSITIONAL; `--seeds` is not a flag. The original spelling
        // failed clap on BOTH routes, and the byte comparison passed on two
        // identical usage errors.
        &["context", "mainA"],
    );
}

#[test]
fn parity_brain_impact_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(&fixture.db_path, "impact", &["impact", "mainA"]);
}

#[test]
fn parity_search_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(&fixture.db_path, "search", &["search", "helper"]);
}

#[test]
fn parity_read_symbols_direct_vs_daemon() {
    // nw-340: `--root` is not decoration here. Without it the DIRECT route
    // resolves `src/a.js` against the test process's cwd and the DAEMON route
    // against its own — neither is the fixture repo — so both printed a header
    // with a blank line under it and this test compared two empty bodies and
    // called it parity. It is also the reason the exit code had to become a
    // discriminator: with `EXIT_SUCCESS` on an unreadable body there was
    // nothing for the test to notice.
    let fixture = setup_fixture();
    let root = fixture.repo_dir.display().to_string();
    let args = ["read-symbols", "mainA", "--root", root.as_str()];
    check_parity(&fixture.db_path, "read-symbols", &args);

    // And the body must actually be there, on both routes. Parity alone would
    // still pass on two identical blanks.
    let direct = run_direct(&fixture.db_path, &args);
    let stdout = String::from_utf8_lossy(&direct.stdout);
    assert!(
        stdout.contains("export function mainA"),
        "read-symbols must emit the symbol's SOURCE, not just its header: {stdout:?}"
    );
    assert!(
        !stdout.contains("source unavailable"),
        "the fixture root was passed, so the body must be readable: {stdout:?}"
    );
}

#[test]
fn parity_detect_changes_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(
        &fixture.db_path,
        "detect-changes",
        &["detect-changes", "--files", CHANGED_FILES],
    );
}

/// nw-218. `parity_project_context_direct_vs_daemon` was DELETED in 8.0.0
/// because `setup_fixture` creates no Project, so both routes exited NOT_FOUND
/// and the byte comparison passed on two identical failures. `setup_project_fixture`
/// removes that blocker.
///
/// Restored as a KEY-SET comparison, not the byte comparison it used to be.
/// The two routes legitimately differ on `semantic_applied` and
/// `degraded_components` — the direct path passes `HybridSearchConfig::default()`
/// and `embed_model: None` — so a byte comparison would fail for a reason that
/// is not the defect, and "the test is red for a known-benign reason" is how a
/// suite stops being read.
#[test]
fn parity_project_context_direct_vs_daemon() {
    let fixture = setup_project_fixture();
    let db = &fixture.db_path;
    let args = &["project-context", "demo", "--json", "--token-budget", "400"];

    let direct = run_direct(db, args);
    assert!(
        direct.status.success(),
        "project-context (direct) failed:\n{}",
        String::from_utf8_lossy(&direct.stderr)
    );

    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    let daemon = run_via_daemon(db, args);
    assert!(
        daemon.status.success(),
        "project-context (daemon) failed:\n{}",
        flatten_miette(&daemon.stderr)
    );

    assert_both_ran_for_real("project-context", "json", &direct, &daemon);
    assert_same_key_sets("project-context", &direct, &daemon);

    // The nw-188 honesty fields the deleted test was written for. These are
    // the ones a caller acts on, so they must AGREE, not merely both exist.
    let direct_json = parse_stdout("project-context (direct)", &direct);
    let daemon_json = parse_stdout("project-context (daemon)", &daemon);
    for field in ["truncated", "more_available", "seed_tokens_charged"] {
        assert_eq!(
            direct_json[field], daemon_json[field],
            "`{field}` differs between routes for the same project and the same \
             budget, so how much was dropped depends on which transport \
             answered.\ndirect: {direct_json}\ndaemon: {daemon_json}"
        );
    }

    // Counterweight: a budget the project fits must report NOT truncated, or
    // the equality above is satisfiable by both routes hardcoding `true`.
    let roomy = &[
        "project-context",
        "demo",
        "--json",
        "--token-budget",
        "16000",
    ];
    let roomy_direct = parse_stdout("project-context (roomy)", &run_direct(db, roomy));
    assert_eq!(
        roomy_direct["truncated"],
        serde_json::json!(false),
        "a budget that fits must not report truncation: {roomy_direct}"
    );
}

/// msgpack must honour `--scope` on BOTH routes.
///
/// Direct mode handled msgpack BEFORE parsing `--scope`, so `--scope vault`
/// produced a code-only file and reported success, and an INVALID scope
/// succeeded too — the parse that would have rejected it never ran. The daemon
/// route rejected vault. Two routes, two answers, for a flag that is supposed
/// to mean one thing.
#[test]
fn parity_msgpack_scope_direct_vs_daemon() {
    let fixture = setup_fixture();
    // A scope msgpack CANNOT satisfy must be refused identically, and for the
    // stated reason — not merely fail.
    check_parity_of_refusal(
        &fixture.db_path,
        "export msgpack --scope vault",
        &["export", "--format", "msgpack", "--scope", "vault"],
        "code-only",
    );
    // An INVALID scope must fail on both, not slip through whichever route
    // happens to dispatch on format first.
    //
    // nw-312 moved this refusal from the handler to the PARSER, so the reason
    // changed from `unknown export scope` (an `anyhow` chain, exit 1) to clap's
    // usage error, which enumerates the legal values and exits 64. The parity
    // is now structural rather than asserted: clap runs before either route
    // dispatches, so the two cannot disagree about what a bad `--scope` is.
    check_parity_of_refusal(
        &fixture.db_path,
        "export msgpack --scope nonsense",
        &["export", "--format", "msgpack", "--scope", "nonsense"],
        "possible values: all, code, vault",
    );
}

/// nw-244: `export`'s default scope is a CONTRACT, and only its help text
/// asserted it.
///
/// The default moved from `code` to `all` deliberately, and reverting it kept
/// the suite green — because nothing ran `export` with no `--scope` and checked
/// what came out. A help string is not the behaviour.
///
/// `graphml` is the format that honours all three scopes, so it is the one that
/// can tell them apart: under `all` the export carries vault nodes, under
/// `code` it does not.
#[test]
fn export_defaults_to_the_all_scope_not_code() {
    // A fixture with BOTH code and vault content. `setup_fixture` alone is
    // code-only, so `all` and `code` produce identical output there and the
    // test would pass under either default — the assertion at the bottom
    // catches exactly that, and it fired the first time I wrote this.
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let db_path = dir.path().join("db").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&vault_dir).unwrap();
    write_repo_files(
        &repo_dir,
        &[("src/a.js", "export function one() { return 1; }\n")],
    );
    std::fs::write(
        vault_dir.join("note.md"),
        "# A Note\n\nSome vault content.\n",
    )
    .unwrap();
    create_db(&repo_dir, &db_path);
    no_daemon_cmd()
        .args(["brain", "add"])
        .arg(&vault_dir)
        .args(["--db", &db_path.display().to_string()])
        .assert()
        .success();
    let fixture = Fixture {
        _dir: dir,
        db_path,
        repo_dir,
    };

    let defaulted = run_direct(&fixture.db_path, &["export", "--format", "graphml"]);
    assert!(
        defaulted.status.success(),
        "export with no --scope must succeed; stderr:\n{}",
        String::from_utf8_lossy(&defaulted.stderr)
    );
    let explicit_all = run_direct(
        &fixture.db_path,
        &["export", "--format", "graphml", "--scope", "all"],
    );
    let explicit_code = run_direct(
        &fixture.db_path,
        &["export", "--format", "graphml", "--scope", "code"],
    );

    assert_eq!(
        defaulted.stdout, explicit_all.stdout,
        "no --scope must behave exactly as `--scope all`"
    );
    // The counterweight, and the half that makes the assertion above mean
    // something: `all` and `code` must actually DIFFER on this fixture. If they
    // produced identical output the test would pass under either default and
    // prove nothing.
    assert_ne!(
        explicit_all.stdout, explicit_code.stdout,
        "the fixture cannot distinguish `all` from `code`, so this test would \
         pass under either default — it needs vault content to be meaningful"
    );
}

/// nw-244: the exit CODE is the contract, and nothing asserted it.
///
/// `stale_check_help_states_the_real_exit_contract` asserts against `--help`
/// TEXT, so it passes with the runtime logic inverted. `parity_stale_check_...`
/// below asserts the two routes AGREE, so it passes if both regress together.
/// And its own doc comment said "exits 1" — the code is 2. Reverting
/// `EXIT_NEEDS_REINDEX` to `EXIT_ERROR` kept the whole suite green.
///
/// This exercises the stale path for real: index, then advance the repo one
/// commit so HEAD differs from the indexed SHA.
#[test]
fn stale_check_exits_needs_reindex_not_generic_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("db").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_repo_files(
        &repo_dir,
        &[("src/a.js", "export function one() { return 1; }\n")],
    );
    create_db(&repo_dir, &db_path);

    // Advance HEAD past the indexed SHA.
    std::fs::write(
        repo_dir.join("src/b.js"),
        "export function two() { return 2; }\n",
    )
    .unwrap();
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "second",
        ],
    ] {
        StdCommand::new("git")
            .args(&args)
            .current_dir(&repo_dir)
            .output()
            .unwrap();
    }

    let output = run_direct(&db_path, &["stale-check"]);
    let code = output.status.code();

    // 2 is EXIT_NEEDS_REINDEX; 1 is EXIT_ERROR. The distinction is the whole
    // point — a CI gate keying on "stale" must not fire on an unrelated
    // failure, and vice versa. Asserting `!= 0` would pass with them merged.
    assert_eq!(
        code,
        Some(2),
        "a stale index must exit EXIT_NEEDS_REINDEX (2), not EXIT_ERROR (1) or success; \
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The counterweight: a FRESH index must exit 0. Without it, a change making
/// stale-check always return 2 would pass the test above.
#[test]
fn stale_check_exits_zero_when_the_index_is_current() {
    let fixture = setup_fixture();

    let output = run_direct(&fixture.db_path, &["stale-check"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "a freshly indexed repo is not stale; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `stale-check` is a freshness gate. The fixture here is freshly indexed, so
/// this asserts only that the two routes agree — the exit-code contract itself
/// is pinned by the two tests above.
#[test]
fn parity_stale_check_direct_vs_daemon() {
    let fixture = setup_fixture();
    check_parity(&fixture.db_path, "stale-check", &["stale-check"]);
}

// ─── nw-210: the daemon owns the writes, and that is TESTED ──────────────────

/// The CLI and the daemon both call `TantivyIndex::open_or_create` +
/// `reindex_from_store` on the SAME index. Tantivy enforces a single writer via
/// `INDEX_WRITER_LOCK`, and `tantivy_index.rs`'s `state.lock()` is an
/// IN-PROCESS mutex that coordinates nothing across processes — so the CLI path
/// either failed with `DirectoryLockBusy` or raced on segment writes. The plain
/// JSON sidecars (cochange, gitactivity, PageRank) had no lock at all.
///
/// The fix was not to make the two writers careful with each other. It was to
/// give the database a single owner: the daemon takes a write lease for its
/// whole lifetime, and every CLI write path takes the same lease before
/// touching the store. With a daemon running, a CLI writer cannot start.
///
/// These tests exist because "the daemon owns writes" is an architectural
/// claim, and an architectural claim with no test decays into a comment.
#[test]
fn a_cli_writer_is_refused_while_the_daemon_holds_the_lease() {
    let fixture = setup_fixture();
    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);

    // `brain reindex-search` is the sharpest case: it rebuilds the Tantivy
    // index the daemon also writes, and Tantivy's own lock would surface this
    // as DirectoryLockBusy — an internal error about a lock file, rather than
    // an answer about who owns the database.
    //
    // `run_direct` sets NESTWEAVER_NO_DAEMON, so this is the BYPASS path: the
    // one that would otherwise open the store itself. Routed normally the same
    // command reaches the daemon's ReindexSearch RPC and simply works, which is
    // the point — the daemon is not a restriction, it is the owner.
    let output = run_direct(&fixture.db_path, &["brain", "reindex-search"]);

    assert!(
        !output.status.success(),
        "a second writer must be refused while the daemon holds the lease; \
         stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = flatten_miette(&output.stderr);
    assert!(
        stderr.contains("write lease"),
        "the refusal must name the LEASE, not a Tantivy lock file or a generic \
         I/O error — the operator needs to know who owns the database:\n{stderr}"
    );
    assert!(
        stderr.contains("daemon"),
        "and it must say what to do about it:\n{stderr}"
    );
}

/// The other half. Without this, a guard that refused unconditionally — or a
/// lease that was never released when the daemon stopped — would pass the test
/// above while making the CLI permanently unusable.
#[test]
fn the_same_cli_writer_succeeds_once_the_daemon_is_gone() {
    let fixture = setup_fixture();

    {
        let _guard = DaemonGuard::new(&fixture.db_path);
        start_daemon(&fixture.db_path);
        let refused = run_direct(&fixture.db_path, &["brain", "reindex-search"]);
        assert!(
            !refused.status.success(),
            "precondition: refused with daemon"
        );
    } // DaemonGuard stops the daemon, which drops the lease.

    let output = run_direct(&fixture.db_path, &["brain", "reindex-search"]);

    assert!(
        output.status.success(),
        "the lease must be RELEASED when the daemon stops, or the CLI is \
         permanently broken after any daemon ever ran; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// nw-267: `watch` and `brain watch` must take the WRITE LEASE before running
/// a direct watcher.
///
/// Both fell back to a direct watcher after a point-in-time
/// `daemon_process_running_for_db` probe — a check of a different file,
/// followed by a writer that runs for HOURS. That is CWE-367, and it is
/// verbatim the construct `embed` removed; `embed`'s comment is still in the
/// tree calling it "CWE-367 exactly" and quoting MITRE's mitigation: *"ensure
/// that locking occurs before the check, as opposed to afterwards."*
///
/// Of five production write paths, these two were the ones that never took the
/// lease. The `.lock` PID file they write is a hint for readers, not a lock —
/// it cannot survive PID reuse and an operator's `rm` erases it.
///
/// Probed the same way as `a_cli_writer_is_refused_while_the_daemon_holds_the_lease`:
/// with the daemon holding the lease, a direct watcher must refuse rather than
/// start indexing into a store it does not own.
#[test]
fn a_direct_watcher_is_refused_while_the_daemon_holds_the_lease() {
    let fixture = setup_fixture();
    let repo_dir = fixture.db_path.parent().unwrap().join("watched-repo");
    write_repo_files(
        &repo_dir,
        &[("src/a.js", "export function one() { return 1; }\n")],
    );
    let vault_dir = fixture.db_path.parent().unwrap().join("watched-vault");
    std::fs::create_dir_all(&vault_dir).unwrap();
    std::fs::write(vault_dir.join("note.md"), "# Note\n").unwrap();

    let _guard = DaemonGuard::new(&fixture.db_path);
    start_daemon(&fixture.db_path);

    let repo_arg = repo_dir.display().to_string();
    let vault_arg = vault_dir.display().to_string();
    for (label, args) in [
        ("watch", vec!["watch", repo_arg.as_str()]),
        ("brain watch", vec!["brain", "watch", vault_arg.as_str()]),
    ] {
        let output = run_direct(&fixture.db_path, &args);

        assert!(
            !output.status.success(),
            "`{label}` must refuse to run a direct watcher while the daemon \
             holds the lease; a watcher that starts here writes into a store it \
             does not own, for hours.\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = flatten_miette(&output.stderr);
        assert!(
            stderr.contains("write lease") || stderr.contains("holds the write lock"),
            "`{label}`'s refusal must name who owns the database, not surface a \
             generic I/O error:\n{stderr}"
        );
    }
}

// ─── C. The MCP leg ──────────────────────────────────────────────────────────
//
// Routes A (CLI→daemon) and B (CLI direct) were the only two this file ever
// compared. These tests add route C (MCP over stdio) for the fields the
// bounds/disclosure pass changed, so a future refactor cannot quietly reopen
// them.

/// Every path down which a JSON key can be reached, so a missing field fails
/// as a missing field rather than as "just ordering".
fn json_key_paths(value: &serde_json::Value) -> Vec<String> {
    fn walk(value: &serde_json::Value, prefix: &str, into: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let path = format!("{prefix}.{key}");
                    into.push(path.clone());
                    walk(child, &path, into);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, &format!("{prefix}[]"), into);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, "", &mut out);
    out.sort();
    out.dedup();
    out
}

fn parse_stdout(command: &str, output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{command}: expected JSON on stdout ({error})\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// nw-320. A capped `code_context` used to report only what it RETURNED, so
/// the count agreed with the item list by construction and a truncated answer
/// was indistinguishable from a complete one. Asserted on the MCP route
/// because that is the route with no CLI enrichment layer above it: whatever
/// the tool does not say, an agent never learns.
#[test]
fn mcp_code_context_discloses_the_pre_cap_total() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let capped = run_via_mcp(
        db,
        "code_context",
        serde_json::json!({ "seeds": ["mainA"], "limit": 1 }),
    );

    let returned = capped["connected"]
        .as_array()
        .expect("connected is an array")
        .len();
    assert_eq!(returned, 1, "the cap must bite or this test proves nothing");
    for field in ["total", "returned", "truncated", "limit"] {
        assert!(
            capped.get(field).is_some(),
            "code_context (MCP) omits `{field}` — the agent cannot tell a capped \
             answer from a complete one: {capped}"
        );
    }
    let total = capped["total"].as_u64().expect("total is a number");
    assert!(
        total > returned as u64,
        "`total` is {total} for {returned} returned rows — it is counting what SURVIVED \
         the cap: {capped}"
    );
    assert_eq!(capped["truncated"], serde_json::json!(true));
}

/// nw-299(a). `clusters` declared no bounding parameter and applied none, and
/// `additionalProperties: false` meant a caller-supplied `limit` was actively
/// REJECTED — 98.7 MB from a default call with no way for a client to prevent
/// it. The bound must be accepted, applied, and disclosed.
#[test]
fn mcp_clusters_accepts_applies_and_discloses_its_bound() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let unbounded = run_via_mcp(db, "clusters", serde_json::json!({}));
    let all = unbounded["clusters"]
        .as_array()
        .expect("clusters is an array")
        .len();
    assert_eq!(
        unbounded["total"].as_u64().map(|n| n as usize),
        Some(all),
        "an unbounded call must report a total equal to what it returned: {unbounded}"
    );
    assert_eq!(unbounded["truncated"], serde_json::json!(false));

    // `limit: 1` used to be a hard validation error, not a bound.
    let bounded = run_via_mcp(db, "clusters", serde_json::json!({ "limit": 1 }));
    let returned = bounded["clusters"].as_array().unwrap().len();
    assert!(
        returned <= 1,
        "the declared bound was not applied: {bounded}"
    );
    assert_eq!(
        bounded["total"].as_u64().map(|n| n as usize),
        Some(all),
        "`total` must count what MATCHED, not what survived the cut: {bounded}"
    );
    assert_eq!(bounded["truncated"], serde_json::json!(all > returned));
}

/// nw-304. A present-but-out-of-range value is a caller bug and must surface
/// as one. `as_u64()` returns None for a negative, so `-1` used to fall
/// through to `unwrap_or_else(configured_result_limit)` and become 50 — the
/// caller was told nothing and got a confident answer to a request they never
/// made.
#[test]
fn mcp_rejects_a_negative_limit_rather_than_defaulting_it() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let baseline = run_via_mcp(db, "brain_tag_graph", serde_json::json!({}));

    // Deliberately not `run_via_mcp`, which asserts success: the point is that
    // this call must FAIL.
    let raw = mcp_raw_frame(db, "brain_tag_graph", serde_json::json!({ "limit": -1 }));
    assert_eq!(
        raw["result"]["isError"],
        serde_json::json!(true),
        "`limit: -1` was accepted. It does not mean 'use the default' — it means the \
         caller made a mistake.\nbaseline for comparison: {baseline}\ngot: {raw}"
    );
}

/// Like `run_via_mcp` but returns the whole frame and does not assert success,
/// for the cases where the correct behaviour IS an error.
fn mcp_raw_frame(db_path: &Path, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
    use std::io::Write as _;
    use std::process::Stdio;

    let frames = [
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        }),
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }),
    ];
    let input = frames
        .iter()
        .map(|frame| serde_json::to_string(frame).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut child = StdCommand::new(bin_path())
        .args(["mcp", "--db", &db_path.display().to_string()])
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn nestweaver mcp");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("failed to read mcp output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["id"] == serde_json::json!(2))
        .unwrap_or_else(|| panic!("{tool}: no tools/call frame in MCP stdout:\n{stdout}"))
}

/// nw-317 leg 2. `--intent blast-radius` is documented by `brain context
/// --help`, accepted by `QueryIntent::from_str`, accepted on the CLI's direct
/// route — and was rejected through the daemon and on MCP by a JSON-Schema
/// `enum` that restated the parser and disagreed with it. One vocabulary,
/// three routes.
#[test]
fn intent_vocabulary_agrees_across_all_three_routes() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let args = &[
        "brain",
        "context",
        "mainA",
        "--limit",
        "3",
        "--intent",
        "blast-radius",
        "--json",
    ];
    let direct = run_direct(db, args);
    assert!(
        direct.status.success(),
        "direct route rejected a documented --intent value:\n{}",
        String::from_utf8_lossy(&direct.stderr)
    );

    let mcp = mcp_raw_frame(
        db,
        "brain_context",
        // `brain_context` takes no `limit` — `token_budget` is its bound.
        // Sending an undeclared key would fail on `additionalProperties`
        // rather than on the enum, which would pass this test for the wrong
        // reason.
        serde_json::json!({ "seeds": ["mainA"], "token_budget": 500, "intent": "blast-radius" }),
    );
    assert!(
        mcp["result"]["isError"] != serde_json::json!(true),
        "MCP rejected `intent: \"blast-radius\"`, which the engine's own parser accepts \
         and the CLI's --help documents. A schema that restates a parser is how the same \
         string came to be valid on `code_context` and invalid on `brain_context`: {mcp}"
    );

    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    let daemon = run_via_daemon(db, args);
    assert_eq!(
        direct.status.code(),
        daemon.status.code(),
        "`--intent blast-radius` diverges by route.\ndaemon stderr:\n{}",
        flatten_miette(&daemon.stderr)
    );
    let stderr = flatten_miette(&daemon.stderr);
    assert!(
        !stderr.contains("schema keyword"),
        "a CLI user who never invoked MCP was shown a raw JSON-Schema error: {stderr}"
    );
}

/// nw-217a. The containment guard, generalised from ONE tool to a table.
///
/// nw-217 is the most recurrent defect class in this workspace — "a guard
/// present in one implementation and absent in its twin" — and it decomposes
/// into six shapes that do NOT share one answer. This is the mechanical check
/// for the third and highest-leverage of them: response-shape drift between
/// routes. A key the CLI emits and MCP does not is a field an agent cannot see;
/// the converse is a field a human cannot see.
///
/// `the_mcp_route_does_not_grow_new_disclosure_gaps` has held `stale_check` to
/// this since nw-315 and held nothing else to it, which is how nw-316's three
/// missing `project_context` disclosure fields and nw-347's missing `_meta` on
/// `hubs`/`bridges` both survived a release that spent six commits on route
/// parity. Both of those rows are in this table now, and both fail without the
/// fixes in this branch.
///
/// The assertion is CONTAINMENT against an explicit `KNOWN_GAPS` list, not
/// equality: closing a gap shrinks the set safely, opening a new one fails, and
/// a deliberate asymmetry has to be written down with the reason.
///
/// NOT covered by this, and not claimed: shapes S1 (two dispatch seams — a
/// TYPE, `provenance_seam::Unstamped`), S2 (registry drift — enumeration),
/// S4 (argument-contract drift — the clap/schema cross-check, nw-217b),
/// S5 (guard call-site parity) and S6 (semantic divergence, which needs a
/// fixture that can tell two behaviours apart and is the parity harness, one
/// test at a time).
#[test]
fn no_cli_command_discloses_more_than_its_mcp_twin() {
    /// A gap is `(tool, key path)` with the finding that owns it. EMPTY is the
    /// goal; an entry is a promise, not a permission.
    const KNOWN_GAPS: &[(&str, &str)] = &[];

    // Rows are (CLI argv, MCP tool, MCP arguments). Arguments must be the
    // SAME question on both sides or the diff is about the question, not the
    // route.
    let rows: Vec<(Vec<&str>, &str, serde_json::Value)> = vec![
        (
            vec!["stale-check", "--json"],
            "stale_check",
            serde_json::json!({}),
        ),
        (
            vec!["hubs", "--json", "--top", "3"],
            "hub_nodes",
            serde_json::json!({ "top_n": 3 }),
        ),
        (
            vec!["bridges", "--json", "--top", "3"],
            "bridge_nodes",
            serde_json::json!({ "top_n": 3 }),
        ),
        (
            vec!["brain", "search", "mainA", "--json"],
            "brain_search",
            serde_json::json!({ "query": "mainA" }),
        ),
        (
            vec!["dead-code", "--json"],
            "dead_code",
            serde_json::json!({}),
        ),
        (
            vec!["flow-trace", "mainA", "--json"],
            "flow_trace",
            serde_json::json!({ "symbol": "mainA" }),
        ),
        (
            vec!["blast-radius", "--files", "src/a.js", "--json"],
            "blast_radius",
            serde_json::json!({ "changed_files": ["src/a.js"] }),
        ),
    ];

    let fixture = setup_fixture();
    let db = &fixture.db_path;
    let mut failures: Vec<String> = Vec::new();

    for (argv, tool, args) in rows {
        let label = argv.join(" ");
        let cli = run_direct(db, &argv);
        assert!(
            cli.status.success(),
            "{label} (direct) failed:\n{}",
            String::from_utf8_lossy(&cli.stderr)
        );
        let cli_json = parse_stdout(&label, &cli);
        let mcp_json = run_via_mcp(db, tool, args);

        let mcp_keys = json_key_paths(&mcp_json);
        let missing: Vec<String> = json_key_paths(&cli_json)
            .into_iter()
            .filter(|key| !mcp_keys.contains(key))
            .filter(|key| !KNOWN_GAPS.contains(&(tool, key.as_str())))
            .collect();
        if !missing.is_empty() {
            failures.push(format!(
                "`{label}` -> `{tool}`: {missing:?}\n  CLI: {cli_json}\n  MCP: {mcp_json}"
            ));
        }
    }

    // `project_context` needs a project, which is why this row could not exist
    // before nw-218 — `setup_fixture` creates none, so both routes answered
    // NOT_FOUND and any comparison passed on two identical failures.
    let project = setup_project_fixture();
    let argv = ["project-context", "demo", "--json", "--token-budget", "400"];
    let cli = run_direct(&project.db_path, &argv);
    assert!(
        cli.status.success(),
        "project-context (direct) failed:\n{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_json = parse_stdout("project-context", &cli);
    let mcp_json = run_via_mcp(
        &project.db_path,
        "project_context",
        serde_json::json!({ "project": "demo", "token_budget": 400 }),
    );
    let mcp_keys = json_key_paths(&mcp_json);
    let missing: Vec<String> = json_key_paths(&cli_json)
        .into_iter()
        .filter(|key| !mcp_keys.contains(key))
        .filter(|key| !KNOWN_GAPS.contains(&("project_context", key.as_str())))
        .collect();
    if !missing.is_empty() {
        failures.push(format!(
            "`project-context` -> `project_context`: {missing:?}\n  CLI: {cli_json}\n  MCP: {mcp_json}"
        ));
    }

    assert!(
        failures.is_empty(),
        "these fields reach a CLI caller and not an MCP one, and they are not in \
         the list of gaps this workspace knowingly left open:\n{}",
        failures.join("\n")
    );
}

/// The structural guard: for a tool with a CLI twin, any key the CLI emits and
/// MCP does not is a field an agent cannot see. The known gaps are listed
/// explicitly with the finding that owns them, and the assertion is
/// CONTAINMENT — closing one shrinks the set safely, opening a new one fails.
///
/// Kept alongside `no_cli_command_discloses_more_than_its_mcp_twin` rather than
/// folded into it: this one names `stale_check` in its failure message, which
/// is the row nw-315 closed, and its docstring records why the `._meta*`
/// entries that used to sit in `KNOWN_GAPS` were INERT.
#[test]
fn the_mcp_route_does_not_grow_new_disclosure_gaps() {
    /// EMPTY, and that is the point. nw-315 owned every entry that used to be
    /// here: `stale_check`'s two summary lists were derived in `src/main.rs`
    /// (twice — once per CLI route) instead of by the tool, so the MCP route,
    /// which calls `tools::dispatch` directly, never saw them.
    ///
    /// The `._meta*` entries that also sat here were INERT and are not evidence
    /// of anything: `stale-check --json` on the direct route emits no `_meta`
    /// at all, so a containment check CLI ⊆ MCP could never have failed on
    /// them. `_meta` on the MCP route is proved by
    /// `the_mcp_route_carries_the_provenance_its_own_instructions_promise`,
    /// which asserts on the field rather than on its absence from a diff.
    const KNOWN_GAPS: &[&str] = &[];

    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let cli = run_direct(db, &["stale-check", "--json"]);
    assert!(
        cli.status.success(),
        "stale-check (direct) failed:\n{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_json = parse_stdout("stale-check", &cli);
    let mcp_json = run_via_mcp(db, "stale_check", serde_json::json!({}));

    let mcp_keys = json_key_paths(&mcp_json);
    let missing: Vec<String> = json_key_paths(&cli_json)
        .into_iter()
        .filter(|key| !mcp_keys.contains(key))
        .filter(|key| !KNOWN_GAPS.contains(&key.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "stale_check: these fields reach a CLI caller and not an MCP one, and they are \
         not in the list of gaps this batch knowingly left open: {missing:?}\n\
         CLI: {cli_json}\nMCP: {mcp_json}"
    );
}

/// nw-315. `_meta` was never ADDED on the MCP stdio route — not dropped. The
/// four provenance authors were `src/main.rs` (CLI direct), the federation
/// client (CLI daemon), `http.rs` under a third and namespaced spelling, and
/// stdio: nothing. Meanwhile `SERVER_INSTRUCTIONS` — returned by `initialize`
/// on all three transports — tells the agent "Results include `_meta.sources`
/// indicating which data sources contributed". An agent therefore had no way to
/// learn that its answer was scoped, drawn from a partial source set, or built
/// on stale repos, while the human got all three for free. That is the precise
/// inverse of 8.0.0's "disclose stale rankings to the agent, not just to the
/// human".
///
/// Asserted on the WIRE (a real `nestweaver mcp` process) rather than on
/// `tools::dispatch`, because the unit test one layer down cannot see a
/// serialization step that drops the key.
#[test]
fn the_mcp_route_carries_the_provenance_its_own_instructions_promise() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    for (tool, args) in [
        ("brain_status", serde_json::json!({})),
        ("stale_check", serde_json::json!({})),
        ("dead_code", serde_json::json!({})),
        (
            "detect_changes",
            serde_json::json!({ "changed_files": ["src/a.js"] }),
        ),
        (
            "cross_repo_contracts",
            serde_json::json!({ "name": "mainA" }),
        ),
        ("flow_trace", serde_json::json!({ "symbol": "mainA" })),
    ] {
        let payload = run_via_mcp(db, tool, args);
        let meta = &payload["_meta"];
        assert!(
            meta["sources"].is_array(),
            "{tool}: the MCP response has no `_meta.sources`, but the server's \
             own `initialize` instructions promise the agent \"Results include \
             _meta.sources indicating which data sources contributed\": {payload}"
        );
        for leg in ["scope", "stale_repos"] {
            assert!(
                meta.get(leg).is_some(),
                "{tool}: `_meta` is missing `{leg}`, which the CLI emits — \
                 partial provenance is how an agent learns the wrong thing \
                 confidently: {payload}"
            );
        }
    }
}

/// The `stale_check` half of nw-315, on the wire. MCP returned
/// `[any_needs_reindex, any_stale, repo_count, repos]` where the CLI returned
/// those plus the two pre-summarised lists, so the agent was told "at least one
/// repo needs re-indexing" and had to linearly scan the array to find out
/// which. `needs_reindex_repos` is the field 8.0.0 added as a documented
/// breaking change; it had never reached the MCP surface at all.
#[test]
fn mcp_stale_check_reports_which_repos_not_merely_that_some_do() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let cli = run_direct(db, &["stale-check", "--json"]);
    assert!(
        cli.status.success(),
        "stale-check (direct) failed:\n{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_json = parse_stdout("stale-check", &cli);
    let mcp_json = run_via_mcp(db, "stale_check", serde_json::json!({}));

    for field in ["stale_repos", "needs_reindex_repos"] {
        assert_eq!(
            mcp_json[field], cli_json[field],
            "stale_check: `{field}` differs between the CLI and MCP routes — \
             the agent cannot act on a summary it is not given.\nCLI: \
             {cli_json}\nMCP: {mcp_json}"
        );
    }
}

/// nw-347. `_meta` is a promise `SERVER_INSTRUCTIONS` makes on every route, and
/// three of the four CLI emitters break it. `print_ranking_json` (`hubs`,
/// `bridges`) has no `_meta` parameter at all, and the `bridges` daemon leg
/// actively runs `strip_hybrid_meta` over the envelope before rendering it —
/// so the daemon's own stamp is discarded and the renderer has nothing to put
/// back. Meanwhile `hub_nodes`/`bridge_nodes` over MCP carry one, because
/// `tools::dispatch` stamps.
///
/// Asserted CLI-vs-MCP rather than CLI-vs-CLI because MCP is the route with no
/// presentation layer above the tool: whatever the CLI does not print, the
/// human never learns, and the two surfaces are documented to agree.
#[test]
fn every_json_cli_surface_carries_the_provenance_mcp_carries() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    for (argv, tool, args) in [
        (
            vec!["hubs", "--json", "--top", "3"],
            "hub_nodes",
            serde_json::json!({ "top_n": 3 }),
        ),
        (
            vec!["bridges", "--json", "--top", "3"],
            "bridge_nodes",
            serde_json::json!({ "top_n": 3 }),
        ),
        (
            vec!["brain", "search", "mainA", "--json"],
            "brain_search",
            serde_json::json!({ "query": "mainA" }),
        ),
    ] {
        let label = argv.join(" ");
        let cli = run_direct(db, &argv);
        assert!(
            cli.status.success(),
            "{label} failed:\n{}",
            String::from_utf8_lossy(&cli.stderr)
        );
        let cli_json = parse_stdout(&label, &cli);
        let mcp_json = run_via_mcp(db, tool, args);

        assert!(
            cli_json["_meta"]["sources"].is_array(),
            "`{label}` --json carries no `_meta`, while `{tool}` over MCP does. A \
             renderer that rebuilds from a typed struct dropped the field the tool \
             layer was given one author for (nw-315/nw-347): {cli_json}"
        );
        for leg in ["scope", "stale_repos"] {
            assert!(
                cli_json["_meta"].get(leg).is_some(),
                "`{label}`: partial provenance is how a caller learns the wrong \
                 thing confidently: {cli_json}"
            );
        }
        assert_eq!(
            cli_json["_meta"], mcp_json["_meta"],
            "`{label}`: the CLI and MCP disagree about where the same answer came \
             from"
        );
    }
}

/// nw-347, the sharpest leg: the split is INSIDE one command. `brain search
/// --json` prints `tools::dispatch`'s stamped payload verbatim on the direct
/// route (`src/main.rs`, the `BrainCommands::Search` direct leg) and rebuilds
/// field-by-field from `nestweaver_proto::BrainSearchResponse` on the daemon
/// route (`render_brain_search_response`), and that proto has no `_meta` field.
/// So the SHAPE of the answer tracks whether a daemon happens to be running
/// rather than what the caller asked for — nw-108's defect recurring on the
/// provenance field, on the DEFAULT route.
#[test]
fn brain_search_json_has_one_shape_whether_or_not_a_daemon_is_running() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;
    let argv = ["brain", "search", "mainA", "--json"];

    let direct = run_direct(db, &argv);
    assert!(
        direct.status.success(),
        "brain search (direct) failed:\n{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    let direct_json = parse_stdout("brain search (direct)", &direct);

    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    let daemon = run_via_daemon(db, &argv);
    assert!(
        daemon.status.success(),
        "brain search (daemon) failed:\n{}",
        flatten_miette(&daemon.stderr)
    );
    let daemon_json = parse_stdout("brain search (daemon)", &daemon);

    assert_eq!(
        direct_json["_meta"].is_object(),
        daemon_json["_meta"].is_object(),
        "`brain search --json` emits `_meta` on one route and not the other, so a \
         caller parsing the response has to know which transport answered.\n\
         direct: {direct_json}\ndaemon: {daemon_json}"
    );
    assert!(
        daemon_json["_meta"]["sources"].is_array(),
        "the daemon route lost the provenance the proto boundary could not \
         carry: {daemon_json}"
    );
}

/// nw-259(b). `--token-budget` got `range(1..=16000)` to match its schema;
/// `--limit`, declared six lines below it, got nothing — while `code_context`'s
/// schema carries `maximum: 5000` (with a comment explaining that the tool asks
/// for `limit + 1` and an unbounded value overflows it) and the daemon proxy
/// validates against that schema. So the same invocation was accepted or
/// rejected by whether a daemon happened to be running: the bound was a
/// property of the transport, not of the contract.
#[test]
fn context_limit_is_bounded_identically_on_both_routes() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;
    let args = &["context", "mainA", "--limit", "6000"];

    let direct = run_direct(db, args);

    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    let daemon = run_via_daemon(db, args);

    assert_eq!(
        direct.status.code(),
        daemon.status.code(),
        "`--limit 6000` is rejected on one route and accepted on the other.\n\
         direct ({:?}):\n{}\ndaemon ({:?}):\n{}",
        direct.status.code(),
        flatten_miette(&direct.stderr),
        daemon.status.code(),
        flatten_miette(&daemon.stderr)
    );
    assert_eq!(
        direct.status.code(),
        Some(64),
        "an out-of-range argument is a USAGE error; `--token-budget` already \
         classifies it that way on this same command"
    );

    // Counterweight: a value INSIDE the bound must still be accepted on both,
    // or a parser with the wrong range would satisfy the above.
    let ok_args = &["context", "mainA", "--limit", "5000"];
    assert!(
        run_direct(db, ok_args).status.success(),
        "5000 is the schema's maximum and must be accepted"
    );
    assert!(run_via_daemon(db, ok_args).status.success());
}

/// nw-259(a), machine route. **Which cap cut** must be readable by a consumer
/// that cannot read prose.
///
/// The human route already names the cause — `TRUNCATED by --token-budget 200
/// — raise it for more` versus `TRUNCATED at limit 5 — pass --limit for more`
/// — but that disclosure lived only in the `--stats` string. `--json` emitted
/// `{"total": 576, "limit": 5000, "truncated": true}` for a cut the BUDGET
/// made, so an agent read "truncated at limit 5000", raised `--limit`, and got
/// the same rows back. That is the wrong-remedy defect nw-259 exists to close,
/// still live for every script and every agent — the audience with no prose to
/// fall back on.
///
/// Asserted on all three routes, because the direct path, the daemon path and
/// the MCP tool each decide this independently and a fix to one is how they
/// diverged before.
#[test]
fn context_truncation_names_the_cause_on_every_route() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    // A LIMIT cut: the budget is roomy, so only `--limit` can have cut.
    let by_limit = ["context", "mainA", "--json", "--limit", "1"];
    // A BUDGET cut: the limit is the schema maximum and cannot bite, so only
    // the budget can have cut. This is the case that reported `limit`.
    let by_budget = [
        "context",
        "mainA",
        "--json",
        "--limit",
        "5000",
        "--token-budget",
        "1",
    ];
    // BOTH fired. The budget cut LAST, so raising `--limit` alone cannot get
    // past it — naming the limit here is the wrong remedy.
    let by_both = [
        "context",
        "mainA",
        "--json",
        "--limit",
        "1",
        "--token-budget",
        "1",
    ];
    // Neither fired: the field must not accuse a cap that did nothing.
    let uncapped = ["context", "mainA", "--json", "--limit", "5000"];

    let assert_cause =
        |route: &str, payload: &serde_json::Value, expected: Option<&str>| match expected {
            Some(cause) => {
                assert_eq!(
                    payload["truncated"],
                    serde_json::json!(true),
                    "{route}: the cap must actually bite or this proves nothing: {payload}"
                );
                assert_eq!(
                    payload.get("truncated_by").and_then(|v| v.as_str()),
                    Some(cause),
                    "{route}: a consumer cannot tell WHICH cap cut, so it will raise the \
                     wrong knob and get the same rows: {payload}"
                );
            }
            None => {
                assert_ne!(
                    payload["truncated"],
                    serde_json::json!(true),
                    "{route}: nothing was capped: {payload}"
                );
                assert!(
                    payload
                        .get("truncated_by")
                        .is_none_or(serde_json::Value::is_null),
                    "{route}: a complete answer must not blame a cap: {payload}"
                );
            }
        };

    // ── Route 1: direct ──
    for (args, expected) in [
        (&by_limit[..], Some("limit")),
        (&by_budget[..], Some("token_budget")),
        (&by_both[..], Some("token_budget")),
        (&uncapped[..], None),
    ] {
        let out = run_direct(db, args);
        assert!(
            out.status.success(),
            "context (direct) {args:?} failed:\n{}",
            flatten_miette(&out.stderr)
        );
        assert_cause(
            &format!("direct {args:?}"),
            &parse_stdout("context (direct)", &out),
            expected,
        );
    }

    // ── Route 3: MCP, before the daemon takes the DB lock ──
    //
    // `code_context` has ONE cap, so `truncated: true` is unambiguous there
    // *today* — but the CLI's daemon route parses this very payload and then
    // applies its own budget on top, so the cause has to be IN the payload for
    // route 2 to be able to override it rather than recompute it.
    let mcp_capped = run_via_mcp(
        db,
        "code_context",
        serde_json::json!({ "seeds": ["mainA"], "limit": 1 }),
    );
    assert_cause("mcp code_context limit=1", &mcp_capped, Some("limit"));
    let mcp_uncapped = run_via_mcp(
        db,
        "code_context",
        serde_json::json!({ "seeds": ["mainA"], "limit": 5000 }),
    );
    assert_cause("mcp code_context limit=5000", &mcp_uncapped, None);

    // ── Route 2: daemon ──
    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    for (args, expected) in [
        (&by_limit[..], Some("limit")),
        (&by_budget[..], Some("token_budget")),
        (&by_both[..], Some("token_budget")),
        (&uncapped[..], None),
    ] {
        let out = run_via_daemon(db, args);
        assert!(
            out.status.success(),
            "context (daemon) {args:?} failed:\n{}",
            flatten_miette(&out.stderr)
        );
        assert_cause(
            &format!("daemon {args:?}"),
            &parse_stdout("context (daemon)", &out),
            expected,
        );
    }
}

/// nw-259(a), the human half. `--stats` is OFF by default.
///
/// The truncation clause was built into the `--stats` line, so the DEFAULT
/// human output of a capped `context` was byte-identical to a complete one:
/// the reader saw `Connected (1 symbols, ranked by relevance)` and had nothing
/// to compare it against. Disclosure that only appears under an opt-in flag is
/// the same silence the cap was supposed to stop being.
///
/// Asserted with `--stats` absent on purpose. The counterweight is the second
/// half: an UNCAPPED run must stay quiet, or printing the notice
/// unconditionally would satisfy the first assertion.
#[test]
fn a_capped_context_says_so_with_stats_off() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let capped = run_direct(db, &["context", "mainA", "--limit", "1"]);
    let stdout = String::from_utf8_lossy(&capped.stdout);
    assert!(
        stdout.contains("TRUNCATED at limit 1"),
        "a capped result renders identically to a complete one with `--stats` \
         off, which is the whole defect:\n{stdout}"
    );
    assert!(
        stdout.contains("pass --limit for more"),
        "the notice must carry the remedy, not just the fact:\n{stdout}"
    );

    let budgeted = run_direct(
        db,
        &["context", "mainA", "--limit", "5000", "--token-budget", "1"],
    );
    let stdout = String::from_utf8_lossy(&budgeted.stdout);
    assert!(
        stdout.contains("--token-budget"),
        "the human notice must name the cap that actually cut, exactly as the \
         `--stats` line does — they read the SAME field:\n{stdout}"
    );
    assert!(
        !stdout.contains("pass --limit for more"),
        "a budget cut prescribed `--limit`, which cannot change the outcome:\n{stdout}"
    );

    let complete = run_direct(db, &["context", "mainA", "--limit", "5000"]);
    let stdout = String::from_utf8_lossy(&complete.stdout);
    assert!(
        !stdout.contains("TRUNCATED"),
        "nothing was capped, so nothing may be claimed:\n{stdout}"
    );
}

/// Where else does the property hold? `summary` — TWO caps, one boolean.
///
/// A `summary` result can be cut by the level's generator cap (500 symbols, 50
/// clusters, 30 hubs — none of them a knob the caller passed) or by
/// `--token-budget`, and both routes reported a single `truncated` for both.
/// The remedies are different — narrow with `--target`, or raise the budget —
/// so `truncated: true` alone leaves a consumer guessing.
///
/// Named as INDEPENDENT booleans, not as `context`'s single `truncated_by`
/// string, because these two caps do not compose in an order: both remedies
/// stay useful when both fire. That is `brain_impact`'s existing shape
/// (`truncated_by_depth` / `truncated_by_threshold`), not a new one.
#[test]
fn summary_names_which_cap_cut_on_both_routes() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let cli = run_direct(
        db,
        &[
            "summary",
            "--level",
            "file",
            "--json",
            "--token-budget",
            "1",
        ],
    );
    assert!(
        cli.status.success(),
        "summary (direct) failed:\n{}",
        flatten_miette(&cli.stderr)
    );
    let payload = parse_stdout("summary (direct)", &cli);
    assert_eq!(
        payload["truncated"],
        serde_json::json!(true),
        "the budget must bite or this proves nothing: {payload}"
    );
    assert_eq!(
        payload["truncated_by_budget"],
        serde_json::json!(true),
        "a budget cut must say so: {payload}"
    );
    assert_eq!(
        payload["truncated_by_cap"],
        serde_json::json!(false),
        "the generator cap did not fire; blaming it sends the caller to \
         `--target`, which cannot help here: {payload}"
    );

    let mcp = run_via_mcp(
        db,
        "get_summary",
        serde_json::json!({ "level": "file", "token_budget": 1 }),
    );
    assert_eq!(
        mcp["truncated"],
        serde_json::json!(true),
        "the budget must bite on the MCP route too: {mcp}"
    );
    assert_eq!(
        mcp["truncated_by_budget"],
        serde_json::json!(true),
        "the agent-facing route is the one with no prose to fall back on: {mcp}"
    );
    assert_eq!(mcp["truncated_by_cap"], serde_json::json!(false));

    // Counterweight: an unbounded run must set neither, or hardcoding `true`
    // would satisfy everything above.
    let roomy = run_direct(
        db,
        &[
            "summary",
            "--level",
            "file",
            "--json",
            "--token-budget",
            "0",
        ],
    );
    let payload = parse_stdout("summary (unbounded)", &roomy);
    assert_eq!(payload["truncated"], serde_json::json!(false), "{payload}");
    assert_eq!(
        payload["truncated_by_budget"],
        serde_json::json!(false),
        "{payload}"
    );
    assert_eq!(
        payload["truncated_by_cap"],
        serde_json::json!(false),
        "{payload}"
    );
}

/// nw-353. `brain context` is the most-called retrieval surface in the
/// catalogue and its MACHINE routes cannot say they were cut. `budgeted_cut`
/// computes the cut count, the caller keeps only the slice index, and
/// `result.connected.len()` — the pre-cap total, in scope on the line above —
/// is dropped. A capped answer is then byte-indistinguishable from a complete
/// one.
///
/// The HUMAN route already discloses: `print_brain_context_text` prints
/// `Connected (N of M, ...)`. So this is nw-259(a)'s shape exactly — the
/// human sees the cap and the agent does not — and the fix has a local model
/// to copy rather than a design to invent.
///
/// The counterweight is the last block: an UNCAPPED call must report
/// `truncated: false` with `returned == total` and blame no cap, or a fix
/// that flags unconditionally passes.
#[test]
fn brain_context_discloses_that_it_was_cut_on_every_machine_route() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;

    let assert_bounded = |route: &str, payload: &serde_json::Value, cut: bool| {
        for key in ["returned", "total", "truncated"] {
            assert!(
                payload.get(key).is_some(),
                "{route}: `{key}` absent — a capped answer is byte-identical to a \
                 complete one: {payload}"
            );
        }
        let returned = payload["returned"].as_u64().unwrap();
        let total = payload["total"].as_u64().unwrap();
        assert_eq!(
            returned,
            payload["connected"].as_array().unwrap().len() as u64,
            "{route}: `returned` must be the length of the list actually returned: {payload}"
        );
        assert_eq!(
            payload["truncated"],
            serde_json::json!(returned < total),
            "{route}: `truncated` must agree with the counts beside it: {payload}"
        );
        assert_eq!(
            payload["truncated"],
            serde_json::json!(cut),
            "{route}: expected truncated={cut}: {payload}"
        );
    };

    // ── CLI direct, cut by --limit ──
    let capped = run_direct(db, &["brain", "context", "mainA", "--json", "--limit", "1"]);
    assert!(
        capped.status.success(),
        "{}",
        flatten_miette(&capped.stderr)
    );
    let capped = parse_stdout("brain context (direct, limit 1)", &capped);
    assert_bounded("direct --limit 1", &capped, true);
    assert_eq!(
        capped.get("truncated_by").and_then(|v| v.as_str()),
        Some("limit"),
        "a consumer that cannot tell WHICH cap cut raises the wrong knob: {capped}"
    );

    // ── CLI direct, cut by --token-budget; the budget takes precedence ──
    let by_budget = run_direct(
        db,
        &[
            "brain",
            "context",
            "mainA",
            "--json",
            "--limit",
            "1",
            "--token-budget",
            "1",
        ],
    );
    assert!(
        by_budget.status.success(),
        "{}",
        flatten_miette(&by_budget.stderr)
    );
    let by_budget = parse_stdout("brain context (direct, both caps)", &by_budget);
    assert_bounded("direct --limit 1 --token-budget 1", &by_budget, true);
    assert_eq!(
        by_budget.get("truncated_by").and_then(|v| v.as_str()),
        Some("token_budget"),
        "the budget is applied INSTEAD of --limit here, so naming --limit is the \
         wrong remedy: {by_budget}"
    );

    // ── MCP ──
    let mcp = run_via_mcp(
        db,
        "brain_context",
        serde_json::json!({ "seeds": ["mainA"], "token_budget": 1 }),
    );
    assert_bounded("mcp token_budget=1", &mcp, true);
    assert_eq!(
        mcp.get("truncated_by").and_then(|v| v.as_str()),
        Some("token_budget"),
        "the MCP route has exactly one cap and must still name it: {mcp}"
    );

    // ── CLI via daemon — the route this test's NAME already claimed ──
    //
    // nw-353 fixed `tool_brain_context` and the CLI's DIRECT arm and stopped.
    // This test asserted `run_direct` and `run_via_mcp` while calling itself
    // "every machine route", so the third leg was never run — the same
    // unasserted-route shape the gap it was written to close.
    //
    // The daemon runs the very `tool_brain_context` the MCP leg above proves
    // correct, so `total` is already on the wire; `BrainContextResult` has no
    // field for it, so `serde_json::from_value` dropped it and the renderer
    // re-derived `total` from the rows that SURVIVED the daemon's cut. That
    // makes `returned < total` structurally unreachable here — the answer
    // measured against itself always looks complete.
    let via_daemon = run_via_daemon(
        db,
        &["brain", "context", "mainA", "--json", "--token-budget", "1"],
    );
    assert!(
        via_daemon.status.success(),
        "{}",
        flatten_miette(&via_daemon.stderr)
    );
    let via_daemon = parse_stdout("brain context (daemon, token_budget 1)", &via_daemon);
    assert_bounded("daemon --token-budget 1", &via_daemon, true);
    assert_eq!(
        via_daemon.get("truncated_by").and_then(|v| v.as_str()),
        Some("token_budget"),
        "the daemon route must name the cap the direct and MCP routes name: {via_daemon}"
    );

    // The three routes must agree on how many rows MATCHED, not merely each be
    // self-consistent. Re-deriving `total` post-cut satisfies every
    // single-route invariant above while still disagreeing with the other two.
    assert_eq!(
        via_daemon["total"], mcp["total"],
        "daemon and MCP run the same tool over the same DB and must report the \
         same pre-cut total: daemon={via_daemon} mcp={mcp}"
    );

    // ── COUNTERWEIGHT: nothing cut, nothing claimed ──
    let roomy = run_direct(
        db,
        &[
            "brain",
            "context",
            "mainA",
            "--json",
            "--limit",
            "5000",
            "--token-budget",
            "16000",
        ],
    );
    assert!(roomy.status.success(), "{}", flatten_miette(&roomy.stderr));
    let roomy = parse_stdout("brain context (roomy)", &roomy);
    assert_bounded("direct roomy", &roomy, false);
    assert!(
        roomy
            .get("truncated_by")
            .is_none_or(serde_json::Value::is_null),
        "a complete answer must not blame a cap: {roomy}"
    );
    let mcp_roomy = run_via_mcp(
        db,
        "brain_context",
        serde_json::json!({ "seeds": ["mainA"], "token_budget": 16000 }),
    );
    assert_bounded("mcp roomy", &mcp_roomy, false);
    assert!(
        mcp_roomy
            .get("truncated_by")
            .is_none_or(serde_json::Value::is_null),
        "a complete answer must not blame a cap: {mcp_roomy}"
    );
    // The daemon leg needs its own counterweight: carrying an upstream `total`
    // through must not make an UNCUT answer claim it was cut.
    let daemon_roomy = run_via_daemon(
        db,
        &[
            "brain",
            "context",
            "mainA",
            "--json",
            "--limit",
            "5000",
            "--token-budget",
            "16000",
        ],
    );
    assert!(
        daemon_roomy.status.success(),
        "{}",
        flatten_miette(&daemon_roomy.stderr)
    );
    let daemon_roomy = parse_stdout("brain context (daemon, roomy)", &daemon_roomy);
    assert_bounded("daemon roomy", &daemon_roomy, false);
    assert!(
        daemon_roomy
            .get("truncated_by")
            .is_none_or(serde_json::Value::is_null),
        "a complete answer must not blame a cap: {daemon_roomy}"
    );
}

/// A fixture with more edge-bearing symbols than `generate_hub_summaries_bounded`'s
/// internal `HUB_COUNT` of 30, so that cap actually BITES.
///
/// `setup_fixture` indexes four `.js` files and produces ~8 edge-bearing
/// symbols, which is under every summary cap in the tree — so a cap-disclosure
/// test written against it passes VACUOUSLY with `truncated_by_cap: false` on
/// both sides. That trap is the reason this exists rather than a comment
/// saying the fixture is too small.
fn setup_hub_capped_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("db").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // A 40-link call chain: every function has at least one edge, so
    // `candidate_total` is 40 against a `HUB_COUNT` of 30 and the generator
    // drops exactly 10.
    const CHAIN: usize = 40;
    let mut body = String::new();
    for i in 0..CHAIN {
        if i + 1 < CHAIN {
            body.push_str(&format!(
                "export function fn_{i}(x) {{ return fn_{next}(x) + {i}; }}\n",
                next = i + 1
            ));
        } else {
            body.push_str(&format!(
                "export function fn_{i}(x) {{ return x + {i}; }}\n"
            ));
        }
    }
    write_repo_files(&repo_dir, &[("src/chain.js", body.as_str())]);
    create_db(&repo_dir, &db_path);

    Fixture {
        _dir: dir,
        db_path,
        repo_dir,
    }
}

/// nw-361. The cap is a property of the SET, not of the code path that
/// produced it. On a sidecar cache hit the generator does not run, so
/// `cap_dropped` stays 0 and a set that WAS capped when it was written reports
/// `truncated_by_cap: false` when it is read back.
///
/// Worse than a plain omission because it is correct on the COLD path, which
/// is the path anyone verifying it will use — and because the CLI is the
/// WRITER: `nestweaver summary --level hub` persists the already-capped set
/// that `get_summary` later reads back and calls complete.
///
/// TWO counterweights, both required:
///   - warm must EQUAL cold, not merely be non-false;
///   - a level with no generator cap must report false on both, or a fix that
///     flags unconditionally passes.
#[test]
fn a_cached_summary_still_reports_the_cap_that_produced_it() {
    let fixture = setup_hub_capped_fixture();
    let db = &fixture.db_path;

    // Cold read. `no_cache` bypasses the F16 RESPONSE cache — without it the
    // second call below replays this very payload byte for byte and the
    // sidecar is never consulted at all, which is how a cold/warm pair written
    // the obvious way passes while proving nothing. It does NOT stop the
    // generated set being written to the summary sidecar.
    let cold = run_via_mcp(
        db,
        "get_summary",
        serde_json::json!({ "level": "hub", "no_cache": true }),
    );
    assert_eq!(
        cold["cached"],
        serde_json::json!(false),
        "the first call must be a MISS or this test proves nothing: {cold}"
    );
    assert_eq!(
        cold["truncated_by_cap"],
        serde_json::json!(true),
        "the generator's HUB_COUNT must actually bite on this fixture, or every \
         assertion below passes vacuously: {cold}"
    );

    // Warm read: the same question, served from `<db>.summaries.json`.
    let warm = run_via_mcp(db, "get_summary", serde_json::json!({ "level": "hub" }));
    assert_eq!(
        warm["cached"],
        serde_json::json!(true),
        "the second call must HIT the sidecar or this test proves nothing: {warm}"
    );
    for field in ["truncated", "truncated_by_cap", "total", "total_available"] {
        assert_eq!(
            cold[field], warm[field],
            "`{field}` changed between a cold and a warm read of the SAME set, so \
             how much was dropped depends on whether the cache happened to be \
             warm\ncold: {cold}\nwarm: {warm}"
        );
    }

    // COUNTERWEIGHT: `file` level has no generator cap, so both reads must
    // report false — otherwise flagging unconditionally satisfies the above.
    let cold_file = run_via_mcp(
        db,
        "get_summary",
        serde_json::json!({ "level": "file", "no_cache": true }),
    );
    let warm_file = run_via_mcp(db, "get_summary", serde_json::json!({ "level": "file" }));
    for (label, payload) in [("cold", &cold_file), ("warm", &warm_file)] {
        assert_eq!(
            payload["truncated_by_cap"],
            serde_json::json!(false),
            "{label}: `file` has no generator cap; blaming one sends the caller \
             to `--target`, which cannot help: {payload}"
        );
    }
}

/// nw-361, the real-world sequence and the half the finding does not name:
/// the CLI is the WRITER. `Commands::Summary` never calls `load_summaries` —
/// it always regenerates, so its own `truncated_by_cap` is accurate — and it
/// persists the already-capped set that MCP then reads back. So the two routes
/// disagree about the SAME bytes on disk.
#[test]
fn the_cli_writes_a_capped_summary_set_that_mcp_must_still_call_capped() {
    let fixture = setup_hub_capped_fixture();
    let db = &fixture.db_path;

    let cli = run_direct(
        db,
        &["summary", "--level", "hub", "--json", "--token-budget", "0"],
    );
    assert!(cli.status.success(), "{}", flatten_miette(&cli.stderr));
    let cli = parse_stdout("summary (direct)", &cli);
    assert_eq!(
        cli["truncated_by_cap"],
        serde_json::json!(true),
        "the CLI regenerates and so knows the cap fired; if it does not, this \
         fixture is too small and the test below proves nothing: {cli}"
    );

    let mcp = run_via_mcp(db, "get_summary", serde_json::json!({ "level": "hub" }));
    assert_eq!(
        mcp["cached"],
        serde_json::json!(true),
        "the CLI must have written the sidecar MCP reads back, or the routes \
         are not looking at the same set: {mcp}"
    );
    assert_eq!(
        cli["truncated_by_cap"], mcp["truncated_by_cap"],
        "the CLI wrote this set and knows it was capped; MCP reads the same set \
         back and does not\ncli: {cli}\nmcp: {mcp}"
    );
    assert_eq!(
        cli["total"], mcp["total"],
        "and the population disagrees too, because `total_available` is built \
         from the same dropped count\ncli: {cli}\nmcp: {mcp}"
    );
}

/// nw-357. `impact`'s result-set cap is a property of the TRANSPORT: the
/// daemon route defaults `limit` to 50 from the `brain_impact` schema, and the
/// CLI had no `--limit` at all — it neither declared one nor sent one, so the
/// direct route capped nothing. `returned < total`, the documented way to
/// detect the cap, was therefore structurally impossible on the direct route,
/// and the two routes returned DIFFERENT ROW COUNTS for the same command.
///
/// That is worse than the field-meaning divergence it was filed as. It is
/// nw-259(b)'s shape verbatim — the bound is a property of the transport, not
/// of the contract — which also makes it an nw-217b instance.
///
/// The counterweight is the second half: a limit the result set FITS must
/// report `returned == total` on both routes, or a fix that always reports a
/// cap passes.
#[test]
fn impact_applies_the_same_result_set_cap_on_both_routes() {
    let fixture = setup_hub_capped_fixture();
    let db = &fixture.db_path;
    // The 40-link chain gives `fn_39` far more than one transitive dependent,
    // so a `--limit 1` genuinely cuts on a route that applies one.
    let args = &["impact", "fn_39", "--json", "--limit", "1", "--depth", "15"];

    let direct = run_direct(db, args);
    assert!(
        direct.status.success(),
        "impact (direct) failed:\n{}",
        flatten_miette(&direct.stderr)
    );
    let direct = parse_stdout("impact (direct)", &direct);
    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    let daemon = run_via_daemon(db, args);
    assert!(
        daemon.status.success(),
        "impact (daemon) failed:\n{}",
        flatten_miette(&daemon.stderr)
    );
    let daemon = parse_stdout("impact (daemon)", &daemon);

    assert_eq!(
        direct["returned"], daemon["returned"],
        "the same --limit returns a different number of rows depending on \
         whether a daemon is running\ndirect: {direct}\ndaemon: {daemon}"
    );
    assert_eq!(
        direct["total"], daemon["total"],
        "`total` means two things\ndirect: {direct}\ndaemon: {daemon}"
    );
    for (label, payload) in [("direct", &direct), ("daemon", &daemon)] {
        assert_eq!(
            payload["truncated"],
            serde_json::json!(true),
            "{label}: the cap must actually bite or this proves nothing: {payload}"
        );
        assert!(
            payload["returned"].as_u64().unwrap() < payload["total"].as_u64().unwrap(),
            "{label}: `returned < total` is the documented cap signal and it is \
             structurally unreachable here: {payload}"
        );
        // nw-357 step 2. `truncated` alone cannot say WHICH of the three caps
        // fired, and the three remedies are independent: raise `--depth`,
        // lower `--min-score`, raise `--limit`.
        assert_eq!(
            payload["truncated_by_limit"],
            serde_json::json!(true),
            "{label}: the result-set cap has no flag beside the depth and \
             threshold ones, so a caller reads `truncated` and raises the wrong \
             knob: {payload}"
        );
    }

    // COUNTERWEIGHT: a limit the set fits must report no cap on either route.
    let roomy = &[
        "impact", "fn_39", "--json", "--limit", "1000", "--depth", "15",
    ];
    let roomy_direct = parse_stdout("impact (direct, roomy)", &run_direct(db, roomy));
    let roomy_daemon = parse_stdout("impact (daemon, roomy)", &run_via_daemon(db, roomy));
    for (label, payload) in [("direct", &roomy_direct), ("daemon", &roomy_daemon)] {
        assert_eq!(
            payload["returned"], payload["total"],
            "{label}: nothing was capped: {payload}"
        );
        assert_eq!(
            payload["truncated_by_limit"],
            serde_json::json!(false),
            "{label}: a complete answer must not blame the limit: {payload}"
        );
    }
    assert_eq!(roomy_direct["returned"], roomy_daemon["returned"]);
    assert!(
        roomy_direct["returned"].as_u64().unwrap() > 1,
        "the roomy leg must return more than the capped leg, or the two halves \
         of this test are the same measurement: {roomy_direct}"
    );
}

/// nw-358. `stale_repos` is produced by two different functions reading two
/// different universes, so `hubs --json` answers differently depending on
/// whether a daemon happens to be running — in ORDER, and (when the sidecar is
/// incomplete) in CONTENT.
///
/// TWO CORRECTIONS to the finding, both load-bearing:
///   - NEITHER route sorts. It is `ResolverGenerations::repos`, a `BTreeMap`,
///     whose `.keys()` are lexicographic INCIDENTALLY, against
///     `GraphStore::list_repos`, which issues `MATCH (r:Repo) RETURN` with no
///     `ORDER BY`. No `.sort()` exists on either path.
///   - it does NOT need 43 repos and is NOT invisible to fixtures. It needs
///     TWO, inserted so the store's scan order is not lexicographic.
///
/// And a THIRD divergence the finding does not name: the daemon ALREADY ships
/// the exact answer (`attach_ranking_staleness`, computed from
/// `store.list_repos`) and the CLI discards it to recompute a weaker
/// sidecar-only one whose own docstring admits it under-approximates.
#[test]
fn stale_repos_is_the_same_list_on_every_route() {
    let fixture = setup_fixture();
    let db = &fixture.db_path;
    {
        let store = nestweaver_store::GraphStore::open_or_create(db).unwrap();
        // REVERSE lexicographic insertion, so the graph's scan order is not
        // sorted and the two producers are distinguishable with two repos.
        for uid in ["repo:zeta", "repo:alpha"] {
            store
                .insert_repo(&nestweaver_schema::Repo {
                    uid: uid.to_string(),
                    url: format!("file:///tmp/{uid}"),
                    indexed_sha: String::new(),
                    staleness_commits_behind: 0,
                    instance_id: "default".to_string(),
                    name: None,
                    root_path: None,
                })
                .unwrap();
        }
    }
    // Drop the generation record. Every repo then reads generation 0 and is
    // stale, so both routes select the same SET — and it is also what exposes
    // the third divergence, because the sidecar route enumerates the
    // SIDECAR's repos (now none) while the store route enumerates the GRAPH's.
    std::fs::remove_file(nestweaver_engine::sidecar_path(
        db,
        nestweaver_engine::resolver_generation::RESOLVER_GENERATION_SIDECAR,
    ))
    .ok();

    let args = &["hubs", "--json", "--top", "3"];
    let direct = parse_stdout("hubs (direct)", &run_direct(db, args));
    let mcp = run_via_mcp(db, "hub_nodes", serde_json::json!({ "top_n": 3 }));
    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    let daemon = parse_stdout("hubs (daemon)", &run_via_daemon(db, args));

    // The set must be non-empty on the route that can see it, or all three
    // agree vacuously.
    assert!(
        direct["stale_repos"].as_array().unwrap().len() >= 3,
        "no repo read as stale, so this proves nothing: {direct}"
    );
    assert_eq!(
        direct["stale_repos"], daemon["stale_repos"],
        "one command, two answers, selected by whether a daemon happens to be \
         running\ndirect: {direct}\ndaemon: {daemon}"
    );
    assert_eq!(
        direct["stale_repos"], mcp["stale_repos"],
        "the agent-facing route disagrees with the CLI it is supposed to \
         mirror\ncli: {direct}\nmcp: {mcp}"
    );
    assert_eq!(
        direct["rankings_stale"], daemon["rankings_stale"],
        "the boolean derived from the list must agree too\ndirect: {direct}\ndaemon: {daemon}"
    );

    // And it must be SORTED, so the answer is stable across databases and not
    // merely across routes on this one. The repos above were inserted in
    // reverse order, so scan order alone cannot satisfy this.
    let listed: Vec<&str> = direct["stale_repos"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    assert_eq!(
        listed, sorted,
        "ordering must be a property of the SET, not of whichever container \
         the caller happened to enumerate: {direct}"
    );

    // `bridges` is downstream of the same edges and the same two producers.
    let bridge_args = &["bridges", "--json", "--top", "3"];
    let bridges_direct = parse_stdout("bridges (direct)", &run_via_daemon(db, bridge_args));
    assert_eq!(
        bridges_direct["stale_repos"], direct["stale_repos"],
        "`bridges` recomputes the same answer through the same pair of \
         functions and must not be left behind: {bridges_direct}"
    );
}

// ─── nw-218: the three-route VALUE comparator ────────────────────────────────

/// Compare the VALUES of named fields across all three routes.
///
/// `assert_same_key_sets` answers "can this route SEE the field", which is the
/// nw-217(a) question, and both containment tables in this file compare key
/// PATHS in ONE direction (CLI subset of MCP). That is exactly right for the
/// defect they were built for and it cannot express this cluster's: a field
/// present on every route whose VALUE differs. nw-316 (`more_available`
/// differing by route) and nw-358 (`stale_repos` differing by route) are both
/// instances, and neither was assertable here before.
///
/// `legitimate` names the fields ALLOWED to differ, WITH the reason, in code.
/// Two reasons for that: a suite that goes red for a known-benign reason stops
/// being read, and — more importantly — a difference that is written down can
/// be RE-EXAMINED when it turns out to be the defect. nw-316's residual may BE
/// `semantic_applied`, which this list currently excuses; keeping the excuse in
/// prose on a ticket is how it stayed unexamined for two rounds.
fn assert_routes_agree_on(
    command: &str,
    fields: &[&str],
    legitimate: &[(&str, &str)],
    direct: &serde_json::Value,
    daemon: &serde_json::Value,
    mcp: &serde_json::Value,
) {
    for field in fields {
        if let Some((_, reason)) = legitimate.iter().find(|(name, _)| name == field) {
            // Not asserted — but RECORDED, so a run's log says what was
            // excused and why rather than silently omitting it.
            println!(
                "{command}: `{field}` excused: {reason}\n  direct={} daemon={} mcp={}",
                direct[*field], daemon[*field], mcp[*field]
            );
            continue;
        }
        assert_eq!(
            direct[*field], daemon[*field],
            "{command}: `{field}` differs between the DIRECT and DAEMON routes, so \
             the answer depends on whether a daemon happens to be \
             running\ndirect: {direct}\ndaemon: {daemon}"
        );
        assert_eq!(
            direct[*field], mcp[*field],
            "{command}: `{field}` differs between the CLI and the MCP tool it is \
             supposed to mirror\ncli: {direct}\nmcp: {mcp}"
        );
    }
}

/// nw-316 / nw-218. `project-context` answered from THREE routes, with every
/// field the experiment specified recorded rather than sampled.
///
/// The residual has TWO ambient inputs, not one:
///   1. the instance config (six ranking parameters read from
///      `current_instance_config()`), which is the CLI's `--config` on the
///      direct route and the DAEMON's boot config on the other two;
///   2. the embedding model — the direct route passes `None` and the daemon
///      passes its warm model — which changes which retrieval LEGS ran at all
///      and is therefore a bigger lever on the result set than the config.
///
/// The restored `parity_project_context_direct_vs_daemon` excuses
/// `semantic_applied` as a benign difference while asserting that
/// `more_available` — which moves when the semantic leg moves — must agree.
/// If input 2 is the residual, that pair of claims cannot both hold. This test
/// exists to make that visible in code rather than in a ticket.
#[test]
fn project_context_answers_the_same_on_all_three_routes() {
    let fixture = setup_project_fixture();
    let db = &fixture.db_path;
    let args = &["project-context", "demo", "--json", "--token-budget", "400"];

    let direct = parse_stdout("project-context (direct)", &run_direct(db, args));
    let mcp = run_via_mcp(
        db,
        "project_context",
        // The SAME arguments the CLI sends. `response_format` defaults to
        // "concise" on the CLI and "detailed" in the schema, so omitting it
        // here would manufacture a difference that has nothing to do with the
        // two ambient inputs under test.
        serde_json::json!({
            "project": "demo",
            "token_budget": 400,
            "response_format": "concise",
        }),
    );
    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    let daemon = parse_stdout("project-context (daemon)", &run_via_daemon(db, args));

    // The full record the experiment asks for, printed on every run so a
    // failure is diagnosable without re-running it.
    println!("nw-316 record\n  direct: {direct}\n  daemon: {daemon}\n  mcp:    {mcp}");

    assert_routes_agree_on(
        "project-context",
        &[
            "truncated",
            "more_available",
            "seed_tokens_charged",
            "seeds_expanded",
            "tokens_used",
            "budget_exceeded",
            "semantic_applied",
            "degraded_components",
        ],
        &[
            // The ONLY two entries, and both are on notice. `tools.rs` reads
            // the embedding model from whoever dispatched, so the direct route
            // has none; that is nw-120's declined tradeoff, not a fact about
            // the project. If it ever differs while `more_available` also
            // differs, this list is wrong and nw-316's option (ii) or (iii) is
            // the fix — not a wider exclusion list.
            (
                "semantic_applied",
                "the direct route passes `embed_model: None` (main.rs) and the daemon                  passes its warm model (server.rs) — nw-120's declined tradeoff",
            ),
            (
                "degraded_components",
                "derived from `semantic_applied`; excusing one and asserting the other                  would be incoherent",
            ),
        ],
        &direct,
        &daemon,
        &mcp,
    );
}

/// nw-218 step 2, row 3 (nw-357). `impact`'s counts are now comparable across
/// all THREE routes, which they could not be before `--limit` existed: the
/// direct route capped nothing, so `total` and `returned` were the same
/// number there and a different pair everywhere else.
///
/// Deliberately NOT a row in `no_cli_command_discloses_more_than_its_mcp_twin`:
/// that table compares key PATHS, and `impact`'s two envelopes name the same
/// list `nodes` and `impact_nodes` and the same subject `symbol` and `target`.
/// Adding it there would go red for a naming divergence that has nothing to do
/// with this item, and a `KNOWN_GAPS` entry to silence it would be a promise
/// nobody made. The VALUES are what nw-357 is about, so this is the table it
/// belongs in.
#[test]
fn impact_counts_agree_across_all_three_routes() {
    let fixture = setup_hub_capped_fixture();
    let db = &fixture.db_path;
    let args = &["impact", "fn_39", "--json", "--limit", "5", "--depth", "15"];

    let direct = parse_stdout("impact (direct)", &run_direct(db, args));
    let mcp = run_via_mcp(
        db,
        "brain_impact",
        serde_json::json!({ "symbol": "fn_39", "limit": 5, "depth": 15 }),
    );
    let _guard = DaemonGuard::new(db);
    start_daemon(db);
    let daemon = parse_stdout("impact (daemon)", &run_via_daemon(db, args));

    println!("nw-357 record\n  direct: {direct}\n  daemon: {daemon}\n  mcp:    {mcp}");

    assert_routes_agree_on(
        "impact",
        &[
            "total",
            "returned",
            "truncated",
            "truncated_by_limit",
            "truncated_by_depth",
            "truncated_by_threshold",
        ],
        // EMPTY. There is no legitimate reason for any of these six to differ:
        // they are counts of the same traversal under the same three caps.
        &[],
        &direct,
        &daemon,
        &mcp,
    );
    assert_eq!(
        direct["truncated_by_limit"],
        serde_json::json!(true),
        "the cap must bite or the agreement above is vacuous: {direct}"
    );
}
