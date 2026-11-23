//! The RFC 822 size a new `mails` row is stamped with: the exact byte count
//! an IMAP `BODY[]` fetch of the row serializes to, which is also what
//! `RFC822.SIZE` reports.
//!
//! The IMAP serializer owns this arithmetic; a save needs only the total, for
//! the shape a save has in hand (body octet counts rather than loaded
//! bodies), so that is all this module computes.
//!
//! ```ignore
//! let size = compute_full_message_size(&MailForSize { message_id, date, mail_id, text_octets, .. })?;
//! ```

// Mirrors imap::session_utils::compute_full_message_size, with what it calls:
// imap::util::{format_headers, header_field_value, header_quoted_param,
// media_type_parts, attachment_part_header_fields, boundary_token} and
// mails::util::get_attachment_file_path; unify when both land.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::js;

/// Where attachment files are stored, relative to the working directory.
const ATTACHMENT_FOLDER: &str = "./attachments";

#[cfg(test)]
thread_local! {
    /// Where a test keeps its attachment files, instead of the working directory.
    pub(crate) static ATTACHMENT_FOLDER_OVERRIDE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

fn attachment_folder() -> String {
    #[cfg(test)]
    if let Some(folder) = ATTACHMENT_FOLDER_OVERRIDE.with(|folder| folder.borrow().clone()) {
        return folder;
    }
    ATTACHMENT_FOLDER.to_string()
}

/// Raw byte count emitted for an attachment whose file cannot be read.
const MISSING_ATTACHMENT_NOTICE: &str = "Attachment data not found";

/// Trailing CRLF the wire response appends after the body serialization.
const WIRE_TRAILER: u64 = 2;

/// The fields of a mail its serialized size depends on.
#[derive(Debug, Clone, Default)]
pub(crate) struct MailForSize<'a> {
    pub message_id: &'a str,
    pub date: &'a str,
    pub subject: Option<&'a str>,
    /// UTF-8 length of the text and html bodies.
    pub text_octets: usize,
    pub html_octets: usize,
    /// The row's id, which the MIME boundaries are derived from.
    pub mail_id: &'a str,
    /// The `attachments` document as it is stored.
    pub attachments: Option<&'a Value>,
    pub from_text: Option<&'a str>,
    pub to_text: Option<&'a str>,
    pub cc_text: Option<&'a str>,
    pub bcc_text: Option<&'a str>,
    pub reply_to_text: Option<&'a str>,
}

/// Byte length of the unfolded base64 encoding of `raw_bytes` octets.
fn base64_byte_len(raw_bytes: u64) -> u64 {
    raw_bytes.div_ceil(3) * 4
}

/// A stored value as it may appear in a header: line breaks, NUL and the
/// Unicode line separators collapse to one space per run, so no stored value
/// can start a header line of its own.
fn header_field_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_run = false;
    for c in value.chars() {
        if matches!(c, '\r' | '\n' | '\0' | '\u{2028}' | '\u{2029}') {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

fn header_quoted_param(value: &str) -> String {
    let mut out = String::from("\"");
    for c in header_field_value(value).chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The MIME boundary token for a mail's stable id: anything outside RFC 2046
/// `bcharsnospace` maps to `_`, one for each UTF-16 code unit.
fn boundary_token(stable_id: &str) -> String {
    stable_id
        .encode_utf16()
        .map(|unit| match char::from_u32(unit as u32) {
            Some(c) if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') => c,
            _ => '_',
        })
        .collect()
}

/// Whether JavaScript reads the value as true in a condition.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A string field of a stored attachment, `None` where the fallback applies.
/// A set value that is not a string cannot be serialized into a header.
fn attachment_text<'a>(attachment: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match attachment.get(field) {
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text)),
        Some(other) if truthy(other) => bail!("attachment {field} is not a string"),
        _ => Ok(None),
    }
}

/// The `Content-Type` value and quoted `filename` parameter of one
/// attachment part header.
fn attachment_part_header_fields(attachment: &Value) -> Result<(String, String)> {
    if attachment.is_null() {
        bail!("attachment is null");
    }
    let content_type = attachment_text(attachment, "contentType")?.unwrap_or("application/octet-stream");
    let mut halves = content_type.split('/');
    let media_type = halves.next().filter(|half| !half.is_empty()).unwrap_or("application");
    let subtype = halves.next().filter(|half| !half.is_empty()).unwrap_or("octet-stream");
    let filename = attachment_text(attachment, "filename")?.unwrap_or("unnamed");
    Ok((header_field_value(&format!("{media_type}/{subtype}")), header_quoted_param(filename)))
}

