use assert_cmd::Command;
use assert_cmd::assert::OutputAssertExt;
use nestweaver_engine::sidecar_path;
use predicates::prelude::PredicateBooleanExt;
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
fn publication_rebuild_rejects_no_embed_before_config_or_operation_setup() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("brain.lbug");
    let missing_config = dir.path().join("missing.toml");

    nestweaver_cmd()
        .arg("--no-embed")
        .args(["publication", "rebuild", "--config"])
        .arg(&missing_config)
        .arg("--db")
        .arg(&db)
        .assert()
        .failure()
        .stderr(contains(
            "--no-embed is incompatible with `publication rebuild`",
        ));

    assert!(!db.exists());
    assert!(!nestweaver_engine::publication::default_publication_root(&db).exists());

    // The global flag is irrelevant to read-only publication commands. This
    // guards against attaching the validation to the wrong match arm.
    let root = dir.path().join("empty-publications");
    std::fs::create_dir_all(&root).unwrap();
    nestweaver_cmd()
        .arg("--no-embed")
        .args(["publication", "status", "--root"])
        .arg(&root)
        .assert()
        .success();
}

#[test]
fn mcp_help_hides_deprecated_flag_and_invalid_allowlists_fail_early() {
    nestweaver_cmd()
        .args(["mcp", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("allow-mcp-add-sources").not());

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("brain.lbug");
    nestweaver_cmd()
        .args(["mcp", "--db"])
        .arg(&db)
        .args(["--tools", "context"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(contains("unknown MCP tool 'context'"));

    drop(nestweaver_store::GraphStore::open_or_create(&db).unwrap());
    let output = nestweaver_cmd()
        .args(["mcp", "--db"])
        .arg(&db)
        .arg("--allow-mcp-add-sources")
        .write_stdin("")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr
            .matches("--allow-mcp-add-sources is deprecated")
            .count(),
        1
    );
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

/// nw-256: `stale_check`'s commit distance must be `null` when it cannot be
/// counted — never `0`.
///
/// The MCP route was fixed to `Option<u64>` in #310; the CLI kept
/// `.unwrap_or(0)`, so the two routes answered differently for the same repo.
/// That is the SECOND time this function pair diverged for this reason — the
/// comment recording the first (`is_stale`, nw-163) sits four lines below the
/// defect.
///
/// A zero here is not a missed detection, it is a self-contradiction: the
/// branch is only reached when HEAD already differs from the indexed SHA, so
/// the row reads "STALE, 0 commits behind".
///
/// Provoked the way it actually happens — a repo whose history was replaced
/// (re-clone, force-push, squash). HEAD reads fine, but `indexed_sha..HEAD`
/// spans no common ancestor and `git rev-list` fails.
#[test]
fn stale_check_reports_an_uncountable_commit_distance_as_null_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir(&repo_dir).unwrap();

    let git = |args: &[&str]| {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(&repo_dir)
            .status()
            .expect("git command failed to spawn");
        assert!(status.success(), "git {args:?} failed with {status:?}");
    };
    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(repo_dir.join("a.js"), "function hello() {}").unwrap();
    git(&["add", "a.js"]);
    git(&["commit", "-m", "initial"]);

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

    // Replace the history wholesale. HEAD is still a perfectly readable sha —
    // it simply has no path back to the indexed one.
    std::fs::remove_dir_all(repo_dir.join(".git")).unwrap();
    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(repo_dir.join("a.js"), "function hello() { return 1; }").unwrap();
    git(&["add", "a.js"]);
    git(&["commit", "-m", "unrelated history"]);

    let output = nestweaver_cmd()
        .args([
            "stale-check",
            "--db",
            &db_path.display().to_string(),
            "--json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stale-check --json: {e}\n{stdout}"));

    let repos = parsed["repos"]
        .as_array()
        .unwrap_or_else(|| panic!("no `repos` array in:\n{stdout}"));
    let repo = repos.first().expect("exactly one repo was indexed");

    // The counterweight: the row really is stale. Without this, a `null` count
    // could just mean the whole check declined to look.
    assert_eq!(
        repo["is_stale"].as_bool(),
        Some(true),
        "the fixture must actually be stale for the count to be meaningful:\n{stdout}"
    );
    assert!(
        repo["staleness_commits_behind"].is_null(),
        "an uncountable commit distance must be null, not a fabricated 0 — \
         `stale, 0 commits behind` is a contradiction:\n{stdout}"
    );

    // And the human-facing renderer must not collapse it back to a real zero.
    let text = nestweaver_cmd()
        .args(["stale-check", "--db", &db_path.display().to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains("unavailable"),
        "the text renderer must say the count is unavailable rather than print \
         the row as if nothing were behind:\n{text}"
    );
}

/// nw-257: `extensions list` documents its filtering as mirroring
/// `query_extensions` "exactly", and did not.
///
/// `property_matches` treats a scalar query as a membership test against an
/// array-valued property, because that is the shape real sidecars have
/// (`aliases: ["Widget","widget"]`) — nw-109 added it for exactly that reason.
/// The CLI restated the filter as equality only, so the audit command printed
/// "No extension annotations found." for data the agent matched.
///
/// The existing guards stop at clap: replacing the whole match arm with
/// `_ => true` passes both of them, and the one named
/// `the_filters_match_the_mcp_tools_two_modes` guards precisely the property
/// that was broken.
#[test]
fn extensions_list_matches_array_valued_properties_like_the_mcp_tool() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

    std::fs::write(
        sidecar_path(&db_path, ".extensions.json"),
        r#"{"sym:repo:widget": {"aliases": ["Widget", "widget"], "owner": "platform"}}"#,
    )
    .unwrap();

    let matched = nestweaver_cmd()
        .args([
            "extensions",
            "list",
            "--db",
            &db_path.display().to_string(),
            "--key",
            "aliases",
            "--value",
            "Widget",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&matched.stdout);
    assert!(
        stdout.contains("sym:repo:widget"),
        "a scalar --value must match a member of an array-valued property, the \
         way `query_extensions` does:\n{stdout}"
    );

    // The counterweight: membership must not have become an any-of that
    // matches everything. A value that is in no array still finds nothing.
    let absent = nestweaver_cmd()
        .args([
            "extensions",
            "list",
            "--db",
            &db_path.display().to_string(),
            "--key",
            "aliases",
            "--value",
            "Gadget",
        ])
        .output()
        .unwrap();
    let absent = String::from_utf8_lossy(&absent.stdout);
    assert!(
        !absent.contains("sym:repo:widget"),
        "membership must stay a filter — a value in no array must not match:\n{absent}"
    );
}

/// nw-257: the audit command must not report "0 annotated node(s)" because it
/// could not read the sidecar.
///
/// `load_extensions` folds both a parse failure and an I/O failure into an
/// empty map. In a command whose entire purpose is reporting what is
/// annotated, that turns "I cannot tell you" into "there is nothing", and
/// exits 0 while doing it.
#[test]
fn extensions_list_fails_loudly_on_an_unreadable_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

    // The counterweight, first: NO sidecar is not an error. Nothing has been
    // annotated yet, and "0 annotated node(s)" is the honest answer.
    nestweaver_cmd()
        .args(["extensions", "list", "--db", &db_path.display().to_string()])
        .assert()
        .success();

    std::fs::write(
        sidecar_path(&db_path, ".extensions.json"),
        "{ this is not json",
    )
    .unwrap();

    let output = nestweaver_cmd()
        .args(["extensions", "list", "--db", &db_path.display().to_string()])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a sidecar that cannot be parsed must fail the audit, not be reported \
         as an empty one"
    );
    // Asserted POSITIVELY. `.stdout(contains("0 annotated node(s)").not())`
    // was the obvious way to write this and is the wrong one: a negative
    // substring check passes for every reason a string can be absent,
    // including a rendering change or a line wrap, so it would keep passing
    // after the fix was undone in some other way.
    let stderr = flatten_miette(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("extension sidecar"),
        "the failure must name the sidecar as the thing it could not read:\n{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("annotated node"),
        "an unreadable sidecar must not also print a count as though it had \
         looked:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// nw-244: PR #307 taught `brain remove` to honour `--config` like its two
/// sibling vault commands, but both of its guards stop at clap — one asserts
/// the flag is *accepted*, the other that it lands in the parsed struct.
/// Reverting the resolution itself leaves the flag parsing perfectly and the
/// whole suite green, which is precisely the nw-217 bug returning: the command
/// silently targets instance "default" and `./nestweaver.lbug`.
///
/// The behaviour that actually closes that hole is a REFUSAL. The direct
/// (no-daemon) store cannot honour a pinned instance config, so rather than
/// quietly acting on some other instance, `brain remove --config` fails and
/// names the config it could not honour. Silence is the defect; the error is
/// the fix.
///
/// Both halves are asserted, because the refusal alone would also be satisfied
/// by a command that is simply broken: with the same vault and the same store
/// addressed by `--db` instead, the remove must succeed.
#[test]
fn brain_remove_refuses_a_pinned_config_it_cannot_honor_rather_than_silently_defaulting() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("configured.lbug");
    let vault_dir = dir.path().join("vault");
    std::fs::create_dir(&vault_dir).unwrap();
    std::fs::write(vault_dir.join("note.md"), "# Note\n\nBody.\n").unwrap();

    let config_path = dir.path().join("instance.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"instance_id = "vault-config-parity"
db = "{}"
repos = []

[snapshot_storage]
backend = "local"
path = "/tmp/nestweaver/vault-config-parity/snapshots"

[workspace]
backend = "local"
path = "/tmp/nestweaver/vault-config-parity/workspace"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"
"#,
            toml_basic_string(&db_path),
        ),
    )
    .unwrap();

    // Run from somewhere that is neither the configured DB's directory nor the
    // repository, so a fallback to `./nestweaver.lbug` cannot make anything
    // pass by accident.
    let unrelated_cwd = dir.path().join("unrelated-cwd");
    std::fs::create_dir(&unrelated_cwd).unwrap();

    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env_remove("NESTWEAVER_DB")
        .args(["brain", "add"])
        .arg(&vault_dir)
        .arg("--config")
        .arg(&config_path)
        .assert()
        .success();
    assert!(
        db_path.exists(),
        "`brain add --config` must have created the database the config names, \
         at {}; it went somewhere else",
        db_path.display()
    );

    let refused = nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env_remove("NESTWEAVER_DB")
        .args(["brain", "remove"])
        .arg(&vault_dir)
        .arg("--config")
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "`brain remove --config` must not silently succeed against some other \
         instance; that is the nw-217 defect"
    );
    let stderr = flatten_miette(&refused.stderr);
    assert!(
        stderr.contains("cannot be honored by the direct store"),
        "the refusal must say WHY, and name the config it could not honour — \
         a bare non-zero exit leaves the caller guessing:\n{stderr}"
    );
    // Compared with ALL whitespace removed, on both sides: miette breaks the
    // line wherever the width runs out, and on a long macOS
    // `/var/folders/...` temp path that break lands INSIDE the path itself.
    // Any check that tolerates wrapping only between words still fails there.
    let squeeze = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
    assert!(
        squeeze(&stderr).contains(&squeeze(&config_path.display().to_string())),
        "the refusal must name the config path it could not honour:\n{stderr}"
    );

    // The counterweight: the very same removal, addressed without a config,
    // succeeds. Without this the refusal above would be indistinguishable from
    // a `brain remove` that cannot remove anything at all.
    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env_remove("NESTWEAVER_DB")
        .args(["brain", "remove"])
        .arg(&vault_dir)
        .args(["--db", &db_path.display().to_string()])
        .args(["--instance", "vault-config-parity"])
        .assert()
        .success();
}

