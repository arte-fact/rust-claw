use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::protocol::content::Routing;
use crate::protocol::ids::{MessageOutId, SessionId};

#[derive(Debug, Clone, PartialEq)]
pub struct PendingQuestion {
    pub question_id: String,
    pub session_id: SessionId,
    pub message_out_id: MessageOutId,
    pub routing: Routing,
    pub title: String,
    pub options: Vec<String>,
    pub created_at: String,
}

pub fn insert(
    conn: &Connection,
    question_id: &str,
    session_id: &SessionId,
    message_out_id: &MessageOutId,
    routing: &Routing,
    title: &str,
    options: &[String],
) -> Result<(), rusqlite::Error> {
    let options_json = serde_json::to_string(options)
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
    conn.execute(
        "INSERT INTO pending_questions
           (question_id, session_id, message_out_id, platform_id, channel_type, thread_id,
            title, options_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![
            question_id,
            session_id,
            message_out_id,
            routing.platform_id,
            routing.channel_type,
            routing.thread_id,
            title,
            options_json,
        ],
    )?;
    Ok(())
}

pub fn take(
    conn: &Connection,
    question_id: &str,
) -> Result<Option<PendingQuestion>, rusqlite::Error> {
    let question = conn
        .query_row(
            "SELECT question_id, session_id, message_out_id, platform_id, channel_type,
                    thread_id, title, options_json, created_at
             FROM pending_questions WHERE question_id = ?1",
            params![question_id],
            from_row,
        )
        .optional()?;
    if question.is_some() {
        conn.execute(
            "DELETE FROM pending_questions WHERE question_id = ?1",
            params![question_id],
        )?;
    }
    Ok(question)
}

fn from_row(row: &Row<'_>) -> Result<PendingQuestion, rusqlite::Error> {
    let options_json: String = row.get(7)?;
    let options = serde_json::from_str(&options_json).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(PendingQuestion {
        question_id: row.get(0)?,
        session_id: row.get(1)?,
        message_out_id: row.get(2)?,
        routing: Routing {
            platform_id: row.get(3)?,
            channel_type: row.get(4)?,
            thread_id: row.get(5)?,
        },
        title: row.get(6)?,
        options,
        created_at: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, agent_groups, sessions};

    #[test]
    fn insert_then_take_consumes_the_question() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let group = agent_groups::create(conn, "Andy", "andy")?;
            let session = sessions::create(conn, &group.id, None, None)?;
            let routing = Routing {
                channel_type: Some("web".to_owned()),
                platform_id: Some("chat-1".to_owned()),
                thread_id: None,
            };
            let options = vec!["yes".to_owned(), "no".to_owned()];
            insert(
                conn,
                "q-1",
                &session.id,
                &MessageOutId::new("out-1"),
                &routing,
                "Deploy?",
                &options,
            )?;

            let taken = take(conn, "q-1")?.expect("must exist");
            assert_eq!(taken.session_id, session.id);
            assert_eq!(taken.options, options);
            assert_eq!(taken.routing, routing);
            assert_eq!(take(conn, "q-1")?, None);
            Ok(())
        })
        .expect("db ops");
    }
}
