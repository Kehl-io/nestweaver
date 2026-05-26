pub mod error;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post, put},
};
use axum_embed::{FallbackBehavior, ServeEmbed};
use rust_embed::RustEmbed;
use tower_http::cors::CorsLayer;

use crate::state::AppState;

#[derive(RustEmbed, Clone)]
#[folder = "frontend/dist/"]
struct FrontendAssets;

pub fn create_router(state: Arc<AppState>) -> Router {
    let static_handler = ServeEmbed::<FrontendAssets>::with_parameters(
        Some("index.html".to_string()),
        FallbackBehavior::Ok,
        Some("index.html".to_string()),
    );

    Router::new()
        .route("/api/v1/health", get(routes::health::health))
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
        // Events (SSE)
        .route("/api/v1/events", get(routes::events::events))
        .fallback_service(static_handler)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

pub async fn start_server(
    state: Arc<AppState>,
    port: u16,
    open_browser: bool,
) -> anyhow::Result<()> {
    let app = create_router(state);
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
