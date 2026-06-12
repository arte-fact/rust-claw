use serde_json::{Map, Value, json};

use crate::commands::CallerContext;
use crate::commands::registry::{
    Access, ArgKind, ArgSpec, CommandDef, handler_error, invalid, not_found, opt_str, require_str,
};
use crate::db::{CentralDb, agent_groups, connectors};
use crate::protocol::entities::{ConnectorConfig, ConnectorKind, SmsConnectorConfig};
use crate::protocol::frame::FrameError;
use crate::protocol::ids::{AgentGroupId, ConnectorId};

const KIND_VALUES: &[&str] = &["sms"];

/// One row per external channel instance, assigned to one agent group (§10).
/// Secrets are write-only: responses report presence, never the value.
#[must_use]
pub fn commands() -> Vec<CommandDef> {
    const ID: ArgSpec = ArgSpec {
        name: "id",
        label: "Connector id",
        kind: ArgKind::Text,
        required: true,
    };
    const LABEL: ArgSpec = ArgSpec {
        name: "label",
        label: "Label",
        kind: ArgKind::Text,
        required: false,
    };
    const AGENT_GROUP: ArgSpec = ArgSpec {
        name: "agent_group",
        label: "Assigned agent group id",
        kind: ArgKind::Text,
        required: true,
    };
    const TOKEN: ArgSpec = ArgSpec {
        name: "token",
        label: "API token (read+send scopes)",
        kind: ArgKind::Text,
        required: true,
    };
    const WEBHOOK_SECRET: ArgSpec = ArgSpec {
        name: "webhook_secret",
        label: "Webhook secret (wake-up hook)",
        kind: ArgKind::Text,
        required: false,
    };
    vec![
        CommandDef {
            name: "connectors-list",
            summary: "List external channel connectors",
            resource: "connectors",
            access: Access::Open,
            args: &[],
            handler: |_args, _caller, db| {
                let rows = db.with(connectors::list).map_err(handler_error)?;
                Ok(Value::Array(rows.iter().map(connector_json).collect()))
            },
        },
        CommandDef {
            name: "connectors-create",
            summary: "Add an external channel and assign it to an agent group",
            resource: "connectors",
            access: Access::Open,
            args: &[
                ArgSpec {
                    name: "kind",
                    label: "Channel",
                    kind: ArgKind::Enum(KIND_VALUES),
                    required: true,
                },
                AGENT_GROUP,
                LABEL,
                ArgSpec {
                    name: "base_url",
                    label: "Base URL",
                    kind: ArgKind::Text,
                    required: true,
                },
                TOKEN,
                WEBHOOK_SECRET,
            ],
            handler: connectors_create,
        },
        CommandDef {
            name: "connectors-update",
            summary: "Reassign, reconfigure, or enable/disable a connector",
            resource: "connectors",
            access: Access::Open,
            args: &[
                ID,
                ArgSpec {
                    name: "agent_group",
                    label: "Assigned agent group id",
                    kind: ArgKind::Text,
                    required: false,
                },
                LABEL,
                ArgSpec {
                    name: "base_url",
                    label: "Base URL",
                    kind: ArgKind::Text,
                    required: false,
                },
                ArgSpec {
                    name: "token",
                    label: "API token (read+send scopes)",
                    kind: ArgKind::Text,
                    required: false,
                },
                WEBHOOK_SECRET,
                ArgSpec {
                    name: "enabled",
                    label: "Enabled",
                    kind: ArgKind::Enum(&["true", "false"]),
                    required: false,
                },
            ],
            handler: connectors_update,
        },
        CommandDef {
            name: "connectors-delete",
            summary: "Delete a connector (its channel goes dark)",
            resource: "connectors",
            // Destructive: an agent must get owner approval; the operator is unaffected.
            access: Access::Approval,
            args: &[ID],
            handler: |args, _caller, db| {
                let id = ConnectorId::new(require_str(args, "id")?);
                let deleted = db
                    .with(|conn| connectors::delete(conn, &id))
                    .map_err(handler_error)?;
                if deleted {
                    Ok(json!({ "deleted": id }))
                } else {
                    Err(not_found(format!("no connector {id}")))
                }
            },
        },
    ]
}

fn connector_json(connector: &connectors::Connector) -> Value {
    let (base_url, has_token, has_webhook_secret) = match &connector.config {
        ConnectorConfig::Sms(config) => (
            config.base_url.as_str(),
            !config.token.is_empty(),
            config.webhook_secret.is_some(),
        ),
    };
    json!({
        "id": connector.id,
        "kind": connector.kind().as_str(),
        "label": connector.label,
        "base_url": base_url,
        "has_token": has_token,
        "has_webhook_secret": has_webhook_secret,
        "agent_group": connector.agent_group_id,
        "enabled": connector.enabled,
    })
}

fn connectors_create(
    args: &Map<String, Value>,
    _caller: &CallerContext,
    db: &CentralDb,
) -> Result<Value, FrameError> {
    let kind: ConnectorKind = require_str(args, "kind")?
        .parse()
        .map_err(|_| invalid("unknown connector kind"))?;
    let agent_group = AgentGroupId::new(require_str(args, "agent_group")?);
    let label = opt_str(args, "label");
    let config = match kind {
        ConnectorKind::Sms => ConnectorConfig::Sms(SmsConnectorConfig {
            base_url: require_str(args, "base_url")?,
            token: require_str(args, "token")?,
            webhook_secret: opt_str(args, "webhook_secret"),
        }),
    };
    db.with(|conn| {
        if agent_groups::get(conn, &agent_group)?.is_none() {
            return Ok(None);
        }
        connectors::create(conn, &config, &agent_group, label.as_deref()).map(Some)
    })
    .map_err(handler_error)?
    .map(|connector| connector_json(&connector))
    .ok_or_else(|| not_found(format!("no agent group {agent_group}")))
}

