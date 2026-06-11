use serde_json::{Map, Value, json};

use crate::commands::CallerContext;
use crate::commands::registry::{
    Access, ArgKind, ArgSpec, CommandDef, handler_error, invalid, not_found, opt_str, require_str,
};
use crate::db::agent_groups::{self, AgentGroup};
use crate::db::endpoints::{self, Endpoint};
use crate::db::{CentralDb, messaging_groups, users};
use crate::protocol::entities::{AgentProviderKind, CliScope, Role, ToolProfile};
use crate::protocol::frame::FrameError;
use crate::protocol::ids::{AgentGroupId, EndpointName, UserId};

/// Every registered command. Adding a resource means adding its definitions here.
#[must_use]
pub fn all() -> Vec<CommandDef> {
    let mut commands = endpoint_commands();
    commands.extend(group_commands());
    commands.extend(wiring_commands());
    commands.extend(role_commands());
    commands
}

// ── endpoints ───────────────────────────────────────────────────────────

fn endpoint_json(endpoint: &Endpoint) -> Value {
    // Never echo the stored key; report only whether one is configured.
    json!({
        "name": endpoint.name,
        "base_url": endpoint.base_url,
        "has_api_key": endpoint.api_key.is_some(),
        "api_key_env": endpoint.api_key_env,
        "notes": endpoint.notes,
    })
}

fn endpoint_commands() -> Vec<CommandDef> {
    const NAME: ArgSpec = ArgSpec {
        name: "name",
        label: "Name",
        kind: ArgKind::Text,
        required: true,
    };
    vec![
        CommandDef {
            name: "endpoints-list",
            summary: "List OpenAI-compatible inference endpoints",
            resource: "endpoints",
            access: Access::Open,
            args: &[],
            handler: |_args, _caller, db| {
                let rows = db.with(endpoints::list).map_err(handler_error)?;
                Ok(Value::Array(rows.iter().map(endpoint_json).collect()))
            },
        },
        CommandDef {
            name: "endpoints-create",
            summary: "Add an inference endpoint",
            resource: "endpoints",
            access: Access::Open,
            args: &[
                NAME,
                ArgSpec {
                    name: "base_url",
                    label: "Base URL",
                    kind: ArgKind::Text,
                    required: true,
                },
                ArgSpec {
                    name: "api_key",
                    label: "API key",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "api_key_env",
                    label: "API key env var",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "notes",
                    label: "Notes",
                    kind: ArgKind::Text,
                    required: false,
                },
            ],
            handler: endpoints_create,
        },
        CommandDef {
            name: "endpoints-update",
            summary: "Update an inference endpoint",
            resource: "endpoints",
            access: Access::Open,
            args: &[
                NAME,
                ArgSpec {
                    name: "base_url",
                    label: "Base URL",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "api_key",
                    label: "API key",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "api_key_env",
                    label: "API key env var",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "notes",
                    label: "Notes",
                    kind: ArgKind::Text,
                    required: false,
                },
            ],
            handler: endpoints_update,
        },
        CommandDef {
            name: "endpoints-delete",
            summary: "Delete an inference endpoint",
            resource: "endpoints",
            // Destructive: an agent must get owner approval; the operator (Host) is unaffected.
            access: Access::Approval,
            args: &[NAME],
            handler: |args, _caller, db| {
                let name = EndpointName::new(require_str(args, "name")?);
                let deleted = db
                    .with(|conn| endpoints::delete(conn, &name))
                    .map_err(handler_error)?;
                if deleted {
                    Ok(json!({ "deleted": name }))
                } else {
                    Err(not_found(format!("no endpoint {name:?}")))
                }
            },
        },
    ]
}

fn endpoints_create(
    args: &Map<String, Value>,
    _caller: &CallerContext,
    db: &CentralDb,
) -> Result<Value, FrameError> {
    let name = EndpointName::new(require_str(args, "name")?);
    let base_url = require_str(args, "base_url")?;
    let api_key = opt_str(args, "api_key");
    let api_key_env = opt_str(args, "api_key_env");
    let notes = opt_str(args, "notes");
    let created = db
        .with(|conn| {
            let mut endpoint = endpoints::create(conn, &name, &base_url)?;
            endpoint.api_key = api_key;
            endpoint.api_key_env = api_key_env;
            endpoint.notes = notes;
            endpoints::update(conn, &endpoint)?;
            Ok(endpoint)
        })
        .map_err(handler_error)?;
    Ok(endpoint_json(&created))
}

