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

use crate::state::{AdminState, PendingDevice};
use nestweaver_engine::jobs::JobQueue;
use std::sync::Mutex;

/// Shutdown-visible admission for an admin mutation.
///
/// Increment-before-check with sequential consistency gives the daemon drain
/// and this request a single order: either the drain observes this writer, or
/// this request observes shutdown and refuses before its first mutation.
struct AdminMutationAdmission {
    active_writes: Arc<std::sync::atomic::AtomicU32>,
}

impl AdminMutationAdmission {
    fn admit(state: &AdminState) -> Result<Self, (StatusCode, String)> {
        state.active_writes.fetch_add(1, Ordering::SeqCst);
        if state.shutdown_started.load(Ordering::SeqCst) {
            state.active_writes.fetch_sub(1, Ordering::SeqCst);
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon is shutting down and is not accepting new admin mutations; retry against the daemon that starts next".to_string(),
            ));
        }
        Ok(Self {
            active_writes: Arc::clone(&state.active_writes),
        })
    }
}

impl Drop for AdminMutationAdmission {
    fn drop(&mut self) {
        self.active_writes.fetch_sub(1, Ordering::Release);
    }
}

// ── Shared job-queue access ────────────────────────────────────────────

/// RAII handle to a job-queue connection for an admin operation. Wraps either
/// the daemon's shared connection (held under its mutex) or a transient
/// fallback. Derefs to [`JobQueue`] so every call site is uniform.
pub(crate) enum JobQueueHandle<'a> {
    Shared(std::sync::MutexGuard<'a, JobQueue>),
    Owned(JobQueue),
}

impl std::ops::Deref for JobQueueHandle<'_> {
    type Target = JobQueue;
    fn deref(&self) -> &JobQueue {
        match self {
            JobQueueHandle::Shared(guard) => guard,
            JobQueueHandle::Owned(queue) => queue,
        }
    }
}

/// Acquire a job-queue handle for an admin operation. Uses the daemon's shared
/// connection when wired (server mode) — opening a *second* connection to the
/// same SQLite file races the worker's WAL checkpoint and crashes the daemon
/// with SIGBUS on macOS. Opens a transient connection only when no shared queue
/// is present (tests / non-server mode).
pub(crate) fn acquire_job_queue<'a>(
    shared: &'a Option<Arc<Mutex<JobQueue>>>,
    db_path: &std::path::Path,
) -> anyhow::Result<JobQueueHandle<'a>> {
    if let Some(shared) = shared {
        let guard = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("job queue mutex poisoned"))?;
        return Ok(JobQueueHandle::Shared(guard));
    }
    let jobs_path = nestweaver_engine::sidecar_path(db_path, ".jobs.sqlite");
    Ok(JobQueueHandle::Owned(JobQueue::open(&jobs_path)?))
}

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

/// Admin-gated Prometheus `/metrics` handler for the `/admin/api` router.
///
/// The admin router is nested onto the network-facing MCP listener, so its
/// `/metrics` (reachable at `/admin/api/metrics`) would otherwise leak
/// operational counters unauthenticated — every other admin route requires the
/// admin token, and this one now does too (S.5). The top-level `/metrics` on
/// the MCP listener is separately gated behind the query/admin bearer.
pub async fn metrics(_auth: AdminAuth) -> impl axum::response::IntoResponse {
    crate::routes::metrics::metrics_handler().await
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

/// Live client-connection counts shown on the admin dashboard. `grpc` is the
/// in-flight gRPC read+write count (a proxy for active clients); `mcp` is the
/// number of live MCP-over-HTTP sessions.
#[derive(Serialize)]
pub struct Connections {
    pub grpc: u32,
    pub mcp: u32,
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
    /// Size of the brain database file in bytes. Used by the frontend
    /// Overview dashboard to display the database size.
    pub db_size_bytes: u64,
    // Nested shapes expected by the React admin dashboard.
    pub repos: RepoStats,
    pub symbols: SymbolStats,
    pub queue: QueueStats,
    /// Live gRPC/MCP client-connection counts for the Overview dashboard.
    pub connections: Connections,
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
                let name = r.name.clone().unwrap_or_else(|| {
                    nestweaver_schema::repo_name(&r.url).unwrap_or_else(|| r.url.clone())
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

pub use nestweaver_engine::ssrf::config_repo_url_allowed;
/// SSRF helpers now live in `nestweaver_engine::ssrf` so the engine's
/// clone/fetch-time guard and this add-time gate share one implementation
/// (pure refactor — see nw-007). Re-export `config_repo_url_allowed` so existing
/// callers (`server.rs` config-repo enqueue) keep using
/// `nestweaver_web::routes::admin::config_repo_url_allowed`.
use nestweaver_engine::ssrf::{
    any_resolved_ip_is_internal, host_to_resolve, resolve_host, validate_repo_url,
};

/// POST /admin/api/repos — add a new repo.
pub async fn add_repo(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Json(req): Json<AddRepoRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    // Validate the URL scheme + host to prevent SSRF (file://, internal
    // hostnames, private/loopback IPs, alternate IPv4 encodings, IPv6-embedded
    // IPv4). See `validate_repo_url` — this part is pure (no DNS).
    validate_repo_url(&req.url).map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    // SSRF defense-in-depth: for a DNS hostname (not a literal/encoded IP),
    // resolve it and reject if ANY resolved address is internal. Catches names
    // that point at internal IPs plus basic DNS-rebinding at add-time. The
    // lookup blocks, so it runs on a blocking thread (kept out of the pure
    // `validate_repo_url`).
    //
    // TOCTOU caveat: resolution happens here at add-time, so a name could later
    // re-resolve to an internal IP at fetch time. True fetch-time enforcement
    // (validating the connected IP when the indexer clones) is out of scope and
    // tracked separately.
    if let Some(host) = host_to_resolve(&req.url) {
        let resolved = tokio::task::spawn_blocking(move || resolve_host(&host))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("task panicked: {e}"),
                )
            })?
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("rejected hostname: {e}")))?;
        if any_resolved_ip_is_internal(&resolved) {
            return Err((
                StatusCode::BAD_REQUEST,
                "rejected hostname: resolves to an internal address".to_string(),
            ));
        }
    }

    // Persist admin-added repos into instance config FIRST so scheduler/webhook
    // allowlisting survives daemon restarts. If this fails, we never enqueue the
    // index job — the config is the source of truth for what should be indexed.
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

    // Use the daemon's shared job-queue connection (or a transient fallback).
    let job_queue = state.job_queue.clone();
    let db_path = state.db_path.clone();
    let repo_url = req.url.clone();
    let branch = req.branch.clone();

    tokio::task::spawn_blocking(move || {
        let queue = acquire_job_queue(&job_queue, &db_path).map_err(|e| {
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
    if let Some(ref lock) = state.webhook_allowed_repos
        && let Ok(mut guard) = lock.write()
        && let Some(ref mut set) = *guard
    {
        set.insert(canonical.clone());
    }

    // Update webhook branch map if a branch was specified.
    if let Some(ref branch) = req.branch
        && let Some(ref lock) = state.webhook_repo_branches
        && let Ok(mut guard) = lock.write()
    {
        guard.insert(canonical, branch.clone());
    }

    Ok(Json(MessageResponse {
        message: format!("repo {} queued for indexing", req.url),
    }))
}

fn run_admin_remove_repo_with<C, D>(
    store: &nestweaver_store::GraphStore,
    db_path: &std::path::Path,
    repo_uid: &str,
    clear_derived: C,
    delete_repo: D,
) -> Result<(), (StatusCode, String)>
where
    C: FnOnce(&nestweaver_store::GraphStore, &str) -> Result<(), (StatusCode, String)>,
    D: FnOnce(&nestweaver_store::GraphStore, &str) -> Result<(), (StatusCode, String)>,
{
    store
        .bulk_delete_repo_files_and_symbols(repo_uid)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("bulk_delete failed: {e}"),
            )
        })?;
    let cascade_result = clear_derived(store, repo_uid).and_then(|()| delete_repo(store, repo_uid));
    let reconciliation = nestweaver_engine::finalize_code_graph_deletion(
        store,
        db_path,
        &[repo_uid.to_string()],
        "admin repo removal",
    );
    match (cascade_result, reconciliation) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(mutation), Ok(())) => Err(mutation),
        (Ok(()), Err(reconciliation)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            reconciliation.to_string(),
        )),
        (Err((status, mutation)), Err(reconciliation)) => {
            Err((status, format!("{mutation}; {reconciliation}")))
        }
    }
}

