//! Integration tests for the NestWeaver daemon lifecycle.
//!
//! These tests exercise the full daemon path: start, stop, auto-start, crash
//! recovery, concurrent MCP queries, and brain_add_source through the daemon.
//!
//! Run with:
//!   cargo test --test daemon_test -- --test-threads=1

use assert_cmd::Command;
use nestweaver_engine::{load_filemeta_sidecar, save_filemeta_sidecar, sidecar_path};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::io::Write;
use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

/// Helper: build a `Command` for the `nestweaver` binary **without** setting
/// `NESTWEAVER_NO_DAEMON`. This is the key difference from `cli_test.rs`'s
/// `nestweaver_cmd()` — we want the daemon path exercised.
fn daemon_cmd() -> Command {
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    cmd.env_remove("NESTWEAVER_NO_DAEMON");
    // Use fork-based daemon in tests — launchd agents don't work
    // reliably in the cargo test environment.
    cmd.env("NESTWEAVER_DAEMON_FORK", "1");
    cmd
}

/// Helper: build a `Command` with `NESTWEAVER_NO_DAEMON=1` for initial DB
/// creation (before daemon tests).
fn no_daemon_cmd() -> Command {
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    cmd.env("NESTWEAVER_NO_DAEMON", "1");
    cmd
}

/// Build a daemon subcommand with the correct arg order:
///   `nestweaver daemon --db <path> <action> [extra_args...]`
fn daemon_action_cmd(db_path: &Path, action: &str) -> Command {
    let mut cmd = daemon_cmd();
    cmd.args(["daemon", "--db", &db_path.display().to_string(), action]);
    cmd
}

/// Start a daemon and wait until its Unix socket is accepting connections.
///
/// Fork-based `daemon start` returns before the child binds the socket. Tests
/// must synchronize on readiness instead of guessing how long startup takes,
/// especially when the workspace suite starts several daemons in parallel.
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

#[test]
fn daemon_readiness_timeout_runs_cleanup_before_returning_error() {
    let cleaned = std::cell::Cell::new(false);
    let error = wait_for_daemon_readiness(
        Duration::ZERO,
        Duration::ZERO,
        || {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not ready",
            ))
        },
        || cleaned.set(true),
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(cleaned.get());
}

/// Create a minimal git repo with a JS file for indexing.
fn write_test_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    StdCommand::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::fs::write(dir.join("main.js"), "function greet(name) { return name; }").unwrap();
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

/// Create a minimal vault (directory with `.md` files, no `.obsidian/`).
fn write_test_vault(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("note1.md"),
        "# Hello\nThis is a test note.\n## Section\nSome content.",
    )
    .unwrap();
    std::fs::write(
        dir.join("note2.md"),
        "# World\nAnother note with [[Hello]] link.",
    )
    .unwrap();
}

/// Resolve the path to the built `nestweaver` binary (same as what
/// `Command::cargo_bin` uses).
fn bin_path() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin("nestweaver")
}

/// Stop the daemon for a given DB path, ignoring errors (best-effort cleanup).
fn stop_daemon(db_path: &Path) {
    let _ = daemon_action_cmd(db_path, "stop").ok();
}

/// RAII guard that stops the daemon on drop — ensures cleanup even on panic.
struct DaemonGuard {
    db_path: std::path::PathBuf,
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

/// Send an MCP JSON-RPC request sequence (initialize + notification + tool call)
/// to `nestweaver mcp` and return the stdout.
fn mcp_tool_call(db_path: &Path, tool_name: &str, arguments: serde_json::Value) -> String {
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05" }
    });
    let init_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let tool_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": tool_name, "arguments": arguments }
    });

    let input = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&init_req).unwrap(),
        serde_json::to_string(&init_notif).unwrap(),
        serde_json::to_string(&tool_req).unwrap(),
    );

    let mut child = StdCommand::new(bin_path())
        .args(["mcp", "--db", &db_path.display().to_string()])
        .env("NESTWEAVER_NO_DAEMON", "1")
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
    // Close stdin so MCP server exits after processing.
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to read mcp output");
    String::from_utf8_lossy(&output.stdout).to_string()
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

/// Create a DB whose indexed repo belongs to a specific instance.
fn create_db_for_instance(repo_dir: &Path, db_path: &Path, instance: &str) {
    no_daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--instance",
            instance,
        ])
        .assert()
        .success();
}

/// Ensure the source repo has a filemeta slice and a persisted PageRank sentinel.
fn seed_deletion_sidecars(db_path: &Path, repo_uid: &str) {
    let filemeta_path = sidecar_path(db_path, ".filemeta.json");
    let mut filemeta = load_filemeta_sidecar(&filemeta_path);
    filemeta.repos.entry(repo_uid.to_string()).or_default();
    save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();
    std::fs::write(
        sidecar_path(db_path, ".pagerank.json"),
        r#"{"rank-sentinel":0.75}"#,
    )
    .unwrap();
}

/// Index a repo through the daemon (no NESTWEAVER_NO_DAEMON).
fn index_via_daemon(repo_dir: &Path, db_path: &Path) {
    daemon_cmd()
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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn daemon_start_stop() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("testdb").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);

    // Start daemon.
    start_daemon(&db_path);

    // Status: should report running.
    daemon_action_cmd(&db_path, "status")
        .assert()
        .success()
        .stdout(contains("running"));

    // Stop daemon.
    daemon_action_cmd(&db_path, "stop").assert().success();
    std::thread::sleep(Duration::from_secs(1));

    // Status: should report not running.
    daemon_action_cmd(&db_path, "status")
        .assert()
        .success()
        .stdout(contains("not running"));
}

#[test]
fn daemon_auto_start() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("autostart").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);

    // Run index WITHOUT NESTWEAVER_NO_DAEMON. Should auto-start the daemon.
    index_via_daemon(&repo_dir, &db_path);

    // Wait for daemon socket to appear.
    std::thread::sleep(Duration::from_secs(5));

    // Verify daemon is running.
    daemon_action_cmd(&db_path, "status")
        .assert()
        .success()
        .stdout(contains("running").and(contains("PID")));
}

#[test]
fn daemon_index_and_query() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("iq").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);

    // Start daemon.
    start_daemon(&db_path);

    // Index through the daemon.
    index_via_daemon(&repo_dir, &db_path);

    // Query via MCP brain_status and check repo count.
    let mcp_output = mcp_tool_call(&db_path, "brain_status", serde_json::json!({}));

    // The MCP output contains line-delimited JSON responses.
    // Find the response with id=2 (our tools/call).
    let tool_response = mcp_output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v.get("id") == Some(&serde_json::json!(2)))
        .expect("should find tools/call response (id=2)");

    // Verify the result contains repo information.
    let result = &tool_response["result"];
    assert!(
        !result.is_null(),
        "brain_status result should not be null; got: {tool_response}"
    );

    // The structuredContent should mention repos.
    let result_str = serde_json::to_string(result).unwrap();
    assert!(
        result_str.contains("repo") || result_str.contains("Repo"),
        "brain_status should mention repos; got: {result_str}"
    );
}

