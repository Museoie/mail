//! The streaming half of a FETCH response, run through `write_fetch_response`
//! to an in-memory socket (the streaming cases of fetch-helpers.test.ts and
//! session-utils.test.ts, and block 2 of command-budget-wiring.test.ts).
//!
//! The pure half, what a part announces, is pinned in `tests/imap/fetch.rs`.
//! These hold the emitters to it: every literal carries exactly the octets
//! it announced, the octets are the materialized message's, and a body takes
//! its stream lock and budget slot the way the Bun server's generators do.
//! Database bodies (`LazyText`) need PostgreSQL and are covered by the e2e
//! suite.

use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use super::*;
use crate::imap::body_budget::{body_budget_capacity, reset_body_budget, BodyBudgetSlot};
use crate::imap::command_budget::{
    acquire_command_budget, command_budget_capacity, command_budget_in_flight, release_command_budget,
    reset_command_budget,
};
use crate::imap::command_budget_hold::{create_command_budget_hold, run_in_command_budget_context};
use crate::imap::fetch_format::build_fetch_response;
use crate::imap::session_format::build_full_message;
use crate::imap::socket::SocketHandle;
use crate::imap::stream_mutex::{reset_stream_mutex, stream_mutex_held_count};
use crate::imap::types::{BodyFetch, BodySection, FetchDataItem, FetchMailInput, PartialRange};
use crate::mails::util::ATTACHMENT_FOLDER;

const MARKER: &str = "filename=\"a.bin\"\r\n\r\n";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Attachment files under the real attachment folder, each id unique to the
/// run, removed when dropped.
#[derive(Default)]
struct Files {
    ids: Vec<String>,
}

