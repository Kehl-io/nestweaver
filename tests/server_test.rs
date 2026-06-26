//! Integration tests for NestWeaver server mode.
//!
//! Run with:
//!   cargo test --test server_test -- --test-threads=1

mod helpers;

use std::process::Command as StdCommand;

use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;
use nestweaver_proto::{BrainStatusRequest, RepoStatesRequest};
use tonic::transport::{Certificate, ClientTlsConfig};
use serde_json::json;

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

/// Test cert bundle: CA cert (for client trust), server cert + key (for server identity).
struct TestCerts {
    ca_cert: std::path::PathBuf,
    server_cert: std::path::PathBuf,
    server_key: std::path::PathBuf,
}

/// Generate a CA certificate and a server certificate signed by it.
/// Returns (ca_cert, server_cert, server_key) paths.
fn generate_test_certs(dir: &std::path::Path) -> TestCerts {
    let ca_key = dir.join("ca-key.pem");
    let ca_cert = dir.join("ca-cert.pem");
    let server_key = dir.join("server-key.pem");
    let server_csr = dir.join("server.csr");
    let server_cert = dir.join("server-cert.pem");

    // 1. Generate CA key + self-signed CA cert
    let output = StdCommand::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048",
            "-keyout", &ca_key.display().to_string(),
            "-out", &ca_cert.display().to_string(),
            "-days", "1", "-nodes",
            "-subj", "/CN=Test CA",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .expect("openssl must be installed for TLS tests");
    assert!(output.status.success(), "CA cert generation failed");

    // 2. Generate server key + CSR
    let output = StdCommand::new("openssl")
        .args([
            "req", "-newkey", "rsa:2048",
            "-keyout", &server_key.display().to_string(),
            "-out", &server_csr.display().to_string(),
            "-nodes",
            "-subj", "/CN=localhost",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .expect("openssl CSR generation failed");
    assert!(output.status.success(), "server CSR generation failed");

    // 3. Sign server cert with CA
    let ext_file = dir.join("ext.cnf");
    std::fs::write(&ext_file, "subjectAltName=DNS:localhost,IP:127.0.0.1\n").unwrap();

    let output = StdCommand::new("openssl")
        .args([
            "x509", "-req",
            "-in", &server_csr.display().to_string(),
            "-CA", &ca_cert.display().to_string(),
            "-CAkey", &ca_key.display().to_string(),
            "-CAcreateserial",
            "-out", &server_cert.display().to_string(),
            "-days", "1",
            "-extfile", &ext_file.display().to_string(),
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .expect("openssl cert signing failed");
    assert!(output.status.success(), "server cert signing failed");

    TestCerts { ca_cert, server_cert, server_key }
}

#[tokio::test]
async fn server_tls_connection() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    // Index first so the DB exists.
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

    let certs = generate_test_certs(dir.path());

    let guard = helpers::server_guard::ServerGuard::start_with_tls(
        &db_path,
        &certs.server_cert,
        &certs.server_key,
    );
    let port = guard.grpc_port();

    // Client trusts the CA that signed the server cert.
    let ca_pem = std::fs::read(&certs.ca_cert).expect("read CA cert");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem))
        .domain_name("localhost");

    let channel = tonic::transport::Channel::from_shared(format!("https://127.0.0.1:{port}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .expect("failed to connect to TLS gRPC server");

    let mut client = NestWeaverDaemonClient::new(channel);

    let response = client
        .brain_status(BrainStatusRequest {})
        .await
        .expect("BrainStatus RPC should succeed over TLS");

    let status = response.into_inner();
    assert!(
        status.repo_count >= 1,
        "expected at least 1 repo, got {}",
        status.repo_count
    );
}

#[tokio::test]
async fn server_tls_rejects_plain_tcp() {
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

    let certs = generate_test_certs(dir.path());

    let guard = helpers::server_guard::ServerGuard::start_with_tls(
        &db_path,
        &certs.server_cert,
        &certs.server_key,
    );

    // Try to connect without TLS — should fail.
    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await;

    match channel {
        Err(_) => {
            // Connection refused or failed — expected when TLS is required.
        }
        Ok(ch) => {
            // Connection might succeed at TCP level but the RPC should fail.
            let mut client = NestWeaverDaemonClient::new(ch);
            let result = client.brain_status(BrainStatusRequest {}).await;
            assert!(
                result.is_err(),
                "plain TCP RPC should fail when server requires TLS"
            );
        }
    }
}

#[tokio::test]
async fn server_repo_states_rpc() {
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
        .repo_states(RepoStatesRequest {})
        .await
        .expect("RepoStates RPC failed");

    let repo_states = response.into_inner();
    assert!(
        !repo_states.repos.is_empty(),
        "expected at least 1 repo in RepoStates response"
    );

    let repo = &repo_states.repos[0];
    assert!(
        !repo.indexed_sha.is_empty(),
        "expected non-empty indexed_sha"
    );
    assert!(
        !repo.repo_uid.is_empty(),
        "expected non-empty repo_uid"
    );
    assert!(
        !repo.repo_name.is_empty(),
        "expected non-empty repo_name"
    );
}

#[tokio::test]
async fn server_mcp_http_initialize() {
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

    let guard = helpers::server_guard::ServerGuard::start(&db_path);
    let mcp_addr = guard.mcp_addr();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
        }))
        .send()
        .await
        .expect("MCP HTTP request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], 1);
    assert_eq!(body["jsonrpc"], "2.0");
    assert!(body["result"]["protocolVersion"].is_string());
    assert_eq!(body["result"]["serverInfo"]["name"], "nestweaver-brain");
}

#[tokio::test]
async fn server_mcp_http_tools_list() {
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

    let guard = helpers::server_guard::ServerGuard::start(&db_path);
    let mcp_addr = guard.mcp_addr();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
        }))
        .send()
        .await
        .expect("MCP HTTP request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], 2);
    let tools = body["result"]["tools"]
        .as_array()
        .expect("tools should be an array");
    assert!(
        tools.len() >= 30,
        "expected 30+ tools, got {}",
        tools.len()
    );
}

