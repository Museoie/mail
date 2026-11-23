//! The spam_allowlist table (repositories/spam_allowlists.ts, models/spam_allowlist.ts).
//!
//! Per-user allowlist entries for trusted senders. A pattern is an exact
//! address (`user@example.com`) or a domain wildcard (`*@example.com`), stored
//! lowercased.
//!
//! The statements are the Bun server's, text for text; both servers read and
//! write the same rows on one database.

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::postgres::{pool, DbError, DbResult, Param, Row};
use crate::{logger, params};

/// `pattern` is `TEXT`, so the column imposes no ceiling of its own. The cap is
/// on bytes rather than characters because a non-ASCII character occupies up to
/// four of them, and bytes are what the per-received-mail lookup pays. 320 is
/// the longest address RFC 5321 permits (64-byte local part, `@`, 255-byte
/// domain), so no legal address is refused.
pub const ALLOWLIST_PATTERN_MAX_BYTES: usize = 320;

/// Ceiling on allowlist entries per user. The allowlist lookup runs on every
/// received mail and no index covers `LOWER(pattern)`, so every row for that
/// user is scanned — the row count is what bounds that scan.
pub const ALLOWLIST_COUNT_MAX: i64 = 1000;

/// A row of `spam_allowlist`. Serializes as the Bun model's `toJSON()` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpamAllowlistModel {
    pub allowlist_id: String,
    pub user_id: String,
    pub pattern: String,
    /// ISO 8601 in milliseconds, as the pool returns a `timestamptz`.
    pub created_at: String,
}

impl SpamAllowlistModel {
    pub fn from_row(row: &Row) -> SpamAllowlistModel {
        SpamAllowlistModel::from_map(row.as_map())
    }

    fn from_map(row: &Map<String, Value>) -> SpamAllowlistModel {
        let text = |column: &str| row.get(column).and_then(Value::as_str).unwrap_or_default().to_string();
        SpamAllowlistModel {
            allowlist_id: text("allowlist_id"),
            user_id: text("user_id"),
            pattern: text("pattern"),
            created_at: text("created_at"),
        }
    }
}

/// The outcome of an add attempt. A refusal names its own reason so the route
/// can answer with the message the user acts on — `Exists` is already-done to
/// someone re-adding a trusted sender, while `AtLimit` and `TooLong` are not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAllowlistEntryResult {
    Created(SpamAllowlistModel),
    Exists,
    AtLimit,
    TooLong,
}

impl AddAllowlistEntryResult {
    /// The Bun server's `status` discriminant: `created`, `exists`, `at_limit` or `too_long`.
    pub fn status(&self) -> &'static str {
        match self {
            AddAllowlistEntryResult::Created(_) => "created",
            AddAllowlistEntryResult::Exists => "exists",
            AddAllowlistEntryResult::AtLimit => "at_limit",
            AddAllowlistEntryResult::TooLong => "too_long",
        }
    }
}

/// What a statement reported, each row as a column map.
#[derive(Debug, Default)]
struct Reply {
    rows: Vec<Map<String, Value>>,
    row_count: Option<u64>,
}

/// Where the table functions run their statements: the pool in production, a
/// recorder in the unit tests.
trait Database {
    async fn query(&self, sql: &str, params: &[Param]) -> DbResult<Reply>;
}

struct PoolDatabase;

impl Database for PoolDatabase {
    async fn query(&self, sql: &str, params: &[Param]) -> DbResult<Reply> {
        let result = pool().query(sql, params).await?;
        Ok(Reply { row_count: result.row_count, rows: result.rows.into_iter().map(Row::into_map).collect() })
    }
}

