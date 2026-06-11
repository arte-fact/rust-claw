use std::sync::Arc;
use std::time::Duration;

use jiff::tz::TimeZone;
use tokio_util::sync::CancellationToken;

use crate::cron::Cron;
use crate::db::{CentralDb, DbError, questions, sessions, web_messages};
use crate::runs::queue::RunQueue;
use crate::session::{InboundMessage, SessionDb, SessionStore, SessionStoreError};

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// How long an unanswered question stays open before the card collapses to "no answer".
const QUESTION_TTL_SECONDS: i64 = 24 * 3600;

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// The 60s housekeeping pass over every active session: advance due recurrences
/// and wake sessions whose scheduled messages have come due (§10). Outbound
/// `deliver_after` is handled by the delivery loop's own sweep, not here.
pub struct Sweep {
    central: Arc<CentralDb>,
    store: Arc<SessionStore>,
    queue: Arc<RunQueue>,
    timezone: TimeZone,
}

impl Sweep {
    #[must_use]
    pub fn new(
        central: Arc<CentralDb>,
        store: Arc<SessionStore>,
        queue: Arc<RunQueue>,
        timezone: &str,
    ) -> Self {
        Self {
            central,
            store,
            queue,
            timezone: TimeZone::get(timezone).unwrap_or(TimeZone::UTC),
        }
    }

    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if let Err(error) = self.sweep_all().await {
                        tracing::error!(%error, "sweep failed");
                    }
                }
            }
        }
    }

    pub async fn sweep_all(&self) -> Result<(), SweepError> {
        let central = self.central.clone();
        let active = blocking(move || central.with(sessions::list_active)).await?;
        for session in active {
            if let Err(error) = self.sweep_session(&session).await {
                tracing::error!(session = %session.id, %error, "session sweep failed");
            }
        }
        self.expire_questions().await
    }

    /// Collapses the cards of questions that went unanswered past their TTL.
    async fn expire_questions(&self) -> Result<(), SweepError> {
        let central = self.central.clone();
        blocking(move || {
            central.with(|conn| {
                for stale in questions::expire_stale(conn, QUESTION_TTL_SECONDS)? {
                    web_messages::resolve_question(conn, &stale.question_id, "no answer")?;
                }
                Ok(())
            })
        })
        .await
    }

    pub async fn sweep_session(&self, session: &sessions::Session) -> Result<(), SweepError> {
        let db = {
            let store = self.store.clone();
            let agent_group = session.agent_group_id.clone();
            let id = session.id.clone();
            Arc::new(blocking(move || store.open(&agent_group, &id)).await?)
        };

        let timezone = self.timezone.clone();
        let due_pending = {
            let db = db.clone();
            blocking(move || advance_and_check_due(&db, &timezone)).await?
        };

        if due_pending {
            self.queue.enqueue(session.id.clone());
        }
        Ok(())
    }
}

/// Advances every completed recurrence in the session, then reports whether any
/// pending trigger is now due (so the caller can wake the session). Runs on the
/// blocking pool — all SQLite, no awaits.
fn advance_and_check_due(db: &SessionDb, timezone: &TimeZone) -> Result<bool, SessionStoreError> {
    let now = db.now_timestamp()?;
    for completed in db.completed_recurrences()? {
        match next_occurrence(&completed, &now, timezone) {
            Some(next) => db.advance_recurrence(&completed, &next)?,
            None => {
                tracing::warn!(message = %completed.id, "dropping unschedulable recurrence");
                db.clear_recurrence(&completed.id)?;
            }
        }
    }
    Ok(db.count_pending_triggers(&now)? > 0)
}

/// Next fire time for a recurring row, anchored at `now` so the grid is absolute:
/// no drift from processing latency, and missed windows are skipped, not replayed.
fn next_occurrence(completed: &InboundMessage, now: &str, timezone: &TimeZone) -> Option<String> {
    let recurrence = completed.recurrence.as_deref()?;
    let cron = Cron::parse(recurrence).ok()?;
    crate::cron::next_after_utc(&cron, now, timezone)
}

