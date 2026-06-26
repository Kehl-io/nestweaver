//! Integration tests for NestWeaver server mode.
//!
//! Run with:
//!   cargo test --test server_test -- --test-threads=1

mod helpers;

use std::process::Command as StdCommand;

use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;
use nestweaver_proto::BrainStatusRequest;

/// Create a minimal git repo with a JS file for indexing.
fn write_test_repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    StdCommand::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::fs::write(
        dir.join("main.js"),
        "function greet(name) { return name; }",
    )
    .unwrap();
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

#[test]
fn server_starts_and_writes_port_file() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    // Index first (no daemon) so the DB exists.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let guard = helpers::server_guard::ServerGuard::start(&db_path);
    let port = guard.grpc_port();
    assert!(port > 0, "bound port should be nonzero");
}

#[tokio::test]
async fn server_tcp_brain_status() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    // Index first (no daemon) so the DB exists.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let guard = helpers::server_guard::ServerGuard::start(&db_path);

    // Connect tonic gRPC client to TCP port
    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await
        .expect("failed to connect to TCP gRPC server");

    let mut client = NestWeaverDaemonClient::new(channel);

    let response = client
        .brain_status(BrainStatusRequest {})
        .await
        .expect("BrainStatus RPC failed over TCP");

    let status = response.into_inner();
    // We indexed one repo, so repo_count should be >= 1
    assert!(
        status.repo_count >= 1,
        "expected at least 1 repo, got {}",
        status.repo_count
    );
}

#[tokio::test]
async fn server_transport_parity() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    // Index (no daemon) so the DB exists.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let guard = helpers::server_guard::ServerGuard::start(&db_path);

    // ── TCP query via tonic ──────────────────────────────────────────
    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await
        .expect("failed to connect to TCP gRPC server");
    let mut tcp_client = NestWeaverDaemonClient::new(channel);
    let tcp_status = tcp_client
        .brain_status(BrainStatusRequest {})
        .await
        .expect("BrainStatus RPC failed over TCP")
        .into_inner();

    // ── UDS query via CLI (daemon is running, CLI routes through UDS) ─
    let cli_output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args([
            "brain",
            "status",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        cli_output.status.success(),
        "CLI status --json failed: {}",
        String::from_utf8_lossy(&cli_output.stderr)
    );

    // Parse CLI JSON output. The pretty-printed JSON may span multiple lines,
    // so we find the first '{' and parse from there.
    let stdout = String::from_utf8_lossy(&cli_output.stdout);
    let json_start = stdout
        .find('{')
        .expect("no JSON object in CLI output");
    let cli_json: serde_json::Value =
        serde_json::from_str(&stdout[json_start..]).expect("CLI output is not valid JSON");

    // ── Assert parity ────────────────────────────────────────────────
    let cli_repo_count = cli_json["repo_count"]
        .as_i64()
        .expect("CLI JSON missing repo_count");
    assert_eq!(
        tcp_status.repo_count as i64, cli_repo_count,
        "repo_count mismatch between TCP ({}) and UDS ({})",
        tcp_status.repo_count, cli_repo_count
    );

    let cli_vault_count = cli_json["vault_count"]
        .as_i64()
        .expect("CLI JSON missing vault_count");
    assert_eq!(
        tcp_status.vault_count as i64, cli_vault_count,
        "vault_count mismatch between TCP ({}) and UDS ({})",
        tcp_status.vault_count, cli_vault_count
    );

    let cli_notes = cli_json["notes"]
        .as_i64()
        .expect("CLI JSON missing notes");
    assert_eq!(
        tcp_status.notes as i64, cli_notes,
        "notes mismatch between TCP ({}) and UDS ({})",
        tcp_status.notes, cli_notes
    );
}

#[tokio::test]
async fn server_auth_rejects_unauthenticated() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let guard = helpers::server_guard::ServerGuard::start_with_auth(&db_path, "test-secret");

    // Connect without a bearer token — should be rejected.
    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await
        .expect("failed to connect to TCP gRPC server");

    let mut client = NestWeaverDaemonClient::new(channel);

    let result = client.brain_status(BrainStatusRequest {}).await;
    assert!(result.is_err(), "expected UNAUTHENTICATED error");
    let status = result.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::Unauthenticated,
        "expected UNAUTHENTICATED, got {:?}",
        status.code()
    );
}

#[tokio::test]
async fn server_auth_passes_valid_token() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let guard = helpers::server_guard::ServerGuard::start_with_auth(&db_path, "test-secret");

    // Connect WITH a valid bearer token — should succeed.
    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await
        .expect("failed to connect to TCP gRPC server");

    let mut client = NestWeaverDaemonClient::with_interceptor(channel, |mut req: tonic::Request<()>| {
        req.metadata_mut().insert(
            "authorization",
            "Bearer test-secret".parse().unwrap(),
        );
        Ok(req)
    });

    let response = client
        .brain_status(BrainStatusRequest {})
        .await
        .expect("BrainStatus RPC should succeed with valid token");

    let status = response.into_inner();
    assert!(
        status.repo_count >= 1,
        "expected at least 1 repo, got {}",
        status.repo_count
    );
}
