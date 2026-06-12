use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::json;

use super::*;
use crate::protocol::content::OutboundContent;

#[derive(Default)]
struct MockSim {
    messages: Mutex<Vec<serde_json::Value>>,
    sent: Mutex<Vec<(Option<String>, serde_json::Value)>>,
    polls: Mutex<Vec<i64>>,
    fail_sends: AtomicBool,
}

async fn list_messages(
    State(state): State<Arc<MockSim>>,
    Query(query): Query<HashMap<String, String>>,
) -> axum::Json<Vec<serde_json::Value>> {
    let after: i64 = query
        .get("after_seq")
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);
    state.polls.lock().expect("lock").push(after);
    let messages = state
        .messages
        .lock()
        .expect("lock")
        .iter()
        .filter(|message| message["Seq"].as_i64().unwrap_or(0) > after)
        .cloned()
        .collect();
    axum::Json(messages)
}

async fn send_message(
    State(state): State<Arc<MockSim>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    if state.fail_sends.load(Ordering::SeqCst) {
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let auth = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.sent.lock().expect("lock").push((auth, body));
    axum::Json(json!({ "ok": true })).into_response()
}

async fn spawn_mock() -> (Arc<MockSim>, String) {
    let state = Arc::new(MockSim::default());
    let app = Router::new()
        .route("/api/messages", get(list_messages).post(send_message))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (state, format!("http://{addr}"))
}

fn channel(central: &Arc<CentralDb>, base_url: &str) -> SmsChannel {
    SmsChannel::new(
        central.clone(),
        &SmsConnectorConfig {
            base_url: base_url.to_owned(),
            token: "sms_secret".to_owned(),
            webhook_secret: None,
        },
    )
    .expect("channel")
    .with_poll_interval(Duration::from_millis(20))
}

fn sms_json(seq: i64, phone: &str, content: &str) -> serde_json::Value {
    json!({
        "Index": seq.to_string(),
        "Seq": seq,
        "Smstat": "0",
        "Phone": phone,
        "Content": content,
        "Date": "2026-06-12 18:00:00",
    })
}

async fn wait_for_cursor(central: &Arc<CentralDb>, expected: i64) {
    for _ in 0..200 {
        let cursor = central
            .with(|conn| channel_cursors::get(conn, CHANNEL_TYPE))
            .expect("cursor query");
        if cursor == Some(expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cursor never reached {expected}");
}

fn spawn_run(
    adapter: &Arc<SmsChannel>,
    cancel: &CancellationToken,
) -> (
    mpsc::Receiver<InboundEvent>,
    tokio::task::JoinHandle<Result<(), ChannelError>>,
) {
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn({
        let adapter = adapter.clone();
        let cancel = cancel.clone();
        async move { adapter.run(tx, cancel).await }
    });
    (rx, task)
}

#[tokio::test]
async fn first_run_skips_history_then_routes_new_messages() {
    let (mock, url) = spawn_mock().await;
    mock.messages.lock().expect("lock").extend([
        sms_json(47, "+33611111111", "old one"),
        sms_json(48, "+33611111111", "old two"),
    ]);
    let central = Arc::new(CentralDb::open_in_memory().expect("central"));
    let adapter = Arc::new(channel(&central, &url));
    let cancel = CancellationToken::new();
    let (mut rx, task) = spawn_run(&adapter, &cancel);

    wait_for_cursor(&central, 48).await;
    assert!(rx.try_recv().is_err(), "history must not be routed");

    mock.messages
        .lock()
        .expect("lock")
        .push(sms_json(49, "+33622222222", "fresh"));
    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timely")
        .expect("event");
    assert_eq!(event.channel_type, "sms");
    assert_eq!(event.platform_id, "+33622222222");
    assert!(event.content.contains("fresh"));
    wait_for_cursor(&central, 49).await;

    cancel.cancel();
    task.await.expect("join").expect("clean stop");
}

#[tokio::test]
async fn restart_resumes_from_the_persisted_cursor() {
    let (mock, url) = spawn_mock().await;
    mock.messages.lock().expect("lock").extend([
        sms_json(47, "+336", "before"),
        sms_json(48, "+336", "before too"),
        sms_json(49, "+336", "after restart"),
    ]);
    let central = Arc::new(CentralDb::open_in_memory().expect("central"));
    central
        .with(|conn| channel_cursors::set(conn, CHANNEL_TYPE, 48))
        .expect("seed cursor");

    let adapter = Arc::new(channel(&central, &url));
    let cancel = CancellationToken::new();
    let (mut rx, task) = spawn_run(&adapter, &cancel);

    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timely")
        .expect("event");
    assert!(
        event.content.contains("after restart"),
        "only seq > 48 must route, got {}",
        event.content
    );
    wait_for_cursor(&central, 49).await;
    assert!(rx.try_recv().is_err(), "47/48 must not replay");
    assert!(
        mock.polls
            .lock()
            .expect("lock")
            .iter()
            .all(|after| *after >= 48),
        "polling must resume from the stored cursor, not replay history"
    );

    cancel.cancel();
    task.await.expect("join").expect("clean stop");
}

fn chat_delivery(text: &str) -> OutboundDelivery {
    OutboundDelivery {
        kind: "chat".to_owned(),
        content: OutboundContent::from_text(text),
        files: Vec::new(),
    }
}

fn address(phone: &str) -> Address {
    Address {
        platform_id: phone.to_owned(),
        thread_id: None,
    }
}

#[tokio::test]
async fn deliver_posts_the_rendered_text_with_bearer_auth() {
    let (mock, url) = spawn_mock().await;
    let central = Arc::new(CentralDb::open_in_memory().expect("central"));
    let adapter = channel(&central, &url);

    let platform_message_id = adapter
        .deliver(&address("+33612345678"), &chat_delivery("hello"))
        .await
        .expect("deliver");
    assert_eq!(
        platform_message_id, None,
        "sim-server returns no message id"
    );

    let sent = mock.sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0.as_deref(), Some("Bearer sms_secret"));
    assert_eq!(
        sent[0].1,
        json!({ "phone": "+33612345678", "content": "hello" })
    );
}

#[tokio::test]
async fn a_500_from_sim_server_is_a_delivery_error() {
    let (mock, url) = spawn_mock().await;
    mock.fail_sends.store(true, Ordering::SeqCst);
    let central = Arc::new(CentralDb::open_in_memory().expect("central"));
    let adapter = channel(&central, &url);

    let error = adapter
        .deliver(&address("+336"), &chat_delivery("doomed"))
        .await
        .expect_err("must fail");
    assert!(matches!(error, ChannelError::Delivery(_)), "{error}");
}

#[tokio::test]
async fn an_empty_render_is_skipped_without_a_request() {
    let (mock, url) = spawn_mock().await;
    let central = Arc::new(CentralDb::open_in_memory().expect("central"));
    let adapter = channel(&central, &url);

    let result = adapter
        .deliver(&address("+336"), &chat_delivery(""))
        .await
        .expect("nothing to send is not an error");
    assert_eq!(result, None);
    assert!(mock.sent.lock().expect("lock").is_empty());
}

#[test]
fn backoff_doubles_and_caps() {
    let base = Duration::from_secs(2);
    let cases: &[(u32, u64)] = &[(0, 2), (1, 4), (2, 8), (3, 16), (4, 30), (10, 30)];
    for (failures, expected_secs) in cases {
        assert_eq!(
            backoff_delay(base, *failures),
            Duration::from_secs(*expected_secs),
            "failures={failures}"
        );
    }
}
