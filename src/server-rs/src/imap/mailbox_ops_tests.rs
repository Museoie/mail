//! LIST / LSUB and the guards that answer before the store
//! (mailbox-list, mailbox-lsub-subscription, utility-mailboxes,
//! inbox-case-insensitive and mailbox-name-limit .test.ts).
//!
//! The Bun suites drive `listMailboxes` / `listSubscribedMailboxes` with a
//! fake store; here the same listings go to `list_response` /
//! `lsub_response`, the part of each command after the store read. The
//! CREATE / DELETE / RENAME refusals run the real commands on a session: they
//! answer before any database work, which a unit test could not reach.

use std::time::Instant;

use super::*;
use crate::imap::test_support::wire_session;
use crate::imap::util::UTILITY_FOLDERS;
use crate::models::SignedUser;

const TREE: [&str; 6] =
    ["INBOX", "Sent Messages", "INBOX/accounts", "INBOX/accounts/work", "INBOX/accounts/personal", "Archive"];

fn matches(reference: &str, pattern: &str, name: &str) -> bool {
    create_list_pattern_matcher(reference, pattern)(name)
}

/// The names `LIST <reference> <pattern>` returns over `boxes`.
fn listed_in(boxes: &[&str], reference: &str, pattern: &str) -> Vec<String> {
    list_response("A1", reference, pattern, boxes).iter().filter(|line| line.starts_with("* LIST")).map(|line| name_of(line)).collect()
}

fn listed(reference: &str, pattern: &str) -> Vec<String> {
    listed_in(&TREE, reference, pattern)
}

/// `.*"/" "(.*)"\r\n$`
fn name_of(line: &str) -> String {
    let start = line.find("\"/\" \"").expect("a delimiter") + 5;
    line[start..].strip_suffix("\"\r\n").expect("a quoted name").to_string()
}

/// Each listed name with its attributes.
fn attributes_for(boxes: &[&str], pattern: &str) -> Vec<(String, String)> {
    list_response("A1", "", pattern, boxes)
        .iter()
        .filter_map(|line| {
            let rest = line.strip_prefix("* LIST (")?;
            let close = rest.find(") \"/\" ")?;
            Some((name_of(line), rest[..close].to_string()))
        })
        .collect()
}

