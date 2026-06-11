use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::protocol::entities::{
    EngageMode, IgnoredMessagePolicy, SenderScope, SessionMode, UnknownSenderPolicy,
};
use crate::protocol::ids::{AgentGroupId, MessagingGroupId};

use super::generate_id;

#[derive(Debug, Clone, PartialEq)]
pub struct MessagingGroup {
    pub id: MessagingGroupId,
    pub channel_type: String,
    pub platform_id: String,
    pub name: Option<String>,
    pub is_group: bool,
    pub unknown_sender_policy: UnknownSenderPolicy,
    pub denied_at: Option<String>,
    pub archived_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Wiring {
    pub id: String,
    pub messaging_group_id: MessagingGroupId,
    pub agent_group_id: AgentGroupId,
    pub engage_mode: EngageMode,
    pub engage_pattern: Option<String>,
    pub sender_scope: SenderScope,
    pub ignored_message_policy: IgnoredMessagePolicy,
    pub session_mode: SessionMode,
    pub priority: i64,
    pub created_at: String,
}

pub fn create(
    conn: &Connection,
    channel_type: &str,
    platform_id: &str,
    name: Option<&str>,
    is_group: bool,
) -> Result<MessagingGroup, rusqlite::Error> {
    let id = MessagingGroupId::new(generate_id("mg"));
    conn.execute(
        "INSERT INTO messaging_groups (id, channel_type, platform_id, name, is_group, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![id, channel_type, platform_id, name, is_group],
    )?;
    conn.query_row(
        &format!("{SELECT_GROUP} WHERE id = ?1"),
        params![id],
        group_from_row,
    )
}

pub fn get(
    conn: &Connection,
    id: &MessagingGroupId,
) -> Result<Option<MessagingGroup>, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_GROUP} WHERE id = ?1"),
        params![id],
        group_from_row,
    )
    .optional()
}

pub fn get_by_platform(
    conn: &Connection,
    channel_type: &str,
    platform_id: &str,
) -> Result<Option<MessagingGroup>, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_GROUP} WHERE channel_type = ?1 AND platform_id = ?2"),
        params![channel_type, platform_id],
        group_from_row,
    )
    .optional()
}

pub fn list(conn: &Connection) -> Result<Vec<MessagingGroup>, rusqlite::Error> {
    conn.prepare(&format!("{SELECT_GROUP} ORDER BY created_at, id"))?
        .query_map([], group_from_row)?
        .collect()
}

pub fn update(conn: &Connection, group: &MessagingGroup) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE messaging_groups
         SET name = ?2, is_group = ?3, unknown_sender_policy = ?4, denied_at = ?5
         WHERE id = ?1",
        params![
            group.id,
            group.name,
            group.is_group,
            group.unknown_sender_policy,
            group.denied_at,
        ],
    )?;
    Ok(changed == 1)
}

pub fn delete(conn: &Connection, id: &MessagingGroupId) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute("DELETE FROM messaging_groups WHERE id = ?1", params![id])?;
    Ok(changed == 1)
}

pub fn wire(
    conn: &Connection,
    messaging_group_id: &MessagingGroupId,
    agent_group_id: &AgentGroupId,
) -> Result<Wiring, rusqlite::Error> {
    let id = generate_id("wire");
    conn.execute(
        "INSERT INTO messaging_group_agents (id, messaging_group_id, agent_group_id, created_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![id, messaging_group_id, agent_group_id],
    )?;
    conn.query_row(
        &format!("{SELECT_WIRING} WHERE id = ?1"),
        params![id],
        wiring_from_row,
    )
}

pub fn update_wiring(conn: &Connection, wiring: &Wiring) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE messaging_group_agents
         SET engage_mode = ?2, engage_pattern = ?3, sender_scope = ?4,
             ignored_message_policy = ?5, session_mode = ?6, priority = ?7
         WHERE id = ?1",
        params![
            wiring.id,
            wiring.engage_mode,
            wiring.engage_pattern,
            wiring.sender_scope,
            wiring.ignored_message_policy,
            wiring.session_mode,
            wiring.priority,
        ],
    )?;
    Ok(changed == 1)
}

pub fn wirings_for(
    conn: &Connection,
    messaging_group_id: &MessagingGroupId,
) -> Result<Vec<Wiring>, rusqlite::Error> {
    conn.prepare(&format!(
        "{SELECT_WIRING} WHERE messaging_group_id = ?1 ORDER BY priority DESC, created_at"
    ))?
    .query_map(params![messaging_group_id], wiring_from_row)?
    .collect()
}

pub fn unwire(conn: &Connection, wiring_id: &str) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "DELETE FROM messaging_group_agents WHERE id = ?1",
        params![wiring_id],
    )?;
    Ok(changed == 1)
}

const SELECT_GROUP: &str = "SELECT id, channel_type, platform_id, name, is_group,
        unknown_sender_policy, denied_at, archived_at, created_at FROM messaging_groups";

