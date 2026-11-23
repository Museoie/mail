//! The Bun server's `imap/util.test.ts`, case for case.

use mail_server::imap::session_format::{build_message_segments, select_part_body_segments};
use mail_server::imap::types::{FetchMailInput, MailAddressValue, MailAttachment};
use mail_server::imap::util::*;
use serde_json::json;

use crate::support::{mail, segment_bytes, AttachmentFiles};

fn addresses(value: serde_json::Value) -> Vec<MailAddressValue> {
    serde_json::from_value(value).unwrap()
}

fn address_list(value: serde_json::Value) -> String {
    format_address_list(Some(&addresses(value)))
}

fn empty() -> FetchMailInput {
    FetchMailInput::default()
}

fn pdf(filename: &str, size: u64) -> serde_json::Value {
    json!({ "content": { "data": "att1" }, "filename": filename, "size": size, "contentType": "application/pdf" })
}

#[test]
fn is_domain_scoped_is_true_for_inbox_in_any_casing() {
    assert!(is_domain_scoped("INBOX"));
    assert!(is_domain_scoped("inbox"));
    assert!(is_domain_scoped("Inbox"));
}

#[test]
fn is_domain_scoped_is_true_for_the_unified_sent_messages_folder() {
    assert!(is_domain_scoped("Sent Messages"));
}

#[test]
fn is_domain_scoped_is_false_for_account_scoped_and_user_mailboxes() {
    assert!(!is_domain_scoped("Sent Messages/accounts/foo"));
    assert!(!is_domain_scoped("INBOX/accounts/foo"));
    assert!(!is_domain_scoped("Archive"));
    assert!(!is_domain_scoped(""));
}

#[test]
fn encode_text_encodes_simple_text_to_base64() {
    assert_eq!(encode_text("Hello"), "SGVsbG8=");
}

#[test]
fn encode_text_encodes_unicode_text() {
    assert_eq!(encode_text("日本語"), "5pel5pys6Kqe");
}

#[test]
fn encode_text_encodes_empty_string() {
    assert_eq!(encode_text(""), "");
}

#[test]
fn address_list_returns_nil_for_undefined_value() {
    assert_eq!(format_address_list(None), "NIL");
}

#[test]
fn address_list_returns_nil_for_empty_array() {
    assert_eq!(format_address_list(Some(&[])), "NIL");
}

#[test]
fn address_list_formats_single_address() {
    assert_eq!(
        address_list(json!([{ "name": "John Doe", "address": "john@example.com" }])),
        "(\"John Doe\" NIL \"john\" \"example.com\")"
    );
}

#[test]
fn address_list_formats_multiple_addresses() {
    assert_eq!(
        address_list(json!([
            { "name": "John Doe", "address": "john@example.com" },
            { "name": "Jane Smith", "address": "jane@example.com" }
        ])),
        "(\"John Doe\" NIL \"john\" \"example.com\") (\"Jane Smith\" NIL \"jane\" \"example.com\")"
    );
}

#[test]
fn address_list_emits_nil_as_addr_name_for_an_empty_name() {
    assert_eq!(address_list(json!([{ "name": "", "address": "john@example.com" }])), "(NIL NIL \"john\" \"example.com\")");
}

#[test]
fn address_list_emits_nil_as_addr_name_for_an_absent_name() {
    assert_eq!(address_list(json!([{ "address": "john@example.com" }])), "(NIL NIL \"john\" \"example.com\")");
}

#[test]
fn address_list_keeps_a_whitespace_only_name() {
    assert_eq!(address_list(json!([{ "name": " ", "address": "john@example.com" }])), "(\" \" NIL \"john\" \"example.com\")");
}

#[test]
fn address_list_mixes_nil_and_quoted_addr_names() {
    assert_eq!(
        address_list(json!([
            { "name": "", "address": "john@example.com" },
            { "name": "Jane Smith", "address": "jane@example.com" }
        ])),
        "(NIL NIL \"john\" \"example.com\") (\"Jane Smith\" NIL \"jane\" \"example.com\")"
    );
}

#[test]
fn address_list_escapes_quotes_in_name() {
    assert_eq!(
        address_list(json!([{ "name": "John \"Johnny\" Doe", "address": "john@example.com" }])),
        "(\"John \\\"Johnny\\\" Doe\" NIL \"john\" \"example.com\")"
    );
}

#[test]
fn address_list_skips_invalid_addresses_without_domain() {
    assert_eq!(address_list(json!([{ "name": "Invalid", "address": "nodomain" }])), "NIL");
}

#[test]
fn address_list_skips_addresses_with_empty_address_field() {
    assert_eq!(address_list(json!([{ "name": "Empty", "address": "" }])), "NIL");
}

#[test]
fn flags_returns_empty_array_for_unset_flags() {
    assert!(format_flags(&empty()).is_empty());
}

#[test]
fn flags_returns_each_flag_for_its_field() {
    assert_eq!(format_flags(&mail(json!({ "read": true }))), vec!["\\Seen"]);
    assert_eq!(format_flags(&mail(json!({ "saved": true }))), vec!["\\Flagged"]);
    assert_eq!(format_flags(&mail(json!({ "deleted": true }))), vec!["\\Deleted"]);
}

