use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Serialize, Deserialize, Clone)]
pub struct Perspective {
    pub id: String,
    pub name: String,
    pub config: serde_json::Value,
}

fn perspectives_path(db_path: &Path) -> PathBuf {
    nestweaver_engine::sidecar_path(db_path, ".perspectives.json")
}

fn read_perspectives(db_path: &Path) -> Result<Vec<Perspective>, ApiError> {
    let path = perspectives_path(db_path);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| ApiError::internal(format!("failed to read perspectives: {e}")))?;
    let perspectives: Vec<Perspective> = serde_json::from_str(&data)?;
    Ok(perspectives)
}

fn write_perspectives(db_path: &Path, perspectives: &[Perspective]) -> Result<(), ApiError> {
    let path = perspectives_path(db_path);
    let data = serde_json::to_string_pretty(perspectives)
        .map_err(|e| ApiError::internal(format!("failed to serialize perspectives: {e}")))?;
    std::fs::write(&path, data)
        .map_err(|e| ApiError::internal(format!("failed to write perspectives: {e}")))?;
    Ok(())
}

pub async fn list(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let perspectives = read_perspectives(&state.db_path)?;
    Ok(Json(perspectives).into_response())
}

#[derive(Deserialize)]
pub struct CreatePerspective {
    pub name: String,
    pub config: serde_json::Value,
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePerspective>,
) -> Result<Response, ApiError> {
    let _lock = state
        .file_lock
        .lock()
        .map_err(|_| ApiError::internal("lock poisoned"))?;
    let mut perspectives = read_perspectives(&state.db_path)?;
    let perspective = Perspective {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name,
        config: body.config,
    };
    perspectives.push(perspective.clone());
    write_perspectives(&state.db_path, &perspectives)?;
    Ok(Json(perspective).into_response())
}

pub async fn update(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<CreatePerspective>,
) -> Result<Response, ApiError> {
    let _lock = state
        .file_lock
        .lock()
        .map_err(|_| ApiError::internal("lock poisoned"))?;
    let mut perspectives = read_perspectives(&state.db_path)?;
    let entry = perspectives
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found(format!("perspective {id} not found")))?;
    entry.name = body.name;
    entry.config = body.config;
    let updated = entry.clone();
    write_perspectives(&state.db_path, &perspectives)?;
    Ok(Json(updated).into_response())
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    let _lock = state
        .file_lock
        .lock()
        .map_err(|_| ApiError::internal("lock poisoned"))?;
    let mut perspectives = read_perspectives(&state.db_path)?;
    let len_before = perspectives.len();
    perspectives.retain(|p| p.id != id);
    if perspectives.len() == len_before {
        return Err(ApiError::not_found(format!("perspective {id} not found")));
    }
    write_perspectives(&state.db_path, &perspectives)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
