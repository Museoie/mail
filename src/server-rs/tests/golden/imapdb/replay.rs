//! Golden tests: the IMAP data layer against what the Bun server's
//! `repositories/mails/imap-query.ts`, `repositories/mails/imap.ts` and
//! `imap/store.ts` build, issue and return.
//!
//! The fixtures beside this file are generated from the real TypeScript by
//! `generate.ts`. `builders.json` holds the output of the pure functions;
//! `traces.json` holds, for each repository function and store method, the
//! statements it issues in order, the values it binds and what it returns
//! when the pool answers from a script.
//!
//! This file is a test module of `src/imap/store.rs` (it reads the store's
//! private account-list builder), included with `#[path]`.

use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use super::*;
use crate::imap::parsers::command_parser::parse_command;
use crate::imap::types::{js_number_string, ImapRequest, SearchCriterion, SequenceRange};
use crate::imap::util::JsDate;
use crate::models::SignedUser;
use crate::postgres::repositories::mails::imap::testing::with_inline_reads;
use crate::postgres::repositories::mails::imap::{
    self as repository, build_flag_set_clause, pg_byte_chunks, pg_text_chunks, TextColumn, PG_TEXT_CHUNK_BYTES,
    PG_TEXT_CHUNK_CHARS,
};
use crate::postgres::repositories::mails::imap_query::{self as query, DateKey, SizeKey, TextKey};
use crate::postgres::repositories::mails::table::script::with_script;
use crate::postgres::repositories::mails::table::{Filter, PreparedQuery, Record, Rows};
use crate::postgres::{DbError, DbResult, Param};
use crate::util::get_domain;

const BUILDERS: &str = include_str!("builders.json");
const TRACES: &str = include_str!("traces.json");

const USER: &str = "11111111-1111-4111-8111-111111111111";
const FIXED_IDS: [&str; 4] = [
    USER,
    "22222222-2222-4222-8222-222222222222",
    "33333333-3333-4333-8333-333333333333",
    "44444444-4444-4444-8444-444444444444",
];

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).expect("the fixture is JSON")
}

fn list<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    value[key].as_array().unwrap_or_else(|| panic!("{key} is a list"))
}

// ---------------------------------------------------------------------------
// Values as the fixtures spell them
// ---------------------------------------------------------------------------

/// A number: JSON, or `{ $n }` for one JSON cannot carry.
fn number(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().expect("a number"),
        Value::Object(object) => match object.get("$n").and_then(Value::as_str) {
            Some("Infinity") => f64::INFINITY,
            Some("-Infinity") => f64::NEG_INFINITY,
            Some("NaN") => f64::NAN,
            Some(text) => text.parse().unwrap_or_else(|_| panic!("{text} is a number")),
            None => panic!("{value} is not a number"),
        },
        other => panic!("{other} is not a number"),
    }
}

/// An optional argument: absent and `null` are JavaScript's `undefined`.
fn opt_number(args: &Value, key: &str) -> Option<f64> {
    args.get(key).filter(|value| !value.is_null()).map(number)
}

fn opt_text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn text<'a>(args: &'a Value, key: &str) -> &'a str {
    opt_text(args, key).unwrap_or_else(|| panic!("{key} is a string in {args}"))
}

fn flag(args: &Value, key: &str) -> bool {
    args[key].as_bool().unwrap_or_else(|| panic!("{key} is a boolean in {args}"))
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().expect("a list").iter().map(|item| item.as_str().expect("a string").to_string()).collect()
}

fn integers(value: &Value) -> Vec<i64> {
    value.as_array().expect("a list").iter().map(|item| item.as_i64().expect("an integer")).collect()
}

/// `{ $date: <ms or null> }`.
fn date(value: &Value) -> JsDate {
    match value["$date"].as_i64() {
        Some(ms) => JsDate::from_ms(ms),
        None => JsDate::invalid(),
    }
}

/// A number as `enc` writes it: JSON when finite, `{ $n }` otherwise.
fn enc_number(value: f64) -> Value {
    if value.is_finite() {
        serde_json::from_str(&js_number_string(value)).expect("a JavaScript number is JSON")
    } else {
        json!({ "$n": js_number_string(value) })
    }
}

