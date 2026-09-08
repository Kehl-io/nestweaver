//! Process-level checks for the September regression backlog. Every mutating
//! command is bound to a private database, registry, and checkout destination.
use assert_cmd::Command;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

struct Fixture {
    dir: tempfile::TempDir,
    db: PathBuf,
    repo: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.lbug");
        let repo = dir.path().join("repo");
        let config = dir.path().join("instance.toml");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(
            repo.join("calc.py"),
            "def add(x, y):\n    return x + y\n\ndef twice(x):\n    return add(x, x)\n",
        )
        .unwrap();
        for args in [
            vec!["init"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Regression",
                "-c",
                "user.email=regression@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        ] {
            let out = std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // JSON quoted strings are valid TOML basic strings, including Windows
        // path escaping. No credentials or external services are required.
        let quote = |path: &Path| serde_json::to_string(&path.to_string_lossy()).unwrap();
        std::fs::write(&config, format!(
            "instance_id = \"selected\"\ndb = {}\n[snapshot_storage]\nbackend = \"local\"\npath = {}\n[workspace]\nbackend = \"local\"\npath = {}\n[inference]\nendpoint = \"http://localhost:11434\"\nembedding_model = \"unused\"\nsummary_model = \"unused\"\n[embedding]\nexternal_endpoint = \"http://127.0.0.1:9\"\nexternal_model = \"unused\"\n[git]\ncredential_method = \"gh\"\n",
            quote(&db), quote(&dir.path().join("snapshots")), quote(&dir.path().join("selected-workspace")),
        )).unwrap();
        Self {
            dir,
            db,
            repo,
            config,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("nestweaver").unwrap();
        cmd.current_dir(self.dir.path())
            .timeout(Duration::from_secs(180))
            .env_remove("NESTWEAVER_DB")
            .env_remove("NESTWEAVER_NO_DAEMON")
            .env_remove("NESTWEAVER_ALLOW_NO_DAEMON")
            .env("NESTWEAVER_DIAGNOSTIC_WIDTH", "1000")
            .env("NESTWEAVER_EPHEMERAL_IDLE_TIMEOUT_SECS", "60")
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"));
        #[cfg(not(target_os = "macos"))]
        cmd.env("NESTWEAVER_DAEMON_FORK", "1");
        cmd
    }

    fn query(&self, args: &[&str], direct: bool) -> Output {
        let mut cmd = self.cmd();
        cmd.args(args).arg("--db").arg(&self.db);
        if direct {
            cmd.env("NESTWEAVER_NO_DAEMON", "1")
                .env("NESTWEAVER_ALLOW_NO_DAEMON", "1");
        }
        cmd.output().unwrap()
    }

    fn index(&self) {
        self.cmd()
            .args(["index", "--repo"])
            .arg(&self.repo)
            .arg("--db")
            .arg(&self.db)
            .env("NESTWEAVER_NO_DAEMON", "1")
            .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
            .assert()
            .success();
        // Bind lexical-only fixtures to a deliberately unavailable loopback
        // embedding endpoint, avoiding unrelated CPU model initialization.
        // Subsequent configless autostarts reuse this explicit startup intent.
        self.cmd()
            .args(["daemon", "start", "--db"])
            .arg(&self.db)
            .arg("--config")
            .arg(&self.config)
            .assert()
            .success();
        self.stop();
    }

    fn stop(&self) {
        if self.db.exists() {
            self.cmd()
                .args(["daemon", "stop", "--db"])
                .arg(&self.db)
                .assert()
                .success();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Cleanup must also execute after a failed assertion; never signal any
        // daemon except the one identified by this unique temporary database.
        if self.db.exists() {
            let _ = self
                .cmd()
                .args(["daemon", "stop", "--db"])
                .arg(&self.db)
                .output();
        }
    }
}

fn payload(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}; exit={:?}, stdout={}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn invalid_regex_preserves_usage_exit_and_json_across_routes() {
    let f = Fixture::new();
    f.index();
    for tool in ["regex-search", "count-patterns"] {
        for direct in [false, true] {
            f.stop();
            let out = f.query(&[tool, "[", "--json"], direct);
            assert_eq!(
                out.status.code(),
                Some(64),
                "{tool}, direct={direct}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(payload(&out)["error"], "invalid argument");
        }
    }
}

#[test]
fn missing_service_and_brain_seed_preserve_not_found_json_across_routes() {
    let f = Fixture::new();
    f.index();
    for args in [
        vec!["service-summary", "absent-regression-service", "--json"],
        vec![
            "brain",
            "context",
            "absent-regression-seed",
            "--no-embed",
            "--json",
        ],
    ] {
        for direct in [false, true] {
            f.stop();
            let out = f.query(&args, direct);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{args:?}, direct={direct}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(payload(&out)["error"], "not found");
        }
    }
}

#[test]
fn empty_git_diff_still_refuses_incompatible_resolver_generation() {
    let f = Fixture::new();
    f.index();
    let sidecar = PathBuf::from(format!("{}.resolver_generation.json", f.db.display()));
    let mut generation: Value = serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    for incompatible in [0_u64, u64::MAX] {
        for value in generation["repos"].as_object_mut().unwrap().values_mut() {
            *value = json!(incompatible);
        }
        std::fs::write(&sidecar, serde_json::to_vec(&generation).unwrap()).unwrap();
        // Both routes must refuse, although no changed file ever reaches the
        // selection engine on the old shortcut.
        for direct in [false, true] {
            f.stop();
            let mut cmd = f.cmd();
            cmd.current_dir(&f.repo)
                .args(["affected-tests", "--base-ref", "HEAD", "--json", "--db"])
                .arg(&f.db);
            if direct {
                cmd.env("NESTWEAVER_NO_DAEMON", "1")
                    .env("NESTWEAVER_ALLOW_NO_DAEMON", "1");
            }
            let out = cmd.output().unwrap();
            assert_eq!(
                out.status.code(),
                Some(2),
                "direct={direct}, generation={incompatible}"
            );
            let value = payload(&out);
            assert_eq!(value["refused"], true);
            assert_eq!(value["recommendation"], "run-full-suite");
            assert!(value.get("tier_1").is_none());
        }
        f.stop();
    }
}

#[test]
fn daemon_start_and_restart_honor_config_database_without_redundant_flag() {
    let f = Fixture::new();
    f.index();
    for action in ["start", "restart"] {
        f.cmd()
            .env(
                "NESTWEAVER_DB",
                f.dir.path().join("ambient-must-not-win.lbug"),
            )
            .args(["daemon", action, "--config"])
            .arg(&f.config)
            .assert()
            .success();
        let out = f.query(&["search", "twice", "--json"], false);
        assert!(out.status.success());
        assert!(!payload(&out)["results"].as_array().unwrap().is_empty());
    }
}

#[test]
fn foreground_daemon_run_honors_config_database_before_environment() {
    let f = Fixture::new();
    f.index();
    let ambient = f.dir.path().join("ambient-must-not-win.lbug");
    f.cmd()
        .env("NESTWEAVER_DB", &ambient)
        .args(["daemon", "run", "--config"])
        .arg(&f.config)
        .args(["--idle-timeout", "1"])
        .assert()
        .success();
    assert!(
        !ambient.exists(),
        "foreground run opened the ambient DB instead of config.db"
    );
}

#[test]
fn healthy_empty_diff_ignores_an_unrelated_incompatible_repo() {
    let f = Fixture::new();
    f.index();
    let unrelated = f.dir.path().join("unrelated");
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::write(unrelated.join("other.py"), "def unrelated(): pass\n").unwrap();
    f.cmd()
        .args(["index", "--repo"])
        .arg(&unrelated)
        .arg("--db")
        .arg(&f.db)
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .assert()
        .success();
    let repos = payload(&f.query(&["list-repos", "--json"], true));
    // The indexer stores a CANONICAL root_path, so comparing it to the path
    // string the test spelled is only correct where the two coincide. On macOS
    // `TMPDIR` lives under `/var/folders/...` and `/var` is a symlink to
    // `/private/var`, so the stored path is `/private/var/folders/...` and this
    // find never matched -- the test failed at "unrelated repo must be indexed"
    // on every macOS run while passing in CI, which is Linux-only. Canonicalise
    // both sides so the comparison means what it says on either platform.
    let unrelated_canonical =
        std::fs::canonicalize(&unrelated).expect("unrelated dir must exist to canonicalise");
    let unrelated_uid = repos
        .as_array()
        .unwrap()
        .iter()
        .find(|repo| {
            repo["root_path"]
                .as_str()
                .and_then(|path| std::fs::canonicalize(path).ok())
                .is_some_and(|path| path == unrelated_canonical)
        })
        .expect("unrelated repo must be indexed")["uid"]
        .as_str()
        .unwrap();
    let sidecar = PathBuf::from(format!("{}.resolver_generation.json", f.db.display()));
    let mut generation: Value = serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    generation["repos"][unrelated_uid] = json!(0);
    std::fs::write(&sidecar, serde_json::to_vec(&generation).unwrap()).unwrap();
    for direct in [false, true] {
        f.stop();
        let mut cmd = f.cmd();
        cmd.current_dir(&f.repo)
            .args(["affected-tests", "--base-ref", "HEAD", "--json", "--db"])
            .arg(&f.db);
        if direct {
            cmd.env("NESTWEAVER_NO_DAEMON", "1")
                .env("NESTWEAVER_ALLOW_NO_DAEMON", "1");
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let result = payload(&out);
        assert_eq!(result["recommendation"], "selection-usable");
        assert_eq!(result["status"], "complete");
        assert!(result["tier_1"].as_array().unwrap().is_empty());
        assert!(result["disclaimer"].as_str().is_some());
    }
}

#[test]
fn invalid_configuration_json_is_on_stdout() {
    let f = Fixture::new();
    let invalid = f.dir.path().join("invalid.toml");
    std::fs::write(&invalid, "broken = [\n").unwrap();
    let out = f
        .cmd()
        .args(["config", "validate"])
        .arg(invalid)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(payload(&out)["valid"], false);
}

#[test]
fn format_comment_stdin_matches_file_and_missing_file_still_fails() {
    let f = Fixture::new();
    let report = r#"{"changes":0,"impacts":[],"total_impacted_files":0,"total_impacted_repos":0}"#;
    let input = f.dir.path().join("impact.json");
    let from_file = f.dir.path().join("file.md");
    let from_stdin = f.dir.path().join("stdin.md");
    std::fs::write(&input, report).unwrap();
    f.cmd()
        .args(["format-comment", "--input"])
        .arg(input)
        .arg("--output")
        .arg(&from_file)
        .assert()
        .success();
    f.cmd()
        .args(["format-comment", "--input", "-", "--output"])
        .arg(&from_stdin)
        .write_stdin(report)
        .assert()
        .success();
    assert_eq!(
        std::fs::read(from_file).unwrap(),
        std::fs::read(from_stdin).unwrap()
    );
    f.cmd()
        .args(["format-comment", "--input", "does-not-exist.json"])
        .assert()
        .code(2);
}

#[test]
fn diagnostics_human_output_identifies_the_structured_incident_and_signatures() {
    let f = Fixture::new();
    let structured = f
        .cmd()
        .args(["diagnostics", "capabilities", "--json"])
        .output()
        .unwrap();
    let human = f
        .cmd()
        .args(["diagnostics", "capabilities"])
        .output()
        .unwrap();
    assert!(structured.status.success() && human.status.success());
    let value = payload(&structured);
    let crash = &value["crash_recurrence"];
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(
        text.contains(crash["backlog_id"].as_str().unwrap()),
        "{text}"
    );
    match &crash["signature"] {
        Value::String(signature) => assert!(text.contains(signature)),
        Value::Array(signatures) => {
            for signature in signatures {
                assert!(text.contains(signature.as_str().unwrap()), "{text}");
            }
        }
        other => panic!("unhandled diagnostic schema: {other}"),
    }
}

#[test]
fn brain_remove_counts_real_nondefault_vault_and_does_not_claim_a_second_removal() {
    let f = Fixture::new();
    // Vault-only database: an unrelated default-instance code repo would trip
    // instance ambiguity before reaching the removal accounting behavior.
    let vault = f.dir.path().join("vault");
    std::fs::create_dir(&vault).unwrap();
    std::fs::write(vault.join("Home.md"), "# Home\nregression sentinel\n").unwrap();
    f.cmd()
        .args(["daemon", "start", "--db"])
        .arg(&f.db)
        .arg("--config")
        .arg(&f.config)
        .assert()
        .success();
    f.cmd()
        .args(["brain", "add"])
        .arg(&vault)
        .args(["--instance", "selected", "--db"])
        .arg(&f.db)
        .assert()
        .success();
    let first = f
        .cmd()
        .args(["brain", "remove"])
        .arg(&vault)
        .arg("--db")
        .arg(&f.db)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_text = String::from_utf8(first.stdout).unwrap();
    assert!(first_text.contains("1 row(s) cleaned"), "{first_text}");
    let second = f
        .cmd()
        .args(["brain", "remove"])
        .arg(vault)
        .arg("--db")
        .arg(&f.db)
        .output()
        .unwrap();
    assert!(matches!(second.status.code(), Some(0 | 2)));
    assert!(!String::from_utf8_lossy(&second.stdout).contains("1 row(s) cleaned"));
}

// On Linux dirs::config_dir/data_local_dir honor XDG. Other platforms need
// their own private registry mechanism before these writes can be isolated.
#[cfg(target_os = "linux")]
#[test]
fn selected_pull_rejects_unknown_instance_and_uses_registered_workspace() {
    let f = Fixture::new();
    f.index();
    let registry = f.dir.path().join("config/nestweaver/registry.json");
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    std::fs::write(&registry, r#"{"instances":[]}"#).unwrap();
    let url = format!("file://{}", f.repo.display());
    let unknown = f.query(
        &["pull", &url, "--instance", "absent-instance", "--full"],
        false,
    );
    assert!(
        !unknown.status.success(),
        "unknown instance cloned: {}",
        String::from_utf8_lossy(&unknown.stdout)
    );
    assert!(!f.dir.path().join("data/nestweaver/workspace").exists());
    std::fs::write(&registry, serde_json::to_vec(&json!({"instances": [{"id": "selected", "config_path": f.config, "snapshot_path": null}]})).unwrap()).unwrap();
    let selected = f.query(&["pull", &url, "--instance", "selected", "--full"], false);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(
        String::from_utf8_lossy(&selected.stdout)
            .contains(f.dir.path().join("selected-workspace").to_str().unwrap())
    );
}

#[test]
fn invalid_dead_code_confidence_fails_before_any_route_or_database_work() {
    let fixture = Fixture::new();
    for direct in [false, true] {
        for value in ["", "bogus", "NaN", "LOW"] {
            let output = fixture.query(
                &["dead-code", &format!("--min-confidence={value}"), "--json"],
                direct,
            );
            assert_eq!(
                output.status.code(),
                Some(64),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !fixture.db.exists(),
                "invalid input must not start a database"
            );
        }
    }
}

#[test]
fn prune_cli_preserves_commit_state_for_noop_and_real_deletion() {
    let fixture = Fixture::new();
    fixture.index();
    let clean = fixture.query(&["prune-stale", "--json"], false);
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let clean = payload(&clean);
    assert_eq!(clean["committed"], false);
    assert_eq!(clean["removed_repos"], json!([]));
    assert_eq!(clean["reconciliation_failures"], json!([]));
    std::fs::remove_dir_all(&fixture.repo).unwrap();
    let removed = fixture.query(&["prune-stale", "--json"], false);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let removed = payload(&removed);
    assert_eq!(removed["committed"], true);
    assert_eq!(removed["removed_repos"].as_array().unwrap().len(), 1);
    assert_eq!(removed["reconciliation_failures"], json!([]));
    let repeat = fixture.query(&["prune-stale"], false);
    assert!(repeat.status.success());
    assert!(String::from_utf8_lossy(&repeat.stdout).contains("Committed: false"));
}

#[test]
fn discovered_mcp_configuration_rejects_invalid_files_before_handshake() {
    let f = Fixture::new();
    f.index();
    let valid = std::fs::read_to_string(&f.config).unwrap();
    let init = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"regression\",\"version\":\"1\"}}}\n";
    for contents in [
        None,
        Some(valid.as_str()),
        Some("[broken"),
        Some("unknown_typo = true\n"),
    ] {
        match contents {
            Some(text) => std::fs::write(&f.config, text).unwrap(),
            None => std::fs::remove_file(&f.config).unwrap(),
        }
        let output = f
            .cmd()
            .args(["mcp", "--db"])
            .arg(&f.db)
            .env("NESTWEAVER_NO_DAEMON", "1")
            .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
            .write_stdin(init)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        if matches!(contents, Some("[broken" | "unknown_typo = true\n")) {
            assert!(!output.status.success());
            assert!(
                !stdout.contains("serverInfo"),
                "invalid configuration initialized: {stdout}"
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("instance.toml"));
        } else {
            assert!(
                stdout.contains("serverInfo"),
                "valid/absent config failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn first_index_retains_full_edge_count_and_incremental_does_not_invent_zero() {
    let f = Fixture::new();
    let run = |force: bool| {
        let mut cmd = f.cmd();
        cmd.args(["index", "--repo"])
            .arg(&f.repo)
            .arg("--db")
            .arg(&f.db)
            .arg("--json")
            .env("NESTWEAVER_NO_DAEMON", "1")
            .env("NESTWEAVER_ALLOW_NO_DAEMON", "1");
        if force {
            cmd.arg("--force");
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        payload(&out)
    };
    let first = run(false);
    let force = run(true);
    assert!(first["edges_found"].as_u64().unwrap() > 0);
    assert_eq!(first["edges_found"], force["edges_found"]);
    // This fixture has only top-level functions: its build population is
    // exactly persisted REFERENCES, with no MEMBER_OF or cross-repo edges.
    let store = nestweaver_store::GraphStore::open_read_only(&f.db).unwrap();
    assert_eq!(
        first["edges_found"].as_u64().unwrap(),
        store
            .load_typed_edges()
            .unwrap()
            .iter()
            .filter(|e| e.0.starts_with("sym:") && e.1.starts_with("sym:"))
            .count() as u64
    );
    drop(store);
    assert!(run(false)["edges_found"].is_null());
}

#[test]
fn brain_list_preserves_selected_output_mode_across_routes() {
    let f = Fixture::new();
    f.index();
    for direct in [true, false] {
        let human = f.query(&["brain", "list"], direct);
        assert!(
            human.status.success(),
            "{}",
            String::from_utf8_lossy(&human.stderr)
        );
        assert!(String::from_utf8_lossy(&human.stdout).contains("No vaults indexed"));
        let json = f.query(&["brain", "list", "--json"], direct);
        assert!(json.status.success());
        assert_eq!(payload(&json), json!([]));
        f.stop();
    }
    let vault = f.dir.path().join("notes");
    std::fs::create_dir(&vault).unwrap();
    std::fs::write(vault.join("Note.md"), "# Note\nA populated inventory.\n").unwrap();
    let added = f.query(&["brain", "add", vault.to_str().unwrap()], true);
    assert!(added.status.success());
    let direct = f.query(&["brain", "list", "--json"], true);
    let daemon = f.query(&["brain", "list", "--json"], false);
    assert!(direct.status.success() && daemon.status.success());
    assert_eq!(payload(&direct), payload(&daemon));
    assert_eq!(payload(&daemon)[0]["notes"], 1);
    assert_eq!(payload(&daemon)[0]["instance_id"], "default");
    let human = f.query(&["brain", "list"], false);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("Notes: 1"));
}

#[test]
fn immutable_context_ranking_is_identical_across_ten_processes() {
    let f = Fixture::new();
    for i in 0..8 {
        std::fs::write(
            f.repo.join(format!("tied{i}.py")),
            "def tied():\n    return 1\n",
        )
        .unwrap();
    }
    f.index();
    let mut baseline = None;
    let mut eval_baseline = None;
    let judgments = f.dir.path().join("judgments.jsonl");
    let search = f.query(&["brain", "search", "tied", "--json"], true);
    assert!(search.status.success());
    let hits = payload(&search);
    let uid = hits["results"][0]["uid"].as_str().unwrap();
    std::fs::write(
        &judgments,
        json!({"query":"tied", "relevance":{uid:3}}).to_string(),
    )
    .unwrap();

    for _ in 0..10 {
        let out = f.query(
            &[
                "brain",
                "context",
                "tied",
                "--weight-semantic",
                "0",
                "--json",
            ],
            true,
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value = payload(&out);
        let ranking =
            json!({"seeds_expanded":value["seeds_expanded"], "connected":value["connected"]});
        let run = f.query(
            &[
                "eval",
                "run",
                "--queries",
                judgments.to_str().unwrap(),
                "--json",
            ],
            true,
        );
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let report = payload(&run);
        assert!(
            report["mean_mrr"].as_f64().unwrap() > 0.0,
            "the judged UID must be retrieved: {report}"
        );
        if let Some(expected) = &eval_baseline {
            assert_eq!(&report, expected);
        } else {
            eval_baseline = Some(report.clone());
        }
        let compare = f.query(
            &[
                "eval",
                "compare",
                "--queries",
                judgments.to_str().unwrap(),
                "--prf",
                "--json",
            ],
            true,
        );
        assert!(
            compare.status.success(),
            "{}",
            String::from_utf8_lossy(&compare.stderr)
        );
        assert_eq!(payload(&compare)["baseline"], report);

        assert!(ranking["seeds_expanded"].as_u64().unwrap() > 0);
        if let Some(expected) = &baseline {
            assert_eq!(&ranking, expected);
        } else {
            baseline = Some(ranking);
        }
    }
}
