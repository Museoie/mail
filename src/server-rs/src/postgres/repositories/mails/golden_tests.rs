//! Golden tests: this port against what the Bun server's repositories build,
//! issue and return.
//!
//! The fixtures in `tests/golden/storage/` are generated from the real
//! TypeScript by `generate.ts` there. `builders.json` holds the output of the
//! pure functions; `traces.json` holds, for each repository function and
//! branch, the statements it issues in order and what it returns when the
//! pool answers from a script.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use super::core::{
    delete_mail, get_mail_by_id, get_mail_by_message_id, mark_mail_read, mark_mail_saved, mark_mail_spam,
    record_mappings, save_mail, sync_mapped_pivots_for_row, update_rfc822_size, Placement, SaveMailInput,
};
use super::core_query::{build_mail_exists_query, build_mark_mail_spam_query};
use super::counters::*;
use super::mapping_decisions::{decide_mapping_writes, MappingScope, MappingWrite, MappingWriteInput};
use super::model::*;
use super::pivot_decisions::{decide_mapped_pivots, STARRED_MAILBOX, TRASH_MAILBOX};
use super::rfc822_size::{compute_full_message_size, MailForSize, ATTACHMENT_FOLDER_OVERRIDE};
use super::set_flags_query::build_set_mail_flags_queries;
use super::table::script::with_script;
use super::table::{CompareOp, Filter, PreparedQuery, Record, Rows, SelectFilter, SetValue, Table};
use super::views::*;
use crate::js;
use crate::postgres::repositories::mailboxes::*;
use crate::postgres::{DbError, DbResult, Param};

const BUILDERS: &str = include_str!("../../../../tests/golden/storage/builders.json");
const TRACES: &str = include_str!("../../../../tests/golden/storage/traces.json");

fn builders() -> Value {
    serde_json::from_str(BUILDERS).expect("builders.json is JSON")
}

fn list<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    value[key].as_array().unwrap_or_else(|| panic!("{key} is a list"))
}

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

/// A bound value as the pool sends it: everything is text, NULL aside.
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

fn params_of(values: &Value) -> Vec<Param> {
    values.as_array().expect("values is a list").iter().map(param).collect()
}

fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

fn is_iso_timestamp(text: &str) -> bool {
    text.len() == 24 && text.ends_with('Z') && js::date_parse(text).is_some() && text.as_bytes()[10] == b'T'
}

/// Generated ids and the save time differ per run; everything else is exact.
struct Normalizer {
    fixed_ids: Vec<String>,
    fixed_date: String,
}

impl Normalizer {
    fn text(&self, text: &str) -> String {
        if is_uuid(text) && !self.fixed_ids.iter().any(|id| id == text) {
            "<uuid>".to_string()
        } else if is_iso_timestamp(text) && text != self.fixed_date {
            "<now>".to_string()
        } else {
            text.to_string()
        }
    }

    fn param(&self, value: &Param) -> Param {
        match value {
            Param::Text(text) => Param::Text(self.text(text)),
            other => other.clone(),
        }
    }

    fn json(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.json(item)).collect()),
            Value::Object(object) => {
                Value::Object(object.iter().map(|(key, item)| (key.clone(), self.json(item))).collect())
            }
            other => other.clone(),
        }
    }
}

fn assert_query(label: &str, actual: &PreparedQuery, expected: &Value) {
    assert_eq!(actual.sql, expected["sql"].as_str().unwrap(), "{label}: sql");
    assert_eq!(actual.values, params_of(&expected["values"]), "{label}: values");
}

// ---------------------------------------------------------------------------
// builders.json
// ---------------------------------------------------------------------------

