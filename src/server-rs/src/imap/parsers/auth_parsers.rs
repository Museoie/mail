//! LOGIN and AUTHENTICATE arguments (imap/parsers/auth-parsers.ts).

use super::primitive_parsers::{parse_string, skip_whitespace};
use crate::imap::types::{AuthenticateRequest, ImapRequest, LoginRequest, ParseContext, ParseResult};

/// Parses LOGIN: a user name and a password, each a string.
pub fn parse_login(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    skip_whitespace(context);
    let Some(username) = parse_string(context).into_result().ok() else {
        return ParseResult::fail("Invalid username in LOGIN");
    };

    skip_whitespace(context);
    let Some(password) = parse_string(context).into_result().ok() else {
        return ParseResult::fail("Invalid password in LOGIN");
    };

    ParseResult::ok(ImapRequest::Login(LoginRequest { username, password }), context.position)
}

/// Parses AUTHENTICATE: a mechanism name and an optional initial response
/// (RFC 4959 SASL-IR). The mechanism keeps the case the client sent.
pub fn parse_authenticate(context: &mut ParseContext) -> ParseResult<ImapRequest> {
    skip_whitespace(context);
    let Some(mechanism) = parse_string(context).into_result().ok() else {
        return ParseResult::fail("Invalid mechanism in AUTHENTICATE");
    };

    skip_whitespace(context);
    let mut initial_response = None;
    if context.position < context.length {
        // A response that does not parse is dropped, not an error.
        initial_response = parse_string(context).into_result().ok();
    }

    ParseResult::ok(
        ImapRequest::Authenticate(AuthenticateRequest { mechanism, initial_response }),
        context.position,
    )
}
