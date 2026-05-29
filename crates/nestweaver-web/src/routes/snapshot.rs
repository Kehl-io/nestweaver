use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

use nestweaver_engine::export_in_memory_graph;

use crate::state::AppState;

/// `GET /api/v1/snapshot.msgpack`
///
/// Exports the full code graph as a MessagePack-encoded [`InMemoryGraph`].
/// The `X-Graph-Generation` response header carries the generation counter so
/// callers can detect staleness without re-fetching.
pub async fn snapshot_msgpack(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let graph = match export_in_memory_graph(&state.store) {
        Ok(g) => g,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Export error: {e}"),
            )
                .into_response();
        }
    };

    let generation = graph.generation;
    let bytes = match rmp_serde::to_vec(&graph) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Serialize error: {e}"),
            )
                .into_response();
        }
    };

    let gen_str = generation.to_string();
    let gen_header_value = match header::HeaderValue::from_str(&gen_str) {
        Ok(v) => v,
        Err(_) => header::HeaderValue::from_static("0"),
    };

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/msgpack"),
            ),
            (
                header::HeaderName::from_static("x-graph-generation"),
                gen_header_value,
            ),
        ],
        bytes,
    )
        .into_response()
}
