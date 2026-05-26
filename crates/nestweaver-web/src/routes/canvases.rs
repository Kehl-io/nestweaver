use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Serialize, Deserialize, Clone)]
pub struct Canvas {
    pub id: String,
    pub name: String,
    pub elements: Vec<serde_json::Value>,
    pub connections: Vec<serde_json::Value>,
    pub sections: Vec<serde_json::Value>,
}

fn canvases_dir(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".canvases");
    PathBuf::from(s)
}

fn canvas_file(db_path: &Path, id: &str) -> PathBuf {
    canvases_dir(db_path).join(format!("{id}.json"))
}

pub async fn list(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let dir = canvases_dir(&state.db_path);
    if !dir.exists() {
        return Ok(Json(json!([])).into_response());
    }

    let mut items = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| ApiError::internal(format!("failed to read canvases dir: {e}")))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| ApiError::internal(format!("failed to read dir entry: {e}")))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let data = std::fs::read_to_string(&path)
                .map_err(|e| ApiError::internal(format!("failed to read canvas: {e}")))?;
            let canvas: Canvas = serde_json::from_str(&data)?;
            let metadata = std::fs::metadata(&path)
                .map_err(|e| ApiError::internal(format!("failed to read metadata: {e}")))?;
            let modified_at = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            items.push(json!({
                "id": canvas.id,
                "name": canvas.name,
                "element_count": canvas.elements.len(),
                "modified_at": modified_at,
            }));
        }
    }

    Ok(Json(items).into_response())
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ApiError::bad_request("invalid id format"));
    }
    let path = canvas_file(&state.db_path, &id);
    if !path.exists() {
        return Err(ApiError::not_found(format!("canvas {id} not found")));
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| ApiError::internal(format!("failed to read canvas: {e}")))?;
    let canvas: Canvas = serde_json::from_str(&data)?;
    Ok(Json(canvas).into_response())
}

#[derive(Deserialize)]
pub struct CreateCanvas {
    pub name: String,
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateCanvas>,
) -> Result<Response, ApiError> {
    let _lock = state
        .file_lock
        .lock()
        .map_err(|_| ApiError::internal("lock poisoned"))?;
    let dir = canvases_dir(&state.db_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError::internal(format!("failed to create canvases dir: {e}")))?;

    let canvas = Canvas {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name,
        elements: Vec::new(),
        connections: Vec::new(),
        sections: Vec::new(),
    };

    let path = canvas_file(&state.db_path, &canvas.id);
    let data = serde_json::to_string_pretty(&canvas)
        .map_err(|e| ApiError::internal(format!("failed to serialize canvas: {e}")))?;
    std::fs::write(&path, data)
        .map_err(|e| ApiError::internal(format!("failed to write canvas: {e}")))?;

    Ok(Json(canvas).into_response())
}

pub async fn update(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(canvas): Json<Canvas>,
) -> Result<Response, ApiError> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ApiError::bad_request("invalid id format"));
    }
    let _lock = state
        .file_lock
        .lock()
        .map_err(|_| ApiError::internal("lock poisoned"))?;
    let path = canvas_file(&state.db_path, &id);
    if !path.exists() {
        return Err(ApiError::not_found(format!("canvas {id} not found")));
    }
    let data = serde_json::to_string_pretty(&canvas)
        .map_err(|e| ApiError::internal(format!("failed to serialize canvas: {e}")))?;
    std::fs::write(&path, data)
        .map_err(|e| ApiError::internal(format!("failed to write canvas: {e}")))?;
    Ok(Json(canvas).into_response())
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ApiError::bad_request("invalid id format"));
    }
    let _lock = state
        .file_lock
        .lock()
        .map_err(|_| ApiError::internal("lock poisoned"))?;
    let path = canvas_file(&state.db_path, &id);
    if !path.exists() {
        return Err(ApiError::not_found(format!("canvas {id} not found")));
    }
    std::fs::remove_file(&path)
        .map_err(|e| ApiError::internal(format!("failed to delete canvas: {e}")))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