#[test]
fn statement_builders_emit_the_same_sql_and_values() {
    let fixture = builders();
    let cases = list(&fixture, "statements");
    assert!(cases.len() >= 30);
    for case in cases {
        let args = &case["args"];
        let user_id = text(args, "user_id");
        let name = case["fn"].as_str().unwrap();
        let query = match name {
            "buildDomainUidQuery" => build_domain_uid_query(user_id, flag(args, "sent")),
            "buildDomainUidNextQuery" => build_domain_uid_next_query(user_id, flag(args, "sent")),
            "buildAccountUidQuery" => build_account_uid_query(user_id, text(args, "account"), flag(args, "sent")),
            "buildAccountUidNextQuery" => {
                build_account_uid_next_query(user_id, text(args, "account"), flag(args, "sent"))
            }
            "buildMailboxUidQuery" => build_mailbox_uid_query(user_id, text(args, "mailbox")),
            "buildMailboxUidNextQuery" => build_mailbox_uid_next_query(user_id, text(args, "mailbox")),
            "buildWriteMailboxUidQuery" => {
                build_write_mailbox_uid_query(user_id, text(args, "mailbox"), text(args, "mail_id"), integer(args, "uid"))
            }
            "buildModseqQuery" => build_modseq_query(user_id),
            "buildMarkMailSpamQuery" => {
                build_mark_mail_spam_query(user_id, text(args, "mail_id"), flag(args, "is_spam"), integer(args, "modseq"))
            }
            "buildMailExistsQuery" => build_mail_exists_query(user_id, text(args, "mail_id")),
            other => panic!("no Rust builder for {other}"),
        };
        assert_query(&format!("{name} {args}"), &query, case);
    }
}

#[test]
fn set_flags_queries_match_for_every_branch() {
    let fixture = builders();
    let cases = list(&fixture, "setFlags");
    assert_eq!(cases.len(), 64);
    for case in cases {
        let args = &case["args"];
        let queries = build_set_mail_flags_queries(
            text(args, "user_id"),
            opt_text(args, "mailbox"),
            flag(args, "sent"),
            integer(args, "start"),
            integer(args, "end"),
            flag(args, "useUid"),
            text(args, "setClause"),
            flag(args, "conditional"),
        );
        assert_eq!(queries.select_sql, case["selectSql"].as_str().unwrap(), "selectSql {args}");
        assert_eq!(queries.matched_uid_sql, case["matchedUidSql"].as_str().unwrap(), "matchedUidSql {args}");
        assert_eq!(queries.update_sql, case["updateSql"].as_str().unwrap(), "updateSql {args}");
        assert_eq!(queries.base_values, params_of(&case["baseValues"]), "baseValues {args}");
    }
}

#[test]
fn view_rules_match_for_every_mailbox_name() {
    let fixture = builders();
    let cases = list(&fixture, "views");
    assert!(cases.len() >= 80);
    for case in cases {
        let mailbox = opt_text(case, "mailbox");
        let sent = flag(case, "sent");
        let prefix = text(case, "prefix");
        assert_eq!(is_utility_view(mailbox), flag(case, "isUtilityView"), "isUtilityView {case}");
        assert_eq!(uses_domain_uid_space(mailbox), flag(case, "usesDomainUidSpace"), "usesDomainUidSpace {case}");
        assert_eq!(is_inbox_tree(mailbox, sent), flag(case, "isInboxTree"), "isInboxTree {case}");
        let filter: Vec<Value> =
            membership_filter(mailbox, sent).into_iter().map(|(column, value)| json!([column, value])).collect();
        assert_eq!(Value::Array(filter), case["filter"], "membershipFilter {case}");
        assert_eq!(membership_expression(mailbox, sent, prefix), text(case, "expression"), "expression {case}");
        assert_eq!(membership_condition(mailbox, sent, prefix), text(case, "condition"), "condition {case}");
    }
    for case in list(&fixture, "destinations") {
        assert_eq!(
            domain_view_for_destination(text(case, "destination")),
            opt_text(case, "view"),
            "domainViewForDestination {case}"
        );
    }
}

fn scope_name(scope: MappingScope) -> &'static str {
    match scope {
        MappingScope::Mailbox => "mailbox",
        MappingScope::Domain => "domain",
    }
}

fn mapping_write_input(input: &Value) -> MappingWriteInput {
    MappingWriteInput {
        mailbox: opt_text(input, "mailbox").map(str::to_string),
        uid_mailbox: opt_integer(input, "uid_mailbox"),
        domain_mailbox: opt_text(input, "domain_mailbox").map(str::to_string),
        uid_domain: opt_integer(input, "uid_domain"),
        sent: flag(input, "sent"),
    }
}

