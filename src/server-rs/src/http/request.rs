//! The request every route handler receives, and the readers that build it.

use std::collections::HashMap;
use std::io::Read;
use std::net::IpAddr;

use bytes::Bytes;
use http::HeaderMap;
use http_body_util::BodyExt;
use hyper::body::{Body, Incoming};
use serde_json::{Map, Value};

use super::session::HttpSession;
use super::upload::UploadedFiles;
use crate::js::{self, NonFinite, PathKey};

/// A failure the client caused, answered with `status` instead of a 500.
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> HttpError {
        HttpError { status, message: message.into() }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpError {}

/// One query parameter: a repeated key collects its values.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryValue {
    One(String),
    Many(Vec<String>),
}

impl QueryValue {
    /// JavaScript truthiness: a non-empty string, or any repeated key.
    pub fn truthy(&self) -> bool {
        match self {
            QueryValue::One(value) => !value.is_empty(),
            QueryValue::Many(_) => true,
        }
    }

    /// The value when the key appeared exactly once.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            QueryValue::One(value) => Some(value),
            QueryValue::Many(_) => None,
        }
    }

    /// The first value, whether or not the key was repeated.
    pub fn first(&self) -> Option<&str> {
        match self {
            QueryValue::One(value) => Some(value),
            QueryValue::Many(values) => values.first().map(String::as_str),
        }
    }
}

/// The query string, in arrival order. `?a=1&a=2` → `a: Many(["1", "2"])`.
#[derive(Debug, Clone, Default)]
pub struct Query(Vec<(String, QueryValue)>);

impl Query {
    pub fn parse(search: &str) -> Query {
        let mut query = Query::default();
        let raw = search.strip_prefix('?').unwrap_or(search);
        for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            let (key, value) = (key.into_owned(), value.into_owned());
            match query.0.iter_mut().find(|(existing, _)| *existing == key) {
                None => query.0.push((key, QueryValue::One(value))),
                Some((_, QueryValue::Many(values))) => values.push(value),
                Some((_, slot)) => {
                    let QueryValue::One(first) = std::mem::replace(slot, QueryValue::Many(Vec::new())) else {
                        unreachable!("the Many case is matched above");
                    };
                    *slot = QueryValue::Many(vec![first, value]);
                }
            }
        }
        query
    }

    pub fn get(&self, key: &str) -> Option<&QueryValue> {
        self.0.iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    /// Whether the key is present with a truthy value.
    pub fn truthy(&self, key: &str) -> bool {
        self.get(key).is_some_and(QueryValue::truthy)
    }
}

/// The request body as the pipeline leaves it for a route.
pub enum RawBody {
    /// Not read yet: a multipart or other non-JSON body.
    Unread(Incoming),
    /// Already consumed, or there was none.
    Taken,
}

/// The request every route handler and middleware receives.
pub struct HttpRequest {
    pub method: String,
    /// Full pathname, still percent-encoded.
    pub path: String,
    /// Pathname plus query string, as the request line carried it.
    pub url: String,
    pub headers: HeaderMap,
    pub query: Query,
    /// Decoded `:name` segments of the matched route.
    pub params: HashMap<String, String>,
    /// Parsed JSON, multipart fields, or `{}` when the request carried neither.
    pub body: Value,
    /// Multipart file parts by field name.
    pub files: Option<UploadedFiles>,
    /// Client address; see [`resolve_client_ip`].
    pub ip: String,
    /// Whether the client reached us over HTTPS.
    pub secure: bool,
    pub session: HttpSession,
    /// The body, still unread unless it was JSON.
    pub raw: RawBody,
    /// Whether the request carries a body at all.
    pub has_body: bool,
    /// Numbers of the JSON body that were outside the range of a double:
    /// `null` in `body`, `Infinity` to the Bun server. See [`HttpRequest::body_number`].
    pub non_finite: Vec<NonFinite>,
}

impl HttpRequest {
    /// A header's value when it is valid text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// A route parameter; empty when the route has no such segment.
    pub fn param(&self, name: &str) -> &str {
        self.params.get(name).map(String::as_str).unwrap_or("")
    }

