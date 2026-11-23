//! Which rows each IMAP mailbox contains, and which UID space it enumerates
//! (repositories/mails/views.ts).
//!
//! Two kinds of mailbox reach this repository:
//!
//!  - **Mapped boxes**: the per-account views (`INBOX/accounts/<local-part>`,
//!    `Sent Messages/accounts/<local-part>`) and user-created boxes.
//!    Membership is a row in `mail_mailbox_uid`, which also carries the
//!    per-box UID.
//!  - **Domain views**: `INBOX`, the unified `Sent Messages`, and the utility
//!    views below. Membership is a predicate over `mails` and UIDs come from
//!    `mails.uid_domain`. `INBOX` additionally carries a mapping row for every
//!    mail filed into its tree or into one of the utility views, holding that
//!    same `uid_domain`; no read consults it. Those rows are the tree's
//!    *scope*, not its membership: the predicate still applies on top.
//!
//! A mailbox is `None` for the two views with no name of their own, `INBOX`
//! and the unified `Sent Messages`.
//!
//! ```ignore
//! let filter = membership_condition(Some("Junk"), false, "m."); // " AND m.is_spam = TRUE"
//! ```

use super::domain_scope::{domains_array_literal, mail_in_domains_expression};

pub const INBOX_ACCOUNTS_PREFIX: &str = "INBOX/accounts/";

/// The name the INBOX tree's mapping rows are recorded under in
/// `mail_mailbox_uid`. Matched case-insensitively on the way in, per RFC 3501
/// §5.1, and stored in this spelling.
pub const INBOX_VIEW: &str = "INBOX";

/// Flag-derived views over the user's whole domain. Each is one predicate
/// over `mails`, so membership tracks the flag: a mail leaves `Drafts` the
/// moment `\Draft` is cleared.
pub const DRAFTS_VIEW: &str = "Drafts";
pub const JUNK_VIEW: &str = "Junk";

/// A mailbox's membership rule as `(column, required value)` pairs, in the
/// order they render.
pub type MembershipFilter = Vec<(&'static str, bool)>;

const DRAFT: &str = "draft";
const IS_SPAM: &str = "is_spam";

/// Looked up lowercase, because the LIST de-dup is case-insensitive: `drafts`
/// resolves to no listable box, so it must not resolve to a different UID
/// space either.
fn utility_membership(mailbox: Option<&str>) -> Option<MembershipFilter> {
    let lowered = mailbox?.to_lowercase();
    if lowered == DRAFTS_VIEW.to_lowercase() {
        Some(vec![(DRAFT, true)])
    } else if lowered == JUNK_VIEW.to_lowercase() {
        Some(vec![(IS_SPAM, true)])
    } else {
        None
    }
}

/// Whether `mailbox` names one of the flag-derived utility views.
pub fn is_utility_view(mailbox: Option<&str>) -> bool {
    utility_membership(mailbox).is_some()
}

/// Whether a mailbox enumerates `mails.uid_domain` rather than joining
/// `mail_mailbox_uid`. True for the two unnamed domain views and for the
/// utility views, which carry a name only so their membership rule can be
/// looked up.
pub fn uses_domain_uid_space(mailbox: Option<&str>) -> bool {
    mailbox.is_none() || is_utility_view(mailbox)
}

pub fn is_inbox_tree(mailbox: Option<&str>, sent: bool) -> bool {
    !sent && mailbox.is_none_or(|name| name.starts_with(INBOX_ACCOUNTS_PREFIX))
}

/// The domain view a write into `destination` records a mapping row under, or
/// `None` when the destination claims no INBOX scope. `destination` is the
/// box the write names: the wire box of an IMAP `COPY` / `MOVE` / `APPEND`,
/// and the per-account view of an SMTP delivery.
///
/// The utility views scope to INBOX alongside the tree: they select a row by
/// a flag the client can clear at any time, so an `APPEND Junk` followed by
/// "not spam" has to land the mail in INBOX. `Starred` and `Trash` are mapped
/// boxes holding a distinct clone, and are left unscoped.
pub fn domain_view_for_destination(destination: &str) -> Option<&'static str> {
    let scoped = destination.to_uppercase() == INBOX_VIEW
        || destination.starts_with(INBOX_ACCOUNTS_PREFIX)
        || is_utility_view(Some(destination));
    scoped.then_some(INBOX_VIEW)
}

pub fn membership_filter(mailbox: Option<&str>, sent: bool) -> MembershipFilter {
    if let Some(utility) = utility_membership(mailbox) {
        return utility;
    }
    if is_inbox_tree(mailbox, sent) {
        return vec![(IS_SPAM, false), (DRAFT, false)];
    }
    Vec::new()
}

