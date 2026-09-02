use assert_cmd::Command;
use assert_cmd::assert::OutputAssertExt;
use nestweaver_engine::sidecar_path;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::process::Command as StdCommand;

fn nestweaver_cmd() -> Command {
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    // nw-261: pin miette's render width so a diagnostic phrase cannot be split
    // mid-assertion by the runner's terminal geometry. A test asserting WHAT a
    // message says must not depend on HOW WIDE the terminal was. Tests that
    // exercise wrapping itself override or remove this deliberately.
    cmd.env("NESTWEAVER_DIAGNOSTIC_WIDTH", "1000");
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
        // nw-280: `brain list` joins the matrix the moment it gained
        // `--config`, as the inventory guard in `main.rs` requires — a command
        // that takes both flags must be shown to HONOUR the config, not merely
        // to accept it.
        &["brain", "list", "--json"][..],
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
        // nw-316: the phrase moved because the direct route stopped
        // re-implementing the tool. It used to print "<name> has no associated
        // notes or symbols" while the daemon route printed the TOOL's note,
        // "No notes or symbols are associated with this project yet." — one
        // condition, two sentences, chosen by transport. What this assertion
        // is actually for is the CONFIG resolution (the project named in
        // `--config`'s database was found at all), and that is unchanged.
        .stdout(contains(
            "No notes or symbols are associated with this project yet.",
        ))
        .stdout(contains("configured-project"));

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
    // FLIP THESE WHEN THE PUBLISH ACTUALLY LANDS. The npm entries are listed as
    // unsupported because `nestweaver` has never been published (registry 404 as
    // of 9.0.0 prep) -- nw-115: the release job's publish step exits 0 when
    // NPM_TOKEN is unset, so a green release has never implied a published
    // package. Once `npm view nestweaver version` resolves, drop the four npm
    // lines so the docs are free to advertise the install. `cargo install` and
    // `brew install` stay: neither channel exists.
    let unsupported_commands = [
        "npm install -g nestweaver",
        "npm install nestweaver",
        "cargo install nestweaver",
        "brew install nestweaver",
        "npx nestweaver",
        "npm exec nestweaver",
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

/// The 22.04 runner pin fixes the glibc floor and BREAKS the C++ build unless
/// the toolchain and runtime are also controlled. 22.04's default GCC 11 lacks
/// both lbug's AVX-512 FP16 intrinsics and `std::format`; GCC 12 has the former
/// but its libstdc++ still lacks the latter. GCC 13 supplies both, but its
/// dynamic libstdc++ is newer than the one installed on stock 22.04, so the C++
/// and GCC runtimes must be shipped beside the release binary. The v9.0.3
/// static-runtime and v9.0.4 dynamic-runtime artifacts were both linked with
/// mold and both segfaulted on their first LadybugDB open even though their
/// version-only release checks passed. GNU ld plus the real database smoke is
/// the verified final-link contract.
///
/// v9.0.0 failed on the SIMD requirement and v9.0.1 failed on `std::format`
/// AFTER each tag was public. These constraints belong together and must be
/// preflighted on the architecture where each one applies.
#[test]
fn release_workflow_pins_a_portable_linux_cxx20_toolchain() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow =
        std::fs::read_to_string(repo_root.join(".github/workflows/release-please.yml")).unwrap();
    let preflight = workflow_step(
        &workflow,
        "Verify the Linux C++20 and architecture toolchain",
    );
    let linker =
        std::fs::read_to_string(repo_root.join("scripts/gcc13-bundled-cxx-linker")).unwrap();

    for needle in ["gcc-13", "g++-13", "CC=gcc-13", "CXX=g++-13"] {
        assert!(
            workflow.contains(needle),
            "the Linux build must pin {needle}: ubuntu-22.04 defaults to GCC 11, \
             and GCC 12 still cannot compile lbug's use of std::format"
        );
    }
    assert!(
        preflight.contains("#include <format>") && preflight.contains("-std=c++20"),
        "the job must preflight the C++20 std::format support lbug requires"
    );
    assert!(
        preflight.contains("if [ \"${{ matrix.target }}\" = \"x86_64-unknown-linux-gnu\" ]; then")
            && preflight.contains("avx512fp16"),
        "the AVX-512 FP16 probe must run on x86_64 without being passed to the \
         native ARM64 compiler"
    );
    assert!(
        workflow.contains("scripts/gcc13-bundled-cxx-linker")
            && linker.contains("-Wl,-rpath,$ORIGIN/lib")
            && !linker.contains("libstdc++.a")
            && !repo_root.join("scripts/gcc13-static-cxx-linker").exists(),
        "the binary must dynamically load the compiler runtime bundled at \
         $ORIGIN/lib; static libstdc++ corrupts LadybugDB's Rust/C++ boundary"
    );
    assert!(
        workflow.contains("Stage Linux compiler runtime")
            && workflow.contains("g++-13 -print-file-name=libstdc++.so.6")
            && workflow.contains("gcc-13 -print-file-name=libgcc_s.so.1")
            && workflow.contains("cp -L")
            && workflow.contains("LICENSE-GCC-runtime")
            && workflow.contains("Library runpath:")
            && workflow.contains("$ORIGIN/lib")
            && workflow
                .contains("tar czf \"$ARCHIVE\" -C \"target/$TARGET/release\" nestweaver lib"),
        "the Linux archive must carry and resolve the exact GCC 13 runtimes it was built against"
    );
    assert!(
        workflow.contains("NESTWEAVER_ALLOW_NO_DAEMON=1 NESTWEAVER_NO_DAEMON=1")
            && workflow.contains("--name release-artifact-smoke")
            && workflow.contains("--with-trigrams"),
        "release verification must open a real database and index code; \
         `--version` did not catch the v9.0.3 runtime segfault"
    );
    assert!(
        workflow.contains("workflow_dispatch:")
            && workflow.contains("operation:")
            && workflow.contains("options: [dry-run, resume, cleanup-draft, recover-npm]")
            && workflow.contains("needs.release-context.outputs.mode == 'dry-run'")
            && workflow.contains("needs.release-context.outputs.mode == 'publish'")
            && workflow.contains("verify-dry-run:")
            && workflow.contains("cleanup-canary:")
            && workflow.contains("scripts/observe-release-visibility.sh")
            && workflow.contains("scripts/verify-release-canary-pr.sh")
            && workflow.contains("CFLAGS: \"-DZSTD_DISABLE_ASM\"")
            && !workflow.contains("fuse-ld=mold")
            && workflow.contains("zstdnoasm-gnuld")
            && workflow.contains("thread apply all backtrace")
            && workflow.contains("visibility_after: $after, automation_pr: $canary"),
        "release dry-run must be mode-isolated and preserve observed tag/release/npm plus \
         automation-PR evidence; Linux must use the system linker and the same zstd \
         native-code contract as normal CI, with a backtrace on functional-smoke failure"
    );
}

/// Ubuntu 22.04's protobuf-compiler is protoc 3.12. The schema uses proto3
/// optional fields, which that version refuses unless explicitly enabled.
/// Current protoc releases still accept the compatibility flag, so the build
/// script must carry it rather than making only one CI runner special.
#[test]
fn proto_build_supports_the_declared_ubuntu_2204_baseline() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let build_script =
        std::fs::read_to_string(repo_root.join("crates/nestweaver-proto/build.rs")).unwrap();

    assert!(
        build_script.contains(".protoc_arg(\"--experimental_allow_proto3_optional\")"),
        "the proto build must opt into proto3 optional fields for Ubuntu 22.04's protoc 3.12"
    );
}

/// The release PR is the ONE pull request in this repo that receives no CI:
/// release-please opens it with `GITHUB_TOKEN`, and GitHub deliberately does not
/// trigger workflows for bot-token-created PRs. Everything that would otherwise
/// be caught by a check has to be caught here instead.
///
/// This pins the specific hole that reached a cut: the lockfile-sync steps were
/// gated on `prs_created`, which release-please sets ONLY when it CREATES the
/// PR. On every later push it UPDATES the existing PR, `prs_created` is false,
/// and the sync never ran -- so `Cargo.toml` advanced to 9.0.0 while
/// `Cargo.lock` stayed at 8.0.0. `cargo build --locked` refuses that outright
/// ("cannot update the lock file ... because --locked was passed", exit 101),
/// so merging would have cut the tag and then failed all four binary builds,
/// publishing a release with no artifacts.
#[test]
fn release_workflow_syncs_the_lockfile_however_the_release_pr_got_there() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow =
        std::fs::read_to_string(repo_root.join(".github/workflows/release-please.yml")).unwrap();

    let release_pr = workflow
        .split_once("\n  release-pr:\n")
        .expect("release workflow must define release-pr")
        .1
        .split_once("\n  prepare-release-lockfile:\n")
        .expect("release-pr must precede the read-only lockfile job")
        .0;
    for output in [
        "number: ${{ steps.release_pr.outputs.number }}",
        "branch: ${{ steps.release_pr.outputs.branch }}",
        "head_sha: ${{ steps.release_pr.outputs.head_sha }}",
    ] {
        assert!(
            release_pr.contains(output),
            "release-pr must export `{output}` across the job boundary"
        );
    }

    let prepare = workflow
        .split_once("\n  prepare-release-lockfile:\n")
        .expect("release workflow must define prepare-release-lockfile")
        .1
        .split_once("\n  publish-release-lockfile:\n")
        .expect("the read-only lockfile job must precede its publisher")
        .0;
    assert!(
        prepare.contains("permissions:\n      contents: read")
            && !prepare.contains("contents: write")
            && prepare.contains("cargo update --workspace")
            && prepare.contains("cargo metadata --locked"),
        "untrusted release-PR Cargo execution must remain in the read-only preparation job"
    );

    let publisher = workflow
        .split_once("\n  publish-release-lockfile:\n")
        .expect("release workflow must define publish-release-lockfile")
        .1
        .split_once("\n  build:\n")
        .expect("the lockfile publisher must precede release builds")
        .0;
    for lease_contract in [
        "repos/$GITHUB_REPOSITORY/git/commits/$RELEASE_PR_HEAD_SHA",
        "parents: [$parent]",
        "{sha: $sha, force: false}",
        "repos/$GITHUB_REPOSITORY/git/refs/heads/$RELEASE_PR_BRANCH",
    ] {
        assert!(
            publisher.contains(lease_contract),
            "lockfile publication must preserve exact-head lease contract `{lease_contract}`"
        );
    }
    assert!(
        !publisher.contains("actions/checkout")
            && !publisher.contains("cargo ")
            && !publisher.contains("--method PUT"),
        "the write-capable publisher must neither execute PR code nor use a blob-only Contents API lease"
    );
    // Assert on the GATES, not on the file text: the comments above these steps
    // name `prs_created` deliberately, to explain why it is the wrong condition.
    // A test that forbids the word would forbid the explanation.
    let gated_on_prs_created: Vec<&str> = workflow
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("if:") && line.contains("prs_created"))
        .collect();
    assert!(
        gated_on_prs_created.is_empty(),
        "no step may be gated on `prs_created`: it is true only when \
         release-please CREATES the PR, so an UPDATED release PR silently skips \
         the step -- the defect this test exists to prevent. Found: {gated_on_prs_created:?}"
    );
    assert!(
        workflow.contains("autorelease: pending"),
        "the release PR must be locatable by its own label, for the UPDATE case \
         where release-please emits no `pr` output at all"
    );
    // Both lookups are required, and neither is sufficient alone. The label
    // alone RACES: PR #338 was created at 04:49:05 and the label lookup ran
    // within five seconds, matched nothing, and skipped the sync -- shipping a
    // 9.0.1 PR with a 9.0.0 lockfile, the exact defect the lookup was added to
    // fix. The action output alone misses the update case.
    assert!(
        workflow.contains("steps.release.outputs.pr"),
        "the job must prefer release-please's own `pr` output, which is \
         populated in-process and cannot race the label being attached"
    );
    assert!(
        workflow.contains("refusing to skip the lockfile sync"),
        "if the action reports creating a PR that cannot then be located, that \
         is an inconsistency and must fail the job -- silently skipping the \
         sync is how a release reaches `cargo build --locked` with a stale lock"
    );
    assert!(
        prepare.contains("cargo metadata --locked"),
        "the read-only job must PROVE --locked accepts the synchronized tree; \
         `cargo update` exiting 0 says the lock was refreshed, not that the \
         build job's --locked will accept it"
    );
}

/// The Linux entries pin an OLD runner deliberately: glibc is backward
/// compatible but not forward, so the build host's glibc is the compatibility
/// floor shipped to users. These were `ubuntu-latest`/`ubuntu-24.04-arm`, and
/// when `ubuntu-latest` moved to 24.04 the shipped floor rose to GLIBC_2.39
/// without any label changing -- which is why the public v8.0.0 GNU archive
/// cannot start on Ubuntu 22.04, Debian 12 or RHEL 9. Do NOT "modernise" these
/// back to `ubuntu-latest`. The floor is separately enforced against the built
/// artifact in the workflow's `Verify release binary` step, because a runner
/// pin is a claim about the build host and the artifact is what users run.
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
            (Some("ubuntu-22.04-arm".to_string()), Some(String::new())),
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
            (Some("ubuntu-22.04".to_string()), Some(String::new())),
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

