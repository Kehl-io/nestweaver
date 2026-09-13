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
