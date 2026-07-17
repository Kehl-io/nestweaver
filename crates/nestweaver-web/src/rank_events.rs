use std::sync::Arc;

use crate::error::ApiError;
use crate::state::{AppState, GraphEvent};

/// Run `f` on a blocking thread; if it triggered a PageRank (re)compute
/// (observable as a generation change), notify SSE subscribers so the UI
/// refreshes instead of showing stale/absent ranks. Send errors are ignored
/// (no subscribers is normal).
///
/// PageRank is computed lazily on the first ranking query after an index and
/// can take seconds-to-minutes on a large graph. Running that inline in a
/// plain `async fn` pins a tokio worker thread for the whole compute, so
/// ranking handlers hand the store work to `spawn_blocking` via this helper.
pub async fn with_rank_event<T, F>(state: &Arc<AppState>, f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    let before = state.store.pagerank_generation();
    let state2 = state.clone();
    let out = tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| ApiError::internal(format!("blocking task failed: {err}")))??;
    if state.store.pagerank_generation() != before {
        let _ = state2.event_tx.send(GraphEvent {
            event_type: "pagerank:recomputed".to_string(),
            payload: serde_json::json!({}),
        });
    }
    Ok(out)
}
