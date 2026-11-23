//! The pure half of imap/session-utils.ts: section keys, the RFC 822
//! serialization of a stored mail as a list of segments, the exact sizes of
//! those segments, and the arithmetic of serving a byte range of them.
//!
//! Nothing here reads a file, a socket or the database. A stored mail is
//! never built as one string to be served: it is described as
//! [`MessageSegment`]s, measured, and then emitted segment by segment by the
//! session code, which owns the readers. Where a function needs a fact about
//! an attachment file, the caller passes a closure that supplies it.

use base64::Engine;
use serde_json::json;

use super::types::{
    json_string, BodySection, FetchDataItem, FetchMailInput, MailAttachment, PartialRange,
};
use super::util::{attachment_part_header_fields, boundary_token, format_headers, js_trim};
use crate::logger;

/// Applies a partial fetch range to content: octets `[start, start + length)`
/// of its UTF-8, decoded again. A cut through a character leaves U+FFFD.
pub fn apply_partial_fetch(content: &str, partial: &PartialRange) -> String {
    let bytes = content.as_bytes();
    let total = bytes.len() as f64;
    if partial.start >= total {
        return String::new();
    }
    let end = (partial.start + partial.length).min(total);
    if end <= partial.start {
        return String::new();
    }
    let start = partial.start.max(0.0) as usize;
    String::from_utf8_lossy(&bytes[start..end as usize]).into_owned()
}

/// The IMAP body section key a response labels its data with: `BODY[…]`.
pub fn get_body_section_key(section: &BodySection) -> String {
    match section {
        BodySection::Full => "BODY[]".to_string(),
        BodySection::Text => "BODY[TEXT]".to_string(),
        BodySection::Header => "BODY[HEADER]".to_string(),
        BodySection::MimePart { part_number, sub_section } => match sub_section {
            Some(sub) => format!("BODY[{part_number}.{}]", sub.as_str()),
            None => format!("BODY[{part_number}]"),
        },
        BodySection::HeaderFields { not, fields } => {
            let field_list = fields.join(" ");
            if *not {
                format!("BODY[HEADER.FIELDS.NOT ({field_list})]")
            } else {
                format!("BODY[HEADER.FIELDS ({field_list})]")
            }
        }
    }
}

/// Whether any fetch data item marks the message as read.
///
/// RFC822 is BODY[] and RFC822.TEXT is BODY[TEXT], both non-peek, so they set
/// \Seen like a non-peek BODY fetch. RFC822.HEADER is BODY.PEEK[HEADER] and
/// never does (RFC 3501 §6.4.5).
pub fn should_mark_as_read(data_items: &[FetchDataItem]) -> bool {
    data_items.iter().any(|item| match item {
        FetchDataItem::Body(body) => !body.peek,
        FetchDataItem::Rfc822 | FetchDataItem::Rfc822Text => true,
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// Message segments
// ---------------------------------------------------------------------------

/// What a literal segment is: the top-level RFC 2822 header block, or MIME
/// framing (a boundary, a part header, a closing delimiter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentRole {
    Headers,
    Framing,
}

impl SegmentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentRole::Headers => "headers",
            SegmentRole::Framing => "framing",
        }
    }
}

/// Which body column a segment reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySource {
    Text,
    Html,
}

impl BodySource {
    pub fn as_str(self) -> &'static str {
        match self {
            BodySource::Text => "text",
            BodySource::Html => "html",
        }
    }
}

/// One piece of the RFC 822 serialization.
///
/// `part_path`, where set, is the RFC 3501 part number the body belongs to
/// (`1`, `1.2`, `2`, …), which is how `BODY[<part>]` selects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageSegment {
    /// Emitted verbatim. `BODY[TEXT]` skips the one whose role is `Headers`.
    Literal { value: String, role: Option<SegmentRole> },
    /// `source` base64-encoded, unfolded.
    Base64 { source: String, part_path: Option<String> },
    /// An attachment file, base64-encoded, `raw_size` octets as resolved when
    /// the segment was built.
    Attachment { data_id: String, raw_size: u64, filename: Option<String>, part_path: Option<String> },
    /// A body column streamed out of the database and base64-encoded;
    /// `byte_length` is its `octet_length()`.
    LazyText { source: BodySource, mail_id: String, user_id: String, byte_length: u64, part_path: Option<String> },
}

