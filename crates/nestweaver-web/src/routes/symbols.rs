use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::ApiError;
use crate::rank_events::with_rank_event;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Result<Response, ApiError> {
    let q = params.q.unwrap_or_default();
    if q.is_empty() {
        return Err(ApiError::bad_request("query parameter 'q' is required"));
    }
    let limit = params.limit.unwrap_or(20).min(1000);
    let results = nestweaver_engine::search_symbols(&state.store, &q, limit)?;
    let json = serde_json::to_value(&results)?;
    Ok(Json(json).into_response())
}

pub async fn symbol_by_uid(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
) -> Result<Response, ApiError> {
    match nestweaver_engine::lookup_symbol(&state.store, &uid, None)? {
        nestweaver_engine::LookupResult::Found(detail) => {
            let json = serde_json::to_value(&*detail)?;
            Ok(Json(json).into_response())
        }
        nestweaver_engine::LookupResult::NotFound => {
            Err(ApiError::not_found(format!("symbol '{uid}' not found")))
        }
        nestweaver_engine::LookupResult::Ambiguous(candidates) => {
            let json = serde_json::to_value(&candidates)?;
            Ok((StatusCode::MULTIPLE_CHOICES, Json(json)).into_response())
        }
    }
}

#[derive(Deserialize)]
pub struct FileParams {
    pub path: String,
}

pub async fn symbols_in_file(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FileParams>,
) -> Result<Response, ApiError> {
    let symbols = state.store.symbols_in_file(&params.path)?;
    let json = serde_json::to_value(&symbols)?;
    Ok(Json(json).into_response())
}

#[derive(Deserialize)]
pub struct TopParams {
    pub limit: Option<usize>,
}

pub async fn symbols_top(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TopParams>,
) -> Result<Response, ApiError> {
    let limit = params.limit.unwrap_or(20).min(1000);
    // `symbols_by_pagerank` triggers the lazy PageRank compute on a cold cache,
    // so run it off the async runtime and emit `pagerank:recomputed` if it fired.
    let state2 = state.clone();
    with_rank_event(&state, move || {
        let symbols = state2.store.symbols_by_pagerank(Some(limit))?;
        let json = serde_json::to_value(&symbols)?;
        Ok(Json(json).into_response())
    })
    .await
}
