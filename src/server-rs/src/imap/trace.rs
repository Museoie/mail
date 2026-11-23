//! The IMAP wire trace and its credential redaction (imap/trace.ts).
//!
//! `IMAP_TRACE=1` journals the command lines a session receives and the
//! framing lines it sends. Mail content never reaches the journal: inbound
//! lines must look like `<tag> <VERB>`, outbound lines must be on a short
//! allowlist, and credentials are scrubbed.

use std::sync::OnceLock;

use serde_json::json;

use super::util::js_is_whitespace;
use crate::{env, logger};

const LINE_CAP: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// Whether the wire trace is on. Read once, like the Bun server reads it when
/// the module loads, and announced in the log the first time it is found on.
pub fn trace_enabled() -> bool {
    static TRACE_ON: OnceLock<bool> = OnceLock::new();
    *TRACE_ON.get_or_init(|| {
        let on = env::var("IMAP_TRACE").as_deref() == Some("1");
        if on {
            logger::info("IMAP wire trace ENABLED via IMAP_TRACE=1", json!({ "component": "imap.trace" }));
        }
        on
    })
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The rest of `text` after its leading run of characters matching `matches`.
fn skip_while(text: &str, matches: impl Fn(char) -> bool) -> &str {
    text.trim_start_matches(matches)
}

fn strip_prefix_ignore_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

/// `/^[A-Za-z0-9]+\s+[A-Z]+\b/`: the inbound allow-shape. Requiring
/// `<tag> <VERB>` at the start keeps every non-command line (junk, empty,
/// literal payloads) out of the journal.
fn is_inbound_command(line: &str) -> bool {
    let after_tag = skip_while(line, |c| c.is_ascii_alphanumeric());
    if after_tag.len() == line.len() {
        return false;
    }
    let after_space = skip_while(after_tag, js_is_whitespace);
    if after_space.len() == after_tag.len() {
        return false;
    }
    let after_verb = skip_while(after_space, |c| c.is_ascii_uppercase());
    if after_verb.len() == after_space.len() {
        return false;
    }
    !after_verb.chars().next().is_some_and(is_word_char)
}

/// The outbound allowlist: IMAP framing and status lines only. Tag
/// completions (OK/NO/BAD), untagged server status, sequence updates without
/// a data tuple (EXISTS / RECENT / EXPUNGE), and the `+` continuation prompt.
/// FETCH data, which carries real mail headers, matches none of these.
fn is_outbound_framing(line: &str) -> bool {
    // /^\+ /
    if line.starts_with("+ ") {
        return true;
    }
    if let Some(rest) = line.strip_prefix("* ") {
        // /^\* (?:OK|BAD|NO|BYE|CAPABILITY|ENABLED|PREAUTH)\b/
        let status = ["OK", "BAD", "NO", "BYE", "CAPABILITY", "ENABLED", "PREAUTH"]
            .iter()
            .filter_map(|word| rest.strip_prefix(word))
            .any(|after| !after.chars().next().is_some_and(is_word_char));
        if status {
            return true;
        }
        // /^\* \d+ (?:EXISTS|RECENT|EXPUNGE)\s*$/
        let after_number = skip_while(rest, |c| c.is_ascii_digit());
        if after_number.len() != rest.len() {
            if let Some(after_space) = after_number.strip_prefix(' ') {
                let update = ["EXISTS", "RECENT", "EXPUNGE"]
                    .iter()
                    .filter_map(|word| after_space.strip_prefix(word))
                    .any(|after| after.chars().all(js_is_whitespace));
                if update {
                    return true;
                }
            }
        }
    }
    // /^[A-Za-z0-9]+ (?:OK|BAD|NO) /
    let after_tag = skip_while(line, |c| c.is_ascii_alphanumeric());
    if after_tag.len() != line.len() {
        if let Some(after_space) = after_tag.strip_prefix(' ') {
            return ["OK ", "BAD ", "NO "].iter().any(|word| after_space.starts_with(word));
        }
    }
    false
}

/// Finds `<verb> <white space>` where a command's verb can stand: after
/// optional white space and one optional token (the tag). Returns the byte
/// offset just past the white space that follows the verb, trying the tagged
/// reading first, as the regular expressions do.
fn after_verb<'a>(line: &'a str, verb: &str, then: impl Fn(&'a str) -> Option<usize>) -> Option<usize> {
    let offset_of = |rest: &str| line.len() - rest.len();
    let start = skip_while(line, js_is_whitespace);

    let tagged = {
        let after_token = skip_while(start, |c| !js_is_whitespace(c));
        let after_space = skip_while(after_token, js_is_whitespace);
        (after_token.len() != start.len() && after_space.len() != after_token.len()).then_some(after_space)
    };
    for candidate in [tagged, Some(start)].into_iter().flatten() {
        let Some(after) = strip_prefix_ignore_ascii_case(candidate, verb) else {
            continue;
        };
        let arguments = skip_while(after, js_is_whitespace);
        if arguments.len() == after.len() {
            continue;
        }
        if let Some(extra) = then(arguments) {
            return Some(offset_of(arguments) + extra);
        }
    }
    None
}

/// Scrubs the credentials out of a command line.
///
/// LOGIN carries the plaintext password; AUTHENTICATE carries the SASL
/// initial response (RFC 4959), and base64-decoded PLAIN is
/// `\0user\0password`. Everything after `LOGIN ` and everything after the
/// AUTHENTICATE mechanism becomes `[REDACTED]`.
///
/// The tag is optional and leading white space is skipped: an untagged
/// `LOGIN admin hunter2` still reaches the parse-failure log, and a redactor
/// that only covers well-formed input is not a redactor.
pub fn redact_credentials(line: &str) -> String {
    // /^(\s*(?:\S+\s+)?LOGIN\s+)[\s\S]*$/i  ->  "$1[REDACTED]"
    let line = match after_verb(line, "LOGIN", |_| Some(0)) {
        Some(end) => format!("{}[REDACTED]", &line[..end]),
        None => line.to_string(),
    };
    // /^(\s*(?:\S+\s+)?AUTHENTICATE\s+\S+)\s+[\s\S]*$/i  ->  "$1 [REDACTED]"
    let mechanism_end = after_verb(&line, "AUTHENTICATE", |arguments| {
        let after_mechanism = skip_while(arguments, |c| !js_is_whitespace(c));
        // The mechanism has to be followed by white space: by a response.
        after_mechanism
            .chars()
            .next()
            .is_some_and(js_is_whitespace)
            .then_some(arguments.len() - after_mechanism.len())
    });
    match mechanism_end {
        Some(end) => format!("{} [REDACTED]", &line[..end]),
        None => line,
    }
}

/// Clips a line to `LINE_CAP` octets of UTF-8, on a character boundary, and
/// marks how many octets were dropped: `…[+N]`.
///
/// Measured in octets, not characters: the wire is decoded as UTF-8, so every
/// octet that is not valid UTF-8 arrives as one U+FFFD and leaves as three
/// octets again.
pub fn clip(line: &str) -> String {
    let octets = line.len();
    if octets <= LINE_CAP {
        return line.to_string();
    }
    let mut kept = 0;
    for c in line.chars() {
        if kept + c.len_utf8() > LINE_CAP {
            break;
        }
        kept += c.len_utf8();
    }
    format!("{}…[+{}]", &line[..kept], octets - kept)
}

/// The lines of `data` the trace records, redacted and clipped.
pub fn trace_lines(direction: Direction, data: &str) -> Vec<String> {
    data.split("\r\n")
        .filter(|raw| !raw.is_empty())
        .filter(|raw| match direction {
            Direction::In => is_inbound_command(raw),
            Direction::Out => is_outbound_framing(raw),
        })
        .map(|raw| match direction {
            Direction::In => clip(&redact_credentials(raw)),
            Direction::Out => clip(raw),
        })
        .collect()
}

/// Journals the traceable lines of `data` when the wire trace is on.
pub fn imap_trace(direction: Direction, session_id: &str, data: &str) {
    if !trace_enabled() {
        return;
    }
    for line in trace_lines(direction, data) {
        logger::info(
            format!("IMAP wire {}", direction.as_str()),
            json!({ "component": "imap.trace", "sessionId": session_id, "line": line }),
        );
    }
}