impl MessageSegment {
    pub fn part_path(&self) -> Option<&str> {
        match self {
            MessageSegment::Literal { .. } => None,
            MessageSegment::Base64 { part_path, .. }
            | MessageSegment::Attachment { part_path, .. }
            | MessageSegment::LazyText { part_path, .. } => part_path.as_deref(),
        }
    }

    /// `JSON.stringify(segment)` of the Bun server's segment object.
    pub fn to_json_string(&self) -> String {
        let with_path = |mut out: String, part_path: &Option<String>| {
            if let Some(path) = part_path {
                out.push_str(&format!(",\"partPath\":{}", json_string(path)));
            }
            out.push('}');
            out
        };
        match self {
            MessageSegment::Literal { value, role } => {
                let mut out = format!("{{\"kind\":\"literal\",\"value\":{}", json_string(value));
                if let Some(role) = role {
                    out.push_str(&format!(",\"role\":\"{}\"", role.as_str()));
                }
                out.push('}');
                out
            }
            MessageSegment::Base64 { source, part_path } => {
                with_path(format!("{{\"kind\":\"base64\",\"source\":{}", json_string(source)), part_path)
            }
            MessageSegment::Attachment { data_id, raw_size, filename, part_path } => {
                let mut out = format!("{{\"kind\":\"attachment\",\"dataId\":{},\"rawSize\":{raw_size}", json_string(data_id));
                if let Some(filename) = filename {
                    out.push_str(&format!(",\"filename\":{}", json_string(filename)));
                }
                with_path(out, part_path)
            }
            MessageSegment::LazyText { source, mail_id, user_id, byte_length, part_path } => with_path(
                format!(
                    "{{\"kind\":\"lazy-text\",\"source\":\"{}\",\"mail_id\":{},\"user_id\":{},\"byteLength\":{byte_length}",
                    source.as_str(),
                    json_string(mail_id),
                    json_string(user_id)
                ),
                part_path,
            ),
        }
    }
}

/// Length of unfolded base64 of `raw_bytes` octets: `4 * ceil(n / 3)`. Exact,
/// since the server never folds its base64.
pub fn base64_byte_len(raw_bytes: u64) -> u64 {
    raw_bytes.div_ceil(3) * 4
}

/// What is emitted, base64-encoded, for an attachment whose file cannot be
/// read. Measured and emitted through the same constant so the two agree.
pub const MISSING_ATTACHMENT_NOTICE: &str = "Attachment data not found";

/// Raw size of an attachment as it will be emitted, given the size of its
/// file on disk (`None` when it is missing or not a regular file).
///
/// The file is what gets streamed, so it is the file that is measured, not
/// the `size` stored on the mail row: that is what the sender declared.
pub fn resolve_attachment_raw_size(file_size: Option<u64>) -> u64 {
    file_size.unwrap_or(MISSING_ATTACHMENT_NOTICE.len() as u64)
}

/// The stable boundary strings a build for this mail generates, derived from
/// the same `doc_id || message_id` the header block used.
fn boundaries_for(mail: &FetchMailInput, doc_id: Option<&str>) -> (String, String) {
    let stable_id = doc_id
        .filter(|id| !id.is_empty())
        .or(mail.message_id.as_deref().filter(|id| !id.is_empty()))
        .unwrap_or("default");
    let token = boundary_token(stable_id);
    (format!("boundary_{token}"), format!("alt_{token}"))
}

