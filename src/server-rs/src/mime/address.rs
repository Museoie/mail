//! RFC 5322 address lists (mime/address.ts).

use serde::{Deserialize, Serialize};

use super::charset::jsstr;
use super::header::{decode_words, only_b_encoded_words};

/// One mailbox, or a group with its members.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedAddressValue {
    /// Absent on a group.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub address: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub group: Option<Vec<ParsedAddressValue>>,
}

/// An address header: its entries, and the list written back out as text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedAddress {
    pub value: Vec<ParsedAddressValue>,
    pub text: String,
}

/// A header that may repeat (`To`, `Cc`, `Bcc`): its one occurrence, or all
/// of them in order. Serializes as the value or as a list, like the
/// `ParsedAddress | ParsedAddress[]` it stands for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn as_slice(&self) -> &[T] {
        match self {
            OneOrMany::One(value) => std::slice::from_ref(value),
            OneOrMany::Many(values) => values,
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    pub fn last(&self) -> Option<&T> {
        self.as_slice().last()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Operator { value: char, no_break: bool },
    Text(String),
}

/// The character that closes an opening operator, `None` for a lone delimiter.
fn closing_of(operator: char) -> Option<Option<char>> {
    Some(match operator {
        '"' => Some('"'),
        '(' => Some(')'),
        '<' => Some('>'),
        ':' => Some(';'),
        // Outside a group a semicolon is not valid, but some clients type it
        // as a list separator, so it delimits like a comma.
        ',' | ';' => None,
        _ => return None,
    })
}

/// The first `[start, end)` of `text` matching `\s*\b[^@\s]+@[^\s]+\b\s*` as
/// a regex: an address written inside a display name, with the whitespace
/// around it.
fn find_address_in_text(text: &[char]) -> Option<(usize, usize)> {
    let is_word = |i: usize| text.get(i).copied().is_some_and(jsstr::is_word);
    let is_space = |i: usize| text.get(i).copied().is_some_and(jsstr::is_space);
    let boundary = |i: usize| (i > 0 && is_word(i - 1)) != is_word(i);
    let mut i = 0;
    while i < text.len() {
        if text[i] == '@' || is_space(i) {
            i += 1;
            continue;
        }
        let run_start = i;
        let mut run_end = i;
        while run_end < text.len() && text[run_end] != '@' && !is_space(run_end) {
            run_end += 1;
        }
        i = run_end;
        if text.get(run_end) != Some(&'@') {
            continue;
        }

        let mut local = run_start;
        while local < run_end && !boundary(local) {
            local += 1;
        }
        if local == run_end {
            continue;
        }

        let mut domain_end = run_end + 1;
        while domain_end < text.len() && !is_space(domain_end) {
            domain_end += 1;
        }
        let mut end = domain_end;
        while end > run_end + 1 && !boundary(end) {
            end -= 1;
        }
        if end == run_end + 1 {
            continue;
        }

        let mut start = local;
        if local == run_start {
            while start > 0 && is_space(start - 1) {
                start -= 1;
            }
        }
        if end == domain_end {
            while is_space(end) {
                end += 1;
            }
        }
        return Some((start, end));
    }
    None
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut list: Vec<Token> = Vec::new();
    let mut expecting: Option<char> = None;
    let mut in_text = false;
    let mut escaped = false;

    let mut chars = input.chars().peekable();
    while let Some(chr) = chars.next() {
        if !escaped {
            if expecting == Some(chr) {
                let no_break = chars
                    .peek()
                    .is_some_and(|next| !matches!(next, ' ' | '\t' | '\r' | '\n' | ',' | ';'));
                list.push(Token::Operator { value: chr, no_break });
                in_text = false;
                expecting = None;
                continue;
            }
            if expecting.is_none() {
                if let Some(closing) = closing_of(chr) {
                    list.push(Token::Operator {
                        value: chr,
                        no_break: false,
                    });
                    in_text = false;
                    expecting = closing;
                    continue;
                }
            }
            if expecting == Some('"') && chr == '\\' {
                escaped = true;
                continue;
            }
        }

        if !in_text {
            list.push(Token::Text(String::new()));
            in_text = true;
        }
        let chr = if chr == '\n' { ' ' } else { chr };
        if chr >= '\u{21}' || chr == ' ' || chr == '\t' {
            if let Some(Token::Text(value)) = list.last_mut() {
                value.push(chr);
            }
        }
        escaped = false;
    }

    list.into_iter()
        .filter_map(|token| match token {
            Token::Text(value) => {
                let trimmed = jsstr::trim(&value);
                (!trimmed.is_empty()).then(|| Token::Text(trimmed.to_string()))
            }
            operator => Some(operator),
        })
        .collect()
}

const MAX_GROUP_DEPTH: usize = 50;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Text,
    Address,
    Comment,
    Group,
}

