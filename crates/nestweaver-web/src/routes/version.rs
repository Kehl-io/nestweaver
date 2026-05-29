use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct VersionInfo {
    pub graph_generation: u64,
    pub pagerank_generation: u64,
}

pub async fn version(State(state): State<Arc<AppState>>) -> Json<VersionInfo> {
    Json(VersionInfo {
        graph_generation: state.store.graph_generation(),
        pagerank_generation: state.store.pagerank_generation(),
    })
}
