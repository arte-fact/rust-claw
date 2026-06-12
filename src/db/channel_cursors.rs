use rusqlite::{Connection, OptionalExtension, params};

pub fn get(conn: &Connection, channel_type: &str) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT cursor FROM channel_cursors WHERE channel_type = ?1",
        params![channel_type],
        |row| row.get(0),
    )
    .optional()
}

pub fn set(conn: &Connection, channel_type: &str, cursor: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO channel_cursors (channel_type, cursor) VALUES (?1, ?2)
         ON CONFLICT(channel_type) DO UPDATE SET cursor = excluded.cursor",
        params![channel_type, cursor],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CentralDb;

    #[test]
    fn a_missing_cursor_is_none_and_set_upserts() {
        let db = CentralDb::open_in_memory().expect("open");
        db.with(|conn| {
            assert_eq!(get(conn, "sms")?, None);

            set(conn, "sms", 48)?;
            assert_eq!(get(conn, "sms")?, Some(48));

            set(conn, "sms", 51)?;
            assert_eq!(get(conn, "sms")?, Some(51));

            assert_eq!(get(conn, "telegram")?, None, "cursors are per channel");
            Ok(())
        })
        .expect("db ops");
    }
}
