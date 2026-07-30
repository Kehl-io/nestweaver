//! gRPC server implementation for the NestWeaver daemon.
//!
//! Binds to a Unix domain socket and dispatches read RPCs through the
//! existing MCP tool dispatch layer, avoiding any duplication of
//! business logic.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use nestweaver_proto::nest_weaver_daemon_server::{NestWeaverDaemon, NestWeaverDaemonServer};
use nestweaver_proto::*;
use nestweaver_store::{GraphStore, TantivyIndex};
use tokio::sync::Notify;
use tonic::codegen::http;
use tonic::{Request, Response, Status};

use crate::lifecycle;
use crate::safeguards::{
    ClientRateLimiters, QuerySafeguards, RateLimitConfig, with_safeguard_cancellable,
};

// ── State ───────────────────────────────────────────────────────────

/// Map a dispatch error to a gRPC `Status`, preserving cancellation semantics:
/// a cancelled query surfaces as `deadline_exceeded` rather than an opaque
/// `internal`. Non-cancel errors keep the `internal` mapping. This is
/// defense-in-depth: on timeout the safeguard's `select!` already returns
/// `deadline_exceeded` and drops this future, but a query that finishes with a
/// cancel error just before that race is mapped consistently here too.
///
/// The only cancel reason that can reach here is `Timeout`: the cooperative
/// flag is a bare `AtomicBool` that cannot carry a reason, so the leaf always
/// reports `Timeout`. On a client disconnect the request future is dropped
/// before any `Status` is returned, so that path never surfaces here.
fn dispatch_err_to_status(tool_name: &str, e: anyhow::Error) -> Status {
    if let Some(reason) = e
        .downcast_ref::<nestweaver_store::StoreError>()
        .and_then(|s| s.cancel_reason())
    {
        return match reason {
            nestweaver_store::CancelReason::Timeout => {
                Status::deadline_exceeded(format!("{tool_name} query cancelled: timeout"))
            }
        };
    }
    Status::internal(format!("tool {tool_name} failed: {e}"))
}

/// Trip `cancel` when the request future is dropped (client cancel /
/// disconnect), reusing the same cooperative flag the timeout path sets.
///
/// Returns a `DropGuard` that MUST be held for the lifetime of the request
/// future: when that future is dropped, the guard cancels a token, and a small
/// listener task then stores `true` into the shared flag so the in-flight
/// `spawn_blocking` dispatch bails cheaply instead of running to completion and
/// caching a result no client is waiting for. On normal completion the guard
/// still fires, but the dispatch has already returned, so the late store is a
/// harmless no-op on a per-request flag.
fn arm_disconnect_cancel(cancel: Arc<AtomicBool>) -> tokio_util::sync::DropGuard {
    let token = tokio_util::sync::CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        child.cancelled().await;
        // Release to pair with the Acquire load in the cooperative reader
        // (nestweaver-store `vector_search_cancellable`), matching the timeout
        // writer in `safeguards::with_safeguard_cancellable`.
        cancel.store(true, Ordering::Release);
    });
    token.drop_guard()
}

/// RAII guard that decrements a connection counter on drop.
/// Fixes cancellation-safety: if a client disconnects mid-RPC or
/// the async task is cancelled, the counter is still decremented.
struct ConnectionGuard {
    counter: Arc<AtomicU32>,
}

impl ConnectionGuard {
    fn read(state: &DaemonState) -> Self {
        state.active_reads.fetch_add(1, Ordering::Relaxed);
        state.idle_notify.notify_one();
        Self {
            counter: Arc::clone(&state.active_reads),
        }
    }

    fn write(state: &DaemonState) -> Self {
        state.active_writes.fetch_add(1, Ordering::Relaxed);
        state.idle_notify.notify_one();
        Self {
            counter: Arc::clone(&state.active_writes),
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Shared state held by the daemon process.
#[derive(Clone)]
enum SearchIndexReconciliation {
    Disabled,
    Available(Arc<TantivyIndex>),
    Unavailable(String),
}

/// A registered daemon-side file watcher and its shutdown handle.
///
/// The `id` lets the watcher thread clear only ITS OWN registration on exit:
/// without it, a force-replaced watcher's exit would wipe the
/// replacement's registration and re-orphan the slot.
pub struct WatcherRegistration {
    id: u64,
    handle: nestweaver_engine::ShutdownHandle,
}

#[derive(Debug, Clone)]
struct EmbeddingRuntimeStatus {
    state: String,
    backend: String,
    requested_device: String,
    selected_device: String,
    model_id: String,
    error: String,
    metal_compiled: bool,
    fallback_used: bool,
}

#[derive(Clone)]
enum EmbeddingRuntimeSnapshot {
    Unavailable {
        status: EmbeddingRuntimeStatus,
    },
    Ready {
        status: EmbeddingRuntimeStatus,
        model: Arc<dyn nestweaver_engine::EmbedQueryFn>,
    },
}

impl EmbeddingRuntimeSnapshot {
    fn status(&self) -> &EmbeddingRuntimeStatus {
        match self {
            Self::Unavailable { status } | Self::Ready { status, .. } => status,
        }
    }

    fn model(&self) -> Option<Arc<dyn nestweaver_engine::EmbedQueryFn>> {
        match self {
            Self::Unavailable { .. } => None,
            Self::Ready { model, .. } => Some(model.clone()),
        }
    }
}

struct EmbeddingRuntime {
    snapshot: std::sync::RwLock<EmbeddingRuntimeSnapshot>,
}

impl EmbeddingRuntime {
    fn unavailable(status: EmbeddingRuntimeStatus) -> Self {
        assert_ne!(status.state, "ready");
        Self {
            snapshot: std::sync::RwLock::new(EmbeddingRuntimeSnapshot::Unavailable { status }),
        }
    }

    fn snapshot(
        &self,
    ) -> (
        EmbeddingRuntimeStatus,
        Option<Arc<dyn nestweaver_engine::EmbedQueryFn>>,
    ) {
        let snapshot = self.snapshot.read().unwrap_or_else(|e| e.into_inner());
        (snapshot.status().clone(), snapshot.model())
    }

    fn status(&self) -> EmbeddingRuntimeStatus {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .status()
            .clone()
    }

    fn current_model(&self) -> Option<Arc<dyn nestweaver_engine::EmbedQueryFn>> {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model()
    }

    fn publish_unavailable(&self, status: EmbeddingRuntimeStatus) {
        assert_ne!(status.state, "ready");
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) =
            EmbeddingRuntimeSnapshot::Unavailable { status };
    }

    fn publish_ready(
        &self,
        status: EmbeddingRuntimeStatus,
        model: Arc<dyn nestweaver_engine::EmbedQueryFn>,
    ) {
        assert_eq!(status.state, "ready");
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) =
            EmbeddingRuntimeSnapshot::Ready { status, model };
    }
}

impl nestweaver_engine::EmbedModelProvider for EmbeddingRuntime {
    fn current_model(&self) -> Option<Arc<dyn nestweaver_engine::EmbedQueryFn>> {
        EmbeddingRuntime::current_model(self)
    }
}

#[cfg(any(feature = "embed", test))]
#[derive(Debug, Clone, Copy)]
struct EmbeddingProbeMetadata {
    backend: &'static str,
    selected_device: &'static str,
    vector_dimension: usize,
}

fn initial_embedding_status(
    cfg: &nestweaver_engine::config::EmbeddingConfig,
    stored_model_id: Option<&str>,
    embedding_compiled: bool,
    metal_compiled: bool,
) -> EmbeddingRuntimeStatus {
    let backend = if cfg.external_endpoint.is_some() {
        "external"
    } else {
        "local"
    };
    let requested_device = match cfg.accelerator {
        nestweaver_engine::config::EmbeddingAccelerator::Auto => "auto",
        nestweaver_engine::config::EmbeddingAccelerator::Metal => "metal",
        nestweaver_engine::config::EmbeddingAccelerator::Cpu => "cpu",
    };
    let model_id = stored_model_id
        .filter(|model_id| !model_id.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            if backend == "external" {
                cfg.external_model
                    .clone()
                    .unwrap_or_else(|| "text-embedding-3-small".to_string())
            } else {
                cfg.model_id.clone()
            }
        });
    EmbeddingRuntimeStatus {
        state: if embedding_compiled {
            "loading".to_string()
        } else {
            "disabled".to_string()
        },
        backend: backend.to_string(),
        requested_device: requested_device.to_string(),
        selected_device: String::new(),
        model_id,
        error: String::new(),
        metal_compiled,
        fallback_used: false,
    }
}

#[cfg(any(feature = "embed", test))]
fn finalize_embedding_status(
    mut status: EmbeddingRuntimeStatus,
    stored_dimension: Option<usize>,
    result: Result<EmbeddingProbeMetadata, String>,
) -> EmbeddingRuntimeStatus {
    match result {
        Ok(metadata) => {
            if let Some(stored_dimension) = stored_dimension
                && stored_dimension != metadata.vector_dimension
            {
                status.state = "failed".to_string();
                status.error = format!(
                    "embedding model dimension ({}) does not match stored embeddings ({}); \
                     run `nestweaver embed --force` to re-embed",
                    metadata.vector_dimension, stored_dimension
                );
                status.selected_device.clear();
                return status;
            }
            status.state = "ready".to_string();
            status.backend = metadata.backend.to_string();
            status.selected_device = if metadata.backend == "local" {
                metadata.selected_device.to_string()
            } else {
                String::new()
            };
            status.error.clear();
        }
        Err(error) => {
            status.state = "failed".to_string();
            status.selected_device.clear();
            status.error = error;
        }
    }
    status
}

fn embedding_status_proto(status: &EmbeddingRuntimeStatus) -> nestweaver_proto::EmbeddingStatus {
    nestweaver_proto::EmbeddingStatus {
        state: status.state.clone(),
        backend: status.backend.clone(),
        requested_device: status.requested_device.clone(),
        selected_device: status.selected_device.clone(),
        model_id: status.model_id.clone(),
        error: status.error.clone(),
        metal_compiled: status.metal_compiled,
        fallback_used: status.fallback_used,
    }
}

fn embedding_status_json(status: &EmbeddingRuntimeStatus) -> serde_json::Value {
    serde_json::json!({
        "state": status.state,
        "backend": status.backend,
        "requested_device": status.requested_device,
        "selected_device": status.selected_device,
        "model_id": status.model_id,
        "error": status.error,
        "metal_compiled": status.metal_compiled,
        "fallback_used": status.fallback_used,
    })
}

fn daemon_metal_compiled() -> bool {
    #[cfg(feature = "embed")]
    {
        nestweaver_embed::metal_compiled()
    }
    #[cfg(not(feature = "embed"))]
    {
        false
    }
}

pub struct DaemonState {
    pub store: Arc<GraphStore>,
    pub tantivy: Option<Arc<TantivyIndex>>,
    search_reconciliation: SearchIndexReconciliation,
    pub db_path: PathBuf,
    /// Runtime identity: the SHA-256-derived id of the canonical `--db` path
    /// (see [`lifecycle::instance_id_from_db_path`]). Used ONLY for runtime
    /// paths — sockets/pidfiles/launchd/replica locks — where the 104-byte
    /// `sun_path` limit forbids arbitrary logical names. Never written into
    /// graph nodes (nw-019).
    pub instance_id: String,
    /// Graph-data identity: the config's logical `instance_id` when `--config`
    /// was supplied, else the db-path hash. This is what gets stamped on every
    /// repo/symbol/note we write, so users see and type one name everywhere
    /// (nw-019). Config-less starts collapse it back onto `instance_id`.
    pub data_instance_id: String,
    pub start_time: Instant,
    pub active_reads: Arc<AtomicU32>,
    pub active_writes: Arc<AtomicU32>,
    pub idle_notify: Arc<Notify>,
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub watcher_stop: std::sync::Mutex<Option<WatcherRegistration>>,
    /// Monotonic id source for [`WatcherRegistration`]s.
    pub next_watcher_id: std::sync::atomic::AtomicU64,
    /// Parsed `nestweaver-instance.toml` if `--config` was supplied at
    /// daemon start. Used by tool dispatch (e.g. F6 `[ranking]` priors in
    /// `brain_search`) via the `set_current_instance_config` thread-local.
    pub instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
    /// Per-repo authorization policy (R9/R9b), built ONCE at startup from
    /// `[authz]` config — not rebuilt per request. No `[authz]` ⇒ a disabled
    /// source that resolves every identity to `VisibleRepos::All`. Mirrors the
    /// MCP-HTTP boundary.
    pub permission_source: Arc<dyn nestweaver_engine::authz::PermissionSource>,
    /// Embedding readiness and the exact usable model as one immutable
    /// snapshot. A handler can never observe `ready` without that model.
    embedding_runtime: Arc<EmbeddingRuntime>,
    /// Serializes write RPCs so only one runs at a time (KùzuDB allows a
    /// single write transaction).
    pub write_mutex: Arc<tokio::sync::Mutex<()>>,
    /// Whether this daemon is running in server mode (TCP, no local source files).
    pub server_mode: bool,
    /// Whether this daemon serves a read-only snapshot replica. When true, all
    /// write RPCs (index/add/remove/merge/watch) are rejected with
    /// `FAILED_PRECONDITION` and the write machinery (worker, scheduler,
    /// webhook) is never started.
    pub read_only: bool,
    /// Whether the server-side worker pool is currently indexing a repo.
    pub indexing_active: Arc<AtomicBool>,
    /// The repo currently being indexed (empty string when idle).
    pub indexing_repo: Arc<tokio::sync::RwLock<String>>,
    /// Number of pending + running jobs in the server-side job queue.
    pub indexing_queue_depth: Arc<AtomicU32>,
    /// Per-tool query safeguards (timeouts, depth limits, result caps).
    pub safeguards: QuerySafeguards,
    /// Per-client rate limiters (token bucket via governor).
    pub rate_limiters: Option<Arc<ClientRateLimiters>>,
    /// Whether the server-side worker pool is drained (not picking new jobs).
    pub drained: Arc<AtomicBool>,
    /// Admin token for admin API authentication (separate from query token).
    pub admin_token: Option<String>,
    /// Shared admin state, set once after construction. Used by `serve_ui`
    /// to mount the admin API on the web UI server as well.
    pub admin_state: std::sync::OnceLock<Arc<nestweaver_web::state::AdminState>>,
    /// Handle to the server-mode worker-pool task. Awaited on shutdown so an
    /// in-flight index write is allowed to finish rather than being abandoned.
    pub worker_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Handle to the `serve_ui` web-server task plus the port it is bound
    /// to, aborted by `stop_ui` so the listen port is released when the CLI
    /// exits (LOW: ui port leak). The port is tracked so a repeated
    /// `serve_ui` can report the ACTUAL running port instead of echoing the
    /// requested one (which would send the CLI to a dead URL).
    pub ui_server: std::sync::Mutex<Option<(u16, tokio::task::JoinHandle<()>)>>,
}

impl DaemonState {
    /// Resolve the caller's per-repo visibility (R9/R9b — Blast Radius scoping)
    /// from a request's tonic extensions.
    ///
    /// The auth interceptor attaches an
    /// [`Identity`](nestweaver_engine::authz::Identity) extension for
    /// authenticated requests; its absence (no-auth / UDS admin paths) is
    /// treated as `Anonymous`. Uses the startup-built [`Self::permission_source`]
    /// (never rebuilt per request).
    ///
    /// Returns `Result<VisibleRepos, Status>`. When the policy is disabled —
    /// the no-`[authz]` single-trust-domain default — this returns
    /// [`VisibleRepos::All`] WITHOUT listing repos, so the vast majority of
    /// RPCs (which don't even read the result) pay nothing. An enabled policy
    /// lists repos per request, retrying once on a store error; if both
    /// attempts fail this returns `Err(Status::unavailable(..))` (nw-043 fail
    /// loud — never a silently-redacted success), which callers propagate with
    /// `?`. Mirrors the MCP-HTTP boundary in `nestweaver-mcp`.
    fn visible_repos_for(
        &self,
        extensions: &tonic::Extensions,
    ) -> Result<nestweaver_engine::authz::VisibleRepos, Status> {
        use nestweaver_engine::authz::{
            AuthzRepoListing, Identity, VisibleRepos, classify_repo_listing,
        };
        // Disabled policy ⇒ everyone is All; skip the per-request repo listing.
        if !self.permission_source.is_enabled() {
            return Ok(VisibleRepos::All);
        }
        let identity = extensions
            .get::<Identity>()
            .cloned()
            .unwrap_or(Identity::Anonymous);
        // nw-043: a store ERROR here must fail the RPC loudly (after one retry),
        // never silently redact everything — a transient store error is
        // indistinguishable from the nw-043 isolation anomaly, and a silent full
        // redaction reads as a valid empty result. A genuinely EMPTY listing
        // still fails closed quietly (enabled policy over ∅ ⇒ nothing visible).
        match classify_repo_listing(self.store.list_repos(None), || self.store.list_repos(None)) {
            AuthzRepoListing::Resolve(repos) => {
                Ok(self.permission_source.visible_repos(&identity, &repos))
            }
            AuthzRepoListing::FailLoud(msg) => Err(Status::unavailable(msg)),
        }
    }
}

/// Build the daemon's permission source once at startup (mirrors the MCP-HTTP
/// boundary). No `[authz]` config ⇒ an empty, disabled source ⇒ every identity
/// resolves to [`nestweaver_engine::authz::VisibleRepos::All`] (zero behavior
/// change for the single-trust-domain default).
fn build_daemon_permission_source(
    instance_cfg: Option<&Arc<nestweaver_engine::InstanceConfig>>,
) -> Arc<dyn nestweaver_engine::authz::PermissionSource> {
    match instance_cfg.and_then(|c| c.authz.as_ref()) {
        Some(authz) => Arc::new(authz.build_permission_source()),
        None => Arc::new(nestweaver_engine::authz::StaticConfigPermissionSource::new(
            std::collections::HashMap::new(),
        )),
    }
}

/// Resolve the empty RPC sentinel to the daemon's configured graph-data
/// identity, then validate the effective ID at the trust boundary.
fn resolve_effective_instance_id(requested: &str, configured: &str) -> Result<String, Status> {
    let effective = if requested.is_empty() {
        configured
    } else {
        requested
    };
    nestweaver_engine::validate_instance_id(effective)
        .map_err(|error| Status::invalid_argument(format!("{error:#}")))?;
    Ok(effective.to_string())
}

/// Stop and unregister any active file watcher. Idempotent.
///
/// Called on EVERY shutdown path (gRPC Shutdown, SIGTERM, post-serve
/// cleanup) so a watcher orphaned by a kill -9'd `watch` CLI can't pin
/// daemon shutdown — the watcher runs on a `spawn_blocking` thread that
/// Tokio's runtime drop waits for, so an unstopped watcher hangs the
/// process until the client's SIGKILL.
fn stop_active_watcher(state: &DaemonState) {
    if let Ok(mut guard) = state.watcher_stop.lock()
        && let Some(reg) = guard.take()
    {
        tracing::info!(watcher_id = reg.id, "stopping active watcher");
        reg.handle.stop();
    }
}

/// Register a watcher's shutdown handle, returning its registration id (the
/// watcher thread passes it to [`clear_watcher_registration`] on exit).
///
/// Refuses when a watcher is already registered unless `force` — in
/// which case the incumbent (possibly orphaned by a kill -9'd `watch` CLI)
/// is stopped and replaced instead of failing new watch sessions forever.
fn register_watcher(
    state: &DaemonState,
    handle: nestweaver_engine::ShutdownHandle,
    force: bool,
) -> Result<u64, Status> {
    let mut guard = state
        .watcher_stop
        .lock()
        .map_err(|e| Status::internal(format!("watcher_stop lock poisoned: {e}")))?;
    if let Some(existing) = guard.as_ref() {
        if !force {
            return Err(Status::already_exists(
                "a watcher is already running; stop it first (StopWatch) or retry with force",
            ));
        }
        tracing::info!(
            watcher_id = existing.id,
            "force-stopping existing watcher (possibly orphaned)"
        );
        existing.handle.stop();
    }
    let id = state.next_watcher_id.fetch_add(1, Ordering::Relaxed);
    *guard = Some(WatcherRegistration { id, handle });
    Ok(id)
}

/// Clear a watcher registration on watcher-thread exit — but only when the
/// slot still holds THIS watcher. A force-replaced watcher's exit must not
/// wipe its replacement's registration.
fn clear_watcher_registration(state: &DaemonState, id: u64) {
    if let Ok(mut guard) = state.watcher_stop.lock()
        && guard.as_ref().is_some_and(|reg| reg.id == id)
    {
        *guard = None;
    }
}

/// The gRPC service implementation. Wraps shared state in an `Arc`.
pub struct DaemonService {
    state: Arc<DaemonState>,
}

/// Mutable state shared by the debounce + circuit-breaker in the watcher's
/// embed-on-change callback (`make_embed_on_change_with`).
#[cfg(feature = "embed")]
struct EmbedOnChangeState {
    last_pass: Option<std::time::Instant>,
    all_fail_passes: u32,
    disabled: bool,
}

impl DaemonService {
    pub fn new(state: Arc<DaemonState>) -> Self {
        Self { state }
    }

    /// Generic JSON pass-through dispatch. Maps every read RPC to the
    /// corresponding MCP tool via `nestweaver_mcp::tools::dispatch`.
    /// Runs the blocking dispatch on a dedicated thread to avoid
    /// starving the tokio runtime.
    ///
    /// In server mode, the dispatch is wrapped with a per-tool timeout
    /// via `with_safeguard`.
    async fn dispatch_json_tool(
        &self,
        tool_name: &str,
        args_json: &str,
        visible: nestweaver_engine::authz::VisibleRepos,
    ) -> Result<Response<JsonResponse>, Status> {
        let started = std::time::Instant::now();
        // Increment gRPC request counter for this tool/method.
        nestweaver_web::routes::metrics::GRPC_REQUESTS
            .with_label_values(&[tool_name])
            .inc();

        let safeguards = &self.state.safeguards;
        let tool = tool_name.to_string();
        let timeout = safeguards.effective_timeout(&tool, None);
        // Cooperative cancellation: the flag is tripped by
        // `with_safeguard_cancellable` on timeout and observed by the
        // `spawn_blocking` dispatch (e.g. brain_context's vector fan-out).
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler = self.dispatch_json_tool_inner(tool_name, args_json, cancel.clone(), visible);

        let response = if self.state.server_mode {
            with_safeguard_cancellable(&tool, safeguards, None, cancel, handler).await
        } else {
            handler.await
        };

        let elapsed = started.elapsed();
        nestweaver_web::routes::metrics::QUERY_DURATION
            .with_label_values(&[tool.as_str()])
            .observe(elapsed.as_secs_f64());
        if elapsed >= timeout.mul_f64(0.8) {
            nestweaver_web::routes::metrics::SLOW_QUERIES.inc();
        }
        if response.is_err() {
            nestweaver_web::routes::metrics::QUERY_ERRORS
                .with_label_values(&[tool.as_str()])
                .inc();
        }

        response
    }

    /// Inner dispatch without safeguard wrapper. Extracted so
    /// `with_safeguard` can race it against a timeout.
    async fn dispatch_json_tool_inner(
        &self,
        tool_name: &str,
        args_json: &str,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        visible: nestweaver_engine::authz::VisibleRepos,
    ) -> Result<Response<JsonResponse>, Status> {
        let t0 = std::time::Instant::now();
        let _guard = ConnectionGuard::read(&self.state);
        // Trip the cancel flag if this request future is dropped (client cancel
        // / disconnect), the same flag the timeout path sets.
        let _disconnect_guard = arm_disconnect_cancel(cancel.clone());

        let state = self.state.clone();
        let tool_name = tool_name.to_string();
        let args_json = args_json.to_string();

        // Clone one atomic readiness/model snapshot before entering the
        // blocking dispatch thread.
        let embed_arc = self.state.embedding_runtime.current_model();

        let tool_name_for_log = tool_name.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || -> Result<String, Status> {
            let t_parse = std::time::Instant::now();
            let args: serde_json::Value = serde_json::from_str(&args_json)
                .map_err(|e| Status::invalid_argument(format!("invalid JSON in args_json: {e}")))?;
            tracing::debug!(
                tool = %tool_name,
                elapsed_us = t_parse.elapsed().as_micros(),
                "arg parse completed"
            );

            nestweaver_mcp::tools::set_current_db_path(state.db_path.clone());
            nestweaver_mcp::tools::set_lite_mode(false);
            nestweaver_mcp::tools::set_current_instance_config(state.instance_cfg.clone());
            nestweaver_mcp::tools::set_server_mode(state.server_mode);

            // In server mode, clamp depth and result limits per safeguard config
            // before passing to the tool handler.
            let mut args = args;
            // Hard-cap traversal depth in ALL modes (local + server) before dispatch — a huge
            // client-supplied depth can overflow the stack in recursive traces (build_flow_tree /
            // walk_trace) or run the graph away in impact BFS. Server mode further tightens this
            // via the safeguard config below.
            clamp_traversal_depth(&mut args);
            let depth_result = if state.server_mode {
                // Clamp depth parameter.
                let client_depth = args
                    .get("depth")
                    .or_else(|| args.get("max_depth"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let dr = state.safeguards.effective_depth(&tool_name, client_depth);
                if args.get("depth").is_some() {
                    args["depth"] = serde_json::json!(dr.depth);
                } else if args.get("max_depth").is_some() {
                    args["max_depth"] = serde_json::json!(dr.depth);
                }

                // Clamp result limit parameter.
                let client_limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
                let effective_limit = state
                    .safeguards
                    .effective_result_limit(&tool_name, client_limit);
                if args.get("limit").is_some() {
                    args["limit"] = serde_json::json!(effective_limit);
                }
                Some(dr)
            } else {
                None
            };

            let embed_ref = embed_arc.as_deref();
            tracing::debug!(
                has_model = embed_ref.is_some(),
                "dispatch_json_tool embed_model status"
            );

            let t_dispatch = std::time::Instant::now();
            let mut value = nestweaver_mcp::tools::dispatch_cancellable(
                &state.store,
                state.tantivy.as_deref(),
                &tool_name,
                args,
                embed_ref,
                Some(&cancel),
                // R9/R9b: scope blast_radius output to the caller's visible
                // repos. `visible` was resolved from the request's Identity
                // extension before the request was consumed. With no `[authz]`
                // config this is `VisibleRepos::All`, so redaction is a no-op
                // (zero behavior change); every non-blast_radius tool ignores it.
                Some(&visible),
            )
            .map_err(|e| dispatch_err_to_status(&tool_name, e))?;
            tracing::debug!(
                tool = %tool_name,
                elapsed_ms = t_dispatch.elapsed().as_millis(),
                "dispatch completed"
            );

            // Communicate depth clamping in response metadata.
            if let Some(ref dr) = depth_result
                && dr.clamped
                && let Some(obj) = value.as_object_mut()
            {
                let meta = obj.entry("_meta").or_insert_with(|| serde_json::json!({}));
                if let Some(meta_obj) = meta.as_object_mut() {
                    meta_obj.insert("_clamped".to_string(), serde_json::json!(true));
                    meta_obj.insert(
                        "_original_depth".to_string(),
                        serde_json::json!(dr.original_depth),
                    );
                }
            }

            let t_ser = std::time::Instant::now();
            let json = serde_json::to_string(&value)
                .map_err(|e| Status::internal(format!("failed to serialize result: {e}")))?;
            tracing::debug!(
                tool = %tool_name,
                elapsed_us = t_ser.elapsed().as_micros(),
                bytes = json.len(),
                "response serialization completed"
            );
            Ok(json)
        })
        .await
        .map_err(|e| Status::internal(format!("dispatch task panicked: {e}")))?;

        tracing::debug!(
            tool = %tool_name_for_log,
            elapsed_ms = t0.elapsed().as_millis(),
            "dispatch_json_tool total completed"
        );

        result.map(|json| Response::new(JsonResponse { result_json: json }))
    }

    /// Dispatch a tool by name with a pre-built JSON args value, returning the
    /// raw `serde_json::Value` result. Used by typed RPC handlers that convert
    /// protobuf → JSON on input and JSON → protobuf on output.
    ///
    /// In server mode, the dispatch is wrapped with a per-tool timeout.
    async fn dispatch_tool_json(
        &self,
        tool_name: &str,
        args: serde_json::Value,
        visible: nestweaver_engine::authz::VisibleRepos,
    ) -> Result<serde_json::Value, Status> {
        let started = std::time::Instant::now();
        // Increment gRPC request counter for this tool/method.
        nestweaver_web::routes::metrics::GRPC_REQUESTS
            .with_label_values(&[tool_name])
            .inc();

        let safeguards = &self.state.safeguards;
        let tool = tool_name.to_string();
        let timeout = safeguards.effective_timeout(&tool, None);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler = self.dispatch_tool_json_inner(tool_name, args, cancel.clone(), visible);

        let response = if self.state.server_mode {
            with_safeguard_cancellable(&tool, safeguards, None, cancel, handler).await
        } else {
            handler.await
        };

        let elapsed = started.elapsed();
        nestweaver_web::routes::metrics::QUERY_DURATION
            .with_label_values(&[tool.as_str()])
            .observe(elapsed.as_secs_f64());
        if elapsed >= timeout.mul_f64(0.8) {
            nestweaver_web::routes::metrics::SLOW_QUERIES.inc();
        }
        if response.is_err() {
            nestweaver_web::routes::metrics::QUERY_ERRORS
                .with_label_values(&[tool.as_str()])
                .inc();
        }

        response
    }

    /// Inner dispatch without safeguard wrapper.
    async fn dispatch_tool_json_inner(
        &self,
        tool_name: &str,
        args: serde_json::Value,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        visible: nestweaver_engine::authz::VisibleRepos,
    ) -> Result<serde_json::Value, Status> {
        let t0 = std::time::Instant::now();
        let _guard = ConnectionGuard::read(&self.state);
        // Trip the cancel flag if this request future is dropped (client cancel
        // / disconnect), the same flag the timeout path sets.
        let _disconnect_guard = arm_disconnect_cancel(cancel.clone());

        let state = self.state.clone();
        let tool_name = tool_name.to_string();

        // Clone one atomic readiness/model snapshot before entering the
        // blocking dispatch thread.
        let embed_arc = self.state.embedding_runtime.current_model();

        let tool_name_for_log = tool_name.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, Status> {
            nestweaver_mcp::tools::set_current_db_path(state.db_path.clone());
            nestweaver_mcp::tools::set_lite_mode(false);
            nestweaver_mcp::tools::set_current_instance_config(state.instance_cfg.clone());
            nestweaver_mcp::tools::set_server_mode(state.server_mode);

            let embed_ref = embed_arc.as_deref();

            let t_dispatch = std::time::Instant::now();
            let value = nestweaver_mcp::tools::dispatch_cancellable(
                &state.store,
                state.tantivy.as_deref(),
                &tool_name,
                args,
                embed_ref,
                Some(&cancel),
                // R9/R9b: scope tool output to the caller's visible repos
                // (`visible`, resolved from the request Identity by the typed
                // handler). With no `[authz]` config this is
                // `VisibleRepos::All` ⇒ redaction is a no-op. Enforcement
                // applies to blast_radius and to the repo-scoped search tools
                // (brain_search, brain_impact, affected_tests); other tools
                // ignore it.
                Some(&visible),
            )
            .map_err(|e| dispatch_err_to_status(&tool_name, e))?;
            tracing::debug!(
                tool = %tool_name,
                elapsed_ms = t_dispatch.elapsed().as_millis(),
                "dispatch_tool_json dispatch completed"
            );
            Ok(value)
        })
        .await
        .map_err(|e| Status::internal(format!("dispatch task panicked: {e}")))?;

        tracing::debug!(
            tool = %tool_name_for_log,
            elapsed_ms = t0.elapsed().as_millis(),
            "dispatch_tool_json total completed"
        );

        result
    }

    /// Minimum interval between embed passes triggered by watcher batches.
    /// A burst of file saves produces many debounced watcher batches; without
    /// this throttle each batch synchronously embeds up to 64 nodes INLINE on
    /// the watcher thread (local-model inference is the dominant CPU cost of
    /// a watch session), so a 10-file burst could stall the watcher's DB
    /// writes for minutes at 300%+ CPU. Coalescing to one pass per interval
    /// keeps embeddings fresh without letting them starve reindexing.
    #[cfg(feature = "embed")]
    const EMBED_ON_CHANGE_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

    /// Consecutive embed passes that attempted nodes but stored nothing
    /// (every `embed_query` failed or every vector was rejected, e.g. a
    /// dimension-mismatched/zero-length vector that `add_embedding`
    /// refuses) before the callback stops retrying. Retrying a
    /// deterministically-failing embed on every watcher batch is a hot
    /// loop that burns CPU forever without making progress; the warn tells
    /// the operator how to recover (`nestweaver embed --force` / restart).
    #[cfg(feature = "embed")]
    const EMBED_ON_CHANGE_MAX_ALL_FAIL_PASSES: u32 = 3;

    /// Build an `on_change` callback that embeds un-embedded nodes after every
    /// watcher batch.  Returns `None` when the `embed` feature is disabled or
    /// the model is not yet loaded.
    ///
    /// The embed writes run **inline** on the watcher thread. That thread holds
    /// `write_mutex` + a `ConnectionGuard::write` for the watcher's whole run
    /// (see `watch_vault`/`watch_code` wiring) and invokes this callback within
    /// that hold, so executing the `add_embedding`/`flush_embedding_index`
    /// writes here keeps them under the same write gate every other daemon
    /// mutation uses:
    ///  - **backup-safe:** the write runs while the watcher holds `write_mutex`,
    ///    so a backup's `.embeddings` sidecar copy (which also takes
    ///    `write_mutex` in `stage_backup_from_store`) cannot run concurrently,
    ///    and no detached task outlives the lock to race the copy;
    ///  - **drain-visible:** the write completes before the watcher thread drops
    ///    its `ConnectionGuard::write`, so `active_writes` stays > 0 until the
    ///    embed finishes and the shutdown drain waits for it.
    ///
    /// It must NOT be moved to a detached `spawn_blocking` task: that escapes
    /// both guarantees. It also must NOT re-acquire `write_mutex` itself — the
    /// watcher thread already holds it for the whole run, so a fresh
    /// `blocking_lock()` here would deadlock. Inheriting the existing hold is
    /// the correct single-writer discipline.
    #[cfg(feature = "embed")]
    fn make_embed_on_change(
        embedding_runtime: Arc<EmbeddingRuntime>,
        store: Arc<nestweaver_store::GraphStore>,
    ) -> Option<Box<dyn Fn() + Send>> {
        Self::make_embed_on_change_with(
            embedding_runtime,
            store,
            Self::EMBED_ON_CHANGE_MIN_INTERVAL,
        )
    }

    /// [`Self::make_embed_on_change`] with an injectable debounce interval
    /// (tests use `Duration::ZERO` to exercise every pass).
    #[cfg(feature = "embed")]
    fn make_embed_on_change_with(
        embedding_runtime: Arc<EmbeddingRuntime>,
        store: Arc<nestweaver_store::GraphStore>,
        min_interval: std::time::Duration,
    ) -> Option<Box<dyn Fn() + Send>> {
        let state = Arc::new(std::sync::Mutex::new(EmbedOnChangeState {
            last_pass: None,
            all_fail_passes: 0,
            disabled: false,
        }));
        Some(Box::new(move || {
            {
                let mut st = state.lock().unwrap();
                if st.disabled {
                    return;
                }
                // Debounce: coalesce a burst of watcher batches into at most
                // one embed pass per interval.
                if st.last_pass.is_some_and(|t| t.elapsed() < min_interval) {
                    return;
                }
                st.last_pass = Some(std::time::Instant::now());
            }
            let model = embedding_runtime.current_model();
            let Some(model) = model else { return };

            let mut attempted = 0u32;
            let mut embedded = 0u32;
            let limit: usize = 64; // Max nodes per watcher cycle

            // Symbols
            if let Ok(symbols) = store.list_all_symbols() {
                for sym in symbols
                    .iter()
                    .filter(|s| !store.has_embedding(&s.uid))
                    .take(limit)
                {
                    let text = nestweaver_embed::preprocess::symbol_embed_text(
                        &sym.kind.to_string(),
                        &sym.name,
                        None,
                    );
                    attempted += 1;
                    match model.embed_query(&text) {
                        Ok(emb) => {
                            // Dimension-guard rejections must not count as
                            // embedded (add_embedding logs them).
                            if store.add_embedding(&sym.uid, emb) {
                                embedded += 1;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(uid = %sym.uid, "embedding failed: {e}");
                        }
                    }
                }
            }

            // Budget on ATTEMPTS, not successes: with a failing
            // endpoint, success-based budgeting lets one pass fire 64
            // symbol + 64 note + 64 heading requests. The 64-node cap is
            // meant to bound work per watcher cycle regardless of outcome.
            let remaining = limit.saturating_sub(attempted as usize);

            // Notes
            if remaining > 0
                && let Ok(notes) = store.list_notes(None)
            {
                for note in notes
                    .iter()
                    .filter(|n| !store.has_embedding(&n.uid))
                    .take(remaining)
                {
                    let text = nestweaver_embed::preprocess::note_embed_text(&note.title, None);
                    attempted += 1;
                    match model.embed_query(&text) {
                        Ok(emb) => {
                            if store.add_embedding(&note.uid, emb) {
                                embedded += 1;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(uid = %note.uid, "embedding failed: {e}");
                        }
                    }
                }
            }

            let remaining = limit.saturating_sub(attempted as usize);

            // Headings
            if remaining > 0
                && let Ok(headings) = store.list_all_headings()
            {
                for heading in headings
                    .iter()
                    .filter(|h| !store.has_embedding(&h.uid))
                    .take(remaining)
                {
                    let text = nestweaver_embed::preprocess::heading_embed_text("", &heading.text);
                    attempted += 1;
                    match model.embed_query(&text) {
                        Ok(emb) => {
                            if store.add_embedding(&heading.uid, emb) {
                                embedded += 1;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(uid = %heading.uid, "embedding failed: {e}");
                        }
                    }
                }
            }

            if embedded > 0 {
                if let Err(e) = store.flush_embedding_index() {
                    tracing::warn!("failed to flush embedding index: {e}");
                }
                tracing::debug!(count = embedded, "embedded new nodes from watcher");
            }

            // Circuit breaker: a pass that attempted nodes but stored none
            // will fail the same way on every future batch (deterministic
            // rejection or a dead endpoint). After enough consecutive
            // all-fail passes, stop retrying instead of hot-looping.
            let mut st = state.lock().unwrap();
            if attempted > 0 && embedded == 0 {
                st.all_fail_passes += 1;
                if st.all_fail_passes >= Self::EMBED_ON_CHANGE_MAX_ALL_FAIL_PASSES {
                    st.disabled = true;
                    tracing::warn!(
                        passes = st.all_fail_passes,
                        "watcher embed disabled after repeated all-failed passes \
                         (every embed_query failed or every vector was rejected); \
                         fix the embedding model/endpoint and run `nestweaver embed --force` \
                         or restart the daemon"
                    );
                }
            } else {
                st.all_fail_passes = 0;
            }
        }))
    }

    #[cfg(not(feature = "embed"))]
    fn make_embed_on_change(
        _embedding_runtime: Arc<EmbeddingRuntime>,
        _store: Arc<nestweaver_store::GraphStore>,
    ) -> Option<Box<dyn Fn() + Send>> {
        None
    }
}

#[cfg(test)]
mod instance_id_validation_tests {
    use super::*;

    #[test]
    fn effective_instance_id_uses_request_then_configured_default() {
        assert_eq!(
            resolve_effective_instance_id("request-id", "configured-id").unwrap(),
            "request-id"
        );
        assert_eq!(
            resolve_effective_instance_id("", "configured-id").unwrap(),
            "configured-id"
        );
    }

    #[test]
    fn effective_instance_id_rejects_every_invalid_source() {
        for (requested, configured) in [
            ("", ""),
            ("a:b", "configured-id"),
            ("has space", "configured-id"),
            ("has\ttab", "configured-id"),
            ("", "configured:bad"),
            ("", "configured bad"),
        ] {
            let error = resolve_effective_instance_id(requested, configured).unwrap_err();
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert!(
                error.message().contains("instance_id"),
                "requested={requested:?}, configured={configured:?}: {error}"
            );
        }
    }
}

// ── Trait impl ──────────────────────────────────────────────────────

/// Tools that mutate server state and require admin-level auth via gRPC.
///
/// The gRPC gate and the HTTP/MCP gate share ONE list — the canonical
/// [`nestweaver_mcp::http::MUTATING_TOOLS`] — so the two surfaces can never
/// drift (a new mutating tool guarded on one gate but not the other). Re-export
/// it under the module-local name the `json_rpc!` gate reads.
use nestweaver_mcp::http::MUTATING_TOOLS;

/// Maps each gRPC RPC name to the MCP tool name it dispatches to.
macro_rules! json_rpc {
    ($self:ident, $request:ident, $tool:expr) => {{
        // Gate mutating tools behind admin auth, matching the HTTP layer.
        if MUTATING_TOOLS.contains(&$tool) {
            if let Some(crate::auth::IsAdmin(false)) | None =
                $request.extensions().get::<crate::auth::IsAdmin>()
            {
                return Err(Status::permission_denied(format!(
                    "tool '{}' is mutating and requires the admin token",
                    $tool
                )));
            }
        }
        // R9/R9b: resolve the caller's per-repo visibility from the request's
        // Identity extension BEFORE consuming the request. This covers the
        // generic `/mcp` tool path and every typed RPC routed through this
        // macro (including `blast_radius`), so their output is redacted to the
        // caller's visible repos. No `[authz]` config ⇒ `VisibleRepos::All` ⇒
        // no-op redaction (backward compatible).
        let visible = $self.state.visible_repos_for($request.extensions())?;
        let req = $request.into_inner();
        $self
            .dispatch_json_tool($tool, &req.args_json, visible)
            .await
    }};
}

type ProgressStream = tokio_stream::wrappers::ReceiverStream<Result<IndexProgress, Status>>;

/// Delete every repo whose KNOWN local working tree no longer exists on disk.
///
/// Safety contract (data-loss guard): a repo is prunable only when
/// [`nestweaver_schema::Repo::local_root`] yields a path — i.e. it has a
/// recorded `root_path`, or a legacy `file://` identity url. Repos with a
/// remote identity and no working tree (server-side bare-clone repos,
/// `root_path: None`) are skipped entirely: a disk-existence check cannot
/// apply to them, and bulk-deleting them here would destroy server data.
///
/// Returns the display names and UIDs of the removed repos so the RPC can
/// finalize graph and sidecar state after every graph deletion succeeds.
#[derive(Debug, Default)]
struct PrunedRepos {
    names: Vec<String>,
    uids: Vec<String>,
}

impl PrunedRepos {
    fn is_empty(&self) -> bool {
        self.uids.is_empty()
    }
}

fn prune_stale_repos_with<F>(
    store: &nestweaver_store::GraphStore,
    mut delete_repo: F,
) -> (PrunedRepos, Option<anyhow::Error>)
where
    F: FnMut(&nestweaver_store::GraphStore, &nestweaver_schema::Repo) -> Result<(), anyhow::Error>,
{
    let mut removed_repos = PrunedRepos::default();
    let repos = match store.list_repos(None) {
        Ok(repos) => repos,
        Err(error) => {
            return (
                removed_repos,
                Some(anyhow::anyhow!("list_repos failed: {error:#}")),
            );
        }
    };

    for repo in &repos {
        let Some(path) = repo.local_root() else {
            continue;
        };
        if !Path::new(path).exists() {
            tracing::info!(
                uid = %repo.uid,
                url = %repo.url,
                root = path,
                "pruning stale repo: local working tree no longer exists"
            );
            if let Err(error) = delete_repo(store, repo) {
                // The cascade is multi-statement; conservatively treat the
                // failing repo as changed so a mid-cascade error cannot leave
                // cache or sidecar state describing deleted children.
                removed_repos.uids.push(repo.uid.clone());
                return (removed_repos, Some(error));
            }

            removed_repos.uids.push(repo.uid.clone());
            removed_repos
                .names
                .push(repo.name.clone().unwrap_or_else(|| repo.url.clone()));
        }
    }
    (removed_repos, None)
}

fn delete_repo_cascade(
    store: &nestweaver_store::GraphStore,
    repo: &nestweaver_schema::Repo,
) -> Result<(), anyhow::Error> {
    store
        .bulk_delete_repo_files_and_symbols(&repo.uid)
        .map_err(|e| anyhow::anyhow!("bulk_delete_repo_files_and_symbols failed: {e:#}"))?;
    store
        .clear_repo_derived_nodes(&repo.uid)
        .map_err(|e| anyhow::anyhow!("clear_repo_derived_nodes failed: {e:#}"))?;
    store
        .delete_repo_node(&repo.uid)
        .map_err(|e| anyhow::anyhow!("delete_repo_node failed: {e:#}"))
}

#[cfg(test)]
fn prune_stale_repos(store: &nestweaver_store::GraphStore) -> Result<PrunedRepos, anyhow::Error> {
    let (removed, error) = prune_stale_repos_with(store, delete_repo_cascade);
    match error {
        Some(error) => Err(error),
        None => Ok(removed),
    }
}

fn finalize_code_graph_deletion(
    state: &DaemonState,
    repo_uids: &[String],
) -> Vec<nestweaver_engine::DeletionReconciliationFailure> {
    match nestweaver_engine::finalize_code_graph_deletion(
        &state.store,
        &state.db_path,
        repo_uids,
        "code graph deletion",
    ) {
        Ok(()) => Vec::new(),
        Err(error) => error.failures,
    }
}

fn reconcile_deleted_extension_uids(
    state: &DaemonState,
    deleted_uids: &[String],
    failures: &mut Vec<nestweaver_engine::DeletionReconciliationFailure>,
) {
    match nestweaver_engine::reconcile_deleted_extension_uids(
        &state.store,
        &state.db_path,
        deleted_uids,
    ) {
        Ok(removed) => tracing::info!(removed, "targeted extension metadata reconciled"),
        Err(error) => push_reconciliation_failure(
            failures,
            nestweaver_engine::DeletionReconciliationStage::ExtensionMetadata,
            format!("targeted extension metadata reconciliation failed: {error:#}"),
        ),
    }
}

fn push_reconciliation_failure(
    failures: &mut Vec<nestweaver_engine::DeletionReconciliationFailure>,
    stage: nestweaver_engine::DeletionReconciliationStage,
    message: impl Into<String>,
) {
    failures.push(nestweaver_engine::DeletionReconciliationFailure {
        stage,
        repo_uid: None,
        message: message.into(),
    });
}

fn finalize_node_graph_deletion(
    state: &DaemonState,
    operation: &str,
) -> Vec<nestweaver_engine::DeletionReconciliationFailure> {
    let mut failures = Vec::new();
    match state.store.reconcile_embedding_index_stages() {
        Err(error) => push_reconciliation_failure(
            &mut failures,
            nestweaver_engine::DeletionReconciliationStage::EmbeddingIndex,
            format!("embedding live-set reconciliation failed: {error:#}"),
        ),
        Ok(result) => {
            match result.canonical_persistence {
                Ok(()) => tracing::info!(
                    removed = result.removed,
                    operation,
                    "reconciled deleted node embeddings"
                ),
                Err(error) => push_reconciliation_failure(
                    &mut failures,
                    nestweaver_engine::DeletionReconciliationStage::EmbeddingIndex,
                    format!("embedding persistence failed: {error:#}"),
                ),
            }
            if let Some(Err(error)) = result.legacy_retirement {
                push_reconciliation_failure(
                    &mut failures,
                    nestweaver_engine::DeletionReconciliationStage::LegacyRetirement,
                    format!("legacy embedding retirement failed: {error:#}"),
                );
            }
        }
    }
    let generation_advanced = match state.store.try_bump_graph_generation() {
        Ok(_) => true,
        Err(error) => {
            push_reconciliation_failure(
                &mut failures,
                nestweaver_engine::DeletionReconciliationStage::GenerationPersistence,
                format!("advance graph generation: {error:#}"),
            );
            false
        }
    };
    let generation_path = nestweaver_engine::sidecar_path(&state.db_path, ".generation");
    if generation_advanced && let Err(error) = state.store.save_graph_generation(&generation_path) {
        push_reconciliation_failure(
            &mut failures,
            nestweaver_engine::DeletionReconciliationStage::GenerationPersistence,
            format!("{}: {error:#}", generation_path.display()),
        );
    }
    if let Err(error) =
        nestweaver_engine::reconcile_extension_liveness(&state.store, &state.db_path)
    {
        push_reconciliation_failure(
            &mut failures,
            nestweaver_engine::DeletionReconciliationStage::ExtensionMetadata,
            format!("extension metadata liveness reconciliation failed: {error:#}"),
        );
    }
    state.store.invalidate_pagerank();
    let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
    if let Err(error) =
        nestweaver_store::durable_sidecar::remove_file_durable_if_exists(&pagerank_path)
    {
        push_reconciliation_failure(
            &mut failures,
            nestweaver_engine::DeletionReconciliationStage::PersistedPageRank,
            format!("{}: {error}", pagerank_path.display()),
        );
    }
    failures
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct IndexedSearchDocument {
    uid: String,
    kind: &'static str,
    title: String,
    body: String,
    vault_uid: String,
    note_uid: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexedSearchMutation {
    Unchanged,
    Changed,
    Unknown,
}

fn indexed_section_lines(lines: &[&str], start: u32, end: u32) -> String {
    if start == 0 || start as usize > lines.len() {
        return String::new();
    }
    let end = (end as usize).min(lines.len());
    let start = (start - 1) as usize;
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

fn indexed_search_rows(
    store: &GraphStore,
) -> Result<std::collections::HashSet<IndexedSearchDocument>, anyhow::Error> {
    use std::collections::{HashMap, HashSet};

    let notes = store.list_notes(None)?;
    let headings = store.list_all_headings()?;
    let sections = store.list_all_sections()?;
    let mut headings_by_note: HashMap<&str, Vec<_>> = HashMap::new();
    for heading in &headings {
        headings_by_note
            .entry(heading.note_uid.as_str())
            .or_default()
            .push(heading);
    }
    let mut sections_by_note: HashMap<&str, Vec<_>> = HashMap::new();
    for section in &sections {
        sections_by_note
            .entry(section.note_uid.as_str())
            .or_default()
            .push(section);
    }

    let mut documents = HashSet::new();
    for note in &notes {
        let note_headings = headings_by_note
            .get(note.uid.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let note_sections = sections_by_note
            .get(note.uid.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let body_from_disk = store.lookup_vault(&note.vault_uid).ok().and_then(|vault| {
            let path = std::path::Path::new(&vault.root_path).join(&note.file_path);
            std::fs::read_to_string(path).ok()
        });
        documents.insert(IndexedSearchDocument {
            uid: note.uid.clone(),
            kind: "note",
            title: note.title.clone(),
            body: body_from_disk.clone().unwrap_or_else(|| note.title.clone()),
            vault_uid: note.vault_uid.clone(),
            note_uid: note.uid.clone(),
        });
        for heading in note_headings {
            documents.insert(IndexedSearchDocument {
                uid: heading.uid.clone(),
                kind: "heading",
                title: heading.text.clone(),
                body: heading.text.clone(),
                vault_uid: note.vault_uid.clone(),
                note_uid: note.uid.clone(),
            });
        }
        let body_lines: Vec<&str> = body_from_disk
            .as_deref()
            .map(|body| body.lines().collect())
            .unwrap_or_default();
        for section in note_sections {
            let title = section
                .heading_uid
                .as_deref()
                .and_then(|heading_uid| {
                    note_headings
                        .iter()
                        .find(|heading| heading.uid == heading_uid)
                })
                .map(|heading| heading.text.clone())
                .unwrap_or_default();
            let body = if section.text_content.is_empty() {
                indexed_section_lines(&body_lines, section.start_line, section.end_line)
            } else {
                section.text_content.clone()
            };
            documents.insert(IndexedSearchDocument {
                uid: section.uid.clone(),
                kind: "section",
                title,
                body,
                vault_uid: note.vault_uid.clone(),
                note_uid: note.uid.clone(),
            });
        }
    }
    for tag in store.list_tags(None)? {
        documents.insert(IndexedSearchDocument {
            uid: tag.uid,
            kind: "tag",
            title: tag.name.clone(),
            body: tag.name,
            vault_uid: tag.vault_uid,
            note_uid: String::new(),
        });
    }
    Ok(documents)
}

fn indexed_search_rows_before_with<F>(
    search: &SearchIndexReconciliation,
    project: F,
) -> Option<Result<std::collections::HashSet<IndexedSearchDocument>, anyhow::Error>>
where
    F: FnOnce() -> Result<std::collections::HashSet<IndexedSearchDocument>, anyhow::Error>,
{
    match search {
        SearchIndexReconciliation::Disabled => None,
        SearchIndexReconciliation::Available(_) | SearchIndexReconciliation::Unavailable(_) => {
            Some(project())
        }
    }
}

fn indexed_search_rows_before(
    state: &DaemonState,
) -> Option<Result<std::collections::HashSet<IndexedSearchDocument>, anyhow::Error>> {
    indexed_search_rows_before_with(&state.search_reconciliation, || {
        indexed_search_rows(&state.store)
    })
}

fn indexed_search_mutation(
    before: Option<Result<std::collections::HashSet<IndexedSearchDocument>, anyhow::Error>>,
    store: &GraphStore,
) -> IndexedSearchMutation {
    match before {
        None => IndexedSearchMutation::Unchanged,
        Some(Err(error)) => {
            tracing::warn!(
                error = %error,
                "indexed search mutation preflight is unknown; repairing conservatively"
            );
            IndexedSearchMutation::Unknown
        }
        Some(Ok(before)) => match indexed_search_rows(store) {
            Ok(after) if before == after => IndexedSearchMutation::Unchanged,
            Ok(_) => IndexedSearchMutation::Changed,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "indexed search mutation postflight is unknown; repairing conservatively"
                );
                IndexedSearchMutation::Unknown
            }
        },
    }
}

fn reconcile_search_index(
    state: &SearchIndexReconciliation,
    store: &GraphStore,
    mutation: IndexedSearchMutation,
    operation: &str,
) -> Result<(), anyhow::Error> {
    if mutation == IndexedSearchMutation::Unchanged {
        return Ok(());
    }
    match state {
        SearchIndexReconciliation::Disabled => Ok(()),
        SearchIndexReconciliation::Available(tantivy) => {
            let docs = tantivy.reindex_from_store(store)?;
            tracing::info!(docs, operation, "Tantivy reindexed after mutation");
            Ok(())
        }
        SearchIndexReconciliation::Unavailable(reason) => {
            anyhow::bail!("configured Tantivy index unavailable: {reason}")
        }
    }
}

fn open_search_index(
    tantivy_path: &Path,
    read_only: bool,
) -> (Option<Arc<TantivyIndex>>, SearchIndexReconciliation) {
    if read_only {
        return match TantivyIndex::open_reader_only(tantivy_path) {
            Ok(index) => {
                tracing::info!(
                    docs = index.doc_count(),
                    path = %tantivy_path.display(),
                    "Tantivy index open (replica, reader-only)"
                );
                (Some(Arc::new(index)), SearchIndexReconciliation::Disabled)
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not open Tantivy index reader — search will use substring fallback"
                );
                (None, SearchIndexReconciliation::Disabled)
            }
        };
    }

    match TantivyIndex::open_or_create(tantivy_path) {
        Ok(index) => {
            tracing::info!(
                docs = index.doc_count(),
                path = %tantivy_path.display(),
                "Tantivy index open (read-write)"
            );
            let index = Arc::new(index);
            (
                Some(Arc::clone(&index)),
                SearchIndexReconciliation::Available(index),
            )
        }
        Err(writer_error) => match TantivyIndex::open_reader_only(tantivy_path) {
            Ok(index) => {
                tracing::info!(
                    docs = index.doc_count(),
                    path = %tantivy_path.display(),
                    "Tantivy index open (reader-only fallback)"
                );
                (
                    Some(Arc::new(index)),
                    SearchIndexReconciliation::Unavailable(format!(
                        "writer open failed: {writer_error}"
                    )),
                )
            }
            Err(reader_error) => {
                tracing::warn!(
                    error = %reader_error,
                    "could not open Tantivy index — search will use substring fallback"
                );
                (
                    None,
                    SearchIndexReconciliation::Unavailable(format!(
                        "writer open failed: {writer_error}; reader open failed: {reader_error}"
                    )),
                )
            }
        },
    }
}

fn finish_reconciled_mutation<T>(
    mutation: Result<T, Status>,
    operation: &str,
    failures: Vec<nestweaver_engine::DeletionReconciliationFailure>,
) -> Result<T, Status> {
    match mutation {
        // The destructive mutation COMMITTED. Post-commit reconciliation failures
        // (a generation-bump, a sidecar removal, an embedding/search reconcile)
        // must NOT turn a durable success into a reported FAILURE (nw-091 / Bug 2)
        // — that made a user believe a committed remove_vault had not happened and
        // take corrective action against already-deleted data (752 → 1 notes).
        // Return success; the reconciliation debt is logged loudly and is
        // idempotently retryable by re-running the operation (a re-run is a
        // confirmed no-op that re-runs finalize). Handlers whose response carries
        // `committed` / `reconciliation_failures` stamp them BEFORE calling this.
        Ok(response) => {
            if !failures.is_empty() {
                let reconciliation =
                    nestweaver_engine::DeletionReconciliationError::new(operation, failures);
                tracing::error!(
                    operation,
                    reconciliation = %reconciliation,
                    "mutation COMMITTED but post-commit reconciliation failed; re-run the \
                     operation to retry reconciliation"
                );
            }
            Ok(response)
        }
        // Genuine failure: the mutation did NOT commit. Fold in any finalize noise.
        Err(status) if failures.is_empty() => Err(status),
        Err(status) => {
            let reconciliation =
                nestweaver_engine::DeletionReconciliationError::new(operation, failures);
            Err(Status::new(
                status.code(),
                format!("{}; {reconciliation}", status.message()),
            ))
        }
    }
}

/// Convert engine reconciliation failures into their wire form so a committed
/// mutation can honestly report "succeeded, but N post-commit steps failed"
/// (nw-091 / Bug 2).
fn to_proto_reconciliation_failures(
    failures: &[nestweaver_engine::DeletionReconciliationFailure],
) -> Vec<nestweaver_proto::ReconciliationFailure> {
    failures
        .iter()
        .map(|f| nestweaver_proto::ReconciliationFailure {
            stage: f.stage.to_string(),
            repo_uid: f.repo_uid.clone().unwrap_or_default(),
            message: f.message.clone(),
        })
        .collect()
}

fn append_search_reconciliation(
    failures: &mut Vec<nestweaver_engine::DeletionReconciliationFailure>,
    result: Result<(), anyhow::Error>,
) {
    if let Err(error) = result {
        push_reconciliation_failure(
            failures,
            nestweaver_engine::DeletionReconciliationStage::SearchIndex,
            format!("{error:#}"),
        );
    }
}

fn run_remove_repo_with<C, D>(
    state: &DaemonState,
    repo_uid: &str,
    clear_derived: C,
    delete_repo: D,
) -> Result<RemoveRepoResponse, Status>
where
    C: FnOnce(&GraphStore, &str) -> Result<(), Status>,
    D: FnOnce(&GraphStore, &str) -> Result<(), Status>,
{
    run_remove_repo_with_bulk(
        state,
        repo_uid,
        |store, uid| {
            store.bulk_delete_repo_files_and_symbols(uid).map_err(|e| {
                Status::internal(format!("bulk_delete_repo_files_and_symbols failed: {e:#}"))
            })
        },
        clear_derived,
        delete_repo,
    )
}

fn run_remove_repo_with_bulk<B, C, D>(
    state: &DaemonState,
    repo_uid: &str,
    bulk_delete: B,
    clear_derived: C,
    delete_repo: D,
) -> Result<RemoveRepoResponse, Status>
where
    B: FnOnce(&GraphStore, &str) -> Result<(usize, usize), Status>,
    C: FnOnce(&GraphStore, &str) -> Result<(), Status>,
    D: FnOnce(&GraphStore, &str) -> Result<(), Status>,
{
    let mutation = match bulk_delete(&state.store, repo_uid) {
        Ok((file_count, sym_count)) => clear_derived(&state.store, repo_uid)
            .and_then(|()| delete_repo(&state.store, repo_uid))
            .map(|()| RemoveRepoResponse {
                files_deleted: file_count as u64,
                symbols_deleted: sym_count as u64,
                ..Default::default()
            }),
        Err(error) => Err(error),
    };

    let reconciliation = nestweaver_engine::finalize_code_graph_deletion(
        &state.store,
        &state.db_path,
        &[repo_uid.to_string()],
        "repo removal",
    )
    .err()
    .map(|error| error.failures)
    .unwrap_or_default();

    // nw-091 / Bug 2: mark the committed response so a post-commit reconciliation
    // failure is reported as success-with-warnings, never a bare error.
    let reconciliation_failures = to_proto_reconciliation_failures(&reconciliation);
    let mutation = mutation.map(|mut response| {
        response.committed = true;
        response.reconciliation_failures = reconciliation_failures;
        response
    });
    finish_reconciled_mutation(mutation, "repo removal", reconciliation)
}

fn run_remove_project_with<D, L, C, F>(
    state: &DaemonState,
    project_uid: &str,
    delete_project: D,
    project_exists: L,
    cleanup_extensions: C,
    finalize: F,
) -> Result<RemoveProjectResponse, Status>
where
    D: FnOnce(
        &GraphStore,
        &str,
    ) -> Result<
        nestweaver_store::DeleteProjectCascadeOutcome,
        nestweaver_store::DeleteProjectCascadeError,
    >,
    L: FnOnce(&GraphStore, &str) -> Result<bool, nestweaver_store::StoreError>,
    C: FnOnce(&Path, &str) -> Result<bool, anyhow::Error>,
    F: FnOnce(&DaemonState, &str) -> Vec<nestweaver_engine::DeletionReconciliationFailure>,
{
    enum ExtensionCleanup {
        No,
        Yes,
        CheckLiveness,
    }

    let deletion = delete_project(&state.store, project_uid);
    let (mutation, finalize_needed, mut extension_cleanup) = match deletion {
        Ok(outcome) => {
            let finalize_needed = matches!(
                outcome.disposition,
                nestweaver_store::ProjectMutationDisposition::Changed
                    | nestweaver_store::ProjectMutationDisposition::Ambiguous
            );
            let cleanup = match outcome.disposition {
                nestweaver_store::ProjectMutationDisposition::Changed
                | nestweaver_store::ProjectMutationDisposition::ConfirmedUnchanged => {
                    ExtensionCleanup::Yes
                }
                nestweaver_store::ProjectMutationDisposition::Ambiguous => {
                    ExtensionCleanup::CheckLiveness
                }
                nestweaver_store::ProjectMutationDisposition::ConfirmedRolledBack => {
                    ExtensionCleanup::No
                }
            };
            (
                Ok(RemoveProjectResponse {
                    project_name: outcome.project_name.unwrap_or_default(),
                    ..Default::default()
                }),
                finalize_needed,
                cleanup,
            )
        }
        Err(error) => {
            let (finalize_needed, cleanup) = match error.disposition {
                nestweaver_store::ProjectMutationDisposition::ConfirmedUnchanged
                | nestweaver_store::ProjectMutationDisposition::ConfirmedRolledBack => {
                    (false, ExtensionCleanup::No)
                }
                nestweaver_store::ProjectMutationDisposition::Changed => {
                    (true, ExtensionCleanup::Yes)
                }
                nestweaver_store::ProjectMutationDisposition::Ambiguous => {
                    (true, ExtensionCleanup::CheckLiveness)
                }
            };
            (
                Err(Status::internal(format!(
                    "delete Project cascade failed: {error}"
                ))),
                finalize_needed,
                cleanup,
            )
        }
    };
    let mut failures = Vec::new();

    if matches!(extension_cleanup, ExtensionCleanup::CheckLiveness) {
        extension_cleanup = match project_exists(&state.store, project_uid) {
            Ok(true) => ExtensionCleanup::No,
            Ok(false) => ExtensionCleanup::Yes,
            Err(error) => {
                push_reconciliation_failure(
                    &mut failures,
                    nestweaver_engine::DeletionReconciliationStage::GraphLiveness,
                    format!("Project {project_uid} liveness query failed: {error:#}"),
                );
                ExtensionCleanup::No
            }
        };
    }
    if matches!(extension_cleanup, ExtensionCleanup::Yes)
        && let Err(error) = cleanup_extensions(&state.db_path, project_uid)
    {
        push_reconciliation_failure(
            &mut failures,
            nestweaver_engine::DeletionReconciliationStage::ExtensionMetadata,
            format!("Project {project_uid}: {error:#}"),
        );
    }
    if finalize_needed {
        failures.extend(finalize(state, "project removal"));
    }

    let reconciliation_failures = to_proto_reconciliation_failures(&failures);
    let mutation = mutation.map(|mut response| {
        response.committed = true;
        response.reconciliation_failures = reconciliation_failures;
        response
    });
    finish_reconciled_mutation(mutation, "project removal", failures)
}

fn rebuild_tantivy_after_mutation(
    state: &DaemonState,
    mutation: IndexedSearchMutation,
    operation: &str,
) -> Result<(), anyhow::Error> {
    reconcile_search_index(
        &state.search_reconciliation,
        &state.store,
        mutation,
        operation,
    )
}

fn run_prune_stale_with<DR, DV, R>(
    state: &DaemonState,
    delete_repo: DR,
    mut delete_vault: DV,
    mut reconcile_search: R,
) -> Result<PruneStaleResponse, Status>
where
    DR: FnMut(&GraphStore, &nestweaver_schema::Repo) -> Result<(), anyhow::Error>,
    DV: FnMut(&GraphStore, &nestweaver_schema::Vault) -> Result<(), anyhow::Error>,
    R: FnMut(&DaemonState, IndexedSearchMutation, &str) -> Result<(), anyhow::Error>,
{
    let search_rows_before = indexed_search_rows_before(state);
    let (removed_repos, mut error) = prune_stale_repos_with(&state.store, delete_repo);
    let mut removed_vaults = Vec::new();
    let mut removed_vault_uids = Vec::new();
    let mut vault_mutation_attempted = false;

    if error.is_none() {
        match state.store.list_vaults(None) {
            Ok(vaults) => {
                for vault in &vaults {
                    if !Path::new(&vault.root_path).exists() {
                        vault_mutation_attempted = true;
                        if let Err(delete_error) = delete_vault(&state.store, vault) {
                            error = Some(delete_error);
                            break;
                        }
                        removed_vaults.push(vault.name.clone());
                        removed_vault_uids.push(vault.uid.clone());
                    }
                }
            }
            Err(list_error) => {
                error = Some(anyhow::anyhow!("list_vaults failed: {list_error:#}"));
            }
        }
    }

    let changed = !removed_repos.is_empty()
        || !removed_vaults.is_empty()
        || (error.is_some() && vault_mutation_attempted);
    let mut failures = if !removed_repos.is_empty() {
        finalize_code_graph_deletion(state, &removed_repos.uids)
    } else if changed {
        finalize_node_graph_deletion(state, "prune_stale")
    } else {
        Vec::new()
    };
    reconcile_deleted_extension_uids(state, &removed_vault_uids, &mut failures);
    let search_mutation = if changed {
        indexed_search_mutation(search_rows_before, &state.store)
    } else {
        IndexedSearchMutation::Unchanged
    };
    if search_mutation != IndexedSearchMutation::Unchanged {
        append_search_reconciliation(
            &mut failures,
            reconcile_search(state, search_mutation, "prune_stale"),
        );
    }

    let mutation = match error {
        Some(error) => Err(Status::internal(format!("prune_stale failed: {error:#}"))),
        None => Ok(PruneStaleResponse {
            removed_repos: removed_repos.names,
            removed_vaults,
            committed: true,
            reconciliation_failures: to_proto_reconciliation_failures(&failures),
        }),
    };
    finish_reconciled_mutation(mutation, "prune_stale", failures)
}

fn run_purge_instance_with<F, R>(
    state: &DaemonState,
    instance_id: &str,
    purge: F,
    mut reconcile_search: R,
) -> Result<nestweaver_store::PurgeInstanceResult, Status>
where
    F: FnOnce(&GraphStore, &str) -> Result<nestweaver_store::PurgeInstanceResult, anyhow::Error>,
    R: FnMut(&DaemonState, IndexedSearchMutation, &str) -> Result<(), anyhow::Error>,
{
    let search_rows_before = indexed_search_rows_before(state);
    let mut repo_uids = list_instance_code_repo_uids(&state.store, instance_id)
        .map_err(|e| Status::internal(format!("PurgeInstance failed to list code repos: {e:#}")))?;
    let vault_prefix = format!("vlt:{instance_id}:");
    let mut vault_uids: Vec<_> = state
        .store
        .list_vaults(None)
        .map_err(|error| {
            Status::internal(format!("PurgeInstance failed to list Vaults: {error:#}"))
        })?
        .into_iter()
        .filter(|vault| vault.instance_id == instance_id || vault.uid.starts_with(&vault_prefix))
        .map(|vault| vault.uid)
        .collect();
    vault_uids.sort();
    vault_uids.dedup();
    let project_prefix = format!("proj:{instance_id}:");
    let mut project_uids: Vec<_> = state
        .store
        .list_projects()
        .map_err(|error| {
            Status::internal(format!("PurgeInstance failed to list Projects: {error:#}"))
        })?
        .into_iter()
        .filter(|project| {
            project.instance_id == instance_id || project.uid.starts_with(&project_prefix)
        })
        .map(|project| project.uid)
        .collect();
    project_uids.sort();
    project_uids.dedup();

    match purge(&state.store, instance_id) {
        Ok(result) => {
            repo_uids.extend(result.code_repo_uids.iter().cloned());
            repo_uids.sort();
            repo_uids.dedup();
            let code_changed = !repo_uids.is_empty()
                || result.repos > 0
                || result.files > 0
                || result.symbols > 0
                || result.code_orphans_swept > 0;
            let changed = code_changed
                || result.vaults > 0
                || result.projects > 0
                || result.orphans_swept > 0;
            let mut failures = if code_changed {
                finalize_code_graph_deletion(state, &repo_uids)
            } else if changed {
                finalize_node_graph_deletion(state, "purge_instance")
            } else {
                Vec::new()
            };
            reconcile_deleted_extension_uids(state, &vault_uids, &mut failures);
            reconcile_deleted_project_extensions(state, &project_uids, &mut failures);
            let search_mutation = if changed {
                indexed_search_mutation(search_rows_before, &state.store)
            } else {
                IndexedSearchMutation::Unchanged
            };
            if search_mutation != IndexedSearchMutation::Unchanged {
                append_search_reconciliation(
                    &mut failures,
                    reconcile_search(state, search_mutation, "purge_instance"),
                );
            }
            finish_reconciled_mutation(Ok(result), "purge_instance", failures)
        }
        Err(error) => {
            // `purge_instance` is intentionally non-transactional. A late
            // error can follow committed deletions. Both finalizers invalidate
            // PageRank; the code-repo preflight only selects whether code
            // sidecars also require reconciliation.
            let mut failures = if repo_uids.is_empty() {
                finalize_node_graph_deletion(state, "purge_instance_error")
            } else {
                finalize_code_graph_deletion(state, &repo_uids)
            };
            reconcile_deleted_extension_uids(state, &vault_uids, &mut failures);
            reconcile_deleted_project_extensions(state, &project_uids, &mut failures);
            let search_mutation = indexed_search_mutation(search_rows_before, &state.store);
            if search_mutation != IndexedSearchMutation::Unchanged {
                append_search_reconciliation(
                    &mut failures,
                    reconcile_search(state, search_mutation, "purge_instance_error"),
                );
            }
            finish_reconciled_mutation(
                Err(Status::internal(format!("PurgeInstance failed: {error:#}"))),
                "purge_instance_error",
                failures,
            )
        }
    }
}

fn reconcile_deleted_project_extensions(
    state: &DaemonState,
    project_uids: &[String],
    failures: &mut Vec<nestweaver_engine::DeletionReconciliationFailure>,
) {
    for project_uid in project_uids {
        match state.store.project_exists(project_uid) {
            Ok(true) => {}
            Ok(false) => {
                if let Err(error) =
                    nestweaver_engine::remove_extension_uid_durable(&state.db_path, project_uid)
                {
                    push_reconciliation_failure(
                        failures,
                        nestweaver_engine::DeletionReconciliationStage::ExtensionMetadata,
                        format!("Project {project_uid}: {error:#}"),
                    );
                }
            }
            Err(error) => push_reconciliation_failure(
                failures,
                nestweaver_engine::DeletionReconciliationStage::GraphLiveness,
                format!("Project {project_uid} liveness query failed: {error:#}"),
            ),
        }
    }
}

fn list_instance_code_repo_uids(
    store: &GraphStore,
    instance_id: &str,
) -> Result<Vec<String>, anyhow::Error> {
    let mut repo_uids = store.list_purge_code_repo_uids(instance_id)?;
    repo_uids.extend(
        store
            .list_repos(Some(instance_id))?
            .into_iter()
            .map(|repo| repo.uid),
    );
    repo_uids.sort();
    repo_uids.dedup();
    Ok(repo_uids)
}

fn recover_pending_instance_extension_migration(state: &DaemonState) -> Result<(), anyhow::Error> {
    let pending = nestweaver_engine::pending_instance_extension_migration(&state.db_path)?;
    if !pending.is_active() {
        return Ok(());
    }
    let from_id = pending
        .from_id()
        .ok_or_else(|| anyhow::anyhow!("pending extension migration is missing source instance"))?
        .to_string();
    let to_id = pending
        .to_id()
        .ok_or_else(|| anyhow::anyhow!("pending extension migration is missing target instance"))?
        .to_string();
    run_merge_instance_with(
        state,
        &from_id,
        &to_id,
        |store, from_id, to_id| {
            store
                .merge_instance_ids(from_id, to_id)
                .map_err(anyhow::Error::from)
        },
        rebuild_tantivy_after_mutation,
    )
    .map(|_| ())
    .map_err(|status| anyhow::anyhow!("startup extension migration recovery failed: {status}"))
}

fn run_merge_instance_with<F, R>(
    state: &DaemonState,
    from_id: &str,
    to_id: &str,
    merge: F,
    reconcile_search: R,
) -> Result<nestweaver_store::MergeResult, Status>
where
    F: FnOnce(&GraphStore, &str, &str) -> Result<nestweaver_store::MergeResult, anyhow::Error>,
    R: FnMut(&DaemonState, IndexedSearchMutation, &str) -> Result<(), anyhow::Error>,
{
    let mut reconcile_search = reconcile_search;
    let pending = nestweaver_engine::pending_instance_extension_migration(&state.db_path).map_err(
        |error| {
            Status::internal(format!(
                "merge_instance migration-journal preparation failed: {error:#}"
            ))
        },
    )?;
    let (migration, graph_already_applied) = if pending.is_active() {
        if pending.from_id() != Some(from_id) || pending.to_id() != Some(to_id) {
            return Err(Status::internal(format!(
                "pending instance migration {:?} -> {:?} conflicts with requested {from_id:?} -> {to_id:?}",
                pending.from_id(),
                pending.to_id()
            )));
        }
        state
            .store
            .recover_missing_instance_roots(
                to_id,
                &pending.repo_recoveries(),
                &pending.vault_recoveries(),
                &pending.project_recoveries(),
            )
            .map_err(|error| {
                Status::internal(format!(
                    "recover pending instance root insertion failed: {error:#}"
                ))
            })?;
        let remaps = pending.uid_remaps();
        let graph_state = if remaps.is_empty() {
            if state
                .store
                .list_vaults(Some(from_id))
                .map_err(|error| {
                    Status::internal(format!(
                        "verify pending vault migration state failed: {error:#}"
                    ))
                })?
                .is_empty()
            {
                nestweaver_store::InstanceUidRemapPlanState::Applied
            } else {
                nestweaver_store::InstanceUidRemapPlanState::Prepared
            }
        } else {
            state
                .store
                .verify_instance_uid_remap_plan_state(from_id, to_id, &remaps)
                .map_err(|error| {
                    Status::internal(format!(
                        "verify pending instance migration graph state failed: {error:#}"
                    ))
                })?
        };
        // nw-091 / Bug 3B: SELF-HEAL instead of wedging the daemon. A journal
        // marked `graph_applied` while the graph still holds source rows is a
        // durability inversion (the journal sidecar landed, the DB commit didn't).
        // `merge_instance_ids` is idempotent (reparented vaults are skipped by the
        // instance_id guard), so we re-derive the ACTUAL applied state and re-run
        // the merge below rather than erroring with no forward path. The `verify_`
        // call above already failed closed on a non-reproducible plan, so reaching
        // here means the plan is still reproducible and safe to re-drive.
        let graph_already_applied =
            graph_state == nestweaver_store::InstanceUidRemapPlanState::Applied;
        if pending.graph_applied() && !graph_already_applied {
            tracing::warn!(
                from_id,
                to_id,
                "instance migration journal marked graph-applied but source rows remain; \
                 re-driving the idempotent merge to self-heal"
            );
        }
        (pending, graph_already_applied)
    } else {
        let plan = state
            .store
            .plan_instance_uid_migration(from_id, to_id)
            .map_err(|error| {
                Status::internal(format!(
                    "merge failed to plan instance UID remaps: {error:#}"
                ))
            })?;
        let repo_uids = list_instance_code_repo_uids(&state.store, from_id).map_err(|error| {
            Status::internal(format!("merge failed to list code repos: {error:#}"))
        })?;
        let search_reconciliation_required = !state
            .store
            .list_vaults(Some(from_id))
            .map_err(|error| {
                Status::internal(format!("merge failed to project source vaults: {error:#}"))
            })?
            .is_empty();
        let fresh_migration = nestweaver_engine::prepare_instance_uid_migration_with_finalizers(
            &state.db_path,
            from_id,
            to_id,
            &plan,
            &nestweaver_engine::InstanceMigrationFinalizerPlan {
                repo_uids,
                search_reconciliation_required,
            },
        )
        .map_err(|error| {
            Status::internal(format!(
                "merge_instance migration-journal preparation failed: {error:#}"
            ))
        })?;
        // A freshly prepared migration has not mutated the graph yet.
        (fresh_migration, false)
    };

    if migration.reconciled() {
        nestweaver_engine::finalize_instance_extension_migration(&state.db_path, &migration)
            .map_err(|error| {
                Status::internal(format!(
                    "merge_instance extension-metadata completion failed: {error:#}"
                ))
            })?;
        return Ok(empty_instance_merge_result());
    }

    // Use the ACTUAL applied state, not just the journal's graph_applied flag, so a
    // journal that claims applied while the graph still has source rows (a
    // durability inversion) re-drives the idempotent merge instead of skipping it
    // (nw-091 / Bug 3B).
    let mutation = if graph_already_applied {
        Ok(empty_instance_merge_result())
    } else {
        merge(&state.store, from_id, to_id)
    };
    let result = match mutation {
        Ok(result) => result,
        Err(error) => {
            let mut failures = if migration.finalizer_repo_uids().is_empty() {
                finalize_node_graph_deletion(state, "merge_instance_error")
            } else {
                finalize_code_graph_deletion(state, migration.finalizer_repo_uids())
            };
            if migration.search_reconciliation_required() {
                append_search_reconciliation(
                    &mut failures,
                    reconcile_search(
                        state,
                        IndexedSearchMutation::Changed,
                        "merge_instance_error",
                    ),
                );
            }
            return finish_reconciled_mutation(
                Err(Status::internal(format!(
                    "merge_instance_ids failed: {error:#}"
                ))),
                "merge_instance_error",
                failures,
            );
        }
    };

    if !migration.is_active() {
        return Ok(result);
    }
    let graph_applied = if migration.graph_applied() {
        migration
    } else {
        nestweaver_engine::mark_instance_extension_migration_graph_applied(
            &state.db_path,
            &migration,
        )
        .map_err(|error| {
            Status::internal(format!(
                "merge_instance graph-applied journal persistence failed: {error:#}"
            ))
        })?
    };

    let mut failures = if graph_applied.finalizer_repo_uids().is_empty() {
        finalize_node_graph_deletion(state, "merge_instance")
    } else {
        finalize_code_graph_deletion(state, graph_applied.finalizer_repo_uids())
    };
    if graph_applied.search_reconciliation_required() {
        append_search_reconciliation(
            &mut failures,
            reconcile_search(state, IndexedSearchMutation::Changed, "merge_instance"),
        );
    }
    if !failures.is_empty() {
        return finish_reconciled_mutation(Ok(result), "merge_instance", failures);
    }

    let reconciled = nestweaver_engine::mark_instance_extension_migration_reconciled(
        &state.db_path,
        &graph_applied,
    )
    .map_err(|error| {
        Status::internal(format!(
            "merge_instance reconciled journal persistence failed: {error:#}"
        ))
    })?;
    nestweaver_engine::finalize_instance_extension_migration(&state.db_path, &reconciled).map_err(
        |error| {
            Status::internal(format!(
                "merge_instance extension-metadata completion failed: {error:#}"
            ))
        },
    )?;
    Ok(result)
}

fn empty_instance_merge_result() -> nestweaver_store::MergeResult {
    nestweaver_store::MergeResult {
        vaults: 0,
        repos: 0,
        projects: 0,
        discarded: Vec::new(),
        repos_moved: Vec::new(),
        repo_uids_removed: Vec::new(),
    }
}

#[cfg(test)]
fn run_merge_instance_with_extension_ops<F, R, P, Prepare, Complete>(
    state: &DaemonState,
    from_id: &str,
    to_id: &str,
    merge: F,
    mut reconcile_search: R,
    prepare_extensions: Prepare,
    complete_extensions: Complete,
) -> Result<nestweaver_store::MergeResult, Status>
where
    F: FnOnce(&GraphStore, &str, &str) -> Result<nestweaver_store::MergeResult, anyhow::Error>,
    R: FnMut(&DaemonState, IndexedSearchMutation, &str) -> Result<(), anyhow::Error>,
    Prepare: FnOnce(&GraphStore, &Path, &str, &str) -> Result<(P, bool), anyhow::Error>,
    Complete: FnOnce(&Path, &P) -> Result<(), anyhow::Error>,
{
    let search_rows_before = indexed_search_rows_before(state);
    let repo_uids = list_instance_code_repo_uids(&state.store, from_id)
        .map_err(|e| Status::internal(format!("merge failed to list code repos: {e:#}")))?;
    let (extension_migration, extension_migration_active) =
        prepare_extensions(&state.store, &state.db_path, from_id, to_id).map_err(|error| {
            Status::internal(format!(
                "merge_instance extension-metadata preparation failed: {error:#}"
            ))
        })?;

    match merge(&state.store, from_id, to_id) {
        Ok(result) => {
            let changed = !result.repo_uids_removed.is_empty()
                || result.vaults > 0
                || result.repos > 0
                || result.projects > 0
                || extension_migration_active;
            let mut failures = Vec::new();
            if let Err(error) = complete_extensions(&state.db_path, &extension_migration) {
                push_reconciliation_failure(
                    &mut failures,
                    nestweaver_engine::DeletionReconciliationStage::ExtensionMetadata,
                    format!("instance {from_id} -> {to_id}: {error:#}"),
                );
            }
            failures.extend(if !result.repo_uids_removed.is_empty() {
                finalize_code_graph_deletion(state, &result.repo_uids_removed)
            } else if changed {
                finalize_node_graph_deletion(state, "merge_instance")
            } else {
                Vec::new()
            });
            let search_mutation = if changed {
                indexed_search_mutation(search_rows_before, &state.store)
            } else {
                IndexedSearchMutation::Unchanged
            };
            if search_mutation != IndexedSearchMutation::Unchanged {
                append_search_reconciliation(
                    &mut failures,
                    reconcile_search(state, search_mutation, "merge_instance"),
                );
            }
            finish_reconciled_mutation(Ok(result), "merge_instance", failures)
        }
        Err(error) => {
            // Instance merge is multi-statement and can commit earlier source
            // entries before a later one fails. Both finalizers invalidate
            // PageRank; the code-repo preflight only selects whether code
            // sidecars also require reconciliation.
            let mut failures = if repo_uids.is_empty() {
                finalize_node_graph_deletion(state, "merge_instance_error")
            } else {
                finalize_code_graph_deletion(state, &repo_uids)
            };
            let search_mutation = indexed_search_mutation(search_rows_before, &state.store);
            if search_mutation != IndexedSearchMutation::Unchanged {
                append_search_reconciliation(
                    &mut failures,
                    reconcile_search(state, search_mutation, "merge_instance_error"),
                );
            }
            finish_reconciled_mutation(
                Err(Status::internal(format!(
                    "merge_instance_ids failed: {error:#}"
                ))),
                "merge_instance_error",
                failures,
            )
        }
    }
}

fn run_remove_vault_with_projection(
    state: &DaemonState,
    vault_uid: &str,
    search_rows_before: Option<
        Result<std::collections::HashSet<IndexedSearchDocument>, anyhow::Error>,
    >,
) -> Result<RemoveVaultResponse, Status> {
    let mutation = state
        .store
        .delete_vault_cascade_with_outcome(vault_uid)
        .map_err(|error| Status::internal(format!("delete_vault_cascade failed: {error:#}")));
    let confirmed_noop = matches!(&mutation, Ok(outcome) if !outcome.changed);
    let mut failures = Vec::new();
    if !confirmed_noop {
        failures = finalize_node_graph_deletion(state, "remove_vault");
        reconcile_deleted_extension_uids(state, &[vault_uid.to_string()], &mut failures);
        let search_mutation = indexed_search_mutation(search_rows_before, &state.store);
        if search_mutation != IndexedSearchMutation::Unchanged {
            append_search_reconciliation(
                &mut failures,
                rebuild_tantivy_after_mutation(state, search_mutation, "remove_vault"),
            );
        }
    }

    let reconciliation_failures = to_proto_reconciliation_failures(&failures);
    finish_reconciled_mutation(
        mutation.map(|outcome| RemoveVaultResponse {
            notes_deleted: outcome.notes_deleted as u64,
            // committed reflects whether the delete actually changed durable state,
            // so a confirmed no-op stays distinguishable from a committed delete
            // (nw-091 / Bug 2 — "nothing happened" must remain a distinct signal).
            committed: outcome.changed,
            reconciliation_failures,
        }),
        "remove_vault",
        failures,
    )
}

#[derive(Clone, Copy)]
struct EmbeddingScopes {
    symbols: bool,
    notes: bool,
    headings: bool,
}

fn embedding_scopes(scope: &str) -> Result<EmbeddingScopes, Status> {
    let scopes = EmbeddingScopes {
        symbols: scope == "all" || scope == "symbols",
        notes: scope == "all" || scope == "notes",
        headings: scope == "all" || scope == "headings",
    };
    if !scopes.symbols && !scopes.notes && !scopes.headings {
        return Err(Status::invalid_argument(format!(
            "unknown scope '{scope}': expected one of: all, symbols, notes, headings"
        )));
    }
    Ok(scopes)
}

fn embedding_is_eligible(store: &GraphStore, uid: &str, force: bool) -> bool {
    force || !store.has_embedding(uid)
}

fn embedding_store_status(operation: &str, error: anyhow::Error) -> Status {
    tracing::error!(operation, "embedding eligibility query failed: {error:#}");
    Status::internal(format!("failed to inspect embedding {operation}"))
}

fn plan_embeddings(store: &GraphStore, scope: &str, force: bool) -> Result<EmbedResponse, Status> {
    let scopes = embedding_scopes(scope)?;
    let mut scoped = 0u64;
    let mut eligible = 0u64;

    if scopes.symbols {
        let symbols = store
            .list_all_symbols()
            .map_err(|error| embedding_store_status("symbols", error.into()))?;
        scoped += symbols.len() as u64;
        eligible += symbols
            .iter()
            .filter(|symbol| embedding_is_eligible(store, &symbol.uid, force))
            .count() as u64;
    }
    if scopes.notes {
        let notes = store
            .list_notes(None)
            .map_err(|error| embedding_store_status("notes", error.into()))?;
        scoped += notes.len() as u64;
        eligible += notes
            .iter()
            .filter(|note| embedding_is_eligible(store, &note.uid, force))
            .count() as u64;
    }
    if scopes.headings {
        let headings = store
            .list_all_headings()
            .map_err(|error| embedding_store_status("headings", error.into()))?;
        scoped += headings.len() as u64;
        eligible += headings
            .iter()
            .filter(|heading| embedding_is_eligible(store, &heading.uid, force))
            .count() as u64;
    }

    Ok(EmbedResponse {
        scoped,
        eligible,
        skipped: scoped.saturating_sub(eligible),
        ..EmbedResponse::default()
    })
}

#[tonic::async_trait]
impl NestWeaverDaemon for DaemonService {
    // ── Lifecycle ───────────────────────────────────────────────────

    async fn health_check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let uptime = self.state.start_time.elapsed().as_secs();
        let active = self.state.active_reads.load(Ordering::Relaxed)
            + self.state.active_writes.load(Ordering::Relaxed);
        Ok(Response::new(HealthCheckResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            instance_id: self.state.instance_id.clone(),
            db_path: self.state.db_path.display().to_string(),
            uptime_seconds: uptime,
            active_connections: active,
            // The daemon's own PID, so the CLI can cross-check a pidfile
            // PID against the socket-reported PID before signaling it.
            pid: std::process::id(),
        }))
    }

    async fn shutdown(
        &self,
        request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        tracing::info!("shutdown requested via gRPC — draining active writes");

        // T6.2: mark the pool drained BEFORE the drain wait loop so the worker
        // stops claiming NEW jobs immediately and only finishes in-flight work.
        // Without this the worker keeps claiming new jobs during the drain, so
        // under continuous webhook enqueue `indexing_active` never clears and
        // shutdown burns the full drain ceiling doing work it will abandon.
        self.state.drained.store(true, Ordering::Relaxed);

        // Stop any active watcher BEFORE the drain wait — an orphaned
        // watcher's blocking thread would otherwise pin shutdown until the
        // client's SIGKILL.
        stop_active_watcher(&self.state);

        let state = self.state.clone();
        tokio::spawn(async move {
            let ceiling = nestweaver_schema::drain_ceiling_from_env();

            let timeout = std::time::Duration::from_secs(ceiling);
            let half = std::time::Duration::from_secs(ceiling / 2);
            let ninety = std::time::Duration::from_secs(ceiling * 9 / 10);
            let start = tokio::time::Instant::now();
            let mut warned_half = false;
            let mut warned_ninety = false;

            loop {
                let writes = state.active_writes.load(Ordering::Relaxed);
                // Index jobs bump `indexing_active`, not `active_writes`, so the
                // drain must wait on both — otherwise a shutdown could proceed
                // while the worker is mid-write.
                let indexing = state.indexing_active.load(Ordering::Relaxed);
                if writes == 0 && !indexing {
                    tracing::info!("no active writes or indexing — shutting down");
                    break;
                }

                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    tracing::warn!(
                        active_writes = writes,
                        "drain timeout ({ceiling}s) reached — forcing shutdown"
                    );
                    break;
                }

                if !warned_half && elapsed >= half {
                    tracing::warn!(
                        active_writes = writes,
                        "drain at 50% of timeout ({ceiling}s)"
                    );
                    warned_half = true;
                }
                if !warned_ninety && elapsed >= ninety {
                    tracing::warn!(
                        active_writes = writes,
                        "drain at 90% of timeout ({ceiling}s)"
                    );
                    warned_ninety = true;
                }

                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            let _ = state.shutdown_tx.send(true);
        });

        Ok(Response::new(ShutdownResponse { ok: true }))
    }

    // ── Backup ──────────────────────────────────────────────────────

    async fn backup(
        &self,
        request: Request<BackupRequest>,
    ) -> Result<Response<BackupResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let req = request.into_inner();
        if req.output_path.trim().is_empty() {
            return Err(Status::invalid_argument("output_path is required"));
        }
        let config = nestweaver_engine::BackupConfig {
            db_path: self.state.db_path.clone(),
            output_path: std::path::PathBuf::from(&req.output_path),
            include_clones: req.include_clones,
            // nw-019: the manifest's instance_id is a DATA claim about the backed-up
            // contents, so stamp the logical instance (config name when set, else the
            // runtime hash) — not the runtime hash unconditionally. Restore keys
            // nothing on this field (checksum + schema-compat only), so it is safe.
            instance_id: self.state.data_instance_id.clone(),
            workspace_path: if req.include_clones {
                self.state.db_path.parent().map(|p| p.join("workspace"))
            } else {
                None
            },
        };

        // Stage the backup while HOLDING the write lock. Quiesce == lock-held:
        // no writer touches the files mid-copy, and the RAII guard releases the
        // lock on drop/panic — there is no persistent quiesce flag to leak.
        let staged = {
            let _write_lock = self.state.write_mutex.lock().await;
            let _guard = ConnectionGuard::write(&self.state);
            let store = self.state.store.clone();
            let cfg = config.clone();
            tracing::info!("backup: staging under write lock");
            tokio::task::spawn_blocking(move || {
                nestweaver_engine::stage_backup_from_store(&store, &cfg)
            })
            .await
            .map_err(|e| Status::internal(format!("backup staging panicked: {e}")))?
            .map_err(|e| Status::internal(format!("backup staging failed: {e}")))?
        }; // write lock released here — writers resume while we package below

        let cfg = config.clone();
        let result =
            tokio::task::spawn_blocking(move || nestweaver_engine::package_staged(&cfg, staged))
                .await
                .map_err(|e| Status::internal(format!("backup packaging panicked: {e}")))?
                .map_err(|e| Status::internal(format!("backup packaging failed: {e}")))?;

        let m = &result.manifest;
        tracing::info!(output = %result.output_path.display(), "backup complete");
        Ok(Response::new(BackupResponse {
            output_path: result.output_path.to_string_lossy().into_owned(),
            instance_id: m.instance_id.clone(),
            tier: m.tier.clone(),
            nestweaver_version: m.nestweaver_version.clone(),
            repo_count: m.repo_count as u64,
            symbol_count: m.symbol_count as u64,
            db_size_bytes: m.sizes.db,
            total_compressed: m.sizes.total_compressed,
        }))
    }

    // ── Watching ─────────────────────────────────────────────────────

    async fn watch_vault(
        &self,
        request: Request<WatchVaultRequest>,
    ) -> Result<Response<WatchVaultResponse>, Status> {
        if self.state.server_mode {
            return Err(Status::unimplemented(
                "watchers are server-managed in server mode",
            ));
        }
        let req = request.into_inner();
        let vault_path = PathBuf::from(&req.vault_path);
        let vault_name = req.vault_name.clone();
        let instance_id =
            resolve_effective_instance_id(&req.instance_id, &self.state.data_instance_id)?;
        let extra_patterns = req.extra_ignore_patterns.clone();

        if !vault_path.exists() || !vault_path.is_dir() {
            return Ok(Response::new(WatchVaultResponse {
                ok: false,
                message: format!("vault path is not a directory: {}", vault_path.display()),
            }));
        }

        let vault_path = vault_path.canonicalize().map_err(|e| {
            Status::invalid_argument(format!("cannot canonicalize vault path: {e}"))
        })?;

        // Only allow paths registered in the instance config; without a
        // config, fall back to the unsafe-root denylist.
        watch_path_allowed(
            self.state.instance_cfg.as_ref().map(|c| c.repos.as_slice()),
            &vault_path,
            "vault",
            true,
        )?;

        let db_path = self.state.db_path.clone();
        let manifests_path = nestweaver_engine::sidecar_path(&db_path, ".manifests.json");
        let tantivy_path = nestweaver_mcp::tantivy_sidecar_path(&db_path);

        // Build the watcher. Use the daemon's Tantivy index if it has a
        // writer; otherwise let the watcher open from the path.
        let mut watcher =
            nestweaver_engine::BrainWatcher::new(&db_path, &vault_path, &instance_id, &vault_name)
                .with_manifests_path(&manifests_path)
                .with_extra_ignore_patterns(&extra_patterns);

        // Share the daemon's writer-mode Tantivy handle with the watcher
        // so live edits update BM25 in place. Opening a separate handle
        // (reader-only or read-write) would either silently no-op writes
        // or collide on the writer lock.
        if let Some(ref tantivy) = self.state.tantivy
            && tantivy.has_writer()
        {
            watcher = watcher.with_external_tantivy(Arc::clone(tantivy));
        } else {
            watcher = watcher.with_tantivy_index(&tantivy_path);
        }

        let shutdown_handle = watcher.shutdown_handle();

        // register_watcher holds the lock across check + store (TOCTOU-safe).
        let watcher_id = match register_watcher(&self.state, shutdown_handle, false) {
            Ok(id) => id,
            Err(e) if e.code() == tonic::Code::AlreadyExists => {
                return Ok(Response::new(WatchVaultResponse {
                    ok: false,
                    message: "A watcher is already running. Stop it first with StopWatch."
                        .to_string(),
                }));
            }
            Err(e) => return Err(e),
        };

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();
        let state = self.state.clone();
        let store = self.state.store.clone();
        let on_change = Self::make_embed_on_change(
            self.state.embedding_runtime.clone(),
            self.state.store.clone(),
        );

        tokio::task::spawn_blocking(move || {
            let _write_lock = write_lock.blocking_lock();
            let _guard = guard;
            tracing::info!(vault = %vault_path.display(), "watcher thread started");

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                watcher.run_with_store(store, on_change)
            }));

            match result {
                Ok(Ok(())) => tracing::info!("watcher exited cleanly"),
                Ok(Err(e)) => tracing::error!(error = %e, "watcher exited with error"),
                Err(_) => tracing::error!("watcher thread panicked"),
            }

            clear_watcher_registration(&state, watcher_id);
        });

        Ok(Response::new(WatchVaultResponse {
            ok: true,
            message: format!(
                "Watcher started for {} (vault: {})",
                req.vault_path, vault_name
            ),
        }))
    }

    async fn watch_code(
        &self,
        request: Request<WatchCodeRequest>,
    ) -> Result<Response<WatchCodeResponse>, Status> {
        if self.state.server_mode {
            return Err(Status::unimplemented(
                "watchers are server-managed in server mode",
            ));
        }
        let req = request.into_inner();
        let repo_path = PathBuf::from(&req.repo_path);
        let force = req.force;
        let instance_id =
            resolve_effective_instance_id(&req.instance_id, &self.state.data_instance_id)?;

        if !repo_path.exists() || !repo_path.is_dir() {
            return Ok(Response::new(WatchCodeResponse {
                ok: false,
                message: format!("repo path is not a directory: {}", repo_path.display()),
            }));
        }

        let repo_path = repo_path
            .canonicalize()
            .map_err(|e| Status::invalid_argument(format!("cannot canonicalize repo path: {e}")))?;

        // Only allow paths registered in the instance config; without a
        // config, fall back to the unsafe-root denylist.
        watch_path_allowed(
            self.state.instance_cfg.as_ref().map(|c| c.repos.as_slice()),
            &repo_path,
            "repo",
            false,
        )?;

        let db_path = self.state.db_path.clone();

        let watcher = nestweaver_engine::CodeWatcher::new(&db_path, &repo_path, &instance_id);
        let shutdown_handle = watcher.shutdown_handle();

        // register_watcher holds the lock across check + store (TOCTOU-safe).
        // With `force`, an already-running watcher (e.g. orphaned by a
        // kill -9'd `watch` CLI) is stopped and replaced instead of
        // failing every new watch session.
        let watcher_id = match register_watcher(&self.state, shutdown_handle, force) {
            Ok(id) => id,
            Err(e) if e.code() == tonic::Code::AlreadyExists => {
                return Ok(Response::new(WatchCodeResponse {
                    ok: false,
                    message: "A watcher is already running. Stop it first with StopWatch \
                              (or retry with --force)."
                        .to_string(),
                }));
            }
            Err(e) => return Err(e),
        };

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();
        let state = self.state.clone();
        let store = self.state.store.clone();
        let on_change = Self::make_embed_on_change(
            self.state.embedding_runtime.clone(),
            self.state.store.clone(),
        );

        tokio::task::spawn_blocking(move || {
            let _write_lock = write_lock.blocking_lock();
            let _guard = guard;
            tracing::info!(repo = %repo_path.display(), "code watcher thread started");

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                watcher.run_with_store(store, on_change)
            }));

            match result {
                Ok(Ok(())) => tracing::info!("code watcher exited cleanly"),
                Ok(Err(e)) => tracing::error!(error = %e, "code watcher exited with error"),
                Err(_) => tracing::error!("code watcher thread panicked"),
            }

            clear_watcher_registration(&state, watcher_id);
        });

        Ok(Response::new(WatchCodeResponse {
            ok: true,
            message: format!("Code watcher started for {}", req.repo_path,),
        }))
    }

    async fn stop_watch(
        &self,
        _request: Request<StopWatchRequest>,
    ) -> Result<Response<StopWatchResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            _request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let mut guard = self
            .state
            .watcher_stop
            .lock()
            .map_err(|e| Status::internal(format!("watcher_stop lock poisoned: {e}")))?;

        if let Some(reg) = guard.take() {
            tracing::info!(watcher_id = reg.id, "stop_watch: stopping active watcher");
            reg.handle.stop();
            Ok(Response::new(StopWatchResponse { ok: true }))
        } else {
            Ok(Response::new(StopWatchResponse { ok: false }))
        }
    }

    // ── Export ───────────────────────────────────────────────────────

    #[allow(clippy::result_large_err)]
    async fn export_graph(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let format = args
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("cypher");
            let top = args.get("top").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let output_path = args.get("output").and_then(|v| v.as_str());

            match format {
                "cypher" | "graphml" | "mermaid" => {
                    let mut buf = Vec::new();
                    match format {
                        "cypher" => nestweaver_engine::export_cypher(&state.store, &mut buf),
                        "graphml" => nestweaver_engine::export_graphml(&state.store, &mut buf),
                        "mermaid" => nestweaver_engine::export_mermaid(&state.store, top, &mut buf),
                        _ => unreachable!(),
                    }
                    .map_err(|e| Status::internal(format!("export failed: {e:#}")))?;

                    let text = String::from_utf8(buf)
                        .map_err(|e| Status::internal(format!("export produced non-UTF-8: {e}")))?;

                    if let Some(path) = output_path {
                        if state.server_mode {
                            return Err(Status::permission_denied(
                                "file output is disabled in server mode; use the returned text field",
                            ));
                        }
                        std::fs::write(path, &text).map_err(|e| {
                            Status::internal(format!("failed to write {path}: {e}"))
                        })?;
                    }

                    serde_json::to_string(&serde_json::json!({
                        "format": format,
                        "bytes": text.len(),
                        "text": if output_path.is_some() { None } else { Some(&text) },
                        "output": output_path,
                    }))
                    .map_err(|e| Status::internal(format!("json serialize failed: {e:#}")))
                }
                "msgpack" => {
                    if state.server_mode {
                        return Err(Status::permission_denied(
                            "msgpack export writes to disk and is disabled in server mode; export locally",
                        ));
                    }

                    let graph = nestweaver_engine::export_in_memory_graph(&state.store)
                        .map_err(|e| Status::internal(format!("export failed: {e:#}")))?;
                    let bytes = rmp_serde::to_vec(&graph).map_err(|e| {
                        Status::internal(format!("msgpack serialize failed: {e:#}"))
                    })?;

                    // For msgpack, always write to a file (the binary data is too
                    // large for JSON transport). If no output path is given, use
                    // a default next to the DB.
                    let path = match output_path {
                        Some(p) => PathBuf::from(p),
                        None => {
                            let mut name = state
                                .db_path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned();
                            name.push_str(".graph.msgpack");
                            state
                                .db_path
                                .parent()
                                .unwrap_or(std::path::Path::new("."))
                                .join(name)
                        }
                    };
                    std::fs::write(&path, &bytes).map_err(|e| {
                        Status::internal(format!("failed to write {}: {e}", path.display()))
                    })?;

                    serde_json::to_string(&serde_json::json!({
                        "format": "msgpack",
                        "output": path.display().to_string(),
                        "nodes": graph.uids.len(),
                        "edges": graph.edges.len(),
                        "bytes": bytes.len(),
                    }))
                    .map_err(|e| Status::internal(format!("json serialize failed: {e:#}")))
                }
                other => Err(Status::invalid_argument(format!(
                    "unknown format '{other}'; supported: cypher, graphml, mermaid, msgpack"
                ))),
            }
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    // ── UI serving ───────────────────────────────────────────────────

    async fn serve_ui(
        &self,
        request: Request<ServeUiRequest>,
    ) -> Result<Response<ServeUiResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let req = request.into_inner();
        let state = self.state.clone();
        let watch_instance =
            resolve_effective_instance_id(&req.watch_instance_id, &state.data_instance_id)?;

        let app_state = nestweaver_web::state::AppState::new_with_arc_tantivy(
            state.store.clone(),
            state.tantivy.clone(),
            state.db_path.clone(),
        );

        // nw-029: pre-warm PageRank so the first overview/impact query never pays
        // the lazy compute. Fire-and-forget; single-flight (nw-029 T1) makes a
        // concurrent first query wait on this instead of duplicating it. A DB
        // whose sidecar was loaded at open is a no-op (ensure_pagerank_loaded's
        // is_some() fast path).
        {
            let store = state.store.clone();
            tokio::task::spawn_blocking(move || {
                store.ensure_pagerank_loaded();
            });
        }

        let port = if req.port > 0 { req.port as u16 } else { 3000 };
        let open_browser = req.open_browser;

        // LOW (ui port leak): re-asking for the UI while it is already served
        // is a no-op success, but a FOREIGN process bound to the port must be a
        // clear error — the old code spawned a task whose bind failure was only
        // logged, and reported ok:true regardless.
        {
            let guard = self
                .state
                .ui_server
                .lock()
                .map_err(|e| Status::internal(format!("ui_server lock poisoned: {e}")))?;
            if let Some((running_port, handle)) = guard.as_ref()
                && !handle.is_finished()
            {
                // A watch request must not be silently dropped just because
                // the UI is already served — start the watcher here too.
                if req.watch && !req.watch_repo_path.is_empty() {
                    let watch_db = state.db_path.clone();
                    let watch_repo = std::path::PathBuf::from(&req.watch_repo_path);
                    let watch_store = state.store.clone();
                    let watch_instance = watch_instance.clone();

                    tokio::task::spawn_blocking(move || {
                        let watcher = nestweaver_engine::CodeWatcher::new(
                            &watch_db,
                            &watch_repo,
                            &watch_instance,
                        );
                        if let Err(e) = watcher.run_with_store(watch_store, None) {
                            tracing::error!("CodeWatcher failed: {e}");
                        }
                    });
                    // Report the ACTUAL running port, not the requested one —
                    // the CLI prints this port in the URL it shows the user.
                    return Ok(Response::new(ServeUiResponse {
                        ok: true,
                        message: format!(
                            "UI server already running on port {running_port}; watcher started for {}",
                            req.watch_repo_path
                        ),
                        port: u32::from(*running_port),
                        error: String::new(),
                    }));
                }
                // Report the ACTUAL running port, not the requested one —
                // the CLI prints this port in the URL it shows the user.
                return Ok(Response::new(ServeUiResponse {
                    ok: true,
                    message: format!("UI server already running on port {running_port}"),
                    port: u32::from(*running_port),
                    error: String::new(),
                }));
            }
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
            return Ok(Response::new(ServeUiResponse {
                ok: false,
                message: format!(
                    "port {port} is already in use by another process — pick another --port"
                ),
                port: 0,
                error: "port_in_use".to_string(),
            }));
        }

        // Build web UI router, mounting the admin API when available so the
        // admin dashboard SPA can reach its backend on the same origin.
        let mut web_router = nestweaver_web::create_router(app_state);
        if let Some(admin_state) = state.admin_state.get() {
            let device_router = nestweaver_web::create_device_flow_router(admin_state.clone());
            web_router = web_router.nest("/auth", device_router);
            let admin_router = nestweaver_web::create_admin_router(admin_state.clone());
            web_router = web_router.nest("/admin/api", admin_router);
            tracing::info!("admin API also mounted on web UI server");
        }

        // Spawn web server as a background task inside the daemon. The handle
        // is tracked so `stop_ui` can abort it and release the listen port
        // (LOW: ui port leak).
        let handle = tokio::spawn(async move {
            if let Err(e) =
                nestweaver_web::start_server_with_router(web_router, port, open_browser).await
            {
                tracing::error!("UI server error: {e}");
            }
        });
        {
            let mut guard = self
                .state
                .ui_server
                .lock()
                .map_err(|e| Status::internal(format!("ui_server lock poisoned: {e}")))?;
            *guard = Some((port, handle));
        }

        // If watch mode requested, spawn a CodeWatcher.
        if req.watch && !req.watch_repo_path.is_empty() {
            let watch_db = state.db_path.clone();
            let watch_repo = std::path::PathBuf::from(&req.watch_repo_path);
            let watch_store = state.store.clone();

            tokio::task::spawn_blocking(move || {
                let watcher =
                    nestweaver_engine::CodeWatcher::new(&watch_db, &watch_repo, &watch_instance);
                if let Err(e) = watcher.run_with_store(watch_store, None) {
                    tracing::error!("CodeWatcher failed: {e}");
                }
            });
        }

        Ok(Response::new(ServeUiResponse {
            ok: true,
            message: format!("UI server started on port {port}"),
            port: u32::from(port),
            error: String::new(),
        }))
    }

    async fn stop_ui(
        &self,
        request: Request<StopUiRequest>,
    ) -> Result<Response<StopUiResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let handle = {
            let mut guard = self
                .state
                .ui_server
                .lock()
                .map_err(|e| Status::internal(format!("ui_server lock poisoned: {e}")))?;
            guard.take()
        };
        match handle {
            Some((_port, handle)) if !handle.is_finished() => {
                // Aborting the task drops the axum listener, releasing the port.
                handle.abort();
                Ok(Response::new(StopUiResponse {
                    ok: true,
                    message: "UI server stopped".to_string(),
                }))
            }
            _ => Ok(Response::new(StopUiResponse {
                ok: false,
                message: "UI server is not running".to_string(),
            })),
        }
    }

    // ── Indexing ─────────────────────────────────────────────────────

    type IndexRepoStream = ProgressStream;

    async fn index_repo(
        &self,
        request: Request<IndexRepoRequest>,
    ) -> Result<Response<Self::IndexRepoStream>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let req = request.into_inner();
        // Canonicalize so a relative or `.` repo path (which a detached daemon
        // resolves against CWD=/) is caught before it walks the whole filesystem,
        // then refuse a system root outright.
        let repo_path = PathBuf::from(&req.repo_path);
        let repo_path = repo_path.canonicalize().unwrap_or(repo_path);
        if is_unsafe_index_root(&repo_path) {
            return Err(Status::invalid_argument(format!(
                "refusing to index '{}': a system root would walk the entire filesystem — \
                 pass an absolute path to a specific repository",
                repo_path.display()
            )));
        }
        let state = self.state.clone();
        let force = req.force;
        let with_trigrams = req.with_trigrams;
        let with_git_activity = req.with_git_activity;
        let name = if req.name.is_empty() {
            None
        } else {
            Some(req.name.clone())
        };
        let effective_instance =
            resolve_effective_instance_id(&req.instance_id, &state.data_instance_id)?;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<IndexProgress, Status>>(16);

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();

        // Cooperative cancellation for the otherwise-uncancelable spawn_blocking
        // index. A watchdog trips this flag when the index exceeds an overall
        // timeout OR when the requesting client disconnects; the engine observes
        // it at the pre-write boundary and aborts without a partial write. The
        // `done` oneshot lets the watchdog tear down as soon as the index
        // finishes — otherwise its extra stream sender would keep the client's
        // response stream open forever.
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let index_timeout = std::time::Duration::from_secs(
            std::env::var("NESTWEAVER_INDEX_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|s| *s > 0)
                .unwrap_or(1800),
        );
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        {
            let cancel = cancel.clone();
            let watch_tx = tx.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(index_timeout) => {
                        tracing::warn!(?index_timeout, "index exceeded timeout; cancelling");
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    _ = watch_tx.closed() => {
                        // Client dropped the progress stream — stop wasting CPU.
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    _ = done_rx => { /* index finished on its own; nothing to cancel */ }
                }
            });
        }
        let cancel_for_index = cancel;

        tokio::task::spawn_blocking(move || {
            let _write_lock = write_lock.blocking_lock();
            let _guard = guard;
            // Dropped when the index task ends → fires the watchdog's `done_rx`
            // so it releases its stream sender and the response can terminate.
            let _done = done_tx;
            // Identity: prefer the git origin remote when configured (the
            // returned URL is only an identity string — never fetched);
            // fall back to a file:// URL. The engine records the on-disk
            // location separately as `root_path` and prunes a prior
            // file://-identified node for the same working tree by uid.
            // Guard on `.git` at the indexed root: `git config` walks up to
            // an enclosing repo, and a subdirectory index must not capture
            // (and collide with) its parent repo's identity.
            let repo_url = nestweaver_engine::mint_repo_identity(&repo_path);

            let indexed_sha = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo_path)
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8(o.stdout)
                            .ok()
                            .map(|s| s.trim().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "local".to_string());

            let _ = tx.blocking_send(Ok(IndexProgress {
                phase: Phase::Discovering as i32,
                message: format!("Scanning {}", repo_path.display()),
                files_processed: 0,
                files_total: 0,
                symbols_found: 0,
            }));

            match nestweaver_engine::index_directory_with_store_cancellable(
                &state.store,
                &repo_path,
                &state.db_path,
                // nw-019: stamp the effective logical instance on indexed repos —
                // an explicit request `--instance` overrides the daemon default.
                &effective_instance,
                &repo_url,
                &indexed_sha,
                force,
                name.as_deref(),
                &cancel_for_index,
            ) {
                Ok(result) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Writing as i32,
                        message: format!(
                            "Indexed {} files, {} symbols",
                            result.files_count, result.symbols_count
                        ),
                        files_processed: result.files_count as u64,
                        files_total: result.files_count as u64,
                        symbols_found: result.symbols_count as u64,
                    }));

                    // PageRank is deferred to first query (lazy evaluation
                    // in GraphStore::ensure_pagerank_loaded).

                    // Tantivy indexes notes/markdown only, not code symbols.
                    // No Tantivy update needed after code repo indexing.

                    // If the request was cancelled after the index committed
                    // (client disconnect or overall timeout), skip the expensive
                    // post-index phases — git activity mines up to 500 commits
                    // and the trigram rebuild scans the whole index — since
                    // nobody is waiting on the result. The graph itself is
                    // already indexed; these sidecars rebuild on the next index.
                    if cancel_for_index.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }

                    // Git activity (churn sidecar + co-changes) — runs on
                    // the repo, writes sidecar files next to the DB.
                    if with_git_activity {
                        let _ = tx.blocking_send(Ok(IndexProgress {
                            message: "Mining git activity...".to_string(),
                            ..Default::default()
                        }));
                        let scores =
                            nestweaver_engine::git_activity::compute_git_activity(&repo_path);
                        if scores.is_empty() {
                            let _ = tx.blocking_send(Ok(IndexProgress {
                                message:
                                    "No usable git history found; git-activity sidecar not written."
                                        .to_string(),
                                ..Default::default()
                            }));
                        } else {
                            let ga_path = nestweaver_engine::sidecar_path(
                                &state.db_path,
                                ".gitactivity.json",
                            );
                            if let Err(e) = nestweaver_engine::git_activity::save_git_activity(
                                &scores, &ga_path,
                            ) {
                                tracing::warn!("save git activity sidecar failed: {e}");
                            } else {
                                let _ = tx.blocking_send(Ok(IndexProgress {
                                    message: format!(
                                        "Git activity sidecar written ({} files scored).",
                                        scores.len()
                                    ),
                                    ..Default::default()
                                }));
                            }
                        }

                        // Co-change mining (piggybacks on --with-git-activity).
                        let _ = tx.blocking_send(Ok(IndexProgress {
                            message: "Mining co-changes...".to_string(),
                            ..Default::default()
                        }));
                        match nestweaver_engine::compute_cochanges(&repo_path, 500, 3, 0.30) {
                            Ok(edges) => {
                                let cochange_path = nestweaver_engine::sidecar_path(
                                    &state.db_path,
                                    ".cochange.json",
                                );
                                if let Err(e) =
                                    nestweaver_engine::save_cochange_sidecar(&edges, &cochange_path)
                                {
                                    tracing::warn!("failed to save co-change sidecar: {e}");
                                }
                                let _ = tx.blocking_send(Ok(IndexProgress {
                                    message: format!("Found {} co-change pairs.", edges.len()),
                                    ..Default::default()
                                }));
                            }
                            Err(e) => tracing::warn!("co-change mining failed: {e}"),
                        }
                    }

                    // Trigram index.
                    if with_trigrams {
                        let _ = tx.blocking_send(Ok(IndexProgress {
                            message: "Building trigram index...".to_string(),
                            ..Default::default()
                        }));
                        match state.store.build_trigram_index() {
                            Ok(postings) => {
                                tracing::info!(postings, "trigram index built");
                                let _ = tx.blocking_send(Ok(IndexProgress {
                                    message: format!("Trigram index built ({postings} postings)."),
                                    ..Default::default()
                                }));
                            }
                            Err(e) => tracing::warn!("trigram index build failed: {e}"),
                        }
                    }

                    // DONE phase
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Done as i32,
                        message: format!(
                            "Done — {} files, {} symbols, {} edges",
                            result.files_count, result.symbols_count, result.edges_count
                        ),
                        files_processed: result.files_count as u64,
                        files_total: result.files_count as u64,
                        symbols_found: result.symbols_count as u64,
                    }));
                }
                Err(e) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Error as i32,
                        message: format!("IndexRepo failed: {e:#}"),
                        files_processed: 0,
                        files_total: 0,
                        symbols_found: 0,
                    }));
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    type IndexVaultStream = ProgressStream;

    async fn index_vault(
        &self,
        request: Request<IndexVaultRequest>,
    ) -> Result<Response<Self::IndexVaultStream>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let req = request.into_inner();
        let vault_path = PathBuf::from(&req.vault_path);
        let vault_name = req.vault_name.clone();
        let extra_patterns = req.extra_ignore_patterns.clone();
        let instance_id =
            resolve_effective_instance_id(&req.instance_id, &self.state.data_instance_id)?;
        let state = self.state.clone();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<IndexProgress, Status>>(16);

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();
        tokio::task::spawn_blocking(move || {
            let _write_lock = write_lock.blocking_lock();
            let _guard = guard;
            let _ = tx.blocking_send(Ok(IndexProgress {
                phase: Phase::Discovering as i32,
                message: format!("Scanning vault {}", vault_path.display()),
                files_processed: 0,
                files_total: 0,
                symbols_found: 0,
            }));

            let index_result = nestweaver_engine::index_markdown_directory_with_store(
                &state.store,
                &vault_path,
                &state.db_path,
                &instance_id,
                &vault_name,
                &extra_patterns,
            );

            match index_result {
                Ok(result) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Writing as i32,
                        message: format!(
                            "Indexed {} notes, {} headings, {} sections",
                            result.notes_count, result.headings_count, result.sections_count
                        ),
                        files_processed: result.notes_count as u64,
                        files_total: result.notes_count as u64,
                        symbols_found: result.headings_count as u64,
                    }));

                    // Rebuild Tantivy search index so BM25 search reflects
                    // the freshly indexed vault content.
                    if let Some(ref tantivy) = state.tantivy
                        && tantivy.has_writer()
                    {
                        match tantivy.reindex_from_store(&state.store) {
                            Ok(n) => {
                                tracing::info!(docs = n, "Tantivy reindexed after vault indexing")
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Tantivy reindex failed after vault indexing")
                            }
                        }
                    }

                    // DONE phase
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Done as i32,
                        message: format!(
                            "Done — {} notes, {} headings, {} sections, {} tags",
                            result.notes_count,
                            result.headings_count,
                            result.sections_count,
                            result.tags_count
                        ),
                        files_processed: result.notes_count as u64,
                        files_total: result.notes_count as u64,
                        symbols_found: result.headings_count as u64,
                    }));
                }
                Err(e) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Error as i32,
                        message: format!("IndexVault failed: {e:#}"),
                        files_processed: 0,
                        files_total: 0,
                        symbols_found: 0,
                    }));
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    type MaterializeProjectsStream = ProgressStream;

    async fn materialize_projects(
        &self,
        request: Request<MaterializeProjectsRequest>,
    ) -> Result<Response<Self::MaterializeProjectsStream>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let req = request.into_inner();
        let config_path = PathBuf::from(&req.config_path);
        let instance_id =
            resolve_effective_instance_id(&req.instance_id, &self.state.data_instance_id)?;
        let state = self.state.clone();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<IndexProgress, Status>>(16);

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();
        tokio::task::spawn_blocking(move || {
            let _write_lock = write_lock.blocking_lock();
            let _guard = guard;
            let _ = tx.blocking_send(Ok(IndexProgress {
                phase: Phase::Discovering as i32,
                message: format!("Loading instance config from {}", config_path.display()),
                files_processed: 0,
                files_total: 0,
                symbols_found: 0,
            }));

            let instance_config = match nestweaver_engine::InstanceConfig::from_file(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Error as i32,
                        message: format!("Failed to load instance config: {e:#}"),
                        files_processed: 0,
                        files_total: 0,
                        symbols_found: 0,
                    }));
                    return;
                }
            };

            let _ = tx.blocking_send(Ok(IndexProgress {
                phase: Phase::Writing as i32,
                message: format!("Materializing projects for instance {}", instance_id),
                files_processed: 0,
                files_total: 0,
                symbols_found: 0,
            }));

            match nestweaver_engine::materialize_projects(
                &state.store,
                &instance_config,
                &instance_id,
                &state.db_path,
            ) {
                Ok(result) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Done as i32,
                        message: format!(
                            "Done — {} projects, {} note edges, {} symbol edges, {} component edges",
                            result.projects_created,
                            result.note_edges,
                            result.symbol_edges,
                            result.component_edges,
                        ),
                        files_processed: result.projects_created as u64,
                        files_total: result.projects_created as u64,
                        symbols_found: result.symbol_edges as u64,
                    }));
                }
                Err(e) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Error as i32,
                        message: format!("MaterializeProjects failed: {e:#}"),
                        files_processed: 0,
                        files_total: 0,
                        symbols_found: 0,
                    }));
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn remove_vault(
        &self,
        request: Request<RemoveVaultRequest>,
    ) -> Result<Response<RemoveVaultResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            let search_rows_before = indexed_search_rows_before(&state);
            run_remove_vault_with_projection(&state, &req.vault_uid, search_rows_before)
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    async fn remove_repo(
        &self,
        request: Request<RemoveRepoRequest>,
    ) -> Result<Response<RemoveRepoResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            run_remove_repo_with(
                &state,
                &req.repo_uid,
                |store, uid| {
                    store.clear_repo_derived_nodes(uid).map_err(|e| {
                        Status::internal(format!("clear_repo_derived_nodes failed: {e:#}"))
                    })
                },
                |store, uid| {
                    store
                        .delete_repo_node(uid)
                        .map_err(|e| Status::internal(format!("delete_repo_node failed: {e:#}")))
                },
            )
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    async fn remove_project(
        &self,
        request: Request<RemoveProjectRequest>,
    ) -> Result<Response<RemoveProjectResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            run_remove_project_with(
                &state,
                &req.project_uid,
                |store, uid| store.delete_project_cascade_with_outcome(uid),
                |store, uid| store.project_exists(uid),
                nestweaver_engine::remove_extension_uid_durable,
                finalize_node_graph_deletion,
            )
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    async fn prune_stale(
        &self,
        request: Request<PruneStaleRequest>,
    ) -> Result<Response<PruneStaleResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let _req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            run_prune_stale_with(
                &state,
                delete_repo_cascade,
                |store, vault| {
                    store
                        .delete_vault_cascade(&vault.uid)
                        .map(|_| ())
                        .map_err(|e| anyhow::anyhow!("delete_vault_cascade failed: {e:#}"))
                },
                rebuild_tantivy_after_mutation,
            )
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    async fn merge_instance(
        &self,
        request: Request<MergeInstanceRequest>,
    ) -> Result<Response<MergeInstanceResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let req = request.into_inner();
        let from_id = resolve_effective_instance_id(&req.from_id, &self.state.data_instance_id)?;
        let to_id = resolve_effective_instance_id(&req.to_id, &self.state.data_instance_id)?;
        if from_id == to_id {
            return Err(Status::invalid_argument(
                "source and target instance IDs must differ",
            ));
        }
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            let result = run_merge_instance_with(
                &state,
                &from_id,
                &to_id,
                |store, from_id, to_id| {
                    store
                        .merge_instance_ids(from_id, to_id)
                        .map_err(|e| anyhow::anyhow!("{e:#}"))
                },
                rebuild_tantivy_after_mutation,
            )?;

            let discarded_vaults = result
                .discarded
                .into_iter()
                .map(|d| format!("{} ({} notes discarded)", d.root_path, d.notes_discarded))
                .collect::<Vec<_>>();

            Ok::<_, Status>(MergeInstanceResponse {
                vaults_reparented: result.vaults as u64,
                repos_reparented: result.repos as u64,
                projects_reparented: result.projects as u64,
                discarded_vaults,
                repos_needing_reindex: result.repos_moved,
                // Reaching here means the merge committed (nw-091 / Bug 2).
                // Reconciliation warnings, if any, are logged by
                // finish_reconciled_mutation; wire-surfacing them for merge would
                // require threading them through the internal MergeResult path.
                committed: true,
                reconciliation_failures: Vec::new(),
            })
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    type PurgeInstanceStream = ProgressStream;

    async fn purge_instance(
        &self,
        request: Request<PurgeInstanceRequest>,
    ) -> Result<Response<Self::PurgeInstanceStream>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let req = request.into_inner();
        let instance_id =
            resolve_effective_instance_id(&req.instance_id, &self.state.data_instance_id)?;
        let state = self.state.clone();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<IndexProgress, Status>>(16);

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();
        tokio::task::spawn_blocking(move || {
            let _write_lock = write_lock.blocking_lock();
            let _guard = guard;
            let _ = tx.blocking_send(Ok(IndexProgress {
                phase: Phase::Writing as i32,
                message: format!("Purging instance {instance_id}"),
                files_processed: 0,
                files_total: 0,
                symbols_found: 0,
            }));

            match run_purge_instance_with(
                &state,
                &instance_id,
                |store, id| {
                    store
                        .purge_instance(id)
                        .map_err(|e| anyhow::anyhow!("{e:#}"))
                },
                rebuild_tantivy_after_mutation,
            ) {
                Ok(result) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Done as i32,
                        message: format!(
                            "Done — {} repos, {} files, {} symbols, {} vaults, {} notes, {} projects, {} orphans swept",
                            result.repos,
                            result.files,
                            result.symbols,
                            result.vaults,
                            result.notes,
                            result.projects,
                            result.orphans_swept,
                        ),
                        files_processed: (result.repos + result.vaults) as u64,
                        files_total: (result.repos + result.vaults) as u64,
                        symbols_found: result.symbols as u64,
                    }));
                }
                Err(e) => {
                    let _ = tx.blocking_send(Ok(IndexProgress {
                        phase: Phase::Error as i32,
                        message: e.message().to_string(),
                        files_processed: 0,
                        files_total: 0,
                        symbols_found: 0,
                    }));
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    type RefreshBrainStream = ProgressStream;

    async fn refresh_brain(
        &self,
        _request: Request<RefreshBrainRequest>,
    ) -> Result<Response<Self::RefreshBrainStream>, Status> {
        Err(Status::unimplemented("RefreshBrain is not yet implemented"))
    }

    async fn reindex_search(
        &self,
        _request: Request<ReindexSearchRequest>,
    ) -> Result<Response<ReindexSearchResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            _request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        // Rebuilding the Tantivy index is a mutation: it must run under the
        // write gate so it serializes against a `backup`'s sidecar staging
        // (which copies under the same lock) and is visible to the shutdown
        // drain / idle timeout via `active_writes` — mirroring `prune_stale`
        // and `purge_instance`.
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let state = self.state.clone();
        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            let tantivy = state
                .tantivy
                .as_ref()
                .filter(|t| t.has_writer())
                .ok_or_else(|| {
                    Status::failed_precondition("daemon has no writer-mode Tantivy index")
                })?;
            let count = tantivy
                .reindex_from_store(&state.store)
                .map_err(|e| Status::internal(format!("reindex failed: {e:#}")))?;
            Ok::<_, Status>(ReindexSearchResponse {
                document_count: count as i32,
            })
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    // ── Read RPCs — typed hot-path ─────────────────────────────────

    async fn search(
        &self,
        r: Request<BrainSearchRequest>,
    ) -> Result<Response<BrainSearchResponse>, Status> {
        let visible = self.state.visible_repos_for(r.extensions())?;
        let req = r.into_inner();
        let mut args = serde_json::json!({
            "query": req.query,
            "include_bodies": req.include_bodies,
            "prf": req.prf,
            "rerank": req.rerank,
        });
        if req.limit > 0 {
            args["limit"] = serde_json::json!(req.limit);
        }
        if let Some(ref fmt) = req.response_format {
            args["response_format"] = serde_json::json!(fmt);
        }
        if let Some(ref root) = req.root {
            args["root"] = serde_json::json!(root);
        }

        // `brain_search` includes repo-owned symbols, so the typed hot path
        // must enforce the same request-derived scope as generic JSON-RPC.
        let value = self
            .dispatch_tool_json("brain_search", args, visible)
            .await?;

        // Parse JSON result into typed response.
        let query_echo = value
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let engine = value
            .get("engine")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let total_matches = value
            .get("total_matches")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;

        let results: Vec<SearchResultItem> = value
            .get("results")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|item| SearchResultItem {
                        uid: item
                            .get("uid")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        canonical_id: item
                            .get("canonical_id")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                        kind: item
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        title: item
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        score: item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        location: item
                            .get("location")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                        matched_headings: item
                            .get("matched_headings")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|s| s.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        inline_body: item
                            .get("inline_body")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                        vault_uid: item
                            .get("vault_uid")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let expansion_terms = value
            .get("expansion_terms")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let returned_matches = value
            .get("returned_matches")
            .and_then(|v| v.as_i64())
            .unwrap_or(results.len() as i64) as i32;
        let total_matches_relation = value
            .get("total_matches_relation")
            .and_then(|v| v.as_str())
            .unwrap_or("eq")
            .to_string();
        let explicit_truncated = value
            .get("truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let truncated = explicit_truncated
            || total_matches_relation != "eq"
            || returned_matches < total_matches;

        Ok(Response::new(BrainSearchResponse {
            query: query_echo,
            engine,
            total_matches,
            results,
            expansion_terms,
            returned_matches,
            total_matches_relation,
            truncated,
            // `brain_search` is keyword/BM25-only; it must not claim a
            // semantic leg was requested or degraded.
            semantic_applied: false,
            degraded_components: Vec::new(),
        }))
    }

    async fn get_context(
        &self,
        r: Request<BrainContextRequest>,
    ) -> Result<Response<BrainContextResponse>, Status> {
        let req = r.into_inner();
        let mut args = serde_json::json!({
            "seeds": req.seeds,
            "include_seeds": req.include_seeds,
            "include_bodies": req.include_bodies,
            "prf": req.prf,
            "rerank": req.rerank,
        });
        if req.token_budget > 0 {
            args["token_budget"] = serde_json::json!(req.token_budget);
        }
        if !req.response_format.is_empty() {
            args["response_format"] = serde_json::json!(req.response_format);
        }
        if !req.repos.is_empty() {
            args["repos"] = serde_json::json!(req.repos);
        }
        if !req.vaults.is_empty() {
            args["vaults"] = serde_json::json!(req.vaults);
        }
        if !req.kinds.is_empty() {
            args["kinds"] = serde_json::json!(req.kinds);
        }
        if !req.path_prefix.is_empty() {
            args["path_prefix"] = serde_json::json!(req.path_prefix);
        }
        if !req.tags.is_empty() {
            args["tags"] = serde_json::json!(req.tags);
        }
        if !req.exclude_tags.is_empty() {
            args["exclude_tags"] = serde_json::json!(req.exclude_tags);
        }
        if req.weight_ppr != 0.0 {
            args["weight_ppr"] = serde_json::json!(req.weight_ppr);
        }
        if req.weight_bm25 != 0.0 {
            args["weight_bm25"] = serde_json::json!(req.weight_bm25);
        }
        if !req.intent.is_empty() {
            args["intent"] = serde_json::json!(req.intent);
        }
        if !req.root.is_empty() {
            args["root"] = serde_json::json!(req.root);
        }
        if !req.since.is_empty() {
            args["since"] = serde_json::json!(req.since);
        }
        if req.weight_semantic > 0.0 {
            args["weight_semantic"] = serde_json::json!(req.weight_semantic);
        }
        if req.recency_weight > 0.0 {
            args["recency_weight"] = serde_json::json!(req.recency_weight);
        }
        if req.recency_half_life_days > 0.0 {
            args["recency_half_life_days"] = serde_json::json!(req.recency_half_life_days);
        }

        // Fixed non-blast_radius tool — see brain_search above.
        let value = self
            .dispatch_tool_json(
                "brain_context",
                args,
                nestweaver_engine::authz::VisibleRepos::All,
            )
            .await?;
        let result_json = serde_json::to_string(&value)
            .map_err(|e| Status::internal(format!("failed to serialize result: {e}")))?;
        let semantic_applied = value
            .get("semantic_applied")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let degraded_components = value
            .get("degraded_components")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Response::new(BrainContextResponse {
            result_json,
            semantic_applied,
            degraded_components,
        }))
    }

    async fn get_project_context(
        &self,
        r: Request<ProjectContextRequest>,
    ) -> Result<Response<ProjectContextResponse>, Status> {
        let req = r.into_inner();
        let mut args = serde_json::json!({
            "project": req.project,
            "include_components": req.include_components,
            "include_seeds": req.include_seeds,
        });
        if req.token_budget > 0 {
            args["token_budget"] = serde_json::json!(req.token_budget);
        }
        if !req.kinds.is_empty() {
            args["kinds"] = serde_json::json!(req.kinds);
        }
        if !req.intent.is_empty() {
            args["intent"] = serde_json::json!(req.intent);
        }
        if !req.since.is_empty() {
            args["since"] = serde_json::json!(req.since);
        }
        if req.recency_weight > 0.0 {
            args["recency_weight"] = serde_json::json!(req.recency_weight);
        }
        if req.recency_half_life_days > 0.0 {
            args["recency_half_life_days"] = serde_json::json!(req.recency_half_life_days);
        }
        if !req.response_format.is_empty() {
            args["response_format"] = serde_json::json!(req.response_format);
        }
        if !req.repos.is_empty() {
            args["repos"] = serde_json::json!(req.repos);
        }
        if !req.path_prefix.is_empty() {
            args["path_prefix"] = serde_json::json!(req.path_prefix);
        }
        if !req.tags.is_empty() {
            args["tags"] = serde_json::json!(req.tags);
        }
        if !req.exclude_tags.is_empty() {
            args["exclude_tags"] = serde_json::json!(req.exclude_tags);
        }

        // Fixed non-blast_radius tool — see brain_search above.
        let value = self
            .dispatch_tool_json(
                "project_context",
                args,
                nestweaver_engine::authz::VisibleRepos::All,
            )
            .await?;
        let result_json = serde_json::to_string(&value)
            .map_err(|e| Status::internal(format!("failed to serialize result: {e}")))?;

        Ok(Response::new(ProjectContextResponse { result_json }))
    }

    async fn get_note(
        &self,
        r: Request<NoteGetRequest>,
    ) -> Result<Response<NoteGetResponse>, Status> {
        let req = r.into_inner();
        let mut args = serde_json::json!({
            "include_body": req.include_body,
        });
        if let Some(ref uid) = req.uid {
            args["uid"] = serde_json::json!(uid);
        }
        if let Some(ref title) = req.title {
            args["title"] = serde_json::json!(title);
        }
        if !req.sections.is_empty() {
            args["sections"] = serde_json::json!(req.sections);
        }

        // Fixed non-blast_radius tool — see brain_search above.
        let value = self
            .dispatch_tool_json(
                "note_get",
                args,
                nestweaver_engine::authz::VisibleRepos::All,
            )
            .await?;

        Ok(Response::new(NoteGetResponse {
            uid: value
                .get("uid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            title: value
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            path: value
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            note_kind: value
                .get("note_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            word_count: value
                .get("word_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32,
            body: value
                .get("body")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            section_count: value
                .get("section_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32,
            // Parity with the local note_get JSON: frontmatter (a JSON object)
            // and the heading outline ride the typed response too — the MCP
            // daemon path used to drop both.
            frontmatter_json: value
                .get("frontmatter")
                .map(|v| v.to_string())
                .unwrap_or_default(),
            outline: value
                .get("outline")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|h| nestweaver_proto::NoteHeading {
                            uid: h
                                .get("uid")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            level: h.get("level").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                            text: h
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            slug: h
                                .get("slug")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            line: h.get("line").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }))
    }

    async fn brain_status(
        &self,
        _r: Request<BrainStatusRequest>,
    ) -> Result<Response<BrainStatusResponse>, Status> {
        let args = serde_json::json!({});
        // Fixed non-blast_radius tool — see brain_search above.
        let value = self
            .dispatch_tool_json(
                "brain_status",
                args,
                nestweaver_engine::authz::VisibleRepos::All,
            )
            .await?;

        let indexing_active = self.state.indexing_active.load(Ordering::Relaxed);
        let indexing_repo = if indexing_active {
            self.state.indexing_repo.read().await.clone()
        } else {
            String::new()
        };
        let queue_depth = self.state.indexing_queue_depth.load(Ordering::Relaxed) as i32;
        let embedding_status = embedding_status_proto(&self.state.embedding_runtime.status());

        Ok(Response::new(BrainStatusResponse {
            vault_count: value
                .get("vault_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32,
            notes: value.get("notes").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            headings: value.get("headings").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            sections: value.get("sections").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            tags: value.get("tags").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            wikilinks: value.get("wikilinks").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            repo_count: value
                .get("repo_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32,
            tantivy_available: value
                .get("tantivy_available")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            tantivy_doc_count: value
                .get("tantivy_doc_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32,
            indexing_active,
            indexing_repo,
            queue_depth,
            embedding_status: Some(embedding_status),
        }))
    }

    async fn hub_nodes(
        &self,
        r: Request<HubNodesRequest>,
    ) -> Result<Response<HubNodesResponse>, Status> {
        let req = r.into_inner();
        let mut args = serde_json::json!({});
        if req.top_n > 0 {
            args["top_n"] = serde_json::json!(req.top_n);
        }
        if !req.response_format.is_empty() {
            args["response_format"] = serde_json::json!(req.response_format);
        }

        // Fixed non-blast_radius tool — see brain_search above.
        let value = self
            .dispatch_tool_json(
                "hub_nodes",
                args,
                nestweaver_engine::authz::VisibleRepos::All,
            )
            .await?;
        let result_json = serde_json::to_string(&value)
            .map_err(|e| Status::internal(format!("failed to serialize result: {e}")))?;

        Ok(Response::new(HubNodesResponse { result_json }))
    }

    async fn brain_status_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let req = r.into_inner();
        // Fixed non-blast_radius tool — pass `VisibleRepos::All` (no scoping).
        let resp = self
            .dispatch_json_tool(
                "brain_status",
                &req.args_json,
                nestweaver_engine::authz::VisibleRepos::All,
            )
            .await?;
        // Inject server-side indexing status into the JSON response so
        // AI agents see it via the MCP tool path as well.
        let mut json_resp = resp.into_inner();
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&json_resp.result_json) {
            value["server_mode"] = serde_json::json!(self.state.server_mode);
            let indexing_active = self.state.indexing_active.load(Ordering::Relaxed);
            value["indexing_active"] = serde_json::json!(indexing_active);
            if indexing_active {
                value["indexing_repo"] = serde_json::json!(*self.state.indexing_repo.read().await);
            }
            value["queue_depth"] =
                serde_json::json!(self.state.indexing_queue_depth.load(Ordering::Relaxed));
            let embedding_status = self.state.embedding_runtime.status();
            value["embedding_status"] = embedding_status_json(&embedding_status);
            if let Ok(s) = serde_json::to_string(&value) {
                json_resp.result_json = s;
            }
        }
        Ok(Response::new(json_resp))
    }

    async fn repo_states(
        &self,
        _request: Request<RepoStatesRequest>,
    ) -> Result<Response<RepoStatesResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();

        let result = tokio::task::spawn_blocking(move || {
            let repos = state
                .store
                .list_repos(None)
                .map_err(|e| Status::internal(format!("list_repos failed: {e:#}")))?;

            let repo_states: Vec<RepoState> = repos
                .into_iter()
                .map(|r| {
                    let symbol_count = state
                        .store
                        .symbol_names_by_repo(&r.uid)
                        .map(|v| v.len() as i64)
                        .unwrap_or(0);
                    RepoState {
                        repo_uid: r.uid,
                        repo_url: r.url.clone(),
                        repo_name: r.name.clone().unwrap_or_else(|| {
                            nestweaver_schema::repo_name(&r.url).unwrap_or_else(|| r.url.clone())
                        }),
                        indexed_sha: r.indexed_sha,
                        symbol_count,
                    }
                })
                .collect();

            Ok::<_, Status>(RepoStatesResponse { repos: repo_states })
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(Response::new)
    }

    // ── Phase 4 — typed RPCs ───────────────────────────────────────

    async fn flow_trace_continue(
        &self,
        request: Request<FlowTraceContinueRequest>,
    ) -> Result<Response<FlowTraceContinueResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let req = request.into_inner();
        let state = self.state.clone();

        let result =
            tokio::task::spawn_blocking(move || flow_trace_continue_impl(&state.store, req))
                .await
                .map_err(|e| Status::internal(format!("task panicked: {e}")))?;

        result.map(Response::new)
    }

    async fn impact_analysis(
        &self,
        request: Request<ImpactAnalysisRequest>,
    ) -> Result<Response<ImpactAnalysisResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let req = request.into_inner();
        let state = self.state.clone();

        let result = tokio::task::spawn_blocking(move || impact_analysis_impl(&state.store, req))
            .await
            .map_err(|e| Status::internal(format!("task panicked: {e}")))?;

        result.map(Response::new)
    }

    // ── Read RPCs — JSON pass-through ───────────────────────────────

    async fn get_backlinks(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "backlinks")
    }

    async fn flow_trace(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "flow_trace")
    }

    async fn blast_radius(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "blast_radius")
    }

    async fn impact(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_impact")
    }

    async fn brain_guide(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_guide")
    }

    async fn brain_diff(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_diff")
    }

    async fn read_symbols(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "read_symbols")
    }

    async fn regex_search(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "regex_search")
    }

    async fn count_patterns(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "count_patterns")
    }

    async fn cross_repo_contracts(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "cross_repo_contracts")
    }

    async fn contract_drift(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "contract_drift")
    }

    async fn dead_code(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "dead_code")
    }

    async fn brain_broken_links(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_broken_links")
    }

    async fn brain_orphan_documents(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_orphan_documents")
    }

    async fn brain_topic_clusters(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_topic_clusters")
    }

    async fn brain_tag_graph(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_tag_graph")
    }

    async fn brain_doc_stats(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_doc_stats")
    }

    async fn brain_memory_lint(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_memory_lint")
    }

    async fn brain_memory_consolidate(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_memory_consolidate")
    }

    async fn brain_memory_related(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "brain_memory_related")
    }

    async fn detect_changes(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "detect_changes")
    }

    async fn affected_tests(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "affected_tests")
    }

    async fn clusters(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "clusters")
    }

    async fn stale_check(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "stale_check")
    }

    async fn bridge_nodes(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "bridge_nodes")
    }

    async fn get_summary(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "get_summary")
    }

    async fn investigate(&self, r: Request<JsonRequest>) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "investigate")
    }

    async fn investigate_expand(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "investigate_expand")
    }

    async fn investigate_hydrate(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "investigate_hydrate")
    }

    async fn set_extension(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        // set_extension is a mutating tool — require admin, matching json_rpc!.
        if let Some(crate::auth::IsAdmin(false)) | None =
            r.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied(
                "tool 'set_extension' is mutating and requires the admin token",
            ));
        }
        // Read-modify-write of the `.extensions.json` sidecar, which is part of
        // the backup sidecar set. It must run under the write gate — write_mutex
        // + ConnectionGuard::write — so it serializes against a `backup`'s
        // sidecar staging (which copies under the same lock), is visible to the
        // shutdown drain / idle timeout, and two concurrent callers cannot lose
        // updates (last-writer-wins). This is the single gated write path: the
        // MCP `tool_set_extension` routes here rather than mutating directly.
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let req = r.into_inner();
        let db_path = self.state.db_path.clone();

        // Errors are wrapped in the standard `tool <name> failed:` format (see
        // dispatch_err_to_status) so MCP clients don't mislabel tool argument
        // errors as gRPC transport failures.
        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            let args: serde_json::Value = serde_json::from_str(&req.args_json).map_err(|e| {
                Status::invalid_argument(format!(
                    "tool set_extension failed: invalid JSON in args_json: {e}"
                ))
            })?;
            let uid = args.get("uid").and_then(|v| v.as_str()).ok_or_else(|| {
                Status::invalid_argument("tool set_extension failed: 'uid' must be a string")
            })?;
            let key = args.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                Status::invalid_argument("tool set_extension failed: 'key' must be a string")
            })?;
            let value = args.get("value").cloned().ok_or_else(|| {
                Status::invalid_argument("tool set_extension failed: 'value' is required")
            })?;

            let mut store = nestweaver_engine::extensions::load_extensions(&db_path);
            nestweaver_engine::extensions::set_property(&mut store, uid, key, value.clone());
            nestweaver_engine::extensions::save_extensions(&db_path, &store).map_err(|e| {
                Status::internal(format!("tool set_extension failed: save_extensions: {e:#}"))
            })?;

            let result_json = serde_json::json!({
                "uid": uid,
                "key": key,
                "value": value,
                "status": "saved",
            })
            .to_string();
            Ok::<_, Status>(JsonResponse { result_json })
        })
        .await
        .map_err(|e| Status::internal(format!("tool set_extension failed: spawn_blocking: {e}")))?;

        result.map(Response::new)
    }

    async fn query_extensions(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        // Same `tool <name> failed:` wrapping as set_extension above.
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).map_err(|error| {
                Status::invalid_argument(format!(
                    "tool query_extensions failed: invalid args JSON: {error}"
                ))
            })?;
        let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, Status> {
            let extensions = nestweaver_engine::load_live_extensions(&state.store, &state.db_path)
                .map_err(|error| {
                    Status::internal(format!(
                        "tool query_extensions failed: extension liveness: {error:#}"
                    ))
                })?;
            if let Some(uid) = args.get("uid").and_then(|value| value.as_str()) {
                return Ok(serde_json::json!({
                    "uid": uid,
                    "properties": nestweaver_engine::get_all_properties(&extensions, uid),
                }));
            }
            let key = args
                .get("key")
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    Status::invalid_argument(
                        "tool query_extensions failed: provide either 'uid' or both 'key' and 'value'",
                    )
                })?;
            let value = args.get("value").cloned().ok_or_else(|| {
                Status::invalid_argument(
                    "tool query_extensions failed: 'value' is required when 'key' is given",
                )
            })?;
            let results: Vec<_> = nestweaver_engine::query_by_property(&extensions, key, &value)
                .into_iter()
                .map(|uid| {
                    serde_json::json!({
                        "uid": uid,
                        "properties": extensions.get(uid).cloned().unwrap_or_default(),
                    })
                })
                .collect();
            Ok(serde_json::json!({
                "key": key,
                "value": value,
                "count": results.len(),
                "results": results,
            }))
        })
        .await
        .map_err(|error| {
            Status::internal(format!(
                "tool query_extensions failed: spawn_blocking panicked: {error}"
            ))
        })??;
        serde_json::to_string(&result)
            .map(|result_json| Response::new(JsonResponse { result_json }))
            .map_err(|error| {
                Status::internal(format!(
                    "tool query_extensions failed: serialization: {error}"
                ))
            })
    }

    // ── Read RPCs — direct store access (no MCP tool) ──────────────

    #[allow(clippy::result_large_err)]
    async fn list_repos_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let instance = args.get("instance").and_then(|v| v.as_str());
            let repos = state
                .store
                .list_repos(instance)
                .map_err(|e| Status::internal(format!("list_repos failed: {e:#}")))?;
            serde_json::to_string(&repos)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn list_vaults_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let instance = args.get("instance").and_then(|v| v.as_str());
            let vaults = state
                .store
                .list_vaults(instance)
                .map_err(|e| Status::internal(format!("list_vaults failed: {e:#}")))?;
            serde_json::to_string(&vaults)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn embedding_dimension(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let _args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let dim = state
                .store
                .embedding_dimension()
                .map_err(|e| Status::internal(format!("embedding_dimension failed: {e:#}")))?;
            serde_json::to_string(&dim)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn list_services_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let instance = args.get("instance").and_then(|v| v.as_str());
            let services = state
                .store
                .list_services(instance)
                .map_err(|e| Status::internal(format!("list_services failed: {e:#}")))?;
            serde_json::to_string(&services)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn service_summary_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let instance = args.get("instance").and_then(|v| v.as_str());
            let services = state
                .store
                .list_services(instance)
                .map_err(|e| Status::internal(format!("list_services failed: {e:#}")))?;
            let service = services.iter().find(|s| s.name == name || s.uid == name);
            match service {
                Some(s) => serde_json::to_string(s)
                    .map_err(|e| Status::internal(format!("serialization failed: {e:#}"))),
                None => Err(Status::not_found(format!("service not found: {name}"))),
            }
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn list_projects_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let _args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let projects = state
                .store
                .list_projects()
                .map_err(|e| Status::internal(format!("list_projects failed: {e:#}")))?;
            serde_json::to_string(&projects)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn search_symbols(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let candidates = nestweaver_engine::search_symbols(&state.store, query, limit)
                .map_err(|e| Status::internal(format!("search_symbols failed: {e:#}")))?;
            serde_json::to_string(&candidates)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn symbol_lookup(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let name_or_uid = args
                .get("name_or_uid")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let instance = args.get("instance").and_then(|v| v.as_str());
            // An unknown --instance surfaces here with a message listing valid instances.
            let lookup = nestweaver_engine::lookup_symbol(&state.store, name_or_uid, instance)
                .map_err(|e| Status::internal(format!("lookup_symbol failed: {e:#}")))?;
            // Serialize the LookupResult as a tagged JSON value.
            let value = match lookup {
                nestweaver_engine::LookupResult::Found(detail) => {
                    serde_json::json!({ "status": "found", "detail": *detail })
                }
                nestweaver_engine::LookupResult::NotFound => {
                    serde_json::json!({ "status": "not_found" })
                }
                nestweaver_engine::LookupResult::Ambiguous(candidates) => {
                    serde_json::json!({ "status": "ambiguous", "candidates": candidates })
                }
            };
            serde_json::to_string(&value)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    // ── Engine-level RPCs ──────────────────────────────────────────────

    #[allow(clippy::result_large_err)]
    async fn repo_map_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let token_budget = args
                .get("token_budget")
                .and_then(|v| v.as_u64())
                .unwrap_or(4096) as usize;
            let map = nestweaver_engine::generate_repo_map(&state.store, token_budget)
                .map_err(|e| Status::internal(format!("generate_repo_map failed: {e:#}")))?;
            let token_count = map.len().div_ceil(4);
            serde_json::to_string(&serde_json::json!({
                "map": map,
                "token_count": token_count,
            }))
            .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn suggest_links_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let _args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let manifests =
                nestweaver_engine::load_manifest_cache_for_db(&state.db_path).unwrap_or_default();
            let suggestions = nestweaver_engine::suggest_links(&state.store, &manifests)
                .map_err(|e| Status::internal(format!("suggest_links failed: {e:#}")))?;
            serde_json::to_string(&suggestions)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn detect_implicit_projects_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        let result = tokio::task::spawn_blocking(move || {
            let vault_path = args
                .get("vault")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Status::invalid_argument("missing 'vault' argument"))?;
            let vault = std::path::PathBuf::from(vault_path);
            let canonical = std::fs::canonicalize(&vault).unwrap_or_else(|_| vault.clone());
            let instance_id = "default";
            let vault_uid = nestweaver_schema::vault_uid(instance_id, &canonical.to_string_lossy());
            let detected = nestweaver_engine::detect_implicit_projects(
                &state.store,
                &vault,
                &vault_uid,
                instance_id,
            )
            .map_err(|e| Status::internal(format!("detect_implicit_projects failed: {e:#}")))?;
            serde_json::to_string(&detected)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    #[allow(clippy::result_large_err)]
    async fn pr_impact_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        // R9/R9b: resolve the caller's per-repo visibility from the request's
        // Identity extension BEFORE the request is consumed. A disabled policy
        // (no `[authz]`) short-circuits to `All` with no repo listing (zero
        // behavior change). nw-043: a store error while listing fails the RPC
        // loudly (Unavailable, after one retry inside `visible_repos_for`)
        // instead of silently redacting everything.
        let visible = self.state.visible_repos_for(r.extensions())?;
        let args: serde_json::Value = serde_json::from_str(&r.into_inner().args_json)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;

        // Route pr_impact through the same safeguard framework as dispatched MCP
        // tools. pr_impact analysis is `blast_radius` under the hood, so it uses
        // that tool's safeguard profile for timeout and depth clamping. (Per-client
        // rate limiting is applied upstream at the auth interceptor for every RPC,
        // so it already covers this handler.)
        let tool = "blast_radius";
        let safeguards = &self.state.safeguards;
        let server_mode = state.server_mode;
        // Cooperative cancellation flag: tripped by `with_safeguard_cancellable`
        // on timeout and observed by `analyze_blast_radius`'s BFS, which then
        // stops and yields status=Degraded.
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Clamp traversal depth before building options, mirroring
        // `dispatch_json_tool_inner`: hard-cap in all modes (guards the BFS
        // regardless of server_mode), then tighten via the safeguard config in
        // server mode. The pr_impact response is a raw serialized
        // `BlastRadiusResult` with no `_meta`, so the clamp is applied silently
        // (no `_clamped`/`_original_depth` annotation).
        let requested_depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(3);
        let depth = requested_depth.min(HARD_MAX_DEPTH) as u32;
        let depth = if server_mode {
            state.safeguards.effective_depth(tool, Some(depth)).depth
        } else {
            depth
        };

        let changed_files: Vec<std::path::PathBuf> = args
            .get("files")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(std::path::PathBuf::from))
                    .collect()
            })
            .unwrap_or_default();
        if changed_files.is_empty() {
            return Err(Status::invalid_argument(
                "missing or empty 'files' array argument",
            ));
        }

        let cancel_for_task = cancel.clone();
        let handler = async move {
            tokio::task::spawn_blocking(move || {
                // TODO(nw-033): resolve target repo_uid from the working repo
                let options = nestweaver_engine::BlastRadiusOptions {
                    target_repo: None,
                    max_depth: depth,
                    include_data_edges: false,
                    limit: None,
                };
                let mut result = nestweaver_engine::analyze_blast_radius(
                    &state.store,
                    &changed_files,
                    &options,
                    Some(&cancel_for_task),
                    Some(&state.db_path),
                )
                .map_err(|e| {
                    // Log the detailed chain server-side; return a generic message
                    // so the client never sees internal error internals.
                    tracing::error!("analyze_blast_radius failed: {e:#}");
                    Status::internal("blast radius analysis failed")
                })?;
                // R9/R9b: redact the result to the caller's visible repos before
                // serialization. `VisibleRepos::All` (the no-`[authz]` default)
                // skips the listing entirely — redaction is a no-op — preserving
                // single-trust-domain. nw-043: a store error at this re-list means
                // the earlier listing succeeded and this one failed — exactly the
                // transient signature — so fail the RPC rather than serve a
                // mis-redacted result.
                if matches!(visible, nestweaver_engine::authz::VisibleRepos::Only(_)) {
                    let repos = state.store.list_repos(None).map_err(|e| {
                        // Log the detailed chain server-side; return a generic
                        // message so the client never sees store internals.
                        tracing::error!(
                            "authz: repo listing failed at pr_impact redaction point: {e:#}"
                        );
                        Status::unavailable("authz repo listing unavailable")
                    })?;
                    nestweaver_engine::authz::redact_blast_radius_for_visibility(
                        &mut result,
                        &visible,
                        &repos,
                    );
                }
                serde_json::to_string(&result)
                    .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
            })
            .await
            .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?
        };

        // Wrap in the per-tool timeout only in server mode, matching
        // `dispatch_json_tool`. On timeout the cancel flag is set, which stops
        // the BFS above.
        let result_json = if server_mode {
            with_safeguard_cancellable(tool, safeguards, None, cancel, handler).await?
        } else {
            handler.await?
        };

        Ok(Response::new(JsonResponse { result_json }))
    }

    // ── Embedding ───────────────────────────────────────────────────

    #[allow(clippy::result_large_err)]
    async fn plan_embed(
        &self,
        request: Request<EmbedRequest>,
    ) -> Result<Response<EmbedResponse>, Status> {
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::read(&self.state);
        let request = request.into_inner();
        let store = self.state.store.clone();
        let response = tokio::task::spawn_blocking(move || {
            plan_embeddings(&store, &request.scope, request.force)
        })
        .await
        .map_err(|error| Status::internal(format!("embed plan task panicked: {error}")))??;
        Ok(Response::new(response))
    }

    #[allow(clippy::result_large_err)]
    async fn embed(
        &self,
        request: Request<EmbedRequest>,
    ) -> Result<Response<EmbedResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        #[cfg(not(feature = "embed"))]
        {
            let _ = request;
            return Err(Status::failed_precondition(
                "embedding is not available — the daemon was built without the `embed` feature",
            ));
        }

        #[cfg(feature = "embed")]
        {
            let req = request.into_inner();
            let scope = req.scope.clone();
            let force = req.force;
            let batch_size = if req.batch_size == 0 {
                32
            } else {
                req.batch_size as usize
            };

            let scopes = embedding_scopes(&scope)?;
            let do_symbols = scopes.symbols;
            let do_notes = scopes.notes;
            let do_headings = scopes.headings;

            let (status, model) = self.state.embedding_runtime.snapshot();
            let Some(model) = model else {
                return Err(Status::failed_precondition(if status.error.is_empty() {
                    format!("embedding is not ready (state: {})", status.state)
                } else {
                    format!(
                        "embedding is not ready (state: {}): {}",
                        status.state, status.error
                    )
                }));
            };

            let store = self.state.store.clone();

            let result = tokio::task::spawn_blocking(move || {
                let mut succeeded = 0u32;
                let mut failed = 0u32;
                let mut rejected = 0u32;
                let mut scoped = 0u64;
                let mut eligible = 0u64;

                // Each embed run may legitimately force-switch the model once;
                // re-arm the once-per-run clear guard on this long-lived index.
                store.reset_embedding_force_guard();

                if do_symbols {
                    let symbols = store
                        .list_all_symbols()
                        .map_err(|error| embedding_store_status("symbols", error.into()))?;
                    scoped += symbols.len() as u64;
                    let to_embed: Vec<_> = symbols
                        .iter()
                        .filter(|symbol| embedding_is_eligible(&store, &symbol.uid, force))
                        .collect();
                    eligible += to_embed.len() as u64;
                    for chunk in to_embed.chunks(batch_size) {
                        for sym in chunk {
                            let text = nestweaver_embed::preprocess::symbol_embed_text(
                                &sym.kind.to_string(),
                                &sym.name,
                                None,
                            );
                            match model.embed_query(&text) {
                                Ok(emb) => {
                                    if store.add_embedding_with_force(&sym.uid, emb, force) {
                                        succeeded += 1;
                                    } else {
                                        rejected += 1;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(uid = %sym.uid, "embedding failed: {e}");
                                    failed += 1;
                                }
                            }
                        }
                    }
                }

                if do_notes {
                    let notes = store
                        .list_notes(None)
                        .map_err(|error| embedding_store_status("notes", error.into()))?;
                    scoped += notes.len() as u64;
                    let to_embed: Vec<_> = notes
                        .iter()
                        .filter(|note| embedding_is_eligible(&store, &note.uid, force))
                        .collect();
                    eligible += to_embed.len() as u64;
                    for chunk in to_embed.chunks(batch_size) {
                        for note in chunk {
                            let text =
                                nestweaver_embed::preprocess::note_embed_text(&note.title, None);
                            match model.embed_query(&text) {
                                Ok(emb) => {
                                    if store.add_embedding_with_force(&note.uid, emb, force) {
                                        succeeded += 1;
                                    } else {
                                        rejected += 1;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(uid = %note.uid, "embedding failed: {e}");
                                    failed += 1;
                                }
                            }
                        }
                    }
                }

                if do_headings {
                    let headings = store
                        .list_all_headings()
                        .map_err(|error| embedding_store_status("headings", error.into()))?;
                    scoped += headings.len() as u64;
                    let to_embed: Vec<_> = headings
                        .iter()
                        .filter(|heading| embedding_is_eligible(&store, &heading.uid, force))
                        .collect();
                    eligible += to_embed.len() as u64;
                    for chunk in to_embed.chunks(batch_size) {
                        for heading in chunk {
                            let text =
                                nestweaver_embed::preprocess::heading_embed_text("", &heading.text);
                            match model.embed_query(&text) {
                                Ok(emb) => {
                                    if store.add_embedding_with_force(&heading.uid, emb, force) {
                                        succeeded += 1;
                                    } else {
                                        rejected += 1;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(uid = %heading.uid, "embedding failed: {e}");
                                    failed += 1;
                                }
                            }
                        }
                    }
                }

                if succeeded > 0
                    && let Err(e) = store.flush_embedding_index()
                {
                    tracing::warn!("failed to flush embedding index: {e}");
                }

                tracing::info!(succeeded, failed, rejected, "embed RPC completed");
                Ok::<_, Status>(EmbedResponse {
                    succeeded,
                    failed,
                    rejected,
                    scoped,
                    eligible,
                    skipped: scoped.saturating_sub(eligible),
                })
            })
            .await
            .map_err(|e| Status::internal(format!("embed task panicked: {e}")))?;

            result.map(Response::new)
        }
    }
}

// ── Server entry point ──────────────────────────────────────────────

/// Run the daemon gRPC server, binding to a Unix domain socket.
///
/// `idle_timeout` controls how long the daemon stays alive with no
/// active requests before self-terminating. Pass `None` to disable.
/// Options for TCP server mode (passed when `--server` is set).
#[derive(Debug, Clone)]
pub struct ServerOpts {
    /// TCP bind address, e.g. `"127.0.0.1:9378"` or `"127.0.0.1:0"` for OS-assigned.
    pub bind_addr: String,
    /// When set, the actual bound port is written here (useful for tests with port 0).
    pub port_file: Option<PathBuf>,
    /// Optional bearer token for TCP authentication. When set, TCP clients
    /// must send `Authorization: Bearer <token>` or receive UNAUTHENTICATED.
    pub auth_token: Option<String>,
    /// Path to a PEM-encoded TLS certificate. When both `tls_cert` and
    /// `tls_key` are set, the TCP listener uses TLS via rustls and plain
    /// TCP connections are refused.
    pub tls_cert: Option<PathBuf>,
    /// Path to a PEM-encoded TLS private key.
    pub tls_key: Option<PathBuf>,
    /// Webhook HMAC secret for verifying push event signatures.
    pub webhook_secret: Option<String>,
    /// Previous webhook secret, checked as fallback during secret rotation.
    pub webhook_secret_old: Option<String>,
    /// Admin token for admin API endpoints (separate from query auth token).
    pub admin_token: Option<String>,
    /// When set, boot as a read-only snapshot replica: materialize this snapshot
    /// directory into a private working copy, open it read-only, reject write
    /// RPCs, and never start the write machinery (flock/worker/scheduler/webhook).
    pub snapshot: Option<PathBuf>,
    /// ACME (Let's Encrypt) domain. When set, TLS is auto-provisioned at runtime
    /// via TLS-ALPN-01 (opt-in; default off). Counts as TLS for the non-loopback
    /// bind gate. Requires the `acme` build feature to actually provision.
    pub acme_domain: Option<String>,
    /// Contact email for the ACME account (optional, recommended for expiry
    /// notifications).
    pub acme_email: Option<String>,
    /// Use the Let's Encrypt STAGING directory (untrusted certs, high rate
    /// limits). Defaults to true — production issuance is an explicit opt-in to
    /// avoid rate-limit bans during setup / on a launchd respawn loop.
    pub acme_staging: bool,
}

/// Minimum byte length for auth/admin tokens supplied via [`ServerOpts`].
/// Short tokens are trivially brute-forceable, so startup rejects them.
const MIN_TOKEN_LEN: usize = 32;

/// Minimum byte length for webhook HMAC secrets supplied via [`ServerOpts`].
/// Webhook secrets key an HMAC over the request body rather than acting as a
/// bearer token, so the bar is lower than [`MIN_TOKEN_LEN`], but trivially
/// short secrets still weaken the signature, so startup rejects them.
const MIN_WEBHOOK_SECRET_LEN: usize = 16;

/// Reject any present auth/admin token shorter than [`MIN_TOKEN_LEN`] bytes.
/// `None` tokens (auth disabled) are accepted. The error names which token is
/// too short and the required minimum.
fn validate_token_lengths(
    auth_token: &Option<String>,
    admin_token: &Option<String>,
) -> anyhow::Result<()> {
    for (name, token) in [("auth", auth_token), ("admin", admin_token)] {
        if let Some(t) = token
            && t.len() < MIN_TOKEN_LEN
        {
            anyhow::bail!(
                "{name} token is too short ({} bytes); minimum is {MIN_TOKEN_LEN} bytes",
                t.len()
            );
        }
    }
    // An admin token WITHOUT a query (auth) token is a footgun that silently
    // disables authentication: the auth layer (McpHttpState::with_auth, the gRPC
    // interceptor's expected_token) is only installed when the query token is set,
    // and the mutating-tool gate keys off `auth_token.is_some()` — so admin-only
    // leaves BOTH reads and every mutating tool completely uncredentialed. (A
    // non-loopback bind is already refused without a query token; this catches the
    // loopback case where the operator believes the server is locked down.)
    if admin_token.is_some() && auth_token.is_none() {
        anyhow::bail!(
            "--admin-token requires --auth-token: an admin token alone leaves the \
             server unauthenticated, because the auth layer is only enabled when a \
             query token is set. Set --auth-token as well (a different value)."
        );
    }
    // The admin token must differ from the query (auth) token. Admin privilege is
    // granted on an admin-token match, so an identical query token would silently
    // make every query-token holder an admin.
    if let (Some(auth), Some(admin)) = (auth_token, admin_token)
        && auth == admin
    {
        anyhow::bail!(
            "admin token must differ from the query (auth) token; identical tokens \
             grant admin access to every query-token holder"
        );
    }
    Ok(())
}

/// Refuse to index a path that is almost certainly a mistake — a system root or
/// the user's home directory — which would recursively walk the entire disk
/// (pegging every core, hammering TCC-protected paths, and looping on symlinks
/// like `/dev/fd` and Time Machine). A repository to index is always a specific
/// project directory, never a top-level root. This guards the case where a
/// detached daemon (whose CWD is `/`) receives a relative or `.` repo path.
fn is_unsafe_index_root(path: &std::path::Path) -> bool {
    if path.as_os_str().is_empty() {
        return true;
    }
    // Case-insensitive, exact-match denylist. macOS APFS is case-insensitive by
    // default and `canonicalize` does NOT normalize case, so wrong-case roots
    // (`/users`, `/SYSTEM`) would slip past a case-sensitive match.
    // `/System/Volumes/Data` is the real data-volume firmlink root on modern
    // macOS — indexing it (or `/System/Volumes`) walks the entire disk.
    let p = path.to_string_lossy();
    let p = p.trim_end_matches('/').to_ascii_lowercase();
    if p.is_empty() {
        return true; // "" and "/"
    }
    let dangerous = [
        "/users",
        "/system",
        "/system/volumes",
        "/system/volumes/data",
        "/library",
        "/private",
        "/var",
        "/etc",
        "/home",
        "/opt",
        "/usr",
        "/bin",
        "/sbin",
        "/tmp",
        "/volumes",
        "/dev",
    ];
    if dangerous.iter().any(|d| p == *d) {
        return true;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if p == home.trim_end_matches('/').to_ascii_lowercase() {
            return true;
        }
    }
    false
}

/// Canonicalize a config repo entry URL into an allow-list path.
///
/// Config `[[repos]]` entries store `file://`-prefixed identity URLs;
/// `PathBuf::from("file:///x")` is a *relative* path that never canonicalizes,
/// which silently emptied the watcher allow-list.
fn config_repo_canonical_path(url: &str) -> Option<PathBuf> {
    let stripped = url.strip_prefix("file://").unwrap_or(url);
    std::fs::canonicalize(stripped).ok()
}

/// Validate a watcher target path against the instance config's registered
/// sources (allow-list), or — when the daemon runs without `--config` —
/// against the system-root denylist so an explicit `watch --repo X --db Y`
/// works without an instance config. `vault_only` restricts the
/// allow-list to `type = "vault"` entries (used by `watch_vault`).
fn watch_path_allowed(
    repos: Option<&[nestweaver_engine::config::RepoConfig]>,
    path: &std::path::Path,
    kind: &str,
    vault_only: bool,
) -> Result<(), Status> {
    let Some(repos) = repos else {
        if is_unsafe_index_root(path) {
            return Err(Status::invalid_argument(format!(
                "refusing to watch {kind} path {}: system roots and home directories \
                 are not watchable",
                path.display()
            )));
        }
        return Ok(());
    };
    let allowed: Vec<PathBuf> = repos
        .iter()
        .filter(|r| !vault_only || r.repo_type == Some(nestweaver_engine::config::RepoType::Vault))
        .filter_map(|r| config_repo_canonical_path(&r.url))
        .collect();
    if !allowed.iter().any(|a| path.starts_with(a)) {
        return Err(Status::invalid_argument(format!(
            "{kind} path {} is not in the instance's registered sources",
            path.display()
        )));
    }
    Ok(())
}

/// The daemon is idle only when there is no active read/write AND no index job
/// in flight. Index jobs bump `indexing_active` (not `active_writes`), so an
/// idle-timeout check that ignores it could fire mid-index — the same footgun
/// the shutdown drain already guards against.
fn is_idle(active_readwrite: u32, indexing_active: bool) -> bool {
    active_readwrite == 0 && !indexing_active
}

/// gRPC methods that only READ graph/store state. On a read-only snapshot
/// replica every method NOT in this set is rejected at the single
/// [`ReadOnlyGuard`] chokepoint (default-deny) — mirroring how PostgreSQL hot
/// standby, Datasette `--immutable`, and LiteFS reject the entire write class
/// at one node-level gate instead of a per-operation allowlist that silently
/// misses a newly-added mutating path (the exact bug this replaces).
///
/// Keep this in sync with the proto service definition: the
/// `read_only_method_partition_is_exhaustive` test fails if a new RPC is added
/// without classifying it here or as mutating, forcing a deliberate
/// (fail-closed) read/write decision.
const READ_ONLY_ALLOWED_METHODS: &[&str] = &[
    "AffectedTests",
    "BlastRadius",
    "BrainBrokenLinks",
    "BrainDiff",
    "BrainDocStats",
    "BrainGuide",
    "BrainMemoryLint",
    "BrainMemoryRelated",
    "BrainOrphanDocuments",
    "BrainStatus",
    "BrainStatusJson",
    "BrainTagGraph",
    "BrainTopicClusters",
    "BridgeNodes",
    "Clusters",
    "ContractDrift",
    "CountPatterns",
    "CrossRepoContracts",
    "DeadCode",
    "DetectChanges",
    "DetectImplicitProjectsJson",
    "EmbeddingDimension",
    "ExportGraph",
    "FlowTrace",
    "FlowTraceContinue",
    "GetBacklinks",
    "GetContext",
    "GetNote",
    "GetProjectContext",
    "GetSummary",
    "HealthCheck",
    "HubNodes",
    "Impact",
    "ImpactAnalysis",
    "Investigate",
    "InvestigateExpand",
    "InvestigateHydrate",
    "ListProjectsJson",
    "ListReposJson",
    "ListServicesJson",
    "ListVaultsJson",
    "PlanEmbed",
    "PrImpactJson",
    "QueryExtensions",
    "ReadSymbols",
    "RegexSearch",
    "RepoMapJson",
    "RepoStates",
    "Search",
    "SearchSymbols",
    "ServeUi",
    "ServiceSummaryJson",
    "Shutdown",
    "StaleCheck",
    "StopWatch",
    "SuggestLinksJson",
    "SymbolLookup",
];

/// Decide whether a gRPC request must be rejected because this daemon serves a
/// read-only snapshot replica. `path` is the full gRPC path
/// (`/package.Service/Method`). Default-deny: any method not known to be a pure
/// read is rejected with `FAILED_PRECONDITION` (a permanent condition for this
/// daemon, not a transient error the client should retry).
fn read_only_rejection(read_only: bool, path: &str) -> Option<Status> {
    if !read_only {
        return None;
    }
    let method = path.rsplit('/').next().unwrap_or(path);
    if READ_ONLY_ALLOWED_METHODS.contains(&method) {
        None
    } else {
        Some(Status::failed_precondition(format!(
            "this daemon serves a read-only snapshot replica; {method} is not available"
        )))
    }
}

/// Single read-only enforcement chokepoint for the whole gRPC surface. Wraps
/// the generated `NestWeaverDaemonServer` and, on a read-only replica, rejects
/// every mutating RPC (typed hot-path handlers AND `json_rpc!`-dispatched ones
/// alike) at the transport layer BEFORE it reaches a handler — so no mutating
/// handler can do partial work and surface a mid-stream `internal` error, and
/// the "replica rejects writes with FAILED_PRECONDITION" contract holds for
/// ALL mutating methods. This guard is the ONLY read-only enforcement point:
/// the per-handler `reject_if_read_only` calls it superseded have been
/// removed. On a read-write daemon it is a transparent
/// pass-through. Applied once to the shared service so BOTH the TCP and UDS
/// transports inherit the same gate.
#[derive(Clone)]
struct ReadOnlyGuard<S> {
    inner: S,
    read_only: bool,
}

impl<S> ReadOnlyGuard<S> {
    fn new(read_only: bool, inner: S) -> Self {
        Self { inner, read_only }
    }
}

impl<S, ReqBody> tower::Service<http::Request<ReqBody>> for ReadOnlyGuard<S>
where
    S: tower::Service<http::Request<ReqBody>, Response = http::Response<tonic::body::Body>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        if let Some(status) = read_only_rejection(self.read_only, req.uri().path()) {
            // Trailers-only gRPC error response carrying grpc-status =
            // FAILED_PRECONDITION; identical shape to what tonic emits for an
            // interceptor-level rejection.
            let resp = status.into_http::<tonic::body::Body>();
            return Box::pin(std::future::ready(Ok(resp)));
        }
        Box::pin(self.inner.call(req))
    }
}

impl<S: tonic::server::NamedService> tonic::server::NamedService for ReadOnlyGuard<S> {
    const NAME: &'static str = S::NAME;
}

/// Whether to mount the `/webhook` endpoint. A read-only replica must NEVER
/// accept a webhook: its worker pool and poll scheduler are gated out, so an
/// accepted push would enqueue an index job into a queue that no worker drains
/// — a silent blackhole with unbounded growth. Only a read-write daemon with a
/// configured secret mounts it.
fn replica_mounts_webhook(read_only: bool, webhook_secret_configured: bool) -> bool {
    !read_only && webhook_secret_configured
}

/// Whether to enqueue config-declared repos for initial indexing at startup.
/// A read-only replica serves a pre-built snapshot and has no worker to index,
/// so it must not enqueue anything (the same blackhole as the webhook path).
fn replica_enqueues_config_repos(read_only: bool) -> bool {
    !read_only
}

/// Whether to mount the admin write API (`/admin/api/*` + device-flow auth).
/// A read-only replica exposes no mutating admin surface (add/remove repo,
/// reindex, reload, drain/resume, dead-letter retry); it must not mount these.
/// `/metrics` is mounted separately and stays available for replica
/// monitoring.
fn replica_mounts_admin_api(read_only: bool, admin_token_configured: bool) -> bool {
    !read_only && admin_token_configured
}

/// Enforce the bind-scope security invariants for a server-mode listener:
/// a non-loopback bind must be both authenticated (`--auth-token`) and
/// encrypted (`--tls-cert` + `--tls-key`). Loopback binds, and bind strings
/// that cannot be parsed as a socket address (e.g. `localhost:9378`), retain
/// the pre-existing auth-only requirement. Returns the first violated invariant.
fn validate_bind_security(
    bind_addr: &str,
    auth_token: &Option<String>,
    tls_cert: &Option<PathBuf>,
    tls_key: &Option<PathBuf>,
    acme_enabled: bool,
) -> anyhow::Result<()> {
    // ACME (Let's Encrypt) provisions a publicly-trusted cert at runtime, so an
    // ACME-enabled bind is encrypted even without a static --tls-cert/--tls-key.
    let tls_enabled = (tls_cert.is_some() && tls_key.is_some()) || acme_enabled;
    match bind_addr.parse::<std::net::SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => { /* safe — loopback is process-local */ }
        Ok(addr) => {
            // Non-loopback bind: the listener is reachable from the network, so it
            // must be both authenticated and encrypted.
            if auth_token.is_none() {
                anyhow::bail!(
                    "Cannot bind to non-loopback address {addr} without --auth-token; \
                     the server would be fully open to the network"
                );
            }
            if !tls_enabled {
                anyhow::bail!(
                    "Cannot bind to non-loopback address {addr} without TLS; bearer \
                     tokens and source data would be sent in cleartext. Provide \
                     --tls-cert and --tls-key (or bind to a loopback address)."
                );
            }
        }
        Err(_) => {
            // Bind string isn't a socket address (e.g. `localhost:9378`); we cannot
            // confirm it is loopback. Preserve the auth-only requirement.
            if auth_token.is_none() {
                anyhow::bail!(
                    "Cannot determine if bind address '{bind_addr}' is loopback. \
                     Use --auth-token or specify an IP address."
                );
            }
        }
    }
    Ok(())
}

/// Whether an `--acme-domain` request can ACTUALLY provide TLS in THIS build.
/// ACME is feature-gated; a binary compiled without `acme` cannot provision a
/// certificate, so an ACME request must NOT count as TLS for the bind-security
/// gate — otherwise a non-loopback bind would pass the gate and then serve
/// cleartext. Returns `true` only when a domain was requested AND the `acme`
/// feature is compiled in.
fn acme_provides_tls(acme_domain_present: bool) -> bool {
    acme_domain_present && cfg!(feature = "acme")
}

/// Whether `bind_addr` resolves to a loopback socket address. A string we
/// cannot parse as a `SocketAddr` (e.g. `localhost:9378`) is treated as
/// NON-loopback — the safe default, so an ambiguous bind is never assumed to be
/// process-local when deciding whether cleartext is acceptable.
///
/// Only wired into the live path under the `acme` feature; always exercised by
/// tests, so `dead_code` is allowed only when ACME is compiled out.
#[cfg_attr(not(feature = "acme"), allow(dead_code))]
fn bind_addr_is_loopback(bind_addr: &str) -> bool {
    matches!(bind_addr.parse::<std::net::SocketAddr>(), Ok(addr) if addr.ip().is_loopback())
}

/// Private per-replica working directory for a materialized snapshot.
///
/// Keyed on `instance_id` (the SHA-256-derived id of the canonical `--db`
/// path — see [`lifecycle::instance_id_from_db_path`]) rather than only on the
/// parent directory. Two co-located replicas started with distinct `--db`
/// paths under a shared parent (`/data/a.lbug` + `/data/b.lbug`) get distinct
/// instance ids and therefore distinct working dirs, so one replica's
/// `materialize_snapshot` (an in-place `fs::copy` that truncates the target)
/// can never clobber a sibling's open, running working copy. This mirrors how
/// every read-replica system (LiteFS per-node dir, a PostgreSQL standby's own
/// data dir, Litestream) gives each replica private local state; the only
/// shared artifact is the immutable snapshot.
///
/// Two replicas sharing the *same* `--db` on one host collapse to the same
/// `instance_id` and thus the same `replica-work-<id>`; that duplicate is
/// rejected by [`claim_instance_lock`], which is acquired **before**
/// materialization — so the second boot never truncates a live sibling's copy.
/// (Horizontal scale is still separate hosts/containers.)
fn replica_working_dir(db_path: &Path, instance_id: &str) -> PathBuf {
    let name = format!("replica-work-{instance_id}");
    db_path
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Claim the exclusive per-instance lock — the pidfile flock that makes an
/// `instance_id` single-owner on this host — and return the pidfile handle
/// whose lifetime holds the lock (dropping it releases the lock). On non-macOS,
/// `None` identifies the daemonized child that proves it inherited the
/// launcher's pidfile lock.
///
/// Acquired **before** snapshot materialization so a duplicate replica started
/// with the identical `--db` (hence identical `instance_id` and
/// `replica-work-<id>`) is rejected here, before its `materialize_snapshot`
/// `fs::copy` can truncate a live sibling's open working copy. Two opens of the
/// same pidfile hold independent open-file descriptions, so `flock(LOCK_EX)`
/// from a second process fails even though the first holder is also us-shaped.
fn claim_instance_lock(instance_id: &str) -> Result<Option<std::fs::File>, anyhow::Error> {
    #[cfg(not(target_os = "macos"))]
    {
        // daemonize2 acquired this flock before forking and the child inherited
        // its open file description. Opening the pidfile again would conflict
        // with that inherited lock. The launcher marks only the real daemonize
        // child; matching the file's PID prevents an inherited/user-set value
        // from disabling exclusion for a normal foreground server.
        let inherited_daemonize_lock =
            std::env::var("NESTWEAVER_DAEMON_PIDFILE_LOCK_HELD").as_deref() == Ok("1")
                && std::fs::read_to_string(lifecycle::pidfile_path(instance_id))
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    == Some(std::process::id());
        if inherited_daemonize_lock {
            return Ok(None);
        }
    }
    // The pidfile lives in the per-instance runtime dir (created on demand).
    Ok(Some(claim_pidfile_lock(&lifecycle::pidfile_path(
        instance_id,
    ))?))
}

/// Create `pid_path` (and its parent dir), write our pid, and take an exclusive
/// non-blocking `flock`, returning the handle whose lifetime holds the lock.
/// Fails if another open file description already holds the lock. Split out from
/// [`claim_instance_lock`] so the flock semantics are unit-testable against a
/// plain temp path, without touching the per-instance runtime dir or env.
fn claim_pidfile_lock(pid_path: &Path) -> Result<std::fs::File, anyhow::Error> {
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create runtime dir: {}", parent.display()))?;
    }

    let pid_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(pid_path)
        .with_context(|| format!("open pidfile: {}", pid_path.display()))?;

    {
        use std::io::Write;
        write!(&pid_file, "{}", std::process::id())
            .with_context(|| format!("write pidfile: {}", pid_path.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = pid_file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            anyhow::bail!("Another daemon instance is already running (pidfile locked)");
        }
    }

    Ok(pid_file)
}

/// Build a rustls TLS acceptor backed by an in-memory self-signed certificate
/// for the given SANs. Used both as the ACME provisioning-failure fallback
/// (encryption preserved even though the cert is untrusted) and as a hermetic
/// acceptor in tests. Advertises `h2` + `http/1.1` so gRPC and MCP HTTP both
/// negotiate over it. Does NO network I/O.
#[cfg_attr(not(feature = "acme"), allow(dead_code))]
fn build_self_signed_acceptor(
    server_names: &[String],
) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    // ACME's directory client and rustls both need an installed default crypto
    // provider; installing ring here is idempotent with the manual/ACME paths.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Short-lived (self-signed, interim only); ACME's `drive()` swaps in a
    // trusted cert on the next successful renewal without a restart.
    let bundle = nestweaver_engine::tls::generate_tls_bundle(server_names, 397, false)
        .context("generate interim self-signed certificate")?;

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

    let certs = CertificateDer::pem_slice_iter(bundle.server_cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parse interim self-signed certificate PEM")?;
    let key = PrivateKeyDer::from_pem_slice(bundle.server_key_pem.as_bytes())
        .context("parse interim self-signed key PEM")?;

    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut server_config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configure rustls protocol versions for self-signed fallback")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build rustls ServerConfig for self-signed fallback")?;
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(
        server_config,
    )))
}

/// Decide the TLS acceptor after ACME bootstrap has FAILED. This is the
/// security-critical fallback: a public (non-loopback) listener must NEVER be
/// downgraded to cleartext because provisioning failed.
///
/// - **Loopback bind** → `Ok(None)`: plaintext is acceptable (process-local),
///   matching the pre-existing loopback fast path.
/// - **Non-loopback bind** → `Ok(Some(acceptor))`: an interim self-signed TLS
///   acceptor so bearer tokens and source stay encrypted (the client sees a
///   trust error, but nothing travels in the clear). ACME keeps retrying in the
///   background via `drive()` and swaps in a trusted cert without a restart.
/// - If even the self-signed acceptor cannot be built on a non-loopback bind,
///   the error propagates so the caller **fails closed** (refuses to bind)
///   rather than serving plaintext.
///
/// No mature ACME server downgrades to plaintext on failure; this mirrors
/// CertMagic/Caddy and cert-manager behaviour.
#[cfg_attr(not(feature = "acme"), allow(dead_code))]
fn acme_failure_fallback_acceptor(
    domain: &str,
    bind_is_loopback: bool,
) -> anyhow::Result<Option<tokio_rustls::TlsAcceptor>> {
    if bind_is_loopback {
        return Ok(None);
    }
    Ok(Some(build_self_signed_acceptor(&[domain.to_string()])?))
}

/// Per-connection TLS handshake budget. A client that opens a TCP connection
/// but never sends a ClientHello must not stall the accept loop, so each
/// handshake runs in its own task under this timeout (B3 — serial-handshake DoS).
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run a single TLS handshake under [`TLS_HANDSHAKE_TIMEOUT`].
///
/// - `Ok(Some(tls))` — handshake completed.
/// - `Ok(None)` — the handshake timed out; the caller drops the connection.
/// - `Err(e)` — the handshake failed (bad ClientHello, etc.).
///
/// This isolates a slow/stalled peer to its own task+timeout so it cannot block
/// new connections on a public listener.
async fn accept_tls_with_timeout(
    acceptor: &tokio_rustls::TlsAcceptor,
    stream: tokio::net::TcpStream,
    timeout: Duration,
) -> std::io::Result<Option<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>> {
    match tokio::time::timeout(timeout, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => Ok(Some(tls)),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => Ok(None),
    }
}

/// Ceiling on TLS handshakes in flight at once per public listener. The
/// per-connection handshake tasks (B3) each hold a `TcpStream` + acceptor Arc
/// for up to [`TLS_HANDSHAKE_TIMEOUT`]; without a cap, a flood of
/// connecting-but-silent clients would spawn unbounded tasks and exhaust
/// memory/FDs. A semaphore with this many permits backpressures the accept
/// loop instead.
const MAX_INFLIGHT_HANDSHAKES: usize = 256;

/// Run one TLS handshake while holding `permit` for its whole duration, so the
/// caller's semaphore bounds how many handshakes run concurrently (the permit
/// is released when this future completes — on success, timeout, or error). A
/// completed stream is forwarded to `tx`; a timeout/error drops the connection.
async fn drive_capped_handshake(
    permit: tokio::sync::OwnedSemaphorePermit,
    acceptor: tokio_rustls::TlsAcceptor,
    stream: tokio::net::TcpStream,
    timeout: Duration,
    tx: tokio::sync::mpsc::Sender<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>,
    label: &'static str,
) {
    // Held until this future returns, then dropped → permit returned to the pool.
    let _permit = permit;
    match accept_tls_with_timeout(&acceptor, stream, timeout).await {
        Ok(Some(tls)) => {
            let _ = tx.send(tls).await;
        }
        Ok(None) => {
            tracing::debug!("{label} TLS handshake timed out; dropping connection")
        }
        Err(e) => tracing::debug!("{label} TLS handshake failed: {e}"),
    }
}

/// Reject any present webhook secret shorter than [`MIN_WEBHOOK_SECRET_LEN`]
/// bytes. Both the active secret and the rotation fallback are checked. `None`
/// secrets (webhook signature verification disabled) are accepted. The error
/// names which secret is too short and the required minimum.
fn validate_webhook_secret_lengths(
    webhook_secret: &Option<String>,
    webhook_secret_old: &Option<String>,
) -> anyhow::Result<()> {
    for (name, secret) in [
        ("webhook", webhook_secret),
        ("webhook-old", webhook_secret_old),
    ] {
        if let Some(s) = secret
            && s.len() < MIN_WEBHOOK_SECRET_LEN
        {
            anyhow::bail!(
                "{name} secret is too short ({} bytes); minimum is {MIN_WEBHOOK_SECRET_LEN} bytes",
                s.len()
            );
        }
    }
    Ok(())
}

/// Build the per-repo index-strategy map consumed by
/// [`nestweaver_engine::worker::WorkerPool::with_repo_types`]. Keyed by the same
/// [`canonical_repo_id`](nestweaver_engine::jobs::canonical_repo_id) the worker
/// uses for lookup so vault repos index as markdown; untyped repos default to
/// [`RepoType::Code`](nestweaver_engine::RepoType::Code).
fn build_repo_types(
    repos: &[nestweaver_engine::RepoConfig],
) -> std::collections::HashMap<String, nestweaver_engine::RepoType> {
    repos
        .iter()
        .map(|repo| {
            (
                nestweaver_engine::jobs::canonical_repo_id(&repo.url),
                repo.repo_type
                    .clone()
                    .unwrap_or(nestweaver_engine::RepoType::Code),
            )
        })
        .collect()
}

/// Hard upper bound on client/peer-supplied traversal depth, enforced in ALL modes.
const HARD_MAX_DEPTH: u64 = 64;

/// Clamp the `depth`/`max_depth` args at [`HARD_MAX_DEPTH`] before dispatch. A huge depth can
/// overflow the stack in the recursive trace builders (build_flow_tree / walk_trace) or run the
/// graph away in impact BFS. Values under the cap, missing keys, and non-numeric values are
/// left untouched.
fn clamp_traversal_depth(args: &mut serde_json::Value) {
    for key in ["depth", "max_depth"] {
        if let Some(n) = args.get(key).and_then(|v| v.as_u64())
            && n > HARD_MAX_DEPTH
        {
            args[key] = serde_json::json!(HARD_MAX_DEPTH);
        }
    }
}

#[cfg(test)]
mod depth_clamp_tests {
    use super::*;

    #[test]
    fn clamps_huge_depth_leaves_small_and_nonnumeric_alone() {
        let mut a = serde_json::json!({ "depth": 2_000_000_000u64, "max_depth": 5, "other": 1 });
        clamp_traversal_depth(&mut a);
        assert_eq!(a["depth"], serde_json::json!(HARD_MAX_DEPTH));
        assert_eq!(a["max_depth"], serde_json::json!(5)); // under the cap → untouched
        assert_eq!(a["other"], serde_json::json!(1)); // unrelated key → untouched

        let mut b = serde_json::json!({ "depth": "not-a-number" });
        clamp_traversal_depth(&mut b);
        assert_eq!(b["depth"], serde_json::json!("not-a-number"));
    }
}

#[cfg(test)]
mod embedding_status_tests {
    use super::*;
    use nestweaver_engine::config::{EmbeddingAccelerator, EmbeddingConfig};

    fn config(accelerator: EmbeddingAccelerator, external: bool) -> EmbeddingConfig {
        EmbeddingConfig {
            accelerator,
            external_endpoint: external.then(|| "http://127.0.0.1:9".to_string()),
            external_model: external.then(|| "external-model".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn loading_and_disabled_statuses_are_explicit() {
        let loading = initial_embedding_status(
            &config(EmbeddingAccelerator::Auto, false),
            Some("stored-model"),
            true,
            true,
        );
        assert_eq!(loading.state, "loading");
        assert_eq!(loading.backend, "local");
        assert_eq!(loading.requested_device, "auto");
        assert_eq!(loading.selected_device, "");
        assert_eq!(loading.model_id, "stored-model");
        assert!(loading.metal_compiled);
        assert!(!loading.fallback_used);

        let disabled = initial_embedding_status(
            &config(EmbeddingAccelerator::Cpu, false),
            None,
            false,
            false,
        );
        assert_eq!(disabled.state, "disabled");
        assert_eq!(disabled.selected_device, "");
        assert!(!disabled.fallback_used);
    }

    #[test]
    fn ready_local_status_reports_the_selected_device_without_fallback() {
        for (selected, requested) in [
            ("metal", EmbeddingAccelerator::Metal),
            ("cpu", EmbeddingAccelerator::Cpu),
        ] {
            let initial = initial_embedding_status(
                &config(requested, false),
                None,
                true,
                selected == "metal",
            );
            let ready = finalize_embedding_status(
                initial,
                Some(384),
                Ok(EmbeddingProbeMetadata {
                    backend: "local",
                    selected_device: selected,
                    vector_dimension: 384,
                }),
            );
            assert_eq!(ready.state, "ready");
            assert_eq!(ready.selected_device, selected);
            assert!(ready.error.is_empty());
            assert!(!ready.fallback_used);
        }
    }

    #[test]
    fn failed_local_external_and_dimension_states_are_actionable() {
        let metal = initial_embedding_status(
            &config(EmbeddingAccelerator::Metal, false),
            None,
            true,
            true,
        );
        let metal_failed =
            finalize_embedding_status(metal, None, Err("Metal device unavailable".to_string()));
        assert_eq!(metal_failed.state, "failed");
        assert!(metal_failed.error.contains("Metal"));
        assert_eq!(metal_failed.selected_device, "");

        let external =
            initial_embedding_status(&config(EmbeddingAccelerator::Auto, true), None, true, true);
        let external_failed =
            finalize_embedding_status(external, None, Err("external endpoint refused".to_string()));
        assert_eq!(external_failed.state, "failed");
        assert_eq!(external_failed.backend, "external");
        assert_eq!(external_failed.selected_device, "");

        let cpu =
            initial_embedding_status(&config(EmbeddingAccelerator::Cpu, false), None, true, false);
        let mismatch = finalize_embedding_status(
            cpu,
            Some(768),
            Ok(EmbeddingProbeMetadata {
                backend: "local",
                selected_device: "cpu",
                vector_dimension: 384,
            }),
        );
        assert_eq!(mismatch.state, "failed");
        assert!(mismatch.error.contains("384"));
        assert!(mismatch.error.contains("768"));
        assert_eq!(mismatch.selected_device, "");
        assert!(!mismatch.fallback_used);
    }
}

#[cfg(all(test, feature = "embed"))]
mod embedding_load_config_tests {
    use super::*;
    use candle_core::{DType, Device};
    use candle_nn::{VarBuilder, VarMap};
    use candle_transformers::models::bert::{BertModel, Config as BertConfig};
    use std::path::Path;
    use tokenizers::Tokenizer;
    use tokenizers::models::wordlevel::WordLevel;
    use tokenizers::pre_tokenizers::whitespace::Whitespace;

    fn write_complete_hf_cache(cache_dir: &Path) -> nestweaver_embed::ModelArtifacts {
        const CONFIG_JSON: &str = r#"{
            "vocab_size": 3,
            "hidden_size": 4,
            "num_hidden_layers": 1,
            "num_attention_heads": 1,
            "intermediate_size": 8,
            "hidden_act": "gelu",
            "hidden_dropout_prob": 0.0,
            "max_position_embeddings": 8,
            "type_vocab_size": 2,
            "initializer_range": 0.02,
            "layer_norm_eps": 0.000000000001,
            "pad_token_id": 0,
            "position_embedding_type": "absolute",
            "use_cache": false,
            "classifier_dropout": null,
            "model_type": "bert"
        }"#;
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let repo_dir = cache_dir.join("models--test-owner--test-model");
        let snapshot_dir = repo_dir.join("snapshots").join(commit);
        std::fs::create_dir_all(repo_dir.join("refs")).expect("create refs");
        std::fs::create_dir_all(&snapshot_dir).expect("create snapshot");
        std::fs::write(repo_dir.join("refs").join("main"), commit).expect("write ref");
        let artifacts = nestweaver_embed::ModelArtifacts {
            config: snapshot_dir.join("config.json"),
            tokenizer: snapshot_dir.join("tokenizer.json"),
            weights: snapshot_dir.join("model.safetensors"),
        };

        std::fs::write(&artifacts.config, CONFIG_JSON).expect("write model config");
        let config: BertConfig = serde_json::from_str(CONFIG_JSON).expect("parse model config");
        let varmap = VarMap::new();
        let builder = VarBuilder::from_varmap(&varmap, DType::F32, &Device::Cpu);
        BertModel::load(builder, &config).expect("initialize tiny BERT fixture");
        varmap
            .save(&artifacts.weights)
            .expect("write model weights");

        let vocab = [
            ("[PAD]".to_string(), 0_u32),
            ("[UNK]".to_string(), 1_u32),
            ("test".to_string(), 2_u32),
        ]
        .into_iter()
        .collect();
        let tokenizer_model = WordLevel::builder()
            .vocab(vocab)
            .unk_token("[UNK]".to_string())
            .build()
            .expect("build tokenizer model");
        let mut tokenizer = Tokenizer::new(tokenizer_model);
        tokenizer.with_pre_tokenizer(Some(Whitespace));
        tokenizer
            .save(&artifacts.tokenizer, false)
            .expect("write tokenizer");

        artifacts
    }

    #[test]
    fn daemon_accelerator_maps_each_policy() {
        assert_eq!(
            daemon_embedding_device_policy(nestweaver_engine::config::EmbeddingAccelerator::Auto),
            nestweaver_embed::DevicePolicy::Auto
        );
        assert_eq!(
            daemon_embedding_device_policy(nestweaver_engine::config::EmbeddingAccelerator::Metal),
            nestweaver_embed::DevicePolicy::Metal
        );
        assert_eq!(
            daemon_embedding_device_policy(nestweaver_engine::config::EmbeddingAccelerator::Cpu),
            nestweaver_embed::DevicePolicy::Cpu
        );
    }

    #[test]
    fn stored_model_identity_targets_the_active_backend() {
        let cache_dir = std::path::PathBuf::from("/tmp/nestweaver-embed-test");
        let local = nestweaver_engine::config::EmbeddingConfig::default();
        let local_config = embedding_load_config(&local, cache_dir.clone(), Some("stored-local"));
        assert_eq!(local_config.model_id, "stored-local");
        assert_eq!(local_config.external_model, None);

        let external = nestweaver_engine::config::EmbeddingConfig {
            external_endpoint: Some("http://127.0.0.1:11434/v1".to_string()),
            external_model: Some("configured-external".to_string()),
            ..Default::default()
        };
        let external_config = embedding_load_config(&external, cache_dir, Some("stored-external"));
        assert_eq!(external_config.model_id, external.model_id);
        assert_eq!(
            external_config.external_model.as_deref(),
            Some("stored-external")
        );
    }

    #[test]
    fn daemon_embedding_startup_constructs_cached_model_offline() {
        let cache = tempfile::tempdir().expect("cache tempdir");
        write_complete_hf_cache(cache.path());
        let config = nestweaver_embed::EmbedConfig {
            model_id: "test-owner/test-model".to_string(),
            cache_dir: cache.path().to_path_buf(),
            external_endpoint: None,
            external_model: None,
        };

        let model = load_daemon_embedding_backend_with(
            &config,
            nestweaver_embed::DevicePolicy::Cpu,
            nestweaver_embed::EmbedModel::load_with_policy_and_artifact_mode,
        )
        .expect("daemon startup must construct a complete configured-cache model offline");

        assert_eq!(
            model.backend_kind(),
            nestweaver_embed::EmbeddingBackendKind::Local
        );
        assert_eq!(model.device_kind(), Some(nestweaver_embed::DeviceKind::Cpu));
        assert_eq!(model.known_dimension(), Some(4));
        let embeddings = model.embed(&["test"]).expect("embed with loaded fixture");
        assert_eq!(embeddings.len(), 1);
        assert_eq!(embeddings[0].len(), 4);
        assert!(embeddings[0].iter().all(|value| value.is_finite()));
    }
}

#[cfg(feature = "embed")]
fn daemon_embedding_device_policy(
    accelerator: nestweaver_engine::config::EmbeddingAccelerator,
) -> nestweaver_embed::DevicePolicy {
    match accelerator {
        nestweaver_engine::config::EmbeddingAccelerator::Auto => {
            nestweaver_embed::DevicePolicy::Auto
        }
        nestweaver_engine::config::EmbeddingAccelerator::Metal => {
            nestweaver_embed::DevicePolicy::Metal
        }
        nestweaver_engine::config::EmbeddingAccelerator::Cpu => nestweaver_embed::DevicePolicy::Cpu,
    }
}

#[cfg(feature = "embed")]
fn load_daemon_embedding_backend_with<T, Load>(
    config: &nestweaver_embed::EmbedConfig,
    policy: nestweaver_embed::DevicePolicy,
    load: Load,
) -> anyhow::Result<T>
where
    Load: FnOnce(
        &nestweaver_embed::EmbedConfig,
        nestweaver_embed::DevicePolicy,
        nestweaver_embed::ArtifactMode,
    ) -> anyhow::Result<T>,
{
    load(config, policy, nestweaver_embed::ArtifactMode::CacheOnly)
}

#[cfg(feature = "embed")]
fn embedding_load_config(
    cfg: &nestweaver_engine::config::EmbeddingConfig,
    cache_dir: std::path::PathBuf,
    stored_model_id: Option<&str>,
) -> nestweaver_embed::EmbedConfig {
    let stored_model_id = stored_model_id.filter(|model_id| !model_id.is_empty());
    let (model_id, external_model) = if cfg.external_endpoint.is_some() {
        (
            cfg.model_id.clone(),
            stored_model_id
                .map(ToOwned::to_owned)
                .or_else(|| cfg.external_model.clone()),
        )
    } else {
        (
            stored_model_id
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| cfg.model_id.clone()),
            cfg.external_model.clone(),
        )
    };

    nestweaver_embed::EmbedConfig {
        model_id,
        cache_dir,
        external_endpoint: cfg.external_endpoint.clone(),
        external_model,
    }
}

#[cfg(feature = "embed")]
fn probe_embedding_model(
    model: &nestweaver_embed::EmbedModel,
) -> Result<EmbeddingProbeMetadata, String> {
    let backend = match model.backend_kind() {
        nestweaver_embed::EmbeddingBackendKind::Local => "local",
        nestweaver_embed::EmbeddingBackendKind::External => "external",
    };
    let selected_device = match model.device_kind() {
        Some(nestweaver_embed::DeviceKind::Metal) => "metal",
        Some(nestweaver_embed::DeviceKind::Cpu) => "cpu",
        None => "",
    };
    model
        .embed_query("NestWeaver embedding readiness probe")
        .and_then(|vector| {
            anyhow::ensure!(
                !vector.is_empty(),
                "readiness probe returned an empty vector"
            );
            anyhow::ensure!(
                vector.iter().all(|value| value.is_finite()),
                "readiness probe returned a non-finite vector"
            );
            Ok(EmbeddingProbeMetadata {
                backend,
                selected_device,
                vector_dimension: vector.len(),
            })
        })
        .map_err(|error| format!("{error:#}"))
}

/// External embedding uses `reqwest::blocking`; execute that probe on Tokio's
/// blocking pool so constructing/dropping its private runtime never happens
/// inside an async worker. Local probing remains inline at the caller's
/// main-thread control point because Candle Metal requires it.
#[cfg(feature = "embed")]
async fn probe_loaded_embedding_model(
    model: nestweaver_embed::EmbedModel,
) -> Result<(nestweaver_embed::EmbedModel, EmbeddingProbeMetadata), String> {
    match model.backend_kind() {
        nestweaver_embed::EmbeddingBackendKind::External => {
            tokio::task::spawn_blocking(move || {
                let metadata = probe_embedding_model(&model)?;
                Ok::<_, String>((model, metadata))
            })
            .await
            .map_err(|error| format!("external readiness probe task failed: {error}"))?
        }
        nestweaver_embed::EmbeddingBackendKind::Local => {
            let metadata = probe_embedding_model(&model)?;
            Ok((model, metadata))
        }
    }
}

/// Load the embedding model into `state.embedding_runtime`. MUST be called on the daemon's main
/// (block_on) thread: candle compiles Metal shaders via MTLCompilerService, an Aqua
/// per-session XPC service reachable from the main thread but NOT from a tokio worker/blocking
/// thread. Called AFTER the UDS server is spawned. Local model artifacts are resolved strictly
/// from the configured cache, so daemon startup never initiates a network download. During the
/// load, non-semantic RPCs are served normally and semantic search returns "model not loaded"
/// until it completes.
///
/// The production call site is gated `not(test)`. The function remains testable so the external
/// path can be exercised under Tokio; only the local backend has the main-thread requirement.
#[cfg(feature = "embed")]
async fn load_embedding_model(state: &std::sync::Arc<DaemonState>) {
    let cfg = state
        .instance_cfg
        .as_ref()
        .map(|c| c.embedding.clone())
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    if cfg.external_endpoint.is_none() {
        // MTLCompilerService is only reachable from the process main thread;
        // loading elsewhere silently falls back to CPU embeddings. External
        // models do not touch Metal and are deliberately exempt so their
        // blocking HTTP probe can be tested/driven from an async runtime.
        let on_main = unsafe { libc::pthread_main_np() } != 0;
        debug_assert!(
            on_main,
            "local load_embedding_model must run on the main (block_on) thread"
        );
        if !on_main {
            tracing::warn!(
                "local embedding model loading off the main thread; Metal GPU will be unavailable"
            );
        }
    }
    // The DB records which model generated the stored embeddings (set on embed). Load THAT
    // model regardless of the compiled default or config — it must match the stored vectors,
    // or semantic search is disabled on a dimension mismatch. This lets the shipped default
    // stay light while a given instance uses whatever it was actually embedded with.
    let stored_model_id = state
        .store
        .get_embedding_metadata()
        .ok()
        .flatten()
        .map(|(model_id, _)| model_id);
    if let Some(stored_model_id) = stored_model_id.as_deref() {
        let configured_model = if cfg.external_endpoint.is_some() {
            cfg.external_model.as_deref().unwrap_or_default()
        } else {
            &cfg.model_id
        };
        if stored_model_id != configured_model {
            tracing::info!(
                stored = %stored_model_id,
                configured = %configured_model,
                "Loading the embedding model recorded in the DB (matches stored embeddings)"
            );
        }
    }
    // Expand tilde in cache_dir using the home directory.
    let cache_dir = if cfg.cache_dir.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(&cfg.cache_dir[2..])
        } else {
            std::path::PathBuf::from(&cfg.cache_dir)
        }
    } else {
        std::path::PathBuf::from(&cfg.cache_dir)
    };
    let policy = daemon_embedding_device_policy(cfg.accelerator);
    let config = embedding_load_config(&cfg, cache_dir, stored_model_id.as_deref());
    let loaded = load_daemon_embedding_backend_with(
        &config,
        policy,
        nestweaver_embed::EmbedModel::load_with_policy_and_artifact_mode,
    );
    match loaded {
        Ok(model) => match probe_loaded_embedding_model(model).await {
            Ok((model, probe)) => {
                let status = finalize_embedding_status(
                    state.embedding_runtime.status(),
                    state.store.embedding_index_dimension(),
                    Ok(probe),
                );
                if status.state == "ready" {
                    let backend = status.backend.clone();
                    let selected_device = status.selected_device.clone();
                    state.embedding_runtime.publish_ready(
                        status,
                        std::sync::Arc::new(model)
                            as std::sync::Arc<dyn nestweaver_engine::EmbedQueryFn>,
                    );
                    tracing::info!(backend, device = selected_device, "Embedding model ready");
                } else {
                    let error = status.error.clone();
                    state.embedding_runtime.publish_unavailable(status);
                    tracing::warn!("Embedding model failed readiness: {error}");
                }
            }
            Err(error) => {
                let status = finalize_embedding_status(
                    state.embedding_runtime.status(),
                    state.store.embedding_index_dimension(),
                    Err(error),
                );
                let error = status.error.clone();
                state.embedding_runtime.publish_unavailable(status);
                tracing::warn!("Embedding model failed readiness: {error}");
            }
        },
        Err(e) => {
            let status = finalize_embedding_status(
                state.embedding_runtime.status(),
                state.store.embedding_index_dimension(),
                Err(format!("{e:#}")),
            );
            state.embedding_runtime.publish_unavailable(status);
            tracing::warn!("Failed to load embedding model: {e}");
        }
    }
}

pub async fn run_server(
    db_path: &Path,
    mut idle_timeout: Option<Duration>,
    config_path: Option<&Path>,
    server_opts: Option<ServerOpts>,
) -> Result<(), anyhow::Error> {
    // Idle-timeout counts only gRPC/UDS reads+writes (active_reads/active_writes)
    // and indexing — the MCP-over-HTTP path (server mode only) does NOT bump them,
    // so an idle timer would treat an actively-querying MCP client as idle and
    // shut the server down mid-use. Force it off in server mode. Today the two are
    // already mutually exclusive (the idle-enabled Start path is UDS-only), so this
    // is a guard against a future regression, not a live bug.
    if server_opts.is_some() && idle_timeout.is_some() {
        tracing::warn!(
            "idle_timeout is ignored in server mode (MCP-HTTP activity is not \
             counted); forcing it off"
        );
        idle_timeout = None;
    }

    // Canonicalize if possible, but don't fail if the DB doesn't exist yet.
    // The DB will be created by GraphStore::open_or_create below.
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let db_path = lifecycle::canonical_db_path(db_path);

    let instance_id = lifecycle::instance_id_from_db_path(&db_path);
    let instance_label = lifecycle::instance_label_from_db_path(&db_path);
    // nw-019: two identities. `instance_id` (db-path hash) is for RUNTIME paths
    // only — sockets/pidfiles/launchd/replica locks (104-byte sun_path limit).
    // `data_instance_id` (the config's logical name when we have one) is what
    // gets written into graph nodes, so users see and type one name everywhere.
    // Built later once `instance_cfg` is parsed; a config-less start falls back
    // to the hash so `data_instance_id == instance_id`.

    // Set up daily-rolling log file via tracing-appender. This replaces
    // the manual rotate-at-startup approach and handles rotation while the
    // daemon is running. Max 3 log files retained.
    let log_dir_path = lifecycle::log_dir(&instance_id);
    std::fs::create_dir_all(&log_dir_path).ok();
    let file_appender = tracing_appender::rolling::daily(&log_dir_path, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    tracing::info!(
        db = %db_path.display(),
        instance = %instance_label,
        "daemon process starting"
    );
    eprintln!(
        "[daemon] starting for {} (instance {instance_label})",
        db_path.display()
    );

    // Claim the single-owner instance lock BEFORE materializing a snapshot. A
    // duplicate replica on the identical `--db` shares this instance id (and its
    // `replica-work-<id>` dir), so rejecting it here stops its materialize copy
    // from truncating a live sibling's working copy. Held for the process
    // lifetime; released on drop. A daemonize child proves it inherited the
    // launcher's lock instead of trying to acquire a conflicting second flock.
    let _pid_guard = claim_instance_lock(&instance_id)?;

    // Snapshot replica: materialize the snapshot into a private working copy and
    // serve it read-only. `read_only` gates out the write RPCs (via the
    // `ReadOnlyGuard` transport layer) and the write machinery
    // (worker/scheduler/webhook) below. Default (`snapshot: None`) keeps the
    // read-write path unchanged.
    let read_only = server_opts
        .as_ref()
        .and_then(|o| o.snapshot.as_ref())
        .is_some();
    let db_path = if let Some(snapshot_dir) = server_opts.as_ref().and_then(|o| o.snapshot.clone())
    {
        let cfg = config_path.and_then(|p| nestweaver_engine::InstanceConfig::from_file(p).ok());
        let (_, _, schema_hash) = nestweaver_engine::schema_hashes(cfg.as_ref());
        let embedding_model = cfg
            .as_ref()
            .map(|c| c.embedding.model_id.clone())
            .unwrap_or_else(|| "unknown".to_string());
        // Private per-replica working dir keyed on the instance id so two
        // co-located replicas (distinct `--db` under one parent) never share a
        // mutable path and clobber each other's materialized copy.
        let working_dir = replica_working_dir(&db_path, &instance_id);
        tracing::info!(snapshot = %snapshot_dir.display(), "booting as read-only snapshot replica");
        nestweaver_engine::materialize_snapshot(
            &snapshot_dir,
            &working_dir,
            env!("CARGO_PKG_VERSION"),
            &schema_hash,
            &embedding_model,
        )
        .with_context(|| format!("failed to materialize snapshot {}", snapshot_dir.display()))?
    } else {
        db_path
    };

    // Open the graph store: read-only for a snapshot replica, read-write
    // otherwise (the daemon is the sole DB owner).
    // Time the pre-bind phases. A client gives the daemon a bounded window to
    // bind its socket, and when that expired the only evidence was a single
    // "[daemon] starting" line — nothing said WHERE the time went, so the cause
    // had to be guessed at. It is not the embedding model: that loads long after
    // the socket is serving (see `load_embedding_model` below). Opening the
    // database is the dominant pre-bind cost, so measure it (nw-114).
    let boot_started = std::time::Instant::now();

    let store = if read_only {
        GraphStore::open_read_only(&db_path).with_context(|| {
            format!("failed to open snapshot read-only at {}", db_path.display())
        })?
    } else {
        match GraphStore::open_or_create(&db_path) {
            Ok(s) => s,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to open database with write access at {}; \
                         another process may hold the write lock",
                        db_path.display()
                    )
                });
            }
        }
    };
    let store_open_ms = boot_started.elapsed().as_millis() as u64;

    // Load sidecars (PageRank, interaction scores).
    //
    // nw-119: nw-114 attributed daemon boot cost to opening the store, and the
    // instrumentation it added disproved that — `boot_ms=6712` against
    // `store_open_ms=876` on a warm cache, and a later boot measured 36,474ms
    // against 10,041ms. So the majority of pre-bind time was somewhere in here
    // and nobody could say where. Guessing once was already wrong; time each
    // phase instead. These sidecars are not small (the production
    // `.pagerank.json` is 13 MB of JSON), which is a candidate but not yet a
    // conclusion.
    let sidecar_started = std::time::Instant::now();
    nestweaver_engine::migrate_sidecar(&db_path, "pagerank.json", ".pagerank.json");
    let pr_path = nestweaver_engine::sidecar_path(&db_path, ".pagerank.json");
    let _ = store.load_pagerank_cache(&pr_path);
    let pagerank_load_ms = sidecar_started.elapsed().as_millis() as u64;

    let interactions_started = std::time::Instant::now();
    if let Some(scores) = nestweaver_engine::load_interaction_scores(&db_path) {
        store.load_interaction_cache(scores);
    }
    let interactions_load_ms = interactions_started.elapsed().as_millis() as u64;

    // Open the Tantivy index. A read-only snapshot replica intentionally
    // disables search reconciliation and uses a reader when one is available.
    // A read-write daemon requests a writer for vault indexing and deletion
    // repair. If that writer is unavailable, a reader can still serve queries,
    // but the explicit unavailable reconciliation state makes a later indexed
    // mutation fail instead of silently skipping repair. When neither handle
    // opens, queries use substring fallback and indexed mutations still surface
    // the configured-index failure.
    let tantivy_path = nestweaver_mcp::tantivy_sidecar_path(&db_path);
    let tantivy_started = std::time::Instant::now();
    let (tantivy, search_reconciliation) = open_search_index(&tantivy_path, read_only);
    let tantivy_open_ms = tantivy_started.elapsed().as_millis() as u64;

    let idle_notify = Arc::new(Notify::new());
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Load the InstanceConfig once at start if `--config` was supplied so
    // tool dispatch (e.g. F6 `[ranking]` priors in `brain_search`) can apply
    // it without re-parsing the file per RPC.
    //
    // Distinguish a MISSING file (non-fatal — the daemon serves with built-in
    // defaults) from a file that is PRESENT but unparseable. In server mode a
    // malformed config means the server would silently index nothing and have no
    // webhook secret; that failure must be loud (stderr) and fatal rather than a
    // warning buried in the rotating log file.
    let instance_cfg = match config_path {
        None => None,
        Some(p) => match nestweaver_engine::InstanceConfig::from_file(p) {
            Ok(c) => {
                tracing::info!(
                    config = %p.display(),
                    "loaded instance config (ranking, response, features)"
                );
                Some(Arc::new(c))
            }
            Err(e) if p.exists() => {
                // Present but broken. Surface to the console (docker/foreground
                // operators never see the rotating log) and fail fast in server
                // mode so a typo can't masquerade as a healthy-but-empty server.
                eprintln!("[daemon] failed to parse --config {}: {e}", p.display());
                tracing::error!(config = %p.display(), error = %e, "failed to parse --config");
                if server_opts.is_some() {
                    anyhow::bail!(
                        "invalid --config {}: {e} (server mode requires a parseable config)",
                        p.display()
                    );
                }
                None
            }
            Err(e) => {
                tracing::warn!(
                    config = %p.display(),
                    error = %e,
                    "instance config not found — using built-in defaults"
                );
                None
            }
        },
    };

    // nw-019: graph-data identity — the config's logical `instance_id` when we
    // have a parsed config, else fall back to the runtime hash so a config-less
    // daemon still has `data_instance_id == instance_id`.
    let data_instance_id = instance_cfg
        .as_ref()
        .map(|c| c.instance_id.clone())
        .unwrap_or_else(|| instance_id.clone());

    let is_server_mode = server_opts.is_some();

    // Build safeguards and rate limiters for server mode.
    let safeguards = if is_server_mode {
        QuerySafeguards::default_server()
    } else {
        QuerySafeguards::disabled()
    };

    let rate_limiters = if is_server_mode {
        let config = RateLimitConfig::default();
        Some(Arc::new(ClientRateLimiters::new(&config)))
    } else {
        None
    };

    // Extract admin token from server opts (if present).
    let admin_token = server_opts
        .as_ref()
        .and_then(|opts| opts.admin_token.clone());

    // Reject any present auth/admin token that is too short to be safe.
    validate_token_lengths(
        &server_opts
            .as_ref()
            .and_then(|opts| opts.auth_token.clone()),
        &admin_token,
    )?;

    // A binary built WITHOUT the `acme` feature cannot provision certs. Reject
    // --acme-domain up front rather than binding a non-loopback listener with no
    // certificate behind it. Checked BEFORE the bind gate so the operator gets
    // this actionable message. The passthrough in the root Cargo.toml makes
    // `--features acme` a valid build of the top-level binary, so the
    // instruction is now correct (B2).
    #[cfg(not(feature = "acme"))]
    if server_opts
        .as_ref()
        .and_then(|o| o.acme_domain.as_ref())
        .is_some()
    {
        anyhow::bail!(
            "--acme-domain requires a binary built with `--features acme`, but this \
             binary was compiled without it. Rebuild with `cargo build --release \
             --features acme` (or `cargo install --features acme`), or use \
             --tls-cert/--tls-key for manual TLS instead."
        );
    }

    // Enforce bind-scope security. ACME only counts as TLS when the feature is
    // actually compiled in (`acme_provides_tls`); on a no-feature build the
    // guard above has already bailed, so here a non-loopback ACME-only bind
    // would fail closed rather than being waved through as "TLS" and then
    // serving cleartext.
    if let Some(ref opts) = server_opts {
        validate_bind_security(
            &opts.bind_addr,
            &opts.auth_token,
            &opts.tls_cert,
            &opts.tls_key,
            acme_provides_tls(opts.acme_domain.is_some()),
        )?;
    }

    // Reject any present webhook secret that is too short to be safe.
    validate_webhook_secret_lengths(
        &server_opts
            .as_ref()
            .and_then(|opts| opts.webhook_secret.clone()),
        &server_opts
            .as_ref()
            .and_then(|opts| opts.webhook_secret_old.clone()),
    )?;

    // Build the per-repo authz policy ONCE, before the state is assembled.
    // nw-119: the first instrumentation pass narrowed the gap but did not close
    // it — store open, pagerank and tantivy together accounted for well under a
    // third of boot, leaving ~52s unattributed on a 66s boot. The 13 MB
    // `.pagerank.json` was the leading suspect and measured 448ms, so that
    // hypothesis is dead too. Everything between the tantivy open and the bind
    // is the remaining candidate; `get_embedding_metadata` is a store query
    // against a 5.6 GB database, which is the next thing worth timing rather
    // than assuming.
    let permission_started = std::time::Instant::now();
    let permission_source = build_daemon_permission_source(instance_cfg.as_ref());
    let permission_source_ms = permission_started.elapsed().as_millis() as u64;
    let embedding_probe_started = std::time::Instant::now();
    let embedding_cfg = instance_cfg
        .as_ref()
        .map(|config| config.embedding.clone())
        .unwrap_or_default();
    let stored_embedding_model = store
        .get_embedding_metadata()
        .ok()
        .flatten()
        .map(|(model_id, _)| model_id);
    let embedding_status = initial_embedding_status(
        &embedding_cfg,
        stored_embedding_model.as_deref(),
        cfg!(feature = "embed"),
        daemon_metal_compiled(),
    );
    let embedding_probe_ms = embedding_probe_started.elapsed().as_millis() as u64;
    let state = Arc::new(DaemonState {
        store: Arc::new(store),

        tantivy,
        search_reconciliation,
        db_path: db_path.clone(),
        read_only,
        instance_id: instance_id.clone(),
        data_instance_id: data_instance_id.clone(),
        start_time: Instant::now(),
        active_reads: Arc::new(AtomicU32::new(0)),
        active_writes: Arc::new(AtomicU32::new(0)),
        idle_notify: idle_notify.clone(),
        shutdown_tx: shutdown_tx.clone(),
        watcher_stop: std::sync::Mutex::new(None),
        next_watcher_id: std::sync::atomic::AtomicU64::new(0),
        instance_cfg,
        permission_source,
        embedding_runtime: Arc::new(EmbeddingRuntime::unavailable(embedding_status)),
        write_mutex: Arc::new(tokio::sync::Mutex::new(())),
        server_mode: is_server_mode,
        indexing_active: Arc::new(AtomicBool::new(false)),
        indexing_repo: Arc::new(tokio::sync::RwLock::new(String::new())),
        indexing_queue_depth: Arc::new(AtomicU32::new(0)),
        safeguards,
        rate_limiters: rate_limiters.clone(),
        drained: Arc::new(AtomicBool::new(false)),
        admin_token,
        admin_state: std::sync::OnceLock::new(),
        worker_handle: std::sync::Mutex::new(None),
        ui_server: std::sync::Mutex::new(None),
    });

    // nw-119: two instrumentation passes narrowed the unattributed boot time to
    // the span between the Tantivy open and the bind — 253s of a 268s boot,
    // with no log output in it at all. Config load, permission source and the
    // embedding probe measured 0-210ms, so the cost is here: these reconcile
    // passes walk extension state against a 5.6 GB graph, synchronously, before
    // the socket is allowed to bind. Measure rather than assume — the previous
    // two hypotheses (store open, then the 13 MB pagerank sidecar) were both
    // wrong.
    let reconcile_started = std::time::Instant::now();
    if !read_only {
        recover_pending_instance_extension_migration(&state).with_context(|| {
            format!(
                "failed to recover pending instance extension migration for {}",
                db_path.display()
            )
        })?;
        nestweaver_engine::reconcile_extension_handoffs(&state.store, &state.db_path)
            .with_context(|| {
                format!(
                    "failed to reconcile pending extension handoffs for {}",
                    db_path.display()
                )
            })?;
        nestweaver_engine::reconcile_extension_liveness(&state.store, &state.db_path)
            .with_context(|| {
                format!(
                    "failed to reconcile extension liveness for {}",
                    db_path.display()
                )
            })?;
    }

    let extension_reconcile_ms = reconcile_started.elapsed().as_millis() as u64;

    // Pre-warm PPR adjacency cache so the first PPR query after startup
    // hits the cache instead of spending ~350ms rebuilding from the DB.
    {
        let store = state.store.clone();
        tokio::task::spawn_blocking(move || match store.warm_ppr_cache() {
            Ok(()) => tracing::info!("PPR adjacency cache warmed"),
            Err(e) => tracing::warn!("failed to warm PPR cache: {e}"),
        });
    }

    // Spawn periodic rate limiter cleanup (every 10 minutes).
    if let Some(ref rl) = state.rate_limiters {
        let rl = rl.clone();
        let mut sweep_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(600)) => {
                        rl.sweep_stale();
                        tracing::debug!("rate limiter stale entries swept");
                    }
                    _ = sweep_shutdown.changed() => break,
                }
            }
        });
    }

    // The embedding model is loaded on the MAIN thread near the end of run_server — AFTER the
    // UDS server is spawned and the socket is bound + serving. candle needs the main thread to
    // reach Metal, but the socket must not wait on a cold-cache model download. See the serve
    // spawn + `load_embedding_model(&state)` below.
    tracing::debug!("embed feature compiled in: {}", cfg!(feature = "embed"));

    // Wrap the generated service in the single read-only chokepoint. On a
    // read-only snapshot replica this rejects EVERY mutating RPC (typed + JSON)
    // with FAILED_PRECONDITION before it reaches a handler; on a read-write
    // daemon it is a transparent pass-through. Applied once here so both the
    // TCP and UDS transports below inherit the same gate.
    //
    // NOTE: this guard covers the gRPC transport ONLY. The `/mcp` HTTP surface
    // does NOT flow through it — its replica safety comes from the read-only
    // store open plus the `MUTATING_TOOLS` permission gate in the MCP handler
    // (see http.rs). Don't assume this guard wraps `/mcp`.
    let svc = ReadOnlyGuard::new(
        read_only,
        NestWeaverDaemonServer::new(DaemonService::new(state.clone()))
            .max_decoding_message_size(256 * 1024 * 1024)
            .max_encoding_message_size(256 * 1024 * 1024),
    );

    // Prepare the socket path.
    let sock_dir = lifecycle::runtime_dir(&instance_id);
    std::fs::create_dir_all(&sock_dir)
        .with_context(|| format!("create runtime dir: {}", sock_dir.display()))?;

    let sock_path = lifecycle::socket_path(&instance_id);
    let _ = std::fs::remove_file(&sock_path);

    // The single-owner instance lock (pidfile flock) was already claimed above,
    // before snapshot materialization — see `claim_instance_lock`.

    tracing::info!(
        socket = %sock_path.display(),
        instance = %instance_label,
        "daemon starting"
    );

    // Idle timeout: signal shutdown instead of process::exit.
    if let Some(timeout) = idle_timeout {
        let notify = idle_notify.clone();
        let active = state.clone();
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = notify.notified() => continue,
                    _ = tokio::time::sleep(timeout) => {
                        if is_idle(
                            active.active_reads.load(Ordering::Relaxed)
                                + active.active_writes.load(Ordering::Relaxed),
                            active.indexing_active.load(Ordering::Relaxed),
                        ) {
                            tracing::info!(
                                timeout_secs = timeout.as_secs(),
                                "idle timeout reached — shutting down"
                            );
                            let _ = tx.send(true);
                            return;
                        }
                    }
                }
            }
        });
    }

    // Catch SIGTERM for graceful shutdown (sent by `daemon stop`).
    {
        let tx = shutdown_tx.clone();
        let drained = Arc::clone(&state.drained);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("register SIGTERM handler");
            sig.recv().await;
            tracing::info!("received SIGTERM — shutting down");
            // T6.2: stop the worker claiming new jobs BEFORE broadcasting
            // shutdown, mirroring the gRPC Shutdown handler. In-flight jobs
            // still drain via the worker loop's JoinSet; only NEW claims stop.
            drained.store(true, Ordering::Relaxed);
            // Stop any active watcher too — its `spawn_blocking` thread
            // would otherwise outlive the broadcast and pin process exit
            // (Tokio's runtime drop waits for blocking threads) until the
            // client's stop grace elapses and it SIGKILLs us.
            stop_active_watcher(&state);
            let _ = tx.send(true);
        });
    }

    let uds = tokio::net::UnixListener::bind(&sock_path)
        .with_context(|| format!("bind UDS: {}", sock_path.display()))?;
    // The line a stalled-boot investigation actually needs: how long the client
    // had to wait, and where it went.
    //
    // nw-119: `boot_ms` and `store_open_ms` alone showed the store was a small
    // fraction of boot without saying what the rest was, so the phases are
    // broken out. `unattributed_ms` is deliberately reported rather than left
    // for the reader to subtract — if it stays large, the next phase to
    // instrument is still missing, and that should be visible instead of
    // requiring arithmetic nobody performs.
    let boot_ms = boot_started.elapsed().as_millis() as u64;
    tracing::info!(
        boot_ms = boot_ms,
        store_open_ms = store_open_ms,
        pagerank_load_ms = pagerank_load_ms,
        interactions_load_ms = interactions_load_ms,
        tantivy_open_ms = tantivy_open_ms,
        permission_source_ms = permission_source_ms,
        embedding_probe_ms = embedding_probe_ms,
        extension_reconcile_ms = extension_reconcile_ms,
        unattributed_ms = boot_ms.saturating_sub(
            store_open_ms
                + pagerank_load_ms
                + interactions_load_ms
                + tantivy_open_ms
                + permission_source_ms
                + embedding_probe_ms
                + extension_reconcile_ms
        ),
        socket = %sock_path.display(),
        "socket bound and accepting connections"
    );
    let uds_stream = tokio_stream::wrappers::UnixListenerStream::new(uds);

    // Create scheduler command channel. The sender goes into AdminState
    // so the reload endpoint can push commands; the receiver is consumed
    // by the scheduler task below.
    let (scheduler_tx, scheduler_rx) =
        tokio::sync::mpsc::channel::<nestweaver_engine::scheduler::SchedulerCommand>(64);

    // Shared job queue Arc — created once and shared across webhook handler,
    // worker pool, and poll scheduler to avoid concurrent SQLite opens.
    let mut shared_job_queue_opt: Option<
        std::sync::Arc<std::sync::Mutex<nestweaver_engine::jobs::JobQueue>>,
    > = None;

    // Live MCP session-count handle, shared from the MCP HTTP state into the
    // admin API (dashboard "connected clients") and the metrics task
    // (`MCP_SESSIONS` gauge). `None` outside server mode.
    let mut mcp_session_gauge_opt: Option<std::sync::Arc<std::sync::atomic::AtomicU32>> = None;

    // MCP-over-HTTP server — spawned alongside the gRPC servers.
    // Binds to grpc_port + 1 when server mode is active, or a separate OS-assigned
    // port when grpc_port is 0.
    if let Some(ref opts) = server_opts {
        let mcp_state = {
            let mut s = if let Some(ref token) = opts.auth_token {
                nestweaver_mcp::http::McpHttpState::with_auth(
                    false,
                    state.store.clone(),
                    state.tantivy.clone(),
                    state.db_path.clone(),
                    state.instance_cfg.clone(),
                    state.server_mode,
                    token.clone(),
                    opts.admin_token.clone(),
                )
            } else {
                nestweaver_mcp::http::McpHttpState::new(
                    false,
                    state.store.clone(),
                    state.tantivy.clone(),
                    state.db_path.clone(),
                    state.instance_cfg.clone(),
                    state.server_mode,
                )
            };
            // Share the daemon's atomic readiness/model provider so HTTP and
            // gRPC observe the same exact published model.
            s.embed_model_provider =
                Some(state.embedding_runtime.clone()
                    as Arc<dyn nestweaver_engine::EmbedModelProvider>);
            // A read-only replica must reject mutating MCP tools before dispatch,
            // just as the gRPC ReadOnlyGuard rejects mutating RPCs.
            s.read_only = read_only;
            // Build the daemon-side federation coordinator from the instance
            // config's `[[upstream]]` entries. `None` for the common
            // single-node case (no upstreams) — the `/mcp` boundary then stamps
            // the honest single-node provenance.
            s.federation = state.instance_cfg.as_ref().and_then(|cfg| {
                nestweaver_mcp::federation::FederationState::from_instance_config(cfg)
            });
            if let Some(ref fed) = s.federation {
                tracing::info!(
                    upstreams = fed.upstream_count(),
                    "MCP /mcp boundary: federation coordinator active"
                );
            }
            std::sync::Arc::new(s)
        };
        // Spawn the background staleness/health-recovery task, mirroring the
        // hybrid client's maintenance loop. Only when an upstream is configured;
        // aborts on shutdown via the same watch channel as the other sweepers.
        if let Some(ref fed) = mcp_state.federation {
            nestweaver_mcp::federation::spawn_staleness_refresher(
                state.store.clone(),
                fed.clone(),
                shutdown_tx.subscribe(),
            );
        }
        // Share the live session-count handle with the admin API and metrics.
        mcp_session_gauge_opt = Some(mcp_state.mcp_session_gauge.clone());
        nestweaver_mcp::http::spawn_session_sweeper(
            mcp_state.sessions.clone(),
            mcp_state.mcp_session_gauge.clone(),
            shutdown_tx.subscribe(),
        );
        nestweaver_mcp::http::spawn_bucket_sweeper(
            mcp_state.client_rate_limiter.clone(),
            shutdown_tx.subscribe(),
        );
        // Captured before `mcp_state` is moved into `router()` — used for the
        // T5.1 bind-time auth assertion below and to decide whether the network
        // `/metrics` route is gated (S.5).
        let mcp_auth_token = mcp_state.auth_token.clone();
        let mut mcp_router = nestweaver_mcp::http::router(mcp_state);

        // Shared webhook state Arcs — populated inside the webhook block,
        // then passed to AdminState so /admin/api/reload can update them.
        let mut webhook_allowed_repos: Option<
            std::sync::Arc<std::sync::RwLock<Option<std::collections::HashSet<String>>>>,
        > = None;
        let mut webhook_repo_branches: Option<
            std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>,
        > = None;

        // Single shared job queue for webhook handler, worker pool, and
        // poll scheduler.  Opening the same SQLite file from multiple
        // independent connections caused Bus errors (SIGBUS) on macOS
        // when WAL checkpointing raced with a concurrent open.
        let jobs_db_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
        let shared_job_queue: std::sync::Arc<std::sync::Mutex<nestweaver_engine::jobs::JobQueue>> = {
            let jq = nestweaver_engine::jobs::JobQueue::open(&jobs_db_path)
                .with_context(|| format!("open shared job queue: {}", jobs_db_path.display()))?;
            std::sync::Arc::new(std::sync::Mutex::new(jq))
        };
        shared_job_queue_opt = Some(std::sync::Arc::clone(&shared_job_queue));

        if replica_enqueues_config_repos(read_only)
            && let Some(ref cfg) = state.instance_cfg
            && let Ok(queue) = shared_job_queue.lock()
        {
            for repo_cfg in &cfg.repos {
                // nw-019: look up under the same logical instance the worker
                // stamps on the repo, or the "already indexed?" check never
                // matches and we re-enqueue on every boot.
                let repo_uid = nestweaver_schema::repo_uid(&data_instance_id, &repo_cfg.url);
                let needs_initial_index = state
                    .store
                    .lookup_repo(&repo_uid)
                    .ok()
                    .flatten()
                    .map(|repo| repo.indexed_sha.is_empty())
                    .unwrap_or(true);
                if needs_initial_index {
                    // SSRF guard: repos declared in instance.toml bypass the
                    // add_repo API and its validation, so a hostile config could
                    // otherwise smuggle an internal/private target that gets
                    // cloned at startup (the clone/worker path has no SSRF guard
                    // of its own). Run the same synchronous checks add_repo and
                    // reload_config use and refuse to enqueue rejected URLs.
                    // DNS resolution is intentionally skipped here (matching
                    // reload_config) to keep startup non-blocking; only
                    // scheme/literal-IP/localhost checks apply at this stage.
                    if !nestweaver_web::routes::admin::config_repo_url_allowed(&repo_cfg.url) {
                        tracing::warn!(
                            repo = %repo_cfg.url,
                            "startup: skipping config repo — URL rejected by SSRF guard"
                        );
                        continue;
                    }
                    let canonical_id = nestweaver_engine::jobs::canonical_repo_id(&repo_cfg.url);
                    if let Err(e) = queue.upsert(
                        &canonical_id,
                        &repo_cfg.url,
                        nestweaver_engine::jobs::JobTrigger::Unindexed,
                        repo_cfg.branch.as_deref(),
                    ) {
                        tracing::warn!(
                            repo = %repo_cfg.url,
                            error = %e,
                            "failed to enqueue config repo for initial indexing"
                        );
                    }
                }
            }
        }

        // Mount webhook endpoint when a secret is configured — but never on a
        // read-only replica, which has no worker to drain enqueued jobs.
        if replica_mounts_webhook(read_only, opts.webhook_secret.is_some())
            && let Some(ref secret) = opts.webhook_secret
        {
            let allowed_repos: Option<std::collections::HashSet<String>> =
                state.instance_cfg.as_ref().map(|cfg| {
                    cfg.repos
                        .iter()
                        .filter(|r| r.poll.as_deref() != Some("manual"))
                        .map(|r| nestweaver_engine::jobs::canonical_repo_id(&r.url))
                        .collect()
                });
            let repo_branches: std::collections::HashMap<String, String> = state
                .instance_cfg
                .as_ref()
                .map(|cfg| {
                    cfg.repos
                        .iter()
                        .filter_map(|r| {
                            r.branch.as_ref().map(|b| {
                                (
                                    nestweaver_engine::jobs::canonical_repo_id(&r.url),
                                    b.clone(),
                                )
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let shared_allowed = std::sync::Arc::new(std::sync::RwLock::new(allowed_repos));
            let shared_branches = std::sync::Arc::new(std::sync::RwLock::new(repo_branches));
            webhook_allowed_repos = Some(std::sync::Arc::clone(&shared_allowed));
            webhook_repo_branches = Some(std::sync::Arc::clone(&shared_branches));
            let webhook_state = std::sync::Arc::new(crate::webhook::WebhookState {
                config: crate::webhook::WebhookConfig {
                    secret: secret.clone(),
                    secret_old: opts.webhook_secret_old.clone(),
                },
                job_queue: std::sync::Arc::clone(&shared_job_queue),
                allowed_repos: shared_allowed,
                repo_branches: shared_branches,
            });
            mcp_router = mcp_router.route(
                "/webhook",
                axum::routing::post(crate::webhook::handle_webhook)
                    .with_state(webhook_state)
                    .layer(axum::extract::DefaultBodyLimit::max(5_242_880)),
            );
            tracing::info!("webhook endpoint enabled at /webhook");
        }

        // Mount admin API routes when an admin token is configured — but not on
        // a read-only replica, which exposes no mutating admin surface.
        // `/metrics` (mounted below) stays available for replica monitoring.
        if replica_mounts_admin_api(read_only, opts.admin_token.is_some())
            && let Some(ref admin_tok) = opts.admin_token
        {
            let admin_state = std::sync::Arc::new(nestweaver_web::state::AdminState {
                admin_token: admin_tok.clone(),
                auth_token: opts.auth_token.clone(),
                device_flow: std::sync::Arc::new(tokio::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
                daemon_store: state.store.clone(),
                tantivy: state.tantivy.clone(),
                instance_id: state.instance_id.clone(),
                start_time: state.start_time,
                active_reads: state.active_reads.clone(),
                active_writes: state.active_writes.clone(),
                mcp_sessions: mcp_session_gauge_opt
                    .clone()
                    .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0))),
                drained: state.drained.clone(),
                indexing_queue_depth: state.indexing_queue_depth.clone(),
                db_path: db_path.clone(),
                config_path: config_path.map(|p| p.to_path_buf()),
                scheduler_tx: Some(scheduler_tx.clone()),
                webhook_allowed_repos: webhook_allowed_repos.clone(),
                webhook_repo_branches: webhook_repo_branches.clone(),
                write_mutex: Some(Arc::clone(&state.write_mutex)),
                // Share the daemon's single job-queue connection. Opening a
                // second connection from admin routes races the worker's WAL
                // checkpoint and crashes with SIGBUS on macOS.
                job_queue: shared_job_queue_opt.clone(),
            });
            // Store the admin state so serve_ui can mount the admin API on
            // the web UI server as well (shared Arc = same state).
            let _ = state.admin_state.set(admin_state.clone());

            // Device-flow auth endpoints (RFC 8628) share the admin state so
            // approvals can hand back the configured org query token.
            let device_router = nestweaver_web::create_device_flow_router(admin_state.clone());
            mcp_router = mcp_router.nest("/auth", device_router);

            let admin_router = nestweaver_web::create_admin_router(admin_state);
            mcp_router = mcp_router.nest("/admin/api", admin_router);
            // Mount top-level /admin redirect → /admin/api/status so that
            // hitting /admin in a browser doesn't 404.
            mcp_router = mcp_router.route(
                "/admin",
                axum::routing::get(|| async {
                    axum::response::Redirect::permanent("/admin/api/status")
                }),
            );
            tracing::info!("admin API enabled at /admin/api/*");
        }

        // Mount /metrics at the top level of the MCP HTTP router so Prometheus
        // scrapers can use the standard path without knowing the /admin/api
        // prefix. S.5: on the network-facing listener the endpoint is gated
        // behind a bearer token (query or admin) — operational counters are a
        // metadata leak on a non-loopback deployment. When no auth is
        // configured (loopback-only dev bind) it stays open for local scrape
        // convenience; validate_bind_security forces --auth-token for any
        // non-loopback bind, so on the network this route is always gated.
        let metrics_auth = nestweaver_web::routes::metrics::MetricsAuthState {
            auth_token: mcp_auth_token.clone(),
            admin_token: opts.admin_token.clone(),
        };
        let metrics_route = axum::Router::new()
            .route(
                "/metrics",
                axum::routing::get(nestweaver_web::routes::metrics::metrics_authenticated),
            )
            .with_state(metrics_auth);
        mcp_router = mcp_router.merge(metrics_route);

        // Outermost safety net: convert any unhandled panic in a handler into a
        // 500 for that ONE request instead of dropping the connection (and, if the
        // panic were inside a held std::Mutex, poisoning it and wedging the pool).
        // The request surface is audited panic-free today; this guards future edits.
        mcp_router = mcp_router.layer(tower_http::catch_panic::CatchPanicLayer::new());

        // Parse the bind address to determine the MCP port.  When the gRPC
        // bind uses port 0 (OS-assigned), the MCP server also binds to port 0
        // and records the actual port in the port file (second line).
        let mcp_bind_addr: std::net::SocketAddr = opts
            .bind_addr
            .parse()
            .unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], 0)));

        // T5.1 defense-in-depth: a non-loopback MCP HTTP listener MUST be
        // authenticated. `validate_bind_security` already forces `--auth-token`
        // for a non-loopback bind (so `mcp_state` is built via `with_auth`),
        // but that guarantee is indirect. Assert it directly at bind time so a
        // future refactor that weakens the upstream check fails loudly here
        // rather than silently exposing an unauthenticated network surface.
        if !mcp_bind_addr.ip().is_loopback() && mcp_auth_token.is_none() {
            anyhow::bail!(
                "refusing to bind MCP HTTP listener to non-loopback {} without \
                 authentication — validate_bind_security should have required \
                 --auth-token; this is a bug, not a config error",
                mcp_bind_addr
            );
        }
        let mcp_bind = if mcp_bind_addr.port() == 0 {
            std::net::SocketAddr::from((mcp_bind_addr.ip(), 0))
        } else {
            std::net::SocketAddr::from((
                mcp_bind_addr.ip(),
                mcp_bind_addr.port().checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!(
                        "MCP port overflow: gRPC port {} + 1 exceeds u16::MAX",
                        mcp_bind_addr.port()
                    )
                })?,
            ))
        };

        // ACME auto-TLS (opt-in via --acme-domain) takes precedence over manual
        // --tls-cert. On success both listeners share the ACME acceptor; gRPC
        // pre-terminates TLS below since tonic's ServerTlsConfig is
        // static-Identity only and cannot pick up runtime-renewed certs.
        // Bootstrap is NON-FATAL: any ACME error logs and falls through — a
        // fatal exit under launchd KeepAlive would respawn straight into a
        // Let's Encrypt validation-failure ban.
        #[cfg(feature = "acme")]
        let acme_acceptor: Option<tokio_rustls::TlsAcceptor> = match opts.acme_domain {
            Some(ref domain) => match crate::acme::build_server_config(
                domain,
                opts.acme_email.as_deref(),
                opts.acme_staging,
                lifecycle::log_dir(&instance_id).join("acme"),
            ) {
                Ok((server_config, acme_state)) => {
                    // Drive provisioning + renewal forever in the background.
                    tokio::spawn(crate::acme::drive(acme_state));
                    tracing::info!(
                        domain,
                        staging = opts.acme_staging,
                        "ACME auto-TLS enabled (TLS-ALPN-01)"
                    );
                    eprintln!(
                        "[daemon] ACME auto-TLS enabled for {domain} (staging={})",
                        opts.acme_staging
                    );
                    Some(tokio_rustls::TlsAcceptor::from(server_config))
                }
                Err(e) => {
                    // SECURITY (B1): bootstrap failed. Never downgrade a public
                    // listener to cleartext. `build_server_config` does NO network
                    // I/O, so a failure here is a config/filesystem error, not a
                    // transient network blip (those surface later in `drive()`,
                    // which already retries with backoff and never crashes). On a
                    // non-loopback bind we serve an interim self-signed cert
                    // (encrypted; untrusted) so tokens/source stay off the wire; on
                    // loopback we may fall through to plaintext.
                    tracing::error!("ACME setup failed: {e:#}");
                    let is_loopback = bind_addr_is_loopback(&opts.bind_addr);
                    match acme_failure_fallback_acceptor(domain, is_loopback) {
                        Ok(Some(acceptor)) => {
                            tracing::warn!(
                                domain,
                                "serving an INTERIM SELF-SIGNED certificate (clients \
                                 will see a trust warning) so traffic stays encrypted; \
                                 fix the ACME error and restart to provision a trusted \
                                 cert. Traffic is NOT in cleartext."
                            );
                            eprintln!(
                                "[daemon] ACME provisioning failed; serving interim \
                                 self-signed TLS for {domain} (untrusted, encrypted). \
                                 Fix the cause and restart for a trusted cert."
                            );
                            Some(acceptor)
                        }
                        Ok(None) => {
                            // Loopback: plaintext is acceptable (process-local).
                            None
                        }
                        Err(fe) => {
                            // Fail closed: refuse to bind rather than serve cleartext.
                            anyhow::bail!(
                                "ACME provisioning failed and the self-signed TLS \
                                 fallback could not be built for a non-loopback bind \
                                 ({}); refusing to bind in cleartext: {fe:#}",
                                opts.bind_addr
                            );
                        }
                    }
                }
            },
            None => None,
        };
        #[cfg(not(feature = "acme"))]
        let acme_acceptor: Option<tokio_rustls::TlsAcceptor> = None;

        // Validate TLS config BEFORE binding any ports so we don't
        // advertise addresses that will never serve traffic. Yields
        // `(Option<tonic ServerTlsConfig>, TlsAcceptor)`: the tonic config is
        // `Some` only for the manual-cert path (gRPC uses tonic's terminator);
        // ACME leaves it `None` (gRPC pre-terminates with the shared acceptor).
        let manual_tls_config = match (&opts.tls_cert, &opts.tls_key) {
            (Some(cert_path), Some(key_path)) => {
                let _ = rustls::crypto::ring::default_provider().install_default();

                let cert_pem = std::fs::read(cert_path)
                    .with_context(|| format!("read TLS cert: {}", cert_path.display()))?;
                let key_pem = std::fs::read(key_path)
                    .with_context(|| format!("read TLS key: {}", key_path.display()))?;

                let identity =
                    tonic::transport::Identity::from_pem(cert_pem.clone(), key_pem.clone());
                let tonic_tls = tonic::transport::ServerTlsConfig::new().identity(identity);

                use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

                let certs = CertificateDer::pem_slice_iter(&cert_pem)
                    .collect::<Result<Vec<_>, _>>()
                    .context("parse TLS certificate PEM")?;
                let key =
                    PrivateKeyDer::from_pem_slice(&key_pem).context("parse TLS private key PEM")?;
                let mut server_config = rustls::ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    .context("build rustls ServerConfig")?;
                server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
                let tls_acceptor =
                    tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));

                tracing::info!("TLS enabled for TCP server and MCP HTTP");
                tracing::warn!(
                    "TLS mode: MCP HTTP rate limiter uses per-token buckets instead of per-IP \
                     because ConnectInfo is unavailable over TLS; all clients sharing the same \
                     bearer token share one rate-limit bucket. Terminate TLS at a trusted \
                     reverse proxy that sets X-Forwarded-For for per-IP granularity."
                );
                eprintln!("[daemon] TLS enabled for TCP server and MCP HTTP");
                Some((Some(tonic_tls), tls_acceptor))
            }
            (Some(_), None) | (None, Some(_)) => {
                anyhow::bail!("--tls-cert and --tls-key must both be provided for TLS");
            }
            (None, None) => None,
        };

        // ACME wins when enabled; otherwise use the manual-cert result.
        let tls_config: Option<(
            Option<tonic::transport::ServerTlsConfig>,
            tokio_rustls::TlsAcceptor,
        )> = if let Some(acme_acceptor) = acme_acceptor {
            Some((None, acme_acceptor))
        } else {
            manual_tls_config
        };

        let mcp_listener = tokio::net::TcpListener::bind(mcp_bind)
            .await
            .with_context(|| format!("bind MCP HTTP: {mcp_bind}"))?;
        let mcp_actual_addr = mcp_listener.local_addr()?;
        let mcp_tls_label = if tls_config.is_some() { " (TLS)" } else { "" };
        tracing::info!(%mcp_actual_addr, "MCP HTTP server listening{}", mcp_tls_label);
        eprintln!("[daemon] MCP HTTP server listening{mcp_tls_label} on {mcp_actual_addr}");

        // Store the MCP port — written alongside the gRPC port below.
        let mcp_port_for_file = mcp_actual_addr.port();

        let mcp_tls_acceptor = tls_config.as_ref().map(|(_, a)| a.clone());
        let mut mcp_shutdown_rx = shutdown_tx.subscribe();
        let mut mcp_accept_shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            if let Some(acceptor) = mcp_tls_acceptor {
                // B3: run each TLS handshake in its OWN task under a timeout and
                // funnel only completed handshakes through a channel, so a peer
                // that connects and never sends a ClientHello can't stall accepts
                // for every other client on this public listener. A semaphore caps
                // in-flight handshakes so a silent-client flood can't exhaust
                // resources, and the accept task exits on shutdown so it doesn't
                // leak the listener / spawn doomed handshakes.
                let (handshake_tx, mut handshake_rx) = tokio::sync::mpsc::channel::<
                    tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
                >(256);
                let handshake_sem =
                    std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_HANDSHAKES));
                tokio::spawn(async move {
                    loop {
                        let (stream, _addr) = tokio::select! {
                            _ = mcp_accept_shutdown_rx.changed() => break,
                            res = mcp_listener.accept() => match res {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::debug!("MCP TCP accept failed: {e}");
                                    continue;
                                }
                            },
                        };
                        // Backpressure: wait for a handshake permit before spawning.
                        let permit = tokio::select! {
                            _ = mcp_accept_shutdown_rx.changed() => break,
                            res = handshake_sem.clone().acquire_owned() => match res {
                                Ok(p) => p,
                                Err(_) => break,
                            },
                        };
                        let acceptor = acceptor.clone();
                        let handshake_tx = handshake_tx.clone();
                        tokio::spawn(drive_capped_handshake(
                            permit,
                            acceptor,
                            stream,
                            TLS_HANDSHAKE_TIMEOUT,
                            handshake_tx,
                            "MCP",
                        ));
                    }
                });
                let mut shutdown = Box::pin(mcp_shutdown_rx.changed());
                loop {
                    tokio::select! {
                        Some(stream) = handshake_rx.recv() => {
                            // Serving the router directly via hyper here does not
                            // populate `ConnectInfo<SocketAddr>`, so the nested
                            // device-flow `/auth` rate limiter (`auth_rate_limit_key`)
                            // can't see the direct peer IP over TLS and falls back to
                            // the proxy-supplied `X-Forwarded-For`/`X-Real-IP` (client
                            // spoofable) or a single global bucket. The limiter's key
                            // map is bounded, so a spoofed-XFF flood degrades to a
                            // global cap rather than unbounded growth — matching the
                            // MCP limiter's documented TLS fallback below. Terminate
                            // TLS at a trusted proxy that sets XFF for per-IP fidelity.
                            let svc = mcp_router.clone();
                            tokio::spawn(async move {
                                let hyper_svc = hyper_util::service::TowerToHyperService::new(svc);
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper_util::server::conn::auto::Builder::new(
                                    hyper_util::rt::TokioExecutor::new(),
                                )
                                .serve_connection(io, hyper_svc)
                                .await;
                            });
                        }
                        _ = &mut shutdown => break,
                    }
                }
            } else {
                // `into_make_service_with_connect_info` populates
                // `ConnectInfo<SocketAddr>` so the MCP rate limiter can key
                // pre-session requests (e.g. `initialize`) on the peer IP.
                // The TLS branch above serves connections directly via hyper
                // and cannot supply ConnectInfo, so over TLS the limiter falls
                // back to the bearer identity — still a stable key, just
                // coarser (residual: no per-IP granularity for TLS clients).
                axum::serve(
                    mcp_listener,
                    mcp_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = mcp_shutdown_rx.changed().await;
                })
                .await
                .ok();
            }
        });

        // TCP listener for server mode — spawned before the blocking UDS serve.
        {
            let tcp_listener = tokio::net::TcpListener::bind(&opts.bind_addr)
                .await
                .with_context(|| format!("bind TCP: {}", opts.bind_addr))?;
            let actual_addr = tcp_listener.local_addr()?;
            tracing::info!(%actual_addr, "TCP server listening");
            eprintln!("[daemon] TCP server listening on {}", actual_addr);

            if let Some(ref pf) = opts.port_file {
                // Write gRPC port on line 1, MCP HTTP port on line 2.
                let contents = format!("{}\n{}", actual_addr.port(), mcp_port_for_file);
                std::fs::write(pf, contents)?;
            }

            let tcp_stream = tokio_stream::wrappers::TcpListenerStream::new(tcp_listener);
            let mut tcp_shutdown_rx = shutdown_tx.subscribe();
            let mut grpc_accept_shutdown_rx = shutdown_tx.subscribe();

            // When an auth token is configured, wrap the TCP service with a
            // bearer-token interceptor + rate limiting. UDS stays unauthenticated.
            let interceptor = crate::auth::bearer_auth_interceptor(
                opts.auth_token.clone(),
                opts.admin_token.clone(),
                rate_limiters.clone(),
            );
            let tcp_svc =
                tonic::service::interceptor::InterceptedService::new(svc.clone(), interceptor);

            tokio::spawn(async move {
                let mut builder = tonic::transport::Server::builder();
                let shutdown = async move {
                    let _ = tcp_shutdown_rx.changed().await;
                };
                match tls_config {
                    // Manual TLS: tonic's built-in terminator over a static cert.
                    Some((Some(tonic_tls), _)) => {
                        let mut builder = match builder.tls_config(tonic_tls) {
                            Ok(b) => b,
                            Err(e) => {
                                tracing::error!(error = %e, "TLS configuration failed at serve time");
                                return;
                            }
                        };
                        let _ = builder
                            .add_service(tcp_svc)
                            .serve_with_incoming_shutdown(tcp_stream, shutdown)
                            .await;
                    }
                    // ACME: pre-terminate TLS with the shared acceptor so renewed
                    // certs are used on new handshakes (tonic's ServerTlsConfig is
                    // static-Identity only). The ACME resolver also serves the
                    // acme-tls/1 challenge; those handshakes complete then close.
                    Some((None, acme_acceptor)) => {
                        use futures::StreamExt;
                        // B3: spawn each handshake in its own task under a timeout
                        // and feed completed TLS conns to tonic via a channel, so a
                        // stalled peer can't block new gRPC connections. A semaphore
                        // caps in-flight handshakes (silent-client flood), and the
                        // accept task exits on shutdown rather than leaking.
                        let (tls_tx, tls_rx) = tokio::sync::mpsc::channel::<
                            tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
                        >(256);
                        let handshake_sem = std::sync::Arc::new(tokio::sync::Semaphore::new(
                            MAX_INFLIGHT_HANDSHAKES,
                        ));
                        tokio::spawn(async move {
                            let mut tcp = tcp_stream;
                            loop {
                                let conn = tokio::select! {
                                    _ = grpc_accept_shutdown_rx.changed() => break,
                                    c = tcp.next() => match c {
                                        Some(c) => c,
                                        None => break,
                                    },
                                };
                                let tcp_conn = match conn {
                                    Ok(c) => c,
                                    Err(e) => {
                                        tracing::debug!("gRPC TCP accept failed: {e}");
                                        continue;
                                    }
                                };
                                let permit = tokio::select! {
                                    _ = grpc_accept_shutdown_rx.changed() => break,
                                    res = handshake_sem.clone().acquire_owned() => match res {
                                        Ok(p) => p,
                                        Err(_) => break,
                                    },
                                };
                                let acme_acceptor = acme_acceptor.clone();
                                let tls_tx = tls_tx.clone();
                                tokio::spawn(drive_capped_handshake(
                                    permit,
                                    acme_acceptor,
                                    tcp_conn,
                                    TLS_HANDSHAKE_TIMEOUT,
                                    tls_tx,
                                    "gRPC",
                                ));
                            }
                        });
                        // tonic wants a stream of `Result<conn, E>`; our channel
                        // carries only successfully-handshaked streams.
                        let tls_incoming = tokio_stream::wrappers::ReceiverStream::new(tls_rx)
                            .map(Ok::<_, std::io::Error>);
                        let _ = builder
                            .add_service(tcp_svc)
                            .serve_with_incoming_shutdown(tls_incoming, shutdown)
                            .await;
                    }
                    // Plain TCP (loopback / no TLS).
                    None => {
                        let _ = builder
                            .add_service(tcp_svc)
                            .serve_with_incoming_shutdown(tcp_stream, shutdown)
                            .await;
                    }
                }
            });
        }

        // Spawn the worker pool to consume index jobs from the SQLite queue.
        // Skipped for a read-only snapshot replica — it serves reads only and
        // must never write to (or re-clone against) the snapshot.
        if !read_only {
            let worker_store = Arc::clone(&state.store);
            let worker_db = db_path.clone();
            // nw-019: the worker stamps this on every repo it indexes.
            let worker_instance = data_instance_id.clone();
            let mut worker_shutdown = shutdown_tx.subscribe();
            let worker_drained = Arc::clone(&state.drained);
            let worker_write_mutex = Arc::clone(&state.write_mutex);
            let worker_count = state
                .instance_cfg
                .as_ref()
                .map(|c| c.server.indexing.workers)
                .unwrap_or(8);
            let indexing_status = nestweaver_engine::worker::IndexingStatus::from_arcs(
                Arc::clone(&state.indexing_active),
                state.indexing_repo.clone(),
                Arc::clone(&state.indexing_queue_depth),
            );
            // Map each declared repo to its index strategy so vault repos index
            // as markdown instead of code. Keyed by the same canonical repo id
            // the worker uses for lookup.
            let worker_repo_types = state
                .instance_cfg
                .as_ref()
                .map(|c| build_repo_types(&c.repos))
                .unwrap_or_default();
            let worker_job_queue = std::sync::Arc::clone(&shared_job_queue);
            let worker_handle = tokio::spawn(async move {
                let workspace_dir = worker_db
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("workspace");
                // Recover jobs orphaned by a previous crash. At startup no worker is
                // alive, so EVERY `running` row is orphaned — reclaim them all
                // immediately instead of waiting out the ~30-min lease/threshold.
                if let Ok(guard) = worker_job_queue.lock()
                    && let Ok(recovered) = guard.recover_all_running_at_startup()
                    && recovered > 0
                {
                    tracing::info!(recovered, "recovered orphaned running jobs at startup");
                }
                let workspace =
                    match nestweaver_engine::bare_clone::BareCloneWorkspace::new(&workspace_dir) {
                        Ok(w) => w,
                        Err(e) => {
                            tracing::error!("failed to create bare clone workspace: {e}");
                            return;
                        }
                    };
                let pool = nestweaver_engine::worker::WorkerPool::new(worker_count)
                    .with_repo_types(worker_repo_types);
                pool.run_with_drain(
                    worker_job_queue,
                    std::sync::Arc::new(workspace),
                    worker_store,
                    worker_instance,
                    &mut worker_shutdown,
                    Some(indexing_status),
                    Some(worker_drained),
                    Some(worker_write_mutex),
                )
                .await;
            });
            *state
                .worker_handle
                .lock()
                .expect("worker_handle mutex poisoned") = Some(worker_handle);

            // T4.2: continuous lease reaper. The once-at-startup recover_stale
            // above only rescues jobs stale for >30min; a daemon crash seconds
            // into an index leaves a `running` row that recover_stale never
            // sees on restart, so the repo is never re-indexed. This periodic
            // tick reclaims any job whose per-claim lease has expired, closing
            // that gap. `reap_expired_leases` takes an explicit `now` so the
            // SQL is unit-tested without sleeps.
            const REAPER_INTERVAL_SECS: u64 = 60;
            let reaper_queue = std::sync::Arc::clone(&shared_job_queue);
            let mut reaper_shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_secs(REAPER_INTERVAL_SECS));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            // Two reclaim paths on the same tick: the reaper for
                            // leased rows, and recover_stale for legacy rows that
                            // predate the lease columns (NULL lease → invisible
                            // to the reaper). Running recover_stale periodically
                            // (not just once at startup) closes finding #12 for a
                            // legacy row that was younger than the threshold at
                            // startup but crosses it while the daemon runs.
                            let (reclaimed, recovered) = match reaper_queue.lock() {
                                Ok(guard) => (
                                    guard.reap_expired_leases(now),
                                    guard.recover_stale(
                                        nestweaver_engine::jobs::STALE_RECOVERY_SECS,
                                    ),
                                ),
                                Err(_) => {
                                    tracing::error!("lease reaper: job queue mutex poisoned");
                                    (Ok(0), Ok(0))
                                }
                            };
                            match reclaimed {
                                Ok(n) if n > 0 => tracing::warn!(
                                    reclaimed = n,
                                    "lease reaper reclaimed expired in-flight jobs"
                                ),
                                Ok(_) => {}
                                Err(e) => tracing::error!("lease reaper: {e}"),
                            }
                            match recovered {
                                Ok(n) if n > 0 => tracing::warn!(
                                    recovered = n,
                                    "lease reaper recovered stale legacy running jobs"
                                ),
                                Ok(_) => {}
                                Err(e) => tracing::error!("stale recovery: {e}"),
                            }
                        }
                        _ = reaper_shutdown.changed() => break,
                    }
                }
            });
        }
    } // end if server_opts

    // Spawn adaptive poll scheduler in server mode. Not for a read-only replica
    // (it would poll git remotes and enqueue index jobs against the snapshot).
    if server_opts.is_some() && !read_only {
        let poll_store = Arc::clone(&state.store);
        // nw-019: must match the worker's stamp, or list_repos/repo_uid
        // lookups here find nothing and the scheduler never polls the repos.
        let poll_instance = data_instance_id.clone();
        let poll_job_queue = shared_job_queue_opt.clone();
        let poll_cfg = state.instance_cfg.clone();
        let poll_drained = Arc::clone(&state.drained);
        let mut poll_shutdown = shutdown_tx.subscribe();
        let mut scheduler_rx = scheduler_rx; // move into the spawned task
        tokio::spawn(async move {
            use nestweaver_engine::scheduler::PollScheduler;
            use std::time::Duration;
            let indexing_cfg = poll_cfg.as_ref().map(|c| &c.server.indexing);
            let mut min_poll = indexing_cfg
                .and_then(|c| nestweaver_engine::config::parse_duration(&c.min_poll))
                .unwrap_or(Duration::from_secs(45));
            let mut max_poll = indexing_cfg
                .and_then(|c| nestweaver_engine::config::parse_duration(&c.max_poll))
                .unwrap_or(Duration::from_secs(8 * 3600));
            let mut scheduler = PollScheduler::new(min_poll, max_poll);

            // Seed from config repos first (includes unindexed repos).
            let mut seeded_urls = std::collections::HashSet::new();
            if let Some(ref cfg) = poll_cfg {
                for repo_cfg in &cfg.repos {
                    let repo_name = repo_cfg.name.clone().unwrap_or_else(|| {
                        nestweaver_engine::pull::repo_name_from_url(&repo_cfg.url)
                    });
                    let poll_override = repo_cfg.poll.as_deref().and_then(|p| match p {
                        "never" => Some(nestweaver_engine::scheduler::PollOverride::Never),
                        "manual" => Some(nestweaver_engine::scheduler::PollOverride::Manual),
                        other => nestweaver_engine::config::parse_duration(other)
                            .map(nestweaver_engine::scheduler::PollOverride::Fixed),
                    });
                    scheduler.add_repo(
                        repo_name,
                        repo_cfg.url.clone(),
                        poll_override,
                        repo_cfg.branch.clone(),
                    );
                    seeded_urls.insert(repo_cfg.url.clone());
                }
            }

            // Also seed any already-indexed repos not in the config (legacy).
            if let Ok(repos) = poll_store.list_repos(Some(&poll_instance)) {
                for repo in repos {
                    if seeded_urls.contains(&repo.url) {
                        continue;
                    }
                    let repo_name = repo
                        .name
                        .clone()
                        .unwrap_or_else(|| nestweaver_engine::pull::repo_name_from_url(&repo.url));
                    scheduler.add_repo(repo_name, repo.url.clone(), None, None);
                }
            }

            loop {
                tokio::select! {
                    _ = poll_shutdown.changed() => break,
                    cmd = scheduler_rx.recv() => {
                        if let Some(cmd) = cmd {
                            match cmd {
                                nestweaver_engine::scheduler::SchedulerCommand::AddRepo { repo_id, repo_url, poll_override, branch } => {
                                    scheduler.add_repo(repo_id, repo_url, poll_override, branch);
                                }
                                nestweaver_engine::scheduler::SchedulerCommand::RemoveRepo { repo_id } => {
                                    scheduler.remove_repo(&repo_id);
                                }
                                nestweaver_engine::scheduler::SchedulerCommand::ReloadConfig { repos, min_poll: new_min, max_poll: new_max } => {
                                    if let Some(m) = new_min { min_poll = m; }
                                    if let Some(m) = new_max { max_poll = m; }
                                    scheduler = PollScheduler::new(min_poll, max_poll);
                                    for (id, url, ovr, branch) in repos {
                                        scheduler.add_repo(id, url, ovr, branch);
                                    }
                                    tracing::info!(count = scheduler.repo_count(), "scheduler reloaded from config");
                                }
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {
                        // Skip polling when drained.
                        if poll_drained.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        let due = scheduler.due_repos();
                        for (repo_id, repo_url, branch) in due {
                            // Determine which branch ref to check. If the repo
                            // config specifies a branch, use that ref; otherwise
                            // fall back to HEAD (symref of the remote's default
                            // branch) so we aren't hardcoded to "main".
                            let url = repo_url.clone();
                            let ref_spec = branch.as_deref().unwrap_or("HEAD").to_string();
                            // SSRF guard the remote probe too: reject internal targets
                            // and pin the resolved IP (closes a DNS-rebinding vector on
                            // the ls-remote ref probe, mirroring clone/fetch).
                            let ls_guard = match nestweaver_engine::ssrf::guard_git_url(&url) {
                                Ok(g) => g,
                                Err(e) => {
                                    tracing::warn!(url = %url, error = %e, "skipping poll: repo URL failed SSRF guard");
                                    continue;
                                }
                            };
                            // Run the ls-remote probe OFF the async runtime via
                            // spawn_blocking, and bounded by GIT_TIMEOUT (which
                            // kills+reaps the child): a hung remote no longer
                            // stalls this scheduler task's runtime, and can never
                            // block past the timeout. The status check inside
                            // `probe_remote_sha` distinguishes a genuine failure
                            // (Err → log+skip) from an unadvertised ref
                            // (Ok(None) → nothing to enqueue).
                            let probe_args = ls_guard.config_args.clone();
                            let probe_url = url.clone();
                            let probe_ref = ref_spec.clone();
                            let probe = tokio::task::spawn_blocking(move || {
                                probe_remote_sha(&probe_args, &probe_url, &probe_ref)
                            })
                            .await;
                            let remote_sha = match probe {
                                Ok(Ok(Some(sha))) => sha,
                                Ok(Ok(None)) => continue,
                                Ok(Err(e)) => {
                                    tracing::warn!(url = %url, error = %e, "poll ls-remote failed");
                                    continue;
                                }
                                Err(e) => {
                                    tracing::warn!(url = %url, error = %e, "poll ls-remote task panicked");
                                    continue;
                                }
                            };
                            // A completed ls-remote probe.
                            nestweaver_web::routes::metrics::POLL_CHECKS.inc();
                            let r_uid = nestweaver_schema::repo_uid(&poll_instance, &url);
                            let indexed_sha = poll_store.lookup_repo(&r_uid)
                                .ok().flatten().map(|r| r.indexed_sha).unwrap_or_default();
                            if remote_sha != indexed_sha {
                                // A new commit was observed on the remote.
                                nestweaver_web::routes::metrics::POLL_CHANGES_DETECTED.inc();
                            }
                            if remote_sha != indexed_sha
                                && let Some(ref jq) = poll_job_queue
                                && let Ok(queue) = jq.lock()
                            {
                                let canonical_id =
                                    nestweaver_engine::jobs::canonical_repo_id(&url);
                                let _ = queue.upsert(
                                    &canonical_id,
                                    &url,
                                    nestweaver_engine::jobs::JobTrigger::Poll,
                                    branch.as_deref(),
                                );
                                // A new commit was just observed — record it so
                                // the scheduler's adaptive interval shortens for
                                // active repos. Only on new-commit detection;
                                // calling this every poll would peg the interval
                                // at the min_poll floor.
                                scheduler.update_commit_time(
                                    &repo_id,
                                    std::time::Instant::now(),
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    // Spawn a periodic metrics refresh task that updates gauge-type metrics
    // (queue depth, repo count, MCP sessions) from live state. Counter-type
    // metrics (gRPC requests, webhooks, jobs) are incremented at their call sites.
    {
        let metrics_store = Arc::clone(&state.store);
        let metrics_queue_depth = Arc::clone(&state.indexing_queue_depth);
        let metrics_active_reads = Arc::clone(&state.active_reads);
        let metrics_active_writes = Arc::clone(&state.active_writes);
        let metrics_job_queue = shared_job_queue_opt.clone();
        // nw-019: must match the worker's stamp, or the repo-count gauge
        // filters on the wrong instance and always reads zero.
        let metrics_instance = data_instance_id.clone();
        let metrics_mcp_sessions = mcp_session_gauge_opt.clone();
        let mut metrics_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            use nestweaver_web::routes::metrics;
            let mut last_metric_completed_at = 0_i64;
            loop {
                // Update repo gauge.
                if let Ok(repos) = metrics_store.list_repos(Some(&metrics_instance)) {
                    metrics::REPOS_TOTAL
                        .with_label_values(&["indexed"])
                        .set(repos.len() as i64);
                }

                // Update queue depth gauge.
                let depth = metrics_job_queue
                    .as_ref()
                    .and_then(|queue| {
                        let guard = queue.lock().ok()?;
                        let depth = guard.queue_depth().ok()?;
                        Some(depth.pending + depth.running)
                    })
                    .unwrap_or_else(|| metrics_queue_depth.load(Ordering::Relaxed) as i64);
                metrics::QUEUE_DEPTH
                    .with_label_values(&["total"])
                    .set(depth);
                metrics::ACTIVE_READS.set(metrics_active_reads.load(Ordering::Relaxed) as i64);
                metrics::ACTIVE_WRITES.set(metrics_active_writes.load(Ordering::Relaxed) as i64);
                metrics::GRPC_CONNECTIONS.set(
                    (metrics_active_reads.load(Ordering::Relaxed)
                        + metrics_active_writes.load(Ordering::Relaxed)) as i64,
                );
                if let Some(ref sessions) = metrics_mcp_sessions {
                    metrics::MCP_SESSIONS.set(sessions.load(Ordering::Relaxed) as i64);
                }

                if let Some(queue) = &metrics_job_queue
                    && let Ok(guard) = queue.lock()
                    && let Ok(completed) =
                        guard.completed_job_metrics_after(last_metric_completed_at)
                {
                    for job in completed {
                        last_metric_completed_at = last_metric_completed_at.max(job.completed_at);
                        let result = match job.status {
                            nestweaver_engine::jobs::JobStatus::Succeeded => "succeeded",
                            nestweaver_engine::jobs::JobStatus::DeadLetter => "dead_letter",
                            nestweaver_engine::jobs::JobStatus::Cancelled => "cancelled",
                            _ => "failed",
                        };
                        metrics::JOBS_TOTAL.with_label_values(&[result]).inc();
                        metrics::JOB_DURATION
                            .with_label_values(&[] as &[&str])
                            .observe(job.duration_s);
                    }
                }

                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {}
                    _ = metrics_shutdown.changed() => break,
                }
            }
        });
    }

    // Set process title for easier identification via pgrep.
    set_process_title(&format!("nestweaver-daemon-{instance_id}"));

    // Wrap the UDS service with an interceptor that grants admin access.
    // Local socket connections are implicitly trusted — the OS enforces
    // file-system permissions on the socket, so any process that can connect
    // is on the same machine and running as the same user.
    let uds_svc = tonic::service::interceptor::InterceptedService::new(
        svc,
        crate::auth::uds_admin_interceptor,
    );

    // Spawn the UDS gRPC server so the socket binds and accepts immediately, THEN load the
    // embedding model on this (main) thread concurrently — non-semantic RPCs are served during
    // the load, and semantic search returns "model not loaded" until it completes. candle needs
    // the main thread for Metal, and this keeps cache resolution and model construction from
    // delaying the bind.
    let uds_serve = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(uds_svc)
            .serve_with_incoming_shutdown(uds_stream, async move {
                let _ = shutdown_rx.changed().await;
            }),
    );

    // Keep shutdown and model loading in the same control point. If shutdown wins before the
    // synchronous model-construction phase begins, abandon the load and proceed to drain.
    //
    // `not(test)`: the load asserts it runs on the PROCESS main thread
    // (`pthread_main_np`, Metal shader compilation) — a guarantee the test
    // harness can never provide (unit tests drive `run_server` from a tokio
    // worker thread). Under `cargo test --workspace`, feature unification
    // compiles this crate's unit tests WITH `embed` enabled (the root bin's
    // default features), so without this gate an in-process `run_server`
    // under test panics on that assert. Skipping the load in unit-test
    // builds only matches the feature-off behavior those tests already get
    // from `cargo test -p nestweaver-daemon`.
    #[cfg(all(feature = "embed", not(test)))]
    {
        let mut load_shutdown = shutdown_tx.subscribe();
        tokio::select! {
            _ = load_embedding_model(&state) => {}
            _ = load_shutdown.changed() => {
                tracing::info!("shutdown requested during embedding model load — abandoning load");
            }
        }
    }

    uds_serve
        .await
        .context("UDS serve task panicked")?
        .context("gRPC server error")?;

    // Cleanup — runs on graceful shutdown (not skipped like process::exit would).
    tracing::info!("daemon shutting down, cleaning up");

    // Belt-and-suspenders watcher stop covering shutdown triggers that
    // don't pass through the gRPC Shutdown handler or the SIGTERM handler
    // (e.g. idle timeout). Idempotent — no-op when already stopped.
    stop_active_watcher(&state);

    // Await the worker pool so an in-flight index write finishes before we exit.
    // The worker loop sees the same shutdown signal, breaks, and drains its
    // per-job tasks; awaiting its handle blocks until that drain completes.
    // `spawn_blocking` work cannot be aborted, so this is the only way to avoid
    // tearing down the runtime mid-write.
    let worker_handle = state.worker_handle.lock().ok().and_then(|mut g| g.take());
    if let Some(handle) = worker_handle {
        tracing::info!("draining worker pool before exit");
        let _ = handle.await;
    }

    let _ = std::fs::remove_file(&sock_path);
    // Drop the instance lock's pidfile on clean shutdown (the flock itself is
    // released when `_pid_guard` drops at end of scope).
    let _ = std::fs::remove_file(lifecycle::pidfile_path(&instance_id));

    Ok(())
}

// ── FlowTraceContinue implementation ────────────────────────────────────

/// Server-side flow trace continuation: given an entry symbol's canonical_id,
/// walk the call graph forward up to `remaining_depth`, skipping visited
/// symbols. Returns trace spans + boundary symbols (call targets not in
/// this database).
fn flow_trace_continue_impl(
    store: &nestweaver_store::GraphStore,
    req: FlowTraceContinueRequest,
) -> Result<FlowTraceContinueResponse, Status> {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering as AtomOrd};

    // Counter for generating unique span IDs within this request.
    static SPAN_COUNTER: AtomicU64 = AtomicU64::new(0);
    let make_span_id = || {
        let n = SPAN_COUNTER.fetch_add(1, AtomOrd::Relaxed);
        format!("srv-{:016x}", n)
    };

    // Hard-cap depth: a peer daemon could send a huge remaining_depth, and the recursive
    // walk_trace below would otherwise overflow the stack. This typed RPC bypasses the
    // JSON-dispatch depth clamp, so it needs its own bound.
    let max_depth = (req.remaining_depth.max(0) as usize).min(64);
    let trace_id = req.trace_id.clone();

    // Build the visited set from the request.
    let visited: HashSet<String> = req.visited_canonical_ids.into_iter().collect();

    // Look up the entry symbol by canonical_id.
    let entry = store
        .symbol_by_canonical_id(&req.entry_canonical_id)
        .map_err(|e| Status::internal(format!("canonical_id lookup failed: {e}")))?;

    let Some(entry) = entry else {
        // Entry symbol not found in this database — it's a boundary from
        // this server's perspective. Return empty response.
        return Ok(FlowTraceContinueResponse {
            spans: vec![],
            boundaries: vec![BoundarySymbolProto {
                canonical_id: req.entry_canonical_id.clone(),
                name: String::new(),
                parent_span_id: req.parent_span_id.clone(),
            }],
            truncated: false,
        });
    };

    // Get the repo URL for source annotation.
    let repo_url = store.repo_url_for_uid(&entry.repo_uid).unwrap_or_default();

    // Recursive trace builder.
    struct TraceCtx<'a> {
        store: &'a nestweaver_store::GraphStore,
        trace_id: String,
        repo_url: String,
        visited: HashSet<String>,
        spans: Vec<TraceSpanProto>,
        boundaries: Vec<BoundarySymbolProto>,
        truncated: bool,
        make_span_id: Box<dyn Fn() -> String>,
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_trace(
        ctx: &mut TraceCtx<'_>,
        uid: &str,
        canonical_id: &str,
        name: &str,
        file_path: &str,
        start_line: u32,
        repo_url: &str,
        parent_span_id: Option<&str>,
        depth: usize,
        max_depth: usize,
    ) -> String {
        let span_id = (ctx.make_span_id)();

        // Mark this canonical_id as visited.
        ctx.visited.insert(canonical_id.to_string());

        let mut callee_span_ids = Vec::new();

        if depth < max_depth {
            // On a DB read error, mark the trace truncated (incomplete) rather than
            // silently pruning this branch as if it had no callees.
            let callees = match ctx.store.callees_of(uid) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("walk_trace: callees_of failed: {e}");
                    ctx.truncated = true;
                    Vec::new()
                }
            };
            for callee in &callees {
                let callee_cid = callee.canonical_id.as_deref().unwrap_or("");

                // Skip visited symbols (cycle prevention).
                if !callee_cid.is_empty() && ctx.visited.contains(callee_cid) {
                    continue;
                }

                if callee_cid.is_empty() {
                    // Symbol without canonical_id — skip as boundary.
                    continue;
                }

                // Resolve the callee's repo URL from its repo_uid. If
                // the callee belongs to a different repo than the entry
                // symbol, this ensures the span carries the correct URL.
                let callee_repo_url = ctx
                    .store
                    .repo_url_for_uid(&callee.repo_uid)
                    .unwrap_or_else(|| ctx.repo_url.clone());

                let child_span_id = walk_trace(
                    ctx,
                    &callee.uid,
                    callee_cid,
                    &callee.name,
                    &callee.file_path,
                    callee.start_line,
                    &callee_repo_url,
                    Some(&span_id),
                    depth + 1,
                    max_depth,
                );
                callee_span_ids.push(child_span_id);
            }
        } else if depth >= max_depth {
            // Check if there are callees we didn't follow due to depth limit.
            // On a DB read error, mark the trace truncated (incomplete) rather than
            // silently pruning this branch as if it had no callees.
            let callees = match ctx.store.callees_of(uid) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("walk_trace: callees_of failed: {e}");
                    ctx.truncated = true;
                    Vec::new()
                }
            };
            if !callees.is_empty() {
                ctx.truncated = true;
            }
        }

        ctx.spans.push(TraceSpanProto {
            trace_id: ctx.trace_id.clone(),
            span_id: span_id.clone(),
            parent_span_id: parent_span_id.map(String::from),
            canonical_id: canonical_id.to_string(),
            name: name.to_string(),
            repo_url: repo_url.to_string(),
            file_path: file_path.to_string(),
            start_line: start_line as i32,
            callee_span_ids,
            source: "server".to_string(),
        });

        span_id
    }

    let entry_cid = entry
        .canonical_id
        .as_deref()
        .unwrap_or(&req.entry_canonical_id);

    let mut ctx = TraceCtx {
        store,
        trace_id: trace_id.clone(),
        repo_url: repo_url.clone(),
        visited,
        spans: Vec::new(),
        boundaries: Vec::new(),
        truncated: false,
        make_span_id: Box::new(make_span_id),
    };

    walk_trace(
        &mut ctx,
        &entry.uid,
        entry_cid,
        &entry.name,
        &entry.file_path,
        entry.start_line,
        &repo_url,
        if req.parent_span_id.is_empty() {
            None
        } else {
            Some(&req.parent_span_id)
        },
        0,
        max_depth,
    );

    Ok(FlowTraceContinueResponse {
        spans: ctx.spans,
        boundaries: ctx.boundaries,
        truncated: ctx.truncated,
    })
}

// ── Process title helper ────────────────────────────────────────────────

#[cfg(all(
    any(
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ),
    not(target_os = "macos")
))]
unsafe extern "C" {
    fn setproctitle(fmt: *const libc::c_char, ...);
}

/// Set the process title for easier identification via pgrep.
/// Works on Linux (via prctl) and BSD systems (via setproctitle).
/// On macOS and other unsupported platforms, this is a no-op.
fn set_process_title(title: &str) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(c_title) = std::ffi::CString::new(title) {
            unsafe {
                // PR_SET_NAME is limited to 15 bytes + NUL on Linux
                let _ = libc::prctl(libc::PR_SET_NAME, c_title.as_ptr() as libc::c_long, 0, 0, 0);
            }
        }
    }

    #[cfg(all(
        any(
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ),
        not(target_os = "macos")
    ))]
    {
        if let Ok(c_title) = std::ffi::CString::new(title) {
            unsafe {
                // setproctitle accepts a format string and arguments like printf
                setproctitle(b"-%s\0".as_ptr() as *const libc::c_char, c_title.as_ptr());
            }
        }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        let _ = title; // Suppress unused warning on unsupported platforms (macOS, etc.)
        // Note: macOS doesn't provide setproctitle in the C library, so the daemon
        // process title cannot be modified on macOS. Users can identify the daemon
        // via pgrep using the socket path instead.
    }
}

// ── ImpactAnalysis implementation ───────────────────────────────────────

/// Server-side impact analysis: converts proto atomic changes to engine types,
/// runs graph queries for affected symbols, and returns classified impacts.
fn impact_analysis_impl(
    store: &nestweaver_store::GraphStore,
    req: ImpactAnalysisRequest,
) -> Result<ImpactAnalysisResponse, Status> {
    use nestweaver_engine::atomic_changes::{AtomicChange, ImpactSeverity, analyze_impact};

    // Convert proto AtomicChangeProto -> engine AtomicChange
    let changes: Vec<AtomicChange> = req
        .changes
        .iter()
        .filter_map(proto_to_atomic_change)
        .collect();

    if changes.is_empty() {
        return Ok(ImpactAnalysisResponse {
            impacts: vec![],
            total_impacted_files: 0,
            total_impacted_repos: 0,
            impacted_repo_urls: vec![],
        });
    }

    let max_depth = if req.max_depth > 0 {
        req.max_depth as u32
    } else {
        3 // default
    };

    let results = analyze_impact(store, &changes, max_depth, req.include_tests)
        .map_err(|e| Status::internal(format!("impact analysis failed: {e}")))?;

    // Collect unique impacted files and repos
    let mut impacted_files = std::collections::HashSet::new();
    let mut impacted_repos = std::collections::HashSet::new();

    let impacts: Vec<ImpactItem> = results
        .iter()
        .map(|r| {
            impacted_files.insert(r.affected_file.clone());
            if !r.affected_repo_url.is_empty() {
                impacted_repos.insert(r.affected_repo_url.clone());
            }

            let severity = match r.severity {
                ImpactSeverity::Breaking => Severity::Breaking,
                ImpactSeverity::Warning => Severity::Warning,
                ImpactSeverity::Info => Severity::Info,
            };

            let change_kind = match r.change_kind.as_str() {
                "SYMBOL_REMOVED" => ChangeKind::SymbolRemoved,
                "SIGNATURE_CHANGED" => ChangeKind::SignatureChanged,
                "SYMBOL_RENAMED" => ChangeKind::SymbolRenamed,
                "SYMBOL_MOVED" => ChangeKind::SymbolMoved,
                "EXPORT_REMOVED" => ChangeKind::ExportRemoved,
                "EXPORT_ADDED" => ChangeKind::ExportAdded,
                "SYMBOL_ADDED" => ChangeKind::SymbolAdded,
                _ => ChangeKind::Unspecified,
            };

            ImpactItem {
                change_canonical_id: r.change_canonical_id.clone(),
                change_kind: change_kind.into(),
                affected_canonical_id: r.affected_canonical_id.clone(),
                affected_name: r.affected_name.clone(),
                affected_repo_url: r.affected_repo_url.clone(),
                affected_file: r.affected_file.clone(),
                affected_line: r.affected_line as i32,
                affected_signature: r.affected_signature.clone(),
                severity: severity.into(),
                reason: r.reason.clone(),
            }
        })
        .collect();

    let impacted_repo_urls: Vec<String> = impacted_repos.into_iter().collect();

    Ok(ImpactAnalysisResponse {
        impacts,
        total_impacted_files: impacted_files.len() as i32,
        total_impacted_repos: impacted_repo_urls.len() as i32,
        impacted_repo_urls,
    })
}

/// Convert a proto AtomicChangeProto to an engine AtomicChange.
fn proto_to_atomic_change(
    proto: &AtomicChangeProto,
) -> Option<nestweaver_engine::atomic_changes::AtomicChange> {
    use nestweaver_engine::atomic_changes::AtomicChange;
    use nestweaver_schema::SymbolKind;

    let kind = ChangeKind::try_from(proto.kind).unwrap_or(ChangeKind::Unspecified);
    let parse_kind = |s: &str| -> SymbolKind {
        match s {
            "Function" => SymbolKind::Function,
            "Class" => SymbolKind::Class,
            "Method" => SymbolKind::Method,
            "Interface" => SymbolKind::Interface,
            "Trait" => SymbolKind::Trait,
            "Enum" => SymbolKind::Enum,
            "Module" => SymbolKind::Module,
            _ => SymbolKind::Function,
        }
    };

    match kind {
        ChangeKind::SymbolAdded => Some(AtomicChange::SymbolAdded {
            name: proto.name.clone(),
            kind: parse_kind(&proto.symbol_kind),
            signature: proto.new_signature.clone().unwrap_or_default(),
            file_path: proto.file_path.clone(),
        }),
        ChangeKind::SymbolRemoved => Some(AtomicChange::SymbolRemoved {
            canonical_id: proto.canonical_id.clone(),
            name: proto.name.clone(),
            kind: parse_kind(&proto.symbol_kind),
            file_path: proto.file_path.clone(),
        }),
        ChangeKind::SignatureChanged => Some(AtomicChange::SignatureChanged {
            canonical_id: proto.canonical_id.clone(),
            name: proto.name.clone(),
            old_signature: proto.old_signature.clone().unwrap_or_default(),
            new_signature: proto.new_signature.clone().unwrap_or_default(),
            file_path: proto.file_path.clone(),
        }),
        ChangeKind::SymbolRenamed => Some(AtomicChange::SymbolRenamed {
            old_canonical_id: proto.canonical_id.clone(),
            old_name: proto.old_name.clone().unwrap_or_default(),
            new_name: proto.new_name.clone().unwrap_or_default(),
            new_canonical_id: String::new(), // Computed client-side
            file_path: proto.file_path.clone(),
        }),
        ChangeKind::SymbolMoved => Some(AtomicChange::SymbolMoved {
            canonical_id: proto.canonical_id.clone(),
            name: proto.name.clone(),
            old_file: proto.old_file.clone().unwrap_or_default(),
            new_file: proto.new_file.clone().unwrap_or_default(),
        }),
        ChangeKind::ExportAdded => Some(AtomicChange::ExportAdded {
            canonical_id: proto.canonical_id.clone(),
            name: proto.name.clone(),
            file_path: proto.file_path.clone(),
        }),
        ChangeKind::ExportRemoved => Some(AtomicChange::ExportRemoved {
            canonical_id: proto.canonical_id.clone(),
            name: proto.name.clone(),
            file_path: proto.file_path.clone(),
        }),
        ChangeKind::Unspecified => None,
    }
}

/// Probe a remote's ref SHA via `git ls-remote`, with a hard timeout.
///
/// This is the blocking body run off the async runtime by the poll scheduler.
/// It distinguishes three outcomes so a genuine failure is never mistaken for
/// "no new commit":
/// - `Ok(Some(sha))` — the remote advertises the ref.
/// - `Ok(None)` — ls-remote succeeded but advertised nothing for `ref_spec`
///   (e.g. an unknown branch); there is simply nothing to enqueue.
/// - `Err(_)` — git exited non-zero or timed out (unreachable/blackholed remote,
///   auth failure, bad URL). The caller logs and skips rather than treating the
///   empty stdout as an up-to-date signal.
fn probe_remote_sha(
    config_args: &[String],
    url: &str,
    ref_spec: &str,
) -> anyhow::Result<Option<String>> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(config_args);
    cmd.args(["ls-remote", url, ref_spec]);
    let output = nestweaver_engine::git_cmd::run_git_with_timeout(
        cmd,
        nestweaver_engine::git_cmd::git_net_timeout(),
    )?;
    if !output.status.success() {
        anyhow::bail!(
            "git ls-remote failed for {url} ({ref_spec}): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let sha = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(if sha.is_empty() { None } else { Some(sha) })
}

#[cfg(test)]
mod startup_helper_tests {
    use super::*;
    use nestweaver_engine::{RepoConfig, RepoType};

    /// Helper: init a git repo with one commit and return its path.
    fn init_repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@t.com"],
            vec!["config", "user.name", "t"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .unwrap();
        }
        std::fs::write(dir.join("a.txt"), "hi").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "init"]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .unwrap();
        }
    }

    fn test_repo(uid: &str, url: &str, root_path: Option<&str>) -> nestweaver_schema::Repo {
        nestweaver_schema::Repo {
            uid: uid.to_string(),
            url: url.to_string(),
            indexed_sha: "abc".to_string(),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: None,
            root_path: root_path.map(String::from),
        }
    }

    fn seed_manifest_and_embedding(
        state: &DaemonState,
        repo_uid: &str,
        package_name: &str,
    ) -> String {
        let manifests_path = nestweaver_engine::sidecar_path(&state.db_path, ".manifests.json");
        let mut manifests =
            nestweaver_engine::load_manifest_cache(&manifests_path).unwrap_or_default();
        manifests.insert(
            repo_uid.to_string(),
            nestweaver_engine::ManifestInfo {
                package_name: Some(package_name.to_string()),
                dependencies: Vec::new(),
                entry_files: Vec::new(),
            },
        );
        nestweaver_engine::save_manifest_cache(&manifests, &manifests_path).unwrap();

        let symbol_uid = format!("sym:{repo_uid}:{package_name}");
        state
            .store
            .insert_symbol(&nestweaver_schema::Symbol {
                uid: symbol_uid.clone(),
                name: package_name.to_string(),
                kind: nestweaver_schema::SymbolKind::Function,
                repo_uid: repo_uid.to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: format!("fn {package_name}()"),
                summary: None,
                content_hash: format!("hash:{package_name}"),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: nestweaver_schema::Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();
        assert!(state.store.add_embedding(&symbol_uid, vec![1.0, 0.0]));
        state.store.flush_embedding_index().unwrap();
        symbol_uid
    }

    fn seed_project(state: &DaemonState, uid: &str, name: &str) {
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: uid.to_string(),
                name: name.to_string(),
                summary: None,
                instance_id: "test".to_string(),
            })
            .unwrap();
    }

    fn seed_extension(state: &DaemonState, uid: &str, key: &str, value: serde_json::Value) {
        let mut extensions = nestweaver_engine::load_extensions(&state.db_path);
        nestweaver_engine::set_property(&mut extensions, uid, key, value);
        nestweaver_engine::save_extensions(&state.db_path, &extensions).unwrap();
    }

    fn project_delete_error(
        uid: &str,
        name: Option<&str>,
        disposition: nestweaver_store::ProjectMutationDisposition,
        message: &str,
    ) -> nestweaver_store::DeleteProjectCascadeError {
        nestweaver_store::DeleteProjectCascadeError {
            project_uid: uid.to_string(),
            project_name: name.map(str::to_string),
            disposition,
            primary: nestweaver_store::StoreError::Query(message.to_string()),
            rollback: None,
        }
    }

    fn seed_vault_note_heading_embeddings(
        state: &DaemonState,
        vault_uid: &str,
        instance_id: &str,
        root_path: &str,
    ) -> (String, String) {
        use nestweaver_schema::{Heading, Note, NoteKind, Vault};

        state
            .store
            .insert_vault(&Vault {
                uid: vault_uid.to_string(),
                name: format!("vault-{vault_uid}"),
                root_path: root_path.to_string(),
                instance_id: instance_id.to_string(),
            })
            .unwrap();
        let note_uid = format!("note:{vault_uid}");
        state
            .store
            .insert_note(&Note {
                uid: note_uid.clone(),
                vault_uid: vault_uid.to_string(),
                file_path: "note.md".to_string(),
                title: format!("Note {vault_uid}"),
                note_kind: NoteKind::General,
                word_count: 3,
                content_hash: format!("hash:{note_uid}"),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        state
            .store
            .insert_vault_note_edge(vault_uid, &note_uid)
            .unwrap();
        let heading_uid = format!("head:{vault_uid}");
        state
            .store
            .insert_heading(&Heading {
                uid: heading_uid.clone(),
                note_uid: note_uid.clone(),
                level: 1,
                text: "Heading".to_string(),
                slug: "heading".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: format!("hash:{heading_uid}"),
                embedding: None,
            })
            .unwrap();
        state
            .store
            .batch_insert_note_heading_edges(&[(&note_uid, &heading_uid)])
            .unwrap();
        assert!(state.store.add_embedding(&note_uid, vec![1.0, 0.0]));
        assert!(state.store.add_embedding(&heading_uid, vec![0.0, 1.0]));
        state.store.flush_embedding_index().unwrap();
        (note_uid, heading_uid)
    }

    fn assert_embeddings_absent(state: &DaemonState, uids: &[&str]) {
        for uid in uids {
            assert!(
                !state.store.has_embedding(uid),
                "stale embedding survived for {uid}"
            );
        }
    }

    #[tokio::test]
    async fn remove_vault_prunes_note_and_heading_embeddings() {
        let state = test_state_with_writer();
        let (note_uid, heading_uid) = seed_vault_note_heading_embeddings(
            &state,
            "vlt:remove:docs",
            "remove",
            "/missing/remove-docs",
        );
        let service = DaemonService::new(state.clone());
        let mut request = Request::new(RemoveVaultRequest {
            vault_uid: "vlt:remove:docs".to_string(),
        });
        request.extensions_mut().insert(crate::auth::IsAdmin(true));

        service.remove_vault(request).await.unwrap();

        assert_embeddings_absent(&state, &[&note_uid, &heading_uid]);
    }

    #[test]
    fn remove_vault_targeted_extension_cleanup_preserves_unrelated_note_keys() {
        use nestweaver_schema::{Note, NoteKind, Vault};

        let state = test_state_with_writer();
        let vault_uid = "vlt:remove:scoped";
        let note_uid = format!("note:{vault_uid}:dead");
        state
            .store
            .insert_vault(&Vault {
                uid: vault_uid.to_string(),
                name: "scoped".to_string(),
                root_path: "/missing/scoped-vault".to_string(),
                instance_id: "remove".to_string(),
            })
            .unwrap();
        state
            .store
            .insert_note(&Note {
                uid: note_uid.clone(),
                vault_uid: vault_uid.to_string(),
                file_path: "dead.md".to_string(),
                title: "Dead".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "dead".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        state
            .store
            .insert_vault_note_edge(vault_uid, &note_uid)
            .unwrap();

        let unrelated = "note:vlt:unrelated:aaaaaaaaaaaa";
        let mut extensions = nestweaver_engine::load_extensions(&state.db_path);
        nestweaver_engine::set_property(
            &mut extensions,
            &note_uid,
            "owner",
            serde_json::json!("dead"),
        );
        nestweaver_engine::set_property(
            &mut extensions,
            unrelated,
            "owner",
            serde_json::json!("keep"),
        );
        nestweaver_engine::save_extensions(&state.db_path, &extensions).unwrap();

        run_remove_vault_with_projection(&state, vault_uid, None).unwrap();

        let reopened = nestweaver_engine::load_extensions(&state.db_path);
        assert!(!reopened.contains_key(&note_uid));
        assert!(reopened.contains_key(unrelated));
    }

    #[test]
    fn nonexistent_vault_skips_all_reconciliation_after_unknown_preflight() {
        let state = test_state_with_writer();
        let tantivy = state.tantivy.as_ref().unwrap();
        tantivy
            .update_note(
                "note:nonexistent-vault-noop",
                "nonexistent_vault_noop_sentinel",
                "vlt:nonexistent-vault-noop",
                &["nonexistent_vault_noop_sentinel".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        assert!(
            state
                .store
                .add_embedding("note:nonexistent-vault-noop", vec![1.0, 0.0])
        );
        state.store.flush_embedding_index().unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"nonexistent-vault-noop":0.5}"#).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = state.store.graph_generation();

        let response = run_remove_vault_with_projection(
            &state,
            "vlt:does-not-exist",
            Some(Err(anyhow::anyhow!(
                "deterministic preflight projection failure"
            ))),
        )
        .unwrap();

        assert_eq!(response.notes_deleted, 0);
        // nw-091 / Bug 2 boundary: a confirmed no-op stays distinguishable from a
        // committed delete — committed:false and no reconciliation warnings.
        assert!(
            !response.committed,
            "a confirmed no-op did not commit anything"
        );
        assert!(response.reconciliation_failures.is_empty());
        assert_eq!(state.store.graph_generation(), generation_before);
        assert!(pagerank_path.exists());
        assert!(state.store.has_embedding("note:nonexistent-vault-noop"));
        assert!(
            !tantivy
                .search("nonexistent_vault_noop_sentinel", 10)
                .unwrap()
                .is_empty(),
            "confirmed no-op must not rebuild available search"
        );

        let mut unavailable = test_state_with_writer();
        Arc::get_mut(&mut unavailable)
            .unwrap()
            .search_reconciliation =
            SearchIndexReconciliation::Unavailable("configured search unavailable".to_string());
        let generation_before = unavailable.store.graph_generation();
        let response = run_remove_vault_with_projection(
            &unavailable,
            "vlt:still-does-not-exist",
            Some(Err(anyhow::anyhow!(
                "deterministic preflight projection failure"
            ))),
        )
        .unwrap();
        assert_eq!(response.notes_deleted, 0);
        assert_eq!(unavailable.store.graph_generation(), generation_before);
    }

    #[tokio::test]
    async fn typed_remove_vault_surfaces_embedding_persistence_failure() {
        let state = test_state_with_writer();
        let (note_uid, heading_uid) = seed_vault_note_heading_embeddings(
            &state,
            "vlt:remove:embedding-failure",
            "remove-embedding-failure",
            "/missing/remove-embedding-failure",
        );
        let embedding_path = state.store.embedding_sidecar_path().unwrap();
        std::fs::remove_file(&embedding_path).unwrap();
        std::fs::create_dir(&embedding_path).unwrap();
        let tantivy = state.tantivy.as_ref().unwrap();
        tantivy
            .update_note(
                "note:stale-vault-reconciliation",
                "stale_vault_reconciliation_sentinel",
                "vlt:remove:embedding-failure",
                &["stale_vault_reconciliation_sentinel".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        let generation_before = state.store.graph_generation();
        let service = DaemonService::new(state.clone());
        let mut request = Request::new(RemoveVaultRequest {
            vault_uid: "vlt:remove:embedding-failure".to_string(),
        });
        request.extensions_mut().insert(crate::auth::IsAdmin(true));

        // nw-091 / Bug 2: a post-commit reconciliation failure on an ALREADY-
        // COMMITTED delete must be reported as success-with-warnings, NOT a bare
        // error. The prior contract (unwrap_err / Code::Internal) is exactly what
        // made a user believe a committed remove_vault had not happened and take
        // corrective action against already-deleted data (752 → 1 notes).
        let response = service.remove_vault(request).await.unwrap().into_inner();

        assert!(response.committed, "the vault delete durably committed");
        assert!(
            response
                .reconciliation_failures
                .iter()
                .any(|f| f.stage.contains("embedding")),
            "the embedding-index reconciliation failure must surface as a warning, got: {:?}",
            response.reconciliation_failures
        );
        // The mutation genuinely committed (this is what makes the honest
        // "committed + warnings" response correct, not a lie).
        assert_embeddings_absent(&state, &[&note_uid, &heading_uid]);
        assert!(state.store.graph_generation() > generation_before);
        assert!(
            tantivy
                .search("stale_vault_reconciliation_sentinel", 10)
                .unwrap()
                .is_empty(),
            "search rebuild must run after embedding persistence failure"
        );
    }

    #[test]
    fn prune_stale_vault_prunes_note_and_heading_embeddings() {
        let state = test_state_with_writer();
        let (note_uid, heading_uid) = seed_vault_note_heading_embeddings(
            &state,
            "vlt:prune:docs",
            "prune",
            "/definitely/missing/prune-docs",
        );

        let result = run_prune_stale_with(
            &state,
            delete_repo_cascade,
            |store, vault| {
                store
                    .delete_vault_cascade(&vault.uid)
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Ok(()),
        )
        .unwrap();

        assert_eq!(result.removed_vaults.len(), 1);
        assert_embeddings_absent(&state, &[&note_uid, &heading_uid]);
    }

    #[test]
    fn prune_surfaces_search_reconciliation_failure_after_other_finalizers() {
        let state = test_state_with_writer();
        seed_vault_note_heading_embeddings(
            &state,
            "vlt:prune:search-failure",
            "prune-search-failure",
            "/definitely/missing/prune-search-failure",
        );
        let generation_before = state.store.graph_generation();

        let error = run_prune_stale_with(
            &state,
            delete_repo_cascade,
            |store, vault| {
                store
                    .delete_vault_cascade(&vault.uid)
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| {
                Err(anyhow::anyhow!("injected Tantivy rebuild failure"))
            },
        )
        .unwrap();

        // nw-091 / Bug 2: committed prune → success-with-warnings (`error` binds the Ok response).
        assert!(error.committed);
        assert!(
            error
                .reconciliation_failures
                .iter()
                .any(|f| f.stage.contains("search-index")
                    || f.message.contains("injected Tantivy rebuild failure")),
            "search-index failure must surface as a warning, got: {:?}",
            error.reconciliation_failures
        );
        assert!(
            state.store.graph_generation() > generation_before,
            "generation finalization must precede the surfaced search failure"
        );
    }

    #[test]
    fn purge_preserves_real_mutation_error_and_appends_real_reconciliation_failure() {
        let state = test_state_with_writer();
        let repo_uid = "repo:partial:real-purge-error";
        let file_uid = nestweaver_schema::file_uid(repo_uid, "src/lib.rs");
        let mut repo = test_repo(repo_uid, "https://example.test/real-purge-error", None);
        repo.instance_id = "partial".to_string();
        state.store.insert_repo(&repo).unwrap();
        state
            .store
            .insert_file(&nestweaver_schema::File {
                uid: file_uid,
                path: "src/lib.rs".to_string(),
                repo_uid: repo_uid.to_string(),
                content_hash: "hash".to_string(),
            })
            .unwrap();
        let generation_path = nestweaver_engine::sidecar_path(&state.db_path, ".generation");
        std::fs::create_dir(&generation_path).unwrap();
        let mut search_reconciliations = 0;

        let error = run_purge_instance_with(
            &state,
            "partial",
            |store, _id| {
                let repo = store.lookup_repo(repo_uid)?.unwrap();
                delete_repo_cascade(store, &repo)?;
                Err(anyhow::anyhow!("real committed purge mutation failure"))
            },
            |_state, _mutation, _operation| {
                search_reconciliations += 1;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(
            error
                .message()
                .contains("real committed purge mutation failure")
        );
        assert!(error.message().contains("generation-persistence"));
        assert!(state.store.lookup_repo(repo_uid).unwrap().is_none());
        assert_eq!(
            search_reconciliations, 0,
            "code-only purge changed no indexed documents"
        );
    }

    #[test]
    fn merge_surfaces_search_reconciliation_failure_after_committed_mutation() {
        let state = test_state_with_writer();
        seed_vault_note_heading_embeddings(
            &state,
            "vlt:merge:search-failure",
            "merge-search-source",
            "/missing/merge-search-failure",
        );

        run_merge_instance_with(
            &state,
            "merge-search-source",
            "merge-search-target",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Err(anyhow::anyhow!("injected merge search failure")),
        )
        .unwrap();

        // nw-091 / Bug 2: the merge COMMITTED (asserted below); the search
        // finalizer failure is logged, not returned as an error.
        assert!(
            state
                .store
                .list_vaults(Some("merge-search-source"))
                .unwrap()
                .is_empty(),
            "source vault mutation must have committed before reconciliation failed"
        );
    }

    #[test]
    fn merge_extension_prepare_failure_prevents_graph_mutation() {
        let state = test_state_with_writer();
        seed_project(&state, "proj:old:prepare", "Prepare failure");
        let merge_called = std::cell::Cell::new(false);
        let generation_before = state.store.graph_generation();

        let error = run_merge_instance_with_extension_ops(
            &state,
            "old",
            "new",
            |_store, _from, _to| {
                merge_called.set(true);
                unreachable!("graph mutation must not run after extension prepare failure")
            },
            |_state, _mutation, _operation| Ok(()),
            |_store, _db_path, _from, _to| {
                Err::<((), bool), _>(anyhow::anyhow!("injected atomic prepare write failure"))
            },
            |_db_path, _migration| Ok(()),
        )
        .unwrap_err();

        assert!(error.message().contains("extension-metadata"));
        assert!(
            error
                .message()
                .contains("injected atomic prepare write failure")
        );
        assert!(!merge_called.get());
        assert!(state.store.project_exists("proj:old:prepare").unwrap());
        assert_eq!(state.store.graph_generation(), generation_before);
    }

    #[test]
    fn merge_extension_finalize_failure_surfaces_after_graph_and_finalizers() {
        let state = test_state_with_writer();
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: "proj:old:finalize".to_string(),
                name: "Finalize failure".to_string(),
                summary: None,
                instance_id: "old".to_string(),
            })
            .unwrap();
        let generation_before = state.store.graph_generation();

        run_merge_instance_with_extension_ops(
            &state,
            "old",
            "new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Ok(()),
            |_store, _db_path, _from, _to| Ok(((), true)),
            |_db_path, _migration| Err(anyhow::anyhow!("injected atomic finalize write failure")),
        )
        .unwrap();

        // nw-091 / Bug 2: merge COMMITTED; the extension-metadata finalize failure
        // is logged, not returned as an error.
        assert!(!state.store.project_exists("proj:old:finalize").unwrap());
        assert!(state.store.graph_generation() > generation_before);
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json").exists(),
            "node finalizer must still invalidate PageRank"
        );
    }

    #[test]
    fn merge_extension_finalize_failure_retries_from_durable_journal() {
        use nestweaver_schema::uid::project_uid;

        let state = test_state_with_writer();
        let source_uid = project_uid("old", "Retry migration");
        let destination_uid = project_uid("new", "Retry migration");
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: source_uid.clone(),
                name: "Retry migration".to_string(),
                summary: None,
                instance_id: "old".to_string(),
            })
            .unwrap();
        seed_extension(
            &state,
            &source_uid,
            "nested",
            serde_json::json!({"retry": [true, {"depth": 2}]}),
        );

        run_merge_instance_with_extension_ops(
            &state,
            "old",
            "new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Ok(()),
            |store, db_path, from, to| {
                let mappings = store.plan_instance_uid_remaps(from, to)?;
                let migration = nestweaver_engine::prepare_instance_extension_migration(
                    db_path, from, to, &mappings,
                )?;
                let active = migration.is_active();
                Ok((migration, active))
            },
            |_db_path, _migration| {
                Err(anyhow::anyhow!(
                    "injected post-graph extension write failure"
                ))
            },
        )
        .unwrap();
        // nw-091 / Bug 2: merge COMMITTED; the extension-metadata failure is logged
        // and the durable journal drives the retry (asserted below).
        assert!(!state.store.project_exists(&source_uid).unwrap());
        let staged = nestweaver_engine::load_extensions(&state.db_path);
        assert!(staged.contains_key(&source_uid));
        assert!(!staged.contains_key(&destination_uid));
        assert!(
            nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );

        // The graph no longer contains the source Project, so this retry can
        // succeed only by loading the persisted mapping journal.
        let retried = run_merge_instance_with(
            &state,
            "old",
            "new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Ok(()),
        )
        .unwrap();
        assert_eq!(retried.projects, 0);
        let finalized = nestweaver_engine::load_extensions(&state.db_path);
        assert!(!finalized.contains_key(&source_uid));
        assert_eq!(
            nestweaver_engine::get_property(&finalized, &destination_uid, "nested"),
            Some(&serde_json::json!({"retry": [true, {"depth": 2}]}))
        );
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn corrupt_extension_sidecar_prevents_real_merge_mutation() {
        use nestweaver_schema::uid::project_uid;

        let state = test_state_with_writer();
        let source_uid = project_uid("old", "Corrupt extension");
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: source_uid.clone(),
                name: "Corrupt extension".to_string(),
                summary: None,
                instance_id: "old".to_string(),
            })
            .unwrap();
        let extension_path = nestweaver_engine::sidecar_path(&state.db_path, ".extensions.json");
        std::fs::write(&extension_path, b"{not-json").unwrap();
        let generation_before = state.store.graph_generation();

        let error = run_merge_instance_with(
            &state,
            "old",
            "new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Ok(()),
        )
        .unwrap_err();

        assert!(error.message().contains("parse extension sidecar"));
        assert!(state.store.project_exists(&source_uid).unwrap());
        assert_eq!(state.store.graph_generation(), generation_before);
        assert_eq!(std::fs::read(&extension_path).unwrap(), b"{not-json");
    }

    #[test]
    fn startup_recovery_automatically_finishes_a_prepared_extension_migration() {
        use nestweaver_schema::uid::project_uid;

        let state = test_state_with_writer();
        let source_uid = project_uid("old", "Startup recovery");
        let destination_uid = project_uid("new", "Startup recovery");
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: source_uid.clone(),
                name: "Startup recovery".to_string(),
                summary: None,
                instance_id: "old".to_string(),
            })
            .unwrap();
        seed_extension(
            &state,
            &source_uid,
            "nested",
            serde_json::json!({"automatic": [true, {"depth": 3}]}),
        );
        let mappings = state.store.plan_instance_uid_remaps("old", "new").unwrap();
        nestweaver_engine::prepare_instance_extension_migration(
            &state.db_path,
            "old",
            "new",
            &mappings,
        )
        .unwrap();
        let generation_before = state.store.graph_generation();

        recover_pending_instance_extension_migration(&state).unwrap();

        assert!(!state.store.project_exists(&source_uid).unwrap());
        assert!(state.store.project_exists(&destination_uid).unwrap());
        assert!(state.store.graph_generation() > generation_before);
        let extensions = nestweaver_engine::load_extensions(&state.db_path);
        assert!(!extensions.contains_key(&source_uid));
        assert_eq!(
            nestweaver_engine::get_property(&extensions, &destination_uid, "nested"),
            Some(&serde_json::json!({"automatic": [true, {"depth": 3}]}))
        );
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn startup_recovery_restores_repo_deleted_before_destination_insert() {
        use nestweaver_schema::uid::repo_uid;

        let state = test_state_with_writer();
        let db_path = state.db_path.clone();
        let url = "https://example.test/repo-delete-before-insert";
        let source_uid = repo_uid("old", url);
        let destination_uid = repo_uid("new", url);
        let mut repo = test_repo(&source_uid, url, Some("/work/repo-delete-before-insert"));
        repo.instance_id = "old".to_string();
        repo.name = Some("crash-window".to_string());
        repo.staleness_commits_behind = 7;
        state.store.insert_repo(&repo).unwrap();
        seed_extension(
            &state,
            &source_uid,
            "owner",
            serde_json::json!("preserve-on-recovery"),
        );

        let plan = state
            .store
            .plan_instance_uid_migration("old", "new")
            .unwrap();
        nestweaver_engine::prepare_instance_uid_migration_with_finalizers(
            &state.db_path,
            "old",
            "new",
            &plan,
            &nestweaver_engine::InstanceMigrationFinalizerPlan {
                repo_uids: vec![source_uid.clone()],
                search_reconciliation_required: false,
            },
        )
        .unwrap();

        state
            .store
            .bulk_delete_repo_files_and_symbols(&source_uid)
            .unwrap();
        state.store.clear_repo_derived_nodes(&source_uid).unwrap();
        state.store.delete_repo_node(&source_uid).unwrap();
        assert!(state.store.lookup_repo(&source_uid).unwrap().is_none());
        assert!(state.store.lookup_repo(&destination_uid).unwrap().is_none());

        recover_pending_instance_extension_migration(&state).unwrap();

        let recovered = state
            .store
            .lookup_repo(&destination_uid)
            .unwrap()
            .expect("startup recovery must restore the missing destination Repo");
        assert_eq!(recovered.instance_id, "new");
        assert_eq!(recovered.url, url);
        assert_eq!(recovered.indexed_sha, "");
        assert_eq!(recovered.staleness_commits_behind, 7);
        assert_eq!(recovered.name.as_deref(), Some("crash-window"));
        assert_eq!(
            recovered.root_path.as_deref(),
            Some("/work/repo-delete-before-insert")
        );
        assert!(state.store.lookup_repo(&source_uid).unwrap().is_none());
        let extensions = nestweaver_engine::load_extensions(&state.db_path);
        assert!(!extensions.contains_key(&source_uid));
        assert_eq!(
            nestweaver_engine::get_property(&extensions, &destination_uid, "owner"),
            Some(&serde_json::json!("preserve-on-recovery"))
        );
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );

        drop(state);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        let recovered = reopened.lookup_repo(&destination_uid).unwrap().unwrap();
        assert_eq!(recovered.indexed_sha, "");
        assert_eq!(recovered.name.as_deref(), Some("crash-window"));
        assert_eq!(
            nestweaver_engine::get_property(
                &nestweaver_engine::load_extensions(&db_path),
                &destination_uid,
                "owner",
            ),
            Some(&serde_json::json!("preserve-on-recovery"))
        );
    }

    #[test]
    fn startup_recovery_restores_vault_and_project_deleted_before_destination_insert() {
        use nestweaver_schema::uid::{project_uid, vault_uid};

        let state = test_state_with_writer();
        let vault_root = "/tmp/startup-vault-delete-before-insert";
        let source_vault_uid = vault_uid("old", vault_root);
        let destination_vault_uid = vault_uid("new", vault_root);
        state
            .store
            .insert_vault(&nestweaver_schema::Vault {
                uid: source_vault_uid.clone(),
                name: "Startup vault recovery".to_string(),
                root_path: vault_root.to_string(),
                instance_id: "old".to_string(),
            })
            .unwrap();
        let source_project_uid = project_uid("old", "Startup project recovery");
        let destination_project_uid = project_uid("new", "Startup project recovery");
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: source_project_uid.clone(),
                name: "Startup project recovery".to_string(),
                summary: Some("recover project metadata".to_string()),
                instance_id: "old".to_string(),
            })
            .unwrap();
        seed_extension(
            &state,
            &source_vault_uid,
            "owner",
            serde_json::json!("vault-owner"),
        );
        seed_extension(
            &state,
            &source_project_uid,
            "owner",
            serde_json::json!("project-owner"),
        );

        let plan = state
            .store
            .plan_instance_uid_migration("old", "new")
            .unwrap();
        nestweaver_engine::prepare_instance_uid_migration_with_finalizers(
            &state.db_path,
            "old",
            "new",
            &plan,
            &nestweaver_engine::InstanceMigrationFinalizerPlan::default(),
        )
        .unwrap();
        state.store.delete_vault_cascade(&source_vault_uid).unwrap();
        state
            .store
            .delete_project_node(&source_project_uid)
            .unwrap();
        assert!(
            state
                .store
                .list_vaults(None)
                .unwrap()
                .iter()
                .all(|vault| vault.uid != destination_vault_uid)
        );
        assert!(
            !state
                .store
                .project_exists(&destination_project_uid)
                .unwrap()
        );

        recover_pending_instance_extension_migration(&state).unwrap();

        let recovered_vault = state
            .store
            .list_vaults(None)
            .unwrap()
            .into_iter()
            .find(|vault| vault.uid == destination_vault_uid)
            .expect("startup recovery must restore the missing destination Vault");
        assert_eq!(recovered_vault.instance_id, "new");
        assert_eq!(recovered_vault.root_path, vault_root);
        assert!(
            state
                .store
                .project_exists(&destination_project_uid)
                .unwrap()
        );
        assert!(!state.store.project_exists(&source_project_uid).unwrap());
        let extensions = nestweaver_engine::load_extensions(&state.db_path);
        assert_eq!(
            nestweaver_engine::get_property(&extensions, &destination_vault_uid, "owner"),
            Some(&serde_json::json!("vault-owner"))
        );
        assert_eq!(
            nestweaver_engine::get_property(&extensions, &destination_project_uid, "owner"),
            Some(&serde_json::json!("project-owner"))
        );
        assert!(!extensions.contains_key(&source_vault_uid));
        assert!(!extensions.contains_key(&source_project_uid));
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn startup_recovery_uses_graph_applied_journal_for_code_sidecars() {
        use nestweaver_schema::uid::repo_uid;

        let state = test_state_with_writer();
        let url = "https://example.test/crash-recovery-code";
        let source_uid = repo_uid("old", url);
        let mut repo = test_repo(&source_uid, url, None);
        repo.instance_id = "old".to_string();
        state.store.insert_repo(&repo).unwrap();

        let filemeta_path = nestweaver_engine::sidecar_path(&state.db_path, ".filemeta.json");
        let mut filemeta = nestweaver_engine::load_filemeta_sidecar(&filemeta_path);
        filemeta.repos.entry(source_uid.clone()).or_default();
        nestweaver_engine::save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();
        let deps_path = nestweaver_engine::sidecar_path(&state.db_path, ".resolution_deps.bin");
        let mut deps = nestweaver_engine::resolution_cache::ResolutionDeps::default();
        deps.set_deps_for_repo(
            &source_uid,
            "src/lib.rs",
            std::collections::HashSet::from(["src/dep.rs".to_string()]),
        );
        deps.save(&deps_path).unwrap();

        let mappings = state.store.plan_instance_uid_remaps("old", "new").unwrap();
        let prepared = nestweaver_engine::prepare_instance_extension_migration_with_finalizers(
            &state.db_path,
            "old",
            "new",
            &mappings,
            &nestweaver_engine::InstanceMigrationFinalizerPlan {
                repo_uids: vec![source_uid.clone()],
                search_reconciliation_required: false,
            },
        )
        .unwrap();
        state.store.merge_instance_ids("old", "new").unwrap();
        nestweaver_engine::mark_instance_extension_migration_graph_applied(
            &state.db_path,
            &prepared,
        )
        .unwrap();

        assert!(
            nestweaver_engine::load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(&source_uid)
        );
        assert!(
            !nestweaver_engine::resolution_cache::ResolutionDeps::load(&deps_path)
                .is_empty_for_repo(&source_uid)
        );

        recover_pending_instance_extension_migration(&state).unwrap();

        assert!(
            !nestweaver_engine::load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(&source_uid),
            "post-graph recovery selected node-only finalization"
        );
        assert!(
            nestweaver_engine::resolution_cache::ResolutionDeps::load(&deps_path)
                .is_empty_for_repo(&source_uid)
        );
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn startup_recovery_uses_graph_applied_journal_for_tantivy() {
        use nestweaver_schema::uid::vault_uid;

        let state = test_state_with_writer();
        let root = "/missing/crash-recovery-vault";
        let source_vault_uid = vault_uid("old", root);
        let (source_note_uid, _) =
            seed_vault_note_heading_embeddings(&state, &source_vault_uid, "old", root);
        reconcile_search_index(
            &state.search_reconciliation,
            &state.store,
            IndexedSearchMutation::Changed,
            "seed_crash_recovery_vault",
        )
        .unwrap();

        let prepared = nestweaver_engine::prepare_instance_extension_migration_with_finalizers(
            &state.db_path,
            "old",
            "new",
            &[],
            &nestweaver_engine::InstanceMigrationFinalizerPlan {
                repo_uids: Vec::new(),
                search_reconciliation_required: true,
            },
        )
        .unwrap();
        state.store.merge_instance_ids("old", "new").unwrap();
        nestweaver_engine::mark_instance_extension_migration_graph_applied(
            &state.db_path,
            &prepared,
        )
        .unwrap();
        let destination_vault_uid = state.store.list_vaults(Some("new")).unwrap()[0].uid.clone();
        let stale_hits = state.tantivy.as_ref().unwrap().search("Note", 10).unwrap();
        assert!(
            stale_hits
                .iter()
                .any(|hit| { hit.uid == source_note_uid && hit.vault_uid == source_vault_uid })
        );
        assert!(
            !stale_hits.iter().any(|hit| {
                hit.uid == source_note_uid && hit.vault_uid == destination_vault_uid
            })
        );

        recover_pending_instance_extension_migration(&state).unwrap();

        let recovered_hits = state.tantivy.as_ref().unwrap().search("Note", 10).unwrap();
        assert!(
            !recovered_hits
                .iter()
                .any(|hit| { hit.uid == source_note_uid && hit.vault_uid == source_vault_uid })
        );
        assert!(
            recovered_hits
                .iter()
                .any(|hit| hit.uid == source_note_uid && hit.vault_uid == destination_vault_uid)
        );
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn boot_self_heals_graph_applied_journal_with_remaining_source_rows() {
        // nw-091 / Bug 3B: a journal marked graph_applied while the graph still
        // holds source rows (a durability inversion — the journal sidecar landed,
        // the DB commit didn't) used to WEDGE daemon boot with no forward path.
        // Recovery must self-heal by re-driving the idempotent merge.
        use nestweaver_schema::uid::vault_uid;
        let state = test_state_with_writer();
        let root = "/self-heal/vault";
        let source_vault_uid = vault_uid("old", root);
        seed_vault_note_heading_embeddings(&state, &source_vault_uid, "old", root);

        // Prepare a migration journal and advance it to GraphApplied WITHOUT
        // mutating the graph — exactly the wedge state.
        let mappings = state.store.plan_instance_uid_remaps("old", "new").unwrap();
        let migration = nestweaver_engine::prepare_instance_extension_migration(
            &state.db_path,
            "old",
            "new",
            &mappings,
        )
        .unwrap();
        nestweaver_engine::mark_instance_extension_migration_graph_applied(
            &state.db_path,
            &migration,
        )
        .unwrap();
        // Precondition: journal says graph_applied, but the source vault remains.
        assert!(
            nestweaver_engine::pending_instance_extension_migration(&state.db_path)
                .unwrap()
                .graph_applied()
        );
        assert!(!state.store.list_vaults(Some("old")).unwrap().is_empty());

        // Recovery must SELF-HEAL (re-drive the idempotent merge), not error.
        recover_pending_instance_extension_migration(&state).unwrap();

        // The source vault's notes are reparented under "new"; none remain under
        // "old" — no data lost, and the daemon boots.
        assert!(
            state.store.list_vaults(Some("old")).unwrap().is_empty(),
            "self-heal must finish migrating the remaining source rows"
        );
        assert!(!state.store.list_vaults(Some("new")).unwrap().is_empty());
    }

    #[test]
    fn graph_applied_finalizer_failure_keeps_journal_for_retry() {
        use nestweaver_schema::uid::vault_uid;

        let state = test_state_with_writer();
        let root = "/missing/retry-search-finalizer";
        let source_vault_uid = vault_uid("old", root);
        seed_vault_note_heading_embeddings(&state, &source_vault_uid, "old", root);

        run_merge_instance_with(
            &state,
            "old",
            "new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| {
                Err(anyhow::anyhow!(
                    "injected persisted search finalizer failure"
                ))
            },
        )
        .unwrap();
        // nw-091 / Bug 2: the merge COMMITTED; a post-commit finalizer failure is
        // logged and the journal is retained at graph_applied for retry (asserted
        // below), not returned as an error that reads as "the merge failed".
        let pending =
            nestweaver_engine::pending_instance_extension_migration(&state.db_path).unwrap();
        assert!(pending.graph_applied());
        assert!(!pending.reconciled());
        assert!(pending.search_reconciliation_required());

        let mut retries = 0;
        run_merge_instance_with(
            &state,
            "old",
            "new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, mutation, _operation| {
                retries += 1;
                assert_eq!(mutation, IndexedSearchMutation::Changed);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(retries, 1);
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn real_code_finalizer_persistence_failure_keeps_graph_applied_journal() {
        use nestweaver_schema::uid::repo_uid;

        let state = test_state_with_writer();
        let url = "https://example.test/real-finalizer-retry";
        let source_uid = repo_uid("old", url);
        let mut repo = test_repo(&source_uid, url, None);
        repo.instance_id = "old".to_string();
        state.store.insert_repo(&repo).unwrap();
        let generation_path = nestweaver_engine::sidecar_path(&state.db_path, ".generation");
        std::fs::create_dir(&generation_path).unwrap();

        run_merge_instance_with(
            &state,
            "old",
            "new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Ok(()),
        )
        .unwrap();
        // nw-091 / Bug 2: merge COMMITTED; the generation-persistence failure is
        // logged and the journal is retained at graph_applied for retry, below.
        let pending =
            nestweaver_engine::pending_instance_extension_migration(&state.db_path).unwrap();
        assert!(pending.graph_applied());
        assert!(!pending.reconciled());
        assert_eq!(pending.finalizer_repo_uids(), &[source_uid]);

        std::fs::remove_dir(&generation_path).unwrap();
        recover_pending_instance_extension_migration(&state).unwrap();
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn unavailable_search_finalizer_keeps_graph_applied_journal_until_restart_retry() {
        use nestweaver_schema::uid::vault_uid;

        let mut state = test_state_with_writer();
        let root = "/missing/unavailable-search-retry";
        let source_vault_uid = vault_uid("old", root);
        seed_vault_note_heading_embeddings(&state, &source_vault_uid, "old", root);
        Arc::get_mut(&mut state).unwrap().search_reconciliation =
            SearchIndexReconciliation::Unavailable("injected writer outage".to_string());

        run_merge_instance_with(
            &state,
            "old",
            "new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            rebuild_tantivy_after_mutation,
        )
        .unwrap();
        // nw-091 / Bug 2: merge COMMITTED; the search-unavailable finalizer failure
        // is logged and the journal is retained at graph_applied for retry, below.
        let pending =
            nestweaver_engine::pending_instance_extension_migration(&state.db_path).unwrap();
        assert!(pending.graph_applied());
        assert!(!pending.reconciled());
        assert!(pending.search_reconciliation_required());

        let tantivy = Arc::clone(state.tantivy.as_ref().unwrap());
        Arc::get_mut(&mut state).unwrap().search_reconciliation =
            SearchIndexReconciliation::Available(tantivy);
        recover_pending_instance_extension_migration(&state).unwrap();
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn reconciled_retry_skips_graph_and_finalizers_then_finishes_extensions() {
        use nestweaver_schema::uid::project_uid;

        let state = test_state_with_writer();
        let source_uid = project_uid("old", "Reconciled retry");
        let destination_uid = project_uid("new", "Reconciled retry");
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: source_uid.clone(),
                name: "Reconciled retry".to_string(),
                summary: None,
                instance_id: "old".to_string(),
            })
            .unwrap();
        seed_extension(&state, &source_uid, "owner", serde_json::json!("source"));
        let mappings = state.store.plan_instance_uid_remaps("old", "new").unwrap();
        let prepared = nestweaver_engine::prepare_instance_extension_migration_with_finalizers(
            &state.db_path,
            "old",
            "new",
            &mappings,
            &nestweaver_engine::InstanceMigrationFinalizerPlan::default(),
        )
        .unwrap();
        state.store.merge_instance_ids("old", "new").unwrap();
        let graph_applied = nestweaver_engine::mark_instance_extension_migration_graph_applied(
            &state.db_path,
            &prepared,
        )
        .unwrap();
        assert!(finalize_node_graph_deletion(&state, "test_reconciled_retry").is_empty());
        let pagerank_generation = state.store.pagerank_generation();
        nestweaver_engine::mark_instance_extension_migration_reconciled(
            &state.db_path,
            &graph_applied,
        )
        .unwrap();
        let merge_calls = std::cell::Cell::new(0);
        let search_calls = std::cell::Cell::new(0);

        run_merge_instance_with(
            &state,
            "old",
            "new",
            |_store, _from, _to| {
                merge_calls.set(merge_calls.get() + 1);
                unreachable!("reconciled retry must not re-run graph mutation")
            },
            |_state, _mutation, _operation| {
                search_calls.set(search_calls.get() + 1);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(merge_calls.get(), 0);
        assert_eq!(search_calls.get(), 0);
        assert_eq!(state.store.pagerank_generation(), pagerank_generation);
        let extensions = nestweaver_engine::load_extensions(&state.db_path);
        assert!(!extensions.contains_key(&source_uid));
        assert_eq!(
            nestweaver_engine::get_property(&extensions, &destination_uid, "owner"),
            Some(&serde_json::json!("source"))
        );
        assert!(
            !nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json").exists()
        );
    }

    #[test]
    fn startup_recovery_fails_closed_on_corrupt_versioned_or_mismatched_journal() {
        use nestweaver_schema::uid::project_uid;

        for case in ["corrupt", "version", "pair"] {
            let state = test_state_with_writer();
            let source_uid = project_uid("old", &format!("Startup blocked {case}"));
            let destination_uid = project_uid("new", &format!("Startup blocked {case}"));
            state
                .store
                .insert_project(&nestweaver_schema::Project {
                    uid: source_uid.clone(),
                    name: format!("Startup blocked {case}"),
                    summary: None,
                    instance_id: "old".to_string(),
                })
                .unwrap();
            seed_extension(&state, &source_uid, "must-stay", serde_json::json!(true));
            let mappings = state.store.plan_instance_uid_remaps("old", "new").unwrap();
            nestweaver_engine::prepare_instance_extension_migration(
                &state.db_path,
                "old",
                "new",
                &mappings,
            )
            .unwrap();
            let journal_path =
                nestweaver_engine::sidecar_path(&state.db_path, ".extensions.migration.json");
            if case == "corrupt" {
                std::fs::write(&journal_path, b"{not-json").unwrap();
            } else {
                let mut journal: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
                if case == "version" {
                    journal["version"] = serde_json::json!(999);
                } else {
                    journal["from_id"] = serde_json::json!("other");
                }
                std::fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap())
                    .unwrap();
            }
            let generation_before = state.store.graph_generation();

            assert!(
                recover_pending_instance_extension_migration(&state).is_err(),
                "startup accepted {case} journal"
            );
            assert!(state.store.project_exists(&source_uid).unwrap());
            assert!(!state.store.project_exists(&destination_uid).unwrap());
            assert_eq!(state.store.graph_generation(), generation_before);
            let extensions = nestweaver_engine::load_extensions(&state.db_path);
            assert!(extensions.contains_key(&source_uid));
            assert!(!extensions.contains_key(&destination_uid));
        }
    }

    #[test]
    fn purge_vault_only_instance_prunes_note_and_heading_embeddings() {
        let state = test_state_with_writer();
        let (note_uid, heading_uid) = seed_vault_note_heading_embeddings(
            &state,
            "vlt:purge:docs",
            "purge-source",
            "/missing/purge-docs",
        );

        let result = run_purge_instance_with(
            &state,
            "purge-source",
            |store, id| store.purge_instance(id).map_err(anyhow::Error::from),
            |_state, _mutation, _operation| Ok(()),
        )
        .unwrap();

        assert_eq!(result.vaults, 1);
        assert_embeddings_absent(&state, &[&note_uid, &heading_uid]);
    }

    #[test]
    fn purge_instance_durably_removes_deleted_project_extensions_only() {
        let state = test_state_with_writer();
        let removed_uid = "proj:purge-projects:removed";
        let retained_uid = "proj:other:retained";
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: removed_uid.to_string(),
                name: "Removed".to_string(),
                summary: None,
                instance_id: "purge-projects".to_string(),
            })
            .unwrap();
        state
            .store
            .insert_project(&nestweaver_schema::Project {
                uid: retained_uid.to_string(),
                name: "Retained".to_string(),
                summary: None,
                instance_id: "other".to_string(),
            })
            .unwrap();
        seed_extension(&state, removed_uid, "owner", serde_json::json!("remove"));
        seed_extension(&state, retained_uid, "owner", serde_json::json!("keep"));

        let result = run_purge_instance_with(
            &state,
            "purge-projects",
            |store, id| store.purge_instance(id).map_err(anyhow::Error::from),
            |_state, _mutation, _operation| Ok(()),
        )
        .unwrap();

        assert_eq!(result.projects, 1);
        let db_path = state.db_path.clone();
        drop(state);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        let extensions = nestweaver_engine::load_extensions(&db_path);
        assert!(!extensions.contains_key(removed_uid));
        assert_eq!(
            nestweaver_engine::get_property(&extensions, retained_uid, "owner"),
            Some(&serde_json::json!("keep"))
        );
        assert!(!reopened.project_exists(removed_uid).unwrap());
        assert!(reopened.project_exists(retained_uid).unwrap());
    }

    #[test]
    fn merge_vault_collision_prunes_discarded_note_and_heading_embeddings() {
        let state = test_state_with_writer();
        let root_path = "/shared/merge-docs";
        let (source_note, source_heading) = seed_vault_note_heading_embeddings(
            &state,
            "vlt:merge:source",
            "merge-source",
            root_path,
        );
        let (target_note, target_heading) = seed_vault_note_heading_embeddings(
            &state,
            "vlt:merge:target",
            "merge-target",
            root_path,
        );

        let result = run_merge_instance_with(
            &state,
            "merge-source",
            "merge-target",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Ok(()),
        )
        .unwrap();

        assert_eq!(result.discarded.len(), 1);
        assert_embeddings_absent(&state, &[&source_note, &source_heading]);
        assert!(state.store.has_embedding(&target_note));
        assert!(state.store.has_embedding(&target_heading));
    }

    #[test]
    fn actual_repo_merge_collision_reconciles_source_and_destination_state() {
        use nestweaver_schema::uid::repo_uid;

        let state = test_state_with_writer();
        let url = "https://example.test/merge-collision";
        let source_uid = repo_uid("merge-old", url);
        let target_uid = repo_uid("merge-new", url);
        let mut source = test_repo(&source_uid, url, None);
        source.instance_id = "merge-old".to_string();
        let mut target = test_repo(&target_uid, url, None);
        target.instance_id = "merge-new".to_string();
        state.store.insert_repo(&source).unwrap();
        state.store.insert_repo(&target).unwrap();
        let source_symbol = seed_manifest_and_embedding(&state, &source_uid, "source-package");
        let target_symbol = seed_manifest_and_embedding(&state, &target_uid, "target-package");

        let result = run_merge_instance_with(
            &state,
            "merge-old",
            "merge-new",
            |store, from, to| {
                store
                    .merge_instance_ids(from, to)
                    .map_err(anyhow::Error::from)
            },
            |_state, _mutation, _operation| Ok(()),
        )
        .unwrap();

        assert_eq!(result.repo_uids_removed, vec![source_uid.clone()]);
        let manifests = nestweaver_engine::load_manifest_cache_for_db(&state.db_path).unwrap();
        assert!(!manifests.contains_key(&source_uid));
        assert_eq!(
            manifests[&target_uid].package_name.as_deref(),
            Some("target-package")
        );
        assert!(!state.store.has_embedding(&source_symbol));
        assert!(state.store.has_embedding(&target_symbol));
        assert_eq!(
            state
                .store
                .lookup_repo(&target_uid)
                .unwrap()
                .unwrap()
                .indexed_sha,
            target.indexed_sha
        );
    }

    #[tokio::test]
    async fn suggest_links_reads_the_canonical_manifest_sidecar() {
        let state = test_state_with_writer();
        for (uid, url) in [
            ("repo:test:app", "https://example.test/app"),
            ("repo:test:dependency", "https://example.test/dependency"),
        ] {
            state.store.insert_repo(&test_repo(uid, url, None)).unwrap();
        }
        let manifests = std::collections::HashMap::from([
            (
                "repo:test:app".to_string(),
                nestweaver_engine::ManifestInfo {
                    package_name: Some("app-package".to_string()),
                    dependencies: vec!["dependency-package".to_string()],
                    entry_files: Vec::new(),
                },
            ),
            (
                "repo:test:dependency".to_string(),
                nestweaver_engine::ManifestInfo {
                    package_name: Some("dependency-package".to_string()),
                    dependencies: Vec::new(),
                    entry_files: Vec::new(),
                },
            ),
        ]);
        nestweaver_engine::save_manifest_cache(
            &manifests,
            &nestweaver_engine::sidecar_path(&state.db_path, ".manifests.json"),
        )
        .unwrap();

        let response = DaemonService::new(state)
            .suggest_links_json(Request::new(JsonRequest {
                args_json: "{}".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        let suggestions: serde_json::Value = serde_json::from_str(&response.result_json).unwrap();
        assert!(suggestions["links"].as_array().unwrap().iter().any(|link| {
            link["description"] == "Depends on dependency-package (from manifest)"
        }));
    }

    #[tokio::test]
    async fn suggest_links_migrates_and_reads_a_legacy_only_manifest_sidecar() {
        let state = test_state_with_writer();
        for (uid, url) in [
            ("repo:test:legacy-app", "https://example.test/legacy-app"),
            (
                "repo:test:legacy-dependency",
                "https://example.test/legacy-dependency",
            ),
        ] {
            state.store.insert_repo(&test_repo(uid, url, None)).unwrap();
        }
        let manifests = std::collections::HashMap::from([
            (
                "repo:test:legacy-app".to_string(),
                nestweaver_engine::ManifestInfo {
                    package_name: Some("legacy-app-package".to_string()),
                    dependencies: vec!["legacy-dependency-package".to_string()],
                    entry_files: Vec::new(),
                },
            ),
            (
                "repo:test:legacy-dependency".to_string(),
                nestweaver_engine::ManifestInfo {
                    package_name: Some("legacy-dependency-package".to_string()),
                    dependencies: Vec::new(),
                    entry_files: Vec::new(),
                },
            ),
        ]);
        let legacy_path = state.db_path.with_extension("manifests.json");
        let canonical_path = nestweaver_engine::manifest_cache_path(&state.db_path);
        nestweaver_engine::save_manifest_cache(&manifests, &legacy_path).unwrap();
        assert!(!canonical_path.exists());

        let response = DaemonService::new(state)
            .suggest_links_json(Request::new(JsonRequest {
                args_json: "{}".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        let suggestions: serde_json::Value = serde_json::from_str(&response.result_json).unwrap();
        assert!(suggestions["links"].as_array().unwrap().iter().any(|link| {
            link["description"] == "Depends on legacy-dependency-package (from manifest)"
        }));
        assert!(canonical_path.exists());
        assert!(!legacy_path.exists());
    }

    /// DATA-LOSS REGRESSION GUARD: `prune_stale_repos` must NEVER delete a
    /// repo with a remote identity url and no local working tree
    /// (`root_path: None`) — e.g. a server-side bare-clone repo. The old
    /// implementation derived a disk path from `url.strip_prefix("file://")`,
    /// which fell back to the raw https URL, never exists on disk, and
    /// bulk-deleted the repo's entire graph.
    #[test]
    fn prune_stale_repos_never_deletes_remote_identity_repos() {
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        store
            .insert_repo(&test_repo(
                "repo:server",
                "https://github.com/acme/server-only.git",
                None,
            ))
            .unwrap();

        let removed = prune_stale_repos(&store).unwrap();

        assert!(
            removed.is_empty(),
            "remote-identity repo without a working tree must be skipped, got {removed:?}"
        );
        assert!(
            store.lookup_repo("repo:server").unwrap().is_some(),
            "server-side repo must survive prune_stale"
        );
    }

    /// Repos with a recorded root_path (or a legacy file:// identity) whose
    /// working tree vanished ARE pruned; ones that still exist are kept.
    #[test]
    fn prune_stale_repos_prunes_missing_local_trees_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let existing = tmp.path().join("still-here");
        std::fs::create_dir_all(&existing).unwrap();
        let gone = tmp.path().join("gone");

        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        // Origin identity + existing working tree → kept.
        store
            .insert_repo(&test_repo(
                "repo:kept",
                "https://github.com/acme/kept.git",
                Some(existing.to_str().unwrap()),
            ))
            .unwrap();
        // Origin identity + vanished working tree → pruned.
        store
            .insert_repo(&test_repo(
                "repo:moved",
                "https://github.com/acme/moved.git",
                Some(gone.to_str().unwrap()),
            ))
            .unwrap();
        // Legacy row: file:// identity, no root_path, tree vanished → pruned
        // via the local_root() compat fallback.
        store
            .insert_repo(&test_repo(
                "repo:legacy",
                &format!("file://{}/legacy-gone", tmp.path().display()),
                None,
            ))
            .unwrap();

        let removed = prune_stale_repos(&store).unwrap();

        assert_eq!(removed.names.len(), 2, "got {removed:?}");
        assert!(store.lookup_repo("repo:kept").unwrap().is_some());
        assert!(store.lookup_repo("repo:moved").unwrap().is_none());
        assert!(store.lookup_repo("repo:legacy").unwrap().is_none());
    }

    #[test]
    fn prune_repo_failure_finalizes_earlier_deletions_before_returning_error() {
        let state = test_state_with_writer();
        let missing = state.db_path.with_extension("missing-repo-root");
        for uid in ["repo:test:first", "repo:test:second"] {
            state
                .store
                .insert_repo(&test_repo(
                    uid,
                    &format!("https://example.test/{uid}"),
                    Some(missing.to_str().unwrap()),
                ))
                .unwrap();
        }
        let first_symbol = seed_manifest_and_embedding(&state, "repo:test:first", "first-package");
        let second_symbol =
            seed_manifest_and_embedding(&state, "repo:test:second", "second-package");
        let generation = state.store.graph_generation();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"sentinel":0.5}"#).unwrap();

        let mut calls = 0;
        let mut reconciliations = 0;
        let error = run_prune_stale_with(
            &state,
            |store, repo| {
                calls += 1;
                if calls == 1 {
                    delete_repo_cascade(store, repo)
                } else {
                    Err(anyhow::anyhow!("injected second repo failure"))
                }
            },
            |_store, _vault| Ok(()),
            |_state, _mutation, _operation| {
                reconciliations += 1;
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::Internal);
        assert_eq!(calls, 2);
        assert_eq!(
            reconciliations, 0,
            "code-only prune must not rebuild vault document search"
        );
        assert!(state.store.graph_generation() > generation);
        assert!(!pagerank_path.exists(), "stale PageRank sidecar survived");
        assert_eq!(state.store.list_repos(None).unwrap().len(), 1);
        let manifests = nestweaver_engine::load_manifest_cache(&nestweaver_engine::sidecar_path(
            &state.db_path,
            ".manifests.json",
        ))
        .unwrap();
        assert!(!manifests.contains_key("repo:test:first"));
        assert!(manifests.contains_key("repo:test:second"));
        assert!(!state.store.has_embedding(&first_symbol));
        assert!(state.store.has_embedding(&second_symbol));
    }

    #[test]
    fn prune_vault_failure_finalizes_earlier_repo_deletion() {
        let state = test_state_with_writer();
        let missing = state.db_path.with_extension("missing-prune-roots");
        state
            .store
            .insert_repo(&test_repo(
                "repo:test:gone",
                "https://example.test/gone",
                Some(missing.to_str().unwrap()),
            ))
            .unwrap();
        state
            .store
            .insert_vault(&nestweaver_schema::Vault {
                uid: "vlt:test:gone".to_string(),
                name: "gone-vault".to_string(),
                root_path: missing.display().to_string(),
                instance_id: "test".to_string(),
            })
            .unwrap();
        let generation = state.store.graph_generation();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"sentinel":0.5}"#).unwrap();

        let mut reconciliations = 0;
        let error = run_prune_stale_with(
            &state,
            delete_repo_cascade,
            |_store, _vault| Err(anyhow::anyhow!("injected vault failure")),
            |_state, _mutation, _operation| {
                reconciliations += 1;
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(state.store.lookup_repo("repo:test:gone").unwrap().is_none());
        assert!(state.store.lookup_vault("vlt:test:gone").is_ok());
        assert!(state.store.graph_generation() > generation);
        assert!(!pagerank_path.exists(), "stale PageRank sidecar survived");
        assert_eq!(
            reconciliations, 0,
            "repo deletion plus an empty-vault failure changes no indexed document rows"
        );
    }

    #[test]
    fn purge_failure_finalizes_partial_repo_deletion() {
        let state = test_state_with_writer();
        state
            .store
            .insert_repo(&test_repo(
                "repo:old:partial",
                "https://example.test/partial",
                None,
            ))
            .unwrap();
        state
            .store
            .insert_repo(&test_repo(
                "repo:survivor:purge",
                "https://example.test/purge-survivor",
                None,
            ))
            .unwrap();
        let removed_symbol =
            seed_manifest_and_embedding(&state, "repo:old:partial", "purged-package");
        let survivor_symbol =
            seed_manifest_and_embedding(&state, "repo:survivor:purge", "survivor-package");
        let generation = state.store.graph_generation();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"sentinel":0.5}"#).unwrap();

        let mut reconciliations = 0;
        let error = run_purge_instance_with(
            &state,
            "old",
            |store, _id| {
                let repo = store.lookup_repo("repo:old:partial")?.unwrap();
                delete_repo_cascade(store, &repo)?;
                Err(anyhow::anyhow!("injected late purge failure"))
            },
            |_state, _mutation, _operation| {
                reconciliations += 1;
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(
            state
                .store
                .lookup_repo("repo:old:partial")
                .unwrap()
                .is_none()
        );
        assert!(state.store.graph_generation() > generation);
        assert!(!pagerank_path.exists(), "stale PageRank sidecar survived");
        assert_eq!(
            reconciliations, 0,
            "code-only purge must not rebuild Tantivy"
        );
        let manifests = nestweaver_engine::load_manifest_cache(&nestweaver_engine::sidecar_path(
            &state.db_path,
            ".manifests.json",
        ))
        .unwrap();
        assert!(!manifests.contains_key("repo:old:partial"));
        assert!(manifests.contains_key("repo:survivor:purge"));
        assert!(!state.store.has_embedding(&removed_symbol));
        assert!(state.store.has_embedding(&survivor_symbol));
    }

    #[test]
    fn merge_failure_finalizes_partial_repo_deletion() {
        let state = test_state_with_writer();
        for suffix in ["first", "second"] {
            let mut repo = test_repo(
                &format!("repo:old:{suffix}"),
                &format!("https://example.test/{suffix}"),
                None,
            );
            repo.instance_id = "old".to_string();
            state.store.insert_repo(&repo).unwrap();
        }
        let first_symbol = seed_manifest_and_embedding(&state, "repo:old:first", "first-package");
        let second_symbol =
            seed_manifest_and_embedding(&state, "repo:old:second", "second-package");
        let filemeta_path = nestweaver_engine::sidecar_path(&state.db_path, ".filemeta.json");
        let mut filemeta = nestweaver_engine::load_filemeta_sidecar(&filemeta_path);
        filemeta
            .repos
            .entry("repo:old:first".to_string())
            .or_default();
        nestweaver_engine::save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"repo:old:first":1.0}"#).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let graph_generation = state.store.graph_generation();
        let pagerank_generation = state.store.pagerank_generation();
        let mut reconciliations = 0;

        let error = run_merge_instance_with(
            &state,
            "old",
            "new",
            |store, _from, _to| {
                let first = store.lookup_repo("repo:old:first")?.unwrap();
                delete_repo_cascade(store, &first)?;
                Err(anyhow::anyhow!("injected later repo merge failure"))
            },
            |_state, _mutation, _operation| {
                reconciliations += 1;
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(state.store.graph_generation() > graph_generation);
        let mut generation_path = state.db_path.as_os_str().to_owned();
        generation_path.push(".generation");
        assert_eq!(
            std::fs::read_to_string(std::path::PathBuf::from(generation_path))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap(),
            state.store.graph_generation(),
            "graph generation bump was not persisted"
        );
        assert!(
            !nestweaver_engine::load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key("repo:old:first"),
            "deleted repo sidecar slice survived merge error"
        );
        assert!(!pagerank_path.exists(), "stale PageRank sidecar survived");
        let scores_after = state.store.pagerank_scores();
        assert!(state.store.pagerank_generation() > pagerank_generation);
        assert!(
            !scores_after.contains_key("repo:old:first"),
            "post-error rank query returned the deleted repo"
        );
        assert_eq!(
            reconciliations, 0,
            "code-only merge must not rebuild Tantivy"
        );
        let manifests = nestweaver_engine::load_manifest_cache(&nestweaver_engine::sidecar_path(
            &state.db_path,
            ".manifests.json",
        ))
        .unwrap();
        assert!(!manifests.contains_key("repo:old:first"));
        assert!(manifests.contains_key("repo:old:second"));
        assert!(!state.store.has_embedding(&first_symbol));
        assert!(state.store.has_embedding(&second_symbol));
    }

    #[test]
    fn vault_only_purge_failure_invalidates_live_and_persisted_pagerank() {
        let state = test_state_with_writer();
        state
            .store
            .insert_vault(&nestweaver_schema::Vault {
                uid: "vlt:old:docs".to_string(),
                name: "docs".to_string(),
                root_path: "/missing/docs".to_string(),
                instance_id: "old".to_string(),
            })
            .unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        let persisted_rank = r#"{"rank-sentinel":0.75}"#;
        std::fs::write(&pagerank_path, persisted_rank).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let graph_generation = state.store.graph_generation();
        let pagerank_generation = state.store.pagerank_generation();
        let mut reconciliations = 0;

        let error = run_purge_instance_with(
            &state,
            "old",
            |store, _id| {
                store.delete_vault_cascade("vlt:old:docs")?;
                Err(anyhow::anyhow!("injected late vault purge failure"))
            },
            |_state, _mutation, _operation| {
                reconciliations += 1;
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(state.store.graph_generation() > graph_generation);
        assert!(!pagerank_path.exists());
        assert!(!state.store.pagerank_scores().contains_key("rank-sentinel"));
        assert!(state.store.pagerank_generation() > pagerank_generation);
        assert_eq!(
            reconciliations, 0,
            "an empty vault changes PageRank state but no indexed document rows"
        );
    }

    #[test]
    fn configured_unavailable_search_errors_only_after_indexed_rows_change() {
        let state = SearchIndexReconciliation::Unavailable(
            "configured Tantivy index is corrupt".to_string(),
        );
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open_or_create(&dir.path().join("test.lbug")).unwrap();

        assert!(
            reconcile_search_index(
                &state,
                &store,
                IndexedSearchMutation::Unchanged,
                "code-only",
            )
            .is_ok()
        );
        let error = reconcile_search_index(
            &state,
            &store,
            IndexedSearchMutation::Changed,
            "vault-delete",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("configured Tantivy index is corrupt")
        );
        assert!(
            reconcile_search_index(
                &state,
                &store,
                IndexedSearchMutation::Unknown,
                "vault-delete-unknown",
            )
            .unwrap_err()
            .to_string()
            .contains("configured Tantivy index is corrupt")
        );
        assert!(
            reconcile_search_index(
                &SearchIndexReconciliation::Disabled,
                &store,
                IndexedSearchMutation::Changed,
                "disabled"
            )
            .is_ok()
        );
    }

    #[test]
    fn disabled_search_skips_projection_preflight() {
        let projection = indexed_search_rows_before_with(
            &SearchIndexReconciliation::Disabled,
            || -> Result<std::collections::HashSet<IndexedSearchDocument>, anyhow::Error> {
                panic!("disabled search must not read the graph projection")
            },
        );

        assert!(projection.is_none());
    }

    #[test]
    fn available_search_repairs_after_unknown_preflight_projection() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let tantivy = Arc::new(TantivyIndex::open_or_create(&dir.path().join("tantivy")).unwrap());
        tantivy
            .update_note(
                "note:deleted-before-repair",
                "unknown_projection_repair_sentinel",
                "vlt:deleted-before-repair",
                &["unknown_projection_repair_sentinel".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        assert!(
            !tantivy
                .search("unknown_projection_repair_sentinel", 10)
                .unwrap()
                .is_empty()
        );
        let comparison = indexed_search_mutation(
            Some(Err(anyhow::anyhow!(
                "deterministic preflight projection failure"
            ))),
            &store,
        );

        assert_eq!(comparison, IndexedSearchMutation::Unknown);
        reconcile_search_index(
            &SearchIndexReconciliation::Available(tantivy.clone()),
            &store,
            comparison,
            "unknown_projection_repair",
        )
        .unwrap();
        assert!(
            tantivy
                .search("unknown_projection_repair_sentinel", 10)
                .unwrap()
                .is_empty(),
            "available search must repair after an unknown preflight projection"
        );
    }

    #[test]
    fn indexed_search_fingerprint_ignores_non_tantivy_note_metadata() {
        use nestweaver_schema::{Note, NoteKind, Vault};

        let state = test_state_with_writer();
        let vault_uid = "vlt:fingerprint:metadata";
        let note_uid = "note:fingerprint:metadata";
        state
            .store
            .insert_vault(&Vault {
                uid: vault_uid.to_string(),
                name: "fingerprint metadata".to_string(),
                root_path: "/missing/fingerprint-metadata".to_string(),
                instance_id: "test".to_string(),
            })
            .unwrap();
        let mut note = Note {
            uid: note_uid.to_string(),
            vault_uid: vault_uid.to_string(),
            file_path: "note.md".to_string(),
            title: "Indexed title".to_string(),
            note_kind: NoteKind::General,
            word_count: 1,
            content_hash: "old-hash".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        state.store.insert_note(&note).unwrap();
        state
            .store
            .insert_vault_note_edge(vault_uid, note_uid)
            .unwrap();
        let before = indexed_search_rows(&state.store).unwrap();

        state.store.delete_note_cascade(note_uid).unwrap();
        note.note_kind = NoteKind::Design;
        note.word_count = 99;
        note.content_hash = "new-hash".to_string();
        note.frontmatter = Some("private: metadata".to_string());
        note.modified_at = Some("2026-07-18".to_string());
        note.pagerank_score = Some(0.75);
        state.store.insert_note(&note).unwrap();
        state
            .store
            .insert_vault_note_edge(vault_uid, note_uid)
            .unwrap();

        assert_eq!(before, indexed_search_rows(&state.store).unwrap());
    }

    #[test]
    fn production_startup_preserves_configured_but_writer_unavailable_state() {
        let dir = tempfile::tempdir().unwrap();
        let tantivy_path = dir.path().join("tantivy");
        let _writer = TantivyIndex::open_or_create(&tantivy_path).unwrap();

        let (reader, state) = open_search_index(&tantivy_path, false);

        assert!(reader.is_some(), "reader fallback should remain queryable");
        match state {
            SearchIndexReconciliation::Unavailable(reason) => {
                assert!(reason.contains("writer open failed"));
            }
            _ => panic!("writer lock must remain explicit unavailable mutation state"),
        }
    }

    #[test]
    fn mismatched_uid_merge_error_finalizes_registered_repo() {
        let state = test_state_with_writer();
        let repo_uid = "repo:unexpected-owner:merge";
        let mut repo = test_repo(repo_uid, "https://example.test/mismatched-merge", None);
        repo.instance_id = "old".to_string();
        state.store.insert_repo(&repo).unwrap();
        let filemeta_path = nestweaver_engine::sidecar_path(&state.db_path, ".filemeta.json");
        let mut filemeta = nestweaver_engine::load_filemeta_sidecar(&filemeta_path);
        filemeta.repos.entry(repo_uid.to_string()).or_default();
        nestweaver_engine::save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, format!(r#"{{"{repo_uid}":1.0}}"#)).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let graph_generation = state.store.graph_generation();
        let pagerank_generation = state.store.pagerank_generation();
        let mut reconciliations = 0;

        let error = run_merge_instance_with(
            &state,
            "old",
            "new",
            |store, _from, _to| {
                let repo = store.lookup_repo(repo_uid)?.unwrap();
                delete_repo_cascade(store, &repo)?;
                Err(anyhow::anyhow!("injected mismatched-UID merge failure"))
            },
            |_state, _mutation, _operation| {
                reconciliations += 1;
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(
            !nestweaver_engine::load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(repo_uid),
            "registered mismatched-UID repo sidecar survived merge error"
        );
        assert!(state.store.graph_generation() > graph_generation);
        assert!(!pagerank_path.exists(), "stale PageRank sidecar survived");
        let scores_after = state.store.pagerank_scores();
        assert!(state.store.pagerank_generation() > pagerank_generation);
        assert!(!scores_after.contains_key(repo_uid));
        assert_eq!(
            reconciliations, 0,
            "code-only merge must not rebuild Tantivy"
        );
    }

    #[test]
    fn mismatched_uid_purge_success_finalizes_registered_repo() {
        let state = test_state_with_writer();
        let repo_uid = "repo:unexpected-owner:purge";
        let mut repo = test_repo(repo_uid, "https://example.test/mismatched-purge", None);
        repo.instance_id = "old".to_string();
        state.store.insert_repo(&repo).unwrap();
        let filemeta_path = nestweaver_engine::sidecar_path(&state.db_path, ".filemeta.json");
        let mut filemeta = nestweaver_engine::load_filemeta_sidecar(&filemeta_path);
        filemeta.repos.entry(repo_uid.to_string()).or_default();
        nestweaver_engine::save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, format!(r#"{{"{repo_uid}":1.0}}"#)).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let graph_generation = state.store.graph_generation();
        let pagerank_generation = state.store.pagerank_generation();
        let mut reconciliations = 0;

        let result = run_purge_instance_with(
            &state,
            "old",
            |store, id| store.purge_instance(id).map_err(anyhow::Error::from),
            |_state, _mutation, _operation| {
                reconciliations += 1;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(result.repos, 1, "precondition: registered repo was purged");
        assert!(
            !nestweaver_engine::load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(repo_uid),
            "registered mismatched-UID repo sidecar survived purge"
        );
        assert!(state.store.graph_generation() > graph_generation);
        assert!(!pagerank_path.exists(), "stale PageRank sidecar survived");
        let scores_after = state.store.pagerank_scores();
        assert!(state.store.pagerank_generation() > pagerank_generation);
        assert!(!scores_after.contains_key(repo_uid));
        assert_eq!(
            reconciliations, 0,
            "code-only purge must not rebuild Tantivy"
        );
    }

    #[test]
    fn code_deletion_invalidates_primed_in_memory_pagerank() {
        let state = test_state_with_writer();
        state
            .store
            .insert_repo(&test_repo(
                "repo:test:pagerank",
                "https://example.test/pagerank",
                None,
            ))
            .unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"repo:test:pagerank":1.0}"#).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let before_scores = state.store.pagerank_scores();
        let before_generation = state.store.pagerank_generation();
        assert!(
            before_scores.contains_key("repo:test:pagerank"),
            "precondition: PageRank cache must be primed"
        );

        let repo = state
            .store
            .lookup_repo("repo:test:pagerank")
            .unwrap()
            .unwrap();
        delete_repo_cascade(&state.store, &repo).unwrap();
        finalize_code_graph_deletion(&state, &[repo.uid]);
        let after_scores = state.store.pagerank_scores();

        assert!(state.store.pagerank_generation() > before_generation);
        assert!(!after_scores.contains_key("repo:test:pagerank"));
    }

    #[test]
    fn typed_remove_repo_surfaces_required_generation_persistence_failure() {
        let state = test_state_with_writer();
        let repo_uid = "repo:test:typed-reconciliation-failure";
        state
            .store
            .insert_repo(&test_repo(
                repo_uid,
                "https://example.test/typed-reconciliation-failure",
                None,
            ))
            .unwrap();
        let generation_path = nestweaver_engine::sidecar_path(&state.db_path, ".generation");
        std::fs::create_dir(&generation_path).unwrap();
        let generation_before = state.store.graph_generation();

        let error = run_remove_repo_with(
            &state,
            repo_uid,
            |store, uid| {
                store.clear_repo_derived_nodes(uid).map_err(|error| {
                    Status::internal(format!("clear_repo_derived_nodes failed: {error:#}"))
                })
            },
            |store, uid| {
                store.delete_repo_node(uid).map_err(|error| {
                    Status::internal(format!("delete_repo_node failed: {error:#}"))
                })
            },
        )
        .unwrap();

        // nw-091 / Bug 2: committed delete → success-with-warnings (`error` binds the Ok response).
        assert!(error.committed);
        assert!(
            error
                .reconciliation_failures
                .iter()
                .any(|f| f.stage.contains("generation-persistence")),
            "generation-persistence failure must surface as a warning, got: {:?}",
            error.reconciliation_failures
        );
        assert!(state.store.lookup_repo(repo_uid).unwrap().is_none());
        assert!(state.store.graph_generation() > generation_before);
    }

    #[test]
    fn repo_extension_cleanup_failure_is_retryable_and_preserves_non_graph_keys_on_reopen() {
        let state = test_state_with_writer();
        let repo_uid = "repo:test:extension-liveness-retry";
        state
            .store
            .insert_repo(&test_repo(
                repo_uid,
                "https://example.test/extension-liveness-retry",
                None,
            ))
            .unwrap();
        let extension_path = nestweaver_engine::sidecar_path(&state.db_path, ".extensions.json");
        std::fs::write(&extension_path, b"{not-json").unwrap();

        let error = run_remove_repo_with(
            &state,
            repo_uid,
            |store, uid| {
                store.clear_repo_derived_nodes(uid).map_err(|error| {
                    Status::internal(format!("clear_repo_derived_nodes failed: {error:#}"))
                })
            },
            |store, uid| {
                store.delete_repo_node(uid).map_err(|error| {
                    Status::internal(format!("delete_repo_node failed: {error:#}"))
                })
            },
        )
        .unwrap();

        // nw-091 / Bug 2: a committed delete with a post-commit reconciliation
        // failure returns success-with-warnings, never a bare error. (`error` here
        // now binds the Ok response.)
        assert!(error.committed);
        assert!(
            error
                .reconciliation_failures
                .iter()
                .any(|f| f.stage.contains("extension-metadata")),
            "extension-metadata failure must surface as a warning, got: {:?}",
            error.reconciliation_failures
        );
        assert_eq!(std::fs::read(&extension_path).unwrap(), b"{not-json");
        assert!(state.store.lookup_repo(repo_uid).unwrap().is_none());

        let mut extensions = nestweaver_engine::ExtensionStore::new();
        nestweaver_engine::set_property(
            &mut extensions,
            repo_uid,
            "owner",
            serde_json::json!("stale"),
        );
        nestweaver_engine::set_property(
            &mut extensions,
            "application:release-channel",
            "owner",
            serde_json::json!("keep"),
        );
        nestweaver_engine::save_extensions(&state.db_path, &extensions).unwrap();
        assert!(finalize_code_graph_deletion(&state, &[repo_uid.to_string()]).is_empty());

        let db_path = state.db_path.clone();
        drop(state);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        let visible = nestweaver_engine::load_live_extensions(&reopened, &db_path).unwrap();
        assert!(!visible.contains_key(repo_uid));
        assert_eq!(
            nestweaver_engine::get_property(&visible, "application:release-channel", "owner"),
            Some(&serde_json::json!("keep"))
        );
    }

    #[test]
    fn node_deletion_generation_exhaustion_is_reported_while_pagerank_is_invalidated() {
        let state = test_state_with_writer_generation(Some(u64::MAX));
        let generation_path = nestweaver_engine::sidecar_path(&state.db_path, ".generation");
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let pagerank_generation = state.store.pagerank_generation();

        let failures = finalize_node_graph_deletion(&state, "generation exhaustion regression");

        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].stage,
            nestweaver_engine::DeletionReconciliationStage::GenerationPersistence
        );
        assert!(failures[0].message.contains("exhaust"));
        assert_eq!(state.store.graph_generation(), u64::MAX);
        assert_eq!(
            std::fs::read_to_string(generation_path).unwrap(),
            u64::MAX.to_string()
        );
        assert!(!pagerank_path.exists());
        assert!(state.store.pagerank_scores().is_empty());
        assert!(state.store.pagerank_generation() > pagerank_generation);
    }

    #[test]
    fn remove_project_persists_generation_invalidates_pagerank_and_survives_reopen() {
        let state = test_state_with_writer();
        let project_uid = "proj:test:durable-remove";
        seed_project(&state, project_uid, "Durable remove");
        seed_extension(
            &state,
            project_uid,
            "external_refs",
            serde_json::json!(["ticket-141"]),
        );
        seed_extension(
            &state,
            "proj:test:unrelated-metadata",
            "tags",
            serde_json::json!(["keep"]),
        );
        assert!(state.store.add_embedding(project_uid, vec![1.0, 0.0]));
        state.store.flush_embedding_index().unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, format!(r#"{{"{project_uid}":1.0}}"#)).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = state.store.graph_generation();

        let response = run_remove_project_with(
            &state,
            project_uid,
            |store, uid| store.delete_project_cascade_with_outcome(uid),
            |store, uid| store.project_exists(uid),
            nestweaver_engine::remove_extension_uid_durable,
            finalize_node_graph_deletion,
        )
        .unwrap();

        assert_eq!(response.project_name, "Durable remove");
        assert!(state.store.graph_generation() > generation_before);
        assert!(!pagerank_path.exists());
        assert!(!state.store.pagerank_scores().contains_key(project_uid));
        assert!(!state.store.has_embedding(project_uid));
        let extensions = nestweaver_engine::load_extensions(&state.db_path);
        assert!(
            nestweaver_engine::get_all_properties(&extensions, project_uid).is_empty(),
            "query_extensions UID lookup must not expose removed Project metadata"
        );
        assert_eq!(
            nestweaver_engine::get_property(&extensions, "proj:test:unrelated-metadata", "tags"),
            Some(&serde_json::json!(["keep"])),
            "Project cleanup must preserve unrelated extension entries"
        );

        let db_path = state.db_path.clone();
        let expected_generation = state.store.graph_generation();
        drop(state);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert!(
            reopened
                .list_projects()
                .unwrap()
                .iter()
                .all(|project| project.uid != project_uid)
        );
        assert_eq!(reopened.graph_generation(), expected_generation);
        assert!(!reopened.has_embedding(project_uid));
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(!reopened.pagerank_scores().contains_key(project_uid));
    }

    #[test]
    fn remove_null_name_project_cleans_graph_and_sidecars_durably() {
        use nestweaver_schema::{Note, NoteKind};

        let state = test_state_with_writer();
        let project_uid = "proj:test:null-name-durable-remove";
        let note_uid = "note:null-name-durable-remove";
        state
            .store
            .insert_note(&Note {
                uid: note_uid.to_string(),
                vault_uid: "vlt:null-name-durable-remove".to_string(),
                file_path: "null-name-durable-remove.md".to_string(),
                title: "Null-name durable remove".to_string(),
                note_kind: NoteKind::General,
                word_count: 3,
                content_hash: "null-name-durable-remove-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        {
            let conn = state.store.begin_transaction().unwrap();
            conn.query(
                "CREATE (:Project {uid: 'proj:test:null-name-durable-remove', name: NULL, summary: NULL, instance_id: 'test'})",
            )
            .unwrap();
            state.store.commit_transaction(&conn).unwrap();
        }
        state
            .store
            .batch_insert_project_note_edges(&[(project_uid, note_uid)])
            .unwrap();
        {
            let conn = state.store.begin_transaction().unwrap();
            conn.query(
                "CREATE REL TABLE FUTURE_NULL_PROJECT_EDGE(FROM Project TO Note, marker STRING)",
            )
            .unwrap();
            conn.query(
                "MATCH (p:Project {uid: 'proj:test:null-name-durable-remove'}), \
                 (n:Note {uid: 'note:null-name-durable-remove'}) \
                 CREATE (p)-[:FUTURE_NULL_PROJECT_EDGE {marker: 'future'}]->(n)",
            )
            .unwrap();
            state.store.commit_transaction(&conn).unwrap();
        }
        assert_eq!(
            state
                .store
                .list_projects()
                .unwrap()
                .into_iter()
                .find(|project| project.uid == project_uid)
                .unwrap()
                .name,
            ""
        );
        seed_extension(
            &state,
            project_uid,
            "external_refs",
            serde_json::json!(["ticket-null-141"]),
        );
        assert!(state.store.add_embedding(project_uid, vec![1.0, 0.0]));
        state.store.flush_embedding_index().unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, format!(r#"{{"{project_uid}":1.0}}"#)).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = state.store.graph_generation();

        let response = run_remove_project_with(
            &state,
            project_uid,
            |store, uid| store.delete_project_cascade_with_outcome(uid),
            |store, uid| store.project_exists(uid),
            nestweaver_engine::remove_extension_uid_durable,
            finalize_node_graph_deletion,
        )
        .unwrap();

        assert_eq!(response.project_name, "");
        assert!(!state.store.project_exists(project_uid).unwrap());
        assert!(state.store.graph_generation() > generation_before);
        assert!(!pagerank_path.exists());
        assert!(!state.store.pagerank_scores().contains_key(project_uid));
        assert!(!state.store.has_embedding(project_uid));
        assert!(
            nestweaver_engine::get_all_properties(
                &nestweaver_engine::load_extensions(&state.db_path),
                project_uid,
            )
            .is_empty()
        );
        {
            let conn = state.store.begin_transaction().unwrap();
            for edge_type in ["PROJECT_INCLUDES_NOTE", "FUTURE_NULL_PROJECT_EDGE"] {
                let count = conn
                    .query(&format!("MATCH ()-[r:{edge_type}]->() RETURN r"))
                    .unwrap()
                    .count();
                assert_eq!(count, 0, "{edge_type} survived the Project delete");
            }
            state.store.commit_transaction(&conn).unwrap();
        }

        let db_path = state.db_path.clone();
        let expected_generation = state.store.graph_generation();
        drop(state);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert!(!reopened.project_exists(project_uid).unwrap());
        assert_eq!(reopened.graph_generation(), expected_generation);
        assert!(!reopened.has_embedding(project_uid));
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(!reopened.pagerank_scores().contains_key(project_uid));
        assert!(
            nestweaver_engine::get_all_properties(
                &nestweaver_engine::load_extensions(&db_path),
                project_uid,
            )
            .is_empty()
        );
        let conn = reopened.begin_transaction().unwrap();
        for edge_type in ["PROJECT_INCLUDES_NOTE", "FUTURE_NULL_PROJECT_EDGE"] {
            let count = conn
                .query(&format!("MATCH ()-[r:{edge_type}]->() RETURN r"))
                .unwrap()
                .count();
            assert_eq!(count, 0, "{edge_type} reappeared after reopen");
        }
        reopened.commit_transaction(&conn).unwrap();
    }

    #[test]
    fn missing_project_is_a_true_noop_without_finalization() {
        let state = test_state_with_writer();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"still-valid":1.0}"#).unwrap();
        let generation_before = state.store.graph_generation();
        let finalized = std::cell::Cell::new(false);

        let response = run_remove_project_with(
            &state,
            "proj:test:missing",
            |store, uid| store.delete_project_cascade_with_outcome(uid),
            |store, uid| store.project_exists(uid),
            nestweaver_engine::remove_extension_uid_durable,
            |_state, _operation| {
                finalized.set(true);
                Vec::new()
            },
        )
        .unwrap();

        assert_eq!(response.project_name, "");
        assert!(!finalized.get());
        assert_eq!(state.store.graph_generation(), generation_before);
        assert!(pagerank_path.exists());
    }

    #[tokio::test]
    async fn removed_project_metadata_is_absent_from_query_extensions_rpc() {
        let state = test_state_with_writer();
        let project_uid = "proj:test:query-extensions";
        seed_project(&state, project_uid, "Query extensions");
        seed_extension(
            &state,
            project_uid,
            "external_refs",
            serde_json::json!(["visible-before-delete"]),
        );
        run_remove_project_with(
            &state,
            project_uid,
            |store, uid| store.delete_project_cascade_with_outcome(uid),
            |store, uid| store.project_exists(uid),
            nestweaver_engine::remove_extension_uid_durable,
            finalize_node_graph_deletion,
        )
        .unwrap();

        let stale_repo_uid = "repo:test:stale-query-extension";
        seed_extension(
            &state,
            stale_repo_uid,
            "owner",
            serde_json::json!("must-not-leak"),
        );

        let service = DaemonService::new(state);
        let response = service
            .query_extensions(Request::new(JsonRequest {
                args_json: serde_json::json!({"uid": project_uid}).to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        let result: serde_json::Value = serde_json::from_str(&response.result_json).unwrap();

        assert_eq!(result["uid"], project_uid);
        assert_eq!(result["properties"], serde_json::json!({}));

        let stale_response = service
            .query_extensions(Request::new(JsonRequest {
                args_json: serde_json::json!({"uid": stale_repo_uid}).to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        let stale_result: serde_json::Value =
            serde_json::from_str(&stale_response.result_json).unwrap();
        assert_eq!(stale_result["properties"], serde_json::json!({}));

        let filtered_response = service
            .query_extensions(Request::new(JsonRequest {
                args_json: serde_json::json!({
                    "key": "owner",
                    "value": "must-not-leak"
                })
                .to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        let filtered_result: serde_json::Value =
            serde_json::from_str(&filtered_response.result_json).unwrap();
        assert_eq!(filtered_result["count"], 0);
    }

    #[test]
    fn remove_project_surfaces_generation_exhaustion_after_committed_delete() {
        let state = test_state_with_writer_generation(Some(u64::MAX));
        let project_uid = "proj:test:exhausted-remove";
        seed_project(&state, project_uid, "Exhausted remove");
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();

        let error = run_remove_project_with(
            &state,
            project_uid,
            |store, uid| store.delete_project_cascade_with_outcome(uid),
            |store, uid| store.project_exists(uid),
            nestweaver_engine::remove_extension_uid_durable,
            finalize_node_graph_deletion,
        )
        .unwrap();

        // nw-091 / Bug 2: committed delete → success-with-warnings (`error` binds the Ok response).
        assert!(error.committed);
        assert!(
            error
                .reconciliation_failures
                .iter()
                .any(
                    |f| f.stage.contains("generation-persistence") || f.message.contains("exhaust")
                ),
            "generation-persistence exhaustion must surface as a warning, got: {:?}",
            error.reconciliation_failures
        );
        assert!(
            state
                .store
                .list_projects()
                .unwrap()
                .iter()
                .all(|project| project.uid != project_uid)
        );
        assert!(!pagerank_path.exists());
    }

    #[test]
    fn remove_project_surfaces_generation_save_and_pagerank_unlink_failures() {
        for failure in ["generation", "pagerank"] {
            let state = test_state_with_writer();
            let project_uid = format!("proj:test:{failure}-failure");
            seed_project(&state, &project_uid, failure);
            let generation_path = nestweaver_engine::sidecar_path(&state.db_path, ".generation");
            let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
            if failure == "generation" {
                if generation_path.exists() {
                    std::fs::remove_file(&generation_path).unwrap();
                }
                std::fs::create_dir(&generation_path).unwrap();
            } else {
                std::fs::create_dir(&pagerank_path).unwrap();
            }

            let error = run_remove_project_with(
                &state,
                &project_uid,
                |store, uid| store.delete_project_cascade_with_outcome(uid),
                |store, uid| store.project_exists(uid),
                nestweaver_engine::remove_extension_uid_durable,
                finalize_node_graph_deletion,
            )
            .unwrap();

            // nw-091 / Bug 2: committed delete → success-with-warnings (`error` binds the Ok response).
            assert!(error.committed);
            let expected_stage = if failure == "generation" {
                "generation-persistence"
            } else {
                "persisted-pagerank"
            };
            assert!(
                error
                    .reconciliation_failures
                    .iter()
                    .any(|f| f.stage.contains(expected_stage)),
                "unexpected {failure} reconciliation: {:?}",
                error.reconciliation_failures
            );
            assert!(
                state
                    .store
                    .list_projects()
                    .unwrap()
                    .iter()
                    .all(|project| project.uid != project_uid)
            );
        }
    }

    #[test]
    fn remove_project_aggregates_graph_and_finalizer_errors() {
        let state = test_state_with_writer();
        let error = run_remove_project_with(
            &state,
            "proj:test:aggregate-errors",
            |_store, _uid| {
                Err(project_delete_error(
                    "proj:test:aggregate-errors",
                    None,
                    nestweaver_store::ProjectMutationDisposition::Ambiguous,
                    "injected graph mutation failure",
                ))
            },
            |_store, _uid| Ok(false),
            |_db_path, _uid| Ok(false),
            |_state, _operation| {
                vec![nestweaver_engine::DeletionReconciliationFailure {
                    stage: nestweaver_engine::DeletionReconciliationStage::PersistedPageRank,
                    repo_uid: None,
                    message: "injected durable unlink failure".to_string(),
                }]
            },
        )
        .unwrap_err();

        assert!(error.message().contains("injected graph mutation failure"));
        assert!(error.message().contains("persisted-pagerank"));
        assert!(error.message().contains("injected durable unlink failure"));
    }

    #[test]
    fn confirmed_project_rollback_does_not_reconcile_or_mutate_sidecars() {
        let state = test_state_with_writer();
        let project_uid = "proj:test:confirmed-rollback";
        seed_project(&state, project_uid, "Confirmed rollback");
        seed_extension(
            &state,
            project_uid,
            "aliases",
            serde_json::json!(["still-live"]),
        );
        assert!(state.store.add_embedding(project_uid, vec![1.0, 0.0]));
        state.store.flush_embedding_index().unwrap();
        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, format!(r#"{{"{project_uid}":1.0}}"#)).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = state.store.graph_generation();
        let pagerank_generation_before = state.store.pagerank_generation();
        let liveness_called = std::cell::Cell::new(false);
        let cleanup_called = std::cell::Cell::new(false);
        let finalizer_called = std::cell::Cell::new(false);

        let error = run_remove_project_with(
            &state,
            project_uid,
            |_store, uid| {
                Err(project_delete_error(
                    uid,
                    Some("Confirmed rollback"),
                    nestweaver_store::ProjectMutationDisposition::ConfirmedRolledBack,
                    "injected DETACH failure followed by rollback",
                ))
            },
            |_store, _uid| {
                liveness_called.set(true);
                Ok(true)
            },
            |_db_path, _uid| {
                cleanup_called.set(true);
                Ok(false)
            },
            |_state, _operation| {
                finalizer_called.set(true);
                Vec::new()
            },
        )
        .unwrap_err();

        assert!(error.message().contains("ConfirmedRolledBack"));
        assert!(!liveness_called.get());
        assert!(!cleanup_called.get());
        assert!(!finalizer_called.get());
        assert_eq!(state.store.graph_generation(), generation_before);
        assert_eq!(
            state.store.pagerank_generation(),
            pagerank_generation_before
        );
        assert!(pagerank_path.exists());
        assert!(state.store.has_embedding(project_uid));
        assert!(
            !nestweaver_engine::get_all_properties(
                &nestweaver_engine::load_extensions(&state.db_path),
                project_uid,
            )
            .is_empty()
        );
    }

    #[test]
    fn confirmed_unchanged_project_error_does_not_run_reconciliation() {
        let state = test_state_with_writer();
        let project_uid = "proj:test:confirmed-unchanged";
        seed_project(&state, project_uid, "Confirmed unchanged");
        seed_extension(
            &state,
            project_uid,
            "tags",
            serde_json::json!(["still-live"]),
        );
        let generation_before = state.store.graph_generation();
        let cleanup_called = std::cell::Cell::new(false);
        let finalizer_called = std::cell::Cell::new(false);

        let error = run_remove_project_with(
            &state,
            project_uid,
            |_store, uid| {
                Err(project_delete_error(
                    uid,
                    None,
                    nestweaver_store::ProjectMutationDisposition::ConfirmedUnchanged,
                    "injected lookup failure",
                ))
            },
            |_store, _uid| panic!("confirmed unchanged must not query liveness"),
            |_db_path, _uid| {
                cleanup_called.set(true);
                Ok(false)
            },
            |_state, _operation| {
                finalizer_called.set(true);
                Vec::new()
            },
        )
        .unwrap_err();

        assert!(error.message().contains("ConfirmedUnchanged"));
        assert!(!cleanup_called.get());
        assert!(!finalizer_called.get());
        assert_eq!(state.store.graph_generation(), generation_before);
        assert!(state.store.project_exists(project_uid).unwrap());
        assert!(nestweaver_engine::load_extensions(&state.db_path).contains_key(project_uid));
    }

    #[test]
    fn ambiguous_project_delete_reconciles_and_uses_graph_liveness_for_extensions() {
        for graph_present in [false, true] {
            let state = test_state_with_writer();
            let project_uid = format!("proj:test:ambiguous-{graph_present}");
            if graph_present {
                seed_project(&state, &project_uid, "Ambiguous present");
            }
            seed_extension(
                &state,
                &project_uid,
                "features",
                serde_json::json!(["flag"]),
            );
            let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
            std::fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
            let generation_before = state.store.graph_generation();

            let error = run_remove_project_with(
                &state,
                &project_uid,
                |_store, uid| {
                    Err(project_delete_error(
                        uid,
                        Some("Ambiguous"),
                        nestweaver_store::ProjectMutationDisposition::Ambiguous,
                        "injected ambiguous commit result",
                    ))
                },
                |store, uid| store.project_exists(uid),
                nestweaver_engine::remove_extension_uid_durable,
                finalize_node_graph_deletion,
            )
            .unwrap_err();

            assert!(error.message().contains("injected ambiguous commit result"));
            assert!(state.store.graph_generation() > generation_before);
            assert!(!pagerank_path.exists());
            assert_eq!(
                nestweaver_engine::load_extensions(&state.db_path).contains_key(&project_uid),
                graph_present,
                "extension liveness decision disagreed with graph state"
            );
        }
    }

    #[test]
    fn ambiguous_liveness_failure_preserves_extensions_and_aggregates_error() {
        let state = test_state_with_writer();
        let project_uid = "proj:test:liveness-failure";
        seed_extension(&state, project_uid, "tags", serde_json::json!(["preserve"]));

        let error = run_remove_project_with(
            &state,
            project_uid,
            |_store, uid| {
                Err(project_delete_error(
                    uid,
                    None,
                    nestweaver_store::ProjectMutationDisposition::Ambiguous,
                    "ambiguous delete",
                ))
            },
            |_store, _uid| {
                Err(nestweaver_store::StoreError::Query(
                    "injected liveness query failure".to_string(),
                ))
            },
            nestweaver_engine::remove_extension_uid_durable,
            finalize_node_graph_deletion,
        )
        .unwrap_err();

        assert!(error.message().contains("ambiguous delete"));
        assert!(error.message().contains("graph-liveness"));
        assert!(error.message().contains("injected liveness query failure"));
        assert!(
            nestweaver_engine::load_extensions(&state.db_path).contains_key(project_uid),
            "metadata must be preserved when graph liveness is unknown"
        );
    }

    #[test]
    fn project_extension_cleanup_failure_is_retryable_and_survives_reopen() {
        let state = test_state_with_writer();
        let project_uid = "proj:test:extension-retry";
        seed_project(&state, project_uid, "Extension retry");
        seed_extension(
            &state,
            project_uid,
            "external_refs",
            serde_json::json!(["retry-me"]),
        );

        let error = run_remove_project_with(
            &state,
            project_uid,
            |store, uid| store.delete_project_cascade_with_outcome(uid),
            |store, uid| store.project_exists(uid),
            |_db_path, _uid| anyhow::bail!("injected extension cleanup failure"),
            finalize_node_graph_deletion,
        )
        .unwrap();

        // nw-091 / Bug 2: committed delete → success-with-warnings (`error` binds the Ok response).
        assert!(error.committed);
        assert!(
            error
                .reconciliation_failures
                .iter()
                .any(|f| f.stage.contains("extension-metadata")
                    || f.message.contains("injected extension cleanup failure")),
            "extension-metadata failure must surface as a warning, got: {:?}",
            error.reconciliation_failures
        );
        assert!(nestweaver_engine::load_extensions(&state.db_path).contains_key(project_uid));

        let finalized_on_retry = std::cell::Cell::new(false);
        let response = run_remove_project_with(
            &state,
            project_uid,
            |store, uid| store.delete_project_cascade_with_outcome(uid),
            |store, uid| store.project_exists(uid),
            nestweaver_engine::remove_extension_uid_durable,
            |_state, _operation| {
                finalized_on_retry.set(true);
                Vec::new()
            },
        )
        .unwrap();

        assert_eq!(response.project_name, "");
        assert!(!finalized_on_retry.get());
        assert!(!nestweaver_engine::load_extensions(&state.db_path).contains_key(project_uid));
        let db_path = state.db_path.clone();
        drop(state);
        assert!(!nestweaver_engine::load_extensions(&db_path).contains_key(project_uid));
    }

    #[test]
    fn remove_repo_late_failure_finalizes_committed_children() {
        let state = test_state_with_writer();
        let repo_uid = "repo:test:late-remove";
        let file_uid = nestweaver_schema::file_uid(repo_uid, "src/lib.rs");
        state
            .store
            .insert_repo(&test_repo(
                repo_uid,
                "https://example.test/late-remove",
                None,
            ))
            .unwrap();
        state
            .store
            .insert_file(&nestweaver_schema::File {
                uid: file_uid.clone(),
                path: "src/lib.rs".to_string(),
                repo_uid: repo_uid.to_string(),
                content_hash: "hash".to_string(),
            })
            .unwrap();
        let removed_symbol =
            seed_manifest_and_embedding(&state, repo_uid, "partial-remove-package");

        let filemeta_path = nestweaver_engine::sidecar_path(&state.db_path, ".filemeta.json");
        let mut filemeta = nestweaver_engine::load_filemeta_sidecar(&filemeta_path);
        filemeta.repos.entry(repo_uid.to_string()).or_default();
        nestweaver_engine::save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();
        let deps_path = nestweaver_engine::sidecar_path(&state.db_path, ".resolution_deps.bin");
        let mut deps = nestweaver_engine::resolution_cache::ResolutionDeps::default();
        deps.set_deps_for_repo(
            repo_uid,
            "src/lib.rs",
            ["src/dep.rs".to_string()].into_iter().collect(),
        );
        deps.save(&deps_path).unwrap();

        let pagerank_path = nestweaver_engine::sidecar_path(&state.db_path, ".pagerank.json");
        std::fs::write(&pagerank_path, format!(r#"{{"{file_uid}":1.0}}"#)).unwrap();
        state.store.load_pagerank_cache(&pagerank_path).unwrap();
        let tantivy = state.tantivy.as_ref().unwrap();
        tantivy
            .update_note(
                "note:late-remove",
                "late_remove_search_sentinel",
                "vault:test",
                &["late_remove_search_sentinel".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        let generation_before = state.store.graph_generation();

        let error = run_remove_repo_with(
            &state,
            repo_uid,
            |_store, _uid| Err(Status::internal("injected derived-node failure")),
            |_store, _uid| Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(
            state.store.list_files_by_repo(repo_uid).unwrap().is_empty(),
            "precondition: the first delete transaction must have committed"
        );
        assert!(
            state.store.lookup_repo(repo_uid).unwrap().is_some(),
            "precondition: the injected late failure leaves the Repo row"
        );
        assert!(state.store.graph_generation() > generation_before);
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
        assert!(!state.store.pagerank_scores().contains_key(&file_uid));
        assert!(
            !tantivy
                .search("late_remove_search_sentinel", 10)
                .unwrap()
                .is_empty(),
            "code-only remove must not rebuild unrelated vault search documents"
        );
        assert!(
            nestweaver_engine::load_manifest_cache(&nestweaver_engine::sidecar_path(
                &state.db_path,
                ".manifests.json"
            ))
            .unwrap()
            .contains_key(repo_uid),
            "the live Repo row must retain its manifest after a partial delete"
        );
        assert!(
            !state.store.has_embedding(&removed_symbol),
            "the committed Symbol deletion must remove its embedding"
        );
    }

    #[test]
    fn remove_repo_bulk_error_finalizes_ambiguous_partial_deletion_extensions() {
        let state = test_state_with_writer();
        let repo_uid = "repo:test:bulk-error";
        let file_uid = nestweaver_schema::file_uid(repo_uid, "src/lib.rs");
        state
            .store
            .insert_repo(&test_repo(
                repo_uid,
                "https://example.test/bulk-error",
                None,
            ))
            .unwrap();
        state
            .store
            .insert_file(&nestweaver_schema::File {
                uid: file_uid.clone(),
                path: "src/lib.rs".to_string(),
                repo_uid: repo_uid.to_string(),
                content_hash: "hash".to_string(),
            })
            .unwrap();
        seed_extension(&state, repo_uid, "owner", serde_json::json!("keep-live"));
        seed_extension(&state, &file_uid, "owner", serde_json::json!("remove-dead"));
        let generation_before = state.store.graph_generation();

        let error = run_remove_repo_with_bulk(
            &state,
            repo_uid,
            |store, uid| {
                store.bulk_delete_repo_files_and_symbols(uid).unwrap();
                Err(Status::internal(
                    "injected ambiguous error after bulk commit",
                ))
            },
            |_store, _uid| panic!("later stages must not run after bulk error"),
            |_store, _uid| panic!("later stages must not run after bulk error"),
        )
        .unwrap_err();

        assert!(
            error
                .message()
                .contains("ambiguous error after bulk commit")
        );
        assert!(state.store.graph_generation() > generation_before);
        assert!(state.store.lookup_repo(repo_uid).unwrap().is_some());
        assert!(state.store.list_files_by_repo(repo_uid).unwrap().is_empty());
        let extensions = nestweaver_engine::load_extensions(&state.db_path);
        assert!(extensions.contains_key(repo_uid));
        assert!(!extensions.contains_key(&file_uid));
    }

    /// The gRPC mutating-tool gate MUST reference the single shared
    /// `MUTATING_TOOLS` list defined in `nestweaver-mcp`, not a private copy
    /// that can drift from the HTTP/MCP gate. This asserts the daemon reads the
    /// shared const (it won't compile if the daemon reintroduces a private one)
    /// and pins the known set so adding a mutating tool is a deliberate edit.
    #[test]
    fn mutating_tools_gate_uses_shared_const() {
        assert_eq!(
            nestweaver_mcp::http::MUTATING_TOOLS,
            &[
                "brain_add_source",
                "brain_remove_source",
                "brain_memory_consolidate",
                "set_extension",
                "prune_stale",
            ]
        );
        // The daemon's gate is this exact const, not a copy.
        assert!(std::ptr::eq(
            MUTATING_TOOLS.as_ptr(),
            nestweaver_mcp::http::MUTATING_TOOLS.as_ptr(),
        ));
    }

    /// Two co-located replicas — distinct `--db` files under a shared parent
    /// dir — must get DISTINCT private working directories. Before the fix the
    /// working dir was `<parent>/replica-work` (keyed only on the parent), so
    /// both replicas shared one mutable path and the second boot's in-place
    /// `fs::copy` truncated the first replica's open working copy.
    #[test]
    fn co_located_replicas_use_distinct_working_dirs() {
        let parent = Path::new("/data");
        let db_a = parent.join("a.lbug");
        let db_b = parent.join("b.lbug");

        let id_a = lifecycle::instance_id_from_db_path(&db_a);
        let id_b = lifecycle::instance_id_from_db_path(&db_b);
        assert_ne!(
            id_a, id_b,
            "distinct --db paths must yield distinct instance ids"
        );

        let work_a = replica_working_dir(&db_a, &id_a);
        let work_b = replica_working_dir(&db_b, &id_b);

        assert_ne!(
            work_a, work_b,
            "co-located replicas with distinct --db must not share a working dir"
        );
        // Both stay under the shared parent (the snapshot/db live there) …
        assert_eq!(work_a.parent(), Some(parent));
        assert_eq!(work_b.parent(), Some(parent));
        // … but neither is the old collision-prone shared path.
        assert_ne!(work_a, parent.join("replica-work"));
        assert_ne!(work_b, parent.join("replica-work"));
    }

    /// The instance lock is exclusive: while one holder keeps the pidfile flock,
    /// a second claim on the SAME pidfile is refused — this is what stops a
    /// duplicate same-`--db` replica from proceeding to materialize (and
    /// truncate a live sibling's working copy). Releasing the first holder frees
    /// the lock for a fresh claim.
    #[cfg(unix)]
    #[test]
    fn claim_pidfile_lock_is_exclusive_until_released() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pid_path = tmp.path().join("rt").join("daemon.pid");

        let first = claim_pidfile_lock(&pid_path).expect("first claim should acquire the lock");

        // A second claim on the same pidfile — a duplicate instance — is refused.
        let err = claim_pidfile_lock(&pid_path)
            .expect_err("a second claim must be refused while the lock is held");
        assert!(
            err.to_string().contains("already running"),
            "err was: {err}"
        );

        // Release the first holder; the lock is now free to claim again.
        drop(first);
        claim_pidfile_lock(&pid_path).expect("claim should succeed once the lock is released");
    }

    /// A reachable ref resolves to a 40-char SHA.
    #[test]
    fn probe_remote_sha_returns_sha_for_existing_ref() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("src");
        init_repo(&repo);
        let url = format!("file://{}", repo.display());

        let sha = probe_remote_sha(&[], &url, "HEAD")
            .expect("ls-remote against a valid repo should succeed")
            .expect("HEAD should advertise a SHA");
        assert_eq!(sha.len(), 40, "should be a full SHA, got {sha:?}");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// A ref the remote doesn't advertise is `Ok(None)` — successful probe,
    /// nothing to enqueue — NOT an error.
    #[test]
    fn probe_remote_sha_returns_none_for_missing_ref() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("src");
        init_repo(&repo);
        let url = format!("file://{}", repo.display());

        let result = probe_remote_sha(&[], &url, "refs/heads/does-not-exist")
            .expect("ls-remote itself should still succeed for an unknown ref");
        assert!(
            result.is_none(),
            "an unadvertised ref must be None, not a SHA"
        );
    }

    /// A failing ls-remote (non-zero exit — e.g. an unreachable repo) must be an
    /// `Err`, NOT `Ok(None)`. This is the bug: without a status check, git's
    /// empty stdout on failure is indistinguishable from "no new commit".
    #[test]
    fn probe_remote_sha_errors_on_git_failure() {
        // A path that isn't a git repo → `git ls-remote` exits non-zero with
        // empty stdout.
        let tmp = tempfile::TempDir::new().unwrap();
        let bogus = format!("file://{}/not-a-repo", tmp.path().display());

        let result = probe_remote_sha(&[], &bogus, "HEAD");
        assert!(
            result.is_err(),
            "a non-zero git exit must be an error, not Ok(None): {result:?}"
        );
    }

    /// Dropping the request-future scope (client cancel / disconnect) must trip
    /// the shared cooperative cancel flag, so in-flight `spawn_blocking` work
    /// bails instead of running to completion for a caller that is gone.
    #[tokio::test]
    async fn cancelled_flag_trips_on_request_future_drop() {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let _disconnect_guard = arm_disconnect_cancel(cancel.clone());
            // Still armed: nothing has been dropped yet.
            assert!(!cancel.load(Ordering::Relaxed), "flag must start unset");
            // guard drops here → token cancelled → listener stores true
        }
        // The listener runs on a spawned task; poll briefly for it to observe.
        for _ in 0..200 {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            cancel.load(Ordering::Relaxed),
            "dropping the request-future guard must trip the cancel flag"
        );
    }

    #[test]
    fn validate_token_lengths_rejects_short_token() {
        let short = Some("abcd".to_string());
        assert!(validate_token_lengths(&short, &None).is_err());
        assert!(validate_token_lengths(&None, &short).is_err());
    }

    #[test]
    fn validate_token_lengths_accepts_min_length() {
        let tok = Some("a".repeat(MIN_TOKEN_LEN));
        assert_eq!(tok.as_ref().unwrap().len(), MIN_TOKEN_LEN);
        assert!(validate_token_lengths(&tok, &None).is_ok());
    }

    #[test]
    fn validate_token_lengths_rejects_equal_auth_and_admin() {
        // A query (auth) token equal to the admin token collapses the privilege
        // boundary: admin access is granted on admin-token match, so an identical
        // query token would make every query-token holder an admin.
        let tok = Some("a".repeat(MIN_TOKEN_LEN));
        let other = Some("b".repeat(MIN_TOKEN_LEN));
        assert!(validate_token_lengths(&tok, &tok).is_err());
        // Distinct tokens of valid length remain acceptable.
        assert!(validate_token_lengths(&tok, &other).is_ok());
    }

    #[test]
    fn validate_token_lengths_accepts_none() {
        assert!(validate_token_lengths(&None, &None).is_ok());
    }

    #[test]
    fn validate_token_lengths_rejects_admin_without_auth() {
        // An admin token alone disables auth entirely (the auth layer is only
        // installed with a query token), so it must be rejected at startup.
        let admin = Some("a".repeat(MIN_TOKEN_LEN));
        assert!(validate_token_lengths(&None, &admin).is_err());
        // But a query token alone (read-gated, mutations denied) is fine.
        let auth = Some("q".repeat(MIN_TOKEN_LEN));
        assert!(validate_token_lengths(&auth, &None).is_ok());
    }

    #[test]
    fn is_unsafe_index_root_rejects_system_roots_but_allows_real_repos() {
        use std::path::Path;
        for bad in [
            "", "/", "/Users", "/System", "/Library", "/private", "/tmp", "/Volumes", "/dev",
            "/usr", "/etc",
        ] {
            assert!(
                is_unsafe_index_root(Path::new(bad)),
                "{bad:?} is a system root and must be refused"
            );
        }
        for ok in [
            "/home/user/dev/myrepo",
            "/tmp/ppi2/repo",
            "/private/tmp/abc/project",
            "/var/folders/xx/y/T/repo",
        ] {
            assert!(
                !is_unsafe_index_root(Path::new(ok)),
                "{ok:?} is a specific repo path and must be allowed"
            );
        }
    }

    #[test]
    fn is_unsafe_index_root_handles_macos_firmlink_and_case() {
        use std::path::Path;
        for bad in [
            // The real data-volume root on modern macOS (a firmlink) — refuse.
            "/System/Volumes",
            "/System/Volumes/Data",
            // APFS is case-insensitive but `canonicalize` does NOT normalize
            // case, so wrong-case system roots must still be refused.
            "/users",
            "/USERS",
            "/system",
            "/Var",
            "/VOLUMES",
            "/System/Volumes/data",
        ] {
            assert!(
                is_unsafe_index_root(Path::new(bad)),
                "{bad:?} is a system root and must be refused"
            );
        }
        // A real repo whose deep path merely CONTAINS a dangerous component
        // name must still be allowed (exact-match only, not substring).
        for ok in [
            "/home/user/dev/System",
            "/home/user/dev/Volumes/app",
            "/private/tmp/x/Data",
        ] {
            assert!(
                !is_unsafe_index_root(Path::new(ok)),
                "{ok:?} is a specific repo path and must be allowed"
            );
        }
    }

    #[test]
    fn idle_requires_no_active_work_or_indexing() {
        assert!(is_idle(0, false), "no active work and not indexing is idle");
        assert!(!is_idle(1, false), "active read/write blocks idle");
        // An in-flight index job bumps `indexing_active`, not `active_writes`,
        // so it must independently block an idle shutdown.
        assert!(!is_idle(0, true), "an in-flight index blocks idle shutdown");
    }

    #[test]
    fn validate_bind_security_requires_tls_for_non_loopback() {
        let tok = Some("a".repeat(MIN_TOKEN_LEN));
        let cert = Some(std::path::PathBuf::from("/tmp/cert.pem"));
        let key = Some(std::path::PathBuf::from("/tmp/key.pem"));

        // Non-loopback bind with an auth token but NO TLS must be rejected:
        // bearer tokens and source would travel in cleartext.
        assert!(validate_bind_security("0.0.0.0:9378", &tok, &None, &None, false).is_err());
        assert!(validate_bind_security("0.0.0.0:9378", &tok, &cert, &None, false).is_err());

        // Non-loopback bind with auth token AND TLS is acceptable.
        assert!(validate_bind_security("0.0.0.0:9378", &tok, &cert, &key, false).is_ok());
    }

    #[test]
    fn validate_bind_security_requires_auth_for_non_loopback() {
        let cert = Some(std::path::PathBuf::from("/tmp/cert.pem"));
        let key = Some(std::path::PathBuf::from("/tmp/key.pem"));
        // Preserves the pre-existing invariant: non-loopback requires auth even with TLS.
        assert!(validate_bind_security("0.0.0.0:9378", &None, &cert, &key, false).is_err());
    }

    #[test]
    fn acme_provides_tls_reflects_build_feature() {
        // A domain request only counts as TLS when the `acme` feature is compiled
        // in; without it, ACME cannot provision a cert.
        assert_eq!(acme_provides_tls(true), cfg!(feature = "acme"));
        // No domain requested is never TLS regardless of the build.
        assert!(!acme_provides_tls(false));
    }

    #[test]
    fn acme_bind_gate_fails_closed_without_feature() {
        // B2: a non-loopback bind that relies on ACME for TLS must be REFUSED when
        // the binary was compiled without the `acme` feature (fail closed), and
        // ACCEPTED when the feature is present (ACME provides TLS). Drive the gate
        // with the effective, build-aware ACME flag exactly as the call site does.
        let tok = Some("a".repeat(MIN_TOKEN_LEN));
        let effective_acme = acme_provides_tls(/* domain present */ true);
        let result = validate_bind_security("0.0.0.0:9378", &tok, &None, &None, effective_acme);
        if cfg!(feature = "acme") {
            assert!(
                result.is_ok(),
                "with the acme feature, ACME counts as TLS for a non-loopback bind"
            );
        } else {
            assert!(
                result.is_err(),
                "without the acme feature, an ACME-only non-loopback bind must fail closed"
            );
        }
    }

    #[test]
    fn validate_bind_security_treats_acme_as_tls() {
        let tok = Some("a".repeat(MIN_TOKEN_LEN));
        // ACME provisions a trusted cert at runtime, so a non-loopback bind with
        // a token and ACME enabled — but no static --tls-cert/--tls-key — is OK.
        assert!(validate_bind_security("0.0.0.0:9378", &tok, &None, &None, true).is_ok());
        // ...but ACME does not waive the auth requirement.
        assert!(validate_bind_security("0.0.0.0:9378", &None, &None, &None, true).is_err());
    }

    #[test]
    fn acme_bootstrap_failure_never_serves_plaintext() {
        // SECURITY (B1): when ACME provisioning fails on a NON-loopback bind, the
        // fallback decision must be a TLS acceptor (self-signed interim) — never a
        // `None` "serve plaintext" state. Encryption is preserved even though the
        // interim cert is untrusted.
        let non_loopback = acme_failure_fallback_acceptor("example.com", false)
            .expect("non-loopback ACME failure must produce a TLS acceptor, not error");
        assert!(
            non_loopback.is_some(),
            "non-loopback ACME failure must fall back to TLS, never cleartext"
        );

        // Loopback is process-local, so plaintext is acceptable there (matches the
        // pre-existing loopback fast path): the decision is `None`.
        let loopback = acme_failure_fallback_acceptor("example.com", true)
            .expect("loopback fallback decision must not error");
        assert!(
            loopback.is_none(),
            "loopback ACME failure may serve plaintext (process-local)"
        );
    }

    #[tokio::test]
    async fn tls_handshake_times_out_on_silent_client() {
        // B3: a client that opens a TCP connection but never sends a ClientHello
        // must NOT block the accept loop — the handshake times out (Ok(None)) so
        // the connection is dropped and new connections keep flowing.
        let acceptor = build_self_signed_acceptor(&["localhost".to_string()])
            .expect("build self-signed acceptor");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_tls_with_timeout(&acceptor, stream, Duration::from_millis(200)).await
        });

        // Connect but send nothing — no TLS ClientHello ever arrives.
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();

        let result = server.await.unwrap();
        assert!(
            matches!(result, Ok(None)),
            "a silent client must time out (Ok(None)), got {result:?}"
        );
        drop(client);
    }

    #[tokio::test]
    async fn handshake_concurrency_is_capped() {
        // B3 follow-up: each in-flight handshake holds a semaphore permit for its
        // whole duration, so N stalled (silent) handshakes exhaust an N-permit
        // pool — the accept loop cannot spawn an (N+1)th until one releases. This
        // caps the self-inflicted DoS of unbounded per-connection tasks.
        use std::sync::Arc;
        use tokio::sync::{Semaphore, mpsc};

        let acceptor = build_self_signed_acceptor(&["localhost".to_string()]).unwrap();
        let sem = Arc::new(Semaphore::new(2));
        let (tx, _rx) = mpsc::channel(8);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Occupy both permits with silent clients (never send a ClientHello).
        let mut handles = Vec::new();
        let mut clients = Vec::new();
        for _ in 0..2 {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let client = tokio::net::TcpStream::connect(addr).await.unwrap();
            clients.push(client);
            let (server_stream, _) = listener.accept().await.unwrap();
            handles.push(tokio::spawn(drive_capped_handshake(
                permit,
                acceptor.clone(),
                server_stream,
                Duration::from_millis(300),
                tx.clone(),
                "test",
            )));
        }

        // Both permits are held while the handshakes are in flight: the cap is
        // reached, so no further permit is available right now.
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "in-flight handshakes must hold the permits (cap reached)"
        );

        // Once the handshakes time out, their permits are returned to the pool.
        for h in handles {
            let _ = h.await;
        }
        assert_eq!(
            sem.available_permits(),
            2,
            "permits must be released after handshakes finish"
        );
        drop(clients);
    }

    #[tokio::test]
    async fn tls_handshake_completes_for_valid_client() {
        // Sanity: a real TLS client completes the handshake well within the
        // timeout, so the timeout does not reject legitimate connections.
        use tokio_rustls::TlsConnector;

        let bundle =
            nestweaver_engine::tls::generate_tls_bundle(&["localhost".to_string()], 30, false)
                .unwrap();
        // Build the acceptor from the SAME bundle the client trusts so cert
        // verification succeeds (build_self_signed_acceptor mints its own CA).
        let _ = rustls::crypto::ring::default_provider().install_default();
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

        let certs = CertificateDer::pem_slice_iter(bundle.server_cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(bundle.server_key_pem.as_bytes()).unwrap();
        let mut server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_tls_with_timeout(&acceptor, stream, Duration::from_secs(5)).await
        });

        // Client trusts the generated CA and connects to `localhost`.
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(bundle.ca_cert_pem.as_bytes()) {
            roots.add(cert.unwrap()).unwrap();
        }
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(std::sync::Arc::new(client_config));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let domain = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let client_handshake = tokio::spawn(async move { connector.connect(domain, tcp).await });

        let result = server.await.unwrap();
        assert!(
            matches!(result, Ok(Some(_))),
            "a valid TLS client must complete the handshake, got {result:?}"
        );
        assert!(client_handshake.await.unwrap().is_ok());
    }

    #[test]
    fn bind_addr_is_loopback_classifies_safely() {
        assert!(bind_addr_is_loopback("127.0.0.1:9378"));
        assert!(bind_addr_is_loopback("[::1]:9378"));
        assert!(!bind_addr_is_loopback("0.0.0.0:9378"));
        assert!(!bind_addr_is_loopback("10.0.0.5:9378"));
        // Unparseable → treated as non-loopback (safe default).
        assert!(!bind_addr_is_loopback("localhost:9378"));
    }

    #[test]
    fn validate_bind_security_allows_loopback_plaintext() {
        // Loopback is process-local; no auth/TLS required (the fast local path).
        assert!(validate_bind_security("127.0.0.1:9378", &None, &None, &None, false).is_ok());
        assert!(validate_bind_security("[::1]:9378", &None, &None, &None, false).is_ok());
    }

    #[test]
    fn validate_webhook_secret_lengths_rejects_short_secret() {
        let short = Some("secret".to_string());
        assert!(validate_webhook_secret_lengths(&short, &None).is_err());
        assert!(validate_webhook_secret_lengths(&None, &short).is_err());
    }

    #[test]
    fn validate_webhook_secret_lengths_accepts_min_length() {
        let secret = Some("a".repeat(MIN_WEBHOOK_SECRET_LEN));
        assert_eq!(secret.as_ref().unwrap().len(), MIN_WEBHOOK_SECRET_LEN);
        assert!(validate_webhook_secret_lengths(&secret, &None).is_ok());
        assert!(validate_webhook_secret_lengths(&secret, &secret).is_ok());
    }

    #[test]
    fn validate_webhook_secret_lengths_accepts_none() {
        assert!(validate_webhook_secret_lengths(&None, &None).is_ok());
    }

    fn repo_cfg(url: &str, repo_type: Option<RepoType>) -> RepoConfig {
        RepoConfig {
            url: url.to_string(),
            repo_type,
            name: None,
            sparse: None,
            pin_sha: None,
            use_git_activity: None,
            branch: None,
            poll: None,
        }
    }

    #[test]
    fn build_repo_types_maps_vault_and_code_under_canonical_keys() {
        let vault_url = "https://github.com/kory/notes.git";
        let code_url = "https://github.com/kory/app.git";
        let repos = vec![
            repo_cfg(vault_url, Some(RepoType::Vault)),
            // Untyped repo defaults to code.
            repo_cfg(code_url, None),
        ];
        let map = build_repo_types(&repos);

        let vault_key = nestweaver_engine::jobs::canonical_repo_id(vault_url);
        assert_eq!(map.get(&vault_key), Some(&RepoType::Vault));

        let code_key = nestweaver_engine::jobs::canonical_repo_id(code_url);
        assert_eq!(map.get(&code_key), Some(&RepoType::Code));
    }

    /// A `nestweaver_engine::EmbedQueryFn` that probes the write gate from
    /// *inside* `embed_query` — the moment the embed write actually executes.
    /// It records whether `write_mutex` is held and the value of `active_writes`
    /// at that instant, so a test can assert the embed write ran under the same
    /// gate every other daemon mutation uses.
    #[cfg(feature = "embed")]
    struct GateProbeEmbed {
        write_mutex: Arc<tokio::sync::Mutex<()>>,
        active_writes: Arc<AtomicU32>,
        observed_locked: Arc<AtomicBool>,
        observed_writes: Arc<AtomicU32>,
        done: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    }

    #[cfg(feature = "embed")]
    impl nestweaver_engine::EmbedQueryFn for GateProbeEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            // Capture the gate state at the exact point the embed write runs.
            self.observed_locked
                .store(self.write_mutex.try_lock().is_err(), Ordering::Relaxed);
            self.observed_writes.store(
                self.active_writes.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            if let Some(tx) = self.done.lock().unwrap().take() {
                let _ = tx.send(());
            }
            Ok(vec![0.1_f32, 0.2, 0.3])
        }
    }

    #[cfg(feature = "embed")]
    fn ready_test_embedding_runtime(
        model: Arc<dyn nestweaver_engine::EmbedQueryFn>,
    ) -> Arc<EmbeddingRuntime> {
        let mut status = initial_embedding_status(
            &nestweaver_engine::config::EmbeddingConfig::default(),
            None,
            true,
            daemon_metal_compiled(),
        );
        status.state = "ready".to_string();
        status.selected_device = "cpu".to_string();
        let runtime = Arc::new(EmbeddingRuntime::unavailable(initial_embedding_status(
            &nestweaver_engine::config::EmbeddingConfig::default(),
            None,
            true,
            daemon_metal_compiled(),
        )));
        runtime.publish_ready(status, model);
        runtime
    }

    /// The watcher's embed-on-change callback must perform its `add_embedding`
    /// writes under the write gate the watcher thread holds (`write_mutex` +
    /// `ConnectionGuard::write`), not on a detached fire-and-forget task that
    /// escapes it. Escaping the gate (a) races a backup's sidecar copy and
    /// (b) is invisible to the shutdown drain (`active_writes`).
    ///
    /// This mirrors the `watch_vault`/`watch_code` wiring (server.rs ~863-872):
    /// the watcher thread takes `write_mutex.blocking_lock()` + a write guard
    /// for its whole run and calls the callback inline within that hold.
    #[cfg(feature = "embed")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_embed_holds_write_gate() {
        use nestweaver_schema::{Symbol, SymbolKind, Visibility};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());

        // One un-embedded symbol so the callback reaches exactly one embed_query.
        let sym_uid = "sym-gate-probe".to_string();
        store
            .insert_symbol(&Symbol {
                uid: sym_uid.clone(),
                name: "gate_probe".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo-1".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn gate_probe()".to_string(),
                summary: None,
                content_hash: "h".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();

        let write_mutex = Arc::new(tokio::sync::Mutex::new(()));
        let active_writes = Arc::new(AtomicU32::new(0));
        let observed_locked = Arc::new(AtomicBool::new(false));
        let observed_writes = Arc::new(AtomicU32::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        let probe = Arc::new(GateProbeEmbed {
            write_mutex: write_mutex.clone(),
            active_writes: active_writes.clone(),
            observed_locked: observed_locked.clone(),
            observed_writes: observed_writes.clone(),
            done: std::sync::Mutex::new(Some(done_tx)),
        }) as Arc<dyn nestweaver_engine::EmbedQueryFn>;
        let embedding_runtime = ready_test_embedding_runtime(probe);

        let cb = DaemonService::make_embed_on_change(embedding_runtime, store.clone())
            .expect("embed callback should be present when a model is loaded");

        // Mirror the watcher thread: hold write_mutex + bump active_writes for
        // the callback's whole run, exactly as watch_vault/watch_code do.
        let wm = write_mutex.clone();
        let aw = active_writes.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let _write_lock = wm.blocking_lock();
            aw.fetch_add(1, Ordering::Relaxed); // == ConnectionGuard::write
            cb();
            aw.fetch_sub(1, Ordering::Relaxed); // == guard drop on watcher exit
        });

        // Wait for the embed write to run (or fail the test if it never does).
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("embed_query must run for the inserted symbol");
        handle.await.unwrap();

        // The whole point: while the embed write ran, the write gate was held.
        assert!(
            observed_locked.load(Ordering::Relaxed),
            "embed write must run while write_mutex is held (backup-safe)"
        );
        assert!(
            observed_writes.load(Ordering::Relaxed) > 0,
            "embed write must run while active_writes > 0 (drain-visible)"
        );
        // And it actually wrote the embedding.
        assert!(
            store.has_embedding(&sym_uid),
            "callback should have embedded the pending symbol"
        );
    }

    /// An `EmbedQueryFn` that counts calls and returns a fixed vector, for
    /// exercising the debounce + circuit-breaker in `make_embed_on_change`.
    #[cfg(feature = "embed")]
    struct CountingEmbed {
        calls: Arc<AtomicU32>,
        vector: Vec<f32>,
    }

    #[cfg(feature = "embed")]
    impl nestweaver_engine::EmbedQueryFn for CountingEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.vector.clone())
        }
    }

    /// Insert one un-embedded symbol so the embed callback has work to do.
    #[cfg(feature = "embed")]
    fn insert_unembedded_symbol(store: &GraphStore, uid: &str) {
        use nestweaver_schema::{Symbol, SymbolKind, Visibility};
        store
            .insert_symbol(&Symbol {
                uid: uid.to_string(),
                name: format!("name_{uid}"),
                kind: SymbolKind::Function,
                repo_uid: "repo-1".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn f()".to_string(),
                summary: None,
                content_hash: "h".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();
    }

    #[cfg(feature = "embed")]
    fn insert_unembedded_nodes_for_every_scope(store: &GraphStore, suffix: &str) -> [String; 3] {
        use nestweaver_schema::{Heading, Note, NoteKind};

        let symbol_uid = format!("sym-{suffix}");
        let note_uid = format!("note-{suffix}");
        let heading_uid = format!("heading-{suffix}");
        insert_unembedded_symbol(store, &symbol_uid);
        store
            .insert_note(&Note {
                uid: note_uid.clone(),
                vault_uid: "vault-1".to_string(),
                file_path: format!("notes/{suffix}.md"),
                title: suffix.to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: format!("note-hash-{suffix}"),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: heading_uid.clone(),
                note_uid: note_uid.clone(),
                level: 1,
                text: suffix.to_string(),
                slug: suffix.to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: format!("heading-hash-{suffix}"),
                embedding: None,
            })
            .unwrap();

        [symbol_uid, note_uid, heading_uid]
    }

    /// Invoke the callback on a blocking thread — it uses
    /// the callback is synchronous (mirrors how the watcher thread calls it).
    #[cfg(feature = "embed")]
    async fn run_embed_cb(cb: Arc<std::sync::Mutex<Box<dyn Fn() + Send>>>) {
        tokio::task::spawn_blocking(move || (cb.lock().unwrap())())
            .await
            .unwrap();
    }

    #[cfg(feature = "embed")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_embed_skips_sidecar_embeddings_for_every_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(GraphStore::open_or_create(&dir.path().join("brain.lbug")).unwrap());
        let uids = insert_unembedded_nodes_for_every_scope(&store, "watcher-incremental");
        for uid in &uids {
            assert!(
                store.add_embedding(uid, vec![0.1, 0.2, 0.3]),
                "fixture embedding should be accepted for {uid}"
            );
            assert!(store.has_embedding(uid));
        }

        let calls = Arc::new(AtomicU32::new(0));
        let probe = Arc::new(CountingEmbed {
            calls: calls.clone(),
            vector: vec![0.1_f32, 0.2, 0.3],
        }) as Arc<dyn nestweaver_engine::EmbedQueryFn>;
        let callback = Arc::new(std::sync::Mutex::new(
            DaemonService::make_embed_on_change_with(
                ready_test_embedding_runtime(probe),
                store,
                std::time::Duration::ZERO,
            )
            .expect("embed callback should be present when a model is loaded"),
        ));

        run_embed_cb(callback).await;

        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "watcher embedding must consult the sidecar index, not graph-row fields"
        );
    }

    /// Regression (re-embedding stalls the watcher, #perf): back-to-back
    /// watcher batches must NOT each pay a full inline embed pass — passes
    /// are debounced to at most one per interval so a burst of saves can't
    /// stall the watcher's DB writes behind local-model inference.
    #[cfg(feature = "embed")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_embed_debounces_back_to_back_batches() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(GraphStore::open_or_create(&dir.path().join("brain.lbug")).unwrap());
        insert_unembedded_symbol(&store, "sym-debounce");

        let calls = Arc::new(AtomicU32::new(0));
        let probe = Arc::new(CountingEmbed {
            calls: calls.clone(),
            vector: vec![0.1_f32, 0.2, 0.3],
        }) as Arc<dyn nestweaver_engine::EmbedQueryFn>;
        let embedding_runtime = ready_test_embedding_runtime(probe);

        let cb = Arc::new(std::sync::Mutex::new(
            DaemonService::make_embed_on_change_with(
                embedding_runtime,
                store,
                std::time::Duration::from_secs(3600),
            )
            .expect("embed callback should be present when a model is loaded"),
        ));

        run_embed_cb(cb.clone()).await;
        run_embed_cb(cb.clone()).await;
        run_embed_cb(cb).await;

        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "batches within the debounce window must skip the embed pass"
        );
    }

    /// Regression (hot retry loop): when every vector is rejected (e.g. a
    /// dimension-mismatched/zero-length vector that `add_embedding`
    /// refuses — the zero-vector case), retrying the same nodes on every
    /// watcher batch burns CPU forever without progress. After enough
    /// consecutive all-failed passes the callback must stop calling the
    /// model.
    #[cfg(feature = "embed")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_embed_circuit_breaks_after_repeated_all_fail_passes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(GraphStore::open_or_create(&dir.path().join("brain.lbug")).unwrap());
        insert_unembedded_symbol(&store, "sym-reject");
        // Establish a dim-2 embedding index so the probe's dim-3 vector is
        // always REJECTED by add_embedding's dimension guard.
        assert!(store.add_embedding("seed-uid", vec![1.0_f32, 0.0]));

        let calls = Arc::new(AtomicU32::new(0));
        let probe = Arc::new(CountingEmbed {
            calls: calls.clone(),
            vector: vec![0.1_f32, 0.2, 0.3],
        }) as Arc<dyn nestweaver_engine::EmbedQueryFn>;
        let embedding_runtime = ready_test_embedding_runtime(probe);

        let cb = Arc::new(std::sync::Mutex::new(
            DaemonService::make_embed_on_change_with(
                embedding_runtime,
                store,
                std::time::Duration::ZERO, // every pass runs
            )
            .expect("embed callback should be present when a model is loaded"),
        ));

        for _ in 0..DaemonService::EMBED_ON_CHANGE_MAX_ALL_FAIL_PASSES {
            run_embed_cb(cb.clone()).await;
        }
        let after_failures = calls.load(Ordering::Relaxed);
        assert_eq!(
            after_failures,
            DaemonService::EMBED_ON_CHANGE_MAX_ALL_FAIL_PASSES,
            "each all-fail pass attempts the model exactly once"
        );

        // The circuit is now open: further batches must not call the model.
        run_embed_cb(cb.clone()).await;
        run_embed_cb(cb).await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            after_failures,
            "after repeated all-fail passes the callback must stop retrying the model"
        );
    }

    /// Grab a free localhost port (bind :0, read the port, release).
    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn admin_serve_ui_request(port: u16) -> Request<ServeUiRequest> {
        let mut request = Request::new(ServeUiRequest {
            port: u32::from(port),
            open_browser: false,
            watch: false,
            watch_repo_path: String::new(),
            watch_instance_id: "default".to_string(),
        });
        request.extensions_mut().insert(crate::auth::IsAdmin(true));
        request
    }

    /// Regression (dead URL): when the UI is already running, `serve_ui`
    /// must report the ACTUAL running port — not the requested one —
    /// because the CLI prints that port in the URL it shows the user.
    #[tokio::test]
    async fn serve_ui_already_running_reports_actual_port() {
        let state = test_state_with_writer();
        let service = DaemonService::new(state);

        let port_a = free_port();
        let resp = service
            .serve_ui(admin_serve_ui_request(port_a))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.ok, "first serve_ui must start: {}", resp.message);
        assert_eq!(resp.port, u32::from(port_a));
        assert!(resp.error.is_empty());

        // Re-ask with a DIFFERENT port: the response must carry port A.
        let port_b = free_port();
        let resp = service
            .serve_ui(admin_serve_ui_request(port_b))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.ok);
        assert_eq!(
            resp.port,
            u32::from(port_a),
            "already-running response must report the ACTUAL port, not the requested one"
        );
        assert!(
            resp.message.contains(&port_a.to_string()),
            "message must name the actual port: {}",
            resp.message
        );
    }

    /// Regression (ambiguous failure): a port bound by a FOREIGN process
    /// must come back as ok:false with a machine-readable `port_in_use`
    /// error code so the CLI can map it to a non-zero exit.
    #[tokio::test]
    async fn serve_ui_foreign_busy_port_returns_port_in_use() {
        let state = test_state_with_writer();
        let service = DaemonService::new(state);

        let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy_port = blocker.local_addr().unwrap().port();

        let resp = service
            .serve_ui(admin_serve_ui_request(busy_port))
            .await
            .unwrap()
            .into_inner();
        assert!(!resp.ok);
        assert_eq!(resp.error, "port_in_use");
        assert_eq!(resp.port, 0);
        drop(blocker);
    }

    /// Build a minimal `DaemonState` with a writer-mode Tantivy index for
    /// exercising admin mutation RPCs in isolation.
    fn test_state_with_writer() -> Arc<DaemonState> {
        test_state_with_writer_generation(None)
    }

    fn test_state_with_writer_generation(generation: Option<u64>) -> Arc<DaemonState> {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        if let Some(generation) = generation {
            let generation_path = nestweaver_engine::sidecar_path(&db_path, ".generation");
            std::fs::write(generation_path, generation.to_string()).unwrap();
        }
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let tantivy = Arc::new(TantivyIndex::open_or_create(&dir.path().join("tantivy")).unwrap());
        // Keep the temp dir alive for the duration of the test process.
        std::mem::forget(dir);
        let (shutdown_tx, _rx) = tokio::sync::watch::channel(false);
        Arc::new(DaemonState {
            store,
            tantivy: Some(Arc::clone(&tantivy)),
            search_reconciliation: SearchIndexReconciliation::Available(tantivy),
            db_path,
            instance_id: "default".to_string(),
            data_instance_id: "default".to_string(),
            start_time: Instant::now(),
            active_reads: Arc::new(AtomicU32::new(0)),
            active_writes: Arc::new(AtomicU32::new(0)),
            idle_notify: Arc::new(Notify::new()),
            shutdown_tx,
            watcher_stop: std::sync::Mutex::new(None),
            next_watcher_id: std::sync::atomic::AtomicU64::new(0),
            instance_cfg: None,
            permission_source: build_daemon_permission_source(None),
            embedding_runtime: Arc::new(EmbeddingRuntime::unavailable(initial_embedding_status(
                &nestweaver_engine::config::EmbeddingConfig::default(),
                None,
                cfg!(feature = "embed"),
                daemon_metal_compiled(),
            ))),
            write_mutex: Arc::new(tokio::sync::Mutex::new(())),
            server_mode: false,
            read_only: false,
            indexing_active: Arc::new(AtomicBool::new(false)),
            indexing_repo: Arc::new(tokio::sync::RwLock::new(String::new())),
            indexing_queue_depth: Arc::new(AtomicU32::new(0)),
            safeguards: QuerySafeguards::default_server(),
            rate_limiters: None,
            drained: Arc::new(AtomicBool::new(false)),
            admin_token: None,
            admin_state: std::sync::OnceLock::new(),
            worker_handle: std::sync::Mutex::new(None),
            ui_server: std::sync::Mutex::new(None),
        })
    }

    /// Build a minimal `DaemonState` over the given store and permission
    /// source, for exercising `visible_repos_for` under a real authz policy.
    fn test_state_with_authz(
        store: Arc<GraphStore>,
        permission_source: Arc<dyn nestweaver_engine::authz::PermissionSource>,
    ) -> Arc<DaemonState> {
        let (shutdown_tx, _rx) = tokio::sync::watch::channel(false);
        Arc::new(DaemonState {
            store,
            tantivy: None,
            search_reconciliation: SearchIndexReconciliation::Disabled,
            db_path: std::path::PathBuf::from(":memory:"),
            instance_id: "default".to_string(),
            data_instance_id: "default".to_string(),
            start_time: Instant::now(),
            active_reads: Arc::new(AtomicU32::new(0)),
            active_writes: Arc::new(AtomicU32::new(0)),
            idle_notify: Arc::new(Notify::new()),
            shutdown_tx,
            watcher_stop: std::sync::Mutex::new(None),
            next_watcher_id: std::sync::atomic::AtomicU64::new(0),
            instance_cfg: None,
            permission_source,
            embedding_runtime: Arc::new(EmbeddingRuntime::unavailable(initial_embedding_status(
                &nestweaver_engine::config::EmbeddingConfig::default(),
                None,
                cfg!(feature = "embed"),
                daemon_metal_compiled(),
            ))),
            write_mutex: Arc::new(tokio::sync::Mutex::new(())),
            server_mode: false,
            read_only: false,
            indexing_active: Arc::new(AtomicBool::new(false)),
            indexing_repo: Arc::new(tokio::sync::RwLock::new(String::new())),
            indexing_queue_depth: Arc::new(AtomicU32::new(0)),
            safeguards: QuerySafeguards::default_server(),
            rate_limiters: None,
            drained: Arc::new(AtomicBool::new(false)),
            admin_token: None,
            admin_state: std::sync::OnceLock::new(),
            worker_handle: std::sync::Mutex::new(None),
            ui_server: std::sync::Mutex::new(None),
        })
    }

    #[tokio::test]
    async fn embedding_status_is_exposed_by_typed_and_json_status_rpcs() {
        let state = test_state_with_writer();
        state
            .embedding_runtime
            .publish_unavailable(EmbeddingRuntimeStatus {
                state: "failed".to_string(),
                backend: "local".to_string(),
                requested_device: "metal".to_string(),
                selected_device: String::new(),
                model_id: "test-model".to_string(),
                error: "Metal runtime probe failed".to_string(),
                metal_compiled: true,
                fallback_used: false,
            });
        let service = DaemonService::new(state);

        let typed = service
            .brain_status(Request::new(BrainStatusRequest {}))
            .await
            .expect("typed brain status")
            .into_inner()
            .embedding_status
            .expect("structured embedding status");
        assert_eq!(typed.state, "failed");
        assert_eq!(typed.requested_device, "metal");
        assert_eq!(typed.selected_device, "");
        assert!(typed.error.contains("Metal"));
        assert!(!typed.fallback_used);

        let json = service
            .brain_status_json(Request::new(JsonRequest {
                args_json: "{}".to_string(),
            }))
            .await
            .expect("JSON brain status")
            .into_inner();
        let value: serde_json::Value =
            serde_json::from_str(&json.result_json).expect("valid status JSON");
        assert_eq!(value["embedding_status"]["state"], "failed");
        assert_eq!(value["embedding_status"]["requested_device"], "metal");
        assert_eq!(value["embedding_status"]["selected_device"], "");
        assert_eq!(value["embedding_status"]["fallback_used"], false);
    }

    #[tokio::test]
    async fn embedding_status_blocks_embed_rpc_until_ready() {
        let state = test_state_with_writer();
        let expected_state = state.embedding_runtime.status().state;
        let service = DaemonService::new(state);
        let mut request = Request::new(EmbedRequest {
            scope: "all".to_string(),
            force: false,
            batch_size: 0,
        });
        request.extensions_mut().insert(crate::auth::IsAdmin(true));

        let error = service
            .embed(request)
            .await
            .expect_err("an unavailable embedding model must block embedding");

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(
            error.message().contains(&expected_state)
                || error.message().contains("without the `embed` feature"),
            "error should identify the structured readiness failure: {error}"
        );
    }

    #[cfg(feature = "embed")]
    #[tokio::test]
    async fn embed_plan_reports_all_missing_nodes_as_eligible() {
        let state = test_state_with_writer();
        insert_unembedded_nodes_for_every_scope(&state.store, "plan-missing");
        let service = DaemonService::new(state);

        let response = service
            .plan_embed(Request::new(EmbedRequest {
                scope: "all".to_string(),
                force: false,
                batch_size: 0,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.scoped, 3);
        assert_eq!(response.eligible, 3);
        assert_eq!(response.skipped, 0);
    }

    #[cfg(feature = "embed")]
    #[tokio::test]
    async fn embed_plan_skips_nodes_present_in_the_sidecar() {
        let state = test_state_with_writer();
        let uids = insert_unembedded_nodes_for_every_scope(&state.store, "plan-sidecar");
        for uid in &uids {
            assert!(state.store.add_embedding(uid, vec![0.1, 0.2, 0.3]));
        }
        let service = DaemonService::new(state);

        let response = service
            .plan_embed(Request::new(EmbedRequest {
                scope: "all".to_string(),
                force: false,
                batch_size: 0,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.scoped, 3);
        assert_eq!(response.eligible, 0);
        assert_eq!(response.skipped, 3);
    }

    #[cfg(feature = "embed")]
    #[tokio::test]
    async fn forced_embed_plan_keeps_sidecar_nodes_eligible() {
        let state = test_state_with_writer();
        let uids = insert_unembedded_nodes_for_every_scope(&state.store, "plan-force");
        for uid in &uids {
            assert!(state.store.add_embedding(uid, vec![0.1, 0.2, 0.3]));
        }
        let service = DaemonService::new(state);

        let response = service
            .plan_embed(Request::new(EmbedRequest {
                scope: "all".to_string(),
                force: true,
                batch_size: 0,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.scoped, 3);
        assert_eq!(response.eligible, 3);
        assert_eq!(response.skipped, 0);
    }

    #[cfg(feature = "embed")]
    #[tokio::test]
    async fn embed_plan_rejects_an_unknown_scope() {
        let service = DaemonService::new(test_state_with_writer());

        let status = service
            .plan_embed(Request::new(EmbedRequest {
                scope: "unknown".to_string(),
                force: false,
                batch_size: 0,
            }))
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("unknown scope 'unknown'"));
    }

    #[cfg(feature = "embed")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incremental_embed_rpc_skips_sidecar_embeddings_for_every_scope() {
        let mut state = test_state_with_writer();
        let uids = insert_unembedded_nodes_for_every_scope(&state.store, "rpc-incremental");
        for uid in &uids {
            assert!(
                state.store.add_embedding(uid, vec![0.1, 0.2, 0.3]),
                "fixture embedding should be accepted for {uid}"
            );
            assert!(state.store.has_embedding(uid));
        }

        let calls = Arc::new(AtomicU32::new(0));
        let model = Arc::new(CountingEmbed {
            calls: calls.clone(),
            vector: vec![0.1, 0.2, 0.3],
        }) as Arc<dyn nestweaver_engine::EmbedQueryFn>;
        Arc::get_mut(&mut state)
            .expect("test owns the only state Arc")
            .embedding_runtime = ready_test_embedding_runtime(model);
        let service = DaemonService::new(state);

        let mut request = Request::new(EmbedRequest {
            scope: "all".to_string(),
            force: false,
            batch_size: 1,
        });
        request.extensions_mut().insert(crate::auth::IsAdmin(true));
        let response = service.embed(request).await.unwrap().into_inner();

        assert_eq!(response.succeeded, 0);
        assert_eq!(response.failed, 0);
        assert_eq!(response.rejected, 0);
        assert_eq!(response.scoped, 3);
        assert_eq!(response.eligible, 0);
        assert_eq!(response.skipped, 3);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "incremental embedding must consult the sidecar index, not graph-row fields"
        );
    }

    #[cfg(feature = "embed")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unavailable_external_probe_keeps_daemon_healthy_and_failed() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let config = nestweaver_engine::InstanceConfig::from_toml_str(&format!(
            r#"
instance_id = "external-probe-test"

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

[embedding]
external_endpoint = {endpoint:?}
external_model = "unavailable-test-model"
"#
        ))
        .unwrap();
        let mut state = test_state_with_writer();
        let state_mut = Arc::get_mut(&mut state).expect("test owns the only state Arc");
        state_mut.instance_cfg = Some(Arc::new(config.clone()));
        state_mut
            .embedding_runtime
            .publish_unavailable(initial_embedding_status(
                &config.embedding,
                None,
                true,
                daemon_metal_compiled(),
            ));

        tokio::time::timeout(Duration::from_secs(5), load_embedding_model(&state))
            .await
            .expect("unavailable local endpoint must fail promptly without panicking");

        let (status, model) = state.embedding_runtime.snapshot();
        assert_eq!(status.state, "failed");
        assert_eq!(status.backend, "external");
        assert_eq!(status.model_id, "unavailable-test-model");
        assert!(!status.error.is_empty());
        assert!(!status.fallback_used);
        assert!(model.is_none(), "failed probe must not publish a model");

        // The daemon state remains usable after the failed probe.
        let typed = DaemonService::new(state)
            .brain_status(Request::new(BrainStatusRequest {}))
            .await
            .expect("status handler must remain healthy")
            .into_inner()
            .embedding_status
            .expect("structured embedding status");
        assert_eq!(typed.state, "failed");
        assert!(!typed.fallback_used);
    }

    #[cfg(feature = "embed")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn embed_handler_never_observes_ready_without_exact_model() {
        let state = test_state_with_writer();
        insert_unembedded_symbol(&state.store, "sym-atomic-handler");
        let calls = Arc::new(AtomicU32::new(0));
        let model = Arc::new(CountingEmbed {
            calls: calls.clone(),
            vector: vec![0.1, 0.2, 0.3],
        }) as Arc<dyn nestweaver_engine::EmbedQueryFn>;
        let mut loading = state.embedding_runtime.status();
        loading.state = "loading".to_string();
        loading.error.clear();
        let mut ready = loading.clone();
        ready.state = "ready".to_string();
        ready.selected_device = "cpu".to_string();

        let stop = Arc::new(AtomicBool::new(false));
        let handler_runtime = state.embedding_runtime.clone();
        let writer_runtime = state.embedding_runtime.clone();
        let writer_model = model.clone();
        let writer_stop = stop.clone();
        let writer = tokio::task::spawn_blocking(move || {
            while !writer_stop.load(Ordering::Acquire) {
                writer_runtime.publish_unavailable(loading.clone());
                std::thread::yield_now();
                writer_runtime.publish_ready(ready.clone(), writer_model.clone());
                std::thread::yield_now();
            }
        });
        let service = DaemonService::new(state);

        for _ in 0..32 {
            let mut request = Request::new(EmbedRequest {
                scope: "symbols".to_string(),
                force: true,
                batch_size: 1,
            });
            request.extensions_mut().insert(crate::auth::IsAdmin(true));
            if let Err(error) = service.embed(request).await {
                assert!(
                    !error.message().contains("state: ready"),
                    "handler observed impossible ready-without-model snapshot: {error}"
                );
            }
        }

        stop.store(true, Ordering::Release);
        writer.await.unwrap();
        let mut final_ready = handler_runtime.status();
        final_ready.state = "ready".to_string();
        final_ready.selected_device = "cpu".to_string();
        handler_runtime.publish_ready(final_ready, model);
        let mut final_request = Request::new(EmbedRequest {
            scope: "symbols".to_string(),
            force: true,
            batch_size: 1,
        });
        final_request
            .extensions_mut()
            .insert(crate::auth::IsAdmin(true));
        service
            .embed(final_request)
            .await
            .expect("published ready snapshot must use its exact model");
        assert!(
            calls.load(Ordering::Relaxed) > 0,
            "ready snapshots must route work through the exact published model"
        );
    }

    #[tokio::test]
    async fn typed_brain_search_enforces_request_repo_visibility_and_preserves_counts() {
        use nestweaver_engine::authz::{Identity, StaticConfigPermissionSource};
        use nestweaver_schema::{Symbol, SymbolKind, Visibility};

        let store = Arc::new(GraphStore::in_memory().unwrap());
        for repo in [
            test_repo("repo:visible", "https://github.com/acme/visible.git", None),
            test_repo("repo:hidden", "https://github.com/acme/hidden.git", None),
        ] {
            store.insert_repo(&repo).unwrap();
        }
        for (uid, repo_uid, name) in [
            ("sym:visible", "repo:visible", "searchneedle_visible"),
            ("sym:hidden", "repo:hidden", "searchneedle_hidden"),
        ] {
            store
                .insert_symbol(&Symbol {
                    uid: uid.to_string(),
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    repo_uid: repo_uid.to_string(),
                    file_path: format!("src/{name}.rs"),
                    start_line: 1,
                    end_line: 2,
                    signature: format!("fn {name}()"),
                    summary: None,
                    content_hash: format!("hash:{uid}"),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: Visibility::Public,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: Some(format!("canonical:{uid}")),
                })
                .unwrap();
        }

        let token = "scoped-query-token";
        let source = Arc::new(StaticConfigPermissionSource::new(
            [(token.to_string(), vec!["repo:visible".to_string()])]
                .into_iter()
                .collect(),
        ));
        let service = DaemonService::new(test_state_with_authz(store, source));
        let search_request = |limit| BrainSearchRequest {
            query: "searchneedle".to_string(),
            limit,
            response_format: None,
            include_bodies: false,
            prf: false,
            rerank: false,
            root: None,
        };

        // Warm the unrestricted cache scope first. The subsequent restricted
        // request must neither reuse this response nor dispatch as `All`.
        let mut unrestricted = Request::new(search_request(1));
        unrestricted.extensions_mut().insert(Identity::Admin);
        let unrestricted = service.search(unrestricted).await.unwrap().into_inner();
        assert_eq!(unrestricted.total_matches, 2);
        assert_eq!(unrestricted.results.len(), 1);
        assert_eq!(unrestricted.returned_matches, 1);
        assert_eq!(unrestricted.total_matches_relation, "eq");
        assert!(unrestricted.truncated);

        let mut request = Request::new(search_request(10));
        request
            .extensions_mut()
            .insert(Identity::Token(token.to_string()));

        let response = service.search(request).await.unwrap().into_inner();

        assert_eq!(response.total_matches, 1);
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.returned_matches, 1);
        assert_eq!(response.total_matches_relation, "eq");
        assert!(!response.truncated);
        assert_eq!(response.results[0].uid, "sym:visible");
        assert_eq!(
            response.results[0].canonical_id.as_deref(),
            Some("canonical:sym:visible")
        );
        assert!(
            response.results.iter().all(|row| row.uid != "sym:hidden"),
            "typed Search must not leak hidden symbol rows"
        );
    }

    /// nw-050: a UDS trusted-admin request must see ALL repos under an enabled
    /// `[authz]` policy. The UDS interceptor is the trusted-local-admin
    /// boundary; before the fix it attached only `IsAdmin(true)` and no
    /// `Identity`, so `visible_repos_for` fell back to `Identity::Anonymous`,
    /// which an enabled policy maps to `Only(∅)` — silently redacting EVERY
    /// cross-repo blast-radius node away from the trusted local admin. The fix
    /// makes the interceptor attach `Identity::Admin` (symmetric with the TCP
    /// admin-token path), which resolves to `VisibleRepos::All`.
    #[test]
    fn uds_admin_sees_all_repos_under_enabled_policy() {
        use nestweaver_engine::authz::{
            PermissionSource, StaticConfigPermissionSource, VisibleRepos,
        };

        // Enabled policy: a non-empty rules map. Under it, Anonymous (and any
        // unknown token) fails closed to Only(∅).
        let mut rules = std::collections::HashMap::new();
        rules.insert(
            "some-query-token".to_string(),
            vec!["acme/scoped-*".to_string()],
        );
        let source = Arc::new(StaticConfigPermissionSource::new(rules));
        assert!(source.is_enabled(), "precondition: policy must be enabled");

        // At least one repo exists — a non-admin identity would see none of it.
        let store = Arc::new(nestweaver_store::GraphStore::in_memory().unwrap());
        store
            .insert_repo(&test_repo(
                "repo:one",
                "https://github.com/acme/one.git",
                None,
            ))
            .unwrap();

        let state = test_state_with_authz(store, source);

        // Run a request through the trusted-local-admin UDS interceptor.
        let req = crate::auth::uds_admin_interceptor(Request::new(())).unwrap();

        let visible = state
            .visible_repos_for(req.extensions())
            .expect("enabled policy over a healthy store must resolve, not fail loud");

        assert_eq!(
            visible,
            VisibleRepos::All,
            "a UDS trusted-admin request must see ALL repos under an enabled \
             authz policy — not be redacted to Only(∅) via the Anonymous \
             fallback (nw-050)"
        );
    }

    /// The admin `reindex_search` RPC rebuilds the whole Tantivy index. Like
    /// every other daemon mutation (`prune_stale`, `purge_instance`, the
    /// watcher embed write) it MUST run under the write gate — `write_mutex`
    /// plus a `ConnectionGuard::write` — so it (a) serializes against a
    /// `backup`'s sidecar staging (which copies under the same lock) and
    /// (b) is visible to the shutdown drain / idle timeout via `active_writes`.
    ///
    /// The gate is probed behaviorally: while the test holds `write_mutex`
    /// (exactly as a concurrent backup staging would), a gated `reindex_search`
    /// must block until the gate is released. An ungated implementation returns
    /// immediately — the RED failure this test guards against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_search_holds_write_gate() {
        let state = test_state_with_writer();
        let service = DaemonService::new(state.clone());

        // Hold the write gate, standing in for a backup's sidecar staging.
        let gate = state.write_mutex.clone().lock_owned().await;

        let mut req = Request::new(ReindexSearchRequest {});
        req.extensions_mut().insert(crate::auth::IsAdmin(true));

        let res = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            service.reindex_search(req),
        )
        .await;

        assert!(
            res.is_err(),
            "reindex_search must block on the write gate while it is held \
             (drain-visible + backup-safe); it returned without waiting"
        );

        drop(gate);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_project_holds_write_gate() {
        let state = test_state_with_writer();
        let service = DaemonService::new(state.clone());
        let gate = state.write_mutex.clone().lock_owned().await;
        let mut request = Request::new(RemoveProjectRequest {
            project_uid: "proj:test:write-gate".to_string(),
        });
        request.extensions_mut().insert(crate::auth::IsAdmin(true));

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            service.remove_project(request),
        )
        .await;

        assert!(
            result.is_err(),
            "remove_project must serialize with the daemon write gate"
        );
        drop(gate);
    }

    /// The admin `set_extension` RPC does a read-modify-write of the
    /// `.extensions.json` sidecar, which is part of the backup sidecar set.
    /// Like every other daemon mutation it MUST run under the write gate
    /// (`write_mutex` + `ConnectionGuard::write`) so two concurrent writers
    /// cannot lose updates and a `backup`'s sidecar staging cannot race it.
    ///
    /// Probed the same way as `reindex_search_holds_write_gate`: while the
    /// test holds `write_mutex`, a gated `set_extension` must block until the
    /// gate is released. The `json_rpc!`-dispatched implementation runs under
    /// a *read* guard and returns immediately — the RED failure this guards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_extension_holds_write_gate() {
        let state = test_state_with_writer();
        let service = DaemonService::new(state.clone());

        let gate = state.write_mutex.clone().lock_owned().await;

        let args =
            serde_json::json!({ "uid": "sym:x", "key": "owner", "value": "team-a" }).to_string();
        let mut req = Request::new(JsonRequest { args_json: args });
        req.extensions_mut().insert(crate::auth::IsAdmin(true));

        let res = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            service.set_extension(req),
        )
        .await;

        assert!(
            res.is_err(),
            "set_extension must block on the write gate while it is held \
             (drain-visible + backup-safe); it returned without waiting"
        );

        drop(gate);
    }

    /// T6.2: the Shutdown RPC MUST set `state.drained` synchronously — before
    /// it spawns the drain wait loop — so the worker pool STOPS CLAIMING new
    /// jobs the instant shutdown begins. Otherwise, under continuous webhook
    /// enqueue, the worker keeps claiming brand-new work during the entire
    /// drain, `indexing_active` never clears, and shutdown burns the full
    /// drain ceiling doing work it will then abandon-signal.
    ///
    /// Probed by holding the drain loop open (in-flight indexing) so it cannot
    /// complete within the test, then asserting `drained` flipped by the time
    /// the RPC returns. The pre-fix handler never touches `drained` — the RED
    /// failure this guards against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_sets_drained_before_draining() {
        let state = test_state_with_writer();
        // Stand in for an in-flight index: the drain wait loop waits on
        // `!indexing_active`, so with this set it never completes during the
        // test — proving `drained` is set by the handler itself, not by the
        // loop's completion.
        state.indexing_active.store(true, Ordering::Relaxed);
        assert!(
            !state.drained.load(Ordering::Relaxed),
            "precondition: drained starts clear"
        );

        let service = DaemonService::new(state.clone());
        let mut req = Request::new(ShutdownRequest {});
        req.extensions_mut().insert(crate::auth::IsAdmin(true));

        let resp = service.shutdown(req).await.expect("shutdown ok");
        assert!(resp.into_inner().ok);

        assert!(
            state.drained.load(Ordering::Relaxed),
            "shutdown must set drained immediately (before the drain wait loop) \
             so the worker pool stops claiming new jobs the moment shutdown begins"
        );
    }

    /// health_check must report the daemon process's own PID so the CLI
    /// can cross-check a pidfile PID against the socket-reported PID before
    /// signaling it (a foreign PID planted in the pidfile fails that check).
    #[tokio::test]
    async fn health_check_reports_own_pid() {
        let state = test_state_with_writer();
        let service = DaemonService::new(state);
        let resp = service
            .health_check(Request::new(HealthCheckRequest {}))
            .await
            .expect("health check ok")
            .into_inner();
        assert_eq!(resp.pid, std::process::id());
    }

    /// A second watcher registration is refused without force; with
    /// force the incumbent (possibly orphaned by a kill -9'd `watch` CLI) is
    /// stopped and replaced — and the replaced watcher's exit-clear must not
    /// wipe the replacement's registration.
    #[test]
    fn register_watcher_force_replaces_orphaned_incumbent() {
        let state = test_state_with_writer();
        let flag_a = Arc::new(AtomicBool::new(false));
        let flag_b = Arc::new(AtomicBool::new(false));

        let id_a = register_watcher(
            &state,
            nestweaver_engine::ShutdownHandle::from_flag(flag_a.clone()),
            false,
        )
        .expect("first registration");

        // Without force: refused, incumbent untouched.
        let err = register_watcher(
            &state,
            nestweaver_engine::ShutdownHandle::from_flag(flag_b.clone()),
            false,
        )
        .expect_err("second registration without force must fail");
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
        assert!(
            !flag_a.load(Ordering::Relaxed),
            "refused registration must not stop the incumbent"
        );

        // With force: incumbent stopped, replacement installed.
        let id_b = register_watcher(
            &state,
            nestweaver_engine::ShutdownHandle::from_flag(flag_b.clone()),
            true,
        )
        .expect("force registration");
        assert!(
            flag_a.load(Ordering::Relaxed),
            "force must stop the orphaned incumbent"
        );
        assert_ne!(id_a, id_b);
        assert!(!flag_b.load(Ordering::Relaxed));

        // The replaced watcher's exit-clear must NOT wipe the replacement.
        clear_watcher_registration(&state, id_a);
        assert!(
            state.watcher_stop.lock().unwrap().is_some(),
            "stale watcher exit must not clear the replacement's registration"
        );
        clear_watcher_registration(&state, id_b);
        assert!(state.watcher_stop.lock().unwrap().is_none());
    }

    /// The shutdown RPC stops any active watcher up front, so an
    /// orphaned watcher's blocking thread can't pin the drain until the
    /// client's SIGKILL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_stops_active_watcher() {
        let state = test_state_with_writer();
        let flag = Arc::new(AtomicBool::new(false));
        register_watcher(
            &state,
            nestweaver_engine::ShutdownHandle::from_flag(flag.clone()),
            false,
        )
        .unwrap();

        let service = DaemonService::new(state.clone());
        let mut req = Request::new(ShutdownRequest {});
        req.extensions_mut().insert(crate::auth::IsAdmin(true));
        let resp = service.shutdown(req).await.expect("shutdown ok");
        assert!(resp.into_inner().ok);

        assert!(
            flag.load(Ordering::Relaxed),
            "shutdown must stop the active watcher"
        );
        assert!(
            state.watcher_stop.lock().unwrap().is_none(),
            "shutdown must unregister the watcher"
        );
    }

    // ── Read-only replica: single-chokepoint mutating-RPC rejection ─────

    /// The FULL set of gRPC RPCs that mutate daemon/graph state. Includes the
    /// six that had scattered `reject_if_read_only` guards AND the nine the
    /// snapshot/replica review flagged as unguarded (they used to reach their
    /// handlers on a replica and fail mid-stream at the read-only storage
    /// layer as opaque `internal` errors, or do partial work first).
    const MUTATING_RPC_METHODS: &[&str] = &[
        // Previously guarded by reject_if_read_only.
        "WatchVault",
        "WatchCode",
        "IndexRepo",
        "IndexVault",
        "RemoveRepo",
        "MergeInstance",
        // Previously UNGUARDED — the core of finding #10.
        "MaterializeProjects",
        "RemoveVault",
        "RemoveProject",
        "PruneStale",
        "PurgeInstance",
        "ReindexSearch",
        "Embed",
        "SetExtension",
        "Backup",
        "BrainMemoryConsolidate",
        "RefreshBrain",
    ];

    /// Parametrized over the FULL mutating-RPC set: on a read-only replica the
    /// single chokepoint must reject every one with FAILED_PRECONDITION, and a
    /// read-write daemon must reject none of them.
    #[test]
    fn read_only_rejects_all_mutating_rpcs() {
        for m in MUTATING_RPC_METHODS {
            let path = format!("/nestweaver.daemon.v1.NestWeaverDaemon/{m}");
            let rej = read_only_rejection(true, &path);
            assert!(
                rej.is_some(),
                "read-only replica must reject mutating RPC {m} at the chokepoint"
            );
            assert_eq!(
                rej.unwrap().code(),
                tonic::Code::FailedPrecondition,
                "{m} must be rejected with FAILED_PRECONDITION, not another code"
            );
            assert!(
                read_only_rejection(false, &path).is_none(),
                "a read-write daemon must NOT reject mutating RPC {m}"
            );
        }
    }

    /// The replica must still serve every pure-read RPC — the chokepoint must
    /// not over-reject and break the replica's reason for existing.
    #[test]
    fn read_only_allows_all_pure_read_rpcs() {
        for m in READ_ONLY_ALLOWED_METHODS {
            let path = format!("/nestweaver.daemon.v1.NestWeaverDaemon/{m}");
            assert!(
                read_only_rejection(true, &path).is_none(),
                "read-only replica must still serve read RPC {m}"
            );
        }
    }

    /// Every RPC the proto service exposes is classified as EXACTLY ONE of
    /// read-allowed or mutating — no overlap, no gaps. A new RPC that is
    /// neither trips this test, forcing a deliberate fail-closed decision so
    /// the default-deny chokepoint can never silently miss a mutating path.
    #[test]
    fn read_only_method_partition_is_exhaustive() {
        // MAINTENANCE — this MUST list every RPC method the proto service
        // (`NestWeaverDaemon` in nestweaver.daemon.v1) exposes. It is hand-kept:
        // there is no runtime proto method registry to derive it from, so when
        // you add a proto RPC you MUST add it here AND classify it in
        // `READ_ONLY_ALLOWED_METHODS` or `MUTATING_RPC_METHODS`. Both this
        // partition test and the runtime default-deny chokepoint
        // (`read_only_rejection`) depend on this list being complete — a missing
        // entry means the new RPC is untested here (runtime still fails closed:
        // an unknown method default-denies on a replica). Regenerate/verify with:
        //   grep -oE '/nestweaver\.daemon\.v1\.NestWeaverDaemon/[A-Za-z]+' \
        //     $(find target -name nestweaver.daemon.v1.rs -path '*out*' | head -1) \
        //     | sed -E 's#.*/##' | sort -u
        const ALL_RPC_METHODS: &[&str] = &[
            "AffectedTests",
            "Backup",
            "BlastRadius",
            "BrainBrokenLinks",
            "BrainDiff",
            "BrainDocStats",
            "BrainGuide",
            "BrainMemoryConsolidate",
            "BrainMemoryLint",
            "BrainMemoryRelated",
            "BrainOrphanDocuments",
            "BrainStatus",
            "BrainStatusJson",
            "BrainTagGraph",
            "BrainTopicClusters",
            "BridgeNodes",
            "Clusters",
            "ContractDrift",
            "CountPatterns",
            "CrossRepoContracts",
            "DeadCode",
            "DetectChanges",
            "DetectImplicitProjectsJson",
            "Embed",
            "EmbeddingDimension",
            "ExportGraph",
            "FlowTrace",
            "FlowTraceContinue",
            "GetBacklinks",
            "GetContext",
            "GetNote",
            "GetProjectContext",
            "GetSummary",
            "HealthCheck",
            "HubNodes",
            "Impact",
            "ImpactAnalysis",
            "IndexRepo",
            "IndexVault",
            "Investigate",
            "InvestigateExpand",
            "InvestigateHydrate",
            "ListProjectsJson",
            "ListReposJson",
            "ListServicesJson",
            "ListVaultsJson",
            "MaterializeProjects",
            "MergeInstance",
            "PlanEmbed",
            "PrImpactJson",
            "PruneStale",
            "PurgeInstance",
            "QueryExtensions",
            "ReadSymbols",
            "RefreshBrain",
            "RegexSearch",
            "ReindexSearch",
            "RemoveProject",
            "RemoveRepo",
            "RemoveVault",
            "RepoMapJson",
            "RepoStates",
            "Search",
            "SearchSymbols",
            "ServeUi",
            "ServiceSummaryJson",
            "SetExtension",
            "Shutdown",
            "StaleCheck",
            "StopWatch",
            "SuggestLinksJson",
            "SymbolLookup",
            "WatchCode",
            "WatchVault",
        ];
        for m in ALL_RPC_METHODS {
            let is_read = READ_ONLY_ALLOWED_METHODS.contains(m);
            let is_mut = MUTATING_RPC_METHODS.contains(m);
            assert!(
                is_read ^ is_mut,
                "RPC {m} must be classified as exactly one of read/mutating \
                 (read={is_read}, mutating={is_mut})"
            );
        }
        assert_eq!(
            ALL_RPC_METHODS.len(),
            READ_ONLY_ALLOWED_METHODS.len() + MUTATING_RPC_METHODS.len(),
            "read + mutating classifications must partition the full RPC set"
        );
    }

    /// A minimal inner service that counts how many requests reach it, so we
    /// can prove the guard rejects a mutating RPC BEFORE the handler runs.
    #[derive(Clone)]
    struct CountingService(Arc<AtomicU32>);

    impl tower::Service<http::Request<tonic::body::Body>> for CountingService {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
        >;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: http::Request<tonic::body::Body>) -> Self::Future {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(http::Response::new(tonic::body::Body::default())) })
        }
    }

    /// End-to-end at the actual chokepoint the server installs: a mutating RPC
    /// is rejected with FAILED_PRECONDITION and NEVER reaches the inner
    /// handler, while a read RPC passes straight through.
    #[tokio::test]
    async fn read_only_guard_service_rejects_mutating_and_passes_reads() {
        use tower::Service as _;
        let calls = Arc::new(AtomicU32::new(0));
        let mut guard = ReadOnlyGuard::new(true, CountingService(calls.clone()));

        // Mutating RPC → rejected, inner handler must not run.
        let req = http::Request::builder()
            .uri("/nestweaver.daemon.v1.NestWeaverDaemon/PruneStale")
            .body(tonic::body::Body::default())
            .unwrap();
        let resp = guard.call(req).await.unwrap();
        let status = resp
            .extensions()
            .get::<Status>()
            .expect("rejected response must carry a gRPC Status");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a mutating RPC must be rejected before reaching the handler"
        );

        // Read RPC → passes through to the inner handler.
        let req = http::Request::builder()
            .uri("/nestweaver.daemon.v1.NestWeaverDaemon/Search")
            .body(tonic::body::Body::default())
            .unwrap();
        let _ = guard.call(req).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a read RPC must reach the handler on a read-only replica"
        );
    }

    // ── Read-only replica: write/webhook/admin route mounting ──────────

    /// A read-only replica must NOT mount `/webhook`, even with a secret
    /// configured: no worker drains the queue, so an accepted push is a silent
    /// blackhole. A read-write daemon still mounts it when a secret is set.
    #[test]
    fn read_only_does_not_mount_webhook() {
        // Read-only replica: never mount, regardless of secret.
        assert!(!replica_mounts_webhook(true, true));
        assert!(!replica_mounts_webhook(true, false));
        // Read-write daemon: mount iff a secret is configured.
        assert!(replica_mounts_webhook(false, true));
        assert!(!replica_mounts_webhook(false, false));
    }

    /// A read-only replica must NOT enqueue config repos for initial indexing —
    /// it has no worker to index them, so the jobs would accumulate forever.
    #[test]
    fn read_only_does_not_enqueue_config_repos() {
        assert!(!replica_enqueues_config_repos(true));
        assert!(replica_enqueues_config_repos(false));
    }

    /// A read-only replica must NOT mount the mutating admin API, even with an
    /// admin token configured. A read-write daemon mounts it when a token is
    /// present.
    #[test]
    fn read_only_does_not_mount_admin_api() {
        assert!(!replica_mounts_admin_api(true, true));
        assert!(!replica_mounts_admin_api(true, false));
        assert!(replica_mounts_admin_api(false, true));
        assert!(!replica_mounts_admin_api(false, false));
    }
}

#[cfg(test)]
mod watch_path_allowed_tests {
    use super::*;
    use nestweaver_engine::{RepoConfig, RepoType};

    fn repo_cfg(url: &str, repo_type: Option<RepoType>) -> RepoConfig {
        RepoConfig {
            url: url.to_string(),
            repo_type,
            name: None,
            sparse: None,
            pin_sha: None,
            use_git_activity: None,
            branch: None,
            poll: None,
        }
    }

    /// Config repo URLs are `file://`-prefixed; the allow-list must
    /// strip the scheme before canonicalizing, otherwise every entry drops
    /// out and every watch is rejected.
    #[test]
    fn config_repo_canonical_path_strips_file_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();

        let via_file_url = config_repo_canonical_path(&format!("file://{}", tmp.path().display()));
        assert_eq!(via_file_url.as_deref(), Some(canonical.as_path()));

        let via_plain = config_repo_canonical_path(&tmp.path().display().to_string());
        assert_eq!(via_plain.as_deref(), Some(canonical.as_path()));

        assert!(config_repo_canonical_path("file:///definitely/missing/path").is_none());
    }

    /// Without an instance config, an explicit `--repo` path must be
    /// watchable (guarded by the unsafe-root denylist instead of failing
    /// `failed_precondition`).
    #[test]
    fn no_config_allows_explicit_safe_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(watch_path_allowed(None, &canonical, "repo", false).is_ok());
        assert!(watch_path_allowed(None, &canonical, "vault", true).is_ok());
    }

    /// Without a config, system roots are still refused.
    #[test]
    fn no_config_rejects_unsafe_roots() {
        assert!(watch_path_allowed(None, std::path::Path::new("/"), "repo", false).is_err());
        assert!(watch_path_allowed(None, std::path::Path::new("/usr"), "repo", false).is_err());
    }

    /// With a config, a repo registered via `file://` URL must pass the
    /// allow-list (previously rejected because the scheme was never stripped).
    #[test]
    fn config_allows_file_url_registered_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir(&repo_dir).unwrap();
        let canonical = std::fs::canonicalize(&repo_dir).unwrap();
        let repos = vec![repo_cfg(
            &format!("file://{}", repo_dir.display()),
            Some(RepoType::Code),
        )];
        assert!(watch_path_allowed(Some(&repos), &canonical, "repo", false).is_ok());
        // A subdirectory of a registered repo also passes (starts_with).
        let sub = repo_dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let sub = std::fs::canonicalize(&sub).unwrap();
        assert!(watch_path_allowed(Some(&repos), &sub, "repo", false).is_ok());
    }

    /// Paths outside the config's registered sources are still rejected, and
    /// `vault_only` skips code-type entries.
    #[test]
    fn config_rejects_unregistered_and_vault_filter_applies() {
        let tmp = tempfile::tempdir().unwrap();
        let code_dir = tmp.path().join("code");
        let vault_dir = tmp.path().join("vault");
        std::fs::create_dir(&code_dir).unwrap();
        std::fs::create_dir(&vault_dir).unwrap();
        let code_canon = std::fs::canonicalize(&code_dir).unwrap();
        let vault_canon = std::fs::canonicalize(&vault_dir).unwrap();
        let repos = vec![
            repo_cfg(
                &format!("file://{}", code_dir.display()),
                Some(RepoType::Code),
            ),
            repo_cfg(
                &format!("file://{}", vault_dir.display()),
                Some(RepoType::Vault),
            ),
        ];

        assert!(watch_path_allowed(Some(&repos), &code_canon, "repo", false).is_ok());
        // Vault watch must not match a code-type entry.
        assert!(watch_path_allowed(Some(&repos), &code_canon, "vault", true).is_err());
        assert!(watch_path_allowed(Some(&repos), &vault_canon, "vault", true).is_ok());
        // Unregistered path rejected.
        let other = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(watch_path_allowed(Some(&repos), &other, "repo", false).is_err());
    }
}

#[cfg(test)]
mod watcher_e2e_tests {
    use super::*;
    use hyper_util::rt::TokioIo;
    use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;
    use tonic::transport::{Channel, Endpoint, Uri};
    use tower::service_fn;

    async fn connect_uds(sock: &std::path::Path) -> Result<NestWeaverDaemonClient<Channel>, ()> {
        let path = sock.to_path_buf();
        let channel = Endpoint::try_from("http://[::]:50051")
            .map_err(|_| ())?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|_| ())?;
        Ok(NestWeaverDaemonClient::new(channel))
    }

    /// End-to-end over a real unix socket:
    ///  - health_check reports the daemon's own PID (CLI pidfile cross-check);
    ///  - a watch session whose CLI vanished (no StopWatch — the kill -9
    ///    scenario) blocks a plain re-watch but is adoptable with force;
    ///  - shutdown with an ACTIVE watcher completes promptly instead of
    ///    hanging on the watcher's blocking thread until the drain ceiling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn daemon_e2e_pid_watch_force_and_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("brain.lbug");
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let db_for_server = db_path.clone();
        let server =
            tokio::spawn(async move { run_server(&db_for_server, None, None, None).await });

        let instance_id = lifecycle::instance_id_from_db_path(&db_path);
        let sock = lifecycle::socket_path(&instance_id);

        // Wait for the daemon's socket to accept connections.
        let mut client = None;
        for _ in 0..100 {
            if sock.exists()
                && let Ok(c) = connect_uds(&sock).await
            {
                client = Some(c);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let mut client = client.expect("daemon socket did not come up within 10s");

        // The socket reports the daemon's own PID (in-process here, so
        // it equals the test process PID).
        let health = client
            .health_check(nestweaver_proto::HealthCheckRequest {})
            .await
            .expect("health check")
            .into_inner();
        assert_eq!(health.pid, std::process::id());

        let repo_str = repo.to_string_lossy().into_owned();
        let mk_req = |force: bool| nestweaver_proto::WatchCodeRequest {
            repo_path: repo_str.clone(),
            instance_id: String::new(),
            force,
        };

        let r1 = client
            .watch_code(mk_req(false))
            .await
            .expect("watch_code")
            .into_inner();
        assert!(r1.ok, "first watch must start: {}", r1.message);

        // Simulated orphan: the first "CLI" never calls StopWatch.
        let r2 = client
            .watch_code(mk_req(false))
            .await
            .expect("watch_code")
            .into_inner();
        assert!(!r2.ok, "second watch without force must be refused");
        assert!(
            r2.message.contains("already running"),
            "unexpected refusal message: {}",
            r2.message
        );

        // Force adopts the orphaned watcher slot.
        let r3 = client
            .watch_code(mk_req(true))
            .await
            .expect("watch_code")
            .into_inner();
        assert!(
            r3.ok,
            "force watch must adopt the orphaned slot: {}",
            r3.message
        );

        // Shutdown with the watcher ACTIVE must stop it and exit
        // promptly — the pre-fix SIGTERM/cleanup path left the watcher's
        // blocking thread running, pinning process exit.
        client
            .shutdown(nestweaver_proto::ShutdownRequest {})
            .await
            .expect("shutdown RPC");

        let finished = tokio::time::timeout(std::time::Duration::from_secs(30), server)
            .await
            .expect("daemon must exit promptly with an active watcher (no drain hang)");
        finished
            .expect("server task panicked")
            .expect("run_server error");

        // run_server writes its log under the real per-user state dir —
        // clean up this instance's artifacts.
        let _ = std::fs::remove_dir_all(lifecycle::log_dir(&instance_id));
        let _ = std::fs::remove_dir_all(lifecycle::runtime_dir(&instance_id));
    }
}
