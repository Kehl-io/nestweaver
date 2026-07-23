//! Daemon-side federation coordinator for the `/mcp` boundary (nw-017 Phase B).
//!
//! Per the accepted server-mode ADR (Decision 2), the daemon itself is the
//! federated coordinator at its `/mcp` boundary: a raw MCP client POSTing to
//! `/mcp` gets two-tier results (`local_impact` + `org_wide_impact`) for
//! two-tier-routed tools when an upstream is configured, plus namespaced
//! in-band provenance on every tool result.
//!
//! This module holds the daemon-side half of that seam:
//! - [`FederationState`]: upstream handles built once from the instance
//!   config, the shared ejection guard, and the cached staleness verdict.
//! - [`federate_two_tier`]: the coordinator step run after the local dispatch
//!   and before envelope assembly. Delegates the upstream call + timeout +
//!   ejection flow to the shared [`nestweaver_federation::two_tier`]
//!   machinery (the local tier is parameterized as data).
//! - [`spawn_staleness_refresher`]: the background maintenance task mirroring
//!   the client's `maintenance_loop` — active health recovery of ejected
//!   upstreams and the off-hot-path staleness verdict refresh. The daemon has
//!   in-process store access, so the LOCAL repo states are read directly from
//!   the [`GraphStore`] (the same fields the `RepoStates` RPC returns) rather
//!   than over gRPC.
//!
//! Scoping for Stage B: only `TwoTier`-routed tools (blast_radius,
//! brain_impact, affected_tests) federate at this boundary. `Merge`/`FanOut`/
//! `Continuation`/`ServerPreferred` tools stay local-only for now — their
//! orchestration (RRF merge, boundary stitching, repo-partitioned fan-out)
//! still lives in the client-side `HybridClient`, and wiring it here is a
//! follow-up. Their `_meta` therefore stays honest: scope `"single-node"`,
//! sources `["daemon"]`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, info, warn};

use nestweaver_engine::authz::VisibleRepos;
use nestweaver_federation::discovery::{RoutingMode, UpstreamConfig};
use nestweaver_federation::health::MaintenanceProbe;
use nestweaver_federation::routing::{ToolRouting, tool_routing};
use nestweaver_federation::upstream::{ProbeOutcome, UpstreamHandle, now_ms};
use nestweaver_store::GraphStore;

/// Refresh cadence for the background maintenance task. Mirrors the client's
/// `MAINTENANCE_INTERVAL` in `nestweaver-client/src/hybrid.rs` so the daemon
/// and the hybrid client observe staleness with the same latency.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(10);

/// Timeout for a single active health-recovery probe. Mirrors the client's
/// `HEALTH_PROBE_TIMEOUT`.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared federation state for the daemon-side coordinator.
///
/// Built once at `McpHttpState` construction from the instance config's
/// `[[upstream]]` entries and shared between the `/mcp` request handler and
/// the background staleness/health task.
pub struct FederationState {
    /// Upstream handles (lazy channels + health/ejection/latency state).
    pub upstreams: Vec<UpstreamHandle>,
    /// Serializes the recount→decide→eject sequence so concurrent failing
    /// queries cannot breach the blast-radius cap (see
    /// [`nestweaver_federation::health::eject_with_cap`]).
    ejection_guard: Mutex<()>,
    /// Cached staleness verdict — repo URLs where the daemon's local index is
    /// behind an upstream's. Written by the background refresher, read
    /// (non-blocking, zero upstream I/O) on every `/mcp` tool result.
    stale_repos: RwLock<Vec<String>>,
}

