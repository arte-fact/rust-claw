use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};

use super::WebState;

/// sim-server's webhook, consumed as a wake-up ping only (§10): verify the
/// signature, nudge the poller, discard the payload — the cursor stays the
/// source of truth, so a lost or forged-then-rejected hook changes nothing.
/// Unauthenticated route: the HMAC over the raw body IS the authentication.
pub async fn sms(State(state): State<WebState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let Some(sms) = &state.sms else {
        return StatusCode::NOT_FOUND;
    };
    let Some(secret) = sms.webhook_secret() else {
        return StatusCode::NOT_FOUND;
    };
    let Some(signature) = headers
        .get("x-sms-signature")
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::UNAUTHORIZED;
    };
    if !signature_matches(secret, &body, signature) {
        return StatusCode::UNAUTHORIZED;
    }
    sms.wake();
    StatusCode::NO_CONTENT
}

/// `Mac::verify_slice` compares in constant time; the hex decode only touches
/// attacker-supplied data, so it leaks nothing about the secret.
fn signature_matches(secret: &str, body: &[u8], signature_hex: &str) -> bool {
    use hmac::Mac;
    let Some(signature) = decode_hex(signature_hex.trim()) else {
        return false;
    };
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    (0..raw.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(raw.get(at..at + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use crate::channels::ChannelAdapter;
    use crate::channels::sms::SmsChannel;
    use crate::db::CentralDb;
    use crate::protocol::entities::SmsConnectorConfig;
    use crate::web::{WebState, build_app};

    fn web_state(sms: Option<Arc<SmsChannel>>) -> WebState {
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let hub = crate::web::sse::Hub::new();
        WebState {
            auth: Arc::new(crate::web::auth::AuthState::new("token".to_owned())),
            web_channel: Arc::new(crate::channels::web::WebChannel::new(
                central.clone(),
                hub.clone(),
            )),
            commands: Arc::new(crate::commands::Registry::new(central.clone())),
            store: Arc::new(crate::session::SessionStore::new(std::path::PathBuf::from(
                "/tmp/claw-hook-test",
            ))),
            timezone: "UTC".to_owned(),
            groups_dir: std::path::PathBuf::from("/tmp/claw-hook-test/groups"),
            logs: crate::logs::LogBuffer::new(crate::logs::DEFAULT_CAPACITY),
            activity: crate::activity::ActivityHub::new(),
            queue: Arc::new(crate::runs::queue::RunQueue::new()),
            sms,
            central,
            hub,
        }
    }

    fn sms_channel(base_url: &str, secret: Option<&str>) -> Arc<SmsChannel> {
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        Arc::new(
            SmsChannel::new(
                central,
                &SmsConnectorConfig {
                    base_url: base_url.to_owned(),
                    token: "sms_secret".to_owned(),
                    webhook_secret: secret.map(str::to_owned),
                },
            )
            .expect("channel")
            // Long enough that only the wake-up can cause a second poll.
            .with_poll_interval(Duration::from_secs(60)),
        )
    }

    fn hook_request(body: &'static [u8], signature: Option<&str>) -> Request<Body> {
        let mut request = Request::post("/api/hooks/sms");
        if let Some(signature) = signature {
            request = request.header("x-sms-signature", signature);
        }
        request.body(Body::from(body)).expect("request")
    }

    async fn spawn_counting_sim() -> (Arc<Mutex<u32>>, String) {
        let polls = Arc::new(Mutex::new(0u32));
        let handler_polls = polls.clone();
        let app = Router::new().route(
            "/api/messages",
            axum::routing::get(move || {
                let polls = handler_polls.clone();
                async move {
                    *polls.lock().expect("lock") += 1;
                    axum::Json(serde_json::json!([]))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (polls, format!("http://{addr}"))
    }

    const BODY: &[u8] = br#"{"event":"sms.received","seq":48}"#;

    #[tokio::test]
    async fn hook_without_a_configured_connector_is_404() {
        let app = build_app(web_state(None));
        let response = app
            .oneshot(hook_request(BODY, Some("anything")))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hook_without_a_webhook_secret_is_404() {
        let channel = sms_channel("http://127.0.0.1:9", None);
        let app = build_app(web_state(Some(channel)));
        let response = app
            .oneshot(hook_request(BODY, Some("anything")))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_missing_or_bad_signature_is_401() {
        let channel = sms_channel("http://127.0.0.1:9", Some("hook-secret"));
        let app = build_app(web_state(Some(channel)));

        let missing = app
            .clone()
            .oneshot(hook_request(BODY, None))
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let tampered = app
            .oneshot(hook_request(BODY, Some(&sign("wrong-secret", BODY))))
            .await
            .expect("response");
        assert_eq!(tampered.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_valid_hook_wakes_the_poller_immediately() {
        let (polls, base_url) = spawn_counting_sim().await;
        let channel = sms_channel(&base_url, Some("hook-secret"));
        let cancel = CancellationToken::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let run = tokio::spawn({
            let channel = channel.clone();
            let cancel = cancel.clone();
            async move { channel.run(tx, cancel).await }
        });

        // The first-run cursor init is the only poll until something wakes the loop.
        for _ in 0..200 {
            if *polls.lock().expect("lock") >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(*polls.lock().expect("lock"), 1, "only the init fetch ran");

        let app = build_app(web_state(Some(channel.clone())));
        let response = app
            .oneshot(hook_request(BODY, Some(&sign("hook-secret", BODY))))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        for _ in 0..200 {
            if *polls.lock().expect("lock") >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            *polls.lock().expect("lock") >= 2,
            "the hook must trigger an immediate poll"
        );

        cancel.cancel();
        run.await.expect("join").expect("clean stop");
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        use hmac::Mac;
        let mut mac =
            hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("any key works");
        mac.update(body);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn signature_verification_table() {
        let body = br#"{"event":"sms.received","seq":48}"#;
        let good = sign("hook-secret", body);
        let cases: &[(&str, &[u8], String, bool)] = &[
            ("valid", body, good.clone(), true),
            ("tampered body", b"{}", good.clone(), false),
            ("wrong secret", body, sign("other-secret", body), false),
            ("not hex", body, "zz".repeat(32), false),
            ("odd length", body, good[1..].to_owned(), false),
            ("empty", body, String::new(), false),
        ];
        for (name, payload, signature, expected) in cases {
            assert_eq!(
                signature_matches("hook-secret", payload, signature),
                *expected,
                "{name}"
            );
        }
    }
}