#[test]
fn daemon_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("crash").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);

    // Start daemon.
    start_daemon(&db_path);

    // Read PID from pidfile and kill with SIGKILL.
    let instance_id = nestweaver_daemon::instance_id_from_db_path(&db_path);
    let pidfile = nestweaver_daemon::pidfile_path(&instance_id);
    let pid_str = std::fs::read_to_string(&pidfile).expect("pidfile should exist");
    let pid: i32 = pid_str
        .trim()
        .parse()
        .expect("pidfile should contain a PID");

    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }

    std::thread::sleep(Duration::from_secs(1));

    // Verify the old daemon is dead.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "daemon should be dead after SIGKILL");

    // Run another index command (without NESTWEAVER_NO_DAEMON).
    // Should auto-restart the daemon and succeed.
    index_via_daemon(&repo_dir, &db_path);

    // Wait for auto-start.
    std::thread::sleep(Duration::from_secs(5));

    // Daemon should be running again.
    daemon_action_cmd(&db_path, "status")
        .assert()
        .success()
        .stdout(contains("running").and(contains("PID")));
}

#[test]
fn daemon_concurrent_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("concurrent").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);

    // Start daemon.
    start_daemon(&db_path);

    // Launch two MCP brain_status queries simultaneously.
    let db1 = db_path.clone();
    let db2 = db_path.clone();

    let handle1 =
        std::thread::spawn(move || mcp_tool_call(&db1, "brain_status", serde_json::json!({})));
    let handle2 =
        std::thread::spawn(move || mcp_tool_call(&db2, "brain_status", serde_json::json!({})));

    let result1 = handle1.join().expect("thread 1 should not panic");
    let result2 = handle2.join().expect("thread 2 should not panic");

    // Both should contain a valid response with id=2.
    for (i, result) in [(1, &result1), (2, &result2)] {
        let tool_response = result
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|v| v.get("id") == Some(&serde_json::json!(2)))
            .unwrap_or_else(|| {
                panic!("query {i} should have a tools/call response; got: {result}")
            });

        assert!(
            tool_response.get("result").is_some(),
            "query {i} response should have a result field; got: {tool_response}"
        );
    }
}

#[test]
fn daemon_mcp_brain_add_source() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let db_path = dir.path().join("addsrc").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    write_test_vault(&vault_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);

    // Start daemon.
    start_daemon(&db_path);

    // Use brain_add_source MCP tool to add the vault.
    let add_output = mcp_tool_call(
        &db_path,
        "brain_add_source",
        serde_json::json!({
            "path": vault_dir.display().to_string()
        }),
    );

    // Find the tools/call response.
    let add_response = add_output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v.get("id") == Some(&serde_json::json!(2)))
        .unwrap_or_else(|| panic!("brain_add_source should return a response; got: {add_output}"));

    // Should not be an error.
    assert!(
        add_response.get("result").is_some(),
        "brain_add_source should succeed; got: {add_response}"
    );

    // Verify notes are indexed by querying brain_status.
    let status_output = mcp_tool_call(&db_path, "brain_status", serde_json::json!({}));
    let status_response = status_output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v.get("id") == Some(&serde_json::json!(2)))
        .expect("brain_status should return a response");

    let result_str = serde_json::to_string(&status_response["result"]).unwrap();

    // The vault notes should appear in the status output.
    assert!(
        result_str.contains("note") || result_str.contains("vault") || result_str.contains("Note"),
        "brain_status should show indexed notes after brain_add_source; got: {result_str}"
    );
}

#[test]
fn daemon_materialize_twice_no_broken_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let db_path = dir.path().join("test.lbug");

    write_test_repo(&repo_dir);
    write_test_vault(&vault_dir);

    // Index repo to create the DB (no-daemon path).
    create_db(&repo_dir, &db_path);

    // Write a minimal instance config with a project.
    let config_path = dir.path().join("instance.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "{storage}"

[workspace]
backend = "local"
path = "{workspace}"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

[[repos]]
url = "file://{repo}"
name = "repo"

[[projects]]
name = "test-project"
description = "A test project"
repos = ["repo"]
"#,
            storage = dir.path().join("storage").display(),
            workspace = dir.path().join("workspace").display(),
            repo = repo_dir.display(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("storage")).unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();

    let _guard = DaemonGuard::new(&db_path);

    // First materialize — should succeed and start the daemon.
    daemon_cmd()
        .args([
            "materialize-projects",
            "--config",
            &config_path.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    // Brief pause to let the file mtime settle (filesystem resolution).
    std::thread::sleep(Duration::from_millis(200));

    // Second materialize — previously triggered h2 broken-pipe because
    // the daemon's db_opened_at was stale after the first write.
    daemon_cmd()
        .args([
            "materialize-projects",
            "--config",
            &config_path.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();
}

#[test]
fn daemon_concurrent_client_during_write() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let db_path = dir.path().join("test.lbug");

    write_test_repo(&repo_dir);
    write_test_vault(&vault_dir);
    create_db(&repo_dir, &db_path);

    let config_path = dir.path().join("instance.toml");
    let repo_url = format!("file://{}", repo_dir.display());
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "{storage}"

[workspace]
backend = "local"
path = "{workspace}"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

[[repos]]
url = "{repo_url}"
name = "repo"

[[projects]]
name = "test-project"
description = "A test project"
repos = ["repo"]
"#,
            storage = dir.path().join("storage").display(),
            workspace = dir.path().join("workspace").display(),
            repo_url = repo_url,
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("storage")).unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();

    let _guard = DaemonGuard::new(&db_path);

    // Start materialize in background thread.
    let db_str = db_path.display().to_string();
    let config_str = config_path.display().to_string();
    let materialize_handle = std::thread::spawn(move || {
        daemon_cmd()
            .args([
                "materialize-projects",
                "--config",
                &config_str,
                "--db",
                &db_str,
            ])
            .assert()
            .success();
    });

    // Brief pause to let materialize start.
    std::thread::sleep(Duration::from_millis(500));

    // Concurrent read — should NOT trigger a daemon restart.
    daemon_cmd()
        .args(["list-repos", "--db", &db_path.display().to_string()])
        .assert()
        .success();

    materialize_handle
        .join()
        .expect("materialize thread panicked");
}

#[test]
fn daemon_shutdown_rpc_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");

    write_test_repo(&repo_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);

    // Start the daemon.
    start_daemon(&db_path);

    // Verify daemon is running.
    daemon_action_cmd(&db_path, "status").assert().success();

    // Stop via the CLI (which uses the Shutdown RPC).
    daemon_action_cmd(&db_path, "stop").assert().success();

    std::thread::sleep(Duration::from_secs(2));

    // Verify daemon is no longer running.
    daemon_action_cmd(&db_path, "status")
        .assert()
        .success()
        .stdout(contains("not running"));
}

/// nw-019 root cause: the daemon adopted its db-path HASH as the instance_id
/// and stamped it on every repo/symbol it indexed, ignoring the logical
/// `instance_id` from the loaded `instance.toml`. Projects used the config
/// name while repos used the hash, so `--instance <config-name>` never matched
/// indexed code.
///
/// This test starts a daemon WITH `--config` (logical `instance_id =
/// "test-instance"`), indexes a repo through it, then lists the repos and
/// asserts every row carries the config's logical name — NOT an 8-hex hash.
#[test]
fn daemon_indexed_repo_carries_config_instance_id() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");

    write_test_repo(&repo_dir);

    // Minimal instance config with a logical instance_id. Starting the daemon
    // with `--config` is what gives it a `data_instance_id` distinct from the
    // db-path hash.
    let config_path = dir.path().join("instance.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "{storage}"

[workspace]
backend = "local"
path = "{workspace}"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

[[repos]]
url = "file://{repo}"
name = "repo"
"#,
            storage = dir.path().join("storage").display(),
            workspace = dir.path().join("workspace").display(),
            repo = repo_dir.display(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("storage")).unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();

    let _guard = DaemonGuard::new(&db_path);

    // Index through the daemon WITH --config. Autostart spawns the daemon with
    // `--config`, so the Index RPC stamps the config's logical instance_id.
    daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--config",
            &config_path.display().to_string(),
        ])
        .assert()
        .success();

    // List repos as JSON and inspect the stamped instance_id on every row.
    let output = daemon_cmd()
        .args([
            "list-repos",
            "--db",
            &db_path.display().to_string(),
            "--json",
        ])
        .output()
        .expect("list-repos failed to run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let repos: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("list-repos --json did not emit valid JSON");
    let arr = repos.as_array().expect("expected a JSON array of repos");
    assert!(
        !arr.is_empty(),
        "expected at least one indexed repo, got none: {stdout}"
    );

    for repo in arr {
        let iid = repo["instance_id"]
            .as_str()
            .expect("repo row missing instance_id");
        // The bug: repos stamped with the 8-hex db-path hash.
        let looks_like_hash = iid.len() == 8 && iid.chars().all(|c| c.is_ascii_hexdigit());
        assert!(
            !looks_like_hash,
            "repo instance_id looks like a db-path hash ({iid}); \
             expected the config's logical name 'test-instance'"
        );
        assert_eq!(
            iid, "test-instance",
            "indexed repo should carry the config's logical instance_id"
        );
    }
}