impl Files {
    /// Writes `data` as a fresh attachment and returns its id.
    fn write(&mut self, data: &[u8]) -> String {
        let id = format!("fetch-stream-test-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(ATTACHMENT_FOLDER).unwrap();
        std::fs::write(get_attachment_file_path(Some(&id)), data).unwrap();
        self.ids.push(id.clone());
        id
    }

    /// An id no file has.
    fn missing(&mut self) -> String {
        let id = format!("fetch-stream-test-missing-{}", uuid::Uuid::new_v4());
        self.ids.push(id.clone());
        id
    }

    fn replace(&self, id: &str, data: &[u8]) {
        std::fs::write(get_attachment_file_path(Some(id)), data).unwrap();
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        for id in &self.ids {
            let _ = std::fs::remove_file(get_attachment_file_path(Some(id)));
        }
    }
}

/// What the server measures an attachment at: its file.
fn file_size(id: &str) -> Option<u64> {
    std::fs::metadata(get_attachment_file_path(Some(id))).ok().filter(|meta| meta.is_file()).map(|meta| meta.len())
}

fn read_file(id: &str) -> Option<Vec<u8>> {
    std::fs::read(get_attachment_file_path(Some(id))).ok()
}

fn base() -> Value {
    json!({
        "uid": { "account": 1, "domain": 1 },
        "messageId": "<divergent@local>",
        "date": "2026-07-30T00:00:00Z",
        "from": { "text": "alice@example.com", "value": [] },
        "to": { "text": "bob@example.com", "value": [] },
        "subject": "divergent",
        "text": "see attached",
        "html": "",
        "attachments": []
    })
}

fn with(mut value: Value, changes: Value) -> Value {
    for (key, change) in changes.as_object().unwrap() {
        value[key] = change.clone();
    }
    value
}

fn mail(value: Value) -> FetchMailInput {
    serde_json::from_value(value).expect("a mail")
}

fn attachment(id: &str, size: Option<u64>) -> Value {
    let mut entry = json!({ "filename": "a.bin", "contentType": "application/octet-stream", "content": { "data": id } });
    if let Some(size) = size {
        entry["size"] = json!(size);
    }
    entry
}

fn with_attachment(id: &str, stored_size: Option<u64>) -> FetchMailInput {
    mail(with(base(), json!({ "attachments": [attachment(id, stored_size)] })))
}

fn body(section: BodySection, partial: Option<(f64, f64)>) -> FetchDataItem {
    FetchDataItem::Body(BodyFetch {
        peek: true,
        section,
        partial: partial.map(|(start, length)| PartialRange { start, length }),
    })
}

fn full() -> FetchDataItem {
    body(BodySection::Full, None)
}

fn parts_for(message: &FetchMailInput, items: &[FetchDataItem], doc_id: &str) -> Vec<FetchResponsePart> {
    build_fetch_response(message, items, doc_id, 1.0, false, "INBOX", false, &file_size).parts
}

/// Writes `parts` as message 1's FETCH response and returns every octet the
/// client received, the socket's write side ended once the response is out.
async fn write_and_read(parts: &[FetchResponsePart]) -> Vec<u8> {
    let (socket, _read, mut client) = SocketHandle::for_test("203.0.113.7");
    let session = ImapSession::new(socket, false);
    let writing = async {
        write_fetch_response(&session, 1, parts).await.unwrap();
        session.socket().end();
    };
    let reading = async {
        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();
        received
    };
    let ((), received) = tokio::join!(writing, reading);
    received
}

/// As `write_and_read`, with what the emitters did.
async fn write_and_probe(parts: &[FetchResponsePart]) -> (Vec<u8>, Probe) {
    PROBE
        .scope(RefCell::new(Probe::default()), async {
            let received = write_and_read(parts).await;
            (received, PROBE.with(|probe| std::mem::take(&mut *probe.borrow_mut())))
        })
        .await
}

/// A response whose one literal comes last: the text before the literal's
/// `{N}`, N, and the literal. The response must end right after it.
fn split_literal(wire: &[u8]) -> (String, u64, Vec<u8>) {
    let open = wire.iter().position(|&byte| byte == b'{').expect("a literal");
    let close = open + wire[open..].iter().position(|&byte| byte == b'}').unwrap();
    let length: u64 = std::str::from_utf8(&wire[open + 1..close]).unwrap().parse().unwrap();
    assert_eq!(&wire[close + 1..close + 3], b"\r\n");
    let start = close + 3;
    let end = start + length as usize;
    assert!(end <= wire.len(), "the literal announced {length} octets and fewer came");
    assert_eq!(&wire[end..], b")\r\n", "the literal announced {length} octets and more came");
    (String::from_utf8(wire[..open].to_vec()).unwrap(), length, wire[start..end].to_vec())
}

/// The literal of a one-item response.
async fn literal(message: &FetchMailInput, item: FetchDataItem, doc_id: &str) -> Vec<u8> {
    split_literal(&write_and_read(&parts_for(message, &[item], doc_id)).await).2
}

/// The base64 payload of the part named `filename`, decoded.
fn decode_part(wire: &[u8], filename: &str) -> Vec<u8> {
    let wire = String::from_utf8_lossy(wire);
    let marker = format!("filename=\"{filename}\"\r\n\r\n");
    let from = wire.find(&marker).unwrap_or_else(|| panic!("part {filename} not on the wire")) + marker.len();
    let end = wire[from..].find("\r\n--").map_or(wire.len(), |at| from + at);
    base64::engine::general_purpose::STANDARD.decode(&wire[from..end]).expect("clean base64")
}

/// RFC822.SIZE and BODY[] of one message, through one response: the size
/// reported, the length the literal announced, and the literal.
async fn drain(message: &FetchMailInput, doc_id: &str) -> (u64, u64, Vec<u8>) {
    let wire = write_and_read(&parts_for(message, &[FetchDataItem::Rfc822Size, full()], doc_id)).await;
    let (before, declared, literal) = split_literal(&wire);
    let reported: u64 = before
        .split("RFC822.SIZE ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|size| size.parse().ok())
        .expect("an RFC822.SIZE");
    (reported, declared, literal)
}

fn patterned(length: usize, step: usize, modulo: usize) -> Vec<u8> {
    (0..length).map(|index| ((index * step) % modulo) as u8).collect()
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// Every slot of the body budget, held.
async fn fill_body_budget() -> Vec<BodyBudgetSlot> {
    let mut slots = Vec::new();
    for _ in 0..body_budget_capacity() {
        slots.push(tokio::time::timeout(Duration::from_secs(1), acquire_body_budget()).await.expect("a free slot"));
    }
    slots
}

// ---------------------------------------------------------------------------
// BODY[] {N} holds when the attachment file disagrees with the mail row
// ---------------------------------------------------------------------------

mod literal_holds {
    use super::*;

    #[tokio::test]
    async fn stored_size_larger_than_the_file_on_disk() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let original = vec![7u8; 1024];
        let id = files.write(&original);
        let (reported, declared, wire) = drain(&with_attachment(&id, Some(999_999)), "doc-too-big").await;
        assert_eq!(wire.len() as u64, reported);
        assert_eq!(declared, reported);
        // The size follows the file, so nothing pads the attachment out.
        assert_eq!(decode_part(&wire, "a.bin"), original);
    }

    #[tokio::test]
    async fn stored_size_smaller_than_the_file_on_disk() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let original = vec![9u8; 200_000];
        let id = files.write(&original);
        let (reported, declared, wire) = drain(&with_attachment(&id, Some(11)), "doc-too-small").await;
        assert_eq!(wire.len() as u64, reported);
        assert_eq!(declared, reported);
        assert_eq!(decode_part(&wire, "a.bin"), original);
    }

    #[tokio::test]
    async fn stored_size_zero() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let original = vec![3u8; 50_000];
        let id = files.write(&original);
        let (reported, _, wire) = drain(&with_attachment(&id, Some(0)), "doc-zero").await;
        assert_eq!(wire.len() as u64, reported);
        assert_eq!(decode_part(&wire, "a.bin"), original);
    }

