//! Saving, reading and deleting mails (repositories/mails/core.ts).
//!
//! A save writes one `mails` row, the `mail_mailbox_uid` rows that file it
//! into its mailboxes, and the `Starred` / `Trash` pivots its flags imply. A
//! second delivery of a Message-ID the user already holds merges into the
//! row that exists instead of failing.
//!
//! ```ignore
//! let saved = save_mail(&SaveMailInput {
//!     user_id,
//!     message_id: "<id@example.com>".into(),
//!     uid_domain: Some(get_domain_uid_next(&user_id, false).await?),
//!     domain_mailbox: Some(INBOX_VIEW.into()),
//!     ..Default::default()
//! })
//! .await?;
//! ```

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Result};
use futures_util::future::join_all;
use serde_json::{json, Value};

use super::core_query::{build_mail_exists_query, build_mark_mail_spam_query};
use super::counters::{get_next_modseq, sync_mailbox_pivot, write_mailbox_uid};
use super::mapping_decisions::{decide_mapping_writes, MappingScope, MappingWrite, MappingWriteInput};
use super::model::{
    mails_table, MailModel, ENVELOPE_TO, MAIL_ID, MESSAGE_ID, MODSEQ, READ, RFC822_SIZE, SAVED, UPDATED, USER_ID,
};
use super::rfc822_size::{compute_full_message_size, MailForSize};
use super::table::{run_query, Filter, Record, SelectFilter, SetValue};
use crate::postgres::{DbError, DbResult, Param};
use crate::{js, logger};

pub use super::pivot_decisions::{decide_mapped_pivots, MappedPivotDecision};

/// Issues the pivot writes `decide_mapped_pivots` asks for, in order.
pub async fn sync_mapped_pivots_for_row(
    user_id: &str,
    mail_id: &str,
    saved: Option<bool>,
    deleted: Option<bool>,
    dest_mailbox: Option<&str>,
) -> DbResult<()> {
    for decision in decide_mapped_pivots(saved, deleted, dest_mailbox) {
        sync_mailbox_pivot(user_id, decision.mailbox, mail_id, decision.present).await?;
    }
    Ok(())
}

/// The flags a utility destination requires of the row filed into it, such
/// as `draft` for `Drafts`. A flag left `None` is not touched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Placement {
    pub draft: Option<bool>,
    pub is_spam: Option<bool>,
    pub saved: Option<bool>,
    pub deleted: Option<bool>,
}

impl Placement {
    /// The flags the placement sets, as `(column, value)`.
    fn entries(&self) -> Vec<(&'static str, bool)> {
        [("draft", self.draft), ("is_spam", self.is_spam), ("saved", self.saved), ("deleted", self.deleted)]
            .into_iter()
            .filter_map(|(column, value)| value.map(|value| (column, value)))
            .collect()
    }
}

/// What `save_mail` stores. Address fields, attachments and insight are the
/// JSON documents their columns hold.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SaveMailInput {
    pub user_id: String,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub reference_ids: Option<Vec<String>>,
    pub subject: Option<String>,
    /// An ISO timestamp; the time of the save when absent.
    pub date: Option<String>,
    pub html: Option<String>,
    pub text: Option<String>,
    pub from_address: Option<Value>,
    pub from_text: Option<String>,
    pub to_address: Option<Value>,
    pub to_text: Option<String>,
    pub cc_address: Option<Value>,
    pub cc_text: Option<String>,
    pub bcc_address: Option<Value>,
    pub bcc_text: Option<String>,
    pub reply_to_address: Option<Value>,
    pub reply_to_text: Option<String>,
    pub envelope_from: Option<Value>,
    pub envelope_to: Option<Value>,
    pub attachments: Option<Value>,
    pub read: bool,
    pub saved: bool,
    pub sent: bool,
    pub deleted: bool,
    pub draft: bool,
    pub answered: bool,
    pub expunged: bool,
    pub insight: Option<Value>,
    /// The UID reserved in the user's domain-wide sequence.
    pub uid_domain: Option<i64>,
    /// The UID reserved in `mailbox`'s own sequence.
    pub uid_mailbox: Option<i64>,
    pub spam_score: Option<f64>,
    pub spam_reasons: Option<Vec<String>>,
    pub is_spam: bool,
    pub placement: Option<Placement>,
    /// The mapped destination: a per-account view or a user-created box.
    pub mailbox: Option<String>,
    /// A domain view this mail also belongs to (`INBOX`), recorded in
    /// `mail_mailbox_uid` against `uid_domain`. Independent of `mailbox`,
    /// which carries `uid_mailbox`: a received mail belongs to its
    /// per-account view and to INBOX at once.
    pub domain_mailbox: Option<String>,
}

/// The row a save wrote or merged into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedMail {
    /// The row's `mail_id`.
    pub id: String,
    /// The UID the mapped destination holds for the mail, which is the
    /// earlier one when the mapping already existed.
    pub uid_mailbox: Option<i64>,
    /// The row's `uid_domain`: the caller's reservation for a new row, the
    /// surviving row's own for a merge.
    pub uid_domain: Option<i64>,
}

/// Whether JavaScript reads the value as true in a condition.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A JSONB parameter: the document as JSON text, NULL when there is none.
fn json_param(value: &Option<Value>) -> Param {
    match value {
        Some(value) if truthy(value) => Param::json(value),
        _ => Param::Null,
    }
}

// `mail_mailbox_uid` is UNIQUE on `(user_id, mailbox, uid)` while `mails`
// constrains only `(user_id, message_id)`, so two received rows can hold the
// same `uid_domain`: rows written before UID reservation became atomic still
// do, and the merge branch files them under their historical value. A domain
// view row is a scope marker whose UID is already authoritative in
// `mails.uid_domain`, so losing one costs a marker no read consults;
// aborting would cost the delivery it rides on. The mapped destination keeps
// aborting: it is the only UID source its box has.
async fn write_one_mapping<F, Fut>(
    write: &F,
    user_id: &str,
    mailbox: &str,
    mail_id: &str,
    uid: i64,
    scope: MappingScope,
) -> DbResult<Option<i64>>
where
    F: Fn(String, String, String, i64) -> Fut,
    Fut: Future<Output = DbResult<i64>>,
{
    match write(user_id.to_string(), mailbox.to_string(), mail_id.to_string(), uid).await {
        Ok(persisted) => Ok(Some(persisted)),
        Err(error) => {
            if scope != MappingScope::Domain || !error.is_unique_violation() {
                return Err(error);
            }
            logger::warn(
                "Skipping a domain view mapping row whose UID is already taken",
                json!({ "user_id": user_id, "mailbox": mailbox, "mail_id": mail_id, "uid": uid }),
            );
            Ok(None)
        }
    }
}

