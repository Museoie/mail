//! The reads behind the HTTP mail lists, search and the account sidebar
//! (repositories/mails/http.ts).

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::http_query::{
    build_account_stats_query, build_delta_as_of_query, build_delta_eviction_query, build_mail_header_id_page_query,
    build_mail_headers_by_ids_query, build_mail_headers_query, build_search_account_stats_query,
    build_search_mails_query, build_unified_delta_eviction_query, build_unified_mail_header_id_page_query,
    build_unified_mail_headers_query,
};
pub use super::http_query::{GetMailHeadersOptions, ListCursor, UnifiedMailHeadersOptions};
use super::table::{run_query, PreparedQuery, Record};
use crate::logger;
use crate::postgres::DbResult;

/// A list row: the columns of `HEADER_COLUMNS`, by name.
pub type MailHeaderResult = Record;

/// A full list is fetched as its ordered ids, then their headers a batch at
/// a time.
pub const HEADER_BATCH_SIZE: usize = 500;

async fn rows_of(query: &PreparedQuery) -> DbResult<Vec<Record>> {
    Ok(run_query(query).await?.rows)
}

fn ids_of(rows: Vec<Record>) -> Vec<String> {
    rows.into_iter()
        .map(|row| row.get("mail_id").and_then(Value::as_str).unwrap_or("").to_string())
        .collect()
}

pub async fn get_mail_headers(
    user_id: &str,
    address: &str,
    options: &GetMailHeadersOptions,
) -> DbResult<Vec<MailHeaderResult>> {
    rows_of(&build_mail_headers_query(user_id, address, options, None)).await
}

pub async fn get_mail_header_ids(user_id: &str, address: &str, options: &GetMailHeadersOptions) -> DbResult<Vec<String>> {
    Ok(ids_of(rows_of(&build_mail_headers_query(user_id, address, options, Some("mail_id"))).await?))
}

pub async fn get_unified_mail_header_ids(
    user_id: &str,
    addresses: &[String],
    options: &UnifiedMailHeadersOptions,
) -> DbResult<Vec<String>> {
    Ok(ids_of(rows_of(&build_unified_mail_headers_query(user_id, addresses, options, Some("mail_id"))).await?))
}

/// One page of a list: its ids in order, and where the next page starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailIdPage {
    pub ids: Vec<String>,
    pub next: Option<ListCursor>,
}

/// A row of a page query: the id and its `date::text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorRow {
    pub mail_id: String,
    pub cursor_date: String,
}

/// The page of `limit` ids in `rows`. The page query asks for one row past
/// the page, so the next cursor exists only when that extra row came back.
pub fn to_id_page(rows: &[CursorRow], limit: usize) -> MailIdPage {
    let page = &rows[..rows.len().min(limit)];
    let next = match page.last() {
        Some(last) if rows.len() > limit => Some(ListCursor { date: last.cursor_date.clone(), id: last.mail_id.clone() }),
        _ => None,
    };
    MailIdPage { ids: page.iter().map(|row| row.mail_id.clone()).collect(), next }
}

fn cursor_rows(rows: Vec<Record>) -> Vec<CursorRow> {
    rows.into_iter()
        .map(|row| CursorRow {
            mail_id: row.get("mail_id").and_then(Value::as_str).unwrap_or("").to_string(),
            cursor_date: row.get("cursor_date").and_then(Value::as_str).unwrap_or("").to_string(),
        })
        .collect()
}

pub async fn get_mail_header_id_page(
    user_id: &str,
    address: &str,
    options: &GetMailHeadersOptions,
    limit: usize,
) -> DbResult<MailIdPage> {
    let rows = rows_of(&build_mail_header_id_page_query(user_id, address, options, limit as i64)).await?;
    Ok(to_id_page(&cursor_rows(rows), limit))
}

pub async fn get_unified_mail_header_id_page(
    user_id: &str,
    addresses: &[String],
    options: &UnifiedMailHeadersOptions,
    limit: usize,
) -> DbResult<MailIdPage> {
    let rows = rows_of(&build_unified_mail_header_id_page_query(user_id, addresses, options, limit as i64)).await?;
    Ok(to_id_page(&cursor_rows(rows), limit))
}

/// The rows of one batch in the order of `ids`; an id with no row is skipped.
pub fn rows_in_id_order(ids: &[String], rows: Vec<Record>) -> Vec<Record> {
    let mut by_id: HashMap<String, Record> = HashMap::new();
    for row in rows {
        let id = row.get("mail_id").and_then(Value::as_str).unwrap_or("").to_string();
        by_id.insert(id, row);
    }
    ids.iter().filter_map(|id| by_id.remove(id)).collect()
}

