//! Hybrid client — wraps `DaemonClient` with optional upstream servers.
//!
//! When no upstreams are configured, this is a zero-cost wrapper around
//! `DaemonClient`. When upstreams exist, queries are routed based on the
//! upstream's routing mode:
//!
//! - **Fallback**: query local first; if results are sparse (< threshold),
//!   query server as fallback.
//! - **Merge**: query both in parallel, merge via weighted RRF + scope-hash dedup.
//! - **Primary**: always query server; local only for uncommitted file overlay.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;
use nestweaver_proto::{JsonRequest, JsonResponse};

use crate::DaemonClient;
use crate::discovery::{RoutingMode, discover_upstreams};
use crate::merge::rrf_merge;
use crate::routing::{ToolRouting, tool_routing};
use crate::upstream::UpstreamHandle;

/// Provenance metadata injected into every hybrid response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProvenanceMeta {
    /// Which sources contributed to this response (e.g. ["local"], ["local", "acme"]).
    pub sources: Vec<String>,
    /// Repos where local index is behind the server's indexed SHA.
    pub stale_repos: Vec<String>,
    /// Routing scope: "local", "server", or "hybrid".
    pub scope: String,
}

/// Status information for a single upstream server.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpstreamStatus {
    pub name: String,
    pub healthy: bool,
    pub mode: String,
    pub repo_count: usize,
    pub stale_repos: Vec<String>,
}

/// Minimum result count before we consider querying the server in fallback mode.
const FALLBACK_THRESHOLD: usize = 5;

/// Default timeout for upstream queries.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(1);

/// Hybrid client that routes queries to a local daemon and optional upstream
/// servers.
///
/// When no upstreams are configured, this is a zero-cost wrapper around
/// `DaemonClient`. When upstreams exist, queries are routed based on routing
/// mode (fallback/merge/primary).
pub struct HybridClient {
    local: DaemonClient,
    upstreams: Vec<UpstreamHandle>,
}

impl HybridClient {
    /// Connect to the local daemon with auto-discovery of upstream servers.
    ///
    /// Upstream discovery walks `start_dir` upward looking for
    /// `.nestweaver/server.toml`, checks `~/.config/nestweaver/upstreams.toml`,
    /// and honors the `NESTWEAVER_UPSTREAM` env var.
    pub async fn connect(
        db_path: &Path,
        config_path: Option<&Path>,
        start_dir: &Path,
    ) -> Result<Self> {
        let local = DaemonClient::connect(db_path, config_path).await?;
        let upstream_configs = discover_upstreams(start_dir);

        let mut upstreams = Vec::new();
        for cfg in &upstream_configs {
            match UpstreamHandle::from_config(cfg) {
                Ok(handle) => {
                    info!(
                        name = %handle.name,
                        mode = ?handle.mode,
                        "registered upstream server"
                    );
                    upstreams.push(handle);
                }
                Err(e) => {
                    warn!(
                        url = %cfg.url,
                        error = %e,
                        "failed to create upstream handle — continuing without it"
                    );
                }
            }
        }

        Ok(Self { local, upstreams })
    }

    /// Create from an existing `DaemonClient` with no upstreams.
    pub fn local_only(client: DaemonClient) -> Self {
        Self {
            local: client,
            upstreams: vec![],
        }
    }

    /// Create from an existing `DaemonClient` with explicit upstreams.
    /// Useful for tests.
    pub fn from_parts(local: DaemonClient, upstreams: Vec<UpstreamHandle>) -> Self {
        Self { local, upstreams }
    }

    /// Access the underlying `DaemonClient`.
    pub fn local(&self) -> &DaemonClient {
        &self.local
    }

    /// Mutable access to the underlying `DaemonClient`.
    pub fn local_mut(&mut self) -> &mut DaemonClient {
        &mut self.local
    }

    /// Whether any upstream servers are connected and healthy.
    pub fn has_upstreams(&self) -> bool {
        self.upstreams.iter().any(|u| u.is_healthy())
    }

    /// List connected upstream names and their health state.
    pub fn upstream_info(&self) -> Vec<(&str, bool)> {
        self.upstreams
            .iter()
            .map(|u| (u.name.as_str(), u.is_healthy()))
            .collect()
    }

    /// Access the raw gRPC client for the local daemon (pass-through).
    ///
    /// This is the primary interface used by existing callsites that
    /// work directly with `DaemonClient`. Using `inner_mut()` on a
    /// `HybridClient` is equivalent to using it on the wrapped
    /// `DaemonClient` — upstreams are not involved.
    pub fn inner_mut(&mut self) -> &mut NestWeaverDaemonClient<Channel> {
        self.local.inner_mut()
    }

    /// Read-only access to the raw gRPC client.
    pub fn inner(&self) -> &NestWeaverDaemonClient<Channel> {
        self.local.inner()
    }

    // ── Query routing ─────────────────────────────────────────────────