    #[tokio::test]
    async fn no_stored_size() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let id = files.write(&[1u8; 4096]);
        let (reported, _, wire) = drain(&with_attachment(&id, None), "doc-undef-size").await;
        assert_eq!(wire.len() as u64, reported);
        assert_eq!(decode_part(&wire, "a.bin"), vec![1u8; 4096]);
    }

    #[tokio::test]
    async fn attachment_file_missing_entirely() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let id = files.missing();
        let (reported, declared, wire) = drain(&with_attachment(&id, Some(1_398_738)), "doc-missing").await;
        assert_eq!(wire.len() as u64, reported);
        assert_eq!(declared, reported);
        assert_eq!(decode_part(&wire, "a.bin"), MISSING_ATTACHMENT_NOTICE.as_bytes());
    }

    #[tokio::test]
    async fn multiple_attachments_mixed_present_and_missing() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let present = files.write(&[5u8; 120_000]);
        let gone = files.missing();
        let message = mail(with(
            base(),
            json!({ "attachments": [
                { "filename": "present.bin", "contentType": "application/octet-stream", "size": 120_000, "content": { "data": present } },
                { "filename": "gone.bin", "contentType": "application/octet-stream", "size": 777_777, "content": { "data": gone } }
            ] }),
        ));
        let (reported, _, wire) = drain(&message, "doc-multi").await;
        assert_eq!(wire.len() as u64, reported);
        assert_eq!(decode_part(&wire, "present.bin"), vec![5u8; 120_000]);
        assert_eq!(decode_part(&wire, "gone.bin"), MISSING_ATTACHMENT_NOTICE.as_bytes());
    }

    // Every branch of the MIME layout, with a real file on disk.
    #[tokio::test]
    async fn the_literal_matches_the_emitted_octets_for_every_layout() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let original = vec![11u8; 60_000];
        let id = files.write(&original);
        for (label, text, html) in [
            ("text + html + attachment", "plain", "<p>rich</p>"),
            ("html + attachment", "", "<p>rich</p>"),
            ("attachment only", "", ""),
            ("multibyte text + attachment", "café ☕ 日本語 😀", ""),
        ] {
            let message =
                mail(with(base(), json!({ "text": text, "html": html, "attachments": [attachment(&id, Some(60_000))] })));
            let (reported, declared, wire) = drain(&message, "doc-layout").await;
            assert_eq!(wire.len() as u64, reported, "{label}");
            assert_eq!(declared, reported, "{label}");
            assert_eq!(decode_part(&wire, "a.bin"), original, "{label}");
        }
    }

    #[tokio::test]
    async fn a_multi_slice_attachment_round_trips_byte_for_byte() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let original = patterned(200_000, 1, 251);
        let id = files.write(&original);
        let wire = literal(&with_attachment(&id, Some(original.len() as u64)), full(), "doc-slices").await;
        let text = String::from_utf8(wire).unwrap();
        let start = text.find(MARKER).unwrap() + MARKER.len();
        let end = start + text[start..].find("\r\n--").unwrap();
        let encoded = &text[start..end];
        // A slice size that is not a multiple of 3 would put `=` mid-stream.
        assert_eq!(encoded.len(), original.len().div_ceil(3) * 4);
        assert!(!encoded[..encoded.len() - 4].contains('='));
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(encoded).unwrap(), original);
    }

    // The segment list is built once per fetch: the file changing between the
    // announcement and the emit cannot desync the literal.
    #[tokio::test]
    async fn a_file_that_grows_after_the_announcement_is_trimmed_to_it() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let id = files.write(&[7u8; 1024]);
        let parts = parts_for(&with_attachment(&id, Some(1024)), &[full()], "doc-grows");
        files.replace(&id, &[9u8; 64 * 1024]);
        let (_, _, wire) = split_literal(&write_and_read(&parts).await);
        // The announced 1368 octets of the new file's base64, cut there.
        let text = String::from_utf8(wire).unwrap();
        let start = text.find(MARKER).unwrap() + MARKER.len();
        let end = start + text[start..].find("\r\n--").unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode([9u8; 64 * 1024]);
        assert_eq!(&text[start..end], &encoded[..1368]);
    }

    #[tokio::test]
    async fn a_file_that_shrinks_after_the_announcement_is_padded_with_newlines() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let id = files.write(&[7u8; 3000]);
        let parts = parts_for(&with_attachment(&id, Some(3000)), &[full()], "doc-shrinks");
        files.replace(&id, &[8u8; 300]);
        let (_, _, wire) = split_literal(&write_and_read(&parts).await);
        let text = String::from_utf8(wire).unwrap();
        let start = text.find(MARKER).unwrap() + MARKER.len();
        let end = start + text[start..].find("\r\n--").unwrap();
        let payload = &text[start..end];
        assert_eq!(payload.len(), 4000);
        let encoded = base64::engine::general_purpose::STANDARD.encode([8u8; 300]);
        assert_eq!(&payload[..encoded.len()], encoded);
        assert!(payload[encoded.len()..].bytes().all(|byte| byte == b'\n'));
    }

    #[tokio::test]
    async fn a_file_removed_after_the_announcement_becomes_the_notice_padded_to_length() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let id = files.write(&[7u8; 3000]);
        let parts = parts_for(&with_attachment(&id, Some(3000)), &[full()], "doc-removed");
        std::fs::remove_file(get_attachment_file_path(Some(&id))).unwrap();
        let (_, _, wire) = split_literal(&write_and_read(&parts).await);
        let text = String::from_utf8(wire).unwrap();
        let start = text.find(MARKER).unwrap() + MARKER.len();
        let notice = base64::engine::general_purpose::STANDARD.encode(MISSING_ATTACHMENT_NOTICE);
        assert_eq!(&text[start..start + notice.len()], notice);
        let end = start + text[start..].find("\r\n--").unwrap();
        assert_eq!(end - start, 4000);
        assert!(text[start + notice.len()..end].bytes().all(|byte| byte == b'\n'));
    }
}