#[tokio::test]
async fn server_mcp_http_brain_status_tool() {
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

    let guard = helpers::server_guard::ServerGuard::start(&db_path);
    let mcp_addr = guard.mcp_addr();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "brain_status",
                "arguments": {}
            }
        }))
        .send()
        .await
        .expect("MCP HTTP tools/call request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], 10);
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["result"]["isError"], false);

    // brain_status returns repo_count in structuredContent
    let structured = &body["result"]["structuredContent"];
    assert!(
        structured["repo_count"].is_number(),
        "expected repo_count in structuredContent, got: {structured}"
    );
    assert!(
        structured["repo_count"].as_i64().unwrap() >= 1,
        "expected at least 1 repo, got {}",
        structured["repo_count"]
    );
}

#[tokio::test]
async fn server_mcp_sessions_tracked() {
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

    let guard = helpers::server_guard::ServerGuard::start(&db_path);
    let mcp_addr = guard.mcp_addr();
    let client = reqwest::Client::new();

    // 1. Initialize — should get Mcp-Session-Id header back.
    let resp1 = client
        .post(format!("{mcp_addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
        }))
        .send()
        .await
        .expect("first initialize failed");

    assert_eq!(resp1.status(), 200);
    let session_id_1 = resp1
        .headers()
        .get("mcp-session-id")
        .expect("initialize response should contain Mcp-Session-Id header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        !session_id_1.is_empty(),
        "session ID should not be empty"
    );

    // 2. Second initialize — should get a *different* session ID.
    let resp2 = client
        .post(format!("{mcp_addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
        }))
        .send()
        .await
        .expect("second initialize failed");

    assert_eq!(resp2.status(), 200);
    let session_id_2 = resp2
        .headers()
        .get("mcp-session-id")
        .expect("second initialize should also return Mcp-Session-Id")
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(
        session_id_1, session_id_2,
        "two initialize calls should produce different session IDs"
    );

    // 3. tools/call WITH session header — should succeed.
    let resp3 = client
        .post(format!("{mcp_addr}/mcp"))
        .header("mcp-session-id", &session_id_1)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list",
        }))
        .send()
        .await
        .expect("tools/list with session failed");

    assert_eq!(resp3.status(), 200);
    let body3: serde_json::Value = resp3.json().await.unwrap();
    assert!(
        body3["result"]["tools"].is_array(),
        "tools/list should return tools array"
    );

    // 4. tools/list WITHOUT session header — should also work (stateless fallback).
    let resp4 = client
        .post(format!("{mcp_addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/list",
        }))
        .send()
        .await
        .expect("stateless tools/list failed");

    assert_eq!(resp4.status(), 200);
    let body4: serde_json::Value = resp4.json().await.unwrap();
    assert!(
        body4["result"]["tools"].is_array(),
        "stateless tools/list should return tools array"
    );
}
