//! Differential tests against the TypeScript server.
//!
//! `tests/golden/generate.ts` runs the TypeScript functions over a corpus of
//! inputs and records inputs and outputs in `tests/golden/fixtures`. Each test
//! here runs the Rust port of one module over the same inputs and requires
//! the same output for every case.

use std::path::{Path, PathBuf};

use mail_server::mails::entities::{decode_attribute_entities, decode_entities};
use mail_server::mails::plain_text::{html_to_plain_text, PlainTextOptions};
use mail_server::mails::snippet::build_snippet;
use mail_server::mails::text_to_html::{body_html, plain_text_to_html};
use mail_server::mails::threading::{
    bare_message_id, collect_thread, has_thread_headers, is_reply_subject, linked_by_headers, linked_ids,
    matches_by_subject, normalize_subject, other_participants_of, stored_id_forms, ThreadMail,
};
use mail_server::mails::util::{get_text, nested_path};
use mail_server::mime::address::{parse_address_header, OneOrMany, ParsedAddress};
use mail_server::mime::charset::{decode_body_bytes, decode_header_bytes};
use mail_server::mime::header::{
    decode_words, ensure_message_id_format, parse_header_value, parse_message_id_list, raw_octets_as_utf8,
    split_header_line,
};
use mail_server::mime::parser::{
    date_parse, mime_type_from_filename, parse_message, MessageDate, ParseOptions, ParsedMessage,
};
use mail_server::mime::text::{
    addresses_to_html, embedded_headers_as_html, embedded_headers_as_text, encode_entities, text_to_html,
    EmbeddedHeaders,
};
use mail_server::mime::transfer_decoder::create_transfer_decoder;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// The fixtures: the committed ones, or those of a fuzzing run when
/// `GOLDEN_FIXTURES` names its directory (see `tests/golden/generate.ts`).
fn fixture(name: &str) -> Value {
    let directory = match std::env::var_os("GOLDEN_FIXTURES") {
        Some(directory) => PathBuf::from(directory),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/fixtures"),
    };
    let path = directory.join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn section<'a>(fixture: &'a Value, name: &str) -> &'a [Value] {
    fixture[name].as_array().unwrap_or_else(|| panic!("no section {name}"))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Outputs past this length are recorded by hash, as in the generator.
const INLINE_OUTPUT_LIMIT: usize = 4000;

/// A text the way the generator records one: itself, or its hash and length.
fn compact(text: &str) -> Value {
    if utf16_len(text) > INLINE_OUTPUT_LIMIT {
        json!({ "sha256": sha256(text.as_bytes()), "length": utf16_len(text) })
    } else {
        json!(text)
    }
}

/// A value with each long string in it recorded by hash.
fn compact_deep(value: Value) -> Value {
    match value {
        Value::String(text) => compact(&text),
        Value::Array(items) => Value::Array(items.into_iter().map(compact_deep).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, item)| (key, compact_deep(item)))
                .collect(),
        ),
        other => other,
    }
}

/// Bytes recorded a piece at a time, each piece Latin-1 text or `[text, n]`
/// for the text repeated.
fn raw_bytes(raw: &Value) -> Vec<u8> {
    let latin1 = |text: &str| -> Vec<u8> { text.chars().map(|c| u8::try_from(c as u32).expect("Latin-1")).collect() };
    let mut bytes = Vec::new();
    for piece in raw.as_array().expect("raw pieces") {
        match piece {
            Value::String(text) => bytes.extend(latin1(text)),
            repeated => {
                let text = latin1(repeated[0].as_str().expect("repeated text"));
                for _ in 0..repeated[1].as_u64().expect("repeat count") {
                    bytes.extend_from_slice(&text);
                }
            }
        }
    }
    bytes
}

fn shorten(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 600 {
        format!("{}…", text.chars().take(600).collect::<String>())
    } else {
        text
    }
}

