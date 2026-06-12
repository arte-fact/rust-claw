use askama::Template;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::channels::web::CHANNEL_TYPE;
use crate::db::{agent_groups, messaging_groups};
use crate::protocol::entities::ToolProfile;
use crate::workspace::{self, Entry, Workspace};

use super::WebState;
use super::api::ApiError;
use super::pages::{ChatItem, render_page, web_chat_items};

/// Resolves a web chat to the folder of its highest-priority wired agent, but only
/// when that agent is a coder — the file browser is coder-only. `None` means the
/// chat has no browsable workspace (unknown chat, no wiring, or a chat-profile agent).
pub(super) fn coder_folder(
    conn: &Connection,
    platform_id: &str,
) -> Result<Option<String>, rusqlite::Error> {
    let Some(chat) = messaging_groups::get_by_platform(conn, CHANNEL_TYPE, platform_id)? else {
        return Ok(None);
    };
    let wirings = messaging_groups::wirings_for(conn, &chat.id)?;
    let Some(wiring) = wirings.first() else {
        return Ok(None);
    };
    let Some(agent) = agent_groups::get(conn, &wiring.agent_group_id)? else {
        return Ok(None);
    };
    Ok((agent.tool_profile == ToolProfile::Coder).then_some(agent.folder))
}

/// Resolves the coder workspace for `platform_id` and runs `op` against it on the
/// blocking pool. A non-coder or unknown chat is a 404 — the browser does not exist.
async fn with_workspace<T, F>(state: &WebState, platform_id: String, op: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Workspace) -> Result<T, ApiError> + Send + 'static,
{
    let central = state.central.clone();
    let groups_dir = state.groups_dir.clone();
    crate::blocking::run::<_, ApiError, ApiError>(move || {
        let folder = central
            .with(|conn| coder_folder(conn, &platform_id))?
            .ok_or(ApiError::ChatNotFound)?;
        let workspace = Workspace::open(groups_dir.join(folder))?;
        op(&workspace)
    })
    .await
}

#[derive(Template)]
#[template(path = "files.html")]
struct FilesPage {
    chats: Vec<ChatItem>,
    archived: Vec<ChatItem>,
    platform_id: String,
    label: String,
    folder: String,
}

pub async fn page(
    State(state): State<WebState>,
    Path(platform_id): Path<String>,
) -> Result<Response, ApiError> {
    let central = state.central.clone();
    let page = crate::blocking::run::<_, _, ApiError>(move || {
        central.with(|conn| {
            let Some(folder) = coder_folder(conn, &platform_id)? else {
                return Ok(None);
            };
            let Some(chat) = messaging_groups::get_by_platform(conn, CHANNEL_TYPE, &platform_id)?
            else {
                return Ok(None);
            };
            let (chats, archived) = web_chat_items(conn, Some(&platform_id))?;
            Ok(Some(FilesPage {
                chats,
                archived,
                label: chat.name.unwrap_or_else(|| "unnamed".to_owned()),
                platform_id: chat.platform_id,
                folder,
            }))
        })
    })
    .await?
    .ok_or(ApiError::ChatNotFound)?;
    Ok(render_page(&page))
}

#[derive(Deserialize)]
pub struct PathQuery {
    #[serde(default)]
    pub path: String,
}

#[derive(Serialize)]
pub struct Listing {
    path: String,
    entries: Vec<Entry>,
}

pub async fn list_entries(
    State(state): State<WebState>,
    Path(platform_id): Path<String>,
    Query(query): Query<PathQuery>,
) -> Result<Json<Listing>, ApiError> {
    let path = query.path;
    let entries = with_workspace(&state, platform_id, {
        let path = path.clone();
        move |workspace| Ok(workspace::ops::list(workspace, &path)?)
    })
    .await?;
    Ok(Json(Listing { path, entries }))
}

#[derive(Serialize)]
pub struct FileContent {
    path: String,
    content: String,
}

pub async fn read_file(
    State(state): State<WebState>,
    Path(platform_id): Path<String>,
    Query(query): Query<PathQuery>,
) -> Result<Json<FileContent>, ApiError> {
    let path = query.path;
    let content = with_workspace(&state, platform_id, {
        let path = path.clone();
        move |workspace| Ok(workspace::ops::read_text(workspace, &path)?)
    })
    .await?;
    Ok(Json(FileContent { path, content }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CentralDb;

    #[test]
    fn coder_folder_resolves_only_for_coder_wired_chats() {
        let db = CentralDb::open_in_memory().expect("db");
        db.with(|conn| {
            let chatty = agent_groups::create(conn, "Chatty", "chatty")?;
            let chat =
                messaging_groups::create(conn, CHANNEL_TYPE, "c-chat", Some("Chatty"), false)?;
            messaging_groups::wire(conn, &chat.id, &chatty.id)?;
            assert_eq!(
                coder_folder(conn, "c-chat")?,
                None,
                "chat-profile agent has no browser"
            );

            let mut coder = agent_groups::create(conn, "Coder", "coder-ws")?;
            coder.tool_profile = ToolProfile::Coder;
            agent_groups::update(conn, &coder)?;
            let cchat =
                messaging_groups::create(conn, CHANNEL_TYPE, "c-code", Some("Coder"), false)?;
            messaging_groups::wire(conn, &cchat.id, &coder.id)?;
            assert_eq!(coder_folder(conn, "c-code")?.as_deref(), Some("coder-ws"));

            let unwired =
                messaging_groups::create(conn, CHANNEL_TYPE, "c-bare", Some("Bare"), false)?;
            let _ = unwired;
            assert_eq!(
                coder_folder(conn, "c-bare")?,
                None,
                "unwired chat has no agent"
            );
            assert_eq!(
                coder_folder(conn, "ghost")?,
                None,
                "unknown chat resolves to none"
            );
            Ok(())
        })
        .expect("seed");
    }
}
