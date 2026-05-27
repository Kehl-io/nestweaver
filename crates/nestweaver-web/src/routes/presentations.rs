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
pub struct Presentation {
    pub id: String,
    pub name: String,
    pub slides: Vec<serde_json::Value>,
}

fn presentations_dir(db_path: &Path) -> PathBuf {
    nestweaver_engine::sidecar_path(db_path, ".presentations")
}

fn presentation_file(db_path: &Path, id: &str) -> PathBuf {
    presentations_dir(db_path).join(format!("{id}.json"))
}

pub async fn list(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let dir = presentations_dir(&state.db_path);
    if !dir.exists() {
        return Ok(Json(json!([])).into_response());
    }

    let mut items = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| ApiError::internal(format!("failed to read presentations dir: {e}")))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| ApiError::internal(format!("failed to read dir entry: {e}")))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let data = std::fs::read_to_string(&path)
                .map_err(|e| ApiError::internal(format!("failed to read presentation: {e}")))?;
            let presentation: Presentation = serde_json::from_str(&data)?;
            let metadata = std::fs::metadata(&path)
                .map_err(|e| ApiError::internal(format!("failed to read metadata: {e}")))?;
            let modified_at = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            items.push(json!({
                "id": presentation.id,
                "name": presentation.name,
                "slide_count": presentation.slides.len(),
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
    let path = presentation_file(&state.db_path, &id);
    if !path.exists() {
        return Err(ApiError::not_found(format!("presentation {id} not found")));
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| ApiError::internal(format!("failed to read presentation: {e}")))?;
    let presentation: Presentation = serde_json::from_str(&data)?;
    Ok(Json(presentation).into_response())
}

#[derive(Deserialize)]
pub struct CreatePresentation {
    pub name: String,
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePresentation>,
) -> Result<Response, ApiError> {
    let _lock = state
        .file_lock
        .lock()
        .map_err(|_| ApiError::internal("lock poisoned"))?;
    let dir = presentations_dir(&state.db_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError::internal(format!("failed to create presentations dir: {e}")))?;

    let presentation = Presentation {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name,
        slides: Vec::new(),
    };

    let path = presentation_file(&state.db_path, &presentation.id);
    let data = serde_json::to_string_pretty(&presentation)
        .map_err(|e| ApiError::internal(format!("failed to serialize presentation: {e}")))?;
    std::fs::write(&path, data)
        .map_err(|e| ApiError::internal(format!("failed to write presentation: {e}")))?;

    Ok(Json(presentation).into_response())
}

pub async fn update(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(presentation): Json<Presentation>,
) -> Result<Response, ApiError> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ApiError::bad_request("invalid id format"));
    }
    let _lock = state
        .file_lock
        .lock()
        .map_err(|_| ApiError::internal("lock poisoned"))?;
    let path = presentation_file(&state.db_path, &id);
    if !path.exists() {
        return Err(ApiError::not_found(format!("presentation {id} not found")));
    }
    let data = serde_json::to_string_pretty(&presentation)
        .map_err(|e| ApiError::internal(format!("failed to serialize presentation: {e}")))?;
    std::fs::write(&path, data)
        .map_err(|e| ApiError::internal(format!("failed to write presentation: {e}")))?;
    Ok(Json(presentation).into_response())
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
    let path = presentation_file(&state.db_path, &id);
    if !path.exists() {
        return Err(ApiError::not_found(format!("presentation {id} not found")));
    }
    std::fs::remove_file(&path)
        .map_err(|e| ApiError::internal(format!("failed to delete presentation: {e}")))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn export_html(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ApiError::bad_request("invalid id format"));
    }
    let path = presentation_file(&state.db_path, &id);
    if !path.exists() {
        return Err(ApiError::not_found(format!("presentation {id} not found")));
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| ApiError::internal(format!("failed to read presentation: {e}")))?;
    let presentation: Presentation = serde_json::from_str(&data)?;
    let slides_json = serde_json::to_string(&presentation.slides)
        .map_err(|e| ApiError::internal(format!("failed to serialize slides: {e}")))?;

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  body {{ margin: 0; font-family: system-ui, sans-serif; background: #1a1a2e; color: #eee; }}
  .slide {{ display: none; min-height: 100vh; padding: 2rem; box-sizing: border-box;
            align-items: center; justify-content: center; flex-direction: column; }}
  .slide.active {{ display: flex; }}
  .nav {{ position: fixed; bottom: 1rem; right: 1rem; font-size: 0.9rem; color: #888; }}
  pre {{ background: #16213e; padding: 1rem; border-radius: 8px; overflow-x: auto; }}
</style>
</head>
<body>
<div id="slides"></div>
<div class="nav"><span id="counter"></span> &mdash; Arrow keys to navigate</div>
<script>
const slides = {slides_json};
let current = 0;
const container = document.getElementById('slides');
const counter = document.getElementById('counter');
slides.forEach((s, i) => {{
  const div = document.createElement('div');
  div.className = 'slide' + (i === 0 ? ' active' : '');
  div.innerHTML = '<pre>' + JSON.stringify(s, null, 2) + '</pre>';
  container.appendChild(div);
}});
function show(n) {{
  const all = document.querySelectorAll('.slide');
  if (all.length === 0) return;
  current = Math.max(0, Math.min(n, all.length - 1));
  all.forEach((el, i) => el.classList.toggle('active', i === current));
  counter.textContent = (current + 1) + ' / ' + all.length;
}}
show(0);
document.addEventListener('keydown', e => {{
  if (e.key === 'ArrowRight' || e.key === 'ArrowDown') show(current + 1);
  if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') show(current - 1);
}});
</script>
</body>
</html>"#,
        title = presentation.name,
        slides_json = slides_json,
    );

    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response())
}