#[test]
fn mapping_and_pivot_decisions_match() {
    let fixture = builders();
    let cases = list(&fixture, "mappingWrites");
    assert!(cases.len() >= 200);
    for case in cases {
        let writes: Vec<Value> = decide_mapping_writes(&mapping_write_input(&case["input"]))
            .into_iter()
            .map(|write| json!({ "mailbox": write.mailbox, "uid": write.uid, "scope": scope_name(write.scope) }))
            .collect();
        assert_eq!(Value::Array(writes), case["writes"], "decideMappingWrites {}", case["input"]);
    }

    let cases = list(&fixture, "mappedPivots");
    assert_eq!(cases.len(), 63);
    for case in cases {
        let decisions: Vec<Value> =
            decide_mapped_pivots(opt_flag(case, "saved"), opt_flag(case, "deleted"), opt_text(case, "destMailbox"))
                .into_iter()
                .map(|decision| json!({ "mailbox": decision.mailbox, "present": decision.present }))
                .collect();
        assert_eq!(Value::Array(decisions), case["decisions"], "decideMappedPivots {case}");
        // core.ts carries its own copy of the rule; here it is one function.
        assert_eq!(
            super::core::decide_mapped_pivots(opt_flag(case, "saved"), None, None),
            decide_mapped_pivots(opt_flag(case, "saved"), None, None)
        );
    }
}

fn names(values: &Value) -> Vec<&str> {
    values.as_array().unwrap().iter().map(|value| value.as_str().unwrap()).collect()
}

fn model_outcome<T>(result: Result<T, ModelValidationError>, to_json: impl Fn(&T) -> Value) -> Value {
    match result {
        Ok(model) => json!({ "ok": to_json(&model) }),
        Err(error) => json!({ "threw": error.to_string() }),
    }
}

#[test]
fn models_validate_and_serialize_the_same() {
    let fixture = builders();
    let models = &fixture["models"];

    let checker_keys: Vec<&str> = MAIL_TYPE_CHECKER.iter().map(|(name, _)| *name).collect();
    assert_eq!(checker_keys, names(&models["mailTypeCheckerKeys"]));
    assert_eq!(PartialMailModel::valid_fields(), names(&models["partialValidFields"]));
    assert_eq!(mail_columns(), names(&models["mailColumns"]));
    assert_eq!(mailbox_columns(), names(&models["mailboxColumns"]));
    assert_eq!(mail_uid_counters_table().columns(), names(&models["mailUidCounterColumns"]));
    assert_eq!(mail_mailbox_uid_table().columns(), names(&models["mailMailboxUidColumns"]));

    let constants = &models["constants"];
    assert_eq!(MAILBOX_NAME_MAX_BYTES as i64, integer(constants, "MAILBOX_NAME_MAX_BYTES"));
    assert_eq!(MAILBOX_COUNT_MAX, integer(constants, "MAILBOX_COUNT_MAX"));
    assert_eq!(INBOX_VIEW, text(constants, "INBOX_VIEW"));
    assert_eq!(DRAFTS_VIEW, text(constants, "DRAFTS_VIEW"));
    assert_eq!(JUNK_VIEW, text(constants, "JUNK_VIEW"));
    assert_eq!(STARRED_MAILBOX, text(constants, "STARRED_MAILBOX"));
    assert_eq!(TRASH_MAILBOX, text(constants, "TRASH_MAILBOX"));

    for case in list(models, "mail") {
        let actual = model_outcome(MailModel::from_value(&case["data"]), MailModel::to_json);
        assert_eq!(actual, case["result"], "MailModel {}", case["data"]);
    }

    for case in list(models, "partialMail") {
        let fields = names(&case["fields"]);
        let actual = model_outcome(PartialMailModel::from_value(&fields, &case["data"]), |partial| {
            let selected: BTreeSet<&str> = partial.selected_fields().iter().map(String::as_str).collect();
            let values: Map<String, Value> = fields
                .iter()
                .filter_map(|field| partial.get(field).map(|value| (field.to_string(), value.clone())))
                .collect();
            json!({ "selectedFields": selected, "values": values })
        });
        assert_eq!(actual, case["result"], "PartialMailModel {} {}", case["fields"], case["data"]);
    }

    for case in list(models, "mailbox") {
        let actual = model_outcome(MailboxModel::from_value(&case["data"]), MailboxModel::to_json);
        assert_eq!(actual, case["result"], "MailboxModel {}", case["data"]);
    }
    for case in list(models, "mailUidCounter") {
        let record = case["data"].as_object().unwrap();
        let actual = model_outcome(MailUidCounterModel::from_record(record), MailUidCounterModel::to_json);
        assert_eq!(actual, case["result"], "MailUidCounterModel {}", case["data"]);
    }
    for case in list(models, "mailMailboxUid") {
        let record = case["data"].as_object().unwrap();
        let actual = model_outcome(MailMailboxUidModel::from_record(record), MailMailboxUidModel::to_json);
        assert_eq!(actual, case["result"], "MailMailboxUidModel {}", case["data"]);
    }

    let checker = [("name", FieldType::String), ("age", FieldType::Number)];
    for case in list(models, "validateObject") {
        let skip = names(&case["skip"]);
        let errors = validate_object(&case["input"], &checker, &skip);
        assert_eq!(json!(errors), case["errors"], "validateObject {}", case["input"]);
    }
}

