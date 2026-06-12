pub mod web;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::protocol::content::OutboundContent;
use crate::router::InboundEvent;

/// Internal pseudo-channel for agent-to-agent messages (`send_to_agent`). These
/// never reach a `ChannelAdapter`; delivery injects them into the target agent's
/// session instead (§8.6). `platform_id` = the target agent group.
pub const AGENT_CHANNEL_TYPE: &str = "agent";

/// The return leg of a delegation: a worker's reply routed back to the exact
/// originating session so the concierge can relay it to the user (M15).
/// `platform_id` = the originating session id. A distinct channel (not a
/// `thread_id`) so a forward's routing can't accidentally inherit it.
pub const AGENT_RETURN_CHANNEL_TYPE: &str = "agent-return";

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("operation not supported by this channel")]
    Unsupported,
    #[error("no adapter registered for channel type {0:?}")]
    UnknownChannel(String),
    #[error("delivery failed: {0}")]
    Delivery(String),
}

/// Where on a platform a message goes: conversation id plus optional thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub platform_id: String,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundFile {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboundDelivery {
    pub kind: String,
    pub content: OutboundContent,
    pub files: Vec<OutboundFile>,
}

#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn channel_type(&self) -> &'static str;
    fn supports_threads(&self) -> bool;

    /// Long-running connection loop; push platform events into `inbound`,
    /// return when `cancel` fires. Reconnect policy is the adapter's business.
    async fn run(
        &self,
        inbound: mpsc::Sender<InboundEvent>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError>;

    /// Returns the platform message id when the platform provides one.
    async fn deliver(
        &self,
        address: &Address,
        delivery: &OutboundDelivery,
    ) -> Result<Option<String>, ChannelError>;

    async fn set_typing(&self, _address: &Address) -> Result<(), ChannelError> {
        Ok(())
    }

    async fn edit(
        &self,
        _address: &Address,
        _platform_message_id: &str,
        _text: &str,
    ) -> Result<(), ChannelError> {
        Err(ChannelError::Unsupported)
    }

    async fn react(
        &self,
        _address: &Address,
        _platform_message_id: &str,
        _emoji: &str,
    ) -> Result<(), ChannelError> {
        Err(ChannelError::Unsupported)
    }
}

#[derive(Default)]
pub struct AdapterRegistry {
    adapters: HashMap<&'static str, Arc<dyn ChannelAdapter>>,
}

impl AdapterRegistry {
    #[must_use]
    pub fn new(adapters: Vec<Arc<dyn ChannelAdapter>>) -> Self {
        Self {
            adapters: adapters
                .into_iter()
                .map(|adapter| (adapter.channel_type(), adapter))
                .collect(),
        }
    }

    pub fn get(&self, channel_type: &str) -> Result<&Arc<dyn ChannelAdapter>, ChannelError> {
        self.adapters
            .get(channel_type)
            .ok_or_else(|| ChannelError::UnknownChannel(channel_type.to_owned()))
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn ChannelAdapter>> {
        self.adapters.values()
    }
}
