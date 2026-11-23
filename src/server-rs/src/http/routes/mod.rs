//! The `/api` router.

pub mod auth;
pub mod client_error;
pub mod domains;
pub mod health;
pub mod mailgun_events;
pub mod mails;
pub mod push;
pub mod route;
pub mod users;

use std::sync::OnceLock;

use serde_json::json;

use self::route::{dispatch, mounted_path, Route};
use super::rate_limit::{get_client_ip, mailgun_events_limiter};
use super::request::{HttpError, HttpRequest};
use super::respond::{json, Response};
use crate::alarm::spawn_alarm;
use crate::{js, logger};

fn log_request(req: &HttpRequest, url: &str) {
    // The health check and the client's liveness ping are polled constantly
    // and would flood the request log.
    if url == "/health" || url == "/ping" {
        return;
    }
    let now = chrono::Utc::now();
    logger::info(
        format!("<{}> /api{url}", req.method),
        json!({ "at": format!("{}, 0H", now.format("%-m/%-d/%Y, %-I:%M:%S %p")), "from": get_client_ip(req) }),
    );
}

fn is_ping(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower == "/ping" || lower == "/ping/"
}

fn mailgun_events_routes() -> &'static [Route] {
    static ROUTES: OnceLock<Vec<Route>> = OnceLock::new();
    ROUTES.get_or_init(|| vec![mailgun_events::route()])
}

async fn route(req: &mut HttpRequest, path: &str) -> anyhow::Result<Response> {
    let url = req.url.get("/api".len()..).unwrap_or("").to_string();
    let logged = if url.starts_with('/') { url } else { format!("/{url}") };
    log_request(req, &logged);

    // A cheap liveness probe for the client's offline heartbeat: none of
    // /health's database query and socket probes.
    if (req.method == "GET" || req.method == "HEAD") && is_ping(path) {
        return Ok(json(&json!({ "status": "success" }), 200));
    }

    if let Some(rest) = mounted_path(path, "/health") {
        if let Some(response) = health::handle_health(req, rest).await? {
            return Ok(response);
        }
    }
    if let Some(rest) = mounted_path(path, "/client-error") {
        if let Some(response) = client_error::handle_client_error(req, rest).await? {
            return Ok(response);
        }
    }
    if let Some(rest) = mounted_path(path, "/users") {
        if let Some(response) = users::handle_users(req, rest).await? {
            return Ok(response);
        }
    }
    if let Some(rest) = mounted_path(path, "/mails") {
        if let Some(response) = mails::handle_mails(req, rest).await? {
            return Ok(response);
        }
    }
    if let Some(rest) = mounted_path(path, "/push") {
        if let Some(response) = push::handle_push(req, rest).await? {
            return Ok(response);
        }
    }
    if let Some(rest) = mounted_path(path, "/auth") {
        if let Some(response) = auth::handle_auth(req, rest).await? {
            return Ok(response);
        }
    }
    if let Some(rest) = mounted_path(path, "/domains") {
        if let Some(response) = domains::handle_domains(req, rest).await? {
            return Ok(response);
        }
    }

    // Public: Mailgun signs each event, and the handler verifies it.
    // Rate-limited per address against replayed captured signatures.
    if mounted_path(path, "/mailgun-events").is_some() {
        if let Some(refused) = mailgun_events_limiter().check(req) {
            return Ok(refused);
        }
        if let Some(response) = dispatch(mailgun_events_routes(), req, path).await? {
            return Ok(response);
        }
    }

    // A JSON 404 rather than the single-page fallback, which would hand
    // HTML to a client that parses JSON.
    Ok(json(&json!({ "status": "failed", "message": "Not found" }), 404))
}

/// Answers every `/api/*` request; `path` is relative to `/api`. A client
/// error ([`HttpError`]) propagates to the application's error handler, and
/// any other fault becomes a generic 500.
pub async fn handle_api(req: &mut HttpRequest, path: &str) -> anyhow::Result<Response> {
    match route(req, path).await {
        Ok(response) => Ok(response),
        Err(error) if error.is::<HttpError>() => Err(error),
        Err(error) => {
            eprintln!("Unhandled route error: {error}");
            spawn_alarm(
                "Unhandled Route Error",
                format!("**Message:** {error}\n```\n{}\n```", js::utf16_truncate(&format!("{error:?}"), 1000)),
            );
            Ok(json(&json!({ "status": "error", "message": "Internal server error" }), 500))
        }
    }
}
