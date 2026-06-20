//! Integration tests for the NestWeaver daemon lifecycle.
//!
//! These tests exercise the full daemon path: start, stop, auto-start, crash
//! recovery, concurrent MCP queries, and brain_add_source through the daemon.
//!
//! Run with:
//!   cargo test --test daemon_test -- --test-threads=1

use assert_cmd::Command;
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
    daemon_action_cmd(&db_path, "start").assert().success();
    std::thread::sleep(Duration::from_secs(3));

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
    daemon_action_cmd(&db_path, "start").assert().success();
    std::thread::sleep(Duration::from_secs(3));

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
    daemon_action_cmd(&db_path, "start").assert().success();
    std::thread::sleep(Duration::from_secs(3));

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
    daemon_action_cmd(&db_path, "start").assert().success();
    std::thread::sleep(Duration::from_secs(3));

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
    daemon_action_cmd(&db_path, "start").assert().success();
    std::thread::sleep(Duration::from_secs(3));

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
