//! Atoms, strings, literals, numbers and sequence sets (imap/parsers/primitive-parsers.ts).
//!
//! Every parser reads from `context.position` and reports the Bun parser's
//! `{ success, value, error, consumed }`. Positions count UTF-16 code units.

use crate::imap::types::{
    Literal, MessageLiteral, ParseContext, ParseResult, SequenceRange, SequenceSet, MAX_SAFE_INTEGER,
};
use crate::imap::util::JsDate;

const fn unit(byte: u8) -> u16 {
    byte as u16
}

/// The code unit at `context.position`, or `None` at the end of the input.
pub fn peek(context: &ParseContext) -> Option<u16> {
    if context.position < context.length {
        context.input.get(context.position).copied()
    } else {
        None
    }
}

/// Whether the next code unit is the ASCII character `byte`.
pub fn peek_is(context: &ParseContext, byte: u8) -> bool {
    peek(context) == Some(unit(byte))
}

/// Skips spaces. Tabs and line breaks are not white space here.
pub fn skip_whitespace(context: &mut ParseContext) {
    while peek_is(context, b' ') {
        context.position += 1;
    }
}

/// Consumes `expected` if the input continues with it.
pub fn consume(context: &mut ParseContext, expected: &str) -> bool {
    let expected: Vec<u16> = expected.encode_utf16().collect();
    let end = (context.position + expected.len()).min(context.input.len());
    let start = context.position.min(end);
    if context.input[start..end] == expected[..] {
        context.position += expected.len();
        true
    } else {
        false
    }
}

/// What ends an atom, a flag or a list-mailbox alike: SP, parentheses, `{`,
/// the double quote, CR, LF and the control characters.
fn is_common_delimiter(code: u16) -> bool {
    code < 32 || code == unit(b' ') || code == unit(b'(') || code == unit(b')') || code == unit(b'{') || code == unit(b'"')
}

/// Advances while `stops` is false, and returns the text passed over.
fn take_until(context: &mut ParseContext, stops: impl Fn(u16) -> bool) -> (usize, String) {
    let start = context.position;
    while let Some(code) = peek(context) {
        if stops(code) {
            break;
        }
        context.position += 1;
    }
    (start, context.substring(start, context.position))
}

/// Parses an IMAP atom (unquoted string).
///
/// ATOM-CHAR is any CHAR except the atom-specials: `(`, `)`, `{`, SP, CTL,
/// the list wildcards `*` and `%`, and the quoted-specials `"` and `\`.
pub fn parse_atom(context: &mut ParseContext) -> ParseResult<String> {
    let (start, value) = take_until(context, |code| {
        is_common_delimiter(code) || code == unit(b'\\') || code == unit(b'*') || code == unit(b'%')
    });
    if context.position == start {
        return ParseResult::fail("Expected atom");
    }
    ParseResult::ok(value, context.position - start)
}

/// Parses an atom as a date: anything `new Date(atom)` accepts.
pub fn parse_date(context: &mut ParseContext) -> ParseResult<JsDate> {
    let start = context.position;
    let atom = parse_atom(context);
    let Some(text) = atom.value.filter(|_| atom.success) else {
        return ParseResult { success: false, value: None, error: atom.error, consumed: atom.consumed };
    };
    let date = JsDate::parse(&text);
    if !date.is_valid() {
        // Reset the position on failure.
        context.position = start;
        return ParseResult::fail("Invalid date");
    }
    ParseResult::ok(date, atom.consumed)
}

/// Parses an IMAP flag, which may start with a backslash.
pub fn parse_flag(context: &mut ParseContext) -> ParseResult<String> {
    let start = context.position;
    if peek_is(context, b'\\') {
        context.position += 1;
    }
    take_until(context, |code| is_common_delimiter(code) || code == unit(b'*') || code == unit(b'%'));
    if context.position == start {
        return ParseResult::fail("Expected flag");
    }
    ParseResult::ok(context.substring(start, context.position), context.position - start)
}

