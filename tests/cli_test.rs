use assert_cmd::Command;
use assert_cmd::assert::OutputAssertExt;
use nestweaver_engine::sidecar_path;
use predicates::str::contains;
use std::process::Command as StdCommand;

fn nestweaver_cmd() -> Command {
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    cmd.env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1");
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
fn config_validate_accepts_minimal_fixture() {
    let config_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/minimal-instance.toml");
    let output = nestweaver_cmd()
        .args(["config", "validate"])
        .arg(&config_path)
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["valid"], true);
    assert_eq!(result["path"], config_path.display().to_string());
    assert_eq!(result["instance_id"], "minimal-example");
    assert_eq!(result["repo_count"], 0);
}

#[test]
fn config_validate_rejects_obsolete_instance_table() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("obsolete-instance.toml");
    std::fs::write(
        &config_path,
        r#"
[instance]
name = "obsolete-example"

[snapshot_storage]
backend = "local"
path = "/tmp/nestweaver/snapshots"

[workspace]
backend = "local"
path = "/tmp/nestweaver/workspace"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

[[repos]]
path = "/tmp/nestweaver/repo"
"#,
    )
    .unwrap();

    let output = nestweaver_cmd()
        .args(["config", "validate"])
        .arg(&config_path)
        .arg("--json")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(result["valid"], false);
    assert_eq!(result["path"], config_path.display().to_string());
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("[instance]"), "error: {error}");
    assert!(error.contains("instance_id"), "error: {error}");
    assert!(error.contains("[[repos]]"), "error: {error}");
    assert!(error.contains("url"), "error: {error}");
    assert!(error.contains("path"), "error: {error}");
}

#[test]
fn config_validate_has_no_filesystem_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("instance.toml");
    let db_path = dir.path().join("graph.lbug");
    let snapshot_path = dir.path().join("snapshots");
    let workspace_path = dir.path().join("workspace");
    let missing_repo = dir.path().join("missing-repo");
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "side-effect-check"
db = "{}"

[snapshot_storage]
backend = "local"
path = "{}"

[workspace]
backend = "local"
path = "{}"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

[[repos]]
url = "{}"
"#,
            toml_basic_string(&db_path),
            toml_basic_string(&snapshot_path),
            toml_basic_string(&workspace_path),
            toml_basic_string(&missing_repo),
        ),
    )
    .unwrap();

    let output = nestweaver_cmd()
        .current_dir(dir.path())
        .args(["config", "validate"])
        .arg(&config_path)
        .arg("--json")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut entries = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    assert_eq!(entries, vec![std::ffi::OsString::from("instance.toml")]);
}

#[test]
fn installation_docs_only_claim_live_channels() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut docs = vec![
        "INSTALL.md",
        "README.md",
        "docs/guide/instance-config.md",
        "docs/server-mode.md",
        "docs/architecture/project-brain.md",
        "CLAUDE.md",
        "npm/README.md",
        "npm/install.js",
        "npm/bin/nestweaver",
    ];
    if repo_root.join("smithery.yaml").exists() {
        docs.push("smithery.yaml");
    }
    let unsupported_commands = [
        "npm install -g @kehl-io/nestweaver",
        "npm install @kehl-io/nestweaver",
        "cargo install nestweaver",
        "brew install nestweaver",
        "npx @kehl-io/nestweaver",
        "npm exec @kehl-io/nestweaver",
    ];

    for relative_path in docs {
        let contents = std::fs::read_to_string(repo_root.join(relative_path))
            .unwrap_or_else(|error| panic!("failed to read {relative_path}: {error}"));
        for command in unsupported_commands {
            assert!(
                !contents.contains(command),
                "{relative_path} advertises unavailable installation command `{command}`"
            );
        }
    }
}

