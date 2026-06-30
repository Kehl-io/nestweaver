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
    /// Shared admin state, set once after construction. Used by `serve_ui`
    /// to mount the admin API on the web UI server as well.
    pub admin_state: std::sync::OnceLock<Arc<nestweaver_web::state::AdminState>>,
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
        let started = std::time::Instant::now();
        // Increment gRPC request counter for this tool/method.
        nestweaver_web::routes::metrics::GRPC_REQUESTS
            .with_label_values(&[tool_name])
            .inc();

        let safeguards = &self.state.safeguards;
        let tool = tool_name.to_string();
        let timeout = safeguards.effective_timeout(&tool, None);
        let handler = self.dispatch_json_tool_inner(tool_name, args_json);

        let response = if self.state.server_mode {
            with_safeguard(&tool, safeguards, None, handler).await
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
            let mut value = nestweaver_mcp::tools::dispatch(
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
    ) -> Result<serde_json::Value, Status> {
        let started = std::time::Instant::now();
        // Increment gRPC request counter for this tool/method.
        nestweaver_web::routes::metrics::GRPC_REQUESTS
            .with_label_values(&[tool_name])
            .inc();

        let safeguards = &self.state.safeguards;
        let tool = tool_name.to_string();
        let timeout = safeguards.effective_timeout(&tool, None);
        let handler = self.dispatch_tool_json_inner(tool_name, args);

        let response = if self.state.server_mode {
            with_safeguard(&tool, safeguards, None, handler).await
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
        request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
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

    // ── Backup ──────────────────────────────────────────────────────

    async fn prepare_backup(
        &self,
        _request: Request<PrepareBackupRequest>,
    ) -> Result<Response<PrepareBackupResponse>, Status> {
        if let Some(crate::auth::IsAdmin(false)) | None =
            _request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
        tracing::info!("prepare_backup: acquiring write mutex");
        let _write_lock = self.state.write_mutex.lock().await;
        let _guard = ConnectionGuard::write(&self.state);

        let store = self.state.store.clone();
        tokio::task::spawn_blocking(move || {
            // Flush in-memory embedding index to disk.
            if let Err(e) = store.flush_embedding_index() {
                tracing::warn!("prepare_backup: flush_embedding_index failed: {e}");
            }
            // Run CHECKPOINT to merge the WAL into the main database file.
            if let Err(e) = store.checkpoint() {
                tracing::warn!("prepare_backup: CHECKPOINT failed: {e}");
            }
        })
        .await
        .map_err(|e| Status::internal(format!("prepare_backup task panicked: {e}")))?;

        tracing::info!("prepare_backup: database quiesced for backup");
        Ok(Response::new(PrepareBackupResponse {
            ok: true,
            message: "database quiesced — safe to copy files".to_string(),
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

        let vault_path = vault_path.canonicalize().map_err(|e| {
            Status::invalid_argument(format!("cannot canonicalize vault path: {e}"))
        })?;

        // Only allow paths registered in the instance config.
        if let Some(ref cfg) = self.state.instance_cfg {
            let allowed: Vec<PathBuf> = cfg
                .repos
                .iter()
                .filter(|r| r.repo_type == Some(nestweaver_engine::config::RepoType::Vault))
                .filter_map(|r| {
                    let p = PathBuf::from(&r.url);
                    p.canonicalize().ok()
                })
                .collect();
            if !allowed.iter().any(|a| vault_path.starts_with(a)) {
                return Err(Status::invalid_argument(format!(
                    "vault path {} is not in the instance's registered sources",
                    vault_path.display()
                )));
            }
        } else {
            return Err(Status::failed_precondition(
                "watch_vault requires an instance config (--config); \
                 path validation cannot be performed without one",
            ));
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
        if self.state.server_mode {
            return Err(Status::unimplemented(
                "watchers are server-managed in server mode",
            ));
        }

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

        let repo_path = repo_path
            .canonicalize()
            .map_err(|e| Status::invalid_argument(format!("cannot canonicalize repo path: {e}")))?;

        // Only allow paths registered in the instance config.
        if let Some(ref cfg) = self.state.instance_cfg {
            let allowed: Vec<PathBuf> = cfg
                .repos
                .iter()
                .filter_map(|r| {
                    let p = PathBuf::from(&r.url);
                    p.canonicalize().ok()
                })
                .collect();
            if !allowed.iter().any(|a| repo_path.starts_with(a)) {
                return Err(Status::invalid_argument(format!(
                    "repo path {} is not in the instance's registered sources",
                    repo_path.display()
                )));
            }
        } else {
            return Err(Status::failed_precondition(
                "watch_code requires an instance config (--config); \
                 path validation cannot be performed without one",
            ));
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

        let app_state = nestweaver_web::state::AppState::new_with_arc_tantivy(
            state.store.clone(),
            state.tantivy.clone(),
            state.db_path.clone(),
        );

        let port = if req.port > 0 { req.port as u16 } else { 3000 };
        let open_browser = req.open_browser;

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

        // Spawn web server as a background task inside the daemon.
        tokio::spawn(async move {
            if let Err(e) =
                nestweaver_web::start_server_with_router(web_router, port, open_browser).await
            {
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
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
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
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
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
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
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
        if let Some(crate::auth::IsAdmin(false)) | None =
            request.extensions().get::<crate::auth::IsAdmin>()
        {
            return Err(Status::permission_denied("admin token required"));
        }
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
    Ok(())
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

pub async fn run_server(
    db_path: &Path,
    idle_timeout: Option<Duration>,
    config_path: Option<&Path>,
    server_opts: Option<ServerOpts>,
) -> Result<(), anyhow::Error> {
    // Canonicalize if possible, but don't fail if the DB doesn't exist yet.
    // The DB will be created by GraphStore::open_or_create below.
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let db_path = lifecycle::canonical_db_path(db_path);

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

    // Reject any present auth/admin token that is too short to be safe.
    validate_token_lengths(
        &server_opts
            .as_ref()
            .and_then(|opts| opts.auth_token.clone()),
        &admin_token,
    )?;

    // C1: Reject non-loopback bind without an auth token — the server would
    // be fully open to the network because the auth interceptor passes all
    // requests when no token is configured.
    if let Some(ref opts) = server_opts
        && opts.auth_token.is_none()
    {
        match opts.bind_addr.parse::<std::net::SocketAddr>() {
            Ok(addr) if addr.ip().is_loopback() => { /* safe — loopback */ }
            Ok(addr) => {
                anyhow::bail!(
                    "Cannot bind to non-loopback address {} without --auth-token; \
                     the server would be fully open to the network",
                    addr
                );
            }
            Err(_) => {
                anyhow::bail!(
                    "Cannot determine if bind address '{}' is loopback. \
                     Use --auth-token or specify an IP address.",
                    opts.bind_addr
                );
            }
        }
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
        admin_state: std::sync::OnceLock::new(),
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
    let (scheduler_tx, scheduler_rx) =
        tokio::sync::mpsc::channel::<nestweaver_engine::scheduler::SchedulerCommand>(64);

    // Shared job queue Arc — created once and shared across webhook handler,
    // worker pool, and poll scheduler to avoid concurrent SQLite opens.
    let mut shared_job_queue_opt: Option<
        std::sync::Arc<std::sync::Mutex<nestweaver_engine::jobs::JobQueue>>,
    > = None;

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
            // Share the daemon's embed model so HTTP dispatch has parity with gRPC.
            s.embed_model = state.embed_model.clone();
            std::sync::Arc::new(s)
        };
        nestweaver_mcp::http::spawn_session_sweeper(
            mcp_state.sessions.clone(),
            shutdown_tx.subscribe(),
        );
        nestweaver_mcp::http::spawn_bucket_sweeper(
            mcp_state.client_rate_limiter.clone(),
            shutdown_tx.subscribe(),
        );
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

        if let Some(ref cfg) = state.instance_cfg
            && let Ok(queue) = shared_job_queue.lock()
        {
            for repo_cfg in &cfg.repos {
                let repo_uid = nestweaver_schema::repo_uid(&instance_id, &repo_cfg.url);
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

        // Mount webhook endpoint when a secret is configured.
        if let Some(ref secret) = opts.webhook_secret {
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

        // Mount admin API routes when an admin token is configured.
        if let Some(ref admin_tok) = opts.admin_token {
            let admin_state = std::sync::Arc::new(nestweaver_web::state::AdminState {
                admin_token: admin_tok.clone(),
                auth_token: opts.auth_token.clone(),
                device_flow: std::sync::Arc::new(tokio::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
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
                webhook_allowed_repos: webhook_allowed_repos.clone(),
                webhook_repo_branches: webhook_repo_branches.clone(),
                write_mutex: Some(Arc::clone(&state.write_mutex)),
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

        // Mount /metrics at the top level of the MCP HTTP router so
        // Prometheus scrapers can use the standard path without knowing
        // the /admin/api prefix. Works even without an admin token.
        mcp_router = mcp_router.route(
            "/metrics",
            axum::routing::get(nestweaver_web::routes::metrics::metrics_handler),
        );

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

        // Validate TLS config BEFORE binding any ports so we don't
        // advertise addresses that will never serve traffic.
        let tls_config = match (&opts.tls_cert, &opts.tls_key) {
            (Some(cert_path), Some(key_path)) => {
                let _ = rustls::crypto::ring::default_provider().install_default();

                let cert_pem = std::fs::read(cert_path)
                    .with_context(|| format!("read TLS cert: {}", cert_path.display()))?;
                let key_pem = std::fs::read(key_path)
                    .with_context(|| format!("read TLS key: {}", key_path.display()))?;

                let identity =
                    tonic::transport::Identity::from_pem(cert_pem.clone(), key_pem.clone());
                let tonic_tls = tonic::transport::ServerTlsConfig::new().identity(identity);

                let certs = rustls_pemfile::certs(&mut &cert_pem[..])
                    .collect::<Result<Vec<_>, _>>()
                    .context("parse TLS certificate PEM")?;
                let key = rustls_pemfile::private_key(&mut &key_pem[..])
                    .context("parse TLS private key PEM")?
                    .context("no private key found in PEM")?;
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
                Some((tonic_tls, tls_acceptor))
            }
            (Some(_), None) | (None, Some(_)) => {
                anyhow::bail!("--tls-cert and --tls-key must both be provided for TLS");
            }
            (None, None) => None,
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
        tokio::spawn(async move {
            if let Some(acceptor) = mcp_tls_acceptor {
                let incoming = async_stream::stream! {
                    loop {
                        match mcp_listener.accept().await {
                            Ok((stream, _addr)) => {
                                match acceptor.accept(stream).await {
                                    Ok(tls_stream) => {
                                        let result: Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, std::io::Error> = Ok(tls_stream);
                                        yield result;
                                    }
                                    Err(e) => tracing::debug!("MCP TLS handshake failed: {e}"),
                                }
                            }
                            Err(e) => tracing::debug!("MCP TCP accept failed: {e}"),
                        }
                    }
                };
                use futures::StreamExt;
                tokio::pin!(incoming);
                let mut shutdown = Box::pin(mcp_shutdown_rx.changed());
                loop {
                    tokio::select! {
                        Some(Ok(stream)) = incoming.next() => {
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
                if let Some((tonic_tls, _)) = tls_config {
                    builder = match builder.tls_config(tonic_tls) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::error!(error = %e, "TLS configuration failed at serve time");
                            return;
                        }
                    };
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
            tokio::spawn(async move {
                let workspace_dir = worker_db
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("workspace");
                // Recover any stale running jobs from a previous crash.
                if let Ok(guard) = worker_job_queue.lock()
                    && let Ok(recovered) = guard.recover_stale(1800)
                    && recovered > 0
                {
                    tracing::info!(recovered, "recovered stale running jobs");
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
                    worker_shutdown,
                    Some(indexing_status),
                    worker_drained,
                    Some(worker_write_mutex),
                )
                .await;
            });
        }
    } // end if server_opts

    // Spawn adaptive poll scheduler in server mode.
    if server_opts.is_some() {
        let poll_store = Arc::clone(&state.store);
        let poll_instance = instance_id.clone();
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
                            let mut ls_cmd = std::process::Command::new("git");
                            ls_cmd.args(&ls_guard.config_args);
                            ls_cmd.args(["ls-remote", &url, &ref_spec]);
                            if let Ok(output) = ls_cmd.output() {
                                let remote_sha = String::from_utf8_lossy(&output.stdout)
                                    .split_whitespace().next().unwrap_or("").to_string();
                                let r_uid = nestweaver_schema::repo_uid(&poll_instance, &url);
                                let indexed_sha = poll_store.lookup_repo(&r_uid)
                                    .ok().flatten().map(|r| r.indexed_sha).unwrap_or_default();
                                if !remote_sha.is_empty()
                                    && remote_sha != indexed_sha
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
        let metrics_instance = instance_id.clone();
        let mut metrics_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            use nestweaver_web::routes::metrics;
            let mut last_metric_job_id = 0_i64;
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

                if let Some(queue) = &metrics_job_queue
                    && let Ok(guard) = queue.lock()
                    && let Ok(completed) = guard.completed_job_metrics_after(last_metric_job_id)
                {
                    for job in completed {
                        last_metric_job_id = last_metric_job_id.max(job.id);
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

    tonic::transport::Server::builder()
        .add_service(uds_svc)
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

#[cfg(test)]
mod startup_helper_tests {
    use super::*;
    use nestweaver_engine::{RepoConfig, RepoType};

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
        assert!(validate_token_lengths(&tok, &tok).is_ok());
    }

    #[test]
    fn validate_token_lengths_accepts_none() {
        assert!(validate_token_lengths(&None, &None).is_ok());
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
}