impl FederationState {
    /// Build federation state from an instance config's `[[upstream]]`
    /// entries. Returns `None` when no upstreams are configured or none can
    /// be constructed, so callers can skip federation entirely (zero
    /// regression for the common single-node case).
    pub fn from_instance_config(cfg: &nestweaver_engine::InstanceConfig) -> Option<Arc<Self>> {
        if cfg.upstream.is_empty() {
            return None;
        }
        let upstreams: Vec<UpstreamHandle> = cfg
            .upstream
            .iter()
            .filter_map(|entry| {
                let config = upstream_entry_to_config(entry);
                match UpstreamHandle::from_config(&config) {
                    Ok(handle) => Some(handle),
                    Err(e) => {
                        warn!(
                            upstream = %config.name.as_deref().unwrap_or("upstream"),
                            url = %config.url,
                            error = %e,
                            "skipping upstream — handle construction failed"
                        );
                        None
                    }
                }
            })
            .collect();
        if upstreams.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            upstreams,
            ejection_guard: Mutex::new(()),
            stale_repos: RwLock::new(Vec::new()),
        }))
    }

    /// Number of configured upstream handles.
    pub fn upstream_count(&self) -> usize {
        self.upstreams.len()
    }

    /// Whether at least one upstream is currently in rotation.
    pub fn has_healthy_upstream(&self) -> bool {
        self.upstreams.iter().any(|u| u.is_healthy())
    }

    /// Non-blocking read of the cached staleness verdict (the only thing the
    /// query hot path does for staleness — RFC 5861 stale-while-revalidate).
    pub fn stale_repos(&self) -> Vec<String> {
        // Poison-tolerant, symmetric with `set_stale_repos`: recover the last
        // good verdict via `into_inner()` rather than blanking it, so a panicked
        // prior holder does not lose the staleness reporting.
        match self.stale_repos.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Publish a freshly computed staleness verdict.
    fn set_stale_repos(&self, stale: Vec<String>) {
        // Poison-tolerant: the verdict is plain data, so a panicked prior
        // holder must not permanently freeze staleness reporting.
        match self.stale_repos.write() {
            Ok(mut guard) => *guard = stale,
            Err(poisoned) => *poisoned.into_inner() = stale,
        }
    }
}

/// Convert an engine `[[upstream]]` entry into the federation crate's
/// `UpstreamConfig`. The two shapes mirror each other field-for-field; the
/// only translation is the `mode` string → [`RoutingMode`] (matching the
/// parse in `nestweaver-federation`'s discovery layer) and `${VAR}` env
/// expansion on the token (matching the client's connect path).
pub fn upstream_entry_to_config(entry: &nestweaver_engine::UpstreamEntry) -> UpstreamConfig {
    let mode = match entry.mode.to_ascii_lowercase().as_str() {
        "merge" => RoutingMode::Merge,
        "primary" => RoutingMode::Primary,
        "fallback" => RoutingMode::Fallback,
        other => {
            warn!(
                mode = %other,
                upstream = %entry.name.as_deref().unwrap_or("upstream"),
                "unknown upstream mode — defaulting to fallback"
            );
            RoutingMode::Fallback
        }
    };
    UpstreamConfig {
        name: entry.name.clone(),
        url: entry.url.clone(),
        token: entry.token.as_deref().map(expand_env),
        mode,
        repos: entry.repos.clone(),
        timeout: entry.timeout.clone(),
        ca_cert: entry.ca_cert.clone(),
    }
}

/// Expand a `${VAR}` pattern using environment variables, mirroring the
/// client discovery layer's token expansion so `token = "${NW_TOKEN}"` in
/// `instance.toml` behaves identically on both connect paths.
fn expand_env(s: &str) -> String {
    if let Some(var) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        std::env::var(var).unwrap_or_else(|_| s.to_string())
    } else {
        s.to_string()
    }
}

