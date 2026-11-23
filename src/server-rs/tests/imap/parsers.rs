//! The Bun server's parser unit tests: `parsers/*.test.ts`, case for case.

use mail_server::imap::parsers::append_parser::parse_append;
use mail_server::imap::parsers::auth_parsers::{parse_authenticate, parse_login};
use mail_server::imap::parsers::command_parser::parse_command;
use mail_server::imap::parsers::mailbox_parsers::{
    parse_create, parse_delete, parse_list, parse_rename, parse_select, parse_status,
};
use mail_server::imap::parsers::primitive_parsers::*;
use mail_server::imap::parsers::search_parsers::{parse_search, parse_search_criteria};
use mail_server::imap::parsers::store_parsers::{parse_copy, parse_store};
use mail_server::imap::types::*;

fn ctx(input: &str) -> ParseContext {
    ParseContext::new(input)
}

fn ctx_at(input: &str, position: usize) -> ParseContext {
    let mut context = ParseContext::new(input);
    context.position = position;
    context
}

fn ctx_with(input: &str, literals: Vec<Literal>) -> ParseContext {
    ParseContext::with_literals(input, literals)
}

fn command(line: &str) -> ParseResult<ParsedCommand> {
    parse_command(line, None)
}

fn command_with(line: &str, literals: &[Literal]) -> ParseResult<ParsedCommand> {
    parse_command(line, Some(literals))
}

/// The request of a command line that has to parse.
fn request(line: &str) -> ImapRequest {
    let result = command(line);
    assert!(result.success, "{line}: {:?}", result.error);
    result.value.unwrap().request
}

fn request_with(line: &str, literals: &[Literal]) -> ImapRequest {
    let result = command_with(line, literals);
    assert!(result.success, "{line}: {:?}", result.error);
    result.value.unwrap().request
}

/// The handler spools a message literal to disk; the parser only routes it.
fn spooled(message: &str) -> MessageLiteral {
    MessageLiteral { path: "/spool/message".to_string(), size: message.len() as u64 }
}

fn texts(items: &[&str]) -> Vec<Literal> {
    items.iter().map(|item| Literal::from(*item)).collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

fn single(start: f64) -> SequenceRange {
    SequenceRange::single(start)
}

fn range(start: f64, end: f64) -> SequenceRange {
    SequenceRange::range(start, end)
}

// ---------------------------------------------------------------------------
// primitive-parsers.test.ts
// ---------------------------------------------------------------------------

mod primitive {
    use super::*;

    #[test]
    fn atom_parses_a_simple_atom() {
        let mut c = ctx("INBOX");
        let r = parse_atom(&mut c);
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("INBOX"));
        assert_eq!(c.position, 5);
    }

    #[test]
    fn atom_stops_at_space() {
        let mut c = ctx("INBOX rest");
        let r = parse_atom(&mut c);
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("INBOX"));
        assert_eq!(c.position, 5);
    }

    #[test]
    fn atom_stops_at_parenthesis() {
        assert_eq!(parse_atom(&mut ctx("FLAGS)")).value.as_deref(), Some("FLAGS"));
    }

    #[test]
    fn atom_fails_on_empty_input() {
        assert!(!parse_atom(&mut ctx("")).success);
    }

    #[test]
    fn atom_fails_when_first_char_is_a_special() {
        assert!(!parse_atom(&mut ctx("(atom")).success);
    }

    #[test]
    fn atom_stops_at_double_quote() {
        assert_eq!(parse_atom(&mut ctx("atom\"rest")).value.as_deref(), Some("atom"));
    }

    #[test]
    fn atom_stops_at_backslash() {
        assert_eq!(parse_atom(&mut ctx("atom\\flag")).value.as_deref(), Some("atom"));
    }

    #[test]
    fn atom_stops_at_asterisk() {
        assert_eq!(parse_atom(&mut ctx("atom*rest")).value.as_deref(), Some("atom"));
    }

    #[test]
    fn atom_stops_at_percent() {
        assert_eq!(parse_atom(&mut ctx("atom%rest")).value.as_deref(), Some("atom"));
    }

    #[test]
    fn atom_stops_at_control_character() {
        assert_eq!(parse_atom(&mut ctx("atom\x01rest")).value.as_deref(), Some("atom"));
    }

    #[test]
    fn number_parses_a_single_digit() {
        let r = parse_number(&mut ctx("5"));
        assert!(r.success);
        assert_eq!(r.value, Some(5.0));
    }

    #[test]
    fn number_parses_a_multi_digit_number() {
        let r = parse_number(&mut ctx("12345"));
        assert_eq!(r.value, Some(12345.0));
        assert_eq!(r.consumed, 5);
    }

    #[test]
    fn number_stops_at_non_digit() {
        let mut c = ctx("42abc");
        assert_eq!(parse_number(&mut c).value, Some(42.0));
        assert_eq!(c.position, 2);
    }

    #[test]
    fn number_fails_on_non_digit_input() {
        assert!(!parse_number(&mut ctx("abc")).success);
    }

    #[test]
    fn number_fails_on_empty_input() {
        assert!(!parse_number(&mut ctx("")).success);
    }

    #[test]
    fn flag_parses_a_backslash_flag() {
        let r = parse_flag(&mut ctx("\\Seen"));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("\\Seen"));
    }

    #[test]
    fn flag_parses_a_flag_without_backslash() {
        let r = parse_flag(&mut ctx("Answered"));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("Answered"));
    }

    #[test]
    fn flag_stops_at_space() {
        let mut c = ctx("\\Seen \\Flagged");
        assert_eq!(parse_flag(&mut c).value.as_deref(), Some("\\Seen"));
        assert_eq!(c.position, 5);
    }

    #[test]
    fn flag_fails_on_empty_input() {
        assert!(!parse_flag(&mut ctx("")).success);
    }

    #[test]
    fn flag_accepts_a_lone_backslash_at_end_of_input() {
        // The backslash moved the position past it, so something was consumed.
        let r = parse_flag(&mut ctx("\\"));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("\\"));
    }

    #[test]
    fn quoted_parses_a_simple_quoted_string() {
        let r = parse_quoted_string(&mut ctx("\"hello\""));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("hello"));
    }

    #[test]
    fn quoted_parses_an_empty_quoted_string() {
        let r = parse_quoted_string(&mut ctx("\"\""));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some(""));
    }

    #[test]
    fn quoted_handles_escaped_characters() {
        let r = parse_quoted_string(&mut ctx("\"hello\\\"world\""));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("hello\"world"));
    }

    #[test]
    fn quoted_handles_escaped_backslash() {
        let r = parse_quoted_string(&mut ctx("\"hello\\\\world\""));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("hello\\world"));
    }

    #[test]
    fn quoted_fails_on_unterminated_string() {
        let r = parse_quoted_string(&mut ctx("\"hello"));
        assert!(!r.success);
        assert!(r.error.unwrap().contains("Unterminated"));
    }

    #[test]
    fn quoted_fails_if_not_starting_with_quote() {
        assert!(!parse_quoted_string(&mut ctx("hello")).success);
    }

    #[test]
    fn string_delegates_to_quoted_string_when_input_starts_with_quote() {
        let r = parse_string(&mut ctx("\"test\""));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("test"));
    }

    #[test]
    fn string_falls_back_to_atom_for_unquoted_input() {
        let r = parse_string(&mut ctx("INBOX"));
        assert!(r.success);
        assert_eq!(r.value.as_deref(), Some("INBOX"));
    }

    #[test]
    fn sequence_set_parses_a_single_number() {
        let r = parse_sequence_set(&mut ctx("5"));
        assert!(r.success);
        assert_eq!(r.value.unwrap().ranges, vec![single(5.0)]);
    }

    #[test]
    fn sequence_set_parses_a_range() {
        let r = parse_sequence_set(&mut ctx("1:5"));
        assert!(r.success);
        assert_eq!(r.value.unwrap().ranges[0], range(1.0, 5.0));
    }

    #[test]
    fn sequence_set_parses_wildcard_range() {
        let r = parse_sequence_set(&mut ctx("1:*"));
        assert!(r.success);
        assert_eq!(r.value.unwrap().ranges[0].end, Some(MAX_SAFE_INTEGER));
    }

    #[test]
    fn sequence_set_parses_standalone_star() {
        let r = parse_sequence_set(&mut ctx("*"));
        assert!(r.success);
        assert_eq!(r.value.unwrap().ranges[0].start, MAX_SAFE_INTEGER);
    }

    #[test]
    fn sequence_set_parses_comma_separated_set() {
        let r = parse_sequence_set(&mut ctx("1,3,5"));
        assert!(r.success);
        assert_eq!(r.value.unwrap().ranges.len(), 3);
    }

    #[test]
    fn sequence_set_parses_mixed_ranges_and_singles() {
        let r = parse_sequence_set(&mut ctx("1:3,5,7:9"));
        assert!(r.success);
        assert_eq!(r.value.unwrap().ranges, vec![range(1.0, 3.0), single(5.0), range(7.0, 9.0)]);
    }

    #[test]
    fn sequence_set_fails_on_empty_input() {
        assert!(!parse_sequence_set(&mut ctx("")).success);
    }

    #[test]
    fn sequence_set_fails_on_non_numeric_non_star_input() {
        assert!(!parse_sequence_set(&mut ctx("abc")).success);
    }

    #[test]
    fn sequence_set_fails_on_invalid_range_end() {
        assert!(!parse_sequence_set(&mut ctx("1:abc")).success);
    }

    #[test]
    fn skip_whitespace_skips_spaces() {
        let mut c = ctx("   abc");
        skip_whitespace(&mut c);
        assert_eq!(c.position, 3);
    }

    #[test]
    fn skip_whitespace_does_nothing_without_leading_spaces() {
        let mut c = ctx("abc");
        skip_whitespace(&mut c);
        assert_eq!(c.position, 0);
    }

    #[test]
    fn skip_whitespace_does_not_skip_tabs_or_newlines() {
        let mut c = ctx("\tabc");
        skip_whitespace(&mut c);
        assert_eq!(c.position, 0);
    }

    #[test]
    fn peek_returns_current_character_without_advancing() {
        let c = ctx("abc");
        assert_eq!(peek(&c), Some(b'a' as u16));
        assert_eq!(c.position, 0);
    }

    #[test]
    fn peek_returns_nothing_at_end_of_input() {
        assert_eq!(peek(&ctx_at("a", 1)), None);
    }

    #[test]
    fn consume_consumes_matching_string() {
        let mut c = ctx("FLAGS");
        assert!(consume(&mut c, "FLAGS"));
        assert_eq!(c.position, 5);
    }

    #[test]
    fn consume_does_not_advance_for_non_match() {
        let mut c = ctx("FLAGS");
        assert!(!consume(&mut c, "BODY"));
        assert_eq!(c.position, 0);
    }

    #[test]
    fn consume_matches_from_current_position() {
        let mut c = ctx_at("ABCDEF", 2);
        assert!(consume(&mut c, "CD"));
        assert_eq!(c.position, 4);
    }

    #[test]
    fn date_parses_a_valid_date_atom() {
        let r = parse_date(&mut ctx("01-Jan-2024"));
        assert!(r.success);
        assert!(r.value.unwrap().is_valid());
    }

    #[test]
    fn date_fails_on_invalid_date() {
        let r = parse_date(&mut ctx("not-a-date"));
        assert!(!r.success);
        assert!(r.error.unwrap().contains("Invalid date"));
    }

    #[test]
    fn date_fails_on_empty_input() {
        assert!(!parse_date(&mut ctx("")).success);
    }
}

