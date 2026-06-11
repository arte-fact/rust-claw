use std::path::PathBuf;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use claw::config::Config;
use claw::db::{CentralDb, agent_groups, endpoints};
use claw::protocol::entities::{AgentProviderKind, CliScope, ToolProfile};
use claw::protocol::ids::EndpointName;

const TOKEN: &str = "e2e-token";

/// Mock OpenAI-compatible server: replies with a summary of what it received.
async fn mock_llm() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let message_count = body["messages"].as_array().map_or(0, Vec::len);
            let last_user = body["messages"]
                .as_array()
                .and_then(|messages| {
                    messages
                        .iter()
                        .rev()
                        .find(|message| message["role"] == "user")
                })
                .and_then(|message| message["content"].as_str())
                .unwrap_or_default()
                .to_owned();
            Json(serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": format!("model saw {message_count} messages, last: {last_user}")
                    }
                }]
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/v1")
}

/// Mock that drives the tool loop: a `send_message` tool call on the first
/// round (no `tool` role yet), then a bare acknowledgement once the result
/// comes back — proving the full assistant↔tool round-trip.
async fn mock_llm_send_message_tool() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let already_called_tool = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            let response = if already_called_tool {
                serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": "done" } }]
                })
            } else {
                serde_json::json!({
                    "choices": [{ "message": {
                        "role": "assistant",
                        "content": Value::Null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "send_message",
                                "arguments": "{\"text\":\"hi from a tool call\"}"
                            }
                        }]
                    }}]
                })
            };
            Json(response)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/v1")
}

/// Mock for the schedule→fire→reply lifecycle. When the conversation already
/// contains the fired task ("[scheduled task]"), it replies with the reminder.
/// Otherwise it schedules an immediately-due task, then acknowledges.
async fn mock_llm_scheduler() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            let task_fired = messages.iter().any(|m| {
                m["role"] == "user"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("[scheduled task]"))
            });
            let called_tool = messages.iter().any(|m| m["role"] == "tool");

            let response = if task_fired {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": "reminder: drink water"
                }}]})
            } else if called_tool {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": "scheduled it"
                }}]})
            } else {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": Value::Null,
                    "tool_calls": [{"id": "c1", "type": "function", "function": {
                        "name": "schedule_task",
                        "arguments": "{\"prompt\":\"drink water\",\"process_after\":\"2020-01-01T00:00:00.000Z\"}"
                    }}]
                }}]})
            };
            Json(response)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/v1")
}

/// Mock that drives a coder turn: `bash` first (no tool role yet), then `edit`
/// once the bash result is back, then a plain reply — proving multi-round tool
/// use with bash + a file edit in one turn.
async fn mock_llm_coder() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let names: Vec<String> = body["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["role"] == "tool")
                .filter_map(|m| m["content"].as_str().map(str::to_owned))
                .collect();
            let tool_call = |name: &str, args: &str| {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": Value::Null,
                    "tool_calls": [{"id": "c1", "type": "function",
                        "function": {"name": name, "arguments": args}}]
                }}]})
            };
            let response = if names.is_empty() {
                tool_call("bash", r#"{"command":"echo original > note.txt"}"#)
            } else if names.len() == 1 {
                tool_call(
                    "edit",
                    r#"{"path":"note.txt","old_string":"original","new_string":"edited"}"#,
                )
            } else {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": "done — note.txt now says edited"
                }}]})
            };
            Json(response)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/v1")
}

/// Pre-seed central.db with an endpoint + native group, then build the app on
/// the same data dir — bootstrap sees the group and leaves it alone.
fn seed_native_group(config: &Config, base_url: &str) {
    seed_group(config, base_url, ToolProfile::Chat);
}

fn seed_group(config: &Config, base_url: &str, profile: ToolProfile) {
    let central = CentralDb::open(&config.central_db_path()).expect("central");
    central
        .with(|conn| {
            endpoints::create(conn, &EndpointName::new("mock"), base_url)?;
            let mut group = agent_groups::create(conn, "Agent", "agent")?;
            group.agent_provider = Some(AgentProviderKind::Native);
            group.endpoint = Some(EndpointName::new("mock"));
            group.model = Some("test-model".to_owned());
            group.tool_profile = profile;
            agent_groups::update(conn, &group)?;
            Ok(())
        })
        .expect("seed");
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("json body")
}

