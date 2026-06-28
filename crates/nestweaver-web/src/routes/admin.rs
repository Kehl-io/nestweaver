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
use subtle::ConstantTimeEq;

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
            Some(t)
                if t.as_bytes()
                    .ct_eq(admin_state.admin_token.as_bytes())
                    .into() =>
            {
                Ok(AdminAuth)
            }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_priority: Option<std::collections::HashMap<String, u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize, Deserialize)]
pub struct DrainStatus {
    pub drained: bool,
    pub active_reads: u32,
    pub active_writes: u32,
}

#[derive(Serialize)]
pub struct RepoStats {
    pub total: usize,
    pub indexed: usize,
    pub stale: usize,
    pub dead_letter: usize,
}

#[derive(Serialize)]
pub struct SymbolStats {
    pub total: usize,
}

#[derive(Serialize)]
pub struct QueueStats {
    pub pending: u32,
    pub running: u32,
    pub dead_letter: usize,
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
    // Nested shapes expected by the React admin dashboard.
    pub repos: RepoStats,
    pub symbols: SymbolStats,
    pub queue: QueueStats,
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
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_repos failed: {e}"),
            )
        })?;

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
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })?;

    Ok(Json(repo_infos))
}

/// POST /admin/api/repos — add a new repo.
pub async fn add_repo(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Json(req): Json<AddRepoRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    // Derive the jobs database path from the brain database path.
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
    let repo_url = req.url.clone();
    let branch = req.branch.clone();

    tokio::task::spawn_blocking(move || {
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        let repo_id = nestweaver_engine::jobs::canonical_repo_id(&repo_url);
        queue
            .upsert(
                &repo_id,
                &repo_url,
                nestweaver_engine::jobs::JobTrigger::Unindexed,
                branch.as_deref(),
            )
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("enqueue job: {e}"),
                )
            })?;
        Ok::<_, (StatusCode, String)>(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    // Persist admin-added repos into instance config so scheduler/webhook
    // allowlisting survives daemon restarts.
    if let Some(config_path) = state.config_path.clone() {
        let repo_url = req.url.clone();
        let branch = req.branch.clone();
        tokio::task::spawn_blocking(move || {
            nestweaver_engine::append_repo_to_config_file(
                &config_path,
                &repo_url,
                branch.as_deref(),
            )
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist repo config: {e}"),
            )
        })?;
    }

    // Update live scheduler so the new repo is polled without restart.
    if let Some(ref tx) = state.scheduler_tx {
        let repo_name = nestweaver_engine::pull::repo_name_from_url(&req.url);
        let _ = tx
            .send(nestweaver_engine::scheduler::SchedulerCommand::AddRepo {
                repo_id: repo_name,
                repo_url: req.url.clone(),
                poll_override: None,
                branch: req.branch.clone(),
            })
            .await;
    }

    // Update webhook allowed repos so pushes are accepted immediately.
    let canonical = nestweaver_engine::jobs::canonical_repo_id(&req.url);
    if let Some(ref lock) = state.webhook_allowed_repos {
        if let Ok(mut guard) = lock.write() {
            if let Some(ref mut set) = *guard {
                set.insert(canonical.clone());
            }
        }
    }

    // Update webhook branch map if a branch was specified.
    if let Some(ref branch) = req.branch {
        if let Some(ref lock) = state.webhook_repo_branches {
            if let Ok(mut guard) = lock.write() {
                guard.insert(canonical, branch.clone());
            }
        }
    }

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

    // Look up the repo URL before deletion so we can clean up scheduler
    // and webhook state afterwards.
    let store_for_lookup = state.daemon_store.clone();
    let uid_for_lookup = repo_uid.clone();
    let repo_url: Option<String> = tokio::task::spawn_blocking(move || {
        store_for_lookup
            .lookup_repo(&uid_for_lookup)
            .ok()
            .flatten()
            .map(|r| r.url)
    })
    .await
    .ok()
    .flatten();

    // Purge queued jobs FIRST so no new workers can claim while we delete.
    if let Some(ref url) = repo_url {
        let canonical = nestweaver_engine::jobs::canonical_repo_id(url);
        let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(queue) = nestweaver_engine::jobs::JobQueue::open(&jobs_path) {
                let _ = queue.cancel_repo(&canonical);
            }
        })
        .await;
    }

    // Persist the removal before deleting graph data so a failed config write
    // cannot leave the next restart re-admitting the repo silently.
    if let (Some(config_path), Some(url)) = (state.config_path.clone(), repo_url.clone()) {
        tokio::task::spawn_blocking(move || {
            nestweaver_engine::remove_repo_from_config_file(&config_path, &url)
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist repo removal: {e}"),
            )
        })?;
    }

    // Delete graph data under write mutex. An already-claimed worker will
    // also acquire this mutex before indexing; when it runs, it checks
    // whether the repo node still exists and skips if deleted.
    let write_mutex = state.write_mutex.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = write_mutex.as_ref().map(|m| m.blocking_lock());
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
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    // Remove from live scheduler.
    if let Some(ref tx) = state.scheduler_tx {
        // The scheduler seeds repos with `repo_cfg.name.unwrap_or(repo_name_from_url(...))`.
        // To match that, look up the configured name from the instance config first.
        let url_derived = repo_url
            .as_deref()
            .map(nestweaver_engine::pull::repo_name_from_url)
            .unwrap_or_else(|| repo_uid.clone());
        let sched_id = if let Some(ref config_path) = state.config_path {
            nestweaver_engine::InstanceConfig::from_file(config_path)
                .ok()
                .and_then(|cfg| {
                    let canonical = repo_url
                        .as_deref()
                        .map(nestweaver_engine::jobs::canonical_repo_id)
                        .unwrap_or_default();
                    cfg.repos
                        .iter()
                        .find(|r| nestweaver_engine::jobs::canonical_repo_id(&r.url) == canonical)
                        .and_then(|r| r.name.clone())
                })
                .unwrap_or(url_derived)
        } else {
            url_derived
        };
        let _ = tx
            .send(nestweaver_engine::scheduler::SchedulerCommand::RemoveRepo {
                repo_id: sched_id.clone(),
            })
            .await;
        // Also try the URL-derived name in case the config name didn't match
        // (e.g., repo already removed from config, or name was customized).
        let url_fallback = repo_url
            .as_deref()
            .map(nestweaver_engine::pull::repo_name_from_url)
            .unwrap_or_default();
        if !url_fallback.is_empty() && url_fallback != sched_id {
            let _ = tx
                .send(nestweaver_engine::scheduler::SchedulerCommand::RemoveRepo {
                    repo_id: url_fallback,
                })
                .await;
        }
    }

    // Remove from webhook allowed repos.
    if let Some(ref url) = repo_url {
        let canonical = nestweaver_engine::jobs::canonical_repo_id(url);
        if let Some(ref lock) = state.webhook_allowed_repos {
            if let Ok(mut guard) = lock.write() {
                if let Some(ref mut set) = *guard {
                    set.remove(&canonical);
                }
            }
        }
        if let Some(ref lock) = state.webhook_repo_branches {
            if let Ok(mut guard) = lock.write() {
                guard.remove(&canonical);
            }
        }
    }

    Ok(Json(MessageResponse {
        message: format!("repo {} removed", repo_uid),
    }))
}