#[test]
fn standalone_suggest_links_reads_canonical_manifest_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("brain.lbug");
    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    for (uid, url) in [
        ("repo:test:app", "https://example.test/app"),
        ("repo:test:dependency", "https://example.test/dependency"),
    ] {
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: uid.to_string(),
                url: url.to_string(),
                indexed_sha: "sha".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
    }
    let manifests = std::collections::HashMap::from([
        (
            "repo:test:app".to_string(),
            nestweaver_engine::ManifestInfo {
                package_name: Some("app-package".to_string()),
                dependencies: vec!["dependency-package".to_string()],
                entry_files: Vec::new(),
            },
        ),
        (
            "repo:test:dependency".to_string(),
            nestweaver_engine::ManifestInfo {
                package_name: Some("dependency-package".to_string()),
                dependencies: Vec::new(),
                entry_files: Vec::new(),
            },
        ),
    ]);
    nestweaver_engine::save_manifest_cache_for_db(&manifests, &db_path).unwrap();
    drop(store);

    nestweaver_cmd()
        .args([
            "suggest-links",
            "--json",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("Depends on dependency-package (from manifest)"));
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
fn brain_search_json_count_contract_is_limit_independent() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let db_path = dir.path().join("brain.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&vault_dir).unwrap();
    std::fs::write(
        repo_dir.join("main.js"),
        "function cardinalityneedle() {}\n\
         function cardinalityneedleExtra() {}\n\
         function helper_cardinalityneedle() {}\n",
    )
    .unwrap();
    for (name, title) in [
        ("one.md", "cardinalityneedle"),
        ("two.md", "cardinalityneedle alpha"),
        ("three.md", "cardinalityneedle beta"),
    ] {
        std::fs::write(vault_dir.join(name), format!("# {title}\n\nbody\n")).unwrap();
    }

    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo_dir)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();
    nestweaver_cmd()
        .args(["brain", "add"])
        .arg(&vault_dir)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    let search = |limit: &str| {
        let output = nestweaver_cmd()
            .args([
                "brain",
                "search",
                "cardinalityneedle",
                "--json",
                "--limit",
                limit,
                "--db",
            ])
            .arg(&db_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "brain search --limit {limit} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "brain search --limit {limit} returned invalid JSON ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    };

    let narrow = search("1");
    let broad = search("20");

    nestweaver_cmd()
        .args([
            "brain",
            "search",
            "cardinalityneedle",
            "--limit",
            "1",
            "--db",
        ])
        .arg(&db_path)
        .assert()
        .success()
        .stdout(contains(" of "));

    assert_eq!(narrow["total_matches"], broad["total_matches"]);
    assert_eq!(
        narrow["total_matches_relation"],
        broad["total_matches_relation"]
    );
    assert_eq!(narrow["total_matches_relation"], "eq");
    assert_ne!(narrow["returned_matches"], broad["returned_matches"]);
    assert_eq!(
        narrow["returned_matches"].as_u64().unwrap(),
        narrow["results"].as_array().unwrap().len() as u64
    );
    assert_eq!(narrow["truncated"], true);
    assert_eq!(broad["truncated"], false);

    let broad_rows = broad["results"].as_array().unwrap();
    assert!(
        broad_rows
            .iter()
            .any(|row| row["kind"] == "note" && row["title"] == "cardinalityneedle")
    );
    assert!(
        broad_rows
            .iter()
            .any(|row| { row["kind"] == "Symbol/Function" && row["title"] == "cardinalityneedle" }),
        "same-title note and symbol must remain distinct rows: {broad}"
    );
    assert!(
        broad_rows
            .iter()
            .filter(|row| {
                row["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("Symbol/"))
            })
            .all(|row| {
                row["canonical_id"]
                    .as_str()
                    .is_some_and(|canonical_id| !canonical_id.is_empty())
            }),
        "CLI JSON must preserve canonical symbol IDs: {broad}"
    );
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

    // nw-052b residual: a colon in the snapshot `--instance` flag must be
    // rejected (it would otherwise land in the stamp label + output-dir name).
    nestweaver_cmd()
        .args([
            "snapshot",
            "build",
            "--db",
            &db_path.display().to_string(),
            "--output",
            &dir.path().join("snap-colon").display().to_string(),
            "--instance",
            "a:b",
        ])
        .assert()
        .failure()
        .stderr(contains("colon"));
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
fn cli_impact_json_is_array_and_notfound_is_object() {
    // nw-086: `impact --json` must emit a bare node ARRAY on success and a JSON
    // error OBJECT on not-found (never a plain-text-only stderr line), so a
    // --json consumer can always parse the output.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.lbug");
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("m.js"),
        "function a(){ return b(); }\nfunction b(){ return 1; }\n",
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

    // Found → bare array (a calls b, so impact(b) is non-empty).
    let out = nestweaver_cmd()
        .args([
            "impact",
            "b",
            "--db",
            &db_path.display().to_string(),
            "--json",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("impact --json (found) must be valid JSON");
    assert!(
        v.is_array(),
        "impact --json (found) must be a bare array, got: {v}"
    );

    // Not-found → JSON object with an `error` field, exit 2.
    let out = nestweaver_cmd()
        .args([
            "impact",
            "zzz_no_symbol",
            "--db",
            &db_path.display().to_string(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "not-found exits 2");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("impact --json (not-found) must be valid JSON");
    assert!(
        v.get("error").is_some(),
        "not-found --json must carry an error field, got: {v}"
    );
}

#[test]
fn cli_read_on_missing_db_daemon_path_does_not_create_it() {
    // nw-087: a query against a NONEXISTENT db (parent dir exists) on the DAEMON
    // path must fail loudly — not autostart a daemon that materializes an empty
    // store, which would read as a false-green "0 results / complete" success.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("typo_never_created.lbug");
    // Raw command WITHOUT the allow-hatch and WITHOUT any CI marker → routes
    // through the daemon path (the one that used to create the store).
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    cmd.env_remove("NESTWEAVER_ALLOW_NO_DAEMON")
        .env_remove("NESTWEAVER_NO_DAEMON")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .args([
            "impact",
            "anySymbol",
            "--db",
            &db_path.display().to_string(),
        ]);
    cmd.assert().failure();
    assert!(
        !db_path.exists(),
        "a read against a missing db must not create it (nw-087)"
    );
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
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

#[test]
fn cli_connect_help() {
    nestweaver_cmd()
        .args(["connect", "--help"])
        .assert()
        .success()
        .stdout(contains("--token"))
        .stdout(contains("--mode"))
        .stdout(contains("--name"));
}

/// nw-010: a repo with a configured git origin remote indexes under the
/// origin identity (`url` = remote URL, read from git config — no network)
/// with its working tree recorded in `root_path`; a prior `file://` row for
/// the same path is pruned by uid on re-index. A repo WITHOUT an origin
/// keeps its `file://` identity, also with `root_path` set.
#[test]
fn cli_index_reidentifies_repo_under_origin_remote() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test"],
    ] {
        StdCommand::new("git")
            .args(&args)
            .current_dir(&repo_dir)
            .output()
            .unwrap();
    }
    std::fs::write(
        repo_dir.join("main.js"),
        "function greet(name) { return name; }",
    )
    .unwrap();
    for args in [vec!["add", "."], vec!["commit", "-m", "init"]] {
        StdCommand::new("git")
            .args(&args)
            .current_dir(&repo_dir)
            .output()
            .unwrap();
    }

    // The CLI canonicalizes --repo before minting the identity.
    let canonical = std::fs::canonicalize(&repo_dir).unwrap();
    let file_url = format!("file://{}", canonical.display());

    // 1. No origin remote → file:// identity, root_path recorded.
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

    let old_uid = nestweaver_schema::repo_uid("default", &file_url);
    {
        let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
        let repos = store.list_repos(None).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].uid, old_uid);
        assert_eq!(repos[0].url, file_url);
        assert_eq!(
            repos[0].root_path.as_deref(),
            Some(canonical.display().to_string().as_str()),
            "root_path must be recorded for the no-origin fixture"
        );
    }

    // 2. Configure an origin remote and re-index → origin identity wins and
    //    the old file:// row is pruned by uid.
    StdCommand::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://example.com/acme/demo.git",
        ])
        .current_dir(&repo_dir)
        .output()
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

    let new_uid = nestweaver_schema::repo_uid("default", "https://example.com/acme/demo.git");
    assert_ne!(new_uid, old_uid);
    {
        let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
        let repos = store.list_repos(None).unwrap();
        assert_eq!(
            repos.len(),
            1,
            "old file:// row must be pruned; repos: {:?}",
            repos.iter().map(|r| &r.url).collect::<Vec<_>>()
        );
        assert_eq!(repos[0].uid, new_uid);
        assert_eq!(repos[0].url, "https://example.com/acme/demo.git");
        assert_eq!(
            repos[0].root_path.as_deref(),
            Some(canonical.display().to_string().as_str())
        );
        assert!(store.lookup_repo(&old_uid).unwrap().is_none());

        // Regression guard: the re-index above ran WITHOUT --force through
        // the incremental path. Files are unchanged on disk, so a trusted
        // filemeta sidecar would skip every write under the new uid while
        // the prune deletes the old copy — silently emptying the graph. The
        // repo's data must exist under the NEW uid.
        let symbols = store.symbol_names_by_repo(&new_uid).unwrap();
        assert!(
            symbols.iter().any(|n| n == "greet"),
            "symbol `greet` must exist under the new uid after re-identify, got {symbols:?}"
        );
        let files = store.list_files_by_repo(&new_uid).unwrap();
        assert!(
            !files.is_empty(),
            "files must be re-inserted under the new uid"
        );
    }

    // And the symbol stays reachable end-to-end via CLI search.
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
fn cli_contracts_diff_flags_breaking_change() {
    // A removed response field is a BREAKING change; --fail-on-breaking exits nonzero.
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("openapi.base.yaml");
    let head = dir.path().join("openapi.head.yaml");
    std::fs::write(
        &base,
        "openapi: 3.0.0\ninfo: {title: t, version: \"1\"}\npaths:\n  /x:\n    get:\n      responses:\n        '200':\n          description: ok\n          content: {application/json: {schema: {type: object, properties: {id: {type: string}, status: {type: string}}}}}\n",
    )
    .unwrap();
    std::fs::write(
        &head,
        "openapi: 3.0.0\ninfo: {title: t, version: \"1\"}\npaths:\n  /x:\n    get:\n      responses:\n        '200':\n          description: ok\n          content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}\n",
    )
    .unwrap();
    nestweaver_cmd()
        .args(["contracts", "diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .arg("--fail-on-breaking")
        .assert()
        .failure()
        .stdout(contains("BREAKING"));
}

#[test]
fn cli_contracts_diff_clean_on_identical_specs() {
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("openapi.yaml");
    std::fs::write(
        &spec,
        "openapi: 3.0.0\ninfo: {title: t, version: \"1\"}\npaths:\n  /x:\n    get:\n      responses:\n        '200':\n          description: ok\n          content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}\n",
    )
    .unwrap();
    nestweaver_cmd()
        .args(["contracts", "diff", "--base"])
        .arg(&spec)
        .arg("--head")
        .arg(&spec)
        .arg("--fail-on-breaking")
        .assert()
        .success()
        .stdout(contains("No API changes"));
}

/// nw-019: `brain refresh --config` (no `--instance` flag) must tag the vault
/// under the config's `instance_id`, not the literal "default". The vault UID
/// is `vlt:{instance}:{hash}`, so the instance is directly readable from
/// `brain list --json`. Mirrors `brain watch`/`brain add` precedence:
/// `--instance` flag > config's instance_id > "default".
#[test]
fn brain_refresh_uses_config_instance_id() {
    let dir = tempfile::tempdir().unwrap();
    let vault_dir = dir.path().join("vault");
    let db_path = dir.path().join("brain.lbug");
    std::fs::create_dir_all(&vault_dir).unwrap();
    std::fs::write(vault_dir.join("note.md"), "# Hello\n\nsome content\n").unwrap();

    // Instance config declaring a non-default instance_id. Only the fields
    // without serde defaults are required: instance_id, snapshot_storage,
    // workspace, inference, git.
    let config_path = dir.path().join("instance.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "vault-test"
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
"#,
            storage = dir.path().join("storage").display(),
            workspace = dir.path().join("workspace").display(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("storage")).unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();

    // Seed the DB: `brain add --config` creates the vault under the config's
    // instance ("vlt:vault-test:..."). `brain refresh` requires an existing DB.
    nestweaver_cmd()
        .args(["brain", "add"])
        .arg(&vault_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    // Refresh with --config but NO --instance flag. Pre-fix, refresh ignored
    // the config and resolved "default", cascade-deleting nothing and creating
    // a spurious second "vlt:default:..." vault. Post-fix it honors the config's
    // instance_id and re-indexes the same "vlt:vault-test:..." vault in place.
    nestweaver_cmd()
        .args(["brain", "refresh"])
        .arg(&vault_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    // Read the vault back and assert its UID carries the config's instance.
    let output = nestweaver_cmd()
        .args(["brain", "list", "--json", "--db"])
        .arg(&db_path)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let rows: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let uids: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uid"].as_str().unwrap())
        .collect();
    assert!(
        uids.iter().any(|u| u.starts_with("vlt:vault-test:")),
        "vault should be tagged under config instance_id 'vault-test', got uids: {uids:?}"
    );
    assert!(
        !uids.iter().any(|u| u.starts_with("vlt:default:")),
        "vault must NOT be tagged under the literal 'default' instance, got uids: {uids:?}"
    );
}

/// nw-047: the no-daemon `index` direct-write path must resolve the instance
/// id as `--instance` > config `instance_id` > "default" (was
/// `instance.unwrap_or("default")`, which ignored the config). Without a
/// `--instance` flag, a `--config` naming `cfgname` must stamp repos under
/// `repo:cfgname:…`, not `repo:default:…`.
fn nw047_valid_config(dir: &std::path::Path, instance_id: &str) -> std::path::PathBuf {
    let config_path = dir.join(format!("instance-{instance_id}.toml"));
    std::fs::write(
        &config_path,
        format!(
            r#"
instance_id = "{instance_id}"
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
"#,
            storage = dir.join("storage").display(),
            workspace = dir.join("workspace").display(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("storage")).unwrap();
    std::fs::create_dir_all(dir.join("workspace")).unwrap();
    config_path
}

fn nw047_repo_instances(db_path: &std::path::Path) -> Vec<String> {
    let output = nestweaver_cmd()
        .args(["list-repos", "--json", "--db"])
        .arg(db_path)
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let rows: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["instance_id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn no_daemon_index_uses_config_instance_id() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    let db = dir.path().join("test.lbug");
    let config_path = nw047_valid_config(dir.path(), "cfgname");

    // Index with --config but NO --instance flag.
    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .arg("--config")
        .arg(&config_path)
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .assert()
        .success();

    let instances = nw047_repo_instances(&db);
    assert!(
        instances.iter().any(|i| i == "cfgname"),
        "repo must be stamped under config instance 'cfgname', got: {instances:?}"
    );
    assert!(
        !instances.iter().any(|i| i == "default"),
        "repo must NOT fall back to 'default' when --config names an instance, got: {instances:?}"
    );
}

#[test]
fn no_daemon_index_empty_instance_flag_falls_back_to_config() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    let db = dir.path().join("test.lbug");
    let config_path = nw047_valid_config(dir.path(), "cfgname");

    // `--instance ""` must be treated as unset (not a literal empty instance)
    // and fall through to the config's instance_id.
    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .arg("--config")
        .arg(&config_path)
        .arg("--instance")
        .arg("")
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .assert()
        .success();

    let instances = nw047_repo_instances(&db);
    assert!(
        instances.iter().any(|i| i == "cfgname"),
        "empty --instance must fall back to config 'cfgname', got: {instances:?}"
    );
    assert!(
        !instances.iter().any(|i| i.is_empty()),
        "empty --instance must not be stored as a literal empty instance, got: {instances:?}"
    );
}

/// nw-052b: the `--instance` flag must be validated at the CLI choke point.
/// nw-052 rejected a colon only in a `--config`'s `instance_id` (config-load),
/// so `--instance "a:b"` still slipped through and stamped an ambiguous uid
/// `repo:a:b:<hash>`. Validating the RESOLVED instance in `resolve_instance_id`
/// closes the flag path: the command now exits non-zero with a colon error.
#[test]
fn no_daemon_index_rejects_colon_in_instance_flag() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    let db = dir.path().join("test.lbug");

    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .arg("--instance")
        .arg("a:b")
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .assert()
        .failure()
        .stderr(contains("colon"));
}

fn toml_basic_string(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn write_disabled_watch_config(path: &std::path::Path) {
    let root = path.parent().unwrap();
    let storage = root.join("storage");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        path,
        format!(
            r#"instance_id = "cfg-watch"
repos = []

[snapshot_storage]
backend = "local"
path = "{}"

[workspace]
backend = "local"
path = "{}"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

[watch]
enabled = false
"#,
            toml_basic_string(&storage),
            toml_basic_string(&workspace)
        ),
    )
    .unwrap();
}

#[test]
fn toml_path_escaping_preserves_windows_separators() {
    let path = std::path::Path::new(r#"C:\Users\kory\workspace"#);
    assert_eq!(toml_basic_string(path), r#"C:\\Users\\kory\\workspace"#);
}

#[test]
fn no_daemon_brain_watch_rejects_invalid_instance_before_graph_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    let db = dir.path().join("brain.lbug");
    let config = dir.path().join("instance.toml");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("note.md"), "# note").unwrap();
    write_disabled_watch_config(&config);

    nestweaver_cmd()
        .args(["brain", "watch"])
        .arg(&vault)
        .arg("--db")
        .arg(&db)
        .arg("--config")
        .arg(&config)
        .arg("--instance")
        .arg("a:b")
        .assert()
        .failure()
        .stderr(contains("colon"));

    assert!(!db.exists(), "invalid instance must not create the graph");
    assert!(
        !std::path::PathBuf::from(format!("{}.lock", db.display())).exists(),
        "invalid instance must not publish a watcher lock"
    );
}

#[test]
fn no_daemon_brain_watch_accepts_empty_and_valid_instances() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    let db = dir.path().join("brain.lbug");
    let config = dir.path().join("instance.toml");
    std::fs::create_dir_all(&vault).unwrap();
    write_disabled_watch_config(&config);

    for instance in ["", "valid-watch"] {
        nestweaver_cmd()
            .args(["brain", "watch"])
            .arg(&vault)
            .arg("--db")
            .arg(&db)
            .arg("--config")
            .arg(&config)
            .arg("--instance")
            .arg(instance)
            .assert()
            .success()
            .stderr(contains("Watching disabled"));
    }
}

#[test]
fn index_does_not_write_setup_files_into_unrelated_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("cwd");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    // Deterministic detection without relying on host PATH:
    std::fs::create_dir_all(cwd.join(".cursor")).unwrap();
    let db = dir.path().join("test.lbug");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args([
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .current_dir(&cwd)
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .output()
        .unwrap();
    assert!(out.status.success());

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
            "index must not write {f} into an unrelated cwd"
        );
    }
    // Piped output → stderr not a TTY → gate blocks even repo-root writes:
    assert!(
        !repo.join(".mcp.json").exists(),
        "non-TTY index must not write into the repo either"
    );
    // Skipped ≠ done: marker must NOT be written, so a future interactive index still gets setup.
    assert!(
        !dir.path().join("test.lbug.setup_done").exists(),
        "marker must only be written when setup actually ran"
    );
    // The hint must tell the user what they can do instead:
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nestweaver setup"),
        "skip must print a hint, got: {stderr}"
    );
}

#[test]
fn index_setup_prints_banner_at_most_once_with_multiple_tools() {
    // nw-051: auto-setup used to reprint the "NestWeaver Setup" banner once per
    // detected tool. With two tools present it must still appear exactly once.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    // Two detectable tools anchored to the repo root.
    std::fs::create_dir_all(repo.join(".cursor")).unwrap();
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    let db = dir.path().join("test.lbug");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args([
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--setup",
        ])
        .current_dir(&repo)
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .output()
        .unwrap();
    assert!(out.status.success());

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let banner_count = combined.matches("NestWeaver Setup").count();
    assert_eq!(
        banner_count, 1,
        "banner must print exactly once regardless of tool count, got {banner_count}:\n{combined}"
    );
}

#[test]
fn index_setup_quiet_suppresses_setup_banner() {
    // nw-051: --setup --quiet should still configure tools but stay quiet.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    std::fs::create_dir_all(repo.join(".cursor")).unwrap();
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    let db = dir.path().join("test.lbug");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args([
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--setup",
            "--quiet",
        ])
        .current_dir(&repo)
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .output()
        .unwrap();
    assert!(out.status.success());

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("NestWeaver Setup"),
        "--quiet must suppress the setup banner, got:\n{combined}"
    );
    // Setup still happened.
    assert!(
        repo.join(".cursor/mcp.json").exists(),
        "--setup must still configure tools under --quiet"
    );
}

#[test]
fn index_setup_flag_forces_setup_anchored_to_repo_root() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("cwd");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    // Detection anchored to the repo (base), not the cwd:
    std::fs::create_dir_all(repo.join(".cursor")).unwrap();
    let db = dir.path().join("test.lbug");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args([
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--setup",
        ])
        .current_dir(&cwd)
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(
        repo.join(".cursor/mcp.json").exists(),
        "--setup must write configs at the indexed repo root even from a foreign, non-TTY cwd"
    );
    assert!(
        !cwd.join(".cursor/mcp.json").exists(),
        "cwd must stay clean"
    );
    assert!(
        dir.path().join("test.lbug.setup_done").exists(),
        "a forced run counts as done"
    );
}

#[test]
fn index_setup_failure_does_not_write_done_marker() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".cursor")).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    std::fs::write(repo.join(".cursor/mcp.json"), "{ invalid json").unwrap();
    let db = dir.path().join("test.lbug");

    std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .arg("--setup")
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .assert()
        .success();

    assert!(!sidecar_path(&db, ".setup_done").exists());
}

#[test]
fn index_setup_retries_secondary_before_writing_done_marker() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".cursor")).unwrap();
    std::fs::write(repo.join("main.js"), "function f() {}").unwrap();
    let rules_path = repo.join(".cursor/rules");
    std::fs::write(&rules_path, "blocks directory creation").unwrap();
    let db = dir.path().join("test.lbug");
    let marker = sidecar_path(&db, ".setup_done");

    std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .arg("--setup")
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .assert()
        .success();

    assert!(repo.join(".cursor/mcp.json").exists());
    assert!(!rules_path.join("nestweaver.mdc").exists());
    assert!(!marker.exists());

    std::fs::remove_file(&rules_path).unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .arg("--setup")
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .assert()
        .success();

    assert!(rules_path.join("nestweaver.mdc").exists());
    assert!(marker.exists());
}

