//! Hybrid client — wraps `DaemonClient` with optional upstream servers.
//!
//! When no upstreams are configured, this is a zero-cost wrapper around
//! `DaemonClient`. When upstreams exist, queries are routed based on the
//! upstream's routing mode:
//!
//! - **Fallback**: query local first; if results are sparse (< threshold) or
//!   the matching local repo is stale, query server as fallback.
//! - **Merge**: query both in parallel, merge via weighted RRF + scope-hash dedup.
//! - **Primary**: always query server; local only for uncommitted file overlay.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;

use nestweaver_federation::dispatch::{dispatch_json_rpc, dispatch_json_rpc_authed};
use nestweaver_federation::health::{
    MaintenanceProbe, code_is_down, effective_timeout, eject_with_cap, is_upstream_down,
    local_sha_for_server_repo,
};
use nestweaver_federation::results::{
    concat_fanout, count_results, inject_or_wrap_provenance, merge_structured_results,
    set_stale_repos,
};

use crate::DaemonClient;
use crate::discovery::{RoutingMode, discover_upstreams_with_config};
use crate::repo_identity::{normalized_repo_key, repo_name};
use crate::routing::{ToolRouting, tool_routing};
use crate::upstream::{ProbeOutcome, UpstreamHandle, now_ms};

// The portable coordinator logic (boundary detection, span stitching, result
// merging, two-tier composition, dispatch) moved to `nestweaver-federation`
// (nw-017 Phase B, 5a). Re-export the public pieces at their old paths so
// existing `nestweaver_client::hybrid::…` imports keep compiling.
pub use nestweaver_federation::trace::{
    TraceBoundary, detect_boundaries_in_trace, stitch_server_spans,
};

/// Provenance metadata injected into every hybrid response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProvenanceMeta {
    /// Which sources contributed to this response (e.g. ["local"], ["local", "server"]).
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

/// Fallback timeout used only when no upstream matches a query's repo hint.
/// The live per-query timeout is computed adaptively by [`effective_timeout`].
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(1);

/// How often the background maintenance task wakes to re-probe ejected
/// upstreams (active recovery) and refresh the staleness verdict. LAN default;
/// runs entirely off the query hot path, so a generous interval is fine.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(10);

/// Timeout for a single background health-probe RPC. Generous on purpose: the
/// probe is off the query hot path, so a slow probe only delays the next
/// verdict update, never a user query.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Hybrid client that routes queries to a local daemon and optional upstream
/// servers.
///
/// When no upstreams are configured, this is a zero-cost wrapper around
/// `DaemonClient`. When upstreams exist, queries are routed based on routing
/// mode (fallback/merge/primary).
pub struct HybridClient {
    local: DaemonClient,
    upstreams: Vec<UpstreamHandle>,
    /// Cached list of repo URLs where the local index is behind an upstream.
    /// Written by the background maintenance task (RFC 5861
    /// stale-while-revalidate: the freshness check lives OFF the query hot
    /// path). The query path only ever reads this — a non-blocking clone with
    /// ZERO upstream I/O — so upstream RTT can never inflate query latency or
    /// blank out `_meta.stale_repos` via a timed-out per-query probe.
    stale_verdict: Arc<Mutex<Vec<String>>>,
    /// Serializes the recount→eject decision in [`eject_with_cap`] so two
    /// concurrent failing queries can't both observe "under cap" and both
    /// eject, breaching [`nestweaver_federation::upstream::MAX_EJECTION_PERCENT`]. The correctness of the
    /// blast-radius guard must not rest on callers being single-threaded.
    ejection_guard: Arc<Mutex<()>>,
    /// Handle to the background maintenance task (active health recovery +
    /// staleness refresh). `None` until [`HybridClient::start_maintenance`] is
    /// called (only the long-lived MCP session needs it; one-shot CLI commands
    /// don't). Dropping it cancels the task.
    maintenance: Option<MaintenanceHandle>,
}

/// Owns the background maintenance task and cancels it on drop, tying the
/// task's lifetime to the `HybridClient` (and thus the MCP session).
struct MaintenanceHandle {
    /// Cancels the task's `CancellationToken` when dropped.
    _cancel: tokio_util::sync::DropGuard,
    _task: tokio::task::JoinHandle<()>,
}

impl HybridClient {
    /// Connect to the local daemon with auto-discovery of upstream servers.
    ///
    /// Upstream discovery walks `start_dir` upward looking for
    /// `.nestweaver/server.toml`, checks `~/.config/nestweaver/upstreams.toml`,
    /// honors the `NESTWEAVER_UPSTREAM` env var, and reads any `[[upstream]]`
    /// entries from the instance config file (`config_path`).
    pub async fn connect(
        db_path: &Path,
        config_path: Option<&Path>,
        start_dir: &Path,
    ) -> Result<Self> {
        let local = DaemonClient::connect(db_path, config_path).await?;
        let upstream_configs = discover_upstreams_with_config(start_dir, config_path);

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

        Ok(Self {
            local,
            upstreams,
            stale_verdict: Arc::new(Mutex::new(Vec::new())),
            ejection_guard: Arc::new(Mutex::new(())),
            maintenance: None,
        })
    }

