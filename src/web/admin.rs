use std::collections::BTreeMap;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::commands::registry::{ArgKind, ArgSpec, CommandDef};
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

/// The admin sidebar entries, shared by the resource pages and the Tasks page.
pub(super) fn resource_nav(state: &WebState, active: Option<&str>) -> Vec<NavItem> {
    resource_names(state)
        .iter()
        .map(|name| NavItem {
            name: (*name).to_owned(),
            active: active == Some(name),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminPage {
    resources: Vec<NavItem>,
    tasks_active: bool,
    current: String,
    error: Option<String>,
    /// Existing items as inline-editable (or read-only) rows.
    rows: Vec<EditRow>,
    /// The "add" form, when the resource has a `<resource>-create`.
    new_form: Option<EditForm>,
    /// Standalone mutating commands that aren't lifecycle CRUD (e.g. roles grant/revoke).
    forms: Vec<EditForm>,
}

pub(super) struct NavItem {
    pub name: String,
    pub active: bool,
}

struct EditRow {
    title: String,
    /// Some → an inline update form; None → a read-only row rendered as `cells`.
    form: Option<EditForm>,
    cells: Vec<Cell>,
}

struct EditForm {
    command: String,
    label: String,
    delete_command: Option<String>,
    fields: Vec<Field>,
}

struct Cell {
    label: String,
    value: String,
}

struct Field {
    name: String,
    label: String,
    /// "text" | "bool" | "select" — drives the input the template renders.
    input: &'static str,
    options: Vec<String>,
    required: bool,
    value: String,
    checked: bool,
    readonly: bool,
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
    let list_items = run_list(&state, current).await;

    let update = state.commands.get(&format!("{current}-update"));
    let delete = state.commands.get(&format!("{current}-delete"));
    let create = state.commands.get(&format!("{current}-create"));
    let identity = update.or(delete).and_then(identity_arg);
    let delete_command = delete.map(|def| def.name.to_owned());

    let rows = list_items
        .iter()
        .map(|item| {
            build_row(
                item,
                update,
                delete_command.as_deref(),
                identity,
                &endpoint_names,
            )
        })
        .collect();
    let new_form = create.map(|def| blank_form(def, &endpoint_names));
    let forms = state
        .commands
        .commands()
        .filter(|command| command.resource == current && !is_lifecycle(command.name))
        .map(|def| blank_form(def, &endpoint_names))
        .collect();

    let page = AdminPage {
        resources: resource_nav(&state, Some(current)),
        tasks_active: false,
        current: current.to_owned(),
        error: flash.error,
        rows,
        new_form,
        forms,
    };
    Ok(render(&page))
}

/// Runs `<resource>-list` (as Host) when it takes no required args; returns its rows.
async fn run_list(state: &WebState, resource: &str) -> Vec<Value> {
    let list = format!("{resource}-list");
    let Some(def) = state.commands.get(&list) else {
        return Vec::new();
    };
    if def.args.iter().any(|arg| arg.required) {
        return Vec::new();
    }
    state
        .commands
        .dispatch(request(&list, Map::new()), CallerContext::Host)
        .await
        .data
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn build_row(
    item: &Value,
    update: Option<&CommandDef>,
    delete_command: Option<&str>,
    identity: Option<&str>,
    endpoint_names: &[String],
) -> EditRow {
    let title = identity
        .and_then(|key| item.get(key))
        .map(value_text)
        .filter(|text| !text.is_empty())
        .or_else(|| item.as_object()?.values().next().map(value_text))
        .unwrap_or_default();

    match update {
        Some(def) => {
            let fields = def
                .args
                .iter()
                .map(|arg| {
                    let value = item.get(arg.name).map(value_text).unwrap_or_default();
                    field(arg, endpoint_names, &value, Some(arg.name) == identity)
                })
                .collect();
            EditRow {
                title,
                form: Some(EditForm {
                    command: def.name.to_owned(),
                    label: "save".to_owned(),
                    delete_command: delete_command.map(str::to_owned),
                    fields,
                }),
                cells: Vec::new(),
            }
        }
        None => {
            let cells = item
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .map(|(key, value)| Cell {
                            label: key.clone(),
                            value: value_text(value),
                        })
                        .collect()
                })
                .unwrap_or_default();
            EditRow {
                title,
                form: None,
                cells,
            }
        }
    }
}

fn blank_form(def: &CommandDef, endpoint_names: &[String]) -> EditForm {
    EditForm {
        command: def.name.to_owned(),
        label: def.name.to_owned(),
        delete_command: None,
        fields: def
            .args
            .iter()
            .map(|arg| field(arg, endpoint_names, "", false))
            .collect(),
    }
}

fn identity_arg(def: &CommandDef) -> Option<&'static str> {
    def.args.iter().find(|arg| arg.required).map(|arg| arg.name)
}

fn is_lifecycle(name: &str) -> bool {
    ["-list", "-get", "-create", "-update", "-delete"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn field(arg: &ArgSpec, endpoint_names: &[String], value: &str, readonly: bool) -> Field {
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
        value: value.to_owned(),
        checked: value == "true",
        readonly,
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

pub(super) fn render<T: Template>(page: &T) -> Response {
    match page.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "admin template render failed");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
