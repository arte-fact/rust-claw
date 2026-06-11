use rusqlite::{Connection, Row, params};

use crate::db::generate_id;
use crate::protocol::content::Routing;
use crate::protocol::ids::MessageInId;
use crate::protocol::message::{MessageKind, MessageStatus, Seq};

use super::{SessionDb, SessionStoreError};

#[derive(Debug, Clone, PartialEq)]
pub struct NewInboundMessage {
    pub kind: MessageKind,
    pub content: String,
    pub routing: Routing,
    pub trigger: bool,
    pub process_after: Option<String>,
    pub recurrence: Option<String>,
    pub series_id: Option<String>,
    pub source_session_id: Option<String>,
}

impl NewInboundMessage {
    #[must_use]
    pub fn chat(content: String, routing: Routing) -> Self {
        Self {
            kind: MessageKind::Chat,
            content,
            routing,
            trigger: true,
            process_after: None,
            recurrence: None,
            series_id: None,
            source_session_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InboundMessage {
    pub id: MessageInId,
    pub seq: Seq,
    pub kind: String,
    pub timestamp: String,
    pub status: MessageStatus,
    pub process_after: Option<String>,
    pub recurrence: Option<String>,
    pub series_id: Option<String>,
    pub tries: i64,
    pub trigger: bool,
    pub routing: Routing,
    pub content: String,
    pub source_session_id: Option<String>,
}

impl SessionDb {
    pub fn write_message(
        &self,
        message: &NewInboundMessage,
    ) -> Result<InboundMessage, SessionStoreError> {
        self.with(|conn| {
            let seq = Seq::next_claw_after(highest_seq(conn)?);
            let id = MessageInId::new(generate_id("in"));
            conn.execute(
                "INSERT INTO messages_in
                   (id, seq, kind, timestamp, process_after, recurrence, series_id, trigger,
                    platform_id, channel_type, thread_id, content, source_session_id)
                 VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?4, ?5, ?6, ?7,
                    ?8, ?9, ?10, ?11, ?12)",
                params![
                    id,
                    seq,
                    message.kind,
                    message.process_after,
                    message.recurrence,
                    message.series_id,
                    message.trigger,
                    message.routing.platform_id,
                    message.routing.channel_type,
                    message.routing.thread_id,
                    message.content,
                    message.source_session_id,
                ],
            )?;
            conn.query_row(
                &format!("{SELECT_INBOUND} WHERE id = ?1"),
                params![id],
                from_row,
            )
        })
    }

    /// Pending messages whose `process_after` is unset or has passed, oldest first.
    pub fn pending_due(
        &self,
        now: &str,
        limit: i64,
    ) -> Result<Vec<InboundMessage>, SessionStoreError> {
        self.with(|conn| {
            conn.prepare(&format!(
                "{SELECT_INBOUND}
                 WHERE status = 'pending' AND (process_after IS NULL OR process_after <= ?1)
                 ORDER BY seq LIMIT ?2"
            ))?
            .query_map(params![now, limit], from_row)?
            .collect()
        })
    }

    pub fn mark_status(
        &self,
        ids: &[MessageInId],
        status: MessageStatus,
    ) -> Result<(), SessionStoreError> {
        self.with(|conn| {
            for id in ids {
                conn.execute(
                    "UPDATE messages_in SET status = ?2 WHERE id = ?1",
                    params![id, status],
                )?;
            }
            Ok(())
        })
    }

    pub fn count_pending_triggers(&self, now: &str) -> Result<i64, SessionStoreError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT count(*) FROM messages_in
                 WHERE status = 'pending' AND trigger = 1
                   AND (process_after IS NULL OR process_after <= ?1)",
                params![now],
                |row| row.get(0),
            )
        })
    }
}

pub(super) fn highest_seq(conn: &Connection) -> Result<Option<Seq>, rusqlite::Error> {
    conn.query_row(
        "SELECT MAX(s) FROM (
           SELECT MAX(seq) AS s FROM messages_in
           UNION ALL
           SELECT MAX(seq) AS s FROM messages_out
         )",
        [],
        |row| row.get::<_, Option<Seq>>(0),
    )
}

const SELECT_INBOUND: &str = "SELECT id, seq, kind, timestamp, status, process_after, recurrence,
        series_id, tries, trigger, platform_id, channel_type, thread_id, content,
        source_session_id FROM messages_in";

fn from_row(row: &Row<'_>) -> Result<InboundMessage, rusqlite::Error> {
    Ok(InboundMessage {
        id: row.get(0)?,
        seq: row.get(1)?,
        kind: row.get(2)?,
        timestamp: row.get(3)?,
        status: row.get(4)?,
        process_after: row.get(5)?,
        recurrence: row.get(6)?,
        series_id: row.get(7)?,
        tries: row.get(8)?,
        trigger: row.get(9)?,
        routing: Routing {
            platform_id: row.get(10)?,
            channel_type: row.get(11)?,
            thread_id: row.get(12)?,
        },
        content: row.get(13)?,
        source_session_id: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_session_db;

    const FAR_FUTURE: &str = "2999-01-01T00:00:00Z";
    const NOW: &str = "2026-06-11T12:00:00Z";

    fn chat(text: &str) -> NewInboundMessage {
        NewInboundMessage::chat(format!("{{\"text\":\"{text}\"}}"), Routing::default())
    }

    #[test]
    fn written_messages_get_even_increasing_seqs() {
        let (_tmp, db) = test_session_db();
        let first = db.write_message(&chat("a")).expect("write");
        let second = db.write_message(&chat("b")).expect("write");
        assert_eq!(first.seq, Seq::new(0));
        assert_eq!(second.seq, Seq::new(2));
        assert_eq!(first.status, MessageStatus::Pending);
        assert!(first.trigger);
    }

    #[test]
    fn pending_due_respects_process_after_and_order() {
        let (_tmp, db) = test_session_db();
        let due_now = db.write_message(&chat("now")).expect("write");
        let mut scheduled = chat("later");
        scheduled.process_after = Some(FAR_FUTURE.to_owned());
        db.write_message(&scheduled).expect("write");

        let due = db.pending_due(NOW, 10).expect("query");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, due_now.id);

        let all = db.pending_due("2999-12-31T00:00:00Z", 10).expect("query");
        assert_eq!(all.len(), 2);
        assert!(all[0].seq < all[1].seq);
    }

    #[test]
    fn mark_status_moves_messages_out_of_pending() {
        let (_tmp, db) = test_session_db();
        let msg = db.write_message(&chat("a")).expect("write");
        db.mark_status(std::slice::from_ref(&msg.id), MessageStatus::Processing)
            .expect("mark");
        assert!(db.pending_due(NOW, 10).expect("query").is_empty());
        db.mark_status(&[msg.id], MessageStatus::Completed)
            .expect("mark");
        assert!(db.pending_due(NOW, 10).expect("query").is_empty());
    }

    #[test]
    fn accumulated_context_does_not_count_as_trigger() {
        let (_tmp, db) = test_session_db();
        let mut context_only = chat("fyi");
        context_only.trigger = false;
        db.write_message(&context_only).expect("write");
        assert_eq!(db.count_pending_triggers(NOW).expect("count"), 0);
        db.write_message(&chat("hey")).expect("write");
        assert_eq!(db.count_pending_triggers(NOW).expect("count"), 1);
    }
}