/// `/^\d+\+?$/`: the size of a literal, `N` or `N+`.
fn is_literal_size(token: &[u16]) -> bool {
    let digits = match token.split_last() {
        Some((last, head)) if *last == unit(b'+') => head,
        _ => token,
    };
    !digits.is_empty() && digits.iter().all(|code| (unit(b'0')..=unit(b'9')).contains(code))
}

/// Parses an RFC 3501 §4.3 literal marker, `{N}` (synchronizing) or RFC 7888
/// `{N+}` (LITERAL+), and takes the payload it stands for.
///
/// `N` counts octets, and `input` is UTF-16, so the payload is not sliced out
/// of `input`: the handler has already taken exactly N bytes off the socket
/// buffer and queued the result on `context.literals`. A marker with no
/// queued payload means the context was built by hand.
fn take_literal(context: &mut ParseContext) -> ParseResult<Literal> {
    let start = context.position;
    if !peek_is(context, b'{') {
        return ParseResult::fail("Expected literal");
    }
    let Some(closing) = context
        .input
        .iter()
        .skip(start + 1)
        .position(|code| *code == unit(b'}'))
        .map(|offset| start + 1 + offset)
    else {
        return ParseResult::fail("Unterminated literal");
    };
    if !is_literal_size(&context.input[start + 1..closing]) {
        return ParseResult::fail("Invalid literal size");
    }
    let Some(literal) = context.literals.as_mut().and_then(|queue| queue.pop_front()) else {
        return ParseResult::fail("Missing literal payload");
    };
    context.position = closing + 1;
    ParseResult::ok(literal, context.position - start)
}

/// Puts a literal of the wrong kind back and rewinds to its marker.
fn put_back(context: &mut ParseContext, start: usize, literal: Literal) {
    context.position = start;
    if let Some(queue) = context.literals.as_mut() {
        queue.push_front(literal);
    }
}

/// A literal argument decoded as text.
pub fn parse_literal(context: &mut ParseContext) -> ParseResult<String> {
    let start = context.position;
    let literal = take_literal(context);
    match literal.value {
        Some(Literal::Text(text)) if literal.success => ParseResult::ok(text, literal.consumed),
        Some(message) if literal.success => {
            put_back(context, start, message);
            ParseResult::fail("Unexpected message literal")
        }
        _ => ParseResult { success: false, value: None, error: literal.error, consumed: 0 },
    }
}

/// An APPEND message literal, which the handler spools to a file rather than decoding.
pub fn parse_message_literal(context: &mut ParseContext) -> ParseResult<MessageLiteral> {
    let start = context.position;
    let literal = take_literal(context);
    match literal.value {
        Some(Literal::Message(message)) if literal.success => ParseResult::ok(message, literal.consumed),
        Some(text) if literal.success => {
            put_back(context, start, text);
            ParseResult::fail("Expected message literal")
        }
        _ => ParseResult { success: false, value: None, error: literal.error, consumed: 0 },
    }
}

/// Parses a string: quoted, literal, or atom.
pub fn parse_string(context: &mut ParseContext) -> ParseResult<String> {
    if peek_is(context, b'"') {
        return parse_quoted_string(context);
    }
    if peek_is(context, b'{') {
        return parse_literal(context);
    }
    parse_atom(context)
}

/// Parses a list-mailbox token (RFC 3501 `list-mailbox`). Like an atom, but
/// the list wildcards `*` and `%` are ordinary characters here, so a pattern
/// such as `INBOX/%` is captured whole. A quoted form is also accepted.
pub fn parse_list_mailbox(context: &mut ParseContext) -> ParseResult<String> {
    if peek_is(context, b'"') {
        return parse_quoted_string(context);
    }
    let (start, value) = take_until(context, |code| is_common_delimiter(code) || code == unit(b'\\'));
    if context.position == start {
        return ParseResult::fail("Expected list mailbox");
    }
    ParseResult::ok(value, context.position - start)
}

