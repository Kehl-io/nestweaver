//! CI integration tests for NestWeaver impact analysis pipeline.
//!
//! These tests exercise the end-to-end flow of indexing a repo, computing
//! impact from local changes, and verifying JSON/markdown output — the same
//! steps a CI pipeline would run.
//!
//! Run with:
//!   cargo test --test ci_integration_test -- --test-threads=1

use std::process::Command as StdCommand;

/// Create a minimal git repo with a JS function, committed.
fn init_git_repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    StdCommand::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    StdCommand::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .unwrap();
    StdCommand::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();
}

/// Run a git command in the given directory, panicking on failure.
fn git(dir: &std::path::Path, args: &[&str]) {
    let output = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git command failed to spawn");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Build a nestweaver command with daemon suppressed.
fn nestweaver() -> StdCommand {
    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"));
    cmd.env("NESTWEAVER_NO_DAEMON", "1");
    cmd
}

/// Index a repo into a database, asserting success.
fn index_repo(repo_dir: &std::path::Path, db_path: &std::path::Path) {
    let output = nestweaver()
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
}

// ── Test 1: impact diff produces valid JSON ─────────────────────────────

#[test]
fn impact_diff_produces_json() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");

    // Set up a git repo with an initial commit containing a function.
    init_git_repo(&repo_dir);
    std::fs::write(
        repo_dir.join("lib.js"),
        "function processOrder(orderId) { return orderId; }\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "."]);
    git(
        &repo_dir,
        &[
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "initial: add processOrder",
        ],
    );

    // Index the repo at its initial state.
    index_repo(&repo_dir, &db_path);

    // Second commit: change the function signature (breaking change).
    std::fs::write(
        repo_dir.join("lib.js"),
        "function processOrder(orderId, options) { return orderId; }\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "."]);
    git(
        &repo_dir,
        &[
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "change processOrder signature",
        ],
    );

    // Run pre-push-impact using --diff against the previous commit.
    let output = nestweaver()
        .args([
            "pre-push-impact",
            "--diff",
            "HEAD~1..HEAD",
            "--format",
            "json",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "pre-push-impact failed (exit {}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // stdout must be a SINGLE clean JSON document — no leading status text.
    // Regression guard for CI JSON pollution: in --format json, all status and
    // progress messages must go to stderr so jq / format-comment can parse stdout.
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{'),
        "stdout must start with JSON (status text must go to stderr), got: {stdout:?}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("entire stdout should be valid JSON");

    // Verify expected top-level structure.
    assert!(
        parsed.get("changes").is_some(),
        "JSON should have 'changes' field, got: {parsed}"
    );
    assert!(
        parsed.get("impacts").is_some(),
        "JSON should have 'impacts' field, got: {parsed}"
    );
}

// ── Test 2: format-comment produces markdown ────────────────────────────

#[test]
fn format_comment_produces_markdown() {
    let dir = tempfile::tempdir().unwrap();
    let input_path = dir.path().join("impact.json");

    // Write a sample ImpactReport JSON matching the engine struct.
    let impact_data = serde_json::json!({
        "changes": 1,
        "impacts": [
            {
                "change_canonical_id": "sym:repo:lib.js:processOrder:1",
                "change_kind": "SignatureChanged",
                "affected_canonical_id": "sym:repo:app.js:handleCheckout:5",
                "affected_name": "handleCheckout",
                "affected_repo_url": "https://github.com/acme/frontend.git",
                "affected_file": "app.js",
                "affected_line": 5,
                "affected_signature": "function handleCheckout(order)",
                "severity": "Breaking",
                "reason": "Calls processOrder which changed signature"
            }
        ]
    });

    std::fs::write(
        &input_path,
        serde_json::to_string_pretty(&impact_data).unwrap(),
    )
    .unwrap();

    let output = nestweaver()
        .args([
            "format-comment",
            "--input",
            &input_path.display().to_string(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "format-comment failed (exit {}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("<!-- nestweaver-impact -->"),
        "markdown output should contain hidden marker, got: {stdout}"
    );
}

// ── Test 4: --fail-on-breaking exits nonzero when breaking change detected ─

#[test]
fn fail_on_breaking_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");

    // Set up a git repo with a function and a caller of that function,
    // so the store records a CALLS edge that makes removal Breaking.
    init_git_repo(&repo_dir);
    std::fs::write(
        repo_dir.join("lib.js"),
        "function processOrder(orderId) { return orderId; }\n",
    )
    .unwrap();
    std::fs::write(
        repo_dir.join("app.js"),
        "function handleCheckout(order) { return processOrder(order.id); }\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "."]);
    git(
        &repo_dir,
        &[
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "initial: add processOrder and handleCheckout",
        ],
    );

    // Index the repo so the CALLS edge processOrder <- handleCheckout is stored.
    index_repo(&repo_dir, &db_path);

    // Second commit: remove processOrder — this is a Breaking change for handleCheckout.
    std::fs::write(repo_dir.join("lib.js"), "// processOrder removed\n").unwrap();
    git(&repo_dir, &["add", "."]);
    git(
        &repo_dir,
        &[
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "remove processOrder",
        ],
    );

    // Run pre-push-impact with --fail-on-breaking. The removal of processOrder
    // should be detected as Breaking because handleCheckout calls it.
    let output = nestweaver()
        .args([
            "pre-push-impact",
            "--diff",
            "HEAD~1..HEAD",
            "--fail-on-breaking",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "pre-push-impact --fail-on-breaking should exit nonzero when breaking change detected \
         (exit {}): stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── Test 5: --fail-on-error exits nonzero when server is unreachable ───────

#[test]
fn fail_on_error_exits_nonzero_when_server_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");

    // Minimal repo with a local change so pre-push-impact has something to analyze.
    init_git_repo(&repo_dir);
    std::fs::write(
        repo_dir.join("lib.js"),
        "function hello() { return 1; }\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "."]);
    git(
        &repo_dir,
        &[
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "initial",
        ],
    );

    // Create a local (uncommitted) change so --local-changes detects something.
    std::fs::write(
        repo_dir.join("lib.js"),
        "function hello(name) { return name; }\n",
    )
    .unwrap();

    // Point --server at an address where nothing is listening. With --fail-on-error
    // the command must exit nonzero instead of silently degrading.
    let output = nestweaver()
        .args([
            "pre-push-impact",
            "--local-changes",
            "--server",
            "http://127.0.0.1:1",
            "--fail-on-error",
            "--repo",
            &repo_dir.display().to_string(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "pre-push-impact --fail-on-error should exit nonzero when server is unreachable \
         (exit {}): stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── Test 3: graceful degradation when server is unreachable ─────────────

#[test]
fn impact_server_down_graceful() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");

    // Set up a git repo with uncommitted changes so pre-push-impact has
    // something to analyze.
    init_git_repo(&repo_dir);
    std::fs::write(repo_dir.join("lib.js"), "function hello() { return 1; }\n").unwrap();
    git(&repo_dir, &["add", "."]);
    git(
        &repo_dir,
        &[
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "initial",
        ],
    );

    // Index the repo.
    index_repo(&repo_dir, &db_path);

    // Create a second commit so --diff has something to compare.
    std::fs::write(
        repo_dir.join("lib.js"),
        "function hello(name) { return name; }\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "."]);
    git(
        &repo_dir,
        &[
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "change hello",
        ],
    );

    // Point --server at localhost:1 which should be unreachable. Without
    // --fail-on-error the command should exit 0 gracefully.
    let output = nestweaver()
        .args([
            "pre-push-impact",
            "--diff",
            "HEAD~1..HEAD",
            "--format",
            "json",
            "--server",
            "grpc://localhost:1",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "pre-push-impact should exit 0 when server is unreachable (exit {}): stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // When server is down and format is json, stdout must still be a clean JSON
    // document (status/warnings go to stderr) with empty impacts and an error.
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{'),
        "stdout must be clean JSON even on server failure, got: {stdout:?}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("entire stdout should be valid JSON");
    let impacts = parsed
        .get("impacts")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        impacts, 0,
        "impacts should be empty when server is unreachable"
    );
    assert!(
        parsed.get("error").is_some(),
        "JSON output should include an 'error' field when server is down, got: {parsed}"
    );
}
