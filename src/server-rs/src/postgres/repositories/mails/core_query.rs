//! The mutation SQL the core mail routes issue (repositories/mails/core-query.ts),
//! built with no pool involved so the emitted string and its bound parameters
//! can be asserted directly.
//!
//! ```ignore
//! let update = build_mark_mail_spam_query(user_id, mail_id, is_spam, modseq);
//! let result = run_query(&update).await?;
//! ```

use super::table::PreparedQuery;
use crate::params;

/// `is_spam IS DISTINCT FROM $1` makes a re-mark of the same value match no
/// row, so the reserved mod-sequence goes unused and a CONDSTORE client is
/// not told a mail changed when nothing did. The flip is a membership change
/// (it moves the mail out of INBOX), so it has to stamp `modseq` or the
/// client reads an unchanged HIGHESTMODSEQ and never resyncs.
///
/// An expunged row matches neither this nor [`build_mail_exists_query`]: it is
/// kept only so the delta can report it gone, and to a route that takes a
/// mail id it is as missing as one that never existed.
pub fn build_mark_mail_spam_query(user_id: &str, mail_id: &str, is_spam: bool, modseq: i64) -> PreparedQuery {
    PreparedQuery {
        sql: "UPDATE mails SET is_spam = $1, updated = NOW(), modseq = $4\n       WHERE mail_id = $2 AND user_id = $3 AND expunged = FALSE\n         AND is_spam IS DISTINCT FROM $1\n       RETURNING mail_id"
            .to_string(),
        values: params![is_spam, mail_id, user_id, modseq],
    }
}

/// Distinguishes "no such mail" from "the update matched nothing".
pub fn build_mail_exists_query(user_id: &str, mail_id: &str) -> PreparedQuery {
    PreparedQuery {
        sql: "SELECT 1 FROM mails WHERE mail_id = $1 AND user_id = $2 AND expunged = FALSE LIMIT 1".to_string(),
        values: params![mail_id, user_id],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::Param;

    const USER: &str = "11111111-1111-1111-1111-111111111111";
    const MAIL: &str = "22222222-2222-2222-2222-222222222222";

    // core-query.test.ts — "buildMarkMailSpamQuery"

    #[test]
    fn mark_spam_stamps_the_mod_sequence_the_caller_reserved() {
        let query = build_mark_mail_spam_query(USER, MAIL, true, 41);
        assert!(query.sql.contains("modseq = $4"));
        assert_eq!(query.values[3], Param::from(41));
    }

    #[test]
    fn mark_spam_matches_no_row_when_the_flag_already_holds_the_requested_value() {
        assert!(build_mark_mail_spam_query(USER, MAIL, true, 41).sql.contains("is_spam IS DISTINCT FROM $1"));
    }

    #[test]
    fn mark_spam_scopes_the_update_to_one_mail_owned_by_the_caller() {
        let query = build_mark_mail_spam_query(USER, MAIL, false, 7);
        assert!(query.sql.contains("WHERE mail_id = $2 AND user_id = $3"));
        assert_eq!(query.values, params![false, MAIL, USER, 7]);
    }

    #[test]
    fn mark_spam_refreshes_updated_so_delta_sync_clients_evict_the_row() {
        assert!(build_mark_mail_spam_query(USER, MAIL, true, 1).sql.contains("updated = NOW()"));
    }

    #[test]
    fn mark_spam_leaves_an_expunged_row_alone() {
        assert!(build_mark_mail_spam_query(USER, MAIL, true, 1).sql.contains("AND expunged = FALSE"));
    }

    #[test]
    fn mark_spam_returns_the_touched_id() {
        assert!(build_mark_mail_spam_query(USER, MAIL, true, 1).sql.contains("RETURNING mail_id"));
    }

    // core-query.test.ts — "buildMailExistsQuery"

    #[test]
    fn exists_probes_one_row_scoped_to_the_owner() {
        let query = build_mail_exists_query(USER, MAIL);
        assert!(query.sql.contains("WHERE mail_id = $1 AND user_id = $2"));
        assert!(query.sql.contains("LIMIT 1"));
        assert_eq!(query.values, params![MAIL, USER]);
    }

    #[test]
    fn exists_does_not_find_an_expunged_row() {
        assert!(build_mail_exists_query(USER, MAIL).sql.contains("AND expunged = FALSE"));
    }
}
