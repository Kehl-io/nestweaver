//! nw-048 / nw-045: `remove-repo` must drop the removed repo's change-detection
//! sidecar slices (`.filemeta` + `.resolution_deps`) so a later re-index of the
//! SAME path re-indexes its files instead of silently skipping every one as
//! `Unchanged`. These tests exercise the engine helper the daemon `remove_repo`
//! RPC calls (`remove_repo_sidecar_slices`) both end-to-end (index → simulate
//! remove → re-index → symbols restored) and in isolation (uid-scoped + fail-safe).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use nestweaver_engine::resolution_cache::ResolutionDeps;
use nestweaver_engine::{
    CachedFileMeta, FileMetaCache, FileMetaSidecar, load_filemeta_sidecar,
    remove_repo_sidecar_slices, save_filemeta_sidecar, sidecar_path,
};
use nestweaver_schema::uid::repo_uid;
use nestweaver_store::GraphStore;

const INST: &str = "default";

fn write_file(repo: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(repo).unwrap();
    std::fs::write(repo.join(name), body).unwrap();
}

/// Mirror exactly what the daemon `remove_repo` RPC does: delete the repo's
/// graph nodes/symbols from the store, then drop its sidecar slices.
fn remove_repo(store: &GraphStore, db: &Path, uid: &str) {
    store.bulk_delete_repo_files_and_symbols(uid).unwrap();
    store.clear_repo_derived_nodes(uid).unwrap();
    store.delete_repo_node(uid).unwrap();
    remove_repo_sidecar_slices(db, uid).unwrap();
}

/// End-to-end nw-048 guard: after remove-repo drops the sidecar slice, a
/// re-index of the same path re-indexes the file and restores its symbol.
/// Also proves a co-located repo B is untouched by removing A.
#[test]
fn remove_repo_then_reindex_restores_symbols_and_spares_other_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo_a = dir.path().join("a");
    let repo_b = dir.path().join("b");
    write_file(&repo_a, "a.js", "function alphafn(){return 1;}");
    write_file(&repo_b, "b.js", "function betafn(){return 2;}");
    let db = dir.path().join("t.lbug");
    let url_a = "file:///repro/a";
    let url_b = "file:///repro/b";
    let uid_a = repo_uid(INST, url_a);
    let uid_b = repo_uid(INST, url_b);

    let store = GraphStore::open(&db).unwrap();

    // Index both repos into the shared DB (writes both sidecar slices).
    nestweaver_engine::index_directory_with_store(
        &store, &repo_a, &db, INST, url_a, "sha", false, None,
    )
    .unwrap();
    nestweaver_engine::index_directory_with_store(
        &store, &repo_b, &db, INST, url_b, "sha", false, None,
    )
    .unwrap();

    let counts = store.count_symbols_by_repo().unwrap();
    assert_eq!(
        counts.get(&uid_a).copied().unwrap_or(0),
        1,
        "alphafn indexed"
    );
    assert_eq!(
        counts.get(&uid_b).copied().unwrap_or(0),
        1,
        "betafn indexed"
    );
    // Both slices present in the filemeta sidecar.
    let fm = load_filemeta_sidecar(&sidecar_path(&db, ".filemeta.json"));
    assert!(fm.repos.contains_key(&uid_a) && fm.repos.contains_key(&uid_b));

    // Remove repo A (graph + sidecar slices).
    remove_repo(&store, &db, &uid_a);

    // A gone from store AND from the filemeta sidecar; B fully intact.
    let after_rm = store.count_symbols_by_repo().unwrap();
    assert_eq!(
        after_rm.get(&uid_a).copied().unwrap_or(0),
        0,
        "A symbols deleted"
    );
    assert_eq!(
        after_rm.get(&uid_b).copied().unwrap_or(0),
        1,
        "B survives removal of A"
    );
    let fm = load_filemeta_sidecar(&sidecar_path(&db, ".filemeta.json"));
    assert!(!fm.repos.contains_key(&uid_a), "A's filemeta slice dropped");
    assert!(fm.repos.contains_key(&uid_b), "B's filemeta slice intact");

    // Re-index the SAME path for A. Pre-fix (stale slice) this classified a.js
    // `Unchanged` and restored 0 symbols; with the slice dropped it re-indexes.
    let reindex = nestweaver_engine::index_directory_with_store(
        &store, &repo_a, &db, INST, url_a, "sha2", false, None,
    )
    .unwrap();
    assert_eq!(
        reindex.files_count, 1,
        "re-index must process a.js (nw-048)"
    );

    let restored = store.count_symbols_by_repo().unwrap();
    assert_eq!(
        restored.get(&uid_a).copied().unwrap_or(0),
        1,
        "alphafn restored after re-index"
    );
    assert_eq!(
        restored.get(&uid_b).copied().unwrap_or(0),
        1,
        "betafn still present"
    );
}

