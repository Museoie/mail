//! The pure half of imap/fetch-helpers.ts: which columns a FETCH needs, and
//! what each data item's response part is.
//!
//! A part that carries a body is not produced here as bytes. It comes back as
//! a [`StreamPart`]: the label, the exact octet count for the `{N}` literal,
//! and the segments to emit. The session code turns that into socket writes,
//! under its per-message stream lock and body budget. Sizes and labels are
//! decided here so that the announced length and the emitted octets come from
//! one segment list.

use super::session_format::{
    apply_partial_fetch, build_message_segments, get_body_part_headers, get_body_section_key, select_body_segments,
    select_part_body_segments, sum_body_bytes, sum_part_body_bytes, sum_segment_bytes, MessageSegment, WIRE_TRAILER,
};
use super::types::{
    js_number_string, BodyFetch, BodySection, FetchDataItem, FetchMailInput, MimeSubSection, PartialRange,
    SequenceSet,
};
use super::util::{
    format_body_structure, format_envelope, format_flags, format_headers, format_internal_date, is_domain_scoped,
    JsDate,
};

/// A body to stream as an IMAP literal: `header {length}\r\n` and then
/// exactly `length` octets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamPart {
    /// The label: `BODY[]`, `BODY[TEXT]<0>`, `RFC822`, …
    pub header: String,
    /// The octet count the literal announces.
    pub length: u64,
    /// `<docId>::<section key>`: fetches of the same body of the same message
    /// are streamed one at a time under this key.
    pub stream_key: String,
    /// The segments to emit, in order.
    pub segments: Vec<MessageSegment>,
    /// A partial fetch: emit only wire bytes `[start, start + length)` of
    /// `segments` (see `plan_partial_from_segments`).
    pub window: Option<(u64, u64)>,
    /// Emit `\r\n` after the segments. Set for a non-partial fetch; the
    /// trailer is counted in `length`.
    pub trailer: bool,
}

/// One data item of a FETCH response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchResponsePart {
    /// Written as is: `UID 7`, `FLAGS (\Seen)`, `BODY[1] NIL`.
    Simple { content: String },
    /// `header {length}\r\n` followed by `content`.
    Literal { content: String, header: String, length: u64 },
    Stream(StreamPart),
}

// ---------------------------------------------------------------------------
// Body content extraction
// ---------------------------------------------------------------------------

