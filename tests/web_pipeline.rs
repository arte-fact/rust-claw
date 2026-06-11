use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use claw::config::Config;

const TOKEN: &str = "e2e-token";

fn test_config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        port: 0,
        auth_token: Some(TOKEN.to_owned()),
        timezone: "UTC".to_owned(),
        default_endpoint: None,
        default_model: None,
    }
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

struct Client {
    app: Router,
    cookie: String,
}

impl Client {
    async fn login(app: Router) -> Self {
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
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("cookie")
            .split(';')
            .next()
            .expect("cookie pair")
            .to_owned();
        Self { app, cookie }
    }

    async fn get(&self, path: &str) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::get(path)
                    .header(header::COOKIE, &self.cookie)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    async fn post_json(&self, path: &str, body: &Value) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::post(path)
                    .header(header::COOKIE, &self.cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    async fn post_form(&self, path: &str, body: &str) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::post(path)
                    .header(header::COOKIE, &self.cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_owned()))
                    .expect("request"),
            )
            .await
            .expect("response")
    }
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

#[tokio::test]
async fn browser_message_round_trips_through_the_echo_agent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config(tmp.path().to_path_buf());
    let app = claw::app::build(&config).await.expect("build app");
    let client = Client::login(app.http.clone()).await;

    let created = client
        .post_json("/api/chats", &serde_json::json!({"name": "Main"}))
        .await;
    assert_eq!(created.status(), StatusCode::OK);
    let chat = body_json(created).await;
    let chat_id = chat["platform_id"]
        .as_str()
        .expect("platform_id")
        .to_owned();

    let listed = body_json(client.get("/api/chats").await).await;
    assert_eq!(listed.as_array().expect("array").len(), 1);

    let posted = client
        .post_json(
            &format!("/api/chats/{chat_id}/messages"),
            &serde_json::json!({"text": "hello agent"}),
        )
        .await;
    assert_eq!(posted.status(), StatusCode::OK);

    let messages_path = format!("/api/chats/{chat_id}/messages");
    let mut transcript = Vec::new();
    for _ in 0..100 {
        let response = client.get(&messages_path).await;
        assert_eq!(response.status(), StatusCode::OK);
        let messages = body_json(response).await;
        transcript = messages.as_array().expect("array").clone();
        if transcript.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(transcript.len(), 2, "user message and echo reply expected");
    assert_eq!(transcript[0]["direction"], "in");
    assert_eq!(transcript[0]["body"], "hello agent");
    assert_eq!(transcript[1]["direction"], "out");
    assert_eq!(transcript[1]["sender"], "assistant");
    assert_eq!(transcript[1]["body"], "hello agent");

    app.shutdown().await;
}

#[tokio::test]
async fn admin_renders_resources_and_runs_commands_as_host() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config(tmp.path().to_path_buf());
    let app = claw::app::build(&config).await.expect("build app");
    let client = Client::login(app.http.clone()).await;

    // The endpoints page lists the (empty) table and a registry-driven create form.
    let page = body_text(client.get("/admin/endpoints").await).await;
    assert!(page.contains("endpoints-create"), "create form must render");
    assert!(
        page.contains("/admin/run"),
        "forms post to the run endpoint"
    );

    // Creating an endpoint through the generic run endpoint works (as Host).
    let created = client
        .post_form(
            "/admin/run",
            "command=endpoints-create&name=openrouter&base_url=https://openrouter.ai/api/v1",
        )
        .await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);

    let page = body_text(client.get("/admin/endpoints").await).await;
    assert!(page.contains("openrouter"), "the new endpoint must appear");

    // The groups page renders Enum fields (cli_scope) as a select.
    let groups = body_text(client.get("/admin/groups").await).await;
    assert!(groups.contains("groups-update"));
    assert!(groups.contains("<select name=\"cli_scope\""));

    // Hidden commands are operator-only, not agent-only: the admin shows roles-grant.
    let roles = body_text(client.get("/admin/roles").await).await;
    assert!(roles.contains("roles-grant"));

    // An unknown resource redirects to the default page.
    assert_eq!(
        client.get("/admin/nope").await.status(),
        StatusCode::SEE_OTHER
    );

    app.shutdown().await;
}