/// nw-334's class, in help text rather than in an error text: a string the
/// user ACTS on that nobody verified against the code.
///
/// `dead-code --limit` advertised "(1-1000, default: all)". The default is
/// `configured_result_limit()` — 50 — so omitting `--limit` to get every row
/// returned 50 with `truncated: true`, a state the help said could not arise.
/// There is no "all" to reach either: the parser rejects `0`, so no value of
/// the flag disables the cap.
///
/// `impact --limit` was the mirror image. Its rationale was written as a `///`
/// doc comment, which clap promotes to `long_help`, so `--help` answered "what
/// does --limit do?" with "nw-357. The `brain_impact` schema has carried a
/// `limit`…" and never named the default or the range at all.
///
/// Both are pinned here rather than only fixed, because a help string drifts
/// from behaviour silently — nothing compiles against it.
#[test]
fn cli_limit_help_states_the_default_the_code_actually_applies() {
    // The claim that replaced "default: all" must name the real default …
    nestweaver_cmd()
        .args(["dead-code", "--help"])
        .assert()
        .success()
        .stdout(contains("default 50"))
        .stdout(contains("default: all").not());

    // … and "there is no 'all'" must be true: the cap has no off switch.
    nestweaver_cmd()
        .args(["dead-code", "--limit", "0"])
        .assert()
        .failure()
        .stderr(contains("0 is not in 1..=1000"));

    // `--help` renders long_help, so a doc-comment narrative would surface
    // here and not in `-h`. The flag must be described, not changelogged.
    let long_help = nestweaver_cmd()
        .args(["impact", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let long_help = String::from_utf8_lossy(&long_help);
    let limit_help = long_help
        .split("--limit <LIMIT>")
        .nth(1)
        .expect("impact --help must document --limit");
    let limit_help = limit_help.split("--json").next().unwrap_or(limit_help);
    assert!(
        !limit_help.contains("nw-357"),
        "`impact --limit` help is a ticket narrative, not user-facing help: {limit_help}"
    );
    assert!(
        limit_help.contains("default 50") && limit_help.contains("1-1000"),
        "`impact --limit` help must state its default and range: {limit_help}"
    );
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

/// nw-269 / nw-277: this is the COUNTERWEIGHT half, and it is named for what
/// it actually asserts.
///
/// It was called `an_unreadable_count_is_never_rendered_as_zero`, which is not
/// what it does: a per-vault `list_notes` failure cannot be provoked from the
/// CLI without corrupting a store mid-run, so this only ever exercised the
/// readable case. A test whose NAME claims the guarantee while its body checks
/// the opposite direction is worse than no test — it answers the question for
/// anyone who greps for it.
///
/// The guarantee itself is pinned by value in
/// `optional_count_rendering_tests::unknown_and_zero_do_not_render_alike`,
/// which asserts both directions against the shared renderer. This one earns
/// its place as the other half of that pair: a renderer that called EVERYTHING
/// unavailable would satisfy the unit test's "unknown != zero" assertion and
/// fail here.
#[test]
fn a_readable_database_reports_real_counts_not_unavailable() {
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

    // nw-277: the TEXT route must disclose it too. The nw-270 fix reached
    // `--json` and stopped there, leaving the default output — the one most
    // people actually read — showing 500 summaries with nothing saying the
    // rest exist.
    let text = nestweaver_cmd()
        .args([
            "summary",
            "--db",
            &db_path.display().to_string(),
            "--level",
            "symbol",
            "--token-budget",
            "0",
        ])
        .output()
        .unwrap();
    let notice = String::from_utf8_lossy(&text.stderr);
    assert!(
        notice.contains("capped") || notice.contains("showing"),
        "the text route must say the output was capped, not just the JSON \
         one:\n{notice}"
    );
}

/// nw-273: `--refresh-wiki-hours` must REFUSE, not claim to have scheduled
/// something.
///
/// The periodic refresh thread is spawned only on the direct-watcher fallback
/// and reaches the daemon via `DaemonClient::connect`, which autostarts one —
/// and nw-267 gave the direct watcher the write lease, so that autostart now
/// correctly fails. The thread swallowed it into `tracing::warn!`. On the
/// daemon route the thread is never spawned at all. Both routes printed
/// "Wiki refresh scheduled every {h}h" regardless.
///
/// Our own fix produced this: nw-267's blast radius was not traced to the
/// feature sitting beside it.
#[test]
fn refresh_wiki_hours_refuses_rather_than_claiming_to_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let vault = dir.path().join("vault");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(vault.join("n.md"), "# n\n").unwrap();
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

    let config = dir.path().join("instance.toml");
    std::fs::write(
        &config,
        format!(
            r#"instance_id = "wiki-refusal"
db = "{}"
repos = []

[snapshot_storage]
backend = "local"
path = "/tmp/nestweaver/wiki-refusal/snapshots"

[workspace]
backend = "local"
path = "/tmp/nestweaver/wiki-refusal/workspace"

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

    let cfg = config.display().to_string();
    let vault_s = vault.display().to_string();
    let repo_s = repo.display().to_string();
    for (label, args) in [
        (
            "watch",
            vec![
                "watch",
                &repo_s,
                "--config",
                &cfg,
                "--refresh-wiki-hours",
                "6",
            ],
        ),
        (
            "brain watch",
            vec![
                "brain",
                "watch",
                &vault_s,
                "--config",
                &cfg,
                "--refresh-wiki-hours",
                "6",
            ],
        ),
    ] {
        // Timed out deliberately. Without the refusal these commands START A
        // WATCHER and block forever, so a regression would HANG this test
        // rather than fail it — CI would eventually kill the job and report a
        // timeout instead of the assertion that explains why. A test whose
        // failure mode is a hang tells you almost nothing.
        let out = nestweaver_cmd()
            .args(&args)
            .timeout(std::time::Duration::from_secs(20))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            !out.status.success(),
            "`{label} --refresh-wiki-hours` must refuse; it honours nothing.\
             \nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("not implemented"),
            "`{label}` must say the flag is not implemented:\n{stderr}"
        );
        assert!(
            stderr.contains("materialize-projects"),
            "`{label}`'s refusal must name the remedy the user can actually \
             run:\n{stderr}"
        );
        // The specific lie this replaces. A refusal that still printed it
        // would be no better than the no-op.
        assert!(
            !stdout.contains("Wiki refresh scheduled")
                && !stderr.contains("Wiki refresh scheduled"),
            "`{label}` must not still claim it scheduled a refresh:\n{stdout}\n{stderr}"
        );
    }
}

/// nw-281: `extensions list` must fail on a missing `--db`, like its siblings.
///
/// It resolved a db path and read the sidecar beside it without ever checking
/// the database existed — so a typo'd `--db` reported "0 annotated node(s)"
/// and exited 0. In an auditing command that is the same defect nw-257 fixed
/// one layer up: a fact about this invocation presented as a fact about the
/// store. 36 other read commands call `require_existing_db`; this one did not.
#[test]
fn extensions_list_refuses_a_database_that_does_not_exist() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not-created.lbug");

    let refused = nestweaver_cmd()
        .args(["extensions", "list", "--db", &missing.display().to_string()])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "a missing --db must be an error, not an empty audit"
    );
    assert!(
        !String::from_utf8_lossy(&refused.stdout).contains("annotated node"),
        "and it must not print a count as though it had looked:\n{}",
        String::from_utf8_lossy(&refused.stdout)
    );

    // The counterweight: a database that DOES exist with no sidecar is still
    // a successful, empty audit — that is a real answer, not a failure.
    let present = dir.path().join("real.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&present).unwrap());
    nestweaver_cmd()
        .args(["extensions", "list", "--db", &present.display().to_string()])
        .assert()
        .success();
}

/// nw-280: the three commands the upgrade runbook names must all accept
/// `--config`.
///
/// `instance-id-migration.md` step 4 — "verify one convention everywhere" —
/// tells a config-driven user to add `--config` to `list-repos`,
/// `list-projects` and `brain list`. The first two took it; `brain list` did
/// not, so the verification step could not be run as written. Dropping the
/// flag doesn't rescue it either: without `--config` the command silently
/// reads `./nestweaver.lbug`, which is the wrong database, and reports a clean
/// result — a verification step that cannot fail.
#[test]
fn every_command_the_upgrade_runbook_names_accepts_a_pinned_config() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("configured.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

    let config_path = dir.path().join("instance.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"instance_id = "runbook-parity"
db = "{}"
repos = []

[snapshot_storage]
backend = "local"
path = "/tmp/nestweaver/runbook-parity/snapshots"

[workspace]
backend = "local"
path = "/tmp/nestweaver/runbook-parity/workspace"

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

    // Run from somewhere else entirely, so a fallback to `./nestweaver.lbug`
    // cannot succeed by accident — which is the failure mode being guarded.
    let unrelated_cwd = dir.path().join("unrelated-cwd");
    std::fs::create_dir(&unrelated_cwd).unwrap();

    for args in [
        &["list-repos", "--json"][..],
        &["list-projects", "--json"][..],
        &["brain", "list", "--json"][..],
    ] {
        let out = nestweaver_cmd()
            .current_dir(&unrelated_cwd)
            .env_remove("NESTWEAVER_DB")
            .args(args)
            .arg("--config")
            .arg(&config_path)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "`{args:?} --config` must work — the upgrade runbook tells users to \
             run exactly this:\n{stderr}"
        );
        // Specifically NOT an unknown-argument rejection, which is how this
        // failed before: a bare non-zero exit would also be produced by a
        // missing database, and the two mean very different things.
        assert!(
            !stderr.contains("unexpected argument"),
            "`{args:?}` must ACCEPT --config, not reject it as unknown:\n{stderr}"
        );
    }
}

// ── nw-284 / S2: a mutating command may infer its source OR its target ────

/// nw-284 / S2. A bare `nestweaver index` — neither source nor target stated —
/// must refuse, because it would otherwise index the current directory into
/// whatever `NESTWEAVER_DB` happens to name. This fired against the production
/// graph during the 8.0.0 post-release hunt (102 files, 2329 symbols, repos
/// 43 -> 44) and exited 0.
///
/// The test owns its environment end to end: cwd is a temp dir, NESTWEAVER_DB
/// is set to a path inside that same temp dir, and no assertion depends on any
/// inherited variable.
#[test]
fn index_refuses_when_neither_repo_nor_db_was_stated() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("some-checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("a.py"), "def f():\n    return 1\n").unwrap();
    let ambient_db = dir.path().join("ambient.lbug");

    nestweaver_cmd()
        .current_dir(&cwd)
        .env("NESTWEAVER_DB", &ambient_db)
        .arg("index")
        .assert()
        .code(64)
        // names the inferred SOURCE
        .stderr(contains(cwd.display().to_string()))
        // names the inferred TARGET and where it came from
        .stderr(contains(ambient_db.display().to_string()))
        .stderr(contains("NESTWEAVER_DB"))
        // offers a remedy that actually discriminates
        .stderr(contains("--repo").and(contains("--db")));

    // The refusal must be total: nothing was written to the ambient target.
    assert!(
        !ambient_db.exists(),
        "a refused index must not create or touch the ambient database"
    );
}

/// The companion half of S2, and the reason the guard is not "make --repo
/// required": stating EITHER end is intent, and must still work.
#[test]
fn index_still_runs_when_only_one_end_was_stated() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("some-checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("a.py"), "def f():\n    return 1\n").unwrap();
    let stated_db = dir.path().join("stated.lbug");
    let ambient_db = dir.path().join("ambient.lbug");

    // Source inferred from cwd, target stated -> allowed.
    nestweaver_cmd()
        .current_dir(&cwd)
        .env("NESTWEAVER_DB", &ambient_db)
        .args(["index", "--db"])
        .arg(&stated_db)
        .assert()
        .success();
    assert!(stated_db.exists());
    assert!(!ambient_db.exists());

    // Target inferred from the environment, source stated -> also allowed.
    let second = dir.path().join("second.lbug");
    nestweaver_cmd()
        .current_dir(dir.path())
        .env("NESTWEAVER_DB", &second)
        .args(["index", "--repo"])
        .arg(&cwd)
        .assert()
        .success();
    assert!(second.exists());
}

/// S2 is a property, not a fix to `index`. `watch` shares the shape
/// byte-for-byte and is strictly worse — it writes for as long as it runs — so
/// it must refuse identically.
#[test]
fn watch_refuses_when_neither_repo_nor_db_was_stated() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("some-checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let ambient_db = dir.path().join("ambient.lbug");

    // Bounded on purpose. Without the guard `watch` does not exit — it starts
    // watching and writes to the ambient database for as long as it runs,
    // which is precisely why this arm is a refusal and not a warning. An
    // unbounded assert here would hang the suite on a regression instead of
    // reporting one.
    nestweaver_cmd()
        .current_dir(&cwd)
        .env("NESTWEAVER_DB", &ambient_db)
        .arg("watch")
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .code(64)
        .stderr(contains("NESTWEAVER_DB"));
    assert!(!ambient_db.exists());
}

/// `ui --watch` re-indexes continuously and had NO `--repo` flag at all, so
/// its source was not even statable. The flag added by this fix is what makes
/// the guard satisfiable rather than a dead end.
#[test]
fn ui_watch_refuses_a_wholly_inferred_write_and_can_be_corrected() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("some-checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let ambient_db = dir.path().join("ambient.lbug");

    nestweaver_cmd()
        .current_dir(&cwd)
        .env("NESTWEAVER_DB", &ambient_db)
        .args(["ui", "--watch"])
        .assert()
        .code(64)
        .stderr(contains("NESTWEAVER_DB"));

    // The remedy the refusal names must exist on this command.
    nestweaver_cmd()
        .args(["ui", "--help"])
        .assert()
        .success()
        .stdout(contains("--repo"));
}

/// nw-312. The documented exit-code contract is 0 ok / 1 error / 2 not-found /
/// 64 usage, and 8.0.0 specifically split usage errors out so a caller could
/// tell "you asked wrongly" from "it went wrong". An invalid enum value is a
/// usage error, and `--format`/`--scope` were bare `String`s with the legal
/// values enumerated only in prose — so validation happened in the handler and
/// arrived as a generic error, exit 1.
///
/// The internal inconsistency is both the evidence and the counterweight:
/// `--top` on the SAME command is a `usize`, so clap's own parse rejects a bad
/// value and the binary already answers 64. The classification exists; these
/// two arguments took a different route to it.
#[test]
fn export_rejects_an_invalid_enum_as_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("main.js"),
        "export function greet(n) { return n; }",
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

    // `--top 0` is the control: a bad value on this same command, already 64.
    for argv in [
        vec!["export", "--format", "bogus"],
        vec!["export", "--scope", "bogus"],
        vec!["export", "--top", "not-a-number"],
    ] {
        let output = nestweaver_cmd()
            .args(&argv)
            .args(["--db", &db_path.display().to_string()])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(64),
            "{argv:?} exited {:?}; `--top not-a-number` on this same command \
             exits 64, so the classification exists and this path bypassed it.\
             \nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Counterweight: every documented value must still be ACCEPTED, or a
    // `value_parser` built from the wrong list would satisfy the above.
    for (format, scope) in [
        ("cypher", "code"),
        ("graphml", "all"),
        ("graphml", "vault"),
        ("mermaid", "code"),
    ] {
        nestweaver_cmd()
            .args([
                "export",
                "--format",
                format,
                "--scope",
                scope,
                "--db",
                &db_path.display().to_string(),
            ])
            .assert()
            .success();
    }

    // And `msgpack` must still reach its handler, which refuses `--scope vault`
    // as a SEMANTIC combination rather than an unknown value — a distinction a
    // `PossibleValuesParser` must not flatten.
    let output = nestweaver_cmd()
        .args([
            "export",
            "--format",
            "msgpack",
            "--scope",
            "vault",
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert_ne!(
        output.status.code(),
        Some(0),
        "msgpack cannot represent the vault subgraph and must say so"
    );
    assert_ne!(
        output.status.code(),
        Some(64),
        "`--scope vault` is a LEGAL value that this format cannot satisfy; \
         reporting it as a usage error would tell the caller to fix the \
         spelling of a word that is spelled correctly.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A read-only `ui` writes nothing, so S2 does not apply to it. Pinned so a
/// later widening of the guard cannot quietly break plain `nestweaver ui`.
#[test]
fn ui_without_watch_is_not_subject_to_the_s2_guard() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("some-checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let ambient_db = dir.path().join("ambient.lbug");

    // Port 0 is refused by the server, so this cannot bind and hang; what
    // matters is only that it is NOT the usage refusal.
    let output = nestweaver_cmd()
        .current_dir(&cwd)
        .env("NESTWEAVER_DB", &ambient_db)
        .args(["ui", "--no-open", "--port", "0"])
        .timeout(std::time::Duration::from_secs(20))
        .output()
        .unwrap();
    assert_ne!(
        output.status.code(),
        Some(64),
        "plain `ui` reads; S2 must not refuse it: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── nw-285: a corrupt database must fail closed, not die on a signal ─────

/// nw-285 / F-CLI-3. Opening a corrupted `.lbug` killed the process with
/// SIGSEGV (exit 139) and printed nothing at all — indistinguishable from
/// being killed by something else. The fault is in the vendored lbug C++
/// (`StorageManager::recover` -> `PrimaryKeyIndex::load`), a pinned crates.io
/// dependency, so this pins OUR half of the contract: whatever the engine does
/// internally, the process terminates NORMALLY with a diagnosable message.
///
/// Corruption starts well PAST the header on purpose. A header/magic-byte
/// check would pass the real reproduction — which overwrote 40%-60% of a
/// 5.6 GB file — and would therefore pass its own regression test while
/// missing the fault. That is the exact failure class this release exists to
/// remove, so the fixture is built to rule it out.
///
/// Several ranges are exercised because WHICH range faults is a function of
/// where this fixture's index pages happen to land: some return an ordinary
/// `Err`, some take the process down. Both are acceptable outcomes; dying on a
/// signal is not. Asserting the invariant over the set is what keeps this test
/// honest at fixture scale instead of pinning one lucky offset.
#[test]
#[cfg(unix)]
fn opening_a_corrupted_database_never_dies_on_a_signal() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::process::ExitStatusExt;

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    // Enough symbols that the on-disk index has real structure to corrupt: a
    // near-empty database keeps all its data in the WAL, where this recipe
    // would touch nothing and the test would pass vacuously.
    for i in 0..120 {
        let mut body = String::new();
        for j in 0..20 {
            body.push_str(&format!(
                "def f{i}_{j}(x):\n    return x + {j}\n\nclass C{i}_{j}:\n    def g(self):\n        return f{i}_{j}(1)\n\n"
            ));
        }
        std::fs::write(repo.join(format!("m{i}.py")), body).unwrap();
    }
    let pristine = dir.path().join("pristine.lbug");
    nestweaver_cmd()
        .args(["index", "--db"])
        .arg(&pristine)
        .arg("--repo")
        .arg(&repo)
        .assert()
        .success();

    let size = std::fs::metadata(&pristine).unwrap().len();
    assert!(
        size > 1024 * 1024,
        "the fixture database is {size} bytes — too small for the corruption to \
         land on real on-disk index structures, which would make this test pass \
         for the wrong reason"
    );

    let mut crashed_at_least_once = false;
    for (index, (from, to)) in [(0.005, 0.2), (0.05, 0.95), (0.4, 0.6)].iter().enumerate() {
        let db = dir.path().join(format!("corrupt{index}.lbug"));
        std::fs::copy(&pristine, &db).unwrap();
        let start = (size as f64 * from) as u64;
        let end = (size as f64 * to) as u64;
        assert!(
            start > 8192,
            "corruption must begin past any header, or a header check could \
             satisfy this test without touching the fault"
        );
        {
            let mut file = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
            file.seek(SeekFrom::Start(start)).unwrap();
            file.write_all(&vec![0xFFu8; (end - start) as usize])
                .unwrap();
            file.sync_all().unwrap();
        }

        let output = nestweaver_cmd()
            .args(["brain", "status", "--db"])
            .arg(&db)
            .arg("--json")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // THE PRIMARY ASSERTION.
        assert!(
            output.status.signal().is_none(),
            "corrupting {from}-{to} of the database killed the process with \
             signal {:?} (139 == SIGSEGV); it must fail closed instead. \
             stderr: {stderr}",
            output.status.signal()
        );

        if output.status.code() == Some(0) {
            // This range did not disturb anything the open reads. Not a
            // failure — just a range that proves nothing.
            continue;
        }

        // Whatever the engine did, the user must be able to act on it.
        // Matched on the FILE NAME, not the full path: miette hard-wraps the
        // rendered diagnostic at the terminal width, so a long temp path is
        // split across lines and a whole-path `contains` would fail on a
        // message that does name it.
        let file_name = db.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            stderr.contains(&file_name),
            "the error must name the database it could not open: {stderr}"
        );
        if stderr.contains("the storage engine crashed while reading it") {
            crashed_at_least_once = true;
            assert!(
                stderr.contains("restore") || stderr.contains("re-index"),
                "a crash attribution must offer a way out: {stderr}"
            );
        }
    }

    // If NO range faulted, this fixture never exercised the guard and the test
    // would be green while proving nothing — say so rather than pass.
    assert!(
        crashed_at_least_once,
        "no corruption range reached the crashing code path, so this run did \
         not exercise the crash-attribution guard at all"
    );
}

// ── nw-309: an exists-but-unopenable --db must fail on the store ─────────

/// nw-309 / F-CLI-1. A `--db` path that EXISTS but is not a database was
/// admitted to the daemon-autostart route, and the client's boot-readiness
/// wait has no failure channel for a daemon that never gets far enough to
/// publish a live PID — so the caller paid the full 30s boot ceiling before
/// the direct path produced the correct exit 1. A genuinely NONEXISTENT path
/// was fast, because that case had a guard.
///
/// Pinned to the DAEMON route (`env_remove`) because the stall only existed on
/// the route that dials. The ceiling is left at its default and the budget is
/// far under it, so this test cannot be satisfied by shortening the timeout —
/// only by giving the decision a failure channel.
#[test]
fn a_db_that_is_not_a_database_fails_fast_without_paying_the_boot_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake.lbug");
    std::fs::write(&fake, b"hello not a db").unwrap();

    let start = std::time::Instant::now();
    let output = Command::cargo_bin("nestweaver")
        .unwrap()
        .args(["stale-check", "--db"])
        .arg(&fake)
        .env_remove("NESTWEAVER_NO_DAEMON")
        .env_remove("NESTWEAVER_ALLOW_NO_DAEMON")
        .env_remove("NESTWEAVER_DB")
        .env("NESTWEAVER_DAEMON_BOOT_TIMEOUT_SECS", "30")
        .timeout(std::time::Duration::from_secs(60))
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert_ne!(output.status.code(), Some(0), "a broken DB must not exit 0");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "took {elapsed:?} against a 30s ceiling — an unopenable database is \
         still being handed to the daemon route, where 'will never boot' is \
         indistinguishable from 'still booting'. stderr: {stderr}"
    );
    // And the failure must say what is wrong, not just that something is.
    assert!(
        stderr.contains("fake.lbug"),
        "the error must name the path: {stderr}"
    );
    assert!(
        stderr.contains("not a NestWeaver database"),
        "the error must say WHY it is unusable: {stderr}"
    );
}

/// The companion: a genuinely missing `--db` keeps its existing, different
/// answer. nw-309 is about closing the gap between "missing" and "present but
/// broken", not about collapsing them into one message.
#[test]
fn a_missing_db_still_reports_db_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.lbug");

    nestweaver_cmd()
        .args(["stale-check", "--db"])
        .arg(&missing)
        .assert()
        .failure()
        .stderr(contains("nope.lbug"));
    assert!(
        !missing.exists(),
        "a failed lookup must not create a database"
    );
}

// ---------------------------------------------------------------------------
// nw-297 — a summary printed beside a truncated list must describe the
// POPULATION, not the page.
// ---------------------------------------------------------------------------

/// Build a vault with a known, unequal split: six links that resolve at a lower
/// confidence tier (unique global filename-stem match, 0.90) and three that
/// resolve to nothing at all.
///
/// The store emits the low-confidence group first, so any page shorter than six
/// is a pure sample of the benign category — which is exactly the shape that
/// made a 226-unresolved vault print `0 unresolved (genuinely broken)`.
fn broken_links_vault(root: &std::path::Path) -> std::path::PathBuf {
    let vault = root.join("vault");
    std::fs::create_dir_all(vault.join("targets")).unwrap();
    std::fs::create_dir_all(vault.join("sources")).unwrap();
    for i in 1..=6 {
        // Title deliberately unlike the stem, so the link misses the 1.0
        // unique-title tier and lands on the 0.90 global-stem tier.
        std::fs::write(
            vault.join(format!("targets/stemkey-{i}.md")),
            format!("---\ntitle: Utterly Different Title {i}\n---\n\nbody\n"),
        )
        .unwrap();
        std::fs::write(
            vault.join(format!("sources/src{i}.md")),
            format!("---\ntitle: Source {i}\n---\n\nSee [[stemkey-{i}]]\n"),
        )
        .unwrap();
    }
    for i in 1..=3 {
        std::fs::write(
            vault.join(format!("sources/miss{i}.md")),
            format!("---\ntitle: Miss {i}\n---\n\nSee [[Nonexistent Note {i}]]\n"),
        )
        .unwrap();
    }
    vault
}

fn indexed_broken_links_db(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let vault = broken_links_vault(dir.path());
    let db = dir.path().join("brain.lbug");
    nestweaver_cmd()
        .args(["brain", "add"])
        .arg(&vault)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();
    db
}

/// The headline classification must not move when `--limit` moves. A page of
/// four said `0 unresolved (genuinely broken)` on a vault holding three.
#[test]
fn broken_links_classification_counts_the_population_not_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let db = indexed_broken_links_db(&dir);

    let full = nestweaver_cmd()
        .args(["brain", "broken-links", "--limit", "100", "--db"])
        .arg(&db)
        .assert()
        .success();
    let full = String::from_utf8(full.get_output().stdout.clone()).unwrap();
    assert!(
        full.contains("3 unresolved (genuinely broken), 6 resolved"),
        "the whole population is 3 unresolved / 6 lower-tier: {full}"
    );

    let page = nestweaver_cmd()
        .args(["brain", "broken-links", "--limit", "4", "--db"])
        .arg(&db)
        .assert()
        .success();
    let page = String::from_utf8(page.get_output().stdout.clone()).unwrap();
    assert!(
        page.contains("Broken / ambiguous wikilinks (4 of 9)"),
        "the page itself is still bounded by --limit: {page}"
    );
    assert!(
        page.contains("3 unresolved (genuinely broken), 6 resolved"),
        "the classification describes the population, not the page: {page}"
    );
}

/// `--json` must carry the same split, so an agent does not have to re-derive
/// it from a page that cannot answer the question.
#[test]
fn broken_links_json_carries_the_population_split() {
    let dir = tempfile::tempdir().unwrap();
    let db = indexed_broken_links_db(&dir);

    let out = nestweaver_cmd()
        .args(["brain", "broken-links", "--limit", "4", "--json", "--db"])
        .arg(&db)
        .assert()
        .success();
    let out = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();

    assert_eq!(value["total"], 9, "payload: {out}");
    assert_eq!(value["returned"], 4, "payload: {out}");
    assert_eq!(
        value["unresolved"], 3,
        "the genuinely-broken count is a property of the vault: {out}"
    );
    assert_eq!(
        value["low_confidence"], 6,
        "and so is the benign count: {out}"
    );
}

/// `memory lint` is the second surface onto the same `broken_links` call, so it
/// corroborated the first only because it shared the defect.
#[test]
fn memory_lint_splits_the_broken_wikilink_count() {
    let dir = tempfile::tempdir().unwrap();
    let db = indexed_broken_links_db(&dir);

    let out = nestweaver_cmd()
        .args(["memory", "lint", "--db"])
        .arg(&db)
        .assert()
        .success();
    let out = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        out.contains("broken wikilinks:      9 (3 genuinely broken, 6 lower-tier resolutions)"),
        "the bare length conflates two categories: {out}"
    );
}

// ---------------------------------------------------------------------------
// nw-287 — a precondition must test the operation the caller will perform.
// ---------------------------------------------------------------------------

/// `chmod 000` on a directory leaves `stat(2)` working — that needs `+x` on the
/// PARENT, not on the directory itself — so `exists()` and `is_dir()` both pass
/// on a vault that cannot be ENUMERATED. Only `read_dir` fails, two layers down,
/// where the empty scan was indistinguishable from "the user deleted every
/// note": `brain refresh` reported rc=0 and dropped the whole vault.
fn unreadable_vault(dir: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("keep.md"),
        "---\ntitle: Keep Me\n---\n\nimportant content\n",
    )
    .unwrap();
    let db = dir.path().join("brain.lbug");
    nestweaver_cmd()
        .args(["brain", "add"])
        .arg(&vault)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();
    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o000)).unwrap();
    (vault, db)
}

