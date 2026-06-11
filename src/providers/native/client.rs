use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::protocol::macros::text_enum;
use crate::providers::resolution::ResolvedInference;

const COMPLETION_TIMEOUT: Duration = Duration::from_secs(300);

text_enum!(Role {
    System => "system",
    User => "user",
    Assistant => "assistant",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    #[must_use]
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("api returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("malformed completion response: {0}")]
    Malformed(String),
}

impl ClientError {
    /// Network failures and server-side errors are worth retrying; 4xx is not.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Api { status, .. } => *status == 429 || *status >= 500,
            Self::Malformed(_) => false,
        }
    }
}

/// Minimal OpenAI-compatible `/chat/completions` client — covers OpenRouter,
/// llama.cpp, vLLM, Ollama, and most hosted gateways with one protocol.
pub struct ChatClient {
    http: reqwest::Client,
    completions_url: String,
    api_key: Option<String>,
    model: String,
}

impl ChatClient {
    pub fn new(inference: &ResolvedInference) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .timeout(COMPLETION_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            completions_url: format!(
                "{}/chat/completions",
                inference.base_url.trim_end_matches('/')
            ),
            api_key: inference.api_key.clone(),
            model: inference.model.clone(),
        })
    }

    pub async fn complete(&self, messages: &[ChatMessage]) -> Result<Option<String>, ClientError> {
        let mut request = self.http.post(&self.completions_url).json(&ChatRequest {
            model: &self.model,
            messages,
        });
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(ClientError::Api {
                status: status.as_u16(),
                body: truncate(&body, 500),
            });
        }
        let parsed: ChatResponse =
            serde_json::from_str(&body).map_err(|err| ClientError::Malformed(err.to_string()))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ClientError::Malformed("response has no choices".to_owned()))?;
        Ok(choice.message.content)
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Option<String>,
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_owned()
    } else {
        let cut = text
            .char_indices()
            .take_while(|(index, _)| *index < max)
            .last()
            .map_or(0, |(index, ch)| index + ch.len_utf8());
        format!("{}…", &text[..cut])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use std::sync::{Arc, Mutex};

    type SeenRequest = (Option<String>, serde_json::Value);

    #[derive(Clone, Default)]
    struct MockState {
        seen: Arc<Mutex<Vec<SeenRequest>>>,
        respond: Arc<Mutex<Option<(u16, String)>>>,
    }

    async fn mock_server(state: MockState) -> String {
        let app = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(
                    |State(state): State<MockState>,
                     headers: HeaderMap,
                     Json(body): Json<serde_json::Value>| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        state.seen.lock().expect("lock").push((auth, body));
                        let (status, body) =
                            state.respond.lock().expect("lock").clone().unwrap_or((
                                200,
                                r#"{"choices":[{"message":{"role":"assistant","content":"hi!"}}]}"#
                                    .to_owned(),
                            ));
                        (
                            axum::http::StatusCode::from_u16(status).expect("status"),
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            body,
                        )
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://{addr}/v1")
    }

    fn client_for(base_url: String, api_key: Option<&str>) -> ChatClient {
        ChatClient::new(&ResolvedInference {
            base_url,
            api_key: api_key.map(str::to_owned),
            model: "test-model".to_owned(),
        })
        .expect("client")
    }

    #[tokio::test]
    async fn sends_model_messages_and_bearer_key() {
        let state = MockState::default();
        let base = mock_server(state.clone()).await;
        let client = client_for(base, Some("sk-test"));

        let reply = client
            .complete(&[
                ChatMessage::new(Role::System, "be brief"),
                ChatMessage::new(Role::User, "hello"),
            ])
            .await
            .expect("complete");
        assert_eq!(reply.as_deref(), Some("hi!"));

        let seen = state.seen.lock().expect("lock");
        let (auth, body) = &seen[0];
        assert_eq!(auth.as_deref(), Some("Bearer sk-test"));
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hello");
    }

    #[tokio::test]
    async fn keyless_requests_send_no_authorization_header() {
        let state = MockState::default();
        let base = mock_server(state.clone()).await;
        let client = client_for(base, None);
        client
            .complete(&[ChatMessage::new(Role::User, "hi")])
            .await
            .expect("complete");
        assert_eq!(state.seen.lock().expect("lock")[0].0, None);
    }

    #[tokio::test]
    async fn server_errors_are_retryable_and_client_errors_are_not() {
        let state = MockState::default();
        let base = mock_server(state.clone()).await;
        let client = client_for(base, None);

        *state.respond.lock().expect("lock") = Some((503, "overloaded".to_owned()));
        let err = client
            .complete(&[ChatMessage::new(Role::User, "hi")])
            .await
            .expect_err("must fail");
        assert!(err.is_retryable());

        *state.respond.lock().expect("lock") = Some((400, "bad request".to_owned()));
        let err = client
            .complete(&[ChatMessage::new(Role::User, "hi")])
            .await
            .expect_err("must fail");
        assert!(!err.is_retryable());

        *state.respond.lock().expect("lock") = Some((200, "not json".to_owned()));
        let err = client
            .complete(&[ChatMessage::new(Role::User, "hi")])
            .await
            .expect_err("must fail");
        assert!(matches!(err, ClientError::Malformed(_)));
        assert!(!err.is_retryable());
    }
}
