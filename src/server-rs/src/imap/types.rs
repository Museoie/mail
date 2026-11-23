//! Types shared by the IMAP server (imap/types.ts).
//!
//! The parsers hand these to the session code. Each one also writes itself as
//! the JSON the Bun server's object would stringify to (`to_json_string`), which
//! is what the command diagnostics log and what the golden fixtures in
//! `tests/golden/imap` compare against.
//!
//! Numbers that come off the wire (`parseInt` in the Bun server) are `f64`
//! here: a client may send more digits than any integer type holds, and the
//! Bun server carries on with the rounded double, or with `Infinity`.

use std::collections::VecDeque;
use std::fmt;
use std::ops::Deref;

use serde::Deserialize;

use super::util::JsDate;

/// `Number.MAX_SAFE_INTEGER`, which is what `*` parses to in a sequence set.
pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

// ---------------------------------------------------------------------------
// JSON, as `JSON.stringify` writes it
// ---------------------------------------------------------------------------

/// `JSON.stringify(value)` for a string.
pub fn json_string(value: &str) -> String {
    // serde_json escapes exactly what ECMAScript's QuoteJSONString escapes:
    // the quote, the backslash and U+0000..U+001F, with the same spellings.
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// `String(value)` for a number: the shortest digits that round-trip, laid
/// out the way ECMAScript's Number::toString lays them out.
pub fn js_number_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // `{:e}` prints the shortest round-trip digits as `d.ddde<exp>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i64;
    // `n` is the position of the decimal point relative to the digits.
    let n = exponent.parse::<i64>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let exponent = format!("{}{}", if e < 0 { "-" } else { "+" }, e.abs());
        if k == 1 {
            format!("{digits}e{exponent}")
        } else {
            format!("{}.{}e{exponent}", &digits[..1], &digits[1..])
        }
    };
    format!("{sign}{body}")
}

/// `JSON.stringify(value)` for a number: `null` for NaN and the infinities.
pub fn json_number(value: f64) -> String {
    if value.is_finite() {
        js_number_string(value)
    } else {
        "null".to_string()
    }
}

fn json_string_array(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|item| json_string(item)).collect();
    format!("[{}]", parts.join(","))
}

// ---------------------------------------------------------------------------
// Parse context and result
// ---------------------------------------------------------------------------

/// An APPEND message literal, written to `path` as its octets arrived rather
/// than held in memory. `size` is the octet count the literal declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLiteral {
    pub path: String,
    pub size: u64,
}

impl MessageLiteral {
    pub fn to_json_string(&self) -> String {
        format!("{{\"path\":{},\"size\":{}}}", json_string(&self.path), self.size)
    }
}

/// A literal payload as the parsers receive it: decoded text, or a spooled message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Text(String),
    Message(MessageLiteral),
}

impl From<&str> for Literal {
    fn from(text: &str) -> Self {
        Literal::Text(text.to_string())
    }
}

impl From<String> for Literal {
    fn from(text: String) -> Self {
        Literal::Text(text)
    }
}

impl From<MessageLiteral> for Literal {
    fn from(message: MessageLiteral) -> Self {
        Literal::Message(message)
    }
}

/// Where a parser is in a command line.
///
/// `input` is UTF-16 code units and `position` / `length` count them, as a
/// JavaScript string does: every offset the Bun parsers report (`consumed`)
/// is in those units, and a line with non-ASCII text has to report the same
/// numbers here.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseContext {
    pub input: Vec<u16>,
    pub position: usize,
    pub length: usize,
    /// Literal payloads (RFC 3501 §4.3), in wire order. A literal's `{N}` is a
    /// count of octets, which no offset into `input` can express, so the
    /// handler consumes each payload off the byte buffer and queues it here.
    /// `input` keeps only the `{N}` marker, and `parse_literal` takes the
    /// matching payload off the front.
    pub literals: Option<VecDeque<Literal>>,
}

impl ParseContext {
    pub fn new(input: &str) -> ParseContext {
        let input: Vec<u16> = input.encode_utf16().collect();
        let length = input.len();
        ParseContext { input, position: 0, length, literals: None }
    }

    pub fn with_literals(input: &str, literals: Vec<Literal>) -> ParseContext {
        let mut context = ParseContext::new(input);
        context.literals = Some(literals.into());
        context
    }

    /// `input.substring(start, end)`.
    pub fn substring(&self, start: usize, end: usize) -> String {
        let end = end.min(self.input.len());
        let start = start.min(end);
        String::from_utf16_lossy(&self.input[start..end])
    }

    /// `input.substring(position)`: what is left to parse.
    pub fn rest(&self) -> String {
        self.substring(self.position, self.input.len())
    }
}

/// What a parser returns: the Bun server's `{ success, value, error, consumed }`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseResult<T> {
    pub success: bool,
    pub value: Option<T>,
    pub error: Option<String>,
    pub consumed: usize,
}

impl<T> ParseResult<T> {
    pub fn ok(value: T, consumed: usize) -> ParseResult<T> {
        ParseResult { success: true, value: Some(value), error: None, consumed }
    }

    /// A failure that consumed nothing.
    pub fn fail(error: impl Into<String>) -> ParseResult<T> {
        ParseResult { success: false, value: None, error: Some(error.into()), consumed: 0 }
    }

    /// The value, or the error text a `BAD` response carries.
    pub fn into_result(self) -> Result<T, String> {
        match (self.success, self.value) {
            (true, Some(value)) => Ok(value),
            (_, _) => Err(self.error.unwrap_or_default()),
        }
    }

    /// `JSON.stringify(result)`, given how to write the value.
    pub fn to_json_string_with(&self, value: impl Fn(&T) -> String) -> String {
        let mut out = format!("{{\"success\":{}", self.success);
        if let Some(inner) = &self.value {
            out.push_str(",\"value\":");
            out.push_str(&value(inner));
        }
        if let Some(error) = &self.error {
            out.push_str(",\"error\":");
            out.push_str(&json_string(error));
        }
        out.push_str(&format!(",\"consumed\":{}}}", self.consumed));
        out
    }
}

