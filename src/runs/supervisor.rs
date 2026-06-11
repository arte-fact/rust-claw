use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::db::{CentralDb, DbError, agent_groups, sessions};
use crate::protocol::content::{InboundContent, OutboundContent, Routing};
use crate::protocol::entities::AgentProviderKind;
use crate::protocol::ids::SessionId;
use crate::protocol::message::MessageStatus;
use crate::providers::{
    ActiveRun, AgentProvider, ProviderError, ProviderEvent, QueryInput, create_provider,
};
use crate::session::{
    InboundMessage, NewOutboundMessage, SessionDb, SessionStore, SessionStoreError,
};

use super::queue::RunQueue;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("session {0} not found or inactive")]
    UnknownSession(SessionId),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

type ProviderFactory =
    Box<dyn Fn(AgentProviderKind) -> Result<Arc<dyn AgentProvider>, ProviderError> + Send + Sync>;

pub struct Supervisor {
    central: Arc<CentralDb>,
    store: Arc<SessionStore>,
    queue: Arc<RunQueue>,
    factory: ProviderFactory,
    batch_limit: i64,
}

impl Supervisor {
    #[must_use]
    pub fn new(central: Arc<CentralDb>, store: Arc<SessionStore>, queue: Arc<RunQueue>) -> Self {
        Self::with_factory(central, store, queue, Box::new(create_provider))
    }

    #[must_use]
    pub fn with_factory(
        central: Arc<CentralDb>,
        store: Arc<SessionStore>,
        queue: Arc<RunQueue>,
        factory: ProviderFactory,
    ) -> Self {
        Self {
            central,
            store,
            queue,
            factory,
            batch_limit: 10,
        }
    }

    /// The single run consumer: pops sessions one at a time, forever.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                session = self.queue.next() => {
                    if let Err(error) = self.run_session(&session).await {
                        tracing::error!(session = %session, %error, "agent run failed");
                    }
                }
            }
        }
    }

    /// Drain-and-exit: process due batches until none remain, then return.
    pub async fn run_session(&self, session_id: &SessionId) -> Result<(), RunError> {
        let (session, group) = self.load_session(session_id).await?;
        let db = self.open_session_db(&session).await?;
        let provider = (self.factory)(group.agent_provider.unwrap_or(AgentProviderKind::Native))?;

        loop {
            let batch = {
                let db = db.clone();
                let limit = self.batch_limit;
                blocking(move || {
                    let now = db.now_timestamp()?;
                    db.pending_due(&now, limit)
                })
                .await?
            };
            if batch.is_empty() || batch.iter().all(|message| !message.trigger) {
                break;
            }

            let ids: Vec<_> = batch.iter().map(|message| message.id.clone()).collect();
            {
                let db = db.clone();
                let ids = ids.clone();
                blocking(move || db.mark_status(&ids, MessageStatus::Processing)).await?;
            }

            let run = provider.start(QueryInput {
                prompt: draft_prompt(&batch),
                cwd: db.dir().to_path_buf(),
                session_dir: db.dir().join("pi"),
                model: group.model.clone(),
                system_context: None,
            })?;

            match consume_run(run).await {
                Ok(reply_text) => {
                    let db = db.clone();
                    let first_id = ids[0].clone();
                    blocking(move || {
                        if let Some(text) = reply_text.filter(|text| !text.trim().is_empty()) {
                            let content = OutboundContent::from_text(text);
                            let mut reply = NewOutboundMessage::chat(
                                serde_json::to_string(&content).unwrap_or_else(|_| "{}".to_owned()),
                                Routing::default(),
                            );
                            reply.in_reply_to = Some(first_id);
                            db.write_outbound(&reply)?;
                        }
                        db.mark_status(&ids, MessageStatus::Completed)
                    })
                    .await?;
                }
                Err(message) => {
                    tracing::error!(session = %session.id, error = %message, "agent turn failed");
                    let db = db.clone();
                    blocking(move || db.mark_status(&ids, MessageStatus::Failed)).await?;
                }
            }
        }

        let central = self.central.clone();
        let session_id = session.id.clone();
        blocking(move || central.with(|conn| sessions::touch_last_active(conn, &session_id)))
            .await?;
        Ok(())
    }

    async fn load_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(sessions::Session, agent_groups::AgentGroup), RunError> {
        let central = self.central.clone();
        let id = session_id.clone();
        let loaded = blocking(move || {
            central.with(|conn| {
                let Some(session) = sessions::get(conn, &id)? else {
                    return Ok(None);
                };
                let group = agent_groups::get(conn, &session.agent_group_id)?;
                Ok(group.map(|group| (session, group)))
            })
        })
        .await?;
        loaded.ok_or_else(|| RunError::UnknownSession(session_id.clone()))
    }

    async fn open_session_db(
        &self,
        session: &sessions::Session,
    ) -> Result<Arc<SessionDb>, RunError> {
        let store = self.store.clone();
        let agent_group = session.agent_group_id.clone();
        let id = session.id.clone();
        Ok(Arc::new(
            blocking(move || store.open(&agent_group, &id)).await?,
        ))
    }
}

