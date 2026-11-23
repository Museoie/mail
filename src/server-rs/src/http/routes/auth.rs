//! `/api/auth/*`: single sign-on. Public, and answered only while single
//! sign-on is on, so with it off they are unknown routes.

use std::sync::OnceLock;

use serde_json::json;

use super::route::{dispatch, Reply, Route};
use crate::http::issue_session::issue_authenticated_session;
use crate::http::request::{read_text_body, HttpRequest};
use crate::http::respond::{json, redirect, Response};
use crate::sso::events::{receive_sso_event, EVENT_BODY_LIMIT};
use crate::sso::sign_in::{finish_sign_in, sign_in_failure, start_sign_in, CompletedSignIn, SignInRedirect};
use crate::sso::{get_sso, require_sso};

/// The provider events route reads its own body: a request there is not
/// parsed as JSON, whatever content type it claims.
pub fn is_raw_body_path(pathname: &str) -> bool {
    let lower = pathname.to_ascii_lowercase();
    lower == "/api/auth/events" || lower == "/api/auth/events/"
}

fn answer(target: SignInRedirect) -> Reply {
    redirect(&target.location, &target.cookies).into()
}

/// Starts a single sign-on: sends the browser to the provider.
async fn get_login(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let sso = require_sso()?;
    let return_to = req.query.get("returnTo").and_then(|value| value.first()).map(str::to_string);
    Ok(answer(start_sign_in(&sso, return_to.as_deref()).await))
}

/// Starts the app's own session for a completed sign-in. The row is written
/// before the redirect: the cookie means nothing until it exists.
async fn start_session(req: &mut HttpRequest, done: &CompletedSignIn) -> anyhow::Result<()> {
    issue_authenticated_session(req, done.user.clone()).await?;
    req.session.auth = Some(done.auth.clone());
    req.session.save().await?;
    Ok(())
}

/// Where the provider sends the browser back. Any failure lands on the
/// sign-in page with a code saying why.
async fn get_callback(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let sso = require_sso()?;
    let cookie_header = req.header("cookie").map(str::to_string);
    let done = match finish_sign_in(&sso, &req.url, cookie_header.as_deref()).await {
        Ok(done) => done,
        Err(refused) => return Ok(answer(refused)),
    };
    Ok(match start_session(req, &done).await {
        Ok(()) => answer(done.redirect()),
        Err(error) => answer(sign_in_failure(&error)),
    })
}

/// Receives the provider's events. The body's signature is what
/// authenticates the call, so the route is public and reads the body as text
/// whatever its content type.
async fn post_events(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let sso = require_sso()?;
    let body = match read_text_body(req, EVENT_BODY_LIMIT).await {
        Ok(body) => body,
        Err(error) => {
            return Ok(json(&json!({ "status": "failed", "message": error.message }), error.status).into());
        }
    };
    let answered = receive_sso_event(&sso, &body).await?;
    Ok(json(&answered.body, answered.status).into())
}

fn routes() -> &'static [Route] {
    static ROUTES: OnceLock<Vec<Route>> = OnceLock::new();
    ROUTES.get_or_init(|| {
        vec![
            Route::new("GET", "/login", |req| Box::pin(get_login(req))),
            Route::new("GET", "/callback", |req| Box::pin(get_callback(req))),
            Route::new("POST", "/events", |req| Box::pin(post_events(req))),
        ]
    })
}

/// `/api/auth/*`, with `path` relative to that mount.
pub async fn handle_auth(req: &mut HttpRequest, path: &str) -> anyhow::Result<Option<Response>> {
    if get_sso().is_none() {
        return Ok(None);
    }
    dispatch(routes(), req, path).await
}