/// Raw size of an attachment as it will be emitted: the size of its file,
/// or of the notice that stands in for a file that is missing.
fn attachment_raw_size(attachment: &Value) -> Result<u64> {
    let content = match attachment.get("content") {
        Some(content) if !content.is_null() => content,
        _ => bail!("attachment has no content"),
    };
    let notice = MISSING_ATTACHMENT_NOTICE.len() as u64;
    let Some(data_id) = content.get("data").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
        return Ok(notice);
    };
    match std::fs::metadata(format!("{}/{data_id}", attachment_folder())) {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        _ => Ok(notice),
    }
}

/// The attachments a mail serializes, empty for a document that lists none.
fn attachment_list(attachments: Option<&Value>) -> Result<&[Value]> {
    match attachments {
        Some(Value::Array(items)) => Ok(items),
        Some(Value::String(text)) if !text.is_empty() => bail!("attachments is not a list"),
        Some(Value::Object(object)) if object.get("length").and_then(Value::as_f64).is_some_and(|n| n > 0.0) => {
            bail!("attachments is not a list")
        }
        _ => Ok(&[]),
    }
}

/// The top-level header block, without the blank line that ends it.
fn format_headers(mail: &MailForSize, has_text: bool, has_html: bool, has_attachments: bool) -> String {
    let mut headers: Vec<String> = Vec::new();
    if !mail.message_id.is_empty() {
        headers.push(format!("Message-ID: {}", header_field_value(mail.message_id)));
    }
    if !mail.date.is_empty() {
        let date = js::date_parse(mail.date)
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|date| js::utc_string(&date))
            .unwrap_or_else(|| "Invalid Date".to_string());
        headers.push(format!("Date: {date}"));
    }
    for (name, text) in [
        ("From", mail.from_text),
        ("To", mail.to_text),
        ("Cc", mail.cc_text),
        ("Bcc", mail.bcc_text),
        ("Reply-To", mail.reply_to_text),
        ("Subject", mail.subject),
    ] {
        if let Some(text) = text.filter(|value| !value.is_empty()) {
            headers.push(format!("{name}: {}", header_field_value(text)));
        }
    }
    headers.push("MIME-Version: 1.0".to_string());

    let boundary = format!("boundary_{}", boundary_token(mail.mail_id));
    if has_attachments {
        headers.push(format!("Content-Type: multipart/mixed; boundary=\"{boundary}\""));
    } else if has_text && has_html {
        headers.push(format!("Content-Type: multipart/alternative; boundary=\"{boundary}\""));
    } else if has_html {
        headers.push("Content-Type: text/html; charset=utf-8".to_string());
        headers.push("Content-Transfer-Encoding: base64".to_string());
    } else {
        headers.push("Content-Type: text/plain; charset=utf-8".to_string());
        headers.push("Content-Transfer-Encoding: base64".to_string());
    }
    headers.join("\r\n")
}

