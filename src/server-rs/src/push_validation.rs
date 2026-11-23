//! Narrowing an untrusted request body field to a storable push subscription.

use std::net::IpAddr;

use serde_json::Value;

use crate::js;

/// The three fields a push sender needs, and nothing else a caller attached.
#[derive(Debug, Clone, PartialEq)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

const MAX_ENDPOINT_LENGTH: usize = 2048;
const MAX_KEY_LENGTH: usize = 256;

/// Suffixes reserved for names that resolve inside the deployment's own network.
const INTERNAL_HOSTNAME_SUFFIXES: [&str; 4] = [".local", ".localhost", ".internal", ".home.arpa"];

fn bounded_string(value: Option<&Value>, max_length: usize) -> Option<&str> {
    let text = value?.as_str()?;
    let length = js::utf16_len(text);
    (length > 0 && length <= max_length).then_some(text)
}

/// A push endpoint is a URL this server later POSTs to, so an unconstrained
/// one is a request-forgery primitive. Every IP literal and every name that
/// can only resolve on an internal network is refused.
fn find_endpoint_url_problem(endpoint: &str) -> Option<&'static str> {
    let Ok(url) = url::Url::parse(endpoint) else {
        return Some("subscription.endpoint must be a valid URL");
    };
    if url.scheme() != "https" {
        return Some("subscription.endpoint must use https");
    }
    if !url.username().is_empty() || url.password().is_some_and(|password| !password.is_empty()) {
        return Some("subscription.endpoint must not embed credentials");
    }
    let host = url.host_str().unwrap_or("");
    let bracketed = host.starts_with('[') && host.ends_with(']');
    let hostname = if bracketed { &host[1..host.len() - 1] } else { host };
    let hostname = hostname.trim_end_matches('.');
    if hostname.parse::<IpAddr>().is_ok() {
        return Some("subscription.endpoint must address a push service by hostname, not by IP");
    }
    if !hostname.contains('.') {
        return Some("subscription.endpoint must use a fully qualified hostname");
    }
    if INTERNAL_HOSTNAME_SUFFIXES.iter().any(|suffix| hostname.ends_with(suffix)) {
        return Some("subscription.endpoint must not address an internal hostname");
    }
    None
}

/// The subscription to store, or the message that says why it is unusable.
pub fn validate_push_subscription(input: Option<&Value>) -> Result<PushSubscription, String> {
    let Some(input) = input.and_then(Value::as_object) else {
        return Err("subscription must be an object".to_string());
    };
    let Some(endpoint) = bounded_string(input.get("endpoint"), MAX_ENDPOINT_LENGTH) else {
        return Err(format!(
            "subscription.endpoint must be a string of 1 to {MAX_ENDPOINT_LENGTH} characters"
        ));
    };
    if let Some(problem) = find_endpoint_url_problem(endpoint) {
        return Err(problem.to_string());
    }
    let Some(keys) = input.get("keys").and_then(Value::as_object) else {
        return Err("subscription.keys must be an object".to_string());
    };
    let Some(p256dh) = bounded_string(keys.get("p256dh"), MAX_KEY_LENGTH) else {
        return Err(format!("subscription.keys.p256dh must be a string of 1 to {MAX_KEY_LENGTH} characters"));
    };
    let Some(auth) = bounded_string(keys.get("auth"), MAX_KEY_LENGTH) else {
        return Err(format!("subscription.keys.auth must be a string of 1 to {MAX_KEY_LENGTH} characters"));
    };
    Ok(PushSubscription { endpoint: endpoint.to_string(), p256dh: p256dh.to_string(), auth: auth.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn problem(endpoint: &str) -> String {
        validate_push_subscription(Some(&json!({ "endpoint": endpoint, "keys": { "p256dh": "a", "auth": "b" } })))
            .unwrap_err()
    }

    #[test]
    fn endpoints() {
        assert!(problem("http://fcm.example.com/s").contains("https"));
        assert!(problem("https://127.0.0.1:6379/s").contains("not by IP"));
        assert!(problem("https://[::1]/s").contains("not by IP"));
        assert!(problem("https://localhost/s").contains("fully qualified"));
        assert!(problem("https://printer.local/s").contains("internal hostname"));
        assert!(problem("https://user:pw@push.example.com/").contains("credentials"));
        assert!(problem("not a url").contains("valid URL"));
        let ok = validate_push_subscription(Some(&json!({
            "endpoint": "https://push.example.com/abc", "keys": { "p256dh": "a", "auth": "b" }, "extra": 1
        })));
        assert_eq!(ok.unwrap().endpoint, "https://push.example.com/abc");
    }

    #[test]
    fn shapes() {
        assert_eq!(validate_push_subscription(None).unwrap_err(), "subscription must be an object");
        assert_eq!(validate_push_subscription(Some(&json!([]))).unwrap_err(), "subscription must be an object");
        let missing_keys = validate_push_subscription(Some(&json!({ "endpoint": "https://push.example.com/s" })));
        assert_eq!(missing_keys.unwrap_err(), "subscription.keys must be an object");
    }
}
