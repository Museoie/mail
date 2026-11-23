//! The mailboxes and messages of one user, as the IMAP session reads and
//! writes them (imap/store.ts).
//!
//! A [`Store`] takes mailbox names as the client wrote them, after
//! `canonical_mailbox`, and turns each into what the mail repository is keyed
//! on: the view (`INBOX`, the unified `Sent Messages`, a utility folder) or
//! the mapped box, and the counter that assigns its UIDs.
//!
//! ```ignore
//! let store = Store::new(user, StoreScope::whole_store(&primary_domain));
//! if store.mailbox_exists("Archive").await? {
//!     let uids = store.get_all_uids("Archive").await;
//!     let mails = store.get_messages("Archive", 1.0, 50.0, &["uid", "read", "subject"], true, None).await;
//! }
//! ```
//!
//! Reads that the Bun server answers with a fallback on a database fault
//! (an empty list, `INBOX` alone) do the same here; `mailbox_exists`,
//! `get_uid_next` and `expunge_uids` report theirs, because their callers
//! must not mistake a fault for an answer.

use std::collections::HashSet;

use anyhow::Result;
use futures_util::future::{BoxFuture, FutureExt};
use serde_json::{json, Map, Value};

use crate::imap::types::{FetchMailInput, MailAddress, MailUid as FetchMailUid, SearchCriterion};
use crate::imap::util::{
    account_to_box, account_to_sent_box, box_to_account, is_domain_scoped, is_inbox, is_sent_box, js_trim,
    utility_folder, utility_placement, UtilityUidSpace, ACCOUNTS_FOLDER, SENT_MESSAGES_ACCOUNTS_FOLDER,
    SENT_MESSAGES_FOLDER, UTILITY_FOLDERS,
};
use crate::models::SignedUser;
use crate::postgres::repositories::mailboxes::get_mailboxes_by_user;
use crate::postgres::repositories::mails::core::{save_mail, Placement, SaveMailInput};
use crate::postgres::repositories::mails::counters::{get_highest_modseq, get_uid_next, UidScope};
use crate::postgres::repositories::mails::imap::{
    count_messages, expunge_deleted_mails, expunge_mails_by_uid, get_all_uids, get_first_unseen_uid,
    get_mail_flag_updates, get_mails_by_range, search_mails_by_uid, set_mail_flags,
};
use crate::postgres::repositories::mails::imap_query::{DateKey, SizeKey, TextKey};
use crate::postgres::repositories::mails::model::PartialMailModel;
use crate::postgres::repositories::mails::table::{run_query, PreparedQuery};
use crate::postgres::repositories::mails::views::domain_view_for_destination;
use crate::postgres::DbResult;
use crate::util::get_random_id;
use crate::{js, logger};

pub use crate::postgres::repositories::mails::imap::{
    MailFlagUpdate, MessageCounts, SetMailFlagsResult, StoreOperationType, UpdatedMailFlags,
};
pub use crate::postgres::repositories::mails::imap_query::SimplifiedCriterion;

// ---------------------------------------------------------------------------
// Mails
// ---------------------------------------------------------------------------

/// An address header: the list of mailboxes it names, as the JSONB column
/// holds it, and its text as received.
#[derive(Debug, Clone, PartialEq)]
pub struct Address {
    pub value: Value,
    pub text: String,
}

/// The UIDs a mail holds: `domain` in the user's domain-wide sequence
/// (`INBOX`, the unified `Sent Messages`, `Drafts`, `Junk`), `account` in a
/// mapped box. `account` is 0 for a mail read from a domain view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MailUid {
    pub domain: i64,
    pub account: i64,
}

/// A mail as [`Store::get_messages`] returns it: the fields that were asked
/// for, and no others.
///
/// `text_octets` and `html_octets` are `octet_length()` of the body columns.
/// A caller that asks for them with `mail_id` and `user_id`, and not for
/// `text` and `html`, can stream the bodies out of PostgreSQL with
/// `pg_text_chunks` instead of loading them.
///
/// Address lists, attachments and insight are the JSON documents their
/// columns hold, so that a COPY stores what it read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreFetchedMail {
    pub message_id: Option<String>,
    pub subject: Option<String>,
    /// An ISO timestamp.
    pub date: Option<String>,
    pub html: Option<String>,
    pub text: Option<String>,
    pub read: Option<bool>,
    pub saved: Option<bool>,
    pub sent: Option<bool>,
    pub deleted: Option<bool>,
    pub draft: Option<bool>,
    pub answered: Option<bool>,
    pub text_octets: Option<i64>,
    pub html_octets: Option<i64>,
    pub mail_id: Option<String>,
    pub user_id: Option<String>,
    pub uid: Option<MailUid>,
    pub modseq: Option<i64>,
    /// The cached size of the RFC 822 form. `None`: not asked for.
    /// `Some(None)`: asked for and not computed yet, so the FETCH derives and
    /// persists it.
    pub rfc822_size: Option<Option<i64>>,
    pub from: Option<Address>,
    pub to: Option<Address>,
    pub cc: Option<Address>,
    pub bcc: Option<Address>,
    pub reply_to: Option<Address>,
    pub envelope_from: Option<Value>,
    pub envelope_to: Option<Value>,
    pub attachments: Option<Value>,
    pub insight: Option<Value>,
}

/// The messages of a range read, keyed by `mail_id`, in ascending UID order.
pub type FetchedMails = Vec<(String, StoreFetchedMail)>;

fn address_json(address: &Address) -> Value {
    json!({ "value": address.value, "text": address.text })
}

impl StoreFetchedMail {
    /// The mail as the Bun server's object, with its field names: a field
    /// that was not read is absent.
    pub fn to_json(&self) -> Value {
        let mut mail = Map::new();
        let mut set = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                mail.insert(key.to_string(), value);
            }
        };
        set("messageId", self.message_id.as_deref().map(Value::from));
        set("subject", self.subject.as_deref().map(Value::from));
        set("date", self.date.as_deref().map(Value::from));
        set("html", self.html.as_deref().map(Value::from));
        set("text", self.text.as_deref().map(Value::from));
        set("read", self.read.map(Value::from));
        set("saved", self.saved.map(Value::from));
        set("sent", self.sent.map(Value::from));
        set("deleted", self.deleted.map(Value::from));
        set("draft", self.draft.map(Value::from));
        set("answered", self.answered.map(Value::from));
        set("text_octets", self.text_octets.map(Value::from));
        set("html_octets", self.html_octets.map(Value::from));
        set("mail_id", self.mail_id.as_deref().map(Value::from));
        set("user_id", self.user_id.as_deref().map(Value::from));
        set("uid", self.uid.map(|uid| json!({ "domain": uid.domain, "account": uid.account })));
        set("modseq", self.modseq.map(Value::from));
        set("rfc822_size", self.rfc822_size.map(|size| size.map_or(Value::Null, Value::from)));
        set("from", self.from.as_ref().map(address_json));
        set("to", self.to.as_ref().map(address_json));
        set("cc", self.cc.as_ref().map(address_json));
        set("bcc", self.bcc.as_ref().map(address_json));
        set("replyTo", self.reply_to.as_ref().map(address_json));
        set("envelopeFrom", self.envelope_from.clone());
        set("envelopeTo", self.envelope_to.clone());
        set("attachments", self.attachments.clone());
        set("insight", self.insight.clone());
        Value::Object(mail)
    }

    /// The mail as the FETCH formatters read it. An address list or an
    /// attachment list that is not the shape they expect reads as absent.
    pub fn to_fetch_input(&self) -> FetchMailInput {
        let address = |address: &Option<Address>| {
            address.as_ref().map(|address| MailAddress {
                value: serde_json::from_value(address.value.clone()).ok(),
                text: Some(address.text.clone()),
            })
        };
        FetchMailInput {
            attachments: self.attachments.as_ref().and_then(|list| serde_json::from_value(list.clone()).ok()),
            from: address(&self.from),
            to: address(&self.to),
            cc: address(&self.cc),
            bcc: address(&self.bcc),
            reply_to: address(&self.reply_to),
            date: self.date.clone(),
            html: self.html.clone(),
            text: self.text.clone(),
            subject: self.subject.clone(),
            message_id: self.message_id.clone(),
            read: self.read,
            saved: self.saved,
            deleted: self.deleted,
            draft: self.draft,
            answered: self.answered,
            uid: self.uid.map(|uid| FetchMailUid { domain: uid.domain as f64, account: uid.account as f64 }),
            modseq: self.modseq.map(|modseq| modseq as f64),
            rfc822_size: self.rfc822_size.flatten().map(|size| size as f64),
            text_octets: self.text_octets.map(|octets| octets as f64),
            html_octets: self.html_octets.map(|octets| octets as f64),
            mail_id: self.mail_id.clone(),
            user_id: self.user_id.clone(),
        }
    }
}

