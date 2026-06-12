use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use jiff::tz::TimeZone;
use serde::Deserialize;

use crate::cron::{Cron, next_after_utc};
use crate::db::sessions;
use crate::protocol::ids::{AgentGroupId, SessionId};

use super::WebState;
use super::admin::{NavItem, render, resource_nav};
use super::api::ApiError;

#[derive(Template)]
#[template(path = "tasks.html")]
struct TasksPage {
    resources: Vec<NavItem>,
    tasks_active: bool,
    logs_active: bool,
    activity_active: bool,
    tasks: Vec<TaskRow>,
}

struct TaskRow {
    session_id: String,
    agent_group_id: String,
    group: String,
    series: String,
    prompt: String,
    schedule: String,
    next_fire: String,
    paused: bool,
}

pub async fn page(State(state): State<WebState>) -> Result<Response, ApiError> {
    let resources = resource_nav(&state, None);
    let tasks = collect_tasks(&state).await?;
    Ok(render(&TasksPage {
        resources,
        tasks_active: true,
        logs_active: false,
        activity_active: false,
        tasks,
    }))
}

/// Scans every active session's DB for scheduled tasks and computes each one's
/// next fire from the cron evaluator (recurring) or its `process_after` (one-shot).
async fn collect_tasks(state: &WebState) -> Result<Vec<TaskRow>, ApiError> {
    let central = state.central.clone();
    let store = state.store.clone();
    let timezone = state.timezone.clone();
    crate::blocking::run::<_, ApiError, ApiError>(move || {
        let tz = TimeZone::get(&timezone).unwrap_or(TimeZone::UTC);
        let group_names = central.with(|conn| {
            Ok(crate::db::agent_groups::list(conn)?
                .into_iter()
                .map(|group| (group.id, group.name))
                .collect::<std::collections::HashMap<_, _>>())
        })?;
        let mut rows = Vec::new();
        for session in central.with(sessions::list_active)? {
            let db = store.open(&session.agent_group_id, &session.id)?;
            let now = db.now_timestamp()?;
            for task in db.list_scheduled_tasks()? {
                let group = group_names
                    .get(&session.agent_group_id)
                    .cloned()
                    .unwrap_or_else(|| session.agent_group_id.as_str().to_owned());
                rows.push(TaskRow {
                    session_id: session.id.as_str().to_owned(),
                    agent_group_id: session.agent_group_id.as_str().to_owned(),
                    group,
                    series: task.series.clone(),
                    prompt: task.prompt.clone(),
                    schedule: task.recurrence.clone().unwrap_or_else(|| "once".to_owned()),
                    next_fire: next_fire(&task, &now, &tz),
                    paused: task.paused,
                });
            }
        }
        Ok(rows)
    })
    .await
}

fn next_fire(task: &crate::session::ScheduledTask, now: &str, tz: &TimeZone) -> String {
    if task.paused {
        return "paused".to_owned();
    }
    if let Some(recurrence) = &task.recurrence {
        return Cron::parse(recurrence)
            .ok()
            .and_then(|cron| next_after_utc(&cron, now, tz))
            .unwrap_or_else(|| "—".to_owned());
    }
    match &task.process_after {
        Some(at) if at.as_str() > now => at.clone(),
        Some(_) => "due".to_owned(),
        None => "asap".to_owned(),
    }
}

#[derive(Deserialize)]
pub struct TaskAction {
    session_id: String,
    agent_group_id: String,
    series: String,
    action: String,
}

pub async fn action(State(state): State<WebState>, Form(form): Form<TaskAction>) -> Response {
    let store = state.store.clone();
    let result = crate::blocking::run::<_, ApiError, ApiError>(move || {
        let db = store.open(
            &AgentGroupId::new(form.agent_group_id),
            &SessionId::new(form.session_id),
        )?;
        match form.action.as_str() {
            "pause" => db.set_task_paused(&form.series, true)?,
            "resume" => db.set_task_paused(&form.series, false)?,
            "cancel" => db.cancel_task(&form.series)?,
            _ => 0,
        };
        Ok(())
    })
    .await;
    if let Err(error) = result {
        tracing::error!(%error, "task action failed");
    }
    Redirect::to("/admin/tasks").into_response()
}
