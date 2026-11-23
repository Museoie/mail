//! Header parsing and RFC 2047 words (mime/header.ts).

use std::collections::{HashMap, HashSet};

use encoding_rs::Encoding;
use serde::ser::{Serialize, SerializeMap, Serializer};

use super::charset::{decode_header_bytes, jsstr, text_decoder};

/// What two charset labels are compared by: the encoding both name, or the
/// label itself when it names none.
#[derive(PartialEq)]
enum CanonicalCharset {
    Encoding(&'static Encoding),
    Label(String),
}

fn canonical_charset(label: &str) -> CanonicalCharset {
    let label = jsstr::trim(label);
    match text_decoder(label) {
        Some(encoding) => CanonicalCharset::Encoding(encoding),
        None => CanonicalCharset::Label(label.to_lowercase()),
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

/// The octets of a Q-encoded word's text.
fn decode_q(text: &str) -> Vec<u8> {
    // `=`, whitespace, then a hex digit: the whitespace is dropped. Then
    // every `_` and whitespace character is a space.
    let mut source = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '=' {
            let mut ahead = chars.clone();
            let mut skipped = 0;
            while ahead.peek().copied().is_some_and(jsstr::is_space) {
                ahead.next();
                skipped += 1;
            }
            if skipped > 0 {
                if let Some(digit) = ahead.peek().copied().filter(char::is_ascii_hexdigit) {
                    ahead.next();
                    chars = ahead;
                    source.push('=');
                    source.push(digit);
                    continue;
                }
            }
            source.push('=');
        } else if c == '_' || jsstr::is_space(c) {
            source.push(' ');
        } else {
            source.push(c);
        }
    }

    let source = source.as_bytes();
    let mut bytes = Vec::with_capacity(source.len());
    let mut i = 0;
    while i < source.len() {
        if source[i] == b'=' && i + 2 < source.len() {
            if let (Some(high), Some(low)) = (hex_value(source[i + 1]), hex_value(source[i + 2])) {
                bytes.push(high << 4 | low);
                i += 3;
                continue;
            }
        }
        bytes.push(source[i]);
        i += 1;
    }
    bytes
}

fn base64_value(byte: u8) -> Option<u32> {
    Some(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => return None,
    } as u32)
}

/// `Buffer.from(text, "base64")`: both alphabets are read, anything else is
/// skipped, `=` ends the input and a lone trailing character is dropped.
/// Each UTF-16 code unit counts by its low byte, as Bun reads it.
fn buffer_from_base64(text: &str, out: &mut Vec<u8>) {
    let mut quantum = 0u32;
    let mut filled = 0;
    let mut units = [0u16; 2];
    'text: for c in text.chars() {
        for unit in c.encode_utf16(&mut units) {
            let byte = *unit as u8;
            if byte == b'=' {
                break 'text;
            }
            let Some(value) = base64_value(byte) else {
                continue;
            };
            quantum = quantum << 6 | value;
            filled += 1;
            if filled == 4 {
                out.extend_from_slice(&quantum.to_be_bytes()[1..]);
                quantum = 0;
                filled = 0;
            }
        }
    }
    match filled {
        2 => out.push((quantum >> 4) as u8),
        3 => out.extend_from_slice(&[(quantum >> 10) as u8, (quantum >> 2) as u8]),
        _ => {}
    }
}

/// The octets of a B-encoded word's text.
fn decode_b(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    for part in text.split('=').filter(|part| !part.is_empty()) {
        buffer_from_base64(part, &mut bytes);
    }
    bytes
}

fn decode_word(charset: &str, encoding: u8, text: &str) -> String {
    let label = charset.split('*').next().unwrap_or(charset);
    let bytes = if encoding.eq_ignore_ascii_case(&b'Q') {
        decode_q(text)
    } else {
        decode_b(text)
    };
    decode_header_bytes(&bytes, label)
}

const JOIN: &str = "__\x00JOIN\x00__";

const B: &[u8] = b"Bb";
const Q: &[u8] = b"Qq";
const ANY: &[u8] = b"QqBb";

/// One `=?charset?X?text?=` at byte offset `at`, with `X` among `encodings`.
struct EncodedWord<'a> {
    /// The whole word, delimiters included.
    word: &'a str,
    end: usize,
    charset: &'a str,
    encoding: u8,
    text: &'a str,
}

