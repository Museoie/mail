//! The differential database test for the IMAP data layer.
//!
//! `tests/golden/imapdb/scenario.ts` drives the Bun server's IMAP store
//! through a scripted scenario against a fresh database and records every
//! return value and the resulting tables. This test runs the same steps
//! (`scenario.steps.json`) through the Rust store against another fresh
//! database and requires the same record (`scenario.expected.json`).
//!
//! Most steps are one store call. FETCH, STORE, COPY, MOVE and APPEND are the
//! session's steps around the store; the session is not part of this port,
//! so they are written out here after `message-ops.ts`
//! (`clone_mail_to_destination` is `cloneMailToDestination`), and the Bun
//! side runs the session's own helpers.
//!
//! It needs PostgreSQL, so it only runs when asked to:
//!
//! ```bash
//! MAIL_TEST_POSTGRES=1 cargo test --manifest-path src/server-rs/Cargo.toml --test imap_store_differential
//! ```
//!
//! The server is reached with the usual `POSTGRES_*` variables (default
//! `postgres:postgres@127.0.0.1:5432`); the database is created and dropped
//! here, named `mail_imapdb_rs_<pid>`.

use std::collections::HashMap;
use std::panic::{resume_unwind, AssertUnwindSafe};

use futures_util::future::join_all;
use futures_util::{FutureExt, StreamExt};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use mail_server::imap::fetch_format::{convert_sequence_set, get_requested_fields};
use mail_server::imap::parsers::command_parser::parse_command;
use mail_server::imap::sequence_resolver::{resolve_seq_range_to_uids, resolve_uid_range_sentinel};
use mail_server::imap::store::{
    Address, Mail, MailFlagUpdate, MailUid, Store, StoreFetchedMail, StoreOperationType, StoreScope, UpdatedMailFlags,
};
use mail_server::imap::types::{ImapRequest, SequenceSet, SequenceSetType};
use mail_server::imap::util::{
    account_to_box, account_to_sent_box, box_to_account, canonical_mailbox, derive_copy_message_id, is_domain_scoped,
    is_mapped_utility_folder, is_sent_box,
};
use mail_server::models::SignedUser;
use mail_server::postgres::repositories::mailboxes::{
    create_mailbox, delete_mailbox_by_name, rename_mailbox, set_mailbox_subscribed, CreateMailboxInput,
    CreateMailboxResult, DeleteMailboxResult, RenameMailboxResult,
};
use mail_server::postgres::repositories::mails::core::{save_mail, SaveMailInput, SavedMail};
use mail_server::postgres::repositories::mails::counters::{
    get_account_uid_next, get_domain_uid_next, get_mailbox_uid_next, sync_mailbox_pivot,
};
use mail_server::postgres::repositories::mails::imap::{
    pg_byte_chunks, pg_text_chunks, TextColumn, PG_TEXT_CHUNK_BYTES, PG_TEXT_CHUNK_CHARS,
};
use mail_server::postgres::schema::schema;
use mail_server::postgres::{pool, DbError, Param};

const STEPS: &str = include_str!("golden/imapdb/scenario.steps.json");
const EXPECTED: &str = include_str!("golden/imapdb/scenario.expected.json");

const DOMAIN: &str = "example.com";
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

// ---------------------------------------------------------------------------
// Reading arguments
// ---------------------------------------------------------------------------

fn text<'a>(args: &'a Value, key: &str) -> &'a str {
    args[key].as_str().unwrap_or_else(|| panic!("{key} is a string in {args}"))
}

