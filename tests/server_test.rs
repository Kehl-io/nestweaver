//! Integration tests for NestWeaver server mode.
//!
//! Run with:
//!   cargo test --test server_test -- --test-threads=1

mod helpers;

use std::process::Command as StdCommand;

use hmac::{Hmac, KeyInit, Mac};
use nestweaver_client::DaemonClient;
use nestweaver_client::discovery::{RoutingMode, UpstreamConfig};
use nestweaver_client::hybrid::{
    HybridClient, TraceBoundary, detect_boundaries_in_trace, flow_trace_with_stitching,
};
use nestweaver_client::upstream::UpstreamHandle;
use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;
use nestweaver_proto::{
    BackupRequest, BrainSearchRequest, BrainStatusRequest, JsonRequest, RepoStatesRequest,
};
use serde_json::{Value, json};
use sha2::Sha256;
use tonic::transport::{Certificate, ClientTlsConfig};

/// Create a minimal git repo with a JS file for indexing.
fn write_test_repo(dir: &std::path::Path) {
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

/// Create a git repo populated with the given `(relative_path, contents)`
/// files. Parent directories are created on demand so callers can place files
/// under subdirectories (e.g. `"server/main.js"`).
fn write_repo_files(dir: &std::path::Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    StdCommand::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    for (rel, contents) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
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

/// Index `repo_dir` into `db_path` using the no-daemon path (so the on-disk DB
/// exists before a `ServerGuard` serves it). Panics with the indexer's stderr
/// on failure. Returns the child's combined stdout+stderr so tests can embed
/// it in forensic failure messages (nw-043); most callers may ignore it.
fn index_repo(repo_dir: &std::path::Path, db_path: &std::path::Path) -> String {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
    format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A bearer token that satisfies the daemon's 32-byte minimum for
/// `--auth-token`. Used for every server started in the hybrid tests.
const HYBRID_TOKEN: &str = "hybrid-integration-secret-token-0123456789abcdef";

/// Connect a local `DaemonClient` to an already-running daemon (started via
/// `ServerGuard`) over its Unix domain socket. The socket is bound before the
/// port file is written, so by the time `ServerGuard::start` returns it should
/// exist — but retry briefly to absorb any accept-loop start-up jitter. The
/// budget (10 s) is sized for full-workspace parallel test load, where the
/// daemon's accept loop can be starved well past the old 10×150 ms window.
async fn connect_local(db_path: &std::path::Path) -> DaemonClient {
    let mut last_err = None;
    for _ in 0..40 {
        match DaemonClient::connect_existing(db_path).await {
            Ok(client) => return client,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
    panic!(
        "failed to connect to local daemon over UDS: {}",
        last_err.expect("at least one attempt")
    );
}

/// Build an `UpstreamHandle` in `Merge` mode pointing at a `ServerGuard`'s gRPC
/// address with the given bearer token. Empty `repos` globs => matches every
/// query, so the merge path always selects it. The upstream timeout is
/// load-tolerant: 5 s was too tight when the full workspace test suite runs in
/// parallel and the upstream server competes for CPU.
fn merge_upstream(grpc_addr: String, token: &str) -> UpstreamHandle {
    let cfg = UpstreamConfig {
        name: Some("server".to_string()),
        url: grpc_addr,
        token: Some(token.to_string()),
        mode: RoutingMode::Merge,
        repos: vec![],
        timeout: "15s".to_string(),
        ca_cert: None,
    };
    UpstreamHandle::from_config(&cfg).expect("build upstream handle")
}

/// Collect `_meta.sources` from a hybrid response as owned strings.
fn meta_sources(resp: &Value) -> Vec<String> {
    resp["_meta"]["sources"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
    let json_start = stdout.find('{').expect("no JSON object in CLI output");
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

    let cli_notes = cli_json["notes"].as_i64().expect("CLI JSON missing notes");
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let guard = helpers::server_guard::ServerGuard::start_with_auth(
        &db_path,
        "test-secret-token-0123456789abcdef",
    );

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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let guard = helpers::server_guard::ServerGuard::start_with_auth(
        &db_path,
        "test-secret-token-0123456789abcdef",
    );

    // Connect WITH a valid bearer token — should succeed.
    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await
        .expect("failed to connect to TCP gRPC server");

    let mut client =
        NestWeaverDaemonClient::with_interceptor(channel, |mut req: tonic::Request<()>| {
            req.metadata_mut().insert(
                "authorization",
                "Bearer test-secret-token-0123456789abcdef".parse().unwrap(),
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
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            &ca_key.display().to_string(),
            "-out",
            &ca_cert.display().to_string(),
            "-days",
            "1",
            "-nodes",
            "-subj",
            "/CN=Test CA",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .expect("openssl must be installed for TLS tests");
    assert!(output.status.success(), "CA cert generation failed");

    // 2. Generate server key + CSR
    let output = StdCommand::new("openssl")
        .args([
            "req",
            "-newkey",
            "rsa:2048",
            "-keyout",
            &server_key.display().to_string(),
            "-out",
            &server_csr.display().to_string(),
            "-nodes",
            "-subj",
            "/CN=localhost",
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
            "x509",
            "-req",
            "-in",
            &server_csr.display().to_string(),
            "-CA",
            &ca_cert.display().to_string(),
            "-CAkey",
            &ca_key.display().to_string(),
            "-CAcreateserial",
            "-out",
            &server_cert.display().to_string(),
            "-days",
            "1",
            "-extfile",
            &ext_file.display().to_string(),
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .expect("openssl cert signing failed");
    assert!(output.status.success(), "server cert signing failed");

    TestCerts {
        ca_cert,
        server_cert,
        server_key,
    }
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
    assert!(!repo.repo_uid.is_empty(), "expected non-empty repo_uid");
    assert!(!repo.repo_name.is_empty(), "expected non-empty repo_name");
}

#[tokio::test]
async fn server_mcp_http_initialize() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
    assert!(tools.len() >= 30, "expected 30+ tools, got {}", tools.len());
}

#[tokio::test]
async fn server_mcp_http_brain_status_tool() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
    assert!(!session_id_1.is_empty(), "session ID should not be empty");

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

/// Regression guard: the MCP-over-HTTP transport must thread `server_mode`
/// into tool dispatch. Before the fix, `brain_status` over HTTP reported
/// `server_mode: false` even when running `--server`, and the same missing
/// thread-local made `read_symbols` read from an empty filesystem.
#[tokio::test]
async fn server_mcp_http_reports_server_mode_true() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
            "id": 21,
            "method": "tools/call",
            "params": { "name": "brain_status", "arguments": {} }
        }))
        .send()
        .await
        .expect("MCP HTTP tools/call request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let structured = &body["result"]["structuredContent"];
    assert_eq!(
        structured["server_mode"],
        json!(true),
        "brain_status over MCP-HTTP must report server_mode=true when running --server, got: {structured}"
    );
}

/// Regression guard: in server mode, `read_symbols` over MCP-HTTP must take the
/// server (bare-clone) code path rather than silently reading the filesystem.
/// This harness indexes from a working tree with no bare-clone workspace, so
/// the server branch is exercised and surfaces the bare-clone diagnostic note —
/// which only appears when `is_server_mode()` is true on the HTTP dispatch
/// thread. The companion `bare_index_test` proves GitBareReader returns bodies.
#[tokio::test]
async fn server_mcp_http_read_symbols_takes_server_path() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
            "id": 22,
            "method": "tools/call",
            "params": {
                "name": "read_symbols",
                "arguments": { "targets": ["greet"] }
            }
        }))
        .send()
        .await
        .expect("MCP HTTP read_symbols request failed");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["result"]["isError"],
        json!(false),
        "read_symbols over HTTP should not error: {}",
        body
    );
    let structured = &body["result"]["structuredContent"];
    let note = structured["server_note"].as_str().unwrap_or("");
    assert!(
        note.contains("server mode"),
        "read_symbols in server mode must take the server code path (server_note expected); got: {structured}"
    );
}

/// Regression guard: the authenticated MCP-HTTP boundary must redact blast
/// totals before applying `limit`. A query-scoped caller must never receive the
/// larger admin-visible total or any hidden repo identifiers.
#[tokio::test]
async fn server_mcp_http_blast_count_is_exact_within_visible_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("authz").join("test.lbug");
    let visible_repo = dir.path().join("visible_repo");
    let hidden_repo = dir.path().join("hidden_repo");

    write_repo_files(
        &visible_repo,
        &[
            (
                "src/visible_target.js",
                "export function visibleimpacttarget() { return 1; }",
            ),
            (
                "src/visible_callers.js",
                r#"
import { visibleimpacttarget } from "./visible_target.js";
export function visiblecaller_one() { return visibleimpacttarget(); }
export function visiblecaller_two() { return visibleimpacttarget(); }
"#,
            ),
        ],
    );
    write_repo_files(
        &hidden_repo,
        &[
            (
                "src/hidden_target.js",
                "export function hiddenimpacttarget() { return 1; }",
            ),
            (
                "src/hidden_callers.js",
                r#"
import { hiddenimpacttarget } from "./hidden_target.js";
export function hiddencaller_one() { return hiddenimpacttarget(); }
export function hiddencaller_two() { return hiddenimpacttarget(); }
export function hiddencaller_three() { return hiddenimpacttarget(); }
"#,
            ),
        ],
    );
    index_repo(&visible_repo, &db_path);
    index_repo(&hidden_repo, &db_path);

    let query_token = "authz-query-token-0123456789abcdef012345";
    let admin_token = "authz-admin-token-0123456789abcdef012345";
    let config_path = dir.path().join("instance.toml");
    let config = format!(
        r#"
instance_id = "authz-process-test"
repos = []

[snapshot_storage]
backend = "local"
path = "/tmp/nw-authz-test-snapshots"

[workspace]
backend = "local"
path = "/tmp/nw-authz-test-workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[authz.rules]
"{query_token}" = ["*visible_repo*"]
"#,
    );
    std::fs::write(&config_path, config).unwrap();

    let guard = helpers::server_guard::ServerGuard::start_with_admin_auth_and_config(
        &db_path,
        query_token,
        admin_token,
        &config_path,
    );
    let mcp_addr = guard.mcp_addr();
    let client = reqwest::Client::new();
    let arguments = json!({
        "changed_files": ["src/visible_target.js", "src/hidden_target.js"],
        "limit": 50
    });

    let admin_response = client
        .post(format!("{mcp_addr}/mcp"))
        .bearer_auth(admin_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 51,
            "method": "tools/call",
            "params": {
                "name": "blast_radius",
                "arguments": arguments.clone()
            }
        }))
        .send()
        .await
        .expect("admin MCP HTTP blast_radius request failed");
    assert_eq!(admin_response.status(), 200);
    let admin_body: Value = admin_response.json().await.unwrap();
    assert_eq!(
        admin_body["result"]["isError"],
        json!(false),
        "admin blast_radius failed: {admin_body}"
    );
    let admin = &admin_body["result"]["structuredContent"];

    let query_response = client
        .post(format!("{mcp_addr}/mcp"))
        .bearer_auth(query_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 52,
            "method": "tools/call",
            "params": {
                "name": "blast_radius",
                "arguments": {
                    "changed_files": ["src/visible_target.js", "src/hidden_target.js"],
                    "limit": 1
                }
            }
        }))
        .send()
        .await
        .expect("query MCP HTTP blast_radius request failed");
    assert_eq!(query_response.status(), 200);
    let query_body: Value = query_response.json().await.unwrap();
    assert_eq!(
        query_body["result"]["isError"],
        json!(false),
        "query-scoped blast_radius failed: {query_body}"
    );
    let visible = &query_body["result"]["structuredContent"];

    let admin_total = admin["affected_symbol_count"]
        .as_u64()
        .expect("admin affected_symbol_count");
    let visible_total = visible["affected_symbol_count"]
        .as_u64()
        .expect("visible affected_symbol_count");
    assert_eq!(
        admin_total, 5,
        "admin fixture should include two visible and three hidden affected symbols: {admin}"
    );
    assert_eq!(
        visible_total, 2,
        "restricted total must count only the two visible affected symbols: {visible}"
    );
    assert_eq!(
        visible["returned_affected_symbol_count"],
        json!(1),
        "small limit should return one visible row: {visible}"
    );
    assert_eq!(
        visible["affected_symbols_truncated"],
        json!(true),
        "restricted total should remain exact when the visible rows are truncated: {visible}"
    );

    let admin_serialized = admin.to_string();
    assert!(
        admin_serialized.contains("hiddencaller"),
        "admin fixture must prove hidden affected rows exist: {admin}"
    );
    let visible_serialized = visible.to_string();
    for hidden_marker in [
        "hidden_repo",
        "hidden_callers.js",
        "hiddenimpacttarget",
        "hiddencaller",
    ] {
        assert!(
            !visible_serialized.contains(hidden_marker),
            "query-scoped output leaked {hidden_marker}: {visible}"
        );
    }
}

