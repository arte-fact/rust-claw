use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::blocking;
use crate::db::{CentralDb, DbError, connectors, dropped, messaging_groups, sessions};
use crate::engage::{Engage, EngageInput, evaluate};
use crate::protocol::content::{InboundContent, Routing};
use crate::protocol::entities::{ConnectorKind, EngageMode, SessionMode};
use crate::protocol::ids::SessionId;
use crate::protocol::message::MessageKind;
use crate::runs::queue::RunQueue;
use crate::session::{NewInboundMessage, SessionStore, SessionStoreError};

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InboundEvent {
    pub channel_type: String,
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub kind: MessageKind,
    pub content: String,
    pub is_mention: bool,
    pub is_group: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RouteOutcome {
    Delivered { sessions: Vec<SessionId> },
    Dropped { reason: &'static str },
}

pub struct Router {
    central: Arc<CentralDb>,
    store: Arc<SessionStore>,
    queue: Arc<RunQueue>,
}

impl Router {
    #[must_use]
    pub fn new(central: Arc<CentralDb>, store: Arc<SessionStore>, queue: Arc<RunQueue>) -> Self {
        Self {
            central,
            store,
            queue,
        }
    }

    pub async fn run(
        self: Arc<Self>,
        mut inbound: mpsc::Receiver<InboundEvent>,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                event = inbound.recv() => {
                    let Some(event) = event else { break };
                    match self.route(event).await {
                        Ok(RouteOutcome::Delivered { sessions }) => {
                            tracing::debug!(count = sessions.len(), "inbound routed");
                        }
                        Ok(RouteOutcome::Dropped { reason }) => {
                            tracing::warn!(reason, "inbound dropped");
                        }
                        Err(error) => tracing::error!(%error, "routing failed"),
                    }
                }
            }
        }
    }

    /// Routes an inbound to every wired agent group: the message is always
    /// written, but only an *engaging* message (per the wiring's `engage_mode`)
    /// enqueues a run — others accumulate for the next run (§10).
    pub async fn route(&self, event: InboundEvent) -> Result<RouteOutcome, RouteError> {
        let targets = self.resolve_targets(&event).await?;
        if targets.is_empty() {
            self.record_drop(&event, "no-wiring").await?;
            return Ok(RouteOutcome::Dropped {
                reason: "no-wiring",
            });
        }

        let mut delivered = Vec::with_capacity(targets.len());
        for target in targets {
            let (session_id, engaged) = self.deliver_to_target(&event, target).await?;
            if engaged {
                self.queue.enqueue(session_id.clone());
            }
            delivered.push(session_id);
        }
        Ok(RouteOutcome::Delivered {
            sessions: delivered,
        })
    }

    /// Messaging-group lookup (auto-created on first contact) fanned out over its wirings.
    async fn resolve_targets(&self, event: &InboundEvent) -> Result<Vec<RouteTarget>, RouteError> {
        let central = self.central.clone();
        let channel_type = event.channel_type.clone();
        let platform_id = event.platform_id.clone();
        let is_group = event.is_group;
        blocking::run(move || {
            central.with(|conn| {
                let group =
                    match messaging_groups::get_by_platform(conn, &channel_type, &platform_id)? {
                        Some(group) => group,
                        None => messaging_groups::create(
                            conn,
                            &channel_type,
                            &platform_id,
                            None,
                            is_group,
                        )?,
                    };
                let wirings = messaging_groups::wirings_for(conn, &group.id)?;
                if wirings.is_empty() {
                    return connector_fallback_targets(conn, &channel_type, &group.id);
                }
                Ok(wirings
                    .into_iter()
                    .map(|wiring| RouteTarget {
                        agent_group_id: wiring.agent_group_id,
                        session_mode: wiring.session_mode,
                        engage_mode: wiring.engage_mode,
                        engage_pattern: wiring.engage_pattern,
                        messaging_group_id: group.id.clone(),
                    })
                    .collect())
            })
        })
        .await
    }

    async fn deliver_to_target(
        &self,
        event: &InboundEvent,
        target: RouteTarget,
    ) -> Result<(SessionId, bool), RouteError> {
        let central = self.central.clone();
        let store = self.store.clone();
        let event = event.clone();
        blocking::run(move || -> Result<(SessionId, bool), RouteError> {
            let session_thread = match target.session_mode {
                SessionMode::Shared => None,
                SessionMode::PerThread => event.thread_id.clone(),
            };
            let session = central.with(|conn| {
                match sessions::find_active(
                    conn,
                    &target.agent_group_id,
                    Some(&target.messaging_group_id),
                    session_thread.as_deref(),
                )? {
                    Some(session) => Ok(session),
                    None => sessions::create(
                        conn,
                        &target.agent_group_id,
                        Some(&target.messaging_group_id),
                        session_thread.as_deref(),
                    ),
                }
            })?;

            let engaged = engages(&event, &target);
            let routing = Routing {
                channel_type: Some(event.channel_type.clone()),
                platform_id: Some(event.platform_id.clone()),
                thread_id: event.thread_id.clone(),
            };
            let db = store.init(&session.agent_group_id, &session.id)?;
            db.write_routing(&routing)?;
            let mut message = NewInboundMessage::chat(event.content.clone(), routing);
            message.kind = event.kind;
            message.trigger = engaged;
            db.write_message(&message)?;
            Ok((session.id, engaged))
        })
        .await
    }

    async fn record_drop(&self, event: &InboundEvent, reason: &str) -> Result<(), RouteError> {
        let central = self.central.clone();
        let channel_type = event.channel_type.clone();
        let platform_id = event.platform_id.clone();
        let content = event.content.clone();
        let reason = reason.to_owned();
        blocking::run(move || {
            central.with(|conn| {
                dropped::record(conn, &channel_type, &platform_id, &reason, Some(&content))
            })
        })
        .await
    }
}