/// `=\?([^?]+)\?[X]\?` at `at`: the charset, the encoding letter and where
/// the text starts.
fn word_prefix_at<'a>(value: &'a str, at: usize, encodings: &[u8]) -> Option<(&'a str, u8, usize)> {
    let bytes = value.as_bytes();
    if !bytes[at..].starts_with(b"=?") {
        return None;
    }
    let charset_start = at + 2;
    let charset_end = charset_start + bytes[charset_start..].iter().position(|&b| b == b'?')?;
    if charset_end == charset_start {
        return None;
    }
    let encoding = *bytes.get(charset_end + 1)?;
    if !encodings.contains(&encoding) || bytes.get(charset_end + 2) != Some(&b'?') {
        return None;
    }
    Some((&value[charset_start..charset_end], encoding, charset_end + 3))
}

/// `=\?([^?]+)\?[X]\?[^?]*\?=` at `at`.
fn word_at<'a>(value: &'a str, at: usize, encodings: &[u8]) -> Option<EncodedWord<'a>> {
    let bytes = value.as_bytes();
    let (charset, encoding, text_start) = word_prefix_at(value, at, encodings)?;
    let text_end = text_start + bytes[text_start..].iter().position(|&b| b == b'?')?;
    if bytes.get(text_end + 1) != Some(&b'=') {
        return None;
    }
    let end = text_end + 2;
    Some(EncodedWord {
        word: &value[at..end],
        end,
        charset,
        encoding,
        text: &value[text_start..text_end],
    })
}

/// Where the run of whitespace starting at `at` ends.
fn skip_space(value: &str, at: usize) -> usize {
    at + (value.len() - at - jsstr::trim_start(&value[at..]).len())
}

/// Whether `value` is nothing but B-encoded words, with or without
/// whitespace between them.
pub(crate) fn only_b_encoded_words(value: &str) -> bool {
    let mut at = 0;
    while let Some(word) = word_at(value, at, B) {
        if word.end == value.len() {
            return true;
        }
        at = skip_space(value, word.end);
    }
    false
}

/// Rewrites each encoded word that is followed, after at least `min_space`
/// whitespace, by another one. `rewrite` gets the two words and writes what
/// replaces the first word and the whitespace, or answers `false` to keep
/// them as they are.
fn replace_adjacent<F>(value: &str, encodings: &[u8], min_space: usize, mut rewrite: F) -> String
where
    F: FnMut(&EncodedWord, &EncodedWord, &mut String) -> bool,
{
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut last = 0;
    let mut at = 0;
    while let Some(found) = bytes[at..].iter().position(|&b| b == b'=') {
        let start = at + found;
        at = start + 1;
        let Some(left) = word_at(value, start, encodings) else {
            continue;
        };
        let space_end = skip_space(value, left.end);
        if space_end - left.end < min_space {
            continue;
        }
        let Some(right) = word_at(value, space_end, encodings) else {
            continue;
        };
        out.push_str(&value[last..start]);
        if !rewrite(&left, &right, &mut out) {
            out.push_str(&value[start..space_end]);
        }
        last = space_end;
        at = space_end;
    }
    out.push_str(&value[last..]);
    out
}

/// Marks a word for joining with the next when both are in one charset.
fn mark_joinable(value: &str, encodings: &[u8]) -> String {
    replace_adjacent(value, encodings, 0, |left, right, out| {
        if canonical_charset(left.charset) != canonical_charset(right.charset) {
            return false;
        }
        out.push_str(left.word);
        out.push_str(JOIN);
        true
    })
}

/// Drops each join mark with the `?=` before it and the `=?charset?X?` after.
fn apply_joins(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut at = 0;
    while let Some(found) = value[at..].find(JOIN) {
        let mark = at + found;
        let start = if mark >= at + 2 && value[..mark].ends_with("?=") {
            mark - 2
        } else {
            mark
        };
        out.push_str(&value[at..start]);
        at = mark + JOIN.len();
        if let Some((_, _, text_start)) = word_prefix_at(value, at, ANY) {
            at = text_start;
        }
    }
    out.push_str(&value[at..]);
    out
}