// ── nw-087: missing-DB guard matrix ───────────────────────────────────
//
// A command run against a NONEXISTENT --db must fail loudly with the
// `db_not_found` diagnostic (exit 1) — never CREATE an empty DB, exit 0 on a
// typo'd path, or autostart a daemon that materializes an empty store. Each
// command is exercised twice: on the no-daemon path (NESTWEAVER_NO_DAEMON=1)
// and on the daemon-autostart path (both env vars removed), proving the guard
// fires before any daemon autostart.

/// Run `args` plus `--db <missing>` in a fresh tempdir, in both daemon-env
/// modes, and assert the full guard contract.
fn assert_missing_db_guard(args: &[&str]) {
    for no_daemon_env in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nope.lbug");
        let db_arg = db_path.display().to_string();

        let mut cmd = Command::cargo_bin("nestweaver").unwrap();
        if no_daemon_env {
            cmd.env("NESTWEAVER_NO_DAEMON", "1")
                .env("NESTWEAVER_ALLOW_NO_DAEMON", "1");
        } else {
            // Same pattern as cli_read_on_missing_db_daemon_path_does_not_create_it:
            // no allow-hatch and no CI marker → the daemon-autostart path.
            cmd.env_remove("NESTWEAVER_ALLOW_NO_DAEMON")
                .env_remove("NESTWEAVER_NO_DAEMON")
                .env_remove("CI")
                .env_remove("GITHUB_ACTIONS");
        }
        let output = cmd
            .env_remove("NESTWEAVER_DB")
            .args(args)
            .arg("--db")
            .arg(&db_arg)
            .output()
            .unwrap();

        let mode = if no_daemon_env {
            "no-daemon"
        } else {
            "daemon-autostart"
        };
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?} against a missing db must exit 1 ({mode} path); stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        assert!(
            combined.contains("db_not_found"),
            "{args:?} against a missing db must render the db_not_found diagnostic ({mode} path), got:\n{combined}"
        );
        assert!(
            !db_path.exists(),
            "{args:?} must not CREATE the missing db ({mode} path)"
        );
        // No sidecar files (`nope.lbug.*`) may appear either.
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().starts_with("nope.lbug"),
                "{args:?} left sidecar {name:?} behind ({mode} path)"
            );
        }
        // No daemon may be left running for this db.
        let status = Command::cargo_bin("nestweaver")
            .unwrap()
            .env_remove("NESTWEAVER_DB")
            .args(["daemon", "--db", &db_arg, "status"])
            .output()
            .unwrap();
        let status_out = format!(
            "{}{}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            status_out.contains("not running"),
            "{args:?} must not leave a daemon running for the missing db ({mode} path); `daemon status` says:\n{status_out}"
        );
    }
}

