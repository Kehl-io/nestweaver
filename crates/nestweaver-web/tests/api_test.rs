use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use nestweaver_schema::{
    EdgeType, Note, NoteKind, Repo, ResolvedEdge, Section, Service, Symbol, SymbolKind, Vault,
    Visibility,
};
use nestweaver_store::{GraphScope, GraphStore};
use nestweaver_web::create_router;
use nestweaver_web::state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let app = app.clone();
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let app = app.clone();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[allow(dead_code)]
async fn put_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let app = app.clone();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[allow(dead_code)]
async fn delete_request(app: &axum::Router, uri: &str) -> StatusCode {
    let app = app.clone();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    response.status()
}

fn setup_test_store() -> GraphStore {
    let store = GraphStore::in_memory().unwrap();

    let repo = Repo {
        uid: "repo:test".to_string(),
        url: "https://example.com/test.git".to_string(),
        indexed_sha: "abc123".to_string(),
        staleness_commits_behind: 0,
        instance_id: String::new(),
        name: None,
        root_path: None,
    };
    store.insert_repo(&repo).unwrap();

    let symbol = Symbol {
        uid: "sym:test:greet".to_string(),
        name: "greet".to_string(),
        kind: SymbolKind::Function,
        repo_uid: "repo:test".to_string(),
        file_path: "src/main.js".to_string(),
        start_line: 1,
        end_line: 1,
        signature: "function greet(name)".to_string(),
        summary: None,
        content_hash: "hash123".to_string(),
        embedding: None,
        pagerank_score: Some(0.85),
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    };
    store.insert_symbol(&symbol).unwrap();

    store
        .compute_pagerank(0.85, 20, &GraphScope::code_only())
        .unwrap();

    store
}

fn make_app() -> axum::Router {
    let store = setup_test_store();
    let state = AppState::new(store, None, std::path::PathBuf::from("/tmp/test.lbug"));
    create_router(state)
}

#[tokio::test]
async fn health_check_returns_ok() {
    let app = make_app();
    let (status, _) = get_json(&app, "/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn search_returns_matching_symbols() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/search?q=greet").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert!(!arr.is_empty(), "should find at least one result");
    assert_eq!(arr[0]["name"], "greet");
}

