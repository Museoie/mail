//! SEARCH criteria (imap/parsers/search-parsers.ts).
//!
//! Two things the Bun parser does here escape as JavaScript exceptions rather
//! than as a failed result, and callers see them: a search key that is not an
//! atom (`(`, a quoted string, a missing operand) throws a `TypeError`, and
//! `NOT` / `OR` nested past the JavaScript stack throw a `RangeError`. Both
//! are reported as [`ParseThrow`], which `parse_command` turns into the same
//! `Parse error: …` text the Bun server sends.

use super::primitive_parsers::{parse_atom, parse_date, parse_number, parse_sequence_set, parse_string, skip_whitespace};
use crate::imap::types::{ImapRequest, ParseContext, ParseResult, ParseThrow, SearchCriterion, SearchRequest};
use crate::imap::util::JsDate;

/// What Bun prints for `atom.value!.toUpperCase()` when the atom did not parse.
pub const NOT_AN_ATOM: &str =
    "TypeError: undefined is not an object (evaluating 'parseAtom(context).value.toUpperCase')";

/// What Bun prints when a parser recurses past the JavaScript stack.
pub const STACK_EXHAUSTED: &str = "RangeError: Maximum call stack size exceeded.";

/// How many `NOT` / `OR` levels a SEARCH may nest before the Bun parser runs
/// out of stack. Bun's own limit moves with what its JIT has compiled: it
/// measured between 20,000 and 31,000 levels. The longest nest that fits in
/// one 64 KiB command line is 16,000 levels, which always parses.
pub const MAX_SEARCH_DEPTH: usize = 22_000;

/// Parses SEARCH.
pub fn parse_search(context: &mut ParseContext) -> Result<ParseResult<ImapRequest>, ParseThrow> {
    parse_search_within(context, MAX_SEARCH_DEPTH)
}

/// SEARCH with `max_depth` levels of stack left, for a SEARCH that is itself
/// nested inside `UID` commands.
pub(crate) fn parse_search_within(
    context: &mut ParseContext,
    max_depth: usize,
) -> Result<ParseResult<ImapRequest>, ParseThrow> {
    let criteria = parse_criteria(context, max_depth)?;
    let Some(criteria) = criteria.into_result().ok() else {
        return Ok(ParseResult::fail("Invalid search criteria"));
    };
    Ok(ParseResult::ok(ImapRequest::Search(SearchRequest { criteria }), context.position))
}

/// Parses search keys to the end of the input.
///
/// A top-level SEARCH is `1*(SP search-key)`: space-separated keys ANDed
/// together (RFC 3501 §6.4.4). An atom that is no known key is skipped.
pub fn parse_search_criteria(context: &mut ParseContext) -> Result<ParseResult<Vec<SearchCriterion>>, ParseThrow> {
    parse_criteria(context, MAX_SEARCH_DEPTH)
}

fn parse_criteria(
    context: &mut ParseContext,
    max_depth: usize,
) -> Result<ParseResult<Vec<SearchCriterion>>, ParseThrow> {
    let start = context.position;
    let mut criteria: Vec<SearchCriterion> = Vec::new();

    while context.position < context.length {
        skip_whitespace(context);
        if context.position >= context.length {
            break;
        }
        match parse_search_key(context, max_depth)? {
            KeyOutcome::Key(criterion) => criteria.push(criterion),
            KeyOutcome::Unknown => {}
            KeyOutcome::Failed(error) => return Ok(ParseResult::fail(error)),
        }
    }

    Ok(ParseResult::ok(criteria, context.position - start))
}

/// What parsing one search key came to.
enum KeyOutcome {
    Key(SearchCriterion),
    /// An atom that matches no known key: success with nothing to keep.
    Unknown,
    Failed(String),
}

/// An operator whose operands are still being parsed.
enum Operator {
    Not,
    /// `OR` waiting for its left operand.
    OrLeft,
    /// `OR` holding its left operand and waiting for the right one.
    OrRight(SearchCriterion),
}

const NOT_OPERAND: &str = "Expected exactly 1 search criterion after NOT";
const OR_OPERANDS: &str = "Expected exactly 2 search criteria after OR";

