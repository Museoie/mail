//! Route definitions, matching and dispatch.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::alarm::spawn_alarm;
use crate::http::request::{HttpError, HttpRequest};
use crate::http::respond::{json, Response};
use crate::{js, logger};

/// The JSON body every API route answers with. Fields serialize in this
/// order, and an absent `body` or `message` is left out.
#[derive(Debug, Clone, Serialize)]
pub struct ApiResponse {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ApiResponse {
    pub fn success() -> ApiResponse {
        ApiResponse { status: "success", body: None, message: None }
    }

    pub fn success_with(body: impl Serialize) -> ApiResponse {
        ApiResponse {
            status: "success",
            body: Some(serde_json::to_value(body).unwrap_or(Value::Null)),
            message: None,
        }
    }

    pub fn failed(message: impl Into<String>) -> ApiResponse {
        ApiResponse { status: "failed", body: None, message: Some(message.into()) }
    }

    /// `{"status":"failed"}` with no message.
    pub fn failed_silently() -> ApiResponse {
        ApiResponse { status: "failed", body: None, message: None }
    }
}

/// What a route handler produces.
pub enum Reply {
    /// Serialized as JSON with status 200.
    Api(ApiResponse),
    /// Sent as is.
    Response(Response),
    /// `200` with no body.
    Empty,
}

impl From<ApiResponse> for Reply {
    fn from(response: ApiResponse) -> Reply {
        Reply::Api(response)
    }
}

impl From<Response> for Reply {
    fn from(response: Response) -> Reply {
        Reply::Response(response)
    }
}

pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Reply>> + Send + 'a>>;

/// A route handler. Write it as an `async fn(&mut HttpRequest)` and register
/// it with `|req| Box::pin(handler(req))`.
pub type Handler = for<'a> fn(&'a mut HttpRequest) -> HandlerFuture<'a>;

/// Compiles a route path such as `/body/:id`. Matching ignores case and a
/// trailing slash, and each `:name` captures one non-empty segment.
fn compile_path(route_path: &str) -> (Regex, Vec<String>) {
    let mut keys = Vec::new();
    let mut source = String::new();
    let mut rest = route_path;
    while let Some(colon) = rest.find(':') {
        let after = &rest[colon + 1..];
        let name_len = after
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        source.push_str(&regex::escape(&rest[..colon]));
        if name_len == 0 {
            source.push(':');
            rest = after;
            continue;
        }
        keys.push(after[..name_len].to_string());
        source.push_str("([^/]+?)");
        rest = &after[name_len..];
    }
    source.push_str(&regex::escape(rest));
    let pattern = Regex::new(&format!("(?i)^{source}/?$")).expect("a route path compiles");
    (pattern, keys)
}

fn decode_param(value: &str) -> Result<String, HttpError> {
    js::decode_uri_component(value)
        .ok_or_else(|| HttpError::new(400, format!("Failed to decode param '{value}'")))
}

/// The remainder of `path` below a router mounted at `prefix`, or `None`
/// when `path` is not under it. Matching ignores case.
///
/// ```ignore
/// mounted_path("/users/login", "/users"); // Some("/login")
/// mounted_path("/users", "/users");       // Some("/")
/// ```
pub fn mounted_path<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let head = path.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = &path[prefix.len()..];
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(if rest.is_empty() { "/" } else { rest })
}

pub struct Route {
    pub method: &'static str,
    pub path: &'static str,
    handler: Handler,
    pattern: Regex,
    keys: Vec<String>,
}

impl Route {
    pub fn new(method: &'static str, path: &'static str, handler: Handler) -> Route {
        let (pattern, keys) = compile_path(path);
        Route { method, path, handler, pattern, keys }
    }

