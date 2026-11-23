//! Writing a FETCH response (`writeFetchResponse` in imap/fetch-helpers.ts)
//! and streaming the message bodies it carries (the generators of
//! imap/session-utils.ts).
//!
//! A body part arrives from `fetch_format` as a [`StreamPart`]: the literal's
//! announced length and the segments that make it up. It is emitted here
//! chunk by chunk, each chunk written at the pace the client reads, under
//! the per-message stream lock and a body-budget slot, both taken when the
//! first chunk is about to be produced and given back when the part is done
//! or abandoned. Nothing is materialized whole: attachments are read from
//! disk in slices and database bodies in chunks.
//!
//! Every emitter produces exactly the octets the literal announced. The one
//! exception is a database fault halfway through a body, which ends the
//! response there, as a thrown generator does in the Bun server.

use std::io::SeekFrom;

use anyhow::Result;
use base64::Engine;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::body_budget::acquire_body_budget;
use super::fetch_format::{literal_prefix, FetchResponsePart, StreamPart};
use super::session::ImapSession;
use super::session_format::{
    base64_byte_len, base64_seek, literal_chunks, plan_partial_from_segments, segment_byte_length, Base64Encoder,
    MessageSegment, StreamWindow, MISSING_ATTACHMENT_NOTICE, SLICE_RAW_BYTES,
};
use super::stream_mutex::acquire_stream_mutex;
use crate::logger;
use crate::mails::util::get_attachment_file_path;
use crate::postgres::repositories::mails::imap::{
    pg_byte_chunks, pg_text_chunks, TextColumn, PG_TEXT_CHUNK_BYTES, PG_TEXT_CHUNK_CHARS,
};

/// Writes one message's FETCH response: `* <seq> FETCH (<parts>)`.
pub async fn write_fetch_response(session: &ImapSession, seq: usize, parts: &[FetchResponsePart]) -> Result<()> {
    session.write(&format!("* {seq} FETCH ("));
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            session.write(" ");
        }
        match part {
            FetchResponsePart::Simple { content } => {
                session.write(content);
            }
            FetchResponsePart::Literal { content, header, length } => {
                session.write(&literal_prefix(header, *length));
                session.write(content);
            }
            FetchResponsePart::Stream(stream) => {
                session.write(&literal_prefix(&stream.header, stream.length));
                emit_stream_part(session, stream).await?;
            }
        }
    }
    session.write(")\r\n");
    Ok(())
}

/// Where emitted chunks go: the client, through an optional window of the
/// part's octets.
struct Sink<'a> {
    session: &'a ImapSession,
    window: Option<StreamWindow>,
    /// The socket went away: stop producing.
    gone: bool,
}

