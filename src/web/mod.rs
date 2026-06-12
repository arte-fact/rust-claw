pub mod admin;
pub mod api;
pub mod auth;
pub mod files;
pub mod pages;
pub mod render;
pub mod sse;
pub mod tasks;

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::get;

use crate::channels::web::WebChannel;
use crate::commands::Registry;
use crate::db::CentralDb;
use crate::session::SessionStore;
use auth::AuthState;
use sse::Hub;

#[derive(Clone)]
pub struct WebState {
    pub auth: Arc<AuthState>,
    pub central: Arc<CentralDb>,
    pub web_channel: Arc<WebChannel>,
    pub hub: Hub,
    /// Used to execute a command once the operator approves it (M7.2).
    pub commands: Arc<Registry>,
    /// Per-session DBs, for the Tasks page (M7.3b).
    pub store: Arc<SessionStore>,
    pub timezone: String,
    /// Base of agent workspace folders, for the per-chat file browser (M11.2).
    pub groups_dir: std::path::PathBuf,
}

pub fn build_app(state: WebState) -> Router {
    let protected = Router::new()
        .route("/", get(pages::home))
        .route("/chats", axum::routing::post(pages::create_chat_form))
        .route("/chats/{platform_id}", get(pages::chat_page))
        .route("/chats/{platform_id}/files", get(files::page))
        .route(
            "/api/chats/{platform_id}/files/list",
            get(files::list_entries),
        )
        .route("/api/chats/{platform_id}/files/read", get(files::read_file))
        .route("/events", get(sse::events))
        .route("/api/chats", get(api::list_chats).post(api::create_chat))
        .route(
            "/api/chats/{platform_id}/messages",
            get(api::list_messages).post(api::post_message),
        )
        .route(
            "/api/chats/{platform_id}/archive",
            axum::routing::post(api::archive_chat),
        )
        .route(
            "/api/questions/{question_id}/answer",
            axum::routing::post(api::answer_question),
        )
        .route(
            "/api/approvals/{approval_id}/answer",
            axum::routing::post(api::answer_approval),
        )
        .route("/admin", get(admin::index))
        .route("/admin/tasks", get(tasks::page))
        .route("/admin/tasks/action", axum::routing::post(tasks::action))
        .route("/admin/run", axum::routing::post(admin::run))
        .route("/admin/{resource}", get(admin::resource_page))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/{*path}", get(pages::asset))
        .merge(auth::routes())
        .merge(protected)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    fn app() -> Router {
        let central = Arc::new(CentralDb::open_in_memory().expect("central"));
        let hub = Hub::new();
        build_app(WebState {
            auth: Arc::new(AuthState::new("secret-token".to_owned())),
            web_channel: Arc::new(WebChannel::new(central.clone(), hub.clone())),
            commands: Arc::new(Registry::new(central.clone())),
            store: Arc::new(SessionStore::new(std::path::PathBuf::from(
                "/tmp/claw-test",
            ))),
            timezone: "UTC".to_owned(),
            groups_dir: std::path::PathBuf::from("/tmp/claw-test/groups"),
            central,
            hub,
        })
    }

    async fn send(app: Router, request: Request<Body>) -> axum::response::Response {
        app.oneshot(request).await.expect("request must complete")
    }

    #[tokio::test]
    async fn healthz_needs_no_auth() {
        let response = send(
            app(),
            Request::get("/healthz").body(Body::empty()).expect("req"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unauthenticated_page_requests_redirect_to_login() {
        let response = send(app(), Request::get("/").body(Body::empty()).expect("req")).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|loc| loc.to_str().ok()),
            Some("/login")
        );
    }

    #[tokio::test]
    async fn unauthenticated_api_requests_get_401() {
        let response = send(
            app(),
            Request::get("/api/chats").body(Body::empty()).expect("req"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_redirects_back_to_login_with_error() {
        let response = send(
            app(),
            Request::post("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("token=wrong"))
                .expect("req"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|loc| loc.to_str().ok()),
            Some("/login?error=1")
        );
    }

    #[tokio::test]
    async fn assets_are_served_without_auth() {
        let response = send(
            app(),
            Request::get("/assets/claw.css")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/css")
        );
    }

    #[tokio::test]
    async fn login_sets_a_cookie_that_unlocks_protected_routes() {
        let app = app();
        let login = send(
            app.clone(),
            Request::post("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("token=secret-token"))
                .expect("req"),
        )
        .await;
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("set-cookie header")
            .to_owned();
        assert!(cookie.contains("HttpOnly"), "cookie must be HttpOnly");

        let session_pair = cookie.split(';').next().expect("cookie pair");
        let response = send(
            app,
            Request::get("/")
                .header(header::COOKIE, session_pair)
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