/// nw-019 T4: an explicit `nestweaver index --instance <name>` must override the
/// daemon's default instance (the config's logical `instance_id`) when threaded
/// through the RPC. Before T4 the `IndexRepoRequest` had no instance field, so
/// the flag was a silent no-op through the daemon and rows kept the config name.
///
/// This starts a daemon WITH `--config` (logical `instance_id = "test-instance"`),
/// indexes a repo through it WITH `--instance override-name`, then asserts every
/// repo row carries `override-name` — the explicit flag beats the config default.
#[test]
fn daemon_index_instance_flag_overrides_config() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");

    write_test_repo(&repo_dir);

    let config_path = dir.path().join("instance.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "{storage}"

[workspace]
backend = "local"
path = "{workspace}"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

[[repos]]
url = "file://{repo}"
name = "repo"
"#,
            storage = dir.path().join("storage").display(),
            workspace = dir.path().join("workspace").display(),
            repo = repo_dir.display(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("storage")).unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();

    let _guard = DaemonGuard::new(&db_path);

    // Index through the daemon WITH --config (default instance "test-instance")
    // but ALSO pass an explicit --instance override-name. The flag must win.
    daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--config",
            &config_path.display().to_string(),
            "--instance",
            "override-name",
        ])
        .assert()
        .success();

    // List repos as JSON and inspect the stamped instance_id on every row.
    let output = daemon_cmd()
        .args([
            "list-repos",
            "--db",
            &db_path.display().to_string(),
            "--json",
        ])
        .output()
        .expect("list-repos failed to run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let repos: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("list-repos --json did not emit valid JSON");
    let arr = repos.as_array().expect("expected a JSON array of repos");
    assert!(
        !arr.is_empty(),
        "expected at least one indexed repo, got none: {stdout}"
    );

    for repo in arr {
        let iid = repo["instance_id"]
            .as_str()
            .expect("repo row missing instance_id");
        assert_eq!(
            iid, "override-name",
            "explicit --instance should override the config's default instance_id \
             (got '{iid}' — flag was ignored through the daemon RPC)"
        );
    }
}

/// nw-023: prove the DEFAULT daemon index path now reaches the gated auto-setup
/// helper. Before this change, `maybe_run_auto_setup` was only wired into the
/// `NESTWEAVER_NO_DAEMON=1` direct path, so the daemon branch (what real
/// interactive users hit) never ran setup OR printed the hint. Here we index
/// through the daemon from a controlled cwd and assert the gate behaved the same
/// as the direct-path regression test: piped stderr is not a TTY, so setup is
/// SKIPPED (no config-file pollution, no marker written) and the hint is printed.
#[test]
fn daemon_index_reaches_auto_setup_gate() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let cwd = dir.path().join("cwd");
    let db_path = dir.path().join("setup").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    // Deterministic detection surface without relying on host PATH (mirrors the
    // direct-path regression test in cli_test.rs).
    std::fs::create_dir_all(cwd.join(".cursor")).unwrap();
    write_test_repo(&repo_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);

    // Start daemon.
    start_daemon(&db_path);

    // Index through the daemon (NO NESTWEAVER_NO_DAEMON) from the controlled cwd.
    // The client process — which runs the gate — evaluates against this cwd.
    let output = daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .current_dir(&cwd)
        .output()
        .expect("daemon index failed to run");
    assert!(
        output.status.success(),
        "daemon index should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Skipped (non-TTY): the gate must not write setup files into the cwd...
    for f in [
        ".mcp.json",
        "AGENTS.md",
        ".claude",
        ".codex",
        ".github",
        ".cursor/mcp.json",
        "devin.json",
    ] {
        assert!(
            !cwd.join(f).exists(),
            "daemon index must not write {f} into an unrelated cwd"
        );
    }
    // ...nor into the repo root...
    assert!(
        !repo_dir.join(".mcp.json").exists(),
        "non-TTY daemon index must not write into the repo either"
    );
    // ...nor write the marker (skip ≠ done, so a future interactive index still runs setup).
    let marker = db_path.with_file_name(format!(
        "{}.setup_done",
        db_path.file_name().unwrap().to_str().unwrap()
    ));
    assert!(
        !marker.exists(),
        "marker must only be written when setup actually ran, not on a skip"
    );
    // The hint proves the daemon branch reached the gate (previously it never did).
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nestweaver setup"),
        "daemon index must reach the gate and print the setup hint, got: {stderr}"
    );
}

/// nw-052 (P2a): the daemon's `index_repo` RPC must reject a colon (or
/// whitespace) `instance_id` at the RPC BOUNDARY — before it's stamped into a
/// `repo:<instance>:<hash>` uid. The CLI guards its own `--instance` flag and
/// would short-circuit a `nestweaver index --instance a:b` before the daemon
/// ever sees it, so a CLI-driven test can't isolate the server-side guard.
/// Here we drive the low-level gRPC client directly over the daemon's UDS
/// (admin), bypassing the CLI validation entirely — the rejection therefore
/// proves the RPC-boundary guard protects any client (e.g. a future MCP/other
/// caller), not just the CLI. Pre-fix this call SUCCEEDS and stamps `a:b`.
#[test]
fn daemon_index_rpc_rejects_colon_in_instance() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    write_test_repo(&repo_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let status = rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .expect("connect to daemon over UDS");
        let req = nestweaver_proto::IndexRepoRequest {
            repo_path: repo_dir.display().to_string(),
            instance_id: "a:b".to_string(),
            ..Default::default()
        };
        // The guard rejects before the stream opens, so the initial call errors.
        // Defensively drain any stream that does open, looking for an in-band
        // error status, so this test fails loudly if the guard ever regresses.
        match client.inner_mut().index_repo(req).await {
            Ok(resp) => {
                let mut stream = resp.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(_)) => continue,
                        Ok(None) => break None,
                        Err(s) => break Some(s),
                    }
                }
            }
            Err(s) => Some(s),
        }
    });

    let status = status.expect("daemon must REJECT a colon instance_id, not accept it");
    assert_eq!(
        status.code(),
        tonic::Code::InvalidArgument,
        "expected invalid_argument, got: {status:?}"
    );
    assert!(
        status.message().contains("colon") || status.message().contains(':'),
        "error should explain the colon problem, got: {}",
        status.message()
    );
}

