pub mod agent_groups;
pub mod dropped;
pub mod endpoints;
pub mod messaging_groups;
pub mod migrations;
pub mod questions;
pub mod sessions;
pub mod users;
pub mod web_messages;

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("migration {name} failed: {source}")]
    Migration {
        name: &'static str,
        #[source]
        source: rusqlite::Error,
    },
    #[error("central db mutex poisoned")]
    Poisoned,
}

pub struct CentralDb {
    conn: Mutex<Connection>,
}

impl CentralDb {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, DbError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let mut conn = conn;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn with<T>(
        &self,
        op: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, DbError> {
        let conn = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        op(&conn).map_err(DbError::from)
    }
}

pub(crate) fn generate_id(prefix: &str) -> String {
    format!("{prefix}-{}", ulid::Ulid::new().to_string().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_applies_pragmas_and_migrations() {
        let db = CentralDb::open_in_memory().expect("open");
        let foreign_keys: i64 = db
            .with(|conn| conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)))
            .expect("pragma");
        assert_eq!(foreign_keys, 1);
        let tables: i64 = db
            .with(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'agent_groups'",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("table check");
        assert_eq!(tables, 1);
    }

    #[test]
    fn generated_ids_carry_the_prefix_and_are_unique() {
        let a = generate_id("ag");
        let b = generate_id("ag");
        assert!(a.starts_with("ag-"));
        assert_ne!(a, b);
    }
}
