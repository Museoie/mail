//! Message operations (imap/message-ops.ts): FETCH, SEARCH, STORE, COPY,
//! MOVE, APPEND and EXPUNGE, and the mailbox reads IDLE notifications make.
//!
//! Every command resolves its sequence set against the session's mapping,
//! reads and writes through the [`Store`], and answers on the session.
//! Commands run one at a time on a session, inside its turn, so the mapping
//! read at the start of a command is the one it answers against.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use futures_util::future::{join_all, try_join_all};
use serde_json::{json, Value};

use super::fetch_format::{build_fetch_response, convert_sequence_set, get_requested_fields};
use super::fetch_stream::write_fetch_response;
use super::sequence_resolver::{
    clamp_sequence_set_to_first, count_sequence_set_messages, reconcile_sequence_mapping, resolve_seq_range_to_uids,
    resolve_uid_range_sentinel, uid_to_seq_number, MailboxUids,
};
use super::session::ImapSession;
use super::session_format::should_mark_as_read;
use super::store::{Address, Mail, MailUid, Store, StoreFetchedMail, StoreOperationType};
use super::types::{
    AppendRequest, BodySection, CopyRequest, FetchDataItem, FetchRequest, MoveRequest, SearchCriterion,
    SearchRequest, SequenceRange, SequenceSet, SequenceSetType, StoreRequest,
};
use super::util::{
    box_to_account, canonical_mailbox, derive_copy_message_id, is_domain_scoped, is_mapped_utility_folder,
    is_sent_box,
};
use crate::logger;
use crate::mails::receive::{convert_attachments, convert_mail_address, incoming_mail_from_parsed, MailAddress};
use crate::mails::update::mark_read;
use crate::mails::util::{get_attachment_file_path, get_text};
use crate::mime::parser::{parse_message, ParseOptions};
use crate::postgres::repositories::mails::core::update_rfc822_size;
use crate::postgres::repositories::mails::counters::{
    get_account_uid_next, get_domain_uid_next, get_mailbox_uid_next, sync_mailbox_pivot,
};
use crate::postgres::repositories::users::get_imap_uid_validity;

// ---------------------------------------------------------------------------
// What IDLE notifications read
// ---------------------------------------------------------------------------

/// The flags of one mail in a mailbox's view, as a flag notification
/// reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFlagUpdate {
    pub uid: i64,
    pub read: bool,
    pub saved: bool,
    pub deleted: bool,
    pub draft: bool,
    pub answered: bool,
}

impl MailFlagUpdate {
    /// The system flags that are set, in the order every FLAGS list uses.
    pub fn flags(&self) -> Vec<&'static str> {
        [
            (self.read, "\\Seen"),
            (self.saved, "\\Flagged"),
            (self.deleted, "\\Deleted"),
            (self.draft, "\\Draft"),
            (self.answered, "\\Answered"),
        ]
        .into_iter()
        .filter_map(|(set, flag)| set.then_some(flag))
        .collect()
    }
}

/// The store and the selected mailbox, or `None` when either is missing.
fn store_and_selected(session: &ImapSession) -> Option<(Store, String)> {
    let state = session.state();
    Some((state.store.clone()?, state.selected_mailbox.clone()?))
}

/// The UIDs of the session's selected mailbox in ascending order. Used
/// wherever the sequence mapping is rebuilt: an IDLE notification, and after
/// EXPUNGE, MOVE and APPEND into the selected mailbox.
pub async fn read_selected_mailbox_uids(session: &ImapSession) -> MailboxUids {
    let Some((store, mailbox)) = store_and_selected(session) else {
        return MailboxUids::NotSelected;
    };
    match store.get_all_uids(&mailbox).await {
        Some(uids) => MailboxUids::Uids(uids),
        None => MailboxUids::Unreadable,
    }
}

/// The current flags of `mail_ids` as the selected mailbox sees them;
/// `None` when the mailbox could not be read.
pub async fn read_mail_flag_updates(
    session: &ImapSession,
    selected_mailbox: &str,
    mail_ids: &[String],
) -> Result<Option<Vec<MailFlagUpdate>>> {
    let Some(store) = session.state().store.clone() else {
        return Ok(None);
    };
    let updates = store.get_mail_flag_updates(selected_mailbox, mail_ids).await;
    Ok(updates.map(|updates| {
        updates
            .into_iter()
            .map(|update| MailFlagUpdate {
                uid: update.uid,
                read: update.read,
                saved: update.saved,
                deleted: update.deleted,
                draft: update.draft,
                answered: update.answered,
            })
            .collect()
    }))
}

/// Rebuilds the sequence mapping from the selected mailbox, writing an
/// untagged EXPUNGE for every message that left it, highest first. The new
/// message count, or `None` when the mailbox could not be read.
pub async fn reconcile_selected(session: &ImapSession) -> Option<usize> {
    let read = read_selected_mailbox_uids(session).await;
    let mut pending: Vec<String> = Vec::new();
    let total = reconcile_sequence_mapping(&mut session.state().seq_state, read, &mut |data| {
        pending.push(data.to_string())
    });
    for response in &pending {
        session.write(response);
    }
    total
}

