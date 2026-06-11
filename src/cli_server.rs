use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::commands::{CallerContext, Dispatcher};
use crate::protocol::frame::{ErrorCode, RequestFrame, ResponseFrame};

/// The admin CLI socket: newline-delimited JSON request/response frames over a
/// `0600` unix socket. Each accepted connection is served independently; the
/// socket caller always speaks for the operator (`CallerContext::Host`).
pub struct CliServer {
    socket_path: PathBuf,
    dispatcher: Arc<dyn Dispatcher>,
}

#[derive(Debug, thiserror::Error)]
pub enum CliServerError {
    #[error("binding {path}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl CliServer {
    #[must_use]
    pub fn new(socket_path: PathBuf, dispatcher: Arc<dyn Dispatcher>) -> Self {
        Self {
            socket_path,
            dispatcher,
        }
    }

    pub async fn run(self: Arc<Self>, cancel: CancellationToken) -> Result<(), CliServerError> {
        let listener = self.bind()?;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _addr)) => {
                        let server = self.clone();
                        tokio::spawn(async move { server.serve(stream).await });
                    }
                    Err(error) => tracing::warn!(%error, "cli accept failed"),
                },
            }
        }
        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }

    fn bind(&self) -> Result<UnixListener, CliServerError> {
        // A leftover socket from a previous run would make bind fail with EADDRINUSE.
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(parent) = self.socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let listener =
            UnixListener::bind(&self.socket_path).map_err(|source| CliServerError::Bind {
                path: self.socket_path.clone(),
                source,
            })?;
        if let Err(error) =
            std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!(%error, "could not set 0600 on the cli socket");
        }
        Ok(listener)
    }

    async fn serve(&self, stream: UnixStream) {
        let mut lines = BufReader::new(stream).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => line,
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(%error, "cli read failed");
                    break;
                }
            };
            let response = self.handle(&line).await;
            let mut encoded = match serde_json::to_string(&response) {
                Ok(encoded) => encoded,
                Err(error) => {
                    tracing::error!(%error, "cli response encode failed");
                    break;
                }
            };
            encoded.push('\n');
            if lines.get_mut().write_all(encoded.as_bytes()).await.is_err() {
                break;
            }
        }
    }

    async fn handle(&self, line: &str) -> ResponseFrame {
        match serde_json::from_str::<RequestFrame>(line) {
            Ok(request) => {
                let id = request.id.clone();
                let response = self.dispatcher.dispatch(request, CallerContext::Host).await;
                // Defend the id correlation even if a dispatcher returns the wrong one.
                ResponseFrame { id, ..response }
            }
            Err(error) => ResponseFrame::error(
                "",
                ErrorCode::TransportError,
                format!("bad request: {error}"),
            ),
        }
    }
}

/// Single round-trip helper for the CLI client (M6.2) and tests: connect, send
/// one request, read one response.
pub async fn request(socket_path: &Path, request: &RequestFrame) -> std::io::Result<ResponseFrame> {
    let stream = UnixStream::connect(socket_path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut payload = serde_json::to_string(request)?;
    payload.push('\n');
    write_half.write_all(payload.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    serde_json::from_str(&line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    /// Echoes the command back, or errors on a reserved name.
    struct EchoDispatcher;

    #[async_trait]
    impl Dispatcher for EchoDispatcher {
        async fn dispatch(&self, request: RequestFrame, caller: CallerContext) -> ResponseFrame {
            if request.command == "boom" {
                return ResponseFrame::error(request.id, ErrorCode::HandlerError, "exploded");
            }
            ResponseFrame::ok(
                request.id,
                json!({ "command": request.command, "from_agent": caller.is_agent() }),
            )
        }
    }

    async fn start_server() -> (tempfile::TempDir, PathBuf, CancellationToken) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_path = tmp.path().join("claw.sock");
        let server = Arc::new(CliServer::new(
            socket_path.clone(),
            Arc::new(EchoDispatcher),
        ));
        let cancel = CancellationToken::new();
        {
            let server = server.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { server.run(cancel).await });
        }
        // Wait for the socket to appear.
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (tmp, socket_path, cancel)
    }

    #[tokio::test]
    async fn round_trips_a_request_as_the_host_caller() {
        let (_tmp, socket_path, cancel) = start_server().await;
        let response = request(
            &socket_path,
            &RequestFrame::new("r1", "groups-list", serde_json::Map::new()),
        )
        .await
        .expect("request");
        assert_eq!(response.id, "r1");
        assert!(response.ok);
        let data = response.data.expect("data");
        assert_eq!(data["command"], "groups-list");
        assert_eq!(data["from_agent"], false);
        cancel.cancel();
    }

    #[tokio::test]
    async fn handler_errors_become_error_frames() {
        let (_tmp, socket_path, cancel) = start_server().await;
        let response = request(
            &socket_path,
            &RequestFrame::new("r2", "boom", serde_json::Map::new()),
        )
        .await
        .expect("request");
        assert!(!response.ok);
        assert_eq!(response.error.expect("error").code, ErrorCode::HandlerError);
        cancel.cancel();
    }

    #[tokio::test]
    async fn malformed_input_yields_a_transport_error() {
        let (_tmp, socket_path, cancel) = start_server().await;
        let stream = UnixStream::connect(&socket_path).await.expect("connect");
        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(b"not json\n").await.expect("write");
        let mut line = String::new();
        BufReader::new(read_half)
            .read_line(&mut line)
            .await
            .expect("read");
        let response: ResponseFrame = serde_json::from_str(&line).expect("parse");
        assert_eq!(
            response.error.expect("error").code,
            ErrorCode::TransportError
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn socket_is_created_0600_and_a_stale_socket_is_replaced() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_path = tmp.path().join("claw.sock");
        // Plant a stale file where the socket should go.
        std::fs::write(&socket_path, b"stale").expect("plant");

        let server = Arc::new(CliServer::new(
            socket_path.clone(),
            Arc::new(EchoDispatcher),
        ));
        let cancel = CancellationToken::new();
        {
            let server = server.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { server.run(cancel).await });
        }
        for _ in 0..100 {
            if request(
                &socket_path,
                &RequestFrame::new("r", "ping", serde_json::Map::new()),
            )
            .await
            .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let mode = std::fs::metadata(&socket_path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "socket must be owner-only");
        cancel.cancel();
    }
}