// ── Webhook integration tests ───────────────────────────────────────────

/// Compute HMAC-SHA256 signature in GitHub's `sha256=<hex>` format.
fn webhook_sign(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(body);
    let result = mac.finalize().into_bytes();
    format!("sha256={}", hex::encode(result))
}

/// Compute HMAC-SHA256 signature in Gitea's raw-hex format (no `sha256=` prefix).
fn webhook_sign_gitea(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[tokio::test]
async fn server_webhook_enqueues_job() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    // Index once so the DB exists.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let secret = "webhook-test-secret-token-0123456789abcdef";
    let guard = helpers::server_guard::ServerGuard::start_with_webhook(&db_path, secret);
    let mcp_addr = guard.mcp_addr();

    let payload = json!({
        "repository": {
            "clone_url": "https://github.com/acme/api-service.git"
        },
        "ref": "refs/heads/main"
    });
    let body = serde_json::to_vec(&payload).unwrap();
    let sig = webhook_sign(&body, secret);

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/webhook"))
        .header("x-hub-signature-256", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("webhook POST failed");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert_eq!(text, "accepted");

    // Stop the daemon first so the jobs sidecar can be opened from a SINGLE
    // connection — opening a second JobQueue while the daemon runs risks the
    // SIGBUS-on-WAL-checkpoint the shared-queue design exists to avoid.
    drop(guard);

    // The webhook must actually ENQUEUE a job, not merely return 200/"accepted".
    let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
    let depth = nestweaver_engine::jobs::JobQueue::open(&jobs_path)
        .expect("open jobs queue")
        .queue_depth()
        .expect("queue depth");
    let total = depth.pending + depth.running + depth.succeeded + depth.dead_letter;
    assert!(
        total >= 1,
        "webhook should have enqueued a job, got queue depth {depth:?}"
    );
}

#[tokio::test]
async fn server_webhook_gitea_enqueues_job() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    // Index once so the DB exists.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let secret = "webhook-test-secret-token-0123456789abcdef";
    let guard = helpers::server_guard::ServerGuard::start_with_webhook(&db_path, secret);
    let mcp_addr = guard.mcp_addr();

    let payload = json!({
        "repository": {
            "clone_url": "https://gitea.example.com/acme/api-service.git"
        },
        "ref": "refs/heads/main"
    });
    let body = serde_json::to_vec(&payload).unwrap();
    // Gitea sends a raw-hex HMAC in x-gitea-signature (no `sha256=` prefix).
    let sig = webhook_sign_gitea(&body, secret);

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/webhook"))
        .header("x-gitea-signature", &sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("webhook POST failed");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert_eq!(text, "accepted");

    // Stop the daemon first so the jobs sidecar can be opened from a SINGLE
    // connection — opening a second JobQueue while the daemon runs risks the
    // SIGBUS-on-WAL-checkpoint the shared-queue design exists to avoid.
    drop(guard);

    // The webhook must actually ENQUEUE a job, not merely return 200/"accepted".
    let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
    let depth = nestweaver_engine::jobs::JobQueue::open(&jobs_path)
        .expect("open jobs queue")
        .queue_depth()
        .expect("queue depth");
    let total = depth.pending + depth.running + depth.succeeded + depth.dead_letter;
    assert!(
        total >= 1,
        "webhook should have enqueued a job, got queue depth {depth:?}"
    );
}

#[tokio::test]
async fn server_webhook_rejects_invalid_sig() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let guard = helpers::server_guard::ServerGuard::start_with_webhook(
        &db_path,
        "correct-webhook-secret-0123456789",
    );
    let mcp_addr = guard.mcp_addr();

    let payload = json!({
        "repository": {
            "clone_url": "https://github.com/acme/api-service.git"
        }
    });
    let body = serde_json::to_vec(&payload).unwrap();
    // Sign with the wrong secret.
    let wrong_sig = webhook_sign(&body, "wrong-webhook-secret-0123456789");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/webhook"))
        .header("x-hub-signature-256", &wrong_sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("webhook POST failed");

    assert_eq!(resp.status(), 401);
}

/// During secret rotation the server accepts BOTH the new secret and the
/// previous one (`--webhook-secret-old`), so an overlap window exists where a
/// provider still signing with the old secret is not rejected — the gap
/// Sourcegraph's single-secret model suffers. A payload signed with the OLD
/// secret must be accepted (200 + enqueued); a bogus secret still 401s.
#[tokio::test]
async fn server_webhook_dual_secret_rotation_accepts_old() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let new_secret = "new-webhook-secret-0123456789abcdef";
    let old_secret = "old-webhook-secret-fedcba9876543210";
    let guard = helpers::server_guard::ServerGuard::start_with_webhook_rotation(
        &db_path, new_secret, old_secret,
    );
    let mcp_addr = guard.mcp_addr();

    let payload = json!({
        "repository": { "clone_url": "https://github.com/acme/api-service.git" },
        "ref": "refs/heads/main"
    });
    let body = serde_json::to_vec(&payload).unwrap();
    let client = reqwest::Client::new();

    // Signed with the OLD secret — must still be accepted during the overlap.
    let old_sig = webhook_sign(&body, old_secret);
    let resp = client
        .post(format!("{mcp_addr}/webhook"))
        .header("x-hub-signature-256", &old_sig)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("webhook POST failed");
    assert_eq!(
        resp.status(),
        200,
        "old secret must be accepted mid-rotation"
    );
    assert_eq!(resp.text().await.unwrap(), "accepted");

    // A bogus secret is still rejected even with two valid secrets configured.
    let bogus_sig = webhook_sign(&body, "not-either-configured-secret");
    let resp = client
        .post(format!("{mcp_addr}/webhook"))
        .header("x-hub-signature-256", &bogus_sig)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("webhook POST failed");
    assert_eq!(resp.status(), 401, "a non-matching secret must still 401");

    drop(guard);

    let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
    let depth = nestweaver_engine::jobs::JobQueue::open(&jobs_path)
        .expect("open jobs queue")
        .queue_depth()
        .expect("queue depth");
    let total = depth.pending + depth.running + depth.succeeded + depth.dead_letter;
    assert!(
        total >= 1,
        "old-secret webhook should have enqueued a job, got {depth:?}"
    );
}

