//! `?limit=` and `?cursor=` of the paged lists (http/routes/mails/list-page.ts).

use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};

use crate::http::request::{Query, QueryValue};
use crate::js;
use crate::postgres::repositories::mails::http::ListCursor;
use crate::util::is_uuid;

/// Largest page one request may ask for: the size the unpaged list returns.
pub const MAX_PAGE_SIZE: usize = 10_000;

/// `timestamptz::text`, e.g. `2026-11-15 02:55:04.123456+00`.
static PG_TIMESTAMPTZ_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,6})?[+-][0-9]{2}(:[0-9]{2}){0,2}$")
        .expect("a valid pattern")
});

/// The cursor a client passes back as `?cursor=` for the rows after a page;
/// `null` on the last page.
pub fn encode_list_cursor(cursor: Option<&ListCursor>) -> Value {
    match cursor {
        Some(cursor) => {
            let text = json!({ "date": cursor.date, "id": cursor.id }).to_string();
            Value::String(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text))
        }
        None => Value::Null,
    }
}

/// Decodes base64 or base64url leniently, as `Buffer.from(value, "base64url")`
/// does: characters outside both alphabets are skipped, and `=` ends the data.
fn decode_base64_lenient(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in value.bytes() {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        buffer = (buffer << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    out
}

fn decode_list_cursor(value: &str) -> Option<ListCursor> {
    let text = String::from_utf8_lossy(&decode_base64_lenient(value)).into_owned();
    let parsed = js::json_parse(&text)?;
    // Any object passes this point, a list included; its fields decide.
    let fields = match &parsed {
        Value::Object(fields) => fields,
        Value::Array(_) => return None,
        _ => return None,
    };
    let date = fields.get("date")?.as_str()?;
    if !PG_TIMESTAMPTZ_TEXT.is_match(date) {
        return None;
    }
    let id = fields.get("id")?.as_str()?;
    if !is_uuid(id) {
        return None;
    }
    Some(ListCursor { date: date.to_string(), id: id.to_string() })
}

/// What `?limit=` and `?cursor=` ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListPageParams {
    /// The whole list, as before paging existed.
    Unpaged,
    Paged { limit: usize, after: Option<ListCursor> },
    /// Refused with this message.
    Invalid(String),
}

/// Reads `?limit=` and `?cursor=`. Without `limit` the route answers with the
/// whole list; with it, one page and the cursor of the next.
pub fn parse_list_page_params(query: &Query) -> ListPageParams {
    let (limit, cursor) = (query.get("limit"), query.get("cursor"));
    let Some(limit) = limit else {
        return match cursor {
            None => ListPageParams::Unpaged,
            Some(_) => ListPageParams::Invalid("`cursor` needs `limit`.".to_string()),
        };
    };
    let size = match limit {
        QueryValue::One(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            // Digits only: too many of them is simply too large.
            text.trim_start_matches('0').parse::<usize>().unwrap_or(if text.bytes().all(|b| b == b'0') { 0 } else { usize::MAX })
        }
        _ => 0,
    };
    if !(1..=MAX_PAGE_SIZE).contains(&size) {
        return ListPageParams::Invalid(format!("Invalid `limit` parameter: expected 1-{MAX_PAGE_SIZE}."));
    }
    let Some(cursor) = cursor else {
        return ListPageParams::Paged { limit: size, after: None };
    };
    match cursor.as_str().and_then(decode_list_cursor) {
        Some(after) => ListPageParams::Paged { limit: size, after: Some(after) },
        None => ListPageParams::Invalid("Invalid `cursor` parameter.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATE: &str = "2026-11-15 02:55:04.123456+00";
    const ID: &str = "22222222-2222-2222-2222-222222222222";

    fn cursor() -> ListCursor {
        ListCursor { date: DATE.into(), id: ID.into() }
    }

    fn encode(value: Value) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    }

    fn params(search: &str) -> ListPageParams {
        parse_list_page_params(&Query::parse(search))
    }

    fn encoded() -> String {
        encode_list_cursor(Some(&cursor())).as_str().unwrap().to_string()
    }

    #[test]
    fn no_limit_answers_the_whole_list() {
        assert_eq!(params(""), ListPageParams::Unpaged);
        assert_eq!(params("?limit=100"), ListPageParams::Paged { limit: 100, after: None });
        assert_eq!(params("?limit=01"), ListPageParams::Paged { limit: 1, after: None });
    }

    #[test]
    fn a_cursor_round_trips() {
        assert_eq!(params(&format!("?limit=100&cursor={}", encoded())), ListPageParams::Paged { limit: 100, after: Some(cursor()) });
        assert_eq!(
            encode_list_cursor(Some(&cursor())),
            json!(encode(json!({ "date": DATE, "id": ID })))
        );
        assert_eq!(encode_list_cursor(None), Value::Null);
    }

    #[test]
    fn the_text_form_of_any_session_time_zone_is_accepted() {
        for date in ["2026-11-15 02:55:04-07", "2026-11-15 02:55:04.5+05:30", "2026-03-08 03:00:00+00", "2026-03-08 03:00:00+05:30:15"] {
            let value = encode_list_cursor(Some(&ListCursor { date: date.into(), id: ID.into() }));
            let search = format!("?limit=1&cursor={}", value.as_str().unwrap());
            assert_eq!(params(&search), ListPageParams::Paged { limit: 1, after: Some(ListCursor { date: date.into(), id: ID.into() }) });
        }
    }

    #[test]
    fn limits_outside_one_to_the_maximum_are_refused() {
        assert_eq!(params("?limit=10000"), ListPageParams::Paged { limit: 10_000, after: None });
        for limit in ["0", "10001", "1.5", "-3", "ten", "", "99999999999999999999999", "+1", " 1"] {
            let search = format!("?limit={}", js::encode_uri_component(limit));
            assert_eq!(params(&search), ListPageParams::Invalid("Invalid `limit` parameter: expected 1-10000.".into()), "{limit:?}");
        }
        assert_eq!(params("?limit=1&limit=2"), ListPageParams::Invalid("Invalid `limit` parameter: expected 1-10000.".into()));
    }

    #[test]
    fn a_cursor_needs_a_limit_and_must_be_one_of_ours() {
        assert_eq!(params(&format!("?cursor={}", encoded())), ListPageParams::Invalid("`cursor` needs `limit`.".into()));
        for value in [
            "not-base64-json".to_string(),
            encode(json!([{ "date": DATE, "id": ID }])),
            encode(json!({ "date": DATE, "id": "1; DROP TABLE mails" })),
            encode(json!({ "date": "yesterday", "id": ID })),
            encode(json!({ "date": "2026-11-15T02:55:04Z", "id": ID })),
            encode(json!({ "id": ID })),
            encode(json!({ "date": DATE, "id": 5 })),
        ] {
            assert_eq!(params(&format!("?limit=100&cursor={value}")), ListPageParams::Invalid("Invalid `cursor` parameter.".into()));
        }
        assert_eq!(params("?limit=100&cursor=a&cursor=b"), ListPageParams::Invalid("Invalid `cursor` parameter.".into()));
    }

    #[test]
    fn base64_is_read_in_either_alphabet() {
        assert_eq!(decode_base64_lenient("aGk"), b"hi");
        assert_eq!(decode_base64_lenient("aGk="), b"hi");
        assert_eq!(decode_base64_lenient("-_8"), vec![0xfb, 0xff]);
        assert_eq!(decode_base64_lenient("+/8"), vec![0xfb, 0xff]);
        assert_eq!(decode_base64_lenient("a G\nk"), b"hi");
    }
}
