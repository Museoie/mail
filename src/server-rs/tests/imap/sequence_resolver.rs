//! The Bun server's `imap/sequence-resolver.test.ts`, case for case.
//!
//! Where the Bun tests hand the resolver a store, these hand it what the
//! store read: the caller does the read in the port.

use std::collections::HashMap;

use mail_server::imap::sequence_resolver::*;
use mail_server::imap::types::{SequenceRange, SequenceSet, MAX_SAFE_INTEGER as STAR};

fn state_of(uids: &[i64]) -> SequenceState {
    let mut state = SequenceState::default();
    set_sequence_mapping(&mut state, uids.to_vec());
    state
}

fn single(start: f64) -> SequenceRange {
    SequenceRange::single(start)
}

fn range(start: f64, end: f64) -> SequenceRange {
    SequenceRange::range(start, end)
}

fn set(ranges: Vec<SequenceRange>) -> SequenceSet {
    SequenceSet::new(ranges)
}

fn pruned() -> Vec<i64> {
    (0..9950).map(|i| 10001 + i).collect()
}

// --- buildSequenceMapping ---

#[test]
fn build_populates_both_maps_from_store_uids() {
    let mut state = SequenceState::default();
    assert!(build_sequence_mapping(&mut state, MailboxUids::Uids(vec![10, 20, 30])));
    assert_eq!(state.seq_to_uid, vec![10, 20, 30]);
    assert_eq!(state.uid_to_seq.get(&10), Some(&1));
    assert_eq!(state.uid_to_seq.get(&20), Some(&2));
    assert_eq!(state.uid_to_seq.get(&30), Some(&3));
}

#[test]
fn build_clears_the_mapping_when_there_is_no_store_or_mailbox() {
    // No store and no selected mailbox are the same thing to the port.
    let mut state = state_of(&[1, 2, 3]);
    assert!(build_sequence_mapping(&mut state, MailboxUids::NotSelected));
    assert!(state.seq_to_uid.is_empty());
    assert!(state.uid_to_seq.is_empty());
}

#[test]
fn build_returns_false_and_keeps_the_previous_mapping_when_the_read_fails() {
    let mut state = state_of(&[10, 20]);
    assert!(!build_sequence_mapping(&mut state, MailboxUids::Unreadable));
    assert_eq!(state.seq_to_uid, vec![10, 20]);
    assert_eq!(state.uid_to_seq.get(&20), Some(&2));
}

#[test]
fn build_replaces_the_previous_mapping_on_re_select() {
    let mut state = state_of(&[100]);
    build_sequence_mapping(&mut state, MailboxUids::Uids(vec![5, 15]));
    assert_eq!(state.seq_to_uid, vec![5, 15]);
    assert!(!state.uid_to_seq.contains_key(&100));
    assert_eq!(state.uid_to_seq.get(&5), Some(&1));
    assert_eq!(state.uid_to_seq.get(&15), Some(&2));
}

// --- seqToUidNumber ---

const UIDS: [i64; 5] = [10, 20, 30, 40, 50];

#[test]
fn seq_to_uid_maps_sequence_numbers_to_uids() {
    assert_eq!(seq_to_uid_number(&UIDS, 1.0), Some(10));
    assert_eq!(seq_to_uid_number(&UIDS, 3.0), Some(30));
}

#[test]
fn seq_to_uid_maps_star_to_the_last_uid() {
    assert_eq!(seq_to_uid_number(&UIDS, STAR), Some(50));
}

#[test]
fn seq_to_uid_returns_nothing_out_of_range_or_on_an_empty_mailbox() {
    assert_eq!(seq_to_uid_number(&UIDS, 6.0), None);
    assert_eq!(seq_to_uid_number(&[], 1.0), None);
    assert_eq!(seq_to_uid_number(&[], STAR), None);
}

// --- resolveSeqRangeToUids (inbox #588) ---

fn seq_range(uids: &[i64], start: f64, end: f64) -> Option<(i64, i64)> {
    resolve_seq_range_to_uids(uids, start, end).map(|range| (range.uid_start, range.uid_end))
}