// ---------------------------------------------------------------------------
// The streamed bytes are the materialized message's
// ---------------------------------------------------------------------------

mod base64_parity {
    use super::*;

    /// BODY[] streamed, against the same message built in memory, plus the
    /// CRLF a non-partial BODY[] literal ends with.
    async fn assert_matches_materialized(message: &FetchMailInput, doc_id: &str) -> Probe {
        let (wire, probe) = write_and_probe(&parts_for(message, &[full()], doc_id)).await;
        let (_, _, streamed) = split_literal(&wire);
        let materialized = build_full_message(message, Some(doc_id), &file_size, &read_file).unwrap();
        assert_eq!(String::from_utf8(streamed).unwrap(), format!("{materialized}\r\n"));
        probe
    }

    fn body_of(literal: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(literal);
        let start = text.find("base64\r\n\r\n").expect("a base64 body") + "base64\r\n\r\n".len();
        // As `Buffer.from(…, "base64")` reads it: line breaks skipped.
        let encoded: String = text[start..].chars().filter(|c| !c.is_ascii_whitespace()).collect();
        base64::engine::general_purpose::STANDARD.decode(encoded).unwrap()
    }

    #[tokio::test]
    async fn a_small_text_mail_is_byte_identical() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let message = mail(with(base(), json!({ "text": "Hello, streaming world!" })));
        assert_matches_materialized(&message, "small-text").await;
        let streamed = literal(&message, full(), "small-text").await;
        assert_eq!(body_of(&streamed), b"Hello, streaming world!");
    }

    #[tokio::test]
    async fn a_large_html_body_goes_out_in_slices_never_whole() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let html = format!("<p>{}</p>", "x".repeat(200 * 1024 - 8));
        let message = mail(with(base(), json!({ "text": "", "html": html })));
        let probe = assert_matches_materialized(&message, "big-html").await;
        assert!(probe.pushes.len() >= 4, "{:?}", probe.pushes);
        assert!(probe.pushes.iter().all(|&push| push < 80 * 1024), "{:?}", probe.pushes);
        let streamed = literal(&message, full(), "big-html").await;
        assert_eq!(String::from_utf8(body_of(&streamed)).unwrap(), html);
    }

    #[tokio::test]
    async fn a_body_whose_length_is_not_a_multiple_of_3_round_trips_across_slices() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let text = "a".repeat(100_003);
        let message = mail(with(base(), json!({ "text": text })));
        assert_matches_materialized(&message, "misaligned").await;
        assert_eq!(body_of(&literal(&message, full(), "misaligned").await), text.as_bytes());
    }

    #[tokio::test]
    async fn multi_byte_utf8_round_trips_across_slice_boundaries() {
        let _budget = crate::imap::body_budget::test_lock().await;
        for text in ["🚀".repeat(20_000), format!("{}🚀{}", "a".repeat(SLICE_RAW_BYTES - 2), "b".repeat(100))] {
            let message = mail(with(base(), json!({ "text": text })));
            let probe = assert_matches_materialized(&message, "emoji").await;
            assert!(probe.pushes.iter().all(|&push| push < 80 * 1024));
            let decoded = body_of(&literal(&message, full(), "emoji").await);
            assert_eq!(String::from_utf8(decoded).unwrap(), text);
        }
    }

    #[tokio::test]
    async fn an_empty_body_emits_no_base64() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let message = mail(with(base(), json!({ "text": "" })));
        assert_matches_materialized(&message, "empty-text").await;
        let streamed = String::from_utf8(literal(&message, full(), "empty-text").await).unwrap();
        assert!(streamed.contains("MIME-Version: 1.0"));
        let after = streamed.split("base64\r\n\r\n").nth(1);
        assert!(after.is_none_or(|rest| !rest.starts_with(|c: char| c.is_ascii_alphanumeric())));
    }

    #[tokio::test]
    async fn text_html_and_an_attachment_match_the_materialized_message() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let id = files.write(&patterned(150_001, 13, 256));
        let message = mail(with(
            base(),
            json!({ "text": "plain body text", "html": "<p>rich body</p>".repeat(3000), "attachments": [attachment(&id, Some(1))] }),
        ));
        assert_matches_materialized(&message, "parity-test").await;
    }
}

