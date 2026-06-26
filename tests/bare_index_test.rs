//! End-to-end integration test proving GitBareReader + index pipeline works.
//!
//! Creates a git repo, clones it bare, indexes via GitBareReader,
//! and verifies symbols are searchable in the graph store.
//!
//! Run with:
//!   cargo test --test bare_index_test -- --test-threads=1

use std::path::Path;
use std::process::Command;

use nestweaver_engine::content_reader::{ContentReader, GitBareReader};
use nestweaver_engine::index::index_with_reader;
use nestweaver_store::GraphStore;
use nestweaver_store::SeedResolutionConfig;

/// Create a git repo at `dir` with the given files, commit them,
/// and return the HEAD SHA.
fn create_test_repo(dir: &Path, files: &[(&str, &str)]) -> String {
    std::fs::create_dir_all(dir).unwrap();

    // git init
    Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();

    // Write files
    for (path, content) in files {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
    }

    // Stage and commit
    Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(dir)
        .output()
        .unwrap();

    // Return HEAD SHA
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Clone a repo as a bare clone and return the bare path.
fn bare_clone(src: &Path, dest: &Path) {
    let out = Command::new("git")
        .args([
            "clone",
            "--bare",
            &src.display().to_string(),
            &dest.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bare clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn index_via_bare_clone_produces_searchable_symbols() {
    let tmp = tempfile::tempdir().unwrap();

    // 1. Create a source repo with known symbols in multiple languages.
    let src = tmp.path().join("source");
    let _sha = create_test_repo(
        &src,
        &[
            (
                "src/main.js",
                "function greet(name) { return `Hello ${name}`; }\nfunction farewell() { return 'bye'; }",
            ),
            (
                "src/utils.js",
                "export function add(a, b) { return a + b; }\nexport const PI = 3.14;",
            ),
            (
                "src/lib.rs",
                "pub fn hello() -> &'static str { \"hello\" }\npub fn add(a: i32, b: i32) -> i32 { a + b }",
            ),
        ],
    );

    // 2. Bare clone
    let bare = tmp.path().join("source.git");
    bare_clone(&src, &bare);

    // 3. Create GitBareReader from HEAD
    let reader = GitBareReader::from_head(&bare).unwrap();

    // Sanity: verify file listing works
    let files = reader.list_files().unwrap();
    assert!(
        files.len() >= 3,
        "expected at least 3 files, got {}",
        files.len()
    );

    // Sanity: verify file reading works
    let content = reader.read_file(Path::new("src/main.js")).unwrap();
    assert!(content.contains("greet"), "content should contain 'greet'");

    // 4. Index via GitBareReader into an in-memory store
    let store = GraphStore::in_memory().unwrap();
    let result = index_with_reader(
        &reader,
        &store,
        "test-instance",
        "file:///source.git",
        reader.version_id(),
        Some("test-repo"),
    )
    .unwrap();

    // Should have indexed some symbols
    assert!(
        result.symbols_count > 0,
        "expected symbols to be indexed, got {}",
        result.symbols_count
    );

    // 5. Verify symbols are searchable via the graph store
    let cfg = SeedResolutionConfig::default();
    let results = store.search_symbols_by_name("greet", 10, &cfg).unwrap();
    assert!(
        !results.is_empty(),
        "search for 'greet' should return results"
    );
    assert!(
        results.iter().any(|s| s.name == "greet"),
        "should find a symbol named 'greet', got: {:?}",
        results.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    // Search for another symbol
    let results2 = store.search_symbols_by_name("add", 10, &cfg).unwrap();
    assert!(
        !results2.is_empty(),
        "search for 'add' should return results"
    );

    // Verify the repo node was created
    let r_uid = nestweaver_schema::repo_uid("test-instance", "file:///source.git");
    let repo = store.lookup_repo(&r_uid).unwrap();
    assert!(repo.is_some(), "repo node should exist in the store");
}

#[test]
fn bare_clone_indexes_rust_symbols() {
    let tmp = tempfile::tempdir().unwrap();

    let src = tmp.path().join("rust-repo");
    let _sha = create_test_repo(
        &src,
        &[
            (
                "src/lib.rs",
                concat!(
                    "pub struct Config {\n",
                    "    pub name: String,\n",
                    "    pub port: u16,\n",
                    "}\n\n",
                    "impl Config {\n",
                    "    pub fn new(name: &str, port: u16) -> Self {\n",
                    "        Self { name: name.to_string(), port }\n",
                    "    }\n",
                    "}\n"
                ),
            ),
            (
                "src/main.rs",
                concat!(
                    "mod lib;\n",
                    "fn main() {\n",
                    "    let cfg = lib::Config::new(\"app\", 8080);\n",
                    "    println!(\"{}\", cfg.name);\n",
                    "}\n"
                ),
            ),
        ],
    );

    let bare = tmp.path().join("rust-repo.git");
    bare_clone(&src, &bare);

    let reader = GitBareReader::from_head(&bare).unwrap();
    let store = GraphStore::in_memory().unwrap();
    let result = index_with_reader(
        &reader,
        &store,
        "test",
        "file:///rust-repo.git",
        reader.version_id(),
        None,
    )
    .unwrap();

    assert!(result.symbols_count > 0);

    let cfg = SeedResolutionConfig::default();

    // Should find Config struct
    let results = store.search_symbols_by_name("Config", 10, &cfg).unwrap();
    assert!(
        !results.is_empty(),
        "search for 'Config' should return results"
    );

    // Should find main function
    let results = store.search_symbols_by_name("main", 10, &cfg).unwrap();
    assert!(
        !results.is_empty(),
        "search for 'main' should return results"
    );
}

#[test]
fn bare_clone_reader_version_matches_head_sha() {
    let tmp = tempfile::tempdir().unwrap();

    let src = tmp.path().join("ver-repo");
    let sha = create_test_repo(&src, &[("hello.js", "function hello() {}")]);

    let bare = tmp.path().join("ver-repo.git");
    bare_clone(&src, &bare);

    let reader = GitBareReader::from_head(&bare).unwrap();
    assert_eq!(
        reader.version_id(),
        sha,
        "GitBareReader version_id should match the HEAD SHA"
    );
}
