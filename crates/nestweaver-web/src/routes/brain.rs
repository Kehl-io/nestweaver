use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::routes::workspaces::{self, P1Provenance, WorkspaceKind};
use crate::state::AppState;

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

pub async fn brain_status(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let vault_count = state.store.list_vaults(None)?.len();
    let note_count = state.store.count_notes()?;
    let heading_count = state.store.count_headings()?;
    let section_count = state.store.count_sections()?;
    let tag_count = state.store.count_tags()?;
    let wikilink_count = state.store.count_wikilink_edges()?;
    let cross_domain_count = state.store.count_references_code_edges()?;

    Ok(Json(json!({
        "vault_count": vault_count,
        "note_count": note_count,
        "heading_count": heading_count,
        "section_count": section_count,
        "tag_count": tag_count,
        "wikilink_count": wikilink_count,
        "cross_domain_count": cross_domain_count,
    }))
    .into_response())
}

pub async fn list_vaults(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let vaults = state.store.list_vaults(None)?;
    let json = serde_json::to_value(&vaults)?;
    Ok(Json(json).into_response())
}

pub async fn list_tags(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let tags = state.store.list_tags(None)?;
    let json = serde_json::to_value(&tags)?;
    Ok(Json(json).into_response())
}

pub async fn list_notes(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let notes = state.store.list_notes(None)?;
    let json = serde_json::to_value(&notes)?;
    Ok(Json(json).into_response())
}

pub async fn note_by_uid(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
) -> Result<Response, ApiError> {
    let note = state
        .store
        .lookup_note(&uid)
        .map_err(|_| ApiError::not_found(format!("note '{uid}' not found")))?;

    let headings = state.store.headings_in_note(&uid)?;
    let sections = state.store.sections_in_note(&uid)?;

    if note.file_path.contains("..") {
        return Err(ApiError::bad_request("invalid note file path"));
    }

    // Read file body from vault root + note file_path
    let body = match state.store.lookup_vault(&note.vault_uid) {
        Ok(vault) => {
            let full_path = std::path::Path::new(&vault.root_path).join(&note.file_path);
            std::fs::read_to_string(&full_path).unwrap_or_default()
        }
        Err(_) => String::new(),
    };

    Ok(Json(json!({
        "note": note,
        "headings": headings,
        "sections": sections,
        "body": body,
    }))
    .into_response())
}

pub async fn backlinks(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
) -> Result<Response, ApiError> {
    let links = state.store.wikilink_sources_to_note(&uid)?;
    let json = serde_json::to_value(&links)?;
    Ok(Json(json).into_response())
}

#[derive(serde::Serialize)]
struct UnlinkedMention {
    note_uid: String,
    title: String,
    path: String,
    snippet: String,
}

pub async fn unlinked_mentions(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
) -> Result<Response, ApiError> {
    let note = state
        .store
        .lookup_note(&uid)
        .map_err(|_| ApiError::not_found(format!("note '{uid}' not found")))?;

    let title = &note.title;
    if title.is_empty() {
        return Ok(Json(serde_json::Value::Array(vec![])).into_response());
    }

    // Get all notes that wikilink to this note so we can exclude them
    let backlinks = state.store.wikilink_sources_to_note(&uid)?;
    let linked_uids: std::collections::HashSet<&str> = backlinks
        .iter()
        .map(|b| b.source_note_uid.as_str())
        .collect();

    // List all notes in the same vault
    let all_notes = state.store.list_notes(Some(&note.vault_uid))?;
    let title_lower = title.to_lowercase();

    let mut mentions = Vec::new();

    for candidate in &all_notes {
        // Skip the note itself and notes that already wikilink to it
        if candidate.uid == uid || linked_uids.contains(candidate.uid.as_str()) {
            continue;
        }

        // Validate candidate file path before reading
        if candidate.file_path.contains("..") {
            continue;
        }

        // Read the note's file content to check for title mentions
        let content = match state.store.lookup_vault(&candidate.vault_uid) {
            Ok(vault) => {
                let full_path = std::path::Path::new(&vault.root_path).join(&candidate.file_path);
                std::fs::read_to_string(&full_path).unwrap_or_default()
            }
            Err(_) => continue,
        };

        let content_lower = content.to_lowercase();
        if let Some(pos) = content_lower.find(&title_lower) {
            let start = pos.saturating_sub(50);
            let end = (pos + title.len() + 50).min(content.len());
            let start = floor_char_boundary(&content, start);
            let end = ceil_char_boundary(&content, end);
            let snippet = content[start..end].to_string();

            mentions.push(UnlinkedMention {
                note_uid: candidate.uid.clone(),
                title: candidate.title.clone(),
                path: candidate.file_path.clone(),
                snippet,
            });
        }
    }

    let json = serde_json::to_value(&mentions)?;
    Ok(Json(json).into_response())
}

