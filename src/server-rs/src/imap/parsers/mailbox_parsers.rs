//! Mailbox command arguments (imap/parsers/mailbox-parsers.ts).

use serde_json::json;

use super::primitive_parsers::{consume, parse_atom, parse_list_mailbox, parse_string, peek_is, skip_whitespace};
use crate::imap::types::{ImapRequest, ListRequest, ParseContext, ParseResult, SelectRequest, StatusItem, StatusRequest};
use crate::logger;

/// One string argument after optional spaces, or `error`.
fn mailbox_argument(context: &mut ParseContext, error: &str) -> Result<String, String> {
    skip_whitespace(context);
    parse_string(context).into_result().map_err(|_| error.to_string())
}

/// Parses LIST or LSUB: a reference name and a mailbox pattern.
pub fn parse_list(command: &str, context: &mut ParseContext) -> ParseResult<ImapRequest> {
    skip_whitespace(context);

    // The reference name can be a quoted string or an atom.
    let Some(reference) = parse_string(context).into_result().ok() else {
        logger::debug(
            "Failed to parse reference name",
            json!({ "component": "imap.parser", "position": context.position }),
        );
        return ParseResult::fail("Invalid reference name in LIST");
    };

    skip_whitespace(context);

    // list-mailbox allows the `*` / `%` wildcards as ordinary characters, so
    // a mixed pattern like `INBOX/%` is captured in full.
    let Some(pattern) = parse_list_mailbox(context).into_result().ok() else {
        logger::debug(
            "Failed to parse mailbox name",
            json!({ "component": "imap.parser", "position": context.position }),
        );
        return ParseResult::fail("Invalid mailbox name in LIST");
    };

    let data = ListRequest { reference, pattern };
    let request = if command == "LSUB" { ImapRequest::Lsub(data) } else { ImapRequest::List(data) };
    ParseResult::ok(request, context.position)
}

/// Parses SELECT, or EXAMINE when `read_only`.
pub fn parse_select(read_only: bool, context: &mut ParseContext) -> ParseResult<ImapRequest> {
    let mailbox = match mailbox_argument(context, "Invalid mailbox name") {
        Ok(mailbox) => mailbox,
        Err(error) => return ParseResult::fail(error),
    };
    let data = SelectRequest { mailbox };
    let request = if read_only { ImapRequest::Examine(data) } else { ImapRequest::Select(data) };
    ParseResult::ok(request, context.position)
}

/// Parses STATUS: a mailbox and a parenthesized list of status items.
///
/// The list may run to the end of the line without its closing parenthesis.
pub fn parse_status(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    let mailbox = match mailbox_argument(context, "Invalid mailbox name in STATUS") {
        Ok(mailbox) => mailbox,
        Err(error) => return ParseResult::fail(error),
    };

    skip_whitespace(context);
    if !consume(context, "(") {
        return ParseResult::fail("Expected opening parenthesis in STATUS");
    }

    let mut items: Vec<StatusItem> = Vec::new();
    while context.position < context.length {
        skip_whitespace(context);
        if peek_is(context, b')') {
            context.position += 1;
            break;
        }

        let Some(item) = parse_atom(context).into_result().ok() else {
            return ParseResult::fail("Invalid status item");
        };
        let name = item.to_uppercase();
        match StatusItem::parse(&name) {
            Some(item) => items.push(item),
            None => return ParseResult::fail(format!("Unknown status item: {name}")),
        }

        skip_whitespace(context);
    }

    ParseResult::ok(ImapRequest::Status(StatusRequest { mailbox, items }), context.position)
}

/// Parses CREATE.
pub fn parse_create(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    match mailbox_argument(context, "Invalid mailbox name in CREATE") {
        Ok(mailbox) => ParseResult::ok(ImapRequest::Create { mailbox }, context.position),
        Err(error) => ParseResult::fail(error),
    }
}

/// Parses DELETE.
pub fn parse_delete(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    match mailbox_argument(context, "Invalid mailbox name in DELETE") {
        Ok(mailbox) => ParseResult::ok(ImapRequest::Delete { mailbox }, context.position),
        Err(error) => ParseResult::fail(error),
    }
}

/// Parses RENAME: the existing name and the new one.
pub fn parse_rename(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    let old_name = match mailbox_argument(context, "Invalid existing mailbox name in RENAME") {
        Ok(name) => name,
        Err(error) => return ParseResult::fail(error),
    };
    let new_name = match mailbox_argument(context, "Invalid new mailbox name in RENAME") {
        Ok(name) => name,
        Err(error) => return ParseResult::fail(error),
    };
    ParseResult::ok(ImapRequest::Rename { old_name, new_name }, context.position)
}

/// Parses SUBSCRIBE.
pub fn parse_subscribe(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    match mailbox_argument(context, "Invalid mailbox name in SUBSCRIBE") {
        Ok(mailbox) => ParseResult::ok(ImapRequest::Subscribe { mailbox }, context.position),
        Err(error) => ParseResult::fail(error),
    }
}

/// Parses UNSUBSCRIBE.
pub fn parse_unsubscribe(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    match mailbox_argument(context, "Invalid mailbox name in UNSUBSCRIBE") {
        Ok(mailbox) => ParseResult::ok(ImapRequest::Unsubscribe { mailbox }, context.position),
        Err(error) => ParseResult::fail(error),
    }
}
