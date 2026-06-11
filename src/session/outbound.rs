use rusqlite::{Row, params};

use crate::db::generate_id;
use crate::protocol::content::Routing;
use crate::protocol::ids::{MessageInId, MessageOutId};
use crate::protocol::message::{MessageKind, Seq};

use super::inbound::highest_seq;
use super::{SessionDb, SessionStoreError};

#[derive(Debug, Clone, PartialEq)]
pub struct NewOutboundMessage {
    pub kind: MessageKind,
    pub content: String,
    pub routing: Routing,
    pub in_reply_to: Option<MessageInId>,
    pub deliver_after: Option<String>,
    pub recurrence: Option<String>,
}

impl NewOutboundMessage {
    #[must_use]
    pub fn chat(content: String, routing: Routing) -> Self {
        Self {
            kind: MessageKind::Chat,
            content,
            routing,
            in_reply_to: None,
            deliver_after: None,
            recurrence: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboundMessage {
    pub id: MessageOutId,
    pub seq: Seq,
    pub in_reply_to: Option<MessageInId>,
    pub timestamp: String,
    pub deliver_after: Option<String>,
    pub recurrence: Option<String>,
    pub kind: String,
    pub routing: Routing,
    pub content: String,
}

impl SessionDb {
    /// Agent-side write: assigns the next ODD seq. The pi tool extension mirrors this statement.
    pub fn write_outbound(
        &self,
        message: &NewOutboundMessage,
    ) -> Result<OutboundMessage, SessionStoreError> {
        self.with(|conn| {
            let seq = Seq::next_agent_after(highest_seq(conn)?);
            let id = MessageOutId::new(generate_id("out"));
            conn.execute(
                "INSERT INTO messages_out
                   (id, seq, in_reply_to, timestamp, deliver_after, recurrence, kind,
                    platform_id, channel_type, thread_id, content)
                 VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?4, ?5, ?6,
                    ?7, ?8, ?9, ?10)",
                params![
                    id,
                    seq,
                    message.in_reply_to,
                    message.deliver_after,
                    message.recurrence,
                    message.kind,
                    message.routing.platform_id,
                    message.routing.channel_type,
                    message.routing.thread_id,
                    message.content,
                ],
            )?;
            conn.query_row(
                &format!("{SELECT_OUTBOUND} WHERE id = ?1"),
                params![id],
                from_row,
            )
        })
    }

    /// Undelivered messages whose `deliver_after` is unset or has passed, oldest first.
    pub fn due_outbound(&self, now: &str) -> Result<Vec<OutboundMessage>, SessionStoreError> {
        self.with(|conn| {
            conn.prepare(&format!(
                "{SELECT_OUTBOUND}
                 WHERE id NOT IN (SELECT message_out_id FROM delivered)
                   AND (deliver_after IS NULL OR deliver_after <= ?1)
                 ORDER BY seq"
            ))?
            .query_map(params![now], from_row)?
            .collect()
        })
    }

    pub fn mark_delivered(
        &self,
        id: &MessageOutId,
        platform_message_id: Option<&str>,
    ) -> Result<(), SessionStoreError> {
        self.record_delivery(id, platform_message_id, "delivered")
    }

    pub fn mark_delivery_failed(&self, id: &MessageOutId) -> Result<(), SessionStoreError> {
        self.record_delivery(id, None, "failed")
    }

    pub fn platform_message_id_for(&self, seq: Seq) -> Result<Option<String>, SessionStoreError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT d.platform_message_id FROM delivered d
                 JOIN messages_out m ON m.id = d.message_out_id
                 WHERE m.seq = ?1",
                params![seq],
                |row| row.get(0),
            )
            .map_or_else(
                |err| match err {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                },
                Ok,
            )
        })
    }

    fn record_delivery(
        &self,
        id: &MessageOutId,
        platform_message_id: Option<&str>,
        status: &str,
    ) -> Result<(), SessionStoreError> {
        self.with(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO delivered
                   (message_out_id, platform_message_id, status, delivered_at)
                 VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                params![id, platform_message_id, status],
            )?;
            Ok(())
        })
    }
}

const SELECT_OUTBOUND: &str = "SELECT id, seq, in_reply_to, timestamp, deliver_after, recurrence,
        kind, platform_id, channel_type, thread_id, content FROM messages_out";

fn from_row(row: &Row<'_>) -> Result<OutboundMessage, rusqlite::Error> {
    Ok(OutboundMessage {
        id: row.get(0)?,
        seq: row.get(1)?,
        in_reply_to: row.get(2)?,
        timestamp: row.get(3)?,
        deliver_after: row.get(4)?,
        recurrence: row.get(5)?,
        kind: row.get(6)?,
        routing: Routing {
            platform_id: row.get(7)?,
            channel_type: row.get(8)?,
            thread_id: row.get(9)?,
        },
        content: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::inbound::NewInboundMessage;
    use crate::session::test_session_db;

    const NOW: &str = "2026-06-11T12:00:00Z";

    fn reply(text: &str) -> NewOutboundMessage {
        NewOutboundMessage::chat(format!("{{\"text\":\"{text}\"}}"), Routing::default())
    }

    #[test]
    fn outbound_seqs_are_odd_and_interleave_with_inbound() {
        let (_tmp, db) = test_session_db();
        let inbound = db
            .write_message(&NewInboundMessage::chat(
                "{}".to_owned(),
                Routing::default(),
            ))
            .expect("write in");
        assert_eq!(inbound.seq, Seq::new(0));

        let first_out = db.write_outbound(&reply("a")).expect("write out");
        assert_eq!(first_out.seq, Seq::new(1));
        assert!(first_out.seq.is_agent_assigned());

        let second_in = db
            .write_message(&NewInboundMessage::chat(
                "{}".to_owned(),
                Routing::default(),
            ))
            .expect("write in");
        assert_eq!(second_in.seq, Seq::new(2));

        let second_out = db.write_outbound(&reply("b")).expect("write out");
        assert_eq!(second_out.seq, Seq::new(3));
    }

    #[test]
    fn due_outbound_excludes_delivered_and_future_messages() {
        let (_tmp, db) = test_session_db();
        let now_msg = db.write_outbound(&reply("now")).expect("write");
        let mut later = reply("later");
        later.deliver_after = Some("2999-01-01T00:00:00Z".to_owned());
        db.write_outbound(&later).expect("write");

        let due = db.due_outbound(NOW).expect("query");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, now_msg.id);

        db.mark_delivered(&now_msg.id, Some("platform-7"))
            .expect("mark");
        assert!(db.due_outbound(NOW).expect("query").is_empty());
        assert_eq!(
            db.platform_message_id_for(now_msg.seq).expect("lookup"),
            Some("platform-7".to_owned())
        );
    }

    #[test]
    fn failed_deliveries_leave_the_due_queue() {
        let (_tmp, db) = test_session_db();
        let msg = db.write_outbound(&reply("x")).expect("write");
        db.mark_delivery_failed(&msg.id).expect("mark");
        assert!(db.due_outbound(NOW).expect("query").is_empty());
        assert_eq!(db.platform_message_id_for(msg.seq).expect("lookup"), None);
    }
}