#[test]
fn seq_range_resolves_a_fully_in_range_range_to_its_uid_bounds() {
    assert_eq!(seq_range(&UIDS, 2.0, 4.0), Some((20, 40)));
}

#[test]
fn seq_range_clamps_an_upper_bound_past_the_end_to_the_last_message() {
    assert_eq!(seq_range(&UIDS, 3.0, 80.0), Some((30, 50)));
}

#[test]
fn seq_range_resolves_n_star_to_the_last_message() {
    assert_eq!(seq_range(&UIDS, 3.0, STAR), Some((30, 50)));
}

#[test]
fn seq_range_resolves_a_single_message_range() {
    assert_eq!(seq_range(&UIDS, 5.0, 5.0), Some((50, 50)));
}

#[test]
fn seq_range_returns_nothing_when_the_whole_range_is_past_the_end() {
    assert_eq!(seq_range(&UIDS, 6.0, 9.0), None);
}

#[test]
fn seq_range_returns_nothing_on_an_empty_mailbox() {
    assert_eq!(seq_range(&[], 1.0, 10.0), None);
}

#[test]
fn seq_range_resolves_a_bare_star_to_the_last_message() {
    assert_eq!(seq_range(&UIDS, STAR, STAR), Some((50, 50)));
    assert_eq!(seq_range(&[10], STAR, STAR), Some((10, 10)));
}

#[test]
fn seq_range_returns_nothing_for_star_on_an_empty_mailbox() {
    assert_eq!(seq_range(&[], STAR, STAR), None);
}

#[test]
fn seq_range_still_returns_nothing_for_a_concrete_past_end_start() {
    assert_eq!(seq_range(&UIDS, 6.0, STAR), None);
}

// --- resolveUidRangeSentinel (inbox #678) ---

fn uid_range(uids: &[i64], start: f64, end: f64) -> (f64, f64) {
    let range = resolve_uid_range_sentinel(uids, start, end);
    (range.uid_start, range.uid_end)
}

#[test]
fn uid_range_passes_concrete_uids_through_unchanged() {
    assert_eq!(uid_range(&UIDS, 20.0, 40.0), (20.0, 40.0));
}

#[test]
fn uid_range_resolves_a_bare_star_to_the_highest_uid() {
    assert_eq!(uid_range(&UIDS, STAR, STAR), (50.0, 50.0));
}

#[test]
fn uid_range_resolves_an_open_ended_upper_bound_to_the_highest_uid() {
    assert_eq!(uid_range(&UIDS, 30.0, STAR), (30.0, 50.0));
}

#[test]
fn uid_range_never_leaves_the_sentinel_in_the_resolved_pair() {
    let (start, end) = uid_range(&UIDS, STAR, STAR);
    // Postgres `integer` max.
    assert!(start < 2147483647.0 && end < 2147483647.0);
}

#[test]
fn uid_range_resolves_star_to_minus_one_on_an_empty_mailbox() {
    assert_eq!(uid_range(&[], STAR, STAR), (-1.0, -1.0));
}

#[test]
fn uid_range_passes_a_concrete_out_of_range_uid_through() {
    assert_eq!(uid_range(&UIDS, 999999.0, 999999.0), (999999.0, 999999.0));
}

// --- uidToSeqNumber ---

#[test]
fn uid_to_seq_maps_uids_to_sequence_numbers() {
    let uids = [10, 20, 30];
    let map: HashMap<i64, usize> = HashMap::from([(10, 1), (20, 2), (30, 3)]);
    assert_eq!(uid_to_seq_number(&uids, &map, 20.0), Some(2));
    assert_eq!(uid_to_seq_number(&uids, &map, STAR), Some(3));
    assert_eq!(uid_to_seq_number(&uids, &map, 99.0), None);
    assert_eq!(uid_to_seq_number(&[], &HashMap::new(), STAR), Some(0));
}

// --- countSequenceSetMessages, sequence axis ---

fn count_seq(uids: &[i64], ranges: Vec<SequenceRange>) -> f64 {
    count_sequence_set_messages(uids, &set(ranges), false)
}