/// POST /admin/api/repos/:id/reindex — trigger an immediate re-index.
pub async fn trigger_reindex(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(repo_uid): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let store = state.daemon_store.clone();
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
    let uid = repo_uid.clone();
    let branch_map = state.webhook_repo_branches.clone();

    tokio::task::spawn_blocking(move || {
        // Look up the repo URL from the store.
        let repo = store
            .lookup_repo(&uid)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("lookup repo: {e}"),
                )
            })?
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("repo {} not found", uid)))?;

        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        let repo_id = nestweaver_engine::jobs::canonical_repo_id(&repo.url);
        let branch = configured_branch_for_repo(&branch_map, &repo_id);
        queue
            .upsert(
                &repo_id,
                &repo.url,
                nestweaver_engine::jobs::JobTrigger::Webhook,
                branch.as_deref(),
            )
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("enqueue job: {e}"),
                )
            })?;
        Ok::<_, (StatusCode, String)>(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    Ok(Json(MessageResponse {
        message: format!("reindex queued for repo {}", repo_uid),
    }))
}

fn configured_branch_for_repo(
    branch_map: &Option<Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>>,
    repo_id: &str,
) -> Option<String> {
    branch_map.as_ref().and_then(|branches| {
        branches
            .read()
            .ok()
            .and_then(|map| map.get(repo_id).cloned())
    })
}

// ── Queue management ───────────────────────────────────────────────────

