use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use nestweaver_store::GraphStore;
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct FlowParams {
    pub max_depth: Option<usize>,
}

pub async fn flow(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
    Query(params): Query<FlowParams>,
) -> Result<Response, ApiError> {
    let max_depth = params.max_depth.unwrap_or(10);

    // Verify root symbol exists
    let root = state
        .store
        .lookup_symbol(&uid)
        .map_err(|_| ApiError::not_found(format!("symbol '{uid}' not found")))?;

    let mut visited = HashSet::new();
    visited.insert(uid.clone());

    let tree = build_flow_tree(
        &state.store,
        &root.uid,
        &root.name,
        &root.file_path,
        0,
        max_depth,
        &mut visited,
    );

    Ok(Json(tree).into_response())
}

fn build_flow_tree(
    store: &GraphStore,
    uid: &str,
    name: &str,
    file_path: &str,
    depth: usize,
    max_depth: usize,
    visited: &mut HashSet<String>,
) -> serde_json::Value {
    let mut children = Vec::new();

    if depth < max_depth
        && let Ok(callees) = store.callees_of(uid)
    {
        for callee in &callees {
            if visited.contains(&callee.uid) {
                continue;
            }
            visited.insert(callee.uid.clone());
            let child = build_flow_tree(
                store,
                &callee.uid,
                &callee.name,
                &callee.file_path,
                depth + 1,
                max_depth,
                visited,
            );
            children.push(child);
        }
    }

    json!({
        "uid": uid,
        "name": name,
        "file_path": file_path,
        "depth": depth,
        "children": children,
    })
}