// ---------------------------------------------------------------------------
// append-parser.test.ts
// ---------------------------------------------------------------------------

mod append {
    use super::*;

    fn append(line: &str, message: &str) -> AppendRequest {
        match request_with(line, &[spooled(message).into()]) {
            ImapRequest::Append(data) => data,
            other => panic!("Expected APPEND request type, got {}", other.type_name()),
        }
    }

    #[test]
    fn parses_append_with_simple_mailbox_and_message() {
        let message = "From: test@example.com\r\nSubject: Test\r\n\r\nHello";
        let line = format!("A001 APPEND INBOX {{{}}}", message.len());
        let result = command_with(&line, &[spooled(message).into()]);
        assert!(result.success);
        assert_eq!(result.value.as_ref().unwrap().tag, "A001");
        let data = append(&line, message);
        assert_eq!(data.mailbox, "INBOX");
        assert_eq!(data.message, spooled(message));
        assert_eq!(data.flags, None);
        assert_eq!(data.date, None);
    }

    #[test]
    fn parses_append_with_quoted_mailbox_name() {
        assert_eq!(append("A001 APPEND \"Sent Items\" {5}", "Hello").mailbox, "Sent Items");
    }

    #[test]
    fn parses_append_with_flags() {
        let data = append("A001 APPEND INBOX (\\Seen \\Flagged) {5}", "Hello");
        assert_eq!(data.flags, Some(strings(&["\\Seen", "\\Flagged"])));
    }

    #[test]
    fn parses_append_with_single_flag() {
        assert_eq!(append("A001 APPEND INBOX (\\Draft) {5}", "Hello").flags, Some(strings(&["\\Draft"])));
    }

    #[test]
    fn parses_append_with_date() {
        let data = append("A001 APPEND INBOX \"25-Feb-2026 10:30:00 -0800\" {5}", "Hello");
        assert_eq!(data.date.as_deref(), Some("25-Feb-2026 10:30:00 -0800"));
    }

    #[test]
    fn parses_append_with_flags_and_date() {
        let message = "Test message";
        let data = append("A001 APPEND INBOX (\\Seen) \"25-Feb-2026 10:30:00 -0800\" {12}", message);
        assert_eq!(data.flags, Some(strings(&["\\Seen"])));
        assert_eq!(data.date.as_deref(), Some("25-Feb-2026 10:30:00 -0800"));
        assert_eq!(data.message, spooled(message));
    }

    #[test]
    fn parses_append_with_multiple_flags() {
        let data = append("A001 APPEND INBOX (\\Seen \\Answered \\Flagged \\Deleted) {5}", "Hello");
        assert_eq!(data.flags, Some(strings(&["\\Seen", "\\Answered", "\\Flagged", "\\Deleted"])));
    }

    #[test]
    fn fails_append_with_missing_literal() {
        assert!(!command("A001 APPEND INBOX").success);
    }

    #[test]
    fn fails_append_with_malformed_literal() {
        assert!(!command("A001 APPEND INBOX {100").success);
    }

    #[test]
    fn fails_append_with_invalid_literal_size() {
        assert!(!command("A001 APPEND INBOX {abc}\r\ndata").success);
    }

    #[test]
    fn parses_append_with_hierarchical_mailbox() {
        assert_eq!(append("A001 APPEND \"INBOX/Subfolder/Deep\" {2}", "Hi").mailbox, "INBOX/Subfolder/Deep");
    }

    #[test]
    fn parses_append_with_empty_flags_list() {
        assert_eq!(append("A001 APPEND INBOX () {4}", "Test").flags, Some(Vec::new()));
    }

    #[test]
    fn parses_append_with_binary_like_message_content() {
        let message = "Content with \x00 null bytes and \u{ff} high bytes";
        let line = format!("A001 APPEND INBOX {{{}}}", message.chars().count());
        assert_eq!(append(&line, message).message, spooled(message));
    }

    #[test]
    fn handles_append_with_exact_literal_size() {
        let message = "Exactly this much";
        let line = format!("A001 APPEND INBOX {{{}}}", message.len());
        assert_eq!(append(&line, message).message.size, message.len() as u64);
    }

    #[test]
    fn parses_append_with_zero_length_message() {
        assert_eq!(append("A001 APPEND INBOX {0}", "").message, spooled(""));
    }
}

// ---------------------------------------------------------------------------
// auth-parsers.test.ts
// ---------------------------------------------------------------------------

mod auth {
    use super::*;

    fn login(line: &str) -> LoginRequest {
        match request(line) {
            ImapRequest::Login(data) => data,
            other => panic!("Expected LOGIN request type, got {}", other.type_name()),
        }
    }

    fn authenticate(line: &str) -> AuthenticateRequest {
        match request(line) {
            ImapRequest::Authenticate(data) => data,
            other => panic!("Expected AUTHENTICATE request type, got {}", other.type_name()),
        }
    }