/// `headers.replace(/^Content-Type: [^\r\n]*/m, replacement)`: rewrites the
/// first `Content-Type` line. Anchored to a line start so a stored subject
/// containing that text is never the one rewritten.
fn rewrite_content_type(headers: &str, replacement: &str) -> String {
    const NEEDLE: &str = "Content-Type: ";
    let is_line_start = |at: usize| {
        headers[..at].chars().next_back().is_none_or(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
    };
    let Some(at) = headers.match_indices(NEEDLE).map(|(at, _)| at).find(|at| is_line_start(*at)) else {
        return headers.to_string();
    };
    let value_start = at + NEEDLE.len();
    let line_end = headers[value_start..].find(['\r', '\n']).map_or(headers.len(), |offset| value_start + offset);
    format!("{}{replacement}{}", &headers[..at], &headers[line_end..])
}

/// True iff the four synthetic fields are all set and neither body string is loaded.
fn wants_lazy_bodies(mail: &FetchMailInput) -> bool {
    mail.text_octets.is_some()
        && mail.html_octets.is_some()
        && mail.mail_id.is_some()
        && mail.user_id.is_some()
        && mail.text.is_none()
        && mail.html.is_none()
}

/// Does this mail have a text / an html body part?
fn resolve_body_presence(mail: &FetchMailInput) -> (bool, bool) {
    if wants_lazy_bodies(mail) {
        return (mail.text_octets.is_some_and(|n| n > 0.0), mail.html_octets.is_some_and(|n| n > 0.0));
    }
    let loaded = |body: &Option<String>| body.as_deref().is_some_and(|text| !js_trim(text).is_empty());
    (loaded(&mail.text), loaded(&mail.html))
}

fn attachments(mail: &FetchMailInput) -> &[MailAttachment] {
    mail.attachments.as_deref().unwrap_or(&[])
}

/// The RFC 822 serialization of `mail`, as an ordered segment list.
///
/// Attachment bodies are referenced by id and size, never read:
/// `attachment_file_size` returns the size of an attachment's file on disk,
/// or `None` when it is missing or not a regular file, and is asked once per
/// attachment.
///
/// When the row carries the four synthetic fields and no body strings, the
/// text and html parts are `LazyText` segments, read from the database in
/// chunks when the message is emitted.
///
/// ```ignore
/// let segments = build_message_segments(&mail, Some(&doc_id), &|id| std::fs::metadata(path_of(id)).ok().filter(|m| m.is_file()).map(|m| m.len()));
/// ```
pub fn build_message_segments(
    mail: &FetchMailInput,
    doc_id: Option<&str>,
    attachment_file_size: &dyn Fn(&str) -> Option<u64>,
) -> Vec<MessageSegment> {
    let headers = format_headers(mail, doc_id);
    let is_lazy = wants_lazy_bodies(mail);
    let (has_text, has_html) = resolve_body_presence(mail);
    let has_attachments = !attachments(mail).is_empty();

    let mut segments: Vec<MessageSegment> = Vec::new();
    let literal = |segments: &mut Vec<MessageSegment>, value: String, role: SegmentRole| {
        segments.push(MessageSegment::Literal { value, role: Some(role) });
    };
    let framing = |segments: &mut Vec<MessageSegment>, value: &str| {
        segments.push(MessageSegment::Literal { value: value.to_string(), role: Some(SegmentRole::Framing) });
    };
    // A lazy segment in lazy mode, a base64 segment otherwise. Both encode
    // to identical wire bytes for the same column.
    let push_body = |segments: &mut Vec<MessageSegment>, which: BodySource, part_path: &str| {
        let part_path = Some(part_path.to_string());
        if is_lazy {
            let octets = match which {
                BodySource::Text => mail.text_octets,
                BodySource::Html => mail.html_octets,
            };
            segments.push(MessageSegment::LazyText {
                source: which,
                mail_id: mail.mail_id.clone().unwrap_or_default(),
                user_id: mail.user_id.clone().unwrap_or_default(),
                byte_length: octets.unwrap_or(0.0).max(0.0) as u64,
                part_path,
            });
        } else {
            let source = match which {
                BodySource::Text => mail.text.clone(),
                BodySource::Html => mail.html.clone(),
            };
            segments.push(MessageSegment::Base64 { source: source.unwrap_or_default(), part_path });
        }
    };

    if !has_text && !has_html && !has_attachments {
        literal(&mut segments, format!("{headers}\r\n\r\n"), SegmentRole::Headers);
        return segments;
    }
    if !has_attachments && has_text != has_html {
        literal(&mut segments, format!("{headers}\r\n\r\n"), SegmentRole::Headers);
        push_body(&mut segments, if has_text { BodySource::Text } else { BodySource::Html }, "1");
        framing(&mut segments, "\r\n");
        return segments;
    }

    if doc_id.is_none_or(str::is_empty) {
        logger::warn(
            "docId is missing in buildMessageSegments, falling back to messageId",
            json!({ "component": "imap", "messageId": mail.message_id }),
        );
    }
    let (boundary, alt_boundary) = boundaries_for(mail, doc_id);

    // One base64 body part: boundary, part headers, encoded payload, CRLF.
    let body_part = |segments: &mut Vec<MessageSegment>, delimiter: &str, content_type: &str, which: BodySource, part_path: &str| {
        framing(segments, &format!("--{delimiter}\r\n"));
        framing(segments, &format!("Content-Type: {content_type}; charset=utf-8\r\n"));
        framing(segments, "Content-Transfer-Encoding: base64\r\n\r\n");
        push_body(segments, which, part_path);
        framing(segments, "\r\n");
    };

    if has_text && has_html && !has_attachments {
        // multipart/alternative: top-level parts 1 (text) and 2 (html).
        let rewritten = rewrite_content_type(
            &headers,
            &format!("Content-Type: multipart/alternative; boundary=\"{boundary}\""),
        );
        literal(&mut segments, format!("{rewritten}\r\n\r\n"), SegmentRole::Headers);
        body_part(&mut segments, &boundary, "text/plain", BodySource::Text, "1");
        body_part(&mut segments, &boundary, "text/html", BodySource::Html, "2");
        framing(&mut segments, &format!("--{boundary}--"));
        return segments;
    }

    // Attachments: multipart/mixed. With both text and html, the alternative
    // wrapper is part 1 with sub-parts 1.1 and 1.2; with one of them, that
    // body is part 1. Attachments follow from part 2, or from part 1 when
    // there is no body at all.
    let rewritten = rewrite_content_type(&headers, &format!("Content-Type: multipart/mixed; boundary=\"{boundary}\""));
    literal(&mut segments, format!("{rewritten}\r\n\r\n"), SegmentRole::Headers);

    let mut next_attachment_part = 1;
    if has_text && has_html {
        framing(&mut segments, &format!("--{boundary}\r\n"));
        framing(&mut segments, &format!("Content-Type: multipart/alternative; boundary=\"{alt_boundary}\"\r\n\r\n"));
        body_part(&mut segments, &alt_boundary, "text/plain", BodySource::Text, "1.1");
        body_part(&mut segments, &alt_boundary, "text/html", BodySource::Html, "1.2");
        framing(&mut segments, &format!("--{alt_boundary}--\r\n"));
        next_attachment_part = 2;
    } else if has_text {
        body_part(&mut segments, &boundary, "text/plain", BodySource::Text, "1");
        next_attachment_part = 2;
    } else if has_html {
        body_part(&mut segments, &boundary, "text/html", BodySource::Html, "1");
        next_attachment_part = 2;
    }

    for attachment in attachments(mail) {
        let fields = attachment_part_header_fields(attachment);
        let data_id = attachment.content.as_ref().map(|content| content.data.clone()).unwrap_or_default();
        framing(&mut segments, &format!("--{boundary}\r\n"));
        framing(&mut segments, &format!("Content-Type: {}\r\n", fields.content_type));
        framing(&mut segments, "Content-Transfer-Encoding: base64\r\n");
        framing(
            &mut segments,
            &format!("Content-Disposition: attachment; filename={}\r\n\r\n", fields.filename_param),
        );
        segments.push(MessageSegment::Attachment {
            raw_size: resolve_attachment_raw_size(attachment_file_size(&data_id)),
            data_id,
            filename: attachment.filename.clone(),
            part_path: Some(next_attachment_part.to_string()),
        });
        framing(&mut segments, "\r\n");
        next_attachment_part += 1;
    }
    framing(&mut segments, &format!("--{boundary}--"));
    segments
}

/// Exact wire byte count of one segment. No I/O.
pub fn segment_byte_length(segment: &MessageSegment) -> u64 {
    match segment {
        MessageSegment::Literal { value, .. } => value.len() as u64,
        MessageSegment::Base64 { source, .. } => base64_byte_len(source.len() as u64),
        MessageSegment::Attachment { raw_size, .. } => base64_byte_len(*raw_size),
        // The raw count is `octet_length()`, measured against the same value
        // the stream will pull.
        MessageSegment::LazyText { byte_length, .. } => base64_byte_len(*byte_length),
    }
}

/// The trailing `\r\n` the wire response appends after the body
/// serialization. `RFC822.SIZE` includes it so that SIZE equals the `{N}` of
/// `BODY[]`; a partial fetch addresses RFC 2822 bytes only and excludes it.
pub const WIRE_TRAILER: u64 = 2;

/// The exact wire byte count of a segment list, trailer included. The size
/// and the stream must derive from the same list, built once.
pub fn sum_segment_bytes(segments: &[MessageSegment]) -> u64 {
    sum_segment_list_bytes(segments) + WIRE_TRAILER
}

/// The wire byte count the `{N}` literal advertises for `BODY[]`, and the
/// value `RFC822.SIZE` reports.
pub fn compute_full_message_size(
    mail: &FetchMailInput,
    doc_id: Option<&str>,
    attachment_file_size: &dyn Fn(&str) -> Option<u64>,
) -> u64 {
    sum_segment_bytes(&build_message_segments(mail, doc_id, attachment_file_size))
}

/// Raw bytes per base64 slice. Divisible by 3: base64 pads any input whose
/// length is not a multiple of 3, so any other slice size would put `=`
/// padding at every slice boundary. 48 KiB raw encodes to 64 KiB, one socket
/// write.
pub const SLICE_RAW_BYTES: usize = 48 * 1024;

/// Bytes per chunk of an emitted literal: one socket write.
pub const LITERAL_CHUNK_BYTES: usize = 64 * 1024;

/// `value` as byte-bounded chunks, so one huge literal is not one write.
pub fn literal_chunks(value: &str) -> impl Iterator<Item = &[u8]> {
    value.as_bytes().chunks(LITERAL_CHUNK_BYTES)
}

/// `source` base64-encoded in slices of `SLICE_RAW_BYTES` raw bytes. The
/// slices concatenate to the base64 of the whole.
pub fn base64_chunks(source: &str) -> impl Iterator<Item = String> + '_ {
    source
        .as_bytes()
        .chunks(SLICE_RAW_BYTES)
        .map(|slice| base64::engine::general_purpose::STANDARD.encode(slice))
}