/// Decodes RFC 2047 encoded words anywhere in `value`. Adjacent words in the
/// same charset are joined before decoding, so a multi-byte character split
/// across two words comes out whole; whitespace between encoded words is
/// dropped as the RFC requires.
pub fn decode_words(value: &str) -> String {
    if !value.contains("=?") && !value.contains(JOIN) {
        return value.to_string();
    }
    let joined = apply_joins(&mark_joinable(&mark_joinable(value, B), Q));
    let packed = replace_adjacent(&joined, ANY, 1, |left, _, out| {
        out.push_str(left.word);
        true
    });

    let bytes = packed.as_bytes();
    let mut out = String::with_capacity(packed.len());
    let mut last = 0;
    let mut at = 0;
    while let Some(found) = bytes[at..].iter().position(|&b| b == b'=') {
        let start = at + found;
        at = start + 1;
        let Some(word) = word_at(&packed, start, ANY) else {
            continue;
        };
        if !word
            .charset
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'*'))
        {
            continue;
        }
        out.push_str(&packed[last..start]);
        out.push_str(&decode_word(word.charset, word.encoding, word.text));
        last = word.end;
        at = word.end;
    }
    out.push_str(&packed[last..]);
    out
}

/// Re-reads a string of raw octets as UTF-8, when those octets are valid UTF-8.
pub fn raw_octets_as_utf8(value: &str) -> String {
    let mut high = false;
    for c in value.chars() {
        if c > '\u{ff}' {
            return value.to_string();
        }
        high |= c >= '\u{80}';
    }
    if !high {
        return value.to_string();
    }
    let octets: Vec<u8> = value.chars().map(|c| c as u8).collect();
    match String::from_utf8(octets) {
        // A fatal `TextDecoder` still drops a byte order mark.
        Ok(text) => text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text),
        Err(_) => value.to_string(),
    }
}

/// One header line as its lowercased key and unfolded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderLine {
    pub key: String,
    pub value: String,
}

/// Splits one raw (possibly folded) header line into its lowercased key and
/// unfolded value.
pub fn split_header_line(line: &str) -> HeaderLine {
    // A line break and the indentation after it become one space.
    let mut unfolded = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\r' && c != '\n' {
            unfolded.push(c);
            continue;
        }
        if c == '\r' && chars.peek() == Some(&'\n') {
            chars.next();
        }
        while matches!(chars.peek(), Some(' ' | '\t')) {
            chars.next();
        }
        unfolded.push(' ');
    }
    let unfolded = jsstr::trim(&unfolded);

    // `^\s*([^:]+):(.*)$`, where `.` stops at a line terminator.
    let split = unfolded
        .split_once(':')
        .filter(|(key, value)| !key.is_empty() && !value.contains(jsstr::is_line_terminator));
    match split {
        Some((key, value)) => HeaderLine {
            key: jsstr::trim(key).to_lowercase(),
            value: jsstr::trim(value).to_string(),
        },
        None => HeaderLine {
            key: String::new(),
            value: String::new(),
        },
    }
}

/// The parameters of a structured header value, in the order they were
/// first written.
#[derive(Debug, Clone, Default)]
pub struct HeaderParams {
    slots: Vec<Option<(String, String)>>,
    index: HashMap<String, usize>,
}

impl HeaderParams {
    pub fn get(&self, name: &str) -> Option<&str> {
        let slot = self.slots[*self.index.get(name)?].as_ref()?;
        Some(slot.1.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.slots
            .iter()
            .flatten()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn set(&mut self, name: &str, value: String) {
        // Assigning a string to `__proto__` on a JavaScript object does nothing.
        if name == "__proto__" {
            return;
        }
        match self.index.get(name) {
            Some(&slot) => self.slots[slot] = Some((name.to_string(), value)),
            None => {
                self.index.insert(name.to_string(), self.slots.len());
                self.slots.push(Some((name.to_string(), value)));
            }
        }
    }

    fn remove(&mut self, name: &str) -> Option<String> {
        let slot = self.index.remove(name)?;
        self.slots[slot].take().map(|(_, value)| value)
    }
}

impl Serialize for HeaderParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.len()))?;
        for (name, value) in self.iter() {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

/// `value; key=value; key="quoted value"` taken apart.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StructuredHeaderValue {
    pub value: String,
    pub params: HeaderParams,
}