/// A channel with no explicit wirings falls back to its connector's assigned
/// agent (M17): the connector row IS the assignment, so reassigning it moves
/// every conversation on that channel at once. Wiring defaults apply (Shared
/// session, Mention engage — moot for DMs, which always run). Channels that are
/// not connector kinds ("web", "agent") never reach a connector row.
fn connector_fallback_targets(
    conn: &rusqlite::Connection,
    channel_type: &str,
    messaging_group_id: &crate::protocol::ids::MessagingGroupId,
) -> Result<Vec<RouteTarget>, rusqlite::Error> {
    let Ok(kind) = channel_type.parse::<ConnectorKind>() else {
        return Ok(Vec::new());
    };
    Ok(connectors::get_enabled_by_kind(conn, kind)?
        .map(|connector| {
            vec![RouteTarget {
                agent_group_id: connector.agent_group_id,
                session_mode: SessionMode::Shared,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                messaging_group_id: messaging_group_id.clone(),
            }]
        })
        .unwrap_or_default())
}

/// Applies the wiring's engage mode to an inbound message. Sticky mention state
/// is deferred (the router passes `sticky=false`) until group channels exist —
/// today the only channel is the web DM, which always engages.
fn engages(event: &InboundEvent, target: &RouteTarget) -> bool {
    let is_chat = matches!(event.kind, MessageKind::Chat);
    let text = if is_chat {
        chat_text(&event.kind, &event.content)
    } else {
        String::new()
    };
    let decision = evaluate(&EngageInput {
        is_chat,
        is_group: event.is_group,
        is_mention: event.is_mention,
        sticky: false,
        mode: target.engage_mode,
        pattern: target.engage_pattern.as_deref(),
        text: &text,
    });
    matches!(decision, Engage::Run)
}

fn chat_text(kind: &MessageKind, content: &str) -> String {
    match InboundContent::parse(kind.as_str(), content) {
        Ok(InboundContent::Chat(chat)) => chat.text,
        _ => String::new(),
    }
}

struct RouteTarget {
    agent_group_id: crate::protocol::ids::AgentGroupId,
    session_mode: SessionMode,
    engage_mode: EngageMode,
    engage_pattern: Option<String>,
    messaging_group_id: crate::protocol::ids::MessagingGroupId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::agent_groups;
    use crate::protocol::ids::AgentGroupId;

    struct Fixture {
        router: Router,
        central: Arc<CentralDb>,
        store: Arc<SessionStore>,
        queue: Arc<RunQueue>,
        agent_group_id: AgentGroupId,
        _tmp: tempfile::TempDir,
    }

