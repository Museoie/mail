//! Every SQL statement the HTTP mail reads issue, built with no pool
//! involved so the emitted string and its bound parameters can be asserted
//! directly (repositories/mails/http-query.ts).
//!
//! ```ignore
//! let PreparedQuery { sql, values } = build_mail_headers_query(user_id, address, &options, None);
//! let result = pool().query(&sql, &values).await?;
//! ```

use serde_json::json;

use super::domain_scope::{mail_in_domains_condition, scope_to_domains};
use super::table::PreparedQuery;
use crate::postgres::Param;

/// Which per-account list a query reads, and how.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GetMailHeadersOptions {
    pub sent: bool,
    pub saved: bool,
    /// Spam-flagged received mail only.
    pub spam: bool,
    /// Only the mail of these of the owner's domains (`mail_in_domains_condition`).
    pub domains: Option<Vec<String>>,
    pub from: Option<i64>,
    pub size: Option<i64>,
    /// Rows changed after this time (`updated > since`).
    pub since: Option<String>,
    pub after: Option<ListCursor>,
}

/// The sort keys of the row a page ended on, in newest-first order. The next
/// page starts strictly after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListCursor {
    /// `date::text`, with the column's microseconds.
    pub date: String,
    pub id: String,
}

/// Paging and cursor options for the unified list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnifiedMailHeadersOptions {
    pub from: Option<i64>,
    pub size: Option<i64>,
    pub since: Option<String>,
    /// Newest first instead of unread first, as a paged list needs.
    pub newest_first: bool,
    /// Only with `newest_first`: start after this position.
    pub after: Option<ListCursor>,
}

/// Selected by a paged list query: the id, plus the exact sort keys the next
/// cursor is built from.
pub const LIST_CURSOR_COLUMNS: &str = "mail_id, date::text AS cursor_date";

/// Seconds the delta cursor lags real time.
const DELTA_CURSOR_SAFETY_MARGIN_SECONDS: i64 = 2;

const RECEIVED_ADDRESS_EXPANSION: &str = "jsonb_array_elements(
  COALESCE(to_address, '[]'::jsonb) ||
  COALESCE(cc_address, '[]'::jsonb) ||
  COALESCE(bcc_address, '[]'::jsonb) ||
  COALESCE(envelope_to, '[]'::jsonb)
)->>'address' as address";
// The lower-cased domain part of the `address` an expansion produced.
const ADDRESS_DOMAIN: &str = "lower(regexp_replace(address, '^.*@', ''))";

const RECEIVED_ADDRESS_NOT_NULL: &str =
    "(to_address IS NOT NULL OR cc_address IS NOT NULL OR bcc_address IS NOT NULL OR envelope_to IS NOT NULL)";

/// The columns of a list row, shared by the per-account list, the unified
/// list and search. The body is read as a 400-character prefix, for the
/// snippet; References keeps its first and last two entries.
pub const HEADER_COLUMNS: &str = "mail_id, user_id, subject, date, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, envelope_to, read, saved, sent, is_spam, insight, left(text, 400) AS text_prefix, message_id, in_reply_to, CASE WHEN jsonb_array_length(reference_ids) > 3
    THEN jsonb_build_array(reference_ids->0, reference_ids->-2, reference_ids->-1)
    ELSE reference_ids END AS reference_ids";

fn address_json(address: &str) -> Param {
    Param::json(&json!([{ "address": address }]))
}

pub fn build_header_address_condition(sent: bool, saved: bool) -> String {
    let sent_condition = "from_address @> $2::jsonb";
    let received_condition = "(to_address @> $2::jsonb OR cc_address @> $2::jsonb OR bcc_address @> $2::jsonb OR envelope_to @> $2::jsonb)";
    if saved && !sent {
        format!("({sent_condition} OR {received_condition})")
    } else if sent {
        sent_condition.to_string()
    } else {
        received_condition.to_string()
    }
}

/// Header rows for `ids`, in no particular order.
pub fn build_mail_headers_by_ids_query(user_id: &str, ids: &[String]) -> PreparedQuery {
    PreparedQuery {
        sql: format!(
            "
    SELECT {HEADER_COLUMNS} FROM mails
    WHERE user_id = $1
      AND mail_id = ANY($2::uuid[])
      AND expunged = FALSE
  "
        ),
        values: vec![Param::from(user_id), Param::array(ids)],
    }
}