#[test]
fn cli_missing_db_prune_stale() {
    assert_missing_db_guard(&["prune-stale"]);
}

#[test]
fn cli_missing_db_remove_repo() {
    assert_missing_db_guard(&["remove-repo", "anything"]);
}

#[test]
fn cli_missing_db_export() {
    assert_missing_db_guard(&["export", "--format", "mermaid"]);
}

#[test]
fn cli_missing_db_hubs() {
    assert_missing_db_guard(&["hubs"]);
}

#[test]
fn cli_missing_db_brain_search() {
    assert_missing_db_guard(&["brain", "search", "anything"]);
}

#[test]
fn cli_missing_db_instance_merge() {
    assert_missing_db_guard(&["instance", "merge", "--from", "a", "--to", "b"]);
}

#[test]
fn cli_missing_db_stale_check() {
    assert_missing_db_guard(&["stale-check"]);
}

#[test]
fn cli_missing_db_list_repos() {
    assert_missing_db_guard(&["list-repos"]);
}

#[test]
fn cli_missing_db_brain_orphans() {
    assert_missing_db_guard(&["brain", "orphans"]);
}

#[test]
fn cli_missing_db_brain_topic_clusters() {
    assert_missing_db_guard(&["brain", "topic-clusters"]);
}

#[test]
fn cli_missing_db_brain_tag_graph() {
    assert_missing_db_guard(&["brain", "tag-graph"]);
}