    /// The body when it is a JSON object, which is what every route expects.
    pub fn body_object(&self) -> Option<&Map<String, Value>> {
        self.body.as_object()
    }

    /// `String(value)` of the JSON number at `path` of the body, or `None`
    /// when what is there is not a number. A number too large for a double
    /// prints as `Infinity` or `-Infinity`, as it does in JavaScript.
    ///
    /// ```ignore
    /// let line = req.body_number(&[PathKey::Key("lineno".into())]);
    /// ```
    pub fn body_number(&self, path: &[PathKey]) -> Option<String> {
        if let Some(infinity) = js::non_finite_at(&self.non_finite, path) {
            return Some(infinity.to_string());
        }
        let mut value = &self.body;
        for step in path {
            value = match step {
                PathKey::Key(key) => value.as_object()?.get(key)?,
                PathKey::Index(index) => value.as_array()?.get(*index)?,
            };
        }
        js::json_number_to_string(value)
    }
}

/// Decides whether a peer is a proxy whose forwarding headers are believed.
pub type ProxyTrust = fn(&str) -> bool;

/// Loopback and private addresses, including their IPv4-mapped IPv6 forms.
pub fn trust_loopback_and_private(address: &str) -> bool {
    let Ok(ip) = address.parse::<IpAddr>() else {
        return false;
    };
    let v4 = |ip: std::net::Ipv4Addr| {
        let [a, b, ..] = ip.octets();
        a == 127 || a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
    };
    match ip {
        IpAddr::V4(ip) => v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => v4(mapped),
            None => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00,
        },
    }
}

pub fn trust_no_proxy(_address: &str) -> bool {
    false
}

/// `X-Forwarded-For` entries, nearest hop first. Only spaces are trimmed.
fn forwarded_hops(header: Option<&str>) -> Vec<String> {
    let mut hops: Vec<String> = header
        .unwrap_or("")
        .split(',')
        .map(|entry| entry.trim_matches(' ').to_string())
        .filter(|entry| !entry.is_empty())
        .collect();
    hops.reverse();
    hops
}

/// The client address a request is attributed to, walking `X-Forwarded-For`
/// from the socket outward and stopping at the first hop that is not a
/// trusted proxy. A peer that is not itself trusted is the answer whatever
/// headers it sends.
pub fn resolve_client_ip(peer_address: &str, forwarded_for: Option<&str>, trust: ProxyTrust) -> String {
    let mut hops = vec![peer_address.to_string()];
    hops.extend(forwarded_hops(forwarded_for));
    for hop in &hops[..hops.len() - 1] {
        if !trust(hop) {
            return hop.clone();
        }
    }
    hops.pop().unwrap_or_default()
}

/// HTTPS when a trusted proxy says so in `X-Forwarded-Proto`.
pub fn is_secure_request(peer_address: &str, forwarded_proto: Option<&str>, trust: ProxyTrust) -> bool {
    match forwarded_proto {
        Some(proto) if !proto.is_empty() && trust(peer_address) => {
            crate::js::trim(proto.split(',').next().unwrap_or("")) == "https"
        }
        _ => false,
    }
}

const JSON_BODY_LIMIT: usize = 10 * 1024 * 1024;

fn media_type_of(content_type: Option<&str>) -> String {
    crate::js::trim(content_type.unwrap_or("").split(';').next().unwrap_or("")).to_ascii_lowercase()
}

/// The `charset` parameter, lower-cased; `utf-8` when there is none.
fn charset_of(content_type: &str) -> String {
    let lower = content_type.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(at) = rest.find(';') {
        rest = &rest[at + 1..];
        let candidate = rest.trim_start_matches(|c: char| c.is_whitespace());
        let Some(after) = candidate.strip_prefix("charset") else { continue };
        let after = after.trim_start_matches(|c: char| c.is_whitespace());
        let Some(value) = after.strip_prefix('=') else { continue };
        let value = value.trim_start_matches(|c: char| c.is_whitespace());
        let value = value.strip_prefix('"').unwrap_or(value);
        let end = value
            .find(|c: char| c == '"' || c == ';' || c.is_whitespace())
            .unwrap_or(value.len());
        if end > 0 {
            return value[..end].to_string();
        }
    }
    "utf-8".to_string()
}