    #[test]
    fn parses_login_with_simple_credentials() {
        let result = command("A001 LOGIN user password");
        assert!(result.success);
        assert_eq!(result.value.unwrap().tag, "A001");
        let data = login("A001 LOGIN user password");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("user", "password"));
    }

    #[test]
    fn parses_login_with_quoted_username() {
        let data = login("A001 LOGIN \"john.doe@example.com\" password");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("john.doe@example.com", "password"));
    }

    #[test]
    fn parses_login_with_quoted_password() {
        let data = login("A001 LOGIN user \"my secret password\"");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("user", "my secret password"));
    }

    #[test]
    fn parses_login_with_special_characters_in_quoted_password() {
        assert_eq!(login("A001 LOGIN user \"p@ss{w0rd}!#$%\"").password, "p@ss{w0rd}!#$%");
    }

    #[test]
    fn parses_login_with_both_credentials_quoted() {
        let data = login("A001 LOGIN \"user@domain.com\" \"complex password\"");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("user@domain.com", "complex password"));
    }

    #[test]
    fn fails_login_with_missing_password() {
        assert!(!command("A001 LOGIN user").success);
    }

    #[test]
    fn fails_login_with_missing_username() {
        assert!(!command("A001 LOGIN").success);
    }

    #[test]
    fn parses_login_with_numeric_credentials() {
        let data = login("A001 LOGIN 12345 67890");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("12345", "67890"));
    }

    #[test]
    fn parses_authenticate_plain_without_initial_response() {
        let result = command("A001 AUTHENTICATE PLAIN");
        assert!(result.success);
        assert_eq!(result.value.unwrap().tag, "A001");
        let data = authenticate("A001 AUTHENTICATE PLAIN");
        assert_eq!(data.mechanism, "PLAIN");
        assert_eq!(data.initial_response, None);
    }

    #[test]
    fn parses_authenticate_plain_with_initial_response() {
        let data = authenticate("A001 AUTHENTICATE PLAIN AGVtYWlsQGV4YW1wbGUuY29tAHBhc3N3b3Jk");
        assert_eq!(data.mechanism, "PLAIN");
        assert_eq!(data.initial_response.as_deref(), Some("AGVtYWlsQGV4YW1wbGUuY29tAHBhc3N3b3Jk"));
    }

    #[test]
    fn parses_authenticate_with_login_mechanism() {
        assert_eq!(authenticate("A001 AUTHENTICATE LOGIN").mechanism, "LOGIN");
    }

    #[test]
    fn fails_authenticate_with_missing_mechanism() {
        assert!(!command("A001 AUTHENTICATE").success);
    }

    #[test]
    fn parses_authenticate_with_lowercase_mechanism_preserving_case() {
        assert_eq!(authenticate("A001 AUTHENTICATE plain").mechanism, "plain");
    }

    #[test]
    fn parses_authenticate_xoauth2() {
        assert_eq!(authenticate("A001 AUTHENTICATE XOAUTH2").mechanism, "XOAUTH2");
    }
}

// ---------------------------------------------------------------------------
// store-parsers.test.ts
// ---------------------------------------------------------------------------

mod store {
    use super::*;

    fn store(input: &str) -> StoreRequest {
        let r = parse_store(&mut ctx(input));
        assert!(r.success, "{input}: {:?}", r.error);
        match r.value.unwrap() {
            ImapRequest::Store(data) => data,
            other => panic!("expected STORE, got {}", other.type_name()),
        }
    }

    fn copy(input: &str) -> CopyRequest {
        let r = parse_copy(&mut ctx(input));
        assert!(r.success, "{input}: {:?}", r.error);
        match r.value.unwrap() {
            ImapRequest::Copy(data) => data,
            other => panic!("expected COPY, got {}", other.type_name()),
        }
    }

    #[test]
    fn parses_flags_with_a_single_flag() {
        let data = store("1 FLAGS (\\Seen)");
        assert_eq!(data.operation.as_str(), "FLAGS");
        assert_eq!(data.flags, strings(&["\\Seen"]));
        assert!(!data.silent);
    }

    #[test]
    fn parses_add_flags_silent_with_multiple_flags() {
        let data = store("1:5 +FLAGS.SILENT (\\Seen \\Flagged)");
        assert_eq!(data.operation.as_str(), "+FLAGS.SILENT");
        assert_eq!(data.flags, strings(&["\\Seen", "\\Flagged"]));
        assert!(data.silent);
        assert_eq!(data.sequence_set.ranges[0], range(1.0, 5.0));
    }

    #[test]
    fn parses_remove_flags_with_a_single_flag_without_parens() {
        let data = store("2 -FLAGS \\Deleted");
        assert_eq!(data.operation.as_str(), "-FLAGS");
        assert_eq!(data.flags, strings(&["\\Deleted"]));
    }

    #[test]
    fn parses_flags_silent() {
        let data = store("3 FLAGS.SILENT (\\Answered)");
        assert_eq!(data.operation.as_str(), "FLAGS.SILENT");
        assert!(data.silent);
    }

    #[test]
    fn parses_remove_flags_silent() {
        assert!(store("1 -FLAGS.SILENT (\\Seen)").silent);
    }

    #[test]
    fn parses_empty_flags_list() {
        assert!(store("1 FLAGS ()").flags.is_empty());
    }

    #[test]
    fn store_fails_on_invalid_sequence_set() {
        let r = parse_store(&mut ctx("abc FLAGS (\\Seen)"));
        assert!(!r.success);
        assert!(r.error.unwrap().contains("sequence set"));
    }

    #[test]
    fn store_fails_on_invalid_item_name() {
        let r = parse_store(&mut ctx("1 BADOP (\\Seen)"));
        assert!(!r.success);
        assert!(r.error.unwrap().contains("Invalid store operation"));
    }

    #[test]
    fn store_fails_on_missing_item_name() {
        assert!(!parse_store(&mut ctx("1 ")).success);
    }

    #[test]
    fn store_fails_when_flag_is_missing_in_parenthesized_list() {
        let r = parse_store(&mut ctx("1 FLAGS ({)"));
        assert!(!r.success);
        assert!(r.error.unwrap().contains("Invalid flag"));
    }

    #[test]
    fn parses_a_simple_copy_command() {
        let data = copy("1 INBOX");
        assert_eq!(data.mailbox, "INBOX");
        assert_eq!(data.sequence_set.ranges[0], single(1.0));
    }

    #[test]
    fn parses_a_range_copy() {
        let data = copy("1:10 Trash");
        assert_eq!(data.sequence_set.ranges[0], range(1.0, 10.0));
        assert_eq!(data.mailbox, "Trash");
    }

    #[test]
    fn parses_copy_with_quoted_mailbox_name() {
        assert_eq!(copy("1 \"My Mailbox\"").mailbox, "My Mailbox");
    }

    #[test]
    fn copy_fails_on_invalid_sequence_set() {
        let r = parse_copy(&mut ctx("abc INBOX"));
        assert!(!r.success);
        assert!(r.error.unwrap().contains("sequence set"));
    }

    #[test]
    fn copy_fails_on_missing_mailbox_name() {
        let r = parse_copy(&mut ctx("1 "));
        assert!(!r.success);
        assert!(r.error.unwrap().contains("mailbox"));
    }
}

// ---------------------------------------------------------------------------
// fetch-parsers.test.ts
// ---------------------------------------------------------------------------

mod fetch {
    use super::*;

    fn items(args: &str) -> Vec<FetchDataItem> {
        match request(&format!("A001 FETCH {args}")) {
            ImapRequest::Fetch(data) => data.data_items,
            other => panic!("FETCH parse failed: got {}", other.type_name()),
        }
    }

