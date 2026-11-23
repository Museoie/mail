//! The Bun server's `session-utils*.test.ts` and `fetch-helpers*.test.ts`,
//! for the functions the port covers.
//!
//! The Bun tests drain a response part's stream and compare the octets. The
//! port describes a streamed part instead of producing it, so these emit the
//! description the way the session code is meant to (`support::stream_bytes`)
//! and compare those octets. Tests of the stream lock, the body budget and
//! the database reader belong with the session code and are not here.

use base64::Engine;
use mail_server::imap::fetch_format::*;
use mail_server::imap::session_format::*;
use mail_server::imap::types::*;
use mail_server::imap::util::{encode_text, format_envelope};
use serde_json::{json, Value};

use crate::support::{mail, segment_bytes, stream_bytes, AttachmentFiles};

fn b64(bytes: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn none() -> AttachmentFiles {
    AttachmentFiles::default()
}

fn body_fetch(section: BodySection, partial: Option<(f64, f64)>) -> BodyFetch {
    BodyFetch { peek: true, section, partial: partial.map(|(start, length)| PartialRange { start, length }) }
}

fn part_number(number: &str, sub_section: Option<MimeSubSection>) -> BodySection {
    BodySection::MimePart { part_number: number.to_string(), sub_section }
}

fn header_fields(not: bool, fields: &[&str]) -> BodySection {
    BodySection::HeaderFields { not, fields: fields.iter().map(|field| field.to_string()).collect() }
}

fn body_part(mail: &FetchMailInput, fetch: &BodyFetch, doc_id: &str, files: &AttachmentFiles) -> Option<FetchResponsePart> {
    build_body_response_part(mail, fetch, doc_id, None, &|id| files.size(id))
}

fn item_part(mail: &FetchMailInput, item: &FetchDataItem, mailbox: &str, files: &AttachmentFiles) -> Option<FetchResponsePart> {
    build_fetch_response_part(mail, item, "doc-1", mailbox, &|id| files.size(id)).part
}

/// The label, the announced length and the octets of a literal or stream part.
fn emitted(part: &FetchResponsePart, files: &AttachmentFiles) -> (String, u64, Vec<u8>) {
    match part {
        FetchResponsePart::Literal { content, header, length } => (header.clone(), *length, content.as_bytes().to_vec()),
        FetchResponsePart::Stream(stream) => {
            (stream.header.clone(), stream.length, stream_bytes(stream, files).expect("a body the database holds"))
        }
        FetchResponsePart::Simple { content } => panic!("expected a literal or a stream, got {content}"),
    }
}

fn simple(part: Option<FetchResponsePart>) -> String {
    match part {
        Some(FetchResponsePart::Simple { content }) => content,
        other => panic!("expected a simple part, got {other:?}"),
    }
}

fn full_message(value: Value, doc_id: Option<&str>, files: &AttachmentFiles) -> String {
    build_full_message(&mail(value), doc_id, &|id| files.size(id), &|id| files.read(id)).unwrap()
}

/// Every segment of a message emitted in order.
fn streamed(value: Value, doc_id: &str) -> Vec<u8> {
    let files = none();
    build_message_segments(&mail(value), Some(doc_id), &|id| files.size(id))
        .iter()
        .flat_map(|segment| segment_bytes(segment, &files).unwrap())
        .collect()
}

/// The base64 body of a single-part message on the wire.
fn wire_body(wire: &[u8]) -> String {
    let wire = std::str::from_utf8(wire).unwrap();
    let start = wire.find("base64\r\n\r\n").expect("a base64 body") + "base64\r\n\r\n".len();
    wire[start..].strip_suffix("\r\n").expect("a trailing CRLF").to_string()
}

fn decoded(body: &str) -> String {
    String::from_utf8(base64::engine::general_purpose::STANDARD.decode(body).unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// session-utils.test.ts
// ---------------------------------------------------------------------------

mod apply_partial_fetch {
    use super::*;

    fn slice(content: &str, start: f64, length: f64) -> String {
        apply_partial_fetch(content, &PartialRange { start, length })
    }

    #[test]
    fn returns_empty_when_start_is_at_or_beyond_content_length() {
        assert_eq!(slice("Hello", 100.0, 5.0), "");
        assert_eq!(slice("Hello", 5.0, 5.0), "");
    }

    #[test]
    fn slices_from_the_middle_and_from_the_start() {
        assert_eq!(slice("Hello, World!", 7.0, 5.0), "World");
        assert_eq!(slice("Hello, World!", 0.0, 5.0), "Hello");
    }

    #[test]
    fn clamps_to_end_of_content() {
        assert_eq!(slice("Hello", 3.0, 100.0), "lo");
        assert_eq!(slice("Hello", 3.0, 2.0), "lo");
    }

    #[test]
    fn counts_byte_offsets_not_char_offsets() {
        assert_eq!("日本語".len(), 9);
        assert_eq!(slice("日本語", 3.0, 3.0), "本");
    }

    #[test]
    fn returns_empty_for_zero_length_content() {
        assert_eq!(slice("", 0.0, 5.0), "");
    }
}

mod body_section_key {
    use super::*;

    #[test]
    fn names_the_whole_message_sections() {
        assert_eq!(get_body_section_key(&BodySection::Full), "BODY[]");
        assert_eq!(get_body_section_key(&BodySection::Text), "BODY[TEXT]");
        assert_eq!(get_body_section_key(&BodySection::Header), "BODY[HEADER]");
    }

    #[test]
    fn names_mime_parts() {
        assert_eq!(get_body_section_key(&part_number("1", None)), "BODY[1]");
        assert_eq!(get_body_section_key(&part_number("1.2.3", None)), "BODY[1.2.3]");
    }

    #[test]
    fn names_mime_part_sub_sections() {
        assert_eq!(get_body_section_key(&part_number("1", Some(MimeSubSection::Header))), "BODY[1.HEADER]");
        assert_eq!(get_body_section_key(&part_number("2", Some(MimeSubSection::Mime))), "BODY[2.MIME]");
        assert_eq!(get_body_section_key(&part_number("1.2", Some(MimeSubSection::Text))), "BODY[1.2.TEXT]");
    }

    #[test]
    fn names_header_fields_sections() {
        assert_eq!(get_body_section_key(&header_fields(false, &["FROM", "TO"])), "BODY[HEADER.FIELDS (FROM TO)]");
        assert_eq!(get_body_section_key(&header_fields(true, &["FROM", "TO"])), "BODY[HEADER.FIELDS.NOT (FROM TO)]");
        assert_eq!(get_body_section_key(&header_fields(false, &["SUBJECT"])), "BODY[HEADER.FIELDS (SUBJECT)]");
    }
}

mod mark_as_read {
    use super::*;

    fn body(peek: bool) -> FetchDataItem {
        FetchDataItem::Body(BodyFetch { peek, section: BodySection::Full, partial: None })
    }

    #[test]
    fn is_false_without_a_content_fetch() {
        assert!(!should_mark_as_read(&[]));
        assert!(!should_mark_as_read(&[FetchDataItem::Flags, FetchDataItem::Uid, FetchDataItem::Envelope]));
    }

    #[test]
    fn is_false_for_the_bare_body_structure_item() {
        assert!(!should_mark_as_read(&[FetchDataItem::BodyStructure { extensible: false }]));
    }

    #[test]
    fn follows_the_peek_flag_of_body_items() {
        assert!(!should_mark_as_read(&[body(true)]));
        assert!(should_mark_as_read(&[body(false)]));
        assert!(should_mark_as_read(&[FetchDataItem::Flags, body(false), FetchDataItem::Uid]));
        assert!(!should_mark_as_read(&[body(true), body(true)]));
        assert!(should_mark_as_read(&[body(true), body(false), body(true)]));
    }

    #[test]
    fn treats_the_rfc822_items_as_their_body_equivalents() {
        assert!(should_mark_as_read(&[FetchDataItem::Rfc822]));
        assert!(should_mark_as_read(&[FetchDataItem::Rfc822Text]));
        assert!(!should_mark_as_read(&[FetchDataItem::Rfc822Header]));
        assert!(!should_mark_as_read(&[FetchDataItem::Rfc822Size]));
    }
}

mod full_message {
    use super::*;

    fn attachment(id: &str, content_type: &str, filename: &str, size: usize) -> Value {
        json!({ "content": { "data": id }, "contentType": content_type, "filename": filename, "size": size })
    }

    #[test]
    fn returns_headers_and_an_empty_body_for_mail_with_no_content() {
        let result = full_message(json!({}), None, &none());
        assert!(result.contains("MIME-Version: 1.0"));
        assert!(result.ends_with("\r\n\r\n"));
    }

    #[test]
    fn returns_headers_and_encoded_text_for_plain_text_only_mail() {
        let result = full_message(json!({ "text": "Hello, World!" }), None, &none());
        assert!(result.contains("MIME-Version: 1.0") && result.contains("text/plain"));
        assert!(result.contains(&b64("Hello, World!")));
    }

    #[test]
    fn returns_headers_and_encoded_html_for_html_only_mail() {
        let result = full_message(json!({ "html": "<p>Hello</p>" }), None, &none());
        assert!(result.contains("text/html"));
        assert!(result.contains(&b64("<p>Hello</p>")));
    }

    #[test]
    fn returns_multipart_alternative_for_text_and_html() {
        let result = full_message(json!({ "text": "Hello plain", "html": "<p>Hello HTML</p>" }), Some("test-doc-123"), &none());
        for expected in [
            "multipart/alternative",
            "boundary_test-doc-123",
            "Content-Type: text/plain; charset=utf-8",
            "Content-Type: text/html; charset=utf-8",
            "Content-Transfer-Encoding: base64",
            "--boundary_test-doc-123--",
        ] {
            assert!(result.contains(expected), "{expected}");
        }
        assert!(result.contains(&b64("Hello plain")) && result.contains(&b64("<p>Hello HTML</p>")));
    }

    #[test]
    fn returns_multipart_mixed_for_text_html_and_attachment() {
        let data = b"PDF_BINARY_DATA";
        let files = none().with("att-file-id-1", data);
        let result = full_message(
            json!({ "text": "See attached", "html": "<p>See attached</p>", "attachments": [attachment("att-file-id-1", "application/pdf", "document.pdf", data.len())] }),
            Some("doc-mixed-1"),
            &files,
        );
        for expected in ["multipart/mixed", "Content-Type: application/pdf", "filename=\"document.pdf\"", "Content-Disposition: attachment", "--boundary_doc-mixed-1--"] {
            assert!(result.contains(expected), "{expected}");
        }
        assert!(result.contains(&b64(data)));
    }

    #[test]
    fn returns_multipart_mixed_for_text_only_and_attachment() {
        let data = b"SOME_DATA";
        let files = none().with("att-file-id-2", data);
        let result = full_message(
            json!({ "text": "See attached", "attachments": [attachment("att-file-id-2", "text/plain", "notes.txt", data.len())] }),
            Some("doc-mixed-2"),
            &files,
        );
        assert!(result.contains("multipart/mixed") && result.contains("Content-Type: text/plain"));
        assert!(result.contains("filename=\"notes.txt\"") && result.contains(&b64(data)));
    }

    #[test]
    fn uses_the_message_id_when_the_document_id_is_missing() {
        let result = full_message(json!({ "text": "Hello", "html": "<p>Hello</p>", "messageId": "<test-msg-id@example.com>" }), None, &none());
        assert!(result.contains("multipart/alternative"));
        assert!(result.contains("--boundary__test-msg-id_example.com_--"));
    }

    #[test]
    fn uses_crlf_line_endings_throughout() {
        assert!(full_message(json!({ "text": "Hello" }), None, &none()).split("\r\n").count() > 1);
    }

    #[test]
    fn a_subject_carrying_a_boundary_parameter_does_not_become_the_boundary() {
        let result = full_message(
            json!({ "subject": "winter sale boundary=\"hijacked\"", "text": "Hello", "html": "<p>Hello</p>" }),
            Some("doc-hijack"),
            &none(),
        );
        assert!(result.contains("Content-Type: multipart/alternative; boundary=\"boundary_doc-hijack\""));
        assert!(result.contains("--boundary_doc-hijack\r\n") && result.contains("--boundary_doc-hijack--"));
        assert!(!result.contains("--hijacked"));
    }

    #[test]
    fn a_subject_carrying_the_text_content_type_is_not_rewritten() {
        let result = full_message(
            json!({ "subject": "Content-Type: text/plain; boundary=\"evil\"", "text": "Hello", "html": "<p>Hello</p>" }),
            Some("docA"),
            &none(),
        );
        assert!(result.contains("Subject: Content-Type: text/plain; boundary=\"evil\"\r\n"));
        assert!(result.contains("Content-Type: multipart/alternative; boundary=\"boundary_docA\"\r\n"));
        assert!(!result.contains("--evil"));
    }

    #[test]
    fn a_subject_carrying_a_line_separator_before_content_type_is_not_rewritten() {
        let result = full_message(
            json!({ "subject": "Hi\u{2028}Content-Type: text/plain; boundary=\"evil\"", "text": "Hello", "html": "<p>Hello</p>" }),
            Some("docU"),
            &none(),
        );
        assert!(result.contains("Subject: Hi Content-Type: text/plain; boundary=\"evil\"\r\n"));
        assert!(result.contains("Content-Type: multipart/alternative; boundary=\"boundary_docU\"\r\n"));
        assert!(!result.contains('\u{2028}') && !result.contains("--evil"));
    }

    #[test]
    fn an_attachment_with_no_content_type_or_filename_still_serializes() {
        let files = none().with("att-bare", b"DATA");
        let result = full_message(json!({ "text": "hi", "attachments": [{ "content": { "data": "att-bare" }, "size": 4 }] }), Some("docC"), &files);
        assert!(result.contains("Content-Type: application/octet-stream\r\n"));
        assert!(result.contains("filename=\"unnamed\"\r\n"));
        assert!(!result.contains("undefined"));
    }

    #[test]
    fn an_attachment_filename_cannot_open_a_new_parameter_or_header() {
        let files = none().with("att-inject", b"DATA");
        let result = full_message(
            json!({ "text": "See attached", "attachments": [attachment("att-inject", "text/plain\r\nX-Evil: 1", "safe.txt\"; filename=\"payroll.pdf", 4)] }),
            Some("doc-att-inject"),
            &files,
        );
        assert!(result.contains("Content-Type: text/plain X-Evil: 1\r\n"));
        assert!(result.contains("Content-Disposition: attachment; filename=\"safe.txt\\\"; filename=\\\"payroll.pdf\"\r\n"));
        assert!(!result.split("\r\n").any(|line| line.starts_with("X-Evil:")));
    }

    #[test]
    fn a_row_projected_without_its_body_strings_cannot_be_built_in_hand() {
        let lazy = mail(json!({ "text_octets": 5, "html_octets": 0, "mail_id": "m", "user_id": "u" }));
        let error = build_full_message(&lazy, Some("d"), &|_| None, &|_| None).unwrap_err();
        assert!(error.to_string().starts_with("buildFullMessage: cannot materialize a lazy-text segment."));
    }
}

mod body_parts {
    use super::*;

    const TEXT_HDR: &str = "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64";
    const HTML_HDR: &str = "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64";

    fn part(value: Value, number: &str, files: &AttachmentFiles) -> Option<String> {
        get_body_part(&mail(value), number, &|id| files.read(id))
    }

    fn headers(value: Value, number: &str) -> Option<String> {
        get_body_part_headers(&mail(value), number)
    }

    fn with_attachment(body: Value, id: &str, content_type: &str, filename: &str) -> Value {
        let mut value = body;
        value["attachments"] = json!([{ "content": { "data": id }, "contentType": content_type, "filename": filename, "size": 100 }]);
        value
    }

    #[test]
    fn part_is_none_for_empty_mail() {
        assert_eq!(part(json!({}), "1", &none()), None);
    }

    #[test]
    fn part_1_is_the_only_body_of_a_single_part_mail() {
        assert_eq!(part(json!({ "text": "Hello plain" }), "1", &none()), Some(b64("Hello plain")));
        assert_eq!(part(json!({ "text": "Hello plain" }), "2", &none()), None);
        assert_eq!(part(json!({ "html": "<p>Hello</p>" }), "1", &none()), Some(b64("<p>Hello</p>")));
    }

    #[test]
    fn parts_1_and_2_are_text_and_html_of_an_alternative() {
        let both = json!({ "text": "Plain text", "html": "<p>HTML</p>" });
        assert_eq!(part(both.clone(), "1", &none()), Some(b64("Plain text")));
        assert_eq!(part(both.clone(), "2", &none()), Some(b64("<p>HTML</p>")));
        assert_eq!(part(both, "3", &none()), None);
    }

    #[test]
    fn parts_1_1_and_1_2_are_the_alternative_nested_in_a_mixed_mail() {
        let value = with_attachment(json!({ "text": "Body text", "html": "<p>Body HTML</p>" }), "att-1", "application/pdf", "doc.pdf");
        assert_eq!(part(value.clone(), "1.1", &none()), Some(b64("Body text")));
        assert_eq!(part(value, "1.2", &none()), Some(b64("<p>Body HTML</p>")));
    }

    #[test]
    fn part_2_is_the_attachment_of_a_mail_with_one() {
        let value = with_attachment(json!({ "text": "Body" }), "att-file-xyz", "image/png", "photo.png");
        let files = none().with("att-file-xyz", b"ATTACHMENT_BYTES");
        assert_eq!(part(value.clone(), "2", &files), Some(b64("ATTACHMENT_BYTES")));
        // A missing file, and an index past the last attachment.
        assert_eq!(part(value.clone(), "2", &none()), None);
        assert_eq!(part(value, "3", &files), None);
    }

    #[test]
    fn headers_are_none_for_empty_mail() {
        assert_eq!(headers(json!({}), "1"), None);
    }

    #[test]
    fn headers_of_single_part_mails() {
        assert_eq!(headers(json!({ "text": "Hello" }), "1").as_deref(), Some(TEXT_HDR));
        assert_eq!(headers(json!({ "html": "<p>Hi</p>" }), "1").as_deref(), Some(HTML_HDR));
    }

    #[test]
    fn headers_of_each_part_of_an_alternative() {
        let both = json!({ "text": "Plain", "html": "<p>H</p>" });
        assert_eq!(headers(both.clone(), "1").as_deref(), Some(TEXT_HDR));
        assert_eq!(headers(both.clone(), "2").as_deref(), Some(HTML_HDR));
        assert_eq!(headers(both, "3"), None);
    }

    #[test]
    fn headers_of_the_nested_parts_of_a_mixed_mail() {
        let value = with_attachment(json!({ "text": "Body", "html": "<p>Body</p>" }), "att-1", "application/pdf", "doc.pdf");
        assert_eq!(headers(value.clone(), "1.1").as_deref(), Some(TEXT_HDR));
        assert_eq!(headers(value, "1.2").as_deref(), Some(HTML_HDR));
    }

    #[test]
    fn headers_of_an_attachment_carry_its_disposition() {
        let value = with_attachment(json!({ "text": "Body" }), "att-file", "image/png", "photo.png");
        assert_eq!(
            headers(value.clone(), "2").as_deref(),
            Some("Content-Type: image/png\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"photo.png\"")
        );
        assert_eq!(headers(value, "3"), None);
    }

    #[test]
    fn headers_escape_a_hostile_attachment_content_type_and_filename() {
        let value = with_attachment(json!({ "text": "Body" }), "att-file", "image/png\r\nX-Evil: 1", "photo.png\"; filename=\"payroll.pdf");
        assert_eq!(
            headers(value, "2").as_deref(),
            Some("Content-Type: image/png X-Evil: 1\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"photo.png\\\"; filename=\\\"payroll.pdf\"")
        );
    }

    #[test]
    fn headers_default_a_missing_content_type_and_filename() {
        let value = json!({ "text": "Body", "attachments": [{ "content": { "data": "att-bare" }, "size": 10 }] });
        assert_eq!(
            headers(value, "2").as_deref(),
            Some("Content-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"unnamed\"")
        );
    }
}

mod base64_emission {
    use super::*;

    /// A source fed to the chunked encoder in the pieces given.
    fn encode_pieces(pieces: &[&[u8]]) -> String {
        let mut encoder = Base64Encoder::default();
        let mut out = String::new();
        for piece in pieces {
            out.push_str(&encoder.push(piece));
        }
        out.push_str(&encoder.finish());
        out
    }

    fn encode_split(source: &str, parts: usize) -> String {
        let size = source.len().div_ceil(parts).max(1);
        let pieces: Vec<&[u8]> = source.as_bytes().chunks(size).collect();
        encode_pieces(&pieces)
    }

    #[test]
    fn a_small_text_mail_carries_the_base64_of_its_source() {
        let wire = streamed(json!({ "text": "Hello, streaming world!" }), "small-text");
        assert!(String::from_utf8(wire).unwrap().contains(&b64("Hello, streaming world!")));
    }

    #[test]
    fn a_large_body_is_emitted_in_several_chunks_of_bounded_size() {
        let html = format!("<p>{}</p>", "x".repeat(200 * 1024 - 8));
        let chunks: Vec<String> = base64_chunks(&html).collect();
        assert!(chunks.len() >= 4);
        assert!(chunks.iter().all(|chunk| chunk.len() < 80 * 1024));
        assert_eq!(decoded(&wire_body(&streamed(json!({ "html": html }), "big-html"))), html);
    }

    #[test]
    fn a_body_whose_length_is_not_a_multiple_of_3_round_trips_across_slices() {
        let text = "a".repeat(100003);
        assert_eq!(decoded(&wire_body(&streamed(json!({ "text": text }), "misaligned"))), text);
    }

    #[test]
    fn multi_byte_utf8_round_trips_across_slice_boundaries() {
        let emoji = "🚀".repeat(20000);
        assert!(base64_chunks(&emoji).all(|chunk| chunk.len() < 80 * 1024));
        assert_eq!(decoded(&wire_body(&streamed(json!({ "text": emoji }), "emoji"))), emoji);
    }

    #[test]
    fn a_character_straddling_a_slice_boundary_stays_whole() {
        let text = format!("{}🚀{}", "a".repeat(SLICE_RAW_BYTES - 1), "b".repeat(100));
        let body = wire_body(&streamed(json!({ "text": text }), "surrogate-split"));
        let round_trip = decoded(&body);
        assert_eq!(round_trip, text);
        assert!(!round_trip.contains('\u{fffd}'));
        assert_eq!(body.len() as u64, base64_byte_len(text.len() as u64));
    }

    #[test]
    fn an_empty_body_yields_no_base64_output() {
        let wire = String::from_utf8(streamed(json!({ "text": "" }), "empty-text")).unwrap();
        assert!(wire.contains("MIME-Version: 1.0"));
        assert!(wire.ends_with("base64\r\n\r\n"));
        assert_eq!(base64_chunks("").count(), 0);
    }

    #[test]
    fn split_input_matches_whole_input() {
        let source = format!("{}-END-", "a".repeat(100_000));
        let whole: String = base64_chunks(&source).collect();
        assert_eq!(whole, b64(&source));
        for parts in [1, 2, 3, 10, 100, 1000] {
            assert_eq!(encode_split(&source, parts), whole, "{parts} parts");
        }
    }

    #[test]
    fn split_input_matches_when_a_multi_byte_sequence_straddles_a_chunk_boundary() {
        let source = format!("{}🚀{}", "a".repeat(5000), "b".repeat(5000));
        let whole = b64(&source);
        let bytes = source.as_bytes();
        // Before the character, and through the middle of it.
        for at in [5000, 5001, 5002, 5003] {
            assert_eq!(encode_pieces(&[&bytes[..at], &bytes[at..]]), whole);
        }
    }

    #[test]
    fn split_input_matches_for_a_source_split_at_every_octet() {
        let source = format!("{}🚀 café ☕ 日本語 😀{}", "a".repeat(500), "b".repeat(500));
        let pieces: Vec<&[u8]> = source.as_bytes().chunks(1).collect();
        let out = encode_pieces(&pieces);
        assert_eq!(out, b64(&source));
        assert_eq!(decoded(&out), source);
    }

    #[test]
    fn an_empty_source_yields_nothing() {
        assert_eq!(encode_pieces(&[]), "");
        assert_eq!(encode_pieces(&[b"", b"", b""]), "");
    }

    #[test]
    fn split_at_every_point_across_a_slice_boundary() {
        let source = format!("{}🚀{}", "x".repeat(SLICE_RAW_BYTES + 100), "y".repeat(100));
        let whole = b64(&source);
        let bytes = source.as_bytes();
        for at in [1, 100, SLICE_RAW_BYTES - 1, SLICE_RAW_BYTES, SLICE_RAW_BYTES + 1, SLICE_RAW_BYTES + 100, SLICE_RAW_BYTES + 101, SLICE_RAW_BYTES + 102, bytes.len() - 1] {
            assert_eq!(encode_pieces(&[&bytes[..at], &bytes[at..]]), whole, "split at {at}");
        }
    }

    #[test]
    fn the_emitted_segments_equal_the_message_built_in_hand() {
        let value = json!({ "text": "plain body text", "html": "<p>rich body</p>".repeat(3000) });
        let wire = streamed(value.clone(), "parity-test");
        assert_eq!(String::from_utf8(wire).unwrap(), full_message(value, Some("parity-test"), &none()));
    }

    #[test]
    fn a_literal_is_emitted_in_bounded_chunks() {
        let value = "h".repeat(LITERAL_CHUNK_BYTES * 2 + 5);
        let chunks: Vec<&[u8]> = literal_chunks(&value).collect();
        assert_eq!(chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(), [LITERAL_CHUNK_BYTES, LITERAL_CHUNK_BYTES, 5]);
    }
}

// ---------------------------------------------------------------------------
// session-utils-lazy-text.test.ts and fetch-helpers-lazy-body.test.ts: what
// they assert about sizes and ranges, which needs no database.
// ---------------------------------------------------------------------------

mod lazy_bodies {
    use super::*;

    fn lazy(text_octets: u64, html_octets: u64) -> Value {
        json!({ "text_octets": text_octets, "html_octets": html_octets, "mail_id": "mail-1", "user_id": "user-1", "subject": "lazy" })
    }

    fn segments(value: Value) -> Vec<MessageSegment> {
        build_message_segments(&mail(value), Some("doc-lazy"), &|_| None)
    }

    #[test]
    fn a_lazy_segment_measures_as_the_base64_of_its_octet_length() {
        for octets in [0u64, 1, 2, 3, 4, 100, 12_000, 500_000, 11_138_020] {
            let segment = MessageSegment::LazyText {
                source: BodySource::Html,
                mail_id: "m".into(),
                user_id: "u".into(),
                byte_length: octets,
                part_path: Some("1".into()),
            };
            assert_eq!(segment_byte_length(&segment), octets.div_ceil(3) * 4);
        }
    }

    #[test]
    fn a_lazy_mail_is_measured_without_its_body() {
        let list = segments(lazy(0, 500_000));
        assert!(matches!(&list[1], MessageSegment::LazyText { source: BodySource::Html, byte_length: 500_000, .. }));
        let headers = segment_byte_length(&list[0]);
        assert_eq!(sum_segment_bytes(&list), headers + base64_byte_len(500_000) + 2 + WIRE_TRAILER);
        assert_eq!(compute_full_message_size(&mail(lazy(0, 500_000)), Some("doc-lazy"), &|_| None), sum_segment_bytes(&list));
    }

    #[test]
    fn lazy_and_loaded_rows_of_the_same_mail_have_the_same_segments_and_size() {
        let text = "plain text body";
        let html = "<p>html body</p>";
        let loaded = segments(json!({ "text": text, "html": html, "subject": "lazy" }));
        let projected = segments(lazy(text.len() as u64, html.len() as u64));
        assert_eq!(loaded.len(), projected.len());
        assert_eq!(sum_segment_bytes(&loaded), sum_segment_bytes(&projected));
        for (a, b) in loaded.iter().zip(&projected) {
            assert_eq!(segment_byte_length(a), segment_byte_length(b));
            assert_eq!(a.part_path(), b.part_path());
        }
    }

    #[test]
    fn a_lazy_mail_with_no_octets_is_headers_only_like_an_empty_loaded_mail() {
        let projected = segments(lazy(0, 0));
        let loaded = segments(json!({ "text": "", "html": "", "subject": "lazy" }));
        assert_eq!(projected, loaded);
        assert_eq!(projected.len(), 1);
    }

    #[test]
    fn a_range_of_a_segment_list_is_that_slice_of_its_concatenation() {
        let files = none();
        let list = build_message_segments(&mail(json!({ "text": "text body ".repeat(40), "html": "<p>html</p>".repeat(40) })), Some("doc-range"), &|_| None);
        let whole: Vec<u8> = list.iter().flat_map(|segment| segment_bytes(segment, &files).unwrap()).collect();
        let total = whole.len() as u64;
        for (start, length) in [(0, 10), (0, total), (5, 5), (100, 400), (total - 1, 1), (total - 10, 100), (37, 0), (211, 3)] {
            let mut out = Vec::new();
            for slice in plan_partial_from_segments(&list, start, length) {
                let bytes = segment_bytes(&list[slice.index], &files).unwrap();
                out.extend_from_slice(&bytes[slice.skip as usize..(slice.skip + slice.take) as usize]);
            }
            let end = (start + length).min(total) as usize;
            assert_eq!(out, &whole[start as usize..end], "<{start}.{length}>");
        }
    }

    #[test]
    fn a_range_at_or_past_the_end_is_empty() {
        let list = segments(lazy(30, 0));
        let total = sum_segment_list_bytes(&list);
        assert!(plan_partial_from_segments(&list, total, 10).is_empty());
        assert!(plan_partial_from_segments(&list, total + 500, 10).is_empty());
        assert!(plan_partial_from_segments(&list, 0, 0).is_empty());
    }

    #[test]
    fn a_misaligned_skip_seeks_to_the_group_before_it_and_trims_the_rest() {
        // Base64 maps 3 raw bytes to 4 characters, so a read starts on a
        // 4-character boundary and drops up to 3 characters.
        assert_eq!(base64_seek(0), Base64Seek { aligned_base64: 0, raw_offset: 0, residual: 0 });
        assert_eq!(base64_seek(4), Base64Seek { aligned_base64: 4, raw_offset: 3, residual: 0 });
        assert_eq!(base64_seek(7), Base64Seek { aligned_base64: 4, raw_offset: 3, residual: 3 });
        assert_eq!(base64_seek(1_000_002), Base64Seek { aligned_base64: 1_000_000, raw_offset: 750_000, residual: 2 });

        let source: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let whole = b64(&source);
        for skip in [0u64, 1, 2, 3, 4, 5, 6, 7, 8, 333, 1001] {
            let seek = base64_seek(skip);
            let read = b64(&source[seek.raw_offset as usize..]);
            let mut window = StreamWindow::new(seek.residual, 20);
            assert_eq!(window.slice(read.as_bytes()).unwrap(), &whole.as_bytes()[skip as usize..skip as usize + 20]);
            assert!(window.is_done());
        }
    }

    #[test]
    fn a_window_stops_pulling_once_it_is_satisfied() {
        let mut window = StreamWindow::new(5, 7);
        assert_eq!(window.slice(b"abc"), None);
        assert_eq!(window.slice(b"defgh"), Some(&b"fgh"[..]));
        assert!(!window.is_done());
        assert_eq!(window.slice(b"ijklmnop"), Some(&b"ijkl"[..]));
        assert!(window.is_done());
        assert_eq!(window.slice(b"more"), None);
    }

    #[test]
    fn text_is_the_whole_message_minus_its_header_block() {
        let files = none();
        let list = build_message_segments(&mail(json!({ "text": "plain", "html": "<p>rich</p>", "subject": "s" })), Some("doc-text"), &|_| None);
        let bytes = |segments: &[MessageSegment]| -> Vec<u8> {
            segments.iter().flat_map(|segment| segment_bytes(segment, &files).unwrap()).collect()
        };
        let whole = bytes(&list);
        let header_block = segment_byte_length(&list[0]) as usize;
        assert_eq!(bytes(&select_body_segments(&list)), &whole[header_block..]);
        assert_eq!(sum_body_bytes(&list) as usize, whole.len() - header_block);
    }

    #[test]
    fn a_part_is_the_body_segment_at_its_path() {
        let files = none().with("att-1", b"attachment bytes");
        let value = json!({ "text": "plain", "html": "<p>rich</p>", "attachments": [{ "content": { "data": "att-1" }, "filename": "a.bin", "contentType": "application/octet-stream", "size": 16 }] });
        let list = build_message_segments(&mail(value), Some("doc-parts"), &|id| files.size(id));
        let part = |path: &str| -> String {
            let bytes: Vec<u8> = select_part_body_segments(&list, path).iter().flat_map(|segment| segment_bytes(segment, &files).unwrap()).collect();
            String::from_utf8(bytes).unwrap()
        };
        assert_eq!(part("1.1"), b64("plain"));
        assert_eq!(part("1.2"), b64("<p>rich</p>"));
        assert_eq!(part("2"), b64("attachment bytes"));
        assert_eq!(part("3"), "");
        assert_eq!(sum_part_body_bytes(&list, "2"), 24);

        let alternative = build_message_segments(&mail(json!({ "text": "plain", "html": "<p>rich</p>" })), Some("d"), &|_| None);
        assert_eq!(select_part_body_segments(&alternative, "1").len(), 1);
        assert_eq!(select_part_body_segments(&alternative, "2").len(), 1);
        let single = build_message_segments(&mail(json!({ "text": "plain" })), Some("d"), &|_| None);
        assert_eq!(sum_part_body_bytes(&single, "1"), 8);
    }
}

// ---------------------------------------------------------------------------
// fetch-helpers.test.ts
// ---------------------------------------------------------------------------

mod requested_fields {
    use super::*;

    const LAZY: [&str; 4] = ["text_octets", "html_octets", "mail_id", "user_id"];

    fn body(section: BodySection) -> FetchDataItem {
        FetchDataItem::Body(BodyFetch { peek: true, section, partial: None })
    }

    fn fields(items: &[FetchDataItem]) -> RequestedFields {
        get_requested_fields(items)
    }

    fn sorted(fields: &RequestedFields) -> Vec<&'static str> {
        let mut names = fields.as_slice().to_vec();
        names.sort_unstable();
        names
    }

    fn has_all(fields: &RequestedFields, names: &[&str]) -> bool {
        names.iter().all(|name| fields.contains(name))
    }

    #[test]
    fn flags_requests_every_flag_backing_column_including_answered() {
        assert!(has_all(&fields(&[FetchDataItem::Flags]), &["read", "saved", "deleted", "draft", "answered"]));
    }

    #[test]
    fn envelope_requests_the_header_columns_including_reply_to() {
        assert!(has_all(&fields(&[FetchDataItem::Envelope]), &["subject", "from", "messageId", "replyTo"]));
    }

    #[test]
    fn full_header_and_all_except_sections_request_reply_to() {
        assert!(fields(&[body(BodySection::Full)]).contains("replyTo"));
        assert!(fields(&[body(BodySection::Header)]).contains("replyTo"));
        assert!(fields(&[body(header_fields(true, &["SUBJECT"]))]).contains("replyTo"));
    }

    #[test]
    fn always_includes_uid() {
        assert!(fields(&[]).contains("uid"));
    }

    #[test]
    fn unions_columns_across_multiple_data_items() {
        assert!(has_all(&fields(&[FetchDataItem::Flags, FetchDataItem::InternalDate]), &["answered", "date"]));
    }

    #[test]
    fn rfc822_requests_the_same_columns_as_body() {
        let rfc = fields(&[FetchDataItem::Rfc822]);
        assert_eq!(sorted(&rfc), sorted(&fields(&[body(BodySection::Full)])));
        assert!(has_all(&rfc, &LAZY) && has_all(&rfc, &["subject", "from", "attachments"]));
        assert!(!rfc.contains("text") && !rfc.contains("html"));
    }

    #[test]
    fn rfc822_header_requests_the_same_columns_as_body_header() {
        let rfc = fields(&[FetchDataItem::Rfc822Header]);
        assert_eq!(sorted(&rfc), sorted(&fields(&[body(BodySection::Header)])));
        assert!(rfc.contains("subject") && !rfc.contains("text"));
    }

    #[test]
    fn rfc822_text_requests_the_same_columns_as_body_text() {
        let rfc = fields(&[FetchDataItem::Rfc822Text]);
        assert_eq!(sorted(&rfc), sorted(&fields(&[body(BodySection::Text)])));
        assert!(!rfc.contains("text") && !rfc.contains("html") && has_all(&rfc, &LAZY));
    }

    #[test]
    fn body_sections_project_the_lazy_synthetics_and_never_the_body_columns() {
        let sections = [
            BodySection::Full,
            BodySection::Text,
            part_number("1", None),
            part_number("1", Some(MimeSubSection::Text)),
            part_number("1", Some(MimeSubSection::Mime)),
            part_number("1", Some(MimeSubSection::Header)),
        ];
        for section in sections {
            for partial in [None, Some((0.0, 100.0))] {
                let item = FetchDataItem::Body(body_fetch(section.clone(), partial));
                let projected = fields(&[item]);
                assert!(!projected.contains("text") && !projected.contains("html"), "{section:?}");
                assert!(has_all(&projected, &LAZY), "{section:?}");
            }
        }
    }

    #[test]
    fn text_and_a_mime_sub_section_in_one_fetch_stay_lazy() {
        let union = fields(&[body(BodySection::Text), body(part_number("1", Some(MimeSubSection::Mime)))]);
        assert!(!union.contains("text") && !union.contains("html") && has_all(&union, &LAZY));
    }

    #[test]
    fn rfc822_size_requests_the_full_message_columns_plus_the_cached_column() {
        let size = fields(&[FetchDataItem::Rfc822Size]);
        let full = fields(&[body(BodySection::Full)]);
        assert!(has_all(&size, full.as_slice()));
        assert!(size.contains("rfc822_size"));
        assert_eq!(size.len(), full.len() + 1);
        assert!(has_all(&size, &["text_octets", "html_octets", "mail_id", "user_id", "subject", "from", "to", "cc", "bcc", "date", "messageId", "attachments"]));
        assert!(!size.contains("text") && !size.contains("html"));
    }

    #[test]
    fn bodystructure_projects_exactly_uid_octets_and_attachments() {
        for extensible in [true, false] {
            let projected = fields(&[FetchDataItem::BodyStructure { extensible }]);
            assert_eq!(sorted(&projected), ["attachments", "html_octets", "text_octets", "uid"]);
        }
    }

    #[test]
    fn a_partial_full_body_projects_the_same_lazy_columns_as_a_whole_one() {
        let mut whole = RequestedFields::default();
        add_body_fields(&body_fetch(BodySection::Full, None), &mut whole);
        let mut partial = RequestedFields::default();
        add_body_fields(&body_fetch(BodySection::Full, Some((0.0, 100.0))), &mut partial);
        assert_eq!(whole, partial);
        assert!(has_all(&partial, &LAZY) && !partial.contains("text"));
    }

    #[test]
    fn header_fields_request_only_the_columns_they_name() {
        let projected = fields(&[body(header_fields(false, &["From", "x-custom", "REPLY-TO"]))]);
        assert_eq!(projected.as_slice(), ["uid", "from", "replyTo"]);
    }
}

mod sequence_sets {
    use super::*;

    fn converted(ranges: Vec<SequenceRange>) -> Vec<(f64, f64)> {
        convert_sequence_set(&SequenceSet::new(ranges)).iter().map(|range| (range.start, range.end)).collect()
    }

    #[test]
    fn flips_a_descending_range_to_ascending() {
        assert_eq!(converted(vec![SequenceRange::range(3.0, 1.0)]), [(1.0, 3.0)]);
    }

    #[test]
    fn leaves_an_ascending_range_unchanged() {
        assert_eq!(converted(vec![SequenceRange::range(1.0, 3.0)]), [(1.0, 3.0)]);
    }

    #[test]
    fn normalizes_star_to_one_so_it_covers_the_whole_mailbox() {
        assert_eq!(converted(vec![SequenceRange::range(MAX_SAFE_INTEGER, 1.0)]), [(1.0, MAX_SAFE_INTEGER)]);
    }

    #[test]
    fn keeps_a_single_number_as_a_unit_range() {
        assert_eq!(converted(vec![SequenceRange::single(5.0)]), [(5.0, 5.0)]);
    }

    #[test]
    fn normalizes_each_range_independently() {
        let ranges = vec![SequenceRange::range(5.0, 1.0), SequenceRange::single(7.0), SequenceRange::range(2.0, 4.0)];
        assert_eq!(converted(ranges), [(1.0, 5.0), (7.0, 7.0), (2.0, 4.0)]);
    }
}

mod response_parts {
    use super::*;

    fn base() -> Value {
        json!({
            "uid": { "account": 1, "domain": 1 },
            "messageId": "<test@local>",
            "date": "2026-06-21T00:00:00Z",
            "from": { "text": "alice@example.com", "value": [] },
            "to": { "text": "bob@example.com", "value": [] },
            "subject": "hello",
            "text": "body line one\r\nbody line two",
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

    fn whole(section: BodySection) -> FetchDataItem {
        FetchDataItem::Body(BodyFetch { peek: false, section, partial: None })
    }

    // --- RFC822 aliases (inbox #587) ---

    #[test]
    fn rfc822_emits_the_same_bytes_as_body_labelled_rfc822() {
        let message = mail(base());
        let files = none();
        let rfc = emitted(&item_part(&message, &FetchDataItem::Rfc822, "INBOX", &files).unwrap(), &files);
        let body = emitted(&item_part(&message, &whole(BodySection::Full), "INBOX", &files).unwrap(), &files);
        assert_eq!(rfc.2, body.2);
        assert_eq!(rfc.1, body.1);
        assert_eq!((rfc.0.as_str(), body.0.as_str()), ("RFC822", "BODY[]"));
    }

    #[test]
    fn rfc822_header_emits_the_same_bytes_as_body_header() {
        let message = mail(base());
        let files = none();
        let rfc = item_part(&message, &FetchDataItem::Rfc822Header, "INBOX", &files).unwrap();
        assert!(matches!(rfc, FetchResponsePart::Literal { .. }));
        let rfc = emitted(&rfc, &files);
        let body = emitted(&item_part(&message, &whole(BodySection::Header), "INBOX", &files).unwrap(), &files);
        assert_eq!(rfc.2, body.2);
        assert_eq!(rfc.0, "RFC822.HEADER");
    }

    #[test]
    fn rfc822_text_emits_the_same_bytes_as_body_text() {
        let message = mail(base());
        let files = none();
        let rfc = item_part(&message, &FetchDataItem::Rfc822Text, "INBOX", &files).unwrap();
        assert!(matches!(rfc, FetchResponsePart::Stream(_)));
        let rfc = emitted(&rfc, &files);
        let body = emitted(&item_part(&message, &whole(BodySection::Text), "INBOX", &files).unwrap(), &files);
        assert_eq!(rfc.2, body.2);
        assert_eq!(rfc.1, body.1);
        assert_eq!(rfc.0, "RFC822.TEXT");
    }

    // --- bare BODY vs BODYSTRUCTURE (#666) ---

    fn structure_mail() -> FetchMailInput {
        mail(json!({
            "uid": { "account": 1, "domain": 1 },
            "text": "Hello",
            "attachments": [{ "content": { "data": "att1" }, "filename": "document.pdf", "size": 1024, "contentType": "application/pdf" }]
        }))
    }

    #[test]
    fn bare_body_emits_a_structure_line_not_content() {
        let content = simple(item_part(&structure_mail(), &FetchDataItem::BodyStructure { extensible: false }, "INBOX", &none()));
        assert!(content.starts_with("BODY ") && !content.starts_with("BODY["));
        assert!(!content.contains("\"ATTACHMENT\"") && content.contains("\"mixed\")"));
    }

    #[test]
    fn bodystructure_emits_a_line_with_the_extension_data() {
        let content = simple(item_part(&structure_mail(), &FetchDataItem::BodyStructure { extensible: true }, "INBOX", &none()));
        assert!(content.starts_with("BODYSTRUCTURE "));
        assert!(content.contains("\"ATTACHMENT\"") && content.contains("\"mixed\" NIL NIL NIL NIL)"));
    }

    // --- RFC822.SIZE == BODY[] octet count (inbox #654) ---

    #[test]
    fn rfc822_size_equals_the_octets_body_actually_emits() {
        let shapes = [
            ("text only", json!({ "text": "line one\r\nline two", "html": "" })),
            ("html only", json!({ "text": "", "html": "<p>hi there</p>" })),
            ("multipart/alternative", json!({ "text": "plain body", "html": "<p>rich body</p>" })),
            (
                "with attachment",
                json!({ "text": "see attached", "html": "", "attachments": [{ "filename": "a.txt", "contentType": "text/plain", "size": 11, "content": { "data": "aGVsbG8gd29ybGQ=" } }] }),
            ),
            ("whitespace-only text", json!({ "text": "   \r\n  ", "html": "" })),
            ("whitespace-only text with real html", json!({ "text": "   ", "html": "<p>rich</p>" })),
            ("whitespace-only text and html", json!({ "text": "  ", "html": "   " })),
            ("no body at all", json!({ "text": "", "html": "" })),
            ("multibyte utf-8", json!({ "subject": "größe 日本語 ✉", "text": "café ☕ 日本語 — first line\r\nemoji 😀 tail", "html": "" })),
        ];
        let files = none();
        for (label, changes) in shapes {
            let message = mail(with(base(), changes));
            let size = simple(item_part(&message, &FetchDataItem::Rfc822Size, "INBOX", &files));
            let reported: u64 = size.strip_prefix("RFC822.SIZE ").unwrap().parse().unwrap();
            let body = item_part(&message, &FetchDataItem::Body(body_fetch(BodySection::Full, None)), "INBOX", &files).unwrap();
            assert!(matches!(body, FetchResponsePart::Stream(_)), "{label}");
            let (_, length, bytes) = emitted(&body, &files);
            assert_eq!(reported, length, "{label}");
            assert_eq!(bytes.len() as u64, reported, "{label}");
        }
    }

    // --- BODY[] {N} holds when the attachment file disagrees with the mail row ---

    fn attached(entries: &[(&str, Option<u64>)]) -> FetchMailInput {
        let attachments: Vec<Value> = entries
            .iter()
            .map(|(id, size)| {
                let mut entry = json!({ "content": { "data": id }, "filename": format!("{id}.bin"), "contentType": "application/octet-stream" });
                if let Some(size) = size {
                    entry["size"] = json!(size);
                }
                entry
            })
            .collect();
        mail(with(base(), json!({ "attachments": attachments })))
    }

    /// `BODY[]`, `RFC822.SIZE` and the emitted octets of a mail all agree.
    fn assert_literal_holds(message: &FetchMailInput, files: &AttachmentFiles) -> Vec<u8> {
        let body = item_part(message, &FetchDataItem::Body(body_fetch(BodySection::Full, None)), "INBOX", files).unwrap();
        let (_, length, bytes) = emitted(&body, files);
        assert_eq!(bytes.len() as u64, length);
        let size = simple(item_part(message, &FetchDataItem::Rfc822Size, "INBOX", files));
        assert_eq!(size, format!("RFC822.SIZE {length}"));
        bytes
    }

    #[test]
    fn the_literal_holds_whatever_size_the_row_stores() {
        let data = b"twelve bytes";
        let files = none().with("att-a", data);
        // Larger than the file, smaller, zero, and absent.
        for stored in [Some(5000), Some(3), Some(0), None] {
            let bytes = assert_literal_holds(&attached(&[("att-a", stored)]), &files);
            let wire = String::from_utf8(bytes).unwrap();
            assert!(wire.contains(&b64(data)), "{stored:?}");
            assert!(!wire.contains("NaN"));
        }
    }

    #[test]
    fn the_literal_holds_when_the_attachment_file_is_missing() {
        let bytes = assert_literal_holds(&attached(&[("att-gone", Some(100))]), &none());
        assert!(String::from_utf8(bytes).unwrap().contains(&b64(MISSING_ATTACHMENT_NOTICE)));
    }

    #[test]
    fn the_literal_holds_for_several_attachments_present_and_missing() {
        let files = none().with("att-1", b"first").with("att-3", &[7u8; 1000]);
        let bytes = assert_literal_holds(&attached(&[("att-1", Some(5)), ("att-2", Some(9)), ("att-3", Some(10))]), &files);
        let wire = String::from_utf8(bytes).unwrap();
        assert!(wire.contains(&b64("first")) && wire.contains(&b64([7u8; 1000])) && wire.contains(&b64(MISSING_ATTACHMENT_NOTICE)));
    }

    #[test]
    fn a_file_that_changed_after_it_was_measured_is_cut_or_padded_to_the_literal() {
        // The size is resolved when the segments are built; the file is read
        // when they are emitted.
        assert_eq!(attachment_base64(3, Some(b"abcdef")), "YWJj");
        assert_eq!(attachment_base64(6, Some(b"abc")), "YWJj\n\n\n\n");
        assert_eq!(attachment_base64(0, Some(b"abc")), "");
        let gone = attachment_base64(MISSING_ATTACHMENT_NOTICE.len() as u64, None);
        assert_eq!(gone, b64(MISSING_ATTACHMENT_NOTICE));
    }

    #[test]
    fn a_multi_slice_attachment_round_trips_byte_for_byte() {
        let data: Vec<u8> = (0..(SLICE_RAW_BYTES * 2 + 1234)).map(|i| (i * 31 % 251) as u8).collect();
        let files = none().with("att-big", &data);
        let message = attached(&[("att-big", Some(data.len() as u64))]);
        let wire = String::from_utf8(assert_literal_holds(&message, &files)).unwrap();
        let start = wire.find("filename=\"att-big.bin\"\r\n\r\n").unwrap() + "filename=\"att-big.bin\"\r\n\r\n".len();
        let encoded = &wire[start..start + base64_byte_len(data.len() as u64) as usize];
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(encoded).unwrap(), data);
    }

    // --- BODY[] streams without materializing ---

    #[test]
    fn body_is_a_stream_part_with_a_pre_computed_length() {
        let message = mail(base());
        let part = item_part(&message, &FetchDataItem::Body(body_fetch(BodySection::Full, None)), "INBOX", &none()).unwrap();
        let FetchResponsePart::Stream(stream) = part else {
            panic!("expected a stream part");
        };
        assert_eq!(stream.header, "BODY[]");
        assert_eq!(stream.stream_key, "doc-1::BODY[]");
        assert!(stream.trailer && stream.window.is_none());
        assert_eq!(stream.length, sum_segment_bytes(&stream.segments));
        assert_eq!(stream.length, compute_full_message_size(&message, Some("doc-1"), &|_| None));
    }

    // --- BODYSTRUCTURE metadata-only emit (#740) ---

    #[test]
    fn bodystructure_derives_size_from_the_cached_octet_column() {
        let content = simple(item_part(&mail(json!({ "text_octets": 30, "html_octets": 0 })), &FetchDataItem::BodyStructure { extensible: true }, "INBOX", &none()));
        assert!(content.starts_with("BODYSTRUCTURE (\"TEXT\" \"PLAIN\""));
        assert!(content.contains("\"BASE64\" 40 1"));
    }

    #[test]
    fn bodystructure_emits_an_alternative_from_cached_octets_with_no_strings_loaded() {
        let content = simple(item_part(&mail(json!({ "text_octets": 30, "html_octets": 60 })), &FetchDataItem::BodyStructure { extensible: true }, "INBOX", &none()));
        assert!(content.contains("\"alternative\""));
        assert!(content.contains("\"BASE64\" 40 1") && content.contains("\"BASE64\" 80 1"));
    }

    #[test]
    fn bodystructure_from_loaded_strings_reports_the_same_size_and_lines() {
        let text = "line one\r\nline two";
        let loaded = simple(item_part(&mail(json!({ "text": text })), &FetchDataItem::BodyStructure { extensible: true }, "INBOX", &none()));
        let cached = simple(item_part(&mail(json!({ "text_octets": text.len(), "html_octets": 0 })), &FetchDataItem::BodyStructure { extensible: true }, "INBOX", &none()));
        assert_eq!(loaded, cached);
        assert!(loaded.contains(&format!("\"BASE64\" {} 1", encode_text(text).len())));
    }

    // --- RFC822.SIZE cached-column short-circuit (#729) ---

    #[test]
    fn rfc822_size_returns_the_cached_column_verbatim() {
        let built = build_fetch_response_part(&mail(with(base(), json!({ "rfc822_size": 424242 }))), &FetchDataItem::Rfc822Size, "doc-1", "INBOX", &|_| None);
        assert_eq!(simple(built.part), "RFC822.SIZE 424242");
        assert_eq!(built.computed_rfc822_size, None);
    }

    #[test]
    fn rfc822_size_is_computed_and_handed_back_when_the_column_is_null_or_absent() {
        for value in [with(base(), json!({ "rfc822_size": null })), base()] {
            let message = mail(value);
            let built = build_fetch_response_part(&message, &FetchDataItem::Rfc822Size, "doc-1", "INBOX", &|_| None);
            let expected = compute_full_message_size(&message, Some("doc-1"), &|_| None);
            assert_eq!(simple(built.part), format!("RFC822.SIZE {expected}"));
            // The caller persists it for the next fetch.
            assert_eq!(built.computed_rfc822_size, Some(expected));
        }
    }

    // --- header terminators (inbox #645) ---

    fn header_mail() -> Value {
        with(base(), json!({ "messageId": "<term@local>", "date": "2026-07-03T00:00:00Z", "subject": "Hello", "text": "body line" }))
    }

    fn literal_content(value: Value, section: BodySection, partial: Option<(f64, f64)>) -> (String, String, u64) {
        match body_part(&mail(value), &body_fetch(section, partial), "doc-term", &none()) {
            Some(FetchResponsePart::Literal { content, header, length }) => {
                assert_eq!(content.len() as u64, length);
                (content, header, length)
            }
            other => panic!("expected literal part, got {other:?}"),
        }
    }

    #[test]
    fn body_header_ends_in_exactly_one_delimiting_blank_line() {
        let (content, _, _) = literal_content(header_mail(), BodySection::Header, None);
        assert!(content.ends_with("\r\n\r\n") && !content.ends_with("\r\n\r\n\r\n"));
        assert!(content.contains("Content-Transfer-Encoding: base64\r\n\r\n"));
    }

    #[test]
    fn body_header_fields_ends_in_exactly_one_delimiting_blank_line() {
        let (content, _, _) = literal_content(header_mail(), header_fields(false, &["From", "Subject"]), None);
        assert!(content.contains("From: alice@example.com") && content.contains("Subject: Hello"));
        assert!(!content.contains("To: bob@example.com"));
        assert!(content.ends_with("\r\n\r\n") && !content.ends_with("\r\n\r\n\r\n"));
    }

    #[test]
    fn body_header_fields_from_returns_only_the_genuine_from() {
        let spoofed = with(header_mail(), json!({ "subject": "Hello\r\nFrom: ceo@bank.example\r\nX-Evil: 1" }));
        assert_eq!(literal_content(spoofed, header_fields(false, &["From"]), None).0, "From: alice@example.com\r\n\r\n");
    }

    #[test]
    fn body_header_fields_with_no_match_is_exactly_a_single_blank_line() {
        assert_eq!(literal_content(header_mail(), header_fields(false, &["X-Nonexistent"]), None).0, "\r\n");
    }

    #[test]
    fn body_header_fields_not_ends_in_exactly_one_delimiting_blank_line() {
        let (content, _, _) = literal_content(header_mail(), header_fields(true, &["Subject"]), None);
        assert!(!content.contains("Subject: Hello") && content.contains("From: alice@example.com"));
        assert!(content.ends_with("\r\n\r\n") && !content.ends_with("\r\n\r\n\r\n"));
    }

    #[test]
    fn a_partial_fetch_of_body_header_announces_what_it_emits() {
        let (_, header, length) = literal_content(header_mail(), BodySection::Header, Some((0.0, 10.0)));
        assert_eq!(length, 10);
        assert_eq!(header, "BODY[HEADER]<0>");
    }

    // --- ENVELOPE ---

    #[test]
    fn envelope_delegates_to_format_envelope() {
        let message = mail(json!({
            "date": "2024-01-15T10:30:00Z",
            "subject": "Hello",
            "from": { "text": "John Doe <john@example.com>", "value": [{ "name": "John Doe", "address": "john@example.com" }] },
            "to": { "text": "Jane Roe <jane@example.com>", "value": [{ "name": "Jane Roe", "address": "jane@example.com" }] },
            "messageId": "<test@example.com>"
        }));
        let content = simple(item_part(&message, &FetchDataItem::Envelope, "INBOX", &none()));
        assert_eq!(content, format!("ENVELOPE {}", format_envelope(&message)));
        // From sits in slot 3, not slot 6, and the message id in slot 10.
        assert!(content.contains("\"Hello\" ((\"John Doe\""));
        assert!(!content.contains("\"Hello\" NIL NIL NIL"));
        assert!(content.contains("\"<test@example.com>\""));
    }

    // --- partial fetch literal length (inbox #581) ---

    fn text_mail() -> Value {
        with(base(), json!({ "text": "0123456789 abcdefghij klmnopqrst uvwxyz", "html": "<p>rich</p>" }))
    }

    #[test]
    fn a_partial_fetch_announces_a_length_equal_to_the_octets_emitted() {
        let files = none();
        for (start, length) in [(0.0, 16.0), (5.0, 7.0), (33.0, 64.0)] {
            let part = body_part(&mail(text_mail()), &body_fetch(BodySection::Text, Some((start, length))), "doc-1", &files).unwrap();
            let (header, announced, bytes) = emitted(&part, &files);
            assert_eq!(bytes.len() as u64, announced);
            assert_eq!(announced, length as u64);
            assert_eq!(header, format!("BODY[TEXT]<{start}>"));
        }
    }

    #[test]
    fn a_partial_fetch_slices_the_same_octets_the_whole_section_emits() {
        let files = none();
        let message = mail(text_mail());
        let (_, whole_length, whole_bytes) = emitted(&body_part(&message, &body_fetch(BodySection::Text, None), "doc-1", &files).unwrap(), &files);
        // The whole section carries the wire trailer; a range never does.
        assert!(whole_bytes.ends_with(b"\r\n"));
        let section = &whole_bytes[..whole_bytes.len() - 2];
        assert_eq!(whole_length as usize, section.len() + 2);
        for (start, length) in [(0usize, 20usize), (7, 40), (100, 25)] {
            let part = body_part(&message, &body_fetch(BodySection::Text, Some((start as f64, length as f64))), "doc-1", &files).unwrap();
            assert_eq!(emitted(&part, &files).2, &section[start..start + length]);
        }
    }

    #[test]
    fn a_range_that_overruns_is_clamped_and_one_that_starts_past_the_end_is_nil() {
        let files = none();
        let message = mail(text_mail());
        let (_, whole_length, _) = emitted(&body_part(&message, &body_fetch(BodySection::Text, None), "doc-1", &files).unwrap(), &files);
        let section = whole_length - 2;

        let overrun = body_part(&message, &body_fetch(BodySection::Text, Some((10.0, 1_000_000.0))), "doc-1", &files).unwrap();
        let (_, announced, bytes) = emitted(&overrun, &files);
        assert_eq!(announced, section - 10);
        assert_eq!(bytes.len() as u64, announced);

        for start in [section as f64, section as f64 + 50.0] {
            let past = body_part(&message, &body_fetch(BodySection::Text, Some((start, 10.0))), "doc-1", &files);
            assert_eq!(simple(past), "BODY[TEXT] NIL");
        }
    }

    // --- MIME part sub-sections (inbox #657) ---

    fn alternative() -> Value {
        with(base(), json!({ "text": "plain part", "html": "<p>html part</p>" }))
    }

    #[test]
    fn body_1_mime_and_1_header_return_part_1s_mime_header_block() {
        for (sub, key) in [(MimeSubSection::Mime, "BODY[1.MIME]"), (MimeSubSection::Header, "BODY[1.HEADER]")] {
            let (content, header, _) = literal_content(alternative(), part_number("1", Some(sub)), None);
            assert_eq!(header, key);
            assert_eq!(content, "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
        }
    }

    #[test]
    fn body_2_mime_returns_part_2s_mime_header_block() {
        let (content, _, _) = literal_content(alternative(), part_number("2", Some(MimeSubSection::Mime)), None);
        assert_eq!(content, "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
    }

    #[test]
    fn body_1_text_returns_the_header_less_base64_body_same_as_body_1() {
        let files = none();
        let message = mail(alternative());
        let text = emitted(&body_part(&message, &body_fetch(part_number("1", Some(MimeSubSection::Text)), None), "doc-1", &files).unwrap(), &files);
        let bare = emitted(&body_part(&message, &body_fetch(part_number("1", None), None), "doc-1", &files).unwrap(), &files);
        assert_eq!(text.2, bare.2);
        assert_eq!(text.2, format!("{}\r\n", b64("plain part")).into_bytes());
        assert_eq!((text.0.as_str(), bare.0.as_str()), ("BODY[1.TEXT]", "BODY[1]"));
    }

    // --- UID data item: domain-scoped UID space (#702) ---

    #[test]
    fn uid_is_the_domain_uid_in_domain_scoped_mailboxes_and_the_account_uid_elsewhere() {
        let message = mail(json!({ "uid": { "domain": 7001, "account": 12 } }));
        assert_eq!(simple(item_part(&message, &FetchDataItem::Uid, "INBOX", &none())), "UID 7001");
        assert_eq!(simple(item_part(&message, &FetchDataItem::Uid, "Sent Messages", &none())), "UID 7001");
        assert_eq!(simple(item_part(&message, &FetchDataItem::Uid, "INBOX/accounts/support", &none())), "UID 12");
    }

    // --- cached-path shape preservation ---

    #[test]
    fn body_text_on_a_mail_with_no_body_is_nil() {
        let empty = with(base(), json!({ "text": "", "html": "" }));
        assert_eq!(simple(body_part(&mail(empty), &body_fetch(BodySection::Text, None), "doc-1", &none())), "BODY[TEXT] NIL");
    }

    #[test]
    fn a_part_the_message_does_not_have_is_dropped_not_emitted_as_nil() {
        let files = none();
        assert_eq!(body_part(&mail(base()), &body_fetch(part_number("99", None), None), "doc-1", &files), None);
        assert_eq!(body_part(&mail(base()), &body_fetch(part_number("2", None), None), "doc-1", &files), None);
    }

    // --- prod-shape (lazy-projected) mail (inbox #770 R1) ---

    fn projected() -> Value {
        json!({
            "uid": { "account": 1, "domain": 1 },
            "messageId": "<lazy@local>",
            "subject": "lazy",
            "text_octets": 1200,
            "html_octets": 0,
            "mail_id": "mail-770",
            "user_id": "user-770",
            "attachments": []
        })
    }

    #[test]
    fn body_1_mime_on_a_lazy_mail_returns_the_parts_mime_header() {
        let (content, header, _) = literal_content(projected(), part_number("1", Some(MimeSubSection::Mime)), None);
        assert_eq!(header, "BODY[1.MIME]");
        assert_eq!(content, "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
    }

    #[test]
    fn body_1_mime_and_body_1_agree_on_part_numbering_for_a_lazy_mail() {
        let message = mail(projected());
        let part = body_part(&message, &body_fetch(part_number("1", None), None), "doc-1", &none()).unwrap();
        let FetchResponsePart::Stream(stream) = part else {
            panic!("expected a stream part");
        };
        assert_eq!(stream.length, base64_byte_len(1200) + WIRE_TRAILER);
        assert!(matches!(&stream.segments[..], [MessageSegment::LazyText { source: BodySource::Text, byte_length: 1200, .. }]));
        assert!(get_body_part_headers(&message, "1").is_some());
        assert_eq!(get_body_part_headers(&message, "2"), None);
    }

    #[test]
    fn partial_text_and_part_fetches_on_a_lazy_mail_are_streams() {
        let message = mail(projected());
        for (section, key) in [(BodySection::Text, "BODY[TEXT]<0>"), (part_number("1", None), "BODY[1]<0>")] {
            let part = body_part(&message, &body_fetch(section, Some((0.0, 100.0))), "doc-1", &none()).unwrap();
            let FetchResponsePart::Stream(stream) = part else {
                panic!("expected a stream part");
            };
            assert_eq!(stream.header, key);
            assert_eq!(stream.length, 100);
            assert_eq!(stream.window, Some((0, 100)));
            assert!(!stream.trailer);
        }
    }

    // --- partial BODY[]<start.length> ---

    #[test]
    fn a_partial_full_body_is_a_stream_of_exactly_its_length() {
        let files = none();
        let message = mail(text_mail());
        let (_, whole_length, whole_bytes) = emitted(&body_part(&message, &body_fetch(BodySection::Full, None), "doc-1", &files).unwrap(), &files);
        // The trailer is IMAP framing, not part of the message a range addresses.
        let addressable = whole_length - WIRE_TRAILER;

        let first = body_part(&message, &body_fetch(BodySection::Full, Some((0.0, 100.0))), "doc-1", &files).unwrap();
        let (header, length, bytes) = emitted(&first, &files);
        assert_eq!((header.as_str(), length), ("BODY[]<0>", 100));
        assert_eq!(bytes, &whole_bytes[..100]);

        // Chunked through the whole message, every chunk emits what it announces.
        let mut rebuilt = Vec::new();
        let mut offset = 0;
        while offset < addressable {
            let part = body_part(&message, &body_fetch(BodySection::Full, Some((offset as f64, 97.0))), "doc-1", &files).unwrap();
            let (_, length, bytes) = emitted(&part, &files);
            assert_eq!(bytes.len() as u64, length);
            rebuilt.extend(bytes);
            offset += 97;
        }
        assert_eq!(rebuilt, &whole_bytes[..addressable as usize]);

        let clamped = body_part(&message, &body_fetch(BodySection::Full, Some((addressable as f64 - 10.0, 500.0))), "doc-1", &files).unwrap();
        assert_eq!(emitted(&clamped, &files).1, 10);
        let past = body_part(&message, &body_fetch(BodySection::Full, Some((addressable as f64, 10.0))), "doc-1", &files);
        assert_eq!(simple(past), "BODY[] NIL");
    }

    // --- partial fetches into an attachment ---

    #[test]
    fn a_range_into_an_attachment_equals_that_range_of_the_whole_at_every_alignment() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 256) as u8).collect();
        let files = none().with("att-w", &data);
        let message = attached(&[("att-w", Some(5000))]);
        for section in [BodySection::Full, part_number("2", None)] {
            let (_, _, whole) = emitted(&body_part(&message, &body_fetch(section.clone(), None), "doc-1", &files).unwrap(), &files);
            for start in [0usize, 1, 2, 3, 4, 5, 6, 7, 400, 401, 402, 403, 3000] {
                let part = body_part(&message, &body_fetch(section.clone(), Some((start as f64, 333.0))), "doc-1", &files).unwrap();
                assert_eq!(emitted(&part, &files).2, &whole[start..start + 333], "{section:?} <{start}.333>");
            }
        }
    }

    #[test]
    fn a_missing_attachment_yields_the_same_bytes_partially_as_in_full() {
        let files = none();
        let message = attached(&[("att-none", Some(999))]);
        let section = part_number("2", None);
        let (_, _, whole) = emitted(&body_part(&message, &body_fetch(section.clone(), None), "doc-1", &files).unwrap(), &files);
        assert_eq!(whole, format!("{}\r\n", b64(MISSING_ATTACHMENT_NOTICE)).into_bytes());
        let part = body_part(&message, &body_fetch(section, Some((5.0, 12.0))), "doc-1", &files).unwrap();
        assert_eq!(emitted(&part, &files).2, &whole[5..17]);
    }

    // --- buildFetchResponse ---

    #[test]
    fn a_response_leads_with_uid_for_a_uid_fetch_and_ends_with_modseq() {
        let message = mail(with(base(), json!({ "modseq": 77, "read": true })));
        let items = [FetchDataItem::Flags, FetchDataItem::Uid, FetchDataItem::Modseq];
        let contents = |is_uid_fetch: bool, condstore: bool, items: &[FetchDataItem]| -> Vec<String> {
            build_fetch_response(&message, items, "doc-1", 4242.0, is_uid_fetch, "INBOX", condstore, &|_| None)
                .parts
                .into_iter()
                .map(|part| simple(Some(part)))
                .collect()
        };
        assert_eq!(contents(true, false, &items), ["UID 4242", "FLAGS (\\Seen)", "MODSEQ (77)"]);
        assert_eq!(contents(false, false, &items), ["FLAGS (\\Seen)", "UID 1", "MODSEQ (77)"]);
        // Implicit once CONDSTORE is enabled, and never twice.
        assert_eq!(contents(false, true, &[FetchDataItem::Flags]), ["FLAGS (\\Seen)", "MODSEQ (77)"]);
        assert_eq!(contents(false, false, &[FetchDataItem::Flags]), ["FLAGS (\\Seen)"]);
        assert_eq!(literal_prefix("BODY[]", 632), "BODY[] {632}\r\n");
    }
}
