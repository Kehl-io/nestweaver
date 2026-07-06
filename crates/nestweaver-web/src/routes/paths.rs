use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct PathsParams {
    pub max_depth: Option<usize>,
    pub limit: Option<usize>,
}

pub async fn paths_between(
    State(state): State<Arc<AppState>>,
    Path((from, to)): Path<(String, String)>,
    Query(params): Query<PathsParams>,
) -> Result<Response, ApiError> {
    // Clamp client-supplied bounds: an uncapped BFS depth/limit lets a request
    // exhaust CPU/memory on a large graph.
    let max_depth = params.max_depth.unwrap_or(5).min(25);
    let limit = params.limit.unwrap_or(10).min(100);

    // BFS: each entry is (current_uid, path_so_far, edges_so_far)
    let mut queue: VecDeque<(String, Vec<String>, Vec<serde_json::Value>)> = VecDeque::new();
    let mut found_paths: Vec<serde_json::Value> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    queue.push_back((from.clone(), vec![from.clone()], Vec::new()));
    visited.insert(from.clone());

    while let Some((current, path, edges)) = queue.pop_front() {
        if found_paths.len() >= limit {
            break;
        }

        let depth = path.len() - 1;
        if depth >= max_depth {
            continue;
        }

        let callees = match state.store.callees_of(&current) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for callee in &callees {
            let mut new_path = path.clone();
            new_path.push(callee.uid.clone());

            let mut new_edges = edges.clone();
            new_edges.push(json!({
                "type": "CALLS",
                "confidence": 1.0,
            }));

            if callee.uid == to {
                let length = new_path.len() - 1;
                found_paths.push(json!({
                    "nodes": new_path,
                    "edges": new_edges,
                    "length": length,
                }));
                if found_paths.len() >= limit {
                    break;
                }
            } else if !visited.contains(&callee.uid) {
                visited.insert(callee.uid.clone());
                queue.push_back((callee.uid.clone(), new_path, new_edges));
            }
        }
    }

    Ok(Json(serde_json::Value::Array(found_paths)).into_response())
}
