use rusqlite::{Connection, Row, params};

use crate::protocol::ids::{MessageOutId, MessagingGroupId};
use crate::protocol::macros::text_enum;

text_enum!(Direction {
    In => "in",
    Out => "out",
});

text_enum!(MessageRowKind {
    Chat => "chat",
    Question => "question",
    Approval => "approval",
    Error => "error",
});

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WebMessage {
    pub id: i64,
    pub messaging_group_id: MessagingGroupId,
    pub direction: Direction,
    pub sender: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_out_id: Option<MessageOutId>,
    pub created_at: String,
    pub kind: MessageRowKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

pub fn append(
    conn: &Connection,
    messaging_group_id: &MessagingGroupId,
    direction: Direction,
    sender: &str,
    body: &str,
    message_out_id: Option<&MessageOutId>,
) -> Result<WebMessage, rusqlite::Error> {
    conn.execute(
        "INSERT INTO web_messages
           (messaging_group_id, direction, sender, body, message_out_id, created_at, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'chat')",
        params![messaging_group_id, direction, sender, body, message_out_id],
    )?;
    fetch(conn, conn.last_insert_rowid())
}

/// Appends a presentation-only error notice (M14). Lives solely in the web
/// transcript ledger — never written to the session DB, so agent context is
/// untouched. `direction='out'`/`sender='system'` mark it as a non-user notice.
/// Returns `None` when the last row is already an identical error, so a session
/// that keeps failing (e.g. a misconfigured agent the sweep retries) collapses to
/// one card instead of spamming the transcript.
pub fn append_error(
    conn: &Connection,
    messaging_group_id: &MessagingGroupId,
    detail: &str,
) -> Result<Option<WebMessage>, rusqlite::Error> {
    if let Some(last) = list(conn, messaging_group_id, 1)?.pop()
        && last.kind == MessageRowKind::Error
        && last.body == detail
    {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO web_messages
           (messaging_group_id, direction, sender, body, created_at, kind)
         VALUES (?1, 'out', 'system', ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'error')",
        params![messaging_group_id, detail],
    )?;
    fetch(conn, conn.last_insert_rowid()).map(Some)
}

/// Appends an open question card; `answer` stays NULL until the user chooses.
pub fn append_question(
    conn: &Connection,
    messaging_group_id: &MessagingGroupId,
    sender: &str,
    question: &str,
    question_id: &str,
    options: &[String],
) -> Result<WebMessage, rusqlite::Error> {
    let options_json = serde_json::to_string(options)
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
    conn.execute(
        "INSERT INTO web_messages
           (messaging_group_id, direction, sender, body, created_at, kind, question_id, options_json)
         VALUES (?1, 'out', ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'question', ?4, ?5)",
        params![messaging_group_id, sender, question, question_id, options_json],
    )?;
    fetch(conn, conn.last_insert_rowid())
}

/// Appends an open approval card. It reuses the question columns (`question_id`
/// holds the approval id, `options` are Allow/Deny) so it renders and collapses
/// through the same path; the buttons target the approvals endpoint (§9, M7.2).
pub fn append_approval(
    conn: &Connection,
    messaging_group_id: &MessagingGroupId,
    sender: &str,
    summary: &str,
    approval_id: &str,
) -> Result<WebMessage, rusqlite::Error> {
    let options = ["Allow".to_owned(), "Deny".to_owned()];
    let options_json = serde_json::to_string(&options)
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
    conn.execute(
        "INSERT INTO web_messages
           (messaging_group_id, direction, sender, body, created_at, kind, question_id, options_json)
         VALUES (?1, 'out', ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'approval', ?4, ?5)",
        params![messaging_group_id, sender, summary, approval_id, options_json],
    )?;
    fetch(conn, conn.last_insert_rowid())
}

/// Collapses an open question card by recording its `answer` (a chosen option or
/// an expiry note). Returns the updated row, or `None` if no *open* card matches.
pub fn resolve_question(
    conn: &Connection,
    question_id: &str,
    answer: &str,
) -> Result<Option<WebMessage>, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE web_messages SET answer = ?2 WHERE question_id = ?1 AND answer IS NULL",
        params![question_id, answer],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    conn.query_row(
        &format!("{SELECT_MESSAGE} WHERE question_id = ?1"),
        params![question_id],
        from_row,
    )
    .map(Some)
}

