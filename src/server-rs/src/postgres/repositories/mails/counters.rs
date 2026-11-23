//! UID and mod-sequence allocation (repositories/mails/counters.ts).
//!
//! `mail_uid_counters` is the authority that assigns IMAP UIDs and
//! mod-sequences: one row per (user, kind, scope, sent), incremented by an
//! atomic `INSERT … ON CONFLICT DO UPDATE`, which is what makes concurrent
//! mail receipt race-free. `mail_mailbox_uid` records the UID one mail holds
//! in one mailbox.
//!
//! ```ignore
//! let uid = get_domain_uid_next(user_id, false).await?;                       // allocates
//! let next = get_uid_next(user_id, &UidScope::Domain { sent: false }).await?; // only reads
//! ```

use serde_json::{json, Value};

use super::table::{run_query, PreparedQuery, Record};
use super::views::{membership_condition, uses_domain_uid_space};
use crate::postgres::{DbError, DbResult, Param};
use crate::{logger, params};

/// The seed a counter row starts from: a query for one past the highest value
/// already assigned in the scope, and the parameters it binds from `$5` on.
struct Seed {
    sql: String,
    params: Vec<Param>,
}

fn counter_values(user_id: &str, kind: &str, scope: &str, sent: bool, seed_params: Vec<Param>) -> Vec<Param> {
    let mut values = params![user_id, kind, scope, sent];
    values.extend(seed_params);
    values
}

fn build_reserve_uid_query(user_id: &str, kind: &str, scope: &str, sent: bool, seed: Seed) -> PreparedQuery {
    let sql = format!(
        "\n    INSERT INTO mail_uid_counters (user_id, uid_kind, uid_scope, sent, last_uid)\n    VALUES ($1, $2, $3, $4, ({}))\n    ON CONFLICT (user_id, uid_kind, uid_scope, sent)\n    DO UPDATE SET last_uid = mail_uid_counters.last_uid + 1\n    RETURNING last_uid AS next_uid\n  ",
        seed.sql
    );
    PreparedQuery { sql, values: counter_values(user_id, kind, scope, sent, seed.params) }
}

/// The non-allocating counterpart of the reservation: what the NEXT
/// reservation would return, without consuming it. This is UIDNEXT
/// (RFC 3501 §2.3.1.1), which must exceed every UID ever assigned in the
/// scope and must never decrease, so it has to come from the counter that
/// assigns UIDs, not from a `MAX(uid)` over surviving rows.
///
/// `COALESCE` covers the pre-reservation state: a scope whose counter row
/// does not exist yet answers with the very seed the first reservation would
/// insert, so a peek and the allocation it predicts can never disagree.
fn build_peek_uid_next_query(user_id: &str, kind: &str, scope: &str, sent: bool, seed: Seed) -> PreparedQuery {
    let sql = format!(
        "\n    SELECT COALESCE(\n      (\n        SELECT last_uid + 1 FROM mail_uid_counters\n        WHERE user_id = $1 AND uid_kind = $2\n          AND uid_scope = $3 AND sent = $4\n      ),\n      ({})\n    ) AS uid_next\n  ",
        seed.sql
    );
    PreparedQuery { sql, values: counter_values(user_id, kind, scope, sent, seed.params) }
}

/// Seed for the domain-wide sequence, shared by the reservation and the peek.
fn domain_seed() -> Seed {
    Seed {
        sql: "\n      SELECT COALESCE(MAX(uid_domain), 0) + 1 FROM mails\n      WHERE user_id = $1 AND sent = $4\n    "
            .to_string(),
        params: Vec::new(),
    }
}

/// Domain-wide UID-reservation query (kind `domain`, no scope).
pub fn build_domain_uid_query(user_id: &str, sent: bool) -> PreparedQuery {
    build_reserve_uid_query(user_id, "domain", "", sent, domain_seed())
}

/// Domain-wide UIDNEXT peek: the same counter row as `build_domain_uid_query`.
pub fn build_domain_uid_next_query(user_id: &str, sent: bool) -> PreparedQuery {
    build_peek_uid_next_query(user_id, "domain", "", sent, domain_seed())
}

