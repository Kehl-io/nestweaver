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
use crate::routes::workspaces::{self, P1Meta, P1Provenance, ResolvedWorkspace, WorkspaceKind};
use crate::state::AppState;

#[derive(Deserialize)]
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
    let confidence = params.confidence.unwrap_or(0.3).clamp(0.0, 1.0);
    let depth = params.depth.unwrap_or(3).min(20);
    let limit = params.limit.unwrap_or(250).clamp(1, 1000);
    let workspace = workspaces::resolve_workspace(
        &state.store,
        workspaces::workspace_param(params.workspace.as_deref(), params.scope.as_deref()),
    )?;

    if workspace.kind == WorkspaceKind::Vault {
        return Ok(Json(unsupported_workspace_response(&workspace, limit)).into_response());
    }

    let target = state
        .store
        .lookup_symbol(&uid)
        .map_err(|_| ApiError::not_found(format!("symbol '{uid}' not found")))?;

    let Some(scope) = ImpactScope::resolve(&state, &workspace)? else {
        return Ok(Json(unsupported_workspace_response(&workspace, limit)).into_response());
    };
    if !scope.contains(&target) {
        return Ok(Json(out_of_scope_response(&workspace, &target, limit)).into_response());
    }

    let impact_nodes: Vec<ImpactNode> = state
        .store
        .impact(&uid, depth, confidence)?
        .into_iter()
        .filter(|node| scope.contains_impact_node(node))
        .collect();
    let affected_tests = scoped_affected_tests(&state, &scope, &target)?;
    let total_node_count = impact_nodes.len() + 1;
    let returned_impact_nodes: Vec<ImpactNode> = impact_nodes
        .iter()
        .take(limit.saturating_sub(1))
        .cloned()
        .collect();
    let returned_count = returned_impact_nodes.len() + 1;
    let stale = target_repo_is_stale(&state, &target)?;
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
    let edges = build_layered_edges(&state, &target.uid, &returned_impact_nodes)?;
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

    Ok(Json(ImpactResponse {
        target: Some(target_node),
        nodes,
        edges,
        affected_tests,
        states,
        meta,
    })
    .into_response())
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
            .map(|file| file.tests.len())
            .sum::<usize>(),
        result
            .tier_2
            .iter()
            .map(|file| file.tests.len())
            .sum::<usize>(),
        result
            .tier_3
            .iter()
            .map(|file| file.tests.len())
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
        name: symbol.name.clone(),
        file_path: symbol.file_path.clone(),
        start_line: symbol.start_line,
        layer: 0,
        role: "target".to_string(),
        confidence: 1.0,
        impact_score: 1.0,
        edge_type: None,
        source: source_evidence(&symbol.file_path, symbol.start_line),
    }
}

fn impact_graph_node(node: &ImpactNode) -> ImpactGraphNode {
    ImpactGraphNode {
        uid: node.uid.clone(),
        name: node.name.clone(),
        file_path: node.file_path.clone(),
        start_line: node.start_line,
        layer: node.depth,
        role: "impact".to_string(),
        confidence: node.confidence,
        impact_score: node.impact_score,
        edge_type: Some(node.edge_type.clone()),
        source: source_evidence(&node.file_path, node.start_line),
    }
}

fn source_evidence(file_path: &str, start_line: u32) -> ImpactSourceEvidence {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("file", file_path);
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
) -> Result<Vec<ImpactGraphEdge>, ApiError> {
    use std::collections::HashMap;

    let mut layers: HashMap<&str, u32> = HashMap::new();
    layers.insert(target_uid, 0);
    for node in nodes {
        layers.insert(node.uid.as_str(), node.depth);
    }

    let mut edges = Vec::new();
    for node in nodes {
        let callees = state.store.callees_of(&node.uid)?;
        for callee in callees {
            let Some(target_layer) = layers.get(callee.uid.as_str()).copied() else {
                continue;
            };
            if target_layer + 1 != node.depth {
                continue;
            }
            edges.push(ImpactGraphEdge {
                source: node.uid.clone(),
                target: callee.uid,
                edge_type: node.edge_type.clone(),
                confidence: node.confidence,
                source_layer: node.depth,
                target_layer,
            });
        }
    }

    let mut has_edge_for: std::collections::HashSet<String> =
        edges.iter().map(|edge| edge.source.clone()).collect();
    let mut previous_by_layer: HashMap<u32, &ImpactNode> = HashMap::new();
    for node in nodes {
        previous_by_layer.entry(node.depth).or_insert(node);
    }
    for node in nodes {
        if has_edge_for.contains(&node.uid) {
            continue;
        }
        let fallback_target = if node.depth <= 1 {
            Some((target_uid, 0))
        } else {
            previous_by_layer
                .get(&(node.depth - 1))
                .map(|parent| (parent.uid.as_str(), parent.depth))
        };
        if let Some((fallback_uid, fallback_layer)) = fallback_target {
            edges.push(ImpactGraphEdge {
                source: node.uid.clone(),
                target: fallback_uid.to_string(),
                edge_type: node.edge_type.clone(),
                confidence: node.confidence,
                source_layer: node.depth,
                target_layer: fallback_layer,
            });
            has_edge_for.insert(node.uid.clone());
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
