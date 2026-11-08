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

use nestweaver_federation::dispatch::dispatch_json_rpc_authed_profile;
use nestweaver_federation::health::{
    MaintenanceProbe, code_is_down, effective_timeout_for_tool, eject_with_cap,
    handle_query_timeout_for_tool, is_upstream_down, local_entry_for_server_repo, log_tool_timeout,
};
use nestweaver_federation::results::{count_results, inject_or_wrap_provenance, set_stale_repos};

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
    pub last_query_outcome: String,
    pub ejected: bool,
    pub fallback_timeout_ms: u128,
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
    delivery_profile: nestweaver_schema::ToolDeliveryProfile,
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

/// Marks an error from dispatch on the local daemon channel, never an upstream.
/// The private constructor keeps provenance at the dispatch boundary; callers
/// may attach their selected local database without guessing from error prose.
#[derive(Debug)]
pub struct LocalQueryFailure(String);

impl std::fmt::Display for LocalQueryFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

async fn dispatch_local_query(
    local: &mut DaemonClient,
    tool_name: &str,
    params: &Value,
    profile: nestweaver_schema::ToolDeliveryProfile,
) -> Result<Value> {
    dispatch_json_rpc_authed_profile(local.inner_mut(), tool_name, params, None, profile)
        .await
        .map_err(|error| {
            let message = error.to_string();
            error.context(LocalQueryFailure(message))
        })
}

impl HybridClient {
    /// Connect to the local daemon with auto-discovery of upstream servers.
    ///
    /// Upstream discovery walks `start_dir` upward looking for
    /// `.nestweaver/server.toml`, checks `~/.config/nestweaver/upstreams.toml`,
    /// honors the `NESTWEAVER_UPSTREAM` env var, and reads any `[[upstream]]`
    /// entries from the instance config file (`config_path`).
    ///
    /// Autostarts the local daemon as ONE-SHOT (see
    /// `DaemonClient::connect`/`autostart::DaemonUsage`) — the shortened
    /// ephemeral idle-timeout applies when `db_path` looks temporary. This is
    /// correct for every CLI command that issues its RPC(s) and exits, which
    /// is every caller of this function except `mcp`'s daemon-proxy route;
    /// that one calls [`Self::connect_long_running`] instead.
    pub async fn connect(
        db_path: &Path,
        config_path: Option<&Path>,
        start_dir: &Path,
    ) -> Result<Self> {
        let local = DaemonClient::connect(db_path, config_path).await?;
        Self::finish_connect(local, config_path, start_dir)
    }

    /// Route reads through a verified incumbent without autostart. Reuse the
    /// exact admitted client so an exit between admission and query cannot
    /// create an empty database at a replica's absent logical anchor.
    pub async fn connect_existing_verified_read(
        db_path: &Path,
        config_path: Option<&Path>,
        start_dir: &Path,
    ) -> Result<Option<Self>> {
        DaemonClient::connect_existing_verified_read(db_path, config_path)
            .await?
            .map(|local| Self::finish_connect(local, config_path, start_dir))
            .transpose()
    }

    /// Connect the way [`Self::connect`] does, but declare the CALLER as a
    /// long-running foreground session (`mcp`'s daemon-proxy stdio server) —
    /// see `DaemonClient::connect_long_running` for why this must be a
    /// distinct, explicit entrypoint rather than a cleverer path check.
    pub async fn connect_long_running(
        db_path: &Path,
        config_path: Option<&Path>,
        start_dir: &Path,
    ) -> Result<Self> {
        let local = DaemonClient::connect_long_running(db_path, config_path).await?;
        Self::finish_connect(local, config_path, start_dir)
    }

