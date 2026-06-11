pub mod registry;
mod resources;

pub use registry::{Access, ArgKind, ArgSpec, CommandDef, Registry};

use async_trait::async_trait;

use crate::protocol::frame::{RequestFrame, ResponseFrame};
use crate::protocol::ids::{AgentGroupId, MessagingGroupId, SessionId};

/// Who is issuing a command. The socket server speaks for the operator (`Host`);
/// the agent-side transport (M6.3) speaks for a specific run (`Agent`), which the
/// dispatcher scope-gates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallerContext {
    Host,
    Agent {
        session_id: SessionId,
        agent_group_id: AgentGroupId,
        messaging_group_id: Option<MessagingGroupId>,
    },
}

impl CallerContext {
    #[must_use]
    pub fn is_agent(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }
}

/// Turns a request into a response. Implemented by the command registry (M6.2);
/// the socket server and the agent transport both drive it.
#[async_trait]
pub trait Dispatcher: Send + Sync {
    async fn dispatch(&self, request: RequestFrame, caller: CallerContext) -> ResponseFrame;
}
