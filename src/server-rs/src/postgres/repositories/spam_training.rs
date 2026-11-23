//! The spam_training table (repositories/spam_training.ts, models/spam_training.ts).
//!
//! Per-user Naive Bayes training data: one row per word, counting how many
//! spam and how many ham documents it appeared in. The row whose word is
//! `__total__` holds the number of documents trained in each class.
//!
//! The statements are the Bun server's, text for text; both servers write the
//! same rows on one database.

use std::collections::{HashMap, HashSet};

use serde_json::json;

use crate::postgres::{pool, DbError, Param, Row};
use crate::{logger, params};

/// The word of the sentinel row that holds the per-class document totals. No
/// token can collide with it: tokens are letters only.
const TOTAL_WORD: &str = "__total__";

/// A row of `spam_training`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpamTrainingModel {
    pub training_id: String,
    pub user_id: String,
    pub word: String,
    pub spam_count: i64,
    pub ham_count: i64,
}

impl SpamTrainingModel {
    pub fn from_row(row: &Row) -> SpamTrainingModel {
        SpamTrainingModel {
            training_id: row.text("training_id"),
            user_id: row.text("user_id"),
            word: row.text("word"),
            spam_count: row.i64("spam_count"),
            ham_count: row.i64("ham_count"),
        }
    }
}

/// How many spam and ham documents a user's classifier was trained with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DocCounts {
    pub spam_docs: i64,
    pub ham_docs: i64,
}

/// How many spam and ham documents a word appeared in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WordCounts {
    pub spam_count: i64,
    pub ham_count: i64,
}

type Statement = (String, Vec<Param>);

fn all_for_user_statement(user_id: &str) -> Statement {
    ("SELECT * FROM spam_training WHERE user_id = $1".to_string(), params![user_id])
}

fn doc_counts_statement(user_id: &str) -> Statement {
    let sql = concat!(
        "\n",
        "      SELECT spam_count, ham_count\n",
        "      FROM spam_training\n",
        "      WHERE user_id = $1 AND word = $2\n",
        "    ",
    );
    (sql.to_string(), params![user_id, TOTAL_WORD])
}

/// One multi-row upsert for every distinct word plus the totals row, or
/// `None` when there is nothing to train with. The words are de-duplicated
/// first: a multi-row `VALUES` cannot upsert one conflict key twice.
fn train_statement(user_id: &str, words: &[String], is_spam: bool) -> Option<Statement> {
    if words.is_empty() {
        return None;
    }

    let spam_increment = if is_spam { 1 } else { 0 };
    let ham_increment = if is_spam { 0 } else { 1 };

    let mut seen = HashSet::new();
    let unique_words = words.iter().map(String::as_str).filter(|word| seen.insert(*word));

    let mut values: Vec<Param> = params![user_id, spam_increment, ham_increment];
    let mut values_sql = Vec::new();
    for word in unique_words.chain([TOTAL_WORD]) {
        values.push(Param::from(word));
        values_sql.push(format!("($1, ${}, $2, $3)", values.len()));
    }

    let sql = format!(
        concat!(
            "\n",
            "      INSERT INTO spam_training (user_id, word, spam_count, ham_count)\n",
            "      VALUES {}\n",
            "      ON CONFLICT (user_id, word)\n",
            "      DO UPDATE SET\n",
            "        spam_count = spam_training.spam_count + EXCLUDED.spam_count,\n",
            "        ham_count = spam_training.ham_count + EXCLUDED.ham_count\n",
            "    ",
        ),
        values_sql.join(", ")
    );
    Some((sql, values))
}

/// Selects the rows of the given words, or `None` when no word is asked for.
fn word_counts_statement(user_id: &str, words: &[String]) -> Option<Statement> {
    if words.is_empty() {
        return None;
    }
    let placeholders: Vec<String> = (0..words.len()).map(|index| format!("${}", index + 2)).collect();
    let sql = format!(
        concat!(
            "\n",
            "      SELECT * FROM spam_training\n",
            "      WHERE user_id = $1 AND word IN ({})\n",
            "    ",
        ),
        placeholders.join(", ")
    );
    let mut values: Vec<Param> = params![user_id];
    values.extend(words.iter().map(Param::from));
    Some((sql, values))
}

fn clear_statement(user_id: &str) -> Statement {
    ("DELETE FROM spam_training WHERE user_id = $1".to_string(), params![user_id])
}

/// Every training row of a user, the totals row included.
pub async fn get_all_training_for_user(user_id: &str) -> Result<Vec<SpamTrainingModel>, DbError> {
    let (sql, values) = all_for_user_statement(user_id);
    let result = pool().query(&sql, &values).await?;
    Ok(result.rows.iter().map(SpamTrainingModel::from_row).collect())
}

/// Trains the classifier for a user with a document's words: `is_spam` counts
/// each distinct word, and the document itself, as spam; otherwise as ham.
///
/// A word longer than the column's 100 characters fails the whole statement,
/// as it does in the Bun server; nothing of that document is recorded.
pub async fn train_classifier(user_id: &str, words: &[String], is_spam: bool) -> Result<(), DbError> {
    let Some((sql, values)) = train_statement(user_id, words, is_spam) else {
        return Ok(());
    };
    match pool().query(&sql, &values).await {
        Ok(_) => Ok(()),
        Err(error) => {
            logger::error_with("Error training spam classifier", json!({}), &error);
            Err(error)
        }
    }
}