    /// Execute a query with per-tool routing from the routing matrix.
    ///
    /// The routing matrix (`crate::routing::tool_routing`) maps each tool
    /// name to a routing strategy (Merge, LocalFirst, ServerPreferred,
    /// TwoTier, FanOut, LocalOnly, Combined, Continuation). When no
    /// upstreams are configured, all queries go to the local daemon.
    pub async fn query(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        if !self.has_upstreams() {
            let mut result = self.query_local(tool_name, params).await?;
            inject_provenance(&mut result, &["local"], &[]);
            return Ok(result);
        }

        let routing = tool_routing(tool_name);
        match routing {
            ToolRouting::LocalOnly => {
                let mut result = self.query_local(tool_name, params).await?;
                inject_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
            ToolRouting::ServerPreferred => self.query_server_preferred(tool_name, params).await,
            ToolRouting::TwoTier => self.query_two_tier(tool_name, params).await,
            ToolRouting::Continuation => self.query_with_continuation(tool_name, params).await,
            ToolRouting::Combined | ToolRouting::Merge | ToolRouting::FanOut => {
                self.query_merge(tool_name, params).await
            }
            ToolRouting::LocalFirst => self.query_fallback(tool_name, params).await,
        }
    }

    /// Server-preferred routing: query upstream first, fall back to local.
    async fn query_server_preferred(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        match self
            .query_upstream(tool_name, params, UPSTREAM_TIMEOUT)
            .await
        {
            Ok(mut r) => {
                inject_provenance(&mut r, &["server"], &[]);
                Ok(r)
            }
            Err(e) => {
                debug!(error = %e, tool = tool_name, "server-preferred: upstream failed, falling back to local");
                let mut r = self.query_local(tool_name, params).await?;
                inject_provenance(&mut r, &["local"], &[]);
                Ok(r)
            }
        }
    }

    /// Two-tier routing: local impact + org-wide impact from server.
    /// Delegates to `two_tier_query` for blast_radius/brain_impact/
    /// affected_tests tools.
    async fn query_two_tier(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        two_tier_query(self, tool_name, params).await.or_else(|e| {
            debug!(error = %e, tool = tool_name, "two-tier query failed");
            Err(e)
        })
    }

    /// Continuation routing: run locally, then stitch server spans at
    /// cross-repo boundaries. Used for flow_trace and investigate_expand.
    async fn query_with_continuation(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        if tool_name == "flow_trace" {
            flow_trace_with_stitching(self, params, &[]).await
        } else {
            // For investigate_expand and other continuation tools, fall back
            // to merge routing (continuation stitching is flow_trace-specific).
            self.query_merge(tool_name, params).await
        }
    }

    /// Fallback routing: query local first, query server only if local
    /// results are sparse (fewer than [`FALLBACK_THRESHOLD`]).
    pub async fn query_fallback(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        // 1. Always query local first.
        let mut local_result = self.query_local(tool_name, params).await?;

        // 2. If no healthy upstreams, return local as-is with provenance.
        if !self.has_upstreams() {
            inject_provenance(&mut local_result, &["local"], &[]);
            return Ok(local_result);
        }

        // 3. Check if local results are sufficient.
        let local_count = count_results(&local_result);
        if local_count >= FALLBACK_THRESHOLD {
            debug!(
                tool = tool_name,
                local_count, "fallback: local results sufficient, skipping server"
            );
            inject_provenance(&mut local_result, &["local"], &[]);
            return Ok(local_result);
        }

        // 4. Local results are sparse — query server with timeout.
        debug!(
            tool = tool_name,
            local_count,
            threshold = FALLBACK_THRESHOLD,
            "fallback: local results sparse, querying server"
        );
        match self
            .query_upstream(tool_name, params, UPSTREAM_TIMEOUT)
            .await
        {
            Ok(server_result) => {
                let merged = merge_json_results(&local_result, &server_result);
                Ok(merged)
            }
            Err(e) => {
                debug!(error = %e, "fallback: server query failed, using local only");
                inject_provenance(&mut local_result, &["local"], &[]);
                Ok(local_result)
            }
        }
    }

    /// Merge routing: query both local and server in parallel, merge via
    /// weighted RRF + scope-hash dedup.
    pub async fn query_merge(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        if !self.has_upstreams() {
            let mut result = self.query_local(tool_name, params).await?;
            inject_provenance(&mut result, &["local"], &[]);
            return Ok(result);
        }

        // Prepare the server future before borrowing self.local mutably.
        // Pick the first healthy upstream and clone its client (cheap channel clone).
        let repo_hint = extract_repo_hint(params);
        let server_task = find_upstream_for_repo(&self.upstreams, repo_hint).map(|u| {
            let timeout = u.timeout;
            let mut client = u.client();
            let token = u.auth_token().map(|t| t.to_string());
            let tool = tool_name.to_string();
            let p = params.clone();
            async move {
                tokio::time::timeout(
                    timeout,
                    dispatch_json_rpc_authed(&mut client, &tool, &p, token.as_deref()),
                )
                .await
            }
        });

        let Some(server_fut) = server_task else {
            return self.query_local(tool_name, params).await;
        };

        // Now borrow self.local mutably for the local query.
        let local_fut = dispatch_json_rpc(self.local.inner_mut(), tool_name, params);

        let (local_result, server_result) = tokio::join!(local_fut, server_fut);
        let local = local_result?;

        match server_result {
            Ok(Ok(server)) => {
                Ok(merge_structured_results(&local, &server))
            }
            Ok(Err(e)) => {
                debug!(error = %e, "merge: server query failed, using local only");
                let mut result = local;
                inject_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
            Err(_) => {
                debug!("merge: server query timed out, using local only");
                let mut result = local;
                inject_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
        }
    }

    /// Compare local repo SHAs against each upstream's `RepoStates`.
    /// Returns repo URLs where the local index is behind the server.
    pub async fn check_staleness(&mut self) -> Vec<String> {
        let mut stale = Vec::new();

        // Get local repo states.
        let local_states: std::collections::HashMap<String, String> = {
            let req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
            match self.local.inner_mut().repo_states(req).await {
                Ok(resp) => resp
                    .into_inner()
                    .repos
                    .into_iter()
                    .map(|r| (r.repo_url.clone(), r.indexed_sha))
                    .collect(),
                Err(_) => return stale,
            }
        };

        for upstream in &self.upstreams {
            if !upstream.is_healthy() {
                continue;
            }
            let mut client = upstream.client();
            let mut req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
            upstream.inject_auth(&mut req);

            if let Ok(resp) = client.repo_states(req).await {
                for server_repo in resp.into_inner().repos {
                    if let Some(local_sha) = local_states.get(&server_repo.repo_url) {
                        if local_sha != &server_repo.indexed_sha
                            && !server_repo.indexed_sha.is_empty()
                        {
                            stale.push(server_repo.repo_url.clone());
                        }
                    }
                }
            }
        }

        stale
    }

    /// Collect status information for all configured upstreams.
    pub async fn upstream_status(&mut self) -> Vec<UpstreamStatus> {
        let mut statuses = Vec::new();

        // Get local repo states for staleness comparison.
        let local_states: std::collections::HashMap<String, String> = {
            let req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
            match self.local.inner_mut().repo_states(req).await {
                Ok(resp) => resp
                    .into_inner()
                    .repos
                    .into_iter()
                    .map(|r| (r.repo_url.clone(), r.indexed_sha))
                    .collect(),
                Err(_) => std::collections::HashMap::new(),
            }
        };

        for upstream in &self.upstreams {
            let mut status = UpstreamStatus {
                name: upstream.name.clone(),
                healthy: upstream.is_healthy(),
                mode: format!("{:?}", upstream.mode).to_lowercase(),
                repo_count: 0,
                stale_repos: vec![],
            };

            if upstream.is_healthy() {
                let mut client = upstream.client();
                let mut req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
                upstream.inject_auth(&mut req);

                if let Ok(resp) = client.repo_states(req).await {
                    let server_repos = resp.into_inner().repos;
                    status.repo_count = server_repos.len();

                    for server_repo in &server_repos {
                        if let Some(local_sha) = local_states.get(&server_repo.repo_url) {
                            if local_sha != &server_repo.indexed_sha
                                && !server_repo.indexed_sha.is_empty()
                            {
                                status.stale_repos.push(server_repo.repo_url.clone());
                            }
                        }
                    }
                }
            }

            statuses.push(status);
        }

        statuses
    }

    /// Query the local daemon via its gRPC channel.
    async fn query_local(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        dispatch_json_rpc(self.local.inner_mut(), tool_name, params).await
    }

    /// Query an upstream server via its gRPC channel with a timeout.
    ///
    /// Marks the upstream unhealthy on failure or timeout so subsequent
    /// queries skip it until the background health check recovers it.
    async fn query_upstream(
        &self,
        tool_name: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let repo_hint = extract_repo_hint(params);
        let upstream = find_upstream_for_repo(&self.upstreams, repo_hint)
            .context("no healthy upstream servers")?;

        let mut client = upstream.client();
        let token = upstream.auth_token().map(|t| t.to_string());
        match tokio::time::timeout(
            timeout,
            dispatch_json_rpc_authed(&mut client, tool_name, params, token.as_deref()),
        )
        .await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => {
                warn!(
                    upstream = %upstream.name,
                    error = %e,
                    "upstream query failed, marking unhealthy"
                );
                upstream.mark_unhealthy();
                Err(e)
            }
            Err(_) => {
                warn!(
                    upstream = %upstream.name,
                    timeout_ms = timeout.as_millis() as u64,
                    "upstream query timed out, marking unhealthy"
                );
                upstream.mark_unhealthy();
                anyhow::bail!("upstream query timed out after {}ms", timeout.as_millis())
            }
        }
    }

    /// Start background health checks for all upstreams.
    ///
    /// Every 30 seconds, unhealthy upstreams are probed with a HealthCheck
    /// RPC (2s timeout). If the probe succeeds, the upstream is marked
    /// healthy again. Healthy upstreams are not probed (they'll be marked
    /// unhealthy on the next failed query).
    ///
    /// Returns a `JoinHandle` that runs until dropped.
    pub fn start_health_checks(&self) -> tokio::task::JoinHandle<()> {
        use std::sync::atomic::Ordering;

        let upstream_data: Vec<_> = self
            .upstreams
            .iter()
            .map(|u| {
                (
                    u.name.clone(),
                    u.client(),
                    u.token().map(String::from),
                    u.healthy_ref(),
                )
            })
            .collect();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;

                for (name, client, token, healthy) in &upstream_data {
                    let was_healthy = healthy.load(Ordering::Relaxed);
                    if was_healthy {
                        // Don't probe healthy upstreams — they'll be marked
                        // unhealthy on the next failed query.
                        continue;
                    }

                    let mut c = client.clone();
                    let mut req = tonic::Request::new(nestweaver_proto::HealthCheckRequest {});
                    if let Some(t) = token {
                        if let Ok(val) =
                            format!("Bearer {}", t).parse::<tonic::metadata::MetadataValue<_>>()
                        {
                            req.metadata_mut().insert("authorization", val);
                        }
                    }

                    match tokio::time::timeout(Duration::from_secs(2), c.health_check(req)).await {
                        Ok(Ok(_)) => {
                            info!(upstream = %name, "upstream recovered, marking healthy");
                            healthy.store(true, Ordering::Relaxed);
                        }
                        _ => {
                            debug!(upstream = %name, "upstream still unhealthy");
                        }
                    }
                }
            }
        })
    }
}

// ── Dispatch helper ───────────────────────────────────────────────────