async fn login(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::post("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("token={TOKEN}")))
                .expect("request"),
        )
        .await
        .expect("login");
    response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("cookie")
        .split(';')
        .next()
        .expect("pair")
        .to_owned()
}

/// Posts one user message to a fresh chat against a native group backed by
/// `llm_base`, then polls until the agent's reply lands. Returns its body.
async fn first_reply_body(llm_base: &str, user_text: &str) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: PathBuf::from(tmp.path()),
        port: 0,
        auth_token: Some(TOKEN.to_owned()),
        timezone: "UTC".to_owned(),
        default_endpoint: None,
        default_model: None,
    };
    seed_native_group(&config, llm_base);

    let app = claw::app::build(&config).await.expect("build app");
    let cookie = login(&app.http).await;
    let request = |req: Request<Body>| {
        let app = app.http.clone();
        async move { app.oneshot(req).await.expect("response") }
    };

    let created = request(
        Request::post("/api/chats")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"name":"Main"}"#))
            .expect("request"),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let chat_id = body_json(created).await["platform_id"]
        .as_str()
        .expect("platform_id")
        .to_owned();

    let posted = request(
        Request::post(format!("/api/chats/{chat_id}/messages"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(
                r#"{{"text":{}}}"#,
                json_string(user_text)
            )))
            .expect("request"),
    )
    .await;
    assert_eq!(posted.status(), StatusCode::OK);

    let mut reply = None;
    for _ in 0..100 {
        let response = request(
            Request::get(format!("/api/chats/{chat_id}/messages"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        let transcript = body_json(response).await.as_array().expect("array").clone();
        if let Some(out) = transcript.iter().find(|m| m["direction"] == "out") {
            reply = Some(out["body"].as_str().expect("body").to_owned());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    app.shutdown().await;
    reply.expect("native reply must arrive")
}

fn json_string(value: &str) -> String {
    Value::String(value.to_owned()).to_string()
}

#[tokio::test]
async fn fallback_path_turns_plain_text_into_a_reply() {
    let llm_base = mock_llm().await;
    let body = first_reply_body(&llm_base, "hello model").await;
    // Two messages: the scaffolded AGENT.md system prompt (M9.1) + the user turn.
    assert_eq!(body, "model saw 2 messages, last: [you] hello model");
}

#[tokio::test]
async fn tool_call_path_sends_a_message_via_the_send_message_tool() {
    let llm_base = mock_llm_send_message_tool().await;
    let body = first_reply_body(&llm_base, "say hi").await;
    assert_eq!(body, "hi from a tool call");
}

/// Schedule a due task → the drain loop fires it in the same run → the agent
/// replies to the fired task. Proves the full schedule→fire→reply lifecycle
/// without waiting on the 60s sweep (the task is already due).
#[tokio::test]
async fn scheduled_task_fires_and_the_agent_replies() {
    let llm_base = mock_llm_scheduler().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: PathBuf::from(tmp.path()),
        port: 0,
        auth_token: Some(TOKEN.to_owned()),
        timezone: "UTC".to_owned(),
        default_endpoint: None,
        default_model: None,
    };
    seed_native_group(&config, &llm_base);

    let app = claw::app::build(&config).await.expect("build app");
    let cookie = login(&app.http).await;
    let request = |req: Request<Body>| {
        let app = app.http.clone();
        async move { app.oneshot(req).await.expect("response") }
    };

    let chat_id = body_json(
        request(
            Request::post("/api/chats")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Main"}"#))
                .expect("request"),
        )
        .await,
    )
    .await["platform_id"]
        .as_str()
        .expect("platform_id")
        .to_owned();

    request(
        Request::post(format!("/api/chats/{chat_id}/messages"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"text":"remind me to drink water"}"#))
            .expect("request"),
    )
    .await;

    // Poll until both the acknowledgement and the fired-task reminder have landed.
    let mut bodies: Vec<String> = Vec::new();
    for _ in 0..100 {
        let response = request(
            Request::get(format!("/api/chats/{chat_id}/messages"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        bodies = body_json(response)
            .await
            .as_array()
            .expect("array")
            .iter()
            .filter(|m| m["direction"] == "out")
            .filter_map(|m| m["body"].as_str().map(str::to_owned))
            .collect();
        if bodies.iter().any(|b| b.contains("reminder: drink water")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        bodies.iter().any(|b| b == "scheduled it"),
        "acknowledgement expected, got {bodies:?}"
    );
    assert!(
        bodies.iter().any(|b| b == "reminder: drink water"),
        "fired-task reminder expected, got {bodies:?}"
    );

    app.shutdown().await;
}

#[tokio::test]
async fn coder_group_runs_bash_then_edit_in_the_workspace_then_replies() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: PathBuf::from(tmp.path()),
        port: 0,
        auth_token: Some(TOKEN.to_owned()),
        timezone: "UTC".to_owned(),
        default_endpoint: None,
        default_model: None,
    };
    let llm_base = mock_llm_coder().await;
    seed_group(&config, &llm_base, ToolProfile::Coder);

    let app = claw::app::build(&config).await.expect("build app");
    let cookie = login(&app.http).await;
    let request = |req: Request<Body>| {
        let app = app.http.clone();
        async move { app.oneshot(req).await.expect("response") }
    };

    let chat_id = body_json(
        request(
            Request::post("/api/chats")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Work"}"#))
                .expect("request"),
        )
        .await,
    )
    .await["platform_id"]
        .as_str()
        .expect("platform_id")
        .to_owned();

    request(
        Request::post(format!("/api/chats/{chat_id}/messages"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"text":"make the note say edited"}"#))
            .expect("request"),
    )
    .await;

    let mut reply = None;
    for _ in 0..100 {
        let response = request(
            Request::get(format!("/api/chats/{chat_id}/messages"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        let transcript = body_json(response).await.as_array().expect("array").clone();
        if let Some(out) = transcript.iter().find(|m| m["direction"] == "out") {
            reply = Some(out["body"].as_str().expect("body").to_owned());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(reply.as_deref(), Some("done — note.txt now says edited"));
    // The tools really ran in the group workspace: bash created note.txt, edit changed it.
    let note = tmp.path().join("groups/agent/note.txt");
    assert_eq!(
        std::fs::read_to_string(&note).expect("note.txt must exist"),
        "edited\n"
    );

    app.shutdown().await;
}

/// Run a coder group against a real OpenAI-compatible endpoint.
/// `CLAW_TEST_ENDPOINT` (e.g. http://localhost:8000/v1), `CLAW_TEST_MODEL`,
/// optional `CLAW_TEST_API_KEY`. Ignored by default (needs a live model).
#[tokio::test]
#[ignore = "requires a real LLM endpoint via CLAW_TEST_ENDPOINT/CLAW_TEST_MODEL"]
async fn coder_group_against_a_real_endpoint() {
    let Ok(base_url) = std::env::var("CLAW_TEST_ENDPOINT") else {
        return;
    };
    let model = std::env::var("CLAW_TEST_MODEL").expect("CLAW_TEST_MODEL");

    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: PathBuf::from(tmp.path()),
        port: 0,
        auth_token: Some(TOKEN.to_owned()),
        timezone: "UTC".to_owned(),
        default_endpoint: None,
        default_model: None,
    };
    let central = CentralDb::open(&config.central_db_path()).expect("central");
    central
        .with(|conn| {
            let endpoint = EndpointName::new("real");
            endpoints::create(conn, &endpoint, &base_url)?;
            if let Ok(key) = std::env::var("CLAW_TEST_API_KEY") {
                let mut row = endpoints::get(conn, &endpoint)?.expect("endpoint");
                row.api_key = Some(key);
                endpoints::update(conn, &row)?;
            }
            let mut group = agent_groups::create(conn, "Coder", "agent")?;
            group.agent_provider = Some(AgentProviderKind::Native);
            group.endpoint = Some(endpoint);
            group.model = Some(model);
            group.tool_profile = ToolProfile::Coder;
            agent_groups::update(conn, &group)?;
            Ok(())
        })
        .expect("seed");

    let app = claw::app::build(&config).await.expect("build app");
    let cookie = login(&app.http).await;
    let request = |req: Request<Body>| {
        let app = app.http.clone();
        async move { app.oneshot(req).await.expect("response") }
    };

    let chat_id = body_json(
        request(
            Request::post("/api/chats")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Work"}"#))
                .expect("request"),
        )
        .await,
    )
    .await["platform_id"]
        .as_str()
        .expect("platform_id")
        .to_owned();

    request(
        Request::post(format!("/api/chats/{chat_id}/messages"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"text":"create a file hello.txt containing the word banana, then tell me you did it"}"#,
            ))
            .expect("request"),
    )
    .await;

    let mut replied = false;
    for _ in 0..600 {
        let response = request(
            Request::get(format!("/api/chats/{chat_id}/messages"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        let transcript = body_json(response).await.as_array().expect("array").clone();
        if transcript.iter().any(|m| m["direction"] == "out") {
            replied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(replied, "the agent must reply");

    let hello = std::fs::read_to_string(tmp.path().join("groups/agent/hello.txt"))
        .expect("hello.txt must exist");
    assert!(
        hello.to_lowercase().contains("banana"),
        "file should contain banana, got: {hello}"
    );

    app.shutdown().await;
}

/// Mock that drives an admin-command turn: an `admin` tool call updating the
/// agent's own group model (no `id` — the group `cli_scope` auto-fills it), then
/// a plain reply once the command result is back.
async fn mock_llm_admin() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let called_tool = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
            let response = if called_tool {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": "updated my model"
                }}]})
            } else {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": Value::Null,
                    "tool_calls": [{"id": "c1", "type": "function", "function": {
                        "name": "admin",
                        "arguments": "{\"command\":\"groups-update\",\"args\":{\"model\":\"swapped-by-agent\"}}"
                    }}]
                }}]})
            };
            Json(response)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/v1")
}

/// Full chain: supervisor hands the run admin access (group `cli_scope`), the
/// native loop calls the `admin` tool, the dispatcher executes `groups-update`
/// as the agent (own-group auto-filled, M6.3), and the model is changed in the DB.
#[tokio::test]
async fn agent_runs_an_admin_command_through_the_dispatcher() {
    let llm_base = mock_llm_admin().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: PathBuf::from(tmp.path()),
        port: 0,
        auth_token: Some(TOKEN.to_owned()),
        timezone: "UTC".to_owned(),
        default_endpoint: None,
        default_model: None,
    };
    seed_native_group(&config, &llm_base);

    let app = claw::app::build(&config).await.expect("build app");
    let cookie = login(&app.http).await;
    let request = |req: Request<Body>| {
        let app = app.http.clone();
        async move { app.oneshot(req).await.expect("response") }
    };

    let chat_id = body_json(
        request(
            Request::post("/api/chats")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Main"}"#))
                .expect("request"),
        )
        .await,
    )
    .await["platform_id"]
        .as_str()
        .expect("platform_id")
        .to_owned();

    request(
        Request::post(format!("/api/chats/{chat_id}/messages"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"text":"switch your model"}"#))
            .expect("request"),
    )
    .await;

    let mut replied = false;
    for _ in 0..100 {
        let response = request(
            Request::get(format!("/api/chats/{chat_id}/messages"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        let transcript = body_json(response).await.as_array().expect("array").clone();
        if transcript.iter().any(|m| m["direction"] == "out") {
            replied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        replied,
        "the agent must reply after running the admin command"
    );

    app.shutdown().await;

    let central = CentralDb::open(&config.central_db_path()).expect("central");
    let model = central
        .with(|conn| Ok(agent_groups::list(conn)?.remove(0).model))
        .expect("group");
    assert_eq!(
        model.as_deref(),
        Some("swapped-by-agent"),
        "the admin command must have changed the group model through the full chain"
    );
}

/// Mock for the approval lifecycle: call the `admin` tool to delete an endpoint
/// (round 1), acknowledge once it is "submitted for approval" (round 2), then
/// confirm once the `[system …]` result of the approved command comes back.
async fn mock_llm_approval() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            let saw_system = messages.iter().any(|m| {
                m["role"] == "user"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("[system"))
            });
            let called_tool = messages.iter().any(|m| m["role"] == "tool");
            let response = if saw_system {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": "the endpoint is gone"
                }}]})
            } else if called_tool {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": "awaiting your approval"
                }}]})
            } else {
                serde_json::json!({"choices": [{"message": {
                    "role": "assistant", "content": Value::Null,
                    "tool_calls": [{"id": "c1", "type": "function", "function": {
                        "name": "admin",
                        "arguments": "{\"command\":\"endpoints-delete\",\"args\":{\"name\":\"victim\"}}"
                    }}]
                }}]})
            };
            Json(response)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/v1")
}