fn make_readable_again(vault: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(vault, std::fs::Permissions::from_mode(0o755));
}

fn note_count(db: &std::path::Path) -> String {
    let out = nestweaver_cmd()
        .args(["brain", "status", "--db"])
        .arg(db)
        .assert()
        .success();
    String::from_utf8(out.get_output().stdout.clone()).unwrap()
}

#[test]
fn brain_refresh_refuses_a_vault_it_cannot_enumerate() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (vault, db) = unreadable_vault(&dir);

    let assertion = nestweaver_cmd()
        .args(["brain", "refresh"])
        .arg(&vault)
        .arg("--db")
        .arg(&db)
        .timeout(std::time::Duration::from_secs(60))
        .assert();
    let output = assertion.get_output().clone();
    make_readable_again(&vault);

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "an unreadable vault is an error, not an empty vault: stderr={stderr}"
    );
    assert!(
        stderr.contains("cannot be read"),
        "the error must say the directory could not be READ, not that it is not a \
         directory — it is one: {stderr}"
    );

    let status = note_count(&db);
    assert!(
        status.contains("Notes:     1"),
        "the indexed note must survive a refresh that could not see it: {status}"
    );
}

#[test]
fn brain_watch_refuses_a_vault_it_cannot_enumerate() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (vault, db) = unreadable_vault(&dir);

    let assertion = nestweaver_cmd()
        .args(["brain", "watch"])
        .arg(&vault)
        .arg("--db")
        .arg(&db)
        .timeout(std::time::Duration::from_secs(60))
        .assert();
    let output = assertion.get_output().clone();
    make_readable_again(&vault);

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "watch must refuse before it starts a watcher on a directory it cannot \
         enumerate: stderr={stderr}"
    );
    assert!(
        stderr.contains("cannot be read"),
        "the error must name the readability failure: {stderr}"
    );
}

