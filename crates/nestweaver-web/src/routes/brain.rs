use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use nestweaver_store::{SearchHit, TantivyIndex};
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

/// nw-648: each vault carries its TRUE `note_count` (one aggregate query), so
/// the explorer can list every vault with its real size instead of counting
/// the rows of whatever page it happened to load.
pub async fn list_vaults(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let vaults = state.store.list_vaults(None)?;
    let counts = state.store.note_counts_by_vault()?;
    let rows = vaults
        .iter()
        .map(|vault| {
            let mut row = serde_json::to_value(vault)?;
            if let Some(object) = row.as_object_mut() {
                object.insert(
                    "note_count".to_string(),
                    json!(counts.get(&vault.uid).copied().unwrap_or(0)),
                );
            }
            Ok(row)
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    Ok(Json(rows).into_response())
}

pub async fn list_tags(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let tags = state.store.list_tags(None)?;
    let json = serde_json::to_value(&tags)?;
    Ok(Json(json).into_response())
}

/// Default `limit` for GET `/api/v1/brain/notes` when the query param is omitted.
/// Matches `/api/v1/symbols/top`. Callers that need a larger page must pass
/// `limit` (hard-capped at [`LIST_NOTES_LIMIT_MAX`]).
pub const LIST_NOTES_DEFAULT_LIMIT: usize = 20;
/// Hard cap on `limit` / effective page size for GET `/api/v1/brain/notes`.
pub const LIST_NOTES_LIMIT_MAX: usize = 1000;

/// Response header carrying how many notes match the request's filter, so a
/// caller holding one page can disclose or fetch the rest (nw-648). The body
/// stays a raw array, matching the sibling brain list routes.
pub const LIST_NOTES_TOTAL_HEADER: &str = "x-total-count";

/// Response header carrying the `after` cursor for the next page of a cursor
/// listing (any request without `offset`), form-urlencoded so any uid fits a
/// header. ABSENT means the listing reached the end.
///
/// nw-648 review: the store drops a corrupt row from a page, so a page one
/// row short is not the end of the vault — a caller that treated a short
/// page as "done" lost every later note. The cursor is computed from the
/// rows the scan reached, before any were dropped.
pub const LIST_NOTES_NEXT_AFTER_HEADER: &str = "x-next-after";

#[derive(Deserialize)]
pub struct ListNotesParams {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// nw-648: restrict to one vault uid. `vault_uid` is accepted too — both
    /// spellings were tried against the live UI, and both were silently
    /// ignored, returning the first vault's page.
    #[serde(alias = "vault_uid")]
    pub vault: Option<String>,
    /// nw-648: keyset cursor — return notes whose uid sorts after this one.
    /// Pages of any vault size reach the end, where `offset` stops at
    /// [`LIST_NOTES_LIMIT_MAX`]. Cannot be combined with `offset`.
    pub after: Option<String>,
}

pub async fn list_notes(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListNotesParams>,
) -> Result<Response, ApiError> {
    let limit = params
        .limit
        .unwrap_or(LIST_NOTES_DEFAULT_LIMIT)
        .min(LIST_NOTES_LIMIT_MAX);
    let offset = params.offset.unwrap_or(0);
    // Raw JSON array, matching `/brain/vaults`, `/brain/tags`, and `/symbols/top`.
    // Offset is capped at LIST_NOTES_LIMIT_MAX so Cypher's LIMIT offset+limit
    // cannot reconstruct an unbounded scan via ?limit=1000&offset=N.
    //
    // nw-648: an offset PAST the cap is an empty page, not a clamp. Clamping
    // made `offset=2000` silently return rows 1000..2000 a second time; a
    // caller paging a large vault now sees the end, and the total header
    // tells it how much it could not reach.
    let limit = if offset > LIST_NOTES_LIMIT_MAX {
        0
    } else {
        limit
    };
    // nw-648 review: a malformed request is refused before any vault lookup
    // or count — it is a 400 whatever the vault, not a 404 because the vault
    // also happens to be unknown.
    if params.after.is_some() && params.offset.is_some() {
        return Err(ApiError::bad_request(
            "`after` and `offset` cannot be combined; page with one or the other",
        ));
    }
    let vault = params.vault.as_deref().filter(|uid| !uid.is_empty());
    let total = match vault {
        Some(uid) => {
            // An unknown vault is a 404, not an empty page that reads as
            // "this vault has no notes". Any OTHER store failure propagates:
            // a broken database is not a missing vault.
            match state.store.lookup_vault(uid) {
                Ok(_) => {}
                Err(nestweaver_store::StoreError::NotFound) => {
                    return Err(ApiError::not_found(format!("vault '{uid}' not found")));
                }
                Err(error) => return Err(error.into()),
            }
            state.store.count_notes_in_vault(uid)?
        }
        None => state.store.count_notes()?,
    };
    // Without `offset` this is a cursor listing (the first page is simply
    // `after` = none), which is what lets it hand out the next cursor.
    let (notes, next_after) = if params.offset.is_some() {
        (state.store.list_notes_page(vault, limit, offset)?, None)
    } else {
        let page = state
            .store
            .list_notes_after(vault, params.after.as_deref(), limit)?;
        (page.notes, page.next_after)
    };
    let json = serde_json::to_value(&notes)?;
    let mut response = Json(json).into_response();
    let headers = response.headers_mut();
    headers.insert(
        LIST_NOTES_TOTAL_HEADER,
        axum::http::HeaderValue::from(total),
    );
    if let Some(next) = next_after {
        let encoded: String = url::form_urlencoded::byte_serialize(next.as_bytes()).collect();
        // Form-urlencoded output is visible ASCII, so this cannot fail; if it
        // somehow did, a missing cursor would read as "end", so fail loudly.
        let value = axum::http::HeaderValue::from_str(&encoded).map_err(|error| {
            ApiError::internal(format!("next-page cursor is not a header value: {error}"))
        })?;
        headers.insert(LIST_NOTES_NEXT_AFTER_HEADER, value);
    }
    Ok(response)
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

#[derive(Deserialize)]
pub struct BacklinksParams {
    pub limit: Option<usize>,
}

/// GET `/api/v1/brain/backlinks/{uid}` — same default/cap as notes-list.
pub async fn backlinks(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
    Query(params): Query<BacklinksParams>,
) -> Result<Response, ApiError> {
    let limit = params
        .limit
        .unwrap_or(LIST_NOTES_DEFAULT_LIMIT)
        .min(LIST_NOTES_LIMIT_MAX);
    state.admit_vault_derivation()?;
    let links = state.store.wikilink_sources_to_note(&uid)?;
    let total = links.len();
    let truncated = total > limit;
    let backlinks: Vec<_> = links.into_iter().take(limit).collect();
    Ok(Json(json!({
        "backlinks": backlinks,
        "count": backlinks.len(),
        "total": total,
        "truncated": truncated,
        "limit": limit,
    }))
    .into_response())
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
    let limit = params.limit.unwrap_or(20).clamp(1, 500);
    let workspace_param =
        workspaces::workspace_param(params.workspace.as_deref(), params.scope.as_deref());
    if let Some(workspace_param) = workspace_param {
        let workspace = workspaces::resolve_workspace(&state.store, Some(workspace_param))?;
        let search = scoped_brain_search(&state, &workspace, &q, limit)?;
        let meta = workspaces::p1_meta_for_result_set(
            &workspace,
            search.result_state,
            search.unsupported,
            search.provenance,
            Some(limit),
            search.results.len(),
            search.total_count,
        );
        return Ok(Json(json!({
            "results": search.results,
            "_meta": meta,
        }))
        .into_response());
    }

    // Use tantivy if available, otherwise fall back to lookup_notes_by_title
    if let Some(tantivy) = &state.tantivy {
        match tantivy.search(&q, limit) {
            Ok(hits) => {
                let hits = retain_graph_backed_search_hits(&state.store, hits);
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

struct ScopedBrainSearch {
    results: Vec<serde_json::Value>,
    provenance: Vec<P1Provenance>,
    result_state: &'static str,
    unsupported: Vec<&'static str>,
    total_count: Option<usize>,
}

/// Cap on the Tantivy over-fetch used to fill scoped searches: hits are
/// fetched beyond the requested limit so that post-filtering by workspace
/// membership can still fill the page, without letting a large limit fan
/// out into an unbounded fetch.
const SCOPED_SEARCH_OVERFETCH_CAP: usize = 1000;

struct TantivyScopedNotes {
    results: Vec<serde_json::Value>,
    /// In-scope hit count; `None` when the over-fetch saturated and the
    /// true total is therefore unknown.
    total_count: Option<usize>,
    /// True when in-scope hits were cut to the limit or the over-fetch
    /// saturated (coverage beyond the returned hits is uncertain).
    truncated: bool,
    saturated: bool,
}

/// Full-text note search via Tantivy, filtered to workspace membership.
///
/// Returns `Ok(None)` when the Tantivy query fails so callers can fall back
/// to the substring path (which must disclose `note-body-search` as
/// unsupported).
fn tantivy_scoped_note_search(
    state: &Arc<AppState>,
    tantivy: &TantivyIndex,
    workspace: &workspaces::ResolvedWorkspace,
    q: &str,
    limit: usize,
) -> Result<Option<TantivyScopedNotes>, ApiError> {
    let fetch_limit = limit
        .saturating_mul(4)
        .clamp(limit.max(1), SCOPED_SEARCH_OVERFETCH_CAP);
    let hits = match tantivy.search(q, fetch_limit) {
        Ok(hits) => hits,
        Err(e) => {
            tracing::warn!(error = %e, "tantivy search failed, falling back to scoped title lookup");
            return Ok(None);
        }
    };
    let saturated = hits.len() >= fetch_limit;
    let hits = retain_graph_backed_search_hits(&state.store, hits);

    let project_note_uids: Option<HashSet<String>> = if workspace.kind == WorkspaceKind::Project {
        Some(
            state
                .store
                .list_project_note_uids(workspace.uid.as_deref().unwrap_or_default())?
                .into_iter()
                .collect(),
        )
    } else {
        None
    };
    let mut in_scope = Vec::new();
    for hit in hits {
        if hit_in_workspace(state, workspace, project_note_uids.as_ref(), &hit) {
            in_scope.push(hit);
        }
    }

    let total_in_scope = in_scope.len();
    let results: Vec<serde_json::Value> = in_scope
        .into_iter()
        .take(limit)
        .map(|hit| serde_json::to_value(hit).unwrap_or(serde_json::Value::Null))
        .collect();
    let truncated = saturated || total_in_scope > results.len();
    Ok(Some(TantivyScopedNotes {
        results,
        total_count: (!saturated).then_some(total_in_scope),
        truncated,
        saturated,
    }))
}

fn hit_in_workspace(
    state: &Arc<AppState>,
    workspace: &workspaces::ResolvedWorkspace,
    project_note_uids: Option<&HashSet<String>>,
    hit: &SearchHit,
) -> bool {
    match workspace.kind {
        WorkspaceKind::Vault => workspace.uid.as_deref() == Some(hit.vault_uid.as_str()),
        WorkspaceKind::Project => {
            let Some(note_uids) = project_note_uids else {
                return false;
            };
            // Join the hit back to its owning note: note docs carry the
            // note uid directly; heading/section docs resolve through the
            // store. Tag docs are vault-level, never project members.
            let note_uid = match hit.kind.as_str() {
                "note" => Some(hit.uid.clone()),
                "heading" => state
                    .store
                    .lookup_heading(&hit.uid)
                    .ok()
                    .map(|heading| heading.note_uid),
                "section" => state
                    .store
                    .lookup_section(&hit.uid)
                    .ok()
                    .map(|section| section.note_uid),
                _ => None,
            };
            note_uid.is_some_and(|uid| note_uids.contains(&uid))
        }
        WorkspaceKind::All | WorkspaceKind::Repo => true,
    }
}

fn scoped_brain_search(
    state: &Arc<AppState>,
    workspace: &workspaces::ResolvedWorkspace,
    q: &str,
    limit: usize,
) -> Result<ScopedBrainSearch, ApiError> {
    if workspace.kind == WorkspaceKind::Repo {
        let page = workspaces::symbols_for_query(&state.store, q, workspace, limit)?;
        let total_count = page.total_count;
        let result_state = page.result_state("partial");
        let results = page
            .items
            .into_iter()
            .map(workspaces::symbol_search_hit)
            .collect();
        return Ok(ScopedBrainSearch {
            results,
            provenance: vec![P1Provenance::local_graph_store("repo-scoped symbol search")],
            result_state,
            unsupported: vec!["note-search"],
            total_count: Some(total_count),
        });
    }

    if workspace.kind == WorkspaceKind::Project {
        let symbol_page = workspaces::symbols_for_query(&state.store, q, workspace, limit)?;
        let remaining = limit.saturating_sub(symbol_page.items.len());

        if let Some(tantivy) = &state.tantivy
            && let Some(scoped) =
                tantivy_scoped_note_search(state, tantivy, workspace, q, remaining)?
        {
            let symbols_truncated = symbol_page.is_truncated();
            let total_count = scoped
                .total_count
                .map(|note_total| symbol_page.total_count + note_total);
            let mut results: Vec<_> = symbol_page
                .items
                .into_iter()
                .map(workspaces::symbol_search_hit)
                .collect();
            results.extend(scoped.results);
            let result_state = if results.is_empty() {
                if scoped.saturated {
                    "partial"
                } else {
                    "no-match"
                }
            } else if symbols_truncated || scoped.truncated {
                "truncated"
            } else {
                "partial"
            };
            return Ok(ScopedBrainSearch {
                results,
                provenance: vec![
                    P1Provenance::local_graph_store("project-scoped symbol search"),
                    P1Provenance::local_tantivy("project-scoped note search"),
                ],
                result_state,
                unsupported: vec!["project-components"],
                total_count,
            });
        }

        // Substring fallback: note titles/paths only, so note bodies are
        // disclosed as unsearched.
        let note_page = workspaces::notes_for_query(&state.store, q, workspace, remaining)?;
        let total_count = symbol_page.total_count + note_page.total_count;
        let mut results: Vec<_> = symbol_page
            .items
            .into_iter()
            .map(workspaces::symbol_search_hit)
            .collect();
        results.extend(note_page.items.into_iter().map(workspaces::note_search_hit));
        let result_state = if total_count == 0 {
            "no-match"
        } else if limit > 0 && total_count > results.len() {
            "truncated"
        } else {
            "partial"
        };
        return Ok(ScopedBrainSearch {
            results,
            provenance: vec![P1Provenance::local_graph_store(
                "project-scoped brain search",
            )],
            result_state,
            unsupported: vec!["project-components", "note-body-search"],
            total_count: Some(total_count),
        });
    }

    if workspace.kind == WorkspaceKind::All
        && let Some(tantivy) = &state.tantivy
    {
        match tantivy.search(q, limit) {
            Ok(hits) => {
                let hits = retain_graph_backed_search_hits(&state.store, hits);
                let saturated = limit > 0 && hits.len() >= limit;
                let results: Vec<_> = hits
                    .into_iter()
                    .map(|hit| serde_json::to_value(hit).unwrap_or(serde_json::Value::Null))
                    .collect();
                let result_state = if results.is_empty() {
                    if saturated { "partial" } else { "no-match" }
                } else if saturated {
                    "truncated"
                } else {
                    "complete"
                };
                let total_count = if saturated { None } else { Some(results.len()) };
                return Ok(ScopedBrainSearch {
                    results,
                    provenance: vec![P1Provenance::local_tantivy("brain search")],
                    result_state,
                    unsupported: Vec::new(),
                    total_count,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "tantivy search failed, falling back to scoped title lookup");
            }
        }
    }

    if workspace.kind == WorkspaceKind::Vault
        && let Some(tantivy) = &state.tantivy
        && let Some(scoped) = tantivy_scoped_note_search(state, tantivy, workspace, q, limit)?
    {
        let result_state = if scoped.results.is_empty() {
            if scoped.saturated {
                "partial"
            } else {
                "no-match"
            }
        } else if scoped.truncated {
            "truncated"
        } else {
            "complete"
        };
        return Ok(ScopedBrainSearch {
            results: scoped.results,
            provenance: vec![P1Provenance::local_tantivy("vault-scoped brain search")],
            result_state,
            unsupported: Vec::new(),
            total_count: scoped.total_count,
        });
    }

    // Substring fallback (Tantivy absent or failed): matches note
    // titles/paths only, never bodies, so coverage is never reported as
    // complete and the gap is disclosed.
    let page = workspaces::notes_for_query(&state.store, q, workspace, limit)?;
    let total_count = page.total_count;
    let result_state = page.result_state("partial");
    let results = page
        .items
        .into_iter()
        .map(workspaces::note_search_hit)
        .collect();
    Ok(ScopedBrainSearch {
        results,
        provenance: vec![P1Provenance::local_graph_store(match workspace.kind {
            WorkspaceKind::Vault => "vault-scoped note search",
            _ => "scoped note search fallback",
        })],
        result_state,
        unsupported: vec!["note-body-search"],
        total_count: Some(total_count),
    })
}

/// Drop Tantivy hits whose note identity is absent from the graph so search
/// cannot present a ghost UID as a real note (404 on `/brain/note/{uid}`).
fn retain_graph_backed_search_hits(
    store: &nestweaver_store::GraphStore,
    hits: Vec<SearchHit>,
) -> Vec<SearchHit> {
    hits.into_iter()
        .filter(|hit| search_hit_exists_in_graph(store, hit))
        .collect()
}

fn search_hit_exists_in_graph(store: &nestweaver_store::GraphStore, hit: &SearchHit) -> bool {
    let note_uid = if hit.kind.eq_ignore_ascii_case("note") || hit.uid.starts_with("note:") {
        Some(hit.uid.as_str())
    } else if !hit.note_uid.is_empty() {
        Some(hit.note_uid.as_str())
    } else {
        None
    };
    match note_uid {
        Some(uid) => store.lookup_note(uid).is_ok(),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{Note, NoteKind, Vault};
    use nestweaver_store::GraphStore;

    fn hit(uid: &str, kind: &str) -> SearchHit {
        SearchHit {
            uid: uid.to_string(),
            kind: kind.to_string(),
            title: uid.to_string(),
            vault_uid: "vlt:notes".to_string(),
            note_uid: String::new(),
            score: 1.0,
        }
    }

    #[test]
    fn graph_missing_note_hits_are_dropped_from_search() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_vault(&Vault {
                uid: "vlt:notes".to_string(),
                name: "Notes".to_string(),
                root_path: "/tmp/notes".to_string(),
                instance_id: "local".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:notes:real".to_string(),
                vault_uid: "vlt:notes".to_string(),
                file_path: "real.md".to_string(),
                title: "Real".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "h".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();

        let kept = retain_graph_backed_search_hits(
            &store,
            vec![
                hit("note:notes:ghost", "note"),
                hit("note:notes:real", "note"),
                hit("sym:test:greet", "symbol"),
            ],
        );
        let uids: Vec<&str> = kept.iter().map(|h| h.uid.as_str()).collect();
        assert_eq!(uids, vec!["note:notes:real", "sym:test:greet"]);
        assert!(
            !uids.contains(&"note:notes:ghost"),
            "a UID that 404s on brain/note must not appear as a search hit"
        );
    }
}
