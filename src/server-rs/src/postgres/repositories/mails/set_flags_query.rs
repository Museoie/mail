//! The SELECT / UPDATE pair one IMAP STORE runs
//! (repositories/mails/set-flags-query.ts), built without touching the pool
//! so the parameter numbering can be asserted directly.
//!
//! ```ignore
//! let queries = build_set_mail_flags_queries(user_id, mailbox, sent, start, end, true, "read = TRUE", false);
//! let mut values = queries.base_values.clone();
//! values.push(modseq.into());
//! pool().query(&queries.update_sql, &values).await?;
//! ```

use super::views::{membership_condition, membership_expression, uses_domain_uid_space};
use crate::params;
use crate::postgres::Param;

#[derive(Debug, Clone, PartialEq)]
pub struct SetMailFlagsQueries {
    pub select_sql: String,
    /// The same match as `select_sql`, projecting only `uid`. The conditional
    /// path reads it purely to diff matched against updated, so a
    /// `UID STORE 1:*` over a large mailbox has no reason to materialize
    /// eight flag columns per row.
    pub matched_uid_sql: String,
    pub update_sql: String,
    /// The values bound ahead of the two the caller appends: the freshly
    /// reserved mod-sequence, then the UNCHANGEDSINCE ceiling when
    /// `conditional`. So the stamped placeholder is `$(base_values.len() + 1)`
    /// and the guard is one past it.
    pub base_values: Vec<Param>,
}

