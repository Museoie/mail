//! The reads behind the conversation view and reply threading
//! (repositories/mails/thread.ts).

use serde_json::Value;

use super::table::{run_query, PreparedQuery, Record};
use crate::mails::threading::ThreadCandidate;
use crate::postgres::{DbResult, Param};

/// The columns a thread view needs: the header fields of each message, its
/// threading headers, and a bounded body prefix for the collapsed preview.
const THREAD_COLUMNS: &str = "mail_id, message_id, in_reply_to, reference_ids, subject, date, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, envelope_to, read, saved, sent, is_spam, insight, left(text, 400) AS text_prefix";

/// Rows fetched by header links per round; a thread is never this long.
pub const THREAD_CANDIDATE_LIMIT: usize = 300;

/// Rows fetched by subject; the newest are the likeliest conversation.
pub const THREAD_SUBJECT_LIMIT: usize = 100;

/// The subject with leading reply/forward markers removed, whitespace
/// collapsed, lower-cased: the SQL counterpart of `normalize_subject`.
/// `idx_mails_thread_subject_key` is built on its digest, so the query must
/// use exactly this text.
pub const NORMALIZED_SUBJECT_SQL: &str = r"lower(btrim(regexp_replace(regexp_replace(subject, '^\s*((re|fwd?|aw|wg|sv|vs|antw|tr|rv)\s*(\[\d+\])?\s*[:：]\s*)+', '', 'i'), '\s+', ' ', 'g')))";

/// Header-less mail, the only rows the subject match applies to.
pub const HEADERLESS_SQL: &str = "in_reply_to IS NULL AND reference_ids IS NULL";

/// The Message-ID suffix IMAP COPY/MOVE gives a copy.
pub const COPY_MESSAGE_ID_SUFFIX: &str = ".copy@server";

fn subject_key_sql() -> String {
    format!("md5({NORMALIZED_SUBJECT_SQL})")
}

fn not_a_copy_sql() -> String {
    format!("message_id NOT LIKE '%{COPY_MESSAGE_ID_SUFFIX}'")
}

/// One mail of a conversation, as the thread queries select it.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRow {
    /// Every selected column, by name, in select order.
    pub record: Record,
    mail_id: String,
    message_id: String,
    in_reply_to: Option<String>,
    reference_ids: Option<Vec<String>>,
    subject: String,
}

impl ThreadRow {
    pub fn from_record(record: Record) -> ThreadRow {
        let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
        let reference_ids = match record.get("reference_ids") {
            Some(Value::Array(ids)) => Some(ids.iter().filter_map(Value::as_str).map(str::to_string).collect()),
            _ => None,
        };
        ThreadRow {
            mail_id: text("mail_id").unwrap_or_default(),
            message_id: text("message_id").unwrap_or_default(),
            in_reply_to: text("in_reply_to"),
            reference_ids,
            subject: text("subject").unwrap_or_default(),
            record,
        }
    }

    pub fn is_spam(&self) -> bool {
        self.record.get("is_spam").and_then(Value::as_bool).unwrap_or(false)
    }

    /// The stored date, as the ISO string the pool returns.
    pub fn date(&self) -> &str {
        self.record.get("date").and_then(Value::as_str).unwrap_or("")
    }
}

impl ThreadCandidate for ThreadRow {
    fn mail_id(&self) -> &str {
        &self.mail_id
    }

    fn message_id(&self) -> &str {
        &self.message_id
    }

    fn in_reply_to(&self) -> Option<&str> {
        self.in_reply_to.as_deref()
    }

    fn reference_ids(&self) -> Option<&[String]> {
        self.reference_ids.as_deref()
    }

    fn subject(&self) -> &str {
        &self.subject
    }

    fn from_address(&self) -> Option<&Value> {
        self.record.get("from_address").filter(|value| !value.is_null())
    }

    fn to_address(&self) -> Option<&Value> {
        self.record.get("to_address").filter(|value| !value.is_null())
    }

    fn cc_address(&self) -> Option<&Value> {
        self.record.get("cc_address").filter(|value| !value.is_null())
    }
}

pub fn build_thread_seed_query(user_id: &str, mail_id: &str) -> PreparedQuery {
    PreparedQuery {
        sql: format!(
            "SELECT {THREAD_COLUMNS} FROM mails
    WHERE user_id = $1 AND mail_id = $2 AND expunged = FALSE"
        ),
        values: vec![Param::from(user_id), Param::from(mail_id)],
    }
}

/// Mails whose own id, In-Reply-To or References overlaps `ids` (both
/// spellings of each id). Spam stays out unless the thread is spam itself.
pub fn build_thread_candidates_query(user_id: &str, ids: &[String], include_spam: bool) -> PreparedQuery {
    let spam = if include_spam { "" } else { "AND is_spam = FALSE" };
    PreparedQuery {
        sql: format!(
            "SELECT {THREAD_COLUMNS} FROM mails
    WHERE user_id = $1
      AND expunged = FALSE
      AND draft = FALSE
      AND {}
      {spam}
      AND (
        message_id = ANY($2::text[])
        OR in_reply_to = ANY($2::text[])
        OR (reference_ids IS NOT NULL AND reference_ids ?| $2::text[])
      )
    ORDER BY date DESC
    LIMIT {THREAD_CANDIDATE_LIMIT}",
            not_a_copy_sql()
        ),
        values: vec![Param::from(user_id), Param::array(ids)],
    }
}