    /// The route's decoded params when it answers `method` at `path`
    /// (relative to its router). A GET route also answers HEAD. An escape
    /// that cannot be decoded is a 400.
    pub fn matches(&self, method: &str, path: &str) -> Result<Option<HashMap<String, String>>, HttpError> {
        let answers = method == self.method || (method == "HEAD" && self.method == "GET");
        if !answers {
            return Ok(None);
        }
        let Some(found) = self.pattern.captures(path) else {
            return Ok(None);
        };
        let mut params = HashMap::new();
        for (index, key) in self.keys.iter().enumerate() {
            let raw = found.get(index + 1).map(|m| m.as_str()).unwrap_or("");
            params.insert(key.clone(), decode_param(raw)?);
        }
        Ok(Some(params))
    }

    /// Runs the handler. A fault is logged, alarmed and answered with a
    /// generic 500 whose body never carries the error's text.
    pub async fn handle(&self, req: &mut HttpRequest) -> Response {
        match (self.handler)(req).await {
            Ok(Reply::Response(response)) => response,
            Ok(Reply::Api(result)) => json(&result, 200),
            Ok(Reply::Empty) => Response::empty(),
            Err(error) => {
                logger::error_with(
                    "Route handler error",
                    json!({ "method": self.method, "path": self.path }),
                    &error,
                );
                spawn_alarm(
                    format!("Route Error: {} {}", self.method, self.path),
                    format!("**Error:** {error}"),
                );
                json(&json!({ "status": "error", "message": "Internal server error" }), 500)
            }
        }
    }
}

/// Runs the first of `routes` that answers the request at `path`, relative
/// to the router the routes belong to. `None` when none does.
pub async fn dispatch(routes: &[Route], req: &mut HttpRequest, path: &str) -> anyhow::Result<Option<Response>> {
    for route in routes {
        if let Some(params) = route.matches(&req.method, path)? {
            req.params = params;
            return Ok(Some(route.handle(req).await));
        }
    }
    Ok(None)
}

pub const AUTH_ERROR_MESSAGE: &str = "Request user is not logged in.";

/// A 401 for an unauthenticated request, `None` otherwise.
pub fn auth_required(req: &HttpRequest) -> Option<Response> {
    match req.session.user {
        Some(_) => None,
        None => Some(json(&json!({ "status": "failed", "message": AUTH_ERROR_MESSAGE }), 401)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop(_req: &mut HttpRequest) -> HandlerFuture<'_> {
        Box::pin(async { Ok(Reply::Empty) })
    }

    #[test]
    fn route_matching() {
        let route = Route::new("GET", "/body/:id", noop);
        let params = route.matches("GET", "/body/abc").unwrap().unwrap();
        assert_eq!(params["id"], "abc");
        assert!(route.matches("HEAD", "/BODY/abc/").unwrap().is_some());
        assert!(route.matches("POST", "/body/abc").unwrap().is_none());
        assert!(route.matches("GET", "/body/").unwrap().is_none());
        assert!(route.matches("GET", "/body/a/b").unwrap().is_none());
        assert_eq!(route.matches("GET", "/body/a%20b").unwrap().unwrap()["id"], "a b");
        assert_eq!(route.matches("GET", "/body/%E0%A4%A").unwrap_err().status, 400);

        let route = Route::new("DELETE", "/:id", noop);
        assert!(route.matches("DELETE", "/x").unwrap().is_some());
        assert!(route.matches("DELETE", "/").unwrap().is_none());

        let route = Route::new("POST", "/spam/mark", noop);
        assert!(route.matches("POST", "/spam/mark/").unwrap().is_some());
        assert!(route.matches("POST", "/spamXmark").unwrap().is_none());
    }

    #[test]
    fn mounted_paths() {
        assert_eq!(mounted_path("/users/login", "/users"), Some("/login"));
        assert_eq!(mounted_path("/USERS", "/users"), Some("/"));
        assert_eq!(mounted_path("/usersx", "/users"), None);
        assert_eq!(mounted_path("/u", "/users"), None);
    }

    #[test]
    fn api_response_shape() {
        let body = serde_json::to_string(&ApiResponse::failed("nope")).unwrap();
        assert_eq!(body, r#"{"status":"failed","message":"nope"}"#);
        let body = serde_json::to_string(&ApiResponse::success_with(json!({ "a": 1 }))).unwrap();
        assert_eq!(body, r#"{"status":"success","body":{"a":1}}"#);
    }
}