/// Exact wire byte count of the mail's RFC 822 serialization, including the
/// CRLF the wire response appends. Costs one `stat` per attachment and reads
/// no body.
pub(crate) fn compute_full_message_size(mail: &MailForSize) -> Result<i64> {
    let has_text = mail.text_octets > 0;
    let has_html = mail.html_octets > 0;
    let attachments = attachment_list(mail.attachments)?;
    let has_attachments = !attachments.is_empty();

    let headers = format_headers(mail, has_text, has_html, has_attachments);
    let token = boundary_token(mail.mail_id);
    let boundary = format!("boundary_{token}");
    let alt_boundary = format!("alt_{token}");

    let literal = |value: &str| value.len() as u64;
    // One base64 body part: boundary, part headers, encoded payload, CRLF.
    let body_part = |delimiter: &str, content_type: &str, octets: usize| {
        literal(&format!("--{delimiter}\r\n"))
            + literal(&format!("Content-Type: {content_type}; charset=utf-8\r\n"))
            + literal("Content-Transfer-Encoding: base64\r\n\r\n")
            + base64_byte_len(octets as u64)
            + literal("\r\n")
    };

    // The header block and the blank line after it open every shape.
    let mut bytes = literal(&headers) + literal("\r\n\r\n");
    if !has_attachments {
        match (has_text, has_html) {
            (false, false) => {}
            (true, false) => bytes += base64_byte_len(mail.text_octets as u64) + literal("\r\n"),
            (false, true) => bytes += base64_byte_len(mail.html_octets as u64) + literal("\r\n"),
            (true, true) => {
                bytes += body_part(&boundary, "text/plain", mail.text_octets)
                    + body_part(&boundary, "text/html", mail.html_octets)
                    + literal(&format!("--{boundary}--"));
            }
        }
    } else {
        if has_text && has_html {
            bytes += literal(&format!("--{boundary}\r\n"))
                + literal(&format!("Content-Type: multipart/alternative; boundary=\"{alt_boundary}\"\r\n\r\n"))
                + body_part(&alt_boundary, "text/plain", mail.text_octets)
                + body_part(&alt_boundary, "text/html", mail.html_octets)
                + literal(&format!("--{alt_boundary}--\r\n"));
        } else if has_text {
            bytes += body_part(&boundary, "text/plain", mail.text_octets);
        } else if has_html {
            bytes += body_part(&boundary, "text/html", mail.html_octets);
        }
        for attachment in attachments {
            let (content_type, filename_param) = attachment_part_header_fields(attachment)?;
            bytes += literal(&format!("--{boundary}\r\n"))
                + literal(&format!("Content-Type: {content_type}\r\n"))
                + literal("Content-Transfer-Encoding: base64\r\n")
                + literal(&format!("Content-Disposition: attachment; filename={filename_param}\r\n\r\n"))
                + base64_byte_len(attachment_raw_size(attachment)?)
                + literal("\r\n");
        }
        bytes += literal(&format!("--{boundary}--"));
    }
    Ok((bytes + WIRE_TRAILER) as i64)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const MAIL_ID: &str = "0b0e1a2c-3d4f-4a5b-8c6d-7e8f9a0b1c2d";

    fn mail<'a>() -> MailForSize<'a> {
        MailForSize {
            message_id: "<m1@example.com>",
            date: "2024-01-02T03:04:05.000Z",
            mail_id: MAIL_ID,
            ..Default::default()
        }
    }

    #[test]
    fn a_mail_with_no_body_is_its_header_block() {
        let headers = "Message-ID: <m1@example.com>\r\nDate: Tue, 02 Jan 2024 03:04:05 GMT\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64";
        assert_eq!(compute_full_message_size(&mail()).unwrap(), (headers.len() + 4 + 2) as i64);
    }

    #[test]
    fn stored_line_breaks_cannot_start_a_header_line() {
        assert_eq!(header_field_value("a\r\n\0b\u{2028}\u{2029}c"), "a b c");
        assert_eq!(header_quoted_param("a\"b\\c\n"), "\"a\\\"b\\\\c \"");
    }

    #[test]
    fn the_boundary_token_keeps_its_code_unit_count() {
        assert_eq!(boundary_token("a b<c>@d.e-f_g"), "a_b_c__d.e-f_g");
        assert_eq!(boundary_token("a😀b"), "a__b");
    }

    #[test]
    fn a_missing_attachment_file_measures_as_the_notice() {
        let attachment = json!({ "content": { "data": "no-such-file-anywhere" }, "filename": "a.txt" });
        assert_eq!(attachment_raw_size(&attachment).unwrap(), 25);
        assert_eq!(attachment_raw_size(&json!({ "content": { "data": "" } })).unwrap(), 25);
        assert!(attachment_raw_size(&json!({ "filename": "a.txt" })).is_err());
    }

    #[test]
    fn attachment_headers_fall_back_on_missing_halves() {
        let fields = |value: Value| attachment_part_header_fields(&value).unwrap();
        assert_eq!(fields(json!({})), ("application/octet-stream".to_string(), "\"unnamed\"".to_string()));
        assert_eq!(fields(json!({ "contentType": "application" })).0, "application/octet-stream");
        assert_eq!(fields(json!({ "contentType": "/pdf" })).0, "application/pdf");
        assert_eq!(fields(json!({ "contentType": "text/" })).0, "text/octet-stream");
        assert_eq!(fields(json!({ "contentType": "a/b/c", "filename": "x\"y" })), ("a/b".to_string(), "\"x\\\"y\"".to_string()));
    }
}