#[tokio::test]
async fn search_empty_query_returns_400() {
    let app = make_app();
    let (status, _) = get_json(&app, "/api/v1/search?q=").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn symbol_lookup_not_found_returns_404() {
    let app = make_app();
    let (status, _) = get_json(&app, "/api/v1/symbol/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn symbol_lookup_by_ambiguous_name_is_not_bare_300() {
    let store = GraphStore::in_memory().unwrap();
    store
        .insert_repo(&Repo {
            uid: "repo:js-ping".to_string(),
            url: "https://example.com/js-ping.git".to_string(),
            indexed_sha: "js".to_string(),
            staleness_commits_behind: 0,
            instance_id: String::new(),
            name: Some("js-ping".to_string()),
            root_path: None,
        })
        .unwrap();
    store
        .insert_repo(&Repo {
            uid: "repo:py-ping".to_string(),
            url: "https://example.com/py-ping.git".to_string(),
            indexed_sha: "py".to_string(),
            staleness_commits_behind: 0,
            instance_id: String::new(),
            name: Some("py-ping".to_string()),
            root_path: None,
        })
        .unwrap();
    for (uid, repo_uid, path) in [
        ("sym:js-ping:ping", "repo:js-ping", "src/lib.js"),
        ("sym:py-ping:ping", "repo:py-ping", "src/lib.py"),
    ] {
        store
            .insert_symbol(&Symbol {
                uid: uid.to_string(),
                name: "ping".to_string(),
                kind: SymbolKind::Function,
                repo_uid: repo_uid.to_string(),
                file_path: path.to_string(),
                start_line: 1,
                end_line: 2,
                signature: "function ping()".to_string(),
                summary: None,
                content_hash: format!("hash-{uid}"),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();
    }

    let state = AppState::new(store, None, std::path::PathBuf::from("/tmp/ping.lbug"));
    let app = create_router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/symbol/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .cloned();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "GET /api/v1/symbol/ping must be HTTP 409 with JSON candidates (fetch() treats bare 300 as failure); \
         location={location:?} body={json}"
    );
    let listed = json
        .get("candidate_uids")
        .or_else(|| json.get("candidates"))
        .cloned()
        .unwrap_or(json.clone());
    let count = listed.as_array().map(Vec::len).unwrap_or(0);
    assert!(
        count >= 2,
        "ambiguous symbol lookup must list candidates: {json}"
    );
}

fn error_message(json: &Value) -> &str {
    json.get("error").and_then(Value::as_str).unwrap_or("")
}

#[tokio::test]
async fn symbol_lookup_note_uid_is_not_missing_symbol() {
    let store = setup_test_store();
    store
        .insert_vault(&Vault {
            uid: "vault:test".to_string(),
            name: "test-vault".to_string(),
            root_path: "/tmp/test-vault".to_string(),
            instance_id: String::new(),
        })
        .unwrap();
    store
        .insert_note(&Note {
            uid: "note:test:alpha".to_string(),
            vault_uid: "vault:test".to_string(),
            file_path: "Alpha.md".to_string(),
            title: "Alpha".to_string(),
            note_kind: NoteKind::General,
            word_count: 12,
            content_hash: "note-hash".to_string(),
            frontmatter: None,
            frontmatter_raw: None,
            created_at: None,
            modified_at: None,
            pagerank_score: Some(0.4),
            embedding: None,
        })
        .unwrap();
    let state = AppState::new(
        store,
        None,
        std::path::PathBuf::from("/tmp/note-symbol.lbug"),
    );
    let app = create_router(state);

    let (missing_status, missing_json) = get_json(&app, "/api/v1/symbol/note:does-not-exist").await;
    let missing_error = error_message(&missing_json);
    assert!(
        !missing_error.contains("symbol 'note:"),
        "note: UIDs must not 404 as missing code symbols: status={missing_status} body={missing_json}"
    );
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert!(
        missing_error.contains("note 'note:does-not-exist' not found"),
        "missing note: UID should 404 from the note handler: {missing_json}"
    );

    let (status, json) = get_json(&app, "/api/v1/symbol/note:test:alpha").await;
    let err = error_message(&json);
    assert!(
        !err.contains("symbol 'note:"),
        "selecting a vault note must not hit symbol-by-uid 404: status={status} body={json}"
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["note"]["uid"], "note:test:alpha");
    assert_eq!(json["note"]["title"], "Alpha");
}

#[tokio::test]
async fn symbol_lookup_file_path_with_kind_file_is_not_missing_symbol() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/symbol/src%2Fmain.js?kind=file").await;
    let err = error_message(&json);
    assert!(
        !err.contains("symbol 'src/main.js' not found"),
        "kind=file must not 404 as a missing symbol when the file exists: status={status} body={json}"
    );
    assert_eq!(status, StatusCode::OK);
    let listed = json
        .as_array()
        .cloned()
        .or_else(|| json.get("symbols").and_then(Value::as_array).cloned())
        .expect("file lookup should return symbols for the path");
    assert!(
        listed.iter().any(|item| item["file_path"] == "src/main.js"),
        "file selection should surface symbols from the indexed path: {json}"
    );
}

#[tokio::test]
async fn context_returns_seeds_and_connected() {
    let app = make_app();
    let (status, json) = post_json(&app, "/api/v1/context", json!({ "seeds": ["greet"] })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("seeds").is_some(), "response should have 'seeds'");
    assert!(
        json.get("connected").is_some(),
        "response should have 'connected'"
    );
}

#[tokio::test]
async fn context_empty_seeds_returns_400() {
    let app = make_app();
    let (status, _) = post_json(&app, "/api/v1/context", json!({ "seeds": [] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

fn assert_context_seed_is_client_error(status: StatusCode, json: &Value, raw_seed: &str) {
    assert_ne!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "unresolved context seeds must not 500: {json}"
    );
    assert!(
        status.is_client_error(),
        "unresolved context seeds must be 4xx, got {status}: {json}"
    );
    let body = json.to_string();
    assert!(
        !body.contains(raw_seed),
        "error body must not echo the raw attacker seed: {json}"
    );
}

#[tokio::test]
async fn context_unresolved_xss_seed_is_4xx_without_echo() {
    let app = make_app();
    let seed = "<script>alert(1)</script>";
    for uri in ["/api/v1/context", "/api/v1/brain/context"] {
        let (status, json) = post_json(&app, uri, json!({ "seeds": [seed] })).await;
        assert_context_seed_is_client_error(status, &json, seed);
    }
}

#[tokio::test]
async fn context_unresolved_path_seed_is_4xx_without_echo() {
    let app = make_app();
    let seed = "../etc/passwd";
    for uri in ["/api/v1/context", "/api/v1/brain/context"] {
        let (status, json) = post_json(&app, uri, json!({ "seeds": [seed] })).await;
        assert_context_seed_is_client_error(status, &json, seed);
    }
}

#[tokio::test]
async fn context_ghost_note_uid_is_4xx_without_echo() {
    let app = make_app();
    let seed = "note:vlt:ghost:deadbeef";
    for uri in ["/api/v1/context", "/api/v1/brain/context"] {
        let (status, json) = post_json(&app, uri, json!({ "seeds": [seed] })).await;
        assert_context_seed_is_client_error(status, &json, seed);
    }
}

#[tokio::test]
async fn brain_context_valid_seed_still_200() {
    let app = make_app();
    let (status, json) =
        post_json(&app, "/api/v1/brain/context", json!({ "seeds": ["greet"] })).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json.get("seeds").is_some(), "response should have 'seeds'");
}

fn make_app_with_ellipsis_and_angle_seeds() -> axum::Router {
    let store = setup_test_store();
    store
        .insert_symbol(&Symbol {
            uid: "sym:test:wait-ellipsis".to_string(),
            name: "Wait...".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:test".to_string(),
            file_path: "src/wait.js".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "function Wait...()".to_string(),
            summary: None,
            content_hash: "hash-wait".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        })
        .unwrap();
    store
        .insert_symbol(&Symbol {
            uid: "sym:test:vector-t".to_string(),
            name: "vector<T>".to_string(),
            kind: SymbolKind::Class,
            repo_uid: "repo:test".to_string(),
            file_path: "src/vector.h".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "template <typename T> class vector".to_string(),
            summary: None,
            content_hash: "hash-vector".to_string(),
            embedding: None,
            pagerank_score: Some(0.4),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        })
        .unwrap();
    store
        .insert_vault(&Vault {
            uid: "vault:ellipsis".to_string(),
            name: "ellipsis-vault".to_string(),
            root_path: "/tmp/ellipsis-vault".to_string(),
            instance_id: String::new(),
        })
        .unwrap();
    store
        .insert_note(&Note {
            uid: "note:ellipsis:wait".to_string(),
            vault_uid: "vault:ellipsis".to_string(),
            file_path: "Wait.md".to_string(),
            title: "Wait...".to_string(),
            note_kind: NoteKind::General,
            word_count: 8,
            content_hash: "note-wait".to_string(),
            frontmatter: None,
            frontmatter_raw: None,
            created_at: None,
            modified_at: None,
            pagerank_score: Some(0.3),
            embedding: None,
        })
        .unwrap();
    let state = AppState::new(
        store,
        None,
        std::path::PathBuf::from("/tmp/ellipsis-context.lbug"),
    );
    create_router(state)
}

#[tokio::test]
async fn context_resolvable_seed_with_ellipsis_or_angles_is_200() {
    let app = make_app_with_ellipsis_and_angle_seeds();
    for (uri, seed) in [
        ("/api/v1/context", "Wait..."),
        ("/api/v1/context", "vector<T>"),
        ("/api/v1/brain/context", "Wait..."),
        ("/api/v1/brain/context", "vector<T>"),
    ] {
        let (status, json) = post_json(&app, uri, json!({ "seeds": [seed] })).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{uri} seed {seed:?} must not be rejected for containing '..' or '<'/'>': {json}"
        );
        assert!(
            json.get("seeds").is_some(),
            "{uri} seed {seed:?} should have 'seeds': {json}"
        );
    }
}

#[tokio::test]
async fn impact_not_found_returns_404() {
    let app = make_app();
    let (status, _) = get_json(&app, "/api/v1/impact/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn repos_returns_list() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/repos").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert_eq!(arr.len(), 1, "should have exactly one repo");
}

#[tokio::test]
async fn services_returns_empty() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/services").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert!(arr.is_empty(), "should have no services");
}

#[tokio::test]
async fn overview_returns_ranked_landmarks() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/overview?limit=10").await;
    assert_eq!(status, StatusCode::OK);

    assert!(json.get("counts").is_some(), "should include counts");
    assert_eq!(json["counts"]["repo_count"], 1);
    assert_eq!(json["counts"]["symbol_count"], 1);

    let landmarks = json["landmarks"]
        .as_array()
        .expect("landmarks should be an array");
    assert!(
        landmarks.iter().any(|item| item["uid"] == "sym:test:greet"),
        "overview should include top symbol"
    );
    assert!(
        landmarks
            .iter()
            .any(|item| item["uid"] == "repo:test" && item["label"] == "test.git"),
        "overview should preserve literal repo URL segment"
    );

    let start_here = json["start_here"]
        .as_array()
        .expect("start_here should be an array");
    assert!(
        start_here.iter().any(|item| item["kind"] == "symbol"),
        "start_here should include symbol guidance"
    );
}

#[tokio::test]
async fn overview_keeps_symbol_when_repos_exceed_limit() {
    let store = setup_test_store();
    for index in 0..8 {
        let repo = Repo {
            uid: format!("repo:extra:{index}"),
            url: format!("https://example.com/extra-{index}.git"),
            indexed_sha: format!("extra-{index}"),
            staleness_commits_behind: 0,
            instance_id: String::new(),
            name: None,
            root_path: None,
        };
        store.insert_repo(&repo).unwrap();
    }

    let state = AppState::new(store, None, std::path::PathBuf::from("/tmp/test.lbug"));
    let app = create_router(state);
    let (status, json) = get_json(&app, "/api/v1/overview?limit=6").await;
    assert_eq!(status, StatusCode::OK);

    let landmarks = json["landmarks"]
        .as_array()
        .expect("landmarks should be an array");
    assert_eq!(landmarks.len(), 6);
    assert!(
        landmarks.iter().any(|item| item["uid"] == "sym:test:greet"),
        "overview should retain a representative symbol when repos exceed limit"
    );
}

#[tokio::test]
async fn brain_status_returns_counts() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/brain/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("vault_count").is_some(), "should have vault_count");
    assert!(json.get("note_count").is_some(), "should have note_count");
    assert!(
        json.get("heading_count").is_some(),
        "should have heading_count"
    );
    assert!(
        json.get("section_count").is_some(),
        "should have section_count"
    );
    assert!(json.get("tag_count").is_some(), "should have tag_count");
    assert!(
        json.get("wikilink_count").is_some(),
        "should have wikilink_count"
    );
    assert!(
        json.get("cross_domain_count").is_some(),
        "should have cross_domain_count"
    );
}