/// The same `!exists() || !is_dir()` shape guards `detect-implicit-projects`,
/// which enumerates the vault too. Not named in the nw-287 report — found by
/// asking where else the property has to hold.
#[test]
fn detect_implicit_projects_refuses_a_vault_it_cannot_enumerate() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (vault, db) = unreadable_vault(&dir);

    let assertion = nestweaver_cmd()
        .args(["detect-implicit-projects", "--vault"])
        .arg(&vault)
        .arg("--db")
        .arg(&db)
        .timeout(std::time::Duration::from_secs(60))
        .assert();
    let output = assertion.get_output().clone();
    make_readable_again(&vault);

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "an unreadable vault is an error here too: stderr={stderr}"
    );
    assert!(
        stderr.contains("cannot be read"),
        "the error must name the readability failure: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// nw-308 / F-DC-13 — `hubs` / `bridges` must disclose stale rankings on the
// JSON payload, not only on stderr.
// ---------------------------------------------------------------------------

/// Index a small repo and return its database path.
fn indexed_ranking_db(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("a.js"),
        "import { b } from './b.js';\nexport function a() { return b(); }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("b.js"),
        "export function b() { return 1; }\nexport function c() { return b(); }\n",
    )
    .unwrap();
    let db = dir.path().join("graph.lbug");
    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();
    db
}

fn ranking_json(db: &std::path::Path, command: &str) -> serde_json::Value {
    let out = nestweaver_cmd()
        .args([command, "--json", "--db"])
        .arg(db)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{command} --json: {e}\n{stdout}"))
}

/// A database whose resolver-generation record is absent is exactly the
/// upgrade case: every repo's edges predate the current resolver, so every
/// ranking is stale. `warn_stale_resolver_rankings` already says so — on
/// stderr, where the `--json` consumer most likely to act on it cannot see it.
fn forget_resolver_generation(db: &std::path::Path) {
    let sidecar = sidecar_path(db, ".resolver_generation.json");
    std::fs::remove_file(&sidecar).unwrap();
}

#[test]
fn hubs_json_discloses_stale_rankings() {
    let dir = tempfile::tempdir().unwrap();
    let db = indexed_ranking_db(&dir);

    // Freshly indexed: current by construction, and the disclosure must not
    // cry wolf.
    let fresh = ranking_json(&db, "hubs");
    assert_eq!(
        fresh["rankings_stale"], false,
        "a repo indexed by this binary is not stale: {fresh}"
    );
    assert!(
        fresh["hubs"].is_array(),
        "the rows keep their own key: {fresh}"
    );

    forget_resolver_generation(&db);
    let stale = ranking_json(&db, "hubs");
    assert_eq!(
        stale["rankings_stale"], true,
        "no generation record means every repo predates the current resolver: {stale}"
    );
    assert!(
        stale["stale_repos"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "and the payload must name which repos, so the caller can re-index them: {stale}"
    );
}

/// `bridges` is the same shape one screen away. Fixing only `hubs` would be
/// the same scoping error this remediation is about.
#[test]
fn bridges_json_discloses_stale_rankings() {
    let dir = tempfile::tempdir().unwrap();
    let db = indexed_ranking_db(&dir);

    let fresh = ranking_json(&db, "bridges");
    assert_eq!(fresh["rankings_stale"], false, "payload: {fresh}");
    assert!(fresh["bridges"].is_array(), "payload: {fresh}");

    forget_resolver_generation(&db);
    let stale = ranking_json(&db, "bridges");
    assert_eq!(stale["rankings_stale"], true, "payload: {stale}");
    assert!(
        stale["stale_repos"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "payload: {stale}"
    );
}

// ── nw-313 + nw-281(b): the selective delete verbs ─────────────────────────
//
// `interactions clear` and the extension teardown are both all-or-nothing, so
// one poisoned entry could only be removed by destroying every accumulated
// signal beside it. The engine primitives (`remove_node_score`,
// `remove_extension_key_durable`) landed with the sidecar work; these are the
// user-facing verbs, and the property they must hold is SELECTIVITY — the
// neighbouring entries have to survive, which is the whole reason the verbs
// exist and the one thing `clear` cannot do.

/// Seed a two-node interaction sidecar so "the other node survived" is
/// assertable rather than assumed.
fn seed_interaction_sidecar(db_path: &std::path::Path) {
    std::fs::write(
        sidecar_path(db_path, ".interactions.json"),
        r#"{"version":1,"last_compacted":0.0,"node_scores":{
             "sym:repo:keep":  {"access_count":3,"query_seed_count":1,
                                "result_used_count":1,"result_shown_count":2,
                                "last_accessed":1.0,"content_hash_at_access":null,
                                "distinct_sessions":1,"computed_score":0.5},
             "sym:repo:forget":{"access_count":9,"query_seed_count":4,
                                "result_used_count":0,"result_shown_count":9,
                                "last_accessed":2.0,"content_hash_at_access":null,
                                "distinct_sessions":2,"computed_score":0.9}}}"#,
    )
    .unwrap();
}

#[test]
fn interactions_forget_removes_one_node_and_leaves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());
    seed_interaction_sidecar(&db_path);

    nestweaver_cmd()
        .args([
            "interactions",
            "forget",
            "sym:repo:forget",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success()
        .stdout(contains("sym:repo:forget"));

    let after = std::fs::read_to_string(sidecar_path(&db_path, ".interactions.json")).unwrap();
    assert!(
        !after.contains("sym:repo:forget"),
        "the named node's interaction memory must be gone:\n{after}"
    );
    assert!(
        after.contains("sym:repo:keep"),
        "SELECTIVITY is the entire point of this verb — `clear` already removes \
         everything, and a forget that took the neighbours with it would be \
         `clear` with a longer name:\n{after}"
    );
}

#[test]
fn interactions_forget_separates_nothing_to_forget_from_success() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());
    seed_interaction_sidecar(&db_path);

    // Idempotent verbs are not errors, but the caller must still be able to
    // tell "I removed it" from "there was nothing there" — otherwise a typo'd
    // UID reports the same success as a real deletion.
    let output = nestweaver_cmd()
        .args([
            "interactions",
            "forget",
            "sym:repo:never-recorded",
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "a uid with no recorded memory must exit NOT_FOUND, not SUCCESS: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn extensions_unset_removes_one_key_and_leaves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());
    std::fs::write(
        sidecar_path(&db_path, ".extensions.json"),
        r#"{"sym:repo:widget": {"owner": "platform", "tier": "gold"},
            "sym:repo:other":  {"owner": "search"}}"#,
    )
    .unwrap();

    nestweaver_cmd()
        .args([
            "extensions",
            "unset",
            "sym:repo:widget",
            "owner",
            "--db",
            &db_path.display().to_string(),
        ])
        .assert()
        .success();

    let after = std::fs::read_to_string(sidecar_path(&db_path, ".extensions.json")).unwrap();
    let store: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert!(
        store["sym:repo:widget"]["owner"].is_null(),
        "the named key must be gone:\n{after}"
    );
    assert_eq!(
        store["sym:repo:widget"]["tier"], "gold",
        "the node's OTHER properties must survive — the only delete that \
         existed removed all of them:\n{after}"
    );
    assert_eq!(
        store["sym:repo:other"]["owner"], "search",
        "and so must the same key on a DIFFERENT node:\n{after}"
    );
}

#[test]
fn extensions_unset_separates_nothing_to_remove_from_success() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());
    std::fs::write(
        sidecar_path(&db_path, ".extensions.json"),
        r#"{"sym:repo:widget": {"owner": "platform"}}"#,
    )
    .unwrap();

    let output = nestweaver_cmd()
        .args([
            "extensions",
            "unset",
            "sym:repo:widget",
            "tier",
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "a key that was never set must exit NOT_FOUND: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// ── nw-295: `--since` was never parsed ────────────────────────────────────
//
// `since` went straight into `WHERE n.modified_at >= $since`. `modified_at` is
// a String column, so that is a LEXICOGRAPHIC comparison which cannot fail:
// `"garbage"` leads with `'g'` (0x67) and every stored timestamp with `'2'`
// (0x32), so an unparseable value matched no note and silently dropped every
// Note and Section from the answer — byte-identical to `--since 2099-12-31`.
//
// The failure direction is the harmful one. It does not no-op; it narrows the
// result toward emptiness while reporting success, so the caller reads "this
// project has no notes" off a typo.

/// Every `--since` that reaches a lexicographic comparison, with the command
/// name and the flag's own value. `brain refresh` is deliberately in the list:
/// it already validated (via `parse_iso8601_to_system_time`) before nw-295, and
/// its presence here is what makes this a sweep over the flag rather than a
/// re-statement of the two sites that were broken.
const SINCE_ACCEPTING_COMMANDS: &[&[&str]] = &[
    &["project-context", "anything"],
    &["brain", "context", "anything"],
];

#[test]
fn an_unparseable_since_is_refused_rather_than_silently_matching_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

    for command in SINCE_ACCEPTING_COMMANDS {
        let output = nestweaver_cmd()
            .args(*command)
            .args(["--db", &db_path.display().to_string()])
            .args(["--since", "garbage"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "`{}` --since garbage must FAIL. Exiting 0 with a filtered answer is \
             how a typo reads as 'this project has no recent notes'.\nstdout={}\nstderr={stderr}",
            command.join(" "),
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.contains("since") || stderr.contains("garbage"),
            "the refusal must name the flag or the value it rejected, or the \
             caller cannot tell it apart from any other failure:\n{stderr}"
        );
    }
}

/// The counterweight, and the one that matters most: a validator that rejects
/// a currently-working input is a regression dressed as a fix. A bare
/// `YYYY-MM-DD` is not RFC 3339, is the natural thing to type, and works
/// today — an Rfc3339-only parser would break it.
#[test]
fn a_bare_calendar_date_is_still_accepted_by_since() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

    for command in SINCE_ACCEPTING_COMMANDS {
        for value in [
            "2026-01-31",
            "2026-01-31T00:00:00Z",
            "2026-01-31T02:00:00+02:00",
        ] {
            let output = nestweaver_cmd()
                .args(*command)
                .args(["--db", &db_path.display().to_string()])
                .args(["--since", value])
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            // The command may still fail for its OWN reasons (no such project
            // in an empty graph); what it may not do is reject the timestamp.
            assert!(
                !stderr.contains("--since") && !stderr.contains(&format!("'{value}'")),
                "`{}` --since {value} must be accepted:\n{stderr}",
                command.join(" ")
            );
        }
    }
}

// ── F-DC-11: `summary --level cluster` computed `total` AFTER the cap ──────
//
// `generate_cluster_summaries` truncated to 50 and returned a bare `Vec`,
// which has nowhere to say "I dropped some". The CLI then took `total` from
// the already-capped vector, so a 71,184-community graph reported
// `{returned: 50, total: 50, truncated: false}` — the cap made invisible in
// exactly the two fields that exist to disclose it.
//
// The honesty machinery already existed for `SummaryLevel::Symbol`
// (`generate_symbol_summaries_bounded` -> `cap_dropped`) and was wired for
// that level only.

/// 60 modules with three functions each, calling only within their own module.
/// That is 60 disjoint components of size 3 — comfortably over the cap of 50,
/// and with no cross-module edge that could let the clusterer merge them.
fn write_sixty_disjoint_communities(root: &std::path::Path) {
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 1..=60 {
        std::fs::write(
            src.join(format!("m{i}.js")),
            format!(
                "export function alpha{i}() {{ return beta{i}() + gamma{i}(); }}\n\
                 export function beta{i}() {{ return gamma{i}(); }}\n\
                 export function gamma{i}() {{ return {i}; }}\n"
            ),
        )
        .unwrap();
    }
}

#[test]
fn summary_at_cluster_level_reports_the_pre_cap_match_count() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    write_sixty_disjoint_communities(dir.path());

    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(dir.path())
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    let output = nestweaver_cmd()
        .args(["summary", "--level", "cluster", "--json"])
        .arg("--db")
        .arg(&db_path)
        .args(["--token-budget", "0"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("summary --json must emit JSON ({e}):\n{stdout}"));

    let returned = value["returned"].as_u64().unwrap();
    let total = value["total"].as_u64().unwrap();
    let truncated = value["truncated"].as_bool().unwrap();

    // The counterweight first: if the cap did not bite, this test proves
    // nothing and must say so rather than passing vacuously.
    assert_eq!(
        returned, 50,
        "this fixture exists to exercise the 50-cluster cap; if it did not \
         bite, the assertions below are vacuous:\n{stdout}"
    );
    assert!(
        total > returned,
        "`total` must count what MATCHED, not what survived the cap. Reporting \
         `total == returned` beside a capped list is not merely imprecise — it \
         is the one number a caller would use to decide whether to look \
         further, saying there is nothing further to look at. \
         got total={total}, returned={returned}"
    );
    assert!(
        truncated,
        "`returned == total` and `truncated: false` together are a claim that \
         the answer is complete. It was not."
    );
}

/// The third offender, and the reason this file sweeps all four levels rather
/// than fixing the one that was reported. `HUB_COUNT` is an internal 30 that
/// the caller never stated, so `total: 30` is not a truncation notice — it is
/// a claim that the graph HAS thirty hubs.
///
/// `File` is uncapped and `Symbol`'s cap is 500, so on this fixture only
/// `Cluster` (50) and `Hub` (30) can bite; measured before the fix, both
/// reported `total == returned` and `truncated: false`.
#[test]
fn summary_at_hub_level_reports_the_candidate_population() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    write_sixty_disjoint_communities(dir.path());

    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(dir.path())
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    let output = nestweaver_cmd()
        .args(["summary", "--level", "hub", "--json"])
        .arg("--db")
        .arg(&db_path)
        .args(["--token-budget", "0"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("summary --json must emit JSON ({e}):\n{stdout}"));

    let returned = value["returned"].as_u64().unwrap();
    let total = value["total"].as_u64().unwrap();
    assert_eq!(
        returned, 30,
        "this fixture exists to exercise the internal 30-hub bound; if it did \
         not bite the assertion below is vacuous:\n{stdout}"
    );
    assert!(
        total > returned,
        "180 symbols carry code edges in this fixture, so 30 is a selection \
         from a much larger candidate set, not the size of it. \
         got total={total}, returned={returned}"
    );
    assert!(
        value["truncated"].as_bool().unwrap(),
        "a bound the caller never asked for and cannot see must at minimum be \
         disclosed:\n{stdout}"
    );
}

// ── nw-289 (deeper property): a generation advance orphans its artifacts ───
//
// `.manifests.json` is an identity- AND generation-bound artifact: its
// envelope records `source_graph_generation`, and `load_manifest_cache_for_db`
// refuses to decode it when that no longer matches the live graph. The
// deletion path already handles this — it reads the manifest payload at
// generation N, advances to N+1, and republishes at N+1. The index/watcher
// path advances the generation and does not.
//
// A markdown/vault index cannot change any code manifest, so the payload stays
// correct while its BINDING goes stale. The graph then has no manifests for as
// long as nobody runs a code index, and every CLI consumer loads them with
// `.unwrap_or_default()` — so the failure is not an error, it is
// `dead-code` losing its manifest-driven entry points and `suggest-links`
// losing its cross-repo signal, both silently.

#[test]
fn a_vault_index_does_not_orphan_the_code_manifest_cache() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"demo-pkg","version":"1.2.3","dependencies":{"left-pad":"^1.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("src/a.js"),
        "export function hello(){return 1;}\n",
    )
    .unwrap();
    std::fs::write(vault.join("n.md"), "# Note\n").unwrap();

    let db_path = dir.path().join("test.lbug");

    nestweaver_cmd()
        .args(["index", "--repo"])
        .arg(&repo)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    // Baseline: the code index leaves the cache readable. If it did not, the
    // assertion after the vault index would prove nothing about the vault.
    {
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        let manifests = nestweaver_engine::load_manifest_cache_for_db(&store, &db_path)
            .expect("a code index must leave its own manifest cache readable");
        assert!(
            !manifests.is_empty(),
            "the fixture must actually produce manifests, or the test below is \
             vacuous"
        );
    }

    nestweaver_cmd()
        .args(["brain", "add"])
        .arg(&vault)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
    let manifests = nestweaver_engine::load_manifest_cache_for_db(&store, &db_path).expect(
        "indexing a VAULT advanced the graph generation and left the code \
         manifest cache bound to the previous one. A markdown index cannot \
         change a code manifest, so the payload is still correct — only its \
         binding went stale, and every CLI consumer swallows that with \
         `.unwrap_or_default()`",
    );
    assert!(
        !manifests.is_empty(),
        "and it must still carry the manifests, not merely decode to an empty \
         map — an invalidation that silently becomes 'this repo has no \
         manifests' is the same outcome with a different spelling"
    );
}

