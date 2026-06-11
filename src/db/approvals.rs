use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Map, Value};

use crate::protocol::content::Routing;
use crate::protocol::ids::{AgentGroupId, SessionId};

/// A privileged agent command held for owner approval: everything needed to run
/// it on allow, plus the originating session/routing to report the result back.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub approval_id: String,
    pub session_id: SessionId,
    pub agent_group_id: AgentGroupId,
    pub command: String,
    pub args: Map<String, Value>,
    pub routing: Routing,
    pub summary: String,
    pub created_at: String,
}

/// The fields needed to hold a command for approval (everything but `created_at`).
pub struct NewApproval<'a> {
    pub approval_id: &'a str,
    pub session_id: &'a SessionId,
    pub agent_group_id: &'a AgentGroupId,
    pub command: &'a str,
    pub args: &'a Map<String, Value>,
    pub routing: &'a Routing,
    pub summary: &'a str,
}

pub fn insert(conn: &Connection, new: &NewApproval<'_>) -> Result<(), rusqlite::Error> {
    let args_json = serde_json::to_string(new.args)
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
    conn.execute(
        "INSERT OR IGNORE INTO pending_approvals
           (approval_id, session_id, agent_group_id, command, args_json,
            platform_id, channel_type, thread_id, summary, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![
            new.approval_id,
            new.session_id,
            new.agent_group_id,
            new.command,
            args_json,
            new.routing.platform_id,
            new.routing.channel_type,
            new.routing.thread_id,
            new.summary,
        ],
    )?;
    Ok(())
}

pub fn take(
    conn: &Connection,
    approval_id: &str,
) -> Result<Option<PendingApproval>, rusqlite::Error> {
    let approval = conn
        .query_row(
            &format!("{SELECT} WHERE approval_id = ?1"),
            params![approval_id],
            from_row,
        )
        .optional()?;
    if approval.is_some() {
        conn.execute(
            "DELETE FROM pending_approvals WHERE approval_id = ?1",
            params![approval_id],
        )?;
    }
    Ok(approval)
}

const SELECT: &str = "SELECT approval_id, session_id, agent_group_id, command, args_json,
        platform_id, channel_type, thread_id, summary, created_at FROM pending_approvals";

fn from_row(row: &Row<'_>) -> Result<PendingApproval, rusqlite::Error> {
    let args_json: String = row.get(4)?;
    let args = serde_json::from_str(&args_json).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(PendingApproval {
        approval_id: row.get(0)?,
        session_id: row.get(1)?,
        agent_group_id: row.get(2)?,
        command: row.get(3)?,
        args,
        routing: Routing {
            platform_id: row.get(5)?,
            channel_type: row.get(6)?,
            thread_id: row.get(7)?,
        },
        summary: row.get(8)?,
        created_at: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, agent_groups, sessions};

    #[test]
    fn insert_then_take_consumes_the_approval() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let group = agent_groups::create(conn, "Andy", "andy")?;
            let session = sessions::create(conn, &group.id, None, None)?;
            let mut args = Map::new();
            args.insert("name".to_owned(), Value::String("openrouter".to_owned()));
            let routing = Routing {
                channel_type: Some("web".to_owned()),
                platform_id: Some("chat-1".to_owned()),
                thread_id: None,
            };
            insert(
                conn,
                &NewApproval {
                    approval_id: "ap-1",
                    session_id: &session.id,
                    agent_group_id: &group.id,
                    command: "endpoints-delete",
                    args: &args,
                    routing: &routing,
                    summary: "delete endpoint openrouter",
                },
            )?;

            let taken = take(conn, "ap-1")?.expect("must exist");
            assert_eq!(taken.command, "endpoints-delete");
            assert_eq!(taken.args, args);
            assert_eq!(taken.agent_group_id, group.id);
            assert_eq!(taken.routing.platform_id.as_deref(), Some("chat-1"));
            assert_eq!(take(conn, "ap-1")?, None);
            Ok(())
        })
        .expect("db ops");
    }
}
