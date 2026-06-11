use serde_json::{Map, Value};

use crate::protocol::entities::CliScope;
use crate::protocol::frame::{ErrorCode, FrameError};
use crate::protocol::ids::AgentGroupId;

use super::registry::CommandDef;

/// Resources a `group`-scoped agent may touch, restricted to its own group.
const GROUP_SCOPED_RESOURCES: &[&str] = &["groups", "sessions", "destinations", "members"];
/// Arg keys that name an agent group; they are auto-filled / cross-checked.
const GROUP_ARG_KEYS: &[&str] = &["id", "group", "agent_group"];

/// Applies a group's `cli_scope` to an agent-issued command. On success returns
/// the (possibly auto-filled) args; on failure a ready-to-send error.
///
/// - `disabled` → nothing is allowed.
/// - `global`   → unrestricted.
/// - `group`    → only `GROUP_SCOPED_RESOURCES`, never a `cli_scope` change, and
///   any group-id arg is forced to the caller's own group.
pub fn enforce_cli_scope(
    scope: CliScope,
    command: &CommandDef,
    caller_group: &AgentGroupId,
    mut args: Map<String, Value>,
) -> Result<Map<String, Value>, FrameError> {
    match scope {
        CliScope::Disabled => Err(forbidden("CLI access is disabled for this agent group")),
        CliScope::Global => Ok(args),
        CliScope::Group => {
            if !GROUP_SCOPED_RESOURCES.contains(&command.resource) {
                return Err(forbidden(format!(
                    "agents in this group may not use {} commands",
                    command.resource
                )));
            }
            if command.args.iter().any(|spec| spec.name == "cli_scope")
                && args.contains_key("cli_scope")
            {
                return Err(forbidden("agents may not change cli_scope"));
            }
            for key in GROUP_ARG_KEYS {
                match args.get(*key).and_then(Value::as_str) {
                    Some(value) if value != caller_group.as_str() => {
                        return Err(forbidden(format!(
                            "agents in this group may only act on their own group, not {value:?}"
                        )));
                    }
                    Some(_) => {}
                    None if command.args.iter().any(|spec| spec.name == *key) => {
                        // Auto-fill the caller's own group so the agent need not pass it.
                        args.insert(
                            (*key).to_owned(),
                            Value::String(caller_group.as_str().to_owned()),
                        );
                    }
                    None => {}
                }
            }
            Ok(args)
        }
    }
}

fn forbidden(message: impl Into<String>) -> FrameError {
    FrameError {
        code: ErrorCode::Forbidden,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::registry::{Access, ArgKind, ArgSpec, CommandDef, Handler};

    const NOOP: Handler = |_args, _caller, _db| Ok(Value::Null);

    fn command(resource: &'static str, args: &'static [ArgSpec]) -> CommandDef {
        CommandDef {
            name: "x-y",
            summary: "",
            resource,
            access: Access::Open,
            args,
            handler: NOOP,
        }
    }

    fn args(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned())))
            .collect()
    }

    const ID_ARG: &[ArgSpec] = &[ArgSpec {
        name: "id",
        label: "id",
        kind: ArgKind::Text,
        required: false,
    }];

    fn group() -> AgentGroupId {
        AgentGroupId::new("ag-self")
    }

    #[test]
    fn disabled_blocks_everything() {
        let result = enforce_cli_scope(
            CliScope::Disabled,
            &command("groups", &[]),
            &group(),
            Map::new(),
        );
        assert_eq!(result.unwrap_err().code, ErrorCode::Forbidden);
    }

    #[test]
    fn global_allows_anything_unchanged() {
        let original = args(&[("id", "ag-other")]);
        let result = enforce_cli_scope(
            CliScope::Global,
            &command("endpoints", ID_ARG),
            &group(),
            original.clone(),
        );
        assert_eq!(result.expect("allowed"), original);
    }

    #[test]
    fn group_scope_rejects_non_whitelisted_resources() {
        let result = enforce_cli_scope(
            CliScope::Group,
            &command("endpoints", &[]),
            &group(),
            Map::new(),
        );
        assert_eq!(result.unwrap_err().code, ErrorCode::Forbidden);
    }

    #[test]
    fn group_scope_autofills_the_callers_own_group() {
        let filled = enforce_cli_scope(
            CliScope::Group,
            &command("groups", ID_ARG),
            &group(),
            Map::new(),
        )
        .expect("allowed");
        assert_eq!(filled["id"], "ag-self");
    }

    #[test]
    fn group_scope_rejects_acting_on_another_group() {
        let result = enforce_cli_scope(
            CliScope::Group,
            &command("groups", ID_ARG),
            &group(),
            args(&[("id", "ag-other")]),
        );
        assert_eq!(result.unwrap_err().code, ErrorCode::Forbidden);
    }

    #[test]
    fn group_scope_blocks_cli_scope_changes() {
        const SCOPE_ARG: &[ArgSpec] = &[ArgSpec {
            name: "cli_scope",
            label: "scope",
            kind: ArgKind::Text,
            required: false,
        }];
        let result = enforce_cli_scope(
            CliScope::Group,
            &command("groups", SCOPE_ARG),
            &group(),
            args(&[("cli_scope", "global")]),
        );
        assert_eq!(result.unwrap_err().code, ErrorCode::Forbidden);
    }
}
