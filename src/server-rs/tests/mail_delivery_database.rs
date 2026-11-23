//! Storing mail against a real PostgreSQL: a parsed message handed to
//! `save_mail_handler` with its envelope, the way the SMTP server delivers,
//! and the sender's copy written by `save_mail`. What each leaves behind is
//! what `e2e/smtp/delivery.e2e.ts` pins for the Bun server.
//!
//! Runs when `MAIL_TEST_POSTGRES=1` and reports itself skipped otherwise:
//!
//! ```bash
//! MAIL_TEST_POSTGRES=1 cargo test --manifest-path src/server-rs/Cargo.toml --test mail_delivery_database
//! ```
//!
//! The server is `MAIL_TEST_POSTGRES_HOST`, `_PORT`, `_USER`, `_PASSWORD`
//! (default `postgres:postgres@127.0.0.1:5432`). The database
//! `mail_delivery_rs_<pid>` is created from `schema.json` and dropped at the end.

use std::panic::{resume_unwind, AssertUnwindSafe};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{json, Value};

use mail_server::imap::util::{account_to_box, account_to_sent_box};
use mail_server::mails::receive::{
    incoming_mail_from_parsed, save_mail, save_mail_handler, IncomingMail, IncomingMailAddressValue, Mail,
    MailAddress, MailAddressValue, MailUid, SaveMailHandlerOptions,
};
use mail_server::mime::parser::{parse_message, ParseOptions};
use mail_server::params;
use mail_server::postgres::schema::schema;
use mail_server::postgres::{pool, Pool, PoolOptions, Row};

fn setting(name: &str, default: &str) -> String {
    std::env::var(format!("MAIL_TEST_POSTGRES_{name}")).unwrap_or_else(|_| default.to_string())
}

fn admin_pool() -> Pool {
    Pool::new(PoolOptions {
        host: setting("HOST", "127.0.0.1"),
        port: setting("PORT", "5432").parse().expect("a port"),
        user: setting("USER", "postgres"),
        password: Some(setting("PASSWORD", "postgres")),
        database: "postgres".to_string(),
        max: 1,
        connection_timeout: Duration::from_secs(5),
        statement_timeout: Duration::from_secs(30),
        query_timeout: Duration::from_secs(30),
    })
}

async fn admin(sql: &str) {
    let pool = admin_pool();
    pool.query(sql, &[]).await.expect("the admin statement runs");
    pool.end();
}

async fn rows(sql: &str, values: Vec<mail_server::postgres::Param>) -> Vec<Row> {
    pool().query(sql, &values).await.expect("the query runs").rows
}

async fn new_user(username: &str) -> String {
    rows("INSERT INTO users (username, password) VALUES ($1, 'not-a-digest') RETURNING user_id", params![username]).await[0].text("user_id")
}

async fn parse(message: &str, attachment_dir: &std::path::Path) -> IncomingMail {
    let options = ParseOptions { attachment_dir: attachment_dir.to_path_buf() };
    let parsed = parse_message(message.as_bytes(), &options).await.expect("the message parses");
    incoming_mail_from_parsed(&parsed)
}

fn envelope(mail: &mut IncomingMail, from: Option<&str>, to: &[&str]) {
    mail.envelope_from = from.map(|address| vec![IncomingMailAddressValue::address(address)]);
    mail.envelope_to = Some(to.iter().map(|address| IncomingMailAddressValue::address(*address)).collect());
}

async fn mails_of(user_id: &str) -> Vec<Row> {
    rows("SELECT * FROM mails WHERE user_id = $1 ORDER BY date, message_id", params![user_id]).await
}

async fn mailboxes_of(user_id: &str, mail_id: &str) -> Vec<String> {
    rows(
        "SELECT mailbox FROM mail_mailbox_uid WHERE user_id = $1 AND mail_id = $2 ORDER BY mailbox",
        params![user_id, mail_id],
    )
    .await
    .iter()
    .map(|row| row.text("mailbox"))
    .collect()
}

