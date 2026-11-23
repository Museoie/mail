//! APPEND arguments (imap/parsers/append-parser.ts).

use super::primitive_parsers::{parse_flag, parse_message_literal, parse_string, skip_whitespace};
use crate::imap::types::{AppendRequest, ImapRequest, ParseContext, ParseResult};

fn at(context: &ParseContext, byte: u8) -> bool {
    context.input.get(context.position) == Some(&(byte as u16))
}

/// Parses APPEND: `mailbox [flags] [date] message`.
///
/// The flag list and the date are each taken only if they are well formed; a
/// list that never closes is left where it stopped, and the message literal
/// is then looked for there.
pub fn parse_append(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    let Some(mailbox) = parse_string(context).into_result().ok() else {
        return ParseResult::fail("Expected mailbox name");
    };

    skip_whitespace(context);

    let mut flags: Option<Vec<String>> = None;
    if at(context, b'(') {
        context.position += 1;
        let mut names = Vec::new();
        while context.position < context.length && !at(context, b')') {
            if at(context, b' ') {
                context.position += 1;
                continue;
            }
            match parse_flag(context).into_result() {
                Ok(flag) => names.push(flag),
                Err(_) => break,
            }
        }
        if at(context, b')') {
            context.position += 1;
            flags = Some(names);
            skip_whitespace(context);
        }
    }

    let mut date: Option<String> = None;
    if at(context, b'"') {
        if let Ok(value) = parse_string(context).into_result() {
            date = Some(value);
            skip_whitespace(context);
        }
    }

    // The message literal. `{N}` counts octets, so the payload arrives out of
    // band on `context.literals`, already spooled to a file by the handler.
    let literal = parse_message_literal(context);
    let Some(message) = literal.value.filter(|_| literal.success) else {
        let error = literal.error.filter(|error| !error.is_empty());
        return ParseResult::fail(error.unwrap_or_else(|| "Expected message literal".to_string()));
    };

    ParseResult::ok(
        ImapRequest::Append(AppendRequest { mailbox, flags, date, message }),
        context.position,
    )
}
