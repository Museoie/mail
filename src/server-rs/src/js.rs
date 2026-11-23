//! JavaScript semantics the wire format depends on.
//!
//! The Bun server's limits, timestamps and escapes are defined by JavaScript
//! built-ins: a `string.length` counts UTF-16 code units, `Date.parse` accepts
//! what it accepts, `encodeURIComponent` escapes what it escapes. Anything a
//! client can observe has to come out the same here, so those built-ins are
//! reproduced in one place instead of approximated at each call site.

use chrono::{DateTime, Datelike, SecondsFormat, TimeZone, Timelike, Utc};

/// `string.length`: the number of UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `string.slice(start, end)` for non-negative indexes, counted in UTF-16
/// code units. A cut through the middle of a surrogate pair drops the half
/// that is left, where JavaScript would keep a lone surrogate.
pub fn utf16_slice(s: &str, start: usize, end: usize) -> String {
    if start == 0 && utf16_len(s) <= end {
        return s.to_string();
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let end = end.min(units.len());
    if start >= end {
        return String::new();
    }
    char::decode_utf16(units[start..end].iter().copied())
        .filter_map(Result::ok)
        .collect()
}

/// `string.slice(0, end)`.
pub fn utf16_truncate(s: &str, end: usize) -> String {
    utf16_slice(s, 0, end)
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `string.trim()`: JavaScript's whitespace set, which includes the byte
/// order mark and excludes U+0085.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `Date.now()`.
pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// `new Date(ms).toISOString()`, e.g. `2026-11-21T05:53:08.123Z`.
pub fn iso_from_ms(ms: i64) -> String {
    iso_string(&Utc.timestamp_millis_opt(ms).single().unwrap_or_default())
}

/// `date.toISOString()`: millisecond precision, truncated, always `Z`.
pub fn iso_string(date: &DateTime<Utc>) -> String {
    date.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    iso_string(&Utc::now())
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `date.toUTCString()`, e.g. `Sat, 21 Nov 2026 05:53:08 GMT` — the format of
/// `Expires`, `Last-Modified` and `Date`.
pub fn utc_string(date: &DateTime<Utc>) -> String {
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[date.weekday().num_days_from_monday() as usize],
        date.day(),
        MONTHS[date.month0() as usize],
        date.year(),
        date.hour(),
        date.minute(),
        date.second()
    )
}

/// `Date.parse(input)`: milliseconds since the epoch, or `None` for NaN.
///
/// Bun's `Date.parse` is lenient well beyond the ECMAScript format
/// (`"5"` is 1 May 2001, `1-Jan-2026` is a date). The exact rules live with
/// the IMAP date handling, where they were verified against the Bun runtime
/// on thousands of inputs.
pub fn date_parse(input: &str) -> Option<i64> {
    crate::imap::util::date_parse(input)
}

fn is_uri_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte)
}

/// `encodeURIComponent(value)`.
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_uri_unreserved(byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

/// `decodeURIComponent(value)`, or `None` where JavaScript throws `URIError`:
/// a `%` not followed by two hex digits, or escapes that are not valid UTF-8.
pub fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if !bytes.contains(&b'%') {
        return Some(value.to_string());
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let hi = hex_value(*bytes.get(i + 1)?)?;
        let lo = hex_value(*bytes.get(i + 2)?)?;
        let first = hi << 4 | lo;
        i += 3;
        if first < 0x80 {
            out.push(first);
            continue;
        }
        // A multi-byte sequence must be spelled entirely with escapes.
        let extra = match first {
            0xC2..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF4 => 3,
            _ => return None,
        };
        let mut sequence = vec![first];
        for _ in 0..extra {
            if bytes.get(i) != Some(&b'%') {
                return None;
            }
            let hi = hex_value(*bytes.get(i + 1)?)?;
            let lo = hex_value(*bytes.get(i + 2)?)?;
            sequence.push(hi << 4 | lo);
            i += 3;
        }
        std::str::from_utf8(&sequence).ok()?;
        out.extend_from_slice(&sequence);
    }
    String::from_utf8(out).ok()
}

