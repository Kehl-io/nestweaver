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

pub async fn code_context(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ContextRequest>,
) -> Result<Response, ApiError> {
    if body.seeds.is_empty() {
        return Err(ApiError::bad_request("seeds must not be empty"));
    }
    let result = nestweaver_engine::build_context(&state.store, &body.seeds)?;
    let json = serde_json::to_value(&result)?;
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
    let workspace = workspaces::resolve_workspace(
        &state.store,
        workspaces::workspace_param(body.workspace.as_deref(), body.scope.as_deref()),
    )?;
    let config = nestweaver_engine::HybridSearchConfig::default();
    let mut result = nestweaver_engine::build_brain_context_hybrid(
        &state.store,
        &body.seeds,
        state.tantivy.as_deref(),
        &config,
        None,
        None,
    )?;
    filter_brain_context_result(&state, &workspace, &mut result)?;
    let empty_result = result.seeds.is_empty() && result.connected.is_empty();
    let meta = brain_context_meta(&workspace, body.token_budget, empty_result);
    let mut json = serde_json::to_value(&result)?;
    if let serde_json::Value::Object(ref mut object) = json {
        object.insert("_meta".to_string(), serde_json::to_value(meta)?);
    }
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
    match workspace.kind {
        WorkspaceKind::All => workspaces::p1_meta(
            workspace,
            if empty_result { "no-match" } else { "complete" },
            Vec::<&str>::new(),
            vec![P1Provenance::local_graph_store("brain context")],
            token_budget,
        ),
        WorkspaceKind::Repo => workspaces::p1_meta(
            workspace,
            if empty_result { "no-match" } else { "partial" },
            vec!["note-results"],
            vec![P1Provenance::local_graph_store(
                "repo-filtered brain context",
            )],
            token_budget,
        ),
        WorkspaceKind::Vault => workspaces::p1_meta(
            workspace,
            if empty_result { "no-match" } else { "partial" },
            vec!["code-results"],
            vec![P1Provenance::local_graph_store(
                "vault-filtered brain context",
            )],
            token_budget,
        ),
    }
}
