use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::ApiError;
use crate::rank_events::with_rank_event;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Result<Response, ApiError> {
    let q = params.q.unwrap_or_default();
    if q.is_empty() {
        return Err(ApiError::bad_request("query parameter 'q' is required"));
    }
    let limit = params.limit.unwrap_or(20).min(1000);
    let results = nestweaver_engine::search_symbols(&state.store, &q, limit)?;
    let json = serde_json::to_value(&results)?;
    Ok(Json(json).into_response())
}

/// nw-113: a file's symbol list as JSON, its `<module>` owner labelled
/// `path::<module>` rather than bare.
fn labelled_symbols_json(
    symbols: Vec<nestweaver_schema::Symbol>,
) -> serde_json::Result<serde_json::Value> {
    let symbols: Vec<_> = symbols
        .into_iter()
        .map(|mut symbol| {
            symbol.name = symbol.display_name().into_owned();
            symbol
        })
        .collect();
    serde_json::to_value(&symbols)
}

#[derive(Deserialize, Default)]
pub struct SymbolLookupParams {
    pub kind: Option<String>,
}

fn is_file_kind(kind: Option<&str>) -> bool {
    kind.is_some_and(|k| k.eq_ignore_ascii_case("file"))
}

async fn symbols_for_file(state: &AppState, path: &str) -> Result<Response, ApiError> {
    let symbols = state.store.symbols_in_file(path)?;
    if symbols.is_empty() {
        return Err(ApiError::not_found(format!("file '{path}' not found")));
    }
    let json = labelled_symbols_json(symbols)?;
    Ok(Json(json).into_response())
}

pub async fn symbol_by_uid(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
    Query(params): Query<SymbolLookupParams>,
) -> Result<Response, ApiError> {
    if uid.starts_with("note:") {
        return crate::routes::brain::note_by_uid(State(state), Path(uid)).await;
    }

    if is_file_kind(params.kind.as_deref()) {
        return symbols_for_file(&state, &uid).await;
    }

    match nestweaver_engine::lookup_symbol(&state.store, &uid, None)? {
        nestweaver_engine::LookupResult::Found(detail) => {
            let json = serde_json::to_value(&*detail)?;
            Ok(Json(json).into_response())
        }
        nestweaver_engine::LookupResult::NotFound => {
            let symbols = state.store.symbols_in_file(&uid)?;
            if !symbols.is_empty() {
                let json = labelled_symbols_json(symbols)?;
                return Ok(Json(json).into_response());
            }
            Err(ApiError::not_found(format!("symbol '{uid}' not found")))
        }
        nestweaver_engine::LookupResult::Ambiguous(candidates) => {
            let candidate_uids: Vec<String> = candidates.iter().map(|c| c.uid.clone()).collect();
            let json = serde_json::json!({
                "error": "ambiguous",
                "status": "ambiguous",
                "candidates": candidates,
                "candidate_uids": candidate_uids,
            });
            // fetch() treats a bare 300 (no Location) as a network failure.
            Ok((StatusCode::CONFLICT, Json(json)).into_response())
        }
    }
}

#[derive(Deserialize)]
pub struct FileParams {
    pub path: String,
}

pub async fn symbols_in_file(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FileParams>,
) -> Result<Response, ApiError> {
    let symbols = state.store.symbols_in_file(&params.path)?;
    let json = labelled_symbols_json(symbols)?;
    Ok(Json(json).into_response())
}

#[derive(Deserialize)]
pub struct TopParams {
    pub limit: Option<usize>,
    pub workspace: Option<String>,
    pub scope: Option<String>,
}

pub async fn symbols_top(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TopParams>,
) -> Result<Response, ApiError> {
    let limit = params.limit.unwrap_or(20).min(1000);
    // `symbols_by_pagerank` triggers the lazy PageRank compute on a cold cache,
    // so run it off the async runtime and emit `pagerank:recomputed` if it fired.
    let state2 = state.clone();
    with_rank_event(&state, move || {
        // A dirty index publication fails ranking closed: surface 503 so the
        // UI says "ranking unavailable" instead of rendering an empty list.
        let workspace = crate::routes::workspaces::resolve_workspace(
            &state2.store,
            crate::routes::workspaces::workspace_param(
                params.workspace.as_deref(),
                params.scope.as_deref(),
            ),
        )?;
        let symbols = if workspace.kind == crate::routes::workspaces::WorkspaceKind::All {
            state2
                .store
                .symbols_by_pagerank(Some(limit))
                .map_err(|e| ApiError::from_ranking(e.into()))?
        } else {
            crate::routes::workspaces::symbols_page(&state2.store, &workspace, None, limit)?.items
        };
        let json = serde_json::to_value(&symbols)?;
        Ok(Json(json).into_response())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nw-113: the file's symbol list labels its owner `path::<module>`.
    #[tokio::test]
    async fn symbols_in_file_labels_the_module_owner() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.js"), "function f() {}\nf();\n").unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        nestweaver_engine::index_directory_with_store(
            &store,
            &src,
            &db_path,
            "test",
            "https://example.com/r",
            "sha",
            false,
            None,
        )
        .unwrap();
        let state = AppState::new(store, None, db_path);
        let response = symbols_in_file(
            State(state),
            Query(FileParams {
                path: "a.js".to_string(),
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("the file lists its symbols"));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let symbols: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let mut names: Vec<&str> = symbols
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        names.sort();
        assert_eq!(names, ["a.js::<module>", "f"], "{symbols}");
    }

    /// During a dirty index publication the top-symbols route must say
    /// "ranking unavailable" (503), never answer 200 with an empty list — an
    /// empty list is indistinguishable from a graph with no ranked symbols
    /// (the `ranking.rs` dirty-publication contract).
    #[tokio::test]
    async fn symbols_top_reports_ranking_unavailable_during_dirty_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        std::fs::write(format!("{}.index-dirty", db_path.display()), b"dirty").unwrap();
        let state = AppState::new(store, None, db_path);

        let error = match symbols_top(
            State(state),
            Query(TopParams {
                limit: None,
                workspace: None,
                scope: None,
            }),
        )
        .await
        {
            Ok(_) => panic!("a dirty publication must not render a successful top-symbols list"),
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