#[test]
fn count_seq_counts_single_messages_and_ranges() {
    assert_eq!(count_seq(&UIDS, vec![single(2.0)]), 1.0);
    assert_eq!(count_seq(&UIDS, vec![range(2.0, 4.0)]), 3.0);
}

#[test]
fn count_seq_clamps_ranges_beyond_mailbox_size() {
    assert_eq!(count_seq(&UIDS, vec![range(3.0, 100.0)]), 3.0);
}

#[test]
fn count_seq_sums_multiple_ranges() {
    assert_eq!(count_seq(&UIDS, vec![range(1.0, 2.0), single(4.0)]), 3.0);
}

#[test]
fn count_seq_clamps_both_ends_to_zero_on_an_empty_mailbox() {
    assert_eq!(count_seq(&[], vec![range(1.0, 10.0)]), 1.0);
}

#[test]
fn count_seq_returns_zero_for_an_empty_sequence_set() {
    assert_eq!(count_seq(&UIDS, vec![]), 0.0);
}

#[test]
fn count_seq_handles_reversed_ranges() {
    // `*:1` is `1:*`: the whole mailbox, not nothing.
    assert_eq!(count_seq(&UIDS, vec![range(STAR, 1.0)]), UIDS.len() as f64);
}

// --- countSequenceSetMessages, UID axis ---

fn count_uid(uids: &[i64], ranges: Vec<SequenceRange>) -> f64 {
    count_sequence_set_messages(uids, &set(ranges), true)
}

#[test]
fn count_uid_counts_real_uids_not_seq_positions_on_a_pruned_mailbox() {
    assert_eq!(count_uid(&pruned(), vec![range(10051.0, STAR)]), 9900.0);
}

#[test]
fn count_uid_counts_zero_outside_the_mailboxs_uids() {
    assert_eq!(count_uid(&pruned(), vec![range(1.0, 500.0)]), 0.0);
    assert_eq!(count_uid(&pruned(), vec![range(50000.0, 60000.0)]), 0.0);
}

#[test]
fn count_uid_counts_the_intersection_of_a_partial_overlap() {
    assert_eq!(count_uid(&pruned(), vec![range(9900.0, 10100.0)]), 100.0);
}

#[test]
fn count_uid_counts_zero_for_star_on_an_empty_mailbox() {
    assert_eq!(count_uid(&[], vec![range(STAR, STAR)]), 0.0);
}

#[test]
fn count_uid_counts_a_bare_single_uid() {
    assert_eq!(count_uid(&pruned(), vec![single(10500.0)]), 1.0);
}

#[test]
fn count_uid_sums_per_range_intersections() {
    let ranges = vec![range(10001.0, 10005.0), range(15000.0, 15003.0), range(50000.0, 60000.0)];
    assert_eq!(count_uid(&pruned(), ranges), 9.0);
}

#[test]
fn count_uid_returns_zero_on_an_empty_sequence_set() {
    assert_eq!(count_uid(&pruned(), vec![]), 0.0);
}

#[test]
fn count_uid_handles_reversed_ranges() {
    assert_eq!(count_uid(&pruned(), vec![range(10100.0, 10000.0)]), 100.0);
}

// --- clampSequenceSetToFirst, sequence axis ---

fn clamp_seq(ranges: Vec<SequenceRange>, limit: f64) -> Vec<SequenceRange> {
    let uids: Vec<i64> = (101..=110).collect();
    clamp_sequence_set_to_first(&uids, &set(ranges), limit, false).ranges
}

#[test]
fn clamp_seq_returns_the_set_unchanged_at_or_below_the_limit() {
    assert_eq!(clamp_seq(vec![range(1.0, 5.0)], 5.0), vec![range(1.0, 5.0)]);
    assert_eq!(clamp_seq(vec![range(1.0, 3.0)], 50.0), vec![range(1.0, 3.0)]);
}

#[test]
fn clamp_seq_truncates_a_single_range_that_exceeds_the_limit() {
    assert_eq!(clamp_seq(vec![range(1.0, 10.0)], 3.0), vec![range(1.0, 3.0)]);
}