#[test]
fn flags_returns_multiple_flags() {
    assert_eq!(
        format_flags(&mail(json!({ "read": true, "saved": true, "answered": true }))),
        vec!["\\Seen", "\\Flagged", "\\Answered"]
    );
}

#[test]
fn flags_returns_all_flags_when_all_are_set() {
    assert_eq!(
        format_flags(&mail(json!({ "read": true, "saved": true, "deleted": true, "draft": true, "answered": true }))),
        vec!["\\Seen", "\\Flagged", "\\Deleted", "\\Draft", "\\Answered"]
    );
}

#[test]
fn account_to_box_extracts_the_local_part() {
    assert_eq!(account_to_box("user@example.com"), "INBOX/accounts/user");
    assert_eq!(account_to_box("first.last@example.com"), "INBOX/accounts/first.last");
    assert_eq!(account_to_box("user+tag@example.com"), "INBOX/accounts/user+tag");
}

#[test]
fn box_to_account_converts_each_kind_of_mailbox() {
    assert!(box_to_account("admin", "INBOX/accounts/support").starts_with("support@"));
    assert!(box_to_account("testuser", "Sent Messages/accounts/support").starts_with("support@"));
    assert!(box_to_account("testuser", "support").starts_with("support@"));
}

fn is_internal_date(text: &str) -> bool {
    // DD-Mon-YYYY HH:MM:SS +ZZZZ
    let bytes = text.as_bytes();
    bytes.len() == 26
        && bytes[2] == b'-'
        && bytes[6] == b'-'
        && bytes[11] == b' '
        && bytes[14] == b':'
        && bytes[17] == b':'
        && bytes[20] == b' '
        && (bytes[21] == b'+' || bytes[21] == b'-')
        && bytes[22..].iter().all(u8::is_ascii_digit)
}

#[test]
fn internal_date_has_the_imap_format() {
    let result = format_internal_date(&JsDate::parse("2024-01-15T10:30:45Z"));
    assert!(is_internal_date(&result), "{result}");
    assert_eq!(result, "15-Jan-2024 10:30:45 +0000");
}

#[test]
fn internal_date_space_pads_a_single_digit_day() {
    assert!(format_internal_date(&JsDate::parse("2024-01-05T10:30:45")).starts_with(" 5-Jan-"));
}

#[test]
fn internal_date_renders_a_two_digit_day_without_padding() {
    assert!(format_internal_date(&JsDate::parse("2024-01-15T10:30:45")).starts_with("15-Jan-"));
}

#[test]
fn internal_date_uses_the_month_abbreviations() {
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    for (index, month) in months.iter().enumerate() {
        let date = JsDate::parse(&format!("2024-{:02}-15T10:30:45", index + 1));
        assert!(format_internal_date(&date).contains(&format!("-{month}-")));
    }
}

#[test]
fn envelope_formats_minimal_envelope_with_nils() {
    assert!(format_envelope(&empty()).contains("NIL NIL"));
}

#[test]
fn envelope_includes_date_when_present() {
    let result = format_envelope(&mail(json!({ "date": "2024-01-15T10:30:00Z" })));
    assert!(result.starts_with("(\"Mon, 15 Jan 2024 10:30:00 GMT\" "), "{result}");
}

#[test]
fn envelope_includes_subject_when_present() {
    assert!(format_envelope(&mail(json!({ "subject": "Test Subject" }))).contains("\"Test Subject\""));
}

#[test]
fn envelope_escapes_quotes_in_subject() {
    let result = format_envelope(&mail(json!({ "subject": "Test \"Quoted\" Subject" })));
    assert!(result.contains("Test \\\"Quoted\\\" Subject"));
}

#[test]
fn envelope_includes_message_id_when_present() {
    assert!(format_envelope(&mail(json!({ "messageId": "<test@example.com>" }))).contains("\"<test@example.com>\""));
}

#[test]
fn envelope_formats_from_address() {
    let result = format_envelope(&mail(json!({
        "from": { "text": "John Doe <john@example.com>", "value": [{ "name": "John Doe", "address": "john@example.com" }] }
    })));
    assert!(result.contains("\"John Doe\" NIL \"john\" \"example.com\""));
}

#[test]
fn envelope_emits_absent_address_list_members_as_bare_nil() {
    // date subject from sender reply-to to cc bcc in-reply-to message-id.
    let result = format_envelope(&empty());
    assert!(!result.contains("(NIL)"));
    assert_eq!(result, "(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)");
}

#[test]
fn envelope_keeps_populated_address_slots_parenthesized() {
    let result = format_envelope(&mail(json!({
        "from": { "text": "John <john@example.com>", "value": [{ "name": "John", "address": "john@example.com" }] },
        "to": { "text": "Jane <jane@example.com>", "value": [{ "name": "Jane", "address": "jane@example.com" }] }
    })));
    assert!(result.contains("((\"John\" NIL \"john\" \"example.com\"))"));
    assert!(result.contains("((\"Jane\" NIL \"jane\" \"example.com\"))"));
    assert!(!result.contains("(NIL)"));
}