fn enc_date(value: &JsDate) -> Value {
    json!({ "$date": value.timestamp_millis() })
}

/// A bound value as the pool sends it.
fn wire(param: &Param) -> Value {
    match param {
        Param::Null => Value::Null,
        Param::Text(text) => Value::String(text.clone()),
        Param::Bytes(bytes) => json!({ "$hex": hex::encode(bytes) }),
    }
}

fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

/// Generated ids differ per run; everything else is exact.
fn normalize(value: Value) -> Value {
    match value {
        Value::String(text) if is_uuid(&text) && !FIXED_IDS.contains(&text.as_str()) => json!("<uuid>"),
        other => other,
    }
}

/// The fixtures are generated without `EMAIL_DOMAIN`; a test run with one
/// binds its own domain where they bind the default.
fn localize(value: &Value) -> Value {
    match value.as_str() {
        Some("mydomain") => json!(get_domain()),
        Some("alice.mydomain") => json!(format!("alice.{}", get_domain())),
        _ => value.clone(),
    }
}

fn wire_values(values: &[Param]) -> Value {
    Value::Array(values.iter().map(|value| normalize(wire(value))).collect())
}

fn expected_values(values: &Value) -> Value {
    Value::Array(list_of(values).iter().map(localize).collect())
}

fn list_of(value: &Value) -> &Vec<Value> {
    value.as_array().unwrap_or_else(|| panic!("{value} is a list"))
}

fn assert_query(label: &str, actual: &PreparedQuery, expected: &Value) {
    assert_eq!(actual.sql, text(expected, "sql"), "{label}: sql");
    assert_eq!(wire_values(&actual.values), expected_values(&expected["values"]), "{label}: values");
}

fn assert_statements(label: &str, actual: &[PreparedQuery], expected: &Value) {
    let expected = list_of(expected);
    let issued: Vec<&str> = actual.iter().map(|statement| statement.sql.as_str()).collect();
    assert_eq!(actual.len(), expected.len(), "{label}: statement count; issued {issued:#?}");
    for (at, (statement, wanted)) in actual.iter().zip(expected).enumerate() {
        assert_query(&format!("{label}: statement {}", at + 1), statement, wanted);
    }
}