/// The sidecar slice-drop is strictly uid-scoped: only the named repo's slice is
/// removed from BOTH sidecars; every other repo's slice survives untouched.
#[test]
fn slice_drop_is_uid_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.lbug");
    let a = "repo:default:aaaa";
    let b = "repo:default:bbbb";

    // filemeta sidecar with A and B slices.
    let mut fm = FileMetaSidecar::default();
    let mut a_files = FileMetaCache::new();
    a_files.insert(
        "a.js".into(),
        CachedFileMeta {
            mtime_secs: 1,
            size_bytes: 2,
            content_hash: "ha".into(),
        },
    );
    let mut b_files = FileMetaCache::new();
    b_files.insert(
        "b.js".into(),
        CachedFileMeta {
            mtime_secs: 3,
            size_bytes: 4,
            content_hash: "hb".into(),
        },
    );
    fm.repos.insert(a.into(), a_files);
    fm.repos.insert(b.into(), b_files);
    save_filemeta_sidecar(&fm, &sidecar_path(&db, ".filemeta.json")).unwrap();

    // resolution_deps sidecar with A and B slices.
    let mut deps = ResolutionDeps::default();
    deps.set_deps_for_repo(a, "a.js", HashSet::from(["a2.js".to_string()]));
    deps.set_deps_for_repo(b, "b.js", HashSet::from(["b2.js".to_string()]));
    let rd_path = sidecar_path(&db, ".resolution_deps.bin");
    deps.save(&rd_path).unwrap();

    // Drop only A.
    remove_repo_sidecar_slices(&db, a).unwrap();

    let fm = load_filemeta_sidecar(&sidecar_path(&db, ".filemeta.json"));
    assert!(!fm.repos.contains_key(a), "A filemeta slice removed");
    assert!(fm.repos.contains_key(b), "B filemeta slice kept");
    assert_eq!(fm.repos[b]["b.js"].content_hash, "hb", "B slice unchanged");

    let deps = ResolutionDeps::load(&rd_path);
    assert!(deps.is_empty_for_repo(a), "A resolution slice removed");
    assert!(!deps.is_empty_for_repo(b), "B resolution slice kept");
}

/// Fail-safe: a missing or corrupt sidecar is a no-op — never a panic, and a
/// missing sidecar is never materialized just because remove-repo ran.
#[test]
fn slice_drop_is_fail_safe() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.lbug");
    let fm_path = sidecar_path(&db, ".filemeta.json");
    let rd_path = sidecar_path(&db, ".resolution_deps.bin");

    // Missing sidecars: no panic, and nothing is created (the uid isn't present,
    // so there's nothing to drop and no reason to write an empty sidecar).
    remove_repo_sidecar_slices(&db, "repo:default:whatever").unwrap();
    assert!(!fm_path.exists(), "missing filemeta not materialized");
    assert!(
        !rd_path.exists(),
        "missing resolution_deps not materialized"
    );

    // Corrupt sidecar: no panic. The corrupt file loads as empty (fail-open), the
    // uid isn't present, so the file is left untouched rather than clobbered.
    std::fs::write(&fm_path, b"{ this is not valid json").unwrap();
    std::fs::write(&rd_path, b"\x00not-msgpack").unwrap();
    remove_repo_sidecar_slices(&db, "repo:default:whatever").unwrap();
    assert_eq!(
        std::fs::read(&fm_path).unwrap(),
        b"{ this is not valid json"
    );
    assert_eq!(std::fs::read(&rd_path).unwrap(), b"\x00not-msgpack");

    // Sanity: HashMap import used (keeps the test self-documenting).
    let _: HashMap<String, u8> = HashMap::new();
}
