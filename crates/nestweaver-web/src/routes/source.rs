use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct SourceParams {
    pub file: Option<String>,
    pub line: Option<usize>,
    pub context: Option<usize>,
}

pub async fn source(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SourceParams>,
) -> Result<Response, ApiError> {
    let file = params
        .file
        .filter(|f| !f.is_empty())
        .ok_or_else(|| ApiError::bad_request("query parameter 'file' is required"))?;

    // Reject path traversal attempts before any filesystem operations
    if file.contains("..") || file.starts_with('/') || file.starts_with('\\') {
        return Err(ApiError::bad_request("invalid file path"));
    }

    let line = params.line.unwrap_or(1).max(1);
    let context = params.context.unwrap_or(10);

    let repos = nestweaver_engine::list_repos(&state.store, None)?;

    for repo in &repos {
        // Only repos with a known local working tree can serve source from
        // disk; remote-identity repos without one are skipped.
        let Some(repo_root) = repo.local_root() else {
            continue;
        };

        let full_path = std::path::Path::new(repo_root).join(&file);

        // Path safety: canonicalize and verify the file stays within the repo root
        let canon_root = match std::fs::canonicalize(repo_root) {
            Ok(p) => p,
            Err(_) => continue, // repo root doesn't exist on disk, skip
        };
        let canon_path = match std::fs::canonicalize(&full_path) {
            Ok(p) => p,
            Err(_) => continue, // file doesn't exist in this repo, skip
        };
        if !canon_path.starts_with(&canon_root) {
            return Err(ApiError::bad_request("file path escapes repository root"));
        }

        // Read the file
        let content = match std::fs::read_to_string(&canon_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let all_lines: Vec<&str> = content.lines().collect();
        let total_lines = all_lines.len();

        let start = line.saturating_sub(context + 1);
        let end = (line + context).min(total_lines);

        let extracted: Vec<&str> = all_lines[start..end].to_vec();

        return Ok(Json(json!({
            "file": file,
            "start_line": start + 1,
            "end_line": end,
            "lines": extracted,
            "total_lines": total_lines,
        }))
        .into_response());
    }

    // File not found in any repo
    Ok(Json(json!({
        "file": file,
        "error": "source not available",
        "line": line,
    }))
    .into_response())
}