/// Collects the cases where the port and the TypeScript disagree.
struct Comparison {
    name: &'static str,
    total: usize,
    failures: Vec<String>,
}

impl Comparison {
    fn new(name: &'static str) -> Comparison {
        Comparison {
            name,
            total: 0,
            failures: Vec::new(),
        }
    }

    fn check(&mut self, input: &dyn std::fmt::Debug, expected: &Value, actual: Value) {
        self.total += 1;
        if *expected != actual {
            self.failures.push(format!(
                "input:    {}\n  expected: {}\n  actual:   {}",
                shorten(&json!(format!("{input:?}"))),
                shorten(expected),
                shorten(&actual)
            ));
        }
    }

    fn finish(self) {
        println!("{}: {} golden cases", self.name, self.total);
        assert!(self.total > 0, "{}: no cases", self.name);
        if !self.failures.is_empty() {
            let shown: Vec<&str> = self.failures.iter().take(12).map(String::as_str).collect();
            panic!(
                "{}: {} of {} cases differ from the TypeScript\n\n  {}",
                self.name,
                self.failures.len(),
                self.total,
                shown.join("\n\n  ")
            );
        }
    }
}

/// Runs `port` over every `[input, output]` case of a section.
fn compare(name: &'static str, cases: &[Value], port: impl Fn(&Value) -> Value) {
    let mut comparison = Comparison::new(name);
    for case in cases {
        comparison.check(&case[0], &case[1], port(&case[0]));
    }
    comparison.finish();
}

fn text_of(value: &Value) -> &str {
    value.as_str().expect("a string input")
}

// ---------------------------------------------------------------------------
// charset
// ---------------------------------------------------------------------------

#[test]
fn charset_labels_and_decoding() {
    let fixture = fixture("charset");
    let inputs: Vec<Vec<u8>> = section(&fixture, "inputs")
        .iter()
        .map(|input| hex::decode(text_of(input)).unwrap())
        .collect();
    let few: Vec<&[u8]> = section(&fixture, "few")
        .iter()
        .map(|index| inputs[index.as_u64().unwrap() as usize].as_slice())
        .collect();
    let all: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();

    let mut comparison = Comparison::new("charset");
    for (name, bytes) in [("labels", &few), ("encodings", &all)] {
        for case in section(&fixture, name) {
            let label = text_of(&case[0]);
            for (index, input) in bytes.iter().enumerate() {
                let shown = (label, "header", hex::encode(input));
                comparison.check(&shown, &case[1][index], json!(decode_header_bytes(input, label)));
                let shown = (label, "body", hex::encode(input));
                comparison.check(&shown, &case[2][index], json!(decode_body_bytes(input, Some(label))));
            }
        }
    }
    let every_byte: Vec<u8> = (0..=255).collect();
    for case in section(&fixture, "singleByte") {
        let label = text_of(&case[0]);
        comparison.check(&label, &case[1], json!(decode_header_bytes(&every_byte, label)));
    }
    // Every two-byte sequence of the multi-byte encodings, a lead byte at a time.
    for case in section(&fixture, "grids") {
        let label = text_of(&case[0]);
        for (lead, expected) in (0x80..=0xffu8).zip(case[1].as_array().unwrap()) {
            let row: Vec<u8> = (0..=0xffu8).flat_map(|trail| [lead, trail]).collect();
            let decoded = decode_body_bytes(&row, Some(label));
            comparison.check(&(label, lead), expected, json!(sha256(decoded.as_bytes())));
        }
    }
    for case in section(&fixture, "wide") {
        let label = text_of(&case[0]);
        let rows: Vec<Vec<u8>> = match label {
            "euc-jp" => (0xa1..=0xfeu8)
                .map(|second| (0xa1..=0xfeu8).flat_map(|third| [0x8f, second, third]).collect())
                .collect(),
            "gb18030" => (0x81..=0xfeu8)
                .map(|first| {
                    (0x30..=0x39u8)
                        .flat_map(|second| (0x81..=0xfeu8).map(move |third| (second, third)))
                        .flat_map(|(second, third)| [0x30, 0x35, 0x39].map(|fourth| [first, second, third, fourth]))
                        .flatten()
                        .collect()
                })
                .collect(),
            "iso-2022-jp" => (0x21..=0x7eu8)
                .map(|lead| {
                    let mut row = vec![0x1b, 0x24, 0x42];
                    row.extend((0x21..=0x7eu8).flat_map(|trail| [lead, trail]));
                    row.extend([0x1b, 0x28, 0x42]);
                    row
                })
                .collect(),
            other => panic!("no rows for {other}"),
        };
        for (index, (row, expected)) in rows.iter().zip(case[1].as_array().unwrap()).enumerate() {
            let decoded = decode_body_bytes(row, Some(label));
            comparison.check(&(label, index), expected, json!(sha256(decoded.as_bytes())));
        }
    }
    for case in section(&fixture, "utf8") {
        let bytes = hex::decode(text_of(&case[0])).unwrap();
        comparison.check(&case[0], &case[1], json!(decode_body_bytes(&bytes, Some("utf-8"))));
        comparison.check(
            &case[0],
            &case[2],
            json!(decode_header_bytes(&bytes, "unicode-1-1-utf-8")),
        );
    }
    comparison.finish();
}

