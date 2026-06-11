mod inbound;
mod outbound;
mod routing;
mod transcript;

pub use inbound::{InboundMessage, NewInboundMessage};
pub use outbound::{NewOutboundMessage, OutboundMessage};
pub use routing::Destination;
pub use transcript::TranscriptEntry;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

use crate::protocol::ids::{AgentGroupId, SessionId};

#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session db mutex poisoned")]
    Poisoned,
}

pub struct SessionStore {
    sessions_root: PathBuf,
}

impl SessionStore {
    #[must_use]
    pub fn new(sessions_root: PathBuf) -> Self {
        Self { sessions_root }
    }

    #[must_use]
    pub fn session_dir(&self, agent_group: &AgentGroupId, session: &SessionId) -> PathBuf {
        self.sessions_root
            .join(agent_group.as_str())
            .join(session.as_str())
    }

    /// Idempotent: creates the folder layout and schema on first call, opens on later calls.
    pub fn init(
        &self,
        agent_group: &AgentGroupId,
        session: &SessionId,
    ) -> Result<SessionDb, SessionStoreError> {
        let dir = self.session_dir(agent_group, session);
        for sub in ["inbox", "outbox", "pi"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        SessionDb::open_dir(dir)
    }

    pub fn open(
        &self,
        agent_group: &AgentGroupId,
        session: &SessionId,
    ) -> Result<SessionDb, SessionStoreError> {
        self.init(agent_group, session)
    }
}

pub struct SessionDb {
    conn: Mutex<Connection>,
    dir: PathBuf,
}

impl SessionDb {
    /// Opens an already-initialized session folder (e.g. from `QueryInput.session_dir`).
    pub fn open_dir(dir: PathBuf) -> Result<Self, SessionStoreError> {
        let conn = Connection::open(dir.join("session.db"))?;
        apply_pragmas(&conn)?;
        conn.execute_batch(SESSION_SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
            dir,
        })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn outbox_dir(&self, message_out_id: &str) -> PathBuf {
        self.dir.join("outbox").join(message_out_id)
    }

    #[must_use]
    pub fn inbox_dir(&self, message_in_id: &str) -> PathBuf {
        self.dir.join("inbox").join(message_in_id)
    }

    pub(crate) fn with<T>(
        &self,
        op: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, SessionStoreError> {
        let conn = self.conn.lock().map_err(|_| SessionStoreError::Poisoned)?;
        op(&conn).map_err(SessionStoreError::from)
    }

    /// Single time source for due-comparisons: SQLite's own UTC clock.
    pub fn now_timestamp(&self) -> Result<String, SessionStoreError> {
        self.with(|conn| {
            conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
                row.get(0)
            })
        })
    }
}

fn apply_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

const SESSION_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS messages_in (
      id                TEXT PRIMARY KEY,
      seq               INTEGER UNIQUE,
      kind              TEXT NOT NULL,
      timestamp         TEXT NOT NULL,
      status            TEXT NOT NULL DEFAULT 'pending',
      process_after     TEXT,
      recurrence        TEXT,
      series_id         TEXT,
      tries             INTEGER NOT NULL DEFAULT 0,
      trigger           INTEGER NOT NULL DEFAULT 1,
      platform_id       TEXT,
      channel_type      TEXT,
      thread_id         TEXT,
      content           TEXT NOT NULL,
      source_session_id TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_messages_in_series ON messages_in(series_id);
    CREATE INDEX IF NOT EXISTS idx_messages_in_status ON messages_in(status);

    CREATE TABLE IF NOT EXISTS messages_out (
      id            TEXT PRIMARY KEY,
      seq           INTEGER UNIQUE,
      in_reply_to   TEXT,
      timestamp     TEXT NOT NULL,
      deliver_after TEXT,
      recurrence    TEXT,
      kind          TEXT NOT NULL,
      platform_id   TEXT,
      channel_type  TEXT,
      thread_id     TEXT,
      content       TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS delivered (
      message_out_id      TEXT PRIMARY KEY,
      platform_message_id TEXT,
      status              TEXT NOT NULL DEFAULT 'delivered',
      delivered_at        TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS destinations (
      name           TEXT PRIMARY KEY,
      display_name   TEXT,
      type           TEXT NOT NULL,
      channel_type   TEXT,
      platform_id    TEXT,
      agent_group_id TEXT
    );

    CREATE TABLE IF NOT EXISTS session_routing (
      id           INTEGER PRIMARY KEY CHECK (id = 1),
      channel_type TEXT,
      platform_id  TEXT,
      thread_id    TEXT
    );
";

#[cfg(test)]
pub(crate) fn test_session_db() -> (tempfile::TempDir, SessionDb) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::new(tmp.path().to_path_buf());
    let db = store
        .init(&AgentGroupId::new("ag-test"), &SessionId::new("sess-test"))
        .expect("init session");
    (tmp, db)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_layout_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(tmp.path().join("sessions"));
        let ag = AgentGroupId::new("ag-1");
        let sess = SessionId::new("sess-1");

        let db = store.init(&ag, &sess).expect("first init");
        for sub in ["inbox", "outbox", "pi"] {
            assert!(db.dir().join(sub).is_dir(), "{sub} must exist");
        }
        assert!(db.dir().join("session.db").is_file());

        let reopened = store.open(&ag, &sess).expect("reopen");
        let tables: i64 = reopened
            .with(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table'
                     AND name IN ('messages_in','messages_out','delivered','destinations','session_routing')",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("count tables");
        assert_eq!(tables, 5);
    }

    #[test]
    fn outbox_and_inbox_dirs_are_message_scoped() {
        let (_tmp, db) = test_session_db();
        assert!(db.outbox_dir("out-1").ends_with("outbox/out-1"));
        assert!(db.inbox_dir("in-1").ends_with("inbox/in-1"));
    }
}
