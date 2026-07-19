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
