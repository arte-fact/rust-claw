pub mod client;
pub mod context;

use std::path::Path;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::SessionDb;

use client::ChatClient;

use super::{ActiveRun, AgentProvider, ProviderError, ProviderEvent, QueryInput};

const CONTEXT_TOKEN_BUDGET: usize = 12_000;
const TRANSCRIPT_LIMIT: i64 = 400;

/// claw's own conversational agent: an in-process chat-completion loop.
/// Memory is the session DB itself — no provider-side state (§8.5).
pub struct NativeProvider;

impl AgentProvider for NativeProvider {
    fn start(&self, input: QueryInput) -> Result<ActiveRun, ProviderError> {
        let inference = input.inference.clone().ok_or_else(|| {
            ProviderError::Spawn("native provider needs resolved inference".to_owned())
        })?;
        let client =
            ChatClient::new(&inference).map_err(|err| ProviderError::Spawn(err.to_string()))?;

        let (input_tx, _input_rx) = mpsc::channel::<String>(1);
        let (event_tx, event_rx) = mpsc::channel::<ProviderEvent>(4);
        let abort = CancellationToken::new();
        let run_abort = abort.clone();

        tokio::spawn(async move {
            let turn = tokio::select! {
                () = run_abort.cancelled() => return,
                turn = run_turn(&client, &input) => turn,
            };
            let event = match turn {
                Ok(text) => ProviderEvent::TurnEnd { text },
                Err(error) => {
                    let retryable = error.retryable;
                    ProviderEvent::Error {
                        message: error.message,
                        retryable,
                    }
                }
            };
            let _ = event_tx.send(event).await;
        });

        Ok(ActiveRun {
            input: input_tx,
            events: event_rx,
            abort,
        })
    }
}

struct TurnError {
    message: String,
    retryable: bool,
}

async fn run_turn(client: &ChatClient, input: &QueryInput) -> Result<Option<String>, TurnError> {
    let session_dir = input.session_dir.clone();
    let messages = {
        let system_prompt = read_agent_md(&input.cwd);
        tokio::task::spawn_blocking(move || {
            let db = SessionDb::open_dir(session_dir).map_err(|err| TurnError {
                message: err.to_string(),
                retryable: false,
            })?;
            let transcript = db.transcript(TRANSCRIPT_LIMIT).map_err(|err| TurnError {
                message: err.to_string(),
                retryable: false,
            })?;
            Ok(context::build_messages(
                system_prompt.as_deref(),
                &transcript,
                CONTEXT_TOKEN_BUDGET,
            ))
        })
        .await
        .map_err(|err| TurnError {
            message: err.to_string(),
            retryable: false,
        })??
    };

    client.complete(&messages).await.map_err(|err| TurnError {
        retryable: err.is_retryable(),
        message: err.to_string(),
    })
}

fn read_agent_md(workspace: &Path) -> Option<String> {
    let content = std::fs::read_to_string(workspace.join("AGENT.md")).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}
