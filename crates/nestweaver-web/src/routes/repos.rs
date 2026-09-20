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

fn without_force_index_advice(message: &str) -> String {
    message.replace(
        "; re-index with `nestweaver index --repo <path> --force`",
        "",
    )
}

fn manifest_unavailable(
    state: &AppState,
    mut error: nestweaver_engine::manifest::ManifestUnavailable,
) -> Response {
    let rebuild = state.manifest_recovery.get().map(|runtime| {
        runtime.wake.notify_one();
        runtime.status()
    });
    if let Some(status) = &rebuild
        && let Some(source_error) = &status.error
        && error.retryable
        && source_error.reason
            == nestweaver_engine::manifest::ManifestUnavailableReason::SourceUnavailable
    {
        // A later retryable stale/pending generation must not hide a blocked
        // source failure, and must not prescribe `--force`. Recovery owns the
        // sidecar catch-up; generation can advance while the source error is
        // still the operator-relevant cause.
        error = source_error.clone();
    }
    if rebuild.is_some() && error.retryable {
        // A watching coordinator owns sidecar catch-up. A 503 that still names
        // `--force` while recovery is installed tells the operator to smash a
        // generation the watcher is already going to rebuild.
        error.message = without_force_index_advice(&error.message);
    }
    let retryable = error.retryable
        && rebuild.as_ref().is_some_and(|s| {
            matches!(
                s.state,
                "queued" | "running" | "retry_scheduled" | "deferred"
            )
        });
    let mut response = (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({
        "code": error.code, "reason": error.reason, "actual_generation": error.actual_generation,
        "expected_generation": error.expected_generation, "retryable": retryable,
        "message": error.message, "rebuild": rebuild,
        "diagnostic_id": format!("manifest-{:?}", error.reason).to_lowercase(),
    }))).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    if retryable {
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("2"),
        );
    }
    response
}

pub async fn suggest_links(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let state2 = Arc::clone(&state);
    tokio::task::spawn_blocking(move || {
        let generation = state2.store.graph_generation();
        let manifests = match nestweaver_engine::manifest::current_manifest_snapshot(
            &state2.store,
            &state2.db_path,
        ) {
            Ok(manifests) => manifests,
            Err(error) => return Ok(manifest_unavailable(&state2, error)),
        };
        let suggestions = nestweaver_engine::suggest_links(&state2.store, &manifests)?;
        if let Err(error) =
            nestweaver_engine::manifest::ensure_manifest_generation(&state2.store, generation)
        {
            return Ok(manifest_unavailable(&state2, error));
        }
        if nestweaver_engine::manifest::manifest_debt_revision(&state2.db_path)?.is_some() {
            return Ok(manifest_unavailable(
                &state2,
                nestweaver_engine::manifest::ManifestUnavailable::new(
                    nestweaver_engine::manifest::ManifestUnavailableReason::PendingSourceChange,
                    generation,
                    "manifest source changed while computing suggestions",
                ),
            ));
        }
        Ok(Json(
            json!({ "links": suggestions.links, "features": suggestions.features,
                "graph_generation": generation,
                "manifest_revision": state2.manifest_recovery.get().map(|s| s.status().revision),
            }),
        )
        .into_response())
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!("suggestions worker failed: {e}")))?
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
    async fn blocked_source_failure_overrides_later_retryable_stale_generation() {
        use nestweaver_engine::manifest::{
            ManifestRecoveryRuntime, ManifestUnavailable, ManifestUnavailableReason,
        };
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        let generation = store.graph_generation();
        let state = AppState::new(store, None, db_path);
        let runtime = Arc::new(ManifestRecoveryRuntime::default());
        runtime.publish(
            "blocked",
            0,
            None,
            Some(ManifestUnavailable::new(
                ManifestUnavailableReason::SourceUnavailable,
                generation,
                "repo:default:fixture: go.mod: unsupported Go module directive this",
            )),
        );
        assert!(state.manifest_recovery.set(runtime).is_ok());
        let mut stale = ManifestUnavailable::new(
            ManifestUnavailableReason::StaleGeneration,
            generation + 2,
            "repo_manifest stale artifact generation 4, expected 6; re-index with `nestweaver index --repo <path> --force`",
        );
        stale.actual_generation = Some(generation);
        let response = manifest_unavailable(&state, stale);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["reason"], "source_unavailable");
        let message = payload["message"].as_str().unwrap_or_default();
        assert!(
            !message.to_ascii_lowercase().contains("force"),
            "blocked source recovery must not prescribe --force: {payload}"
        );
        assert!(
            message.contains("go.mod"),
            "operator-relevant source failure must remain visible: {payload}"
        );
    }

    #[tokio::test]
    async fn ready_recovery_does_not_prescribe_force_for_stale_generation() {
        use nestweaver_engine::manifest::{
            ManifestRecoveryRuntime, ManifestUnavailable, ManifestUnavailableReason,
        };
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        let generation = store.graph_generation();
        let state = AppState::new(store, None, db_path);
        let runtime = Arc::new(ManifestRecoveryRuntime::default());
        runtime.publish("ready", 0, None, None);
        assert!(state.manifest_recovery.set(runtime).is_ok());
        let mut stale = ManifestUnavailable::new(
            ManifestUnavailableReason::StaleGeneration,
            generation + 1,
            "repo_manifest stale artifact generation 16, expected 17; re-index with `nestweaver index --repo <path> --force`",
        );
        stale.actual_generation = Some(generation);
        let response = manifest_unavailable(&state, stale);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["reason"], "stale_generation");
        let message = payload["message"].as_str().unwrap_or_default();
        assert!(
            !message.to_ascii_lowercase().contains("force"),
            "watching recovery must not prescribe --force for a one-generation sidecar lag: {payload}"
        );
        assert!(
            message.contains("stale artifact generation"),
            "the operator still needs the stale generation numbers: {payload}"
        );
    }

    #[tokio::test]
    async fn current_artifact_integrity_failure_overrides_prior_source_failure() {
        use nestweaver_engine::manifest::{
            ManifestRecoveryRuntime, ManifestUnavailable, ManifestUnavailableReason,
        };
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        let generation = store.graph_generation();
        let state = AppState::new(store, None, db_path);
        let runtime = Arc::new(ManifestRecoveryRuntime::default());
        runtime.publish(
            "blocked",
            0,
            None,
            Some(ManifestUnavailable::new(
                ManifestUnavailableReason::SourceUnavailable,
                generation,
                "earlier source failure",
            )),
        );
        assert!(state.manifest_recovery.set(runtime).is_ok());
        for (reason, wire_reason) in [
            (ManifestUnavailableReason::Corrupt, "corrupt"),
            (
                ManifestUnavailableReason::ForeignIdentity,
                "foreign_identity",
            ),
            (ManifestUnavailableReason::Incompatible, "incompatible"),
        ] {
            let response = manifest_unavailable(
                &state,
                ManifestUnavailable::new(reason, generation, "current artifact failure"),
            );
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(payload["reason"], wire_reason);
            assert_eq!(payload["message"], "current artifact failure");
        }
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
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let suggestions: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(suggestions["reason"], "missing");
        assert!(!canonical_path.exists());
        assert!(legacy_path.exists());
    }
}
