use assert_cmd::Command;
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
fn cli_snapshot_build_and_verify() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    let snapshot_dir = dir.path().join("snapshot-out");
    std::fs::create_dir_all(&repo_dir).unwrap();

    // Create a minimal git repo so index produces a valid database
    StdCommand::new("git")
        .args(["init"])
        .current_dir(&repo_dir)
        .output()
        .expect("git init failed");
    StdCommand::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    StdCommand::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    std::fs::write(
        repo_dir.join("main.js"),
        "function greet(name) { return name; }",
    )
    .unwrap();
    StdCommand::new("git")
        .args(["add", "."])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    StdCommand::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&repo_dir)
        .output()
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

    // Build snapshot
    nestweaver_cmd()
        .args([
            "snapshot",
            "build",
            "--db",
            &db_path.display().to_string(),
            "--output",
            &snapshot_dir.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("Snapshot built successfully"));

    // Verify expected files exist
    assert!(snapshot_dir.join("graph.lbug").exists());
    assert!(snapshot_dir.join("stamp.json").exists());
    assert!(snapshot_dir.join("manifest.json").exists());
    assert!(snapshot_dir.join("checksum.blake3").exists());

    // Verify integrity via CLI
    nestweaver_cmd()
        .args(["snapshot", "verify", &snapshot_dir.display().to_string()])
        .assert()
        .success()
        .stdout(contains("Snapshot verified OK"));
}

#[test]
fn cli_snapshot_push_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let snap_dir = dir.path().join("snapshot");
    let storage_dir = dir.path().join("storage");
    std::fs::create_dir_all(&snap_dir).unwrap();
    std::fs::create_dir_all(&storage_dir).unwrap();

    // Minimal valid snapshot files
    let graph_bytes = b"fake-graph-data";
    let manifest_bytes = b"{\"repos\":[]}";
    let stamp_bytes = br#"{
        "instance_id": "push-test",
        "engine_version": "0.1.0",
        "min_compatible_engine": "0.1.0",
        "schema_hash_core": "c",
        "schema_hash_extensions": "e",
        "schema_hash_effective": "eff",
        "embedding_model_id": "model",
        "embedding_dimension": 0,
        "built_at": "2026-06-01T00:00:00Z",
        "repos": []
    }"#;

    std::fs::write(snap_dir.join("graph.lbug"), graph_bytes).unwrap();
    std::fs::write(snap_dir.join("manifest.json"), manifest_bytes).unwrap();
    std::fs::write(snap_dir.join("stamp.json"), stamp_bytes).unwrap();

    // Compute per-file checksums (blake3 format)
    let checksums = [
        ("graph.lbug", graph_bytes.as_slice()),
        ("manifest.json", manifest_bytes.as_slice()),
        ("stamp.json", stamp_bytes.as_slice()),
    ]
    .iter()
    .map(|(name, data)| format!("{}  {name}", blake3::hash(data).to_hex()))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    std::fs::write(snap_dir.join("checksum.blake3"), &checksums).unwrap();

    nestweaver_cmd()
        .args([
            "snapshot",
            "push",
            "--snapshot-dir",
            &snap_dir.display().to_string(),
            "--backend",
            "local",
            "--backend-path",
            &storage_dir.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("Snapshot pushed"));

    // A versioned directory v0.1.0 should exist in the storage dir
    assert!(
        storage_dir.join("v0.1.0").exists(),
        "expected versioned snapshot directory v0.1.0 in storage"
    );
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

#[test]
fn setup_does_not_overwrite_existing_skill_file() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".claude/skills/nestweaver");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_path, "# My custom skill content\n").unwrap();

    let db_path = dir.path().join("test.lbug");
    std::fs::write(&db_path, "").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["setup", "--db", db_path.to_str().unwrap(), "--all"])
        .current_dir(dir.path())
        .env("NESTWEAVER_NO_DAEMON", "1")
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("already exists"),
        "should report skill file already exists, got: {stdout}"
    );

    let content = std::fs::read_to_string(&skill_path).unwrap();
    assert_eq!(
        content, "# My custom skill content\n",
        "existing skill file must not be overwritten"
    );
}

#[test]
fn setup_force_overwrites_existing_skill_file() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".claude/skills/nestweaver");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_path, "# My custom skill content\n").unwrap();

    let db_path = dir.path().join("test.lbug");
    std::fs::write(&db_path, "").unwrap();

    std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args([
            "setup",
            "--db",
            db_path.to_str().unwrap(),
            "--all",
            "--force",
        ])
        .current_dir(dir.path())
        .env("NESTWEAVER_NO_DAEMON", "1")
        .output()
        .unwrap();

    let content = std::fs::read_to_string(&skill_path).unwrap();
    assert_ne!(
        content, "# My custom skill content\n",
        "with --force, skill file should be regenerated"
    );
}