#[test]
fn clamp_seq_truncates_one_to_star_to_the_first_n() {
    assert_eq!(clamp_seq(vec![range(1.0, STAR)], 4.0), vec![range(1.0, 4.0)]);
}

#[test]
fn clamp_seq_keeps_whole_ranges_first_then_partially_takes_the_next() {
    assert_eq!(
        clamp_seq(vec![range(1.0, 2.0), range(5.0, 8.0), single(10.0)], 5.0),
        vec![range(1.0, 2.0), range(5.0, 7.0)]
    );
}

#[test]
fn clamp_seq_keeps_single_message_ranges_until_the_limit() {
    assert_eq!(clamp_seq(vec![single(3.0), single(5.0), single(7.0)], 2.0), vec![single(3.0), single(5.0)]);
}

#[test]
fn clamp_seq_returns_no_ranges_when_the_limit_is_zero() {
    assert!(clamp_seq(vec![range(1.0, 10.0)], 0.0).is_empty());
}

#[test]
fn clamp_seq_normalizes_reversed_ranges() {
    assert_eq!(clamp_seq(vec![range(STAR, 1.0)], 3.0), vec![range(1.0, 3.0)]);
    assert_eq!(clamp_seq(vec![range(10.0, 3.0)], 4.0), vec![range(3.0, 6.0)]);
}

// --- clampSequenceSetToFirst, UID axis ---

fn clamp_uid(uids: &[i64], ranges: Vec<SequenceRange>, limit: f64) -> Vec<SequenceRange> {
    clamp_sequence_set_to_first(uids, &set(ranges), limit, true).ranges
}

fn ten() -> Vec<i64> {
    (10001..=10010).collect()
}

#[test]
fn clamp_uid_emits_real_uids_not_seq_positions_on_a_pruned_mailbox() {
    assert_eq!(clamp_uid(&ten(), vec![range(1.0, STAR)], 3.0), vec![range(10001.0, 10003.0)]);
}

#[test]
fn clamp_uid_takes_only_uids_that_intersect_the_requested_range() {
    assert_eq!(clamp_uid(&ten(), vec![range(10005.0, 10008.0)], 2.0), vec![range(10005.0, 10006.0)]);
}

#[test]
fn clamp_uid_returns_no_ranges_when_no_uids_intersect() {
    assert!(clamp_uid(&ten(), vec![range(1.0, 50.0)], 3.0).is_empty());
}

#[test]
fn clamp_uid_walks_ranges_in_order_and_emits_coalesced_sub_ranges() {
    assert_eq!(
        clamp_uid(&ten(), vec![range(10001.0, 10002.0), range(10005.0, 10008.0)], 4.0),
        vec![range(10001.0, 10002.0), range(10005.0, 10006.0)]
    );
}

#[test]
fn clamp_uid_coalesces_discrete_uids_without_breaching_the_cap() {
    let dense: Vec<i64> = (1..=200).collect();
    let requested: Vec<SequenceRange> = (0..100).map(|i| range((i * 2 + 1) as f64, (i * 2 + 1) as f64)).collect();
    let result = clamp_uid(&dense, requested, 50.0);
    assert_eq!(result.len(), 50);
    for (i, clamped) in result.iter().enumerate() {
        assert_eq!(*clamped, range((i * 2 + 1) as f64, (i * 2 + 1) as f64));
    }
}

#[test]
fn clamp_uid_resolves_star_and_normalizes_reversed_ranges() {
    assert_eq!(clamp_uid(&pruned(), vec![range(STAR, 10051.0)], 50.0), vec![range(10051.0, 10100.0)]);
    assert_eq!(clamp_uid(&pruned(), vec![range(19950.0, 10051.0)], 50.0), vec![range(10051.0, 10100.0)]);
}

#[test]
fn clamp_uid_collapses_contiguous_uids_but_keeps_holes_as_separate_ranges() {
    let dense: Vec<i64> = (1..=20).collect();
    let ranges = vec![range(1.0, 3.0), range(7.0, 9.0), range(15.0, 17.0)];
    assert_eq!(clamp_uid(&dense, ranges.clone(), 9.0), ranges);
}

