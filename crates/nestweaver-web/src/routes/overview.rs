use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct OverviewParams {
    pub limit: Option<usize>,
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
}

pub async fn overview(
    State(state): State<Arc<AppState>>,
    Query(params): Query<OverviewParams>,
) -> Result<Response, ApiError> {
    let limit = params.limit.unwrap_or(24).clamp(6, 100);

    let repos = nestweaver_engine::list_repos(&state.store, None)?;
    let services = nestweaver_engine::list_services(&state.store, None)?;
    let top_symbols = state.store.symbols_by_pagerank(Some(limit))?;
    let vaults = state.store.list_vaults(None)?;
    let mut notes = state.store.list_notes_lite(None)?;
    let note_count = state.store.count_notes()?;
    let symbol_count = state.store.count_symbols()?;

    notes.sort_by(|a, b| {
        b.pagerank_score
            .partial_cmp(&a.pagerank_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    notes.truncate(limit.min(12));

    let mut gaps = Vec::new();
    if symbol_count > 0 && state.store.count_references_code_edges()? == 0 {
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
            repo_count: repos.len(),
            service_count: services.len(),
            vault_count: vaults.len(),
            note_count,
            symbol_count,
            gap_count: gaps.len(),
        },
        landmarks,
        start_here,
        gaps,
    };

    Ok(Json(response).into_response())
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
