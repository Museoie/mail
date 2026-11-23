//! Statements behind the IMAP mail reads (repositories/mails/imap-query.ts),
//! built with no pool involved so the emitted SQL, its projection and its
//! bound parameters can be asserted directly.
//!
//! A mailbox is the raw IMAP box path for a mapped box (`INBOX/accounts/sales`,
//! `Archive`), the view's name for a utility view (`Drafts`), and `None` for
//! the two views with no name of their own, `INBOX` and the unified
//! `Sent Messages`. Which branch a box takes is `uses_domain_uid_space`.
//!
//! ```ignore
//! let query = build_mails_by_range_query(user_id, None, false, 1.0, 50.0, true, &["mail_id", "read"], None);
//! let rows = run(&query.sql, &query.values).await?;
//! ```

use super::domain_scope::{domains_array_literal, mail_in_domains_expression};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;

use serde_json::json;

use super::model::{
    PartialMailModel, ANSWERED, DELETED, DRAFT, EXPUNGED, MAILBOX, MAIL_ID, MAIL_MAILBOX_UID, MESSAGE_ID, MODSEQ, READ,
    SAVED, SENT, UID, UID_DOMAIN, USER_ID,
};
use super::pivot_decisions::{STARRED_MAILBOX, TRASH_MAILBOX};
use super::table::{Filter, PreparedQuery};
use super::thread::COPY_MESSAGE_ID_SUFFIX;
use super::views::{
    membership_condition, membership_expression, membership_filter, uses_domain_uid_space, INBOX_ACCOUNTS_PREFIX,
    INBOX_VIEW,
};
use crate::imap::types::SequenceRange;
use crate::imap::util::JsDate;
use crate::postgres::Param;
use crate::{logger, params};

/// The mapped box a statement joins `mail_mailbox_uid` for, or `None` for a
/// box that enumerates `mails.uid_domain`.
fn mapped_box(mailbox: Option<&str>) -> Option<&str> {
    mailbox.filter(|name| !uses_domain_uid_space(Some(name)))
}

/// The statement behind a mailbox's message and unread counts.
///
/// `total` and `unread` describe what the mailbox contains, so they honour
/// its membership rule. UIDNEXT is not computed here: it comes from
/// `get_uid_next`, which reads the counter that assigns UIDs.
pub fn build_count_messages_query(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> PreparedQuery {
    let Some(mapped) = mapped_box(mailbox) else {
        let membership = membership_expression(mailbox, sent, "", domains);
        return PreparedQuery {
            sql: format!(
                "\n        SELECT\n          COUNT(*) FILTER (WHERE {membership}) as total,\n          COUNT(*) FILTER (WHERE read = FALSE AND {membership}) as unread\n        FROM mails\n        WHERE user_id = $1 AND sent = $2 AND expunged = FALSE\n      "
            ),
            values: params![user_id, sent],
        };
    };
    // The mapping row is the membership: a row exists exactly when the mail
    // is in this mailbox, so the INNER JOIN encodes it.
    let membership = membership_expression(Some(mapped), sent, "m.", domains);
    PreparedQuery {
        sql: format!(
            "\n        SELECT\n          COUNT(*) FILTER (WHERE {membership}) as total,\n          COUNT(*) FILTER (WHERE m.read = FALSE AND {membership}) as unread\n        FROM mails m\n        JOIN {MAIL_MAILBOX_UID} x\n          ON x.{USER_ID} = m.{USER_ID}\n          AND x.{MAILBOX} = $3\n          AND x.{MAIL_ID} = m.{MAIL_ID}\n        WHERE m.{USER_ID} = $1 AND m.{SENT} = $2 AND m.{EXPUNGED} = FALSE\n      "
        ),
        values: params![user_id, sent, mapped],
    }
}

/// A range read: the statement, its parameters, and the fields its rows carry.
#[derive(Debug, Clone, PartialEq)]
pub struct MailsByRangeQuery {
    pub sql: String,
    pub values: Vec<Param>,
    /// The projection the rows come back under: the `PartialMailModel` key set.
    pub selected_fields: Vec<String>,
}

/// The largest UID a range read asks PostgreSQL for.
const UID_RANGE_CEILING: f64 = 999_999_999.0;

/// `Math.min(end, 999999999)`, which is NaN for a NaN.
fn clamp_uid_end(end: f64) -> f64 {
    if end.is_nan() {
        end
    } else {
        end.min(UID_RANGE_CEILING)
    }
}

/// The statement behind an IMAP range read over `start..=end`: UIDs when
/// `use_uid`, 1-based sequence numbers otherwise.
///
/// `fields` names what to project; `["*"]` is every field a partial row may
/// carry. Unknown names are dropped, and `mail_id` is always projected: it is
/// the key the result is built on. `uid_mailbox`, `text_octets` and
/// `html_octets` are not columns: the first is the UID the box shows the mail
/// under, the others are `octet_length()` of the body columns, so a caller
/// can size a `BODY[]` literal without loading the body.
///
/// With `changed_since` only rows whose mod-sequence exceeds it are read
/// (RFC 4551 CHANGEDSINCE).
#[allow(clippy::too_many_arguments)]
pub fn build_mails_by_range_query<S: AsRef<str>>(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    start: f64,
    end: f64,
    use_uid: bool,
    fields: &[S],
    changed_since: Option<f64>,
    domains: Option<&[String]>,
) -> MailsByRangeQuery {
    let is_select_all = fields.len() == 1 && fields[0].as_ref() == "*";
    let resolved_fields: Vec<String> = if is_select_all {
        PartialMailModel::valid_fields().into_iter().map(str::to_string).collect()
    } else {
        fields.iter().map(|field| field.as_ref().to_string()).collect()
    };
    let unknown_fields: Vec<&String> =
        resolved_fields.iter().filter(|field| !PartialMailModel::is_valid_field(field)).collect();
    if !unknown_fields.is_empty() {
        logger::warn("getMailsByRange: unknown fields requested", json!({ "unknownFields": unknown_fields }));
    }
    let mut safe_fields: Vec<String> =
        resolved_fields.iter().filter(|field| PartialMailModel::is_valid_field(field)).cloned().collect();
    // Without `mail_id` every row would collapse onto one key.
    if !safe_fields.iter().any(|field| field == MAIL_ID) {
        safe_fields.insert(0, MAIL_ID.to_string());
    }

    let wants = |name: &str| safe_fields.iter().any(|field| field == name);
    let (wants_uid_mailbox, wants_text_octets, wants_html_octets) =
        (wants("uid_mailbox"), wants("text_octets"), wants("html_octets"));
    let mails_columns: Vec<&str> = safe_fields
        .iter()
        .map(String::as_str)
        .filter(|field| !matches!(*field, "uid_mailbox" | "text_octets" | "html_octets"))
        .collect();

    let octet_projections = |prefix: &str| -> String {
        let mut parts: Vec<String> = Vec::new();
        if wants_text_octets {
            parts.push(format!("octet_length({prefix}text) AS text_octets"));
        }
        if wants_html_octets {
            parts.push(format!("octet_length({prefix}html) AS html_octets"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(", {}", parts.join(", "))
        }
    };

    let (sql, mut values) = match mapped_box(mailbox) {
        None => {
            // The predicate names the parameter appended after this branch's
            // fixed arguments.
            let modseq_clause = if changed_since.is_some() { format!(" AND {MODSEQ} > $5") } else { String::new() };
            let projection = if mails_columns.is_empty() { "*".to_string() } else { mails_columns.join(", ") };
            let uid_mailbox_alias =
                if wants_uid_mailbox { format!(", {UID_DOMAIN} AS uid_mailbox") } else { String::new() };
            let field_list = format!("{projection}{uid_mailbox_alias}{}", octet_projections(""));
            let membership = membership_condition(mailbox, sent, "", domains);
            if use_uid {
                (
                    format!(
                        "\n          SELECT {field_list} FROM mails\n          WHERE user_id = $1 AND sent = $2 AND {UID_DOMAIN} >= $3 AND {UID_DOMAIN} <= $4\n            AND expunged = FALSE{membership}{modseq_clause}\n          ORDER BY {UID_DOMAIN} ASC\n        "
                    ),
                    params![user_id, sent, start, clamp_uid_end(end)],
                )
            } else {
                (
                    format!(
                        "\n          SELECT {field_list} FROM mails\n          WHERE user_id = $1 AND sent = $2 AND expunged = FALSE{membership}{modseq_clause}\n          ORDER BY {UID_DOMAIN} ASC\n          OFFSET $3 LIMIT $4\n        "
                    ),
                    params![user_id, sent, start - 1.0, end - start + 1.0],
                )
            }
        }
        Some(mapped) => {
            let modseq_clause = if changed_since.is_some() { format!(" AND m.{MODSEQ} > $6") } else { String::new() };
            // Columns of `mails` are qualified so the projection is
            // unambiguous across the join.
            let qualified_fields =
                mails_columns.iter().map(|field| format!("m.{field}")).collect::<Vec<_>>().join(", ");
            let uid_mailbox_alias = if wants_uid_mailbox {
                format!("{}x.{UID} AS uid_mailbox", if qualified_fields.is_empty() { "" } else { ", " })
            } else {
                String::new()
            };
            let octets = octet_projections("m.");
            let field_list = if qualified_fields.len() + uid_mailbox_alias.len() + octets.len() > 0 {
                format!("{qualified_fields}{uid_mailbox_alias}{octets}")
            } else {
                "m.*".to_string()
            };
            let membership = membership_condition(Some(mapped), sent, "m.", domains);
            if use_uid {
                (
                    format!(
                        "\n          SELECT {field_list} FROM mails m\n          JOIN {MAIL_MAILBOX_UID} x\n            ON x.{USER_ID} = m.{USER_ID}\n            AND x.{MAILBOX} = $3\n            AND x.{MAIL_ID} = m.{MAIL_ID}\n          WHERE m.{USER_ID} = $1 AND m.{SENT} = $2\n            AND x.{UID} >= $4 AND x.{UID} <= $5\n            AND m.{EXPUNGED} = FALSE{membership}{modseq_clause}\n          ORDER BY x.{UID} ASC\n        "
                    ),
                    params![user_id, sent, mapped, start, clamp_uid_end(end)],
                )
            } else {
                (
                    format!(
                        "\n          SELECT {field_list} FROM mails m\n          JOIN {MAIL_MAILBOX_UID} x\n            ON x.{USER_ID} = m.{USER_ID}\n            AND x.{MAILBOX} = $3\n            AND x.{MAIL_ID} = m.{MAIL_ID}\n          WHERE m.{USER_ID} = $1 AND m.{SENT} = $2 AND m.{EXPUNGED} = FALSE{membership}{modseq_clause}\n          ORDER BY x.{UID} ASC\n          OFFSET $4 LIMIT $5\n        "
                    ),
                    params![user_id, sent, mapped, start - 1.0, end - start + 1.0],
                )
            }
        }
    };
    if let Some(changed_since) = changed_since {
        values.push(Param::from(changed_since));
    }
    MailsByRangeQuery { sql, values, selected_fields: safe_fields }
}

// ---------------------------------------------------------------------------
// SEARCH
// ---------------------------------------------------------------------------

/// The fragment for a criterion the backend cannot express as a predicate but
/// which, by RFC 3501 §6.4.4, matches no message (KEYWORD, when no custom
/// keywords are stored). A literal `FALSE` fails the search closed; dropping
/// the criterion would leave it out of the WHERE and match every message.
pub const MATCH_NONE: &str = "FALSE";

/// A SEARCH key that matches text with `ILIKE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKey {
    Subject,
    From,
    To,
    Cc,
    Bcc,
    Body,
    Text,
    /// An alias of `Text` no parser produces.
    SubjectText,
}

impl TextKey {
    /// The Bun server's `type` tag.
    pub fn as_str(self) -> &'static str {
        match self {
            TextKey::Subject => "SUBJECT",
            TextKey::From => "FROM",
            TextKey::To => "TO",
            TextKey::Cc => "CC",
            TextKey::Bcc => "BCC",
            TextKey::Body => "BODY",
            TextKey::Text => "TEXT",
            TextKey::SubjectText => "SUBJECT_TEXT",
        }
    }
}

/// A SEARCH key that compares the mail's date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateKey {
    Before,
    On,
    Since,
    SentBefore,
    SentOn,
    SentSince,
}