/// The content of a header-like body section: `BODY[HEADER]`,
/// `BODY[HEADER.FIELDS …]`, and a MIME part's `.HEADER` / `.MIME`. Each is a
/// few hundred bytes of header text and includes its own delimiting blank
/// line. `None` for every other section, and for a part the mail does not
/// have.
///
/// Deliberately not a general body reader: body-bearing sections are streamed.
pub fn get_body_content(mail: &FetchMailInput, section: &BodySection, doc_id: &str) -> Option<String> {
    match section {
        // RFC 3501 §6.4.5: a HEADER fetch includes the delimiting blank line
        // between the header block and the body.
        BodySection::Header => Some(format!("{}\r\n\r\n", format_headers(mail, Some(doc_id)))),
        BodySection::HeaderFields { not, fields } => {
            let all_headers = format_headers(mail, Some(doc_id));
            let requested: Vec<String> = fields.iter().map(|field| field.to_uppercase()).collect();
            let mut filtered: Vec<String> = Vec::new();
            for line in all_headers.split("\r\n") {
                if line.is_empty() {
                    break;
                }
                if line.starts_with([' ', '\t']) {
                    if let Some(last) = filtered.last_mut() {
                        last.push_str("\r\n");
                        last.push_str(line);
                        continue;
                    }
                }
                if let Some(colon) = line.find(':').filter(|at| *at > 0) {
                    let field_name = line[..colon].to_uppercase();
                    if requested.contains(&field_name) != *not {
                        filtered.push(line.to_string());
                    }
                }
            }
            Some(if filtered.is_empty() { "\r\n".to_string() } else { format!("{}\r\n\r\n", filtered.join("\r\n")) })
        }
        // `.HEADER` / `.MIME` return the part's MIME header fields. `.TEXT`
        // and a bare part number return the part body, which is streamed.
        BodySection::MimePart { part_number, sub_section: Some(MimeSubSection::Header | MimeSubSection::Mime) } => {
            get_body_part_headers(mail, part_number).map(|headers| format!("{headers}\r\n\r\n"))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Requested fields
// ---------------------------------------------------------------------------

/// The columns a FETCH reads, in the order they were first asked for: stored
/// mail fields by their API names (`messageId`, `replyTo`), plus the
/// synthetic `text_octets` / `html_octets` / `mail_id` / `user_id` that let a
/// body be measured and streamed without being loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestedFields(Vec<&'static str>);

impl RequestedFields {
    pub fn add(&mut self, field: &'static str) {
        if !self.0.contains(&field) {
            self.0.push(field);
        }
    }

    fn add_all(&mut self, fields: &[&'static str]) {
        for field in fields {
            self.add(field);
        }
    }

    pub fn contains(&self, field: &str) -> bool {
        self.0.contains(&field)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[&'static str] {
        &self.0
    }
}

const HEADER_FIELDS: [&str; 8] = ["subject", "from", "to", "cc", "bcc", "replyTo", "date", "messageId"];
const LAZY_BODY_FIELDS: [&str; 4] = ["text_octets", "html_octets", "mail_id", "user_id"];

pub fn get_requested_fields(data_items: &[FetchDataItem]) -> RequestedFields {
    let mut fields = RequestedFields::default();
    fields.add("uid");

    let full = BodyFetch { peek: true, section: BodySection::Full, partial: None };
    for item in data_items {
        match item {
            FetchDataItem::Envelope => fields.add_all(&HEADER_FIELDS),
            FetchDataItem::Flags => fields.add_all(&["read", "saved", "deleted", "draft", "answered"]),
            // BODYSTRUCTURE needs the size of each text/html part: the
            // pre-measured octet counts, not the body columns.
            FetchDataItem::BodyStructure { .. } => fields.add_all(&["text_octets", "html_octets", "attachments"]),
            FetchDataItem::Body(body) => add_body_fields(body, &mut fields),
            FetchDataItem::InternalDate => fields.add("date"),
            FetchDataItem::Rfc822Size => {
                add_body_fields(&full, &mut fields);
                fields.add("rfc822_size");
            }
            // RFC822* alias the matching BODY[...] sections (§6.4.5).
            FetchDataItem::Rfc822 => add_body_fields(&full, &mut fields),
            FetchDataItem::Rfc822Header => add_body_fields(
                &BodyFetch { peek: true, section: BodySection::Header, partial: None },
                &mut fields,
            ),
            FetchDataItem::Rfc822Text => add_body_fields(
                &BodyFetch { peek: false, section: BodySection::Text, partial: None },
                &mut fields,
            ),
            FetchDataItem::Uid | FetchDataItem::Modseq => {}
        }
    }

    fields
}

pub fn add_body_fields(body_fetch: &BodyFetch, fields: &mut RequestedFields) {
    match &body_fetch.section {
        // Full and partial BODY[] alike are streamed from segments, the body
        // columns read in chunks at emit time.
        BodySection::Full => {
            fields.add_all(&LAZY_BODY_FIELDS);
            fields.add_all(&HEADER_FIELDS);
            fields.add("attachments");
        }
        // BODY[TEXT] and a MIME part stream the same way; the header columns
        // are needed for the multipart framing inside the body.
        BodySection::Text | BodySection::MimePart { .. } => {
            fields.add_all(&LAZY_BODY_FIELDS);
            fields.add("attachments");
            fields.add_all(&HEADER_FIELDS);
        }
        BodySection::Header => fields.add_all(&HEADER_FIELDS),
        BodySection::HeaderFields { not, fields: requested } => {
            if *not {
                fields.add_all(&HEADER_FIELDS);
            } else {
                for field in requested {
                    let mapped = match field.to_uppercase().as_str() {
                        "FROM" => "from",
                        "TO" => "to",
                        "CC" => "cc",
                        "BCC" => "bcc",
                        "REPLY-TO" => "replyTo",
                        "SUBJECT" => "subject",
                        "DATE" => "date",
                        "MESSAGE-ID" => "messageId",
                        _ => continue,
                    };
                    fields.add(mapped);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// convertSequenceSet
// ---------------------------------------------------------------------------

/// A range with both ends, in ascending order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NumberRange {
    pub start: f64,
    pub end: f64,
}

/// The ranges of a sequence set, each put in ascending order: RFC 3501 §9
/// makes a range order-independent, so `3:1` is `1:3` and `*:1` is `1:*`.
pub fn convert_sequence_set(sequence_set: &SequenceSet) -> Vec<NumberRange> {
    sequence_set
        .ranges
        .iter()
        .map(|range| {
            let end = range.end.unwrap_or(range.start);
            NumberRange { start: range.start.min(end), end: range.start.max(end) }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

/// Sections whose content already ends in its own delimiting blank line:
/// HEADER, HEADER.FIELDS, and a MIME part's `.HEADER` / `.MIME`.
pub fn is_header_like_section(section: &BodySection) -> bool {
    matches!(
        section,
        BodySection::Header
            | BodySection::HeaderFields { .. }
            | BodySection::MimePart { sub_section: Some(MimeSubSection::Header | MimeSubSection::Mime), .. }
    )
}

fn nil(section_key: &str) -> FetchResponsePart {
    FetchResponsePart::Simple { content: format!("{section_key} NIL") }
}

/// `BODY[<section>]<start.length>` over an already-selected segment subset
/// of `addressable_bytes` octets.
///
/// RFC 3501 §6.4.5: a start past the end is a vacuous range, answered NIL
/// rather than with an empty literal. Otherwise the length is clamped to
/// what is there, so the literal announces the count actually emitted. The
/// response label is `BODY[<section>]<start>`: the origin octet only.
fn stream_partial_subset(
    segments: Vec<MessageSegment>,
    addressable_bytes: u64,
    partial: &PartialRange,
    section_key: &str,
    stream_key: String,
) -> FetchResponsePart {
    if partial.start >= addressable_bytes as f64 {
        return nil(section_key);
    }
    let start = partial.start as u64;
    let emitted_length = partial.length.min((addressable_bytes - start) as f64) as u64;
    FetchResponsePart::Stream(StreamPart {
        header: format!("{section_key}<{}>", js_number_string(partial.start)),
        length: emitted_length,
        stream_key,
        segments,
        window: Some((start, emitted_length)),
        trailer: false,
    })
}

/// The response part of a `BODY[...]` fetch, or `None` when the mail has no
/// such part and the item is left out of the response.
///
/// `key_override` labels the part for RFC822 / RFC822.HEADER / RFC822.TEXT,
/// which reuse the BODY builders. `attachment_file_size` is as for
/// `build_message_segments`.
pub fn build_body_response_part(
    mail: &FetchMailInput,
    body_fetch: &BodyFetch,
    doc_id: &str,
    key_override: Option<&str>,
    attachment_file_size: &dyn Fn(&str) -> Option<u64>,
) -> Option<FetchResponsePart> {
    let section = &body_fetch.section;
    let partial = body_fetch.partial.as_ref();
    let section_key = match key_override {
        Some(key) => key.to_string(),
        None => get_body_section_key(section),
    };
    let stream_key = format!("{doc_id}::{section_key}");
    let whole = |segments: Vec<MessageSegment>, length: u64| {
        FetchResponsePart::Stream(StreamPart {
            header: section_key.clone(),
            length,
            stream_key: stream_key.clone(),
            segments,
            window: None,
            trailer: true,
        })
    };

    match section {
        BodySection::Full => {
            let segments = build_message_segments(mail, Some(doc_id), attachment_file_size);
            // The total includes the two-byte wire trailer the non-partial
            // response appends. A partial fetch addresses the RFC 2822
            // serialization only, so the trailer is not part of its range.
            let total = sum_segment_bytes(&segments);
            return Some(match partial {
                Some(partial) => {
                    stream_partial_subset(segments, total - WIRE_TRAILER, partial, &section_key, stream_key.clone())
                }
                None => whole(segments, total),
            });
        }
        BodySection::Text => {
            // RFC 3501 §6.4.5: the text body of the message, omitting the header.
            let segments = build_message_segments(mail, Some(doc_id), attachment_file_size);
            let body_bytes = sum_body_bytes(&segments);
            if body_bytes == 0 {
                return Some(nil(&section_key));
            }
            let body = select_body_segments(&segments);
            return Some(match partial {
                Some(partial) => stream_partial_subset(body, body_bytes, partial, &section_key, stream_key.clone()),
                None => whole(body, body_bytes + WIRE_TRAILER),
            });
        }
        // A bare part number and `.TEXT` both return the part's body.
        BodySection::MimePart { part_number, sub_section: None | Some(MimeSubSection::Text) } => {
            let segments = build_message_segments(mail, Some(doc_id), attachment_file_size);
            let part_bytes = sum_part_body_bytes(&segments, part_number);
            if part_bytes == 0 {
                // A body segment exists only for a part with content, so the
                // mail has no such part.
                return None;
            }
            let part = select_part_body_segments(&segments, part_number);
            return Some(match partial {
                Some(partial) => stream_partial_subset(part, part_bytes, partial, &section_key, stream_key.clone()),
                None => whole(part, part_bytes + WIRE_TRAILER),
            });
        }
        _ => {}
    }

    // Only header-like sections reach here; they are small and built in hand.
    let content = get_body_content(mail, section, doc_id)?;
    let mut header = section_key.clone();
    let mut final_content = content;
    let mut length = final_content.len() as u64;

    if final_content.is_empty() || partial.is_some_and(|partial| partial.start >= length as f64) {
        return Some(nil(&section_key));
    }

    if let Some(partial) = partial {
        let end = partial.start + partial.length;
        if 0.0 < partial.start || end < length as f64 {
            final_content = apply_partial_fetch(&final_content, partial);
            length = final_content.len() as u64;
        }
        // The response marker is the origin octet only (RFC 3501 §7.4.2);
        // `<start.length>` is request grammar. A partial fetch returns
        // exactly the requested substring, with no trailing CRLF.
        header.push_str(&format!("<{}>", js_number_string(partial.start)));
    }

    Some(FetchResponsePart::Literal { content: final_content, header, length })
}

/// One data item's response part, and the message size it had to compute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltPart {
    /// `None` when the item produces nothing: a part the mail does not have,
    /// or MODSEQ, which `build_fetch_response` emits once at the end.
    pub part: Option<FetchResponsePart>,
    /// Set when RFC822.SIZE was computed because the row had no cached
    /// `rfc822_size`: the caller persists it for the next fetch.
    pub computed_rfc822_size: Option<u64>,
}

impl From<Option<FetchResponsePart>> for BuiltPart {
    fn from(part: Option<FetchResponsePart>) -> BuiltPart {
        BuiltPart { part, computed_rfc822_size: None }
    }
}

fn simple(content: String) -> BuiltPart {
    BuiltPart { part: Some(FetchResponsePart::Simple { content }), computed_rfc822_size: None }
}

/// The response part of one FETCH data item.
///
/// `selected_mailbox` decides which of the mail's two UIDs is reported.
pub fn build_fetch_response_part(
    mail: &FetchMailInput,
    item: &FetchDataItem,
    doc_id: &str,
    selected_mailbox: &str,
    attachment_file_size: &dyn Fn(&str) -> Option<u64>,
) -> BuiltPart {
    let body = |section: BodySection, peek: bool, key: &str| {
        let fetch = BodyFetch { peek, section, partial: None };
        BuiltPart::from(build_body_response_part(mail, &fetch, doc_id, Some(key), attachment_file_size))
    };
    match item {
        FetchDataItem::Uid => {
            let uid = mail.uid.unwrap_or_default();
            let uid = if is_domain_scoped(selected_mailbox) { uid.domain } else { uid.account };
            simple(format!("UID {}", js_number_string(uid)))
        }
        FetchDataItem::Flags => simple(format!("FLAGS ({})", format_flags(mail).join(" "))),
        FetchDataItem::InternalDate => {
            let date = match mail.date.as_deref().filter(|date| !date.is_empty()) {
                Some(date) => JsDate::parse(date),
                None => JsDate::now(),
            };
            simple(format!("INTERNALDATE \"{}\"", format_internal_date(&date)))
        }
        FetchDataItem::Rfc822Size => {
            if let Some(size) = mail.rfc822_size {
                return simple(format!("RFC822.SIZE {}", js_number_string(size)));
            }
            let size = sum_segment_bytes(&build_message_segments(mail, Some(doc_id), attachment_file_size));
            BuiltPart {
                part: Some(FetchResponsePart::Simple { content: format!("RFC822.SIZE {size}") }),
                computed_rfc822_size: Some(size),
            }
        }
        FetchDataItem::Envelope => simple(format!("ENVELOPE {}", format_envelope(mail))),
        // `extensible: false` is the bare `BODY` data item: the same
        // structure without the extension data, labelled `BODY`.
        FetchDataItem::BodyStructure { extensible } => {
            let label = if *extensible { "BODYSTRUCTURE" } else { "BODY" };
            simple(format!("{label} {}", format_body_structure(mail, *extensible)))
        }
        FetchDataItem::Body(fetch) => {
            build_body_response_part(mail, fetch, doc_id, None, attachment_file_size).into()
        }
        // RFC 3501 §6.4.5 aliases: RFC822 is BODY[], RFC822.HEADER is
        // BODY[HEADER], RFC822.TEXT is BODY[TEXT], each keeping its own label.
        FetchDataItem::Rfc822 => body(BodySection::Full, false, "RFC822"),
        FetchDataItem::Rfc822Header => body(BodySection::Header, true, "RFC822.HEADER"),
        FetchDataItem::Rfc822Text => body(BodySection::Text, false, "RFC822.TEXT"),
        FetchDataItem::Modseq => None.into(),
    }
}

/// The parts of one message's FETCH response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    pub parts: Vec<FetchResponsePart>,
    /// As [`BuiltPart::computed_rfc822_size`].
    pub computed_rfc822_size: Option<u64>,
}

/// Builds the parts of one message's FETCH response, in request order.
///
/// A UID FETCH leads with `UID <uid>`. MODSEQ is emitted once, last, when
/// the client asked for it or CONDSTORE is enabled (RFC 4551 §3.3.2), and
/// only for a row that carries a mod-sequence.
#[allow(clippy::too_many_arguments)]
pub fn build_fetch_response(
    mail: &FetchMailInput,
    data_items: &[FetchDataItem],
    doc_id: &str,
    uid: f64,
    is_uid_fetch: bool,
    selected_mailbox: &str,
    condstore_enabled: bool,
    attachment_file_size: &dyn Fn(&str) -> Option<u64>,
) -> FetchResponse {
    let mut parts: Vec<FetchResponsePart> = Vec::new();
    let mut computed_rfc822_size = None;

    if is_uid_fetch {
        parts.push(FetchResponsePart::Simple { content: format!("UID {}", js_number_string(uid)) });
    }

    for item in data_items {
        if matches!(item, FetchDataItem::Uid) && is_uid_fetch {
            continue;
        }
        let built = build_fetch_response_part(mail, item, doc_id, selected_mailbox, attachment_file_size);
        computed_rfc822_size = built.computed_rfc822_size.or(computed_rfc822_size);
        parts.extend(built.part);
    }

    let modseq_requested = data_items.iter().any(|item| matches!(item, FetchDataItem::Modseq));
    if let Some(modseq) = mail.modseq.filter(|_| condstore_enabled || modseq_requested) {
        parts.push(FetchResponsePart::Simple { content: format!("MODSEQ ({})", js_number_string(modseq)) });
    }

    FetchResponse { parts, computed_rfc822_size }
}

/// The line that opens a literal on the wire: `header {length}\r\n`.
pub fn literal_prefix(header: &str, length: u64) -> String {
    format!("{header} {{{length}}}\r\n")
}
