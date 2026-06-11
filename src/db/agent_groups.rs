use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::protocol::entities::{AgentProviderKind, CliScope};
use crate::protocol::ids::{AgentGroupId, EndpointName};

use super::generate_id;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentGroup {
    pub id: AgentGroupId,
    pub name: String,
    pub folder: String,
    pub agent_provider: Option<AgentProviderKind>,
    pub endpoint: Option<EndpointName>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    pub cli_scope: CliScope,
    pub created_at: String,
}

pub fn create(conn: &Connection, name: &str, folder: &str) -> Result<AgentGroup, rusqlite::Error> {
    let id = AgentGroupId::new(generate_id("ag"));
    conn.execute(
        "INSERT INTO agent_groups (id, name, folder, created_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![id, name, folder],
    )?;
    fetch_required(conn, &id)
}

pub fn get(conn: &Connection, id: &AgentGroupId) -> Result<Option<AgentGroup>, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_COLUMNS} WHERE id = ?1"),
        params![id],
        from_row,
    )
    .optional()
}

pub fn list(conn: &Connection) -> Result<Vec<AgentGroup>, rusqlite::Error> {
    conn.prepare(&format!("{SELECT_COLUMNS} ORDER BY created_at, id"))?
        .query_map([], from_row)?
        .collect()
}

pub fn update(conn: &Connection, group: &AgentGroup) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE agent_groups
         SET name = ?2, folder = ?3, agent_provider = ?4, endpoint = ?5,
             model = ?6, thinking_level = ?7, cli_scope = ?8
         WHERE id = ?1",
        params![
            group.id,
            group.name,
            group.folder,
            group.agent_provider,
            group.endpoint,
            group.model,
            group.thinking_level,
            group.cli_scope,
        ],
    )?;
    Ok(changed == 1)
}

pub fn delete(conn: &Connection, id: &AgentGroupId) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute("DELETE FROM agent_groups WHERE id = ?1", params![id])?;
    Ok(changed == 1)
}

const SELECT_COLUMNS: &str = "SELECT id, name, folder, agent_provider, endpoint, model,
        thinking_level, cli_scope, created_at FROM agent_groups";

fn fetch_required(conn: &Connection, id: &AgentGroupId) -> Result<AgentGroup, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_COLUMNS} WHERE id = ?1"),
        params![id],
        from_row,
    )
}

fn from_row(row: &Row<'_>) -> Result<AgentGroup, rusqlite::Error> {
    Ok(AgentGroup {
        id: row.get(0)?,
        name: row.get(1)?,
        folder: row.get(2)?,
        agent_provider: row.get(3)?,
        endpoint: row.get(4)?,
        model: row.get(5)?,
        thinking_level: row.get(6)?,
        cli_scope: row.get(7)?,
        created_at: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CentralDb;

    #[test]
    fn create_get_list_round_trip() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let created = create(conn, "Andy", "andy")?;
            assert!(created.id.as_str().starts_with("ag-"));
            assert_eq!(created.cli_scope, CliScope::Group);
            assert_eq!(created.agent_provider, None);

            let fetched = get(conn, &created.id)?.expect("must exist");
            assert_eq!(fetched, created);
            assert_eq!(list(conn)?, vec![created]);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn update_persists_provider_endpoint_and_model() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            conn.execute(
                "INSERT INTO endpoints (name, base_url, created_at)
                 VALUES ('local', 'http://localhost:8000/v1', '2026-01-01T00:00:00Z')",
                [],
            )?;
            let mut group = create(conn, "Coder", "coder")?;
            group.agent_provider = Some(AgentProviderKind::Native);
            group.endpoint = Some(EndpointName::new("local"));
            group.model = Some("qwen3.6-dense".to_owned());
            group.cli_scope = CliScope::Global;
            assert!(update(conn, &group)?);

            let fetched = get(conn, &group.id)?.expect("must exist");
            assert_eq!(fetched.agent_provider, Some(AgentProviderKind::Native));
            assert_eq!(fetched.endpoint, Some(EndpointName::new("local")));
            assert_eq!(fetched.model.as_deref(), Some("qwen3.6-dense"));
            assert_eq!(fetched.cli_scope, CliScope::Global);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn duplicate_folder_is_rejected() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            create(conn, "A", "same")?;
            assert!(create(conn, "B", "same").is_err());
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn delete_removes_the_row() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let group = create(conn, "Temp", "temp")?;
            assert!(delete(conn, &group.id)?);
            assert!(!delete(conn, &group.id)?);
            assert_eq!(get(conn, &group.id)?, None);
            Ok(())
        })
        .expect("db ops");
    }
}
