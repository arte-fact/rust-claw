pub mod echo;
pub mod native;
pub mod resolution;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::commands::{CallerContext, Dispatcher};
use crate::protocol::entities::AgentProviderKind;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("failed to start agent run: {0}")]
    Spawn(String),
}

/// The capability that lets a run issue admin commands as itself (§8.7, M6.4).
/// Present only when the group's `cli_scope` is not `disabled`; the dispatcher
/// re-checks scope per command, so this is the seam, not the security boundary.
#[derive(Clone)]
pub struct AgentAdmin {
    pub dispatcher: Arc<dyn Dispatcher>,
    pub caller: CallerContext,
}

impl std::fmt::Debug for AgentAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentAdmin")
            .field("caller", &self.caller)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct QueryInput {
    pub prompt: String,
    /// Group workspace (AGENT.md, skills, working files).
    pub cwd: PathBuf,
    /// Session folder root (session.db, inbox/, outbox/).
    pub session_dir: PathBuf,
    pub model: Option<String>,
    pub system_context: Option<String>,
    /// Resolved endpoint+model+key — required by the native provider.
    pub inference: Option<resolution::ResolvedInference>,
    /// Which tool surface the agent gets (§8.5).
    pub tool_profile: crate::protocol::entities::ToolProfile,
    /// In-chat admin access, when the group's `cli_scope` permits it.
    pub admin: Option<AgentAdmin>,
    /// Shared MCP server, when one is connected — its tools go to every agent (M12).
    pub mcp: Option<Arc<crate::mcp::McpClient>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    TurnEnd { text: Option<String> },
    Activity,
    Progress { message: String },
    Error { message: String, retryable: bool },
}

/// A live agent run: push follow-ups in, stream events out, cancel to abort.
pub struct ActiveRun {
    pub input: mpsc::Sender<String>,
    pub events: mpsc::Receiver<ProviderEvent>,
    pub abort: CancellationToken,
}

pub trait AgentProvider: Send + Sync {
    fn start(&self, input: QueryInput) -> Result<ActiveRun, ProviderError>;
}

pub fn create_provider(kind: AgentProviderKind) -> Result<Arc<dyn AgentProvider>, ProviderError> {
    match kind {
        AgentProviderKind::Echo => Ok(Arc::new(echo::EchoProvider)),
        AgentProviderKind::Native => Ok(Arc::new(native::NativeProvider)),
    }
}