// ── F-DC-7: the adaptive cluster resolution has exactly one definition ─────
//
// Community IDs are ASSIGNMENT-dependent, so two runs at different
// resolutions produce two different ID SPACES rather than two orderings of
// one. Five copies of the 0.3/0.5 rule existed; the fifth
// (`generate_cluster_summaries`) was hard-coded to 1.0, which is how
// `summary --level cluster` came to emit IDs that `cluster <id>` could not
// resolve — 26 of 50.
//
// A behavioural test cannot catch the recurrence: every surviving copy agreed
// with the authority, so an end-to-end assertion passes on a tree with all of
// them restored. The defect appears only when one copy is edited and the
// others are not. What IS checkable is that no second copy exists.
//
// This lives in the integration suite, not beside the code, because a sweep
// that scans `src/main.rs` from within `src/main.rs` matches its own predicate
// and its own fixture. The alternative — teaching it to skip its own module —
// is the over-skipping that once let a `src/main.rs` check pass against a tree
// with the known bugs restored, by cutting the file at line 548 of 29,000.

fn cli_source() -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"))
        .expect("src/main.rs must be readable")
}

#[test]
fn the_adaptive_cluster_resolution_is_not_open_coded_in_the_cli() {
    let source = cli_source();
    // Matched as a PAIR: either literal alone appears legitimately — 0.5 is a
    // common default and 10_000 a common bound — so either alone would be a
    // sweep that cries wolf and gets deleted.
    let offenders: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("10_000") && line.contains("0.3"))
        .collect();
    assert!(
        offenders.is_empty(),
        "the adaptive cluster resolution must come from \
         `nestweaver_engine::default_cluster_resolution`, not be restated in \
         the CLI — a private copy silently re-partitions the graph and makes \
         the IDs every other command emits unaddressable. Found:\n{}",
        offenders.join("\n")
    );
}

/// The counterweight. A sweep's silence means nothing unless it can be shown
/// to SEE the shape it forbids, and to be reading the whole file.
#[test]
fn the_cluster_resolution_sweep_can_detect_what_it_forbids() {
    let restored = "                    let adaptive = if count > 10_000 { 0.3 } else { 0.5 };";
    assert!(
        restored.trim().contains("10_000") && restored.trim().contains("0.3"),
        "the predicate must match the literal form the removed copies had"
    );

    let source = cli_source();
    assert!(
        source.lines().count() > 30_000,
        "the sweep reads {} lines of src/main.rs; if that ever collapses to a \
         prefix, the test above passes by not looking",
        source.lines().count()
    );
}

// ── nw-311 / D2-E4: `service-summary` renders the pre-resolver shape ───────
//
// `nestweaver_engine::query::service_summary` is the single resolver: it
// returns the chosen service FLATTENED alongside `matched`, `alternatives` and
// `entry_points`, and carries the ambiguity text as
// `ServiceSummary::ambiguity_warning`. The CLI calls neither. Its daemon
// branch deserializes the envelope back down to a bare `Service` — which still
// succeeds, because the service is flattened, so nothing looks broken — and
// its direct branch re-implements the resolution with its own
// `list_services` + filter and its own copy of the warning.
//
// Two consequences. The daemon route drops the ambiguity disclosure entirely,
// which is the divergence nw-311 is about. And BOTH routes drop the entry
// points, which `--help` has always promised: "Show a service summary with
// entry points".

/// Two services with the same name, in two repos, each with one entry-point
/// symbol and one ordinary symbol so the entry-point filter has something to
/// exclude. Persistent (not in-memory) because the CLI opens by path.
fn seed_ambiguous_services(db_path: &std::path::Path) {
    use nestweaver_schema::{Repo, Service, Symbol, SymbolKind, Visibility};

    let store = nestweaver_store::GraphStore::open_or_create(db_path).unwrap();
    for i in 0..2 {
        let repo_uid = format!("repo:test:{i:012x}");
        store
            .insert_repo(&Repo {
                uid: repo_uid.clone(),
                url: format!("https://github.com/example/r{i}"),
                indexed_sha: "abc123".to_string(),
                staleness_commits_behind: 0,
                instance_id: "default".to_string(),
                name: Some(format!("r{i}")),
                root_path: None,
            })
            .unwrap();
        let svc_uid = format!("svc:{repo_uid}:{i:012x}");
        store
            .insert_service(&Service {
                uid: svc_uid.clone(),
                name: "checkout".to_string(),
                repo_uid: repo_uid.clone(),
                summary: None,
                summary_hash: None,
                embedding: None,
            })
            .unwrap();
        let mk = |suffix: &str, name: &str, entry: bool| Symbol {
            uid: format!("sym:{repo_uid}:{suffix}"),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.clone(),
            file_path: format!("src/{suffix}.rs"),
            start_line: 1,
            end_line: 2,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("{repo_uid}:{suffix}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: entry,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        let entry = mk("entry", "handler", true);
        let plain = mk("plain", "helper", false);
        store.insert_symbol(&entry).unwrap();
        store.insert_symbol(&plain).unwrap();
        store
            .batch_insert_service_symbol_edges(&[
                (svc_uid.as_str(), entry.uid.as_str()),
                (svc_uid.as_str(), plain.uid.as_str()),
            ])
            .unwrap();
    }
    drop(store);
}

#[test]
fn service_summary_json_carries_the_resolvers_disclosure() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    seed_ambiguous_services(&db_path);

    let output = nestweaver_cmd()
        .args(["service-summary", "checkout", "--json"])
        .arg("--db")
        .arg(&db_path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--json must emit JSON ({e}):\n{stdout}"));

    // The flattened `Service` must still be there — the envelope is a
    // SUPERSET, so an existing consumer keeps working.
    assert!(
        value.get("uid").is_some() && value["name"] == "checkout",
        "the payload must stay a superset of the bare Service it replaced:\n{stdout}"
    );
    assert_eq!(
        value["matched"], 2,
        "two services carry this name; a payload that cannot say so is how \
         `service-summary` silently answered one of several questions:\n{stdout}"
    );
    assert_eq!(
        value["alternatives"].as_array().map(Vec::len),
        Some(1),
        "the candidate it did NOT choose must be listed, or the caller cannot \
         re-ask unambiguously:\n{stdout}"
    );
    assert_eq!(
        value["entry_points"].as_array().map(Vec::len),
        Some(1),
        "`--help` says 'Show a service summary with entry points'; the one \
         entry-point symbol must be there and the ordinary symbol must \
         not:\n{stdout}"
    );
}

#[test]
fn service_summary_text_warns_about_ambiguity_and_lists_entry_points() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    seed_ambiguous_services(&db_path);

    let output = nestweaver_cmd()
        .args(["service-summary", "checkout"])
        .arg("--db")
        .arg(&db_path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("matches 2 services"),
        "an ambiguous name must warn, on stderr, naming the count:\n{stderr}"
    );
    assert!(
        stdout.contains("handler"),
        "the entry point `--help` promises must actually be printed:\n{stdout}"
    );
    assert!(
        !stdout.contains("helper"),
        "and only ENTRY POINTS — listing every symbol in the service would be \
         a different command:\n{stdout}"
    );
}

/// nw-261. Wrap position must not be an ambient input to the suite.
///
/// The failure this closes is real and has fired: a phrase the code emits
/// contiguously arrives at a `stderr.contains(...)` assertion split across
/// miette's box-drawing gutter, so the assertion fails for a RENDERING reason
/// while the message is exactly right. The audit found nine more assertions
/// with the same exposure in `tests/daemon_test.rs`, all inside one
/// Linux-gated test nobody can reproduce locally.
///
/// Flattening each assertion is the local fix and it only protects the
/// assertions someone remembered to flatten. `NESTWEAVER_DIAGNOSTIC_WIDTH`
/// pins the wrap column instead, which does two things flattening cannot:
/// it covers assertions nobody has audited, and it makes the property
/// TESTABLE — a wrap that previously depended on the tempdir path and the
/// terminal can now be forced.
///
/// The first assertion is the vacuity guard, and it is the same one
/// `flatten_diagnostic_recovers_a_phrase_miette_wrapped` uses: if the fixture
/// does not actually reproduce the wrap, the test below it proves nothing.
#[test]
fn a_diagnostic_phrase_survives_a_narrow_render() {
    // Chosen because width 40 wraps between `--repo` and `<path>`, which is
    // what makes the vacuity guard below meaningful.
    const PHRASE: &str = "index --repo <path>";
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.lbug");

    let narrow = nestweaver_cmd()
        .env("NESTWEAVER_DIAGNOSTIC_WIDTH", "40")
        .args(["hubs", "--db"])
        .arg(&missing)
        .output()
        .unwrap();
    let raw = String::from_utf8_lossy(&narrow.stderr).to_string();

    assert!(
        raw.contains("nestweaver index"),
        "the fixture must reach the diagnostic at all: {raw}"
    );
    assert!(
        !raw.contains(PHRASE),
        "the fixture must reproduce the WRAP at width 40, or this test proves \
         nothing — `NESTWEAVER_DIAGNOSTIC_WIDTH` is not being honoured and the \
         render used the ambient width: {raw}"
    );
    assert!(
        flatten_miette(&narrow.stderr).contains(PHRASE),
        "flattening must recover a phrase miette wrapped, which is the whole \
         reason a width-dependent assertion is a false failure: {raw}"
    );

    // Counterweight: unset must be TODAY's behaviour exactly. A knob that
    // changed the default would be a user-visible rendering change shipped to
    // make a test convenient.
    let ambient = nestweaver_cmd()
        .env_remove("NESTWEAVER_DIAGNOSTIC_WIDTH")
        .args(["hubs", "--db"])
        .arg(&missing)
        .output()
        .unwrap();
    assert!(
        flatten_miette(&ambient.stderr).contains(PHRASE),
        "{}",
        String::from_utf8_lossy(&ambient.stderr)
    );
}

/// nw-259(a). A token-budget cut must not tell the caller it hit `--limit`.
///
/// The `context` arm carries three caps and had one `truncated` boolean and one
/// `limit` field to explain all of them, so a BUDGET cut set `truncated` and
/// left `limit` at whatever the earlier cap set — reporting "TRUNCATED at limit
/// 500 — pass --limit for more" for a cut `--limit` did not make and cannot
/// undo. An agent following that remedy raises `--limit`, gets the same rows,
/// and has no next move. That is an nw-334 instance none of the four shipped
/// tiers can see: it is not a `CliDiagnostic`, not an environment variable, and
/// not a backtick-quoted subcommand.
///
/// The counterweight is the second half: a genuine LIMIT cut must still say
/// `--limit`, or deleting the message entirely would pass.
#[test]
fn a_context_budget_cut_names_the_budget_and_a_limit_cut_names_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    let db_path = dir.path().join("test.lbug");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("a.js"),
        "export function mainA(x){return helperB(x)+helperC(x);}\n\
         export function helperB(n){return helperC(n)+1;}\n\
         export function helperC(n){return n*3;}\n\
         export function extraB(n){return n-1;}\n\
         export function otherC(n){return n+7;}\n",
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

    // `--stats` is what prints the truncation clause.
    let budget = nestweaver_cmd()
        .args([
            "--stats",
            "context",
            "mainA",
            "--token-budget",
            "1",
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    let stderr = flatten_miette(&budget.stderr);
    assert!(
        stderr.contains("TRUNCATED"),
        "the fixture must actually truncate, or this test proves nothing: {stderr}"
    );
    assert!(
        !stderr.contains("pass --limit for more"),
        "a token-budget cut prescribed `--limit`, which cannot change the \
         outcome — the budget is what cut, and it cut LAST: {stderr}"
    );
    assert!(
        stderr.contains("--token-budget"),
        "the message must name the cap that actually cut: {stderr}"
    );

    let limited = nestweaver_cmd()
        .args([
            "--stats",
            "context",
            "mainA",
            "--limit",
            "1",
            "--db",
            &db_path.display().to_string(),
        ])
        .output()
        .unwrap();
    let stderr = flatten_miette(&limited.stderr);
    assert!(
        stderr.contains("pass --limit for more"),
        "a real limit cut must still say so, or deleting the message would \
         satisfy the assertions above: {stderr}"
    );
}

/// nw-366. `frontmatter_raw` is an additive column populated by
/// `ALTER TABLE ... DEFAULT ''`, which fills the COLUMN and not the DATA. Every
/// note indexed by 8.0.0 therefore reads back empty, both regex collectors
/// `continue` past it, and an upgrader who does not re-index keeps nw-298's
/// symptom on a binary that contains the fix — silently.
///
/// Two halves, and the second is the one this project keeps getting wrong:
///
///  1. `brain status` must DISCLOSE the deficit, and
///  2. the remedy it prints must be EXECUTED here, not asserted as a string.
///
/// The ticket proposed `brain add --force`. There is no `--force` on `Add` or
/// on `Refresh` — shipping that string would have reintroduced the exact class
/// of defect (nw-328, nw-318, nw-259a) this fix is disclosing. `brain refresh
/// <root>` is run below and its effect is measured.
#[test]
fn a_pre_column_note_is_disclosed_and_the_printed_remedy_actually_fixes_it() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("note.md"),
        "---\nstatus: loadbearingtoken\n---\n\n# A note\n\nBody text.\n",
    )
    .unwrap();
    let db = dir.path().join("scratch.lbug");

    // A real index, so the row shape is the product's own and not a fixture's.
    nestweaver_cmd()
        .args(["brain", "add"])
        .arg(&vault)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();

    // Now reproduce the pre-column state exactly: the Note row keeps its parsed
    // `frontmatter` (8.0.0 wrote that) and loses `frontmatter_raw` (the column
    // did not exist). `content_hash` is PRESERVED — so this also tests the
    // claim the remedy rests on, that the vault path has no unchanged-file
    // short-circuit. If `brain refresh` skipped the file, the assertions below
    // would fail and the remedy would be wrong.
    {
        let store = nestweaver_store::GraphStore::open(&db).unwrap();
        let notes = store.list_notes(None).unwrap();
        assert_eq!(notes.len(), 1, "precondition: one indexed note");
        for note in notes {
            assert!(
                note.frontmatter_raw.is_some(),
                "precondition: this binary DOES write frontmatter_raw"
            );
            store.delete_note_cascade(&note.uid).unwrap();
            let mut legacy = note.clone();
            legacy.frontmatter_raw = None;
            store.insert_note(&legacy).unwrap();
        }
        assert_eq!(
            store
                .count_notes_predating_frontmatter_indexing(None)
                .unwrap(),
            1,
            "precondition: the deficit is present"
        );
    }

    // Half 1 — the deficit is DISCLOSED, and the disclosure names the vault the
    // remedy has to be pointed at.
    let status = nestweaver_cmd()
        .args(["brain", "status", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        rendered.contains("brain refresh"),
        "a deficit nobody can see is the defect itself: {rendered}"
    );
    assert!(
        rendered.contains(vault.to_str().unwrap()),
        "and the remedy must name WHICH vault to refresh: {rendered}"
    );
    assert!(
        !rendered.contains("--force"),
        "there is no --force on `brain add` or `brain refresh`; printing one \
         would be the very defect being disclosed: {rendered}"
    );

    // Half 2 — RUN the remedy that was printed, then measure.
    nestweaver_cmd()
        .args(["brain", "refresh"])
        .arg(&vault)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();

    {
        let store = nestweaver_store::GraphStore::open_read_only(&db).unwrap();
        assert_eq!(
            store
                .count_notes_predating_frontmatter_indexing(None)
                .unwrap(),
            0,
            "the printed remedy must actually clear the deficit"
        );
        let hits = store
            .regex_search("loadbearingtoken", None, None, Some(10), Some(5_000))
            .unwrap();
        assert!(
            hits.results.iter().any(|r| r.kind == "Frontmatter"),
            "and the SYMPTOM must be gone: frontmatter text that is in the file \
             must be findable again, which is what the count stands in for: {:?}",
            hits.results
        );
    }

    // The counterweight: the disclosure must not fire on a healthy vault, or it
    // becomes noise that trains the operator to ignore it.
    let clean = nestweaver_cmd()
        .args(["brain", "status", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let clean_rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&clean.stdout),
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(
        !clean_rendered.contains("brain refresh"),
        "a healthy vault must produce no backfill warning: {clean_rendered}"
    );
}

/// nw-367. `<db>.wal.checkpoint` left by a crash makes every read-only open
/// report *"Cannot open database in read-only mode while checkpoint is in
/// progress. Please retry later."* as a bare `nestweaver::error` with no help
/// text — advice to WAIT for a state that nothing later removes.
///
/// Round 2 declined to classify it because a checkpoint genuinely can be in
/// progress. It IS decidable: the engine takes `F_WRLCK` POSIX record locks on
/// `<db>.checkpoint.{intent,apply}.lock`, the kernel releases them when the
/// holder dies, and `releaseCheckpointLocks` runs AFTER the artifacts are
/// removed — so "no lock held, artifacts present" cannot describe a healthy
/// checkpoint.
///
/// This must NOT reach the corrupt-WAL runbook. That runbook moves the frozen
/// WAL aside, which discards committed transactions and then demands a full
/// re-index. A read-write open replays it and removes the debris itself — and
/// this test RUNS that, rather than asserting the sentence.
#[test]
fn stale_checkpoint_debris_is_named_and_its_remedy_is_executed() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }

    // The observed state, built with plain file operations: a frozen WAL plus
    // both lock files, and NO process holding either lock. The lock files are
    // present deliberately — the item claimed they were causal and they are
    // not (a lock file that merely exists returns early in the engine), so
    // including them proves the classification does not depend on them.
    let frozen = dir.path().join("scratch.lbug.wal.checkpoint");
    std::fs::write(&frozen, b"").unwrap();
    std::fs::write(dir.path().join("scratch.lbug.checkpoint.apply.lock"), b"").unwrap();
    std::fs::write(dir.path().join("scratch.lbug.checkpoint.intent.lock"), b"").unwrap();

    let output = nestweaver_cmd()
        .args(["brain", "status", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("db_checkpoint_debris"),
        "no process holds either checkpoint lock or the database write lock, so \
         this is debris and must be a NAMED condition — not `nestweaver::error` \
         with no remedy at all: {combined}"
    );
    assert!(
        !combined.contains("Please retry later"),
        "a state that never clears must not tell the operator to wait: {combined}"
    );
    assert!(
        !combined.contains("MOVE ASIDE"),
        "and it must NOT reach the corrupt-WAL runbook: moving a frozen WAL \
         aside discards committed transactions. A read-write open replays it: \
         {combined}"
    );

    // The remedy, RUN. `nestweaver daemon --db <path> start` is named in the
    // help because starting the daemon is the canonical read-write open; the
    // operation it performs on the database is exactly this one, and doing it
    // here keeps the test free of a spawned process.
    {
        let _store = nestweaver_store::GraphStore::open(&db).unwrap();
    }
    assert!(
        !frozen.exists(),
        "the engine's own recovery path must have replayed the frozen log and \
         removed it — that is what makes the printed remedy true"
    );
    nestweaver_store::GraphStore::open_read_only(&db)
        .expect("and the read-only open the operator was trying to make now succeeds");
}

/// The counterweight, and the reason round 2 declined to classify at all: a
/// checkpoint that IS in progress must keep today's transient message. Held
/// here by a real `fcntl` write lock taken by this test process, which is
/// precisely the evidence the classifier consults.
#[test]
fn a_genuinely_held_checkpoint_lock_stays_transient() {
    use std::os::unix::io::AsRawFd;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }
    std::fs::write(dir.path().join("scratch.lbug.wal.checkpoint"), b"").unwrap();

    let apply_path = dir.path().join("scratch.lbug.checkpoint.apply.lock");
    let apply = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&apply_path)
        .unwrap();
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as libc::c_short;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    fl.l_start = 0;
    fl.l_len = 0;
    assert_eq!(
        unsafe { libc::fcntl(apply.as_raw_fd(), libc::F_SETLK, &fl) },
        0,
        "precondition: this test process holds the checkpoint write lock"
    );

    let output = nestweaver_cmd()
        .args(["brain", "status", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("checkpoint is in progress"),
        "precondition: the engine still refuses the read-only open: {combined}"
    );
    assert!(
        !combined.contains("db_checkpoint_debris"),
        "a LIVE checkpoint must not be called debris — that is an \
         unconditional attribution wearing the other hat: {combined}"
    );
    drop(apply);
}