// ---------------------------------------------------------------------------
// Partial BODY[]<start.length>
// ---------------------------------------------------------------------------

mod partial {
    use super::*;

    fn small() -> FetchMailInput {
        mail(with(
            base(),
            json!({ "messageId": "<partial@local>", "subject": "partial", "text": "plain body content", "html": "<p>rich body content</p>" }),
        ))
    }

    #[tokio::test]
    async fn a_chunked_sequence_of_windows_emits_exactly_what_each_announces_and_reassembles_the_body() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let message = small();
        let whole = literal(&message, full(), "doc-chunked").await;
        let mut reassembled = Vec::new();
        let mut start = 0.0;
        loop {
            let wire = write_and_read(&parts_for(&message, &[body(BodySection::Full, Some((start, 250.0)))], "doc-chunked")).await;
            if !wire.contains(&b'{') {
                // NIL past the end.
                assert!(String::from_utf8_lossy(&wire).contains("NIL"));
                break;
            }
            let (before, length, chunk) = split_literal(&wire);
            assert!(before.contains(&format!("BODY[]<{start}>")), "{before}");
            reassembled.extend_from_slice(&chunk);
            if length < 250 {
                break;
            }
            start += 250.0;
        }
        // The partial range addresses the message without its closing CRLF.
        assert_eq!(reassembled, whole[..whole.len() - 2]);
    }

    #[tokio::test]
    async fn an_oversized_window_is_clamped_to_the_body() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let message = small();
        let whole = literal(&message, full(), "doc-clamp").await;
        let clamped = literal(&message, body(BodySection::Full, Some((0.0, 100_000_000.0))), "doc-clamp").await;
        assert_eq!(clamped, whole[..whole.len() - 2]);
    }

    fn mail_with(id: &str, size: u64) -> FetchMailInput {
        mail(with(
            base(),
            json!({
                "messageId": "<attachment-partial@local>",
                "date": "2026-10-28T00:00:00Z",
                "subject": "attachment partial",
                "attachments": [attachment(id, Some(size))]
            }),
        ))
    }

    async fn range_of(message: &FetchMailInput, section: &BodySection, window: Option<(u64, u64)>) -> Vec<u8> {
        let item = body(section.clone(), window.map(|(start, length)| (start as f64, length as f64)));
        literal(message, item, "doc-attachment-partial").await
    }

    #[tokio::test]
    async fn a_window_equals_the_same_range_of_the_whole_at_every_base64_alignment() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let original = patterned(200_000, 7, 253);
        let id = files.write(&original);
        let message = mail_with(&id, original.len() as u64);
        for (label, section) in [
            ("BODY[]", BodySection::Full),
            ("BODY[2]", BodySection::MimePart { part_number: "2".into(), sub_section: None }),
        ] {
            let whole = range_of(&message, &section, None).await;
            let addressable = range_of(&message, &section, Some((0, 1_000_000_000))).await.len() as i64;
            let encoded_start = if label == "BODY[]" {
                (String::from_utf8_lossy(&whole).find(MARKER).unwrap() + MARKER.len()) as i64
            } else {
                0
            };
            let starts = [
                0,
                1,
                2,
                3,
                encoded_start - 1,
                encoded_start,
                encoded_start + 1,
                encoded_start + 2,
                encoded_start + 3,
                encoded_start + 65_536,
                encoded_start + 65_537,
                encoded_start + 150_001,
                encoded_start + 266_665,
                addressable - 3,
                addressable - 1,
            ];
            for start in starts.into_iter().filter(|&start| start >= 0) {
                for length in [1i64, 5, 4_099, 65_537] {
                    let got = range_of(&message, &section, Some((start as u64, length as u64))).await;
                    let end = (start + length).min(addressable) as usize;
                    assert!(got == whole[start as usize..end], "{label}<{start}.{length}>");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_window_deep_in_the_attachment_never_reads_the_file_before_it() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let original = patterned(200_000, 7, 253);
        let id = files.write(&original);
        let message = mail_with(&id, original.len() as u64);
        let whole = range_of(&message, &BodySection::Full, None).await;
        let encoded_start = (String::from_utf8_lossy(&whole).find(MARKER).unwrap() + MARKER.len()) as u64;
        let skip = 200_001u64;
        let item = body(BodySection::Full, Some(((encoded_start + skip) as f64, 1_000.0)));
        let (wire, probe) = write_and_probe(&parts_for(&message, &[item], "doc-attachment-partial")).await;
        let (_, _, got) = split_literal(&wire);
        let from = (encoded_start + skip) as usize;
        assert_eq!(got, whole[from..from + 1_000]);
        assert_eq!(probe.attachment_reads_from, [skip / 4 * 4 * 3 / 4]);
    }

    #[tokio::test]
    async fn a_missing_attachment_file_yields_the_same_bytes_partially_as_in_full() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let mut files = Files::default();
        let id = files.missing();
        let message = mail_with(&id, 90_000);
        let whole = range_of(&message, &BodySection::Full, None).await;
        let addressable = range_of(&message, &BodySection::Full, Some((0, 1_000_000_000))).await.len();
        let encoded_start = String::from_utf8_lossy(&whole).find(MARKER).unwrap() + MARKER.len();
        for offset in [0, 1, 2, 3, 5, 33, 35] {
            let start = encoded_start + offset;
            let got = range_of(&message, &BodySection::Full, Some((start as u64, 8))).await;
            assert_eq!(got, whole[start..(start + 8).min(addressable)], "offset {offset}");
        }
    }
}

