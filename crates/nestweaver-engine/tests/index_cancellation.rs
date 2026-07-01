//! Cooperative cancellation of an in-flight index (server-mode `index_repo`).
//!
//! The daemon runs indexing in an uncancelable `spawn_blocking` task. These
//! tests cover the cooperative cancellation flag that lets a timed-out or
//! disconnected client abort the index at the pre-write boundary without
//! leaving a partial write.

use nestweaver_store::GraphStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn testdata_js() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("testdata/js")
}

#[test]
fn cancelled_index_bails_without_writing() {
    let store = GraphStore::in_memory().unwrap();
    let cancel = Arc::new(AtomicBool::new(true)); // pre-cancelled

    let result = nestweaver_engine::index_directory_with_store_cancellable(
        &store,
        &testdata_js(),
        &std::env::temp_dir().join("nw-cancel-test-1"),
        "default",
        "file:///test/js",
        "abc",
        true,
        None,
        &cancel,
    );

    let err = match result {
        Ok(_) => panic!("a pre-cancelled index must return an error"),
        Err(e) => e,
    };
    assert!(
        err.to_string().to_lowercase().contains("cancel"),
        "error should mention cancellation, got: {err}"
    );
    assert!(
        store.list_repos(None).unwrap().is_empty(),
        "a cancelled index must not write the repo into the store"
    );
}

#[test]
fn uncancelled_index_via_cancellable_path_succeeds() {
    let store = GraphStore::in_memory().unwrap();
    let cancel = Arc::new(AtomicBool::new(false)); // never cancelled

    let result = nestweaver_engine::index_directory_with_store_cancellable(
        &store,
        &testdata_js(),
        &std::env::temp_dir().join("nw-cancel-test-2"),
        "default",
        "file:///test/js",
        "abc",
        true,
        None,
        &cancel,
    );

    let files = match result {
        Ok(r) => r.files_count,
        Err(e) => panic!("an uncancelled index must succeed: {e}"),
    };
    assert!(files > 0, "should index at least one file");
    assert!(!store.list_repos(None).unwrap().is_empty());
}
