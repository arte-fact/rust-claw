use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::commands::{CallerContext, Dispatcher};
use crate::db::{CentralDb, DbError, agent_groups, sessions};
use crate::protocol::content::{InboundContent, OutboundContent, Routing};
use crate::protocol::entities::{AgentProviderKind, CliScope};
use crate::protocol::ids::SessionId;
use crate::protocol::message::MessageStatus;
use crate::providers::{
    ActiveRun, AgentAdmin, AgentProvider, ProviderError, ProviderEvent, QueryInput, create_provider,
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
    #[error(transparent)]
    Resolution(#[from] crate::providers::resolution::ResolutionError),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

type ProviderFactory =
    Box<dyn Fn(AgentProviderKind) -> Result<Arc<dyn AgentProvider>, ProviderError> + Send + Sync>;

/// The supervisor's slice of the instance config.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub groups_dir: std::path::PathBuf,
    pub default_endpoint: Option<String>,
    pub default_model: Option<String>,
}

pub struct Supervisor {
    central: Arc<CentralDb>,
    store: Arc<SessionStore>,
    queue: Arc<RunQueue>,
    dispatcher: Arc<dyn Dispatcher>,
    config: RunConfig,
    factory: ProviderFactory,
    batch_limit: i64,
}

impl Supervisor {
    #[must_use]
    pub fn new(
        central: Arc<CentralDb>,
        store: Arc<SessionStore>,
        queue: Arc<RunQueue>,
        dispatcher: Arc<dyn Dispatcher>,
        config: RunConfig,
    ) -> Self {
        Self::with_factory(
            central,
            store,
            queue,
            dispatcher,
            config,
            Box::new(create_provider),
        )
    }

    #[must_use]
    pub fn with_factory(
        central: Arc<CentralDb>,
        store: Arc<SessionStore>,
        queue: Arc<RunQueue>,
        dispatcher: Arc<dyn Dispatcher>,
        config: RunConfig,
        factory: ProviderFactory,
    ) -> Self {
        Self {
            central,
            store,
            queue,
            dispatcher,
            config,
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
        let provider_kind = group.agent_provider.unwrap_or(AgentProviderKind::Native);
        let provider = (self.factory)(provider_kind)?;
        let inference = self
            .resolve_inference_if_needed(provider_kind, &group)
            .await?;
        let workspace = self.config.groups_dir.join(&group.folder);
        {
            let workspace = workspace.clone();
            let name = group.name.clone();
            blocking(move || {
                scaffold_workspace(&workspace, &name).map_err(SessionStoreError::from)
            })
            .await?;
        }

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
                cwd: workspace.clone(),
                session_dir: db.dir().to_path_buf(),
                model: group.model.clone(),
                system_context: None,
                inference: inference.clone(),
                tool_profile: group.tool_profile,
                admin: self.agent_admin(&session, &group),
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
                    tracing::warn!(session = %session.id, error = %message, "agent turn failed");
                    let db = db.clone();
                    let batch = batch.clone();
                    blocking(move || apply_retry_backoff(&db, &batch)).await?;
                    // One failure ends this session's drain; the sweep re-wakes it when due.
                    break;
                }
            }
        }

        let central = self.central.clone();
        let session_id = session.id.clone();
        blocking(move || central.with(|conn| sessions::touch_last_active(conn, &session_id)))
            .await?;
        Ok(())
    }

    async fn resolve_inference_if_needed(
        &self,
        provider_kind: AgentProviderKind,
        group: &agent_groups::AgentGroup,
    ) -> Result<Option<crate::providers::resolution::ResolvedInference>, RunError> {
        match provider_kind {
            AgentProviderKind::Echo => Ok(None),
            AgentProviderKind::Native => {
                let central = self.central.clone();
                let group = group.clone();
                let default_endpoint = self.config.default_endpoint.clone();
                let default_model = self.config.default_model.clone();
                let resolved = blocking(move || {
                    central.with(|conn| {
                        Ok(crate::providers::resolution::resolve_inference(
                            conn,
                            &group,
                            default_endpoint.as_deref(),
                            default_model.as_deref(),
                            |var| std::env::var(var).ok(),
                        ))
                    })
                })
                .await??;
                Ok(Some(resolved))
            }
        }
    }

    /// Hands a run admin access only when its group's `cli_scope` permits it; the
    /// dispatcher re-enforces scope per command (M6.3), so this just opens the seam.
    fn agent_admin(
        &self,
        session: &sessions::Session,
        group: &agent_groups::AgentGroup,
    ) -> Option<AgentAdmin> {
        if group.cli_scope == CliScope::Disabled {
            return None;
        }
        Some(AgentAdmin {
            dispatcher: self.dispatcher.clone(),
            caller: CallerContext::Agent {
                session_id: session.id.clone(),
                agent_group_id: group.id.clone(),
                messaging_group_id: session.messaging_group_id.clone(),
            },
        })
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
/// Creates a group's workspace and, on a fresh group, drops a starter `AGENT.md`
/// so a newly created agent (M9.1) boots with a basic persona instead of none.
fn scaffold_workspace(workspace: &std::path::Path, group_name: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(workspace)?;
    let agent_md = workspace.join("AGENT.md");
    if !agent_md.exists() {
        std::fs::write(
            &agent_md,
            format!(
                "# {group_name}\n\nYou are {group_name}, a helpful assistant. Be concise and use \
                 your tools when they help.\n"
            ),
        )?;
    }
    Ok(())
}

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

/// Increments each batch message's tries and either reschedules it with
/// exponential backoff or fails it once retries are exhausted (§8.2).
fn apply_retry_backoff(db: &SessionDb, batch: &[InboundMessage]) -> Result<(), SessionStoreError> {
    let now = db.now_timestamp()?;
    for message in batch {
        let tries = message.tries + 1;
        match crate::recovery::retry_decision(tries, crate::recovery::MAX_TRIES) {
            crate::recovery::Retry::Fail => db.fail_message(&message.id, tries)?,
            crate::recovery::Retry::After(delay) => {
                let process_after = crate::recovery::add_seconds_utc(&now, delay.as_secs())
                    .unwrap_or_else(|| now.clone());
                db.reschedule_retry(&message.id, tries, &process_after)?;
            }
        }
    }
    Ok(())
}

/// Per-batch turn: no follow-ups are pushed; the drain loop handles new arrivals.
/// A run that goes silent past the watchdog deadline is aborted and treated as a
/// (retryable) failure.
async fn consume_run(mut run: ActiveRun) -> Result<Option<String>, String> {
    drop(run.input);
    loop {
        match tokio::time::timeout(crate::recovery::WATCHDOG_TIMEOUT, run.events.recv()).await {
            Err(_) => {
                run.abort.cancel();
                return Err(format!(
                    "watchdog: no activity for {}s",
                    crate::recovery::WATCHDOG_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => return Err("agent run ended without a result".to_owned()),
            Ok(Some(ProviderEvent::TurnEnd { text })) => return Ok(text),
            Ok(Some(ProviderEvent::Error { message, .. })) => return Err(message),
            Ok(Some(ProviderEvent::Activity | ProviderEvent::Progress { .. })) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::content::Routing;
    use crate::protocol::ids::AgentGroupId;
    use crate::session::NewInboundMessage;
    use tokio::sync::mpsc;

    #[test]
    fn scaffold_workspace_writes_a_default_agent_md_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("newbie");
        scaffold_workspace(&workspace, "Newbie").expect("scaffold");
        let md = std::fs::read_to_string(workspace.join("AGENT.md")).expect("AGENT.md");
        assert!(md.contains("Newbie"));

        // An edited AGENT.md is never clobbered on the next run.
        std::fs::write(workspace.join("AGENT.md"), "custom").expect("edit");
        scaffold_workspace(&workspace, "Newbie").expect("re-scaffold");
        assert_eq!(
            std::fs::read_to_string(workspace.join("AGENT.md")).expect("read"),
            "custom"
        );
    }

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

    fn run_config(fix: &Fixture) -> RunConfig {
        RunConfig {
            groups_dir: fix._tmp.path().join("groups"),
            default_endpoint: None,
            default_model: None,
        }
    }

    fn dispatcher(fix: &Fixture) -> Arc<dyn Dispatcher> {
        Arc::new(crate::commands::Registry::new(fix.central.clone()))
    }

    fn supervisor(fix: &Fixture) -> Supervisor {
        Supervisor::new(
            fix.central.clone(),
            fix.store.clone(),
            fix.queue.clone(),
            dispatcher(fix),
            run_config(fix),
        )
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
    async fn native_without_an_endpoint_fails_resolution_and_leaves_messages_pending() {
        let fix = fixture(AgentProviderKind::Native);
        write_chat(&fix, "hello");

        let err = supervisor(&fix)
            .run_session(&fix.session_id)
            .await
            .expect_err("no endpoint is configured");
        assert!(matches!(err, RunError::Resolution(_)));

        let db = session_db(&fix);
        let now = db.now_timestamp().expect("now");
        assert_eq!(db.pending_due(&now, 10).expect("due").len(), 1);
    }

    struct FailingProvider;
    impl AgentProvider for FailingProvider {
        fn start(&self, _input: QueryInput) -> Result<ActiveRun, ProviderError> {
            let (input_tx, _input_rx) = mpsc::channel(1);
            let (event_tx, event_rx) = mpsc::channel(1);
            tokio::spawn(async move {
                let _ = event_tx
                    .send(ProviderEvent::Error {
                        message: "boom".to_owned(),
                        retryable: true,
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

    fn failing_supervisor(fix: &Fixture) -> Supervisor {
        Supervisor::with_factory(
            fix.central.clone(),
            fix.store.clone(),
            fix.queue.clone(),
            dispatcher(fix),
            run_config(fix),
            Box::new(|_| Ok(Arc::new(FailingProvider))),
        )
    }

    #[tokio::test]
    async fn provider_error_reschedules_with_backoff_then_fails() {
        let fix = fixture(AgentProviderKind::Echo);
        write_chat(&fix, "hello");
        let db = session_db(&fix);

        // First four failures reschedule the message (pending, future, tries bumped).
        for expected_tries in 1..=4 {
            failing_supervisor(&fix)
                .run_session(&fix.session_id)
                .await
                .expect("run");
            let all = db
                .pending_due("2999-12-31T00:00:00.000Z", 10)
                .expect("pending");
            assert_eq!(all.len(), 1, "still pending after failure {expected_tries}");
            assert_eq!(all[0].tries, expected_tries);
            assert!(
                all[0].process_after.is_some(),
                "rescheduled into the future"
            );
            // Make it due again for the next attempt.
            db.reschedule_retry(&all[0].id, all[0].tries, "2000-01-01T00:00:00.000Z")
                .expect("force due");
        }

        // The fifth failure exhausts retries → failed, no longer pending.
        failing_supervisor(&fix)
            .run_session(&fix.session_id)
            .await
            .expect("run");
        assert!(
            db.pending_due("2999-12-31T00:00:00.000Z", 10)
                .expect("pending")
                .is_empty(),
            "message must be failed, not pending"
        );
        assert!(
            db.due_outbound("2999-12-31T00:00:00.000Z")
                .expect("out")
                .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_run_trips_the_watchdog_and_retries() {
        // A provider that never emits and keeps its channel open: the watchdog must
        // fire. Paused time auto-advances to the watchdog deadline.
        struct SilentProvider;
        impl AgentProvider for SilentProvider {
            fn start(&self, _input: QueryInput) -> Result<ActiveRun, ProviderError> {
                let (input_tx, _input_rx) = mpsc::channel(1);
                let (event_tx, event_rx) = mpsc::channel::<ProviderEvent>(1);
                // Hold the sender forever so the event channel never closes.
                tokio::spawn(async move {
                    let _keep = event_tx;
                    std::future::pending::<()>().await;
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
            dispatcher(&fix),
            run_config(&fix),
            Box::new(|_| Ok(Arc::new(SilentProvider))),
        );
        sup.run_session(&fix.session_id).await.expect("run");

        // Treated as a retryable failure: rescheduled, tries bumped, still pending.
        let db = session_db(&fix);
        let all = db
            .pending_due("2999-12-31T00:00:00.000Z", 10)
            .expect("pending");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].tries, 1);
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
                dispatcher(&fix),
                run_config(&fix),
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
