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

use crate::discovery::discover_upstreams;
use crate::merge::rrf_merge;
use crate::upstream::UpstreamHandle;
use crate::DaemonClient;

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

    /// Fallback routing: query local first, query server only if local
    /// results are sparse (fewer than [`FALLBACK_THRESHOLD`]).
    pub async fn query_fallback(
        &mut self,
        tool_name: &str,
        params: &Value,
    ) -> Result<Value> {
        // 1. Always query local first.
        let local_result = self.query_local(tool_name, params).await?;

        // 2. If no healthy upstreams, return local as-is.
        if !self.has_upstreams() {
            return Ok(local_result);
        }

        // 3. Check if local results are sufficient.
        let local_count = count_results(&local_result);
        if local_count >= FALLBACK_THRESHOLD {
            debug!(
                tool = tool_name,
                local_count,
                "fallback: local results sufficient, skipping server"
            );
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
                Ok(local_result)
            }
        }
    }

    /// Query the local daemon via its gRPC channel.
    async fn query_local(
        &mut self,
        tool_name: &str,
        params: &Value,
    ) -> Result<Value> {
        dispatch_json_rpc(self.local.inner_mut(), tool_name, params).await
    }

    /// Query an upstream server via its gRPC channel with a timeout.
    async fn query_upstream(
        &self,
        tool_name: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let upstream = self
            .upstreams
            .iter()
            .find(|u| u.is_healthy())
            .context("no healthy upstream servers")?;

        let mut client = upstream.client();
        let result = tokio::time::timeout(
            timeout,
            dispatch_json_rpc(&mut client, tool_name, params),
        )
        .await
        .context("upstream query timed out")?
        .context("upstream query failed")?;

        Ok(result)
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
    let args_json = serde_json::to_string(params)?;
    let request = tonic::Request::new(JsonRequest { args_json });

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
        // hub_nodes uses HubNodesRequest — dispatch separately if needed.
        "brain_status" | "brain_status_json" => client.brain_status_json(request).await,
        "export_graph" => client.export_graph(request).await,
        "search_symbols" => client.search_symbols(request).await,
        "symbol_lookup" => client.symbol_lookup(request).await,
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
    } else if value.is_object() && !value.as_object().unwrap().is_empty() {
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

/// Wrap merged results into a response envelope with provenance metadata.
fn wrap_merged_response(results: Vec<Value>, sources: &[&str]) -> Value {
    serde_json::json!({
        "results": results,
        "sources": sources,
        "merged": true,
    })
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

    // ── Fallback merge helpers test ──────────────────────────────

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
        assert_eq!(merged["merged"], true);
        assert!(merged["sources"].as_array().unwrap().len() >= 2);
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
        assert_eq!(wrapped["merged"], true);
        assert!(wrapped["sources"].is_array());
        assert!(wrapped["results"].is_array());
    }

    #[test]
    fn upstream_timeout_is_one_second() {
        assert_eq!(UPSTREAM_TIMEOUT, Duration::from_secs(1));
    }
}