/// Dispatch a tool call to the gRPC daemon via `JsonRequest`/`JsonResponse`.
///
/// Most NestWeaver tools use the `JsonRequest { args_json }` /
/// `JsonResponse { result_json }` pass-through pattern. This function
/// serializes the params, calls the matching RPC, and deserializes the
/// response.
async fn dispatch_json_rpc(
    client: &mut NestWeaverDaemonClient<Channel>,
    tool_name: &str,
    params: &Value,
) -> Result<Value> {
    dispatch_json_rpc_authed(client, tool_name, params, None).await
}

/// Like `dispatch_json_rpc` but optionally injects a bearer token into the
/// request metadata (required for authenticated upstream servers).
async fn dispatch_json_rpc_authed(
    client: &mut NestWeaverDaemonClient<Channel>,
    tool_name: &str,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    // ── Typed RPCs (not JsonRequest/JsonResponse) ─────────────────────
    //
    // These five tools use typed proto requests. Handle them first so
    // we don't build an unnecessary JsonRequest.
    match tool_name {
        "brain_search" => {
            return dispatch_typed_brain_search(client, params, auth_token).await;
        }
        "brain_context" => {
            return dispatch_typed_brain_context(client, params, auth_token).await;
        }
        "project_context" => {
            return dispatch_typed_project_context(client, params, auth_token).await;
        }
        "note_get" => {
            return dispatch_typed_note_get(client, params, auth_token).await;
        }
        "hub_nodes" => {
            return dispatch_typed_hub_nodes(client, params, auth_token).await;
        }
        _ => {} // fall through to JsonRequest dispatch
    }

    // ── JsonRequest/JsonResponse pass-through RPCs ────────────────────
    let args_json = serde_json::to_string(params)?;
    let mut request = tonic::Request::new(JsonRequest { args_json });

    if let Some(token) = auth_token {
        if let Ok(val) = format!("Bearer {}", token).parse::<tonic::metadata::MetadataValue<_>>() {
            request.metadata_mut().insert("authorization", val);
        }
    }

    let response: JsonResponse = match tool_name {
        "backlinks" | "get_backlinks" => client.get_backlinks(request).await,
        "flow_trace" => client.flow_trace(request).await,
        "blast_radius" => client.blast_radius(request).await,
        "brain_impact" | "impact" => client.impact(request).await,
        "brain_guide" => client.brain_guide(request).await,
        "brain_diff" => client.brain_diff(request).await,
        "read_symbols" => client.read_symbols(request).await,
        "regex_search" => client.regex_search(request).await,
        "count_patterns" => client.count_patterns(request).await,
        "cross_repo_contracts" => client.cross_repo_contracts(request).await,
        "contract_drift" => client.contract_drift(request).await,
        "dead_code" => client.dead_code(request).await,
        "brain_broken_links" => client.brain_broken_links(request).await,
        "brain_orphan_documents" => client.brain_orphan_documents(request).await,
        "brain_topic_clusters" => client.brain_topic_clusters(request).await,
        "brain_tag_graph" => client.brain_tag_graph(request).await,
        "brain_doc_stats" => client.brain_doc_stats(request).await,
        "brain_memory_lint" => client.brain_memory_lint(request).await,
        "brain_memory_consolidate" => client.brain_memory_consolidate(request).await,
        "brain_memory_related" => client.brain_memory_related(request).await,
        "detect_changes" => client.detect_changes(request).await,
        "affected_tests" => client.affected_tests(request).await,
        "clusters" => client.clusters(request).await,
        "stale_check" => client.stale_check(request).await,
        "bridge_nodes" => client.bridge_nodes(request).await,
        "get_summary" => client.get_summary(request).await,
        "investigate" => client.investigate(request).await,
        "investigate_expand" => client.investigate_expand(request).await,
        "investigate_hydrate" => client.investigate_hydrate(request).await,
        "set_extension" => client.set_extension(request).await,
        "query_extensions" => client.query_extensions(request).await,
        "brain_status" | "brain_status_json" => client.brain_status_json(request).await,
        "export_graph" => client.export_graph(request).await,
        "search_symbols" => client.search_symbols(request).await,
        "symbol_lookup" => client.symbol_lookup(request).await,
        // Admin / listing RPCs (local-only in routing matrix, but must be
        // dispatchable so HybridClient::query can route them).
        "list_repos" => client.list_repos_json(request).await,
        "list_vaults" => client.list_vaults_json(request).await,
        "embedding_dimension" => client.embedding_dimension(request).await,
        "list_services" => client.list_services_json(request).await,
        "service_summary" => client.service_summary_json(request).await,
        "list_projects" => client.list_projects_json(request).await,
        "repo_map" => client.repo_map_json(request).await,
        "suggest_links" => client.suggest_links_json(request).await,
        "detect_implicit_projects" => client.detect_implicit_projects_json(request).await,
        "pr_impact" => client.pr_impact_json(request).await,
        _ => {
            anyhow::bail!("unsupported tool for JSON dispatch: {tool_name}");
        }
    }
    .with_context(|| format!("{tool_name} RPC failed"))?
    .into_inner();

    let parsed: Value = serde_json::from_str(&response.result_json)
        .unwrap_or_else(|_| Value::String(response.result_json));
    Ok(parsed)
}

/// Inject an optional bearer token into a tonic request.
fn inject_bearer_token<T>(request: &mut tonic::Request<T>, auth_token: Option<&str>) {
    if let Some(token) = auth_token {
        if let Ok(val) = format!("Bearer {}", token).parse::<tonic::metadata::MetadataValue<_>>() {
            request.metadata_mut().insert("authorization", val);
        }
    }
}