pub fn build_mail_headers_query(
    user_id: &str,
    address: &str,
    options: &GetMailHeadersOptions,
    columns: Option<&str>,
) -> PreparedQuery {
    let columns = columns.unwrap_or(HEADER_COLUMNS);
    let address_condition = build_header_address_condition(options.sent, options.saved);
    let mut sql = format!(
        "
    SELECT {columns} FROM mails
    WHERE user_id = $1
      AND {address_condition}
      AND expunged = FALSE
      AND draft = FALSE
  "
    );
    let mut values = vec![Param::from(user_id), address_json(address)];
    let mut index = 3;

    if options.saved {
        sql.push_str(" AND saved = TRUE");
    }
    if options.spam {
        sql.push_str(" AND is_spam = TRUE AND sent = FALSE");
    } else {
        sql.push_str(" AND is_spam = FALSE");
    }
    if let Some(domains) = &options.domains {
        sql.push_str(&format!(" AND {}", mail_in_domains_condition(index, "")));
        index += 1;
        values.push(Param::array(domains));
    }
    if let Some(since) = &options.since {
        sql.push_str(&format!(" AND updated > ${index}"));
        index += 1;
        values.push(Param::from(since.as_str()));
    }
    if let Some(after) = &options.after {
        sql.push_str(&format!(" AND (date, mail_id) < (${index}::timestamptz, ${}::uuid)", index + 1));
        values.push(Param::from(after.date.as_str()));
        values.push(Param::from(after.id.as_str()));
        index += 2;
    }
    sql.push_str(" ORDER BY date DESC, mail_id DESC");
    if let Some(size) = options.size {
        sql.push_str(&format!(" LIMIT ${index}"));
        index += 1;
        values.push(Param::from(size));
    }
    if let Some(from) = options.from {
        sql.push_str(&format!(" OFFSET ${index}"));
        values.push(Param::from(from));
    }
    PreparedQuery { sql, values }
}

/// One page of a folder's ids: `limit` rows after `options.after`, plus one
/// more so the caller can tell whether another page follows.
pub fn build_mail_header_id_page_query(
    user_id: &str,
    address: &str,
    options: &GetMailHeadersOptions,
    limit: i64,
) -> PreparedQuery {
    let options = GetMailHeadersOptions { from: None, size: Some(limit + 1), ..options.clone() };
    build_mail_headers_query(user_id, address, &options, Some(LIST_CURSOR_COLUMNS))
}

pub fn build_delta_as_of_query() -> PreparedQuery {
    PreparedQuery {
        sql: "SELECT now() - make_interval(secs => $1) AS as_of".to_string(),
        values: vec![Param::from(DELTA_CURSOR_SAFETY_MARGIN_SECONDS)],
    }
}

pub fn build_delta_eviction_query(
    user_id: &str,
    address: &str,
    options: &GetMailHeadersOptions,
    since: &str,
) -> PreparedQuery {
    let address_condition = build_header_address_condition(options.sent, options.saved);
    let eviction_condition =
        if options.spam { "(expunged = TRUE OR is_spam = FALSE)" } else { "(expunged = TRUE OR is_spam = TRUE)" };
    let (sql, values) = scope_to_domains(
        format!(
            "
      SELECT mail_id FROM mails
      WHERE user_id = $1
        AND {address_condition}
        AND {eviction_condition}
        AND updated > $3
    "
        ),
        vec![Param::from(user_id), address_json(address), Param::from(since)],
        options.domains.as_deref(),
        "",
    );
    PreparedQuery { sql, values }
}

/// A mail belongs to the unified view when any recipient column names one of
/// the user's receiving addresses, bound as one text[].
pub fn build_unified_address_condition(param_index: usize) -> String {
    format!(
        "EXISTS (SELECT 1 FROM jsonb_array_elements(COALESCE(to_address, '[]'::jsonb) || COALESCE(cc_address, '[]'::jsonb) || COALESCE(bcc_address, '[]'::jsonb) || COALESCE(envelope_to, '[]'::jsonb)) AS recipient WHERE recipient->>'address' = ANY(${param_index}::text[]))"
    )
}

