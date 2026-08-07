//! Cooperative cancellation of an in-flight index (server-mode `index_repo`).
//!
//! The daemon runs indexing in an uncancelable `spawn_blocking` task. These
//! tests cover the cooperative cancellation flag that lets a timed-out or
//! disconnected client abort the index at the pre-write boundary without
//! leaving a partial write.

use nestweaver_engine::content_reader::{ContentReader, FilesystemReader};
use nestweaver_store::GraphStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// A ContentReader that counts `read_file` calls, wrapping a real
/// FilesystemReader. Used to prove that a cancelled index stops BEFORE reading
/// or parsing any file (not merely before the write).
struct CountingReader {
    inner: FilesystemReader,
    reads: AtomicUsize,
}

impl CountingReader {
    fn new(inner: FilesystemReader) -> Self {
        Self {
            inner,
            reads: AtomicUsize::new(0),
        }
    }
    fn read_count(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

impl ContentReader for CountingReader {
    fn read_file(&self, rel_path: &Path) -> anyhow::Result<String> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_file(rel_path)
    }
    fn list_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        self.inner.list_files()
    }
    fn file_meta(&self, rel_path: &Path) -> anyhow::Result<Option<(u64, u64)>> {
        self.inner.file_meta(rel_path)
    }
    fn root(&self) -> &Path {
        self.inner.root()
    }
    fn version_id(&self) -> &str {
        self.inner.version_id()
    }
}

#[test]
fn cancelled_index_stops_before_reading_or_parsing_any_file() {
    let store = GraphStore::in_memory().unwrap();
    let spy = CountingReader::new(FilesystemReader::new(&testdata_js()));
    let cancel = Arc::new(AtomicBool::new(true)); // pre-cancelled

    let result = nestweaver_engine::index_with_reader_and_write_gate(
        &spy,
        &store,
        "default",
        "file:///test/js",
        "abc",
        None,
        Some(&cancel),
        || Ok::<(), anyhow::Error>(()),
    );

    assert!(result.is_err(), "a pre-cancelled index must return Err");
    assert_eq!(
        spy.read_count(),
        0,
        "a cancelled index must not read/parse ANY file (not just skip the write)"
    );
    assert!(
        store.list_repos(None).unwrap().is_empty(),
        "a cancelled index must not write the repo into the store"
    );
}

/// Cancellation that trips AT the write gate — after the per-file and
/// post-parse barrier polls have already passed — must still abort the run
/// before any graph mutation. The flag flips inside the caller-supplied
/// `acquire_write_guard` closure, so the abort can only come from the poll at
/// the pre-write boundary: `read_count() > 0` proves files were read and
/// parsed (ruling out the earlier barrier), and the empty store proves no
/// write slipped through.
#[test]
fn cancelled_at_the_write_gate_bails_after_parsing_without_writing() {
    let store = GraphStore::in_memory().unwrap();
    let spy = CountingReader::new(FilesystemReader::new(&testdata_js()));
    let cancel = Arc::new(AtomicBool::new(false)); // trips only at the write gate
    let cancel_in_guard = Arc::clone(&cancel);

    let result = nestweaver_engine::index_with_reader_and_write_gate(
        &spy,
        &store,
        "default",
        "file:///test/js",
        "abc",
        None,
        Some(&cancel),
        move || {
            cancel_in_guard.store(true, Ordering::SeqCst);
            Ok::<(), anyhow::Error>(())
        },
    );

    let err = match result {
        Ok(_) => panic!("an index cancelled at the write gate must return an error"),
        Err(e) => e,
    };
    assert!(
        err.to_string().to_lowercase().contains("cancel"),
        "error should mention cancellation, got: {err}"
    );
    assert!(
        spy.read_count() > 0,
        "the abort happens at the write gate, AFTER parsing — not at the earlier barrier"
    );
    assert!(
        store.list_repos(None).unwrap().is_empty(),
        "an index cancelled at the write gate must not write the repo into the store"
    );
}

#[test]
fn uncancelled_reader_index_still_reads_and_writes() {
    let store = GraphStore::in_memory().unwrap();
    let spy = CountingReader::new(FilesystemReader::new(&testdata_js()));
    let cancel = Arc::new(AtomicBool::new(false)); // never cancelled

    let result = nestweaver_engine::index_with_reader_and_write_gate(
        &spy,
        &store,
        "default",
        "file:///test/js",
        "abc",
        None,
        Some(&cancel),
        || Ok::<(), anyhow::Error>(()),
    );

    assert!(result.is_ok(), "an uncancelled index must succeed");
    assert!(
        spy.read_count() > 0,
        "an uncancelled index must actually read files"
    );
    assert!(!store.list_repos(None).unwrap().is_empty());
}