fn endpoints_update(
    args: &Map<String, Value>,
    _caller: &CallerContext,
    db: &CentralDb,
) -> Result<Value, FrameError> {
    let name = EndpointName::new(require_str(args, "name")?);
    db.with(|conn| {
        let Some(mut endpoint) = endpoints::get(conn, &name)? else {
            return Ok(None);
        };
        if let Some(base_url) = opt_str(args, "base_url") {
            endpoint.base_url = base_url;
        }
        if let Some(api_key) = opt_str(args, "api_key") {
            endpoint.api_key = Some(api_key);
        }
        if let Some(api_key_env) = opt_str(args, "api_key_env") {
            endpoint.api_key_env = Some(api_key_env);
        }
        if let Some(notes) = opt_str(args, "notes") {
            endpoint.notes = Some(notes);
        }
        endpoints::update(conn, &endpoint)?;
        Ok(Some(endpoint))
    })
    .map_err(handler_error)?
    .map(|endpoint| endpoint_json(&endpoint))
    .ok_or_else(|| not_found(format!("no endpoint {name:?}")))
}

// ── agent groups ────────────────────────────────────────────────────────

fn group_json(group: &AgentGroup) -> Value {
    json!({
        "id": group.id,
        "name": group.name,
        "folder": group.folder,
        "agent_provider": group.agent_provider.map(|p| p.as_str()),
        "endpoint": group.endpoint,
        "model": group.model,
        "tool_profile": group.tool_profile.as_str(),
        "cli_scope": group.cli_scope.as_str(),
    })
}

fn group_commands() -> Vec<CommandDef> {
    vec![
        CommandDef {
            name: "groups-list",
            summary: "List agent groups",
            resource: "groups",
            access: Access::Open,
            args: &[],
            handler: |_args, _caller, db| {
                let rows = db.with(agent_groups::list).map_err(handler_error)?;
                Ok(Value::Array(rows.iter().map(group_json).collect()))
            },
        },
        CommandDef {
            name: "groups-get",
            summary: "Show one agent group",
            resource: "groups",
            access: Access::Open,
            args: &[ArgSpec {
                name: "id",
                label: "Group id",
                kind: ArgKind::Text,
                required: true,
            }],
            handler: |args, _caller, db| {
                let id = AgentGroupId::new(require_str(args, "id")?);
                db.with(|conn| agent_groups::get(conn, &id))
                    .map_err(handler_error)?
                    .map(|group| group_json(&group))
                    .ok_or_else(|| not_found(format!("no group {id}")))
            },
        },
        CommandDef {
            name: "groups-update",
            summary: "Update an agent group's provider/endpoint/model/profile",
            resource: "groups",
            access: Access::Open,
            args: &[
                ArgSpec {
                    name: "id",
                    label: "Group id",
                    kind: ArgKind::Text,
                    required: true,
                },
                ArgSpec {
                    name: "name",
                    label: "Name",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "agent_provider",
                    label: "Provider",
                    kind: ArgKind::Enum(&["native", "echo"]),
                    required: false,
                },
                ArgSpec {
                    name: "endpoint",
                    label: "Endpoint",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "model",
                    label: "Model",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "tool_profile",
                    label: "Tool profile",
                    kind: ArgKind::Enum(&["chat", "coder"]),
                    required: false,
                },
                ArgSpec {
                    name: "cli_scope",
                    label: "CLI scope",
                    kind: ArgKind::Enum(&["disabled", "group", "global"]),
                    required: false,
                },
            ],
            handler: groups_update,
        },
    ]
}

fn groups_update(
    args: &Map<String, Value>,
    _caller: &CallerContext,
    db: &CentralDb,
) -> Result<Value, FrameError> {
    let id = AgentGroupId::new(require_str(args, "id")?);
    let provider = parse_enum::<AgentProviderKind>(args, "agent_provider")?;
    let profile = parse_enum::<ToolProfile>(args, "tool_profile")?;
    let scope = parse_enum::<CliScope>(args, "cli_scope")?;

    db.with(|conn| {
        let Some(mut group) = agent_groups::get(conn, &id)? else {
            return Ok(None);
        };
        if let Some(name) = opt_str(args, "name") {
            group.name = name;
        }
        if let Some(provider) = provider {
            group.agent_provider = Some(provider);
        }
        if let Some(endpoint) = opt_str(args, "endpoint") {
            group.endpoint = Some(EndpointName::new(endpoint));
        }
        if let Some(model) = opt_str(args, "model") {
            group.model = Some(model);
        }
        if let Some(profile) = profile {
            group.tool_profile = profile;
        }
        if let Some(scope) = scope {
            group.cli_scope = scope;
        }
        agent_groups::update(conn, &group)?;
        Ok(Some(group))
    })
    .map_err(handler_error)?
    .map(|group| group_json(&group))
    .ok_or_else(|| not_found(format!("no group {id}")))
}