#[tokio::test]
async fn brain_vaults_returns_list() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/brain/vaults").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.as_array().is_some(), "response should be an array");
}

fn insert_vault_notes(store: &GraphStore, count: usize) {
    store
        .insert_vault(&Vault {
            uid: "vlt:notes".to_string(),
            name: "Notes".to_string(),
            root_path: "/tmp/notes".to_string(),
            instance_id: "local".to_string(),
        })
        .unwrap();
    for i in 0..count {
        store
            .insert_note(&Note {
                uid: format!("note:notes:{i:03}"),
                vault_uid: "vlt:notes".to_string(),
                file_path: format!("n{i:03}.md"),
                title: format!("Note {i:03}"),
                note_kind: NoteKind::General,
                word_count: 10,
                content_hash: format!("h{i:03}"),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
    }
}

fn notes_list_app(note_count: usize) -> axum::Router {
    let store = setup_test_store();
    insert_vault_notes(&store, note_count);
    let state = AppState::new(store, None, std::path::PathBuf::from("/tmp/test.lbug"));
    create_router(state)
}

fn backlinks_app(source_count: usize) -> axum::Router {
    let store = setup_test_store();
    store
        .insert_vault(&Vault {
            uid: "vlt:notes".to_string(),
            name: "Notes".to_string(),
            root_path: "/tmp/notes".to_string(),
            instance_id: "local".to_string(),
        })
        .unwrap();
    store
        .insert_note(&Note {
            uid: "note:notes:target".to_string(),
            vault_uid: "vlt:notes".to_string(),
            file_path: "Target.md".to_string(),
            title: "Target".to_string(),
            note_kind: NoteKind::General,
            word_count: 1,
            content_hash: "ht".to_string(),
            frontmatter: None,
            frontmatter_raw: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        })
        .unwrap();
    let mut note_secs: Vec<(String, String)> = Vec::new();
    for i in 0..source_count {
        let note_uid = format!("note:notes:src{i:03}");
        let sec_uid = format!("sec:notes:src{i:03}");
        store
            .insert_note(&Note {
                uid: note_uid.clone(),
                vault_uid: "vlt:notes".to_string(),
                file_path: format!("src{i:03}.md"),
                title: format!("Src {i:03}"),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: format!("hs{i:03}"),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: sec_uid.clone(),
                note_uid: note_uid.clone(),
                heading_uid: None,
                start_line: 1,
                end_line: 2,
                text_hash: format!("th{i:03}"),
                text_content: "[[Target]]".to_string(),
                word_count: 1,
                pagerank_score: None,
            })
            .unwrap();
        note_secs.push((note_uid, sec_uid));
    }
    let section_edges: Vec<(&str, &str)> = note_secs
        .iter()
        .map(|(n, s)| (n.as_str(), s.as_str()))
        .collect();
    store
        .batch_insert_note_section_edges(&section_edges)
        .unwrap();
    let wiki: Vec<(&str, &str, f32, &str, &str)> = note_secs
        .iter()
        .map(|(_, s)| (s.as_str(), "note:notes:target", 1.0, "Target", "Target"))
        .collect();
    store.batch_insert_wikilink_to_note_edges(&wiki).unwrap();
    let state = AppState::new(store, None, std::path::PathBuf::from("/tmp/test.lbug"));
    create_router(state)
}

#[tokio::test]
async fn brain_notes_limit_1_returns_one_row() {
    let app = notes_list_app(5);
    let (status, json) = get_json(&app, "/api/v1/brain/notes?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json
        .as_array()
        .expect("sibling brain list routes return a raw array");
    assert_eq!(arr.len(), 1, "?limit=1 must return exactly one note");
}

#[tokio::test]
async fn brain_notes_omitted_limit_does_not_dump_unbounded_corpus() {
    // Documented default matches `/symbols/top`: 20, hard cap 1000.
    const CORPUS: usize = 25;
    let app = notes_list_app(CORPUS);
    let (status, json) = get_json(&app, "/api/v1/brain/notes").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json
        .as_array()
        .expect("sibling brain list routes return a raw array");
    assert_eq!(
        arr.len(),
        nestweaver_web::routes::brain::LIST_NOTES_DEFAULT_LIMIT,
        "omitted limit must use the documented default cap, not the whole vault"
    );
    assert!(
        arr.len() < CORPUS,
        "omitted limit must not dump the unbounded corpus"
    );
}

#[tokio::test]
async fn brain_notes_limit_0_returns_empty() {
    let app = notes_list_app(5);
    let (status, json) = get_json(&app, "/api/v1/brain/notes?limit=0").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json
        .as_array()
        .expect("sibling brain list routes return a raw array");
    assert!(
        arr.is_empty(),
        "?limit=0 must match /symbols/top and return an empty array, not clamp to 1"
    );
}

#[tokio::test]
async fn brain_notes_limit_2000_is_capped_at_1000() {
    let cap = nestweaver_web::routes::brain::LIST_NOTES_LIMIT_MAX;
    let app = notes_list_app(cap + 1);
    let (status, json) = get_json(&app, "/api/v1/brain/notes?limit=2000").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json
        .as_array()
        .expect("sibling brain list routes return a raw array");
    assert_eq!(
        arr.len(),
        cap,
        "?limit=2000 must be capped at LIST_NOTES_LIMIT_MAX"
    );
}

#[tokio::test]
async fn brain_notes_offset_pages_past_the_first_row() {
    let app = notes_list_app(3);
    let (status, first) = get_json(&app, "/api/v1/brain/notes?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    let (status, second) = get_json(&app, "/api/v1/brain/notes?limit=1&offset=1").await;
    assert_eq!(status, StatusCode::OK);
    let a = first.as_array().unwrap();
    let b = second.as_array().unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_ne!(a[0]["uid"], b[0]["uid"], "offset must skip the first row");
}

#[tokio::test]
async fn brain_notes_huge_offset_is_capped() {
    let app = notes_list_app(5);
    let (status, json) = get_json(&app, "/api/v1/brain/notes?limit=1&offset=999999999").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json
        .as_array()
        .expect("sibling brain list routes return a raw array");
    assert!(
        arr.is_empty(),
        "offset must be capped so a huge skip cannot scan the vault"
    );
}

#[tokio::test]
async fn brain_backlinks_omitted_limit_uses_notes_list_default() {
    const CORPUS: usize = 25;
    let app = backlinks_app(CORPUS);
    let (status, json) = get_json(&app, "/api/v1/brain/backlinks/note:notes:target").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let rows = json["backlinks"]
        .as_array()
        .expect("backlinks envelope must carry an array");
    assert_eq!(
        rows.len(),
        nestweaver_web::routes::brain::LIST_NOTES_DEFAULT_LIMIT,
        "omitted limit must default to 20: {json}"
    );
    assert_eq!(json["total"], CORPUS);
    assert_eq!(json["truncated"], true);
    assert_eq!(
        json["limit"],
        nestweaver_web::routes::brain::LIST_NOTES_DEFAULT_LIMIT
    );
    assert_eq!(json["count"], rows.len());
}

#[tokio::test]
async fn brain_backlinks_limit_1_returns_one_row() {
    let app = backlinks_app(5);
    let (status, json) = get_json(&app, "/api/v1/brain/backlinks/note:notes:target?limit=1").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["backlinks"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["count"], 1);
    assert_eq!(json["total"], 5);
    assert_eq!(json["truncated"], true);
}

#[tokio::test]
async fn brain_backlinks_limit_2000_is_capped_at_1000() {
    let cap = nestweaver_web::routes::brain::LIST_NOTES_LIMIT_MAX;
    let app = backlinks_app(cap + 1);
    let (status, json) =
        get_json(&app, "/api/v1/brain/backlinks/note:notes:target?limit=2000").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["backlinks"].as_array().map(Vec::len), Some(cap));
    assert_eq!(json["limit"], cap);
    assert_eq!(json["truncated"], true);
    assert_eq!(json["total"], cap + 1);
}

#[tokio::test]
async fn brain_note_not_found_returns_404() {
    let app = make_app();
    let (status, _) = get_json(&app, "/api/v1/brain/note/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn source_missing_file_returns_400() {
    let app = make_app();
    let (status, _) = get_json(&app, "/api/v1/source").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn paths_between_nonexistent_returns_empty() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/paths/a/b").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert!(arr.is_empty(), "should return empty paths array");
}

#[tokio::test]
async fn flow_nonexistent_returns_404() {
    let app = make_app();
    let (status, _) = get_json(&app, "/api/v1/flow/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn gaps_returns_report_structure() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/gaps").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        json.get("undocumented").is_some(),
        "should have undocumented"
    );
    assert!(json.get("untested").is_some(), "should have untested");
    assert!(
        json.get("disconnected_pairs").is_some(),
        "should have disconnected_pairs"
    );
}

#[tokio::test]
async fn gaps_second_call_returns_cached_result() {
    let app = make_app();

    // First call: cold
    let (status1, json1) = get_json(&app, "/api/v1/gaps").await;
    assert_eq!(status1, StatusCode::OK);

    // Second call: should return identical result (cached)
    let (status2, json2) = get_json(&app, "/api/v1/gaps").await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(json1, json2, "cached response should be identical");
}

#[test]
fn tested_service_uids_returns_only_tested() {
    let store = GraphStore::in_memory().unwrap();

    // Insert a repo
    let repo = Repo {
        uid: "repo:test".to_string(),
        url: "https://example.com/test.git".to_string(),
        indexed_sha: "abc123".to_string(),
        staleness_commits_behind: 0,
        instance_id: String::new(),
        name: None,
        root_path: None,
    };
    store.insert_repo(&repo).unwrap();

    // Two services
    let svc_tested = Service {
        uid: "svc:tested".to_string(),
        name: "TestedService".to_string(),
        repo_uid: "repo:test".to_string(),
        summary: None,
        summary_hash: None,
        embedding: None,
    };
    let svc_untested = Service {
        uid: "svc:untested".to_string(),
        name: "UntestedService".to_string(),
        repo_uid: "repo:test".to_string(),
        summary: None,
        summary_hash: None,
        embedding: None,
    };
    store.insert_service(&svc_tested).unwrap();
    store.insert_service(&svc_untested).unwrap();

    // Symbols belonging to each service
    let sym_a = Symbol {
        uid: "sym:a".to_string(),
        name: "handleRequest".to_string(),
        kind: SymbolKind::Function,
        repo_uid: "repo:test".to_string(),
        file_path: "src/service_a.ts".to_string(),
        start_line: 1,
        end_line: 10,
        signature: "fn handleRequest()".to_string(),
        summary: None,
        content_hash: "h1".to_string(),
        embedding: None,
        pagerank_score: None,
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    };
    let sym_b = Symbol {
        uid: "sym:b".to_string(),
        name: "processData".to_string(),
        kind: SymbolKind::Function,
        repo_uid: "repo:test".to_string(),
        file_path: "src/service_b.ts".to_string(),
        start_line: 1,
        end_line: 10,
        signature: "fn processData()".to_string(),
        summary: None,
        content_hash: "h2".to_string(),
        embedding: None,
        pagerank_score: None,
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    };
    store.insert_symbol(&sym_a).unwrap();
    store.insert_symbol(&sym_b).unwrap();
    store
        .insert_service_symbol_edge("svc:tested", "sym:a")
        .unwrap();
    store
        .insert_service_symbol_edge("svc:untested", "sym:b")
        .unwrap();

    // A test-file symbol that calls sym_a (making svc:tested "tested")
    let test_caller = Symbol {
        uid: "sym:test_caller".to_string(),
        name: "testHandleRequest".to_string(),
        kind: SymbolKind::Function,
        repo_uid: "repo:test".to_string(),
        file_path: "src/__tests__/service_a.test.ts".to_string(),
        start_line: 1,
        end_line: 5,
        signature: "fn testHandleRequest()".to_string(),
        summary: None,
        content_hash: "h3".to_string(),
        embedding: None,
        pagerank_score: None,
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    };
    store.insert_symbol(&test_caller).unwrap();
    store
        .insert_edge(&ResolvedEdge {
            source_uid: "sym:test_caller".to_string(),
            target_uid: "sym:a".to_string(),
            edge_type: EdgeType::Calls,
            confidence: 1.0,
            link_type: None,
            evidence: vec![],
        })
        .unwrap();

    let tested = store.tested_service_uids().unwrap();
    assert!(
        tested.contains("svc:tested"),
        "svc:tested should be in the tested set"
    );
    assert!(
        !tested.contains("svc:untested"),
        "svc:untested should NOT be in the tested set"
    );
    assert_eq!(tested.len(), 1);
}

fn make_app_with_tempdir() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");
    let store = setup_test_store();
    let state = AppState::new(store, None, db_path);
    (create_router(state), dir)
}

#[tokio::test]
async fn perspectives_list_empty() {
    let (app, _dir) = make_app_with_tempdir();
    let (status, json) = get_json(&app, "/api/v1/perspectives").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert!(arr.is_empty(), "should return empty array");
}

#[tokio::test]
async fn perspectives_create_and_list() {
    let (app, _dir) = make_app_with_tempdir();
    let (status, created) = post_json(
        &app,
        "/api/v1/perspectives",
        json!({ "name": "Arch View", "config": { "layout": "force" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["name"], "Arch View");
    assert!(created.get("id").is_some(), "should have an id");

    let (status, json) = get_json(&app, "/api/v1/perspectives").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Arch View");
}

#[tokio::test]
async fn canvases_list_empty() {
    let (app, _dir) = make_app_with_tempdir();
    let (status, json) = get_json(&app, "/api/v1/canvases").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert!(arr.is_empty(), "should return empty array");
}

#[tokio::test]
async fn canvases_create_and_get() {
    let (app, _dir) = make_app_with_tempdir();
    let (status, created) =
        post_json(&app, "/api/v1/canvases", json!({ "name": "My Canvas" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["name"], "My Canvas");
    let id = created["id"].as_str().expect("should have string id");

    let (status, fetched) = get_json(&app, &format!("/api/v1/canvases/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["name"], "My Canvas");
    assert_eq!(fetched["id"], id);
}

#[tokio::test]
async fn presentations_list_empty() {
    let (app, _dir) = make_app_with_tempdir();
    let (status, json) = get_json(&app, "/api/v1/presentations").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert!(arr.is_empty(), "should return empty array");
}

#[tokio::test]
async fn events_returns_sse_content_type() {
    let (app, _dir) = make_app_with_tempdir();
    let app = app.clone();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("should have content-type header")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/event-stream"),
        "content-type should contain text/event-stream, got: {content_type}"
    );
}

async fn post_raw(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, String, String) {
    let app = app.clone();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    (status, content_type, text)
}

#[tokio::test]
async fn llm_query_extracts_keywords() {
    let app = make_app();
    let (status, json) = post_json(
        &app,
        "/api/v1/llm/query",
        json!({ "query": "authentication service handler", "token_budget": 4000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("seeds").is_some(), "should have seeds");
    assert!(json.get("explanation").is_some(), "should have explanation");
    assert!(json.get("context").is_some(), "should have context");
    let seeds = json["seeds"].as_array().expect("seeds should be an array");
    assert!(!seeds.is_empty(), "should have extracted seeds");
}

#[tokio::test]
async fn llm_query_strips_punctuation_from_seeds() {
    // Regression: "what is main?" used to seed "main?" — which never resolves
    // — because trailing punctuation was kept. Seeds must be punctuation-free.
    let app = make_app();
    let (status, json) = post_json(
        &app,
        "/api/v1/llm/query",
        json!({ "query": "what is authentication?" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seeds: Vec<&str> = json["seeds"]
        .as_array()
        .expect("seeds should be an array")
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(
        seeds.contains(&"authentication"),
        "trailing '?' must be stripped from the seed, got: {seeds:?}"
    );
    assert!(
        seeds.iter().all(|s| !s.ends_with('?')),
        "no seed may keep trailing punctuation, got: {seeds:?}"
    );
}

#[tokio::test]
async fn llm_query_short_words_returns_400() {
    let app = make_app();
    let (status, _) = post_json(&app, "/api/v1/llm/query", json!({ "query": "a b c" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn timeline_returns_empty_array() {
    let app = make_app();
    let (status, json) = get_json(&app, "/api/v1/timeline/any").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().expect("response should be an array");
    assert!(arr.is_empty(), "should return empty array");
}

fn snapshot_body() -> Value {
    json!({
        "nodes": [{"uid": "a", "x": 0, "y": 0, "size": 10, "color": "#333", "label": "test"}],
        "edges": [],
        "width": 800,
        "height": 600,
        "background": "#fff",
        "legend": false
    })
}

#[tokio::test]
async fn export_svg_returns_svg() {
    let app = make_app();
    let (status, content_type, body) = post_raw(&app, "/api/v1/export/svg", snapshot_body()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.contains("svg"),
        "content-type should contain svg, got: {content_type}"
    );
    assert!(body.contains("<svg"), "body should contain SVG markup");
}

#[tokio::test]
async fn export_html_returns_html() {
    let app = make_app();
    let (status, content_type, body) = post_raw(&app, "/api/v1/export/html", snapshot_body()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.contains("html"),
        "content-type should contain html, got: {content_type}"
    );
    assert!(
        body.contains("<!DOCTYPE html>"),
        "body should contain HTML doctype"
    );
}