/// The repository's row as the mail the session reads.
fn fetched_mail(model: &PartialMailModel) -> StoreFetchedMail {
    let text = |value: Option<&str>| value.map(str::to_string);
    let address = |value: Option<&Value>, text: Option<&str>| {
        value.map(|value| Address { value: value.clone(), text: text.unwrap_or_default().to_string() })
    };
    StoreFetchedMail {
        message_id: text(model.message_id()),
        subject: text(model.subject()),
        date: text(model.date()),
        html: text(model.html()),
        text: text(model.text()),
        read: model.read(),
        saved: model.saved(),
        sent: model.sent(),
        deleted: model.deleted(),
        draft: model.draft(),
        answered: model.answered(),
        text_octets: model.text_octets(),
        html_octets: model.html_octets(),
        mail_id: text(model.mail_id()),
        user_id: text(model.user_id()),
        // A domain view reads no per-box UID; 0 says "use the domain UID".
        uid: model.uid_domain().map(|domain| MailUid { domain, account: model.uid_mailbox().unwrap_or(0) }),
        modseq: model.modseq(),
        rfc822_size: model.get("rfc822_size").map(|_| model.rfc822_size()),
        from: address(model.from_address(), model.from_text()),
        to: address(model.to_address(), model.to_text()),
        cc: address(model.cc_address(), model.cc_text()),
        bcc: address(model.bcc_address(), model.bcc_text()),
        reply_to: address(model.reply_to_address(), model.reply_to_text()),
        envelope_from: model.envelope_from().cloned(),
        envelope_to: model.envelope_to().cloned(),
        attachments: model.attachments().cloned(),
        insight: model.insight().cloned(),
    }
}

/// The columns a field of the mail is read from; a name that is not a mail
/// field is taken to be a column already.
fn map_field_name(field: &str) -> Vec<&str> {
    match field {
        "messageId" => vec!["message_id"],
        "uid" => vec!["uid_domain", "uid_mailbox"],
        "from" => vec!["from_address", "from_text"],
        "to" => vec!["to_address", "to_text"],
        "cc" => vec!["cc_address", "cc_text"],
        "bcc" => vec!["bcc_address", "bcc_text"],
        "replyTo" => vec!["reply_to_address", "reply_to_text"],
        other => vec![other],
    }
}

/// A mail to store: what an APPEND parsed, or the copy a COPY or MOVE files.
/// The fields are the shared mail model's, and [`Mail::default`] gives its
/// defaults: no recipients, no attachments, the current time, a random
/// Message-ID.
#[derive(Debug, Clone, PartialEq)]
pub struct Mail {
    /// The attachment list, as stored.
    pub attachments: Option<Value>,
    pub from: Option<Address>,
    pub to: Option<Address>,
    pub cc: Option<Address>,
    pub bcc: Option<Address>,
    pub reply_to: Option<Address>,
    pub envelope_from: Option<Value>,
    pub envelope_to: Value,
    /// An ISO timestamp.
    pub date: String,
    pub html: String,
    pub text: String,
    pub subject: String,
    pub message_id: String,
    pub read: bool,
    pub saved: bool,
    pub sent: bool,
    pub deleted: bool,
    pub draft: bool,
    pub answered: bool,
    pub insight: Option<Value>,
    /// The UIDs reserved for the mail. [`Store::store_mail`] replaces them
    /// with the ones the stored row holds.
    pub uid: MailUid,
    /// The Message-ID this mail answers, for threading.
    pub in_reply_to: Option<String>,
    /// The conversation's Message-IDs, oldest first, for threading.
    pub references: Option<Vec<String>>,
}

impl Default for Mail {
    fn default() -> Mail {
        Mail {
            attachments: Some(json!([])),
            from: None,
            to: None,
            cc: None,
            bcc: None,
            reply_to: None,
            envelope_from: None,
            envelope_to: json!([]),
            date: js::now_iso(),
            html: String::new(),
            text: String::new(),
            subject: String::new(),
            message_id: get_random_id(),
            read: false,
            saved: false,
            sent: false,
            deleted: false,
            draft: false,
            answered: false,
            insight: None,
            uid: MailUid::default(),
            in_reply_to: None,
            references: None,
        }
    }
}

// ---------------------------------------------------------------------------
// SEARCH
// ---------------------------------------------------------------------------

/// The key as the statement builder reads it.
fn simplify_key(key: &SearchCriterion) -> SimplifiedCriterion {
    let text = |key: TextKey, value: &str| SimplifiedCriterion::Text { key, value: value.to_string() };
    let date = |key: DateKey, value| SimplifiedCriterion::Date { key, value };
    match key {
        SearchCriterion::Subject(value) => text(TextKey::Subject, value),
        SearchCriterion::From(value) => text(TextKey::From, value),
        SearchCriterion::To(value) => text(TextKey::To, value),
        SearchCriterion::Cc(value) => text(TextKey::Cc, value),
        SearchCriterion::Bcc(value) => text(TextKey::Bcc, value),
        SearchCriterion::Body(value) => text(TextKey::Body, value),
        SearchCriterion::Text(value) => text(TextKey::Text, value),
        SearchCriterion::Header { field, value } => {
            SimplifiedCriterion::Header { field: field.clone(), text: value.clone() }
        }
        SearchCriterion::Before(value) => date(DateKey::Before, *value),
        SearchCriterion::On(value) => date(DateKey::On, *value),
        SearchCriterion::Since(value) => date(DateKey::Since, *value),
        SearchCriterion::SentBefore(value) => date(DateKey::SentBefore, *value),
        SearchCriterion::SentOn(value) => date(DateKey::SentOn, *value),
        SearchCriterion::SentSince(value) => date(DateKey::SentSince, *value),
        SearchCriterion::Larger(size) => SimplifiedCriterion::Size { key: SizeKey::Larger, value: *size },
        SearchCriterion::Smaller(size) => SimplifiedCriterion::Size { key: SizeKey::Smaller, value: *size },
        SearchCriterion::Uid(set) => SimplifiedCriterion::UidSet(set.ranges.clone()),
        // The flags, and the keywords: the server stores no custom keywords,
        // so the builder needs only the type to know that KEYWORD matches
        // nothing and UNKEYWORD everything.
        SearchCriterion::All
        | SearchCriterion::Unseen
        | SearchCriterion::Seen
        | SearchCriterion::Answered
        | SearchCriterion::Unanswered
        | SearchCriterion::Deleted
        | SearchCriterion::Undeleted
        | SearchCriterion::Flagged
        | SearchCriterion::Unflagged
        | SearchCriterion::Draft
        | SearchCriterion::Undraft
        | SearchCriterion::New
        | SearchCriterion::Old
        | SearchCriterion::Recent
        | SearchCriterion::Keyword(_)
        | SearchCriterion::Unkeyword(_) => SimplifiedCriterion::bare(key.type_name()),
        // Anything else is kept by its type, which the builder fails closed
        // on: dropping it would widen the search to every message.
        other => {
            logger::warn("Unsupported search criterion", json!({ "component": "imap.store", "type": other.type_name() }));
            SimplifiedCriterion::bare(other.type_name())
        }
    }
}

