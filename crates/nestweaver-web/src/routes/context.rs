use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ContextRequest {
    pub seeds: Vec<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    50
}

pub async fn code_context(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ContextRequest>,
) -> Result<Response, ApiError> {
    if body.seeds.is_empty() {
        return Err(ApiError::bad_request("seeds must not be empty"));
    }
    let result = nestweaver_engine::build_context(&state.store, &body.seeds)?;
    let json = serde_json::to_value(&result)?;
    Ok(Json(json).into_response())
}

#[derive(Deserialize)]
pub struct BrainContextRequest {
    pub seeds: Vec<String>,
    #[serde(default)]
    pub token_budget: Option<usize>,
    #[serde(default)]
    pub scope: Option<String>,
}

pub async fn brain_context(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BrainContextRequest>,
) -> Result<Response, ApiError> {
    if body.seeds.is_empty() {
        return Err(ApiError::bad_request("seeds must not be empty"));
    }
    let config = nestweaver_engine::HybridSearchConfig::default();
    let result = nestweaver_engine::build_brain_context_hybrid(
        &state.store,
        &body.seeds,
        state.tantivy.as_deref(),
        &config,
        None,
        None,
    )?;
    let json = serde_json::to_value(&result)?;
    Ok(Json(json).into_response())
}