#[tokio::test]
async fn server_webhook_rejects_missing_sig() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let guard = helpers::server_guard::ServerGuard::start_with_webhook(
        &db_path,
        "my-webhook-secret-abcdef",
    );
    let mcp_addr = guard.mcp_addr();

    let payload = json!({
        "repository": {
            "clone_url": "https://github.com/acme/api-service.git"
        }
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/webhook"))
        .header("content-type", "application/json")
        .json(&payload)
        .send()
        .await
        .expect("webhook POST failed");

    // No signature header at all -> 401
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn server_webhook_rejects_bad_json() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let secret = "my-webhook-secret-abcdef";
    let guard = helpers::server_guard::ServerGuard::start_with_webhook(&db_path, secret);
    let mcp_addr = guard.mcp_addr();

    let body = b"not valid json";
    let sig = webhook_sign(body, secret);

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/webhook"))
        .header("x-hub-signature-256", &sig)
        .header("content-type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("webhook POST failed");

    assert_eq!(resp.status(), 400);
}

// ── Device-flow integration tests ───────────────────────────────────────

/// End-to-end OAuth 2.0 device grant (RFC 8628) over real HTTP: a developer
/// requests a code, polls before approval (authorization_pending), an admin
/// approves with the user code, and the next poll exchanges the device code for
/// the configured query token. This drives the same three endpoints as
/// `nestweaver_client::connect::device_flow_authenticate` without its
/// browser-open/stdin side effects; the client's poll-loop branch logic is
/// unit-tested separately in `nestweaver-client/src/connect.rs`.
#[tokio::test]
async fn server_device_flow_grants_query_token_after_admin_approval() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let auth_token = "device-flow-query-token-0123456789abcdef";
    let admin_token = "device-flow-admin-token-0123456789abcdef";
    let guard = helpers::server_guard::ServerGuard::start_with_admin_and_auth(
        &db_path,
        auth_token,
        admin_token,
    );
    let base = guard.mcp_addr();
    let client = reqwest::Client::new();

    // 1. Request a device + user code.
    let resp = client
        .post(format!("{base}/auth/device"))
        .json(&json!({}))
        .send()
        .await
        .expect("POST /auth/device failed");
    assert_eq!(resp.status(), 200);
    let device: Value = resp.json().await.unwrap();
    let device_code = device["device_code"]
        .as_str()
        .expect("device_code present")
        .to_string();
    let user_code = device["user_code"]
        .as_str()
        .expect("user_code present")
        .to_string();

    // 2. Poll before approval — must report authorization_pending (not 2xx).
    let resp = client
        .post(format!("{base}/auth/token"))
        .json(&json!({ "device_code": device_code }))
        .send()
        .await
        .expect("POST /auth/token (pending) failed");
    assert!(
        !resp.status().is_success(),
        "token poll before approval should not succeed"
    );
    let pending: Value = resp.json().await.unwrap();
    assert_eq!(pending["error"], json!("authorization_pending"));

    // 3. Approval is admin-gated: an unauthenticated approve must be rejected so
    // a developer can't self-approve their own grant.
    let resp = client
        .post(format!("{base}/auth/device/approve"))
        .json(&json!({ "user_code": user_code }))
        .send()
        .await
        .expect("POST /auth/device/approve (no token) failed");
    assert_eq!(
        resp.status(),
        401,
        "approve without admin token must be 401"
    );

    // 4. Admin approves the pending grant by user code.
    let resp = client
        .post(format!("{base}/auth/device/approve"))
        .bearer_auth(admin_token)
        .json(&json!({ "user_code": user_code }))
        .send()
        .await
        .expect("POST /auth/device/approve failed");
    assert_eq!(resp.status(), 200, "admin approval should succeed");

    // 5. Next poll exchanges the device code for the configured query token.
    let resp = client
        .post(format!("{base}/auth/token"))
        .json(&json!({ "device_code": device_code }))
        .send()
        .await
        .expect("POST /auth/token (granted) failed");
    assert_eq!(resp.status(), 200);
    let granted: Value = resp.json().await.unwrap();
    assert_eq!(
        granted["access_token"],
        json!(auth_token),
        "device flow must return the configured query token"
    );
}

// ── `server status` CLI integration tests ───────────────────────────────

/// `nestweaver server status` over the admin HTTP API: with the correct admin
/// token it renders the concise status summary; with a wrong token it maps the
/// 401 to a clear authentication error and a non-zero exit.
#[tokio::test]
async fn server_status_cli_happy_path_and_401() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let auth_token = "status-query-token-0123456789abcdef0123";
    let admin_token = "status-admin-token-0123456789abcdef0123";
    let guard = helpers::server_guard::ServerGuard::start_with_admin_and_auth(
        &db_path,
        auth_token,
        admin_token,
    );
    let url = guard.mcp_addr();

    // Happy path: correct admin token → exit 0, concise summary on stdout.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env_remove("NESTWEAVER_ADMIN_TOKEN")
        .args(["server", "status", "--url", &url, "--token", admin_token])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "server status should succeed with the admin token; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Connected to") && stdout.contains("Instance:"),
        "status summary missing expected fields; got: {stdout}"
    );

    // 401 mapping: wrong admin token → non-zero exit, clear auth error.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env_remove("NESTWEAVER_ADMIN_TOKEN")
        .args([
            "server",
            "status",
            "--url",
            &url,
            "--token",
            "wrong-admin-token-0123456789abcdef0123",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "server status with a wrong token should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("authentication failed"),
        "wrong token should map to an authentication error; got: {stderr}"
    );
}

#[tokio::test]
async fn export_graph_rejects_file_output() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    // Index first (no daemon) so the DB exists.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await
        .expect("failed to connect to TCP gRPC server");

    let mut client = NestWeaverDaemonClient::new(channel);

    let output_path = dir.path().join("export_output.cypher");
    let args_json = serde_json::to_string(&json!({
        "format": "cypher",
        "output": output_path.display().to_string(),
    }))
    .unwrap();

    let result = client.export_graph(JsonRequest { args_json }).await;

    assert!(result.is_err(), "expected PERMISSION_DENIED error");
    let status = result.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::PermissionDenied,
        "expected PERMISSION_DENIED, got {:?}: {}",
        status.code(),
        status.message()
    );

    assert!(
        !output_path.exists(),
        "export output file should NOT have been created in server mode"
    );
}

#[tokio::test]
async fn export_graph_rejects_msgpack_file_output() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    // Index first (no daemon) so the DB exists.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await
        .expect("failed to connect to TCP gRPC server");

    let mut client = NestWeaverDaemonClient::new(channel);

    let output_path = dir.path().join("export_output.graph.msgpack");
    let args_json = serde_json::to_string(&json!({
        "format": "msgpack",
        "output": output_path.display().to_string(),
    }))
    .unwrap();

    let result = client.export_graph(JsonRequest { args_json }).await;

    assert!(result.is_err(), "expected PERMISSION_DENIED error");
    let status = result.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::PermissionDenied,
        "expected PERMISSION_DENIED, got {:?}: {}",
        status.code(),
        status.message()
    );

    assert!(
        !output_path.exists(),
        "msgpack export output file should NOT have been created in server mode"
    );
}

// ── Hybrid (local + server) end-to-end tests ────────────────────────────────
//
// These exercise `nestweaver_client::HybridClient` against TWO real daemons:
// a local daemon (reached over its Unix socket via `DaemonClient`) and a
// `--server` daemon (reached over authenticated gRPC via an `UpstreamHandle`).
// Each daemon indexes a different repo into a different DB, so a genuine merge
// must combine both sources. Tokens are always >= 32 bytes (`HYBRID_TOKEN`).

