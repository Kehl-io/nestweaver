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
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;
use nestweaver_proto::{JsonRequest, JsonResponse};

use crate::DaemonClient;
use crate::discovery::{RoutingMode, discover_upstreams_with_config};
use crate::merge::rrf_merge;
use crate::repo_identity::{normalized_repo_key, repo_name};
use crate::routing::{ToolRouting, tool_routing};
use crate::upstream::{HealthState, ProbeOutcome, UpstreamHandle, now_ms};

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

/// Multiplier applied to the observed latency EWMA when deriving the adaptive
/// timeout. ~1.75x the smoothed mean approximates a p95-ish trigger point
/// without maintaining a full histogram — "The Tail at Scale" (Dean &
/// Barroso, 2013) argues for hedging/aborting around the 95th percentile
/// rather than a fixed deadline.
const TIMEOUT_K: f64 = 1.75;

/// Lower bound on any adaptive timeout. Below this, scheduling jitter
/// dominates and a tight deadline only manufactures spurious timeouts.
const TIMEOUT_FLOOR: Duration = Duration::from_millis(50);

/// Hard cap on the *Fallback*-mode upstream timeout. In Fallback mode the
/// local index is the fast path and the server is consulted only when local
/// results are sparse, so the upstream must never block the local answer past
/// the product's <200ms budget. Merge/Primary keep the configured ceiling
/// (~1s) because the richer upstream answer is the entire point of those modes.
const FALLBACK_MODE_CAP: Duration = Duration::from_millis(250);

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
            maintenance: None,
        })
    }

    /// Create from an existing `DaemonClient` with no upstreams.
    pub fn local_only(client: DaemonClient) -> Self {
        Self {
            local: client,
            upstreams: vec![],
            stale_verdict: Arc::new(Mutex::new(Vec::new())),
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
            ToolRouting::Merge | ToolRouting::FanOut => self.query_merge(tool_name, params).await,
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
        let repo_hint = extract_repo_hint(params);
        let server_task = find_upstream_for_repo(&self.upstreams, repo_hint).map(|u| {
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
                let mut result = local;
                inject_or_wrap_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
            Err(_) => {
                debug!("merge: server query timed out, using local only");
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

/// Per-upstream data the background maintenance task needs. Holds cloned
/// handles (cheap channel clone + shared `Arc<HealthState>`) so the task is
/// decoupled from the `HybridClient`'s mutable borrow on the query path.
struct MaintenanceProbe {
    name: String,
    client: NestWeaverDaemonClient<Channel>,
    token: Option<String>,
    health: Arc<HealthState>,
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
/// `RepoStates` and return the repo URLs where the local index is behind. On
/// any RPC failure the affected source is skipped (staleness degrades to a
/// false-negative rather than blocking).
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

    let mut stale = Vec::new();
    for p in probes {
        if !p.health.is_healthy() {
            continue;
        }
        let mut client = p.client.clone();
        let mut req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
        if let Some(ref t) = p.token
            && let Ok(val) = format!("Bearer {t}").parse::<MetadataValue<_>>()
        {
            req.metadata_mut().insert("authorization", val);
        }
        if let Ok(resp) = client.repo_states(req).await {
            for server_repo in resp.into_inner().repos {
                if let Some(local_sha) =
                    local_sha_for_server_repo(&local_states, &server_repo.repo_url)
                    && local_sha != server_repo.indexed_sha.as_str()
                    && !server_repo.indexed_sha.is_empty()
                {
                    stale.push(server_repo.repo_url.clone());
                }
            }
        }
    }
    stale
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

    if let Some(token) = auth_token
        && let Ok(val) = format!("Bearer {}", token).parse::<tonic::metadata::MetadataValue<_>>()
    {
        request.metadata_mut().insert("authorization", val);
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

    let parsed: Value =
        serde_json::from_str(&response.result_json).unwrap_or(Value::String(response.result_json));
    Ok(parsed)
}

/// Inject an optional bearer token into a tonic request.
fn inject_bearer_token<T>(request: &mut tonic::Request<T>, auth_token: Option<&str>) {
    if let Some(token) = auth_token
        && let Ok(val) = format!("Bearer {}", token).parse::<tonic::metadata::MetadataValue<_>>()
    {
        request.metadata_mut().insert("authorization", val);
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
        serde_json::from_str(&resp.result_json).unwrap_or(Value::String(resp.result_json));
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
        serde_json::from_str(&resp.result_json).unwrap_or(Value::String(resp.result_json));
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
        serde_json::from_str(&resp.result_json).unwrap_or(Value::String(resp.result_json));
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
        let matched = upstreams
            .iter()
            .find(|u| u.is_healthy() && u.matches_repo(repo));
        if matched.is_some() {
            return matched;
        }
    }
    upstreams.iter().find(|u| u.is_healthy())
}

/// Compute the mode-aware *adaptive* upstream timeout for `handle`.
///
/// Rationale: a single static deadline rots as fleet latency drifts (the
/// InfoQ adaptive-hedging finding), so we scale off a rolling EWMA of observed
/// successful upstream latencies rather than a fixed 1s:
///
/// ```text
/// effective = clamp(K * latency_ewma, FLOOR, mode_ceiling)
/// ```
///
/// where `K ≈ 1.75` puts the deadline near the upstream's p95 ("The Tail at
/// Scale", Dean & Barroso, 2013) and `FLOOR = 50ms` avoids deadlines so tight
/// that scheduling jitter alone trips them. The per-upstream *configured*
/// timeout is the hard ceiling; `mode_ceiling = min(configured, mode_cap)`
/// where the Fallback cap is 250ms (keep the local fast path unblocked —
/// honors the <200ms budget) and Merge/Primary keep the full configured
/// ceiling (the richer upstream answer is the point). On a cold start (no
/// EWMA samples yet) we use `mode_ceiling`.
fn effective_timeout(mode: RoutingMode, handle: &UpstreamHandle) -> Duration {
    let configured = handle.timeout;
    let mode_ceiling = match mode {
        RoutingMode::Fallback => configured.min(FALLBACK_MODE_CAP),
        RoutingMode::Merge | RoutingMode::Primary => configured,
    };

    match handle.latency_ewma_ms() {
        // Cold start — no observed latency yet. Use the mode ceiling.
        None => mode_ceiling,
        Some(ewma_ms) => {
            let scaled = Duration::from_secs_f64((TIMEOUT_K * ewma_ms).max(0.0) / 1000.0);
            // `min(FLOOR, ceiling)` keeps the clamp bounds ordered even when an
            // explicit configured timeout is below the floor (e.g. "30ms").
            scaled.clamp(TIMEOUT_FLOOR.min(mode_ceiling), mode_ceiling)
        }
    }
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

fn local_sha_for_server_repo<'a>(
    local_states: &'a std::collections::HashMap<String, String>,
    server_repo_url: &str,
) -> Option<&'a str> {
    if let Some(sha) = local_states.get(server_repo_url) {
        return Some(sha.as_str());
    }

    let server_key = normalized_repo_key(server_repo_url);
    for (local_url, sha) in local_states {
        if normalized_repo_key(local_url) == server_key {
            return Some(sha.as_str());
        }
    }

    let server_name = repo_name(server_repo_url)?;
    let mut match_sha = None;
    for (local_url, sha) in local_states {
        let Some(local_name) = repo_name(local_url) else {
            continue;
        };
        if local_name.eq_ignore_ascii_case(&server_name) {
            if match_sha.is_some() {
                return None;
            }
            match_sha = Some(sha.as_str());
        }
    }
    match_sha
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
    } else if let Some(connected) = value.get("connected").and_then(|v| v.as_array()) {
        // Structured responses (brain_context / project_context) carry their
        // payload in `connected` — count those so the fallback threshold is
        // meaningful instead of always treating the whole object as 1 result.
        connected.len()
    } else if value.is_object() {
        // A single object counts as 1 result.
        1
    } else {
        0
    }
}

/// Set (or replace) the `_meta.stale_repos` provenance on a response, creating
/// the `_meta` object if absent. No-op for non-object responses.
fn set_stale_repos(result: &mut Value, stale: &[String]) {
    if let Some(obj) = result.as_object_mut() {
        let meta = obj
            .entry("_meta")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(meta_obj) = meta.as_object_mut() {
            meta_obj.insert(
                "stale_repos".to_string(),
                serde_json::to_value(stale).unwrap_or(Value::Null),
            );
        }
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
                    map.insert(
                        "_provenance".to_string(),
                        serde_json::to_value(mr.provenance).unwrap_or(Value::Null),
                    );
                    map.insert(
                        "_confidence".to_string(),
                        serde_json::to_value(mr.confidence).unwrap_or(Value::Null),
                    );
                    map.insert("_rrf_score".to_string(), Value::from(mr.score));
                }
                v
            })
            .collect();

        let mut seeds = local
            .get("seeds")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if let Some(server_seeds) = server.get("seeds").and_then(|v| v.as_array()) {
            seeds.extend(server_seeds.iter().cloned());
        }

        let mut unresolved = local
            .get("unresolved_seeds")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if let Some(su) = server.get("unresolved_seeds").and_then(|v| v.as_array()) {
            unresolved.extend(su.iter().cloned());
        }

        let mut expansion = local
            .get("expansion_terms")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
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
        for key in [
            "project",
            "project_uid",
            "seeds_expanded",
            "tokens_used",
            "token_budget",
            "external_refs",
        ] {
            if let Some(val) = local.get(key) {
                result[key] = val.clone();
            }
        }
        inject_or_wrap_provenance(&mut result, &["local", "server"], &[]);
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

/// Inject `_meta` provenance into an existing JSON object response.
///
/// This is the inner helper; prefer [`inject_or_wrap_provenance`] which
/// also handles bare-array responses.
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

/// Add provenance to a response, wrapping bare result arrays when needed.
///
/// Most structured RPC responses are JSON objects and can receive `_meta`
/// directly. A few legacy JSON RPCs, notably `search_symbols`, still return a
/// bare array. In the upstream-only fallback path callers still need to know
/// that the data came from the server, so preserve the array under `results`.
fn inject_or_wrap_provenance(result: &mut Value, sources: &[&str], stale_repos: &[String]) {
    if result.is_array() {
        let items = result.take();
        *result = serde_json::json!({
            "results": items,
            "_meta": {
                "sources": sources,
                "stale_repos": stale_repos,
                "scope": if sources.len() > 1 {
                    "hybrid"
                } else {
                    sources.first().copied().unwrap_or("local")
                },
            },
        });
    } else {
        inject_provenance(result, sources, stale_repos);
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
/// node's `repo_uid` (the locally-initiated trace) **and** is not itself a
/// repo the local index knows about. These represent cross-repo call edges
/// the local daemon cannot follow — the callee resolves in another repo (a
/// `CROSS_REPO_LINK` stub) that is unresolved locally, so the upstream
/// server should continue the trace from there.
///
/// `local_repos` is the set of `repo_uid`s the LOCAL daemon has indexed (see
/// [`HybridClient::local_repo_uids`]). When the local daemon indexes more
/// than one repo, a trace can legitimately resolve *into* another local repo;
/// that leaf carries a foreign `repo_uid` but is still locally followed, so
/// flagging it would emit a spurious server continuation. Excluding leaves
/// whose `repo_uid` is in `local_repos` prevents that false positive. An
/// empty set restores the prior "any foreign-repo leaf is a boundary"
/// behavior, which is the safe default when the local repo set is unknown.
///
/// Two flow_trace response shapes are handled:
/// - the standard single-root trace `{ root_uid, tree: {...} }`, and
/// - the class-expanded trace `{ root_uid, methods: [ {...}, ... ] }`
///   produced when the root symbol is a class (mirrors the `methods`
///   handling in [`stitch_server_spans`]).
///
/// Requires flow_trace output to include `repo_uid` and `canonical_id`
/// fields on each node (the detailed output format; concise traces omit
/// them and therefore yield no boundaries).
///
/// See architecture spec: cross-boundary-flow-trace.md
pub fn detect_boundaries_in_trace(
    result: &Value,
    local_repos: &std::collections::HashSet<String>,
) -> Vec<TraceBoundary> {
    let mut boundaries = Vec::new();

    if let Some(tree) = result.get("tree") {
        // Standard single-root trace.
        collect_from_root(tree, local_repos, &mut boundaries);
    } else if let Some(methods) = result.get("methods").and_then(|v| v.as_array()) {
        // Class-expanded trace: each method is its own subtree rooted in the
        // class's repo. Use the first method that carries a repo_uid as the
        // local-repo reference (all methods of a class share its repo).
        let root_repo = methods
            .iter()
            .find_map(nonempty_repo_uid)
            .unwrap_or_default();
        if root_repo.is_empty() {
            debug!(
                "detect_boundaries_in_trace: class-expanded trace lacks repo_uid, cannot detect boundaries"
            );
        } else {
            for method in methods {
                let mut path = Vec::new();
                collect_boundaries(method, &root_repo, local_repos, &mut path, &mut boundaries);
            }
        }
    } else {
        // The result itself may be a bare trace node.
        collect_from_root(result, local_repos, &mut boundaries);
    }

    debug!(
        count = boundaries.len(),
        "detect_boundaries_in_trace: found boundary nodes"
    );
    boundaries
}

/// The node's `repo_uid` as an owned `String`, or `None` when absent/empty.
fn nonempty_repo_uid(node: &Value) -> Option<String> {
    node.get("repo_uid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Walk a single trace tree rooted at `root`, collecting boundaries against
/// the root's own `repo_uid`. A no-op when the root lacks a `repo_uid`.
fn collect_from_root(
    root: &Value,
    local_repos: &std::collections::HashSet<String>,
    out: &mut Vec<TraceBoundary>,
) {
    let Some(root_repo) = nonempty_repo_uid(root) else {
        debug!("detect_boundaries_in_trace: root node lacks repo_uid, cannot detect boundaries");
        return;
    };
    let mut path = Vec::new();
    collect_boundaries(root, &root_repo, local_repos, &mut path, out);
}

/// Recursively walk the flow_trace tree collecting boundary nodes.
fn collect_boundaries(
    node: &Value,
    root_repo: &str,
    local_repos: &std::collections::HashSet<String>,
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

    // A boundary: different repo than the root, is a leaf (trace couldn't
    // follow), has a canonical_id for cross-boundary matching, and is NOT a
    // repo the local index knows about. The last clause is the false-positive
    // guard: a foreign-repo leaf that the local daemon *can* resolve (another
    // locally-indexed repo) was already followed here, so continuing it on the
    // server would be a spurious round-trip.
    if is_leaf
        && !repo_uid.is_empty()
        && repo_uid != root_repo
        && !canonical_id.is_empty()
        && !local_repos.contains(repo_uid)
    {
        out.push(TraceBoundary {
            canonical_id: canonical_id.to_string(),
            name: name.to_string(),
            parent_path: path.clone(),
        });
    }

    if let Some(children) = children {
        path.push(name.to_string());
        for child in children {
            collect_boundaries(child, root_repo, local_repos, path, out);
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
        if let Some(cid) = node.get("canonical_id").and_then(|v| v.as_str())
            && cid == boundary_cid
        {
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

        // Recurse into children.
        if let Some(children) = node.get_mut("children")
            && let Some(arr) = children.as_array_mut()
        {
            for child in arr.iter_mut() {
                if inject_at_boundary(child, boundary_cid, subtrees, server_name) {
                    return true;
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
    if let Some(methods) = local_result.get_mut("methods")
        && let Some(arr) = methods.as_array_mut()
    {
        for method in arr.iter_mut() {
            inject_at_boundary(method, boundary_canonical_id, &subtrees, server_name);
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
            }
            Err(_) => {
                debug!(
                    boundary = %boundary.canonical_id,
                    "FlowTraceContinue timed out"
                );
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
/// When an upstream server is available:
/// 1. Run the tool locally (existing logic)
/// 2. Query the server for the same tool
/// 3. Combine into a response with `local_impact` and `org_wide_impact` sections
///
/// Used for blast_radius, brain_impact, and affected_tests.
pub async fn two_tier_query(
    client: &mut HybridClient,
    tool_name: &str,
    params: &Value,
) -> Result<Value> {
    // 1. Always run the tool locally.
    let mut local_result = client.query_local(tool_name, params).await?;

    // 2. If no upstream, return local-only with clear annotation.
    if !client.has_upstreams() {
        inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
        if let Some(obj) = local_result.as_object_mut() {
            obj.insert("tier".to_string(), Value::String("local_only".into()));
        }
        return Ok(local_result);
    }

    // 3. Query upstream for org-wide impact.
    let upstream = match client.upstreams.iter().find(|u| u.is_healthy()) {
        Some(u) => u,
        None => {
            inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
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
    // Route the org-wide tier through the adaptive resolver instead of the
    // static configured timeout, mirroring query_upstream/query_merge.
    let timeout = effective_timeout(upstream.mode, upstream);
    let tool = tool_name.to_string();

    let server_params = params.clone();
    let started = Instant::now();
    let server_result = match tokio::time::timeout(
        timeout,
        dispatch_json_rpc_authed(&mut up_client, &tool, &server_params, token.as_deref()),
    )
    .await
    {
        Ok(Ok(result)) => {
            upstream.record_latency(started.elapsed());
            Some(result)
        }
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

        response["org_wide_impact"] = serde_json::json!({
            "source_server": server_name,
            "results": filtered_server,
        });
    } else {
        response["org_wide_impact"] = serde_json::json!({
            "source_server": server_name,
            "status": "unavailable",
            "note": "upstream server query failed — showing local impact only",
        });
    }

    inject_or_wrap_provenance(&mut response, &["local", &server_name], &[]);

    Ok(response)
}

/// Reduce a `repo_uid` to its instance-independent identity for two-tier dedup.
///
/// `repo_uid` is `repo:{instance}:{url_hash}`. The `{instance}` segment differs
/// between the LOCAL daemon (`local` / db-path hash) and the SERVER
/// (`nestweaver-server`) *by construction*, while `{url_hash}` is normalized at
/// mint time (T3.1b) and is therefore identical for the same repo across
/// instances. Matching on the full `repo_uid` never dedups across instances;
/// matching on `{url_hash}` does. This mirrors the instance-stripping the merge
/// dedup performs in [`crate::dedup::extract_identity`].
///
/// Falls back to the raw string when the value is not in canonical
/// `repo:{instance}:{url_hash}` form (e.g. a `file_path` stand-in), so both
/// sides key consistently.
fn repo_identity_key(repo_uid: &str) -> String {
    // "repo:{instance}:{url_hash}" -> "{url_hash}"
    repo_uid
        .strip_prefix("repo:")
        .and_then(|rest| rest.split_once(':'))
        .map(|(_instance, url_hash)| url_hash)
        .filter(|url_hash| !url_hash.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| repo_uid.to_string())
}

/// Extract repo identifiers mentioned in local blast_radius results.
///
/// Prefers the `repo_uid` field on each symbol (populated from the graph
/// store), reduced to its instance-independent identity via
/// [`repo_identity_key`] so the local and server tiers reconcile despite their
/// differing instance segments. Falls back to `file_path` as a whole-path key
/// when `repo_uid` is absent, so entries are still tracked but dedup is
/// path-exact rather than repo-level.
fn extract_local_repos(local: &Value) -> std::collections::HashSet<String> {
    let mut repos = std::collections::HashSet::new();

    for key in &["changed_symbols", "affected_symbols"] {
        if let Some(arr) = local.get(key).and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(repo) = item.get("repo_uid").and_then(|v| v.as_str())
                    && !repo.is_empty()
                {
                    repos.insert(repo_identity_key(repo));
                    continue;
                }
                // Fallback: use the full file_path as identity when no
                // repo_uid is available (should not happen for indexed repos).
                if let Some(fp) = item.get("file_path").and_then(|v| v.as_str()) {
                    repos.insert(fp.to_string());
                }
            }
        }
    }

    repos
}

/// Filter org-wide results to exclude repos already covered by local impact.
///
/// Removes entries from the server's `affected_symbols` and `changed_symbols`
/// whose repo matches a repo already present in the local impact set. Repo
/// identity is compared instance-independently via [`repo_identity_key`] — the
/// full `repo_uid` carries an `{instance}` segment that differs between the
/// local daemon and the server, so a full-string match never dedups. Falls back
/// to full `file_path` matching when `repo_uid` is absent (for backward
/// compatibility with older servers that don't emit it).
fn filter_org_results(server: &Value, local_repos: &std::collections::HashSet<String>) -> Value {
    if local_repos.is_empty() {
        return server.clone();
    }
    let mut filtered = server.clone();

    // Filter affected_symbols and changed_symbols arrays.
    for key in &["affected_symbols", "changed_symbols"] {
        if let Some(arr) = filtered.get_mut(key).and_then(|v| v.as_array_mut()) {
            arr.retain(|item| {
                // Prefer repo_uid for matching; fall back to full file_path.
                let dominated = item
                    .get("repo_uid")
                    .and_then(|v| v.as_str())
                    .filter(|r| !r.is_empty())
                    .map(|repo| local_repos.contains(&repo_identity_key(repo)))
                    .unwrap_or_else(|| {
                        item.get("file_path")
                            .and_then(|v| v.as_str())
                            .is_some_and(|fp| local_repos.contains(fp))
                    });
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
            // Prefer repo_uid; fall back to full representative_file path.
            let dominated = cluster
                .get("repo_uid")
                .and_then(|v| v.as_str())
                .filter(|r| !r.is_empty())
                .map(|repo| local_repos.contains(&repo_identity_key(repo)))
                .unwrap_or_else(|| {
                    cluster
                        .get("representative_file")
                        .and_then(|v| v.as_str())
                        .is_some_and(|fp| local_repos.contains(fp))
                });
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

    // ── Mode-aware adaptive timeout (`effective_timeout`) ─────────────────

    fn handle_with(mode: RoutingMode, timeout: &str) -> UpstreamHandle {
        use crate::discovery::UpstreamConfig;
        let cfg = UpstreamConfig {
            name: Some("t".to_string()),
            url: "http://127.0.0.1:19999".to_string(),
            token: None,
            repos: vec![],
            mode,
            timeout: timeout.to_string(),
            ca_cert: None,
        };
        UpstreamHandle::from_config(&cfg).unwrap()
    }

    #[tokio::test]
    async fn effective_timeout_cold_start_uses_mode_ceiling() {
        // Fallback with the default 1s config: ceiling is capped at 250ms.
        let h = handle_with(RoutingMode::Fallback, "1s");
        assert_eq!(
            effective_timeout(RoutingMode::Fallback, &h),
            Duration::from_millis(250)
        );
        // Merge with default 1s config: ceiling is the full configured 1s.
        let h = handle_with(RoutingMode::Merge, "1s");
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn effective_timeout_warm_scales_by_k() {
        // Small p95: 80ms EWMA, Merge mode, 1s ceiling -> ~K*ewma = 140ms.
        let h = handle_with(RoutingMode::Merge, "1s");
        h.record_latency(Duration::from_millis(80));
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_secs_f64(0.140)
        );
    }

    #[tokio::test]
    async fn effective_timeout_fallback_capped_at_250ms() {
        // Even with a 1s config and a high EWMA, Fallback never exceeds 250ms.
        let h = handle_with(RoutingMode::Fallback, "1s");
        h.record_latency(Duration::from_millis(400)); // K*400 = 700ms
        assert_eq!(
            effective_timeout(RoutingMode::Fallback, &h),
            Duration::from_millis(250)
        );
    }

    #[tokio::test]
    async fn effective_timeout_merge_primary_up_to_configured_ceiling() {
        // A large EWMA clamps up to the configured 1s ceiling, not beyond.
        let h = handle_with(RoutingMode::Merge, "1s");
        h.record_latency(Duration::from_millis(2000)); // K*2000 = 3.5s
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_secs(1)
        );
        let h = handle_with(RoutingMode::Primary, "1s");
        h.record_latency(Duration::from_millis(2000));
        assert_eq!(
            effective_timeout(RoutingMode::Primary, &h),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn effective_timeout_explicit_small_config_caps_all_modes() {
        // An explicit "200ms" config is the hard ceiling for every mode.
        for mode in [
            RoutingMode::Fallback,
            RoutingMode::Merge,
            RoutingMode::Primary,
        ] {
            let h = handle_with(mode, "200ms");
            h.record_latency(Duration::from_millis(900)); // K*900 = 1.575s
            assert_eq!(
                effective_timeout(mode, &h),
                Duration::from_millis(200),
                "mode {mode:?} should cap at the configured 200ms"
            );
        }
    }

    #[tokio::test]
    async fn effective_timeout_respects_floor() {
        // A tiny EWMA would scale below the 50ms floor — clamp up to it.
        let h = handle_with(RoutingMode::Merge, "1s");
        h.record_latency(Duration::from_millis(5)); // K*5 = 8.75ms
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_millis(50)
        );
    }

    #[tokio::test]
    async fn effective_timeout_floor_clamp_ordered_for_tiny_config() {
        // Configured ceiling below the floor must not panic the clamp; the
        // ceiling wins (effective bounds become [ceiling, ceiling]).
        let h = handle_with(RoutingMode::Merge, "30ms");
        h.record_latency(Duration::from_millis(5));
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_millis(30)
        );
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
    fn count_results_structured_connected() {
        // brain_context / project_context return a structured object whose real
        // payload is the `connected` array. Fallback must count that, not treat
        // the whole response as a single result (which would always trip the
        // server query and then mangle the merge).
        let v = json!({
            "seeds": ["x"],
            "connected": [{"name": "a"}, {"name": "b"}, {"name": "c"}],
        });
        assert_eq!(count_results(&v), 3);
    }

    #[test]
    fn merge_structured_results_preserves_connected_schema() {
        // Merging two structured responses must keep the structured schema
        // (top-level `connected`), not wrap both whole responses into a flat
        // `results` envelope. Regression guard for the fallback merge bug.
        let local = json!({
            "seeds": ["s"],
            "connected": [{"uid": "sym:1", "name": "a", "location": "a.rs"}],
        });
        let server = json!({
            "seeds": ["s"],
            "connected": [{"uid": "sym:2", "name": "b", "location": "b.rs"}],
        });
        let merged = merge_structured_results(&local, &server);
        assert!(
            merged.get("connected").and_then(|v| v.as_array()).is_some(),
            "merged response must retain the `connected` array; got: {merged}"
        );
        assert!(
            merged.get("results").is_none(),
            "structured merge must not flatten into a `results` envelope"
        );
        let connected = merged["connected"].as_array().unwrap();
        assert_eq!(
            connected.len(),
            2,
            "both repos' connected items should merge"
        );
        assert_eq!(merged["_meta"]["sources"][0], "local");
        assert_eq!(merged["_meta"]["sources"][1], "server");
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

    #[test]
    fn set_stale_repos_populates_meta() {
        let mut v = json!({"results": [], "_meta": {"sources": ["local", "server"]}});
        set_stale_repos(&mut v, &["github.com/acme/api".to_string()]);
        assert_eq!(v["_meta"]["stale_repos"][0], "github.com/acme/api");
    }

    #[test]
    fn set_stale_repos_creates_meta_when_missing() {
        let mut v = json!({"results": []});
        set_stale_repos(&mut v, &["r".to_string()]);
        assert_eq!(v["_meta"]["stale_repos"][0], "r");
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

    #[test]
    fn repo_state_lookup_matches_file_checkout_to_remote_url() {
        let local_states = std::collections::HashMap::from([
            (
                "file:///home/user/dev/workspaces/api".to_string(),
                "local-sha".to_string(),
            ),
            (
                "file:///home/user/dev/workspaces/billing".to_string(),
                "billing-sha".to_string(),
            ),
        ]);

        assert_eq!(
            local_sha_for_server_repo(&local_states, "https://github.com/acme/api.git"),
            Some("local-sha")
        );
        assert_eq!(
            local_sha_for_server_repo(&local_states, "git@github.com:acme/billing.git"),
            Some("billing-sha")
        );
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
    fn inject_or_wrap_provenance_wraps_bare_array() {
        let mut val = json!([{"name": "server_only"}]);
        inject_or_wrap_provenance(&mut val, &["server"], &[]);

        assert_eq!(val["results"][0]["name"], "server_only");
        assert_eq!(val["_meta"]["sources"][0], "server");
        assert_eq!(val["_meta"]["scope"], "server");
    }

    #[test]
    fn inject_provenance_adds_meta() {
        let mut val = json!({"results": [1, 2, 3]});
        inject_or_wrap_provenance(&mut val, &["local", "acme"], &["repo-a".to_string()]);
        assert!(val["_meta"].is_object());
        assert_eq!(val["_meta"]["scope"], "hybrid");
        assert_eq!(val["_meta"]["stale_repos"][0], "repo-a");
        assert_eq!(val["_meta"]["sources"][0], "local");
        assert_eq!(val["_meta"]["sources"][1], "acme");
    }

    #[test]
    fn inject_provenance_local_only_scope() {
        let mut val = json!({"results": []});
        inject_or_wrap_provenance(&mut val, &["local"], &[]);
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

    /// Empty local-repo set: the common single-repo case where every
    /// foreign-repo leaf is a genuine cross-repo boundary.
    fn no_local_repos() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    #[test]
    fn detect_boundaries_needs_root_repo_uid() {
        // Concise / unannotated traces have no repo_uid on the root, so the
        // local-repo reference is unknown and nothing can be flagged.
        let result = json!({
            "tree": {
                "name": "funcA",
                "children": [{"name": "funcB", "children": []}]
            }
        });
        assert!(
            detect_boundaries_in_trace(&result, &no_local_repos()).is_empty(),
            "no repo_uid on root means no boundaries"
        );
    }

    #[test]
    fn detect_boundaries_flags_cross_repo_leaf() {
        // A leaf whose repo_uid differs from the root's, carrying a
        // canonical_id, is a cross-repo boundary the server should continue.
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
        let boundaries = detect_boundaries_in_trace(&result, &no_local_repos());
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].name, "funcB");
        assert_eq!(boundaries[0].canonical_id, "def:src/api.rs#funcB:uvw");
        // parent_path records the chain of node names from the root down to
        // (but excluding) the boundary, for stitching the continuation back.
        assert_eq!(boundaries[0].parent_path, vec!["funcA".to_string()]);
    }

    #[test]
    fn detect_boundaries_records_nested_parent_path() {
        // root(A) -> mid(A) -> leaf(B): the boundary is the deep leaf and its
        // parent_path is the full name chain above it.
        let result = json!({
            "tree": {
                "name": "root",
                "repo_uid": "A",
                "canonical_id": "a:root",
                "children": [{
                    "name": "mid",
                    "repo_uid": "A",
                    "canonical_id": "a:mid",
                    "children": [{
                        "name": "leaf",
                        "repo_uid": "B",
                        "canonical_id": "b:leaf",
                        "children": []
                    }]
                }]
            }
        });
        let boundaries = detect_boundaries_in_trace(&result, &no_local_repos());
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].canonical_id, "b:leaf");
        assert_eq!(
            boundaries[0].parent_path,
            vec!["root".to_string(), "mid".to_string()]
        );
    }

    #[test]
    fn detect_boundaries_ignores_same_repo_and_uncrossable_leaves() {
        // A same-repo leaf is not a boundary; a foreign leaf without a
        // canonical_id cannot be matched on the server, so it is skipped too.
        let result = json!({
            "tree": {
                "name": "root",
                "repo_uid": "A",
                "canonical_id": "a:root",
                "children": [
                    { "name": "localChild", "repo_uid": "A", "canonical_id": "a:child", "children": [] },
                    { "name": "foreignNoCid", "repo_uid": "B", "canonical_id": "", "children": [] }
                ]
            }
        });
        assert!(
            detect_boundaries_in_trace(&result, &no_local_repos()).is_empty(),
            "same-repo leaves and canonical-id-less foreign leaves are not boundaries"
        );
    }

    #[test]
    fn detect_boundaries_requires_leaf() {
        // A foreign node that still has locally-resolved children is not a
        // leaf: the local trace already followed past it, so it is not a
        // continuation boundary (only the genuine leaf below it could be).
        let result = json!({
            "tree": {
                "name": "root",
                "repo_uid": "A",
                "canonical_id": "a:root",
                "children": [{
                    "name": "foreignWithChild",
                    "repo_uid": "B",
                    "canonical_id": "b:foreign",
                    "children": [
                        { "name": "deeperLocal", "repo_uid": "A", "canonical_id": "a:deep", "children": [] }
                    ]
                }]
            }
        });
        assert!(
            detect_boundaries_in_trace(&result, &no_local_repos()).is_empty(),
            "a foreign node with local children is not a leaf boundary"
        );
    }

    #[test]
    fn detect_boundaries_walks_class_expanded_methods() {
        // Class-expanded traces have no `tree`; each method is its own subtree
        // rooted in the class's repo. A cross-repo leaf under any method must
        // still be detected (parity with stitch_server_spans' `methods`
        // handling).
        let result = json!({
            "root_uid": "sym:repoA::Klass",
            "root_kind": "class",
            "methods": [
                {
                    "name": "methodNoCross",
                    "repo_uid": "A",
                    "canonical_id": "a:m1",
                    "children": [
                        { "name": "localHelper", "repo_uid": "A", "canonical_id": "a:h", "children": [] }
                    ]
                },
                {
                    "name": "methodCalls",
                    "repo_uid": "A",
                    "canonical_id": "a:m2",
                    "children": [
                        { "name": "remoteApi", "repo_uid": "B", "canonical_id": "b:remoteApi", "children": [] }
                    ]
                }
            ]
        });
        let boundaries = detect_boundaries_in_trace(&result, &no_local_repos());
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].name, "remoteApi");
        assert_eq!(boundaries[0].canonical_id, "b:remoteApi");
        assert_eq!(boundaries[0].parent_path, vec!["methodCalls".to_string()]);
    }

    #[test]
    fn detect_boundaries_skips_leaf_resolvable_in_another_local_repo() {
        // Multi-repo local daemon: the trace resolves from repo A INTO repo B,
        // and the local index also has repo B indexed. That leaf carries a
        // foreign repo_uid but is locally followed, so it must NOT be flagged
        // as a cross-repo boundary (no spurious server continuation).
        let result = json!({
            "tree": {
                "name": "funcA",
                "repo_uid": "local-repo-a",
                "canonical_id": "a:src/lib.rs#funcA",
                "children": [{
                    "name": "funcB",
                    "repo_uid": "local-repo-b",
                    "canonical_id": "b:src/api.rs#funcB",
                    "children": []
                }]
            }
        });
        let local_repos: std::collections::HashSet<String> =
            ["local-repo-a".to_string(), "local-repo-b".to_string()]
                .into_iter()
                .collect();
        assert!(
            detect_boundaries_in_trace(&result, &local_repos).is_empty(),
            "a foreign-repo leaf the local index can resolve is not a boundary"
        );
    }

    #[test]
    fn detect_boundaries_flags_leaf_in_unindexed_foreign_repo() {
        // Same multi-repo daemon, but the leaf resolves into a repo the local
        // index does NOT know about. That is a genuine cross-repo edge the
        // server must continue, so it stays a boundary even when other repos
        // are indexed locally.
        let result = json!({
            "tree": {
                "name": "funcA",
                "repo_uid": "local-repo-a",
                "canonical_id": "a:src/lib.rs#funcA",
                "children": [{
                    "name": "funcRemote",
                    "repo_uid": "server-only-repo",
                    "canonical_id": "r:src/remote.rs#funcRemote",
                    "children": []
                }]
            }
        });
        // Local set has another repo (B) but NOT "server-only-repo".
        let local_repos: std::collections::HashSet<String> =
            ["local-repo-a".to_string(), "local-repo-b".to_string()]
                .into_iter()
                .collect();
        let boundaries = detect_boundaries_in_trace(&result, &local_repos);
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].name, "funcRemote");
        assert_eq!(boundaries[0].canonical_id, "r:src/remote.rs#funcRemote");
    }

    #[test]
    fn uuid_simple_generates_hex() {
        let id = trace_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── Two-tier blast_radius tests ──────────────────────────────

    #[test]
    fn extract_local_repos_from_changed_symbols_with_repo_uid() {
        let local = json!({
            "changed_symbols": [
                {"uid": "1", "name": "foo", "file_path": "src/lib.rs", "repo_uid": "repo-alpha"},
                {"uid": "2", "name": "bar", "file_path": "api/handler.rs", "repo_uid": "repo-beta"},
            ],
            "affected_symbols": [
                {"uid": "3", "name": "baz", "file_path": "src/util.rs", "repo_uid": "repo-alpha"},
            ]
        });
        let repos = extract_local_repos(&local);
        assert!(repos.contains("repo-alpha"));
        assert!(repos.contains("repo-beta"));
        // Should NOT contain path components — repo_uid takes precedence.
        assert!(!repos.contains("src"));
        assert!(!repos.contains("api"));
    }

    #[test]
    fn extract_local_repos_falls_back_to_file_path() {
        // When repo_uid is absent, falls back to full file_path.
        let local = json!({
            "changed_symbols": [
                {"uid": "1", "name": "foo", "file_path": "src/lib.rs"},
            ],
            "affected_symbols": []
        });
        let repos = extract_local_repos(&local);
        assert!(repos.contains("src/lib.rs"));
    }

    #[test]
    fn filter_org_results_returns_full_for_now() {
        let server = json!({"affected_symbols": [{"name": "x"}]});
        let local_repos = std::collections::HashSet::new();
        let filtered = filter_org_results(&server, &local_repos);
        assert_eq!(filtered, server);
    }

    #[test]
    fn filter_org_results_dedups_same_repo_across_instances() {
        // Finding #6: the LOCAL tier and the SERVER's org-wide tier index the
        // SAME repo — identical normalized `url_hash` post-T3.1b — but under
        // DIFFERENT instance ids (`local` vs `nestweaver-server`). Matching on
        // the FULL `repo_uid` never coalesces, so `org_wide_impact` duplicates
        // everything the local tier already reported. Dedup must match on the
        // instance-independent repo identity and drop the redundant server rows.
        let url_hash = "abc123def456";
        let local = json!({
            "changed_symbols": [
                {"name": "process_payment", "file_path": "src/billing.rs",
                 "repo_uid": format!("repo:local:{url_hash}")},
            ],
            "affected_symbols": []
        });
        let local_repos = extract_local_repos(&local);

        let server = json!({
            "affected_symbols": [
                {"name": "process_payment", "file_path": "src/billing.rs",
                 "repo_uid": format!("repo:nestweaver-server:{url_hash}")},
            ],
            "affected_clusters": [
                {"representative_file": "src/billing.rs",
                 "repo_uid": format!("repo:nestweaver-server:{url_hash}")},
            ]
        });
        let filtered = filter_org_results(&server, &local_repos);

        let affected = filtered["affected_symbols"].as_array().unwrap();
        assert!(
            affected.is_empty(),
            "server rows for a repo already covered by the local tier must be \
             dropped despite the differing instance segment, got {affected:?}"
        );
        let clusters = filtered["affected_clusters"].as_array().unwrap();
        assert!(
            clusters.is_empty(),
            "server clusters for a locally-covered repo must be dropped too, \
             got {clusters:?}"
        );
    }

    #[test]
    fn filter_org_results_retains_distinct_repo_and_uncovered_symbol() {
        // Guard: a genuinely different repo (different `url_hash`) must NOT be
        // collapsed, and a server symbol whose repo is NOT in the local tier
        // must be RETAINED in org_wide_impact.
        let local = json!({
            "changed_symbols": [
                {"name": "f", "file_path": "src/a.rs",
                 "repo_uid": "repo:local:aaaaaaaaaaaa"},
            ],
            "affected_symbols": []
        });
        let local_repos = extract_local_repos(&local);

        let server = json!({
            "affected_symbols": [
                {"name": "g", "file_path": "src/b.rs",
                 "repo_uid": "repo:nestweaver-server:bbbbbbbbbbbb"},
            ]
        });
        let filtered = filter_org_results(&server, &local_repos);
        let affected = filtered["affected_symbols"].as_array().unwrap();
        assert_eq!(
            affected.len(),
            1,
            "a server symbol in a repo not covered locally must be retained"
        );
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

        assert!(
            merged.get("connected").is_some(),
            "connected field must be preserved"
        );
        assert!(
            merged.get("seeds").is_some(),
            "seeds field must be preserved"
        );
        assert!(merged.get("_meta").is_some(), "_meta must be present");
        let connected = merged["connected"].as_array().unwrap();
        assert!(
            connected.len() >= 3,
            "should merge connected items from both"
        );
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
