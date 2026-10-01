//! Builds the small databases in `testdata/lbug-0.20.4/` with LadybugDB
//! 0.20.4, the engine before the 0.21 string-hash change, for NestWeaver's
//! storage-engine cutover tests.
//!
//! Not part of the workspace (the root manifest excludes it, so CI never
//! compiles 0.20.4). Run it only through `../regenerate.sh`, which pins the
//! engine version, builds it from source, checks the output with the current
//! engine, and compresses it into place. See `../README.md`.
//!
//! After writing, this program checks what it wrote with the engine it linked:
//! that engine reports version 0.20.x, and no database carries NestWeaver's
//! engine-format marker (Meta row or sidecar). It exits non-zero otherwise.

use lbug::{Connection, Database, SystemConfig};

fn config() -> SystemConfig {
    SystemConfig::default()
        .max_db_size(1 << 30)
        .max_num_threads(1)
        .buffer_pool_size(64 << 20)
}

fn build(conn: &Connection<'_>) {
    for statement in include_str!("../schema.cypher").lines().filter(|l| !l.trim().is_empty()) {
        conn.query(statement).unwrap_or_else(|e| panic!("{statement}: {e}"));
    }
    for statement in [
        "CREATE (:Meta {key: 'publication.brain_uuid', value: '3f2b8c1e-5d4a-4e6f-9a7b-1c2d3e4f5a6b'})",
        "CREATE (:Meta {key: 'publication.publication_uuid', value: '7a8b9c0d-1e2f-4a3b-8c4d-5e6f7a8b9c0d'})",
        "CREATE (:Meta {key: 'instance.data_instance_id', value: 'default'})",
        "CREATE (:Repo {uid: 'repo:default:café', url: 'file:///fixture/café', indexed_sha: 'local', \
         staleness_commits_behind: 0, instance_id: 'default', name: 'café', root_path: 'fixture-repo'})",
        "CREATE (:Symbol {uid: 'sym:default:café:grüßen', name: 'grüßen', kind: 'Function', \
         repo_uid: 'repo:default:café', file_path: 'café.js', start_line: 1, end_line: 1})",
        "CREATE (:Symbol {uid: 'sym:default:café:main', name: 'main', kind: 'Function', \
         repo_uid: 'repo:default:café', file_path: 'café.js', start_line: 2, end_line: 2})",
    ] {
        conn.query(statement).unwrap_or_else(|e| panic!("{statement}: {e}"));
    }
}

/// Fail loudly unless `db` is what the cutover tests need: written by 0.20.x,
/// with no engine-format marker of either kind.
fn check(db: &std::path::Path) {
    let database = Database::new(db, config().read_only(true))
        .unwrap_or_else(|e| panic!("reopen {}: {e}", db.display()));
    let conn = Connection::new(&database).unwrap();
    let version: String = conn
        .query("CALL db_version() RETURN *")
        .unwrap()
        .flatten()
        .map(|value| value.to_string())
        .collect();
    assert!(
        version.contains("0.20."),
        "{}: the linked engine reports version {version:?}, not 0.20.x; the fixture \
         would be in the new format",
        db.display()
    );
    let marker = conn
        .query("MATCH (m:Meta) RETURN m.key")
        .unwrap()
        .flatten()
        .any(|value| value.to_string() == "storage.string_hash");
    assert!(!marker, "{}: carries an engine-format Meta row", db.display());
    let mut sidecar = db.as_os_str().to_owned();
    sidecar.push(".engine-format");
    assert!(
        !std::path::Path::new(&sidecar).exists(),
        "{}: has an engine-format sidecar",
        db.display()
    );
}

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("output directory"));
    std::fs::create_dir_all(&out).unwrap();
    {
        let db = Database::new(out.join("pre-cutover.lbug"), config()).unwrap();
        let conn = Connection::new(&db).unwrap();
        build(&conn);
        conn.query("CHECKPOINT").unwrap();
    }
    check(&out.join("pre-cutover.lbug"));
    // The same graph with its last write still in the write-ahead log: the
    // process exits without closing the database, as a crash would.
    let db = Database::new(out.join("pre-cutover-wal.lbug"), config().auto_checkpoint(false)).unwrap();
    let conn = Connection::new(&db).unwrap();
    build(&conn);
    conn.query("CHECKPOINT").unwrap();
    conn.query("CREATE (:Repo {uid: 'repo:default:naïve', url: 'file:///fixture/naïve', \
        indexed_sha: 'local', staleness_commits_behind: 0, instance_id: 'default', \
        name: 'naïve', root_path: 'fixture-repo'})").unwrap();
    std::process::exit(0);
}
