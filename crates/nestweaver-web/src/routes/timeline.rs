use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use serde_json::Value;

use crate::error::ApiError;
use crate::state::AppState;

pub async fn timeline(
    State(_state): State<Arc<AppState>>,
    Path(_repo_uid): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(Value::Array(vec![])))
}
