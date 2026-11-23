//! Mailbox management operations (imap/mailbox-ops.ts): CREATE, DELETE,
//! RENAME, SUBSCRIBE, UNSUBSCRIBE, STATUS, LIST, LSUB and SELECT / EXAMINE.
//!
//! CREATE, DELETE, RENAME, SUBSCRIBE and UNSUBSCRIBE go to the mailbox
//! repository; STATUS, LIST, LSUB and SELECT read through the IMAP store.

use std::collections::HashSet;

use serde_json::json;

use super::auth::strip_surrounding_quotes;
use super::sequence_resolver::{build_sequence_mapping, MailboxUids};
use super::session::ImapSession;
use super::store::{MailboxEntry, Store};
use super::types::StatusItem;
use super::util::{
    canonical_mailbox, is_accounts_folder, is_inbox, is_sent_messages_accounts_folder, is_utility_folder,
    quote_string, utility_folder, ACCOUNTS_FOLDER, SENT_MESSAGES_ACCOUNTS_FOLDER,
};
use crate::logger;
use crate::postgres::repositories::mailboxes::{
    create_mailbox as db_create_mailbox, delete_mailbox_by_name, rename_mailbox as db_rename_mailbox,
    set_mailbox_subscribed, CreateMailboxInput, CreateMailboxResult, DeleteMailboxResult, RenameMailboxResult,
    MAILBOX_COUNT_MAX, MAILBOX_NAME_MAX_BYTES,
};
use crate::postgres::repositories::users::get_imap_uid_validity;

fn exceeds_name_limit(name: &str) -> bool {
    name.len() > MAILBOX_NAME_MAX_BYTES
}

// ---------------------------------------------------------------------------
// CREATE
// ---------------------------------------------------------------------------

