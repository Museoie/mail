//! The terminal error handler: reports a fault raised outside any route and
//! answers it without ever putting the error's text in the body.

use serde_json::json;

use super::request::HttpError;
use super::respond::{json, text, Response};
use crate::alarm::spawn_alarm;
use crate::logger;

fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

const DETAIL: &str = "Internal server error";

/// The reason phrase for a client error status this server raises.
fn status_text(status: u16) -> Option<&'static str> {
    http::StatusCode::from_u16(status).ok().and_then(|code| match status {
        // Node's table says "Payload Too Large" where the `http` crate says "Content Too Large".
        413 => Some("Payload Too Large"),
        422 => Some("Unprocessable Entity"),
        _ => code.canonical_reason(),
    })
}

/// The 4xx a body parser or route matcher asked for. Anything else is ours.
fn client_error_status(error: &anyhow::Error) -> Option<u16> {
    error
        .downcast_ref::<HttpError>()
        .map(|error| error.status)
        .filter(|status| (400..500).contains(status))
}

/// What the error handler needs to know about the request that failed.
pub struct FailedRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub session_id: Option<&'a str>,
}

/// Logs and alarms on a fault raised outside any route: a session store read
/// or write, a static file read. A client error is not reported.
pub fn report_middleware_error(error: &anyhow::Error, req: &FailedRequest<'_>) {
    if client_error_status(error).is_some() {
        return;
    }
    logger::error_with(
        "Unhandled middleware error",
        json!({ "method": req.method, "path": req.path, "session_id": req.session_id }),
        error,
    );
    spawn_alarm(
        "Unhandled Middleware Error",
        format!("**Request:** {} {}\n**Error:** {error}", req.method, req.path),
    );
}

/// Reports the fault and answers it. A client error keeps its 4xx; anything
/// else is a 500.
pub fn error_handler(error: &anyhow::Error, req: &FailedRequest<'_>) -> Response {
    report_middleware_error(error, req);
    let client_status = client_error_status(error);
    let status = client_status.unwrap_or(500);
    let detail = client_status.and_then(status_text).unwrap_or(DETAIL);
    if is_api_path(req.path) {
        json(&json!({ "status": "error", "message": detail }), status)
    } else {
        text(detail, status)
    }
}