/// The coordinator step for a `tools/call` result at the `/mcp` boundary.
///
/// Runs AFTER the local dispatch (the local tier is the already-computed
/// `local` value) and BEFORE envelope assembly. Returns the (possibly
/// federated) tool value plus the name of the upstream that contributed an
/// org-wide tier, when one did — the caller stamps envelope provenance from
/// that:
///
/// - `Some(name)` → scope `"federated"`, sources `["daemon", name]`
/// - `None` → the Phase A single-node stamp (honest degradation: the local
///   result is all a caller actually got)
///
/// Only `TwoTier`-routed tools federate here (see module docs for the Stage B
/// scoping rationale). The upstream call + adaptive timeout + ejection flow is
/// the shared [`nestweaver_federation::two_tier::two_tier_query`] machinery.
/// A repository-restricted caller is stopped before that machinery: the
/// configured service credential is not proof of the caller's upstream scope,
/// so the response retains the scoped local result and a generic withheld
/// org-tier status without making upstream I/O.
pub async fn federate_two_tier(
    fed: &FederationState,
    tool_name: &str,
    arguments: &Value,
    local: Value,
    visible: &VisibleRepos,
) -> (Value, Option<String>) {
    if !matches!(tool_routing(tool_name), ToolRouting::TwoTier) {
        return (local, None);
    }
    if matches!(visible, VisibleRepos::Only(_)) {
        // The configured upstream credential can be broader than the inbound
        // caller's repository scope. Until a caller-bound credential can be
        // forwarded and authorized upstream, attaching any org result would
        // cross the HTTP authorization boundary. Preserve the already-scoped
        // local result and disclose only a stable, count-free withheld status.
        return (
            serde_json::json!({
                "tier": "two_tier",
                "local_impact": local,
                "org_wide_impact": {
                    "status": "withheld",
                    "reason": "authorization-unproven",
                },
            }),
            None,
        );
    }
    if !fed.has_healthy_upstream() {
        // Upstream(s) configured but currently ejected: serve the local
        // result untouched with the single-node stamp rather than a two-tier
        // envelope with an empty org tier.
        debug!(tool = %tool_name, "no healthy upstream — serving local-only result");
        return (local, None);
    }

    debug!(tool = %tool_name, "federating two-tier query at /mcp boundary");
    let mut response = nestweaver_federation::two_tier::two_tier_query(
        local,
        &fed.upstreams,
        &fed.ejection_guard,
        tool_name,
        arguments,
    )
    .await;

    // The upstream only counts as a source when its org-wide tier actually
    // contributed results. `two_tier_query` degrades in-band: a failed or
    // timed-out upstream call yields `org_wide_impact.status = "unavailable"`
    // (and a raced ejection yields `tier: "local_only"` with no
    // `org_wide_impact` at all) — both keep the single-node stamp so the
    // envelope provenance never claims a source that did not answer.
    //
    // An upstream that answered but whose rows were ALL locally deduped
    // (`results` present but empty) still counts as a contributing source: the
    // org tier DID answer and confirmed no additional org-wide impact, which is
    // itself a meaningful, provenance-worthy verdict.
    let contributed = response
        .get("org_wide_impact")
        .map(|org| org.get("results").is_some())
        .unwrap_or(false);
    let source = if contributed {
        response
            .get("org_wide_impact")
            .and_then(|org| org.get("source_server"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    } else {
        None
    };

    // Provenance honesty on the daemon `/mcp` path. `two_tier_query` stamps the
    // INNER result `_meta` with UNPREFIXED provenance (`sources`/`scope`/
    // `stale_repos`) — correct for the CLIENT path, where `HybridClient`
    // returns that value directly. But on the daemon path the authoritative
    // OUTER envelope `_meta` is stamped separately with namespaced
    // `nestweaver.io/*` keys (`http.rs::add_provenance_metadata`). Shipping
    // both means two provenance representations, and in the upstream-
    // unavailable case the inner one still names the server as a source and
    // claims scope "hybrid" — contradicting the honest outer single-node stamp.
    // Strip the inner provenance keys so the namespaced outer `_meta` is the
    // single source of truth on `/mcp`.
    strip_inner_provenance_meta(&mut response);
    (response, source)
}

/// Remove the unprefixed provenance keys (`sources`, `scope`, `stale_repos`)
/// that [`nestweaver_federation::two_tier::two_tier_query`] injects into a
/// result's `_meta`. On the daemon `/mcp` path the namespaced outer envelope
/// `_meta` (`nestweaver.io/*`) is the single source of truth, so this inner
/// duplicate — which can contradict it when the upstream is unavailable — must
/// not survive. Surgical: only those three provenance keys are touched; any
/// other `_meta` a tool legitimately set (limits, `_clamped`, …) is preserved,
/// and an `_meta` object left empty by the removal is dropped entirely.
fn strip_inner_provenance_meta(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(meta) = obj.get_mut("_meta").and_then(Value::as_object_mut) else {
        return;
    };
    meta.remove("sources");
    meta.remove("scope");
    meta.remove("stale_repos");
    if meta.is_empty() {
        obj.remove("_meta");
    }
}

/// Spawn the background staleness/health maintenance task.
///
/// Mirrors the client's `maintenance_loop` (same 10s cadence): each tick
/// (1) re-probes ejected upstreams for active recovery — passive mark-down
/// without active re-probing would latch a downed upstream out forever — and
/// (2) refreshes the staleness verdict by diffing the daemon's own indexed
/// SHAs (read in-process from the store, the same fields the `RepoStates`
/// RPC serves) against each healthy upstream's `RepoStates`.
///
/// The task exits when the daemon's shutdown signal fires, following the
/// same `watch`-receiver pattern as the session/bucket sweepers.
pub fn spawn_staleness_refresher(
    store: Arc<GraphStore>,
    fed: Arc<FederationState>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let probes: Vec<MaintenanceProbe> = fed
            .upstreams
            .iter()
            .map(|u| MaintenanceProbe {
                name: u.name.clone(),
                client: u.client(),
                token: u.auth_token().map(String::from),
                health: u.health_ref(),
            })
            .collect();

        // Populate the verdict once up front so `stale_repos` is meaningful
        // without waiting a full interval after startup.
        refresh_stale_verdict(&store, &probes, &fed).await;

        loop {
            tokio::select! {
                _ = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
                _ = shutdown_rx.changed() => break,
            }
            run_recovery_tick(&probes).await;
            refresh_stale_verdict(&store, &probes, &fed).await;
        }
    });
}