/// Parses exactly one search key. Leading white space is the caller's to skip.
///
/// `NOT` takes one following key and `OR` takes two, so `NOT SEEN SINCE
/// 1-Jan-2026` is `[NOT SEEN]` and `[SINCE …]`. The Bun parser recurses for
/// the operands; this keeps the open operators on a heap stack instead, so a
/// nest as deep as Bun accepts cannot exhaust the thread's stack.
fn parse_search_key(context: &mut ParseContext, max_depth: usize) -> Result<KeyOutcome, ParseThrow> {
    let mut open: Vec<Operator> = Vec::new();

    'key: loop {
        if open.len() > max_depth {
            return Err(ParseThrow(STACK_EXHAUSTED.to_string()));
        }
        let start = context.position;

        // A bare sequence set is the SEQ key (RFC 3501 §6.4.4): message
        // sequence numbers, not UIDs. The explicit `UID <set>` stays UID.
        let sequence_set = parse_sequence_set(context);
        let mut outcome = if let (true, Some(set)) = (sequence_set.success, sequence_set.value) {
            KeyOutcome::Key(SearchCriterion::Seq(set))
        } else {
            context.position = start;
            let Some(atom) = parse_atom(context).value else {
                return Err(ParseThrow(NOT_AN_ATOM.to_string()));
            };
            match atom.to_uppercase().as_str() {
                "NOT" => {
                    skip_whitespace(context);
                    open.push(Operator::Not);
                    continue 'key;
                }
                "OR" => {
                    skip_whitespace(context);
                    open.push(Operator::OrLeft);
                    continue 'key;
                }
                name => parse_plain_key(name, context),
            }
        };

        // Hand the key to the operators waiting for it, innermost first.
        loop {
            outcome = match (open.pop(), outcome) {
                (None, outcome) => return Ok(outcome),
                (Some(Operator::Not), KeyOutcome::Key(operand)) => KeyOutcome::Key(SearchCriterion::not(operand)),
                (Some(Operator::Not), _) => KeyOutcome::Failed(NOT_OPERAND.to_string()),
                (Some(Operator::OrLeft), KeyOutcome::Key(left)) => {
                    skip_whitespace(context);
                    open.push(Operator::OrRight(left));
                    continue 'key;
                }
                (Some(Operator::OrRight(left)), KeyOutcome::Key(right)) => {
                    KeyOutcome::Key(SearchCriterion::or(left, right))
                }
                (Some(Operator::OrLeft | Operator::OrRight(_)), _) => KeyOutcome::Failed(OR_OPERANDS.to_string()),
            };
        }
    }
}

/// A key that is not an operator, by its upper-cased name.
fn parse_plain_key(name: &str, context: &mut ParseContext) -> KeyOutcome {
    use SearchCriterion as C;

    let flag = |context: &mut ParseContext, build: fn(String) -> C| {
        skip_whitespace(context);
        match parse_atom(context).into_result() {
            Ok(flag) => KeyOutcome::Key(build(flag)),
            Err(_) => KeyOutcome::Failed(format!("Expected flag after {name}")),
        }
    };
    let date = |context: &mut ParseContext, build: fn(JsDate) -> C| {
        skip_whitespace(context);
        match parse_date(context).into_result() {
            Ok(date) => KeyOutcome::Key(build(date)),
            Err(_) => KeyOutcome::Failed(format!("Expected date after {name}")),
        }
    };
    let text = |context: &mut ParseContext, build: fn(String) -> C| {
        skip_whitespace(context);
        match parse_string(context).into_result() {
            Ok(value) => KeyOutcome::Key(build(value)),
            Err(_) => KeyOutcome::Failed(format!("Expected text value after {name}")),
        }
    };
    let size = |context: &mut ParseContext, build: fn(f64) -> C| {
        skip_whitespace(context);
        match parse_number(context).into_result() {
            Ok(size) => KeyOutcome::Key(build(size)),
            Err(_) => KeyOutcome::Failed(format!("Expected size after {name}")),
        }
    };

    match name {
        "ALL" => KeyOutcome::Key(C::All),
        "ANSWERED" => KeyOutcome::Key(C::Answered),
        "DELETED" => KeyOutcome::Key(C::Deleted),
        "FLAGGED" => KeyOutcome::Key(C::Flagged),
        "NEW" => KeyOutcome::Key(C::New),
        "OLD" => KeyOutcome::Key(C::Old),
        "RECENT" => KeyOutcome::Key(C::Recent),
        "SEEN" => KeyOutcome::Key(C::Seen),
        "UNANSWERED" => KeyOutcome::Key(C::Unanswered),
        "UNDELETED" => KeyOutcome::Key(C::Undeleted),
        "UNFLAGGED" => KeyOutcome::Key(C::Unflagged),
        "UNSEEN" => KeyOutcome::Key(C::Unseen),
        "DRAFT" => KeyOutcome::Key(C::Draft),
        "UNDRAFT" => KeyOutcome::Key(C::Undraft),
        "KEYWORD" => flag(context, C::Keyword),
        "UNKEYWORD" => flag(context, C::Unkeyword),
        "BEFORE" => date(context, C::Before),
        "ON" => date(context, C::On),
        "SINCE" => date(context, C::Since),
        "SENTBEFORE" => date(context, C::SentBefore),
        "SENTON" => date(context, C::SentOn),
        "SENTSINCE" => date(context, C::SentSince),
        "FROM" => text(context, C::From),
        "TO" => text(context, C::To),
        "CC" => text(context, C::Cc),
        "BCC" => text(context, C::Bcc),
        "SUBJECT" => text(context, C::Subject),
        "BODY" => text(context, C::Body),
        "TEXT" => text(context, C::Text),
        "HEADER" => {
            skip_whitespace(context);
            let field = parse_string(context);
            skip_whitespace(context);
            let value = parse_string(context);
            match (field.into_result(), value.into_result()) {
                (Ok(field), Ok(value)) => KeyOutcome::Key(C::Header { field, value }),
                _ => KeyOutcome::Failed("Expected field & value after HEADER".to_string()),
            }
        }
        "UID" => {
            skip_whitespace(context);
            match parse_sequence_set(context).into_result() {
                Ok(set) => KeyOutcome::Key(C::Uid(set)),
                Err(_) => KeyOutcome::Failed("Expected sequence set after UID".to_string()),
            }
        }
        "LARGER" => size(context, C::Larger),
        "SMALLER" => size(context, C::Smaller),
        _ => KeyOutcome::Unknown,
    }
}