    fn types(args: &str) -> Vec<&'static str> {
        items(args).iter().map(FetchDataItem::type_name).collect()
    }

    fn body(args: &str) -> BodyFetch {
        match items(args).remove(0) {
            FetchDataItem::Body(body) => body,
            other => panic!("expected BODY, got {}", other.type_name()),
        }
    }

    #[test]
    fn parses_each_simple_item_as_a_single_item() {
        for item in ["ENVELOPE", "FLAGS", "INTERNALDATE", "RFC822", "RFC822.HEADER", "RFC822.SIZE", "RFC822.TEXT", "UID", "BODYSTRUCTURE"] {
            assert_eq!(types(&format!("1 {item}")), vec![item]);
        }
    }

    #[test]
    fn parses_single_message_number() {
        assert_eq!(types("1 FLAGS")[0], "FLAGS");
    }

    #[test]
    fn parses_range_sequence() {
        assert_eq!(types("1:10 FLAGS")[0], "FLAGS");
    }

    #[test]
    fn parses_wildcard_sequence() {
        assert_eq!(types("1:* FLAGS")[0], "FLAGS");
    }

    #[test]
    fn parses_a_list_with_one_item() {
        assert_eq!(types("1 (FLAGS)"), vec!["FLAGS"]);
    }

    #[test]
    fn parses_a_list_with_two_items() {
        assert_eq!(types("1 (FLAGS UID)"), vec!["FLAGS", "UID"]);
    }

    #[test]
    fn parses_a_list_with_three_items() {
        assert_eq!(types("1 (FLAGS UID ENVELOPE)"), vec!["FLAGS", "UID", "ENVELOPE"]);
    }

    #[test]
    fn parses_size_and_internaldate() {
        assert_eq!(types("1 (RFC822.SIZE INTERNALDATE)"), vec!["RFC822.SIZE", "INTERNALDATE"]);
    }

    #[test]
    fn parses_bare_body_as_the_non_extensible_bodystructure_not_content() {
        // RFC 3501 §6.4.5: `BODY` with no section and no .PEEK is a structure
        // line, not BODY[] content.
        assert_eq!(items("1 BODY"), vec![FetchDataItem::BodyStructure { extensible: false }]);
    }

    #[test]
    fn parses_body_brackets_as_full_body_fetch() {
        assert_eq!(body("1 BODY[]").section, BodySection::Full);
    }

    #[test]
    fn parses_body_peek_as_peek_body_fetch() {
        assert!(body("1 BODY.PEEK[]").peek);
    }

    #[test]
    fn parses_body_header() {
        assert_eq!(body("1 BODY[HEADER]").section, BodySection::Header);
    }

    #[test]
    fn parses_body_text() {
        assert_eq!(body("1 BODY[TEXT]").section, BodySection::Text);
    }

    #[test]
    fn parses_body_header_fields() {
        let BodySection::HeaderFields { fields, .. } = body("1 BODY[HEADER.FIELDS (FROM TO SUBJECT DATE)]").section else {
            panic!("expected HEADER_FIELDS section");
        };
        for field in ["FROM", "TO", "SUBJECT", "DATE"] {
            assert!(fields.contains(&field.to_string()));
        }
    }

    #[test]
    fn parses_body_peek_header_fields() {
        let fetch = body("1 BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE)]");
        assert!(fetch.peek);
        assert_eq!(fetch.section.type_name(), "HEADER_FIELDS");
    }

    #[test]
    fn parses_body_1_for_first_mime_part() {
        assert_eq!(body("1 BODY[1]").section.type_name(), "MIME_PART");
    }

    #[test]
    fn parses_body_with_partial_range() {
        assert!(body("1 BODY[]<0.2048>").partial.is_some());
    }

    #[test]
    fn parses_the_typical_list_view_fetch() {
        let parsed = items("1:* (UID FLAGS ENVELOPE BODYSTRUCTURE)");
        let names: Vec<_> = parsed.iter().map(FetchDataItem::type_name).collect();
        for name in ["UID", "FLAGS", "ENVELOPE", "BODYSTRUCTURE"] {
            assert!(names.contains(&name));
        }
        // The BODYSTRUCTURE token is the extensible form.
        assert!(parsed.contains(&FetchDataItem::BodyStructure { extensible: true }));
    }

    #[test]
    fn parses_the_full_message_fetch() {
        let names = types("1 (UID BODY.PEEK[])");
        assert!(names.contains(&"UID") && names.contains(&"BODY"));
    }

    #[test]
    fn parses_the_metadata_fetch() {
        let names = types("1:* (UID RFC822.SIZE INTERNALDATE FLAGS)");
        for name in ["UID", "RFC822.SIZE", "INTERNALDATE", "FLAGS"] {
            assert!(names.contains(&name));
        }
    }

    #[test]
    fn expands_fast() {
        assert_eq!(types("2 FAST"), vec!["FLAGS", "INTERNALDATE", "RFC822.SIZE"]);
    }

    #[test]
    fn expands_all() {
        assert_eq!(types("2 ALL"), vec!["FLAGS", "INTERNALDATE", "RFC822.SIZE", "ENVELOPE"]);
    }

    #[test]
    fn expands_full_to_the_non_extensible_structure() {
        // FULL's BODY is the non-extensible BODYSTRUCTURE, not BODY[]:
        // `FETCH 1:* FULL` must not stream every message's body.
        let parsed = items("2 FULL");
        let names: Vec<_> = parsed.iter().map(FetchDataItem::type_name).collect();
        assert_eq!(names, vec!["FLAGS", "INTERNALDATE", "RFC822.SIZE", "ENVELOPE", "BODYSTRUCTURE"]);
        assert!(!names.contains(&"BODY"));
        assert!(parsed.contains(&FetchDataItem::BodyStructure { extensible: false }));
    }

    #[test]
    fn macros_are_case_insensitive() {
        assert_eq!(items("2 fast").len(), 3);
        assert_eq!(items("2 all").len(), 4);
        assert_eq!(items("2 full").len(), 5);
    }

    #[test]
    fn expands_macros_under_uid_fetch_too() {
        let ImapRequest::Uid(uid) = request("A001 UID FETCH 2 FAST") else {
            panic!("UID FETCH parse failed");
        };
        let ImapRequest::Fetch(inner) = uid.request() else {
            panic!("UID FETCH parse failed");
        };
        let names: Vec<_> = inner.data_items.iter().map(FetchDataItem::type_name).collect();
        assert_eq!(names, vec!["FLAGS", "INTERNALDATE", "RFC822.SIZE"]);
    }

    #[test]
    fn does_not_treat_a_non_macro_single_item_as_a_macro() {
        assert_eq!(types("1 FLAGS"), vec!["FLAGS"]);
        assert_eq!(types("1 RFC822.SIZE"), vec!["RFC822.SIZE"]);
        assert_eq!(types("1 BODY[]"), vec!["BODY"]);
    }
}

// ---------------------------------------------------------------------------
// mailbox-parsers.test.ts
// ---------------------------------------------------------------------------

mod mailbox {
    use super::*;

    fn select(line: &str) -> String {
        match request(line) {
            ImapRequest::Select(data) | ImapRequest::Examine(data) => data.mailbox,
            other => panic!("Expected SELECT or EXAMINE, got {}", other.type_name()),
        }
    }

    fn status(line: &str) -> StatusRequest {
        match request(line) {
            ImapRequest::Status(data) => data,
            other => panic!("Expected STATUS request type, got {}", other.type_name()),
        }
    }