/// `COUNT(*) AS count` of the first row; zero when the row is missing.
fn count_of(reply: &Reply) -> i64 {
    match reply.rows.first().and_then(|row| row.get("count")) {
        Some(Value::Number(count)) => count.as_i64().unwrap_or(0),
        Some(Value::String(count)) => count.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// All entries of a user, newest first.
async fn all_for_user(db: &impl Database, user_id: &str) -> DbResult<Vec<SpamAllowlistModel>> {
    let sql = "SELECT * FROM spam_allowlist WHERE user_id = $1 ORDER BY created_at DESC";
    let reply = db.query(sql, &params![user_id]).await?;
    Ok(reply.rows.iter().map(SpamAllowlistModel::from_map).collect())
}

/// Whether the address matches an exact or a domain-wildcard entry of the user.
async fn has_match(db: &impl Database, user_id: &str, email_address: &str) -> DbResult<bool> {
    let normalized_email = email_address.to_lowercase();
    // `split("@")[1]`: the second component, so `a@b@c` reads as domain `b`,
    // and an address with no `@` interpolates JavaScript's `undefined`.
    let domain = normalized_email.split('@').nth(1).unwrap_or("undefined");
    let sql = concat!(
        "\n",
        "      SELECT COUNT(*) AS count FROM spam_allowlist\n",
        "      WHERE user_id = $1\n",
        "        AND (LOWER(pattern) = $2 OR LOWER(pattern) = $3)\n",
        "    ",
    );
    let wildcard = format!("*@{domain}");
    let reply = db.query(sql, &params![user_id, normalized_email.as_str(), wildcard]).await?;
    Ok(count_of(&reply) > 0)
}

/// Inserts a new entry, refusing a pattern over
/// [`ALLOWLIST_PATTERN_MAX_BYTES`] and a user already at
/// [`ALLOWLIST_COUNT_MAX`] rows.
///
/// Both ceilings live here because this is the only write path to the table.
/// The count is a subquery of the INSERT rather than a preceding SELECT so
/// the row being counted and the row being written are decided together, and
/// a zero-row result is disambiguated by probing for the pattern first — an
/// entry that already exists reports as existing even at the ceiling.
async fn add_entry(db: &impl Database, user_id: &str, pattern: &str) -> DbResult<AddAllowlistEntryResult> {
    let normalized_pattern = pattern.to_lowercase();
    if normalized_pattern.len() > ALLOWLIST_PATTERN_MAX_BYTES {
        return Ok(AddAllowlistEntryResult::TooLong);
    }

    let sql = concat!(
        "\n",
        "      INSERT INTO spam_allowlist (user_id, pattern)\n",
        "      SELECT $1, $2\n",
        "      WHERE (SELECT COUNT(*) FROM spam_allowlist WHERE user_id = $1) < $3\n",
        "      ON CONFLICT (user_id, pattern) DO NOTHING\n",
        "      RETURNING *\n",
        "    ",
    );
    let reply = db.query(sql, &params![user_id, normalized_pattern.as_str(), ALLOWLIST_COUNT_MAX]).await?;
    if let Some(row) = reply.rows.first() {
        return Ok(AddAllowlistEntryResult::Created(SpamAllowlistModel::from_map(row)));
    }

    let probe = db
        .query(
            "SELECT 1 FROM spam_allowlist WHERE user_id = $1 AND pattern = $2 LIMIT 1",
            &params![user_id, normalized_pattern.as_str()],
        )
        .await?;
    Ok(if probe.row_count.unwrap_or(0) > 0 {
        AddAllowlistEntryResult::Exists
    } else {
        AddAllowlistEntryResult::AtLimit
    })
}

/// Deletes the entry matching the user and pattern (case-insensitive).
async fn remove_by_pattern(db: &impl Database, user_id: &str, pattern: &str) -> DbResult<bool> {
    let normalized_pattern = pattern.to_lowercase();
    let sql = concat!(
        "\n",
        "      DELETE FROM spam_allowlist\n",
        "      WHERE user_id = $1 AND LOWER(pattern) = $2\n",
        "    ",
    );
    let reply = db.query(sql, &params![user_id, normalized_pattern]).await?;
    Ok(reply.row_count.unwrap_or(0) > 0)
}

/// Deletes an entry by its primary key, scoped to the user for safety.
async fn remove_by_id(db: &impl Database, user_id: &str, allowlist_id: &str) -> DbResult<bool> {
    let sql = concat!(
        "\n",
        "      DELETE FROM spam_allowlist\n",
        "      WHERE user_id = $1 AND allowlist_id = $2\n",
        "    ",
    );
    let reply = db.query(sql, &params![user_id, allowlist_id]).await?;
    Ok(reply.row_count.unwrap_or(0) > 0)
}

/// All allowlist entries of a user, newest first.
pub async fn get_allowlist_for_user(user_id: &str) -> Result<Vec<SpamAllowlistModel>, DbError> {
    all_for_user(&PoolDatabase, user_id).await
}

/// Whether an address matches an allowlist entry of the user: the address
/// itself, or `*@` and its domain, compared lowercased.
///
/// ```ignore
/// // with `*@example.com` on the user's list
/// is_allowlisted(user_id, "Bob@Example.COM").await?; // true
/// ```
pub async fn is_allowlisted(user_id: &str, email_address: &str) -> Result<bool, DbError> {
    has_match(&PoolDatabase, user_id, email_address).await
}

/// Adds an allowlist entry for a user, lowercased. Refuses a duplicate, an
/// oversized pattern, or a user at the row ceiling.
pub async fn add_allowlist_entry(user_id: &str, pattern: &str) -> Result<AddAllowlistEntryResult, DbError> {
    match add_entry(&PoolDatabase, user_id, pattern).await {
        Ok(result) => Ok(result),
        Err(error) => {
            logger::error_with("Error adding allowlist entry", json!({}), &error);
            Err(error)
        }
    }
}

/// Removes an allowlist entry by user and pattern (case-insensitive).
/// Returns whether a row was deleted.
pub async fn remove_allowlist_entry(user_id: &str, pattern: &str) -> Result<bool, DbError> {
    remove_by_pattern(&PoolDatabase, user_id, pattern).await
}

/// Removes an allowlist entry by id. Returns whether a row was deleted.
pub async fn remove_allowlist_entry_by_id(user_id: &str, allowlist_id: &str) -> Result<bool, DbError> {
    remove_by_id(&PoolDatabase, user_id, allowlist_id).await
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// Records every statement and answers from `planned`, in order; an
    /// unplanned statement reports no rows and no count.
    #[derive(Default)]
    struct Recorder {
        captured: Mutex<Vec<(String, Vec<Param>)>>,
        planned: Mutex<VecDeque<Reply>>,
    }

    impl Recorder {
        fn with(replies: Vec<Reply>) -> Recorder {
            Recorder { captured: Mutex::default(), planned: Mutex::new(replies.into()) }
        }

        fn captured(&self) -> Vec<(String, Vec<Param>)> {
            self.captured.lock().unwrap().clone()
        }
    }

    impl Database for Recorder {
        async fn query(&self, sql: &str, params: &[Param]) -> DbResult<Reply> {
            self.captured.lock().unwrap().push((sql.to_string(), params.to_vec()));
            Ok(self.planned.lock().unwrap().pop_front().unwrap_or_default())
        }
    }

    fn rows(rows: Vec<Value>) -> Reply {
        let rows: Vec<Map<String, Value>> =
            rows.into_iter().map(|row| row.as_object().cloned().unwrap()).collect();
        Reply { row_count: None, rows }
    }

    fn count(row_count: Option<u64>) -> Reply {
        Reply { rows: Vec::new(), row_count }
    }

    fn text(values: &[&str]) -> Vec<Param> {
        values.iter().map(|value| Param::Text(value.to_string())).collect()
    }

    fn created_row(user_id: &str) -> Value {
        json!({
            "allowlist_id": "aaaa",
            "user_id": user_id,
            "pattern": "alice@example.com",
            "created_at": "2026-05-14T00:00:00Z",
        })
    }

    #[test]
    fn the_table_declares_the_unique_constraint_add_entry_conflicts_on() {
        let schema = crate::postgres::schema::schema();
        let table =
            schema.tables.iter().find(|table| table.name == "spam_allowlist").expect("the table exists");
        assert!(table.create_sql.contains("UNIQUE(user_id, pattern)"));
    }

    mod add_entry {
        use super::*;

        const USER: &str = "11111111-1111-1111-1111-111111111111";

        #[tokio::test]
        async fn issues_an_insert_on_conflict_do_nothing() {
            let db = Recorder::with(vec![rows(vec![created_row(USER)])]);
            let result = add_entry(&db, USER, "alice@example.com").await.unwrap();
            assert_eq!(result.status(), "created");
            assert!(
                matches!(&result, AddAllowlistEntryResult::Created(entry) if entry.pattern == "alice@example.com")
            );
            let captured = db.captured();
            assert_eq!(captured.len(), 1);
            assert!(captured[0].0.contains("INSERT INTO spam_allowlist"));
            assert!(captured[0].0.contains("ON CONFLICT (user_id, pattern)"));
            assert!(captured[0].0.contains("DO NOTHING"));
            assert!(captured[0].0.contains("RETURNING *"));
        }

        #[tokio::test]
        async fn lowercases_the_pattern_before_insertion() {
            let db = Recorder::with(vec![rows(vec![created_row(USER)])]);
            add_entry(&db, USER, "ALICE@Example.COM").await.unwrap();
            assert_eq!(db.captured()[0].1, text(&[USER, "alice@example.com", "1000"]));
        }

        #[tokio::test]
        async fn gates_the_insert_on_the_users_row_count() {
            let db = Recorder::with(vec![rows(vec![created_row(USER)])]);
            add_entry(&db, USER, "alice@example.com").await.unwrap();
            let captured = db.captured();
            assert!(captured[0].0.contains("SELECT COUNT(*)"));
            assert!(captured[0].0.contains("WHERE user_id = $1) < $3"));
            assert_eq!(captured[0].1[2], Param::Text(ALLOWLIST_COUNT_MAX.to_string()));
        }

        #[tokio::test]
        async fn reports_exists_when_the_write_produced_no_row_and_the_pattern_is_present() {
            let db = Recorder::with(vec![
                count(Some(0)),
                Reply { rows: rows(vec![json!({ "?column?": 1 })]).rows, row_count: Some(1) },
            ]);
            let result = add_entry(&db, USER, "alice@example.com").await.unwrap();
            assert_eq!(result, AddAllowlistEntryResult::Exists);
            let captured = db.captured();
            assert_eq!(captured.len(), 2);
            assert!(captured[1].0.contains("SELECT 1 FROM spam_allowlist"));
        }

        #[tokio::test]
        async fn reports_at_limit_when_the_write_produced_no_row_and_the_pattern_is_absent() {
            let db = Recorder::with(vec![count(Some(0)), count(Some(0))]);
            let result = add_entry(&db, USER, "new@example.com").await.unwrap();
            assert_eq!(result, AddAllowlistEntryResult::AtLimit);
        }

        #[tokio::test]
        async fn probes_for_the_pattern_before_concluding_at_limit() {
            let db = Recorder::with(vec![
                count(Some(0)),
                Reply { rows: rows(vec![json!({ "?column?": 1 })]).rows, row_count: Some(1) },
            ]);
            let result = add_entry(&db, USER, "alice@example.com").await.unwrap();
            assert_ne!(result.status(), "at_limit");
            assert_eq!(result.status(), "exists");
        }

        #[tokio::test]
        async fn refuses_a_pattern_over_the_byte_ceiling_without_issuing_a_query() {
            let db = Recorder::default();
            let local_part = "a".repeat(ALLOWLIST_PATTERN_MAX_BYTES);
            let result = add_entry(&db, USER, &format!("{local_part}@example.com")).await.unwrap();
            assert_eq!(result, AddAllowlistEntryResult::TooLong);
            assert!(db.captured().is_empty());
        }

        #[tokio::test]
        async fn measures_the_ceiling_in_utf8_bytes_not_characters() {
            // 200 two-byte characters count as 200 towards a character count
            // and 400 towards the measure the column actually stores.
            let db = Recorder::default();
            let pattern = format!("{}@example.com", "é".repeat(200));
            assert!(crate::js::utf16_len(&pattern) < ALLOWLIST_PATTERN_MAX_BYTES);
            assert!(pattern.len() > ALLOWLIST_PATTERN_MAX_BYTES);
            let result = add_entry(&db, USER, &pattern).await.unwrap();
            assert_eq!(result, AddAllowlistEntryResult::TooLong);
            assert!(db.captured().is_empty());
        }

        #[tokio::test]
        async fn accepts_a_pattern_exactly_at_the_byte_ceiling() {
            let local_part = "a".repeat(ALLOWLIST_PATTERN_MAX_BYTES - "@example.com".len());
            let pattern = format!("{local_part}@example.com");
            assert_eq!(pattern.len(), ALLOWLIST_PATTERN_MAX_BYTES);
            let mut row = created_row(USER);
            row["pattern"] = json!(pattern);
            let db = Recorder::with(vec![rows(vec![row])]);
            let result = add_entry(&db, USER, &pattern).await.unwrap();
            assert_eq!(result.status(), "created");
            assert_eq!(db.captured().len(), 1);
        }
    }

    #[test]
    fn bounds_the_pattern_at_the_longest_address_rfc_5321_permits() {
        assert_eq!(ALLOWLIST_PATTERN_MAX_BYTES, 320);
    }

    #[test]
    fn caps_rows_per_user() {
        assert_eq!(ALLOWLIST_COUNT_MAX, 1000);
    }

    mod is_allowlisted {
        use super::*;

        const USER: &str = "22222222-2222-2222-2222-222222222222";

        #[tokio::test]
        async fn queries_with_both_exact_and_domain_wildcard_forms_lowercased() {
            let db = Recorder::with(vec![rows(vec![json!({ "count": 1 })])]);
            assert!(has_match(&db, USER, "BOB@Example.COM").await.unwrap());
            let captured = db.captured();
            assert_eq!(captured.len(), 1);
            assert!(captured[0].0.contains("COUNT(*) AS count"));
            assert_eq!(captured[0].1, text(&[USER, "bob@example.com", "*@example.com"]));
        }

        #[tokio::test]
        async fn returns_false_when_the_count_is_zero() {
            let db = Recorder::with(vec![rows(vec![json!({ "count": 0 })])]);
            assert!(!has_match(&db, USER, "stranger@unknown.com").await.unwrap());
        }

        #[tokio::test]
        async fn returns_false_when_the_count_row_is_missing() {
            let db = Recorder::default();
            assert!(!has_match(&db, USER, "x@y.com").await.unwrap());
        }

        #[tokio::test]
        async fn reads_a_count_the_driver_left_as_text() {
            // The Bun suite plants the count as the string a raw driver returns.
            let db = Recorder::with(vec![
                rows(vec![json!({ "count": "1" })]),
                rows(vec![json!({ "count": "0" })]),
            ]);
            assert!(has_match(&db, USER, "bob@example.com").await.unwrap());
            assert!(!has_match(&db, USER, "bob@example.com").await.unwrap());
        }

        #[tokio::test]
        async fn takes_the_second_component_as_the_domain() {
            // `split("@")[1]`, quirks included: the middle of a multi-@ address,
            // and the literal `undefined` for an address with no `@`.
            let db = Recorder::default();
            has_match(&db, USER, "a@B.example@c.example").await.unwrap();
            has_match(&db, USER, "Postmaster").await.unwrap();
            has_match(&db, USER, "trailing@").await.unwrap();
            let captured = db.captured();
            assert_eq!(captured[0].1, text(&[USER, "a@b.example@c.example", "*@b.example"]));
            assert_eq!(captured[1].1, text(&[USER, "postmaster", "*@undefined"]));
            assert_eq!(captured[2].1, text(&[USER, "trailing@", "*@"]));
        }
    }

    mod remove_by_pattern {
        use super::*;

        const USER: &str = "33333333-3333-3333-3333-333333333333";

        #[tokio::test]
        async fn issues_a_delete_filtered_by_user_and_lower_pattern_and_lowercases_input() {
            let db = Recorder::with(vec![count(Some(1))]);
            assert!(remove_by_pattern(&db, USER, "Alice@Example.com").await.unwrap());
            let captured = db.captured();
            assert!(captured[0].0.contains("DELETE FROM spam_allowlist"));
            assert!(captured[0].0.contains("LOWER(pattern) = $2"));
            assert_eq!(captured[0].1, text(&[USER, "alice@example.com"]));
        }

        #[tokio::test]
        async fn returns_false_when_no_row_matched() {
            let db = Recorder::with(vec![count(Some(0))]);
            assert!(!remove_by_pattern(&db, USER, "ghost@example.com").await.unwrap());
        }

        #[tokio::test]
        async fn treats_a_missing_row_count_as_zero() {
            let db = Recorder::with(vec![count(None)]);
            assert!(!remove_by_pattern(&db, USER, "ghost@example.com").await.unwrap());
        }
    }

    mod remove_by_id {
        use super::*;

        const USER: &str = "44444444-4444-4444-4444-444444444444";
        const ALLOWLIST_ID: &str = "55555555-5555-5555-5555-555555555555";

        #[tokio::test]
        async fn scopes_the_delete_by_both_user_and_allowlist_id() {
            let db = Recorder::with(vec![count(Some(1))]);
            assert!(remove_by_id(&db, USER, ALLOWLIST_ID).await.unwrap());
            let captured = db.captured();
            assert!(captured[0].0.contains("DELETE FROM spam_allowlist"));
            assert!(captured[0].0.contains("user_id = $1"));
            assert!(captured[0].0.contains("allowlist_id = $2"));
            assert_eq!(captured[0].1, text(&[USER, ALLOWLIST_ID]));
        }

        #[tokio::test]
        async fn returns_false_when_no_row_matched() {
            let db = Recorder::with(vec![count(Some(0))]);
            assert!(!remove_by_id(&db, USER, ALLOWLIST_ID).await.unwrap());
        }
    }

    mod all_for_user {
        use super::*;

        const USER: &str = "66666666-6666-6666-6666-666666666666";

        #[tokio::test]
        async fn selects_the_users_rows_newest_first() {
            let db = Recorder::with(vec![rows(vec![
                json!({ "allowlist_id": "z", "user_id": USER, "pattern": "p1", "created_at": "2026-05-14T00:00:00Z" }),
                json!({ "allowlist_id": "y", "user_id": USER, "pattern": "p2", "created_at": "2026-05-13T00:00:00Z" }),
            ])]);
            let result = all_for_user(&db, USER).await.unwrap();
            assert_eq!(result.len(), 2);
            assert_eq!(result[0].allowlist_id, "z");
            assert_eq!(result[1].pattern, "p2");
            let captured = db.captured();
            assert!(captured[0].0.contains("SELECT * FROM spam_allowlist"));
            assert!(captured[0].0.contains("user_id = $1"));
            assert!(captured[0].0.contains("ORDER BY created_at DESC"));
            assert_eq!(captured[0].1, text(&[USER]));
        }

        #[tokio::test]
        async fn returns_nothing_when_the_user_has_no_entries() {
            let db = Recorder::default();
            assert!(all_for_user(&db, USER).await.unwrap().is_empty());
        }
    }

    #[test]
    fn the_model_serializes_as_the_bun_model_does() {
        let model = SpamAllowlistModel::from_map(created_row("u").as_object().unwrap());
        assert_eq!(
            serde_json::to_string(&model).unwrap(),
            r#"{"allowlist_id":"aaaa","user_id":"u","pattern":"alice@example.com","created_at":"2026-05-14T00:00:00Z"}"#
        );
    }

    /// The statements the Bun server issues, recorded by
    /// `tests/golden/spam/generate.ts` from the real table object.
    #[tokio::test]
    async fn statements_match_the_bun_server_text_for_text() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tests/golden/spam/sql_allowlist.json"))
                .expect("the fixture is JSON");
        let cases = fixture["cases"].as_array().expect("cases");
        assert!(!cases.is_empty());
        for case in cases {
            let user_id = case["userId"].as_str().unwrap();
            let argument = case["argument"].as_str().unwrap();
            // The generator answers every statement with no rows, so an add
            // goes on to its probe, as it does here.
            let db = Recorder::default();
            let outcome = match case["call"].as_str().unwrap() {
                "getAllForUser" => json!(all_for_user(&db, user_id).await.unwrap().len()),
                "isAllowlisted" => json!(has_match(&db, user_id, argument).await.unwrap()),
                "addEntry" => json!(add_entry(&db, user_id, argument).await.unwrap().status()),
                "removeByPattern" => json!(remove_by_pattern(&db, user_id, argument).await.unwrap()),
                "removeById" => json!(remove_by_id(&db, user_id, argument).await.unwrap()),
                other => panic!("unknown call {other}"),
            };
            assert_eq!(outcome, case["outcome"], "{case}");
            let expected: Vec<(String, Vec<Param>)> = case["statements"]
                .as_array()
                .unwrap()
                .iter()
                .map(|statement| {
                    let values = statement["params"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| Param::Text(value.as_str().unwrap().to_string()))
                        .collect();
                    (statement["sql"].as_str().unwrap().to_string(), values)
                })
                .collect();
            assert_eq!(db.captured(), expected, "{case}");
        }
    }
}
