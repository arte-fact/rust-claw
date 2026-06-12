use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::protocol::entities::{ConnectorConfig, ConnectorKind};
use crate::protocol::ids::{AgentGroupId, ConnectorId};

#[derive(Debug, Clone, PartialEq)]
pub struct Connector {
    pub id: ConnectorId,
    pub label: Option<String>,
    pub config: ConnectorConfig,
    pub agent_group_id: AgentGroupId,
    pub enabled: bool,
    pub created_at: String,
}

impl Connector {
    #[must_use]
    pub fn kind(&self) -> ConnectorKind {
        self.config.kind()
    }
}

pub fn create(
    conn: &Connection,
    config: &ConnectorConfig,
    agent_group_id: &AgentGroupId,
    label: Option<&str>,
) -> Result<Connector, rusqlite::Error> {
    let id = ConnectorId::new(crate::db::generate_id("conn"));
    let json = config.to_json().map_err(json_error)?;
    conn.execute(
        "INSERT INTO connectors (id, kind, label, config, agent_group_id, enabled, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![id, config.kind(), label, json, agent_group_id],
    )?;
    conn.query_row(
        &format!("{SELECT_CONNECTOR} WHERE id = ?1"),
        params![id],
        from_row,
    )
}

pub fn get(conn: &Connection, id: &ConnectorId) -> Result<Option<Connector>, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_CONNECTOR} WHERE id = ?1"),
        params![id],
        from_row,
    )
    .optional()
}

/// The routing fallback lookup: the channel's assignment, if it is switched on.
pub fn get_enabled_by_kind(
    conn: &Connection,
    kind: ConnectorKind,
) -> Result<Option<Connector>, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_CONNECTOR} WHERE kind = ?1 AND enabled = 1"),
        params![kind],
        from_row,
    )
    .optional()
}

pub fn list(conn: &Connection) -> Result<Vec<Connector>, rusqlite::Error> {
    conn.prepare(&format!("{SELECT_CONNECTOR} ORDER BY kind"))?
        .query_map([], from_row)?
        .collect()
}

pub fn update(conn: &Connection, connector: &Connector) -> Result<bool, rusqlite::Error> {
    let json = connector.config.to_json().map_err(json_error)?;
    let changed = conn.execute(
        "UPDATE connectors
         SET kind = ?2, label = ?3, config = ?4, agent_group_id = ?5, enabled = ?6
         WHERE id = ?1",
        params![
            connector.id,
            connector.kind(),
            connector.label,
            json,
            connector.agent_group_id,
            connector.enabled,
        ],
    )?;
    Ok(changed == 1)
}

pub fn delete(conn: &Connection, id: &ConnectorId) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute("DELETE FROM connectors WHERE id = ?1", params![id])?;
    Ok(changed == 1)
}

const SELECT_CONNECTOR: &str =
    "SELECT id, kind, label, config, agent_group_id, enabled, created_at FROM connectors";

fn from_row(row: &Row<'_>) -> Result<Connector, rusqlite::Error> {
    let kind: ConnectorKind = row.get(1)?;
    let raw: String = row.get(3)?;
    let config = ConnectorConfig::parse(kind, &raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(Connector {
        id: row.get(0)?,
        label: row.get(2)?,
        config,
        agent_group_id: row.get(4)?,
        enabled: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn json_error(err: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, agent_groups};
    use crate::protocol::entities::SmsConnectorConfig;

    fn sms_config(base_url: &str) -> ConnectorConfig {
        ConnectorConfig::Sms(SmsConnectorConfig {
            base_url: base_url.to_owned(),
            token: "sms_secret".to_owned(),
            webhook_secret: None,
        })
    }

    fn db_with_group() -> (CentralDb, AgentGroupId) {
        let db = CentralDb::open_in_memory().expect("open");
        let id = db
            .with(|conn| agent_groups::create(conn, "Andy", "andy").map(|group| group.id))
            .expect("group");
        (db, id)
    }

    #[test]
    fn create_update_delete_round_trip() {
        let (db, group) = db_with_group();
        db.with(|conn| {
            let mut connector = create(
                conn,
                &sms_config("http://sim:8080"),
                &group,
                Some("the SIM"),
            )?;
            assert_eq!(connector.kind(), ConnectorKind::Sms);
            assert!(connector.enabled, "connectors start enabled");
            assert_eq!(connector.label.as_deref(), Some("the SIM"));

            let ConnectorConfig::Sms(config) = &mut connector.config;
            config.webhook_secret = Some("hook-secret".to_owned());
            connector.enabled = false;
            assert!(update(conn, &connector)?);

            let fetched = get(conn, &connector.id)?.expect("must exist");
            assert_eq!(fetched, connector);

            assert_eq!(list(conn)?.len(), 1);
            assert!(delete(conn, &connector.id)?);
            assert_eq!(get(conn, &connector.id)?, None);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn one_connector_per_kind() {
        let (db, group) = db_with_group();
        db.with(|conn| {
            create(conn, &sms_config("http://sim:8080"), &group, None)?;
            assert!(
                create(conn, &sms_config("http://other:8080"), &group, None).is_err(),
                "kind is unique"
            );
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn an_unknown_agent_group_is_rejected_by_foreign_keys() {
        let (db, _group) = db_with_group();
        db.with(|conn| {
            let ghost = AgentGroupId::new("ag-ghost");
            assert!(create(conn, &sms_config("http://sim:8080"), &ghost, None).is_err());
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn get_enabled_by_kind_skips_a_disabled_connector() {
        let (db, group) = db_with_group();
        db.with(|conn| {
            let mut connector = create(conn, &sms_config("http://sim:8080"), &group, None)?;
            assert!(get_enabled_by_kind(conn, ConnectorKind::Sms)?.is_some());

            connector.enabled = false;
            update(conn, &connector)?;
            assert_eq!(get_enabled_by_kind(conn, ConnectorKind::Sms)?, None);
            Ok(())
        })
        .expect("db ops");
    }
}
