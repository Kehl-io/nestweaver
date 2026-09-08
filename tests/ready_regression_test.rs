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