impl DateKey {
    /// The Bun server's `type` tag.
    pub fn as_str(self) -> &'static str {
        match self {
            DateKey::Before => "BEFORE",
            DateKey::On => "ON",
            DateKey::Since => "SINCE",
            DateKey::SentBefore => "SENTBEFORE",
            DateKey::SentOn => "SENTON",
            DateKey::SentSince => "SENTSINCE",
        }
    }
}

/// A SEARCH key that compares the message size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeKey {
    Larger,
    Smaller,
}

impl SizeKey {
    /// The Bun server's `type` tag.
    pub fn as_str(self) -> &'static str {
        match self {
            SizeKey::Larger => "LARGER",
            SizeKey::Smaller => "SMALLER",
        }
    }
}

/// One SEARCH criterion in the shape the statement builder reads: the Bun
/// server's `{ type, value }`, which `imap::store::simplify_criterion`
/// produces from a parsed [`crate::imap::types::SearchCriterion`].
///
/// `Not` and `Or` nest as deep as the client's command did. Building a
/// statement from a criterion and dropping one never recurse; cloning,
/// comparing and printing do, and are for tests.
#[derive(Debug, Clone, PartialEq)]
pub enum SimplifiedCriterion {
    /// `{ type }`: a key with no operand, such as `ALL`, `SEEN` or `KEYWORD`,
    /// or a type the builder does not know, which matches nothing.
    Bare(String),
    /// `{ type, value }` for a text key.
    Text { key: TextKey, value: String },
    /// `{ type: "HEADER", value: { field, text } }`.
    Header { field: String, text: String },
    /// `{ type, value }` for a date key.
    Date { key: DateKey, value: JsDate },
    /// `{ type, value }` for a size key.
    Size { key: SizeKey, value: f64 },
    /// `{ type: "UID_SET", value: ranges }`: the UIDs of a `UID <set>` key.
    UidSet(Vec<SequenceRange>),
    /// `{ type: "NOT", value }`.
    Not(Operand),
    /// `{ type: "OR", value: { left, right } }`.
    Or(Operand, Operand),
}

impl SimplifiedCriterion {
    /// A key with no operand.
    pub fn bare(kind: &str) -> SimplifiedCriterion {
        SimplifiedCriterion::Bare(kind.to_string())
    }

    pub fn text(key: TextKey, value: &str) -> SimplifiedCriterion {
        SimplifiedCriterion::Text { key, value: value.to_string() }
    }

    /// `NOT <criterion>`.
    #[allow(clippy::should_implement_trait)]
    pub fn not(criterion: SimplifiedCriterion) -> SimplifiedCriterion {
        SimplifiedCriterion::Not(Operand::new(criterion))
    }

    /// `OR <left> <right>`.
    pub fn or(left: SimplifiedCriterion, right: SimplifiedCriterion) -> SimplifiedCriterion {
        SimplifiedCriterion::Or(Operand::new(left), Operand::new(right))
    }
}

/// The operand of a `Not` or an `Or`. Dropping one walks the tree with a heap
/// stack, so a criterion nested tens of thousands of levels deep cannot run
/// the thread out of stack on its way out.
#[derive(Debug, Clone, PartialEq)]
pub struct Operand(Box<SimplifiedCriterion>);

impl Operand {
    pub fn new(criterion: SimplifiedCriterion) -> Operand {
        Operand(Box::new(criterion))
    }
}

impl std::ops::Deref for Operand {
    type Target = SimplifiedCriterion;

    fn deref(&self) -> &SimplifiedCriterion {
        &self.0
    }
}

/// Moves the operands of `node` onto `pending`, leaving childless keys behind.
fn detach_operands(node: &mut SimplifiedCriterion, pending: &mut Vec<SimplifiedCriterion>) {
    let mut detach =
        |operand: &mut Operand| pending.push(std::mem::replace(&mut *operand.0, SimplifiedCriterion::Bare(String::new())));
    match node {
        SimplifiedCriterion::Not(inner) => detach(inner),
        SimplifiedCriterion::Or(left, right) => {
            detach(left);
            detach(right);
        }
        _ => {}
    }
}

impl Drop for Operand {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        detach_operands(&mut self.0, &mut pending);
        while let Some(mut node) = pending.pop() {
            detach_operands(&mut node, &mut pending);
        }
    }
}

/// A search that binds an Invalid Date: a date criterion whose date, or
/// whose day after, JavaScript cannot represent. The Bun server fails to
/// bind such a parameter, so its statement is never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDateParam;

impl fmt::Display for InvalidDateParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Invalid Date")
    }
}

impl std::error::Error for InvalidDateParam {}

/// Binds a date. An Invalid Date binds a placeholder whose place is noted in
/// `invalid`: it fails the statement only if no operator discards it.
fn push_date(date: &JsDate, values: &mut Vec<Param>, invalid: &mut Vec<usize>) {
    match date.to_iso_string() {
        Some(iso) => values.push(Param::from(iso)),
        None => {
            invalid.push(values.len());
            values.push(Param::Null);
        }
    }
}

/// The day after `date`, as `setDate(getDate() + 1)` moves it.
fn next_day(date: &JsDate) -> JsDate {
    const DAY_MS: i64 = 86_400_000;
    match date.timestamp_millis().and_then(|ms| ms.checked_add(DAY_MS)) {
        Some(ms) => JsDate::from_ms(ms),
        None => JsDate::invalid(),
    }
}

/// The fragment of a key that is not an operator: `None` for one that
/// imposes no constraint, [`MATCH_NONE`] for one that matches nothing.
fn key_clause(
    criterion: &SimplifiedCriterion,
    uid_field: &str,
    values: &mut Vec<Param>,
    invalid: &mut Vec<usize>,
) -> Option<String> {
    let fragment = |sql: &str| Some(sql.to_string());
    match criterion {
        SimplifiedCriterion::Bare(kind) => match kind.to_uppercase().as_str() {
            "ALL" => None,
            "UNSEEN" => fragment("read = FALSE"),
            "SEEN" => fragment("read = TRUE"),
            "FLAGGED" => fragment("saved = TRUE"),
            "UNFLAGGED" => fragment("saved = FALSE"),
            "ANSWERED" => fragment("answered = TRUE"),
            "UNANSWERED" => fragment("answered = FALSE"),
            "DELETED" => fragment("deleted = TRUE"),
            "UNDELETED" => fragment("deleted = FALSE"),
            "DRAFT" => fragment("draft = TRUE"),
            "UNDRAFT" => fragment("draft = FALSE"),
            // NEW is RECENT and UNSEEN; \Recent is not tracked, so RECENT
            // and OLD match everything.
            "NEW" => fragment("read = FALSE"),
            "OLD" | "RECENT" => None,
            // No custom keywords are stored: none can match, and every
            // message lacks any given one.
            "KEYWORD" => fragment(MATCH_NONE),
            "UNKEYWORD" => None,
            _ => fragment(MATCH_NONE),
        },
        SimplifiedCriterion::Text { key, value } => {
            values.push(Param::from(format!("%{value}%")));
            let p = values.len();
            Some(match key {
                TextKey::Subject => format!("subject ILIKE ${p}"),
                TextKey::From => format!("from_text ILIKE ${p}"),
                TextKey::To => format!("to_text ILIKE ${p}"),
                TextKey::Cc => format!("cc_text ILIKE ${p}"),
                TextKey::Bcc => format!("bcc_text ILIKE ${p}"),
                // RFC 3501 §6.4.4: BODY matches the body, TEXT the header
                // and the body.
                TextKey::Body => format!("text ILIKE ${p}"),
                TextKey::Text | TextKey::SubjectText => {
                    format!("(subject ILIKE ${p} OR from_text ILIKE ${p} OR to_text ILIKE ${p} OR text ILIKE ${p})")
                }
            })
        }
        SimplifiedCriterion::Header { field, text } => {
            let column = match field.to_lowercase().as_str() {
                "subject" => "subject",
                "from" => "from_text",
                "to" => "to_text",
                "message-id" => "message_id",
                _ => return fragment(MATCH_NONE),
            };
            values.push(Param::from(format!("%{text}%")));
            Some(format!("{column} ILIKE ${}", values.len()))
        }
        // The SENT* keys read the same column: there is only one date.
        SimplifiedCriterion::Date { key, value } => match key {
            DateKey::Before | DateKey::SentBefore => {
                push_date(value, values, invalid);
                Some(format!("date < ${}", values.len()))
            }
            DateKey::On | DateKey::SentOn => {
                push_date(value, values, invalid);
                push_date(&next_day(value), values, invalid);
                Some(format!("date >= ${} AND date < ${}", values.len() - 1, values.len()))
            }
            DateKey::Since | DateKey::SentSince => {
                push_date(value, values, invalid);
                Some(format!("date >= ${}", values.len()))
            }
        },
        // RFC822.SIZE is not searchable.
        SimplifiedCriterion::Size { .. } => fragment(MATCH_NONE),
        SimplifiedCriterion::UidSet(ranges) => {
            let parts: Vec<String> = ranges
                .iter()
                .map(|range| match range.end {
                    None => {
                        values.push(Param::from(range.start));
                        format!("{uid_field} = ${}", values.len())
                    }
                    Some(end) => {
                        values.push(Param::from(range.start));
                        values.push(Param::from(end));
                        format!("({uid_field} >= ${} AND {uid_field} <= ${})", values.len() - 1, values.len())
                    }
                })
                .collect();
            match parts.len() {
                0 => None,
                1 => parts.into_iter().next(),
                _ => Some(format!("({})", parts.join(" OR "))),
            }
        }
        // Operators are reduced by the caller.
        SimplifiedCriterion::Not(_) | SimplifiedCriterion::Or(_, _) => None,
    }
}