#[test]
fn envelope_emits_a_no_display_name_address_as_nil_addr_name() {
    let result = format_envelope(&mail(json!({
        "from": { "text": "sender@example.com", "value": [{ "name": "", "address": "sender@example.com" }] },
        "to": { "text": "recipient@example.com", "value": [{ "name": "", "address": "recipient@example.com" }] }
    })));
    assert!(result.contains("((NIL NIL \"sender\" \"example.com\"))"));
    assert!(result.contains("((NIL NIL \"recipient\" \"example.com\"))"));
    assert!(!result.contains("\"\""));
}

#[test]
fn headers_include_mime_version() {
    assert!(format_headers(&empty(), None).contains("MIME-Version: 1.0"));
}

#[test]
fn headers_include_message_id_when_present() {
    let result = format_headers(&mail(json!({ "messageId": "<test@example.com>" })), None);
    assert!(result.contains("Message-ID: <test@example.com>"));
}

#[test]
fn headers_include_subject_when_present() {
    assert!(format_headers(&mail(json!({ "subject": "Test Subject" })), None).contains("Subject: Test Subject"));
}

#[test]
fn headers_set_text_plain_for_text_only_mail() {
    let result = format_headers(&mail(json!({ "text": "Hello, World!" })), None);
    assert!(result.contains("Content-Type: text/plain; charset=utf-8"));
}

#[test]
fn headers_set_text_html_for_html_only_mail() {
    let result = format_headers(&mail(json!({ "html": "<p>Hello, World!</p>" })), None);
    assert!(result.contains("Content-Type: text/html; charset=utf-8"));
}

#[test]
fn headers_set_multipart_alternative_for_text_and_html() {
    let result = format_headers(&mail(json!({ "text": "Hello", "html": "<p>Hello</p>" })), Some("test-doc-id"));
    assert!(result.contains("multipart/alternative"));
    assert!(result.contains("boundary=\"boundary_test-doc-id\""));
}

#[test]
fn headers_set_multipart_mixed_for_mail_with_attachments() {
    let result = format_headers(
        &mail(json!({ "text": "Hello", "attachments": [{ "content": { "data": "att1" }, "filename": "test.txt", "size": 100, "contentType": "text/plain" }] })),
        Some("test-doc-id"),
    );
    assert!(result.contains("multipart/mixed"));
}

#[test]
fn headers_use_crlf_line_endings() {
    let result = format_headers(&mail(json!({ "subject": "Test", "text": "Hello" })), None);
    assert!(result.contains("\r\n"));
    // No bare LF.
    assert!(!result.replace("\r\n", "").contains('\n'));
}

// --- formatHeaders: header injection (#826) ---

/// The block's field names, in order, one entry per emitted line.
fn fields_of(block: &str) -> Vec<&str> {
    block.split("\r\n").map(|line| &line[..line.find(':').unwrap_or(line.len())]).collect()
}

const HOSTILE: &str = "Hello\r\nFrom: ceo@bank.example\r\nX-Evil: 1";

#[test]
fn a_crlf_bearing_subject_emits_one_subject_line_and_no_new_field() {
    let block = format_headers(
        &mail(json!({ "messageId": "<x@y>", "from": { "text": "real@sender.example", "value": [] }, "subject": HOSTILE, "text": "body" })),
        Some("doc1"),
    );
    assert_eq!(
        fields_of(&block),
        vec!["Message-ID", "From", "Subject", "MIME-Version", "Content-Type", "Content-Transfer-Encoding"]
    );
    assert!(block.contains("Subject: Hello From: ceo@bank.example X-Evil: 1"));
    assert!(block.contains("From: real@sender.example\r\n"));
}

/// A mail whose one stored field is `value`, for each field a header carries.
fn carriers(value: &str) -> Vec<(&'static str, FetchMailInput)> {
    let address = json!({ "text": value, "value": [] });
    vec![
        ("messageId", mail(json!({ "messageId": value }))),
        ("subject", mail(json!({ "subject": value }))),
        ("from", mail(json!({ "from": address }))),
        ("to", mail(json!({ "to": address }))),
        ("cc", mail(json!({ "cc": address }))),
        ("bcc", mail(json!({ "bcc": address }))),
        ("replyTo", mail(json!({ "replyTo": address }))),
    ]
}

#[test]
fn no_stored_field_can_open_a_header_line() {
    for (name, carrier) in carriers(HOSTILE) {
        let block = format_headers(&carrier, Some("doc1"));
        // The payload survives as text inside its own field; what must not
        // happen is a line of it.
        assert!(!block.split("\r\n").any(|line| line.starts_with("From: ceo@bank.example")), "{name}");
    }
}

#[test]
fn no_stored_field_can_terminate_the_header_block_early() {
    for (name, carrier) in carriers("start\r\n\r\nbody-goes-here") {
        assert!(!format_headers(&carrier, Some("doc1")).contains("\r\n\r\n"), "{name}");
    }
}