#[test]
fn cli_missing_db_brain_doc_stats() {
    assert_missing_db_guard(&["brain", "doc-stats"]);
}

#[test]
fn cli_missing_db_memory_lint() {
    assert_missing_db_guard(&["memory", "lint"]);
}

#[test]
fn cli_missing_db_memory_consolidate() {
    assert_missing_db_guard(&["memory", "consolidate"]);
}

#[test]
fn cli_missing_db_memory_related() {
    assert_missing_db_guard(&["memory", "related", "note:x"]);
}

// ── create-operations create missing --db parent dirs ──────────────────────
//
// `index` / `brain add` MATERIALIZE the database, so a --db pointing into a
// not-yet-existing directory must be created — never fail with the circular
// db_not_found diagnostic ("Run `nestweaver index` to create a database")
// while running index.

#[test]
fn cli_index_creates_missing_db_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("main.js"),
        "function greet(name) { return name; }",
    )
    .unwrap();
    let db_path = dir.path().join("missing").join("parent").join("test.lbug");

    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo_dir)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();
    assert!(
        db_path.exists(),
        "index must create the db in a previously missing parent dir"
    );
}

#[test]
fn cli_brain_add_creates_missing_db_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let vault_dir = dir.path().join("vault");
    std::fs::create_dir_all(&vault_dir).unwrap();
    std::fs::write(vault_dir.join("note.md"), "# Hello\n\nworld\n").unwrap();
    let db_path = dir.path().join("missing").join("parent").join("brain.lbug");

    nestweaver_cmd()
        .args(["brain", "add"])
        .arg(&vault_dir)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();
    assert!(
        db_path.exists(),
        "brain add must create the db in a previously missing parent dir"
    );
}

