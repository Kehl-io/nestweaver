use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::routes::workspaces::{self, P1Meta, P1Provenance, ResolvedWorkspace, WorkspaceKind};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct OverviewParams {
    pub limit: Option<usize>,
    pub workspace: Option<String>,
    pub scope: Option<String>,
}

#[derive(Clone, Serialize)]
struct OverviewLandmark {
    uid: String,
    kind: String,
    label: String,
    location: String,
    score: f64,
    reason: String,
}

#[derive(Serialize)]
struct OverviewCounts {
    repo_count: usize,
    service_count: usize,
    vault_count: usize,
    note_count: usize,
    symbol_count: usize,
    gap_count: usize,
}

#[derive(Serialize)]
struct OverviewGap {
    kind: String,
    label: String,
    detail: String,
}

#[derive(Serialize)]
struct OverviewResponse {
    counts: OverviewCounts,
    landmarks: Vec<OverviewLandmark>,
    start_here: Vec<OverviewLandmark>,
    gaps: Vec<OverviewGap>,
    #[serde(rename = "_meta")]
    meta: P1Meta,
}

pub async fn overview(
    State(state): State<Arc<AppState>>,
    Query(params): Query<OverviewParams>,
) -> Result<Response, ApiError> {
    let limit = params.limit.unwrap_or(24).clamp(6, 100);
    let workspace = workspaces::resolve_workspace(
        &state.store,
        workspaces::workspace_param(params.workspace.as_deref(), params.scope.as_deref()),
    )?;

    let (repos, services, top_symbols, _vaults, mut notes, counts, meta) =
        overview_scope_data(&state, &workspace, limit)?;

    notes.sort_by(|a, b| {
        b.pagerank_score
            .partial_cmp(&a.pagerank_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    notes.truncate(limit.min(12));

    let mut gaps = Vec::new();
    if workspace.kind == WorkspaceKind::All
        && counts.symbol_count > 0
        && state.store.count_references_code_edges()? == 0
    {
        gaps.push(OverviewGap {
            kind: "documentation".to_string(),
            label: "Code-documentation links missing".to_string(),
            detail: "No note-to-code reference edges have been detected yet.".to_string(),
        });
    }

    let repo_landmarks = repos
        .iter()
        .map(|repo| OverviewLandmark {
            uid: repo.uid.clone(),
            kind: "repo".to_string(),
            label: repo_label(repo),
            location: repo.url.clone(),
            score: 1.0,
            reason: "Indexed repository".to_string(),
        })
        .collect();
    let service_landmarks = services
        .iter()
        .map(|service| OverviewLandmark {
            uid: service.uid.clone(),
            kind: "service".to_string(),
            label: service.name.clone(),
            location: service.repo_uid.clone(),
            score: 0.9,
            reason: "Detected service".to_string(),
        })
        .collect();
    let symbol_landmarks = top_symbols
        .iter()
        .map(|symbol| OverviewLandmark {
            uid: symbol.uid.clone(),
            kind: "symbol".to_string(),
            label: symbol.name.clone(),
            location: symbol.file_path.clone(),
            score: symbol.pagerank_score.unwrap_or(0.0),
            reason: "High PageRank symbol".to_string(),
        })
        .collect();
    let note_landmarks = notes
        .iter()
        .map(|note| OverviewLandmark {
            uid: note.uid.clone(),
            kind: "note".to_string(),
            label: note.title.clone(),
            location: note.file_path.clone(),
            score: note.pagerank_score,
            reason: "High PageRank note".to_string(),
        })
        .collect();
    let landmarks = select_landmarks(
        repo_landmarks,
        service_landmarks,
        symbol_landmarks,
        note_landmarks,
        limit,
    );

    let start_here = landmarks.iter().take(8).cloned().collect();
    let response = OverviewResponse {
        counts: OverviewCounts {
            gap_count: gaps.len(),
            ..counts
        },
        landmarks,
        start_here,
        gaps,
        meta,
    };

    Ok(Json(response).into_response())
}

fn overview_scope_data(
    state: &Arc<AppState>,
    workspace: &ResolvedWorkspace,
    limit: usize,
) -> Result<
    (
        Vec<nestweaver_schema::Repo>,
        Vec<nestweaver_schema::Service>,
        Vec<nestweaver_schema::Symbol>,
        Vec<nestweaver_schema::Vault>,
        Vec<nestweaver_store::NoteLite>,
        OverviewCounts,
        P1Meta,
    ),
    ApiError,
> {
    match workspace.kind {
        WorkspaceKind::All => {
            let repos = nestweaver_engine::list_repos(&state.store, None)?;
            let services = nestweaver_engine::list_services(&state.store, None)?;
            let top_symbols = state.store.symbols_by_pagerank(Some(limit))?;
            let vaults = state.store.list_vaults(None)?;
            let notes = state.store.list_notes_lite(None)?;
            let counts = OverviewCounts {
                repo_count: repos.len(),
                service_count: services.len(),
                vault_count: vaults.len(),
                note_count: state.store.count_notes()?,
                symbol_count: state.store.count_symbols()?,
                gap_count: 0,
            };
            let meta = workspaces::p1_meta(
                workspace,
                "complete",
                Vec::<&str>::new(),
                vec![P1Provenance::local_graph_store("overview landmarks")],
                Some(limit),
            );
            Ok((repos, services, top_symbols, vaults, notes, counts, meta))
        }
        WorkspaceKind::Repo => {
            let repo_uid = workspace.uid.as_deref().unwrap_or_default();
            let repos: Vec<_> = nestweaver_engine::list_repos(&state.store, None)?
                .into_iter()
                .filter(|repo| repo.uid == repo_uid)
                .collect();
            let services = workspaces::services_for_repo(&state.store, repo_uid)?;
            let mut top_symbols = workspaces::symbols_for_repo(&state.store, repo_uid)?;
            let symbol_count = top_symbols.len();
            top_symbols.truncate(limit);
            let counts = OverviewCounts {
                repo_count: repos.len(),
                service_count: services.len(),
                vault_count: 0,
                note_count: 0,
                symbol_count,
                gap_count: 0,
            };
            let meta = workspaces::p1_meta(
                workspace,
                "partial",
                vec!["note-landmarks"],
                vec![P1Provenance::local_graph_store("repo overview landmarks")],
                Some(limit),
            );
            Ok((
                repos,
                services,
                top_symbols,
                Vec::new(),
                Vec::new(),
                counts,
                meta,
            ))
        }
        WorkspaceKind::Vault => {
            let vault_uid = workspace.uid.as_deref().unwrap_or_default();
            let vaults: Vec<_> = state
                .store
                .list_vaults(None)?
                .into_iter()
                .filter(|vault| vault.uid == vault_uid)
                .collect();
            let notes = state.store.list_notes_lite(Some(vault_uid))?;
            let counts = OverviewCounts {
                repo_count: 0,
                service_count: 0,
                vault_count: vaults.len(),
                note_count: notes.len(),
                symbol_count: 0,
                gap_count: 0,
            };
            let meta = workspaces::p1_meta(
                workspace,
                "partial",
                vec!["code-landmarks"],
                vec![P1Provenance::local_graph_store("vault overview landmarks")],
                Some(limit),
            );
            Ok((
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vaults,
                notes,
                counts,
                meta,
            ))
        }
    }
}

fn select_landmarks(
    repos: Vec<OverviewLandmark>,
    services: Vec<OverviewLandmark>,
    symbols: Vec<OverviewLandmark>,
    notes: Vec<OverviewLandmark>,
    limit: usize,
) -> Vec<OverviewLandmark> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();

    if let Some(symbol) = symbols.first() {
        push_landmark(&mut selected, &mut seen, symbol.clone(), limit);
    }
    if let Some(note) = notes.first() {
        push_landmark(&mut selected, &mut seen, note.clone(), limit);
    }

    for landmark in repos.iter().take(4) {
        push_landmark(&mut selected, &mut seen, landmark.clone(), limit);
    }
    for landmark in services.iter().take(4) {
        push_landmark(&mut selected, &mut seen, landmark.clone(), limit);
    }

    let mut candidates = Vec::new();
    candidates.extend(repos);
    candidates.extend(services);
    candidates.extend(symbols);
    candidates.extend(notes);
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for landmark in candidates {
        push_landmark(&mut selected, &mut seen, landmark, limit);
        if selected.len() >= limit {
            break;
        }
    }

    selected.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    selected.truncate(limit);
    selected
}

fn push_landmark(
    selected: &mut Vec<OverviewLandmark>,
    seen: &mut HashSet<String>,
    landmark: OverviewLandmark,
    limit: usize,
) {
    if selected.len() < limit && seen.insert(landmark.uid.clone()) {
        selected.push(landmark);
    }
}

fn repo_label(repo: &nestweaver_schema::Repo) -> String {
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
