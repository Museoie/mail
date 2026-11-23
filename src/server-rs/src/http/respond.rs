//! Building responses: whole bodies with an `ETag`, files, lists, redirects,
//! and the 304 for a client whose copy is current.

use std::path::PathBuf;

use base64::Engine;
use bytes::Bytes;
use serde::Serialize;
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::js;

/// What a response carries.
#[derive(Debug, Clone)]
pub enum Body {
    Empty,
    Bytes(Bytes),
    /// Sent as one body; kept apart so a large list is never copied whole.
    Chunks(Vec<Bytes>),
    /// Streamed from disk rather than held in memory.
    File { path: PathBuf, size: u64 },
}

impl Body {
    pub fn len(&self) -> u64 {
        match self {
            Body::Empty => 0,
            Body::Bytes(bytes) => bytes.len() as u64,
            Body::Chunks(chunks) => chunks.iter().map(|chunk| chunk.len() as u64).sum(),
            Body::File { size, .. } => *size,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A response before it is put on the wire. Header names keep the spelling
/// they were set with; lookups ignore case.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    pub fn new(status: u16, body: Body) -> Response {
        Response { status, headers: Vec::new(), body }
    }

    /// `200` with no body.
    pub fn empty() -> Response {
        Response::new(200, Body::Empty)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Replaces every header of this name.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.remove_header(name);
        self.headers.push((name.to_string(), value.into()));
    }

    /// Adds a header, keeping any of the same name (`Set-Cookie`).
    pub fn append_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.push((name.to_string(), value.into()));
    }

    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.set_header(name, value);
        self
    }
}

fn etag_of(byte_length: u64, sha1: &[u8]) -> String {
    let hash = base64::engine::general_purpose::STANDARD.encode(sha1);
    format!("W/\"{byte_length:x}-{}\"", &hash[..27])
}

/// A weak validator over the exact bytes of a body.
fn body_etag(body: &[u8]) -> String {
    etag_of(body.len() as u64, &Sha1::digest(body))
}

/// A body sent whole, tagged with an `ETag` so a revalidating client can get a 304.
pub fn send(body: impl Into<String>, status: u16, content_type: &str) -> Response {
    let body: String = body.into();
    let etag = body_etag(body.as_bytes());
    Response {
        status,
        headers: vec![("Content-Type".into(), content_type.into()), ("ETag".into(), etag)],
        body: Body::Bytes(Bytes::from(body)),
    }
}

/// A file streamed from disk; its `ETag` comes from its size and modification time.
pub fn send_file(path: PathBuf, size: u64, modified_ms: i64, content_type: &str) -> Response {
    Response {
        status: 200,
        headers: vec![
            ("Content-Type".into(), content_type.into()),
            ("ETag".into(), format!("W/\"{size:x}-{modified_ms:x}\"")),
        ],
        body: Body::File { path, size },
    }
}

pub const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

/// A JSON body with `status`.
pub fn json<T: Serialize + ?Sized>(body: &T, status: u16) -> Response {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "null".to_string());
    send(text, status, JSON_CONTENT_TYPE)
}

/// `{"status":"failed","message":…}` with an HTTP status.
pub fn json_failed(message: &str, status: u16) -> Response {
    json(&serde_json::json!({ "status": "failed", "message": message }), status)
}

const LIST_CHUNK_CHARS: usize = 64 * 1024;

/// Extra fields of a paged list body, and the key the list goes under.
pub struct ListPage {
    pub list_key: String,
    /// Written before the list, in this order.
    pub fields: Vec<(String, Value)>,
}

/// Builds `{"status":"success","body":[…]}` one item at a time, so the whole
/// list and its JSON string never exist at once. With `page`, the body is an
/// object instead: the page's fields, then the list under `page.list_key`.
///
/// ```ignore
/// let mut list = JsonList::new(Some(ListPage { list_key: "headers".into(), fields }));
/// for row in rows { list.push(&to_item(row)); }
/// return Ok(list.finish());
/// ```
pub struct JsonList {
    chunk: String,
    parts: Vec<Bytes>,
    hasher: Sha1,
    byte_length: u64,
    first: bool,
    paged: bool,
}

impl JsonList {
    pub fn new(page: Option<ListPage>) -> JsonList {
        let (open, paged) = match page {
            None => ("[".to_string(), false),
            Some(page) => {
                let mut object = serde_json::Map::new();
                for (key, value) in page.fields {
                    object.insert(key, value);
                }
                object.insert(page.list_key, Value::Array(Vec::new()));
                let text = Value::Object(object).to_string();
                // The object's own JSON with its empty list left open: `{…,"key":[`.
                (text[..text.len() - 2].to_string(), true)
            }
        };
        JsonList {
            chunk: format!("{{\"status\":\"success\",\"body\":{open}"),
            parts: Vec::new(),
            hasher: Sha1::new(),
            byte_length: 0,
            first: true,
            paged,
        }
    }

    fn flush(&mut self) {
        let bytes = Bytes::from(std::mem::take(&mut self.chunk));
        self.hasher.update(&bytes);
        self.byte_length += bytes.len() as u64;
        self.parts.push(bytes);
    }