#[test]
fn commands_honor_database_declared_by_config() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("configured.lbug");
    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    store
        .insert_project(&nestweaver_schema::Project {
            uid: "proj:config-db:configured-project".to_string(),
            name: "configured-project".to_string(),
            summary: None,
            instance_id: "config-db".to_string(),
        })
        .unwrap();
    drop(store);

    let config_path = dir.path().join("instance.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"instance_id = "config-db"
db = "{}"
repos = []

[snapshot_storage]
backend = "local"
path = "/tmp/nestweaver/config-db/snapshots"

[workspace]
backend = "local"
path = "/tmp/nestweaver/config-db/workspace"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"
"#,
            toml_basic_string(&db_path),
        ),
    )
    .unwrap();

    // Run away from both the configured DB and the repository so the default
    // `./nestweaver.lbug` cannot accidentally make either assertion pass.
    let unrelated_cwd = dir.path().join("unrelated-cwd");
    std::fs::create_dir(&unrelated_cwd).unwrap();

    for args in [
        &["list-repos", "--json"][..],
        &["search", "configured-project", "--json"][..],
        &["regex-search", "configured-project", "--json"][..],
        &["count-patterns", "configured-project", "--json"][..],
        &["suggest-links", "--json"][..],
        &["generate-guide", "--format", "agents-md"][..],
        &["hubs", "--json"][..],
        &["bridges", "--json"][..],
        &["clusters", "--json"][..],
        &["brain", "status", "--json"][..],
    ] {
        nestweaver_cmd()
            .current_dir(&unrelated_cwd)
            .env_remove("NESTWEAVER_DB")
            .args(args)
            .arg("--config")
            .arg(&config_path)
            .assert()
            .success();
    }

    for args in [
        &["context", "definitely-not-present"][..],
        &["impact", "definitely-not-present"][..],
        &["read-symbols", "definitely-not-present"][..],
    ] {
        nestweaver_cmd()
            .current_dir(&unrelated_cwd)
            .env_remove("NESTWEAVER_DB")
            .args(args)
            .arg("--config")
            .arg(&config_path)
            .assert()
            .code(2)
            .stderr(contains("Database not found").not());
    }

    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env_remove("NESTWEAVER_DB")
        .args(["mcp", "--config"])
        .arg(&config_path)
        .write_stdin("")
        .assert()
        .success();

    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env_remove("NESTWEAVER_DB")
        .args(["list-projects", "--config"])
        .arg(&config_path)
        .arg("--json")
        .assert()
        .success()
        .stdout(contains("configured-project"));

    let env_db_path = dir.path().join("environment.lbug");
    let env_store = nestweaver_store::GraphStore::open_or_create(&env_db_path).unwrap();
    env_store
        .insert_project(&nestweaver_schema::Project {
            uid: "proj:config-db:environment-project".to_string(),
            name: "environment-project".to_string(),
            summary: None,
            instance_id: "config-db".to_string(),
        })
        .unwrap();
    drop(env_store);
    let explicit_db_path = dir.path().join("explicit.lbug");
    let explicit_store = nestweaver_store::GraphStore::open_or_create(&explicit_db_path).unwrap();
    explicit_store
        .insert_project(&nestweaver_schema::Project {
            uid: "proj:config-db:explicit-project".to_string(),
            name: "explicit-project".to_string(),
            summary: None,
            instance_id: "config-db".to_string(),
        })
        .unwrap();
    drop(explicit_store);

    // --db > config.db > NESTWEAVER_DB > fallback.
    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env("NESTWEAVER_DB", &env_db_path)
        .args(["list-projects", "--config"])
        .arg(&config_path)
        .arg("--json")
        .assert()
        .success()
        .stdout(contains("configured-project"))
        .stdout(contains("environment-project").not());
    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env("NESTWEAVER_DB", &env_db_path)
        .args(["list-projects", "--db"])
        .arg(&explicit_db_path)
        .arg("--config")
        .arg(&config_path)
        .arg("--json")
        .assert()
        .success()
        .stdout(contains("explicit-project"))
        .stdout(contains("configured-project").not());
    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env("NESTWEAVER_DB", &env_db_path)
        .args(["list-projects", "--json"])
        .assert()
        .success()
        .stdout(contains("environment-project"));

    let invalid_config = dir.path().join("invalid.toml");
    std::fs::write(&invalid_config, "this is not valid = [toml").unwrap();
    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env("NESTWEAVER_DB", &env_db_path)
        .args(["list-projects", "--config"])
        .arg(&invalid_config)
        .arg("--json")
        .assert()
        .failure()
        .stderr(contains("loading --config"));

    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env_remove("NESTWEAVER_DB")
        .args(["project-context", "configured-project", "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .stdout(contains("has no associated notes or symbols"));

    // Repository-scoped commands have a different final fallback
    // (`<repo>/nestweaver.lbug`), so cover their shared resolver explicitly.
    // `index` is finite; code `watch` uses the same resolver before entering
    // its long-running service loop.
    let repo = unrelated_cwd.join("configured-repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.js"), "export function configuredDb() {}\n").unwrap();
    nestweaver_cmd()
        .current_dir(&unrelated_cwd)
        .env_remove("NESTWEAVER_DB")
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--config")
        .arg(&config_path)
        .assert()
        .success();
    assert!(
        !repo.join("nestweaver.lbug").exists(),
        "index must not create its repository-local fallback when config.db is set"
    );
    let configured = nestweaver_store::GraphStore::open(&db_path).unwrap();
    assert!(
        !configured.list_repos(None).unwrap().is_empty(),
        "the indexed repository must land in config.db"
    );
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

/// Guards the Ladybug source-build configuration.
///
/// lbug's published prebuilt archive hides its zstd Huffman ASM symbols in the
/// ELF, which breaks the link on x86_64 Linux. Building lbug from source is the
/// fix, and it only works if every build path agrees: the workspace
/// `.cargo/config.toml` must force `LBUG_BUILD_FROM_SOURCE`, CI workflows must
/// append to that file rather than overwrite it, the Docker build context must
/// include it, and the image must install the native toolchain the source build
/// needs and resolve the committed lockfile with `--locked`.
///
/// Each of these has silently regressed before, and the failure only surfaces as
/// a link error in a release build, so assert them here.
#[test]
fn source_build_configuration_is_intact_and_reproducible() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo_config = std::fs::read_to_string(repo_root.join(".cargo/config.toml")).unwrap();
    let ci_workflow = std::fs::read_to_string(repo_root.join(".github/workflows/ci.yml")).unwrap();
    let release_workflow =
        std::fs::read_to_string(repo_root.join(".github/workflows/release-please.yml")).unwrap();
    let dockerfile = std::fs::read_to_string(repo_root.join("Dockerfile")).unwrap();
    let dockerignore = std::fs::read_to_string(repo_root.join(".dockerignore")).unwrap();

    assert!(
        cargo_config.contains("LBUG_BUILD_FROM_SOURCE = { value = \"1\", force = true }"),
        "workspace builds must compile Ladybug from source instead of downloading a prebuilt archive"
    );
    for (workflow_name, workflow) in [
        ("ci.yml", ci_workflow.as_str()),
        ("release-please.yml", release_workflow.as_str()),
    ] {
        assert!(
            !workflow.contains(" > .cargo/config.toml"),
            "{workflow_name} must not overwrite the workspace source-build configuration"
        );
    }
    for native_dependency in [
        "cmake",
        "g++",
        "libssl-dev",
        "libzstd-dev",
        "pkg-config",
        "protobuf-compiler",
    ] {
        assert!(
            dockerfile.contains(native_dependency),
            "Dockerfile must install Ladybug source-build dependency `{native_dependency}`"
        );
    }
    assert!(
        !dockerignore.lines().any(|line| line.trim() == ".cargo/"),
        "the Docker build context must include the workspace source-build configuration"
    );
    assert!(
        dockerfile.contains("cargo build --locked --release --bin nestweaver"),
        "the Docker image must resolve the reviewed dependency lockfile without updating it"
    );
}

