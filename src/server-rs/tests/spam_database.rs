//! The spam tables against a real PostgreSQL: training upserts, word counts,
//! the allowlist with its wildcard, case and ceiling rules, and the same
//! stages the Bun server ran for `tests/golden/spam/crosscheck.json`.
//!
//! These run when `MAIL_TEST_POSTGRES=1` and report themselves skipped
//! otherwise:
//!
//! ```bash
//! MAIL_TEST_POSTGRES=1 cargo test --manifest-path src/server-rs/Cargo.toml --test spam_database
//! ```
//!
//! The server is `MAIL_TEST_POSTGRES_HOST`, `_PORT`, `_USER`, `_PASSWORD`
//! (default `postgres:postgres@127.0.0.1:5432`). A database named
//! `mail_spam_rs_<pid>_<time>` is created with the tables from `schema.json`
//! and dropped when the test process exits. `MAIL_TEST_POSTGRES_DATABASE`
//! names an existing database to use instead; that one is left in place.

use std::future::Future;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::runtime::Runtime;

use mail_server::params;
use mail_server::postgres::repositories::spam_allowlists::{
    add_allowlist_entry, get_allowlist_for_user, is_allowlisted, remove_allowlist_entry,
    remove_allowlist_entry_by_id, AddAllowlistEntryResult, ALLOWLIST_COUNT_MAX, ALLOWLIST_PATTERN_MAX_BYTES,
};
use mail_server::postgres::repositories::spam_training::{
    clear_classifier_data, get_all_training_for_user, get_classifier_doc_counts, get_word_counts,
    train_classifier, DocCounts, WordCounts,
};
use mail_server::postgres::schema::schema;
use mail_server::postgres::{pool, Pool, PoolOptions};
use mail_server::spam::classifier::{classify_email, extract_tokens, train_with_email};
use mail_server::spam::service::check_spam;
use mail_server::spam::types::{EmailContext, FlaggedBy};

/// One runtime for every test: the process-wide pool keeps connections that
/// belong to the runtime they were opened on.
static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("a runtime")
});

fn setting(name: &str, default: &str) -> String {
    std::env::var(format!("MAIL_TEST_POSTGRES_{name}")).unwrap_or_else(|_| default.to_string())
}

fn pool_for(database: &str) -> Pool {
    Pool::new(PoolOptions {
        host: setting("HOST", "127.0.0.1"),
        port: setting("PORT", "5432").parse().expect("a port"),
        user: setting("USER", "postgres"),
        password: Some(setting("PASSWORD", "postgres")),
        database: database.to_string(),
        max: 1,
        connection_timeout: Duration::from_secs(5),
        statement_timeout: Duration::from_secs(30),
        query_timeout: Duration::from_secs(30),
    })
}

/// The database this process created, for the exit handler to drop.
static CREATED: OnceLock<String> = OnceLock::new();

extern "C" {
    fn atexit(callback: extern "C" fn()) -> std::os::raw::c_int;
}

/// Drops the database this process created. It runs as the process exits,
/// on a thread and runtime of its own: the test runtime's are winding down.
extern "C" fn drop_created_database() {
    let Some(name) = CREATED.get() else { return };
    let dropped = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
        runtime.block_on(async {
            let admin = pool_for("postgres");
            let result = admin.query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"), &[]).await;
            admin.end();
            result.map(|_| ())
        })
    })
    .join();
    if !matches!(dropped, Ok(Ok(()))) {
        eprintln!("could not drop the test database {name}; drop it by hand");
    }
}