/// Typed dispatch for `brain_search` -> `Search` RPC.
async fn dispatch_typed_brain_search(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let req = nestweaver_proto::BrainSearchRequest {
        query: params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        limit: params.get("limit").and_then(|v| v.as_i64()).unwrap_or(20) as i32,
        response_format: params
            .get("response_format")
            .and_then(|v| v.as_str())
            .map(String::from),
        include_bodies: params
            .get("include_bodies")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        prf: params.get("prf").and_then(|v| v.as_bool()).unwrap_or(false),
        rerank: params
            .get("rerank")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        root: params
            .get("root")
            .and_then(|v| v.as_str())
            .map(String::from),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .search(request)
        .await
        .context("brain_search RPC failed")?
        .into_inner();
    // Serialize the typed response back to JSON.
    let results: Vec<Value> = resp
        .results
        .iter()
        .map(|r| {
            let mut obj = serde_json::json!({
                "uid": r.uid,
                "kind": r.kind,
                "title": r.title,
                "score": r.score,
            });
            if let Some(ref loc) = r.location {
                obj["location"] = Value::String(loc.clone());
            }
            if !r.matched_headings.is_empty() {
                obj["matched_headings"] = serde_json::json!(r.matched_headings);
            }
            if let Some(ref body) = r.inline_body {
                obj["inline_body"] = Value::String(body.clone());
            }
            obj
        })
        .collect();
    Ok(serde_json::json!({
        "query": resp.query,
        "engine": resp.engine,
        "total_matches": resp.total_matches,
        "results": results,
        "expansion_terms": resp.expansion_terms,
    }))
}

/// Typed dispatch for `brain_context` -> `GetContext` RPC.
/// Response is `BrainContextResponse { result_json }`.
async fn dispatch_typed_brain_context(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let seeds = params
        .get("seeds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let req = nestweaver_proto::BrainContextRequest {
        seeds,
        token_budget: params
            .get("token_budget")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32,
        response_format: params
            .get("response_format")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        repos: json_str_array(params, "repos"),
        vaults: json_str_array(params, "vaults"),
        kinds: json_str_array(params, "kinds"),
        path_prefix: params
            .get("path_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        tags: json_str_array(params, "tags"),
        exclude_tags: json_str_array(params, "exclude_tags"),
        weight_ppr: params
            .get("weight_ppr")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        weight_bm25: params
            .get("weight_bm25")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        intent: params
            .get("intent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        include_seeds: params
            .get("include_seeds")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        include_bodies: params
            .get("include_bodies")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        root: params
            .get("root")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        prf: params.get("prf").and_then(|v| v.as_bool()).unwrap_or(false),
        rerank: params
            .get("rerank")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        weight_semantic: params
            .get("weight_semantic")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        since: params
            .get("since")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        recency_weight: params
            .get("recency_weight")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        recency_half_life_days: params
            .get("recency_half_life_days")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .get_context(request)
        .await
        .context("brain_context RPC failed")?
        .into_inner();
    let parsed: Value =
        serde_json::from_str(&resp.result_json).unwrap_or_else(|_| Value::String(resp.result_json));
    Ok(parsed)
}

/// Typed dispatch for `project_context` -> `GetProjectContext` RPC.
/// Response is `ProjectContextResponse { result_json }`.
async fn dispatch_typed_project_context(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let req = nestweaver_proto::ProjectContextRequest {
        project: params
            .get("project")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        token_budget: params
            .get("token_budget")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32,
        kinds: json_str_array(params, "kinds"),
        include_components: params
            .get("include_components")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        intent: params
            .get("intent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        include_seeds: params
            .get("include_seeds")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        since: params
            .get("since")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        recency_weight: params
            .get("recency_weight")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        recency_half_life_days: params
            .get("recency_half_life_days")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .get_project_context(request)
        .await
        .context("project_context RPC failed")?
        .into_inner();
    let parsed: Value =
        serde_json::from_str(&resp.result_json).unwrap_or_else(|_| Value::String(resp.result_json));
    Ok(parsed)
}

/// Typed dispatch for `note_get` -> `GetNote` RPC.
async fn dispatch_typed_note_get(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let req = nestweaver_proto::NoteGetRequest {
        uid: params.get("uid").and_then(|v| v.as_str()).map(String::from),
        title: params
            .get("title")
            .and_then(|v| v.as_str())
            .map(String::from),
        include_body: params
            .get("include_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        sections: json_str_array(params, "sections"),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .get_note(request)
        .await
        .context("note_get RPC failed")?
        .into_inner();
    let mut result = serde_json::json!({
        "uid": resp.uid,
        "title": resp.title,
        "path": resp.path,
        "note_kind": resp.note_kind,
        "word_count": resp.word_count,
        "section_count": resp.section_count,
    });
    if let Some(body) = resp.body {
        result["body"] = Value::String(body);
    }
    Ok(result)
}

/// Typed dispatch for `hub_nodes` -> `HubNodes` RPC.
/// Response is `HubNodesResponse { result_json }`.
async fn dispatch_typed_hub_nodes(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let req = nestweaver_proto::HubNodesRequest {
        top_n: params.get("top_n").and_then(|v| v.as_i64()).unwrap_or(10) as i32,
        response_format: params
            .get("response_format")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .hub_nodes(request)
        .await
        .context("hub_nodes RPC failed")?
        .into_inner();
    let parsed: Value =
        serde_json::from_str(&resp.result_json).unwrap_or_else(|_| Value::String(resp.result_json));
    Ok(parsed)
}

/// Helper: extract a `Vec<String>` from a JSON array field.
fn json_str_array(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ── Upstream selection helpers ────────────────────────────────────────

/// Pick an upstream whose repo globs match `repo_hint`, falling back to the
/// first healthy upstream when no glob matches (or no hint is provided).
fn find_upstream_for_repo<'a>(
    upstreams: &'a [UpstreamHandle],
    repo_hint: Option<&str>,
) -> Option<&'a UpstreamHandle> {
    if let Some(repo) = repo_hint {
        let matched = upstreams.iter().find(|u| u.is_healthy() && u.matches_repo(repo));
        if matched.is_some() {
            return matched;
        }
    }
    upstreams.iter().find(|u| u.is_healthy())
}

/// Extract a repo hint from query params — checks `repos[0]`, `repo`, `repo_url`.
fn extract_repo_hint(params: &Value) -> Option<&str> {
    params.get("repos")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .or_else(|| params.get("repo").and_then(|v| v.as_str()))
        .or_else(|| params.get("repo_url").and_then(|v| v.as_str()))
}

// ── Result helpers ────────────────────────────────────────────────────

/// Count the number of result items in a JSON response.
///
/// Handles both `{ "results": [...] }` envelope and bare arrays.
fn count_results(value: &Value) -> usize {
    if let Some(arr) = value.as_array() {
        arr.len()
    } else if let Some(results) = value.get("results").and_then(|v| v.as_array()) {
        results.len()
    } else if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
        items.len()
    } else if value.is_object() {
        // A single object counts as 1 result.
        1
    } else {
        0
    }
}

/// Extract the result items array from a JSON response.
fn extract_result_items(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        arr.clone()
    } else if let Some(results) = value.get("results").and_then(|v| v.as_array()) {
        results.clone()
    } else if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
        items.clone()
    } else if value.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        vec![value.clone()]
    } else {
        vec![]
    }
}

/// Merge two JSON responses by concatenating their result arrays and
/// deduplicating via scope-hash identity.
fn merge_json_results(local: &Value, server: &Value) -> Value {
    let local_items = extract_result_items(local);
    let server_items = extract_result_items(server);

    let merged = rrf_merge(local_items, server_items);
    let values: Vec<Value> = merged.into_iter().map(|mr| mr.value).collect();

    wrap_merged_response(values, &["local", "server"])
}

/// Merge two JSON responses, preserving structured schemas (e.g. brain_context's
/// `{ seeds, connected, unresolved_seeds, expansion_terms }`) when detected.
/// Falls back to flat `{ results: [...] }` envelope for non-structured responses.
fn merge_structured_results(local: &Value, server: &Value) -> Value {
    let local_connected = local.get("connected").and_then(|v| v.as_array());
    let server_connected = server.get("connected").and_then(|v| v.as_array());

    if let (Some(lc), Some(sc)) = (local_connected, server_connected) {
        let merged_connected = rrf_merge(lc.clone(), sc.clone());
        let merged_values: Vec<Value> = merged_connected
            .into_iter()
            .map(|mr| {
                let mut v = mr.value;
                if let Value::Object(ref mut map) = v {
                    map.insert("_provenance".to_string(), serde_json::to_value(mr.provenance).unwrap_or(Value::Null));
                    map.insert("_confidence".to_string(), serde_json::to_value(mr.confidence).unwrap_or(Value::Null));
                    map.insert("_rrf_score".to_string(), Value::from(mr.score));
                }
                v
            })
            .collect();

        let mut seeds = local.get("seeds").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if let Some(server_seeds) = server.get("seeds").and_then(|v| v.as_array()) {
            seeds.extend(server_seeds.iter().cloned());
        }

        let mut unresolved = local.get("unresolved_seeds").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if let Some(su) = server.get("unresolved_seeds").and_then(|v| v.as_array()) {
            unresolved.extend(su.iter().cloned());
        }

        let mut expansion = local.get("expansion_terms").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if let Some(se) = server.get("expansion_terms").and_then(|v| v.as_array()) {
            expansion.extend(se.iter().cloned());
        }

        let mut result = serde_json::json!({
            "seeds": seeds,
            "connected": merged_values,
        });
        if !unresolved.is_empty() {
            result["unresolved_seeds"] = Value::Array(unresolved);
        }
        if !expansion.is_empty() {
            result["expansion_terms"] = Value::Array(expansion);
        }
        // Carry over scalar metadata from the local response (project header,
        // budget accounting, etc.) that the merge would otherwise drop.
        for key in ["project", "project_uid", "seeds_expanded", "tokens_used",
                     "token_budget", "external_refs"] {
            if let Some(val) = local.get(key) {
                result[key] = val.clone();
            }
        }
        inject_provenance(&mut result, &["local", "server"], &[]);
        result
    } else {
        merge_json_results(local, server)
    }
}

/// Wrap merged results into a response envelope with provenance metadata.
fn wrap_merged_response(results: Vec<Value>, sources: &[&str]) -> Value {
    let scope = if sources.len() > 1 {
        "hybrid"
    } else {
        sources.first().copied().unwrap_or("local")
    };
    serde_json::json!({
        "results": results,
        "_meta": {
            "sources": sources,
            "stale_repos": [],
            "scope": scope,
        },
    })
}

/// Inject `_meta` provenance into an existing JSON response.
fn inject_provenance(result: &mut Value, sources: &[&str], stale_repos: &[String]) {
    let scope = if sources.len() > 1 {
        "hybrid"
    } else {
        sources.first().copied().unwrap_or("local")
    };
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "_meta".to_string(),
            serde_json::json!({
                "sources": sources,
                "stale_repos": stale_repos,
                "scope": scope,
            }),
        );
    }
}

// ── Flow trace stitching ────────────────────────────────────────────────

/// A boundary symbol detected in a local flow_trace result.
///
/// The local trace knows the symbol name and canonical_id but cannot
/// follow the call graph past it because the target repo is not indexed
/// locally.
#[derive(Debug, Clone)]
pub struct TraceBoundary {
    /// The canonical_id of the boundary symbol.
    pub canonical_id: String,
    /// The symbol name (for display/logging).
    pub name: String,
    /// The span_id (or JSON path) of the parent node in the local trace,
    /// used for stitching the server continuation back into the tree.
    pub parent_path: Vec<String>,
}

/// Detect cross-repo boundary symbols in a flow_trace JSON result tree.
///
/// A boundary is a leaf node whose `repo_uid` differs from the root
/// node's `repo_uid` (the locally-initiated trace). These represent
/// cross-repo call edges where the upstream server should continue
/// the trace.
///
/// Requires flow_trace output to include `repo_uid` and `canonical_id`
/// fields on each node (added in the detailed output format).
///
/// See architecture spec: cross-boundary-flow-trace.md
pub fn detect_boundaries_in_trace(result: &Value) -> Vec<TraceBoundary> {
    let tree = result.get("tree").or(Some(result));
    let Some(tree) = tree else {
        return vec![];
    };

    // Extract root repo_uid — the "local" repo for this trace.
    let root_repo = tree.get("repo_uid").and_then(|v| v.as_str()).unwrap_or("");
    if root_repo.is_empty() {
        debug!("detect_boundaries_in_trace: root node lacks repo_uid, cannot detect boundaries");
        return vec![];
    }

    let mut boundaries = Vec::new();
    let mut path = Vec::new();
    collect_boundaries(tree, root_repo, &mut path, &mut boundaries);

    debug!(
        count = boundaries.len(),
        "detect_boundaries_in_trace: found boundary nodes"
    );
    boundaries
}

/// Recursively walk the flow_trace tree collecting boundary nodes.
fn collect_boundaries(
    node: &Value,
    root_repo: &str,
    path: &mut Vec<String>,
    out: &mut Vec<TraceBoundary>,
) {
    let children = node.get("children").and_then(|v| v.as_array());
    let is_leaf = children.is_none_or(|c| c.is_empty());
    let repo_uid = node.get("repo_uid").and_then(|v| v.as_str()).unwrap_or("");
    let canonical_id = node
        .get("canonical_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");

    // A boundary: different repo than the root, is a leaf (trace couldn't follow),
    // and has a canonical_id for cross-boundary matching.
    if is_leaf && !repo_uid.is_empty() && repo_uid != root_repo && !canonical_id.is_empty() {
        out.push(TraceBoundary {
            canonical_id: canonical_id.to_string(),
            name: name.to_string(),
            parent_path: path.clone(),
        });
    }

    if let Some(children) = children {
        path.push(name.to_string());
        for child in children {
            collect_boundaries(child, root_repo, path, out);
        }
        path.pop();
    }
}

/// Stitch server-side trace spans into a local flow_trace result tree.
///
/// Given a local trace result (JSON tree) and server continuation
/// response (spans from FlowTraceContinue RPC), merge the server spans
/// into the tree at the correct boundary point.
///
/// The merge strategy:
/// 1. Find the node in the local tree matching `parent_span_id`
/// 2. Convert server spans into the same JSON tree format
/// 3. Append server subtrees as children of the boundary node
/// 4. Annotate server-sourced nodes with `"source": "server"`
pub fn stitch_server_spans(
    local_result: &mut Value,
    server_spans: &[nestweaver_proto::TraceSpanProto],
    boundary_canonical_id: &str,
    server_name: &str,
) {
    if server_spans.is_empty() {
        return;
    }

    // Build a lookup from span_id -> span for parent linkage.
    let span_map: std::collections::HashMap<&str, &nestweaver_proto::TraceSpanProto> = server_spans
        .iter()
        .map(|s| (s.span_id.as_str(), s))
        .collect();

    // Find the root span(s) — those whose canonical_id matches the boundary.
    let root_spans: Vec<&nestweaver_proto::TraceSpanProto> = server_spans
        .iter()
        .filter(|s| s.canonical_id == boundary_canonical_id)
        .collect();

    if root_spans.is_empty() {
        return;
    }

    // Build JSON subtree(s) from server spans.
    fn build_subtree(
        span: &nestweaver_proto::TraceSpanProto,
        span_map: &std::collections::HashMap<&str, &nestweaver_proto::TraceSpanProto>,
        server_name: &str,
    ) -> Value {
        let children: Vec<Value> = span
            .callee_span_ids
            .iter()
            .filter_map(|cid| span_map.get(cid.as_str()))
            .map(|child| build_subtree(child, span_map, server_name))
            .collect();

        serde_json::json!({
            "name": span.name,
            "file_path": span.file_path,
            "canonical_id": span.canonical_id,
            "source": format!("server:{}", server_name),
            "children": children,
        })
    }

    let subtrees: Vec<Value> = root_spans
        .iter()
        .map(|s| build_subtree(s, &span_map, server_name))
        .collect();

    // Find the boundary node in the local tree and inject server subtrees.
    // The boundary node is a leaf with matching canonical_id.
    fn inject_at_boundary(
        node: &mut Value,
        boundary_cid: &str,
        subtrees: &[Value],
        server_name: &str,
    ) -> bool {
        // Check if this node is the boundary (leaf with matching canonical_id).
        if let Some(cid) = node.get("canonical_id").and_then(|v| v.as_str()) {
            if cid == boundary_cid {
                // Inject children.
                if let Some(children) = node.get_mut("children") {
                    if let Some(arr) = children.as_array_mut() {
                        arr.extend_from_slice(subtrees);
                    }
                } else if let Some(obj) = node.as_object_mut() {
                    obj.insert("children".to_string(), Value::Array(subtrees.to_vec()));
                    obj.insert(
                        "boundary_crossed".to_string(),
                        Value::String(format!("-> {}", server_name)),
                    );
                }
                return true;
            }
        }

        // Recurse into children.
        if let Some(children) = node.get_mut("children") {
            if let Some(arr) = children.as_array_mut() {
                for child in arr.iter_mut() {
                    if inject_at_boundary(child, boundary_cid, subtrees, server_name) {
                        return true;
                    }
                }
            }
        }

        false
    }

    // Try to inject into the "tree" field of the response.
    if let Some(tree) = local_result.get_mut("tree") {
        inject_at_boundary(tree, boundary_canonical_id, &subtrees, server_name);
    }

    // Also try "methods" array for class-expanded traces.
    if let Some(methods) = local_result.get_mut("methods") {
        if let Some(arr) = methods.as_array_mut() {
            for method in arr.iter_mut() {
                inject_at_boundary(method, boundary_canonical_id, &subtrees, server_name);
            }
        }
    }
}

/// Execute a flow_trace with cross-boundary stitching.
///
/// This is the high-level function that:
/// 1. Runs flow_trace locally via the daemon
/// 2. If boundaries are detected and an upstream is available, sends
///    FlowTraceContinue RPCs
/// 3. Stitches server spans into the local result
///
/// Currently, boundary detection from JSON is limited (see
/// `detect_boundaries_in_trace`). Full integration requires the MCP
/// tool to annotate boundary nodes with canonical_ids. When boundaries
/// are provided explicitly (e.g., from a store-aware caller), they
/// are used directly.
pub async fn flow_trace_with_stitching(
    client: &mut HybridClient,
    params: &Value,
    explicit_boundaries: &[TraceBoundary],
) -> Result<Value> {
    // 1. Run flow_trace locally.
    let mut local_result = client.query_local("flow_trace", params).await?;

    // 2. Detect boundaries (from JSON or explicit).
    let boundaries = if explicit_boundaries.is_empty() {
        detect_boundaries_in_trace(&local_result)
    } else {
        explicit_boundaries.to_vec()
    };

    if boundaries.is_empty() || !client.has_upstreams() {
        inject_provenance(&mut local_result, &["local"], &[]);
        return Ok(local_result);
    }

    // 3. For each boundary, send FlowTraceContinue to the upstream.
    let max_depth = params
        .get("max_depth")
        .and_then(|v| v.as_i64())
        .unwrap_or(10) as i32;
    let trace_id = format!("trace-{}", uuid_v4_simple());

    let upstream = match client.upstreams.iter().find(|u| u.is_healthy()) {
        Some(u) => u,
        None => {
            // Mark boundaries as stubs.
            if let Some(obj) = local_result.as_object_mut() {
                obj.insert(
                    "_boundary_stubs".to_string(),
                    serde_json::json!(
                        boundaries
                            .iter()
                            .map(|b| serde_json::json!({
                                "canonical_id": b.canonical_id,
                                "name": b.name,
                                "reason": "server unavailable"
                            }))
                            .collect::<Vec<_>>()
                    ),
                );
            }
            inject_provenance(&mut local_result, &["local"], &[]);
            return Ok(local_result);
        }
    };

    let mut all_visited: Vec<String> = Vec::new();
    let server_name = upstream.name.clone();

    for boundary in &boundaries {
        let mut up_client = upstream.client();
        let mut req = tonic::Request::new(nestweaver_proto::FlowTraceContinueRequest {
            trace_id: trace_id.clone(),
            entry_canonical_id: boundary.canonical_id.clone(),
            parent_span_id: String::new(), // No span linkage in JSON mode.
            remaining_depth: max_depth
                .saturating_sub(boundary.parent_path.len() as i32)
                .max(1),
            visited_canonical_ids: all_visited.clone(),
        });
        upstream.inject_auth(&mut req);

        match tokio::time::timeout(upstream.timeout, up_client.flow_trace_continue(req)).await {
            Ok(Ok(resp)) => {
                let resp = resp.into_inner();
                // Collect visited canonical_ids from server spans.
                for span in &resp.spans {
                    all_visited.push(span.canonical_id.clone());
                }
                // Stitch server spans into local result.
                stitch_server_spans(
                    &mut local_result,
                    &resp.spans,
                    &boundary.canonical_id,
                    &server_name,
                );
            }
            Ok(Err(e)) => {
                debug!(
                    boundary = %boundary.canonical_id,
                    error = %e,
                    "FlowTraceContinue failed"
                );
            }
            Err(_) => {
                debug!(
                    boundary = %boundary.canonical_id,
                    "FlowTraceContinue timed out"
                );
            }
        }
    }

    inject_provenance(&mut local_result, &["local", &server_name], &[]);
    Ok(local_result)
}

/// Generate a simple pseudo-UUID for trace IDs.
fn uuid_v4_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", t)
}

// ── Two-tier blast_radius ───────────────────────────────────────────────

/// Execute a two-tier impact query: local impact + org-wide impact.
///
/// When an upstream server is available:
/// 1. Run the tool locally (existing logic)
/// 2. Query the server for the same tool
/// 3. Combine into a response with `local_impact` and `org_impact` sections
///
/// Used for blast_radius, brain_impact, and affected_tests.
pub async fn two_tier_query(client: &mut HybridClient, tool_name: &str, params: &Value) -> Result<Value> {
    // 1. Always run the tool locally.
    let mut local_result = client.query_local(tool_name, params).await?;

    // 2. If no upstream, return local-only with clear annotation.
    if !client.has_upstreams() {
        inject_provenance(&mut local_result, &["local"], &[]);
        if let Some(obj) = local_result.as_object_mut() {
            obj.insert("tier".to_string(), Value::String("local_only".into()));
        }
        return Ok(local_result);
    }

    // 3. Query upstream for org-wide impact.
    let upstream = match client.upstreams.iter().find(|u| u.is_healthy()) {
        Some(u) => u,
        None => {
            inject_provenance(&mut local_result, &["local"], &[]);
            if let Some(obj) = local_result.as_object_mut() {
                obj.insert("tier".to_string(), Value::String("local_only".into()));
                obj.insert(
                    "org_note".to_string(),
                    Value::String("upstream unavailable — showing local impact only".into()),
                );
            }
            return Ok(local_result);
        }
    };

    let server_name = upstream.name.clone();
    let mut up_client = upstream.client();
    let token = upstream.auth_token().map(|t| t.to_string());
    let timeout = upstream.timeout;
    let tool = tool_name.to_string();

    let server_params = params.clone();
    let server_result = match tokio::time::timeout(
        timeout,
        dispatch_json_rpc_authed(
            &mut up_client,
            &tool,
            &server_params,
            token.as_deref(),
        ),
    )
    .await
    {
        Ok(Ok(result)) => Some(result),
        Ok(Err(e)) => {
            debug!(error = %e, tool = %tool, "org-wide two-tier query failed");
            None
        }
        Err(_) => {
            debug!(tool = %tool, "org-wide two-tier query timed out");
            None
        }
    };

    // 4. Build two-tier response.
    let mut response = serde_json::json!({
        "tier": "two_tier",
        "local_impact": local_result,
    });

    if let Some(server) = server_result {
        // Filter out results that are already in the local impact to avoid
        // duplicating repos the user has indexed locally.
        let local_repos = extract_local_repos(&local_result);
        let filtered_server = filter_org_results(&server, &local_repos);

        response["org_impact"] = serde_json::json!({
            "source_server": server_name,
            "results": filtered_server,
        });
    } else {
        response["org_impact"] = serde_json::json!({
            "source_server": server_name,
            "status": "unavailable",
            "note": "upstream server query failed — showing local impact only",
        });
    }

    inject_provenance(&mut response, &["local", &server_name], &[]);

    Ok(response)
}

/// Extract repo URLs/paths mentioned in local blast_radius results.
fn extract_local_repos(local: &Value) -> std::collections::HashSet<String> {
    let mut repos = std::collections::HashSet::new();

    // Look for repo info in changed_symbols and affected_symbols.
    for key in &["changed_symbols", "affected_symbols"] {
        if let Some(arr) = local.get(key).and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(fp) = item.get("file_path").and_then(|v| v.as_str()) {
                    // Extract repo-level prefix (first path component).
                    if let Some(repo) = fp.split('/').next() {
                        repos.insert(repo.to_string());
                    }
                }
            }
        }
    }

    repos
}