fn attribute_of(boxes: &[&str], name: &str) -> Option<String> {
    attributes_for(boxes, "*").into_iter().find(|(listed, _)| listed == name).map(|(_, attributes)| attributes)
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

mod list_pattern_matcher {
    use super::*;

    #[test]
    fn a_star_matches_across_the_hierarchy_delimiter() {
        assert!(matches("", "*", "INBOX/accounts/work"));
    }

    #[test]
    fn a_percent_does_not_cross_the_hierarchy_delimiter() {
        assert!(matches("", "%", "INBOX"));
        assert!(!matches("", "%", "INBOX/accounts"));
    }

    #[test]
    fn the_inbox_fold_applies_to_the_stored_name_not_just_the_pattern() {
        // The store dedups mailbox names case-sensitively, so a legacy
        // lowercase "inbox" row can be listed beside the canonical one.
        assert!(matches("", "INBOX", "inbox"));
        assert!(matches("", "inbox", "inbox"));
    }

    #[test]
    fn an_exact_name_with_no_wildcard_matches_only_itself() {
        assert!(matches("", "INBOX", "INBOX"));
        assert!(!matches("", "INBOX", "INBOX/accounts"));
        assert!(!matches("", "INBOX", "Sent Messages"));
    }

    #[test]
    fn a_percent_after_a_path_segment_matches_one_further_level_only() {
        assert!(matches("", "INBOX/%", "INBOX/accounts"));
        assert!(!matches("", "INBOX/%", "INBOX/accounts/work"));
    }

    #[test]
    fn concatenates_a_non_empty_reference_with_the_pattern() {
        assert!(matches("INBOX/", "%", "INBOX/accounts"));
        assert!(!matches("INBOX/", "%", "Archive"));
    }

    #[test]
    fn a_percent_that_stalls_at_the_delimiter_lets_an_earlier_star_absorb_it() {
        assert!(matches("", "*a%b", "xa/ab"));
        assert!(!matches("", "*a%b", "xa/ab/c"));
        assert!(matches("", "*/%", "INBOX/accounts"));
        assert!(matches("", "*/%", "INBOX/accounts/work"));
        assert!(!matches("", "*/%", "Archive"));
    }

    #[test]
    fn interleaved_wildcards_and_literals_match_by_position() {
        assert!(matches("", "*a*b*", "xxaybzz"));
        assert!(!matches("", "*a*b*", "xxbyazz"));
        assert!(!matches("", "%a%", "za/b"));
        assert!(!matches("", "*a%", "za/b"));
        assert!(matches("", "*a*", "za/b"));
    }

    #[test]
    fn regex_metacharacters_in_a_pattern_stay_literal() {
        assert!(!matches("", "a.c", "abc"));
        assert!(matches("", "a.c", "a.c"));
        assert!(!matches("", "a+", "aa"));
        assert!(matches("", "a+", "a+"));
    }

    #[test]
    fn an_empty_concatenated_pattern_matches_only_an_empty_name() {
        assert!(matches("", "", ""));
        assert!(!matches("", "", "INBOX"));
    }

    // inbox-case-insensitive.test.ts: RFC 3501 §5.1 for the pattern too.
    #[test]
    fn matches_any_casing_of_inbox_against_the_canonical_name() {
        assert!(matches("", "inbox", "INBOX"));
        assert!(matches("", "Inbox", "INBOX"));
        assert!(matches("", "INBOX", "INBOX"));
    }

    #[test]
    fn an_inbox_pattern_does_not_match_another_mailbox() {
        assert!(!matches("", "inbox", "Archive"));
        assert!(!matches("", "inbox", "Sent Messages"));
    }

    #[test]
    fn other_names_stay_case_sensitive() {
        assert!(!matches("", "archive", "Archive"));
    }

    #[test]
    fn a_star_matches_everything() {
        for name in ["INBOX", "Archive", "Sent Messages"] {
            assert!(matches("", "*", name), "{name}");
        }
    }
}

// A LIST pattern is client input, so a matcher that answers right but slowly
// is a denial of service; these bound the wall clock (#856).
mod list_pattern_matcher_cost {
    use super::*;

    const BUDGET_MS: u128 = 1000;

    fn long_name() -> String {
        format!("INBOX/accounts/{}", "a".repeat(60))
    }

    fn elapsed_ms(run: impl FnOnce()) -> u128 {
        let started = Instant::now();
        run();
        started.elapsed().as_millis()
    }

    #[test]
    fn a_failing_alternation_of_wildcards_and_literals_returns_promptly() {
        let mut result = true;
        let took = elapsed_ms(|| result = matches("", &format!("{}!", "*a".repeat(10)), &long_name()));
        assert!(!result);
        assert!(took < BUDGET_MS, "{took} ms");
    }

    #[test]
    fn a_long_run_of_wildcards_returns_promptly() {
        let mut result = false;
        let took = elapsed_ms(|| result = matches("", &"*".repeat(10_000), &long_name()));
        assert!(result);
        assert!(took < BUDGET_MS, "{took} ms");
    }

    #[test]
    fn a_long_mixed_wildcard_run_returns_promptly_and_keeps_its_semantics() {
        let mut crosses_delimiter = false;
        let took = elapsed_ms(|| crosses_delimiter = matches("", &"%*%".repeat(3_000), &long_name()));
        assert!(crosses_delimiter);
        assert!(!matches("", "%%%", "INBOX/accounts"));
        assert!(took < BUDGET_MS, "{took} ms");
    }
}

mod collapse_wildcards {
    use super::*;

    #[test]
    fn reduces_a_mixed_run_to_the_delimiter_crossing_wildcard() {
        for pattern in ["%*%", "*%", "%*", "**"] {
            assert_eq!(collapse_wildcard_runs(pattern), "*", "{pattern}");
        }
    }

    #[test]
    fn reduces_a_run_of_only_percent_to_one_percent() {
        assert_eq!(collapse_wildcard_runs("%%"), "%");
        assert_eq!(collapse_wildcard_runs("%%%%"), "%");
    }

    #[test]
    fn keeps_runs_separated_by_a_literal_apart() {
        assert_eq!(collapse_wildcard_runs("%%a%%"), "%a%");
        assert_eq!(collapse_wildcard_runs("a**%b"), "a*b");
        assert_eq!(collapse_wildcard_runs("*a*a*"), "*a*a*");
    }

    #[test]
    fn leaves_a_wildcard_free_pattern_untouched() {
        assert_eq!(collapse_wildcard_runs(""), "");
        assert_eq!(collapse_wildcard_runs("INBOX/accounts"), "INBOX/accounts");
    }
}

// listMailboxes filtering (#596)
mod list_filtering {
    use super::*;

    #[test]
    fn a_percent_returns_top_level_names_only() {
        let result = listed("", "%");
        assert_eq!(sorted(result.clone()), ["Archive", "INBOX", "Sent Messages"]);
        assert!(!result.contains(&"INBOX/accounts".to_string()));
        assert!(!result.contains(&"INBOX/accounts/work".to_string()));
    }

    #[test]
    fn an_exact_inbox_returns_exactly_the_one_entry() {
        assert_eq!(listed("", "INBOX"), ["INBOX"]);
    }

    #[test]
    fn a_star_returns_the_full_tree() {
        assert_eq!(sorted(listed("", "*")), sorted(TREE.iter().map(|name| name.to_string()).collect()));
    }

    #[test]
    fn percent_accounts_returns_only_the_accounts_parents() {
        assert_eq!(listed("", "%/accounts"), ["INBOX/accounts"]);
    }

    #[test]
    fn an_empty_pattern_returns_the_hierarchy_delimiter_and_no_mailboxes() {
        let lines = list_response("A1", "", "", &TREE);
        assert!(lines.iter().any(|line| line.contains("(\\Noselect) \"/\" \"\"")));
        assert_eq!(lines.iter().filter(|line| line.starts_with("* LIST")).count(), 1);
        assert_eq!(lines.last().unwrap(), "A1 OK LIST completed\r\n");
    }
}

// listMailboxes hierarchy attributes (RFC 5258 §3)
mod list_hierarchy_attributes {
    use super::*;

    #[test]
    fn reports_a_user_created_parent_has_children() {
        let boxes = ["INBOX", "Projects", "Projects/Work"];
        assert_eq!(attribute_of(&boxes, "Projects").unwrap(), "\\HasChildren");
        assert_eq!(attribute_of(&boxes, "Projects/Work").unwrap(), "\\HasNoChildren");
    }

    #[test]
    fn reports_inbox_has_children_once_the_accounts_tree_exists() {
        assert_eq!(attribute_of(&TREE, "INBOX").unwrap(), "\\HasChildren");
        assert_eq!(attribute_of(&TREE, "INBOX/accounts").unwrap(), "\\HasChildren \\Noselect");
        assert_eq!(attribute_of(&TREE, "INBOX/accounts/work").unwrap(), "\\HasNoChildren");
        assert_eq!(attribute_of(&TREE, "Archive").unwrap(), "\\HasNoChildren");
    }

    #[test]
    fn keeps_a_parent_has_children_when_the_pattern_filters_its_children_out() {
        let rows = attributes_for(&TREE, "%");
        assert_eq!(sorted(rows.iter().map(|(name, _)| name.clone()).collect()), ["Archive", "INBOX", "Sent Messages"]);
        assert_eq!(rows.iter().find(|(name, _)| name == "INBOX").unwrap().1, "\\HasChildren");
    }

    #[test]
    fn does_not_treat_a_mere_string_prefix_as_a_parent() {
        assert_eq!(attribute_of(&["INBOX", "Project", "Projects/Work"], "Project").unwrap(), "\\HasNoChildren");
    }

    #[test]
    fn derives_sent_messages_from_its_own_child_not_from_a_hardcoded_name() {
        let with_accounts = ["INBOX", "Sent Messages", "Sent Messages/accounts", "Sent Messages/accounts/work"];
        assert_eq!(attribute_of(&with_accounts, "Sent Messages").unwrap(), "\\HasChildren");
        assert_eq!(attribute_of(&["INBOX", "Sent Messages"], "Sent Messages").unwrap(), "\\HasNoChildren");
    }
}

// RFC 5258 §3: every \HasChildren is answerable by the `%` expansion of that
// same name, and every \HasNoChildren by its absence.
mod list_attribute_and_expansion_agree {
    use super::*;

    fn expansion_of(boxes: &[&str], parent: &str) -> Vec<String> {
        attributes_for(boxes, &format!("{parent}/%")).into_iter().map(|(name, _)| name).collect()
    }

    #[test]
    fn a_parent_with_a_child() {
        let boxes = ["INBOX", "Projects", "Projects/Work"];
        assert_eq!(attribute_of(&boxes, "Projects").unwrap(), "\\HasChildren");
        assert_eq!(expansion_of(&boxes, "Projects"), ["Projects/Work"]);
    }

    #[test]
    fn a_utility_folder_with_a_child_keeps_its_special_use_attribute() {
        let boxes = ["INBOX", "Drafts", "Drafts/sub"];
        assert_eq!(attribute_of(&boxes, "Drafts").unwrap(), "\\Drafts \\HasChildren");
        assert_eq!(expansion_of(&boxes, "Drafts"), ["Drafts/sub"]);
    }

    #[test]
    fn a_name_differing_only_in_leading_segment_case_is_not_a_child() {
        let boxes = ["INBOX", "Archive", "archive/old"];
        assert_eq!(attribute_of(&boxes, "Archive").unwrap(), "\\HasNoChildren");
        assert!(expansion_of(&boxes, "Archive").is_empty());
    }

    #[test]
    fn the_same_holds_for_the_reserved_names_each_reachable_under_its_own_spelling() {
        let boxes = ["INBOX", "Drafts", "inbox/foo", "drafts/sub"];
        assert_eq!(attribute_of(&boxes, "INBOX").unwrap(), "\\HasNoChildren");
        assert_eq!(attribute_of(&boxes, "Drafts").unwrap(), "\\Drafts \\HasNoChildren");
        assert!(expansion_of(&boxes, "INBOX").is_empty());
        assert_eq!(expansion_of(&boxes, "inbox"), ["inbox/foo"]);
        assert_eq!(expansion_of(&boxes, "drafts"), ["drafts/sub"]);
    }
}

// A mailbox name reaches the wire as a quoted string; emitted raw, a quote
// or backslash in it desyncs LIST / LSUB for the whole session.
mod list_quotes_mailbox_names {
    use super::*;

    const HOSTILE: [&str; 3] = ["a\"b", "back\\slash", "tail\\"];

    /// One RFC 3501 quoted string starting at or after `from`, decoded.
    fn read_quoted(line: &str, from: usize) -> String {
        let bytes = line.as_bytes();
        let mut index = from + line[from..].find('"').expect("a quoted string") + 1;
        let mut out = String::new();
        loop {
            assert!(index < bytes.len(), "unterminated quoted string in {line:?}");
            match bytes[index] {
                b'\\' => {
                    let next = bytes[index + 1];
                    assert!(next == b'"' || next == b'\\', "illegal escape in {line:?}");
                    out.push(next as char);
                    index += 2;
                }
                b'"' => return out,
                other => {
                    out.push(other as char);
                    index += 1;
                }
            }
        }
    }

    /// The second quoted string: `* LIST (attrs) "/" "box"`.
    fn quoted_name_of(line: &str) -> String {
        let delimiter_start = line.find('"').unwrap();
        let after_delimiter = delimiter_start + 1 + line[delimiter_start + 1..].find('"').unwrap() + 1;
        read_quoted(line, after_delimiter)
    }

    fn entries(names: &[&str]) -> Vec<MailboxEntry> {
        names.iter().map(|name| MailboxEntry { name: name.to_string(), subscribed: true }).collect()
    }

    #[test]
    fn list_round_trips_a_name_containing_a_quote_or_a_backslash() {
        let lines: Vec<String> =
            list_response("A1", "", "*", &HOSTILE).into_iter().filter(|line| line.starts_with("* LIST")).collect();
        assert_eq!(lines.len(), HOSTILE.len());
        assert_eq!(sorted(lines.iter().map(|line| quoted_name_of(line)).collect()), sorted(HOSTILE.map(String::from).to_vec()));
    }

    #[test]
    fn lsub_round_trips_the_same_names() {
        let lines: Vec<String> =
            lsub_response("A1", "", "*", entries(&HOSTILE)).into_iter().filter(|line| line.starts_with("* LSUB")).collect();
        assert_eq!(lines.len(), HOSTILE.len());
        assert_eq!(sorted(lines.iter().map(|line| quoted_name_of(line)).collect()), sorted(HOSTILE.map(String::from).to_vec()));
    }

    #[test]
    fn quotes_the_reference_echoed_back_for_an_empty_pattern() {
        let none: [&str; 0] = [];
        let list: Vec<String> =
            list_response("A1", "ref\"x", "", &none).into_iter().filter(|line| line.starts_with("* LIST")).collect();
        assert_eq!(list.len(), 1);
        assert_eq!(read_quoted(&list[0], list[0].find("\"/\"").unwrap() + 3), "ref\"x");

        let lsub: Vec<String> =
            lsub_response("A1", "ref\"x", "", Vec::new()).into_iter().filter(|line| line.starts_with("* LSUB")).collect();
        assert_eq!(lsub.len(), 1);
        assert_eq!(read_quoted(&lsub[0], lsub[0].find("\"/\"").unwrap() + 3), "ref\"x");
    }
}

// mailbox-lsub-subscription.test.ts
mod lsub {
    use super::*;

    fn entry(name: &str, subscribed: bool) -> MailboxEntry {
        MailboxEntry { name: name.to_string(), subscribed }
    }

    fn tree() -> Vec<MailboxEntry> {
        vec![
            entry("INBOX", true),
            entry("Sent Messages", true),
            entry("Sent Messages/accounts", true),
            entry("Sent Messages/accounts/work", true),
            entry("INBOX/accounts", true),
            entry("INBOX/accounts/work", true),
            entry("ZzSubYes", true),
            entry("ZzSubNo", false),
        ]
    }

    fn hier() -> Vec<MailboxEntry> {
        vec![entry("INBOX", true), entry("Projects", false), entry("Projects/Work", true)]
    }

    fn emit(reference: &str, pattern: &str, entries: Vec<MailboxEntry>) -> Vec<String> {
        lsub_response("A1", reference, pattern, entries)
    }

    fn names_of(lines: &[String]) -> Vec<String> {
        lines.iter().filter(|line| line.starts_with("* LSUB")).map(|line| name_of(line)).collect()
    }

    #[test]
    fn omits_an_unsubscribed_user_created_mailbox() {
        assert_eq!(names_of(&emit("", "ZzSub*", tree())), ["ZzSubYes"]);
    }

    #[test]
    fn list_still_returns_the_unsubscribed_sibling() {
        let names: Vec<String> = tree().into_iter().map(|entry| entry.name).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(sorted(listed_in(&names, "", "ZzSub*")), ["ZzSubNo", "ZzSubYes"]);
    }

    #[test]
    fn keeps_inbox_which_has_no_row_to_unsubscribe_from() {
        let names = names_of(&emit("", "*", tree()));
        for name in ["INBOX", "Sent Messages", "INBOX/accounts"] {
            assert!(names.contains(&name.to_string()), "{name}");
        }
        assert!(!names.contains(&"ZzSubNo".to_string()));
    }

    #[test]
    fn returns_nothing_when_every_user_mailbox_is_unsubscribed_except_the_derived_set() {
        assert!(names_of(&emit("", "Zz*", vec![entry("INBOX", true), entry("ZzSubNo", false)])).is_empty());
    }

    #[test]
    fn takes_sent_messages_attributes_from_the_full_set_even_when_its_children_are_filtered_out() {
        let lines = emit(
            "",
            "Sent Messages",
            vec![
                entry("Sent Messages", true),
                entry("Sent Messages/accounts", false),
                entry("Sent Messages/accounts/work", false),
            ],
        );
        assert!(lines.iter().any(|line| line.contains("(\\HasChildren)")));
    }

    // RFC 3501 §6.3.9: an unsubscribed parent of a subscribed child stays
    // visible as \Noselect.
    #[test]
    fn a_percent_still_returns_the_unsubscribed_parent() {
        assert!(names_of(&emit("", "%", hier())).contains(&"Projects".to_string()));
    }

    #[test]
    fn marks_that_parent_has_children_noselect() {
        assert!(emit("", "%", hier()).contains(&"* LSUB (\\HasChildren \\Noselect) \"/\" \"Projects\"\r\n".to_string()));
    }

    #[test]
    fn drops_the_parent_once_no_descendant_is_subscribed() {
        assert!(names_of(&emit("", "%", vec![entry("Projects", false), entry("Projects/Work", false)])).is_empty());
    }

    #[test]
    fn promotes_every_ancestor_of_a_subscribed_leaf() {
        let deep = || vec![entry("Projects", false), entry("Projects/Work", false), entry("Projects/Work/Q3", true)];
        assert_eq!(names_of(&emit("", "%", deep())), ["Projects"]);
        assert_eq!(names_of(&emit("", "Projects/%", deep())), ["Projects/Work"]);
        assert_eq!(names_of(&emit("", "Projects/Work/%", deep())), ["Projects/Work/Q3"]);
    }

    #[test]
    fn does_not_promote_a_name_that_is_only_a_string_prefix() {
        let names = names_of(&emit("", "%", vec![entry("Project", false), entry("Projects/Work", true)]));
        assert_eq!(names, ["Projects"]);
    }

    #[test]
    fn promotes_an_ancestor_that_has_no_mailboxes_row_of_its_own() {
        let lines = emit("", "%", vec![entry("INBOX", true), entry("Projects/Work", true)]);
        assert_eq!(sorted(names_of(&lines)), ["INBOX", "Projects"]);
        assert!(lines.contains(&"* LSUB (\\HasChildren \\Noselect) \"/\" \"Projects\"\r\n".to_string()));
    }

    #[test]
    fn reports_a_subscribed_parent_has_children_too() {
        let lines = emit("", "%", vec![entry("Projects", true), entry("Projects/Work", true)]);
        assert!(lines.contains(&"* LSUB (\\HasChildren) \"/\" \"Projects\"\r\n".to_string()));
    }

    #[test]
    fn does_not_promote_under_a_star() {
        let names = names_of(&emit("", "*", hier()));
        assert_eq!(sorted(names), ["INBOX", "Projects/Work"]);
    }

    #[test]
    fn an_empty_pattern_still_returns_the_hierarchy_delimiter_and_no_mailboxes() {
        let lines = emit("", "", tree());
        assert!(lines.iter().any(|line| line.contains("(\\Noselect) \"/\" \"\"")));
        assert_eq!(lines.iter().filter(|line| line.starts_with("* LSUB")).count(), 1);
    }

    #[test]
    fn applies_the_pattern_on_top_of_the_subscription_filter() {
        let names = names_of(&emit("", "%", tree()));
        for name in ["INBOX", "Sent Messages", "ZzSubYes"] {
            assert!(names.contains(&name.to_string()), "{name}");
        }
        assert!(!names.contains(&"INBOX/accounts".to_string()));
    }

    #[test]
    fn concatenates_a_non_empty_reference_with_the_pattern() {
        assert_eq!(names_of(&emit("INBOX/", "%", tree())), ["INBOX/accounts"]);
    }

    #[test]
    fn promotes_an_ancestor_under_a_non_empty_reference() {
        let lines = emit(
            "Projects/",
            "%",
            vec![entry("Projects", true), entry("Projects/Work", false), entry("Projects/Work/Q3", true)],
        );
        assert_eq!(names_of(&lines), ["Projects/Work"]);
        assert!(lines.contains(&"* LSUB (\\HasChildren \\Noselect) \"/\" \"Projects/Work\"\r\n".to_string()));
    }

    // A Bun oddity, pinned as the Bun suite pins it: two rows for what RFC
    // 3501 §5.1 calls one mailbox, `INBOX` selectable and `inbox` \Noselect.
    #[test]
    fn keeps_its_own_two_inbox_spellings_self_consistent_wrong_as_the_pair_is() {
        let boxes = || vec![entry("INBOX", true), entry("inbox/foo", true)];
        let lines = emit("", "%", boxes());
        assert!(lines.contains(&"* LSUB (\\HasNoChildren) \"/\" \"INBOX\"\r\n".to_string()));
        assert!(lines.contains(&"* LSUB (\\HasChildren \\Noselect) \"/\" \"inbox\"\r\n".to_string()));
        assert_eq!(names_of(&emit("", "inbox/%", boxes())), ["inbox/foo"]);
        assert!(names_of(&emit("", "INBOX/%", boxes())).is_empty());
    }
}

// utility-mailboxes.test.ts: LIST attributes
mod utility_attributes {
    use super::*;

    fn names() -> Vec<&'static str> {
        UTILITY_FOLDERS.iter().map(|folder| folder.name).collect()
    }

    fn attributes_of(mailbox: &str, boxes: &[&str]) -> String {
        get_mailbox_attributes(mailbox, &collect_ancestors(boxes))
    }

    #[test]
    fn reports_the_rfc_6154_special_use_attribute() {
        assert_eq!(attributes_of("Drafts", &names()), "\\Drafts \\HasNoChildren");
        assert_eq!(attributes_of("Junk", &names()), "\\Junk \\HasNoChildren");
        assert_eq!(attributes_of("Starred", &names()), "\\Flagged \\HasNoChildren");
        assert_eq!(attributes_of("Trash", &names()), "\\Trash \\HasNoChildren");
    }

    #[test]
    fn pairs_the_special_use_attribute_with_has_children_once_a_child_exists() {
        let mut boxes = names();
        boxes.push("Drafts/sub");
        assert_eq!(attributes_of("Drafts", &boxes), "\\Drafts \\HasChildren");
    }

    #[test]
    fn leaves_every_other_boxs_attributes_alone() {
        assert_eq!(attributes_of("Archive", &["Archive"]), "\\HasNoChildren");
        assert_eq!(
            attributes_of("INBOX/accounts", &["INBOX/accounts", "INBOX/accounts/work"]),
            "\\HasChildren \\Noselect"
        );
    }

    #[test]
    fn reports_a_childless_accounts_parent_has_no_children_still_noselect() {
        assert_eq!(attributes_of("INBOX/accounts", &["INBOX", "INBOX/accounts"]), "\\HasNoChildren \\Noselect");
        assert_eq!(
            attributes_of("Sent Messages/accounts", &["Sent Messages", "Sent Messages/accounts"]),
            "\\HasNoChildren \\Noselect"
        );
    }

    #[test]
    fn ancestors_are_every_proper_prefix_ending_at_a_delimiter() {
        let ancestors = collect_ancestors(&["a/b/c", "x"]);
        let mut ancestors: Vec<String> = ancestors.into_iter().collect();
        ancestors.sort();
        assert_eq!(ancestors, ["a", "a/b"]);
    }
}

