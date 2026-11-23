//! Sequence numbers and UIDs (imap/sequence-resolver.ts).
//!
//! Per RFC 3501, sequence numbers are contiguous 1..N and are rebuilt whenever
//! the mailbox changes (SELECT, EXPUNGE, APPEND, …).
//!
//! UIDs here are what the database holds, `i64`. Numbers out of a parsed
//! sequence set are `f64`, as the parser produces them: a client can name a
//! message number no integer holds, and `*` is `MAX_SAFE_INTEGER`.

use std::collections::{HashMap, HashSet};

use super::types::{SequenceRange, SequenceSet, MAX_SAFE_INTEGER};

/// The sequence state a session holds for its selected mailbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SequenceState {
    /// UID of each message, in sequence order: index 0 is sequence number 1.
    pub seq_to_uid: Vec<i64>,
    /// Sequence number (1-based) of each UID.
    pub uid_to_seq: HashMap<i64, usize>,
}

/// What reading the selected mailbox's UIDs came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxUids {
    /// No store or no selected mailbox: there is nothing to read.
    NotSelected,
    /// The mailbox could not be read.
    Unreadable,
    /// The mailbox's UIDs in ascending order.
    Uids(Vec<i64>),
}

/// Builds the sequence number → UID mapping from what the store read.
///
/// The caller does the read (`store.getAllUids(selectedMailbox)`) and passes
/// its result. `state.seq_to_uid` is replaced, not refilled. Returns false
/// when the mailbox could not be read, leaving `state` as it was: an
/// unreadable mailbox and an empty one must not collapse to the same mapping,
/// or every advertised message reads as departed.
pub fn build_sequence_mapping(state: &mut SequenceState, read: MailboxUids) -> bool {
    match read {
        MailboxUids::NotSelected => {
            state.seq_to_uid = Vec::new();
            state.uid_to_seq.clear();
            true
        }
        MailboxUids::Unreadable => false,
        MailboxUids::Uids(uids) => {
            set_sequence_mapping(state, uids);
            true
        }
    }
}

/// Points `state` at `uids` as the mailbox's 1..N sequence order.
pub fn set_sequence_mapping(state: &mut SequenceState, uids: Vec<i64>) {
    state.uid_to_seq.clear();
    for (index, uid) in uids.iter().enumerate() {
        state.uid_to_seq.insert(*uid, index + 1);
    }
    state.seq_to_uid = uids;
}

/// Rebuilds the mapping, announces whatever left the mailbox, and reports the
/// new message count. Returns `None` when there is no mailbox or it could not
/// be read, in which case nothing is written and `state` is left alone.
///
/// RFC 3501 §7.3.1 lets the message count shrink only via EXPUNGE, so every
/// rebuild that drops a message the client still holds owes it an untagged
/// `* <seq> EXPUNGE` first. Messages leave without this session running an
/// EXPUNGE, so departures are found by diffing the mailbox against what this
/// session last advertised.
pub fn reconcile_sequence_mapping(
    state: &mut SequenceState,
    read: MailboxUids,
    write: &mut dyn FnMut(&str),
) -> Option<usize> {
    if read == MailboxUids::NotSelected {
        return None;
    }
    let advertised = state.seq_to_uid.clone();
    if !build_sequence_mapping(state, read) {
        return None;
    }
    for seq in departed_sequence_numbers(&advertised, &state.seq_to_uid) {
        write(&format!("* {seq} EXPUNGE\r\n"));
    }
    Some(state.seq_to_uid.len())
}

/// Sequence numbers of the messages that have left a mailbox since it was
/// last advertised, in descending order.
///
/// Descending is the order they have to be announced in: RFC 3501 §7.4.1
/// renumbers every message after an expunged one the instant its EXPUNGE is
/// sent, so working from the back keeps each number valid when it is written.
pub fn departed_sequence_numbers(advertised: &[i64], live: &[i64]) -> Vec<usize> {
    let live_uids: HashSet<i64> = live.iter().copied().collect();
    (1..=advertised.len())
        .rev()
        .filter(|seq| !live_uids.contains(&advertised[seq - 1]))
        .collect()
}

