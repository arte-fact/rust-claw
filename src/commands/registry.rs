use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::commands::{CallerContext, Dispatcher};
use crate::db::CentralDb;
use crate::protocol::frame::{ErrorCode, FrameError, RequestFrame, ResponseFrame};

/// A handler does blocking DB work, so the dispatcher runs it on the blocking
/// pool. It is a plain fn pointer (no captured state) to stay `Send + 'static`.
pub type Handler = fn(&Map<String, Value>, &CallerContext, &CentralDb) -> Result<Value, FrameError>;

/// Whether an agent caller may run a command directly (`Open`), needs approval
/// (`Approval`, M7.2), or is operator-only and hidden from agents (`Hidden`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Open,
    Approval,
    Hidden,
}

/// The shape of one argument — drives both light CLI validation and the
/// self-rendering web admin forms (§9.2).
#[derive(Debug, Clone, Copy)]
pub struct ArgSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: ArgKind,
    pub required: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ArgKind {
    Text,
    Bool,
    Enum(&'static [&'static str]),
}

pub struct CommandDef {
    pub name: &'static str,
    pub summary: &'static str,
    pub resource: &'static str,
    pub access: Access,
    pub args: &'static [ArgSpec],
    pub handler: Handler,
}

pub struct Registry {
    commands: BTreeMap<&'static str, CommandDef>,
    central: Arc<CentralDb>,
}

impl Registry {
    #[must_use]
    pub fn new(central: Arc<CentralDb>) -> Self {
        let mut commands = BTreeMap::new();
        for def in super::resources::all() {
            commands.insert(def.name, def);
        }
        Self { commands, central }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&CommandDef> {
        self.commands.get(name)
    }

    /// All commands, for the help listing and the self-rendering admin (§9.2).
    pub fn commands(&self) -> impl Iterator<Item = &CommandDef> {
        self.commands.values()
    }
}

#[async_trait]
impl Dispatcher for Registry {
    async fn dispatch(&self, request: RequestFrame, caller: CallerContext) -> ResponseFrame {
        let id = request.id.clone();
        let Some(def) = self.commands.get(request.command.as_str()) else {
            return ResponseFrame::error(
                id,
                ErrorCode::UnknownCommand,
                format!("unknown command {:?}", request.command),
            );
        };

        let args = match self.gate_for_agent(def, &caller, request.args).await {
            Ok(args) => args,
            Err(error) => {
                return ResponseFrame {
                    id,
                    ok: false,
                    data: None,
                    error: Some(error),
                };
            }
        };

        let handler = def.handler;
        let central = self.central.clone();
        let result = tokio::task::spawn_blocking(move || handler(&args, &caller, &central)).await;
        match result {
            Ok(Ok(data)) => ResponseFrame::ok(id, data),
            Ok(Err(error)) => ResponseFrame {
                id,
                ok: false,
                data: None,
                error: Some(error),
            },
            Err(join) => ResponseFrame::error(id, ErrorCode::HandlerError, join.to_string()),
        }
    }
}

impl Registry {
    /// Host callers pass through; agent callers are gated by their group's
    /// `cli_scope` (and `Hidden` commands are operator-only).
    async fn gate_for_agent(
        &self,
        def: &CommandDef,
        caller: &CallerContext,
        args: Map<String, Value>,
    ) -> Result<Map<String, Value>, FrameError> {
        let CallerContext::Agent { agent_group_id, .. } = caller else {
            return Ok(args);
        };
        if def.access == Access::Hidden {
            return Err(FrameError {
                code: ErrorCode::Forbidden,
                message: "command not available to agents".to_owned(),
            });
        }
        let central = self.central.clone();
        let group = agent_group_id.clone();
        let scope = tokio::task::spawn_blocking(move || {
            central.with(|conn| crate::db::agent_groups::get(conn, &group))
        })
        .await
        .map_err(|join| FrameError {
            code: ErrorCode::HandlerError,
            message: join.to_string(),
        })?
        .map_err(handler_error)?
        .map(|group| group.cli_scope)
        .unwrap_or(crate::protocol::entities::CliScope::Disabled);

        super::gates::enforce_cli_scope(scope, def, agent_group_id, args)
    }
}

// ── argument helpers for handlers ───────────────────────────────────────

pub(super) fn require_str(args: &Map<String, Value>, key: &str) -> Result<String, FrameError> {
    match args.get(key).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_owned()),
        _ => Err(invalid(format!("missing required argument {key:?}"))),
    }
}

pub(super) fn opt_str(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(super) fn invalid(message: impl Into<String>) -> FrameError {
    FrameError {
        code: ErrorCode::InvalidArgs,
        message: message.into(),
    }
}

pub(super) fn not_found(message: impl Into<String>) -> FrameError {
    FrameError {
        code: ErrorCode::NotFound,
        message: message.into(),
    }
}

pub(super) fn handler_error(error: impl std::fmt::Display) -> FrameError {
    FrameError {
        code: ErrorCode::HandlerError,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::agent_groups;
    use crate::protocol::entities::CliScope;
    use crate::protocol::ids::{AgentGroupId, SessionId};

    fn registry_with_group(scope: CliScope) -> (Registry, AgentGroupId) {
        let central = Arc::new(CentralDb::open_in_memory().expect("open"));
        let group_id = central
            .with(|conn| {
                let mut group = agent_groups::create(conn, "coder", "coder")?;
                group.cli_scope = scope;
                agent_groups::update(conn, &group)?;
                Ok(group.id)
            })
            .expect("seed group");
        (Registry::new(central), group_id)
    }

    fn agent_caller(group_id: &AgentGroupId) -> CallerContext {
        CallerContext::Agent {
            session_id: SessionId::new("s-test"),
            agent_group_id: group_id.clone(),
            messaging_group_id: None,
        }
    }

    fn request(command: &str, args: &[(&str, &str)]) -> RequestFrame {
        RequestFrame {
            id: "req-1".to_owned(),
            command: command.to_owned(),
            args: args
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned())))
                .collect(),
        }
    }

    #[tokio::test]
    async fn agent_with_group_scope_may_not_touch_endpoints() {
        let (registry, group_id) = registry_with_group(CliScope::Group);
        let response = registry
            .dispatch(request("endpoints-list", &[]), agent_caller(&group_id))
            .await;
        assert_eq!(response.error.expect("denied").code, ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn agent_with_group_scope_is_confined_to_its_own_group() {
        let (registry, group_id) = registry_with_group(CliScope::Group);
        let response = registry
            .dispatch(
                request("groups-update", &[("id", "ag-someone-else")]),
                agent_caller(&group_id),
            )
            .await;
        assert_eq!(response.error.expect("denied").code, ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn agent_may_not_run_hidden_role_commands() {
        let (registry, group_id) = registry_with_group(CliScope::Global);
        let response = registry
            .dispatch(
                request("roles-grant", &[("user", "web:x"), ("role", "owner")]),
                agent_caller(&group_id),
            )
            .await;
        assert_eq!(response.error.expect("denied").code, ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn disabled_scope_blocks_even_open_commands() {
        let (registry, group_id) = registry_with_group(CliScope::Disabled);
        let response = registry
            .dispatch(request("groups-list", &[]), agent_caller(&group_id))
            .await;
        assert_eq!(response.error.expect("denied").code, ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn host_caller_bypasses_scope_gating() {
        let (registry, _group_id) = registry_with_group(CliScope::Disabled);
        let response = registry
            .dispatch(request("groups-list", &[]), CallerContext::Host)
            .await;
        assert!(response.ok, "host must not be scope-gated");
    }
}
