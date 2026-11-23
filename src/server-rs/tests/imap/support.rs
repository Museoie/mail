//! Helpers shared by the IMAP tests.

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine;
use mail_server::imap::fetch_format::{FetchResponsePart, StreamPart};
use mail_server::imap::session_format::{
    attachment_base64, base64_chunks, plan_partial_from_segments, MessageSegment,
};
use mail_server::imap::types::{
    BodyFetch, BodySection, FetchDataItem, FetchMailInput, Literal, MessageLiteral, MimeSubSection, PartialRange,
    SequenceRange, SequenceSet,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/imap").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Whether `actual` is what the fixture recorded. A long string is recorded
/// as `{"$sha256", "$length"}`: its digest and its length in UTF-16 units.
pub fn same(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(digest), Value::String(text)) if digest.contains_key("$sha256") => {
            digest["$sha256"].as_str() == Some(sha256_hex(text.as_bytes()).as_str())
                && digest["$length"].as_u64() == Some(text.encode_utf16().count() as u64)
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(key, value)| b.get(key).is_some_and(|other| same(value, other)))
        }
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y)),
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (a, b) => a == b,
    }
}

/// Collects the cases of one fixture that came out differently, and fails
/// the test with the first few of them.
#[derive(Default)]
pub struct Report {
    checked: usize,
    failures: Vec<String>,
}

impl Report {
    pub fn check(&mut self, what: impl FnOnce() -> String, expected: &Value, actual: &Value) {
        self.checked += 1;
        if !same(expected, actual) {
            let clip = |value: &Value| value.to_string().chars().take(700).collect::<String>();
            self.failures.push(format!("{}\n  expected: {}\n  actual:   {}", what(), clip(expected), clip(actual)));
        }
    }

    pub fn fail(&mut self, message: String) {
        self.checked += 1;
        self.failures.push(message);
    }

    /// Fails unless every case matched and there were at least `at_least` of them.
    pub fn finish(self, name: &str, at_least: usize) {
        assert!(
            self.failures.is_empty(),
            "{name}: {} of {} cases differ from the Bun server\n\n{}",
            self.failures.len(),
            self.checked,
            self.failures.iter().take(15).cloned().collect::<Vec<_>>().join("\n\n")
        );
        assert!(self.checked >= at_least, "{name}: only {} cases ran, expected at least {at_least}", self.checked);
    }
}

/// `JSON.parse` of text the port wrote.
pub fn parse_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("the port wrote invalid JSON ({error}): {text}"))
}

pub fn literal_from_json(value: &Value) -> Literal {
    match value {
        Value::String(text) => Literal::Text(text.clone()),
        other => Literal::Message(MessageLiteral {
            path: other["path"].as_str().unwrap_or_default().to_string(),
            size: other["size"].as_u64().unwrap_or_default(),
        }),
    }
}

pub fn literals_from_json(value: Option<&Value>) -> Option<Vec<Literal>> {
    value?.as_array().map(|items| items.iter().map(literal_from_json).collect())
}

