//! The command line dispatcher (imap/parsers/command-parser.ts).

use super::append_parser::parse_append;
use super::auth_parsers::{parse_authenticate, parse_login};
use super::fetch_parsers::parse_fetch;
use super::mailbox_parsers::{
    parse_create, parse_delete, parse_list, parse_rename, parse_select, parse_status, parse_subscribe,
    parse_unsubscribe,
};
use super::primitive_parsers::{parse_atom, skip_whitespace};
use super::search_parsers::{parse_search_within, MAX_SEARCH_DEPTH, STACK_EXHAUSTED};
use super::store_parsers::{parse_copy, parse_move, parse_store};
use crate::imap::types::{ImapRequest, Literal, ParseContext, ParseResult, ParseThrow, ParsedCommand, UidRequest};
use crate::imap::util::js_trim_start;

/// How many `UID` prefixes one command may stack (`UID UID … FETCH`) before
/// the Bun parser runs out of stack. Like [`MAX_SEARCH_DEPTH`] this is a
/// point inside the range Bun's own limit moves in, 26,000 to 35,000. The
/// most that fits in one 64 KiB command line is 16,000, which always parses.
pub const MAX_UID_DEPTH: usize = 27_000;

/// Parses a complete IMAP command line: a tag, a command name and its arguments.
///
/// `literals` are the payloads of the `{N}` markers in `line`, in wire order.
/// They are copied, never drained, so the same command can be parsed twice:
/// the handler asks whether a command parses yet before it dispatches it.
///
/// ```ignore
/// let parsed = parse_command("a1 LOGIN {5+} {8+}", Some(&["admin".into(), "password".into()]));
/// ```
pub fn parse_command(line: &str, literals: Option<&[Literal]>) -> ParseResult<ParsedCommand> {
    // Only leading white space is framing. A literal payload ending in white
    // space is legal precisely because it was sent as a literal.
    let mut context = ParseContext::new(js_trim_start(line));
    context.literals = literals.map(|items| items.iter().cloned().collect());

    match parse_tagged_command(&mut context) {
        Ok(result) => result,
        Err(thrown) => ParseResult::fail(format!("Parse error: {thrown}")),
    }
}

fn parse_tagged_command(context: &mut ParseContext) -> Result<ParseResult<ParsedCommand>, ParseThrow> {
    let Some(tag) = parse_atom(context).value.filter(|tag| !tag.is_empty()) else {
        return Ok(ParseResult::fail("Invalid tag"));
    };

    skip_whitespace(context);

    let Some(command) = parse_atom(context).value.filter(|command| !command.is_empty()) else {
        return Ok(ParseResult::fail("Invalid command"));
    };

    skip_whitespace(context);

    let request = parse_imap_request(&command.to_uppercase(), context)?;
    let Some(request) = request.value.filter(|_| request.success) else {
        return Ok(ParseResult { success: false, value: None, error: request.error, consumed: 0 });
    };

    Ok(ParseResult::ok(ParsedCommand { tag, request }, context.position))
}

fn bare(request: ImapRequest, context: &ParseContext) -> ParseResult<ImapRequest> {
    ParseResult::ok(request, context.position)
}

/// Parses the arguments of `command`, an upper-cased command name.
///
/// `UID` wraps whatever command follows it, itself included, and the Bun
/// parser recurses once per `UID`. The prefixes are counted here and wrapped
/// around the innermost request afterwards.
fn parse_imap_request(command: &str, context: &mut ParseContext) -> Result<ParseResult<ImapRequest>, ParseThrow> {
    let mut command = command.to_string();
    // The sub-command name each `UID` read, outermost first.
    let mut uid_commands: Vec<String> = Vec::new();

    while command == "UID" {
        if uid_commands.len() >= MAX_UID_DEPTH {
            return Err(ParseThrow(STACK_EXHAUSTED.to_string()));
        }
        let Some(sub_command) = parse_atom(context).into_result().ok() else {
            return Ok(ParseResult::fail("Invalid UID subcommand"));
        };
        skip_whitespace(context);
        command = sub_command.to_uppercase();
        uid_commands.push(command.clone());
    }

    // The stack a nested SEARCH has left shrinks with the `UID`s above it.
    let search_depth = MAX_SEARCH_DEPTH.saturating_sub(uid_commands.len() * 4 / 5);
    let inner = parse_plain_request(&command, context, search_depth)?;
    if uid_commands.is_empty() {
        return Ok(inner);
    }
    let Some(mut request) = inner.value.filter(|_| inner.success) else {
        return Ok(ParseResult { success: false, value: None, error: inner.error, consumed: 0 });
    };
    for sub_command in uid_commands.into_iter().rev() {
        request = ImapRequest::Uid(UidRequest::new(sub_command, request));
    }
    Ok(ParseResult::ok(request, context.position))
}

/// Every command but `UID`.
fn parse_plain_request(
    command: &str,
    context: &mut ParseContext,
    search_depth: usize,
) -> Result<ParseResult<ImapRequest>, ParseThrow> {
    Ok(match command {
        "CAPABILITY" => bare(ImapRequest::Capability, context),
        "NOOP" => bare(ImapRequest::Noop, context),
        "LOGOUT" => bare(ImapRequest::Logout, context),
        "ID" => {
            // The ID parameters are not parsed: skip them.
            context.position = context.length;
            bare(ImapRequest::Id, context)
        }
        "DONE" => bare(ImapRequest::Done, context),
        "LOGIN" => parse_login(context),
        "AUTHENTICATE" => parse_authenticate(context),
        "LIST" | "LSUB" => parse_list(command, context),
        "SELECT" => parse_select(false, context),
        "EXAMINE" => parse_select(true, context),
        "CREATE" => parse_create(context),
        "DELETE" => parse_delete(context),
        "RENAME" => parse_rename(context),
        "SUBSCRIBE" => parse_subscribe(context),
        "UNSUBSCRIBE" => parse_unsubscribe(context),
        "STATUS" => parse_status(context),
        "FETCH" => parse_fetch(context),
        "SEARCH" => parse_search_within(context, search_depth)?,
        "STORE" => parse_store(context),
        "COPY" => parse_copy(context),
        "MOVE" => parse_move(context),
        "CHECK" => bare(ImapRequest::Check, context),
        "CLOSE" => bare(ImapRequest::Close, context),
        "EXPUNGE" => bare(ImapRequest::Expunge, context),
        "APPEND" => parse_append(context),
        "IDLE" => bare(ImapRequest::Idle, context),
        "STARTTLS" => bare(ImapRequest::Starttls, context),
        "NAMESPACE" => bare(ImapRequest::Namespace, context),
        "ENABLE" => {
            // ENABLE <capability> [<capability> ...]
            let mut capabilities: Vec<String> = Vec::new();
            skip_whitespace(context);
            while context.position < context.length {
                let Some(capability) = parse_atom(context).into_result().ok() else {
                    break;
                };
                capabilities.push(capability.to_uppercase());
                skip_whitespace(context);
            }
            bare(ImapRequest::Enable { capabilities }, context)
        }
        "UNSELECT" => bare(ImapRequest::Unselect, context),
        "GETQUOTAROOT" => {
            skip_whitespace(context);
            let mailbox = parse_atom(context).into_result().unwrap_or_else(|_| "INBOX".to_string());
            bare(ImapRequest::Getquotaroot { mailbox }, context)
        }
        _ => ParseResult::fail(format!("Unknown command: {command}")),
    })
}
