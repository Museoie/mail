//! The differential database test for the mail storage core.
//!
//! `tests/golden/storage/scenario.ts` drives the Bun server's repositories
//! through a scripted scenario against a fresh database and records every
//! return value and the resulting tables. This test runs the same steps
//! (`scenario.steps.json`) through the Rust repositories against another
//! fresh database and requires the same record (`scenario.expected.json`).
//!
//! It needs PostgreSQL, so it only runs when asked to:
//!
//! ```bash
//! MAIL_TEST_POSTGRES=1 cargo test --manifest-path src/server-rs/Cargo.toml --test storage_differential
//! ```
//!
//! The server is reached with the usual `POSTGRES_*` variables (default
//! `postgres:postgres@127.0.0.1:5432`); the database is created and dropped
//! here, named `mail_core_rs_<pid>`.

use std::collections::{HashMap, HashSet};
use std::panic::{resume_unwind, AssertUnwindSafe};

use futures_util::future::join_all;
use futures_util::FutureExt;
use serde_json::{json, Map, Value};

use mail_server::js;
use mail_server::postgres::repositories::mailboxes::{
    create_mailbox, delete_mailbox_by_name, get_mailbox_by_address, get_mailbox_by_name, get_mailboxes_by_user,
    rename_mailbox, set_mailbox_subscribed, CreateMailboxInput, CreateMailboxResult, DeleteMailboxResult,
    MailboxModel, RenameMailboxResult,
};
use mail_server::postgres::repositories::mails::core::{
    delete_mail, get_mail_by_id, get_mail_by_message_id, mark_mail_read, mark_mail_saved, mark_mail_spam,
    record_mappings, save_mail, sync_mapped_pivots_for_row, update_rfc822_size, Placement, SaveMailInput, SavedMail,
};
use mail_server::postgres::repositories::mails::counters::{
    delete_mailbox_uid, get_account_uid_next, get_domain_uid_next, get_highest_modseq, get_mailbox_uid_next,
    get_next_modseq, get_uid_next, sync_mailbox_pivot, write_mailbox_uid, UidScope,
};
use mail_server::postgres::repositories::mails::mapping_decisions::{MappingScope, MappingWrite};
use mail_server::postgres::repositories::mails::model::{mails_table, MailModel};
use mail_server::postgres::repositories::mails::set_flags_query::build_set_mail_flags_queries;
use mail_server::postgres::repositories::mails::table::SelectFilter;
use mail_server::postgres::schema::schema;
use mail_server::postgres::{pool, DbError, Param};

const STEPS: &str = include_str!("golden/storage/scenario.steps.json");
const EXPECTED: &str = include_str!("golden/storage/scenario.expected.json");

/// The date the scenario's concurrent saves carry.
const DATE: &str = "2024-03-04T05:06:07.000Z";

// ---------------------------------------------------------------------------
// Reading arguments
// ---------------------------------------------------------------------------

fn text<'a>(args: &'a Value, key: &str) -> &'a str {
    args[key].as_str().unwrap_or_else(|| panic!("{key} is a string in {args}"))
}

fn opt_text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn flag(args: &Value, key: &str) -> bool {
    args[key].as_bool().unwrap_or_else(|| panic!("{key} is a boolean in {args}"))
}