#[test]
fn daemon_merge_removes_source_sidecars_and_invalidates_rank() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("merge").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db_for_instance(&repo_dir, &db_path, "old");

    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    let source_uid = store.list_repos(Some("old")).unwrap()[0].uid.clone();
    let generation_before = store.graph_generation();
    drop(store);
    seed_deletion_sidecars(&db_path, &source_uid);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let response = rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .expect("connect to daemon over UDS");
        client.merge_instance("old", "new").await.unwrap()
    });
    assert_eq!(response.repos_reparented, 1);

    stop_daemon(&db_path);
    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert!(
        !load_filemeta_sidecar(&sidecar_path(&db_path, ".filemeta.json"))
            .repos
            .contains_key(&source_uid)
    );
    assert!(store.graph_generation() > generation_before);
    assert!(!sidecar_path(&db_path, ".pagerank.json").exists());
}

#[test]
fn daemon_merge_migrates_authored_extensions_for_every_reminted_code_uid() {
    use nestweaver_schema::uid::{file_uid, project_uid, repo_uid, symbol_uid};

    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("extension-merge").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db_for_instance(&repo_dir, &db_path, "old");

    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    let source_repo = store.list_repos(Some("old")).unwrap().remove(0);
    let source_file = store
        .list_files_by_repo(&source_repo.uid)
        .unwrap()
        .remove(0);
    let source_symbol = store
        .lookup_symbols_by_repo(&source_repo.uid)
        .unwrap()
        .remove(0);
    let source_project_uid = project_uid("old", "Extension migration");
    store
        .insert_project(&nestweaver_schema::Project {
            uid: source_project_uid.clone(),
            name: "Extension migration".to_string(),
            summary: None,
            instance_id: "old".to_string(),
        })
        .unwrap();

    let target_repo_uid = repo_uid("new", &source_repo.url);
    store
        .insert_repo(&nestweaver_schema::Repo {
            uid: target_repo_uid.clone(),
            instance_id: "new".to_string(),
            indexed_sha: "target-sha".to_string(),
            name: Some("target collision".to_string()),
            ..source_repo.clone()
        })
        .unwrap();
    let target_file_uid = file_uid(&target_repo_uid, &source_file.1);
    let target_symbol_uid = symbol_uid(
        &target_repo_uid,
        &source_symbol.file_path,
        &source_symbol.name,
        source_symbol.start_line,
    );
    let target_project_uid = project_uid("new", "Extension migration");
    let unrelated_uid = "note:vlt:unrelated:aaaaaaaaaaaa";

    let mut extensions = nestweaver_engine::ExtensionStore::new();
    nestweaver_engine::set_property(
        &mut extensions,
        &source_repo.uid,
        "owner",
        serde_json::json!("source-owner"),
    );
    nestweaver_engine::set_property(
        &mut extensions,
        &source_repo.uid,
        "source-only",
        serde_json::json!({"nested": [1, {"preserved": true}]}),
    );
    nestweaver_engine::set_property(
        &mut extensions,
        &source_file.0,
        "kind",
        serde_json::json!("file"),
    );
    nestweaver_engine::set_property(
        &mut extensions,
        &source_symbol.uid,
        "kind",
        serde_json::json!("symbol"),
    );
    nestweaver_engine::set_property(
        &mut extensions,
        &source_project_uid,
        "kind",
        serde_json::json!("project"),
    );
    nestweaver_engine::set_property(
        &mut extensions,
        &target_repo_uid,
        "owner",
        serde_json::json!("destination-owner"),
    );
    nestweaver_engine::set_property(
        &mut extensions,
        unrelated_uid,
        "keep",
        serde_json::json!(true),
    );
    nestweaver_engine::save_extensions(&db_path, &extensions).unwrap();
    let generation_before = store.graph_generation();
    drop(store);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let response = rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .unwrap();
        client.merge_instance("old", "new").await.unwrap()
    });
    assert_eq!(response.repos_reparented, 1);
    assert_eq!(response.projects_reparented, 1);
    stop_daemon(&db_path);

    let reopened = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert!(reopened.graph_generation() > generation_before);
    drop(reopened);
    let migrated = nestweaver_engine::load_extensions(&db_path);
    for source_uid in [
        source_repo.uid.as_str(),
        source_file.0.as_str(),
        source_symbol.uid.as_str(),
        source_project_uid.as_str(),
    ] {
        assert!(
            !migrated.contains_key(source_uid),
            "old key survived: {source_uid}"
        );
    }
    assert_eq!(
        nestweaver_engine::get_property(&migrated, &target_repo_uid, "owner"),
        Some(&serde_json::json!("destination-owner"))
    );
    assert_eq!(
        nestweaver_engine::get_property(&migrated, &target_repo_uid, "source-only"),
        Some(&serde_json::json!({"nested": [1, {"preserved": true}]}))
    );
    assert_eq!(
        nestweaver_engine::get_property(&migrated, unrelated_uid, "keep"),
        Some(&serde_json::json!(true))
    );
    assert!(!sidecar_path(&db_path, ".extensions.migration.json").exists());

    // Reopen through a fresh daemon and exercise the actual query_extensions
    // RPC, proving the migrated target UIDs are visible across process restart.
    start_daemon(&db_path);
    rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .unwrap();
        for (uid, key, expected) in [
            (
                &target_repo_uid,
                "owner",
                serde_json::json!("destination-owner"),
            ),
            (&target_file_uid, "kind", serde_json::json!("file")),
            (&target_symbol_uid, "kind", serde_json::json!("symbol")),
            (&target_project_uid, "kind", serde_json::json!("project")),
        ] {
            let response = client
                .inner_mut()
                .query_extensions(nestweaver_proto::JsonRequest {
                    args_json: serde_json::json!({"uid": uid}).to_string(),
                })
                .await
                .unwrap()
                .into_inner();
            let result: serde_json::Value = serde_json::from_str(&response.result_json).unwrap();
            assert_eq!(result["uid"], uid.as_str());
            assert_eq!(result["properties"][key], expected);
        }
    });
    stop_daemon(&db_path);
}

