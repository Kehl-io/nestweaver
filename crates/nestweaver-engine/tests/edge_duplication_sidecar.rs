//! Group E (`fix/incremental-edge-duplication`): an incremental index must
//! never create a second copy of a resolved edge it already created.
//!
//! Root cause (pre-change): edges are inserted with a bare `CREATE` and the
//! stale-edge clear ran only when a resolve filter existed. A missing, corrupt,
//! or stale-version `.resolution_deps.bin` sidecar fails open to an empty dep
//! map; combined with a still-valid filemeta sidecar (`files_unchanged > 0`)
//! that meant full unfiltered resolution re-`CREATE`d every resolved edge on
//! top of the existing ones — multiplicity 2 per edge per affected run.
//!
//! The fix has three parts, each pinned here:
//!   1. An empty-for-repo deps slice bypasses the filemeta cache, so every
//!      file classifies Parsed and the atomic `bulk_reindex_write` replaces
//!      the repo's graph instead of accumulating on top of it.
//!   2. The stale-edge clear also runs repo-wide whenever full resolution
//!      runs (no filter), not only per filtered file.
//!   3. Resolution records dep sets for every re-resolved file INCLUDING
//!      empty sets, so a repo whose files legitimately have zero cross-file
//!      edges does not look empty-for-repo on the next run (which would
//!      force a full replacement on every index).
//!
//! Multiplicities are measured through the public `load_typed_edges` read
//! API: it returns one row per edge with no DISTINCT, so a duplicated
//! (src, tgt, type) triple shows up with multiplicity > 1.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use nestweaver_engine::resolution_cache::ResolutionDeps;
use nestweaver_engine::{load_filemeta_sidecar, sidecar_path};
use nestweaver_schema::uid::repo_uid;
use nestweaver_store::GraphStore;

const INST: &str = "test";
const URL: &str = "https://example.com/repo";

/// Two-file fixture with a cross-file call (main.js → helper.js) and a
/// same-file call (greet → hello inside main.js), so the tests can tell apart
/// edges with both endpoints in an unchanged file (the duplication vector)
/// from edges incident to a re-indexed file (deleted and re-created once).
fn write_fixture(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    std::fs::write(
        repo.join("main.js"),
        "import { helper } from './helper.js';\n\
         \n\
         function greet() {\n\
         \x20   return hello();\n\
         }\n\
         \n\
         function hello() {\n\
         \x20   return helper();\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("helper.js"),
        "export function helper() {\n\x20   return 41;\n}\n",
    )
    .unwrap();
}

/// (src_uid, tgt_uid, edge_type) → number of identical edges in the store.
fn edge_multiplicities(store: &GraphStore) -> HashMap<(String, String, String), usize> {
    let mut m = HashMap::new();
    for (src, tgt, ty, _confidence, _evidence) in store.load_typed_edges().unwrap() {
        *m.entry((src, tgt, ty)).or_insert(0) += 1;
    }
    m
}

/// The fixture must produce at least one CALLS and one IMPORTS edge, each
/// exactly once — otherwise the multiplicity comparisons below prove nothing.
fn sane_baseline(m: &HashMap<(String, String, String), usize>) {
    assert!(
        m.keys().any(|(_, _, ty)| ty == "CALLS"),
        "fixture must produce a CALLS edge; got {m:?}"
    );
    assert!(
        m.keys().any(|(_, _, ty)| ty == "IMPORTS"),
        "fixture must produce a cross-file IMPORTS edge; got {m:?}"
    );
    assert!(
        m.values().all(|&n| n == 1),
        "fixture must not legitimately contain parallel edges; got {m:?}"
    );
}

fn deps_sidecar(db: &Path) -> std::path::PathBuf {
    sidecar_path(db, ".resolution_deps.bin")
}

/// The hazard precondition: the deps sidecar is empty for this repo while the
/// filemeta sidecar still holds its slice — i.e. unchanged files WOULD stay
/// classified Unchanged, which is exactly the gap that duplicated edges.
fn assert_hazard_precondition(db: &Path, uid: &str) {
    assert!(
        ResolutionDeps::load(&deps_sidecar(db)).is_empty_for_repo(uid),
        "deps sidecar must be empty for the repo or the test exercises nothing"
    );
    assert!(
        load_filemeta_sidecar(&sidecar_path(db, ".filemeta.json"))
            .repos
            .contains_key(uid),
        "filemeta slice must still be present (files_unchanged > 0 precondition)"
    );
}

