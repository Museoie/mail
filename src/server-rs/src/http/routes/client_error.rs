//! `POST /api/client-error`: frontend error reports, forwarded as an alarm.
//! No session is required (the beacon fires after page unload), so it is
//! rate-limited per address.

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::alarm::send_alarm_utf16;
use crate::http::rate_limit::{get_client_ip, RateLimiter};
use crate::http::request::HttpRequest;
use crate::http::respond::{json, Response};
use crate::js::PathKey;

fn client_error_limiter() -> &'static RateLimiter {
    static LIMITER: OnceLock<RateLimiter> = OnceLock::new();
    LIMITER.get_or_init(|| RateLimiter::new(5, "Too many client error reports, try again later", false))
}

/// How much of a stack trace an alarm carries, in UTF-16 code units.
const MAX_STACK_UNITS: usize = 1000;

/// The file, line and column an error was thrown at. The column is only
/// appended once a line is known, or `file:7` would read as line 7.
fn source_location(filename: &str, lineno: Option<&str>, colno: Option<&str>) -> String {
    match (filename.is_empty(), lineno, colno) {
        (true, _, _) => String::new(),
        (false, Some(line), Some(column)) => format!("{filename}:{line}:{column}"),
        (false, Some(line), None) => format!("{filename}:{line}"),
        (false, None, _) => filename.to_string(),
    }
}

/// The alarm's description, in UTF-16 code units: the stack is cut at
/// [`MAX_STACK_UNITS`] the way JavaScript's `slice` cuts, which may leave
/// half of a character outside the basic plane.
fn alarm_detail(url: &str, message: &str, location: &str, stack: &str) -> Vec<u16> {
    let mut lines: Vec<Vec<u16>> = Vec::new();
    if !url.is_empty() {
        lines.push(format!("**URL:** {url}").encode_utf16().collect());
    }
    lines.push(format!("**Message:** {message}").encode_utf16().collect());
    if !location.is_empty() {
        lines.push(format!("**Source:** {location}").encode_utf16().collect());
    }
    if !stack.is_empty() {
        let mut block: Vec<u16> = "```\n".encode_utf16().collect();
        block.extend(stack.encode_utf16().take(MAX_STACK_UNITS));
        block.extend("\n```".encode_utf16());
        lines.push(block);
    }
    lines.join(&[u16::from(b'\n')][..])
}

/// `/api/client-error`, with `path` relative to that mount.
pub async fn handle_client_error(req: &mut HttpRequest, path: &str) -> anyhow::Result<Option<Response>> {
    if req.method != "POST" || path != "/" {
        return Ok(None);
    }
    if let Some(refused) = client_error_limiter().check(req) {
        return Ok(Some(refused));
    }
    // The cap bounds report volume, so every accepted report takes a slot.
    client_error_limiter().record_failure(&get_client_ip(req), None);

    let string = |key: &str| req.body.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    // Any JSON number, printed as JavaScript prints it; one too large for a
    // double is `Infinity` there, and here.
    let number = |key: &str| req.body_number(&[PathKey::Key(key.to_string())]);
    let message = req.body.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
    let (stack, url, filename) = (string("stack"), string("url"), string("filename"));
    let (lineno, colno) = (number("lineno"), number("colno"));
    let location = source_location(&filename, lineno.as_deref(), colno.as_deref());

    eprintln!("Client error reported: {}", json!({ "url": url, "message": message, "location": location }));

    // A bucket of its own, so client-error volume cannot suppress other alarms.
    let detail = alarm_detail(&url, &message, &location, &stack);
    send_alarm_utf16("Client JS Error", &detail, Some("client-error")).await;
    Ok(Some(json(&json!({ "status": "success" }), 200)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(units: &[u16]) -> String {
        String::from_utf16_lossy(units)
    }

    #[test]
    fn locations() {
        assert_eq!(source_location("a.js", Some("3"), Some("9")), "a.js:3:9");
        assert_eq!(source_location("a.js", Some("3"), None), "a.js:3");
        assert_eq!(source_location("a.js", None, Some("9")), "a.js");
        assert_eq!(source_location("", Some("3"), Some("9")), "");
    }

    #[test]
    fn details() {
        assert_eq!(text(&alarm_detail("", "boom", "", "")), "**Message:** boom");
        assert_eq!(
            text(&alarm_detail("https://app/x", "boom", "a.js:3", "at x\nat y")),
            "**URL:** https://app/x\n**Message:** boom\n**Source:** a.js:3\n```\nat x\nat y\n```"
        );
    }

    #[test]
    fn the_stack_is_cut_in_utf16_units() {
        let detail = alarm_detail("", "m", "", &format!("{}😀tail", "s".repeat(999)));
        // 999 letters and the first half of the emoji, between the fences.
        let fenced: Vec<u16> = "```\n".encode_utf16().collect();
        let start = "**Message:** m\n".encode_utf16().count() + fenced.len();
        assert_eq!(detail[start + 999], 0xd83d);
        assert_eq!(detail.len(), start + 1000 + "\n```".len());
    }
}