/// Recompute the staleness verdict and publish it into the shared cell the
/// `/mcp` result path reads. Bounded only by the upstreams' own RPC timeouts —
/// this is background work, so a slow upstream delays the next verdict update
/// but never a user query.
async fn refresh_stale_verdict(
    store: &Arc<GraphStore>,
    probes: &[MaintenanceProbe],
    fed: &FederationState,
) {
    let Some(local_states) = local_repo_states(store).await else {
        // Store read failed — keep the previous verdict rather than blanking
        // it (staleness degrades to possibly-outdated, never to a lie of
        // freshness caused by a transient read error).
        return;
    };
    let stale = nestweaver_federation::health::compute_stale_repos(&local_states, probes).await;
    fed.set_stale_repos(stale);
}

/// Read the daemon's local repo states (repo URL → indexed SHA) directly from
/// the store — the same fields the gRPC `RepoStates` handler serves, without
/// the loopback RPC. Store reads are blocking, so this hops to the blocking
/// pool like every other store access on the async path.
async fn local_repo_states(store: &Arc<GraphStore>) -> Option<HashMap<String, String>> {
    let store = Arc::clone(store);
    let result = tokio::task::spawn_blocking(move || {
        store
            .list_repos(None)
            .map(|repos| {
                repos
                    .into_iter()
                    .map(|r| (r.url, r.indexed_sha))
                    .collect::<HashMap<String, String>>()
            })
            .map_err(|e| e.to_string())
    })
    .await;
    match result {
        Ok(Ok(states)) => Some(states),
        Ok(Err(e)) => {
            warn!(error = %e, "staleness refresh: reading local repo states failed");
            None
        }
        Err(e) => {
            warn!(error = %e, "staleness refresh: repo-state read task panicked");
            None
        }
    }
}