/// A string argument; absent and `null` are JavaScript's `undefined`.
fn opt_text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn opt_flag(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn number(args: &Value, key: &str) -> f64 {
    args[key].as_f64().unwrap_or_else(|| panic!("{key} is a number in {args}"))
}

fn opt_number(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().expect("a list").iter().map(|item| item.as_str().expect("a string").to_string()).collect()
}

fn user(args: &Value) -> SignedUser {
    let username = text(args, "user");
    let (id, email) = match username {
        "admin" => ("a0000000-0000-4000-8000-000000000001", format!("admin@{DOMAIN}")),
        "alice" => ("a0000000-0000-4000-8000-000000000002", format!("alice@alice.{DOMAIN}")),
        "bob" => ("a0000000-0000-4000-8000-000000000003", format!("bob@bob.{DOMAIN}")),
        other => panic!("no user {other}"),
    };
    SignedUser::new(Some(id.to_string()), Some(username.to_string()), Some(email))
}

/// A plain login on the user's own domain, as the session opens one.
fn store_of(args: &Value) -> Store {
    let user = user(args);
    let own_domain = user.email[user.email.find('@').unwrap() + 1..].to_string();
    Store::new(user, StoreScope::whole_store(&own_domain))
}

fn user_id(args: &Value) -> String {
    user(args).id
}

/// An address header from its scenario form, `{ value, text }`.
fn address(value: Option<&Value>) -> Option<Address> {
    value.filter(|value| !value.is_null()).map(|value| Address {
        value: value["value"].clone(),
        text: value["text"].as_str().unwrap_or_default().to_string(),
    })
}

/// The mail `new Mail(fields)` builds: the model's defaults stand in for
/// whatever the fields leave out or leave falsy.
fn mail_of(fields: &Value) -> Mail {
    let defaults = Mail::default();
    let present = |key: &str| fields.get(key).filter(|value| !value.is_null());
    let string = |key: &str, default: String| {
        fields.get(key).and_then(Value::as_str).filter(|value| !value.is_empty()).map_or(default, str::to_string)
    };
    let on = |key: &str| opt_flag(fields, key).unwrap_or(false);
    let uid = |key: &str| fields.get("uid").and_then(|uid| uid.get(key)).and_then(Value::as_i64).unwrap_or(0);
    Mail {
        attachments: present("attachments").cloned().or(defaults.attachments),
        from: address(fields.get("from")),
        to: address(fields.get("to")),
        cc: address(fields.get("cc")),
        bcc: address(fields.get("bcc")),
        reply_to: address(fields.get("replyTo")),
        envelope_from: present("envelopeFrom").cloned(),
        envelope_to: present("envelopeTo").cloned().unwrap_or(defaults.envelope_to),
        date: string("date", defaults.date),
        html: string("html", defaults.html),
        text: string("text", defaults.text),
        subject: string("subject", defaults.subject),
        message_id: string("messageId", defaults.message_id),
        read: on("read"),
        saved: on("saved"),
        sent: on("sent"),
        deleted: on("deleted"),
        draft: on("draft"),
        answered: on("answered"),
        insight: present("insight").cloned(),
        uid: MailUid { domain: uid("domain"), account: uid("account") },
        in_reply_to: opt_text(fields, "inReplyTo").map(str::to_string),
        references: fields.get("references").filter(|value| value.is_array()).map(strings),
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// A failure a step reported, as the scenario records it.
#[derive(Debug)]
struct Thrown {
    code: Option<String>,
    constraint: Option<String>,
    message: Option<String>,
}

impl From<DbError> for Thrown {
    fn from(error: DbError) -> Thrown {
        // PostgreSQL's own wording carries over; any other error's does not.
        let message = error.code.as_ref().map(|_| error.message.clone());
        Thrown { code: error.code, constraint: error.constraint, message }
    }
}

impl From<anyhow::Error> for Thrown {
    fn from(error: anyhow::Error) -> Thrown {
        match error.downcast::<DbError>() {
            Ok(error) => error.into(),
            Err(_) => Thrown { code: None, constraint: None, message: None },
        }
    }
}

impl From<String> for Thrown {
    /// A JavaScript error, such as a line that does not parse.
    fn from(_: String) -> Thrown {
        Thrown { code: None, constraint: None, message: None }
    }
}

type Outcome = Result<Value, Thrown>;

fn saved_json(saved: &SavedMail) -> Value {
    let mut result = Map::new();
    result.insert("_id".to_string(), json!(saved.id));
    if let Some(uid) = saved.uid_mailbox {
        result.insert("uid_mailbox".to_string(), json!(uid));
    }
    if let Some(uid) = saved.uid_domain {
        result.insert("uid_domain".to_string(), json!(uid));
    }
    Value::Object(result)
}

fn flag_row(row: &UpdatedMailFlags) -> Value {
    json!({
        "mail_id": row.mail_id, "uid": row.uid, "read": row.read, "saved": row.saved,
        "deleted": row.deleted, "draft": row.draft, "answered": row.answered, "modseq": row.modseq,
    })
}

fn flag_update(row: &MailFlagUpdate) -> Value {
    json!({
        "mail_id": row.mail_id, "uid": row.uid, "read": row.read, "saved": row.saved,
        "deleted": row.deleted, "draft": row.draft, "answered": row.answered,
    })
}

fn entries(mails: &[(String, StoreFetchedMail)]) -> Value {
    Value::Array(mails.iter().map(|(mail_id, mail)| json!([mail_id, mail.to_json()])).collect())
}

fn sorted(mut numbers: Vec<i64>) -> Vec<i64> {
    numbers.sort_unstable();
    numbers
}

/// JavaScript orders strings by UTF-16 code unit.
fn js_order(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// A number as JSON.stringify writes it: integers without a fraction.
fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// The session's steps around the store, after message-ops.ts
// ---------------------------------------------------------------------------

/// A command line's request, and whether it was a UID command.
fn parse_line(line: &str) -> Result<(bool, ImapRequest), Thrown> {
    let parsed = parse_command(line, None).into_result()?;
    Ok(match parsed.request {
        ImapRequest::Uid(uid) => (true, uid.into_parts().1),
        other => (false, other),
    })
}

/// The UID ranges a sequence set resolves to, as the session resolves them.
fn uid_ranges(sequence_set: &SequenceSet, is_uid: bool, seq_to_uid: &[i64]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for range in convert_sequence_set(sequence_set) {
        if is_uid || sequence_set.kind == SequenceSetType::Uid {
            let resolved = resolve_uid_range_sentinel(seq_to_uid, range.start, range.end);
            out.push((resolved.uid_start, resolved.uid_end));
        } else if let Some(resolved) = resolve_seq_range_to_uids(seq_to_uid, range.start, range.end) {
            out.push((resolved.uid_start as f64, resolved.uid_end as f64));
        }
    }
    out
}

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

/// `resolveDestContext`.
struct DestContext {
    account: String,
    is_sent: bool,
    is_domain_scoped: bool,
    is_mapped_utility: bool,
    preserves_recipient: bool,
}

fn resolve_dest_context(primary_domain: &str, destination: &str) -> DestContext {
    let is_domain_scoped = is_domain_scoped(destination);
    let is_mapped_utility = is_mapped_utility_folder(destination);
    DestContext {
        account: box_to_account(primary_domain, destination),
        is_sent: is_sent_box(destination),
        is_domain_scoped,
        is_mapped_utility,
        preserves_recipient: is_domain_scoped || is_mapped_utility,
    }
}

/// `cloneMailToDestination`: the copy of `source` filed into `destination`
/// under fresh UIDs, and the (source, destination) UID pair; `None` when the
/// store refused it.
async fn clone_mail_to_destination(
    store: &Store,
    source: &StoreFetchedMail,
    source_uid: i64,
    destination: &str,
    context: &DestContext,
) -> Result<Option<(i64, i64)>, Thrown> {
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
        mail.to = Some(Address {
            value: json!([destination_address]),
            text: to_text.unwrap_or_else(|| context.account.clone()),
        });
        mail.envelope_to = json!([destination_address]);
        mail.cc = source.cc.as_ref().map(|cc| Address { value: json!([]), text: cc.text.clone() });
        mail.bcc = source.bcc.as_ref().map(|bcc| Address { value: json!([]), text: bcc.text.clone() });
    }
    mail.sent = context.is_sent;

    let user = store.get_user();
    let domain_uid = get_domain_uid_next(&user.id, context.is_sent).await?;
    let account_uid = if context.is_mapped_utility {
        get_mailbox_uid_next(&user.id, destination).await?
    } else {
        get_account_uid_next(&user.id, &context.account, context.is_sent).await?
    };
    mail.uid = MailUid { domain: domain_uid, account: account_uid };
    if !store.store_mail(&mut mail, Some(destination)).await {
        return Ok(None);
    }
    let destination_uid = if context.is_domain_scoped { mail.uid.domain } else { mail.uid.account };
    Ok(Some((source_uid, destination_uid)))
}

/// COPY and MOVE as the session runs them, up to the response.
async fn copy_or_move(args: &Value, is_move: bool) -> Outcome {
    let store = store_of(args);
    let mailbox = text(args, "box");
    let (is_uid, request) = parse_line(text(args, "line"))?;
    let request = match request {
        ImapRequest::Copy(request) | ImapRequest::Move(request) => request,
        other => panic!("{} is not a COPY or MOVE", other.type_name()),
    };
    let destination = canonical_mailbox(&request.mailbox);
    if is_move && destination == mailbox {
        return Ok(json!({ "status": "noop" }));
    }
    if !store.mailbox_exists(&destination).await? {
        return Ok(json!({ "status": "TRYCREATE" }));
    }
    let seq_to_uid = store.get_all_uids(mailbox).await.unwrap_or_default();
    let ranges = uid_ranges(&request.sequence_set, is_uid, &seq_to_uid);
    if ranges.is_empty() {
        return Ok(json!({ "status": "nothing" }));
    }
    let mut sources: Vec<StoreFetchedMail> = Vec::new();
    for (start, end) in ranges {
        let batch = store.get_messages(mailbox, start, end, &CLONE_FIELDS, true, None).await;
        sources.extend(batch.into_iter().map(|(_, mail)| mail));
    }
    if sources.is_empty() {
        return Ok(json!({ "status": "nothing" }));
    }
    let context = resolve_dest_context(store.get_primary_domain(), &destination);
    let source_is_domain_scoped = is_domain_scoped(mailbox);
    let source_uid = |mail: &StoreFetchedMail| {
        let uid = mail.uid.expect("the uid was read");
        if source_is_domain_scoped {
            uid.domain
        } else {
            uid.account
        }
    };
    // A stable sort, as Array.prototype.sort is.
    sources.sort_by_key(|mail| source_uid(mail));
    let mut seen = std::collections::HashSet::new();
    sources.retain(|mail| seen.insert(source_uid(mail)));
    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for source in &sources {
        match clone_mail_to_destination(&store, source, source_uid(source), &destination, &context).await? {
            Some(pair) => pairs.push(pair),
            None => return Ok(json!({ "status": "partial", "pairs": pairs })),
        }
    }
    if !is_move {
        return Ok(json!({ "status": "copied", "destination": destination, "pairs": pairs }));
    }
    let source_uids: Vec<i64> = pairs.iter().map(|(source, _)| *source).collect();
    let expunged = store.expunge_uids(mailbox, &source_uids).await?;
    Ok(json!({ "status": "moved", "destination": destination, "pairs": pairs, "expunged": sorted(expunged) }))
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Harness {
    /// Generated ids, and what each stands for.
    labels: HashMap<String, String>,
}

impl Harness {
    async fn refresh_labels(&mut self) {
        let mails =
            pool().query("SELECT mail_id, user_id, message_id FROM mails", &[]).await.expect("mails are readable");
        for row in &mails.rows {
            self.labels
                .entry(row.text("mail_id"))
                .or_insert_with(|| format!("mail({} {})", row.text("user_id"), row.text("message_id")));
        }
        let boxes =
            pool().query("SELECT mailbox_id, user_id, name FROM mailboxes", &[]).await.expect("mailboxes are readable");
        for row in &boxes.rows {
            self.labels
                .entry(row.text("mailbox_id"))
                .or_insert_with(|| format!("mailbox({} {})", row.text("user_id"), row.text("name")));
        }
    }

    fn label<'a>(&'a self, id: &'a str) -> &'a str {
        self.labels.get(id).map(String::as_str).unwrap_or(id)
    }

    /// Replaces generated ids by what they stand for, and drops the clock.
    fn normalize(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.label(text).to_string()),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.normalize(item)).collect()),
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "created")
                    .map(|(key, entry)| (key.clone(), self.normalize(entry)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// By UID, then by name: an UPDATE returns its rows in no set order.
    fn by_uid(&self, a: (i64, &str), b: (i64, &str)) -> std::cmp::Ordering {
        a.0.cmp(&b.0).then_with(|| js_order(self.label(a.1), self.label(b.1)))
    }

    fn sorted_flag_rows(&self, mut rows: Vec<UpdatedMailFlags>) -> Vec<UpdatedMailFlags> {
        rows.sort_by(|a, b| self.by_uid((a.uid, &a.mail_id), (b.uid, &b.mail_id)));
        rows
    }

    async fn mail_ids_of(&self, user_id: &str, message_ids: &[String]) -> Vec<String> {
        let result = pool()
            .query(
                "SELECT mail_id FROM mails WHERE user_id = $1 AND message_id = ANY($2) ORDER BY message_id COLLATE \"C\"",
                &[Param::from(user_id), Param::array(message_ids)],
            )
            .await
            .expect("mails are readable");
        result.rows.iter().map(|row| row.text("mail_id")).collect()
    }

    async fn mail_id_of(&self, user_id: &str, message_id: &str) -> String {
        self.mail_ids_of(user_id, &[message_id.to_string()])
            .await
            .into_iter()
            .next()
            .unwrap_or_else(|| "c0000000-0000-4000-8000-00000000dead".to_string())
    }

    async fn run(&mut self, op: &str, args: &Value) -> Outcome {
        Ok(match op {
            "sql" => {
                let values: Vec<Param> = strings(&args["values"]).into_iter().map(Param::from).collect();
                let result = pool().query(text(args, "sql"), &values).await?;
                json!({ "rowCount": result.row_count })
            }

            "ddl" => {
                pool().query(text(args, "sql"), &[]).await?;
                Value::Null
            }

            "deliver" => {
                let user_id = user_id(args);
                let sent = args["sent"].as_bool().expect("sent");
                let first_address = |key: &str| {
                    args.get(key)
                        .and_then(|list| list.get(0))
                        .and_then(|entry| entry.get("address"))
                        .and_then(Value::as_str)
                };
                let scope = if sent { first_address("envelopeFrom") } else { first_address("envelopeTo") };
                let mailbox = scope.map(|scope| if sent { account_to_sent_box(scope) } else { account_to_box(scope) });
                let uid_domain = get_domain_uid_next(&user_id, sent).await?;
                let uid_mailbox = get_account_uid_next(&user_id, scope.unwrap_or(""), sent).await?;
                let text_body = match args.get("textRepeat").filter(|repeat| repeat.is_object()) {
                    Some(repeat) => text(repeat, "unit").repeat(repeat["count"].as_u64().expect("a count") as usize),
                    None => text(args, "text").to_string(),
                };
                let field = |key: &str| address(args.get(key));
                let document = |key: &str| args.get(key).filter(|value| !value.is_null()).cloned();
                let input = SaveMailInput {
                    user_id: user_id.clone(),
                    message_id: text(args, "messageId").to_string(),
                    in_reply_to: opt_text(args, "inReplyTo").map(str::to_string),
                    reference_ids: args
                        .get("references")
                        .filter(|value| value.is_array())
                        .map(strings)
                        .filter(|list| !list.is_empty()),
                    subject: Some(text(args, "subject").to_string()),
                    date: Some(text(args, "date").to_string()),
                    html: Some(text(args, "html").to_string()),
                    text: Some(text_body),
                    from_address: field("from").map(|address| address.value),
                    from_text: field("from").map(|address| address.text),
                    to_address: field("to").map(|address| address.value),
                    to_text: field("to").map(|address| address.text),
                    cc_address: field("cc").map(|address| address.value),
                    cc_text: field("cc").map(|address| address.text),
                    bcc_address: field("bcc").map(|address| address.value),
                    bcc_text: field("bcc").map(|address| address.text),
                    reply_to_address: field("replyTo").map(|address| address.value),
                    reply_to_text: field("replyTo").map(|address| address.text),
                    envelope_from: document("envelopeFrom"),
                    envelope_to: document("envelopeTo"),
                    attachments: document("attachments"),
                    read: opt_flag(args, "read").unwrap_or(false),
                    saved: opt_flag(args, "saved").unwrap_or(false),
                    sent,
                    deleted: opt_flag(args, "deleted").unwrap_or(false),
                    draft: false,
                    answered: false,
                    expunged: false,
                    insight: document("insight"),
                    uid_domain: Some(uid_domain),
                    uid_mailbox: Some(uid_mailbox),
                    mailbox,
                    spam_score: Some(opt_number(args, "spamScore").unwrap_or(0.0)),
                    spam_reasons: args.get("spamReasons").filter(|value| value.is_array()).map(strings),
                    is_spam: opt_flag(args, "isSpam").unwrap_or(false),
                    placement: None,
                    domain_mailbox: Some("INBOX".to_string()),
                };
                let saved = save_mail(&input).await?;
                self.refresh_labels().await;
                saved.as_ref().map_or(Value::Null, saved_json)
            }

            "createMailbox" => {
                let input = CreateMailboxInput {
                    user_id: user_id(args),
                    name: text(args, "name").to_string(),
                    address: opt_text(args, "address").map(str::to_string),
                    parent_id: None,
                    special_use: opt_text(args, "special_use").map(str::to_string),
                    subscribed: opt_flag(args, "subscribed"),
                };
                let result = create_mailbox(&input).await?;
                self.refresh_labels().await;
                match result {
                    CreateMailboxResult::Created(mailbox) => {
                        json!({ "status": "created", "mailbox": mailbox.to_json() })
                    }
                    CreateMailboxResult::Exists => json!({ "status": "exists" }),
                    CreateMailboxResult::AtLimit => json!({ "status": "at_limit" }),
                }
            }
            "deleteMailboxByName" => match delete_mailbox_by_name(&user_id(args), text(args, "name")).await? {
                DeleteMailboxResult::Deleted => json!("deleted"),
                DeleteMailboxResult::NotFound => json!("not_found"),
                DeleteMailboxResult::Protected => json!("protected"),
            },
            "renameMailbox" => {
                match rename_mailbox(&user_id(args), text(args, "oldName"), text(args, "newName")).await? {
                    RenameMailboxResult::Renamed => json!("renamed"),
                    RenameMailboxResult::NotFound => json!("not_found"),
                    RenameMailboxResult::Protected => json!("protected"),
                    RenameMailboxResult::NameTaken => json!("name_taken"),
                }
            }
            "setMailboxSubscribed" => json!(
                set_mailbox_subscribed(
                    &user_id(args),
                    text(args, "name"),
                    args["subscribed"].as_bool().expect("subscribed")
                )
                .await?
            ),

            "listMailboxes" => json!(store_of(args).list_mailboxes().await),
            "listMailboxEntries" => Value::Array(
                store_of(args)
                    .list_mailbox_entries()
                    .await
                    .iter()
                    .map(|entry| json!({ "name": entry.name, "subscribed": entry.subscribed }))
                    .collect(),
            ),
            "mailboxExists" => json!(store_of(args).mailbox_exists(text(args, "box")).await?),
            "countMessages" => match store_of(args).count_messages(text(args, "box")).await {
                Some(counts) => json!({ "total": counts.total, "unread": counts.unread }),
                None => Value::Null,
            },
            "getUidNext" => json!(store_of(args).get_uid_next(text(args, "box")).await?),
            "getAllUids" => json!(store_of(args).get_all_uids(text(args, "box")).await),
            "getFirstUnseenUid" => json!(store_of(args).get_first_unseen_uid(text(args, "box")).await),
            "getHighestModseq" => json!(store_of(args).get_highest_modseq(text(args, "box")).await),
            "getMailFlagUpdates" => {
                let ids = self.mail_ids_of(&user_id(args), &strings(&args["messageIds"])).await;
                match store_of(args).get_mail_flag_updates(text(args, "box"), &ids).await {
                    Some(mut updates) => {
                        updates.sort_by(|a, b| self.by_uid((a.uid, &a.mail_id), (b.uid, &b.mail_id)));
                        Value::Array(updates.iter().map(flag_update).collect())
                    }
                    None => Value::Null,
                }
            }
            "getMessages" => {
                let mails = store_of(args)
                    .get_messages(
                        text(args, "box"),
                        number(args, "start"),
                        number(args, "end"),
                        &strings(&args["fields"]),
                        args["useUid"].as_bool().expect("useUid"),
                        opt_number(args, "changedSince"),
                    )
                    .await;
                entries(&mails)
            }
            "concurrentFetch" => {
                let store = store_of(args);
                let fields = strings(&args["fields"]);
                let count = args["count"].as_u64().expect("a count") as usize;
                let reads = join_all(
                    (0..count)
                        .map(|_| store.get_messages(text(args, "box"), 1.0, MAX_SAFE_INTEGER, &fields, true, None)),
                )
                .await;
                let shapes: std::collections::HashSet<String> =
                    reads.iter().map(|read| entries(read).to_string()).collect();
                json!({ "reads": reads.len(), "distinct": shapes.len(), "first": entries(&reads[0]) })
            }
            "setFlags" => {
                let operation = opt_text(args, "operation").map_or(StoreOperationType::Flags, |operation| {
                    StoreOperationType::parse(operation).expect("an operation")
                });
                let result = store_of(args)
                    .set_flags(
                        text(args, "box"),
                        number(args, "start"),
                        number(args, "end"),
                        &strings(&args["flags"]),
                        args["useUid"].as_bool().expect("useUid"),
                        operation,
                        opt_number(args, "unchangedSince"),
                    )
                    .await;
                let updated = self.sorted_flag_rows(result.updated);
                json!({ "updated": updated.iter().map(flag_row).collect::<Vec<_>>(), "failed": sorted(result.failed) })
            }
            "expunge" => json!(sorted(store_of(args).expunge(text(args, "box")).await?)),
            "expungeUids" => {
                let uids: Vec<i64> =
                    args["uids"].as_array().expect("uids").iter().map(|uid| uid.as_i64().expect("a uid")).collect();
                json!(sorted(store_of(args).expunge_uids(text(args, "box"), &uids).await?))
            }
            "search" => {
                let criteria = match parse_line(text(args, "line"))?.1 {
                    ImapRequest::Search(search) => search.criteria,
                    other => panic!("{} is not a SEARCH", other.type_name()),
                };
                json!(store_of(args).search(text(args, "box"), &criteria).await)
            }
            "storeMail" => {
                let mut mail = mail_of(&args["mail"]);
                let ok = store_of(args).store_mail(&mut mail, opt_text(args, "destination")).await;
                self.refresh_labels().await;
                json!({ "ok": ok, "uid": { "domain": mail.uid.domain, "account": mail.uid.account } })
            }

            // FETCH as the session reads it: the projection the items ask
            // for, one range read per resolved range, merged by mail id.
            "fetch" => {
                let store = store_of(args);
                let mailbox = text(args, "box");
                let (is_uid, request) = parse_line(text(args, "line"))?;
                let ImapRequest::Fetch(fetch) = request else { panic!("{} is not a FETCH", text(args, "line")) };
                let seq_to_uid = store.get_all_uids(mailbox).await.unwrap_or_default();
                let fields = get_requested_fields(&fetch.data_items);
                let mut merged: Vec<(String, StoreFetchedMail)> = Vec::new();
                for (start, end) in uid_ranges(&fetch.sequence_set, is_uid, &seq_to_uid) {
                    let messages =
                        store.get_messages(mailbox, start, end, fields.as_slice(), true, fetch.changed_since).await;
                    for (mail_id, mail) in messages {
                        match merged.iter_mut().find(|(id, _)| *id == mail_id) {
                            Some(entry) => entry.1 = mail,
                            None => merged.push((mail_id, mail)),
                        }
                    }
                }
                entries(&merged)
            }

            // STORE as the session runs it: one store call per resolved
            // range, then the Starred / Trash pivots of every row it changed.
            "store" => {
                let store = store_of(args);
                let mailbox = text(args, "box");
                let user_id = user_id(args);
                let (is_uid, request) = parse_line(text(args, "line"))?;
                let ImapRequest::Store(request) = request else { panic!("{} is not a STORE", text(args, "line")) };
                let operation = StoreOperationType::from(request.operation);
                let touches_saved =
                    operation == StoreOperationType::Flags || request.flags.iter().any(|flag| flag == "\\Flagged");
                let touches_deleted =
                    operation == StoreOperationType::Flags || request.flags.iter().any(|flag| flag == "\\Deleted");
                let seq_to_uid = store.get_all_uids(mailbox).await.unwrap_or_default();
                let mut passes = Vec::new();
                for (start, end) in uid_ranges(&request.sequence_set, is_uid, &seq_to_uid) {
                    let result = store
                        .set_flags(mailbox, start, end, &request.flags, true, operation, request.unchanged_since)
                        .await;
                    let updated = self.sorted_flag_rows(result.updated);
                    for row in &updated {
                        if touches_saved {
                            sync_mailbox_pivot(&user_id, "Starred", &row.mail_id, row.saved).await?;
                        }
                        if touches_deleted {
                            sync_mailbox_pivot(&user_id, "Trash", &row.mail_id, row.deleted).await?;
                        }
                    }
                    passes.push(json!({
                        "uidStart": js_number(start),
                        "uidEnd": js_number(end),
                        "updated": updated.iter().map(flag_row).collect::<Vec<_>>(),
                        "failed": sorted(result.failed),
                    }));
                }
                Value::Array(passes)
            }

            "copy" => copy_or_move(args, false).await?,
            "move" => copy_or_move(args, true).await?,

            // APPEND's storage step, as the session runs it once the message
            // is parsed.
            "append" => {
                let store = store_of(args);
                let target = canonical_mailbox(text(args, "mailbox"));
                if !store.mailbox_exists(&target).await? {
                    return Ok(json!({ "status": "TRYCREATE" }));
                }
                let flags = strings(&args["flags"]);
                let has = |flag: &str| flags.iter().any(|named| named == flag);
                let mut mail = mail_of(&args["mail"]);
                mail.draft = has("\\Draft");
                mail.read = has("\\Seen");
                mail.saved = has("\\Flagged");
                mail.deleted = has("\\Deleted");
                mail.answered = has("\\Answered");
                let user = store.get_user().clone();
                let account = box_to_account(store.get_primary_domain(), &target);
                let target_is_sent = is_sent_box(&target);
                mail.sent = target_is_sent;
                mail.uid.domain = get_domain_uid_next(&user.id, target_is_sent).await?;
                mail.uid.account = if is_mapped_utility_folder(&target) {
                    get_mailbox_uid_next(&user.id, &target).await?
                } else {
                    get_account_uid_next(&user.id, &account, target_is_sent).await?
                };
                let ok = store.store_mail(&mut mail, Some(&target)).await;
                self.refresh_labels().await;
                let uid = if is_domain_scoped(&target) { mail.uid.domain } else { mail.uid.account };
                json!({ "status": if ok { "stored" } else { "failed" }, "uid": uid })
            }

            // The chunked readers against real rows: what each chunk holds.
            "textChunks" => {
                let user_id = user_id(args);
                let mail_id = self.mail_id_of(&user_id, text(args, "messageId")).await;
                let chunk_chars = opt_number(args, "chunkChars").map_or(PG_TEXT_CHUNK_CHARS, |chars| chars as usize);
                let mut chunks = std::pin::pin!(pg_text_chunks(&mail_id, &user_id, column(args), chunk_chars));
                let mut out = Vec::new();
                while let Some(chunk) = chunks.next().await {
                    let chunk = chunk?;
                    out.push(json!({
                        "codePoints": chunk.chars().count(),
                        "utf16": chunk.encode_utf16().count(),
                        "sha256": sha256_hex(chunk.as_bytes()),
                        "head": chunk.chars().take(4).collect::<String>(),
                    }));
                }
                Value::Array(out)
            }
            "byteChunks" => {
                let user_id = user_id(args);
                let mail_id = self.mail_id_of(&user_id, text(args, "messageId")).await;
                let start = opt_number(args, "startByte").map_or(1, |start| start as usize);
                let chunk_bytes = opt_number(args, "chunkBytes").map_or(PG_TEXT_CHUNK_BYTES, |bytes| bytes as usize);
                let mut chunks = std::pin::pin!(pg_byte_chunks(&mail_id, &user_id, column(args), start, chunk_bytes));
                let mut out = Vec::new();
                while let Some(chunk) = chunks.next().await {
                    let chunk = chunk?;
                    out.push(json!({
                        "bytes": chunk.len(),
                        "sha256": sha256_hex(&chunk),
                        "head": hex::encode(&chunk[..chunk.len().min(4)]),
                    }));
                }
                Value::Array(out)
            }

            other => panic!("no op {other}"),
        })
    }
}

fn column(args: &Value) -> TextColumn {
    match text(args, "source") {
        "text" => TextColumn::Text,
        "html" => TextColumn::Html,
        other => panic!("no column {other}"),
    }
}

const MAIL_COLUMNS: &str = "mail_id, user_id, message_id, subject, date, html, CASE WHEN length(text) > 1000 THEN NULL ELSE text END AS text, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, reply_to_address, reply_to_text, envelope_from, envelope_to, attachments, read, saved, sent, deleted, draft, answered, expunged, insight, uid_domain, modseq, spam_score, spam_reasons, is_spam, rfc822_size, text_line_count, html_line_count, in_reply_to, reference_ids, md5(text) AS text_md5";

async fn table(sql: &str) -> Vec<Value> {
    let result = pool().query(sql, &[]).await.unwrap_or_else(|error| panic!("{sql}: {error}"));
    result.rows.into_iter().map(|row| Value::Object(row.into_map())).collect()
}

/// Runs every step and returns the record the Bun scenario writes.
async fn run_scenario() -> Value {
    let steps: Value = serde_json::from_str(STEPS).expect("scenario.steps.json is JSON");
    let mut harness = Harness::default();
    let mut recorded: Vec<Value> = Vec::new();

    for (index, step) in steps["steps"].as_array().expect("steps is a list").iter().enumerate() {
        let op = text(step, "op");
        let mut entry = Map::new();
        entry.insert("step".to_string(), json!(index + 1));
        entry.insert("op".to_string(), json!(op));
        // Rows an earlier step filed are named before this one sorts by name.
        harness.refresh_labels().await;
        match harness.run(op, &step["args"]).await {
            Ok(returned) => {
                // A COPY files rows nobody has named yet; name them before
                // recording.
                harness.refresh_labels().await;
                entry.insert("ok".to_string(), harness.normalize(&returned));
            }
            Err(thrown) => {
                harness.refresh_labels().await;
                entry.insert(
                    "threw".to_string(),
                    json!({ "code": thrown.code, "constraint": thrown.constraint, "message": thrown.message }),
                );
            }
        }
        recorded.push(Value::Object(entry));
    }

    harness.refresh_labels().await;
    let mails = table(&format!("SELECT {MAIL_COLUMNS} FROM mails ORDER BY user_id, message_id COLLATE \"C\"")).await;
    let mappings = table(
        "SELECT user_id, mailbox, mail_id, uid FROM mail_mailbox_uid ORDER BY user_id, mailbox COLLATE \"C\", uid",
    )
    .await;
    let counters = table(
        "SELECT user_id, uid_kind, uid_scope, sent, last_uid FROM mail_uid_counters ORDER BY user_id, uid_kind, uid_scope COLLATE \"C\", sent",
    )
    .await;
    let boxes = table(
        "SELECT mailbox_id, user_id, name, address, parent_id, uid_validity, uid_next, subscribed, special_use FROM mailboxes ORDER BY user_id, name COLLATE \"C\"",
    )
    .await;

    let normalized = |rows: &[Value]| -> Vec<Value> { rows.iter().map(|row| harness.normalize(row)).collect() };
    json!({
        "results": recorded,
        "tables": {
            "mails": normalized(&mails),
            "mail_mailbox_uid": normalized(&mappings),
            "mail_uid_counters": normalized(&counters),
            "mailboxes": normalized(&boxes),
        },
    })
}

/// Where the two records differ, one line each.
fn differences(path: &str, actual: &Value, expected: &Value, out: &mut Vec<String>) {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => {
            let keys: std::collections::BTreeSet<&String> = actual.keys().chain(expected.keys()).collect();
            for key in keys {
                match (actual.get(key), expected.get(key)) {
                    (Some(a), Some(e)) => differences(&format!("{path}.{key}"), a, e, out),
                    (Some(a), None) => out.push(format!("{path}.{key}: Rust has {a}, Bun has no such field")),
                    (None, Some(e)) => out.push(format!("{path}.{key}: Rust has no such field, Bun has {e}")),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(actual), Value::Array(expected)) => {
            if actual.len() != expected.len() {
                out.push(format!("{path}: Rust has {} entries, Bun has {}", actual.len(), expected.len()));
            }
            for (index, (a, e)) in actual.iter().zip(expected).enumerate() {
                let name = match a.get("op").and_then(Value::as_str) {
                    Some(op) => format!("{path}[{index}] ({op})"),
                    None => format!("{path}[{index}]"),
                };
                differences(&name, a, e, out);
            }
        }
        (actual, expected) if actual != expected => out.push(format!("{path}: Rust has {actual}, Bun has {expected}")),
        _ => {}
    }
}

struct Server {
    host: String,
    port: String,
    user: String,
    password: String,
}

impl Server {
    fn from_env() -> Server {
        let var = |name: &str, default: &str| {
            std::env::var(name).ok().filter(|value| !value.is_empty()).unwrap_or_else(|| default.to_string())
        };
        Server {
            host: var("POSTGRES_HOST", "127.0.0.1"),
            port: var("POSTGRES_PORT", "5432"),
            user: var("POSTGRES_USER", "postgres"),
            password: std::env::var("POSTGRES_PASSWORD").unwrap_or_else(|_| "postgres".to_string()),
        }
    }

    /// Runs one statement on the server's maintenance database.
    async fn admin(&self, sql: &str) {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&self.host)
            .port(self.port.parse().expect("POSTGRES_PORT is a port"))
            .user(&self.user)
            .password(&self.password)
            .dbname("postgres");
        let (client, connection) = config.connect(tokio_postgres::NoTls).await.expect("PostgreSQL is reachable");
        let driver = tokio::spawn(connection);
        client.batch_execute(sql).await.unwrap_or_else(|error| panic!("{sql}: {error}"));
        drop(client);
        let _ = driver.await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rust_store_answers_and_writes_as_the_bun_store_does() {
    if std::env::var("MAIL_TEST_POSTGRES").as_deref() != Ok("1") {
        eprintln!("skipped: set MAIL_TEST_POSTGRES=1 to run the differential database test");
        return;
    }

    let server = Server::from_env();
    let database = format!("mail_imapdb_rs_{}", std::process::id());
    server.admin(&format!("DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)")).await;
    server.admin(&format!("CREATE DATABASE \"{database}\"")).await;
    // The pool reads these on first use; nothing has used it yet.
    std::env::set_var("POSTGRES_HOST", &server.host);
    std::env::set_var("POSTGRES_PORT", &server.port);
    std::env::set_var("POSTGRES_USER", &server.user);
    std::env::set_var("POSTGRES_PASSWORD", &server.password);
    std::env::set_var("POSTGRES_DATABASE", &database);
    std::env::set_var("EMAIL_DOMAIN", DOMAIN);
    std::env::set_var("TZ", "UTC");
    std::env::set_var("NODE_ENV", "test");

    // Attachments are measured under `./attachments`.
    let work_directory = tempfile::tempdir().expect("a temporary directory");
    std::env::set_current_dir(work_directory.path()).expect("the working directory");

    let outcome = AssertUnwindSafe(async {
        let schema = schema();
        for table in &schema.tables {
            pool().query(&table.create_sql, &[]).await.expect("the table is created");
        }
        for sql in &schema.search_vector_ddl {
            pool().query(sql, &[]).await.expect("the search trigger is installed");
        }
        for index in &schema.indexes {
            pool().query(&index.sql, &[]).await.expect("the index is created");
        }
        run_scenario().await
    })
    .catch_unwind()
    .await;

    pool().end();
    server.admin(&format!("DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)")).await;

    let actual = match outcome {
        Ok(actual) => actual,
        Err(panic) => resume_unwind(panic),
    };
    let expected: Value = serde_json::from_str(EXPECTED).expect("scenario.expected.json is JSON");
    let mut found = Vec::new();
    differences("results", &actual["results"], &expected["results"], &mut found);
    differences("tables", &actual["tables"], &expected["tables"], &mut found);
    assert!(
        found.is_empty(),
        "the Rust store differs from the Bun store in {} places:\n{}",
        found.len(),
        found.iter().take(60).cloned().collect::<Vec<_>>().join("\n")
    );
    assert!(actual["results"].as_array().is_some_and(|results| results.len() >= 1200), "every step ran");
}
