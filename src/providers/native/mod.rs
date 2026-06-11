pub mod client;
pub mod context;
pub mod tools;

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::SessionDb;

use client::{ChatClient, ChatMessage, ClientError};

use super::{ActiveRun, AgentProvider, ProviderError, ProviderEvent, QueryInput};

const CONTEXT_TOKEN_BUDGET: usize = 12_000;
const TRANSCRIPT_LIMIT: i64 = 400;
/// Cap on assistant↔tool round-trips per turn, so a confused model can't loop forever.
const MAX_TOOL_ROUNDS: usize = 6;

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
                Ok(()) => ProviderEvent::TurnEnd { text: None },
                Err(error) => ProviderEvent::Error {
                    retryable: error.retryable,
                    message: error.message,
                },
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

/// The native agent writes its own outbound rows (via the `send_message` tool or
/// the no-tool-call fallback), so a successful turn returns no text for the
/// supervisor to write.
async fn run_turn(client: &ChatClient, input: &QueryInput) -> Result<(), TurnError> {
    let db = open_session_db(input.session_dir.clone()).await?;
    let system_prompt = read_agent_md(&input.cwd);
    let mut messages = initial_messages(db.clone(), system_prompt).await?;
    let tools = tools::definitions();
    let mut produced_message = false;

    for _ in 0..MAX_TOOL_ROUNDS {
        let completion = client
            .complete(&messages, &tools)
            .await
            .map_err(turn_error)?;

        if completion.tool_calls.is_empty() {
            return finish_with_fallback(&db, completion.content, produced_message).await;
        }

        messages.push(ChatMessage::assistant_tool_calls(
            completion.tool_calls.clone(),
        ));
        let results = run_tools(db.clone(), completion.tool_calls).await?;
        for result in results {
            produced_message |= result.produced_message;
            messages.push(ChatMessage::tool_result(
                result.tool_call_id,
                result.content,
            ));
        }
    }

    // Ran out of rounds — any output the model produced was already written by its tools.
    Ok(())
}

/// No tool calls this round: treat any plain text as the reply, unless a
/// `send_message` tool already produced one earlier in the turn.
async fn finish_with_fallback(
    db: &Arc<SessionDb>,
    content: Option<String>,
    produced_message: bool,
) -> Result<(), TurnError> {
    if produced_message {
        return Ok(());
    }
    let Some(text) = content
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
    else {
        return Ok(());
    };
    let db = db.clone();
    spawn_db(move || tools::send_text(&db, &text)).await
}

struct ToolResult {
    tool_call_id: String,
    content: String,
    produced_message: bool,
}

async fn run_tools(
    db: Arc<SessionDb>,
    calls: Vec<client::ToolCall>,
) -> Result<Vec<ToolResult>, TurnError> {
    tokio::task::spawn_blocking(move || {
        calls
            .iter()
            .map(|call| {
                let outcome = tools::dispatch(&db, call);
                ToolResult {
                    tool_call_id: call.id.clone(),
                    content: outcome.result,
                    produced_message: outcome.produced_message,
                }
            })
            .collect()
    })
    .await
    .map_err(|err| fatal(err.to_string()))
}

async fn open_session_db(session_dir: std::path::PathBuf) -> Result<Arc<SessionDb>, TurnError> {
    tokio::task::spawn_blocking(move || SessionDb::open_dir(session_dir))
        .await
        .map_err(|err| fatal(err.to_string()))?
        .map(Arc::new)
        .map_err(|err| fatal(err.to_string()))
}

async fn initial_messages(
    db: Arc<SessionDb>,
    system_prompt: Option<String>,
) -> Result<Vec<ChatMessage>, TurnError> {
    tokio::task::spawn_blocking(move || {
        db.transcript(TRANSCRIPT_LIMIT).map(|transcript| {
            context::build_messages(system_prompt.as_deref(), &transcript, CONTEXT_TOKEN_BUDGET)
        })
    })
    .await
    .map_err(|err| fatal(err.to_string()))?
    .map_err(|err| fatal(err.to_string()))
}

async fn spawn_db(
    op: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), TurnError> {
    tokio::task::spawn_blocking(op)
        .await
        .map_err(|err| fatal(err.to_string()))?
        .map_err(fatal)
}

fn turn_error(err: ClientError) -> TurnError {
    TurnError {
        retryable: err.is_retryable(),
        message: err.to_string(),
    }
}

fn fatal(message: impl Into<String>) -> TurnError {
    TurnError {
        message: message.into(),
        retryable: false,
    }
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