/// Trigger 1 (partial duplication): one file changed, one file unchanged, deps
/// sidecar deleted. Pre-change the unchanged file kept its symbols and edges
/// while full unfiltered resolution re-`CREATE`d its edges — the same-file
/// CALLS edge greet→hello reached multiplicity 2. Post-change the empty deps
/// slice bypasses the filemeta cache, so both files re-parse and the atomic
/// full replacement leaves every multiplicity at 1.
#[test]
fn sidecar_deleted_with_one_changed_file_keeps_edge_multiplicities() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let db = dir.path().join("t.lbug");
    let uid = repo_uid(INST, URL);
    write_fixture(&repo);

    let store = GraphStore::open(&db).unwrap();
    nestweaver_engine::index_directory_with_store(
        &store, &repo, &db, INST, URL, "sha1", false, None,
    )
    .unwrap();
    let baseline = edge_multiplicities(&store);
    sane_baseline(&baseline);

    // Change helper.js without changing its symbol or edge set (comment only).
    // The sleep guarantees an mtime delta so tiered change detection
    // cannot skip the file as mtime-identical.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        repo.join("helper.js"),
        "export function helper() {\n\x20   return 41;\n}\n// touched\n",
    )
    .unwrap();

    std::fs::remove_file(deps_sidecar(&db)).unwrap();
    assert_hazard_precondition(&db, &uid);

    // Re-index WITHOUT --force.
    let result = nestweaver_engine::index_directory_with_store(
        &store, &repo, &db, INST, URL, "sha2", false, None,
    )
    .unwrap();

    // Primary acceptance assertion first: identical multiplicities.
    let after = edge_multiplicities(&store);
    assert_eq!(
        after, baseline,
        "re-index with a deleted deps sidecar must not duplicate edges"
    );
    // And the mechanism: the empty deps slice must have bypassed the filemeta
    // cache, forcing the atomic full-replacement path.
    assert_eq!(
        result.files_unchanged, 0,
        "empty deps slice must bypass the filemeta cache (full-replacement path)"
    );
    assert_eq!(result.files_count, 2, "both files re-parsed");
}

/// Trigger 2 (total duplication): zero changed files, deps sidecar deleted.
/// Pre-change the incremental branch performed NO deletes at all
/// (`actually_changed_files` empty) and full resolution re-`CREATE`d EVERY
/// resolved edge in the repo — multiplicity 2 across the board. Post-change
/// the cache bypass forces the same full replacement as trigger 1.
#[test]
fn sidecar_deleted_with_zero_changed_files_keeps_edge_multiplicities() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let db = dir.path().join("t.lbug");
    let uid = repo_uid(INST, URL);
    write_fixture(&repo);

    let store = GraphStore::open(&db).unwrap();
    nestweaver_engine::index_directory_with_store(
        &store, &repo, &db, INST, URL, "sha1", false, None,
    )
    .unwrap();
    let baseline = edge_multiplicities(&store);
    sane_baseline(&baseline);

    std::fs::remove_file(deps_sidecar(&db)).unwrap();
    assert_hazard_precondition(&db, &uid);

    // Re-index with NO file changes at all, without --force.
    let result = nestweaver_engine::index_directory_with_store(
        &store, &repo, &db, INST, URL, "sha1", false, None,
    )
    .unwrap();

    // Primary acceptance assertion first: identical multiplicities.
    let after = edge_multiplicities(&store);
    assert_eq!(
        after, baseline,
        "zero-change re-index with a deleted deps sidecar must not duplicate edges"
    );
    assert!(
        after.values().all(|&n| n == 1),
        "every edge must have multiplicity exactly 1; got {after:?}"
    );
    // And the mechanism: the empty deps slice must have bypassed the filemeta
    // cache, forcing the atomic full-replacement path.
    assert_eq!(
        result.files_unchanged, 0,
        "empty deps slice must bypass the filemeta cache (full-replacement path)"
    );
}