    /// Create from an existing `DaemonClient` with no upstreams.
    pub fn local_only(client: DaemonClient) -> Self {
        Self {
            local: client,
            upstreams: vec![],
            stale_verdict: Arc::new(Mutex::new(Vec::new())),
            ejection_guard: Arc::new(Mutex::new(())),
            maintenance: None,
        }
    }

    /// Create from an existing `DaemonClient` with explicit upstreams.
    /// Useful for tests.
    pub fn from_parts(local: DaemonClient, upstreams: Vec<UpstreamHandle>) -> Self {
        Self {
            local,
            upstreams,
            stale_verdict: Arc::new(Mutex::new(Vec::new())),
            ejection_guard: Arc::new(Mutex::new(())),
            maintenance: None,
        }
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
        // Short-circuit only when no upstreams are *configured*. Previously
        // this checked `has_upstreams()` (which requires at least one to be
        // healthy), causing queries to silently go local-only when all
        // upstreams were temporarily unhealthy. Individual routing functions
        // already handle the "no healthy upstream" case with graceful fallback.
        if self.upstreams.is_empty() {
            let mut result = self.query_local(tool_name, params).await?;
            inject_or_wrap_provenance(&mut result, &["local"], &[]);
            return Ok(result);
        }

        let mut routing = tool_routing(tool_name);
        let mut fallback_mode = false;
        let repo_hint = extract_repo_hint(params);

        // Let the upstream's RoutingMode override the per-tool default.
        // LocalOnly and TwoTier are never overridden (they have tool-specific
        // semantics), but Merge/LocalFirst/ServerPreferred can be promoted.
        if routing != ToolRouting::LocalOnly
            && routing != ToolRouting::TwoTier
            && routing != ToolRouting::Combined
            && let Some(upstream) = find_upstream_for_repo(&self.upstreams, repo_hint)
        {
            match upstream.mode {
                RoutingMode::Primary => routing = ToolRouting::ServerPreferred,
                RoutingMode::Merge => routing = ToolRouting::Merge,
                RoutingMode::Fallback => {
                    routing = ToolRouting::LocalFirst;
                    fallback_mode = true;
                }
            }
        }

        // Populate `_meta.stale_repos` provenance from the background-computed
        // verdict (non-blocking, zero upstream I/O). Without this the field was
        // always empty even when the local graph was stale.
        let stale = self.current_stale_repos();
        let force_fallback_server = fallback_mode && query_targets_stale_repo(repo_hint, &stale);

        let mut result = self
            .route_query(routing, tool_name, params, force_fallback_server)
            .await?;

        if !stale.is_empty() {
            set_stale_repos(&mut result, &stale);
        }
        Ok(result)
    }