/// `Number(value)` for a string: whitespace is trimmed, an empty string is 0,
/// and anything that is not a complete numeric literal is NaN.
pub fn number_from_str(value: &str) -> f64 {
    // JavaScript's own whitespace set: U+0085, which Rust counts, is not in it.
    let trimmed = trim(value);
    if trimmed.is_empty() {
        return 0.0;
    }
    let lower = trimmed.to_ascii_lowercase();
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return match u64::from_str_radix(rest, radix) {
                Ok(n) if !rest.is_empty() => n as f64,
                _ => f64::NAN,
            };
        }
    }
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust's parser also accepts "inf", "nan" and "infinity", which JavaScript does not.
    let numeric = trimmed
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'));
    if !numeric || !trimmed.bytes().any(|b| b.is_ascii_digit()) {
        return f64::NAN;
    }
    trimmed.parse().unwrap_or(f64::NAN)
}

/// `String(number)`: ECMAScript's Number::toString. The shortest digits that
/// read back as the same double, written plainly while the decimal point
/// falls within 21 digits to the left or 6 zeros to the right, and with an
/// exponent (`1e+21`, `1e-7`) beyond that.
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value == 0.0 {
        return "0".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    // `{:e}` prints the shortest round-trip digits as `d.ddde<exponent>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let count = digits.len() as i32;
    // Where the decimal point sits, counted in digits from the left.
    let point = exponent + 1;

    let mut out = String::new();
    if value < 0.0 {
        out.push('-');
    }
    if count <= point && point <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat('0').take((point - count) as usize));
    } else if 0 < point && point <= 21 {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    } else if -6 < point && point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat('0').take((-point) as usize));
        out.push_str(&digits);
    } else {
        let power = point - 1;
        out.push_str(&digits[..1]);
        if count > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if power < 0 { '-' } else { '+' });
        out.push_str(&power.abs().to_string());
    }
    out
}

/// `JSON.stringify(text)` for a string given as UTF-16 code units, which may
/// hold a lone surrogate (the half a `slice` left of a character outside the
/// basic plane). JavaScript writes such a half as a `\udXXX` escape, and so
/// does this; a Rust `String` could not carry it at all.
pub fn json_quote_utf16(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len() + 2);
    out.push('"');
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok('"') => out.push_str("\\\""),
            Ok('\\') => out.push_str("\\\\"),
            Ok('\u{8}') => out.push_str("\\b"),
            Ok('\u{c}') => out.push_str("\\f"),
            Ok('\n') => out.push_str("\\n"),
            Ok('\r') => out.push_str("\\r"),
            Ok('\t') => out.push_str("\\t"),
            Ok(c) if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            Ok(c) => out.push(c),
            Err(lone) => out.push_str(&format!("\\u{:04x}", lone.unpaired_surrogate())),
        }
    }
    out.push('"');
    out
}

/// `JSON.stringify(value)`: compact, with numbers written as JavaScript
/// writes them (`1e+21` where `serde_json` writes `1e21`), so the text is as
/// long as the Bun server's wherever a length is measured.
pub fn json_stringify(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_json(value, &mut out);
    out
}

