//! `POST /api/mailgun-events`: Mailgun's delivery-event webhook.
//!
//! Every event is stored for correlation with `mails.message_id`, and a
//! permanent failure or complaint raises an alarm. Mailgun signs each POST
//! with HMAC-SHA256(timestamp + token); invalid or replayed (over 15 minutes
//! old) events are refused. Whatever the outcome the status is 200: Mailgun
//! retries for hours on a 5xx, so attacker input must never produce one.

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::route::{ApiResponse, Reply, Route};
use crate::alarm::spawn_alarm_keyed;
use crate::http::rate_limit::{get_client_ip, mailgun_events_limiter};
use crate::http::request::HttpRequest;
use crate::js::{NonFinite, PathKey};
use crate::postgres::repositories::mailgun_events::{insert_mailgun_event, MailgunEvent};
use crate::{env, js, logger};

const REPLAY_WINDOW_MS: f64 = 15.0 * 60.0 * 1000.0;

/// `Buffer.from(text, "hex")`: stops at the first pair that is not hex.
fn lenient_hex(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        match (hex_digit(pair[0]), hex_digit(pair[1])) {
            (Some(hi), Some(lo)) => out.push(hi << 4 | lo),
            _ => break,
        }
    }
    out
}

fn hex_digit(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

fn verify_signature(signature: Option<&Value>, key: &str) -> bool {
    // The body is caller-supplied JSON: anything that is not three non-empty
    // strings is refused before any of it is used.
    let Some(sig) = signature.filter(|value| js::is_truthy(value)) else {
        return false;
    };
    let field = |name: &str| sig.get(name).and_then(Value::as_str).filter(|value| !value.is_empty());
    let (Some(timestamp), Some(token), Some(signature)) = (field("timestamp"), field("token"), field("signature"))
    else {
        return false;
    };
    let age_ms = js::now_ms() as f64 - js::number_from_str(timestamp) * 1000.0;
    if !age_ms.is_finite() || age_ms > REPLAY_WINDOW_MS || age_ms < -REPLAY_WINDOW_MS {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(timestamp.as_bytes());
    mac.update(token.as_bytes());
    let expected = mac.finalize().into_bytes();
    let provided = lenient_hex(signature);
    provided.len() == expected.len() && bool::from(provided.ct_eq(&expected))
}

const KNOWN_MAILGUN_EVENTS: [&str; 11] = [
    "accepted",
    "rejected",
    "delivered",
    "failed",
    "opened",
    "clicked",
    "unsubscribed",
    "complained",
    "stored",
    // The legacy names stay accepted: both schemas keep flowing.
    "permanent_fail",
    "temporary_fail",
];

const MAX_EVENT_ID_LENGTH: usize = 128;
const MAX_RECIPIENT_LENGTH: usize = 320;
const MAX_SEVERITY_LENGTH: usize = 32;
const MAX_REASON_LENGTH: usize = 2048;
const MAX_MESSAGE_ID_LENGTH: usize = 1024;
const MAX_RAW_BYTES: usize = 16 * 1024;

/// True when a present string exceeds its column cap.
fn over_cap(raw: Option<&Value>, max: usize) -> bool {
    raw.and_then(Value::as_str).is_some_and(|text| js::utf16_len(text) > max)
}

/// A temporary failure is greylisting or throttling that Mailgun retries on
/// its own; only a permanent one, or a complaint, is worth an alarm.
fn should_alarm(event: &str, severity: Option<&str>) -> bool {
    match event {
        "complained" | "permanent_fail" => true,
        "failed" => severity == Some("permanent"),
        _ => false,
    }
}

const MAX_TIMESTAMP_SECONDS: f64 = 4_102_444_800.0;

/// Mailgun's Unix-seconds timestamp as ISO, or now when it is missing,
/// non-numeric or out of range: `event-data` is not covered by the signature.
fn safe_occurred_at(raw: Option<&Value>) -> String {
    match raw.filter(|value| value.is_number()).and_then(Value::as_f64) {
        Some(seconds) if seconds > 0.0 && seconds < MAX_TIMESTAMP_SECONDS => {
            js::iso_from_ms((seconds * 1000.0) as i64)
        }
        _ => js::now_iso(),
    }
}

/// `String(value)` for the JSON value at `path` of the request body, as a
/// template string would print it. `non_finite` names the numbers of the
/// body that were too large for a double: `null` in the value, `Infinity`
/// in JavaScript.
fn js_string(value: &Value, path: &mut Vec<PathKey>, non_finite: &[NonFinite]) -> String {
    if let Some(infinity) = js::non_finite_at(non_finite, path) {
        return infinity.to_string();
    }
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(_) => js::json_number_to_string(value).unwrap_or_default(),
        Value::String(text) => text.clone(),
        // An array joins its items with commas; `null` among them prints as nothing.
        Value::Array(items) => {
            let mut printed = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                path.push(PathKey::Index(index));
                let absent = item.is_null() && js::non_finite_at(non_finite, path).is_none();
                printed.push(if absent { String::new() } else { js_string(item, path, non_finite) });
                path.pop();
            }
            printed.join(",")
        }
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// The path of `event-data.delivery-status.code` in the request body.
fn code_path() -> Vec<PathKey> {
    ["event-data", "delivery-status", "code"].iter().map(|key| PathKey::Key(key.to_string())).collect()
}

fn extract_reason(data: &Value, non_finite: &[NonFinite]) -> Option<String> {
    // Unsigned JSON: anything but an object with string fields reads as absent.
    let status = data.get("delivery-status").filter(|value| value.is_object() || value.is_array())?;
    let mut path = code_path();
    let code = status
        .get("code")
        .filter(|value| !value.is_null() || js::non_finite_at(non_finite, &path).is_some())
        .map(|value| js_string(value, &mut path, non_finite));
    let text = |key: &str| status.get(key).and_then(Value::as_str).map(str::to_string);
    let parts: Vec<String> = [code, text("message"), text("description")]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty() && !js::trim(part).is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The unsigned `event-data` object for the JSONB column, or a redaction
/// marker when it is larger than any real event. The size is that of the
/// data as `JSON.stringify` writes it, in bytes.
fn bounded_raw(data: &Value) -> Value {
    if js::json_stringify(data).len() <= MAX_RAW_BYTES {
        data.clone()
    } else {
        json!({ "redacted": format!("event-data exceeded {MAX_RAW_BYTES} bytes") })
    }
}

async fn post_mailgun_events(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    // A volume cap: one captured signature replayed inside its window is
    // validly signed every time, so every request takes a slot.
    mailgun_events_limiter().record_failure(&get_client_ip(req), None);

    let Some(key) = env::non_empty("MAILGUN_WEBHOOK_SIGNING_KEY") else {
        return Ok(ApiResponse::failed("Webhook not configured").into());
    };
    if !verify_signature(req.body.get("signature"), &key) {
        return Ok(ApiResponse::failed("Invalid signature").into());
    }

    const MALFORMED: &str = "Malformed event payload";
    let data = req.body.get("event-data").cloned().unwrap_or(Value::Null);
    let truthy = |key: &str| data.get(key).is_some_and(js::is_truthy);
    if !truthy("id") || !truthy("event") {
        return Ok(ApiResponse::failed(MALFORMED).into());
    }
    let Some(id) = data.get("id").and_then(Value::as_str).filter(|id| js::utf16_len(id) <= MAX_EVENT_ID_LENGTH)
    else {
        return Ok(ApiResponse::failed(MALFORMED).into());
    };
    let Some(event) = data
        .get("event")
        .and_then(Value::as_str)
        .filter(|event| KNOWN_MAILGUN_EVENTS.contains(event))
    else {
        return Ok(ApiResponse::failed(MALFORMED).into());
    };
    if over_cap(data.get("recipient"), MAX_RECIPIENT_LENGTH) || over_cap(data.get("severity"), MAX_SEVERITY_LENGTH) {
        return Ok(ApiResponse::failed(MALFORMED).into());
    }
    // `data.message?.headers?.["message-id"]`: reading through a value that
    // is neither an object nor absent yields nothing.
    let raw_message_id = data
        .get("message")
        .and_then(|message| message.get("headers"))
        .and_then(|headers| headers.get("message-id"));
    if over_cap(raw_message_id, MAX_MESSAGE_ID_LENGTH) {
        return Ok(ApiResponse::failed(MALFORMED).into());
    }
    let reason = extract_reason(&data, &req.non_finite);
    if reason.as_deref().is_some_and(|reason| js::utf16_len(reason) > MAX_REASON_LENGTH) {
        return Ok(ApiResponse::failed(MALFORMED).into());
    }

    let message_id = raw_message_id.and_then(Value::as_str);
    let recipient = data.get("recipient").and_then(Value::as_str);
    let severity = data.get("severity").and_then(Value::as_str);
    let occurred_at = safe_occurred_at(data.get("timestamp"));
    let received_at = js::now_iso();
    let raw = bounded_raw(&data);

    let stored = insert_mailgun_event(&MailgunEvent {
        event_id: id,
        event,
        message_id,
        recipient,
        severity,
        reason: reason.as_deref(),
        occurred_at: &occurred_at,
        received_at: &received_at,
        raw: &raw,
    })
    .await;
    if let Err(error) = stored {
        // Still a 200: a 5xx would trigger unbounded retries.
        logger::error_with("Failed to persist Mailgun event", json!({ "event_id": id }), &error);
    }

    logger::info(
        "Mailgun event",
        json!({ "event": event, "message_id": message_id, "recipient": recipient, "severity": severity }),
    );

    if should_alarm(event, severity) {
        let mut detail = vec![format!("**Event:** {event}")];
        let mut line = |label: &str, value: Option<&str>| {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                detail.push(format!("**{label}:** {value}"));
            }
        };
        line("Recipient", recipient);
        line("Severity", severity);
        line("Reason", reason.as_deref());
        line("Message-Id", message_id);
        // A cooldown per event type, so one type cannot drown out another.
        spawn_alarm_keyed(format!("Mailgun: {event}"), detail.join("\n"), format!("mailgun-{event}"));
    }

    Ok(ApiResponse::success().into())
}

pub fn route() -> Route {
    Route::new("POST", "/mailgun-events", |req| Box::pin(post_mailgun_events(req)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons() {
        let data = json!({ "delivery-status": { "code": 550, "message": "No such user", "description": " " } });
        assert_eq!(extract_reason(&data, &[]).as_deref(), Some("550 · No such user"));
        assert_eq!(extract_reason(&json!({ "delivery-status": "nope" }), &[]), None);
        assert_eq!(extract_reason(&json!({ "delivery-status": { "message": 5 } }), &[]), None);
        assert_eq!(extract_reason(&json!({}), &[]), None);
    }

    #[test]
    fn codes_print_as_javascript_prints_them() {
        let reason = |body: &str| {
            let parsed = js::json_parse_full(body).unwrap();
            extract_reason(&parsed.value["event-data"], &parsed.non_finite)
        };
        let with_code = |code: &str| format!(r#"{{"event-data":{{"delivery-status":{{"code":{code}}}}}}}"#);
        assert_eq!(reason(&with_code("1e21")).as_deref(), Some("1e+21"));
        assert_eq!(reason(&with_code("550.0")).as_deref(), Some("550"));
        assert_eq!(reason(&with_code("[1,[2,null],null,\"x\"]")).as_deref(), Some("1,2,,,x"));
        assert_eq!(reason(&with_code("{\"smtp\":550}")).as_deref(), Some("[object Object]"));
        assert_eq!(reason(&with_code("null")), None);
        // Too large for a double: Infinity, also inside an array.
        assert_eq!(reason(&with_code("1e400")).as_deref(), Some("Infinity"));
        assert_eq!(reason(&with_code("-1e400")).as_deref(), Some("-Infinity"));
        assert_eq!(reason(&with_code("[1e400, 2.50, -0]")).as_deref(), Some("Infinity,2.5,0"));
    }

    #[test]
    fn the_raw_copy_is_measured_as_javascript_writes_it() {
        // 1e21 is "1e+21" to JSON.stringify: one byte more than serde_json's "1e21".
        let pad = "x".repeat(MAX_RAW_BYTES - r#"{"n":1e+21,"pad":""}"#.len());
        let fits = js::json_parse(&format!(r#"{{"n":1e21,"pad":"{pad}"}}"#)).unwrap();
        assert_eq!(js::json_stringify(&fits).len(), MAX_RAW_BYTES);
        assert_eq!(bounded_raw(&fits), fits);
        let over = js::json_parse(&format!(r#"{{"n":1e21,"pad":"{pad}x"}}"#)).unwrap();
        assert_eq!(bounded_raw(&over), json!({ "redacted": "event-data exceeded 16384 bytes" }));
    }

    #[test]
    fn alarms() {
        assert!(should_alarm("complained", None));
        assert!(should_alarm("failed", Some("permanent")));
        assert!(!should_alarm("failed", Some("temporary")));
        assert!(!should_alarm("delivered", None));
    }

    #[test]
    fn hex_like_node() {
        assert_eq!(lenient_hex("0aff"), vec![0x0a, 0xff]);
        assert_eq!(lenient_hex("0azz11"), vec![0x0a]);
        assert_eq!(lenient_hex("abc"), vec![0xab]);
    }
}
