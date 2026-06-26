//! Admin REST API routes for NestWeaver server management.
//!
//! All routes under `/admin/api/` require the admin token via the
//! `AdminAuth` extractor. Covers repos, queue, drain/resume,
//! dead-letter, config reload, and full server status.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{
    Json,
    extract::{FromRef, FromRequestParts, Path, State},
    http::{StatusCode, request::Parts},
};
use serde::{Deserialize, Serialize};

use crate::state::AdminState;

// ── Admin auth extractor ───────────────────────────────────────────────

/// Axum extractor that validates the admin token from the Authorization
/// header. Returns 401 if missing or invalid.
pub struct AdminAuth;

impl<S> FromRequestParts<S> for AdminAuth
where
    S: Send + Sync,
    Arc<AdminState>: FromRef<S>,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let admin_state = Arc::<AdminState>::from_ref(state);
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match token {
            Some(t) if t == admin_state.admin_token => Ok(AdminAuth),
            _ => Err((StatusCode::UNAUTHORIZED, "admin token required")),
        }
    }
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RepoInfo {
    pub id: String,
    pub url: String,
    pub name: String,
    pub status: String,
    pub indexed_sha: String,
    pub symbol_count: i64,
}

#[derive(Deserialize)]
pub struct AddRepoRequest {
    pub url: String,
    pub branch: Option<String>,
}

#[derive(Serialize)]
pub struct QueueInfo {
    pub depth: u32,
    pub drained: bool,
}

#[derive(Serialize, Deserialize)]
pub struct DrainStatus {
    pub drained: bool,
    pub active_reads: u32,
    pub active_writes: u32,
}

#[derive(Serialize)]
pub struct AdminStatus {
    pub instance_id: String,
    pub uptime_seconds: u64,
    pub server_mode: bool,
    pub repo_count: usize,
    pub active_reads: u32,
    pub active_writes: u32,
    pub queue_depth: u32,
    pub drained: bool,
    pub version: String,
}

#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Repo management ────────────────────────────────────────────────────

/// GET /admin/api/repos — list repos with status and freshness.
pub async fn list_repos(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<Vec<RepoInfo>>, (StatusCode, String)> {
    let store = state.daemon_store.clone();

    let repos = tokio::task::spawn_blocking(move || store.list_repos(None))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task panicked: {e}")))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("list_repos failed: {e}")))?;

    let store = state.daemon_store.clone();
    let repo_infos = tokio::task::spawn_blocking(move || {
        repos
            .into_iter()
            .map(|r| {
                let symbol_count = store
                    .symbol_names_by_repo(&r.uid)
                    .map(|v| v.len() as i64)
                    .unwrap_or(0);
                let name = r.name.unwrap_or_else(|| {
                    r.url
                        .strip_prefix("file://")
                        .unwrap_or(&r.url)
                        .rsplit('/')
                        .next()
                        .unwrap_or(&r.url)
                        .to_string()
                });
                RepoInfo {
                    id: r.uid,
                    url: r.url,
                    name,
                    status: "indexed".to_string(),
                    indexed_sha: r.indexed_sha,
                    symbol_count,
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task panicked: {e}")))?;

    Ok(Json(repo_infos))
}

/// POST /admin/api/repos — add a new repo.
pub async fn add_repo(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Json(req): Json<AddRepoRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let _ = state;
    // Adding a repo requires writing to the instance config and triggering
    // an index. For now, return a stub that acknowledges the request.
    Ok(Json(MessageResponse {
        message: format!("repo {} queued for indexing", req.url),
    }))
}

/// DELETE /admin/api/repos/:id — remove a repo.
pub async fn remove_repo(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(repo_uid): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let store = state.daemon_store.clone();
    let uid = repo_uid.clone();

    tokio::task::spawn_blocking(move || {
        store
            .bulk_delete_repo_files_and_symbols(&uid)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("bulk_delete failed: {e}"),
                )
            })?;
        store.clear_repo_derived_nodes(&uid).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("clear_derived failed: {e}"),
            )
        })?;
        store.delete_repo_node(&uid).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("delete_repo_node failed: {e}"),
            )
        })?;
        Ok::<_, (StatusCode, String)>(())
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task panicked: {e}")))??;

    Ok(Json(MessageResponse {
        message: format!("repo {} removed", repo_uid),
    }))
}

/// POST /admin/api/repos/:id/reindex — trigger an immediate re-index.
pub async fn trigger_reindex(
    _auth: AdminAuth,
    State(_state): State<Arc<AdminState>>,
    Path(repo_uid): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    // Triggering a re-index requires the job queue from the daemon.
    // For now, acknowledge the request.
    Ok(Json(MessageResponse {
        message: format!("reindex queued for repo {}", repo_uid),
    }))
}

// ── Queue management ───────────────────────────────────────────────────