/// A parameter name's RFC 2231 suffix, `/\*((\d+)\*?)?$/`: where it starts,
/// its section number and whether it ends in `*`.
fn continuation_suffix(name: &str) -> Option<(usize, Option<&str>, bool)> {
    let starred = name.ends_with('*');
    let body = name.strip_suffix('*').unwrap_or(name);
    let digits = body.len() - body.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 && body[..body.len() - digits].ends_with('*') {
        let start = body.len() - digits - 1;
        return Some((start, Some(&body[start + 1..]), starred));
    }
    starred.then_some((body.len(), None, true))
}

struct Continuation {
    name: String,
    charset: Option<String>,
    parts: Vec<(f64, String)>,
}

/// Parses `value; key=value; key="quoted value"` as used by Content-Type and
/// Content-Disposition, including RFC 2231 continuations (`name*0=`,
/// `name*1=`) and charset-tagged values (`name*=utf-8''%ED%95%9C`).
pub fn parse_header_value(input: &str) -> StructuredHeaderValue {
    let mut params = HeaderParams::default();
    let mut main = String::new();
    let mut key: Option<String> = None;
    let mut value = String::new();
    let mut in_key = false;
    let mut quoted = false;
    let mut escaped = false;

    for chr in input.chars() {
        if in_key {
            if chr == '=' {
                key = Some(jsstr::trim(&value).to_lowercase());
                in_key = false;
                value.clear();
            } else {
                value.push(chr);
            }
            continue;
        }
        if escaped {
            value.push(chr);
        } else if chr == '\\' {
            escaped = true;
            continue;
        } else if chr == '"' {
            quoted = !quoted;
        } else if !quoted && chr == ';' {
            match &key {
                None => main = jsstr::trim(&value).to_string(),
                Some(key) => params.set(key, jsstr::trim(&value).to_string()),
            }
            in_key = true;
            value.clear();
        } else {
            value.push(chr);
        }
        escaped = false;
    }

    let value = jsstr::trim(&value);
    if !in_key {
        match &key {
            None => main = value.to_string(),
            Some(key) => params.set(key, value.to_string()),
        }
    } else if !value.is_empty() {
        params.set(&value.to_lowercase(), String::new());
    }

    let mut continued: Vec<Continuation> = Vec::new();
    let mut continued_index: HashMap<String, usize> = HashMap::new();
    let names: Vec<String> = params.iter().map(|(name, _)| name.to_string()).collect();
    for name in names {
        let Some((start, digits, starred)) = continuation_suffix(&name) else {
            continue;
        };
        let actual = name[..start].to_lowercase();
        // `Number(digits) || 0`: an absurdly long number is Infinity.
        let nr: f64 = digits.and_then(|digits| digits.parse().ok()).unwrap_or(0.0);
        let entry = *continued_index.entry(actual.clone()).or_insert_with(|| {
            continued.push(Continuation {
                name: actual,
                charset: None,
                parts: Vec::new(),
            });
            continued.len() - 1
        });
        let entry = &mut continued[entry];
        let mut part = params.remove(&name).unwrap_or_default();
        if nr == 0.0 && starred {
            // `^([^']*)'[^']*'(.*)$`
            let mut pieces = part.splitn(3, '\'');
            if let (Some(charset), Some(_), Some(rest)) = (pieces.next(), pieces.next(), pieces.next()) {
                if !rest.contains(jsstr::is_line_terminator) {
                    entry.charset = Some(if charset.is_empty() { "utf-8" } else { charset }.to_string());
                    part = rest.to_string();
                }
            }
        }
        entry.parts.push((nr, part));
    }

    for mut entry in continued {
        entry
            .parts
            .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let joined: String = entry.parts.iter().map(|(_, part)| part.as_str()).collect();
        let decoded = match &entry.charset {
            Some(charset) => {
                // The value re-spelled as the text of a Q-encoded word.
                let mut word = format!("=?{charset}?Q?");
                let mut units = [0u16; 2];
                for c in joined.chars() {
                    match c {
                        ' ' => word.push('_'),
                        '%' => word.push('='),
                        '=' | '?' | '_' => word.push_str(&format!("={:02x}", c as u32)),
                        c if jsstr::is_space(c) => {
                            for unit in c.encode_utf16(&mut units) {
                                word.push_str(&format!("={unit:02x}"));
                            }
                        }
                        c => word.push(c),
                    }
                }
                word.push_str("?=");
                decode_words(&word)
            }
            None => decode_words(&joined),
        };
        params.set(&entry.name, decoded);
    }

    StructuredHeaderValue { value: main, params }
}