/// DELETE /admin/api/repos/:id — remove a repo.
pub async fn remove_repo(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(repo_uid): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    // Admission must precede queue/config/scheduler as well as graph mutation:
    // all are durable parts of this operation and none may outlive shutdown.
    // The task owns the admission (and the entire mutation) independently of
    // the HTTP request future: dropping an axum request drops its JoinHandle,
    // not the spawned task, so a disconnected/aborted client cannot make the
    // drain counter reach zero while spawn_blocking still owns graph work.
    let admission = AdminMutationAdmission::admit(&state)?;
    tokio::spawn(async move {
        let _admission = admission;
        remove_repo_owned(state, repo_uid).await
    })
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("admin repo removal task panicked: {error}"),
        )
    })?
}

async fn remove_repo_owned(
    state: Arc<AdminState>,
    repo_uid: String,
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
        let job_queue = state.job_queue.clone();
        let db_path = state.db_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(queue) = acquire_job_queue(&job_queue, &db_path) {
                let _ = queue.cancel_repo(&canonical);
            }
        })
        .await;
    }

    // Derive the scheduler name from the config BEFORE removing the config
    // entry — remove_repo_from_config_file deletes the entry, so the name
    // lookup would fail if done afterwards.
    let sched_id = {
        let url_derived = repo_url
            .as_deref()
            .map(nestweaver_engine::pull::repo_name_from_url)
            .unwrap_or_else(|| repo_uid.clone());
        if let Some(ref config_path) = state.config_path {
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
        }
    };

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
    let write_gate = state.write_gate.clone();
    let db_path = state.db_path.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = write_gate
            .as_ref()
            .map(|gate| gate.blocking_lock("admin_remove_repo"));
        run_admin_remove_repo_with(
            &store,
            &db_path,
            &uid,
            |store, uid| {
                store.clear_repo_derived_nodes(uid).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("clear_derived failed: {e}"),
                    )
                })
            },
            |store, uid| {
                store.delete_repo_node(uid).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("delete_repo_node failed: {e}"),
                    )
                })
            },
        )
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    // Remove from live scheduler (using the name derived before config removal).
    if let Some(ref tx) = state.scheduler_tx {
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
        if let Some(ref lock) = state.webhook_allowed_repos
            && let Ok(mut guard) = lock.write()
            && let Some(ref mut set) = *guard
        {
            set.remove(&canonical);
        }
        if let Some(ref lock) = state.webhook_repo_branches
            && let Ok(mut guard) = lock.write()
        {
            guard.remove(&canonical);
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
    let job_queue = state.job_queue.clone();
    let db_path = state.db_path.clone();
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

        let queue = acquire_job_queue(&job_queue, &db_path).map_err(|e| {
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
    let job_queue = state.job_queue.clone();
    let (running_jobs, pending_count): (Option<Vec<serde_json::Value>>, Option<i64>) =
        tokio::task::spawn_blocking(move || match acquire_job_queue(&job_queue, &db_path) {
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
    let job_queue = state.job_queue.clone();
    let db_path = state.db_path.clone();

    let entries = tokio::task::spawn_blocking(move || {
        let queue = acquire_job_queue(&job_queue, &db_path).map_err(|e| {
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
    let job_queue = state.job_queue.clone();
    let db_path = state.db_path.clone();
    let job_id: i64 = id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid job id: {id}")))?;

    let retried = tokio::task::spawn_blocking(move || {
        let queue = acquire_job_queue(&job_queue, &db_path).map_err(|e| {
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
    let job_queue = state.job_queue.clone();
    let db_path = state.db_path.clone();
    let job_id: i64 = id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid job id: {id}")))?;

    let dismissed = tokio::task::spawn_blocking(move || {
        let queue = acquire_job_queue(&job_queue, &db_path).map_err(|e| {
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
    let job_queue = state.job_queue.clone();
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
                let queue = acquire_job_queue(&job_queue, &db_path).ok();

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
                let mut skipped_repos = 0usize;

                // New repos in config but not yet indexed: enqueue.
                for r in &cfg.repos {
                    if !indexed_urls.contains(&r.url) {
                        // Config-sourced repos bypass the add_repo API and its
                        // SSRF validation, so re-run the same synchronous URL
                        // checks here and refuse to enqueue any internal/private
                        // target declared in config. DNS resolution is skipped
                        // (reload must stay non-blocking); literal/encoded
                        // internal IPs and localhost/metadata hosts are still
                        // rejected.
                        if !config_repo_url_allowed(&r.url) {
                            tracing::warn!(
                                url = %r.url,
                                "config reload: skipping repo — URL rejected by SSRF guard"
                            );
                            skipped_repos += 1;
                            continue;
                        }
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
                if skipped_repos > 0 {
                    msg.push_str(&format!(
                        ", {} repos skipped (rejected URL)",
                        skipped_repos
                    ));
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

    // Re-read the config ONCE for both the scheduler and webhook propagation
    // below. Parsing it separately in each block doubled the I/O and could observe
    // two different on-disk versions within a single reload if the file changed
    // mid-handler.
    let reloaded_cfg = state
        .config_path
        .as_ref()
        .and_then(|p| nestweaver_engine::InstanceConfig::from_file(p).ok());

    // Notify the live scheduler so it picks up added/removed repos
    // without a daemon restart.
    if let Some(ref tx) = state.scheduler_tx
        && let Some(cfg) = reloaded_cfg.as_ref()
    {
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
        let new_min_poll = nestweaver_engine::config::parse_duration(&cfg.server.indexing.min_poll);
        let new_max_poll = nestweaver_engine::config::parse_duration(&cfg.server.indexing.max_poll);
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

    // Update webhook state so new/changed repos take effect without restart.
    if let Some(cfg) = reloaded_cfg.as_ref() {
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
    let job_queue = state.job_queue.clone();
    let (pending_count, dead_letter_count, running_count) =
        tokio::task::spawn_blocking(move || {
            let queue = acquire_job_queue(&job_queue, &db_path).ok();
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

    let db_size_bytes = std::fs::metadata(&state.db_path)
        .map(|m| m.len())
        .unwrap_or(0);

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
        db_size_bytes,
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
        connections: Connections {
            grpc: state.active_reads.load(Ordering::Relaxed)
                + state.active_writes.load(Ordering::Relaxed),
            mcp: state.mcp_sessions.load(Ordering::Relaxed),
        },
    })
}

// ── Device-flow authentication (OAuth 2.0 Device Grant, RFC 8628) ──────

/// How long a device grant stays valid before it must be re-requested.
const DEVICE_CODE_TTL_SECS: u64 = 600;
/// Minimum interval (seconds) the client should wait between token polls.
const DEVICE_POLL_INTERVAL_SECS: u64 = 5;
/// Unambiguous alphabet for user codes — omits easily confused characters
/// (0/O, 1/I/L) so codes are easy to read aloud and type.
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Per-IP request budget (per minute) for the unauthenticated `/auth/device`
/// and `/auth/token` endpoints. A legitimate device-flow client polls `/token`
/// every `DEVICE_POLL_INTERVAL_SECS` (~12/min), so this leaves ample headroom
/// while throttling floods.
pub const AUTH_RATE_PER_MIN: u64 = 60;
/// Upper bound on the number of distinct client keys the auth rate limiter
/// tracks. Prevents the limiter map from becoming its own unbounded-growth DoS
/// when an attacker rotates source IPs.
pub const AUTH_RATE_MAX_KEYS: usize = 4096;
/// Max request body accepted on the `/auth` router. Device-flow bodies are a
/// tiny JSON object (`device_code`/`user_code`); 4 KiB is generous.
pub const AUTH_BODY_LIMIT_BYTES: usize = 4096;

struct AuthTokenBucket {
    tokens: f64,
    last_refill: std::time::Instant,
}

/// Bounded, per-client token-bucket rate limiter for the public device-flow
/// endpoints. Mirrors the MCP `HttpRateLimiter` token-bucket math but caps the
/// number of tracked keys so a flood of distinct source IPs can't turn the
/// limiter itself into an unbounded-growth leak (we'd just move the DoS).
///
/// Pure and synchronous so the refill/eviction logic is unit-testable with an
/// injectable clock.
pub struct AuthRateLimiter {
    buckets: std::sync::Mutex<std::collections::HashMap<String, AuthTokenBucket>>,
    capacity: f64,
    refill_per_sec: f64,
    max_keys: usize,
    clock: Arc<dyn Fn() -> std::time::Instant + Send + Sync>,
}

impl AuthRateLimiter {
    pub fn new(requests_per_min: u64, max_keys: usize) -> Self {
        Self::new_with_clock(
            requests_per_min,
            max_keys,
            Arc::new(std::time::Instant::now),
        )
    }

    fn new_with_clock(
        requests_per_min: u64,
        max_keys: usize,
        clock: Arc<dyn Fn() -> std::time::Instant + Send + Sync>,
    ) -> Self {
        Self {
            buckets: std::sync::Mutex::new(std::collections::HashMap::new()),
            capacity: requests_per_min as f64,
            refill_per_sec: requests_per_min as f64 / 60.0,
            max_keys,
            clock,
        }
    }

    /// Consume one token for `key`. Returns `true` if the request is allowed.
    ///
    /// When the tracked-key cap is reached and `key` is new, fully-refilled
    /// (idle) buckets are evicted first; if the map is still full the request is
    /// rejected rather than inserting an unbounded new key.
    pub fn check(&self, key: &str) -> bool {
        let now = (self.clock)();
        let mut buckets = self.buckets.lock().unwrap();

        if buckets.len() >= self.max_keys && !buckets.contains_key(key) {
            // Drop buckets that have fully refilled — they carry no state worth
            // keeping and freeing them keeps the map bounded.
            let cap = self.capacity;
            let refill = self.refill_per_sec;
            buckets.retain(|_, b| {
                let elapsed = now.duration_since(b.last_refill).as_secs_f64();
                (b.tokens + elapsed * refill) < cap
            });
            if buckets.len() >= self.max_keys {
                return false;
            }
        }

        let bucket = buckets
            .entry(key.to_string())
            .or_insert_with(|| AuthTokenBucket {
                tokens: self.capacity,
                last_refill: now,
            });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Derive the rate-limit key for an inbound `/auth` request.
///
/// Prefers the direct peer address (`ConnectInfo`, unspoofable) when the server
/// wired it; otherwise falls back to a reverse-proxy-supplied client IP
/// (`X-Forwarded-For`/`X-Real-IP`); if neither is available the key collapses to
/// a single global bucket, which degrades the per-IP limit to a global rate cap
/// on `/auth` (the documented fallback when no peer-addr source exists).
fn auth_rate_limit_key(req: &axum::extract::Request) -> String {
    if let Some(ci) = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
    {
        return format!("ip:{}", ci.0.ip());
    }
    if let Some(ip) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("xff:{ip}");
    }
    if let Some(ip) = req
        .headers()
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("xrip:{ip}");
    }
    "global".to_string()
}

/// Axum middleware enforcing [`AuthRateLimiter`] on the public device-flow
/// endpoints. On rejection, the token endpoint returns the RFC 8628 `slow_down`
/// error (so polling clients back off); other endpoints get a plain 429.
pub async fn auth_rate_limit(
    limiter: Arc<AuthRateLimiter>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let key = auth_rate_limit_key(&req);
    if !limiter.check(&key) {
        if req.uri().path().ends_with("/token") {
            // RFC 8628 §3.5: tell polling clients to slow down.
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(DeviceErrorResponse {
                    error: "slow_down".to_string(),
                }),
            )
                .into_response();
        }
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded; slow down".to_string(),
        )
            .into_response();
    }
    next.run(req).await
}

#[derive(Serialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Deserialize)]
pub struct DeviceTokenRequest {
    pub device_code: String,
}

#[derive(Serialize)]
pub struct DeviceTokenResponse {
    pub access_token: String,
}

#[derive(Serialize)]
pub struct DeviceErrorResponse {
    pub error: String,
}

#[derive(Deserialize)]
pub struct DeviceApproveRequest {
    pub user_code: String,
}

/// Number of characters in a generated user code.
const USER_CODE_LEN: usize = 8;
/// Hard cap on concurrently-pending device grants. The map is also TTL-pruned,
/// but the cap bounds memory against an unauthenticated flood on `/auth/device`
/// (the endpoint is public, so without this it could grow without bound).
const MAX_PENDING_DEVICES: usize = 1024;
/// Bound on how many times we re-roll a colliding `user_code` before giving up.
/// With a 30^8 space and ≤1024 pending grants, a single roll almost never
/// collides; the cap just guarantees termination.
const USER_CODE_MAX_ATTEMPTS: usize = 16;

/// Generate a short, human-readable user code (8 chars from an unambiguous
/// uppercase-alnum alphabet). Randomness comes from v4 UUIDs (getrandom-backed)
/// so we don't pull in an extra RNG dependency.
///
/// Bytes are mapped to the alphabet by **rejection sampling**, not `% len`: the
/// alphabet has 30 symbols and 256 is not a multiple of 30, so a plain modulo
/// would bias the first 16 symbols. We discard any byte ≥ the largest multiple
/// of the alphabet length that fits in a `u8` (240), leaving a uniform mapping.
fn generate_user_code() -> String {
    let alpha_len = USER_CODE_ALPHABET.len() as u16; // 30
    // Largest multiple of the alphabet length that fits in a u8 (240). Bytes at
    // or above this are rejected to avoid modulo bias.
    let reject_threshold = (256 / alpha_len * alpha_len) as u8;

    let mut out = String::with_capacity(USER_CODE_LEN);
    while out.len() < USER_CODE_LEN {
        // Pull a fresh batch of CSPRNG bytes; UUID v4 is getrandom-backed.
        for &b in uuid::Uuid::new_v4().into_bytes().iter() {
            if out.len() >= USER_CODE_LEN {
                break;
            }
            if b < reject_threshold {
                out.push(USER_CODE_ALPHABET[(b % alpha_len as u8) as usize] as char);
            }
        }
    }
    out
}

/// Generate a `user_code` that is unique among the currently-pending grants
/// (compared canonically). Returns `None` if a unique code couldn't be found
/// within `USER_CODE_MAX_ATTEMPTS` rolls (practically impossible at our cap).
fn generate_unique_user_code(
    map: &std::collections::HashMap<String, PendingDevice>,
) -> Option<String> {
    let taken: std::collections::HashSet<String> = map
        .values()
        .map(|p| normalize_user_code(&p.user_code))
        .collect();
    for _ in 0..USER_CODE_MAX_ATTEMPTS {
        let code = generate_user_code();
        if !taken.contains(&normalize_user_code(&code)) {
            return Some(code);
        }
    }
    None
}

/// Canonicalize a user code for comparison: uppercase, keep only alphanumerics
/// (so an admin can paste `WDJB-MJHT` or `wdjb mjht` and still match).
fn normalize_user_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Drop expired grants so the pending map can't grow without bound.
fn prune_expired(map: &mut std::collections::HashMap<String, PendingDevice>) {
    let now = std::time::Instant::now();
    map.retain(|_, v| v.expires_at > now);
}

/// Build an RFC 8628 token-endpoint error response (`400` + `{ "error": ... }`).
fn device_error(error: &str) -> (StatusCode, Json<DeviceErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(DeviceErrorResponse {
            error: error.to_string(),
        }),
    )
}

/// Derive the externally-visible base URL of this server from request headers,
/// honoring a reverse-proxy `X-Forwarded-Proto`. Used to build the verification
/// URIs handed back to the developer.
fn verification_base(headers: &axum::http::HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// POST /auth/device — start a device-authorization grant (no auth).
///
/// Returns a `device_code` (opaque) and a `user_code` (shown to the developer),
/// along with the verification URIs and polling parameters per RFC 8628 §3.2.
pub async fn device_authorize(
    State(state): State<Arc<AdminState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<DeviceAuthResponse>, (StatusCode, String)> {
    let device_code = uuid::Uuid::new_v4().to_string();

    let expires_at =
        std::time::Instant::now() + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS);

    let user_code = {
        let mut map = state.device_flow.write().await;
        prune_expired(&mut map);

        // Bound the pending map: the endpoint is unauthenticated, so without a
        // cap a flood could grow it without limit (TTL pruning alone lags). Once
        // pruning can't free a slot, shed load rather than grow.
        if map.len() >= MAX_PENDING_DEVICES {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "device authorization capacity reached; retry later".to_string(),
            ));
        }

        // Pick a code that doesn't collide with another pending grant, so an
        // admin approving a code can never match two devices.
        let Some(code) = generate_unique_user_code(&map) else {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "could not allocate a unique user code; retry later".to_string(),
            ));
        };
        map.insert(
            device_code.clone(),
            PendingDevice {
                user_code: code.clone(),
                expires_at,
                approved_token: None,
            },
        );
        code
    };

    let base = verification_base(&headers);
    let verification_uri = format!("{base}/admin/device-approve");
    let verification_uri_complete = format!("{base}/admin/device-approve?user_code={user_code}");

    Ok(Json(DeviceAuthResponse {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
        expires_in: DEVICE_CODE_TTL_SECS,
        interval: DEVICE_POLL_INTERVAL_SECS,
    }))
}

/// POST /auth/token — exchange a `device_code` for the granted token (no auth).
///
/// RFC 8628 §3.5: unknown/expired → `expired_token`; pending approval →
/// `authorization_pending`; approved → `200 { access_token }` (one-shot).
pub async fn device_token(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<DeviceTokenRequest>,
) -> Result<Json<DeviceTokenResponse>, (StatusCode, Json<DeviceErrorResponse>)> {
    let mut map = state.device_flow.write().await;
    prune_expired(&mut map);

    // After pruning, a missing entry means it was never issued or has expired.
    let Some(entry) = map.get(&req.device_code) else {
        return Err(device_error("expired_token"));
    };

    match entry.approved_token.clone() {
        None => Err(device_error("authorization_pending")),
        Some(token) => {
            // Single use: remove the grant once the token is handed out.
            map.remove(&req.device_code);
            Ok(Json(DeviceTokenResponse {
                access_token: token,
            }))
        }
    }
}

/// POST /auth/device/approve — admin approves a pending grant (admin auth).
///
/// Looks up the pending grant by `user_code` and attaches the configured org
/// query token (org-wide read token per the security model). The developer's
/// next `POST /auth/token` then succeeds.
pub async fn device_approve(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Json(req): Json<DeviceApproveRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let wanted = normalize_user_code(&req.user_code);
    if wanted.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "user_code required".to_string()));
    }
    // Refuse to hand out an empty token: when the server has no query token
    // configured, approval would otherwise grant `""`, silently authenticating
    // the developer as the empty (no-auth) principal.
    //
    // 409 Conflict (vs. 503): this is a misconfiguration of the approval target,
    // not a transient outage — retrying without reconfiguring the server's query
    // token will never succeed, so a 4xx is the honest class. Kept as 409 to
    // match the ambiguous-user_code conflict below and the existing test.
    let Some(granted) = state.auth_token.clone().filter(|t| !t.is_empty()) else {
        return Err((
            StatusCode::CONFLICT,
            "server has no query token configured; device flow unavailable".to_string(),
        ));
    };

    let mut map = state.device_flow.write().await;
    prune_expired(&mut map);

    // Collect every grant whose code matches. `user_code`s are generated to be
    // unique among pending grants, so >1 match means an invariant broke; treat
    // it as an error rather than approving an arbitrary device.
    let matched: Vec<String> = map
        .iter()
        .filter(|(_, entry)| normalize_user_code(&entry.user_code) == wanted)
        .map(|(device_code, _)| device_code.clone())
        .collect();

    match matched.as_slice() {
        [] => Err((
            StatusCode::NOT_FOUND,
            "no pending device with that code".to_string(),
        )),
        [device_code] => {
            if let Some(entry) = map.get_mut(device_code) {
                entry.approved_token = Some(granted);
            }
            Ok(Json(MessageResponse {
                message: "device approved".to_string(),
            }))
        }
        _ => Err((
            StatusCode::CONFLICT,
            "ambiguous user_code: multiple pending grants match".to_string(),
        )),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::{
        Router,
        routing::{delete, get, post},
    };
    use tower::ServiceExt;

    fn test_admin_state() -> Arc<AdminState> {
        admin_state_with_auth(Some("test-query-token".to_string()))
    }

    #[test]
    fn admin_mutation_admission_is_shutdown_visible_and_fail_closed() {
        let state = test_admin_state();
        let admission = AdminMutationAdmission::admit(&state).expect("admit before shutdown");
        assert_eq!(state.active_writes.load(Ordering::SeqCst), 1);

        state.shutdown_started.store(true, Ordering::SeqCst);
        let error = AdminMutationAdmission::admit(&state)
            .err()
            .expect("new admin mutation must be refused after shutdown");
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(state.active_writes.load(Ordering::SeqCst), 1);

        drop(admission);
        assert_eq!(state.active_writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_remove_request_keeps_admission_until_blocked_writer_finishes() {
        let gate = nestweaver_engine::write_gate::WriteGate::new();
        let blocker = gate.lock("test_block_admin_remove").await;
        let mut state = test_admin_state();
        Arc::get_mut(&mut state)
            .expect("test owns the only state Arc")
            .write_gate = Some(gate.clone());
        let active_writes = Arc::clone(&state.active_writes);
        let app = Router::new()
            .route("/admin/api/repos/{id}", delete(remove_repo))
            .with_state(state);

        let request = tokio::spawn(
            app.oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/admin/api/repos/missing-repo")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while gate.waiting() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admin graph worker must block on the held write gate");
        assert_eq!(active_writes.load(Ordering::SeqCst), 1);

        request.abort();
        let _ = request.await;
        tokio::task::yield_now().await;
        assert_eq!(
            active_writes.load(Ordering::SeqCst),
            1,
            "request cancellation must not release the mutation admission"
        );
        assert_eq!(gate.waiting(), 1, "the owned mutation must still be alive");

        drop(blocker);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while active_writes.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned mutation must finish and release admission");
        assert_eq!(gate.waiting(), 0);
    }

    fn admin_state_with_auth(auth_token: Option<String>) -> Arc<AdminState> {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.lbug");
        let store =
            nestweaver_store::GraphStore::open_or_create(&db_path).expect("open test store");
        let db_path_clone = db_path.clone();
        // Leak the tempdir so it lives as long as the store.
        std::mem::forget(dir);
        Arc::new(AdminState {
            admin_token: "test-admin-token".to_string(),
            auth_token,
            device_flow: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            daemon_store: Arc::new(store),
            tantivy: None,
            instance_id: "test".to_string(),
            start_time: std::time::Instant::now(),
            active_reads: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            active_writes: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            shutdown_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mcp_sessions: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            drained: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            indexing_queue_depth: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            db_path: db_path_clone,
            config_path: None,
            scheduler_tx: None,
            webhook_allowed_repos: None,
            webhook_repo_branches: None,
            write_gate: None,
            job_queue: None,
        })
    }

    #[test]
    fn acquire_job_queue_uses_shared_connection_when_wired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.lbug");
        let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
        let shared = Arc::new(Mutex::new(
            nestweaver_engine::jobs::JobQueue::open(&jobs_path).expect("open jobs"),
        ));
        // Enqueue through the shared connection.
        {
            let q = shared.lock().unwrap();
            q.upsert(
                "repo1",
                "https://example.com/repo1",
                nestweaver_engine::jobs::JobTrigger::Unindexed,
                None,
            )
            .expect("upsert");
        }
        let opt = Some(Arc::clone(&shared));

        let handle = acquire_job_queue(&opt, &db_path).expect("acquire");
        // Must return the SHARED variant — never open a second connection that
        // would race the worker's WAL checkpoint (the SIGBUS regression).
        assert!(matches!(handle, JobQueueHandle::Shared(_)));
        // And it operates on the same data enqueued above.
        assert_eq!(handle.queue_depth().expect("depth").pending, 1);
    }

    #[test]
    fn acquire_job_queue_falls_back_when_not_wired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.lbug");
        let handle = acquire_job_queue(&None, &db_path).expect("acquire");
        assert!(matches!(handle, JobQueueHandle::Owned(_)));
        assert_eq!(handle.queue_depth().expect("depth").pending, 0);
    }

    fn test_router() -> Router {
        let state = test_admin_state();
        Router::new()
            .route("/admin/api/status", get(get_status))
            .route("/admin/api/repos", get(list_repos))
            .route("/admin/api/queue", get(get_queue))
            .route("/admin/api/drain/status", get(drain_status))
            .route("/admin/api/metrics", get(metrics))
            .with_state(state)
    }

    #[tokio::test]
    async fn metrics_requires_admin_token() {
        // The admin router is nested onto the network MCP listener, so
        // /admin/api/metrics must not leak operational counters unauthenticated.
        crate::routes::metrics::init_metrics();
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn metrics_with_valid_admin_token() {
        crate::routes::metrics::init_metrics();
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/metrics")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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
            auth_token: Some("test-query-token".to_string()),
            device_flow: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            daemon_store: Arc::new(store),
            tantivy: None,
            instance_id: "test".to_string(),
            start_time: std::time::Instant::now(),
            active_reads: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            active_writes: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            shutdown_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mcp_sessions: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            drained: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            indexing_queue_depth: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            db_path,
            config_path: Some(config_path.clone()),
            scheduler_tx: None,
            webhook_allowed_repos: None,
            webhook_repo_branches: None,
            write_gate: None,
            job_queue: None,
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

    #[tokio::test]
    async fn remove_repo_finalizes_code_state_without_rebuilding_vault_search() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("main.js"),
            "function admin_delete_target() { return 1; }",
        )
        .unwrap();

        let repo_url = "https://example.com/admin-delete";
        nestweaver_engine::index_directory(&src, &db_path, "test", repo_url, "sha-1").unwrap();
        let repo_uid = nestweaver_schema::repo_uid("test", repo_url);
        let store = Arc::new(
            nestweaver_store::GraphStore::open_or_create(&db_path).expect("open test store"),
        );

        let pagerank_path = nestweaver_engine::sidecar_path(&db_path, ".pagerank.json");
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let removed_symbol_uid = store
            .symbols_in_file("main.js")
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .uid;
        assert!(
            store
                .pagerank_scores()
                .unwrap()
                .contains_key(&removed_symbol_uid)
        );
        store.set_embedding_metadata("test-model", 2).unwrap();
        assert!(store.add_embedding(&removed_symbol_uid, vec![1.0, 0.0]));
        store.flush_embedding_index().unwrap();

        assert!(
            nestweaver_engine::load_manifest_cache_for_db(&store, &db_path)
                .unwrap()
                .contains_key(&repo_uid)
        );

        let filemeta_path = nestweaver_engine::sidecar_path(&db_path, ".filemeta.json");
        assert!(
            nestweaver_engine::load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(&repo_uid)
        );
        let parsed_cache_path = nestweaver_engine::sidecar_path(&db_path, ".parsed_cache.bin");
        let parsed_cache_before = std::fs::read(&parsed_cache_path).unwrap();

        let resolution_deps_path =
            nestweaver_engine::sidecar_path(&db_path, ".resolution_deps.bin");
        let mut deps = nestweaver_engine::resolution_cache::ResolutionDeps::default();
        deps.set_deps_for_repo(
            &repo_uid,
            "main.js",
            ["dependency.js".to_string()].into_iter().collect(),
        );
        deps.save(&resolution_deps_path).unwrap();

        let tantivy = Arc::new(
            nestweaver_store::TantivyIndex::open_or_create(&dir.path().join("tantivy")).unwrap(),
        );
        // Seed an unrelated vault document. Repo deletion changes no indexed
        // vault document kind, so the admin path must leave Tantivy untouched.
        tantivy
            .update_note(
                "note:stale-admin-delete",
                "admin_delete_target",
                "vault:test",
                &["admin_delete_target".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        assert!(
            !tantivy
                .search("admin_delete_target", 10)
                .unwrap()
                .is_empty()
        );

        let generation_before = store.graph_generation();
        let state = Arc::new(AdminState {
            admin_token: "test-admin-token".to_string(),
            auth_token: Some("test-query-token".to_string()),
            device_flow: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            daemon_store: Arc::clone(&store),
            tantivy: Some(Arc::clone(&tantivy)),
            instance_id: "test".to_string(),
            start_time: std::time::Instant::now(),
            active_reads: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            active_writes: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            shutdown_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mcp_sessions: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            drained: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            indexing_queue_depth: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            db_path: db_path.clone(),
            config_path: None,
            scheduler_tx: None,
            webhook_allowed_repos: None,
            webhook_repo_branches: None,
            write_gate: None,
            job_queue: None,
        });
        let app = Router::new()
            .route("/admin/api/repos/{id}", delete(remove_repo))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/admin/api/repos/{repo_uid}"))
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        assert!(
            store.graph_generation() > generation_before,
            "admin deletion must publish a new graph generation"
        );
        assert!(
            !nestweaver_engine::load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(&repo_uid),
            "admin deletion must remove the repo-scoped filemeta slice"
        );
        assert!(
            nestweaver_engine::resolution_cache::ResolutionDeps::load(&resolution_deps_path)
                .is_empty_for_repo(&repo_uid),
            "admin deletion must remove the repo-scoped resolution-deps slice"
        );
        assert_eq!(
            std::fs::read(&parsed_cache_path).unwrap(),
            parsed_cache_before,
            "the content-hash-keyed parsed cache is intentionally retained"
        );
        assert!(
            !pagerank_path.exists(),
            "admin deletion must remove the persisted PageRank cache"
        );
        assert!(
            !store
                .pagerank_scores()
                .unwrap()
                .contains_key(&removed_symbol_uid),
            "admin deletion must invalidate the live PageRank cache"
        );
        assert!(
            !tantivy
                .search("admin_delete_target", 10)
                .unwrap()
                .is_empty(),
            "code-only admin deletion must not rebuild unrelated vault search"
        );
        assert!(
            !nestweaver_engine::load_manifest_cache_for_db(&store, &db_path)
                .unwrap()
                .contains_key(&repo_uid),
            "admin deletion must remove the deleted repo manifest"
        );
        assert!(
            !store.has_embedding(&removed_symbol_uid),
            "admin deletion must remove the deleted symbol embedding"
        );
    }

    // ── Device flow ─────────────────────────────────────────────────────

    #[test]
    fn admin_remove_repo_late_failure_finalizes_committed_children() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        let repo_uid = "repo:test:web-late-remove";
        let file_uid = nestweaver_schema::file_uid(repo_uid, "src/lib.rs");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: repo_uid.to_string(),
                url: "https://example.test/web-late-remove".to_string(),
                indexed_sha: "sha".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_file(&nestweaver_schema::File {
                uid: file_uid.clone(),
                path: "src/lib.rs".to_string(),
                repo_uid: repo_uid.to_string(),
                content_hash: "hash".to_string(),
            })
            .unwrap();

        let filemeta_path = nestweaver_engine::sidecar_path(&db_path, ".filemeta.json");
        let mut filemeta = nestweaver_engine::load_filemeta_sidecar(&filemeta_path);
        filemeta.repos.entry(repo_uid.to_string()).or_default();
        nestweaver_engine::save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();
        let deps_path = nestweaver_engine::sidecar_path(&db_path, ".resolution_deps.bin");
        let mut deps = nestweaver_engine::resolution_cache::ResolutionDeps::default();
        deps.set_deps_for_repo(
            repo_uid,
            "src/lib.rs",
            ["src/dep.rs".to_string()].into_iter().collect(),
        );
        deps.save(&deps_path).unwrap();

        let pagerank_path = nestweaver_engine::sidecar_path(&db_path, ".pagerank.json");
        store
            .compute_pagerank(0.85, 20, &nestweaver_store::GraphScope::code_only())
            .unwrap();
        store.save_pagerank_cache(&pagerank_path).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let tantivy =
            nestweaver_store::TantivyIndex::open_or_create(&dir.path().join("tantivy")).unwrap();
        tantivy
            .update_note(
                "note:web-late-remove",
                "web_late_remove_search_sentinel",
                "vault:test",
                &["web_late_remove_search_sentinel".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        let generation_before = store.graph_generation();

        let error = run_admin_remove_repo_with(
            &store,
            &db_path,
            repo_uid,
            |store, uid| {
                store.clear_repo_derived_nodes(uid).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("clear_derived failed: {e}"),
                    )
                })
            },
            |_store, _uid| {
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "injected repo-node failure".to_string(),
                ))
            },
        )
        .unwrap_err();

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            store.list_files_by_repo(repo_uid).unwrap().is_empty(),
            "precondition: the first delete transaction must have committed"
        );
        assert!(
            store.lookup_repo(repo_uid).unwrap().is_some(),
            "precondition: the injected late failure leaves the Repo row"
        );
        assert!(store.graph_generation() > generation_before);
        assert!(
            !nestweaver_engine::load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(repo_uid)
        );
        assert!(
            nestweaver_engine::resolution_cache::ResolutionDeps::load(&deps_path)
                .is_empty_for_repo(repo_uid)
        );
        assert!(!pagerank_path.exists());
        assert!(!store.pagerank_scores().unwrap().contains_key(&file_uid));
        assert!(
            !tantivy
                .search("web_late_remove_search_sentinel", 10)
                .unwrap()
                .is_empty(),
            "code-only deletion must not rebuild unrelated vault search documents"
        );
    }

    #[test]
    fn admin_remove_repo_preserves_real_mutation_and_reconciliation_failures() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        let repo_uid = "repo:test:web-real-combined-error";
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: repo_uid.to_string(),
                url: "https://example.test/web-real-combined-error".to_string(),
                indexed_sha: "sha".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_file(&nestweaver_schema::File {
                uid: nestweaver_schema::file_uid(repo_uid, "src/lib.rs"),
                path: "src/lib.rs".to_string(),
                repo_uid: repo_uid.to_string(),
                content_hash: "hash".to_string(),
            })
            .unwrap();
        let generation_path = nestweaver_engine::sidecar_path(&db_path, ".generation");
        std::fs::create_dir(&generation_path).unwrap();
        let generation_before = store.graph_generation();

        let error = run_admin_remove_repo_with(
            &store,
            &db_path,
            repo_uid,
            |store, uid| {
                store.clear_repo_derived_nodes(uid).map_err(|error| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("clear derived: {error}"),
                    )
                })
            },
            |_store, _uid| {
                Err((
                    StatusCode::CONFLICT,
                    "real committed admin mutation failure".to_string(),
                ))
            },
        )
        .unwrap_err();

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(error.1.contains("real committed admin mutation failure"));
        assert!(error.1.contains("generation-persistence"));
        assert!(store.list_files_by_repo(repo_uid).unwrap().is_empty());
        assert!(store.lookup_repo(repo_uid).unwrap().is_some());
        assert!(store.graph_generation() > generation_before);
    }

    fn device_router(state: Arc<AdminState>) -> Router {
        Router::new()
            .route("/auth/device", post(device_authorize))
            .route("/auth/token", post(device_token))
            .route("/auth/device/approve", post(device_approve))
            .with_state(state)
    }

    async fn post_json(
        app: &Router,
        uri: &str,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("Content-Type", "application/json");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {t}"));
        }
        let resp = app
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[test]
    fn generate_user_code_is_eight_unambiguous_chars() {
        let code = generate_user_code();
        assert_eq!(code.len(), 8);
        assert!(
            code.bytes().all(|b| USER_CODE_ALPHABET.contains(&b)),
            "code {code} contains chars outside the alphabet"
        );
    }

    #[test]
    fn normalize_user_code_strips_separators_and_uppercases() {
        assert_eq!(normalize_user_code("wdjb-mjht"), "WDJBMJHT");
        assert_eq!(normalize_user_code(" ab cd "), "ABCD");
    }

    #[tokio::test]
    async fn device_flow_request_pending_approve_token() {
        let app = device_router(test_admin_state());

        // 1. Request a device code.
        let (status, auth) = post_json(&app, "/auth/device", None, "{}").await;
        assert_eq!(status, StatusCode::OK);
        let device_code = auth["device_code"].as_str().unwrap().to_string();
        let user_code = auth["user_code"].as_str().unwrap().to_string();
        assert_eq!(auth["expires_in"], 600);
        assert_eq!(auth["interval"], 5);
        assert!(
            auth["verification_uri_complete"]
                .as_str()
                .unwrap()
                .contains(&user_code)
        );

        // 2. Polling before approval → authorization_pending.
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            &format!(r#"{{"device_code":"{device_code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "authorization_pending");

        // 3. Admin approves the user code.
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            &format!(r#"{{"user_code":"{user_code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // 4. Polling after approval → access token (the org query token).
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            &format!(r#"{{"device_code":"{device_code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["access_token"], "test-query-token");

        // 5. The grant is single-use: a second poll fails as expired.
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            &format!(r#"{{"device_code":"{device_code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "expired_token");
    }

    #[tokio::test]
    async fn device_token_unknown_code_is_expired() {
        let app = device_router(test_admin_state());
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            r#"{"device_code":"does-not-exist"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "expired_token");
    }

    #[tokio::test]
    async fn device_token_after_expiry_is_expired() {
        let state = test_admin_state();
        // Insert a grant that already expired and was approved — pruning must
        // still treat it as expired.
        {
            let mut map = state.device_flow.write().await;
            map.insert(
                "expired-code".to_string(),
                PendingDevice {
                    user_code: "ABCD1234".to_string(),
                    expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
                    approved_token: Some("test-query-token".to_string()),
                },
            );
        }
        let app = device_router(state);
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            r#"{"device_code":"expired-code"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "expired_token");
    }

    #[tokio::test]
    async fn device_approve_requires_admin_token() {
        let app = device_router(test_admin_state());
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            None,
            r#"{"user_code":"ABCD1234"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn device_approve_unknown_code_is_not_found() {
        let app = device_router(test_admin_state());
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            r#"{"user_code":"NOSUCHCODE"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn device_authorize_rejects_past_capacity() {
        let state = test_admin_state();
        // Fill the pending map to capacity with non-expired grants.
        {
            let mut map = state.device_flow.write().await;
            for i in 0..MAX_PENDING_DEVICES {
                map.insert(
                    format!("code-{i}"),
                    PendingDevice {
                        user_code: format!("USERCODE{i}"),
                        expires_at: std::time::Instant::now()
                            + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                        approved_token: None,
                    },
                );
            }
        }
        let app = device_router(state.clone());
        let (status, _) = post_json(&app, "/auth/device", None, "{}").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        // The map must not have grown past the cap.
        assert_eq!(state.device_flow.read().await.len(), MAX_PENDING_DEVICES);
    }

    #[tokio::test]
    async fn device_approve_rejected_when_no_query_token_configured() {
        let state = admin_state_with_auth(None);
        // Seed a pending grant so the failure is the missing token, not a miss.
        {
            let mut map = state.device_flow.write().await;
            map.insert(
                "dev-code".to_string(),
                PendingDevice {
                    user_code: "ABCD2345".to_string(),
                    expires_at: std::time::Instant::now()
                        + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                    approved_token: None,
                },
            );
        }
        let app = device_router(state.clone());
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            r#"{"user_code":"ABCD2345"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // The grant must remain unapproved (no empty token granted).
        let map = state.device_flow.read().await;
        assert!(map.get("dev-code").unwrap().approved_token.is_none());
    }

    #[tokio::test]
    async fn device_approve_empty_string_token_also_rejected() {
        // A configured-but-empty token is as unusable as None; reject it too.
        let state = admin_state_with_auth(Some(String::new()));
        {
            let mut map = state.device_flow.write().await;
            map.insert(
                "dev-code".to_string(),
                PendingDevice {
                    user_code: "ABCD2345".to_string(),
                    expires_at: std::time::Instant::now()
                        + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                    approved_token: None,
                },
            );
        }
        let app = device_router(state);
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            r#"{"user_code":"ABCD2345"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[test]
    fn generate_user_code_is_unbiased_and_within_alphabet() {
        // Sample many codes: every byte must be in the alphabet, and across a
        // large sample every alphabet symbol should appear (a biased mapping
        // would still stay in-alphabet, so the spread check guards the sampler).
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2000 {
            let code = generate_user_code();
            assert_eq!(code.len(), USER_CODE_LEN);
            for b in code.bytes() {
                assert!(
                    USER_CODE_ALPHABET.contains(&b),
                    "byte {b} outside the user-code alphabet"
                );
                seen.insert(b);
            }
        }
        assert_eq!(
            seen.len(),
            USER_CODE_ALPHABET.len(),
            "some alphabet symbols never appeared — distribution looks skewed"
        );
    }

    #[test]
    fn generate_unique_user_code_avoids_collisions() {
        let mut map = std::collections::HashMap::new();
        for i in 0..256 {
            let code = generate_unique_user_code(&map)
                .expect("should always find a unique code at this size");
            map.insert(
                format!("device-{i}"),
                PendingDevice {
                    user_code: code,
                    expires_at: std::time::Instant::now()
                        + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                    approved_token: None,
                },
            );
        }
        let distinct: std::collections::HashSet<String> = map
            .values()
            .map(|p| normalize_user_code(&p.user_code))
            .collect();
        assert_eq!(distinct.len(), map.len());
    }

    #[tokio::test]
    async fn device_approve_ambiguous_user_code_is_conflict() {
        // Two pending grants sharing a code (an invariant break) must not be
        // silently approved — the handler reports a conflict.
        let state = test_admin_state();
        {
            let mut map = state.device_flow.write().await;
            for code in ["dev-a", "dev-b"] {
                map.insert(
                    code.to_string(),
                    PendingDevice {
                        user_code: "DUPCODE9".to_string(),
                        expires_at: std::time::Instant::now()
                            + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                        approved_token: None,
                    },
                );
            }
        }
        let app = device_router(state);
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            r#"{"user_code":"DUPCODE9"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[test]
    fn auth_rate_limiter_throttles_and_stays_bounded() {
        use std::cell::Cell;
        use std::time::{Duration, Instant};

        // Frozen clock so refill is deterministic.
        let start = Instant::now();
        thread_local! {
            static NOW: Cell<Option<Instant>> = const { Cell::new(None) };
        }
        NOW.with(|n| n.set(Some(start)));
        let clock = Arc::new(|| NOW.with(|n| n.get().unwrap()));

        let limiter = AuthRateLimiter::new_with_clock(3, 2, clock);

        // Same key: first 3 allowed, 4th rejected (bucket empty, clock frozen).
        assert!(limiter.check("ip:1.2.3.4"));
        assert!(limiter.check("ip:1.2.3.4"));
        assert!(limiter.check("ip:1.2.3.4"));
        assert!(!limiter.check("ip:1.2.3.4"));

        // After enough time the bucket refills.
        NOW.with(|n| n.set(Some(start + Duration::from_secs(60))));
        assert!(limiter.check("ip:1.2.3.4"));

        // Key-cap bound: with two saturated keys, a third distinct key is
        // rejected rather than growing the map without bound.
        let bounded = AuthRateLimiter::new(3, 2);
        for key in ["ip:a", "ip:b"] {
            assert!(bounded.check(key));
            assert!(bounded.check(key));
            assert!(bounded.check(key)); // drains each bucket
        }
        assert!(!bounded.check("ip:c"));
    }
}