/// Whether the request carries a body; a declared length of zero means it
/// does not, and neither GET nor HEAD has one whatever was sent with it.
pub fn has_body(method: &str, headers: &HeaderMap, body: &Incoming) -> bool {
    method != "GET"
        && method != "HEAD"
        && !body.is_end_stream()
        && headers.get("content-length").and_then(|v| v.to_str().ok()) != Some("0")
}

fn declared_length(headers: &HeaderMap) -> f64 {
    match headers.get("content-length").and_then(|v| v.to_str().ok()) {
        Some(value) => crate::js::number_from_str(value),
        None => 0.0,
    }
}

/// How much of a refused body is still read and thrown away, and for how
/// long, so that the refusal reaches a client that is busy sending.
const DISCARD_LIMIT: usize = 64 * 1024 * 1024;
const DISCARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Reads the rest of a body that is already over its limit. Answering while
/// the client is still writing would close the connection under it, and a
/// reset can cost the client the answer it has not read yet.
pub(super) async fn discard_rest(body: &mut Incoming) {
    let discard = async {
        let mut discarded = 0;
        while let Some(Ok(frame)) = body.frame().await {
            discarded += frame.data_ref().map_or(0, |data| data.len());
            if discarded > DISCARD_LIMIT {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(DISCARD_TIMEOUT, discard).await;
}

async fn read_limited(body: Incoming, limit: usize) -> Result<Vec<u8>, HttpError> {
    let mut body = body;
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| HttpError::new(400, "invalid request body"))?;
        if let Ok(data) = frame.into_data() {
            bytes.extend_from_slice(&data);
            if bytes.len() > limit {
                drop(bytes);
                discard_rest(&mut body).await;
                return Err(HttpError::new(413, "request entity too large"));
            }
        }
    }
    Ok(bytes)
}

fn read_within(reader: &mut dyn Read, limit: usize) -> Result<Vec<u8>, HttpError> {
    let mut decoded = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(decoded),
            Ok(n) => {
                decoded.extend_from_slice(&chunk[..n]);
                if decoded.len() > limit {
                    return Err(HttpError::new(413, "request entity too large"));
                }
            }
            Err(_) => return Err(HttpError::new(400, "invalid request body")),
        }
    }
}

/// Undoes a `Content-Encoding` of gzip or deflate, refusing the result once
/// it passes `limit` bytes. The whole input has to be the encoding: several
/// gzip members in a row are one body, anything else after the data is an
/// error, as it is to the Bun server's `DecompressionStream`.
fn decode_body(encoded: Vec<u8>, headers: &HeaderMap, limit: usize) -> Result<Vec<u8>, HttpError> {
    let encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity")
        .to_ascii_lowercase();
    match encoding.as_str() {
        "identity" => Ok(encoded),
        "gzip" => read_within(&mut flate2::read::MultiGzDecoder::new(&encoded[..]), limit),
        "deflate" => {
            let mut decoder = flate2::read::ZlibDecoder::new(&encoded[..]);
            let decoded = read_within(&mut decoder, limit)?;
            if decoder.total_in() != encoded.len() as u64 {
                return Err(HttpError::new(400, "invalid request body"));
            }
            Ok(decoded)
        }
        other => Err(HttpError::new(415, format!("unsupported content encoding \"{other}\""))),
    }
}

