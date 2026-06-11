pub mod echo;
pub mod native;
pub mod resolution;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::protocol::entities::AgentProviderKind;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("failed to start agent run: {0}")]
    Spawn(String),
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