/// The header rows of `ids` in the order given, [`HEADER_BATCH_SIZE`] at a
/// time, each batch handed to `sink`. An id expunged since it was listed is
/// skipped.
pub async fn get_mail_headers_by_ids(
    user_id: &str,
    ids: &[String],
    mut sink: impl FnMut(Vec<MailHeaderResult>),
) -> DbResult<()> {
    for batch in ids.chunks(HEADER_BATCH_SIZE) {
        let rows = rows_of(&build_mail_headers_by_ids_query(user_id, batch)).await?;
        sink(rows_in_id_order(batch, rows));
    }
    Ok(())
}

pub async fn get_unified_mail_headers(
    user_id: &str,
    addresses: &[String],
    options: &UnifiedMailHeadersOptions,
) -> DbResult<Vec<MailHeaderResult>> {
    rows_of(&build_unified_mail_headers_query(user_id, addresses, options, None)).await
}

/// Rows changed since a time, the ids that left the view, and the time to
/// ask from next.
#[derive(Debug, Clone, Default)]
pub struct MailHeadersDeltaResult {
    pub as_of: String,
    pub headers: Vec<MailHeaderResult>,
    pub expunged_ids: Vec<String>,
}

async fn as_of() -> DbResult<String> {
    let rows = rows_of(&build_delta_as_of_query()).await?;
    Ok(rows.first().and_then(|row| row.get("as_of")).and_then(Value::as_str).unwrap_or("").to_string())
}

/// The delta of a per-account list. A failure echoes `since` back as
/// `as_of` with nothing changed, so the client's cursor never passes unseen
/// mutations.
pub async fn get_mail_headers_delta(
    user_id: &str,
    address: &str,
    options: &GetMailHeadersOptions,
    since: &str,
) -> MailHeadersDeltaResult {
    let read = async {
        let as_of = as_of().await?;
        let eviction = build_delta_eviction_query(user_id, address, options, since);
        let list = GetMailHeadersOptions {
            sent: options.sent,
            saved: options.saved,
            spam: options.spam,
            since: Some(since.to_string()),
            ..Default::default()
        };
        let (headers, expunged) = tokio::join!(get_mail_headers(user_id, address, &list), rows_of(&eviction));
        DbResult::Ok(MailHeadersDeltaResult { as_of, headers: headers?, expunged_ids: ids_of(expunged?) })
    };
    match read.await {
        Ok(delta) => delta,
        Err(error) => {
            logger::error_with("Failed to get mail headers delta", json!({}), &error);
            MailHeadersDeltaResult { as_of: since.to_string(), ..Default::default() }
        }
    }
}

/// The unified list's [`get_mail_headers_delta`].
pub async fn get_unified_mail_headers_delta(user_id: &str, addresses: &[String], since: &str) -> MailHeadersDeltaResult {
    let read = async {
        let as_of = as_of().await?;
        let eviction = build_unified_delta_eviction_query(user_id, addresses, since);
        let list = UnifiedMailHeadersOptions { since: Some(since.to_string()), ..Default::default() };
        let (headers, expunged) =
            tokio::join!(get_unified_mail_headers(user_id, addresses, &list), rows_of(&eviction));
        DbResult::Ok(MailHeadersDeltaResult { as_of, headers: headers?, expunged_ids: ids_of(expunged?) })
    };
    match read.await {
        Ok(delta) => delta,
        Err(error) => {
            logger::error_with("Failed to get unified mail headers delta", json!({}), &error);
            MailHeadersDeltaResult { as_of: since.to_string(), ..Default::default() }
        }
    }
}

/// A search hit: its list row and the highlighted fragments.
#[derive(Debug, Clone)]
pub struct SearchMailResult {
    pub row: MailHeaderResult,
    /// `subject` and `text` lists, each present only when it highlights a match.
    pub highlight: Map<String, Value>,
}