/// A scripted response from its fixture form. A BYTEA value comes back from
/// the pool as hexadecimal.
fn response(value: &Value) -> DbResult<Rows> {
    fn decode(value: &Value) -> Value {
        match value {
            Value::Object(object) if object.contains_key("$hex") => object["$hex"].clone(),
            Value::Object(object) => {
                Value::Object(object.iter().map(|(key, entry)| (key.clone(), decode(entry))).collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(decode).collect()),
            other => other.clone(),
        }
    }
    if let Some(error) = value.get("error") {
        let code = text(error, "code");
        return Err(DbError { code: Some(code.to_string()), ..DbError::new(text(error, "message").to_string()) });
    }
    let rows: Vec<Record> =
        list(value, "rows").iter().map(|row| decode(row).as_object().expect("a row is an object").clone()).collect();
    Ok(Rows { row_count: Some(rows.len() as u64), rows })
}

fn script(case: &Value) -> Vec<DbResult<Rows>> {
    list(case, "script").iter().map(response).collect()
}

/// What a JavaScript caller sees of a failure: the SQLSTATE, or nothing.
fn threw(error: &DbError) -> Value {
    json!({ "threw": { "code": error.code } })
}

fn returned(value: Value) -> Value {
    json!({ "returned": value })
}

fn outcome_of(case: &Value) -> Value {
    match case.get("threw") {
        Some(thrown) => json!({ "threw": thrown }),
        None => json!({ "returned": case.get("returned").cloned().unwrap_or(Value::Null) }),
    }
}

fn mailbox_arg(args: &Value) -> Option<&str> {
    opt_text(args, "mailbox")
}

// ---------------------------------------------------------------------------
// Criteria
// ---------------------------------------------------------------------------

fn text_key(kind: &str) -> Option<TextKey> {
    Some(match kind {
        "SUBJECT" => TextKey::Subject,
        "FROM" => TextKey::From,
        "TO" => TextKey::To,
        "CC" => TextKey::Cc,
        "BCC" => TextKey::Bcc,
        "BODY" => TextKey::Body,
        "TEXT" => TextKey::Text,
        "SUBJECT_TEXT" => TextKey::SubjectText,
        _ => return None,
    })
}

fn date_key(kind: &str) -> Option<DateKey> {
    Some(match kind {
        "BEFORE" => DateKey::Before,
        "ON" => DateKey::On,
        "SINCE" => DateKey::Since,
        "SENTBEFORE" => DateKey::SentBefore,
        "SENTON" => DateKey::SentOn,
        "SENTSINCE" => DateKey::SentSince,
        _ => return None,
    })
}

fn ranges_of(value: &Value) -> Vec<SequenceRange> {
    list_of(value)
        .iter()
        .map(|range| SequenceRange { start: number(&range["start"]), end: opt_number(range, "end") })
        .collect()
}

/// A `{ type, value }` criterion. The builder upper-cases the type, so a
/// lower-case operator is still that operator.
fn criterion_of(value: &Value) -> SimplifiedCriterion {
    let kind = text(value, "type");
    let upper = kind.to_uppercase();
    let operand = value.get("value").filter(|operand| !operand.is_null());
    match (upper.as_str(), operand) {
        ("NOT", Some(inner)) => SimplifiedCriterion::not(criterion_of(inner)),
        ("OR", Some(sides)) => SimplifiedCriterion::or(criterion_of(&sides["left"]), criterion_of(&sides["right"])),
        ("HEADER", Some(header)) => SimplifiedCriterion::Header {
            field: text(header, "field").to_string(),
            text: text(header, "text").to_string(),
        },
        ("UID_SET", Some(ranges)) => SimplifiedCriterion::UidSet(ranges_of(ranges)),
        ("LARGER", Some(size)) => SimplifiedCriterion::Size { key: SizeKey::Larger, value: number(size) },
        ("SMALLER", Some(size)) => SimplifiedCriterion::Size { key: SizeKey::Smaller, value: number(size) },
        (kind_upper, Some(operand)) if text_key(kind_upper).is_some() => SimplifiedCriterion::Text {
            key: text_key(kind_upper).unwrap(),
            value: operand.as_str().expect("a text operand").to_string(),
        },
        (kind_upper, Some(operand)) if date_key(kind_upper).is_some() => {
            SimplifiedCriterion::Date { key: date_key(kind_upper).unwrap(), value: date(operand) }
        }
        _ => SimplifiedCriterion::Bare(kind.to_string()),
    }
}

/// A criterion as `enc` writes the Bun server's object.
fn enc_criterion(criterion: &SimplifiedCriterion) -> Value {
    match criterion {
        SimplifiedCriterion::Bare(kind) => json!({ "type": kind }),
        SimplifiedCriterion::Text { key, value } => json!({ "type": key.as_str(), "value": value }),
        SimplifiedCriterion::Header { field, text } => {
            json!({ "type": "HEADER", "value": { "field": field, "text": text } })
        }
        SimplifiedCriterion::Date { key, value } => json!({ "type": key.as_str(), "value": enc_date(value) }),
        SimplifiedCriterion::Size { key, value } => json!({ "type": key.as_str(), "value": enc_number(*value) }),
        SimplifiedCriterion::UidSet(ranges) => json!({
            "type": "UID_SET",
            "value": ranges
                .iter()
                .map(|range| {
                    let mut entry = Map::new();
                    entry.insert("start".to_string(), enc_number(range.start));
                    if let Some(end) = range.end {
                        entry.insert("end".to_string(), enc_number(end));
                    }
                    Value::Object(entry)
                })
                .collect::<Vec<_>>(),
        }),
        SimplifiedCriterion::Not(inner) => json!({ "type": "NOT", "value": enc_criterion(inner) }),
        SimplifiedCriterion::Or(left, right) => {
            json!({ "type": "OR", "value": { "left": enc_criterion(left), "right": enc_criterion(right) } })
        }
    }
}

/// The criteria of a `SEARCH` or `UID SEARCH` line, as the Rust parser reads it.
fn search_criteria(line: &str) -> Vec<SearchCriterion> {
    let parsed = parse_command(line, None).into_result().unwrap_or_else(|error| panic!("{line}: {error}"));
    let request = match parsed.request {
        ImapRequest::Uid(uid) => uid.into_parts().1,
        other => other,
    };
    match request {
        ImapRequest::Search(search) => search.criteria,
        other => panic!("{line} is not a SEARCH: {}", other.type_name()),
    }
}

// ---------------------------------------------------------------------------
// builders.json
// ---------------------------------------------------------------------------

#[test]
fn the_count_and_uid_list_builders_emit_the_same_sql_and_values() {
    let fixture = fixture(BUILDERS);
    for (group, build) in [
        ("countMessages", query::build_count_messages_query as fn(&str, Option<&str>, bool, Option<&[String]>) -> PreparedQuery),
        ("allUids", query::build_all_uids_query),
        ("firstUnseenUid", query::build_first_unseen_uid_query),
        ("expungeDeletedSelect", query::build_expunge_deleted_select_query),
        ("expungeDeletedInbox", query::build_expunge_deleted_inbox_query),
    ] {
        let cases = list(&fixture, group);
        assert_eq!(cases.len(), 20, "{group}");
        for case in cases {
            let args = &case["args"];
            assert_query(&format!("{group} {args}"), &build(USER, mailbox_arg(args), flag(args, "sent"), None), case);
        }
    }
}

/// A filter bag as the fixture writes it: `[column, String(value)]`, and
/// `{ op, value }` for an IN.
fn enc_filters(filters: &[(&str, Filter)]) -> Value {
    let param_text = |param: &Param| match param {
        Param::Text(text) => json!(text),
        other => panic!("{other:?} is not a text filter value"),
    };
    Value::Array(
        filters
            .iter()
            .map(|(column, filter)| {
                let value = match filter {
                    Filter::Equals(param) => param_text(param),
                    Filter::In { values: Some(values), not_null: false } => {
                        json!({ "op": "IN", "value": values.iter().map(param_text).collect::<Vec<_>>() })
                    }
                    other => panic!("{other:?} is not a filter the expunge builders produce"),
                };
                json!([column, value])
            })
            .collect(),
    )
}

#[test]
fn the_expunge_builders_emit_the_same_filters_and_statements() {
    let fixture = fixture(BUILDERS);
    for case in list(&fixture, "expungeDeletedFilters") {
        let args = &case["args"];
        let filters = query::build_expunge_deleted_filters(USER, mailbox_arg(args), flag(args, "sent"));
        assert_eq!(enc_filters(&filters), case["filters"], "expungeDeletedFilters {args}");
    }
    assert_eq!(list(&fixture, "expungeUidsFilters").len(), 80);
    for case in list(&fixture, "expungeUidsFilters") {
        let args = &case["args"];
        let filters =
            query::build_expunge_uids_filters(USER, mailbox_arg(args), flag(args, "sent"), &integers(&args["uids"]));
        assert_eq!(enc_filters(&filters), case["filters"], "expungeUidsFilters {args}");
    }
    for case in list(&fixture, "expungeUidsSelect") {
        let args = &case["args"];
        let built = query::build_expunge_uids_select_query(
            USER,
            mailbox_arg(args),
            flag(args, "sent"),
            &integers(&args["uids"]),
            None,
        );
        assert_query(&format!("expungeUidsSelect {args}"), &built, case);
    }
}

#[test]
fn the_flag_push_builder_emits_the_same_sql_and_values() {
    let fixture = fixture(BUILDERS);
    let cases = list(&fixture, "mailFlagUpdates");
    assert_eq!(cases.len(), 60);
    for case in cases {
        let args = &case["args"];
        let built = query::build_mail_flag_updates_query(
            USER,
            mailbox_arg(args),
            flag(args, "sent"),
            &strings(&args["mail_ids"]),
            None,
        );
        assert_query(&format!("mailFlagUpdates {args}"), &built, case);
    }
}

#[test]
fn the_range_builder_emits_the_same_sql_projection_and_values() {
    let fixture = fixture(BUILDERS);
    let cases = list(&fixture, "mailsByRange");
    assert_eq!(cases.len(), 1500);
    for case in cases {
        let args = &case["args"];
        let built = query::build_mails_by_range_query(
            USER,
            mailbox_arg(args),
            flag(args, "sent"),
            number(&args["start"]),
            number(&args["end"]),
            flag(args, "useUid"),
            &strings(&args["fields"]),
            opt_number(args, "changedSince"),
            None,
        );
        let label = format!("mailsByRange {args}");
        assert_eq!(built.sql, text(case, "sql"), "{label}: sql");
        assert_eq!(json!(built.selected_fields), case["selectedFields"], "{label}: selectedFields");
        assert_eq!(wire_values(&built.values), expected_values(&case["values"]), "{label}: values");
    }
}

#[test]
fn criterion_clauses_reduce_and_bind_as_the_bun_server_does() {
    let fixture = fixture(BUILDERS);
    let cases = list(&fixture, "criterionClause");
    assert_eq!(cases.len(), 1844);
    for case in cases {
        let criterion = criterion_of(&case["criterion"]);
        let uid_field = text(case, "uidField");
        let mut values: Vec<Param> = list(case, "seed")
            .iter()
            .map(|seed| match seed {
                Value::Bool(flag) => Param::from(*flag),
                Value::String(text) => Param::from(text.as_str()),
                other => panic!("no seed {other}"),
            })
            .collect();
        let label = format!("criterionClause {} on {uid_field}", case["criterion"]);
        let built = query::build_criterion_clause(&criterion, uid_field, &mut values);
        if case.get("threw").is_some() {
            // The Bun server builds the fragment and then fails to bind its
            // Invalid Date; the port reports that before binding.
            assert_eq!(built, Err(query::InvalidDateParam), "{label}");
            continue;
        }
        assert_eq!(built.map(|clause| json!(clause)), Ok(case["clause"].clone()), "{label}: clause");
        assert_eq!(wire_values(&values), expected_values(&case["values"]), "{label}: values");
    }
}

#[test]
fn search_statements_match_for_every_mailbox_and_criteria_list() {
    let fixture = fixture(BUILDERS);
    let cases = list(&fixture, "searchMailsByUid");
    assert_eq!(cases.len(), 130);
    for case in cases {
        let args = &case["args"];
        let criteria: Vec<SimplifiedCriterion> = list(args, "criteria").iter().map(criterion_of).collect();
        let built = query::build_search_mails_by_uid_query(USER, mailbox_arg(args), flag(args, "sent"), &criteria, None);
        let label = format!("searchMailsByUid {args}");
        if case.get("threw").is_some() {
            assert_eq!(built, Err(query::InvalidDateParam), "{label}");
            continue;
        }
        assert_query(&label, &built.unwrap_or_else(|_| panic!("{label}: no statement")), case);
    }
}

#[test]
fn flag_set_clauses_match_for_every_operation_and_flag_list() {
    let fixture = fixture(BUILDERS);
    let cases = list(&fixture, "flagSetClause");
    assert_eq!(cases.len(), 111);
    for case in cases {
        let operation = StoreOperationType::parse(text(case, "operation")).expect("a STORE operation");
        let clause = build_flag_set_clause(operation, &strings(&case["flags"]));
        assert_eq!(clause, text(case, "clause"), "flagSetClause {} {}", case["operation"], case["flags"]);
    }
}

#[test]
fn parsed_search_keys_simplify_as_the_bun_server_simplifies_them() {
    let fixture = fixture(BUILDERS);
    let cases = list(&fixture, "simplifyCriterion");
    assert_eq!(cases.len(), 156);
    for case in cases {
        let line = text(case, "line");
        let simplified: Vec<Value> =
            search_criteria(line).iter().map(|criterion| enc_criterion(&simplify_criterion(criterion))).collect();
        assert_eq!(Value::Array(simplified), case["simplified"], "{line}");
    }
}

#[test]
fn the_account_list_statement_matches_the_http_repositorys() {
    let fixture = fixture(BUILDERS);
    let cases = list(&fixture, "accountStats");
    assert_eq!(cases.len(), 8);
    for case in cases {
        let args = &case["args"];
        // `domain ? [domain] : undefined`
        let domains: Vec<String> =
            opt_text(args, "domain").filter(|domain| !domain.is_empty()).map(str::to_string).into_iter().collect();
        let filter = (!domains.is_empty()).then_some(domains.as_slice());
        let built = crate::postgres::repositories::mails::http_query::build_account_stats_query(
            USER,
            flag(args, "sent"),
            filter,
            false,
            None,
        );
        assert_query(&format!("accountStats {args}"), &built, case);
    }
}

// ---------------------------------------------------------------------------
// traces.json: the repository
// ---------------------------------------------------------------------------

fn flag_rows(rows: &[UpdatedMailFlags]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "mail_id": row.mail_id, "uid": row.uid, "read": row.read, "saved": row.saved,
                    "deleted": row.deleted, "draft": row.draft, "answered": row.answered, "modseq": row.modseq,
                })
            })
            .collect(),
    )
}

