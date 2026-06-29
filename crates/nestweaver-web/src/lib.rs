pub mod error;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::{
    Router,
    extract::Request,
    http::{self, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use rust_embed::RustEmbed;
use tower_http::cors::CorsLayer;

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

pub fn create_router(state: Arc<AppState>) -> Router {
    // Touch all lazy metric statics so the /metrics endpoint always reports
    // the full set of metric names, even before any events occur.
    routes::metrics::init_metrics();

    Router::new()
        .route("/api/v1/health", get(routes::health::health))
        .route("/api/v1/version", get(routes::version::version))
        .route("/api/v1/overview", get(routes::overview::overview))
        .route("/api/v1/search", get(routes::symbols::search))
        .route("/api/v1/symbol/{uid}", get(routes::symbols::symbol_by_uid))
        .route(
            "/api/v1/symbols/file",
            get(routes::symbols::symbols_in_file),
        )
        .route("/api/v1/symbols/top", get(routes::symbols::symbols_top))
        .route("/api/v1/context", post(routes::context::code_context))
        .route(
            "/api/v1/brain/context",
            post(routes::context::brain_context),
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
        .layer(CorsLayer::permissive())
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
        // a single endpoint regardless of which port it targets.
        .route("/metrics", get(routes::metrics::metrics_handler))
        .with_state(state)
}

/// Creates the device-flow auth router (OAuth 2.0 Device Authorization Grant,
/// RFC 8628). Mounted at `/auth` on the MCP/admin HTTP listener.
///
/// `/device` and `/token` are public (developers without a token reach them);
/// `/device/approve` is guarded by the `AdminAuth` extractor inside the handler.
pub fn create_device_flow_router(state: Arc<AdminState>) -> Router {
    use routes::admin;

    Router::new()
        .route("/device", post(admin::device_authorize))
        .route("/token", post(admin::device_token))
        .route("/device/approve", post(admin::device_approve))
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

/// Start the web UI server with a pre-built router.
///
/// This allows callers (e.g. the daemon's `serve_ui` RPC) to customise the
/// router — for instance by nesting the admin API — before starting.
pub async fn start_server_with_router(
    app: Router,
    port: u16,
    open_browser: bool,
) -> anyhow::Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("nestweaver-web listening on http://{addr}");

    if open_browser {
        let url = format!("http://{addr}");
        if let Err(e) = open::that(&url) {
            tracing::warn!(error = %e, "failed to open browser");
        }
    }

    axum::serve(listener, app).await?;
    Ok(())
}
