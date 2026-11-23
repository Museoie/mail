//! Octets a session's socket has delivered and its drain loop has not read
//! yet (imap/session-buffer.ts).
//!
//! A single growing byte vector cannot hold them at a bounded cost, because
//! everything the drain does to it is a whole copy:
//!
//! - **Taking a segment.** Appending copies nothing by itself, but joining on
//!   every arrival moves everything received so far once per TCP segment, so a
//!   payload arriving in `n` segments moves `O(n²)` octets before it is even
//!   complete. Segments are held on a list instead, and joined once, when a
//!   read needs them contiguous.
//!
//! - **Searching for the terminator.** The line splitter wakes on every
//!   segment, and it runs before authentication, so joining in order to
//!   search puts the same `O(n²)` back on command text that carries no CRLF.
//!   The search walks the held octets in place and joins only once it has
//!   found one.
//!
//! - **Consuming a prefix.** Copying the residual out on every read is
//!   `O(residual)` per read, so a command chaining `k` literals copies its own
//!   tail `k` times. Reads move a cursor instead, and the residual is copied
//!   out only once the cursor has passed halfway.
//!
//! Offsets and lengths are octets throughout: `{N}` counts octets.

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};

const CR: u8 = 0x0d;
const LF: u8 = 0x0a;

/// Segments held unmerged. Deferring the join trades octets moved for
/// segments retained, and the peer sets the exchange rate: one per TCP
/// segment, so a payload dribbled an octet at a time buys far more
/// bookkeeping than payload. Reaching the ceiling merges that run into a
/// single segment and leaves everything already held alone, so each octet is
/// copied into a run once and never again.
const MAX_PENDING_SEGMENTS: usize = 4096;

/// Index of the first CRLF at or after `from`.
fn find_crlf_in(bytes: &[u8], from: usize) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    bytes[from..].windows(2).position(|pair| pair == [CR, LF]).map(|at| at + from)
}

/// What a buffer has cost so far. The buffer exists to bound these, so its
/// tests assert them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BufferCost {
    /// Times held octets were copied into a new block.
    pub copies: u64,
    /// Octets those copies moved.
    pub copied_bytes: u64,
    /// Held segments the terminator search stepped onto.
    pub segment_steps: u64,
}

#[derive(Debug, Default)]
pub struct SessionBuffer {
    /// Contiguous block; its unread octets start at `cursor`.
    block: Bytes,
    cursor: usize,
    /// Segments taken since the last join, in arrival order.
    segments: VecDeque<Bytes>,
    segment_bytes: usize,
    /// Leading `segments` entries already merged into runs of their own.
    merged_runs: usize,
    /// Unread octets a search has already found to hold no terminator.
    scanned_up_to: usize,
    /// Where the search that scanned them stopped: the `segments` entry it was
    /// walking, and the unread offset that entry begins at. Carrying the octet
    /// count alone would leave every search re-deriving that position one
    /// entry at a time, `O(#segments)` per arriving segment.
    scan_index: usize,
    scan_base: usize,
    cost: BufferCost,
}

impl SessionBuffer {
    pub fn new() -> SessionBuffer {
        SessionBuffer::default()
    }

