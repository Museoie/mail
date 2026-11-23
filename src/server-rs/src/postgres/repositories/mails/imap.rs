//! Mail queries for the IMAP server (repositories/mails/imap.ts): counts,
//! range reads, SEARCH, STORE, EXPUNGE and the chunked body readers.
//!
//! A mailbox argument is the raw IMAP box path for a mapped box
//! (`INBOX/accounts/sales`, `Sent Messages/accounts/sales`, a user-created
//! `Archive`), the view's name for a utility view (`Drafts`), and `None` for
//! the two views with no name of their own, `INBOX` and the unified
//! `Sent Messages`. For a mapped box that is the exact string the write side
//! stores in `mail_mailbox_uid.mailbox`.
//!
//! Most of these absorb a database fault: they log it and answer as if the
//! mailbox were empty, as the Bun server does. `expunge_mails_by_uid` and the
//! chunk readers report theirs.
//!
//! ```ignore
//! let mails = get_mails_by_range(user_id, None, false, 1.0, 50.0, true, &["mail_id", "read"], None, None).await;
//! let mut chunks = std::pin::pin!(pg_text_chunks(mail_id, user_id, TextColumn::Text, PG_TEXT_CHUNK_CHARS));
//! while let Some(chunk) = chunks.try_next().await? { /* … */ }
//! ```

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};
use futures_util::stream::{self, Stream};
use serde_json::{json, Value};

use super::counters::get_next_modseq;
use super::imap_query::{
    build_all_uids_query, build_count_messages_query, build_expunge_deleted_filters,
    build_expunge_deleted_inbox_query, build_expunge_deleted_select_query, build_expunge_uids_filters,
    build_expunge_uids_select_query, build_scoped_expunge_deleted_select_query,
    build_scoped_expunge_uids_select_query,
    build_first_unseen_uid_query, build_mail_flag_updates_query, build_mails_by_range_query,
    build_search_mails_by_uid_query, SimplifiedCriterion,
};
use super::inflight::single_flight;
use super::model::{
    mails_table, PartialMailModel, ANSWERED, DELETED, DRAFT, EXPUNGED, MAIL_ID, MODSEQ, READ, SAVED, UID_DOMAIN,
    UPDATED, USER_ID,
};
use super::set_flags_query::build_set_mail_flags_queries;
use super::table::{run, run_query, Filter, Record, SetValue};
use super::views::uses_domain_uid_space;
use crate::imap::session_format::BodySource;
use crate::imap::types::{json_number, json_string, StoreOperation};
use crate::postgres::{DbError, DbResult, Param};
use crate::{logger, params};

// Part of this module's surface in the Bun server; the definitions live
// beside the queries that consume them.
pub use super::imap_query::{build_criterion_clause, MATCH_NONE};

// ---------------------------------------------------------------------------
// Chunked body reads
// ---------------------------------------------------------------------------

/// How many characters one round trip of [`pg_text_chunks`] reads. A
/// character is at most 4 bytes of UTF-8, so a chunk is at most 48 000 bytes
/// and fits the 48 KiB slices the base64 emitter works in. `SUBSTRING` on
/// text counts characters, so this is in characters, not bytes.
pub const PG_TEXT_CHUNK_CHARS: usize = 12_000;

/// How many bytes one round trip of [`pg_byte_chunks`] reads: 48 KiB, the
/// base64 emitter's slice size. Divisible by 3, so every chunk but the last
/// base64-encodes with nothing carried over the chunk boundary.
pub const PG_TEXT_CHUNK_BYTES: usize = 48 * 1024;

const _: () = assert!(
    PG_TEXT_CHUNK_BYTES % 3 == 0,
    "PG_TEXT_CHUNK_BYTES must be divisible by 3 so per-chunk base64 encoding needs no cross-chunk carry"
);

/// The body column a chunk reader reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextColumn {
    Text,
    Html,
}

impl TextColumn {
    /// The column's name, the only text these readers splice into SQL.
    pub fn as_str(self) -> &'static str {
        match self {
            TextColumn::Text => "text",
            TextColumn::Html => "html",
        }
    }
}

impl From<BodySource> for TextColumn {
    fn from(source: BodySource) -> TextColumn {
        match source {
            BodySource::Text => TextColumn::Text,
            BodySource::Html => TextColumn::Html,
        }
    }
}

/// Streams one mail's `text` or `html` column in chunks of at most
/// `chunk_chars` characters ([`PG_TEXT_CHUNK_CHARS`] in production), one
/// `SUBSTRING(<column> FROM $offset FOR $take)` round trip per chunk, so the
/// whole column is never in memory. It pairs with the `text_octets` /
/// `html_octets` projections of [`get_mails_by_range`]: the caller sizes the
/// literal from the octet count, then streams the body from here.
///
/// The stream ends after a chunk shorter than `chunk_chars`, or an empty one,
/// which is what PostgreSQL answers for an offset past the end of the column.
/// A failed read is yielded and ends it.
pub fn pg_text_chunks(
    mail_id: &str,
    user_id: &str,
    source_column: TextColumn,
    chunk_chars: usize,
) -> impl Stream<Item = DbResult<String>> + Send + 'static {
    // The `::int` casts are load-bearing. Every parameter is bound as untyped
    // text, and without them PostgreSQL resolves `substring(text, unknown,
    // unknown)` to the SIMILAR TO overload, reads the chunk size as an escape
    // character and fails with "invalid escape string".
    let sql = format!(
        "SELECT SUBSTRING({} FROM $3::int FOR $4::int) AS chunk\n                 FROM mails WHERE mail_id = $1 AND user_id = $2",
        source_column.as_str()
    );
    let (mail_id, user_id) = (mail_id.to_string(), user_id.to_string());
    page_by_code_points(
        move |offset, take| {
            let (sql, values) = (sql.clone(), params![mail_id.as_str(), user_id.as_str(), offset, take]);
            async move {
                let result = run(&sql, &values).await?;
                let chunk = result.rows.first().and_then(|row| row.get("chunk")).and_then(Value::as_str);
                Ok(chunk.unwrap_or_default().to_string())
            }
        },
        chunk_chars,
    )
}

/// Pages through a text of unknown length: asks `read_chunk(offset, take)`
/// for `chunk_chars` characters at a time, from the 1-based character
/// `offset`, until a chunk comes back short or empty.
///
/// The offset advances by the characters (code points) a chunk holds, not by
/// its length in UTF-16 units or in bytes, so no character at a chunk
/// boundary is skipped or read twice.
pub fn page_by_code_points<F, Fut, E>(read_chunk: F, chunk_chars: usize) -> impl Stream<Item = Result<String, E>>
where
    F: FnMut(usize, usize) -> Fut,
    Fut: Future<Output = Result<String, E>>,
{
    stream::try_unfold((read_chunk, Some(1usize)), move |(mut read_chunk, offset)| async move {
        let Some(offset) = offset else {
            return Ok(None);
        };
        let chunk = read_chunk(offset, chunk_chars).await?;
        if chunk.is_empty() {
            return Ok(None);
        }
        let code_points = chunk.chars().count();
        let next = (code_points >= chunk_chars).then_some(offset + code_points);
        Ok(Some((chunk, (read_chunk, next))))
    })
}

/// A BYTEA value as the pool returns it, in hexadecimal; nothing for NULL.
fn bytea(value: Option<&Value>) -> DbResult<Vec<u8>> {
    match value.and_then(Value::as_str) {
        Some(encoded) => hex::decode(encoded).map_err(|error| DbError::new(format!("unreadable bytea chunk: {error}"))),
        None => Ok(Vec::new()),
    }
}

/// Streams one mail's `text` or `html` column as its UTF-8 bytes, in chunks
/// of at most `chunk_bytes` ([`PG_TEXT_CHUNK_BYTES`] in production) starting
/// at the 1-based byte `start_byte` (1 for the whole column).
///
/// `SUBSTRING` on the `convert_to(<column>, 'UTF8')` bytes is byte-indexed,
/// which is what a consumer that base64-encodes wants: a partial fetch seeks
/// straight to a 3-byte boundary instead of draining from the start. A chunk
/// may end inside a multi-byte character; the bytes concatenate correctly.
/// Use [`pg_text_chunks`] when the consumer needs text.
pub fn pg_byte_chunks(
    mail_id: &str,
    user_id: &str,
    source_column: TextColumn,
    start_byte: usize,
    chunk_bytes: usize,
) -> impl Stream<Item = DbResult<Vec<u8>>> + Send + 'static {
    // `convert_to` rather than a `::bytea` cast, which would run the text
    // through the bytea escape parser and fail on a body holding a backslash.
    let sql = format!(
        "SELECT SUBSTRING(convert_to({}, 'UTF8') FROM $3::int FOR $4::int) AS chunk\n                 FROM mails WHERE mail_id = $1 AND user_id = $2",
        source_column.as_str()
    );
    let (mail_id, user_id) = (mail_id.to_string(), user_id.to_string());
    stream::try_unfold(Some(start_byte), move |offset| {
        let (sql, mail_id, user_id) = (sql.clone(), mail_id.clone(), user_id.clone());
        async move {
            let Some(offset) = offset else {
                return Ok(None);
            };
            let result = run(&sql, &params![mail_id, user_id, offset, chunk_bytes]).await?;
            let chunk = bytea(result.rows.first().and_then(|row| row.get("chunk")))?;
            if chunk.is_empty() {
                return Ok(None);
            }
            let next = (chunk.len() >= chunk_bytes).then_some(offset + chunk.len());
            Ok(Some((chunk, next)))
        }
    })
}

