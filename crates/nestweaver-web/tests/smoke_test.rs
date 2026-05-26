use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use nestweaver_store::GraphStore;
use nestweaver_web::create_router;
use nestweaver_web::state::AppState;
use tower::ServiceExt;

async fn check(app: &axum::Router, method: Method, uri: &str, body: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req_body = body
        .map(|b| Body::from(b.to_string()))
        .unwrap_or(Body::empty());
    app.clone()
        .oneshot(builder.body(req_body).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn all_endpoints_respond() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("smoke.lbug");
    let store = GraphStore::open_or_create(&db_path).unwrap();
    let state = AppState::new(store, None, db_path);
    let app = create_router(state);

    // ── Health ────────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/health", None).await,
        StatusCode::OK
    );

    // ── Search ───────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/search?q=test&limit=10", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/search?q=&limit=10", None).await,
        StatusCode::BAD_REQUEST
    );

    // ── Symbol ───────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/symbol/none", None).await,
        StatusCode::NOT_FOUND
    );

    // ── Symbols in file ──────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/symbols/file?path=test.js", None).await,
        StatusCode::OK
    );

    // ── Symbols top ──────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/symbols/top?limit=5", None).await,
        StatusCode::OK
    );

    // ── Context ──────────────────────────────────────────────────────────────
    // With unknown seeds on an empty store, build_context bails -> 500
    assert_eq!(
        check(
            &app,
            Method::POST,
            "/api/v1/context",
            Some(r#"{"seeds":["x"]}"#)
        )
        .await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        check(
            &app,
            Method::POST,
            "/api/v1/context",
            Some(r#"{"seeds":[]}"#)
        )
        .await,
        StatusCode::BAD_REQUEST
    );

    // ── Brain context ────────────────────────────────────────────────────────
    // With unknown seeds on an empty store, build_brain_context_hybrid bails -> 500
    assert_eq!(
        check(
            &app,
            Method::POST,
            "/api/v1/brain/context",
            Some(r#"{"seeds":["x"]}"#)
        )
        .await,
        StatusCode::INTERNAL_SERVER_ERROR
    );

    // ── Impact ───────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/impact/none", None).await,
        StatusCode::NOT_FOUND
    );

    // ── Repos / services ─────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/repos", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/services", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/repo-map?budget=100", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/suggest-links", None).await,
        StatusCode::OK
    );

    // ── Cross-repo refs ──────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/cross-repo/none", None).await,
        StatusCode::OK
    );

    // ── Brain ────────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/brain/status", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/brain/vaults", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/brain/tags", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/brain/note/none", None).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        check(
            &app,
            Method::GET,
            "/api/v1/brain/search?q=test&limit=5",
            None
        )
        .await,
        StatusCode::OK
    );

    // ── Brain backlinks ──────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/brain/backlinks/none", None).await,
        StatusCode::OK
    );

    // ── Brain unlinked mentions ──────────────────────────────────────────────
    assert_eq!(
        check(
            &app,
            Method::GET,
            "/api/v1/brain/unlinked-mentions/none",
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );

    // ── Source ────────────────────────────────────────────────────────────────
    assert_eq!(
        check(
            &app,
            Method::GET,
            "/api/v1/source?file=test.js&line=1&context=5",
            None
        )
        .await,
        StatusCode::OK
    );

    // ── Paths ────────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/paths/a/b", None).await,
        StatusCode::OK
    );

    // ── Flow ─────────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/flow/none", None).await,
        StatusCode::NOT_FOUND
    );

    // ── Gaps ─────────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/gaps", None).await,
        StatusCode::OK
    );

    // ── Perspectives ─────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/perspectives", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(
            &app,
            Method::PUT,
            "/api/v1/perspectives/nonexistent",
            Some(r#"{"name":"x","config":{}}"#)
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        check(
            &app,
            Method::DELETE,
            "/api/v1/perspectives/nonexistent",
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );

    // ── Canvases ─────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/canvases", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/canvases/nonexistent", None).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        check(&app, Method::DELETE, "/api/v1/canvases/nonexistent", None).await,
        StatusCode::NOT_FOUND
    );

    // ── Presentations ────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/presentations", None).await,
        StatusCode::OK
    );
    assert_eq!(
        check(&app, Method::GET, "/api/v1/presentations/nonexistent", None).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        check(
            &app,
            Method::DELETE,
            "/api/v1/presentations/nonexistent",
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        check(
            &app,
            Method::POST,
            "/api/v1/presentations/nonexistent/export",
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );

    // ── Timeline ─────────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/timeline/none", None).await,
        StatusCode::OK
    );

    // ── LLM query ────────────────────────────────────────────────────────────
    assert_eq!(
        check(
            &app,
            Method::POST,
            "/api/v1/llm/query",
            Some(r#"{"query":"authentication service"}"#)
        )
        .await,
        StatusCode::OK
    );

    // ── Export SVG ────────────────────────────────────────────────────────────
    let snap =
        r##"{"nodes":[],"edges":[],"width":100,"height":100,"background":"#fff","legend":false}"##;
    assert_eq!(
        check(&app, Method::POST, "/api/v1/export/svg", Some(snap)).await,
        StatusCode::OK
    );

    // ── Export HTML ───────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::POST, "/api/v1/export/html", Some(snap)).await,
        StatusCode::OK
    );

    // ── Export PNG (not implemented) ──────────────────────────────────────────
    assert_eq!(
        check(&app, Method::POST, "/api/v1/export/png", Some(snap)).await,
        StatusCode::NOT_IMPLEMENTED
    );

    // ── SSE events ───────────────────────────────────────────────────────────
    assert_eq!(
        check(&app, Method::GET, "/api/v1/events", None).await,
        StatusCode::OK
    );
}