    fn item_names(data: &StatusRequest) -> Vec<&'static str> {
        data.items.iter().map(|item| item.as_str()).collect()
    }

    fn list(line: &str) -> ListRequest {
        match request(line) {
            ImapRequest::List(data) | ImapRequest::Lsub(data) => data,
            other => panic!("Expected LIST request type, got {}", other.type_name()),
        }
    }

    #[test]
    fn parses_select_with_simple_mailbox() {
        let result = command("A001 SELECT INBOX");
        assert!(result.success);
        let parsed = result.value.unwrap();
        assert_eq!(parsed.tag, "A001");
        assert_eq!(parsed.request.type_name(), "SELECT");
        assert_eq!(select("A001 SELECT INBOX"), "INBOX");
    }

    #[test]
    fn parses_select_with_quoted_mailbox_containing_spaces() {
        assert_eq!(select("A001 SELECT \"Sent Items\""), "Sent Items");
    }

    #[test]
    fn parses_select_with_nested_hierarchy() {
        assert_eq!(select("A001 SELECT \"INBOX/Work/Projects\""), "INBOX/Work/Projects");
    }

    #[test]
    fn fails_select_without_mailbox() {
        assert!(!command("A001 SELECT").success);
    }

    #[test]
    fn parses_examine_with_simple_mailbox() {
        assert_eq!(request("A001 EXAMINE INBOX").type_name(), "EXAMINE");
        assert_eq!(select("A001 EXAMINE INBOX"), "INBOX");
    }

    #[test]
    fn parses_examine_with_quoted_mailbox() {
        assert_eq!(request("A001 EXAMINE \"Archive/2025\"").type_name(), "EXAMINE");
        assert_eq!(select("A001 EXAMINE \"Archive/2025\""), "Archive/2025");
    }

    #[test]
    fn parses_status_with_single_item() {
        let data = status("A001 STATUS INBOX (MESSAGES)");
        assert_eq!(data.mailbox, "INBOX");
        assert_eq!(item_names(&data), vec!["MESSAGES"]);
    }

    #[test]
    fn parses_status_with_multiple_items() {
        let data = status("A001 STATUS INBOX (MESSAGES RECENT UIDNEXT UIDVALIDITY UNSEEN)");
        assert_eq!(item_names(&data), vec!["MESSAGES", "RECENT", "UIDNEXT", "UIDVALIDITY", "UNSEEN"]);
    }

    #[test]
    fn parses_status_with_quoted_mailbox() {
        assert_eq!(status("A001 STATUS \"Sent Items\" (MESSAGES UNSEEN)").mailbox, "Sent Items");
    }

    #[test]
    fn parses_status_with_lowercase_items() {
        assert_eq!(item_names(&status("A001 STATUS INBOX (messages unseen)")), vec!["MESSAGES", "UNSEEN"]);
    }

    #[test]
    fn fails_status_with_unknown_item() {
        assert!(!command("A001 STATUS INBOX (MESSAGES INVALID)").success);
    }

    #[test]
    fn fails_status_without_parentheses() {
        assert!(!command("A001 STATUS INBOX MESSAGES").success);
    }

    #[test]
    fn parses_create_with_simple_mailbox() {
        assert_eq!(request("A001 CREATE \"New Folder\""), ImapRequest::Create { mailbox: "New Folder".into() });
    }

    #[test]
    fn parses_create_with_nested_hierarchy() {
        assert_eq!(
            request("A001 CREATE \"INBOX/Work/Projects/2026\""),
            ImapRequest::Create { mailbox: "INBOX/Work/Projects/2026".into() }
        );
    }

    #[test]
    fn parses_create_with_unquoted_mailbox() {
        assert_eq!(request("A001 CREATE Drafts"), ImapRequest::Create { mailbox: "Drafts".into() });
    }

    #[test]
    fn parses_delete_with_simple_mailbox() {
        assert_eq!(request("A001 DELETE \"Old Folder\""), ImapRequest::Delete { mailbox: "Old Folder".into() });
    }

    #[test]
    fn parses_delete_with_unquoted_mailbox() {
        assert_eq!(request("A001 DELETE Trash"), ImapRequest::Delete { mailbox: "Trash".into() });
    }

    #[test]
    fn parses_rename_with_quoted_names() {
        assert_eq!(
            request("A001 RENAME \"Old Name\" \"New Name\""),
            ImapRequest::Rename { old_name: "Old Name".into(), new_name: "New Name".into() }
        );
    }

    #[test]
    fn parses_rename_with_unquoted_names() {
        assert_eq!(
            request("A001 RENAME OldFolder NewFolder"),
            ImapRequest::Rename { old_name: "OldFolder".into(), new_name: "NewFolder".into() }
        );
    }

    #[test]
    fn fails_rename_with_missing_new_name() {
        assert!(!command("A001 RENAME \"Old Name\"").success);
    }

    #[test]
    fn parses_list_with_empty_reference_and_wildcard() {
        assert_eq!(request("A001 LIST \"\" *").type_name(), "LIST");
        assert_eq!(list("A001 LIST \"\" *"), ListRequest { reference: "".into(), pattern: "*".into() });
    }

    #[test]
    fn parses_list_with_reference_path() {
        assert_eq!(list("A001 LIST \"INBOX/\" *"), ListRequest { reference: "INBOX/".into(), pattern: "*".into() });
    }

    #[test]
    fn parses_list_with_percent_wildcard() {
        assert_eq!(list("A001 LIST \"\" %").pattern, "%");
    }

    #[test]
    fn parses_list_with_specific_pattern() {
        assert_eq!(list("A001 LIST \"\" \"INBOX\""), ListRequest { reference: "".into(), pattern: "INBOX".into() });
    }

    #[test]
    fn parses_lsub_command() {
        assert_eq!(request("A001 LSUB \"\" *").type_name(), "LSUB");
        assert_eq!(list("A001 LSUB \"\" *"), ListRequest { reference: "".into(), pattern: "*".into() });
    }

    #[test]
    fn keeps_a_trailing_wildcard_after_a_hierarchy_path() {
        assert_eq!(list("A001 LIST \"\" \"INBOX/%\"").pattern, "INBOX/%");
    }

    #[test]
    fn keeps_a_leading_wildcard_followed_by_a_path() {
        assert_eq!(list("A001 LIST \"\" \"%/accounts\"").pattern, "%/accounts");
    }

    #[test]
    fn keeps_a_trailing_wildcard_on_an_unquoted_pattern() {
        assert_eq!(list("A001 LIST \"\" INBOX*").pattern, "INBOX*");
    }

    #[test]
    fn parses_subscribe_with_quoted_mailbox() {
        assert_eq!(
            request("A001 SUBSCRIBE \"INBOX/Notifications\""),
            ImapRequest::Subscribe { mailbox: "INBOX/Notifications".into() }
        );
    }

    #[test]
    fn parses_subscribe_with_unquoted_mailbox() {
        assert_eq!(request("A001 SUBSCRIBE INBOX").type_name(), "SUBSCRIBE");
    }

    #[test]
    fn parses_unsubscribe_with_quoted_mailbox() {
        assert_eq!(
            request("A001 UNSUBSCRIBE \"Old Subscription\""),
            ImapRequest::Unsubscribe { mailbox: "Old Subscription".into() }
        );
    }

    #[test]
    fn parses_unsubscribe_with_unquoted_mailbox() {
        assert_eq!(request("A001 UNSUBSCRIBE Drafts").type_name(), "UNSUBSCRIBE");
    }
}

// ---------------------------------------------------------------------------
// search-parsers.test.ts
// ---------------------------------------------------------------------------

mod search {
    use super::*;
    use SearchCriterion as C;

    fn criteria(input: &str) -> Vec<SearchCriterion> {
        let result = parse_search_criteria(&mut ctx(input)).expect("the parser threw");
        assert!(result.success, "{input}: {:?}", result.error);
        result.value.unwrap()
    }