/// nw-359 leg (2). `repair` never opens the database when the publication
/// marker is clean, so on a database that cannot be opened at all it prints
/// three true sentences — Database, Marker, "Index publication is CLEAN —
/// nothing to repair" — and exits 0.
///
/// (The item said it "prints nothing". It does not; the code prints three
/// lines. The precise version is the one with a fix: every sentence it prints
/// is TRUE and the EXIT CODE is the lie, and exit 0 is the one answer an
/// unattended caller acts on.)
#[test]
fn repair_does_not_report_success_over_a_database_it_cannot_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }
    // The nw-332 state, built without a crash: garbage in the WAL with no
    // `.shadow` beside it makes every open report an unreadable log.
    std::fs::write(dir.path().join("scratch.lbug.wal"), vec![0xABu8; 4096]).unwrap();
    let _ = std::fs::remove_file(dir.path().join("scratch.lbug.shadow"));

    let output = nestweaver_cmd()
        .args(["repair", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        output.status.code(),
        Some(0),
        "exit 0 declares a repair that did not happen, on a database that \
         cannot be opened: {combined}"
    );
    assert!(
        combined.contains("MOVE ASIDE"),
        "and it must reach the ONE runbook, not a second phrasing: {combined}"
    );

    // The same probe must ALSO see nw-367's state, which is why nw-367 had to
    // land first: without its discriminator this arm would either miss the
    // condition or send a frozen WAL to the move-aside runbook, which discards
    // committed transactions.
    let debris_dir = tempfile::tempdir().unwrap();
    let debris_db = debris_dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&debris_db).unwrap();
    }
    std::fs::write(debris_dir.path().join("scratch.lbug.wal.checkpoint"), b"").unwrap();
    let debris = nestweaver_cmd()
        .args(["repair", "--db"])
        .arg(&debris_db)
        .output()
        .unwrap();
    let debris_out = format!(
        "{}{}",
        String::from_utf8_lossy(&debris.stdout),
        String::from_utf8_lossy(&debris.stderr)
    );
    assert_ne!(debris.status.code(), Some(0), "{debris_out}");
    assert!(
        debris_out.contains("db_checkpoint_debris"),
        "and it must carry nw-367's classification, not the corrupt-WAL \
         runbook: {debris_out}"
    );
}

/// The counterweight, and it is what keeps the probe from being a blanket
/// refusal: a HEALTHY database with a clean publication marker must still
/// report clean and still exit 0. Without this, `repair` could satisfy the
/// test above by failing always.
#[test]
fn repair_still_reports_a_clean_publication_on_a_healthy_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }

    let output = nestweaver_cmd()
        .args(["repair", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(0), "{combined}");
    assert!(
        combined.contains("CLEAN"),
        "a healthy database is still reported clean: {combined}"
    );

    // And the JSON route must agree on BOTH facts, or the two routes disagree
    // about whether a repair happened — the class this repo keeps re-finding.
    let json = nestweaver_cmd()
        .args(["repair", "--json", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    assert_eq!(json.status.code(), Some(0));
    let payload: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("repair --json emits JSON");
    assert_eq!(payload["after"]["dirty"], serde_json::json!(false));
    assert_eq!(payload["error"], serde_json::Value::Null);
}

/// nw-359 leg (1). The product ships a runbook whose FIRST instruction is to
/// stop everything that opens this database, because starting a daemon against
/// a log whose records do not parse IS the crash-restart loop that took a graph
/// down for seven hours (nw-332). The product then auto-started one against
/// exactly that state, and what the operator saw was "daemon process exited
/// before becoming healthy" — a message about the spawn, describing a problem
/// in the database.
///
/// Autostart was a TRANSPORT decision made with no knowledge of STORAGE state:
/// not one frame in `ensure_daemon_impl` opened, stat'd or classified the
/// database. The guard is not new machinery — the store already classifies this
/// as `CorruptionKind::WalUnreadable` at the FFI boundary.
#[test]
fn a_command_that_would_autostart_refuses_an_unreadable_wal_and_names_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }
    std::fs::write(dir.path().join("scratch.lbug.wal"), vec![0xABu8; 4096]).unwrap();
    let _ = std::fs::remove_file(dir.path().join("scratch.lbug.shadow"));

    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let sock = tempfile::tempdir().unwrap();

    // The DEFAULT route, the one that auto-starts — which is the whole point of
    // the item. `env_remove` rather than absence: an inherited
    // NESTWEAVER_NO_DAEMON would silently move this test off the path it exists
    // to cover, and `every_cli_invocation_pins_its_daemon_routing` requires the
    // choice to be explicit for exactly that reason.
    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["brain", "status", "--db"])
        .arg(&db)
        .env_remove("NESTWEAVER_NO_DAEMON")
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("NESTWEAVER_SOCK_FALLBACK_DIR", sock.path())
        .env("NESTWEAVER_DAEMON_BOOT_TIMEOUT_SECS", "10")
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !combined.contains("did not become healthy")
            && !combined.contains("exited before becoming healthy"),
        "the refusal must name the DATABASE state, not report a spawn that was \
         never allowed to happen: {combined}"
    );
    assert!(
        combined.contains("db_wal_corrupt"),
        "and it must carry the corrupt-WAL classification so the CLI renders \
         the runbook rather than a transport error: {combined}"
    );
}

/// The counterweight, and it is the half that makes the guard honest: a
/// database that is merely UNREPLAYED, or carrying nw-367's checkpoint debris,
/// must still be allowed to start a daemon — a read-write open is the CORRECT
/// remedy for both, and for the debris it is the remedy this release prints. A
/// guard that refused there would be nw-333's unconditional attribution
/// pointing the other way.
#[test]
fn the_autostart_guard_only_refuses_a_log_no_open_can_replay() {
    use nestweaver_daemon::lifecycle::db_wal_unreadable;

    let dir = tempfile::tempdir().unwrap();

    // A database that does not exist yet: a cold start creating one.
    assert!(db_wal_unreadable(&dir.path().join("absent.lbug")).is_none());

    // A healthy database.
    let healthy = dir.path().join("healthy.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&healthy).unwrap();
    }
    assert!(db_wal_unreadable(&healthy).is_none());

    // nw-367's checkpoint debris: a read-only open is refused, and starting a
    // read-write daemon is the PUBLISHED remedy. Refusing here would break the
    // instruction shipped in the same release.
    let debris = dir.path().join("debris.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&debris).unwrap();
    }
    std::fs::write(dir.path().join("debris.lbug.wal.checkpoint"), b"").unwrap();
    assert!(
        nestweaver_store::GraphStore::open_read_only(&debris).is_err(),
        "precondition: the read-only open really is refused"
    );
    assert!(
        db_wal_unreadable(&debris).is_none(),
        "checkpoint debris must still be allowed to start a daemon — that IS \
         its remedy"
    );

    // And the one state it does refuse.
    let corrupt = dir.path().join("corrupt.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&corrupt).unwrap();
    }
    std::fs::write(dir.path().join("corrupt.lbug.wal"), vec![0xABu8; 4096]).unwrap();
    assert!(db_wal_unreadable(&corrupt).is_some());
}

/// nw-359 leg (3). `run` resolves the daemon decision exactly once, in
/// `resolve_use_daemon`, and `Commands::Instance` dropped it on the floor:
/// `run_instance` took no `use_daemon` at all, so `instance merge` connected —
/// and therefore auto-started a daemon — with the caller's bypass nowhere in
/// sight and nothing said about it.
///
/// The item's stated trigger was wrong and is corrected here. Bare
/// `NESTWEAVER_NO_DAEMON=1` routing through the daemon is CORRECT by policy:
/// `no_daemon_allowed_from` grants the bypass on `NESTWEAVER_ALLOW_NO_DAEMON`
/// alone, and `CI`/`GITHUB_ACTIONS` confer nothing, deliberately. The case that
/// matters is the one where the bypass IS granted.
///
/// The cost is concrete and is already recorded elsewhere in this repo:
/// `tests/error_remedy_test.rs` stops the daemon by hand after running this
/// exact command, with a comment noting that the auto-started daemon holds the
/// write lease "for its idle timeout — an hour" and would otherwise break the
/// re-index that follows. That workaround IS the bug report.
#[test]
fn instance_merge_discloses_that_it_cannot_honour_a_granted_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }

    let output = nestweaver_cmd()
        .args(["instance", "merge", "--from", "a", "--to", "b", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        stderr.contains("cannot be honoured"),
        "a bypass that is granted and then silently not honoured is the defect: \
         {stderr}"
    );
    assert!(
        stderr.contains("write lease"),
        "and the disclosure must name the COST, not just the fact — the lease \
         is what blocks the follow-up index: {stderr}"
    );
    assert!(
        stderr.contains(&format!("nestweaver daemon --db {} stop", db.display())),
        "and it must carry the command that releases it, substituted for THIS \
         database: {stderr}"
    );
}