/// Seed for a per-account sequence, shared by the reservation and the peek.
///
/// Sources from `mail_mailbox_uid.uid`, the authoritative per-mailbox UID
/// store. The predicate matches the mailbox paths the write side derives
/// from (address, sent): the per-account path `INBOX/accounts/<local>` or
/// `Sent Messages/accounts/<local>`, plus the raw local part for
/// user-created mailboxes, where the write side stores the box name
/// unchanged.
fn account_seed(account: &str, sent: bool) -> Seed {
    let local_part = account.split('@').next().unwrap_or("");
    let per_account_path = if sent {
        format!("Sent Messages/accounts/{local_part}")
    } else {
        format!("INBOX/accounts/{local_part}")
    };
    Seed {
        sql: "\n      SELECT COALESCE(MAX(uid), 0) + 1 FROM mail_mailbox_uid\n      WHERE user_id = $1\n        AND mailbox IN ($5, $6)\n    "
            .to_string(),
        params: params![per_account_path, local_part],
    }
}

/// Per-account UID-reservation query (kind `account`, scope = address).
pub fn build_account_uid_query(user_id: &str, account: &str, sent: bool) -> PreparedQuery {
    build_reserve_uid_query(user_id, "account", account, sent, account_seed(account, sent))
}

/// Per-account UIDNEXT peek: the same counter row as `build_account_uid_query`.
pub fn build_account_uid_next_query(user_id: &str, account: &str, sent: bool) -> PreparedQuery {
    build_peek_uid_next_query(user_id, "account", account, sent, account_seed(account, sent))
}

/// Seed for a mapped-utility sequence, shared by the reservation and the peek.
///
/// Scoped to the single `mail_mailbox_uid.mailbox` value the pivot rows
/// carry, with no `sent` axis: `Starred` and `Trash` are one mailbox each,
/// not a received/sent pair, so `(user, mailbox)` is the whole key.
fn mailbox_seed(mailbox: &str) -> Seed {
    Seed {
        sql: "\n      SELECT COALESCE(MAX(uid), 0) + 1 FROM mail_mailbox_uid\n      WHERE user_id = $1 AND mailbox = $5\n    "
            .to_string(),
        params: params![mailbox],
    }
}

/// Mapped-utility UID-reservation query (kind `mailbox`, scope = box name).
pub fn build_mailbox_uid_query(user_id: &str, mailbox: &str) -> PreparedQuery {
    build_reserve_uid_query(user_id, "mailbox", mailbox, false, mailbox_seed(mailbox))
}

/// Mapped-utility UIDNEXT peek: the same counter row as `build_mailbox_uid_query`.
pub fn build_mailbox_uid_next_query(user_id: &str, mailbox: &str) -> PreparedQuery {
    build_peek_uid_next_query(user_id, "mailbox", mailbox, false, mailbox_seed(mailbox))
}

/// A counter value as the statement returned it; a missing or zero value
/// reads as 1.
fn counter_value(row: Option<&Record>, column: &str) -> i64 {
    row.and_then(|row| row.get(column))
        .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|number| number as i64)))
        .filter(|value| *value != 0)
        .unwrap_or(1)
}

async fn reserve_next_uid(query: PreparedQuery) -> DbResult<i64> {
    let result = run_query(&query).await?;
    Ok(counter_value(result.rows.first(), "next_uid"))
}

fn logged<T>(message: &str, context: Value, result: DbResult<T>) -> DbResult<T> {
    if let Err(error) = &result {
        logger::error_with(message, context, error);
    }
    result
}

/// Reserves the next UID of the user's domain-wide sequence.
///
/// The counter is the authoritative UID source, so a reservation failure
/// aborts the save: an `Err` here must never be replaced by a fabricated
/// UID, which would collide with the mailbox's first message.
pub async fn get_domain_uid_next(user_id: &str, sent: bool) -> DbResult<i64> {
    logged("Error getting next UID", json!({}), reserve_next_uid(build_domain_uid_query(user_id, sent)).await)
}

/// Reserves the next UID of one account's sequence.
pub async fn get_account_uid_next(user_id: &str, account: &str, sent: bool) -> DbResult<i64> {
    logged(
        "Error getting account UID next",
        json!({}),
        reserve_next_uid(build_account_uid_query(user_id, account, sent)).await,
    )
}

