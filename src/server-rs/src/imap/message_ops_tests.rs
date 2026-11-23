//! The parts of the message commands that need no store
//! (message-ops.test.ts: resolveSeqSearchKeys, resolveDestContext; the
//! COPYUID / MODIFIED set formatting; the FETCH range merge).

use super::*;
use crate::imap::types::MAX_SAFE_INTEGER;

const SEQ_TO_UID: [i64; 3] = [11395, 11396, 11400];

fn set(ranges: Vec<SequenceRange>) -> SequenceSet {
    SequenceSet::new(ranges)
}

fn uid(ranges: Vec<SequenceRange>) -> SearchCriterion {
    SearchCriterion::Uid(set(ranges))
}

fn seq(ranges: Vec<SequenceRange>) -> SearchCriterion {
    SearchCriterion::Seq(set(ranges))
}

fn range(start: f64, end: f64) -> SequenceRange {
    SequenceRange::range(start, end)
}

// resolveSeqSearchKeys: bare sequence-set (#649)
mod resolve_seq_search_keys_tests {
    use super::*;

    #[test]
    fn plain_search_resolves_a_seq_range_to_the_matching_uid_range() {
        // seq 1:3 is uid 11395:11400, not 1:3, which would match the wrong axis.
        let out = resolve_seq_search_keys(&[seq(vec![range(1.0, 3.0)])], false, &SEQ_TO_UID);
        assert_eq!(out, [uid(vec![range(11395.0, 11400.0)])]);
    }

    #[test]
    fn plain_search_resolves_a_single_bare_seq_number_to_its_uid() {
        let out = resolve_seq_search_keys(&[seq(vec![SequenceRange::single(2.0)])], false, &SEQ_TO_UID);
        assert_eq!(out, [uid(vec![range(11396.0, 11396.0)])]);
    }

    #[test]
    fn uid_search_keeps_the_set_as_uids_only_relabeling_seq_to_uid() {
        let out = resolve_seq_search_keys(&[seq(vec![range(11395.0, 11400.0)])], true, &SEQ_TO_UID);
        assert_eq!(out, [uid(vec![range(11395.0, 11400.0)])]);
    }

    #[test]
    fn a_plain_search_seq_set_past_the_end_of_the_mailbox_matches_nothing() {
        // Pinned to an impossible UID range rather than dropped from the
        // AND, where it would match everything.
        let out = resolve_seq_search_keys(&[seq(vec![range(5.0, 7.0)])], false, &SEQ_TO_UID);
        assert_eq!(out, [uid(vec![range(-1.0, -1.0)])]);
    }

    #[test]
    fn leaves_flag_criteria_untouched_and_normalizes_an_explicit_uid_criterions_ranges() {
        let criteria = [SearchCriterion::Seen, uid(vec![SequenceRange::single(42.0)])];
        assert_eq!(
            resolve_seq_search_keys(&criteria, false, &SEQ_TO_UID),
            [SearchCriterion::Seen, uid(vec![range(42.0, 42.0)])]
        );
    }

    #[test]
    fn resolves_a_bare_star_in_an_explicit_uid_criterion_to_the_highest_uid() {
        let criteria = [uid(vec![SequenceRange::single(MAX_SAFE_INTEGER)])];
        assert_eq!(resolve_seq_search_keys(&criteria, false, &SEQ_TO_UID), [uid(vec![range(11400.0, 11400.0)])]);
    }

    #[test]
    fn resolves_a_seq_set_alongside_a_flag_key() {
        let out = resolve_seq_search_keys(&[SearchCriterion::Seen, seq(vec![range(1.0, 3.0)])], false, &SEQ_TO_UID);
        let types: Vec<&str> = out.iter().map(SearchCriterion::type_name).collect();
        assert_eq!(types, ["SEEN", "UID"]);
    }

    #[test]
    fn resolves_a_bare_star_nested_under_not() {
        let criteria = [SearchCriterion::not(uid(vec![SequenceRange::single(MAX_SAFE_INTEGER)]))];
        assert_eq!(
            resolve_seq_search_keys(&criteria, false, &SEQ_TO_UID),
            [SearchCriterion::not(uid(vec![range(11400.0, 11400.0)]))]
        );
    }

    #[test]
    fn resolves_a_bare_star_nested_under_or_on_both_sides() {
        let criteria = [SearchCriterion::or(uid(vec![range(1000.0, MAX_SAFE_INTEGER)]), SearchCriterion::Seen)];
        assert_eq!(
            resolve_seq_search_keys(&criteria, false, &SEQ_TO_UID),
            [SearchCriterion::or(uid(vec![range(1000.0, 11400.0)]), SearchCriterion::Seen)]
        );
    }

    #[test]
    fn a_deep_nest_resolves_without_recursion() {
        let mut criterion = seq(vec![range(1.0, 3.0)]);
        for _ in 0..50_000 {
            criterion = SearchCriterion::not(criterion);
        }
        let out = resolve_seq_search_keys(&[criterion], false, &SEQ_TO_UID);
        let mut inner = &out[0];
        let mut depth = 0;
        while let SearchCriterion::Not(sub) = inner {
            inner = &**sub;
            depth += 1;
        }
        assert_eq!(depth, 50_000);
        assert_eq!(inner, &uid(vec![range(11395.0, 11400.0)]));
    }
}