    fn type_names(input: &str) -> Vec<&'static str> {
        criteria(input).iter().map(SearchCriterion::type_name).collect()
    }

    fn sequence(ranges: Vec<SequenceRange>) -> SequenceSet {
        SequenceSet::new(ranges)
    }

    #[test]
    fn parses_each_flag_key() {
        let keys = [
            ("ALL", C::All),
            ("ANSWERED", C::Answered),
            ("DELETED", C::Deleted),
            ("FLAGGED", C::Flagged),
            ("NEW", C::New),
            ("OLD", C::Old),
            ("RECENT", C::Recent),
            ("SEEN", C::Seen),
            ("UNANSWERED", C::Unanswered),
            ("UNDELETED", C::Undeleted),
            ("UNFLAGGED", C::Unflagged),
            ("UNSEEN", C::Unseen),
            ("DRAFT", C::Draft),
            ("UNDRAFT", C::Undraft),
        ];
        for (name, expected) in keys {
            assert_eq!(criteria(name), vec![expected], "{name}");
        }
    }

    #[test]
    fn parses_each_date_key_with_a_date() {
        for (input, name) in [
            ("BEFORE 01-Jan-2024", "BEFORE"),
            ("ON 15-Mar-2024", "ON"),
            ("SINCE 01-Jan-2024", "SINCE"),
            ("SENTBEFORE 01-Jan-2024", "SENTBEFORE"),
            ("SENTON 15-Mar-2024", "SENTON"),
            ("SENTSINCE 01-Jun-2024", "SENTSINCE"),
        ] {
            assert_eq!(type_names(input)[0], name);
        }
    }

    #[test]
    fn parses_each_text_key_with_a_value() {
        assert_eq!(criteria("FROM alice")[0], C::From("alice".into()));
        assert_eq!(criteria("TO bob")[0], C::To("bob".into()));
        assert_eq!(criteria("CC carol")[0], C::Cc("carol".into()));
        assert_eq!(criteria("BCC dave")[0], C::Bcc("dave".into()));
        assert_eq!(criteria("SUBJECT hello")[0], C::Subject("hello".into()));
        assert_eq!(criteria("BODY world")[0], C::Body("world".into()));
        assert_eq!(criteria("TEXT search_term")[0], C::Text("search_term".into()));
    }

    #[test]
    fn parses_header_with_field_and_value() {
        assert_eq!(
            criteria("HEADER X-Custom value123")[0],
            C::Header { field: "X-Custom".into(), value: "value123".into() }
        );
    }

    // RFC 3501 §6.4.4 / §9: search-key arguments are astrings, and every MUA
    // quotes them.
    #[test]
    fn parses_subject_with_a_quoted_single_word_value() {
        assert_eq!(criteria("SUBJECT \"order\"")[0], C::Subject("order".into()));
    }

    #[test]
    fn parses_subject_with_a_quoted_multi_word_phrase() {
        assert_eq!(criteria("SUBJECT \"order confirmation\"")[0], C::Subject("order confirmation".into()));
    }

    #[test]
    fn parses_from_with_a_quoted_value() {
        assert_eq!(criteria("FROM \"alice@example.com\"")[0], C::From("alice@example.com".into()));
    }

    #[test]
    fn parses_body_and_text_with_quoted_phrases() {
        assert_eq!(criteria("BODY \"two words\"")[0], C::Body("two words".into()));
        assert_eq!(criteria("TEXT \"two words\"")[0], C::Text("two words".into()));
    }

    #[test]
    fn parses_header_with_quoted_field_and_quoted_value() {
        assert_eq!(
            criteria("HEADER \"Message-Id\" \"<abc@host>\"")[0],
            C::Header { field: "Message-Id".into(), value: "<abc@host>".into() }
        );
    }

    #[test]
    fn parses_larger_and_smaller_with_a_size() {
        assert_eq!(criteria("LARGER 1000")[0], C::Larger(1000.0));
        assert_eq!(criteria("SMALLER 500")[0], C::Smaller(500.0));
    }

    #[test]
    fn parses_uid_with_a_number_a_range_and_a_wildcard() {
        for input in ["UID 42", "UID 1:100", "UID 1:*"] {
            assert_eq!(type_names(input)[0], "UID");
        }
    }

    #[test]
    fn parses_a_bare_range_as_seq_not_uid() {
        assert_eq!(criteria("1:3")[0], C::Seq(sequence(vec![range(1.0, 3.0)])));
    }

    #[test]
    fn parses_a_bare_single_number_as_seq() {
        assert_eq!(type_names("11324")[0], "SEQ");
    }

    #[test]
    fn parses_a_bare_comma_set_as_seq() {
        assert_eq!(type_names("2,4")[0], "SEQ");
    }

    #[test]
    fn combines_a_flag_key_with_a_bare_seq_set() {
        assert_eq!(type_names("SEEN 1:3"), vec!["SEEN", "SEQ"]);
    }

    #[test]
    fn parses_not_with_single_criterion() {
        assert_eq!(criteria("NOT SEEN")[0], C::not(C::Seen));
    }

    #[test]
    fn parses_not_deleted() {
        assert_eq!(criteria("NOT DELETED")[0], C::not(C::Deleted));
    }

    #[test]
    fn parses_or_with_two_criteria() {
        assert_eq!(criteria("OR SEEN ANSWERED")[0], C::or(C::Seen, C::Answered));
    }

    #[test]
    fn parses_or_flagged_deleted() {
        let parsed = criteria("OR FLAGGED DELETED");
        let C::Or(left, right) = &parsed[0] else {
            panic!("expected OR");
        };
        assert_eq!(left.type_name(), "FLAGGED");
        assert_eq!(right.type_name(), "DELETED");
    }

    #[test]
    fn parses_not_seen_followed_by_another_anded_key() {
        let parsed = criteria("NOT SEEN SINCE 1-Jan-2026");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], C::not(C::Seen));
        assert_eq!(parsed[1].type_name(), "SINCE");
    }

    #[test]
    fn not_bounds_to_one_key() {
        assert_eq!(criteria("NOT SEEN FROM bob"), vec![C::not(C::Seen), C::From("bob".into())]);
    }

    #[test]
    fn parses_or_followed_by_another_anded_key() {
        assert_eq!(criteria("OR SEEN ANSWERED FROM bob"), vec![C::or(C::Seen, C::Answered), C::From("bob".into())]);
    }

    #[test]
    fn parses_an_operator_that_appears_after_other_keys() {
        let parsed = criteria("SINCE 1-Jan-2026 NOT SEEN");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].type_name(), "SINCE");
        assert_eq!(parsed[1], C::not(C::Seen));
    }

    #[test]
    fn parses_nested_not_not_seen() {
        assert_eq!(criteria("NOT NOT SEEN")[0], C::not(C::not(C::Seen)));
    }

    #[test]
    fn parses_or_with_a_not_operand() {
        assert_eq!(criteria("OR NOT SEEN FLAGGED"), vec![C::or(C::not(C::Seen), C::Flagged)]);
    }

    #[test]
    fn parses_all_seen() {
        assert_eq!(criteria("ALL SEEN"), vec![C::All, C::Seen]);
    }

    #[test]
    fn parses_three_criteria() {
        let parsed = criteria("UNSEEN FLAGGED SINCE 01-Jan-2024");
        assert!(parsed.len() >= 3);
        assert_eq!(parsed[0], C::Unseen);
        assert_eq!(parsed[1], C::Flagged);
        assert_eq!(parsed[2].type_name(), "SINCE");
    }

    #[test]
    fn parses_from_alice_subject_hello() {
        assert_eq!(criteria("FROM alice SUBJECT hello"), vec![C::From("alice".into()), C::Subject("hello".into())]);
    }

    #[test]
    fn returns_empty_criteria_for_empty_string() {
        assert!(criteria("").is_empty());
    }

    // An atom that matches no known search key is skipped and the loop
    // continues: the parser has to advance past it.
    #[test]
    fn skips_an_unknown_atom_and_continues_to_the_next_key() {
        assert_eq!(criteria("FOO SEEN"), vec![C::Seen]);
    }

    #[test]
    fn returns_empty_for_a_sole_unknown_atom() {
        assert!(criteria("FOO").is_empty());
    }

    fn searched(input: &str) -> Vec<SearchCriterion> {
        let result = parse_search(&mut ctx(input)).expect("the parser threw");
        assert!(result.success);
        match result.value.unwrap() {
            ImapRequest::Search(data) => data.criteria,
            other => panic!("expected SEARCH, got {}", other.type_name()),
        }
    }

    #[test]
    fn wraps_criteria_in_a_search_request() {
        assert_eq!(searched("ALL"), vec![C::All]);
    }

    #[test]
    fn parses_search_unseen_flagged() {
        assert_eq!(searched("UNSEEN FLAGGED"), vec![C::Unseen, C::Flagged]);
    }

    #[test]
    fn parses_search_from_alice() {
        assert_eq!(searched("FROM alice")[0], C::From("alice".into()));
    }

    #[test]
    fn empty_search_input_is_success_with_no_criteria() {
        assert!(searched("").is_empty());
    }
}

// ---------------------------------------------------------------------------
// security.test.ts
// ---------------------------------------------------------------------------

mod security {
    use super::*;

    fn login(input: &str) -> ParseResult<ImapRequest> {
        parse_login(&mut ctx(input))
    }

    fn login_data(input: &str) -> LoginRequest {
        match login(input).into_result() {
            Ok(ImapRequest::Login(data)) => data,
            other => panic!("expected LOGIN, got {other:?}"),
        }
    }

    fn authenticate_data(input: &str) -> AuthenticateRequest {
        match parse_authenticate(&mut ctx(input)).into_result() {
            Ok(ImapRequest::Authenticate(data)) => data,
            other => panic!("expected AUTHENTICATE, got {other:?}"),
        }
    }