fn flag_updates(rows: &[MailFlagUpdate]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "mail_id": row.mail_id, "uid": row.uid, "read": row.read, "saved": row.saved,
                    "deleted": row.deleted, "draft": row.draft, "answered": row.answered,
                })
            })
            .collect(),
    )
}

fn counts(counts: &MessageCounts) -> Value {
    json!({ "total": counts.total, "unread": counts.unread })
}

fn source_column(args: &Value) -> TextColumn {
    match text(args, "source") {
        "text" => TextColumn::Text,
        "html" => TextColumn::Html,
        other => panic!("no column {other}"),
    }
}

/// A drained stream: its chunks, and the failure that ended it if one did.
fn drained<T>(chunks: Vec<DbResult<T>>, enc: impl Fn(T) -> Value) -> Value {
    let mut out = Vec::new();
    for chunk in chunks {
        match chunk {
            Ok(chunk) => out.push(enc(chunk)),
            Err(error) => return json!({ "chunks": out, "failed": error.code }),
        }
    }
    json!({ "chunks": out })
}

async fn run_repository(name: &str, args: &Value) -> Value {
    let mailbox = mailbox_arg(args);
    let sent = || flag(args, "sent");
    match name {
        "countMessages" => returned(counts(&repository::count_messages(USER, mailbox, sent(), None).await)),
        "getAllUids" => returned(json!(repository::get_all_uids(USER, mailbox, sent(), None).await)),
        "getFirstUnseenUid" => returned(json!(repository::get_first_unseen_uid(USER, mailbox, sent(), None).await)),
        "getMailFlagUpdates" => returned(flag_updates(
            &repository::get_mail_flag_updates(USER, mailbox, sent(), &strings(&args["mail_ids"]), None).await,
        )),
        "getMailsByRange" => {
            let mails = repository::get_mails_by_range(
                USER,
                mailbox,
                sent(),
                number(&args["start"]),
                number(&args["end"]),
                flag(args, "useUid"),
                &strings(&args["fields"]),
                opt_number(args, "changedSince"),
                None,
            )
            .await;
            returned(Value::Array(
                mails
                    .iter()
                    .map(|(mail_id, model)| {
                        let fields: Map<String, Value> = model
                            .selected_fields()
                            .iter()
                            .filter_map(|field| Some((field.to_string(), model.get(field)?.clone())))
                            .collect();
                        json!([mail_id, fields])
                    })
                    .collect(),
            ))
        }
        "setMailFlags" => {
            let operation = opt_text(args, "operation").map_or(StoreOperationType::Flags, |operation| {
                StoreOperationType::parse(operation).expect("a STORE operation")
            });
            let result = repository::set_mail_flags(
                USER,
                mailbox,
                sent(),
                number(&args["start"]),
                number(&args["end"]),
                &strings(&args["flags"]),
                args["useUid"].as_bool().unwrap_or(false),
                operation,
                opt_number(args, "unchangedSince"),
                None,
            )
            .await;
            returned(json!({ "updated": flag_rows(&result.updated), "failed": result.failed }))
        }
        "searchMailsByUid" => {
            let criteria: Vec<SimplifiedCriterion> = list(args, "criteria").iter().map(criterion_of).collect();
            returned(json!(repository::search_mails_by_uid(USER, mailbox, sent(), &criteria, None).await))
        }
        "expungeDeletedMails" => returned(json!(repository::expunge_deleted_mails(USER, mailbox, sent(), None).await)),
        "expungeMailsByUid" => {
            match repository::expunge_mails_by_uid(USER, mailbox, sent(), &integers(&args["uids"]), None).await {
                Ok(uids) => returned(json!(uids)),
                Err(error) => threw(&error),
            }
        }
        "pgTextChunks" => {
            let chunk_chars = opt_number(args, "chunkChars").map_or(PG_TEXT_CHUNK_CHARS, |chars| chars as usize);
            let chunks = pg_text_chunks(text(args, "mail_id"), text(args, "user_id"), source_column(args), chunk_chars);
            returned(drained(chunks.collect::<Vec<_>>().await, Value::String))
        }
        "pgByteChunks" => {
            let start = opt_number(args, "startByte").map_or(1, |start| start as usize);
            let chunk_bytes = opt_number(args, "chunkBytes").map_or(PG_TEXT_CHUNK_BYTES, |bytes| bytes as usize);
            let chunks =
                pg_byte_chunks(text(args, "mail_id"), text(args, "user_id"), source_column(args), start, chunk_bytes);
            returned(drained(chunks.collect::<Vec<_>>().await, |bytes| json!({ "$hex": hex::encode(bytes) })))
        }
        other => panic!("no Rust repository function for {other}"),
    }
}