/// The database the tests run on, set up once; `None` when they are not enabled.
fn database() -> Option<&'static str> {
    static DATABASE: OnceLock<Option<String>> = OnceLock::new();
    DATABASE
        .get_or_init(|| {
            if std::env::var("MAIL_TEST_POSTGRES").as_deref() != Ok("1") {
                return None;
            }
            let name = match std::env::var("MAIL_TEST_POSTGRES_DATABASE") {
                Ok(existing) => existing,
                Err(_) => {
                    let name = format!("mail_spam_rs_{}_{}", std::process::id(), mail_server::js::now_ms());
                    RUNTIME.block_on(async {
                        let admin = pool_for("postgres");
                        admin
                            .query(&format!("CREATE DATABASE {name}"), &[])
                            .await
                            .expect("the database is created");
                        admin.end();
                    });
                    CREATED.set(name.clone()).expect("set once");
                    // SAFETY: registers a plain function with the C runtime.
                    unsafe { atexit(drop_created_database) };
                    name
                }
            };

            // The process-wide pool reads these on first use, which is below.
            std::env::set_var("POSTGRES_HOST", setting("HOST", "127.0.0.1"));
            std::env::set_var("POSTGRES_PORT", setting("PORT", "5432"));
            std::env::set_var("POSTGRES_USER", setting("USER", "postgres"));
            std::env::set_var("POSTGRES_PASSWORD", setting("PASSWORD", "postgres"));
            std::env::set_var("POSTGRES_DATABASE", &name);
            // One test trains a word the column cannot hold; its error log is expected.
            std::env::set_var("NODE_ENV", "test");

            RUNTIME.block_on(async {
                for table in ["users", "spam_allowlist", "spam_training"] {
                    let spec = schema()
                        .tables
                        .iter()
                        .find(|spec| spec.name == table)
                        .expect("the table is in schema.json");
                    pool().query(&spec.create_sql, &[]).await.expect("the table is created");
                }
            });
            Some(name)
        })
        .as_deref()
}

/// Runs a test against the database, or reports it skipped.
fn with_database<F: Future<Output = ()>>(test: impl FnOnce() -> F) {
    if database().is_none() {
        eprintln!("skipped: set MAIL_TEST_POSTGRES=1 to run against PostgreSQL");
        return;
    }
    RUNTIME.block_on(test());
}

/// A new row in `users`; every test works on users of its own.
async fn new_user() -> String {
    let username = format!("spam-test-{}", uuid::Uuid::new_v4());
    let result = pool()
        .query("INSERT INTO users (username) VALUES ($1) RETURNING user_id", &params![username])
        .await
        .expect("the user is created");
    result.rows[0].text("user_id")
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| word.to_string()).collect()
}

/// `(word, spam_count, ham_count)` of every training row, by word.
async fn training_rows(user_id: &str) -> Vec<(String, i64, i64)> {
    let result = pool()
        .query(
            "SELECT word, spam_count, ham_count FROM spam_training WHERE user_id = $1 ORDER BY word COLLATE \"C\"",
            &params![user_id],
        )
        .await
        .expect("the rows are read");
    result.rows.iter().map(|row| (row.text("word"), row.i64("spam_count"), row.i64("ham_count"))).collect()
}

fn row(word: &str, spam_count: i64, ham_count: i64) -> (String, i64, i64) {
    (word.to_string(), spam_count, ham_count)
}

async fn insert_pattern(user_id: &str, pattern: &str) {
    pool()
        .query("INSERT INTO spam_allowlist (user_id, pattern) VALUES ($1, $2)", &params![user_id, pattern])
        .await
        .expect("the pattern is inserted");
}

async fn patterns(user_id: &str) -> Vec<String> {
    get_allowlist_for_user(user_id).await.unwrap().into_iter().map(|entry| entry.pattern).collect()
}

