use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::blocking;
use crate::db::{CentralDb, DbError, dropped, messaging_groups, sessions};
use crate::protocol::content::Routing;
use crate::protocol::entities::SessionMode;
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

#[derive(Debug, Clone)]
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

    /// Minimal routing: every wiring engages (engage modes land with M8.2).
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
            let session_id = self.deliver_to_target(&event, target).await?;
            self.queue.enqueue(session_id.clone());
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
                Ok(wirings
                    .into_iter()
                    .map(|wiring| RouteTarget {
                        agent_group_id: wiring.agent_group_id,
                        session_mode: wiring.session_mode,
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
    ) -> Result<SessionId, RouteError> {
        let central = self.central.clone();
        let store = self.store.clone();
        let event = event.clone();
        blocking::run(move || -> Result<SessionId, RouteError> {
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

            let routing = Routing {
                channel_type: Some(event.channel_type.clone()),
                platform_id: Some(event.platform_id.clone()),
                thread_id: event.thread_id.clone(),
            };
            let db = store.init(&session.agent_group_id, &session.id)?;
            db.write_routing(&routing)?;
            let mut message = NewInboundMessage::chat(event.content.clone(), routing);
            message.kind = event.kind;
            db.write_message(&message)?;
            Ok(session.id)
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

struct RouteTarget {
    agent_group_id: crate::protocol::ids::AgentGroupId,
    session_mode: SessionMode,
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