fn fetch(conn: &Connection, id: i64) -> Result<WebMessage, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_MESSAGE} WHERE id = ?1"),
        params![id],
        from_row,
    )
}

pub fn list(
    conn: &Connection,
    messaging_group_id: &MessagingGroupId,
    limit: i64,
) -> Result<Vec<WebMessage>, rusqlite::Error> {
    let mut newest_first: Vec<WebMessage> = conn
        .prepare(&format!(
            "{SELECT_MESSAGE} WHERE messaging_group_id = ?1 ORDER BY id DESC LIMIT ?2"
        ))?
        .query_map(params![messaging_group_id, limit], from_row)?
        .collect::<Result<_, _>>()?;
    newest_first.reverse();
    Ok(newest_first)
}

const SELECT_MESSAGE: &str = "SELECT id, messaging_group_id, direction, sender, body,
        message_out_id, created_at, kind, question_id, options_json, answer FROM web_messages";

fn from_row(row: &Row<'_>) -> Result<WebMessage, rusqlite::Error> {
    let options_json: Option<String> = row.get(9)?;
    let options = match options_json {
        Some(json) => serde_json::from_str(&json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(err))
        })?,
        None => Vec::new(),
    };
    Ok(WebMessage {
        id: row.get(0)?,
        messaging_group_id: row.get(1)?,
        direction: row.get(2)?,
        sender: row.get(3)?,
        body: row.get(4)?,
        message_out_id: row.get(5)?,
        created_at: row.get(6)?,
        kind: row.get(7)?,
        question_id: row.get(8)?,
        options,
        answer: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, messaging_groups};

    #[test]
    fn append_and_list_keep_chronological_order_with_limit() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let chat = messaging_groups::create(conn, "web", "chat-1", None, false)?;
            append(conn, &chat.id, Direction::In, "you", "first", None)?;
            append(conn, &chat.id, Direction::Out, "andy", "second", None)?;
            append(conn, &chat.id, Direction::In, "you", "third", None)?;

            let all = list(conn, &chat.id, 10)?;
            assert_eq!(
                all.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
                vec!["first", "second", "third"]
            );

            let last_two = list(conn, &chat.id, 2)?;
            assert_eq!(
                last_two.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
                vec!["second", "third"]
            );
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn error_card_round_trips_in_the_web_transcript() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let chat = messaging_groups::create(conn, "web", "chat-1", None, false)?;
            let card = append_error(conn, &chat.id, "no endpoint configured")?
                .expect("first error inserted");
            assert_eq!(card.kind, MessageRowKind::Error);
            assert_eq!(card.sender, "system");
            assert_eq!(card.body, "no endpoint configured");

            // A consecutive identical error collapses (no duplicate card).
            assert!(append_error(conn, &chat.id, "no endpoint configured")?.is_none());
            // A different error still appends.
            assert!(append_error(conn, &chat.id, "something else")?.is_some());

            let listed = list(conn, &chat.id, 10)?;
            assert_eq!(listed.len(), 2);
            assert!(listed.iter().all(|m| m.kind == MessageRowKind::Error));
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn question_card_round_trips_and_resolves_once() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let chat = messaging_groups::create(conn, "web", "chat-1", None, false)?;
            let options = vec!["ship".to_owned(), "wait".to_owned()];
            let card = append_question(conn, &chat.id, "andy", "Deploy now?", "q-1", &options)?;
            assert_eq!(card.kind, MessageRowKind::Question);
            assert_eq!(card.question_id.as_deref(), Some("q-1"));
            assert_eq!(card.options, options);
            assert_eq!(card.answer, None);

            // It surfaces in the transcript alongside chat rows.
            let listed = list(conn, &chat.id, 10)?;
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].kind, MessageRowKind::Question);

            let resolved = resolve_question(conn, "q-1", "ship")?.expect("open card");
            assert_eq!(resolved.id, card.id);
            assert_eq!(resolved.answer.as_deref(), Some("ship"));

            // A second answer finds no open card.
            assert_eq!(resolve_question(conn, "q-1", "wait")?, None);
            Ok(())
        })
        .expect("db ops");
    }
}
