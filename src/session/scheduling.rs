use rusqlite::params;

use crate::db::generate_id;
use crate::protocol::content::Routing;
use crate::protocol::ids::MessageInId;
use crate::protocol::message::{MessageKind, Seq};

use super::inbound::{SELECT_INBOUND, from_row, highest_seq};
use super::{InboundMessage, NewInboundMessage, SessionDb, SessionStoreError};

/// A scheduled task as the agent sees it: a stable `series` handle plus its
/// prompt and schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledTask {
    pub series: String,
    pub prompt: String,
    pub process_after: Option<String>,
    pub recurrence: Option<String>,
    pub paused: bool,
}

impl SessionDb {
    // ── recurrence (driven by the sweep) ────────────────────────────────

    /// Completed rows that still carry a `recurrence` — each owes one next occurrence.
    pub fn completed_recurrences(&self) -> Result<Vec<InboundMessage>, SessionStoreError> {
        self.with(|conn| {
            conn.prepare(&format!(
                "{SELECT_INBOUND}
                 WHERE status = 'completed' AND recurrence IS NOT NULL
                 ORDER BY seq"
            ))?
            .query_map([], from_row)?
            .collect()
        })
    }

    /// Inserts the next occurrence (new pending row, same content/kind/recurrence/
    /// series, given `process_after`) and clears the completed row's recurrence so
    /// it is advanced exactly once. Clearing happens first: a crash mid-call drops
    /// an occurrence rather than duplicating one.
    pub fn advance_recurrence(
        &self,
        completed: &InboundMessage,
        next_process_after: &str,
    ) -> Result<(), SessionStoreError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE messages_in SET recurrence = NULL WHERE id = ?1",
                params![completed.id],
            )?;
            let seq = Seq::next_claw_after(highest_seq(conn)?);
            let id = MessageInId::new(generate_id("in"));
            conn.execute(
                "INSERT INTO messages_in
                   (id, seq, kind, timestamp, status, process_after, recurrence, series_id,
                    trigger, platform_id, channel_type, thread_id, content, source_session_id)
                 VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'pending', ?4, ?5, ?6,
                    ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    id,
                    seq,
                    completed.kind,
                    next_process_after,
                    completed.recurrence,
                    completed.series_id,
                    completed.trigger,
                    completed.routing.platform_id,
                    completed.routing.channel_type,
                    completed.routing.thread_id,
                    completed.content,
                    completed.source_session_id,
                ],
            )?;
            Ok(())
        })
    }

    /// Drops a recurrence that can no longer advance (e.g. an unparseable cron).
    pub fn clear_recurrence(&self, id: &MessageInId) -> Result<(), SessionStoreError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE messages_in SET recurrence = NULL WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    // ── task lifecycle (driven by agent tools) ──────────────────────────

    /// Schedules a task: a `task` row carrying the prompt, with a generated
    /// `series` handle the agent can later pause/resume/cancel.
    pub fn schedule_task(
        &self,
        prompt: &str,
        process_after: Option<&str>,
        recurrence: Option<&str>,
    ) -> Result<String, SessionStoreError> {
        let series = generate_id("task");
        let content = serde_json::json!({ "prompt": prompt }).to_string();
        let message = NewInboundMessage {
            kind: MessageKind::Task,
            content,
            routing: Routing::default(),
            trigger: true,
            process_after: process_after.map(str::to_owned),
            recurrence: recurrence.map(str::to_owned),
            series_id: Some(series.clone()),
            source_session_id: None,
        };
        self.write_message(&message)?;
        Ok(series)
    }

    /// Active (pending or paused) scheduled tasks, oldest first.
    pub fn list_scheduled_tasks(&self) -> Result<Vec<ScheduledTask>, SessionStoreError> {
        self.with(|conn| {
            conn.prepare(
                "SELECT series_id, content, process_after, recurrence, status
                 FROM messages_in
                 WHERE kind = 'task' AND series_id IS NOT NULL
                   AND status IN ('pending', 'paused')
                 ORDER BY seq",
            )?
            .query_map([], |row| {
                let content: String = row.get(1)?;
                let prompt = serde_json::from_str::<serde_json::Value>(&content)
                    .ok()
                    .and_then(|value| value.get("prompt")?.as_str().map(str::to_owned))
                    .unwrap_or_default();
                let status: String = row.get(4)?;
                Ok(ScheduledTask {
                    series: row.get(0)?,
                    prompt,
                    process_after: row.get(2)?,
                    recurrence: row.get(3)?,
                    paused: status == "paused",
                })
            })?
            .collect()
        })
    }

    /// Cancels a series: its active occurrences (and any recurrence) are removed.
    /// Returns how many rows were dropped.
    pub fn cancel_task(&self, series: &str) -> Result<usize, SessionStoreError> {
        self.with(|conn| {
            conn.execute(
                "DELETE FROM messages_in
                 WHERE series_id = ?1 AND status IN ('pending', 'paused')",
                params![series],
            )
        })
    }

    /// Pauses (`pending` → `paused`) or resumes (`paused` → `pending`) a series.
    pub fn set_task_paused(&self, series: &str, paused: bool) -> Result<usize, SessionStoreError> {
        let (to, from) = if paused {
            ("paused", "pending")
        } else {
            ("pending", "paused")
        };
        self.with(|conn| {
            conn.execute(
                "UPDATE messages_in SET status = ?2
                 WHERE series_id = ?1 AND status = ?3",
                params![series, to, from],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::message::MessageStatus;
    use crate::session::test_session_db;

    #[test]
    fn schedule_list_and_lifecycle_round_trip() {
        let (_tmp, db) = test_session_db();
        let series = db
            .schedule_task(
                "daily briefing",
                Some("2999-01-01T00:00:00.000Z"),
                Some("0 9 * * *"),
            )
            .expect("schedule");

        let tasks = db.list_scheduled_tasks().expect("list");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].series, series);
        assert_eq!(tasks[0].prompt, "daily briefing");
        assert_eq!(tasks[0].recurrence.as_deref(), Some("0 9 * * *"));
        assert!(!tasks[0].paused);

        assert_eq!(db.set_task_paused(&series, true).expect("pause"), 1);
        assert!(db.list_scheduled_tasks().expect("list")[0].paused);

        assert_eq!(db.set_task_paused(&series, false).expect("resume"), 1);
        assert!(!db.list_scheduled_tasks().expect("list")[0].paused);

        assert_eq!(db.cancel_task(&series).expect("cancel"), 1);
        assert!(db.list_scheduled_tasks().expect("list").is_empty());
    }

    #[test]
    fn paused_recurring_task_is_not_advanced() {
        let (_tmp, db) = test_session_db();
        let series = db
            .schedule_task("ping", None, Some("*/5 * * * *"))
            .expect("schedule");
        db.set_task_paused(&series, true).expect("pause");

        // Even if it were somehow completed, only completed (not paused) rows advance.
        assert!(db.completed_recurrences().expect("query").is_empty());
        // And a paused task is not due for processing.
        assert_eq!(
            db.count_pending_triggers("2999-12-31T00:00:00.000Z")
                .expect("count"),
            0
        );
    }

    #[test]
    fn completed_one_shot_task_does_not_appear_as_scheduled() {
        let (_tmp, db) = test_session_db();
        let series = db.schedule_task("once", None, None).expect("schedule");
        let written = db.list_scheduled_tasks().expect("list");
        assert_eq!(written.len(), 1);

        // Mark the underlying row completed; it should drop off the scheduled list.
        let due = db
            .pending_due("2999-12-31T00:00:00.000Z", 10)
            .expect("pending");
        let id = due
            .iter()
            .find(|m| m.series_id.as_deref() == Some(series.as_str()))
            .map(|m| m.id.clone())
            .expect("task row");
        db.mark_status(&[id], MessageStatus::Completed)
            .expect("complete");
        assert!(db.list_scheduled_tasks().expect("list").is_empty());
    }
}
