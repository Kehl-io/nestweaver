use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::rank_events::with_rank_event;
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
    /// Betweenness-centrality emphasis for bridge glyphs. Set only on the
    /// top scene bridges (at most `crate::bridge::SCENE_BRIDGE_LIMIT`),
    /// normalized 0..=1 by the scene maximum; omitted everywhere else.
    #[serde(skip_serializing_if = "Option::is_none")]
    bridge_score: Option<f32>,
}

#[derive(Serialize)]
struct OverviewCounts {
    project_count: usize,
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

struct OverviewMetaState {
    result: &'static str,
    unsupported: Vec<&'static str>,
    provenance: P1Provenance,
    total_landmark_count: usize,
}

pub async fn overview(
    State(state): State<Arc<AppState>>,
    Query(params): Query<OverviewParams>,
) -> Result<Response, ApiError> {
    // The overview aggregates `symbols_by_pagerank`, which triggers the lazy
    // PageRank compute on a cold cache. Run the store work off the async
    // runtime and emit `pagerank:recomputed` if a (re)compute fired.
    let state2 = state.clone();
    with_rank_event(&state, move || overview_response(&state2, &params)).await
}

fn overview_response(state: &Arc<AppState>, params: &OverviewParams) -> Result<Response, ApiError> {
    let limit = params.limit.unwrap_or(24).clamp(6, 100);
    let workspace = workspaces::resolve_workspace(
        &state.store,
        workspaces::workspace_param(params.workspace.as_deref(), params.scope.as_deref()),
    )?;

    let (repos, services, top_symbols, _vaults, mut notes, counts, meta_state) =
        overview_scope_data(state, &workspace, limit)?;

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
            bridge_score: None,
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
            bridge_score: None,
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
            bridge_score: None,
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
            bridge_score: None,
        })
        .collect();
    let mut landmarks = select_landmarks(
        repo_landmarks,
        service_landmarks,
        symbol_landmarks,
        note_landmarks,
        limit,
    );

    // Bridge glyph emphasis: intersect this scene's landmarks with the
    // cached global betweenness pool (see crate::bridge) and mark only the
    // top scene bridges, normalized by the scene max. Additive optional
    // field — landmarks that are not bridges serialize unchanged.
    let bridge_scores = crate::bridge::scene_bridge_scores(
        &crate::bridge::global_bridge_scores(state),
        landmarks.iter().map(|landmark| landmark.uid.clone()),
    );
    for landmark in &mut landmarks {
        landmark.bridge_score = bridge_scores.get(&landmark.uid).copied();
    }

    let meta = workspaces::p1_meta_for_result_set(
        &workspace,
        meta_state.result,
        meta_state.unsupported,
        vec![meta_state.provenance],
        Some(limit),
        landmarks.len(),
        Some(meta_state.total_landmark_count),
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

type OverviewScopeData = (
    Vec<nestweaver_schema::Repo>,
    Vec<nestweaver_schema::Service>,
    Vec<nestweaver_schema::Symbol>,
    Vec<nestweaver_schema::Vault>,
    Vec<nestweaver_store::NoteLite>,
    OverviewCounts,
    OverviewMetaState,
);

fn overview_scope_data(
    state: &Arc<AppState>,
    workspace: &ResolvedWorkspace,
    limit: usize,
) -> Result<OverviewScopeData, ApiError> {
    match workspace.kind {
        WorkspaceKind::All => {
            let projects = state.store.list_projects()?;
            let repos = nestweaver_engine::list_repos(&state.store, None)?;
            let services = nestweaver_engine::list_services(&state.store, None)?;
            // A dirty index publication fails ranking closed: surface 503 so
            // the UI says "ranking unavailable" instead of rendering an
            // overview with an empty landmark list.
            let top_symbols = state
                .store
                .symbols_by_pagerank(Some(limit))
                .map_err(|e| ApiError::from_ranking(e.into()))?;
            let vaults = state.store.list_vaults(None)?;
            let notes = state.store.list_notes_lite(None)?;
            let counts = OverviewCounts {
                project_count: projects.len(),
                repo_count: repos.len(),
                service_count: services.len(),
                vault_count: vaults.len(),
                note_count: state.store.count_notes()?,
                symbol_count: state.store.count_symbols()?,
                gap_count: 0,
            };
            let total_landmark_count =
                counts.repo_count + counts.service_count + counts.symbol_count + counts.note_count;
            let meta = OverviewMetaState {
                result: "complete",
                unsupported: Vec::new(),
                provenance: P1Provenance::local_graph_store("overview landmarks"),
                total_landmark_count,
            };
            Ok((repos, services, top_symbols, vaults, notes, counts, meta))
        }
        WorkspaceKind::Project => {
            let project_uid = workspace.uid.as_deref().unwrap_or_default();
            let repos = workspaces::repos_for_project(&state.store, project_uid)?;
            let services = workspaces::services_for_project(&state.store, project_uid)?;
            let vaults = workspaces::vaults_for_project(&state.store, project_uid)?;
            let mut top_symbols = workspaces::symbols_for_project(&state.store, project_uid)?;
            let symbol_count = top_symbols.len();
            top_symbols.truncate(limit);
            let notes = workspaces::note_lites_for_project(&state.store, project_uid)?;
            let note_count = notes.len();
            let counts = OverviewCounts {
                project_count: 1,
                repo_count: repos.len(),
                service_count: services.len(),
                vault_count: vaults.len(),
                note_count,
                symbol_count,
                gap_count: 0,
            };
            let total_landmark_count =
                counts.repo_count + counts.service_count + symbol_count + note_count;
            let meta = OverviewMetaState {
                result: "partial",
                unsupported: vec!["project-components"],
                provenance: P1Provenance::local_graph_store("project overview landmarks"),
                total_landmark_count,
            };
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
                project_count: 0,
                repo_count: repos.len(),
                service_count: services.len(),
                vault_count: 0,
                note_count: 0,
                symbol_count,
                gap_count: 0,
            };
            let total_landmark_count = counts.repo_count + counts.service_count + symbol_count;
            let meta = OverviewMetaState {
                result: "partial",
                unsupported: vec!["note-landmarks"],
                provenance: P1Provenance::local_graph_store("repo overview landmarks"),
                total_landmark_count,
            };
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
                project_count: 0,
                repo_count: 0,
                service_count: 0,
                vault_count: vaults.len(),
                note_count: notes.len(),
                symbol_count: 0,
                gap_count: 0,
            };
            let total_landmark_count = notes.len();
            let meta = OverviewMetaState {
                result: "partial",
                unsupported: vec!["code-landmarks"],
                provenance: P1Provenance::local_graph_store("vault overview landmarks"),
                total_landmark_count,
            };
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    /// Same contract as the top-symbols route: a dirty index publication must
    /// surface as 503 "ranking unavailable", not as a 200 overview with an
    /// empty landmark list (the `ranking.rs` dirty-publication contract).
    #[tokio::test]
    async fn overview_reports_ranking_unavailable_during_dirty_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        std::fs::write(format!("{}.index-dirty", db_path.display()), b"dirty").unwrap();
        let state = AppState::new(store, None, db_path);

        let error = match overview(
            State(state),
            Query(OverviewParams {
                limit: None,
                workspace: None,
                scope: None,
            }),
        )
        .await
        {
            Ok(_) => panic!("a dirty publication must not render a successful overview"),
            Err(error) => error,
        };

        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            error.message.contains("ranking"),
            "the error must name ranking as unavailable: {}",
            error.message
        );
    }
}