/// `array[index]` for a JavaScript number: `None` for anything that is not
/// an index of the array.
fn element_at(array: &[i64], index: f64) -> Option<i64> {
    if index < 0.0 || index.fract() != 0.0 || index >= array.len() as f64 {
        return None;
    }
    array.get(index as usize).copied()
}

/// Converts a sequence number to a UID. `*` is the highest UID.
pub fn seq_to_uid_number(seq_to_uid: &[i64], seq: f64) -> Option<i64> {
    if seq == MAX_SAFE_INTEGER {
        return seq_to_uid.last().copied();
    }
    // Sequence numbers are 1-based.
    element_at(seq_to_uid, seq - 1.0)
}

/// A range of UIDs, both ends inclusive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UidRange<T> {
    pub uid_start: T,
    pub uid_end: T,
}

/// The UIDs of a sequence-number range, each end clamped to the mailbox.
/// `None` for an empty mailbox or a range that starts past its end.
pub fn resolve_seq_range_to_uids(seq_to_uid: &[i64], start: f64, end: f64) -> Option<UidRange<i64>> {
    let max_seq = seq_to_uid.len() as f64;
    if seq_to_uid.is_empty() {
        return None;
    }
    if start != MAX_SAFE_INTEGER && start > max_seq {
        return None;
    }
    let uid_start = seq_to_uid_number(seq_to_uid, start.min(max_seq))?;
    let uid_end = seq_to_uid_number(seq_to_uid, end.min(max_seq))?;
    Some(UidRange { uid_start, uid_end })
}

/// Converts a UID to a sequence number. `*` is the highest sequence number.
pub fn uid_to_seq_number(seq_to_uid: &[i64], uid_to_seq: &HashMap<i64, usize>, uid: f64) -> Option<usize> {
    if uid == MAX_SAFE_INTEGER {
        return Some(seq_to_uid.len());
    }
    if uid.fract() != 0.0 || uid.abs() >= 9.3e18 {
        return None;
    }
    uid_to_seq.get(&(uid as i64)).copied()
}

/// A UID range with `*` resolved to the highest UID, or to -1 in an empty mailbox.
pub fn resolve_uid_range_sentinel(seq_to_uid: &[i64], start: f64, end: f64) -> UidRange<f64> {
    let max_uid = seq_to_uid.last().map_or(-1.0, |uid| *uid as f64);
    let resolve = |value: f64| if value == MAX_SAFE_INTEGER { max_uid } else { value };
    UidRange { uid_start: resolve(start), uid_end: resolve(end) }
}

/// The ends of a UID range with `*` resolved and a reversed range put in
/// order (RFC 3501 §9: `10:3` is `3:10`).
fn uid_bounds(range: &SequenceRange, max_uid: f64) -> (f64, f64) {
    let resolve = |value: f64| if value == MAX_SAFE_INTEGER { max_uid } else { value };
    let start = resolve(range.start);
    let end = resolve(range.end.unwrap_or(range.start));
    (start.min(end), start.max(end))
}

/// The ends of a sequence-number range clamped to the mailbox and put in order.
fn seq_bounds(start: f64, end: f64, max_seq: f64) -> (f64, f64) {
    let clamped_start = start.min(max_seq);
    let clamped_end = end.min(max_seq);
    (clamped_start.min(clamped_end), clamped_start.max(clamped_end))
}