// ── wirings (messaging group ↔ agent group) ─────────────────────────────

fn wiring_commands() -> Vec<CommandDef> {
    vec![CommandDef {
        name: "wirings-list",
        summary: "List which agent groups handle which chats",
        resource: "wirings",
        access: Access::Open,
        args: &[],
        handler: |_args, _caller, db| {
            let rows = db
                .with(|conn| {
                    let mut out = Vec::new();
                    for group in messaging_groups::list(conn)? {
                        for wiring in messaging_groups::wirings_for(conn, &group.id)? {
                            out.push(json!({
                                "messaging_group": group.platform_id,
                                "channel": group.channel_type,
                                "agent_group": wiring.agent_group_id,
                                "engage_mode": wiring.engage_mode.as_str(),
                                "session_mode": wiring.session_mode.as_str(),
                            }));
                        }
                    }
                    Ok(out)
                })
                .map_err(handler_error)?;
            Ok(Value::Array(rows))
        },
    }]
}

// ── roles (owner / admin grants) ────────────────────────────────────────

fn role_commands() -> Vec<CommandDef> {
    const USER: ArgSpec = ArgSpec {
        name: "user",
        label: "User id (e.g. web:owner)",
        kind: ArgKind::Text,
        required: true,
    };
    const ROLE: ArgSpec = ArgSpec {
        name: "role",
        label: "Role",
        kind: ArgKind::Enum(&["owner", "admin"]),
        required: true,
    };
    const SCOPE: ArgSpec = ArgSpec {
        name: "agent_group",
        label: "Agent group (admin only; omit for global)",
        kind: ArgKind::Text,
        required: false,
    };
    vec![
        CommandDef {
            name: "roles-list",
            summary: "List a user's role grants",
            resource: "roles",
            access: Access::Open,
            args: &[USER],
            handler: |args, _caller, db| {
                let user = UserId::new(require_str(args, "user")?);
                let grants = db
                    .with(|conn| users::roles_for(conn, &user))
                    .map_err(handler_error)?;
                Ok(Value::Array(
                    grants
                        .iter()
                        .map(|grant| {
                            json!({
                                "role": grant.role.as_str(),
                                "agent_group": grant.agent_group_id,
                            })
                        })
                        .collect(),
                ))
            },
        },
        CommandDef {
            name: "roles-grant",
            summary: "Grant owner (global) or admin (global or scoped to a group)",
            resource: "roles",
            access: Access::Hidden, // operator-only — agents must not grant themselves power
            args: &[USER, ROLE, SCOPE],
            handler: roles_grant,
        },
        CommandDef {
            name: "roles-revoke",
            summary: "Revoke a role grant",
            resource: "roles",
            access: Access::Hidden,
            args: &[USER, ROLE, SCOPE],
            handler: roles_revoke,
        },
    ]
}

fn roles_grant(
    args: &Map<String, Value>,
    _caller: &CallerContext,
    db: &CentralDb,
) -> Result<Value, FrameError> {
    let user = UserId::new(require_str(args, "user")?);
    let role: Role = require_str(args, "role")?
        .parse()
        .map_err(|_| invalid("role must be owner or admin"))?;
    let scope = opt_str(args, "agent_group").map(AgentGroupId::new);
    if role == Role::Owner && scope.is_some() {
        return Err(invalid("owner is global; do not pass agent_group"));
    }
    db.with(|conn| {
        users::upsert(
            conn,
            &user,
            user.as_str().split(':').next().unwrap_or("web"),
            None,
        )?;
        users::grant_role(conn, &user, role, scope.as_ref(), None)
    })
    .map_err(handler_error)?;
    Ok(json!({ "granted": role.as_str(), "user": user }))
}