// ---------------------------------------------------------------------------
// header
// ---------------------------------------------------------------------------

#[test]
fn header_decode_words() {
    compare("decodeWords", section(&fixture("header"), "decodeWords"), |input| {
        json!(decode_words(text_of(input)))
    });
}

#[test]
fn header_parse_header_value() {
    compare(
        "parseHeaderValue",
        section(&fixture("header"), "parseHeaderValue"),
        |input| serde_json::to_value(parse_header_value(text_of(input))).unwrap(),
    );
}

#[test]
fn header_split_header_line() {
    compare(
        "splitHeaderLine",
        section(&fixture("header"), "splitHeaderLine"),
        |input| {
            let line = split_header_line(text_of(input));
            json!({ "key": line.key, "value": line.value })
        },
    );
}

#[test]
fn header_raw_octets_as_utf8() {
    compare(
        "rawOctetsAsUtf8",
        section(&fixture("header"), "rawOctetsAsUtf8"),
        |input| json!(raw_octets_as_utf8(text_of(input))),
    );
}

#[test]
fn header_message_ids() {
    let fixture = fixture("header");
    compare("parseMessageIdList", section(&fixture, "parseMessageIdList"), |input| {
        json!(parse_message_id_list(text_of(input)))
    });
    compare(
        "ensureMessageIdFormat",
        section(&fixture, "ensureMessageIdFormat"),
        |input| json!(ensure_message_id_format(text_of(input))),
    );
}

// ---------------------------------------------------------------------------
// address
// ---------------------------------------------------------------------------