async fn scenario(attachment_dir: &std::path::Path) {
    let bob = new_user("bob").await;
    let alice = new_user("alice").await;
    let options = SaveMailHandlerOptions { remote_address: Some("127.0.0.1".into()) };

    // A text-only message to two local users, a foreign address and a
    // local address nobody owns.
    let message = "From: Sender <Sender@Remote.Example>\r\nTo: Bob <BOB@bob.mail.test>\r\nSubject: Hello\r\nMessage-ID: <m1@remote.example>\r\nDate: Tue, 03 Feb 2026 04:05:06 +0000\r\n\r\nHi there\r\n";
    let mut mail = parse(message, attachment_dir).await;
    let recipients = ["BOB@bob.mail.test", "alice@alice.mail.test", "x@remote.example", "nobody@ghost.mail.test"];
    envelope(&mut mail, Some("sender@remote.example"), &recipients);
    assert_eq!(save_mail_handler(&mail, &options).await.expect("the delivery is stored"), 2);

    let stored = mails_of(&bob).await;
    assert_eq!(stored.len(), 1);
    let row = &stored[0];
    assert_eq!(row.text("message_id"), "<m1@remote.example>");
    assert_eq!(row.text("subject"), "Hello");
    assert_eq!(row.text("text"), "Hi there\n");
    // A text-only message keeps its text as the HTML.
    assert_eq!(row.text("html"), "Hi there\n");
    assert_eq!(row.text("date"), "2026-02-03T04:05:06.000Z");
    assert!(!row.bool("read") && !row.bool("sent") && !row.bool("is_spam"));
    assert_eq!(row.get("attachments"), &json!([]));
    assert_eq!(row.get("spam_reasons"), &json!([]));
    assert_eq!(row.get("from_address"), &json!([{ "name": "Sender", "address": "sender@remote.example" }]));
    // The text is the parser's rendering of the header.
    assert_eq!(row.text("from_text"), "\"Sender\" <Sender@Remote.Example>");
    assert_eq!(row.get("to_address"), &json!([{ "name": "Bob", "address": "bob@bob.mail.test" }]));
    assert_eq!(row.get("envelope_from"), &json!([{ "address": "sender@remote.example" }]));
    // The whole envelope, foreign and unknown recipients included, lower-cased.
    let expected_envelope: Vec<Value> =
        recipients.iter().map(|address| json!({ "address": address.to_lowercase() })).collect();
    assert_eq!(row.get("envelope_to"), &Value::Array(expected_envelope));
    assert_eq!(row.get("reference_ids"), &Value::Null);
    let bob_mail = row.text("mail_id");
    assert_eq!(mailboxes_of(&bob, &bob_mail).await, vec!["INBOX".to_string(), account_to_box("bob@bob.mail.test")]);

    // Every copy is filed under the first envelope recipient's account.
    let alice_rows = mails_of(&alice).await;
    assert_eq!(alice_rows.len(), 1);
    let alice_mail = alice_rows[0].text("mail_id");
    assert_eq!(mailboxes_of(&alice, &alice_mail).await, vec!["INBOX".to_string(), account_to_box("bob@bob.mail.test")]);

    // The same Message-ID again keeps the first row and merges the envelope.
    let mut again = parse(message, attachment_dir).await;
    envelope(&mut again, Some("sender@remote.example"), &["other@bob.mail.test"]);
    assert_eq!(save_mail_handler(&again, &options).await.expect("the repeat is stored"), 1);
    let stored = mails_of(&bob).await;
    assert_eq!(stored.len(), 1);
    let merged: Vec<&str> =
        stored[0].get("envelope_to").as_array().unwrap().iter().map(|e| e["address"].as_str().unwrap()).collect();
    assert!(merged.contains(&"other@bob.mail.test") && merged.contains(&"bob@bob.mail.test"));

    // No Message-ID, no Date, an attachment, a reply.
    let before = mail_server::js::now_ms();
    let message = "From: a@remote.example\r\nTo: bob@bob.mail.test\r\nSubject: Re: Hello\r\nIn-Reply-To: <m1@remote.example>\r\nReferences: <root@remote.example> <m1@remote.example>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=XX\r\n\r\n--XX\r\nContent-Type: text/html\r\n\r\n<p>See <b>attached</b></p>\r\n--XX\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"notes.txt\"\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--XX--\r\n";
    let mut mail = parse(message, attachment_dir).await;
    envelope(&mut mail, None, &["bob@bob.mail.test"]);
    assert_eq!(save_mail_handler(&mail, &options).await.expect("the delivery is stored"), 1);
    let stored = mails_of(&bob).await;
    let row = stored.iter().find(|row| row.text("subject") == "Re: Hello").expect("the reply is stored");
    let message_id = row.text("message_id");
    assert_eq!(message_id.len(), 8);
    assert!(message_id.bytes().all(|b| b.is_ascii_hexdigit()));
    let dated = mail_server::js::date_parse(&row.text("date")).unwrap();
    assert!(dated >= before - 1000 && dated <= mail_server::js::now_ms() + 1000);
    assert_eq!(row.get("envelope_from"), &Value::Null);
    assert_eq!(row.text("in_reply_to"), "<m1@remote.example>");
    assert_eq!(row.get("reference_ids"), &json!(["<root@remote.example>", "<m1@remote.example>"]));
    assert_eq!(row.text("html"), "<p>See <b>attached</b></p>");
    let attachments = row.get("attachments").as_array().expect("a list").clone();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["filename"], json!("notes.txt"));
    assert_eq!(attachments[0]["contentType"], json!("text/plain"));
    assert_eq!(attachments[0]["size"], json!(5));
    let id = attachments[0]["content"]["data"].as_str().unwrap();
    assert_eq!(std::fs::read(format!("./attachments/{id}")).expect("the file is stored"), b"hello");

    // No local recipient, or none with an account: nothing is stored.
    let mut nowhere = parse("Subject: x\r\n\r\nx\r\n", attachment_dir).await;
    envelope(&mut nowhere, Some("a@remote.example"), &["x@remote.example"]);
    assert_eq!(save_mail_handler(&nowhere, &options).await.unwrap(), 0);
    envelope(&mut nowhere, Some("a@remote.example"), &["x@ghost.mail.test"]);
    assert_eq!(save_mail_handler(&nowhere, &options).await.unwrap(), 0);

    // The sender's copy of a sent mail lands in its Sent account.
    let sender = MailAddressValue::new("box@bob.mail.test", Some(json!("Box")));
    let sent = Mail {
        message_id: "<s1@bob.mail.test>".into(),
        subject: "Out".into(),
        html: "<p>out</p>".into(),
        text: "out".into(),
        date: mail_server::js::now_iso(),
        from: Some(MailAddress { value: vec![sender.clone()], text: "Box <box@bob.mail.test>".into() }),
        to: Some(MailAddress { value: vec![MailAddressValue::new("x@remote.example", None)], text: "x@remote.example".into() }),
        envelope_from: Some(vec![sender]),
        envelope_to: vec![MailAddressValue::new("x@remote.example", None)],
        read: true,
        sent: true,
        uid: MailUid { domain: 1, account: 1 },
        ..Default::default()
    };
    let saved = save_mail(&sent, Some(&bob), None).await.expect("the copy is stored").expect("a row");
    assert_eq!(mailboxes_of(&bob, &saved.id).await, vec![account_to_sent_box("box@bob.mail.test")]);
    let row = rows("SELECT * FROM mails WHERE mail_id = $1", params![saved.id.as_str()]).await.remove(0);
    assert!(row.bool("sent") && row.bool("read"));
    assert_eq!(row.get("spam_reasons"), &Value::Null);
    assert_eq!(row.get("from_address"), &json!([{ "name": "Box", "address": "box@bob.mail.test" }]));

    // Without a user nothing is written.
    assert!(save_mail(&sent, None, None).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliveries_and_sent_copies_are_stored_as_the_bun_server_stores_them() {
    if std::env::var("MAIL_TEST_POSTGRES").as_deref() != Ok("1") {
        eprintln!("skipped: set MAIL_TEST_POSTGRES=1 to run against PostgreSQL");
        return;
    }
    let database = format!("mail_delivery_rs_{}", std::process::id());
    admin(&format!("DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)")).await;
    admin(&format!("CREATE DATABASE \"{database}\"")).await;
    // The pool and the domain helpers read these on first use.
    std::env::set_var("POSTGRES_HOST", setting("HOST", "127.0.0.1"));
    std::env::set_var("POSTGRES_PORT", setting("PORT", "5432"));
    std::env::set_var("POSTGRES_USER", setting("USER", "postgres"));
    std::env::set_var("POSTGRES_PASSWORD", setting("PASSWORD", "postgres"));
    std::env::set_var("POSTGRES_DATABASE", &database);
    std::env::set_var("EMAIL_DOMAIN", "mail.test");
    std::env::set_var("NODE_ENV", "test");

    // Attachments are stored under `./attachments`.
    let work_directory = tempfile::tempdir().expect("a temporary directory");
    std::env::set_current_dir(work_directory.path()).expect("the working directory");
    let attachment_dir = work_directory.path().join("incoming");

    let outcome = AssertUnwindSafe(async {
        let schema = schema();
        for table in &schema.tables {
            pool().query(&table.create_sql, &[]).await.expect("the table is created");
        }
        for sql in &schema.search_vector_ddl {
            pool().query(sql, &[]).await.expect("the search trigger is installed");
        }
        for index in &schema.indexes {
            pool().query(&index.sql, &[]).await.expect("the index is created");
        }
        scenario(&attachment_dir).await
    })
    .catch_unwind()
    .await;

    pool().end();
    admin(&format!("DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)")).await;
    if let Err(panic) = outcome {
        resume_unwind(panic);
    }
}