/// GET /admin/api/queue — queue state.
pub async fn get_queue(_auth: AdminAuth, State(state): State<Arc<AdminState>>) -> Json<QueueInfo> {
    let depth = state.indexing_queue_depth.load(Ordering::Relaxed);
    let drained = state.drained.load(Ordering::Relaxed);

    // Read actual running jobs and pending count from the SQLite job queue.
    // Show pending jobs regardless of drain state so operators can see what
    // is waiting to be processed.
    let db_path = state.db_path.clone();
    let (running_jobs, pending_count): (Option<Vec<serde_json::Value>>, Option<i64>) =
        tokio::task::spawn_blocking(move || {
            let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
            match nestweaver_engine::jobs::JobQueue::open(&jobs_path) {
                Ok(q) => {
                    let running = q.running_jobs().ok().map(|jobs| {
                        jobs.into_iter()
                            .map(|j| {
                                serde_json::json!({
                                    "repo": j.repo,
                                    "started_at": j.started_at,
                                    "duration_s": j.duration_s,
                                })
                            })
                            .collect()
                    });
                    let pending = q.queue_depth().ok().map(|d| d.pending);
                    (running, pending)
                }
                Err(_) => (None, None),
            }
        })
        .await
        .unwrap_or((None, None));

    Json(QueueInfo {
        depth,
        drained,
        pending: pending_count,
        by_priority: None,
        running: running_jobs,
    })
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
    State(state): State<Arc<AdminState>>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");

    let entries = tokio::task::spawn_blocking(move || {
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        let dead = queue.dead_letters().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("dead_letters: {e}"),
            )
        })?;
        let values: Vec<serde_json::Value> = dead
            .into_iter()
            .map(|j| {
                serde_json::json!({
                    "id": j.id,
                    "repo_id": j.repo_id,
                    "repo_url": j.repo_url,
                    // Frontend-expected fields:
                    "repo": j.repo_id,
                    "error": j.error_msg,
                    "last_attempt": j.updated_at,
                    "attempts": j.attempt,
                    // Keep original fields for backwards compat:
                    "attempt": j.attempt,
                    "max_attempts": j.max_attempts,
                    "updated_at": j.updated_at,
                })
            })
            .collect();
        Ok::<_, (StatusCode, String)>(values)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    Ok(Json(entries))
}

/// POST /admin/api/dead-letter/:id/retry — retry a dead-letter entry.
///
/// The `:id` parameter is the integer primary key from the dead-letter listing,
/// not the `repo_id` string. This matches the `id` field in the JSON returned
/// by `GET /admin/api/dead-letter`.
pub async fn retry_dead_letter(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
    let job_id: i64 = id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid job id: {id}")))?;

    let retried = tokio::task::spawn_blocking(move || {
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        queue.reset_dead_letter_by_id(job_id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reset_dead_letter: {e}"),
            )
        })
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    if retried {
        Ok(Json(MessageResponse {
            message: format!("dead-letter entry {} queued for retry", id),
        }))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("no dead-letter entry with id {id}"),
        ))
    }
}

/// DELETE /admin/api/dead-letter/:id — dismiss a dead-letter entry.
pub async fn dismiss_dead_letter(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
    let job_id: i64 = id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid job id: {id}")))?;

    let dismissed = tokio::task::spawn_blocking(move || {
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        queue.dismiss_dead_letter(job_id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("dismiss_dead_letter: {e}"),
            )
        })
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    if dismissed {
        Ok(Json(MessageResponse {
            message: format!("dead-letter entry {} dismissed", id),
        }))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("no dead-letter entry with id {id}"),
        ))
    }
}

// ── Config reload ──────────────────────────────────────────────────────

