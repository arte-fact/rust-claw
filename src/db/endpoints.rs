use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::protocol::ids::EndpointName;

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub name: EndpointName,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub notes: Option<String>,
    pub created_at: String,
}

pub fn create(
    conn: &Connection,
    name: &EndpointName,
    base_url: &str,
) -> Result<Endpoint, rusqlite::Error> {
    conn.execute(
        "INSERT INTO endpoints (name, base_url, created_at)
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![name, base_url],
    )?;
    conn.query_row(
        &format!("{SELECT_ENDPOINT} WHERE name = ?1"),
        params![name],
        from_row,
    )
}

pub fn get(conn: &Connection, name: &EndpointName) -> Result<Option<Endpoint>, rusqlite::Error> {
    conn.query_row(
        &format!("{SELECT_ENDPOINT} WHERE name = ?1"),
        params![name],
        from_row,
    )
    .optional()
}

pub fn list(conn: &Connection) -> Result<Vec<Endpoint>, rusqlite::Error> {
    conn.prepare(&format!("{SELECT_ENDPOINT} ORDER BY name"))?
        .query_map([], from_row)?
        .collect()
}

pub fn update(conn: &Connection, endpoint: &Endpoint) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE endpoints
         SET base_url = ?2, api_key = ?3, api_key_env = ?4, notes = ?5
         WHERE name = ?1",
        params![
            endpoint.name,
            endpoint.base_url,
            endpoint.api_key,
            endpoint.api_key_env,
            endpoint.notes,
        ],
    )?;
    Ok(changed == 1)
}

pub fn delete(conn: &Connection, name: &EndpointName) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute("DELETE FROM endpoints WHERE name = ?1", params![name])?;
    Ok(changed == 1)
}

const SELECT_ENDPOINT: &str =
    "SELECT name, base_url, api_key, api_key_env, notes, created_at FROM endpoints";

fn from_row(row: &Row<'_>) -> Result<Endpoint, rusqlite::Error> {
    Ok(Endpoint {
        name: row.get(0)?,
        base_url: row.get(1)?,
        api_key: row.get(2)?,
        api_key_env: row.get(3)?,
        notes: row.get(4)?,
        created_at: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CentralDb;

    #[test]
    fn create_update_delete_round_trip() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let name = EndpointName::new("local-llama");
            let mut endpoint = create(conn, &name, "http://localhost:8000/v1")?;
            assert_eq!(endpoint.api_key, None);

            endpoint.api_key_env = Some("LOCAL_LLAMA_KEY".to_owned());
            endpoint.notes = Some("MI50 box".to_owned());
            assert!(update(conn, &endpoint)?);
            let fetched = get(conn, &name)?.expect("must exist");
            assert_eq!(fetched.api_key_env.as_deref(), Some("LOCAL_LLAMA_KEY"));

            assert_eq!(list(conn)?.len(), 1);
            assert!(delete(conn, &name)?);
            assert_eq!(get(conn, &name)?, None);
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let name = EndpointName::new("openrouter");
            create(conn, &name, "https://openrouter.ai/api/v1")?;
            assert!(create(conn, &name, "https://other.example/v1").is_err());
            Ok(())
        })
        .expect("db ops");
    }

    #[test]
    fn deleting_a_referenced_endpoint_is_blocked_by_foreign_keys() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            let name = EndpointName::new("local");
            create(conn, &name, "http://localhost:8000/v1")?;
            let mut group = crate::db::agent_groups::create(conn, "Andy", "andy")?;
            group.endpoint = Some(name.clone());
            crate::db::agent_groups::update(conn, &group)?;
            assert!(delete(conn, &name).is_err());
            Ok(())
        })
        .expect("db ops");
    }
}