/// Parses a quoted string. A backslash takes the next character literally,
/// whatever it is.
pub fn parse_quoted_string(context: &mut ParseContext) -> ParseResult<String> {
    if !consume(context, "\"") {
        return ParseResult::fail("Expected opening quote");
    }
    let start = context.position;
    let mut result: Vec<u16> = Vec::new();
    while let Some(code) = peek(context) {
        if code == unit(b'"') {
            context.position += 1;
            return ParseResult::ok(String::from_utf16_lossy(&result), context.position - start + 1);
        }
        if code == unit(b'\\') && context.position + 1 < context.length {
            context.position += 1;
            if let Some(escaped) = context.input.get(context.position) {
                result.push(*escaped);
            }
        } else {
            result.push(code);
        }
        context.position += 1;
    }
    ParseResult::fail("Unterminated quoted string")
}

/// Parses a sequence set such as `1:5,7,9:*`. `*` is `MAX_SAFE_INTEGER`.
///
/// The set ends at the first thing that is not part of one, so a caller
/// finds out about trailing junk from whatever it parses next.
pub fn parse_sequence_set(context: &mut ParseContext) -> ParseResult<SequenceSet> {
    let mut ranges: Vec<SequenceRange> = Vec::new();

    while context.position < context.length {
        skip_whitespace(context);
        if context.position >= context.length {
            break;
        }

        let start = if peek_is(context, b'*') {
            context.position += 1;
            MAX_SAFE_INTEGER
        } else {
            match parse_number(context).value {
                Some(number) => number,
                None => break,
            }
        };

        if peek_is(context, b':') {
            context.position += 1;
            let end = if peek_is(context, b'*') {
                context.position += 1;
                MAX_SAFE_INTEGER
            } else {
                match parse_number(context).value {
                    Some(number) => number,
                    None => return ParseResult::fail("Invalid range end"),
                }
            };
            ranges.push(SequenceRange::range(start, end));
        } else {
            ranges.push(SequenceRange::single(start));
        }

        skip_whitespace(context);
        if peek_is(context, b',') {
            context.position += 1;
        } else {
            break;
        }
    }

    if ranges.is_empty() {
        return ParseResult::fail("Empty sequence set");
    }
    // Reports the position, not the length of the set.
    ParseResult::ok(SequenceSet::new(ranges), context.position)
}

/// Parses a run of ASCII digits as `parseInt` does: to the nearest double,
/// or to infinity for a run too long for one.
pub fn parse_number(context: &mut ParseContext) -> ParseResult<f64> {
    let (start, digits) = take_until(context, |code| !(unit(b'0')..=unit(b'9')).contains(&code));
    if context.position == start {
        return ParseResult::fail("Expected number");
    }
    let value = digits.parse::<f64>().unwrap_or(f64::INFINITY);
    ParseResult::ok(value, context.position - start)
}

/// Parses a CONDSTORE modifier group: the parenthesized `(NAME <mod-sequence>)`
/// that trails a FETCH sequence set and data items (RFC 4551 §3.3.1) or sits
/// between a STORE sequence set and its item name (RFC 7162 §3.1.3). Both
/// grammars admit exactly one modifier name, so an unrecognized one is a
/// client error: dropping it would run an unconditional command where the
/// client asked for a conditional one.
///
/// An absent group is the normal case and parses as success with no value.
///
/// ```ignore
/// let modifier = parse_modifier_group(&mut context, "FETCH", "CHANGEDSINCE");
/// ```
pub fn parse_modifier_group(context: &mut ParseContext, command: &str, modifier: &str) -> ParseResult<f64> {
    skip_whitespace(context);
    if !peek_is(context, b'(') {
        return ParseResult { success: true, value: None, error: None, consumed: context.position };
    }
    context.position += 1;

    let mut value: Option<f64> = None;
    while context.position < context.length {
        skip_whitespace(context);
        if peek_is(context, b')') {
            context.position += 1;
            return ParseResult { success: true, value, error: None, consumed: context.position };
        }

        let Some(name) = parse_atom(context).value else {
            return ParseResult::fail(format!("Invalid {command} modifier"));
        };
        if name.to_uppercase() != modifier {
            return ParseResult::fail(format!("Unknown {command} modifier: {name}"));
        }
        skip_whitespace(context);
        let Some(modseq) = parse_number(context).value else {
            return ParseResult::fail(format!("{modifier} requires a mod-sequence value"));
        };
        value = Some(modseq);
    }

    ParseResult::fail(format!("Unterminated {command} modifier group"))
}