pub fn build_unified_mail_headers_query(
    user_id: &str,
    addresses: &[String],
    options: &UnifiedMailHeadersOptions,
    columns: Option<&str>,
) -> PreparedQuery {
    let columns = columns.unwrap_or(HEADER_COLUMNS);
    let address_condition = build_unified_address_condition(2);
    let mut sql = format!(
        "
    SELECT {columns} FROM mails
    WHERE user_id = $1
      AND {address_condition}
      AND expunged = FALSE
      AND draft = FALSE
      AND is_spam = FALSE
  "
    );
    let mut values = vec![Param::from(user_id), Param::array(addresses)];
    let mut index = 3;

    if let Some(since) = &options.since {
        sql.push_str(&format!(" AND updated > ${index}"));
        index += 1;
        values.push(Param::from(since.as_str()));
    }
    if options.newest_first {
        if let Some(after) = &options.after {
            sql.push_str(&format!(" AND (date, mail_id) < (${index}::timestamptz, ${}::uuid)", index + 1));
            values.push(Param::from(after.date.as_str()));
            values.push(Param::from(after.id.as_str()));
            index += 2;
        }
        sql.push_str(" ORDER BY date DESC, mail_id DESC");
    } else {
        sql.push_str(" ORDER BY read ASC, date DESC");
    }
    if let Some(size) = options.size {
        sql.push_str(&format!(" LIMIT ${index}"));
        index += 1;
        values.push(Param::from(size));
    }
    if let Some(from) = options.from {
        sql.push_str(&format!(" OFFSET ${index}"));
        values.push(Param::from(from));
    }
    PreparedQuery { sql, values }
}

/// The unified list's [`build_mail_header_id_page_query`], newest first.
pub fn build_unified_mail_header_id_page_query(
    user_id: &str,
    addresses: &[String],
    options: &UnifiedMailHeadersOptions,
    limit: i64,
) -> PreparedQuery {
    let options = UnifiedMailHeadersOptions { newest_first: true, from: None, size: Some(limit + 1), ..options.clone() };
    build_unified_mail_headers_query(user_id, addresses, &options, Some(LIST_CURSOR_COLUMNS))
}

pub fn build_unified_delta_eviction_query(user_id: &str, addresses: &[String], since: &str) -> PreparedQuery {
    let address_condition = build_unified_address_condition(2);
    PreparedQuery {
        sql: format!(
            "
      SELECT mail_id FROM mails
      WHERE user_id = $1
        AND {address_condition}
        AND (expunged = TRUE OR is_spam = TRUE)
        AND updated > $3
    "
        ),
        values: vec![Param::from(user_id), Param::array(addresses), Param::from(since)],
    }
}

pub fn build_search_mails_query(user_id: &str, search_term: &str) -> PreparedQuery {
    PreparedQuery {
        sql: format!(
            "
      SELECT
        {HEADER_COLUMNS},
        ts_rank(search_vector, plainto_tsquery('english', $2)) as rank,
        ts_headline('english', subject, plainto_tsquery('english', $2),
          'StartSel=<em>, StopSel=</em>, MaxWords=50, MinWords=10') as subject_highlight,
        ts_headline('english', text, plainto_tsquery('english', $2),
          'StartSel=<em>, StopSel=</em>, MaxWords=50, MinWords=10') as text_highlight
      FROM mails
      WHERE user_id = $1
        AND search_vector @@ plainto_tsquery('english', $2)
        AND expunged = FALSE
        AND draft = FALSE
      ORDER BY rank DESC, date DESC
      LIMIT 1000
    "
        ),
        values: vec![Param::from(user_id), Param::from(search_term)],
    }
}