fn created(
    result: AddAllowlistEntryResult,
) -> mail_server::postgres::repositories::spam_allowlists::SpamAllowlistModel {
    match result {
        AddAllowlistEntryResult::Created(entry) => entry,
        other => panic!("expected a created entry, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// spam_training
// ---------------------------------------------------------------------------

#[test]
fn doc_counts_are_zero_before_any_training() {
    with_database(|| async {
        let user = new_user().await;
        assert_eq!(get_classifier_doc_counts(&user).await.unwrap(), DocCounts { spam_docs: 0, ham_docs: 0 });
        assert!(get_all_training_for_user(&user).await.unwrap().is_empty());
        assert!(get_word_counts(&user, &words(&["free"])).await.unwrap().is_empty());
    });
}

#[test]
fn training_counts_each_distinct_word_once_per_document() {
    with_database(|| async {
        let user = new_user().await;
        train_classifier(&user, &words(&["free", "prize", "free", "winner", "prize"]), true).await.unwrap();
        assert_eq!(
            training_rows(&user).await,
            vec![row("__total__", 1, 0), row("free", 1, 0), row("prize", 1, 0), row("winner", 1, 0)]
        );
        assert_eq!(get_classifier_doc_counts(&user).await.unwrap(), DocCounts { spam_docs: 1, ham_docs: 0 });
    });
}

#[test]
fn training_adds_up_across_documents_and_classes() {
    with_database(|| async {
        let user = new_user().await;
        train_classifier(&user, &words(&["free", "prize"]), true).await.unwrap();
        train_classifier(&user, &words(&["free", "meeting"]), true).await.unwrap();
        train_classifier(&user, &words(&["meeting", "notes", "free"]), false).await.unwrap();
        assert_eq!(
            training_rows(&user).await,
            vec![
                row("__total__", 2, 1),
                row("free", 2, 1),
                row("meeting", 1, 1),
                row("notes", 0, 1),
                row("prize", 1, 0)
            ]
        );
        assert_eq!(get_classifier_doc_counts(&user).await.unwrap(), DocCounts { spam_docs: 2, ham_docs: 1 });

        let all = get_all_training_for_user(&user).await.unwrap();
        assert_eq!(all.len(), 5);
        assert!(all.iter().all(|model| model.user_id == user && model.training_id.len() == 36));
    });
}

#[test]
fn training_is_kept_per_user() {
    with_database(|| async {
        let (first, second) = (new_user().await, new_user().await);
        train_classifier(&first, &words(&["shared", "first"]), true).await.unwrap();
        train_classifier(&second, &words(&["shared", "second"]), false).await.unwrap();
        assert_eq!(
            training_rows(&first).await,
            vec![row("__total__", 1, 0), row("first", 1, 0), row("shared", 1, 0)]
        );
        assert_eq!(
            training_rows(&second).await,
            vec![row("__total__", 0, 1), row("second", 0, 1), row("shared", 0, 1)]
        );
    });
}

#[test]
fn training_nothing_writes_nothing() {
    with_database(|| async {
        let user = new_user().await;
        train_classifier(&user, &[], true).await.unwrap();
        assert!(training_rows(&user).await.is_empty());
        // A mail with no tokens trains nothing either, totals row included.
        let email = EmailContext { subject: Some("1234 !!".into()), ..Default::default() };
        train_with_email(&user, &email, false).await.unwrap();
        assert!(training_rows(&user).await.is_empty());
    });
}

#[test]
fn word_counts_hold_only_the_words_asked_for_and_known() {
    with_database(|| async {
        let user = new_user().await;
        train_classifier(&user, &words(&["free", "prize"]), true).await.unwrap();
        train_classifier(&user, &words(&["free", "meeting"]), false).await.unwrap();

        let counts = get_word_counts(&user, &words(&["free", "meeting", "unseen"])).await.unwrap();
        assert_eq!(counts.len(), 2);
        assert_eq!(counts["free"], WordCounts { spam_count: 1, ham_count: 1 });
        assert_eq!(counts["meeting"], WordCounts { spam_count: 0, ham_count: 1 });
        assert!(get_word_counts(&user, &[]).await.unwrap().is_empty());
        // The totals row is a row like any other to this lookup, as in the Bun server.
        let totals = get_word_counts(&user, &words(&["__total__"])).await.unwrap();
        assert_eq!(totals["__total__"], WordCounts { spam_count: 1, ham_count: 1 });
    });
}

#[test]
fn clearing_removes_only_that_users_rows() {
    with_database(|| async {
        let (first, second) = (new_user().await, new_user().await);
        train_classifier(&first, &words(&["free"]), true).await.unwrap();
        train_classifier(&second, &words(&["free"]), true).await.unwrap();
        clear_classifier_data(&first).await.unwrap();
        assert!(training_rows(&first).await.is_empty());
        assert_eq!(get_classifier_doc_counts(&first).await.unwrap(), DocCounts::default());
        assert_eq!(training_rows(&second).await.len(), 2);
    });
}

#[test]
fn a_word_the_column_cannot_hold_fails_the_whole_document() {
    with_database(|| async {
        // The tokenizer has no upper word length and `word` is VARCHAR(100):
        // the Bun server loses such a document's training, and so does this one.
        let user = new_user().await;
        train_classifier(&user, &words(&["before"]), true).await.unwrap();

        let exactly = "a".repeat(100);
        train_classifier(&user, std::slice::from_ref(&exactly), true).await.unwrap();

        let too_long = "b".repeat(101);
        let error = train_classifier(&user, &["kept".to_string(), too_long], true).await.unwrap_err();
        assert_eq!(error.code.as_deref(), Some("22001"), "{error}");
        assert_eq!(
            training_rows(&user).await,
            vec![row("__total__", 2, 0), row(&exactly, 1, 0), row("before", 1, 0)]
        );

        let email = EmailContext {
            text: Some(format!("ordinary words and {}", "c".repeat(150))),
            ..Default::default()
        };
        assert!(train_with_email(&user, &email, true).await.is_err());
        assert_eq!(get_classifier_doc_counts(&user).await.unwrap(), DocCounts { spam_docs: 2, ham_docs: 0 });
    });
}

#[test]
fn a_document_trains_at_most_500_words() {
    with_database(|| async {
        let user = new_user().await;
        let text: Vec<String> = (0..650)
            .map(|index| {
                let letters: String = [index / 26 / 26 % 26, index / 26 % 26, index % 26]
                    .iter()
                    .map(|n| (b'a' + *n as u8) as char)
                    .collect();
                format!("w{letters}")
            })
            .collect();
        let email = EmailContext { text: Some(text.join(" ")), ..Default::default() };
        assert_eq!(extract_tokens(&email).len(), 500);
        train_with_email(&user, &email, true).await.unwrap();

        let rows = training_rows(&user).await;
        assert_eq!(rows.len(), 501);
        assert!(rows.contains(&row("waaa", 1, 0)));
        assert!(rows.contains(&row(&text[499], 1, 0)));
        assert!(!rows.iter().any(|(word, _, _)| word == &text[500]));
        // Every one of them comes back from one lookup.
        assert_eq!(get_word_counts(&user, &extract_tokens(&email)).await.unwrap().len(), 500);
    });
}

#[test]
fn concurrent_training_loses_no_increment() {
    with_database(|| async {
        let user = new_user().await;
        let document = words(&["alpha", "beta", "gamma"]);
        let rounds: Vec<_> =
            (0..24).map(|round| train_classifier(&user, &document, round % 3 != 0)).collect();
        for result in futures_util::future::join_all(rounds).await {
            result.unwrap();
        }
        assert_eq!(
            training_rows(&user).await,
            vec![row("__total__", 16, 8), row("alpha", 16, 8), row("beta", 16, 8), row("gamma", 16, 8)]
        );
    });
}

#[test]
fn a_trained_classifier_scores_mail_from_the_table() {
    with_database(|| async {
        let user = new_user().await;
        let mail = |subject: &str| EmailContext { subject: Some(subject.to_string()), ..Default::default() };
        // Below five documents the classifier stays silent.
        for subject in ["win a free prize", "free prize inside"] {
            train_with_email(&user, &mail(subject), true).await.unwrap();
        }
        for subject in ["team meeting notes", "meeting agenda"] {
            train_with_email(&user, &mail(subject), false).await.unwrap();
        }
        let silent = classify_email(&user, &mail("free prize")).await;
        assert_eq!((silent.score, silent.reason), (0.0, None));

        train_with_email(&user, &mail("claim your free prize"), true).await.unwrap();
        // free and prize: 3 of 3 spam documents, 0 of 2 ham documents.
        // spam 3/5 * 4/5 * 4/5 = 0.384, ham 2/5 * 1/4 * 1/4 = 0.025: 0.384 / 0.409 = 93.9%.
        let spam = classify_email(&user, &mail("free prize")).await;
        assert_eq!(spam.score, 94.0);
        assert_eq!(
            spam.reason.as_deref(),
            Some("Bayesian classifier: 94% spam probability (3 spam / 2 ham documents trained)")
        );
        // meeting: 0 spam, 2 ham; notes: 0 spam, 1 ham.
        // spam 3/5 * 1/5 * 1/5 = 0.024, ham 2/5 * 3/4 * 2/4 = 0.15: 0.024 / 0.174 = 13.8%.
        let ham = classify_email(&user, &mail("meeting notes")).await;
        assert_eq!((ham.score, ham.reason), (14.0, None));
    });
}

// ---------------------------------------------------------------------------
// spam_allowlist
// ---------------------------------------------------------------------------

#[test]
fn add_stores_the_lowercased_pattern_and_lists_newest_first() {
    with_database(|| async {
        let user = new_user().await;
        let first = created(add_allowlist_entry(&user, "Alice@Example.COM").await.unwrap());
        assert_eq!(first.pattern, "alice@example.com");
        assert_eq!(first.user_id, user);
        assert_eq!(first.allowlist_id.len(), 36);
        // ISO 8601 in milliseconds, as the Bun pool returns a timestamptz.
        assert_eq!(first.created_at.len(), 24, "{}", first.created_at);
        assert!(first.created_at.ends_with('Z') && first.created_at.as_bytes()[10] == b'T');

        tokio::time::sleep(Duration::from_millis(5)).await;
        created(add_allowlist_entry(&user, "*@Trusted.Example").await.unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        created(add_allowlist_entry(&user, "ÀLICE@ÉXAMPLE.COM").await.unwrap());

        let entries = get_allowlist_for_user(&user).await.unwrap();
        let listed: Vec<&str> = entries.iter().map(|entry| entry.pattern.as_str()).collect();
        assert_eq!(listed, vec!["àlice@éxample.com", "*@trusted.example", "alice@example.com"]);
        assert_eq!(entries[2], first);
    });
}

#[test]
fn add_reports_a_duplicate_whatever_its_case() {
    with_database(|| async {
        let user = new_user().await;
        created(add_allowlist_entry(&user, "alice@example.com").await.unwrap());
        assert_eq!(
            add_allowlist_entry(&user, "alice@example.com").await.unwrap(),
            AddAllowlistEntryResult::Exists
        );
        assert_eq!(
            add_allowlist_entry(&user, "ALICE@EXAMPLE.COM").await.unwrap(),
            AddAllowlistEntryResult::Exists
        );
        assert_eq!(patterns(&user).await, vec!["alice@example.com"]);
        // Another user's list is another list.
        let other = new_user().await;
        created(add_allowlist_entry(&other, "alice@example.com").await.unwrap());
    });
}

#[test]
fn add_refuses_a_pattern_over_the_byte_ceiling() {
    with_database(|| async {
        let user = new_user().await;
        let at_ceiling =
            format!("{}@example.com", "a".repeat(ALLOWLIST_PATTERN_MAX_BYTES - "@example.com".len()));
        assert_eq!(created(add_allowlist_entry(&user, &at_ceiling).await.unwrap()).pattern, at_ceiling);

        let over = format!("b{at_ceiling}");
        assert_eq!(add_allowlist_entry(&user, &over).await.unwrap(), AddAllowlistEntryResult::TooLong);
        // Bytes, not characters: 200 two-byte letters are 400 bytes.
        let two_byte = format!("{}@example.com", "é".repeat(200));
        assert_eq!(add_allowlist_entry(&user, &two_byte).await.unwrap(), AddAllowlistEntryResult::TooLong);
        assert_eq!(patterns(&user).await, vec![at_ceiling]);
    });
}

#[test]
fn add_stops_at_the_row_ceiling_and_still_reports_an_existing_entry() {
    with_database(|| async {
        let user = new_user().await;
        pool()
            .query(
                "INSERT INTO spam_allowlist (user_id, pattern) SELECT $1::uuid, 'bulk' || n || '@example.com' FROM generate_series(1, $2::int) AS n",
                &params![user.as_str(), ALLOWLIST_COUNT_MAX - 1],
            )
            .await
            .unwrap();

        // The last free row is granted, the next one refused.
        created(add_allowlist_entry(&user, "last@example.com").await.unwrap());
        assert_eq!(
            add_allowlist_entry(&user, "more@example.com").await.unwrap(),
            AddAllowlistEntryResult::AtLimit
        );
        // An entry that exists reports as existing even at the ceiling.
        assert_eq!(
            add_allowlist_entry(&user, "BULK1@example.com").await.unwrap(),
            AddAllowlistEntryResult::Exists
        );
        assert_eq!(
            add_allowlist_entry(&user, "last@example.com").await.unwrap(),
            AddAllowlistEntryResult::Exists
        );
        assert_eq!(get_allowlist_for_user(&user).await.unwrap().len() as i64, ALLOWLIST_COUNT_MAX);

        // Freeing a row frees the ceiling; other users were never affected.
        assert!(remove_allowlist_entry(&user, "bulk2@example.com").await.unwrap());
        created(add_allowlist_entry(&user, "more@example.com").await.unwrap());
        assert_eq!(
            add_allowlist_entry(&user, "again@example.com").await.unwrap(),
            AddAllowlistEntryResult::AtLimit
        );
        created(add_allowlist_entry(&new_user().await, "more@example.com").await.unwrap());
    });
}

#[test]
fn two_adds_of_one_pattern_create_one_entry() {
    with_database(|| async {
        let user = new_user().await;
        let (first, second) = tokio::join!(
            add_allowlist_entry(&user, "race@example.com"),
            add_allowlist_entry(&user, "RACE@example.com")
        );
        let mut statuses = vec![first.unwrap().status(), second.unwrap().status()];
        statuses.sort();
        assert_eq!(statuses, vec!["created", "exists"]);
        assert_eq!(patterns(&user).await, vec!["race@example.com"]);
    });
}

#[test]
fn matches_exact_and_wildcard_entries_case_insensitively() {
    with_database(|| async {
        let user = new_user().await;
        created(add_allowlist_entry(&user, "Friend@Example.com").await.unwrap());
        created(add_allowlist_entry(&user, "*@Trusted.Example").await.unwrap());
        created(add_allowlist_entry(&user, "ÀLICE@ÉXAMPLE.COM").await.unwrap());

        for address in [
            "friend@example.com",
            "FRIEND@EXAMPLE.COM",
            "anyone@trusted.example",
            "Any.One+tag@TRUSTED.example",
            "@trusted.example",
            "àlice@éxample.com",
            "Àlice@Éxample.Com",
        ] {
            assert!(is_allowlisted(&user, address).await.unwrap(), "{address}");
        }
        for address in [
            "other@example.com",
            "friend@example.org",
            "friend@example.com.evil.test",
            "anyone@sub.trusted.example",
            "anyone@trusted.example.evil.test",
            "anyone@nottrusted.example",
            "alice@example.com",
            "*@example.com",
            "",
        ] {
            assert!(!is_allowlisted(&user, address).await.unwrap(), "{address}");
        }
        // The wildcard entry is itself an "address" that matches its own pattern.
        assert!(is_allowlisted(&user, "*@trusted.example").await.unwrap());
    });
}

#[test]
fn matches_only_the_users_own_entries() {
    with_database(|| async {
        let (owner, other) = (new_user().await, new_user().await);
        created(add_allowlist_entry(&owner, "friend@example.com").await.unwrap());
        created(add_allowlist_entry(&owner, "*@trusted.example").await.unwrap());
        assert!(is_allowlisted(&owner, "friend@example.com").await.unwrap());
        assert!(!is_allowlisted(&other, "friend@example.com").await.unwrap());
        assert!(!is_allowlisted(&other, "anyone@trusted.example").await.unwrap());
    });
}

#[test]
fn matches_a_pattern_another_writer_stored_in_upper_case() {
    with_database(|| async {
        // Rows written before patterns were lowercased on the way in.
        let user = new_user().await;
        insert_pattern(&user, "MixedCase@Example.COM").await;
        insert_pattern(&user, "*@UPPER.Example").await;
        assert!(is_allowlisted(&user, "mixedcase@example.com").await.unwrap());
        assert!(is_allowlisted(&user, "someone@upper.example").await.unwrap());
        // Removal is by LOWER(pattern) too; the add path compares the stored text.
        assert_eq!(
            created(add_allowlist_entry(&user, "MixedCase@Example.COM").await.unwrap()).pattern,
            "mixedcase@example.com"
        );
        assert!(remove_allowlist_entry(&user, "MIXEDCASE@example.com").await.unwrap());
        assert_eq!(patterns(&user).await, vec!["*@UPPER.Example"]);
    });
}

#[test]
fn the_second_component_of_the_address_is_its_domain() {
    with_database(|| async {
        // `address.split("@")[1]`, as the Bun server reads it.
        let user = new_user().await;
        insert_pattern(&user, "*@middle.example").await;
        assert!(is_allowlisted(&user, "a@middle.example@last.example").await.unwrap());
        assert!(!is_allowlisted(&user, "a@first.example@middle.example").await.unwrap());

        // An address with no `@` asks for `*@undefined`.
        assert!(!is_allowlisted(&user, "postmaster").await.unwrap());
        insert_pattern(&user, "*@undefined").await;
        assert!(is_allowlisted(&user, "postmaster").await.unwrap());
        assert!(is_allowlisted(&user, "anyone@undefined").await.unwrap());
    });
}

#[test]
fn remove_by_pattern_is_case_insensitive_and_scoped_to_the_user() {
    with_database(|| async {
        let (owner, other) = (new_user().await, new_user().await);
        created(add_allowlist_entry(&owner, "friend@example.com").await.unwrap());
        created(add_allowlist_entry(&other, "friend@example.com").await.unwrap());

        assert!(remove_allowlist_entry(&owner, "FRIEND@Example.COM").await.unwrap());
        assert!(!remove_allowlist_entry(&owner, "friend@example.com").await.unwrap());
        assert!(!remove_allowlist_entry(&owner, "never@added.example").await.unwrap());
        assert!(patterns(&owner).await.is_empty());
        assert_eq!(patterns(&other).await, vec!["friend@example.com"]);
        assert!(!is_allowlisted(&owner, "friend@example.com").await.unwrap());
    });
}

#[test]
fn remove_by_id_is_scoped_to_the_user() {
    with_database(|| async {
        let (owner, other) = (new_user().await, new_user().await);
        let entry = created(add_allowlist_entry(&owner, "friend@example.com").await.unwrap());

        assert!(!remove_allowlist_entry_by_id(&other, &entry.allowlist_id).await.unwrap());
        assert_eq!(patterns(&owner).await, vec!["friend@example.com"]);
        assert!(remove_allowlist_entry_by_id(&owner, &entry.allowlist_id).await.unwrap());
        assert!(!remove_allowlist_entry_by_id(&owner, &entry.allowlist_id).await.unwrap());
        // An id that is no UUID is the database's error, as in the Bun server.
        assert!(remove_allowlist_entry_by_id(&owner, "not-a-uuid").await.is_err());
    });
}

#[test]
fn the_allowlist_exempts_only_a_corroborated_sender() {
    with_database(|| async {
        let user = new_user().await;
        created(add_allowlist_entry(&user, "*@trusted.example").await.unwrap());
        let spam = EmailContext {
            from_address: Some("anyone@trusted.example".into()),
            from_name: Some("Trusted".into()),
            envelope_from_address: Some("bounce@trusted.example".into()),
            subject: Some("FREE MONEY WINNER ACT NOW!!!".into()),
            text: Some("Click http://bit.ly/x to claim your free prize. Act now!!!".into()),
            // A private address: the blocklists have a definite answer without being asked.
            remote_address: Some("10.1.2.3".into()),
            ..Default::default()
        };
        let exempt = check_spam(&user, &spam).await;
        assert_eq!(exempt.reasons, vec!["Sender is allowlisted"]);
        assert_eq!((exempt.score, exempt.is_spam, exempt.flagged_by), (0, false, Some(FlaggedBy::Allowlist)));

        let forged =
            EmailContext { envelope_from_address: Some("bounce@elsewhere.example".into()), ..spam.clone() };
        let scored = check_spam(&user, &forged).await;
        assert_eq!(scored.reasons[0], "Allowlisted sender not confirmed by the envelope sender");
        assert_eq!((scored.score, scored.is_spam, scored.flagged_by), (60, true, Some(FlaggedBy::Rules)));

        // Another recipient has no such entry: the mail is scored on its merits.
        let stranger = check_spam(&new_user().await, &spam).await;
        assert_eq!((stranger.score, stranger.flagged_by), (60, Some(FlaggedBy::Rules)));
    });
}

// ---------------------------------------------------------------------------
// The stages the Bun server ran
// ---------------------------------------------------------------------------

fn golden(name: &str) -> Value {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/spam").join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).expect("the fixture is readable"))
        .expect("the fixture is JSON")
}

fn emails(value: &Value) -> Vec<EmailContext> {
    value
        .as_array()
        .expect("emails")
        .iter()
        .map(|email| serde_json::from_value(email.clone()).unwrap())
        .collect()
}

/// An integral score as the integer the Bun server serializes.
fn number(score: f64) -> Value {
    if score.fract() == 0.0 {
        json!(score as i64)
    } else {
        json!(score)
    }
}

/// What `crosscheck.ts` records after a stage, from this server.
async fn snapshot(user_id: &str, input: &Value) -> Value {
    let training: Vec<Value> =
        training_rows(user_id).await.into_iter().map(|(word, spam, ham)| json!([word, spam, ham])).collect();
    let doc_counts = get_classifier_doc_counts(user_id).await.unwrap();

    let mut classify = Vec::new();
    for email in emails(&input["classify"]) {
        let result = classify_email(user_id, &email).await;
        classify.push(json!({ "score": number(result.score), "reason": result.reason }));
    }
    let mut allowlisted = Vec::new();
    for address in input["allowlisted"].as_array().unwrap() {
        allowlisted.push(is_allowlisted(user_id, address.as_str().unwrap()).await.unwrap());
    }
    let mut spam = Vec::new();
    for email in emails(&input["checkSpam"]) {
        spam.push(serde_json::to_value(check_spam(user_id, &email).await).unwrap());
    }

    json!({
        "training": training,
        "docCounts": { "spamDocs": doc_counts.spam_docs, "hamDocs": doc_counts.ham_docs },
        "classify": classify,
        "allowlist": patterns(user_id).await,
        "allowlisted": allowlisted,
        "checkSpam": spam,
    })
}

/// Runs one stage of `crosscheck_input.json` and returns what `crosscheck.ts` records for it.
async fn run_stage(user_id: &str, input: &Value, name: &str) -> Value {
    let stage = input["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["name"] == name)
        .unwrap_or_else(|| panic!("no stage {name}"));

    for document in stage["train"].as_array().unwrap() {
        let email: EmailContext = serde_json::from_value(document["email"].clone()).unwrap();
        train_with_email(user_id, &email, document["isSpam"].as_bool().unwrap()).await.unwrap();
    }
    let mut removed = Vec::new();
    for pattern in stage["allowlistRemove"].as_array().unwrap() {
        removed.push(remove_allowlist_entry(user_id, pattern.as_str().unwrap()).await.unwrap());
    }
    let mut added = Vec::new();
    for pattern in stage["allowlistAdd"].as_array().unwrap() {
        added.push(add_allowlist_entry(user_id, pattern.as_str().unwrap()).await.unwrap().status());
        // Entries list newest first; keep their timestamps apart.
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    json!({ "allowlistRemove": removed, "allowlistAdd": added, "snapshot": snapshot(user_id, input).await })
}

#[test]
fn the_stages_leave_what_the_bun_server_left() {
    with_database(|| async {
        let input = golden("crosscheck_input.json");
        let bun = golden("crosscheck.json");
        let user = new_user().await;
        for stage in ["a", "b"] {
            let rust = run_stage(&user, &input, stage).await;
            for part in ["allowlistRemove", "allowlistAdd"] {
                assert_eq!(rust[part], bun[stage][part], "stage {stage}: {part}");
            }
            for part in ["training", "docCounts", "classify", "allowlist", "allowlisted", "checkSpam"] {
                assert_eq!(rust["snapshot"][part], bun[stage]["snapshot"][part], "stage {stage}: {part}");
            }
        }
        // The corpus is not trivial: both verdicts, an exemption, a refusal.
        let last = &bun["b"]["snapshot"];
        assert!(last["training"].as_array().unwrap().len() > 90);
        assert!(last["classify"].as_array().unwrap().iter().any(|result| result["reason"].is_string()));
        assert!(last["classify"].as_array().unwrap().iter().any(|result| result["reason"].is_null()));
        assert!(last["checkSpam"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["flaggedBy"] == "allowlist"));
    });
}

/// Both servers on one database: `crosscheck.ts --create --stages a` wrote
/// stage a for the fixed user of `crosscheck_input.json`, and this continues
/// with stage b. Run by hand, naming that database:
///
/// ```bash
/// MAIL_TEST_POSTGRES=1 MAIL_TEST_POSTGRES_DATABASE=mail_spam_shared \
///   cargo test --manifest-path src/server-rs/Cargo.toml --test spam_database continues -- --ignored
/// ```
#[test]
#[ignore = "needs a database the Bun server wrote stage a into"]
fn continues_on_a_database_the_bun_server_wrote() {
    with_database(|| async {
        let input = golden("crosscheck_input.json");
        let bun = golden("crosscheck.json");
        let user = input["userId"].as_str().unwrap();

        // What the Bun server wrote reads the same through this server.
        assert_eq!(snapshot(user, &input).await, bun["a"]["snapshot"], "the Bun server's stage a");

        let rust = run_stage(user, &input, "b").await;
        assert_eq!(rust, bun["b"], "stage b on top of the Bun server's stage a");
    });
}

/// The other direction: this server writes stage a for the fixed user into an
/// empty database, and `crosscheck.ts --stages b --check` continues on it.
///
/// ```bash
/// createdb mail_spam_shared
/// MAIL_TEST_POSTGRES=1 MAIL_TEST_POSTGRES_DATABASE=mail_spam_shared \
///   cargo test --manifest-path src/server-rs/Cargo.toml --test spam_database starts -- --ignored
/// bun src/server-rs/tests/golden/spam/crosscheck.ts --database mail_spam_shared --stages b --check --drop
/// ```
#[test]
#[ignore = "writes the fixed cross-check user into the named database"]
fn starts_a_database_for_the_bun_server() {
    with_database(|| async {
        let input = golden("crosscheck_input.json");
        let bun = golden("crosscheck.json");
        let user = input["userId"].as_str().unwrap();
        pool()
            .query(
                "INSERT INTO users (user_id, username) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &params![user, input["username"].as_str().unwrap()],
            )
            .await
            .unwrap();
        assert_eq!(run_stage(user, &input, "a").await, bun["a"], "stage a");
    });
}
