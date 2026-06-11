use serde::Deserialize;
use serde_json::json;

use crate::protocol::content::{OutboundContent, Routing};
use crate::protocol::message::MessageKind;
use crate::session::{NewOutboundMessage, SessionDb};

use super::client::{ToolCall, ToolDefinition};

pub const SEND_MESSAGE: &str = "send_message";
pub const SCHEDULE_TASK: &str = "schedule_task";
pub const SEND_TO_AGENT: &str = "send_to_agent";
pub const ASK_USER_QUESTION: &str = "ask_user_question";

/// Messaging-only tool surface for the native agent (§8.5). No bash/filesystem.
#[must_use]
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            SEND_MESSAGE,
            "Send a chat message back to the user in this conversation.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The message to send." }
                },
                "required": ["text"]
            }),
        ),
        ToolDefinition::function(
            SCHEDULE_TASK,
            "Schedule a prompt to run later, optionally on a recurring cron schedule.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "What to do when the task fires." },
                    "process_after": {
                        "type": "string",
                        "description": "ISO-8601 UTC timestamp for the first run. Omit to run as soon as possible."
                    },
                    "recurrence": {
                        "type": "string",
                        "description": "Cron expression for recurring runs. Omit for a one-shot task."
                    }
                },
                "required": ["prompt"]
            }),
        ),
        ToolDefinition::function(
            SEND_TO_AGENT,
            "Send a message to another agent group, e.g. to delegate work.",
            json!({
                "type": "object",
                "properties": {
                    "agent_group": { "type": "string", "description": "Target agent group id." },
                    "text": { "type": "string", "description": "The message to deliver." }
                },
                "required": ["agent_group", "text"]
            }),
        ),
        ToolDefinition::function(
            ASK_USER_QUESTION,
            "Ask the user a multiple-choice question and wait for their selection.",
            json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string" },
                    "options": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["question", "options"]
            }),
        ),
    ]
}

/// One tool execution. `produced_message` lets the run loop know whether the
/// agent already replied, so the no-tool-call fallback does not double-send.
pub struct ToolOutcome {
    pub result: String,
    pub produced_message: bool,
}

impl ToolOutcome {
    fn note(result: impl Into<String>) -> Self {
        Self {
            result: result.into(),
            produced_message: false,
        }
    }

    fn message(result: impl Into<String>) -> Self {
        Self {
            result: result.into(),
            produced_message: true,
        }
    }
}

/// Executes a tool call against the session DB. Tool errors become result
/// strings (fed back to the model) rather than failing the whole turn.
pub fn dispatch(db: &SessionDb, call: &ToolCall) -> ToolOutcome {
    match call.function.name.as_str() {
        SEND_MESSAGE => send_message(db, &call.function.arguments),
        SCHEDULE_TASK => schedule_task(db, &call.function.arguments),
        SEND_TO_AGENT => send_to_agent(db, &call.function.arguments),
        ASK_USER_QUESTION => {
            ToolOutcome::note("ask_user_question is not available yet; ask in a normal message.")
        }
        other => ToolOutcome::note(format!("error: unknown tool {other}")),
    }
}

/// Writes a plain reply directly — the no-tool-call fallback path.
pub fn send_text(db: &SessionDb, text: &str) -> Result<(), String> {
    let content = OutboundContent::from_text(text);
    let body = serde_json::to_string(&content).map_err(|err| err.to_string())?;
    db.write_outbound(&NewOutboundMessage::chat(body, Routing::default()))
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[derive(Deserialize)]
struct SendMessageArgs {
    text: String,
}

fn send_message(db: &SessionDb, arguments: &str) -> ToolOutcome {
    let args: SendMessageArgs = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(err) => return ToolOutcome::note(format!("error: bad arguments: {err}")),
    };
    match send_text(db, &args.text) {
        Ok(()) => ToolOutcome::message("sent"),
        Err(err) => ToolOutcome::note(format!("error: {err}")),
    }
}

#[derive(Deserialize)]
struct ScheduleTaskArgs {
    prompt: String,
    #[serde(default)]
    process_after: Option<String>,
    #[serde(default)]
    recurrence: Option<String>,
}

fn schedule_task(db: &SessionDb, arguments: &str) -> ToolOutcome {
    let args: ScheduleTaskArgs = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(err) => return ToolOutcome::note(format!("error: bad arguments: {err}")),
    };
    let payload = json!({
        "action": "schedule_task",
        "prompt": args.prompt,
        "process_after": args.process_after,
        "recurrence": args.recurrence,
    });
    match write_system(db, &payload) {
        Ok(()) => ToolOutcome::note("scheduled"),
        Err(err) => ToolOutcome::note(format!("error: {err}")),
    }
}

