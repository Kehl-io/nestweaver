use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub async fn list_repos(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let repos = nestweaver_engine::list_repos(&state.store, None)?;
    let json = serde_json::to_value(&repos)?;
    Ok(Json(json).into_response())
}

pub async fn list_services(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let services = nestweaver_engine::list_services(&state.store, None)?;
    let json = serde_json::to_value(&services)?;
    Ok(Json(json).into_response())
}

#[derive(Deserialize)]
pub struct RepoMapParams {
    pub budget: Option<usize>,
}

pub async fn repo_map(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RepoMapParams>,
) -> Result<Response, ApiError> {
    let budget = params.budget.unwrap_or(2000);
    let map = nestweaver_engine::generate_repo_map(&state.store, budget)?;
    Ok(Json(json!({ "map": map })).into_response())
}

pub async fn cross_repo_refs(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
) -> Result<Response, ApiError> {
    let refs = state.store.cross_repo_links(&uid)?;
    let json = serde_json::to_value(&refs)?;
    Ok(Json(json).into_response())
}

pub async fn suggest_links(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    // Build manifest cache path: <db_path>.manifests.json
    let manifest_path = nestweaver_engine::sidecar_path(&state.db_path, ".manifests.json");

    let manifests = nestweaver_engine::load_manifest_cache(&manifest_path)?;
    let suggestions = nestweaver_engine::suggest_links(&state.store, &manifests)?;
    let json = json!({
        "links": serde_json::to_value(&suggestions.links)?,
        "features": serde_json::to_value(&suggestions.features)?,
    });
    Ok(Json(json).into_response())
}