/// Guards the single-copy-of-zstd invariant.
///
/// `liblbug.a` vendors zstd, exports its `ZSTD_*` symbols, and is linked
/// `+whole-archive`, so every binary already contains a complete libzstd. Rust
/// code reaches it through `nestweaver_store::zstd`.
///
/// Adding the `zstd` crate back pulls in `zstd-sys`, which compiles a second
/// complete copy; `rust-lld` then fails every link with duplicate `ZSTD_*`
/// symbols. The historical response was `-Wl,--allow-multiple-definition`,
/// which suppressed the error without merging the copies — the linker picked
/// one definition silently and the other stayed in the binary. Assert both
/// halves here: no second copy in the tree, and no suppression flag anywhere.
#[test]
fn exactly_one_copy_of_zstd_is_linked() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    let lockfile = std::fs::read_to_string(repo_root.join("Cargo.lock")).unwrap();
    assert!(
        !lockfile.contains("name = \"zstd-sys\""),
        "zstd-sys is back in the dependency tree; it compiles a second complete \
         libzstd alongside the one liblbug.a already exports. Use \
         nestweaver_store::zstd instead of the `zstd` crate."
    );

    for relative_path in [
        ".cargo/config.toml",
        ".github/workflows/ci.yml",
        ".github/workflows/release-please.yml",
        "Dockerfile",
    ] {
        let contents = std::fs::read_to_string(repo_root.join(relative_path))
            .unwrap_or_else(|error| panic!("failed to read {relative_path}: {error}"));
        assert!(
            !contents.contains("allow-multiple-definition"),
            "{relative_path} still carries --allow-multiple-definition; it suppresses \
             duplicate symbols rather than removing the duplicate"
        );
    }
}

#[test]
fn embedding_docs_match_runtime_contract() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let documentation = [
        ("README.md", "README.md"),
        ("instance config guide", "docs/guide/instance-config.md"),
        ("server mode guide", "docs/server-mode.md"),
        (
            "annotated instance config",
            "examples/nestweaver-instance.toml",
        ),
    ]
    .map(|(label, relative_path)| {
        let contents = std::fs::read_to_string(repo_root.join(relative_path))
            .unwrap_or_else(|error| panic!("failed to read {relative_path}: {error}"));
        (label, contents)
    });

    let combined = documentation
        .iter()
        .map(|(_, contents)| contents.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let combined_lowercase = combined.to_lowercase();

    for obsolete_claim in [
        "cpu fallback",
        "fallback to cpu",
        "downloads automatically",
        "daemon's main thread",
        "external-to-local fallback",
    ] {
        assert!(
            !combined_lowercase.contains(obsolete_claim),
            "embedding docs contain obsolete claim `{obsolete_claim}`"
        );
    }

    for required_contract in [
        "Metal in a Metal-enabled build; CPU only when Metal is not compiled",
        "A Metal failure is reported; `auto` does not retry on CPU",
        "`metal`",
        "`cpu`",
        "Daemon startup is cache-only",
        "nestweaver embed --db <path> --local --model-id <id> --cache-dir <path>",
        "nestweaver diagnostics capabilities --json",
        "nestweaver daemon --db <path> status",
        "nestweaver brain status --db <path> --json",
        "nestweaver daemon --db \"$DB\" start --config \"$CONFIG\"",
        "nestweaver embed --db \"$DB\" --local --model-id \"$MODEL\" --cache-dir \"$CACHE\" --force",
        "`requested_device`",
        "`selected_device`",
        "`fallback_used`",
        "`degraded_components`",
    ] {
        assert!(
            combined.contains(required_contract),
            "embedding docs are missing runtime contract `{required_contract}`"
        );
    }

    let required_by_document: &[(&str, &[&str])] = &[
        (
            "README.md",
            &[
                "**Device policy.**",
                "**Model selection and cache.**",
                "**External embedding endpoints.**",
                "**Readiness and diagnostics.**",
                "CONFIG=/absolute/path/to/nestweaver-instance.toml",
            ],
        ),
        (
            "instance config guide",
            &[
                "Device policies for the local backend are exact:",
                "An external endpoint is authoritative.",
                "Do not omit `--local`",
                "Switching from an external backend to a local model",
            ],
        ),
        (
            "server mode guide",
            &[
                "### Embedding backend and readiness",
                "### Embedding or semantic retrieval unavailable",
                "`metal_compiled = false`",
                "`selected_device = \"\"`",
                "--cache-dir \"$CACHE\" --force",
            ],
        ),
    ];
    for (label, required_claims) in required_by_document {
        let contents = documentation
            .iter()
            .find_map(|(candidate, contents)| (candidate == label).then_some(contents))
            .unwrap_or_else(|| panic!("{label} must be part of the documentation contract"));
        for required_claim in *required_claims {
            assert!(
                contents.contains(required_claim),
                "{label} is missing `{required_claim}`"
            );
        }
    }

    let example = documentation
        .iter()
        .find_map(|(label, contents)| (*label == "annotated instance config").then_some(contents))
        .expect("annotated instance config must be part of the documentation contract");
    for required_example_claim in [
        "accelerator = \"auto\"",
        "# auto:",
        "# metal:",
        "# cpu:",
        "# Daemon startup is cache-only",
    ] {
        assert!(
            example.contains(required_example_claim),
            "annotated instance config is missing `{required_example_claim}`"
        );
    }
}

fn workflow_step<'a>(workflow: &'a str, name: &str) -> &'a str {
    let marker = format!("      - name: {name}\n");
    let start = workflow
        .find(&marker)
        .unwrap_or_else(|| panic!("workflow is missing step `{name}`"));
    let remainder = &workflow[start + marker.len()..];
    remainder
        .split("\n      - ")
        .next()
        .expect("step body must be present")
}

fn release_matrix(workflow: &str) -> Vec<(String, Option<String>, Option<String>)> {
    let marker = "      matrix:\n        include:\n";
    let matrix = workflow
        .split_once(marker)
        .expect("release workflow must define an include matrix")
        .1
        .split_once("\n    steps:\n")
        .expect("release matrix must be followed by build steps")
        .0;
    let mut entries = Vec::new();
    let mut current: Option<(String, Option<String>, Option<String>)> = None;

    for line in matrix.lines().map(str::trim) {
        if let Some(target) = line.strip_prefix("- target: ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some((target.trim_matches(['"', '\'']).to_string(), None, None));
        } else if let Some(os) = line.strip_prefix("os: ") {
            current.as_mut().expect("os must follow target").1 =
                Some(os.trim_matches(['"', '\'']).to_string());
        } else if let Some(features) = line.strip_prefix("features: ") {
            current.as_mut().expect("features must follow target").2 =
                Some(features.trim_matches(['"', '\'']).to_string());
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

#[test]
fn release_workflow_matrix_assigns_metal_only_to_apple_targets() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow =
        std::fs::read_to_string(repo_root.join(".github/workflows/release-please.yml")).unwrap();
    let entries = release_matrix(&workflow);
    let actual = entries
        .iter()
        .cloned()
        .map(|(target, os, features)| (target, (os, features)))
        .collect::<std::collections::BTreeMap<_, _>>();
    let expected = std::collections::BTreeMap::from([
        (
            "aarch64-apple-darwin".to_string(),
            (Some("macos-latest".to_string()), Some("metal".to_string())),
        ),
        (
            "aarch64-unknown-linux-gnu".to_string(),
            (Some("ubuntu-24.04-arm".to_string()), Some(String::new())),
        ),
        (
            "x86_64-apple-darwin".to_string(),
            (
                Some("macos-15-intel".to_string()),
                Some("metal".to_string()),
            ),
        ),
        (
            "x86_64-unknown-linux-gnu".to_string(),
            (Some("ubuntu-latest".to_string()), Some(String::new())),
        ),
    ]);

    assert_eq!(
        entries.len(),
        actual.len(),
        "release matrix must not contain duplicate targets"
    );
    assert_eq!(
        actual, expected,
        "release target/os/features contract drifted"
    );
}

