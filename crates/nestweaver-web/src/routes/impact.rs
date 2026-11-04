use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use nestweaver_engine::affected_tests::{AffectedTestsResult, affected_tests};
use nestweaver_schema::Symbol;
use nestweaver_store::ImpactNode;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::rank_events::with_rank_event;
use crate::routes::workspaces::{self, P1Meta, P1Provenance, ResolvedWorkspace, WorkspaceKind};
use crate::state::AppState;

#[derive(Clone, Deserialize)]
pub struct ImpactParams {
    pub depth: Option<u32>,
    pub confidence: Option<f32>,
    pub workspace: Option<String>,
    pub scope: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
struct ImpactSourceEvidence {
    file_path: String,
    start_line: u32,
    url: String,
}

#[derive(Serialize)]
struct ImpactGraphNode {
    uid: String,
    name: String,
    file_path: String,
    start_line: u32,
    layer: u32,
    role: String,
    confidence: f32,
    impact_score: f64,
    edge_type: Option<String>,
    source: ImpactSourceEvidence,
}

#[derive(Clone, Serialize)]
struct ImpactGraphEdge {
    source: String,
    target: String,
    edge_type: String,
    confidence: f32,
    source_layer: u32,
    target_layer: u32,
}

#[derive(Serialize)]
struct ImpactStates {
    tier: &'static str,
    local: &'static str,
    org: &'static str,
    freshness: &'static str,
    timeout: &'static str,
    permission: &'static str,
    read_only: &'static str,
    result: String,
}

#[derive(Serialize)]
struct ImpactResponse {
    target: Option<ImpactGraphNode>,
    nodes: Vec<ImpactGraphNode>,
    edges: Vec<ImpactGraphEdge>,
    affected_tests: AffectedTestsResult,
    states: ImpactStates,
    #[serde(rename = "_meta")]
    meta: P1Meta,
}

pub async fn impact(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
    Query(params): Query<ImpactParams>,
) -> Result<Response, ApiError> {
    let state2 = state.clone();
    let uid2 = uid.clone();
    let params2 = params.clone();
    let local = with_rank_event(&state, move || impact_response(&state2, &uid2, &params2)).await;
    let Some(fed) = state.federation.get().filter(|f| f.is_configured()) else {
        return Ok(Json(local?).into_response());
    };
    let selected =
        workspaces::workspace_param(params.workspace.as_deref(), params.scope.as_deref());
    if selected.is_some_and(|id| id != "all") {
        return Ok(Json(local?).into_response());
    }
    let local_available = local.is_ok();
    let mut value = match local {
        Ok(local) => serde_json::to_value(local)?,
        Err(error) if error.status == axum::http::StatusCode::NOT_FOUND => {
            let workspace = workspaces::resolve_workspace(&state.store, Some("all"))?;
            let meta = workspaces::p1_meta_for_result_set(
                &workspace,
                "partial",
                vec!["remote-source-file"],
                Vec::new(),
                params.limit,
                0,
                None,
            );
            serde_json::json!({"target":null,"nodes":[],"edges":[],"affected_tests":empty_affected_tests(Vec::new(),"Local target unavailable; inspect the org-wide tier."),"states":{},"_meta":meta})
        }
        Err(error) => return Err(error),
    };
    // Ownership established by search/detail is retained even during an
    // outage: another peer's clean miss cannot stand in for the owning peer.
    let mut owner = fed.owner(&uid);
    if value["target"].is_null() {
        match fed
            .lookup("symbol_lookup", &serde_json::json!({"name_or_uid":uid}))
            .await
        {
            crate::federation::LookupOutcome::Found {
                source,
                value: detail,
            } => {
                owner = Some(source);
                if let Ok(symbol) =
                    serde_json::from_value::<Symbol>(detail["detail"]["symbol"].clone())
                {
                    value["target"] = serde_json::to_value(target_graph_node(&symbol))?;
                    value["nodes"] = serde_json::json!([target_graph_node(&symbol)]);
                }
            }
            crate::federation::LookupOutcome::Ambiguous { source, value } => {
                return Err(
                    crate::federation::LookupOutcome::Ambiguous { source, value }.into_error(),
                );
            }
            crate::federation::LookupOutcome::Missing => {
                return Err(ApiError::not_found(format!("symbol '{uid}' not found")));
            }
            crate::federation::LookupOutcome::Unavailable { sources } => {
                if owner.is_none() && sources.len() == 1 {
                    owner = sources.first().cloned();
                }
            }
        }
    }
    let args = serde_json::json!({"symbol":uid,"depth":params.depth.unwrap_or(3).min(15),"confidence":params.confidence.unwrap_or(0.3).clamp(0.0,1.0),"limit":params.limit.unwrap_or(250).clamp(1,1000)});
    let impact = fed.analysis("brain_impact", &args, owner.as_deref()).await;
    let file = value["target"]["file_path"].as_str().map(str::to_owned);
    let (tests, blast) = if let Some(file) = file {
        // affected_tests rejects unknown fields; traversal controls belong
        // only to blast_radius, not to the test-selection request.
        let test_args = serde_json::json!({"changed_files":[file]});
        let blast_args = serde_json::json!({"changed_files":[file],"limit":params.limit.unwrap_or(250).clamp(1,1000),"max_depth":params.depth.unwrap_or(3).min(15)});
        tokio::join!(
            fed.analysis("affected_tests", &test_args, owner.as_deref()),
            fed.analysis("blast_radius", &blast_args, owner.as_deref())
        )
    } else {
        let unavailable =
            serde_json::json!({"status":"unavailable","recommendation":"run-full-suite"});
        (unavailable.clone(), unavailable)
    };
    // Keep graph node presentation in this adapter. Shared two-tier composition
    // retains the authoritative local/org separation and repo reconciliation.
    if let Some(rows) = impact["org_wide_impact"]["results"]["impact_nodes"].as_array() {
        let nodes = value["nodes"]
            .as_array_mut()
            .expect("impact nodes are an array");
        for row in rows {
            if let (Some(uid), Some(name), Some(path)) = (
                row["uid"].as_str(),
                row["name"].as_str(),
                row["file_path"].as_str(),
            ) {
                let mut node = ImpactGraphNode {
                    uid: uid.into(),
                    name: name.into(),
                    file_path: path.into(),
                    start_line: row["start_line"].as_u64().unwrap_or(1) as u32,
                    layer: row["depth"].as_u64().unwrap_or(1) as u32,
                    role: String::new(),
                    confidence: row["confidence"].as_f64().unwrap_or(0.0) as f32,
                    impact_score: row["impact_score"].as_f64().unwrap_or(0.0),
                    edge_type: row["edge_type"].as_str().map(str::to_owned),
                    source: source_evidence(
                        path,
                        row["start_line"].as_u64().unwrap_or(1) as u32,
                        uid,
                    ),
                };
                node.role = "org-impact".into();
                if !nodes.iter().any(|existing| existing["uid"] == node.uid) {
                    nodes.push(serde_json::to_value(node)?);
                }
            }
        }
    }
    let available = impact["org_wide_impact"]
        .get("results")
        .is_some_and(|result| {
            !matches!(
                result["status"].as_str(),
                Some("not_found" | "unavailable" | "ambiguous")
            )
        });
    if let Some(org_tests) = tests["org_wide_impact"].get("results") {
        // The local field remains local. Org tests are rendered separately by
        // the UI; a missing local target never claims an empty safe test set.
        value["org_affected_tests"] = org_tests.clone();
    }
    value["org_impact"] = impact.clone();
    value["blast_radius"] = blast;
    value["affected_test_tiers"] = tests;
    value["states"] = serde_json::json!({"tier":"two-tier","local":if local_available {"available"} else {"unavailable"},"org":if available {"available"} else {"org-unavailable"},"freshness":"unknown","timeout":"unknown","permission":"unknown","read_only":if fed.read_only {"read-only"} else {"local-writes-only"},"result":"partial"});
    value["_meta"]["trust"]["federation"] = serde_json::json!("two-tier");
    value["_meta"]["trust"]["data_scope"] = serde_json::json!("local-and-org");
    value["_meta"]["trust"]["freshness"] = serde_json::json!("unknown");
    value["_meta"]["trust"]["partial"] = serde_json::json!(true);
    value["_meta"]["trust"]["result"] = serde_json::json!("partial");
    value["_meta"]["trust"]["capability"] =
        serde_json::json!(if fed.read_only { "read-only" } else { "read" });
    value["_meta"]["trust"]["unsupported"] =
        serde_json::json!(["remote-source-file", "remote-edits", "remote-layered-edges"]);
    value["_meta"]["trust"]["message"] = serde_json::json!(if available {
        "Local and org impact are shown separately. Freshness is unknown; remote source files, edits and layered edges are unsupported. Inspect blast radius and both affected-test tiers before narrowing tests."
    } else {
        "Org impact is unavailable or permission-rejected. Local results are partial; run the full test suite. Freshness is unknown."
    });
    if available && let Some(provenance) = value["_meta"]["provenance"].as_array_mut() {
        provenance.push(serde_json::json!({"source":impact["org_wide_impact"]["source_server"],"detail":"org-wide reverse impact; freshness unknown"}));
    }
    Ok(Json(value).into_response())
}

fn impact_response(
    state: &Arc<AppState>,
    uid: &str,
    params: &ImpactParams,
) -> Result<ImpactResponse, ApiError> {
    let confidence = params.confidence.unwrap_or(0.3).clamp(0.0, 1.0);
    let depth = params.depth.unwrap_or(3).min(20);
    let limit = params.limit.unwrap_or(250).clamp(1, 1000);
    let workspace = workspaces::resolve_workspace(
        &state.store,
        workspaces::workspace_param(params.workspace.as_deref(), params.scope.as_deref()),
    )?;

    // Nonexistent symbols are a 404 regardless of scope; the vault
    // unsupported envelope is reserved for symbols that exist.
    let target = state
        .store
        .lookup_symbol(uid)
        .map_err(|_| ApiError::not_found(format!("symbol '{uid}' not found")))?;

    if workspace.kind == WorkspaceKind::Vault {
        return Ok(unsupported_workspace_response(&workspace, limit));
    }

    let Some(scope) = ImpactScope::resolve(state, &workspace)? else {
        return Ok(unsupported_workspace_response(&workspace, limit));
    };
    if !scope.contains(&target) {
        return Ok(out_of_scope_response(&workspace, &target, limit));
    }

    let impact_nodes: Vec<ImpactNode> = state
        .store
        .impact(uid, depth, confidence)?
        .into_iter()
        .filter(|node| scope.contains_impact_node(node))
        .collect();
    let affected_tests = scoped_affected_tests(state, &scope, &target)?;
    let total_node_count = impact_nodes.len() + 1;
    let returned_impact_nodes: Vec<ImpactNode> = impact_nodes
        .iter()
        .take(limit.saturating_sub(1))
        .cloned()
        .collect();
    let returned_count = returned_impact_nodes.len() + 1;
    let stale = target_repo_is_stale(state, &target)?;
    let result_state = if total_node_count > returned_count {
        "truncated"
    } else if impact_nodes.is_empty() {
        "no-match"
    } else if stale || workspace_has_unsupported_upstream() {
        "partial"
    } else {
        "complete"
    };

    let mut meta = workspaces::p1_meta_for_result_set(
        &workspace,
        result_state,
        unsupported_capabilities(),
        vec![
            P1Provenance::local_graph_store("confidence-weighted impact traversal"),
            P1Provenance::local_graph_store("static affected-test hints"),
        ],
        Some(limit),
        returned_count,
        Some(total_node_count),
    );
    meta.trust.freshness = if stale { "stale" } else { "current" }.to_string();
    meta.trust.partial = meta.trust.result == "partial" || !meta.trust.unsupported.is_empty();
    meta.trust.message = impact_trust_message(&workspace, stale, &meta.trust.result);

    let target_node = target_graph_node(&target);
    let mut nodes = Vec::with_capacity(returned_count);
    nodes.push(target_graph_node(&target));
    nodes.extend(returned_impact_nodes.iter().map(impact_graph_node));
    let edges = build_layered_edges(state, &target.uid, &returned_impact_nodes, confidence)?;
    let states = ImpactStates {
        tier: "local-only",
        local: "available",
        org: "org-unavailable",
        freshness: if stale { "stale" } else { "current" },
        timeout: "unknown",
        permission: "unknown",
        read_only: "unknown",
        result: meta.trust.result.clone(),
    };

    Ok(ImpactResponse {
        target: Some(target_node),
        nodes,
        edges,
        affected_tests,
        states,
        meta,
    })
}

struct ImpactScope {
    allowed_symbol_uids: Option<HashSet<String>>,
}

impl ImpactScope {
    fn resolve(
        state: &Arc<AppState>,
        workspace: &ResolvedWorkspace,
    ) -> Result<Option<Self>, ApiError> {
        match workspace.kind {
            WorkspaceKind::All => Ok(Some(Self {
                allowed_symbol_uids: None,
            })),
            WorkspaceKind::Repo => {
                let repo_uid = workspace.uid.as_deref().unwrap_or_default();
                Ok(Some(Self {
                    allowed_symbol_uids: Some(
                        workspaces::symbols_for_repo(&state.store, repo_uid)?
                            .into_iter()
                            .map(|symbol| symbol.uid)
                            .collect(),
                    ),
                }))
            }
            WorkspaceKind::Project => {
                let project_uid = workspace.uid.as_deref().unwrap_or_default();
                Ok(Some(Self {
                    allowed_symbol_uids: Some(
                        workspaces::symbols_for_project(&state.store, project_uid)?
                            .into_iter()
                            .map(|symbol| symbol.uid)
                            .collect(),
                    ),
                }))
            }
            WorkspaceKind::Vault => Ok(None),
        }
    }

