use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db::{CentralDb, messaging_groups, web_messages};
use crate::router::InboundEvent;
use crate::web::sse::Hub;

use super::{Address, ChannelAdapter, ChannelError, OutboundDelivery};

pub const CHANNEL_TYPE: &str = "web";
pub const AGENT_SENDER: &str = "assistant";

/// The built-in channel. Inbound flows from HTTP handlers through `submit`;
/// outbound lands in the `web_messages` ledger and is pushed to browsers over SSE.
pub struct WebChannel {
    central: Arc<CentralDb>,
    hub: Hub,
    inbound: Mutex<Option<mpsc::Sender<InboundEvent>>>,
}

impl WebChannel {
    #[must_use]
    pub fn new(central: Arc<CentralDb>, hub: Hub) -> Self {
        Self {
            central,
            hub,
            inbound: Mutex::new(None),
        }
    }

    /// Called by the HTTP message handler; fails before `run` has wired the router.
    pub async fn submit(&self, event: InboundEvent) -> Result<(), ChannelError> {
        let sender = self
            .inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| ChannelError::Delivery("web channel not started".to_owned()))?;
        sender
            .send(event)
            .await
            .map_err(|_| ChannelError::Delivery("router is gone".to_owned()))
    }

    fn ledger_outbound(
        &self,
        address: &Address,
        delivery: &OutboundDelivery,
    ) -> Result<web_messages::WebMessage, ChannelError> {
        let body = delivery
            .content
            .text
            .clone()
            .unwrap_or_else(|| "[no text]".to_owned());
        self.central
            .with(|conn| {
                let Some(chat) =
                    messaging_groups::get_by_platform(conn, CHANNEL_TYPE, &address.platform_id)?
                else {
                    return Ok(None);
                };
                web_messages::append(
                    conn,
                    &chat.id,
                    web_messages::Direction::Out,
                    AGENT_SENDER,
                    &body,
                    None,
                )
                .map(Some)
            })
            .map_err(|err| ChannelError::Delivery(err.to_string()))?
            .ok_or_else(|| {
                ChannelError::Delivery(format!("unknown web chat {:?}", address.platform_id))
            })
    }
}

#[async_trait]
impl ChannelAdapter for WebChannel {
    fn channel_type(&self) -> &'static str {
        CHANNEL_TYPE
    }

    fn supports_threads(&self) -> bool {
        false
    }

    async fn run(
        &self,
        inbound: mpsc::Sender<InboundEvent>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError> {
        *self
            .inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(inbound);
        cancel.cancelled().await;
        Ok(())
    }

    async fn deliver(
        &self,
        address: &Address,
        delivery: &OutboundDelivery,
    ) -> Result<Option<String>, ChannelError> {
        let message = self.ledger_outbound(address, delivery)?;
        self.hub.publish(
            "message",
            crate::web::render::message_event_payload(&address.platform_id, &message),
        );
        Ok(Some(message.id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::content::OutboundContent;

    fn fixture() -> (Arc<CentralDb>, WebChannel, Hub) {
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        central
            .with(|conn| messaging_groups::create(conn, "web", "chat-1", None, false).map(|_| ()))
            .expect("chat row");
        let hub = Hub::new();
        let channel = WebChannel::new(central.clone(), hub.clone());
        (central, channel, hub)
    }

    fn text_delivery(text: &str) -> OutboundDelivery {
        OutboundDelivery {
            kind: "chat".to_owned(),
            content: OutboundContent::from_text(text),
            files: Vec::new(),
        }
    }

    #[tokio::test]
    async fn deliver_appends_to_ledger_and_publishes_sse() {
        let (central, channel, hub) = fixture();
        let mut events = hub.subscribe();
        let address = Address {
            platform_id: "chat-1".to_owned(),
            thread_id: None,
        };

        let platform_id = channel
            .deliver(&address, &text_delivery("hi there"))
            .await
            .expect("deliver");
        assert!(platform_id.is_some());

        let event = events.recv().await.expect("sse event");
        assert_eq!(event.kind, "message");
        assert!(event.data.contains("hi there"));

        let rows = central
            .with(|conn| {
                let chat = messaging_groups::get_by_platform(conn, "web", "chat-1")?.expect("chat");
                web_messages::list(conn, &chat.id, 10)
            })
            .expect("ledger");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender, AGENT_SENDER);
        assert_eq!(rows[0].direction, web_messages::Direction::Out);
    }

    #[tokio::test]
    async fn deliver_to_unknown_chat_fails() {
        let (_central, channel, _hub) = fixture();
        let address = Address {
            platform_id: "nope".to_owned(),
            thread_id: None,
        };
        assert!(
            channel
                .deliver(&address, &text_delivery("x"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn submit_before_run_is_an_error() {
        let (_central, channel, _hub) = fixture();
        let event = InboundEvent {
            channel_type: "web".to_owned(),
            platform_id: "chat-1".to_owned(),
            thread_id: None,
            kind: crate::protocol::message::MessageKind::Chat,
            content: "{}".to_owned(),
            is_mention: false,
            is_group: false,
        };
        assert!(channel.submit(event).await.is_err());
    }
}