/// Full M7.2 chain: a global-`cli_scope` agent asks to delete an endpoint, the
/// destructive command is held (not run), an approval card appears, and only the
/// owner's Allow actually deletes the endpoint.
#[tokio::test]
async fn agent_destructive_command_waits_for_owner_approval() {
    let llm_base = mock_llm_approval().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: PathBuf::from(tmp.path()),
        port: 0,
        auth_token: Some(TOKEN.to_owned()),
        timezone: "UTC".to_owned(),
        default_endpoint: None,
        default_model: None,
    };
    seed_native_group(&config, &llm_base);
    // Give the group global scope (endpoints aren't group-scoped) and a victim.
    {
        let central = CentralDb::open(&config.central_db_path()).expect("central");
        central
            .with(|conn| {
                let mut group = agent_groups::list(conn)?.remove(0);
                group.cli_scope = CliScope::Global;
                agent_groups::update(conn, &group)?;
                endpoints::create(conn, &EndpointName::new("victim"), "https://victim")?;
                Ok(())
            })
            .expect("patch");
    }

    let app = claw::app::build(&config).await.expect("build app");
    let cookie = login(&app.http).await;
    let request = |req: Request<Body>| {
        let app = app.http.clone();
        async move { app.oneshot(req).await.expect("response") }
    };

    let chat_id = body_json(
        request(
            Request::post("/api/chats")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Main"}"#))
                .expect("request"),
        )
        .await,
    )
    .await["platform_id"]
        .as_str()
        .expect("platform_id")
        .to_owned();

    request(
        Request::post(format!("/api/chats/{chat_id}/messages"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"text":"delete the victim endpoint"}"#))
            .expect("request"),
    )
    .await;

    // Wait for the approval card; read its approval id from the card row.
    let mut approval_id = None;
    for _ in 0..100 {
        let messages = body_json(
            request(
                Request::get(format!("/api/chats/{chat_id}/messages"))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await,
        )
        .await;
        if let Some(card) = messages
            .as_array()
            .expect("array")
            .iter()
            .find(|m| m["kind"] == "approval")
        {
            approval_id = Some(card["question_id"].as_str().expect("id").to_owned());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let approval_id = approval_id.expect("an approval card must appear");

    // Still held: the endpoint is not deleted before approval.
    {
        let central = CentralDb::open(&config.central_db_path()).expect("central");
        let exists = central
            .with(|conn| endpoints::get(conn, &EndpointName::new("victim")))
            .expect("get")
            .is_some();
        assert!(exists, "the endpoint must survive until approved");
    }

    // The owner allows it.
    let allowed = request(
        Request::post(format!("/api/approvals/{approval_id}/answer"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"decision":"Allow"}"#))
            .expect("request"),
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::OK);

    app.shutdown().await;

    let central = CentralDb::open(&config.central_db_path()).expect("central");
    let gone = central
        .with(|conn| endpoints::get(conn, &EndpointName::new("victim")))
        .expect("get")
        .is_none();
    assert!(gone, "the approved delete must have run");
}