    fn fixture(wired: bool) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let agent_group_id = central
            .with(|conn| {
                let group = agent_groups::create(conn, "Andy", "andy")?;
                if wired {
                    let mg = messaging_groups::create(conn, "web", "chat-1", None, false)?;
                    messaging_groups::wire(conn, &mg.id, &group.id)?;
                }
                Ok(group.id)
            })
            .expect("fixture rows");
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        let queue = Arc::new(RunQueue::new());
        Fixture {
            router: Router::new(central.clone(), store.clone(), queue.clone()),
            central,
            store,
            queue,
            agent_group_id,
            _tmp: tmp,
        }
    }

    fn chat_event(platform_id: &str, thread_id: Option<&str>, text: &str) -> InboundEvent {
        InboundEvent {
            channel_type: "web".to_owned(),
            platform_id: platform_id.to_owned(),
            thread_id: thread_id.map(str::to_owned),
            kind: MessageKind::Chat,
            content: format!("{{\"sender\":\"you\",\"text\":\"{text}\"}}"),
            is_mention: false,
            is_group: false,
        }
    }

    #[tokio::test]
    async fn wired_message_creates_session_writes_and_enqueues() {
        let fix = fixture(true);
        let outcome = fix
            .router
            .route(chat_event("chat-1", None, "hello"))
            .await
            .expect("route");
        let RouteOutcome::Delivered { sessions } = outcome else {
            panic!("expected delivery");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(fix.queue.snapshot(), sessions);

        let db = fix
            .store
            .open(&fix.agent_group_id, &sessions[0])
            .expect("open");
        let now = db.now_timestamp().expect("now");
        let pending = db.pending_due(&now, 10).expect("due");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].routing.platform_id.as_deref(), Some("chat-1"));
        let routing = db.routing().expect("routing").expect("must be set");
        assert_eq!(routing.channel_type.as_deref(), Some("web"));
    }

    fn group_event(platform_id: &str, text: &str, is_mention: bool) -> InboundEvent {
        InboundEvent {
            is_group: true,
            is_mention,
            ..chat_event(platform_id, None, text)
        }
    }

    #[tokio::test]
    async fn group_chat_accumulates_until_mentioned() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let agent_group_id = central
            .with(|conn| {
                let group = agent_groups::create(conn, "Andy", "andy")?;
                // is_group + default engage_mode = 'mention'.
                let mg = messaging_groups::create(conn, "web", "room-1", None, true)?;
                messaging_groups::wire(conn, &mg.id, &group.id)?;
                Ok(group.id)
            })
            .expect("seed");
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        let queue = Arc::new(RunQueue::new());
        let router = Router::new(central, store.clone(), queue.clone());

        // A non-mention message is written but must not wake the agent.
        let RouteOutcome::Delivered { sessions } = router
            .route(group_event("room-1", "hi all", false))
            .await
            .expect("route")
        else {
            panic!("expected delivery");
        };
        assert!(
            queue.snapshot().is_empty(),
            "a non-mention group message must not enqueue"
        );
        let db = store.open(&agent_group_id, &sessions[0]).expect("open");
        let now = db.now_timestamp().expect("now");
        let pending = db.pending_due(&now, 10).expect("due");
        assert_eq!(pending.len(), 1);
        assert!(
            !pending[0].trigger,
            "accumulated message keeps trigger=false"
        );

        // A mention engages and enqueues; the accumulated message rides along.
        router
            .route(group_event("room-1", "@andy deploy", true))
            .await
            .expect("route");
        assert_eq!(
            queue.snapshot().len(),
            1,
            "a mention must enqueue the session"
        );
        assert_eq!(db.pending_due(&now, 10).expect("due").len(), 2);
    }

    #[tokio::test]
    async fn second_message_reuses_the_shared_session() {
        let fix = fixture(true);
        let first = fix
            .router
            .route(chat_event("chat-1", None, "one"))
            .await
            .expect("route");
        let second = fix
            .router
            .route(chat_event("chat-1", Some("ignored-thread"), "two"))
            .await
            .expect("route");
        let (RouteOutcome::Delivered { sessions: a }, RouteOutcome::Delivered { sessions: b }) =
            (first, second)
        else {
            panic!("expected deliveries");
        };
        assert_eq!(a, b, "shared mode must reuse the session");

        let total: i64 = fix
            .central
            .with(|conn| conn.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0)))
            .expect("count");
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn per_thread_wiring_separates_sessions_by_thread() {
        let fix = fixture(true);
        fix.central
            .with(|conn| {
                let mg = messaging_groups::get_by_platform(conn, "web", "chat-1")?
                    .expect("messaging group");
                let mut wiring = messaging_groups::wirings_for(conn, &mg.id)?.remove(0);
                wiring.session_mode = SessionMode::PerThread;
                messaging_groups::update_wiring(conn, &wiring)?;
                Ok(())
            })
            .expect("rewire");

        let t1 = fix
            .router
            .route(chat_event("chat-1", Some("t1"), "one"))
            .await
            .expect("route");
        let t2 = fix
            .router
            .route(chat_event("chat-1", Some("t2"), "two"))
            .await
            .expect("route");
        let (RouteOutcome::Delivered { sessions: a }, RouteOutcome::Delivered { sessions: b }) =
            (t1, t2)
        else {
            panic!("expected deliveries");
        };
        assert_ne!(a, b, "threads must get separate sessions");
    }

    fn sms_event(platform_id: &str, text: &str) -> InboundEvent {
        InboundEvent {
            channel_type: "sms".to_owned(),
            ..chat_event(platform_id, None, text)
        }
    }

    fn create_sms_connector(
        conn: &rusqlite::Connection,
        agent_group_id: &AgentGroupId,
    ) -> Result<crate::db::connectors::Connector, rusqlite::Error> {
        let config = crate::protocol::entities::ConnectorConfig::Sms(
            crate::protocol::entities::SmsConnectorConfig {
                base_url: "http://sim:8080".to_owned(),
                token: "sms_secret".to_owned(),
                webhook_secret: None,
            },
        );
        connectors::create(conn, &config, agent_group_id, None)
    }

    #[tokio::test]
    async fn an_unwired_sms_number_falls_back_to_the_connectors_agent() {
        let fix = fixture(false);
        fix.central
            .with(|conn| create_sms_connector(conn, &fix.agent_group_id).map(|_| ()))
            .expect("connector");

        let outcome = fix
            .router
            .route(sms_event("+33612345678", "hello"))
            .await
            .expect("route");
        let RouteOutcome::Delivered { sessions } = outcome else {
            panic!("expected the connector fallback to deliver");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            fix.queue.snapshot(),
            sessions,
            "an SMS DM always engages and must enqueue"
        );

        let db = fix
            .store
            .open(&fix.agent_group_id, &sessions[0])
            .expect("open");
        let routing = db.routing().expect("routing").expect("must be set");
        assert_eq!(routing.channel_type.as_deref(), Some("sms"));
        assert_eq!(routing.platform_id.as_deref(), Some("+33612345678"));
    }

    #[tokio::test]
    async fn a_disabled_connector_still_drops_unwired_messages() {
        let fix = fixture(false);
        fix.central
            .with(|conn| {
                let mut connector = create_sms_connector(conn, &fix.agent_group_id)?;
                connector.enabled = false;
                connectors::update(conn, &connector)?;
                Ok(())
            })
            .expect("connector");

        let outcome = fix
            .router
            .route(sms_event("+33612345678", "hello"))
            .await
            .expect("route");
        assert_eq!(
            outcome,
            RouteOutcome::Dropped {
                reason: "no-wiring"
            }
        );
        assert!(fix.queue.snapshot().is_empty());
    }

    #[tokio::test]
    async fn an_explicit_wiring_beats_the_connector_fallback() {
        let fix = fixture(false);
        let pinned_group = fix
            .central
            .with(|conn| {
                create_sms_connector(conn, &fix.agent_group_id)?;
                // The number is explicitly wired to a different agent.
                let pinned = agent_groups::create(conn, "Coder", "coder")?;
                let mg = messaging_groups::create(conn, "sms", "+33612345678", None, false)?;
                messaging_groups::wire(conn, &mg.id, &pinned.id)?;
                Ok(pinned.id)
            })
            .expect("seed");

        let RouteOutcome::Delivered { sessions } = fix
            .router
            .route(sms_event("+33612345678", "hello"))
            .await
            .expect("route")
        else {
            panic!("expected delivery");
        };
        assert_eq!(sessions.len(), 1, "the wiring wins; no fallback fan-out");
        let owner: AgentGroupId = fix
            .central
            .with(|conn| {
                conn.query_row(
                    "SELECT agent_group_id FROM sessions WHERE id = ?1",
                    [&sessions[0]],
                    |row| row.get(0),
                )
            })
            .expect("session owner");
        assert_eq!(owner, pinned_group);
    }

    #[tokio::test]
    async fn unknown_chat_is_auto_created_and_unwired_messages_are_dropped() {
        let fix = fixture(false);
        let outcome = fix
            .router
            .route(chat_event("new-chat", None, "anyone?"))
            .await
            .expect("route");
        assert_eq!(
            outcome,
            RouteOutcome::Dropped {
                reason: "no-wiring"
            }
        );
        assert!(fix.queue.snapshot().is_empty());

        let (group_exists, drop_count) = fix
            .central
            .with(|conn| {
                let group = messaging_groups::get_by_platform(conn, "web", "new-chat")?;
                Ok((group.is_some(), dropped::count(conn)?))
            })
            .expect("query");
        assert!(group_exists, "messaging group must be auto-created");
        assert_eq!(drop_count, 1);
    }
}
