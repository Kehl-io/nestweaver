//! HTTP presentation adapter over the shared federation data plane.
//! The host supplies local tool execution; routing, identity reconciliation,
//! upstream dispatch, deadlines and two-tier composition remain shared.
use crate::state::AppState;
use axum::{Json, extract::State};
use nestweaver_federation::{
    discovery::{RoutingMode, UpstreamConfig},
    health::{
        effective_timeout_for_tool, eject_with_cap, handle_query_timeout_for_tool,
        is_upstream_down, log_tool_timeout,
    },
    results::{
        cap_brain_search_results, count_results, inject_or_wrap_provenance,
        merge_json_results_with_vault_mappings,
    },
    upstream::UpstreamHandle,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

pub type LocalQuery = Arc<
    dyn Fn(String, Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

pub enum LookupOutcome {
    Found { source: String, value: Value },
    Ambiguous { source: String, value: Value },
    Missing,
    Unavailable { sources: Vec<String> },
}
impl LookupOutcome {
    pub fn into_error(self) -> crate::error::ApiError {
        match self {
            Self::Ambiguous { source, mut value } => {
                value["source"] = json!(source);
                crate::error::ApiError::conflict_with_body("upstream lookup is ambiguous", value)
            }
            Self::Unavailable { sources } => crate::error::ApiError::unavailable(format!(
                "Upstream lookup unavailable or permission-rejected ({}); absence is unproven.",
                sources.join(", ")
            )),
            _ => crate::error::ApiError::not_found("entity not found"),
        }
    }
}

pub struct WebFederation {
    affinity: Mutex<HashMap<String, String>>,
    upstreams: Vec<UpstreamHandle>,
    #[cfg(test)]
    remote_override: Option<LocalQuery>,
    ejection_guard: Mutex<()>,
    local: LocalQuery,
    pub read_only: bool,
}
impl WebFederation {
    pub fn new(
        configs: &[UpstreamConfig],
        local: LocalQuery,
        read_only: bool,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            affinity: Mutex::new(HashMap::new()),
            #[cfg(test)]
            remote_override: None,
            upstreams: configs
                .iter()
                .map(UpstreamHandle::from_config)
                .collect::<anyhow::Result<_>>()?,
            ejection_guard: Mutex::new(()),
            local,
            read_only,
        })
    }
    pub fn is_configured(&self) -> bool {
        !self.upstreams.is_empty()
    }
    pub async fn local(&self, tool: &str, params: &Value) -> Result<Value, String> {
        (self.local)(tool.to_owned(), params.clone()).await
    }
    /// Query one upstream with the same connection/query budgets and health
    /// accounting used by the CLI. Error messages never expose credentials.
    async fn remote(
        &self,
        upstream: &UpstreamHandle,
        tool: &str,
        params: &Value,
    ) -> Result<Value, &'static str> {
        #[cfg(test)]
        if let Some(remote) = &self.remote_override {
            let mut params = params.clone();
            params["_test_peer"] = json!(upstream.name);
            return remote(tool.into(), params).await.map_err(|_| "unavailable");
        }
        if !upstream.is_healthy() {
            let health = upstream.health_ref();
            let now = nestweaver_federation::upstream::now_ms();
            if !health.probe_due(now) {
                return Err("unavailable");
            }
            let mut client = upstream.client();
            let probe = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                nestweaver_federation::dispatch::dispatch_json_rpc_authed(
                    &mut client,
                    "list_repos",
                    &json!({}),
                    upstream.auth_token(),
                ),
            )
            .await;
            health.apply_probe_result(now, matches!(probe, Ok(Ok(_))));
            if !upstream.is_healthy() {
                return Err("unavailable");
            }
        }
        if upstream.ensure_connected().await.is_err() {
            return Err("unavailable");
        }
        let timeout = effective_timeout_for_tool(upstream.mode, upstream, tool);
        let started = std::time::Instant::now();
        let mut client = upstream.client();
        match tokio::time::timeout(
            timeout,
            nestweaver_federation::dispatch::dispatch_json_rpc_authed(
                &mut client,
                tool,
                params,
                upstream.auth_token(),
            ),
        )
        .await
        {
            Ok(Ok(value)) => {
                upstream.record_tool_latency(tool, started.elapsed());
                Ok(value)
            }
            Ok(Err(error)) => {
                if is_upstream_down(&error) {
                    eject_with_cap(
                        upstream,
                        &self.upstreams,
                        &self.ejection_guard,
                        "web query failed",
                    );
                }
                Err("unavailable-or-permission-rejected")
            }
            Err(_) => {
                log_tool_timeout(upstream, tool, timeout, started.elapsed());
                handle_query_timeout_for_tool(
                    upstream.mode,
                    upstream,
                    &self.upstreams,
                    &self.ejection_guard,
                    tool,
                    timeout,
                    "web query timed out",
                );
                Err("timed-out")
            }
        }
    }
    pub async fn search(&self, tool: &str, params: &Value, local: Value, limit: usize) -> Value {
        let original_meta = local.get("_meta").cloned();
        let mut answer = local;
        annotate_rows(&mut answer, "local");
        let mut peer_truncated = answer["truncated"].as_bool().unwrap_or(false);
        let mut sources = vec!["local".to_owned()];
        let mut states = Vec::new();
        for upstream in &self.upstreams {
            if upstream.mode == RoutingMode::Fallback && count_results(&answer) > 0 {
                states.push(json!({"source":upstream.name,"status":"not-needed"}));
                continue;
            }
            match self.remote(upstream, tool, params).await {
                Ok(mut server) => {
                    peer_truncated |= server["truncated"].as_bool().unwrap_or(false);
                    annotate_rows(&mut server, &upstream.name);
                    for row in nestweaver_federation::results::extract_result_items(&server) {
                        if let Some(uid) = row["uid"].as_str() {
                            self.remember(uid, &upstream.name);
                        }
                    }
                    if upstream.mode == RoutingMode::Primary {
                        answer = server;
                        sources = vec![upstream.name.clone()];
                    } else {
                        answer = merge_json_results_with_vault_mappings(
                            &answer,
                            &server,
                            &upstream.vault_mappings,
                        );
                        sources.push(upstream.name.clone());
                    }
                    states.push(json!({"source":upstream.name,"status":"available"}));
                }
                Err(status) => states.push(json!({"source":upstream.name,"status":status})),
            }
        }
        let refs: Vec<_> = sources.iter().map(String::as_str).collect();
        inject_or_wrap_provenance(&mut answer, &refs, &[]);
        if let Some(meta) = original_meta {
            // Retain workspace trust/continuation metadata expected by the UI.
            if let Some(object) = meta.as_object() {
                for (key, value) in object {
                    answer["_meta"][key] = value.clone();
                }
            }
        }
        if tool == "brain_search" {
            cap_brain_search_results(&mut answer, limit);
        } else if let Some(rows) = answer.get_mut("results").and_then(Value::as_array_mut) {
            let before = rows.len();
            rows.truncate(limit);
            let returned = rows.len();
            answer["returned_matches"] = json!(returned);
            if returned < before {
                answer["truncated"] = json!(true);
            }
        }
        let truncated = peer_truncated || answer["truncated"].as_bool().unwrap_or(false);
        answer["truncated"] = json!(truncated);
        let partial = states
            .iter()
            .any(|s| !matches!(s["status"].as_str(), Some("available" | "not-needed")));
        answer["_federation"] = json!({"sources":sources,"upstreams":states,"partial":partial,"truncated":truncated,"freshness":"unknown","read_only":self.read_only,"unsupported":["remote-source-file","remote-edits","workspace-membership-mapping","uncommitted-overlay"]});
        if answer["_meta"].get("trust").is_some() {
            // P1 scene trust must describe the same contributing sources as
            // shared provenance, rather than retaining the local pre-merge stamp.
            let remote_contributed = sources.iter().any(|source| source != "local");
            answer["_meta"]["trust"]["federation"] = if remote_contributed {
                answer["_meta"]["scope"].clone()
            } else {
                json!("local-only")
            };
            answer["_meta"]["trust"]["capability"] = json!(if remote_contributed {
                "federated-read"
            } else if self.read_only {
                "read-only-replica"
            } else {
                "local-index"
            });
            answer["_meta"]["trust"]["freshness"] = json!("unknown");
            answer["_meta"]["trust"]["partial"] = json!(true);
            answer["_meta"]["trust"]["result"] = json!("partial");
            answer["_meta"]["trust"]["message"] = json!(
                "Federated search; upstream availability and unknown freshness are disclosed in federation metadata. Remote source files and edits are unsupported."
            );
        }
        answer
    }
    fn remember(&self, uid: &str, source: &str) {
        let mut affinity = self.affinity.lock().unwrap_or_else(|e| e.into_inner());
        if affinity.len() >= 4096 {
            affinity.clear();
        }
        affinity.insert(uid.to_owned(), source.to_owned());
    }
    pub fn owner(&self, uid: &str) -> Option<String> {
        self.affinity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uid)
            .cloned()
    }
    pub async fn lookup(&self, tool: &str, params: &Value) -> LookupOutcome {
        let uid = params["name_or_uid"]
            .as_str()
            .or_else(|| params["uid"].as_str());
        let owner = uid.and_then(|uid| self.owner(uid));
        let mut unavailable = Vec::new();
        for upstream in self
            .upstreams
            .iter()
            .filter(|upstream| owner.as_deref().is_none_or(|name| name == upstream.name))
        {
            match self.remote(upstream, tool, params).await {
                Ok(value) if value["status"] == "not_found" => {}
                Ok(value) if value["status"] == "ambiguous" => {
                    return LookupOutcome::Ambiguous {
                        source: upstream.name.clone(),
                        value,
                    };
                }
                Ok(value)
                    if value["status"] == "found"
                        || (tool == "note_get"
                            && value.get("uid").and_then(Value::as_str).is_some()) =>
                {
                    if let Some(uid) = uid {
                        self.remember(uid, &upstream.name);
                    }
                    return LookupOutcome::Found {
                        source: upstream.name.clone(),
                        value,
                    };
                }
                _ => unavailable.push(upstream.name.clone()),
            }
        }
        if unavailable.is_empty() {
            LookupOutcome::Missing
        } else {
            LookupOutcome::Unavailable {
                sources: unavailable,
            }
        }
    }
    pub async fn analysis(&self, tool: &str, params: &Value, preferred: Option<&str>) -> Value {
        let local = self
            .local(tool, params)
            .await
            .unwrap_or_else(|_| json!({"status":"unavailable","recommendation":"run-full-suite"}));
        #[cfg(test)]
        if self.remote_override.is_some() {
            let upstream = self
                .upstreams
                .iter()
                .find(|u| preferred.is_none_or(|name| name == u.name));
            return if let Some(upstream) = upstream {
                nestweaver_federation::two_tier::assemble_two_tier(
                    local,
                    self.remote(upstream, tool, params).await.ok(),
                    &upstream.name,
                )
            } else {
                local
            };
        }
        // Connection establishment has its own budget, before shared two-tier
        // orchestration starts the adaptive query timer.
        for upstream in &self.upstreams {
            if !upstream.is_healthy() {
                let _ = self.remote(upstream, "list_repos", &json!({})).await;
            }
            let _ = upstream.ensure_connected().await;
        }
        let mut result = nestweaver_federation::two_tier::two_tier_query_profile_preferred(
            local,
            &self.upstreams,
            &self.ejection_guard,
            tool,
            params,
            nestweaver_schema::ToolDeliveryProfile::FullCli,
            preferred,
        )
        .await;
        let source = result["org_wide_impact"]
            .get("results")
            .and_then(|_| result["org_wide_impact"]["source_server"].as_str())
            .map(str::to_owned);
        let mut sources = vec!["local"];
        if let Some(source) = source.as_deref() {
            sources.push(source);
        }
        inject_or_wrap_provenance(&mut result, &sources, &[]);
        result
    }
}