/// A JavaScript exception a Bun parser lets escape, as `${error}` prints it.
///
/// Two can happen: the `TypeError` SEARCH raises on a token that is not an
/// atom, and the `RangeError` of a parser recursing past the stack.
/// `parse_command` catches both, as the Bun `parseCommand` does, and reports
/// `Parse error: <text>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseThrow(pub String);

impl fmt::Display for ParseThrow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseThrow {}

// ---------------------------------------------------------------------------
// Sequence sets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceSetType {
    Sequence,
    Uid,
}

impl SequenceSetType {
    pub fn as_str(self) -> &'static str {
        match self {
            SequenceSetType::Sequence => "sequence",
            SequenceSetType::Uid => "uid",
        }
    }
}

/// One member of a sequence set. `end` is `None` for a single message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SequenceRange {
    pub start: f64,
    pub end: Option<f64>,
}

impl SequenceRange {
    pub fn single(start: f64) -> SequenceRange {
        SequenceRange { start, end: None }
    }

    pub fn range(start: f64, end: f64) -> SequenceRange {
        SequenceRange { start, end: Some(end) }
    }

    pub fn to_json_string(&self) -> String {
        match self.end {
            Some(end) => format!("{{\"start\":{},\"end\":{}}}", json_number(self.start), json_number(end)),
            None => format!("{{\"start\":{}}}", json_number(self.start)),
        }
    }
}

/// Sequence set for message selection. The parser always stamps `Sequence`;
/// whether the numbers are UIDs is the caller's knowledge (`UID FETCH`).
#[derive(Debug, Clone, PartialEq)]
pub struct SequenceSet {
    pub kind: SequenceSetType,
    pub ranges: Vec<SequenceRange>,
}

impl SequenceSet {
    pub fn new(ranges: Vec<SequenceRange>) -> SequenceSet {
        SequenceSet { kind: SequenceSetType::Sequence, ranges }
    }

    pub fn to_json_string(&self) -> String {
        let ranges: Vec<String> = self.ranges.iter().map(SequenceRange::to_json_string).collect();
        format!("{{\"type\":\"{}\",\"ranges\":[{}]}}", self.kind.as_str(), ranges.join(","))
    }
}

// ---------------------------------------------------------------------------
// FETCH
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartialRange {
    pub start: f64,
    pub length: f64,
}

/// The sub-section of a MIME part: `BODY[1.HEADER]`, `BODY[1.TEXT]`, `BODY[1.MIME]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MimeSubSection {
    Header,
    Text,
    Mime,
}

impl MimeSubSection {
    pub fn as_str(self) -> &'static str {
        match self {
            MimeSubSection::Header => "HEADER",
            MimeSubSection::Text => "TEXT",
            MimeSubSection::Mime => "MIME",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BodySection {
    /// `BODY[]` or RFC822.
    Full,
    /// `BODY[HEADER]` or RFC822.HEADER.
    Header,
    /// `BODY[TEXT]` or RFC822.TEXT.
    Text,
    /// `part_number` is e.g. `1`, `1.2`, `2.1.3`.
    MimePart { part_number: String, sub_section: Option<MimeSubSection> },
    /// `not` is true for HEADER.FIELDS.NOT; `fields` are upper-cased names.
    HeaderFields { not: bool, fields: Vec<String> },
}

impl BodySection {
    pub fn type_name(&self) -> &'static str {
        match self {
            BodySection::Full => "FULL",
            BodySection::Header => "HEADER",
            BodySection::Text => "TEXT",
            BodySection::MimePart { .. } => "MIME_PART",
            BodySection::HeaderFields { .. } => "HEADER_FIELDS",
        }
    }

    pub fn to_json_string(&self) -> String {
        match self {
            BodySection::MimePart { part_number, sub_section } => {
                let mut out = format!("{{\"type\":\"MIME_PART\",\"partNumber\":{}", json_string(part_number));
                if let Some(sub) = sub_section {
                    out.push_str(&format!(",\"subSection\":\"{}\"", sub.as_str()));
                }
                out.push('}');
                out
            }
            BodySection::HeaderFields { not, fields } => {
                format!("{{\"type\":\"HEADER_FIELDS\",\"not\":{not},\"fields\":{}}}", json_string_array(fields))
            }
            simple => format!("{{\"type\":\"{}\"}}", simple.type_name()),
        }
    }
}

/// A `BODY[...]` / `BODY.PEEK[...]` content fetch.
#[derive(Debug, Clone, PartialEq)]
pub struct BodyFetch {
    /// True for BODY.PEEK.
    pub peek: bool,
    pub section: BodySection,
    pub partial: Option<PartialRange>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FetchDataItem {
    Envelope,
    Flags,
    /// `extensible: false` is the bare `BODY` data item: the non-extensible
    /// form of BODYSTRUCTURE (RFC 3501 §6.4.5), labelled `BODY` and without
    /// the extension fields.
    BodyStructure { extensible: bool },
    Body(BodyFetch),
    Uid,
    InternalDate,
    Rfc822,
    Rfc822Header,
    Rfc822Size,
    Rfc822Text,
    /// RFC 4551 CONDSTORE: the per-message mod-sequence.
    Modseq,
}

impl FetchDataItem {
    /// The Bun server's `type` tag.
    pub fn type_name(&self) -> &'static str {
        match self {
            FetchDataItem::Envelope => "ENVELOPE",
            FetchDataItem::Flags => "FLAGS",
            FetchDataItem::BodyStructure { .. } => "BODYSTRUCTURE",
            FetchDataItem::Body(_) => "BODY",
            FetchDataItem::Uid => "UID",
            FetchDataItem::InternalDate => "INTERNALDATE",
            FetchDataItem::Rfc822 => "RFC822",
            FetchDataItem::Rfc822Header => "RFC822.HEADER",
            FetchDataItem::Rfc822Size => "RFC822.SIZE",
            FetchDataItem::Rfc822Text => "RFC822.TEXT",
            FetchDataItem::Modseq => "MODSEQ",
        }
    }