/// A directory holding the attachment file the fixtures measure, in place of
/// `./attachments` for as long as the guard lives.
struct AttachmentsOnDisk {
    _directory: tempfile::TempDir,
}

impl AttachmentsOnDisk {
    fn new(fixture: &Value) -> AttachmentsOnDisk {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let on_disk = &fixture["attachmentOnDisk"];
        let bytes = vec![b'a'; integer(on_disk, "bytes") as usize];
        std::fs::write(directory.path().join(text(on_disk, "id")), bytes).expect("the attachment file");
        let folder = directory.path().to_string_lossy().into_owned();
        ATTACHMENT_FOLDER_OVERRIDE.with(|current| *current.borrow_mut() = Some(folder));
        AttachmentsOnDisk { _directory: directory }
    }
}

impl Drop for AttachmentsOnDisk {
    fn drop(&mut self) {
        ATTACHMENT_FOLDER_OVERRIDE.with(|current| *current.borrow_mut() = None);
    }
}

#[test]
fn rfc822_sizes_match_the_imap_serializer() {
    let fixture = builders();
    let sizes = &fixture["rfc822Size"];
    let _attachments = AttachmentsOnDisk::new(sizes);
    let cases = list(sizes, "cases");
    assert_eq!(cases.len(), 80);
    for case in cases {
        let input = &case["input"];
        let size = compute_full_message_size(&MailForSize {
            message_id: text(input, "message_id"),
            date: text(input, "date"),
            subject: opt_text(input, "subject"),
            text_octets: integer(input, "text_octets") as usize,
            html_octets: integer(input, "html_octets") as usize,
            mail_id: text(input, "mail_id"),
            attachments: input.get("attachments"),
            from_text: opt_text(input, "from_text"),
            to_text: opt_text(input, "to_text"),
            cc_text: opt_text(input, "cc_text"),
            bcc_text: opt_text(input, "bcc_text"),
            reply_to_text: opt_text(input, "reply_to_text"),
        });
        match (&size, case["result"].get("ok")) {
            (Ok(size), Some(expected)) => assert_eq!(json!(size), *expected, "size of {input}"),
            (Err(_), None) => {}
            _ => panic!("size of {input}: {size:?} against {}", case["result"]),
        }
    }
}

#[test]
fn mailbox_names_sort_like_locale_compare() {
    let fixture = builders();
    let mut sorted = names(&fixture["localeCompare"]["names"]);
    sorted.sort_by(|a, b| locale_compare(a, b));
    assert_eq!(sorted, names(&fixture["localeCompare"]["sorted"]));
}

// ---------------------------------------------------------------------------
// traces.json
// ---------------------------------------------------------------------------

/// A failure a repository function reported.
struct Thrown {
    message: String,
    code: Option<String>,
}

impl From<DbError> for Thrown {
    fn from(error: DbError) -> Thrown {
        Thrown { message: error.message, code: error.code }
    }
}

impl From<anyhow::Error> for Thrown {
    fn from(error: anyhow::Error) -> Thrown {
        Thrown {
            message: error.to_string(),
            code: error.downcast_ref::<DbError>().and_then(|error| error.code.clone()),
        }
    }
}

impl From<ModelValidationError> for Thrown {
    fn from(error: ModelValidationError) -> Thrown {
        Thrown { message: error.to_string(), code: None }
    }
}