    fn contains(&self, symbol: &Symbol) -> bool {
        self.allowed_symbol_uids
            .as_ref()
            .is_none_or(|uids| uids.contains(&symbol.uid))
    }

    fn contains_uid(&self, uid: &str) -> bool {
        self.allowed_symbol_uids
            .as_ref()
            .is_none_or(|uids| uids.contains(uid))
    }

    fn contains_impact_node(&self, node: &ImpactNode) -> bool {
        self.contains_uid(&node.uid)
    }
}

fn workspace_has_unsupported_upstream() -> bool {
    true
}

fn unsupported_capabilities() -> Vec<&'static str> {
    vec![
        "org-wide-impact",
        "two-tier-impact",
        "upstream-federation",
        "timeout-state",
        "permission-state",
        "read-only-state",
    ]
}

fn impact_trust_message(workspace: &ResolvedWorkspace, stale: bool, result: &str) -> String {
    match result {
        "no-match" => format!(
            "{} impact has no scoped reverse-dependency matches. Org-wide/two-tier continuation and upstream timeout/permission/read-only states are unavailable in this route.",
            workspace.label
        ),
        "truncated" => format!(
            "{} impact is truncated to the requested limit and local-only. Org-wide/two-tier continuation and upstream timeout/permission/read-only states are unavailable in this route.",
            workspace.label
        ),
        _ if stale => format!(
            "{} impact is local-only and stale. Org-wide/two-tier continuation and upstream timeout/permission/read-only states are unavailable in this route.",
            workspace.label
        ),
        _ => format!(
            "{} impact is local-only and partial. Org-wide/two-tier continuation and upstream timeout/permission/read-only states are unavailable in this route.",
            workspace.label
        ),
    }
}