    pub fn to_json_string(&self) -> String {
        match self {
            FetchDataItem::BodyStructure { extensible } => {
                format!("{{\"type\":\"BODYSTRUCTURE\",\"extensible\":{extensible}}}")
            }
            FetchDataItem::Body(body) => {
                let mut out = format!(
                    "{{\"type\":\"BODY\",\"peek\":{},\"section\":{}",
                    body.peek,
                    body.section.to_json_string()
                );
                if let Some(partial) = &body.partial {
                    out.push_str(&format!(
                        ",\"partial\":{{\"start\":{},\"length\":{}}}",
                        json_number(partial.start),
                        json_number(partial.length)
                    ));
                }
                out.push('}');
                out
            }
            simple => format!("{{\"type\":\"{}\"}}", simple.type_name()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchRequest {
    pub sequence_set: SequenceSet,
    pub data_items: Vec<FetchDataItem>,
    /// RFC 4551 §3.3.1 CHANGEDSINCE modifier: `FETCH … (CHANGEDSINCE <modseq>)`.
    pub changed_since: Option<f64>,
}

// ---------------------------------------------------------------------------
// SEARCH
// ---------------------------------------------------------------------------

/// One SEARCH key (RFC 3501 §6.4.4).
///
/// `Seq` is a bare sequence set, which names message sequence numbers in a
/// plain SEARCH and UIDs in a UID SEARCH; `Uid` is the explicit `UID <set>`.
///
/// `NOT` and `OR` nest, and a client chooses how deep: tens of thousands of
/// levels parse, as they do in the Bun server. Dropping, cloning, comparing
/// and printing a criterion never recurse, so none of them can run out of
/// stack on such a value. Code that walks one should use [`SearchCriterion::fold`]
/// rather than recursion for the same reason.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchCriterion {
    All,
    Answered,
    Deleted,
    Flagged,
    New,
    Old,
    Recent,
    Seen,
    Unanswered,
    Undeleted,
    Unflagged,
    Unseen,
    Draft,
    Undraft,
    Keyword(String),
    Unkeyword(String),
    Before(JsDate),
    On(JsDate),
    Since(JsDate),
    SentBefore(JsDate),
    SentOn(JsDate),
    SentSince(JsDate),
    From(String),
    To(String),
    Cc(String),
    Bcc(String),
    Subject(String),
    Body(String),
    Text(String),
    Header { field: String, value: String },
    Uid(SequenceSet),
    Seq(SequenceSet),
    Larger(f64),
    Smaller(f64),
    Not(SubCriterion),
    Or(SubCriterion, SubCriterion),
}

impl SearchCriterion {
    /// `NOT <criterion>`.
    #[allow(clippy::should_implement_trait)]
    pub fn not(criterion: SearchCriterion) -> SearchCriterion {
        SearchCriterion::Not(SubCriterion::new(criterion))
    }

    /// `OR <left> <right>`.
    pub fn or(left: SearchCriterion, right: SearchCriterion) -> SearchCriterion {
        SearchCriterion::Or(SubCriterion::new(left), SubCriterion::new(right))
    }

    /// The Bun server's `type` tag.
    pub fn type_name(&self) -> &'static str {
        match self {
            SearchCriterion::All => "ALL",
            SearchCriterion::Answered => "ANSWERED",
            SearchCriterion::Deleted => "DELETED",
            SearchCriterion::Flagged => "FLAGGED",
            SearchCriterion::New => "NEW",
            SearchCriterion::Old => "OLD",
            SearchCriterion::Recent => "RECENT",
            SearchCriterion::Seen => "SEEN",
            SearchCriterion::Unanswered => "UNANSWERED",
            SearchCriterion::Undeleted => "UNDELETED",
            SearchCriterion::Unflagged => "UNFLAGGED",
            SearchCriterion::Unseen => "UNSEEN",
            SearchCriterion::Draft => "DRAFT",
            SearchCriterion::Undraft => "UNDRAFT",
            SearchCriterion::Keyword(_) => "KEYWORD",
            SearchCriterion::Unkeyword(_) => "UNKEYWORD",
            SearchCriterion::Before(_) => "BEFORE",
            SearchCriterion::On(_) => "ON",
            SearchCriterion::Since(_) => "SINCE",
            SearchCriterion::SentBefore(_) => "SENTBEFORE",
            SearchCriterion::SentOn(_) => "SENTON",
            SearchCriterion::SentSince(_) => "SENTSINCE",
            SearchCriterion::From(_) => "FROM",
            SearchCriterion::To(_) => "TO",
            SearchCriterion::Cc(_) => "CC",
            SearchCriterion::Bcc(_) => "BCC",
            SearchCriterion::Subject(_) => "SUBJECT",
            SearchCriterion::Body(_) => "BODY",
            SearchCriterion::Text(_) => "TEXT",
            SearchCriterion::Header { .. } => "HEADER",
            SearchCriterion::Uid(_) => "UID",
            SearchCriterion::Seq(_) => "SEQ",
            SearchCriterion::Larger(_) => "LARGER",
            SearchCriterion::Smaller(_) => "SMALLER",
            SearchCriterion::Not(_) => "NOT",
            SearchCriterion::Or(_, _) => "OR",
        }
    }

    /// How many `NOT` / `OR` levels sit above the deepest key: 0 for a plain key.
    pub fn depth(&self) -> usize {
        let mut deepest = 0;
        let mut pending = vec![(self, 0usize)];
        while let Some((node, depth)) = pending.pop() {
            deepest = deepest.max(depth);
            match node {
                SearchCriterion::Not(inner) => pending.push((inner, depth + 1)),
                SearchCriterion::Or(left, right) => {
                    pending.push((left, depth + 1));
                    pending.push((right, depth + 1));
                }
                _ => {}
            }
        }
        deepest
    }

    /// Reduces the criterion bottom-up without recursion: `leaf` for every key
    /// that is not an operator, then `not` and `or` over the results.
    ///
    /// ```ignore
    /// let sql = criterion.fold(&mut |key| key_to_sql(key), &mut |a| format!("NOT ({a})"), &mut |a, b| format!("({a} OR {b})"));
    /// ```
    pub fn fold<T>(
        &self,
        leaf: &mut dyn FnMut(&SearchCriterion) -> T,
        not: &mut dyn FnMut(T) -> T,
        or: &mut dyn FnMut(T, T) -> T,
    ) -> T {
        enum Step<'a> {
            Visit(&'a SearchCriterion),
            Not,
            Or,
        }
        let mut work = vec![Step::Visit(self)];
        let mut built: Vec<T> = Vec::new();
        while let Some(step) = work.pop() {
            match step {
                Step::Visit(SearchCriterion::Not(inner)) => {
                    work.push(Step::Not);
                    work.push(Step::Visit(inner));
                }
                Step::Visit(SearchCriterion::Or(left, right)) => {
                    work.push(Step::Or);
                    work.push(Step::Visit(right));
                    work.push(Step::Visit(left));
                }
                Step::Visit(key) => built.push(leaf(key)),
                Step::Not => {
                    if let Some(operand) = built.pop() {
                        built.push(not(operand));
                    }
                }
                Step::Or => {
                    if let (Some(right), Some(left)) = (built.pop(), built.pop()) {
                        built.push(or(left, right));
                    }
                }
            }
        }
        match built.pop() {
            Some(result) => result,
            // Every criterion visits at least one key.
            None => leaf(self),
        }
    }

    fn write_key_json(&self, out: &mut String) {
        let name = self.type_name();
        match self {
            SearchCriterion::Keyword(flag) | SearchCriterion::Unkeyword(flag) => {
                out.push_str(&format!("{{\"type\":\"{name}\",\"flag\":{}}}", json_string(flag)));
            }
            SearchCriterion::Before(date)
            | SearchCriterion::On(date)
            | SearchCriterion::Since(date)
            | SearchCriterion::SentBefore(date)
            | SearchCriterion::SentOn(date)
            | SearchCriterion::SentSince(date) => {
                out.push_str(&format!("{{\"type\":\"{name}\",\"date\":{}}}", date.to_json_string()));
            }
            SearchCriterion::From(value)
            | SearchCriterion::To(value)
            | SearchCriterion::Cc(value)
            | SearchCriterion::Bcc(value)
            | SearchCriterion::Subject(value)
            | SearchCriterion::Body(value)
            | SearchCriterion::Text(value) => {
                out.push_str(&format!("{{\"type\":\"{name}\",\"value\":{}}}", json_string(value)));
            }
            SearchCriterion::Header { field, value } => {
                out.push_str(&format!(
                    "{{\"type\":\"HEADER\",\"field\":{},\"value\":{}}}",
                    json_string(field),
                    json_string(value)
                ));
            }
            SearchCriterion::Uid(set) | SearchCriterion::Seq(set) => {
                out.push_str(&format!("{{\"type\":\"{name}\",\"sequenceSet\":{}}}", set.to_json_string()));
            }
            SearchCriterion::Larger(size) | SearchCriterion::Smaller(size) => {
                out.push_str(&format!("{{\"type\":\"{name}\",\"size\":{}}}", json_number(*size)));
            }
            _ => out.push_str(&format!("{{\"type\":\"{name}\"}}")),
        }
    }

    /// Appends `JSON.stringify(criterion)`.
    pub fn write_json(&self, out: &mut String) {
        enum Piece<'a> {
            Node(&'a SearchCriterion),
            Text(&'static str),
        }
        let mut work = vec![Piece::Node(self)];
        while let Some(piece) = work.pop() {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Node(SearchCriterion::Not(inner)) => {
                    out.push_str("{\"type\":\"NOT\",\"criterion\":");
                    work.push(Piece::Text("}"));
                    work.push(Piece::Node(inner));
                }
                Piece::Node(SearchCriterion::Or(left, right)) => {
                    out.push_str("{\"type\":\"OR\",\"left\":");
                    work.push(Piece::Text("}"));
                    work.push(Piece::Node(right));
                    work.push(Piece::Text(",\"right\":"));
                    work.push(Piece::Node(left));
                }
                Piece::Node(key) => key.write_key_json(out),
            }
        }
    }

    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }
}

/// The operand of a `NOT` or an `OR`: a boxed criterion whose drop, clone,
/// comparison and `Debug` walk the tree with a heap stack instead of recursing.
pub struct SubCriterion(Box<SearchCriterion>);

fn is_operator(criterion: &SearchCriterion) -> bool {
    matches!(criterion, SearchCriterion::Not(_) | SearchCriterion::Or(_, _))
}

impl SubCriterion {
    pub fn new(criterion: SearchCriterion) -> SubCriterion {
        SubCriterion(Box::new(criterion))
    }