/// Builds the statements for a STORE over `start..=end` of `mailbox`.
///
/// `start` and `end` are UIDs when `use_uid`, 1-based sequence numbers
/// otherwise. `set_clause` is the flag assignment list; with `conditional`
/// the UPDATE also requires the row's mod-sequence to be at most the value
/// the caller binds last.
#[allow(clippy::too_many_arguments)]
pub fn build_set_mail_flags_queries(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    start: i64,
    end: i64,
    use_uid: bool,
    set_clause: &str,
    conditional: bool,
) -> SetMailFlagsQueries {
    // A STORE addresses messages in the selected mailbox, so it has to see
    // the same set the reads do: otherwise `UID STORE 1:* +FLAGS (\Deleted)`
    // on INBOX would flag quarantined spam the client was never shown, and
    // the following EXPUNGE would destroy it.
    let membership = membership_condition(mailbox, sent, "");

    let Some(mailbox) = mailbox.filter(|name| !uses_domain_uid_space(Some(name))) else {
        // Domain-scoped: stays on `mails.uid_domain`.
        let returning_cols = "mail_id, uid_domain as uid, read, saved, deleted, draft, answered, modseq as modseq";
        let uid_col = "uid_domain as uid";
        let guard = if conditional { " AND modseq <= $6" } else { "" };
        let (where_clause, base_values) = if use_uid {
            (
                format!(
                    "user_id = $1 AND sent = $2 AND uid_domain >= $3 AND uid_domain <= $4\n        AND expunged = FALSE{membership}"
                ),
                params![user_id, sent, start, end],
            )
        } else {
            (
                format!(
                    "mail_id IN (\n        SELECT mail_id FROM mails\n        WHERE user_id = $1 AND sent = $2 AND expunged = FALSE{membership}\n        ORDER BY uid_domain ASC\n        OFFSET $3 LIMIT $4\n      )"
                ),
                params![user_id, sent, start - 1, end - start + 1],
            )
        };
        return SetMailFlagsQueries {
            select_sql: format!("SELECT {returning_cols} FROM mails WHERE {where_clause}"),
            matched_uid_sql: format!("SELECT {uid_col} FROM mails WHERE {where_clause}"),
            update_sql: format!(
                "UPDATE mails\n        SET {set_clause}, updated = CURRENT_TIMESTAMP, modseq = $5\n        WHERE {where_clause}{guard}\n        RETURNING {returning_cols}"
            ),
            base_values,
        };
    };

    // Per-mailbox: JOIN `mail_mailbox_uid` for both membership and UID.
    // RETURNING `x.uid`, the mailbox-specific UID the client sees; the
    // mapping table is its sole source.
    let returning_cols =
        "m.mail_id, x.uid as uid, m.read, m.saved, m.deleted, m.draft, m.answered, m.modseq as modseq";
    let uid_col = "x.uid as uid";
    let guard = if conditional { " AND m.modseq <= $7" } else { "" };
    let (where_clause, base_values) = if use_uid {
        (
            format!(
                "m.user_id = $1 AND m.sent = $2\n        AND x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id\n        AND x.uid >= $4 AND x.uid <= $5\n        AND m.expunged = FALSE{}",
                membership_condition(Some(mailbox), sent, "m.")
            ),
            params![user_id, sent, mailbox, start, end],
        )
    } else {
        // Sequence-number path: slice the mailbox's UID-ordered list by
        // position. That list is the one the sequence map is built from, so
        // the walk filters `sent` and `expunged` too, not just membership:
        // mapping rows outlive the expunge that hid their mail. The join
        // carries those filters unconditionally: the membership expression
        // is `TRUE` on a box that shows spam, where a gated join would drop
        // them.
        let membership_join = format!(
            "JOIN mails z ON z.user_id = y.user_id AND z.mail_id = y.mail_id\n           AND z.sent = $2 AND z.expunged = FALSE\n           AND {}",
            membership_expression(Some(mailbox), sent, "z.")
        );
        let target_subquery = format!(
            "(\n        SELECT y.mail_id FROM mail_mailbox_uid y\n        {membership_join}\n        WHERE y.user_id = $1 AND y.mailbox = $3\n        ORDER BY y.uid ASC\n        OFFSET $4 LIMIT $5\n      )"
        );
        (
            format!(
                "m.user_id = $1 AND m.sent = $2\n        AND x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id\n        AND m.mail_id IN {target_subquery}"
            ),
            params![user_id, sent, mailbox, start - 1, end - start + 1],
        )
    };
    SetMailFlagsQueries {
        select_sql: format!("SELECT {returning_cols} FROM mails m, mail_mailbox_uid x WHERE {where_clause}"),
        matched_uid_sql: format!("SELECT {uid_col} FROM mails m, mail_mailbox_uid x WHERE {where_clause}"),
        // UPDATE ... FROM joins the mapping to the target mails rows; rows
        // matching the join get updated once.
        update_sql: format!(
            "UPDATE mails m\n        SET {set_clause}, updated = CURRENT_TIMESTAMP, modseq = $6\n        FROM mail_mailbox_uid x\n        WHERE {where_clause}{guard}\n        RETURNING {returning_cols}"
        ),
        base_values,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "user-1";
    const START: i64 = 3;
    const END: i64 = 9;
    const SET_CLAUSE: &str = "read = TRUE";

    fn build(mailbox: Option<&str>, sent: bool, use_uid: bool, conditional: bool) -> SetMailFlagsQueries {
        build_set_mail_flags_queries(USER, mailbox, sent, START, END, use_uid, SET_CLAUSE, conditional)
    }

    struct Branch {
        label: &'static str,
        mailbox: Option<&'static str>,
        sent: bool,
        use_uid: bool,
        /// The alias `expunged` wears where this branch applies it.
        expunged: &'static str,
        /// Membership terms the box contributes; empty for a box that shows everything.
        membership: Vec<&'static str>,
        base_values: Vec<Param>,
    }

    fn branches() -> Vec<Branch> {
        vec![
            Branch {
                label: "domain UID range, INBOX tree",
                mailbox: None,
                sent: false,
                use_uid: true,
                expunged: "expunged",
                membership: vec!["is_spam = FALSE", "draft = FALSE"],
                base_values: params![USER, false, START, END],
            },
            Branch {
                label: "domain sequence, INBOX tree",
                mailbox: None,
                sent: false,
                use_uid: false,
                expunged: "expunged",
                membership: vec!["is_spam = FALSE", "draft = FALSE"],
                base_values: params![USER, false, START - 1, END - START + 1],
            },
            Branch {
                label: "domain UID range, sent lane (filters nothing)",
                mailbox: None,
                sent: true,
                use_uid: true,
                expunged: "expunged",
                membership: vec![],
                base_values: params![USER, true, START, END],
            },
            Branch {
                label: "domain sequence, sent lane (filters nothing)",
                mailbox: None,
                sent: true,
                use_uid: false,
                expunged: "expunged",
                membership: vec![],
                base_values: params![USER, true, START - 1, END - START + 1],
            },
            Branch {
                label: "utility view stays in the domain UID space",
                mailbox: Some("Junk"),
                sent: false,
                use_uid: true,
                expunged: "expunged",
                membership: vec!["is_spam = TRUE"],
                base_values: params![USER, false, START, END],
            },
            Branch {
                label: "mapped UID range, INBOX tree",
                mailbox: Some("INBOX/accounts/someone"),
                sent: false,
                use_uid: true,
                expunged: "m.expunged",
                membership: vec!["m.is_spam = FALSE", "m.draft = FALSE"],
                base_values: params![USER, false, "INBOX/accounts/someone", START, END],
            },
            Branch {
                label: "mapped UID range, box that filters nothing",
                mailbox: Some("Archive"),
                sent: false,
                use_uid: true,
                expunged: "m.expunged",
                membership: vec![],
                base_values: params![USER, false, "Archive", START, END],
            },
            Branch {
                label: "mapped sequence, INBOX tree",
                mailbox: Some("INBOX/accounts/someone"),
                sent: false,
                use_uid: false,
                expunged: "z.expunged",
                membership: vec!["z.is_spam = FALSE", "z.draft = FALSE"],
                base_values: params![USER, false, "INBOX/accounts/someone", START - 1, END - START + 1],
            },
            Branch {
                label: "mapped sequence, box that filters nothing",
                mailbox: Some("Archive"),
                sent: false,
                use_uid: false,
                expunged: "z.expunged",
                membership: vec![],
                base_values: params![USER, false, "Archive", START - 1, END - START + 1],
            },
        ]
    }

    /// Every distinct `$N` the statement binds, ascending.
    fn param_indices(sql: &str) -> Vec<usize> {
        let bytes = sql.as_bytes();
        let mut found = std::collections::BTreeSet::new();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'$' {
                let digits: String =
                    sql[at + 1..].chars().take_while(char::is_ascii_digit).collect();
                if let Ok(index) = digits.parse::<usize>() {
                    found.insert(index);
                }
            }
            at += 1;
        }
        found.into_iter().collect()
    }

    fn up_to(n: usize) -> Vec<usize> {
        (1..=n).collect()
    }

    /// Everything from the `FROM` onwards: the match, without the projection.
    fn match_of(sql: &str) -> &str {
        &sql[sql.find(" FROM ").expect("the statement has a FROM")..]
    }

    // set-flags-query.test.ts — "the emitted match"

    #[test]
    fn never_addresses_expunged_mail() {
        for branch in branches() {
            let queries = build(branch.mailbox, branch.sent, branch.use_uid, false);
            for sql in [&queries.select_sql, &queries.matched_uid_sql, &queries.update_sql] {
                assert!(sql.contains(&format!("{} = FALSE", branch.expunged)), "{}", branch.label);
            }
        }
    }

    #[test]
    fn keeps_the_sequence_walks_own_filters_on_a_box_whose_membership_rule_is_true() {
        let queries = build(Some("Archive"), false, false, false);
        assert!(queries.select_sql.contains("z.sent = $2"));
        assert!(queries.select_sql.contains("z.expunged = FALSE"));
        assert!(queries.select_sql.contains("AND TRUE"));
    }

    #[test]
    fn applies_exactly_its_own_membership_rule() {
        for branch in branches() {
            let queries = build(branch.mailbox, branch.sent, branch.use_uid, false);
            for sql in [&queries.select_sql, &queries.matched_uid_sql, &queries.update_sql] {
                for term in &branch.membership {
                    assert!(sql.contains(term), "{}", branch.label);
                }
                if branch.membership.is_empty() {
                    assert!(!sql.contains("is_spam"), "{}", branch.label);
                }
            }
        }
    }

    #[test]
    fn projects_the_uid_only_match_over_the_same_rows() {
        for branch in branches() {
            let queries = build(branch.mailbox, branch.sent, branch.use_uid, false);
            assert_eq!(match_of(&queries.matched_uid_sql), match_of(&queries.select_sql), "{}", branch.label);
            let projection = &queries.matched_uid_sql[..queries.matched_uid_sql.find(" FROM ").unwrap()];
            assert!(!projection.contains("read"), "{}", branch.label);
        }
    }

    // set-flags-query.test.ts — "the emitted range"

    #[test]
    fn slices_the_one_based_range_as_offset_limit() {
        for branch in branches().into_iter().filter(|branch| !branch.use_uid) {
            let queries = build(branch.mailbox, branch.sent, branch.use_uid, false);
            let offset = queries.base_values.len() - 1;
            let limit = queries.base_values.len();
            for sql in [&queries.select_sql, &queries.update_sql] {
                assert!(sql.contains(&format!("OFFSET ${offset} LIMIT ${limit}")), "{}", branch.label);
            }
            assert_eq!(queries.base_values[offset - 1], Param::from(START - 1), "{}", branch.label);
            assert_eq!(queries.base_values[limit - 1], Param::from(END - START + 1), "{}", branch.label);
        }
    }

    #[test]
    fn bounds_the_uid_range_instead_of_paging_it() {
        for branch in branches().into_iter().filter(|branch| branch.use_uid) {
            let queries = build(branch.mailbox, branch.sent, branch.use_uid, false);
            for sql in [&queries.select_sql, &queries.update_sql] {
                assert!(!sql.contains("OFFSET"), "{}", branch.label);
                assert!(!sql.contains("LIMIT"), "{}", branch.label);
            }
        }
    }

    #[test]
    fn binds_the_values_its_placeholders_name() {
        for branch in branches() {
            assert_eq!(
                build(branch.mailbox, branch.sent, branch.use_uid, false).base_values,
                branch.base_values,
                "{}",
                branch.label
            );
        }
    }

    // set-flags-query.test.ts — "placeholder numbering"

    #[test]
    fn numbers_the_match_contiguously_from_one() {
        for branch in branches() {
            let queries = build(branch.mailbox, branch.sent, branch.use_uid, false);
            for sql in [&queries.select_sql, &queries.matched_uid_sql] {
                assert_eq!(param_indices(sql), up_to(queries.base_values.len()), "{}", branch.label);
            }
        }
    }

    #[test]
    fn appends_the_stamped_mod_sequence_past_the_bound_values() {
        for branch in branches() {
            let queries = build(branch.mailbox, branch.sent, branch.use_uid, false);
            let stamped = queries.base_values.len() + 1;
            assert_eq!(param_indices(&queries.update_sql), up_to(stamped), "{}", branch.label);
            assert!(queries.update_sql.contains(&format!("modseq = ${stamped}")), "{}", branch.label);
        }
    }

    #[test]
    fn guards_on_one_index_past_the_stamped_mod_sequence() {
        for branch in branches() {
            let queries = build(branch.mailbox, branch.sent, branch.use_uid, true);
            let guard = queries.base_values.len() + 2;
            assert_eq!(param_indices(&queries.update_sql), up_to(guard), "{}", branch.label);
            assert!(queries.update_sql.contains(&format!("modseq <= ${guard}")), "{}", branch.label);
            assert!(!queries.select_sql.contains("modseq <="), "{}", branch.label);
        }
    }
}