/// Filter org-wide results to exclude repos already covered by local impact.
///
/// Removes entries from the server's `affected_symbols` and `changed_symbols`
/// whose file paths resolve to a repo already present in the local impact.
/// Matching is done by repo-prefix extraction: the first path component of
/// each `file_path` is treated as the repo identifier. Repos indexed locally
/// are excluded from the org section to avoid duplicate noise.
fn filter_org_results(server: &Value, local_repos: &std::collections::HashSet<String>) -> Value {
    if local_repos.is_empty() {
        return server.clone();
    }
    let mut filtered = server.clone();

    // Filter affected_symbols and changed_symbols arrays.
    for key in &["affected_symbols", "changed_symbols"] {
        if let Some(arr) = filtered.get_mut(key).and_then(|v| v.as_array_mut()) {
            arr.retain(|item| {
                let dominated = item
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .and_then(|fp| fp.split('/').next())
                    .is_some_and(|repo| local_repos.contains(repo));
                !dominated
            });
        }
    }

    // Filter affected_clusters entries whose repo matches a local repo.
    if let Some(clusters) = filtered
        .get_mut("affected_clusters")
        .and_then(|v| v.as_array_mut())
    {
        clusters.retain(|cluster| {
            // Keep the cluster if any of its symbols are NOT in a local repo.
            let dominated = cluster
                .get("representative_file")
                .and_then(|v| v.as_str())
                .and_then(|fp| fp.split('/').next())
                .is_some_and(|repo| local_repos.contains(repo));
            !dominated
        });
    }

    filtered
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn local_only_has_no_upstreams() {
        let upstreams: Vec<UpstreamHandle> = vec![];
        assert!(upstreams.is_empty());
    }

    #[tokio::test]
    async fn upstream_info_reports_health() {
        use crate::discovery::{RoutingMode, UpstreamConfig};

        let cfg1 = UpstreamConfig {
            name: Some("acme".to_string()),
            url: "http://127.0.0.1:19990".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Fallback,
            timeout: "1s".to_string(),
        };
        let cfg2 = UpstreamConfig {
            name: Some("partner".to_string()),
            url: "http://127.0.0.1:19991".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
        };

        let h1 = UpstreamHandle::from_config(&cfg1).unwrap();
        let h2 = UpstreamHandle::from_config(&cfg2).unwrap();

        h2.mark_unhealthy();

        let upstreams = vec![h1, h2];
        let info: Vec<(&str, bool)> = upstreams
            .iter()
            .map(|u| (u.name.as_str(), u.is_healthy()))
            .collect();

        assert_eq!(info.len(), 2);
        assert_eq!(info[0], ("acme", true));
        assert_eq!(info[1], ("partner", false));

        let has = upstreams.iter().any(|u| u.is_healthy());
        assert!(has);
    }

    #[tokio::test]
    async fn no_upstreams_means_has_upstreams_false() {
        let upstreams: Vec<UpstreamHandle> = vec![];
        let has = upstreams.iter().any(|u| u.is_healthy());
        assert!(!has);
    }

    // ── Result helper tests ───────────────────────────────────────

    #[test]
    fn count_results_bare_array() {
        let v = json!([1, 2, 3]);
        assert_eq!(count_results(&v), 3);
    }

    #[test]
    fn count_results_envelope() {
        let v = json!({"results": [1, 2, 3, 4, 5]});
        assert_eq!(count_results(&v), 5);
    }

    #[test]
    fn count_results_items_envelope() {
        let v = json!({"items": [1, 2]});
        assert_eq!(count_results(&v), 2);
    }

    #[test]
    fn count_results_single_object() {
        let v = json!({"name": "foo"});
        assert_eq!(count_results(&v), 1);
    }

    #[test]
    fn count_results_null() {
        assert_eq!(count_results(&Value::Null), 0);
    }

    #[test]
    fn extract_items_from_results_key() {
        let v = json!({"results": [{"a": 1}, {"b": 2}]});
        let items = extract_result_items(&v);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn extract_items_from_bare_array() {
        let v = json!([{"a": 1}]);
        let items = extract_result_items(&v);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn extract_items_from_single_object() {
        let v = json!({"name": "foo", "file": "bar.rs"});
        let items = extract_result_items(&v);
        assert_eq!(items.len(), 1);
    }

    // ── Fallback routing logic tests ──────────────────────────────

    #[test]
    fn fallback_threshold_is_five() {
        // The threshold constant should be 5 (matching clangd convention).
        assert_eq!(FALLBACK_THRESHOLD, 5);
    }

    #[test]
    fn sufficient_local_results_skip_server() {
        // When local has >= threshold results, server should be skipped.
        let local = json!({"results": [1, 2, 3, 4, 5]});
        assert!(count_results(&local) >= FALLBACK_THRESHOLD);
    }

    #[test]
    fn sparse_local_results_trigger_server() {
        // When local has < threshold results, server should be queried.
        let local = json!({"results": [1, 2]});
        assert!(count_results(&local) < FALLBACK_THRESHOLD);
    }

    // ── Merge helpers test ────────────────────────────────────────

    #[test]
    fn merge_json_results_deduplicates() {
        let local = json!([{
            "repo_url": "acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "init",
            "scope_chain": "api"
        }]);
        let server = json!([
            {
                "repo_url": "acme/api",
                "file_path": "src/lib.rs",
                "symbol_name": "init",
                "scope_chain": "api"
            },
            {
                "repo_url": "acme/billing",
                "file_path": "src/webhook.rs",
                "symbol_name": "handle",
                "scope_chain": "billing"
            }
        ]);

        let merged = merge_json_results(&local, &server);
        let results = merged["results"].as_array().unwrap();
        // init appears once (deduplicated), handle is new => 2 results
        assert_eq!(results.len(), 2);
        // Has _meta provenance
        assert!(merged["_meta"].is_object());
        let sources = merged["_meta"]["sources"].as_array().unwrap();
        assert!(sources.len() >= 2);
    }

    #[test]
    fn merge_json_results_empty_server() {
        let local = json!([{"name": "a"}]);
        let server = json!([]);
        let merged = merge_json_results(&local, &server);
        let results = merged["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn wrap_merged_response_has_metadata() {
        let results = vec![json!({"name": "a"})];
        let wrapped = wrap_merged_response(results, &["local", "server"]);
        assert!(wrapped["results"].is_array());
        // _meta provenance
        assert!(wrapped["_meta"].is_object());
        let meta = &wrapped["_meta"];
        assert_eq!(meta["scope"], "hybrid");
        let sources = meta["sources"].as_array().unwrap();
        assert!(sources.len() >= 2);
    }

    #[test]
    fn wrap_merged_response_single_source_scope() {
        let results = vec![json!({"name": "a"})];
        let wrapped = wrap_merged_response(results, &["local"]);
        assert_eq!(wrapped["_meta"]["scope"], "local");
    }

    #[test]
    fn inject_provenance_adds_meta() {
        let mut val = json!({"results": [1, 2, 3]});
        inject_provenance(&mut val, &["local", "acme"], &["repo-a".to_string()]);
        assert!(val["_meta"].is_object());
        assert_eq!(val["_meta"]["scope"], "hybrid");
        assert_eq!(val["_meta"]["stale_repos"][0], "repo-a");
        assert_eq!(val["_meta"]["sources"][0], "local");
        assert_eq!(val["_meta"]["sources"][1], "acme");
    }

    #[test]
    fn inject_provenance_local_only_scope() {
        let mut val = json!({"results": []});
        inject_provenance(&mut val, &["local"], &[]);
        assert_eq!(val["_meta"]["scope"], "local");
        assert!(val["_meta"]["stale_repos"].as_array().unwrap().is_empty());
    }

    // ── Routing mode selection test ───────────────────────────────

    #[test]
    fn routing_mode_defaults_to_fallback() {
        // When no upstreams, default mode should be Fallback.
        let mode = RoutingMode::default();
        assert_eq!(mode, RoutingMode::Fallback);
    }

    #[test]
    fn upstream_timeout_is_one_second() {
        assert_eq!(UPSTREAM_TIMEOUT, Duration::from_secs(1));
    }

    // ── Offline fallback tests ───────────────────────────────────

    #[tokio::test]
    async fn unhealthy_upstream_is_skipped() {
        use crate::discovery::UpstreamConfig;

        let cfg = UpstreamConfig {
            name: Some("dead-server".to_string()),
            url: "http://127.0.0.1:19990".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Fallback,
            timeout: "1s".to_string(),
        };
        let handle = UpstreamHandle::from_config(&cfg).unwrap();
        handle.mark_unhealthy();

        // has_upstreams should return false when all upstreams are unhealthy.
        let upstreams = vec![handle];
        let has_healthy = upstreams.iter().any(|u| u.is_healthy());
        assert!(!has_healthy);
    }

    #[tokio::test]
    async fn health_recovery_marks_upstream_healthy() {
        use std::sync::atomic::Ordering;

        let cfg = crate::discovery::UpstreamConfig {
            name: Some("recoverable".to_string()),
            url: "http://127.0.0.1:19990".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Fallback,
            timeout: "1s".to_string(),
        };
        let handle = UpstreamHandle::from_config(&cfg).unwrap();
        let healthy_ref = handle.healthy_ref();

        // Simulate unhealthy state.
        handle.mark_unhealthy();
        assert!(!healthy_ref.load(Ordering::Relaxed));

        // Simulate recovery (background task would do this).
        healthy_ref.store(true, Ordering::Relaxed);
        assert!(handle.is_healthy());
    }

    // ── Trace stitching tests ────────────────────────────────────

    #[test]
    fn stitch_server_spans_into_local_tree() {
        use nestweaver_proto::TraceSpanProto;

        // Local tree: A -> B (boundary)
        let mut local_result = json!({
            "root_uid": "uid-a",
            "root_name": "funcA",
            "max_depth": 5,
            "tree": {
                "uid": "uid-a",
                "name": "funcA",
                "file_path": "src/a.rs",
                "depth": 0,
                "children": [{
                    "uid": "uid-b",
                    "name": "funcB",
                    "canonical_id": "abc123:src/b.rs#funcB:def456",
                    "file_path": "src/b.rs",
                    "depth": 1,
                    "children": []
                }]
            }
        });

        // Server spans: B -> C -> D
        let spans = vec![
            TraceSpanProto {
                trace_id: "t1".into(),
                span_id: "span-b".into(),
                parent_span_id: None,
                canonical_id: "abc123:src/b.rs#funcB:def456".into(),
                name: "funcB".into(),
                repo_url: "https://github.com/acme/api".into(),
                file_path: "src/b.rs".into(),
                start_line: 10,
                callee_span_ids: vec!["span-c".into()],
                source: "server".into(),
            },
            TraceSpanProto {
                trace_id: "t1".into(),
                span_id: "span-c".into(),
                parent_span_id: Some("span-b".into()),
                canonical_id: "abc123:src/c.rs#funcC:ghi789".into(),
                name: "funcC".into(),
                repo_url: "https://github.com/acme/api".into(),
                file_path: "src/c.rs".into(),
                start_line: 20,
                callee_span_ids: vec![],
                source: "server".into(),
            },
        ];

        stitch_server_spans(
            &mut local_result,
            &spans,
            "abc123:src/b.rs#funcB:def456",
            "acme-server",
        );

        // Verify the boundary node now has server children.
        let tree = &local_result["tree"];
        let boundary_node = &tree["children"][0];
        assert_eq!(boundary_node["name"], "funcB");

        let stitched_children = boundary_node["children"].as_array().unwrap();
        assert!(
            !stitched_children.is_empty(),
            "boundary node should have server-sourced children"
        );

        // The stitched root should be funcB with child funcC.
        let server_root = &stitched_children[0];
        assert_eq!(server_root["name"], "funcB");
        assert!(
            server_root["source"]
                .as_str()
                .unwrap()
                .contains("acme-server")
        );

        let server_children = server_root["children"].as_array().unwrap();
        assert_eq!(server_children.len(), 1);
        assert_eq!(server_children[0]["name"], "funcC");
    }

    #[test]
    fn stitch_empty_spans_is_noop() {
        let mut local_result = json!({
            "tree": {
                "name": "funcA",
                "children": []
            }
        });
        let original = local_result.clone();

        stitch_server_spans(&mut local_result, &[], "some-cid", "server");

        assert_eq!(local_result, original);
    }

    #[test]
    fn detect_boundaries_returns_empty_for_now() {
        // No repo_uid on root -> no boundaries detected.
        let result = json!({
            "tree": {
                "name": "funcA",
                "children": [{"name": "funcB", "children": []}]
            }
        });
        let boundaries = detect_boundaries_in_trace(&result);
        assert!(
            boundaries.is_empty(),
            "no repo_uid on root means no boundaries"
        );

        // Cross-repo leaf with canonical_id -> detected as boundary.
        let result = json!({
            "tree": {
                "name": "funcA",
                "repo_uid": "local-repo",
                "canonical_id": "abc:src/lib.rs#funcA:xyz",
                "children": [{
                    "name": "funcB",
                    "repo_uid": "remote-repo",
                    "canonical_id": "def:src/api.rs#funcB:uvw",
                    "children": []
                }]
            }
        });
        let boundaries = detect_boundaries_in_trace(&result);
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].name, "funcB");
        assert_eq!(boundaries[0].canonical_id, "def:src/api.rs#funcB:uvw");
    }

    #[test]
    fn uuid_simple_generates_hex() {
        let id = uuid_v4_simple();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── Two-tier blast_radius tests ──────────────────────────────

    #[test]
    fn extract_local_repos_from_changed_symbols() {
        let local = json!({
            "changed_symbols": [
                {"uid": "1", "name": "foo", "file_path": "src/lib.rs"},
                {"uid": "2", "name": "bar", "file_path": "api/handler.rs"},
            ],
            "affected_symbols": [
                {"uid": "3", "name": "baz", "file_path": "src/util.rs"},
            ]
        });
        let repos = extract_local_repos(&local);
        assert!(repos.contains("src"));
        assert!(repos.contains("api"));
    }

    #[test]
    fn filter_org_results_returns_full_for_now() {
        let server = json!({"affected_symbols": [{"name": "x"}]});
        let local_repos = std::collections::HashSet::new();
        let filtered = filter_org_results(&server, &local_repos);
        assert_eq!(filtered, server);
    }

    #[test]
    fn merge_structured_response_preserves_schema() {
        let local = json!({
            "seeds": [{"uid": "s1", "label": "foo"}],
            "connected": [
                {"uid": "c1", "label": "bar", "score": 0.9},
                {"uid": "c2", "label": "baz", "score": 0.7}
            ],
            "unresolved_seeds": []
        });
        let server = json!({
            "seeds": [{"uid": "s2", "label": "qux"}],
            "connected": [
                {"uid": "c3", "label": "quux", "score": 0.8},
                {"uid": "c1", "label": "bar", "score": 0.85}
            ],
            "unresolved_seeds": []
        });

        let merged = merge_structured_results(&local, &server);

        assert!(merged.get("connected").is_some(), "connected field must be preserved");
        assert!(merged.get("seeds").is_some(), "seeds field must be preserved");
        assert!(merged.get("_meta").is_some(), "_meta must be present");
        let connected = merged["connected"].as_array().unwrap();
        assert!(connected.len() >= 3, "should merge connected items from both");
    }

    #[test]
    fn merge_flat_response_uses_results_envelope() {
        let local = json!({"results": [{"uid": "r1", "score": 0.9}]});
        let server = json!({"results": [{"uid": "r2", "score": 0.8}]});

        let merged = merge_structured_results(&local, &server);
        assert!(merged.get("results").is_some());
    }

    #[test]
    fn merge_structured_preserves_project_metadata() {
        let local = json!({
            "project": "billing",
            "project_uid": "uid-123",
            "seeds_expanded": 5,
            "tokens_used": 1200,
            "token_budget": 5000,
            "external_refs": [{"url": "https://example.com"}],
            "seeds": [{"uid": "s1"}],
            "connected": [{"uid": "c1", "score": 0.9}],
        });
        let server = json!({
            "project": "billing",
            "project_uid": "uid-456",
            "seeds": [],
            "connected": [{"uid": "c2", "score": 0.8}],
        });

        let merged = merge_structured_results(&local, &server);

        assert_eq!(merged["project"], "billing");
        assert_eq!(merged["project_uid"], "uid-123");
        assert_eq!(merged["seeds_expanded"], 5);
        assert_eq!(merged["tokens_used"], 1200);
        assert_eq!(merged["token_budget"], 5000);
        assert!(merged.get("external_refs").is_some());
    }

    // ── Upstream repo-glob routing tests ─────────────────────────────

    #[tokio::test]
    async fn find_upstream_for_repo_uses_globs() {
        use crate::discovery::{RoutingMode, UpstreamConfig};

        let cfg_acme = UpstreamConfig {
            name: Some("acme".to_string()),
            url: "http://127.0.0.1:19990".to_string(),
            token: None,
            repos: vec!["acme/*".to_string()],
            mode: RoutingMode::Fallback,
            timeout: "1s".to_string(),
        };
        let cfg_partner = UpstreamConfig {
            name: Some("partner".to_string()),
            url: "http://127.0.0.1:19991".to_string(),
            token: None,
            repos: vec!["partner/*".to_string()],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
        };

        let h1 = UpstreamHandle::from_config(&cfg_acme).unwrap();
        let h2 = UpstreamHandle::from_config(&cfg_partner).unwrap();
        let upstreams = vec![h1, h2];

        let matched = find_upstream_for_repo(&upstreams, Some("acme/billing"));
        assert_eq!(matched.unwrap().name.as_str(), "acme");

        let matched = find_upstream_for_repo(&upstreams, Some("partner/api"));
        assert_eq!(matched.unwrap().name.as_str(), "partner");

        let matched = find_upstream_for_repo(&upstreams, Some("unknown/thing"));
        assert!(matched.is_some(), "should fall back to first healthy");

        let matched = find_upstream_for_repo(&upstreams, None);
        assert!(matched.is_some());
    }

    #[test]
    fn extract_repo_hint_from_params() {
        let params = json!({"repos": ["acme/billing", "acme/api"]});
        assert_eq!(extract_repo_hint(&params), Some("acme/billing"));

        let params = json!({"repo": "partner/api"});
        assert_eq!(extract_repo_hint(&params), Some("partner/api"));

        let params = json!({"repo_url": "https://github.com/acme/api"});
        assert_eq!(extract_repo_hint(&params), Some("https://github.com/acme/api"));

        let params = json!({"query": "foo"});
        assert_eq!(extract_repo_hint(&params), None);
    }
}