fn connectors_update(
    args: &Map<String, Value>,
    _caller: &CallerContext,
    db: &CentralDb,
) -> Result<Value, FrameError> {
    let id = ConnectorId::new(require_str(args, "id")?);
    let enabled = opt_flag(args, "enabled")?;
    db.with(|conn| {
        let Some(mut connector) = connectors::get(conn, &id)? else {
            return Ok(None);
        };
        if let Some(label) = opt_str(args, "label") {
            connector.label = Some(label);
        }
        if let Some(agent_group) = opt_str(args, "agent_group") {
            connector.agent_group_id = AgentGroupId::new(agent_group);
        }
        if let Some(enabled) = enabled {
            connector.enabled = enabled;
        }
        match &mut connector.config {
            ConnectorConfig::Sms(config) => {
                if let Some(base_url) = opt_str(args, "base_url") {
                    config.base_url = base_url;
                }
                if let Some(token) = opt_str(args, "token") {
                    config.token = token;
                }
                if let Some(secret) = opt_str(args, "webhook_secret") {
                    config.webhook_secret = Some(secret);
                }
            }
        }
        connectors::update(conn, &connector)?;
        Ok(Some(connector))
    })
    .map_err(handler_error)?
    .map(|connector| connector_json(&connector))
    .ok_or_else(|| not_found(format!("no connector {id}")))
}

/// `enabled` arrives as a JSON bool from the web form and as text from the CLI.
fn opt_flag(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, FrameError> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(Value::String(raw)) => match raw.as_str() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(invalid(format!("invalid value {raw:?} for {key}"))),
        },
        Some(other) => Err(invalid(format!("invalid value {other} for {key}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Dispatcher;
    use crate::commands::registry::Registry;
    use crate::protocol::frame::{ErrorCode, RequestFrame};
    use std::sync::Arc;

    fn registry_with_group() -> (Registry, String) {
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let id = central
            .with(|conn| agent_groups::create(conn, "Andy", "andy").map(|group| group.id))
            .expect("group");
        (Registry::new(central), id.to_string())
    }

    fn args(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), Value::String((*value).to_owned())))
            .collect()
    }

    async fn create_sms(registry: &Registry, group: &str) -> Value {
        let created = registry
            .dispatch(
                RequestFrame::new(
                    "1",
                    "connectors-create",
                    args(&[
                        ("kind", "sms"),
                        ("agent_group", group),
                        ("base_url", "http://sim:8080"),
                        ("token", "sms_secret"),
                    ]),
                ),
                CallerContext::Host,
            )
            .await;
        assert!(created.ok, "{:?}", created.error);
        created.data.expect("data")
    }

    #[tokio::test]
    async fn connector_crud_round_trips_and_never_echoes_secrets() {
        let (registry, group) = registry_with_group();

        let created = create_sms(&registry, &group).await;
        assert_eq!(created["kind"], "sms");
        assert_eq!(created["agent_group"], group);
        assert_eq!(created["enabled"], true);
        assert_eq!(created["has_token"], true);
        assert_eq!(created["has_webhook_secret"], false);
        let object = created.as_object().expect("object");
        assert!(
            !object.contains_key("token") && !object.contains_key("webhook_secret"),
            "secrets must never be echoed"
        );
        let id = created["id"].as_str().expect("id").to_owned();

        let updated = registry
            .dispatch(
                RequestFrame::new(
                    "2",
                    "connectors-update",
                    args(&[
                        ("id", &id),
                        ("enabled", "false"),
                        ("webhook_secret", "hook-secret"),
                    ]),
                ),
                CallerContext::Host,
            )
            .await;
        assert!(updated.ok, "{:?}", updated.error);
        let data = updated.data.expect("data");
        assert_eq!(data["enabled"], false);
        assert_eq!(data["has_webhook_secret"], true);

        let listed = registry
            .dispatch(
                RequestFrame::new("3", "connectors-list", Map::new()),
                CallerContext::Host,
            )
            .await;
        assert_eq!(
            listed.data.expect("data").as_array().expect("array").len(),
            1
        );

        let deleted = registry
            .dispatch(
                RequestFrame::new("4", "connectors-delete", args(&[("id", &id)])),
                CallerContext::Host,
            )
            .await;
        assert!(deleted.ok);
    }

    #[tokio::test]
    async fn creating_for_an_unknown_agent_group_is_not_found() {
        let (registry, _group) = registry_with_group();
        let response = registry
            .dispatch(
                RequestFrame::new(
                    "1",
                    "connectors-create",
                    args(&[
                        ("kind", "sms"),
                        ("agent_group", "ag-ghost"),
                        ("base_url", "http://sim:8080"),
                        ("token", "sms_secret"),
                    ]),
                ),
                CallerContext::Host,
            )
            .await;
        assert_eq!(response.error.expect("error").code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn a_bad_enabled_value_is_invalid_args() {
        let (registry, group) = registry_with_group();
        let created = create_sms(&registry, &group).await;
        let id = created["id"].as_str().expect("id").to_owned();
        let response = registry
            .dispatch(
                RequestFrame::new(
                    "2",
                    "connectors-update",
                    args(&[("id", &id), ("enabled", "maybe")]),
                ),
                CallerContext::Host,
            )
            .await;
        assert_eq!(response.error.expect("error").code, ErrorCode::InvalidArgs);
    }
}
