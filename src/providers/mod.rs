pub mod echo;
pub mod native;
pub mod pi;
pub mod resolution;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::protocol::entities::AgentProviderKind;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider {0} is not available in this build")]
    Unavailable(AgentProviderKind),
    #[error("failed to start agent process: {0}")]
    Spawn(String),
}

#[derive(Debug, Clone)]
pub struct QueryInput {
    pub prompt: String,
    pub cwd: PathBuf,
    pub session_dir: PathBuf,
    pub model: Option<String>,
    pub system_context: Option<String>,
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
        AgentProviderKind::Native | AgentProviderKind::Pi => Err(ProviderError::Unavailable(kind)),
    }
}
