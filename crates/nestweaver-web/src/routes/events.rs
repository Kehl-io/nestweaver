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
    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(event) => Some(Ok(Event::default()
            .event(event.event_type)
            .data(event.payload.to_string()))),
        Err(_) => Some(Ok(Event::default().event("full_refresh").data("{}"))),
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