// The refusals CREATE / DELETE / RENAME give before any database work
// (utility-mailboxes, inbox-case-insensitive and mailbox-name-limit).
mod refusals_before_the_store {
    use super::*;

    const LIMIT: usize = 255;
    const ASTRAL: &str = "\u{1F600}";

    fn store() -> Store {
        Store::new(SignedUser::new(Some("u1".into()), Some("admin".into()), None), crate::imap::store::StoreScope::whole_store(&crate::domains::account_domain("admin")))
    }

    async fn create(name: &str) -> String {
        let wire = wire_session();
        create_mailbox("A1", name, &store(), &wire.session).await;
        wire.wire().await
    }

    async fn delete(name: &str) -> String {
        let wire = wire_session();
        delete_mailbox("A1", name, &store(), &wire.session).await;
        wire.wire().await
    }

    async fn rename(from: &str, to: &str) -> String {
        let wire = wire_session();
        rename_mailbox("A1", from, to, &store(), &wire.session).await;
        wire.wire().await
    }

    #[tokio::test]
    async fn create_reports_a_utility_folder_as_existing() {
        for folder in UTILITY_FOLDERS {
            assert_eq!(create(folder.name).await, "A1 NO [ALREADYEXISTS] Mailbox already exists\r\n", "{}", folder.name);
        }
    }