/// Issues the mapping writes for one row through `write` and returns the UID
/// persisted for the mapped destination. The rows are independent, so the
/// round trips overlap: every write is started and runs to its end, and the
/// first failure to arrive is the one reported.
///
/// `write` is a seam: this is the only place the decided rows become round
/// trips. It receives `(user_id, mailbox, mail_id, uid)`.
pub async fn record_mappings_with<F, Fut>(
    user_id: &str,
    mail_id: &str,
    writes: &[MappingWrite],
    mapped_destination: Option<&str>,
    write: F,
) -> DbResult<Option<i64>>
where
    F: Fn(String, String, String, i64) -> Fut,
    Fut: Future<Output = DbResult<i64>>,
{
    // The order the writes settle in, so the failure that arrived first is
    // the one reported.
    let settled = AtomicUsize::new(0);
    let write = &write;
    let settled = &settled;
    let outcomes = join_all(writes.iter().map(|decided| async move {
        let outcome = write_one_mapping(write, user_id, &decided.mailbox, mail_id, decided.uid, decided.scope).await;
        (settled.fetch_add(1, Ordering::Relaxed), decided.mailbox.as_str(), outcome)
    }))
    .await;

    let first_failure = outcomes
        .iter()
        .filter_map(|(at, _, outcome)| outcome.as_ref().err().map(|error| (*at, error)))
        .min_by_key(|(at, _)| *at);
    if let Some((_, error)) = first_failure {
        return Err(error.clone());
    }
    Ok(outcomes
        .iter()
        .find(|(_, mailbox, _)| Some(*mailbox) == mapped_destination)
        .and_then(|(_, _, outcome)| outcome.as_ref().ok().copied().flatten()))
}

/// Issues the mapping writes for one row and returns the UID persisted for
/// the mapped destination, which the caller reconciles onto the mail's
/// account UID.
pub async fn record_mappings(
    user_id: &str,
    mail_id: &str,
    writes: &[MappingWrite],
    mapped_destination: Option<&str>,
) -> DbResult<Option<i64>> {
    record_mappings_with(user_id, mail_id, writes, mapped_destination, |user_id, mailbox, mail_id, uid| async move {
        write_mailbox_uid(&user_id, &mailbox, &mail_id, uid).await
    })
    .await
}

/// The columns and values of the INSERT a save issues, in statement order.
fn insert_data(
    input: &SaveMailInput,
    mail_id: &str,
    modseq: i64,
    date: &str,
    rfc822_size: i64,
) -> Vec<(&'static str, Param)> {
    let text = |value: &Option<String>| Param::from(value.clone().unwrap_or_default());
    let mut data: Vec<(&'static str, Param)> = vec![
        ("mail_id", Param::from(mail_id)),
        ("user_id", Param::from(&input.user_id)),
        ("message_id", Param::from(&input.message_id)),
        ("subject", text(&input.subject)),
        ("date", Param::from(date)),
        ("html", text(&input.html)),
        ("text", text(&input.text)),
        // Populated at INSERT so the RFC822.SIZE fetch handler's cache-hit
        // branch fires from the first observation.
        (RFC822_SIZE, Param::from(rfc822_size)),
        ("from_address", json_param(&input.from_address)),
        ("from_text", Param::from(input.from_text.clone())),
        ("to_address", json_param(&input.to_address)),
        ("to_text", Param::from(input.to_text.clone())),
        ("cc_address", json_param(&input.cc_address)),
        ("cc_text", Param::from(input.cc_text.clone())),
        ("bcc_address", json_param(&input.bcc_address)),
        ("bcc_text", Param::from(input.bcc_text.clone())),
        ("reply_to_address", json_param(&input.reply_to_address)),
        ("reply_to_text", Param::from(input.reply_to_text.clone())),
        ("envelope_from", json_param(&input.envelope_from)),
        ("envelope_to", json_param(&input.envelope_to)),
        ("attachments", json_param(&input.attachments)),
        ("read", Param::from(input.read)),
        ("saved", Param::from(input.saved)),
        ("sent", Param::from(input.sent)),
        ("deleted", Param::from(input.deleted)),
        ("draft", Param::from(input.draft)),
        ("answered", Param::from(input.answered)),
        ("expunged", Param::from(input.expunged)),
        ("insight", json_param(&input.insight)),
        ("uid_domain", Param::from(input.uid_domain.unwrap_or(0))),
        ("modseq", Param::from(modseq)),
        ("spam_score", Param::from(input.spam_score.unwrap_or(0.0))),
        ("spam_reasons", input.spam_reasons.as_ref().map(Param::json).unwrap_or(Param::Null)),
        ("is_spam", Param::from(input.is_spam)),
        ("in_reply_to", Param::from(input.in_reply_to.clone())),
        (
            "reference_ids",
            input.reference_ids.as_ref().filter(|ids| !ids.is_empty()).map(Param::json).unwrap_or(Param::Null),
        ),
    ];
    // A placement overrides the flags it names, in place.
    for (column, value) in input.placement.map(|placement| placement.entries()).unwrap_or_default() {
        if let Some(entry) = data.iter_mut().find(|(name, _)| *name == column) {
            entry.1 = Param::from(value);
        }
    }
    data
}

fn column_text(record: &Record, column: &str) -> String {
    record.get(column).and_then(Value::as_str).unwrap_or_default().to_string()
}

async fn insert_mail(input: &SaveMailInput) -> Result<Option<SavedMail>> {
    let mail_id = uuid::Uuid::new_v4().to_string();
    // Stamp the new message with a fresh mod-sequence so it advances the
    // mailbox's HIGHESTMODSEQ (CONDSTORE, RFC 7162). Reserved before the
    // INSERT, like the UIDs the caller reserved.
    let modseq = get_next_modseq(&input.user_id).await?;
    let text = input.text.as_deref().unwrap_or("");
    let html = input.html.as_deref().unwrap_or("");
    let date = input.date.clone().unwrap_or_else(js::now_iso);
    // Sized from the body octet counts, the same measure every later FETCH
    // of the row uses. Attachments are on disk by the time a mail is saved.
    let rfc822_size = compute_full_message_size(&MailForSize {
        message_id: &input.message_id,
        date: &date,
        subject: input.subject.as_deref(),
        text_octets: text.len(),
        html_octets: html.len(),
        mail_id: &mail_id,
        attachments: input.attachments.as_ref(),
        from_text: input.from_text.as_deref(),
        to_text: input.to_text.as_deref(),
        cc_text: input.cc_text.as_deref(),
        bcc_text: input.bcc_text.as_deref(),
        reply_to_text: input.reply_to_text.as_deref(),
    })?;
    let data = insert_data(input, &mail_id, modseq, &date, rfc822_size);

    let Some(row) = mails_table().insert(&data, &[MAIL_ID]).await? else {
        return Ok(None);
    };
    let inserted_id = column_text(&row, MAIL_ID);
    let persisted_uid = record_mappings(
        &input.user_id,
        &inserted_id,
        &decide_mapping_writes(&MappingWriteInput {
            mailbox: input.mailbox.clone(),
            uid_mailbox: input.uid_mailbox,
            domain_mailbox: input.domain_mailbox.clone(),
            uid_domain: input.uid_domain,
            sent: input.sent,
        }),
        input.mailbox.as_deref(),
    )
    .await?;
    // Mapped-utility invariant sync. A fresh row has no prior pivot, so the
    // write is only issued when the flag is TRUE.
    let placement = input.placement.unwrap_or_default();
    let saved = placement.saved.unwrap_or(input.saved);
    let deleted = placement.deleted.unwrap_or(input.deleted);
    sync_mapped_pivots_for_row(
        &input.user_id,
        &inserted_id,
        saved.then_some(true),
        deleted.then_some(true),
        input.mailbox.as_deref(),
    )
    .await?;
    // The row just written holds the caller's `uid_domain`; it is returned
    // as given so the caller can report it for a domain-scoped COPY/APPEND.
    Ok(Some(SavedMail { id: inserted_id, uid_mailbox: persisted_uid, uid_domain: input.uid_domain }))
}