// ---------------------------------------------------------------------------
// Reading rows
// ---------------------------------------------------------------------------

fn integer(row: &Record, column: &str) -> Option<i64> {
    let value = row.get(column)?;
    value.as_i64().or_else(|| value.as_f64().map(|number| number as i64))
}

/// Whether JavaScript reads the value as true in a condition.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

fn flag(row: &Record, column: &str) -> bool {
    row.get(column).and_then(Value::as_bool).unwrap_or(false)
}

fn text(row: &Record, column: &str) -> String {
    row.get(column).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// `parseInt(value || "0", 10)` of a count column.
fn count(row: Option<&Record>, column: &str) -> i64 {
    match row.and_then(|row| row.get(column)) {
        Some(Value::Number(number)) => number.as_i64().unwrap_or_else(|| number.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(digits)) => {
            let digits = digits.trim_start();
            let end = digits
                .char_indices()
                .find(|(at, c)| !(c.is_ascii_digit() || (*at == 0 && matches!(c, '-' | '+'))))
                .map_or(digits.len(), |(at, _)| at);
            digits[..end].parse().unwrap_or(0)
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Counts
// ---------------------------------------------------------------------------

/// How many messages a mailbox shows, and how many of them are unread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageCounts {
    pub total: i64,
    pub unread: i64,
}

/// Counts the messages of a mailbox. A database fault reads as an empty
/// mailbox.
pub async fn count_messages(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> MessageCounts {
    match run_query(&build_count_messages_query(user_id, mailbox, sent, domains)).await {
        Ok(result) => {
            let row = result.rows.first();
            MessageCounts { total: count(row, "total"), unread: count(row, "unread") }
        }
        Err(error) => {
            logger::error_with("Failed to count messages", json!({}), &error);
            MessageCounts::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Range reads
// ---------------------------------------------------------------------------

/// The rows of a range read, keyed by `mail_id`, in the order the mailbox
/// lists them: ascending UID. Shared between the callers one read served.
pub type MailsByRange = Arc<Vec<(String, PartialMailModel)>>;

/// `JSON.stringify` of the arguments, with the field list sorted so that the
/// order a caller names its fields in does not split one read into two.
#[allow(clippy::too_many_arguments)]
fn inflight_key(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    start: f64,
    end: f64,
    use_uid: bool,
    fields: &[String],
    changed_since: Option<f64>,
    domains: Option<&[String]>,
) -> String {
    let mut sorted: Vec<&String> = fields.iter().collect();
    // JavaScript orders strings by UTF-16 code unit.
    sorted.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    format!(
        "[{},{},{sent},{},{},{use_uid},[{}],{},{}]",
        json_string(user_id),
        mailbox.map_or_else(|| "null".to_string(), json_string),
        json_number(start),
        json_number(end),
        sorted.iter().map(|field| json_string(field)).collect::<Vec<_>>().join(","),
        changed_since.map_or_else(|| "null".to_string(), json_number),
        domains.map_or_else(
            || "null".to_string(),
            |domains| format!("[{}]", domains.iter().map(|domain| json_string(domain)).collect::<Vec<_>>().join(","))
        ),
    )
}

/// The IMAP range read over `mails`: the messages `start..=end` of a mailbox,
/// by UID when `use_uid` and by 1-based sequence number otherwise, each with
/// the `fields` asked for (see [`build_mails_by_range_query`]). A database
/// fault, or a row that fails validation, reads as no messages.
///
/// Concurrent identical calls (same user, mailbox, range and field set) share
/// one read and receive the same rows: a client that pipelines duplicate
/// `UID FETCH <uid> BODY[]` commands would otherwise load the same multi-MB
/// body once per command. A strict subset of the fields is a different
/// projection and does not coalesce.
///
/// The read starts when this is called, not when the future is first polled.
#[allow(clippy::too_many_arguments)]
pub fn get_mails_by_range<S: AsRef<str>>(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    start: f64,
    end: f64,
    use_uid: bool,
    fields: &[S],
    changed_since: Option<f64>,
    domains: Option<&[String]>,
) -> BoxFuture<'static, MailsByRange> {
    let fields: Vec<String> = fields.iter().map(|field| field.as_ref().to_string()).collect();
    let key = inflight_key(user_id, mailbox, sent, start, end, use_uid, &fields, changed_since, domains);
    let (user_id, mailbox) = (user_id.to_string(), mailbox.map(str::to_string));
    let domains: Option<Vec<String>> = domains.map(<[String]>::to_vec);
    let work = move || async move {
        Arc::new(
            get_mails_by_range_uncoalesced(
                &user_id,
                mailbox.as_deref(),
                sent,
                start,
                end,
                use_uid,
                &fields,
                changed_since,
                domains.as_deref(),
            )
            .await,
        )
    };
    #[cfg(test)]
    if testing::reads_inline() {
        return work().boxed();
    }
    single_flight(&key, work).boxed()
}

#[allow(clippy::too_many_arguments)]
async fn get_mails_by_range_uncoalesced(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    start: f64,
    end: f64,
    use_uid: bool,
    fields: &[String],
    changed_since: Option<f64>,
    domains: Option<&[String]>,
) -> Vec<(String, PartialMailModel)> {
    let read = async {
        let query =
            build_mails_by_range_query(user_id, mailbox, sent, start, end, use_uid, fields, changed_since, domains);
        let result = run(&query.sql, &query.values).await?;
        let mut mails: Vec<(String, PartialMailModel)> = Vec::with_capacity(result.rows.len());
        // A repeated id keeps its first place and takes the later row.
        let mut places: HashMap<String, usize> = HashMap::with_capacity(result.rows.len());
        for row in &result.rows {
            let model = PartialMailModel::new(&query.selected_fields, row)?;
            let mail_id = text(row, MAIL_ID);
            match places.get(&mail_id) {
                Some(place) => mails[*place].1 = model,
                None => {
                    places.insert(mail_id.clone(), mails.len());
                    mails.push((mail_id, model));
                }
            }
        }
        anyhow::Ok(mails)
    };
    read.await.unwrap_or_else(|error| {
        logger::error_with("Failed to get mails by range", json!({}), &error);
        Vec::new()
    })
}

// ---------------------------------------------------------------------------
// STORE
// ---------------------------------------------------------------------------

/// One message a STORE addressed, as it stands afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatedMailFlags {
    /// The row's id. The caller needs it to keep the `Starred` and `Trash`
    /// pivots in step with `saved` and `deleted` (see `sync_mailbox_pivot`).
    pub mail_id: String,
    pub uid: i64,
    pub read: bool,
    pub saved: bool,
    pub deleted: bool,
    pub draft: bool,
    pub answered: bool,
    /// The mod-sequence this STORE stamped, shared by every row it changed:
    /// the MODSEQ item of the untagged FETCH a CONDSTORE client gets.
    pub modseq: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetMailFlagsResult {
    pub updated: Vec<UpdatedMailFlags>,
    /// UIDs the range matched whose mod-sequence exceeded the caller's
    /// UNCHANGEDSINCE. Their flags are untouched and they belong in the
    /// tagged MODIFIED response code (RFC 7162 §3.1.3). Always empty for an
    /// unconditional STORE.
    pub failed: Vec<i64>,
}

/// What a STORE does with the flags it names (RFC 3501 §6.4.6); a `.SILENT`
/// item is the same operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOperationType {
    /// `FLAGS`: replace the message's flags with the ones named.
    Flags,
    /// `+FLAGS`: add the ones named, leave the others.
    Add,
    /// `-FLAGS`: remove the ones named, leave the others.
    Remove,
}

impl StoreOperationType {
    pub fn as_str(self) -> &'static str {
        match self {
            StoreOperationType::Flags => "FLAGS",
            StoreOperationType::Add => "+FLAGS",
            StoreOperationType::Remove => "-FLAGS",
        }
    }

    pub fn parse(value: &str) -> Option<StoreOperationType> {
        [StoreOperationType::Flags, StoreOperationType::Add, StoreOperationType::Remove]
            .into_iter()
            .find(|operation| operation.as_str() == value)
    }
}

impl From<StoreOperation> for StoreOperationType {
    fn from(operation: StoreOperation) -> StoreOperationType {
        match operation {
            StoreOperation::Flags | StoreOperation::FlagsSilent => StoreOperationType::Flags,
            StoreOperation::AddFlags | StoreOperation::AddFlagsSilent => StoreOperationType::Add,
            StoreOperation::RemoveFlags | StoreOperation::RemoveFlagsSilent => StoreOperationType::Remove,
        }
    }
}

/// The five flags the server stores, with the column each lives in.
const FLAG_COLUMNS: [(&str, &str); 5] = [
    ("\\Seen", READ),
    ("\\Flagged", SAVED),
    ("\\Deleted", DELETED),
    ("\\Draft", DRAFT),
    ("\\Answered", ANSWERED),
];

/// The `SET` assignments of a flag update. Empty when the operation touches
/// no flag the server stores (an empty `+FLAGS ()`, or only keywords), which
/// is a legal no-op that `set_mail_flags` serves without an UPDATE. `FLAGS`
/// always assigns all five, so it is never empty. Flag names are matched as
/// written: `\seen` is not `\Seen`.
pub fn build_flag_set_clause<S: AsRef<str>>(operation: StoreOperationType, flags: &[S]) -> String {
    let has_flag = |flag: &str| flags.iter().any(|named| named.as_ref() == flag);
    let assign_named = |value: &str| {
        FLAG_COLUMNS
            .iter()
            .filter(|(flag, _)| has_flag(flag))
            .map(|(_, column)| format!("{column} = {value}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match operation {
        StoreOperationType::Add => assign_named("TRUE"),
        StoreOperationType::Remove => assign_named("FALSE"),
        StoreOperationType::Flags => format!(
            "\n        read = {},\n        saved = {},\n        deleted = {},\n        draft = {},\n        answered = {}\n      ",
            has_flag("\\Seen"),
            has_flag("\\Flagged"),
            has_flag("\\Deleted"),
            has_flag("\\Draft"),
            has_flag("\\Answered"),
        ),
    }
}

fn to_updated_mail_flags(row: &Record) -> UpdatedMailFlags {
    UpdatedMailFlags {
        mail_id: text(row, MAIL_ID),
        uid: integer(row, "uid").unwrap_or(0),
        read: flag(row, READ),
        saved: flag(row, SAVED),
        deleted: flag(row, DELETED),
        draft: flag(row, DRAFT),
        answered: flag(row, ANSWERED),
        modseq: integer(row, MODSEQ).unwrap_or(0),
    }
}

/// The two range parameters of a STORE, as the Bun server binds them.
/// `build_set_mail_flags_queries` takes integers; a wire number outside their
/// range has to reach PostgreSQL the way JavaScript prints it, so the STORE
/// fails there exactly as it does for the Bun server.
fn store_range_params(start: f64, end: f64, use_uid: bool) -> [Param; 2] {
    if use_uid {
        [Param::from(start), Param::from(end)]
    } else {
        [Param::from(start - 1.0), Param::from(end - start + 1.0)]
    }
}

/// Applies a STORE to the messages `start..=end` of a mailbox (UIDs when
/// `use_uid`, sequence numbers otherwise) and returns each message it
/// addressed as it stands afterwards.
///
/// With `unchanged_since` (RFC 7162 §3.1.3) only rows whose mod-sequence is
/// at most that value are changed; the others keep their flags and come back
/// in `failed`. Every row starts at mod-sequence 1, so `UNCHANGEDSINCE 0`
/// fails every message, as the RFC requires.
///
/// One mod-sequence is reserved per STORE and stamped on every row it
/// changes. A STORE that names no stored flag changes nothing and reserves
/// nothing: it answers with the rows' current flags. A database fault reads
/// as a STORE that addressed no message.
#[allow(clippy::too_many_arguments)]
pub fn set_mail_flags<S: AsRef<str>>(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    start: f64,
    end: f64,
    flags: &[S],
    use_uid: bool,
    operation: StoreOperationType,
    unchanged_since: Option<f64>,
    domains: Option<&[String]>,
) -> BoxFuture<'static, SetMailFlagsResult> {
    let set_clause = build_flag_set_clause(operation, flags);
    let conditional = unchanged_since.is_some();
    let mut queries = build_set_mail_flags_queries(
        user_id,
        mailbox,
        sent,
        start as i64,
        end as i64,
        use_uid,
        &set_clause,
        conditional,
        domains,
    );
    // The range is always the last two values the builder binds.
    let [range_start, range_end] = store_range_params(start, end, use_uid);
    let bound = queries.base_values.len();
    queries.base_values[bound - 2] = range_start;
    queries.base_values[bound - 1] = range_end;
    let user_id = user_id.to_string();

    async move {
        let store = async {
            if set_clause.is_empty() {
                let result = run(&queries.select_sql, &queries.base_values).await?;
                let rows: Vec<UpdatedMailFlags> = result.rows.iter().map(to_updated_mail_flags).collect();
                let Some(ceiling) = unchanged_since else {
                    return Ok(SetMailFlagsResult { updated: rows, failed: Vec::new() });
                };
                // A no-op still has to answer the UNCHANGEDSINCE question: a
                // row the client believes is older than it is belongs in
                // MODIFIED, not among the applied.
                let failed = rows.iter().filter(|row| row.modseq as f64 > ceiling).map(|row| row.uid).collect();
                let updated = rows.into_iter().filter(|row| row.modseq as f64 <= ceiling).collect();
                return Ok(SetMailFlagsResult { updated, failed });
            }

            let modseq = get_next_modseq(&user_id).await?;
            let mut values = queries.base_values.clone();
            values.push(Param::from(modseq));

            let Some(ceiling) = unchanged_since else {
                let result = run(&queries.update_sql, &values).await?;
                return Ok(SetMailFlagsResult {
                    updated: result.rows.iter().map(to_updated_mail_flags).collect(),
                    failed: Vec::new(),
                });
            };

            // MODIFIED names what the range matched and the guarded UPDATE
            // left alone, so the matched UIDs are read first.
            let matched = run(&queries.matched_uid_sql, &queries.base_values).await?;
            values.push(Param::from(ceiling));
            let result = run(&queries.update_sql, &values).await?;
            let updated: Vec<UpdatedMailFlags> = result.rows.iter().map(to_updated_mail_flags).collect();
            let updated_uids: HashSet<i64> = updated.iter().map(|row| row.uid).collect();
            let failed = matched
                .rows
                .iter()
                .map(|row| integer(row, "uid").unwrap_or(0))
                .filter(|uid| !updated_uids.contains(uid))
                .collect();
            Ok::<_, DbError>(SetMailFlagsResult { updated, failed })
        };
        store.await.unwrap_or_else(|error| {
            logger::error_with("Failed to set mail flags", json!({}), &error);
            SetMailFlagsResult::default()
        })
    }
    .boxed()
}

// ---------------------------------------------------------------------------
// SEARCH and UID lists
// ---------------------------------------------------------------------------

/// The UIDs of the mailbox's messages that match every criterion, ascending.
/// A database fault, or a criterion PostgreSQL cannot be asked, reads as no
/// match.
pub async fn search_mails_by_uid(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    criteria: &[SimplifiedCriterion],
    domains: Option<&[String]>,
) -> Vec<i64> {
    let search = async {
        let query = build_search_mails_by_uid_query(user_id, mailbox, sent, criteria, domains)?;
        let result = run_query(&query).await?;
        anyhow::Ok(result.rows.iter().filter_map(|row| integer(row, "uid")).filter(|uid| *uid > 0).collect())
    };
    search.await.unwrap_or_else(|error| {
        logger::error_with("Failed to search mails by UID", json!({}), &error);
        Vec::new()
    })
}

/// Every UID the mailbox shows, ascending: the sequence-number map of an
/// IMAP session. A database fault reads as an empty mailbox.
pub async fn get_all_uids(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> Vec<i64> {
    match run_query(&build_all_uids_query(user_id, mailbox, sent, domains)).await {
        Ok(result) => result.rows.iter().map(|row| integer(row, "uid").unwrap_or(0)).collect(),
        Err(error) => {
            logger::error_with("Failed to get all UIDs", json!({}), &error);
            Vec::new()
        }
    }
}

/// A mail's UID in a mailbox and its flags, for an untagged `FETCH (FLAGS …)`
/// pushed to IDLE sessions after a flag changed outside IMAP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFlagUpdate {
    pub mail_id: String,
    pub uid: i64,
    pub read: bool,
    pub saved: bool,
    pub deleted: bool,
    pub draft: bool,
    pub answered: bool,
}

/// The UID and flags of the named mails as the mailbox shows them. A mail the
/// mailbox does not hold is left out. A database fault reads as none.
pub async fn get_mail_flag_updates<S: AsRef<str>>(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    mail_ids: &[S],
    domains: Option<&[String]>,
) -> Vec<MailFlagUpdate> {
    if mail_ids.is_empty() {
        return Vec::new();
    }
    let query = build_mail_flag_updates_query(user_id, mailbox, sent, mail_ids, domains);
    match run_query(&query).await {
        Ok(result) => result
            .rows
            .iter()
            .map(|row| MailFlagUpdate {
                mail_id: text(row, MAIL_ID),
                uid: integer(row, "uid").unwrap_or(0),
                read: truthy(row.get(READ)),
                saved: truthy(row.get(SAVED)),
                deleted: truthy(row.get(DELETED)),
                draft: truthy(row.get(DRAFT)),
                answered: truthy(row.get(ANSWERED)),
            })
            .collect(),
        Err(error) => {
            logger::error_with("Failed to get mail flag updates", json!({}), &error);
            Vec::new()
        }
    }
}

/// The UID of the mailbox's lowest-UID unread message, or `None` when every
/// message is read. The SELECT response code `[UNSEEN <seq>]` carries that
/// message's sequence number, never the unread count. A database fault reads
/// as `None`.
pub async fn get_first_unseen_uid(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> Option<i64> {
    match run_query(&build_first_unseen_uid_query(user_id, mailbox, sent, domains)).await {
        Ok(result) => result.rows.first().and_then(|row| integer(row, "uid")),
        Err(error) => {
            logger::error_with("Failed to get first unseen UID", json!({}), &error);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// EXPUNGE
// ---------------------------------------------------------------------------

/// What an expunge writes: the soft-delete mark, the database clock, and a
/// fresh mod-sequence so the removal advances HIGHESTMODSEQ (RFC 7162) and a
/// resyncing client detects it.
async fn expunged_data(user_id: &str) -> DbResult<[(&'static str, SetValue); 3]> {
    Ok([
        (EXPUNGED, SetValue::from(true)),
        (UPDATED, SetValue::DbNow),
        (MODSEQ, SetValue::from(get_next_modseq(user_id).await?)),
    ])
}

/// Which of the mails a lookup found an expunge may still mark.
#[derive(Clone, Copy)]
enum ExpungeGuard {
    /// Every one: MOVE removes its source whatever the flags.
    Found,
    /// Those still `\Deleted` and not yet expunged when the UPDATE runs.
    StillDeleted,
    /// Those not yet expunged: a scoped domain-view expunge, whose lookup
    /// already applied the `\Deleted` test (`expungeSelected`).
    NotExpunged,
}

/// Marks the mails `lookup` finds expunged, and returns the UID each held in
/// the box. Nothing is reserved or written when it finds none.
async fn expunge_mapped(
    user_id: &str,
    lookup: &super::table::PreparedQuery,
    guard: ExpungeGuard,
) -> DbResult<Vec<i64>> {
    let selected = run_query(lookup).await?;
    if selected.rows.is_empty() {
        return Ok(Vec::new());
    }
    let mail_ids: Vec<String> = selected.rows.iter().map(|row| text(row, MAIL_ID)).collect();
    let uids_by_mail_id: HashMap<&str, Option<i64>> =
        mail_ids.iter().map(String::as_str).zip(selected.rows.iter().map(|row| integer(row, "uid"))).collect();

    let filters: Vec<(&str, Filter)> = match guard {
        ExpungeGuard::Found => vec![(MAIL_ID, Filter::any_of(&mail_ids))],
        ExpungeGuard::StillDeleted => vec![
            (USER_ID, Filter::from(user_id)),
            (MAIL_ID, Filter::any_of(&mail_ids)),
            (DELETED, Filter::from(true)),
            (EXPUNGED, Filter::from(false)),
        ],
        ExpungeGuard::NotExpunged => vec![
            (USER_ID, Filter::from(user_id)),
            (MAIL_ID, Filter::any_of(&mail_ids)),
            (EXPUNGED, Filter::from(false)),
        ],
    };
    let rows = mails_table().update_where(&filters, &expunged_data(user_id).await?, &[MAIL_ID]).await?;
    // The UPDATE returns ids; the UIDs the client knows them by come from
    // the lookup.
    Ok(rows
        .iter()
        .filter_map(|row| uids_by_mail_id.get(row.get(MAIL_ID).and_then(Value::as_str)?).copied().flatten())
        .collect())
}

async fn expunge_domain(user_id: &str, filters: &[(&str, Filter)]) -> DbResult<Vec<i64>> {
    let returning = format!("{UID_DOMAIN} as uid");
    let rows = mails_table().update_where(filters, &expunged_data(user_id).await?, &[returning.as_str()]).await?;
    Ok(rows.iter().map(|row| integer(row, "uid").unwrap_or(0)).collect())
}

/// EXPUNGE: marks the mailbox's `\Deleted` messages expunged (the rows stay)
/// and returns their UIDs. A database fault reads as nothing expunged.
///
/// Only the unified Sent view takes the single UPDATE. INBOX and the utility
/// views first look up their `\Deleted` mails less those filed somewhere
/// else, which they list but must not destroy
/// (`build_expunge_deleted_inbox_query`); a mapped box joins its own rows.
///
/// A login scoped to some `domains` expunges only their mail; on the unified
/// Sent view the filter bag cannot carry that predicate, so the rows are
/// selected and expunged by id.
pub async fn expunge_deleted_mails(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> Vec<i64> {
    let expunged = if !uses_domain_uid_space(mailbox) {
        expunge_mapped(
            user_id,
            &build_expunge_deleted_select_query(user_id, mailbox, sent, domains),
            ExpungeGuard::StillDeleted,
        )
        .await
    } else if let (true, Some(domains)) = (sent, domains) {
        let scoped = build_scoped_expunge_deleted_select_query(user_id, mailbox, sent, domains);
        expunge_mapped(user_id, &scoped, ExpungeGuard::NotExpunged).await
    } else if sent {
        expunge_domain(user_id, &build_expunge_deleted_filters(user_id, mailbox, sent)).await
    } else {
        expunge_mapped(
            user_id,
            &build_expunge_deleted_inbox_query(user_id, mailbox, sent, domains),
            ExpungeGuard::StillDeleted,
        )
        .await
    };
    expunged.unwrap_or_else(|error| {
        logger::error_with("Failed to expunge deleted mails", json!({}), &error);
        Vec::new()
    })
}

/// Marks the named UIDs of one mailbox expunged, whatever their `\Deleted`
/// flag, and returns the UIDs it marked: fewer than asked when some were
/// already gone. This is MOVE's removal of its source. RFC 6851 §3.3 rules
/// out STORE `\Deleted` plus EXPUNGE, which would also remove messages that
/// were already flagged.
///
/// Fails on a database fault, so that a MOVE is not reported done.
pub async fn expunge_mails_by_uid(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    uids: &[i64],
    domains: Option<&[String]>,
) -> DbResult<Vec<i64>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let expunged = match domains {
        Some(domains) if uses_domain_uid_space(mailbox) => {
            let scoped = build_scoped_expunge_uids_select_query(user_id, mailbox, sent, uids, domains);
            expunge_mapped(user_id, &scoped, ExpungeGuard::NotExpunged).await
        }
        _ if uses_domain_uid_space(mailbox) => {
            expunge_domain(user_id, &build_expunge_uids_filters(user_id, mailbox, sent, uids)).await
        }
        _ => {
            let lookup = build_expunge_uids_select_query(user_id, mailbox, sent, uids, domains);
            expunge_mapped(user_id, &lookup, ExpungeGuard::Found).await
        }
    };
    if let Err(error) = &expunged {
        logger::error_with("Failed to expunge mails by UID", json!({ "uids": uids }), error);
    }
    expunged
}

/// A switch for tests that answer the pool from a script: the script lives in
/// the calling task, so a range read has to run there too instead of on the
/// task the single flight spawns.
#[cfg(test)]
pub(crate) mod testing {
    use std::future::Future;

    tokio::task_local! {
        static INLINE: ();
    }

    pub(crate) fn reads_inline() -> bool {
        INLINE.try_with(|_| ()).is_ok()
    }

    /// Runs `work` with every range read it makes kept on its own task.
    pub(crate) async fn with_inline_reads<F: Future>(work: F) -> F::Output {
        INLINE.scope((), work).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures_util::TryStreamExt;

    use super::super::table::script::{fault, rows, with_script};
    use super::testing::with_inline_reads;
    use super::*;
    use crate::imap::types::SequenceRange;
    use crate::imap::util::JsDate;
    use crate::postgres::repositories::mails::imap_query::{DateKey, TextKey};

    const USER: &str = "11111111-1111-4111-8111-111111111111";
    const MAIL: &str = "22222222-2222-4222-8222-222222222222";
    const OTHER_MAIL: &str = "33333333-3333-4333-8333-333333333333";

    // imap.test.ts — "STORE operation types"
    //
    // The Bun test simulates the clause on a row of flags; here the clause
    // itself is applied.

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Flags {
        read: bool,
        saved: bool,
        deleted: bool,
        draft: bool,
        answered: bool,
    }

    const fn flags(read: bool, saved: bool, deleted: bool, draft: bool, answered: bool) -> Flags {
        Flags { read, saved, deleted, draft, answered }
    }

    fn apply(operation: StoreOperationType, named: &[&str], current: Flags) -> Flags {
        let mut result = current;
        for assignment in build_flag_set_clause(operation, named).split(',').map(str::trim).filter(|a| !a.is_empty()) {
            let (column, value) = assignment.split_once(" = ").expect("an assignment");
            let value = value.eq_ignore_ascii_case("true");
            match column {
                "read" => result.read = value,
                "saved" => result.saved = value,
                "deleted" => result.deleted = value,
                "draft" => result.draft = value,
                "answered" => result.answered = value,
                other => panic!("no flag column {other}"),
            }
        }
        result
    }

    #[test]
    fn flags_replaces_all_flags_with_the_specified_flags() {
        let current = flags(true, true, false, false, true);
        assert_eq!(
            apply(StoreOperationType::Flags, &["\\Seen", "\\Deleted"], current),
            flags(true, false, true, false, false)
        );
    }

    #[test]
    fn flags_clears_all_flags_when_the_flag_list_is_empty() {
        let current = flags(true, true, true, true, true);
        assert_eq!(apply(StoreOperationType::Flags, &[], current), flags(false, false, false, false, false));
    }

    #[test]
    fn add_sets_flags_without_affecting_others() {
        let current = flags(false, true, false, false, false);
        assert_eq!(
            apply(StoreOperationType::Add, &["\\Seen", "\\Deleted"], current),
            flags(true, true, true, false, false)
        );
    }

    #[test]
    fn add_does_not_change_flags_that_are_already_set() {
        let current = flags(true, true, false, false, false);
        assert_eq!(apply(StoreOperationType::Add, &["\\Seen"], current), current);
    }

    #[test]
    fn add_handles_an_empty_flag_list_without_changes() {
        let current = flags(true, false, false, true, false);
        assert_eq!(apply(StoreOperationType::Add, &[], current), current);
    }

    #[test]
    fn remove_clears_flags_without_affecting_others() {
        let current = flags(true, true, true, false, true);
        assert_eq!(
            apply(StoreOperationType::Remove, &["\\Seen", "\\Answered"], current),
            flags(false, true, true, false, false)
        );
    }

    #[test]
    fn remove_does_not_change_flags_that_are_already_unset() {
        let current = flags(false, true, false, false, false);
        assert_eq!(apply(StoreOperationType::Remove, &["\\Seen"], current), current);
    }

    #[test]
    fn remove_handles_an_empty_flag_list_without_changes() {
        let current = flags(true, false, false, true, false);
        assert_eq!(apply(StoreOperationType::Remove, &[], current), current);
    }

    #[test]
    fn marking_as_read_does_not_mark_as_deleted() {
        let result = apply(StoreOperationType::Add, &["\\Seen"], flags(false, false, false, false, false));
        assert!(result.read);
        assert!(!result.deleted);
    }

    #[test]
    fn marking_for_deletion_keeps_the_read_status() {
        let current = flags(true, true, false, false, false);
        assert_eq!(apply(StoreOperationType::Add, &["\\Deleted"], current), flags(true, true, true, false, false));
    }

    #[test]
    fn undeleting_a_message_keeps_the_read_status() {
        let result = apply(StoreOperationType::Remove, &["\\Deleted"], flags(true, false, true, false, false));
        assert!(!result.deleted);
        assert!(result.read);
    }

    // imap.test.ts — "buildFlagSetClause — empty/unknown-only STORE is a no-op (#671)"

    #[test]
    fn returns_nothing_for_add_or_remove_with_an_empty_flag_list() {
        assert_eq!(build_flag_set_clause::<&str>(StoreOperationType::Add, &[]), "");
        assert_eq!(build_flag_set_clause::<&str>(StoreOperationType::Remove, &[]), "");
    }

    #[test]
    fn returns_nothing_for_add_with_only_unknown_or_custom_keywords() {
        assert_eq!(build_flag_set_clause(StoreOperationType::Add, &["\\CustomKeyword", "Foo"]), "");
        // Flag names are matched as written.
        assert_eq!(build_flag_set_clause(StoreOperationType::Add, &["\\seen", "\\SEEN"]), "");
    }

    #[test]
    fn never_emits_the_updated_double_assignment_sentinel() {
        for operation in [StoreOperationType::Add, StoreOperationType::Remove] {
            for named in [vec![], vec!["Foo"], vec!["\\Seen"]] {
                assert!(!build_flag_set_clause(operation, &named).contains("updated"));
            }
        }
    }

    #[test]
    fn still_emits_real_column_assignments_when_a_flag_is_recognized() {
        assert_eq!(build_flag_set_clause(StoreOperationType::Add, &["\\Seen"]), "read = TRUE");
        assert_eq!(build_flag_set_clause(StoreOperationType::Add, &["\\Seen", "\\Deleted"]), "read = TRUE, deleted = TRUE");
        assert_eq!(build_flag_set_clause(StoreOperationType::Remove, &["\\Flagged"]), "saved = FALSE");
        // The assignments come in column order, not in the order named.
        assert_eq!(
            build_flag_set_clause(StoreOperationType::Add, &["\\Answered", "\\Draft", "\\Seen"]),
            "read = TRUE, draft = TRUE, answered = TRUE"
        );
    }

    #[test]
    fn flags_replace_mode_is_always_a_full_assignment() {
        let clause = build_flag_set_clause::<&str>(StoreOperationType::Flags, &[]);
        assert!(clause.contains("read = false"));
        assert!(clause.contains("answered = false"));
    }

    #[test]
    fn the_operation_of_a_store_item_drops_its_silent_suffix() {
        for operation in StoreOperation::ALL {
            let base = operation.as_str().replace(".SILENT", "");
            assert_eq!(StoreOperationType::from(operation).as_str(), base);
            assert_eq!(StoreOperationType::parse(&base), Some(StoreOperationType::from(operation)));
        }
        assert_eq!(StoreOperationType::parse("FLAGS.SILENT"), None);
    }

    // imap.test.ts — "setMailFlags — no-op STORE skips the UPDATE (#671)"

    fn flag_row(uid: i64, modseq: i64) -> Value {
        json!({ "mail_id": MAIL, "uid": uid, "read": true, "saved": false, "deleted": false, "draft": false, "answered": true, "modseq": modseq })
    }

    #[tokio::test]
    async fn a_no_op_store_runs_the_select_and_never_the_update() {
        let (result, trace) = with_script(
            vec![rows(json!([flag_row(4, 7)]))],
            set_mail_flags::<&str>(USER, None, false, 1.0, 9.0, &["Foo"], true, StoreOperationType::Add, None, None),
        )
        .await;
        assert_eq!(trace.len(), 1);
        assert!(trace[0].sql.starts_with("SELECT mail_id, uid_domain as uid"));
        assert!(!trace[0].sql.contains("UPDATE"));
        assert!(!trace[0].sql.contains("mail_uid_counters"));
        assert_eq!(trace[0].values, params![USER, false, 1, 9]);
        assert_eq!(
            result.updated,
            vec![UpdatedMailFlags {
                mail_id: MAIL.to_string(),
                uid: 4,
                read: true,
                saved: false,
                deleted: false,
                draft: false,
                answered: true,
                modseq: 7,
            }]
        );
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    async fn a_conditional_no_op_still_names_the_rows_that_moved_on() {
        let (result, trace) = with_script(
            vec![rows(json!([flag_row(4, 7), flag_row(5, 8), flag_row(6, 9)]))],
            set_mail_flags::<&str>(USER, Some("Archive"), false, 1.0, 9.0, &[], true, StoreOperationType::Remove, Some(8.0), None),
        )
        .await;
        assert_eq!(trace.len(), 1);
        assert_eq!(result.updated.iter().map(|row| row.uid).collect::<Vec<_>>(), vec![4, 5]);
        assert_eq!(result.failed, vec![6]);
    }

    #[tokio::test]
    async fn an_unconditional_store_reserves_one_mod_sequence_and_updates_once() {
        let (result, trace) = with_script(
            vec![rows(json!([{ "next_uid": 12 }])), rows(json!([flag_row(4, 12), flag_row(5, 12)]))],
            set_mail_flags(USER, None, false, 3.0, 9.0, &["\\Seen"], true, StoreOperationType::Add, None, None),
        )
        .await;
        assert_eq!(trace.len(), 2);
        assert!(trace[0].sql.contains("INSERT INTO mail_uid_counters"));
        assert!(trace[1].sql.starts_with("UPDATE mails\n        SET read = TRUE, updated = CURRENT_TIMESTAMP, modseq = $5"));
        assert_eq!(trace[1].values, params![USER, false, 3, 9, 12]);
        assert_eq!(result.updated.len(), 2);
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    async fn a_conditional_store_reports_what_it_matched_and_did_not_update() {
        let (result, trace) = with_script(
            vec![
                rows(json!([{ "next_uid": 20 }])),
                rows(json!([{ "uid": 4 }, { "uid": 5 }, { "uid": 6 }])),
                rows(json!([flag_row(5, 20)])),
            ],
            set_mail_flags(
                USER,
                Some("Archive"),
                false,
                4.0,
                6.0,
                &["\\Deleted"],
                true,
                StoreOperationType::Add,
                Some(15.0),
                None,
            ),
        )
        .await;
        assert_eq!(trace.len(), 3);
        assert!(trace[1].sql.starts_with("SELECT x.uid as uid FROM"));
        assert_eq!(trace[1].values, params![USER, false, "Archive", 4, 6]);
        assert!(trace[2].sql.contains("AND m.modseq <= $7"));
        assert_eq!(trace[2].values, params![USER, false, "Archive", 4, 6, 20, 15]);
        assert_eq!(result.updated.iter().map(|row| row.uid).collect::<Vec<_>>(), vec![5]);
        assert_eq!(result.failed, vec![4, 6]);
    }

    #[tokio::test]
    async fn a_sequence_store_binds_an_offset_and_a_count() {
        let (_, trace) = with_script(
            vec![rows(json!([]))],
            set_mail_flags::<&str>(USER, None, true, 3.0, 9.0, &[], false, StoreOperationType::Add, None, None),
        )
        .await;
        assert_eq!(trace[0].values, params![USER, true, 2, 7]);
    }

    #[tokio::test]
    async fn a_store_binds_a_number_postgres_cannot_read_as_javascript_prints_it() {
        let (result, trace) = with_script(
            vec![rows(json!([{ "next_uid": 3 }])), fault("22P02")],
            set_mail_flags(USER, None, false, 1e21, f64::INFINITY, &["\\Seen"], true, StoreOperationType::Add, None, None),
        )
        .await;
        // The mod-sequence is reserved before the UPDATE fails.
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[1].values, params![USER, false, "1e+21", "Infinity", 3]);
        assert_eq!(result, SetMailFlagsResult::default());
    }

    #[tokio::test]
    async fn a_failed_store_reads_as_one_that_addressed_nothing() {
        let (result, _) = with_script(
            vec![fault("57014")],
            set_mail_flags(USER, None, false, 1.0, 2.0, &["\\Seen"], true, StoreOperationType::Flags, None, None),
        )
        .await;
        assert_eq!(result, SetMailFlagsResult::default());
    }

    // pg-text-chunks.test.ts — "pageByCodePoints reassembles the column exactly (#765)"

    /// `SUBSTRING(col FROM offset FOR take)` over a string: 1-indexed,
    /// counting characters, empty once the offset is past the end.
    async fn drain(stored: &str, chunk_chars: usize) -> (String, Vec<usize>) {
        let offsets = Mutex::new(Vec::new());
        let chunks: Vec<String> = page_by_code_points(
            |offset, take| {
                offsets.lock().unwrap().push(offset);
                let chunk: String = stored.chars().skip(offset.saturating_sub(1)).take(take).collect();
                async move { Ok::<_, DbError>(chunk) }
            },
            chunk_chars,
        )
        .try_collect()
        .await
        .unwrap();
        (chunks.concat(), offsets.into_inner().unwrap())
    }

    #[tokio::test]
    async fn round_trips_a_pure_ascii_column() {
        let body = "abcdefghij".repeat(50);
        let (out, offsets) = drain(&body, 100).await;
        assert_eq!(out, body);
        assert_eq!(offsets[..5], [1, 101, 201, 301, 401]);
    }

    #[tokio::test]
    async fn round_trips_a_column_whose_chunks_contain_astral_characters() {
        let body = "ab🙂cd🙂ef🙂gh🙂".repeat(60);
        let (out, offsets) = drain(&body, 100).await;
        assert_eq!(out, body);
        assert_eq!(out.len(), body.len());
        // Offsets advance by characters, not by UTF-16 units.
        assert_eq!(offsets[..5], [1, 101, 201, 301, 401]);
    }

    #[tokio::test]
    async fn does_not_drop_the_character_sitting_exactly_on_a_chunk_boundary() {
        let body = format!("{}🙂{}🙂{}", "x".repeat(9), "y".repeat(9), "z".repeat(9));
        let (out, _) = drain(&body, 10).await;
        assert_eq!(out, body);
        assert_eq!(out.chars().count(), body.chars().count());
    }

    #[tokio::test]
    async fn terminates_on_a_final_chunk_that_is_short_in_code_points() {
        // 12 characters, 16 UTF-16 units, 24 bytes: only the character count
        // is below the chunk size.
        let body = "ab🙂cd🙂ef🙂gh🙂";
        let (out, offsets) = drain(body, 16).await;
        assert_eq!(out, body);
        assert_eq!(offsets.len(), 1);
    }

    #[tokio::test]
    async fn handles_a_column_that_is_entirely_astral_characters() {
        let body = "🙂".repeat(250);
        let (out, offsets) = drain(&body, 100).await;
        assert_eq!(out, body);
        assert_eq!(out.len(), 1000);
        assert_eq!(offsets[..3], [1, 101, 201]);
    }

    #[tokio::test]
    async fn mixes_multi_byte_bmp_and_astral_characters_without_drift() {
        let body = "café ☕ 日本語 😀 ".repeat(500);
        let (out, _) = drain(&body, PG_TEXT_CHUNK_CHARS).await;
        assert_eq!(out, body);
        assert_eq!(out.len(), body.len());
    }

    #[tokio::test]
    async fn returns_nothing_for_an_absent_column_without_paging_forever() {
        let (out, offsets) = drain("", 100).await;
        assert_eq!(out, "");
        assert_eq!(offsets.len(), 1);
    }

    #[tokio::test]
    async fn stops_after_one_read_when_the_column_is_exactly_one_short_chunk() {
        let (out, offsets) = drain("abc", 100).await;
        assert_eq!(out, "abc");
        assert_eq!(offsets, vec![1]);
    }

    #[tokio::test]
    async fn pages_once_more_when_the_column_ends_on_a_chunk_boundary() {
        let (out, offsets) = drain(&"x".repeat(200), 100).await;
        assert_eq!(out.len(), 200);
        assert_eq!(offsets, vec![1, 101, 201]);
    }

    #[tokio::test]
    async fn a_failed_read_ends_the_stream_with_its_error() {
        let calls = Mutex::new(0);
        let mut chunks = std::pin::pin!(page_by_code_points(
            |_, take| {
                *calls.lock().unwrap() += 1;
                let first = *calls.lock().unwrap() == 1;
                async move {
                    if first {
                        Ok("y".repeat(take))
                    } else {
                        Err(DbError::new("connection lost"))
                    }
                }
            },
            4,
        ));
        assert_eq!(chunks.try_next().await.unwrap().as_deref(), Some("yyyy"));
        assert_eq!(chunks.try_next().await.unwrap_err().message, "connection lost");
        assert!(chunks.try_next().await.unwrap().is_none());
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    // The readers over the pool.

    #[tokio::test]
    async fn the_text_reader_pages_with_typed_offsets() {
        let (chunks, trace) = with_script(
            vec![rows(json!([{ "chunk": "ab🙂" }])), rows(json!([{ "chunk": "c" }]))],
            pg_text_chunks(MAIL, USER, TextColumn::Html, 3).try_collect::<Vec<String>>(),
        )
        .await;
        assert_eq!(chunks.unwrap(), vec!["ab🙂", "c"]);
        assert_eq!(
            trace[0].sql,
            "SELECT SUBSTRING(html FROM $3::int FOR $4::int) AS chunk\n                 FROM mails WHERE mail_id = $1 AND user_id = $2"
        );
        assert_eq!(trace[0].values, params![MAIL, USER, 1, 3]);
        assert_eq!(trace[1].values, params![MAIL, USER, 4, 3]);
    }

    #[tokio::test]
    async fn the_text_reader_reads_a_missing_row_as_an_empty_column() {
        let (chunks, trace) = with_script(
            vec![rows(json!([]))],
            pg_text_chunks(MAIL, USER, TextColumn::Text, PG_TEXT_CHUNK_CHARS).try_collect::<Vec<String>>(),
        )
        .await;
        assert!(chunks.unwrap().is_empty());
        assert_eq!(trace[0].values, params![MAIL, USER, 1, 12000]);
    }

    #[tokio::test]
    async fn the_byte_reader_seeks_and_pages_by_bytes() {
        let (chunks, trace) = with_script(
            vec![rows(json!([{ "chunk": "616263" }])), rows(json!([{ "chunk": "f09f" }]))],
            pg_byte_chunks(MAIL, USER, TextColumn::Text, 7, 3).try_collect::<Vec<Vec<u8>>>(),
        )
        .await;
        assert_eq!(chunks.unwrap(), vec![b"abc".to_vec(), vec![0xf0, 0x9f]]);
        assert_eq!(
            trace[0].sql,
            "SELECT SUBSTRING(convert_to(text, 'UTF8') FROM $3::int FOR $4::int) AS chunk\n                 FROM mails WHERE mail_id = $1 AND user_id = $2"
        );
        assert_eq!(trace[0].values, params![MAIL, USER, 7, 3]);
        assert_eq!(trace[1].values, params![MAIL, USER, 10, 3]);
    }

    #[tokio::test]
    async fn the_byte_reader_stops_on_an_empty_chunk_and_reports_a_fault() {
        let (chunks, trace) = with_script(
            vec![rows(json!([{ "chunk": "" }]))],
            pg_byte_chunks(MAIL, USER, TextColumn::Html, 1, PG_TEXT_CHUNK_BYTES).try_collect::<Vec<Vec<u8>>>(),
        )
        .await;
        assert!(chunks.unwrap().is_empty());
        assert_eq!(trace[0].values, params![MAIL, USER, 1, 49152]);

        let (chunks, _) = with_script(
            vec![rows(json!([{ "chunk": null }]))],
            pg_byte_chunks(MAIL, USER, TextColumn::Html, 1, 3).try_collect::<Vec<Vec<u8>>>(),
        )
        .await;
        assert!(chunks.unwrap().is_empty());

        let (chunks, _) = with_script(
            vec![fault("57014")],
            pg_byte_chunks(MAIL, USER, TextColumn::Html, 1, 3).try_collect::<Vec<Vec<u8>>>(),
        )
        .await;
        assert_eq!(chunks.unwrap_err().code.as_deref(), Some("57014"));
    }

    #[test]
    fn the_body_sources_of_a_message_segment_name_the_columns() {
        assert_eq!(TextColumn::from(BodySource::Text).as_str(), "text");
        assert_eq!(TextColumn::from(BodySource::Html).as_str(), "html");
    }

    // Counts, UID lists and flag pushes over the pool.

    #[tokio::test]
    async fn counts_read_numbers_and_absorb_a_fault() {
        let (counts, trace) =
            with_script(vec![rows(json!([{ "total": 7, "unread": 2 }]))], count_messages(USER, None, false, None)).await;
        assert_eq!(counts, MessageCounts { total: 7, unread: 2 });
        assert_eq!(trace[0], build_count_messages_query(USER, None, false, None));

        let (counts, _) = with_script(vec![rows(json!([{ "total": "12", "unread": null }]))], count_messages(USER, None, false, None)).await;
        assert_eq!(counts, MessageCounts { total: 12, unread: 0 });
        let (counts, _) = with_script(vec![rows(json!([]))], count_messages(USER, Some("Archive"), true, None)).await;
        assert_eq!(counts, MessageCounts::default());
        let (counts, _) = with_script(vec![fault("08006")], count_messages(USER, None, false, None)).await;
        assert_eq!(counts, MessageCounts::default());
    }

    #[tokio::test]
    async fn the_uid_list_absorbs_a_fault_as_an_empty_mailbox() {
        let (uids, trace) = with_script(
            vec![rows(json!([{ "uid": 4 }, { "uid": 8 }, { "uid": 15 }]))],
            get_all_uids(USER, Some("Archive"), false, None),
        )
        .await;
        assert_eq!(uids, vec![4, 8, 15]);
        assert_eq!(trace[0], build_all_uids_query(USER, Some("Archive"), false, None));
        let (uids, _) = with_script(vec![fault("08006")], get_all_uids(USER, None, false, None)).await;
        assert!(uids.is_empty());
    }

    #[tokio::test]
    async fn the_first_unseen_uid_is_none_when_all_are_read_or_the_read_fails() {
        let (uid, trace) = with_script(vec![rows(json!([{ "uid": 42 }]))], get_first_unseen_uid(USER, None, false, None)).await;
        assert_eq!(uid, Some(42));
        assert_eq!(trace[0], build_first_unseen_uid_query(USER, None, false, None));
        let (uid, _) = with_script(vec![rows(json!([]))], get_first_unseen_uid(USER, None, false, None)).await;
        assert_eq!(uid, None);
        let (uid, _) = with_script(vec![fault("08006")], get_first_unseen_uid(USER, None, false, None)).await;
        assert_eq!(uid, None);
    }

    #[tokio::test]
    async fn flag_updates_issue_no_statement_for_no_mails() {
        let (updates, trace) = with_script(vec![], get_mail_flag_updates::<&str>(USER, None, false, &[], None)).await;
        assert!(updates.is_empty());
        assert!(trace.is_empty());

        let (updates, trace) = with_script(
            vec![rows(json!([{ "mail_id": MAIL, "uid": 9, "read": true, "saved": false, "deleted": null, "draft": false, "answered": true }]))],
            get_mail_flag_updates(USER, Some("Archive"), false, &[MAIL, OTHER_MAIL], None),
        )
        .await;
        assert_eq!(
            updates,
            vec![MailFlagUpdate {
                mail_id: MAIL.to_string(),
                uid: 9,
                read: true,
                saved: false,
                deleted: false,
                draft: false,
                answered: true,
            }]
        );
        assert_eq!(trace[0], build_mail_flag_updates_query(USER, Some("Archive"), false, &[MAIL, OTHER_MAIL], None));
        let (updates, _) = with_script(vec![fault("08006")], get_mail_flag_updates(USER, None, false, &[MAIL], None)).await;
        assert!(updates.is_empty());
    }

    // SEARCH over the pool.

    #[tokio::test]
    async fn search_returns_the_positive_uids_and_absorbs_a_fault() {
        let criteria = [SimplifiedCriterion::text(TextKey::Subject, "x")];
        let (uids, trace) = with_script(
            vec![rows(json!([{ "uid": 0 }, { "uid": 3 }, { "uid": 5 }]))],
            search_mails_by_uid(USER, None, false, &criteria, None),
        )
        .await;
        assert_eq!(uids, vec![3, 5]);
        assert_eq!(trace[0], build_search_mails_by_uid_query(USER, None, false, &criteria, None).unwrap());
        let (uids, _) = with_script(vec![fault("08006")], search_mails_by_uid(USER, None, false, &criteria, None)).await;
        assert!(uids.is_empty());
    }

    #[tokio::test]
    async fn a_search_on_an_invalid_date_matches_nothing_and_asks_nothing() {
        let criteria = [SimplifiedCriterion::Date { key: DateKey::Since, value: JsDate::invalid() }];
        let (uids, trace) = with_script(vec![], search_mails_by_uid(USER, None, false, &criteria, None)).await;
        assert!(uids.is_empty());
        assert!(trace.is_empty());
    }

    #[tokio::test]
    async fn a_search_binds_its_uid_set_after_the_mailbox() {
        let criteria = [SimplifiedCriterion::UidSet(vec![SequenceRange::range(2.0, 4.0), SequenceRange::single(9.0)])];
        let (_, trace) =
            with_script(vec![rows(json!([]))], search_mails_by_uid(USER, Some("Archive"), false, &criteria, None)).await;
        assert!(trace[0].sql.contains("((x.uid >= $4 AND x.uid <= $5) OR x.uid = $6)"));
        assert_eq!(trace[0].values, params![USER, false, "Archive", 2, 4, 9]);
    }

    // Range reads over the pool.

    #[tokio::test]
    async fn a_range_read_keys_validated_rows_by_mail_id_in_row_order() {
        // The read starts when it is called, so the call is made inside the
        // scopes.
        let (mails, trace) = with_inline_reads(with_script(
            vec![rows(json!([
                { "mail_id": OTHER_MAIL, "read": true, "uid_domain": 3 },
                { "mail_id": MAIL, "read": false, "uid_domain": 5 },
            ]))],
            async { get_mails_by_range(USER, None, false, 1.0, 9.0, true, &["read", "uid_domain"], None, None).await },
        ))
        .await;
        assert_eq!(mails.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), vec![OTHER_MAIL, MAIL]);
        assert_eq!(mails[0].1.read(), Some(true));
        assert_eq!(mails[1].1.uid_domain(), Some(5));
        assert_eq!(mails[1].1.subject(), None);
        let expected = build_mails_by_range_query(USER, None, false, 1.0, 9.0, true, &["read", "uid_domain"], None, None);
        assert_eq!((trace[0].sql.as_str(), &trace[0].values), (expected.sql.as_str(), &expected.values));
    }

    #[tokio::test]
    async fn a_range_read_with_a_bad_row_or_a_fault_reads_as_no_messages() {
        let (mails, _) = with_inline_reads(with_script(
            vec![rows(json!([{ "mail_id": MAIL, "read": "yes" }]))],
            async { get_mails_by_range(USER, None, false, 1.0, 9.0, true, &["read"], None, None).await },
        ))
        .await;
        assert!(mails.is_empty());
        let (mails, _) = with_inline_reads(with_script(
            vec![fault("08006")],
            async { get_mails_by_range(USER, Some("Archive"), true, 1.0, 9.0, false, &["*"], Some(4.0), None).await },
        ))
        .await;
        assert!(mails.is_empty());
    }

    #[test]
    fn identical_range_reads_share_a_key_whatever_order_their_fields_come_in() {
        let key = |fields: &[&str], changed_since| {
            let fields: Vec<String> = fields.iter().map(|field| field.to_string()).collect();
            inflight_key("u\"1", Some("Archive"), false, 1.0, 9_007_199_254_740_991.0, true, &fields, changed_since, None)
        };
        assert_eq!(
            key(&["uid_domain", "read", "Z"], None),
            r#"["u\"1","Archive",false,1,9007199254740991,true,["Z","read","uid_domain"],null,null]"#
        );
        // A scoped read is a different read from the whole store's.
        let scoped = inflight_key("u", None, false, 1.0, 2.0, true, &[], None, Some(&["a.test".to_string()]));
        assert_eq!(scoped, r#"["u",null,false,1,2,true,[],null,["a.test"]]"#);
        assert_eq!(key(&["read", "uid_domain"], Some(7.0)), key(&["uid_domain", "read"], Some(7.0)));
        assert_ne!(key(&["read"], None), key(&["read", "uid_domain"], None));
        assert_ne!(key(&["read"], None), key(&["read"], Some(0.0)));
        assert_eq!(
            inflight_key("u", None, true, f64::NAN, f64::INFINITY, false, &[], Some(-0.0), None),
            r#"["u",null,true,null,null,false,[],0,null]"#
        );
    }

    #[tokio::test]
    async fn concurrent_identical_range_reads_share_their_rows() {
        // No pool is configured for unit tests, so the read fails and is
        // absorbed; what matters is that both callers hold the same result.
        let first = get_mails_by_range(USER, Some("coalesced"), false, 1.0, 2.0, true, &["read"], None, None);
        let second = get_mails_by_range(USER, Some("coalesced"), false, 1.0, 2.0, true, &["read"], None, None);
        let (first, second) = tokio::join!(first, second);
        assert!(Arc::ptr_eq(&first, &second));
    }

    // EXPUNGE over the pool.

    #[tokio::test]
    async fn a_unified_sent_expunge_reserves_a_mod_sequence_then_marks_the_deleted_rows() {
        let (uids, trace) = with_script(
            vec![rows(json!([{ "next_uid": 31 }])), rows(json!([{ "uid": 2 }, { "uid": 7 }]))],
            expunge_deleted_mails(USER, None, true, None),
        )
        .await;
        assert_eq!(uids, vec![2, 7]);
        assert!(trace[0].sql.contains("INSERT INTO mail_uid_counters"));
        assert_eq!(
            trace[1].sql,
            "UPDATE mails SET expunged = $1, updated = CURRENT_TIMESTAMP, modseq = $2 WHERE user_id = $3 AND sent = $4 AND deleted = $5 AND expunged = $6 RETURNING uid_domain as uid"
        );
        assert_eq!(trace[1].values, params![true, 31, USER, true, true, false]);
    }

    #[tokio::test]
    async fn an_inbox_expunge_looks_up_what_inbox_may_remove_then_marks_it() {
        // A copy filed into Trash is listed in INBOX flagged \Deleted; the
        // lookup leaves it out, so it is never marked.
        for mailbox in [None, Some("Drafts"), Some("Junk")] {
            let (uids, trace) = with_script(
                vec![
                    rows(json!([{ "mail_id": MAIL, "uid": 2 }])),
                    rows(json!([{ "next_uid": 31 }])),
                    rows(json!([{ "mail_id": MAIL }])),
                ],
                expunge_deleted_mails(USER, mailbox, false, None),
            )
            .await;
            assert_eq!(uids, vec![2]);
            assert_eq!(trace[0], build_expunge_deleted_inbox_query(USER, mailbox, false, None));
            assert_eq!(trace[2].values, params![true, 31, USER, MAIL, true, false]);
        }
    }

    #[tokio::test]
    async fn a_mapped_expunge_with_nothing_deleted_reserves_and_writes_nothing() {
        let (uids, trace) = with_script(vec![rows(json!([]))], expunge_deleted_mails(USER, Some("Archive"), false, None)).await;
        assert!(uids.is_empty());
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0], build_expunge_deleted_select_query(USER, Some("Archive"), false, None));
    }

    // Another client can clear \Deleted between the lookup and the UPDATE, so
    // the UPDATE asks again and only the rows it returns are reported.
    #[tokio::test]
    async fn a_mapped_expunge_marks_only_rows_still_deleted_and_reports_their_uids() {
        let (uids, trace) = with_script(
            vec![
                rows(json!([{ "mail_id": MAIL, "uid": 4 }, { "mail_id": OTHER_MAIL, "uid": 9 }])),
                rows(json!([{ "next_uid": 31 }])),
                rows(json!([{ "mail_id": OTHER_MAIL }])),
            ],
            expunge_deleted_mails(USER, Some("Archive"), false, None),
        )
        .await;
        assert_eq!(uids, vec![9]);
        assert_eq!(
            trace[2].sql,
            "UPDATE mails SET expunged = $1, updated = CURRENT_TIMESTAMP, modseq = $2 WHERE user_id = $3 AND mail_id IN ($4, $5) AND deleted = $6 AND expunged = $7 RETURNING mail_id"
        );
        assert_eq!(trace[2].values, params![true, 31, USER, MAIL, OTHER_MAIL, true, false]);
    }

    #[tokio::test]
    async fn a_move_removes_its_source_rows_whatever_their_flags() {
        let (uids, trace) = with_script(
            vec![rows(json!([{ "mail_id": MAIL, "uid": 4 }])), rows(json!([{ "next_uid": 31 }])), rows(json!([{ "mail_id": MAIL }]))],
            expunge_mails_by_uid(USER, Some("Archive"), false, &[4], None),
        )
        .await;
        assert_eq!(uids.unwrap(), vec![4]);
        assert_eq!(
            trace[2].sql,
            "UPDATE mails SET expunged = $1, updated = CURRENT_TIMESTAMP, modseq = $2 WHERE mail_id IN ($3) RETURNING mail_id"
        );
    }

    #[tokio::test]
    async fn a_failed_expunge_reads_as_nothing_expunged() {
        let (uids, _) = with_script(vec![fault("08006")], expunge_deleted_mails(USER, None, false, None)).await;
        assert!(uids.is_empty());
        let (uids, _) = with_script(
            vec![rows(json!([{ "mail_id": MAIL, "uid": 4 }])), fault("08006")],
            expunge_deleted_mails(USER, Some("Archive"), false, None),
        )
        .await;
        assert!(uids.is_empty());
    }

    #[tokio::test]
    async fn expunging_named_uids_asks_nothing_for_none_and_reports_a_fault() {
        let (uids, trace) = with_script(vec![], expunge_mails_by_uid(USER, None, false, &[], None)).await;
        assert!(uids.unwrap().is_empty());
        assert!(trace.is_empty());

        let (uids, trace) = with_script(
            vec![rows(json!([{ "next_uid": 40 }])), rows(json!([{ "uid": 4 }]))],
            expunge_mails_by_uid(USER, None, true, &[4, 9], None),
        )
        .await;
        assert_eq!(uids.unwrap(), vec![4]);
        assert_eq!(
            trace[1].sql,
            "UPDATE mails SET expunged = $1, updated = CURRENT_TIMESTAMP, modseq = $2 WHERE user_id = $3 AND sent = $4 AND expunged = $5 AND uid_domain IN ($6, $7) RETURNING uid_domain as uid"
        );
        assert_eq!(trace[1].values, params![true, 40, USER, true, false, 4, 9]);

        let (uids, trace) = with_script(
            vec![
                rows(json!([{ "mail_id": MAIL, "uid": 4 }, { "mail_id": OTHER_MAIL, "uid": 9 }])),
                rows(json!([{ "next_uid": 41 }])),
                rows(json!([{ "mail_id": MAIL }, { "mail_id": OTHER_MAIL }])),
            ],
            expunge_mails_by_uid(USER, Some("Archive"), false, &[4, 9], None),
        )
        .await;
        assert_eq!(uids.unwrap(), vec![4, 9]);
        assert_eq!(trace[0], build_expunge_uids_select_query(USER, Some("Archive"), false, &[4, 9], None));

        let (uids, _) = with_script(vec![fault("08006")], expunge_mails_by_uid(USER, None, false, &[4], None)).await;
        assert_eq!(uids.unwrap_err().code.as_deref(), Some("08006"));
        let (uids, trace) =
            with_script(vec![rows(json!([]))], expunge_mails_by_uid(USER, Some("Archive"), false, &[4], None)).await;
        assert!(uids.unwrap().is_empty());
        assert_eq!(trace.len(), 1);
    }

    #[test]
    fn every_future_can_cross_threads() {
        fn sendable<T: Send>(_: T) {}
        let criteria: Vec<SimplifiedCriterion> = Vec::new();
        let ids: Vec<String> = Vec::new();
        // `get_mails_by_range` and `set_mail_flags` declare theirs.
        sendable(count_messages(USER, None, false, None));
        sendable(search_mails_by_uid(USER, None, false, &criteria, None));
        sendable(get_all_uids(USER, None, false, None));
        sendable(get_mail_flag_updates(USER, None, false, &ids, None));
        sendable(get_first_unseen_uid(USER, None, false, None));
        sendable(expunge_deleted_mails(USER, None, false, None));
        sendable(expunge_mails_by_uid(USER, None, false, &[], None));
        sendable(pg_text_chunks(MAIL, USER, TextColumn::Text, PG_TEXT_CHUNK_CHARS));
        sendable(pg_byte_chunks(MAIL, USER, TextColumn::Text, 1, PG_TEXT_CHUNK_BYTES));
    }
}
