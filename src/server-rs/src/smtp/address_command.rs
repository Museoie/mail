//! `MAIL FROM:<...>` and `RCPT TO:<...>` (`smtp/address-command.ts`).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use super::types::{SmtpAddress, SmtpArgValue};
use crate::js;

/// RFC 5321 §4.5.3.1.3: a path is at most 256 octets including `<`, `>` and `@`.
const MAX_ADDRESS_LENGTH: usize = 253;

const V4_SEGMENT: &str = "(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])";

/// `net.isIPv4`.
static IPV4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^(?:{V4_SEGMENT}\.){{3}}{V4_SEGMENT}$")).expect("the IPv4 pattern compiles")
});

/// `net.isIPv6`, zone identifier included.
static IPV6: LazyLock<Regex> = LazyLock::new(|| {
    let v4 = format!(r"(?:{V4_SEGMENT}\.){{3}}{V4_SEGMENT}");
    let v6 = "(?:[0-9a-fA-F]{1,4})";
    let pattern = format!(
        "^(?:\
         (?:{v6}:){{7}}(?:{v6}|:)|\
         (?:{v6}:){{6}}(?:{v4}|:{v6}|:)|\
         (?:{v6}:){{5}}(?::{v4}|(?::{v6}){{1,2}}|:)|\
         (?:{v6}:){{4}}(?:(?::{v6}){{0,1}}:{v4}|(?::{v6}){{1,3}}|:)|\
         (?:{v6}:){{3}}(?:(?::{v6}){{0,2}}:{v4}|(?::{v6}){{1,4}}|:)|\
         (?:{v6}:){{2}}(?:(?::{v6}){{0,3}}:{v4}|(?::{v6}){{1,5}}|:)|\
         (?:{v6}:){{1}}(?:(?::{v6}){{0,4}}:{v4}|(?::{v6}){{1,6}}|:)|\
         (?::(?:(?::{v6}){{0,5}}:{v4}|(?::{v6}){{1,7}}|:))\
         )(?:%[0-9a-zA-Z.:-]{{1,}})?$"
    );
    Regex::new(&pattern).expect("the IPv6 pattern compiles")
});

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `text.trim().split(/\s+/)`: the words of a line, by JavaScript's notion
/// of whitespace. A blank line is one empty word, as it is there.
pub(crate) fn split_whitespace(text: &str) -> Vec<&str> {
    let trimmed = js::trim(text);
    if trimmed.is_empty() {
        return vec![""];
    }
    trimmed.split(is_js_whitespace).filter(|word| !word.is_empty()).collect()
}

fn is_valid_domain(domain: &str) -> bool {
    if domain.starts_with('[') && domain.ends_with(']') && domain.len() >= 2 {
        let literal = &domain[1..domain.len() - 1];
        return match literal.get(..5) {
            Some(prefix) if prefix.eq_ignore_ascii_case("ipv6:") => IPV6.is_match(&literal[5..]),
            _ => IPV4.is_match(literal),
        };
    }
    !(domain.starts_with('.')
        || domain.ends_with('.')
        || domain.contains("..")
        || domain.contains(".-")
        || domain.contains("-.")
        || domain.is_empty()
        || !domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c as u32 >= 0x80))
}

/// `url.domainToUnicode` for one label: its Unicode form, or nothing when
/// the label is not valid Punycode.
fn label_to_unicode(label: &str) -> Option<String> {
    let (unicode, result) = idna::domain_to_unicode(label);
    (result.is_ok() && !unicode.is_empty()).then_some(unicode)
}

