use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::ApiError;
use crate::routes::workspaces::{self, P1Provenance, ResolvedWorkspace, WorkspaceKind};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ContextRequest {
    pub seeds: Vec<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    50
}

/// Graph-missing `note:` UIDs are client errors. Brain-context otherwise
/// accepts the UID as a seed and drops it at render time, which would 200
/// an empty body instead of 4xx. Name/path misses still go through the
/// engine and [`map_context_engine_error`].
fn reject_unresolved_http_seeds(
    store: &nestweaver_store::GraphStore,
    seeds: &[String],
) -> Result<(), ApiError> {
    for seed in seeds {
        let trimmed = seed.trim();
        if trimmed.starts_with("note:") {
            match store.lookup_note(trimmed) {
                Ok(_) => {}
                Err(nestweaver_store::StoreError::NotFound) => {
                    return Err(ApiError::not_found("note seed not found"));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Engine not-found / ambiguous lookup failures are 4xx. Messages stay generic
/// so unresolved seeds never round-trip into the HTTP body.
fn map_context_engine_error(err: anyhow::Error) -> ApiError {
    let message = err.to_string();
    if message.contains("No matching symbols")
        || message.contains("No symbols found")
        || message.contains("No seeds resolved")
    {
        return ApiError::not_found("no matching context seeds");
    }
    if message.contains("Ambiguous") {
        return ApiError::bad_request("ambiguous context seed");
    }
    ApiError::from_ranking(err)
}

pub async fn code_context(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ContextRequest>,
) -> Result<Response, ApiError> {
    if body.seeds.is_empty() {
        return Err(ApiError::bad_request("seeds must not be empty"));
    }
    reject_unresolved_http_seeds(&state.store, &body.seeds)?;
    state.admit_vault_derivation()?;
    let result = nestweaver_engine::build_context(&state.store, &body.seeds)
        .map_err(map_context_engine_error)?;
    let mut json = serde_json::to_value(&result)?;
    crate::bridge::annotate_context_payload(&state, &mut json);
    Ok(Json(json).into_response())
}

#[derive(Deserialize)]
pub struct BrainContextRequest {
    pub seeds: Vec<String>,
    #[serde(default)]
    pub token_budget: Option<usize>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

pub async fn brain_context(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BrainContextRequest>,
) -> Result<Response, ApiError> {
    if body.seeds.is_empty() {
        return Err(ApiError::bad_request("seeds must not be empty"));
    }
    if body.seeds.len() > 100 {
        return Err(ApiError::bad_request(
            "at most 100 context seeds are supported",
        ));
    }
    let generation = state.store.graph_generation();
    nestweaver_engine::context_graph::ensure_context_generation(&state.store, generation)
        .map_err(ApiError::from_ranking)?;
    reject_unresolved_http_seeds(&state.store, &body.seeds)?;
    state.admit_vault_derivation()?;
    let workspace = workspaces::resolve_workspace(
        &state.store,
        workspaces::workspace_param(body.workspace.as_deref(), body.scope.as_deref()),
    )?;
    let config = nestweaver_engine::HybridSearchConfig::default();
    let mut result = match nestweaver_engine::build_brain_context_hybrid(
        &state.store,
        &body.seeds,
        state.tantivy.as_deref(),
        &config,
        None,
        None,
    ) {
        Ok(result) => result,
        // Vault workspaces do not admit code results. An unresolved `sym:`
        // seed would 404 before the vault filter can return the documented
        // empty no-match body.
        Err(err)
            if workspace.kind == WorkspaceKind::Vault
                && err
                    .chain()
                    .any(|cause| cause.to_string().contains("No seeds resolved")) =>
        {
            nestweaver_engine::BrainContextResult {
                unresolved_seeds: body.seeds.clone(),
                ..Default::default()
            }
        }
        Err(err) => return Err(map_context_engine_error(err)),
    };
    filter_brain_context_result(&state, &workspace, &mut result)?;
    // This router is the existing trusted loopback UI, not a remote policy
    // boundary. Remote callers must supply their own resolved VisibleRepos.
    let graph = nestweaver_engine::context_graph::attach_context_graph(
        &state.store,
        &mut result,
        &nestweaver_engine::authz::VisibleRepos::All,
        generation,
    )
    .map_err(ApiError::from_ranking)?;
    let empty_result = result.seeds.is_empty() && result.connected.is_empty();
    let mut meta = brain_context_meta(&workspace, body.token_budget, empty_result);
    if graph.meta.truncated {
        meta.trust.partial = true;
        meta.trust.result = "partial".to_string();
        meta.trust.message.push_str(" Context graph limits omitted nodes or relationships; graph_meta describes their populations.");
        meta.truncation.truncated = true;
        // Nodes and edges are different populations. Do not combine their
        // counts or describe a lower-bound edge count as an exact total.
        meta.truncation.limit = None;
        meta.truncation.omitted_count = None;
        meta.continuation.has_more = false;
        meta.continuation.reason =
            Some("Narrow the context seeds to inspect omitted relationships.".to_string());
    }
    let mut json = serde_json::to_value(&result)?;
    // Scene-level bridge emphasis: seeds + connected form one scene, so
    // the top-12 cap and 0..=1 normalization span the whole response.
    crate::bridge::annotate_context_payload(&state, &mut json);
    if let serde_json::Value::Object(ref mut object) = json {
        object.insert("edges".to_string(), serde_json::to_value(graph.edges)?);
        object.insert("graph_meta".to_string(), serde_json::to_value(graph.meta)?);
        object.insert("_meta".to_string(), serde_json::to_value(meta)?);
    }
    nestweaver_engine::context_graph::ensure_context_generation(&state.store, generation)
        .map_err(ApiError::from_ranking)?;
    Ok(Json(json).into_response())
}

fn filter_brain_context_result(
    state: &Arc<AppState>,
    workspace: &ResolvedWorkspace,
    result: &mut nestweaver_engine::BrainContextResult,
) -> Result<(), ApiError> {
    if workspace.kind == WorkspaceKind::All {
        return Ok(());
    }

    result.seeds.retain(|node| {
        brain_node_in_workspace(&state.store, workspace, &node.uid).unwrap_or(false)
    });
    result.connected.retain(|node| {
        brain_node_in_workspace(&state.store, workspace, &node.uid).unwrap_or(false)
    });
    Ok(())
}

fn brain_node_in_workspace(
    store: &nestweaver_store::GraphStore,
    workspace: &ResolvedWorkspace,
    uid: &str,
) -> Result<bool, ApiError> {
    match workspace.kind {
        WorkspaceKind::All => Ok(true),
        WorkspaceKind::Project => {
            let Some(project_uid) = workspace.uid.as_deref() else {
                return Ok(false);
            };
            if uid.starts_with("sym:") {
                return Ok(store
                    .list_project_symbol_uids(project_uid)?
                    .iter()
                    .any(|member_uid| member_uid == uid));
            }
            if uid.starts_with("note:") {
                return Ok(store
                    .list_project_note_uids(project_uid)?
                    .iter()
                    .any(|member_uid| member_uid == uid));
            }
            if uid.starts_with("head:") {
                return Ok(store
                    .lookup_heading(uid)
                    .map(|heading| {
                        store
                            .list_project_note_uids(project_uid)
                            .map(|note_uids| {
                                note_uids
                                    .iter()
                                    .any(|note_uid| note_uid == &heading.note_uid)
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false));
            }
            if uid.starts_with("sec:") {
                return Ok(store
                    .lookup_section(uid)
                    .map(|section| {
                        store
                            .list_project_note_uids(project_uid)
                            .map(|note_uids| {
                                note_uids
                                    .iter()
                                    .any(|note_uid| note_uid == &section.note_uid)
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false));
            }
            Ok(false)
        }
        WorkspaceKind::Repo => {
            let Some(repo_uid) = workspace.uid.as_deref() else {
                return Ok(false);
            };
            if !uid.starts_with("sym:") {
                return Ok(false);
            }
            Ok(store
                .lookup_symbol(uid)
                .map(|symbol| symbol.repo_uid == repo_uid)
                .unwrap_or(false))
        }
        WorkspaceKind::Vault => {
            let Some(vault_uid) = workspace.uid.as_deref() else {
                return Ok(false);
            };
            if uid.starts_with("note:") {
                return Ok(store
                    .lookup_note(uid)
                    .map(|note| note.vault_uid == vault_uid)
                    .unwrap_or(false));
            }
            if uid.starts_with("head:") {
                return Ok(store
                    .lookup_heading(uid)
                    .and_then(|heading| store.lookup_note(&heading.note_uid))
                    .map(|note| note.vault_uid == vault_uid)
                    .unwrap_or(false));
            }
            if uid.starts_with("sec:") {
                return Ok(store
                    .lookup_section(uid)
                    .and_then(|section| store.lookup_note(&section.note_uid))
                    .map(|note| note.vault_uid == vault_uid)
                    .unwrap_or(false));
            }
            if uid.starts_with("tag:") {
                return Ok(store
                    .lookup_tag(uid)
                    .map(|tag| tag.vault_uid == vault_uid)
                    .unwrap_or(false));
            }
            Ok(false)
        }
    }
}

fn brain_context_meta(
    workspace: &ResolvedWorkspace,
    token_budget: Option<usize>,
    empty_result: bool,
) -> workspaces::P1Meta {
    // The engine's brain-context builder has no token-budget support, so a
    // requested budget is never enforced. Disclose it as unsupported rather
    // than echoing it as an applied truncation limit.
    let (success_result, mut unsupported, provenance_detail) = match workspace.kind {
        WorkspaceKind::All => ("complete", Vec::new(), "brain context"),
        WorkspaceKind::Project => (
            "partial",
            vec!["project-components"],
            "project-filtered brain context",
        ),
        WorkspaceKind::Repo => (
            "partial",
            vec!["note-results"],
            "repo-filtered brain context",
        ),
        WorkspaceKind::Vault => (
            "partial",
            vec!["code-results"],
            "vault-filtered brain context",
        ),
    };
    if token_budget.is_some() {
        unsupported.push("token-budget");
    }
    workspaces::p1_meta(
        workspace,
        if empty_result {
            "no-match"
        } else {
            success_result
        },
        unsupported,
        vec![P1Provenance::local_graph_store(provenance_detail)],
        None,
    )
}