/// Header-less mails whose normalized subject equals the seed's.
pub fn build_thread_subject_query(user_id: &str, seed_mail_id: &str, include_spam: bool) -> PreparedQuery {
    let spam = if include_spam { "" } else { "AND is_spam = FALSE" };
    PreparedQuery {
        sql: format!(
            "WITH seed AS (
      SELECT {NORMALIZED_SUBJECT_SQL} AS subject FROM mails
      WHERE user_id = $1 AND mail_id = $2
    )
    SELECT {THREAD_COLUMNS} FROM mails
    WHERE user_id = $1
      AND {HEADERLESS_SQL}
      AND {} = md5((SELECT subject FROM seed))
      AND {NORMALIZED_SUBJECT_SQL} = (SELECT subject FROM seed)
      AND expunged = FALSE
      AND draft = FALSE
      AND {}
      {spam}
    ORDER BY date DESC
    LIMIT {THREAD_SUBJECT_LIMIT}",
            subject_key_sql(),
            not_a_copy_sql()
        ),
        values: vec![Param::from(user_id), Param::from(seed_mail_id)],
    }
}

/// The References of the user's mail with this Message-ID, either spelling.
pub async fn get_reference_ids_by_message_id(user_id: &str, message_ids: &[String]) -> DbResult<Option<Vec<String>>> {
    let query = PreparedQuery {
        sql: "SELECT reference_ids FROM mails
      WHERE user_id = $1 AND message_id = ANY($2::text[]) AND expunged = FALSE
      LIMIT 1"
            .to_string(),
        values: vec![Param::from(user_id), Param::array(message_ids)],
    };
    let rows = run_query(&query).await?.rows;
    Ok(rows.first().and_then(|row| match row.get("reference_ids") {
        Some(Value::Array(ids)) => Some(ids.iter().map(|id| id.as_str().unwrap_or("").to_string()).collect()),
        _ => None,
    }))
}

pub async fn get_thread_seed(user_id: &str, mail_id: &str) -> DbResult<Option<ThreadRow>> {
    let rows = run_query(&build_thread_seed_query(user_id, mail_id)).await?.rows;
    Ok(rows.into_iter().next().map(ThreadRow::from_record))
}

pub async fn get_thread_candidates(user_id: &str, ids: &[String], include_spam: bool) -> DbResult<Vec<ThreadRow>> {
    let rows = run_query(&build_thread_candidates_query(user_id, ids, include_spam)).await?.rows;
    Ok(rows.into_iter().map(ThreadRow::from_record).collect())
}

pub async fn get_thread_subject_candidates(
    user_id: &str,
    seed_mail_id: &str,
    include_spam: bool,
) -> DbResult<Vec<ThreadRow>> {
    let rows = run_query(&build_thread_subject_query(user_id, seed_mail_id, include_spam)).await?.rows;
    Ok(rows.into_iter().map(ThreadRow::from_record).collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn spam_is_left_out_unless_asked_for() {
        let ids = vec!["a@x".to_string(), "<a@x>".to_string()];
        let query = build_thread_candidates_query("u", &ids, false);
        assert!(query.sql.contains("AND message_id NOT LIKE '%.copy@server'"));
        assert!(query.sql.contains("AND is_spam = FALSE"));
        assert!(query.sql.contains("LIMIT 300"));
        assert_eq!(query.values[1], Param::Text(r#"{"a@x","<a@x>"}"#.into()));
        assert!(!build_thread_candidates_query("u", &ids, true).sql.contains("AND is_spam = FALSE"));
        let subject = build_thread_subject_query("u", "m", false);
        assert!(subject.sql.contains("AND md5(lower(btrim(regexp_replace(regexp_replace(subject, '^\\s*((re|fwd?|aw"));
        assert!(subject.sql.contains("LIMIT 100"));
    }

    #[test]
    fn a_row_reads_its_threading_columns() {
        let mut record = Record::new();
        record.insert("mail_id".into(), json!("m"));
        record.insert("message_id".into(), json!("<m@x>"));
        record.insert("in_reply_to".into(), Value::Null);
        record.insert("reference_ids".into(), json!(["<a@x>"]));
        record.insert("subject".into(), json!("Re: hi"));
        record.insert("is_spam".into(), json!(true));
        record.insert("from_address".into(), Value::Null);
        let row = ThreadRow::from_record(record);
        assert_eq!(row.mail_id(), "m");
        assert_eq!(row.in_reply_to(), None);
        assert_eq!(row.reference_ids(), Some(&["<a@x>".to_string()][..]));
        assert!(row.is_spam());
        assert_eq!(row.from_address(), None);
    }
}
