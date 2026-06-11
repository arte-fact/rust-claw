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

        // Agent-caller scope enforcement (disabled/group/global, approvals) lands
        // in M6.3 / M7.2; for now only `Hidden` is operator-only.
        if caller.is_agent() && def.access == Access::Hidden {
            return ResponseFrame::error(
                id,
                ErrorCode::Forbidden,
                "command not available to agents",
            );
        }

        let handler = def.handler;
        let central = self.central.clone();
        let args = request.args;
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