/// JavaScript truthiness of a JSON value: `null`, `false`, `0` and `""` are
/// falsy, everything else — every array and object included — is truthy.
pub fn is_truthy(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `String(value)` for a JSON number, or `None` for any other value.
pub fn json_number_to_string(value: &serde_json::Value) -> Option<String> {
    let number = value.as_number()?;
    Some(match (number.as_i64(), number.as_u64()) {
        (Some(integer), _) => integer.to_string(),
        (None, Some(integer)) => number_to_string(integer as f64),
        (None, None) => number_to_string(number.as_f64().unwrap_or(f64::NAN)),
    })
}

fn write_json(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(_) => out.push_str(&json_number_to_string(value).unwrap_or_default()),
        Value::String(text) => out.push_str(&serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()));
                out.push(':');
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_units() {
        assert_eq!(utf16_len("aé😀"), 4);
        assert_eq!(utf16_slice("aé😀b", 0, 2), "aé");
        assert_eq!(utf16_slice("aé😀b", 2, 4), "😀");
        assert_eq!(utf16_truncate("abc", 10), "abc");
    }

    #[test]
    fn date_formats() {
        let date = Utc.with_ymd_and_hms(2026, 11, 21, 5, 53, 8).unwrap();
        assert_eq!(utc_string(&date), "Sat, 21 Nov 2026 05:53:08 GMT");
        assert_eq!(iso_string(&date), "2026-11-21T05:53:08.000Z");
    }

    #[test]
    fn parses_iso_dates() {
        assert_eq!(date_parse("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(date_parse("1970-01-01"), Some(0));
        assert_eq!(date_parse("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(date_parse("2026-11-21T05:53:08.123456Z"), Some(1795240388123));
        assert_eq!(date_parse("2026"), Some(1767225600000));
        assert_eq!(date_parse("garbage"), None);
        assert_eq!(date_parse(""), None);
        assert_eq!(date_parse("2026-13-01"), None);
    }

    #[test]
    fn parses_legacy_dates() {
        assert_eq!(date_parse("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(date_parse("Sat, 21 Nov 2026 05:53:08 GMT"), Some(1795240388000));
        assert_eq!(date_parse("21 Nov 2026 06:53:08 +0100"), Some(1795240388000));
    }

    #[test]
    fn uri_components() {
        assert_eq!(encode_uri_component("s:a.b+c/d="), "s%3Aa.b%2Bc%2Fd%3D");
        assert_eq!(decode_uri_component("a%20b%2F").as_deref(), Some("a b/"));
        assert_eq!(decode_uri_component("%E2%9C%93").as_deref(), Some("✓"));
        assert_eq!(decode_uri_component("%E0%A4%A"), None);
        assert_eq!(decode_uri_component("%"), None);
        assert_eq!(decode_uri_component("%zz"), None);
        assert_eq!(decode_uri_component("%ff"), None);
    }

    #[test]
    fn numbers_from_text() {
        assert_eq!(number_from_str(" 12 "), 12.0);
        assert_eq!(number_from_str(""), 0.0);
        assert_eq!(number_from_str(" \t\n"), 0.0);
        assert!(number_from_str("12abc").is_nan());
        assert!(number_from_str("nan").is_nan());
        assert!(number_from_str("infinity").is_nan());
        assert_eq!(number_from_str("0x10"), 16.0);
        assert_eq!(number_from_str("0b101"), 5.0);
        assert!(number_from_str("0x").is_nan());
        assert!(number_from_str("-0x10").is_nan());
        assert_eq!(number_from_str("+5"), 5.0);
        assert_eq!(number_from_str("5."), 5.0);
        assert_eq!(number_from_str(".5e1"), 5.0);
        assert!(number_from_str(".").is_nan());
        assert!(number_from_str("1e").is_nan());
        // The whitespace JavaScript trims: a no-break space and a byte order
        // mark are in it, U+0085 (which Rust calls whitespace) is not.
        assert_eq!(number_from_str("\u{a0}7\u{feff}"), 7.0);
        assert!(number_from_str("\u{85}7").is_nan());
    }

    #[test]
    fn numbers_to_text() {
        for (value, text) in [
            (3.0, "3"),
            (0.5, "0.5"),
            (-0.0, "0"),
            (-1.5, "-1.5"),
            (100.0, "100"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123456789012345680000.0, "123456789012345680000"),
            (1.5e300, "1.5e+300"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (1.25e-7, "1.25e-7"),
            (0.30000000000000004, "0.30000000000000004"),
            (9007199254740993.0, "9007199254740992"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (-2.5e-10, "-2.5e-10"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(number_to_string(value), text, "{value:e}");
        }
        assert_eq!(number_to_string(f64::NAN), "NaN");
    }

    #[test]
    fn quoting_utf16() {
        let units: Vec<u16> = "a\"b\\c\n\u{1}é😀".encode_utf16().collect();
        assert_eq!(json_quote_utf16(&units), "\"a\\\"b\\\\c\\n\\u0001é😀\"");
        // A cut through the middle of a character leaves its first half.
        let cut: Vec<u16> = "x😀".encode_utf16().take(2).collect();
        assert_eq!(json_quote_utf16(&cut), "\"x\\ud83d\"");
        assert_eq!(json_quote_utf16(&[0xdc00, 0x61]), "\"\\udc00a\"");
    }

    #[test]
    fn stringify_like_javascript() {
        let value = json_parse(r#"{"a":1.0,"b":1e21,"c":1e-7,"d":[1.5,-0.0,true,null,"x\n"],"e":123456789012345678901}"#).unwrap();
        assert_eq!(
            json_stringify(&value),
            r#"{"a":1,"b":1e+21,"c":1e-7,"d":[1.5,0,true,null,"x\n"],"e":123456789012345680000}"#
        );
    }
}

/// One step into a JSON document: an object's key or an array's index.
#[derive(Debug, Clone, PartialEq)]
pub enum PathKey {
    Key(String),
    Index(usize),
}

/// A number in a JSON text that is outside the range of a double. JavaScript
/// reads it as `Infinity` or `-Infinity`, which JSON cannot carry: the parsed
/// value holds `null` at `path`, and this records what was there.
#[derive(Debug, Clone, PartialEq)]
pub struct NonFinite {
    pub path: Vec<PathKey>,
    pub negative: bool,
}

/// What [`json_parse_full`] makes of a JSON text.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedJson {
    pub value: serde_json::Value,
    pub non_finite: Vec<NonFinite>,
}

impl ParsedJson {
    /// `Infinity` or `-Infinity` when the number at `path` was one, as text.
    pub fn non_finite_at(&self, path: &[PathKey]) -> Option<&'static str> {
        non_finite_at(&self.non_finite, path)
    }
}

/// `Infinity` or `-Infinity` when `non_finite` records one at `path`.
pub fn non_finite_at(non_finite: &[NonFinite], path: &[PathKey]) -> Option<&'static str> {
    non_finite
        .iter()
        .find(|entry| entry.path == path)
        .map(|entry| if entry.negative { "-Infinity" } else { "Infinity" })
}

/// `JSON.parse(text)`, or `None` where it throws.
///
/// `serde_json` alone is stricter than JavaScript in ways a client can reach:
/// it refuses a number too large for a double (JavaScript reads `Infinity`),
/// an escaped lone surrogate, and nesting past 128 levels. This parser takes
/// what JavaScript takes. An integral number is stored as an integer so it
/// prints without a fraction, as JavaScript prints it; a number outside the
/// double range, which JSON cannot carry, is stored as `null` (see
/// [`json_parse_full`] for where they were).
///
/// Nesting may be as deep as the text is long: the parser keeps its own stack
/// rather than recursing. What it builds stops at [`JSON_BUILD_DEPTH`] levels
/// — anything nested deeper is checked for syntax and then held as `null` —
/// because a value is dropped, cloned and serialized recursively.
pub fn json_parse(text: &str) -> Option<serde_json::Value> {
    json_parse_full(text).map(|parsed| parsed.value)
}

/// [`json_parse`], also reporting the numbers that were out of range.
pub fn json_parse_full(text: &str) -> Option<ParsedJson> {
    let mut parser = JsonParser { bytes: text.as_bytes(), at: 0, non_finite: Vec::new() };
    let value = parser.document()?;
    Some(ParsedJson { value, non_finite: parser.non_finite })
}

/// How many levels of nesting a parsed value keeps.
pub const JSON_BUILD_DEPTH: usize = 4096;

/// A container the parser is in the middle of.
enum Frame {
    Array(Vec<serde_json::Value>),
    /// The members so far, the key whose value is being read, and how many
    /// out-of-range numbers had been recorded when that value began.
    Object(serde_json::Map<String, serde_json::Value>, String, usize),
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    at: usize,
    non_finite: Vec<NonFinite>,
}

impl JsonParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn literal(&mut self, word: &str, value: serde_json::Value) -> Option<serde_json::Value> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Some(value)
        } else {
            None
        }
    }

    /// `"key" :` with the whitespace around it.
    fn key(&mut self) -> Option<String> {
        self.skip_whitespace();
        if self.peek()? != b'"' {
            return None;
        }
        let key = self.string()?;
        self.skip_whitespace();
        if self.peek()? != b':' {
            return None;
        }
        self.at += 1;
        Some(key)
    }

    /// Where the value being read sits, from the containers around it.
    fn path(stack: &[Frame]) -> Vec<PathKey> {
        stack
            .iter()
            .map(|frame| match frame {
                Frame::Array(items) => PathKey::Index(items.len()),
                Frame::Object(_, key, _) => PathKey::Key(key.clone()),
            })
            .collect()
    }

    fn document(&mut self) -> Option<serde_json::Value> {
        use serde_json::Value;
        // Containers being built, outermost first.
        let mut stack: Vec<Frame> = Vec::new();
        // Containers open beyond the build depth: only whether each is an object.
        let mut deeper: Vec<bool> = Vec::new();

        loop {
            // A value starts here.
            self.skip_whitespace();
            let mut value = match self.peek()? {
                open @ (b'{' | b'[') => {
                    let is_object = open == b'{';
                    self.at += 1;
                    self.skip_whitespace();
                    if self.peek()? == (if is_object { b'}' } else { b']' }) {
                        self.at += 1;
                        if is_object { Value::Object(Default::default()) } else { Value::Array(Vec::new()) }
                    } else {
                        let key = if is_object { Some(self.key()?) } else { None };
                        if !deeper.is_empty() || stack.len() >= JSON_BUILD_DEPTH {
                            deeper.push(is_object);
                        } else {
                            stack.push(match key {
                                Some(key) => Frame::Object(Default::default(), key, self.non_finite.len()),
                                None => Frame::Array(Vec::new()),
                            });
                        }
                        continue;
                    }
                }
                b'"' => Value::String(self.string()?),
                b't' => self.literal("true", Value::Bool(true))?,
                b'f' => self.literal("false", Value::Bool(false))?,
                b'n' => self.literal("null", Value::Null)?,
                b'-' | b'0'..=b'9' => match self.number()? {
                    Ok(number) => number,
                    Err(negative) => {
                        if deeper.is_empty() {
                            self.non_finite.push(NonFinite { path: Self::path(&stack), negative });
                        }
                        Value::Null
                    }
                },
                _ => return None,
            };

            // The value is complete: it goes into its container, which may
            // end with it, and so on outwards.
            loop {
                self.skip_whitespace();
                if let Some(is_object) = deeper.last().copied() {
                    // Too deep to keep: only the syntax counts.
                    match (self.peek()?, is_object) {
                        (b',', true) => {
                            self.at += 1;
                            self.key()?;
                            break;
                        }
                        (b',', false) => {
                            self.at += 1;
                            break;
                        }
                        (b'}', true) | (b']', false) => {
                            self.at += 1;
                            deeper.pop();
                            value = Value::Null;
                            continue;
                        }
                        _ => return None,
                    }
                }
                let Some(top) = stack.len().checked_sub(1) else {
                    return (self.at == self.bytes.len()).then_some(value);
                };
                match &mut stack[top] {
                    Frame::Array(items) => {
                        items.push(value);
                        match self.peek()? {
                            b',' => {
                                self.at += 1;
                                break;
                            }
                            b']' => {
                                self.at += 1;
                                let Some(Frame::Array(items)) = stack.pop() else { return None };
                                value = Value::Array(items);
                            }
                            _ => return None,
                        }
                    }
                    Frame::Object(members, key, mark) => {
                        let key = std::mem::take(key);
                        let (repeated, mark) = (members.contains_key(&key), *mark);
                        members.insert(key.clone(), value);
                        if repeated {
                            // The last of a repeated key wins: whatever the
                            // earlier value recorded is gone with it.
                            let mut replaced = Self::path(&stack[..top]);
                            replaced.push(PathKey::Key(key));
                            let mut index = 0;
                            self.non_finite.retain(|entry| {
                                let keep = index >= mark || !entry.path.starts_with(&replaced);
                                index += 1;
                                keep
                            });
                        }
                        match self.peek()? {
                            b',' => {
                                self.at += 1;
                                let next = self.key()?;
                                let Frame::Object(_, key_slot, mark_slot) = &mut stack[top] else { return None };
                                *key_slot = next;
                                *mark_slot = self.non_finite.len();
                                break;
                            }
                            b'}' => {
                                self.at += 1;
                                let Some(Frame::Object(members, _, _)) = stack.pop() else { return None };
                                value = Value::Object(members);
                            }
                            _ => return None,
                        }
                    }
                }
            }
        }
    }

    /// A number, or `Err(negative)` for one outside the range of a double.
    fn number(&mut self) -> Option<Result<serde_json::Value, bool>> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek()? {
            b'0' => self.at += 1,
            b'1'..=b'9' => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.at += 1;
                }
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).ok()?;
        let value: f64 = text.parse().ok()?;
        if !value.is_finite() {
            return Some(Err(value < 0.0));
        }
        if value == value.trunc() && value.abs() <= 9_007_199_254_740_992.0 {
            return Some(Ok(serde_json::Value::from(value as i64)));
        }
        serde_json::Number::from_f64(value).map(|number| Ok(serde_json::Value::Number(number)))
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.at..self.at + 4)?;
        let text = std::str::from_utf8(digits).ok()?;
        if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        self.at += 4;
        u32::from_str_radix(text, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let byte = self.peek()?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                0..=0x1f => return None,
                b'\\' => {
                    let escape = self.peek()?;
                    self.at += 1;
                    let ch = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let unit = self.hex4()?;
                            match unit {
                                0xD800..=0xDBFF => {
                                    // A high surrogate pairs with a following low one.
                                    let rewind = self.at;
                                    let low = if self.bytes[self.at..].starts_with(b"\\u") {
                                        self.at += 2;
                                        self.hex4()
                                    } else {
                                        None
                                    };
                                    match low {
                                        Some(low @ 0xDC00..=0xDFFF) => {
                                            let code = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                                            char::from_u32(code).unwrap_or('\u{fffd}')
                                        }
                                        _ => {
                                            self.at = rewind;
                                            '\u{fffd}'
                                        }
                                    }
                                }
                                0xDC00..=0xDFFF => '\u{fffd}',
                                unit => char::from_u32(unit).unwrap_or('\u{fffd}'),
                            }
                        }
                        _ => return None,
                    };
                    let mut buffer = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                }
                byte => out.push(byte),
            }
        }
    }
}