#[derive(Deserialize)]
pub struct BrainSearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
    pub workspace: Option<String>,
    pub scope: Option<String>,
}

pub async fn brain_search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BrainSearchParams>,
) -> Result<Response, ApiError> {
    let q = params.q.unwrap_or_default();
    if q.is_empty() {
        return Err(ApiError::bad_request("query parameter 'q' is required"));
    }
    let limit = params.limit.unwrap_or(20);
    let workspace_param =
        workspaces::workspace_param(params.workspace.as_deref(), params.scope.as_deref());
    if let Some(workspace_param) = workspace_param {
        let workspace = workspaces::resolve_workspace(&state.store, Some(workspace_param))?;
        let (results, provenance, result_state, unsupported) =
            scoped_brain_search(&state, &workspace, &q, limit)?;
        let meta = workspaces::p1_meta(
            &workspace,
            result_state,
            unsupported,
            vec![provenance],
            Some(limit),
        );
        return Ok(Json(json!({
            "results": results,
            "_meta": meta,
        }))
        .into_response());
    }

    // Use tantivy if available, otherwise fall back to lookup_notes_by_title
    if let Some(tantivy) = &state.tantivy {
        match tantivy.search(&q, limit) {
            Ok(hits) => {
                let json = serde_json::to_value(&hits)?;
                return Ok(Json(json).into_response());
            }
            Err(e) => {
                tracing::warn!(error = %e, "tantivy search failed, falling back to title lookup");
            }
        }
    }

    let notes = state.store.lookup_notes_by_title(&q)?;
    let json = serde_json::to_value(&notes)?;
    Ok(Json(json).into_response())
}

fn scoped_brain_search(
    state: &Arc<AppState>,
    workspace: &workspaces::ResolvedWorkspace,
    q: &str,
    limit: usize,
) -> Result<
    (
        Vec<serde_json::Value>,
        P1Provenance,
        &'static str,
        Vec<&'static str>,
    ),
    ApiError,
> {
    if workspace.kind == WorkspaceKind::Repo {
        return Ok((
            Vec::new(),
            P1Provenance::local_graph_store("repo-scoped brain search unsupported in P1"),
            "partial",
            vec!["note-search"],
        ));
    }

    if let Some(tantivy) = &state.tantivy {
        match tantivy.search(q, limit) {
            Ok(hits) => {
                let results: Vec<_> = hits
                    .into_iter()
                    .filter(|hit| {
                        workspace.kind == WorkspaceKind::All
                            || workspace
                                .uid
                                .as_deref()
                                .is_some_and(|vault_uid| hit.vault_uid == vault_uid)
                    })
                    .map(|hit| serde_json::to_value(hit).unwrap_or(serde_json::Value::Null))
                    .collect();
                return Ok((
                    results,
                    P1Provenance::local_tantivy("brain search"),
                    "complete",
                    Vec::new(),
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "tantivy search failed, falling back to scoped title lookup");
            }
        }
    }

    let results = workspaces::notes_for_query(&state.store, q, workspace, limit)?
        .into_iter()
        .map(workspaces::note_search_hit)
        .collect();
    Ok((
        results,
        P1Provenance::local_graph_store("scoped note search fallback"),
        "complete",
        Vec::new(),
    ))
}