/// How an address entry is told apart when envelope recipients are merged:
/// by its `address`, compared as JavaScript compares set members.
#[derive(Debug, Clone, PartialEq)]
enum AddressKey {
    Missing,
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    /// An object or array is equal to nothing but itself.
    Unmatched,
}

fn address_key(entry: &Value) -> Result<AddressKey> {
    if entry.is_null() {
        bail!("an envelope recipient entry is null");
    }
    Ok(match entry.get("address") {
        None => AddressKey::Missing,
        Some(Value::Null) => AddressKey::Null,
        Some(Value::Bool(flag)) => AddressKey::Bool(*flag),
        Some(Value::Number(number)) => AddressKey::Number(number.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(text)) => AddressKey::Text(text.clone()),
        Some(Value::Array(_) | Value::Object(_)) => AddressKey::Unmatched,
    })
}

/// The stored envelope recipients followed by the incoming ones whose
/// address is not stored yet.
fn merge_envelope_to(existing: Option<&Value>, incoming: &Value) -> Result<Value> {
    let existing_to: &[Value] = match existing {
        None | Some(Value::Null) => &[],
        Some(Value::Array(entries)) => entries,
        Some(_) => bail!("the stored envelope_to is not a list"),
    };
    let Value::Array(incoming_to) = incoming else {
        bail!("the incoming envelope_to is not a list");
    };
    let seen: Vec<AddressKey> = existing_to.iter().map(address_key).collect::<Result<_>>()?;
    let mut merged: Vec<Value> = existing_to.to_vec();
    for entry in incoming_to {
        let key = address_key(entry)?;
        if key == AddressKey::Unmatched || !seen.contains(&key) {
            merged.push(entry.clone());
        }
    }
    Ok(Value::Array(merged))
}

/// The unique `(user_id, message_id)` refused the INSERT: the user already
/// holds this Message-ID. That happens legitimately when one mail is
/// delivered to several of the user's accounts under separate envelopes, so
/// the delivery is merged into the row that exists.
async fn merge_into_existing(input: &SaveMailInput) -> Result<Option<SavedMail>> {
    let Some(existing) = get_mail_by_message_id(&input.user_id, &input.message_id).await? else {
        return Ok(None);
    };
    let same_message = [(USER_ID, Filter::from(&input.user_id)), (MESSAGE_ID, Filter::from(&input.message_id))];

    // Merge the envelope recipients, so BCC recipients can be told later.
    if let Some(incoming) = input.envelope_to.as_ref().filter(|value| truthy(value)) {
        let merged = merge_envelope_to(existing.envelope_to.as_ref(), incoming)?;
        mails_table()
            .update_where(
                &same_message,
                &[(ENVELOPE_TO, SetValue::Value(Param::json(&merged))), (UPDATED, SetValue::DbNow)],
                &[],
            )
            .await?;
    }

    // A mapped destination gets its membership from the mapping row below; a
    // utility destination gets it from the flag, and `existing` predates this
    // write, so the flag has to be set here too. The flip is a membership
    // change, so it advances the mod-sequence.
    if let Some(placement) = &input.placement {
        let mut data: Vec<(&str, SetValue)> =
            placement.entries().into_iter().map(|(column, value)| (column, SetValue::from(value))).collect();
        data.push((MODSEQ, SetValue::from(get_next_modseq(&input.user_id).await?)));
        data.push((UPDATED, SetValue::DbNow));
        mails_table().update_where(&same_message, &data, &[]).await?;
    }

    // The domain view describes the SURVIVING row, so its `uid_domain` and
    // `sent` come off `existing`: the caller's reservation belongs to an
    // INSERT that never happened, and its `sent` describes a different
    // delivery of the same Message-ID. The mapped destination is the
    // caller's own.
    let persisted_uid = record_mappings(
        &input.user_id,
        &existing.mail_id,
        &decide_mapping_writes(&MappingWriteInput {
            mailbox: input.mailbox.clone(),
            uid_mailbox: input.uid_mailbox,
            domain_mailbox: input.domain_mailbox.clone(),
            uid_domain: Some(existing.uid_domain),
            sent: existing.sent,
        }),
        input.mailbox.as_deref(),
    )
    .await?;
    // Mirror the placement flip into the mapped-utility pivots. A flag the
    // placement didn't touch is skipped: the invariant already held.
    sync_mapped_pivots_for_row(
        &input.user_id,
        &existing.mail_id,
        input.placement.and_then(|placement| placement.saved),
        input.placement.and_then(|placement| placement.deleted),
        input.mailbox.as_deref(),
    )
    .await?;
    // The caller's `uid_domain` is a fresh reservation no row carries; the
    // surviving row's own is what a COPYUID / APPENDUID has to report.
    Ok(Some(SavedMail {
        id: existing.mail_id,
        uid_mailbox: persisted_uid,
        uid_domain: Some(existing.uid_domain),
    }))
}

fn is_unique_violation(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DbError>().is_some_and(DbError::is_unique_violation)
}

/// Takes the NUL characters out of a text in place.
fn strip_nul(text: &mut String) {
    if text.contains('\0') {
        text.retain(|c| c != '\0');
    }
}

