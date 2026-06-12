use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::mcp::{McpClient, McpTool, dequalify, qualify};
use crate::protocol::content::{Operation, OutboundContent, Routing};
use crate::protocol::entities::ToolProfile;
use crate::protocol::frame::RequestFrame;
use crate::providers::AgentAdmin;
use crate::session::{NewOutboundMessage, SessionDb};

use super::client::{ToolCall, ToolDefinition};
use super::{exec, files};

pub const ADMIN: &str = "admin";
pub const SEND_MESSAGE: &str = "send_message";
pub const SCHEDULE_TASK: &str = "schedule_task";
pub const LIST_TASKS: &str = "list_tasks";
pub const CANCEL_TASK: &str = "cancel_task";
pub const PAUSE_TASK: &str = "pause_task";
pub const RESUME_TASK: &str = "resume_task";
pub const SEND_TO_AGENT: &str = "send_to_agent";
pub const ASK_USER_QUESTION: &str = "ask_user_question";
pub const BASH: &str = "bash";
pub const READ: &str = "read";
pub const WRITE: &str = "write";
pub const EDIT: &str = "edit";

/// What a tool call may touch: the group workspace (gated by profile), the admin
/// command surface when `cli_scope` permits (§8.7, M6.4), and the shared MCP
/// server's tools (M12) — exposed to every agent.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub profile: ToolProfile,
    pub admin: Option<AgentAdmin>,
    pub mcp: Option<Arc<McpClient>>,
}

/// Tool surface (§8.5): messaging for everyone; bash/files for coders; the `admin`
/// command tool whenever `cli_scope` is not `disabled` (independent of profile);
/// the MCP server's tools for everyone when one is connected (M12).
#[must_use]
pub fn definitions(
    profile: ToolProfile,
    admin_enabled: bool,
    mcp: Option<&McpClient>,
) -> Vec<ToolDefinition> {
    let mut tools = messaging_definitions();
    if profile == ToolProfile::Coder {
        tools.extend(coder_definitions());
    }
    if admin_enabled {
        tools.push(admin_definition());
    }
    if let Some(client) = mcp {
        tools.extend(mcp_definitions(client.server_name(), client.tools()));
    }
    tools
}

/// Turns a server's tools into model-facing definitions, names namespaced as
/// `<server>__<tool>` so they can't collide with native tools. A tool with no
/// input schema gets a permissive empty-object one.
fn mcp_definitions(server: &str, tools: &[McpTool]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|tool| {
            let parameters = if tool.input_schema.is_null() {
                json!({ "type": "object" })
            } else {
                tool.input_schema.clone()
            };
            ToolDefinition::function(
                qualify(server, &tool.name),
                tool.description.clone(),
                parameters,
            )
        })
        .collect()
}