/// Reserves the next UID for a mapped-utility mailbox (`Starred`, `Trash`).
/// Callers pass the literal box name, the same string persisted in
/// `mail_mailbox_uid.mailbox`. Not for per-account boxes: those go through
/// `get_account_uid_next`, because their sent and received halves are
/// different mailboxes that share one number space.
pub async fn get_mailbox_uid_next(user_id: &str, mailbox: &str) -> DbResult<i64> {
    logged(
        "Error getting mailbox UID next",
        json!({ "mailbox": mailbox }),
        reserve_next_uid(build_mailbox_uid_query(user_id, mailbox)).await,
    )
}

/// Which counter row assigns a mailbox's UIDs. One variant per reservation
/// function (`Domain` pairs with `get_domain_uid_next`, `Account` with
/// `get_account_uid_next`, `Mailbox` with `get_mailbox_uid_next`), so a peek
/// can never read a row its own write path does not use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UidScope {
    Domain { sent: bool },
    Account { account: String, sent: bool },
    Mailbox { mailbox: String },
}

/// The statement `get_uid_next` runs for `scope`.
pub fn build_uid_next_query(user_id: &str, scope: &UidScope) -> PreparedQuery {
    match scope {
        UidScope::Domain { sent } => build_domain_uid_next_query(user_id, *sent),
        UidScope::Mailbox { mailbox } => build_mailbox_uid_next_query(user_id, mailbox),
        UidScope::Account { account, sent } => build_account_uid_next_query(user_id, account, *sent),
    }
}

/// UIDNEXT for a mailbox (RFC 3501 §2.3.1.1) without allocating a UID: what
/// the reservation function matching `scope` would hand out next.
///
/// Fails on a database fault instead of returning a floor. A fabricated-low
/// UIDNEXT re-promises a UID the counter has already handed out, and every
/// caller (SELECT / EXAMINE / STATUS) turns an `Err` into a retry-friendly
/// tagged `NO`.
pub async fn get_uid_next(user_id: &str, scope: &UidScope) -> DbResult<i64> {
    let result = run_query(&build_uid_next_query(user_id, scope))
        .await
        .map(|result| counter_value(result.rows.first(), "uid_next"));
    logged("Error reading UIDNEXT", json!({}), result)
}

/// The statement `delete_mailbox_uid` runs.
pub fn build_delete_mailbox_uid_query(user_id: &str, mailbox: &str, mail_id: &str) -> PreparedQuery {
    PreparedQuery {
        sql: "DELETE FROM mail_mailbox_uid\n       WHERE user_id = $1 AND mailbox = $2 AND mail_id = $3".to_string(),
        values: params![user_id, mailbox, mail_id],
    }
}

/// Drops a `mail_mailbox_uid` mapping row. Used by the mapped-utility flag
/// hook (see `sync_mailbox_pivot`): a STORE that clears `\Flagged` on a
/// starred mail has to remove its `Starred` pivot. Idempotent: a delete
/// against a pivot that does not exist is a legal no-op.
///
/// Not a substitute for an expunge on a mapped destination, which keeps the
/// pivot so the mailbox's UID sequence stays intact. This drops the pivot
/// outright, so the UID it held is retired forever: a re-star draws a fresh
/// higher UID.
pub async fn delete_mailbox_uid(user_id: &str, mailbox: &str, mail_id: &str) -> DbResult<()> {
    let result = run_query(&build_delete_mailbox_uid_query(user_id, mailbox, mail_id)).await.map(|_| ());
    logged(
        "Failed to delete mail_mailbox_uid mapping",
        json!({ "user_id": user_id, "mailbox": mailbox, "mail_id": mail_id }),
        result,
    )
}

/// Mirrors a mail's mapped-utility membership flag into `mail_mailbox_uid`.
/// The pivot table for a mapped-utility folder (`Starred`, `Trash`) has to
/// agree with the corresponding `mails` flag (`saved`, `deleted`): the view
/// shows exactly the mails with a pivot row. Called from every path that
/// flips the flag.
///
/// `is_present` names the target state, not the delta. Both branches are
/// idempotent, so a repeat STORE that doesn't change the flag stays cheap.
pub async fn sync_mailbox_pivot(user_id: &str, mailbox: &str, mail_id: &str, is_present: bool) -> DbResult<()> {
    if is_present {
        // Reserve a fresh mailbox UID and try to insert. If the pivot already
        // exists the write keeps the old uid and the reservation is wasted:
        // cheap, and it preserves UID monotonicity for the case that matters,
        // where an unstar-then-restar draws a NEW higher uid.
        let uid = get_mailbox_uid_next(user_id, mailbox).await?;
        write_mailbox_uid(user_id, mailbox, mail_id, uid).await?;
        Ok(())
    } else {
        delete_mailbox_uid(user_id, mailbox, mail_id).await
    }
}