type Outcome = Result<Value, Thrown>;

fn scripted(responses: &Value) -> Vec<DbResult<Rows>> {
    responses
        .as_array()
        .expect("responses is a list")
        .iter()
        .map(|response| match response.get("error") {
            Some(error) => Err(DbError {
                code: opt_text(error, "code").map(str::to_string),
                ..DbError::new(text(error, "message"))
            }),
            None => {
                let rows: Vec<Record> =
                    list(response, "rows").iter().map(|row| row.as_object().unwrap().clone()).collect();
                let row_count = response.get("rowCount").and_then(Value::as_u64).unwrap_or(rows.len() as u64);
                Ok(Rows { rows, row_count: Some(row_count) })
            }
        })
        .collect()
}

fn table_named(name: &str) -> Table {
    match name {
        "mails" => mails_table(),
        "mailboxes" => mailboxes_table(),
        "mail_uid_counters" => mail_uid_counters_table(),
        "mail_mailbox_uid" => mail_mailbox_uid_table(),
        other => panic!("no table {other}"),
    }
}

fn is_marker(value: &Value, marker: &str) -> bool {
    value.get(marker) == Some(&Value::Bool(true))
}

/// A SELECT's filters: a document is matched by containment, anything else
/// by equality.
fn select_filters(filters: &Value) -> Vec<(&str, SelectFilter)> {
    filters
        .as_object()
        .unwrap()
        .iter()
        .map(|(column, value)| {
            let filter = match value {
                Value::Array(_) | Value::Object(_) => SelectFilter::Contains(value.clone()),
                scalar => SelectFilter::Equals(param(scalar)),
            };
            (column.as_str(), filter)
        })
        .collect()
}

fn mutation_filters(filters: &Value) -> Vec<(&str, Filter)> {
    filters
        .as_object()
        .unwrap()
        .iter()
        .map(|(column, value)| {
            let filter = if is_marker(value, "$undefined") {
                Filter::Missing
            } else if let Some(op) = value.get("op").and_then(Value::as_str) {
                let not_null = value.get("notNull") == Some(&Value::Bool(true));
                let operand = value.get("value").filter(|operand| !is_marker(operand, "$undefined"));
                match op {
                    "IS NULL" => Filter::IsNull,
                    "IS NOT TRUE" => Filter::IsNotTrue,
                    "IN" => Filter::In {
                        values: operand.map(|operand| operand.as_array().unwrap().iter().map(param).collect()),
                        not_null,
                    },
                    op => Filter::Compare {
                        op: match op {
                            "=" => CompareOp::Eq,
                            "<" => CompareOp::Lt,
                            "<=" => CompareOp::Le,
                            ">" => CompareOp::Gt,
                            ">=" => CompareOp::Ge,
                            other => panic!("no operator {other}"),
                        },
                        value: operand.map(param),
                        not_null,
                    },
                }
            } else {
                Filter::Equals(param(value))
            };
            (column.as_str(), filter)
        })
        .collect()
}

fn insert_values(data: &Value) -> Vec<(&str, Param)> {
    data.as_object().unwrap().iter().map(|(column, value)| (column.as_str(), param(value))).collect()
}

fn set_values(data: &Value) -> Vec<(&str, SetValue)> {
    data.as_object()
        .unwrap()
        .iter()
        .map(|(column, value)| {
            let value = if is_marker(value, "$dbNow") { SetValue::DbNow } else { SetValue::Value(param(value)) };
            (column.as_str(), value)
        })
        .collect()
}