/// `^[^@\s]+@[^@\s]+$`
fn is_bare_address(text: &str) -> bool {
    let mut parts = text.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && !text.contains(jsstr::is_space)
}

fn handle_address(tokens: &[Token], depth: usize) -> Vec<ParsedAddressValue> {
    let mut is_group = false;
    let mut state = State::Text;
    let mut address: Vec<String> = Vec::new();
    let mut comment: Vec<String> = Vec::new();
    let mut group: Vec<String> = Vec::new();
    let mut text: Vec<String> = Vec::new();
    let mut text_was_quoted: Vec<bool> = Vec::new();
    let mut inside_quotes = false;
    let mut previous_no_break = false;

    for token in tokens {
        let value = match token {
            Token::Operator { value, no_break } => {
                inside_quotes = *value == '"' && !inside_quotes;
                state = match value {
                    '<' => State::Address,
                    '(' => State::Comment,
                    ':' => {
                        is_group = true;
                        State::Group
                    }
                    _ => State::Text,
                };
                previous_no_break = *no_break;
                continue;
            }
            Token::Text(value) => value.as_str(),
        };
        // An unquoted name holding a stray "<": everything up to it is dropped.
        let value = match (state, value.split_once('<')) {
            (State::Address, Some((_, rest))) => jsstr::trim_start(rest),
            _ => value,
        };
        let bucket = match state {
            State::Text => &mut text,
            State::Address => &mut address,
            State::Comment => &mut comment,
            State::Group => &mut group,
        };
        match bucket.last_mut() {
            Some(last) if previous_no_break => {
                last.push_str(value);
                if state == State::Text && inside_quotes {
                    if let Some(quoted) = text_was_quoted.last_mut() {
                        *quoted = true;
                    }
                }
            }
            _ => {
                bucket.push(value.to_string());
                if state == State::Text {
                    text_was_quoted.push(inside_quotes);
                }
            }
        }
        previous_no_break = false;
    }

    if text.is_empty() && !comment.is_empty() {
        text = std::mem::take(&mut comment);
    }

    if is_group {
        let mut members = Vec::new();
        if !group.is_empty() {
            // Groups cannot nest; a nested one contributes its members directly.
            for member in parse_address_values(&group.join(","), depth + 1) {
                match member.group {
                    Some(nested) => members.extend(nested),
                    None => members.push(member),
                }
            }
        }
        return vec![ParsedAddressValue {
            address: None,
            name: text.join(" "),
            group: Some(members),
        }];
    }

    if address.is_empty() && !text.is_empty() {
        // An address quoted as a display name is never promoted to the
        // address: `"user@evil"@example.com` must not route to `user@evil`.
        let quoted = |i: usize| text_was_quoted.get(i).copied().unwrap_or(false);
        if let Some(i) = (0..text.len()).rev().find(|&i| !quoted(i) && is_bare_address(&text[i])) {
            address = vec![text.remove(i)];
            if i < text_was_quoted.len() {
                text_was_quoted.remove(i);
            }
        } else {
            for i in (0..text.len()).rev() {
                if text_was_quoted.get(i).copied().unwrap_or(false) {
                    continue;
                }
                let chars: Vec<char> = text[i].chars().collect();
                let Some((start, end)) = find_address_in_text(&chars) else {
                    text[i] = jsstr::trim(&text[i]).to_string();
                    continue;
                };
                let found: String = chars[start..end].iter().collect();
                let around: String = chars[..start].iter().chain([' '].iter()).chain(&chars[end..]).collect();
                address = vec![jsstr::trim(&found).to_string()];
                text[i] = jsstr::trim(&around).to_string();
                break;
            }
        }
    }

    if text.is_empty() && !comment.is_empty() {
        text = std::mem::take(&mut comment);
    }

    if address.len() > 1 {
        text.extend(address.drain(1..));
    }

    let text = text.join(" ");
    let address = address.join(" ");
    let mut entry_address = if !address.is_empty() {
        address.clone()
    } else {
        text.clone()
    };
    let mut entry_name = if !text.is_empty() { text } else { address };
    if entry_address == entry_name {
        if entry_address.contains('@') {
            entry_name = String::new();
        } else {
            entry_address = String::new();
        }
    }
    vec![ParsedAddressValue {
        address: Some(entry_address),
        name: entry_name,
        group: None,
    }]
}