    /// Upstream discovery, shared by both connect entrypoints above — the
    /// only thing that differs between them is which `DaemonClient`
    /// constructor spawned `local`.
    fn finish_connect(
        local: DaemonClient,
        config_path: Option<&Path>,
        start_dir: &Path,
    ) -> Result<Self> {
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
            delivery_profile: nestweaver_schema::ToolDeliveryProfile::FullCli,
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
            delivery_profile: nestweaver_schema::ToolDeliveryProfile::FullCli,
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
            delivery_profile: nestweaver_schema::ToolDeliveryProfile::FullCli,
        }
    }

    pub fn set_delivery_profile(&mut self, profile: nestweaver_schema::ToolDeliveryProfile) {
        self.delivery_profile = profile;
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

        for upstream in &self.upstreams {
            upstream.record_query_outcome(nestweaver_federation::upstream::QueryOutcome::Skipped);
        }
        let mut routing = tool_routing(tool_name);
        let mut fallback_mode = false;
        let repo_hint = extract_repo_hint(params);

        if let Some(upstream) = find_upstream_for_repo(&self.upstreams, repo_hint) {
            (routing, fallback_mode) = apply_mode_override(routing, upstream.mode);
        }

        // Populate `_meta.stale_repos` provenance from the background-computed
        // verdict (non-blocking, zero upstream I/O). Without this the field was
        // always empty even when the local graph was stale.
        let stale = self.current_stale_repos();
        let force_fallback_server = fallback_mode && query_targets_stale_repo(repo_hint, &stale);

        let mut result = self
            .route_query(routing, tool_name, params, force_fallback_server)
            .await?;

        if let Some(obj) = result.as_object_mut() {
            let meta = obj.entry("_meta").or_insert_with(|| serde_json::json!({}));
            meta["upstream_attempts"] = serde_json::json!(
                self.upstreams
                    .iter()
                    .map(|u| serde_json::json!({
                        "name": u.name, "outcome": u.query_outcome(), "ejected": !u.is_healthy()
                    }))
                    .collect::<Vec<_>>()
            );
        }
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
                    let timeout = self.upstream_timeout(tool_name, params);
                    if let Ok(server) = self.query_upstream(tool_name, params, timeout).await {
                        // Merge scalar/missing keys from server into local result.
                        if let (Some(local_obj), Some(server_obj)) =
                            (result.as_object_mut(), server.as_object())
                        {
                            // Do not expose daemon config provenance through this
                            // Combined JSON merge. A missing local key is backfilled
                            // from upstream, which could falsely label the local
                            // daemon with a remote server's effective config.
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
    fn upstream_timeout(&self, tool_name: &str, params: &Value) -> Duration {
        let repo_hint = extract_repo_hint(params);
        find_upstream_for_repo(&self.upstreams, repo_hint)
            .map(|u| effective_timeout_for_tool(u.mode, u, tool_name))
            .unwrap_or(UPSTREAM_TIMEOUT)
    }

    /// Server-preferred routing: query upstream first, fall back to local.
    async fn query_server_preferred(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        let timeout = self.upstream_timeout(tool_name, params);
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
        let mut local_result = match self.query_local(tool_name, params).await {
            Ok(value) => value,
            Err(error)
                if classify_upstream_rejection(&error) == UpstreamRejection::TargetNotFound =>
            {
                serde_json::json!({"status":"not_found", "message":format!("{error:#}")})
            }
            Err(error) => return Err(error),
        };

        // A local ambiguity needs the caller's selection. A remote match must
        // not silently choose a different entity with the same lookup name.
        if nestweaver_federation::results::lookup_disposition(tool_name, &local_result)
            == nestweaver_federation::results::LookupDisposition::Ambiguous
            || self.upstreams.is_empty()
        {
            inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
            return Ok(local_result);
        }

        // 3. Check if local results are sufficient and fresh. When local
        // returns 0 results or the repo is stale, query the server.
        let local_count = count_results(&local_result);
        if (!force_server_for_stale_repo && local_lookup_succeeded(tool_name, &local_result))
            || !fallback_should_query_server(local_count, force_server_for_stale_repo)
        {
            debug!(
                tool = tool_name,
                local_count, "fallback: local results sufficient, skipping server"
            );
            inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
            return Ok(local_result);
        }

        // 4. Local results are sparse — query server with timeout.
        let timeout = self.upstream_timeout(tool_name, params);
        debug!(
            tool = tool_name,
            local_count,
            threshold = FALLBACK_THRESHOLD,
            stale_repo = force_server_for_stale_repo,
            "fallback: local results sparse or stale, querying server"
        );
        match self.query_upstream(tool_name, params, timeout).await {
            Ok(server_result) => {
                let mappings = find_upstream_for_repo(&self.upstreams, extract_repo_hint(params))
                    .map(|u| u.vault_mappings.clone())
                    .unwrap_or_default();
                let mut merged = nestweaver_federation::results::compose_tool_results(
                    tool_name,
                    &local_result,
                    &server_result,
                    &mappings,
                );
                if tool_name == "brain_search" {
                    nestweaver_federation::results::cap_brain_search_results(
                        &mut merged,
                        nestweaver_federation::results::brain_search_limit(
                            params,
                            &local_result,
                            &server_result,
                        ),
                    );
                }
                inject_or_wrap_provenance(&mut merged, &["local", "server"], &[]);
                Ok(merged)
            }
            Err(e) => {
                if classify_upstream_rejection(&e) != UpstreamRejection::TargetNotFound {
                    debug!(tool = tool_name, local_count, error = %e,
                        "fallback: upstream query failed, returning local-only results");
                }
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
        let Some(selected_index) = selected_idx else {
            let mut result = self.query_local(tool_name, params).await?;
            inject_or_wrap_provenance(&mut result, &["local"], &[]);
            return Ok(result);
        };
        // The deadline the server future runs under, kept so a miss can be
        // fed back into the latency estimate (nw-905).
        let merge_deadline = selected_idx.map(|idx| {
            let u = &self.upstreams[idx];
            effective_timeout_for_tool(u.mode, u, tool_name)
        });
        let server_fut = {
            let u = &self.upstreams[selected_index];
            let timeout = merge_deadline.unwrap_or(u.timeout);
            let mut client = u.client();
            let token = u.auth_token().map(|t| t.to_string());
            let tool = tool_name.to_string();
            let p = params.clone();
            let profile = self.delivery_profile;
            // The future borrows only upstreams; local dispatch borrows the
            // disjoint local channel below.
            async move {
                if let Err(e) = u.ensure_connected().await {
                    u.record_query_outcome(
                        nestweaver_federation::upstream::QueryOutcome::Unavailable,
                    );
                    return Ok(Err(e));
                }
                let started = Instant::now();
                let res = tokio::time::timeout(
                    timeout,
                    dispatch_json_rpc_authed_profile(
                        &mut client,
                        &tool,
                        &p,
                        token.as_deref(),
                        profile,
                    ),
                )
                .await;
                match &res {
                    Ok(Ok(_)) => {
                        u.record_query_outcome(
                            nestweaver_federation::upstream::QueryOutcome::Success,
                        );
                        u.record_tool_latency(&tool, started.elapsed());
                    }
                    Ok(Err(e)) => u.record_query_outcome(if is_upstream_down(e) {
                        nestweaver_federation::upstream::QueryOutcome::Unavailable
                    } else {
                        nestweaver_federation::upstream::QueryOutcome::Rejected
                    }),
                    Err(_) => {
                        u.record_query_outcome(
                            nestweaver_federation::upstream::QueryOutcome::Timeout,
                        );
                        log_tool_timeout(u, &tool, timeout, started.elapsed());
                    }
                }
                res
            }
        };

        // Now borrow self.local mutably for the local query.
        let local_fut =
            dispatch_local_query(&mut self.local, tool_name, params, self.delivery_profile);

        let (local_result, server_result) = tokio::join!(local_fut, server_fut);
        let local = match local_result {
            Ok(l) => l,
            Err(local_err) => {
                // The local tier failed (e.g. the daemon is restarting/busy).
                // Degrade to server-only if the upstream answered — matching every
                // other hybrid mode (fallback/server-preferred all degrade
                // gracefully) — instead of failing a request the server could
                // satisfy. Only propagate the error when BOTH tiers are down.
                if let Ok(Ok(server)) = server_result {
                    let mut result = server;
                    inject_or_wrap_provenance(&mut result, &["server"], &[]);
                    return Ok(result);
                }
                return Err(local_err);
            }
        };

        match server_result {
            Ok(Ok(server)) => {
                let mappings = selected_idx
                    .map(|i| &self.upstreams[i].vault_mappings)
                    .cloned()
                    .unwrap_or_default();
                let mut merged = nestweaver_federation::results::compose_tool_results(
                    tool_name, &local, &server, &mappings,
                );
                if tool_name == "brain_search" {
                    nestweaver_federation::results::cap_brain_search_results(
                        &mut merged,
                        nestweaver_federation::results::brain_search_limit(params, &local, &server),
                    );
                }
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
                } else if let Some(idx) = selected_idx {
                    log_upstream_rejection(&self.upstreams[idx].name, &e);
                }
                let mut result = local;
                inject_or_wrap_provenance(&mut result, &["local"], &[]);
                Ok(result)
            }
            Err(_) => {
                debug!("merge: server query timed out, using local only");
                if let Some(idx) = selected_idx {
                    // `timeout` was captured when the server future was built.
                    let u = &self.upstreams[idx];
                    handle_query_timeout_for_tool(
                        u.mode,
                        u,
                        &self.upstreams,
                        &self.ejection_guard,
                        tool_name,
                        merge_deadline.unwrap_or(u.timeout),
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
        let timeout = self.upstream_timeout(tool_name, params);
        let local = self.query_local(tool_name, params).await?;
        match self.query_upstream(tool_name, params, timeout).await {
            Ok(server) => {
                let mut merged = nestweaver_federation::results::compose_fanout_results(
                    tool_name, &local, &server,
                );
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
        let (local_states, local_checkout_paths): (
            std::collections::HashMap<String, String>,
            std::collections::HashMap<String, String>,
        ) = {
            let req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
            match tokio::time::timeout(
                Duration::from_secs(5),
                self.local.inner_mut().repo_states(req),
            )
            .await
            {
                Ok(Ok(resp)) => resp
                    .into_inner()
                    .repos
                    .into_iter()
                    .map(|r| {
                        (
                            (r.repo_url.clone(), r.indexed_sha),
                            (r.repo_url, r.local_checkout_path),
                        )
                    })
                    .unzip(),
                _ => (
                    std::collections::HashMap::new(),
                    std::collections::HashMap::new(),
                ),
            }
        };

        for upstream in &self.upstreams {
            let mut status = UpstreamStatus {
                name: upstream.name.clone(),
                healthy: upstream.is_healthy(),
                mode: format!("{:?}", upstream.mode).to_lowercase(),
                repo_count: 0,
                stale_repos: vec![],
                last_query_outcome: upstream.query_outcome().to_string(),
                ejected: !upstream.is_healthy(),
                fallback_timeout_ms: upstream.fallback_cap().as_millis(),
            };

            if upstream.is_healthy() {
                let mut client = upstream.client();
                let mut req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
                req.metadata_mut().insert(
                    nestweaver_schema::ToolDeliveryProfile::METADATA_KEY,
                    MetadataValue::from_static(self.delivery_profile.wire_value()),
                );
                upstream.inject_auth(&mut req);

                if let Ok(Ok(resp)) =
                    tokio::time::timeout(Duration::from_secs(5), client.repo_states(req)).await
                {
                    let server_repos = resp.into_inner().repos;
                    status.repo_count = server_repos.len();

                    for server_repo in &server_repos {
                        if let Some((local_url, local_sha)) =
                            local_entry_for_server_repo(&local_states, &server_repo.repo_url)
                            && nestweaver_federation::health::local_is_behind_with_checkout_path(
                                local_url,
                                local_checkout_paths.get(local_url).map(String::as_str),
                                local_sha,
                                &server_repo.indexed_sha,
                            )
                            .await
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
    /// Query the LOCAL daemon only, whatever the routing matrix says for
    /// `tool_name`. For requests carrying data that must never leave this
    /// machine (nw-690 review: `brain_guide` with a caller's config).
    pub async fn query_local_only(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        let mut result = self.query_local(tool_name, params).await?;
        inject_or_wrap_provenance(&mut result, &["local"], &[]);
        Ok(result)
    }

    async fn query_local(&mut self, tool_name: &str, params: &Value) -> Result<Value> {
        dispatch_local_query(&mut self.local, tool_name, params, self.delivery_profile).await
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

        if let Err(e) = upstream.ensure_connected().await {
            upstream
                .record_query_outcome(nestweaver_federation::upstream::QueryOutcome::Unavailable);
            if is_upstream_down(&e) {
                eject_with_cap(
                    upstream,
                    &self.upstreams,
                    &self.ejection_guard,
                    "cold connect failed",
                );
            } else {
                log_upstream_rejection(&upstream.name, &e);
            }
            return Err(e);
        }
        let mut client = upstream.client();
        let token = upstream.auth_token().map(|t| t.to_string());
        let started = Instant::now();
        match tokio::time::timeout(
            timeout,
            dispatch_json_rpc_authed_profile(
                &mut client,
                tool_name,
                params,
                token.as_deref(),
                self.delivery_profile,
            ),
        )
        .await
        {
            Ok(Ok(result)) => {
                upstream.record_tool_latency(tool_name, started.elapsed());
                Ok(result)
            }
            Ok(Err(e)) => {
                upstream.record_query_outcome(if is_upstream_down(&e) {
                    nestweaver_federation::upstream::QueryOutcome::Unavailable
                } else {
                    nestweaver_federation::upstream::QueryOutcome::Rejected
                });
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
                    log_upstream_rejection(&upstream.name, &e);
                }
                Err(e)
            }
            Err(_) => {
                upstream
                    .record_query_outcome(nestweaver_federation::upstream::QueryOutcome::Timeout);
                log_tool_timeout(upstream, tool_name, timeout, started.elapsed());
                handle_query_timeout_for_tool(
                    upstream.mode,
                    upstream,
                    &self.upstreams,
                    &self.ejection_guard,
                    tool_name,
                    timeout,
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
    let (local_states, local_checkout_paths): (
        std::collections::HashMap<String, String>,
        std::collections::HashMap<String, String>,
    ) = {
        let req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
        match local.repo_states(req).await {
            Ok(resp) => resp
                .into_inner()
                .repos
                .into_iter()
                .map(|r| {
                    (
                        (r.repo_url.clone(), r.indexed_sha),
                        (r.repo_url, r.local_checkout_path),
                    )
                })
                .unzip(),
            Err(_) => return Vec::new(),
        }
    };
    nestweaver_federation::health::compute_stale_repos_with_checkout_paths(
        &local_states,
        &local_checkout_paths,
        probes,
    )
    .await
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
    query_configured_upstreams_only_profile(
        config_path,
        start_dir,
        tool_name,
        params,
        nestweaver_schema::ToolDeliveryProfile::FullCli,
    )
    .await
}

pub async fn query_configured_upstreams_only_profile(
    config_path: Option<&Path>,
    start_dir: &Path,
    tool_name: &str,
    params: &Value,
    profile: nestweaver_schema::ToolDeliveryProfile,
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
    upstream.ensure_connected().await?;
    let mut client = upstream.client();
    let token = upstream.auth_token().map(|t| t.to_string());
    let timeout = effective_timeout_for_tool(upstream.mode, upstream, tool_name);
    let started = Instant::now();
    let mut result = tokio::time::timeout(
        timeout,
        dispatch_json_rpc_authed_profile(&mut client, tool_name, params, token.as_deref(), profile),
    )
    .await
    .inspect_err(|_| {
        log_tool_timeout(upstream, tool_name, timeout, started.elapsed());
        handle_query_timeout_for_tool(
            upstream.mode,
            upstream,
            &upstreams,
            &Mutex::new(()),
            tool_name,
            timeout,
            "upstream-only query timed out",
        );
    })
    .with_context(|| format!("upstream query timed out after {}ms", timeout.as_millis()))??;
    upstream.record_tool_latency(tool_name, started.elapsed());
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

/// Why a HEALTHY upstream answered a query with a gRPC error (nw-872).
#[derive(Debug, PartialEq, Eq)]
enum UpstreamRejection {
    /// Missing/expired/insufficient credentials.
    Auth,
    /// The server is fine and simply has no such target (e.g. a note that
    /// only exists in the local vault). Expected under federation.
    TargetNotFound,
    /// Anything else: the status code and message are logged verbatim.
    Other,
}

use nestweaver_engine::node_scope::TOOL_TARGET_NOT_FOUND_CODE as TARGET_NOT_FOUND_CODE;

fn classify_upstream_rejection(err: &anyhow::Error) -> UpstreamRejection {
    let Some(status) = err.chain().find_map(|e| e.downcast_ref::<tonic::Status>()) else {
        return UpstreamRejection::Other;
    };
    if matches!(
        status.code(),
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
    ) {
        return UpstreamRejection::Auth;
    }
    let stamped = status
        .metadata()
        .get(nestweaver_engine::node_scope::NW_ERROR_CODE_METADATA_KEY)
        .and_then(|v| v.to_str().ok());
    if stamped == Some(TARGET_NOT_FOUND_CODE) {
        return UpstreamRejection::TargetNotFound;
    }
    UpstreamRejection::Other
}

/// Log a rejection from a healthy upstream with its real gRPC status. The old
/// text blamed "auth token / arguments" for every rejection and, formatting
/// only the outermost context, dropped the status (nw-872: a `note_get` miss
/// on a server that does not hold the note read as an auth failure).
fn local_lookup_succeeded(tool: &str, result: &Value) -> bool {
    nestweaver_federation::results::lookup_disposition(tool, result)
        == nestweaver_federation::results::LookupDisposition::Found
}

fn log_upstream_rejection(upstream: &str, err: &anyhow::Error) {
    match classify_upstream_rejection(err) {
        UpstreamRejection::TargetNotFound => {
            debug!(upstream, error = %format!("{err:#}"), "upstream has no match for this query (server healthy)");
        }
        UpstreamRejection::Auth => {
            warn!(upstream, error = %format!("{err:#}"), "upstream rejected credentials (server healthy — not ejecting); check the auth token");
        }
        UpstreamRejection::Other => {
            warn!(upstream, error = %format!("{err:#}"), "upstream rejected query (server healthy — not ejecting)");
        }
    }
}

/// Let the upstream's `RoutingMode` override the per-tool default category.
/// Returns the (possibly rewritten) routing and whether Fallback mode applies.
///
/// Only Merge/LocalFirst/ServerPreferred tools are promoted. LocalOnly,
/// TwoTier, Combined, Continuation and FanOut carry tool-specific semantics
/// (response shape, truncation flags, stitching) that a generic merge /
/// server-only route would break, so the mode never rewrites them (nw-904).
fn apply_mode_override(routing: ToolRouting, mode: RoutingMode) -> (ToolRouting, bool) {
    if matches!(
        routing,
        ToolRouting::LocalOnly
            | ToolRouting::TwoTier
            | ToolRouting::Combined
            | ToolRouting::Continuation
            | ToolRouting::FanOut
    ) {
        return (routing, false);
    }
    match mode {
        RoutingMode::Primary => (ToolRouting::ServerPreferred, false),
        RoutingMode::Merge => (ToolRouting::Merge, false),
        RoutingMode::Fallback => (ToolRouting::LocalFirst, true),
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
    // No hint: the query is not scoped to one repo, so any genuinely stale
    // repo (staleness itself no longer false-positives, nw-906) may affect it.
    let Some(repo_hint) = repo_hint else {
        return true;
    };
    stale_repos
        .iter()
        .any(|stale| repo_urls_equivalent(stale, repo_hint))
}

/// Does a query's repo `hint` name the `stale` repo? Full canonical identity
/// (`host/owner/name`) always matches. A hostless `owner/name` hint matches a
/// stale repo whose key ends with that `owner/name` (segment-aligned), and a
/// bare `name` matches by leaf. A hint that names a different owner never
/// matches by leaf (nw-906).
fn repo_urls_equivalent(stale: &str, hint: &str) -> bool {
    let stale_key = normalized_repo_key(stale);
    let hint_key = normalized_repo_key(hint);
    if stale_key == hint_key {
        return true;
    }
    if hint_key.is_empty() || hint.contains("://") || hint.contains('@') {
        return false;
    }
    if !hint_key.contains('/') {
        return repo_name(stale).is_some_and(|leaf| leaf.eq_ignore_ascii_case(&hint_key));
    }
    stale_key.ends_with(&format!("/{hint_key}"))
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

    // NOTE: picks the first healthy upstream, NOT the repo-matched one that every
    // other path uses (`find_upstream_for_repo`). Correct for single-upstream and
    // for multi-upstream deployments where all servers index the same repos; but in
    // a repo-partitioned multi-upstream setup (upstream globs splitting repos across
    // servers) cross-repo continuation may hit the wrong server. Routing by the
    // boundary's repo isn't possible yet: a `TraceBoundary` carries only an opaque
    // `repo_uid`/`canonical_id` hash, not the glob-matchable repo URL that
    // `matches_repo` needs. TODO: thread the callee repo URL through TraceBoundary
    // and select per-boundary via `find_upstream_for_repo`.
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
    let up_timeout = effective_timeout_for_tool(upstream.mode, upstream, "flow_trace_continue");

    for boundary in &boundaries {
        if upstream.ensure_connected().await.is_err() {
            upstream
                .record_query_outcome(nestweaver_federation::upstream::QueryOutcome::Unavailable);
            continue;
        }
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
        req.metadata_mut().insert(
            nestweaver_schema::ToolDeliveryProfile::METADATA_KEY,
            MetadataValue::from_static(client.delivery_profile.wire_value()),
        );
        upstream.inject_auth(&mut req);

        let started = Instant::now();
        match tokio::time::timeout(up_timeout, up_client.flow_trace_continue(req)).await {
            Ok(Ok(resp)) => {
                upstream.record_tool_latency("flow_trace_continue", started.elapsed());
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
                log_tool_timeout(
                    upstream,
                    "flow_trace_continue",
                    up_timeout,
                    started.elapsed(),
                );
                handle_query_timeout_for_tool(
                    upstream.mode,
                    upstream,
                    &client.upstreams,
                    &client.ejection_guard,
                    "flow_trace_continue",
                    up_timeout,
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
///
/// nw-428 review round 3: `query_local` used to be `?`-propagated
/// unconditionally, so `blast_radius`'s hard refusal on an unresolvable
/// `repo` filter (nw-428's fix) killed the ENTIRE two-tier response — local
/// AND org-wide — before upstream was ever asked. A repo that is real
/// org-wide and simply not indexed by THIS daemon is, from the local tier's
/// point of view, indistinguishable from a typo: both fail to resolve
/// locally. Which one it was can only be learned by actually asking
/// upstream, so DETECTION (recognizing "the local tier could not resolve
/// `repo`") is separated from POLICY (deciding that's fatal):
/// [`degraded_local_impact_for_unresolved_repo_filter`] only recognizes and
/// discloses; this function decides whether recognizing it is enough reason
/// to keep going.
///
/// The property this preserves, both halves: with NO healthy upstream (the
/// single-tier / no-federation case — the common deployment), the original
/// error propagates completely unchanged, so a bogus `--repo` still refuses
/// loudly exactly as before this change. With a healthy upstream, the local
/// failure degrades into a disclosed stand-in and upstream is still
/// consulted, so its `org_wide_impact` can still reach the caller.
pub async fn two_tier_query(
    client: &mut HybridClient,
    tool_name: &str,
    params: &Value,
) -> Result<Value> {
    // The LOCAL tier is computed here — the federation crate only ever sees
    // its result as data (the parameterized seam from nw-017 Phase B).
    let local_result = match client.query_local(tool_name, params).await {
        Ok(value) => value,
        Err(error) => {
            let has_healthy_upstream = client.upstreams.iter().any(|u| u.is_healthy());
            match has_healthy_upstream
                .then(|| {
                    degraded_local_impact_for_unresolved_repo_filter(tool_name, &error, params)
                })
                .flatten()
            {
                Some(degraded) => degraded,
                // No healthy upstream to fall back to (this IS the
                // single-tier case), or the error isn't the recognized
                // unresolved-`repo`-filter signal (a real local failure —
                // DB corruption, an RPC transport outage, a genuine bug —
                // must still propagate as a hard error, not be silently
                // absorbed).
                None => return Err(error),
            }
        }
    };
    Ok(nestweaver_federation::two_tier::two_tier_query_profile(
        local_result,
        &client.upstreams,
        &client.ejection_guard,
        tool_name,
        params,
        client.delivery_profile,
    )
    .await)
}

/// LEGACY fallback signal: the substring that identifies "the local `repo`
/// filter could not be resolved" when the daemon did not stamp a typed error
/// code.
///
/// nw-443 replaced this as the PRIMARY contract with
/// [`nestweaver_engine::node_scope::REPO_FILTER_UNRESOLVED_CODE`], carried in
/// gRPC metadata. Matching on prose was pinned by nothing on either end -- a
/// rename in `node_scope.rs` left the producing tests (which assert only
/// `"ambiguous"` / `"not-a-repo"`) and the consuming tests (which
/// hand-construct the string) green while federation silently reverted to
/// killing a healthy upstream's entire two-tier answer.
///
/// Kept only for older daemons that cannot stamp a typed metadata code.
/// Current request coalescing preserves the typed error for every follower.
const UNRESOLVED_REPO_FILTER_SIGNAL: &str = "repo filter entry ";

/// Whether `error` is the daemon reporting an unresolved local `repo` filter.
///
/// nw-443. TYPED FIRST: `dispatch_err_to_status` stamps
/// [`nestweaver_engine::node_scope::REPO_FILTER_UNRESOLVED_CODE`] into the
/// gRPC status metadata, and `tonic::Status` survives the client's
/// `.with_context()` wrapping as a source in the anyhow chain, so it can be
/// downcast rather than read. A rewording of the message can no longer break
/// this.
///
/// PROSE SECOND, and only as a version-skew downgrade -- see
/// [`UNRESOLVED_REPO_FILTER_SIGNAL`].
fn is_unresolved_repo_filter(error: &anyhow::Error, rendered: &str) -> bool {
    let code = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<tonic::Status>())
        .and_then(|status| {
            status
                .metadata()
                .get(nestweaver_engine::node_scope::NW_ERROR_CODE_METADATA_KEY)
        });
    match code {
        Some(value) => {
            value.to_str().ok() == Some(nestweaver_engine::node_scope::REPO_FILTER_UNRESOLVED_CODE)
        }
        None => rendered.contains(UNRESOLVED_REPO_FILTER_SIGNAL),
    }
}

/// Build a degraded, disclosure-shaped stand-in for a two-tier LOCAL result
/// whose `repo` filter did not resolve against the local index.
///
/// Scoped to `blast_radius` specifically (the only one of the three
/// two-tier tools whose `repo` filter currently routes through
/// `node_scope::resolve_repo_filter`) rather than trusting the message
/// signal alone — belt and suspenders, so a future brain_impact/
/// affected_tests error that happened to contain the same substring could
/// not be silently reinterpreted as this condition.
///
/// Returns `None` for any tool other than `blast_radius`, or any error that
/// doesn't carry [`UNRESOLVED_REPO_FILTER_SIGNAL`] — in both cases the
/// original error must propagate unchanged.
///
/// The shape mirrors `tool_blast_radius`'s own trust-contract fields
/// (`status`, `gate_state`, `coverage`, `notifications`) rather than
/// inventing a new disclosure vocabulary, so a caller already reading that
/// contract sees the familiar fields instead of a bespoke sentinel.
///
/// `changed_files`, `max_depth`, `blind_spots` and `analysis_direction` are
/// UNCONDITIONALLY present in every real `tool_blast_radius` response
/// (success or a genuine degrade — see `tools.rs`'s response builder), so
/// this stand-in fills them too rather than silently departing from the
/// documented shape: `changed_files`/`max_depth` echo the request the same
/// way the real tool does, `blind_spots` uses the same four ALWAYS-present
/// inherent gaps plus `not-indexed` (this run has, by construction, zero
/// local coverage of the named repo — the same blind spot
/// `analyze_blast_radius` itself adds whenever `repos_not_indexed` is
/// non-empty), and `analysis_direction` is the one value this codebase ever
/// produces for it.
fn degraded_local_impact_for_unresolved_repo_filter(
    tool_name: &str,
    error: &anyhow::Error,
    params: &Value,
) -> Option<Value> {
    if tool_name != "blast_radius" {
        return None;
    }
    let rendered = format!("{error:#}");
    if !is_unresolved_repo_filter(error, &rendered) {
        return None;
    }
    let changed_files = params
        .get("changed_files")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let max_depth = params.get("max_depth").and_then(Value::as_u64).unwrap_or(3);
    Some(serde_json::json!({
        "changed_files": changed_files,
        "max_depth": max_depth,
        "status": "partial",
        "gate_state": "degraded-unknown",
        "affected_symbol_count": 0,
        "returned_affected_symbol_count": 0,
        "affected_symbols": [],
        "affected_symbols_truncated": false,
        "affected_clusters": [],
        "affected_cluster_count": 0,
        "changed_symbol_count": 0,
        "changed_symbols": [],
        "coverage": {
            "repos_in_scope": [],
            "repos_not_indexed": [],
            "stale_repos": [],
            "traversal_truncated": false,
        },
        "org_wide": null,
        "cochanged_files": [],
        "resolver_stale_repos": [],
        "risk": "low",
        // The four inherent gaps every real response carries, plus
        // "not-indexed" — this stand-in exists BECAUSE the local index has
        // no coverage of the named repo, the exact condition
        // `analyze_blast_radius` itself flags with this same blind spot.
        "blind_spots": [
            "dynamic-dispatch",
            "reflection",
            "config-wiring",
            "codegen",
            "not-indexed",
        ],
        "analysis_direction": "over-approximate",
        "notifications": [{
            "level": "error",
            "descriptor": "repo-filter-unresolved-locally",
            "message": format!(
                "blast_radius's `repo` filter did not resolve against the LOCAL index \
                 (not indexed here, or a typo) — see org_wide_impact for an upstream \
                 answer: {rendered}"
            ),
        }],
        "summary": "local tier degraded: `repo` filter did not resolve locally — see org_wide_impact",
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[test]
    fn found_lookups_are_sufficient_and_partial_symbol_sets_are_not() {
        assert!(local_lookup_succeeded(
            "note_get",
            &serde_json::json!({"uid":"note:x", "title":"X"})
        ));
        assert!(local_lookup_succeeded(
            "get_summary",
            &serde_json::json!({"summaries":[{"text":"X"}],"returned":1,"total":1})
        ));
        assert!(!local_lookup_succeeded(
            "note_get",
            &serde_json::json!({"status":"not_found"})
        ));
        for status in ["ambiguous", "unsupported", "error"] {
            assert!(!local_lookup_succeeded(
                "note_get",
                &serde_json::json!({"status":status, "candidates":[]})
            ));
        }
        assert!(local_lookup_succeeded(
            "read_symbols",
            &serde_json::json!({"symbols":[{"uid":"sym:x"}],"not_found":[]})
        ));
        assert!(!local_lookup_succeeded(
            "read_symbols",
            &serde_json::json!({"symbols":[{"uid":"sym:x"}],"not_found":["y"]})
        ));
        assert!(!local_lookup_succeeded(
            "brain_search",
            &serde_json::json!({"results":[{}]})
        ));
    }

    #[test]
    fn explicit_error_code_overrides_legacy_looking_text() {
        let text = "repo filter entry old message";
        for code in [None, Some("repo-filter-unresolved"), Some("another-code")] {
            let mut status = tonic::Status::internal(text);
            if let Some(code) = code {
                status.metadata_mut().insert(
                    nestweaver_engine::node_scope::NW_ERROR_CODE_METADATA_KEY,
                    code.parse().unwrap(),
                );
            }
            let error = anyhow::Error::new(status).context("outer client context");
            assert_eq!(
                super::is_unresolved_repo_filter(&error, text),
                code != Some("another-code")
            );
        }
        assert!(!super::is_unresolved_repo_filter(
            &anyhow::anyhow!("ordinary error"),
            "ordinary error"
        ));
    }

    use super::*;
    use serde_json::json;

    /// nw-428 review round 3 (federation regression). The full
    /// `two_tier_query` async path needs a live daemon connection to
    /// exercise (`HybridClient::query_local` calls a real gRPC channel; see
    /// `fallback_mode_overrides_merge_to_local_first`'s comment on the same
    /// limitation for `query()`), so this covers the pure detection+build
    /// logic in isolation instead of faking a federated round trip.
    ///
    /// COUNTERWEIGHT: asserting the returned `Value`'s specific fields
    /// (rather than just `is_some()`) matters here — a version of this
    /// function that recognized the signal but built an empty `{}` stand-in
    /// would still pass an `is_some()`-only check while giving a caller
    /// nothing to read.
    #[test]
    fn degraded_stand_in_recognizes_the_daemon_wrapped_signal_and_discloses_it() {
        // Mirrors the ACTUAL wrapping chain: node_scope::resolve_repo_filter's
        // message, then nestweaver-daemon's dispatch_err_to_status prefix,
        // then dispatch_json_rpc_authed's own RPC-failed context — so this
        // proves the signal survives realistic wrapping, not just a bare
        // string.
        let inner = anyhow::anyhow!(
            "repo filter entry \"bx-react-native-client\": repo 'bx-react-native-client' not \
             found in graph; run `nestweaver list-repos` to see indexed repos by name/uid"
        );
        let daemon_wrapped = anyhow::anyhow!("tool blast_radius failed: {inner:#}")
            .context("blast_radius RPC failed");
        let params = json!({
            "changed_files": ["src/theme/ThemeContext.tsx"],
            "max_depth": 5,
            "repo": "bx-react-native-client",
        });

        let degraded = degraded_local_impact_for_unresolved_repo_filter(
            "blast_radius",
            &daemon_wrapped,
            &params,
        )
        .expect("the wrapped daemon error must still be recognized");

        assert_eq!(degraded["status"], "partial");
        assert_eq!(degraded["gate_state"], "degraded-unknown");
        assert_eq!(degraded["affected_symbol_count"], 0);
        // nw-428 review round 4: a real tool_blast_radius response
        // UNCONDITIONALLY carries changed_files/max_depth/blind_spots/
        // analysis_direction, success or degrade — a stand-in that omitted
        // them would silently depart from the documented response shape.
        assert_eq!(
            degraded["changed_files"],
            json!(["src/theme/ThemeContext.tsx"]),
            "must echo the request's changed_files, not omit the field"
        );
        assert_eq!(
            degraded["max_depth"], 5,
            "must echo the request's max_depth"
        );
        assert_eq!(
            degraded["analysis_direction"], "over-approximate",
            "must carry the same analysis_direction every real response does"
        );
        let blind_spots: Vec<&str> = degraded["blind_spots"]
            .as_array()
            .expect("blind_spots must be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for inherent in ["dynamic-dispatch", "reflection", "config-wiring", "codegen"] {
            assert!(
                blind_spots.contains(&inherent),
                "the four inherent blind spots must always be present; got {blind_spots:?}"
            );
        }
        assert!(
            blind_spots.contains(&"not-indexed"),
            "this stand-in exists BECAUSE the repo has no local coverage, so \
             not-indexed must be flagged; got {blind_spots:?}"
        );
        let notifications = degraded["notifications"]
            .as_array()
            .expect("notifications must be an array");
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0]["descriptor"],
            "repo-filter-unresolved-locally"
        );
        assert!(
            notifications[0]["message"]
                .as_str()
                .unwrap()
                .contains("bx-react-native-client"),
            "the disclosure must name the selector that failed to resolve; got {degraded}"
        );
    }

    /// COUNTERWEIGHT: `changed_files`/`max_depth` must come from the
    /// request, not a hardcoded default, or a caller reading the degraded
    /// stand-in back would see the wrong file list. Omitting `max_depth`
    /// from the request must still fall back to the real tool's own
    /// default (3), not a missing field.
    #[test]
    fn degraded_stand_in_echoes_request_fields_and_defaults_missing_max_depth() {
        let error = anyhow::anyhow!("repo filter entry \"x\": repo 'x' not found in graph");
        let params = json!({ "changed_files": ["a.rs", "b.rs"] });
        let degraded =
            degraded_local_impact_for_unresolved_repo_filter("blast_radius", &error, &params)
                .expect("recognized signal must still produce a stand-in");
        assert_eq!(degraded["changed_files"], json!(["a.rs", "b.rs"]));
        assert_eq!(
            degraded["max_depth"], 3,
            "an omitted max_depth must default to 3, matching tool_blast_radius's own default"
        );
    }

    /// nw-443: THE TYPED CONTRACT. Detection must survive a complete
    /// rewording of the message, because the previous contract -- a substring
    /// match on `resolve_repo_filter`'s prose, pinned by nothing on either
    /// end -- meant a rename in `node_scope.rs` silently reverted federation
    /// to killing a healthy upstream's whole answer, with every test on both
    /// sides still green.
    #[test]
    fn degraded_stand_in_recognizes_the_typed_error_code_without_the_prose() {
        let mut status =
            tonic::Status::internal("tool blast_radius failed: entirely different wording");
        status.metadata_mut().insert(
            nestweaver_engine::node_scope::NW_ERROR_CODE_METADATA_KEY,
            nestweaver_engine::node_scope::REPO_FILTER_UNRESOLVED_CODE
                .parse()
                .unwrap(),
        );
        let error = anyhow::Error::new(status).context("blast_radius RPC failed");
        assert!(
            degraded_local_impact_for_unresolved_repo_filter("blast_radius", &error, &json!({}))
                .is_some(),
            "the typed error code must be recognized on its own, with no prose match"
        );
    }

    /// COUNTERWEIGHT to the test above: a `Status` carrying a DIFFERENT code
    /// must not be absorbed. Otherwise the typed check would degrade every
    /// coded error into a stand-in -- a gate that always fires.
    #[test]
    fn degraded_stand_in_ignores_a_different_typed_error_code() {
        let mut status = tonic::Status::internal("tool blast_radius failed: something else");
        status.metadata_mut().insert(
            nestweaver_engine::node_scope::NW_ERROR_CODE_METADATA_KEY,
            "some-other-code".parse().unwrap(),
        );
        let error = anyhow::Error::new(status).context("blast_radius RPC failed");
        assert!(
            degraded_local_impact_for_unresolved_repo_filter("blast_radius", &error, &json!({}))
                .is_none()
        );
    }

    /// nw-443 DOWNGRADE PATH: a new client against an older daemon gets no
    /// metadata code at all, only the legacy prose. That must still be
    /// recognized, or upgrading the client alone would reintroduce the very
    /// regression this detection exists to prevent.
    #[test]
    fn degraded_stand_in_still_recognizes_the_legacy_prose_signal() {
        let error = anyhow::anyhow!("repo filter entry \"x\": repo 'x' not found in graph")
            .context("blast_radius RPC failed");
        assert!(
            degraded_local_impact_for_unresolved_repo_filter("blast_radius", &error, &json!({}))
                .is_some(),
            "an un-stamped daemon's message must still degrade gracefully"
        );
    }

    /// COUNTERWEIGHT: an unrelated local failure (transport outage, a real
    /// bug — anything that doesn't carry the recognized signal) must NOT be
    /// absorbed into a degraded stand-in; it has to keep propagating as a
    /// hard error, or a genuine local outage would be silently downgraded to
    /// "partial" results.
    #[test]
    fn degraded_stand_in_ignores_unrelated_local_failures() {
        let error = anyhow::anyhow!("connection refused").context("blast_radius RPC failed");
        assert!(
            degraded_local_impact_for_unresolved_repo_filter("blast_radius", &error, &json!({}))
                .is_none(),
            "an unrelated failure must not be reinterpreted as an unresolved repo filter"
        );
    }

    /// COUNTERWEIGHT: even a message carrying the exact signal must not be
    /// absorbed for a DIFFERENT tool — `brain_impact`/`affected_tests` don't
    /// route their `repo` filter through `node_scope::resolve_repo_filter`
    /// today, so treating the same substring as this condition for them
    /// would be inventing a guarantee this batch never established.
    #[test]
    fn degraded_stand_in_is_scoped_to_blast_radius_only() {
        let error = anyhow::anyhow!("repo filter entry \"x\": repo 'x' not found in graph");
        assert!(
            degraded_local_impact_for_unresolved_repo_filter("brain_impact", &error, &json!({}))
                .is_none(),
            "the same message signal must not be reinterpreted for a different tool"
        );
    }

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
            fallback_timeout: None,
            vault_mappings: Default::default(),
        };
        let cfg2 = UpstreamConfig {
            name: Some("partner".to_string()),
            url: "http://127.0.0.1:19991".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
            ca_cert: None,
            fallback_timeout: None,
            vault_mappings: Default::default(),
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
            fallback_timeout: None,
            vault_mappings: Default::default(),
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
            fallback_timeout: None,
            vault_mappings: Default::default(),
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
            fallback_timeout: None,
            vault_mappings: Default::default(),
        };
        let cfg_partner = UpstreamConfig {
            name: Some("partner".to_string()),
            url: "http://127.0.0.1:19991".to_string(),
            token: None,
            repos: vec!["partner/*".to_string()],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
            ca_cert: None,
            fallback_timeout: None,
            vault_mappings: Default::default(),
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

    // ── nw-904: tool-specific routing survives the mode override ─────────

    #[test]
    fn fanout_and_continuation_tools_are_never_rewritten_by_mode() {
        for tool in ["regex_search", "count_patterns"] {
            assert_eq!(tool_routing(tool), ToolRouting::FanOut, "{tool}");
            for mode in [
                RoutingMode::Fallback,
                RoutingMode::Merge,
                RoutingMode::Primary,
            ] {
                assert_eq!(
                    apply_mode_override(ToolRouting::FanOut, mode),
                    (ToolRouting::FanOut, false),
                    "{tool} under {mode:?}"
                );
            }
        }
        assert_eq!(
            apply_mode_override(ToolRouting::Continuation, RoutingMode::Merge).0,
            ToolRouting::Continuation
        );
    }

    #[test]
    fn mode_override_still_promotes_overridable_tools() {
        assert_eq!(
            apply_mode_override(ToolRouting::Merge, RoutingMode::Fallback),
            (ToolRouting::LocalFirst, true)
        );
        assert_eq!(
            apply_mode_override(ToolRouting::LocalFirst, RoutingMode::Primary),
            (ToolRouting::ServerPreferred, false)
        );
        assert_eq!(
            apply_mode_override(ToolRouting::ServerPreferred, RoutingMode::Merge),
            (ToolRouting::Merge, false)
        );
        for fixed in [
            ToolRouting::LocalOnly,
            ToolRouting::TwoTier,
            ToolRouting::Combined,
        ] {
            assert_eq!(apply_mode_override(fixed, RoutingMode::Primary).0, fixed);
        }
    }

    // ── nw-872: honest rejection classification ──────────────────────────

    #[test]
    fn healthy_upstream_note_miss_is_not_blamed_on_auth() {
        let mut status = tonic::Status::internal("tool note_get failed: no note found");
        status.metadata_mut().insert(
            nestweaver_engine::node_scope::NW_ERROR_CODE_METADATA_KEY,
            TARGET_NOT_FOUND_CODE.parse().unwrap(),
        );
        let err = anyhow::Error::new(status).context("note_get RPC failed");
        assert_eq!(
            classify_upstream_rejection(&err),
            UpstreamRejection::TargetNotFound
        );
        // The rendered form keeps the real status, not just the context.
        assert!(format!("{err:#}").contains("no note found"));
    }

    #[test]
    fn auth_and_other_rejections_are_distinguished() {
        let auth = anyhow::Error::new(tonic::Status::unauthenticated("bad token"))
            .context("note_get RPC failed");
        assert_eq!(classify_upstream_rejection(&auth), UpstreamRejection::Auth);
        let other = anyhow::Error::new(tonic::Status::invalid_argument("bad arg"));
        assert_eq!(
            classify_upstream_rejection(&other),
            UpstreamRejection::Other
        );
        // An unstamped NotFound (e.g. an unknown RPC) is NOT a benign lookup
        // miss; it stays a warning with its status.
        let unstamped = anyhow::Error::new(tonic::Status::not_found("no such method"));
        assert_eq!(
            classify_upstream_rejection(&unstamped),
            UpstreamRejection::Other
        );
        assert_eq!(
            classify_upstream_rejection(&anyhow::anyhow!("plain")),
            UpstreamRejection::Other
        );
    }

    // ── nw-906: stale-repo targeting ─────────────────────────────────────

    #[test]
    fn unhinted_queries_are_forced_when_a_repo_is_stale_and_not_when_none_is() {
        let stale = vec!["https://github.com/acme/api".to_string()];
        assert!(query_targets_stale_repo(None, &stale));
        assert!(!query_targets_stale_repo(None, &[]));
        assert!(query_targets_stale_repo(
            Some("git@github.com:acme/api.git"),
            &stale
        ));
        assert!(query_targets_stale_repo(Some("api"), &stale));
    }

    #[test]
    fn stale_hint_matches_by_owner_name_without_cross_org_leaf_matches() {
        let stale = vec!["https://github.com/acme/api".to_string()];
        assert!(query_targets_stale_repo(Some("acme/api"), &stale));
        assert!(query_targets_stale_repo(Some("ACME/API"), &stale));
        assert!(!query_targets_stale_repo(Some("other-org/api"), &stale));
        assert!(!query_targets_stale_repo(
            Some("https://github.com/other-org/api"),
            &stale
        ));
        assert!(!query_targets_stale_repo(
            Some("github.com/other/api"),
            &stale
        ));
        // Segment-aligned: `me/api` must not match `acme/api`.
        assert!(!query_targets_stale_repo(Some("me/api"), &stale));
    }
}
