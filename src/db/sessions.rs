use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::protocol::entities::SessionStatus;
use crate::protocol::ids::{AgentGroupId, MessagingGroupId, SessionId};

use super::generate_id;

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: SessionId,
    pub agent_group_id: AgentGroupId,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub thread_id: Option<String>,
    pub status: SessionStatus,
    pub last_active: Option<String>,
    pub created_at: String,
}

pub fn create(
    conn: &Connection,
    agent_group_id: &AgentGroupId,
    messaging_group_id: Option<&MessagingGroupId>,
    thread_id: Option<&str>,
) -> Result<Session, rusqlite::Error> {
    let id = SessionId::new(generate_id("sess"));
    conn.execute(
        "INSERT INTO sessions (id, agent_group_id, messaging_group_id, thread_id, created_at)
         VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![id, agent_group_id, messaging_group_id, thread_id],
    )?;
    conn.query_row(
        &format!("{SELECT_SESSION} WHERE id = ?1"),
        params![id],
        from_row,
    )
}

pub fn get(conn: &Connection, id: &SessionId) -> Result<Option<Session>, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_SESSION} WHERE id = ?1"),
        params![id],
        from_row,
    )
    .optional()
}

pub fn find_active(
    conn: &Connection,
    agent_group_id: &AgentGroupId,
    messaging_group_id: Option<&MessagingGroupId>,
    thread_id: Option<&str>,
) -> Result<Option<Session>, rusqlite::Error> {
    conn.query_row(
        &format!(
            "{SELECT_SESSION}
             WHERE agent_group_id = ?1 AND messaging_group_id IS ?2 AND thread_id IS ?3
               AND status = 'active'
             ORDER BY created_at DESC LIMIT 1"
        ),
        params![agent_group_id, messaging_group_id, thread_id],
        from_row,
    )
    .optional()
}

pub fn list_active(conn: &Connection) -> Result<Vec<Session>, rusqlite::Error> {
    conn.prepare(&format!(
        "{SELECT_SESSION} WHERE status = 'active' ORDER BY created_at, id"
    ))?
    .query_map([], from_row)?
    .collect()
}

pub fn touch_last_active(conn: &Connection, id: &SessionId) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE sessions SET last_active = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn close(conn: &Connection, id: &SessionId) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE sessions SET status = 'closed' WHERE id = ?1 AND status = 'active'",
        params![id],
    )?;
    Ok(changed == 1)
}

const SELECT_SESSION: &str = "SELECT id, agent_group_id, messaging_group_id, thread_id, status,
        last_active, created_at FROM sessions";

fn from_row(row: &Row<'_>) -> Result<Session, rusqlite::Error> {
    Ok(Session {
        id: row.get(0)?,
        agent_group_id: row.get(1)?,
        messaging_group_id: row.get(2)?,
        thread_id: row.get(3)?,
        status: row.get(4)?,
        last_active: row.get(5)?,
        created_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, agent_groups, messaging_groups};

    fn fixtures(conn: &Connection) -> Result<(AgentGroupId, MessagingGroupId), rusqlite::Error> {
        let ag = agent_groups::create(conn, "Andy", "andy")?;
        let mg = messaging_groups::create(conn, "web", "chat-1", None, false)?;
        Ok((ag.id, mg.id))
    }

    #[test]
    fn create_and_find_active_matches_null_thread_exactly() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let (ag, mg) = fixtures(conn)?;
            let shared = create(conn, &ag, Some(&mg), None)?;
            assert_eq!(shared.status, SessionStatus::Active);

            let found = find_active(conn, &ag, Some(&mg), None)?.expect("must exist");
            assert_eq!(found, shared);
            assert_eq!(find_active(conn, &ag, Some(&mg), Some("t1"))?, None);

            let threaded = create(conn, &ag, Some(&mg), Some("t1"))?;
            let found_threaded =
                find_active(conn, &ag, Some(&mg), Some("t1"))?.expect("must exist");
            assert_eq!(found_threaded, threaded);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn closed_sessions_are_not_found_and_a_new_one_can_replace_them() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let (ag, mg) = fixtures(conn)?;
            let first = create(conn, &ag, Some(&mg), None)?;
            assert!(close(conn, &first.id)?);
            assert!(!close(conn, &first.id)?);
            assert_eq!(find_active(conn, &ag, Some(&mg), None)?, None);

            let replacement = create(conn, &ag, Some(&mg), None)?;
            let found = find_active(conn, &ag, Some(&mg), None)?.expect("must exist");
            assert_eq!(found, replacement);
            assert_eq!(list_active(conn)?.len(), 1);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn touch_updates_last_active() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let (ag, mg) = fixtures(conn)?;
            let session = create(conn, &ag, Some(&mg), None)?;
            assert_eq!(session.last_active, None);
            touch_last_active(conn, &session.id)?;
            let touched = get(conn, &session.id)?.expect("must exist");
            assert!(touched.last_active.is_some());
            Ok(())
        })
        .expect("db ops");
    }
}