fn empty_affected_tests(
    changed_files: Vec<String>,
    summary: impl Into<String>,
) -> AffectedTestsResult {
    AffectedTestsResult {
        changed_files,
        changed_symbols: Vec::new(),
        tier_1: Vec::new(),
        tier_2: Vec::new(),
        tier_3: Vec::new(),
        summary: summary.into(),
        disclaimer: "Static affected-test hints are unavailable for this scoped impact response."
            .to_string(),
        // NOT Complete: an empty test set here means "we couldn't compute hints",
        // not "no tests are affected". Reporting Complete would read as "safe to
        // skip all tests" — exactly the false-safe the trust core (D3) forbids.
        status: nestweaver_engine::AnalysisStatus::Degraded,
        resolver_stale_repos: Vec::new(),
        // Degraded hints => fail-safe widening (nw-060): CI must not narrow.
        recommendation: "run-full-suite".to_string(),
        measured: None,
        notifications: vec![nestweaver_engine::Notification {
            level: nestweaver_engine::NotificationLevel::Warning,
            message: "Affected-test hints are unavailable for this scoped impact response; \
                      the empty result does not mean no tests are affected."
                .to_string(),
            descriptor: "affected-tests.unavailable".to_string(),
        }],
    }
}

fn scoped_affected_tests(
    state: &Arc<AppState>,
    scope: &ImpactScope,
    target: &Symbol,
) -> Result<AffectedTestsResult, ApiError> {
    let mut result = affected_tests(&state.store, std::slice::from_ref(&target.file_path))
        .map_err(ApiError::from)?;
    if scope.allowed_symbol_uids.is_none() {
        return Ok(result);
    }

    result
        .changed_symbols
        .retain(|symbol| scope.contains_uid(&symbol.uid));
    result
        .tier_1
        .retain(|test| scope.contains_uid(&test.symbol_uid));
    result
        .tier_2
        .retain(|test| scope.contains_uid(&test.symbol_uid));
    result
        .tier_3
        .retain(|test| scope.contains_uid(&test.symbol_uid));
    result.summary = format!(
        "{} tier-1, {} tier-2, {} tier-3 tests affected in the selected workspace",
        result
            .tier_1
            .iter()
            .map(|file| file.test_count())
            .sum::<usize>(),
        result
            .tier_2
            .iter()
            .map(|file| file.test_count())
            .sum::<usize>(),
        result
            .tier_3
            .iter()
            .map(|file| file.test_count())
            .sum::<usize>(),
    );
    Ok(result)
}

