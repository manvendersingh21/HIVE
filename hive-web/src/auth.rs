//! Password gate.
//!
//! The server is published on a tailnet, so the network is already the primary
//! boundary. This is the second layer: it stops a device that is *on* the
//! tailnet but not yours (a phone someone else borrowed, a shared laptop) from
//! walking straight into a root-capable shell.
//!
//! Tokens live in memory only, so a restart logs everyone out.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;

const COOKIE_NAME: &str = "hive_auth";

#[derive(Clone)]
pub struct Auth {
    password: Arc<String>,
    tokens: Arc<Mutex<HashSet<String>>>,
}

impl Auth {
    pub fn new(password: String) -> Self {
        Self {
            password: Arc::new(password),
            tokens: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn issue(&self) -> String {
        // 256 bits from two v4 UUIDs — plenty for a bearer token that only
        // has to survive until the process restarts.
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        self.tokens.lock().unwrap().insert(token.clone());
        token
    }

    pub fn is_valid(&self, token: &str) -> bool {
        self.tokens.lock().unwrap().contains(token)
    }

    fn check_password(&self, attempt: &str) -> bool {
        constant_time_eq(attempt.as_bytes(), self.password.as_bytes())
    }
}

/// Compare without leaking the match position through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Pull our cookie out of a `Cookie:` header without a cookie-jar dependency.
pub fn token_from_headers(headers: &header::HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == COOKIE_NAME)
        .map(|(_, v)| v.trim().to_string())
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub password: String,
}

pub async fn login(State(auth): State<Auth>, Form(form): Form<LoginForm>) -> Response {
    if !auth.check_password(&form.password) {
        return (StatusCode::UNAUTHORIZED, "bad password").into_response();
    }
    let token = auth.issue();
    // No `Secure` flag hardcoded: Tailscale Serve terminates TLS in front of
    // us and forwards over plain HTTP on loopback, so the browser only ever
    // sees this cookie on an https:// origin regardless.
    let cookie = format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800");
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie.as_str()),
            (header::LOCATION, "/"),
        ],
    )
        .into_response()
}

pub async fn logout(State(auth): State<Auth>, headers: header::HeaderMap) -> Response {
    if let Some(token) = token_from_headers(&headers) {
        auth.tokens.lock().unwrap().remove(&token);
    }
    let cleared = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cleared.as_str()),
            (header::LOCATION, "/login"),
        ],
    )
        .into_response()
}

/// Gate everything except `/login`, `/api/health`, and static assets.
pub async fn require_auth(
    State(auth): State<Auth>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let path = req.uri().path();
    // Worker callbacks carry a bearer token, not a browser cookie — they are
    // authenticated in `workers::ingest` instead of by this middleware.
    let is_open = path == "/login"
        || path == "/api/health"
        || path == "/api/worker/status"
        || path.starts_with("/assets/")
        || path.ends_with(".css")
        || path.ends_with(".js");

    if is_open {
        return Ok(next.run(req).await);
    }

    let authorized = token_from_headers(req.headers())
        .map(|t| auth.is_valid(&t))
        .unwrap_or(false);

    if authorized {
        Ok(next.run(req).await)
    } else if path.starts_with("/api/") || path.starts_with("/ws/") {
        Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response())
    } else {
        Err(Redirect::to("/login").into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_identical_input() {
        assert!(constant_time_eq(b"hunter2", b"hunter2"));
        assert!(!constant_time_eq(b"hunter2", b"hunter3"));
        assert!(!constant_time_eq(b"short", b"longer-value"));
    }

    #[test]
    fn extracts_token_among_other_cookies() {
        let mut h = header::HeaderMap::new();
        h.insert(
            header::COOKIE,
            "theme=dark; hive_auth=abc123; other=1".parse().unwrap(),
        );
        assert_eq!(token_from_headers(&h).as_deref(), Some("abc123"));
    }

    #[test]
    fn no_cookie_yields_no_token() {
        assert_eq!(token_from_headers(&header::HeaderMap::new()), None);
    }

    #[test]
    fn issued_token_validates_and_is_unique() {
        let auth = Auth::new("pw".into());
        let a = auth.issue();
        let b = auth.issue();
        assert_ne!(a, b);
        assert!(auth.is_valid(&a));
        assert!(!auth.is_valid("never-issued"));
    }
}
