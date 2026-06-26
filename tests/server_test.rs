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
fn server_test_helpers_compile() {
    // Placeholder — verifies test infrastructure compiles.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    write_test_repo(&repo);
    assert!(repo.join("main.js").exists());

    // Verify the ServerGuard type is accessible (compile-time check).
    let _ty: fn(&std::path::Path) -> helpers::server_guard::ServerGuard =
        helpers::server_guard::ServerGuard::start;
}