#[derive(Deserialize)]
struct SendToAgentArgs {
    agent_group: String,
    text: String,
}

fn send_to_agent(db: &SessionDb, arguments: &str) -> ToolOutcome {
    let args: SendToAgentArgs = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(err) => return ToolOutcome::note(format!("error: bad arguments: {err}")),
    };
    let content = OutboundContent::from_text(args.text);
    let body = match serde_json::to_string(&content) {
        Ok(body) => body,
        Err(err) => return ToolOutcome::note(format!("error: {err}")),
    };
    let routing = Routing {
        channel_type: Some("agent".to_owned()),
        platform_id: Some(args.agent_group),
        thread_id: None,
    };
    match db.write_outbound(&NewOutboundMessage::chat(body, routing)) {
        Ok(_) => ToolOutcome::note("delivered to agent"),
        Err(err) => ToolOutcome::note(format!("error: {err}")),
    }
}

fn write_system(db: &SessionDb, payload: &serde_json::Value) -> Result<(), String> {
    let body = serde_json::to_string(payload).map_err(|err| err.to_string())?;
    let mut message = NewOutboundMessage::chat(body, Routing::default());
    message.kind = MessageKind::System;
    db.write_outbound(&message)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::native::client::FunctionCall;
    use crate::session::test_session_db;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_owned(),
            call_type: "function".to_owned(),
            function: FunctionCall {
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            },
        }
    }

    #[test]
    fn send_message_writes_an_outbound_chat_row() {
        let (_tmp, db) = test_session_db();
        let outcome = dispatch(&db, &call(SEND_MESSAGE, r#"{"text":"hello"}"#));
        assert!(outcome.produced_message);

        let now = db.now_timestamp().expect("now");
        let due = db.due_outbound(&now).expect("due");
        assert_eq!(due.len(), 1);
        let content = OutboundContent::parse(&due[0].content).expect("content");
        assert_eq!(content.text.as_deref(), Some("hello"));
        assert_eq!(due[0].kind, "chat");
    }

    #[test]
    fn schedule_task_writes_a_system_row_without_claiming_a_reply() {
        let (_tmp, db) = test_session_db();
        let outcome = dispatch(
            &db,
            &call(
                SCHEDULE_TASK,
                r#"{"prompt":"daily briefing","recurrence":"0 9 * * *"}"#,
            ),
        );
        assert!(!outcome.produced_message);

        let now = db.now_timestamp().expect("now");
        let due = db.due_outbound(&now).expect("due");
        assert_eq!(due[0].kind, "system");
        let payload: serde_json::Value = serde_json::from_str(&due[0].content).expect("json");
        assert_eq!(payload["action"], "schedule_task");
        assert_eq!(payload["recurrence"], "0 9 * * *");
    }

    #[test]
    fn send_to_agent_routes_to_the_agent_channel() {
        let (_tmp, db) = test_session_db();
        dispatch(
            &db,
            &call(
                SEND_TO_AGENT,
                r#"{"agent_group":"ag-coder","text":"review this"}"#,
            ),
        );
        let now = db.now_timestamp().expect("now");
        let due = db.due_outbound(&now).expect("due");
        assert_eq!(due[0].routing.channel_type.as_deref(), Some("agent"));
        assert_eq!(due[0].routing.platform_id.as_deref(), Some("ag-coder"));
    }

    #[test]
    fn bad_arguments_become_a_tool_result_not_a_panic() {
        let (_tmp, db) = test_session_db();
        let outcome = dispatch(&db, &call(SEND_MESSAGE, "not json"));
        assert!(!outcome.produced_message);
        assert!(outcome.result.starts_with("error:"));
    }

    #[test]
    fn unknown_tool_is_reported_back() {
        let (_tmp, db) = test_session_db();
        let outcome = dispatch(&db, &call("delete_everything", "{}"));
        assert!(outcome.result.contains("unknown tool"));
    }

    #[test]
    fn ask_user_question_is_a_placeholder_for_now() {
        let (_tmp, db) = test_session_db();
        let outcome = dispatch(
            &db,
            &call(ASK_USER_QUESTION, r#"{"question":"?","options":[]}"#),
        );
        assert!(!outcome.produced_message);
        assert!(outcome.result.contains("not available yet"));
    }
}