// ── --stats is honored by list-repos and search ────────────────────────────

#[test]
fn cli_list_repos_and_search_honor_stats_flag() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("main.js"),
        "function greet(name) { return name; }",
    )
    .unwrap();

    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo_dir)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    // Both commands must print a "stats:" line (stderr) under --stats,
    // matching the hubs/brain-search pattern.
    nestweaver_cmd()
        .args(["list-repos", "--stats", "--db"])
        .arg(&db_path)
        .assert()
        .success()
        .stderr(contains("stats:"));

    nestweaver_cmd()
        .args(["search", "greet", "--stats", "--db"])
        .arg(&db_path)
        .assert()
        .success()
        .stderr(contains("stats:"));
}

// ── `instance abort-migration` journal recovery ────────────────────────

/// The instance-migration journal lives at `<db>.extensions.migration.json`
/// (see `instance_extension_migration_journal_path` in
/// crates/nestweaver-engine/src/extensions.rs).
fn migration_journal_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut path = db_path.as_os_str().to_owned();
    path.push(".extensions.migration.json");
    std::path::PathBuf::from(path)
}

/// Create a scratch DB and leave a syntactically VALID, parseable journal in
/// the `graph_applied` phase — produced by the real engine writer so the
/// version, fingerprint, and operation-id invariants all hold.
fn scratch_db_with_graph_applied_journal(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = dir.join("test.lbug");
    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    drop(store);
    let mappings = vec![nestweaver_store::InstanceUidRemap {
        source_uid: nestweaver_schema::vault_uid("from", "/tmp/nw-abort-migration-test/vault"),
        destination_uid: nestweaver_schema::vault_uid("to", "/tmp/nw-abort-migration-test/vault"),
    }];
    let migration =
        nestweaver_engine::prepare_instance_extension_migration(&db_path, "from", "to", &mappings)
            .unwrap();
    nestweaver_engine::mark_instance_extension_migration_graph_applied(&db_path, &migration)
        .unwrap();
    db_path
}

