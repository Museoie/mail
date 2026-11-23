//! FETCH data items (imap/parsers/fetch-parsers.ts).

use super::primitive_parsers::{parse_atom, parse_modifier_group, parse_sequence_set, peek, peek_is, skip_whitespace};
use crate::imap::types::{
    BodyFetch, BodySection, FetchDataItem, FetchRequest, ImapRequest, MimeSubSection, ParseContext, ParseResult,
    PartialRange,
};
use crate::imap::util::{js_is_whitespace, js_trim};
use crate::js::utf16_slice;

/// Parses FETCH: a sequence set, the data items, and an optional trailing
/// `(CHANGEDSINCE <modseq>)` group (RFC 4551 §3.3.1).
pub fn parse_fetch(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    let Some(sequence_set) = parse_sequence_set(context).into_result().ok() else {
        return ParseResult::fail("Invalid sequence set in FETCH");
    };

    skip_whitespace(context);

    let Some(data_items) = parse_fetch_data_items(context).into_result().ok() else {
        return ParseResult::fail("Invalid data items in FETCH");
    };

    let changed_since = parse_modifier_group(context, "FETCH", "CHANGEDSINCE");
    if !changed_since.success {
        return ParseResult { success: false, value: None, error: changed_since.error, consumed: 0 };
    }

    ParseResult::ok(
        ImapRequest::Fetch(FetchRequest { sequence_set, data_items, changed_since: changed_since.value }),
        context.position,
    )
}

/// Expands a bare FETCH macro name (RFC 3501 §6.4.5) into its data items.
/// A macro only stands for the whole data-item spec, never inside a list.
fn expand_fetch_macro(name: &str) -> Option<Vec<FetchDataItem>> {
    let fast = vec![FetchDataItem::Flags, FetchDataItem::InternalDate, FetchDataItem::Rfc822Size];
    match name {
        // ALL = (FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)
        "ALL" => Some([fast, vec![FetchDataItem::Envelope]].concat()),
        // FAST = (FLAGS INTERNALDATE RFC822.SIZE)
        "FAST" => Some(fast),
        // FULL = (FLAGS INTERNALDATE RFC822.SIZE ENVELOPE BODY). That BODY is
        // the bare data item, the non-extensible form of BODYSTRUCTURE, not
        // BODY[]: `FETCH 1:* FULL` must not stream every message.
        "FULL" => Some(
            [fast, vec![FetchDataItem::Envelope, FetchDataItem::BodyStructure { extensible: false }]].concat(),
        ),
        _ => None,
    }
}

/// Parses FETCH data items: a macro, a parenthesized list, or a single item.
///
/// A list may run to the end of the line without its closing parenthesis.
pub fn parse_fetch_data_items(context: &mut ParseContext) -> ParseResult<Vec<FetchDataItem>> {
    let mut items: Vec<FetchDataItem> = Vec::new();

    skip_whitespace(context);

    // A bare macro (ALL / FAST / FULL) stands in for the whole spec.
    if !peek_is(context, b'(') {
        let macro_start = context.position;
        if let Ok(atom) = parse_atom(context).into_result() {
            if let Some(expanded) = expand_fetch_macro(&atom.to_uppercase()) {
                return ParseResult::ok(expanded, context.position);
            }
        }
        // Not a macro: rewind and parse a single item below.
        context.position = macro_start;
    }

    if peek_is(context, b'(') {
        context.position += 1;
        while context.position < context.length {
            skip_whitespace(context);
            if peek_is(context, b')') {
                context.position += 1;
                break;
            }
            let Some(item) = parse_fetch_data_item(context).into_result().ok() else {
                return ParseResult::fail("Invalid fetch data item");
            };
            items.push(item);
            skip_whitespace(context);
        }
    } else {
        let Some(item) = parse_fetch_data_item(context).into_result().ok() else {
            return ParseResult::fail("Invalid fetch data item");
        };
        items.push(item);
    }

    ParseResult::ok(items, context.position)
}

/// Consumes up to and including `terminator`, returning what came before it.
fn take_through(context: &mut ParseContext, terminator: u8) -> String {
    let mut taken: Vec<u16> = Vec::new();
    while let Some(code) = peek(context) {
        context.position += 1;
        if code == terminator as u16 {
            break;
        }
        taken.push(code);
    }
    String::from_utf16_lossy(&taken)
}

/// Parses a single FETCH data item.
pub fn parse_fetch_data_item(context: &mut ParseContext) -> ParseResult<FetchDataItem> {
    let start = context.position;

    let Some(atom) = parse_atom(context).into_result().ok() else {
        return ParseResult::fail("Expected fetch data item");
    };
    let item_name = atom.to_uppercase();

    let simple = match item_name.as_str() {
        "ENVELOPE" => Some(FetchDataItem::Envelope),
        "FLAGS" => Some(FetchDataItem::Flags),
        "INTERNALDATE" => Some(FetchDataItem::InternalDate),
        "RFC822" => Some(FetchDataItem::Rfc822),
        "RFC822.HEADER" => Some(FetchDataItem::Rfc822Header),
        "RFC822.SIZE" => Some(FetchDataItem::Rfc822Size),
        "RFC822.TEXT" => Some(FetchDataItem::Rfc822Text),
        "UID" => Some(FetchDataItem::Uid),
        "BODYSTRUCTURE" => Some(FetchDataItem::BodyStructure { extensible: true }),
        "MODSEQ" => Some(FetchDataItem::Modseq),
        _ => None,
    };
    if let Some(item) = simple {
        return ParseResult::ok(item, context.position - start);
    }

    if item_name.starts_with("BODY") {
        // For `BODY[HEADER.FIELDS (FROM TO)]` the atom stops at the space
        // before the field list, so the rest of the section is read here.
        let mut full_body_expr = item_name.clone();
        let open_count = full_body_expr.matches('[').count();
        let close_count = full_body_expr.matches(']').count();
        if open_count > close_count {
            let extra = take_through(context, b']');
            full_body_expr = format!("{full_body_expr}{extra}]");
        }

        // The optional partial range `<start.length>` that follows the section.
        if peek_is(context, b'<') {
            context.position += 1;
            let partial = take_through(context, b'>');
            full_body_expr = format!("{full_body_expr}<{partial}>");
        }

        let body = parse_body_fetch(&full_body_expr);
        if body.success {
            return body;
        }
    }

    ParseResult::fail(format!("Unknown fetch data item: {item_name}"))
}