/// POST /admin/api/reload — hot-reload instance.toml.
pub async fn reload_config(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let Some(ref config_path) = state.config_path else {
        return Ok(Json(MessageResponse {
            message: "no config path configured — daemon started without --config".to_string(),
        }));
    };

    let path = config_path.clone();
    let store = state.daemon_store.clone();
    let db_path = state.db_path.clone();
    let message = tokio::task::spawn_blocking(move || {
        match nestweaver_engine::InstanceConfig::from_file(&path) {
            Ok(cfg) => {
                let repo_count = cfg.repos.len();
                tracing::info!(
                    path = %path.display(),
                    repos = repo_count,
                    "config reloaded from disk"
                );

                // ── Reconcile declared repos vs indexed repos ─────────
                let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
                let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).ok();

                // Collect declared repo URLs from config.
                let declared_urls: std::collections::HashSet<String> =
                    cfg.repos.iter().map(|r| r.url.clone()).collect();

                // Collect indexed repo URLs from the store.
                let indexed_urls: std::collections::HashSet<String> = store
                    .list_repos(None)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| r.url)
                    .collect();

                let mut new_repos = 0usize;
                let mut orphaned_repos = 0usize;

                // New repos in config but not yet indexed: enqueue.
                for r in &cfg.repos {
                    if !indexed_urls.contains(&r.url) {
                        tracing::info!(url = %r.url, "config reload: new repo — queueing for indexing");
                        if let Some(ref q) = queue {
                            let repo_id = nestweaver_engine::jobs::canonical_repo_id(&r.url);
                            let _ = q.upsert(
                                &repo_id,
                                &r.url,
                                nestweaver_engine::jobs::JobTrigger::Unindexed,
                                r.branch.as_deref(),
                            );
                        }
                        new_repos += 1;
                    }
                }

                // Indexed repos no longer in config: log warning.
                for url in &indexed_urls {
                    if !declared_urls.contains(url) {
                        tracing::warn!(
                            url = %url,
                            "config reload: repo no longer in config (orphaned)"
                        );
                        orphaned_repos += 1;
                    }
                }

                let mut msg = format!(
                    "config reloaded from {} ({} repos configured)",
                    path.display(),
                    repo_count,
                );
                if new_repos > 0 {
                    msg.push_str(&format!(", {} new repos queued", new_repos));
                }
                if orphaned_repos > 0 {
                    msg.push_str(&format!(", {} orphaned repos", orphaned_repos));
                }
                Ok(msg)
            }
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "config reload failed");
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to parse config: {e}"),
                ))
            }
        }
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    // Notify the live scheduler so it picks up added/removed repos
    // without a daemon restart.
    if let Some(ref tx) = state.scheduler_tx {
        if let Some(ref config_path) = state.config_path {
            if let Ok(cfg) = nestweaver_engine::InstanceConfig::from_file(config_path) {
                let repos: Vec<_> = cfg
                    .repos
                    .iter()
                    .map(|r| {
                        let repo_name = r
                            .name
                            .clone()
                            .unwrap_or_else(|| nestweaver_engine::pull::repo_name_from_url(&r.url));
                        let poll_override = r.poll.as_deref().and_then(|p| match p {
                            "never" => Some(nestweaver_engine::scheduler::PollOverride::Never),
                            "manual" => Some(nestweaver_engine::scheduler::PollOverride::Manual),
                            other => nestweaver_engine::config::parse_duration(other)
                                .map(nestweaver_engine::scheduler::PollOverride::Fixed),
                        });
                        (repo_name, r.url.clone(), poll_override, r.branch.clone())
                    })
                    .collect();
                let new_min_poll =
                    nestweaver_engine::config::parse_duration(&cfg.server.indexing.min_poll);
                let new_max_poll =
                    nestweaver_engine::config::parse_duration(&cfg.server.indexing.max_poll);
                let _ = tx
                    .send(
                        nestweaver_engine::scheduler::SchedulerCommand::ReloadConfig {
                            repos,
                            min_poll: new_min_poll,
                            max_poll: new_max_poll,
                        },
                    )
                    .await;
            }
        }
    }

    // Update webhook state so new/changed repos take effect without restart.
    if let Some(ref config_path) = state.config_path {
        if let Ok(cfg) = nestweaver_engine::InstanceConfig::from_file(config_path) {
            if let Some(ref lock) = state.webhook_allowed_repos {
                let new_allowed: std::collections::HashSet<String> = cfg
                    .repos
                    .iter()
                    .filter(|r| r.poll.as_deref() != Some("manual"))
                    .map(|r| nestweaver_engine::jobs::canonical_repo_id(&r.url))
                    .collect();
                if let Ok(mut guard) = lock.write() {
                    *guard = Some(new_allowed);
                }
            }
            if let Some(ref lock) = state.webhook_repo_branches {
                let new_branches: std::collections::HashMap<String, String> = cfg
                    .repos
                    .iter()
                    .filter_map(|r| {
                        r.branch.as_ref().map(|b| {
                            (
                                nestweaver_engine::jobs::canonical_repo_id(&r.url),
                                b.clone(),
                            )
                        })
                    })
                    .collect();
                if let Ok(mut guard) = lock.write() {
                    *guard = new_branches;
                }
            }
        }
    }

    Ok(Json(MessageResponse { message }))
}

// ── Status ─────────────────────────────────────────────────────────────