    /// Dispatch to the concrete routing strategy. Split out of [`query`] so the
    /// latter can apply cross-cutting provenance (staleness) to every path.
    async fn route_query(
        &mut self,
        routing: ToolRouting,
        tool_name: &str,
        params: &Value,
        force_fallback_server: bool,
    ) -> Result<Value> {
        match routing {
            ToolRouting::LocalOnly => {
                let mut result = self.query_local(tool_name, params).await?;
                inject_or_wrap_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
            ToolRouting::ServerPreferred => self.query_server_preferred(tool_name, params).await,
            ToolRouting::TwoTier => self.query_two_tier(tool_name, params).await,
            ToolRouting::Continuation => self.query_with_continuation(tool_name, params).await,
            ToolRouting::Combined => {
                // Combined tools return status/metadata objects — preserve shape,
                // don't flatten into { results: [...] }.
                let mut result = self.query_local(tool_name, params).await?;
                // Try to enrich with upstream status data (best-effort).
                if self.has_upstreams() {
                    let timeout = self.upstream_timeout(params);
                    if let Ok(server) = self.query_upstream(tool_name, params, timeout).await {
                        // Merge scalar/missing keys from server into local result.
                        if let (Some(local_obj), Some(server_obj)) =
                            (result.as_object_mut(), server.as_object())
                        {
                            for (k, v) in server_obj {
                                if k.starts_with('_') {
                                    continue;
                                }
                                if !local_obj.contains_key(k) {
                                    local_obj.insert(k.clone(), v.clone());
                                }
                            }
                        }
                        inject_or_wrap_provenance(&mut result, &["local", "server"], &[]);
                    } else {
                        inject_or_wrap_provenance(&mut result, &["local"], &[]);
                    }
                } else {
                    inject_or_wrap_provenance(&mut result, &["local"], &[]);
                }
                Ok(result)
            }
            ToolRouting::Merge => self.query_merge(tool_name, params).await,
            ToolRouting::FanOut => self.query_fanout(tool_name, params).await,
            ToolRouting::LocalFirst => {
                self.query_fallback_with_staleness(tool_name, params, force_fallback_server)
                    .await
            }
        }
    }

    /// Resolve the mode-aware adaptive timeout for the best-matching upstream.
    ///
    /// See [`effective_timeout`] for the clamp formula and rationale. Falls
    /// back to [`UPSTREAM_TIMEOUT`] only when no upstream matches the query's
    /// repo hint.
    fn upstream_timeout(&self, params: &Value) -> Duration {
        let repo_hint = extract_repo_hint(params);
        find_upstream_for_repo(&self.upstreams, repo_hint)
            .map(|u| effective_timeout(u.mode, u))
            .unwrap_or(UPSTREAM_TIMEOUT)
    }

    /// Server-preferred routing: query upstream first, fall back to local.
    async fn query_server_preferred(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        let timeout = self.upstream_timeout(params);
        match self.query_upstream(tool_name, params, timeout).await {
            Ok(mut r) => {
                inject_or_wrap_provenance(&mut r, &["server"], &[]);
                Ok(r)
            }
            Err(e) => {
                debug!(error = %e, tool = tool_name, "server-preferred: upstream failed, falling back to local");
                let mut r = self.query_local(tool_name, params).await?;
                inject_or_wrap_provenance(&mut r, &["local"], &[]);
                Ok(r)
            }
        }
    }

    /// Two-tier routing: local impact + org-wide impact from server.
    /// Delegates to `two_tier_query` for blast_radius/brain_impact/
    /// affected_tests tools.
    async fn query_two_tier(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        two_tier_query(self, tool_name, params).await.map_err(|e| {
            debug!(error = %e, tool = tool_name, "two-tier query failed");
            e
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
        self.query_fallback_with_staleness(tool_name, params, false)
            .await
    }

    /// Fallback routing with a stale-repo override from [`query`].
    async fn query_fallback_with_staleness(
        &mut self,
        tool_name: &str,
        params: &Value,
        force_server_for_stale_repo: bool,
    ) -> Result<Value> {
        // 1. Always query local first.
        let mut local_result = self.query_local(tool_name, params).await?;

        // 2. If no upstreams are configured, return local as-is.
        if self.upstreams.is_empty() {
            inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
            return Ok(local_result);
        }

        // 3. Check if local results are sufficient and fresh. When local
        // returns 0 results or the repo is stale, query the server.
        let local_count = count_results(&local_result);
        if !fallback_should_query_server(local_count, force_server_for_stale_repo) {
            debug!(
                tool = tool_name,
                local_count, "fallback: local results sufficient, skipping server"
            );
            inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
            return Ok(local_result);
        }

        // 4. Local results are sparse — query server with timeout.
        let timeout = self.upstream_timeout(params);
        debug!(
            tool = tool_name,
            local_count,
            threshold = FALLBACK_THRESHOLD,
            stale_repo = force_server_for_stale_repo,
            "fallback: local results sparse or stale, querying server"
        );
        match self.query_upstream(tool_name, params, timeout).await {
            Ok(server_result) => {
                // Use the structured merge so brain_context / project_context
                // responses keep their `connected` schema; it falls back to the
                // flat envelope for non-structured tools internally.
                let mut merged = merge_structured_results(&local_result, &server_result);
                inject_or_wrap_provenance(&mut merged, &["local", "server"], &[]);
                Ok(merged)
            }
            Err(e) => {
                warn!(
                    tool = tool_name,
                    local_count,
                    error = %e,
                    "fallback: upstream query failed, returning local-only results"
                );
                inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
                Ok(local_result)
            }
        }
    }

    /// Merge routing: query both local and server in parallel, merge via
    /// weighted RRF + scope-hash dedup.
    pub async fn query_merge(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        if self.upstreams.is_empty() {
            let mut result = self.query_local(tool_name, params).await?;
            inject_or_wrap_provenance(&mut result, &["local"], &[]);
            return Ok(result);
        }

        // Prepare the server future before borrowing self.local mutably.
        // Pick the first healthy upstream and clone its client (cheap channel clone).
        // Capture its *index* so that, on failure, we eject exactly the handle
        // this task queried — not whatever `find_upstream_for_repo` happens to
        // resolve to later (which can differ when several upstreams match the
        // repo or health state changes concurrently).
        let repo_hint = extract_repo_hint(params);
        let selected_idx = find_upstream_for_repo(&self.upstreams, repo_hint)
            .and_then(|u| self.upstreams.iter().position(|x| std::ptr::eq(x, u)));
        let server_task = selected_idx.map(|idx| {
            let u = &self.upstreams[idx];
            let timeout = effective_timeout(u.mode, u);
            let mut client = u.client();
            let token = u.auth_token().map(|t| t.to_string());
            let tool = tool_name.to_string();
            let p = params.clone();
            // Clone the EWMA cell — the future can't borrow `u` because the
            // local query borrows `self.local` mutably for `tokio::join!`.
            let latency = u.latency_ewma_ref();
            async move {
                let started = Instant::now();
                let res = tokio::time::timeout(
                    timeout,
                    dispatch_json_rpc_authed(&mut client, &tool, &p, token.as_deref()),
                )
                .await;
                if let Ok(Ok(_)) = &res {
                    crate::upstream::record_latency_into(&latency, started.elapsed());
                }
                res
            }
        });

        let Some(server_fut) = server_task else {
            let mut result = self.query_local(tool_name, params).await?;
            inject_or_wrap_provenance(&mut result, &["local"], &[]);
            return Ok(result);
        };

        // Now borrow self.local mutably for the local query.
        let local_fut = dispatch_json_rpc(self.local.inner_mut(), tool_name, params);

        let (local_result, server_result) = tokio::join!(local_fut, server_fut);
        let local = local_result?;

        match server_result {
            Ok(Ok(server)) => {
                let mut merged = merge_structured_results(&local, &server);
                inject_or_wrap_provenance(&mut merged, &["local", "server"], &[]);
                Ok(merged)
            }
            Ok(Err(e)) => {
                debug!(error = %e, "merge: server query failed, using local only");
                // Consistent with the primary/fallback path: only a genuine outage
                // passively ejects the upstream (subject to the cap) so the background
                // task can re-probe and recover it. A healthy server that rejects the
                // query stays in rotation. Eject the exact handle this task queried.
                if is_upstream_down(&e) {
                    if let Some(idx) = selected_idx {
                        eject_with_cap(
                            &self.upstreams[idx],
                            &self.upstreams,
                            &self.ejection_guard,
                            "merge query failed",
                        );
                    }
                } else {
                    warn!(
                        error = %e,
                        "merge: upstream rejected query (server healthy — not ejecting); check auth token / arguments"
                    );
                }
                let mut result = local;
                inject_or_wrap_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
            Err(_) => {
                debug!("merge: server query timed out, using local only");
                if let Some(idx) = selected_idx {
                    eject_with_cap(
                        &self.upstreams[idx],
                        &self.upstreams,
                        &self.ejection_guard,
                        "merge query timed out",
                    );
                }
                let mut result = local;
                inject_or_wrap_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
        }
    }

    /// FanOut routing (regex_search, count_patterns): query local + server and CONCATENATE
    /// the result rows. These are aggregate rows — regex matches / per-pattern counts, not
    /// symbols — so the symbol scope-hash RRF dedup used by Merge would wrongly drop or
    /// reorder them. Server failure degrades to local (query_upstream ejects on a real outage).
    async fn query_fanout(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        if self.upstreams.is_empty() {
            let mut result = self.query_local(tool_name, params).await?;
            inject_or_wrap_provenance(&mut result, &["local"], &[]);
            return Ok(result);
        }
        let timeout = self.upstream_timeout(params);
        let local = self.query_local(tool_name, params).await?;
        match self.query_upstream(tool_name, params, timeout).await {
            Ok(server) => {
                let mut merged = concat_fanout(&local, &server);
                inject_or_wrap_provenance(&mut merged, &["local", "server"], &[]);
                Ok(merged)
            }
            Err(_) => {
                let mut result = local;
                inject_or_wrap_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
        }
    }

    /// Return the current set of stale repos as computed by the background
    /// maintenance task. This is a NON-BLOCKING read of the shared verdict
    /// with ZERO upstream I/O — the freshness round-trip lives entirely off
    /// the query hot path (RFC 5861 stale-while-revalidate). Replaces the old
    /// per-query `RepoStates` probe that was bounded by a hostile 50ms timeout
    /// and so silently disabled staleness detection on any WAN upstream.
    ///
    /// The verdict is empty until the maintenance task's first background
    /// refresh completes (a few ms after `start_maintenance`), so an early
    /// query may report no stale repos — the next refresh corrects it.
    fn current_stale_repos(&self) -> Vec<String> {
        read_stale_verdict(&self.stale_verdict)
    }

    /// Compare local repo SHAs against each upstream's `RepoStates`.
    /// Returns repo URLs where the local index is behind the server.
    pub async fn check_staleness(&mut self) -> Vec<String> {
        // Delegates to the same off-hot-path comparison the background
        // maintenance task uses, so there is one implementation of the
        // local-vs-upstream SHA diff.
        let probes: Vec<MaintenanceProbe> = self
            .upstreams
            .iter()
            .map(|u| MaintenanceProbe {
                name: u.name.clone(),
                client: u.client(),
                token: u.auth_token().map(String::from),
                health: u.health_ref(),
            })
            .collect();
        let local = self.local.inner_mut();
        compute_stale_repos(local, &probes).await
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
                        if let Some(local_sha) =
                            local_sha_for_server_repo(&local_states, &server_repo.repo_url)
                            && local_sha != server_repo.indexed_sha.as_str()
                            && !server_repo.indexed_sha.is_empty()
                        {
                            status.stale_repos.push(server_repo.repo_url.clone());
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

    /// The set of `repo_uid`s the LOCAL daemon has indexed.
    ///
    /// Used by flow_trace boundary detection to tell a genuinely cross-repo
    /// leaf (foreign repo, not resolvable here) apart from a leaf that resolves
    /// into another repo the local index *also* knows about. The latter is
    /// followed locally, not stitched from the server. Returns an empty set on
    /// RPC failure, which preserves the prior (foreign-repo == boundary)
    /// behavior so detection degrades safely rather than dropping boundaries.
    async fn local_repo_uids(&mut self) -> std::collections::HashSet<String> {
        let req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
        match self.local.inner_mut().repo_states(req).await {
            Ok(resp) => resp
                .into_inner()
                .repos
                .into_iter()
                .map(|r| r.repo_uid)
                .filter(|uid| !uid.is_empty())
                .collect(),
            Err(_) => std::collections::HashSet::new(),
        }
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
        let started = Instant::now();
        match tokio::time::timeout(
            timeout,
            dispatch_json_rpc_authed(&mut client, tool_name, params, token.as_deref()),
        )
        .await
        {
            Ok(Ok(result)) => {
                upstream.record_latency(started.elapsed());
                Ok(result)
            }
            Ok(Err(e)) => {
                debug!(upstream = %upstream.name, error = %e, "upstream query failed");
                // Only eject on a genuine outage. A healthy server that rejects the query
                // (auth/bad-request/internal) must stay in rotation — ejecting it would
                // silently pull a working upstream for the cooldown.
                if is_upstream_down(&e) {
                    eject_with_cap(
                        upstream,
                        &self.upstreams,
                        &self.ejection_guard,
                        "query failed",
                    );
                } else {
                    warn!(
                        upstream = %upstream.name,
                        error = %e,
                        "upstream rejected query (server healthy — not ejecting); check auth token / arguments"
                    );
                }
                Err(e)
            }
            Err(_) => {
                debug!(
                    upstream = %upstream.name,
                    timeout_ms = timeout.as_millis() as u64,
                    "upstream query timed out"
                );
                eject_with_cap(
                    upstream,
                    &self.upstreams,
                    &self.ejection_guard,
                    "query timed out",
                );
                anyhow::bail!("upstream query timed out after {}ms", timeout.as_millis())
            }
        }
    }

    /// Start the background maintenance task (active health recovery of
    /// ejected upstreams). Idempotent and a no-op when no upstreams are
    /// configured. Must be called from within a Tokio runtime context; the
    /// task is cancelled when this `HybridClient` is dropped.
    ///
    /// Passive mark-down (on a failed live query) is only half of a correct
    /// health scheme — HAProxy's rule is that passive checks MUST be paired
    /// with active re-probing, or a downed upstream latches out forever. This
    /// task is that active half: it periodically re-probes ejected upstreams
    /// and restores them to rotation after `rise` consecutive successes.
    pub fn start_maintenance(&mut self) {
        if self.upstreams.is_empty() || self.maintenance.is_some() {
            return;
        }

        let probes: Vec<MaintenanceProbe> = self
            .upstreams
            .iter()
            .map(|u| MaintenanceProbe {
                name: u.name.clone(),
                client: u.client(),
                token: u.auth_token().map(String::from),
                health: u.health_ref(),
            })
            .collect();

        // The task computes the staleness verdict off the hot path and
        // publishes it into the shared cell the query path reads.
        let local = self.local.inner().clone();
        let stale_verdict = Arc::clone(&self.stale_verdict);

        let cancel = CancellationToken::new();
        let child = cancel.child_token();
        let task = tokio::spawn(maintenance_loop(probes, local, stale_verdict, child));
        self.maintenance = Some(MaintenanceHandle {
            _cancel: cancel.drop_guard(),
            _task: task,
        });
    }
}

/// Non-blocking read of the shared staleness verdict. Zero upstream I/O — this
/// is the only thing the query hot path does for staleness.
fn read_stale_verdict(verdict: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    verdict.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Background maintenance loop: wakes every [`MAINTENANCE_INTERVAL`] and, in a
/// single pass, (1) re-probes ejected upstreams for active recovery and
/// (2) refreshes the staleness verdict — both entirely off the query hot path.
/// Runs until cancelled.
async fn maintenance_loop(
    probes: Vec<MaintenanceProbe>,
    mut local: NestWeaverDaemonClient<Channel>,
    stale_verdict: Arc<Mutex<Vec<String>>>,
    cancel: CancellationToken,
) {
    // Populate the verdict once up front so `_meta.stale_repos` is meaningful
    // without waiting a full interval after startup.
    refresh_stale_verdict(&mut local, &probes, &stale_verdict).await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
        }
        run_recovery_tick(&probes).await;
        refresh_stale_verdict(&mut local, &probes, &stale_verdict).await;
    }
}

/// Recompute the staleness verdict and publish it into the shared cell the
/// query path reads. Bounded only by the upstreams' own RPC timeouts — this is
/// background work, so a slow WAN upstream delays the next verdict update but
/// never a user query.
async fn refresh_stale_verdict(
    local: &mut NestWeaverDaemonClient<Channel>,
    probes: &[MaintenanceProbe],
    stale_verdict: &Arc<Mutex<Vec<String>>>,
) {
    let stale = compute_stale_repos(local, probes).await;
    if let Ok(mut guard) = stale_verdict.lock() {
        *guard = stale;
    }
}

/// Compare the local daemon's indexed SHAs against each healthy upstream's
/// `RepoStates` and return the repo URLs where the local index is behind.
///
/// Fetches the LOCAL tier's repo states here (the client owns the local
/// daemon connection) and feeds them as data into the shared federation
/// comparison — the parameterized seam that keeps `nestweaver-federation`
/// free of any local-daemon coupling. On a local RPC failure this returns
/// empty without probing upstreams (staleness degrades to a false-negative
/// rather than blocking), matching the prior behavior.
async fn compute_stale_repos(
    local: &mut NestWeaverDaemonClient<Channel>,
    probes: &[MaintenanceProbe],
) -> Vec<String> {
    let local_states: std::collections::HashMap<String, String> = {
        let req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
        match local.repo_states(req).await {
            Ok(resp) => resp
                .into_inner()
                .repos
                .into_iter()
                .map(|r| (r.repo_url.clone(), r.indexed_sha))
                .collect(),
            Err(_) => return Vec::new(),
        }
    };
    nestweaver_federation::health::compute_stale_repos(&local_states, probes).await
}

/// Re-probe every ejected upstream whose backoff window has elapsed and fold
/// the result into its health state. Healthy upstreams are left alone — they
/// are marked down passively by failed live queries, not actively.
///
/// The tick decision (`probe_due`) and the result fold (`apply_probe_result`)
/// are pure functions on [`HealthState`], so the recovery behavior is unit
/// tested deterministically without any real sleeps (see `upstream.rs`).
async fn run_recovery_tick(probes: &[MaintenanceProbe]) {
    let now = now_ms();
    for p in probes {
        if !p.health.probe_due(now) {
            continue;
        }
        let ok = probe_upstream_health(&p.client, p.token.as_deref()).await;
        match p.health.apply_probe_result(now_ms(), ok) {
            ProbeOutcome::Recovered => {
                info!(upstream = %p.name, "upstream recovered — restored to rotation");
            }
            ProbeOutcome::Improving => {
                debug!(upstream = %p.name, "upstream probe succeeded — awaiting rise threshold");
            }
            ProbeOutcome::StillDown => {
                debug!(upstream = %p.name, "upstream probe failed — still ejected");
            }
        }
    }
}

/// Issue one `HealthCheck` RPC against an upstream, bounded by
/// [`HEALTH_PROBE_TIMEOUT`]. Returns whether the upstream answered healthily.
async fn probe_upstream_health(
    client: &NestWeaverDaemonClient<Channel>,
    token: Option<&str>,
) -> bool {
    let mut c = client.clone();
    let mut req = tonic::Request::new(nestweaver_proto::HealthCheckRequest {});
    if let Some(t) = token
        && let Ok(val) = format!("Bearer {t}").parse::<MetadataValue<_>>()
    {
        req.metadata_mut().insert("authorization", val);
    }
    matches!(
        tokio::time::timeout(HEALTH_PROBE_TIMEOUT, c.health_check(req)).await,
        Ok(Ok(_))
    )
}

/// Query a configured upstream directly when the local daemon is unavailable.
///
/// This keeps server-backed read commands useful on machines where the local
/// daemon cannot be started, while still refusing local-only tools whose
/// semantics require the local graph.
pub async fn query_configured_upstreams_only(
    config_path: Option<&Path>,
    start_dir: &Path,
    tool_name: &str,
    params: &Value,
) -> Result<Value> {
    if tool_routing(tool_name) == ToolRouting::LocalOnly {
        anyhow::bail!("{tool_name} requires the local daemon");
    }

    let upstreams = discover_upstreams_with_config(start_dir, config_path)
        .into_iter()
        .filter_map(|cfg| match UpstreamHandle::from_config(&cfg) {
            Ok(handle) => Some(handle),
            Err(e) => {
                warn!(
                    url = %cfg.url,
                    error = %e,
                    "failed to create upstream handle"
                );
                None
            }
        })
        .collect::<Vec<_>>();

    let repo_hint = extract_repo_hint(params);
    let upstream =
        find_upstream_for_repo(&upstreams, repo_hint).context("no healthy upstream servers")?;
    let mut client = upstream.client();
    let token = upstream.auth_token().map(|t| t.to_string());
    let timeout = effective_timeout(upstream.mode, upstream);
    let started = Instant::now();
    let mut result = tokio::time::timeout(
        timeout,
        dispatch_json_rpc_authed(&mut client, tool_name, params, token.as_deref()),
    )
    .await
    .with_context(|| format!("upstream query timed out after {}ms", timeout.as_millis()))??;
    upstream.record_latency(started.elapsed());
    inject_or_wrap_provenance(&mut result, &["server"], &[]);
    Ok(result)
}

// ── Upstream selection helpers ────────────────────────────────────────

/// Pick an upstream whose repo globs match `repo_hint`, falling back to the
/// first healthy upstream when no glob matches (or no hint is provided).
fn find_upstream_for_repo<'a>(
    upstreams: &'a [UpstreamHandle],
    repo_hint: Option<&str>,
) -> Option<&'a UpstreamHandle> {
    if let Some(repo) = repo_hint {
        let matched = upstreams
            .iter()
            .find(|u| u.is_healthy() && u.matches_repo(repo));
        if matched.is_some() {
            return matched;
        }
    }
    upstreams.iter().find(|u| u.is_healthy())
}

/// Extract a repo hint from query params — checks `repos[0]`, `repo`, `repo_url`.
fn extract_repo_hint(params: &Value) -> Option<&str> {
    params
        .get("repos")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .or_else(|| params.get("repo").and_then(|v| v.as_str()))
        .or_else(|| params.get("repo_url").and_then(|v| v.as_str()))
}

fn query_targets_stale_repo(repo_hint: Option<&str>, stale_repos: &[String]) -> bool {
    if stale_repos.is_empty() {
        return false;
    }
    let Some(repo_hint) = repo_hint else {
        return true;
    };
    stale_repos
        .iter()
        .any(|stale| repo_urls_equivalent(stale, repo_hint))
}

fn repo_urls_equivalent(a: &str, b: &str) -> bool {
    normalized_repo_key(a) == normalized_repo_key(b)
        || repo_name(a)
            .zip(repo_name(b))
            .is_some_and(|(a_name, b_name)| a_name.eq_ignore_ascii_case(&b_name))
}

fn fallback_should_query_server(local_count: usize, stale_repo: bool) -> bool {
    stale_repo || local_count < FALLBACK_THRESHOLD
}

// ── Flow trace stitching ────────────────────────────────────────────────

/// Execute a flow_trace with cross-boundary stitching.
///
/// This is the high-level function that:
/// 1. Runs flow_trace locally via the daemon
/// 2. If boundaries are detected and an upstream is available, sends
///    FlowTraceContinue RPCs
/// 3. Stitches server spans into the local result
///
/// Boundary detection is automatic: when `explicit_boundaries` is empty,
/// [`detect_boundaries_in_trace`] runs against the local result and finds
/// cross-repo edges from the `repo_uid` + `canonical_id` annotations the
/// flow_trace tool emits on detailed (non-concise) nodes. This is the
/// default path taken by `query_with_continuation`, which calls this with
/// no explicit boundaries. Concise traces omit those annotations and so
/// yield no auto-detected boundaries. Explicit boundaries are retained
/// only as an override (e.g., from a store-aware caller); when provided
/// they are used directly and auto-detection is skipped.
pub async fn flow_trace_with_stitching(
    client: &mut HybridClient,
    params: &Value,
    explicit_boundaries: &[TraceBoundary],
) -> Result<Value> {
    // 1. Run flow_trace locally.
    let mut local_result = client.query_local("flow_trace", params).await?;

    // 2. Detect boundaries (from JSON or explicit). Auto-detection needs the
    // set of repos the LOCAL index knows about so a leaf that resolves into
    // another local repo isn't mistaken for a server continuation boundary.
    let boundaries = if explicit_boundaries.is_empty() {
        let local_repos = client.local_repo_uids().await;
        detect_boundaries_in_trace(&local_result, &local_repos)
    } else {
        explicit_boundaries.to_vec()
    };

    if boundaries.is_empty() || !client.has_upstreams() {
        inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
        return Ok(local_result);
    }

    // 3. For each boundary, send FlowTraceContinue to the upstream.
    let max_depth = params
        .get("max_depth")
        .and_then(|v| v.as_i64())
        .unwrap_or(10) as i32;
    let trace_id = format!("trace-{}", trace_id());

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
            inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
            return Ok(local_result);
        }
    };

    let mut all_visited: Vec<String> = Vec::new();
    let server_name = upstream.name.clone();
    // Route cross-boundary continuation through the adaptive resolver rather
    // than the static configured timeout, mirroring the other upstream paths.
    let up_timeout = effective_timeout(upstream.mode, upstream);

    for boundary in &boundaries {
        let mut up_client = upstream.client();
        let mut req = tonic::Request::new(nestweaver_proto::FlowTraceContinueRequest {
            trace_id: trace_id.clone(),
            entry_canonical_id: boundary.canonical_id.clone(),
            parent_span_id: boundary.canonical_id.clone(),
            remaining_depth: max_depth
                .saturating_sub(boundary.parent_path.len() as i32)
                .max(1),
            visited_canonical_ids: all_visited.clone(),
        });
        upstream.inject_auth(&mut req);

        let started = Instant::now();
        match tokio::time::timeout(up_timeout, up_client.flow_trace_continue(req)).await {
            Ok(Ok(resp)) => {
                upstream.record_latency(started.elapsed());
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
                // A genuine outage here would otherwise never eject this upstream (this path
                // doesn't go through query_upstream), leaving a dead server "healthy" for any
                // client that only ever calls flow_trace. Eject on down and stop the loop —
                // the remaining boundaries would hit the same dead server.
                if code_is_down(e.code()) {
                    eject_with_cap(
                        upstream,
                        &client.upstreams,
                        &client.ejection_guard,
                        "flow_trace continue failed",
                    );
                    break;
                }
            }
            Err(_) => {
                debug!(
                    boundary = %boundary.canonical_id,
                    "FlowTraceContinue timed out"
                );
                eject_with_cap(
                    upstream,
                    &client.upstreams,
                    &client.ejection_guard,
                    "flow_trace continue timed out",
                );
                break;
            }
        }
    }

    inject_or_wrap_provenance(&mut local_result, &["local", &server_name], &[]);
    Ok(local_result)
}

fn trace_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let pid = std::process::id() as u64;
    let id: u128 = ((pid & 0xFFFF) as u128) << 112 | ((seq as u128) << 64) | (ts as u128);
    format!("{:032x}", id)
}

// ── Two-tier blast_radius ───────────────────────────────────────────────

/// Execute a two-tier impact query: local impact + org-wide impact.
///
/// Runs the tool against the LOCAL daemon here, then feeds the result into
/// the shared federation coordinator
/// ([`nestweaver_federation::two_tier::two_tier_query`]), which queries the
/// upstream for the org-wide tier and combines both into a response with
/// `local_impact` and `org_wide_impact` sections.
///
/// Used for blast_radius, brain_impact, and affected_tests.
pub async fn two_tier_query(
    client: &mut HybridClient,
    tool_name: &str,
    params: &Value,
) -> Result<Value> {
    // The LOCAL tier is computed here — the federation crate only ever sees
    // its result as data (the parameterized seam from nw-017 Phase B).
    let local_result = client.query_local(tool_name, params).await?;
    Ok(nestweaver_federation::two_tier::two_tier_query(
        local_result,
        &client.upstreams,
        &client.ejection_guard,
        tool_name,
        params,
    )
    .await)
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
            ca_cert: None,
        };
        let cfg2 = UpstreamConfig {
            name: Some("partner".to_string()),
            url: "http://127.0.0.1:19991".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
            ca_cert: None,
        };

        let h1 = UpstreamHandle::from_config(&cfg1).unwrap();
        let h2 = UpstreamHandle::from_config(&cfg2).unwrap();

        h2.mark_unhealthy();

        let upstreams = [h1, h2];
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

    #[test]
    fn staleness_verdict_read_without_per_query_network() {
        // The query path must read the staleness verdict from a shared cache
        // with ZERO upstream I/O — the freshness check belongs off the hot
        // path (RFC 5861 stale-while-revalidate). The background maintenance
        // task writes the verdict; the query path only reads it.
        let verdict = Arc::new(Mutex::new(vec!["github.com/acme/api".to_string()]));

        // Reading the verdict does no network work at all: even an upstream
        // with RTT far above the old 50ms per-query budget yields the
        // background-computed verdict, never an empty timed-out one.
        let read = read_stale_verdict(&verdict);
        assert_eq!(read, vec!["github.com/acme/api".to_string()]);

        // A background refresh replaces the verdict in place; the next read
        // sees it immediately without any per-query probe.
        *verdict.lock().unwrap() = vec!["github.com/acme/web".to_string()];
        assert_eq!(
            read_stale_verdict(&verdict),
            vec!["github.com/acme/web".to_string()]
        );
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

    #[test]
    fn fallback_queries_server_when_repo_is_stale_even_with_sufficient_local_results() {
        assert!(
            fallback_should_query_server(FALLBACK_THRESHOLD, true),
            "stale fallback repos must query upstream even when local has enough hits"
        );
        assert!(
            !fallback_should_query_server(FALLBACK_THRESHOLD, false),
            "fresh repos with enough local hits should keep the local fast path"
        );
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
            ca_cert: None,
        };
        let handle = UpstreamHandle::from_config(&cfg).unwrap();
        handle.mark_unhealthy();

        // has_upstreams should return false when all upstreams are unhealthy.
        let upstreams = [handle];
        let has_healthy = upstreams.iter().any(|u| u.is_healthy());
        assert!(!has_healthy);
    }

    #[tokio::test]
    async fn health_recovery_marks_upstream_healthy() {
        let cfg = crate::discovery::UpstreamConfig {
            name: Some("recoverable".to_string()),
            url: "http://127.0.0.1:19990".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Fallback,
            timeout: "1s".to_string(),
            ca_cert: None,
        };
        let handle = UpstreamHandle::from_config(&cfg).unwrap();
        let health = handle.health_ref();

        // A failed live query ejects the upstream (passive mark-down).
        handle.mark_unhealthy();
        assert!(!health.is_healthy());

        // The background recovery task drives probe ticks; two successes
        // (rise) restore it to rotation. No permanent latch.
        let now = health.ejected_until_ms();
        health.apply_probe_result(now, true);
        health.apply_probe_result(now, true);
        assert!(handle.is_healthy());
    }

    #[test]
    fn uuid_simple_generates_hex() {
        let id = trace_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
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
            ca_cert: None,
        };
        let cfg_partner = UpstreamConfig {
            name: Some("partner".to_string()),
            url: "http://127.0.0.1:19991".to_string(),
            token: None,
            repos: vec!["partner/*".to_string()],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
            ca_cert: None,
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
        assert_eq!(
            extract_repo_hint(&params),
            Some("https://github.com/acme/api")
        );

        let params = json!({"query": "foo"});
        assert_eq!(extract_repo_hint(&params), None);
    }

    // ── Routing mode override tests ──────────────────────────────────

    #[test]
    fn fallback_mode_overrides_merge_to_local_first() {
        use crate::routing::{ToolRouting, tool_routing};

        // brain_search defaults to Merge in the per-tool matrix
        assert_eq!(tool_routing("brain_search"), ToolRouting::Merge);
        assert_eq!(tool_routing("brain_context"), ToolRouting::Merge);
        assert_eq!(tool_routing("project_context"), ToolRouting::Merge);

        // But RoutingMode::Fallback should override to LocalFirst.
        // We can't test the full query() path without a live daemon, but we can
        // verify the override logic by simulating what query() does:
        let routing = tool_routing("brain_search");
        assert_eq!(routing, ToolRouting::Merge, "default should be Merge");

        // Simulate the override: Fallback => LocalFirst
        let overridden = match RoutingMode::Fallback {
            RoutingMode::Primary => ToolRouting::ServerPreferred,
            RoutingMode::Merge => ToolRouting::Merge,
            RoutingMode::Fallback => ToolRouting::LocalFirst,
        };
        assert_eq!(
            overridden,
            ToolRouting::LocalFirst,
            "Fallback must override to LocalFirst, not keep Merge"
        );
    }

    #[test]
    fn primary_mode_overrides_to_server_preferred() {
        let overridden = match RoutingMode::Primary {
            RoutingMode::Primary => ToolRouting::ServerPreferred,
            RoutingMode::Merge => ToolRouting::Merge,
            RoutingMode::Fallback => ToolRouting::LocalFirst,
        };
        assert_eq!(overridden, ToolRouting::ServerPreferred);
    }

    #[test]
    fn merge_mode_keeps_merge() {
        let overridden = match RoutingMode::Merge {
            RoutingMode::Primary => ToolRouting::ServerPreferred,
            RoutingMode::Merge => ToolRouting::Merge,
            RoutingMode::Fallback => ToolRouting::LocalFirst,
        };
        assert_eq!(overridden, ToolRouting::Merge);
    }
}
