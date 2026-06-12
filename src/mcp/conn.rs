use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use super::{McpError, McpTool};

/// MCP protocol revision we advertise in `initialize`. Servers echo the version
/// they'll actually use; most proceed even on a minor mismatch.
const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Serialize)]
struct Request<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Serialize)]
struct Notification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ReplyError>,
}

#[derive(Deserialize)]
struct ReplyError {
    code: i64,
    message: String,
}

/// One JSON-RPC 2.0 conversation over a newline-delimited byte stream (the MCP
/// stdio transport). Generic over the IO so it can be driven by a child process's
/// stdin/stdout in production and by an in-memory duplex in tests.
pub struct Conn<W, R> {
    writer: W,
    reader: R,
    next_id: u64,
}

impl<W, R> Conn<W, R>
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    pub fn new(writer: W, reader: R) -> Self {
        Self {
            writer,
            reader,
            next_id: 1,
        }
    }

    async fn write_message(&mut self, message: &impl Serialize) -> Result<(), McpError> {
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Sends a request and reads replies until the one matching our id arrives,
    /// skipping any interleaved notifications or log lines.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        })
        .await?;

        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                return Err(McpError::Closed);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let reply: Reply = serde_json::from_str(trimmed)?;
            if reply.id != Some(id) {
                continue;
            }
            if let Some(error) = reply.error {
                return Err(McpError::Rpc {
                    code: error.code,
                    message: error.message,
                });
            }
            return Ok(reply.result.unwrap_or(Value::Null));
        }
    }

    async fn notify(&mut self, method: &str) -> Result<(), McpError> {
        self.write_message(&Notification {
            jsonrpc: "2.0",
            method,
            params: None,
        })
        .await
    }

    /// The MCP startup dance: `initialize` request, then the `initialized` notice.
    pub async fn initialize(&mut self, client_name: &str) -> Result<(), McpError> {
        self.request(
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": client_name, "version": env!("CARGO_PKG_VERSION") },
            })),
        )
        .await?;
        self.notify("notifications/initialized").await
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        let result = self.request("tools/list", Some(json!({}))).await?;
        let tools = result
            .get("tools")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        Ok(serde_json::from_value(tools)?)
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String, McpError> {
        let result = self
            .request(
                "tools/call",
                Some(json!({ "name": name, "arguments": arguments })),
            )
            .await?;
        Ok(flatten_content(&result))
    }
}

/// Reduces an MCP `tools/call` result to the text an LLM can consume: text blocks
/// joined by newlines, non-text blocks (images, …) noted but not inlined.
fn flatten_content(result: &Value) -> String {
    let Some(items) = result.get("content").and_then(Value::as_array) else {
        return result.to_string();
    };
    let mut parts = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(
                item.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            Some(other) => parts.push(format!("[{other} content omitted]")),
            None => {}
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{BufReader, split};

    /// A canned MCP server: answers initialize/tools.list/tools.call and records
    /// the tool call it saw, so a test can assert the full round trip.
    async fn mock_server(stream: tokio::io::DuplexStream) {
        let (read, mut write) = split(stream);
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&line).expect("server got json");
            let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                continue; // a notification (e.g. initialized) — nothing to answer
            };
            let result = match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => json!({"protocolVersion": "2025-06-18"}),
                Some("tools/list") => json!({"tools": [
                    {"name": "search", "description": "web search", "inputSchema": {"type": "object"}},
                    {"name": "fetch", "description": "get a url"},
                ]}),
                Some("tools/call") => {
                    let arg = msg["params"]["arguments"]["q"].as_str().unwrap_or("");
                    json!({"content": [{"type": "text", "text": format!("ran with {arg}")}]})
                }
                _ => json!(null),
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .expect("server write");
        }
    }

    fn client(
        stream: tokio::io::DuplexStream,
    ) -> Conn<impl AsyncWrite + Unpin, impl AsyncBufRead + Unpin> {
        let (read, write) = split(stream);
        Conn::new(write, BufReader::new(read))
    }

    #[tokio::test]
    async fn initialize_list_and_call_round_trip() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        tokio::spawn(mock_server(server_io));
        let mut conn = client(client_io);

        conn.initialize("claw-test").await.expect("initialize");

        let tools = conn.list_tools().await.expect("list");
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["search", "fetch"]
        );
        assert_eq!(tools[0].description, "web search");

        let out = conn
            .call_tool("search", json!({"q": "rust"}))
            .await
            .expect("call");
        assert_eq!(out, "ran with rust");
    }

    #[test]
    fn flatten_joins_text_and_notes_other_blocks() {
        let result = json!({"content": [
            {"type": "text", "text": "line one"},
            {"type": "image", "data": "..."},
            {"type": "text", "text": "line two"},
        ]});
        assert_eq!(
            flatten_content(&result),
            "line one\n[image content omitted]\nline two"
        );
    }

    #[test]
    fn flatten_falls_back_to_raw_json_without_content() {
        assert_eq!(flatten_content(&json!({"ok": true})), "{\"ok\":true}");
    }
}
