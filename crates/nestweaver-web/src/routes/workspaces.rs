use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use nestweaver_schema::{Note, Repo, Service, Symbol, Vault};
use nestweaver_store::GraphStore;
use serde::Serialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceKind {
    All,
    Repo,
    Vault,
}

impl WorkspaceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Repo => "repo",
            Self::Vault => "vault",
        }
    }

    fn data_scope(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Repo => "repo-scoped",
            Self::Vault => "vault-scoped",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedWorkspace {
    pub id: String,
    pub kind: WorkspaceKind,
    pub uid: Option<String>,
    pub label: String,
}

impl ResolvedWorkspace {
    pub fn all() -> Self {
        Self {
            id: "all".to_string(),
            kind: WorkspaceKind::All,
            uid: None,
            label: "All indexed content".to_string(),
        }
    }

    pub fn repo(repo: &Repo) -> Self {
        Self {
            id: repo_workspace_id(&repo.uid),
            kind: WorkspaceKind::Repo,
            uid: Some(repo.uid.clone()),
            label: repo_label(repo),
        }
    }

    pub fn vault(vault: &Vault) -> Self {
        Self {
            id: vault_workspace_id(&vault.uid),
            kind: WorkspaceKind::Vault,
            uid: Some(vault.uid.clone()),
            label: vault.name.clone(),
        }
    }
}

#[derive(Clone, Serialize)]
pub struct WorkspaceCounts {
    pub repo_count: usize,
    pub service_count: usize,
    pub vault_count: usize,
    pub note_count: usize,
    pub symbol_count: usize,
}

#[derive(Clone, Serialize)]
pub struct P1TrustMeta {
    pub data_scope: String,
    pub federation: String,
    pub freshness: String,
    pub capability: String,
    pub result: String,
    pub source_confidence: String,
    pub partial: bool,
    pub unsupported: Vec<String>,
    pub message: String,
}

#[derive(Clone, Serialize)]
pub struct P1Provenance {
    pub source: String,
    pub detail: String,
}

#[derive(Clone, Serialize)]
pub struct P1Truncation {
    pub truncated: bool,
    pub limit: Option<usize>,
    pub omitted_count: Option<usize>,
}

#[derive(Clone, Serialize)]
pub struct P1Continuation {
    pub has_more: bool,
    pub cursor: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct P1Meta {
    pub workspace_id: String,
    pub workspace_type: String,
    pub trust: P1TrustMeta,
    pub provenance: Vec<P1Provenance>,
    pub truncation: P1Truncation,
    pub continuation: P1Continuation,
}

#[derive(Serialize)]
struct WorkspaceEntry {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    uid: Option<String>,
    counts: WorkspaceCounts,
    #[serde(rename = "_meta")]
    meta: P1Meta,
}

#[derive(Serialize)]
struct WorkspaceCatalogResponse {
    workspaces: Vec<WorkspaceEntry>,
    #[serde(rename = "_meta")]
    meta: P1Meta,
}

pub async fn workspaces(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let entries = workspace_entries(&state.store)?;
    let all = ResolvedWorkspace::all();
    Ok(Json(WorkspaceCatalogResponse {
        workspaces: entries,
        meta: p1_meta(
            &all,
            "complete",
            Vec::<&str>::new(),
            vec![P1Provenance::local_graph_store("workspace catalog")],
            None,
        ),
    })
    .into_response())
}

pub fn workspace_param<'a>(workspace: Option<&'a str>, scope: Option<&'a str>) -> Option<&'a str> {
    workspace
        .filter(|value| !value.trim().is_empty())
        .or_else(|| scope.filter(|value| is_workspace_scope_value(value)))
}

pub fn resolve_workspace(
    store: &GraphStore,
    workspace: Option<&str>,
) -> Result<ResolvedWorkspace, ApiError> {
    let Some(raw) = workspace.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(ResolvedWorkspace::all());
    };
    if raw == "all" {
        return Ok(ResolvedWorkspace::all());
    }

    let repos = store.list_repos(None)?;
    if let Some(repo) = repos.iter().find(|repo| repo.uid == raw) {
        return Ok(ResolvedWorkspace::repo(repo));
    }
    if let Some(uid) = raw.strip_prefix("repo:")
        && let Some(repo) = repos.iter().find(|repo| repo.uid == uid)
    {
        return Ok(ResolvedWorkspace::repo(repo));
    }

    let vaults = store.list_vaults(None)?;
    if let Some(vault) = vaults.iter().find(|vault| vault.uid == raw) {
        return Ok(ResolvedWorkspace::vault(vault));
    }
    if let Some(uid) = raw.strip_prefix("vault:")
        && let Some(vault) = vaults.iter().find(|vault| vault.uid == uid)
    {
        return Ok(ResolvedWorkspace::vault(vault));
    }

    Err(ApiError::bad_request(format!(
        "workspace '{raw}' was not found"
    )))
}

pub fn workspace_counts(
    store: &GraphStore,
    workspace: &ResolvedWorkspace,
) -> Result<WorkspaceCounts, ApiError> {
    match workspace.kind {
        WorkspaceKind::All => Ok(WorkspaceCounts {
            repo_count: store.list_repos(None)?.len(),
            service_count: store.list_services(None)?.len(),
            vault_count: store.list_vaults(None)?.len(),
            note_count: store.count_notes()?,
            symbol_count: store.count_symbols()?,
        }),
        WorkspaceKind::Repo => {
            let uid = workspace.uid.as_deref().unwrap_or_default();
            Ok(WorkspaceCounts {
                repo_count: 1,
                service_count: services_for_repo(store, uid)?.len(),
                vault_count: 0,
                note_count: 0,
                symbol_count: symbols_for_repo(store, uid)?.len(),
            })
        }
        WorkspaceKind::Vault => {
            let uid = workspace.uid.as_deref().unwrap_or_default();
            Ok(WorkspaceCounts {
                repo_count: 0,
                service_count: 0,
                vault_count: 1,
                note_count: store.list_notes_lite(Some(uid))?.len(),
                symbol_count: 0,
            })
        }
    }
}

pub fn p1_meta(
    workspace: &ResolvedWorkspace,
    result: &str,
    unsupported: Vec<&str>,
    provenance: Vec<P1Provenance>,
    limit: Option<usize>,
) -> P1Meta {
    let partial = result == "partial" || !unsupported.is_empty();
    let unsupported: Vec<String> = unsupported.into_iter().map(str::to_string).collect();
    let message = if partial {
        format!(
            "{} is local-only; unsupported portions are disclosed explicitly.",
            workspace.label
        )
    } else {
        format!("{} is served from the local graph store.", workspace.label)
    };

    P1Meta {
        workspace_id: workspace.id.clone(),
        workspace_type: workspace.kind.as_str().to_string(),
        trust: P1TrustMeta {
            data_scope: workspace.kind.data_scope().to_string(),
            federation: "local-only".to_string(),
            freshness: if partial { "partial" } else { "current" }.to_string(),
            capability: "local-index".to_string(),
            result: if partial {
                "partial".to_string()
            } else {
                result.to_string()
            },
            source_confidence: "extracted".to_string(),
            partial,
            unsupported,
            message,
        },
        provenance,
        truncation: P1Truncation {
            truncated: false,
            limit,
            omitted_count: None,
        },
        continuation: P1Continuation {
            has_more: false,
            cursor: None,
            reason: None,
        },
    }
}

impl P1Provenance {
    pub fn local_graph_store(detail: impl Into<String>) -> Self {
        Self {
            source: "local_graph_store".to_string(),
            detail: detail.into(),
        }
    }

    pub fn local_tantivy(detail: impl Into<String>) -> Self {
        Self {
            source: "local_tantivy".to_string(),
            detail: detail.into(),
        }
    }
}

pub fn services_for_repo(store: &GraphStore, repo_uid: &str) -> Result<Vec<Service>, ApiError> {
    Ok(store
        .list_services(None)?
        .into_iter()
        .filter(|service| service.repo_uid == repo_uid)
        .collect())
}

pub fn symbols_for_repo(store: &GraphStore, repo_uid: &str) -> Result<Vec<Symbol>, ApiError> {
    let mut symbols: Vec<Symbol> = store
        .list_all_symbols()?
        .into_iter()
        .filter(|symbol| symbol.repo_uid == repo_uid)
        .collect();
    symbols.sort_by(|a, b| {
        b.pagerank_score
            .unwrap_or(0.0)
            .partial_cmp(&a.pagerank_score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(symbols)
}

pub fn notes_for_query(
    store: &GraphStore,
    query: &str,
    workspace: &ResolvedWorkspace,
    limit: usize,
) -> Result<Vec<Note>, ApiError> {
    let needle = query.to_lowercase();
    let mut notes: Vec<Note> = match workspace.kind {
        WorkspaceKind::All => store.list_notes(None)?,
        WorkspaceKind::Vault => store.list_notes(workspace.uid.as_deref())?,
        WorkspaceKind::Repo => Vec::new(),
    }
    .into_iter()
    .filter(|note| {
        note.title.to_lowercase().contains(&needle)
            || note.file_path.to_lowercase().contains(&needle)
    })
    .collect();
    notes.sort_by(|a, b| {
        b.pagerank_score
            .unwrap_or(0.0)
            .partial_cmp(&a.pagerank_score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    notes.truncate(limit);
    Ok(notes)
}

pub fn note_search_hit(note: Note) -> serde_json::Value {
    json!({
        "uid": note.uid,
        "kind": "Note",
        "title": note.title,
        "vault_uid": note.vault_uid,
        "score": note.pagerank_score.unwrap_or(0.0),
    })
}

fn workspace_entries(store: &GraphStore) -> Result<Vec<WorkspaceEntry>, ApiError> {
    let repos = store.list_repos(None)?;
    let vaults = store.list_vaults(None)?;

    let mut entries = Vec::with_capacity(1 + repos.len() + vaults.len());
    let all = ResolvedWorkspace::all();
    entries.push(workspace_entry(store, all, "complete", Vec::<&str>::new())?);

    for repo in &repos {
        entries.push(workspace_entry(
            store,
            ResolvedWorkspace::repo(repo),
            "partial",
            vec!["note-landmarks", "note-search"],
        )?);
    }
    for vault in &vaults {
        entries.push(workspace_entry(
            store,
            ResolvedWorkspace::vault(vault),
            "partial",
            vec!["code-landmarks", "code-search"],
        )?);
    }

    Ok(entries)
}

fn workspace_entry(
    store: &GraphStore,
    workspace: ResolvedWorkspace,
    result: &str,
    unsupported: Vec<&str>,
) -> Result<WorkspaceEntry, ApiError> {
    Ok(WorkspaceEntry {
        id: workspace.id.clone(),
        kind: workspace.kind.as_str().to_string(),
        label: workspace.label.clone(),
        uid: workspace.uid.clone(),
        counts: workspace_counts(store, &workspace)?,
        meta: p1_meta(
            &workspace,
            result,
            unsupported,
            vec![P1Provenance::local_graph_store("workspace catalog")],
            None,
        ),
    })
}

fn is_workspace_scope_value(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed == "all"
        || trimmed.starts_with("repo:")
        || trimmed.starts_with("vault:")
        || trimmed.starts_with("vlt:")
}

fn repo_workspace_id(uid: &str) -> String {
    format!("repo:{uid}")
}

fn vault_workspace_id(uid: &str) -> String {
    format!("vault:{uid}")
}

fn repo_label(repo: &Repo) -> String {
    if let Some(name) = repo.name.as_ref().filter(|name| !name.is_empty()) {
        return name.clone();
    }

    repo.url
        .trim_end_matches('/')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .filter(|segment| !segment.is_empty())
        .unwrap_or(&repo.uid)
        .to_string()
}