#[test]
fn restored_pending_extension_migration_recovers_automatically_on_daemon_start() {
    use nestweaver_schema::uid::project_uid;

    let dir = tempfile::tempdir().unwrap();
    let source_data = dir.path().join("source-data");
    let source_db = source_data.join("test.lbug");
    std::fs::create_dir_all(&source_data).unwrap();
    let store = nestweaver_store::GraphStore::open_or_create(&source_db).unwrap();
    let source_uid = project_uid("old", "Restored migration");
    let destination_uid = project_uid("new", "Restored migration");
    store
        .insert_project(&nestweaver_schema::Project {
            uid: source_uid.clone(),
            name: "Restored migration".to_string(),
            summary: None,
            instance_id: "old".to_string(),
        })
        .unwrap();
    let mut extensions = nestweaver_engine::ExtensionStore::new();
    nestweaver_engine::set_property(
        &mut extensions,
        &source_uid,
        "nested",
        serde_json::json!({"backup": [true, {"path-independent": true}]}),
    );
    nestweaver_engine::save_extensions(&source_db, &extensions).unwrap();
    let mappings = store.plan_instance_uid_remaps("old", "new").unwrap();
    nestweaver_engine::prepare_instance_extension_migration(&source_db, "old", "new", &mappings)
        .unwrap();
    drop(store);

    let snapshot = dir.path().join("pending.nwsnap.zst");
    nestweaver_engine::backup_save(&nestweaver_engine::BackupConfig {
        db_path: source_db,
        output_path: snapshot.clone(),
        include_clones: false,
        instance_id: "backup-source".to_string(),
        workspace_path: None,
    })
    .unwrap();
    let restored_data = dir.path().join("restored-data");
    nestweaver_engine::backup_restore(&nestweaver_engine::RestoreConfig {
        snapshot_path: snapshot,
        data_dir: restored_data.clone(),
    })
    .unwrap();
    let restored_db = restored_data.join("test.lbug");
    assert!(sidecar_path(&restored_db, ".extensions.migration.json").exists());

    let _guard = DaemonGuard::new(&restored_db);
    start_daemon(&restored_db);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&restored_db)
            .await
            .unwrap();
        let response = client
            .inner_mut()
            .query_extensions(nestweaver_proto::JsonRequest {
                args_json: serde_json::json!({"uid": destination_uid}).to_string(),
            })
            .await
            .unwrap()
            .into_inner();
        let result: serde_json::Value = serde_json::from_str(&response.result_json).unwrap();
        assert_eq!(
            result["properties"]["nested"],
            serde_json::json!({"backup": [true, {"path-independent": true}]})
        );
    });
    stop_daemon(&restored_db);

    let reopened = nestweaver_store::GraphStore::open(&restored_db).unwrap();
    assert!(!reopened.project_exists(&source_uid).unwrap());
    assert!(reopened.project_exists(&destination_uid).unwrap());
    drop(reopened);
    let migrated = nestweaver_engine::load_extensions(&restored_db);
    assert!(!migrated.contains_key(&source_uid));
    assert!(migrated.contains_key(&destination_uid));
    assert!(!sidecar_path(&restored_db, ".extensions.migration.json").exists());
}

#[test]
fn daemon_start_recovers_graph_applied_code_finalizers_before_real_readd() {
    use nestweaver_engine::resolution_cache::ResolutionDeps;

    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("code-crash-recovery").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db_for_instance(&repo_dir, &db_path, "old");

    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    let source_repo = store.list_repos(Some("old")).unwrap().remove(0);
    assert!(
        !store
            .lookup_symbols_by_repo(&source_repo.uid)
            .unwrap()
            .is_empty()
    );
    let filemeta_path = sidecar_path(&db_path, ".filemeta.json");
    let mut filemeta = load_filemeta_sidecar(&filemeta_path);
    filemeta.repos.entry(source_repo.uid.clone()).or_default();
    save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();
    let deps_path = sidecar_path(&db_path, ".resolution_deps.bin");
    let mut deps = ResolutionDeps::default();
    deps.set_deps_for_repo(
        &source_repo.uid,
        "main.js",
        std::collections::HashSet::from(["dep.js".to_string()]),
    );
    deps.save(&deps_path).unwrap();

    let mappings = store.plan_instance_uid_remaps("old", "new").unwrap();
    let prepared = nestweaver_engine::prepare_instance_extension_migration_with_finalizers(
        &db_path,
        "old",
        "new",
        &mappings,
        &nestweaver_engine::InstanceMigrationFinalizerPlan {
            repo_uids: vec![source_repo.uid.clone()],
            search_reconciliation_required: false,
        },
    )
    .unwrap();
    store.merge_instance_ids("old", "new").unwrap();
    nestweaver_engine::mark_instance_extension_migration_graph_applied(&db_path, &prepared)
        .unwrap();
    assert!(
        store
            .lookup_symbols_by_repo(&source_repo.uid)
            .unwrap()
            .is_empty()
    );
    drop(store);
    assert!(
        load_filemeta_sidecar(&filemeta_path)
            .repos
            .contains_key(&source_repo.uid)
    );
    assert!(!ResolutionDeps::load(&deps_path).is_empty_for_repo(&source_repo.uid));

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);

    assert!(
        !load_filemeta_sidecar(&filemeta_path)
            .repos
            .contains_key(&source_repo.uid)
    );
    assert!(ResolutionDeps::load(&deps_path).is_empty_for_repo(&source_repo.uid));
    assert!(!sidecar_path(&db_path, ".extensions.migration.json").exists());

    // Re-add the original source scope without --force. A stale `.filemeta`
    // slice would classify main.js as unchanged and restore no symbols.
    daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--instance",
            "old",
        ])
        .assert()
        .success();
    stop_daemon(&db_path);

    let reopened = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert!(reopened.lookup_repo(&source_repo.uid).unwrap().is_some());
    assert!(
        !reopened
            .lookup_symbols_by_repo(&source_repo.uid)
            .unwrap()
            .is_empty(),
        "normal re-add treated the source repo file as unchanged"
    );
}

#[test]
fn daemon_start_recovers_graph_applied_vault_tantivy_scope() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let db_path = dir.path().join("vault-crash-recovery").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    write_test_vault(&vault_dir);
    create_db(&repo_dir, &db_path);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);
    daemon_cmd()
        .args(["brain", "add"])
        .arg(&vault_dir)
        .args(["--instance", "old", "--db"])
        .arg(&db_path)
        .assert()
        .success();
    stop_daemon(&db_path);

    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    let source_vault = store.list_vaults(Some("old")).unwrap().remove(0);
    let source_note = store.list_notes(Some(&source_vault.uid)).unwrap().remove(0);
    let prepared = nestweaver_engine::prepare_instance_extension_migration_with_finalizers(
        &db_path,
        "old",
        "new",
        &[],
        &nestweaver_engine::InstanceMigrationFinalizerPlan {
            repo_uids: Vec::new(),
            search_reconciliation_required: true,
        },
    )
    .unwrap();
    store.merge_instance_ids("old", "new").unwrap();
    let destination_vault = store.list_vaults(Some("new")).unwrap().remove(0);
    nestweaver_engine::mark_instance_extension_migration_graph_applied(&db_path, &prepared)
        .unwrap();
    drop(store);

    let tantivy_path = nestweaver_mcp::tantivy_sidecar_path(&db_path);
    let stale = nestweaver_store::TantivyIndex::open_reader_only(&tantivy_path).unwrap();
    let stale_hits = stale.search("Hello", 10).unwrap();
    assert!(
        stale_hits
            .iter()
            .any(|hit| { hit.uid == source_note.uid && hit.vault_uid == source_vault.uid })
    );
    assert!(
        !stale_hits
            .iter()
            .any(|hit| { hit.uid == source_note.uid && hit.vault_uid == destination_vault.uid })
    );
    drop(stale);

    start_daemon(&db_path);
    stop_daemon(&db_path);

    let recovered = nestweaver_store::TantivyIndex::open_reader_only(&tantivy_path).unwrap();
    let recovered_hits = recovered.search("Hello", 10).unwrap();
    assert!(
        !recovered_hits
            .iter()
            .any(|hit| { hit.uid == source_note.uid && hit.vault_uid == source_vault.uid })
    );
    assert!(
        recovered_hits
            .iter()
            .any(|hit| { hit.uid == source_note.uid && hit.vault_uid == destination_vault.uid })
    );
    assert!(!sidecar_path(&db_path, ".extensions.migration.json").exists());
}