/// The boolean SQL fragment for one criterion: `None` when it imposes no
/// constraint, [`MATCH_NONE`] when it matches nothing.
///
/// Bound parameters are appended to `values`, and `$N` follows its length, so
/// a fragment can be built onto a statement that already binds some. When an
/// operator discards a side that bound a parameter (`OR SUBJECT x ALL`
/// matches everything), `values` is rolled back to where that side began:
/// otherwise the parameter would be bound and never referenced, and
/// PostgreSQL would reject the statement.
///
/// `uid_field` is the column a `UidSet` compares.
///
/// Fails when the fragment binds an Invalid Date. One that an operator
/// discarded is not bound, and does not fail it.
pub fn build_criterion_clause(
    criterion: &SimplifiedCriterion,
    uid_field: &str,
    values: &mut Vec<Param>,
) -> Result<Option<String>, InvalidDateParam> {
    enum Step<'a> {
        Visit(&'a SimplifiedCriterion),
        /// Reduce a NOT whose operand began at this length of `values`.
        Not(usize),
        /// Reduce an OR whose left side began at this length of `values`.
        Or(usize),
    }
    /// Discards what a side bound from `saved_len` on.
    fn roll_back(values: &mut Vec<Param>, invalid: &mut Vec<usize>, saved_len: usize) {
        values.truncate(saved_len);
        invalid.retain(|at| *at < saved_len);
    }
    let mut work = vec![Step::Visit(criterion)];
    let mut built: Vec<Option<Fragment>> = Vec::new();
    // Where `values` holds a placeholder for an Invalid Date.
    let mut invalid: Vec<usize> = Vec::new();
    while let Some(step) = work.pop() {
        match step {
            Step::Visit(SimplifiedCriterion::Not(inner)) => {
                work.push(Step::Not(values.len()));
                work.push(Step::Visit(inner));
            }
            Step::Visit(SimplifiedCriterion::Or(left, right)) => {
                work.push(Step::Or(values.len()));
                work.push(Step::Visit(right));
                work.push(Step::Visit(left));
            }
            Step::Visit(key) => built.push(key_clause(key, uid_field, values, &mut invalid).map(Fragment::from)),
            Step::Not(saved_len) => {
                // NOT match-all is match-none and the reverse; both discard
                // the operand, and with it any parameter it bound.
                let reduced = match built.pop().flatten() {
                    None => {
                        roll_back(values, &mut invalid, saved_len);
                        Some(Fragment::from(MATCH_NONE.to_string()))
                    }
                    Some(inner) if inner.is_match_none() => {
                        roll_back(values, &mut invalid, saved_len);
                        None
                    }
                    Some(inner) => Some(inner.wrapped("NOT (", ")")),
                };
                built.push(reduced);
            }
            Step::Or(saved_len) => {
                let right = built.pop().flatten();
                let left = built.pop().flatten();
                let reduced = match (left, right) {
                    // A side that matches everything makes the whole OR match
                    // everything.
                    (None, _) | (_, None) => {
                        roll_back(values, &mut invalid, saved_len);
                        None
                    }
                    (Some(left), Some(right)) if left.is_match_none() && right.is_match_none() => {
                        roll_back(values, &mut invalid, saved_len);
                        Some(left)
                    }
                    // `X OR none` is `X`. The match-none side bound nothing,
                    // so the kept side's parameters stay aligned.
                    (Some(left), Some(right)) if left.is_match_none() => Some(right),
                    (Some(left), Some(right)) if right.is_match_none() => Some(left),
                    (Some(left), Some(right)) => Some(left.joined(" OR ", right).wrapped("(", ")")),
                };
                built.push(reduced);
            }
        }
    }
    if !invalid.is_empty() {
        return Err(InvalidDateParam);
    }
    Ok(built.pop().flatten().map(Fragment::into_sql))
}