#[test]
fn address_parse_address_header() {
    compare(
        "parseAddressHeader",
        section(&fixture("address"), "parseAddressHeader"),
        |input| serde_json::to_value(parse_address_header(text_of(input))).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// mime/text
// ---------------------------------------------------------------------------

#[test]
fn mime_text_to_html() {
    let fixture = fixture("mime-text");
    compare("textToHtml", section(&fixture, "textToHtml"), |input| {
        json!(text_to_html(text_of(input)))
    });
    compare("encodeEntities", section(&fixture, "encodeEntities"), |input| {
        json!(encode_entities(text_of(input)))
    });
    compare("addressesToHtml", section(&fixture, "addressesToHtml"), |input| {
        json!(addresses_to_html(&parse_address_header(text_of(input)).value))
    });
}

/// The headers an embedded-headers case describes by the header values they
/// come from.
fn embedded_headers(spec: &Value) -> EmbeddedHeaders {
    let recipients = |name: &str| -> Option<OneOrMany<ParsedAddress>> {
        let mut parsed: Vec<ParsedAddress> = spec[name]
            .as_array()?
            .iter()
            .map(|value| parse_address_header(text_of(value)))
            .collect();
        match parsed.len() {
            0 => None,
            1 => parsed.pop().map(OneOrMany::One),
            _ => Some(OneOrMany::Many(parsed)),
        }
    };
    EmbeddedHeaders {
        from: spec["from"].as_str().map(parse_address_header),
        subject: spec["subject"].as_str().map(str::to_string),
        date: spec["date"]
            .as_i64()
            .map(|ms| MessageDate::from_millis(ms).expect("a date in range")),
        to: recipients("to"),
        cc: recipients("cc"),
        bcc: recipients("bcc"),
    }
}

#[test]
fn mime_text_embedded_headers() {
    compare(
        "embeddedHeaders",
        section(&fixture("mime-text"), "embeddedHeaders"),
        |spec| {
            let headers = embedded_headers(spec);
            json!([embedded_headers_as_text(&headers), embedded_headers_as_html(&headers)])
        },
    );
}

// ---------------------------------------------------------------------------
// transfer decoder
// ---------------------------------------------------------------------------

#[test]
fn transfer_decoders() {
    let fixture = fixture("transfer-decoder");
    let mut comparison = Comparison::new("transfer decoder");
    for case in section(&fixture, "cases") {
        let encoding = text_of(&case[0]);
        let input = raw_bytes(&case[1]);
        for size in case[3].as_array().unwrap() {
            let size = size.as_u64().unwrap() as usize;
            let mut decoder = create_transfer_decoder(encoding);
            let mut decoded = Vec::new();
            for chunk in input.chunks(size) {
                decoder.write(chunk, &mut decoded);
            }
            decoder.end(&mut decoded);
            let actual = if decoded.len() > 4000 {
                json!({ "sha256": sha256(&decoded), "length": decoded.len() })
            } else {
                json!(hex::encode(&decoded))
            };
            comparison.check(&(encoding, &case[1], size), &case[2], actual);
        }
    }
    comparison.finish();
}

// ---------------------------------------------------------------------------
// entities, plain text, util
// ---------------------------------------------------------------------------

#[test]
fn entities() {
    let fixture = fixture("entities");
    compare("decodeEntities", section(&fixture, "decodeEntities"), |input| {
        json!(decode_entities(text_of(input)))
    });
    compare(
        "decodeAttributeEntities",
        section(&fixture, "decodeAttributeEntities"),
        |input| json!(decode_attribute_entities(text_of(input))),
    );
}

#[test]
fn plain_text_html_to_plain_text() {
    let render = |html: &str, urls: bool| match html_to_plain_text(html, PlainTextOptions { urls }) {
        Ok(text) => compact(&text),
        Err(failure) => json!({ "error": failure.to_string() }),
    };
    compare(
        "htmlToPlainText",
        section(&fixture("plain-text"), "htmlToPlainText"),
        |input| json!([render(text_of(input), false), render(text_of(input), true)]),
    );
}

#[test]
fn util_get_text() {
    let fixture = fixture("util");
    compare("getText", section(&fixture, "getText"), |input| {
        compact(&get_text(text_of(input)))
    });
    compare("nestedPath", section(&fixture, "nestedPath"), |input| {
        json!(nested_path(text_of(input)))
    });
}

// ---------------------------------------------------------------------------
// snippet, text-to-html
// ---------------------------------------------------------------------------

#[test]
fn snippet_build_snippet() {
    compare("buildSnippet", section(&fixture("snippet"), "buildSnippet"), |input| {
        json!(build_snippet(input.as_str()))
    });
}

#[test]
fn text_to_html_plain_text_to_html() {
    let fixture = fixture("text-to-html");
    compare("plainTextToHtml", section(&fixture, "plainTextToHtml"), |input| {
        json!(plain_text_to_html(text_of(input)))
    });
    compare("bodyHtml", section(&fixture, "bodyHtml"), |input| {
        json!(body_html(input[0].as_str(), input[1].as_str()))
    });
}

// ---------------------------------------------------------------------------
// threading
// ---------------------------------------------------------------------------

fn is_own(address: &str) -> bool {
    address.to_lowercase().ends_with("@inbox.test")
}

fn mail(value: &Value) -> ThreadMail {
    serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("{value}: {error}"))
}

#[test]
fn threading() {
    let fixture = fixture("threading");
    compare("normalizeSubject", section(&fixture, "normalizeSubject"), |input| {
        json!(normalize_subject(text_of(input)))
    });
    compare("isReplySubject", section(&fixture, "isReplySubject"), |input| {
        json!(is_reply_subject(text_of(input)))
    });
    compare("bareMessageId", section(&fixture, "bareMessageId"), |input| {
        json!(bare_message_id(text_of(input)))
    });
    compare("storedIdForms", section(&fixture, "storedIdForms"), |input| {
        json!(stored_id_forms(input.as_array().unwrap().iter().map(text_of)))
    });
    // hasThreadHeaders, linkedIds and otherParticipantsOf of each mail.
    let mails: Vec<ThreadMail> = section(&fixture, "mails").iter().map(|case| mail(&case[0])).collect();
    compare(
        "hasThreadHeaders, linkedIds, otherParticipantsOf",
        section(&fixture, "mails"),
        |input| {
            let mail = mail(input);
            let mut participants: Vec<String> = other_participants_of(&mail, is_own).into_iter().collect();
            // As JavaScript sorts strings: by UTF-16 code unit.
            participants.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            json!([has_thread_headers(&mail), linked_ids(&mail), participants])
        },
    );
    // Pairs name their mails by position.
    let pair = |input: &Value| {
        (
            &mails[input[0].as_u64().unwrap() as usize],
            &mails[input[1].as_u64().unwrap() as usize],
        )
    };
    compare("matchesBySubject", section(&fixture, "matchesBySubject"), |input| {
        let (a, b) = pair(input);
        json!(matches_by_subject(a, b, is_own))
    });
    compare("linkedByHeaders", section(&fixture, "linkedByHeaders"), |input| {
        let (a, b) = pair(input);
        json!(linked_by_headers(a, b))
    });
    compare("collectThread", section(&fixture, "collectThread"), |input| {
        let seed = mail(&input[0]);
        let candidates: Vec<ThreadMail> = input[1].as_array().unwrap().iter().map(mail).collect();
        let thread = collect_thread(&seed, &candidates, is_own);
        json!(thread.iter().map(|mail| mail.mail_id.as_str()).collect::<Vec<_>>())
    });
}

// ---------------------------------------------------------------------------
// dates and file types
// ---------------------------------------------------------------------------

#[test]
fn dates() {
    let fixture = fixture("dates");
    compare("new Date(text)", section(&fixture, "parse"), |input| {
        json!(date_parse(text_of(input)))
    });
    compare("Date formatting", section(&fixture, "format"), |input| {
        let date = MessageDate::from_millis(input.as_i64().unwrap()).expect("a date in range");
        json!([date.to_iso_string(), date.to_utc_string()])
    });
}

/// An error message the way the generator records one: a long one by its
/// start and hash.
fn error_text(message: &str) -> Value {
    if utf16_len(message) > 300 {
        json!({ "start": message.chars().take(80).collect::<String>(), "sha256": sha256(message.as_bytes()) })
    } else {
        json!(message)
    }
}

fn file_type(name: &str) -> Value {
    match mime_type_from_filename(name) {
        Ok(mime_type) => json!(mime_type),
        Err(failure) => json!({ "error": error_text(&failure.to_string()) }),
    }
}

#[test]
fn file_types_by_extension() {
    let fixture = fixture("mime-types");
    let table = section(&fixture, "table");
    compare("file type table", table, |input| {
        file_type(&format!("x.{}", text_of(input)))
    });
    compare("unknown extensions", section(&fixture, "unknown"), |input| {
        file_type(text_of(input))
    });
    compare("mimeTypeFromFilename", section(&fixture, "names"), |input| {
        file_type(text_of(input))
    });

    // The table holds no extension of up to three letters and digits that Bun's does not.
    let known: std::collections::HashSet<&str> = table.iter().map(|entry| text_of(&entry[0])).collect();
    let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789".chars().collect();
    let mut names: Vec<String> = alphabet.iter().map(char::to_string).collect();
    for length in 1..3 {
        let longer: Vec<String> = names
            .iter()
            .filter(|name| name.len() == length)
            .flat_map(|name| alphabet.iter().map(move |letter| format!("{name}{letter}")))
            .collect();
        names.extend(longer);
    }
    for name in names.iter().filter(|name| !known.contains(name.as_str())) {
        assert_eq!(
            file_type(&format!("x.{name}")),
            json!("application/octet-stream"),
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// parser
// ---------------------------------------------------------------------------

struct ParsedCase {
    result: anyhow::Result<ParsedMessage>,
    /// The files of the attachment directory, in the order they were made.
    files: Vec<(String, Vec<u8>)>,
    attachment_dir: PathBuf,
    /// When parsing started and ended: a date that cannot be read is in between.
    window: (i64, i64),
}

async fn parse_in_chunks(raw: &[u8], chunk_size: usize) -> ParsedCase {
    let root = tempfile::tempdir().expect("a temporary directory");
    let attachment_dir = root.path().join("attachments");
    let chunks: Vec<std::io::Result<bytes::Bytes>> = raw
        .chunks(chunk_size)
        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
        .collect();
    let source = tokio_util::io::StreamReader::new(futures_util::stream::iter(chunks));
    let started = MessageDate::now().timestamp_millis();
    let result = parse_message(
        source,
        &ParseOptions {
            attachment_dir: attachment_dir.clone(),
        },
    )
    .await;
    let ended = MessageDate::now().timestamp_millis();

    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&attachment_dir) {
        for entry in entries {
            let entry = entry.expect("a directory entry");
            files.push((
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("an attachment file"),
            ));
        }
    }
    files.sort_by_key(|(name, _)| name.parse::<u64>().unwrap_or(u64::MAX));
    ParsedCase {
        result,
        files,
        attachment_dir,
        window: (started, ended),
    }
}

/// The parse result the way the generator records one.
fn recorded(parsed: &ParsedCase, frozen_now: i64) -> Value {
    let message = match &parsed.result {
        Ok(message) => message,
        Err(failure) => return json!({ "error": error_text(&failure.to_string()) }),
    };
    // The generator freezes "now"; whatever this run read as now is put back to that instant.
    let frozen = MessageDate::from_millis(frozen_now).expect("a date in range");
    let (started, ended) = parsed.window;
    let unfreeze = |text: &str| -> String {
        let mut text = text.to_string();
        for second in started / 1000..=ended / 1000 {
            let now = MessageDate::from_millis(second * 1000).expect("a date in range");
            text = text.replace(&now.to_utc_string(), &frozen.to_utc_string());
        }
        text
    };

    let mut ok = Map::new();
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            ok.insert(key.to_string(), value);
        }
    };
    let address = |value: &Option<ParsedAddress>| {
        value
            .as_ref()
            .map(|value| compact_deep(serde_json::to_value(value).unwrap()))
    };
    let addresses = |value: &Option<OneOrMany<ParsedAddress>>| {
        value
            .as_ref()
            .map(|value| compact_deep(serde_json::to_value(value).unwrap()))
    };
    put("messageId", message.message_id.as_ref().map(|id| compact(id)));
    put("inReplyTo", message.in_reply_to.as_ref().map(|id| compact(id)));
    put(
        "references",
        message.references.as_ref().map(|ids| compact_deep(json!(ids))),
    );
    put("from", address(&message.from));
    put("to", addresses(&message.to));
    put("cc", addresses(&message.cc));
    put("bcc", addresses(&message.bcc));
    put("replyTo", address(&message.reply_to));
    put("subject", message.subject.as_ref().map(|subject| compact(subject)));
    put(
        "date",
        message.date.map(|date| {
            let ms = date.timestamp_millis();
            json!(if (started..=ended).contains(&ms) {
                frozen_now
            } else {
                ms
            })
        }),
    );
    put("text", message.text.as_ref().map(|text| compact(&unfreeze(text))));
    put("html", message.html.as_ref().map(|html| compact(&unfreeze(html))));
    let attachments: Vec<Value> = message
        .attachments
        .iter()
        .map(|attachment| {
            let mut value = serde_json::to_value(attachment).unwrap();
            let relative = attachment
                .path
                .strip_prefix(&parsed.attachment_dir)
                .expect("a file of the attachment directory");
            value["path"] = json!(relative.to_string_lossy());
            compact_deep(value)
        })
        .collect();
    put("attachments", Some(json!(attachments)));
    let files: Vec<Value> = parsed
        .files
        .iter()
        .map(|(name, content)| {
            if content.len() > 256 {
                json!({ "name": name, "sha256": sha256(content), "length": content.len() })
            } else {
                json!({ "name": name, "hex": hex::encode(content) })
            }
        })
        .collect();
    put("files", Some(json!(files)));
    json!({ "ok": ok })
}

#[tokio::test(flavor = "multi_thread")]
async fn parser_parse_message() {
    let fixture = fixture("parser");
    let frozen_now = fixture["frozenNow"][0].as_i64().expect("the frozen instant");
    let mut comparison = Comparison::new("parseMessage");
    for case in section(&fixture, "cases") {
        let raw = raw_bytes(&case["raw"]);
        for size in case["chunkSizes"].as_array().unwrap() {
            let size = size.as_u64().unwrap() as usize;
            let parsed = parse_in_chunks(&raw, size).await;
            comparison.check(
                &(text_of(&case["name"]), size),
                &case["result"],
                recorded(&parsed, frozen_now),
            );
        }
    }
    comparison.finish();
}

// ---------------------------------------------------------------------------
// known differences
// ---------------------------------------------------------------------------

/// The cases the port cannot answer as the TypeScript does. Each records
/// what the TypeScript answers, what the port answers instead and why; the
/// port is held to its side.
#[test]
fn known_differences() {
    let fixture = fixture("known-differences");
    let mut comparison = Comparison::new("known differences");
    for entry in fixture.as_array().expect("a list of known differences") {
        let input = &entry["input"];
        let function = text_of(&entry["function"]);
        assert!(!text_of(&entry["why"]).is_empty() && !text_of(&entry["typescript"]).is_empty());
        let actual = match function {
            "buildSnippet" => json!(build_snippet(input.as_str())),
            "nestedPath" => json!(nested_path(text_of(input))),
            "htmlToPlainText" => {
                let render = |urls: bool| match html_to_plain_text(text_of(input), PlainTextOptions { urls }) {
                    Ok(text) => compact(&text),
                    Err(failure) => json!({ "error": failure.to_string() }),
                };
                json!([render(false), render(true)])
            }
            // The TypeScript throws; the port answers the text it could render.
            "getText" => {
                get_text(text_of(input));
                assert!(html_to_plain_text(text_of(input), PlainTextOptions::default()).is_err());
                Value::Null
            }
            other => panic!("no port to run for the known difference of {other}"),
        };
        comparison.check(&(function, input), &entry["rust"], actual);
    }
    comparison.finish();
}