fn annotate_rows(value: &mut Value, source: &str) {
    let rows = if value.is_array() {
        value.as_array_mut()
    } else {
        value.get_mut("results").and_then(Value::as_array_mut)
    };
    if let Some(rows) = rows {
        for row in rows {
            if row.is_object() {
                row["federation_source"] = json!(source);
            }
        }
    }
}

pub async fn capabilities(State(state): State<Arc<AppState>>) -> Json<Value> {
    let configured = state.federation.get().is_some_and(|f| f.is_configured());
    let read_only = state.federation.get().is_some_and(|f| f.read_only)
        || state.vault_derivation.get().is_some_and(|v| v.read_only);
    Json(
        json!({"federated":configured,"read_only":read_only,"search":true,"impact":true,"affected_tests":true,"blast_radius":true,"freshness":"unknown","unsupported":["remote-source-file","remote-edits","workspace-membership-mapping","uncommitted-overlay","federated-overview","federated-context"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    fn config() -> UpstreamConfig {
        serde_json::from_value(
            json!({"name":"org","url":"http://127.0.0.1:1","mode":"merge","timeout":"50ms"}),
        )
        .unwrap()
    }
    fn local() -> LocalQuery {
        Arc::new(|_, _| Box::pin(async { Ok(json!({"status":"not_found","impact_nodes":[]})) }))
    }
    async fn response(state: Arc<AppState>, path: &str) -> Value {
        let response = crate::create_router(state)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        serde_json::from_slice(
            &to_bytes(response.into_body(), 8 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    fn state() -> (tempfile::TempDir, Arc<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&path).unwrap();
        (dir, AppState::new(store, None, path))
    }
    #[tokio::test]
    async fn remote_only_search_and_detail_use_existing_http_routes() {
        let (_dir, state) = state();
        let mut fed = WebFederation::new(&[config()], local(), true).unwrap();
        fed.remote_override = Some(Arc::new(|tool, _| {
            Box::pin(async move {
                Ok(match tool.as_str() {
                    "search_symbols" => {
                        json!([{"uid":"sym:remote","name":"RemoteOnly","kind":"function","file_path":"remote.rs","start_line":1}])
                    }
                    "symbol_lookup" => {
                        json!({"status":"found","detail":{"symbol":{"uid":"sym:remote","name":"RemoteOnly","kind":"Function","repo_uid":"repo:org:123456789012","file_path":"remote.rs","start_line":1,"end_line":3,"signature":"fn RemoteOnly()","content_hash":"x","is_entry_point":false,"visibility":"Inferred"},"callers":[],"callees":[]}})
                    }
                    "brain_search" => {
                        json!({"total_matches":1,"total_matches_relation":"eq","results":[{"uid":"note:remote","kind":"Note","title":"RemoteOnly","vault_uid":"vault:remote"}]})
                    }
                    "brain_impact" => {
                        json!({"status":"ok","target":"sym:remote","impact_nodes":[{"uid":"sym:caller","name":"RemoteCaller","file_path":"remote.rs","start_line":8,"depth":1,"confidence":1.0,"impact_score":0.8,"edge_type":"CALLS"}]})
                    }
                    "affected_tests" => {
                        json!({"changed_files":["remote.rs"],"changed_symbols":[],"tier_1":[],"tier_2":[],"tier_3":[],"summary":"No indexed tests","disclaimer":"Static hints","status":"Complete"})
                    }
                    "blast_radius" => {
                        json!({"status":"Complete","affected_symbols":[{"uid":"sym:caller","repo_uid":"repo:org:123456789012"}]})
                    }
                    "note_get" => {
                        json!({"uid":"note:remote","title":"RemoteOnly","path":"remote.md","body":"Remote note body","note_kind":"note","word_count":3})
                    }
                    _ => json!({"status":"not_found"}),
                })
            })
        }));
        assert!(state.federation.set(Arc::new(fed)).is_ok());
        let search = response(state.clone(), "/api/v1/search?q=RemoteOnly").await;
        assert_eq!(search["results"][0]["uid"], "sym:remote");
        assert_eq!(search["_federation"]["partial"], false);
        assert_eq!(search["_meta"]["sources"], search["_federation"]["sources"]);
        assert!(
            search["_meta"].get("trust").is_none(),
            "symbol search has shared provenance, not a stale P1 local scene"
        );
        let detail = response(state.clone(), "/api/v1/symbol/sym:remote").await;
        assert_eq!(detail["symbol"]["uid"], "sym:remote");
        assert_eq!(detail["_federation"]["source_available"], false);
        let notes = response(
            state.clone(),
            "/api/v1/brain/search?q=RemoteOnly&workspace=all",
        )
        .await;
        assert_eq!(notes["results"][0]["uid"], "note:remote");
        assert_eq!(
            notes["_meta"]["trust"]["federation"],
            notes["_meta"]["scope"]
        );
        assert_eq!(notes["_meta"]["trust"]["federation"], "hybrid");
        assert_eq!(notes["_meta"]["trust"]["capability"], "federated-read");
        assert_eq!(notes["_meta"]["sources"], notes["_federation"]["sources"]);
        let impact = response(state.clone(), "/api/v1/impact/sym:remote?workspace=all").await;
        assert_eq!(impact["target"]["uid"], "sym:remote");
        assert_eq!(impact["states"]["local"], "unavailable");
        assert_eq!(impact["states"]["org"], "available");
        assert_eq!(impact["nodes"][1]["role"], "org-impact");
        assert!(impact.get("org_affected_tests").is_some());
        assert!(
            impact["blast_radius"]["org_wide_impact"]
                .get("results")
                .is_some()
        );
        let note = response(state, "/api/v1/brain/note/note:remote").await;
        assert_eq!(note["body"], "Remote note body");
    }
    #[tokio::test]
    async fn unreachable_upstream_keeps_search_partial_and_capabilities_read_only() {
        let (_dir, state) = state();
        assert!(
            state
                .federation
                .set(Arc::new(
                    WebFederation::new(&[config()], local(), true).unwrap()
                ))
                .is_ok()
        );
        let search = response(state.clone(), "/api/v1/search?q=RemoteOnly").await;
        assert_eq!(search["_federation"]["partial"], true);
        assert_eq!(search["_federation"]["sources"], json!(["local"]));
        assert_eq!(search["results"], json!([]));
        let notes = response(
            state.clone(),
            "/api/v1/brain/search?q=RemoteOnly&workspace=all",
        )
        .await;
        assert_eq!(notes["_meta"]["trust"]["federation"], "local-only");
        assert_eq!(notes["_meta"]["trust"]["capability"], "read-only-replica");
        assert_eq!(notes["_meta"]["sources"], json!(["local"]));
        let impact = response(state.clone(), "/api/v1/impact/sym:missing?workspace=all").await;
        assert_eq!(impact["states"]["org"], "org-unavailable");
        assert_eq!(impact["_meta"]["trust"]["partial"], true);
        let caps = response(state, "/api/v1/capabilities").await;
        assert_eq!(caps["federated"], true);
        assert_eq!(caps["read_only"], true);
    }
    #[tokio::test]
    async fn local_search_preserves_legacy_array_without_upstreams() {
        let (_dir, state) = state();
        assert!(response(state, "/api/v1/search?q=missing").await.is_array());
    }
    async fn status(state: Arc<AppState>, path: &str) -> axum::http::StatusCode {
        crate::create_router(state)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }
    #[tokio::test]
    async fn clean_miss_ambiguity_and_outage_are_distinct_http_outcomes() {
        for (reply, expected) in [
            (Ok(json!({"status":"not_found"})), 404),
            (
                Ok(json!({"status":"ambiguous","candidates":[{"uid":"sym:a"},{"uid":"sym:b"}]})),
                409,
            ),
            (Err("unavailable".to_owned()), 503),
        ] {
            let (_dir, state) = state();
            let mut fed = WebFederation::new(&[config()], local(), true).unwrap();
            fed.remote_override = Some(Arc::new(move |_, _| {
                let reply = reply.clone();
                Box::pin(async move { reply })
            }));
            assert!(state.federation.set(Arc::new(fed)).is_ok());
            for path in [
                "/api/v1/symbol/sym:remote",
                "/api/v1/brain/note/note:remote",
            ] {
                assert_eq!(status(state.clone(), path).await.as_u16(), expected);
            }
        }
    }
    #[tokio::test]
    async fn two_peer_owner_affinity_covers_all_analyses_and_owner_outage() {
        let mut a = config();
        a.name = Some("A".into());
        let mut b = config();
        b.name = Some("B".into());
        let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let mut fed = WebFederation::new(&[a, b], local(), true).unwrap();
        let calls2 = calls.clone();
        let down2 = down.clone();
        fed.remote_override = Some(Arc::new(move |tool, params| {
            let peer = params["_test_peer"].as_str().unwrap().to_owned();
            calls2.lock().unwrap().push((peer.clone(), tool.clone()));
            let unavailable = peer == "B" && down2.load(std::sync::atomic::Ordering::Relaxed);
            Box::pin(async move {
                if unavailable {
                    return Err("offline".into());
                }
                if peer == "A" {
                    return Ok(json!({"status":"not_found"}));
                }
                Ok(match tool.as_str() {
                    "symbol_lookup" => {
                        json!({"status":"found","detail":{"symbol":{"uid":"sym:remote","name":"RemoteOnly","kind":"Function","repo_uid":"repo:org:123456789012","file_path":"remote.rs","start_line":1,"end_line":3,"signature":"fn RemoteOnly()","content_hash":"x","is_entry_point":false,"visibility":"Inferred"},"callers":[],"callees":[]}})
                    }
                    "brain_impact" => {
                        json!({"status":"ok","impact_nodes":[{"uid":"sym:caller","name":"Caller","file_path":"remote.rs","start_line":8,"depth":1,"confidence":1.0,"impact_score":0.8,"edge_type":"CALLS"}]})
                    }
                    "affected_tests" => json!({"tier_1":[{"symbol_uid":"sym:test"}]}),
                    "blast_radius" => json!({"affected_symbols":[{"uid":"sym:caller"}]}),
                    _ => json!({"status":"not_found"}),
                })
            })
        }));
        assert!(
            matches!(fed.lookup("symbol_lookup",&json!({"name_or_uid":"sym:remote"})).await,LookupOutcome::Found{source,..} if source=="B")
        );
        let owner = fed.owner("sym:remote").unwrap();
        let (_dir, state) = state();
        assert!(state.federation.set(Arc::new(fed)).is_ok());
        let fed = state.federation.get().unwrap();
        calls.lock().unwrap().clear();
        let lens = response(state.clone(), "/api/v1/impact/sym:remote?workspace=all").await;
        assert_eq!(lens["states"]["org"], "available");
        assert_eq!(lens["org_impact"]["org_wide_impact"]["source_server"], "B");
        assert_eq!(lens["nodes"][1]["uid"], "sym:caller");
        assert_eq!(
            lens["org_affected_tests"]["tier_1"][0]["symbol_uid"],
            "sym:test"
        );
        assert_eq!(
            lens["blast_radius"]["org_wide_impact"]["source_server"],
            "B"
        );
        for tool in ["brain_impact", "affected_tests", "blast_radius"] {
            let result = fed
                .analysis(
                    tool,
                    &json!({"symbol":"sym:remote","changed_files":["remote.rs"]}),
                    Some(&owner),
                )
                .await;
            assert_eq!(result["org_wide_impact"]["source_server"], "B");
            assert!(result["org_wide_impact"].get("results").is_some());
        }
        assert!(calls.lock().unwrap().iter().all(|(peer, _)| peer == "B"));
        down.store(true, std::sync::atomic::Ordering::Relaxed);
        let unavailable_lens =
            response(state.clone(), "/api/v1/impact/sym:remote?workspace=all").await;
        assert_eq!(unavailable_lens["states"]["org"], "org-unavailable");
        assert_eq!(unavailable_lens["_meta"]["trust"]["partial"], true);
        assert_eq!(
            status(state.clone(), "/api/v1/symbol/sym:remote")
                .await
                .as_u16(),
            503
        );
        assert!(
            matches!(fed.lookup("symbol_lookup",&json!({"name_or_uid":"sym:remote"})).await,LookupOutcome::Unavailable{sources} if sources==vec!["B"])
        );
        for tool in ["brain_impact", "affected_tests", "blast_radius"] {
            let result = fed.analysis(tool, &json!({}), Some(&owner)).await;
            assert_eq!(result["org_wide_impact"]["source_server"], "B");
            assert_eq!(result["org_wide_impact"]["status"], "unavailable");
        }
    }
    #[tokio::test]
    async fn merged_cap_preserves_returned_totals_and_peer_truncation() {
        let tier = |instance: &str, truncated: bool| {
            json!({"query":"run","total_matches":2,"returned_matches":2,"total_matches_relation":"eq","truncated":truncated,"results":[
            {"uid":format!("sym:repo:{instance}:aaaaaaaaaaaa:src/a.rs:runA:1"),"canonical_id":"aaaaaaaaaaaa:src/a.rs#runA:scope-a","kind":"Symbol/Function","title":"runA"},
            {"uid":format!("sym:repo:{instance}:bbbbbbbbbbbb:src/b.rs:runB:1"),"canonical_id":"bbbbbbbbbbbb:src/b.rs#runB:scope-b","kind":"Symbol/Function","title":"runB"}]})
        };
        let mut fed = WebFederation::new(&[config()], local(), true).unwrap();
        let mut remote = tier("org", true);
        // Distinct repository identities make all four rows legitimate.
        remote["results"][0]["canonical_id"] = json!("cccccccccccc:src/c.rs#runC:scope-c");
        remote["results"][0]["uid"] = json!("sym:repo:org:cccccccccccc:src/c.rs:runC:1");
        remote["results"][1]["canonical_id"] = json!("dddddddddddd:src/d.rs#runD:scope-d");
        remote["results"][1]["uid"] = json!("sym:repo:org:dddddddddddd:src/d.rs:runD:1");
        fed.remote_override = Some(Arc::new(move |_, _| {
            let remote = remote.clone();
            Box::pin(async move { Ok(remote) })
        }));
        let result = fed
            .search(
                "brain_search",
                &json!({"query":"run"}),
                tier("local", false),
                2,
            )
            .await;
        assert_eq!(result["results"].as_array().unwrap().len(), 2);
        assert_eq!(result["returned_matches"], 2);
        assert!(result["total_matches"].as_u64().unwrap() >= 2);
        assert!(matches!(
            result["total_matches_relation"].as_str(),
            Some("eq" | "gte")
        ));
        assert_eq!(result["truncated"], true);
        assert_eq!(result["_federation"]["truncated"], true);
        let result = fed
            .search("search_symbols", &json!({"query":"run"}), json!([]), 1)
            .await;
        assert_eq!(result["returned_matches"], 1);
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["_federation"]["truncated"], true);
    }
    #[tokio::test]
    async fn impact_affected_tests_receives_only_supported_arguments_on_both_tiers() {
        let (_dir, state) = state();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let assert_test_args = |params: &Value| {
            let keys: Vec<_> = params
                .as_object()
                .unwrap()
                .keys()
                .filter(|key| key.as_str() != "_test_peer")
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                vec!["changed_files"],
                "affected_tests schema rejects blast_radius limit/max_depth"
            );
            assert_eq!(params["changed_files"], json!(["remote.js"]));
        };
        let local_calls = calls.clone();
        let local: LocalQuery = Arc::new(move |tool, params| {
            if tool == "affected_tests" {
                assert_test_args(&params);
                local_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Box::pin(async { Ok(json!({"status":"not_found"})) })
        });
        let mut fed = WebFederation::new(&[config()], local, true).unwrap();
        let remote_calls = calls.clone();
        fed.remote_override = Some(Arc::new(move |tool, params| {
            if tool == "affected_tests" {
                assert_test_args(&params);
                remote_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Box::pin(async move {
                Ok(match tool.as_str() {
                    "symbol_lookup" => {
                        json!({"status":"found","detail":{"symbol":{"uid":"sym:remote","name":"RemoteOnly","kind":"Function","repo_uid":"repo:org:123456789012","file_path":"remote.js","start_line":1,"end_line":3,"signature":"fn RemoteOnly()","content_hash":"x","is_entry_point":false,"visibility":"Inferred"}}})
                    }
                    "brain_impact" => json!({"status":"ok","impact_nodes":[]}),
                    "affected_tests" => {
                        json!({"status":"complete","tier_1":[],"tier_2":[],"tier_3":[]})
                    }
                    "blast_radius" => json!({"status":"complete","affected_symbols":[]}),
                    _ => json!({"status":"not_found"}),
                })
            })
        }));
        assert!(state.federation.set(Arc::new(fed)).is_ok());
        let result = response(
            state,
            "/api/v1/impact/sym:remote?workspace=all&limit=17&depth=4",
        )
        .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert!(
            result["affected_test_tiers"]["org_wide_impact"]
                .get("results")
                .is_some()
        );
    }
}