    /// The criterion itself.
    pub fn into_inner(mut self) -> SearchCriterion {
        std::mem::replace(&mut *self.0, SearchCriterion::All)
    }
}

impl Deref for SubCriterion {
    type Target = SearchCriterion;

    fn deref(&self) -> &SearchCriterion {
        &self.0
    }
}

impl Drop for SubCriterion {
    fn drop(&mut self) {
        if !is_operator(&self.0) {
            return;
        }
        // Detach each operator's operands before it is dropped, so no drop
        // ever finds more than one level beneath it.
        let mut pending = vec![std::mem::replace(&mut *self.0, SearchCriterion::All)];
        while let Some(node) = pending.pop() {
            match node {
                SearchCriterion::Not(mut inner) => {
                    if is_operator(&inner.0) {
                        pending.push(std::mem::replace(&mut *inner.0, SearchCriterion::All));
                    }
                }
                SearchCriterion::Or(mut left, mut right) => {
                    for side in [&mut left, &mut right] {
                        if is_operator(&side.0) {
                            pending.push(std::mem::replace(&mut *side.0, SearchCriterion::All));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl Clone for SubCriterion {
    fn clone(&self) -> SubCriterion {
        // `fold` clones only keys with the derived `Clone`; a key holds no
        // operand, so that clone cannot recurse.
        let copy = self.0.fold(
            &mut |key| key.clone(),
            &mut SearchCriterion::not,
            &mut SearchCriterion::or,
        );
        SubCriterion::new(copy)
    }
}

impl PartialEq for SubCriterion {
    fn eq(&self, other: &SubCriterion) -> bool {
        let mut pending: Vec<(&SearchCriterion, &SearchCriterion)> = vec![(&self.0, &other.0)];
        while let Some(pair) = pending.pop() {
            match pair {
                (SearchCriterion::Not(a), SearchCriterion::Not(b)) => pending.push((a, b)),
                (SearchCriterion::Or(a1, a2), SearchCriterion::Or(b1, b2)) => {
                    pending.push((a1, b1));
                    pending.push((a2, b2));
                }
                (a, b) if is_operator(a) || is_operator(b) => return false,
                // Two keys: the derived comparison, which has nothing to descend into.
                (a, b) if a != b => return false,
                _ => {}
            }
        }
        true
    }
}

impl fmt::Debug for SubCriterion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_json_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    pub criteria: Vec<SearchCriterion>,
}

// ---------------------------------------------------------------------------
// STORE, COPY, MOVE, APPEND
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOperation {
    Flags,
    FlagsSilent,
    AddFlags,
    AddFlagsSilent,
    RemoveFlags,
    RemoveFlagsSilent,
}

impl StoreOperation {
    pub const ALL: [StoreOperation; 6] = [
        StoreOperation::Flags,
        StoreOperation::FlagsSilent,
        StoreOperation::AddFlags,
        StoreOperation::AddFlagsSilent,
        StoreOperation::RemoveFlags,
        StoreOperation::RemoveFlagsSilent,
    ];

    /// The item name as the client wrote it, upper-cased: `+FLAGS.SILENT`.
    pub fn as_str(self) -> &'static str {
        match self {
            StoreOperation::Flags => "FLAGS",
            StoreOperation::FlagsSilent => "FLAGS.SILENT",
            StoreOperation::AddFlags => "+FLAGS",
            StoreOperation::AddFlagsSilent => "+FLAGS.SILENT",
            StoreOperation::RemoveFlags => "-FLAGS",
            StoreOperation::RemoveFlagsSilent => "-FLAGS.SILENT",
        }
    }

    pub fn parse(value: &str) -> Option<StoreOperation> {
        StoreOperation::ALL.into_iter().find(|operation| operation.as_str() == value)
    }

    pub fn is_silent(self) -> bool {
        self.as_str().contains(".SILENT")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoreRequest {
    pub sequence_set: SequenceSet,
    pub operation: StoreOperation,
    pub flags: Vec<String>,
    /// True for the `.SILENT` operations.
    pub silent: bool,
    /// RFC 7162 §3.1.3 UNCHANGEDSINCE modifier. Absent: an unconditional STORE.
    pub unchanged_since: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CopyRequest {
    pub sequence_set: SequenceSet,
    pub mailbox: String,
}

/// RFC 6851 MOVE: wire-identical to COPY.
pub type MoveRequest = CopyRequest;

#[derive(Debug, Clone, PartialEq)]
pub struct AppendRequest {
    pub mailbox: String,
    pub flags: Option<Vec<String>>,
    pub date: Option<String>,
    pub message: MessageLiteral,
}

// ---------------------------------------------------------------------------
// Mailbox operations and authentication
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectRequest {
    pub mailbox: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRequest {
    pub reference: String,
    pub pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusItem {
    Messages,
    Recent,
    UidNext,
    UidValidity,
    Unseen,
    HighestModseq,
}

impl StatusItem {
    pub const ALL: [StatusItem; 6] = [
        StatusItem::Messages,
        StatusItem::Recent,
        StatusItem::UidNext,
        StatusItem::UidValidity,
        StatusItem::Unseen,
        StatusItem::HighestModseq,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StatusItem::Messages => "MESSAGES",
            StatusItem::Recent => "RECENT",
            StatusItem::UidNext => "UIDNEXT",
            StatusItem::UidValidity => "UIDVALIDITY",
            StatusItem::Unseen => "UNSEEN",
            StatusItem::HighestModseq => "HIGHESTMODSEQ",
        }
    }

    pub fn parse(value: &str) -> Option<StatusItem> {
        StatusItem::ALL.into_iter().find(|item| item.as_str() == value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRequest {
    pub mailbox: String,
    pub items: Vec<StatusItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticateRequest {
    pub mechanism: String,
    pub initial_response: Option<String>,
}

// ---------------------------------------------------------------------------
// The parsed command
// ---------------------------------------------------------------------------

/// A `UID <command>`: the wrapped request and its upper-cased name.
///
/// The Bun parser accepts `UID UID UID … FETCH`, nesting as deep as the line
/// is long, so this is a chain a client sizes. Like [`SubCriterion`], it is
/// dropped, cloned, compared and printed without recursion.
pub struct UidRequest {
    pub command: String,
    request: Box<ImapRequest>,
}

impl UidRequest {
    pub fn new(command: impl Into<String>, request: ImapRequest) -> UidRequest {
        UidRequest { command: command.into(), request: Box::new(request) }
    }

    /// The wrapped request.
    pub fn request(&self) -> &ImapRequest {
        &self.request
    }

    /// The command name and the wrapped request.
    pub fn into_parts(mut self) -> (String, ImapRequest) {
        let request = std::mem::replace(&mut *self.request, ImapRequest::Noop);
        (std::mem::take(&mut self.command), request)
    }

    /// The `command` of every nested `UID`, outermost first, and the request
    /// the innermost one wraps.
    fn chain(&self) -> (Vec<&str>, &ImapRequest) {
        let mut commands = vec![self.command.as_str()];
        let mut inner: &ImapRequest = &self.request;
        while let ImapRequest::Uid(next) = inner {
            commands.push(next.command.as_str());
            inner = &next.request;
        }
        (commands, inner)
    }
}

impl Drop for UidRequest {
    fn drop(&mut self) {
        // Detach each nested request before the one holding it is dropped.
        let mut next = std::mem::replace(&mut *self.request, ImapRequest::Noop);
        while let ImapRequest::Uid(inner) = &mut next {
            next = std::mem::replace(&mut *inner.request, ImapRequest::Noop);
        }
    }
}

impl Clone for UidRequest {
    fn clone(&self) -> UidRequest {
        let (commands, inner) = self.chain();
        // `inner` is not a UID request, so its derived clone stops there.
        let mut request = inner.clone();
        for command in commands.iter().skip(1).rev() {
            request = ImapRequest::Uid(UidRequest::new(*command, request));
        }
        UidRequest::new(commands[0], request)
    }
}

impl PartialEq for UidRequest {
    fn eq(&self, other: &UidRequest) -> bool {
        let (commands, inner) = self.chain();
        let (other_commands, other_inner) = other.chain();
        commands == other_commands && inner == other_inner
    }
}

impl fmt::Debug for UidRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        self.write_json(&mut out);
        f.write_str(&out)
    }
}

impl UidRequest {
    fn write_json(&self, out: &mut String) {
        let (commands, inner) = self.chain();
        for command in &commands {
            out.push_str(&format!(
                "{{\"type\":\"UID\",\"data\":{{\"command\":{},\"request\":",
                json_string(command)
            ));
        }
        inner.write_json(out);
        for _ in &commands {
            out.push_str("}}");
        }
    }
}

/// A parsed IMAP command.
#[derive(Debug, Clone, PartialEq)]
pub enum ImapRequest {
    Capability,
    Noop,
    Login(LoginRequest),
    Authenticate(AuthenticateRequest),
    List(ListRequest),
    Lsub(ListRequest),
    Select(SelectRequest),
    Examine(SelectRequest),
    Create { mailbox: String },
    Delete { mailbox: String },
    Rename { old_name: String, new_name: String },
    Subscribe { mailbox: String },
    Unsubscribe { mailbox: String },
    Status(StatusRequest),
    Append(AppendRequest),
    Idle,
    Check,
    Close,
    Expunge,
    Search(SearchRequest),
    Fetch(FetchRequest),
    Store(StoreRequest),
    Copy(CopyRequest),
    Move(MoveRequest),
    Uid(UidRequest),
    Id,
    Done,
    Logout,
    Starttls,
    Namespace,
    Enable { capabilities: Vec<String> },
    Unselect,
    Getquotaroot { mailbox: String },
}

impl ImapRequest {
    /// The Bun server's `type` tag: the command name.
    pub fn type_name(&self) -> &'static str {
        match self {
            ImapRequest::Capability => "CAPABILITY",
            ImapRequest::Noop => "NOOP",
            ImapRequest::Login(_) => "LOGIN",
            ImapRequest::Authenticate(_) => "AUTHENTICATE",
            ImapRequest::List(_) => "LIST",
            ImapRequest::Lsub(_) => "LSUB",
            ImapRequest::Select(_) => "SELECT",
            ImapRequest::Examine(_) => "EXAMINE",
            ImapRequest::Create { .. } => "CREATE",
            ImapRequest::Delete { .. } => "DELETE",
            ImapRequest::Rename { .. } => "RENAME",
            ImapRequest::Subscribe { .. } => "SUBSCRIBE",
            ImapRequest::Unsubscribe { .. } => "UNSUBSCRIBE",
            ImapRequest::Status(_) => "STATUS",
            ImapRequest::Append(_) => "APPEND",
            ImapRequest::Idle => "IDLE",
            ImapRequest::Check => "CHECK",
            ImapRequest::Close => "CLOSE",
            ImapRequest::Expunge => "EXPUNGE",
            ImapRequest::Search(_) => "SEARCH",
            ImapRequest::Fetch(_) => "FETCH",
            ImapRequest::Store(_) => "STORE",
            ImapRequest::Copy(_) => "COPY",
            ImapRequest::Move(_) => "MOVE",
            ImapRequest::Uid(_) => "UID",
            ImapRequest::Id => "ID",
            ImapRequest::Done => "DONE",
            ImapRequest::Logout => "LOGOUT",
            ImapRequest::Starttls => "STARTTLS",
            ImapRequest::Namespace => "NAMESPACE",
            ImapRequest::Enable { .. } => "ENABLE",
            ImapRequest::Unselect => "UNSELECT",
            ImapRequest::Getquotaroot { .. } => "GETQUOTAROOT",
        }
    }

    /// The `data` member of the Bun object, or `None` for a command with no arguments.
    fn data_json(&self) -> Option<String> {
        let mailbox = |mailbox: &String| format!("{{\"mailbox\":{}}}", json_string(mailbox));
        let copy = |data: &CopyRequest| {
            format!(
                "{{\"sequenceSet\":{},\"mailbox\":{}}}",
                data.sequence_set.to_json_string(),
                json_string(&data.mailbox)
            )
        };
        Some(match self {
            ImapRequest::Login(data) => format!(
                "{{\"username\":{},\"password\":{}}}",
                json_string(&data.username),
                json_string(&data.password)
            ),
            ImapRequest::Authenticate(data) => {
                let mut out = format!("{{\"mechanism\":{}", json_string(&data.mechanism));
                if let Some(response) = &data.initial_response {
                    out.push_str(&format!(",\"initialResponse\":{}", json_string(response)));
                }
                out.push('}');
                out
            }
            ImapRequest::List(data) | ImapRequest::Lsub(data) => format!(
                "{{\"reference\":{},\"pattern\":{}}}",
                json_string(&data.reference),
                json_string(&data.pattern)
            ),
            ImapRequest::Select(data) | ImapRequest::Examine(data) => mailbox(&data.mailbox),
            ImapRequest::Create { mailbox: name }
            | ImapRequest::Delete { mailbox: name }
            | ImapRequest::Subscribe { mailbox: name }
            | ImapRequest::Unsubscribe { mailbox: name }
            | ImapRequest::Getquotaroot { mailbox: name } => mailbox(name),
            ImapRequest::Rename { old_name, new_name } => format!(
                "{{\"oldName\":{},\"newName\":{}}}",
                json_string(old_name),
                json_string(new_name)
            ),
            ImapRequest::Status(data) => {
                let items: Vec<String> = data.items.iter().map(|item| format!("\"{}\"", item.as_str())).collect();
                format!("{{\"mailbox\":{},\"items\":[{}]}}", json_string(&data.mailbox), items.join(","))
            }
            ImapRequest::Append(data) => {
                let mut out = format!("{{\"mailbox\":{}", json_string(&data.mailbox));
                if let Some(flags) = &data.flags {
                    out.push_str(&format!(",\"flags\":{}", json_string_array(flags)));
                }
                if let Some(date) = &data.date {
                    out.push_str(&format!(",\"date\":{}", json_string(date)));
                }
                out.push_str(&format!(",\"message\":{}}}", data.message.to_json_string()));
                out
            }
            ImapRequest::Search(data) => {
                let mut out = String::from("{\"criteria\":[");
                for (index, criterion) in data.criteria.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    criterion.write_json(&mut out);
                }
                out.push_str("]}");
                out
            }
            ImapRequest::Fetch(data) => {
                let items: Vec<String> = data.data_items.iter().map(FetchDataItem::to_json_string).collect();
                let mut out = format!(
                    "{{\"sequenceSet\":{},\"dataItems\":[{}]",
                    data.sequence_set.to_json_string(),
                    items.join(",")
                );
                if let Some(modseq) = data.changed_since {
                    out.push_str(&format!(",\"changedSince\":{}", json_number(modseq)));
                }
                out.push('}');
                out
            }
            ImapRequest::Store(data) => {
                let mut out = format!(
                    "{{\"sequenceSet\":{},\"operation\":\"{}\",\"flags\":{},\"silent\":{}",
                    data.sequence_set.to_json_string(),
                    data.operation.as_str(),
                    json_string_array(&data.flags),
                    data.silent
                );
                if let Some(modseq) = data.unchanged_since {
                    out.push_str(&format!(",\"unchangedSince\":{}", json_number(modseq)));
                }
                out.push('}');
                out
            }
            ImapRequest::Copy(data) | ImapRequest::Move(data) => copy(data),
            ImapRequest::Enable { capabilities } => {
                format!("{{\"capabilities\":{}}}", json_string_array(capabilities))
            }
            _ => return None,
        })
    }

    /// Appends `JSON.stringify(request)`.
    pub fn write_json(&self, out: &mut String) {
        if let ImapRequest::Uid(uid) = self {
            uid.write_json(out);
            return;
        }
        out.push_str(&format!("{{\"type\":\"{}\"", self.type_name()));
        if let Some(data) = self.data_json() {
            out.push_str(",\"data\":");
            out.push_str(&data);
        }
        out.push('}');
    }

    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }
}

/// What `parse_command` returns for a line that parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCommand {
    pub tag: String,
    pub request: ImapRequest,
}

impl ParsedCommand {
    pub fn to_json_string(&self) -> String {
        let mut out = format!("{{\"tag\":{},\"request\":", json_string(&self.tag));
        self.request.write_json(&mut out);
        out.push('}');
        out
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Basic IMAP command structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapCommand {
    pub tag: String,
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseStatus {
    Ok,
    No,
    Bad,
}

impl ResponseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ResponseStatus::Ok => "OK",
            ResponseStatus::No => "NO",
            ResponseStatus::Bad => "BAD",
        }
    }
}

/// A tagged completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapResponse {
    pub tag: String,
    pub status: ResponseStatus,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusResponseItem {
    pub attribute: StatusItem,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusResponseData {
    pub mailbox: String,
    pub items: Vec<StatusResponseItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListResponseData {
    pub flags: Vec<String>,
    pub delimiter: String,
    pub mailbox: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchResponseData {
    pub uid: Option<i64>,
    pub flags: Option<Vec<String>>,
    pub envelope: Option<String>,
    pub bodystructure: Option<String>,
    /// Section key to content, in insertion order.
    pub body: Option<Vec<(String, String)>>,
    pub size: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResponseData {
    pub results: Vec<i64>,
}

/// Every untagged response the server can send, with its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UntaggedResponseData {
    Exists { count: i64 },
    Recent { count: i64 },
    Expunge { sequence_number: i64 },
    Fetch { sequence_number: i64, data: FetchResponseData },
    Search { data: SearchResponseData },
    List { data: ListResponseData },
    Lsub { data: ListResponseData },
    Status { data: StatusResponseData },
    Flags { flags: Vec<String> },
    Bye { message: String },
    Capability { capabilities: Vec<String> },
}

impl UntaggedResponseData {
    /// The response name: `EXISTS`, `FETCH`, …
    pub fn type_name(&self) -> &'static str {
        match self {
            UntaggedResponseData::Exists { .. } => "EXISTS",
            UntaggedResponseData::Recent { .. } => "RECENT",
            UntaggedResponseData::Expunge { .. } => "EXPUNGE",
            UntaggedResponseData::Fetch { .. } => "FETCH",
            UntaggedResponseData::Search { .. } => "SEARCH",
            UntaggedResponseData::List { .. } => "LIST",
            UntaggedResponseData::Lsub { .. } => "LSUB",
            UntaggedResponseData::Status { .. } => "STATUS",
            UntaggedResponseData::Flags { .. } => "FLAGS",
            UntaggedResponseData::Bye { .. } => "BYE",
            UntaggedResponseData::Capability { .. } => "CAPABILITY",
        }
    }
}

/// An untagged response: its name is `data.type_name()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntaggedResponse {
    pub data: UntaggedResponseData,
}

// ---------------------------------------------------------------------------
// The mail a FETCH formats
// ---------------------------------------------------------------------------

/// One mailbox of an address header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MailAddressValue {
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// An address header: its parsed mailboxes and its text as received.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MailAddress {
    #[serde(default)]
    pub value: Option<Vec<MailAddressValue>>,
    #[serde(default)]
    pub text: Option<String>,
}

/// Where an attachment's bytes are stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct AttachmentContent {
    #[serde(default)]
    pub data: String,
}

/// An attachment as the `attachments` JSONB column holds it. Rows written
/// before a field existed arrive without it, hence the options.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct MailAttachment {
    #[serde(default)]
    pub content: Option<AttachmentContent>,
    #[serde(default, rename = "contentType")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub size: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct MailUid {
    #[serde(default)]
    pub domain: f64,
    #[serde(default)]
    pub account: f64,
}

/// A mail row as a FETCH reads it: any subset of the stored mail, plus the
/// four synthetic fields the range query can project in place of the body
/// columns. `text_octets` / `html_octets` are `octet_length()` of the body
/// columns, and with `mail_id` / `user_id` they let a body be streamed out of
/// PostgreSQL instead of being loaded.
///
/// The JSON field names are the Bun server's.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct FetchMailInput {
    #[serde(default)]
    pub attachments: Option<Vec<MailAttachment>>,
    #[serde(default)]
    pub from: Option<MailAddress>,
    #[serde(default)]
    pub to: Option<MailAddress>,
    #[serde(default)]
    pub cc: Option<MailAddress>,
    #[serde(default)]
    pub bcc: Option<MailAddress>,
    #[serde(default, rename = "replyTo")]
    pub reply_to: Option<MailAddress>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default, rename = "messageId")]
    pub message_id: Option<String>,
    #[serde(default)]
    pub read: Option<bool>,
    #[serde(default)]
    pub saved: Option<bool>,
    #[serde(default)]
    pub deleted: Option<bool>,
    #[serde(default)]
    pub draft: Option<bool>,
    #[serde(default)]
    pub answered: Option<bool>,
    #[serde(default)]
    pub uid: Option<MailUid>,
    #[serde(default)]
    pub modseq: Option<f64>,
    #[serde(default)]
    pub rfc822_size: Option<f64>,
    #[serde(default)]
    pub text_octets: Option<f64>,
    #[serde(default)]
    pub html_octets: Option<f64>,
    #[serde(default)]
    pub mail_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_as_javascript_prints_them() {
        assert_eq!(js_number_string(0.0), "0");
        assert_eq!(js_number_string(-0.0), "0");
        assert_eq!(js_number_string(5.0), "5");
        assert_eq!(js_number_string(-12.0), "-12");
        assert_eq!(js_number_string(0.5), "0.5");
        assert_eq!(js_number_string(1.25e-7), "1.25e-7");
        assert_eq!(js_number_string(0.000001), "0.000001");
        assert_eq!(js_number_string(MAX_SAFE_INTEGER), "9007199254740991");
        assert_eq!(js_number_string(1e20), "100000000000000000000");
        assert_eq!(js_number_string(1e21), "1e+21");
        assert_eq!(js_number_string(1.5e25), "1.5e+25");
        assert_eq!(js_number_string(123456789012345680000.0), "123456789012345680000");
        assert_eq!(js_number_string(f64::INFINITY), "Infinity");
        assert_eq!(js_number_string(f64::NAN), "NaN");
        assert_eq!(json_number(f64::INFINITY), "null");
        assert_eq!(json_number(42.0), "42");
    }

    #[test]
    fn strings_escape_as_json_stringify_escapes_them() {
        assert_eq!(json_string("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(json_string("\u{1}\n\u{7f}é"), "\"\\u0001\\n\u{7f}é\"");
    }

    fn deep_not(levels: usize) -> SearchCriterion {
        let mut criterion = SearchCriterion::Seen;
        for _ in 0..levels {
            criterion = SearchCriterion::not(criterion);
        }
        criterion
    }

    #[test]
    fn a_deep_criterion_is_handled_without_recursion() {
        let levels = 200_000;
        let deep = deep_not(levels);
        assert_eq!(deep.depth(), levels);
        let copy = deep.clone();
        assert!(copy == deep);
        assert!(deep != deep_not(levels - 1));
        let json = deep.to_json_string();
        assert!(json.starts_with("{\"type\":\"NOT\",\"criterion\":{\"type\":\"NOT\""));
        let seen = "{\"type\":\"SEEN\"}";
        assert_eq!(json.len(), levels * "{\"type\":\"NOT\",\"criterion\":}".len() + seen.len());
        assert!(json.trim_end_matches('}').ends_with("{\"type\":\"SEEN\""));
        let count = deep.fold(&mut |_| 0usize, &mut |inner| inner + 1, &mut |a, b| a + b);
        assert_eq!(count, levels);
        drop(copy);
        drop(deep);
    }

    #[test]
    fn a_deep_or_tree_is_handled_without_recursion() {
        let mut criterion = SearchCriterion::Seen;
        for index in 0..100_000 {
            criterion = if index % 2 == 0 {
                SearchCriterion::or(criterion, SearchCriterion::Flagged)
            } else {
                SearchCriterion::or(SearchCriterion::From("a".into()), criterion)
            };
        }
        let copy = criterion.clone();
        assert!(copy == criterion);
        assert_eq!(criterion.depth(), 100_000);
        assert_eq!(format!("{:?}", SearchCriterion::not(SearchCriterion::Seen)), "Not({\"type\":\"SEEN\"})");
    }

    #[test]
    fn a_deep_uid_chain_is_handled_without_recursion() {
        let mut request = ImapRequest::Noop;
        for _ in 0..200_000 {
            request = ImapRequest::Uid(UidRequest::new("UID", request));
        }
        let copy = request.clone();
        assert!(copy == request);
        let json = request.to_json_string();
        assert!(json.starts_with("{\"type\":\"UID\",\"data\":{\"command\":\"UID\",\"request\":{\"type\":\"UID\""));
        assert!(json.trim_end_matches('}').ends_with("{\"type\":\"NOOP\""));
        assert_eq!(json.matches("\"command\":\"UID\"").count(), 200_000);
        let ImapRequest::Uid(outer) = request else { panic!("expected UID") };
        let (command, inner) = outer.into_parts();
        assert_eq!(command, "UID");
        assert_eq!(inner.type_name(), "UID");
    }

    #[test]
    fn requests_write_the_bun_object_shape() {
        let request = ImapRequest::Fetch(FetchRequest {
            sequence_set: SequenceSet::new(vec![SequenceRange::range(1.0, MAX_SAFE_INTEGER), SequenceRange::single(7.0)]),
            data_items: vec![
                FetchDataItem::Flags,
                FetchDataItem::Body(BodyFetch {
                    peek: true,
                    section: BodySection::HeaderFields { not: false, fields: vec!["FROM".into()] },
                    partial: Some(PartialRange { start: 0.0, length: 10.0 }),
                }),
            ],
            changed_since: None,
        });
        assert_eq!(
            request.to_json_string(),
            concat!(
                r#"{"type":"FETCH","data":{"sequenceSet":{"type":"sequence","ranges":[{"start":1,"end":9007199254740991},{"start":7}]},"#,
                r#""dataItems":[{"type":"FLAGS"},{"type":"BODY","peek":true,"section":{"type":"HEADER_FIELDS","not":false,"fields":["FROM"]},"partial":{"start":0,"length":10}}]}}"#
            )
        );
        assert_eq!(ImapRequest::Capability.to_json_string(), r#"{"type":"CAPABILITY"}"#);
    }
}