/// Re-probe every ejected upstream whose backoff window has elapsed and fold
/// the result into its health state (HAProxy rise/fall semantics — the active
/// half that pairs with the query path's passive mark-down).
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
    client: &nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient<
        tonic::transport::Channel,
    >,
    token: Option<&str>,
) -> bool {
    let mut c = client.clone();
    let mut req = tonic::Request::new(nestweaver_proto::HealthCheckRequest {});
    if let Some(t) = token
        && let Ok(val) = format!("Bearer {t}").parse::<tonic::metadata::MetadataValue<_>>()
    {
        req.metadata_mut().insert("authorization", val);
    }
    matches!(
        tokio::time::timeout(HEALTH_PROBE_TIMEOUT, c.health_check(req)).await,
        Ok(Ok(_))
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(mode: &str) -> nestweaver_engine::UpstreamEntry {
        // UpstreamEntry has no public constructor — round-trip through the
        // config parser, exactly the shape the daemon receives at runtime.
        let toml = format!(
            r#"
name = "acme"
url = "http://127.0.0.1:19999"
token = "nw_test"
mode = "{mode}"
repos = ["acme/*"]
timeout = "2s"
"#
        );
        toml::from_str(&toml).expect("valid upstream entry TOML")
    }

    #[test]
    fn upstream_entry_converts_field_for_field() {
        let cfg = upstream_entry_to_config(&entry("merge"));
        assert_eq!(cfg.name.as_deref(), Some("acme"));
        assert_eq!(cfg.url, "http://127.0.0.1:19999");
        assert_eq!(cfg.token.as_deref(), Some("nw_test"));
        assert_eq!(cfg.mode, RoutingMode::Merge);
        assert_eq!(cfg.repos, vec!["acme/*".to_string()]);
        assert_eq!(cfg.timeout, "2s");
        assert_eq!(cfg.ca_cert, None);
    }

    #[test]
    fn upstream_entry_mode_parsing() {
        assert_eq!(
            upstream_entry_to_config(&entry("primary")).mode,
            RoutingMode::Primary
        );
        assert_eq!(
            upstream_entry_to_config(&entry("fallback")).mode,
            RoutingMode::Fallback
        );
        assert_eq!(
            upstream_entry_to_config(&entry("MERGE")).mode,
            RoutingMode::Merge,
            "mode parsing is case-insensitive"
        );
        // Unknown modes degrade to the safe default rather than erroring.
        assert_eq!(
            upstream_entry_to_config(&entry("bogus")).mode,
            RoutingMode::Fallback
        );
    }

    #[test]
    fn expand_env_passes_through_literals() {
        assert_eq!(expand_env("nw_literal"), "nw_literal");
        assert_eq!(
            expand_env("${NW_DEFINITELY_UNSET_VAR_XYZ}"),
            "${NW_DEFINITELY_UNSET_VAR_XYZ}",
            "unset vars keep the original string, matching client discovery"
        );
    }

    fn state_with_upstream(url: &str) -> FederationState {
        let cfg = UpstreamConfig {
            name: Some("server".to_string()),
            url: url.to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "200ms".to_string(),
            ca_cert: None,
        };
        FederationState {
            upstreams: vec![UpstreamHandle::from_config(&cfg).expect("handle")],
            ejection_guard: Mutex::new(()),
            stale_repos: RwLock::new(Vec::new()),
        }
    }

    // `state_with_upstream` builds an `UpstreamHandle`, whose `connect_lazy`
    // channel construction requires a Tokio reactor — so this must be async.
    #[tokio::test]
    async fn stale_repos_cache_round_trips() {
        let fed = state_with_upstream("http://127.0.0.1:19999");
        assert!(fed.stale_repos().is_empty());
        fed.set_stale_repos(vec!["https://github.com/acme/api.git".to_string()]);
        assert_eq!(
            fed.stale_repos(),
            vec!["https://github.com/acme/api.git".to_string()]
        );
    }

    #[tokio::test]
    async fn federate_two_tier_skips_non_two_tier_tools() {
        // Merge/FanOut/etc. tools stay local-only in Stage B: the value passes
        // through untouched and no upstream source is reported, keeping the
        // single-node stamp honest.
        let fed = state_with_upstream("http://127.0.0.1:19999");
        let local = json!({ "results": [{"uid": "a"}] });
        let (value, source) = federate_two_tier(
            &fed,
            "brain_search",
            &json!({}),
            local.clone(),
            &VisibleRepos::All,
        )
        .await;
        assert_eq!(value, local, "non-two-tier tools must pass through");
        assert_eq!(source, None);
    }

    #[tokio::test]
    async fn federate_two_tier_serves_local_when_upstream_ejected() {
        // Configured-but-down upstream: honest degradation — the local value
        // is returned untouched (no two-tier wrapper) and no source is
        // claimed, so the boundary stamps single-node.
        let fed = state_with_upstream("http://127.0.0.1:19999");
        fed.upstreams[0].mark_unhealthy();
        let local = json!({ "changed_symbols": [] });
        let (value, source) = federate_two_tier(
            &fed,
            "blast_radius",
            &json!({}),
            local.clone(),
            &VisibleRepos::All,
        )
        .await;
        assert_eq!(value, local);
        assert_eq!(source, None);
    }

    #[tokio::test]
    async fn restricted_callers_withhold_every_two_tier_tool_without_upstream_io() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind request canary");
        let fed = state_with_upstream(&format!(
            "http://{}",
            listener.local_addr().expect("listener address")
        ));
        let visible = nestweaver_engine::authz::VisibleRepos::Only(Default::default());

        for tool_name in ["blast_radius", "brain_impact", "affected_tests"] {
            let local = json!({
                "tool": tool_name,
                "local_canary": "authorized-local-only"
            });
            let (value, source) =
                federate_two_tier(&fed, tool_name, &json!({}), local.clone(), &visible).await;

            assert_eq!(value["tier"], "two_tier");
            assert_eq!(value["local_impact"], local);
            assert_eq!(value["org_wide_impact"]["status"], "withheld");
            assert_eq!(value["org_wide_impact"]["reason"], "authorization-unproven");
            assert_eq!(
                value["org_wide_impact"].as_object().unwrap().len(),
                2,
                "withheld tier must be generic and count-free: {value}"
            );
            assert_eq!(source, None);
        }

        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "restricted TwoTier tools must not open an upstream connection"
        );
    }

    #[tokio::test]
    async fn federate_two_tier_unreachable_upstream_degrades_honestly() {
        // A healthy-looking handle whose endpoint is unreachable: the shared
        // two_tier machinery returns the two-tier envelope with an
        // `unavailable` org tier — the upstream must NOT be reported as a
        // source, so scope stays single-node.
        let fed = state_with_upstream("http://127.0.0.1:1");
        let local = json!({ "changed_symbols": [{"name": "f", "file_path": "src/a.rs"}] });
        let (value, source) = federate_two_tier(
            &fed,
            "brain_impact",
            &json!({"symbol": "f"}),
            local,
            &VisibleRepos::All,
        )
        .await;
        assert_eq!(source, None, "an unreachable upstream is not a source");
        // The envelope still discloses the degradation in-band.
        assert_eq!(value["tier"], json!("two_tier"));
        assert_eq!(value["org_wide_impact"]["status"], json!("unavailable"));

        // Provenance honesty (nw-017 fix): the INNER structuredContent._meta
        // provenance that `two_tier_query` injects must be stripped on the
        // daemon path, so it can't contradict the honest outer single-node
        // stamp. In the upstream-unavailable case the inner block would have
        // claimed sources `["local","server"]` and scope "hybrid" — none of
        // that may survive here.
        let meta = &value["_meta"];
        assert!(
            meta.get("sources").is_none(),
            "inner _meta must not name any source, got {meta:?}"
        );
        assert!(
            meta.get("scope").is_none(),
            "inner _meta must not claim a scope, got {meta:?}"
        );
        assert!(
            meta.get("stale_repos").is_none(),
            "inner _meta must not carry stale_repos, got {meta:?}"
        );
    }

    #[test]
    fn strip_inner_provenance_meta_removes_only_provenance_keys() {
        // Successful-federation shape: the two-tier envelope plus the inner
        // provenance `two_tier_query` injects, alongside a legitimate non-
        // provenance `_meta` entry a tool may have set (e.g. a limit clamp).
        let mut value = json!({
            "tier": "two_tier",
            "local_impact": { "changed_symbols": [] },
            "org_wide_impact": { "source_server": "acme", "results": [] },
            "_meta": {
                "sources": ["local", "acme"],
                "scope": "hybrid",
                "stale_repos": [],
                "_clamped": true,
                "limits": { "max_depth": 5 },
            },
        });
        strip_inner_provenance_meta(&mut value);

        // Outer two-tier contract is untouched.
        assert_eq!(value["tier"], json!("two_tier"));
        assert!(value["org_wide_impact"]["results"].is_array());
        // Provenance keys gone; non-provenance _meta preserved.
        let meta = &value["_meta"];
        assert!(meta.get("sources").is_none());
        assert!(meta.get("scope").is_none());
        assert!(meta.get("stale_repos").is_none());
        assert_eq!(meta["_clamped"], json!(true));
        assert_eq!(meta["limits"]["max_depth"], json!(5));
    }

    #[test]
    fn strip_inner_provenance_meta_drops_emptied_meta_object() {
        // When `_meta` held only provenance, removing it should leave no empty
        // `_meta` husk behind.
        let mut value = json!({
            "results": [],
            "_meta": { "sources": ["local"], "scope": "local", "stale_repos": [] },
        });
        strip_inner_provenance_meta(&mut value);
        assert!(
            value.get("_meta").is_none(),
            "an emptied _meta object must be removed entirely, got {value:?}"
        );
        assert!(value["results"].is_array(), "other fields must remain");
    }
}