/// A SQL fragment under construction, kept in pieces so that wrapping one in
/// an operator costs the same however long it has grown: a criterion nested
/// thousands of levels deep is assembled once, at the end.
struct Fragment(VecDeque<Cow<'static, str>>);

impl From<String> for Fragment {
    fn from(sql: String) -> Fragment {
        Fragment(VecDeque::from([Cow::Owned(sql)]))
    }
}

impl Fragment {
    fn is_match_none(&self) -> bool {
        self.0.len() == 1 && self.0[0] == MATCH_NONE
    }

    fn wrapped(mut self, open: &'static str, close: &'static str) -> Fragment {
        self.0.push_front(Cow::Borrowed(open));
        self.0.push_back(Cow::Borrowed(close));
        self
    }

    /// `self`, `separator`, then `other`, moving the shorter side's pieces.
    fn joined(mut self, separator: &'static str, mut other: Fragment) -> Fragment {
        if self.0.len() >= other.0.len() {
            self.0.push_back(Cow::Borrowed(separator));
            self.0.extend(other.0);
            self
        } else {
            other.0.push_front(Cow::Borrowed(separator));
            for piece in self.0.into_iter().rev() {
                other.0.push_front(piece);
            }
            other
        }
    }

    fn into_sql(self) -> String {
        let mut sql = String::with_capacity(self.0.iter().map(|piece| piece.len()).sum());
        for piece in &self.0 {
            sql.push_str(piece);
        }
        sql
    }
}

/// The statement behind a SEARCH: the UIDs of the mailbox's messages that
/// match every criterion, ascending. It carries no LIMIT: RFC 3501 §6.4.4
/// has SEARCH return every match.
pub fn build_search_mails_by_uid_query(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    criteria: &[SimplifiedCriterion],
    domains: Option<&[String]>,
) -> Result<PreparedQuery, InvalidDateParam> {
    let mapped = mapped_box(mailbox);
    // The column a criterion's UID comparison reads.
    let uid_field = if mapped.is_some() { format!("x.{UID}") } else { UID_DOMAIN.to_string() };

    // Expunged messages and anything the mailbox does not show are always
    // excluded: SEARCH must not return a UID the client cannot FETCH.
    let mut conditions: Vec<String> = vec![
        "m.user_id = $1".to_string(),
        "m.sent = $2".to_string(),
        "m.expunged = FALSE".to_string(),
        membership_expression(mailbox, sent, "m.", domains),
    ];
    let mut values = params![user_id, sent];

    let from_clause = match mapped {
        None => "mails m".to_string(),
        Some(mapped) => {
            conditions.push(format!("x.{USER_ID} = m.{USER_ID}"));
            conditions.push(format!("x.{MAILBOX} = $3"));
            conditions.push(format!("x.{MAIL_ID} = m.{MAIL_ID}"));
            values.push(Param::from(mapped));
            format!("mails m, {MAIL_MAILBOX_UID} x")
        }
    };

    for criterion in criteria {
        // The fragments name columns of `mails` unqualified, which stays
        // unambiguous under the join.
        if let Some(fragment) = build_criterion_clause(criterion, &uid_field, &mut values)? {
            if !fragment.is_empty() {
                conditions.push(fragment);
            }
        }
    }

    Ok(PreparedQuery {
        sql: format!(
            "\n      SELECT {uid_field} as uid FROM {from_clause}\n      WHERE {}\n      ORDER BY {uid_field} ASC\n    ",
            conditions.join(" AND ")
        ),
        values,
    })
}

// ---------------------------------------------------------------------------
// UID lists
// ---------------------------------------------------------------------------

/// The statement behind the sequence-number map: every UID the mailbox
/// shows, ascending.
pub fn build_all_uids_query(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> PreparedQuery {
    match mapped_box(mailbox) {
        None => PreparedQuery {
            sql: format!(
                "\n        SELECT {UID_DOMAIN} as uid FROM mails\n        WHERE user_id = $1 AND sent = $2 AND expunged = FALSE{}\n        ORDER BY {UID_DOMAIN} ASC\n      ",
                membership_condition(mailbox, sent, "", domains)
            ),
            values: params![user_id, sent],
        },
        Some(mapped) => PreparedQuery {
            sql: format!(
                "\n        SELECT x.{UID} as uid FROM mails m\n        JOIN {MAIL_MAILBOX_UID} x\n          ON x.{USER_ID} = m.{USER_ID}\n          AND x.{MAILBOX} = $3\n          AND x.{MAIL_ID} = m.{MAIL_ID}\n        WHERE m.{USER_ID} = $1 AND m.{SENT} = $2 AND m.{EXPUNGED} = FALSE{}\n        ORDER BY x.{UID} ASC\n      ",
                membership_condition(Some(mapped), sent, "m.", domains)
            ),
            values: params![user_id, sent, mapped],
        },
    }
}

/// The statement behind `[UNSEEN <seq>]`: the lowest UID among the
/// mailbox's unread messages.
pub fn build_first_unseen_uid_query(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> PreparedQuery {
    match mapped_box(mailbox) {
        None => PreparedQuery {
            sql: format!(
                "\n        SELECT {UID_DOMAIN} as uid FROM mails\n        WHERE user_id = $1 AND sent = $2 AND expunged = FALSE AND read = FALSE{}\n        ORDER BY {UID_DOMAIN} ASC\n        LIMIT 1\n      ",
                membership_condition(mailbox, sent, "", domains)
            ),
            values: params![user_id, sent],
        },
        Some(mapped) => PreparedQuery {
            sql: format!(
                "\n        SELECT x.{UID} as uid FROM mails m\n        JOIN {MAIL_MAILBOX_UID} x\n          ON x.{USER_ID} = m.{USER_ID}\n          AND x.{MAILBOX} = $3\n          AND x.{MAIL_ID} = m.{MAIL_ID}\n        WHERE m.{USER_ID} = $1 AND m.{SENT} = $2 AND m.{EXPUNGED} = FALSE AND m.read = FALSE{}\n        ORDER BY x.{UID} ASC\n        LIMIT 1\n      ",
                membership_condition(Some(mapped), sent, "m.", domains)
            ),
            values: params![user_id, sent, mapped],
        },
    }
}

// ---------------------------------------------------------------------------
// EXPUNGE and MOVE
// ---------------------------------------------------------------------------

fn with_membership(
    mut filters: Vec<(&'static str, Filter)>,
    mailbox: Option<&str>,
    sent: bool,
) -> Vec<(&'static str, Filter)> {
    for (column, required) in membership_filter(mailbox, sent) {
        match filters.iter_mut().find(|(name, _)| *name == column) {
            Some(entry) => entry.1 = Filter::from(required),
            None => filters.push((column, Filter::from(required))),
        }
    }
    filters
}

/// The rows an EXPUNGE addresses on a domain view of the sent lane, the
/// unified `Sent Messages` (the received lane's views take
/// `build_expunge_deleted_inbox_query`). EXPUNGE removes `\Deleted` messages
/// from the selected mailbox, so a mail the box does not show is out of reach.
pub fn build_expunge_deleted_filters(user_id: &str, mailbox: Option<&str>, sent: bool) -> Vec<(&'static str, Filter)> {
    with_membership(
        vec![
            (USER_ID, Filter::from(user_id)),
            (SENT, Filter::from(sent)),
            (DELETED, Filter::from(true)),
            (EXPUNGED, Filter::from(false)),
        ],
        mailbox,
        sent,
    )
}

/// The domain predicate as a `WHERE` suffix, empty when unscoped.
fn domain_condition(domains: Option<&[String]>, prefix: &str) -> String {
    match domains {
        None => String::new(),
        Some(domains) => format!(" AND {}", mail_in_domains_expression(&domains_array_literal(domains), prefix)),
    }
}

/// The domain-view EXPUNGE for a login scoped to some domains: the filter bag
/// above cannot carry the domain predicate, so the rows are selected here and
/// expunged by id, like the mapped boxes.
pub fn build_scoped_expunge_deleted_select_query(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    domains: &[String],
) -> PreparedQuery {
    PreparedQuery {
        sql: format!(
            "\n      SELECT {MAIL_ID} as mail_id, {UID_DOMAIN} as uid FROM mails\n      WHERE {USER_ID} = $1 AND {SENT} = $2\n        AND {DELETED} = TRUE AND {EXPUNGED} = FALSE{}\n    ",
            membership_condition(mailbox, sent, "", Some(domains))
        ),
        values: params![user_id, sent],
    }
}

/// The scoped twin of `build_expunge_uids_filters`, for the same reason.
pub fn build_scoped_expunge_uids_select_query(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    uids: &[i64],
    domains: &[String],
) -> PreparedQuery {
    let uids: Vec<String> = uids.iter().map(i64::to_string).collect();
    PreparedQuery {
        sql: format!(
            "\n      SELECT {MAIL_ID} as mail_id, {UID_DOMAIN} as uid FROM mails\n      WHERE {USER_ID} = $1 AND {SENT} = $2\n        AND {UID_DOMAIN} = ANY($3::bigint[])\n        AND {EXPUNGED} = FALSE{}\n    ",
            membership_condition(mailbox, sent, "", Some(domains))
        ),
        values: vec![Param::from(user_id), Param::from(sent), Param::array(&uids)],
    }
}

/// The rows an EXPUNGE addresses on INBOX or a utility view of the received
/// lane, with the `uid_domain` the view numbers them by: its `\Deleted`
/// mails, less those filed somewhere else.
///
/// INBOX shows every received mail, so a copy filed into Trash (`\Deleted` by
/// being there) or into a user mailbox is listed in INBOX too, and the view's
/// predicate alone let an INBOX EXPUNGE destroy everything the user had moved
/// to Trash. A mail is the view's to expunge when
///
/// - it holds a row in the INBOX tree: `INBOX`, or a per-address box when the
///   INBOX row was skipped on a `uid_domain` collision; or
/// - it is not a copy and holds no row but the Starred / Trash pivots its own
///   flags add. That is mail stored before the mapping rows were written: the
///   STORE that flags it `\Deleted` files it into Trash like any other mail.
///
/// Left out is a COPY / MOVE copy (`COPY_MESSAGE_ID_SUFFIX`) or a mail filed
/// into a user mailbox, with no row in the INBOX tree. A mail APPENDed into
/// Trash or Starred holds only that row, like old mail flagged in INBOX, and
/// the two cannot be told apart, so it stays in reach.
pub fn build_expunge_deleted_inbox_query(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> PreparedQuery {
    PreparedQuery {
        sql: format!(
            "\n      SELECT m.{MAIL_ID} as mail_id, m.{UID_DOMAIN} as uid FROM mails m\n      WHERE m.{USER_ID} = $1 AND m.{SENT} = $2\n        AND m.{DELETED} = TRUE AND m.{EXPUNGED} = FALSE{}\n        AND (\n          EXISTS (\n            SELECT 1 FROM {MAIL_MAILBOX_UID} x\n            WHERE x.{USER_ID} = m.{USER_ID} AND x.{MAIL_ID} = m.{MAIL_ID}\n              AND (x.{MAILBOX} = $3 OR x.{MAILBOX} LIKE $4)\n          )\n          OR (\n            m.{MESSAGE_ID} NOT LIKE $5\n            AND NOT EXISTS (\n              SELECT 1 FROM {MAIL_MAILBOX_UID} x\n              WHERE x.{USER_ID} = m.{USER_ID} AND x.{MAIL_ID} = m.{MAIL_ID}\n                AND x.{MAILBOX} NOT IN ($6, $7)\n            )\n          )\n        )\n    ",
            membership_condition(mailbox, sent, "m.", domains)
        ),
        values: params![
            user_id,
            sent,
            INBOX_VIEW,
            format!("{INBOX_ACCOUNTS_PREFIX}%"),
            format!("%{COPY_MESSAGE_ID_SUFFIX}"),
            STARRED_MAILBOX,
            TRASH_MAILBOX
        ],
    }
}

/// The mapped-box twin: the `\Deleted` mails of the box and the UID each
/// holds there.
pub fn build_expunge_deleted_select_query(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> PreparedQuery {
    PreparedQuery {
        sql: format!(
            "\n      SELECT m.{MAIL_ID} as mail_id, x.{UID} as uid FROM mails m\n      JOIN {MAIL_MAILBOX_UID} x\n        ON x.{USER_ID} = m.{USER_ID}\n        AND x.{MAILBOX} = $3\n        AND x.{MAIL_ID} = m.{MAIL_ID}\n      WHERE m.{USER_ID} = $1 AND m.{SENT} = $2\n        AND m.{DELETED} = TRUE AND m.{EXPUNGED} = FALSE{}\n    ",
            membership_condition(mailbox, sent, "m.", domains)
        ),
        values: params![user_id, sent, mailbox],
    }
}

/// The rows a MOVE removes from its source on a domain view: the named UIDs
/// the box shows, whatever their `\Deleted` flag (RFC 6851 §3.3).
pub fn build_expunge_uids_filters(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    uids: &[i64],
) -> Vec<(&'static str, Filter)> {
    with_membership(
        vec![
            (USER_ID, Filter::from(user_id)),
            (SENT, Filter::from(sent)),
            (EXPUNGED, Filter::from(false)),
            (UID_DOMAIN, Filter::any_of(uids)),
        ],
        mailbox,
        sent,
    )
}

/// The mapped-box twin: the mails holding the named UIDs in the box.
pub fn build_expunge_uids_select_query(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    uids: &[i64],
    domains: Option<&[String]>,
) -> PreparedQuery {
    let uid_placeholders = (0..uids.len()).map(|index| format!("${}", index + 4)).collect::<Vec<_>>().join(",");
    let mut values = params![user_id, sent, mailbox];
    values.extend(uids.iter().map(|uid| Param::from(*uid)));
    PreparedQuery {
        sql: format!(
            "\n      SELECT m.{MAIL_ID} as mail_id, x.{UID} as uid FROM mails m\n      JOIN {MAIL_MAILBOX_UID} x\n        ON x.{USER_ID} = m.{USER_ID}\n        AND x.{MAILBOX} = $3\n        AND x.{MAIL_ID} = m.{MAIL_ID}\n      WHERE m.{USER_ID} = $1\n        AND m.{SENT} = $2\n        AND x.{UID} IN ({uid_placeholders})\n        AND m.{EXPUNGED} = FALSE{}\n    ",
            membership_condition(mailbox, sent, "m.", domains)
        ),
        values,
    }
}

// ---------------------------------------------------------------------------
// Flag pushes
// ---------------------------------------------------------------------------

/// The statement behind an untagged `FETCH (FLAGS …)` push: the UID and the
/// five flags of the named mails, as the mailbox shows them. On a domain view
/// the domain UID is authoritative and there is no join; a mail outside the
/// view is dropped by the session, which finds no sequence number for it.
pub fn build_mail_flag_updates_query<S: AsRef<str>>(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    mail_ids: &[S],
    domains: Option<&[String]>,
) -> PreparedQuery {
    let ids = Param::array(mail_ids);
    match mapped_box(mailbox) {
        None => PreparedQuery {
            sql: format!(
                "\n        SELECT {MAIL_ID} AS mail_id, {UID_DOMAIN} AS uid, {READ}, {SAVED}, {DELETED}, {DRAFT}, {ANSWERED} FROM mails\n        WHERE {USER_ID} = $1 AND {SENT} = $2 AND {EXPUNGED} = FALSE\n          AND {MAIL_ID} = ANY($3){}\n      ",
                domain_condition(domains, "")
            ),
            values: vec![Param::from(user_id), Param::from(sent), ids],
        },
        Some(mapped) => PreparedQuery {
            sql: format!(
                "\n        SELECT m.{MAIL_ID} AS mail_id, x.{UID} AS uid,\n               m.{READ}, m.{SAVED}, m.{DELETED}, m.{DRAFT}, m.{ANSWERED}\n        FROM mails m\n        JOIN {MAIL_MAILBOX_UID} x\n          ON x.{USER_ID} = m.{USER_ID}\n          AND x.{MAILBOX} = $3\n          AND x.{MAIL_ID} = m.{MAIL_ID}\n        WHERE m.{USER_ID} = $1 AND m.{SENT} = $2 AND m.{EXPUNGED} = FALSE\n          AND m.{MAIL_ID} = ANY($4){}\n      ",
                domain_condition(domains, "m.")
            ),
            values: vec![Param::from(user_id), Param::from(sent), Param::from(mapped), ids],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "11111111-1111-1111-1111-111111111111";
    const ACCOUNT_BOX: &str = "INBOX/accounts/a";
    const USER_BOX: &str = "Archive";
    const BOXES: [Option<&str>; 5] = [None, Some("Drafts"), Some("Junk"), Some(ACCOUNT_BOX), Some(USER_BOX)];

    fn prefix_of(mailbox: Option<&str>) -> &'static str {
        if uses_domain_uid_space(mailbox) {
            ""
        } else {
            "m."
        }
    }

    #[derive(Clone)]
    struct Range<'a> {
        mailbox: Option<&'a str>,
        use_uid: bool,
        fields: Vec<&'a str>,
        changed_since: Option<f64>,
    }

    impl Default for Range<'_> {
        fn default() -> Self {
            Range { mailbox: None, use_uid: true, fields: vec!["*"], changed_since: None }
        }
    }

    fn range(options: Range) -> MailsByRangeQuery {
        build_mails_by_range_query(
            USER,
            options.mailbox,
            false,
            3.0,
            9.0,
            options.use_uid,
            &options.fields,
            options.changed_since,
            None,
        )
    }

    // imap-query.test.ts — "buildCountMessagesQuery"

    #[test]
    fn counts_only_rows_the_mailbox_contains_in_both_branches() {
        for mailbox in BOXES {
            let sql = build_count_messages_query(USER, mailbox, false, None).sql;
            let prefix = prefix_of(mailbox);
            let membership = membership_expression(mailbox, false, prefix, None);
            assert!(sql.contains(&format!("COUNT(*) FILTER (WHERE {membership}) as total")));
            assert!(sql.contains(&format!("COUNT(*) FILTER (WHERE {prefix}read = FALSE AND {membership}) as unread")));
        }
    }

    #[test]
    fn applies_the_inbox_predicate_so_quarantined_mail_is_not_counted() {
        let sql = build_count_messages_query(USER, None, false, None).sql;
        assert!(sql.contains("is_spam = FALSE AND draft = FALSE"));
    }

    #[test]
    fn selects_the_utility_views_by_their_own_flag() {
        assert!(build_count_messages_query(USER, Some("Drafts"), false, None).sql.contains("FILTER (WHERE draft = TRUE)"));
        assert!(build_count_messages_query(USER, Some("Junk"), false, None).sql.contains("FILTER (WHERE is_spam = TRUE)"));
    }

    #[test]
    fn does_not_derive_uidnext_from_a_max_over_the_surviving_rows() {
        for mailbox in [None, Some(ACCOUNT_BOX)] {
            let sql = build_count_messages_query(USER, mailbox, false, None).sql;
            assert!(!sql.contains("MAX("));
            assert!(!sql.contains("max_uid"));
        }
    }

    #[test]
    fn count_hides_expunged_rows_from_both_branches() {
        assert!(build_count_messages_query(USER, None, false, None).sql.contains("expunged = FALSE"));
        assert!(build_count_messages_query(USER, Some(ACCOUNT_BOX), false, None).sql.contains("m.expunged = FALSE"));
    }

    #[test]
    fn count_joins_the_mapping_table_on_the_mailbox_path_the_caller_passed() {
        let query = build_count_messages_query(USER, Some(USER_BOX), false, None);
        assert!(query.sql.contains("JOIN mail_mailbox_uid x"));
        assert!(query.sql.contains("AND x.mailbox = $3"));
        assert_eq!(query.values, params![USER, false, USER_BOX]);
    }

    #[test]
    fn count_binds_no_mailbox_on_the_domain_branch() {
        assert_eq!(build_count_messages_query(USER, None, true, None).values, params![USER, true]);
    }

    // imap-query.test.ts — "buildMailsByRangeQuery — synthetic projections"

    #[test]
    fn projects_octet_length_of_the_body_columns_when_asked() {
        let sql = range(Range { fields: vec!["mail_id", "text_octets", "html_octets"], ..Default::default() }).sql;
        assert!(sql.contains("octet_length(text) AS text_octets"));
        assert!(sql.contains("octet_length(html) AS html_octets"));
    }

    #[test]
    fn qualifies_the_octet_length_with_the_mails_alias_in_the_join_branch() {
        let sql = range(Range {
            mailbox: Some(ACCOUNT_BOX),
            fields: vec!["mail_id", "text_octets", "html_octets"],
            ..Default::default()
        })
        .sql;
        assert!(sql.contains("octet_length(m.text) AS text_octets"));
        assert!(sql.contains("octet_length(m.html) AS html_octets"));
    }

    #[test]
    fn omits_the_octet_projections_when_they_were_not_requested() {
        let sql = range(Range { fields: vec!["mail_id", "subject"], ..Default::default() }).sql;
        assert!(!sql.contains("octet_length"));
    }

    #[test]
    fn never_emits_a_synthetic_name_as_a_mails_column_reference() {
        for mailbox in [None, Some(ACCOUNT_BOX)] {
            let sql = range(Range {
                mailbox,
                fields: vec!["mail_id", "uid_mailbox", "text_octets", "html_octets"],
                ..Default::default()
            })
            .sql;
            let projection = &sql[sql.find("SELECT").unwrap() + "SELECT".len()..sql.find("FROM").unwrap()];
            let projected: Vec<&str> =
                projection.split(',').map(|item| item.trim()).map(|item| item.strip_prefix("m.").unwrap_or(item)).collect();
            // An `AS <synthetic>` alias is the right shape; a bare name is
            // the leak, so whole projection items are compared.
            assert!(!projected.contains(&"text_octets"));
            assert!(!projected.contains(&"html_octets"));
            assert!(!projected.contains(&"uid_mailbox"));
            assert!(projected.contains(&"mail_id"));
        }
    }

    #[test]
    fn aliases_the_domain_uid_as_uid_mailbox_on_the_domain_branch() {
        let sql = range(Range { fields: vec!["mail_id", "uid_mailbox"], ..Default::default() }).sql;
        assert!(sql.contains("uid_domain AS uid_mailbox"));
    }

    #[test]
    fn aliases_the_mapping_rows_uid_as_uid_mailbox_on_the_join_branch() {
        let sql =
            range(Range { mailbox: Some(ACCOUNT_BOX), fields: vec!["mail_id", "uid_mailbox"], ..Default::default() }).sql;
        assert!(sql.contains("x.uid AS uid_mailbox"));
        assert!(!sql.contains("uid_domain AS uid_mailbox"));
    }

    #[test]
    fn always_projects_mail_id_the_key_the_result_map_is_built_on() {
        let query = range(Range { fields: vec!["subject"], ..Default::default() });
        assert!(query.selected_fields.iter().any(|field| field == "mail_id"));
        assert!(query.sql.contains("mail_id"));
    }

    #[test]
    fn drops_unknown_fields_rather_than_interpolating_them_into_the_select() {
        let query = range(Range { fields: vec!["mail_id", "bogus_field"], ..Default::default() });
        assert!(!query.selected_fields.iter().any(|field| field == "bogus_field"));
        assert!(!query.sql.contains("bogus_field"));
    }

    #[test]
    fn a_star_expands_to_every_field_a_partial_row_may_carry() {
        let query = range(Range::default());
        assert_eq!(query.selected_fields, PartialMailModel::valid_fields());
        assert!(query.sql.contains("search_vector, uid_domain AS uid_mailbox, octet_length(text) AS text_octets"));
        // A star among other names is an unknown field, not a wildcard.
        let query = range(Range { fields: vec!["*", "subject"], ..Default::default() });
        assert_eq!(query.selected_fields, vec!["mail_id", "subject"]);
    }

    // imap-query.test.ts — "buildMailsByRangeQuery — CHANGEDSINCE"

    #[test]
    fn filters_in_sql_rather_than_post_filtering_the_whole_window() {
        assert!(range(Range { changed_since: Some(42.0), ..Default::default() }).sql.contains("AND modseq > $5"));
    }

    #[test]
    fn qualifies_the_predicate_with_the_mails_alias_on_the_join_branch() {
        let sql = range(Range { mailbox: Some(ACCOUNT_BOX), changed_since: Some(42.0), ..Default::default() }).sql;
        assert!(sql.contains("AND m.modseq > $6"));
    }

    #[test]
    fn omits_the_predicate_entirely_when_no_modifier_was_given() {
        for mailbox in [None, Some(ACCOUNT_BOX)] {
            for use_uid in [false, true] {
                assert!(!range(Range { mailbox, use_uid, ..Default::default() }).sql.contains("modseq >"));
            }
        }
    }

    #[test]
    fn binds_changed_since_into_the_slot_its_placeholder_names() {
        let domain = range(Range { changed_since: Some(42.0), ..Default::default() });
        assert_eq!(domain.values, params![USER, false, 3, 9, 42]);
        let mapped = range(Range { mailbox: Some(ACCOUNT_BOX), changed_since: Some(42.0), ..Default::default() });
        assert_eq!(mapped.values, params![USER, false, ACCOUNT_BOX, 3, 9, 42]);
    }

    // imap-query.test.ts — "buildMailsByRangeQuery — range and membership"

    #[test]
    fn range_applies_the_membership_rule_in_every_branch() {
        for mailbox in BOXES {
            for use_uid in [false, true] {
                let expression = membership_expression(mailbox, false, prefix_of(mailbox), None);
                if expression == "TRUE" {
                    continue;
                }
                assert!(range(Range { mailbox, use_uid, ..Default::default() }).sql.contains(&expression));
            }
        }
    }

    #[test]
    fn range_hides_expunged_rows_in_every_branch() {
        for mailbox in [None, Some(ACCOUNT_BOX)] {
            for use_uid in [false, true] {
                assert!(range(Range { mailbox, use_uid, ..Default::default() }).sql.contains("expunged = FALSE"));
            }
        }
    }

    #[test]
    fn ranges_over_the_uid_space_on_a_uid_fetch() {
        assert!(range(Range::default()).sql.contains("uid_domain >= $3 AND uid_domain <= $4"));
        assert!(range(Range { mailbox: Some(ACCOUNT_BOX), ..Default::default() })
            .sql
            .contains("x.uid >= $4 AND x.uid <= $5"));
    }

    #[test]
    fn converts_a_sequence_range_to_a_zero_based_offset_and_a_count() {
        assert_eq!(range(Range { use_uid: false, ..Default::default() }).values, params![USER, false, 2, 7]);
        assert_eq!(
            range(Range { mailbox: Some(ACCOUNT_BOX), use_uid: false, ..Default::default() }).values,
            params![USER, false, ACCOUNT_BOX, 2, 7]
        );
    }

    #[test]
    fn orders_by_the_uid_space_the_branch_enumerates() {
        assert!(range(Range::default()).sql.contains("ORDER BY uid_domain ASC"));
        assert!(range(Range { mailbox: Some(ACCOUNT_BOX), ..Default::default() }).sql.contains("ORDER BY x.uid ASC"));
    }

    #[test]
    fn range_joins_the_mapping_table_on_the_mailbox_path_the_caller_passed() {
        let sql = range(Range { mailbox: Some(USER_BOX), ..Default::default() }).sql;
        assert!(sql.contains("AND x.mailbox = $3"));
        assert!(!sql.contains("INBOX/accounts/"));
    }

    #[test]
    fn binds_range_ends_as_javascript_prints_them() {
        // `*` on an empty mailbox resolves to -1, and a UID past the ceiling
        // is clamped; a number PostgreSQL cannot read is bound as written.
        let query = build_mails_by_range_query(USER, None, false, -1.0, 1e21, true, &["mail_id"], None, None);
        assert_eq!(query.values, params![USER, false, "-1", "999999999"]);
        let query = build_mails_by_range_query(USER, None, false, 1e21, f64::NAN, true, &["mail_id"], Some(1e21), None);
        assert_eq!(query.values, params![USER, false, "1e+21", "NaN", "1e+21"]);
        let query = build_mails_by_range_query(USER, None, false, 5.0, f64::INFINITY, false, &["mail_id"], None, None);
        assert_eq!(query.values, params![USER, false, "4", "Infinity"]);
    }

    // imap-query.test.ts — "buildSearchMailsByUidQuery"

    fn search(mailbox: Option<&str>) -> PreparedQuery {
        build_search_mails_by_uid_query(USER, mailbox, false, &[SimplifiedCriterion::bare("SEEN")], None).unwrap()
    }

    #[test]
    fn returns_every_match_search_is_uncapped() {
        for mailbox in [None, Some(ACCOUNT_BOX)] {
            assert!(!search(mailbox).sql.to_uppercase().contains("LIMIT"));
        }
    }

    #[test]
    fn hides_expunged_rows_and_anything_the_mailbox_does_not_show() {
        for mailbox in [None, Some("Drafts"), Some("Junk"), Some(ACCOUNT_BOX)] {
            let sql = search(mailbox).sql;
            assert!(sql.contains("m.expunged = FALSE"));
            assert!(sql.contains(&membership_expression(mailbox, false, "m.", None)));
        }
    }

    #[test]
    fn enumerates_the_domain_uid_space_on_a_domain_view() {
        let query = search(None);
        assert!(query.sql.contains("SELECT uid_domain as uid FROM mails m"));
        assert!(query.sql.contains("ORDER BY uid_domain ASC"));
        assert_eq!(query.values[..2], params![USER, false]);
    }

    #[test]
    fn joins_the_mapping_table_and_enumerates_its_uid_on_a_mapped_box() {
        let query = search(Some(USER_BOX));
        assert!(query.sql.contains("FROM mails m, mail_mailbox_uid x"));
        assert!(query.sql.contains("x.mailbox = $3"));
        assert!(query.sql.contains("ORDER BY x.uid ASC"));
        assert_eq!(query.values[..3], params![USER, false, USER_BOX]);
    }

    #[test]
    fn binds_criterion_parameters_after_the_branchs_own() {
        let query = build_search_mails_by_uid_query(
            USER,
            Some(USER_BOX),
            false,
            &[SimplifiedCriterion::text(TextKey::Body, "needle")],
            None,
        )
        .unwrap();
        assert_eq!(query.values, params![USER, false, USER_BOX, "%needle%"]);
        assert!(query.sql.contains("text ILIKE $4"));
    }

    #[test]
    fn a_search_on_an_invalid_date_builds_no_statement() {
        for key in [DateKey::Before, DateKey::On, DateKey::Since, DateKey::SentBefore, DateKey::SentOn, DateKey::SentSince] {
            let criteria = [SimplifiedCriterion::Date { key, value: JsDate::invalid() }];
            assert_eq!(build_search_mails_by_uid_query(USER, None, false, &criteria, None), Err(InvalidDateParam));
        }
        // The last day JavaScript can represent has no day after.
        let last = SimplifiedCriterion::Date { key: DateKey::On, value: JsDate::from_ms(8_640_000_000_000_000) };
        assert_eq!(build_search_mails_by_uid_query(USER, None, false, &[last], None), Err(InvalidDateParam));
    }

    // imap-query.test.ts — "buildAllUidsQuery / buildFirstUnseenUidQuery"

    #[test]
    fn applies_the_membership_rule_on_both_branches_of_both_queries() {
        for mailbox in BOXES {
            for sent in [false, true] {
                let expression = membership_expression(mailbox, sent, prefix_of(mailbox), None);
                if expression == "TRUE" {
                    continue;
                }
                assert!(build_all_uids_query(USER, mailbox, sent, None).sql.contains(&expression));
                assert!(build_first_unseen_uid_query(USER, mailbox, sent, None).sql.contains(&expression));
            }
        }
    }

    #[test]
    fn hides_expunged_rows_from_the_sequence_map() {
        assert!(build_all_uids_query(USER, None, false, None).sql.contains("expunged = FALSE"));
        assert!(build_all_uids_query(USER, Some(ACCOUNT_BOX), false, None).sql.contains("m.expunged = FALSE"));
    }

    #[test]
    fn orders_the_sequence_map_by_the_uid_space_the_branch_enumerates() {
        assert!(build_all_uids_query(USER, None, false, None).sql.contains("ORDER BY uid_domain ASC"));
        assert!(build_all_uids_query(USER, Some(ACCOUNT_BOX), false, None).sql.contains("ORDER BY x.uid ASC"));
    }

    #[test]
    fn takes_the_lowest_uid_unread_row_not_the_unread_count() {
        for mailbox in [None, Some(ACCOUNT_BOX)] {
            let sql = build_first_unseen_uid_query(USER, mailbox, false, None).sql;
            assert!(sql.contains(&format!("{}read = FALSE", prefix_of(mailbox))));
            assert!(sql.contains("LIMIT 1"));
            assert!(!sql.contains("COUNT("));
        }
    }

    #[test]
    fn binds_the_mailbox_only_on_the_mapped_branch() {
        assert_eq!(build_all_uids_query(USER, None, true, None).values, params![USER, true]);
        assert_eq!(build_all_uids_query(USER, Some(USER_BOX), true, None).values, params![USER, true, USER_BOX]);
    }

    // imap-query.test.ts — "expunge filters and lookups"

    #[test]
    fn restricts_a_domain_expunge_to_deleted_rows_the_box_still_shows() {
        assert_eq!(
            build_expunge_deleted_filters(USER, None, true),
            vec![
                ("user_id", Filter::from(USER)),
                ("sent", Filter::from(true)),
                ("deleted", Filter::from(true)),
                ("expunged", Filter::from(false)),
            ]
        );
        assert_eq!(
            build_expunge_deleted_filters(USER, None, false),
            vec![
                ("user_id", Filter::from(USER)),
                ("sent", Filter::from(false)),
                ("deleted", Filter::from(true)),
                ("expunged", Filter::from(false)),
                ("is_spam", Filter::from(false)),
                ("draft", Filter::from(false)),
            ]
        );
    }

    #[test]
    fn selects_the_utility_views_by_their_own_flag_on_expunge() {
        assert!(build_expunge_deleted_filters(USER, Some("Junk"), false).contains(&("is_spam", Filter::from(true))));
        assert!(build_expunge_deleted_filters(USER, Some("Drafts"), false).contains(&("draft", Filter::from(true))));
    }

    #[test]
    fn carries_the_membership_rule_into_the_mapped_expunge_lookup() {
        for mailbox in [ACCOUNT_BOX, USER_BOX] {
            let query = build_expunge_deleted_select_query(USER, Some(mailbox), false, None);
            let expression = membership_expression(Some(mailbox), false, "m.", None);
            if expression != "TRUE" {
                assert!(query.sql.contains(&expression));
            }
            assert!(query.sql.contains("m.deleted = TRUE AND m.expunged = FALSE"));
            assert!(query.sql.contains("AND x.mailbox = $3"));
            assert!(query.sql.contains("x.uid as uid"));
            assert_eq!(query.values, params![USER, false, mailbox]);
        }
    }

    // imap-query.test.ts — "an INBOX, Drafts or Junk EXPUNGE"

    const RECEIVED_VIEWS: [Option<&str>; 3] = [None, Some("Drafts"), Some("Junk")];

    #[test]
    fn an_inbox_expunge_keeps_the_views_own_rule_and_numbers_by_uid_domain() {
        for mailbox in RECEIVED_VIEWS {
            let query = build_expunge_deleted_inbox_query(USER, mailbox, false, None);
            assert!(query.sql.contains(&membership_expression(mailbox, false, "m.", None)));
            assert!(query.sql.contains("m.deleted = TRUE AND m.expunged = FALSE"));
            assert!(query.sql.contains("m.uid_domain as uid"));
            // No JOIN: a mail with no mapping row at all must still be found.
            assert!(!query.sql.contains("JOIN"));
        }
    }

    #[test]
    fn an_inbox_expunge_takes_a_mail_holding_a_row_in_the_inbox_tree() {
        let query = build_expunge_deleted_inbox_query(USER, None, false, None);
        assert!(query.sql.contains("AND (x.mailbox = $3 OR x.mailbox LIKE $4)"));
        assert_eq!(query.values[2..4], params!["INBOX", "INBOX/accounts/%"]);
    }

    #[test]
    fn an_inbox_expunge_takes_mail_with_no_row_but_the_pivots_unless_it_is_a_copy() {
        // Old mail has no row, and the STORE that flags it files it into
        // Trash; a COPY / MOVE copy into Trash differs only by its Message-ID.
        let query = build_expunge_deleted_inbox_query(USER, None, false, None);
        assert!(query.sql.contains("m.message_id NOT LIKE $5"));
        assert!(query.sql.contains("AND NOT EXISTS ("));
        assert!(query.sql.contains("x.mailbox NOT IN ($6, $7)"));
        assert_eq!(query.values[4..], params!["%.copy@server", "Starred", "Trash"]);
    }

    #[test]
    fn an_inbox_expunge_binds_every_placeholder_it_uses() {
        for mailbox in RECEIVED_VIEWS {
            let query = build_expunge_deleted_inbox_query(USER, mailbox, false, None);
            assert!(query.sql.contains("$7") && !query.sql.contains("$8"));
            assert_eq!(query.values.len(), 7);
            assert_eq!(query.values[..2], params![USER, false]);
        }
    }

    #[test]
    fn restricts_a_domain_move_removal_to_the_named_uids_the_box_shows() {
        let filters = build_expunge_uids_filters(USER, None, false, &[4, 9]);
        assert_eq!(
            filters,
            vec![
                ("user_id", Filter::from(USER)),
                ("sent", Filter::from(false)),
                ("expunged", Filter::from(false)),
                ("uid_domain", Filter::any_of(&[4i64, 9])),
                ("is_spam", Filter::from(false)),
                ("draft", Filter::from(false)),
            ]
        );
        assert!(!filters.iter().any(|(column, _)| *column == "deleted"));
    }

    #[test]
    fn numbers_the_mapped_move_lookups_uid_placeholders_after_its_fixed_args() {
        let query = build_expunge_uids_select_query(USER, Some(USER_BOX), false, &[4, 9, 11], None);
        assert!(query.sql.contains("x.uid IN ($4,$5,$6)"));
        assert_eq!(query.values, params![USER, false, USER_BOX, 4, 9, 11]);
        assert!(query.sql.contains("m.expunged = FALSE"));
    }

    // imap-query.test.ts — "buildMailFlagUpdatesQuery"

    const IDS: [&str; 2] = ["id-1", "id-2"];

    #[test]
    fn uses_the_domain_uid_column_with_no_mapping_join_in_the_domain_branch() {
        let query = build_mail_flag_updates_query(USER, None, false, &IDS, None);
        assert!(query.sql.contains("uid_domain AS uid"));
        assert!(!query.sql.contains("JOIN"));
        assert!(query.sql.contains("mail_id = ANY($3)"));
        assert_eq!(query.values, vec![Param::from(USER), Param::from(false), Param::array(&IDS)]);
    }

    #[test]
    fn joins_the_per_mailbox_mapping_with_mailbox_at_3_and_ids_at_any_4() {
        let query = build_mail_flag_updates_query(USER, Some(ACCOUNT_BOX), false, &IDS, None);
        assert!(query.sql.contains("JOIN mail_mailbox_uid x"));
        assert!(query.sql.contains("x.mailbox = $3"));
        assert!(query.sql.contains("m.mail_id = ANY($4)"));
        assert_eq!(
            query.values,
            vec![Param::from(USER), Param::from(false), Param::from(ACCOUNT_BOX), Param::array(&IDS)]
        );
    }

    #[test]
    fn projects_the_five_flag_columns_and_hides_expunged_rows_on_both_branches() {
        for mailbox in [None, Some(ACCOUNT_BOX)] {
            let sql = build_mail_flag_updates_query(USER, mailbox, false, &IDS, None).sql;
            for column in ["read", "saved", "deleted", "draft", "answered"] {
                assert!(sql.contains(column));
            }
            assert!(sql.contains("expunged = FALSE"));
        }
    }

    // imap.test.ts — "buildCriterionClause — flag criteria use schema columns"

    fn clause(criterion: &SimplifiedCriterion, values: &mut Vec<Param>) -> Option<String> {
        build_criterion_clause(criterion, "uid_mailbox", values).unwrap()
    }

    fn clause_for(kind: &str) -> Option<String> {
        clause(&SimplifiedCriterion::bare(kind), &mut Vec::new())
    }

    fn some(sql: &str) -> Option<String> {
        Some(sql.to_string())
    }

    fn text(key: TextKey, value: &str) -> SimplifiedCriterion {
        SimplifiedCriterion::text(key, value)
    }

    fn bare(kind: &str) -> SimplifiedCriterion {
        SimplifiedCriterion::bare(kind)
    }

    fn uid_set(ranges: &[(f64, Option<f64>)]) -> SimplifiedCriterion {
        SimplifiedCriterion::UidSet(ranges.iter().map(|(start, end)| SequenceRange { start: *start, end: *end }).collect())
    }

    #[test]
    fn flag_criteria_map_to_their_schema_columns() {
        assert_eq!(clause_for("ANSWERED"), some("answered = TRUE"));
        assert_eq!(clause_for("UNANSWERED"), some("answered = FALSE"));
        assert_eq!(clause_for("DELETED"), some("deleted = TRUE"));
        assert_eq!(clause_for("UNDELETED"), some("deleted = FALSE"));
        assert_eq!(clause_for("DRAFT"), some("draft = TRUE"));
        assert_eq!(clause_for("UNDRAFT"), some("draft = FALSE"));
        assert_eq!(clause_for("UNSEEN"), some("read = FALSE"));
        assert_eq!(clause_for("FLAGGED"), some("saved = TRUE"));
        assert_eq!(clause_for("UNFLAGGED"), some("saved = FALSE"));
        assert_eq!(clause_for("NEW"), some("read = FALSE"));
        assert_eq!(clause_for("OLD"), None);
        assert_eq!(clause_for("RECENT"), None);
        // The type is matched in any case.
        assert_eq!(clause_for("seen"), some("read = TRUE"));
    }

    #[test]
    fn no_flag_criterion_returns_a_bare_false_sentinel() {
        for kind in ["ANSWERED", "UNANSWERED", "DELETED", "UNDELETED", "DRAFT", "UNDRAFT"] {
            assert_ne!(clause_for(kind), some("FALSE"));
        }
    }

    // imap.test.ts — "buildCriterionClause — NOT/OR SQL generation (regression for #551)"

    #[test]
    fn not_wraps_the_inner_clause_instead_of_dropping_it() {
        let mut values = Vec::new();
        assert_eq!(clause(&SimplifiedCriterion::not(bare("SEEN")), &mut values), some("NOT (read = TRUE)"));
        assert!(values.is_empty());
    }

    #[test]
    fn or_joins_both_sides_with_continuous_param_numbering() {
        let mut values = Vec::new();
        let criterion = SimplifiedCriterion::or(text(TextKey::From, "alice"), text(TextKey::From, "bob"));
        assert_eq!(clause(&criterion, &mut values), some("(from_text ILIKE $1 OR from_text ILIKE $2)"));
        assert_eq!(values, params!["%alice%", "%bob%"]);
    }

    #[test]
    fn not_from_negates_a_text_predicate_and_binds_its_param() {
        let mut values = Vec::new();
        let criterion = SimplifiedCriterion::not(text(TextKey::From, "spam@x"));
        assert_eq!(clause(&criterion, &mut values), some("NOT (from_text ILIKE $1)"));
        assert_eq!(values, params!["%spam@x%"]);
    }

    #[test]
    fn continues_param_numbering_from_an_already_populated_values_array() {
        let mut values = params!["user-1", false];
        let criterion = SimplifiedCriterion::or(text(TextKey::Subject, "a"), text(TextKey::To, "b"));
        assert_eq!(clause(&criterion, &mut values), some("(subject ILIKE $3 OR to_text ILIKE $4)"));
        assert_eq!(values, params!["user-1", false, "%a%", "%b%"]);
    }

    #[test]
    fn drops_an_or_whose_side_imposes_no_constraint_rather_than_over_narrowing() {
        let mut values = Vec::new();
        let criterion = SimplifiedCriterion::or(text(TextKey::From, "alice"), bare("ALL"));
        assert_eq!(clause(&criterion, &mut values), None);
    }

    #[test]
    fn normalised_not_before_flows_a_date_param_through_correctly() {
        let mut values = Vec::new();
        let when = JsDate::parse("2026-01-01T00:00:00Z");
        let criterion = SimplifiedCriterion::not(SimplifiedCriterion::Date { key: DateKey::Before, value: when });
        assert_eq!(clause(&criterion, &mut values), some("NOT (date < $1)"));
        assert_eq!(values, params!["2026-01-01T00:00:00.000Z"]);
    }

    #[test]
    fn plain_criteria_are_unaffected_by_the_refactor() {
        let mut values = Vec::new();
        assert_eq!(clause(&bare("SEEN"), &mut values), some("read = TRUE"));
        assert_eq!(clause(&bare("ALL"), &mut values), None);
    }

    #[test]
    fn date_keys_compare_the_one_date_column() {
        let day = JsDate::parse("2026-01-31T00:00:00Z");
        let date = |key| SimplifiedCriterion::Date { key, value: day };
        for (key, sql) in [
            (DateKey::Before, "date < $1"),
            (DateKey::SentBefore, "date < $1"),
            (DateKey::Since, "date >= $1"),
            (DateKey::SentSince, "date >= $1"),
        ] {
            let mut values = Vec::new();
            assert_eq!(clause(&date(key), &mut values), some(sql));
            assert_eq!(values, params!["2026-01-31T00:00:00.000Z"]);
        }
        for key in [DateKey::On, DateKey::SentOn] {
            let mut values = params!["user-1"];
            assert_eq!(clause(&date(key), &mut values), some("date >= $2 AND date < $3"));
            assert_eq!(values, params!["user-1", "2026-01-31T00:00:00.000Z", "2026-02-01T00:00:00.000Z"]);
        }
    }

    // imap.test.ts — "buildCriterionClause — UID_SET ORs its ranges (#659)"

    #[test]
    fn renders_a_single_exact_element_without_an_or_wrapper() {
        let mut values = Vec::new();
        assert_eq!(clause(&uid_set(&[(5.0, None)]), &mut values), some("uid_mailbox = $1"));
        assert_eq!(values, params![5]);
    }

    #[test]
    fn ors_disjoint_exact_elements_instead_of_anding_to_empty() {
        let mut values = Vec::new();
        assert_eq!(
            clause(&uid_set(&[(1.0, None), (3.0, None)]), &mut values),
            some("(uid_mailbox = $1 OR uid_mailbox = $2)")
        );
        assert_eq!(values, params![1, 3]);
    }

    #[test]
    fn ors_mixed_exact_and_range_elements_with_parenthesised_ranges() {
        let mut values = Vec::new();
        assert_eq!(
            clause(&uid_set(&[(2.0, Some(3.0)), (5.0, Some(7.0))]), &mut values),
            some("((uid_mailbox >= $1 AND uid_mailbox <= $2) OR (uid_mailbox >= $3 AND uid_mailbox <= $4))")
        );
        assert_eq!(values, params![2, 3, 5, 7]);
    }

    #[test]
    fn renders_a_lone_range_without_an_or_wrapper() {
        let mut values = Vec::new();
        assert_eq!(
            clause(&uid_set(&[(1.0, Some(3.0))]), &mut values),
            some("(uid_mailbox >= $1 AND uid_mailbox <= $2)")
        );
        assert_eq!(values, params![1, 3]);
    }

    #[test]
    fn ands_correctly_against_a_sibling_flag_key() {
        let mut values = Vec::new();
        let flag = clause(&bare("SEEN"), &mut values).unwrap();
        let set = clause(&uid_set(&[(1.0, None), (3.0, None)]), &mut values).unwrap();
        assert_eq!([flag, set].join(" AND "), "read = TRUE AND (uid_mailbox = $1 OR uid_mailbox = $2)");
        assert_eq!(values, params![1, 3]);
    }

    #[test]
    fn imposes_no_constraint_for_an_empty_set() {
        let mut values = Vec::new();
        assert_eq!(clause(&uid_set(&[]), &mut values), None);
        assert!(values.is_empty());
    }

    // imap.test.ts — "buildCriterionClause — BODY/TEXT search the message body (#552)"

    #[test]
    fn body_matches_the_body_column_only() {
        let mut values = Vec::new();
        let fragment = clause(&text(TextKey::Body, "needle"), &mut values).unwrap();
        assert_eq!(fragment, "text ILIKE $1");
        assert!(!fragment.contains("subject ILIKE"));
        assert!(!fragment.contains("from_text ILIKE"));
        assert_eq!(values, params!["%needle%"]);
    }

    #[test]
    fn text_matches_header_columns_plus_the_body_column() {
        let mut values = Vec::new();
        let fragment = clause(&text(TextKey::Text, "needle"), &mut values).unwrap();
        for column in ["subject ILIKE", "from_text ILIKE", "to_text ILIKE", "text ILIKE"] {
            assert!(fragment.contains(column));
        }
        assert_eq!(clause(&text(TextKey::SubjectText, "needle"), &mut Vec::new()).unwrap(), fragment);
        assert_eq!(values, params!["%needle%"]);
    }

    #[test]
    fn the_other_text_keys_match_their_own_column() {
        for (key, column) in [
            (TextKey::Subject, "subject"),
            (TextKey::From, "from_text"),
            (TextKey::To, "to_text"),
            (TextKey::Cc, "cc_text"),
            (TextKey::Bcc, "bcc_text"),
        ] {
            let mut values = Vec::new();
            assert_eq!(clause(&text(key, "x"), &mut values), Some(format!("{column} ILIKE $1")));
            assert_eq!(values, params!["%x%"]);
        }
    }

    // imap.test.ts — "buildCriterionClause — unexpressible criteria fail closed (#672)"

    fn header(field: &str, text: &str) -> SimplifiedCriterion {
        SimplifiedCriterion::Header { field: field.to_string(), text: text.to_string() }
    }

    #[test]
    fn match_none_is_a_sql_fragment_the_search_keeps_in_the_and() {
        assert_eq!(MATCH_NONE, "FALSE");
        let query = build_search_mails_by_uid_query(USER, None, false, &[bare("SEEN"), bare("KEYWORD")], None).unwrap();
        assert!(query.sql.contains(" AND read = TRUE AND FALSE\n"));
    }

    #[test]
    fn keyword_can_never_match_so_it_matches_none() {
        let mut values = Vec::new();
        assert_eq!(clause(&bare("KEYWORD"), &mut values), some("FALSE"));
        assert!(values.is_empty());
    }

    #[test]
    fn unkeyword_always_matches_so_it_matches_all() {
        assert_eq!(clause_for("UNKEYWORD"), None);
    }

    #[test]
    fn larger_and_smaller_fail_closed() {
        for key in [SizeKey::Larger, SizeKey::Smaller] {
            let mut values = Vec::new();
            assert_eq!(clause(&SimplifiedCriterion::Size { key, value: 999_999_999.0 }, &mut values), some("FALSE"));
            assert!(values.is_empty());
        }
    }

    #[test]
    fn header_on_an_unsupported_field_fails_closed() {
        let mut values = Vec::new();
        assert_eq!(clause(&header("X-Mailer", "z"), &mut values), some("FALSE"));
        assert!(values.is_empty());
    }

    #[test]
    fn header_on_a_supported_field_still_filters() {
        let mut values = Vec::new();
        assert_eq!(clause(&header("Subject", "hi"), &mut values), some("subject ILIKE $1"));
        assert_eq!(values, params!["%hi%"]);
        for (field, column) in [("FROM", "from_text"), ("to", "to_text"), ("Message-ID", "message_id")] {
            assert_eq!(clause(&header(field, "v"), &mut Vec::new()), Some(format!("{column} ILIKE $1")));
        }
    }

    #[test]
    fn an_unknown_criterion_type_fails_closed() {
        assert_eq!(clause_for("SOMETHING-UNSUPPORTED"), some("FALSE"));
        assert_eq!(clause_for("SEQ"), some("FALSE"));
    }

    #[test]
    fn seen_and_keyword_the_keyword_fragment_is_false_so_the_and_matches_nothing() {
        assert_eq!(clause_for("SEEN"), some("read = TRUE"));
        assert_eq!(clause_for("KEYWORD"), some("FALSE"));
    }

    #[test]
    fn or_seen_keyword_reduces_to_the_constrained_side() {
        assert_eq!(
            clause(&SimplifiedCriterion::or(bare("SEEN"), bare("KEYWORD")), &mut Vec::new()),
            some("read = TRUE")
        );
        assert_eq!(
            clause(&SimplifiedCriterion::or(bare("KEYWORD"), bare("SEEN")), &mut Vec::new()),
            some("read = TRUE")
        );
    }

    #[test]
    fn or_keyword_keyword_stays_match_none() {
        assert_eq!(clause(&SimplifiedCriterion::or(bare("KEYWORD"), bare("KEYWORD")), &mut Vec::new()), some("FALSE"));
    }

    #[test]
    fn or_seen_all_still_matches_everything() {
        assert_eq!(clause(&SimplifiedCriterion::or(bare("SEEN"), bare("ALL")), &mut Vec::new()), None);
    }

    #[test]
    fn not_keyword_is_match_all() {
        assert_eq!(clause(&SimplifiedCriterion::not(bare("KEYWORD")), &mut Vec::new()), None);
    }

    #[test]
    fn not_all_is_match_none() {
        assert_eq!(clause(&SimplifiedCriterion::not(bare("ALL")), &mut Vec::new()), some("FALSE"));
    }

    // imap.test.ts — "buildCriterionClause — combinators don't orphan bound params (#672)"

    fn seeded() -> Vec<Param> {
        params!["user-1", false]
    }

    #[test]
    fn or_subject_all_is_match_all_and_rolls_back_subjects_param() {
        let mut values = seeded();
        assert_eq!(clause(&SimplifiedCriterion::or(text(TextKey::Subject, "x"), bare("ALL")), &mut values), None);
        assert_eq!(values, seeded());
    }

    #[test]
    fn or_subject_unkeyword_is_match_all_with_the_param_rolled_back() {
        let mut values = seeded();
        assert_eq!(clause(&SimplifiedCriterion::or(text(TextKey::Subject, "x"), bare("UNKEYWORD")), &mut values), None);
        assert_eq!(values, seeded());
    }

    #[test]
    fn or_all_subject_with_null_on_the_left_also_rolls_back() {
        let mut values = seeded();
        assert_eq!(clause(&SimplifiedCriterion::or(bare("ALL"), text(TextKey::Subject, "x")), &mut values), None);
        assert_eq!(values, seeded());
    }

    #[test]
    fn not_of_or_subject_all_is_match_none_with_the_nested_param_rolled_back() {
        let mut values = seeded();
        let criterion = SimplifiedCriterion::not(SimplifiedCriterion::or(text(TextKey::Subject, "x"), bare("ALL")));
        assert_eq!(clause(&criterion, &mut values), some("FALSE"));
        assert_eq!(values, seeded());
    }

    #[test]
    fn a_real_or_with_two_param_pushing_sides_keeps_both_params_contiguously_numbered() {
        let mut values = seeded();
        let criterion = SimplifiedCriterion::or(text(TextKey::Subject, "x"), text(TextKey::From, "y"));
        assert_eq!(clause(&criterion, &mut values), some("(subject ILIKE $3 OR from_text ILIKE $4)"));
        assert_eq!(values, params!["user-1", false, "%x%", "%y%"]);
    }

    #[test]
    fn x_or_none_keeps_xs_param_aligned() {
        let mut values = seeded();
        let criterion = SimplifiedCriterion::or(text(TextKey::Subject, "x"), bare("KEYWORD"));
        assert_eq!(clause(&criterion, &mut values), some("subject ILIKE $3"));
        assert_eq!(values, params!["user-1", false, "%x%"]);
    }

    // Depth is the client's choice: a deep criterion is reduced and dropped
    // without recursion.

    #[test]
    fn a_deep_criterion_is_built_and_dropped_without_recursion() {
        const LEVELS: usize = 200_000;
        let mut criterion = text(TextKey::Subject, "x");
        for _ in 0..LEVELS {
            criterion = SimplifiedCriterion::not(criterion);
        }
        let mut values = Vec::new();
        let fragment = build_criterion_clause(&criterion, "uid_domain", &mut values).unwrap().unwrap();
        assert!(fragment.starts_with("NOT (NOT (NOT ("));
        assert_eq!(fragment.trim_end_matches(')').trim_start_matches("NOT ("), "subject ILIKE $1");
        assert_eq!(fragment.len(), LEVELS * "NOT ()".len() + "subject ILIKE $1".len());
        assert_eq!(values, params!["%x%"]);
        drop(criterion);

        // An OR chain that collapses level by level, alternating sides.
        let mut criterion = bare("KEYWORD");
        for level in 0..LEVELS {
            criterion = if level % 2 == 0 {
                SimplifiedCriterion::or(criterion, text(TextKey::From, "a"))
            } else {
                SimplifiedCriterion::not(SimplifiedCriterion::or(bare("ALL"), criterion))
            };
        }
        let mut values = seeded();
        assert_eq!(build_criterion_clause(&criterion, "uid_domain", &mut values).unwrap(), some("FALSE"));
        assert_eq!(values, seeded());
    }
}