#[test]
fn clamp_uid_treats_a_single_uid_range_as_start_to_start() {
    assert_eq!(clamp_uid(&ten(), vec![single(10005.0)], 3.0), vec![range(10005.0, 10005.0)]);
}

#[test]
fn clamp_uid_returns_no_ranges_on_an_empty_mailbox_or_a_zero_limit() {
    assert!(clamp_uid(&[], vec![range(1.0, STAR)], 50.0).is_empty());
    assert!(clamp_uid(&ten(), vec![range(10001.0, 10010.0)], 0.0).is_empty());
}

// --- reconcileSequenceMapping ---

#[test]
fn reconcile_emits_an_expunge_per_departure_highest_first() {
    let mut state = state_of(&[10, 20, 30, 40]);
    let mut writes: Vec<String> = Vec::new();
    let total = reconcile_sequence_mapping(&mut state, MailboxUids::Uids(vec![10, 30]), &mut |data| writes.push(data.to_string()));
    assert_eq!(total, Some(2));
    assert_eq!(writes, vec!["* 4 EXPUNGE\r\n", "* 2 EXPUNGE\r\n"]);
    assert_eq!(state.seq_to_uid, vec![10, 30]);
}

#[test]
fn reconcile_writes_nothing_for_a_mailbox_that_only_grew() {
    let mut state = state_of(&[10, 20]);
    let mut writes: Vec<String> = Vec::new();
    let total = reconcile_sequence_mapping(&mut state, MailboxUids::Uids(vec![10, 20, 30]), &mut |data| writes.push(data.to_string()));
    assert_eq!(total, Some(3));
    assert!(writes.is_empty());
}

#[test]
fn reconcile_touches_neither_the_wire_nor_the_mapping_when_the_read_fails() {
    for read in [MailboxUids::Unreadable, MailboxUids::NotSelected] {
        let mut state = state_of(&[10, 20, 30]);
        let mut writes: Vec<String> = Vec::new();
        let total = reconcile_sequence_mapping(&mut state, read, &mut |data| writes.push(data.to_string()));
        assert_eq!(total, None);
        assert!(writes.is_empty());
        assert_eq!(state.seq_to_uid, vec![10, 20, 30]);
        assert_eq!(state.uid_to_seq.get(&30), Some(&3));
    }
}

// --- departedSequenceNumbers ---

#[test]
fn departed_returns_the_sequence_numbers_of_departed_messages_highest_first() {
    assert_eq!(departed_sequence_numbers(&[10, 20, 30, 40, 50], &[10, 30, 50]), vec![4, 2]);
}

#[test]
fn departed_is_descending_so_each_number_is_still_valid_when_written() {
    assert_eq!(departed_sequence_numbers(&[1, 2, 3, 4, 5, 6], &[2, 5]), vec![6, 4, 3, 1]);
}

#[test]
fn departed_replayed_against_the_advertised_list_yields_the_live_list() {
    let advertised = [7, 8, 9, 10, 11];
    let live = [8, 11];
    let mut remaining = advertised.to_vec();
    for seq in departed_sequence_numbers(&advertised, &live) {
        remaining.remove(seq - 1);
    }
    assert_eq!(remaining, live);
}

#[test]
fn departed_reports_nothing_when_the_mailbox_grew_or_is_unchanged() {
    assert!(departed_sequence_numbers(&[1, 2], &[1, 2, 3, 4]).is_empty());
    assert!(departed_sequence_numbers(&[1, 2, 3], &[1, 2, 3]).is_empty());
    assert!(departed_sequence_numbers(&[], &[1, 2, 3]).is_empty());
}

#[test]
fn departed_reports_every_position_when_the_mailbox_emptied() {
    assert_eq!(departed_sequence_numbers(&[1, 2, 3], &[]), vec![3, 2, 1]);
}

#[test]
fn departed_handles_simultaneous_departure_and_arrival() {
    assert_eq!(departed_sequence_numbers(&[1, 2, 3], &[1, 3, 4, 5]), vec![2]);
}
