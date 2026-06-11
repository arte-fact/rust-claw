use rusqlite::{Connection, params};

use super::generate_id;

pub fn record(
    conn: &Connection,
    channel_type: &str,
    platform_id: &str,
    reason: &str,
    content: Option<&str>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO dropped_messages (id, channel_type, platform_id, reason, content, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![
            generate_id("drop"),
            channel_type,
            platform_id,
            reason,
            content
        ],
    )?;
    Ok(())
}

pub fn count(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row("SELECT count(*) FROM dropped_messages", [], |row| {
        row.get(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CentralDb;

    #[test]
    fn recorded_drops_are_counted() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            assert_eq!(count(conn)?, 0);
            record(conn, "web", "chat-1", "no-wiring", Some("{}"))?;
            record(conn, "web", "chat-1", "gate-denied", None)?;
            assert_eq!(count(conn)?, 2);
            Ok(())
        })
        .expect("db ops");
    }
}
