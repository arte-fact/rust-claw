use crate::protocol::entities::EngageMode;

/// Whether an inbound message should wake the agent now or just accumulate into
/// the session for the next run (§10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engage {
    Run,
    Accumulate,
}

/// Everything the engage decision needs. `sticky` is the session's carried
/// mention state (deferred until group channels land — the router passes false).
pub struct EngageInput<'a> {
    /// Host-generated kinds (task/system/webhook) always wake the agent.
    pub is_chat: bool,
    pub is_group: bool,
    pub is_mention: bool,
    pub sticky: bool,
    pub mode: EngageMode,
    pub pattern: Option<&'a str>,
    pub text: &'a str,
}

/// Decides engagement. Non-chat kinds and direct messages always run; group
/// chats consult the wiring's `engage_mode`.
#[must_use]
pub fn evaluate(input: &EngageInput<'_>) -> Engage {
    if !input.is_chat || !input.is_group {
        return Engage::Run;
    }
    let engaged = match input.mode {
        EngageMode::Pattern => input
            .pattern
            .is_some_and(|pattern| matches_pattern(pattern, input.text)),
        EngageMode::Mention => input.is_mention,
        EngageMode::MentionSticky => input.is_mention || input.sticky,
    };
    if engaged {
        Engage::Run
    } else {
        Engage::Accumulate
    }
}

/// MVP "pattern": case-insensitive substring containment. Regex patterns are a
/// future enhancement (would add the `regex` dependency); substring is enough for
/// the keyword-trigger use that group channels need first.
fn matches_pattern(pattern: &str, text: &str) -> bool {
    !pattern.is_empty() && text.to_lowercase().contains(&pattern.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(mode: EngageMode, is_group: bool, is_mention: bool) -> EngageInput<'static> {
        EngageInput {
            is_chat: true,
            is_group,
            is_mention,
            sticky: false,
            mode,
            pattern: None,
            text: "",
        }
    }

    #[test]
    fn direct_messages_always_run() {
        for mode in EngageMode::ALL {
            assert_eq!(evaluate(&input(*mode, false, false)), Engage::Run);
        }
    }

    #[test]
    fn non_chat_always_runs_even_in_groups() {
        let scheduled = EngageInput {
            is_chat: false,
            ..input(EngageMode::Mention, true, false)
        };
        assert_eq!(evaluate(&scheduled), Engage::Run);
    }

    #[test]
    fn mention_mode_runs_only_when_mentioned() {
        assert_eq!(
            evaluate(&input(EngageMode::Mention, true, true)),
            Engage::Run
        );
        assert_eq!(
            evaluate(&input(EngageMode::Mention, true, false)),
            Engage::Accumulate
        );
    }

    #[test]
    fn mention_sticky_runs_when_mentioned_or_sticky() {
        let sticky = EngageInput {
            sticky: true,
            ..input(EngageMode::MentionSticky, true, false)
        };
        assert_eq!(evaluate(&sticky), Engage::Run);
        assert_eq!(
            evaluate(&input(EngageMode::MentionSticky, true, false)),
            Engage::Accumulate
        );
    }

    #[test]
    fn pattern_mode_matches_substring_case_insensitively() {
        let hit = EngageInput {
            pattern: Some("Deploy"),
            text: "please deploy the build",
            ..input(EngageMode::Pattern, true, false)
        };
        assert_eq!(evaluate(&hit), Engage::Run);
        let miss = EngageInput {
            pattern: Some("deploy"),
            text: "what is the weather",
            ..input(EngageMode::Pattern, true, false)
        };
        assert_eq!(evaluate(&miss), Engage::Accumulate);
    }

    #[test]
    fn pattern_without_a_configured_pattern_accumulates() {
        assert_eq!(
            evaluate(&input(EngageMode::Pattern, true, false)),
            Engage::Accumulate
        );
    }
}
