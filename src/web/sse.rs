use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use super::WebState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub kind: &'static str,
    pub data: String,
}

/// Fan-out point for live UI updates; lagging browsers just miss events and
/// recover state on the next page load (the ledger is the source of truth).
#[derive(Clone)]
pub struct Hub {
    sender: broadcast::Sender<SseEvent>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self { sender }
    }

    pub fn publish(&self, kind: &'static str, data: impl Into<String>) {
        let event = SseEvent {
            kind,
            data: data.into(),
        };
        if self.sender.send(event).is_err() {
            tracing::debug!("sse event dropped: no connected clients");
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SseEvent> {
        self.sender.subscribe()
    }
}

pub async fn events(
    State(state): State<WebState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = BroadcastStream::new(state.hub.subscribe()).filter_map(|item| {
        item.ok()
            .map(|event| Ok(Event::default().event(event.kind).data(event.data)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn published_events_reach_subscribers() {
        let hub = Hub::new();
        let mut receiver = hub.subscribe();
        hub.publish("message", "{\"chat\":\"c1\"}");
        let event = receiver.recv().await.expect("event");
        assert_eq!(event.kind, "message");
        assert_eq!(event.data, "{\"chat\":\"c1\"}");
    }

    #[tokio::test]
    async fn publishing_without_subscribers_does_not_panic() {
        Hub::new().publish("queue", "[]");
    }
}