/// Base64 of a byte stream that arrives in chunks of any size.
///
/// Intermediate output is cut at multiples of 3 raw bytes, so the pieces
/// concatenate to the base64 of the whole stream; the 0–2 bytes left over
/// carry to the next chunk, and only `finish` emits padding.
///
/// ```ignore
/// let mut encoder = Base64Encoder::default();
/// while let Some(chunk) = rows.next().await { socket.write(encoder.push(&chunk).as_bytes()).await?; }
/// socket.write(encoder.finish().as_bytes()).await?;
/// ```
#[derive(Debug, Default)]
pub struct Base64Encoder {
    carry: Vec<u8>,
}

impl Base64Encoder {
    /// The base64 of everything received so far that is a whole 3-byte group.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.extend_from_slice(chunk);
        let aligned = bytes.len() - bytes.len() % 3;
        self.carry = bytes.split_off(aligned);
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    }

    /// The base64 of what is left, padded.
    pub fn finish(self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.carry)
    }
}

/// A window `[skip, skip + take)` over a stream of chunks.
///
/// ```ignore
/// let mut window = StreamWindow::new(skip, take);
/// for chunk in chunks { if let Some(part) = window.slice(&chunk) { emit(part); } if window.is_done() { break; } }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamWindow {
    skip: u64,
    take: u64,
}