#[test]
fn a_hostile_message_id_cannot_escape_the_multipart_boundary() {
    // With no document id the boundary falls back to the stored Message-ID.
    let block = format_headers(&mail(json!({ "messageId": "<a@b>\r\nX-Evil: 1\"", "text": "hi", "html": "<p>hi</p>" })), None);
    let start = block.find("boundary=\"").unwrap() + "boundary=\"".len();
    let boundary = &block[start..start + block[start..].find('"').unwrap()];
    assert_eq!(boundary, "boundary__a_b___X-Evil__1_");
    assert_eq!(fields_of(&block), vec!["Message-ID", "MIME-Version", "Content-Type"]);
}

#[test]
fn header_field_value_collapses_each_cr_lf_nul_run_to_a_single_space() {
    assert_eq!(header_field_value("a\r\nb\nc\rd\0e"), "a b c d e");
}

#[test]
fn header_field_value_collapses_line_and_paragraph_separators() {
    assert_eq!(header_field_value("a\u{2028}b\u{2029}c"), "a b c");
}

#[test]
fn header_field_value_leaves_a_value_with_no_line_breaks_identical() {
    assert_eq!(header_field_value("Re: \"quoted\" \\ subject"), "Re: \"quoted\" \\ subject");
}

#[test]
fn header_quoted_param_escapes_the_quote_that_would_close_the_parameter_early() {
    assert_eq!(header_quoted_param("evil.txt\"; filename=\"wanted.pdf"), "\"evil.txt\\\"; filename=\\\"wanted.pdf\"");
}

#[test]
fn header_quoted_param_escapes_a_trailing_backslash() {
    assert_eq!(header_quoted_param("report\\"), "\"report\\\\\"");
}

#[test]
fn header_quoted_param_strips_line_breaks_before_quoting() {
    assert_eq!(header_quoted_param("a\r\nb"), "\"a b\"");
}

#[test]
fn boundary_token_passes_through_the_bcharsnospace_subset() {
    assert_eq!(boundary_token("test-doc.id_1"), "test-doc.id_1");
}

#[test]
fn boundary_token_substitutes_character_for_character() {
    let hostile = "<a@b>\r\n\"";
    assert_eq!(boundary_token(hostile), "_a_b____");
    assert_eq!(boundary_token(hostile).len(), hostile.len());
}

// --- formatBodyStructure ---

fn structure(value: serde_json::Value) -> String {
    format_body_structure(&mail(value), true)
}

fn bare_structure(value: serde_json::Value) -> String {
    format_body_structure(&mail(value), false)
}

#[test]
fn body_structure_quotes_text_plain_and_base64() {
    let result = structure(json!({ "text": "Hello, World!" }));
    for token in ["\"TEXT\"", "\"PLAIN\"", "\"BASE64\""] {
        assert!(result.contains(token));
    }
}

#[test]
fn body_structure_advertises_zero_lines_for_a_zero_octet_part() {
    assert!(structure(json!({})).contains(" \"BASE64\" 0 0 "));
    assert!(structure(json!({ "text": "hi" })).contains(" \"BASE64\" 4 1 "));
}

#[test]
fn body_structure_quotes_text_html() {
    let result = structure(json!({ "html": "<p>Hello, World!</p>" }));
    assert!(result.contains("\"TEXT\"") && result.contains("\"HTML\""));
}

#[test]
fn body_structure_emits_alternative_with_no_space_between_sibling_parts() {
    let result = structure(json!({ "text": "Hello", "html": "<p>Hello</p>" }));
    assert!(result.contains("\"alternative\""));
    assert!(result.contains(")(\"TEXT\" \"HTML\""));
    assert!(!result.contains(") (\"TEXT\" \"HTML\""));
}

#[test]
fn body_structure_emits_mixed_with_no_space_between_sibling_parts() {
    let result = structure(json!({ "text": "Hello", "attachments": [pdf("test.pdf", 1024)] }));
    assert!(result.contains("\"mixed\"") && result.contains("\"application\"") && result.contains("\"pdf\""));
    assert!(result.contains(")(\"application\""));
    assert!(!result.contains(") (\"application\""));
}

#[test]
fn body_structure_includes_attachment_filename_in_disposition() {
    let result = structure(json!({ "text": "Hello", "attachments": [pdf("document.pdf", 1024)] }));
    assert!(result.contains("\"ATTACHMENT\""));
    assert!(result.contains("\"FILENAME\" \"document.pdf\""));
}

#[test]
fn body_structure_defaults_to_an_empty_quoted_text_part() {
    let result = structure(json!({}));
    assert!(result.contains("\"TEXT\"") && result.contains("\"PLAIN\""));
}