/// A parsed SEARCH criterion in the shape the statement builder reads, its
/// `NOT` and `OR` operands included.
///
/// A bare sequence set (`SEQ`) is not a key the store can evaluate: it names
/// sequence numbers, which only the session can resolve. One that reaches
/// here matches nothing.
pub fn simplify_criterion(criterion: &SearchCriterion) -> SimplifiedCriterion {
    criterion.fold(&mut simplify_key, &mut SimplifiedCriterion::not, &mut SimplifiedCriterion::or)
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/// The statement behind the account list: the HTTP repository's, for the
/// received and sent sides (never the spam view).
fn build_account_stats_query(
    user_id: &str,
    sent: bool,
    domain_filter: &[String],
    scope_domains: Option<&[String]>,
) -> PreparedQuery {
    crate::postgres::repositories::mails::http_query::build_account_stats_query(
        user_id,
        sent,
        Some(domain_filter),
        false,
        scope_domains,
    )
}

/// The addresses of the account list, newest mail first. A database fault
/// reads as no accounts.
async fn get_account_addresses(
    user_id: &str,
    sent: bool,
    domain_filter: &[String],
    scope_domains: Option<&[String]>,
) -> Vec<String> {
    match run_query(&build_account_stats_query(user_id, sent, domain_filter, scope_domains)).await {
        Ok(result) => result
            .rows
            .iter()
            .map(|row| row.get("address").and_then(Value::as_str).unwrap_or_default().to_string())
            .collect(),
        Err(error) => {
            logger::error_with("Failed to get account stats", json!({}), &error);
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// A mailbox in the listing, with its LSUB state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxEntry {
    pub name: String,
    pub subscribed: bool,
}

/// The view or mapped box a mailbox name reads from: the repository's
/// mailbox argument, `None` for the two views with no name of their own, and
/// whether it holds sent mail.
fn resolve_mapped_box(mailbox: &str) -> (Option<&str>, bool) {
    let mailbox_arg = if is_inbox(mailbox) || mailbox == SENT_MESSAGES_FOLDER { None } else { Some(mailbox) };
    (mailbox_arg, is_sent_box(mailbox))
}

/// Which of a store's mail an IMAP session sees.
///
/// - `domains` `None`: the whole store, every domain its user owns (a plain
///   `user` login).
/// - `domains` set: only the mail of those domains, in every box (a
///   `user@domain` login, whose store is the domain owner's).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoreScope {
    pub domains: Option<Vec<String>>,
    /// The domains whose addresses LIST shows as account folders: the
    /// scope's domains, or the user's primary domain, read at login.
    pub account_domains: Vec<String>,
    /// The domain account folders (named by local part) are read under.
    pub primary_domain: String,
}

impl StoreScope {
    /// A plain login's scope when its primary domain is `domain`.
    pub fn whole_store(domain: &str) -> StoreScope {
        StoreScope { domains: None, account_domains: vec![domain.to_string()], primary_domain: domain.to_string() }
    }
}

/// One user's mailboxes and messages.
#[derive(Debug, Clone, PartialEq)]
pub struct Store {
    user: SignedUser,
    scope: StoreScope,
}

impl Store {
    /// `user` is the account whose rows the session reads: the owner of its mail.
    pub fn new(user: SignedUser, scope: StoreScope) -> Store {
        Store { user, scope }
    }

    /// The user this store belongs to.
    pub fn get_user(&self) -> &SignedUser {
        &self.user
    }

    /// The domains the session is scoped to, or `None` for the whole store.
    pub fn get_domains(&self) -> Option<&[String]> {
        self.scope.domains.as_deref()
    }

    /// Whether the session sees only some domains of the store.
    pub fn is_domain_scoped(&self) -> bool {
        self.scope.domains.is_some()
    }

    pub fn get_primary_domain(&self) -> &str {
        &self.scope.primary_domain
    }

    /// The counter row that assigns a mailbox's UIDs. The three answers are
    /// the three reservations the write path makes (`get_domain_uid_next`,
    /// `get_mailbox_uid_next`, `get_account_uid_next`), so a box is never
    /// read from a counter nothing writes.
    fn resolve_uid_scope(&self, mailbox: &str) -> UidScope {
        if is_domain_scoped(mailbox) {
            return UidScope::Domain { sent: is_sent_box(mailbox) };
        }
        match utility_folder(mailbox) {
            Some(utility) if utility.uid_space == UtilityUidSpace::Mapped => {
                UidScope::Mailbox { mailbox: utility.name.to_string() }
            }
            _ => UidScope::Account {
                account: box_to_account(&self.scope.primary_domain, mailbox),
                sent: is_sent_box(mailbox),
            },
        }
    }

    async fn list_mailbox_entries_or_throw(&self) -> Result<Vec<MailboxEntry>> {
        // Only addresses on the session's domains are accounts: the mail
        // also names every outside address it was copied to. A scoped session
        // also counts only its domains' mail, and has no user-created
        // mailboxes: those hold the whole store's mail.
        let domains = &self.scope.account_domains;
        let scope = self.scope.domains.as_deref();
        let (received, sent, user_mailboxes) = tokio::join!(
            get_account_addresses(&self.user.id, false, domains, scope),
            get_account_addresses(&self.user.id, true, domains, scope),
            async {
                match scope {
                    Some(_) => Ok(Vec::new()),
                    None => get_mailboxes_by_user(&self.user.id).await,
                }
            },
        );
        let user_mailboxes = user_mailboxes?;

        let mut mailboxes: Vec<MailboxEntry> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut add_mailbox = |name: &str| {
            let trimmed = js_trim(name);
            if seen.insert(trimmed.to_string()) {
                mailboxes.push(MailboxEntry { name: trimmed.to_string(), subscribed: true });
            }
        };

        add_mailbox("INBOX");
        // Utility folders are listed whether or not they hold mail: a client
        // needs `Drafts` to exist before it has a draft to APPEND, and finds
        // a role (RFC 6154) only on a listed box. They come before the
        // user-created boxes, so a user box of the same name gives way.
        for folder in &UTILITY_FOLDERS {
            add_mailbox(folder.name);
        }
        if !sent.is_empty() {
            add_mailbox(SENT_MESSAGES_FOLDER);
        }
        if !received.is_empty() {
            add_mailbox(ACCOUNTS_FOLDER);
        }
        for address in received.iter().filter(|address| !address.is_empty()) {
            add_mailbox(&account_to_box(address));
        }
        if !sent.is_empty() {
            add_mailbox(SENT_MESSAGES_ACCOUNTS_FOLDER);
        }
        for address in sent.iter().filter(|address| !address.is_empty()) {
            add_mailbox(&account_to_sent_box(address));
        }

        // User-created boxes: rows with no role and no address behind them.
        let system_names: HashSet<String> = mailboxes.iter().map(|mailbox| mailbox.name.to_lowercase()).collect();
        for mailbox in user_mailboxes {
            if mailbox.special_use.is_none()
                && mailbox.address.is_none()
                && !system_names.contains(&mailbox.name.to_lowercase())
            {
                mailboxes.push(MailboxEntry { name: mailbox.name, subscribed: mailbox.subscribed });
            }
        }
        Ok(mailboxes)
    }

    async fn list_mailboxes_or_throw(&self) -> Result<Vec<String>> {
        Ok(self.list_mailbox_entries_or_throw().await?.into_iter().map(|entry| entry.name).collect())
    }

    /// Every mailbox name, for LIST: `INBOX`, the utility folders, the
    /// account trees the user's mail gives rise to, then the user-created
    /// boxes, subscribed or not. On a database fault the list is `INBOX`
    /// alone, so that LIST can still answer.
    pub async fn list_mailboxes(&self) -> Vec<String> {
        match self.list_mailboxes_or_throw().await {
            Ok(mailboxes) => mailboxes,
            Err(error) => {
                logger::error_with("Error listing mailboxes", json!({ "component": "imap.store" }), &error);
                vec!["INBOX".to_string()]
            }
        }
    }

    /// The same list with each box's subscription, for LSUB. The boxes the
    /// server derives are always subscribed: they have no row to unsubscribe.
    pub async fn list_mailbox_entries(&self) -> Vec<MailboxEntry> {
        match self.list_mailbox_entries_or_throw().await {
            Ok(entries) => entries,
            Err(error) => {
                logger::error_with(
                    "Error listing mailboxes with subscription state",
                    json!({ "component": "imap.store" }),
                    &error,
                );
                vec![MailboxEntry { name: "INBOX".to_string(), subscribed: true }]
            }
        }
    }

    /// Whether `mailbox` is `INBOX` (in any case) or a listed mailbox,
    /// spelled as listed. Fails on a database fault instead of falling back
    /// to the `INBOX`-only list, which would report every other box missing.
    pub async fn mailbox_exists(&self, mailbox: &str) -> Result<bool> {
        if is_inbox(mailbox) {
            return Ok(true);
        }
        Ok(self.list_mailboxes_or_throw().await?.iter().any(|name| name == mailbox))
    }

    /// The message and unread counts of a mailbox.
    ///
    /// `None` is what the Bun server answers when the repository fails. The
    /// repository absorbs its own faults and reports an empty mailbox, so
    /// this is always `Some`; the shape is kept for the session, which
    /// handles both.
    pub async fn count_messages(&self, mailbox: &str) -> Option<MessageCounts> {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        Some(count_messages(&self.user.id, mailbox_arg, is_sent, self.scope.domains.as_deref()).await)
    }

    /// UIDNEXT of a mailbox, read from the counter that assigns its UIDs.
    /// Fails on a database fault: a made-up UIDNEXT would promise a UID that
    /// has already been handed out.
    pub async fn get_uid_next(&self, mailbox: &str) -> DbResult<i64> {
        get_uid_next(&self.user.id, &self.resolve_uid_scope(mailbox)).await
    }

    /// Every UID of a mailbox, ascending: the sequence-number map.
    ///
    /// `None` stands for a failed read, as distinct from an empty mailbox.
    /// The repository absorbs its own faults and reports an empty mailbox,
    /// so this is always `Some`, as it is for the Bun server.
    pub async fn get_all_uids(&self, mailbox: &str) -> Option<Vec<i64>> {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        Some(get_all_uids(&self.user.id, mailbox_arg, is_sent, self.scope.domains.as_deref()).await)
    }

    /// The UID and flags of the named mails as `mailbox` shows them, for
    /// pushing an untagged `FETCH (FLAGS …)` to idling sessions. A mail the
    /// mailbox does not hold is left out. Always `Some`, like
    /// [`Store::get_all_uids`].
    pub async fn get_mail_flag_updates(&self, mailbox: &str, mail_ids: &[String]) -> Option<Vec<MailFlagUpdate>> {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        Some(get_mail_flag_updates(&self.user.id, mailbox_arg, is_sent, mail_ids, self.scope.domains.as_deref()).await)
    }

    /// The UID of the first unread message of a mailbox, or `None` when all
    /// are read (or the read failed).
    pub async fn get_first_unseen_uid(&self, mailbox: &str) -> Option<i64> {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        get_first_unseen_uid(&self.user.id, mailbox_arg, is_sent, self.scope.domains.as_deref()).await
    }

    /// HIGHESTMODSEQ of a mailbox (RFC 4551 §3.1.1); 1, the floor every
    /// message starts at, when the read fails.
    pub async fn get_highest_modseq(&self, mailbox: &str) -> i64 {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        get_highest_modseq(&self.user.id, mailbox_arg, is_sent, self.scope.domains.as_deref()).await
    }

    /// The messages `start..=end` of a mailbox, by UID when `use_uid` and by
    /// sequence number otherwise, with the `fields` asked for: the mail's
    /// field names (`messageId`, `uid`, `from`, …) or column names
    /// (`text_octets`, `mail_id`, `modseq`, …). With `changed_since`, only
    /// those whose mod-sequence exceeds it.
    ///
    /// The read starts when this is called. A database fault reads as no
    /// messages.
    pub fn get_messages<S: AsRef<str>>(
        &self,
        mailbox: &str,
        start: f64,
        end: f64,
        fields: &[S],
        use_uid: bool,
        changed_since: Option<f64>,
    ) -> BoxFuture<'static, FetchedMails> {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        let columns: Vec<&str> = fields.iter().flat_map(|field| map_field_name(field.as_ref())).collect();
        let read = get_mails_by_range(
            &self.user.id,
            mailbox_arg,
            is_sent,
            start,
            end,
            use_uid,
            &columns,
            changed_since,
            self.scope.domains.as_deref(),
        );
        async move { read.await.iter().map(|(mail_id, model)| (mail_id.clone(), fetched_mail(model))).collect() }.boxed()
    }

    /// Applies a STORE to the messages `start..=end` of a mailbox; see
    /// `set_mail_flags`. A database fault reads as a STORE that addressed no
    /// message.
    #[allow(clippy::too_many_arguments)]
    pub fn set_flags<S: AsRef<str>>(
        &self,
        mailbox: &str,
        start: f64,
        end: f64,
        flags: &[S],
        use_uid: bool,
        operation: StoreOperationType,
        unchanged_since: Option<f64>,
    ) -> BoxFuture<'static, SetMailFlagsResult> {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        set_mail_flags(
            &self.user.id,
            mailbox_arg,
            is_sent,
            start,
            end,
            flags,
            use_uid,
            operation,
            unchanged_since,
            self.scope.domains.as_deref(),
        )
    }

    /// EXPUNGE: removes the `\Deleted` messages of a mailbox and returns
    /// their UIDs.
    ///
    /// The repository absorbs a database fault and reports nothing expunged,
    /// so this is always `Ok`, as the Bun server's never throws; the shape is
    /// kept for the session.
    pub async fn expunge(&self, mailbox: &str) -> DbResult<Vec<i64>> {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        Ok(expunge_deleted_mails(&self.user.id, mailbox_arg, is_sent, self.scope.domains.as_deref()).await)
    }

    /// Removes the named UIDs from a mailbox without touching `\Deleted`, for
    /// MOVE (RFC 6851 §3.3). Fails on a database fault, so that the MOVE is
    /// answered `NO` rather than `OK` with a COPYUID for a removal that did
    /// not happen.
    pub async fn expunge_uids(&self, mailbox: &str, uids: &[i64]) -> DbResult<Vec<i64>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        expunge_mails_by_uid(&self.user.id, mailbox_arg, is_sent, uids, self.scope.domains.as_deref()).await
    }

    /// The UIDs of the messages of a mailbox that match every criterion,
    /// ascending. Sequence-number keys must have been resolved to UID keys
    /// by the session. A database fault reads as no match.
    pub async fn search(&self, mailbox: &str, criteria: &[SearchCriterion]) -> Vec<i64> {
        let (mailbox_arg, is_sent) = resolve_mapped_box(mailbox);
        let simplified: Vec<SimplifiedCriterion> = criteria.iter().map(simplify_criterion).collect();
        search_mails_by_uid(&self.user.id, mailbox_arg, is_sent, &simplified, self.scope.domains.as_deref()).await
    }

    /// What `store_mail` asks the repository to save.
    fn save_mail_input(&self, mail: &Mail, destination: Option<&str>) -> SaveMailInput {
        let destination = destination.filter(|name| !name.is_empty());
        let address = |address: &Option<Address>| address.as_ref().map(|address| address.value.clone());
        let text = |address: &Option<Address>| address.as_ref().map(|address| address.text.clone());
        SaveMailInput {
            user_id: self.user.id.clone(),
            message_id: mail.message_id.clone(),
            // An APPEND sets these from the message's own headers. A copy
            // leaves them unset: it is the same message filed elsewhere under
            // a derived id that the conversation lookup leaves out.
            in_reply_to: mail.in_reply_to.clone(),
            reference_ids: mail.references.clone().filter(|references| !references.is_empty()),
            subject: Some(mail.subject.clone()),
            date: Some(mail.date.clone()),
            html: Some(mail.html.clone()),
            text: Some(mail.text.clone()),
            from_address: address(&mail.from),
            from_text: text(&mail.from),
            to_address: address(&mail.to),
            to_text: text(&mail.to),
            cc_address: address(&mail.cc),
            cc_text: text(&mail.cc),
            bcc_address: address(&mail.bcc),
            bcc_text: text(&mail.bcc),
            reply_to_address: address(&mail.reply_to),
            reply_to_text: text(&mail.reply_to),
            envelope_from: mail.envelope_from.clone(),
            envelope_to: Some(mail.envelope_to.clone()),
            attachments: mail.attachments.clone(),
            read: mail.read,
            saved: mail.saved,
            sent: mail.sent,
            deleted: mail.deleted,
            draft: mail.draft,
            answered: mail.answered,
            expunged: false,
            insight: mail.insight.clone(),
            uid_domain: Some(mail.uid.domain),
            uid_mailbox: Some(mail.uid.account),
            spam_score: None,
            spam_reasons: None,
            is_spam: false,
            placement: destination.and_then(utility_placement).map(|placement| Placement {
                draft: placement.draft,
                is_spam: placement.is_spam,
                saved: placement.saved,
                deleted: placement.deleted,
            }),
            mailbox: destination.filter(|name| !is_domain_scoped(name)).map(str::to_string),
            domain_mailbox: destination.and_then(domain_view_for_destination).map(str::to_string),
        }
    }

    /// Stores a mail, filed by `destination`: the box a COPY, MOVE or APPEND
    /// names. Returns whether it was stored, and on success sets `mail.uid`
    /// to the UIDs the stored row holds, which are the earlier ones when the
    /// Message-ID was already filed there.
    ///
    /// The destination decides two things.
    ///
    /// - **Mapping.** Only a mapped destination gets a `mail_mailbox_uid` row
    ///   carrying `mail.uid.account`. Separately, a destination in the INBOX
    ///   tree or a utility view records a row under `INBOX` holding
    ///   `mail.uid.domain`: `APPEND INBOX` records one row, and a COPY into
    ///   `INBOX/accounts/<name>` both.
    /// - **Placement.** A utility folder selects its messages by a flag, so a
    ///   write that names one sets that flag, whatever flags the client sent:
    ///   an APPEND to `Drafts` without `\Draft` is still a draft.
    pub async fn store_mail(&self, mail: &mut Mail, destination: Option<&str>) -> bool {
        match save_mail(&self.save_mail_input(mail, destination)).await {
            Ok(Some(saved)) => {
                if let Some(uid) = saved.uid_mailbox {
                    mail.uid.account = uid;
                }
                if let Some(uid) = saved.uid_domain {
                    mail.uid.domain = uid;
                }
                true
            }
            Ok(None) => false,
            Err(error) => {
                logger::error_with("Error storing mail", json!({ "component": "imap.store" }), &error);
                false
            }
        }
    }
}