impl StreamWindow {
    pub fn new(skip: u64, take: u64) -> StreamWindow {
        StreamWindow { skip, take }
    }

    /// Nothing more will be emitted: stop pulling the source.
    pub fn is_done(&self) -> bool {
        self.take == 0
    }

    /// The part of the next chunk that falls inside the window.
    pub fn slice<'a>(&mut self, chunk: &'a [u8]) -> Option<&'a [u8]> {
        if self.take == 0 {
            return None;
        }
        let start = self.skip.min(chunk.len() as u64);
        self.skip -= start;
        let available = chunk.len() as u64 - start;
        let taken = available.min(self.take);
        if taken == 0 {
            return None;
        }
        self.take -= taken;
        Some(&chunk[start as usize..(start + taken) as usize])
    }
}

/// Where to start reading a base64-encoded source so as to land on base64
/// offset `skip`: base64 maps 3 raw bytes to 4 characters, so the read seeks
/// to the 4-character boundary at or before `skip` and trims the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Base64Seek {
    /// The 4-aligned base64 offset the read starts at.
    pub aligned_base64: u64,
    /// The raw byte offset that corresponds to it, 0-based.
    pub raw_offset: u64,
    /// Leading base64 characters (0–3) to drop from what is read.
    pub residual: u64,
}

