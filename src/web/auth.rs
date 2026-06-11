use std::collections::HashSet;
use std::sync::Mutex;

use axum::Router;
use axum::extract::{Form, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;

use super::WebState;

pub const SESSION_COOKIE: &str = "claw_session";

pub struct AuthState {
    token: String,
    active_sessions: Mutex<HashSet<String>>,
}

impl AuthState {
    #[must_use]
    pub fn new(token: String) -> Self {
        Self {
            token,
            active_sessions: Mutex::new(HashSet::new()),
        }
    }

    /// Generates and prints a token when none is configured — first-run flow.
    #[must_use]
    pub fn from_configured_token(token: Option<String>) -> Self {
        let token = token.unwrap_or_else(|| {
            let generated = generate_secret();
            tracing::warn!(token = %generated, "CLAW_AUTH_TOKEN not set — generated a login token for this run");
            generated
        });
        Self::new(token)
    }

    fn begin_session(&self) -> String {
        let session = generate_secret();
        self.active_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session.clone());
        session
    }

    fn is_session_valid(&self, session: &str) -> bool {
        self.active_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session)
    }
}

fn generate_secret() -> String {
    format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new()).to_lowercase()
}

pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/login", get(super::pages::login_page))
        .route("/login", post(login_submit))
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login_submit(
    State(state): State<WebState>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    if form.token != state.auth.token {
        return Redirect::to("/login?error=1").into_response();
    }
    let session = state.auth.begin_session();
    let cookie = Cookie::build((SESSION_COOKIE, session))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .build();
    (jar.add(cookie), Redirect::to("/")).into_response()
}

pub async fn require_auth(
    State(state): State<WebState>,
    jar: CookieJar,
    request: Request,
    next: Next,
) -> Response {
    let authenticated = jar
        .get(SESSION_COOKIE)
        .is_some_and(|cookie| state.auth.is_session_valid(cookie.value()));
    if authenticated {
        return next.run(request).await;
    }
    if wants_html(&request) {
        Redirect::to("/login").into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Browser page loads get a redirect; API and SSE calls get a bare 401.
fn wants_html(request: &Request) -> bool {
    let path = request.uri().path();
    !(path.starts_with("/api") || path.starts_with("/events"))
}