/// The golden replay of `tests/golden/imapdb/`: this port against what the
/// Bun server's statement builders, repository and store do.
#[cfg(test)]
#[path = "../../tests/golden/imapdb/replay.rs"]
mod golden;

#[cfg(test)]
mod tests {
    use crate::postgres::Param;
    use super::*;
    use crate::imap::types::{SequenceRange, SequenceSet};
    use crate::imap::util::JsDate;
    use crate::postgres::repositories::mails::imap::testing::with_inline_reads;
    use crate::postgres::repositories::mails::imap_query::{
        build_all_uids_query, build_first_unseen_uid_query, build_mails_by_range_query,
    };
    use crate::postgres::repositories::mails::table::script::{fault, rows, with_script};
    use crate::util::get_domain;

    const USER_ID: &str = "11111111-1111-4111-8111-111111111111";

    fn user(username: &str) -> SignedUser {
        SignedUser::new(Some(USER_ID.to_string()), Some(username.to_string()), Some(format!("{username}@example.com")))
    }

    /// The store a plain login opens: the user's whole store, listing the
    /// user's own domain, as `auth.rs` resolves it.
    fn store_of(username: &str) -> Store {
        let domain = crate::domains::account_domain(username);
        Store::new(user(username), StoreScope::whole_store(&domain))
    }

    fn store() -> Store {
        store_of("alice")
    }

    fn no_rows() -> crate::postgres::DbResult<crate::postgres::repositories::mails::table::Rows> {
        rows(json!([]))
    }

    fn addresses(list: &[&str]) -> crate::postgres::DbResult<crate::postgres::repositories::mails::table::Rows> {
        rows(Value::Array(list.iter().map(|address| json!({ "address": address })).collect()))
    }