async fn blocking<T, E>(op: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, RunError>
where
    T: Send + 'static,
    E: Into<RunError> + Send + 'static,
{
    crate::blocking::run(op).await
}

/// Stand-in prompt builder until the real XML formatter lands (M4.2).
fn draft_prompt(batch: &[InboundMessage]) -> String {
    batch
        .iter()
        .map(
            |message| match InboundContent::parse(&message.kind, &message.content) {
                Ok(InboundContent::Chat(chat)) => chat.text,
                _ => message.content.clone(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

/// Per-batch turn: no follow-ups are pushed; the drain loop handles new arrivals.
async fn consume_run(mut run: ActiveRun) -> Result<Option<String>, String> {
    drop(run.input);
    while let Some(event) = run.events.recv().await {
        match event {
            ProviderEvent::TurnEnd { text } => return Ok(text),
            ProviderEvent::Error { message, .. } => return Err(message),
            ProviderEvent::Activity | ProviderEvent::Progress { .. } => {}
        }
    }
    Err("agent run ended without a result".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::content::Routing;
    use crate::protocol::ids::AgentGroupId;
    use crate::session::NewInboundMessage;
    use tokio::sync::mpsc;

    struct Fixture {
        central: Arc<CentralDb>,
        store: Arc<SessionStore>,
        queue: Arc<RunQueue>,
        session_id: SessionId,
        agent_group_id: AgentGroupId,
        _tmp: tempfile::TempDir,
    }

    fn fixture(provider: AgentProviderKind) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let (session_id, agent_group_id) = central
            .with(|conn| {
                let mut group = agent_groups::create(conn, "Test", "test")?;
                group.agent_provider = Some(provider);
                agent_groups::update(conn, &group)?;
                let session = sessions::create(conn, &group.id, None, None)?;
                Ok((session.id, group.id))
            })
            .expect("fixture rows");
        Fixture {
            central,
            store: Arc::new(SessionStore::new(tmp.path().to_path_buf())),
            queue: Arc::new(RunQueue::new()),
            session_id,
            agent_group_id,
            _tmp: tmp,
        }
    }

    fn supervisor(fix: &Fixture) -> Supervisor {
        Supervisor::new(fix.central.clone(), fix.store.clone(), fix.queue.clone())
    }

    fn write_chat(fix: &Fixture, text: &str) {
        let db = fix
            .store
            .open(&fix.agent_group_id, &fix.session_id)
            .expect("open");
        let content = format!("{{\"sender\":\"you\",\"text\":\"{text}\"}}");
        db.write_message(&NewInboundMessage::chat(content, Routing::default()))
            .expect("write");
    }

    fn session_db(fix: &Fixture) -> SessionDb {
        fix.store
            .open(&fix.agent_group_id, &fix.session_id)
            .expect("open")
    }

    #[tokio::test]
    async fn echo_run_completes_batch_and_writes_reply() {
        let fix = fixture(AgentProviderKind::Echo);
        write_chat(&fix, "hello");
        write_chat(&fix, "world");

        supervisor(&fix)
            .run_session(&fix.session_id)
            .await
            .expect("run");

        let db = session_db(&fix);
        let now = db.now_timestamp().expect("now");
        assert!(db.pending_due(&now, 10).expect("due").is_empty());
        let replies = db.due_outbound(&now).expect("outbound");
        assert_eq!(replies.len(), 1);
        let content = OutboundContent::parse(&replies[0].content).expect("content");
        assert_eq!(content.text.as_deref(), Some("hello\nworld"));
        assert!(replies[0].seq.is_agent_assigned());
        assert!(replies[0].in_reply_to.is_some());
    }

    #[tokio::test]
    async fn unavailable_provider_leaves_messages_pending() {
        let fix = fixture(AgentProviderKind::Native);
        write_chat(&fix, "hello");

        let err = supervisor(&fix)
            .run_session(&fix.session_id)
            .await
            .expect_err("native is not built yet");
        assert!(matches!(err, RunError::Provider(_)));

        let db = session_db(&fix);
        let now = db.now_timestamp().expect("now");
        assert_eq!(db.pending_due(&now, 10).expect("due").len(), 1);
    }

    #[tokio::test]
    async fn provider_error_event_marks_the_batch_failed() {
        struct FailingProvider;
        impl AgentProvider for FailingProvider {
            fn start(&self, _input: QueryInput) -> Result<ActiveRun, ProviderError> {
                let (input_tx, _input_rx) = mpsc::channel(1);
                let (event_tx, event_rx) = mpsc::channel(1);
                tokio::spawn(async move {
                    let _ = event_tx
                        .send(ProviderEvent::Error {
                            message: "boom".to_owned(),
                            retryable: false,
                        })
                        .await;
                });
                Ok(ActiveRun {
                    input: input_tx,
                    events: event_rx,
                    abort: CancellationToken::new(),
                })
            }
        }

        let fix = fixture(AgentProviderKind::Echo);
        write_chat(&fix, "hello");
        let sup = Supervisor::with_factory(
            fix.central.clone(),
            fix.store.clone(),
            fix.queue.clone(),
            Box::new(|_| Ok(Arc::new(FailingProvider))),
        );
        sup.run_session(&fix.session_id).await.expect("run");

        let db = session_db(&fix);
        let now = db.now_timestamp().expect("now");
        assert!(db.pending_due(&now, 10).expect("due").is_empty());
        assert!(db.due_outbound(&now).expect("outbound").is_empty());
    }

    #[tokio::test]
    async fn accumulated_only_context_does_not_start_a_run() {
        let fix = fixture(AgentProviderKind::Echo);
        let db = session_db(&fix);
        let mut context = NewInboundMessage::chat(
            "{\"sender\":\"you\",\"text\":\"fyi\"}".to_owned(),
            Routing::default(),
        );
        context.trigger = false;
        db.write_message(&context).expect("write");

        supervisor(&fix)
            .run_session(&fix.session_id)
            .await
            .expect("run");

        let now = db.now_timestamp().expect("now");
        assert_eq!(db.pending_due(&now, 10).expect("due").len(), 1);
        assert!(db.due_outbound(&now).expect("outbound").is_empty());
    }

    #[tokio::test]
    async fn messages_arriving_mid_run_are_drained_before_returning() {
        struct GatedProvider {
            release: Arc<tokio::sync::Semaphore>,
        }
        impl AgentProvider for GatedProvider {
            fn start(&self, input: QueryInput) -> Result<ActiveRun, ProviderError> {
                let (input_tx, _input_rx) = mpsc::channel(1);
                let (event_tx, event_rx) = mpsc::channel(1);
                let release = self.release.clone();
                tokio::spawn(async move {
                    let permit = release.acquire().await;
                    drop(permit);
                    let _ = event_tx
                        .send(ProviderEvent::TurnEnd {
                            text: Some(input.prompt),
                        })
                        .await;
                });
                Ok(ActiveRun {
                    input: input_tx,
                    events: event_rx,
                    abort: CancellationToken::new(),
                })
            }
        }

        let fix = fixture(AgentProviderKind::Echo);
        write_chat(&fix, "first");

        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let sup = {
            let release = release.clone();
            Supervisor::with_factory(
                fix.central.clone(),
                fix.store.clone(),
                fix.queue.clone(),
                Box::new(move |_| {
                    Ok(Arc::new(GatedProvider {
                        release: release.clone(),
                    }))
                }),
            )
        };

        let run = tokio::spawn({
            let session_id = fix.session_id.clone();
            async move { sup.run_session(&session_id).await }
        });

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        write_chat(&fix, "second");
        release.add_permits(2);

        run.await.expect("join").expect("run");

        let db = session_db(&fix);
        let now = db.now_timestamp().expect("now");
        assert!(db.pending_due(&now, 10).expect("due").is_empty());
        assert_eq!(db.due_outbound(&now).expect("outbound").len(), 2);
    }
}