// ---------------------------------------------------------------------------
// The stream lock and the body budget
// ---------------------------------------------------------------------------

mod slots {
    use super::*;

    fn budget_mail() -> FetchMailInput {
        mail(with(
            base(),
            json!({ "messageId": "<budget-yield@local>", "subject": "budget yield", "text": "plain body", "html": "<p>rich body</p>" }),
        ))
    }

    fn stream_key_of(parts: &[FetchResponsePart]) -> String {
        parts
            .iter()
            .find_map(|part| match part {
                FetchResponsePart::Stream(stream) => Some(stream.stream_key.clone()),
                _ => None,
            })
            .expect("a stream part")
    }

    #[tokio::test]
    async fn a_socket_already_gone_takes_no_slot_and_no_lock() {
        let _budget = crate::imap::body_budget::test_lock().await;
        reset_body_budget();
        let held = fill_body_budget().await;
        let (socket, _read, _client) = SocketHandle::for_test("203.0.113.7");
        let session = ImapSession::new(socket, false);
        session.socket().destroy();
        let parts = parts_for(&budget_mail(), &[full()], "doc-gone");
        let locks_before = stream_mutex_held_count();
        // With every slot taken, waiting for one would hang here.
        tokio::time::timeout(Duration::from_secs(1), write_fetch_response(&session, 1, &parts))
            .await
            .expect("no wait for a slot")
            .unwrap();
        assert_eq!(stream_mutex_held_count(), locks_before);
        drop(held);
    }