    pub fn push<T: Serialize + ?Sized>(&mut self, item: &T) {
        if !self.first {
            self.chunk.push(',');
        }
        self.first = false;
        self.chunk.push_str(&serde_json::to_string(item).unwrap_or_else(|_| "null".to_string()));
        if self.chunk.len() >= LIST_CHUNK_CHARS {
            self.flush();
        }
    }

    pub fn finish(mut self) -> Response {
        self.chunk.push_str(if self.paged { "]}}" } else { "]}" });
        self.flush();
        let etag = etag_of(self.byte_length, &self.hasher.finalize());
        Response {
            status: 200,
            headers: vec![("Content-Type".into(), JSON_CONTENT_TYPE.into()), ("ETag".into(), etag)],
            body: Body::Chunks(self.parts),
        }
    }
}

/// A `302` to `location`, setting each of `cookies`. Never cached.
pub fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut response = Response::new(302, Body::Empty);
    response.append_header("Location", location);
    response.append_header("Cache-Control", "no-store");
    for cookie in cookies {
        response.append_header("Set-Cookie", cookie.clone());
    }
    response
}

pub fn text(body: &str, status: u16) -> Response {
    send(body, status, "text/plain; charset=utf-8")
}

/// `no-cache` as one of the request's `Cache-Control` directives.
fn has_no_cache(cache_control: &str) -> bool {
    cache_control.split(',').any(|directive| directive.trim_matches(char::is_whitespace) == "no-cache")
}

/// Whether the client's cached copy, named by `If-None-Match` or
/// `If-Modified-Since`, still matches the response about to be sent.
fn is_fresh(request: &http::HeaderMap, response: &Response) -> bool {
    let header = |name: &str| request.get(name).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty());
    let modified_since = header("if-modified-since");
    let none_match = header("if-none-match");
    if modified_since.is_none() && none_match.is_none() {
        return false;
    }
    if has_no_cache(header("cache-control").unwrap_or("")) {
        return false;
    }

    if let Some(none_match) = none_match.filter(|value| *value != "*") {
        let Some(etag) = response.header("etag").filter(|etag| !etag.is_empty()) else {
            return false;
        };
        let matches = none_match.split(',').map(js::trim).any(|tag| {
            tag == etag || tag == format!("W/{etag}") || format!("W/{tag}") == etag
        });
        if !matches {
            return false;
        }
    }

    if let Some(modified_since) = modified_since {
        let Some(last_modified) = response.header("last-modified").filter(|v| !v.is_empty()) else {
            return false;
        };
        match (js::date_parse(last_modified), js::date_parse(modified_since)) {
            (Some(last), Some(since)) if last <= since => {}
            _ => return false,
        }
    }
    true
}

/// Turns a successful GET/HEAD into a bodiless 304 when the client's copy is current.
pub fn conditional(method: &str, request: &http::HeaderMap, response: Response) -> Response {
    let cacheable = method == "GET" || method == "HEAD";
    let success = (200..300).contains(&response.status);
    if !cacheable || !success || !is_fresh(request, &response) {
        return response;
    }
    let mut not_modified = Response { status: 304, headers: response.headers, body: Body::Empty };
    not_modified.remove_header("content-type");
    not_modified.remove_header("content-length");
    not_modified.remove_header("transfer-encoding");
    not_modified
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etags_match_the_bun_server() {
        let response = json(&serde_json::json!({ "status": "success" }), 200);
        assert_eq!(response.header("etag"), Some("W/\"14-Y53wuE/mmbSikKcT/WualL1N65U\""));
        assert_eq!(text("Not Found", 404).header("ETag"), Some("W/\"9-0gXL1ngzMqISxa6S1zx3F4wtLyg\""));
    }

    #[test]
    fn lists_serialize_like_one_json_document() {
        let mut list = JsonList::new(None);
        list.push(&serde_json::json!({ "id": 1 }));
        list.push(&serde_json::json!({ "id": 2 }));
        let response = list.finish();
        let Body::Chunks(parts) = &response.body else { panic!("a chunked body") };
        let body: Vec<u8> = parts.iter().flat_map(|part| part.to_vec()).collect();
        assert_eq!(body, br#"{"status":"success","body":[{"id":1},{"id":2}]}"#);
        assert_eq!(response.header("etag").unwrap(), body_etag(&body));

        let page = ListPage { list_key: "headers".into(), fields: vec![("next_cursor".into(), Value::Null)] };
        let response = JsonList::new(Some(page)).finish();
        let Body::Chunks(parts) = &response.body else { panic!("a chunked body") };
        assert_eq!(&parts[0][..], br#"{"status":"success","body":{"next_cursor":null,"headers":[]}}"#);
    }

    #[test]
    fn conditional_requests() {
        let mut request = http::HeaderMap::new();
        request.insert("if-none-match", "W/\"14-Y53wuE/mmbSikKcT/WualL1N65U\"".parse().unwrap());
        let response = conditional("GET", &request, json(&serde_json::json!({ "status": "success" }), 200));
        assert_eq!(response.status, 304);
        assert!(response.header("content-type").is_none());
        assert!(response.header("etag").is_some());

        let response = conditional("POST", &request, json(&serde_json::json!({ "status": "success" }), 200));
        assert_eq!(response.status, 200);

        request.insert("cache-control", "max-age=0, no-cache".parse().unwrap());
        let response = conditional("GET", &request, json(&serde_json::json!({ "status": "success" }), 200));
        assert_eq!(response.status, 200);
    }
}
