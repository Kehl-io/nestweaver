use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nestweaver_schema::{Note, NoteKind, Vault};
use nestweaver_store::GraphStore;
use serde_json::Value;
use tower::ServiceExt;

use crate::state::AppState;

fn insert_vault(store: &GraphStore, uid: &str, root: &Path) {
    store
        .insert_vault(&Vault {
            uid: uid.into(),
            name: uid.into(),
            root_path: root.to_str().unwrap().into(),
            instance_id: "test".into(),
        })
        .unwrap();
}

fn insert_note(store: &GraphStore, vault: &str, id: &str, file: &str, title: &str) {
    store
        .insert_note(&Note {
            uid: format!("note:{id}"),
            vault_uid: vault.into(),
            file_path: file.into(),
            title: title.into(),
            note_kind: NoteKind::General,
            word_count: 0,
            content_hash: "fixture".into(),
            frontmatter: None,
            frontmatter_raw: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        })
        .unwrap();
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn note_detail_and_mentions_use_the_same_indexed_vault_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    let other = temp.path().join("other");
    fs::create_dir_all(vault.join("nested")).unwrap();
    fs::create_dir(&other).unwrap();
    fs::write(vault.join("target.md"), "Résumé\n").unwrap();
    fs::write(vault.join("dupe..name.md"), "A mention of Résumé.\n").unwrap();
    fs::write(vault.join("empty.md"), "").unwrap();
    fs::write(vault.join("binary.md"), [0xff]).unwrap();
    fs::write(vault.join("leaf.md"), "Indexed leaf.\n").unwrap();
    fs::write(vault.join("nested/parent.md"), "Indexed nested.\n").unwrap();
    fs::write(vault.join("inside-link.md"), "Indexed inside.\n").unwrap();
    fs::write(vault.join("unindexed.md"), "Résumé unrelated.\n").unwrap();
    fs::write(other.join("parent.md"), "Résumé outside.\n").unwrap();

    let store = GraphStore::in_memory().unwrap();
    insert_vault(&store, "vlt:local", &vault);
    insert_vault(&store, "vlt:other", &other);
    for (id, path, title) in [
        ("target", "target.md", "Résumé"),
        ("dots", "dupe..name.md", "Dotted"),
        ("empty", "empty.md", "Empty"),
        ("invalid-utf8", "binary.md", "Binary"),
        ("missing", "missing.md", "Missing"),
        ("leaf", "leaf.md", "Leaf"),
        ("parent", "nested/parent.md", "Nested"),
        ("inside", "inside-link.md", "Inside"),
        ("traversal", "../other/parent.md", "Parent traversal"),
        (
            "absolute",
            other.join("parent.md").to_str().unwrap(),
            "Absolute",
        ),
    ] {
        insert_note(&store, "vlt:local", id, path, title);
    }
    insert_note(&store, "vlt:other", "other", "parent.md", "Other vault");
    let app = crate::create_router(AppState::new(store, None, temp.path().join("db")));

    fs::remove_file(vault.join("leaf.md")).unwrap();
    symlink(other.join("parent.md"), vault.join("leaf.md")).unwrap();
    fs::rename(vault.join("nested"), vault.join("old-nested")).unwrap();
    symlink(&other, vault.join("nested")).unwrap();
    fs::remove_file(vault.join("inside-link.md")).unwrap();
    symlink("unindexed.md", vault.join("inside-link.md")).unwrap();

    for (id, expected_body) in [
        ("target", "Résumé\n"),
        ("dots", "A mention of Résumé.\n"),
        ("empty", ""),
        ("invalid-utf8", ""),
        ("missing", ""),
        ("leaf", ""),
        ("parent", ""),
        ("inside", ""),
    ] {
        let (status, body) = get(&app, &format!("/api/v1/brain/note/note:{id}")).await;
        assert_eq!(status, StatusCode::OK, "{id}: {body}");
        assert_eq!(body["note"]["uid"], format!("note:{id}"));
        assert_eq!(body["body"], expected_body, "{id}");
    }
    for id in ["traversal", "absolute"] {
        let (status, body) = get(&app, &format!("/api/v1/brain/note/note:{id}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {body}");
        assert_eq!(body["error"], "invalid note file path");
    }

    let (status, alias) = get(&app, "/api/v1/symbol/note:leaf").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(alias["body"], "");
    let (status, mentions) = get(&app, "/api/v1/brain/unlinked-mentions/note:target").await;
    assert_eq!(status, StatusCode::OK);
    let mentions = mentions.as_array().unwrap();
    assert_eq!(mentions.len(), 1, "{mentions:?}");
    assert_eq!(mentions[0]["note_uid"], "note:dots");
    assert_eq!(mentions[0]["snippet"], "A mention of Résumé.\n");
}