pub async fn create_mailbox(tag: &str, mailbox: &str, store: &Store, session: &ImapSession) {
    let user = store.get_user();
    let clean_name = strip_surrounding_quotes(mailbox);
    if clean_name.is_empty() {
        session.write(&format!("{tag} NO Empty mailbox name\r\n"));
        return;
    }
    // RFC 3501 §6.3.3: "It is an error to attempt to create INBOX or a
    // mailbox with a name that refers to an existent mailbox." INBOX always
    // exists as a synthetic mailbox, in any casing. Without this guard the
    // insert would succeed and leave a phantom row that LIST de-duplicates
    // away but that lingers in the table.
    if is_inbox(clean_name) || is_utility_folder(clean_name) {
        session.write(&format!("{tag} NO [ALREADYEXISTS] Mailbox already exists\r\n"));
        return;
    }
    if exceeds_name_limit(clean_name) {
        session.write(&format!("{tag} NO [LIMIT] Mailbox name exceeds {MAILBOX_NAME_MAX_BYTES} bytes\r\n"));
        return;
    }
    let input = CreateMailboxInput { user_id: user.id.clone(), name: clean_name.to_string(), ..Default::default() };
    match db_create_mailbox(&input).await {
        Ok(CreateMailboxResult::Exists) => {
            session.write(&format!("{tag} NO [ALREADYEXISTS] Mailbox already exists\r\n"));
        }
        Ok(CreateMailboxResult::AtLimit) => {
            session.write(&format!("{tag} NO [LIMIT] Mailbox limit of {MAILBOX_COUNT_MAX} reached\r\n"));
        }
        Ok(CreateMailboxResult::Created(_)) => {
            logger::info("Mailbox created", json!({ "component": "imap", "mailbox": clean_name }));
            session.write(&format!("{tag} OK CREATE completed\r\n"));
        }
        Err(error) => {
            logger::error_with("Error creating mailbox", json!({ "component": "imap", "mailbox": clean_name }), &error);
            session.write(&format!("{tag} NO CREATE failed\r\n"));
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE
// ---------------------------------------------------------------------------

pub async fn delete_mailbox(tag: &str, mailbox: &str, store: &Store, session: &ImapSession) {
    let user = store.get_user();
    let clean_name = strip_surrounding_quotes(mailbox);
    if clean_name.is_empty() {
        session.write(&format!("{tag} NO Empty mailbox name\r\n"));
        return;
    }
    // RFC 3501 §6.3.4: deletion of INBOX is forbidden, in every casing.
    // Without this guard the repository would report "not found" and the
    // client would get a misleading "does not exist".
    if is_inbox(clean_name) {
        session.write(&format!("{tag} NO [CANNOT] Cannot delete INBOX\r\n"));
        return;
    }
    // For the same reason: a utility folder is a server-defined view with
    // no row behind it.
    if is_utility_folder(clean_name) {
        session.write(&format!("{tag} NO [CANNOT] Cannot delete system mailbox\r\n"));
        return;
    }
    match delete_mailbox_by_name(&user.id, clean_name).await {
        Ok(DeleteMailboxResult::NotFound) => {
            session.write(&format!("{tag} NO [NONEXISTENT] Mailbox does not exist\r\n"));
        }
        Ok(DeleteMailboxResult::Protected) => {
            session.write(&format!("{tag} NO [CANNOT] Cannot delete system mailbox\r\n"));
        }
        Ok(DeleteMailboxResult::Deleted) => {
            logger::info("Mailbox deleted", json!({ "component": "imap", "mailbox": clean_name }));
            session.write(&format!("{tag} OK DELETE completed\r\n"));
        }
        Err(error) => {
            logger::error_with("Error deleting mailbox", json!({ "component": "imap", "mailbox": clean_name }), &error);
            session.write(&format!("{tag} NO DELETE failed\r\n"));
        }
    }
}

// ---------------------------------------------------------------------------
// RENAME
// ---------------------------------------------------------------------------

pub async fn rename_mailbox(tag: &str, old_name: &str, new_name: &str, store: &Store, session: &ImapSession) {
    let user = store.get_user();
    let clean_old = strip_surrounding_quotes(old_name);
    let clean_new = strip_surrounding_quotes(new_name);
    if clean_old.is_empty() || clean_new.is_empty() {
        session.write(&format!("{tag} NO Empty mailbox name\r\n"));
        return;
    }
    // RFC 3501 §6.3.5 gives RENAME INBOX special semantics (move every
    // message to the new name, leave INBOX empty) which the backing store
    // does not support, so it is refused outright rather than answered
    // with the misleading "does not exist" the repository would produce.
    if is_inbox(clean_old) {
        session.write(&format!("{tag} NO [CANNOT] RENAME INBOX is not supported\r\n"));
        return;
    }
    if is_utility_folder(clean_old) {
        session.write(&format!("{tag} NO [CANNOT] Cannot rename system mailbox\r\n"));
        return;
    }
    // The target must not collide with a synthetic mailbox either.
    if is_inbox(clean_new) || is_utility_folder(clean_new) {
        session.write(&format!("{tag} NO [ALREADYEXISTS] Target mailbox already exists\r\n"));
        return;
    }
    if exceeds_name_limit(clean_new) {
        session.write(&format!("{tag} NO [LIMIT] Mailbox name exceeds {MAILBOX_NAME_MAX_BYTES} bytes\r\n"));
        return;
    }
    match db_rename_mailbox(&user.id, clean_old, clean_new).await {
        Ok(RenameMailboxResult::NotFound) => {
            session.write(&format!("{tag} NO [NONEXISTENT] Mailbox does not exist\r\n"));
        }
        Ok(RenameMailboxResult::Protected) => {
            session.write(&format!("{tag} NO [CANNOT] Cannot rename system mailbox\r\n"));
        }
        Ok(RenameMailboxResult::NameTaken) => {
            session.write(&format!("{tag} NO [ALREADYEXISTS] Target mailbox already exists\r\n"));
        }
        Ok(RenameMailboxResult::Renamed) => {
            logger::info("Mailbox renamed", json!({ "component": "imap", "from": clean_old, "to": clean_new }));
            session.write(&format!("{tag} OK RENAME completed\r\n"));
        }
        Err(error) => {
            logger::error_with("Error renaming mailbox", json!({ "component": "imap" }), &error);
            session.write(&format!("{tag} NO RENAME failed\r\n"));
        }
    }
}

// ---------------------------------------------------------------------------
// SUBSCRIBE / UNSUBSCRIBE
// ---------------------------------------------------------------------------

pub async fn subscribe_mailbox(tag: &str, mailbox: &str, store: &Store, session: &ImapSession) {
    let user = store.get_user();
    let clean_name = strip_surrounding_quotes(mailbox);
    if let Err(error) = set_mailbox_subscribed(&user.id, clean_name, true).await {
        logger::error_with("Error subscribing mailbox", json!({ "component": "imap", "mailbox": clean_name }), &error);
    }
    // Completes either way.
    session.write(&format!("{tag} OK SUBSCRIBE completed\r\n"));
}

pub async fn unsubscribe_mailbox(tag: &str, mailbox: &str, store: &Store, session: &ImapSession) {
    let user = store.get_user();
    let clean_name = strip_surrounding_quotes(mailbox);
    if let Err(error) = set_mailbox_subscribed(&user.id, clean_name, false).await {
        logger::error_with(
            "Error unsubscribing mailbox",
            json!({ "component": "imap", "mailbox": clean_name }),
            &error,
        );
    }
    session.write(&format!("{tag} OK UNSUBSCRIBE completed\r\n"));
}

// ---------------------------------------------------------------------------
// STATUS
// ---------------------------------------------------------------------------

pub async fn status_mailbox(tag: &str, mailbox_arg: &str, items: &[StatusItem], store: &Store, session: &ImapSession) {
    // RFC 3501 §5.1: INBOX is case-insensitive. Canonicalize so the answer
    // (`* STATUS "INBOX" ...`) names the mailbox the same way whatever
    // casing the client used.
    let mailbox = canonical_mailbox(mailbox_arg);
    if let Err(error) = status_of(tag, &mailbox, items, store, session).await {
        logger::error_with("Error getting mailbox status", json!({ "component": "imap", "mailbox": mailbox }), &error);
        session.write(&format!("{tag} NO STATUS failed\r\n"));
    }
}

async fn status_of(
    tag: &str,
    mailbox: &str,
    items: &[StatusItem],
    store: &Store,
    session: &ImapSession,
) -> anyhow::Result<()> {
    if !store.mailbox_exists(mailbox).await? {
        session.write(&format!("{tag} NO Mailbox does not exist\r\n"));
        return Ok(());
    }
    let Some(counts) = store.count_messages(mailbox).await else {
        session.write(&format!("{tag} NO Mailbox does not exist\r\n"));
        return Ok(());
    };

    // Each counter is its own read, so it is paid for only when asked.
    let uid_validity = match items.contains(&StatusItem::UidValidity) {
        true => Some(get_imap_uid_validity(&store.get_user().id).await?),
        false => None,
    };
    // RFC 4551 §3.1.2: one MAX(modseq) read, not one per item.
    let highest_modseq = match items.contains(&StatusItem::HighestModseq) {
        true => Some(store.get_highest_modseq(mailbox).await),
        false => None,
    };
    let uid_next = match items.contains(&StatusItem::UidNext) {
        true => Some(store.get_uid_next(mailbox).await?),
        false => None,
    };

    let mut status_items: Vec<String> = Vec::new();
    for item in items {
        let value = match item {
            StatusItem::Messages => counts.total,
            StatusItem::UidNext => uid_next.unwrap_or_default(),
            StatusItem::UidValidity => uid_validity.unwrap_or_default(),
            StatusItem::Unseen => counts.unread,
            StatusItem::Recent => 0,
            StatusItem::HighestModseq => highest_modseq.unwrap_or_default(),
        };
        status_items.push(item.as_str().to_string());
        status_items.push(value.to_string());
    }

    session.write(&format!("* STATUS {} ({})\r\n", quote_string(mailbox), status_items.join(" ")));
    session.write(&format!("{tag} OK STATUS completed\r\n"));
    Ok(())
}

// ---------------------------------------------------------------------------
// LIST / LSUB helpers
// ---------------------------------------------------------------------------

/// Every proper ancestor path of the given names: `Projects/Work/Q3`
/// contributes `Projects` and `Projects/Work`. Keyed on path segments, so a
/// `Project` that is merely a string prefix of `Projects/Work` is not its
/// parent. Built in one pass rather than per candidate, which would be
/// quadratic on an account with thousands of per-address boxes.
pub fn collect_ancestors<S: AsRef<str>>(names: &[S]) -> HashSet<String> {
    let mut ancestors = HashSet::new();
    for name in names {
        let parts: Vec<&str> = name.as_ref().split('/').collect();
        for end in 1..parts.len() {
            ancestors.insert(parts[..end].join("/"));
        }
    }
    ancestors
}

/// The attributes of one LIST/LSUB row. `parent_paths` is the ancestor set
/// of the whole listable set (`collect_ancestors`), built once per command.
///
/// RFC 5258 §3: `\HasChildren` / `\HasNoChildren` follows the names actually
/// listed. RFC 6154 §2: the special-use attribute travels with them in a
/// plain LIST, which is how a client maps a role to a box name.
pub fn get_mailbox_attributes(mailbox: &str, parent_paths: &HashSet<String>) -> String {
    let hierarchy = if parent_paths.contains(mailbox) { "\\HasChildren" } else { "\\HasNoChildren" };
    if let Some(utility) = utility_folder(mailbox) {
        return format!("{} {hierarchy}", utility.special_use);
    }
    if is_accounts_folder(mailbox) || is_sent_messages_accounts_folder(mailbox) {
        return format!("{hierarchy} \\Noselect");
    }
    hierarchy.to_string()
}

const STAR: u16 = b'*' as u16;
const PERCENT: u16 = b'%' as u16;
const SLASH: u16 = b'/' as u16;

/// Collapses each run of adjacent wildcards to the widest one it holds: a
/// run with any `*` spans the hierarchy delimiter, so it subsumes every `%`
/// beside it, and a run of `%` alone is no wider than one.
pub fn collapse_wildcard_runs(pattern: &str) -> String {
    String::from_utf16_lossy(&collapse_units(&pattern.encode_utf16().collect::<Vec<u16>>()))
}

fn collapse_units(pattern: &[u16]) -> Vec<u16> {
    let mut collapsed = Vec::with_capacity(pattern.len());
    let mut index = 0;
    while index < pattern.len() {
        let unit = pattern[index];
        if unit != STAR && unit != PERCENT {
            collapsed.push(unit);
            index += 1;
            continue;
        }
        let mut widest = unit;
        while index + 1 < pattern.len() && matches!(pattern[index + 1], STAR | PERCENT) {
            if pattern[index + 1] == STAR {
                widest = STAR;
            }
            index += 1;
        }
        collapsed.push(widest);
        index += 1;
    }
    collapsed
}

/// Sweeps the name one UTF-16 code unit at a time, carrying the set of
/// pattern positions that can still consume the prefix scanned so far. `*`
/// and `%` match any unit (`%` never the `/` delimiter) or nothing; any other
/// unit matches itself. The cost is the product of the two lengths, with no
/// backtracking.
fn is_reachable_by_pattern(source: &[u16], target: &[u16]) -> bool {
    let mut reachable = vec![false; source.len() + 1];
    let mut scratch = vec![false; source.len() + 1];
    reachable[0] = true;
    for (j, unit) in source.iter().enumerate() {
        if *unit != STAR && *unit != PERCENT {
            break;
        }
        reachable[j + 1] = true;
    }

    for consumed in target {
        let mut any_reachable = false;
        scratch[0] = false;
        for j in 1..=source.len() {
            let unit = source[j - 1];
            let position = match unit {
                STAR => scratch[j - 1] || reachable[j],
                PERCENT => scratch[j - 1] || (reachable[j] && *consumed != SLASH),
                _ => reachable[j - 1] && unit == *consumed,
            };
            scratch[j] = position;
            any_reachable |= position;
        }
        // An empty reachable set is absorbing.
        if !any_reachable {
            return false;
        }
        std::mem::swap(&mut reachable, &mut scratch);
    }
    reachable[source.len()]
}

/// A matcher for an IMAP LIST reference and pattern (RFC 3501 §6.3.8),
/// reused across every name of one response. Within `reference + pattern`,
/// `*` matches across the `/` delimiter and `%` within one level. INBOX is
/// matched case-insensitively (RFC 3501 §5.1); every other name strictly.
///
/// ```ignore
/// let matches = create_list_pattern_matcher("INBOX/", "%");
/// assert!(matches("INBOX/accounts"));
/// assert!(!matches("INBOX/accounts/work"));
/// ```
pub fn create_list_pattern_matcher(reference: &str, pattern: &str) -> impl Fn(&str) -> bool {
    let combined = format!("{reference}{pattern}");
    let collapsed = collapse_units(&combined.encode_utf16().collect::<Vec<u16>>());
    let folded_for_inbox = collapse_units(&combined.to_uppercase().encode_utf16().collect::<Vec<u16>>());
    move |mailbox: &str| {
        if is_inbox(mailbox) {
            is_reachable_by_pattern(&folded_for_inbox, &mailbox.to_uppercase().encode_utf16().collect::<Vec<u16>>())
        } else {
            is_reachable_by_pattern(&collapsed, &mailbox.encode_utf16().collect::<Vec<u16>>())
        }
    }
}

/// `Array.prototype.sort()` on strings: by UTF-16 code units.
fn js_string_order(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

// ---------------------------------------------------------------------------
// LIST / LSUB
// ---------------------------------------------------------------------------

pub async fn list_mailboxes(tag: &str, reference: &str, pattern: &str, store: &Store, session: &ImapSession) {
    // An empty pattern reads nothing.
    let boxes = if pattern.is_empty() { Vec::new() } else { store.list_mailboxes().await };
    for line in list_response(tag, reference, pattern, &boxes) {
        session.write(&line);
    }
}

/// The lines LIST answers with, given every listable mailbox.
pub fn list_response<S: AsRef<str>>(tag: &str, reference: &str, pattern: &str, boxes: &[S]) -> Vec<String> {
    // An empty pattern asks for the hierarchy delimiter and the root name of
    // the reference (RFC 3501 §6.3.8); no mailboxes are returned.
    if pattern.is_empty() {
        return vec![
            format!("* LIST (\\Noselect) \"/\" {}\r\n", quote_string(reference)),
            format!("{tag} OK LIST completed\r\n"),
        ];
    }
    // Ancestors come from the unfiltered set: a child the pattern excludes
    // still makes its parent \HasChildren.
    let parent_paths = collect_ancestors(boxes);
    let matches = create_list_pattern_matcher(reference, pattern);
    let mut lines: Vec<String> = boxes
        .iter()
        .map(AsRef::as_ref)
        .filter(|mailbox| matches(mailbox))
        .map(|mailbox| {
            let attributes = get_mailbox_attributes(mailbox, &parent_paths);
            format!("* LIST ({attributes}) \"/\" {}\r\n", quote_string(mailbox))
        })
        .collect();
    lines.push(format!("{tag} OK LIST completed\r\n"));
    lines
}

pub async fn list_subscribed_mailboxes(tag: &str, reference: &str, pattern: &str, store: &Store, session: &ImapSession) {
    // An empty pattern reads nothing.
    let entries = if pattern.is_empty() { Vec::new() } else { store.list_mailbox_entries().await };
    for line in lsub_response(tag, reference, pattern, entries) {
        session.write(&line);
    }
}

/// The lines LSUB answers with, given every listable mailbox and whether it
/// is subscribed.
pub fn lsub_response(tag: &str, reference: &str, pattern: &str, entries: Vec<MailboxEntry>) -> Vec<String> {
    if pattern.is_empty() {
        return vec![
            format!("* LSUB (\\Noselect) \"/\" {}\r\n", quote_string(reference)),
            format!("{tag} OK LSUB completed\r\n"),
        ];
    }
    let matches = create_list_pattern_matcher(reference, pattern);
    // Hierarchy is a property of the full listable set, not of the
    // subscribed subset.
    let all_boxes: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    let parent_paths = collect_ancestors(&all_boxes);

    let promote_ancestors = format!("{reference}{pattern}").contains('%');
    let ancestors_of_subscribed = if promote_ancestors {
        let subscribed: Vec<&str> =
            entries.iter().filter(|entry| entry.subscribed).map(|entry| entry.name.as_str()).collect();
        collect_ancestors(&subscribed)
    } else {
        HashSet::new()
    };

    // An ancestor need not exist as a mailbox of its own: CREATE makes no
    // superior names, so `Projects/Work` can be subscribed with no
    // `Projects` row, and RFC 3501 §6.3.9 marks such a name \Noselect.
    let listed: HashSet<&str> = all_boxes.iter().copied().collect();
    let mut synthesized: Vec<String> =
        ancestors_of_subscribed.iter().filter(|name| !listed.contains(name.as_str())).cloned().collect();
    synthesized.sort_by(|a, b| js_string_order(a, b));
    let synthesized = synthesized.into_iter().map(|name| MailboxEntry { name, subscribed: false });

    let mut lines = Vec::new();
    for entry in entries.iter().cloned().chain(synthesized) {
        if !(entry.subscribed || ancestors_of_subscribed.contains(&entry.name)) || !matches(&entry.name) {
            continue;
        }
        // An unsubscribed name is here only for a subscribed descendant.
        let attributes = if entry.subscribed {
            get_mailbox_attributes(&entry.name, &parent_paths)
        } else {
            "\\HasChildren \\Noselect".to_string()
        };
        lines.push(format!("* LSUB ({attributes}) \"/\" {}\r\n", quote_string(&entry.name)));
    }
    lines.push(format!("{tag} OK LSUB completed\r\n"));
    lines
}

// ---------------------------------------------------------------------------
// SELECT / EXAMINE
// ---------------------------------------------------------------------------

pub async fn select_mailbox(tag: &str, name: &str, read_only: bool, command: &str, store: &Store, session: &ImapSession) {
    let unquoted = strip_surrounding_quotes(name);

    if unquoted.is_empty() {
        session.write(&format!("{tag} NO Empty mailbox name\r\n"));
        return;
    }

    // RFC 3501 §5.1: INBOX is case-insensitive. Canonicalize so the selected
    // mailbox kept on the session, and every later response, say "INBOX"
    // rather than whatever casing the client sent.
    let clean_name = canonical_mailbox(unquoted);

    if is_accounts_folder(&clean_name) {
        session.write(&format!("{tag} NO [CANNOT] {ACCOUNTS_FOLDER} is not selectable\r\n"));
        return;
    }

    if is_sent_messages_accounts_folder(&clean_name) {
        session.write(&format!("{tag} NO [CANNOT] {SENT_MESSAGES_ACCOUNTS_FOLDER} is not selectable\r\n"));
        return;
    }

    if let Err(error) = select_from_store(tag, &clean_name, read_only, command, store, session).await {
        // RFC 3501 §6.3.1: after a failed SELECT no mailbox is selected.
        session.set_selected(None, 0);
        session.clear_seq_state();
        logger::error_with("Error selecting mailbox", json!({ "component": "imap", "name": name }), &error);
        session.write(&format!("{tag} NO SELECT failed\r\n"));
    }
}

/// The store half of SELECT. Every failure exit leaves no mailbox selected
/// (RFC 3501 §6.3.1); an `Err` is answered `NO SELECT failed`.
async fn select_from_store(
    tag: &str,
    mailbox: &str,
    read_only: bool,
    command: &str,
    store: &Store,
    session: &ImapSession,
) -> anyhow::Result<()> {
    let refuse = |answer: &str| {
        session.set_selected(None, 0);
        session.clear_seq_state();
        session.write(&format!("{tag} NO {answer}\r\n"));
    };

    if !store.mailbox_exists(mailbox).await? {
        refuse("Mailbox does not exist");
        return Ok(());
    }

    session.set_selected(Some(mailbox), 0);

    let read = match store.get_all_uids(mailbox).await {
        Some(uids) => MailboxUids::Uids(uids),
        None => MailboxUids::Unreadable,
    };
    if !build_sequence_mapping(&mut session.state().seq_state, read) {
        refuse("Failed to read mailbox");
        return Ok(());
    }

    let Some(counts) = store.count_messages(mailbox).await else {
        refuse("Mailbox does not exist");
        return Ok(());
    };
    let total = counts.total;
    session.set_selected(Some(mailbox), usize::try_from(total).unwrap_or(0));

    // Everything the untagged block needs is read before its first line: a
    // fault after `* EXISTS` would leave half a SELECT response on the wire.
    // [UNSEEN n] is the sequence number of the first unseen message (RFC
    // 3501 §7.1), omitted when all are read. UIDNEXT comes from the UID
    // counter, which never goes down (RFC 3501 §2.3.1.1).
    let user_id = store.get_user().id.clone();
    let (uid_validity, first_unseen_uid, uid_next, highest_modseq) = tokio::join!(
        get_imap_uid_validity(&user_id),
        store.get_first_unseen_uid(mailbox),
        store.get_uid_next(mailbox),
        store.get_highest_modseq(mailbox),
    );
    let uid_validity = uid_validity?;
    let uid_next = uid_next?;
    let first_unseen_seq =
        first_unseen_uid.and_then(|uid| session.state().seq_state.uid_to_seq.get(&uid).copied()).filter(|seq| *seq > 0);

    session.write(&format!("* {total} EXISTS\r\n"));
    session.write("* 0 RECENT\r\n");
    if let Some(seq) = first_unseen_seq {
        session.write(&format!("* OK [UNSEEN {seq}] Message {seq} is first unseen\r\n"));
    }
    session.write(&format!("* OK [UIDVALIDITY {uid_validity}] UIDs valid\r\n"));
    session.write(&format!("* OK [UIDNEXT {uid_next}] Predicted next UID\r\n"));
    session.write(&format!("* OK [HIGHESTMODSEQ {highest_modseq}] Highest mod-sequence\r\n"));
    session.write("* FLAGS (\\Seen \\Flagged \\Deleted \\Draft \\Answered)\r\n");
    session.write("* OK [PERMANENTFLAGS (\\Seen \\Flagged \\Deleted \\Draft \\Answered \\*)] Flags permitted\r\n");
    // RFC 3501 §6.3.1: the code says whether the client may change the
    // mailbox, which is not which command it sent: a read-only user's
    // SELECT is answered `[READ-ONLY] SELECT completed`.
    let mode = if read_only { "READ-ONLY" } else { "READ-WRITE" };
    session.write(&format!("{tag} OK [{mode}] {command} completed\r\n"));
    Ok(())
}

#[cfg(test)]
#[path = "mailbox_ops_tests.rs"]
mod tests;