    #[tokio::test]
    async fn a_peer_that_goes_away_mid_body_gives_the_slot_and_the_lock_back() {
        let _budget = crate::imap::body_budget::test_lock().await;
        reset_body_budget();
        let mut files = Files::default();
        let id = files.write(&patterned(4 * 1024 * 1024, 31, 256));
        let parts = parts_for(&with_attachment(&id, None), &[full()], "doc-peer-gone");
        let key = stream_key_of(&parts);

        let (socket, read, mut client) = SocketHandle::for_test("203.0.113.7");
        let session = ImapSession::new(socket, false);
        let reading = async move {
            let mut first = vec![0u8; 1000];
            client.read_exact(&mut first).await.unwrap();
            drop(client);
            drop(read);
        };
        let writing = tokio::time::timeout(Duration::from_secs(10), write_fetch_response(&session, 1, &parts));
        let (written, ()) = tokio::join!(writing, reading);
        written.expect("the write ends once the peer is gone").unwrap();

        assert_eq!(crate::imap::stream_mutex::stream_mutex_waiters_for(&key), 0);
        let _again = tokio::time::timeout(Duration::from_secs(1), acquire_stream_mutex(&key)).await.expect("the key is free");
        drop(fill_body_budget().await);
    }

    // command-budget-wiring.test.ts, block 2: the command slot is given up
    // only while the fetch waits on another limit, and held for every chunk.

    #[tokio::test]
    async fn the_command_slot_is_released_while_queued_on_the_body_budget_and_held_for_the_drain() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let _commands = crate::imap::command_budget::test_lock().await;
        reset_body_budget();
        reset_command_budget();
        reset_stream_mutex();

        let body_holders = fill_body_budget().await;
        acquire_command_budget().await;
        assert_eq!(command_budget_in_flight(), 1);
        let hold = create_command_budget_hold(true, Duration::ZERO);

