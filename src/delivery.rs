use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::channels::{
    AGENT_CHANNEL_TYPE, AdapterRegistry, Address, OutboundDelivery, OutboundFile,
};
use crate::db::{CentralDb, DbError, agent_groups, approvals, questions, sessions};
use crate::protocol::content::{ChatContent, Operation, OutboundContent, Routing};
use crate::protocol::ids::{AgentGroupId, MessageOutId, SessionId};
use crate::runs::queue::RunQueue;
use crate::session::{
    NewInboundMessage, OutboundMessage, SessionDb, SessionStore, SessionStoreError,
};

const MAX_DELIVERY_ATTEMPTS: u32 = 3;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub struct Delivery {
    central: Arc<CentralDb>,
    store: Arc<SessionStore>,
    registry: Arc<AdapterRegistry>,
    /// Enqueues a target agent's session when an agent-to-agent message lands.
    queue: Arc<RunQueue>,
    /// In-memory: restarts reset the counters, giving failed messages fresh chances.
    attempts: Mutex<HashMap<MessageOutId, u32>>,
}

impl Delivery {
    #[must_use]
    pub fn new(
        central: Arc<CentralDb>,
        store: Arc<SessionStore>,
        registry: Arc<AdapterRegistry>,
        queue: Arc<RunQueue>,
    ) -> Self {
        Self {
            central,
            store,
            registry,
            queue,
            attempts: Mutex::new(HashMap::new()),
        }
    }

    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if let Err(error) = self.drain_all().await {
                        tracing::error!(%error, "delivery sweep failed");
                    }
                }
            }
        }
    }

    pub async fn drain_all(&self) -> Result<(), DeliveryError> {
        let central = self.central.clone();
        let active = blocking(move || central.with(sessions::list_active)).await?;
        for session in active {
            if let Err(error) = self.drain_session(&session).await {
                tracing::error!(session = %session.id, %error, "session drain failed");
            }
        }
        Ok(())
    }

    pub async fn drain_session(&self, session: &sessions::Session) -> Result<(), DeliveryError> {
        let db = {
            let store = self.store.clone();
            let agent_group = session.agent_group_id.clone();
            let id = session.id.clone();
            Arc::new(blocking(move || store.open(&agent_group, &id)).await?)
        };
        let due = {
            let db = db.clone();
            blocking(move || {
                let now = db.now_timestamp()?;
                db.due_outbound(&now)
            })
            .await?
        };
        for message in due {
            self.deliver_one(session, &db, &message).await?;
        }
        Ok(())
    }

    async fn deliver_one(
        &self,
        session: &sessions::Session,
        db: &Arc<SessionDb>,
        message: &OutboundMessage,
    ) -> Result<(), DeliveryError> {
        match self.try_deliver(session, db, message).await {
            Ok(platform_message_id) => {
                let db = db.clone();
                let id = message.id.clone();
                let outbox = db.outbox_dir(message.id.as_str());
                blocking(move || {
                    db.mark_delivered(&id, platform_message_id.as_deref())?;
                    if outbox.is_dir() {
                        std::fs::remove_dir_all(&outbox).map_err(SessionStoreError::from)?;
                    }
                    Ok::<_, SessionStoreError>(())
                })
                .await?;
                self.attempts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&message.id);
            }
            Err(reason) => {
                let attempts = {
                    let mut attempts = self
                        .attempts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let count = attempts.entry(message.id.clone()).or_insert(0);
                    *count += 1;
                    *count
                };
                tracing::warn!(message = %message.id, attempts, %reason, "delivery attempt failed");
                if attempts >= MAX_DELIVERY_ATTEMPTS {
                    let db = db.clone();
                    let id = message.id.clone();
                    blocking(move || db.mark_delivery_failed(&id)).await?;
                    self.attempts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&message.id);
                }
            }
        }
        Ok(())
    }

    /// One delivery attempt; every failure mode is a string reason for the retry counter.
    async fn try_deliver(
        &self,
        session: &sessions::Session,
        db: &Arc<SessionDb>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, String> {
        let default_routing = {
            let db = db.clone();
            blocking(move || db.routing())
                .await
                .map_err(|err| err.to_string())?
        };
        let (channel_type, address) = resolve_address(&message.routing, default_routing.as_ref())
            .ok_or_else(|| {
            "no destination: message and session routing are both empty".to_owned()
        })?;
        let routing = Routing {
            channel_type: Some(channel_type.clone()),
            platform_id: Some(address.platform_id.clone()),
            thread_id: address.thread_id.clone(),
        };

        let content = OutboundContent::parse(&message.content).map_err(|err| err.to_string())?;

        // Agent-to-agent: not an external channel — inject into the target agent's
        // session instead of looking up an adapter (which would never exist).
        if channel_type == AGENT_CHANNEL_TYPE {
            self.deliver_to_agent(session, &address.platform_id, &content)
                .await?;
            return Ok(None);
        }

        match &content.operation {
            Some(Operation::AskQuestion {
                question_id,
                title,
                options,
                ..
            }) => {
                self.register_question(
                    &session.id,
                    &message.id,
                    &routing,
                    question_id,
                    title,
                    options,
                )
                .await?;
            }
            Some(Operation::Approval {
                approval_id,
                command,
                args,
                summary,
            }) => {
                self.register_approval(session, &routing, approval_id, command, args, summary)
                    .await?;
            }
            _ => {}
        }

        let files = collect_outbox_files(db, message);
        let adapter = self
            .registry
            .get(&channel_type)
            .map_err(|err| err.to_string())?;
        adapter
            .deliver(
                &address,
                &OutboundDelivery {
                    kind: message.kind.clone(),
                    content,
                    files,
                },
            )
            .await
            .map_err(|err| err.to_string())
    }

    /// Records an open question in the channel-agnostic registry so an answer
    /// (from any surface) can be validated and routed back to this session. Idempotent
    /// across delivery retries (the `question_id` primary key dedupes).
    async fn register_question(
        &self,
        session_id: &SessionId,
        message_out_id: &MessageOutId,
        routing: &Routing,
        question_id: &str,
        title: &str,
        options: &[String],
    ) -> Result<(), String> {
        let central = self.central.clone();
        let routing = routing.clone();
        let session_id = session_id.clone();
        let message_out_id = message_out_id.clone();
        let question_id = question_id.to_owned();
        let title = title.to_owned();
        let options = options.to_vec();
        blocking(move || {
            central.with(|conn| {
                questions::insert(
                    conn,
                    &question_id,
                    &session_id,
                    &message_out_id,
                    &routing,
                    &title,
                    &options,
                )
            })
        })
        .await
        .map_err(|err| err.to_string())
    }

    /// Records a held command so the operator's allow can run it and report back
    /// to this session. Idempotent across retries (the `approval_id` PK dedupes).
    async fn register_approval(
        &self,
        session: &sessions::Session,
        routing: &Routing,
        approval_id: &str,
        command: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        summary: &str,
    ) -> Result<(), String> {
        let central = self.central.clone();
        let routing = routing.clone();
        let session_id = session.id.clone();
        let agent_group_id = session.agent_group_id.clone();
        let approval_id = approval_id.to_owned();
        let command = command.to_owned();
        let args = args.clone();
        let summary = summary.to_owned();
        blocking(move || {
            central.with(|conn| {
                approvals::insert(
                    conn,
                    &approvals::NewApproval {
                        approval_id: &approval_id,
                        session_id: &session_id,
                        agent_group_id: &agent_group_id,
                        command: &command,
                        args: &args,
                        routing: &routing,
                        summary: &summary,
                    },
                )
            })
        })
        .await
        .map_err(|err| err.to_string())
    }

    /// Delivers an agent-to-agent message by writing it as an inbound chat into the
    /// target group's session (creating one if needed) and enqueuing a run — the
    /// same machinery the router uses, so "everything is a message" still holds. The
    /// inbound's routing points back at the sender so the target can reply (§8.6).
    async fn deliver_to_agent(
        &self,
        source: &sessions::Session,
        target_group: &str,
        content: &OutboundContent,
    ) -> Result<(), String> {
        let central = self.central.clone();
        let store = self.store.clone();
        let target_group = AgentGroupId::new(target_group.to_owned());
        let source_group = source.agent_group_id.clone();
        let text = content.text.clone().unwrap_or_default();

        let target_label = target_group.clone();
        let session_id = blocking(move || -> Result<Option<SessionId>, DeliveryError> {
            let Some(sender) = central
                .with(|conn| Ok(agent_groups::get(conn, &source_group)?.map(|group| group.name)))?
            else {
                return Ok(None);
            };
            let session = central.with(|conn| {
                if agent_groups::get(conn, &target_group)?.is_none() {
                    return Ok(None);
                }
                match sessions::find_active(conn, &target_group, None, None)? {
                    Some(session) => Ok(Some(session)),
                    None => sessions::create(conn, &target_group, None, None).map(Some),
                }
            })?;
            let Some(session) = session else {
                return Ok(None);
            };

            let routing = Routing {
                channel_type: Some(AGENT_CHANNEL_TYPE.to_owned()),
                platform_id: Some(source_group.as_str().to_owned()),
                thread_id: None,
            };
            let chat = ChatContent {
                sender,
                sender_id: None,
                text,
                attachments: Vec::new(),
                is_from_me: false,
                quoted: None,
            };
            let body = serde_json::to_string(&chat).map_err(|err| {
                DbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(err.into()))
            })?;
            let db = store.init(&session.agent_group_id, &session.id)?;
            db.write_routing(&routing)?;
            db.write_message(&NewInboundMessage::chat(body, routing))?;
            Ok(Some(session.id))
        })
        .await
        .map_err(|err| err.to_string())?;

        match session_id {
            Some(session_id) => {
                self.queue.enqueue(session_id);
                Ok(())
            }
            None => Err(format!("unknown target agent group {target_label:?}")),
        }
    }
}

