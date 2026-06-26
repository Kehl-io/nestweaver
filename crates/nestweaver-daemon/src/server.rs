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
use tonic::{Request, Response, Status};

use crate::lifecycle;
use crate::safeguards::{ClientRateLimiters, QuerySafeguards, RateLimitConfig, with_safeguard};

// ── State ───────────────────────────────────────────────────────────

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
pub struct DaemonState {
    pub store: Arc<GraphStore>,
    pub tantivy: Option<Arc<TantivyIndex>>,
    pub db_path: PathBuf,
    pub instance_id: String,
    pub start_time: Instant,
    pub active_reads: Arc<AtomicU32>,
    pub active_writes: Arc<AtomicU32>,
    pub idle_notify: Arc<Notify>,
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub watcher_stop: std::sync::Mutex<Option<nestweaver_engine::ShutdownHandle>>,
    /// Parsed `nestweaver-instance.toml` if `--config` was supplied at
    /// daemon start. Used by tool dispatch (e.g. F6 `[ranking]` priors in
    /// `brain_search`) via the `set_current_instance_config` thread-local.
    pub instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
    /// Lazily-loaded embedding model for semantic search. Populated by a
    /// background task when the `embed` feature is enabled.
    pub embed_model: Arc<tokio::sync::RwLock<Option<Arc<dyn nestweaver_engine::EmbedQueryFn>>>>,
    /// Serializes write RPCs so only one runs at a time (KùzuDB allows a
    /// single write transaction).
    pub write_mutex: Arc<tokio::sync::Mutex<()>>,
    /// Whether this daemon is running in server mode (TCP, no local source files).
    pub server_mode: bool,
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
}

