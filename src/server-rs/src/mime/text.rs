//! Text body decoding (mime/text.ts).

use super::address::{OneOrMany, ParsedAddress, ParsedAddressValue};
use super::charset::jsstr;
use crate::js;

fn push_hex_reference(out: &mut String, c: char) {
    out.push_str(&format!("&#x{:X};", c as u32));
}

fn is_markup(c: char) -> bool {
    matches!(c, '&' | '<' | '>' | '"' | '\'' | '`') || !c.is_ascii()
}

/// Escapes text for HTML: markup-significant characters and every non-ASCII
/// character become hexadecimal character references.
pub fn encode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_markup(c) {
            push_hex_reference(&mut out, c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Like [`encode_entities`], but with the four named references HTML authors
/// expect.
fn encode_text(text: &[char], out: &mut String) {
    for &c in text {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if is_markup(c) => push_hex_reference(out, c),
            c => out.push(c),
        }
    }
}

fn is_label_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// A character of an e-mail address's local part: letters, digits and `._%+-`.
fn is_local_char(c: char) -> bool {
    is_label_char(c) || matches!(c, '.' | '_' | '%' | '+')
}

/// Ends a link: whitespace, `<`, `>` and `"`.
fn is_link_end(c: char) -> bool {
    matches!(c, '<' | '>' | '"') || jsstr::is_space(c)
}

const LINK_PREFIXES: [&str; 5] = ["http://", "https://", "git://", "mailto:", "www."];

fn starts_with_at(text: &[char], start: usize, prefix: &str) -> bool {
    let mut at = start;
    prefix.chars().all(|expected| {
        at += 1;
        text.get(at - 1) == Some(&expected)
    })
}

/// End of a scheme or `www.` link starting at `start`, when one starts there.
fn link_end_at(text: &[char], start: usize) -> Option<usize> {
    if start > 0 && jsstr::is_word(text[start - 1]) {
        return None;
    }
    let prefix = LINK_PREFIXES
        .iter()
        .find(|prefix| starts_with_at(text, start, prefix))?;
    let after_prefix = start + prefix.len();
    let mut end = after_prefix;
    while end < text.len() && !is_link_end(text[end]) {
        end += 1;
    }
    (end > after_prefix).then_some(end)
}

/// End of a dotted domain (`label(.label)+`) starting at `start`.
fn domain_end_at(text: &[char], start: usize) -> Option<usize> {
    let is_label = |i: usize| text.get(i).copied().is_some_and(is_label_char);
    let mut end = start;
    while is_label(end) {
        end += 1;
    }
    if end == start {
        return None;
    }
    let mut labels = 0;
    while text.get(end) == Some(&'.') && is_label(end + 1) {
        end += 2;
        while is_label(end) {
            end += 1;
        }
        labels += 1;
    }
    (labels > 0).then_some(end)
}

/// Finds the links in `text` as `[start, end)` ranges, left to right: `http`,
/// `https` and `git` URLs, `mailto:` and `www.` links, and bare e-mail
/// addresses. One forward pass; each run of address characters is measured
/// once, so the cost stays linear in the length of the text.
fn find_links(text: &[char]) -> Vec<(usize, usize)> {
    let mut links = Vec::new();
    let mut run_end = 0;
    let mut address_end = None;
    let mut i = 0;
    while i < text.len() {
        if let Some(link_end) = link_end_at(text, i) {
            links.push((i, link_end));
            i = link_end;
            continue;
        }
        if is_local_char(text[i]) {
            if i >= run_end {
                run_end = i;
                while run_end < text.len() && is_local_char(text[run_end]) {
                    run_end += 1;
                }
                address_end = if text.get(run_end) == Some(&'@') {
                    domain_end_at(text, run_end + 1)
                } else {
                    None
                };
            }
            if let Some(address_end) = address_end {
                links.push((i, address_end));
                i = address_end;
                continue;
            }
        }
        i += 1;
    }
    links
}

fn without_trailing_punctuation(link: &[char]) -> &[char] {
    let mut end = link.len();
    while end > 0 && ".,;:!?'\")]}>".contains(link[end - 1]) {
        end -= 1;
    }
    &link[..end]
}

fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn href_for(link: &str) -> String {
    if ["http://", "https://", "git://", "mailto:"]
        .iter()
        .any(|scheme| starts_with_ignore_ascii_case(link, scheme))
    {
        link.to_string()
    } else if starts_with_ignore_ascii_case(link, "www.") {
        format!("http://{link}")
    } else {
        format!("mailto:{link}")
    }
}

/// Drops the spaces and tabs that end each line.
fn trim_line_ends(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut from = 0;
    let mut blank_start = None;
    for (i, c) in text.char_indices() {
        if c == ' ' || c == '\t' {
            blank_start.get_or_insert(i);
            continue;
        }
        if let Some(blank) = blank_start.take() {
            if jsstr::is_line_terminator(c) {
                kept.push_str(&text[from..blank]);
                from = i;
            }
        }
    }
    kept.push_str(&text[from..blank_start.unwrap_or(text.len())]);
    kept
}

/// Renders a plain-text body part as HTML: paragraphs on blank lines, `<br/>`
/// on single breaks, and URLs, `www.` hosts and e-mail addresses as links.
pub fn text_to_html(text: &str) -> String {
    let text: Vec<char> = text.chars().collect();
    let mut html = String::with_capacity(text.len() + 16);
    let mut last = 0;
    for (start, end) in find_links(&text) {
        let link = without_trailing_punctuation(&text[start..end]);
        if link.is_empty() {
            continue;
        }
        encode_text(&text[last..start], &mut html);
        let plain: String = link.iter().collect();
        html.push_str("<a href=\"");
        html.push_str(&href_for(&plain).replace('"', "&quot;"));
        html.push_str("\">");
        encode_text(link, &mut html);
        html.push_str("</a>");
        last = start + link.len();
    }
    encode_text(&text[last..], &mut html);

    let unix = html.replace("\r\n", "\n");
    let trimmed = trim_line_ends(jsstr::trim(&unix));
    let mut out = String::with_capacity(trimmed.len() + 16);
    out.push_str("<p>");
    // Two or more line breaks start a paragraph; a paragraph's own leading
    // and trailing whitespace goes with them only at the ends of the text.
    let mut paragraphs = String::with_capacity(trimmed.len());
    let mut rest = jsstr::trim(&trimmed);
    while let Some(at) = rest.find("\n\n") {
        paragraphs.push_str(&rest[..at]);
        paragraphs.push_str("</p><p>");
        rest = rest[at..].trim_start_matches('\n');
    }
    paragraphs.push_str(rest);
    out.push_str(&jsstr::trim(&paragraphs).replace('\n', "<br/>"));
    out.push_str("</p>");
    out
}

/// Formats an address list as the HTML spans an embedded message's header
/// block shows.
pub fn addresses_to_html(addresses: &[ParsedAddressValue]) -> String {
    let entries: Vec<String> = addresses
        .iter()
        .map(|entry| {
            let mut html = String::from("<span class=\"mp_address_group\">");
            if !entry.name.is_empty() {
                html.push_str("<span class=\"mp_address_name\">");
                html.push_str(&encode_entities(&entry.name));
                if entry.group.is_some() {
                    html.push_str(": ");
                }
                html.push_str("</span>");
            }
            if let Some(address) = entry.address.as_deref().filter(|address| !address.is_empty()) {
                let address = encode_entities(address);
                let link = format!("<a href=\"mailto:{address}\" class=\"mp_address_email\">{address}</a>");
                if entry.name.is_empty() {
                    html.push_str(&link);
                } else {
                    html.push_str(&format!(" &lt;{link}&gt;"));
                }
            }
            if let Some(group) = &entry.group {
                html.push_str(&addresses_to_html(group));
                html.push(';');
            }
            html.push_str("</span>");
            html
        })
        .collect();
    entries.join(", ")
}

/// A JavaScript `Date`: milliseconds since the epoch, in the range a `Date`
/// holds (±8.64e15), which is wider than what `chrono` represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageDate(i64);

const MS_PER_DAY: i64 = 86_400_000;
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

impl MessageDate {
    /// The largest time value a `Date` holds, in either direction.
    pub const MAX_MILLIS: i64 = 8_640_000_000_000_000;

    /// `new Date(ms)`, or `None` where the date would be invalid.
    pub fn from_millis(ms: i64) -> Option<MessageDate> {
        (ms.abs() <= Self::MAX_MILLIS).then_some(MessageDate(ms))
    }

    /// `new Date()`.
    pub fn now() -> MessageDate {
        MessageDate(js::now_ms())
    }

    /// `date.getTime()`.
    pub fn timestamp_millis(self) -> i64 {
        self.0
    }

    /// Year, month (1-12) and day of the UTC calendar date.
    fn civil(self) -> (i64, i64, i64) {
        // Days since the epoch to a proleptic Gregorian date.
        let z = self.0.div_euclid(MS_PER_DAY) + 719_468;
        let era = z.div_euclid(146_097);
        let day_of_era = z.rem_euclid(146_097);
        let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        };
        (year_of_era + era * 400 + i64::from(month <= 2), month, day)
    }

    /// Hours, minutes, seconds and milliseconds of the UTC time of day.
    fn clock(self) -> (i64, i64, i64, i64) {
        let ms = self.0.rem_euclid(MS_PER_DAY);
        (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
    }

    /// `date.toISOString()`, e.g. `2026-10-27T01:00:00.000Z`.
    pub fn to_iso_string(self) -> String {
        let (year, month, day) = self.civil();
        let (hour, minute, second, ms) = self.clock();
        let year = match year {
            0..=9999 => format!("{year:04}"),
            _ => format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs()),
        };
        format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{ms:03}Z")
    }

    /// `date.toUTCString()`, e.g. `Tue, 27 Oct 2026 01:00:00 GMT`.
    pub fn to_utc_string(self) -> String {
        let (year, month, day) = self.civil();
        let (hour, minute, second, _) = self.clock();
        let weekday = WEEKDAYS[self.0.div_euclid(MS_PER_DAY).rem_euclid(7) as usize];
        format!(
            "{weekday}, {day:02} {} {}{:04} {hour:02}:{minute:02}:{second:02} GMT",
            MONTHS[month as usize - 1],
            if year < 0 { "-" } else { "" },
            year.abs()
        )
    }
}

