use std::path::Path;

use serde_json::{Map, Value};

use crate::cli_server::request;
use crate::protocol::frame::RequestFrame;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("usage: claw <resource> <verb> [--key value …]")]
    Usage,
    #[error("could not reach the daemon at {path} ({source}). Is `claw serve` running?")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Failed(String),
}

/// Runs one admin command against the daemon socket and prints the result.
/// `argv` is the tokens after `claw` (e.g. `["groups", "list"]`).
pub async fn run(socket_path: &Path, argv: &[String]) -> Result<(), CliError> {
    let (command, args) = parse(argv)?;
    let frame = RequestFrame::new("cli", command, args);
    let response = request(socket_path, &frame)
        .await
        .map_err(|source| CliError::Connect {
            path: socket_path.display().to_string(),
            source,
        })?;

    if response.ok {
        print!("{}", render(response.data.unwrap_or(Value::Null)));
        Ok(())
    } else {
        let error = response.error.expect("error frame must carry an error");
        Err(CliError::Failed(format!(
            "{}: {}",
            error.code, error.message
        )))
    }
}

/// `["groups", "list", "--name", "x"]` → ("groups-list", {name: "x"}).
/// Supports `--key value`, `--key=value`, and bare `--flag` (→ true).
fn parse(argv: &[String]) -> Result<(String, Map<String, Value>), CliError> {
    let resource = argv.first().ok_or(CliError::Usage)?;
    let verb = argv.get(1).ok_or(CliError::Usage)?;
    let command = format!("{resource}-{verb}");

    let mut args = Map::new();
    let mut rest = argv[2..].iter();
    while let Some(token) = rest.next() {
        let Some(flag) = token.strip_prefix("--") else {
            return Err(CliError::Usage);
        };
        if let Some((key, value)) = flag.split_once('=') {
            args.insert(key.to_owned(), Value::String(value.to_owned()));
        } else if let Some(value) = rest.clone().next().filter(|v| !v.starts_with("--")) {
            args.insert(flag.to_owned(), Value::String(value.clone()));
            rest.next();
        } else {
            args.insert(flag.to_owned(), Value::Bool(true));
        }
    }
    Ok((command, args))
}

/// Pretty-prints response data: arrays of objects as a table, a single object as
/// aligned key/value lines, anything else as compact JSON.
fn render(data: Value) -> String {
    match data {
        Value::Array(rows) if rows.iter().all(Value::is_object) => render_table(&rows),
        Value::Object(map) => render_object(&map),
        Value::Null => String::new(),
        other => format!("{other}\n"),
    }
}

fn render_table(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "(none)\n".to_owned();
    }
    let mut columns: Vec<String> = Vec::new();
    for row in rows {
        if let Some(object) = row.as_object() {
            for key in object.keys() {
                if !columns.iter().any(|c| c == key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    let mut widths: Vec<usize> = columns.iter().map(String::len).collect();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let cell = cell_text(row.get(column));
                    widths[index] = widths[index].max(cell.len());
                    cell
                })
                .collect()
        })
        .collect();

    let mut out = String::new();
    push_row(&mut out, &columns, &widths);
    for row in &cells {
        push_row(&mut out, row, &widths);
    }
    out
}

fn render_object(map: &Map<String, Value>) -> String {
    let width = map.keys().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    for (key, value) in map {
        out.push_str(&format!("{key:<width$}  {}\n", cell_text(Some(value))));
    }
    out
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let line: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
        .collect();
    out.push_str(line.join("  ").trim_end());
    out.push('\n');
}

fn cell_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "-".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn parses_resource_verb_and_flags() {
        let (command, args) = parse(&argv(&[
            "endpoints",
            "create",
            "--name",
            "openrouter",
            "--base_url=https://x/v1",
            "--verbose",
        ]))
        .expect("parse");
        assert_eq!(command, "endpoints-create");
        assert_eq!(args["name"], "openrouter");
        assert_eq!(args["base_url"], "https://x/v1");
        assert_eq!(args["verbose"], true);
    }

    #[test]
    fn too_few_tokens_is_a_usage_error() {
        assert!(matches!(parse(&argv(&["groups"])), Err(CliError::Usage)));
    }

    #[test]
    fn renders_a_table_with_a_header_and_dashes_for_nulls() {
        let table = render(json!([
            { "name": "a", "model": "gemma" },
            { "name": "b", "model": null },
        ]));
        let lines: Vec<&str> = table.lines().collect();
        // Columns are sorted (serde_json has no preserve_order feature).
        assert!(
            lines[0].contains("name") && lines[0].contains("model"),
            "{:?}",
            lines[0]
        );
        assert!(table.contains("gemma"));
        assert!(
            lines.iter().any(|l| l.contains('-')),
            "null renders as a dash"
        );
        assert_eq!(lines.len(), 3, "header + two rows");
    }

    #[test]
    fn empty_array_renders_none() {
        assert_eq!(render(json!([])), "(none)\n");
    }

    #[test]
    fn single_object_renders_key_values() {
        let out = render(json!({ "name": "openrouter", "has_api_key": true }));
        assert!(out.contains("name"));
        assert!(out.contains("openrouter"));
        assert!(out.contains("true"));
    }

    /// Full path: a request goes CLI frame → socket → registry → DB and back.
    #[tokio::test]
    async fn registry_is_reachable_over_the_socket() {
        use crate::cli_server::CliServer;
        use crate::commands::Registry;
        use crate::db::CentralDb;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_path = tmp.path().join("claw.sock");
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let server = Arc::new(CliServer::new(
            socket_path.clone(),
            Arc::new(Registry::new(central)),
        ));
        let cancel = CancellationToken::new();
        {
            let server = server.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { server.run(cancel).await });
        }
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let (command, create_args) = parse(&argv(&[
            "endpoints",
            "create",
            "--name",
            "local",
            "--base_url",
            "http://localhost:8000/v1",
        ]))
        .expect("parse");
        let created = request(&socket_path, &RequestFrame::new("1", command, create_args))
            .await
            .expect("request");
        assert!(created.ok, "{:?}", created.error);

        let listed = request(
            &socket_path,
            &RequestFrame::new("2", "endpoints-list", Map::new()),
        )
        .await
        .expect("request");
        assert_eq!(
            listed.data.expect("data").as_array().expect("array").len(),
            1
        );
        cancel.cancel();
    }
}
