use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::protocol::entities::Role;
use crate::protocol::ids::{AgentGroupId, MessagingGroupId, UserId};

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: UserId,
    pub kind: String,
    pub display_name: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleGrant {
    pub user_id: UserId,
    pub role: Role,
    pub agent_group_id: Option<AgentGroupId>,
    pub granted_by: Option<UserId>,
    pub granted_at: String,
}

pub fn upsert(
    conn: &Connection,
    id: &UserId,
    kind: &str,
    display_name: Option<&str>,
) -> Result<User, rusqlite::Error> {
    conn.execute(
        "INSERT INTO users (id, kind, display_name, created_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(id) DO UPDATE SET display_name = COALESCE(excluded.display_name, display_name)",
        params![id, kind, display_name],
    )?;
    conn.query_row(
        "SELECT id, kind, display_name, created_at FROM users WHERE id = ?1",
        params![id],
        user_from_row,
    )
}

pub fn get(conn: &Connection, id: &UserId) -> Result<Option<User>, rusqlite::Error> {
    conn.query_row(
        "SELECT id, kind, display_name, created_at FROM users WHERE id = ?1",
        params![id],
        user_from_row,
    )
    .optional()
}

pub fn grant_role(
    conn: &Connection,
    user_id: &UserId,
    role: Role,
    agent_group_id: Option<&AgentGroupId>,
    granted_by: Option<&UserId>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO user_roles (user_id, role, agent_group_id, granted_by, granted_at)
         VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![user_id, role, agent_group_id, granted_by],
    )?;
    Ok(())
}

pub fn revoke_role(
    conn: &Connection,
    user_id: &UserId,
    role: Role,
    agent_group_id: Option<&AgentGroupId>,
) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "DELETE FROM user_roles
         WHERE user_id = ?1 AND role = ?2 AND agent_group_id IS ?3",
        params![user_id, role, agent_group_id],
    )?;
    Ok(changed > 0)
}

pub fn roles_for(conn: &Connection, user_id: &UserId) -> Result<Vec<RoleGrant>, rusqlite::Error> {
    conn.prepare(
        "SELECT user_id, role, agent_group_id, granted_by, granted_at
         FROM user_roles WHERE user_id = ?1 ORDER BY granted_at",
    )?
    .query_map(params![user_id], grant_from_row)?
    .collect()
}

pub fn owners(conn: &Connection) -> Result<Vec<UserId>, rusqlite::Error> {
    conn.prepare("SELECT user_id FROM user_roles WHERE role = 'owner' ORDER BY granted_at")?
        .query_map([], |row| row.get(0))?
        .collect()
}

pub fn add_member(
    conn: &Connection,
    user_id: &UserId,
    agent_group_id: &AgentGroupId,
    added_by: Option<&UserId>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO agent_group_members (user_id, agent_group_id, added_by, added_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![user_id, agent_group_id, added_by],
    )?;
    Ok(())
}

pub fn remove_member(
    conn: &Connection,
    user_id: &UserId,
    agent_group_id: &AgentGroupId,
) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "DELETE FROM agent_group_members WHERE user_id = ?1 AND agent_group_id = ?2",
        params![user_id, agent_group_id],
    )?;
    Ok(changed == 1)
}

pub fn is_member(
    conn: &Connection,
    user_id: &UserId,
    agent_group_id: &AgentGroupId,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_group_members
         WHERE user_id = ?1 AND agent_group_id = ?2)",
        params![user_id, agent_group_id],
        |row| row.get(0),
    )
}

pub fn cache_dm(
    conn: &Connection,
    user_id: &UserId,
    channel_type: &str,
    messaging_group_id: &MessagingGroupId,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO user_dms (user_id, channel_type, messaging_group_id, resolved_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(user_id, channel_type)
         DO UPDATE SET messaging_group_id = excluded.messaging_group_id,
                       resolved_at = excluded.resolved_at",
        params![user_id, channel_type, messaging_group_id],
    )?;
    Ok(())
}

pub fn dm_for(
    conn: &Connection,
    user_id: &UserId,
    channel_type: &str,
) -> Result<Option<MessagingGroupId>, rusqlite::Error> {
    conn.query_row(
        "SELECT messaging_group_id FROM user_dms WHERE user_id = ?1 AND channel_type = ?2",
        params![user_id, channel_type],
        |row| row.get(0),
    )
    .optional()
}

fn user_from_row(row: &Row<'_>) -> Result<User, rusqlite::Error> {
    Ok(User {
        id: row.get(0)?,
        kind: row.get(1)?,
        display_name: row.get(2)?,
        created_at: row.get(3)?,
    })
}

fn grant_from_row(row: &Row<'_>) -> Result<RoleGrant, rusqlite::Error> {
    Ok(RoleGrant {
        user_id: row.get(0)?,
        role: row.get(1)?,
        agent_group_id: row.get(2)?,
        granted_by: row.get(3)?,
        granted_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, agent_groups, messaging_groups};

    #[test]
    fn upsert_creates_then_updates_display_name_only() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let id = UserId::new("web:owner");
            let created = upsert(conn, &id, "web", None)?;
            assert_eq!(created.display_name, None);
            let updated = upsert(conn, &id, "web", Some("Artefact"))?;
            assert_eq!(updated.display_name.as_deref(), Some("Artefact"));
            let unchanged = upsert(conn, &id, "web", None)?;
            assert_eq!(unchanged.display_name.as_deref(), Some("Artefact"));
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn roles_grant_revoke_and_owner_listing() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let owner = UserId::new("web:owner");
            upsert(conn, &owner, "web", None)?;
            grant_role(conn, &owner, Role::Owner, None, None)?;
            grant_role(conn, &owner, Role::Owner, None, None)?;
            assert_eq!(owners(conn)?, vec![owner.clone()]);

            let group = agent_groups::create(conn, "Andy", "andy")?;
            grant_role(conn, &owner, Role::Admin, Some(&group.id), Some(&owner))?;
            let grants = roles_for(conn, &owner)?;
            assert_eq!(grants.len(), 2);

            assert!(revoke_role(conn, &owner, Role::Admin, Some(&group.id))?);
            assert!(!revoke_role(conn, &owner, Role::Admin, Some(&group.id))?);
            assert_eq!(roles_for(conn, &owner)?.len(), 1);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn membership_round_trip() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let user = UserId::new("web:friend");
            upsert(conn, &user, "web", None)?;
            let group = agent_groups::create(conn, "Andy", "andy")?;
            assert!(!is_member(conn, &user, &group.id)?);
            add_member(conn, &user, &group.id, None)?;
            add_member(conn, &user, &group.id, None)?;
            assert!(is_member(conn, &user, &group.id)?);
            assert!(remove_member(conn, &user, &group.id)?);
            assert!(!is_member(conn, &user, &group.id)?);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn dm_cache_upserts_per_user_and_channel() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let user = UserId::new("web:owner");
            upsert(conn, &user, "web", None)?;
            let dm1 = messaging_groups::create(conn, "web", "dm-1", None, false)?;
            let dm2 = messaging_groups::create(conn, "web", "dm-2", None, false)?;
            assert_eq!(dm_for(conn, &user, "web")?, None);
            cache_dm(conn, &user, "web", &dm1.id)?;
            assert_eq!(dm_for(conn, &user, "web")?, Some(dm1.id));
            cache_dm(conn, &user, "web", &dm2.id)?;
            assert_eq!(dm_for(conn, &user, "web")?, Some(dm2.id));
            Ok(())
        })
        .expect("db ops");
    }
}
