use askama::Template;
use axum::extract::State;
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::logs::LogRecord;

use super::WebState;
use super::admin::{NavItem, render, resource_nav};

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsPage {
    resources: Vec<NavItem>,
    tasks_active: bool,
    logs_active: bool,
    activity_active: bool,
    lines: Vec<LogRecord>,
}

/// The admin log viewer: renders the current in-memory ring, then `claw.js`
/// live-appends new records from the SSE stream below.
pub async fn page(State(state): State<WebState>) -> Response {
    render(&LogsPage {
        resources: resource_nav(&state, None),
        tasks_active: false,
        logs_active: true,
        activity_active: false,
        lines: state.logs.snapshot(),
    })
}

/// Server-sent stream of new log records (one `log` event per record), mirroring
/// `sse::events`; lagging clients drop records and recover on reload.
pub async fn stream(
    State(state): State<WebState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = BroadcastStream::new(state.logs.subscribe()).filter_map(|item| {
        item.ok().map(|record| {
            let data = serde_json::to_string(&record).unwrap_or_default();
            Ok(Event::default().event("log").data(data))
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