#[test]
fn daemon_project_casefold_collision_uses_exact_graph_winner_for_extension_union() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("project-collision").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    let winner_uid = "proj:new:000000000001";
    let target_loser_uid = "proj:new:ffffffffffff";
    let source_first_uid = "proj:old:111111111111";
    let source_second_uid = "proj:old:222222222222";
    // Reverse the natural winner/precedence order to prove DB insertion order
    // does not select the graph survivor or extension conflict winner.
    for project in [
        nestweaver_schema::Project {
            uid: source_second_uid.to_string(),
            name: "RoadMap".to_string(),
            summary: Some("source second".to_string()),
            instance_id: "old".to_string(),
        },
        nestweaver_schema::Project {
            uid: source_first_uid.to_string(),
            name: "roadmap".to_string(),
            summary: Some("source first".to_string()),
            instance_id: "old".to_string(),
        },
        nestweaver_schema::Project {
            uid: target_loser_uid.to_string(),
            name: "ROADMAP".to_string(),
            summary: Some("target loser".to_string()),
            instance_id: "new".to_string(),
        },
        nestweaver_schema::Project {
            uid: winner_uid.to_string(),
            name: "Roadmap".to_string(),
            summary: Some("target winner".to_string()),
            instance_id: "new".to_string(),
        },
    ] {
        store.insert_project(&project).unwrap();
    }
    let mut extensions = nestweaver_engine::ExtensionStore::new();
    for (uid, key, value) in [
        (winner_uid, "priority", serde_json::json!("winner")),
        (
            target_loser_uid,
            "priority",
            serde_json::json!("target-loser"),
        ),
        (
            target_loser_uid,
            "target-only",
            serde_json::json!({"legacy": true}),
        ),
        (
            source_first_uid,
            "priority",
            serde_json::json!("source-first"),
        ),
        (
            source_first_uid,
            "source-only",
            serde_json::json!({"source": 1}),
        ),
        (
            source_second_uid,
            "source-only",
            serde_json::json!({"source": 2}),
        ),
    ] {
        nestweaver_engine::set_property(&mut extensions, uid, key, value);
    }
    nestweaver_engine::save_extensions(&db_path, &extensions).unwrap();
    drop(store);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let response = rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .unwrap();
        client.merge_instance("old", "new").await.unwrap()
    });
    assert_eq!(response.projects_reparented, 2);
    stop_daemon(&db_path);

    let reopened = nestweaver_store::GraphStore::open(&db_path).unwrap();
    let projects = reopened.list_projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].uid, winner_uid);
    assert_eq!(projects[0].name, "Roadmap");
    assert_eq!(projects[0].summary.as_deref(), Some("target winner"));
    drop(reopened);
    let migrated = nestweaver_engine::load_extensions(&db_path);
    for loser in [target_loser_uid, source_first_uid, source_second_uid] {
        assert!(!migrated.contains_key(loser), "loser survived: {loser}");
    }
    assert_eq!(
        nestweaver_engine::get_property(&migrated, winner_uid, "priority"),
        Some(&serde_json::json!("winner"))
    );
    assert_eq!(
        nestweaver_engine::get_property(&migrated, winner_uid, "target-only"),
        Some(&serde_json::json!({"legacy": true}))
    );
    assert_eq!(
        nestweaver_engine::get_property(&migrated, winner_uid, "source-only"),
        Some(&serde_json::json!({"source": 1}))
    );

    start_daemon(&db_path);
    rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .unwrap();
        let response = client
            .inner_mut()
            .query_extensions(nestweaver_proto::JsonRequest {
                args_json: serde_json::json!({"uid": winner_uid}).to_string(),
            })
            .await
            .unwrap()
            .into_inner();
        let result: serde_json::Value = serde_json::from_str(&response.result_json).unwrap();
        assert_eq!(result["properties"]["priority"], "winner");
        assert_eq!(result["properties"]["target-only"]["legacy"], true);
        assert_eq!(result["properties"]["source-only"]["source"], 1);
    });
    stop_daemon(&db_path);
}

#[test]
fn daemon_merge_rejects_self_merge_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("self-merge").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    let vault = nestweaver_schema::Vault {
        uid: "vlt:self:one".to_string(),
        name: "authored".to_string(),
        root_path: "/authored".to_string(),
        instance_id: "same".to_string(),
    };
    store.insert_vault(&vault).unwrap();
    store
        .insert_note(&nestweaver_schema::Note {
            uid: "note:self:one".to_string(),
            vault_uid: vault.uid.clone(),
            file_path: "authored.md".to_string(),
            title: "Authored".to_string(),
            note_kind: nestweaver_schema::NoteKind::General,
            word_count: 42,
            content_hash: "authored-hash".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        })
        .unwrap();
    store
        .insert_vault_note_edge(&vault.uid, "note:self:one")
        .unwrap();
    let generation_before = store.graph_generation();
    drop(store);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let error = rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .expect("connect to daemon over UDS");
        client.merge_instance("same", "same").await.unwrap_err()
    });
    let status = error
        .downcast_ref::<tonic::Status>()
        .expect("merge RPC error should preserve tonic status");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    stop_daemon(&db_path);
    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert_eq!(store.graph_generation(), generation_before);
    assert_eq!(store.list_vaults(Some("same")).unwrap().len(), 1);
    assert_eq!(store.list_notes(Some(&vault.uid)).unwrap().len(), 1);
}

#[test]
fn daemon_purge_removes_repo_sidecars_and_invalidates_rank() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("purge").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db_for_instance(&repo_dir, &db_path, "old");

    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    let source_uid = store.list_repos(Some("old")).unwrap()[0].uid.clone();
    let generation_before = store.graph_generation();
    drop(store);
    seed_deletion_sidecars(&db_path, &source_uid);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let saw_done = rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .expect("connect to daemon over UDS");
        let mut stream = client.purge_instance("old").await.unwrap();
        let mut saw_done = false;
        while let Some(progress) = stream.message().await.unwrap() {
            saw_done |= progress.phase == nestweaver_proto::Phase::Done as i32;
        }
        saw_done
    });
    assert!(saw_done);

    stop_daemon(&db_path);
    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert!(
        !load_filemeta_sidecar(&sidecar_path(&db_path, ".filemeta.json"))
            .repos
            .contains_key(&source_uid)
    );
    assert!(store.graph_generation() > generation_before);
    assert!(!sidecar_path(&db_path, ".pagerank.json").exists());
}

#[test]
fn daemon_purge_orphan_only_code_finalizes_sidecars_and_rank() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("orphan-purge").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    write_test_repo(&repo_dir);
    create_db_for_instance(&repo_dir, &db_path, "old");

    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    let source_uid = store.list_repos(Some("old")).unwrap()[0].uid.clone();
    // Simulate a partial prior mutation: only the registry row disappeared,
    // leaving code children and their repo_uid properties behind.
    store.delete_repo_node(&source_uid).unwrap();
    let generation_before = store.graph_generation();
    drop(store);
    seed_deletion_sidecars(&db_path, &source_uid);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let saw_done = rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .unwrap();
        let mut stream = client.purge_instance("old").await.unwrap();
        let mut saw_done = false;
        while let Some(progress) = stream.message().await.unwrap() {
            saw_done |= progress.phase == nestweaver_proto::Phase::Done as i32;
        }
        saw_done
    });
    assert!(saw_done);

    stop_daemon(&db_path);
    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert!(store.graph_generation() > generation_before);
    assert!(
        !load_filemeta_sidecar(&sidecar_path(&db_path, ".filemeta.json"))
            .repos
            .contains_key(&source_uid)
    );
    assert!(!sidecar_path(&db_path, ".pagerank.json").exists());
}

