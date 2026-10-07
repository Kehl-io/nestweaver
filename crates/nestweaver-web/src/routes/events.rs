use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::state::AppState;

pub async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures::stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    // Subscribe before sampling: a publication during the snapshot is queued.
    let initial = tokio_stream::once(Ok(Event::default().event("graph:generation").data(
        serde_json::json!({
            "graph_generation": state.store.graph_generation().to_string(),
            "pagerank_generation": state.store.pagerank_generation().to_string(),
        })
        .to_string(),
    )));
    let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
        Ok(event) => {
            let mut payload = event.payload;
            if (event.event_type == "graph:updated" || event.event_type == "pagerank:recomputed")
                && let Some(object) = payload.as_object_mut()
            {
                object.insert(
                    "graph_generation".into(),
                    state.store.graph_generation().to_string().into(),
                );
                object.insert(
                    "pagerank_generation".into(),
                    state.store.pagerank_generation().to_string().into(),
                );
            }
            Some(Ok(Event::default()
                .event(event.event_type)
                .data(payload.to_string())))
        }
        Err(_) => Some(Ok(Event::default().event("full_refresh").data("{}"))),
    });
    Sse::new(initial.chain(stream)).keep_alive(KeepAlive::default())
}
