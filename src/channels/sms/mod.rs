mod protocol;

pub use protocol::{CHANNEL_TYPE, SmsMessage, advance_cursor, render_sms, to_inbound_event};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::db::{CentralDb, channel_cursors};
use crate::protocol::entities::SmsConnectorConfig;
use crate::router::InboundEvent;

use super::{Address, ChannelAdapter, ChannelError, OutboundDelivery};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The sim-server SMS connector: inbound = `after_seq` cursor polling (the
/// cursor in `channel_cursors` is the single source of truth, persisted per
/// message — at-least-once across a crash, §10); outbound = `POST /api/messages`.
/// `wake()` (the webhook ping, M17.4) turns the next poll immediate.
pub struct SmsChannel {
    central: Arc<CentralDb>,
    base_url: String,
    token: String,
    webhook_secret: Option<String>,
    wake: Notify,
    http: reqwest::Client,
    poll_interval: Duration,
}

enum PollOutcome {
    Routed(i64),
    RouterGone,
}

enum PollError {
    Unauthorized,
    Transport(String),
}

#[derive(Debug, thiserror::Error)]
enum CursorStoreError {
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl SmsChannel {
    pub fn new(central: Arc<CentralDb>, config: &SmsConnectorConfig) -> Result<Self, ChannelError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| ChannelError::Init(err.to_string()))?;
        Ok(Self {
            central,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            token: config.token.clone(),
            webhook_secret: config.webhook_secret.clone(),
            wake: Notify::new(),
            http,
            poll_interval: POLL_INTERVAL,
        })
    }

    #[must_use]
    pub fn webhook_secret(&self) -> Option<&str> {
        self.webhook_secret.as_deref()
    }

    #[must_use]
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Makes the next poll immediate (the webhook wake-up ping).
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// First run only: take the upstream high-water mark without routing, so a
    /// fresh claw does not replay sim-server's retained history into the agent.
    async fn init_cursor(&self, cancel: &CancellationToken) -> Option<i64> {
        let mut failures: u32 = 0;
        loop {
            match self.load_cursor().await {
                Ok(Some(cursor)) => return Some(cursor),
                Ok(None) => match self.fetch_after(0).await {
                    Ok(messages) => {
                        let cursor = advance_cursor(0, &messages);
                        self.persist_cursor(cursor).await;
                        tracing::info!(
                            cursor,
                            skipped = messages.len(),
                            "sms cursor initialised; history not routed"
                        );
                        return Some(cursor);
                    }
                    Err(error) => log_poll_error(&error, &mut failures),
                },
                Err(reason) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!(%reason, "sms cursor load failed");
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return None,
                () = tokio::time::sleep(backoff_delay(self.poll_interval, failures)) => {}
            }
        }
    }

    async fn poll_once(
        &self,
        inbound: &mpsc::Sender<InboundEvent>,
        mut cursor: i64,
    ) -> Result<PollOutcome, PollError> {
        let messages = self.fetch_after(cursor).await?;
        for message in &messages {
            if let Some(event) = to_inbound_event(message)
                && inbound.send(event).await.is_err()
            {
                return Ok(PollOutcome::RouterGone);
            }
            let advanced = advance_cursor(cursor, std::slice::from_ref(message));
            if advanced != cursor {
                cursor = advanced;
                self.persist_cursor(cursor).await;
            }
        }
        Ok(PollOutcome::Routed(cursor))
    }

    async fn fetch_after(&self, cursor: i64) -> Result<Vec<SmsMessage>, PollError> {
        let response = self
            .http
            .get(format!("{}/api/messages?after_seq={cursor}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|err| PollError::Transport(err.to_string()))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(PollError::Unauthorized);
        }
        if !response.status().is_success() {
            return Err(PollError::Transport(format!(
                "sim-server returned {}",
                response.status()
            )));
        }
        response
            .json()
            .await
            .map_err(|err| PollError::Transport(err.to_string()))
    }

    async fn load_cursor(&self) -> Result<Option<i64>, String> {
        let central = self.central.clone();
        crate::blocking::run::<_, _, CursorStoreError>(move || {
            central.with(|conn| channel_cursors::get(conn, CHANNEL_TYPE))
        })
        .await
        .map_err(|err| err.to_string())
    }

    /// Best-effort: a failed persist widens the replay window to the next
    /// successful one instead of stopping inbound flow.
    async fn persist_cursor(&self, cursor: i64) {
        let central = self.central.clone();
        let result = crate::blocking::run::<_, _, CursorStoreError>(move || {
            central.with(|conn| channel_cursors::set(conn, CHANNEL_TYPE, cursor))
        })
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, cursor, "sms cursor persist failed");
        }
    }
}

#[async_trait]
impl ChannelAdapter for SmsChannel {
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
        let Some(mut cursor) = self.init_cursor(&cancel).await else {
            return Ok(());
        };
        let mut failures: u32 = 0;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                () = tokio::time::sleep(backoff_delay(self.poll_interval, failures)) => {}
                () = self.wake.notified() => {}
            }
            match self.poll_once(&inbound, cursor).await {
                Ok(PollOutcome::Routed(advanced)) => {
                    cursor = advanced;
                    failures = 0;
                }
                Ok(PollOutcome::RouterGone) => return Ok(()),
                Err(error) => log_poll_error(&error, &mut failures),
            }
        }
    }

    async fn deliver(
        &self,
        address: &Address,
        delivery: &OutboundDelivery,
    ) -> Result<Option<String>, ChannelError> {
        let content = render_sms(delivery);
        if content.is_empty() {
            return Ok(None);
        }
        let response = self
            .http
            .post(format!("{}/api/messages", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "phone": address.platform_id,
                "content": content,
            }))
            .send()
            .await
            .map_err(|err| ChannelError::Delivery(err.to_string()))?;
        if !response.status().is_success() {
            return Err(ChannelError::Delivery(format!(
                "sim-server returned {}",
                response.status()
            )));
        }
        Ok(None)
    }
}

/// A wrong token would never heal by retrying fast, so it logs once at ERROR
/// (the first failure) and then rides the backoff quietly.
fn log_poll_error(error: &PollError, failures: &mut u32) {
    *failures = failures.saturating_add(1);
    match error {
        PollError::Unauthorized => {
            if *failures == 1 {
                tracing::error!(
                    "sim-server rejected the token (401) — check the SMS connector's token scopes"
                );
            }
        }
        PollError::Transport(reason) => tracing::warn!(%reason, "sms poll failed"),
    }
}

fn backoff_delay(base: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return base;
    }
    base.saturating_mul(1u32 << failures.min(4))
        .min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests;
