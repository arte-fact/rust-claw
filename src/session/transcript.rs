use rusqlite::params;

use crate::protocol::message::Seq;

use super::{SessionDb, SessionStoreError};

/// One conversation entry in global seq order, from either message table.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptEntry {
    pub seq: Seq,
    pub inbound: bool,
    pub kind: String,
    pub content: String,
}

impl SessionDb {
    /// The newest `limit` messages across both tables, oldest first.
    pub fn transcript(&self, limit: i64) -> Result<Vec<TranscriptEntry>, SessionStoreError> {
        self.with(|conn| {
            let mut newest_first: Vec<TranscriptEntry> = conn
                .prepare(
                    "SELECT seq, inbound, kind, content FROM (
                       SELECT seq, 1 AS inbound, kind, content FROM messages_in
                       UNION ALL
                       SELECT seq, 0 AS inbound, kind, content FROM messages_out
                     )
                     ORDER BY seq DESC LIMIT ?1",
                )?
                .query_map(params![limit], |row| {
                    Ok(TranscriptEntry {
                        seq: row.get(0)?,
                        inbound: row.get(1)?,
                        kind: row.get(2)?,
                        content: row.get(3)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            newest_first.reverse();
            Ok(newest_first)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::content::Routing;
    use crate::session::test_session_db;
    use crate::session::{NewInboundMessage, NewOutboundMessage};

    #[test]
    fn transcript_interleaves_both_tables_in_seq_order() {
        let (_tmp, db) = test_session_db();
        db.write_message(&NewInboundMessage::chat(
            "\"q1\"".to_owned(),
            Routing::default(),
        ))
        .expect("in");
        db.write_outbound(&NewOutboundMessage::chat(
            "\"a1\"".to_owned(),
            Routing::default(),
        ))
        .expect("out");
        db.write_message(&NewInboundMessage::chat(
            "\"q2\"".to_owned(),
            Routing::default(),
        ))
        .expect("in");

        let transcript = db.transcript(10).expect("transcript");
        let shape: Vec<(bool, &str)> = transcript
            .iter()
            .map(|entry| (entry.inbound, entry.content.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![(true, "\"q1\""), (false, "\"a1\""), (true, "\"q2\"")]
        );
        assert!(transcript.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    }

    #[test]
    fn limit_keeps_the_newest_entries() {
        let (_tmp, db) = test_session_db();
        for index in 0..5 {
            db.write_message(&NewInboundMessage::chat(
                format!("\"m{index}\""),
                Routing::default(),
            ))
            .expect("in");
        }
        let transcript = db.transcript(2).expect("transcript");
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].content, "\"m3\"");
        assert_eq!(transcript[1].content, "\"m4\"");
    }
}
