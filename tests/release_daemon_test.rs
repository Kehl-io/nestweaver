//! Standard-artifact acceptance tests. Fixture setup itself uses the daemon.
use std::process::Command;

fn run_fixture(extra: &[&str]) {
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/isolated_daemon.py"
        ))
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_nestweaver"))
        .args(extra)
        .output()
        .expect("run daemon fixture with Python 3");
    assert!(
        result.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn release_daemon_fixture_bootstrap() {
    run_fixture(&[]);
}

// Actual forbidden requests are CI-only. Pure policy coverage runs locally.
#[cfg(not(feature = "ci-direct-tests"))]
#[test]
#[ignore = "CI-only forbidden-request probe"]
fn release_standard_artifact_cannot_bypass() {
    let runner = matches!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"))
        && std::env::var("RUNNER_TEMP").is_ok_and(|v| !v.is_empty())
        && std::env::var("RUNNER_OS").is_ok_and(|v| !v.is_empty())
        && std::env::var("GITHUB_RUN_ID").is_ok_and(|v| !v.is_empty());
    if !runner {
        panic!("CI-only rejection probe; local coverage is no_daemon_gate_tests");
    }
    run_fixture(&["--ci-policy-test"]);
}

#[test]
fn closed_stderr_preserves_semantic_cli_exits() {
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/closed_stderr.py"
        ))
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_nestweaver"))
        .output()
        .expect("run closed-stderr daemon fixture");
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn backing_file_admission_release_fixture() {
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/backing_file.py"
        ))
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_nestweaver"))
        .output()
        .expect("run backing-file daemon fixture");
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn typed_backup_refusal_across_daemon_and_direct_routes() {
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/backup_refusal.py"
        ))
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_nestweaver"))
        .output()
        .expect("run typed backup-refusal fixture");
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn global_incomplete_debt_refuses_cli_and_mcp_without_false_empty_success() {
    use nestweaver_schema::{File, Repo, Symbol, SymbolKind, Visibility};
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let seed = directory.path().join("seed.lbug");
    let store = nestweaver_store::GraphStore::open_or_create(&seed).unwrap();
    store.ensure_data_instance_id("default").unwrap();
    let repo_a = nestweaver_schema::repo_uid("default", "https://example.test/complete-a");
    let repo_b = nestweaver_schema::repo_uid("default", "https://example.test/incomplete-b");
    for (uid, name) in [(&repo_a, "complete-a"), (&repo_b, "incomplete-b")] {
        store
            .insert_repo(&Repo {
                uid: uid.clone(),
                url: format!("https://example.test/{name}"),
                indexed_sha: "seed".into(),
                staleness_commits_behind: 0,
                instance_id: "default".into(),
                name: Some(name.into()),
                root_path: None,
            })
            .unwrap();
        nestweaver_engine::resolver_generation::record(&seed, uid).unwrap();
    }
    let file_uid = nestweaver_schema::file_uid(&repo_a, "src/complete.js");
    store
        .insert_file(&File {
            uid: file_uid.clone(),
            path: "src/complete.js".into(),
            repo_uid: repo_a.clone(),
            content_hash: "seed".into(),
        })
        .unwrap();
    store.insert_repo_file_edge(&repo_a, &file_uid).unwrap();
    let symbol = nestweaver_schema::symbol_uid(&repo_a, "src/complete.js", "completeSeed", 1);
    store
        .insert_symbol(&Symbol {
            uid: symbol.clone(),
            name: "completeSeed".into(),
            kind: SymbolKind::Function,
            repo_uid: repo_a.clone(),
            file_path: "src/complete.js".into(),
            start_line: 1,
            end_line: 1,
            signature: "export function completeSeed() {}".into(),
            summary: None,
            content_hash: "seed".into(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        })
        .unwrap();
    store.insert_file_symbol_edge(&file_uid, &symbol).unwrap();
    store
        .set_repo_publication_incomplete(&repo_b, "injected interrupted publication")
        .unwrap();
    assert!(
        !store
            .repo_index_incomplete(&store.lookup_repo(&repo_a).unwrap().unwrap())
            .unwrap()
    );
    assert!(
        store
            .repo_index_incomplete(&store.lookup_repo(&repo_b).unwrap().unwrap())
            .unwrap()
    );
    drop(store);
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/incomplete_scope.py"
        ))
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_nestweaver"))
        .arg("--seed-db")
        .arg(&seed)
        .arg("--repo-a")
        .arg(&repo_a)
        .arg("--repo-b")
        .arg(&repo_b)
        .arg("--symbol")
        .arg(&symbol)
        .output()
        .expect("run incomplete-scope standard-artifact fixture");
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
