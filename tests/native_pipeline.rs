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
use claw::protocol::entities::AgentProviderKind;
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

/// Pre-seed central.db with an endpoint + native group, then build the app on
/// the same data dir — bootstrap sees the group and leaves it alone.
fn seed_native_group(config: &Config, base_url: &str) {
    let central = CentralDb::open(&config.central_db_path()).expect("central");
    central
        .with(|conn| {
            endpoints::create(conn, &EndpointName::new("mock"), base_url)?;
            let mut group = agent_groups::create(conn, "Chat", "chat")?;
            group.agent_provider = Some(AgentProviderKind::Native);
            group.endpoint = Some(EndpointName::new("mock"));
            group.model = Some("gemma4-moe-test".to_owned());
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

#[tokio::test]
async fn browser_message_round_trips_through_the_native_provider() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: PathBuf::from(tmp.path()),
        port: 0,
        auth_token: Some(TOKEN.to_owned()),
        timezone: "UTC".to_owned(),
        default_endpoint: None,
        default_model: None,
    };
    let llm_base = mock_llm().await;
    seed_native_group(&config, &llm_base);

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
            .body(Body::from(r#"{"text":"hello model"}"#))
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
        let messages = body_json(response).await;
        let transcript = messages.as_array().expect("array").clone();
        if transcript.len() >= 2 {
            reply = Some(transcript[1].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let reply = reply.expect("native reply must arrive");
    assert_eq!(reply["direction"], "out");
    assert_eq!(
        reply["body"],
        "model saw 1 messages, last: [you] hello model"
    );

    app.shutdown().await;
}
