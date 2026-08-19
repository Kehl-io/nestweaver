use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::rank_events::with_rank_event;
use crate::state::AppState;

pub async fn list_repos(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let repos = nestweaver_engine::list_repos(&state.store, None)?;
    let json = serde_json::to_value(&repos)?;
    Ok(Json(json).into_response())
}

pub async fn list_services(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let services = nestweaver_engine::list_services(&state.store, None)?;
    let json = serde_json::to_value(&services)?;
    Ok(Json(json).into_response())
}

#[derive(Deserialize)]
pub struct RepoMapParams {
    pub budget: Option<usize>,
}

pub async fn repo_map(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RepoMapParams>,
) -> Result<Response, ApiError> {
    let budget = params.budget.unwrap_or(2000);
    // `generate_repo_map` ranks symbols by PageRank and can trigger the lazy
    // compute, so run it off the async runtime and emit `pagerank:recomputed`
    // if a (re)compute fired.
    let state2 = state.clone();
    with_rank_event(&state, move || {
        // Same contract as the ranked routes: a dirty index publication fails
        // the repo map closed — surface 503 "ranking unavailable", not a 500.
        let map = nestweaver_engine::generate_repo_map(&state2.store, budget)
            .map_err(ApiError::from_ranking)?;
        Ok(Json(json!({ "map": map })).into_response())
    })
    .await
}

pub async fn cross_repo_refs(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
) -> Result<Response, ApiError> {
    let refs = state.store.cross_repo_links(&uid)?;
    let json = serde_json::to_value(&refs)?;
    Ok(Json(json).into_response())
}

pub async fn suggest_links(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let manifests = nestweaver_engine::load_manifest_cache_for_db(&state.store, &state.db_path)?;
    let suggestions = nestweaver_engine::suggest_links(&state.store, &manifests)?;
    let json = json!({
        "links": serde_json::to_value(&suggestions.links)?,
        "features": serde_json::to_value(&suggestions.features)?,
    });
    Ok(Json(json).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repo-map route follows the same contract as the ranked routes: a
    /// dirty index publication surfaces as 503 "ranking unavailable", not a
    /// 500 and not a successful empty map.
    #[tokio::test]
    async fn repo_map_reports_ranking_unavailable_during_dirty_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        std::fs::write(format!("{}.index-dirty", db_path.display()), b"dirty").unwrap();
        let state = AppState::new(store, None, db_path);

        let error = match repo_map(State(state), Query(RepoMapParams { budget: None })).await {
            Ok(_) => panic!("a dirty publication must not render a successful repo map"),
            Err(error) => error,
        };

        assert_eq!(error.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            error.message.contains("ranking"),
            "the error must name ranking as unavailable: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn suggest_links_does_not_trust_a_legacy_only_manifest_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        for (uid, url) in [
            ("repo:test:legacy-app", "https://example.test/legacy-app"),
            (
                "repo:test:legacy-dependency",
                "https://example.test/legacy-dependency",
            ),
        ] {
            store
                .insert_repo(&nestweaver_schema::Repo {
                    uid: uid.to_string(),
                    url: url.to_string(),
                    indexed_sha: "sha".to_string(),
                    staleness_commits_behind: 0,
                    instance_id: "test".to_string(),
                    name: None,
                    root_path: None,
                })
                .unwrap();
        }
        let manifests = std::collections::HashMap::from([
            (
                "repo:test:legacy-app".to_string(),
                nestweaver_engine::ManifestInfo {
                    package_name: Some("legacy-app-package".to_string()),
                    dependencies: vec!["legacy-dependency-package".to_string()],
                    entry_files: Vec::new(),
                },
            ),
            (
                "repo:test:legacy-dependency".to_string(),
                nestweaver_engine::ManifestInfo {
                    package_name: Some("legacy-dependency-package".to_string()),
                    dependencies: Vec::new(),
                    entry_files: Vec::new(),
                },
            ),
        ]);
        let legacy_path = db_path.with_extension("manifests.json");
        let canonical_path = nestweaver_engine::manifest_cache_path(&db_path);
        nestweaver_engine::save_manifest_cache(&manifests, &legacy_path).unwrap();
        assert!(!canonical_path.exists());
        let state = AppState::new(store, None, db_path);

        let response = suggest_links(State(state))
            .await
            .unwrap_or_else(|_| panic!("suggest_links endpoint failed"));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let suggestions: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(suggestions["links"].as_array().unwrap().is_empty());
        assert!(!canonical_path.exists());
        assert!(legacy_path.exists());
    }
}
