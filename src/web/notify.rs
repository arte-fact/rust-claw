use std::sync::Arc;

use crate::channels::web::CHANNEL_TYPE;
use crate::db::{CentralDb, messaging_groups, web_messages};
use crate::protocol::ids::MessagingGroupId;
use crate::runs::supervisor::RunNotifier;

use super::render::message_event_payload;
use super::sse::Hub;

/// The web layer's [`RunNotifier`]: turns run-lifecycle signals into presentation
/// output — ephemeral `run` SSE for the activity indicator, and a persisted
/// `web_messages` error card for failures (M14). Never touches the session ledger.
pub struct WebNotifier {
    central: Arc<CentralDb>,
    hub: Hub,
}

impl WebNotifier {
    #[must_use]
    pub fn new(central: Arc<CentralDb>, hub: Hub) -> Self {
        Self { central, hub }
    }

    /// The web chat id for a messaging group, or `None` if it isn't a web chat.
    fn web_platform_id(&self, messaging_group_id: &MessagingGroupId) -> Option<String> {
        self.central
            .with(|conn| messaging_groups::get(conn, messaging_group_id))
            .ok()
            .flatten()
            .filter(|group| group.channel_type == CHANNEL_TYPE)
            .map(|group| group.platform_id)
    }
}

impl RunNotifier for WebNotifier {
    fn run_state(&self, messaging_group_id: &MessagingGroupId, busy: bool, detail: Option<&str>) {
        let Some(platform_id) = self.web_platform_id(messaging_group_id) else {
            return;
        };
        let payload = serde_json::json!({
            "chat": platform_id,
            "state": if busy { "working" } else { "idle" },
            "detail": detail,
        })
        .to_string();
        self.hub.publish("run", payload);
    }

    fn run_failed(&self, messaging_group_id: &MessagingGroupId, detail: &str) {
        let card = self
            .central
            .with(|conn| web_messages::append_error(conn, messaging_group_id, detail));
        // `None` => collapsed as a duplicate of the previous error; nothing to show.
        let Ok(Some(card)) = card else {
            return;
        };
        if let Some(platform_id) = self.web_platform_id(messaging_group_id) {
            self.hub
                .publish("message", message_event_payload(&platform_id, &card));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_chat() -> (Arc<CentralDb>, MessagingGroupId) {
        let central = Arc::new(CentralDb::open_in_memory().expect("db"));
        let group = central
            .with(|conn| messaging_groups::create(conn, CHANNEL_TYPE, "chat-1", None, false))
            .expect("group");
        (central, group.id)
    }

    #[tokio::test]
    async fn run_state_publishes_a_run_event_for_a_web_chat() {
        let (central, mg) = web_chat();
        let hub = Hub::new();
        let mut rx = hub.subscribe();
        WebNotifier::new(central, hub).run_state(&mg, true, Some("thinking…"));

        let event = rx.recv().await.expect("event");
        assert_eq!(event.kind, "run");
        assert!(
            event.data.contains("\"state\":\"working\""),
            "{}",
            event.data
        );
        assert!(event.data.contains("chat-1"));
        assert!(event.data.contains("thinking"));
    }

    #[tokio::test]
    async fn run_failed_records_an_error_card_and_streams_it() {
        let (central, mg) = web_chat();
        let hub = Hub::new();
        let mut rx = hub.subscribe();
        WebNotifier::new(central.clone(), hub).run_failed(&mg, "no endpoint configured");

        let cards = central
            .with(|conn| web_messages::list(conn, &mg, 10))
            .expect("list");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].kind, web_messages::MessageRowKind::Error);
        assert_eq!(cards[0].body, "no endpoint configured");

        let event = rx.recv().await.expect("event");
        assert_eq!(event.kind, "message");
    }
}
