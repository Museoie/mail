//! STORE arguments (imap/parsers/store-parsers.ts), and COPY and MOVE.

use super::primitive_parsers::{
    parse_atom, parse_flag, parse_modifier_group, parse_sequence_set, parse_string, peek_is, skip_whitespace,
};
use crate::imap::types::{CopyRequest, ImapRequest, ParseContext, ParseResult, StoreOperation, StoreRequest};

/// Parses STORE: a sequence set, an optional `(UNCHANGEDSINCE <modseq>)`
/// group (RFC 7162 §3.1.3), the item name, and a flag or a parenthesized
/// flag list.
pub fn parse_store(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    let Some(sequence_set) = parse_sequence_set(context).into_result().ok() else {
        return ParseResult::fail("Invalid sequence set in STORE");
    };

    // The modifier group sits between the sequence set and the item name. It
    // is unambiguous to detect: an item name is an atom and never starts with `(`.
    let unchanged_since = parse_modifier_group(context, "STORE", "UNCHANGEDSINCE");
    if !unchanged_since.success {
        return ParseResult { success: false, value: None, error: unchanged_since.error, consumed: 0 };
    }

    skip_whitespace(context);

    let Some(item_name) = parse_atom(context).into_result().ok() else {
        return ParseResult::fail("Invalid item name in STORE");
    };
    let item_name = item_name.to_uppercase();
    let Some(operation) = StoreOperation::parse(&item_name) else {
        return ParseResult::fail(format!("Invalid store operation: {item_name}"));
    };

    skip_whitespace(context);

    let mut flags: Vec<String> = Vec::new();
    if peek_is(context, b'(') {
        context.position += 1;
        while context.position < context.length {
            skip_whitespace(context);
            if peek_is(context, b')') {
                context.position += 1;
                break;
            }
            let Some(flag) = parse_flag(context).into_result().ok() else {
                return ParseResult::fail("Invalid flag in STORE");
            };
            flags.push(flag);
            skip_whitespace(context);
        }
    } else {
        // A single flag without parentheses.
        let Some(flag) = parse_flag(context).into_result().ok() else {
            return ParseResult::fail("Invalid flag in STORE");
        };
        flags.push(flag);
    }

    ParseResult::ok(
        ImapRequest::Store(StoreRequest {
            sequence_set,
            operation,
            flags,
            silent: operation.is_silent(),
            unchanged_since: unchanged_since.value,
        }),
        context.position,
    )
}

/// A sequence set and a target mailbox: the arguments of COPY and of MOVE.
fn parse_copy_arguments(command: &str, context: &mut ParseContext) -> Result<CopyRequest, String> {
    let Some(sequence_set) = parse_sequence_set(context).into_result().ok() else {
        return Err(format!("Invalid sequence set in {command}"));
    };
    skip_whitespace(context);
    let Some(mailbox) = parse_string(context).into_result().ok() else {
        return Err(format!("Invalid mailbox name in {command}"));
    };
    Ok(CopyRequest { sequence_set, mailbox })
}

/// Parses COPY.
pub fn parse_copy(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    match parse_copy_arguments("COPY", context) {
        Ok(data) => ParseResult::ok(ImapRequest::Copy(data), context.position),
        Err(error) => ParseResult::fail(error),
    }
}

/// Parses MOVE (RFC 6851), which has the wire shape of COPY.
pub fn parse_move(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    match parse_copy_arguments("MOVE", context) {
        Ok(data) => ParseResult::ok(ImapRequest::Move(data), context.position),
        Err(error) => ParseResult::fail(error),
    }
}