/// Full-text search over the user's mail. A failure reads as no results.
pub async fn search_mails(user_id: &str, search_term: &str) -> Vec<SearchMailResult> {
    match rows_of(&build_search_mails_query(user_id, search_term)).await {
        Ok(rows) => rows
            .into_iter()
            .map(|mut row| {
                let subject = row.remove("subject_highlight");
                let text = row.remove("text_highlight");
                let mut highlight = Map::new();
                for (key, value) in [("subject", subject), ("text", text)] {
                    if let Some(Value::String(fragment)) = value {
                        if fragment.contains("<em>") {
                            highlight.insert(key.to_string(), json!([fragment]));
                        }
                    }
                }
                SearchMailResult { row, highlight }
            })
            .collect(),
        Err(error) => {
            logger::error_with("Failed to search mails", json!({}), &error);
            Vec::new()
        }
    }
}

/// One address of the account sidebar.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountStat {
    pub address: String,
    pub count: i64,
    pub unread: i64,
    pub saved: i64,
    /// ISO time of the newest mail.
    pub latest: String,
}

fn account_stats(rows: Vec<Record>) -> Vec<AccountStat> {
    let number = |row: &Record, key: &str| {
        let value = row.get(key).unwrap_or(&Value::Null);
        value.as_i64().or_else(|| value.as_f64().map(|f| f as i64)).unwrap_or(0)
    };
    rows.iter()
        .map(|row| AccountStat {
            address: row.get("address").and_then(Value::as_str).unwrap_or("").to_string(),
            count: number(row, "count"),
            unread: number(row, "unread"),
            saved: number(row, "saved_count"),
            latest: row.get("latest").and_then(Value::as_str).unwrap_or("").to_string(),
        })
        .collect()
}

/// Mail counts grouped by address: received (recipients), sent (From), or
/// spam (recipients of spam). A failure reads as no accounts.
pub async fn get_account_stats(
    user_id: &str,
    sent: bool,
    domain_filter: Option<&str>,
    spam_only: bool,
) -> Vec<AccountStat> {
    match rows_of(&build_account_stats_query(user_id, sent, domain_filter, spam_only)).await {
        Ok(rows) => account_stats(rows),
        Err(error) => {
            logger::error_with("Failed to get account stats", json!({}), &error);
            Vec::new()
        }
    }
}

/// Received accounts that own at least one mail matching a search term.
pub async fn search_account_stats(user_id: &str, search_term: &str, domain_filter: Option<&str>) -> Vec<AccountStat> {
    match rows_of(&build_search_account_stats_query(user_id, search_term, domain_filter)).await {
        Ok(rows) => account_stats(rows),
        Err(error) => {
            logger::error_with("Failed to search account stats", json!({}), &error);
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, date: &str) -> CursorRow {
        CursorRow { mail_id: id.into(), cursor_date: date.into() }
    }

    #[test]
    fn a_page_has_a_next_cursor_only_when_the_extra_row_came_back() {
        let rows = vec![row("a", "d1"), row("b", "d2"), row("c", "d3")];
        let page = to_id_page(&rows, 2);
        assert_eq!(page.ids, vec!["a", "b"]);
        assert_eq!(page.next, Some(ListCursor { date: "d2".into(), id: "b".into() }));

        let page = to_id_page(&rows[..2], 2);
        assert_eq!(page.ids, vec!["a", "b"]);
        assert_eq!(page.next, None);

        assert_eq!(to_id_page(&[], 5), MailIdPage { ids: vec![], next: None });
    }

    #[test]
    fn batches_keep_the_order_of_the_ids_and_skip_missing_rows() {
        let record = |id: &str| {
            let mut map = Record::new();
            map.insert("mail_id".into(), json!(id));
            map
        };
        let ids: Vec<String> = ["c", "a", "x", "b"].iter().map(|s| s.to_string()).collect();
        let ordered = rows_in_id_order(&ids, vec![record("a"), record("b"), record("c")]);
        let got: Vec<&str> = ordered.iter().map(|r| r["mail_id"].as_str().unwrap()).collect();
        assert_eq!(got, vec!["c", "a", "b"]);
    }

    #[test]
    fn account_stats_read_counts_of_any_numeric_type() {
        let mut map = Record::new();
        map.insert("address".into(), json!("a@x.test"));
        map.insert("count".into(), json!(3));
        map.insert("unread".into(), json!(2.0));
        map.insert("saved_count".into(), json!(0));
        map.insert("latest".into(), json!("2026-01-01T00:00:00.000Z"));
        assert_eq!(
            account_stats(vec![map]),
            vec![AccountStat {
                address: "a@x.test".into(),
                count: 3,
                unread: 2,
                saved: 0,
                latest: "2026-01-01T00:00:00.000Z".into()
            }]
        );
    }
}
