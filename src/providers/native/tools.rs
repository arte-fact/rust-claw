use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use crate::protocol::content::{OutboundContent, Routing};
use crate::protocol::entities::ToolProfile;
use crate::protocol::message::MessageKind;
use crate::session::{NewOutboundMessage, SessionDb};

use super::client::{ToolCall, ToolDefinition};
use super::{exec, files};

pub const SEND_MESSAGE: &str = "send_message";
pub const SCHEDULE_TASK: &str = "schedule_task";
pub const SEND_TO_AGENT: &str = "send_to_agent";
pub const ASK_USER_QUESTION: &str = "ask_user_question";
pub const BASH: &str = "bash";
pub const READ: &str = "read";
pub const WRITE: &str = "write";
pub const EDIT: &str = "edit";

/// What a tool call may touch: the group workspace, gated by the group's profile.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub profile: ToolProfile,
}

/// Tool surface per profile (§8.5): messaging for everyone; bash/files for coders.
#[must_use]
pub fn definitions(profile: ToolProfile) -> Vec<ToolDefinition> {
    let mut tools = messaging_definitions();
    if profile == ToolProfile::Coder {
        tools.extend(coder_definitions());
    }
    tools
}

fn coder_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            BASH,
            "Run a bash command in the agent workspace. Returns exit code and output.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to run." }
                },
                "required": ["command"]
            }),
        ),
        ToolDefinition::function(
            READ,
            "Read a file with line numbers. Paths are relative to the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "1-based first line to show." },
                    "limit": { "type": "integer", "description": "Max lines to show (default 2000)." }
                },
                "required": ["path"]
            }),
        ),
        ToolDefinition::function(
            WRITE,
            "Create or overwrite a file. Parent directories are created.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        ),
        ToolDefinition::function(
            EDIT,
            "Replace an exact string in a file. old_string must match exactly once.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
    ]
}