        let parts = parts_for(&budget_mail(), &[full()], "doc-command-budget-yield");
        let fetching = tokio::spawn(PROBE.scope(
            RefCell::new(Probe::default()),
            run_in_command_budget_context(hold.clone(), async move {
                let received = write_and_read(&parts).await;
                (received, PROBE.with(|probe| std::mem::take(&mut *probe.borrow_mut())))
            }),
        ));

        // Queued behind the body budget: the slot is back in the pool.
        settle().await;
        assert_eq!(command_budget_in_flight(), 0);
        assert!(!hold.held());

        drop(body_holders);
        let (received, probe) = fetching.await.unwrap();
        split_literal(&received);
        assert!(!probe.command_slots.is_empty());
        assert!(probe.command_slots.iter().all(|&slots| slots == 1), "{:?}", probe.command_slots);
        assert!(hold.held());
        assert_eq!(command_budget_in_flight(), 1);
        release_command_budget();
        reset_command_budget();
    }

    #[tokio::test]
    async fn the_command_slot_is_released_while_queued_on_the_stream_lock_for_the_same_body() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let _commands = crate::imap::command_budget::test_lock().await;
        reset_body_budget();
        reset_command_budget();
        reset_stream_mutex();

        let parts = parts_for(&budget_mail(), &[full()], "doc-command-budget-mutex");
        // Another session is streaming this exact body and owns its key.
        let other = acquire_stream_mutex(&stream_key_of(&parts)).await;

        acquire_command_budget().await;
        let hold = create_command_budget_hold(true, Duration::ZERO);
        let fetching = tokio::spawn(PROBE.scope(
            RefCell::new(Probe::default()),
            run_in_command_budget_context(hold.clone(), async move {
                let received = write_and_read(&parts).await;
                (received, PROBE.with(|probe| std::mem::take(&mut *probe.borrow_mut())))
            }),
        ));

        settle().await;
        assert_eq!(command_budget_in_flight(), 0);
        assert!(!hold.held());

        drop(other);
        let (received, probe) = fetching.await.unwrap();
        split_literal(&received);
        assert!(!probe.command_slots.is_empty());
        assert!(probe.command_slots.iter().all(|&slots| slots == 1), "{:?}", probe.command_slots);
        assert!(hold.held());
        assert_eq!(command_budget_in_flight(), 1);
        release_command_budget();
        reset_command_budget();
    }

    #[tokio::test]
    async fn the_command_slot_is_kept_through_an_uncontended_body_budget_and_stream_lock() {
        let _budget = crate::imap::body_budget::test_lock().await;
        let _commands = crate::imap::command_budget::test_lock().await;
        reset_body_budget();
        reset_command_budget();
        reset_stream_mutex();

        // The command budget is full and another command waits behind it:
        // any slot this fetch gave up would go to that waiter.
        let capacity = command_budget_capacity();
        for _ in 0..capacity {
            acquire_command_budget().await;
        }
        let intruder = tokio::spawn(async {
            acquire_command_budget().await;
        });
        settle().await;
        assert!(!intruder.is_finished());

        let hold = create_command_budget_hold(true, Duration::ZERO);
        let parts = parts_for(&budget_mail(), &[full()], "doc-command-budget-uncontended");
        let fetching = PROBE.scope(
            RefCell::new(Probe::default()),
            run_in_command_budget_context(hold.clone(), async {
                let received = write_and_read(&parts).await;
                (received, PROBE.with(|probe| std::mem::take(&mut *probe.borrow_mut())))
            }),
        );
        let (received, probe) =
            tokio::time::timeout(Duration::from_secs(1), fetching).await.expect("drained, not stalled");
        split_literal(&received);

        assert!(!intruder.is_finished());
        assert!(hold.held());
        assert_eq!(hold.waited_ms(), 0.0);
        assert!(!probe.command_slots.is_empty());
        assert!(probe.command_slots.iter().all(|&slots| slots == capacity), "{:?}", probe.command_slots);

        for _ in 0..capacity {
            release_command_budget();
        }
        intruder.await.unwrap();
        release_command_budget();
        reset_command_budget();
    }
}

#[test]
fn the_attachment_folder_is_the_servers() {
    assert_eq!(Path::new(&get_attachment_file_path(Some("x"))).parent(), Some(Path::new(ATTACHMENT_FOLDER)));
}
