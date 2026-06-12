pub mod sms;
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
    #[error("adapter init failed: {0}")]
    Init(String),
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

/// One adapter per enabled connector row (§10). Exhaustive over
/// `ConnectorConfig`: a new kind does not compile until it is built here.
pub fn build_adapters(
    central: &Arc<crate::db::CentralDb>,
    connectors: &[crate::db::connectors::Connector],
) -> Result<Vec<Arc<dyn ChannelAdapter>>, ChannelError> {
    connectors
        .iter()
        .filter(|connector| connector.enabled)
        .map(|connector| match &connector.config {
            crate::protocol::entities::ConnectorConfig::Sms(config) => {
                sms::SmsChannel::new(central.clone(), config)
                    .map(|channel| Arc::new(channel) as Arc<dyn ChannelAdapter>)
            }
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, agent_groups, connectors};
    use crate::protocol::entities::{ConnectorConfig, SmsConnectorConfig};

    #[test]
    fn build_adapters_skips_disabled_connectors() {
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let rows = central
            .with(|conn| {
                let group = agent_groups::create(conn, "Andy", "andy")?;
                let config = ConnectorConfig::Sms(SmsConnectorConfig {
                    base_url: "http://sim:8080".to_owned(),
                    token: "sms_secret".to_owned(),
                    webhook_secret: None,
                });
                let mut connector = connectors::create(conn, &config, &group.id, None)?;
                connector.enabled = false;
                connectors::update(conn, &connector)?;
                connectors::list(conn)
            })
            .expect("rows");

        let disabled = build_adapters(&central, &rows).expect("build");
        assert!(disabled.is_empty(), "disabled rows must not build adapters");

        let central2 = central.clone();
        central2
            .with(|conn| {
                let mut connector = rows[0].clone();
                connector.enabled = true;
                connectors::update(conn, &connector)
            })
            .expect("enable");
        let rows = central.with(connectors::list).expect("rows");
        let enabled = build_adapters(&central, &rows).expect("build");
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].channel_type(), "sms");
        assert!(!enabled[0].supports_threads());
    }
}