async fn blocking<T, E>(
    op: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, DeliveryError>
where
    T: Send + 'static,
    E: Into<DeliveryError> + Send + 'static,
{
    crate::blocking::run(op).await
}

/// Message routing wins field-by-field; the session's default routing fills the gaps.
fn resolve_address(
    message_routing: &Routing,
    default_routing: Option<&Routing>,
) -> Option<(String, Address)> {
    let pick = |field: fn(&Routing) -> Option<&String>| {
        field(message_routing)
            .or_else(|| default_routing.and_then(field))
            .cloned()
    };
    let channel_type = pick(|routing| routing.channel_type.as_ref())?;
    let platform_id = pick(|routing| routing.platform_id.as_ref())?;
    let thread_id = pick(|routing| routing.thread_id.as_ref());
    Some((
        channel_type,
        Address {
            platform_id,
            thread_id,
        },
    ))
}

fn collect_outbox_files(db: &SessionDb, message: &OutboundMessage) -> Vec<OutboundFile> {
    let dir = db.outbox_dir(message.id.as_str());
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<OutboundFile> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .map(|entry| OutboundFile {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path(),
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{ChannelAdapter, ChannelError};
    use crate::db::{agent_groups, messaging_groups};
    use crate::session::NewOutboundMessage;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::mpsc;

    #[derive(Default)]
    struct RecordingAdapter {
        fail_first: u32,
        calls: AtomicU32,
        seen: Mutex<Vec<(Address, OutboundDelivery)>>,
    }

    #[async_trait]
    impl ChannelAdapter for RecordingAdapter {
        fn channel_type(&self) -> &'static str {
            "web"
        }
        fn supports_threads(&self) -> bool {
            false
        }
        async fn run(
            &self,
            _inbound: mpsc::Sender<crate::router::InboundEvent>,
            _cancel: CancellationToken,
        ) -> Result<(), ChannelError> {
            Ok(())
        }
        async fn deliver(
            &self,
            address: &Address,
            delivery: &OutboundDelivery,
        ) -> Result<Option<String>, ChannelError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.fail_first {
                return Err(ChannelError::Delivery("simulated outage".to_owned()));
            }
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((address.clone(), delivery.clone()));
            Ok(Some(format!("pm-{call}")))
        }
    }

    struct Fixture {
        delivery: Delivery,
        adapter: Arc<RecordingAdapter>,
        store: Arc<SessionStore>,
        central: Arc<CentralDb>,
        queue: Arc<crate::runs::queue::RunQueue>,
        session: sessions::Session,
        _tmp: tempfile::TempDir,
    }

    fn fixture(fail_first: u32) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let session = central
            .with(|conn| {
                let group = agent_groups::create(conn, "Andy", "andy")?;
                let mg = messaging_groups::create(conn, "web", "chat-1", None, false)?;
                sessions::create(conn, &group.id, Some(&mg.id), None)
            })
            .expect("fixture rows");
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        let adapter = Arc::new(RecordingAdapter {
            fail_first,
            ..RecordingAdapter::default()
        });
        let registry = Arc::new(AdapterRegistry::new(vec![adapter.clone()]));
        let queue = Arc::new(crate::runs::queue::RunQueue::new());
        Fixture {
            delivery: Delivery::new(central.clone(), store.clone(), registry, queue.clone()),
            adapter,
            store,
            central,
            queue,
            session,
            _tmp: tmp,
        }
    }

    fn session_db(fix: &Fixture) -> SessionDb {
        fix.store
            .open(&fix.session.agent_group_id, &fix.session.id)
            .expect("open")
    }

    fn write_reply(fix: &Fixture, text: &str, routing: Routing) -> OutboundMessage {
        let db = session_db(fix);
        let content = serde_json::to_string(&OutboundContent::from_text(text)).expect("json");
        db.write_outbound(&NewOutboundMessage::chat(content, routing))
            .expect("write")
    }

    fn web_routing() -> Routing {
        Routing {
            channel_type: Some("web".to_owned()),
            platform_id: Some("chat-1".to_owned()),
            thread_id: None,
        }
    }

    #[tokio::test]
    async fn due_message_is_delivered_once_and_ledgered() {
        let fix = fixture(0);
        write_reply(&fix, "hello", web_routing());

        fix.delivery.drain_all().await.expect("drain");
        fix.delivery.drain_all().await.expect("second drain");

        let seen = fix.adapter.seen.lock().expect("lock");
        assert_eq!(seen.len(), 1, "delivered exactly once");
        assert_eq!(seen[0].0.platform_id, "chat-1");
        assert_eq!(seen[0].1.content.text.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn delivering_an_ask_question_registers_a_pending_question() {
        let fix = fixture(0);
        let db = session_db(&fix);
        let content = OutboundContent {
            text: Some("Deploy now? (ship / wait)".to_owned()),
            files: Vec::new(),
            operation: Some(Operation::AskQuestion {
                question_id: "q-1".to_owned(),
                title: "Deploy now?".to_owned(),
                question: "Deploy now?".to_owned(),
                options: vec!["ship".to_owned(), "wait".to_owned()],
            }),
            extra: serde_json::Map::new(),
        };
        let body = serde_json::to_string(&content).expect("json");
        db.write_outbound(&NewOutboundMessage::chat(body, web_routing()))
            .expect("write");

        fix.delivery.drain_all().await.expect("drain");

        let registered = fix
            .central
            .with(|conn| questions::get(conn, "q-1"))
            .expect("query")
            .expect("registered");
        assert_eq!(registered.session_id, fix.session.id);
        assert_eq!(registered.routing.platform_id.as_deref(), Some("chat-1"));
        assert_eq!(
            registered.options,
            vec!["ship".to_owned(), "wait".to_owned()]
        );
    }

    #[tokio::test]
    async fn agent_channel_message_routes_into_the_target_session_not_an_adapter() {
        let fix = fixture(0);
        let target = fix
            .central
            .with(|conn| Ok(agent_groups::create(conn, "Coder", "coder")?.id))
            .expect("target group");

        // The source (Andy) sends to the Coder group via the "agent" channel.
        write_reply(
            &fix,
            "please review the PR",
            Routing {
                channel_type: Some(AGENT_CHANNEL_TYPE.to_owned()),
                platform_id: Some(target.as_str().to_owned()),
                thread_id: None,
            },
        );

        fix.delivery.drain_all().await.expect("drain");

        // It must NOT have gone to a channel adapter.
        assert!(
            fix.adapter.seen.lock().expect("lock").is_empty(),
            "agent message must not reach the channel adapter"
        );

        // The Coder group now has an active session carrying the inbound chat.
        let target_session = fix
            .central
            .with(|conn| sessions::find_active(conn, &target, None, None))
            .expect("query")
            .expect("target session created");
        let target_db = fix
            .store
            .open(&target_session.agent_group_id, &target_session.id)
            .expect("open");
        let transcript = target_db.transcript(10).expect("transcript");
        let inbound = transcript
            .iter()
            .find(|entry| entry.inbound)
            .expect("an inbound landed");
        assert!(inbound.content.contains("please review the PR"));
        assert!(inbound.content.contains("Andy"), "sender labelled");

        // And the target session is enqueued for a run.
        assert!(
            fix.queue.snapshot().contains(&target_session.id),
            "target session enqueued"
        );

        // The source's outbound is delivered (no retry / no longer due).
        let src_db = session_db(&fix);
        let now = src_db.now_timestamp().expect("now");
        assert!(
            src_db.due_outbound(&now).expect("due").is_empty(),
            "agent message marked delivered"
        );
    }

    #[tokio::test]
    async fn session_routing_fills_in_missing_message_routing() {
        let fix = fixture(0);
        let db = session_db(&fix);
        db.write_routing(&web_routing()).expect("routing");
        write_reply(&fix, "fallback", Routing::default());

        fix.delivery.drain_all().await.expect("drain");

        let seen = fix.adapter.seen.lock().expect("lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0.platform_id, "chat-1");
    }

    #[tokio::test]
    async fn outbox_files_are_attached_and_cleaned_up() {
        let fix = fixture(0);
        let message = write_reply(&fix, "with file", web_routing());
        let db = session_db(&fix);
        let outbox = db.outbox_dir(message.id.as_str());
        std::fs::create_dir_all(&outbox).expect("mkdir");
        std::fs::write(outbox.join("chart.png"), b"png-bytes").expect("file");

        fix.delivery.drain_all().await.expect("drain");

        let seen = fix.adapter.seen.lock().expect("lock");
        assert_eq!(seen[0].1.files.len(), 1);
        assert_eq!(seen[0].1.files[0].name, "chart.png");
        assert!(
            !outbox.exists(),
            "outbox dir must be removed after delivery"
        );
    }

    #[tokio::test]
    async fn transient_failures_retry_then_succeed() {
        let fix = fixture(2);
        write_reply(&fix, "eventually", web_routing());

        for _ in 0..3 {
            fix.delivery.drain_all().await.expect("drain");
        }

        let seen = fix.adapter.seen.lock().expect("lock");
        assert_eq!(seen.len(), 1, "third attempt must succeed");
    }

    #[tokio::test]
    async fn persistent_failures_mark_the_message_failed_at_three_attempts() {
        let fix = fixture(u32::MAX);
        write_reply(&fix, "doomed", web_routing());

        for _ in 0..5 {
            fix.delivery.drain_all().await.expect("drain");
        }

        assert_eq!(fix.adapter.calls.load(Ordering::SeqCst), 3);
        let db = session_db(&fix);
        let now = db.now_timestamp().expect("now");
        assert!(db.due_outbound(&now).expect("due").is_empty());
    }

    #[test]
    fn resolve_address_prefers_message_fields() {
        let message = Routing {
            channel_type: Some("web".to_owned()),
            platform_id: Some("explicit".to_owned()),
            thread_id: None,
        };
        let default = Routing {
            channel_type: Some("telegram".to_owned()),
            platform_id: Some("default".to_owned()),
            thread_id: Some("t1".to_owned()),
        };
        let (channel, address) = resolve_address(&message, Some(&default)).expect("resolvable");
        assert_eq!(channel, "web");
        assert_eq!(address.platform_id, "explicit");
        assert_eq!(address.thread_id.as_deref(), Some("t1"));
        assert_eq!(resolve_address(&Routing::default(), None), None);
    }
}