fn unsupported_workspace_response(workspace: &ResolvedWorkspace, limit: usize) -> ImpactResponse {
    let mut meta = workspaces::p1_meta_for_result_set(
        workspace,
        "unsupported",
        unsupported_capabilities(),
        vec![P1Provenance::local_graph_store(
            "vault workspaces contain notes, not symbol impact traversals",
        )],
        Some(limit),
        0,
        Some(0),
    );
    meta.trust.freshness = "unknown".to_string();
    meta.trust.partial = true;
    meta.trust.message = format!(
        "{} is a vault workspace; symbol impact traversal is unsupported for vault-scoped results.",
        workspace.label
    );

    ImpactResponse {
        target: None,
        nodes: Vec::new(),
        edges: Vec::new(),
        affected_tests: empty_affected_tests(
            Vec::new(),
            "No affected-test hints: vault workspaces have no scoped symbols.",
        ),
        states: ImpactStates {
            tier: "unsupported",
            local: "unsupported",
            org: "not-applicable",
            freshness: "unknown",
            timeout: "not-applicable",
            permission: "not-applicable",
            read_only: "not-applicable",
            result: meta.trust.result.clone(),
        },
        meta,
    }
}

fn out_of_scope_response(
    workspace: &ResolvedWorkspace,
    target: &Symbol,
    limit: usize,
) -> ImpactResponse {
    let mut meta = workspaces::p1_meta_for_result_set(
        workspace,
        "no-match",
        unsupported_capabilities(),
        vec![P1Provenance::local_graph_store(
            "workspace-scoped symbol membership validation",
        )],
        Some(limit),
        0,
        Some(0),
    );
    meta.trust.freshness = "current".to_string();
    meta.trust.partial = true;
    meta.trust.message = format!(
        "{} impact has no match for symbol '{}' in the selected workspace; no out-of-scope impact data was returned.",
        workspace.label, target.uid
    );

    ImpactResponse {
        target: None,
        nodes: Vec::new(),
        edges: Vec::new(),
        affected_tests: empty_affected_tests(
            vec![target.file_path.clone()],
            "No affected-test hints: target symbol is outside the selected workspace.",
        ),
        states: ImpactStates {
            tier: "local-only",
            local: "available",
            org: "org-unavailable",
            freshness: "current",
            timeout: "unknown",
            permission: "unknown",
            read_only: "unknown",
            result: meta.trust.result.clone(),
        },
        meta,
    }
}