/// What `.` does not match in a JavaScript regular expression.
fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `/^BODY(?:\.PEEK)?\[(.*?)\](.*)$/`: the section and what follows it.
fn match_section(body_expr: &str) -> Option<(&str, &str)> {
    let rest = body_expr.strip_prefix("BODY")?;
    let rest = rest.strip_prefix(".PEEK").unwrap_or(rest);
    let rest = rest.strip_prefix('[')?;
    // `.` stops at a line terminator, so one anywhere fails the whole match.
    if rest.contains(is_line_terminator) {
        return None;
    }
    let close = rest.find(']')?;
    Some((&rest[..close], &rest[close + 1..]))
}

/// `/^<(\d+)\.(\d+)>$/`: a partial range.
fn match_partial(remainder: &str) -> Option<PartialRange> {
    let inner = remainder.strip_prefix('<')?.strip_suffix('>')?;
    let (start, length) = inner.split_once('.')?;
    let all_digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !all_digits(start) || !all_digits(length) {
        return None;
    }
    Some(PartialRange {
        start: start.parse().unwrap_or(f64::INFINITY),
        length: length.parse().unwrap_or(f64::INFINITY),
    })
}

/// Parses a `BODY…` expression, already upper-cased up to its first space.
///
/// Anything after the section that is not a well-formed `<start.length>` is
/// ignored, as the Bun parser ignores it.
pub fn parse_body_fetch(body_expr: &str) -> ParseResult<FetchDataItem> {
    // Bare `BODY` is the non-extensible form of BODYSTRUCTURE (RFC 3501
    // §6.4.5): a structure line, not content, and it never sets \Seen.
    if body_expr == "BODY" {
        return ParseResult::ok(FetchDataItem::BodyStructure { extensible: false }, 0);
    }

    if body_expr == "BODY.PEEK" {
        return ParseResult::ok(
            FetchDataItem::Body(BodyFetch { peek: true, section: BodySection::Full, partial: None }),
            0,
        );
    }

    if let Some((section_str, remainder)) = match_section(body_expr) {
        let peek = body_expr.contains(".PEEK");
        let Some(section) = parse_body_section(section_str).into_result().ok() else {
            return ParseResult::fail("Invalid body section");
        };
        let partial = match_partial(remainder);
        return ParseResult::ok(FetchDataItem::Body(BodyFetch { peek, section, partial }), 0);
    }

    ParseResult::fail(format!("Invalid body expression: {body_expr}"))
}

/// `/^\d+(\.\d+)*$/`: a MIME part number such as `1`, `1.2`, `1.2.3`.
fn is_part_number(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|piece| !piece.is_empty() && piece.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Parses a BODY section specifier: the text between the brackets.
pub fn parse_body_section(section_str: &str) -> ParseResult<BodySection> {
    if section_str.is_empty() {
        return ParseResult::ok(BodySection::Full, 0);
    }

    let upper_section = section_str.to_uppercase();
    match upper_section.as_str() {
        "HEADER" => return ParseResult::ok(BodySection::Header, 0),
        "TEXT" => return ParseResult::ok(BodySection::Text, 0),
        _ => {}
    }

    // The field list is cut from the section as written, at the length of
    // the keyword, in UTF-16 code units.
    for (keyword, not) in [("HEADER.FIELDS.NOT ", true), ("HEADER.FIELDS ", false)] {
        if upper_section.starts_with(keyword) {
            let fields = parse_header_fields(&utf16_slice(section_str, keyword.len(), usize::MAX));
            return ParseResult::ok(BodySection::HeaderFields { not, fields }, 0);
        }
    }

    if is_part_number(section_str) {
        return ParseResult::ok(BodySection::MimePart { part_number: section_str.to_string(), sub_section: None }, 0);
    }

    // A part with a sub-section: `1.HEADER`, `1.2.TEXT`, `2.MIME`.
    if let Some((part_number, sub_section)) = section_str.rsplit_once('.') {
        let sub_section = [MimeSubSection::Header, MimeSubSection::Text, MimeSubSection::Mime]
            .into_iter()
            .find(|candidate| candidate.as_str().eq_ignore_ascii_case(sub_section));
        if let (true, Some(sub_section)) = (is_part_number(part_number), sub_section) {
            return ParseResult::ok(
                BodySection::MimePart { part_number: part_number.to_string(), sub_section: Some(sub_section) },
                0,
            );
        }
    }

    ParseResult::fail(format!("Invalid body section: {section_str}"))
}

/// Parses a header field list: parentheses dropped, split on white space, upper-cased.
fn parse_header_fields(fields_str: &str) -> Vec<String> {
    let without_parens: String = fields_str.chars().filter(|c| *c != '(' && *c != ')').collect();
    let cleaned = js_trim(&without_parens);
    if cleaned.is_empty() {
        return Vec::new();
    }
    cleaned
        .split(js_is_whitespace)
        .filter(|field| !field.is_empty())
        .map(str::to_uppercase)
        .collect()
}