/// Records a mail's per-mailbox UID. `DO UPDATE` rather than `DO NOTHING` so
/// a row that already exists still comes back through `RETURNING`: under
/// `DO NOTHING` a conflict returns no row and the caller cannot tell the
/// persisted UID from the one it proposed.
pub fn build_write_mailbox_uid_query(user_id: &str, mailbox: &str, mail_id: &str, uid: i64) -> PreparedQuery {
    PreparedQuery {
        sql: "INSERT INTO mail_mailbox_uid (user_id, mailbox, mail_id, uid)\n       VALUES ($1, $2, $3, $4)\n       ON CONFLICT (user_id, mailbox, mail_id)\n         DO UPDATE SET uid = mail_mailbox_uid.uid\n       RETURNING uid"
            .to_string(),
        values: params![user_id, mailbox, mail_id, uid],
    }
}

/// Records a UID assignment in the per-(user, mailbox, mail) mapping.
///
/// Returns the UID the mapping actually holds: the just-inserted `uid`, or
/// the earlier one when a row for `(user, mailbox, mail_id)` already exists
/// (COPY twice, or a retry after a partial failure). Callers reporting a
/// destination UID over the wire must use the return value, not the `uid`
/// they reserved.
///
/// Fails rather than answering nothing: this mapping is the sole per-mailbox
/// UID source, so a swallowed fault would leave the mail invisible in its
/// destination.
pub async fn write_mailbox_uid(user_id: &str, mailbox: &str, mail_id: &str, uid: i64) -> DbResult<i64> {
    let result = run_query(&build_write_mailbox_uid_query(user_id, mailbox, mail_id, uid)).await.map(|result| {
        result
            .rows
            .first()
            .and_then(|row| row.get("uid"))
            .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|number| number as i64)))
            .unwrap_or(uid)
    });
    logged(
        "Failed to record mail_mailbox_uid mapping — aborting to force retry",
        json!({ "user_id": user_id, "mailbox": mailbox, "mail_id": mail_id, "uid": uid }),
        result,
    )
}

/// Mod-sequence reservation query (kind `modseq`; scope and `sent` unused).
pub fn build_modseq_query(user_id: &str) -> PreparedQuery {
    let seed = Seed {
        sql: "\n      SELECT COALESCE(MAX(modseq), 0) + 1 FROM mails\n      WHERE user_id = $1\n    ".to_string(),
        params: Vec::new(),
    };
    build_reserve_uid_query(user_id, "modseq", "", false, seed)
}

/// Reserves the next mod-sequence for a user's next mailbox mutation. Every
/// insert, flag change and expunge that alters a mailbox stamps the value
/// returned here, so HIGHESTMODSEQ advances monotonically.
pub async fn get_next_modseq(user_id: &str) -> DbResult<i64> {
    logged("Error reserving next modseq", json!({}), reserve_next_uid(build_modseq_query(user_id)).await)
}

/// The statement `get_highest_modseq` runs.
///
/// A scoped login's HIGHESTMODSEQ covers only its domains' mail. Unscoped it
/// stays the box-wide maximum, membership aside, as before.
pub fn build_highest_modseq_query(
    user_id: &str,
    mailbox: Option<&str>,
    sent: bool,
    domains: Option<&[String]>,
) -> PreparedQuery {
    let scope = |prefix: &str| domains.map(|domains| membership_condition(None, true, prefix, Some(domains))).unwrap_or_default();
    match mailbox.filter(|name| !uses_domain_uid_space(Some(name))) {
        None => PreparedQuery {
            sql: format!(
                "\n        SELECT COALESCE(MAX(modseq), 1) AS highest FROM mails\n        WHERE user_id = $1 AND sent = $2{}\n      ",
                scope("")
            ),
            values: params![user_id, sent],
        },
        Some(mailbox) => PreparedQuery {
            sql: format!(
                "\n        SELECT COALESCE(MAX(m.modseq), 1) AS highest FROM mails m\n        JOIN mail_mailbox_uid x\n          ON x.user_id = m.user_id\n          AND x.mailbox = $3\n          AND x.mail_id = m.mail_id\n        WHERE m.user_id = $1 AND m.sent = $2{}\n      ",
                scope("m.")
            ),
            values: params![user_id, sent, mailbox],
        },
    }
}