pub fn section_from_json(value: &Value) -> BodySection {
    match value["type"].as_str().unwrap_or_default() {
        "FULL" => BodySection::Full,
        "HEADER" => BodySection::Header,
        "TEXT" => BodySection::Text,
        "MIME_PART" => BodySection::MimePart {
            part_number: value["partNumber"].as_str().unwrap_or_default().to_string(),
            sub_section: match value["subSection"].as_str() {
                Some("HEADER") => Some(MimeSubSection::Header),
                Some("TEXT") => Some(MimeSubSection::Text),
                Some("MIME") => Some(MimeSubSection::Mime),
                _ => None,
            },
        },
        "HEADER_FIELDS" => BodySection::HeaderFields {
            not: value["not"].as_bool().unwrap_or(false),
            fields: value["fields"]
                .as_array()
                .map(|fields| fields.iter().filter_map(|field| field.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        },
        other => panic!("unknown section type {other}"),
    }
}

pub fn partial_from_json(value: &Value) -> Option<PartialRange> {
    value.as_object().map(|_| PartialRange {
        start: value["start"].as_f64().unwrap_or_default(),
        length: value["length"].as_f64().unwrap_or_default(),
    })
}

pub fn body_fetch_from_json(value: &Value) -> BodyFetch {
    BodyFetch {
        peek: value["peek"].as_bool().unwrap_or(false),
        section: section_from_json(&value["section"]),
        partial: partial_from_json(&value["partial"]),
    }
}

pub fn item_from_json(value: &Value) -> FetchDataItem {
    match value["type"].as_str().unwrap_or_default() {
        "ENVELOPE" => FetchDataItem::Envelope,
        "FLAGS" => FetchDataItem::Flags,
        "BODYSTRUCTURE" => FetchDataItem::BodyStructure { extensible: value["extensible"].as_bool().unwrap_or(false) },
        "BODY" => FetchDataItem::Body(body_fetch_from_json(value)),
        "UID" => FetchDataItem::Uid,
        "INTERNALDATE" => FetchDataItem::InternalDate,
        "RFC822" => FetchDataItem::Rfc822,
        "RFC822.HEADER" => FetchDataItem::Rfc822Header,
        "RFC822.SIZE" => FetchDataItem::Rfc822Size,
        "RFC822.TEXT" => FetchDataItem::Rfc822Text,
        "MODSEQ" => FetchDataItem::Modseq,
        other => panic!("unknown data item {other}"),
    }
}

pub fn items_from_json(value: &Value) -> Vec<FetchDataItem> {
    value.as_array().map(|items| items.iter().map(item_from_json).collect()).unwrap_or_default()
}

pub fn ranges_from_json(value: &Value) -> Vec<SequenceRange> {
    value
        .as_array()
        .map(|ranges| {
            ranges
                .iter()
                .map(|range| SequenceRange { start: range["start"].as_f64().unwrap_or_default(), end: range["end"].as_f64() })
                .collect()
        })
        .unwrap_or_default()
}

pub fn set_from_json(value: &Value) -> SequenceSet {
    SequenceSet::new(ranges_from_json(&value["ranges"]))
}

pub fn mail_from_json(value: &Value) -> FetchMailInput {
    serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("mail fixture {value}: {error}"))
}

/// A mail built from the JSON a Bun test would write as its object literal.
pub fn mail(value: Value) -> FetchMailInput {
    mail_from_json(&value)
}

/// Attachment files a test or a fixture provides, by data id.
#[derive(Default)]
pub struct AttachmentFiles(pub HashMap<String, Vec<u8>>);

impl AttachmentFiles {
    pub fn from_fixture(value: &Value) -> AttachmentFiles {
        let decode = |encoded: &Value| {
            base64::engine::general_purpose::STANDARD.decode(encoded.as_str().unwrap_or_default()).unwrap_or_default()
        };
        AttachmentFiles(
            value.as_object().map(|map| map.iter().map(|(id, bytes)| (id.clone(), decode(bytes))).collect()).unwrap_or_default(),
        )
    }

    pub fn with(mut self, id: &str, bytes: &[u8]) -> AttachmentFiles {
        self.0.insert(id.to_string(), bytes.to_vec());
        self
    }

    pub fn size(&self, id: &str) -> Option<u64> {
        self.0.get(id).map(|bytes| bytes.len() as u64)
    }

    pub fn read(&self, id: &str) -> Option<Vec<u8>> {
        self.0.get(id).cloned()
    }
}

/// The wire bytes of one segment, or `None` for one only the database can supply.
pub fn segment_bytes(segment: &MessageSegment, files: &AttachmentFiles) -> Option<Vec<u8>> {
    match segment {
        MessageSegment::Literal { value, .. } => Some(value.as_bytes().to_vec()),
        MessageSegment::Base64 { source, .. } => Some(base64_chunks(source).collect::<String>().into_bytes()),
        MessageSegment::Attachment { data_id, raw_size, .. } => {
            Some(attachment_base64(*raw_size, files.read(data_id).as_deref()).into_bytes())
        }
        MessageSegment::LazyText { .. } => None,
    }
}

/// The octets a stream part puts on the wire, the way the session code is
/// meant to emit them: each segment in order, a partial fetch through
/// `plan_partial_from_segments`, then the trailer.
pub fn stream_bytes(part: &StreamPart, files: &AttachmentFiles) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match part.window {
        None => {
            for segment in &part.segments {
                out.extend(segment_bytes(segment, files)?);
            }
        }
        Some((start, length)) => {
            for slice in plan_partial_from_segments(&part.segments, start, length) {
                let bytes = segment_bytes(&part.segments[slice.index], files)?;
                out.extend_from_slice(&bytes[slice.skip as usize..(slice.skip + slice.take) as usize]);
            }
        }
    }
    if part.trailer {
        out.extend_from_slice(b"\r\n");
    }
    Some(out)
}

/// A response part as the fixtures record one.
pub fn part_json(part: Option<&FetchResponsePart>, files: &AttachmentFiles) -> Value {
    match part {
        None => Value::Null,
        Some(FetchResponsePart::Simple { content }) => serde_json::json!({ "type": "simple", "content": content }),
        Some(FetchResponsePart::Literal { content, header, length }) => {
            serde_json::json!({ "type": "literal", "content": content, "header": header, "length": length })
        }
        Some(FetchResponsePart::Stream(stream)) => {
            let mut out = serde_json::json!({ "type": "stream", "header": stream.header, "length": stream.length });
            if let Some(bytes) = stream_bytes(stream, files) {
                out["emitted"] = serde_json::json!(bytes.len());
                out["sha256"] = serde_json::json!(sha256_hex(&bytes));
            }
            out
        }
    }
}