    fn mailbox_row(name: &str, subscribed: bool) -> Value {
        json!({
            "mailbox_id": format!("mailbox-{name}"),
            "user_id": USER_ID,
            "name": name,
            "address": null,
            "parent_id": null,
            "uid_validity": 1,
            "uid_next": 1,
            "subscribed": subscribed,
            "special_use": null,
            "created": "2026-08-20T00:00:00.000Z",
        })
    }

    const SERVER_BOXES: [&str; 5] = ["INBOX", "Drafts", "Junk", "Starred", "Trash"];

    // store.test.ts — "Store.listMailboxes"

    #[tokio::test]
    async fn asks_for_the_accounts_of_the_users_own_domain_received_and_sent() {
        let (_, trace) = with_script(vec![no_rows(), no_rows(), no_rows()], store().list_mailboxes()).await;
        assert_eq!(trace.len(), 3);
        let domain = vec![format!("alice.{}", get_domain())];
        assert_eq!(trace[0].values, vec![Param::from(USER_ID), Param::array(&domain)]);
        assert_eq!(trace[1].values, vec![Param::from(USER_ID), Param::array(&domain)]);
        assert!(trace[0].sql.contains("COALESCE(envelope_to, '[]'::jsonb)"));
        assert!(trace[1].sql.contains("jsonb_array_elements(from_address)->>'address' as address"));
        assert!(trace[0].sql.contains("= ANY($2::text[])"));
        assert!(trace[2].sql.contains("FROM mailboxes"));
    }

    #[tokio::test]
    async fn lists_the_primary_domain_it_was_opened_with() {
        let admin = Store::new(user("admin"), StoreScope::whole_store(&get_domain()));
        let (_, trace) = with_script(vec![no_rows(), no_rows(), no_rows()], admin.list_mailboxes()).await;
        assert_eq!(trace[0].values, vec![Param::from(USER_ID), Param::array(&[get_domain()])]);
        assert_eq!(trace[1].values, vec![Param::from(USER_ID), Param::array(&[get_domain()])]);
    }

    #[tokio::test]
    async fn a_scoped_store_counts_only_its_domains_and_lists_no_user_mailboxes() {
        let domains = vec!["example.org".to_string()];
        let scoped = Store::new(
            user("alice"),
            StoreScope {
                domains: Some(domains.clone()),
                account_domains: domains.clone(),
                primary_domain: "example.org".into(),
            },
        );
        let (boxes, trace) = with_script(vec![no_rows(), no_rows()], scoped.list_mailboxes()).await;
        assert_eq!(boxes, SERVER_BOXES);
        assert_eq!(trace.len(), 2, "no mailboxes read");
        assert_eq!(trace[0].values, vec![Param::from(USER_ID), Param::array(&domains), Param::array(&domains)]);
        assert!(scoped.is_domain_scoped());
        assert_eq!(scoped.get_primary_domain(), "example.org");
    }

    #[tokio::test]
    async fn returns_only_the_server_defined_boxes_when_there_are_no_accounts_and_no_user_mailboxes() {
        let (boxes, _) = with_script(vec![no_rows(), no_rows(), no_rows()], store().list_mailboxes()).await;
        assert_eq!(boxes, SERVER_BOXES);
    }

    #[tokio::test]
    async fn lists_the_account_trees_then_the_user_mailboxes() {
        let (boxes, _) = with_script(
            vec![
                addresses(&["work@alice.example", "home @alice.example", "", "work@alice.example"]),
                addresses(&["me@alice.example"]),
                rows(json!([
                    mailbox_row("Archive", true),
                    // A row whose name a server-defined box already takes, in any case.
                    mailbox_row("trash", true),
                    mailbox_row("inbox/accounts/WORK", true),
                    mailbox_row(" Padded ", true),
                    json!({ "mailbox_id": "m1", "user_id": USER_ID, "name": "Role", "address": null, "parent_id": null, "uid_validity": 1, "uid_next": 1, "subscribed": true, "special_use": "\\Archive", "created": null }),
                    json!({ "mailbox_id": "m2", "user_id": USER_ID, "name": "Tied", "address": "tied@alice.example", "parent_id": null, "uid_validity": 1, "uid_next": 1, "subscribed": true, "special_use": null, "created": null }),
                    mailbox_row("Archive", false),
                ])),
            ],
            store().list_mailboxes(),
        )
        .await;
        assert_eq!(
            boxes,
            vec![
                "INBOX",
                "Drafts",
                "Junk",
                "Starred",
                "Trash",
                "Sent Messages",
                "INBOX/accounts",
                "INBOX/accounts/work",
                "INBOX/accounts/home",
                "Sent Messages/accounts",
                "Sent Messages/accounts/me",
                // The repository hands the rows over in collation order.
                " Padded ",
                "Archive",
                "Archive",
            ]
        );
    }

    // store.test.ts — "Store.storeMail — what the destination box decides (#725)"

    fn mail() -> Mail {
        Mail { message_id: "m-1".to_string(), subject: "s".to_string(), uid: MailUid { domain: 7, account: 3 }, ..Mail::default() }
    }

    fn saved_input(mail: &Mail, destination: &str) -> SaveMailInput {
        store().save_mail_input(mail, Some(destination))
    }

    #[test]
    fn sets_the_membership_flag_for_a_utility_destination() {
        let input = saved_input(&mail(), "Drafts");
        assert_eq!(input.placement, Some(Placement { draft: Some(true), ..Placement::default() }));
    }

    #[test]
    fn records_no_mapping_row_for_a_utility_destination() {
        let input = saved_input(&mail(), "Junk");
        assert_eq!(input.mailbox, None);
        assert_eq!(input.placement, Some(Placement { is_spam: Some(true), ..Placement::default() }));
        assert_eq!(input.domain_mailbox.as_deref(), Some("INBOX"));
    }

    #[test]
    fn records_the_mapping_row_for_a_mapped_destination_and_touches_no_flag() {
        let input = saved_input(&mail(), "INBOX/accounts/alice");
        assert_eq!(input.mailbox.as_deref(), Some("INBOX/accounts/alice"));
        assert_eq!(input.placement, None);
        assert_eq!(input.domain_mailbox.as_deref(), Some("INBOX"));
    }

    #[test]
    fn saves_an_appended_mails_threading_headers() {
        let appended = Mail {
            in_reply_to: Some("<a@x>".to_string()),
            references: Some(vec!["<root@x>".to_string(), "<a@x>".to_string()]),
            ..mail()
        };
        let input = saved_input(&appended, "INBOX");
        assert_eq!(input.in_reply_to.as_deref(), Some("<a@x>"));
        assert_eq!(input.reference_ids, Some(vec!["<root@x>".to_string(), "<a@x>".to_string()]));
    }

    #[test]
    fn saves_no_threading_headers_for_a_mail_that_carries_none_such_as_a_copy() {
        let input = saved_input(&mail(), "INBOX");
        assert_eq!(input.in_reply_to, None);
        assert_eq!(input.reference_ids, None);
        let input = saved_input(&Mail { references: Some(Vec::new()), ..mail() }, "INBOX");
        assert_eq!(input.reference_ids, None);
    }

    #[test]
    fn records_no_mapping_row_for_inbox_or_the_unified_sent_view() {
        assert_eq!(saved_input(&mail(), "INBOX").mailbox, None);
        let sent = saved_input(&mail(), "Sent Messages");
        assert_eq!(sent.mailbox, None);
        assert_eq!(sent.domain_mailbox, None);
    }

    #[test]
    fn files_into_a_mapped_utility_folder_with_its_flag_and_its_own_mapping() {
        let input = saved_input(&mail(), "Trash");
        assert_eq!(input.mailbox.as_deref(), Some("Trash"));
        assert_eq!(input.placement, Some(Placement { deleted: Some(true), ..Placement::default() }));
        assert_eq!(input.domain_mailbox, None);
        let input = saved_input(&mail(), "Archive");
        assert_eq!((input.mailbox.as_deref(), input.domain_mailbox.as_deref()), (Some("Archive"), None));
    }

    #[test]
    fn a_mail_with_no_destination_is_filed_nowhere() {
        for destination in [None, Some("")] {
            let input = store().save_mail_input(&mail(), destination);
            assert_eq!((input.mailbox, input.domain_mailbox, input.placement), (None, None, None));
        }
    }