pub fn base64_seek(skip: u64) -> Base64Seek {
    let aligned_base64 = skip / 4 * 4;
    Base64Seek { aligned_base64, raw_offset: aligned_base64 / 4 * 3, residual: skip - aligned_base64 }
}

/// The part of one segment a byte range of a segment list covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentSlice {
    /// Index into the segment list.
    pub index: usize,
    /// Wire bytes of the segment to skip.
    pub skip: u64,
    /// Wire bytes of the segment to emit after the skip.
    pub take: u64,
}

/// Which segments, and which bytes of each, make up wire bytes
/// `[start, start + length)` of a segment list.
///
/// The list must be the one the range was clamped against: the walk is by
/// cumulative byte offset.
pub fn plan_partial_from_segments(segments: &[MessageSegment], start: u64, length: u64) -> Vec<SegmentSlice> {
    let mut slices = Vec::new();
    if length == 0 {
        return slices;
    }
    let end = start.saturating_add(length);
    let mut cumulative: u64 = 0;
    for (index, segment) in segments.iter().enumerate() {
        if cumulative >= end {
            break;
        }
        let segment_bytes = segment_byte_length(segment);
        let segment_start = cumulative;
        let segment_end = cumulative + segment_bytes;
        cumulative = segment_end;
        if segment_end <= start {
            continue;
        }
        let skip = start.saturating_sub(segment_start);
        let take = segment_bytes.min(end - segment_start).saturating_sub(skip);
        if take == 0 {
            continue;
        }
        slices.push(SegmentSlice { index, skip, take });
    }
    slices
}

