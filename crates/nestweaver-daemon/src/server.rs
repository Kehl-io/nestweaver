//! gRPC server implementation for the NestWeaver daemon.
//!
//! Binds to a Unix domain socket and dispatches read RPCs through the
//! existing MCP tool dispatch layer, avoiding any duplication of
//! business logic.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use nestweaver_proto::nest_weaver_daemon_server::{NestWeaverDaemon, NestWeaverDaemonServer};
use nestweaver_proto::*;
use nestweaver_store::{GraphStore, TantivyIndex};
use tokio::sync::Notify;
use tonic::{Request, Response, Status};

use crate::lifecycle;

// ── State ───────────────────────────────────────────────────────────

/// Shared state held by the daemon process.
pub struct DaemonState {
    pub store: GraphStore,
    pub tantivy: Option<TantivyIndex>,
    pub db_path: PathBuf,
    pub instance_id: String,
    pub start_time: Instant,
    pub active_connections: AtomicU32,
    pub idle_notify: Arc<Notify>,
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
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
    async fn dispatch_json_tool(
        &self,
        tool_name: &str,
        args_json: &str,
    ) -> Result<Response<JsonResponse>, Status> {
        self.state.idle_notify.notify_one();
        self.state
            .active_connections
            .fetch_add(1, Ordering::Relaxed);

        let state = self.state.clone();
        let tool_name = tool_name.to_string();
        let args_json = args_json.to_string();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || -> Result<String, Status> {
            let args: serde_json::Value = serde_json::from_str(&args_json)
                .map_err(|e| Status::invalid_argument(format!("invalid JSON in args_json: {e}")))?;

            nestweaver_mcp::tools::set_current_db_path(state.db_path.clone());
            nestweaver_mcp::tools::set_lite_mode(false);

            let value = nestweaver_mcp::tools::dispatch(
                &state.store,
                state.tantivy.as_ref(),
                &tool_name,
                args,
            )
            .map_err(|e| Status::internal(format!("tool {tool_name} failed: {e}")))?;

            serde_json::to_string(&value)
                .map_err(|e| Status::internal(format!("failed to serialize result: {e}")))
        })
        .await
        .map_err(|e| Status::internal(format!("dispatch task panicked: {e}")))?;