#[tokio::test]
async fn archiving_a_chat_drops_it_from_the_active_list() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config(tmp.path().to_path_buf());
    let app = claw::app::build(&config).await.expect("build app");
    let client = Client::login(app.http.clone()).await;

    let chat_id = body_json(
        client
            .post_json("/api/chats", &serde_json::json!({"name": "Main"}))
            .await,
    )
    .await["platform_id"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_eq!(
        body_json(client.get("/api/chats").await)
            .await
            .as_array()
            .expect("array")
            .len(),
        1
    );

    // Archive → gone from the active list, but the shell still shows it under "archived".
    let archived = client
        .post_json(
            &format!("/api/chats/{chat_id}/archive"),
            &serde_json::json!({"archived": true}),
        )
        .await;
    assert_eq!(archived.status(), StatusCode::NO_CONTENT);
    assert!(
        body_json(client.get("/api/chats").await)
            .await
            .as_array()
            .expect("array")
            .is_empty(),
        "archived chat must leave the active list"
    );
    let shell = body_text(client.get("/").await).await;
    assert!(shell.contains("archived (1)"), "archived section shows it");

    // Unarchive → back in the active list.
    let restored = client
        .post_json(
            &format!("/api/chats/{chat_id}/archive"),
            &serde_json::json!({"archived": false}),
        )
        .await;
    assert_eq!(restored.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        body_json(client.get("/api/chats").await)
            .await
            .as_array()
            .expect("array")
            .len(),
        1
    );

    app.shutdown().await;
}

#[tokio::test]
async fn admin_creates_an_agent_with_its_own_chat() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config(tmp.path().to_path_buf());
    let app = claw::app::build(&config).await.expect("build app");
    let client = Client::login(app.http.clone()).await;

    // groups-create is approval-gated for agents, but the operator (Host) runs it directly.
    let created = client
        .post_form("/admin/run", "command=groups-create&name=Researcher")
        .await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);

    // The new agent comes with its own wired chat, visible in the chat list…
    let chats = body_json(client.get("/api/chats").await).await;
    assert!(
        chats
            .as_array()
            .expect("array")
            .iter()
            .any(|chat| chat["name"] == "Researcher"),
        "the new agent's chat must appear"
    );
    // …and the group itself in the admin table.
    let groups = body_text(client.get("/admin/groups").await).await;
    assert!(groups.contains("Researcher"));

    app.shutdown().await;
}

#[tokio::test]
async fn admin_tasks_lists_and_controls_scheduled_tasks() {
    use claw::db::{CentralDb, sessions};
    use claw::session::SessionStore;

    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config(tmp.path().to_path_buf());
    let app = claw::app::build(&config).await.expect("build app");
    let client = Client::login(app.http.clone()).await;

    let chat = body_json(
        client
            .post_json("/api/chats", &serde_json::json!({"name": "Main"}))
            .await,
    )
    .await;
    let chat_id = chat["platform_id"].as_str().expect("id").to_owned();

    // Spawn the session via a posted message, then wait for the echo reply.
    client
        .post_json(
            &format!("/api/chats/{chat_id}/messages"),
            &serde_json::json!({"text": "hi"}),
        )
        .await;
    let messages_path = format!("/api/chats/{chat_id}/messages");
    for _ in 0..100 {
        if body_json(client.get(&messages_path).await)
            .await
            .as_array()
            .expect("array")
            .len()
            >= 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Seed a recurring task (far-future first fire so the supervisor leaves it pending).
    let (series, session_id, agent_group_id) = {
        let central = CentralDb::open(&config.central_db_path()).expect("central");
        let session = central
            .with(|conn| Ok(sessions::list_active(conn)?.remove(0)))
            .expect("session");
        let store = SessionStore::new(config.sessions_dir());
        let db = store
            .open(&session.agent_group_id, &session.id)
            .expect("open session db");
        let series = db
            .schedule_task(
                "daily briefing",
                Some("2999-01-01T00:00:00.000Z"),
                Some("0 9 * * *"),
            )
            .expect("schedule");
        (
            series,
            session.id.as_str().to_owned(),
            session.agent_group_id.as_str().to_owned(),
        )
    };

    let page = body_text(client.get("/admin/tasks").await).await;
    assert!(page.contains("daily briefing"), "task prompt must list");
    assert!(page.contains("0 9 * * *"), "cron schedule must list");
    assert!(page.contains(&series));

    let action = |verb: &str| {
        format!(
            "session_id={session_id}&agent_group_id={agent_group_id}&series={series}&action={verb}"
        )
    };

    // Pause → the task shows as paused.
    let paused = client
        .post_form("/admin/tasks/action", &action("pause"))
        .await;
    assert_eq!(paused.status(), StatusCode::SEE_OTHER);
    let page = body_text(client.get("/admin/tasks").await).await;
    assert!(page.contains("paused"));

    // Cancel → the task is gone.
    let cancelled = client
        .post_form("/admin/tasks/action", &action("cancel"))
        .await;
    assert_eq!(cancelled.status(), StatusCode::SEE_OTHER);
    let page = body_text(client.get("/admin/tasks").await).await;
    assert!(!page.contains(&series), "cancelled task must disappear");

    app.shutdown().await;
}

#[tokio::test]
async fn unknown_chat_returns_404() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config(tmp.path().to_path_buf());
    let app = claw::app::build(&config).await.expect("build app");
    let client = Client::login(app.http.clone()).await;

    let response = client.get("/api/chats/nope/messages").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    app.shutdown().await;
}

