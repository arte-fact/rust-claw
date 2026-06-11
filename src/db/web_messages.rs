use rusqlite::{Connection, Row, params};

use crate::protocol::ids::{MessageOutId, MessagingGroupId};
use crate::protocol::macros::text_enum;

text_enum!(Direction {
    In => "in",
    Out => "out",
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
           (messaging_group_id, direction, sender, body, message_out_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![messaging_group_id, direction, sender, body, message_out_id],
    )?;
    let id = conn.last_insert_rowid();
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
        message_out_id, created_at FROM web_messages";

fn from_row(row: &Row<'_>) -> Result<WebMessage, rusqlite::Error> {
    Ok(WebMessage {
        id: row.get(0)?,
        messaging_group_id: row.get(1)?,
        direction: row.get(2)?,
        sender: row.get(3)?,
        body: row.get(4)?,
        message_out_id: row.get(5)?,
        created_at: row.get(6)?,
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
}
