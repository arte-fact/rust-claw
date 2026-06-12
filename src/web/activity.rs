use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::activity::{ActivityEvent, Status};
use crate::db::{agent_groups, sessions};
use crate::protocol::ids::AgentGroupId;

use super::WebState;
use super::admin::{NavItem, render, resource_nav};
use super::api::ApiError;

#[derive(Template)]
#[template(path = "activity.html")]
struct ActivityPage {
    resources: Vec<NavItem>,
    tasks_active: bool,
    logs_active: bool,
    activity_active: bool,
    board: Vec<BoardRow>,
    feed: Vec<ActivityEvent>,
}

struct BoardRow {
    agent_id: String,
    agent: String,
    /// `running` / `failed` / `queued` / `idle` — drives the badge colour + JS.
    status: &'static str,
    chat: Option<String>,
    delegated_by: Option<String>,
    phase: Option<String>,
    message: Option<String>,
    started_at: Option<String>,
}

/// Renders every agent's current state: the live hub snapshot overlaid on the full
/// agent list (so idle agents show) and the run queue (so waiting agents show).
pub async fn page(State(state): State<WebState>) -> Result<Response, ApiError> {
    let snapshot = state.activity.snapshot();
    let queued_sessions = state.queue.snapshot();
    let central = state.central.clone();

    let board = crate::blocking::run::<_, ApiError, ApiError>(move || {
        // Which agent groups have a session waiting in the run queue.
        let queued = central.with(|conn| {
            let mut queued = HashSet::new();
            for session_id in &queued_sessions {
                if let Some(session) = sessions::get(conn, session_id)? {
                    queued.insert(session.agent_group_id);
                }
            }
            Ok(queued)
        })?;

        let live: HashMap<String, _> = snapshot
            .agents
            .into_iter()
            .map(|slot| (slot.agent_id, slot.activity))
            .collect();

        let groups = central.with(agent_groups::list)?;
        let mut board = Vec::with_capacity(groups.len());
        for group in groups {
            let id = group.id.as_str().to_owned();
            let act = live.get(&id);
            let status = match act.map(|a| a.status) {
                Some(Status::Running) => "running",
                Some(Status::Failed) => "failed",
                _ if queued.contains(&AgentGroupId::new(id.clone())) => "queued",
                _ => "idle",
            };
            board.push(BoardRow {
                agent_id: id,
                agent: group.name,
                status,
                chat: act.and_then(|a| a.chat.clone()),
                delegated_by: act.and_then(|a| a.delegated_by.clone()),
                phase: act.and_then(|a| a.phase.clone()),
                message: act.and_then(|a| a.message.clone()),
                started_at: act.and_then(|a| a.started_at.clone()),
            });
        }
        Ok(board)
    })
    .await?;

    Ok(render(&ActivityPage {
        resources: resource_nav(&state, None),
        tasks_active: false,
        logs_active: false,
        activity_active: true,
        board,
        feed: snapshot.feed,
    }))
}

/// Live stream of per-agent activity updates (one `activity` event per change).
pub async fn stream(
    State(state): State<WebState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = BroadcastStream::new(state.activity.subscribe()).filter_map(|item| {
        item.ok().map(|update| {
            let data = serde_json::to_string(&update).unwrap_or_default();
            Ok(Event::default().event("activity").data(data))
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