#[tokio::test]
async fn answering_a_question_collapses_the_card_and_rejects_bad_input() {
    use claw::db::{CentralDb, messaging_groups, questions, sessions, web_messages};
    use claw::protocol::content::Routing;
    use claw::protocol::ids::MessageOutId;

    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config(tmp.path().to_path_buf());
    let app = claw::app::build(&config).await.expect("build app");
    let client = Client::login(app.http.clone()).await;

    let chat = body_json(
        client
            .post_json("/api/chats", &serde_json::json!({"name": "Main"}))
            .await,
    )
    .await;
    let chat_id = chat["platform_id"].as_str().expect("id").to_owned();

    // A posted message creates the session the answer will be routed back to;
    // wait for the echo reply so the session row exists before seeding.
    client
        .post_json(
            &format!("/api/chats/{chat_id}/messages"),
            &serde_json::json!({"text": "hi"}),
        )
        .await;
    let messages_path = format!("/api/chats/{chat_id}/messages");
    for _ in 0..100 {
        let count = body_json(client.get(&messages_path).await)
            .await
            .as_array()
            .expect("array")
            .len();
        if count >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Seed an open question + card directly, as the delivery path would.
    let options = vec!["ship".to_owned(), "wait".to_owned()];
    {
        let central = CentralDb::open(&config.central_db_path()).expect("central");
        central
            .with(|conn| {
                let chat = messaging_groups::get_by_platform(conn, "web", &chat_id)?.expect("chat");
                let session = sessions::list_active(conn)?.remove(0);
                let routing = Routing {
                    channel_type: Some("web".to_owned()),
                    platform_id: Some(chat_id.clone()),
                    thread_id: None,
                };
                questions::insert(
                    conn,
                    "q-test",
                    &session.id,
                    &MessageOutId::new("out-x"),
                    &routing,
                    "Deploy now?",
                    &options,
                )?;
                web_messages::append_question(
                    conn,
                    &chat.id,
                    "andy",
                    "Deploy now?",
                    "q-test",
                    &options,
                )
                .map(|_| ())
            })
            .expect("seed");
    }

    // A choice outside the offered options is rejected without consuming it.
    let bad = client
        .post_json(
            "/api/questions/q-test/answer",
            &serde_json::json!({"option": "nope"}),
        )
        .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    // The valid answer collapses the card.
    let ok = client
        .post_json(
            "/api/questions/q-test/answer",
            &serde_json::json!({"option": "ship"}),
        )
        .await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(body_json(ok).await["answer"], "ship");

    // Answering again finds no open question.
    let again = client
        .post_json(
            "/api/questions/q-test/answer",
            &serde_json::json!({"option": "ship"}),
        )
        .await;
    assert_eq!(again.status(), StatusCode::NOT_FOUND);

    app.shutdown().await;
}
