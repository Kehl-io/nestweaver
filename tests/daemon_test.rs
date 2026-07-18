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

    // Start the daemon.
    daemon_action_cmd(&db_path, "start").assert().success();

    std::thread::sleep(Duration::from_secs(1));

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
    daemon_action_cmd(&db_path, "start").assert().success();
    std::thread::sleep(Duration::from_secs(3));

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
    daemon_action_cmd(&db_path, "start").assert().success();
    std::thread::sleep(Duration::from_secs(3));

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
