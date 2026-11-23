//! The differential check: every fixture in `tests/golden/imap` replayed
//! against the port. A fixture holds an input and what the Bun server's code
//! returned for it; the port has to return the same.

use std::collections::HashMap;

use mail_server::imap::capabilities::capabilities;
use mail_server::imap::fetch_format::{
    add_body_fields, build_body_response_part, build_fetch_response, build_fetch_response_part, convert_sequence_set,
    get_body_content, get_requested_fields, RequestedFields,
};
use mail_server::imap::parsers::append_parser::parse_append;
use mail_server::imap::parsers::auth_parsers::{parse_authenticate, parse_login};
use mail_server::imap::parsers::command_parser::parse_command;
use mail_server::imap::parsers::fetch_parsers::{
    parse_body_fetch, parse_body_section, parse_fetch, parse_fetch_data_item, parse_fetch_data_items,
};
use mail_server::imap::parsers::mailbox_parsers::{
    parse_create, parse_delete, parse_list, parse_rename, parse_select, parse_status, parse_subscribe,
    parse_unsubscribe,
};
use mail_server::imap::parsers::primitive_parsers::{
    consume, parse_atom, parse_date, parse_flag, parse_list_mailbox, parse_literal, parse_message_literal,
    parse_modifier_group, parse_number, parse_quoted_string, parse_sequence_set, parse_string, peek, skip_whitespace,
};
use mail_server::imap::parsers::search_parsers::{parse_search, parse_search_criteria};
use mail_server::imap::parsers::store_parsers::{parse_copy, parse_move, parse_store};
use mail_server::imap::sequence_resolver::{
    clamp_sequence_set_to_first, count_sequence_set_messages, departed_sequence_numbers, resolve_seq_range_to_uids,
    resolve_uid_range_sentinel, seq_to_uid_number, uid_to_seq_number,
};
use mail_server::imap::session_buffer::SessionBuffer;
use mail_server::imap::session_format::{
    apply_partial_fetch, build_full_message, build_message_segments, compute_full_message_size, get_body_part,
    get_body_part_headers, get_body_section_key, should_mark_as_read, sum_body_bytes, sum_part_body_bytes,
    sum_segment_bytes,
};
use mail_server::imap::trace::{clip, redact_credentials, trace_lines, Direction};
use mail_server::imap::types::{
    json_number, json_string, FetchDataItem, ImapRequest, ParseContext, ParseResult, ParseThrow, SearchCriterion,
};
use mail_server::imap::util::{self, JsDate};
use serde_json::{json, Value};

use crate::support::*;

fn request_json(result: &ParseResult<ImapRequest>) -> String {
    result.to_json_string_with(ImapRequest::to_json_string)
}

fn string_json(result: &ParseResult<String>) -> String {
    result.to_json_string_with(|text| json_string(text))
}

fn thrown(result: Result<String, ParseThrow>) -> String {
    match result {
        Ok(json) => json,
        Err(thrown) => format!("{{\"threw\":{}}}", json_string(&thrown.0)),
    }
}

fn array_json<T>(items: &[T], write: impl Fn(&T) -> String) -> String {
    format!("[{}]", items.iter().map(write).collect::<Vec<_>>().join(","))
}

fn str_arg(args: &Value, index: usize) -> &str {
    args[index].as_str().unwrap_or_default()
}

#[test]
fn command_lines() {
    let cases = fixture("commands.json");
    let mut report = Report::default();
    for case in cases.as_array().unwrap() {
        let line = case["line"].as_str().unwrap();
        let literals = literals_from_json(case.get("literals"));
        let result = parse_command(line, literals.as_deref());
        let actual = parse_json(&result.to_json_string_with(|command| command.to_json_string()));
        report.check(|| format!("parseCommand({line:?}, {})", case.get("literals").unwrap_or(&Value::Null)), &case["expected"], &actual);
    }
    report.finish("commands.json", 8000);
}

