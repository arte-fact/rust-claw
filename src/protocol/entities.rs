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

text_enum!(ConnectorKind {
    Sms => "sms",
});

/// sim-server credentials (M17). `base_url` without a trailing slash; `token`
/// needs the `read` + `send` scopes; `webhook_secret` enables the wake-up hook.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmsConnectorConfig {
    pub base_url: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
}

/// Per-kind connector settings: the `kind` column picks the variant, so config
/// JSON is always parsed against the right shape. Adding a kind extends every
/// `match` below — the compiler walks you to each integration point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorConfig {
    Sms(SmsConnectorConfig),
}

impl ConnectorConfig {
    #[must_use]
    pub fn kind(&self) -> ConnectorKind {
        match self {
            Self::Sms(_) => ConnectorKind::Sms,
        }
    }

    pub fn parse(kind: ConnectorKind, json: &str) -> Result<Self, serde_json::Error> {
        match kind {
            ConnectorKind::Sms => serde_json::from_str(json).map(Self::Sms),
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Sms(config) => serde_json::to_string(config),
        }
    }
}

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
    fn connector_config_round_trips_and_keeps_kind_consistent() {
        let config = ConnectorConfig::Sms(SmsConnectorConfig {
            base_url: "http://sim:8080".to_owned(),
            token: "sms_secret".to_owned(),
            webhook_secret: None,
        });
        assert_eq!(config.kind(), ConnectorKind::Sms);
        let json = config.to_json().expect("serialize");
        assert!(
            !json.contains("webhook_secret"),
            "an absent secret must not serialize as null"
        );
        let back = ConnectorConfig::parse(ConnectorKind::Sms, &json).expect("parse");
        assert_eq!(back, config);
    }

    #[test]
    fn connector_config_rejects_a_wrong_shape() {
        assert!(ConnectorConfig::parse(ConnectorKind::Sms, "{\"nope\":1}").is_err());
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
