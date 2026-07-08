use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use nestweaver_schema::{Note, Project, Repo, Service, Symbol, Vault};
use nestweaver_store::{GraphStore, NoteLite};
use serde::Serialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceKind {
    All,
    Project,
    Repo,
    Vault,
}

impl WorkspaceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Project => "project",
            Self::Repo => "repo",
            Self::Vault => "vault",
        }
    }

    fn data_scope(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Project => "project-scoped",
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

    pub fn project(project: &Project) -> Self {
        Self {
            id: project_workspace_id(&project.uid),
            kind: WorkspaceKind::Project,
            uid: Some(project.uid.clone()),
            label: project.name.clone(),
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
    pub project_count: usize,
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

pub struct BoundedResults<T> {
    pub items: Vec<T>,
    pub total_count: usize,
}

impl<T> BoundedResults<T> {
    pub fn omitted_count(&self) -> usize {
        self.total_count.saturating_sub(self.items.len())
    }

    pub fn is_truncated(&self) -> bool {
        self.omitted_count() > 0
    }

    pub fn result_state(&self, success: &'static str) -> &'static str {
        if self.is_truncated() {
            "truncated"
        } else if self.items.is_empty() {
            "no-match"
        } else {
            success
        }
    }
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

/// Upper bound on entries returned by the workspace catalog. Per-project
/// counts are O(store) each, so an unbounded catalog would scale with the
/// number of registered projects/repos/vaults; bound it and disclose the
/// truncation in `_meta`.
const WORKSPACE_CATALOG_LIMIT: usize = 500;

pub async fn workspaces(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let entries = workspace_entries(&state.store)?;
    let all = ResolvedWorkspace::all();
    let meta = p1_meta_for_result_set(
        &all,
        entries.result_state("complete"),
        Vec::<&str>::new(),
        vec![P1Provenance::local_graph_store("workspace catalog")],
        Some(WORKSPACE_CATALOG_LIMIT),
        entries.items.len(),
        Some(entries.total_count),
    );
    Ok(Json(WorkspaceCatalogResponse {
        workspaces: entries.items,
        meta,
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

    let projects = store.list_projects()?;
    if let Some(project) = projects.iter().find(|project| project.uid == raw) {
        return Ok(ResolvedWorkspace::project(project));
    }
    if let Some(uid_or_name) = raw.strip_prefix("project:")
        && let Some(project) = projects
            .iter()
            .find(|project| project.uid == uid_or_name || project.name == uid_or_name)
    {
        return Ok(ResolvedWorkspace::project(project));
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
            project_count: store.list_projects()?.len(),
            repo_count: store.list_repos(None)?.len(),
            service_count: store.list_services(None)?.len(),
            vault_count: store.list_vaults(None)?.len(),
            note_count: store.count_notes()?,
            symbol_count: store.count_symbols()?,
        }),
        WorkspaceKind::Project => {
            let uid = workspace.uid.as_deref().unwrap_or_default();
            project_counts(store, uid)
        }
        WorkspaceKind::Repo => {
            let uid = workspace.uid.as_deref().unwrap_or_default();
            Ok(WorkspaceCounts {
                project_count: 0,
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
                project_count: 0,
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
            result: result.to_string(),
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

pub fn p1_meta_for_result_set(
    workspace: &ResolvedWorkspace,
    result: &str,
    unsupported: Vec<&str>,
    provenance: Vec<P1Provenance>,
    limit: Option<usize>,
    returned_count: usize,
    total_count: Option<usize>,
) -> P1Meta {
    let truncated = result == "truncated"
        || limit.is_some_and(|limit| {
            if let Some(total_count) = total_count {
                total_count > returned_count
            } else {
                limit > 0 && returned_count >= limit
            }
        });
    let result = if truncated && result != "no-match" {
        "truncated"
    } else {
        result
    };
    let mut meta = p1_meta(workspace, result, unsupported, provenance, limit);
    meta.truncation.truncated = truncated;
    meta.truncation.omitted_count = total_count
        .map(|total_count| total_count.saturating_sub(returned_count))
        .filter(|omitted_count| *omitted_count > 0);
    meta.continuation.has_more = truncated;
    meta.continuation.reason = truncated.then(|| "result-limit".to_string());
    meta
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

pub fn services_for_project(
    store: &GraphStore,
    project_uid: &str,
) -> Result<Vec<Service>, ApiError> {
    let repo_uids: HashSet<String> = symbols_for_project(store, project_uid)?
        .into_iter()
        .map(|symbol| symbol.repo_uid)
        .collect();
    Ok(store
        .list_services(None)?
        .into_iter()
        .filter(|service| repo_uids.contains(&service.repo_uid))
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

pub fn symbols_for_project(store: &GraphStore, project_uid: &str) -> Result<Vec<Symbol>, ApiError> {
    let mut symbols: Vec<Symbol> = store
        .list_project_symbol_uids(project_uid)?
        .into_iter()
        .filter_map(|uid| store.lookup_symbol(&uid).ok())
        .collect();
    symbols.sort_by(|a, b| {
        b.pagerank_score
            .unwrap_or(0.0)
            .partial_cmp(&a.pagerank_score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(symbols)
}

pub fn notes_for_project(store: &GraphStore, project_uid: &str) -> Result<Vec<Note>, ApiError> {
    let mut notes: Vec<Note> = store
        .list_project_note_uids(project_uid)?
        .into_iter()
        .filter_map(|uid| store.lookup_note(&uid).ok())
        .collect();
    notes.sort_by(|a, b| {
        b.pagerank_score
            .unwrap_or(0.0)
            .partial_cmp(&a.pagerank_score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(notes)
}

pub fn note_lites_for_project(
    store: &GraphStore,
    project_uid: &str,
) -> Result<Vec<NoteLite>, ApiError> {
    Ok(notes_for_project(store, project_uid)?
        .into_iter()
        .map(|note| NoteLite {
            uid: note.uid,
            title: note.title,
            file_path: note.file_path,
            vault_uid: note.vault_uid,
            pagerank_score: note.pagerank_score.unwrap_or(0.0),
        })
        .collect())
}

pub fn repos_for_project(store: &GraphStore, project_uid: &str) -> Result<Vec<Repo>, ApiError> {
    let repo_uids: HashSet<String> = symbols_for_project(store, project_uid)?
        .into_iter()
        .map(|symbol| symbol.repo_uid)
        .collect();
    Ok(store
        .list_repos(None)?
        .into_iter()
        .filter(|repo| repo_uids.contains(&repo.uid))
        .collect())
}

pub fn vaults_for_project(store: &GraphStore, project_uid: &str) -> Result<Vec<Vault>, ApiError> {
    let vault_uids: HashSet<String> = notes_for_project(store, project_uid)?
        .into_iter()
        .map(|note| note.vault_uid)
        .collect();
    Ok(store
        .list_vaults(None)?
        .into_iter()
        .filter(|vault| vault_uids.contains(&vault.uid))
        .collect())
}

fn project_counts(store: &GraphStore, project_uid: &str) -> Result<WorkspaceCounts, ApiError> {
    let symbols = symbols_for_project(store, project_uid)?;
    let notes = notes_for_project(store, project_uid)?;
    let repo_uids: HashSet<&str> = symbols
        .iter()
        .map(|symbol| symbol.repo_uid.as_str())
        .collect();
    let vault_uids: HashSet<&str> = notes.iter().map(|note| note.vault_uid.as_str()).collect();
    let service_count = store
        .list_services(None)?
        .into_iter()
        .filter(|service| repo_uids.contains(service.repo_uid.as_str()))
        .count();

    Ok(WorkspaceCounts {
        project_count: 1,
        repo_count: repo_uids.len(),
        service_count,
        vault_count: vault_uids.len(),
        note_count: notes.len(),
        symbol_count: symbols.len(),
    })
}

pub fn notes_for_query(
    store: &GraphStore,
    query: &str,
    workspace: &ResolvedWorkspace,
    limit: usize,
) -> Result<BoundedResults<Note>, ApiError> {
    let needle = query.to_lowercase();
    let mut notes: Vec<Note> = match workspace.kind {
        WorkspaceKind::All => store.list_notes(None)?,
        WorkspaceKind::Project => {
            notes_for_project(store, workspace.uid.as_deref().unwrap_or_default())?
        }
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
    let total_count = notes.len();
    notes.truncate(limit);
    Ok(BoundedResults {
        items: notes,
        total_count,
    })
}

pub fn symbols_for_query(
    store: &GraphStore,
    query: &str,
    workspace: &ResolvedWorkspace,
    limit: usize,
) -> Result<BoundedResults<Symbol>, ApiError> {
    let needle = query.to_lowercase();
    let mut symbols: Vec<Symbol> = match workspace.kind {
        WorkspaceKind::All => store.list_all_symbols()?,
        WorkspaceKind::Project => {
            symbols_for_project(store, workspace.uid.as_deref().unwrap_or_default())?
        }
        WorkspaceKind::Repo => {
            symbols_for_repo(store, workspace.uid.as_deref().unwrap_or_default())?
        }
        WorkspaceKind::Vault => Vec::new(),
    }
    .into_iter()
    .filter(|symbol| {
        symbol.name.to_lowercase().contains(&needle)
            || symbol.file_path.to_lowercase().contains(&needle)
            || symbol.signature.to_lowercase().contains(&needle)
    })
    .collect();
    symbols.sort_by(|a, b| {
        b.pagerank_score
            .unwrap_or(0.0)
            .partial_cmp(&a.pagerank_score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let total_count = symbols.len();
    symbols.truncate(limit);
    Ok(BoundedResults {
        items: symbols,
        total_count,
    })
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

pub fn symbol_search_hit(symbol: Symbol) -> serde_json::Value {
    json!({
        "uid": symbol.uid,
        "kind": "Symbol",
        "name": symbol.name,
        "title": symbol.name,
        "repo_uid": symbol.repo_uid,
        "file_path": symbol.file_path,
        "score": symbol.pagerank_score.unwrap_or(0.0),
    })
}

fn workspace_entries(store: &GraphStore) -> Result<BoundedResults<WorkspaceEntry>, ApiError> {
    let projects = store.list_projects()?;
    let repos = store.list_repos(None)?;
    let vaults = store.list_vaults(None)?;
    let services = store.list_services(None)?;
    let symbols = store.list_all_symbols()?;
    let notes = store.list_notes_lite(None)?;

    let mut service_counts_by_repo = HashMap::<String, usize>::new();
    for service in &services {
        *service_counts_by_repo
            .entry(service.repo_uid.clone())
            .or_default() += 1;
    }

    let mut symbol_counts_by_repo = HashMap::<String, usize>::new();
    for symbol in &symbols {
        *symbol_counts_by_repo
            .entry(symbol.repo_uid.clone())
            .or_default() += 1;
    }

    let mut note_counts_by_vault = HashMap::<String, usize>::new();
    for note in &notes {
        *note_counts_by_vault
            .entry(note.vault_uid.clone())
            .or_default() += 1;
    }

    let total_count = 1 + projects.len() + repos.len() + vaults.len();
    let mut entries = Vec::with_capacity(total_count.min(WORKSPACE_CATALOG_LIMIT));
    let all = ResolvedWorkspace::all();
    entries.push(workspace_entry(
        all,
        WorkspaceCounts {
            project_count: projects.len(),
            repo_count: repos.len(),
            service_count: services.len(),
            vault_count: vaults.len(),
            note_count: notes.len(),
            symbol_count: symbols.len(),
        },
        "complete",
        Vec::<&str>::new(),
    ));

    for project in &projects {
        if entries.len() >= WORKSPACE_CATALOG_LIMIT {
            break;
        }
        entries.push(workspace_entry(
            ResolvedWorkspace::project(project),
            project_counts(store, &project.uid)?,
            "partial",
            vec!["project-components"],
        ));
    }

    for repo in &repos {
        if entries.len() >= WORKSPACE_CATALOG_LIMIT {
            break;
        }
        entries.push(workspace_entry(
            ResolvedWorkspace::repo(repo),
            WorkspaceCounts {
                project_count: 0,
                repo_count: 1,
                service_count: *service_counts_by_repo.get(&repo.uid).unwrap_or(&0),
                vault_count: 0,
                note_count: 0,
                symbol_count: *symbol_counts_by_repo.get(&repo.uid).unwrap_or(&0),
            },
            "partial",
            vec!["note-landmarks", "note-search"],
        ));
    }
    for vault in &vaults {
        if entries.len() >= WORKSPACE_CATALOG_LIMIT {
            break;
        }
        entries.push(workspace_entry(
            ResolvedWorkspace::vault(vault),
            WorkspaceCounts {
                project_count: 0,
                repo_count: 0,
                service_count: 0,
                vault_count: 1,
                note_count: *note_counts_by_vault.get(&vault.uid).unwrap_or(&0),
                symbol_count: 0,
            },
            "partial",
            vec!["code-landmarks", "code-search"],
        ));
    }

    Ok(BoundedResults {
        items: entries,
        total_count,
    })
}

fn workspace_entry(
    workspace: ResolvedWorkspace,
    counts: WorkspaceCounts,
    result: &str,
    unsupported: Vec<&str>,
) -> WorkspaceEntry {
    WorkspaceEntry {
        id: workspace.id.clone(),
        kind: workspace.kind.as_str().to_string(),
        label: workspace.label.clone(),
        uid: workspace.uid.clone(),
        counts,
        meta: p1_meta(
            &workspace,
            result,
            unsupported,
            vec![P1Provenance::local_graph_store("workspace catalog")],
            None,
        ),
    }
}

fn is_workspace_scope_value(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed == "all"
        || trimmed.starts_with("repo:")
        || trimmed.starts_with("project:")
        || trimmed.starts_with("vault:")
        || trimmed.starts_with("vlt:")
}

fn repo_workspace_id(uid: &str) -> String {
    format!("repo:{uid}")
}

fn project_workspace_id(uid: &str) -> String {
    format!("project:{uid}")
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
