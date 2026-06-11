use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::commands::CallerContext;
use crate::db::{DbError, agent_groups, approvals, messaging_groups, questions, web_messages};
use crate::protocol::content::{ChatContent, SystemResult};
use crate::protocol::ids::UserId;
use crate::protocol::message::MessageKind;
use crate::router::InboundEvent;

use super::WebState;

pub const OWNER_USER_ID: &str = "web:owner";
const HISTORY_LIMIT: i64 = 200;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Session(#[from] crate::session::SessionStoreError),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("chat not found")]
    ChatNotFound,
    #[error("no agent group exists to wire the chat to")]
    NoAgentGroup,
    #[error("question is no longer open")]
    QuestionClosed,
    #[error("{0:?} is not one of the offered options")]
    NotAnOption(String),
    #[error("channel error: {0}")]
    Channel(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::ChatNotFound | Self::QuestionClosed => StatusCode::NOT_FOUND,
            Self::NoAgentGroup => StatusCode::CONFLICT,
            Self::NotAnOption(_) => StatusCode::BAD_REQUEST,
            Self::Db(_) | Self::Session(_) | Self::Join(_) | Self::Channel(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(Serialize)]
pub struct ChatSummary {
    pub platform_id: String,
    pub name: Option<String>,
}

pub async fn list_chats(State(state): State<WebState>) -> Result<Json<Vec<ChatSummary>>, ApiError> {
    let central = state.central.clone();
    let chats = blocking(move || {
        central.with(|conn| {
            Ok(messaging_groups::list(conn)?
                .into_iter()
                .filter(|group| group.channel_type == crate::channels::web::CHANNEL_TYPE)
                .map(|group| ChatSummary {
                    platform_id: group.platform_id,
                    name: group.name,
                })
                .collect())
        })
    })
    .await?;
    Ok(Json(chats))
}

#[derive(Deserialize)]
pub struct CreateChat {
    pub name: String,
}

pub async fn create_chat(
    State(state): State<WebState>,
    Json(body): Json<CreateChat>,
) -> Result<Json<ChatSummary>, ApiError> {
    create_chat_inner(&state, body.name).await.map(Json)
}

/// Shared by the JSON API and the sidebar form: create a chat wired to the first agent group.
pub(super) async fn create_chat_inner(
    state: &WebState,
    name: String,
) -> Result<ChatSummary, ApiError> {
    let central = state.central.clone();
    blocking(move || {
        central.with(|conn| {
            let groups = agent_groups::list(conn)?;
            let Some(agent_group) = groups.first() else {
                return Ok(None);
            };
            let platform_id = crate::db::generate_id("chat");
            let chat = messaging_groups::create(
                conn,
                crate::channels::web::CHANNEL_TYPE,
                &platform_id,
                Some(&name),
                false,
            )?;
            messaging_groups::wire(conn, &chat.id, &agent_group.id)?;
            Ok(Some(ChatSummary {
                platform_id: chat.platform_id,
                name: chat.name,
            }))
        })
    })
    .await?
    .ok_or(ApiError::NoAgentGroup)
}

pub async fn list_messages(
    State(state): State<WebState>,
    Path(platform_id): Path<String>,
) -> Result<Json<Vec<web_messages::WebMessage>>, ApiError> {
    let central = state.central.clone();
    let messages = blocking(move || {
        central.with(|conn| {
            let Some(chat) = messaging_groups::get_by_platform(
                conn,
                crate::channels::web::CHANNEL_TYPE,
                &platform_id,
            )?
            else {
                return Ok(None);
            };
            web_messages::list(conn, &chat.id, HISTORY_LIMIT).map(Some)
        })
    })
    .await?
    .ok_or(ApiError::ChatNotFound)?;
    Ok(Json(messages))
}

#[derive(Deserialize)]
pub struct PostMessage {
    pub text: String,
}

pub async fn post_message(
    State(state): State<WebState>,
    Path(platform_id): Path<String>,
    Json(body): Json<PostMessage>,
) -> Result<Json<web_messages::WebMessage>, ApiError> {
    let central = state.central.clone();
    let text = body.text.clone();
    let ledger_platform_id = platform_id.clone();
    let ledgered = blocking(move || {
        central.with(|conn| {
            let Some(chat) = messaging_groups::get_by_platform(
                conn,
                crate::channels::web::CHANNEL_TYPE,
                &ledger_platform_id,
            )?
            else {
                return Ok(None);
            };
            web_messages::append(
                conn,
                &chat.id,
                web_messages::Direction::In,
                "you",
                &text,
                None,
            )
            .map(Some)
        })
    })
    .await?
    .ok_or(ApiError::ChatNotFound)?;

    state.hub.publish(
        "message",
        super::render::message_event_payload(&platform_id, &ledgered),
    );

    let content = ChatContent {
        sender: "you".to_owned(),
        sender_id: Some(UserId::new(OWNER_USER_ID)),
        text: body.text,
        attachments: Vec::new(),
        is_from_me: false,
        quoted: None,
    };
    let event = InboundEvent {
        channel_type: crate::channels::web::CHANNEL_TYPE.to_owned(),
        platform_id,
        thread_id: None,
        kind: MessageKind::Chat,
        content: serde_json::to_string(&content)
            .map_err(|err| ApiError::Channel(err.to_string()))?,
        is_mention: false,
        is_group: false,
    };
    state
        .web_channel
        .submit(event)
        .await
        .map_err(|err| ApiError::Channel(err.to_string()))?;
    Ok(Json(ledgered))
}

#[derive(Deserialize)]
pub struct AnswerQuestion {
    pub option: String,
}

/// Records a user's choice for an open question: collapses the card and re-wakes
/// the asking session with the answer as a normal inbound message.
pub async fn answer_question(
    State(state): State<WebState>,
    Path(question_id): Path<String>,
    Json(body): Json<AnswerQuestion>,
) -> Result<Json<web_messages::WebMessage>, ApiError> {
    let central = state.central.clone();
    let option = body.option.clone();
    let qid = question_id.clone();
    let (routing, card) = blocking(move || {
        central.with(|conn| {
            let Some(question) = questions::get(conn, &qid)? else {
                return Ok(Err(ApiError::QuestionClosed));
            };
            if !question.options.iter().any(|allowed| allowed == &option) {
                return Ok(Err(ApiError::NotAnOption(option.clone())));
            }
            questions::take(conn, &qid)?;
            match web_messages::resolve_question(conn, &qid, &option)? {
                Some(card) => Ok(Ok((question.routing, card))),
                None => Ok(Err(ApiError::QuestionClosed)),
            }
        })
    })
    .await??;

    let platform_id = routing
        .platform_id
        .clone()
        .ok_or(ApiError::QuestionClosed)?;

    state.hub.publish(
        "message_update",
        super::render::message_update_payload(&card),
    );

    let content = ChatContent {
        sender: "you".to_owned(),
        sender_id: Some(UserId::new(OWNER_USER_ID)),
        text: body.option,
        attachments: Vec::new(),
        is_from_me: false,
        quoted: None,
    };
    let event = InboundEvent {
        channel_type: crate::channels::web::CHANNEL_TYPE.to_owned(),
        platform_id,
        thread_id: routing.thread_id,
        kind: MessageKind::Chat,
        content: serde_json::to_string(&content)
            .map_err(|err| ApiError::Channel(err.to_string()))?,
        is_mention: false,
        is_group: false,
    };
    state
        .web_channel
        .submit(event)
        .await
        .map_err(|err| ApiError::Channel(err.to_string()))?;
    Ok(Json(card))
}

#[derive(Deserialize)]
pub struct AnswerApproval {
    pub decision: String,
}

/// Resolves a held approval: on Allow, runs the command (bypassing the approval
/// gate, since the operator authorized it); either way collapses the card and
/// re-wakes the agent's session with a `system` result.
pub async fn answer_approval(
    State(state): State<WebState>,
    Path(approval_id): Path<String>,
    Json(body): Json<AnswerApproval>,
) -> Result<Json<web_messages::WebMessage>, ApiError> {
    let allow = match body.decision.as_str() {
        "Allow" => true,
        "Deny" => false,
        other => return Err(ApiError::NotAnOption(other.to_owned())),
    };

    let central = state.central.clone();
    let aid = approval_id.clone();
    let approval = blocking(move || central.with(|conn| approvals::take(conn, &aid)))
        .await?
        .ok_or(ApiError::QuestionClosed)?;

    let (status, result) = if allow {
        let caller = CallerContext::Agent {
            session_id: approval.session_id.clone(),
            agent_group_id: approval.agent_group_id.clone(),
            messaging_group_id: None,
        };
        let response = state
            .commands
            .execute_approved(
                crate::db::generate_id("acmd"),
                &approval.command,
                approval.args.clone(),
                caller,
            )
            .await;
        if response.ok {
            ("ok", response.data.unwrap_or(serde_json::Value::Null))
        } else {
            let message = response
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| "command failed".to_owned());
            ("error", serde_json::Value::String(message))
        }
    } else {
        (
            "denied",
            serde_json::Value::String("declined by owner".to_owned()),
        )
    };

    let label = if allow { "Allowed" } else { "Denied" };
    let central = state.central.clone();
    let aid = approval_id.clone();
    let card =
        blocking(move || central.with(|conn| web_messages::resolve_question(conn, &aid, label)))
            .await?
            .ok_or(ApiError::QuestionClosed)?;

    state.hub.publish(
        "message_update",
        super::render::message_update_payload(&card),
    );

    let platform_id = approval
        .routing
        .platform_id
        .clone()
        .ok_or(ApiError::QuestionClosed)?;
    let system = SystemResult {
        action: approval.command,
        status: status.to_owned(),
        result,
    };
    let event = InboundEvent {
        channel_type: crate::channels::web::CHANNEL_TYPE.to_owned(),
        platform_id,
        thread_id: approval.routing.thread_id,
        kind: MessageKind::System,
        content: serde_json::to_string(&system)
            .map_err(|err| ApiError::Channel(err.to_string()))?,
        is_mention: false,
        is_group: false,
    };
    state
        .web_channel
        .submit(event)
        .await
        .map_err(|err| ApiError::Channel(err.to_string()))?;
    Ok(Json(card))
}

async fn blocking<T>(
    op: impl FnOnce() -> Result<T, DbError> + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
{
    crate::blocking::run(op).await
}
