use super::macros::string_id;

string_id!(AgentGroupId);
string_id!(MessagingGroupId);
string_id!(SessionId);
string_id!(UserId);
string_id!(MessageInId);
string_id!(MessageOutId);
string_id!(EndpointName);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_as_str_expose_the_raw_value() {
        let id = SessionId::new("sess-01ABC");
        assert_eq!(id.as_str(), "sess-01ABC");
        assert_eq!(id.to_string(), "sess-01ABC");
    }

    #[test]
    fn serde_round_trips_as_plain_string() {
        let id = UserId::new("web:owner");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"web:owner\"");
        let back: UserId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn sql_round_trips_as_text() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute("CREATE TABLE t (id TEXT)", [])
            .expect("create");
        let id = AgentGroupId::new("ag-main");
        conn.execute("INSERT INTO t (id) VALUES (?1)", [&id])
            .expect("insert");
        let back: AgentGroupId = conn
            .query_row("SELECT id FROM t", [], |row| row.get(0))
            .expect("select");
        assert_eq!(back, id);
    }
}