    #[test]
    fn login_with_simple_credentials() {
        let data = login_data("user password");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("user", "password"));
    }

    #[test]
    fn login_with_quoted_username() {
        let data = login_data("\"user@example.com\" password");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("user@example.com", "password"));
    }

    #[test]
    fn login_with_quoted_password() {
        let data = login_data("user \"my password\"");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("user", "my password"));
    }

    #[test]
    fn login_with_both_quoted() {
        let data = login_data("\"user@example.com\" \"pass word\"");
        assert_eq!((data.username.as_str(), data.password.as_str()), ("user@example.com", "pass word"));
    }

    #[test]
    fn login_with_special_characters_in_quoted_password() {
        assert_eq!(login_data("user \"p@ss\\\"w0rd!\"").password, "p@ss\"w0rd!");
    }

    #[test]
    fn login_with_escaped_backslash_in_password() {
        assert_eq!(login_data("user \"pass\\\\word\"").password, "pass\\word");
    }

    #[test]
    fn login_fails_on_missing_username() {
        let result = login("");
        assert!(!result.success);
        assert!(result.error.unwrap().contains("username"));
    }

    #[test]
    fn login_fails_on_missing_password() {
        let result = login("user");
        assert!(!result.success);
        assert!(result.error.unwrap().contains("password"));
    }

    #[test]
    fn login_fails_on_unterminated_quoted_username() {
        assert!(!login("\"unterminated password").success);
    }

    #[test]
    fn authenticate_plain() {
        let data = authenticate_data("PLAIN");
        assert_eq!(data.mechanism, "PLAIN");
        assert_eq!(data.initial_response, None);
    }

    #[test]
    fn authenticate_plain_with_initial_response() {
        let data = authenticate_data("PLAIN AGFsaWNlAHNlY3JldA==");
        assert_eq!(data.mechanism, "PLAIN");
        assert_eq!(data.initial_response.as_deref(), Some("AGFsaWNlAHNlY3JldA=="));
    }

    #[test]
    fn authenticate_with_quoted_mechanism() {
        assert_eq!(authenticate_data("\"PLAIN\"").mechanism, "PLAIN");
    }

    #[test]
    fn authenticate_fails_on_missing_mechanism() {
        let result = parse_authenticate(&mut ctx(""));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("mechanism"));
    }

    fn append_with(input: &str, literals: Vec<Literal>) -> ParseResult<ImapRequest> {
        parse_append(&mut ctx_with(input, literals))
    }

    fn append_data(input: &str, message: &str) -> AppendRequest {
        match append_with(input, vec![spooled(message).into()]).into_result() {
            Ok(ImapRequest::Append(data)) => data,
            other => panic!("expected APPEND, got {other:?}"),
        }
    }

    #[test]
    fn append_with_minimal_args() {
        let message = "From: test@example.com\r\nSubject: Test\r\n\r\nBody";
        let data = append_data(&format!("INBOX {{{}}}", message.len()), message);
        assert_eq!(data.mailbox, "INBOX");
        assert_eq!(data.message, spooled(message));
        assert_eq!(data.flags, None);
        assert_eq!(data.date, None);
    }

    #[test]
    fn append_with_quoted_mailbox() {
        assert_eq!(append_data("\"Sent Items\" {12}", "Test message").mailbox, "Sent Items");
    }

    #[test]
    fn append_with_keyword_flags() {
        assert_eq!(append_data("INBOX (MyLabel Important) {4}", "Test").flags, Some(strings(&["MyLabel", "Important"])));
    }

    #[test]
    fn append_handles_empty_flags_list() {
        assert_eq!(append_data("INBOX () {4}", "Test").flags, Some(Vec::new()));
    }

    #[test]
    fn append_with_date() {
        let data = append_data("INBOX \"25-Feb-2026 12:34:56 +0000\" {4}", "Test");
        assert_eq!(data.date.as_deref(), Some("25-Feb-2026 12:34:56 +0000"));
    }

    #[test]
    fn append_with_keyword_flags_and_date() {
        let data = append_data("INBOX (Important) \"25-Feb-2026 12:34:56 +0000\" {4}", "Test");
        assert_eq!(data.flags, Some(strings(&["Important"])));
        assert_eq!(data.date.as_deref(), Some("25-Feb-2026 12:34:56 +0000"));
    }

    #[test]
    fn append_fails_on_missing_mailbox() {
        let result = parse_append(&mut ctx(""));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("mailbox"));
    }

    #[test]
    fn append_fails_on_missing_literal() {
        let result = parse_append(&mut ctx("INBOX"));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("literal"));
    }

    #[test]
    fn append_fails_on_malformed_literal_size() {
        let result = parse_append(&mut ctx("INBOX {abc}\r\ntest"));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("size"));
    }

    #[test]
    fn append_fails_on_missing_closing_brace() {
        let result = parse_append(&mut ctx("INBOX {100"));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("literal"));
    }

    #[test]
    fn append_handles_large_literal_size_safely() {
        let message = "X".repeat(1000);
        assert_eq!(append_data("INBOX {1000}", &message).message.size, 1000);
    }

    #[test]
    fn append_handles_zero_length_message() {
        assert_eq!(append_data("INBOX {0}", "").message, spooled(""));
    }

    fn mailbox_of(result: ParseResult<ImapRequest>) -> String {
        match result.into_result() {
            Ok(
                ImapRequest::Select(SelectRequest { mailbox })
                | ImapRequest::Examine(SelectRequest { mailbox })
                | ImapRequest::Create { mailbox }
                | ImapRequest::Delete { mailbox },
            ) => mailbox,
            other => panic!("expected a mailbox command, got {other:?}"),
        }
    }

    #[test]
    fn select_with_simple_mailbox() {
        let result = parse_select(false, &mut ctx("INBOX"));
        assert_eq!(result.value.as_ref().unwrap().type_name(), "SELECT");
        assert_eq!(mailbox_of(result), "INBOX");
    }

    #[test]
    fn examine_is_read_only() {
        let result = parse_select(true, &mut ctx("INBOX"));
        assert!(result.success);
        assert_eq!(result.value.unwrap().type_name(), "EXAMINE");
    }

    #[test]
    fn select_with_quoted_mailbox() {
        assert_eq!(mailbox_of(parse_select(false, &mut ctx("\"Sent Items\""))), "Sent Items");
    }

    #[test]
    fn select_with_mailbox_containing_special_chars() {
        assert_eq!(mailbox_of(parse_select(false, &mut ctx("\"INBOX/Folder\\\"Name\""))), "INBOX/Folder\"Name");
    }

    #[test]
    fn select_fails_on_missing_mailbox() {
        assert!(!parse_select(false, &mut ctx("")).success);
    }

    #[test]
    fn create_with_simple_mailbox() {
        let result = parse_create(&mut ctx("NewFolder"));
        assert_eq!(result.value.as_ref().unwrap().type_name(), "CREATE");
        assert_eq!(mailbox_of(result), "NewFolder");
    }

    #[test]
    fn create_with_nested_hierarchy() {
        assert_eq!(mailbox_of(parse_create(&mut ctx("\"INBOX/Subfolder/Deep\""))), "INBOX/Subfolder/Deep");
    }

    #[test]
    fn create_fails_on_missing_mailbox() {
        assert!(!parse_create(&mut ctx("")).success);
    }

    #[test]
    fn delete_with_simple_mailbox() {
        let result = parse_delete(&mut ctx("OldFolder"));
        assert_eq!(result.value.as_ref().unwrap().type_name(), "DELETE");
        assert_eq!(mailbox_of(result), "OldFolder");
    }

    #[test]
    fn delete_with_quoted_mailbox() {
        assert_eq!(mailbox_of(parse_delete(&mut ctx("\"Folder To Delete\""))), "Folder To Delete");
    }

    #[test]
    fn delete_fails_on_missing_mailbox() {
        assert!(!parse_delete(&mut ctx("")).success);
    }

    #[test]
    fn rename_with_simple_names() {
        assert_eq!(
            parse_rename(&mut ctx("OldName NewName")).value,
            Some(ImapRequest::Rename { old_name: "OldName".into(), new_name: "NewName".into() })
        );
    }

    #[test]
    fn rename_with_quoted_names() {
        assert_eq!(
            parse_rename(&mut ctx("\"Old Folder\" \"New Folder\"")).value,
            Some(ImapRequest::Rename { old_name: "Old Folder".into(), new_name: "New Folder".into() })
        );
    }

    #[test]
    fn rename_fails_on_missing_new_name() {
        let result = parse_rename(&mut ctx("OldName"));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("new"));
    }

    #[test]
    fn rename_fails_on_missing_old_name() {
        let result = parse_rename(&mut ctx(""));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("existing"));
    }

    fn list_data(command: &str, input: &str) -> (&'static str, ListRequest) {
        match parse_list(command, &mut ctx(input)).into_result() {
            Ok(ImapRequest::List(data)) => ("LIST", data),
            Ok(ImapRequest::Lsub(data)) => ("LSUB", data),
            other => panic!("expected LIST, got {other:?}"),
        }
    }

    #[test]
    fn list_with_wildcard() {
        let (name, data) = list_data("LIST", "\"\" *");
        assert_eq!(name, "LIST");
        assert_eq!(data, ListRequest { reference: "".into(), pattern: "*".into() });
    }

    #[test]
    fn list_with_percent_wildcard() {
        assert_eq!(list_data("LIST", "\"\" %").1.pattern, "%");
    }

    #[test]
    fn list_with_reference() {
        assert_eq!(list_data("LIST", "\"INBOX\" *").1.reference, "INBOX");
    }

    #[test]
    fn list_parses_lsub_command() {
        assert_eq!(list_data("LSUB", "\"\" *").0, "LSUB");
    }

    #[test]
    fn list_fails_on_missing_reference() {
        assert!(!parse_list("LIST", &mut ctx("")).success);
    }

    fn status_data(input: &str) -> StatusRequest {
        match parse_status(&mut ctx(input)).into_result() {
            Ok(ImapRequest::Status(data)) => data,
            other => panic!("expected STATUS, got {other:?}"),
        }
    }

    fn names(data: &StatusRequest) -> Vec<&'static str> {
        data.items.iter().map(|item| item.as_str()).collect()
    }

    #[test]
    fn status_with_single_item() {
        let data = status_data("INBOX (MESSAGES)");
        assert_eq!(data.mailbox, "INBOX");
        assert_eq!(names(&data), vec!["MESSAGES"]);
    }

    #[test]
    fn status_with_multiple_items() {
        assert_eq!(names(&status_data("INBOX (MESSAGES RECENT UNSEEN)")), vec!["MESSAGES", "RECENT", "UNSEEN"]);
    }

    #[test]
    fn status_with_all_items() {
        assert_eq!(status_data("INBOX (MESSAGES RECENT UIDNEXT UIDVALIDITY UNSEEN)").items.len(), 5);
    }

    #[test]
    fn status_with_quoted_mailbox() {
        assert_eq!(status_data("\"Sent Items\" (MESSAGES)").mailbox, "Sent Items");
    }

    #[test]
    fn status_handles_lowercase_item_names() {
        assert_eq!(names(&status_data("INBOX (messages)")), vec!["MESSAGES"]);
    }

    #[test]
    fn status_fails_on_missing_opening_parenthesis() {
        let result = parse_status(&mut ctx("INBOX MESSAGES"));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("parenthesis"));
    }

    #[test]
    fn status_fails_on_unknown_status_item() {
        let result = parse_status(&mut ctx("INBOX (INVALID)"));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown"));
    }

    #[test]
    fn status_fails_on_missing_mailbox() {
        assert!(!parse_status(&mut ctx("")).success);
    }

    // --- literals (RFC 3501 §4.3 / RFC 7888 LITERAL+) ---

    #[test]
    fn literal_consumes_the_marker_and_returns_the_queued_payload() {
        let mut c = ctx_with("{5} rest", texts(&["admin"]));
        let result = parse_literal(&mut c);
        assert!(result.success);
        assert_eq!(result.value.as_deref(), Some("admin"));
        assert_eq!(c.rest(), " rest");
    }

    #[test]
    fn literal_accepts_the_non_synchronizing_form() {
        let result = parse_literal(&mut ctx_with("{5+}", texts(&["admin"])));
        assert!(result.success);
        assert_eq!(result.value.as_deref(), Some("admin"));
    }

    #[test]
    fn literal_takes_payloads_in_wire_order_across_chained_literals() {
        let mut c = ctx_with("{5} {8}", texts(&["admin", "password"]));
        assert_eq!(parse_literal(&mut c).value.as_deref(), Some("admin"));
        skip_whitespace(&mut c);
        assert_eq!(parse_literal(&mut c).value.as_deref(), Some("password"));
    }

    #[test]
    fn literal_returns_the_payload_byte_for_byte() {
        let result = parse_literal(&mut ctx_with("{9}", texts(&["p@ss \"w\\d"])));
        assert_eq!(result.value.as_deref(), Some("p@ss \"w\\d"));
    }

    #[test]
    fn literal_does_not_measure_the_payload_against_the_declared_size() {
        // `{5}` is five octets; "café" is four characters. The handler
        // already took the octets off the socket.
        let result = parse_literal(&mut ctx_with("{5}", texts(&["café"])));
        assert!(result.success);
        assert_eq!(result.value.as_deref(), Some("café"));
    }

    #[test]
    fn literal_fails_when_a_marker_has_no_queued_payload() {
        let result = parse_literal(&mut ctx_with("{5}", Vec::new()));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Missing literal payload"));
        assert_eq!(result.value, None);
    }

    #[test]
    fn literal_rejects_a_non_numeric_size() {
        let result = parse_literal(&mut ctx_with("{5x}", texts(&["admin"])));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid literal size"));
    }

    #[test]
    fn literal_rejects_an_unterminated_brace() {
        let result = parse_literal(&mut ctx_with("{5", texts(&["admin"])));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unterminated literal"));
    }

    fn login_of(result: ParseResult<ParsedCommand>) -> LoginRequest {
        match result.value.map(|parsed| parsed.request) {
            Some(ImapRequest::Login(data)) => data,
            other => panic!("Expected LOGIN, got {other:?}"),
        }
    }

    #[test]
    fn parses_literal_plus_login_with_both_credentials_as_literals() {
        let result = command_with("A1 LOGIN {5+} {8+}", &texts(&["admin", "password"]));
        assert!(result.success);
        assert_eq!(result.value.as_ref().unwrap().tag, "A1");
        let data = login_of(result);
        assert_eq!((data.username.as_str(), data.password.as_str()), ("admin", "password"));
    }

    #[test]
    fn does_not_drain_the_callers_literal_array() {
        // The handler asks "does this command parse yet?" before it commits
        // to dispatching, then parses again for real.
        let literals = texts(&["admin", "password"]);
        assert!(command_with("A1 LOGIN {5+} {8+}", &literals).success);
        assert_eq!(literals, texts(&["admin", "password"]));

        let second = command_with("A1 LOGIN {5+} {8+}", &literals);
        assert!(second.success);
        assert_eq!(login_of(second).password, "password");
    }

    #[test]
    fn reports_an_incomplete_command_as_unparsed_until_its_last_argument_lands() {
        assert!(!command_with("A1 LOGIN {7+}", &texts(&["admin\r\n"])).success);
        assert!(!command_with("A1 RENAME {7+}", &texts(&["Oldie\r\n"])).success);
        assert!(!command_with("A1 STATUS {5+}", &texts(&["INBOX"])).success);
        // …while a final literal completes it, CRLF-carrying payload included.
        assert!(command_with("a1 APPEND INBOX {13+}", &[spooled("Hello World\r\n").into()]).success);
        assert!(command_with("A1 SELECT {5+}", &texts(&["café"])).success);
        assert!(command_with("A1 LOGIN {5+} {8+}", &texts(&["admin", "password"])).success);
    }

    #[test]
    fn preserves_a_password_whose_octets_include_a_trailing_space() {
        assert_eq!(login_of(command_with("A1 LOGIN {5+} {9+}", &texts(&["admin", "password "]))).password, "password ");
    }

    #[test]
    fn preserves_a_multi_byte_password() {
        assert_eq!(login_of(command_with("A1 LOGIN {5+} {9+}", &texts(&["admin", "pässword"]))).password, "pässword");
    }

    #[test]
    fn mixes_a_literal_username_with_a_quoted_password() {
        let data = login_of(command_with("A1 LOGIN {5+} \"p@ss word\"", &texts(&["admin"])));
        assert_eq!((data.username.as_str(), data.password.as_str()), ("admin", "p@ss word"));
    }

    #[test]
    fn accepts_a_literal_mailbox_name_on_select() {
        assert_eq!(
            request_with("A1 SELECT {5+}", &texts(&["INBOX"])),
            ImapRequest::Select(SelectRequest { mailbox: "INBOX".into() })
        );
    }

    #[test]
    fn routes_the_append_message_through_the_same_literal_path() {
        let message = spooled("Subject: x\r\n\r\nbody\r\n");
        let ImapRequest::Append(data) = request_with("a1 APPEND INBOX (\\Seen) {13}", &[message.clone().into()]) else {
            panic!("Expected APPEND");
        };
        assert_eq!(data.mailbox, "INBOX");
        assert_eq!(data.flags, Some(strings(&["\\Seen"])));
        // The payload travels out of band, so no framing strip over the
        // command text can reach it.
        assert_eq!(data.message, message);
    }

    #[test]
    fn takes_the_message_only_as_a_spooled_literal() {
        let decoded = command_with("a1 APPEND INBOX {5}", &texts(&["hello"]));
        assert!(!decoded.success);
        assert!(decoded.error.unwrap().contains("Expected message literal"));
    }

    #[test]
    fn refuses_a_spooled_literal_where_an_argument_is_text() {
        assert!(!command_with("a1 APPEND {5} {5}", &[spooled("hello").into(), spooled("hello").into()]).success);
        assert!(!command_with("A1 SELECT {5}", &[spooled("INBOX").into()]).success);
    }

    #[test]
    fn rejects_an_append_whose_literal_payload_never_arrived() {
        let result = command_with("a1 APPEND INBOX {11}", &[]);
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Missing literal payload"));
    }
}
