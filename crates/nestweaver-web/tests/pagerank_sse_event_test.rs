use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use nestweaver_schema::{EdgeType, Repo, ResolvedEdge, Symbol, SymbolKind, Visibility};
use nestweaver_store::GraphStore;
use nestweaver_web::create_router;
use nestweaver_web::state::AppState;
use serde_json::Value;
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

fn repo(uid: &str, name: &str) -> Repo {
    Repo {
        uid: uid.to_string(),
        url: format!("https://example.com/{name}.git"),
        indexed_sha: format!("{name}-sha"),
        staleness_commits_behind: 0,
        instance_id: "local".to_string(),
        name: Some(name.to_string()),
        root_path: Some(format!("/tmp/{name}")),
    }
}

fn symbol(uid: &str, repo_uid: &str, name: &str, file_path: &str, line: u32) -> Symbol {
    Symbol {
        uid: uid.to_string(),
        name: name.to_string(),
        kind: SymbolKind::Function,
        repo_uid: repo_uid.to_string(),
        file_path: file_path.to_string(),
        start_line: line,
        end_line: line + 2,
        signature: format!("fn {name}()"),
        summary: None,
        content_hash: format!("{uid}-hash"),
        embedding: None,
        // Intentionally None: the lazy PageRank compute keys off the empty
        // in-memory cache, not this stored field. The fixture must NOT call
        // `compute_pagerank`, so the first ranking query fires the lazy compute
        // and bumps the generation — which is what the SSE producer observes.
        pagerank_score: None,
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    }
}

fn edge(source_uid: &str, target_uid: &str) -> ResolvedEdge {
    ResolvedEdge {
        source_uid: source_uid.to_string(),
        target_uid: target_uid.to_string(),
        edge_type: EdgeType::Calls,
        confidence: 0.9,
        link_type: None,
        evidence: Vec::new(),
    }
}

/// Build a router plus the shared state, with a store that has NOT had
/// PageRank computed — so the first ranking query triggers the lazy compute.
fn make_app_with_state() -> (axum::Router, Arc<AppState>) {
    let store = GraphStore::in_memory().unwrap();
    let r = repo("repo:rank", "rank");
    store.insert_repo(&r).unwrap();
    store
        .insert_symbol(&symbol("sym:rank:a", &r.uid, "alpha", "src/a.rs", 10))
        .unwrap();
    store
        .insert_symbol(&symbol("sym:rank:b", &r.uid, "beta", "src/b.rs", 20))
        .unwrap();
    store
        .insert_edge(&edge("sym:rank:a", "sym:rank:b"))
        .unwrap();

    // NOTE: deliberately no `store.compute_pagerank(...)` here.
    let state = AppState::new(
        store,
        None,
        std::path::PathBuf::from("/tmp/pagerank-sse-test.lbug"),
    );
    let router = create_router(state.clone());
    (router, state)
}

#[tokio::test]
async fn rank_triggering_request_emits_pagerank_recomputed_event() {
    let (app, state) = make_app_with_state();
    let mut rx = state.event_tx.subscribe();

    let (status, _json) = get_json(&app, "/api/v1/symbols/top?limit=5").await;
    assert_eq!(status, StatusCode::OK);

    let evt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("event within 5s")
        .expect("channel open");
    assert_eq!(evt.event_type, "pagerank:recomputed");
}

/// nw-029 T8: HTTP-level single-flight proof. Two concurrent rank-triggering
/// requests against a cold-cache store must SHARE one PageRank compute. The
/// store-level single-flight (T1) composes with the `spawn_blocking`-backed
/// handlers (T4/T5) so that `pagerank_generation` — the per-compute counter —
/// bumps exactly once (0 → 1), never twice.
///
/// `/api/v1/symbols/top` and `/api/v1/overview` both funnel into
/// `symbols_by_pagerank` → `ensure_pagerank_loaded`, which takes the compute
/// lock only when the cache is empty. A multi-thread runtime (≥2 workers) is
/// REQUIRED: on a single-threaded runtime the two `spawn_blocking` closures
/// would serialize, and the assertion would pass vacuously.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_rank_requests_share_one_compute() {
    let (app, state) = make_app_with_state();
    // Freshly built in-memory store starts at generation 0 (no compute yet).
    let initial = state.store.pagerank_generation();
    assert_eq!(initial, 0, "cold fixture must start at generation 0");

    // Two concurrent rank-triggering requests. `oneshot` consumes the service,
    // so `get_json` clones the router per call (Router: Clone).
    let (a, b) = tokio::join!(
        get_json(&app, "/api/v1/symbols/top?limit=5"),
        get_json(&app, "/api/v1/overview"),
    );
    assert_eq!(a.0, StatusCode::OK, "symbols/top must succeed: {a:?}");
    assert_eq!(b.0, StatusCode::OK, "overview must succeed: {b:?}");

    assert_eq!(
        state.store.pagerank_generation(),
        initial + 1,
        "two concurrent rank-triggering requests must share ONE compute"
    );
}