#[test]
fn body_structure_full_shape_for_text_html_and_two_attachments() {
    // The exact production shape Apple Mail on iOS 26 stuck on: quoted
    // TEXT/PLAIN/HTML, quoted BASE64, no space between sibling parts.
    let jpeg = |filename: &str, size: u64| json!({ "content": { "data": "att" }, "filename": filename, "size": size, "contentType": "image/jpeg" });
    let result = structure(json!({
        "text_octets": 27060,
        "html_octets": 11138020,
        "attachments": [jpeg("1000025304.jpg", 6162016), jpeg("1000025344.jpg", 81495)]
    }));
    let expected = concat!(
        "((",
        "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" 36080 1 NIL NIL NIL NIL)",
        "(\"TEXT\" \"HTML\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" 14850696 1 NIL NIL NIL NIL)",
        " \"alternative\" NIL NIL NIL NIL",
        ")",
        "(\"image\" \"jpeg\" (\"NAME\" \"1000025304.jpg\") NIL NIL \"BASE64\" 8216024 NIL (\"ATTACHMENT\" (\"FILENAME\" \"1000025304.jpg\")) NIL NIL)",
        "(\"image\" \"jpeg\" (\"NAME\" \"1000025344.jpg\") NIL NIL \"BASE64\" 108660 NIL (\"ATTACHMENT\" (\"FILENAME\" \"1000025344.jpg\")) NIL NIL)",
        " \"mixed\" NIL NIL NIL NIL)"
    );
    assert_eq!(result, expected);
}

#[test]
fn body_structure_derives_text_part_size_from_the_cached_octets() {
    let cached = structure(json!({ "text_octets": 30, "html_octets": 0 }));
    // ceil(30/3)*4 = 40 octets of base64, served as one unfolded line.
    assert!(cached.contains("\"BASE64\" 40 1"));
    assert!(!cached.contains("alternative"));
}

#[test]
fn body_structure_cached_shape_matches_materialized_shape() {
    let text = "line one\r\nline two\r\nline three";
    assert_eq!(structure(json!({ "text_octets": text.len(), "html_octets": 0 })), structure(json!({ "text": text })));
}

#[test]
fn body_structure_reports_the_encoded_line_count_not_the_decoded_one() {
    let text = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let encoded = encode_text(&text);
    assert_eq!(
        bare_structure(json!({ "text": text })),
        format!("(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" {} 1)", encoded.len())
    );
}

/// The size and line count a single text part's structure ends with.
fn size_and_lines(structure: &str) -> (usize, usize) {
    let tail = &structure[structure.rfind("\"BASE64\" ").unwrap() + 9..structure.len() - 1];
    let (size, lines) = tail.split_once(' ').unwrap();
    (size.parse().unwrap(), lines.parse().unwrap())
}

#[test]
fn body_structure_counts_lines_over_the_same_bytes_it_measures() {
    let html = (0..12).map(|i| format!("<p>row {i}</p>")).collect::<Vec<_>>().join("\r\n");
    assert!(html.len() > 57);
    let encoded = encode_text(&html);
    let (size, lines) = size_and_lines(&bare_structure(json!({ "html": html })));
    assert_eq!(size, encoded.len());
    assert_eq!(lines, encoded.lines().count());
}

#[test]
fn body_structure_reports_zero_octets_and_zero_lines_on_the_empty_default_part() {
    assert_eq!(bare_structure(json!({})), "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" 0 0)");
}

// The line count is a constant, and what makes the constant true is that the
// base64 the server serves is unfolded. That is pinned against what a
// `BODY[n]` fetch serves rather than against this file's own encoder.

