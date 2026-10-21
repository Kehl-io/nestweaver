//! Real rebuild-worker crash/retry coverage, without invoking an embedder.
#![cfg(debug_assertions)]

use nestweaver_engine::{publication, publication_operation};

#[test]
fn planned_worker_resumes_two_crashes_and_commits_exact_identity_before_graph() {
    let scratch = tempfile::tempdir().unwrap();
    let db = scratch.path().join("brain.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db).unwrap());
    let config = scratch.path().join("instance.toml");
    std::fs::write(
        &config,
        include_str!("../examples/minimal-instance.toml").replace(
            "~/.local/share/nestweaver/minimal",
            &scratch.path().join("state").display().to_string(),
        ),
    )
    .unwrap();
    let root = publication::default_publication_root(&db);
    let run = |operation: Option<&str>, crash_after_identity: bool| {
        let mut command = assert_cmd::Command::cargo_bin("nestweaver").unwrap();
        command
            .env("NESTWEAVER_NO_DAEMON", "1")
            .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
            .env("NESTWEAVER_DIAGNOSTIC_WIDTH", "1000")
            .args(["publication", "rebuild", "--no-activate", "--config"])
            .arg(&config)
            .arg("--db")
            .arg(&db);
        if let Some(operation) = operation {
            command.arg("--operation").arg(operation);
        }
        command.env(
            if crash_after_identity {
                "NESTWEAVER_TEST_CRASH_AFTER_STAGED_IDENTITY"
            } else {
                "NESTWEAVER_TEST_CRASH_AFTER_STAGED_AUTHORITY"
            },
            "1",
        );
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(if crash_after_identity { 87 } else { 86 }),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(None, false);
    let list = publication_operation::list_operations(&root).unwrap();
    assert!(list.invalid_operations.is_empty());
    assert_eq!(list.operations.len(), 1);
    let state = &list.operations[0];
    let operation = &state.plan.operation_uuid;
    let target = publication::slot_path(&root, &state.plan.target_publication_uuid)
        .unwrap()
        .join(publication::PUBLICATION_GRAPH_FILE);
    assert_eq!(
        state.phase,
        publication_operation::PublicationPhase::Planned
    );
    assert_eq!(std::fs::metadata(&target).unwrap().len(), 0);
    run(Some(operation), false);
    assert_eq!(
        publication_operation::load_operation(&root, operation)
            .unwrap()
            .phase,
        publication_operation::PublicationPhase::Planned
    );
    assert_eq!(std::fs::metadata(&target).unwrap().len(), 0);
    run(Some(operation), true);
    assert_eq!(
        publication_operation::load_operation(&root, operation)
            .unwrap()
            .phase,
        publication_operation::PublicationPhase::Graph
    );
    let journal = publication::operation_path(&root, operation).unwrap();
    assert!(!journal.join("creation.json").exists());
    assert!(!std::fs::read_dir(&journal).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("creation-seed-")
    }));
    let store = nestweaver_store::GraphStore::open_read_only_without_migration(&target).unwrap();
    let identity = store.publication_identity().unwrap().unwrap();
    assert_eq!(identity.brain_uuid, state.plan.brain_uuid);
    assert_eq!(
        identity.publication_uuid,
        state.plan.target_publication_uuid
    );
    assert!(publication::read_current(&root).unwrap().is_none());
}

/// nw-742: a cancel-requested operation is not resumed. Rebuild acknowledges
/// the cancellation, names it, and exits 1.
#[test]
fn a_cancelled_operation_rebuild_fails_closed_and_names_the_cancellation() {
    let scratch = tempfile::tempdir().unwrap();
    let db = scratch.path().join("brain.lbug");
    drop(nestweaver_store::GraphStore::open_or_create(&db).unwrap());
    let config = scratch.path().join("instance.toml");
    std::fs::write(
        &config,
        include_str!("../examples/minimal-instance.toml").replace(
            "~/.local/share/nestweaver/minimal",
            &scratch.path().join("state").display().to_string(),
        ),
    )
    .unwrap();
    let root = publication::default_publication_root(&db);

    let mut started = assert_cmd::Command::cargo_bin("nestweaver").unwrap();
    let started = started
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .env("NESTWEAVER_DIAGNOSTIC_WIDTH", "1000")
        .env("NESTWEAVER_TEST_STOP_BEFORE_PHASE", "planned")
        .args(["publication", "rebuild", "--no-activate", "--config"])
        .arg(&config)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert_eq!(
        started.status.code(),
        Some(88),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&started.stdout),
        String::from_utf8_lossy(&started.stderr)
    );
    let operation = String::from_utf8_lossy(&started.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("Publication operation: "))
        .unwrap_or_else(|| {
            panic!(
                "no operation uuid in: {}",
                String::from_utf8_lossy(&started.stderr)
            )
        })
        .trim()
        .to_string();
    let created = publication_operation::load_operation(&root, &operation).unwrap();
    assert!(!created.cancel_requested);

    let mut cancel = assert_cmd::Command::cargo_bin("nestweaver").unwrap();
    cancel
        .env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .args([
            "publication",
            "cancel",
            &operation,
            "--revision",
            &created.revision.to_string(),
            "--db",
        ])
        .arg(&db)
        .assert()
        .success();

    let rebuild = |json: bool| {
        let mut command = assert_cmd::Command::cargo_bin("nestweaver").unwrap();
        command
            .env("NESTWEAVER_NO_DAEMON", "1")
            .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
            .env("NESTWEAVER_DIAGNOSTIC_WIDTH", "1000")
            .args(["publication", "rebuild", "--no-activate", "--config"])
            .arg(&config)
            .arg("--db")
            .arg(&db)
            .arg("--operation")
            .arg(&operation);
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    };

    let refused = rebuild(false);
    let stdout = String::from_utf8_lossy(&refused.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert_ne!(
        refused.status.code(),
        Some(0),
        "a cancelled rebuild must not claim success: {stdout}{stderr}"
    );
    assert_eq!(refused.status.code(), Some(1), "{stdout}{stderr}");
    assert!(
        stdout.contains(&format!(
            "Publication {operation} was cancelled at revision"
        )),
        "failure text missing: {stdout}{stderr}"
    );
    assert!(
        !stdout.contains("is Activated"),
        "cancelled rebuild claimed activation: {stdout}"
    );
    assert!(
        !stderr.contains("Publication operation:"),
        "cancelled rebuild started a new operation: {stderr}"
    );
    let acknowledged = publication_operation::load_operation(&root, &operation).unwrap();
    assert_eq!(
        acknowledged.phase,
        publication_operation::PublicationPhase::Cancelled
    );
    assert!(acknowledged.cancel_requested);
    assert!(publication::read_current(&root).unwrap().is_none());

    let json = rebuild(true);
    let json_stdout = String::from_utf8_lossy(&json.stdout).into_owned();
    let json_stderr = String::from_utf8_lossy(&json.stderr).into_owned();
    assert_eq!(json.status.code(), Some(1), "{json_stdout}{json_stderr}");
    let record: serde_json::Value = serde_json::from_str(&json_stdout).unwrap_or_else(|error| {
        panic!("cancelled json record missing ({error}): {json_stdout}{json_stderr}")
    });
    assert_eq!(record["phase"], "cancelled");
    assert_eq!(record["plan"]["operation_uuid"], operation);
    assert_eq!(record["cancel_requested"], true);
    assert!(publication::read_current(&root).unwrap().is_none());
}