/// The accounts (addresses) a store holds mail for, with their counts.
///
/// `domain_filter` keeps the addresses on those domains, so the external
/// correspondents a mail names are not listed as accounts. `scope_domains`
/// keeps only the mail of those domains, for a viewer reading one domain of
/// the owner's.
pub fn build_account_stats_query(
    user_id: &str,
    sent: bool,
    domain_filter: Option<&[String]>,
    spam_only: bool,
    scope_domains: Option<&[String]>,
) -> PreparedQuery {
    let use_sent_expansion = sent && !spam_only;
    let address_expansion = if use_sent_expansion {
        "jsonb_array_elements(from_address)->>'address' as address"
    } else {
        RECEIVED_ADDRESS_EXPANSION
    };
    let address_not_null = if use_sent_expansion { "from_address IS NOT NULL" } else { RECEIVED_ADDRESS_NOT_NULL };
    let spam_condition = if spam_only { "AND is_spam = TRUE AND sent = FALSE" } else { "AND is_spam = FALSE" };
    let mut values = vec![Param::from(user_id)];
    let domain_condition = match domain_filter {
        Some(domains) => {
            values.push(Param::array(domains));
            format!("AND {ADDRESS_DOMAIN} = ANY(${}::text[])", values.len())
        }
        None => String::new(),
    };
    let scope_condition = match scope_domains {
        Some(domains) => {
            values.push(Param::array(domains));
            format!("AND {}", mail_in_domains_condition(values.len(), ""))
        }
        None => String::new(),
    };
    let sql = format!(
        "
      WITH expanded_mails AS (
        SELECT DISTINCT
          mail_id, read, saved, date,
          {address_expansion}
        FROM mails
        WHERE user_id = $1
          AND expunged = FALSE
          AND draft = FALSE
          {spam_condition}
          AND {address_not_null}
          {scope_condition}
      )
      SELECT
        address,
        COUNT(*) as count,
        SUM(CASE WHEN read = FALSE THEN 1 ELSE 0 END) as unread,
        SUM(CASE WHEN saved = TRUE THEN 1 ELSE 0 END) as saved_count,
        MAX(date) as latest
      FROM expanded_mails
      WHERE address IS NOT NULL
      {domain_condition}
      GROUP BY address
      ORDER BY latest DESC
    "
    );
    PreparedQuery { sql, values }
}