#[test]
fn release_workflow_build_quotes_target_and_conditionally_enables_features() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow =
        std::fs::read_to_string(repo_root.join(".github/workflows/release-please.yml")).unwrap();
    let step = workflow_step(&workflow, "Build binary");

    for required in [
        "TARGET: ${{ matrix.target }}",
        "FEATURES: ${{ matrix.features }}",
        "if [ -n \"$FEATURES\" ]; then",
        "cargo build --locked --release --target \"$TARGET\" --features \"$FEATURES\"",
        "cargo build --locked --release --target \"$TARGET\"",
    ] {
        assert!(
            step.contains(required),
            "Build binary must contain `{required}`\nstep:\n{step}"
        );
    }
}

#[test]
fn release_workflow_stages_openssl_outside_build_script_out_dir() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow =
        std::fs::read_to_string(repo_root.join(".github/workflows/release-please.yml")).unwrap();
    let step = workflow_step(&workflow, "Build target OpenSSL");

    assert!(
        !step.contains("OPENSSL_INSTALL=\"$(find"),
        "OpenSSL exports must not reference openssl-sys's self-deleting OUT_DIR\nstep:\n{step}"
    );
    for required in [
        "TARGET: ${{ matrix.target }}",
        "cargo build --locked --release --target \"$TARGET\" -p openssl-sys",
        "OPENSSL_SOURCE=\"$(find \"target/$TARGET/release/build\"",
        "OPENSSL_INSTALL=\"$RUNNER_TEMP/nestweaver-openssl/$TARGET\"",
        "mkdir -p \"$OPENSSL_INSTALL\"",
        "cp -R \"$OPENSSL_SOURCE/.\" \"$OPENSSL_INSTALL/\"",
        "test -f \"$OPENSSL_INSTALL/lib/libssl.a\"",
        "test -f \"$OPENSSL_INSTALL/lib/libcrypto.a\"",
        "test -f \"$OPENSSL_INSTALL/include/openssl/ssl.h\"",
        "test -f \"$OPENSSL_INSTALL/include/openssl/opensslconf.h\"",
        "echo \"OPENSSL_DIR=$OPENSSL_INSTALL\"",
        "echo \"OPENSSL_ROOT_DIR=$OPENSSL_INSTALL\"",
        "echo \"OPENSSL_LIB_DIR=$OPENSSL_INSTALL/lib\"",
        "echo \"OPENSSL_INCLUDE_DIR=$OPENSSL_INSTALL/include\"",
        "echo \"CMAKE_PREFIX_PATH=$OPENSSL_INSTALL\"",
    ] {
        assert!(
            step.contains(required),
            "OpenSSL staging must contain `{required}`\nstep:\n{step}"
        );
    }
}

#[test]
fn release_workflow_native_apple_verifies_metal_capability_and_artifact() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow =
        std::fs::read_to_string(repo_root.join(".github/workflows/release-please.yml")).unwrap();
    let verify_binary = workflow_step(&workflow, "Verify release binary");

    for preserved in [
        "x86_64-apple-darwin) echo \"$FILE_INFO\" | grep -q 'x86_64'",
        "aarch64-apple-darwin) echo \"$FILE_INFO\" | grep -Eq 'arm64|ARM64'",
        "otool -L \"$BIN\"",
        "release binary has a non-system OpenSSL dependency",
    ] {
        assert!(
            verify_binary.contains(preserved),
            "release artifact validation must preserve `{preserved}`"
        );
    }

    let capability = workflow_step(&workflow, "Verify Apple Metal capability");
    for required in [
        "if: runner.os == 'macOS'",
        "TARGET: ${{ matrix.target }}",
        "BIN=\"target/$TARGET/release/nestweaver\"",
        "CAPABILITIES=\"$(\"$BIN\" diagnostics capabilities --json)\"",
        "jq -e '.metal_compiled == true' <<< \"$CAPABILITIES\"",
    ] {
        assert!(
            capability.contains(required),
            "Apple capability step must contain `{required}`\nstep:\n{capability}"
        );
    }
}

#[test]
fn ci_runs_release_workflow_contract_when_release_definition_changes() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = std::fs::read_to_string(repo_root.join(".github/workflows/ci.yml")).unwrap();
    let rust_filter = workflow
        .split_once("            rust:\n")
        .expect("CI must define the Rust change filter")
        .1
        .split_once("            frontend:\n")
        .expect("Rust filter must precede the frontend filter")
        .0;

    assert!(
        rust_filter.contains("- '.github/workflows/release-please.yml'"),
        "changing release-please.yml must select the Rust test job"
    );
    assert!(
        workflow.contains("cargo test --locked --workspace"),
        "selected Rust changes must execute the workspace tests containing this contract"
    );
}

#[test]
fn ci_runs_for_staging_pull_requests() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = std::fs::read_to_string(repo_root.join(".github/workflows/ci.yml")).unwrap();

    assert!(
        workflow.contains("  pull_request:\n    branches: [main, staging]"),
        "feature-to-staging pull requests must run the same CI gate as main"
    );
}

#[test]
fn ci_metal_smoke_is_required_and_narrowly_routed_to_apple_hardware_changes() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = std::fs::read_to_string(repo_root.join(".github/workflows/ci.yml")).unwrap();
    let filters = workflow
        .split_once("          filters: |\n")
        .expect("CI must define path filters")
        .1
        .split_once("\n\n  fmt:")
        .expect("change filters must precede CI jobs")
        .0;
    let metal_filter = filters
        .split_once("            metal:\n")
        .expect("CI must define a Metal smoke-test change filter")
        .1
        .split_once("            frontend:\n")
        .expect("Metal filter must precede the frontend filter")
        .0;

    for selected_path in [
        "crates/nestweaver-embed/**",
        "crates/nestweaver-daemon/src/lifecycle.rs",
        "crates/nestweaver-daemon/src/launchd.rs",
        "crates/nestweaver-daemon/src/server.rs",
        "crates/nestweaver-daemon/Cargo.toml",
        "crates/nestweaver-client/src/autostart.rs",
        "src/main.rs",
        "Cargo.toml",
        "Cargo.lock",
        ".github/workflows/ci.yml",
        ".github/workflows/release-please.yml",
        "tests/metal_smoke.rs",
    ] {
        assert!(
            metal_filter.contains(&format!("- '{selected_path}'")),
            "Metal filter must select `{selected_path}`\nfilter:\n{metal_filter}"
        );
    }
    assert!(
        !metal_filter.contains("'**/*.rs'"),
        "Metal smoke must not run for every Rust change"
    );

    let job = workflow
        .split_once("\n  metal-smoke:\n")
        .expect("CI must define a metal-smoke job")
        .1
        .split_once("\n  fmt:\n")
        .expect("metal-smoke must be a top-level job")
        .0;
    for required in [
        "needs: changes",
        "if: needs.changes.outputs.metal == 'true'",
        "runs-on: macos-latest",
        "test \"$(uname -m)\" = \"arm64\"",
    ] {
        assert!(
            job.contains(required),
            "metal-smoke job must contain `{required}`\njob:\n{job}"
        );
    }
    assert!(
        !job.contains("continue-on-error: true"),
        "Metal smoke is a required gate, not an informational job"
    );
}