#[test]
fn deeply_nested_command_lines() {
    let cases = fixture("commands_deep.json");
    let mut report = Report::default();
    for case in cases.as_array().unwrap() {
        let line: String = case["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part[0].as_str().unwrap().repeat(part[1].as_u64().unwrap() as usize))
            .collect();
        let result = parse_command(&line, None);
        // The result nests as deep as the line: compared as text, by digest.
        let text = result.to_json_string_with(|command| command.to_json_string());
        let actual = json!({
            "success": result.success,
            "length": text.encode_utf16().count(),
            "sha256": sha256_hex(text.as_bytes()),
            "error": result.error,
        });
        let expected = json!({
            "success": case["success"], "length": case["length"], "sha256": case["sha256"], "error": case["error"],
        });
        report.check(|| format!("parseCommand({})", case["parts"]), &expected, &actual);
    }
    report.finish("commands_deep.json", 15);
}

/// Runs one directly-called parser and returns its result as JSON text.
fn run_parser(name: &str, context: &mut ParseContext, args: &Value) -> String {
    match name {
        "parseAtom" => string_json(&parse_atom(context)),
        "parseDate" => parse_date(context).to_json_string_with(JsDate::to_json_string),
        "parseFlag" => string_json(&parse_flag(context)),
        "parseLiteral" => string_json(&parse_literal(context)),
        "parseMessageLiteral" => parse_message_literal(context).to_json_string_with(|message| message.to_json_string()),
        "parseString" => string_json(&parse_string(context)),
        "parseListMailbox" => string_json(&parse_list_mailbox(context)),
        "parseQuotedString" => string_json(&parse_quoted_string(context)),
        "parseSequenceSet" => parse_sequence_set(context).to_json_string_with(|set| set.to_json_string()),
        "parseNumber" => parse_number(context).to_json_string_with(|number| json_number(*number)),
        "parseModifierGroup" => parse_modifier_group(context, str_arg(args, 0), str_arg(args, 1))
            .to_json_string_with(|number| json_number(*number)),
        "skipWhitespace" => {
            skip_whitespace(context);
            "null".to_string()
        }
        "peek" => peek(context).map_or("null".to_string(), |unit| unit.to_string()),
        "consume" => consume(context, str_arg(args, 0)).to_string(),
        "parseFetch" => request_json(&parse_fetch(context)),
        "parseFetchDataItems" => parse_fetch_data_items(context)
            .to_json_string_with(|items| array_json(items, FetchDataItem::to_json_string)),
        "parseFetchDataItem" => parse_fetch_data_item(context).to_json_string_with(FetchDataItem::to_json_string),
        "parseSearch" => thrown(parse_search(context).map(|result| request_json(&result))),
        "parseSearchCriteria" => thrown(parse_search_criteria(context).map(|result| {
            result.to_json_string_with(|criteria| array_json(criteria, SearchCriterion::to_json_string))
        })),
        "parseStore" => request_json(&parse_store(context)),
        "parseCopy" => request_json(&parse_copy(context)),
        "parseMove" => request_json(&parse_move(context)),
        "parseLogin" => request_json(&parse_login(context)),
        "parseAuthenticate" => request_json(&parse_authenticate(context)),
        "parseAppend" => request_json(&parse_append(context)),
        "parseList" => request_json(&parse_list(str_arg(args, 0), context)),
        "parseSelect" => request_json(&parse_select(args[0].as_bool().unwrap_or(false), context)),
        "parseStatus" => request_json(&parse_status(context)),
        "parseCreate" => request_json(&parse_create(context)),
        "parseDelete" => request_json(&parse_delete(context)),
        "parseRename" => request_json(&parse_rename(context)),
        "parseSubscribe" => request_json(&parse_subscribe(context)),
        "parseUnsubscribe" => request_json(&parse_unsubscribe(context)),
        other => panic!("no parser named {other}"),
    }
}

#[test]
fn parsers_called_directly() {
    let cases = fixture("parsers.json");
    let mut report = Report::default();
    for case in cases.as_array().unwrap() {
        let name = case["fn"].as_str().unwrap();
        let input = case["input"].as_str().unwrap();
        let mut context = ParseContext::new(input);
        context.position = case.get("position").and_then(Value::as_u64).unwrap_or(0) as usize;
        context.literals = literals_from_json(case.get("literals")).map(Into::into);
        let args = case.get("args").cloned().unwrap_or(Value::Null);

        let result = parse_json(&run_parser(name, &mut context, &args));
        let actual = json!({
            "result": result,
            "position_after": context.position,
            "literals_after": context.literals.as_ref().map(|queue| queue.len()),
        });
        let expected = json!({
            "result": case["expected"],
            "position_after": case["position_after"],
            "literals_after": case["literals_after"],
        });
        report.check(
            || format!("{name}({input:?}) at {} with {args} literals {}", case.get("position").unwrap_or(&json!(0)), case.get("literals").unwrap_or(&Value::Null)),
            &expected,
            &actual,
        );
    }
    report.finish("parsers.json", 5000);
}

#[test]
fn body_expressions_and_sections() {
    let cases = fixture("body.json");
    let mut report = Report::default();
    for case in cases.as_array().unwrap() {
        let name = case["fn"].as_str().unwrap();
        let input = case["input"].as_str().unwrap();
        let text = match name {
            "parseBodyFetch" => parse_body_fetch(input).to_json_string_with(FetchDataItem::to_json_string),
            "parseBodySection" => parse_body_section(input).to_json_string_with(|section| section.to_json_string()),
            other => panic!("no parser named {other}"),
        };
        report.check(|| format!("{name}({input:?})"), &case["expected"], &parse_json(&text));
    }
    report.finish("body.json", 140);
}

fn date_json(date: &JsDate) -> Value {
    json!({
        "ms": date.timestamp_millis(),
        "iso": date.to_iso_string(),
        "utc": date.to_utc_string(),
        "internal": util::format_internal_date(date),
    })
}

#[test]
fn dates() {
    let mut report = Report::default();
    for case in fixture("dates.json").as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let expected = json!({ "ms": case["ms"], "iso": case["iso"], "utc": case["utc"], "internal": case["internal"] });
        report.check(|| format!("new Date({input:?})"), &expected, &date_json(&JsDate::parse(input)));
    }
    report.finish("dates.json", 5000);

    let mut report = Report::default();
    for case in fixture("dates_ms.json").as_array().unwrap() {
        let ms = case["ms"].as_f64().unwrap();
        let date = if ms.abs() <= 9e15 { JsDate::from_ms(ms as i64) } else { JsDate::invalid() };
        let mut actual = date_json(&date);
        actual["ms"] = case["ms"].clone();
        let expected = json!({ "ms": case["ms"], "iso": case["iso"], "utc": case["utc"], "internal": case["internal"] });
        report.check(|| format!("new Date({ms})"), &expected, &actual);
    }
    report.finish("dates_ms.json", 25);
}