/// GET /admin/api/status — full server status.
pub async fn get_status(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<AdminStatus> {
    let store = state.daemon_store.clone();
    let (repo_count, symbol_count) = tokio::task::spawn_blocking(move || {
        let repos = store.list_repos(None).map(|r| r.len()).unwrap_or(0);
        let symbols = store.count_symbols().unwrap_or(0);
        (repos, symbols)
    })
    .await
    .unwrap_or((0, 0));

    // Count pending/running/dead-letter entries from the job queue. The
    // persisted queue is the operator-facing source of truth, especially while
    // workers are drained and the atomic worker-depth hint is zero.
    let db_path = state.db_path.clone();
    let (pending_count, dead_letter_count, running_count) =
        tokio::task::spawn_blocking(move || {
            let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
            let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).ok();
            let depth = queue.as_ref().and_then(|q| q.queue_depth().ok());
            let dead = depth.as_ref().map(|d| d.dead_letter as usize).unwrap_or(0);
            let running = depth.as_ref().map(|d| d.running as u32).unwrap_or(0);
            let pending = depth.as_ref().map(|d| d.pending as u32);
            (pending, dead, running)
        })
        .await
        .unwrap_or((None, 0, 0));

    let queue_depth =
        pending_count.unwrap_or_else(|| state.indexing_queue_depth.load(Ordering::Relaxed));

    Json(AdminStatus {
        instance_id: state.instance_id.clone(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        server_mode: true,
        repo_count,
        active_reads: state.active_reads.load(Ordering::Relaxed),
        active_writes: state.active_writes.load(Ordering::Relaxed),
        queue_depth,
        drained: state.drained.load(Ordering::Relaxed),
        version: env!("CARGO_PKG_VERSION").to_string(),
        repos: RepoStats {
            total: repo_count,
            indexed: repo_count,
            stale: 0,
            dead_letter: dead_letter_count,
        },
        symbols: SymbolStats {
            total: symbol_count,
        },
        queue: QueueStats {
            pending: queue_depth,
            running: running_count,
            dead_letter: dead_letter_count,
        },
    })
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::{
        Router,
        routing::{get, post},
    };
    use tower::ServiceExt;

    fn test_admin_state() -> Arc<AdminState> {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.lbug");
        let store =
            nestweaver_store::GraphStore::open_or_create(&db_path).expect("open test store");
        let db_path_clone = db_path.clone();
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
            db_path: db_path_clone,
            config_path: None,
            scheduler_tx: None,
            webhook_allowed_repos: None,
            webhook_repo_branches: None,
            write_mutex: None,
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

    #[test]
    fn configured_branch_for_repo_reads_shared_branch_map() {
        let mut map = std::collections::HashMap::new();
        map.insert("github.com/org/repo".to_string(), "release".to_string());
        let branch_map = Some(Arc::new(std::sync::RwLock::new(map)));

        assert_eq!(
            configured_branch_for_repo(&branch_map, "github.com/org/repo"),
            Some("release".to_string())
        );
        assert_eq!(configured_branch_for_repo(&branch_map, "missing"), None);
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

    #[tokio::test]
    async fn status_uses_persisted_pending_queue_count() {
        let state = test_admin_state();
        let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).unwrap();
        queue
            .upsert(
                "repo-a",
                "file:///tmp/repo-a",
                nestweaver_engine::jobs::JobTrigger::Webhook,
                None,
            )
            .unwrap();

        let app = Router::new()
            .route("/admin/api/status", get(get_status))
            .with_state(state);
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
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["queue_depth"], 1);
        assert_eq!(status["queue"]["pending"], 1);
    }

    #[tokio::test]
    async fn add_repo_persists_instance_config() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let config_path = dir.path().join("instance.toml");
        std::fs::write(
            &config_path,
            r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[repos]]
url = "https://github.com/example/existing"
"#,
        )
        .unwrap();
        let store =
            nestweaver_store::GraphStore::open_or_create(&db_path).expect("open test store");
        let state = Arc::new(AdminState {
            admin_token: "test-admin-token".to_string(),
            daemon_store: Arc::new(store),
            instance_id: "test".to_string(),
            start_time: std::time::Instant::now(),
            active_reads: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            active_writes: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            drained: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            indexing_queue_depth: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            db_path,
            config_path: Some(config_path.clone()),
            scheduler_tx: None,
            webhook_allowed_repos: None,
            webhook_repo_branches: None,
            write_mutex: None,
        });

        let app = Router::new()
            .route("/admin/api/repos", post(add_repo))
            .with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api/repos")
                    .header("Authorization", "Bearer test-admin-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        r#"{"url":"https://github.com/example/new","branch":"main"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let cfg = nestweaver_engine::InstanceConfig::from_file(&config_path).unwrap();
        assert!(cfg.repos.iter().any(|repo| {
            repo.url == "https://github.com/example/new" && repo.branch.as_deref() == Some("main")
        }));
    }
}