async fn replay(group: &str, minimum: usize) {
    let fixture = fixture(TRACES);
    let cases = list(&fixture, group);
    assert!(cases.len() >= minimum, "{group}: {} cases", cases.len());
    for case in cases {
        let name = text(case, "fn");
        let args = &case["args"];
        let label = format!("{group} {name} {args}");
        let (outcome, statements) = with_script(
            script(case),
            with_inline_reads(async {
                match group {
                    "repository" => run_repository(name, args).await,
                    _ => run_store(name, args).await,
                }
            }),
        )
        .await;
        assert_statements(&label, &statements, &case["statements"]);
        assert_eq!(outcome, outcome_of(case), "{label}: outcome");
    }
}

#[tokio::test]
async fn every_repository_function_issues_and_returns_what_the_bun_server_does() {
    replay("repository", 583).await;
}

// ---------------------------------------------------------------------------
// traces.json: the store
// ---------------------------------------------------------------------------

fn store_for(args: &Value) -> Store {
    let (username, email) = match opt_text(args, "user").unwrap_or("alice") {
        "alice" => ("alice", "alice@alice.mydomain"),
        "admin" => ("admin", "admin@mydomain"),
        other => panic!("no user {other}"),
    };
    // A plain login on the user's own domain, as the session opens one.
    let own_domain = &email[email.find('@').unwrap() + 1..];
    Store::new(
        SignedUser::new(Some(USER.to_string()), Some(username.to_string()), Some(email.to_string())),
        StoreScope::whole_store(own_domain),
    )
}