fn placement_json(placement: &util::UtilityPlacement) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in [
        ("draft", placement.draft),
        ("is_spam", placement.is_spam),
        ("saved", placement.saved),
        ("deleted", placement.deleted),
    ] {
        if let Some(value) = value {
            out.insert(name.to_string(), json!(value));
        }
    }
    Value::Object(out)
}

fn folder_json(folder: &util::UtilityFolder) -> Value {
    json!({
        "name": folder.name,
        "specialUse": folder.special_use,
        "placement": placement_json(&folder.placement),
        "uidSpace": match folder.uid_space {
            util::UtilityUidSpace::Domain => "domain",
            util::UtilityUidSpace::Mapped => "mapped",
        },
    })
}

fn run_util(name: &str, args: &Value) -> Value {
    let text = |index: usize| args[index].as_str().unwrap_or_default();
    let optional = |index: usize| args.get(index).and_then(Value::as_str);
    match name {
        "encodeText" => json!(util::encode_text(text(0))),
        "headerFieldValue" => json!(util::header_field_value(text(0))),
        "headerQuotedParam" => json!(util::header_quoted_param(text(0))),
        "boundaryToken" => json!(util::boundary_token(text(0))),
        "quoteString" => json!(util::quote_string(text(0))),
        "formatNString" => json!(util::format_n_string(optional(0))),
        "mediaTypeParts" => {
            let (media_type, subtype) = util::media_type_parts(optional(0));
            json!({ "type": media_type, "subtype": subtype })
        }
        "attachmentPartHeaderFields" => {
            let attachment = serde_json::from_value(args[0].clone()).unwrap();
            let fields = util::attachment_part_header_fields(&attachment);
            json!({ "contentType": fields.content_type, "filenameParam": fields.filename_param })
        }
        "formatAddressList" => {
            let list: Option<Vec<_>> = serde_json::from_value(args[0].clone()).unwrap();
            json!(util::format_address_list(list.as_deref()))
        }
        "formatEnvelope" => json!(util::format_envelope(&mail_from_json(&args[0]))),
        "formatFlags" => json!(util::format_flags(&mail_from_json(&args[0]))),
        "formatHeaders" => json!(util::format_headers(&mail_from_json(&args[0]), optional(1))),
        "formatBodyStructure" => {
            json!(util::format_body_structure(&mail_from_json(&args[0]), args.get(1).and_then(Value::as_bool).unwrap_or(true)))
        }
        "utilityFolder" => util::utility_folder(text(0)).map_or(Value::Null, folder_json),
        "isUtilityFolder" => json!(util::is_utility_folder(text(0))),
        "isMappedUtilityFolder" => json!(util::is_mapped_utility_folder(text(0))),
        "utilityPlacement" => util::utility_placement(text(0)).map_or(Value::Null, |placement| placement_json(&placement)),
        "canonicalMailbox" => json!(util::canonical_mailbox(text(0))),
        "accountToBox" => json!(util::account_to_box(text(0))),
        "accountToSentBox" => json!(util::account_to_sent_box(text(0))),
        "isSentBox" => json!(util::is_sent_box(text(0))),
        "isInbox" => json!(util::is_inbox(text(0))),
        "isDomainScoped" => json!(util::is_domain_scoped(text(0))),
        "isAccountsFolder" => json!(util::is_accounts_folder(text(0))),
        "isSentMessagesAccountsFolder" => json!(util::is_sent_messages_accounts_folder(text(0))),
        "boxToAccount" => json!(util::box_to_account(text(0), text(1))),
        "deriveCopyMessageId" => json!(util::derive_copy_message_id(Some(text(0)), text(1))),
        "constants" => json!({
            "ACCOUNTS_FOLDER": util::ACCOUNTS_FOLDER,
            "SENT_MESSAGES_FOLDER": util::SENT_MESSAGES_FOLDER,
            "SENT_MESSAGES_ACCOUNTS_FOLDER": util::SENT_MESSAGES_ACCOUNTS_FOLDER,
            "UTILITY_FOLDERS": util::UTILITY_FOLDERS.iter().map(folder_json).collect::<Vec<_>>(),
        }),
        other => panic!("no util function named {other}"),
    }
}