/// The rule as a boolean SQL expression, `TRUE` for a box that filters
/// nothing. `prefix` qualifies each column, such as `"m."`.
///
/// With `domains`, the box additionally shows only the mail of those of the
/// owner's domains (`mail_in_domains_expression`): an IMAP login scoped to one
/// domain sees that domain's mail in every box. The names are validated and
/// inlined, so the fragment stays free of placeholders and appends to any query.
pub fn membership_expression(mailbox: Option<&str>, sent: bool, prefix: &str, domains: Option<&[String]>) -> String {
    let mut terms: Vec<String> = membership_filter(mailbox, sent)
        .into_iter()
        .map(|(column, value)| format!("{prefix}{column} = {}", if value { "TRUE" } else { "FALSE" }))
        .collect();
    if let Some(domains) = domains {
        terms.push(mail_in_domains_expression(&domains_array_literal(domains), prefix));
    }
    if terms.is_empty() {
        "TRUE".to_string()
    } else {
        terms.join(" AND ")
    }
}

/// The same rule as a suffix for an existing `WHERE`, empty when the box
/// filters nothing so it appends cleanly to any clause.
pub fn membership_condition(mailbox: Option<&str>, sent: bool, prefix: &str, domains: Option<&[String]>) -> String {
    let expression = membership_expression(mailbox, sent, prefix, domains);
    if expression == "TRUE" {
        String::new()
    } else {
        format!(" AND {expression}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // views.test.ts — "utility views"

    #[test]
    fn utility_views_select_their_rows_by_flag() {
        assert_eq!(membership_expression(Some(DRAFTS_VIEW), false, "", None), "draft = TRUE");
        assert_eq!(membership_expression(Some(JUNK_VIEW), false, "", None), "is_spam = TRUE");
        assert_eq!(membership_condition(Some(DRAFTS_VIEW), false, "", None), " AND draft = TRUE");
        assert_eq!(membership_condition(Some(JUNK_VIEW), false, "", None), " AND is_spam = TRUE");
    }

    #[test]
    fn utility_views_qualify_the_column_with_the_callers_alias() {
        assert_eq!(membership_expression(Some(DRAFTS_VIEW), false, "m.", None), "m.draft = TRUE");
        assert_eq!(membership_condition(Some(JUNK_VIEW), false, "m.", None), " AND m.is_spam = TRUE");
    }

    #[test]
    fn utility_views_read_the_domain_uid_space() {
        assert!(uses_domain_uid_space(Some(DRAFTS_VIEW)));
        assert!(uses_domain_uid_space(Some(JUNK_VIEW)));
        assert!(uses_domain_uid_space(None));
        assert!(!uses_domain_uid_space(Some("INBOX/accounts/alice")));
        assert!(!uses_domain_uid_space(Some("Archive")));
    }

    #[test]
    fn utility_views_are_matched_exactly_not_by_prefix() {
        assert!(!is_utility_view(Some("Drafts2")));
        assert!(!is_utility_view(Some("INBOX/Drafts")));
        assert!(!is_utility_view(None));
        assert!(!uses_domain_uid_space(Some("Drafts2")));
    }

    #[test]
    fn utility_views_do_not_inherit_a_prototype_key_as_a_view_name() {
        assert!(!is_utility_view(Some("constructor")));
        assert!(!is_utility_view(Some("toString")));
        assert_eq!(membership_expression(Some("constructor"), false, "", None), "TRUE");
    }

    #[test]
    fn utility_views_match_case_insensitively() {
        assert!(is_utility_view(Some("drafts")));
        assert!(is_utility_view(Some("JUNK")));
        assert!(uses_domain_uid_space(Some("drafts")));
        assert_eq!(membership_expression(Some("drafts"), false, "", None), "draft = TRUE");
        assert_eq!(membership_expression(Some("JUNK"), false, "", None), "is_spam = TRUE");
    }

    // views.test.ts — "INBOX"

    #[test]
    fn inbox_hides_spam_and_drafts() {
        assert_eq!(membership_expression(None, false, "", None), "is_spam = FALSE AND draft = FALSE");
        assert_eq!(
            membership_expression(Some("INBOX/accounts/alice"), false, "m.", None),
            "m.is_spam = FALSE AND m.draft = FALSE"
        );
    }

    #[test]
    fn inbox_stops_at_the_accounts_prefix_boundary() {
        assert_eq!(membership_expression(Some("INBOX/accounts"), false, "", None), "TRUE");
        assert_eq!(membership_expression(Some("INBOX/accountsish"), false, "", None), "TRUE");
    }

    #[test]
    fn inbox_does_not_exclude_deleted_mail() {
        let columns: Vec<&str> = membership_filter(None, false).into_iter().map(|(column, _)| column).collect();
        assert_eq!(columns, vec!["is_spam", "draft"]);
    }

    #[test]
    fn inbox_leaves_sent_mail_and_user_created_boxes_unfiltered() {
        assert_eq!(membership_expression(None, true, "", None), "TRUE");
        assert_eq!(membership_expression(Some("Sent Messages/accounts/alice"), true, "", None), "TRUE");
        assert_eq!(membership_expression(Some("Archive"), false, "", None), "TRUE");
        assert_eq!(membership_condition(Some("Archive"), false, "", None), "");
    }

    // views.test.ts — "the IMAP-side folder table agrees with the predicate".
    // The folder table lives in imap/util.ts, another slice; the declarations
    // it has to agree with are restated here until both land.

    /// Mirrors imap::util::UTILITY_FOLDERS; unify when both land.
    /// `(name, placement, domain-scoped)`.
    const UTILITY_FOLDERS: [(&str, (&str, bool), bool); 4] = [
        ("Drafts", ("draft", true), true),
        ("Junk", ("is_spam", true), true),
        ("Starred", ("saved", true), false),
        ("Trash", ("deleted", true), false),
    ];

    #[test]
    fn folder_table_names_the_same_domain_scoped_boxes() {
        let mut domain_scoped: Vec<&str> =
            UTILITY_FOLDERS.iter().filter(|folder| folder.2).map(|folder| folder.0).collect();
        domain_scoped.sort();
        let mut views = vec![DRAFTS_VIEW, JUNK_VIEW];
        views.sort();
        assert_eq!(domain_scoped, views);
    }

    #[test]
    fn folder_table_writes_exactly_the_flags_the_domain_scoped_view_reads() {
        for (name, placement, domain) in UTILITY_FOLDERS {
            if !domain {
                continue;
            }
            assert_eq!(membership_filter(Some(name), false), vec![placement], "no placement for {name}");
        }
    }

    #[test]
    fn folder_table_is_domain_scoped_on_the_imap_side_too() {
        for (name, _, domain) in UTILITY_FOLDERS {
            if domain {
                assert!(uses_domain_uid_space(Some(name)));
            }
        }
    }

    // views.test.ts — "domainViewForDestination"

    #[test]
    fn destination_records_the_unified_view_for_every_box_in_the_inbox_tree() {
        assert_eq!(domain_view_for_destination("INBOX"), Some(INBOX_VIEW));
        assert_eq!(domain_view_for_destination("INBOX/accounts/admin"), Some(INBOX_VIEW));
    }

    #[test]
    fn destination_matches_inbox_case_insensitively_and_stores_the_canonical_spelling() {
        assert_eq!(INBOX_VIEW, "INBOX");
        assert_eq!(domain_view_for_destination("inbox"), Some(INBOX_VIEW));
        assert_eq!(domain_view_for_destination("Inbox/accounts/admin"), None);
        assert!(!is_inbox_tree(Some("Inbox/accounts/admin"), false));
    }

    #[test]
    fn destination_records_nothing_outside_the_tree() {
        assert_eq!(domain_view_for_destination("Archive"), None);
        assert_eq!(domain_view_for_destination("Sent Messages"), None);
        assert_eq!(domain_view_for_destination("Sent Messages/accounts/admin"), None);
    }

    #[test]
    fn destination_records_the_unified_view_for_the_flag_derived_utility_destinations() {
        assert_eq!(domain_view_for_destination(JUNK_VIEW), Some(INBOX_VIEW));
        assert_eq!(domain_view_for_destination(DRAFTS_VIEW), Some(INBOX_VIEW));
        assert_eq!(domain_view_for_destination("junk"), Some(INBOX_VIEW));
        assert_eq!(domain_view_for_destination("DRAFTS"), Some(INBOX_VIEW));
    }

    #[test]
    fn destination_records_nothing_for_the_mapped_utility_destinations() {
        assert_eq!(domain_view_for_destination("Starred"), None);
        assert_eq!(domain_view_for_destination("Trash"), None);
    }

    #[test]
    fn destination_scopes_every_domain_scoped_utility_folder() {
        for (name, _, domain) in UTILITY_FOLDERS {
            assert_eq!(domain_view_for_destination(name), domain.then_some(INBOX_VIEW), "{name}");
        }
    }

    #[test]
    fn destination_does_not_treat_a_box_that_merely_starts_with_inbox_as_the_tree() {
        assert_eq!(domain_view_for_destination("INBOXER"), None);
        assert_eq!(domain_view_for_destination("INBOX/keep"), None);
    }

    #[test]
    fn destination_agrees_with_the_read_side_twin() {
        let cases: [(&str, Option<&str>); 4] = [
            ("INBOX", None),
            ("INBOX/accounts/admin", Some("INBOX/accounts/admin")),
            ("Archive", Some("Archive")),
            ("Sent Messages/accounts/admin", Some("Sent Messages/accounts/admin")),
        ];
        for (destination, read_side) in cases {
            assert_eq!(
                domain_view_for_destination(destination).is_some(),
                is_inbox_tree(read_side, false),
                "{destination}"
            );
        }
    }
}
