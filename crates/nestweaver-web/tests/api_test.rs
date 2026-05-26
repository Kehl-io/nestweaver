use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use nestweaver_schema::{Repo, Symbol, SymbolKind, Visibility};
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
        url: "https://example.com/test".to_string(),
        indexed_sha: "abc123".to_string(),
        staleness_commits_behind: 0,
        instance_id: String::new(),
    };
    store.insert_repo(&repo).unwrap();

    let symbol = Symbol {
        uid: "sym:test:greet".to_string(),
        name: "greet".to_string(),
        kind: SymbolKind::Function,
        repo_uid: "repo:test".to_string(),
        file_path: "src/main.js".to_string(),
        start_line: 1,
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