#[test]
fn setup_does_not_overwrite_existing_cursor_rule() {
    let dir = tempfile::tempdir().unwrap();
    let rule_dir = dir.path().join(".cursor/rules");
    std::fs::create_dir_all(&rule_dir).unwrap();
    let rule_path = rule_dir.join("nestweaver.mdc");
    std::fs::write(&rule_path, "custom cursor rule content").unwrap();

    let db_path = dir.path().join("test.lbug");
    std::fs::write(&db_path, "").unwrap();

    std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["setup", "--db", db_path.to_str().unwrap(), "--all"])
        .current_dir(dir.path())
        .env("NESTWEAVER_NO_DAEMON", "1")
        .output()
        .unwrap();

    let content = std::fs::read_to_string(&rule_path).unwrap();
    assert_eq!(
        content, "custom cursor rule content",
        "cursor rule must not be overwritten"
    );
}

#[test]
fn setup_strips_deprecated_args_from_existing_config() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    std::fs::write(&db_path, "").unwrap();

    let mcp_path = dir.path().join(".mcp.json");
    std::fs::write(
        &mcp_path,
        serde_json::json!({
            "mcpServers": {
                "nestweaver": {
                    "command": "nestweaver",
                    "args": ["mcp", "--db", "test.lbug", "--allow-mcp-add-sources"]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["setup", "--db", db_path.to_str().unwrap(), "--all"])
        .current_dir(dir.path())
        .env("NESTWEAVER_NO_DAEMON", "1")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stripped deprecated"),
        "should report stripping deprecated flag on stderr, got: {stderr}"
    );

    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
    let args = config["mcpServers"]["nestweaver"]["args"]
        .as_array()
        .unwrap();
    assert!(
        !args
            .iter()
            .any(|a| a.as_str() == Some("--allow-mcp-add-sources")),
        "deprecated flag should be removed from config, got: {args:?}"
    );
}

#[test]
fn daemon_status_accepts_db_after_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    std::fs::write(&db_path, "").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["daemon", "status", "--db", db_path.to_str().unwrap()])
        .env("NESTWEAVER_NO_DAEMON", "1")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unexpected argument"),
        "daemon status should accept --db after subcommand, got: {stderr}"
    );
}

#[test]
fn cli_snapshot_stamp_has_repos_and_correct_embedding_model() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    let snapshot_dir = dir.path().join("snapshot-out");
    std::fs::create_dir_all(&repo_dir).unwrap();

    // Create a minimal git repo
    StdCommand::new("git")
        .args(["init"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    StdCommand::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    StdCommand::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    std::fs::write(
        repo_dir.join("main.js"),
        "function greet(name) { return name; }",
    )
    .unwrap();
    StdCommand::new("git")
        .args(["add", "."])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    StdCommand::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&repo_dir)
        .output()
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

    // Write a config file with a known embedding model_id
    let config_path = dir.path().join("instance.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "test-instance"
repos = []

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

[embedding]
model_id = "sentence-transformers/all-MiniLM-L6-v2"
"#,
            storage = dir.path().join("storage").display(),
            workspace = dir.path().join("workspace").display(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("storage")).unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();

    // Build snapshot with --config so embedding_model_id is populated
    nestweaver_cmd()
        .args([
            "snapshot",
            "build",
            "--db",
            &db_path.display().to_string(),
            "--config",
            &config_path.display().to_string(),
            "--output",
            &snapshot_dir.display().to_string(),
        ])
        .assert()
        .success();

    // Parse stamp.json and verify
    let stamp_json =
        std::fs::read_to_string(snapshot_dir.join("stamp.json")).expect("stamp.json should exist");
    let stamp: serde_json::Value =
        serde_json::from_str(&stamp_json).expect("stamp.json should be valid JSON");

    // Bug 1a: repos should NOT be empty — we indexed one repo
    let repos = stamp["repos"].as_array().expect("repos should be an array");
    assert!(
        !repos.is_empty(),
        "stamp.json repos should not be empty after indexing a repo"
    );

    // The repo URL should match the repo we indexed
    let repo_url = repos[0]["url"].as_str().unwrap();
    assert!(
        repo_url.contains("repo"),
        "repo URL '{repo_url}' should reference the indexed repo"
    );

    // Bug 1b: embedding_model_id should come from [embedding], not [inference]
    let model_id = stamp["embedding_model_id"]
        .as_str()
        .expect("embedding_model_id should be a string");
    assert_eq!(
        model_id, "sentence-transformers/all-MiniLM-L6-v2",
        "embedding_model_id should come from [embedding].model_id, not [inference].embedding_model"
    );
    assert_ne!(
        model_id, "nomic-embed-text",
        "embedding_model_id should NOT be the inference model"
    );
}

#[test]
fn cli_daemon_run_server_help() {
    nestweaver_cmd()
        .args(["daemon", "run", "--help"])
        .assert()
        .success()
        .stdout(contains("--server"))
        .stdout(contains("--bind"))
        .stdout(contains("--tls-cert"))
        .stdout(contains("--tls-key"))
        .stdout(contains("--auth-token"))
        .stdout(contains("--admin-token"))
        .stdout(contains("--port-file"));
}