fn roles_revoke(
    args: &Map<String, Value>,
    _caller: &CallerContext,
    db: &CentralDb,
) -> Result<Value, FrameError> {
    let user = UserId::new(require_str(args, "user")?);
    let role: Role = require_str(args, "role")?
        .parse()
        .map_err(|_| invalid("role must be owner or admin"))?;
    let scope = opt_str(args, "agent_group").map(AgentGroupId::new);
    let revoked = db
        .with(|conn| users::revoke_role(conn, &user, role, scope.as_ref()))
        .map_err(handler_error)?;
    if revoked {
        Ok(json!({ "revoked": role.as_str(), "user": user }))
    } else {
        Err(not_found("no such grant"))
    }
}

/// Parses an optional kebab/text enum argument, surfacing a clear error.
fn parse_enum<T: std::str::FromStr>(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<T>, FrameError> {
    match opt_str(args, key) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<T>()
            .map(Some)
            .map_err(|_| invalid(format!("invalid value {raw:?} for {key}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Dispatcher;
    use crate::commands::registry::Registry;
    use crate::protocol::frame::{ErrorCode, RequestFrame};
    use std::sync::Arc;

    fn registry() -> Registry {
        Registry::new(Arc::new(CentralDb::open_in_memory().expect("central")))
    }

    fn registry_with_group() -> (Registry, String) {
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let id = central
            .with(|conn| agent_groups::create(conn, "Andy", "andy").map(|g| g.id))
            .expect("group");
        (Registry::new(central), id.to_string())
    }

    fn args(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned())))
            .collect()
    }

    #[tokio::test]
    async fn endpoint_crud_round_trips_through_dispatch() {
        let registry = registry();

        let created = registry
            .dispatch(
                RequestFrame::new(
                    "1",
                    "endpoints-create",
                    args(&[
                        ("name", "openrouter"),
                        ("base_url", "https://openrouter.ai/api/v1"),
                        ("api_key", "sk-secret"),
                    ]),
                ),
                CallerContext::Host,
            )
            .await;
        assert!(created.ok);
        let data = created.data.expect("data");
        assert_eq!(data["name"], "openrouter");
        assert_eq!(data["has_api_key"], true);
        assert!(
            data.as_object()
                .map(|o| !o.contains_key("api_key"))
                .unwrap_or(false),
            "raw key must never be echoed"
        );

        let listed = registry
            .dispatch(
                RequestFrame::new("2", "endpoints-list", Map::new()),
                CallerContext::Host,
            )
            .await;
        assert_eq!(
            listed.data.expect("data").as_array().expect("array").len(),
            1
        );

        let deleted = registry
            .dispatch(
                RequestFrame::new("3", "endpoints-delete", args(&[("name", "openrouter")])),
                CallerContext::Host,
            )
            .await;
        assert!(deleted.ok);
    }

    #[tokio::test]
    async fn missing_required_argument_is_an_invalid_args_error() {
        let response = registry()
            .dispatch(
                RequestFrame::new("1", "endpoints-create", Map::new()),
                CallerContext::Host,
            )
            .await;
        assert_eq!(response.error.expect("error").code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn unknown_command_is_reported() {
        let response = registry()
            .dispatch(
                RequestFrame::new("1", "nope-nope", Map::new()),
                CallerContext::Host,
            )
            .await;
        assert_eq!(
            response.error.expect("error").code,
            ErrorCode::UnknownCommand
        );
    }

    #[tokio::test]
    async fn groups_update_points_a_group_at_a_model() {
        let (registry, id) = registry_with_group();

        let updated = registry
            .dispatch(
                RequestFrame::new(
                    "2",
                    "groups-update",
                    args(&[
                        ("id", &id),
                        ("agent_provider", "native"),
                        ("model", "gemma4-moe"),
                        ("tool_profile", "coder"),
                    ]),
                ),
                CallerContext::Host,
            )
            .await;
        assert!(updated.ok, "{:?}", updated.error);
        let data = updated.data.expect("data");
        assert_eq!(data["model"], "gemma4-moe");
        assert_eq!(data["tool_profile"], "coder");

        let bad = registry
            .dispatch(
                RequestFrame::new(
                    "3",
                    "groups-update",
                    args(&[("id", &id), ("tool_profile", "wizard")]),
                ),
                CallerContext::Host,
            )
            .await;
        assert_eq!(bad.error.expect("error").code, ErrorCode::InvalidArgs);
    }
}