fn columns_of(value: &Value) -> Vec<&str> {
    value.as_array().map(|columns| columns.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

/// A row as the Bun `Table` hands it back: a model's JSON form.
fn model_json(table: &str, record: &Record) -> Result<Value, Thrown> {
    Ok(match table {
        "mails" => MailModel::from_record(record)?.to_json(),
        "mailboxes" => MailboxModel::from_record(record)?.to_json(),
        "mail_uid_counters" => MailUidCounterModel::from_record(record)?.to_json(),
        "mail_mailbox_uid" => MailMailboxUidModel::from_record(record)?.to_json(),
        other => panic!("no model for {other}"),
    })
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

fn mapping_writes(writes: &Value) -> Vec<MappingWrite> {
    writes
        .as_array()
        .unwrap()
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
        .collect()
}

fn optional<T>(value: Option<T>, to_json: impl Fn(&T) -> Value) -> Value {
    value.as_ref().map(to_json).unwrap_or(Value::Null)
}

/// Runs the Rust counterpart of one TypeScript repository call.
async fn call(name: &str, args: &Value) -> Outcome {
    let user_id = || text(args, "user_id");
    let mail_id = || text(args, "mail_id");
    Ok(match name {
        "table.query" => {
            let table = text(args, "table");
            let rows = table_named(table).query(&select_filters(&args["filters"])).await?;
            let models: Result<Vec<Value>, Thrown> = rows.iter().map(|row| model_json(table, row)).collect();
            Value::Array(models?)
        }
        "table.count" => json!(table_named(text(args, "table")).count(&select_filters(&args["filters"])).await?),
        "table.queryOne" => {
            let table = text(args, "table");
            let filters = select_filters(&args["filters"]);
            match args.get("columns").filter(|columns| columns.is_array()) {
                Some(columns) => {
                    let row = table_named(table).query_one(&filters, Some(&columns_of(columns))).await?;
                    row.map(Value::Object).unwrap_or(Value::Null)
                }
                None => match table_named(table).query_one(&filters, None).await? {
                    Some(row) => model_json(table, &row)?,
                    None => Value::Null,
                },
            }
        }
        "table.insert" => {
            let row = table_named(text(args, "table"))
                .insert(&insert_values(&args["data"]), &columns_of(&args["returning"]))
                .await?;
            row.map(Value::Object).unwrap_or(Value::Null)
        }
        "table.update" => {
            let row = table_named(text(args, "table"))
                .update(Param::from(text(args, "id")), &insert_values(&args["data"]), &columns_of(&args["returning"]))
                .await?;
            row.map(Value::Object).unwrap_or(Value::Null)
        }
        "table.hardDelete" => json!(table_named(text(args, "table")).hard_delete(Param::from(text(args, "id"))).await?),
        "table.deleteWhere" => {
            json!(table_named(text(args, "table")).delete_where(&mutation_filters(&args["filters"])).await?)
        }
        "table.updateWhere" => {
            let rows = table_named(text(args, "table"))
                .update_where(
                    &mutation_filters(&args["filters"]),
                    &set_values(&args["data"]),
                    &columns_of(&args["returning"]),
                )
                .await?;
            Value::Array(rows.into_iter().map(Value::Object).collect())
        }

        "getDomainUidNext" => json!(get_domain_uid_next(user_id(), flag(args, "sent")).await?),
        "getAccountUidNext" => {
            json!(get_account_uid_next(user_id(), text(args, "account"), flag(args, "sent")).await?)
        }
        "getMailboxUidNext" => json!(get_mailbox_uid_next(user_id(), text(args, "mailbox")).await?),
        "getUidNext" => json!(get_uid_next(user_id(), &uid_scope(&args["scope"])).await?),
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
        "getNextModseq" => json!(get_next_modseq(user_id()).await?),
        "getHighestModseq" => json!(get_highest_modseq(user_id(), opt_text(args, "mailbox"), flag(args, "sent")).await),

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
        "recordMappings" => {
            let persisted = record_mappings(
                user_id(),
                mail_id(),
                &mapping_writes(&args["writes"]),
                opt_text(args, "mappedDestination"),
            )
            .await?;
            json!(persisted)
        }
        "saveMail" => optional(save_mail(&save_mail_input(args)).await?, |saved| {
            let mut result = Map::new();
            result.insert("_id".to_string(), json!(saved.id));
            if let Some(uid) = saved.uid_mailbox {
                result.insert("uid_mailbox".to_string(), json!(uid));
            }
            if let Some(uid) = saved.uid_domain {
                result.insert("uid_domain".to_string(), json!(uid));
            }
            Value::Object(result)
        }),
        "getMailByMessageId" => {
            optional(get_mail_by_message_id(user_id(), text(args, "message_id")).await?, MailModel::to_json)
        }
        "getMailById" => optional(get_mail_by_id(user_id(), mail_id()).await?, MailModel::to_json),
        "updateRfc822Size" => {
            update_rfc822_size(user_id(), mail_id(), integer(args, "rfc822_size")).await?;
            Value::Null
        }
        "markMailRead" => json!(mark_mail_read(user_id(), mail_id()).await?),
        "markMailSaved" => json!(mark_mail_saved(user_id(), mail_id(), flag(args, "saved")).await?),
        "deleteMail" => json!(delete_mail(user_id(), mail_id()).await?),
        "markMailSpam" => {
            let result = mark_mail_spam(user_id(), mail_id(), flag(args, "is_spam")).await?;
            json!({ "found": result.found, "changed": result.changed })
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
            match create_mailbox(&input).await? {
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
        "renameMailbox" => {
            match rename_mailbox(user_id(), text(args, "oldName"), text(args, "newName")).await? {
                RenameMailboxResult::Renamed => json!("renamed"),
                RenameMailboxResult::NotFound => json!("not_found"),
                RenameMailboxResult::Protected => json!("protected"),
                RenameMailboxResult::NameTaken => json!("name_taken"),
            }
        }
        "setMailboxSubscribed" => {
            json!(set_mailbox_subscribed(user_id(), text(args, "name"), flag(args, "subscribed")).await?)
        }
        other => panic!("no Rust call for {other}"),
    })
}

#[tokio::test]
async fn repository_functions_issue_the_same_statements_and_return_the_same_results() {
    let fixture: Value = serde_json::from_str(TRACES).expect("traces.json is JSON");
    let _attachments = AttachmentsOnDisk::new(&fixture);
    let normalizer = Normalizer {
        fixed_ids: names(&fixture["fixedIds"]).into_iter().map(str::to_string).collect(),
        fixed_date: text(&fixture, "fixedDate").to_string(),
    };
    let cases = list(&fixture, "cases");
    assert!(cases.len() >= 180, "the fixture holds every case");

    let mut failures: Vec<String> = Vec::new();
    for case in cases {
        let name = text(case, "name");
        let (outcome, trace) = with_script(scripted(&case["responses"]), call(text(case, "fn"), &case["args"])).await;

        let expected_trace = list(case, "trace");
        if trace.len() != expected_trace.len() {
            failures.push(format!(
                "{name}: issued {} statements, expected {}:\n{:#?}",
                trace.len(),
                expected_trace.len(),
                trace.iter().map(|query| &query.sql).collect::<Vec<_>>()
            ));
            continue;
        }
        for (at, (actual, expected)) in trace.iter().zip(expected_trace).enumerate() {
            if actual.sql != text(expected, "sql") {
                failures.push(format!(
                    "{name}: statement {} sql\n  actual:   {:?}\n  expected: {:?}",
                    at + 1,
                    actual.sql,
                    text(expected, "sql")
                ));
            }
            let values: Vec<Param> = actual.values.iter().map(|value| normalizer.param(value)).collect();
            if values != params_of(&expected["values"]) {
                failures.push(format!(
                    "{name}: statement {} values\n  actual:   {:?}\n  expected: {:?}",
                    at + 1,
                    values,
                    params_of(&expected["values"])
                ));
            }
        }

        let expected = &case["result"];
        match (&outcome, expected.get("ok"), expected.get("threw")) {
            (Ok(actual), Some(ok), _) => {
                let actual = normalizer.json(actual);
                if actual != *ok {
                    failures.push(format!("{name}: returned\n  actual:   {actual}\n  expected: {ok}"));
                }
            }
            (Err(thrown), _, Some(threw)) => {
                if thrown.code.as_deref() != opt_text(threw, "code") {
                    failures.push(format!("{name}: failed with code {:?}, expected {}", thrown.code, threw["code"]));
                }
                // A JavaScript runtime error has no wording to carry over.
                if !flag(threw, "runtime") && thrown.message != text(threw, "message") {
                    failures.push(format!(
                        "{name}: failed with\n  actual:   {:?}\n  expected: {:?}",
                        thrown.message,
                        text(threw, "message")
                    ));
                }
            }
            (Ok(actual), _, _) => failures.push(format!("{name}: returned {actual}, expected {expected}")),
            (Err(thrown), _, _) => failures.push(format!("{name}: failed with {:?}, expected {expected}", thrown.message)),
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