/// The `BODY[TEXT]` subset of a segment list: everything except the header
/// literal. The byte sum, the full stream and the partial stream all select
/// through this, so a range is never clamped against a different subset than
/// the one it walks.
pub fn select_body_segments(segments: &[MessageSegment]) -> Vec<MessageSegment> {
    segments
        .iter()
        .filter(|segment| !matches!(segment, MessageSegment::Literal { role: Some(SegmentRole::Headers), .. }))
        .cloned()
        .collect()
}

/// Byte length of the `BODY[TEXT]` payload.
pub fn sum_body_bytes(segments: &[MessageSegment]) -> u64 {
    sum_segment_list_bytes(&select_body_segments(segments))
}

/// The `BODY[<part_path>]` subset of a segment list: the body segments of
/// that part, without its MIME headers.
pub fn select_part_body_segments(segments: &[MessageSegment], part_path: &str) -> Vec<MessageSegment> {
    segments.iter().filter(|segment| segment.part_path() == Some(part_path)).cloned().collect()
}

/// Byte length of the `BODY[<part_path>]` payload.
pub fn sum_part_body_bytes(segments: &[MessageSegment], part_path: &str) -> u64 {
    sum_segment_list_bytes(&select_part_body_segments(segments, part_path))
}

/// Sums a segment list verbatim, with no `WIRE_TRAILER`.
pub fn sum_segment_list_bytes(segments: &[MessageSegment]) -> u64 {
    segments.iter().map(segment_byte_length).sum()
}

/// The base64 an attachment segment emits, given the file's bytes (`None`
/// when it cannot be read): exactly `base64_byte_len(raw_size)` characters.
/// A file that disagrees with the measured size is truncated or padded with
/// newlines, which decoders ignore, so the literal length always holds.
pub fn attachment_base64(raw_size: u64, file: Option<&[u8]>) -> String {
    let advertised = base64_byte_len(raw_size) as usize;
    let mut encoded = base64::engine::general_purpose::STANDARD
        .encode(file.unwrap_or(MISSING_ATTACHMENT_NOTICE.as_bytes()));
    if encoded.len() > advertised {
        encoded.truncate(advertised);
    } else {
        encoded.push_str(&"\n".repeat(advertised - encoded.len()));
    }
    encoded
}

/// Why a message could not be built as one string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildMessageError {
    #[error(
        "buildFullMessage: cannot materialize a lazy-text segment. Caller must project mail.text / mail.html instead of the text_octets / html_octets streaming fields."
    )]
    LazyText,
}

