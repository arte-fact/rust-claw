use super::macros::text_enum;

text_enum!(SessionMode {
    Shared => "shared",
    PerThread => "per-thread",
});

text_enum!(EngageMode {
    Pattern => "pattern",
    Mention => "mention",
    MentionSticky => "mention-sticky",
});

text_enum!(CliScope {
    Disabled => "disabled",
    Group => "group",
    Global => "global",
});

text_enum!(AgentProviderKind {
    Native => "native",
    Echo => "echo",
});

text_enum!(ToolProfile {
    Chat => "chat",
    Coder => "coder",
});

text_enum!(UnknownSenderPolicy {
    Strict => "strict",
    RequestApproval => "request-approval",
    Public => "public",
});

text_enum!(SenderScope {
    All => "all",
    Known => "known",
});

text_enum!(IgnoredMessagePolicy {
    Drop => "drop",
    Accumulate => "accumulate",
});

text_enum!(Role {
    Owner => "owner",
    Admin => "admin",
});

text_enum!(SessionStatus {
    Active => "active",
    Closed => "closed",
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_word_variants_use_kebab_case() {
        assert_eq!(SessionMode::PerThread.as_str(), "per-thread");
        assert_eq!(EngageMode::MentionSticky.as_str(), "mention-sticky");
    }

    #[test]
    fn every_entity_enum_survives_a_text_round_trip() {
        for mode in SessionMode::ALL {
            assert_eq!(mode.as_str().parse::<SessionMode>().ok(), Some(*mode));
        }
        for mode in EngageMode::ALL {
            assert_eq!(mode.as_str().parse::<EngageMode>().ok(), Some(*mode));
        }
        for scope in CliScope::ALL {
            assert_eq!(scope.as_str().parse::<CliScope>().ok(), Some(*scope));
        }
        for kind in AgentProviderKind::ALL {
            assert_eq!(kind.as_str().parse::<AgentProviderKind>().ok(), Some(*kind));
        }
    }

    #[test]
    fn cli_scope_round_trips_through_sql() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute("CREATE TABLE t (scope TEXT)", [])
            .expect("create");
        conn.execute("INSERT INTO t (scope) VALUES (?1)", [&CliScope::Group])
            .expect("insert");
        let back: CliScope = conn
            .query_row("SELECT scope FROM t", [], |row| row.get(0))
            .expect("select");
        assert_eq!(back, CliScope::Group);
    }
}