/// Trigger 3 (CACHE_VERSION bump): a well-formed sidecar written under a
/// stale/wrong format version. `ResolutionDeps::load` fails open to empty on
/// version mismatch (unit-pinned by `old_format_bin_loads_empty`), so the
/// next index of ANY repository lands in the same empty-deps gap as a deleted
/// sidecar. This is the acceptance criterion "a CACHE_VERSION bump does not
/// duplicate edges" — the trigger is a routine maintenance action.
#[test]
fn stale_version_sidecar_keeps_edge_multiplicities() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let db = dir.path().join("t.lbug");
    let uid = repo_uid(INST, URL);
    write_fixture(&repo);

    let store = GraphStore::open(&db).unwrap();
    nestweaver_engine::index_directory_with_store(
        &store, &repo, &db, INST, URL, "sha1", false, None,
    )
    .unwrap();
    let baseline = edge_multiplicities(&store);
    sane_baseline(&baseline);

    // Overwrite the sidecar with a well-formed MessagePack payload under a
    // version the current loader rejects — what a CACHE_VERSION bump leaves
    // on disk for every repo. Field order mirrors the private
    // `ResolutionCacheFile` (version, repos); rmp-serde encodes structs as
    // arrays, so this deserializes and then fails the version check. Even if
    // the private struct's field order ever changes, deserialization fails
    // and the loader still fails open — the test stays valid either way.
    #[derive(serde::Serialize)]
    struct StaleSidecar {
        version: u32,
        repos: HashMap<String, HashMap<String, HashSet<String>>>,
    }
    let stale = StaleSidecar {
        version: u32::MAX, // any version != current CACHE_VERSION
        repos: HashMap::from([(
            uid.clone(),
            HashMap::from([(
                "main.js".to_string(),
                HashSet::from(["helper.js".to_string()]),
            )]),
        )]),
    };
    let rd_path = deps_sidecar(&db);
    std::fs::write(&rd_path, rmp_serde::to_vec(&stale).unwrap()).unwrap();
    assert_hazard_precondition(&db, &uid);

    // Re-index with no file changes, without --force.
    let result = nestweaver_engine::index_directory_with_store(
        &store, &repo, &db, INST, URL, "sha1", false, None,
    )
    .unwrap();

    // Primary acceptance assertion first: identical multiplicities.
    let after = edge_multiplicities(&store);
    assert_eq!(
        after, baseline,
        "a CACHE_VERSION bump must not duplicate edges on the next index"
    );
    // And the mechanism: a stale-version sidecar must trigger the same cache
    // bypass as a deleted one.
    assert_eq!(
        result.files_unchanged, 0,
        "stale-version sidecar must trigger the same cache bypass as a deleted one"
    );
}

/// Companion fix: files that resolve to ZERO outbound edges are still recorded
/// in the deps sidecar (as empty sets), so a repo with no cross-file edges
/// does NOT look empty-for-repo on the next run. Without the recording, the
/// cache bypass above would misfire (and log its warn) on every index of such
/// a repo — a perpetual full-replacement regression. `files_unchanged == 2`
/// on the second run is the behavioral proof that the bypass branch (and its
/// warn) did not fire: they share one `if`.
#[test]
fn no_edge_repo_records_empty_dep_sets_and_skips_full_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let db = dir.path().join("t.lbug");
    let uid = repo_uid(INST, URL);
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.js"), "function alpha() { return 1; }\n").unwrap();
    std::fs::write(repo.join("b.js"), "function beta() { return 2; }\n").unwrap();

    let store = GraphStore::open(&db).unwrap();
    let result1 = nestweaver_engine::index_directory_with_store(
        &store, &repo, &db, INST, URL, "sha1", false, None,
    )
    .unwrap();
    assert_eq!(result1.files_count, 2);

    // The recording itself: the repo's deps slice is non-empty even though
    // neither file has any cross-file dependency.
    let deps = ResolutionDeps::load(&deps_sidecar(&db));
    assert!(
        !deps.is_empty_for_repo(&uid),
        "files with zero resolved edges must still be recorded (as empty sets), \
         or the empty-deps cache bypass forces a full replacement on every index"
    );

    // Second index without --force: both files classify Unchanged, resolution
    // is skipped, no forced full replacement.
    let result2 = nestweaver_engine::index_directory_with_store(
        &store, &repo, &db, INST, URL, "sha1", false, None,
    )
    .unwrap();
    assert_eq!(
        result2.files_unchanged, 2,
        "recorded (empty) dep sets must keep the no-edge repo on the incremental path"
    );
    assert_eq!(result2.files_count, 0, "no files re-parsed");
}
