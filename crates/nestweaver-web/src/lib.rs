pub mod bridge;
pub mod error;
pub mod gaps_cache;
pub mod hardening;
pub mod rank_events;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::{
    Router,
    extract::Request,
    http::{self, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, delete, get, post, put},
};
use rust_embed::RustEmbed;

use crate::state::{AdminState, AppState};

#[derive(RustEmbed, Clone)]
#[folder = "frontend/dist/"]
struct FrontendAssets;

fn mime_for_path(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("js") | Some("mjs") => "application/javascript",
        Some("css") => "text/css",
        Some("wasm") => "application/wasm",
        Some("html") => "text/html; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("json") => "application/json",
        Some("map") => "application/json",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}

fn has_file_extension(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|seg| seg.contains('.'))
}

async fn spa_fallback(request: Request) -> Response {
    let path = request.uri().path();
    let trimmed = path.trim_start_matches('/');

    if !trimmed.is_empty()
        && let Some(file) = FrontendAssets::get(trimmed)
    {
        return (
            StatusCode::OK,
            [(http::header::CONTENT_TYPE, mime_for_path(trimmed))],
            file.data,
        )
            .into_response();
    }

    // API typos must remain machine-readable HTTP failures. Returning the SPA
    // shell here turns an unknown endpoint into a misleading 200 response.
    if is_api_path(path) {
        return crate::error::ApiError::not_found(format!("no API route at {path}"))
            .into_response();
    }
    // The admin API exists only in server mode, on the server's own listener.
    // This UI answered it with the SPA's HTML and a 200.
    if path == "/admin/api" || path.starts_with("/admin/api/") {
        return crate::error::ApiError::not_found(
            "the admin API is served only by a daemon in server mode, on its own listener, \
             not by this UI",
        )
        .into_response();
    }

    // Paths with file extensions that weren't found should 404
    if has_file_extension(path) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // SPA fallback: serve index.html for navigation routes
    match FrontendAssets::get("index.html") {
        Some(file) => (
            StatusCode::OK,
            [(http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            file.data,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// Largest error body re-encoded as JSON; axum's rejection texts are short.
const MAX_REENCODED_ERROR_BYTES: usize = 64 * 1024;

/// Every API error is a JSON `{"error": ...}` body. axum's own extractor
/// rejections (a query string or JSON body that does not parse, a body over
/// the limit, a method not allowed) answer `text/plain`, or nothing; this
/// re-encodes them with their status unchanged. Responses that are already
/// JSON, successes, and non-API paths pass through untouched.
async fn json_api_errors(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let api = is_api_path(request.uri().path());
    let response = next.run(request).await;
    let status = response.status();
    if !api || !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let is_json = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if is_json {
        return response;
    }
    let (parts, body) = response.into_parts();
    let text = axum::body::to_bytes(body, MAX_REENCODED_ERROR_BYTES)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .unwrap_or_default();
    let message = if text.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_lowercase()
    } else {
        text
    };
    let mut rebuilt = (status, axum::Json(serde_json::json!({ "error": message }))).into_response();
    // Keep headers such as `Allow` on a 405; the body's type is ours now.
    for (name, value) in parts.headers.iter() {
        if name != http::header::CONTENT_TYPE && name != http::header::CONTENT_LENGTH {
            rebuilt.headers_mut().append(name.clone(), value.clone());
        }
    }
    rebuilt
}

/// Reset the daemon idle timer for every UI request (nw-749).
///
/// No-op when the router was built outside a daemon (`idle_activity` unset).
async fn note_http_activity(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(notify) = state.idle_activity.get() {
        notify.notify_one();
    }
    next.run(request).await
}

pub fn create_router(state: Arc<AppState>) -> Router {
    // Touch all lazy metric statics so the /metrics endpoint always reports
    // the full set of metric names, even before any events occur.
    routes::metrics::init_metrics();

    Router::new()
        .route("/api/v1/health", get(routes::health::health))
        .route("/api/v1/version", get(routes::version::version))
        .route("/api/v1/workspaces", get(routes::workspaces::workspaces))
        .route("/api/v1/overview", get(routes::overview::overview))
        .route("/api/v1/search", get(routes::symbols::search))
        .route("/api/v1/symbol/{uid}", get(routes::symbols::symbol_by_uid))
        .route(
            "/api/v1/symbols/file",
            get(routes::symbols::symbols_in_file),
        )
        .route("/api/v1/symbols/top", get(routes::symbols::symbols_top))
        .route(
            "/api/v1/context",
            post(routes::context::code_context).layer(axum::extract::DefaultBodyLimit::max(
                routes::context::CONTEXT_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/api/v1/brain/context",
            post(routes::context::brain_context).layer(axum::extract::DefaultBodyLimit::max(
                routes::context::CONTEXT_BODY_LIMIT_BYTES,
            )),
        )
        // Impact
        .route("/api/v1/impact/{uid}", get(routes::impact::impact))
        // Repos
        .route("/api/v1/repos", get(routes::repos::list_repos))
        .route("/api/v1/services", get(routes::repos::list_services))
        .route("/api/v1/repo-map", get(routes::repos::repo_map))
        .route(
            "/api/v1/cross-repo/{uid}",
            get(routes::repos::cross_repo_refs),
        )
        .route("/api/v1/suggest-links", get(routes::repos::suggest_links))
        // Brain
        .route("/api/v1/brain/status", get(routes::brain::brain_status))
        .route("/api/v1/brain/vaults", get(routes::brain::list_vaults))
        .route("/api/v1/brain/tags", get(routes::brain::list_tags))
        .route("/api/v1/brain/notes", get(routes::brain::list_notes))
        .route("/api/v1/brain/note/{uid}", get(routes::brain::note_by_uid))
        .route(
            "/api/v1/brain/backlinks/{uid}",
            get(routes::brain::backlinks),
        )
        .route(
            "/api/v1/brain/unlinked-mentions/{uid}",
            get(routes::brain::unlinked_mentions),
        )
        .route("/api/v1/brain/search", get(routes::brain::brain_search))
        // Source
        .route("/api/v1/source", get(routes::source::source))
        // Paths
        .route(
            "/api/v1/paths/{from}/{to}",
            get(routes::paths::paths_between),
        )
        // Flow
        .route("/api/v1/flow/{uid}", get(routes::flow::flow))
        // Gaps
        .route("/api/v1/gaps", get(routes::gaps::gaps))
        // Perspectives
        .route(
            "/api/v1/perspectives",
            get(routes::perspectives::list).post(routes::perspectives::create),
        )
        .route(
            "/api/v1/perspectives/{id}",
            put(routes::perspectives::update).delete(routes::perspectives::delete),
        )
        // Canvases
        .route(
            "/api/v1/canvases",
            get(routes::canvases::list).post(routes::canvases::create),
        )
        .route(
            "/api/v1/canvases/{id}",
            get(routes::canvases::get)
                .put(routes::canvases::update)
                .delete(routes::canvases::delete),
        )
        // Presentations
        .route(
            "/api/v1/presentations",
            get(routes::presentations::list).post(routes::presentations::create),
        )
        .route(
            "/api/v1/presentations/{id}",
            get(routes::presentations::get)
                .put(routes::presentations::update)
                .delete(routes::presentations::delete),
        )
        .route(
            "/api/v1/presentations/{id}/export",
            post(routes::presentations::export_html),
        )
        // LLM
        .route("/api/v1/llm/query", post(routes::llm::query))
        // Timeline
        .route(
            "/api/v1/timeline/{repo_uid}",
            get(routes::timeline::timeline),
        )
        // Export
        .route("/api/v1/export/svg", post(routes::export::export_svg))
        .route("/api/v1/export/png", post(routes::export::export_png))
        .route("/api/v1/export/html", post(routes::export::export_html))
        // Metrics (Prometheus text format, no auth)
        .route("/metrics", get(routes::metrics::metrics_handler))
        // Snapshot
        .route(
            "/api/v1/snapshot.msgpack",
            get(routes::snapshot::snapshot_msgpack),
        )
        // Events (SSE)
        .route("/api/v1/events", get(routes::events::events))
        .fallback(get(spa_fallback))
        .layer(axum::middleware::from_fn(json_api_errors))
        // nw-749. Outermost on the UI router so every request, including
        // static assets and API polls, resets the daemon idle timer. The
        // admin router is a separate Router and is intentionally not wrapped:
        // server mode forces the idle timeout off.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            note_http_activity,
        ))
        // No CORS layer: the SPA is served same-origin (serve_ui) in production
        // and via Vite's same-origin dev proxy in development, so no
        // cross-origin access is needed. Sending `Access-Control-Allow-Origin: *`
        // here would let any website `fetch()` these UNAUTHENTICATED `/api/v1/*`
        // read endpoints on the victim's loopback daemon and exfiltrate their
        // indexed code cross-origin (localhost-CORS leak). Same-origin policy
        // blocks that when no ACAO header is present.
        .with_state(state)
}

/// Creates the admin API router for server-mode deployments.
/// All routes require admin token authentication via the `AdminAuth` extractor.
pub fn create_admin_router(state: Arc<AdminState>) -> Router {
    use routes::admin;

    Router::new()
        .route("/repos", get(admin::list_repos).post(admin::add_repo))
        .route("/repos/{id}", delete(admin::remove_repo))
        .route("/repos/{id}/reindex", post(admin::trigger_reindex))
        .route("/queue", get(admin::get_queue))
        .route("/drain", post(admin::drain))
        .route("/resume", post(admin::resume))
        .route("/drain/status", get(admin::drain_status))
        .route("/dead-letter", get(admin::list_dead_letter))
        .route("/dead-letter/{id}/retry", post(admin::retry_dead_letter))
        .route("/dead-letter/{id}", delete(admin::dismiss_dead_letter))
        .route("/reload", post(admin::reload_config))
        .route("/status", get(admin::get_status))
        // Expose /metrics on the admin port as well so Prometheus can scrape
        // a single endpoint regardless of which port it targets. Gated behind
        // the admin token (S.5) — the admin router is nested onto the
        // network-facing MCP listener, so an unauthenticated /admin/api/metrics
        // would leak operational counters.
        .route("/metrics", get(admin::metrics))
        // Nested under the UI router in server mode, this router would
        // otherwise inherit the UI's fallback, which says the admin API is
        // not served here at all.
        .fallback(|request: Request| async move {
            crate::error::ApiError::not_found(format!(
                "unknown admin API route: {}",
                request.uri().path()
            ))
            .into_response()
        })
        .with_state(state)
}

/// Creates the device-flow auth router (OAuth 2.0 Device Authorization Grant,
/// RFC 8628). Mounted at `/auth` on the MCP/admin HTTP listener.
///
/// `/device` and `/token` are public (developers without a token reach them);
/// `/device/approve` is guarded by the `AdminAuth` extractor inside the handler.
pub fn create_device_flow_router(state: Arc<AdminState>) -> Router {
    use axum::extract::{DefaultBodyLimit, Request};
    use axum::middleware::{Next, from_fn};
    use routes::admin;

    // Shared, bounded per-IP rate limiter for the two public endpoints. These
    // are unauthenticated and the MCP limiter lives inside the /mcp handler, so
    // without this /auth would have no throttle at all.
    let limiter = Arc::new(admin::AuthRateLimiter::new(
        admin::AUTH_RATE_PER_MIN,
        admin::AUTH_RATE_MAX_KEYS,
    ));

    // Rate limit only the unauthenticated endpoints; /device/approve is admin
    // authenticated and IP-throttling it could lock an operator out.
    let public = Router::new()
        .route("/device", post(admin::device_authorize))
        .route("/token", post(admin::device_token))
        .layer(from_fn(move |req: Request, next: Next| {
            let limiter = limiter.clone();
            admin::auth_rate_limit(limiter, req, next)
        }));

    Router::new()
        .merge(public)
        .route("/device/approve", post(admin::device_approve))
        // Cap request bodies on the whole /auth router so an unauthenticated
        // caller can't stream a large body before any handler runs.
        .layer(DefaultBodyLimit::max(admin::AUTH_BODY_LIMIT_BYTES))
        .with_state(state)
}

pub async fn start_server(
    state: Arc<AppState>,
    port: u16,
    open_browser: bool,
) -> anyhow::Result<()> {
    let app = create_router(state);
    start_server_with_router(app, port, open_browser).await
}

/// Degraded-mode page served while the daemon that owns the graph is down.
/// The UI process keeps its port bound across a daemon outage and answers
/// every route with this, so a browser or the menubar app sees the outage
/// instead of a dead socket.
const DEGRADED_PAGE: &str = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>NestWeaver — daemon unavailable</title>\n</head>\n<body>\n<h1>The NestWeaver daemon is down</h1>\n<p>The graph daemon is unreachable, so the UI cannot serve graph data right now. The UI is still running and resumes full service automatically once the daemon returns — no restart needed.</p>\n</body>\n</html>\n";

async fn degraded_response(request: Request) -> Response {
    let path = request.uri().path();
    // API routes stay machine-readable in degraded mode too.
    if path == "/api" || path.starts_with("/api/") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(http::header::CONTENT_TYPE, "application/json")],
            r#"{"error":"daemon_unavailable","message":"The NestWeaver daemon is down; the UI is serving a degraded page and resumes full service once the daemon returns."}"#,
        )
            .into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DEGRADED_PAGE,
    )
        .into_response()
}

/// Router served while the daemon is unreachable: every route, on every
/// method, reports the outage. The status is `503`, not `200`, so health
/// checks and supervisors see the truth; browsers still render the page
/// body. `any` (not `get`) so the real UI's POST routes (/api/v1/context,
/// /api/v1/llm/query, exports) get the same 503 instead of a bare 405.
pub fn degraded_router() -> Router {
    Router::new().fallback(any(degraded_response))
}

/// Serve [`degraded_router`] on `port` until `shutdown` resolves. Returning
/// `Ok` therefore always means a requested shutdown; a bind or serve failure
/// is the `Err` case and carries the underlying cause.
/// Deliberately unhardened (nw-682): every response is a static 503 with no
/// graph data, so there is nothing here for a DNS-rebinding page to read.
pub async fn start_degraded_server(
    port: u16,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::warn!("nestweaver-web serving degraded daemon-down page on http://{addr}");
    axum::serve(listener, degraded_router().into_make_service())
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Start the web UI server with a pre-built router.
///
/// This allows callers (e.g. the daemon's `serve_ui` RPC) to customise the
/// router — for instance by nesting the admin API — before starting.
pub async fn start_server_with_router(
    app: Router,
    port: u16,
    open_browser: bool,
) -> anyhow::Result<()> {
    let app = crate::hardening::harden(app);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("nestweaver-web listening on http://{addr}");

    if open_browser {
        let url = format!("http://{addr}");
        if let Err(e) = open::that(&url) {
            tracing::warn!(error = %e, "failed to open browser");
        }
    }

    // Serve with peer-address info so IP-keyed middleware (e.g. the device-flow
    // rate limiter on the nested /auth router) can read the direct client IP via
    // `ConnectInfo`. Purely additive for routers that don't use it.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod frontend_assets_tests {
    use super::*;

    /// Every asset referenced by the embedded `index.html` must itself be
    /// embedded. `rust_embed` pulls from `frontend/dist/` on disk at build time,
    /// so on a clean git checkout this fails if a rebuilt, content-hashed bundle
    /// was not git-tracked — the release-breaking "index.html points at a missing
    /// /assets/* file" class. The `dist/` folder is gitignored and force-added,
    /// so a forgotten `git add -f` after a `vite build` is the exact failure mode.
    #[test]
    fn embedded_index_references_only_embedded_assets() {
        let index = FrontendAssets::get("index.html").expect("index.html must be embedded");
        let html = std::str::from_utf8(index.data.as_ref()).expect("index.html is utf8");

        let mut checked = 0;
        let mut rest = html;
        while let Some(pos) = rest.find("/assets/") {
            // Drop the leading '/', keep the embed-relative "assets/<file>".
            let tail = &rest[pos + 1..];
            let end = tail
                .find(|c: char| c == '"' || c == '\'' || c == ')' || c == '?' || c.is_whitespace())
                .unwrap_or(tail.len());
            let asset_path = &tail[..end];
            assert!(
                FrontendAssets::get(asset_path).is_some(),
                "index.html references /{asset_path} but it is not embedded \
                 (rebuilt frontend bundle not git-tracked?)"
            );
            checked += 1;
            rest = &tail[end..];
        }
        assert!(
            checked > 0,
            "expected index.html to reference at least one /assets/* file"
        );
    }

    /// The JSON re-encoding keeps every value of a multi-valued header: an
    /// error carrying two `Allow` (or `Vary`) lines keeps both.
    #[tokio::test]
    async fn json_api_errors_keeps_every_value_of_a_repeated_header() {
        use tower::ServiceExt;
        let app = Router::new()
            .route(
                "/api/x",
                get(|| async {
                    let mut response =
                        (StatusCode::METHOD_NOT_ALLOWED, "not allowed").into_response();
                    let headers = response.headers_mut();
                    headers.append(http::header::ALLOW, "GET".parse().unwrap());
                    headers.append(http::header::ALLOW, "HEAD".parse().unwrap());
                    response
                }),
            )
            .layer(axum::middleware::from_fn(json_api_errors));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/x")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let allow: Vec<&str> = response
            .headers()
            .get_all(http::header::ALLOW)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(allow, ["GET", "HEAD"]);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[tokio::test]
    async fn unknown_api_path_does_not_fall_back_to_spa() {
        let request = Request::builder()
            .uri("/api/v1/does-not-exist")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = spa_fallback(request).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// nw-749. A UI poll has to notify the idle handle the daemon installed.
    #[tokio::test]
    async fn http_request_notifies_idle_activity() {
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("idle.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db).unwrap();
        let state = AppState::new(store, None, db);
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        state
            .idle_activity
            .set(std::sync::Arc::clone(&notify))
            .unwrap();
        let app = Router::new()
            .route("/api/v1/health", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                std::sync::Arc::clone(&state),
                note_http_activity,
            ))
            .with_state(state);
        let notified = notify.notified();
        app.oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .expect("an HTTP request must reset the idle timer");
    }
}

#[cfg(test)]
mod degraded_tests {
    use super::*;

    /// The degraded page must plainly name the daemon outage and answer 503,
    /// so a port probe never mistakes it for a healthy UI.
    #[tokio::test]
    async fn degraded_page_names_the_daemon_outage() {
        let request = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = degraded_response(request).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("daemon is down"),
            "degraded page must say the daemon is down: {text}"
        );
    }

    /// API consumers get a machine-readable 503, not an HTML page.
    #[tokio::test]
    async fn degraded_api_response_is_machine_readable() {
        let request = Request::builder()
            .uri("/api/v1/health")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = degraded_response(request).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("daemon_unavailable"), "body: {text}");
    }

    /// The real UI has POST routes (/api/v1/context, /api/v1/llm/query,
    /// exports); in degraded mode those must get the same 503 outage
    /// response, not a bare 405 from a GET-only fallback.
    #[tokio::test]
    async fn degraded_router_answers_non_get_methods() {
        use tower::ServiceExt;

        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/context")
            .body(axum::body::Body::from("{}"))
            .unwrap();

        let response = degraded_router().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("daemon_unavailable"), "body: {text}");
    }
}