pub fn build_search_account_stats_query(user_id: &str, search_term: &str, domain_filter: Option<&[String]>) -> PreparedQuery {
    let domain_condition =
        if domain_filter.is_some() { format!("AND {ADDRESS_DOMAIN} = ANY($3::text[])") } else { String::new() };
    let sql = format!(
        "
      WITH expanded_mails AS (
        SELECT DISTINCT
          mail_id, read, saved, date,
          {RECEIVED_ADDRESS_EXPANSION}
        FROM mails
        WHERE user_id = $1
          AND expunged = FALSE
          AND draft = FALSE
          AND {RECEIVED_ADDRESS_NOT_NULL}
          AND search_vector @@ plainto_tsquery('english', $2)
      )
      SELECT
        address,
        COUNT(*) as count,
        SUM(CASE WHEN read = FALSE THEN 1 ELSE 0 END) as unread,
        SUM(CASE WHEN saved = TRUE THEN 1 ELSE 0 END) as saved_count,
        MAX(date) as latest
      FROM expanded_mails
      WHERE address IS NOT NULL
      {domain_condition}
      GROUP BY address
      ORDER BY latest DESC
    "
    );
    let mut values = vec![Param::from(user_id), Param::from(search_term)];
    if let Some(domains) = domain_filter {
        values.push(Param::array(domains));
    }
    PreparedQuery { sql, values }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(param: &Param) -> &str {
        match param {
            Param::Text(text) => text,
            other => panic!("a text parameter, not {other:?}"),
        }
    }

    #[test]
    fn address_conditions_follow_the_view() {
        assert_eq!(build_header_address_condition(true, false), "from_address @> $2::jsonb");
        assert!(build_header_address_condition(false, false).starts_with("(to_address @> $2::jsonb OR cc_address"));
        let saved = build_header_address_condition(false, true);
        assert!(saved.starts_with("(from_address @> $2::jsonb OR (to_address"));
        // Sent wins over saved.
        assert_eq!(build_header_address_condition(true, true), "from_address @> $2::jsonb");
    }

    #[test]
    fn a_full_list_query_numbers_its_parameters_in_order() {
        let options = GetMailHeadersOptions { saved: true, from: Some(0), size: Some(10_000), ..Default::default() };
        let query = build_mail_headers_query("u", "a@x.test", &options, Some("mail_id"));
        assert!(query.sql.contains("SELECT mail_id FROM mails"));
        assert!(query.sql.contains(" AND saved = TRUE AND is_spam = FALSE ORDER BY date DESC, mail_id DESC LIMIT $3 OFFSET $4"));
        assert_eq!(query.values.len(), 4);
        assert_eq!(text(&query.values[1]), r#"[{"address":"a@x.test"}]"#);
        assert_eq!(text(&query.values[2]), "10000");
        assert_eq!(text(&query.values[3]), "0");
    }

    #[test]
    fn a_page_query_asks_for_one_row_past_the_page_after_the_cursor() {
        let after = ListCursor { date: "2026-11-15 02:55:04.123456+00".into(), id: "i".into() };
        let options = GetMailHeadersOptions { spam: true, after: Some(after), ..Default::default() };
        let query = build_mail_header_id_page_query("u", "a@x.test", &options, 50);
        assert!(query.sql.contains("SELECT mail_id, date::text AS cursor_date FROM mails"));
        assert!(query.sql.contains(
            " AND is_spam = TRUE AND sent = FALSE AND (date, mail_id) < ($3::timestamptz, $4::uuid) ORDER BY date DESC, mail_id DESC LIMIT $5"
        ));
        assert!(!query.sql.contains("OFFSET"));
        assert_eq!(text(&query.values[4]), "51");
    }

    #[test]
    fn a_delta_query_reads_rows_changed_since() {
        let options = GetMailHeadersOptions { since: Some("2026-01-01T00:00:00.000Z".into()), ..Default::default() };
        let query = build_mail_headers_query("u", "a@x.test", &options, None);
        assert!(query.sql.contains(" AND is_spam = FALSE AND updated > $3 ORDER BY"));
        assert!(!query.sql.contains("LIMIT"));
        let eviction = build_delta_eviction_query("u", "a@x.test", &options, "s");
        assert!(eviction.sql.contains("(expunged = TRUE OR is_spam = TRUE)"));
        let spam = GetMailHeadersOptions { spam: true, ..options };
        assert!(build_delta_eviction_query("u", "a", &spam, "s").sql.contains("(expunged = TRUE OR is_spam = FALSE)"));
    }

    #[test]
    fn the_unified_list_is_unread_first_unless_paged() {
        let addresses = vec!["a@x.test".to_string(), "b\"@x.test".to_string()];
        let full = build_unified_mail_headers_query(
            "u",
            &addresses,
            &UnifiedMailHeadersOptions { from: Some(0), size: Some(10_000), ..Default::default() },
            None,
        );
        assert!(full.sql.contains("ORDER BY read ASC, date DESC LIMIT $3 OFFSET $4"));
        assert_eq!(text(&full.values[1]), r#"{"a@x.test","b\"@x.test"}"#);
        let page = build_unified_mail_header_id_page_query("u", &addresses, &UnifiedMailHeadersOptions::default(), 10);
        assert!(page.sql.contains("ORDER BY date DESC, mail_id DESC LIMIT $3"));
        assert_eq!(text(&page.values[2]), "11");
    }

    #[test]
    fn account_stats_bind_the_domains_only_when_given() {
        let own = vec!["bob.mail.test".to_string(), "example.org".to_string()];
        let sent = build_account_stats_query("u", true, Some(&own), false, None);
        assert!(sent.sql.contains("jsonb_array_elements(from_address)->>'address' as address"));
        assert!(sent.sql.contains("AND lower(regexp_replace(address, '^.*@', '')) = ANY($2::text[])"));
        assert_eq!(text(&sent.values[1]), r#"{"bob.mail.test","example.org"}"#);
        assert_eq!(sent.values.len(), 2);
        let spam = build_account_stats_query("u", true, None, true, None);
        assert!(spam.sql.contains("COALESCE(envelope_to, '[]'::jsonb)"));
        assert!(spam.sql.contains("AND is_spam = TRUE AND sent = FALSE"));
        assert_eq!(spam.values.len(), 1);
        // A viewer's read: the owner's rows of one domain, accounts on it.
        let shared = vec!["example.org".to_string()];
        let viewed = build_account_stats_query("u", false, Some(&shared), false, Some(&shared));
        assert!(viewed.sql.contains("= ANY($2::text[])"));
        assert!(viewed.sql.contains("= ANY($3::text[])"));
        assert_eq!(viewed.values.len(), 3);
        let search = build_search_account_stats_query("u", "term", Some(&own));
        assert!(search.sql.contains("= ANY($3::text[])"));
        assert_eq!(search.values.len(), 3);
    }

    #[test]
    fn a_scoped_list_filters_to_its_domains_before_the_since() {
        let options = GetMailHeadersOptions {
            domains: Some(vec!["example.org".into()]),
            since: Some("s".into()),
            ..Default::default()
        };
        let query = build_mail_headers_query("u", "a@example.org", &options, None);
        assert!(query.sql.contains("= ANY($3::text[])"));
        assert!(query.sql.contains("AND updated > $4"));
        let eviction = build_delta_eviction_query("u", "a@example.org", &options, "s");
        assert!(eviction.sql.contains("= ANY($4::text[])"));
        assert_eq!(eviction.values.len(), 4);
    }
}