fn target_graph_node(symbol: &Symbol) -> ImpactGraphNode {
    ImpactGraphNode {
        uid: symbol.uid.clone(),
        // nw-113: an owner reads `path::<module>`, never bare.
        name: nestweaver_schema::symbol_display_name(&symbol.name, &symbol.file_path).into_owned(),
        file_path: symbol.file_path.clone(),
        start_line: symbol.start_line,
        layer: 0,
        role: "target".to_string(),
        confidence: 1.0,
        impact_score: 1.0,
        edge_type: None,
        source: source_evidence(&symbol.file_path, symbol.start_line, &symbol.uid),
    }
}

fn impact_graph_node(node: &ImpactNode) -> ImpactGraphNode {
    ImpactGraphNode {
        uid: node.uid.clone(),
        // nw-113: an owner reads `path::<module>`, never bare.
        name: nestweaver_schema::symbol_display_name(&node.name, &node.file_path).into_owned(),
        file_path: node.file_path.clone(),
        start_line: node.start_line,
        layer: node.depth,
        role: "impact".to_string(),
        confidence: node.confidence,
        impact_score: node.impact_score,
        edge_type: Some(node.edge_type.clone()),
        source: source_evidence(&node.file_path, node.start_line, &node.uid),
    }
}

/// nw-683: the link names the symbol's repo so `/source` serves that repo's
/// copy of `file_path` instead of 409ing on (or guessing between) repos that
/// share the path.
fn source_evidence(file_path: &str, start_line: u32, symbol_uid: &str) -> ImpactSourceEvidence {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("file", file_path);
    if let Some(repo) = nestweaver_schema::repo_uid_of_symbol_uid(symbol_uid) {
        serializer.append_pair("repo", repo);
    }
    serializer.append_pair("line", &start_line.to_string());
    ImpactSourceEvidence {
        file_path: file_path.to_string(),
        start_line,
        url: format!("/api/v1/source?{}", serializer.finish()),
    }
}