/// MUST-HAVE: a real local+server MERGE. Repo A is indexed into an
/// authenticated server; a *different* repo B into a separate local daemon. A
/// `brain_search` routed through `HybridClient` (upstream mode = Merge) must
/// report BOTH `"local"` and `"server"` in `_meta.sources` (never `"upstream"`)
/// and the merged results must contain the server-only symbol — proving data
/// actually flowed across the boundary rather than the sources label being
/// cosmetic.
#[tokio::test]
async fn hybrid_brain_search_merge_combines_local_and_server_sources() {
    let dir = tempfile::tempdir().unwrap();

    // Server side: repo A in its own subdir/DB (separate `server.port`).
    let server_repo = dir.path().join("repo_a");
    let db_server = dir.path().join("server").join("server.lbug");
    write_repo_files(
        &server_repo,
        &[(
            "main.js",
            "function serverfn(x) { return x; }\nfunction sharedfn(x) { return x; }",
        )],
    );
    index_repo(&server_repo, &db_server);

    // Local side: repo B in its own subdir/DB.
    let local_repo = dir.path().join("repo_b");
    let db_local = dir.path().join("local").join("local.lbug");
    write_repo_files(
        &local_repo,
        &[(
            "main.js",
            "function localfn(x) { return x; }\nfunction sharedfn(x) { return x; }",
        )],
    );
    index_repo(&local_repo, &db_local);

    let server = helpers::server_guard::ServerGuard::start_with_auth(&db_server, HYBRID_TOKEN);
    let _local_guard = helpers::server_guard::ServerGuard::start(&db_local);

    let mut local = connect_local(&db_local).await;

    // The typed local transport must preserve the display-independent total.
    let local_small = local
        .inner_mut()
        .search(BrainSearchRequest {
            query: "fn".to_string(),
            limit: 1,
            response_format: None,
            include_bodies: false,
            prf: false,
            rerank: false,
            root: None,
        })
        .await
        .expect("typed local brain_search limit=1")
        .into_inner();
    let local_large = local
        .inner_mut()
        .search(BrainSearchRequest {
            query: "fn".to_string(),
            limit: 20,
            response_format: None,
            include_bodies: false,
            prf: false,
            rerank: false,
            root: None,
        })
        .await
        .expect("typed local brain_search limit=20")
        .into_inner();
    assert_eq!(local_small.total_matches_relation, "eq");
    assert_eq!(local_large.total_matches_relation, "eq");
    assert_eq!(local_small.total_matches, local_large.total_matches);
    assert_eq!(local_small.returned_matches, 1);
    assert!(local_small.truncated);
    assert_eq!(local_large.returned_matches, local_large.total_matches);
    assert!(!local_large.truncated);

    let upstream = merge_upstream(server.grpc_addr(), HYBRID_TOKEN);
    let mut hybrid = HybridClient::from_parts(local, vec![upstream]);

    // A source-side display cap makes the hybrid union a conservative lower
    // bound even when the rows returned by both sources happen to be unique.
    let merged_small = hybrid
        .query("brain_search", &json!({ "query": "fn", "limit": 1 }))
        .await
        .expect("hybrid brain_search lower-bound merge query");
    assert_eq!(merged_small["total_matches_relation"], "gte");
    assert_eq!(
        merged_small["returned_matches"].as_u64(),
        merged_small["results"]
            .as_array()
            .map(|rows| rows.len() as u64)
    );
    assert_eq!(merged_small["truncated"], true);
    assert!(merged_small["total_matches"].as_u64().is_some());

    // With both sources complete, RRF dedup has the complete union and may
    // report an exact total without summing overlapping source totals.
    let merged_large = hybrid
        .query("brain_search", &json!({ "query": "fn", "limit": 20 }))
        .await
        .expect("hybrid brain_search exact merge query");
    assert_eq!(merged_large["total_matches_relation"], "eq");
    assert_eq!(
        merged_large["total_matches"].as_u64(),
        merged_large["results"]
            .as_array()
            .map(|rows| rows.len() as u64)
    );
    assert_eq!(
        merged_large["returned_matches"],
        merged_large["total_matches"]
    );
    assert_eq!(merged_large["truncated"], false);

    // `serverfn` exists ONLY on the server. If it shows up in the merged
    // response, the server query genuinely contributed.
    let resp = hybrid
        .query("brain_search", &json!({ "query": "serverfn", "limit": 20 }))
        .await
        .expect("hybrid brain_search merge query");

    let sources = meta_sources(&resp);
    assert!(
        sources.iter().any(|s| s == "local"),
        "merge sources must include 'local'; got {sources:?} in {resp}"
    );
    assert!(
        sources.iter().any(|s| s == "server"),
        "merge sources must include 'server'; got {sources:?} in {resp}"
    );
    assert!(
        !sources.iter().any(|s| s == "upstream"),
        "sources must label the remote 'server', never 'upstream'; got {sources:?}"
    );
    assert!(
        resp.to_string().contains("serverfn"),
        "merged results must contain the server-only symbol 'serverfn' \
         (proof the server side contributed); got {resp}"
    );
    assert_eq!(resp["total_matches_relation"], "eq");
    assert_eq!(resp["returned_matches"], resp["total_matches"]);
    assert_eq!(resp["truncated"], false);

    // Symmetry: a local-only symbol must also surface through the same merge,
    // with both sources still labelled.
    let resp_local = hybrid
        .query("brain_search", &json!({ "query": "localfn", "limit": 20 }))
        .await
        .expect("hybrid brain_search merge query (local symbol)");
    let sources_local = meta_sources(&resp_local);
    assert!(
        sources_local.iter().any(|s| s == "local") && sources_local.iter().any(|s| s == "server"),
        "merge for a local-only symbol must still report both sources; got {sources_local:?}"
    );
    assert!(
        resp_local.to_string().contains("localfn"),
        "merged results must contain the local-only symbol 'localfn'; got {resp_local}"
    );

    // Exercise the real non-JSON CLI renderer through daemon + configured
    // merge routing. A hybrid result must never claim it is a substring
    // fallback merely because its engine is not the single-source "bm25".
    let cfg_path = dir.path().join("instance.toml");
    write_upstream_config(&cfg_path, "server", &server.grpc_addr(), HYBRID_TOKEN);
    // Retry-bound the CLI probe: even with the generous upstream timeout, a
    // transient ejection under CI load may yield a local-only first answer;
    // the circuit breaker re-probes and recovers, so the hybrid path must
    // appear within a bounded number of attempts.
    let mut stdout = String::new();
    for attempt in 1..=10 {
        let cli = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
            .args([
                "brain",
                "search",
                "fn",
                "--db",
                &db_local.display().to_string(),
                "--config",
                &cfg_path.display().to_string(),
            ])
            .current_dir(dir.path())
            .output()
            .expect("run real hybrid text-mode CLI search");
        assert!(
            cli.status.success(),
            "hybrid text-mode CLI failed: {}",
            String::from_utf8_lossy(&cli.stderr)
        );
        stdout = String::from_utf8_lossy(&cli.stdout).into_owned();
        if stdout.contains("Brain search (hybrid)") {
            break;
        }
        assert!(
            attempt < 10,
            "hybrid text output must identify the hybrid engine after {attempt} attempt(s): {stdout}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        stdout.contains("Brain search (hybrid)"),
        "hybrid text output must identify the hybrid engine: {stdout}"
    );
    assert!(
        !stdout.contains("substring fallback"),
        "hybrid text output must not claim substring fallback: {stdout}"
    );
}

/// ATTEMPT: two-tier `blast_radius`. The local daemon indexes a file under
/// `local/`, the server a file under `server/`. With both paths in
/// `changed_files`, the two-tier response must carry a populated `local_impact`
/// (local changed symbols) AND a server-sourced `org_wide_impact` whose results
/// survive the same-repo dedup (distinct path prefixes) — i.e. both tiers
/// populated with real data, with the org tier genuinely reached over
/// authenticated gRPC.
#[tokio::test]
async fn hybrid_blast_radius_two_tier_populates_both_tiers() {
    let dir = tempfile::tempdir().unwrap();

    let server_repo = dir.path().join("repo_a");
    let db_server = dir.path().join("server").join("server.lbug");
    write_repo_files(
        &server_repo,
        &[("server/main.js", "function serverimpactfn(x) { return x; }")],
    );
    index_repo(&server_repo, &db_server);

    let local_repo = dir.path().join("repo_b");
    let db_local = dir.path().join("local").join("local.lbug");
    write_repo_files(
        &local_repo,
        &[("local/main.js", "function localimpactfn(x) { return x; }")],
    );
    index_repo(&local_repo, &db_local);

    let server = helpers::server_guard::ServerGuard::start_with_auth(&db_server, HYBRID_TOKEN);
    let _local_guard = helpers::server_guard::ServerGuard::start(&db_local);

    let local = connect_local(&db_local).await;
    let upstream = merge_upstream(server.grpc_addr(), HYBRID_TOKEN);
    let mut hybrid = HybridClient::from_parts(local, vec![upstream]);

    let resp = hybrid
        .query(
            "blast_radius",
            &json!({ "changed_files": ["local/main.js", "server/main.js"], "max_depth": 3 }),
        )
        .await
        .expect("hybrid blast_radius two-tier query");

    assert_eq!(
        resp["tier"], "two_tier",
        "blast_radius through the hybrid client must produce a two-tier response; got {resp}"
    );

    // Local tier populated with the local repo's changed symbol.
    let local_changed = resp["local_impact"]["changed_symbols"]
        .as_array()
        .expect("local_impact.changed_symbols array");
    assert!(
        !local_changed.is_empty(),
        "local_impact must contain the local changed symbol; got {}",
        resp["local_impact"]
    );
    assert!(
        resp["local_impact"].to_string().contains("localimpactfn"),
        "local_impact should reference 'localimpactfn'; got {}",
        resp["local_impact"]
    );

    // Org tier genuinely reached the authenticated server (not the
    // "unavailable" fallback) and carries the server repo's symbol.
    assert_eq!(
        resp["org_wide_impact"]["source_server"], "server",
        "org_wide_impact must be attributed to the 'server' upstream; got {}",
        resp["org_wide_impact"]
    );
    assert!(
        resp["org_wide_impact"].get("status").is_none(),
        "org_wide_impact must NOT be the 'unavailable' fallback — the server tier \
         must be reached; got {}",
        resp["org_wide_impact"]
    );
    let org_changed = resp["org_wide_impact"]["results"]["changed_symbols"]
        .as_array()
        .expect("org_wide_impact.results.changed_symbols array");
    assert!(
        !org_changed.is_empty(),
        "org_wide_impact.results must contain the server's changed symbol (survives \
         same-repo dedup via distinct path prefix); got {}",
        resp["org_wide_impact"]
    );
    assert!(
        resp["org_wide_impact"]
            .to_string()
            .contains("serverimpactfn"),
        "org_wide_impact should reference the server-only 'serverimpactfn'; got {}",
        resp["org_wide_impact"]
    );

    let sources = meta_sources(&resp);
    assert!(
        sources.iter().any(|s| s == "local") && sources.iter().any(|s| s == "server"),
        "two-tier response must report both 'local' and 'server' sources; got {sources:?}"
    );
}

/// ATTEMPT: cross-boundary `flow_trace` continuation. The same repo is indexed
/// into both daemons so symbol `canonical_id`s line up across them (they are
/// URL-derived). `flow_trace_with_stitching` runs the trace locally, then sends
/// a `FlowTraceContinue` RPC to the authenticated server for the boundary
/// symbol and stitches the returned spans back into the tree. We assert the
/// stitched result gained server-sourced nodes (`source: "server:server"`),
/// proving the continuation returned a non-empty result across the boundary.
#[tokio::test]
async fn hybrid_flow_trace_continue_stitches_server_spans() {
    let dir = tempfile::tempdir().unwrap();

    // One repo, indexed into both DBs => matching canonical_ids on both sides.
    let repo = dir.path().join("flowrepo");
    write_repo_files(
        &repo,
        &[(
            "main.js",
            "function serverfn(x) { return serverhelper(x); }\n\
             function serverhelper(y) { return y + 1; }",
        )],
    );
    let db_server = dir.path().join("server").join("server.lbug");
    let db_local = dir.path().join("local").join("local.lbug");
    index_repo(&repo, &db_server);
    index_repo(&repo, &db_local);

    let server = helpers::server_guard::ServerGuard::start_with_auth(&db_server, HYBRID_TOKEN);
    let _local_guard = helpers::server_guard::ServerGuard::start(&db_local);

    let mut local = connect_local(&db_local).await;

    // Pull the entry symbol's canonical_id from the local trace; the same id
    // resolves on the server because both indexed the same repo URL.
    let ft_args = serde_json::to_string(&json!({ "symbol": "serverfn", "max_depth": 5 })).unwrap();
    let ft_resp = local
        .inner_mut()
        .flow_trace(JsonRequest { args_json: ft_args })
        .await
        .expect("local flow_trace RPC")
        .into_inner();
    let ft: Value = serde_json::from_str(&ft_resp.result_json).expect("flow_trace JSON");
    let entry_cid = ft["tree"]["canonical_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("flow_trace tree must carry a canonical_id; got {ft}"))
        .to_string();
    // The local trace must have followed the in-repo call edge.
    assert!(
        ft["tree"]["children"]
            .as_array()
            .is_some_and(|c| !c.is_empty()),
        "local flow_trace should follow serverfn -> serverhelper; got {ft}"
    );

    let upstream = merge_upstream(server.grpc_addr(), HYBRID_TOKEN);
    let mut hybrid = HybridClient::from_parts(local, vec![upstream]);

    let boundary = TraceBoundary {
        canonical_id: entry_cid,
        name: "serverfn".to_string(),
        parent_path: vec![],
    };
    let params = json!({ "symbol": "serverfn", "max_depth": 5 });
    let stitched = flow_trace_with_stitching(&mut hybrid, &params, std::slice::from_ref(&boundary))
        .await
        .expect("flow_trace_with_stitching");

    let serialized = stitched.to_string();
    assert!(
        serialized.contains("server:server"),
        "stitched trace must contain server-sourced nodes (source = \"server:server\"), \
         proving FlowTraceContinue returned a non-empty continuation across the boundary; \
         got {stitched}"
    );
    // The server continuation walked the call graph (serverfn -> serverhelper),
    // so the stitched-in server subtree carries serverhelper.
    assert!(
        serialized.matches("serverhelper").count() >= 2,
        "server continuation should re-walk serverfn -> serverhelper and stitch it in \
         (expected serverhelper both locally and from the server); got {stitched}"
    );

    let sources = meta_sources(&stitched);
    assert!(
        sources.iter().any(|s| s == "local") && sources.iter().any(|s| s == "server"),
        "stitched flow_trace must report both 'local' and 'server' sources; got {sources:?}"
    );
}

/// Stronger sibling of `hybrid_flow_trace_continue_stitches_server_spans`:
/// two GENUINELY DISTINCT repos and EMPTY explicit boundaries, so the
/// cross-repo boundary must be AUTO-detected by `detect_boundaries_in_trace`.
///
/// Setup: the `caller` repo (entryfn -> midfn) is indexed only into the local
/// DB; the `callee` repo (remotefn -> remotehelper) is indexed only into the
/// server DB. The cross-repo linker's output is simulated by copying the
/// callee's `remotefn` Symbol verbatim into the local DB (preserving its
/// foreign `repo_uid` and real `canonical_id` byte-for-byte — canonical_ids
/// embed a repo-URL hash, so recomputing would break cross-boundary matching)
/// with NO outgoing CALLS edges, plus a `CROSS_REPO_LINK` midfn -> stub.
/// That makes the stub satisfy every auto-detection condition: a leaf, a
/// non-empty `repo_uid` different from the trace root's, a non-empty
/// `canonical_id`, and a `repo_uid` absent from the local daemon's repo set.
///
/// `flow_trace_with_stitching` is called with `&[]` boundaries; the stitched
/// result must contain a `server:server` span AND `remotehelper` — the server
/// resolved the boundary via `symbol_by_canonical_id` and continued into the
/// callee's REAL call graph, which the local index has never seen.
#[tokio::test]
async fn hybrid_flow_trace_auto_detects_cross_repo_boundary() {
    let dir = tempfile::tempdir().unwrap();

    // Two distinct repos => distinct repo URLs => distinct repo_uids and
    // distinct canonical_id repo-hashes.
    let caller = dir.path().join("caller");
    write_repo_files(
        &caller,
        &[(
            "main.js",
            "function entryfn(x) { return midfn(x); }\n\
             function midfn(y) { return y; }",
        )],
    );
    let callee = dir.path().join("callee");
    write_repo_files(
        &callee,
        &[(
            "lib.js",
            "function remotefn(x) { return remotehelper(x); }\n\
             function remotehelper(y) { return y + 1; }",
        )],
    );

    let db_server = dir.path().join("server").join("server.lbug");
    let db_local = dir.path().join("local").join("local.lbug");
    // Indexer output is kept in memory solely for the nw-043 forensic dump on
    // the (never-reproduced) isolation-anomaly path below; no cost otherwise.
    let server_index_output = index_repo(&callee, &db_server);
    let local_index_output = index_repo(&caller, &db_local);

    // All direct GraphStore access happens strictly BEFORE any daemon serves
    // these DBs (the daemon is the sole writer once running). The block scopes
    // the store handles so they drop before the ServerGuards start.
    let local_repo_uids: std::collections::HashSet<String> = {
        // Read the REAL callee Symbol from the server DB — its canonical_id
        // and repo_uid must cross the boundary byte-for-byte.
        let server_store =
            nestweaver_store::GraphStore::open_read_only(&db_server).expect("open db_server");
        let remotefn = server_store
            .lookup_symbols_by_name("remotefn")
            .expect("lookup remotefn")
            .into_iter()
            .next()
            .expect("server DB must contain remotefn");
        assert!(
            remotefn
                .canonical_id
                .as_deref()
                .is_some_and(|c| !c.is_empty()),
            "indexed remotefn must carry a non-empty canonical_id; got {remotefn:?}"
        );
        drop(server_store);

        let local_store = nestweaver_store::GraphStore::open(&db_local).expect("open db_local");
        let midfn = local_store
            .lookup_symbols_by_name("midfn")
            .expect("lookup midfn")
            .into_iter()
            .next()
            .expect("local DB must contain midfn");
        assert_ne!(
            midfn.repo_uid, remotefn.repo_uid,
            "the two fixture repos must be genuinely distinct"
        );

        // Inject the unresolved cross-repo stub: the callee Symbol copied
        // verbatim (foreign repo_uid + real canonical_id, no outgoing CALLS
        // edges => a leaf in the local trace), linked from midfn.
        local_store.insert_symbol(&remotefn).expect("insert stub");
        local_store
            .insert_cross_repo_link(&midfn.uid, &remotefn.uid, 0.9, "shared-symbol")
            .expect("insert CROSS_REPO_LINK");

        let uids: std::collections::HashSet<String> = local_store
            .list_repos(None)
            .expect("list local repos")
            .into_iter()
            .map(|r| r.uid)
            .collect();
        // The stub's repo_uid must be foreign to the local index. This tripped
        // exactly once (nw-043, 1-in-54) and never reproduced; if it recurs,
        // capture everything needed to classify the anomaly BEFORE panicking:
        //   durable mis-write  -> row still present on fresh re-open
        //   transient read     -> row absent on fresh re-open
        //   test-env confusion -> dir listings / indexer logs show it
        if uids.contains(&remotefn.repo_uid) {
            // Forensic re-list on the ORIGINAL handle (with full row detail).
            let full_rows = local_store.list_repos(None).expect("forensic re-list");
            let relist: Vec<String> = full_rows
                .iter()
                .map(|r| {
                    format!(
                        "uid={} url={} root_path={:?} sha={}",
                        r.uid, r.url, r.root_path, r.indexed_sha
                    )
                })
                .collect();
            // Drop the writer handle, then re-open fresh to discriminate
            // durable vs transient.
            drop(local_store);
            let reopened =
                nestweaver_store::GraphStore::open_read_only(&db_local).expect("forensic re-open");
            let reopened_uids: Vec<String> = reopened
                .list_repos(None)
                .expect("forensic re-open list")
                .into_iter()
                .map(|r| r.uid)
                .collect();
            let ls = |p: &std::path::Path| -> String {
                std::fs::read_dir(p.parent().unwrap())
                    .map(|d| {
                        d.filter_map(|e| e.ok())
                            .map(|e| {
                                format!(
                                    "{:?} {}b",
                                    e.file_name(),
                                    e.metadata().map(|m| m.len()).unwrap_or(0)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|e| e.to_string())
            };
            panic!(
                "nw-043 RECURRENCE — foreign repo row in local store.\n\
                 expected-foreign uid: {}\n\
                 in-handle rows: {relist:#?}\n\
                 fresh re-open uids: {reopened_uids:?}  (row present here => DURABLE mis-write; absent => TRANSIENT read)\n\
                 server db dir: [{}]\nlocal db dir: [{}]\n\
                 indexer(callee->server) output:\n{}\n\
                 indexer(caller->local) output:\n{}",
                remotefn.repo_uid,
                ls(&db_server),
                ls(&db_local),
                server_index_output,
                local_index_output,
            );
        }
        uids
    };

    let server = helpers::server_guard::ServerGuard::start_with_auth(&db_server, HYBRID_TOKEN);
    let _local_guard = helpers::server_guard::ServerGuard::start(&db_local);

    let mut local = connect_local(&db_local).await;

    // Detailed (non-concise) format: auto-detection needs the repo_uid +
    // canonical_id annotations that concise traces omit.
    let params = json!({
        "symbol": "entryfn",
        "max_depth": 5,
        "response_format": "detailed",
    });

    // Direct check that auto-detection fires on the raw local trace: the stub
    // leaf must be flagged as a boundary against the local repo set.
    let ft_args = serde_json::to_string(&params).unwrap();
    let ft_resp = local
        .inner_mut()
        .flow_trace(JsonRequest { args_json: ft_args })
        .await
        .expect("local flow_trace RPC")
        .into_inner();
    let ft: Value = serde_json::from_str(&ft_resp.result_json).expect("flow_trace JSON");
    let detected = detect_boundaries_in_trace(&ft, &local_repo_uids);
    assert!(
        !detected.is_empty(),
        "detect_boundaries_in_trace must auto-detect >= 1 cross-repo boundary; got trace {ft}"
    );
    assert!(
        detected.iter().any(|b| b.name == "remotefn"),
        "the auto-detected boundary should be the remotefn stub; got {detected:?}"
    );

    let upstream = merge_upstream(server.grpc_addr(), HYBRID_TOKEN);
    let mut hybrid = HybridClient::from_parts(local, vec![upstream]);

    // EMPTY explicit boundaries — stitching must auto-detect the boundary
    // itself (explicit boundaries would skip detect_boundaries_in_trace).
    let stitched = flow_trace_with_stitching(&mut hybrid, &params, &[])
        .await
        .expect("flow_trace_with_stitching");

    let serialized = stitched.to_string();
    assert!(
        serialized.contains("server:server"),
        "stitched trace must contain server-sourced nodes (source = \"server:server\"), \
         proving the auto-detected boundary triggered a FlowTraceContinue; got {stitched}"
    );
    // remotehelper exists ONLY in the server's index: its presence proves the
    // server resolved the boundary canonical_id and walked the callee repo's
    // real call graph (remotefn -> remotehelper), not just echoed the stub.
    assert!(
        serialized.contains("remotehelper"),
        "server continuation must traverse remotefn -> remotehelper in the callee repo \
         (remotehelper is unknown to the local index); got {stitched}"
    );

    let sources = meta_sources(&stitched);
    assert!(
        sources.iter().any(|s| s == "local") && sources.iter().any(|s| s == "server"),
        "stitched flow_trace must report both 'local' and 'server' sources (the 'server' \
         entry appears when a boundary was detected and a healthy upstream was consulted; \
         the server:server assertion above is what proves the continuation); got {sources:?}"
    );
}

#[tokio::test]
async fn server_backup_rpc_produces_snapshot() {
    // The daemon performs the whole backup in-process (holds its own write lock),
    // so a single admin-authed Backup RPC yields a snapshot file on the server.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_test_repo(&repo_dir);

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

    // Backup is admin-gated; use distinct >=32-byte tokens (startup rejects
    // short or equal tokens) and authenticate as admin.
    let admin = "admin-token-aaaaaaaaaaaaaaaaaaaaaaaa";
    let query = "query-token-bbbbbbbbbbbbbbbbbbbbbbbb";
    let guard =
        helpers::server_guard::ServerGuard::start_with_admin_and_auth(&db_path, query, admin);
    let channel = tonic::transport::Channel::from_shared(guard.grpc_addr())
        .unwrap()
        .connect()
        .await
        .expect("connect to daemon gRPC");
    let mut client = NestWeaverDaemonClient::new(channel);

    let out = dir.path().join("snap.nwsnap.zst");
    let mut req = tonic::Request::new(BackupRequest {
        output_path: out.to_string_lossy().into_owned(),
        include_clones: false,
    });
    req.metadata_mut()
        .insert("authorization", format!("Bearer {admin}").parse().unwrap());

    let resp = client
        .backup(req)
        .await
        .expect("Backup RPC failed")
        .into_inner();

    assert!(out.exists(), "daemon wrote the snapshot to output_path");
    assert_eq!(resp.output_path, out.to_string_lossy());
    assert_eq!(resp.tier, "standard");
}

// ── nw-017 Phase B: daemon-side federation at the /mcp boundary ───────────────

/// Write a minimal valid `instance.toml` with a single `[[upstream]]` block
/// pointing at `upstream_grpc_addr`. Mirrors the engine's `MINIMAL_TOML` fixture
/// (required `snapshot_storage`/`workspace`/`inference`/`git`/`repos` sections)
/// with `repos = []` so the fronting daemon indexes nothing of its own — its DB
/// is pre-built by `index_repo`.
fn write_upstream_config(
    path: &std::path::Path,
    upstream_name: &str,
    upstream_grpc_addr: &str,
    token: &str,
) {
    // Root-level scalar keys (`instance_id`, `repos`) MUST precede every table
    // header — in TOML any bare key after a `[table]` belongs to that table.
    let toml = format!(
        r#"
instance_id = "fronting-daemon"
repos = []

[snapshot_storage]
backend = "local"
path = "/tmp/nw-fed-snapshots"

[workspace]
backend = "local"
path = "/tmp/nw-fed-workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[upstream]]
name = "{upstream_name}"
url = "{upstream_grpc_addr}"
token = "{token}"
mode = "merge"
# Generous timeout: the config default (1s) is too tight for a loaded CI
# runner — one slow upstream response ejects the source and the merge
# degrades to local-only, flaking the hybrid assertions below.
timeout = "15s"
"#
    );
    std::fs::write(path, toml).expect("write instance.toml");
}

/// The daemon IS the federated coordinator at its `/mcp` boundary (ADR
/// Decision 2): a raw MCP client POSTing a two-tier-routed tool to a daemon
/// configured with an `[[upstream]]` gets a `{ local_impact, org_wide_impact }`
/// envelope plus federated provenance — no client-side `HybridClient` involved.
#[tokio::test]
async fn daemon_mcp_boundary_federates_two_tier() {
    let dir = tempfile::tempdir().unwrap();

    // Upstream ("org") server: indexes a repo whose symbol lives under server/.
    let server_repo = dir.path().join("repo_a");
    let db_server = dir.path().join("server").join("server.lbug");
    write_repo_files(
        &server_repo,
        &[("server/main.js", "function serverimpactfn(x) { return x; }")],
    );
    index_repo(&server_repo, &db_server);
    let upstream = helpers::server_guard::ServerGuard::start_with_auth(&db_server, HYBRID_TOKEN);

    // Fronting daemon: indexes a DISTINCT local repo, then serves it with an
    // instance.toml that points its federation coordinator at the upstream.
    let local_repo = dir.path().join("repo_b");
    let db_local = dir.path().join("local").join("local.lbug");
    write_repo_files(
        &local_repo,
        &[("local/main.js", "function localimpactfn(x) { return x; }")],
    );
    index_repo(&local_repo, &db_local);

    let cfg_path = dir.path().join("instance.toml");
    write_upstream_config(&cfg_path, "orgserver", &upstream.grpc_addr(), HYBRID_TOKEN);

    let fronting = helpers::server_guard::ServerGuard::start_with_config(&db_local, &cfg_path);
    let mcp_addr = fronting.mcp_addr();

    // Raw MCP client → fronting daemon /mcp. blast_radius is TwoTier-routed.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {
                "name": "blast_radius",
                "arguments": {
                    "changed_files": ["local/main.js", "server/main.js"],
                    "max_depth": 3
                }
            }
        }))
        .send()
        .await
        .expect("MCP HTTP tools/call request failed");

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], 42);
    assert_eq!(body["result"]["isError"], false, "got: {body}");

    // The tool value is the two-tier envelope the coordinator assembled.
    let structured = &body["result"]["structuredContent"];
    assert_eq!(
        structured["tier"], "two_tier",
        "daemon /mcp boundary must federate blast_radius into a two-tier envelope; got {structured}"
    );
    assert!(
        structured["local_impact"].is_object(),
        "local_impact tier must be populated; got {structured}"
    );
    assert!(
        structured["local_impact"]
            .to_string()
            .contains("localimpactfn"),
        "local_impact should reference the local symbol; got {}",
        structured["local_impact"]
    );
    assert_eq!(
        structured["org_wide_impact"]["source_server"], "orgserver",
        "org_wide_impact must be attributed to the configured upstream; got {}",
        structured["org_wide_impact"]
    );
    assert!(
        structured["org_wide_impact"].get("results").is_some(),
        "org_wide_impact must carry real server results (not the unavailable \
         fallback) — the upstream was reached; got {}",
        structured["org_wide_impact"]
    );
    assert!(
        structured["org_wide_impact"]
            .to_string()
            .contains("serverimpactfn"),
        "org_wide_impact should reference the server-only symbol; got {}",
        structured["org_wide_impact"]
    );

    // In-band provenance: federated scope, both daemon + upstream sourced.
    let meta = &body["result"]["_meta"];
    assert_eq!(
        meta["nestweaver.io/sources"],
        json!(["daemon", "orgserver"]),
        "federated result must list daemon + upstream as sources; got {meta}"
    );
    assert_eq!(
        meta["nestweaver.io/scope"], "federated",
        "federated result must stamp scope=federated; got {meta}"
    );
    assert!(
        meta["nestweaver.io/stale_repos"].is_array(),
        "stale_repos must be present as an array; got {meta}"
    );
}