/// The counterweight, and it is what keeps the disclosure from becoming noise:
/// with no bypass granted there is nothing to disclose. Bare
/// `NESTWEAVER_NO_DAEMON=1` without the opt-in is the SAME case — the bypass
/// was requested and REFUSED, so the daemon route is correct and expected — and
/// that is the half the item had backwards.
#[test]
fn instance_merge_says_nothing_when_no_bypass_was_granted() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }
    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let sock = tempfile::tempdir().unwrap();

    let output = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["instance", "merge", "--from", "a", "--to", "b", "--db"])
        .arg(&db)
        // Requested but NOT granted: policy says route through the daemon, and
        // no warning is owed for behaviour that is correct.
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env_remove("NESTWEAVER_ALLOW_NO_DAEMON")
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("NESTWEAVER_SOCK_FALLBACK_DIR", sock.path())
        .env("NESTWEAVER_DAEMON_BOOT_TIMEOUT_SECS", "30")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !stderr.contains("cannot be honoured"),
        "no bypass was granted, so nothing was dishonoured: {stderr}"
    );

    // Clean up whatever this test started, in its own runtime tree.
    let _ = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["daemon", "--db"])
        .arg(&db)
        .arg("stop")
        .env_remove("NESTWEAVER_NO_DAEMON")
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("NESTWEAVER_SOCK_FALLBACK_DIR", sock.path())
        .output();
}

/// nw-360, the residual of nw-312. `d565547f` closed the spelling half — a
/// bogus `--format` or `--scope` now exits 64 at parse time — and deliberately
/// preserved this case as a SEMANTIC refusal. That reasoning stands and the
/// exit code is not changed here.
///
/// What is wrong is the SHAPE. Two individually valid enums whose COMBINATION
/// is unsupported reached the user as a raw RPC error naming neither value:
/// `PossibleValuesParser` is per-argument and cannot see the other one, the
/// daemon's own good sentence arrived as a `tonic::Status` wrapped in
/// `.context("export_graph RPC failed")`, and `into_diagnostic` had no arm for
/// it — so it fell to `CliDiagnostic::General` and printed the transport's
/// words instead of the condition's.
#[test]
fn an_unsupported_format_scope_pair_is_named_not_relayed() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }

    let output = nestweaver_cmd()
        .args(["export", "--format", "msgpack", "--scope", "vault", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !combined.contains("RPC failed") && !combined.contains("invalid argument"),
        "the transport's words are not this condition's words: {combined}"
    );
    assert!(
        combined.contains("export_scope_unsupported"),
        "an unsupported COMBINATION must be a named condition: {combined}"
    );
    assert!(
        combined.contains("graphml"),
        "and the remedy must name a format that DOES satisfy --scope vault, or \
         it is a refusal with no next step: {combined}"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "semantic refusal, per d565547f — not a usage error: {combined}"
    );
}

/// The parity half, and the reason the check is a client-side PRE-FLIGHT rather
/// than two validations: the direct route and the daemon route must refuse the
/// same pair with the same words. They already disagreed once on this exact
/// argument — the daemon rejected a vault scope while the direct path emitted a
/// code-only file and reported success.
#[test]
fn both_export_routes_refuse_the_pair_identically() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }
    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let sock = tempfile::tempdir().unwrap();

    let bypassed = nestweaver_cmd()
        .args(["export", "--format", "msgpack", "--scope", "vault", "--db"])
        .arg(&db)
        .output()
        .unwrap();

    // The DEFAULT route. The pre-flight sits above the route split, so this
    // must not even reach the daemon — and therefore must not autostart one.
    let routed = StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
        .args(["export", "--format", "msgpack", "--scope", "vault", "--db"])
        .arg(&db)
        // This route is built raw rather than through `nestweaver_cmd`, so it
        // does not inherit the helper's width pin. Both sides must render at the
        // same width or this compares GEOMETRY, not words -- which is what it
        // was accidentally doing before, passing only because both routes
        // happened to wrap at the same ambient terminal width.
        .env("NESTWEAVER_DIAGNOSTIC_WIDTH", "1000")
        .env_remove("NESTWEAVER_NO_DAEMON")
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("NESTWEAVER_SOCK_FALLBACK_DIR", sock.path())
        .env("NESTWEAVER_DAEMON_BOOT_TIMEOUT_SECS", "10")
        .output()
        .unwrap();

    assert_eq!(
        String::from_utf8_lossy(&bypassed.stderr),
        String::from_utf8_lossy(&routed.stderr),
        "one condition, one sentence, whichever route the caller happened to take"
    );
    assert_eq!(bypassed.status.code(), routed.status.code());
    assert_eq!(
        std::fs::read_dir(runtime.path().join("nestweaver"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "an argument-only refusal must not start a daemon to deliver itself"
    );
}

/// The counterweight: the pair that IS supported must still work, or the
/// pre-flight could satisfy the tests above by refusing every export.
#[test]
fn a_supported_format_scope_pair_still_exports() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scratch.lbug");
    {
        let _store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
    }
    nestweaver_cmd()
        .args(["export", "--format", "graphml", "--scope", "vault", "--db"])
        .arg(&db)
        .assert()
        .success();
    nestweaver_cmd()
        .args(["export", "--format", "msgpack", "--scope", "code", "--db"])
        .arg(&db)
        .assert()
        .success();
}

// ── `server init-tls` is key rotation, not initialization ─────────────────

/// The six files `server init-tls` owns, mirroring
/// `nestweaver_engine::tls::MANAGED_FILES`.
const TLS_MANAGED: [&str; 6] = [
    "ca.pem",
    "ca-key.pem",
    "server.pem",
    "server-key.pem",
    "client.pem",
    "client-key.pem",
];

fn tls_snapshot(dir: &std::path::Path) -> Vec<(&'static str, Vec<u8>)> {
    TLS_MANAGED
        .iter()
        .filter_map(|name| std::fs::read(dir.join(name)).ok().map(|b| (*name, b)))
        .collect()
}

/// The reported defect, end to end, through the real binary.
///
/// Steps 1-4 of the filed reproduction: generate a bundle with a client cert,
/// confirm the client verifies, re-run WITHOUT any destructive option, and
/// look at what the directory holds. Measured on 8.0.0 that second run exited
/// 0, replaced `ca.pem` and the CA private key, left `client.pem` byte
/// identical, and produced a directory whose client certificate failed with
/// `unable to get local issuer certificate`.
///
/// The refusal's remedy is then EXECUTED verbatim, because a message naming a
/// flag is worth nothing until the string it prints has been run.
#[test]
fn init_tls_refuses_to_destroy_an_existing_ca_and_its_force_remedy_runs() {
    let dir = tempfile::tempdir().unwrap();
    let tls = dir.path().join("tls");

    nestweaver_cmd()
        .args([
            "server",
            "init-tls",
            "--output-dir",
            tls.to_str().unwrap(),
            "--san",
            "localhost",
            "--client",
        ])
        .assert()
        .success();
    let before = tls_snapshot(&tls);
    assert_eq!(before.len(), 6, "the first run writes the whole bundle");

    // Step 3: the exact invocation that destroyed the CA on 8.0.0.
    let refused = nestweaver_cmd()
        .args([
            "server",
            "init-tls",
            "--output-dir",
            tls.to_str().unwrap(),
            "--san",
            "localhost",
        ])
        .output()
        .unwrap();
    assert_eq!(
        refused.status.code(),
        Some(64),
        "a destructive replacement without --force must exit EXIT_USAGE"
    );
    assert!(
        refused.stdout.is_empty(),
        "a refusal must not print a success report: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(
        stderr.contains("refusing to replace the TLS bundle already in"),
        "{stderr}"
    );
    assert!(
        stderr.contains("would also retire client.pem and client-key.pem"),
        "the refusal must disclose what a replacement costs: {stderr}"
    );
    assert_eq!(
        tls_snapshot(&tls),
        before,
        "a refused run must not touch one byte, least of all the CA private key"
    );

    // The remedy, taken from the message and run as printed.
    let remedy = stderr
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("nestweaver server init-tls"))
        .unwrap_or_else(|| panic!("refusal must print a runnable command: {stderr}"))
        .to_string();
    assert_eq!(
        remedy,
        format!(
            "nestweaver server init-tls --output-dir {} --san localhost --force",
            tls.display()
        ),
        "the remedy must be the caller's own invocation plus --force"
    );
    let argv: Vec<&str> = remedy.split_whitespace().skip(1).collect();
    let forced = nestweaver_cmd().args(&argv).output().unwrap();
    assert_eq!(
        forced.status.code(),
        Some(0),
        "the printed remedy must work: {}",
        String::from_utf8_lossy(&forced.stderr)
    );

    // It did what the refusal said it would do, and nothing else.
    let after = tls_snapshot(&tls);
    assert_eq!(
        after.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        ["ca.pem", "ca-key.pem", "server.pem", "server-key.pem"],
        "the client certificate the destroyed CA signed must not survive it"
    );
    for (name, bytes) in &after {
        let old = before.iter().find(|(n, _)| n == name).unwrap();
        assert_ne!(&old.1, bytes, "{name} should have been replaced");
    }
    // And the destroyed CA is recoverable rather than gone.
    let backup = tls.join(".nestweaver-tls.backup");
    assert_eq!(tls_snapshot(&backup), before);
}

/// A PARTIAL directory is an existing bundle. `ca.pem` alone was the only
/// thing checked on 8.0.0, so a directory missing exactly that file was
/// overwritten with no warning printed at all.
#[test]
fn init_tls_refuses_on_partial_directories_in_both_directions() {
    for missing in [
        vec!["ca.pem", "ca-key.pem"],
        vec!["client.pem", "client-key.pem"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path().join("tls");
        nestweaver_cmd()
            .args([
                "server",
                "init-tls",
                "--output-dir",
                tls.to_str().unwrap(),
                "--client",
            ])
            .assert()
            .success();
        for name in &missing {
            std::fs::remove_file(tls.join(name)).unwrap();
        }
        let before = tls_snapshot(&tls);

        let out = nestweaver_cmd()
            .args(["server", "init-tls", "--output-dir", tls.to_str().unwrap()])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(64),
            "a directory missing {missing:?} is still a bundle"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        for (name, _) in &before {
            assert!(
                stderr.contains(name),
                "the refusal must enumerate {name}: {stderr}"
            );
        }
        assert_eq!(tls_snapshot(&tls), before);
    }
}

/// An install interrupted part way through is rolled back by the next run,
/// which says so — the directory never keeps a mix of two bundles.
///
/// The interrupt is DETERMINISTIC: the on-disk state a kill leaves is planted
/// directly, using the same dot-file names `install` writes. Killing a real
/// process instead would put the fatal signal inside a window microseconds
/// wide and pass on unfixed code most of the time, which is not a test.
/// `nestweaver_engine::tls`'s own suite walks every reachable interrupt point;
/// this asserts the CLI surfaces the recovery rather than performing it in
/// silence.
#[test]
fn init_tls_rolls_back_a_half_replaced_directory_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let tls = dir.path().join("tls");
    nestweaver_cmd()
        .args([
            "server",
            "init-tls",
            "--output-dir",
            tls.to_str().unwrap(),
            "--client",
        ])
        .assert()
        .success();
    let original = tls_snapshot(&tls);
    assert_eq!(original.len(), 6);

    // The state a crash between "old bundle moved aside" and "new bundle in
    // place" leaves: three files retired, two of them already replaced by a
    // different run's bundle.
    let retired = tls.join(".nestweaver-tls.retired");
    std::fs::create_dir(&retired).unwrap();
    let interloper = dir.path().join("other");
    nestweaver_cmd()
        .args([
            "server",
            "init-tls",
            "--output-dir",
            interloper.to_str().unwrap(),
            "--client",
        ])
        .assert()
        .success();
    for name in TLS_MANAGED {
        std::fs::rename(tls.join(name), retired.join(name)).unwrap();
    }
    for name in ["ca.pem", "ca-key.pem"] {
        std::fs::copy(interloper.join(name), tls.join(name)).unwrap();
    }
    std::fs::write(
        tls.join(".nestweaver-tls.journal"),
        serde_json::json!({ "retiring": TLS_MANAGED, "installing": TLS_MANAGED }).to_string(),
    )
    .unwrap();
    // As it stands the directory is a split bundle: a CA from one run beside
    // nothing it signed.
    assert_eq!(tls_snapshot(&tls).len(), 2);

    let out = nestweaver_cmd()
        .args(["server", "init-tls", "--output-dir", tls.to_str().unwrap()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("rolled back an interrupted `server init-tls` install"),
        "a silent recovery is indistinguishable from nothing having gone wrong: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(64),
        "the restored bundle is an existing bundle, so the run still refuses"
    );
    assert_eq!(
        tls_snapshot(&tls),
        original,
        "the bundle that preceded the interrupted install must be restored whole"
    );
    for debris in [
        ".nestweaver-tls.journal",
        ".nestweaver-tls.retired",
        ".nestweaver-tls.staging",
    ] {
        assert!(!tls.join(debris).exists(), "{debris} left behind");
    }
}

/// Two simultaneous invocations must not interleave into a split bundle. On
/// 8.0.0 both exited 0 and the directory was left with a `ca.pem` and a
/// `ca-key.pem` from different processes — reproducible, but only sometimes,
/// which is why the lock is asserted directly rather than raced for.
#[cfg(unix)]
#[test]
fn init_tls_stands_down_while_another_install_holds_the_directory() {
    use std::os::unix::io::AsRawFd;

    let dir = tempfile::tempdir().unwrap();
    let tls = dir.path().join("tls");
    nestweaver_cmd()
        .args([
            "server",
            "init-tls",
            "--output-dir",
            tls.to_str().unwrap(),
            "--client",
        ])
        .assert()
        .success();
    let before = tls_snapshot(&tls);

    // Stand in for an install in progress by holding the lock it holds.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tls.join(".nestweaver-tls.lock"))
        .expect("an installed bundle leaves the lock file behind");
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let blocked = nestweaver_cmd()
        .args([
            "server",
            "init-tls",
            "--output-dir",
            tls.to_str().unwrap(),
            "--client",
            "--force",
        ])
        .output()
        .unwrap();
    assert_eq!(
        blocked.status.code(),
        Some(1),
        "a contended directory is a transient runtime condition, not a usage error"
    );
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        stderr.contains("already installing into"),
        "the loser must say why it stood down: {stderr}"
    );
    assert_eq!(
        tls_snapshot(&tls),
        before,
        "a run that stood down must not have written anything"
    );

    // Releasing the lock lets the same command through.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    drop(lock);
    nestweaver_cmd()
        .args([
            "server",
            "init-tls",
            "--output-dir",
            tls.to_str().unwrap(),
            "--client",
            "--force",
        ])
        .assert()
        .success();

    // And under real contention the directory is still one coherent bundle.
    let children: Vec<_> = (0..6)
        .map(|_| {
            StdCommand::new(env!("CARGO_BIN_EXE_nestweaver"))
                .args([
                    "server",
                    "init-tls",
                    "--output-dir",
                    tls.to_str().unwrap(),
                    "--client",
                    "--force",
                    "--validity-days",
                    "3650",
                ])
                .env("NESTWEAVER_DIAGNOSTIC_WIDTH", "1000")
                .env("NESTWEAVER_NO_DAEMON", "1")
                .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .collect();
    assert!(outputs.iter().any(|o| o.status.success()));
    for out in outputs.iter().filter(|o| !o.status.success()) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("already installing into"), "{stderr}");
    }
    assert_eq!(
        tls_snapshot(&tls)
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>(),
        TLS_MANAGED
    );
    // `openssl verify` is what the filed reproduction used; run it when the
    // host has it so this asserts exactly what the bug report asserted.
    if StdCommand::new("openssl").arg("version").output().is_ok() {
        for leaf in ["server.pem", "client.pem"] {
            let verify = StdCommand::new("openssl")
                .args(["verify", "-CAfile"])
                .arg(tls.join("ca.pem"))
                .arg(tls.join(leaf))
                .output()
                .unwrap();
            assert!(
                verify.status.success(),
                "{leaf} does not verify under the installed ca.pem: {}",
                String::from_utf8_lossy(&verify.stderr)
            );
        }
    }
}