/// The headers an embedded message (`message/rfc822` shown inline)
/// contributes to the body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmbeddedHeaders {
    pub from: Option<ParsedAddress>,
    pub subject: Option<String>,
    pub date: Option<MessageDate>,
    pub to: Option<OneOrMany<ParsedAddress>>,
    pub cc: Option<OneOrMany<ParsedAddress>>,
    pub bcc: Option<OneOrMany<ParsedAddress>>,
}

enum MetaValue<'a> {
    Address(&'a ParsedAddress),
    Text(&'a str),
    Date(MessageDate),
}

/// The last occurrence of a recipient header.
fn recipients(list: &Option<OneOrMany<ParsedAddress>>) -> Option<&ParsedAddress> {
    list.as_ref().and_then(OneOrMany::last)
}

/// The headers that are present, in display order. A repeated recipient
/// header shows its last occurrence.
fn meta_entries(headers: &EmbeddedHeaders) -> Vec<(&'static str, MetaValue<'_>)> {
    let entries = [
        ("From", headers.from.as_ref().map(MetaValue::Address)),
        (
            "Subject",
            headers
                .subject
                .as_deref()
                .filter(|subject| !subject.is_empty())
                .map(MetaValue::Text),
        ),
        ("Date", headers.date.map(MetaValue::Date)),
        ("To", recipients(&headers.to).map(MetaValue::Address)),
        ("Cc", recipients(&headers.cc).map(MetaValue::Address)),
        ("Bcc", recipients(&headers.bcc).map(MetaValue::Address)),
    ];
    entries
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?)))
        .collect()
}

