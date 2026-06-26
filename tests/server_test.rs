//! Integration tests for NestWeaver server mode.
//!
//! Run with:
//!   cargo test --test server_test -- --test-threads=1

mod helpers;

use std::process::Command as StdCommand;

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