#[test]
fn daemon_non_code_merge_and_purge_bump_generation_and_invalidate_rank() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("non-code-instance").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    store
        .insert_vault(&nestweaver_schema::Vault {
            uid: "vlt:old:docs".to_string(),
            name: "docs".to_string(),
            root_path: "/nonexistent/docs".to_string(),
            instance_id: "old".to_string(),
        })
        .unwrap();
    store
        .insert_project(&nestweaver_schema::Project {
            uid: "proj:old:work".to_string(),
            name: "work".to_string(),
            summary: None,
            instance_id: "old".to_string(),
        })
        .unwrap();
    let before_merge = store.graph_generation();
    drop(store);
    std::fs::write(
        sidecar_path(&db_path, ".pagerank.json"),
        r#"{"rank-sentinel":0.75}"#,
    )
    .unwrap();

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .unwrap();
        let response = client.merge_instance("old", "new").await.unwrap();
        assert_eq!(response.vaults_reparented, 1);
        assert_eq!(response.projects_reparented, 1);
    });
    stop_daemon(&db_path);
    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert!(store.graph_generation() > before_merge);
    let before_purge = store.graph_generation();
    drop(store);
    assert!(!sidecar_path(&db_path, ".pagerank.json").exists());

    std::fs::write(
        sidecar_path(&db_path, ".pagerank.json"),
        r#"{"rank-sentinel":0.75}"#,
    )
    .unwrap();

    start_daemon(&db_path);
    rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .unwrap();
        let mut stream = client.purge_instance("new").await.unwrap();
        while stream.message().await.unwrap().is_some() {}
    });
    stop_daemon(&db_path);
    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert!(store.graph_generation() > before_purge);
    assert!(!sidecar_path(&db_path, ".pagerank.json").exists());
}

#[test]
fn daemon_noop_merge_purge_and_prune_do_not_bump_generation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("noop-instance").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    let generation_before = store.graph_generation();
    drop(store);

    let _guard = DaemonGuard::new(&db_path);
    start_daemon(&db_path);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut client = nestweaver_client::DaemonClient::connect_existing(&db_path)
            .await
            .unwrap();
        client.merge_instance("missing", "new").await.unwrap();
        let mut stream = client.purge_instance("missing").await.unwrap();
        while stream.message().await.unwrap().is_some() {}
        let pruned = client.prune_stale().await.unwrap();
        assert!(pruned.removed_repos.is_empty());
        assert!(pruned.removed_vaults.is_empty());
    });
    stop_daemon(&db_path);
    let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert_eq!(store.graph_generation(), generation_before);
}

/// Index a repo through the daemon under an explicit instance id so the repo
/// uid is identical across the initial index and any re-index (the tiered
/// change-detection sidecar is keyed by repo uid).
fn index_via_daemon_instance(repo_dir: &Path, db_path: &Path, instance: &str) {
    daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--instance",
            instance,
        ])
        .assert()
        .success();
}