fn opt_flag(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn integer(args: &Value, key: &str) -> i64 {
    args[key].as_i64().unwrap_or_else(|| panic!("{key} is an integer in {args}"))
}

fn opt_integer(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

/// A statement parameter from its JSON spelling.
fn param(value: &Value) -> Param {
    match value {
        Value::Null => Param::Null,
        Value::Bool(flag) => Param::from(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => Param::from(integer),
            None => Param::from(number.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(text) => Param::from(text.as_str()),
        other => Param::json(other),
    }
}

fn save_mail_input(args: &Value) -> SaveMailInput {
    let string = |key: &str| opt_text(args, key).map(str::to_string);
    let document = |key: &str| args.get(key).cloned();
    let strings = |key: &str| {
        args.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>())
    };
    let on = |key: &str| opt_flag(args, key).unwrap_or(false);
    SaveMailInput {
        user_id: text(args, "user_id").to_string(),
        message_id: text(args, "message_id").to_string(),
        in_reply_to: string("in_reply_to"),
        reference_ids: strings("reference_ids"),
        subject: string("subject"),
        date: string("date"),
        html: string("html"),
        text: string("text"),
        from_address: document("from_address"),
        from_text: string("from_text"),
        to_address: document("to_address"),
        to_text: string("to_text"),
        cc_address: document("cc_address"),
        cc_text: string("cc_text"),
        bcc_address: document("bcc_address"),
        bcc_text: string("bcc_text"),
        reply_to_address: document("reply_to_address"),
        reply_to_text: string("reply_to_text"),
        envelope_from: document("envelope_from"),
        envelope_to: document("envelope_to"),
        attachments: document("attachments"),
        read: on("read"),
        saved: on("saved"),
        sent: on("sent"),
        deleted: on("deleted"),
        draft: on("draft"),
        answered: on("answered"),
        expunged: on("expunged"),
        insight: document("insight"),
        uid_domain: opt_integer(args, "uid_domain"),
        uid_mailbox: opt_integer(args, "uid_mailbox"),
        spam_score: args.get("spam_score").and_then(Value::as_f64),
        spam_reasons: strings("spam_reasons"),
        is_spam: on("is_spam"),
        placement: args.get("placement").filter(|placement| placement.is_object()).map(|placement| Placement {
            draft: opt_flag(placement, "draft"),
            is_spam: opt_flag(placement, "is_spam"),
            saved: opt_flag(placement, "saved"),
            deleted: opt_flag(placement, "deleted"),
        }),
        mailbox: string("mailbox"),
        domain_mailbox: string("domain_mailbox"),
    }
}

fn uid_scope(scope: &Value) -> UidScope {
    match text(scope, "kind") {
        "domain" => UidScope::Domain { sent: flag(scope, "sent") },
        "account" => UidScope::Account { account: text(scope, "account").to_string(), sent: flag(scope, "sent") },
        "mailbox" => UidScope::Mailbox { mailbox: text(scope, "mailbox").to_string() },
        other => panic!("no scope {other}"),
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// A failure a repository function reported.
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

fn optional<T>(value: Option<T>, to_json: impl Fn(&T) -> Value) -> Value {
    value.as_ref().map(to_json).unwrap_or(Value::Null)
}

/// JavaScript orders strings by UTF-16 code unit.
fn js_order(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

const RECENT_MS: i64 = 10 * 60 * 1000;

#[derive(Default)]
struct Harness {
    /// What each named step returned, as it was returned.
    results: HashMap<String, Value>,
    /// Generated ids, and what each stands for.
    labels: HashMap<String, String>,
    /// Mails stored without a date carry the time of the save.
    saved_now: HashSet<String>,
}

fn is_raced(message_id: Option<&Value>) -> bool {
    message_id
        .and_then(Value::as_str)
        .is_some_and(|message_id| message_id.starts_with("<conc-") || message_id.starts_with("<race-"))
}

impl Harness {
    async fn refresh_labels(&mut self) {
        let mails = pool().query("SELECT mail_id, user_id, message_id FROM mails", &[]).await.expect("mails are readable");
        for row in &mails.rows {
            self.labels
                .entry(row.text("mail_id"))
                .or_insert_with(|| format!("mail({} {})", row.text("user_id"), row.text("message_id")));
        }
        let boxes = pool()
            .query("SELECT mailbox_id, user_id, name FROM mailboxes WHERE name NOT LIKE 'bulk-%'", &[])
            .await
            .expect("mailboxes are readable");
        for row in &boxes.rows {
            self.labels
                .entry(row.text("mailbox_id"))
                .or_insert_with(|| format!("mailbox({} {})", row.text("user_id"), row.text("name")));
        }
    }

    fn label<'a>(&'a self, id: &'a str) -> &'a str {
        self.labels.get(id).map(String::as_str).unwrap_or(id)
    }

    /// Replaces what differs from run to run by what it stands for, and
    /// leaves out the columns no two runs share.
    fn normalize(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.label(text).to_string()),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.normalize(item)).collect()),
            Value::Object(object) => {
                let mut out: Map<String, Value> = object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "created")
                    .map(|(key, entry)| (key.clone(), self.normalize(entry)))
                    .collect();
                if let (Some(mail_id), Some(date)) =
                    (object.get("mail_id").and_then(Value::as_str), object.get("date").and_then(Value::as_str))
                {
                    if self.saved_now.contains(mail_id) {
                        let age = js::date_parse(date).map(|ms| (js::now_ms() - ms).abs());
                        let shown = match age {
                            Some(age) if age < RECENT_MS => "<now>".to_string(),
                            _ => format!("stale: {date}"),
                        };
                        out.insert("date".to_string(), Value::String(shown));
                    }
                }
                if is_raced(object.get("message_id")) {
                    for key in ["uid_domain", "modseq"] {
                        if out.contains_key(key) {
                            out.insert(key.to_string(), json!("<conc>"));
                        }
                    }
                    // Overlapping merges each rewrite the envelope from the
                    // row they read.
                    let merged = object["message_id"].as_str().is_some_and(|id| id.starts_with("<race-"));
                    if merged && out.contains_key("envelope_to") {
                        out.insert("envelope_to".to_string(), json!("<conc>"));
                    }
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }

    fn lookup(&self, path: &str) -> Value {
        let alias = self
            .results
            .keys()
            .filter(|name| path == name.as_str() || path.starts_with(&format!("{name}.")))
            .max_by_key(|name| name.len())
            .unwrap_or_else(|| panic!("no result named in {path}"));
        let mut value = &self.results[alias];
        for key in path[alias.len()..].split('.').filter(|key| !key.is_empty()) {
            value = value.get(key).unwrap_or_else(|| panic!("{path} is not in the results"));
        }
        value.clone()
    }

    fn resolve(&self, value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(|item| self.resolve(item)).collect()),
            Value::Object(object) => match object.get("$ref").and_then(Value::as_str) {
                Some(path) => self.lookup(path),
                None => Value::Object(object.iter().map(|(key, entry)| (key.clone(), self.resolve(entry))).collect()),
            },
            other => other.clone(),
        }
    }

    /// The rows of a STORE as the scenario records them: by UID, and by label
    /// where two rows of a domain view share one.
    fn flag_rows(&self, rows: &[mail_server::postgres::Row]) -> Vec<Value> {
        let mut rows: Vec<Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "mail_id": row.text("mail_id"),
                    "uid": row.i64("uid"),
                    "read": row.bool("read"),
                    "saved": row.bool("saved"),
                    "deleted": row.bool("deleted"),
                    "draft": row.bool("draft"),
                    "answered": row.bool("answered"),
                    "modseq": row.i64("modseq"),
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            integer(a, "uid")
                .cmp(&integer(b, "uid"))
                .then_with(|| js_order(self.label(text(a, "mail_id")), self.label(text(b, "mail_id"))))
        });
        rows
    }

    async fn run(&mut self, op: &str, args: &Value) -> Outcome {
        let user_id = || text(args, "user_id");
        let mail_id = || text(args, "mail_id");
        Ok(match op {
            "sql" => {
                let values: Vec<Param> = args["values"].as_array().expect("values is a list").iter().map(param).collect();
                json!({ "rowCount": pool().query(text(args, "sql"), &values).await?.row_count })
            }
            "writeAttachment" => {
                std::fs::create_dir_all("attachments").expect("the attachments folder");
                let bytes = vec![b'a'; integer(args, "bytes") as usize];
                std::fs::write(std::path::Path::new("attachments").join(text(args, "id")), bytes)
                    .expect("the attachment file");
                Value::Null
            }

            "getDomainUidNext" => json!(get_domain_uid_next(user_id(), flag(args, "sent")).await?),
            "getAccountUidNext" => {
                json!(get_account_uid_next(user_id(), text(args, "account"), flag(args, "sent")).await?)
            }
            "getMailboxUidNext" => json!(get_mailbox_uid_next(user_id(), text(args, "mailbox")).await?),
            "getUidNext" => json!(get_uid_next(user_id(), &uid_scope(&args["scope"])).await?),
            "getNextModseq" => json!(get_next_modseq(user_id()).await?),
            "getHighestModseq" => {
                json!(get_highest_modseq(user_id(), opt_text(args, "mailbox"), flag(args, "sent")).await)
            }
            "deleteMailboxUid" => {
                delete_mailbox_uid(user_id(), text(args, "mailbox"), mail_id()).await?;
                Value::Null
            }
            "syncMailboxPivot" => {
                sync_mailbox_pivot(user_id(), text(args, "mailbox"), mail_id(), flag(args, "isPresent")).await?;
                Value::Null
            }
            "writeMailboxUid" => {
                json!(write_mailbox_uid(user_id(), text(args, "mailbox"), mail_id(), integer(args, "uid")).await?)
            }

            "saveMail" => {
                let saved = save_mail(&save_mail_input(args)).await?;
                self.refresh_labels().await;
                if let (Some(saved), None) = (&saved, args.get("date")) {
                    self.saved_now.insert(saved.id.clone());
                }
                optional(saved, saved_json)
            }
            "getMailById" => optional(get_mail_by_id(user_id(), mail_id()).await?, MailModel::to_json),
            "getMailByMessageId" => {
                optional(get_mail_by_message_id(user_id(), text(args, "message_id")).await?, MailModel::to_json)
            }
            "findMailByAttachment" => {
                let holding = json!([{ "content": { "data": text(args, "attachment_id") } }]);
                let row = mails_table()
                    .query_one(
                        &[("user_id", SelectFilter::from(user_id())), ("attachments", SelectFilter::Contains(holding))],
                        None,
                    )
                    .await?;
                match row {
                    Some(row) => MailModel::from_record(&row).map_err(anyhow::Error::from)?.to_json(),
                    None => Value::Null,
                }
            }
            "updateRfc822Size" => {
                update_rfc822_size(user_id(), mail_id(), integer(args, "rfc822_size")).await?;
                Value::Null
            }
            "markMailRead" => json!(mark_mail_read(user_id(), mail_id()).await?),
            "markMailSaved" => json!(mark_mail_saved(user_id(), mail_id(), flag(args, "saved")).await?),
            "markMailSpam" => {
                let result = mark_mail_spam(user_id(), mail_id(), flag(args, "is_spam")).await?;
                json!({ "found": result.found, "changed": result.changed })
            }
            "deleteMail" => json!(delete_mail(user_id(), mail_id()).await?),
            "recordMappings" => {
                let writes: Vec<MappingWrite> = args["writes"]
                    .as_array()
                    .expect("writes is a list")
                    .iter()
                    .map(|write| MappingWrite {
                        mailbox: text(write, "mailbox").to_string(),
                        uid: integer(write, "uid"),
                        scope: match text(write, "scope") {
                            "mailbox" => MappingScope::Mailbox,
                            "domain" => MappingScope::Domain,
                            other => panic!("no scope {other}"),
                        },
                    })
                    .collect();
                json!(record_mappings(user_id(), mail_id(), &writes, opt_text(args, "mappedDestination")).await?)
            }
            "syncMappedPivotsForRow" => {
                sync_mapped_pivots_for_row(
                    user_id(),
                    mail_id(),
                    opt_flag(args, "saved"),
                    opt_flag(args, "deleted"),
                    opt_text(args, "destMailbox"),
                )
                .await?;
                Value::Null
            }

            // An IMAP STORE: the statements the builder returns, one reserved
            // mod-sequence, then the Starred / Trash pivots of every row the
            // STORE touched.
            "setFlags" => {
                let conditional = !args["unchangedSince"].is_null();
                let set_clause = text(args, "setClause");
                let queries = build_set_mail_flags_queries(
                    user_id(),
                    opt_text(args, "mailbox"),
                    flag(args, "sent"),
                    integer(args, "start"),
                    integer(args, "end"),
                    flag(args, "useUid"),
                    set_clause,
                    conditional,
                );
                if set_clause.is_empty() {
                    let selected = pool().query(&queries.select_sql, &queries.base_values).await?;
                    return Ok(json!({ "selected": self.flag_rows(&selected.rows) }));
                }
                let modseq = get_next_modseq(user_id()).await?;
                let matched = if conditional {
                    let rows = pool().query(&queries.matched_uid_sql, &queries.base_values).await?.rows;
                    let mut uids: Vec<i64> = rows.iter().map(|row| row.i64("uid")).collect();
                    uids.sort();
                    json!(uids)
                } else {
                    Value::Null
                };
                let mut values = queries.base_values.clone();
                values.push(Param::from(modseq));
                if conditional {
                    values.push(param(&args["unchangedSince"]));
                }
                let updated = self.flag_rows(&pool().query(&queries.update_sql, &values).await?.rows);
                for row in &updated {
                    if flag(args, "touchesSaved") {
                        sync_mailbox_pivot(user_id(), "Starred", text(row, "mail_id"), flag(row, "saved")).await?;
                    }
                    if flag(args, "touchesDeleted") {
                        sync_mailbox_pivot(user_id(), "Trash", text(row, "mail_id"), flag(row, "deleted")).await?;
                    }
                }
                json!({ "modseq": modseq, "matched": matched, "updated": updated })
            }

            "concurrentReservations" => {
                let reserve = || async {
                    match text(args, "kind") {
                        "domain" => get_domain_uid_next(user_id(), flag(args, "sent")).await,
                        "account" => get_account_uid_next(user_id(), text(args, "account"), flag(args, "sent")).await,
                        "mailbox" => get_mailbox_uid_next(user_id(), text(args, "mailbox")).await,
                        "modseq" => get_next_modseq(user_id()).await,
                        other => panic!("no counter {other}"),
                    }
                };
                let reserved = join_all((0..integer(args, "count")).map(|_| reserve())).await;
                let mut reserved = reserved.into_iter().collect::<Result<Vec<i64>, DbError>>()?;
                reserved.sort();
                json!(reserved)
            }

            // Deliveries of different mails to one account, all at once: each
            // reserves its UIDs and saves, as concurrent SMTP sessions do.
            "concurrentSaves" => {
                let account = text(args, "account");
                let saved = join_all((0..integer(args, "count")).map(|index| async move {
                    let (uid_domain, uid_mailbox) =
                        tokio::join!(get_domain_uid_next(user_id(), false), get_account_uid_next(user_id(), account, false));
                    save_mail(&SaveMailInput {
                        user_id: user_id().to_string(),
                        message_id: format!("<conc-{index}@example.com>"),
                        date: Some(DATE.to_string()),
                        subject: Some("Concurrent delivery".to_string()),
                        text: Some("one of several".to_string()),
                        envelope_to: Some(json!([{ "address": account }])),
                        saved: flag(args, "saved"),
                        uid_domain: Some(uid_domain?),
                        uid_mailbox: Some(uid_mailbox?),
                        mailbox: Some(text(args, "mailbox").to_string()),
                        domain_mailbox: Some("INBOX".to_string()),
                        ..Default::default()
                    })
                    .await
                }))
                .await;
                let saved = saved.into_iter().collect::<anyhow::Result<Vec<Option<SavedMail>>>>()?;
                self.refresh_labels().await;
                let rows = pool()
                    .query(
                        "SELECT modseq FROM mails WHERE user_id = $1 AND message_id LIKE '<conc-%' ORDER BY modseq",
                        &[Param::from(user_id())],
                    )
                    .await?;
                let starred = pool()
                    .query(
                        "SELECT x.uid FROM mail_mailbox_uid x JOIN mails m ON m.mail_id = x.mail_id WHERE x.user_id = $1 AND x.mailbox = 'Starred' AND m.message_id LIKE '<conc-%' ORDER BY x.uid",
                        &[Param::from(user_id())],
                    )
                    .await?;
                let stored: HashSet<Option<&str>> = saved.iter().map(|result| result.as_ref().map(|s| s.id.as_str())).collect();
                let sorted = |pick: fn(&SavedMail) -> Option<i64>| {
                    let mut uids: Vec<i64> =
                        saved.iter().map(|result| result.as_ref().and_then(pick).unwrap_or(-1)).collect();
                    uids.sort();
                    uids
                };
                json!({
                    "stored": stored.len(),
                    "uid_domain": sorted(|saved| saved.uid_domain),
                    "uid_mailbox": sorted(|saved| saved.uid_mailbox),
                    "modseq": rows.rows.iter().map(|row| row.i64("modseq")).collect::<Vec<_>>(),
                    "starred": starred.rows.iter().map(|row| row.i64("uid")).collect::<Vec<_>>(),
                })
            }

            // One Message-ID delivered several times at once: exactly one
            // INSERT wins, the others merge into its row.
            "concurrentDuplicates" => {
                let account = text(args, "account");
                let count = integer(args, "count");
                let before = modseq_counter(user_id()).await?;
                let saved = join_all((0..count).map(|index| async move {
                    let reserved = get_account_uid_next(user_id(), account, false).await?;
                    let result = save_mail(&SaveMailInput {
                        user_id: user_id().to_string(),
                        message_id: "<race-1@example.com>".to_string(),
                        date: Some(DATE.to_string()),
                        subject: Some("Raced delivery".to_string()),
                        text: Some("the same message".to_string()),
                        envelope_to: Some(json!([{ "address": format!("rcpt-{index}@example.com") }])),
                        uid_domain: Some(integer(args, "uid_domain")),
                        uid_mailbox: Some(reserved),
                        mailbox: Some(text(args, "mailbox").to_string()),
                        domain_mailbox: Some("INBOX".to_string()),
                        ..Default::default()
                    })
                    .await?;
                    anyhow::Ok((reserved, result))
                }))
                .await;
                let saved = saved.into_iter().collect::<anyhow::Result<Vec<(i64, Option<SavedMail>)>>>()?;
                self.refresh_labels().await;
                let after = modseq_counter(user_id()).await?;
                let row = pool()
                    .query(
                        "SELECT modseq, uid_domain, jsonb_array_length(envelope_to) AS recipients FROM mails WHERE user_id = $1 AND message_id = '<race-1@example.com>'",
                        &[Param::from(user_id())],
                    )
                    .await?
                    .into_first()
                    .expect("the raced mail is stored");
                let distinct = |pick: fn(&SavedMail) -> Option<i64>| {
                    let mut seen: Vec<i64> = Vec::new();
                    for (_, result) in &saved {
                        let value = result.as_ref().and_then(pick).unwrap_or(-1);
                        if !seen.contains(&value) {
                            seen.push(value);
                        }
                    }
                    seen
                };
                let rows: HashSet<Option<&str>> =
                    saved.iter().map(|(_, result)| result.as_ref().map(|s| s.id.as_str())).collect();
                let persisted = distinct(|saved| saved.uid_mailbox);
                json!({
                    "rows": rows.len(),
                    "uid_domain": distinct(|saved| saved.uid_domain),
                    "everyone_reports_one_mailbox_uid": persisted.len() == 1,
                    "that_uid_was_reserved_by_one_of_them": saved.iter().any(|(reserved, _)| Some(reserved) == persisted.first()),
                    "modseq_reserved_in_this_batch": row.i64("modseq") > before && row.i64("modseq") <= after,
                    "modseqs_reserved": after - before,
                    "stored_uid_domain": row.i64("uid_domain"),
                    // A merge reads the row, then rewrites its envelope: merges
                    // that overlap can each keep only their own recipient.
                    "recipients_within_bounds": row.i64("recipients") >= 1 && row.i64("recipients") <= count,
                })
            }

            "getMailboxesByUser" => {
                Value::Array(get_mailboxes_by_user(user_id()).await?.iter().map(MailboxModel::to_json).collect())
            }
            "getMailboxByName" => optional(get_mailbox_by_name(user_id(), text(args, "name")).await?, MailboxModel::to_json),
            "getMailboxByAddress" => {
                optional(get_mailbox_by_address(user_id(), text(args, "address")).await?, MailboxModel::to_json)
            }
            "createMailbox" => {
                let input = CreateMailboxInput {
                    user_id: user_id().to_string(),
                    name: text(args, "name").to_string(),
                    address: opt_text(args, "address").map(str::to_string),
                    parent_id: opt_text(args, "parent_id").map(str::to_string),
                    special_use: opt_text(args, "special_use").map(str::to_string),
                    subscribed: opt_flag(args, "subscribed"),
                };
                let result = create_mailbox(&input).await?;
                self.refresh_labels().await;
                match result {
                    CreateMailboxResult::Created(mailbox) => json!({ "status": "created", "mailbox": mailbox.to_json() }),
                    CreateMailboxResult::Exists => json!({ "status": "exists" }),
                    CreateMailboxResult::AtLimit => json!({ "status": "at_limit" }),
                }
            }
            "deleteMailboxByName" => match delete_mailbox_by_name(user_id(), text(args, "name")).await? {
                DeleteMailboxResult::Deleted => json!("deleted"),
                DeleteMailboxResult::NotFound => json!("not_found"),
                DeleteMailboxResult::Protected => json!("protected"),
            },
            "renameMailbox" => match rename_mailbox(user_id(), text(args, "oldName"), text(args, "newName")).await? {
                RenameMailboxResult::Renamed => json!("renamed"),
                RenameMailboxResult::NotFound => json!("not_found"),
                RenameMailboxResult::Protected => json!("protected"),
                RenameMailboxResult::NameTaken => json!("name_taken"),
            },
            "setMailboxSubscribed" => {
                json!(set_mailbox_subscribed(user_id(), text(args, "name"), flag(args, "subscribed")).await?)
            }
            other => panic!("no op {other}"),
        })
    }
}