    #[tokio::test]
    async fn delete_refuses_a_utility_folder() {
        for folder in UTILITY_FOLDERS {
            assert_eq!(delete(folder.name).await, "A1 NO [CANNOT] Cannot delete system mailbox\r\n", "{}", folder.name);
        }
    }

    #[tokio::test]
    async fn rename_refuses_a_utility_folder_as_source_and_as_target() {
        for folder in UTILITY_FOLDERS {
            assert_eq!(rename(folder.name, "Elsewhere").await, "A1 NO [CANNOT] Cannot rename system mailbox\r\n");
            assert_eq!(rename("Archive", folder.name).await, "A1 NO [ALREADYEXISTS] Target mailbox already exists\r\n");
        }
    }

    #[tokio::test]
    async fn create_inbox_is_already_existing_in_every_casing() {
        for name in ["INBOX", "inbox", "Inbox", "iNbOx"] {
            assert_eq!(create(name).await, "A1 NO [ALREADYEXISTS] Mailbox already exists\r\n", "{name}");
        }
    }

    #[tokio::test]
    async fn delete_inbox_is_refused_in_every_casing() {
        for name in ["INBOX", "inbox", "Inbox"] {
            assert_eq!(delete(name).await, "A1 NO [CANNOT] Cannot delete INBOX\r\n", "{name}");
        }
    }