async fn blocking<T, E>(op: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, SweepError>
where
    T: Send + 'static,
    E: Into<SweepError> + Send + 'static,
{
    crate::blocking::run(op).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{agent_groups, messaging_groups};
    use crate::protocol::content::Routing;
    use crate::protocol::message::{MessageKind, MessageStatus};
    use crate::session::NewInboundMessage;

    struct Fixture {
        sweep: Sweep,
        store: Arc<SessionStore>,
        queue: Arc<RunQueue>,
        session: sessions::Session,
        _tmp: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let session = central
            .with(|conn| {
                let group = agent_groups::create(conn, "Andy", "andy")?;
                let mg = messaging_groups::create(conn, "web", "chat-1", None, false)?;
                sessions::create(conn, &group.id, Some(&mg.id), None)
            })
            .expect("rows");
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        let queue = Arc::new(RunQueue::new());
        Fixture {
            sweep: Sweep::new(central, store.clone(), queue.clone(), "UTC"),
            store,
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

    #[tokio::test]
    async fn due_pending_task_enqueues_the_session() {
        let fix = fixture();
        session_db(&fix)
            .write_message(&NewInboundMessage::chat(
                "{\"text\":\"go\"}".to_owned(),
                Routing::default(),
            ))
            .expect("write");

        fix.sweep.sweep_session(&fix.session).await.expect("sweep");
        assert_eq!(fix.queue.snapshot(), vec![fix.session.id.clone()]);
    }

    #[tokio::test]
    async fn future_scheduled_task_does_not_enqueue() {
        let fix = fixture();
        let mut scheduled =
            NewInboundMessage::chat("{\"text\":\"later\"}".to_owned(), Routing::default());
        scheduled.process_after = Some("2999-01-01T00:00:00.000Z".to_owned());
        session_db(&fix).write_message(&scheduled).expect("write");

        fix.sweep.sweep_session(&fix.session).await.expect("sweep");
        assert!(fix.queue.snapshot().is_empty());
    }

    #[tokio::test]
    async fn completed_recurrence_spawns_one_future_occurrence_idempotently() {
        let fix = fixture();
        let db = session_db(&fix);
        let recurring = NewInboundMessage {
            kind: MessageKind::Task,
            content: "{\"prompt\":\"daily briefing\"}".to_owned(),
            routing: Routing::default(),
            trigger: true,
            process_after: Some("2026-06-11T09:00:00.000Z".to_owned()),
            recurrence: Some("0 9 * * *".to_owned()),
            series_id: Some("series-1".to_owned()),
            source_session_id: None,
        };
        let written = db.write_message(&recurring).expect("write");
        db.mark_status(std::slice::from_ref(&written.id), MessageStatus::Completed)
            .expect("complete");

        fix.sweep.sweep_session(&fix.session).await.expect("sweep");

        assert!(
            db.completed_recurrences().expect("query").is_empty(),
            "completed recurrence must be cleared"
        );
        let all = db
            .pending_due("2999-12-31T00:00:00.000Z", 100)
            .expect("pending");
        assert_eq!(all.len(), 1, "exactly one next occurrence");
        let next = &all[0];
        assert_eq!(next.recurrence.as_deref(), Some("0 9 * * *"));
        assert_eq!(next.series_id.as_deref(), Some("series-1"));
        assert_eq!(next.kind, "task");
        assert!(
            next.process_after
                .as_deref()
                .expect("scheduled")
                .ends_with("09:00:00.000Z"),
            "grid-aligned to 09:00"
        );

        // A second sweep must not create another occurrence.
        fix.sweep.sweep_session(&fix.session).await.expect("sweep");
        assert_eq!(
            db.pending_due("2999-12-31T00:00:00.000Z", 100)
                .expect("pending")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn unschedulable_recurrence_is_dropped() {
        let fix = fixture();
        let db = session_db(&fix);
        let mut bad = NewInboundMessage::chat("{}".to_owned(), Routing::default());
        bad.recurrence = Some("not a cron".to_owned());
        let written = db.write_message(&bad).expect("write");
        db.mark_status(std::slice::from_ref(&written.id), MessageStatus::Completed)
            .expect("complete");

        fix.sweep.sweep_session(&fix.session).await.expect("sweep");
        assert!(db.completed_recurrences().expect("query").is_empty());
    }
}