impl<'a> Sink<'a> {
    fn new(session: &'a ImapSession, window: Option<StreamWindow>) -> Sink<'a> {
        Sink { session, window, gone: false }
    }

    fn is_open(&self) -> bool {
        let socket = self.session.socket();
        !socket.is_destroyed() && socket.is_writable()
    }

    /// Emits what of `chunk` falls in the window. False when nothing more
    /// should be produced: the window is full or the socket is gone.
    async fn push(&mut self, chunk: &[u8]) -> bool {
        if self.gone {
            return false;
        }
        let piece = match self.window.as_mut() {
            Some(window) => window.slice(chunk),
            None => Some(chunk),
        };
        if let Some(piece) = piece.filter(|piece| !piece.is_empty()) {
            if !self.is_open() {
                self.gone = true;
                return false;
            }
            #[cfg(test)]
            probe(|probe| {
                probe.pushes.push(piece.len());
                probe.command_slots.push(super::command_budget::command_budget_in_flight());
            });
            self.session.write_chunked(Bytes::copy_from_slice(piece)).await;
        }
        !self.window.as_ref().is_some_and(StreamWindow::is_done)
    }
}

/// Emits a body part: the whole segment list (and the CRLF trailer), or the
/// window of a partial fetch.
async fn emit_stream_part(session: &ImapSession, part: &StreamPart) -> Result<()> {
    // A socket that is already gone takes nothing: no lock, no slot.
    let socket = session.socket();
    if socket.is_destroyed() || !socket.is_writable() {
        return Ok(());
    }
    // The key outside the budget, so a waiter for a key that is streaming
    // does not hold a body slot while it waits.
    let _key = acquire_stream_mutex(&part.stream_key).await;
    let _slot = acquire_body_budget().await;

    match part.window {
        None => {
            let mut sink = Sink::new(session, None);
            for segment in &part.segments {
                emit_segment(&mut sink, segment).await?;
                if sink.gone {
                    return Ok(());
                }
            }
            if part.trailer {
                sink.push(b"\r\n").await;
            }
        }
        Some((start, length)) => {
            for slice in plan_partial_from_segments(&part.segments, start, length) {
                let segment = &part.segments[slice.index];
                emit_segment_window(session, segment, slice.skip, slice.take).await?;
            }
        }
    }
    Ok(())
}

/// Emits one whole segment.
async fn emit_segment(sink: &mut Sink<'_>, segment: &MessageSegment) -> Result<()> {
    match segment {
        MessageSegment::Literal { value, .. } => {
            for chunk in literal_chunks(value) {
                if !sink.push(chunk).await {
                    break;
                }
            }
        }
        MessageSegment::Base64 { source, .. } => emit_base64(sink, source).await,
        MessageSegment::Attachment { data_id, raw_size, filename, .. } => {
            emit_attachment(sink, data_id, *raw_size, filename.as_deref(), 0).await;
        }
        MessageSegment::LazyText { source, mail_id, user_id, .. } => {
            let mut chunks =
                std::pin::pin!(pg_text_chunks(mail_id, user_id, TextColumn::from(*source), PG_TEXT_CHUNK_CHARS));
            let mut encoder = Base64Encoder::default();
            while let Some(chunk) = chunks.next().await {
                let encoded = encoder.push(chunk?.as_bytes());
                if !encoded.is_empty() && !sink.push(encoded.as_bytes()).await {
                    return Ok(());
                }
            }
            let tail = encoder.finish();
            if !tail.is_empty() {
                sink.push(tail.as_bytes()).await;
            }
        }
    }
    Ok(())
}

/// Emits wire octets `[skip, skip + take)` of one segment. A database body
/// is read from the byte its window starts at, and an attachment from the
/// file offset, so a deep partial fetch reads only its window.
async fn emit_segment_window(session: &ImapSession, segment: &MessageSegment, skip: u64, take: u64) -> Result<()> {
    match segment {
        MessageSegment::LazyText { source, mail_id, user_id, .. } => {
            // Base64 maps 3 raw bytes to 4 characters: start at the
            // 4-character boundary at or before the skip and trim the rest.
            let seek = base64_seek(skip);
            let mut sink = Sink::new(session, Some(StreamWindow::new(seek.residual, take)));
            let start_byte = usize::try_from(seek.raw_offset + 1).unwrap_or(usize::MAX);
            let mut chunks =
                std::pin::pin!(pg_byte_chunks(mail_id, user_id, TextColumn::from(*source), start_byte, PG_TEXT_CHUNK_BYTES));
            let mut encoder = Base64Encoder::default();
            while let Some(chunk) = chunks.next().await {
                let encoded = encoder.push(&chunk?);
                if !encoded.is_empty() && !sink.push(encoded.as_bytes()).await {
                    return Ok(());
                }
            }
            let tail = encoder.finish();
            if !tail.is_empty() {
                sink.push(tail.as_bytes()).await;
            }
        }
        MessageSegment::Attachment { data_id, raw_size, filename, .. } => {
            let aligned = skip / 4 * 4;
            let mut sink = Sink::new(session, Some(StreamWindow::new(skip - aligned, take)));
            emit_attachment(&mut sink, data_id, *raw_size, filename.as_deref(), aligned).await;
        }
        other => {
            let mut sink = Sink::new(session, Some(StreamWindow::new(skip, take)));
            emit_segment(&mut sink, other).await?;
        }
    }
    debug_assert!(skip + take <= segment_byte_length(segment));
    Ok(())
}

/// `source` base64-encoded, in slices of `SLICE_RAW_BYTES` raw bytes.
async fn emit_base64(sink: &mut Sink<'_>, source: &str) {
    for slice in source.as_bytes().chunks(SLICE_RAW_BYTES) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(slice);
        if !sink.push(encoded.as_bytes()).await {
            return;
        }
    }
}

