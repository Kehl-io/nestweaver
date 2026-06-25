use nestweaver_engine::index_directory_in_memory;
use std::path::Path;

#[test]
fn test_sha_set_on_new_repo() {
    let repo_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("testdata/js");

    let (result, store) =
        index_directory_in_memory(&repo_path, "default", "file:///test/js", "abc123").unwrap();

    assert!(result.files_count > 0, "should index at least one file");

    let repos = store.list_repos(None).unwrap();
    let repo = repos.iter().find(|r| r.url == "file:///test/js").unwrap();
    assert_eq!(repo.indexed_sha, "abc123");
}

#[test]
fn test_sha_updated_on_reindex() {
    let repo_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("testdata/js");

    let (_, store) =
        index_directory_in_memory(&repo_path, "default", "file:///test/js", "aaa").unwrap();

    let repos = store.list_repos(None).unwrap();
    let repo = repos.iter().find(|r| r.url == "file:///test/js").unwrap();
    assert_eq!(repo.indexed_sha, "aaa");

    let result2 = nestweaver_engine::index_directory_with_store(
        &store,
        &repo_path,
        &std::env::temp_dir().join("nestweaver-test-sha"),
        "default",
        "file:///test/js",
        "bbb",
        true,
        None,
    )
    .unwrap();

    assert!(result2.files_count > 0);

    let repos2 = store.list_repos(None).unwrap();
    let repo2 = repos2.iter().find(|r| r.url == "file:///test/js").unwrap();
    assert_eq!(repo2.indexed_sha, "bbb");
}