/// The whole RFC 822 serialization as one string, for a mail whose body
/// strings are loaded. `read_attachment` returns an attachment file's bytes,
/// or `None` when it cannot be read.
///
/// The server streams messages instead of building them; this is the same
/// bytes for callers that need them in hand.
pub fn build_full_message(
    mail: &FetchMailInput,
    doc_id: Option<&str>,
    attachment_file_size: &dyn Fn(&str) -> Option<u64>,
    read_attachment: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Result<String, BuildMessageError> {
    let mut message = String::new();
    for segment in build_message_segments(mail, doc_id, attachment_file_size) {
        match segment {
            MessageSegment::Literal { value, .. } => message.push_str(&value),
            MessageSegment::Base64 { source, .. } => {
                message.push_str(&base64::engine::general_purpose::STANDARD.encode(source.as_bytes()));
            }
            MessageSegment::LazyText { .. } => return Err(BuildMessageError::LazyText),
            MessageSegment::Attachment { data_id, raw_size, .. } => {
                message.push_str(&attachment_base64(raw_size, read_attachment(&data_id).as_deref()));
            }
        }
    }
    Ok(message)
}

/// `parseInt(text, 10)` for a part number piece: its leading integer.
fn leading_integer(text: &str) -> Option<i64> {
    let text = text.trim_start_matches(crate::imap::util::js_is_whitespace);
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    digits[..end].parse::<i64>().ok().map(|value| sign * value)
}

/// The parts a part number names: the top-level part, and the sub-part when
/// the body is an alternative nested in a mixed message.
fn part_indexes(part_num: &str) -> (Option<i64>, Option<i64>) {
    let mut pieces = part_num.split('.');
    let main = pieces.next().and_then(leading_integer);
    // An absent or empty second piece reads as sub-part 1.
    let sub = match pieces.next() {
        Some(piece) if !piece.is_empty() => leading_integer(piece),
        _ => Some(1),
    };
    (main, sub)
}

/// Which loaded body a part number resolves to, or the attachment index.
enum ResolvedPart {
    Text,
    Html,
    Attachment(usize),
}

fn resolve_part(has_text: bool, has_html: bool, attachment_count: usize, part_num: &str) -> Option<ResolvedPart> {
    let (main, sub) = part_indexes(part_num);
    if attachment_count == 0 && !has_text && !has_html {
        return None;
    }
    if attachment_count == 0 {
        return match (has_text, has_html, main) {
            // multipart/alternative
            (true, true, Some(1)) => Some(ResolvedPart::Text),
            (true, true, Some(2)) => Some(ResolvedPart::Html),
            (true, false, Some(1)) => Some(ResolvedPart::Text),
            (false, true, Some(1)) => Some(ResolvedPart::Html),
            _ => None,
        };
    }

    // multipart/mixed: the first part is the body, possibly a nested alternative.
    if main == Some(1) {
        match (has_text, has_html) {
            (true, true) => match sub {
                Some(1) => return Some(ResolvedPart::Text),
                Some(2) => return Some(ResolvedPart::Html),
                _ => {}
            },
            (true, false) => return Some(ResolvedPart::Text),
            (false, true) => return Some(ResolvedPart::Html),
            (false, false) => {}
        }
    }

    // Subsequent parts are attachments.
    let index = main? - 2;
    (index >= 0 && (index as usize) < attachment_count).then_some(ResolvedPart::Attachment(index as usize))
}

/// The base64 body of one MIME part of a mail whose body strings are loaded,
/// or `None` when the mail has no such part. `read_attachment` returns an
/// attachment file's bytes, or `None` when it is missing.
pub fn get_body_part(
    mail: &FetchMailInput,
    part_num: &str,
    read_attachment: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Option<String> {
    let loaded = |body: &Option<String>| body.as_deref().is_some_and(|text| !js_trim(text).is_empty());
    let encode = |body: &Option<String>| {
        base64::engine::general_purpose::STANDARD.encode(body.as_deref().unwrap_or("").as_bytes())
    };
    let list = attachments(mail);
    match resolve_part(loaded(&mail.text), loaded(&mail.html), list.len(), part_num)? {
        ResolvedPart::Text => Some(encode(&mail.text)),
        ResolvedPart::Html => Some(encode(&mail.html)),
        ResolvedPart::Attachment(index) => {
            let data_id = list[index].content.as_ref().map(|content| content.data.as_str()).unwrap_or("");
            read_attachment(data_id).map(|data| base64::engine::general_purpose::STANDARD.encode(data))
        }
    }
}

/// The MIME header block of one part, without its terminating blank line, or
/// `None` when the mail has no such part.
pub fn get_body_part_headers(mail: &FetchMailInput, part_num: &str) -> Option<String> {
    const TEXT_HEADERS: &str = "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64";
    const HTML_HEADERS: &str = "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64";

    let (has_text, has_html) = resolve_body_presence(mail);
    let list = attachments(mail);
    match resolve_part(has_text, has_html, list.len(), part_num)? {
        ResolvedPart::Text => Some(TEXT_HEADERS.to_string()),
        ResolvedPart::Html => Some(HTML_HEADERS.to_string()),
        ResolvedPart::Attachment(index) => {
            let fields = attachment_part_header_fields(&list[index]);
            Some(format!(
                "Content-Type: {}\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename={}",
                fields.content_type, fields.filename_param
            ))
        }
    }
}