/// The total spam and ham document counts of a user's classifier; zeros
/// before the first training.
pub async fn get_classifier_doc_counts(user_id: &str) -> Result<DocCounts, DbError> {
    let (sql, values) = doc_counts_statement(user_id);
    let result = pool().query(&sql, &values).await?;
    Ok(match result.rows.first() {
        Some(row) => DocCounts { spam_docs: row.i64("spam_count"), ham_docs: row.i64("ham_count") },
        None => DocCounts::default(),
    })
}

/// Training counts of the given words. Words never trained are absent.
pub async fn get_word_counts(
    user_id: &str,
    words: &[String],
) -> Result<HashMap<String, WordCounts>, DbError> {
    let Some((sql, values)) = word_counts_statement(user_id, words) else {
        return Ok(HashMap::new());
    };
    let result = pool().query(&sql, &values).await?;
    Ok(result
        .rows
        .iter()
        .map(SpamTrainingModel::from_row)
        .map(|row| (row.word, WordCounts { spam_count: row.spam_count, ham_count: row.ham_count }))
        .collect())
}

/// Clears all training data of a user (full reset).
pub async fn clear_classifier_data(user_id: &str) -> Result<(), DbError> {
    let (sql, values) = clear_statement(user_id);
    match pool().query(&sql, &values).await {
        Ok(_) => Ok(()),
        Err(error) => {
            logger::error_with("Error clearing spam training data", json!({}), &error);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| word.to_string()).collect()
    }

    fn text(value: &str) -> Param {
        Param::Text(value.to_string())
    }

    const USER: &str = "11111111-1111-1111-1111-111111111111";

    #[test]
    fn train_upserts_every_distinct_word_and_the_totals_row() {
        let (sql, values) = train_statement(USER, &words(&["free", "prize", "free"]), true).unwrap();
        assert!(sql.contains("INSERT INTO spam_training (user_id, word, spam_count, ham_count)"));
        assert!(sql.contains("VALUES ($1, $4, $2, $3), ($1, $5, $2, $3), ($1, $6, $2, $3)\n"));
        assert!(sql.contains("ON CONFLICT (user_id, word)"));
        assert_eq!(
            values,
            vec![text(USER), text("1"), text("0"), text("free"), text("prize"), text("__total__")]
        );
    }

    #[test]
    fn train_as_ham_increments_the_ham_column() {
        let (_, values) = train_statement(USER, &words(&["standup"]), false).unwrap();
        assert_eq!(values, vec![text(USER), text("0"), text("1"), text("standup"), text("__total__")]);
    }

    #[test]
    fn train_with_no_words_issues_nothing() {
        assert!(train_statement(USER, &[], true).is_none());
    }

    #[test]
    fn word_counts_binds_one_placeholder_per_word() {
        let (sql, values) = word_counts_statement(USER, &words(&["free", "prize"])).unwrap();
        assert!(sql.contains("WHERE user_id = $1 AND word IN ($2, $3)"));
        assert_eq!(values, vec![text(USER), text("free"), text("prize")]);
        assert!(word_counts_statement(USER, &[]).is_none());
    }

    #[test]
    fn doc_counts_reads_the_totals_row() {
        let (sql, values) = doc_counts_statement(USER);
        assert!(sql.contains("SELECT spam_count, ham_count"));
        assert_eq!(values, vec![text(USER), text("__total__")]);
    }

    /// The statements the Bun server issues, recorded by
    /// `tests/golden/spam/generate.ts` from the real table object.
    #[test]
    fn statements_match_the_bun_server_text_for_text() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tests/golden/spam/sql_training.json"))
                .expect("the fixture is JSON");
        let cases = fixture["cases"].as_array().expect("cases");
        assert!(!cases.is_empty());
        for case in cases {
            let user_id = case["userId"].as_str().unwrap();
            let word_list: Vec<String> = case["words"]
                .as_array()
                .map(|list| list.iter().map(|word| word.as_str().unwrap().to_string()).collect())
                .unwrap_or_default();
            let statement = match case["call"].as_str().unwrap() {
                "getAllForUser" => Some(all_for_user_statement(user_id)),
                "getDocCounts" => Some(doc_counts_statement(user_id)),
                "train" => train_statement(user_id, &word_list, case["isSpam"].as_bool().unwrap()),
                "getWordCounts" => word_counts_statement(user_id, &word_list),
                "clearForUser" => Some(clear_statement(user_id)),
                other => panic!("unknown call {other}"),
            };
            let expected = &case["statements"].as_array().unwrap();
            match statement {
                None => assert!(expected.is_empty(), "{case}: Bun issued a statement, Rust none"),
                Some((sql, values)) => {
                    assert_eq!(expected.len(), 1, "{case}");
                    assert_eq!(sql, expected[0]["sql"].as_str().unwrap(), "{case}");
                    let expected_values: Vec<Param> = expected[0]["params"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| text(value.as_str().unwrap()))
                        .collect();
                    assert_eq!(values, expected_values, "{case}");
                }
            }
        }
    }
}