fn admin_definition() -> ToolDefinition {
    ToolDefinition::function(
        ADMIN,
        "Run a claw admin command (e.g. endpoints-list, groups-update) to inspect or change \
         configuration. Use endpoints-list / groups-list to discover names first.",
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command name, e.g. groups-update." },
                "args": { "type": "object", "description": "Command arguments as a JSON object." }
            },
            "required": ["command"]
        }),
    )
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
            "Schedule a prompt to run later, optionally on a recurring cron schedule. Returns a series id.",
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
                        "description": "Cron expression (5 fields) for recurring runs. Omit for a one-shot task."
                    }
                },
                "required": ["prompt"]
            }),
        ),
        ToolDefinition::function(
            LIST_TASKS,
            "List the active (pending or paused) scheduled tasks with their series ids.",
            json!({ "type": "object", "properties": {} }),
        ),
        ToolDefinition::function(
            CANCEL_TASK,
            "Cancel a scheduled task by its series id.",
            json!({
                "type": "object",
                "properties": { "series": { "type": "string" } },
                "required": ["series"]
            }),
        ),
        ToolDefinition::function(
            PAUSE_TASK,
            "Pause a scheduled task by its series id (stops it firing until resumed).",
            json!({
                "type": "object",
                "properties": { "series": { "type": "string" } },
                "required": ["series"]
            }),
        ),
        ToolDefinition::function(
            RESUME_TASK,
            "Resume a paused scheduled task by its series id.",
            json!({
                "type": "object",
                "properties": { "series": { "type": "string" } },
                "required": ["series"]
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
            "Ask the user a multiple-choice question (>=2 options). Their choice arrives later as a \
             normal message — do not block waiting; end your turn after asking.",
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
    if let Some((server, tool)) = dequalify(name)
        && let Some(client) = &context.mcp
        && client.server_name() == server
    {
        return run_mcp(client, tool, &call.function.arguments).await;
    }
    match name {
        ADMIN => match &context.admin {
            Some(admin) => run_admin(db, admin, &call.function.arguments).await,
            None => {
                ToolOutcome::note("error: admin commands are not available to this agent group")
            }
        },
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

/// Forwards a namespaced tool call to the MCP server. Bad arguments or a server
/// error become a result string the model can react to — never a turn failure.
async fn run_mcp(client: &McpClient, tool: &str, arguments: &str) -> ToolOutcome {
    let args: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
    match client.call(tool, args).await {
        Ok(text) => ToolOutcome::note(text),
        Err(err) => ToolOutcome::note(format!("error: {tool} failed: {err}")),
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
        LIST_TASKS => list_tasks(db),
        CANCEL_TASK => task_action(db, &call.function.arguments, TaskAction::Cancel),
        PAUSE_TASK => task_action(db, &call.function.arguments, TaskAction::Pause),
        RESUME_TASK => task_action(db, &call.function.arguments, TaskAction::Resume),
        SEND_TO_AGENT => send_to_agent(db, &call.function.arguments),
        ASK_USER_QUESTION => ask_user_question(db, &call.function.arguments),
        other => ToolOutcome::note(format!("error: unknown tool {other}")),
    }
}

#[derive(Deserialize)]
struct AdminArgs {
    command: String,
    #[serde(default)]
    args: Map<String, Value>,
}

/// Runs an admin command through the dispatcher as this agent — the same path the
/// operator CLI uses, so M6.3's `cli_scope`/`Hidden` gates apply uniformly. An
/// `Approval` command comes back held; we surface it as an approval card (M7.2).
async fn run_admin(db: &SessionDb, admin: &AgentAdmin, arguments: &str) -> ToolOutcome {
    let parsed: AdminArgs = match serde_json::from_str(arguments) {
        Ok(parsed) => parsed,
        Err(err) => return ToolOutcome::note(format!("error: bad arguments: {err}")),
    };
    let request = RequestFrame {
        id: crate::db::generate_id("acmd"),
        command: parsed.command,
        args: parsed.args,
    };
    let response = admin
        .dispatcher
        .dispatch(request, admin.caller.clone())
        .await;
    if response.error.as_ref().map(|error| error.code)
        == Some(crate::protocol::frame::ErrorCode::ApprovalPending)
    {
        return submit_for_approval(db, &response);
    }
    ToolOutcome::note(render_admin_response(&response))
}

/// Turns a held `Approval` response into an `Operation::Approval` outbound, which
/// delivery registers and the channel renders as an allow/deny card.
fn submit_for_approval(
    db: &SessionDb,
    response: &crate::protocol::frame::ResponseFrame,
) -> ToolOutcome {
    let data = response.data.as_ref();
    let command = data
        .and_then(|value| value.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = data
        .and_then(|value| value.get("args"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let approval_id = crate::db::generate_id("ap");
    let summary = format!("run `{command}`{}", summarize_args(&args));
    let content = OutboundContent {
        text: Some(format!("Requested owner approval to {summary}.")),
        files: Vec::new(),
        operation: Some(Operation::Approval {
            approval_id: approval_id.clone(),
            command,
            args,
            summary,
        }),
        extra: Map::new(),
    };
    let body = match serde_json::to_string(&content) {
        Ok(body) => body,
        Err(err) => return ToolOutcome::note(format!("error: {err}")),
    };
    match db.write_outbound(&NewOutboundMessage::chat(body, Routing::default())) {
        Ok(_) => ToolOutcome::message(format!(
            "submitted for owner approval ({approval_id}); the outcome will arrive as a message"
        )),
        Err(err) => ToolOutcome::note(format!("error: {err}")),
    }
}

fn summarize_args(args: &Map<String, Value>) -> String {
    if args.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = args
        .iter()
        .map(|(key, value)| match value.as_str() {
            Some(text) => format!("{key}={text}"),
            None => format!("{key}={value}"),
        })
        .collect();
    format!(" ({})", parts.join(", "))
}

fn render_admin_response(response: &crate::protocol::frame::ResponseFrame) -> String {
    if response.ok {
        let data = response
            .data
            .as_ref()
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| value.to_string()))
            .unwrap_or_else(|| "ok".to_owned());
        format!("ok: {data}")
    } else if let Some(error) = &response.error {
        format!("error: {}: {}", error.code.as_str(), error.message)
    } else {
        "error: command failed".to_owned()
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
struct AskQuestionArgs {
    question: String,
    options: Vec<String>,
}

/// Posts a multiple-choice question as an outbound `AskQuestion` operation. The
/// run does not block: the question is delivered as a card, and the user's choice
/// returns later as a normal inbound message that re-wakes the session.
fn ask_user_question(db: &SessionDb, arguments: &str) -> ToolOutcome {
    let args: AskQuestionArgs = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(err) => return ToolOutcome::note(format!("error: bad arguments: {err}")),
    };
    if args.options.len() < 2 {
        return ToolOutcome::note("error: ask_user_question needs at least two options");
    }
    let question_id = crate::db::generate_id("q");
    // The transcript-visible text lets a later turn see what was asked (the card
    // itself is rendered from the operation, not this text).
    let content = OutboundContent {
        text: Some(format!("{} ({})", args.question, args.options.join(" / "))),
        files: Vec::new(),
        operation: Some(Operation::AskQuestion {
            question_id: question_id.clone(),
            title: args.question.clone(),
            question: args.question,
            options: args.options,
        }),
        extra: serde_json::Map::new(),
    };
    let body = match serde_json::to_string(&content) {
        Ok(body) => body,
        Err(err) => return ToolOutcome::note(format!("error: {err}")),
    };
    match db.write_outbound(&NewOutboundMessage::chat(body, Routing::default())) {
        Ok(_) => ToolOutcome::message(format!(
            "asked the user (question {question_id}); their choice will arrive as a new message"
        )),
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
    if let Some(recurrence) = &args.recurrence
        && let Err(err) = crate::cron::Cron::parse(recurrence)
    {
        return ToolOutcome::note(format!("error: {err}"));
    }
    match db.schedule_task(
        &args.prompt,
        args.process_after.as_deref(),
        args.recurrence.as_deref(),
    ) {
        Ok(series) => ToolOutcome::note(format!("scheduled (series {series})")),
        Err(err) => ToolOutcome::note(format!("error: {err}")),
    }
}

fn list_tasks(db: &SessionDb) -> ToolOutcome {
    match db.list_scheduled_tasks() {
        Ok(tasks) if tasks.is_empty() => ToolOutcome::note("no scheduled tasks"),
        Ok(tasks) => {
            let lines: Vec<String> = tasks
                .iter()
                .map(|task| {
                    let schedule = match (&task.recurrence, &task.process_after) {
                        (Some(cron), _) => format!("recurring [{cron}]"),
                        (None, Some(at)) => format!("once at {at}"),
                        (None, None) => "as soon as possible".to_owned(),
                    };
                    let paused = if task.paused { " (paused)" } else { "" };
                    format!(
                        "- {} — {} — {}{}",
                        task.series, task.prompt, schedule, paused
                    )
                })
                .collect();
            ToolOutcome::note(lines.join("\n"))
        }
        Err(err) => ToolOutcome::note(format!("error: {err}")),
    }
}

#[derive(Deserialize)]
struct SeriesArgs {
    series: String,
}

enum TaskAction {
    Cancel,
    Pause,
    Resume,
}

fn task_action(db: &SessionDb, arguments: &str, action: TaskAction) -> ToolOutcome {
    let args: SeriesArgs = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(err) => return ToolOutcome::note(format!("error: bad arguments: {err}")),
    };
    let result = match action {
        TaskAction::Cancel => db.cancel_task(&args.series),
        TaskAction::Pause => db.set_task_paused(&args.series, true),
        TaskAction::Resume => db.set_task_paused(&args.series, false),
    };
    match result {
        Ok(0) => ToolOutcome::note(format!("no active task with series {}", args.series)),
        Ok(_) => ToolOutcome::note("done"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::CallerContext;
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
            admin: None,
            mcp: None,
        };
        (tmp, Arc::new(db), context)
    }

    fn tool_names(tools: &[ToolDefinition]) -> Vec<&str> {
        tools
            .iter()
            .map(|tool| tool.function.name.as_ref())
            .collect()
    }

    #[test]
    fn coder_profile_adds_execution_tools_to_the_chat_surface() {
        let chat = definitions(ToolProfile::Chat, false, None);
        let chat = tool_names(&chat);
        let coder = definitions(ToolProfile::Coder, false, None);
        let coder = tool_names(&coder);
        for tool in [BASH, READ, WRITE, EDIT] {
            assert!(!chat.contains(&tool), "{tool} must not be in chat");
            assert!(coder.contains(&tool), "{tool} must be in coder");
        }
        assert!(coder.contains(&SEND_MESSAGE), "messaging tools stay");
    }

    #[test]
    fn mcp_tools_are_namespaced_and_default_their_schema() {
        let tools = vec![
            McpTool {
                name: "search".to_owned(),
                description: "web search".to_owned(),
                input_schema: json!({ "type": "object", "properties": { "q": { "type": "string" } } }),
            },
            McpTool {
                name: "fetch".to_owned(),
                description: "get a url".to_owned(),
                input_schema: Value::Null,
            },
        ];
        let defs = mcp_definitions("web", &tools);
        assert_eq!(tool_names(&defs), ["web__search", "web__fetch"]);
        assert_eq!(
            defs[0].function.parameters["properties"]["q"]["type"],
            "string"
        );
        assert_eq!(defs[1].function.parameters, json!({ "type": "object" }));
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
    async fn schedule_task_creates_a_listable_task_and_lifecycle_works() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let scheduled = dispatch(
            &db,
            &context,
            &call(
                SCHEDULE_TASK,
                r#"{"prompt":"daily briefing","recurrence":"0 9 * * *"}"#,
            ),
        )
        .await;
        assert!(!scheduled.produced_message);
        assert!(scheduled.result.starts_with("scheduled (series task-"));

        let listed = dispatch(&db, &context, &call(LIST_TASKS, "{}")).await;
        assert!(listed.result.contains("daily briefing"));
        assert!(listed.result.contains("recurring [0 9 * * *]"));

        let series = db.list_scheduled_tasks().expect("list")[0].series.clone();
        let series_args = format!(r#"{{"series":"{series}"}}"#);

        assert_eq!(
            dispatch(&db, &context, &call(PAUSE_TASK, &series_args))
                .await
                .result,
            "done"
        );
        assert!(db.list_scheduled_tasks().expect("list")[0].paused);

        assert_eq!(
            dispatch(&db, &context, &call(CANCEL_TASK, &series_args))
                .await
                .result,
            "done"
        );
        assert!(
            dispatch(&db, &context, &call(LIST_TASKS, "{}"))
                .await
                .result
                .contains("no scheduled tasks")
        );
    }

    #[tokio::test]
    async fn schedule_task_rejects_a_bad_cron() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(
            &db,
            &context,
            &call(SCHEDULE_TASK, r#"{"prompt":"x","recurrence":"not a cron"}"#),
        )
        .await;
        assert!(outcome.result.starts_with("error:"));
        assert!(db.list_scheduled_tasks().expect("list").is_empty());
    }

    #[tokio::test]
    async fn cancel_of_unknown_series_is_reported() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(
            &db,
            &context,
            &call(CANCEL_TASK, r#"{"series":"task-nope"}"#),
        )
        .await;
        assert!(outcome.result.contains("no active task"));
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
    async fn ask_user_question_writes_an_outbound_ask_operation() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(
            &db,
            &context,
            &call(
                ASK_USER_QUESTION,
                r#"{"question":"Deploy now?","options":["ship","wait"]}"#,
            ),
        )
        .await;
        assert!(outcome.produced_message);

        let now = db.now_timestamp().expect("now");
        let due = db.due_outbound(&now).expect("due");
        assert_eq!(due.len(), 1);
        let content = OutboundContent::parse(&due[0].content).expect("content");
        // Transcript text is present so a later turn sees the question.
        assert!(content.text.expect("text").contains("Deploy now?"));
        let Some(Operation::AskQuestion {
            question, options, ..
        }) = content.operation
        else {
            panic!("expected an ask_question operation");
        };
        assert_eq!(question, "Deploy now?");
        assert_eq!(options, vec!["ship".to_owned(), "wait".to_owned()]);
    }

    #[tokio::test]
    async fn ask_user_question_rejects_fewer_than_two_options() {
        let (_tmp, db, context) = fixture(ToolProfile::Chat);
        let outcome = dispatch(
            &db,
            &context,
            &call(ASK_USER_QUESTION, r#"{"question":"?","options":["only"]}"#),
        )
        .await;
        assert!(!outcome.produced_message);
        assert!(outcome.result.contains("at least two options"));
        assert!(
            db.due_outbound(&db.now_timestamp().expect("now"))
                .expect("due")
                .is_empty()
        );
    }

    #[test]
    fn admin_tool_appears_only_when_enabled() {
        let without = definitions(ToolProfile::Chat, false, None);
        assert!(!tool_names(&without).contains(&ADMIN));
        let with = definitions(ToolProfile::Chat, true, None);
        assert!(tool_names(&with).contains(&ADMIN));
    }

    #[tokio::test]
    async fn admin_tool_without_capability_is_refused() {
        let (_tmp, db, context) = fixture(ToolProfile::Coder);
        let outcome = dispatch(&db, &context, &call(ADMIN, r#"{"command":"groups-list"}"#)).await;
        assert!(outcome.result.contains("not available"));
    }

    fn admin_context(scope: crate::protocol::entities::CliScope) -> (ToolContext, AgentAdmin) {
        let central = Arc::new(crate::db::CentralDb::open_in_memory().expect("central"));
        let group_id = central
            .with(|conn| {
                let mut group = crate::db::agent_groups::create(conn, "ops", "ops")?;
                group.cli_scope = scope;
                crate::db::agent_groups::update(conn, &group)?;
                Ok(group.id)
            })
            .expect("group");
        let admin = AgentAdmin {
            dispatcher: Arc::new(crate::commands::Registry::new(central)),
            caller: CallerContext::Agent {
                session_id: crate::protocol::ids::SessionId::new("s-1"),
                agent_group_id: group_id,
                messaging_group_id: None,
            },
        };
        let context = ToolContext {
            workspace: std::path::PathBuf::from("."),
            profile: ToolProfile::Chat,
            admin: Some(admin.clone()),
            mcp: None,
        };
        (context, admin)
    }

    #[tokio::test]
    async fn admin_tool_runs_a_command_through_the_dispatcher() {
        use crate::protocol::entities::CliScope;
        let (_tmp, db, _) = fixture(ToolProfile::Chat);
        let (context, _admin) = admin_context(CliScope::Global);
        let outcome = dispatch(&db, &context, &call(ADMIN, r#"{"command":"groups-list"}"#)).await;
        assert!(outcome.result.starts_with("ok:"), "{}", outcome.result);
        assert!(outcome.result.contains("ops"));
    }

    #[tokio::test]
    async fn admin_tool_is_scope_gated_like_the_cli() {
        use crate::protocol::entities::CliScope;
        let (_tmp, db, _) = fixture(ToolProfile::Chat);
        let (context, _admin) = admin_context(CliScope::Group);
        // endpoints are not group-scoped, so a `group` agent is refused (M6.3).
        let outcome = dispatch(
            &db,
            &context,
            &call(ADMIN, r#"{"command":"endpoints-list"}"#),
        )
        .await;
        assert!(outcome.result.starts_with("error:"), "{}", outcome.result);
        assert!(outcome.result.contains("forbidden") || outcome.result.contains("may not"));
    }
}