/// Longest Message-ID kept, matching the `message_id` column.
const MAX_MESSAGE_ID_LENGTH: usize = 512;

/// Most References entries kept: the oldest ones add nothing to threading.
pub const MAX_REFERENCE_IDS: usize = 50;

/// The Message-IDs in an `In-Reply-To` or `References` value, in order, each
/// in angle brackets. Comments and anything outside `<…>` are dropped, as are
/// duplicates and over-long ids; the newest `MAX_REFERENCE_IDS` stay.
pub fn parse_message_id_list(value: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut at = 0;
    while let Some(found) = value[at..].find('<') {
        let start = at + found;
        at = start + 1;
        // `<([^<>\s]+)>`
        let body = &value[start + 1..];
        let length = body
            .find(|c: char| c == '<' || c == '>' || jsstr::is_space(c))
            .unwrap_or(body.len());
        if length == 0 || !body[length..].starts_with('>') {
            continue;
        }
        let id = &value[start..start + length + 2];
        at = start + length + 2;
        if jsstr::len(id) > MAX_MESSAGE_ID_LENGTH || !seen.insert(id) {
            continue;
        }
        ids.push(id.to_string());
    }
    let skip = ids.len().saturating_sub(MAX_REFERENCE_IDS);
    ids.split_off(skip)
}