fn parse_address_values(input: &str, depth: usize) -> Vec<ParsedAddressValue> {
    if depth > MAX_GROUP_DEPTH {
        return Vec::new();
    }
    let tokens = tokenize(input);
    tokens
        .split(|token| matches!(token, Token::Operator { value: ',' | ';', .. }))
        .filter(|entry| !entry.is_empty())
        .flat_map(|entry| handle_address(entry, depth))
        .collect()
}

/// Node's `url.domainToUnicode`: the name goes through the URL host parser
/// (the host ends at `/`, `?`, `#` or `\`, a port is refused, percent
/// escapes are decoded, IDNA applies) and comes back in Unicode, or empty
/// when it is not a valid host.
fn domain_to_unicode(domain: &str) -> String {
    let mut host = String::with_capacity(domain.len());
    let mut inside_brackets = false;
    for c in domain.chars() {
        match c {
            '\t' | '\n' | '\r' => continue,
            '/' | '?' | '#' | '\\' => break,
            ':' if !inside_brackets => return String::new(),
            '[' => inside_brackets = true,
            ']' => inside_brackets = false,
            _ => {}
        }
        host.push(c);
    }
    if host.is_empty() {
        return String::new();
    }
    match url::Host::parse(&host) {
        Ok(url::Host::Domain(ascii)) => idna::domain_to_unicode(&ascii).0,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

fn punycode_domain_to_unicode(address: &str) -> String {
    let at = address.rfind('@').map_or(0, |at| at + 1);
    let unicode: Vec<String> = address[at..]
        .split('.')
        .map(|label| {
            let decoded = if label.starts_with("xn--") {
                domain_to_unicode(label)
            } else {
                String::new()
            };
            if decoded.is_empty() {
                label.to_string()
            } else {
                decoded
            }
        })
        .collect();
    format!("{}{}", &address[..at], unicode.join("."))
}

fn decode_addresses(addresses: &mut Vec<ParsedAddressValue>) {
    let mut expanded = vec![false; addresses.len()];
    let mut i = 0;
    while i < addresses.len() {
        let name = jsstr::trim(&addresses[i].name).to_string();
        let no_address = addresses[i].address.as_deref().unwrap_or("").is_empty();

        // A display name made only of B-encoded words, with no address
        // beside it, is a whole address list someone encoded; it is decoded
        // and re-parsed.
        if no_address && only_b_encoded_words(&name) && !expanded[i] {
            let reparsed = parse_address_values(&decode_words(&name), 0);
            expanded.extend(std::iter::repeat_n(true, reparsed.len()));
            addresses.extend(reparsed);
            addresses.remove(i);
            expanded.remove(i);
            continue;
        }

        let entry = &mut addresses[i];
        entry.name = if name.is_empty() { name } else { decode_words(&name) };
        if let Some(address) = entry.address.as_mut().filter(|address| address.contains("@xn--")) {
            *address = punycode_domain_to_unicode(address);
        }
        if let Some(group) = entry.group.as_mut() {
            decode_addresses(group);
        }
        i += 1;
    }
}

fn format_text(addresses: &[ParsedAddressValue]) -> String {
    let entries: Vec<String> = addresses
        .iter()
        .map(|entry| {
            let mut text = String::new();
            let address = entry.address.as_deref().unwrap_or("");
            if !entry.name.is_empty() {
                text.push('"');
                text.push_str(&entry.name);
                text.push('"');
                if entry.group.is_some() {
                    text.push_str(": ");
                }
            }
            if !address.is_empty() {
                if entry.name.is_empty() {
                    text.push_str(address);
                } else {
                    text.push_str(&format!(" <{address}>"));
                }
            }
            if let Some(group) = &entry.group {
                text.push_str(&format_text(group));
                text.push(';');
            }
            text
        })
        .collect();
    entries.join(", ")
}

/// Parses an unfolded address header value (`From`, `To`, `Cc`, ...) into
/// its entries, with display names decoded from RFC 2047 encoded words, IDN
/// domains shown in Unicode, and RFC 5322 groups kept as `group` members.
/// `text` is the list written back out as `"Name" <address>`.
pub fn parse_address_header(value: &str) -> ParsedAddress {
    let mut entries = parse_address_values(value, 0);
    decode_addresses(&mut entries);
    let text = format_text(&entries);
    ParsedAddress { value: entries, text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(address: &str, name: &str) -> ParsedAddressValue {
        ParsedAddressValue {
            address: Some(address.into()),
            name: name.into(),
            group: None,
        }
    }

    #[test]
    fn reads_a_bare_address_and_a_display_name() {
        let parsed = parse_address_header("a@example.com, Bob <bob@example.com>");
        assert_eq!(
            parsed.value,
            [mailbox("a@example.com", ""), mailbox("bob@example.com", "Bob")]
        );
        assert_eq!(parsed.text, "a@example.com, \"Bob\" <bob@example.com>");
    }

    #[test]
    fn keeps_a_comma_inside_a_quoted_display_name() {
        let parsed = parse_address_header("\"Doe, John\" <john@example.org>");
        assert_eq!(parsed.value, [mailbox("john@example.org", "Doe, John")]);
    }

    #[test]
    fn decodes_encoded_word_display_names() {
        let parsed = parse_address_header("=?UTF-8?B?7ZmN6ri464+Z?= <hong@example.kr>");
        assert_eq!(parsed.value, [mailbox("hong@example.kr", "홍길동")]);
    }

    #[test]
    fn nests_rfc_5322_group_members_under_the_group() {
        let parsed = parse_address_header("Team: alice@example.org, \"Bob B\" <bob@example.org>;");
        assert_eq!(
            parsed.value,
            [ParsedAddressValue {
                address: None,
                name: "Team".into(),
                group: Some(vec![
                    mailbox("alice@example.org", ""),
                    mailbox("bob@example.org", "Bob B")
                ]),
            }]
        );
        assert_eq!(parsed.text, "\"Team\": alice@example.org, \"Bob B\" <bob@example.org>;");
        assert_eq!(
            serde_json::to_string(&parsed.value).unwrap(),
            r#"[{"name":"Team","group":[{"address":"alice@example.org","name":""},{"address":"bob@example.org","name":"Bob B"}]}]"#
        );
    }

    #[test]
    fn shows_an_idn_domain_in_unicode() {
        let parsed = parse_address_header("x@xn--9n2bp8q.com");
        assert_eq!(parsed.value[0].address.as_deref(), Some("x@실례.com"));
    }

    #[test]
    fn reparses_a_display_name_that_is_a_whole_encoded_address_list() {
        // "A <a@x.org>, b@y.org" in base64.
        let parsed = parse_address_header("=?UTF-8?B?QSA8YUB4Lm9yZz4sIGJAeS5vcmc=?=");
        assert_eq!(parsed.value, [mailbox("a@x.org", "A"), mailbox("b@y.org", "")]);
    }

    #[test]
    fn never_promotes_a_quoted_display_name_over_a_real_address() {
        let parsed = parse_address_header("\"user@evil.example\" other@example.com");
        assert_eq!(parsed.value, [mailbox("other@example.com", "user@evil.example")]);
    }

    #[test]
    fn reads_a_comment_as_the_display_name_when_there_is_no_other() {
        let parsed = parse_address_header("a@b.c (Alice)");
        assert_eq!(parsed.value, [mailbox("a@b.c", "Alice")]);
    }

    #[test]
    fn keeps_an_empty_group() {
        let parsed = parse_address_header("undisclosed-recipients:;");
        assert_eq!(
            parsed.value,
            [ParsedAddressValue {
                address: None,
                name: "undisclosed-recipients".into(),
                group: Some(vec![])
            }]
        );
    }

    #[test]
    fn treats_a_semicolon_outside_a_group_as_a_separator() {
        let parsed = parse_address_header("a@x.org; b@x.org");
        let addresses: Vec<_> = parsed.value.iter().map(|entry| entry.address.as_deref()).collect();
        assert_eq!(addresses, [Some("a@x.org"), Some("b@x.org")]);
    }
}