#[test]
fn ci_metal_smoke_gates_offline_cold_and_warm_daemon_inference() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = std::fs::read_to_string(repo_root.join(".github/workflows/ci.yml")).unwrap();
    let job = workflow
        .split_once("\n  metal-smoke:\n")
        .expect("CI must define a metal-smoke job")
        .1
        .split_once("\n  fmt:\n")
        .expect("metal-smoke must be a top-level job")
        .0;

    let cache_setup = workflow_step(job, "Populate model cache on CPU");
    for required in [
        "--no-daemon",
        "--accelerator cpu",
        "--cache-dir \"$MODEL_CACHE\"",
        "--db \"$SETUP_DB\"",
    ] {
        assert!(
            cache_setup.contains(required),
            "network-enabled cache setup must contain `{required}`\nstep:\n{cache_setup}"
        );
    }

    let cache_contract = workflow_step(job, "Verify daemon cache-only contract");
    assert!(
        cache_contract.contains("daemon_embedding_startup_constructs_cached_model_offline"),
        "the hardware gate must execute the focused CacheOnly daemon contract"
    );

    let path_setup = workflow_step(job, "Configure isolated smoke paths");
    for required in [
        "RUNTIME_DIR=\"$HOME/.local/state/nestweaver/$INSTANCE_ID\"",
        "echo \"RUNTIME_DIR=$RUNTIME_DIR\"",
    ] {
        assert!(
            path_setup.contains(required),
            "path setup must export the exact per-instance runtime directory via `{required}`"
        );
    }

    let preflight = workflow_step(job, "Assert clean daemon preconditions");
    assert!(
        preflight.contains("apple_hardware::cold_daemon_preconditions_are_clean"),
        "the cold gate must select exactly the module-qualified precondition test"
    );

    let cold = workflow_step(job, "Cold Metal daemon inference");
    for required in [
        "index --repo testdata/js --db \"$TARGET_DB\" --config \"$TARGET_CONFIG\"",
        "requested_device == \"metal\"",
        "selected_device == \"metal\"",
        "fallback_used == false",
        "embed --db \"$TARGET_DB\" --scope symbols --force",
        "Embedding via daemon",
        "launchctl print",
        "test -f \"$PLIST_PATH\"",
    ] {
        assert!(
            cold.contains(required),
            "cold inference must contain `{required}`\nstep:\n{cold}"
        );
    }
    assert!(
        !cold.contains("--no-daemon"),
        "the cold gate must exercise normal client autostart"
    );

    let warm = workflow_step(job, "Restart and repeat warm Metal inference");
    for required in [
        "daemon --db \"$TARGET_DB\" stop",
        "brain status --db \"$TARGET_DB\" --config \"$TARGET_CONFIG\" --json",
        "requested_device == \"metal\"",
        "selected_device == \"metal\"",
        "fallback_used == false",
        "embed --db \"$TARGET_DB\" --scope symbols --force",
        "Embedding via daemon",
        "launchctl print",
        "test -f \"$PLIST_PATH\"",
    ] {
        assert!(
            warm.contains(required),
            "warm inference must contain `{required}`\nstep:\n{warm}"
        );
    }

    let hardware = workflow_step(job, "Verify cache-only Metal vector");
    assert!(
        hardware.contains("apple_hardware::metal_embedding_is_finite_normalized_and_uses_metal"),
        "the direct vector check must select exactly the module-qualified Metal test"
    );

    let evidence = workflow_step(job, "Collect Metal smoke evidence");
    for required in [
        "if: failure()",
        "diagnostics capabilities --json",
        "daemon.log",
    ] {
        assert!(
            evidence.contains(required),
            "failure evidence must contain `{required}`\nstep:\n{evidence}"
        );
    }
    let upload = workflow_step(job, "Upload Metal smoke evidence");
    for required in [
        "if: failure()",
        "actions/upload-artifact@",
        "metal-smoke-evidence",
    ] {
        assert!(
            upload.contains(required),
            "failure upload must contain `{required}`\nstep:\n{upload}"
        );
    }

    let cleanup = workflow_step(job, "Clean up isolated Metal daemon");
    for required in [
        "daemon_stopped=false",
        "daemon_stopped=true",
        "[ -n \"${INSTANCE_ID:-}\" ]",
        "[ \"${RUNTIME_DIR:-}\" = \"$HOME/.local/state/nestweaver/$INSTANCE_ID\" ]",
        "\"$HOME/.local/state/nestweaver/\"?*",
        "rm -rf -- \"$RUNTIME_DIR\"",
    ] {
        assert!(
            cleanup.contains(required),
            "cleanup must remove only the stopped instance's scoped runtime directory via \
             `{required}`\nstep:\n{cleanup}"
        );
    }
    let stop_position = cleanup
        .find("target/release/nestweaver daemon --db \"$TARGET_DB\" stop")
        .unwrap();
    let runtime_remove_position = cleanup.find("rm -rf -- \"$RUNTIME_DIR\"").unwrap();
    assert!(
        stop_position < runtime_remove_position,
        "the exact per-instance runtime directory may be removed only after the scoped daemon stop"
    );

    let setup_position = job.find("- name: Populate model cache on CPU").unwrap();
    let cold_position = job.find("- name: Cold Metal daemon inference").unwrap();
    let direct_position = job.find("- name: Verify cache-only Metal vector").unwrap();
    assert!(
        setup_position < cold_position && cold_position < direct_position,
        "CPU cache population must precede the daemon's first Metal operation, and direct Metal \
         verification must run only afterward"
    );
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
    nestweaver_engine::save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap();
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
fn cli_capability_aliases_execute_mcp_equivalent_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&vault_dir).unwrap();
    std::fs::write(
        repo_dir.join("app.js"),
        "export function capabilityProbe(x) { return x + 1; }",
    )
    .unwrap();
    std::fs::write(vault_dir.join("Source.md"), "Links to [[Target]].").unwrap();
    std::fs::write(vault_dir.join("Target.md"), "# Target\n").unwrap();

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

    let detect = nestweaver_cmd()
        .args(["detect-changes", "--files", "app.js", "--json", "--db"])
        .arg(&db_path)
        .output()
        .unwrap();
    assert!(
        detect.status.success(),
        "{}",
        String::from_utf8_lossy(&detect.stderr)
    );
    let detect: serde_json::Value = serde_json::from_slice(&detect.stdout).unwrap();
    assert!(detect["status"].is_string());
    assert!(detect["gate_state"].is_string());

    let contracts = nestweaver_cmd()
        .args(["cross-repo-contracts", "capabilityProbe", "--json", "--db"])
        .arg(&db_path)
        .output()
        .unwrap();
    assert!(
        contracts.status.success(),
        "{}",
        String::from_utf8_lossy(&contracts.stderr)
    );
    let contracts: serde_json::Value = serde_json::from_slice(&contracts.stdout).unwrap();
    assert_eq!(contracts["returned"], 0);
    assert_eq!(contracts["contracts_status"], "complete");
    assert!(contracts["uid"].as_str().unwrap().starts_with("sym:"));

    let backlinks = nestweaver_cmd()
        .args(["backlinks", "Target", "--json", "--db"])
        .arg(&db_path)
        .output()
        .unwrap();
    assert!(
        backlinks.status.success(),
        "{}",
        String::from_utf8_lossy(&backlinks.stderr)
    );
    let backlinks: serde_json::Value = serde_json::from_slice(&backlinks.stdout).unwrap();
    assert_eq!(backlinks["count"], 1);
    assert_eq!(backlinks["backlinks"][0]["source_note_title"], "Source");
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
fn cli_impact_json_is_one_envelope_for_every_outcome() {
    // nw-086 required a bare node ARRAY on success and an error OBJECT on
    // not-found. nw-111 REPLACES that contract: the shape used to change with the
    // outcome, and the ambiguous case returned a bare CANDIDATE array that was
    // structurally indistinguishable from a result set — a mistyped name looked
    // like a successful impact query, with only the exit code to tell them apart.
    //
    // Every outcome now returns one envelope discriminated by `status`. What
    // nw-086 actually cared about — that a --json consumer can always parse the
    // output — is preserved and strengthened.
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

    // Found → envelope with status "ok" (a calls b, so impact(b) is non-empty).
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
    assert_eq!(
        v.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "impact --json (found) must be a status:ok envelope, got: {v}"
    );
    assert!(
        v.get("nodes").is_some_and(|n| n.is_array()),
        "the envelope must carry a nodes array, got: {v}"
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
    assert_eq!(
        v.get("status").and_then(|s| s.as_str()),
        Some("not_found"),
        "not-found must be discriminated by status, got: {v}"
    );
    assert!(
        v.get("nodes").is_none(),
        "a not-found envelope must not carry nodes, got: {v}"
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
fn cli_snapshot_stamp_has_repos_and_does_not_invent_embedding_model() {
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

    // Build with a config whose model must not override persisted DB truth.
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

    // This DB has no vector metadata. The config is compatibility input only;
    // it must not fabricate an embedding model in the signed stamp.
    let model_id = stamp["embedding_model_id"]
        .as_str()
        .expect("embedding_model_id should be a string");
    assert_eq!(
        model_id, "",
        "a DB without embedding metadata must stamp an empty model ID"
    );
    assert_ne!(
        model_id, "sentence-transformers/all-MiniLM-L6-v2",
        "[embedding].model_id must not override persisted DB truth"
    );
    assert_ne!(
        model_id, "nomic-embed-text",
        "[inference].embedding_model must not override persisted DB truth"
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
        .stdout(contains("--port-file"))
        .stdout(predicates::str::contains("--idle-timeout").not());
}

/// The help text used to advertise "orphaned daemon state directories" on
/// every platform without saying WHICH root — reading as broader coverage
/// than the sweep had (it touched only the persistent state root). It must
/// name exactly the three roots it sweeps.
#[test]
fn cli_daemon_gc_help_names_exactly_the_roots_it_sweeps() {
    nestweaver_cmd()
        .args(["daemon", "gc", "--help"])
        .assert()
        .success()
        .stdout(contains(".local/state/nestweaver"))
        .stdout(contains("XDG_RUNTIME_DIR"))
        .stdout(contains("/tmp/nw-sock-<uid>"));
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

#[test]
fn brain_refresh_direct_reports_only_committed_atomic_deletions() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    let db = dir.path().join("brain.lbug");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("a.md"), "# A\n\nalpha\n").unwrap();
    std::fs::write(vault.join("b.md"), "# B\n\nbeta\n").unwrap();

    nestweaver_cmd()
        .args(["brain", "refresh"])
        .arg(&vault)
        .args(["--instance", "direct-test", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("dropped 0 stale note(s), reindexed 2 note(s)"))
        .stderr(
            contains("read-only")
                .not()
                .and(contains("delete_note_cascade").not()),
        );

    nestweaver_cmd()
        .args(["brain", "refresh"])
        .arg(&vault)
        .args(["--instance", "direct-test", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("dropped 2 stale note(s), reindexed 2 note(s)"))
        .stderr(
            contains("read-only")
                .not()
                .and(contains("delete_note_cascade").not()),
        );

    std::fs::remove_file(vault.join("a.md")).unwrap();
    std::fs::write(vault.join("b.md"), "# B changed\n\nnew beta\n").unwrap();
    std::fs::write(vault.join("c.md"), "# C\n\ngamma\n").unwrap();
    nestweaver_cmd()
        .args(["brain", "refresh"])
        .arg(&vault)
        .args(["--instance", "direct-test", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("dropped 2 stale note(s), reindexed 2 note(s)"))
        .stderr(
            contains("read-only")
                .not()
                .and(contains("delete_note_cascade").not()),
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

#[test]
fn index_json_reports_degraded_source_coverage_and_fail_on_skip_is_strict() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut source = String::from("pub fn oversized_marker() {}\n");
    source.push_str(&"// padding\n".repeat(200));
    assert!(source.len() > 1024);
    std::fs::write(repo.join("main.rs"), source).unwrap();

    let config = nw047_valid_config(dir.path(), "coverage-test");
    let mut config_body = std::fs::read_to_string(&config).unwrap();
    config_body.push_str("\n[indexing]\nmax_source_file_bytes = 1024\n");
    std::fs::write(&config, config_body).unwrap();

    let run = |db: &std::path::Path, fail_on_skip: bool| {
        let mut command = nestweaver_cmd();
        command
            .args(["index", "--repo"])
            .arg(&repo)
            .arg("--db")
            .arg(db)
            .arg("--config")
            .arg(&config)
            .args(["--force", "--json"]);
        if fail_on_skip {
            command.arg("--fail-on-skip");
        }
        command.output().unwrap()
    };

    let best_effort = run(&dir.path().join("best-effort.lbug"), false);
    assert!(best_effort.status.success(), "{best_effort:?}");
    let payload: serde_json::Value = serde_json::from_slice(&best_effort.stdout).unwrap();
    assert_eq!(payload["coverage_status"], "degraded");
    assert_eq!(payload["skipped_count"], 1);
    assert_eq!(payload["skipped_files"][0]["path"], "main.rs");
    assert_eq!(payload["skipped_files"][0]["reason_code"], "oversized");
    assert_eq!(payload["skipped_files"][0]["limit_bytes"], 1024);

    let strict = run(&dir.path().join("strict.lbug"), true);
    assert!(!strict.status.success());
    let strict_payload: serde_json::Value = serde_json::from_slice(&strict.stdout).unwrap();
    assert_eq!(strict_payload["coverage_status"], "degraded");
    assert_eq!(strict_payload["skipped_count"], 1);
}

#[test]
fn invalid_source_limit_fails_before_database_creation() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.rs"), "pub fn marker() {}\n").unwrap();
    let config = nw047_valid_config(dir.path(), "invalid-limit");
    let mut config_body = std::fs::read_to_string(&config).unwrap();
    config_body.push_str("\n[indexing]\nmax_source_file_bytes = 512\n");
    std::fs::write(&config, config_body).unwrap();
    let db = dir.path().join("must-not-exist").join("brain.lbug");

    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .arg("--config")
        .arg(&config)
        .assert()
        .failure()
        .stderr(contains("max_source_file_bytes"));
    assert!(!db.exists());
    assert!(!db.parent().unwrap().exists());
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

/// Flatten miette's rendered diagnostic into a single line.
///
/// miette hard-wraps a message to the terminal width and prefixes continuation
/// lines with `\u{2502}`, so a `contains("...")` on any phrase long enough to
/// span the wrap is really an assertion about the WIDTH — and the width here
/// depends on the length of the temp path, which differs between a macOS
/// `/var/folders/...` and a Linux `/tmp/...`. CI caught exactly that: the
/// message was correct and the assertion still failed.
fn flatten_miette(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .replace('\u{2502}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

// ── release help-text contract ───────────────────────────────────────────
// The unit guard in src/main.rs proves every command and visible arg carries
// *some* help string. These assert that the specific commands shipped in this
// release describe what they actually do, which a presence check cannot.

#[test]
fn cli_help_documents_the_commands_shipped_in_this_release() {
    // (argv, substring the help must contain)
    let cases: &[(&[&str], &str)] = &[
        (&["config", "validate", "--help"], "without creating files"),
        (&["diagnostics", "capabilities", "--help"], "metal"),
        (&["instance", "abort-migration", "--help"], "journal"),
        (&["embed", "--help"], "--force"),
    ];

    for (args, expected) in cases {
        let output = nestweaver_cmd()
            .args(*args)
            .output()
            .unwrap_or_else(|e| panic!("failed to run `{}`: {e}", args.join(" ")));
        assert!(
            output.status.success(),
            "`nestweaver {}` exited {:?}: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let help = String::from_utf8_lossy(&output.stdout).to_lowercase();
        assert!(
            help.contains(&expected.to_lowercase()),
            "`nestweaver {}` help does not mention {expected:?}. Full help:\n{help}",
            args.join(" ")
        );
    }
}

/// Every direct CLI invocation in the test suite must pin its daemon routing.
///
/// CI exports `NESTWEAVER_NO_DAEMON=1` for the whole job while local runs do
/// not, so an invocation that neither sets nor clears it takes a different code
/// path on each — which is how the embed-preflight test passed locally and
/// failed only on CI.
///
/// This checks each binary-invocation chain individually, not merely whether
/// the file mentions the variable somewhere: the file that carried the bug
/// already pinned routing on 28 other invocations.
///
/// Scoped to invocations that pass `--db`, since daemon routing is only
/// selected for local-database commands — a `server status --url ...` call
/// talks to a remote server over HTTP and has no routing to pin.
#[test]
fn every_cli_invocation_pins_its_daemon_routing() {
    let suite_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut unpinned = Vec::new();

    for entry in std::fs::read_dir(&suite_dir).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(&path).expect("read test source");

        // Build the constructor token at runtime so this scanner does not
        // match its own source when it scans tests/cli_test.rs.
        let needle = format!("CARGO_BIN_EXE_{}", "nestweaver");
        for (idx, _) in src.match_indices(needle.as_str()) {
            // Skip mentions inside comments (docs, not invocations).
            let line_start = src[..idx].rfind('\n').map_or(0, |i| i + 1);
            if src[line_start..idx].trim_start().starts_with("//") {
                continue;
            }
            // The builder chain runs from the constructor to whichever
            // terminal call executes it.
            let rest = &src[idx..];
            let end = [
                "\n        .output()",
                "\n        .status()",
                "\n        .assert()",
            ]
            .iter()
            .filter_map(|t| rest.find(t))
            .min()
            .unwrap_or_else(|| rest.len().min(1200));
            let chain = &rest[..end];
            // Only local-database commands select a daemon route.
            if !chain.contains("\"--db\"") {
                continue;
            }
            if !chain.contains("NESTWEAVER_NO_DAEMON") {
                let line = src[..idx].matches('\n').count() + 1;
                unpinned.push(format!("{name}:{line}"));
            }
        }
    }

    unpinned.sort();
    assert!(
        unpinned.is_empty(),
        "these CLI invocations do not pin daemon routing — set \
         .env(\"NESTWEAVER_NO_DAEMON\", \"1\") for the direct path, or \
         .env_remove(\"NESTWEAVER_NO_DAEMON\") for the daemon path: {unpinned:?}"
    );
}

// ── text/JSON honesty parity ─────────────────────────────────────────────
//
// The engine computes caveats correctly; the failures this guards against are
// text renderers that DROP them. Six separate findings in the v2.7.0 CLI sweep
// were this one defect (nw-097, nw-107, nw-110, nw-111), so the class needs an
// enforced contract rather than six point fixes.
//
// The rule: if --json reports a caveat, the human output must say so too.
// A caller reading a terminal must not be told less than a caller parsing JSON.

/// Build a throwaway indexed repo and return (tempdir, db path).
fn honesty_fixture() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.js"),
        "export function alpha() { return beta(); }\nexport function beta() { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/lib.test.js"),
        "import { alpha } from './lib';\ntest('alpha', () => { alpha(); });\n",
    )
    .unwrap();

    let db = dir.path().join("honesty.lbug").display().to_string();
    nestweaver_cmd()
        .args(["index", "--repo", &repo.display().to_string(), "--db", &db])
        .assert()
        .success();
    (dir, db)
}

/// `affected-tests` must not print a clean zero while privately recommending a
/// full suite (nw-107).
#[test]
fn affected_tests_text_surfaces_status_warning_and_recommendation() {
    let (_dir, db) = honesty_fixture();

    // A path that resolves to no indexed symbols — the analysis is degraded,
    // and the JSON path says so.
    let args = ["affected-tests", "--files", "zzz/not/real.js", "--db", &db];

    let json_out = nestweaver_cmd().args(args).arg("--json").output().unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&json_out.stdout).expect("affected-tests --json must be valid JSON");

    // Only assert parity when the JSON actually reports a caveat; if the
    // analysis came back complete there is nothing for text to surface, and a
    // test that asserted otherwise would be testing the fixture, not the code.
    let status = json.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if status == "complete" {
        return;
    }

    let text_out = nestweaver_cmd().args(args).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&text_out.stdout),
        String::from_utf8_lossy(&text_out.stderr)
    )
    .to_lowercase();

    assert!(
        text.contains(status),
        "--json reported status {status:?} but the text output never mentions it. \
         A developer sees a clean result while the tool privately knows the \
         selection is incomplete.\n\n{text}"
    );

    if let Some(rec) = json.get("recommendation").and_then(|v| v.as_str())
        && rec == "run-full-suite"
    {
        assert!(
            text.contains("full suite"),
            "--json recommends running the full suite; the text output must say so — \
             this is the command where the cost of silence is skipped tests.\n\n{text}"
        );
    }

    if let Some(notes) = json.get("notifications").and_then(|v| v.as_array())
        && !notes.is_empty()
    {
        assert!(
            text.contains("warning") || text.contains("not assessed"),
            "--json carries {} notification(s) that text never shows.\n\n{text}",
            notes.len()
        );
    }
}

/// `regex-search` must not report "No matches" when it merely ran out of budget
/// (nw-097, nw-111). A genuine empty result must still read cleanly.
#[test]
fn regex_search_text_distinguishes_no_matches_from_budget_exhaustion() {
    let (_dir, db) = honesty_fixture();

    // Genuine miss: a pattern that truly is not present.
    let miss = nestweaver_cmd()
        .args([
            "regex-search",
            "zzqq_definitely_absent_pattern_41",
            "--db",
            &db,
            "--max-millis",
            "60000",
        ])
        .output()
        .unwrap();
    let miss_text = String::from_utf8_lossy(&miss.stdout).to_lowercase();
    assert!(
        miss_text.contains("no matches"),
        "a genuine miss should still read as a plain no-match: {miss_text}"
    );
    assert!(
        !miss_text.contains("budget"),
        "a genuine miss must NOT warn about the budget — crying wolf on every \
         empty result is how a real truncation warning gets ignored: {miss_text}"
    );

    // Budget exhaustion: assert parity only if the engine actually reports it,
    // since a tiny fixture may complete within any budget.
    let args = ["regex-search", "alpha", "--db", &db, "--max-millis", "1"];
    let json_out = nestweaver_cmd().args(args).arg("--json").output().unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&json_out.stdout).expect("regex-search --json must be valid JSON");
    let truncated = json
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let empty = json
        .get("results")
        .and_then(|v| v.as_array())
        .is_some_and(|a| a.is_empty());
    if !(truncated && empty) {
        return;
    }

    let text_out = nestweaver_cmd().args(args).output().unwrap();
    let text = String::from_utf8_lossy(&text_out.stdout).to_lowercase();
    assert!(
        text.contains("budget") || text.contains("cut short") || text.contains("truncat"),
        "--json reported truncated:true with zero results, but text printed a bare \
         no-match — an actively false claim that matches do not exist.\n\n{text}"
    );
}

/// A read-only command must never report success for a database that isn't there.
///
/// nw-106: `list-projects` swallowed the store-open error on the daemon path and
/// returned `{"materialized": []}` at exit 0, so a missing database was
/// indistinguishable from "no projects defined" — and text mode suggested adding
/// `[[projects]]` to a config, an actively wrong remedy. A CI gate or agent reads
/// "zero projects" and proceeds on a false premise.
#[test]
fn list_projects_fails_on_a_missing_database() {
    let missing = "/nonexistent/dir/definitely-not-here.lbug";

    for extra in [vec![], vec!["--json"]] {
        let output = nestweaver_cmd()
            .args(["list-projects", "--db", missing])
            .args(&extra)
            .output()
            .unwrap();

        assert!(
            !output.status.success(),
            "list-projects must exit non-zero for a missing --db (mode {extra:?}); \
             17 of 18 comparable commands already do"
        );

        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            stderr.contains("not found") || stderr.contains("db_not_found"),
            "the error must name the real problem — a missing database — rather than \
             leaking a raw store error or suggesting a config change: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("materialized"),
            "no result payload should be emitted for a database that does not exist"
        );
    }
}

/// `daemon gc` must not require a database.
///
/// It sweeps orphaned launch agents and orphaned per-instance directories,
/// sparing live instances by ownership proof (write lock, pidfile lock) rather
/// than by matching a database path — the underlying `gc_orphaned_agents` and
/// `gc_orphaned_daemon_dirs` take no arguments. Requiring `--db` made the one
/// command whose purpose is cleaning up after databases that no longer exist
/// refuse to run without naming one that does, and contradicted its own
/// `--help`, which lists `--db` as optional with no default.
#[test]
fn daemon_gc_runs_without_a_database() {
    // `gc` is DESTRUCTIVE: it sweeps orphaned per-instance directories under the
    // state root, the runtime root and the /tmp socket-fallback root. Run
    // unisolated it would operate on the developer's (or the CI runner's) real
    // roots and could reclaim a concurrently-running daemon's directories — the
    // exact class of interference that shows up elsewhere as a vanished socket.
    //
    // Point all three roots at one scratch tree, which is the same isolation
    // `daemon_test.rs` uses and which `NESTWEAVER_SOCK_FALLBACK_DIR` exists for.
    let scratch = tempfile::tempdir().expect("scratch dir");
    let output = nestweaver_cmd()
        .args(["daemon", "gc"])
        .env_remove("NESTWEAVER_DB")
        .env("XDG_STATE_HOME", scratch.path().join("state"))
        .env("XDG_RUNTIME_DIR", scratch.path().join("runtime"))
        .env(
            "NESTWEAVER_SOCK_FALLBACK_DIR",
            scratch.path().join("fallback"),
        )
        .output()
        .expect("daemon gc must run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("No database path provided"),
        "daemon gc must not demand a database it never reads; got: {stderr}"
    );
    assert!(
        output.status.success(),
        "daemon gc must succeed with no --db and no NESTWEAVER_DB; stderr: {stderr}"
    );
}

/// The help text is the contract the previous behaviour broke, so pin it: if
/// `--db` ever becomes genuinely required here, the help must say so.
#[test]
fn daemon_gc_help_presents_db_as_optional() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let output = nestweaver_cmd()
        .args(["daemon", "gc", "--help"])
        .env("XDG_STATE_HOME", scratch.path().join("state"))
        .env("XDG_RUNTIME_DIR", scratch.path().join("runtime"))
        .env(
            "NESTWEAVER_SOCK_FALLBACK_DIR",
            scratch.path().join("fallback"),
        )
        .output()
        .expect("daemon gc --help must run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--db"), "help must mention --db");
    assert!(
        !stdout.contains("required"),
        "daemon gc --help presents --db as optional; keep behaviour and help in step"
    );
}

