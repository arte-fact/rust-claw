use serde::{Deserialize, Serialize};

use super::macros::text_enum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(i64);

impl Seq {
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> i64 {
        self.0
    }

    #[must_use]
    pub const fn is_claw_assigned(self) -> bool {
        self.0 % 2 == 0
    }

    #[must_use]
    pub const fn is_agent_assigned(self) -> bool {
        !self.is_claw_assigned()
    }

    #[must_use]
    pub fn next_claw_after(highest: Option<Self>) -> Self {
        Self::next_with_parity(highest, 0)
    }

    #[must_use]
    pub fn next_agent_after(highest: Option<Self>) -> Self {
        Self::next_with_parity(highest, 1)
    }

    fn next_with_parity(highest: Option<Self>, parity: i64) -> Self {
        let floor = highest.map_or(-1, Self::value);
        let candidate = floor + 1;
        if candidate.rem_euclid(2) == parity {
            Self(candidate)
        } else {
            Self(candidate + 1)
        }
    }
}

impl std::fmt::Display for Seq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl rusqlite::types::ToSql for Seq {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl rusqlite::types::FromSql for Seq {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        i64::column_result(value).map(Self)
    }
}

text_enum!(MessageKind {
    Chat => "chat",
    Task => "task",
    Webhook => "webhook",
    System => "system",
});

text_enum!(MessageStatus {
    Pending => "pending",
    Processing => "processing",
    Completed => "completed",
    Failed => "failed",
    Paused => "paused",
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_survives_a_text_round_trip() {
        for kind in MessageKind::ALL {
            let parsed: MessageKind = kind.as_str().parse().expect("must parse back");
            assert_eq!(parsed, *kind);
        }
    }

    #[test]
    fn every_status_survives_a_text_round_trip() {
        for status in MessageStatus::ALL {
            let parsed: MessageStatus = status.as_str().parse().expect("must parse back");
            assert_eq!(parsed, *status);
        }
    }

    #[test]
    fn kinds_serialize_as_kebab_case_json_strings() {
        let json = serde_json::to_string(&MessageKind::Webhook).expect("serialize");
        assert_eq!(json, "\"webhook\"");
        let back: MessageKind = serde_json::from_str("\"system\"").expect("deserialize");
        assert_eq!(back, MessageKind::System);
    }

    #[test]
    fn unknown_text_is_rejected_with_the_type_name() {
        let err = "shout".parse::<MessageKind>().expect_err("must fail");
        assert_eq!(err.type_name, "MessageKind");
        assert_eq!(err.value, "shout");
    }

    #[test]
    fn seq_parity_separates_claw_from_agent() {
        assert!(Seq::new(0).is_claw_assigned());
        assert!(Seq::new(42).is_claw_assigned());
        assert!(Seq::new(1).is_agent_assigned());
        assert!(Seq::new(7).is_agent_assigned());
        assert!(!Seq::new(7).is_claw_assigned());
    }

    #[test]
    fn next_seq_respects_parity_from_any_floor() {
        assert_eq!(Seq::next_claw_after(None), Seq::new(0));
        assert_eq!(Seq::next_agent_after(None), Seq::new(1));
        assert_eq!(Seq::next_claw_after(Some(Seq::new(0))), Seq::new(2));
        assert_eq!(Seq::next_claw_after(Some(Seq::new(1))), Seq::new(2));
        assert_eq!(Seq::next_agent_after(Some(Seq::new(1))), Seq::new(3));
        assert_eq!(Seq::next_agent_after(Some(Seq::new(2))), Seq::new(3));
        assert_eq!(Seq::next_claw_after(Some(Seq::new(7))), Seq::new(8));
        assert_eq!(Seq::next_agent_after(Some(Seq::new(8))), Seq::new(9));
    }

    #[test]
    fn seq_serializes_as_a_bare_integer() {
        assert_eq!(serde_json::to_string(&Seq::new(5)).expect("serialize"), "5");
        let back: Seq = serde_json::from_str("6").expect("deserialize");
        assert_eq!(back, Seq::new(6));
    }

    #[test]
    fn statuses_round_trip_through_sql() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute("CREATE TABLE t (status TEXT)", [])
            .expect("create");
        conn.execute(
            "INSERT INTO t (status) VALUES (?1)",
            [&MessageStatus::Processing],
        )
        .expect("insert");
        let back: MessageStatus = conn
            .query_row("SELECT status FROM t", [], |row| row.get(0))
            .expect("select");
        assert_eq!(back, MessageStatus::Processing);
    }
}