/// The gRPC service implementation. Wraps shared state in an `Arc`.
pub struct DaemonService {
    state: Arc<DaemonState>,
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
    ) -> Result<Response<JsonResponse>, Status> {
        let safeguards = &self.state.safeguards;
        let tool = tool_name.to_string();
        let handler = self.dispatch_json_tool_inner(tool_name, args_json);

        if self.state.server_mode {
            with_safeguard(&tool, safeguards, None, handler).await
        } else {
            handler.await
        }
    }

    /// Inner dispatch without safeguard wrapper. Extracted so
    /// `with_safeguard` can race it against a timeout.
    async fn dispatch_json_tool_inner(
        &self,
        tool_name: &str,
        args_json: &str,
    ) -> Result<Response<JsonResponse>, Status> {
        let t0 = std::time::Instant::now();
        let _guard = ConnectionGuard::read(&self.state);

        let state = self.state.clone();
        let tool_name = tool_name.to_string();
        let args_json = args_json.to_string();

        // Read the embed model Arc outside the blocking thread, then drop
        // the RwLock guard before any further awaits.
        let embed_arc = {
            let guard = self.state.embed_model.read().await;
            guard.clone()
        };

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
            if state.server_mode {
                // Clamp depth parameter.
                let client_depth = args
                    .get("depth")
                    .or_else(|| args.get("max_depth"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let effective_depth = state.safeguards.effective_depth(&tool_name, client_depth)?;
                if args.get("depth").is_some() {
                    args["depth"] = serde_json::json!(effective_depth);
                } else if args.get("max_depth").is_some() {
                    args["max_depth"] = serde_json::json!(effective_depth);
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
            }

            let embed_ref = embed_arc.as_deref();
            tracing::debug!(
                has_model = embed_ref.is_some(),
                "dispatch_json_tool embed_model status"
            );

            let t_dispatch = std::time::Instant::now();
            let value = nestweaver_mcp::tools::dispatch(
                &state.store,
                state.tantivy.as_deref(),
                &tool_name,
                args,
                embed_ref,
            )
            .map_err(|e| Status::internal(format!("tool {tool_name} failed: {e}")))?;
            tracing::debug!(
                tool = %tool_name,
                elapsed_ms = t_dispatch.elapsed().as_millis(),
                "dispatch completed"
            );

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
    ) -> Result<serde_json::Value, Status> {
        let safeguards = &self.state.safeguards;
        let tool = tool_name.to_string();
        let handler = self.dispatch_tool_json_inner(tool_name, args);

        if self.state.server_mode {
            with_safeguard(&tool, safeguards, None, handler).await
        } else {
            handler.await
        }
    }

    /// Inner dispatch without safeguard wrapper.
    async fn dispatch_tool_json_inner(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, Status> {
        let t0 = std::time::Instant::now();
        let _guard = ConnectionGuard::read(&self.state);

        let state = self.state.clone();
        let tool_name = tool_name.to_string();

        // Read the embed model Arc outside the blocking thread, then drop
        // the RwLock guard before any further awaits.
        let embed_arc = {
            let guard = self.state.embed_model.read().await;
            guard.clone()
        };

        let tool_name_for_log = tool_name.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, Status> {
            nestweaver_mcp::tools::set_current_db_path(state.db_path.clone());
            nestweaver_mcp::tools::set_lite_mode(false);
            nestweaver_mcp::tools::set_current_instance_config(state.instance_cfg.clone());
            nestweaver_mcp::tools::set_server_mode(state.server_mode);

            let embed_ref = embed_arc.as_deref();

            let t_dispatch = std::time::Instant::now();
            let value = nestweaver_mcp::tools::dispatch(
                &state.store,
                state.tantivy.as_deref(),
                &tool_name,
                args,
                embed_ref,
            )
            .map_err(|e| Status::internal(format!("tool {tool_name} failed: {e}")))?;
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

    /// Build an `on_change` callback that queues un-embedded nodes for
    /// background embedding after every watcher batch.  Returns `None`
    /// when the `embed` feature is disabled or the model is not yet loaded.
    #[cfg(feature = "embed")]
    fn make_embed_on_change(
        embed_model: Arc<tokio::sync::RwLock<Option<Arc<dyn nestweaver_engine::EmbedQueryFn>>>>,
        store: Arc<nestweaver_store::GraphStore>,
    ) -> Option<Box<dyn Fn() + Send>> {
        Some(Box::new(move || {
            // Peek at the model without blocking async code — we are already
            // in a blocking thread (inside spawn_blocking).
            let model = {
                let guard = embed_model.blocking_read();
                guard.clone()
            };
            let Some(model) = model else { return };

            let store = store.clone();
            // Fire-and-forget a new blocking task so the watcher callback
            // returns quickly and the watcher loop is not stalled.
            drop(tokio::task::spawn_blocking(move || {
                let mut embedded = 0u32;
                let limit: usize = 64; // Max nodes per watcher cycle

                // Symbols
                if let Ok(symbols) = store.list_all_symbols() {
                    for sym in symbols.iter().filter(|s| s.embedding.is_none()).take(limit) {
                        let text = nestweaver_embed::preprocess::symbol_embed_text(
                            &sym.kind.to_string(),
                            &sym.name,
                            None,
                        );
                        match model.embed_query(&text) {
                            Ok(emb) => {
                                store.add_embedding(&sym.uid, emb);
                                embedded += 1;
                            }
                            Err(e) => {
                                tracing::warn!(uid = %sym.uid, "embedding failed: {e}");
                            }
                        }
                    }
                }

                let remaining = limit.saturating_sub(embedded as usize);

                // Notes
                if remaining > 0
                    && let Ok(notes) = store.list_notes(None)
                {
                    for note in notes
                        .iter()
                        .filter(|n| n.embedding.is_none())
                        .take(remaining)
                    {
                        let text = nestweaver_embed::preprocess::note_embed_text(&note.title, None);
                        match model.embed_query(&text) {
                            Ok(emb) => {
                                store.add_embedding(&note.uid, emb);
                                embedded += 1;
                            }
                            Err(e) => {
                                tracing::warn!(uid = %note.uid, "embedding failed: {e}");
                            }
                        }
                    }
                }

                let remaining = limit.saturating_sub(embedded as usize);

                // Headings
                if remaining > 0
                    && let Ok(headings) = store.list_all_headings()
                {
                    for heading in headings
                        .iter()
                        .filter(|h| h.embedding.is_none())
                        .take(remaining)
                    {
                        let text =
                            nestweaver_embed::preprocess::heading_embed_text("", &heading.text);
                        match model.embed_query(&text) {
                            Ok(emb) => {
                                store.add_embedding(&heading.uid, emb);
                                embedded += 1;
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
            }));
        }))
    }

    #[cfg(not(feature = "embed"))]
    fn make_embed_on_change(
        _embed_model: Arc<tokio::sync::RwLock<Option<Arc<dyn nestweaver_engine::EmbedQueryFn>>>>,
        _store: Arc<nestweaver_store::GraphStore>,
    ) -> Option<Box<dyn Fn() + Send>> {
        None
    }
}

// ── Trait impl ──────────────────────────────────────────────────────

/// Maps each gRPC RPC name to the MCP tool name it dispatches to.
macro_rules! json_rpc {
    ($self:ident, $request:ident, $tool:expr) => {{
        let req = $request.into_inner();
        $self.dispatch_json_tool($tool, &req.args_json).await
    }};
}

type ProgressStream = tokio_stream::wrappers::ReceiverStream<Result<IndexProgress, Status>>;

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
        }))
    }

    async fn shutdown(
        &self,
        _request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        tracing::info!("shutdown requested via gRPC — draining active writes");

        if let Ok(mut guard) = self.state.watcher_stop.lock()
            && let Some(handle) = guard.take()
        {
            tracing::info!("stopping active watcher before shutdown");
            handle.stop();
        }

        let state = self.state.clone();
        tokio::spawn(async move {
            let ceiling = std::env::var("NESTWEAVER_DRAIN_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(660);

            let timeout = std::time::Duration::from_secs(ceiling);
            let half = std::time::Duration::from_secs(ceiling / 2);
            let ninety = std::time::Duration::from_secs(ceiling * 9 / 10);
            let start = tokio::time::Instant::now();
            let mut warned_half = false;
            let mut warned_ninety = false;

            loop {
                let writes = state.active_writes.load(Ordering::Relaxed);
                if writes == 0 {
                    tracing::info!("no active writes — shutting down");
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

    // ── Watching ─────────────────────────────────────────────────────

    async fn watch_vault(
        &self,
        request: Request<WatchVaultRequest>,
    ) -> Result<Response<WatchVaultResponse>, Status> {
        let req = request.into_inner();
        let vault_path = PathBuf::from(&req.vault_path);
        let vault_name = req.vault_name.clone();
        let instance_id = if req.instance_id.is_empty() {
            self.state.instance_id.clone()
        } else {
            req.instance_id.clone()
        };
        let extra_patterns = req.extra_ignore_patterns.clone();

        if !vault_path.exists() || !vault_path.is_dir() {
            return Ok(Response::new(WatchVaultResponse {
                ok: false,
                message: format!("vault path is not a directory: {}", vault_path.display()),
            }));
        }

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

        // Hold the lock across check + store to prevent TOCTOU race.
        {
            let mut guard = self
                .state
                .watcher_stop
                .lock()
                .map_err(|e| Status::internal(format!("watcher_stop lock poisoned: {e}")))?;
            if guard.is_some() {
                return Ok(Response::new(WatchVaultResponse {
                    ok: false,
                    message: "A watcher is already running. Stop it first with StopWatch."
                        .to_string(),
                }));
            }
            *guard = Some(shutdown_handle);
        }

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();
        let state = self.state.clone();
        let store = self.state.store.clone();
        let on_change =
            Self::make_embed_on_change(self.state.embed_model.clone(), self.state.store.clone());

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

            if let Ok(mut guard) = state.watcher_stop.lock() {
                *guard = None;
            }
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
        let req = request.into_inner();
        let repo_path = PathBuf::from(&req.repo_path);
        let instance_id = if req.instance_id.is_empty() {
            self.state.instance_id.clone()
        } else {
            req.instance_id.clone()
        };

        if !repo_path.exists() || !repo_path.is_dir() {
            return Ok(Response::new(WatchCodeResponse {
                ok: false,
                message: format!("repo path is not a directory: {}", repo_path.display()),
            }));
        }

        let db_path = self.state.db_path.clone();

        let watcher = nestweaver_engine::CodeWatcher::new(&db_path, &repo_path, &instance_id);
        let shutdown_handle = watcher.shutdown_handle();

        // Hold the lock across check + store to prevent TOCTOU race.
        {
            let mut guard = self
                .state
                .watcher_stop
                .lock()
                .map_err(|e| Status::internal(format!("watcher_stop lock poisoned: {e}")))?;
            if guard.is_some() {
                return Ok(Response::new(WatchCodeResponse {
                    ok: false,
                    message: "A watcher is already running. Stop it first with StopWatch."
                        .to_string(),
                }));
            }
            *guard = Some(shutdown_handle);
        }

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();
        let state = self.state.clone();
        let store = self.state.store.clone();
        let on_change =
            Self::make_embed_on_change(self.state.embed_model.clone(), self.state.store.clone());

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

            if let Ok(mut guard) = state.watcher_stop.lock() {
                *guard = None;
            }
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
        let mut guard = self
            .state
            .watcher_stop
            .lock()
            .map_err(|e| Status::internal(format!("watcher_stop lock poisoned: {e}")))?;

        if let Some(handle) = guard.take() {
            tracing::info!("stop_watch: stopping active watcher");
            handle.stop();
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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let req = request.into_inner();
        let state = self.state.clone();

        let app_state = nestweaver_web::state::AppState::new_with_arc_tantivy(
            state.store.clone(),
            state.tantivy.clone(),
            state.db_path.clone(),
        );

        let port = if req.port > 0 { req.port as u16 } else { 3000 };
        let open_browser = req.open_browser;

        // Spawn web server as a background task inside the daemon.
        tokio::spawn(async move {
            if let Err(e) = nestweaver_web::start_server(app_state, port, open_browser).await {
                tracing::error!("UI server error: {e}");
            }
        });

        // If watch mode requested, spawn a CodeWatcher.
        if req.watch && !req.watch_repo_path.is_empty() {
            let watch_db = state.db_path.clone();
            let watch_repo = std::path::PathBuf::from(&req.watch_repo_path);
            let watch_instance = if req.watch_instance_id.is_empty() {
                "default".to_string()
            } else {
                req.watch_instance_id.clone()
            };
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
        }))
    }

    // ── Indexing ─────────────────────────────────────────────────────

    type IndexRepoStream = ProgressStream;

    async fn index_repo(
        &self,
        request: Request<IndexRepoRequest>,
    ) -> Result<Response<Self::IndexRepoStream>, Status> {
        let req = request.into_inner();
        let repo_path = PathBuf::from(&req.repo_path);
        let state = self.state.clone();
        let force = req.force;
        let with_trigrams = req.with_trigrams;
        let with_git_activity = req.with_git_activity;
        let name = if req.name.is_empty() {
            None
        } else {
            Some(req.name.clone())
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<IndexProgress, Status>>(16);

        let guard = ConnectionGuard::write(&self.state);
        let write_lock = self.state.write_mutex.clone();
        tokio::task::spawn_blocking(move || {
            let _write_lock = write_lock.blocking_lock();
            let _guard = guard;
            let repo_url = format!("file://{}", repo_path.display());

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

            match nestweaver_engine::index_directory_with_store(
                &state.store,
                &repo_path,
                &state.db_path,
                &state.instance_id,
                &repo_url,
                &indexed_sha,
                force,
                name.as_deref(),
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
        let req = request.into_inner();
        let vault_path = PathBuf::from(&req.vault_path);
        let vault_name = req.vault_name.clone();
        let extra_patterns = req.extra_ignore_patterns.clone();
        let instance_id = if req.instance_id.is_empty() {
            self.state.instance_id.clone()
        } else {
            req.instance_id.clone()
        };
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
        let req = request.into_inner();
        let config_path = PathBuf::from(&req.config_path);
        let instance_id = if req.instance_id.is_empty() {
            self.state.instance_id.clone()
        } else {
            req.instance_id.clone()
        };
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
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            let notes_deleted = state
                .store
                .delete_vault_cascade(&req.vault_uid)
                .map_err(|e| Status::internal(format!("delete_vault_cascade failed: {e:#}")))?;

            if let Some(ref tantivy) = state.tantivy
                && tantivy.has_writer()
            {
                match tantivy.reindex_from_store(&state.store) {
                    Ok(n) => tracing::info!(docs = n, "Tantivy reindexed after vault removal"),
                    Err(e) => {
                        tracing::warn!(error = %e, "Tantivy reindex failed after vault removal")
                    }
                }
            }

            Ok::<_, Status>(RemoveVaultResponse {
                notes_deleted: notes_deleted as u64,
            })
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    async fn remove_repo(
        &self,
        request: Request<RemoveRepoRequest>,
    ) -> Result<Response<RemoveRepoResponse>, Status> {
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            let (file_count, sym_count) = state
                .store
                .bulk_delete_repo_files_and_symbols(&req.repo_uid)
                .map_err(|e| {
                    Status::internal(format!("bulk_delete_repo_files_and_symbols failed: {e:#}"))
                })?;

            state
                .store
                .clear_repo_derived_nodes(&req.repo_uid)
                .map_err(|e| Status::internal(format!("clear_repo_derived_nodes failed: {e:#}")))?;

            state
                .store
                .delete_repo_node(&req.repo_uid)
                .map_err(|e| Status::internal(format!("delete_repo_node failed: {e:#}")))?;

            if let Some(ref tantivy) = state.tantivy
                && tantivy.has_writer()
            {
                match tantivy.reindex_from_store(&state.store) {
                    Ok(n) => tracing::info!(docs = n, "Tantivy reindexed after repo removal"),
                    Err(e) => {
                        tracing::warn!(error = %e, "Tantivy reindex failed after repo removal")
                    }
                }
            }

            Ok::<_, Status>(RemoveRepoResponse {
                files_deleted: file_count as u64,
                symbols_deleted: sym_count as u64,
            })
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    async fn remove_project(
        &self,
        request: Request<RemoveProjectRequest>,
    ) -> Result<Response<RemoveProjectResponse>, Status> {
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            // Look up project name before deleting.
            let projects = state
                .store
                .list_projects()
                .map_err(|e| Status::internal(format!("list_projects failed: {e:#}")))?;
            let project_name = projects
                .iter()
                .find(|p| p.uid == req.project_uid)
                .map(|p| p.name.clone())
                .unwrap_or_default();

            state
                .store
                .delete_project_edges(&req.project_uid)
                .map_err(|e| Status::internal(format!("delete_project_edges failed: {e:#}")))?;

            state
                .store
                .delete_project_node(&req.project_uid)
                .map_err(|e| Status::internal(format!("delete_project_node failed: {e:#}")))?;

            Ok::<_, Status>(RemoveProjectResponse { project_name })
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    async fn prune_stale(
        &self,
        request: Request<PruneStaleRequest>,
    ) -> Result<Response<PruneStaleResponse>, Status> {
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let _req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            let mut removed_repos = Vec::new();
            let mut removed_vaults = Vec::new();

            // Prune stale repos (path no longer exists on disk).
            let repos = state
                .store
                .list_repos(None)
                .map_err(|e| Status::internal(format!("list_repos failed: {e:#}")))?;

            for repo in &repos {
                let path = repo.url.strip_prefix("file://").unwrap_or(&repo.url);
                if !Path::new(path).exists() {
                    state
                        .store
                        .bulk_delete_repo_files_and_symbols(&repo.uid)
                        .map_err(|e| {
                            Status::internal(format!(
                                "bulk_delete_repo_files_and_symbols failed: {e:#}"
                            ))
                        })?;
                    state
                        .store
                        .clear_repo_derived_nodes(&repo.uid)
                        .map_err(|e| {
                            Status::internal(format!("clear_repo_derived_nodes failed: {e:#}"))
                        })?;
                    state
                        .store
                        .delete_repo_node(&repo.uid)
                        .map_err(|e| Status::internal(format!("delete_repo_node failed: {e:#}")))?;
                    removed_repos.push(repo.name.clone().unwrap_or_else(|| repo.url.clone()));
                }
            }

            // Prune stale vaults (root_path no longer exists on disk).
            let vaults = state
                .store
                .list_vaults(None)
                .map_err(|e| Status::internal(format!("list_vaults failed: {e:#}")))?;

            for vault in &vaults {
                if !Path::new(&vault.root_path).exists() {
                    state.store.delete_vault_cascade(&vault.uid).map_err(|e| {
                        Status::internal(format!("delete_vault_cascade failed: {e:#}"))
                    })?;
                    removed_vaults.push(vault.name.clone());
                }
            }

            // Reindex Tantivy if anything was removed.
            if (!removed_repos.is_empty() || !removed_vaults.is_empty())
                && let Some(ref tantivy) = state.tantivy
                && tantivy.has_writer()
            {
                match tantivy.reindex_from_store(&state.store) {
                    Ok(n) => tracing::info!(docs = n, "Tantivy reindexed after prune_stale"),
                    Err(e) => {
                        tracing::warn!(error = %e, "Tantivy reindex failed after prune_stale")
                    }
                }
            }

            Ok::<_, Status>(PruneStaleResponse {
                removed_repos,
                removed_vaults,
            })
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking failed: {e}")))?;

        result.map(Response::new)
    }

    async fn merge_instance(
        &self,
        request: Request<MergeInstanceRequest>,
    ) -> Result<Response<MergeInstanceResponse>, Status> {
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let req = request.into_inner();
        let state = self.state.clone();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || {
            let result = state
                .store
                .merge_instance_ids(&req.from_id, &req.to_id)
                .map_err(|e| Status::internal(format!("merge_instance_ids failed: {e:#}")))?;

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
        let req = request.into_inner();
        let instance_id = req.instance_id.clone();
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

            match state.store.purge_instance(&instance_id) {
                Ok(result) => {
                    // Rebuild Tantivy so BM25 search reflects purged vaults.
                    if let Some(ref tantivy) = state.tantivy
                        && tantivy.has_writer()
                    {
                        match tantivy.reindex_from_store(&state.store) {
                            Ok(n) => {
                                tracing::info!(docs = n, "Tantivy reindexed after instance purge")
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Tantivy reindex failed after instance purge")
                            }
                        }
                    }

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
                        message: format!("PurgeInstance failed: {e:#}"),
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
        let tantivy = self
            .state
            .tantivy
            .as_ref()
            .filter(|t| t.has_writer())
            .ok_or_else(|| {
                Status::failed_precondition("daemon has no writer-mode Tantivy index")
            })?;
        let count = tantivy
            .reindex_from_store(&self.state.store)
            .map_err(|e| Status::internal(format!("reindex failed: {e:#}")))?;
        Ok(Response::new(ReindexSearchResponse {
            document_count: count as i32,
        }))
    }

    // ── Read RPCs — typed hot-path ─────────────────────────────────

    async fn search(
        &self,
        r: Request<BrainSearchRequest>,
    ) -> Result<Response<BrainSearchResponse>, Status> {
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

        let value = self.dispatch_tool_json("brain_search", args).await?;

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

        let results = value
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

        Ok(Response::new(BrainSearchResponse {
            query: query_echo,
            engine,
            total_matches,
            results,
            expansion_terms,
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

        let value = self.dispatch_tool_json("brain_context", args).await?;
        let result_json = serde_json::to_string(&value)
            .map_err(|e| Status::internal(format!("failed to serialize result: {e}")))?;

        Ok(Response::new(BrainContextResponse { result_json }))
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

        let value = self.dispatch_tool_json("project_context", args).await?;
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

        let value = self.dispatch_tool_json("note_get", args).await?;

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
        }))
    }

    async fn brain_status(
        &self,
        _r: Request<BrainStatusRequest>,
    ) -> Result<Response<BrainStatusResponse>, Status> {
        let args = serde_json::json!({});
        let value = self.dispatch_tool_json("brain_status", args).await?;

        let indexing_active = self.state.indexing_active.load(Ordering::Relaxed);
        let indexing_repo = if indexing_active {
            self.state.indexing_repo.read().await.clone()
        } else {
            String::new()
        };
        let queue_depth = self.state.indexing_queue_depth.load(Ordering::Relaxed) as i32;

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

        let value = self.dispatch_tool_json("hub_nodes", args).await?;
        let result_json = serde_json::to_string(&value)
            .map_err(|e| Status::internal(format!("failed to serialize result: {e}")))?;

        Ok(Response::new(HubNodesResponse { result_json }))
    }

    async fn brain_status_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let req = r.into_inner();
        let resp = self
            .dispatch_json_tool("brain_status", &req.args_json)
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
                        repo_name: r.name.unwrap_or_else(|| {
                            r.url
                                .strip_prefix("file://")
                                .unwrap_or(&r.url)
                                .rsplit('/')
                                .next()
                                .unwrap_or(&r.url)
                                .to_string()
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
        json_rpc!(self, r, "set_extension")
    }

    async fn query_extensions(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        json_rpc!(self, r, "query_extensions")
    }

    // ── Read RPCs — direct store access (no MCP tool) ──────────────

    #[allow(clippy::result_large_err)]
    async fn list_repos_json(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonResponse>, Status> {
        let _guard = ConnectionGuard::read(&self.state);
        let state = self.state.clone();
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let _args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let _args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

        let result = tokio::task::spawn_blocking(move || {
            let name_or_uid = args
                .get("name_or_uid")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lookup = nestweaver_engine::lookup_symbol(&state.store, name_or_uid)
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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let _args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

        let result = tokio::task::spawn_blocking(move || {
            let cache_path = state.db_path.with_extension("manifests.json");
            let manifests = nestweaver_engine::load_manifest_cache(&cache_path).unwrap_or_default();
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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

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
        let args: serde_json::Value =
            serde_json::from_str(&r.into_inner().args_json).unwrap_or(serde_json::Value::Null);

        let result = tokio::task::spawn_blocking(move || {
            let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
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
            let result = nestweaver_engine::analyze_blast_radius(
                &state.store,
                &changed_files,
                depth,
                Some(&state.db_path),
            )
            .map_err(|e| Status::internal(format!("analyze_blast_radius failed: {e:#}")))?;
            serde_json::to_string(&result)
                .map_err(|e| Status::internal(format!("serialization failed: {e:#}")))
        })
        .await
        .map_err(|e| Status::internal(format!("spawn_blocking panicked: {e}")))?;

        result.map(|j| Response::new(JsonResponse { result_json: j }))
    }

    // ── Embedding ───────────────────────────────────────────────────

    #[allow(clippy::result_large_err)]
    async fn embed(
        &self,
        request: Request<EmbedRequest>,
    ) -> Result<Response<EmbedResponse>, Status> {
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        #[cfg(not(feature = "embed"))]
        {
            let _ = request;
            return Err(Status::unavailable(
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

            let do_symbols = scope == "all" || scope == "symbols";
            let do_notes = scope == "all" || scope == "notes";
            let do_headings = scope == "all" || scope == "headings";

            if !do_symbols && !do_notes && !do_headings {
                return Err(Status::invalid_argument(format!(
                    "unknown scope '{scope}': expected one of: all, symbols, notes, headings"
                )));
            }

            let model = {
                let guard = self.state.embed_model.read().await;
                guard.clone()
            };

            let Some(model) = model else {
                return Err(Status::unavailable(
                    "embedding model is not loaded — it may still be initializing",
                ));
            };

            let store = self.state.store.clone();

            let result = tokio::task::spawn_blocking(move || {
                let mut succeeded = 0u32;
                let mut failed = 0u32;

                if do_symbols && let Ok(symbols) = store.list_all_symbols() {
                    let to_embed: Vec<_> = if force {
                        symbols.iter().collect()
                    } else {
                        symbols.iter().filter(|s| s.embedding.is_none()).collect()
                    };
                    for chunk in to_embed.chunks(batch_size) {
                        for sym in chunk {
                            let text = nestweaver_embed::preprocess::symbol_embed_text(
                                &sym.kind.to_string(),
                                &sym.name,
                                None,
                            );
                            match model.embed_query(&text) {
                                Ok(emb) => {
                                    store.add_embedding(&sym.uid, emb);
                                    succeeded += 1;
                                }
                                Err(e) => {
                                    tracing::warn!(uid = %sym.uid, "embedding failed: {e}");
                                    failed += 1;
                                }
                            }
                        }
                    }
                }

                if do_notes && let Ok(notes) = store.list_notes(None) {
                    let to_embed: Vec<_> = if force {
                        notes.iter().collect()
                    } else {
                        notes.iter().filter(|n| n.embedding.is_none()).collect()
                    };
                    for chunk in to_embed.chunks(batch_size) {
                        for note in chunk {
                            let text =
                                nestweaver_embed::preprocess::note_embed_text(&note.title, None);
                            match model.embed_query(&text) {
                                Ok(emb) => {
                                    store.add_embedding(&note.uid, emb);
                                    succeeded += 1;
                                }
                                Err(e) => {
                                    tracing::warn!(uid = %note.uid, "embedding failed: {e}");
                                    failed += 1;
                                }
                            }
                        }
                    }
                }

                if do_headings && let Ok(headings) = store.list_all_headings() {
                    let to_embed: Vec<_> = if force {
                        headings.iter().collect()
                    } else {
                        headings.iter().filter(|h| h.embedding.is_none()).collect()
                    };
                    for chunk in to_embed.chunks(batch_size) {
                        for heading in chunk {
                            let text =
                                nestweaver_embed::preprocess::heading_embed_text("", &heading.text);
                            match model.embed_query(&text) {
                                Ok(emb) => {
                                    store.add_embedding(&heading.uid, emb);
                                    succeeded += 1;
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

                tracing::info!(succeeded, failed, "embed RPC completed");
                Ok::<_, Status>(EmbedResponse { succeeded, failed })
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
}

pub async fn run_server(
    db_path: &Path,
    idle_timeout: Option<Duration>,
    config_path: Option<&Path>,
    server_opts: Option<ServerOpts>,
) -> Result<(), anyhow::Error> {
    // Canonicalize if possible, but don't fail if the DB doesn't exist yet.
    // The DB will be created by GraphStore::open_or_create below.
    let db_path = std::fs::canonicalize(db_path).unwrap_or_else(|_| {
        // Ensure parent directory exists so the DB can be created
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        db_path.to_path_buf()
    });

    let instance_id = lifecycle::instance_id_from_db_path(&db_path);
    let instance_label = lifecycle::instance_label_from_db_path(&db_path);

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

    // Open the graph store with write access — the daemon is the sole DB owner.
    let store = match GraphStore::open_or_create(&db_path) {
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
    };

    // Load sidecars (PageRank, interaction scores).
    nestweaver_engine::migrate_sidecar(&db_path, "pagerank.json", ".pagerank.json");
    let pr_path = nestweaver_engine::sidecar_path(&db_path, ".pagerank.json");
    let _ = store.load_pagerank_cache(&pr_path);

    if let Some(scores) = nestweaver_engine::load_interaction_scores(&db_path) {
        store.load_interaction_cache(scores);
    }

    // Open Tantivy index with a writer so the daemon can update the
    // search index after indexing operations (vault/repo).  Fall back to
    // reader-only when the writer lock is held by another process (e.g. a
    // running brain watcher), and finally to None when the index doesn't
    // exist at all.
    let tantivy_path = nestweaver_mcp::tantivy_sidecar_path(&db_path);
    let tantivy = match TantivyIndex::open_or_create(&tantivy_path) {
        Ok(idx) => {
            tracing::info!(
                docs = idx.doc_count(),
                path = %tantivy_path.display(),
                "Tantivy index open (read-write)"
            );
            Some(Arc::new(idx))
        }
        Err(_) => match TantivyIndex::open_reader_only(&tantivy_path) {
            Ok(idx) => {
                tracing::info!(
                    docs = idx.doc_count(),
                    path = %tantivy_path.display(),
                    "Tantivy index open (reader-only fallback)"
                );
                Some(Arc::new(idx))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "could not open Tantivy index — search will use substring fallback"
                );
                None
            }
        },
    };

    let idle_notify = Arc::new(Notify::new());
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Load the InstanceConfig once at start if `--config` was supplied so
    // tool dispatch (e.g. F6 `[ranking]` priors in `brain_search`) can apply
    // it without re-parsing the file per RPC. A missing/unreadable file is
    // logged but non-fatal — the daemon still serves with built-in defaults.
    let instance_cfg =
        config_path.and_then(|p| match nestweaver_engine::InstanceConfig::from_file(p) {
            Ok(c) => {
                tracing::info!(
                    config = %p.display(),
                    "loaded instance config (ranking, response, features)"
                );
                Some(Arc::new(c))
            }
            Err(e) => {
                tracing::warn!(
                    config = %p.display(),
                    error = %e,
                    "failed to load instance config — ranking/response settings will use defaults"
                );
                None
            }
        });

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

    let state = Arc::new(DaemonState {
        store: Arc::new(store),

        tantivy,
        db_path: db_path.clone(),
        instance_id: instance_id.clone(),
        start_time: Instant::now(),
        active_reads: Arc::new(AtomicU32::new(0)),
        active_writes: Arc::new(AtomicU32::new(0)),
        idle_notify: idle_notify.clone(),
        shutdown_tx: shutdown_tx.clone(),
        watcher_stop: std::sync::Mutex::new(None),
        instance_cfg,
        embed_model: Arc::new(tokio::sync::RwLock::new(None)),
        write_mutex: Arc::new(tokio::sync::Mutex::new(())),
        server_mode: is_server_mode,
        indexing_active: Arc::new(AtomicBool::new(false)),
        indexing_repo: Arc::new(tokio::sync::RwLock::new(String::new())),
        indexing_queue_depth: Arc::new(AtomicU32::new(0)),
        safeguards,
        rate_limiters: rate_limiters.clone(),
        drained: Arc::new(AtomicBool::new(false)),
        admin_token,
    });

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

    // Spawn background embedding model loading when the `embed` feature is on.
    tracing::debug!("embed feature compiled in: {}", cfg!(feature = "embed"));
    #[cfg(feature = "embed")]
    {
        let embed_state = state.embed_model.clone();
        let embedding_cfg = state.instance_cfg.as_ref().map(|c| c.embedding.clone());
        let store_for_dim_check = state.store.clone();
        tokio::spawn(async move {
            let cfg = embedding_cfg.unwrap_or_default();
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
            let config = nestweaver_embed::EmbedConfig {
                model_id: cfg.model_id.clone(),
                cache_dir,
                external_endpoint: cfg.external_endpoint.clone(),
                external_model: cfg.external_model.clone(),
            };
            match tokio::task::spawn_blocking(move || nestweaver_embed::EmbedModel::load(&config))
                .await
            {
                Ok(Ok(model)) => {
                    tracing::info!(dim = model.dimension(), "Embedding model loaded");
                    // Check dimension compatibility with existing embeddings
                    if let Some(stored_dim) = store_for_dim_check.embedding_index_dimension()
                        && stored_dim != model.dimension()
                    {
                        tracing::warn!(
                            model_dim = model.dimension(),
                            stored_dim,
                            "Embedding model dimension ({}) does not match stored embeddings ({}). \
                             Semantic search will be disabled. Re-run `nestweaver embed --force` to re-embed.",
                            model.dimension(),
                            stored_dim
                        );
                        return;
                    }
                    *embed_state.write().await = Some(std::sync::Arc::new(model)
                        as std::sync::Arc<dyn nestweaver_engine::EmbedQueryFn>);
                }
                Ok(Err(e)) => {
                    tracing::warn!("Failed to load embedding model: {e}");
                }
                Err(e) => {
                    tracing::warn!("Embedding model load task panicked: {e}");
                }
            }
        });
    }

    let svc = NestWeaverDaemonServer::new(DaemonService::new(state.clone()))
        .max_decoding_message_size(256 * 1024 * 1024)
        .max_encoding_message_size(256 * 1024 * 1024);

    // Prepare the socket path.
    let sock_dir = lifecycle::runtime_dir(&instance_id);
    std::fs::create_dir_all(&sock_dir)
        .with_context(|| format!("create runtime dir: {}", sock_dir.display()))?;

    let sock_path = lifecycle::socket_path(&instance_id);
    let _ = std::fs::remove_file(&sock_path);

    // Write PID file and optionally acquire flock.
    // In fork mode (daemonize2), the flock is already held by daemonize2.
    // In foreground mode (launchd / daemon run), we acquire it ourselves.
    let pid_path = lifecycle::pidfile_path(&instance_id);
    let _pid_guard: Option<std::fs::File> = if std::env::var("NESTWEAVER_DAEMON_FORK").is_err() {
        let pid_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&pid_path)
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

        Some(pid_file)
    } else {
        None
    };

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
                        if active.active_reads.load(Ordering::Relaxed) + active.active_writes.load(Ordering::Relaxed) == 0 {
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
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("register SIGTERM handler");
            sig.recv().await;
            tracing::info!("received SIGTERM — shutting down");
            let _ = tx.send(true);
        });
    }

    let uds = tokio::net::UnixListener::bind(&sock_path)
        .with_context(|| format!("bind UDS: {}", sock_path.display()))?;
    let uds_stream = tokio_stream::wrappers::UnixListenerStream::new(uds);

    // Create scheduler command channel. The sender goes into AdminState
    // so the reload endpoint can push commands; the receiver is consumed
    // by the scheduler task below.
    let (scheduler_tx, scheduler_rx) = tokio::sync::mpsc::channel::<nestweaver_engine::scheduler::SchedulerCommand>(64);

    // MCP-over-HTTP server — spawned alongside the gRPC servers.
    // Binds to grpc_port + 1 when server mode is active, or a separate OS-assigned
    // port when grpc_port is 0.
    if let Some(ref opts) = server_opts {
        let mcp_state = std::sync::Arc::new(if let Some(ref token) = opts.auth_token {
            nestweaver_mcp::http::McpHttpState::with_auth(
                false,
                state.store.clone(),
                state.tantivy.clone(),
                state.db_path.clone(),
                state.instance_cfg.clone(),
                token.clone(),
            )
        } else {
            nestweaver_mcp::http::McpHttpState::new(
                false,
                state.store.clone(),
                state.tantivy.clone(),
                state.db_path.clone(),
                state.instance_cfg.clone(),
            )
        });
        nestweaver_mcp::http::spawn_session_sweeper(mcp_state.sessions.clone());
        let mut mcp_router = nestweaver_mcp::http::router(mcp_state);

        // Mount webhook endpoint when a secret is configured.
        if let Some(ref secret) = opts.webhook_secret {
            let jobs_db_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
            let job_queue = nestweaver_engine::jobs::JobQueue::open(&jobs_db_path)
                .expect("open webhook job queue");
            let webhook_state = std::sync::Arc::new(crate::webhook::WebhookState {
                config: crate::webhook::WebhookConfig {
                    secret: secret.clone(),
                    secret_old: opts.webhook_secret_old.clone(),
                },
                job_queue: std::sync::Arc::new(std::sync::Mutex::new(job_queue)),
            });
            mcp_router = mcp_router.route(
                "/webhook",
                axum::routing::post(crate::webhook::handle_webhook).with_state(webhook_state),
            );
            tracing::info!("webhook endpoint enabled at /webhook");
        }

        // Mount admin API routes when an admin token is configured.
        if let Some(ref admin_tok) = opts.admin_token {
            let admin_state = std::sync::Arc::new(nestweaver_web::state::AdminState {
                admin_token: admin_tok.clone(),
                daemon_store: state.store.clone(),
                instance_id: state.instance_id.clone(),
                start_time: state.start_time,
                active_reads: state.active_reads.clone(),
                active_writes: state.active_writes.clone(),
                drained: state.drained.clone(),
                indexing_queue_depth: state.indexing_queue_depth.clone(),
                db_path: db_path.clone(),
                config_path: config_path.map(|p| p.to_path_buf()),
                scheduler_tx: Some(scheduler_tx.clone()),
            });
            let admin_router = nestweaver_web::create_admin_router(admin_state);
            mcp_router = mcp_router.nest("/admin/api", admin_router);
            tracing::info!("admin API enabled at /admin/api/*");
        }

        // Parse the bind address to determine the MCP port.  When the gRPC
        // bind uses port 0 (OS-assigned), the MCP server also binds to port 0
        // and records the actual port in the port file (second line).
        let mcp_bind_addr: std::net::SocketAddr = opts
            .bind_addr
            .parse()
            .unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
        let mcp_bind = if mcp_bind_addr.port() == 0 {
            std::net::SocketAddr::from((mcp_bind_addr.ip(), 0))
        } else {
            std::net::SocketAddr::from((mcp_bind_addr.ip(), mcp_bind_addr.port() + 1))
        };

        let mcp_listener = tokio::net::TcpListener::bind(mcp_bind)
            .await
            .with_context(|| format!("bind MCP HTTP: {mcp_bind}"))?;
        let mcp_actual_addr = mcp_listener.local_addr()?;
        tracing::info!(%mcp_actual_addr, "MCP HTTP server listening");
        eprintln!("[daemon] MCP HTTP server listening on {}", mcp_actual_addr);

        // Store the MCP port — written alongside the gRPC port below.
        let mcp_port_for_file = mcp_actual_addr.port();

        let mut mcp_shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            axum::serve(mcp_listener, mcp_router)
                .with_graceful_shutdown(async move {
                    let _ = mcp_shutdown_rx.changed().await;
                })
                .await
                .ok();
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

            // When an auth token is configured, wrap the TCP service with a
            // bearer-token interceptor + rate limiting. UDS stays unauthenticated.
            let interceptor = crate::auth::bearer_auth_interceptor(
                opts.auth_token.clone(),
                opts.admin_token.clone(),
                rate_limiters.clone(),
            );
            let tcp_svc =
                tonic::service::interceptor::InterceptedService::new(svc.clone(), interceptor);

            // Build the TLS config when both cert and key are provided.
            let tls_config = match (&opts.tls_cert, &opts.tls_key) {
                (Some(cert_path), Some(key_path)) => {
                    // Install the ring crypto provider for rustls. This is
                    // required by rustls 0.23+ and must happen before any TLS
                    // config is created.
                    let _ = rustls::crypto::ring::default_provider().install_default();

                    let cert_pem = std::fs::read(cert_path)
                        .with_context(|| format!("read TLS cert: {}", cert_path.display()))?;
                    let key_pem = std::fs::read(key_path)
                        .with_context(|| format!("read TLS key: {}", key_path.display()))?;
                    let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);
                    let tls = tonic::transport::ServerTlsConfig::new().identity(identity);
                    tracing::info!("TLS enabled for TCP server");
                    eprintln!("[daemon] TLS enabled for TCP server");
                    Some(tls)
                }
                (Some(_), None) | (None, Some(_)) => {
                    anyhow::bail!("--tls-cert and --tls-key must both be provided for TLS");
                }
                (None, None) => None,
            };

            tokio::spawn(async move {
                let mut builder = tonic::transport::Server::builder();
                if let Some(tls) = tls_config {
                    builder = builder.tls_config(tls).expect("invalid TLS configuration");
                }
                let _ = builder
                    .add_service(tcp_svc)
                    .serve_with_incoming_shutdown(tcp_stream, async move {
                        let _ = tcp_shutdown_rx.changed().await;
                    })
                    .await;
            });
        }

        // Spawn the worker pool to consume index jobs from the SQLite queue.
        {
            let worker_store = Arc::clone(&state.store);
            let worker_db = db_path.clone();
            let worker_instance = instance_id.clone();
            let worker_shutdown = shutdown_tx.subscribe();
            let worker_drained = Arc::clone(&state.drained);
            let worker_count = state.instance_cfg.as_ref()
                .map(|c| c.server.indexing.workers)
                .unwrap_or(2);
            let indexing_status = nestweaver_engine::worker::IndexingStatus::from_arcs(
                Arc::clone(&state.indexing_active),
                state.indexing_repo.clone(),
                Arc::clone(&state.indexing_queue_depth),
            );
            tokio::spawn(async move {
                let jobs_path = nestweaver_engine::sidecar_path(&worker_db, ".jobs.sqlite");
                let workspace_dir = worker_db
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("workspace");
                let job_queue = match nestweaver_engine::jobs::JobQueue::open(&jobs_path) {
                    Ok(q) => q,
                    Err(e) => {
                        tracing::error!("failed to open job queue for worker pool: {e}");
                        return;
                    }
                };
                // Recover any stale running jobs from a previous crash.
                if let Ok(recovered) = job_queue.recover_stale(1800) {
                    if recovered > 0 {
                        tracing::info!(recovered, "recovered stale running jobs");
                    }
                }
                let workspace =
                    match nestweaver_engine::bare_clone::BareCloneWorkspace::new(&workspace_dir) {
                        Ok(w) => w,
                        Err(e) => {
                            tracing::error!("failed to create bare clone workspace: {e}");
                            return;
                        }
                    };
                let pool = nestweaver_engine::worker::WorkerPool::new(worker_count);
                pool.run_with_drain(
                    std::sync::Arc::new(std::sync::Mutex::new(job_queue)),
                    std::sync::Arc::new(workspace),
                    worker_store,
                    worker_instance,
                    worker_shutdown,
                    Some(indexing_status),
                    worker_drained,
                )
                .await;
            });
        }
    } // end if server_opts

    // Spawn adaptive poll scheduler in server mode.
    if server_opts.is_some() {
        let poll_store = Arc::clone(&state.store);
        let poll_db = db_path.clone();
        let poll_instance = instance_id.clone();
        let poll_cfg = state.instance_cfg.clone();
        let poll_drained = Arc::clone(&state.drained);
        let mut poll_shutdown = shutdown_tx.subscribe();
        let mut scheduler_rx = scheduler_rx;  // move into the spawned task
        tokio::spawn(async move {
            use nestweaver_engine::scheduler::PollScheduler;
            use std::time::Duration;
            let indexing_cfg = poll_cfg.as_ref().map(|c| &c.server.indexing);
            let min_poll = indexing_cfg
                .and_then(|c| nestweaver_engine::config::parse_duration(&c.min_poll))
                .unwrap_or(Duration::from_secs(45));
            let max_poll = indexing_cfg
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
                    let poll_override = repo_cfg.poll.as_deref().and_then(|p| {
                        match p {
                            "never" => Some(nestweaver_engine::scheduler::PollOverride::Never),
                            "manual" => Some(nestweaver_engine::scheduler::PollOverride::Manual),
                            other => nestweaver_engine::config::parse_duration(other)
                                .map(nestweaver_engine::scheduler::PollOverride::Fixed),
                        }
                    });
                    scheduler.add_repo(repo_name, repo_cfg.url.clone(), poll_override);
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
                    scheduler.add_repo(repo_name, repo.url.clone(), None);
                }
            }

            loop {
                tokio::select! {
                    _ = poll_shutdown.changed() => break,
                    cmd = scheduler_rx.recv() => {
                        if let Some(cmd) = cmd {
                            match cmd {
                                nestweaver_engine::scheduler::SchedulerCommand::AddRepo { repo_id, repo_url, poll_override } => {
                                    scheduler.add_repo(repo_id, repo_url, poll_override);
                                }
                                nestweaver_engine::scheduler::SchedulerCommand::RemoveRepo { repo_id } => {
                                    scheduler.remove_repo(&repo_id);
                                }
                                nestweaver_engine::scheduler::SchedulerCommand::ReloadConfig { repos } => {
                                    scheduler = PollScheduler::new(min_poll, max_poll);
                                    for (id, url, ovr) in repos {
                                        scheduler.add_repo(id, url, ovr);
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
                        for (repo_id, repo_url) in due {
                            // Determine which branch ref to check. Default to
                            // HEAD (symref of the remote's default branch) for
                            // the ls-remote so we aren't hardcoded to "main".
                            let url = repo_url.clone();
                            let ref_spec = "HEAD".to_string();
                            if let Ok(output) = std::process::Command::new("git")
                                .args(["ls-remote", &url, &ref_spec])
                                .output()
                            {
                                let remote_sha = String::from_utf8_lossy(&output.stdout)
                                    .split_whitespace().next().unwrap_or("").to_string();
                                let r_uid = nestweaver_schema::repo_uid(&poll_instance, &url);
                                let indexed_sha = poll_store.lookup_repo(&r_uid)
                                    .ok().flatten().map(|r| r.indexed_sha).unwrap_or_default();
                                if !remote_sha.is_empty() && remote_sha != indexed_sha {
                                    let jobs_path = nestweaver_engine::sidecar_path(&poll_db, ".jobs.sqlite");
                                    if let Ok(queue) = nestweaver_engine::jobs::JobQueue::open(&jobs_path) {
                                        let canonical_id = nestweaver_engine::jobs::canonical_repo_id(&url);
                                        let _ = queue.upsert(&canonical_id, &url, nestweaver_engine::jobs::JobTrigger::Poll);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Set process title for easier identification via pgrep.
    set_process_title(&format!("nestweaver-daemon-{instance_id}"));

    tonic::transport::Server::builder()
        .add_service(svc)
        .serve_with_incoming_shutdown(uds_stream, async move {
            let _ = shutdown_rx.changed().await;
        })
        .await
        .context("gRPC server error")?;

    // Cleanup — runs on graceful shutdown (not skipped like process::exit would).
    tracing::info!("daemon shutting down, cleaning up");
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pid_path);

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

    let max_depth = req.remaining_depth.max(0) as usize;
    let trace_id = req.trace_id.clone();

    // Build the visited set from the request.
    let mut visited: HashSet<String> = req.visited_canonical_ids.into_iter().collect();

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

    let mut spans: Vec<TraceSpanProto> = Vec::new();
    let mut boundaries: Vec<BoundarySymbolProto> = Vec::new();
    let mut truncated = false;

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

    fn walk_trace(
        ctx: &mut TraceCtx<'_>,
        uid: &str,
        canonical_id: &str,
        name: &str,
        file_path: &str,
        start_line: u32,
        parent_span_id: Option<&str>,
        depth: usize,
        max_depth: usize,
    ) -> String {
        let span_id = (ctx.make_span_id)();

        // Mark this canonical_id as visited.
        ctx.visited.insert(canonical_id.to_string());

        let mut callee_span_ids = Vec::new();

        if depth < max_depth {
            let callees = ctx.store.callees_of(uid).unwrap_or_default();
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

                // Check if this callee is in the same repo (we have it locally).
                // If callees_of returned it, it's in our database.
                let child_span_id = walk_trace(
                    ctx,
                    &callee.uid,
                    callee_cid,
                    &callee.name,
                    &callee.file_path,
                    callee.start_line,
                    Some(&span_id),
                    depth + 1,
                    max_depth,
                );
                callee_span_ids.push(child_span_id);
            }
        } else if depth >= max_depth {
            // Check if there are callees we didn't follow due to depth limit.
            let callees = ctx.store.callees_of(uid).unwrap_or_default();
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
            repo_url: ctx.repo_url.clone(),
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
        .filter_map(|proto| proto_to_atomic_change(proto))
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
