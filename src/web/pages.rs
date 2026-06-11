use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::db::{messaging_groups, web_messages};

use super::WebState;
use super::api::{ApiError, create_chat_inner};
use super::render::message_html;

const HISTORY_LIMIT: i64 = 200;

#[derive(rust_embed::Embed)]
#[folder = "assets/"]
struct Assets;

pub async fn asset(Path(path): Path<String>) -> Response {
    let Some(file) = Assets::get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = match path.rsplit('.').next() {
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("ttf") => "font/ttf",
        Some("woff2") => "font/woff2",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        file.data,
    )
        .into_response()
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginPage {
    pub error: bool,
}

#[derive(Deserialize)]
pub struct LoginQuery {
    pub error: Option<String>,
}

pub async fn login_page(Query(query): Query<LoginQuery>) -> Response {
    render_page(&LoginPage {
        error: query.error.is_some(),
    })
}

#[derive(Template)]
#[template(path = "shell.html")]
struct ShellPage {
    chats: Vec<ChatItem>,
    current: Option<CurrentChat>,
}

struct ChatItem {
    platform_id: String,
    label: String,
    active: bool,
}

struct CurrentChat {
    platform_id: String,
    label: String,
    messages: Vec<String>,
}

pub async fn home(State(state): State<WebState>) -> Result<Response, ApiError> {
    shell(state, None).await
}

pub async fn chat_page(
    State(state): State<WebState>,
    Path(platform_id): Path<String>,
) -> Result<Response, ApiError> {
    shell(state, Some(platform_id)).await
}

#[derive(Deserialize)]
pub struct NewChatForm {
    pub name: String,
}

pub async fn create_chat_form(
    State(state): State<WebState>,
    Form(form): Form<NewChatForm>,
) -> Result<Response, ApiError> {
    let chat = create_chat_inner(&state, form.name).await?;
    Ok(Redirect::to(&format!("/chats/{}", chat.platform_id)).into_response())
}

async fn shell(state: WebState, selected: Option<String>) -> Result<Response, ApiError> {
    let central = state.central.clone();
    let page = crate::blocking::run::<_, _, ApiError>(move || {
        central.with(|conn| {
            let chats: Vec<ChatItem> = messaging_groups::list(conn)?
                .into_iter()
                .filter(|group| group.channel_type == crate::channels::web::CHANNEL_TYPE)
                .map(|group| ChatItem {
                    active: selected.as_deref() == Some(group.platform_id.as_str()),
                    label: group.name.unwrap_or_else(|| group.platform_id.clone()),
                    platform_id: group.platform_id,
                })
                .collect();

            let current = match &selected {
                None => None,
                Some(platform_id) => {
                    let Some(group) = messaging_groups::get_by_platform(
                        conn,
                        crate::channels::web::CHANNEL_TYPE,
                        platform_id,
                    )?
                    else {
                        return Ok(None);
                    };
                    let messages = web_messages::list(conn, &group.id, HISTORY_LIMIT)?
                        .iter()
                        .map(message_html)
                        .collect();
                    Some(CurrentChat {
                        platform_id: group.platform_id,
                        label: group.name.unwrap_or_else(|| "unnamed".to_owned()),
                        messages,
                    })
                }
            };
            Ok(Some(ShellPage { chats, current }))
        })
    })
    .await?
    .ok_or(ApiError::ChatNotFound)?;
    Ok(render_page(&page))
}

fn render_page<T: Template>(page: &T) -> Response {
    match page.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "page template render failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
