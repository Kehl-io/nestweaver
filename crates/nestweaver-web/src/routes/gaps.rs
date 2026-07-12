use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::AppState;

pub async fn gaps(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let result = state.gaps_cache.get_or_compute(&state.store)?;
    Ok(Json(result).into_response())
}