/// The body as text, decoded the way `TextDecoder` decodes UTF-8: bytes that
/// are not UTF-8 become U+FFFD, and one byte order mark at the start is dropped.
async fn read_decoded(body: Incoming, headers: &HeaderMap, limit: usize) -> Result<String, HttpError> {
    // The unknown-encoding refusal comes before any byte is read.
    let encoding = headers.get("content-encoding").and_then(|v| v.to_str().ok()).unwrap_or("identity");
    if !matches!(encoding.to_ascii_lowercase().as_str(), "identity" | "gzip" | "deflate") {
        return Err(HttpError::new(
            415,
            format!("unsupported content encoding \"{}\"", encoding.to_ascii_lowercase()),
        ));
    }
    let encoded = read_limited(body, limit).await?;
    let decoded = decode_body(encoded, headers, limit)?;
    let text = String::from_utf8_lossy(&decoded);
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// The body of any request as text, whatever its content type: 413 past
/// `limit` bytes, 415 for an unknown content encoding, 400 for a body that
/// cannot be read.
pub async fn read_text_body(req: &mut HttpRequest, limit: usize) -> Result<String, HttpError> {
    if !req.has_body {
        return Ok(String::new());
    }
    if declared_length(&req.headers) > limit as f64 {
        return Err(HttpError::new(413, "request entity too large"));
    }
    match std::mem::replace(&mut req.raw, RawBody::Taken) {
        RawBody::Unread(body) => read_decoded(body, &req.headers, limit).await,
        RawBody::Taken => Ok(String::new()),
    }
}

/// What reading the JSON body left behind.
pub struct JsonBody {
    pub value: Value,
    pub raw: RawBody,
    /// Numbers that were outside the range of a double; `null` in `value`.
    pub non_finite: Vec<NonFinite>,
}

impl JsonBody {
    /// `{}`, the body of a request that carried no JSON.
    pub fn empty(raw: RawBody) -> JsonBody {
        JsonBody { value: Value::Object(Map::new()), raw, non_finite: Vec::new() }
    }
}

/// The body of an `application/json` request, parsed; `{}` for any other
/// request, whose body is left unread. Only an object or array is accepted
/// at the top level: 400 for malformed JSON, 413 over 10 MiB, 415 for a
/// non-UTF charset or an unknown content encoding.
pub async fn read_json_body(headers: &HeaderMap, body: Incoming, has_body: bool) -> Result<JsonBody, HttpError> {
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());
    if media_type_of(content_type) != "application/json" || !has_body {
        return Ok(JsonBody::empty(RawBody::Unread(body)));
    }
    if !charset_of(content_type.unwrap_or("")).starts_with("utf-") {
        return Err(HttpError::new(415, "unsupported charset"));
    }
    if declared_length(headers) > JSON_BODY_LIMIT as f64 {
        return Err(HttpError::new(413, "request entity too large"));
    }

    // The decoder has dropped one byte order mark; a second one goes here,
    // as the Bun server strips one from the decoded text itself.
    let text = read_decoded(body, headers, JSON_BODY_LIMIT).await?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if text.is_empty() {
        return Ok(JsonBody::empty(RawBody::Taken));
    }
    let first = text.trim_start_matches([' ', '\t', '\n', '\r']).chars().next();
    if !matches!(first, Some('{') | Some('[')) {
        return Err(HttpError::new(400, "invalid JSON body"));
    }
    let parsed = js::json_parse_full(text).ok_or_else(|| HttpError::new(400, "invalid JSON body"))?;
    Ok(JsonBody { value: parsed.value, raw: RawBody::Taken, non_finite: parsed.non_finite })
}

/// The path and query string a request asked for, exactly as sent.
pub fn request_target(uri: &http::Uri) -> (String, String) {
    let pathname = if uri.path().is_empty() { "/" } else { uri.path() };
    let search = uri.query().map(|query| format!("?{query}")).unwrap_or_default();
    (pathname.to_string(), search)
}