/// Reads up to `buffer.len()` bytes, short only at the end of the file, so
/// every slice but the last is a whole multiple of 3 and its base64 carries
/// no padding.
async fn fill(file: &mut tokio::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

/// An attachment base64-encoded, read from its file in slices; with
/// `start_base64` (a multiple of 4) the output starts that far in, the file
/// read from the matching offset.
///
/// Emits exactly `base64_byte_len(raw_size) - start_base64` octets, the part
/// of the announced length that lies past `start_base64`. If the file no
/// longer agrees with the size measured when the length was announced, the
/// shortfall is padded with newlines, which decoders ignore: the client sees
/// one damaged attachment instead of a desynchronized connection. A file
/// that cannot be read emits the base64 of a notice instead.
async fn emit_attachment(sink: &mut Sink<'_>, data_id: &str, raw_size: u64, filename: Option<&str>, start_base64: u64) {
    let advertised = base64_byte_len(raw_size);
    let start_raw = start_base64 / 4 * 3;
    let mut emitted = start_base64;
    let mut stopped = false;

    // Emits `encoded`, trimmed to what is left of the announced length.
    macro_rules! emit {
        ($encoded:expr) => {{
            let encoded: &[u8] = $encoded;
            let remaining = advertised.saturating_sub(emitted);
            if remaining > 0 && !stopped {
                let usable = &encoded[..encoded.len().min(remaining as usize)];
                emitted += usable.len() as u64;
                if !sink.push(usable).await {
                    stopped = true;
                }
            }
        }};
    }

    let read: std::io::Result<()> = async {
        let mut file = tokio::fs::File::open(get_attachment_file_path(Some(data_id))).await?;
        file.seek(SeekFrom::Start(start_raw)).await?;
        #[cfg(test)]
        probe(|probe| probe.attachment_reads_from.push(start_raw));
        let mut scratch = vec![0u8; SLICE_RAW_BYTES];
        loop {
            let read = fill(&mut file, &mut scratch).await?;
            if read == 0 {
                break;
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(&scratch[..read]);
            emit!(encoded.as_bytes());
            if emitted >= advertised || stopped {
                break;
            }
        }
        Ok(())
    }
    .await;

    if let Err(error) = read {
        logger::warn(
            "Attachment unreadable while streaming BODY[]",
            json!({ "component": "imap", "filename": filename, "err": error.to_string() }),
        );
        let notice = base64::engine::general_purpose::STANDARD.encode(MISSING_ATTACHMENT_NOTICE);
        let from_start = emitted == start_base64;
        let notice = if from_start { notice.get(start_base64 as usize..).unwrap_or_default() } else { notice.as_str() };
        emit!(notice.as_bytes());
    }

    if emitted < advertised && !stopped {
        logger::error(
            "Attachment shorter than measured size; padding to hold the literal",
            json!({ "component": "imap", "filename": filename, "advertised": advertised, "emitted": emitted }),
        );
        let padding = vec![b'\n'; (advertised - emitted) as usize];
        sink.push(&padding).await;
    }
}

/// What the emitters did, for tests: the size of each chunk handed to the
/// socket, how many command-budget slots were taken as it was, and the file
/// offset each attachment read started at.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Probe {
    pub(crate) pushes: Vec<usize>,
    pub(crate) command_slots: Vec<usize>,
    pub(crate) attachment_reads_from: Vec<u64>,
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static PROBE: std::cell::RefCell<Probe>;
}

#[cfg(test)]
fn probe(record: impl FnOnce(&mut Probe)) {
    let _ = PROBE.try_with(|probe| record(&mut probe.borrow_mut()));
}

#[cfg(test)]
#[path = "fetch_stream_tests.rs"]
mod tests;