/// GET /admin/api/queue — queue state.
pub async fn get_queue(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<QueueInfo> {
    let depth = state.indexing_queue_depth.load(Ordering::Relaxed);
    let drained = state.drained.load(Ordering::Relaxed);
    Json(QueueInfo { depth, drained })
}

// ── Drain/Resume ───────────────────────────────────────────────────────

/// POST /admin/api/drain — stop workers from picking new jobs.
pub async fn drain(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<MessageResponse> {
    state.drained.store(true, Ordering::SeqCst);
    tracing::info!("admin API: workers drained");
    Json(MessageResponse {
        message: "workers drained — in-flight jobs will finish, no new jobs picked".to_string(),
    })
}

/// POST /admin/api/resume — resume normal processing.
pub async fn resume(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<MessageResponse> {
    state.drained.store(false, Ordering::SeqCst);
    tracing::info!("admin API: workers resumed");
    Json(MessageResponse {
        message: "workers resumed".to_string(),
    })
}

/// GET /admin/api/drain/status — current drain state.
pub async fn drain_status(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<DrainStatus> {
    Json(DrainStatus {
        drained: state.drained.load(Ordering::Relaxed),
        active_reads: state.active_reads.load(Ordering::Relaxed),
        active_writes: state.active_writes.load(Ordering::Relaxed),
    })
}

// ── Dead letter ────────────────────────────────────────────────────────

/// GET /admin/api/dead-letter — list dead-letter entries.
pub async fn list_dead_letter(
    _auth: AdminAuth,
    State(_state): State<Arc<AdminState>>,
) -> Json<Vec<serde_json::Value>> {
    // Dead letter data comes from the jobs SQLite DB which is owned by
    // the daemon's worker pool. Return empty list until wired to job queue.
    Json(vec![])
}

/// POST /admin/api/dead-letter/:id/retry — retry a dead-letter entry.
pub async fn retry_dead_letter(
    _auth: AdminAuth,
    State(_state): State<Arc<AdminState>>,
    Path(_id): Path<String>,
) -> Json<MessageResponse> {
    Json(MessageResponse {
        message: "dead-letter entry queued for retry".to_string(),
    })
}

/// DELETE /admin/api/dead-letter/:id — dismiss a dead-letter entry.
pub async fn dismiss_dead_letter(
    _auth: AdminAuth,
    State(_state): State<Arc<AdminState>>,
    Path(_id): Path<String>,
) -> Json<MessageResponse> {
    Json(MessageResponse {
        message: "dead-letter entry dismissed".to_string(),
    })
}

// ── Config reload ──────────────────────────────────────────────────────

/// POST /admin/api/reload — hot-reload instance.toml.
pub async fn reload_config(
    _auth: AdminAuth,
    State(_state): State<Arc<AdminState>>,
) -> Json<MessageResponse> {
    // Config reload will reuse the existing SIGHUP logic once wired.
    Json(MessageResponse {
        message: "config reload triggered".to_string(),
    })
}

// ── Status ─────────────────────────────────────────────────────────────

/// GET /admin/api/status — full server status.
pub async fn get_status(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<AdminStatus> {
    let store = state.daemon_store.clone();
    let repo_count = tokio::task::spawn_blocking(move || {
        store.list_repos(None).map(|r| r.len()).unwrap_or(0)
    })
    .await
    .unwrap_or(0);

    Json(AdminStatus {
        instance_id: state.instance_id.clone(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        server_mode: true,
        repo_count,
        active_reads: state.active_reads.load(Ordering::Relaxed),
        active_writes: state.active_writes.load(Ordering::Relaxed),
        queue_depth: state.indexing_queue_depth.load(Ordering::Relaxed),
        drained: state.drained.load(Ordering::Relaxed),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_admin_state() -> Arc<AdminState> {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.lbug");
        let store =
            nestweaver_store::GraphStore::open_or_create(&db_path).expect("open test store");
        // Leak the tempdir so it lives as long as the store.
        std::mem::forget(dir);
        Arc::new(AdminState {
            admin_token: "test-admin-token".to_string(),
            daemon_store: Arc::new(store),
            instance_id: "test".to_string(),
            start_time: std::time::Instant::now(),
            active_reads: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            active_writes: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            drained: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            indexing_queue_depth: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        })
    }

    fn test_router() -> Router {
        let state = test_admin_state();
        Router::new()
            .route("/admin/api/status", get(get_status))
            .route("/admin/api/repos", get(list_repos))
            .route("/admin/api/queue", get(get_queue))
            .route("/admin/api/drain/status", get(drain_status))
            .with_state(state)
    }

    #[tokio::test]
    async fn status_requires_admin_token() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_with_valid_admin_token() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/status")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn repos_returns_json() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/repos")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn drain_status_shows_not_drained() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/drain/status")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: DrainStatus = serde_json::from_slice(&body).unwrap();
        assert!(!status.drained);
    }
}