#[tokio::test]
async fn event_generation_snapshot_catches_missed_commits_and_queues_after_subscription() {
    use futures::StreamExt;
    use nestweaver_web::state::GraphEvent;

    let (app, state) = make_app_with_state();
    let before = state.store.graph_generation().to_string();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The handler has returned, but its first frame has not been consumed yet.
    state.store.try_bump_graph_generation().unwrap();
    let published = state.store.graph_generation().to_string();
    state
        .event_tx
        .send(GraphEvent {
            event_type: "graph:updated".into(),
            payload: serde_json::json!({}),
        })
        .unwrap_or_else(|_| panic!("subscriber must retain publication"));
    let mut stream = response.into_body().into_data_stream();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("initial generation frame within 5s")
        .unwrap()
        .unwrap();
    let text = String::from_utf8(frame.to_vec()).unwrap();
    let parse = |text: &str| -> (String, Value) {
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name).map(str::trim))
                .unwrap()
                .to_owned()
        };
        (
            field("event:"),
            serde_json::from_str(&field("data:")).unwrap(),
        )
    };
    let (event, snapshot) = parse(&text);
    assert_eq!(event, "graph:generation");
    assert_eq!(snapshot["graph_generation"].as_str(), Some(before.as_str()));
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("queued publication within 5s")
        .unwrap()
        .unwrap();
    let (event, queued) = parse(std::str::from_utf8(&frame).unwrap());
    assert_eq!(event, "graph:updated");
    assert_eq!(
        queued["graph_generation"].as_str(),
        Some(published.as_str())
    );
    drop(stream);

    // Both publications happen while disconnected. Reconnecting must disclose
    // them immediately; no future publication is needed to wake the client.
    state.store.try_bump_graph_generation().unwrap();
    let (status, _) = get_json(&app, "/api/v1/symbols/top?limit=5").await;
    assert_eq!(status, StatusCode::OK);
    let current_graph = state.store.graph_generation().to_string();
    let current_ranks = state.store.pagerank_generation().to_string();
    assert_ne!(current_ranks, "0");
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut stream = response.into_body().into_data_stream();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("reconnect snapshot within 5s")
            .unwrap()
            .unwrap();
        let (event, snapshot) = parse(std::str::from_utf8(&frame).unwrap());
        assert_eq!(event, "graph:generation");
        assert_eq!(
            snapshot["graph_generation"].as_str(),
            Some(current_graph.as_str())
        );
        assert_eq!(
            snapshot["pagerank_generation"].as_str(),
            Some(current_ranks.as_str())
        );
    }
}

#[tokio::test]
async fn completed_rank_recompute_notifies_even_when_presentation_fails() {
    let (_, state) = make_app_with_state();
    let mut events = state.event_tx.subscribe();
    let before = state.store.pagerank_generation();
    let store = Arc::clone(&state.store);
    let result: Result<(), nestweaver_web::error::ApiError> =
        nestweaver_web::rank_events::with_rank_event(&state, move || {
            store
                .compute_pagerank(0.85, 20, &nestweaver_store::GraphScope::code_only())
                .map_err(|error| {
                    nestweaver_web::error::ApiError::from_context_read(error.into())
                })?;
            Err(nestweaver_web::error::ApiError::internal(
                "presentation failed after successful ranking",
            ))
        })
        .await;
    let error = result.expect_err("presentation error is preserved");
    assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        error.message,
        "presentation failed after successful ranking"
    );
    assert_eq!(state.store.pagerank_generation(), before + 1);
    // Emission happens before the helper returns, so no timing-based negative
    // assertion or listener timeout is necessary.
    assert_eq!(
        events
            .try_recv()
            .expect("completed recompute must be observable")
            .event_type,
        "pagerank:recomputed"
    );
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn presentation_failure_without_rank_change_emits_no_rank_event() {
    let (_, state) = make_app_with_state();
    let mut events = state.event_tx.subscribe();
    let before = state.store.pagerank_generation();
    let result: Result<(), nestweaver_web::error::ApiError> =
        nestweaver_web::rank_events::with_rank_event(&state, || {
            Err(nestweaver_web::error::ApiError::internal(
                "presentation failed before ranking",
            ))
        })
        .await;
    assert_eq!(
        result.err().unwrap().status,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(state.store.pagerank_generation(), before);
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}