// ── `admin install-hook` must never destroy content it did not write ────────
//
// The defect, reproduced by hand before any of this was written: a temp
// directory holding `.claude/settings.local.json` with one `//` comment — JSONC,
// which Claude Code itself accepts in this very file — and a live API key.
// `nestweaver admin install-hook` exited 0, printed "Hook installed
// (idempotent) to .claude/settings.local.json", and left the file containing
// NOTHING but NestWeaver's hook. `MY_API_KEY` and the permission grants were
// gone.
//
// Mechanism: the handler read the file, folded ANY `serde_json` failure into
// `Value::Null`, merged its hook into that, and `fs::write`-truncated the
// result over the user's settings.
//
// These run the real binary with its CWD inside a temp directory, because the
// settings path is resolved RELATIVE TO THE CWD — that is exactly what makes
// the command dangerous, so it is what the tests must exercise.

/// The reproduction, verbatim.
#[test]
fn install_hook_does_not_eat_a_secret_out_of_a_commented_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let settings = dir.path().join(".claude/settings.local.json");
    // Unknown keys ride along here rather than only in the well-formed-file
    // test below, because THIS is the fixture on which preservation actually
    // failed. On well-formed JSON the old merge already preserved everything.
    let original = "{\n  // project-local overrides\n  \
                    \"env\": { \"MY_API_KEY\": \"sk-live-do-not-lose-me\" },\n  \
                    \"permissions\": { \"allow\": [\"Bash(git status:*)\"] },\n  \
                    \"aKeyFromSomeFutureRelease\": { \"nested\": [1, 2] }\n}\n";
    std::fs::write(&settings, original).unwrap();

    let output = nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        original,
        "the settings file must be byte-for-byte what it was: {stderr}"
    );
    assert!(
        !output.status.success(),
        "exiting 0 is how this went unnoticed — the user was told the hook was \
         installed while their key was being deleted: {stderr}"
    );
    // Honest about what it did and did not do, and about WHY comments are the
    // problem rather than pretending the file is corrupt.
    assert!(stderr.contains("contains JSON comments"), "{stderr}");
    assert!(stderr.contains("changed nothing"), "{stderr}");
    assert!(
        stderr.contains("nestweaver admin install-hook --dry-run"),
        "a refusal has to hand back something runnable: {stderr}"
    );
}

/// The remedy the refusal above prints, executed. A message naming a command
/// that cannot run on the file that triggered it is not a remedy.
///
/// The refusal is asserted here too, and deliberately: without it this test
/// passes on the unfixed binary, because both remedies happen to work on code
/// that never refuses. A remedy test whose precondition is not checked is a
/// test of nothing.
#[test]
fn the_refusals_remedy_runs_on_the_very_file_that_triggered_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let settings = dir.path().join(".claude/settings.local.json");
    let original = "{\n  // project-local overrides\n  \
                    \"env\": { \"MY_API_KEY\": \"sk-live-do-not-lose-me\" }\n}\n";
    std::fs::write(&settings, original).unwrap();

    // The precondition: it refuses, and the refusal names the remedy below.
    let refusal = nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&refusal.stderr).to_string();
    assert!(!refusal.status.success(), "{stderr}");
    assert!(
        stderr.contains("nestweaver admin install-hook --dry-run"),
        "{stderr}"
    );
    assert!(
        stderr.contains("remove the comments"),
        "the second remedy has to be named too: {stderr}"
    );

    // Remedy 1: `--dry-run` prints the entry to add by hand.
    let dry = nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook", "--dry-run"])
        .assert()
        .success();
    let printed = String::from_utf8_lossy(&dry.get_output().stdout).to_string();
    let delta: serde_json::Value = serde_json::from_str(&printed).unwrap();
    assert_eq!(delta["hooks"]["PreToolUse"][0]["matcher"], "Task");
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        original,
        "the remedy must not write either"
    );

    // Remedy 2: remove the comments and run it again.
    std::fs::write(
        &settings,
        "{\n  \"env\": { \"MY_API_KEY\": \"sk-live-do-not-lose-me\" }\n}\n",
    )
    .unwrap();
    nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .assert()
        .success();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(after["env"]["MY_API_KEY"], "sk-live-do-not-lose-me");
    assert_eq!(after["hooks"]["PreToolUse"][0]["matcher"], "Task");
}

/// Well-formed JSON that NestWeaver does not own survives — in both shapes it
/// comes in.
///
/// The first half (an object with keys from the future) is a FLOOR, not a
/// reproduction: on a well-formed object the old merge already preserved
/// everything, so that half passes on the unfixed binary. It is kept because
/// the merge is what a future change would break, and the reproduction test
/// above now carries an unknown key of its own so the property is covered by
/// something that can fail.
///
/// The second half is the well-formed case the old code DID destroy. Valid JSON
/// that is not an object — `[]`, `"text"`, `null` — parsed fine, and then
/// `compute_claude_hook_patch` replaced anything non-object with `{}` and wrote
/// the hook over it. Nothing here is unreadable; it is simply not this
/// command's to replace.
#[test]
fn install_hook_preserves_every_key_it_does_not_own() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let settings = dir.path().join(".claude/settings.local.json");
    let original = serde_json::json!({
        "env": { "MY_API_KEY": "sk-live-do-not-lose-me" },
        "permissions": { "allow": ["Bash(git status:*)"], "deny": ["Bash(rm:*)"] },
        "model": "opus",
        "hooks": { "PreToolUse": [ { "matcher": "Edit", "hooks": [] } ] },
        "aKeyFromSomeFutureRelease": { "nested": [1, 2, { "deep": true }] }
    });
    std::fs::write(&settings, serde_json::to_string_pretty(&original).unwrap()).unwrap();

    nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .assert()
        .success();

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    for key in ["env", "permissions", "model", "aKeyFromSomeFutureRelease"] {
        assert_eq!(after[key], original[key], "`{key}` must survive in value");
    }
    let matchers: Vec<&str> = after["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["matcher"].as_str())
        .collect();
    assert_eq!(
        matchers,
        vec!["Edit", "Task"],
        "the user's own matcher is not NestWeaver's to move or drop"
    );

    // Well-formed, not an object, and not NestWeaver's to replace.
    for content in ["[1, 2]", "\"just a string\"", "null"] {
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(other.path().join(".claude")).unwrap();
        let path = other.path().join(".claude/settings.local.json");
        std::fs::write(&path, content).unwrap();

        let output = nestweaver_cmd()
            .current_dir(other.path())
            .args(["admin", "install-hook"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        assert!(!output.status.success(), "on {content}: {stderr}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            content,
            "valid JSON that is not an object was replaced wholesale: {stderr}"
        );
        assert!(
            stderr.contains("not an object"),
            "on {content}, say what it found: {stderr}"
        );
    }
}

#[test]
fn install_hook_run_twice_neither_duplicates_the_hook_nor_touches_the_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let settings = dir.path().join(".claude/settings.local.json");
    std::fs::write(&settings, "{\"env\": {\"MY_API_KEY\": \"sk-live\"}}").unwrap();

    nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .assert()
        .success()
        .stderr(contains("Hook installed"));
    let after_first = std::fs::read_to_string(&settings).unwrap();

    nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .assert()
        .success()
        .stderr(contains("already present").and(contains("nothing written")));

    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        after_first,
        "a second run has nothing to add, so it must not rewrite the file at all"
    );
    let after: serde_json::Value = serde_json::from_str(&after_first).unwrap();
    assert_eq!(after["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(after["env"]["MY_API_KEY"], "sk-live");
}

/// An interrupted write must leave the original intact.
///
/// A hard link is a second name for the SAME inode. `fs::write` opens that
/// inode and TRUNCATES it before writing a byte, so the user's settings are
/// destroyed first and a crash in between leaves an empty file. A temp file
/// plus rename never touches the old inode — which is why the witness still
/// holds the original bytes at every instant of the write, and why a crash is
/// survivable.
#[test]
fn install_hook_replaces_the_settings_file_instead_of_truncating_it_in_place() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let settings = dir.path().join(".claude/settings.local.json");
    let witness = dir.path().join("witness.json");
    let original = "{\"env\": {\"MY_API_KEY\": \"sk-live-do-not-lose-me\"}}";
    std::fs::write(&settings, original).unwrap();
    std::fs::hard_link(&settings, &witness).unwrap();

    nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(&witness).unwrap(),
        original,
        "the original inode was truncated in place — a crash mid-write would \
         have left the user with nothing"
    );
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(after["env"]["MY_API_KEY"], "sk-live-do-not-lose-me");
    assert_eq!(after["hooks"]["PreToolUse"][0]["matcher"], "Task");

    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(".claude"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        leftovers.len(),
        1,
        "a failed or completed atomic write leaves no temp file: {leftovers:?}"
    );
}

/// THE DOTFILES CASE, end to end through the real binary.
///
/// `.claude/settings.local.json` is a symbolic link into a dotfiles repository
/// outside the project. `abbde5d8` refused it; that refusal was correct for
/// `server init-tls`, which was writing a CA private key through a link and
/// landing it outside `--output-dir`, and wrong for a user config the user
/// deliberately linked. The hook goes into the file the link names, every other
/// setting in it survives, and the link is still a link afterwards — a rename
/// over the LINK would have replaced it with a regular file, which breaks the
/// dotfiles checkout just as silently as the truncation this command started
/// with.
#[cfg(unix)]
#[test]
fn install_hook_follows_a_symlinked_settings_file_into_a_dotfiles_repo() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let dotfiles = dir.path().join("dotfiles");
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    std::fs::create_dir_all(&dotfiles).unwrap();
    let target = dotfiles.join("settings.local.json");
    std::fs::write(
        &target,
        "{ \"env\": { \"SHARED_KEY\": \"sk-live-shared\" } }\n",
    )
    .unwrap();
    let link = project.join(".claude/settings.local.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let output = nestweaver_cmd()
        .current_dir(&project)
        .args(["admin", "install-hook"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(output.status.success(), "{stderr}");
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(
        after["env"]["SHARED_KEY"], "sk-live-shared",
        "the user's key must survive the merge: {after}"
    );
    assert_eq!(
        after["hooks"]["PreToolUse"][0]["matcher"], "Task",
        "the hook must land in the file the link names: {after}"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link itself is content NestWeaver did not write: {stderr}"
    );
    assert_eq!(std::fs::read_link(&link).unwrap(), target);
    // No temp file may be left beside the link: the replacement is staged in
    // the RESOLVED target's directory, so the rename cannot cross a filesystem.
    let beside: Vec<_> = std::fs::read_dir(project.join(".claude"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(beside.len(), 1, "{beside:?}");

    // Second run is idempotent and still does not disturb the link.
    nestweaver_cmd()
        .current_dir(&project)
        .args(["admin", "install-hook"])
        .assert()
        .success();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

/// A DANGLING link is still refused: there is nothing to merge into, and
/// creating the target would write settings to a path the command was never
/// pointed at. The remedy it prints is executed here.
#[cfg(unix)]
#[test]
fn install_hook_refuses_a_dangling_symlinked_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let missing = dir.path().join("elsewhere/gone.json");
    let link = dir.path().join(".claude/settings.local.json");
    std::os::unix::fs::symlink(&missing, &link).unwrap();

    let output = nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("gone.json"), "name it: {stderr}");
    // The CLI classifier rewrites any message containing "path" and "does not
    // exist" into `repo_not_found` — which retitled the first draft of this
    // refusal "Repository path does not exist" and dropped the sentence naming
    // the link. Pin the two clauses that survive only if it does not fire.
    assert!(stderr.contains("symbolic link"), "misclassified: {stderr}");
    assert!(
        stderr.contains("changed nothing"),
        "misclassified: {stderr}"
    );
    assert!(
        !missing.exists(),
        "nothing may be created out there: {stderr}"
    );

    // The remedy it prints, executed verbatim against the same tree.
    std::fs::create_dir_all(missing.parent().unwrap()).unwrap();
    std::fs::write(&missing, "{}\n").unwrap();
    nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .assert()
        .success();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&missing).unwrap()).unwrap();
    assert_eq!(after["hooks"]["PreToolUse"][0]["matcher"], "Task");
}

/// Unparseable input is a refusal, not an empty object.
#[test]
fn install_hook_refuses_unparseable_settings_and_names_the_position() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let settings = dir.path().join(".claude/settings.local.json");
    let original = "{\n  \"env\": { \"MY_API_KEY\": \"sk-live\" },\n}\n";
    std::fs::write(&settings, original).unwrap();

    let output = nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(!output.status.success(), "{stderr}");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
    assert!(stderr.contains("not valid JSON"), "{stderr}");
    assert!(stderr.contains("settings.local.json"), "{stderr}");
    assert!(
        stderr.contains("line 3") && stderr.contains("column"),
        "the parse position has to be in the message: {stderr}"
    );

    // The remedy: fix the syntax at that position and run it again.
    std::fs::write(
        &settings,
        "{\n  \"env\": { \"MY_API_KEY\": \"sk-live\" }\n}\n",
    )
    .unwrap();
    nestweaver_cmd()
        .current_dir(dir.path())
        .args(["admin", "install-hook"])
        .assert()
        .success();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(after["env"]["MY_API_KEY"], "sk-live");
}