/// Binary search from `from`: the first index whose UID is not `before`.
fn first_index(seq_to_uid: &[i64], from: usize, before: impl Fn(f64) -> bool) -> usize {
    let (mut left, mut right) = (from, seq_to_uid.len());
    while left < right {
        let mid = (left + right) / 2;
        if before(seq_to_uid[mid] as f64) {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    left
}

/// Counts the messages a sequence set covers, clamped to the mailbox. Feeds
/// the FETCH limit check, so a miscount is a cap bypassed.
///
/// `is_uid_command` says which axis the set is on: the parser stamps every
/// set as a sequence set, so the caller is the source of truth. On the UID
/// axis each range is intersected with `seq_to_uid`, which is ascending
/// (RFC 3501 §2.3.1.1), by binary search.
pub fn count_sequence_set_messages(seq_to_uid: &[i64], sequence_set: &SequenceSet, is_uid_command: bool) -> f64 {
    if is_uid_command {
        let Some(max_uid) = seq_to_uid.last().map(|uid| *uid as f64) else {
            return 0.0;
        };
        let mut count = 0.0;
        for range in &sequence_set.ranges {
            let (lo, hi) = uid_bounds(range, max_uid);
            // First index >= lo.
            let start_index = first_index(seq_to_uid, 0, |uid| uid < lo);
            // First index > hi.
            let end_index = first_index(seq_to_uid, start_index, |uid| uid <= hi);
            count += (end_index - start_index) as f64;
        }
        return count;
    }

    let max_seq = seq_to_uid.len() as f64;
    let mut count = 0.0;
    for range in &sequence_set.ranges {
        match range.end {
            None => count += 1.0,
            Some(end) => {
                let (lo, hi) = seq_bounds(range.start, end, max_seq);
                count += (hi - lo + 1.0).max(0.0);
            }
        }
    }
    count
}

/// Takes the first `limit` messages of a sequence set and drops the rest.
///
/// Used when a FETCH would exceed the per-command message cap: instead of
/// refusing with `NO [LIMIT]`, the set shrinks to what will be processed
/// (RFC 3501 §6.4.5 lets a server return a subset), and the client fetches
/// the rest in follow-up commands.
///
/// On the sequence axis the ranges are walked in order: whole ones are kept
/// up to the limit, the next is taken in part. On the UID axis UIDs are not
/// 1..N, so the first `limit` UIDs of `seq_to_uid` that fall in the requested
/// ranges are kept and returned as coalesced ranges of exactly those UIDs.
pub fn clamp_sequence_set_to_first(
    seq_to_uid: &[i64],
    sequence_set: &SequenceSet,
    limit: f64,
    is_uid_command: bool,
) -> SequenceSet {
    let with_ranges = |ranges: Vec<SequenceRange>| SequenceSet { kind: sequence_set.kind, ranges };
    if limit <= 0.0 {
        return with_ranges(Vec::new());
    }

    if is_uid_command {
        let Some(max_uid) = seq_to_uid.last().map(|uid| *uid as f64) else {
            return with_ranges(Vec::new());
        };
        let mut matched: Vec<i64> = Vec::new();
        'ranges: for range in &sequence_set.ranges {
            let (lo, hi) = uid_bounds(range, max_uid);
            for uid in seq_to_uid {
                if *uid as f64 >= lo && *uid as f64 <= hi {
                    matched.push(*uid);
                    if matched.len() as f64 >= limit {
                        break 'ranges;
                    }
                }
            }
        }
        let Some(first) = matched.first().copied() else {
            return with_ranges(Vec::new());
        };
        let mut coalesced: Vec<SequenceRange> = Vec::new();
        let (mut start, mut end) = (first, first);
        for uid in matched.into_iter().skip(1) {
            if uid == end + 1 {
                end = uid;
            } else {
                coalesced.push(SequenceRange::range(start as f64, end as f64));
                start = uid;
                end = uid;
            }
        }
        coalesced.push(SequenceRange::range(start as f64, end as f64));
        return with_ranges(coalesced);
    }

    let max_seq = seq_to_uid.len() as f64;
    let mut clamped: Vec<SequenceRange> = Vec::new();
    let mut remaining = limit;
    for range in &sequence_set.ranges {
        if remaining <= 0.0 {
            break;
        }
        let Some(end) = range.end else {
            clamped.push(*range);
            remaining -= 1.0;
            continue;
        };
        let (lo, hi) = seq_bounds(range.start, end, max_seq);
        let range_count = (hi - lo + 1.0).max(0.0);
        if range_count == 0.0 {
            continue;
        }
        if range_count <= remaining {
            clamped.push(SequenceRange::range(lo, hi));
            remaining -= range_count;
        } else {
            clamped.push(SequenceRange::range(lo, lo + remaining - 1.0));
            remaining = 0.0;
        }
    }
    with_ranges(clamped)
}