// resolveDestContext: the destination facts (#830)
mod resolve_dest_context_tests {
    use super::*;

    fn account_is_under(account: &str, local: &str) {
        assert!(account.starts_with(&format!("{local}@")), "{account}");
    }

    #[test]
    fn inbox_is_domain_scoped_preserves_recipient_not_sent_not_mapped_utility() {
        let d = resolve_dest_context("admin", "INBOX");
        assert!(d.is_domain_scoped && d.preserves_recipient && !d.is_sent && !d.is_mapped_utility);
    }

    #[test]
    fn unified_sent_messages_is_domain_scoped_preserves_recipient_and_is_sent() {
        let d = resolve_dest_context("admin", "Sent Messages");
        assert!(d.is_domain_scoped && d.preserves_recipient && d.is_sent && !d.is_mapped_utility);
    }

    #[test]
    fn drafts_and_junk_preserve_recipient_not_sent_not_mapped_utility() {
        for mailbox in ["Drafts", "Junk"] {
            let d = resolve_dest_context("admin", mailbox);
            assert!(d.is_domain_scoped && d.preserves_recipient && !d.is_sent && !d.is_mapped_utility, "{mailbox}");
        }
    }

    #[test]
    fn starred_and_trash_preserve_recipient_are_mapped_utility_not_domain_scoped() {
        for mailbox in ["Starred", "Trash"] {
            let d = resolve_dest_context("admin", mailbox);
            assert!(!d.is_domain_scoped && d.is_mapped_utility && d.preserves_recipient && !d.is_sent, "{mailbox}");
        }
    }

    #[test]
    fn a_user_created_box_is_mapped_and_address_routed() {
        let d = resolve_dest_context("admin", "Archive");
        assert!(!d.is_domain_scoped && !d.is_mapped_utility && !d.preserves_recipient && !d.is_sent);
        account_is_under(&d.account, "Archive");
    }

    #[test]
    fn a_per_account_received_box_is_mapped_and_address_routed() {
        let d = resolve_dest_context("admin", "INBOX/accounts/alice");
        assert!(!d.is_domain_scoped && !d.is_mapped_utility && !d.preserves_recipient && !d.is_sent);
        account_is_under(&d.account, "alice");
    }

    #[test]
    fn a_per_account_sent_box_is_mapped_address_routed_and_sent() {
        let d = resolve_dest_context("admin", "Sent Messages/accounts/alice");
        assert!(!d.is_domain_scoped && !d.is_mapped_utility && !d.preserves_recipient && d.is_sent);
        account_is_under(&d.account, "alice");
    }

    #[test]
    fn preserves_recipient_is_exactly_domain_scoped_or_mapped_utility() {
        for mailbox in [
            "INBOX",
            "Sent Messages",
            "Drafts",
            "Junk",
            "Starred",
            "Trash",
            "Archive",
            "INBOX/accounts/alice",
            "Sent Messages/accounts/alice",
        ] {
            let d = resolve_dest_context("admin", mailbox);
            assert_eq!(d.preserves_recipient, d.is_domain_scoped || d.is_mapped_utility, "{mailbox}");
        }
    }
}

// formatMessageSet: the sets in COPYUID and [MODIFIED].
mod format_message_set_tests {
    use super::*;

    #[test]
    fn nothing_is_an_empty_set() {
        assert_eq!(format_message_set(&[]), "");
    }

    #[test]
    fn runs_collapse_to_ranges_and_gaps_split_them() {
        assert_eq!(format_message_set(&[7]), "7");
        assert_eq!(format_message_set(&[1, 2, 3]), "1:3");
        assert_eq!(format_message_set(&[1, 2, 3, 5, 7, 8]), "1:3,5,7:8");
    }

    #[test]
    fn sorts_and_drops_duplicates_first() {
        assert_eq!(format_message_set(&[9, 3, 2, 3, 1, 9]), "1:3,9");
    }
}

// The concurrent FETCH range reads merge as the Bun server's Map does: a
// mail read twice keeps its first position and takes the later read.
mod merge_fetched_tests {
    use super::*;

    fn mail(subject: &str) -> StoreFetchedMail {
        StoreFetchedMail { subject: Some(subject.to_string()), ..Default::default() }
    }

    #[test]
    fn keeps_first_position_and_the_last_value() {
        let merged = merge_fetched(vec![
            vec![("a".to_string(), mail("a1")), ("b".to_string(), mail("b1"))],
            vec![("c".to_string(), mail("c1")), ("a".to_string(), mail("a2"))],
        ]);
        let shape: Vec<(String, Option<String>)> =
            merged.into_iter().map(|(id, mail)| (id, mail.subject)).collect();
        assert_eq!(
            shape,
            [
                ("a".to_string(), Some("a2".to_string())),
                ("b".to_string(), Some("b1".to_string())),
                ("c".to_string(), Some("c1".to_string())),
            ]
        );
    }

    #[test]
    fn no_reads_merge_to_nothing() {
        assert!(merge_fetched(Vec::new()).is_empty());
        assert!(merge_fetched(vec![Vec::new(), Vec::new()]).is_empty());
    }
}