/// Wraps a message id in angle brackets, or answers `None` for an empty one.
/// An id longer than the column holds is dropped too, as it is from
/// `In-Reply-To` and `References`: the message is then stored as one without
/// a Message-ID, where keeping it would fail the save every time.
pub fn ensure_message_id_format(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let open = if value.starts_with('<') { "" } else { "<" };
    let close = if value.ends_with('>') { "" } else { ">" };
    let id = format!("{open}{value}{close}");
    (jsstr::len(&id) <= MAX_MESSAGE_ID_LENGTH).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_b_and_q_encoded_words() {
        assert_eq!(decode_words("=?UTF-8?B?7JWI64WV?="), "안녕");
        assert_eq!(decode_words("=?utf-8?q?caf=C3=A9_au_lait?="), "café au lait");
    }

    #[test]
    fn decodes_words_in_legacy_cjk_and_cyrillic_charsets() {
        assert_eq!(decode_words("=?EUC-KR?B?x9Gx2w==?="), "한글");
        assert_eq!(decode_words("=?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?="), "日本語");
        assert_eq!(decode_words("=?Shift_JIS?B?k/qWe4zq?="), "日本語");
        assert_eq!(decode_words("=?GB18030?B?1tDOxA==?="), "中文");
        assert_eq!(decode_words("=?windows-1251?B?7/Do4uXy?="), "привет");
        assert_eq!(decode_words("=?iso-8859-1?Q?R=E9sum=E9?="), "Résumé");
    }

    #[test]
    fn joins_adjacent_words_of_one_charset_before_decoding_a_split_character() {
        // "한" is ED 95 9C; the first word carries two of its three octets.
        assert_eq!(decode_words("=?UTF-8?Q?=ED=95?= =?UTF-8?Q?=9C?="), "한");
        assert_eq!(decode_words("=?UTF-8?B?7ZU=?= =?UTF-8?B?nA==?="), "한");
    }

    #[test]
    fn drops_whitespace_between_encoded_words_but_keeps_it_around_plain_text() {
        assert_eq!(decode_words("=?UTF-8?Q?a?=  =?UTF-8?Q?b?= c"), "ab c");
    }

    #[test]
    fn falls_back_to_utf8_for_an_unknown_charset_and_leaves_malformed_words_alone() {
        assert_eq!(decode_words("=?x-unknown?Q?abc?="), "abc");
        assert_eq!(decode_words("=?utf-8?Q?broken=ZZ?="), "broken=ZZ");
        assert_eq!(decode_words("plain =?not a word"), "plain =?not a word");
    }

    #[test]
    fn splits_the_value_from_quoted_and_bare_parameters() {
        let parsed = parse_header_value("text/plain; charset=\"UTF-8\"; format=flowed");
        assert_eq!(parsed.value, "text/plain");
        assert_eq!(
            parsed.params.iter().collect::<Vec<_>>(),
            [("charset", "UTF-8"), ("format", "flowed")]
        );
    }

    #[test]
    fn joins_rfc_2231_continuations_and_decodes_their_charset() {
        let parsed = parse_header_value(
            "attachment; filename*0*=UTF-8''%EB%B3%B4%EA%B3%A0; filename*1*=%EC%84%9C; filename*2=.pdf",
        );
        assert_eq!(parsed.params.get("filename"), Some("보고서.pdf"));
    }

    #[test]
    fn decodes_a_charset_tagged_value_without_continuations() {
        let parsed = parse_header_value("attachment; filename*=iso-8859-1''caf%E9.txt");
        assert_eq!(parsed.params.get("filename"), Some("café.txt"));
    }

    #[test]
    fn keeps_a_trailing_key_without_a_value() {
        let parsed = parse_header_value("inline; foo");
        assert_eq!(parsed.params.iter().collect::<Vec<_>>(), [("foo", "")]);
    }

    #[test]
    fn unfolds_a_folded_line_and_lowercases_the_key() {
        assert_eq!(
            split_header_line("Subject: one\r\n two"),
            HeaderLine {
                key: "subject".into(),
                value: "one two".into()
            }
        );
    }

    #[test]
    fn rereads_raw_utf8_octets_and_leaves_anything_else_as_it_is() {
        let raw: String = "한글.pdf".bytes().map(|b| b as char).collect();
        assert_eq!(raw_octets_as_utf8(&raw), "한글.pdf");
        assert_eq!(raw_octets_as_utf8("caf\u{e9}"), "caf\u{e9}");
        assert_eq!(raw_octets_as_utf8("보고서"), "보고서");
    }

    #[test]
    fn wraps_an_id_in_angle_brackets_once() {
        assert_eq!(ensure_message_id_format("abc@x").as_deref(), Some("<abc@x>"));
        assert_eq!(ensure_message_id_format("<abc@x>").as_deref(), Some("<abc@x>"));
        assert_eq!(ensure_message_id_format(""), None);
    }

    #[test]
    fn drops_an_id_longer_than_the_column_holds() {
        let fits = format!("<{}@remote.example>", "x".repeat(495));
        assert_eq!(fits.len(), 512);
        assert_eq!(ensure_message_id_format(&fits).as_deref(), Some(fits.as_str()));
        // Counted once the brackets are added.
        assert_eq!(ensure_message_id_format(&fits[1..fits.len() - 1]).as_deref(), Some(fits.as_str()));
        assert_eq!(ensure_message_id_format(&format!("<{}@remote.example>", "x".repeat(496))), None);
        assert_eq!(ensure_message_id_format(&format!("{}@remote.example", "x".repeat(496))), None);
        // In UTF-16 code units, as JavaScript counts.
        assert_eq!(ensure_message_id_format(&format!("<{}@x>", "😀".repeat(255))), None);
        assert!(ensure_message_id_format(&format!("<{}@x>", "😀".repeat(254))).is_some());
    }

    #[test]
    fn reads_every_bracketed_id_in_order() {
        assert_eq!(
            parse_message_id_list("<a@x> <b@y>\r\n <c@z>"),
            ["<a@x>", "<b@y>", "<c@z>"]
        );
    }

    #[test]
    fn drops_comments_text_outside_brackets_and_duplicates() {
        assert_eq!(
            parse_message_id_list("(old) <a@x> junk <a@x> <b@y>"),
            ["<a@x>", "<b@y>"]
        );
    }

    #[test]
    fn keeps_only_the_newest_ids_of_a_long_list() {
        let ids: Vec<String> = (0..80).map(|i| format!("<m{i}@x>")).collect();
        let parsed = parse_message_id_list(&ids.join(" "));
        assert_eq!(parsed.len(), MAX_REFERENCE_IDS);
        assert_eq!(parsed.last().map(String::as_str), Some("<m79@x>"));
    }

    #[test]
    fn drops_ids_longer_than_the_message_id_column() {
        let value = format!("<{}@x> <b@y>", "a".repeat(600));
        assert_eq!(parse_message_id_list(&value), ["<b@y>"]);
    }

    #[test]
    fn is_empty_for_a_value_without_ids() {
        assert!(parse_message_id_list("").is_empty());
        assert!(parse_message_id_list("no ids here").is_empty());
    }
}