#[cfg(test)]
mod json_tests {
    use super::{json_parse, json_parse_full, PathKey, JSON_BUILD_DEPTH};
    use serde_json::json;

    #[test]
    fn parses_what_javascript_parses() {
        assert_eq!(json_parse(r#"{"a":[1,2.5,"x",true,null]}"#), Some(json!({"a": [1, 2.5, "x", true, null]})));
        assert_eq!(json_parse(" [ ] "), Some(json!([])));
        assert_eq!(json_parse(" { } "), Some(json!({})));
        assert_eq!(json_parse("1.0"), Some(json!(1)));
        assert_eq!(json_parse("1e3"), Some(json!(1000)));
        assert_eq!(json_parse(r#"{"a":1e400}"#), Some(json!({"a": null})));
        assert_eq!(json_parse(r#"{"a":"\ud800"}"#), Some(json!({"a": "\u{fffd}"})));
        assert_eq!(json_parse(r#""😀""#), Some(json!("😀")));
        assert_eq!(json_parse(r#"{"a":1,"b":2,"a":3}"#).unwrap().to_string(), r#"{"a":3,"b":2}"#);
        assert_eq!(
            json_parse(r#" { "a" : [ { "b" : [ ] } , [ [ 1 ] , { } ] ] , "c" : "d" } "#),
            Some(json!({"a": [{"b": []}, [[1], {}]], "c": "d"}))
        );
    }

    /// The server's threads have 16 MiB stacks, a test thread 2 MiB: parsing
    /// needs none of it, but dropping what was built is recursive.
    fn on_a_server_sized_stack(test: fn()) {
        std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn(test).unwrap().join().unwrap();
    }

    #[test]
    fn nesting_as_deep_as_the_text_is_long() {
        on_a_server_sized_stack(|| {
            for depth in [10, JSON_BUILD_DEPTH - 1, JSON_BUILD_DEPTH, JSON_BUILD_DEPTH + 1, 200_000] {
                let arrays = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
                assert!(json_parse(&arrays).is_some_and(|value| value.is_array()), "{depth} arrays");
                let objects = format!("{}1{}", r#"{"a":"#.repeat(depth), "}".repeat(depth));
                assert!(json_parse(&objects).is_some_and(|value| value.is_object()), "{depth} objects");
                let mixed = format!("{}null{}", r#"[{"a":"#.repeat(depth), "}]".repeat(depth));
                assert!(json_parse(&mixed).is_some(), "{depth} mixed");
                // The same texts, not closed properly.
                assert!(json_parse(&arrays[..arrays.len() - 1]).is_none());
                assert!(json_parse(&format!("{arrays}]")).is_none());
                assert!(json_parse(&format!("{}{}", "[".repeat(depth), "}".repeat(depth))).is_none());
                assert!(json_parse(&format!("{}1,{}", r#"{"a":"#.repeat(depth), "}".repeat(depth))).is_none());
            }
        });
    }

    #[test]
    fn what_is_built_stops_at_the_build_depth() {
        on_a_server_sized_stack(|| {
            let depth = JSON_BUILD_DEPTH + 50;
            let text = format!(r#"{{"shallow":1,"deep":{}7{},"after":2}}"#, "[".repeat(depth), "]".repeat(depth));
            let value = json_parse(&text).unwrap();
            assert_eq!(value["shallow"], json!(1));
            assert_eq!(value["after"], json!(2));
            // The object is one level; the arrays below it fill the rest.
            let mut arrays = 0;
            let mut at = &value["deep"];
            while at.is_array() {
                arrays += 1;
                at = &at[0];
            }
            assert!(at.is_null());
            assert_eq!(arrays, JSON_BUILD_DEPTH - 1);
        });
    }

    #[test]
    fn records_numbers_out_of_range() {
        let parsed = json_parse_full(r#"{"a":1e400,"b":[1,-1e999,{"c":1e309}],"d":2}"#).unwrap();
        assert_eq!(parsed.value, json!({"a": null, "b": [1, null, {"c": null}], "d": 2}));
        let key = |name: &str| PathKey::Key(name.to_string());
        assert_eq!(parsed.non_finite_at(&[key("a")]), Some("Infinity"));
        assert_eq!(parsed.non_finite_at(&[key("b"), PathKey::Index(1)]), Some("-Infinity"));
        assert_eq!(parsed.non_finite_at(&[key("b"), PathKey::Index(2), key("c")]), Some("Infinity"));
        assert_eq!(parsed.non_finite_at(&[key("d")]), None);
        assert_eq!(json_parse_full("1e400").unwrap().non_finite_at(&[]), Some("Infinity"));
    }

    #[test]
    fn a_repeated_key_forgets_what_the_first_recorded() {
        let key = |name: &str| PathKey::Key(name.to_string());
        let replaced = json_parse_full(r#"{"a":1e400,"a":5}"#).unwrap();
        assert_eq!(replaced.value, json!({"a": 5}));
        assert_eq!(replaced.non_finite_at(&[key("a")]), None);
        let nested = json_parse_full(r#"{"a":{"b":1e400},"a":{"b":null}}"#).unwrap();
        assert_eq!(nested.non_finite_at(&[key("a"), key("b")]), None);
        let kept = json_parse_full(r#"{"a":5,"a":-1e400}"#).unwrap();
        assert_eq!(kept.value, json!({"a": null}));
        assert_eq!(kept.non_finite_at(&[key("a")]), Some("-Infinity"));
    }

    #[test]
    fn refuses_what_javascript_refuses() {
        for bad in [
            "", "{", r#"{"a":}"#, "[1,]", "{'a':1}", "01", "1.", "+1", "NaN", "tru", r#""\x""#, "[1] x", "\"a\nb\"",
            r#"{"a":1,}"#, ".5", "-", "[", "]", "{]", "[}", r#"{"a" 1}"#, r#"{"a":1 "b":2}"#, r#"{1:2}"#, "[1 2]",
            r#"{"a":1}}"#, "[[]", "{}{}", r#"{"a"}"#, r#"{,"a":1}"#, "[,1]", "nulll", "1e", "0x10",
        ] {
            assert_eq!(json_parse(bad), None, "{bad:?}");
        }
    }
}