    #[tokio::test]
    async fn rename_inbox_is_not_supported() {
        assert_eq!(rename("inbox", "NewName").await, "A1 NO [CANNOT] RENAME INBOX is not supported\r\n");
    }

    #[tokio::test]
    async fn rename_onto_inbox_is_already_existing() {
        assert_eq!(rename("Archive", "Inbox").await, "A1 NO [ALREADYEXISTS] Target mailbox already exists\r\n");
    }

    #[tokio::test]
    async fn create_refuses_one_byte_over_the_ceiling() {
        assert_eq!(create(&"a".repeat(LIMIT + 1)).await, format!("A1 NO [LIMIT] Mailbox name exceeds {LIMIT} bytes\r\n"));
    }

    #[tokio::test]
    async fn create_counts_utf8_bytes_not_code_units() {
        let over_by_bytes = ASTRAL.repeat(64);
        assert!(over_by_bytes.encode_utf16().count() < LIMIT);
        assert_eq!(over_by_bytes.len(), LIMIT + 1);
        assert_eq!(create(&over_by_bytes).await, format!("A1 NO [LIMIT] Mailbox name exceeds {LIMIT} bytes\r\n"));
    }

    #[tokio::test]
    async fn rename_refuses_an_over_length_target() {
        assert_eq!(rename("Archive", &"a".repeat(LIMIT + 1)).await, format!("A1 NO [LIMIT] Mailbox name exceeds {LIMIT} bytes\r\n"));
        assert_eq!(rename("Archive", &ASTRAL.repeat(64)).await, format!("A1 NO [LIMIT] Mailbox name exceeds {LIMIT} bytes\r\n"));
    }

    #[tokio::test]
    async fn an_empty_name_is_refused_by_each_command() {
        assert_eq!(create("\"\"").await, "A1 NO Empty mailbox name\r\n");
        assert_eq!(delete("\"\"").await, "A1 NO Empty mailbox name\r\n");
        assert_eq!(rename("\"\"", "x").await, "A1 NO Empty mailbox name\r\n");
    }
}