/// HIGHESTMODSEQ for a mailbox: the largest mod-sequence of any message
/// routed to it. A database fault reads as 1, the floor every row starts at,
/// never as 0, which would signal "no persistent mod-sequences".
pub async fn get_highest_modseq(user_id: &str, mailbox: Option<&str>, sent: bool, domains: Option<&[String]>) -> i64 {
    let result: Result<i64, DbError> =
        run_query(&build_highest_modseq_query(user_id, mailbox, sent, domains)).await.map(|result| {
            result
                .rows
                .first()
                .and_then(|row| row.get("highest"))
                .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|number| number as i64)))
                .unwrap_or(1)
        });
    result.unwrap_or_else(|error| {
        logger::error_with("Failed to get highest modseq", json!({}), &error);
        1
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::repositories::mails::model::{mail_uid_counters_table, mails_table};
    use crate::postgres::schema::schema;

    const USER_ID: &str = "11111111-1111-1111-1111-111111111111";

    fn contains_true(values: &[Param]) -> bool {
        values.contains(&Param::from(true))
    }

    // counters.test.ts — "buildMailboxUidQuery — 596-collision regression pin (#725)"

    #[test]
    fn mailbox_seed_does_not_carry_the_sent_column() {
        let query = build_mailbox_uid_query("user-1", "Starred");
        let lowered = query.sql.to_lowercase();
        let seed_fragment = lowered.split("on conflict").next().unwrap();
        assert!(seed_fragment.contains("mail_mailbox_uid"));
        let sent_predicate = regex::Regex::new(r"(?-u:\b)sent(?-u:\b)\s*=\s*\$").unwrap();
        assert!(!sent_predicate.is_match(seed_fragment));
    }

    #[test]
    fn mailbox_counter_row_is_keyed_with_a_fixed_false_sent() {
        let first = build_mailbox_uid_query("user-1", "Starred");
        let second = build_mailbox_uid_query("user-1", "Starred");
        assert_eq!(first.values, second.values);
        assert!(first.values.contains(&Param::from(false)));
        assert!(!contains_true(&first.values));
    }

    #[test]
    fn mailbox_counter_row_is_scoped_by_the_mailbox_name() {
        let starred = build_mailbox_uid_query("user-1", "Starred");
        let trash = build_mailbox_uid_query("user-1", "Trash");
        assert!(starred.values.contains(&Param::from("Starred")));
        assert!(trash.values.contains(&Param::from("Trash")));
        assert!(!starred.values.contains(&Param::from("Trash")));
        assert!(!trash.values.contains(&Param::from("Starred")));
    }

    // counters.test.ts — "buildWriteMailboxUidQuery"

    #[test]
    fn write_returns_the_persisted_uid_even_when_the_mapping_row_exists() {
        let query = build_write_mailbox_uid_query(
            "11111111-1111-1111-1111-111111111111",
            "Archive",
            "22222222-2222-2222-2222-222222222222",
            7,
        );
        assert!(query.sql.contains("INSERT INTO mail_mailbox_uid"));
        assert!(regex::Regex::new(r"ON CONFLICT\s*\([^)]+\)\s+DO UPDATE").unwrap().is_match(&query.sql));
        assert!(!regex::Regex::new(r"ON CONFLICT\s*\([^)]+\)\s+DO NOTHING").unwrap().is_match(&query.sql));
        assert!(query.sql.contains("RETURNING uid"));
        assert_eq!(
            query.values,
            params![
                "11111111-1111-1111-1111-111111111111",
                "Archive",
                "22222222-2222-2222-2222-222222222222",
                7
            ]
        );
    }

    #[test]
    fn write_keys_the_conflict_on_the_mappings_identity() {
        let query = build_write_mailbox_uid_query("u", "Archive", "m", 7);
        assert!(query.sql.contains("ON CONFLICT (user_id, mailbox, mail_id)"));
    }

    // mail-modseq.test.ts — "buildModseqQuery"

    #[test]
    fn modseq_reserves_atomically_on_mail_uid_counters() {
        let query = build_modseq_query(USER_ID);
        assert!(query.sql.contains("INSERT INTO mail_uid_counters"));
        assert!(query.sql.contains("ON CONFLICT (user_id, uid_kind, uid_scope, sent)"));
        assert!(query.sql.contains("DO UPDATE SET last_uid = mail_uid_counters.last_uid + 1"));
        assert!(query.sql.contains("RETURNING last_uid AS next_uid"));
    }

    #[test]
    fn modseq_seeds_once_from_the_live_max() {
        assert!(build_modseq_query(USER_ID).sql.contains("COALESCE(MAX(modseq), 0) + 1"));
    }

    #[test]
    fn modseq_does_not_issue_the_racy_bare_max_read() {
        assert!(!build_modseq_query(USER_ID).sql.contains("AS next_uid FROM mails"));
    }

    #[test]
    fn modseq_keys_the_counter_kind_modseq() {
        assert_eq!(build_modseq_query(USER_ID).values, params![USER_ID, "modseq", "", false]);
    }

    // mail-modseq.test.ts — "mails.modseq column (CONDSTORE groundwork)"

    #[test]
    fn modseq_column_is_bigint_not_null_default_1() {
        let table = schema().tables.iter().find(|table| table.name == mails_table().name).unwrap();
        let definition = table.columns.iter().find(|(column, _)| column == "modseq").map(|(_, definition)| definition.as_str());
        assert_eq!(definition, Some("BIGINT NOT NULL DEFAULT 1"));
    }

    #[test]
    fn modseq_column_is_indexed() {
        assert!(schema().indexes.iter().any(|index| index.sql.ends_with("ON mails (modseq)")));
    }

    // mail-uid-reservation.test.ts — "mail_uid_counters table"

    #[test]
    fn counters_table_declares_the_composite_primary_key() {
        let table = schema().tables.iter().find(|table| table.name == mail_uid_counters_table().name).unwrap();
        assert!(table.create_sql.contains("PRIMARY KEY (user_id, uid_kind, uid_scope, sent)"));
    }

    // mail-uid-reservation.test.ts — "buildDomainUidQuery"

    #[test]
    fn domain_reserves_atomically_on_mail_uid_counters() {
        let query = build_domain_uid_query(USER_ID, false);
        assert!(query.sql.contains("INSERT INTO mail_uid_counters"));
        assert!(query.sql.contains("ON CONFLICT (user_id, uid_kind, uid_scope, sent)"));
        assert!(query.sql.contains("DO UPDATE SET last_uid = mail_uid_counters.last_uid + 1"));
        assert!(query.sql.contains("RETURNING last_uid AS next_uid"));
        assert!(query.sql.contains("COALESCE(MAX(uid_domain), 0) + 1"));
        assert_eq!(query.values, params![USER_ID, "domain", "", false]);
    }

    #[test]
    fn domain_does_not_issue_the_racy_bare_max_read() {
        assert!(!build_domain_uid_query(USER_ID, false).sql.contains("AS next_uid FROM mails"));
    }

    #[test]
    fn domain_passes_sent_through_to_the_counter_sequence() {
        assert_eq!(build_domain_uid_query(USER_ID, true).values, params![USER_ID, "domain", "", true]);
    }

    // mail-uid-reservation.test.ts — "buildAccountUidQuery"

    #[test]
    fn account_reserves_atomically_keyed_by_kind_and_address() {
        let query = build_account_uid_query(USER_ID, "user@hoie.kim", false);
        assert!(query.sql.contains("INSERT INTO mail_uid_counters"));
        assert!(query.sql.contains("DO UPDATE SET last_uid = mail_uid_counters.last_uid + 1"));
        assert!(query.sql.contains("COALESCE(MAX(uid), 0) + 1 FROM mail_mailbox_uid"));
        assert_eq!(query.values[1], Param::from("account"));
        assert_eq!(query.values[2], Param::from("user@hoie.kim"));
        assert_eq!(query.values[3], Param::from(false));
    }

    #[test]
    fn account_seeds_the_received_sequence_from_the_inbox_path_and_the_local_part() {
        let query = build_account_uid_query(USER_ID, "user@hoie.kim", false);
        assert!(query.sql.contains("mailbox IN ($5, $6)"));
        assert_eq!(query.values[4], Param::from("INBOX/accounts/user"));
        assert_eq!(query.values[5], Param::from("user"));
    }

    #[test]
    fn account_seeds_the_sent_sequence_from_the_sent_path_and_the_local_part() {
        let query = build_account_uid_query(USER_ID, "user@hoie.kim", true);
        assert!(query.sql.contains("mailbox IN ($5, $6)"));
        assert_eq!(query.values[4], Param::from("Sent Messages/accounts/user"));
        assert_eq!(query.values[5], Param::from("user"));
        assert_eq!(query.values[3], Param::from(true));
        assert!(!query.sql.contains("to_address @>"));
        assert!(!query.sql.contains("from_address @>"));
    }

    // mail-uid-reservation.test.ts — "UIDNEXT peek"

    #[test]
    fn peek_reads_the_counter_row_without_allocating() {
        for query in [
            build_domain_uid_next_query(USER_ID, false),
            build_account_uid_next_query(USER_ID, "user@hoie.kim", false),
        ] {
            assert!(query.sql.contains("FROM mail_uid_counters"));
            assert!(!query.sql.contains("INSERT"));
            assert!(!query.sql.contains("DO UPDATE"));
            assert!(query.sql.contains("SELECT last_uid + 1"));
        }
    }

    #[test]
    fn peek_targets_the_same_counter_row_the_reservation_writes() {
        let peek = build_domain_uid_next_query(USER_ID, true);
        let reserve = build_domain_uid_query(USER_ID, true);

        // The peek's WHERE predicate as a whole, not column by column: the
        // seed subquery carries its own `sent = $4`.
        let collapsed = regex::Regex::new(r"\s+").unwrap().replace_all(&peek.sql, " ").into_owned();
        let predicate = regex::Regex::new(r"WHERE ([^)]*?) \)").unwrap();
        let captured = predicate.captures(&collapsed).map(|found| found[1].to_string());
        assert_eq!(
            captured.as_deref(),
            Some("user_id = $1 AND uid_kind = $2 AND uid_scope = $3 AND sent = $4")
        );

        assert_eq!(peek.values[..4], reserve.values[..4]);

        let account_peek = build_account_uid_next_query(USER_ID, "user@hoie.kim", false);
        let account_reserve = build_account_uid_query(USER_ID, "user@hoie.kim", false);
        assert_eq!(account_peek.values, account_reserve.values);
    }

    #[test]
    fn peek_falls_back_to_the_reservations_own_seed() {
        let domain = build_domain_uid_next_query(USER_ID, false);
        assert!(domain.sql.contains("COALESCE("));
        assert!(domain.sql.contains("COALESCE(MAX(uid_domain), 0) + 1"));

        let account = build_account_uid_next_query(USER_ID, "user@hoie.kim", false);
        assert!(account.sql.contains("COALESCE(MAX(uid), 0) + 1 FROM mail_mailbox_uid"));
        assert_eq!(account.values[4], Param::from("INBOX/accounts/user"));
        assert_eq!(account.values[5], Param::from("user"));
    }

    #[test]
    fn peek_keeps_the_seed_fallback_unfiltered() {
        for query in [
            build_domain_uid_next_query(USER_ID, false),
            build_account_uid_next_query(USER_ID, "user@hoie.kim", false),
        ] {
            assert!(!query.sql.contains("expunged"));
        }
    }

    // mail-uid-reservation.test.ts — "buildMailboxUidNextQuery"

    #[test]
    fn mailbox_peek_targets_the_same_counter_row_the_reservation_writes() {
        let peek = build_mailbox_uid_next_query(USER_ID, "Starred");
        let reserve = build_mailbox_uid_query(USER_ID, "Starred");
        assert_eq!(peek.values, reserve.values);
        assert_eq!(peek.values[..4], params![USER_ID, "mailbox", "Starred", false]);
    }

    #[test]
    fn mailbox_peek_reads_the_counter_never_a_max_over_surviving_pivot_rows() {
        let query = build_mailbox_uid_next_query(USER_ID, "Trash");
        assert!(query.sql.contains("FROM mail_uid_counters"));
        assert!(query.sql.contains("SELECT last_uid + 1"));
        assert!(!query.sql.contains("INSERT"));
        assert!(!query.sql.contains("DO UPDATE"));
        assert!(!query.sql.contains("expunged"));
    }

    #[test]
    fn mailbox_peek_carries_no_sent_axis() {
        let starred = build_mailbox_uid_next_query(USER_ID, "Starred");
        assert_eq!(starred.values[3], Param::from(false));
        let trash = build_mailbox_uid_next_query(USER_ID, "Trash");
        assert_ne!(starred.values[2], trash.values[2]);
    }
}
