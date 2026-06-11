use std::collections::BTreeMap;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::commands::registry::ArgKind;
use crate::commands::{CallerContext, Dispatcher};
use crate::db::endpoints;
use crate::protocol::frame::RequestFrame;

use super::WebState;
use super::api::ApiError;

/// Distinct resources, in a stable display order.
fn resource_names(state: &WebState) -> Vec<&'static str> {
    let mut seen = Vec::new();
    for command in state.commands.commands() {
        if !seen.contains(&command.resource) {
            seen.push(command.resource);
        }
    }
    seen
}

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminPage {
    resources: Vec<NavItem>,
    current: String,
    table: Option<Table>,
    forms: Vec<CommandForm>,
    error: Option<String>,
}

struct NavItem {
    name: String,
    active: bool,
}

struct Table {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

struct CommandForm {
    command: String,
    summary: String,
    fields: Vec<Field>,
}

struct Field {
    name: String,
    label: String,
    /// "text" | "bool" | "select" — drives the input the template renders.
    input: &'static str,
    options: Vec<String>,
    required: bool,
}

#[derive(Deserialize)]
pub struct Flash {
    error: Option<String>,
}

pub async fn index() -> Redirect {
    Redirect::to("/admin/endpoints")
}

pub async fn resource_page(
    State(state): State<WebState>,
    Path(resource): Path<String>,
    Query(flash): Query<Flash>,
) -> Result<Response, ApiError> {
    let resources = resource_names(&state);
    let Some(current) = resources.iter().find(|name| **name == resource).copied() else {
        return Ok(Redirect::to("/admin/endpoints").into_response());
    };

    let endpoint_names = endpoint_names(&state).await?;
    let table = build_table(&state, current).await;
    let forms = build_forms(&state, current, &endpoint_names);

    let page = AdminPage {
        resources: resources
            .iter()
            .map(|name| NavItem {
                name: (*name).to_owned(),
                active: *name == current,
            })
            .collect(),
        current: current.to_owned(),
        table,
        forms,
        error: flash.error,
    };
    Ok(render(&page))
}

/// The list table, when the resource has a no-required-args `<resource>-list`.
async fn build_table(state: &WebState, resource: &str) -> Option<Table> {
    let list_command = format!("{resource}-list");
    let def = state.commands.get(&list_command)?;
    if def.args.iter().any(|arg| arg.required) {
        return None;
    }
    let response = state
        .commands
        .dispatch(request(&list_command, Map::new()), CallerContext::Host)
        .await;
    let rows = response.data?;
    Some(table_from_rows(&rows))
}

fn table_from_rows(rows: &Value) -> Table {
    let items = rows.as_array().cloned().unwrap_or_default();
    let mut columns: Vec<String> = Vec::new();
    for item in &items {
        if let Some(object) = item.as_object() {
            for key in object.keys() {
                if !columns.contains(key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    let rows = items
        .iter()
        .map(|item| {
            columns
                .iter()
                .map(|column| cell_text(item.get(column)))
                .collect()
        })
        .collect();
    Table { columns, rows }
}

fn cell_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// One form per mutating command (everything but reads — `-list` / `-get`).
fn build_forms(state: &WebState, resource: &str, endpoint_names: &[String]) -> Vec<CommandForm> {
    state
        .commands
        .commands()
        .filter(|command| command.resource == resource)
        .filter(|command| !command.name.ends_with("-list") && !command.name.ends_with("-get"))
        .map(|command| CommandForm {
            command: command.name.to_owned(),
            summary: command.summary.to_owned(),
            fields: command
                .args
                .iter()
                .map(|arg| field(arg, endpoint_names))
                .collect(),
        })
        .collect()
}

fn field(arg: &crate::commands::ArgSpec, endpoint_names: &[String]) -> Field {
    let (input, options) = if arg.name == "endpoint" {
        ("select", endpoint_names.to_vec())
    } else {
        match arg.kind {
            ArgKind::Bool => ("bool", Vec::new()),
            ArgKind::Enum(values) => (
                "select",
                values.iter().map(|value| (*value).to_owned()).collect(),
            ),
            ArgKind::Text => ("text", Vec::new()),
        }
    };
    Field {
        name: arg.name.to_owned(),
        label: arg.label.to_owned(),
        input,
        options,
        required: arg.required,
    }
}

async fn endpoint_names(state: &WebState) -> Result<Vec<String>, ApiError> {
    let central = state.central.clone();
    crate::blocking::run::<_, _, ApiError>(move || {
        central.with(|conn| {
            Ok(endpoints::list(conn)?
                .into_iter()
                .map(|endpoint| endpoint.name.as_str().to_owned())
                .collect())
        })
    })
    .await
}

pub async fn run(
    State(state): State<WebState>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    let Some(command) = form.get("command").cloned() else {
        return Redirect::to("/admin/endpoints").into_response();
    };
    let Some(def) = state.commands.get(&command) else {
        return Redirect::to("/admin/endpoints").into_response();
    };
    let resource = def.resource;

    let mut args = Map::new();
    for arg in def.args {
        let Some(raw) = form.get(arg.name) else {
            continue;
        };
        match arg.kind {
            ArgKind::Bool => {
                if raw == "on" || raw == "true" {
                    args.insert(arg.name.to_owned(), Value::Bool(true));
                }
            }
            _ => {
                if !raw.is_empty() {
                    args.insert(arg.name.to_owned(), Value::String(raw.clone()));
                }
            }
        }
    }

    let response = state
        .commands
        .dispatch(request(&command, args), CallerContext::Host)
        .await;
    if response.ok {
        Redirect::to(&format!("/admin/{resource}")).into_response()
    } else {
        let message = response
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| "command failed".to_owned());
        Redirect::to(&format!("/admin/{resource}?error={}", urlencode(&message))).into_response()
    }
}

fn request(command: &str, args: Map<String, Value>) -> RequestFrame {
    RequestFrame {
        id: crate::db::generate_id("admin"),
        command: command.to_owned(),
        args,
    }
}

/// Minimal query-component encoding for the flash message (spaces and `&`/`#`).
fn urlencode(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            ' ' => "+".to_owned(),
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

fn render<T: Template>(page: &T) -> Response {
    match page.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "admin template render failed");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
