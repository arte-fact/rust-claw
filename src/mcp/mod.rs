mod conn;

use std::process::Stdio;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use conn::Conn;

/// The single MCP server claw ships with: a web toolbox (fetch/search/screenshot/
/// interact) backed by headless Chromium. Hardcoded — there is no config surface.
pub const WEB_SERVER_NAME: &str = "web";
pub const WEB_SERVER_COMMAND: &str = "mcp-web-search-stdio";

/// Joins MCP tool names under their server so they can't collide with native tools
/// (`bash`, `search` from two servers, …). The agent sees `web__search`.
#[must_use]
pub fn qualify(server: &str, tool: &str) -> String {
    format!("{server}__{tool}")
}

/// Inverse of [`qualify`]: splits a qualified name back into `(server, tool)`.
#[must_use]
pub fn dequalify(qualified: &str) -> Option<(&str, &str)> {
    qualified.split_once("__")
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("server error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("the mcp server closed the connection")]
    Closed,
    #[error("could not spawn mcp server: {0}")]
    Spawn(String),
    #[error("mcp server exposed no stdio pipes")]
    NoPipes,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// A live connection to one stdio MCP server. Holds the child process open and
/// serializes calls behind a mutex (the web server drives a single Chromium, so
/// concurrent calls would contend anyway).
pub struct McpClient {
    server_name: String,
    tools: Vec<McpTool>,
    conn: Mutex<Conn<ChildStdin, BufReader<ChildStdout>>>,
    _child: Child,
}

impl McpClient {
    /// Spawns `command`, performs the MCP handshake, and caches its tool list.
    /// Any failure (missing binary, bad handshake) is returned so the caller can
    /// run without the server rather than aborting boot.
    pub async fn spawn(server_name: &str, command: &str, args: &[&str]) -> Result<Self, McpError> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| McpError::Spawn(format!("{command}: {err}")))?;

        let stdin = child.stdin.take().ok_or(McpError::NoPipes)?;
        let stdout = child.stdout.take().ok_or(McpError::NoPipes)?;
        let mut conn = Conn::new(stdin, BufReader::new(stdout));

        conn.initialize(server_name).await?;
        let tools = conn.list_tools().await?;

        Ok(Self {
            server_name: server_name.to_owned(),
            tools,
            conn: Mutex::new(conn),
            _child: child,
        })
    }

    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    #[must_use]
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Calls a tool by its bare name (no server prefix); returns the flattened text.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<String, McpError> {
        let mut conn = self.conn.lock().await;
        conn.call_tool(tool, arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualify_and_dequalify_round_trip() {
        let q = qualify("web", "search");
        assert_eq!(q, "web__search");
        assert_eq!(dequalify(&q), Some(("web", "search")));
    }

    #[test]
    fn dequalify_rejects_unqualified_names() {
        assert_eq!(dequalify("bash"), None);
    }

    #[test]
    fn dequalify_keeps_the_tool_remainder_intact() {
        assert_eq!(
            dequalify("web__deep__search"),
            Some(("web", "deep__search"))
        );
    }
}