/// A repository-restricted HTTP caller must never inherit the broader
/// configured upstream credential. The admin request proves the upstream
/// fixture can return its hidden canary; the restricted request must keep only
/// its already-authorized local tier and a generic, count-free withheld status.
#[tokio::test]
async fn daemon_mcp_boundary_withholds_two_tier_for_repo_restricted_caller() {
    let dir = tempfile::tempdir().unwrap();

    let hidden_repo = dir.path().join("hidden_upstream_repo");
    let db_server = dir.path().join("server").join("server.lbug");
    write_repo_files(
        &hidden_repo,
        &[
            (
                "shared/entry.js",
                "export function hiddenfederationcanary() { return 1; }",
            ),
            (
                "hidden/secret-derived-path.js",
                r#"
import { hiddenfederationcanary } from "../shared/entry.js";
export function hiddenfederationcaller() { return hiddenfederationcanary(); }
"#,
            ),
        ],
    );
    index_repo(&hidden_repo, &db_server);
    let upstream = helpers::server_guard::ServerGuard::start_with_auth(&db_server, HYBRID_TOKEN);

    let visible_repo = dir.path().join("visible_local_repo");
    let db_local = dir.path().join("local").join("local.lbug");
    write_repo_files(
        &visible_repo,
        &[
            (
                "shared/entry.js",
                "export function visiblefederationcanary() { return 1; }",
            ),
            (
                "tests/visible-federation.test.js",
                "export function visiblefederationtest() { return 1; }",
            ),
        ],
    );

    // Keep a second local repository in the same fronting daemon. Its symbols
    // and tests prove that repository scoping applies before each local
    // TwoTier result is counted or serialized, not only before federation.
    let hidden_local_repo = dir.path().join("hidden_local_repo");
    write_repo_files(
        &hidden_local_repo,
        &[
            (
                "hidden/local-caller.js",
                "export function hiddenlocalcaller() { return 1; }",
            ),
            (
                "hidden/secret-local-path.test.js",
                "export function hiddenlocaltest() { return 1; }",
            ),
        ],
    );
    index_repo(&hidden_local_repo, &db_local);
    index_repo(&visible_repo, &db_local);
    std::fs::write(
        hidden_local_repo.join("hidden/secret-local-path.test.js"),
        "export function hiddenlocaltest() { return 'hiddenbaserefcanary'; }",
    )
    .expect("dirty hidden local repo for base_ref scoping");
    {
        let store = nestweaver_store::GraphStore::open(&db_local).expect("open local graph");
        let target = store
            .lookup_symbols_by_name("visiblefederationcanary")
            .expect("lookup visible target")
            .remove(0);
        for caller_name in [
            "visiblefederationtest",
            "hiddenlocalcaller",
            "hiddenlocaltest",
        ] {
            let caller = store
                .lookup_symbols_by_name(caller_name)
                .unwrap_or_else(|error| panic!("lookup {caller_name}: {error}"))
                .remove(0);
            store
                .insert_edge(&nestweaver_schema::ResolvedEdge {
                    source_uid: caller.uid,
                    target_uid: target.uid.clone(),
                    edge_type: nestweaver_schema::EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap_or_else(|error| panic!("insert {caller_name} edge: {error}"));
        }
    }

    let query_token = "restricted-query-token-0123456789abcdef";
    let admin_token = "restricted-admin-token-0123456789abcdef";
    let cfg_path = dir.path().join("instance.toml");
    write_upstream_config(&cfg_path, "broad-org", &upstream.grpc_addr(), HYBRID_TOKEN);
    let mut config = std::fs::read_to_string(&cfg_path).expect("read instance config");
    config.push_str(&format!(
        r#"

[authz.rules]
"{query_token}" = ["*visible_local_repo*"]
"#
    ));
    std::fs::write(&cfg_path, config).expect("write authz rule");

    let fronting = helpers::server_guard::ServerGuard::start_with_admin_auth_and_config(
        &db_local,
        query_token,
        admin_token,
        &cfg_path,
    );
    let client = reqwest::Client::new();
    let endpoint = format!("{}/mcp", fronting.mcp_addr());
    let request = |id, tool_name: &str, arguments: Value| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments
            }
        })
    };
    let blast_arguments = json!({
        "changed_files": ["shared/entry.js"],
        "max_depth": 3
    });

    // Prove the configured service credential is broad enough to expose the
    // upstream-only canary. This also warms the unrestricted local cache,
    // guarding the restricted request against cross-scope reuse.
    let admin_response = client
        .post(&endpoint)
        .bearer_auth(admin_token)
        .json(&request(61, "blast_radius", blast_arguments.clone()))
        .send()
        .await
        .expect("admin MCP request");
    assert_eq!(admin_response.status(), 200);
    let admin_body: Value = admin_response.json().await.expect("admin JSON");
    let admin_structured = &admin_body["result"]["structuredContent"];
    assert_eq!(
        admin_structured["org_wide_impact"]["source_server"], "broad-org",
        "fixture must reach the broad upstream: {admin_structured}"
    );
    assert!(
        admin_structured["org_wide_impact"]
            .to_string()
            .contains("hiddenfederationcaller"),
        "fixture must prove the upstream returns a hidden-only symbol: {admin_structured}"
    );
    assert!(
        admin_structured["org_wide_impact"]
            .to_string()
            .contains("hidden/secret-derived-path.js"),
        "fixture must prove the upstream returns a hidden-only path: {admin_structured}"
    );

    let restricted_response = client
        .post(&endpoint)
        .bearer_auth(query_token)
        .json(&request(62, "blast_radius", blast_arguments))
        .send()
        .await
        .expect("restricted MCP request");
    assert_eq!(restricted_response.status(), 200);
    let restricted_body: Value = restricted_response.json().await.expect("restricted JSON");
    assert_eq!(
        restricted_body["result"]["isError"], false,
        "restricted request failed: {restricted_body}"
    );

    let structured = &restricted_body["result"]["structuredContent"];
    assert_eq!(structured["tier"], "two_tier");
    assert!(
        structured["local_impact"]
            .to_string()
            .contains("visiblefederationcanary"),
        "authorized local tier must remain available: {structured}"
    );
    let org = &structured["org_wide_impact"];
    assert_eq!(org["status"], "withheld");
    assert_eq!(org["reason"], "authorization-unproven");
    assert_eq!(
        org.as_object().expect("org status object").len(),
        2,
        "withheld org tier must expose no rows, paths, sources, or counts: {org}"
    );

    let serialized = restricted_body.to_string();
    for hidden_marker in [
        "hidden_upstream_repo",
        "hidden/secret-derived-path.js",
        "hiddenfederationcanary",
        "hiddenfederationcaller",
        "hidden_local_repo",
        "hidden/local-caller.js",
        "hidden/secret-local-path.test.js",
        "hiddenlocalcaller",
        "hiddenlocaltest",
        "broad-org",
    ] {
        assert!(
            !serialized.contains(hidden_marker),
            "restricted response leaked hidden upstream marker {hidden_marker}: {restricted_body}"
        );
    }
    let meta = &restricted_body["result"]["_meta"];
    assert_eq!(meta["nestweaver.io/sources"], json!(["daemon"]));
    assert_eq!(meta["nestweaver.io/scope"], "single-node");
    assert_eq!(
        meta["nestweaver.io/stale_repos"],
        json!([]),
        "global staleness cache must not leak repo URLs across caller scope"
    );

    // The unrestricted local tier proves both local repositories participate
    // in impact traversal. The restricted HTTP route must remove hidden local
    // rows before computing totals and before the federation envelope is
    // assembled.
    let impact_arguments = json!({
        "symbol": "visiblefederationcanary",
        "depth": 3
    });
    let admin_impact_response = client
        .post(&endpoint)
        .bearer_auth(admin_token)
        .json(&request(63, "brain_impact", impact_arguments.clone()))
        .send()
        .await
        .expect("admin brain_impact request");
    assert_eq!(admin_impact_response.status(), 200);
    let admin_impact_body: Value = admin_impact_response
        .json()
        .await
        .expect("admin brain_impact JSON");
    assert!(
        admin_impact_body["result"]["structuredContent"]["local_impact"]
            .to_string()
            .contains("hiddenlocalcaller"),
        "fixture must prove the unrestricted local impact includes a hidden-repo caller: \
         {admin_impact_body}"
    );

    let restricted_impact_response = client
        .post(&endpoint)
        .bearer_auth(query_token)
        .json(&request(64, "brain_impact", impact_arguments))
        .send()
        .await
        .expect("restricted brain_impact request");
    assert_eq!(restricted_impact_response.status(), 200);
    let restricted_impact_body: Value = restricted_impact_response
        .json()
        .await
        .expect("restricted brain_impact JSON");
    let restricted_impact = &restricted_impact_body["result"]["structuredContent"]["local_impact"];
    assert_eq!(restricted_impact["status"], "ok");
    assert_eq!(restricted_impact["total"], 1);
    assert_eq!(restricted_impact["returned"], 1);
    assert!(
        restricted_impact
            .to_string()
            .contains("visiblefederationtest"),
        "authorized caller must remain in the local impact: {restricted_impact_body}"
    );
    assert!(
        !restricted_impact_body.to_string().contains("hiddenlocal"),
        "restricted brain_impact leaked a hidden local row or count marker: \
         {restricted_impact_body}"
    );
    assert_eq!(
        restricted_impact_body["result"]["structuredContent"]["org_wide_impact"],
        json!({
            "status": "withheld",
            "reason": "authorization-unproven"
        })
    );

    // The same boundary contract applies to affected-test selection: the
    // hidden test is present for an admin, but it cannot contribute a row,
    // path, summary count, or learned aggregate for the restricted caller.
    let tests_arguments = json!({ "changed_files": ["shared/entry.js"] });
    let admin_tests_response = client
        .post(&endpoint)
        .bearer_auth(admin_token)
        .json(&request(65, "affected_tests", tests_arguments.clone()))
        .send()
        .await
        .expect("admin affected_tests request");
    assert_eq!(admin_tests_response.status(), 200);
    let admin_tests_body: Value = admin_tests_response
        .json()
        .await
        .expect("admin affected_tests JSON");
    assert!(
        admin_tests_body["result"]["structuredContent"]["local_impact"]
            .to_string()
            .contains("hiddenlocaltest"),
        "fixture must prove the unrestricted selection includes the hidden local test: \
         {admin_tests_body}"
    );

    let restricted_tests_response = client
        .post(&endpoint)
        .bearer_auth(query_token)
        .json(&request(66, "affected_tests", tests_arguments))
        .send()
        .await
        .expect("restricted affected_tests request");
    assert_eq!(restricted_tests_response.status(), 200);
    let restricted_tests_body: Value = restricted_tests_response
        .json()
        .await
        .expect("restricted affected_tests JSON");
    let restricted_tests = &restricted_tests_body["result"]["structuredContent"]["local_impact"];
    assert_eq!(
        restricted_tests["summary"],
        "1 tier-1, 0 tier-2, 0 tier-3 tests affected"
    );
    assert!(
        restricted_tests
            .to_string()
            .contains("visiblefederationtest"),
        "authorized test must remain selected: {restricted_tests_body}"
    );
    assert!(
        !restricted_tests_body.to_string().contains("hiddenlocal"),
        "restricted affected_tests leaked a hidden local row, path, or count marker: \
         {restricted_tests_body}"
    );
    assert!(
        restricted_tests["measured"].is_null(),
        "restricted affected_tests must not expose an unscoped measured aggregate: \
         {restricted_tests_body}"
    );
    assert_eq!(
        restricted_tests_body["result"]["structuredContent"]["org_wide_impact"],
        json!({
            "status": "withheld",
            "reason": "authorization-unproven"
        })
    );

    let base_ref_arguments = json!({ "base_ref": "HEAD" });
    let admin_base_ref_response = client
        .post(&endpoint)
        .bearer_auth(admin_token)
        .json(&request(67, "affected_tests", base_ref_arguments.clone()))
        .send()
        .await
        .expect("admin affected_tests base_ref request");
    assert_eq!(admin_base_ref_response.status(), 200);
    let admin_base_ref_body: Value = admin_base_ref_response
        .json()
        .await
        .expect("admin affected_tests base_ref JSON");
    assert!(
        admin_base_ref_body["result"]["structuredContent"]["local_impact"]["changed_files"]
            .to_string()
            .contains("secret-local-path.test.js"),
        "fixture must prove unrestricted base_ref selected the first, hidden local repo: \
         {admin_base_ref_body}"
    );

    let restricted_base_ref_response = client
        .post(&endpoint)
        .bearer_auth(query_token)
        .json(&request(68, "affected_tests", base_ref_arguments))
        .send()
        .await
        .expect("restricted affected_tests base_ref request");
    assert_eq!(restricted_base_ref_response.status(), 200);
    let restricted_base_ref_body: Value = restricted_base_ref_response
        .json()
        .await
        .expect("restricted affected_tests base_ref JSON");
    assert_eq!(
        restricted_base_ref_body["result"]["structuredContent"]["local_impact"]["changed_files"],
        json!([]),
        "restricted base_ref must diff the sole visible local repository: \
         {restricted_base_ref_body}"
    );
    assert!(
        !restricted_base_ref_body
            .to_string()
            .contains("secret-local-path.test.js"),
        "restricted base_ref leaked a changed path from the hidden local repository: \
         {restricted_base_ref_body}"
    );
}