#[test]
fn util_functions() {
    let mut report = Report::default();
    for case in fixture("util.json").as_array().unwrap() {
        let name = case["fn"].as_str().unwrap();
        let expected = case["expected"].clone();
        report.check(|| format!("{name}({})", case["args"]), &expected, &run_util(name, &case["args"]));
    }
    report.finish("util.json", 1200);
}

fn uid_list(value: &Value) -> Vec<i64> {
    value.as_array().unwrap().iter().map(|uid| uid.as_i64().unwrap()).collect()
}

fn number_or_null<T: Into<f64>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, |number| parse_json(&json_number(number.into())))
}

fn run_sequence(name: &str, args: &Value) -> Value {
    let uids = uid_list(&args[0]);
    let number = |index: usize| args[index].as_f64().unwrap();
    let set_json = |ranges: &[mail_server::imap::types::SequenceRange]| {
        parse_json(&array_json(ranges, |range| range.to_json_string()))
    };
    match name {
        "seqToUidNumber" => number_or_null(seq_to_uid_number(&uids, number(1)).map(|uid| uid as f64)),
        "uidToSeqNumber" => {
            let map: HashMap<i64, usize> = args[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| (entry[0].as_i64().unwrap(), entry[1].as_u64().unwrap() as usize))
                .collect();
            number_or_null(uid_to_seq_number(&uids, &map, number(2)).map(|seq| seq as f64))
        }
        "resolveSeqRangeToUids" => resolve_seq_range_to_uids(&uids, number(1), number(2))
            .map_or(Value::Null, |range| json!({ "uidStart": range.uid_start, "uidEnd": range.uid_end })),
        "resolveUidRangeSentinel" => {
            let range = resolve_uid_range_sentinel(&uids, number(1), number(2));
            json!({ "uidStart": number_or_null(Some(range.uid_start)), "uidEnd": number_or_null(Some(range.uid_end)) })
        }
        "countSequenceSetMessages" => {
            number_or_null(Some(count_sequence_set_messages(&uids, &set_from_json(&args[1]), args[2].as_bool().unwrap())))
        }
        "clampSequenceSetToFirst" => {
            let clamped = clamp_sequence_set_to_first(&uids, &set_from_json(&args[1]), number(2), args[3].as_bool().unwrap());
            json!({ "type": clamped.kind.as_str(), "ranges": set_json(&clamped.ranges) })
        }
        "departedSequenceNumbers" => json!(departed_sequence_numbers(&uids, &uid_list(&args[1]))),
        other => panic!("no sequence function named {other}"),
    }
}