    #[test]
    fn carries_the_mail_into_the_save() {
        let filed = Mail {
            from: Some(Address { value: json!([{ "address": "a@x", "name": "A", "extra": 1 }]), text: "A <a@x>".to_string() }),
            cc: Some(Address { value: json!([]), text: "kept".to_string() }),
            envelope_to: json!([{ "address": "b@x" }]),
            html: "<p>h</p>".to_string(),
            read: true,
            sent: true,
            insight: Some(json!({ "summary": "s" })),
            date: "2024-03-04T05:06:07.000Z".to_string(),
            ..mail()
        };
        let input = saved_input(&filed, "Archive");
        assert_eq!(input.user_id, USER_ID);
        assert_eq!(input.message_id, "m-1");
        assert_eq!(input.from_address, Some(json!([{ "address": "a@x", "name": "A", "extra": 1 }])));
        assert_eq!(input.from_text.as_deref(), Some("A <a@x>"));
        assert_eq!((input.to_address.clone(), input.to_text.clone()), (None, None));
        assert_eq!((input.cc_address.clone(), input.cc_text.as_deref()), (Some(json!([])), Some("kept")));
        assert_eq!(input.envelope_to, Some(json!([{ "address": "b@x" }])));
        assert_eq!(input.attachments, Some(json!([])));
        assert_eq!(input.date.as_deref(), Some("2024-03-04T05:06:07.000Z"));
        assert_eq!((input.read, input.sent, input.saved, input.is_spam), (true, true, false, false));
        assert_eq!((input.uid_domain, input.uid_mailbox), (Some(7), Some(3)));
        assert_eq!(input.insight, Some(json!({ "summary": "s" })));
    }

    #[test]
    fn a_new_mail_starts_from_the_models_defaults() {
        let (first, second) = (Mail::default(), Mail::default());
        assert_eq!(first.attachments, Some(json!([])));
        assert_eq!(first.envelope_to, json!([]));
        assert_eq!(first.uid, MailUid { domain: 0, account: 0 });
        assert_eq!(first.message_id.len(), 8);
        assert_ne!(first.message_id, second.message_id);
        assert!(JsDate::parse(&first.date).is_valid());
    }

    #[tokio::test]
    async fn lists_an_unsubscribed_user_mailbox_list_is_not_subscription_filtered() {
        let (boxes, _) = with_script(
            vec![no_rows(), no_rows(), rows(json!([mailbox_row("Archive", false)]))],
            store().list_mailboxes(),
        )
        .await;
        assert_eq!(boxes, ["INBOX", "Drafts", "Junk", "Starred", "Trash", "Archive"]);
    }

    // store.test.ts — "Store.listMailboxEntries — subscription state for LSUB (#688)"

    #[tokio::test]
    async fn carries_each_user_mailboxs_subscribed_column_through() {
        let (entries, _) = with_script(
            vec![no_rows(), no_rows(), rows(json!([mailbox_row("ZzSubYes", true), mailbox_row("ZzSubNo", false)]))],
            store().list_mailbox_entries(),
        )
        .await;
        assert!(entries.contains(&MailboxEntry { name: "ZzSubYes".to_string(), subscribed: true }));
        assert!(entries.contains(&MailboxEntry { name: "ZzSubNo".to_string(), subscribed: false }));
    }

    #[tokio::test]
    async fn marks_the_derived_mailboxes_subscribed() {
        let (entries, _) = with_script(
            vec![addresses(&["work@alice.example.com"]), no_rows(), no_rows()],
            store().list_mailbox_entries(),
        )
        .await;
        assert!(entries.len() > 1);
        assert!(entries.iter().all(|entry| entry.subscribed));
        assert!(entries.iter().any(|entry| entry.name == "INBOX"));
        assert!(entries.iter().any(|entry| entry.name == "INBOX/accounts/work"));
    }

    #[tokio::test]
    async fn falls_back_to_a_subscribed_inbox_when_the_backend_fails() {
        let (entries, _) = with_script(vec![no_rows(), no_rows(), fault("08006")], store().list_mailbox_entries()).await;
        assert_eq!(entries, vec![MailboxEntry { name: "INBOX".to_string(), subscribed: true }]);
    }