/// The mail `new Mail(init)` builds, with the model's defaults for what the
/// init leaves out or leaves falsy.
fn mail_of(init: &Value) -> Mail {
    let defaults = Mail::default();
    let truthy = |key: &str| init.get(key).filter(|value| !value.is_null());
    let string = |key: &str, default: String| {
        init.get(key).and_then(Value::as_str).filter(|value| !value.is_empty()).map_or(default, str::to_string)
    };
    let on = |key: &str| init.get(key).and_then(Value::as_bool).unwrap_or(false);
    let address = |key: &str| {
        truthy(key).map(|address| Address {
            value: address["value"].clone(),
            text: address["text"].as_str().unwrap_or_default().to_string(),
        })
    };
    let uid = |key: &str| init.get("uid").and_then(|uid| uid.get(key)).and_then(Value::as_i64).unwrap_or(0);
    Mail {
        attachments: truthy("attachments").cloned().or(defaults.attachments),
        from: address("from"),
        to: address("to"),
        cc: address("cc"),
        bcc: address("bcc"),
        reply_to: address("replyTo"),
        envelope_from: truthy("envelopeFrom").cloned(),
        envelope_to: truthy("envelopeTo").cloned().unwrap_or(defaults.envelope_to),
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
        insight: truthy("insight").cloned(),
        uid: MailUid { domain: uid("domain"), account: uid("account") },
        in_reply_to: opt_text(init, "inReplyTo").map(str::to_string),
        references: init.get("references").map(strings),
    }
}