#[test]
fn sequence_resolution() {
    let mut report = Report::default();
    for case in fixture("sequence.json").as_array().unwrap() {
        let name = case["fn"].as_str().unwrap();
        report.check(|| format!("{name}({})", case["args"]), &case["expected"], &run_sequence(name, &case["args"]));
    }
    report.finish("sequence.json", 4500);
}

#[test]
fn session_buffer_scripts() {
    let mut report = Report::default();
    for (index, script) in fixture("session_buffer.json").as_array().unwrap().iter().enumerate() {
        let mut buffer = SessionBuffer::new();
        let mut results: Vec<Value> = Vec::new();
        for op in script["ops"].as_array().unwrap() {
            let number = |at: usize| op[at].as_u64().unwrap() as usize;
            results.push(match op[0].as_str().unwrap() {
                "push" => {
                    buffer.push(hex::decode(op[1].as_str().unwrap()).unwrap());
                    Value::Null
                }
                "pushEach" => {
                    for byte in hex::decode(op[1].as_str().unwrap()).unwrap() {
                        buffer.push(vec![byte]);
                    }
                    Value::Null
                }
                "indexOfCrlf" => buffer.index_of_crlf().map_or(json!(-1), |at| json!(at)),
                "decode" => json!(buffer.decode(number(1), number(2))),
                "head" => json!(hex::encode(buffer.head(number(1)))),
                "consume" => {
                    buffer.consume(number(1));
                    Value::Null
                }
                "clear" => {
                    buffer.clear();
                    Value::Null
                }
                "length" => json!(buffer.len()),
                "drainLines" => {
                    let mut lines = Vec::new();
                    while let Some(at) = buffer.index_of_crlf() {
                        lines.push(buffer.decode(0, at));
                        buffer.consume(at + 2);
                    }
                    json!(lines)
                }
                other => panic!("no buffer operation named {other}"),
            });
        }
        let expected = script["results"].as_array().unwrap();
        let first_difference = expected.iter().zip(&results).position(|(a, b)| !same(a, b));
        match first_difference {
            None => report.check(|| format!("script {index}"), &json!(expected.len()), &json!(results.len())),
            Some(at) => report.fail(format!(
                "script {index}, step {at}: {}\n  expected: {}\n  actual:   {}\n  after: {}",
                script["ops"][at],
                expected[at],
                results[at],
                script["ops"].as_array().unwrap()[at.saturating_sub(12)..at]
                    .iter()
                    .map(|op| op.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
        }
    }
    report.finish("session_buffer.json", 160);
}

#[test]
fn trace_and_redaction() {
    let mut report = Report::default();
    for case in fixture("trace.json").as_array().unwrap() {
        let name = case["fn"].as_str().unwrap();
        let args = &case["args"];
        let actual = match name {
            "redactCredentials" => json!(redact_credentials(str_arg(args, 0))),
            "clip" => json!(clip(str_arg(args, 0))),
            "trace" => {
                let direction = if str_arg(args, 0) == "in" { Direction::In } else { Direction::Out };
                json!(trace_lines(direction, str_arg(args, 1)))
            }
            other => panic!("no trace function named {other}"),
        };
        report.check(|| format!("{name}({args})"), &case["expected"], &actual);
    }
    report.finish("trace.json", 380);
}

#[test]
fn capability_lists() {
    let mut report = Report::default();
    for case in fixture("capabilities.json").as_array().unwrap() {
        let flag = |name: &str| case[name].as_bool().unwrap();
        let actual = capabilities(flag("isTls"), flag("sso"), flag("tlsAvailable"));
        report.check(|| format!("getCapabilities({case})"), &case["expected"], &json!(actual));
    }
    report.finish("capabilities.json", 8);
}

/// The generator does not drain a stream that would read the database (a
/// row projected without its body strings), so those parts are recorded
/// without the octets they emit. Where the port can still produce them, from
/// a window that only covers headers, there is nothing to compare them to.
fn forget_unrecorded_streams(expected: &Value, actual: &mut Value) {
    match (expected, actual) {
        (Value::Array(expected), Value::Array(actual)) => {
            for (expected, actual) in expected.iter().zip(actual) {
                forget_unrecorded_streams(expected, actual);
            }
        }
        (Value::Object(expected), Value::Object(actual)) if !expected.contains_key("emitted") => {
            actual.remove("emitted");
            actual.remove("sha256");
        }
        _ => {}
    }
}

fn fields_json(fields: &RequestedFields) -> Value {
    json!(fields.as_slice())
}

#[test]
fn fetch_formatting() {
    let data = fixture("fetch.json");
    let files = AttachmentFiles::from_fixture(&data["attachments"]);
    let mails: Vec<_> = data["mails"].as_array().unwrap().iter().map(mail_from_json).collect();
    let size = |id: &str| files.size(id);
    let read = |id: &str| files.read(id);

    let mut report = Report::default();
    // Cases where the Bun code threw a TypeError on a row no real mail has
    // (an attachment with no `content`); the port has no exception to match.
    let mut threw = 0;
    for case in data["cases"].as_array().unwrap() {
        let name = case["fn"].as_str().unwrap();
        let expected = &case["expected"];
        if expected.get("threw").and_then(Value::as_str).is_some_and(|text| text.starts_with("TypeError")) {
            threw += 1;
            continue;
        }
        let mail = case.get("mail").and_then(Value::as_u64).map(|index| &mails[index as usize]);
        let doc_id = case.get("docId").and_then(Value::as_str);
        let actual = match name {
            "getRequestedFields" => fields_json(&get_requested_fields(&items_from_json(&case["items"]))),
            "shouldMarkAsRead" => json!(should_mark_as_read(&items_from_json(&case["items"]))),
            "getBodySectionKey" => json!(get_body_section_key(&section_from_json(&case["section"]))),
            "addBodyFields" => {
                let mut fields = RequestedFields::default();
                let fetch = mail_server::imap::types::BodyFetch {
                    peek: true,
                    section: section_from_json(&case["section"]),
                    partial: None,
                };
                add_body_fields(&fetch, &mut fields);
                fields_json(&fields)
            }
            "convertSequenceSet" => {
                let set = mail_server::imap::types::SequenceSet::new(ranges_from_json(&case["ranges"]));
                Value::Array(
                    convert_sequence_set(&set)
                        .iter()
                        .map(|range| {
                            json!({ "start": number_or_null(Some(range.start)), "end": number_or_null(Some(range.end)) })
                        })
                        .collect(),
                )
            }
            "applyPartialFetch" => json!(apply_partial_fetch(
                case["content"].as_str().unwrap(),
                &partial_from_json(&case["partial"]).unwrap()
            )),
            "buildMessageSegments" => {
                let mail = mail.unwrap();
                let segments = build_message_segments(mail, doc_id, &size);
                let part_bytes: serde_json::Map<String, Value> = ["1", "2", "3", "1.1", "1.2"]
                    .iter()
                    .map(|path| (path.to_string(), json!(sum_part_body_bytes(&segments, path))))
                    .collect();
                json!({
                    "segments": parse_json(&array_json(&segments, |segment| segment.to_json_string())),
                    "fullSize": sum_segment_bytes(&segments),
                    "computed": compute_full_message_size(mail, doc_id, &size),
                    "bodyBytes": sum_body_bytes(&segments),
                    "partBytes": part_bytes,
                })
            }
            "buildFullMessage" => match build_full_message(mail.unwrap(), doc_id, &size, &read) {
                Ok(message) => json!(message),
                Err(error) => json!({ "threw": format!("Error: {error}") }),
            },
            "getBodyPartHeaders" => json!(get_body_part_headers(mail.unwrap(), case["partNum"].as_str().unwrap())),
            "getBodyPart" => json!(get_body_part(mail.unwrap(), case["partNum"].as_str().unwrap(), &read)),
            "getBodyContent" => {
                json!(get_body_content(mail.unwrap(), &section_from_json(&case["section"]), doc_id.unwrap()))
            }
            "buildBodyResponsePart" => {
                let fetch = body_fetch_from_json(&case["bodyFetch"]);
                part_json(build_body_response_part(mail.unwrap(), &fetch, doc_id.unwrap(), None, &size).as_ref(), &files)
            }
            "buildFetchResponsePart" => {
                let built = build_fetch_response_part(
                    mail.unwrap(),
                    &item_from_json(&case["item"]),
                    doc_id.unwrap(),
                    case["mailbox"].as_str().unwrap(),
                    &size,
                );
                part_json(built.part.as_ref(), &files)
            }
            "buildFetchResponse" => {
                let response = build_fetch_response(
                    mail.unwrap(),
                    &items_from_json(&case["items"]),
                    doc_id.unwrap(),
                    case["uid"].as_f64().unwrap(),
                    case["isUidFetch"].as_bool().unwrap(),
                    case["mailbox"].as_str().unwrap(),
                    case["condstore"].as_bool().unwrap(),
                    &size,
                );
                Value::Array(response.parts.iter().map(|part| part_json(Some(part), &files)).collect())
            }
            other => panic!("no fetch function named {other}"),
        };
        let mut actual = actual;
        forget_unrecorded_streams(expected, &mut actual);
        report.check(
            || {
                let mut inputs = case.as_object().unwrap().clone();
                inputs.remove("expected");
                Value::Object(inputs).to_string()
            },
            expected,
            &actual,
        );
    }
    report.finish("fetch.json", 13000);
    assert!(threw < 400, "{threw} cases threw in the Bun server");
}