/// Collects a streamed body into one buffer.
pub async fn collect_bytes(body: Incoming) -> Result<Bytes, hyper::Error> {
    Ok(body.collect().await?.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ip_resolution() {
        let trust: ProxyTrust = trust_loopback_and_private;
        assert_eq!(resolve_client_ip("127.0.0.1", Some("203.0.113.9"), trust), "203.0.113.9");
        assert_eq!(resolve_client_ip("127.0.0.1", Some("1.1.1.1, 203.0.113.9, 10.0.0.2"), trust), "203.0.113.9");
        assert_eq!(resolve_client_ip("8.8.8.8", Some("203.0.113.9"), trust), "8.8.8.8");
        assert_eq!(resolve_client_ip("127.0.0.1", Some("10.0.0.1"), trust), "10.0.0.1");
        assert_eq!(resolve_client_ip("127.0.0.1", None, trust), "127.0.0.1");
        assert_eq!(resolve_client_ip("127.0.0.1", Some("203.0.113.9"), trust_no_proxy), "127.0.0.1");
        assert!(trust_loopback_and_private("::ffff:10.1.2.3"));
        assert!(trust_loopback_and_private("fd00::1"));
        assert!(!trust_loopback_and_private("172.32.0.1"));
        assert!(!trust_loopback_and_private("not-an-ip"));
    }

    #[test]
    fn secure_requests() {
        let trust: ProxyTrust = trust_loopback_and_private;
        assert!(is_secure_request("127.0.0.1", Some("https"), trust));
        assert!(is_secure_request("127.0.0.1", Some("https, http"), trust));
        assert!(!is_secure_request("127.0.0.1", Some("http"), trust));
        assert!(!is_secure_request("8.8.8.8", Some("https"), trust));
        assert!(!is_secure_request("127.0.0.1", None, trust));
    }

    #[test]
    fn query_values() {
        let query = Query::parse("?a=1&b=&a=2&c=x+y%21");
        assert_eq!(query.get("a"), Some(&QueryValue::Many(vec!["1".into(), "2".into()])));
        assert_eq!(query.get("b"), Some(&QueryValue::One(String::new())));
        assert_eq!(query.get("c").and_then(QueryValue::as_str), Some("x y!"));
        assert!(query.truthy("a") && !query.truthy("b") && !query.truthy("missing"));
    }

    fn gzip(text: &str) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    fn deflate(text: &str) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    fn decoded(encoding: &str, body: Vec<u8>) -> Result<Vec<u8>, u16> {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", encoding.parse().unwrap());
        decode_body(body, &headers, 1024).map_err(|error| error.status)
    }

    #[test]
    fn compressed_bodies() {
        assert_eq!(decoded("gzip", gzip("{}")), Ok(b"{}".to_vec()));
        assert_eq!(decoded("GZIP", gzip("{}")), Ok(b"{}".to_vec()));
        assert_eq!(decoded("deflate", deflate("{}")), Ok(b"{}".to_vec()));
        assert_eq!(decoded("identity", b"{}".to_vec()), Ok(b"{}".to_vec()));
        assert_eq!(decoded("br", b"{}".to_vec()), Err(415));
        // Several gzip members in a row are one body.
        assert_eq!(decoded("gzip", [gzip("{\"a\":"), gzip("1}")].concat()), Ok(b"{\"a\":1}".to_vec()));
        assert_eq!(decoded("gzip", [gzip(""), gzip("{}"), gzip("")].concat()), Ok(b"{}".to_vec()));
        // Anything else after the data is an error, as is data cut short.
        assert_eq!(decoded("gzip", [gzip("{}"), b"trailing".to_vec()].concat()), Err(400));
        assert_eq!(decoded("deflate", [deflate("{}"), b"zz".to_vec()].concat()), Err(400));
        let whole = gzip("{\"a\":1}");
        assert_eq!(decoded("gzip", whole[..whole.len() - 5].to_vec()), Err(400));
        assert_eq!(decoded("gzip", b"{}".to_vec()), Err(400));
        assert_eq!(decoded("deflate", gzip("{}")), Err(400));
        // The limit is on what comes out.
        assert_eq!(decoded("gzip", gzip(&" ".repeat(1024))).map(|body| body.len()), Ok(1024));
        assert_eq!(decoded("gzip", gzip(&" ".repeat(1025))), Err(413));
        assert_eq!(decoded("deflate", deflate(&" ".repeat(100_000))), Err(413));
    }

    #[test]
    fn content_types() {
        assert_eq!(media_type_of(Some("Application/JSON; charset=utf-8")), "application/json");
        assert_eq!(charset_of("application/json; charset=\"UTF-8\""), "utf-8");
        assert_eq!(charset_of("application/json;charset = latin1"), "latin1");
        assert_eq!(charset_of("application/json"), "utf-8");
    }
}
