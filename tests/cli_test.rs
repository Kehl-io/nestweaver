use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::process::Command as StdCommand;

fn nestweaver_cmd() -> Command {
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    cmd.env("NESTWEAVER_NO_DAEMON", "1");
    cmd
}

#[test]
fn cli_ui_shows_help() {
    nestweaver_cmd()
        .args(["ui", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--port"));
}

#[test]
fn cli_shows_version() {
    // Assert against the actual package version so this test survives release
    // bumps (release-please advances the workspace version on `main`).
    nestweaver_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn cli_help_lists_commands() {
    nestweaver_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("symbol"))
        .stdout(contains("search"))
        .stdout(contains("impact"));
}

#[test]
fn cli_index_and_search() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("main.js"),
        "function greet(name) { return name; }",
    )
    .unwrap();

    // Index
    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    // Search
    nestweaver_cmd()
        .args([
            "search",
            "greet",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("greet"));
}

#[test]
fn cli_symbol_not_found_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    // Create empty db via index of empty dir
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    nestweaver_cmd()
        .args([
            "symbol",
            "nonexistent",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .code(2);
}

#[test]
fn cli_index_then_symbol_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("app.js"),
        "function myFunc(x) { return x + 1; }",
    )
    .unwrap();

    // Index
    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    // Symbol lookup returns success and prints name
    nestweaver_cmd()
        .args([
            "symbol",
            "myFunc",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("myFunc"));
}

#[test]
fn cli_instance_list_empty() {
    // instance list should succeed even when no registry file exists yet.
    // We can't easily override the config dir in a subprocess, but we can at
    // minimum confirm the command exits successfully (it just reads or creates
    // the registry file in the default location).
    nestweaver_cmd()
        .args(["instance", "list"])
        .assert()
        .success();
}

#[test]
fn cli_snapshot_verify_nonexistent() {
    nestweaver_cmd()
        .args(["snapshot", "verify", "/nonexistent/path/to/snapshot"])
        .assert()
        .failure();
}

#[test]
fn cli_snapshot_build_exits_error() {
    nestweaver_cmd()
        .args(["snapshot", "build"])
        .assert()
        .failure()
        .stderr(contains("not yet implemented").or(contains("instance-aware")));
}

#[test]
fn cli_snapshot_push_exits_error() {
    nestweaver_cmd()
        .args(["snapshot", "push"])
        .assert()
        .failure()
        .stderr(contains("Not yet implemented"));
}

#[test]
fn cli_service_summary_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();

    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    nestweaver_cmd()
        .args([
            "service-summary",
            "nonexistent-service",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .code(2);
}

#[test]
fn cli_cross_repo_refs_symbol_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();

    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    nestweaver_cmd()
        .args([
            "cross-repo-refs",
            "nosuchsymbol",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .code(2);
}

#[test]
fn cli_cross_repo_refs_empty_for_known_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("app.js"),
        "function myFunc(x) { return x + 1; }",
    )
    .unwrap();

    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    // myFunc is indexed but has no cross-repo links → success + "No cross-repo references"
    nestweaver_cmd()
        .args([
            "cross-repo-refs",
            "myFunc",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("No cross-repo references"));
}

#[test]
fn e2e_index_and_query_js_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(repo_dir.join("src")).unwrap();

    // Multi-file JS repo
    std::fs::write(
        repo_dir.join("src/main.js"),
        r#"
const { greet } = require('./helper');

function main() {
    greet("world");
}

class App {
    run() { main(); }
}
    "#,
    )
    .unwrap();

    std::fs::write(
        repo_dir.join("src/helper.js"),
        r#"
function greet(name) {
    return formatGreeting(name);
}

function formatGreeting(name) {
    return "Hello, " + name + "!";
}

module.exports = { greet };
    "#,
    )
    .unwrap();

    // Index
    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    // Search finds symbols
    nestweaver_cmd()
        .args([
            "search",
            "greet",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("greet"));

    // Symbol lookup works
    nestweaver_cmd()
        .args([
            "symbol",
            "App",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("App"));

    // Symbol not found gives exit code 2
    nestweaver_cmd()
        .args([
            "symbol",
            "nonexistent_xyz",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .code(2);

    // Impact analysis works
    nestweaver_cmd()
        .args([
            "impact",
            "greet",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    // Repo-map works
    nestweaver_cmd()
        .args(["repo-map", "--json", "--db", &db_path.display().to_string()])
        .assert()
        .success();

    // List-repos works
    nestweaver_cmd()
        .args([
            "list-repos",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("repo"));
}

// ── context command tests ─────────────────────────────────────────────────────

/// Helper: index a small JS repo with two files and a call relationship.
fn setup_context_db(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("main.js"),
        "function greet(name) { return hello(name); }\nfunction hello(name) { return name; }",
    )
    .unwrap();
    std::fs::write(
        repo_dir.join("utils.js"),
        "function formatDate(date) { return date; }",
    )
    .unwrap();

    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    db_path
}

#[test]
fn cli_context_basic() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = setup_context_db(&dir);

    let output = nestweaver_cmd()
        .args(["context", "greet", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    assert!(
        stdout.contains("greet"),
        "output should mention seed 'greet'; got:\n{stdout}"
    );
    assert!(
        stdout.contains("Seeds"),
        "output should have a Seeds section; got:\n{stdout}"
    );
}

#[test]
fn cli_context_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = setup_context_db(&dir);

    nestweaver_cmd()
        .args([
            "context",
            "zzz_no_such_symbol_xyz",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .code(2);
}

#[test]
fn cli_context_json() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = setup_context_db(&dir);

    let output = nestweaver_cmd()
        .args([
            "context",
            "greet",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value =
        serde_json::from_slice(&output).expect("output should be valid JSON");

    assert!(
        parsed.get("seeds").is_some(),
        "JSON should have 'seeds' field"
    );
    assert!(
        parsed.get("connected").is_some(),
        "JSON should have 'connected' field"
    );
    assert!(
        parsed.get("cross_repo_links").is_some(),
        "JSON should have 'cross_repo_links' field"
    );
}

#[test]
fn cli_impact_on_empty_db_exits_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("empty.lbug");
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();

    // Create an empty DB
    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    nestweaver_cmd()
        .args([
            "impact",
            "noSuchSymbol",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .code(2);
}

#[test]
fn cli_incremental_index_picks_up_new_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();

    // Initialise a git repo in the temp directory.
    let git = |args: &[&str]| {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(&repo_dir)
            .status()
            .expect("git command failed to spawn");
        assert!(status.success(), "git {:?} failed with {:?}", args, status);
    };

    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);

    // First commit: a.js with `hello`.
    std::fs::write(repo_dir.join("a.js"), "function hello() {}").unwrap();
    git(&["add", "a.js"]);
    git(&["commit", "-m", "initial"]);

    // First index run — no prior DB, so this is a full index.
    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    // Second commit: add b.js with `world`.
    std::fs::write(repo_dir.join("b.js"), "function world() {}").unwrap();
    git(&["add", "b.js"]);
    git(&["commit", "-m", "add world"]);

    // Second index run — should be incremental.
    let output = nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo_dir.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();

    let stderr = String::from_utf8_lossy(&output);
    assert!(
        stderr.contains("added") || stderr.contains("Incremental"),
        "expected incremental stats in stderr; got:\n{stderr}"
    );

    // The new symbol `world` should now be searchable.
    nestweaver_cmd()
        .args(["search", "world", "--db", &db_path.display().to_string()])
        .assert()
        .success()
        .stdout(contains("world"));
}