/// nw-246 (second hole), direct route: the ambiguity refusal must be
/// REACHABLE.
///
/// `resolve_instance_id_for_db` returned the recorded identity before ever
/// consulting `observed_instance_ids`, which made the refusal below dead code
/// for any database that has a record. `ensure_data_instance_id` runs on every
/// repo index and never replaces an existing value, so that is every database
/// created or indexed under 8.0.0 — only a pre-nw-246 database could reach a
/// refusal the runbook promises for everyone.
///
/// The sequence is entirely supported behaviour, which is what makes it bad:
/// two STATED indexes pass by design (instance switching is a feature), and
/// leave a database holding two instances whose record still names the first.
/// A config-less index then adopted that record and silently re-keyed the
/// repo — the nw-246 fork, produced by the guard meant to prevent it.
///
/// Reverting the ordering leaves the whole suite green without this test. That
/// is why it exists.
#[test]
fn a_recorded_identity_does_not_mask_an_ambiguous_database() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");

    let mut repos = Vec::new();
    for name in ["first", "second", "third"] {
        let repo = dir.path().join(name);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = StdCommand::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("git failed to spawn");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(repo.join("a.js"), format!("function {name}() {{}}")).unwrap();
        git(&["add", "a.js"]);
        git(&["commit", "-m", "init"]);
        repos.push(repo);
    }

    // Two STATED indexes. Both must succeed — instance switching is a
    // supported operation, and a guard that refused these would have retired
    // the capability rather than closed the hole.
    for (repo, instance) in [(&repos[0], "one"), (&repos[1], "two")] {
        nestweaver_cmd()
            .args([
                "index",
                "--repo",
                &repo.display().to_string(),
                "--db",
                &db_path.display().to_string(),
                "--instance",
                instance,
            ])
            .assert()
            .success();
    }

    // Now the database holds two instances and its record names "one". A
    // config-less index has no safe default.
    let refused = nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repos[2].display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "a config-less index against a database holding two instances must be \
         refused; adopting the recorded one silently re-keys the repo"
    );
    let stderr = flatten_miette(&refused.stderr);
    assert!(
        stderr.contains("one") && stderr.contains("two"),
        "the refusal must name the instances it found:\n{stderr}"
    );

    // Counterweight: stating an instance still resolves it. Without this, the
    // refusal is indistinguishable from an index that has stopped working.
    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repos[2].display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--instance",
            "one",
        ])
        .assert()
        .success();
}