fn build_layered_edges(
    state: &Arc<AppState>,
    target_uid: &str,
    nodes: &[ImpactNode],
    min_confidence: f32,
) -> Result<Vec<ImpactGraphEdge>, ApiError> {
    use std::collections::HashMap;

    let mut layers: HashMap<&str, u32> = HashMap::new();
    layers.insert(target_uid, 0);
    for node in nodes {
        layers.insert(node.uid.as_str(), node.depth);
    }

    let mut edges = Vec::new();
    for node in nodes {
        let outgoing_edges = state
            .store
            .outgoing_impact_edges(&node.uid, min_confidence)?;
        for edge in outgoing_edges {
            let Some(target_layer) = layers.get(edge.target_uid.as_str()).copied() else {
                continue;
            };
            edges.push(ImpactGraphEdge {
                source: node.uid.clone(),
                target: edge.target_uid,
                edge_type: edge.edge_type,
                confidence: edge.confidence,
                source_layer: node.depth,
                target_layer,
            });
        }
    }

    edges.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.target.cmp(&right.target))
            .then_with(|| left.edge_type.cmp(&right.edge_type))
    });
    Ok(edges)
}

fn target_repo_is_stale(state: &Arc<AppState>, target: &Symbol) -> Result<bool, ApiError> {
    Ok(state
        .store
        .list_repos(None)?
        .into_iter()
        .find(|repo| repo.uid == target.repo_uid)
        .is_some_and(|repo| repo.staleness_commits_behind > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nw-113: the impact graph labels an owner `path::<module>`, as the
    /// target and as an impacted node.
    #[test]
    fn impact_graph_labels_module_owners_by_file() {
        let owner = ImpactNode {
            uid: "sym:owner".to_string(),
            name: nestweaver_schema::MODULE_OWNER_NAME.to_string(),
            file_path: "src/a.js".to_string(),
            start_line: 1,
            edge_type: "CALLS".to_string(),
            confidence: 1.0,
            depth: 1,
            impact_score: 1.0,
        };
        assert_eq!(impact_graph_node(&owner).name, "src/a.js::<module>");
        let target = Symbol {
            uid: owner.uid.clone(),
            name: owner.name.clone(),
            kind: nestweaver_schema::SymbolKind::Module,
            repo_uid: "repo:r".to_string(),
            file_path: owner.file_path.clone(),
            start_line: 1,
            end_line: 1,
            signature: String::new(),
            summary: None,
            content_hash: String::new(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: true,
            entry_point_kind: Some(nestweaver_schema::EntryPointKind::ModuleInit),
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        assert_eq!(target_graph_node(&target).name, "src/a.js::<module>");
    }

    #[test]
    fn source_evidence_names_the_symbol_repo() {
        let repo = "repo:kory-brain:c8f000561246";
        let uid = nestweaver_schema::symbol_uid(repo, "src/App.tsx", "App", 3);
        let ev = source_evidence("src/App.tsx", 3, &uid);
        assert_eq!(
            ev.url,
            "/api/v1/source?file=src%2FApp.tsx&repo=repo%3Akory-brain%3Ac8f000561246&line=3"
        );
    }
}