async fn run_store(name: &str, args: &Value) -> Value {
    let store = store_for(args);
    let mailbox = || text(args, "box");
    let nullable = |value: Option<Value>| returned(value.unwrap_or(Value::Null));
    match name {
        "listMailboxes" => returned(json!(store.list_mailboxes().await)),
        "listMailboxEntries" => returned(Value::Array(
            store
                .list_mailbox_entries()
                .await
                .iter()
                .map(|entry| json!({ "name": entry.name, "subscribed": entry.subscribed }))
                .collect(),
        )),
        "mailboxExists" => match store.mailbox_exists(mailbox()).await {
            Ok(exists) => returned(json!(exists)),
            // A row that fails validation is a JavaScript error, with no code.
            Err(error) => match error.downcast_ref::<DbError>() {
                Some(error) => threw(error),
                None => json!({ "threw": { "code": null } }),
            },
        },
        "countMessages" => nullable(store.count_messages(mailbox()).await.as_ref().map(counts)),
        "getUidNext" => match store.get_uid_next(mailbox()).await {
            Ok(uid_next) => returned(json!(uid_next)),
            Err(error) => threw(&error),
        },
        "getAllUids" => nullable(store.get_all_uids(mailbox()).await.map(|uids| json!(uids))),
        "getFirstUnseenUid" => returned(json!(store.get_first_unseen_uid(mailbox()).await)),
        "getHighestModseq" => returned(json!(store.get_highest_modseq(mailbox()).await)),
        "getMailFlagUpdates" => nullable(
            store.get_mail_flag_updates(mailbox(), &strings(&args["mailIds"])).await.as_deref().map(flag_updates),
        ),
        "expungeUids" => match store.expunge_uids(mailbox(), &integers(&args["uids"])).await {
            Ok(uids) => returned(json!(uids)),
            Err(error) => threw(&error),
        },
        "expunge" => match store.expunge(mailbox()).await {
            Ok(uids) => returned(json!(uids)),
            Err(error) => threw(&error),
        },
        "getMessages" => {
            let mails = store
                .get_messages(
                    mailbox(),
                    number(&args["start"]),
                    number(&args["end"]),
                    &strings(&args["fields"]),
                    args["useUid"].as_bool().unwrap_or(false),
                    opt_number(args, "changedSince"),
                )
                .await;
            returned(Value::Array(mails.iter().map(|(mail_id, mail)| json!([mail_id, mail.to_json()])).collect()))
        }
        "setFlags" => {
            let operation = opt_text(args, "operation").map_or(StoreOperationType::Flags, |operation| {
                StoreOperationType::parse(operation).expect("a STORE operation")
            });
            let result = store
                .set_flags(
                    mailbox(),
                    number(&args["start"]),
                    number(&args["end"]),
                    &strings(&args["flags"]),
                    args["useUid"].as_bool().unwrap_or(false),
                    operation,
                    opt_number(args, "unchangedSince"),
                )
                .await;
            returned(json!({ "updated": flag_rows(&result.updated), "failed": result.failed }))
        }
        "search" => returned(json!(store.search(mailbox(), &search_criteria(text(args, "line"))).await)),
        "storeMail" => {
            let mut mail = mail_of(&args["mail"]);
            let ok = store.store_mail(&mut mail, opt_text(args, "destination")).await;
            returned(json!({ "ok": ok, "uid": { "domain": mail.uid.domain, "account": mail.uid.account } }))
        }
        other => panic!("no Rust store method for {other}"),
    }
}

#[tokio::test]
async fn every_store_method_issues_and_returns_what_the_bun_server_does() {
    replay("store", 1522).await;
}