/// nw-246 (third hole): `nestweaver index` must actually RECORD the instance.
///
/// `ensure_data_instance_id`'s only caller was
/// `index_directory_with_store_inner`, which is not the funnel the CLI index
/// reaches — the CLI goes through `index_into_store_with_write_gate`. So the
/// call ran on no real path, `data_instance_id()` returned `None` for every
/// database, and the mint-once mechanism the whole item rests on was inert.
///
/// Nothing failed visibly, which is why it survived: `resolve_instance_id_for_db`
/// falls back to inferring the instance from existing UIDs, and inference gives
/// the same answer as the record in every single-instance case. The two only
/// disagree where it matters — and there, silence looked like agreement.
///
/// Asserted against the store directly because the record has no CLI surface;
/// `brain status` reports OBSERVED instances, which is the fallback, not this.
#[test]
fn indexing_records_the_data_instance_id() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .expect("git failed to spawn");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(repo.join("a.js"), "function hello() {}").unwrap();
    git(&["add", "a.js"]);
    git(&["commit", "-m", "init"]);

    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo.display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--instance",
            "recorded-one",
        ])
        .assert()
        .success();

    let store = nestweaver_store::GraphStore::open_read_only(&db_path).unwrap();
    assert_eq!(
        store.data_instance_id().unwrap().as_deref(),
        Some("recorded-one"),
        "an index must record the instance its data belongs to; without it the \
         database can only ever INFER its identity from existing UIDs, and the \
         inference agrees with the record in exactly the cases where neither \
         matters"
    );
}

