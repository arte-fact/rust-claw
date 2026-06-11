use rusqlite::Connection;

use super::DbError;

pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub up: fn(&Connection) -> Result<(), rusqlite::Error>,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "001-initial",
        up: initial,
    },
    Migration {
        version: 2,
        name: "002-web-messages",
        up: web_messages,
    },
    Migration {
        version: 3,
        name: "003-tool-profile",
        up: tool_profile,
    },
];

pub fn run(conn: &mut Connection) -> Result<(), DbError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (
           version    INTEGER PRIMARY KEY,
           name       TEXT NOT NULL,
           applied_at TEXT NOT NULL
         )",
        [],
    )?;
    for migration in MIGRATIONS {
        let already_applied: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_version WHERE version = ?1)",
            [migration.version],
            |row| row.get(0),
        )?;
        if already_applied {
            continue;
        }
        let tx = conn.transaction()?;
        (migration.up)(&tx).map_err(|source| DbError::Migration {
            name: migration.name,
            source,
        })?;
        tx.execute(
            "INSERT INTO schema_version (version, name, applied_at)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            rusqlite::params![migration.version, migration.name],
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn initial(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE endpoints (
           name        TEXT PRIMARY KEY,
           base_url    TEXT NOT NULL,
           api_key     TEXT,
           api_key_env TEXT,
           notes       TEXT,
           created_at  TEXT NOT NULL
         );

         CREATE TABLE agent_groups (
           id             TEXT PRIMARY KEY,
           name           TEXT NOT NULL,
           folder         TEXT NOT NULL UNIQUE,
           agent_provider TEXT,
           endpoint       TEXT REFERENCES endpoints(name),
           model          TEXT,
           thinking_level TEXT,
           cli_scope      TEXT NOT NULL DEFAULT 'group',
           created_at     TEXT NOT NULL
         );

         CREATE TABLE messaging_groups (
           id                    TEXT PRIMARY KEY,
           channel_type          TEXT NOT NULL,
           platform_id           TEXT NOT NULL,
           name                  TEXT,
           is_group              INTEGER NOT NULL DEFAULT 0,
           unknown_sender_policy TEXT NOT NULL DEFAULT 'strict',
           denied_at             TEXT,
           created_at            TEXT NOT NULL,
           UNIQUE(channel_type, platform_id)
         );

         CREATE TABLE messaging_group_agents (
           id                     TEXT PRIMARY KEY,
           messaging_group_id     TEXT NOT NULL REFERENCES messaging_groups(id),
           agent_group_id         TEXT NOT NULL REFERENCES agent_groups(id),
           engage_mode            TEXT NOT NULL DEFAULT 'mention',
           engage_pattern         TEXT,
           sender_scope           TEXT NOT NULL DEFAULT 'all',
           ignored_message_policy TEXT NOT NULL DEFAULT 'drop',
           session_mode           TEXT NOT NULL DEFAULT 'shared',
           priority               INTEGER NOT NULL DEFAULT 0,
           created_at             TEXT NOT NULL,
           UNIQUE(messaging_group_id, agent_group_id)
         );

         CREATE TABLE users (
           id           TEXT PRIMARY KEY,
           kind         TEXT NOT NULL,
           display_name TEXT,
           created_at   TEXT NOT NULL
         );

         CREATE TABLE user_roles (
           user_id        TEXT NOT NULL REFERENCES users(id),
           role           TEXT NOT NULL,
           agent_group_id TEXT REFERENCES agent_groups(id),
           granted_by     TEXT REFERENCES users(id),
           granted_at     TEXT NOT NULL,
           PRIMARY KEY (user_id, role, agent_group_id)
         );
         -- NULLs are pairwise-distinct in SQLite unique constraints; global grants need their own index
         CREATE UNIQUE INDEX idx_user_roles_global
           ON user_roles(user_id, role) WHERE agent_group_id IS NULL;

         CREATE TABLE agent_group_members (
           user_id        TEXT NOT NULL REFERENCES users(id),
           agent_group_id TEXT NOT NULL REFERENCES agent_groups(id),
           added_by       TEXT REFERENCES users(id),
           added_at       TEXT NOT NULL,
           PRIMARY KEY (user_id, agent_group_id)
         );

         CREATE TABLE user_dms (
           user_id            TEXT NOT NULL REFERENCES users(id),
           channel_type       TEXT NOT NULL,
           messaging_group_id TEXT NOT NULL REFERENCES messaging_groups(id),
           resolved_at        TEXT NOT NULL,
           PRIMARY KEY (user_id, channel_type)
         );

         CREATE TABLE sessions (
           id                 TEXT PRIMARY KEY,
           agent_group_id     TEXT NOT NULL REFERENCES agent_groups(id),
           messaging_group_id TEXT REFERENCES messaging_groups(id),
           thread_id          TEXT,
           status             TEXT NOT NULL DEFAULT 'active',
           last_active        TEXT,
           created_at         TEXT NOT NULL
         );
         CREATE INDEX idx_sessions_agent_group ON sessions(agent_group_id);
         CREATE INDEX idx_sessions_lookup ON sessions(messaging_group_id, thread_id);

         CREATE TABLE pending_questions (
           question_id    TEXT PRIMARY KEY,
           session_id     TEXT NOT NULL REFERENCES sessions(id),
           message_out_id TEXT NOT NULL,
           platform_id    TEXT,
           channel_type   TEXT,
           thread_id      TEXT,
           title          TEXT NOT NULL,
           options_json   TEXT NOT NULL,
           created_at     TEXT NOT NULL
         );

         CREATE TABLE dropped_messages (
           id           TEXT PRIMARY KEY,
           channel_type TEXT NOT NULL,
           platform_id  TEXT NOT NULL,
           reason       TEXT NOT NULL,
           content      TEXT,
           created_at   TEXT NOT NULL
         );",
    )
}

fn web_messages(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE web_messages (
           id                 INTEGER PRIMARY KEY AUTOINCREMENT,
           messaging_group_id TEXT NOT NULL REFERENCES messaging_groups(id),
           direction          TEXT NOT NULL,
           sender             TEXT NOT NULL,
           body               TEXT NOT NULL,
           message_out_id     TEXT,
           created_at         TEXT NOT NULL
         );
         CREATE INDEX idx_web_messages_group ON web_messages(messaging_group_id, id);",
    )
}

fn tool_profile(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "ALTER TABLE agent_groups ADD COLUMN tool_profile TEXT NOT NULL DEFAULT 'chat';",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_is_idempotent_and_records_versions() {
        let mut conn = Connection::open_in_memory().expect("open");
        run(&mut conn).expect("first run");
        run(&mut conn).expect("second run");
        let applied: Vec<(u32, String)> = conn
            .prepare("SELECT version, name FROM schema_version ORDER BY version")
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("collect");
        assert_eq!(
            applied,
            vec![
                (1, "001-initial".to_owned()),
                (2, "002-web-messages".to_owned()),
                (3, "003-tool-profile".to_owned()),
            ]
        );
    }

    #[test]
    fn versions_are_strictly_increasing_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        let mut previous = 0;
        for migration in MIGRATIONS {
            assert!(
                migration.version > previous,
                "migration {} breaks version ordering",
                migration.name
            );
            assert!(seen.insert(migration.version));
            previous = migration.version;
        }
    }
}
