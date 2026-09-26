use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct SourceParams {
    pub file: Option<String>,
    /// nw-683: the repo uid whose copy of `file` to serve. Required when
    /// more than one indexed repo has the path.
    pub repo: Option<String>,
    pub line: Option<usize>,
    pub context: Option<usize>,
}

fn not_found(error: &str, file: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": error, "file": file })),
    )
        .into_response()
}

pub async fn source(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SourceParams>,
) -> Result<Response, ApiError> {
    let file = params
        .file
        .filter(|f| !f.is_empty())
        .ok_or_else(|| ApiError::bad_request("query parameter 'file' is required"))?;
    // Reject path traversal before anything touches the store or disk.
    // nw-627 (5): a NUL can never name a real file and must not reach the OS.
    if file.contains("..") || file.starts_with('/') || file.starts_with('\\') || file.contains('\0')
    {
        return Err(ApiError::bad_request("invalid file path"));
    }
    let line = params.line.unwrap_or(1).max(1);
    let context = params.context.unwrap_or(10);

    // nw-682: serve only what the graph indexed. A path with no File node
    // (.env, .git/config, anything gitignored) is never read from disk.
    let indexing = state.store.repos_indexing_file(&file)?;
    let repo_uid = match params.repo.as_deref().filter(|r| !r.is_empty()) {
        Some(repo) => {
            if state.store.lookup_repo(repo)?.is_none() {
                return Ok(not_found("repo_not_found", &file));
            }
            if !indexing.iter().any(|r| r == repo) {
                return Ok(not_found("source_not_indexed", &file));
            }
            repo.to_string()
        }
        None => match indexing.as_slice() {
            [] => return Ok(not_found("source_not_indexed", &file)),
            [only] => only.clone(),
            many => {
                return Ok((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "ambiguous_file",
                        "file": file,
                        "candidates": many,
                        "message": "more than one indexed repo has this path; pass repo=<uid>",
                    })),
                )
                    .into_response());
            }
        },
    };

    let Some(repo) = state.store.lookup_repo(&repo_uid)? else {
        return Ok(not_found("repo_not_found", &file));
    };
    // Only repos with a known local working tree can serve source from disk.
    let Some(repo_root) = repo.local_root() else {
        return Ok(not_found("source_not_available", &file));
    };
    let (Ok(canon_root), Ok(canon_path)) = (
        std::fs::canonicalize(repo_root),
        std::fs::canonicalize(std::path::Path::new(repo_root).join(&file)),
    ) else {
        return Ok(not_found("source_not_available", &file));
    };
    // A symlink inside the repo must not lead outside it.
    if !canon_path.starts_with(&canon_root) {
        return Err(ApiError::bad_request("file path escapes repository root"));
    }
    let Ok(content) = std::fs::read_to_string(&canon_path) else {
        return Ok(not_found("source_not_available", &file));
    };
    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();
    // Past-EOF `line` clamps to the last line so the window is never empty
    // and start never exceeds end. All arithmetic saturates: a raw
    // `context + 1` overflow would panic a debug build.
    let line = line.min(total_lines.max(1));
    let end = line.saturating_add(context).min(total_lines);
    let start = line.saturating_sub(context.saturating_add(1)).min(end);
    Ok(Json(json!({
        "file": file,
        "repo": repo_uid,
        "start_line": start + 1,
        "end_line": end,
        "lines": all_lines[start..end].to_vec(),
        "total_lines": total_lines,
    }))
    .into_response())
}