#[test]
fn cli_abort_migration_wedged_valid_journal_requires_force() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = scratch_db_with_graph_applied_journal(dir.path());
    let db_arg = db_path.display().to_string();
    let journal_path = migration_journal_path(&db_path);
    assert!(journal_path.exists(), "precondition: journal was written");

    // A graph-applied journal is REFUSED without --force: the graph was
    // already mutated, so aborting would leave it half-migrated.
    nestweaver_cmd()
        .args(["instance", "abort-migration", "--db", &db_arg])
        .assert()
        .code(1)
        .stderr(contains("graph"));
    assert!(
        journal_path.exists(),
        "a refused abort must leave the journal in place"
    );

    // With --force the journal is discarded and the command succeeds.
    nestweaver_cmd()
        .args(["instance", "abort-migration", "--db", &db_arg, "--force"])
        .assert()
        .code(0);
    assert!(
        !journal_path.exists(),
        "--force must remove the wedged journal"
    );
}

#[test]
fn cli_abort_migration_corrupt_journal_force_discards() {
    // A journal that fails to parse carries no trustworthy phase
    // information. Without --force the abort refuses; with --force the corrupt
    // journal is discarded (phase reported as unknown) so the daemon can boot.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    drop(store);
    let db_arg = db_path.display().to_string();
    let journal_path = migration_journal_path(&db_path);
    std::fs::write(&journal_path, b"\x00\xff not json at all {{{").unwrap();

    nestweaver_cmd()
        .args(["instance", "abort-migration", "--db", &db_arg])
        .assert()
        .code(1)
        .stderr(contains("--force"));
    assert!(
        journal_path.exists(),
        "a refused abort must leave the corrupt journal in place"
    );

    nestweaver_cmd()
        .args(["instance", "abort-migration", "--db", &db_arg, "--force"])
        .assert()
        .code(0)
        .stderr(contains("Force-discarded an unreadable migration journal"));
    assert!(
        !journal_path.exists(),
        "--force must remove the corrupt journal"
    );
}