/// The UID a mail answers to in `mailbox`: its domain UID in a
/// domain-scoped view, its per-box UID elsewhere.
fn uid_in(mail: &StoreFetchedMail, domain_scoped: bool) -> Result<i64> {
    let uid = mail.uid.ok_or_else(|| anyhow!("the row carries no uid"))?;
    Ok(if domain_scoped { uid.domain } else { uid.account })
}

fn sequence_of(session: &ImapSession, uid: i64) -> Option<usize> {
    let state = session.state();
    uid_to_seq_number(&state.seq_state.seq_to_uid, &state.seq_state.uid_to_seq, uid as f64)
}

/// The UID range each range of `sequence_set` resolves to: sequence numbers
/// through the mapping, UIDs with their `*` resolved. A sequence range past
/// the end of the mailbox resolves to nothing and is left out.
fn uid_ranges(session: &ImapSession, sequence_set: &SequenceSet, is_uid: bool) -> Vec<(f64, f64)> {
    let state = session.state();
    let seq_to_uid = &state.seq_state.seq_to_uid;
    let mut out = Vec::new();
    for range in convert_sequence_set(sequence_set) {
        if is_uid {
            let resolved = resolve_uid_range_sentinel(seq_to_uid, range.start, range.end);
            out.push((resolved.uid_start, resolved.uid_end));
        } else {
            match resolve_seq_range_to_uids(seq_to_uid, range.start, range.end) {
                Some(resolved) => out.push((resolved.uid_start as f64, resolved.uid_end as f64)),
                None => logger::warn(
                    "Sequence range matched no messages",
                    json!({ "component": "imap", "start": range.start, "end": range.end }),
                ),
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// FETCH
// ---------------------------------------------------------------------------

fn is_row_metadata_item(item: &FetchDataItem) -> bool {
    matches!(
        item,
        FetchDataItem::Flags
            | FetchDataItem::Uid
            | FetchDataItem::Modseq
            | FetchDataItem::Rfc822Size
            | FetchDataItem::InternalDate
    )
}

/// The size of an attachment's file, `None` when it is missing or not a
/// regular file. A metadata call: nothing is read.
fn attachment_file_size(data_id: &str) -> Option<u64> {
    std::fs::metadata(get_attachment_file_path(Some(data_id))).ok().filter(|meta| meta.is_file()).map(|meta| meta.len())
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_messages_typed(
    tag: &str,
    fetch_request: &FetchRequest,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    session: &ImapSession,
    condstore_enabled: bool,
    // Suppresses the automatic \Seen of a body fetch for a read-only
    // session, so reading a message never changes a flag another client is
    // looking at.
    suppress_read_mark: bool,
) {
    let items = &fetch_request.data_items;
    let is_flags_only = items.iter().all(is_row_metadata_item);
    let is_header_only = items.iter().all(|item| {
        is_row_metadata_item(item)
            || matches!(item, FetchDataItem::Body(body) if matches!(body.section, BodySection::Header | BodySection::HeaderFields { .. }))
    });
    let mut request = fetch_request.clone();
    let requested_count = {
        let state = session.state();
        count_sequence_set_messages(&state.seq_state.seq_to_uid, &request.sequence_set, is_uid_command)
    };
    let limit = if is_flags_only {
        f64::INFINITY
    } else if is_header_only {
        500.0
    } else {
        50.0
    };
    if requested_count > limit {
        // A subset, not `NO [LIMIT]`: RFC 3501 §6.4.5 lets the server return
        // fewer messages than asked, and the client fetches the rest. iOS
        // Mail treats any tagged NO as fatal.
        let clamped = {
            let state = session.state();
            clamp_sequence_set_to_first(&state.seq_state.seq_to_uid, &request.sequence_set, limit, is_uid_command)
        };
        logger::debug(
            "FETCH clamped to server per-command cap",
            json!({ "component": "imap", "tag": tag, "requestedCount": requested_count, "limit": limit }),
        );
        request.sequence_set = clamped;
    }

    // RFC 4551 §3.3.1: a CHANGEDSINCE fetch implies MODSEQ in this response
    // even if the session never enabled CONDSTORE.
    let emit_condstore = condstore_enabled || request.changed_since.is_some();

    let outcome = async {
        let messages = fetch_messages(&request, is_uid_command, store, selected_mailbox, session, emit_condstore).await;
        process_fetch_messages(
            messages,
            &request,
            is_uid_command,
            store,
            selected_mailbox,
            session,
            emit_condstore,
            suppress_read_mark,
        )
        .await
    }
    .await;
    match outcome {
        Ok(()) => {
            session.write(&format!("{tag} OK FETCH completed\r\n"));
        }
        Err(error) => {
            logger::error_with("FETCH error", json!({ "component": "imap" }), &error);
            session.write(&format!("{tag} NO FETCH failed\r\n"));
        }
    }
}

/// The messages of every range, merged by mail id the way a `Map` merges:
/// a mail named by two ranges keeps its first position and takes the later
/// read.
async fn fetch_messages(
    request: &FetchRequest,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    session: &ImapSession,
    condstore_enabled: bool,
) -> Vec<(String, StoreFetchedMail)> {
    let mut fields = get_requested_fields(&request.data_items);
    // MODSEQ comes with the same range read, whether asked for or implied
    // by CONDSTORE (RFC 4551 §3.3.2).
    if condstore_enabled || request.data_items.iter().any(|item| matches!(item, FetchDataItem::Modseq)) {
        fields.add("modseq");
    }
    let is_uid_fetch = request.sequence_set.kind == SequenceSetType::Uid || is_uid_command;
    let ranges = uid_ranges(session, &request.sequence_set, is_uid_fetch);
    let reads = join_all(
        ranges
            .iter()
            .map(|(start, end)| store.get_messages(selected_mailbox, *start, *end, fields.as_slice(), true, request.changed_since)),
    )
    .await;
    merge_fetched(reads)
}

/// Merges range reads with `Map` semantics.
pub fn merge_fetched(reads: Vec<Vec<(String, StoreFetchedMail)>>) -> Vec<(String, StoreFetchedMail)> {
    let mut merged: Vec<(String, StoreFetchedMail)> = Vec::new();
    let mut position: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for read in reads {
        for (mail_id, mail) in read {
            match position.get(&mail_id) {
                Some(at) => merged[*at].1 = mail,
                None => {
                    position.insert(mail_id.clone(), merged.len());
                    merged.push((mail_id, mail));
                }
            }
        }
    }
    merged
}

#[allow(clippy::too_many_arguments)]
async fn process_fetch_messages(
    messages: Vec<(String, StoreFetchedMail)>,
    request: &FetchRequest,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    session: &ImapSession,
    condstore_enabled: bool,
    suppress_read_mark: bool,
) -> Result<()> {
    let source_is_domain_scoped = is_domain_scoped(selected_mailbox);
    let is_uid_fetch = request.sequence_set.kind == SequenceSetType::Uid || is_uid_command;
    let user_id = store.get_user().id.clone();

    for (id, mail) in messages {
        let uid = uid_in(&mail, source_is_domain_scoped)?;
        let Some(seq) = sequence_of(session, uid) else {
            logger::warn("No sequence number found for UID", json!({ "component": "imap", "uid": uid }));
            continue;
        };

        let written: Result<()> = async {
            let input = mail.to_fetch_input();
            let response = build_fetch_response(
                &input,
                &request.data_items,
                &id,
                uid as f64,
                is_uid_fetch,
                selected_mailbox,
                condstore_enabled,
                &attachment_file_size,
            );
            if let Some(size) = response.computed_rfc822_size {
                // Persisted for the next fetch, and not waited for: a failed
                // write is logged and the next fetch computes it again.
                let (user_id, mail_id) = (user_id.clone(), id.clone());
                tokio::spawn(async move {
                    if let Err(error) = update_rfc822_size(&user_id, &mail_id, size as i64).await {
                        logger::warn(
                            "Failed to persist rfc822_size",
                            json!({ "component": "imap.fetch", "mail_id": mail_id, "err": error.to_string() }),
                        );
                    }
                });
            }
            write_fetch_response(session, seq, &response.parts).await?;

            // A body fetch marks the mail read, unless the FETCH already
            // read its flags and found it read.
            if !suppress_read_mark && should_mark_as_read(&request.data_items) && mail.read != Some(true) {
                mark_read(&user_id, &id).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = written {
            logger::error_with(
                "Error processing message",
                json!({ "component": "imap", "seqNum": seq, "stack": format!("{error:?}") }),
                &error,
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SEARCH
// ---------------------------------------------------------------------------

/// A UID-axis set's ranges as concrete UIDs, `*` read as the mailbox's
/// highest UID.
fn resolve_uid_criterion_ranges(sequence_set: &SequenceSet, seq_to_uid: &[i64]) -> SearchCriterion {
    let ranges = convert_sequence_set(sequence_set)
        .into_iter()
        .map(|range| {
            let resolved = resolve_uid_range_sentinel(seq_to_uid, range.start, range.end);
            SequenceRange { start: resolved.uid_start, end: Some(resolved.uid_end) }
        })
        .collect();
    SearchCriterion::Uid(SequenceSet { kind: SequenceSetType::Sequence, ranges })
}

fn resolve_one_search_key(criterion: &SearchCriterion, is_uid_command: bool, seq_to_uid: &[i64]) -> SearchCriterion {
    criterion.fold(
        &mut |key| match key {
            SearchCriterion::Uid(set) => resolve_uid_criterion_ranges(set, seq_to_uid),
            SearchCriterion::Seq(set) if is_uid_command => resolve_uid_criterion_ranges(set, seq_to_uid),
            SearchCriterion::Seq(set) => {
                let mut ranges: Vec<SequenceRange> = convert_sequence_set(set)
                    .into_iter()
                    .filter_map(|range| resolve_seq_range_to_uids(seq_to_uid, range.start, range.end))
                    .map(|resolved| SequenceRange {
                        start: resolved.uid_start as f64,
                        end: Some(resolved.uid_end as f64),
                    })
                    .collect();
                // A set wholly past the end of the mailbox matches nothing,
                // so it is pinned to a UID no message has rather than
                // dropped from the AND, where it would match everything.
                if ranges.is_empty() {
                    ranges.push(SequenceRange { start: -1.0, end: Some(-1.0) });
                }
                SearchCriterion::Uid(SequenceSet { kind: SequenceSetType::Sequence, ranges })
            }
            other => other.clone(),
        },
        &mut SearchCriterion::not,
        &mut SearchCriterion::or,
    )
}

/// The bare sequence-set keys of a SEARCH resolved to UIDs: a plain SEARCH
/// names sequence numbers, read through the mapping; a UID SEARCH names
/// UIDs, whose `*` is resolved as an explicit `UID <set>` key's is. The
/// store understands UID, flag, text and date keys only.
pub fn resolve_seq_search_keys(
    criteria: &[SearchCriterion],
    is_uid_command: bool,
    seq_to_uid: &[i64],
) -> Vec<SearchCriterion> {
    criteria.iter().map(|criterion| resolve_one_search_key(criterion, is_uid_command, seq_to_uid)).collect()
}

pub async fn search_typed(
    tag: &str,
    search_request: &SearchRequest,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    session: &ImapSession,
) {
    if search_request.criteria.is_empty() {
        session.write(&format!("{tag} BAD Search criteria is required\r\n"));
        return;
    }

    // The explicit `UID <set>` key is only valid under the UID command; a
    // bare sequence set is valid in both.
    let has_uid_criteria = search_request.criteria.iter().any(|criterion| matches!(criterion, SearchCriterion::Uid(_)));
    if !is_uid_command && has_uid_criteria {
        session.write(&format!("{tag} NO Not supported\r\n"));
        return;
    }

    let criteria = {
        let state = session.state();
        resolve_seq_search_keys(&search_request.criteria, is_uid_command, &state.seq_state.seq_to_uid)
    };

    let uids = store.search(selected_mailbox, &criteria).await;
    let result: Vec<String> = if is_uid_command {
        uids.iter().map(i64::to_string).collect()
    } else {
        uids.iter().filter_map(|uid| sequence_of(session, *uid)).map(|seq| seq.to_string()).collect()
    };
    session.write(&format!("* SEARCH {}\r\n", result.join(" ")));
    session.write(&format!("{tag} OK SEARCH completed\r\n"));
}

// ---------------------------------------------------------------------------
// STORE
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn store_flags_typed(
    tag: &str,
    store_request: &StoreRequest,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    mailbox_read_only: bool,
    session: &ImapSession,
    condstore_enabled: bool,
) {
    if mailbox_read_only {
        session.write(&format!("{tag} NO [READ-ONLY] Mailbox is read-only\r\n"));
        return;
    }
    if let Err(error) =
        store_flags(tag, store_request, is_uid_command, store, selected_mailbox, session, condstore_enabled).await
    {
        logger::error_with("Error storing flags", json!({ "component": "imap" }), &error);
        session.write(&format!("{tag} NO STORE failed\r\n"));
    }
}

async fn store_flags(
    tag: &str,
    request: &StoreRequest,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    session: &ImapSession,
    condstore_enabled: bool,
) -> Result<()> {
    let is_uid_store = request.sequence_set.kind == SequenceSetType::Uid || is_uid_command;
    // Accumulated across every range, so a message the set names twice is
    // judged on its best outcome: a second pass over a UID the first pass
    // changed finds it above the client's ceiling, and that is no conflict.
    let mut succeeded: HashSet<i64> = HashSet::new();
    let mut failed_uids: Vec<i64> = Vec::new();

    let operation = StoreOperationType::from(request.operation);
    let touches_saved = operation == StoreOperationType::Flags || request.flags.iter().any(|flag| flag == "\\Flagged");
    let touches_deleted =
        operation == StoreOperationType::Flags || request.flags.iter().any(|flag| flag == "\\Deleted");
    let is_silent = request.silent || request.operation.as_str().contains("SILENT");
    // RFC 7162 §3.1.3: a conditional STORE reports every message it changed,
    // with its MODSEQ, even when silent.
    let emit_flags = !is_silent;
    let emit_modseq = condstore_enabled || request.unchanged_since.is_some();
    let user_id = store.get_user().id.clone();

    for (uid_start, uid_end) in uid_ranges(session, &request.sequence_set, is_uid_store) {
        let result = store
            .set_flags(selected_mailbox, uid_start, uid_end, &request.flags, true, operation, request.unchanged_since)
            .await;
        succeeded.extend(result.updated.iter().map(|mail| mail.uid));
        for uid in result.failed {
            if !failed_uids.contains(&uid) {
                failed_uids.push(uid);
            }
        }

        // RFC 3501 §6.4.6: a range that matches nothing is not an error.
        if result.updated.is_empty() {
            continue;
        }

        if touches_saved || touches_deleted {
            let mut writes = Vec::new();
            for mail in &result.updated {
                if touches_saved {
                    writes.push(sync_mailbox_pivot(&user_id, "Starred", &mail.mail_id, mail.saved));
                }
                if touches_deleted {
                    writes.push(sync_mailbox_pivot(&user_id, "Trash", &mail.mail_id, mail.deleted));
                }
            }
            try_join_all(writes).await?;
        }

        if emit_flags || emit_modseq {
            for mail in &result.updated {
                let Some(seq) = sequence_of(session, mail.uid) else {
                    continue;
                };
                let mut items: Vec<String> = Vec::new();
                if emit_flags {
                    let flags = MailFlagUpdate {
                        uid: mail.uid,
                        read: mail.read,
                        saved: mail.saved,
                        deleted: mail.deleted,
                        draft: mail.draft,
                        answered: mail.answered,
                    }
                    .flags();
                    items.push(format!("FLAGS ({})", flags.join(" ")));
                }
                if emit_modseq {
                    items.push(format!("MODSEQ ({})", mail.modseq));
                }
                let uid_item = if is_uid_store { format!("UID {} ", mail.uid) } else { String::new() };
                session.write(&format!("* {seq} FETCH ({uid_item}{})\r\n", items.join(" ")));
            }
        }
    }

    // MODIFIED names the conflicts as the command named its messages: UIDs
    // for UID STORE, sequence numbers for STORE (RFC 7162 §3.1.3). A UID
    // with no sequence number has left the view and is dropped.
    let conflicted: Vec<i64> = failed_uids
        .into_iter()
        .filter(|uid| !succeeded.contains(uid))
        .filter_map(|uid| if is_uid_store { Some(uid) } else { sequence_of(session, uid).map(|seq| seq as i64) })
        .collect();
    if !conflicted.is_empty() {
        session.write(&format!("{tag} OK [MODIFIED {}] Conditional STORE failed\r\n", format_message_set(&conflicted)));
        return Ok(());
    }
    session.write(&format!("{tag} OK STORE completed\r\n"));
    Ok(())
}

// ---------------------------------------------------------------------------
// COPY / MOVE shared helpers
// ---------------------------------------------------------------------------

/// The facts about a COPY / MOVE / APPEND destination every path needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestContext {
    /// The address a per-account box belongs to.
    pub account: String,
    pub is_sent: bool,
    pub is_domain_scoped: bool,
    pub is_mapped_utility: bool,
    /// Domain-scoped and mapped-utility boxes select rows without an
    /// address term, so a copy filed there keeps its recipients.
    pub preserves_recipient: bool,
}

pub fn resolve_dest_context(username: &str, destination: &str) -> DestContext {
    let is_domain_scoped = is_domain_scoped(destination);
    let is_mapped_utility = is_mapped_utility_folder(destination);
    DestContext {
        account: box_to_account(username, destination),
        is_sent: is_sent_box(destination),
        is_domain_scoped,
        is_mapped_utility,
        preserves_recipient: is_domain_scoped || is_mapped_utility,
    }
}

/// The fields a COPY / MOVE reads from its sources.
const CLONE_FIELDS: [&str; 20] = [
    "subject",
    "date",
    "html",
    "text",
    "from",
    "to",
    "cc",
    "bcc",
    "replyTo",
    "envelopeFrom",
    "envelopeTo",
    "attachments",
    "messageId",
    "insight",
    "read",
    "saved",
    "deleted",
    "draft",
    "answered",
    "uid",
];

/// Reserves the destination's UIDs for `mail` and stores it there. False
/// when the store refused it. The domain UID is always reserved; the per-box
/// UID comes from the box's own counter for a mapped utility folder and
/// from the account counter otherwise.
async fn store_in_destination(store: &Store, mail: &mut Mail, destination: &str, context: &DestContext) -> Result<bool> {
    let user = store.get_user();
    mail.sent = context.is_sent;
    let domain_uid = get_domain_uid_next(&user.id, context.is_sent).await?;
    let account_uid = if context.is_mapped_utility {
        get_mailbox_uid_next(&user.id, destination).await?
    } else {
        get_account_uid_next(&user.id, &context.account, context.is_sent).await?
    };
    mail.uid = MailUid { domain: domain_uid, account: account_uid };
    Ok(store.store_mail(mail, Some(destination)).await)
}

/// `cloneMailToDestination`: files a copy of `source` into `destination`
/// under fresh UIDs and answers the (source, destination) UID pair; `None`
/// when the store refused it.
///
/// Flags carry over (RFC 3501 §6.4.7). Unless the destination keeps
/// recipients, the copy's To and envelope are re-anchored to the
/// destination account and its Cc/Bcc address lists emptied (their text
/// kept), so it shows in the destination and not again in the source.
pub async fn clone_mail_to_destination(
    store: &Store,
    source: &StoreFetchedMail,
    source_uid: i64,
    destination: &str,
    context: &DestContext,
) -> Result<Option<(i64, i64)>> {
    let defaults = Mail::default();
    let non_empty =
        |value: &Option<String>, default: String| value.clone().filter(|value| !value.is_empty()).unwrap_or(default);
    let mut mail = Mail {
        attachments: source.attachments.clone().or(defaults.attachments),
        from: source.from.clone(),
        to: None,
        cc: source.cc.clone(),
        bcc: source.bcc.clone(),
        reply_to: source.reply_to.clone(),
        envelope_from: source.envelope_from.clone(),
        envelope_to: defaults.envelope_to,
        date: non_empty(&source.date, defaults.date),
        html: non_empty(&source.html, defaults.html),
        text: non_empty(&source.text, defaults.text),
        subject: non_empty(&source.subject, defaults.subject),
        message_id: derive_copy_message_id(source.message_id.as_deref(), destination),
        read: source.read.unwrap_or(false),
        saved: source.saved.unwrap_or(false),
        sent: false,
        deleted: source.deleted.unwrap_or(false),
        draft: source.draft.unwrap_or(false),
        answered: source.answered.unwrap_or(false),
        insight: source.insight.clone(),
        uid: MailUid::default(),
        in_reply_to: None,
        references: None,
    };
    if context.preserves_recipient {
        mail.to = source.to.clone();
        mail.envelope_to = source.envelope_to.clone().unwrap_or_else(|| json!([]));
    } else {
        let destination_address = json!({ "address": context.account, "name": "" });
        let to_text = source.to.as_ref().map(|to| to.text.clone()).filter(|text| !text.is_empty());
        mail.to = Some(Address { value: json!([destination_address]), text: to_text.unwrap_or_else(|| context.account.clone()) });
        mail.envelope_to = json!([destination_address]);
        mail.cc = source.cc.as_ref().map(|cc| Address { value: json!([]), text: cc.text.clone() });
        mail.bcc = source.bcc.as_ref().map(|bcc| Address { value: json!([]), text: bcc.text.clone() });
    }

    if !store_in_destination(store, &mut mail, destination, context).await? {
        return Ok(None);
    }
    let destination_uid = if context.is_domain_scoped { mail.uid.domain } else { mail.uid.account };
    Ok(Some((source_uid, destination_uid)))
}

/// Compacts message numbers to an RFC 3501 sequence set (`1,3:5,7`), as the
/// COPYUID, APPENDUID and MODIFIED codes carry them: sorted, without
/// repeats.
pub fn format_message_set(numbers: &[i64]) -> String {
    let mut sorted: Vec<i64> = numbers.iter().copied().collect::<HashSet<i64>>().into_iter().collect();
    sorted.sort_unstable();
    let Some(&first) = sorted.first() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    let (mut range_start, mut range_end) = (first, first);
    let close = |start: i64, end: i64, parts: &mut Vec<String>| {
        parts.push(if start == end { start.to_string() } else { format!("{start}:{end}") });
    };
    for &number in &sorted[1..] {
        if number == range_end + 1 {
            range_end = number;
        } else {
            close(range_start, range_end, &mut parts);
            range_start = number;
            range_end = number;
        }
    }
    close(range_start, range_end, &mut parts);
    parts.join(",")
}

/// The copy phase COPY and MOVE share.
enum Copied {
    /// No range resolved to a message: a bare OK.
    Nothing,
    /// The destination does not exist.
    TryCreate,
    /// The store refused one of the copies; those before it stay.
    Partial,
    /// Every source copied: the (source, destination) UID pairs, ascending
    /// by source UID.
    Pairs(Vec<(i64, i64)>),
}

async fn copy_phase(
    sequence_set: &SequenceSet,
    destination: &str,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    session: &ImapSession,
) -> Result<Copied> {
    // RFC 4315 §2.1: the destination must exist; NO [TRYCREATE] otherwise.
    if !store.mailbox_exists(destination).await? {
        return Ok(Copied::TryCreate);
    }
    let is_uid = sequence_set.kind == SequenceSetType::Uid || is_uid_command;
    let ranges = uid_ranges(session, sequence_set, is_uid);
    if ranges.is_empty() {
        return Ok(Copied::Nothing);
    }

    let mut sources: Vec<StoreFetchedMail> = Vec::new();
    for (start, end) in ranges {
        let batch = store.get_messages(selected_mailbox, start, end, &CLONE_FIELDS, true, None).await;
        sources.extend(batch.into_iter().map(|(_, mail)| mail));
    }
    if sources.is_empty() {
        return Ok(Copied::Nothing);
    }

    let context = resolve_dest_context(&store.get_user().username, destination);
    let source_is_domain_scoped = is_domain_scoped(selected_mailbox);
    let mut keyed: Vec<(i64, StoreFetchedMail)> = Vec::with_capacity(sources.len());
    for source in sources {
        keyed.push((uid_in(&source, source_is_domain_scoped)?, source));
    }
    // RFC 4315 §3: the COPYUID sets correspond positionally, and each is
    // sorted on its own, so copies are made in ascending source-UID order
    // (a stable sort, as Array.prototype.sort is). A UID that two ranges
    // both name is copied once.
    keyed.sort_by_key(|(uid, _)| *uid);
    let mut seen = HashSet::new();
    keyed.retain(|(uid, _)| seen.insert(*uid));

    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for (source_uid, source) in &keyed {
        match clone_mail_to_destination(store, source, *source_uid, destination, &context).await? {
            Some(pair) => pairs.push(pair),
            None => return Ok(Copied::Partial),
        }
    }
    Ok(Copied::Pairs(pairs))
}

fn copy_uid_code(uid_validity: i64, pairs: &[(i64, i64)]) -> String {
    let sources: Vec<i64> = pairs.iter().map(|(source, _)| *source).collect();
    let destinations: Vec<i64> = pairs.iter().map(|(_, destination)| *destination).collect();
    format!("COPYUID {uid_validity} {} {}", format_message_set(&sources), format_message_set(&destinations))
}

// ---------------------------------------------------------------------------
// COPY (RFC 3501 §6.4.7, RFC 4315 COPYUID)
// ---------------------------------------------------------------------------

pub async fn copy_message_typed(
    tag: &str,
    copy_request: &CopyRequest,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    session: &ImapSession,
) {
    // RFC 3501 §5.1: INBOX is case-insensitive.
    let destination = canonical_mailbox(&copy_request.mailbox);
    let outcome = async {
        match copy_phase(&copy_request.sequence_set, &destination, is_uid_command, store, selected_mailbox, session)
            .await?
        {
            Copied::TryCreate => {
                session.write(&format!("{tag} NO [TRYCREATE] Mailbox does not exist\r\n"));
            }
            // Nothing copied: OK without COPYUID (RFC 4315).
            Copied::Nothing => {
                session.write(&format!("{tag} OK COPY completed\r\n"));
            }
            Copied::Partial => {
                session.write(&format!("{tag} NO [SERVERBUG] COPY partially failed\r\n"));
            }
            Copied::Pairs(pairs) => {
                let uid_validity = get_imap_uid_validity(&store.get_user().id).await?;
                session.write(&format!("{tag} OK [{}] COPY completed\r\n", copy_uid_code(uid_validity, &pairs)));
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = outcome {
        logger::error_with("COPY error", json!({ "component": "imap" }), &error);
        session.write(&format!("{tag} NO COPY failed\r\n"));
    }
}

// ---------------------------------------------------------------------------
// MOVE (RFC 6851)
// ---------------------------------------------------------------------------

/// MOVE: the copy phase of COPY, then a targeted expunge of exactly the
/// moved source UIDs (RFC 6851 §3.3: never \Deleted, never other messages),
/// then the mapping rebuilt, which announces every departure, then the
/// tagged OK with COPYUID. A failure in the copy phase leaves the copies
/// made so far and the sources in place; the client can re-issue the MOVE.
pub async fn move_message_typed(
    tag: &str,
    move_request: &MoveRequest,
    is_uid_command: bool,
    store: &Store,
    selected_mailbox: &str,
    mailbox_read_only: bool,
    session: &ImapSession,
) {
    if mailbox_read_only {
        session.write(&format!("{tag} NO [READ-ONLY] Mailbox is read-only\r\n"));
        return;
    }

    let destination = canonical_mailbox(&move_request.mailbox);

    // RFC 6851 §3.4-§3.5: MOVE to the selected mailbox is a no-op, since the
    // messages are already where they would land.
    if destination == selected_mailbox {
        session.write(&format!("{tag} OK MOVE completed\r\n"));
        return;
    }

    let outcome = async {
        match copy_phase(&move_request.sequence_set, &destination, is_uid_command, store, selected_mailbox, session)
            .await?
        {
            Copied::TryCreate => {
                session.write(&format!("{tag} NO [TRYCREATE] Mailbox does not exist\r\n"));
            }
            Copied::Nothing => {
                session.write(&format!("{tag} OK MOVE completed\r\n"));
            }
            Copied::Partial => {
                session.write(&format!("{tag} NO [SERVERBUG] MOVE partially failed during copy phase\r\n"));
            }
            Copied::Pairs(pairs) => {
                let source_uids: Vec<i64> = pairs.iter().map(|(source, _)| *source).collect();
                store.expunge_uids(selected_mailbox, &source_uids).await?;
                if reconcile_selected(session).await.is_none() {
                    return Err(anyhow!("Failed to rebuild the sequence mapping after MOVE"));
                }
                let uid_validity = get_imap_uid_validity(&store.get_user().id).await?;
                session.write(&format!("{tag} OK [{}] MOVE completed\r\n", copy_uid_code(uid_validity, &pairs)));
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = outcome {
        logger::error_with("MOVE error", json!({ "component": "imap" }), &error);
        session.write(&format!("{tag} NO MOVE failed\r\n"));
    }
}

// ---------------------------------------------------------------------------
// APPEND
// ---------------------------------------------------------------------------

fn address_of(address: Option<MailAddress>) -> Option<Address> {
    address.map(|address| Address {
        value: Value::Array(address.value.iter().map(|value| value.to_json()).collect()),
        text: address.text,
    })
}

/// The mail an APPEND stores, built from the spooled message the way a
/// received message is converted: parsed as MIME, its attachments stored as
/// attachments.
async fn parse_appended_message(path: &str) -> Result<Mail> {
    let attachment_dir = super::literal_spool::new_transaction_dir();
    let parsed = async {
        let file = tokio::fs::File::open(path).await?;
        let parsed = parse_message(tokio::io::BufReader::new(file), &ParseOptions { attachment_dir: attachment_dir.clone() }).await?;
        let incoming = incoming_mail_from_parsed(&parsed);
        let attachments = convert_attachments(incoming.attachments.as_deref().unwrap_or(&[])).await?;
        Ok::<_, anyhow::Error>((incoming, attachments))
    }
    .await;
    super::literal_spool::remove_transaction_dir(&attachment_dir).await;
    let (incoming, attachments) = parsed?;

    let defaults = Mail::default();
    let html = incoming.html.clone().unwrap_or_default();
    let text = match incoming.text.clone() {
        Some(text) => text,
        None => get_text(&html),
    };
    let non_empty = |value: Option<String>, default: String| value.filter(|value| !value.is_empty()).unwrap_or(default);
    let address = |list: &Option<Vec<crate::mails::receive::IncomingMailAddress>>| {
        address_of(list.as_deref().and_then(convert_mail_address))
    };
    Ok(Mail {
        attachments: Some(Value::Array(attachments.iter().map(|attachment| attachment.to_json()).collect())),
        from: address(&incoming.from),
        to: address(&incoming.to),
        cc: address(&incoming.cc),
        bcc: address(&incoming.bcc),
        reply_to: address(&incoming.reply_to),
        envelope_from: None,
        envelope_to: defaults.envelope_to,
        date: non_empty(incoming.date.clone(), defaults.date),
        html: non_empty(Some(html), defaults.html),
        text: non_empty(Some(text), defaults.text),
        subject: non_empty(incoming.subject.clone(), defaults.subject),
        // Bare, like the ids appended rows already carry: a re-append meets
        // them on (user_id, message_id).
        message_id: non_empty(incoming.message_id.as_ref().map(|id| id.replace(['<', '>'], "")), defaults.message_id),
        read: false,
        saved: false,
        sent: false,
        deleted: false,
        draft: false,
        answered: false,
        insight: None,
        uid: MailUid::default(),
        in_reply_to: incoming.in_reply_to.clone(),
        references: incoming.references.clone(),
    })
}

/// APPEND. `append_request.message` is the spooled literal, a file the
/// connection removes once this returns.
pub async fn append_message(
    tag: &str,
    append_request: &AppendRequest,
    store: &Store,
    selected_mailbox: Option<&str>,
    session: &ImapSession,
) {
    if let Err(error) = append(tag, append_request, store, selected_mailbox, session).await {
        logger::error_with("APPEND error", json!({ "component": "imap" }), &error);
        session.write(&format!("{tag} NO APPEND failed\r\n"));
    }
}

async fn append(
    tag: &str,
    request: &AppendRequest,
    store: &Store,
    selected_mailbox: Option<&str>,
    session: &ImapSession,
) -> Result<()> {
    // RFC 3501 §5.1: canonical, so APPEND inbox to a selected INBOX is seen
    // as the selected mailbox and the mapping is rebuilt.
    let target = canonical_mailbox(&request.mailbox);

    // RFC 3501 §6.3.11: the target must exist; the server does not create
    // it. Checked before the message is parsed or a UID reserved.
    if !store.mailbox_exists(&target).await? {
        session.write(&format!("{tag} NO [TRYCREATE] Mailbox does not exist\r\n"));
        return Ok(());
    }

    let mut mail = parse_appended_message(&request.message.path).await?;
    let has = |flag: &str| request.flags.as_ref().is_some_and(|flags| flags.iter().any(|named| named == flag));
    mail.draft = has("\\Draft");
    mail.read = has("\\Seen");
    mail.saved = has("\\Flagged");
    mail.deleted = has("\\Deleted");
    mail.answered = has("\\Answered");

    let user = store.get_user();
    let context = resolve_dest_context(&user.username, &target);
    let stored = store_in_destination(store, &mut mail, &target, &context).await?;
    let uid = if context.is_domain_scoped { mail.uid.domain } else { mail.uid.account };

    if !stored {
        session.write(&format!("{tag} NO APPEND failed to store message\r\n"));
        return Ok(());
    }
    if selected_mailbox == Some(target.as_str()) {
        // The mail is stored: a failed rebuild is the store's to log, not a
        // NO the client would answer by appending again.
        reconcile_selected(session).await;
    }
    let uid_validity = get_imap_uid_validity(&user.id).await?;
    session.write(&format!("{tag} OK [APPENDUID {uid_validity} {uid}] APPEND completed\r\n"));
    Ok(())
}

// ---------------------------------------------------------------------------
// EXPUNGE
// ---------------------------------------------------------------------------

pub async fn expunge(tag: &str, store: &Store, selected_mailbox: &str, mailbox_read_only: bool, session: &ImapSession) {
    if mailbox_read_only {
        session.write(&format!("{tag} NO [READ-ONLY] Mailbox is read-only\r\n"));
        return;
    }
    let outcome = async {
        store.expunge(selected_mailbox).await?;
        // The rebuild writes the `* n EXPUNGE` lines.
        if reconcile_selected(session).await.is_none() {
            return Err(anyhow!("Failed to rebuild the sequence mapping after EXPUNGE"));
        }
        Ok(())
    }
    .await;
    match outcome {
        Ok(()) => {
            session.write(&format!("{tag} OK EXPUNGE completed\r\n"));
        }
        Err(error) => {
            logger::error_with("Expunge failed", json!({ "component": "imap" }), &error);
            session.write(&format!("{tag} NO EXPUNGE failed\r\n"));
        }
    }
}

#[cfg(test)]
#[path = "message_ops_tests.rs"]
mod tests;
