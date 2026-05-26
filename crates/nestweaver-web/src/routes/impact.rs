use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ImpactParams {
    pub depth: Option<u32>,
    pub confidence: Option<f32>,
}

pub async fn impact(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
    Query(params): Query<ImpactParams>,
) -> Result<Response, ApiError> {
    let confidence = params.confidence.unwrap_or(0.3).clamp(0.0, 1.0);
    let depth = params.depth.unwrap_or(3).min(20);

    // Verify symbol exists — return 404 if not found
    state
        .store
        .lookup_symbol(&uid)
        .map_err(|_| ApiError::not_found(format!("symbol '{uid}' not found")))?;

    let nodes = state.store.impact(&uid, depth, confidence)?;

    // ImpactNode doesn't derive Serialize, so we build JSON manually
    let json_nodes: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "uid": n.uid,
                "name": n.name,
                "file_path": n.file_path,
                "start_line": n.start_line,
                "edge_type": n.edge_type,
                "confidence": n.confidence,
                "depth": n.depth,
            })
        })
        .collect();

    Ok(Json(serde_json::Value::Array(json_nodes)).into_response())
}