fn messaging_definitions() -> Vec<ToolDefinition> {
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

/// Executes a tool call. Tool errors become result strings (fed back to the
/// model) rather than failing the whole turn. Profile gating is enforced here
/// too — a model hallucinating `bash` in a chat group gets a refusal result.
pub async fn dispatch(db: &Arc<SessionDb>, context: &ToolContext, call: &ToolCall) -> ToolOutcome {
    let name = call.function.name.as_str();
    let coder_only = matches!(name, BASH | READ | WRITE | EDIT);
    if coder_only && context.profile != ToolProfile::Coder {
        return ToolOutcome::note(format!(
            "error: {name} is not available in this agent group"
        ));
    }
    match name {
        BASH => bash(context, &call.function.arguments).await,
        READ | WRITE | EDIT => {
            let workspace = context.workspace.clone();
            let call = call.clone();
            tokio::task::spawn_blocking(move || dispatch_files(&workspace, &call))
                .await
                .unwrap_or_else(|err| ToolOutcome::note(format!("error: tool task failed: {err}")))
        }
        _ => {
            let db = db.clone();
            let call = call.clone();
            tokio::task::spawn_blocking(move || dispatch_messaging(&db, &call))
                .await
                .unwrap_or_else(|err| ToolOutcome::note(format!("error: tool task failed: {err}")))
        }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

fn dispatch_files(workspace: &std::path::Path, call: &ToolCall) -> ToolOutcome {
    let arguments = &call.function.arguments;
    let result = match call.function.name.as_str() {
        READ => serde_json::from_str::<ReadArgs>(arguments)
            .map(|args| files::read(workspace, &args.path, args.offset, args.limit)),
        WRITE => serde_json::from_str::<WriteArgs>(arguments)
            .map(|args| files::write(workspace, &args.path, &args.content)),
        EDIT => serde_json::from_str::<EditArgs>(arguments)
            .map(|args| files::edit(workspace, &args.path, &args.old_string, &args.new_string)),
        other => return ToolOutcome::note(format!("error: unknown file tool {other}")),
    };
    match result {
        Ok(rendered) => ToolOutcome::note(rendered),
        Err(err) => ToolOutcome::note(format!("error: bad arguments: {err}")),
    }
}

fn dispatch_messaging(db: &SessionDb, call: &ToolCall) -> ToolOutcome {
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

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

async fn bash(context: &ToolContext, arguments: &str) -> ToolOutcome {
    let args: BashArgs = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(err) => return ToolOutcome::note(format!("error: bad arguments: {err}")),
    };
    ToolOutcome::note(exec::bash(&context.workspace, &args.command).await)
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

    fn fixture(profile: ToolProfile) -> (tempfile::TempDir, Arc<SessionDb>, ToolContext) {
        let (tmp, db) = test_session_db();
        let context = ToolContext {
            workspace: tmp.path().to_path_buf(),
            profile,
        };
        (tmp, Arc::new(db), context)
    }

    #[test]
    fn coder_profile_adds_execution_tools_to_the_chat_surface() {
        let chat: Vec<&str> = definitions(ToolProfile::Chat)
            .iter()
            .map(|tool| tool.function.name)
            .collect();
        let coder: Vec<&str> = definitions(ToolProfile::Coder)
            .iter()
            .map(|tool| tool.function.name)
            .collect();
        for tool in [BASH, READ, WRITE, EDIT] {
            assert!(!chat.contains(&tool), "{tool} must not be in chat");
            assert!(coder.contains(&tool), "{tool} must be in coder");
        }
        assert!(coder.contains(&SEND_MESSAGE), "messaging tools stay");
    }

    #[tokio::test]
    async fn file_tools_round_trip_through_dispatch() {
        let (tmp, db, context) = fixture(ToolProfile::Coder);

        let written = dispatch(
            &db,
            &context,
            &call(WRITE, r#"{"path":"notes.txt","content":"alpha\nbeta"}"#),
        )
        .await;
        assert_eq!(written.result, "wrote 10 bytes to notes.txt");

        let edited = dispatch(
            &db,
            &context,
            &call(
                EDIT,
                r#"{"path":"notes.txt","old_string":"alpha","new_string":"ALPHA"}"#,
            ),
        )
        .await;
        assert_eq!(edited.result, "edited notes.txt");

        let read_back = dispatch(&db, &context, &call(READ, r#"{"path":"notes.txt"}"#)).await;
        assert_eq!(read_back.result, "     1\tALPHA\n     2\tbeta");
        assert!(tmp.path().join("notes.txt").is_file());
    }

    #[tokio::test]
    async fn execution_tools_are_refused_for_chat_groups() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        for (name, arguments) in [
            (BASH, r#"{"command":"echo hi"}"#),
            (READ, r#"{"path":"x"}"#),
            (WRITE, r#"{"path":"x","content":"y"}"#),
            (EDIT, r#"{"path":"x","old_string":"a","new_string":"b"}"#),
        ] {
            let outcome = dispatch(&db, &context, &call(name, arguments)).await;
            assert!(
                outcome.result.contains("not available"),
                "{name}: {}",
                outcome.result
            );
        }
    }

    #[tokio::test]
    async fn send_message_writes_an_outbound_chat_row() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(&db, &context, &call(SEND_MESSAGE, r#"{"text":"hello"}"#)).await;
        assert!(outcome.produced_message);

        let now = db.now_timestamp().expect("now");
        let due = db.due_outbound(&now).expect("due");
        assert_eq!(due.len(), 1);
        let content = OutboundContent::parse(&due[0].content).expect("content");
        assert_eq!(content.text.as_deref(), Some("hello"));
        assert_eq!(due[0].kind, "chat");
    }

    #[tokio::test]
    async fn schedule_task_writes_a_system_row_without_claiming_a_reply() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(
            &db,
            &context,
            &call(
                SCHEDULE_TASK,
                r#"{"prompt":"daily briefing","recurrence":"0 9 * * *"}"#,
            ),
        )
        .await;
        assert!(!outcome.produced_message);

        let now = db.now_timestamp().expect("now");
        let due = db.due_outbound(&now).expect("due");
        assert_eq!(due[0].kind, "system");
        let payload: serde_json::Value = serde_json::from_str(&due[0].content).expect("json");
        assert_eq!(payload["action"], "schedule_task");
        assert_eq!(payload["recurrence"], "0 9 * * *");
    }

    #[tokio::test]
    async fn send_to_agent_routes_to_the_agent_channel() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        dispatch(
            &db,
            &context,
            &call(
                SEND_TO_AGENT,
                r#"{"agent_group":"ag-coder","text":"review this"}"#,
            ),
        )
        .await;
        let now = db.now_timestamp().expect("now");
        let due = db.due_outbound(&now).expect("due");
        assert_eq!(due[0].routing.channel_type.as_deref(), Some("agent"));
        assert_eq!(due[0].routing.platform_id.as_deref(), Some("ag-coder"));
    }

    #[tokio::test]
    async fn bash_runs_in_the_workspace_for_coder_groups() {
        let (tmp, db, context) = fixture(ToolProfile::Coder);
        std::fs::write(tmp.path().join("hello.txt"), "from the workspace").expect("write");
        let outcome = dispatch(&db, &context, &call(BASH, r#"{"command":"cat hello.txt"}"#)).await;
        assert!(!outcome.produced_message);
        assert_eq!(outcome.result, "exit code: 0\nfrom the workspace");
    }

    #[tokio::test]
    async fn bash_is_refused_for_chat_groups() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(&db, &context, &call(BASH, r#"{"command":"echo hi"}"#)).await;
        assert!(outcome.result.contains("not available"));
    }

    #[tokio::test]
    async fn bad_arguments_become_a_tool_result_not_a_panic() {
        let (_tmp, db, context) = fixture(ToolProfile::Coder);
        for name in [SEND_MESSAGE, BASH] {
            let outcome = dispatch(&db, &context, &call(name, "not json")).await;
            assert!(!outcome.produced_message);
            assert!(outcome.result.contains("error:"), "{name}");
        }
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_back() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(&db, &context, &call("delete_everything", "{}")).await;
        assert!(outcome.result.contains("unknown tool"));
    }

    #[tokio::test]
    async fn ask_user_question_is_a_placeholder_for_now() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(
            &db,
            &context,
            &call(ASK_USER_QUESTION, r#"{"question":"?","options":[]}"#),
        )
        .await;
        assert!(!outcome.produced_message);
        assert!(outcome.result.contains("not available yet"));
    }
}