/// A daemon with NO upstream configured is ONE node: its `/mcp` boundary must
/// keep the honest single-node stamp (`sources=["daemon"]`, `scope=single-node`)
/// AND now also carry `stale_repos` as an (empty) array — the staleness key is
/// stamped uniformly on every result, and its addition must not regress the
/// existing single-node provenance contract.
#[tokio::test]
async fn daemon_mcp_boundary_single_node_without_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo_dir = dir.path().join("repo");
    write_repo_files(
        &repo_dir,
        &[("local/main.js", "function solofn(x) { return x; }")],
    );
    index_repo(&repo_dir, &db_path);

    let guard = helpers::server_guard::ServerGuard::start(&db_path);
    let mcp_addr = guard.mcp_addr();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{mcp_addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "blast_radius",
                "arguments": { "changed_files": ["local/main.js"], "max_depth": 3 }
            }
        }))
        .send()
        .await
        .expect("MCP HTTP tools/call request failed");

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["result"]["isError"], false, "got: {body}");

    let meta = &body["result"]["_meta"];
    assert_eq!(
        meta["nestweaver.io/sources"],
        json!(["daemon"]),
        "single-node daemon must report only itself as a source; got {meta}"
    );
    assert_eq!(
        meta["nestweaver.io/scope"], "single-node",
        "no upstream configured => scope must stay single-node; got {meta}"
    );
    assert_eq!(
        meta["nestweaver.io/stale_repos"],
        json!([]),
        "stale_repos must be an empty array when no upstream is configured; got {meta}"
    );
}