/// Takes the NUL characters out of every string in a JSON document, at any
/// depth. Object keys are left as they are.
fn strip_nul_json(value: &mut Value) {
    match value {
        Value::String(text) => strip_nul(text),
        Value::Array(items) => items.iter_mut().for_each(strip_nul_json),
        Value::Object(entries) => entries.values_mut().for_each(strip_nul_json),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// `input` with the NUL characters taken out of every text and JSON document
/// it holds. PostgreSQL text cannot hold one and JSONB refuses `\u0000`, so a
/// single NUL anywhere in a message fails its INSERT, and a sender retrying
/// the delivery meets the same failure every time. The character carries
/// nothing a reader would miss.
pub fn without_nul(input: &SaveMailInput) -> SaveMailInput {
    let mut input = input.clone();
    for text in [&mut input.user_id, &mut input.message_id] {
        strip_nul(text);
    }
    let optional_texts = [
        &mut input.in_reply_to,
        &mut input.subject,
        &mut input.date,
        &mut input.html,
        &mut input.text,
        &mut input.from_text,
        &mut input.to_text,
        &mut input.cc_text,
        &mut input.bcc_text,
        &mut input.reply_to_text,
        &mut input.mailbox,
        &mut input.domain_mailbox,
    ];
    optional_texts.into_iter().flatten().for_each(strip_nul);
    for list in [&mut input.reference_ids, &mut input.spam_reasons].into_iter().flatten() {
        list.iter_mut().for_each(strip_nul);
    }
    let documents = [
        &mut input.from_address,
        &mut input.to_address,
        &mut input.cc_address,
        &mut input.bcc_address,
        &mut input.reply_to_address,
        &mut input.envelope_from,
        &mut input.envelope_to,
        &mut input.attachments,
        &mut input.insight,
    ];
    documents.into_iter().flatten().for_each(strip_nul_json);
    input
}

/// Stores a mail and files it into its mailboxes. `None` when the INSERT
/// returned no row, or when the Message-ID was refused as a duplicate and the
/// row that holds it could not be read back.
///
/// A unique violation anywhere in the save, not only on the `mails` INSERT,
/// takes the merge path; a violation raised while merging is an error. Any
/// other fault is logged and returned: a save never reports success for a
/// row it did not write.
pub async fn save_mail(input: &SaveMailInput) -> Result<Option<SavedMail>> {
    // Every mail row is written here, whichever protocol it came in by.
    let input = &without_nul(input);
    match insert_mail(input).await {
        Ok(saved) => Ok(saved),
        Err(error) if is_unique_violation(&error) => merge_into_existing(input).await,
        Err(error) => {
            logger::error_with("Failed to save mail", json!({}), &error);
            Err(error)
        }
    }
}

/// The user's mail with this Message-ID, the row a duplicate delivery
/// conflicts with.
pub async fn get_mail_by_message_id(user_id: &str, message_id: &str) -> Result<Option<MailModel>> {
    let rows = mails_table().query(&[(USER_ID, SelectFilter::from(user_id)), (MESSAGE_ID, SelectFilter::from(message_id))]).await?;
    let models = rows.iter().map(MailModel::from_record).collect::<Result<Vec<_>, _>>()?;
    Ok(models.into_iter().next())
}

/// The user's mail with this id. A database fault is an error, never "not
/// found".
pub async fn get_mail_by_id(user_id: &str, mail_id: &str) -> Result<Option<MailModel>> {
    let row = mails_table().query_one(&[(MAIL_ID, SelectFilter::from(mail_id)), (USER_ID, SelectFilter::from(user_id))], None).await?;
    Ok(row.as_ref().map(MailModel::from_record).transpose()?)
}

/// Persists the derived `rfc822_size` for a mail on the first FETCH that
/// computes it. Deliberately does not bump `updated`: this is metadata
/// derivation, not a semantic edit, and touching `updated` would wake every
/// delta-sync client on a first observation.
///
/// Idempotent and unconditional: a mail's content is immutable after insert,
/// so concurrent callers write the same value.
pub async fn update_rfc822_size(user_id: &str, mail_id: &str, rfc822_size: i64) -> DbResult<()> {
    mails_table()
        .update_where(
            &[(MAIL_ID, Filter::from(mail_id)), (USER_ID, Filter::from(user_id))],
            &[(RFC822_SIZE, SetValue::from(rfc822_size))],
            &[],
        )
        .await?;
    Ok(())
}

fn flag(record: &Record, column: &str) -> bool {
    record.get(column).is_some_and(truthy)
}

/// Marks a mail read. `false` only when the user has no such mail; marking a
/// mail that is already read is a success that reserves no mod-sequence.
pub async fn mark_mail_read(user_id: &str, mail_id: &str) -> DbResult<bool> {
    let table = mails_table();
    let one_mail = [(MAIL_ID, SelectFilter::from(mail_id)), (USER_ID, SelectFilter::from(user_id))];
    // Probe the current flag before reserving a mod-sequence: the reservation
    // takes the per-user counter lock and burns a value, so a re-mark must
    // not reserve. The probe projects only the flag; the full row would
    // detoast the body columns for a boolean.
    let Some(current) = table.query_one(&one_mail, Some(&[READ])).await? else {
        return Ok(false);
    };
    if flag(&current, READ) {
        return Ok(true);
    }
    // The update still carries the `read = false` filter: the flag can flip
    // between probe and update, and the flip can only land as a zero-row
    // match, never as a write to a row that already holds the target value.
    let modseq = get_next_modseq(user_id).await?;
    let rows = table
        .update_where(
            &[(MAIL_ID, Filter::from(mail_id)), (USER_ID, Filter::from(user_id)), (READ, Filter::from(false))],
            &[(READ, SetValue::from(true)), (UPDATED, SetValue::DbNow), (MODSEQ, SetValue::from(modseq))],
            &[MAIL_ID],
        )
        .await?;
    if !rows.is_empty() {
        return Ok(true);
    }
    // The update matched no row after the probe said unread: the flag flipped
    // between the two calls. A re-mark of an existing mail stays a success,
    // only a missing mail fails.
    Ok(table.query_one(&one_mail, Some(&[READ])).await?.is_some())
}

/// Sets or clears a mail's saved flag and keeps its `Starred` pivot in step.
/// `false` only when the user has no such mail.
pub async fn mark_mail_saved(user_id: &str, mail_id: &str, saved: bool) -> DbResult<bool> {
    let table = mails_table();
    let one_mail = [(MAIL_ID, SelectFilter::from(mail_id)), (USER_ID, SelectFilter::from(user_id))];
    // Same probe-before-reservation shape as `mark_mail_read`: a re-save of a
    // mail that already holds the requested value reserves nothing and still
    // runs the pivot sync for the existing mail.
    let Some(current) = table.query_one(&one_mail, Some(&[SAVED])).await? else {
        return Ok(false);
    };
    if current.get(SAVED) == Some(&Value::Bool(saved)) {
        sync_mailbox_pivot(user_id, "Starred", mail_id, saved).await?;
        return Ok(true);
    }
    let modseq = get_next_modseq(user_id).await?;
    let rows = table
        .update_where(
            &[(MAIL_ID, Filter::from(mail_id)), (USER_ID, Filter::from(user_id)), (SAVED, Filter::from(!saved))],
            &[(SAVED, SetValue::from(saved)), (UPDATED, SetValue::DbNow), (MODSEQ, SetValue::from(modseq))],
            &[MAIL_ID],
        )
        .await?;
    if !rows.is_empty() {
        sync_mailbox_pivot(user_id, "Starred", mail_id, saved).await?;
        return Ok(true);
    }
    // Zero-row match after the probe said the flag differed: the flag flipped
    // between the two calls. An existing mail keeps the pivot sync and stays
    // a success, a missing mail fails.
    if table.query_one(&one_mail, Some(&[SAVED])).await?.is_none() {
        return Ok(false);
    }
    sync_mailbox_pivot(user_id, "Starred", mail_id, saved).await?;
    Ok(true)
}

/// Deletes the user's mail outright. Returns whether there was one.
pub async fn delete_mail(user_id: &str, mail_id: &str) -> DbResult<bool> {
    let count =
        mails_table().delete_where(&[(MAIL_ID, Filter::from(mail_id)), (USER_ID, Filter::from(user_id))]).await?;
    Ok(count > 0)
}

/// What a spam mark did: whether the user has the mail, and whether its flag
/// changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkMailSpamResult {
    pub found: bool,
    pub changed: bool,
}

/// Marks or unmarks a mail as spam. A mod-sequence is reserved up front and
/// stamped only when the flag changes.
pub async fn mark_mail_spam(user_id: &str, mail_id: &str, is_spam: bool) -> DbResult<MarkMailSpamResult> {
    let update = build_mark_mail_spam_query(user_id, mail_id, is_spam, get_next_modseq(user_id).await?);
    let result = run_query(&update).await?;
    if result.row_count.unwrap_or(0) > 0 {
        return Ok(MarkMailSpamResult { found: true, changed: true });
    }
    let exists = run_query(&build_mail_exists_query(user_id, mail_id)).await?;
    Ok(MarkMailSpamResult { found: exists.row_count.unwrap_or(0) > 0, changed: false })
}

#[cfg(test)]
mod tests {
    use super::super::table::script::{affected, fault, rows, with_script};
    use super::super::table::Rows;
    use super::*;

    const USER: &str = "11111111-1111-1111-1111-111111111111";
    const MAIL: &str = "22222222-2222-2222-2222-222222222222";
    const OTHER_MAIL: &str = "33333333-3333-3333-3333-333333333333";
    const INBOX: &str = "INBOX";
    const ACCOUNT_BOX: &str = "INBOX/accounts/admin";

    const UNIQUE_VIOLATION: &str = "23505";
    const ADMIN_SHUTDOWN: &str = "57P01";

    fn none() -> DbResult<Rows> {
        rows(json!([]))
    }

    fn reserved(value: i64) -> DbResult<Rows> {
        rows(json!([{ "next_uid": value }]))
    }

    fn sql(trace: &[super::super::table::PreparedQuery]) -> Vec<&str> {
        trace.iter().map(|query| query.sql.as_str()).collect()
    }

    fn duplicate() -> DbError {
        DbError { code: Some(UNIQUE_VIOLATION.to_string()), ..DbError::new("duplicate key value") }
    }

    fn inbox_tree_writes() -> Vec<MappingWrite> {
        decide_mapping_writes(&MappingWriteInput {
            mailbox: Some(ACCOUNT_BOX.into()),
            uid_mailbox: Some(7),
            domain_mailbox: Some(INBOX.into()),
            uid_domain: Some(42),
            sent: false,
        })
    }

    // mapping-decisions.test.ts — "recordMappings — the consumer that turns
    // the decisions into rows"

    #[tokio::test]
    async fn record_mappings_writes_every_decided_row_at_its_own_uid_and_reports_the_mapped_one() {
        let calls = std::sync::Mutex::new(Vec::new());
        let persisted = record_mappings_with("user", "mail", &inbox_tree_writes(), Some(ACCOUNT_BOX), |_, mailbox, _, uid| {
            calls.lock().unwrap().push((mailbox, uid));
            async move { Ok(uid) }
        })
        .await;
        assert_eq!(*calls.lock().unwrap(), vec![(ACCOUNT_BOX.to_string(), 7), (INBOX.to_string(), 42)]);
        assert_eq!(persisted.unwrap(), Some(7));
    }

    #[tokio::test]
    async fn record_mappings_reports_the_uid_the_mapping_holds_not_the_one_reserved() {
        let writes = decide_mapping_writes(&MappingWriteInput {
            mailbox: Some(ACCOUNT_BOX.into()),
            uid_mailbox: Some(7),
            ..Default::default()
        });
        let persisted = record_mappings_with("user", "mail", &writes, Some(ACCOUNT_BOX), |_, _, _, _| async { Ok(3) }).await;
        assert_eq!(persisted.unwrap(), Some(3));
    }

    #[tokio::test]
    async fn record_mappings_drops_a_domain_view_row_whose_uid_is_taken_rather_than_the_mail() {
        let persisted =
            record_mappings_with("user", "mail", &inbox_tree_writes(), Some(ACCOUNT_BOX), |_, mailbox, _, _| async move {
                if mailbox == INBOX {
                    Err(duplicate())
                } else {
                    Ok(7)
                }
            })
            .await;
        assert_eq!(persisted.unwrap(), Some(7));
    }

    #[tokio::test]
    async fn record_mappings_still_aborts_when_the_mapped_destinations_uid_is_taken() {
        let persisted =
            record_mappings_with("user", "mail", &inbox_tree_writes(), Some(ACCOUNT_BOX), |_, mailbox, _, _| async move {
                if mailbox == ACCOUNT_BOX {
                    Err(duplicate())
                } else {
                    Ok(42)
                }
            })
            .await;
        assert_eq!(persisted.unwrap_err().message, "duplicate key value");
    }

    #[tokio::test]
    async fn record_mappings_still_aborts_on_a_domain_view_row_for_any_other_fault() {
        let persisted =
            record_mappings_with("user", "mail", &inbox_tree_writes(), Some(ACCOUNT_BOX), |_, mailbox, _, _| async move {
                if mailbox == INBOX {
                    Err(DbError { code: Some(ADMIN_SHUTDOWN.to_string()), ..DbError::new("connection terminated") })
                } else {
                    Ok(7)
                }
            })
            .await;
        assert_eq!(persisted.unwrap_err().message, "connection terminated");
    }

    #[tokio::test]
    async fn record_mappings_lets_every_write_finish_and_reports_the_first_failure_to_arrive() {
        // The mapped write fails late, the domain write fails first with a
        // fault that is not a duplicate: the earlier one is what the caller
        // sees, and neither write is abandoned half way.
        let finished = std::sync::Mutex::new(Vec::new());
        let finished = &finished;
        let persisted =
            record_mappings_with("user", "mail", &inbox_tree_writes(), Some(ACCOUNT_BOX), |_, mailbox, _, _| async move {
                if mailbox == ACCOUNT_BOX {
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                }
                finished.lock().unwrap().push(mailbox.clone());
                Err::<i64, _>(DbError::new(format!("failed {mailbox}")))
            })
            .await;
        assert_eq!(persisted.unwrap_err().message, "failed INBOX");
        assert_eq!(*finished.lock().unwrap(), vec![INBOX.to_string(), ACCOUNT_BOX.to_string()]);
    }

    // mapping-decisions.test.ts — "saveMail forwards the caller's domain view
    // on both branches"

    fn mail_row(overrides: Value) -> Value {
        let mut row = json!({
            "mail_id": OTHER_MAIL, "user_id": USER, "message_id": "<dup@example.com>", "subject": "", "date": "2024-01-02T03:04:05.000Z",
            "html": "", "text": "", "from_address": null, "from_text": null, "to_address": null, "to_text": null,
            "cc_address": null, "cc_text": null, "bcc_address": null, "bcc_text": null, "reply_to_address": null,
            "reply_to_text": null, "envelope_from": null, "envelope_to": null, "attachments": null, "read": false,
            "saved": false, "sent": false, "deleted": false, "draft": false, "answered": false, "expunged": false,
            "insight": null, "uid_domain": 5, "modseq": 2, "spam_score": 0, "spam_reasons": null, "is_spam": false,
            "rfc822_size": null, "text_line_count": null, "html_line_count": null, "in_reply_to": null,
            "reference_ids": null, "updated": "2024-01-02T03:04:05.000Z", "search_vector": null,
        });
        for (key, value) in overrides.as_object().unwrap() {
            row[key] = value.clone();
        }
        row
    }

    fn received(message_id: &str) -> SaveMailInput {
        SaveMailInput {
            user_id: USER.into(),
            message_id: message_id.into(),
            uid_domain: Some(9),
            domain_mailbox: Some(INBOX.into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn save_mail_forwards_the_callers_domain_view_on_both_branches() {
        let inserted = vec![reserved(31), rows(json!([{ "mail_id": MAIL }])), rows(json!([{ "uid": 9 }]))];
        let (saved, trace) = with_script(inserted, save_mail(&received("<new@example.com>"))).await;
        assert_eq!(saved.unwrap().unwrap().uid_domain, Some(9));
        assert_eq!(trace[2].values, crate::params![USER, INBOX, MAIL, 9]);

        // On the merge branch the view is filed at the surviving row's UID.
        let merged = vec![reserved(31), fault(UNIQUE_VIOLATION), rows(json!([mail_row(json!({}))])), rows(json!([{ "uid": 5 }]))];
        let (saved, trace) = with_script(merged, save_mail(&received("<dup@example.com>"))).await;
        assert_eq!(saved.unwrap().unwrap().uid_domain, Some(5));
        assert_eq!(trace[3].values, crate::params![USER, INBOX, OTHER_MAIL, 5]);
    }

    // placement.test.ts — "saveMail — utility placement on the INSERT branch"

    #[tokio::test]
    async fn save_mail_carries_the_placement_into_the_insert() {
        let input = SaveMailInput { placement: Some(Placement { draft: Some(true), ..Default::default() }), ..received("<draft@example.com>") };
        let script = vec![reserved(31), rows(json!([{ "mail_id": MAIL }])), rows(json!([{ "uid": 9 }]))];
        let (_, trace) = with_script(script, save_mail(&input)).await;
        let insert = &trace[1];
        let columns: Vec<&str> = insert.sql["INSERT INTO mails (".len()..insert.sql.find(") VALUES").unwrap()].split(", ").collect();
        // `updated` is stamped by the statement and binds no value.
        let draft = columns.iter().position(|column| *column == "draft").unwrap() - 1;
        assert_eq!(insert.values[draft], Param::from(true));
    }

    #[tokio::test]
    async fn save_mail_inserts_the_row_and_returns_its_id() {
        let script = vec![reserved(31), rows(json!([{ "mail_id": MAIL }]))];
        let input = SaveMailInput { user_id: USER.into(), message_id: "<m@example.com>".into(), ..Default::default() };
        let (saved, trace) = with_script(script, save_mail(&input)).await;
        assert!(trace[1].sql.starts_with("INSERT INTO mails (updated, mail_id, user_id, message_id,"));
        assert!(trace[1].sql.ends_with(" RETURNING mail_id"));
        assert_eq!(saved.unwrap(), Some(SavedMail { id: MAIL.into(), uid_mailbox: None, uid_domain: None }));
    }

    // placement.test.ts — "saveMail — utility placement on the 23505 merge branch"

    #[tokio::test]
    async fn merge_issues_no_membership_update_without_a_placement() {
        let script = vec![reserved(31), fault(UNIQUE_VIOLATION), rows(json!([mail_row(json!({}))])), rows(json!([{ "uid": 5 }]))];
        let (_, trace) = with_script(script, save_mail(&received("<dup@example.com>"))).await;
        // No UPDATE, and so no second mod-sequence reservation either.
        assert!(sql(&trace).iter().all(|statement| !statement.starts_with("UPDATE")));
        assert_eq!(sql(&trace).iter().filter(|statement| statement.contains("INSERT INTO mail_uid_counters")).count(), 1);
    }

    #[tokio::test]
    async fn merge_applies_the_placement_with_a_fresh_mod_sequence() {
        let input = SaveMailInput { placement: Some(Placement { is_spam: Some(true), ..Default::default() }), ..received("<dup@example.com>") };
        let script = vec![
            reserved(31),
            fault(UNIQUE_VIOLATION),
            rows(json!([mail_row(json!({}))])),
            reserved(32),
            affected(1),
            rows(json!([{ "uid": 5 }])),
        ];
        let (_, trace) = with_script(script, save_mail(&input)).await;
        assert_eq!(
            trace[4].sql,
            "UPDATE mails SET is_spam = $1, modseq = $2, updated = CURRENT_TIMESTAMP WHERE user_id = $3 AND message_id = $4"
        );
        assert_eq!(trace[4].values, crate::params![true, 32, USER, "<dup@example.com>"]);
    }

    #[tokio::test]
    async fn merge_rereads_the_existing_row_after_the_unique_violation() {
        let script = vec![reserved(31), fault(UNIQUE_VIOLATION), rows(json!([mail_row(json!({}))])), rows(json!([{ "uid": 5 }]))];
        let (saved, trace) = with_script(script, save_mail(&received("<dup@example.com>"))).await;
        assert_eq!(trace[2].sql, "SELECT * FROM mails WHERE user_id = $1 AND message_id = $2");
        assert_eq!(trace[2].values, crate::params![USER, "<dup@example.com>"]);
        // The caller gets the id of the row that was there first.
        assert_eq!(saved.unwrap().unwrap().id, OTHER_MAIL);
    }

    // core.test.ts — "withoutNul"

    #[test]
    fn without_nul_takes_the_character_out_of_every_text_and_document() {
        let input = SaveMailInput {
            subject: Some("nu\0l".into()),
            html: Some("<p>a\0b</p>".into()),
            text: Some("a\0b\0".into()),
            from_address: Some(json!([{ "address": "jo@remote.example", "name": "Jo\0e" }])),
            from_text: Some("Jo\0e <jo@remote.example>".into()),
            attachments: Some(json!([{ "filename": "re\0port.pdf", "contentType": "application/pdf", "content": { "data": "id" }, "size": 1 }])),
            reference_ids: Some(vec!["<r\0@remote.example>".into()]),
            insight: Some(json!({ "a\0": ["x\0", 1, null, true] })),
            read: true,
            spam_score: Some(1.5),
            ..received("<id\0@example.com>")
        };
        let stored = without_nul(&input);
        assert_eq!(
            stored,
            SaveMailInput {
                message_id: "<id@example.com>".into(),
                subject: Some("nul".into()),
                html: Some("<p>ab</p>".into()),
                text: Some("ab".into()),
                from_address: Some(json!([{ "address": "jo@remote.example", "name": "Joe" }])),
                from_text: Some("Joe <jo@remote.example>".into()),
                attachments: Some(json!([{ "filename": "report.pdf", "contentType": "application/pdf", "content": { "data": "id" }, "size": 1 }])),
                reference_ids: Some(vec!["<r@remote.example>".into()]),
                // Keys are ours, not the message's, and are left alone.
                insight: Some(json!({ "a\0": ["x", 1, null, true] })),
                ..input.clone()
            }
        );
        // Nothing else changes.
        assert_eq!(without_nul(&stored), stored);
    }

    #[tokio::test]
    async fn save_mail_binds_no_nul_character_and_merges_under_the_id_without_it() {
        let input = SaveMailInput {
            subject: Some("nu\0l".into()),
            text: Some("a\0b".into()),
            from_address: Some(json!([{ "address": "jo@remote.example", "name": "Jo\0e" }])),
            envelope_to: Some(json!([{ "address": "admin@example.com" }])),
            ..received("<id\0@example.com>")
        };
        let script = vec![
            reserved(31),
            fault(UNIQUE_VIOLATION),
            rows(json!([mail_row(json!({}))])),
            affected(1),
            rows(json!([{ "uid": 5 }])),
        ];
        let (saved, trace) = with_script(script, save_mail(&input)).await;
        assert_eq!(saved.unwrap().unwrap().id, OTHER_MAIL);
        for value in trace.iter().flat_map(|query| &query.values) {
            if let Param::Text(text) = value {
                assert!(!text.contains('\0') && !text.contains("\\u0000"), "{text:?}");
            }
        }
        assert_eq!(trace[2].values, crate::params![USER, "<id@example.com>"]);
    }

    // placement.test.ts — "saveMail — 23505 code narrows the merge branch"

    #[tokio::test]
    async fn only_a_unique_violation_takes_the_merge_path() {
        let (saved, trace) = with_script(vec![reserved(31), fault(ADMIN_SHUTDOWN)], save_mail(&received("<m@example.com>"))).await;
        let error = saved.unwrap_err();
        assert_eq!(error.downcast_ref::<DbError>().and_then(|error| error.code.as_deref()), Some(ADMIN_SHUTDOWN));
        assert_eq!(trace.len(), 2);
    }

    // core.test.ts — "mark-mail mutations — mod-sequence reservation"

    #[tokio::test]
    async fn mark_mail_spam_stamps_a_freshly_reserved_mod_sequence_rather_than_a_constant() {
        let script = vec![reserved(4711), rows(json!([{ "mail_id": MAIL }]))];
        let (_, trace) = with_script(script, mark_mail_spam(USER, MAIL, true)).await;
        assert!(trace[0].sql.contains("INSERT INTO mail_uid_counters"));
        assert!(trace[1].sql.contains("modseq = $4"));
        assert_eq!(trace[1].values[3], Param::from(4711));
    }

    #[tokio::test]
    async fn mark_mail_read_stamps_a_freshly_reserved_mod_sequence_rather_than_a_constant() {
        let script = vec![rows(json!([{ "read": false }])), reserved(4711), rows(json!([{ "mail_id": MAIL }]))];
        let (marked, trace) = with_script(script, mark_mail_read(USER, MAIL)).await;
        assert!(marked.unwrap());
        assert!(trace[2].sql.contains("modseq = $2"));
        assert_eq!(trace[2].values[1], Param::from(4711));
    }

    #[tokio::test]
    async fn mark_mail_saved_stamps_a_freshly_reserved_mod_sequence_rather_than_a_constant() {
        let script = vec![rows(json!([{ "saved": true }])), reserved(4711), rows(json!([{ "mail_id": MAIL }])), affected(1)];
        let (marked, trace) = with_script(script, mark_mail_saved(USER, MAIL, false)).await;
        assert!(marked.unwrap());
        assert!(trace[2].sql.contains("modseq = $2"));
        assert_eq!(trace[2].values[1], Param::from(4711));
    }

    #[tokio::test]
    async fn mark_mail_read_guards_the_update_to_rows_that_do_not_already_hold_the_target_flag() {
        let script = vec![rows(json!([{ "read": false }])), reserved(8), rows(json!([{ "mail_id": MAIL }]))];
        let (_, trace) = with_script(script, mark_mail_read(USER, MAIL)).await;
        assert!(trace[2].sql.ends_with("WHERE mail_id = $3 AND user_id = $4 AND read = $5 RETURNING mail_id"));
        assert_eq!(trace[2].values[2..], crate::params![MAIL, USER, false]);
    }

    #[tokio::test]
    async fn mark_mail_saved_guards_the_update_to_rows_that_do_not_already_hold_the_target_flag() {
        let script =
            vec![rows(json!([{ "saved": false }])), reserved(8), rows(json!([{ "mail_id": MAIL }])), reserved(1), rows(json!([{ "uid": 1 }]))];
        let (_, trace) = with_script(script, mark_mail_saved(USER, MAIL, true)).await;
        assert!(trace[2].sql.ends_with("WHERE mail_id = $3 AND user_id = $4 AND saved = $5 RETURNING mail_id"));
        // The guard is the opposite of the value being written.
        assert_eq!(trace[2].values, crate::params![true, 8, MAIL, USER, false]);
    }

    #[tokio::test]
    async fn mark_mail_read_probes_the_current_flag_before_reserving_a_mod_sequence() {
        // The no-op path reserves nothing: one statement, the probe.
        let (marked, trace) = with_script(vec![rows(json!([{ "read": true }]))], mark_mail_read(USER, MAIL)).await;
        assert!(marked.unwrap());
        assert_eq!(trace.len(), 1);

        // The changing path probes first, then reserves, then writes.
        let script = vec![rows(json!([{ "read": false }])), reserved(8), rows(json!([{ "mail_id": MAIL }]))];
        let (_, trace) = with_script(script, mark_mail_read(USER, MAIL)).await;
        assert!(trace[0].sql.starts_with("SELECT"));
        assert!(trace[1].sql.contains("INSERT INTO mail_uid_counters"));
        assert!(trace[2].sql.starts_with("UPDATE mails"));
    }

    #[tokio::test]
    async fn mark_mail_saved_probes_the_current_flag_before_reserving_a_mod_sequence() {
        // The no-op path reserves no mod-sequence; it still syncs the pivot.
        let (marked, trace) =
            with_script(vec![rows(json!([{ "saved": false }])), affected(0)], mark_mail_saved(USER, MAIL, false)).await;
        assert!(marked.unwrap());
        assert!(trace[0].sql.starts_with("SELECT"));
        assert!(trace[1].sql.starts_with("DELETE FROM mail_mailbox_uid"));
        assert_eq!(trace.len(), 2);

        let script = vec![rows(json!([{ "saved": true }])), reserved(8), rows(json!([{ "mail_id": MAIL }])), affected(1)];
        let (_, trace) = with_script(script, mark_mail_saved(USER, MAIL, false)).await;
        assert!(trace[0].sql.starts_with("SELECT"));
        assert!(trace[1].sql.contains("INSERT INTO mail_uid_counters"));
        assert!(trace[2].sql.starts_with("UPDATE mails"));
    }

    #[tokio::test]
    async fn mark_mail_read_projects_only_the_flag_in_the_probe_never_the_full_row() {
        // Both the probe and the re-probe after a lost race.
        let script = vec![rows(json!([{ "read": false }])), reserved(8), none(), rows(json!([{ "read": true }]))];
        let (marked, trace) = with_script(script, mark_mail_read(USER, MAIL)).await;
        assert!(marked.unwrap());
        let probe = "SELECT read FROM mails WHERE mail_id = $1 AND user_id = $2 LIMIT $3";
        assert_eq!(trace[0].sql, probe);
        assert_eq!(trace[3].sql, probe);
    }

    #[tokio::test]
    async fn mark_mail_saved_projects_only_the_flag_in_the_probe_never_the_full_row() {
        let script = vec![rows(json!([{ "saved": true }])), reserved(8), none(), rows(json!([{ "saved": false }])), affected(0)];
        let (marked, trace) = with_script(script, mark_mail_saved(USER, MAIL, false)).await;
        assert!(marked.unwrap());
        let probe = "SELECT saved FROM mails WHERE mail_id = $1 AND user_id = $2 LIMIT $3";
        assert_eq!(trace[0].sql, probe);
        assert_eq!(trace[3].sql, probe);
    }

    // error-propagation.test.ts — "core.ts does not report a DB fault as
    // not-found"

    #[tokio::test]
    async fn get_mail_by_id_swallows_no_error() {
        let (found, _) = with_script(vec![fault(ADMIN_SHUTDOWN)], get_mail_by_id(USER, MAIL)).await;
        assert!(found.is_err());
    }

    #[tokio::test]
    async fn mark_mail_read_swallows_no_error() {
        let (marked, _) = with_script(vec![fault(ADMIN_SHUTDOWN)], mark_mail_read(USER, MAIL)).await;
        assert!(marked.is_err());
        let script = vec![rows(json!([{ "read": false }])), reserved(8), fault(ADMIN_SHUTDOWN)];
        let (marked, _) = with_script(script, mark_mail_read(USER, MAIL)).await;
        assert!(marked.is_err());
    }

    #[tokio::test]
    async fn mark_mail_saved_swallows_no_error() {
        let (marked, _) = with_script(vec![fault(ADMIN_SHUTDOWN)], mark_mail_saved(USER, MAIL, true)).await;
        assert!(marked.is_err());
        // Not even from the pivot sync behind a flag that already held.
        let script = vec![rows(json!([{ "saved": true }])), fault(ADMIN_SHUTDOWN)];
        let (marked, _) = with_script(script, mark_mail_saved(USER, MAIL, true)).await;
        assert!(marked.is_err());
    }

    #[tokio::test]
    async fn delete_mail_swallows_no_error() {
        let (deleted, _) = with_script(vec![fault(ADMIN_SHUTDOWN)], delete_mail(USER, MAIL)).await;
        assert!(deleted.is_err());
    }

    #[tokio::test]
    async fn mark_mail_spam_swallows_no_error() {
        let (marked, _) = with_script(vec![fault(ADMIN_SHUTDOWN)], mark_mail_spam(USER, MAIL, true)).await;
        assert!(marked.is_err());
        let (marked, _) = with_script(vec![reserved(8), none(), fault(ADMIN_SHUTDOWN)], mark_mail_spam(USER, MAIL, true)).await;
        assert!(marked.is_err());
    }

    // The servers run these on spawned tasks.

    #[test]
    fn every_repository_call_can_cross_threads() {
        use super::super::counters::*;
        use crate::postgres::repositories::mailboxes::*;

        fn sendable<T: Send>(_: T) {}
        let input = SaveMailInput::default();
        sendable(save_mail(&input));
        sendable(get_mail_by_id(USER, MAIL));
        sendable(get_mail_by_message_id(USER, "<m@x>"));
        sendable(update_rfc822_size(USER, MAIL, 1));
        sendable(mark_mail_read(USER, MAIL));
        sendable(mark_mail_saved(USER, MAIL, true));
        sendable(mark_mail_spam(USER, MAIL, true));
        sendable(delete_mail(USER, MAIL));
        sendable(record_mappings(USER, MAIL, &[], None));
        sendable(sync_mapped_pivots_for_row(USER, MAIL, None, None, None));
        sendable(get_domain_uid_next(USER, false));
        sendable(get_account_uid_next(USER, "a@x", false));
        sendable(get_mailbox_uid_next(USER, "Starred"));
        sendable(get_uid_next(USER, &UidScope::Domain { sent: false }));
        sendable(get_next_modseq(USER));
        sendable(get_highest_modseq(USER, None, false));
        sendable(sync_mailbox_pivot(USER, "Starred", MAIL, true));
        sendable(write_mailbox_uid(USER, "Starred", MAIL, 1));
        sendable(delete_mailbox_uid(USER, "Starred", MAIL));
        let create = CreateMailboxInput::default();
        sendable(create_mailbox(&create));
        sendable(get_mailboxes_by_user(USER));
        sendable(get_mailbox_by_name(USER, "Archive"));
        sendable(get_mailbox_by_address(USER, "a@x"));
        sendable(delete_mailbox_by_name(USER, "Archive"));
        sendable(rename_mailbox(USER, "Archive", "Old"));
        sendable(set_mailbox_subscribed(USER, "Archive", true));
    }

    // Envelope merging, which the merge branch does in JavaScript terms.

    #[test]
    fn merging_envelopes_appends_only_addresses_not_stored_yet() {
        let stored = json!([{ "address": "a@x", "name": "A" }, { "name": "no address" }]);
        let incoming = json!([{ "address": "a@x" }, { "address": "b@x" }, { "address": "b@x" }, { "name": "also none" }]);
        assert_eq!(
            merge_envelope_to(Some(&stored), &incoming).unwrap(),
            json!([{ "address": "a@x", "name": "A" }, { "name": "no address" }, { "address": "b@x" }, { "address": "b@x" }])
        );
        assert_eq!(merge_envelope_to(None, &json!([{ "address": "a@x" }])).unwrap(), json!([{ "address": "a@x" }]));
        assert!(merge_envelope_to(Some(&json!({ "address": "a@x" })), &json!([])).is_err());
        assert!(merge_envelope_to(None, &json!({ "address": "a@x" })).is_err());
    }
}