fn long_text() -> String {
    (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\r\n")
}

fn served_body(value: serde_json::Value, part_path: &str) -> String {
    let files = AttachmentFiles::default();
    let segments = build_message_segments(&mail(value), Some("docId"), &|id| files.size(id));
    let bytes: Vec<u8> = select_part_body_segments(&segments, part_path)
        .iter()
        .flat_map(|segment| segment_bytes(segment, &files).unwrap())
        .collect();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn the_bytes_a_part_fetch_serves_carry_no_line_break() {
    let served = served_body(json!({ "text": long_text() }), "1");
    assert!(served.len() > 76);
    assert!(!served.contains('\n') && !served.contains('\r'));
}

#[test]
fn the_served_bytes_are_one_line_which_is_what_bodystructure_advertises() {
    let served = served_body(json!({ "text": long_text() }), "1");
    let (size, lines) = size_and_lines(&bare_structure(json!({ "text": long_text() })));
    assert_eq!(size, served.len());
    assert_eq!(lines, served.lines().count());
}

#[test]
fn the_measuring_encoder_agrees_with_the_serving_one() {
    assert_eq!(encode_text(&long_text()), served_body(json!({ "text": long_text() }), "1"));
}

#[test]
fn body_ext_1part_is_emitted_on_a_leaf_text_part_and_dropped_from_bare_body() {
    let head = format!("(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" {} 1", encode_text("Hello, World!").len());
    assert_eq!(structure(json!({ "text": "Hello, World!" })), format!("{head} NIL NIL NIL NIL)"));
    assert_eq!(bare_structure(json!({ "text": "Hello, World!" })), format!("{head})"));
}

#[test]
fn body_ext_1part_is_emitted_on_text_parts_nested_in_a_multipart() {
    let both = json!({ "text": "Hello", "html": "<p>Hello</p>" });
    // Two text parts and the multipart tail.
    assert_eq!(structure(both.clone()).matches("NIL NIL NIL NIL)").count(), 3);
    assert!(!bare_structure(both).contains("NIL NIL NIL NIL"));
}

fn with_attachment(content_type: &str, size: u64) -> serde_json::Value {
    json!({ "text": "Hello", "attachments": [{ "content": { "data": "att" }, "filename": "notes.txt", "size": size, "contentType": content_type }] })
}

#[test]
fn emits_the_line_count_on_a_text_attachment_and_omits_it_on_a_binary_one() {
    let size = 100u64.div_ceil(3) * 4;
    assert!(bare_structure(with_attachment("text/plain", 100))
        .contains(&format!("(\"text\" \"plain\" (\"NAME\" \"notes.txt\") NIL NIL \"BASE64\" {size} 1)")));
    assert!(bare_structure(with_attachment("application/pdf", 100))
        .contains(&format!("(\"application\" \"pdf\" (\"NAME\" \"notes.txt\") NIL NIL \"BASE64\" {size})")));
}

#[test]
fn keeps_the_line_count_ahead_of_the_body_ext_1part_tail() {
    let size = 100u64.div_ceil(3) * 4;
    assert!(structure(with_attachment("TEXT/PLAIN", 100))
        .contains(&format!("\"BASE64\" {size} 1 NIL (\"ATTACHMENT\" (\"FILENAME\" \"notes.txt\")) NIL NIL)")));
}

#[test]
fn advertises_zero_lines_for_a_zero_octet_text_attachment() {
    assert!(bare_structure(with_attachment("text/plain", 0)).contains("\"BASE64\" 0 0)"));
}

#[test]
fn non_extensible_form_drops_the_alternative_extension_tail() {
    let both = json!({ "text": "Hello", "html": "<p>Hello</p>" });
    assert!(structure(both.clone()).contains("\"alternative\" NIL NIL NIL NIL)"));
    let bare = bare_structure(both);
    assert!(bare.contains("\"alternative\")"));
    assert!(!bare.contains("\"alternative\" NIL"));
}

#[test]
fn non_extensible_form_drops_the_attachment_extension_data_and_the_mixed_tail() {
    let value = json!({ "text": "Hello", "attachments": [pdf("document.pdf", 1024)] });
    let extensible = structure(value.clone());
    let bare = bare_structure(value);
    assert!(extensible.contains("\"ATTACHMENT\""));
    assert!(extensible.contains("\"mixed\" NIL NIL NIL NIL)"));
    assert!(!bare.contains("\"ATTACHMENT\""));
    assert!(bare.contains("\"mixed\")"));
    assert!(!bare.contains("\"mixed\" NIL"));
    // The attachment part now ends at the size field.
    assert!(bare.contains("\"application\" \"pdf\""));
    assert!(bare.contains(&format!("\"BASE64\" {})", 1024u64.div_ceil(3) * 4)));
}

// --- deriveCopyMessageId: retry idempotency (#721) ---

#[test]
fn copy_id_is_the_same_for_the_same_source_and_destination() {
    assert_eq!(
        derive_copy_message_id(Some("src-msg-id"), "INBOX/accounts/foo"),
        derive_copy_message_id(Some("src-msg-id"), "INBOX/accounts/foo")
    );
}

#[test]
fn copy_id_differs_for_different_source_message_ids() {
    assert_ne!(
        derive_copy_message_id(Some("src-msg-id-A"), "INBOX/accounts/foo"),
        derive_copy_message_id(Some("src-msg-id-B"), "INBOX/accounts/foo")
    );
}

#[test]
fn copy_id_differs_for_different_destination_mailboxes() {
    assert_ne!(
        derive_copy_message_id(Some("src-msg-id"), "INBOX/accounts/foo"),
        derive_copy_message_id(Some("src-msg-id"), "Archive")
    );
}

#[test]
fn copy_id_uses_a_separator_that_prevents_boundary_confusion() {
    assert_ne!(derive_copy_message_id(Some("src"), "dest"), derive_copy_message_id(Some("srcdest"), ""));
}

#[test]
fn copy_id_falls_back_to_random_for_a_missing_source_message_id() {
    assert_ne!(derive_copy_message_id(None, "INBOX/accounts/foo"), derive_copy_message_id(None, "INBOX/accounts/foo"));
}

#[test]
fn copy_id_is_a_valid_message_id_shape() {
    let id = derive_copy_message_id(Some("src"), "dest");
    let (hash, rest) = id.split_at(16);
    assert!(hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert_eq!(rest, ".copy@server");
}

// --- quoted-string encoding ---
//
// A mis-escaped backslash and a raw CR/LF are desync bugs: either one makes
// the client's parser lose the string boundary. So these tests read the
// emitted response back with an independent RFC 3501 reader and require the
// original values to come out.

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Text(String),
    Nil,
    List(Vec<Token>),
}

fn text(value: &str) -> Token {
    Token::Text(value.to_string())
}

/// Minimal RFC 3501 §9 response reader: parenthesized lists, quoted strings
/// with `\` escapes, NIL, and bare atoms. Errors on what the grammar forbids.
struct Reader {
    input: Vec<char>,
    at: usize,
}

impl Reader {
    fn read_quoted(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let c = *self.input.get(self.at).ok_or("unterminated quoted string")?;
            match c {
                '\\' => {
                    let next = self.input.get(self.at + 1).copied();
                    if next != Some('"') && next != Some('\\') {
                        return Err(format!("illegal escape \\{next:?}"));
                    }
                    out.extend(next);
                    self.at += 2;
                }
                '"' => {
                    self.at += 1;
                    return Ok(out);
                }
                '\r' | '\n' | '\0' => return Err("CR/LF/NUL inside quoted string".to_string()),
                other => {
                    out.push(other);
                    self.at += 1;
                }
            }
        }
    }

    fn read_list(&mut self) -> Result<Vec<Token>, String> {
        self.at += 1;
        let mut items = Vec::new();
        loop {
            match self.input.get(self.at) {
                None => return Err("unterminated list".to_string()),
                Some(')') => {
                    self.at += 1;
                    return Ok(items);
                }
                Some(' ') => self.at += 1,
                Some(_) => items.push(self.read_token()?),
            }
        }
    }

    fn read_token(&mut self) -> Result<Token, String> {
        match self.input[self.at] {
            '(' => self.read_list().map(Token::List),
            '"' => self.read_quoted().map(Token::Text),
            _ => {
                let mut atom = String::new();
                while let Some(c) = self.input.get(self.at).filter(|c| **c != ' ' && **c != ')') {
                    atom.push(*c);
                    self.at += 1;
                }
                Ok(if atom == "NIL" { Token::Nil } else { Token::Text(atom) })
            }
        }
    }
}

fn read_response(input: &str) -> Result<Vec<Token>, String> {
    let mut reader = Reader { input: input.chars().collect(), at: 0 };
    let mut tokens = Vec::new();
    while reader.at < reader.input.len() {
        if reader.input[reader.at] == ' ' {
            reader.at += 1;
            continue;
        }
        tokens.push(reader.read_token()?);
    }
    Ok(tokens)
}

fn read_envelope(value: serde_json::Value) -> Vec<Token> {
    let mut parsed = read_response(&format_envelope(&mail(value))).unwrap();
    assert_eq!(parsed.len(), 1);
    let Token::List(envelope) = parsed.remove(0) else {
        panic!("envelope is not a list");
    };
    // date subject from sender reply-to to cc bcc in-reply-to message-id
    assert_eq!(envelope.len(), 10);
    envelope
}

fn one_address(name: &str, local: &str, domain: &str) -> Token {
    Token::List(vec![Token::List(vec![text(name), Token::Nil, text(local), text(domain)])])
}

fn from(name: &str, address: &str) -> serde_json::Value {
    json!({ "text": "", "value": [{ "name": name, "address": address }] })
}

#[test]
fn the_reader_rejects_an_unterminated_string_and_a_raw_line_break() {
    assert!(read_response("(\"Bob\\\" NIL)").is_err());
    assert!(read_response("(\"A\nB\")").is_err());
}

#[test]
fn the_reader_round_trips_a_legally_escaped_value() {
    assert_eq!(
        read_response("(\"a\\\\b\" \"c\\\"d\" NIL x)").unwrap(),
        vec![Token::List(vec![text("a\\b"), text("c\"d"), Token::Nil, text("x")])]
    );
}

#[test]
fn keeps_the_response_parseable_when_a_subject_ends_in_a_backslash() {
    let subject = "Re: C:\\Users\\";
    let envelope = read_envelope(json!({ "subject": subject, "messageId": "<id@example>" }));
    assert_eq!(envelope[1], text(subject));
    // If the trailing `\` escaped the terminator, the message-id slot would
    // have swallowed the tokens after it.
    assert_eq!(envelope[9], text("<id@example>"));
}

#[test]
fn keeps_the_response_parseable_when_a_display_name_ends_in_a_backslash() {
    let envelope = read_envelope(json!({ "subject": "hi", "from": from("Bob\\", "bob@example.com") }));
    assert_eq!(envelope[2], one_address("Bob\\", "bob", "example.com"));
}

#[test]
fn escapes_quotes_in_a_message_id() {
    let message_id = "<\"a\\\"b\"@example.com>";
    assert_eq!(read_envelope(json!({ "subject": "hi", "messageId": message_id }))[9], text(message_id));
}

#[test]
fn survives_an_envelope_carrying_both_hazards_at_once() {
    let envelope = read_envelope(json!({
        "subject": "Re: C:\\Users\\ path",
        "messageId": "<\"a\\\"b\"@example.com>",
        "from": from("Bob\\", "bob@example.com")
    }));
    assert_eq!(envelope[1], text("Re: C:\\Users\\ path"));
    assert_eq!(envelope[2], one_address("Bob\\", "bob", "example.com"));
    assert_eq!(envelope[9], text("<\"a\\\"b\"@example.com>"));
}

#[test]
fn emits_one_line_for_a_display_name_containing_a_line_break() {
    let value = json!({ "subject": "hi", "from": from("A\nB", "a@example.com") });
    let wire = format_envelope(&mail(value.clone()));
    assert!(!wire.contains('\n') && !wire.contains('\r'));
    assert_eq!(read_envelope(value)[2], one_address("A B", "a", "example.com"));
}

#[test]
fn collapses_crlf_runs_in_a_subject_to_a_single_space() {
    assert_eq!(read_envelope(json!({ "subject": "one\r\n\r\ntwo" }))[1], text("one two"));
}

#[test]
fn strips_nul_which_is_outside_char() {
    assert_eq!(read_envelope(json!({ "subject": "a\0b" }))[1], text("a b"));
}

/// The attachment part of a BODYSTRUCTURE, read back.
fn attachment_body(filename: &str, content_type: Option<&str>) -> Vec<Token> {
    let mut attachment = json!({ "filename": filename, "size": 12 });
    if let Some(content_type) = content_type {
        attachment["contentType"] = json!(content_type);
    }
    let wire = structure(json!({ "text": "body", "attachments": [attachment] }));
    let mut parsed = read_response(&wire).unwrap();
    assert_eq!(parsed.len(), 1);
    let Token::List(mut multipart) = parsed.remove(0) else {
        panic!("not a multipart list");
    };
    let Token::List(attachment) = multipart.remove(1) else {
        panic!("not an attachment");
    };
    attachment
}

#[test]
fn round_trips_a_filename_containing_a_quote_and_a_backslash() {
    let filename = "in\"voice\\";
    let attachment = attachment_body(filename, Some("application/pdf"));
    assert_eq!(attachment[0], text("application"));
    assert_eq!(attachment[1], text("pdf"));
    assert_eq!(attachment[2], Token::List(vec![text("NAME"), text(filename)]));
    // Extension data: MD5, disposition, language, location.
    assert_eq!(
        attachment[8],
        Token::List(vec![text("ATTACHMENT"), Token::List(vec![text("FILENAME"), text(filename)])])
    );
}

#[test]
fn emits_a_real_subtype_when_the_stored_content_type_has_no_slash() {
    let attachment = attachment_body("a.bin", Some("application"));
    assert_eq!((&attachment[0], &attachment[1]), (&text("application"), &text("octet-stream")));
}

#[test]
fn emits_a_real_media_type_when_the_stored_content_type_has_no_type_half() {
    let attachment = attachment_body("a.bin", Some("/pdf"));
    assert_eq!((&attachment[0], &attachment[1]), (&text("application"), &text("pdf")));
}

#[test]
fn routes_the_media_type_and_subtype_through_the_encoder() {
    let attachment = attachment_body("a.bin", Some("ap\"p/pd\\f"));
    assert_eq!((&attachment[0], &attachment[1]), (&text("ap\"p"), &text("pd\\f")));
}

#[test]
fn emits_the_same_media_type_in_the_part_header_as_in_bodystructure() {
    let shapes = [
        (Some("application"), "application/octet-stream"),
        (Some("/pdf"), "application/pdf"),
        (Some("application/"), "application/octet-stream"),
        (None, "application/octet-stream"),
    ];
    for (stored, expected) in shapes {
        let attachment = attachment_body("a.bin", stored);
        let (Token::Text(media_type), Token::Text(subtype)) = (&attachment[0], &attachment[1]) else {
            panic!("media type is not text");
        };
        assert_eq!(format!("{media_type}/{subtype}"), expected);
        let header = attachment_part_header_fields(&MailAttachment {
            content_type: stored.map(str::to_string),
            filename: Some("a.bin".to_string()),
            ..Default::default()
        });
        assert_eq!(header.content_type, expected);
    }
}

#[test]
fn round_trips_a_local_part_containing_a_quote_and_a_backslash() {
    let envelope = read_envelope(json!({ "subject": "hi", "from": from("X", "we\"ird\\local@example.com") }));
    assert_eq!(envelope[2], one_address("X", "we\"ird\\local", "example.com"));
}

#[test]
fn round_trips_a_domain_containing_a_quoted_special() {
    let envelope = read_envelope(json!({ "subject": "hi", "from": from("X", "user@ex\"ample.com") }));
    assert_eq!(envelope[2], one_address("X", "user", "ex\"ample.com"));
}

#[test]
fn quote_string_leaves_an_ordinary_value_untouched() {
    assert_eq!(quote_string("Hello there"), "\"Hello there\"");
    assert_eq!(format_n_string(Some("Hello")), "\"Hello\"");
}

#[test]
fn quote_string_escapes_both_quoted_specials() {
    assert_eq!(quote_string("a\"b"), "\"a\\\"b\"");
    assert_eq!(quote_string("a\\b"), "\"a\\\\b\"");
    assert_eq!(quote_string("\\"), "\"\\\\\"");
}

#[test]
fn n_string_returns_the_bare_atom_nil_for_an_absent_value() {
    assert_eq!(format_n_string(None), "NIL");
    assert_eq!(format_n_string(Some("")), "NIL");
}