fn punycode_to_unicode(domain: &str) -> String {
    domain
        .split('.')
        .map(|label| match label.starts_with("xn--") {
            true => label_to_unicode(label).unwrap_or_else(|| label.to_string()),
            false => label.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// `value.replace(/\+([0-9A-F]{2})/g, ...)`: the xtext escapes of RFC 3461,
/// each standing for the character with that code.
fn decode_xtext(value: &str) -> String {
    let bytes = value.as_bytes();
    let hex = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(plus) = rest.find('+') {
        let at = value.len() - rest.len() + plus;
        let escape = match (bytes.get(at + 1).copied().and_then(hex), bytes.get(at + 2).copied().and_then(hex)) {
            (Some(high), Some(low)) => Some(high << 4 | low),
            _ => None,
        };
        out.push_str(&rest[..plus]);
        match escape {
            Some(code) => {
                out.push(char::from(code));
                rest = &rest[plus + 3..];
            }
            None => {
                out.push('+');
                rest = &rest[plus + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parses the argument of `MAIL FROM:<...>` or `RCPT TO:<...>` (`verb` names
/// which), with any `KEY=value` ESMTP parameters. `None` for a line that is
/// not that verb or whose address is malformed; `<>` parses to an empty
/// address.
///
/// ```ignore
/// parse_address_command("mail from", "MAIL FROM:<a@example.com> SIZE=1024")
/// // => SmtpAddress { address: "a@example.com", args: Some({ "SIZE": Value("1024") }) }
/// ```
pub fn parse_address_command(verb: &str, line: &str) -> Option<SmtpAddress> {
    let colon = line.find(':');
    let name = js::trim(colon.map_or(line, |at| &line[..at])).to_uppercase();
    if name != verb.to_uppercase() {
        return None;
    }

    let mut parts = split_whitespace(colon.map_or("", |at| &line[at + 1..])).into_iter();
    let bracketed = parts.next().unwrap_or("");
    let inner = bracketed.strip_prefix('<')?.strip_suffix('>')?;
    if inner.contains(['<', '>']) {
        return None;
    }
    let mut address = inner.to_string();

    let mut args: Option<BTreeMap<String, SmtpArgValue>> = None;
    for part in parts {
        let (raw_key, value) = match part.split_once('=') {
            Some((key, value)) => (key, value),
            None => (part, ""),
        };
        let key = raw_key.to_uppercase();
        if js::trim(&key).is_empty() {
            continue;
        }
        let value = match value.is_empty() {
            true => SmtpArgValue::Flag,
            false => SmtpArgValue::Value(decode_xtext(value)),
        };
        args.get_or_insert_with(BTreeMap::new).insert(key, value);
    }

    if !address.is_empty() {
        let pieces: Vec<&str> = address.split('@').collect();
        let [local, domain] = pieces[..] else {
            return None;
        };
        if local.is_empty() || domain.is_empty() {
            return None;
        }
        if js::utf16_len(local) + 1 + js::utf16_len(domain) > MAX_ADDRESS_LENGTH {
            return None;
        }
        if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
            return None;
        }
        if !is_valid_domain(domain) {
            return None;
        }
        let domain = match domain.starts_with('[') {
            true => domain.to_string(),
            false => punycode_to_unicode(domain),
        };
        address = format!("{local}@{domain}");
    }

    Some(SmtpAddress { address, args })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> SmtpArgValue {
        SmtpArgValue::Value(text.to_string())
    }

    #[test]
    fn reads_the_address_and_its_parameters() {
        let parsed = parse_address_command("mail from", "MAIL FROM:<a@example.com> SIZE=1024").unwrap();
        assert_eq!(parsed.address, "a@example.com");
        assert_eq!(parsed.args, Some(BTreeMap::from([("SIZE".to_string(), value("1024"))])));

        let parsed = parse_address_command("rcpt to", "rcpt to: <B@Example.com>").unwrap();
        assert_eq!(parsed, SmtpAddress { address: "B@Example.com".into(), args: None });

        let parsed = parse_address_command("mail from", "MAIL FROM:<> body=8BITMIME smtputf8 x=a=b").unwrap();
        assert_eq!(parsed.address, "");
        assert_eq!(
            parsed.args,
            Some(BTreeMap::from([
                ("BODY".to_string(), value("8BITMIME")),
                ("SMTPUTF8".to_string(), SmtpArgValue::Flag),
                ("X".to_string(), value("a=b")),
            ]))
        );
    }

    #[test]
    fn refuses_another_verb_and_malformed_addresses() {
        for line in [
            "RCPT TO:<a@example.com>",
            "MAIL FROM:a@example.com",
            "MAIL FROM",
            "MAIL  FROM:<a@example.com>",
            "MAIL FROM:<a@b@example.com>",
            "MAIL FROM:<@example.com>",
            "MAIL FROM:<a@>",
            "MAIL FROM:<a>",
            "MAIL FROM:<.a@example.com>",
            "MAIL FROM:<a.@example.com>",
            "MAIL FROM:<a..b@example.com>",
            "MAIL FROM:<a@.example.com>",
            "MAIL FROM:<a@example.com.>",
            "MAIL FROM:<a@exa_mple.com>",
            "MAIL FROM:<a@-.com>",
            "MAIL FROM:<a@[1.2.3]>",
            "MAIL FROM:<a@[IPv6:1::2::3]>",
            "MAIL FROM:<<a@example.com>>",
            "MAIL FROM:<a@example.com>SIZE=1",
        ] {
            assert_eq!(parse_address_command("mail from", line), None, "{line}");
        }
    }

    #[test]
    fn accepts_address_literals_and_decodes_punycode() {
        let address = |line: &str| parse_address_command("mail from", line).map(|parsed| parsed.address);
        assert_eq!(address("MAIL FROM:<a@[192.0.2.1]>").as_deref(), Some("a@[192.0.2.1]"));
        assert_eq!(address("MAIL FROM:<a@[IPv6:2001:db8::1]>").as_deref(), Some("a@[IPv6:2001:db8::1]"));
        assert_eq!(address("MAIL FROM:<a@[ipv6:fe80::1%eth0]>").as_deref(), Some("a@[ipv6:fe80::1%eth0]"));
        assert_eq!(address("MAIL FROM:<a@xn--mnchen-3ya.de>").as_deref(), Some("a@münchen.de"));
        assert_eq!(address("MAIL FROM:<a@xn--a.de>").as_deref(), Some("a@xn--a.de"));
        assert_eq!(address("MAIL FROM:<a@XN--MNCHEN-3YA.de>").as_deref(), Some("a@XN--MNCHEN-3YA.de"));
        assert_eq!(address("MAIL FROM:<δοκιμή@παράδειγμα.δοκιμή>").as_deref(), Some("δοκιμή@παράδειγμα.δοκιμή"));
    }

    #[test]
    fn measures_the_address_in_utf16_units() {
        let local = "a".repeat(64);
        let fits = format!("{}.org", "d".repeat(253 - 64 - 1 - 4));
        assert!(parse_address_command("mail from", &format!("MAIL FROM:<{local}@{fits}>")).is_some());
        assert!(parse_address_command("mail from", &format!("MAIL FROM:<{local}@d{fits}>")).is_none());
        // An astral character is two units long.
        let astral = format!("{}.org", "😀".repeat((253 - 64 - 1 - 4) / 2));
        assert!(parse_address_command("mail from", &format!("MAIL FROM:<{local}@{astral}>")).is_some());
        assert!(parse_address_command("mail from", &format!("MAIL FROM:<{local}@😀{astral}>")).is_none());
    }

    #[test]
    fn decodes_xtext_escapes_in_parameter_values() {
        assert_eq!(decode_xtext("a+2Bb+3Dc"), "a+b=c");
        assert_eq!(decode_xtext("+E9+00"), "\u{e9}\u{0}");
        assert_eq!(decode_xtext("+2b+G0+2"), "+2b+G0+2");
        assert_eq!(decode_xtext("++41"), "+A");
    }

    #[test]
    fn splits_on_javascript_whitespace() {
        assert_eq!(split_whitespace("  EHLO \t client\u{a0}x  "), vec!["EHLO", "client", "x"]);
        assert_eq!(split_whitespace(""), vec![""]);
        assert_eq!(split_whitespace(" \u{feff} "), vec![""]);
        assert_eq!(split_whitespace("a\u{85}b"), vec!["a\u{85}b"]);
    }
}