    /// Unread octets, joined and unjoined alike.
    pub fn len(&self) -> usize {
        self.block.len() - self.cursor + self.segment_bytes
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The copying and walking done since the buffer was created.
    pub fn cost(&self) -> BufferCost {
        self.cost
    }

    /// Size of the contiguous block being held, read octets included.
    pub fn retained_block_len(&self) -> usize {
        self.block.len()
    }

    pub fn push(&mut self, segment: impl Into<Bytes>) {
        let segment: Bytes = segment.into();
        if segment.is_empty() {
            return;
        }
        self.segment_bytes += segment.len();
        self.segments.push_back(segment);
        if self.segments.len() - self.merged_runs >= MAX_PENDING_SEGMENTS {
            self.merge_pending_run();
        }
    }

    pub fn clear(&mut self) {
        *self = SessionBuffer { cost: self.cost, ..SessionBuffer::default() };
    }

    /// Index of the first CRLF among the unread octets, relative to the first
    /// of them, or `None`.
    ///
    /// What a search learned carries across calls, so a prefix already known
    /// to hold no terminator is neither walked nor stepped over again per
    /// arriving segment. The resumed search starts one octet back, because the
    /// `\r` of a pair can be the last octet the previous call saw.
    pub fn index_of_crlf(&mut self) -> Option<usize> {
        match self.find_crlf(self.scanned_up_to.saturating_sub(1)) {
            None => {
                self.scanned_up_to = self.len();
                None
            }
            Some(at) => {
                self.join();
                Some(at)
            }
        }
    }

    /// UTF-8 decode of unread octets `[start, end)`, invalid sequences
    /// replaced by U+FFFD. Does not consume them.
    pub fn decode(&mut self, start: usize, end: usize) -> String {
        self.join();
        let end = (self.cursor + end).min(self.block.len());
        let start = (self.cursor + start).min(end);
        String::from_utf8_lossy(&self.block[start..end]).into_owned()
    }

    /// Up to `max` unread octets from the front, as a view of the first held
    /// segment rather than a join of all of them, so a payload streamed out
    /// segment by segment is never copied. Does not consume them, and the
    /// view stays valid after `consume`.
    pub fn head(&mut self, max: usize) -> Bytes {
        if self.block.len() == self.cursor {
            if let Some(first) = self.segments.pop_front() {
                self.block = first;
                self.cursor = 0;
                self.segment_bytes -= self.block.len();
                self.merged_runs = self.merged_runs.saturating_sub(1);
                // The resume point indexed the list this just shortened.
                self.scan_index = 0;
                self.scan_base = self.block.len();
            }
        }
        let end = self.block.len().min(self.cursor.saturating_add(max));
        self.block.slice(self.cursor..end)
    }

    pub fn consume(&mut self, count: usize) {
        // Every read joins before it returns, so a consume that follows one
        // stays inside the block. The discard path consumes without reading,
        // and it can reach past it: unjoined segments are held octets too.
        if count > self.block.len() - self.cursor {
            self.join();
        }
        self.cursor = self.cursor.saturating_add(count);
        self.scanned_up_to = self.scanned_up_to.saturating_sub(count);
        self.scan_base = self.scan_base.saturating_sub(count);
        if self.cursor >= self.block.len() {
            self.block = Bytes::new();
            self.cursor = 0;
            return;
        }
        // Past halfway the residual is no larger than what has already been
        // read, so copying it out costs no more than the octets that paid for
        // it, and it releases the block: after a message-sized literal that
        // is the whole message held for a command tail of a few dozen octets.
        if self.cursor * 2 >= self.block.len() {
            self.block = self.rebuild(false);
            self.cursor = 0;
            self.count_copy(self.block.len());
        }
    }

    /// First CRLF at or after unread offset `from`. Walks the block and then
    /// each held segment, carrying a trailing `\r` across every boundary, and
    /// copies nothing.
    fn find_crlf(&mut self, from: usize) -> Option<usize> {
        // Unread offset of a `\r` ending the piece just walked.
        let mut dangling_cr: Option<usize> = None;
        let mut index = 0;
        let mut base = self.block.len() - self.cursor;
        // A stored position at or before `from` puts every octet ahead of it,
        // the block's included, behind the resume point, so neither is walked.
        if self.scan_index > 0 && self.scan_base <= from {
            index = self.scan_index;
            base = self.scan_base;
        } else if base > from {
            if let Some(at) = find_crlf_in(&self.block, self.cursor + from) {
                return Some(at - self.cursor);
            }
            if self.block.last() == Some(&CR) {
                dangling_cr = Some(base - 1);
            }
        }
        let mut stopped_at = index;
        let mut stopped_base = base;
        while index < self.segments.len() {
            stopped_at = index;
            stopped_base = base;
            self.cost.segment_steps += 1;
            let segment = &self.segments[index];
            let end = base + segment.len();
            if end <= from {
                base = end;
                dangling_cr = None;
                index += 1;
                continue;
            }
            let start = from.saturating_sub(base);
            if start == 0 && segment.first() == Some(&LF) {
                if let Some(at) = dangling_cr {
                    return Some(at);
                }
            }
            if let Some(at) = find_crlf_in(segment, start) {
                return Some(base + at);
            }
            dangling_cr = (segment.last() == Some(&CR)).then_some(end - 1);
            base = end;
            index += 1;
        }
        self.scan_index = stopped_at;
        self.scan_base = stopped_base;
        None
    }

    fn merge_pending_run(&mut self) {
        let run: Vec<Bytes> = self.segments.drain(self.merged_runs..).collect();
        let mut merged = BytesMut::with_capacity(run.iter().map(Bytes::len).sum());
        for segment in &run {
            merged.extend_from_slice(segment);
        }
        self.count_copy(merged.len());
        self.segments.push_back(merged.freeze());
        self.merged_runs = self.segments.len();
        // The entries the run collapsed into are gone, so a resume point
        // indexing them indexes past the end of the list. Starting over costs
        // one walk of the merged entries, once per ceiling crossed.
        self.scan_index = 0;
        self.scan_base = self.block.len() - self.cursor;
    }

    fn join(&mut self) {
        if self.segment_bytes == 0 {
            return;
        }
        self.block = self.rebuild(true);
        self.cursor = 0;
        self.count_copy(self.block.len());
        self.segments.clear();
        self.segment_bytes = 0;
        self.merged_runs = 0;
        self.scan_index = 0;
        self.scan_base = self.block.len();
    }

    fn count_copy(&mut self, bytes: usize) {
        self.cost.copies += 1;
        self.cost.copied_bytes += bytes as u64;
    }

    /// The unread octets, followed by the held segments when `with_segments`,
    /// in a block of their own. Always a fresh allocation of exactly that
    /// size, so the block being replaced is released rather than pinned.
    fn rebuild(&self, with_segments: bool) -> Bytes {
        let unread = &self.block[self.cursor.min(self.block.len())..];
        let extra = if with_segments { self.segment_bytes } else { 0 };
        let mut joined = BytesMut::with_capacity(unread.len() + extra);
        joined.extend_from_slice(unread);
        if with_segments {
            for segment in &self.segments {
                joined.extend_from_slice(segment);
            }
        }
        joined.freeze()
    }
}