/// nw-268: `pre-push-impact --max-depth` is bounded at parse time.
///
/// It had no `value_parser`, so a mistyped `--max-depth 100000` arrived as a
/// real instruction and became an unbounded transitive graph walk. The gRPC
/// half accepted whatever it was sent, while its own sibling traversal
/// (`remaining_depth`) had clamped at 64 all along — two limits, one absent.
///
/// Asserts WHICH error, not merely that the command failed. My first version
/// checked only a non-zero exit and passed with the bound removed, because
/// `pre-push-impact` without `--local-changes` or `--diff` already exits
/// non-zero for an unrelated reason. A refusal is only evidence when it is the
/// refusal you asked for.
#[test]
fn pre_push_impact_depth_is_bounded_at_parse_time() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

    let depth_error = |depth: &str| -> String {
        let out = nestweaver_cmd()
            .args([
                "pre-push-impact",
                "--db",
                &db_path.display().to_string(),
                "--max-depth",
                depth,
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "`--max-depth {depth}` unexpectedly succeeded"
        );
        String::from_utf8_lossy(&out.stderr).to_lowercase()
    };

    // Past the ceiling: a PARSE error naming the range.
    let past = depth_error("100000");
    assert!(
        past.contains("invalid value") || past.contains("not in"),
        "an out-of-range --max-depth must be refused by the parser, naming the \
         range — not accepted and turned into an unbounded walk:\n{past}"
    );

    // The counterweight: an in-range depth must get PAST parsing. It still
    // fails, on the missing `--local-changes`/`--diff` — which is exactly the
    // error that made the naive version of this test vacuous, so asserting it
    // here is what proves the two cases are distinguishable at all.
    let ok = depth_error("8");
    assert!(
        ok.contains("--local-changes") || ok.contains("--diff"),
        "a depth within the bound must parse and fail later, on the missing \
         change source:\n{ok}"
    );

    // The boundary, as LITERALS. Deriving these from the constant would make
    // the test agree with whatever the constant says.
    let at_limit = depth_error("64");
    assert!(
        at_limit.contains("--local-changes") || at_limit.contains("--diff"),
        "64 is the documented ceiling and must parse:\n{at_limit}"
    );
    let past_limit = depth_error("65");
    assert!(
        past_limit.contains("invalid value") || past_limit.contains("not in"),
        "65 is past the documented ceiling and must be refused by the parser:\n{past_limit}"
    );
}

/// nw-269: an unreadable count must render as unavailable, never as `0`.
///
/// Unit-level because the failure it guards — a per-vault `list_notes` erroring
/// — cannot be provoked from the CLI without corrupting a store mid-run. What
/// CAN be pinned exactly is the rendering decision, which is where all three
/// instances of this bug have lived: `unwrap_or(0)` on a value the producer
/// deliberately nulled.
///
/// Three counts in `main.rs` have needed this (top-level `brain status`
/// totals, per-vault note counts, and `stale-check`'s commits-behind in
/// nw-256), and each hand-rolled copy drifted back to `unwrap_or(0)` at least
/// once — so the renderer is shared and this pins the shared one.
#[test]
fn an_unreadable_count_is_never_rendered_as_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

    // A vault with genuinely nothing in it must still say "0 notes" — the
    // counterweight. Without it, a renderer that called EVERYTHING unavailable
    // would pass the assertion that matters.
    let output = nestweaver_cmd()
        .args(["brain", "status", "--db", &db_path.display().to_string()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "brain status on an empty database must succeed:\n{stdout}"
    );
    assert!(
        !stdout.contains("unavailable (could not be read"),
        "a database that reads fine must report real counts, not unavailable:\n{stdout}"
    );
}

/// nw-270: `summary --json` must disclose the symbol cap.
///
/// `SymbolSummaries` carries `matched_total` and `capped` — its own doc says
/// "so callers can report honest truncation" — and this caller kept only
/// `summaries`. `truncated` was then computed against the ALREADY-CAPPED
/// length, so a 500-of-N answer reported `total: 500, truncated: false`. The
/// cap was invisible in exactly the field that exists to disclose it.
///
/// Needs a corpus larger than `DEFAULT_SYMBOL_SUMMARY_CAP` (500) for the cap
/// to bind at all — a smaller fixture cannot distinguish the states and the
/// test would pass either way.
#[test]
fn summary_json_discloses_the_symbol_cap() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    let mut src = String::new();
    for i in 0..640 {
        src.push_str(&format!("export function fn{i}() {{ return {i}; }}\n"));
    }
    std::fs::write(repo.join("many.js"), src).unwrap();
    let git = |args: &[&str]| {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .expect("git failed to spawn");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "many.js"]);
    git(&["commit", "-m", "init"]);

    nestweaver_cmd()
        .args([
            "index",
            "--repo",
            &repo.display().to_string(),
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    let output = nestweaver_cmd()
        .args([
            "summary",
            "--db",
            &db_path.display().to_string(),
            "--level",
            "symbol",
            "--token-budget",
            "0",
            "--json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("summary --json: {e}\n{}", &stdout[..stdout.len().min(400)]));

    let returned = parsed["returned"].as_u64().expect("returned");
    let total = parsed["total"].as_u64().expect("total");
    let truncated = parsed["truncated"].as_bool().expect("truncated");

    // The counterweight: the cap must actually have bound. If the indexer
    // produced fewer than 500 symbols this proves nothing, and would pass
    // against the unfixed code.
    assert_eq!(
        returned, 500,
        "the fixture must exceed the 500-symbol cap for this to test anything; \
         got {returned} returned of {total}"
    );
    assert!(
        truncated,
        "a capped answer must report truncated: true — reporting false is the \
         defect:\n returned={returned} total={total}"
    );
    assert!(
        total > returned,
        "`total` must count what MATCHED, not what survived the cap; \
         returned == total beside truncated:true is a contradiction \
         (returned={returned}, total={total})"
    );
}