async fn modseq_counter(user_id: &str) -> Result<i64, DbError> {
    let result = pool()
        .query(
            "SELECT last_uid FROM mail_uid_counters WHERE user_id = $1 AND uid_kind = 'modseq'",
            &[Param::from(user_id)],
        )
        .await?;
    Ok(result.rows.first().map(|row| row.i64("last_uid")).unwrap_or(0))
}

const MAIL_COLUMNS: &str = "mail_id, user_id, message_id, subject, date, html, text, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, reply_to_address, reply_to_text, envelope_from, envelope_to, attachments, read, saved, sent, deleted, draft, answered, expunged, insight, uid_domain, modseq, spam_score, spam_reasons, is_spam, rfc822_size, text_line_count, html_line_count, in_reply_to, reference_ids, search_vector::text AS search_vector";

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
        let alias = opt_text(step, "as");
        let args = harness.resolve(&step["args"]);
        let mut entry = Map::new();
        entry.insert("step".to_string(), json!(index + 1));
        entry.insert("op".to_string(), json!(op));
        if let Some(alias) = alias {
            entry.insert("as".to_string(), json!(alias));
        }
        match harness.run(op, &args).await {
            Ok(returned) => {
                // Later steps refer to what this one returned as it was
                // returned; the record holds the normalized form.
                entry.insert("ok".to_string(), harness.normalize(&returned));
                if let Some(alias) = alias {
                    harness.results.insert(alias.to_string(), returned);
                }
            }
            Err(thrown) => {
                harness.refresh_labels().await;
                entry.insert(
                    "threw".to_string(),
                    json!({ "code": thrown.code, "constraint": thrown.constraint, "message": thrown.message }),
                );
                if let Some(alias) = alias {
                    harness.results.insert(alias.to_string(), Value::Null);
                }
            }
        }
        recorded.push(Value::Object(entry));
    }

    harness.refresh_labels().await;
    let mails = table(&format!("SELECT {MAIL_COLUMNS} FROM mails ORDER BY user_id, message_id COLLATE \"C\"")).await;
    let raced: HashSet<&str> =
        mails.iter().filter(|row| is_raced(row.get("message_id"))).map(|row| text(row, "mail_id")).collect();
    let mut mappings: Vec<Value> =
        table("SELECT user_id, mailbox, mail_id, uid FROM mail_mailbox_uid ORDER BY user_id, mailbox COLLATE \"C\", uid")
            .await
            .into_iter()
            .map(|mut row| {
                if raced.contains(text(&row, "mail_id")) {
                    row["uid"] = json!("<conc>");
                }
                harness.normalize(&row)
            })
            .collect();
    mappings.sort_by(|a, b| js_order(&a.to_string(), &b.to_string()));
    let counters = table(
        "SELECT user_id, uid_kind, uid_scope, sent, last_uid FROM mail_uid_counters ORDER BY user_id, uid_kind, uid_scope COLLATE \"C\", sent",
    )
    .await;
    let boxes = table(
        "SELECT mailbox_id, user_id, name, address, parent_id, uid_validity, uid_next, subscribed, special_use FROM mailboxes WHERE name NOT LIKE 'bulk-%' ORDER BY user_id, name COLLATE \"C\"",
    )
    .await;
    let counts = table("SELECT user_id, COUNT(*)::int AS count FROM mailboxes GROUP BY user_id ORDER BY user_id").await;

    let normalized = |rows: &[Value]| -> Vec<Value> { rows.iter().map(|row| harness.normalize(row)).collect() };
    json!({
        "results": recorded,
        "tables": {
            "mails": normalized(&mails),
            "mail_mailbox_uid": mappings,
            "mail_uid_counters": normalized(&counters),
            "mailboxes": normalized(&boxes),
            "mailbox_counts": normalized(&counts),
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
                let name = match (a.get("op").and_then(Value::as_str), a.get("as").and_then(Value::as_str)) {
                    (Some(op), Some(alias)) => format!("{path}[{index}] ({op} {alias})"),
                    (Some(op), None) => format!("{path}[{index}] ({op})"),
                    _ => format!("{path}[{index}]"),
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
async fn the_rust_repositories_leave_the_database_the_bun_repositories_leave() {
    if std::env::var("MAIL_TEST_POSTGRES").as_deref() != Ok("1") {
        eprintln!("skipped: set MAIL_TEST_POSTGRES=1 to run the differential database test");
        return;
    }

    let server = Server::from_env();
    let database = format!("mail_core_rs_{}", std::process::id());
    server.admin(&format!("DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)")).await;
    server.admin(&format!("CREATE DATABASE \"{database}\"")).await;
    // The pool reads these on first use; nothing has used it yet.
    std::env::set_var("POSTGRES_HOST", &server.host);
    std::env::set_var("POSTGRES_PORT", &server.port);
    std::env::set_var("POSTGRES_USER", &server.user);
    std::env::set_var("POSTGRES_PASSWORD", &server.password);
    std::env::set_var("POSTGRES_DATABASE", &database);
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
        "the Rust port differs from the Bun server in {} places:\n{}",
        found.len(),
        found.iter().take(60).cloned().collect::<Vec<_>>().join("\n")
    );
    assert!(actual["results"].as_array().is_some_and(|results| results.len() >= 300), "every step ran");
}