const SELECT_WIRING: &str = "SELECT id, messaging_group_id, agent_group_id, engage_mode,
        engage_pattern, sender_scope, ignored_message_policy, session_mode, priority, created_at
        FROM messaging_group_agents";

fn group_from_row(row: &Row<'_>) -> Result<MessagingGroup, rusqlite::Error> {
    Ok(MessagingGroup {
        id: row.get(0)?,
        channel_type: row.get(1)?,
        platform_id: row.get(2)?,
        name: row.get(3)?,
        is_group: row.get(4)?,
        unknown_sender_policy: row.get(5)?,
        denied_at: row.get(6)?,
        archived_at: row.get(7)?,
        created_at: row.get(8)?,
    })
}

/// Archives or restores a chat. Returns whether a row changed.
pub fn set_archived(
    conn: &Connection,
    id: &MessagingGroupId,
    archived: bool,
) -> Result<bool, rusqlite::Error> {
    let changed = if archived {
        conn.execute(
            "UPDATE messaging_groups SET archived_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1 AND archived_at IS NULL",
            params![id],
        )?
    } else {
        conn.execute(
            "UPDATE messaging_groups SET archived_at = NULL WHERE id = ?1",
            params![id],
        )?
    };
    Ok(changed > 0)
}

fn wiring_from_row(row: &Row<'_>) -> Result<Wiring, rusqlite::Error> {
    Ok(Wiring {
        id: row.get(0)?,
        messaging_group_id: row.get(1)?,
        agent_group_id: row.get(2)?,
        engage_mode: row.get(3)?,
        engage_pattern: row.get(4)?,
        sender_scope: row.get(5)?,
        ignored_message_policy: row.get(6)?,
        session_mode: row.get(7)?,
        priority: row.get(8)?,
        created_at: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, agent_groups};

    #[test]
    fn create_and_lookup_by_platform() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let created = create(conn, "web", "chat-1", Some("Main"), false)?;
            assert_eq!(created.unknown_sender_policy, UnknownSenderPolicy::Strict);
            let found = get_by_platform(conn, "web", "chat-1")?.expect("must exist");
            assert_eq!(found, created);
            assert_eq!(get_by_platform(conn, "web", "other")?, None);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn archive_and_restore_toggles_archived_at() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let chat = create(conn, "web", "chat-1", Some("Main"), false)?;
            assert_eq!(chat.archived_at, None);

            assert!(set_archived(conn, &chat.id, true)?);
            assert!(
                get(conn, &chat.id)?.expect("exists").archived_at.is_some(),
                "archived_at set"
            );
            // Idempotent: archiving an archived chat changes nothing.
            assert!(!set_archived(conn, &chat.id, true)?);

            assert!(set_archived(conn, &chat.id, false)?);
            assert_eq!(get(conn, &chat.id)?.expect("exists").archived_at, None);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn duplicate_platform_identity_is_rejected() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            create(conn, "web", "chat-1", None, false)?;
            assert!(create(conn, "web", "chat-1", None, false).is_err());
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn update_persists_policy_and_denial() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let mut group = create(conn, "web", "chat-1", None, false)?;
            group.unknown_sender_policy = UnknownSenderPolicy::Public;
            group.denied_at = Some("2026-06-11T00:00:00Z".to_owned());
            assert!(update(conn, &group)?);
            let fetched = get(conn, &group.id)?.expect("must exist");
            assert_eq!(fetched.unknown_sender_policy, UnknownSenderPolicy::Public);
            assert_eq!(fetched.denied_at.as_deref(), Some("2026-06-11T00:00:00Z"));
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn wirings_default_sanely_and_order_by_priority() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let mg = create(conn, "web", "chat-1", None, false)?;
            let worker = agent_groups::create(conn, "Worker", "worker")?;
            let manager = agent_groups::create(conn, "Manager", "manager")?;

            let low = wire(conn, &mg.id, &worker.id)?;
            assert_eq!(low.engage_mode, EngageMode::Mention);
            assert_eq!(low.session_mode, SessionMode::Shared);
            assert_eq!(low.ignored_message_policy, IgnoredMessagePolicy::Drop);

            let mut high = wire(conn, &mg.id, &manager.id)?;
            high.priority = 10;
            high.engage_mode = EngageMode::Pattern;
            high.engage_pattern = Some(".".to_owned());
            assert!(update_wiring(conn, &high)?);

            let ordered = wirings_for(conn, &mg.id)?;
            assert_eq!(ordered.len(), 2);
            assert_eq!(ordered[0].agent_group_id, manager.id);
            assert_eq!(ordered[1].agent_group_id, worker.id);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn wiring_the_same_pair_twice_is_rejected_and_unwire_works() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let mg = create(conn, "web", "chat-1", None, false)?;
            let ag = agent_groups::create(conn, "Andy", "andy")?;
            let wiring = wire(conn, &mg.id, &ag.id)?;
            assert!(wire(conn, &mg.id, &ag.id).is_err());
            assert!(unwire(conn, &wiring.id)?);
            assert!(wirings_for(conn, &mg.id)?.is_empty());
            Ok(())
        })
        .expect("db ops");
    }
}