        self.state
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);

        result.map(|json| Response::new(JsonResponse { result_json: json }))
    }

    /// Dispatch a tool by name with a pre-built JSON args value, returning the
    /// raw `serde_json::Value` result. Used by typed RPC handlers that convert
    /// protobuf → JSON on input and JSON → protobuf on output.
    async fn dispatch_tool_json(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, Status> {
        self.state.idle_notify.notify_one();
        self.state
            .active_connections
            .fetch_add(1, Ordering::Relaxed);

        let state = self.state.clone();
        let tool_name = tool_name.to_string();

        #[allow(clippy::result_large_err)]
        let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, Status> {
            nestweaver_mcp::tools::set_current_db_path(state.db_path.clone());
            nestweaver_mcp::tools::set_lite_mode(false);

            nestweaver_mcp::tools::dispatch(&state.store, state.tantivy.as_ref(), &tool_name, args)
                .map_err(|e| Status::internal(format!("tool {tool_name} failed: {e}")))
        })
        .await
        .map_err(|e| Status::internal(format!("dispatch task panicked: {e}")))?;

        self.state
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);

        result
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
        let active = self.state.active_connections.load(Ordering::Relaxed);
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
        tracing::info!("shutdown requested via gRPC");
        let tx = self.state.shutdown_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = tx.send(true);
        });
        Ok(Response::new(ShutdownResponse { ok: true }))
    }

    // ── Indexing ─────────────────────────────────────────────────────

    type IndexRepoStream = ProgressStream;

    async fn index_repo(
        &self,
        request: Request<IndexRepoRequest>,
    ) -> Result<Response<Self::IndexRepoStream>, Status> {
        self.state.idle_notify.notify_one();

        let req = request.into_inner();
        let repo_path = PathBuf::from(&req.repo_path);
        let state = self.state.clone();
        let force = req.force;
        let name = if req.name.is_empty() {
            None
        } else {
            Some(req.name.clone())
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<IndexProgress, Status>>(16);

        tokio::task::spawn_blocking(move || {
            let repo_url = format!("file://{}", repo_path.display());

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
                "local",
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
        self.state.idle_notify.notify_one();

        let req = request.into_inner();
        let vault_path = PathBuf::from(&req.vault_path);
        let vault_name = req.vault_name.clone();
        let extra_patterns = req.extra_ignore_patterns.clone();
        let state = self.state.clone();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<IndexProgress, Status>>(16);

        tokio::task::spawn_blocking(move || {
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
                &state.instance_id,
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

    type RefreshBrainStream = ProgressStream;

    async fn refresh_brain(
        &self,
        _request: Request<RefreshBrainRequest>,
    ) -> Result<Response<Self::RefreshBrainStream>, Status> {
        Err(Status::unimplemented("RefreshBrain is not yet implemented"))
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
        if !req.response_format.is_empty() {
            args["response_format"] = serde_json::json!(req.response_format);
        }
        if !req.root.is_empty() {
            args["root"] = serde_json::json!(req.root);
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
                            .unwrap_or("")
                            .to_string(),
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
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Response::new(BrainSearchResponse {
            query: query_echo,
            engine,
            total_matches,
            results,
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
        if !req.uid.is_empty() {
            args["uid"] = serde_json::json!(req.uid);
        }
        if !req.title.is_empty() {
            args["title"] = serde_json::json!(req.title);
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
                .unwrap_or("")
                .to_string(),
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
}

// ── Server entry point ──────────────────────────────────────────────

/// Run the daemon gRPC server, binding to a Unix domain socket.
///
/// `idle_timeout` controls how long the daemon stays alive with no
/// active requests before self-terminating. Pass `None` to disable.
pub async fn run_server(
    db_path: &Path,
    idle_timeout: Option<Duration>,
) -> Result<(), anyhow::Error> {
    let db_path = std::fs::canonicalize(db_path)
        .with_context(|| format!("canonicalize db path: {}", db_path.display()))?;

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
            // Check if a watcher is holding the lock (writes PID to <db>.lock).
            let lock_path = {
                let mut s = db_path.as_os_str().to_owned();
                s.push(".lock");
                std::path::PathBuf::from(s)
            };
            let mut hint = String::new();
            if let Ok(pid_str) = std::fs::read_to_string(&lock_path)
                && let Ok(pid) = pid_str.trim().parse::<i32>()
                && unsafe { libc::kill(pid, 0) } == 0
            {
                hint = format!(
                    "\n\nA brain watcher (PID {pid}) is holding the database lock.\n\
                     Stop it with: nestweaver brain watch-stop --db {}\n\
                     Or use --no-daemon to bypass the daemon.",
                    db_path.display()
                );
            }
            if hint.is_empty() {
                hint = format!(
                    "\n\nAnother process may hold the write lock on {}.\n\
                     Check for running nestweaver processes and stop them, or use --no-daemon.",
                    db_path.display()
                );
            }
            return Err(e)
                .with_context(|| format!("failed to open database with write access{hint}"));
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
            Some(idx)
        }
        Err(_) => match TantivyIndex::open_reader_only(&tantivy_path) {
            Ok(idx) => {
                tracing::info!(
                    docs = idx.doc_count(),
                    path = %tantivy_path.display(),
                    "Tantivy index open (reader-only fallback)"
                );
                Some(idx)
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

    let state = Arc::new(DaemonState {
        store,
        tantivy,
        db_path: db_path.clone(),
        instance_id: instance_id.clone(),
        start_time: Instant::now(),
        active_connections: AtomicU32::new(0),
        idle_notify: idle_notify.clone(),
        shutdown_tx: shutdown_tx.clone(),
    });

    let svc = NestWeaverDaemonServer::new(DaemonService::new(state.clone()))
        .max_decoding_message_size(64 * 1024 * 1024)
        .max_encoding_message_size(64 * 1024 * 1024);

    // Prepare the socket path.
    let sock_dir = lifecycle::runtime_dir(&instance_id);
    std::fs::create_dir_all(&sock_dir)
        .with_context(|| format!("create runtime dir: {}", sock_dir.display()))?;

    let sock_path = lifecycle::socket_path(&instance_id);
    let _ = std::fs::remove_file(&sock_path);

    // PID file is written by the `daemonize2` crate during the double-fork.
    // We only need the path for cleanup on shutdown.
    let pid_path = lifecycle::pidfile_path(&instance_id);

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
                        if active.active_connections.load(Ordering::Relaxed) == 0 {
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

    tonic::transport::Server::builder()
        .add_service(svc)
        .serve_with_incoming_shutdown(uds_stream, async move {
            let _ = shutdown_rx.changed().await;
        })
        .await
        .context("gRPC server error")?;

    // Cleanup — runs on graceful shutdown (not skipped like process::exit would).
    eprintln!("[daemon] shutting down, cleaning up");
    tracing::info!("daemon shutting down, cleaning up");
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pid_path);

    Ok(())
}