    #[tokio::test]
    async fn a_failed_account_read_lists_no_accounts_and_is_not_a_failure() {
        let (entries, _) = with_script(
            vec![fault("08006"), fault("08006"), rows(json!([mailbox_row("Archive", true)]))],
            store().list_mailbox_entries(),
        )
        .await;
        assert_eq!(entries.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), [
            "INBOX", "Drafts", "Junk", "Starred", "Trash", "Archive"
        ]);
    }

    // store.test.ts — "simplifyCriterion — NOT/OR operand normalisation (regression for #551)"

    fn text(key: TextKey, value: &str) -> SimplifiedCriterion {
        SimplifiedCriterion::text(key, value)
    }

    fn uid(ranges: Vec<SequenceRange>) -> SearchCriterion {
        SearchCriterion::Uid(SequenceSet::new(ranges))
    }

    #[test]
    fn normalises_nots_inner_criterion() {
        assert_eq!(
            simplify_criterion(&SearchCriterion::not(SearchCriterion::Seen)),
            SimplifiedCriterion::not(SimplifiedCriterion::bare("SEEN"))
        );
    }

    #[test]
    fn normalises_nots_inner_text_criterion() {
        assert_eq!(
            simplify_criterion(&SearchCriterion::not(SearchCriterion::From("spam@x".into()))),
            SimplifiedCriterion::not(text(TextKey::From, "spam@x"))
        );
    }

    #[test]
    fn normalises_a_not_before_date() {
        let when = JsDate::parse("2026-01-01T00:00:00Z");
        assert_eq!(
            simplify_criterion(&SearchCriterion::not(SearchCriterion::Before(when))),
            SimplifiedCriterion::not(SimplifiedCriterion::Date { key: DateKey::Before, value: when })
        );
    }

    #[test]
    fn normalises_both_or_operands_recursively() {
        assert_eq!(
            simplify_criterion(&SearchCriterion::or(SearchCriterion::From("alice".into()), SearchCriterion::To("bob".into()))),
            SimplifiedCriterion::or(text(TextKey::From, "alice"), text(TextKey::To, "bob"))
        );
    }

    #[test]
    fn normalises_a_multi_element_uid_set_into_one_uid_set_carrying_every_range() {
        let ranges = vec![SequenceRange::single(1.0), SequenceRange::range(3.0, 5.0)];
        assert_eq!(simplify_criterion(&uid(ranges.clone())), SimplifiedCriterion::UidSet(ranges));
    }

    #[test]
    fn normalises_a_nested_uid_operand_of_an_or_into_a_uid_set() {
        let ranges = vec![SequenceRange::single(1.0), SequenceRange::single(3.0)];
        assert_eq!(
            simplify_criterion(&SearchCriterion::or(SearchCriterion::From("alice".into()), uid(ranges.clone()))),
            SimplifiedCriterion::or(text(TextKey::From, "alice"), SimplifiedCriterion::UidSet(ranges))
        );
    }

    #[test]
    fn normalises_headers_field_and_value() {
        assert_eq!(
            simplify_criterion(&SearchCriterion::Header { field: "Subject".into(), value: "hi".into() }),
            SimplifiedCriterion::Header { field: "Subject".into(), text: "hi".into() }
        );
    }

    #[test]
    fn keeps_every_other_key_under_its_own_type() {
        for (criterion, kind) in [
            (SearchCriterion::All, "ALL"),
            (SearchCriterion::Unseen, "UNSEEN"),
            (SearchCriterion::Answered, "ANSWERED"),
            (SearchCriterion::Unanswered, "UNANSWERED"),
            (SearchCriterion::Deleted, "DELETED"),
            (SearchCriterion::Undeleted, "UNDELETED"),
            (SearchCriterion::Flagged, "FLAGGED"),
            (SearchCriterion::Unflagged, "UNFLAGGED"),
            (SearchCriterion::Draft, "DRAFT"),
            (SearchCriterion::Undraft, "UNDRAFT"),
            (SearchCriterion::New, "NEW"),
            (SearchCriterion::Old, "OLD"),
            (SearchCriterion::Recent, "RECENT"),
        ] {
            assert_eq!(simplify_criterion(&criterion), SimplifiedCriterion::bare(kind));
        }
        assert_eq!(simplify_criterion(&SearchCriterion::Text("t".into())), text(TextKey::Text, "t"));
        assert_eq!(simplify_criterion(&SearchCriterion::Body("b".into())), text(TextKey::Body, "b"));
        assert_eq!(simplify_criterion(&SearchCriterion::Cc("c".into())), text(TextKey::Cc, "c"));
        assert_eq!(simplify_criterion(&SearchCriterion::Bcc("c".into())), text(TextKey::Bcc, "c"));
        assert_eq!(simplify_criterion(&SearchCriterion::Subject("s".into())), text(TextKey::Subject, "s"));
        assert_eq!(
            simplify_criterion(&SearchCriterion::Larger(5.0)),
            SimplifiedCriterion::Size { key: SizeKey::Larger, value: 5.0 }
        );
        assert_eq!(
            simplify_criterion(&SearchCriterion::Smaller(9.0)),
            SimplifiedCriterion::Size { key: SizeKey::Smaller, value: 9.0 }
        );
        let day = JsDate::parse("2026-02-03T00:00:00Z");
        for (criterion, key) in [
            (SearchCriterion::On(day), DateKey::On),
            (SearchCriterion::Since(day), DateKey::Since),
            (SearchCriterion::SentBefore(day), DateKey::SentBefore),
            (SearchCriterion::SentOn(day), DateKey::SentOn),
            (SearchCriterion::SentSince(day), DateKey::SentSince),
        ] {
            assert_eq!(simplify_criterion(&criterion), SimplifiedCriterion::Date { key, value: day });
        }
    }

    // store.test.ts — "simplifyCriterion — unexpressible criteria are preserved, not dropped (#672)"

    #[test]
    fn preserves_keyword_and_unkeyword_as_a_bare_type() {
        assert_eq!(simplify_criterion(&SearchCriterion::Keyword("Foo".into())), SimplifiedCriterion::bare("KEYWORD"));
        assert_eq!(simplify_criterion(&SearchCriterion::Unkeyword("Foo".into())), SimplifiedCriterion::bare("UNKEYWORD"));
    }

    #[test]
    fn preserves_a_criterion_the_store_cannot_evaluate_instead_of_dropping_it() {
        let sequence = SearchCriterion::Seq(SequenceSet::new(vec![SequenceRange::single(1.0)]));
        assert_eq!(simplify_criterion(&sequence), SimplifiedCriterion::bare("SEQ"));
        assert_eq!(
            simplify_criterion(&SearchCriterion::not(sequence)),
            SimplifiedCriterion::not(SimplifiedCriterion::bare("SEQ"))
        );
    }

    #[tokio::test]
    async fn a_deep_search_is_simplified_and_reduced_without_recursion() {
        let mut criterion = SearchCriterion::All;
        for _ in 0..100_001 {
            criterion = SearchCriterion::not(criterion);
        }
        // An odd number of NOTs over ALL matches nothing.
        let (uids, trace) = with_script(vec![no_rows()], store().search("INBOX", &[criterion])).await;
        assert!(uids.is_empty());
        assert!(trace[0].sql.contains("AND m.draft = FALSE AND FALSE\n"));
        assert_eq!(trace[0].values, vec![Param::from(USER_ID), Param::from(false)]);
    }

    // store.test.ts — "Store.getFirstUnseenUid"

    #[tokio::test]
    async fn forwards_the_resolved_mailbox_for_inbox_and_returns_the_unseen_uid() {
        let (uid, trace) = with_script(vec![rows(json!([{ "uid": 42 }]))], store().get_first_unseen_uid("INBOX")).await;
        assert_eq!(uid, Some(42));
        assert_eq!(trace, vec![build_first_unseen_uid_query(USER_ID, None, false, None)]);
    }

    #[tokio::test]
    async fn returns_none_when_every_message_is_read() {
        let (uid, _) = with_script(vec![no_rows()], store().get_first_unseen_uid("INBOX")).await;
        assert_eq!(uid, None);
    }

    #[tokio::test]
    async fn returns_none_instead_of_failing_when_the_query_fails() {
        let (uid, _) = with_script(vec![fault("08006")], store().get_first_unseen_uid("INBOX")).await;
        assert_eq!(uid, None);
    }

    // store.test.ts — "Store.getMessages — replyTo mapping (#667)"

    fn model(fields: &[&str], row: Value) -> PartialMailModel {
        PartialMailModel::from_value(fields, &row).expect("the row is valid")
    }

    #[test]
    fn maps_reply_to_address_and_reply_to_text_onto_the_mails_reply_to() {
        let reply_to = json!([{ "address": "noreply@vendor.example", "name": "Vendor" }]);
        let mail = fetched_mail(&model(
            &["mail_id", "message_id", "from_address", "from_text", "reply_to_address", "reply_to_text"],
            json!({
                "mail_id": "doc-1",
                "message_id": "<m1>",
                "from_address": [{ "address": "sender@vendor.example", "name": "Vendor" }],
                "from_text": "Vendor <sender@vendor.example>",
                "reply_to_address": reply_to,
                "reply_to_text": "Vendor <noreply@vendor.example>",
            }),
        ));
        assert_eq!(mail.reply_to, Some(Address { value: reply_to, text: "Vendor <noreply@vendor.example>".to_string() }));
        assert_eq!(mail.to_json()["replyTo"]["text"], "Vendor <noreply@vendor.example>");
    }

    #[test]
    fn leaves_reply_to_unset_when_the_column_is_absent() {
        let mail = fetched_mail(&model(
            &["mail_id", "message_id", "from_address"],
            json!({ "mail_id": "doc-2", "message_id": "<m2>", "from_address": [] }),
        ));
        assert_eq!(mail.reply_to, None);
        // An address list with no text reads as an empty text.
        assert_eq!(mail.from, Some(Address { value: json!([]), text: String::new() }));
        assert_eq!(mail.to_json(), json!({ "messageId": "<m2>", "mail_id": "doc-2", "from": { "value": [], "text": "" } }));
    }

    #[test]
    fn a_fetched_mail_carries_what_was_read_and_nothing_else() {
        let fields = [
            "mail_id", "user_id", "message_id", "subject", "date", "read", "saved", "sent", "deleted", "draft",
            "answered", "uid_domain", "uid_mailbox", "modseq", "rfc822_size", "text_octets", "html_octets",
            "to_address", "to_text", "cc_address", "cc_text", "envelope_from", "envelope_to", "attachments", "insight",
        ];
        let mail = fetched_mail(&model(
            &fields,
            json!({
                "mail_id": "doc-3", "user_id": USER_ID, "message_id": "<m3>", "subject": "", "date": "2024-03-04T05:06:07.000Z",
                "read": true, "saved": false, "sent": false, "deleted": false, "draft": false, "answered": true,
                "uid_domain": 9, "uid_mailbox": 4, "modseq": 12, "rfc822_size": null, "text_octets": 5, "html_octets": 0,
                "to_address": [{ "address": "a@x" }], "to_text": null, "cc_address": null, "cc_text": "ignored",
                "envelope_from": null, "envelope_to": [{ "address": "a@x" }],
                "attachments": [{ "content": { "data": "id" }, "contentType": "text/plain", "filename": "a.txt", "size": 3 }],
                "insight": { "summary": "s" },
            }),
        ));
        assert_eq!(
            mail.to_json(),
            json!({
                "messageId": "<m3>", "subject": "", "date": "2024-03-04T05:06:07.000Z",
                "read": true, "saved": false, "sent": false, "deleted": false, "draft": false, "answered": true,
                "text_octets": 5, "html_octets": 0, "mail_id": "doc-3", "user_id": USER_ID,
                "uid": { "domain": 9, "account": 4 }, "modseq": 12, "rfc822_size": null,
                "to": { "value": [{ "address": "a@x" }], "text": "" },
                "envelopeTo": [{ "address": "a@x" }],
                "attachments": [{ "content": { "data": "id" }, "contentType": "text/plain", "filename": "a.txt", "size": 3 }],
                "insight": { "summary": "s" },
            })
        );
        assert_eq!((mail.html.as_deref(), mail.text.as_deref()), (None, None));
        assert_eq!(mail.rfc822_size, Some(None));

        let input = mail.to_fetch_input();
        assert_eq!(input.message_id.as_deref(), Some("<m3>"));
        assert_eq!(input.uid.map(|uid| (uid.domain, uid.account)), Some((9.0, 4.0)));
        assert_eq!((input.text_octets, input.html_octets, input.rfc822_size), (Some(5.0), Some(0.0), None));
        assert_eq!(input.to.as_ref().and_then(|to| to.value.as_ref()).map(Vec::len), Some(1));
        assert_eq!(input.attachments.as_ref().map(Vec::len), Some(1));
        assert_eq!(input.mail_id.as_deref(), Some("doc-3"));

        // A domain view reads no per-box UID.
        let domain = fetched_mail(&model(&["mail_id", "uid_domain"], json!({ "mail_id": "doc-4", "uid_domain": 9 })));
        assert_eq!(domain.uid, Some(MailUid { domain: 9, account: 0 }));
        assert_eq!(domain.rfc822_size, None);
    }

    #[tokio::test]
    async fn reads_the_columns_behind_the_fields_asked_for() {
        let store = store();
        let (mails, trace) = with_inline_reads(with_script(
            vec![rows(json!([
                { "mail_id": "doc-1", "message_id": "<m1>", "uid_domain": 5, "uid_mailbox": 2, "from_address": null, "from_text": null, "read": false },
            ]))],
            async {
                store.get_messages("Archive", 2.0, 8.0, &["messageId", "uid", "from", "read"], true, Some(3.0)).await
            },
        ))
        .await;
        let expected = build_mails_by_range_query(
            USER_ID,
            Some("Archive"),
            false,
            2.0,
            8.0,
            true,
            &["message_id", "uid_domain", "uid_mailbox", "from_address", "from_text", "read"],
            Some(3.0),
            None,
        );
        assert_eq!((trace[0].sql.as_str(), &trace[0].values), (expected.sql.as_str(), &expected.values));
        assert_eq!(mails.len(), 1);
        assert_eq!(mails[0].0, "doc-1");
        assert_eq!(
            mails[0].1.to_json(),
            json!({ "messageId": "<m1>", "read": false, "mail_id": "doc-1", "uid": { "domain": 5, "account": 2 } })
        );
    }

    // store.test.ts — "Store.getAllUids"

    #[tokio::test]
    async fn returns_the_repositorys_uids() {
        let (uids, trace) =
            with_script(vec![rows(json!([{ "uid": 4 }, { "uid": 8 }, { "uid": 15 }]))], store().get_all_uids("INBOX")).await;
        assert_eq!(uids, Some(vec![4, 8, 15]));
        assert_eq!(trace, vec![build_all_uids_query(USER_ID, None, false, None)]);
    }

    // The Bun test injects a failing repository and expects `null`. The
    // repository itself never fails: it absorbs the fault, so a failed read
    // and an empty mailbox are the same answer here as they are there.
    #[tokio::test]
    async fn a_failed_query_reads_as_an_empty_mailbox_because_the_repository_absorbs_it() {
        let (uids, _) = with_script(vec![fault("08006")], store().get_all_uids("INBOX")).await;
        assert_eq!(uids, Some(Vec::new()));
        let (counts, _) = with_script(vec![fault("08006")], store().count_messages("INBOX")).await;
        assert_eq!(counts, Some(MessageCounts::default()));
        let (expunged, _) = with_script(vec![fault("08006")], store().expunge("INBOX")).await;
        assert_eq!(expunged.unwrap(), Vec::<i64>::new());
    }

    // store.test.ts — "Store.getUidNext — counter key axis"

    fn scope_for(mailbox: &str) -> UidScope {
        store().resolve_uid_scope(mailbox)
    }

    #[test]
    fn reads_the_domain_counter_for_every_box_whose_uids_come_from_the_domain_sequence() {
        for (mailbox, sent) in [("INBOX", false), ("Sent Messages", true), ("Drafts", false), ("Junk", false)] {
            assert_eq!(scope_for(mailbox), UidScope::Domain { sent });
        }
    }

    #[test]
    fn reads_the_per_mailbox_counter_for_a_mapped_utility_box_with_no_sent_axis() {
        assert_eq!(scope_for("Starred"), UidScope::Mailbox { mailbox: "Starred".to_string() });
        assert_eq!(scope_for("Trash"), UidScope::Mailbox { mailbox: "Trash".to_string() });
    }

    #[test]
    fn canonicalizes_a_mapped_utility_box_before_using_it_as_the_counter_scope() {
        assert_eq!(scope_for("starred"), UidScope::Mailbox { mailbox: "Starred".to_string() });
    }

    #[test]
    fn reads_the_per_account_counter_keyed_by_address_not_the_box_path() {
        let domain = format!("alice.{}", get_domain());
        assert_eq!(
            scope_for("INBOX/accounts/bob"),
            UidScope::Account { account: format!("bob@{domain}"), sent: false }
        );
        assert_eq!(
            scope_for("Sent Messages/accounts/bob"),
            UidScope::Account { account: format!("bob@{domain}"), sent: true }
        );
        assert_eq!(scope_for("Archive"), UidScope::Account { account: format!("Archive@{domain}"), sent: false });
    }

    #[test]
    fn never_passes_a_raw_box_path_as_an_account_scope() {
        for mailbox in ["INBOX/accounts/bob", "Sent Messages/accounts/bob", "Archive"] {
            let UidScope::Account { account, .. } = scope_for(mailbox) else {
                panic!("{mailbox} reads an account counter");
            };
            assert_ne!(account, mailbox);
            assert!(account.contains('@'));
        }
    }

    #[test]
    fn gives_every_declared_utility_folder_the_uid_space_its_declaration_names() {
        for folder in &UTILITY_FOLDERS {
            match (scope_for(folder.name), folder.uid_space) {
                (UidScope::Domain { .. }, UtilityUidSpace::Domain) | (UidScope::Mailbox { .. }, UtilityUidSpace::Mapped) => {}
                (scope, _) => panic!("{} reads {scope:?}", folder.name),
            }
        }
    }

    #[tokio::test]
    async fn uid_next_propagates_a_repository_failure_instead_of_substituting_a_floor() {
        let (uid_next, _) = with_script(vec![fault("08006")], store().get_uid_next("INBOX")).await;
        assert_eq!(uid_next.unwrap_err().code.as_deref(), Some("08006"));
        let (uid_next, trace) = with_script(vec![rows(json!([{ "uid_next": 17 }]))], store().get_uid_next("Trash")).await;
        assert_eq!(uid_next.unwrap(), 17);
        assert_eq!(trace[0].values[1..4], [Param::from("mailbox"), Param::from("Trash"), Param::from(false)]);
    }

    // mailbox-existence.test.ts — "Store.mailboxExists (#595)"
    // mailbox-exists-transient.test.ts — "Store.mailboxExists propagates transient errors (#601)"
    // inbox-case-insensitive.test.ts — "Store.mailboxExists is case-insensitive for INBOX (#600)"

    fn listing() -> Vec<crate::postgres::DbResult<crate::postgres::repositories::mails::table::Rows>> {
        vec![no_rows(), addresses(&["me@alice.example"]), rows(json!([mailbox_row("Archive", true)]))]
    }

    #[tokio::test]
    async fn exists_for_inbox_in_any_case_without_consulting_the_list() {
        for name in ["INBOX", "inbox", "Inbox"] {
            // An empty script fails the test on any statement.
            let (exists, trace) = with_script(vec![], store().mailbox_exists(name)).await;
            assert!(exists.unwrap());
            assert!(trace.is_empty());
        }
    }

    #[tokio::test]
    async fn exists_for_a_listed_mailbox() {
        for name in ["Sent Messages", "Archive", "Drafts", "Sent Messages/accounts/me"] {
            let (exists, _) = with_script(listing(), store().mailbox_exists(name)).await;
            assert!(exists.unwrap(), "{name}");
        }
    }

    #[tokio::test]
    async fn does_not_exist_for_an_unlisted_name_or_another_spelling() {
        for name in ["GarbageBox", "archive", "drafts", "INBOX/accounts/me", ""] {
            let (exists, _) = with_script(listing(), store().mailbox_exists(name)).await;
            assert!(!exists.unwrap(), "{name}");
        }
    }

    #[tokio::test]
    async fn existence_fails_on_a_backend_failure_and_does_not_fall_back_to_inbox() {
        let (exists, _) = with_script(vec![no_rows(), no_rows(), fault("08006")], store().mailbox_exists("Archive")).await;
        assert!(exists.is_err());
    }

    #[tokio::test]
    async fn list_still_falls_back_to_inbox_on_a_backend_failure() {
        let (boxes, _) = with_script(vec![no_rows(), no_rows(), fault("08006")], store().list_mailboxes()).await;
        assert_eq!(boxes, ["INBOX"]);
    }

    // Where each kind of mailbox is read from.

    #[test]
    fn resolves_each_kind_of_mailbox_to_its_view_or_mapped_box() {
        assert_eq!(resolve_mapped_box("INBOX"), (None, false));
        assert_eq!(resolve_mapped_box("inbox"), (None, false));
        assert_eq!(resolve_mapped_box("Sent Messages"), (None, true));
        assert_eq!(resolve_mapped_box("Drafts"), (Some("Drafts"), false));
        assert_eq!(resolve_mapped_box("Trash"), (Some("Trash"), false));
        assert_eq!(resolve_mapped_box("INBOX/accounts/a"), (Some("INBOX/accounts/a"), false));
        assert_eq!(resolve_mapped_box("Sent Messages/accounts/a"), (Some("Sent Messages/accounts/a"), true));
        assert_eq!(resolve_mapped_box("Archive"), (Some("Archive"), false));
        // Only the exact spelling is the unified Sent view.
        assert_eq!(resolve_mapped_box("sent messages"), (Some("sent messages"), false));
    }

    #[test]
    fn maps_mail_fields_to_the_columns_they_are_read_from() {
        assert_eq!(map_field_name("uid"), ["uid_domain", "uid_mailbox"]);
        assert_eq!(map_field_name("replyTo"), ["reply_to_address", "reply_to_text"]);
        assert_eq!(map_field_name("messageId"), ["message_id"]);
        assert_eq!(map_field_name("text_octets"), ["text_octets"]);
        assert_eq!(map_field_name("envelopeTo"), ["envelopeTo"]);
    }

    #[tokio::test]
    async fn expunging_no_uids_asks_nothing() {
        let (expunged, trace) = with_script(vec![], store().expunge_uids("Archive", &[])).await;
        assert!(expunged.unwrap().is_empty());
        assert!(trace.is_empty());
    }

    #[test]
    fn every_future_can_cross_threads() {
        fn sendable<T: Send>(_: T) {}
        let store = store();
        let mut mail = mail();
        let (criteria, ids): (Vec<SearchCriterion>, Vec<String>) = (Vec::new(), Vec::new());
        sendable(store.list_mailboxes());
        sendable(store.list_mailbox_entries());
        sendable(store.mailbox_exists("INBOX"));
        sendable(store.count_messages("INBOX"));
        sendable(store.get_uid_next("INBOX"));
        sendable(store.get_all_uids("INBOX"));
        sendable(store.get_mail_flag_updates("INBOX", &ids));
        sendable(store.get_first_unseen_uid("INBOX"));
        sendable(store.get_highest_modseq("INBOX"));
        sendable(store.expunge("INBOX"));
        sendable(store.expunge_uids("INBOX", &[]));
        sendable(store.search("INBOX", &criteria));
        sendable(store.store_mail(&mut mail, Some("INBOX")));
        // `get_messages` and `set_flags` declare theirs.
    }
}