/// nw-048: `remove-repo` must drop the removed repo's change-detection sidecar
/// slices, so a later re-index of the SAME path re-indexes its files instead of
/// silently skipping every one as `Unchanged`. Pre-fix, the stale `.filemeta`
/// slice survived removal → the re-index found 0 files → the symbol was never
/// restored → search returned nothing. Driven end-to-end through the real
/// daemon (the sole DB writer in normal operation). A fixed `--instance` keeps
/// the repo uid stable across index/remove/re-index so the stale slice is
/// actually consulted (an instance mismatch would mint a new uid and mask it).
#[test]
fn daemon_remove_repo_then_reindex_restores_symbols() {
    const INSTANCE: &str = "nw048";
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("rr").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // Repo with a distinctive symbol name we can search for.
    std::fs::create_dir_all(&repo_dir).unwrap();
    StdCommand::new("git")
        .args(["init"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    std::fs::write(repo_dir.join("m.js"), "function cleanfn() { return 1; }").unwrap();
    StdCommand::new("git")
        .args(["add", "."])
        .current_dir(&repo_dir)
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
        .current_dir(&repo_dir)
        .output()
        .unwrap();

    let _guard = DaemonGuard::new(&db_path);

    // Initial index through the daemon (auto-starts it) creates the DB + the
    // `.filemeta` sidecar slice under a fixed instance/uid.
    index_via_daemon_instance(&repo_dir, &db_path, INSTANCE);
    std::thread::sleep(Duration::from_secs(3));

    // Sanity: the symbol is searchable before removal. Route the search through
    // the daemon (its live store) — a NESTWEAVER_NO_DAEMON read-only open could
    // observe a stale snapshot while the daemon holds the write lock.
    daemon_cmd()
        .args(["search", "cleanfn", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("cleanfn"));

    // Remove the repo through the daemon (normal operation). This deletes the
    // repo's graph nodes/symbols; the fix additionally drops its `.filemeta`
    // and `.resolution_deps` sidecar slices.
    daemon_cmd()
        .args([
            "remove-repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("symbol(s) deleted"));

    // Re-index the SAME path through the daemon under the SAME instance/uid.
    // We assert on the daemon's OWN report of how many files it re-indexed,
    // which is deterministic and needs no read-back (a post-removal read of the
    // store can return stale rows from the daemon's in-memory query cache, and a
    // NESTWEAVER_NO_DAEMON read taken before the forked daemon fully releases the
    // lock observes a stale pre-removal snapshot — both would mask the bug).
    //
    // Pre-fix: the orphaned `.filemeta` slice makes m.js classify as `Unchanged`
    // → the daemon re-indexes 0 files → the deleted symbol is never restored
    // ("Indexed 0 files"). Post-fix: the slice was dropped on removal → the
    // daemon re-indexes the file ("Indexed 1 files") and restores the symbol.
    let reindex = daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--instance",
            INSTANCE,
        ])
        .assert()
        .success();
    let reindex_out = reindex.get_output();
    let reindex_log = format!(
        "{}{}",
        String::from_utf8_lossy(&reindex_out.stdout),
        String::from_utf8_lossy(&reindex_out.stderr),
    );
    assert!(
        reindex_log.contains("Indexed 1 files"),
        "re-index after remove-repo must re-index the file (nw-048); \
         pre-fix it reports 'Indexed 0 files'. Got:\n{reindex_log}"
    );
}

/// nw-054: a query routed through the daemon AFTER a `remove-repo` mutation must
/// reflect the deletion — it must NOT return the removed symbol out of a stale
/// in-memory cache. This is the read-back the nw-048 test deliberately AVOIDED
/// (it asserted on re-index file count instead, because a post-removal read gave
/// a false GREEN by returning the deleted symbol).
///
/// Root cause: `remove_repo` deleted the graph rows but never bumped
/// `graph_generation`. The daemon's `symbol_name_cache` (backing `search`) is
/// keyed on that generation and had been populated by the pre-removal search, so
/// the second search hit the cache and returned the deleted symbol. The fix
/// bumps `graph_generation` on remove (mirroring `index`), invalidating the
/// generation-keyed caches. Driven end-to-end through the real daemon — the sole
/// DB writer and the live query store — with a fixed `--instance` so the repo
/// uid is stable.
#[test]
fn daemon_remove_repo_invalidates_query_cache() {
    const INSTANCE: &str = "nw054";
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("rr54").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // Repo with a distinctive symbol name we can search for.
    std::fs::create_dir_all(&repo_dir).unwrap();
    StdCommand::new("git")
        .args(["init"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    std::fs::write(repo_dir.join("m.js"), "function gonefn() { return 1; }").unwrap();
    StdCommand::new("git")
        .args(["add", "."])
        .current_dir(&repo_dir)
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
        .current_dir(&repo_dir)
        .output()
        .unwrap();

    let _guard = DaemonGuard::new(&db_path);

    // Initial index through the daemon (auto-starts it) under a fixed instance.
    index_via_daemon_instance(&repo_dir, &db_path, INSTANCE);
    std::thread::sleep(Duration::from_secs(3));

    // Prime the daemon's in-memory `symbol_name_cache` with a search that finds
    // the symbol (populates the cache at the current graph_generation).
    daemon_cmd()
        .args(["search", "gonefn", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("Found").and(contains("gonefn")));

    // Remove the repo through the daemon (normal operation).
    daemon_cmd()
        .args([
            "remove-repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("symbol(s) deleted"));

    // The read-back nw-048 avoided: a search through the SAME daemon must now
    // report the symbol as gone. Pre-fix the stale generation-keyed cache still
    // returns "Found 1 symbol(s)"; post-fix the generation bump invalidates it
    // and the daemon re-scans the store → "No symbols found".
    daemon_cmd()
        .args(["search", "gonefn", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("No symbols found"));
}

/// nw-054 (sibling coverage): `prune-stale` deletes graph nodes for repos whose
/// working tree has vanished, but — like `remove-repo` before the fix — it did
/// not bump `graph_generation`. So a `search` primed before the prune kept
/// returning the pruned symbol out of the stale generation-keyed
/// `symbol_name_cache`. The fix bumps the generation after a prune removes
/// anything (mirroring `remove_repo`/`index`), invalidating those caches.
///
/// This is the same read-back nw-054 proved for `remove-repo`, driven through
/// the real daemon (the sole DB writer and live query store): index a repo,
/// prime the symbol cache with a search, delete the repo's working tree so it is
/// now stale, prune it via the daemon, then re-search and assert the symbol is
/// gone. Pre-fix the stale cache returns "Found"; post-fix the bump forces a
/// re-scan → "No symbols found".
///
/// `remove-vault` and `remove-project` take the identical one-line
/// `bump_and_persist_generation()` fix after their deletions (they delete note
/// and project nodes respectively); this prune-stale case exercises the shared
/// generation-invalidation pathway end-to-end for all three.
#[test]
fn daemon_prune_stale_invalidates_query_cache() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("ps54").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // Repo with a distinctive symbol name we can search for.
    std::fs::create_dir_all(&repo_dir).unwrap();
    StdCommand::new("git")
        .args(["init"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    std::fs::write(repo_dir.join("m.js"), "function prunedfn() { return 1; }").unwrap();
    StdCommand::new("git")
        .args(["add", "."])
        .current_dir(&repo_dir)
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
        .current_dir(&repo_dir)
        .output()
        .unwrap();

    let _guard = DaemonGuard::new(&db_path);

    // Initial index through the daemon (auto-starts it).
    index_via_daemon(&repo_dir, &db_path);
    std::thread::sleep(Duration::from_secs(3));

    // Prime the daemon's in-memory `symbol_name_cache` with a search that finds
    // the symbol (populates the cache at the current graph_generation).
    daemon_cmd()
        .args(["search", "prunedfn", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("Found").and(contains("prunedfn")));

    // Make the repo stale: its working tree no longer exists on disk. The DB
    // (under db_path) is untouched — only the source tree is removed.
    std::fs::remove_dir_all(&repo_dir).unwrap();

    // Prune through the same running daemon — detects the vanished tree and
    // deletes the repo's files + symbols.
    daemon_cmd()
        .args(["prune-stale", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("Pruned").and(contains("stale source")));

    // Read-back through the SAME daemon: the pruned symbol must be gone. Pre-fix
    // the stale generation-keyed cache still returns "Found 1 symbol(s)";
    // post-fix the generation bump invalidates it and the daemon re-scans the
    // store → "No symbols found".
    daemon_cmd()
        .args(["search", "prunedfn", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("No symbols found"));
}

/// nw-055 (P1a): `prune-stale` must drop the pruned repo's change-detection
/// sidecar slices — the exact data-loss class nw-048 fixed for `remove-repo`,
/// but in the prune path. Pre-fix, `prune-stale` deleted the graph rows but left
/// the repo's `.filemeta` slice behind. So if the working tree returns at the
/// SAME path, the stale slice classifies its files as `Unchanged` (same size →
/// Tier-2 skip) → a re-index finds 0 files → the symbol is never restored →
/// search returns nothing. The fix drops the sidecar slices inside
/// `prune_stale_repos` (uid-scoped, fail-safe), mirroring `remove_repo`.
///
/// Driven end-to-end through the real daemon (the sole DB writer). A fixed
/// `--instance` keeps the repo uid stable across index/prune/re-index so the
/// stale slice is actually consulted. We assert on the daemon's OWN re-index
/// file count (deterministic, no read-back needed) exactly as nw-048 does.
#[test]
fn daemon_prune_stale_drops_sidecar_slices_so_readd_reindexes() {
    const INSTANCE: &str = "nw055";
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("ps55").join("test.lbug");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    // Helper: init a git repo at repo_dir with the distinctive symbol file.
    let make_repo = || {
        std::fs::create_dir_all(&repo_dir).unwrap();
        StdCommand::new("git")
            .args(["init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        std::fs::write(repo_dir.join("m.js"), "function readdfn() { return 1; }").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(&repo_dir)
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
            .current_dir(&repo_dir)
            .output()
            .unwrap();
    };

    make_repo();

    let _guard = DaemonGuard::new(&db_path);

    // Initial index through the daemon under a fixed instance/uid creates the DB
    // + the `.filemeta` sidecar slice.
    index_via_daemon_instance(&repo_dir, &db_path, INSTANCE);
    std::thread::sleep(Duration::from_secs(3));

    // Sanity: the symbol is searchable before the prune.
    daemon_cmd()
        .args(["search", "readdfn", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("readdfn"));

    // Make the repo stale: remove its working tree so prune-stale detects it.
    std::fs::remove_dir_all(&repo_dir).unwrap();

    // Prune through the daemon. Pre-fix this leaks the `.filemeta` slice.
    daemon_cmd()
        .args(["prune-stale", "--db", &db_path.display().to_string()])
        .assert()
        .success();

    // The working tree returns at the SAME path with the SAME file (same size).
    make_repo();

    // Re-index the SAME path through the daemon under the SAME instance/uid.
    // Pre-fix: the orphaned `.filemeta` slice makes m.js classify `Unchanged`
    // → the daemon re-indexes 0 files → the symbol is never restored ("Indexed
    // 0 files"). Post-fix: prune dropped the slice → the daemon re-indexes the
    // file ("Indexed 1 files") and restores the symbol.
    let reindex = daemon_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--instance",
            INSTANCE,
        ])
        .assert()
        .success();
    let reindex_out = reindex.get_output();
    let reindex_log = format!(
        "{}{}",
        String::from_utf8_lossy(&reindex_out.stdout),
        String::from_utf8_lossy(&reindex_out.stderr),
    );
    assert!(
        reindex_log.contains("Indexed 1 files"),
        "re-index after prune-stale must re-index the file (nw-055/P1a); \
         pre-fix it reports 'Indexed 0 files'. Got:\n{reindex_log}"
    );

    // And the symbol is searchable again through the daemon (nw-054 fixed the
    // query cache, so this read-back is now reliable too).
    daemon_cmd()
        .args(["search", "readdfn", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("readdfn"));
}
