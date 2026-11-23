//! The sign-up token flow and the credential-generation lookup against a real
//! PostgreSQL: a sign-up still in progress (a row with no password) keeps no
//! username of its own, so completing it takes the name the person chose, and
//! a deleted account reads as revoked.
//!
//! These run when `MAIL_TEST_POSTGRES=1` and report themselves skipped
//! otherwise:
//!
//! ```bash
//! MAIL_TEST_POSTGRES=1 cargo test --manifest-path src/server-rs/Cargo.toml --test users_database
//! ```
//!
//! The server is `MAIL_TEST_POSTGRES_HOST`, `_PORT`, `_USER`, `_PASSWORD`
//! (default `postgres:postgres@127.0.0.1:5432`). A database named
//! `mail_users_rs_<pid>_<time>` is created with the tables from `schema.json`
//! and dropped when the test process exits.

use std::future::Future;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use tokio::runtime::Runtime;

use mail_server::imap::credential_version::get_credential_password_version;
use mail_server::params;
use mail_server::postgres::schema::schema;
use mail_server::postgres::{pool, Pool, PoolOptions};
use mail_server::users::{create_token, set_user_info};

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

/// Drops the database this process created, on a thread and runtime of its
/// own: the test runtime's are winding down.
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
            let name = format!("mail_users_rs_{}_{}", std::process::id(), mail_server::js::now_ms());
            RUNTIME.block_on(async {
                let admin = pool_for("postgres");
                admin.query(&format!("CREATE DATABASE {name}"), &[]).await.expect("the database is created");
                admin.end();
            });
            CREATED.set(name.clone()).expect("set once");
            // SAFETY: registers a plain function with the C runtime.
            unsafe { atexit(drop_created_database) };

            // The process-wide pool reads these on first use, which is below.
            std::env::set_var("POSTGRES_HOST", setting("HOST", "127.0.0.1"));
            std::env::set_var("POSTGRES_PORT", setting("PORT", "5432"));
            std::env::set_var("POSTGRES_USER", setting("USER", "postgres"));
            std::env::set_var("POSTGRES_PASSWORD", setting("PASSWORD", "postgres"));
            std::env::set_var("POSTGRES_DATABASE", &name);
            std::env::set_var("NODE_ENV", "test");

            RUNTIME.block_on(async {
                for table in ["users", "sessions"] {
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

/// A name no other test uses.
fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

/// `(username, password IS NULL, token)` of the row with this address.
async fn row_by_email(email: &str) -> Option<(String, bool, Option<String>)> {
    let result = pool()
        .query("SELECT username, password IS NULL AS pending, token FROM users WHERE email = $1", &params![email])
        .await
        .expect("the row is read");
    result.rows.first().map(|row| (row.text("username"), row.bool("pending"), row.opt_text("token")))
}

/// A sign-up as the token route leaves it, and its token.
async fn sign_up(email: &str) -> (String, String) {
    let created = create_token(email).await.expect("a token is issued");
    (created.id, created.token)
}

/// A registered account: a row with a password, and a reset token on it.
async fn registered(username: &str, email: &str, token: &str) {
    pool()
        .query(
            "INSERT INTO users (username, password, email, token) VALUES ($1, 'stored-hash', $2, $3)",
            &params![username, email, token],
        )
        .await
        .expect("the account is created");
}

#[test]
fn a_new_address_gets_a_placeholder_row_and_a_link_without_a_username() {
    with_database(|| async {
        let email = format!("{}@elsewhere.example", unique("first"));
        let created = create_token(&email).await.unwrap();
        assert_eq!(created.username, None);
        let (username, pending, _) = row_by_email(&email).await.unwrap();
        assert_eq!(username, format!("user_{}", &created.id[..8]));
        assert!(pending);
    });
}

#[test]
fn asking_again_for_a_sign_up_in_progress_names_no_username_either() {
    with_database(|| async {
        let email = format!("{}@elsewhere.example", unique("again"));
        let (id, first) = sign_up(&email).await;
        let again = create_token(&email).await.unwrap();
        assert_eq!(again.id, id);
        assert_ne!(again.token, first);
        // The placeholder is nobody's chosen name: the link leaves it out.
        assert_eq!(again.username, None);
    });
}

#[test]
fn a_registered_address_is_named_in_its_reset_link() {
    with_database(|| async {
        let name = unique("reset");
        let email = format!("{name}@elsewhere.example");
        registered(&name, &email, "old-token").await;
        assert_eq!(create_token(&email).await.unwrap().username, Some(name));
    });
}

#[test]
fn completing_a_sign_up_takes_the_chosen_username() {
    with_database(|| async {
        let email = format!("{}@elsewhere.example", unique("chooser"));
        let (id, token) = sign_up(&email).await;
        let chosen = unique("chosen");

        let signed = set_user_info(&email, &chosen, "a-first-password", Some(&token)).await.unwrap();

        assert_eq!(signed.id, id);
        assert_eq!(signed.username, chosen);
        assert_eq!(signed.password_version, Some(2));
        let (username, pending, token) = row_by_email(&email).await.unwrap();
        assert_eq!((username, pending, token), (chosen, false, None));
    });
}

#[test]
fn a_sign_up_may_keep_its_placeholder_name_if_that_is_the_one_asked_for() {
    with_database(|| async {
        let email = format!("{}@elsewhere.example", unique("keeper"));
        let (id, token) = sign_up(&email).await;
        let placeholder = format!("user_{}", &id[..8]);
        let signed = set_user_info(&email, &placeholder, "a-first-password", Some(&token)).await.unwrap();
        assert_eq!(signed.username, placeholder);
    });
}

#[test]
fn a_name_another_account_has_is_refused_and_the_sign_up_stays_pending() {
    with_database(|| async {
        let taken = unique("taken");
        registered(&taken, &format!("{taken}@elsewhere.example"), "unrelated").await;
        let email = format!("{}@elsewhere.example", unique("late"));
        let (id, token) = sign_up(&email).await;

        let refused = set_user_info(&email, &taken, "a-first-password", Some(&token)).await;

        assert!(refused.unwrap_err().to_string().contains("username already exists"));
        let row = row_by_email(&email).await.unwrap();
        assert_eq!(row, (format!("user_{}", &id[..8]), true, Some(token)));
    });
}

#[test]
fn a_reserved_name_is_refused_even_where_no_row_holds_it() {
    with_database(|| async {
        // No `admin` or `admin-ro` row exists in this database.
        let email = format!("{}@elsewhere.example", unique("grabber"));
        let (_, token) = sign_up(&email).await;
        for name in ["admin", "admin-ro"] {
            let refused = set_user_info(&email, name, "a-first-password", Some(&token)).await;
            assert!(refused.unwrap_err().to_string().contains("username is reserved"), "{name}");
        }
        assert!(row_by_email(&email).await.unwrap().1);
    });
}

#[test]
fn a_password_reset_keeps_the_stored_username() {
    with_database(|| async {
        let name = unique("kept");
        let email = format!("{name}@elsewhere.example");
        registered(&name, &email, "reset-token").await;
        let signed = set_user_info(&email, "a-new-name", "a-new-password", Some("reset-token")).await.unwrap();
        assert_eq!(signed.username, name);
        assert_eq!(row_by_email(&email).await.unwrap().0, name);
    });
}

#[test]
fn the_credential_generation_of_a_deleted_or_soft_deleted_account_is_none() {
    with_database(|| async {
        let name = unique("gone");
        registered(&name, &format!("{name}@elsewhere.example"), "t").await;
        pool().query("UPDATE users SET password_version = 3 WHERE username = $1", &params![&name]).await.unwrap();
        assert_eq!(get_credential_password_version(&name).await.unwrap(), Some(3));

        pool().query("UPDATE users SET is_deleted = TRUE WHERE username = $1", &params![&name]).await.unwrap();
        assert_eq!(get_credential_password_version(&name).await.unwrap(), None);

        pool().query("DELETE FROM users WHERE username = $1", &params![&name]).await.unwrap();
        assert_eq!(get_credential_password_version(&name).await.unwrap(), None);
    });
}