/// An embedded message's headers as the lines put before its text body.
pub fn embedded_headers_as_text(headers: &EmbeddedHeaders) -> String {
    let lines: Vec<String> = meta_entries(headers)
        .into_iter()
        .map(|(key, value)| match value {
            MetaValue::Date(date) => format!("{key}: {}", date.to_utc_string()),
            MetaValue::Text(text) => format!("{key}: {text}"),
            MetaValue::Address(address) => format!("{key}: {}", address.text),
        })
        .collect();
    format!("\n{}\n", lines.join("\n"))
}

/// An embedded message's headers as the table put before its HTML body.
pub fn embedded_headers_as_html(headers: &EmbeddedHeaders) -> String {
    let rows: Vec<String> = meta_entries(headers)
        .into_iter()
        .map(|(key, value)| {
            let html = match value {
                MetaValue::Date(date) => date.to_utc_string(),
                MetaValue::Text(text) if key == "Subject" => format!("<strong>{}</strong>", encode_entities(text)),
                MetaValue::Text(text) => encode_entities(text),
                MetaValue::Address(address) => addresses_to_html(&address.value),
            };
            format!("<tr><td class=\"mp_head_key\">{key}:</td><td class=\"mp_head_value\">{html}<td></tr>")
        })
        .collect();
    format!("<table class=\"mp_head\">{}<table>", rows.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_urls_www_hosts_and_addresses_leaving_trailing_punctuation_outside() {
        assert_eq!(
            text_to_html("see http://a.example/b... and (www.x.org)."),
            "<p>see <a href=\"http://a.example/b\">http://a.example/b</a>... and \
             (<a href=\"http://www.x.org\">www.x.org</a>).</p>"
        );
        assert_eq!(
            text_to_html("mail me at a.b+c@d-e.example, ok"),
            "<p>mail me at <a href=\"mailto:a.b+c@d-e.example\">a.b+c@d-e.example</a>, ok</p>"
        );
        assert_eq!(
            text_to_html("xhttp://no.example a@nodot"),
            "<p>xhttp://no.example a@nodot</p>"
        );
    }

    #[test]
    fn stays_linear_on_2_mib_lines_built_to_make_a_backtracking_pattern_quadratic() {
        let size = 2 * 1024 * 1024;
        let lines = [
            "a".repeat(size),
            format!("http://a/{}x", ".".repeat(size)),
            format!("http://a/x{}", ".".repeat(size)),
            format!("x{}y", " ".repeat(size)),
            format!("{}@{}", "a".repeat(size / 2), "b".repeat(size / 2)),
            ".".repeat(size),
        ];
        for line in lines {
            let started = std::time::Instant::now();
            text_to_html(&line);
            // Linear work here takes tens of milliseconds; quadratic takes hours.
            assert!(started.elapsed().as_millis() < 2000);
        }
    }

    #[test]
    fn formats_dates_the_way_javascript_does() {
        let date = MessageDate::from_millis(1_793_062_800_000).unwrap();
        assert_eq!(date.to_iso_string(), "2026-10-27T01:00:00.000Z");
        assert_eq!(date.to_utc_string(), "Tue, 27 Oct 2026 01:00:00 GMT");
        let epoch = MessageDate::from_millis(-1).unwrap();
        assert_eq!(epoch.to_iso_string(), "1969-12-31T23:59:59.999Z");
        assert_eq!(epoch.to_utc_string(), "Wed, 31 Dec 1969 23:59:59 GMT");
        let far = MessageDate::from_millis(MessageDate::MAX_MILLIS).unwrap();
        assert_eq!(far.to_iso_string(), "+275760-09-13T00:00:00.000Z");
        assert_eq!(far.to_utc_string(), "Sat, 13 Sep 275760 00:00:00 GMT");
        let early = MessageDate::from_millis(-62_198_755_200_000).unwrap();
        assert_eq!(early.to_iso_string(), "-000001-01-01T00:00:00.000Z");
        assert_eq!(early.to_utc_string(), "Fri, 01 Jan -0001 00:00:00 GMT");
        assert_eq!(MessageDate::from_millis(MessageDate::MAX_MILLIS + 1), None);
    }
}
