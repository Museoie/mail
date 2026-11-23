//! The IMAP mailbox and message commands against a real PostgreSQL, spoken
//! over the wire to `serve_connection` (the store-backed cases of
//! mailbox-existence, mailbox-exists-transient, select-failed-read,
//! uidnext-wire, inbox-case-insensitive, mailbox-count-limit,
//! mailbox-name-limit and message-ops .test.ts).
//!
//! The Bun suites reach these paths with fake stores and fake pools. Here the
//! store is the real one, and a backend fault is a table renamed away for the
//! one command that must meet it.
//!
//! Runs when `MAIL_TEST_POSTGRES=1` and reports itself skipped otherwise:
//!
//! ```bash
//! MAIL_TEST_POSTGRES=1 cargo test --manifest-path src/server-rs/Cargo.toml --test imap_session_database
//! ```
//!
//! The server is `MAIL_TEST_POSTGRES_HOST`, `_PORT`, `_USER`, `_PASSWORD`
//! (default `postgres:postgres@127.0.0.1:5432`). The database
//! `imap_session_rs_<pid>` is created from `schema.json` and dropped at the end.

use std::panic::{resume_unwind, AssertUnwindSafe};
use std::time::Duration;

use futures_util::FutureExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use mail_server::imap::handler::serve_connection;
use mail_server::params;
use mail_server::postgres::repositories::mailboxes::MAILBOX_COUNT_MAX;
use mail_server::postgres::schema::schema;
use mail_server::postgres::{pool, Pool, PoolOptions, Row};
use mail_server::proxy_protocol::Peer;
use mail_server::verify_password::hash_password;

const PASSWORD: &str = "correct horse";

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

async fn execute(sql: &str) {
    pool().query(sql, &[]).await.expect("the statement runs");
}

async fn new_user(username: &str) -> String {
    let digest = hash_password(PASSWORD).await;
    let email = format!("{username}@mail.test");
    rows(
        "INSERT INTO users (username, password, email) VALUES ($1, $2, $3) RETURNING user_id",
        params![username, digest.as_str(), email.as_str()],
    )
    .await[0]
    .text("user_id")
}

// ---------------------------------------------------------------------------
// A client on an in-memory connection
// ---------------------------------------------------------------------------

struct Client {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    next_tag: u32,
}

impl Client {
    async fn connect() -> Client {
        let (server, client) = tokio::io::duplex(1 << 20);
        tokio::spawn(serve_connection(Box::new(server), Peer { address: "198.51.100.9".into(), port: 50_000 }, false));
        let (read, writer) = tokio::io::split(client);
        let mut client = Client { reader: BufReader::new(read), writer, next_tag: 0 };
        let greeting = client.line().await;
        assert!(greeting.starts_with("* OK [CAPABILITY "), "{greeting}");
        client
    }

    async fn signed_in(username: &str) -> Client {
        let mut client = Client::connect().await;
        let answer = client.run(&format!("LOGIN {username} \"{PASSWORD}\"")).await;
        assert!(answer.last().unwrap().contains(" OK "), "{answer:?}");
        client
    }

    /// One response line, a literal it announces read in with it.
    async fn line(&mut self) -> String {
        let mut line = Vec::new();
        loop {
            let mut part = Vec::new();
            let read = tokio::time::timeout(Duration::from_secs(10), self.reader.read_until(b'\n', &mut part))
                .await
                .expect("the server answers")
                .unwrap();
            assert!(read > 0, "the connection closed after {:?}", String::from_utf8_lossy(&line));
            line.extend_from_slice(&part);
            let text = String::from_utf8_lossy(&part).into_owned();
            let Some(length) = text
                .strip_suffix("}\r\n")
                .and_then(|head| head.rsplit_once('{'))
                .and_then(|(_, digits)| digits.parse::<usize>().ok())
            else {
                break;
            };
            let mut literal = vec![0u8; length];
            self.reader.read_exact(&mut literal).await.unwrap();
            line.extend_from_slice(&literal);
        }
        String::from_utf8(line).expect("UTF-8 on the wire")
    }

    /// Sends `command` under a fresh tag and returns every line up to and
    /// including the tagged one.
    async fn run(&mut self, command: &str) -> Vec<String> {
        self.run_raw(command.as_bytes()).await
    }

    async fn run_raw(&mut self, command: &[u8]) -> Vec<String> {
        self.next_tag += 1;
        let tag = format!("T{}", self.next_tag);
        let mut wire = format!("{tag} ").into_bytes();
        wire.extend_from_slice(command);
        wire.extend_from_slice(b"\r\n");
        self.writer.write_all(&wire).await.unwrap();
        let mut lines = Vec::new();
        loop {
            let line = self.line().await;
            let done = line.starts_with(&format!("{tag} "));
            lines.push(line);
            if done {
                return lines;
            }
        }
    }

    /// The tagged line of `command`, without its tag.
    async fn answer(&mut self, command: &str) -> String {
        let lines = self.run(command).await;
        let last = lines.last().unwrap();
        last.split_once(' ').unwrap().1.to_string()
    }

    /// As `run`, with the tag taken off the tagged line.
    async fn exchange(&mut self, command: &str) -> Vec<String> {
        let mut lines = self.run(command).await;
        let tagged = lines.pop().unwrap();
        lines.push(tagged.split_once(' ').unwrap().1.to_string());
        lines
    }

    /// APPENDs `message` with a non-synchronizing literal (LITERAL+).
    async fn append(&mut self, mailbox: &str, flags: &str, message: &str) -> Vec<String> {
        let mut command = format!("APPEND {mailbox} {flags} {{{}+}}\r\n", message.len()).into_bytes();
        command.extend_from_slice(message.as_bytes());
        self.run_raw(&command).await
    }
}

fn message(subject: &str) -> String {
    format!(
        "From: Alice <alice@mail.test>\r\nTo: Bob <bob@mail.test>\r\nSubject: {subject}\r\nMessage-ID: <{subject}@mail.test>\r\nDate: Mon, 21 Sep 2026 10:00:00 +0000\r\n\r\nbody of {subject}\r\n"
    )
}

/// The first number in `[CODE n …]` among `lines`.
fn code_number(lines: &[String], code: &str) -> i64 {
    let marker = format!("[{code} ");
    let line = lines.iter().find(|line| line.contains(&marker)).unwrap_or_else(|| panic!("no {code} in {lines:?}"));
    let rest = &line[line.find(&marker).unwrap() + marker.len()..];
    rest.split([' ', ']']).next().unwrap().parse().unwrap()
}

/// The value of `item` in a `* STATUS` line among `lines`.
fn status_item(lines: &[String], item: &str) -> i64 {
    let line = lines.iter().find(|line| line.starts_with("* STATUS ")).expect("a STATUS line");
    let marker = format!("{item} ");
    let rest = &line[line.find(&marker).unwrap() + marker.len()..];
    rest.split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

/// Runs `body` with `table` renamed away, so every query on it fails.
async fn with_table_gone<F: std::future::Future<Output = T>, T>(table: &str, body: F) -> T {
    execute(&format!("ALTER TABLE {table} RENAME TO {table}_away")).await;
    let outcome = AssertUnwindSafe(body).catch_unwind().await;
    execute(&format!("ALTER TABLE {table}_away RENAME TO {table}")).await;
    outcome.unwrap_or_else(|panic| resume_unwind(panic))
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// mailbox-existence.test.ts
async fn mailbox_existence() {
    let mut client = Client::signed_in("existence").await;
    assert_eq!(client.exchange("SELECT ThisMailboxDoesNotExist").await, ["NO Mailbox does not exist\r\n"]);
    assert_eq!(client.exchange("EXAMINE NopeNotHere").await, ["NO Mailbox does not exist\r\n"]);
    assert_eq!(client.exchange("SELECT INBOX/accounts/zzz-no-such-account").await, ["NO Mailbox does not exist\r\n"]);
    assert_eq!(client.exchange("STATUS GarbageBox (MESSAGES UIDNEXT UNSEEN)").await, ["NO Mailbox does not exist\r\n"]);

    assert_eq!(client.answer("CREATE Archive").await, "OK CREATE completed\r\n");
    assert_eq!(
        client.exchange("STATUS Archive (MESSAGES UIDNEXT UNSEEN)").await,
        ["* STATUS \"Archive\" (MESSAGES 0 UIDNEXT 1 UNSEEN 0)\r\n", "OK STATUS completed\r\n"]
    );
    // A brand-new account still has INBOX.
    assert_eq!(client.answer("STATUS INBOX (MESSAGES)").await, "OK STATUS completed\r\n");

    // A stored name carrying quoted-specials comes back as a quoted string.
    assert_eq!(client.answer("CREATE \"a\\\"b\\\\\"").await, "OK CREATE completed\r\n");
    let lines = client.run("STATUS \"a\\\"b\\\\\" (MESSAGES)").await;
    assert_eq!(lines[0], "* STATUS \"a\\\"b\\\\\" (MESSAGES 0)\r\n");
}

/// inbox-case-insensitive.test.ts: SELECT and STATUS canonicalize INBOX.
async fn inbox_case() {
    let mut client = Client::signed_in("casing").await;
    assert!(client.append("INBOX", "()", &message("casing")).await.last().unwrap().contains(" OK "));
    for name in ["inbox", "Inbox", "INBOX"] {
        let lines = client.run(&format!("SELECT {name}")).await;
        assert!(lines.last().unwrap().ends_with("OK [READ-WRITE] SELECT completed\r\n"), "{name}: {lines:?}");
        // The session keeps INBOX: a later STATUS of the selected box and an
        // APPEND to it agree on the name.
        let status = client.run(&format!("STATUS {name} (MESSAGES)")).await;
        assert_eq!(status[0], "* STATUS \"INBOX\" (MESSAGES 1)\r\n", "{name}");
    }
    assert_eq!(client.answer("SELECT archive").await, "NO Mailbox does not exist\r\n");
    assert_eq!(client.answer("CREATE Archive").await, "OK CREATE completed\r\n");
    assert_eq!(client.answer("SELECT archive").await, "NO Mailbox does not exist\r\n");
}

/// uidnext-wire.test.ts: UIDNEXT comes from the counter, not the count.
async fn uidnext() {
    let mut client = Client::signed_in("uidnext").await;
    for subject in ["u1", "u2", "u3"] {
        assert!(client.append("INBOX", "()", &message(subject)).await.last().unwrap().contains(" OK [APPENDUID "));
    }
    let select = client.run("SELECT INBOX").await;
    let status = client.run("STATUS INBOX (MESSAGES UIDNEXT)").await;
    assert_eq!(status_item(&status, "MESSAGES"), 3);
    assert_eq!(code_number(&select, "UIDNEXT"), status_item(&status, "UIDNEXT"));

    // Expunging the newest message leaves the counter where it was.
    let before = status_item(&status, "UIDNEXT");
    assert_eq!(client.answer("STORE 3 +FLAGS.SILENT (\\Deleted)").await, "OK STORE completed\r\n");
    let expunged = client.exchange("EXPUNGE").await;
    assert_eq!(expunged, ["* 3 EXPUNGE\r\n", "OK EXPUNGE completed\r\n"]);
    let status = client.run("STATUS INBOX (MESSAGES UIDNEXT)").await;
    assert_eq!(status_item(&status, "MESSAGES"), 2);
    assert_eq!(status_item(&status, "UIDNEXT"), before);
    let select = client.run("SELECT INBOX").await;
    assert_eq!(code_number(&select, "UIDNEXT"), before);

    // The next APPEND takes exactly the UID both predicted.
    let appended = client.append("INBOX", "()", &message("u4")).await;
    let tagged = appended.last().unwrap();
    let uid: i64 = tagged.split("APPENDUID ").nth(1).unwrap().split([' ', ']']).nth(1).unwrap().parse().unwrap();
    assert_eq!(uid, before);
}

/// select-failed-read.test.ts: a SELECT whose reads fail deselects.
///
/// The Bun suite's fake store fails the UID read itself ("Failed to read
/// mailbox"). The real repository absorbs a fault there into an empty
/// listing, as the Bun one does, so with a real fault it is the UIDNEXT read
/// that fails, and SELECT answers its generic failure. Either way nothing
/// stays selected (RFC 3501 §6.3.1).
async fn select_failed_read() {
    let mut client = Client::signed_in("failedread").await;
    assert!(client.append("INBOX", "()", &message("f1")).await.last().unwrap().contains(" OK "));
    assert!(client.run("SELECT INBOX").await.last().unwrap().contains("OK [READ-WRITE]"));
    let answer = with_table_gone("mails", client.exchange("SELECT INBOX")).await;
    assert_eq!(answer, ["NO SELECT failed\r\n"]);
    // Nothing is selected, rather than the stale mapping kept.
    assert_eq!(client.exchange("FETCH 1 FLAGS").await, ["BAD No mailbox selected\r\n"]);
    let select = client.run("SELECT INBOX").await;
    assert!(select.contains(&"* 1 EXISTS\r\n".to_string()), "{select:?}");
}

/// mailbox-exists-transient.test.ts: a backend fault is not "does not exist".
async fn mailbox_exists_transient() {
    let mut client = Client::signed_in("transient").await;
    assert_eq!(client.answer("CREATE Archive").await, "OK CREATE completed\r\n");
    let (select, status, inbox, list) = with_table_gone("mailboxes", async {
        let select = client.exchange("SELECT Archive").await;
        let status = client.exchange("STATUS Archive (MESSAGES)").await;
        // INBOX needs no listing.
        let inbox = client.exchange("STATUS INBOX (MESSAGES)").await;
        // LIST stays answerable on the fallback listing.
        let list = client.exchange("LIST \"\" \"*\"").await;
        (select, status, inbox, list)
    })
    .await;
    assert_eq!(select, ["NO SELECT failed\r\n"]);
    assert_eq!(status, ["NO STATUS failed\r\n"]);
    assert_eq!(inbox.last().unwrap(), "OK STATUS completed\r\n");
    assert!(list.iter().any(|line| line.ends_with("\"/\" \"INBOX\"\r\n")), "{list:?}");
    assert_eq!(list.last().unwrap(), "OK LIST completed\r\n");
    // Healthy again, the real answers are back.
    assert!(client.run("SELECT Archive").await.last().unwrap().contains("OK [READ-WRITE]"));
    assert_eq!(client.exchange("SELECT Ghost").await, ["NO Mailbox does not exist\r\n"]);
}

/// mailbox-count-limit.test.ts and the accept half of mailbox-name-limit.
async fn mailbox_limits() {
    let user_id = new_user("limits").await;
    let mut client = Client::signed_in("limits").await;

    // The name ceiling counts UTF-8 bytes, and a name at it is stored.
    assert_eq!(client.answer(&format!("CREATE {}", "a".repeat(255))).await, "OK CREATE completed\r\n");
    assert_eq!(client.answer(&format!("CREATE \"{}\"", "b".repeat(255))).await, "OK CREATE completed\r\n");
    assert_eq!(client.answer(&format!("CREATE \"{}\"", "\u{1F600}".repeat(63))).await, "OK CREATE completed\r\n");
    assert_eq!(
        client.answer(&format!("CREATE \"{}\"", "\u{1F600}".repeat(64))).await,
        "NO [LIMIT] Mailbox name exceeds 255 bytes\r\n"
    );
    assert_eq!(client.answer(&format!("RENAME {} {}", "a".repeat(255), "c".repeat(255))).await, "OK RENAME completed\r\n");

    // Fill the account to one below the ceiling.
    rows(
        "INSERT INTO mailboxes (user_id, name) SELECT $1, 'box-' || n FROM generate_series(1, $2::int - 3 - 1) AS n",
        params![user_id.as_str(), MAILBOX_COUNT_MAX],
    )
    .await;
    assert_eq!(client.answer("CREATE the-last-one").await, "OK CREATE completed\r\n");
    assert_eq!(client.answer("CREATE one-too-many").await, format!("NO [LIMIT] Mailbox limit of {MAILBOX_COUNT_MAX} reached\r\n"));
    // An existing name is still reported as existing at the ceiling.
    assert_eq!(client.answer("CREATE box-1").await, "NO [ALREADYEXISTS] Mailbox already exists\r\n");
    // The ceiling is per account.
    let mut other = Client::signed_in("limits-other").await;
    assert_eq!(other.answer("CREATE mine").await, "OK CREATE completed\r\n");
}

/// message-ops.test.ts, end to end on one account: APPEND, FETCH, SEARCH,
/// STORE (and UID STORE), COPY, MOVE, EXPUNGE.
async fn message_commands() {
    let mut client = Client::signed_in("messages").await;
    assert_eq!(client.answer("CREATE Archive").await, "OK CREATE completed\r\n");

    // A mailbox that does not exist is a TRYCREATE, and nothing is stored.
    assert_eq!(
        client.append("Nowhere", "()", &message("lost")).await.last().unwrap().split_once(' ').unwrap().1,
        "NO [TRYCREATE] Mailbox does not exist\r\n"
    );
    let validity = {
        let first = client.append("INBOX", "(\\Seen)", &message("m1")).await;
        code_number(&first, "APPENDUID")
    };
    let stored = rows("SELECT imap_uid_validity FROM users WHERE username = 'messages'", vec![]).await;
    assert_eq!(stored[0].get("imap_uid_validity").as_i64(), Some(validity));
    client.append("INBOX", "()", &message("m2")).await;
    client.append("INBOX", "(\\Answered)", &message("m3")).await;
    // A \Draft APPEND to INBOX is filed in Drafts.
    client.append("INBOX", "(\\Draft)", &message("m4")).await;
    assert_eq!(status_item(&client.run("STATUS Drafts (MESSAGES)").await, "MESSAGES"), 1);

    let select = client.run("SELECT INBOX").await;
    assert!(select.contains(&"* 3 EXISTS\r\n".to_string()), "{select:?}");
    assert!(select.contains(&"* OK [UNSEEN 2] Message 2 is first unseen\r\n".to_string()), "{select:?}");

    // FLAGS from APPEND: only those sent.
    let flags = client.exchange("FETCH 1:3 (FLAGS)").await;
    assert_eq!(
        flags,
        ["* 1 FETCH (FLAGS (\\Seen))\r\n", "* 2 FETCH (FLAGS ())\r\n", "* 3 FETCH (FLAGS (\\Answered))\r\n", "OK FETCH completed\r\n"]
    );

    // RFC822.SIZE is the length BODY[] announces, and the body is the message.
    let fetched = client.run("FETCH 2 (RFC822.SIZE BODY.PEEK[])").await;
    let line = &fetched[0];
    let size: usize = line.split("RFC822.SIZE ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
    let announced: usize = line.split("BODY[] {").nth(1).unwrap().split('}').next().unwrap().parse().unwrap();
    assert_eq!(size, announced);
    assert!(line.contains("Subject: m2\r\n"), "{line}");
    assert!(line.ends_with(")\r\n"));
    // A PEEK leaves it unseen; a BODY[] marks it seen.
    assert_eq!(client.run("FETCH 2 (FLAGS)").await[0], "* 2 FETCH (FLAGS ())\r\n");
    client.run("FETCH 2 (BODY[TEXT])").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(client.run("FETCH 2 (FLAGS)").await[0], "* 2 FETCH (FLAGS (\\Seen))\r\n");

    // SEARCH speaks sequence numbers, UID SEARCH UIDs.
    let uids: Vec<i64> = client.run("UID SEARCH ALL").await[0]
        .trim_end()
        .split(' ')
        .skip(2)
        .map(|uid| uid.parse().unwrap())
        .collect();
    assert_eq!(uids.len(), 3);
    assert_eq!(client.run("SEARCH ANSWERED").await[0], "* SEARCH 3\r\n");
    assert_eq!(client.run("SEARCH 2:3 UNANSWERED").await[0], "* SEARCH 2\r\n");

    // STORE: an empty range answers one OK and nothing else; a UID STORE
    // carries the UID item; SILENT carries nothing.
    assert_eq!(client.exchange("UID STORE 999999 +FLAGS (\\Flagged)").await, ["OK STORE completed\r\n"]);
    assert_eq!(
        client.exchange(&format!("UID STORE {} +FLAGS (\\Flagged)", uids[0])).await,
        [format!("* 1 FETCH (UID {} FLAGS (\\Seen \\Flagged))\r\n", uids[0]), "OK STORE completed\r\n".to_string()]
    );
    assert_eq!(client.exchange("STORE 1 -FLAGS (\\Flagged)").await, ["* 1 FETCH (FLAGS (\\Seen))\r\n", "OK STORE completed\r\n"]);
    assert_eq!(client.exchange("STORE 1 +FLAGS.SILENT (\\Answered)").await, ["OK STORE completed\r\n"]);

    // CONDSTORE: a message changed since the client's mod-sequence is not
    // touched and is named in [MODIFIED].
    let modseq_line = client.run("FETCH 2 (MODSEQ)").await;
    let modseq: i64 = modseq_line[0].split("MODSEQ (").nth(1).unwrap().split(')').next().unwrap().parse().unwrap();
    assert!(client.answer("STORE 2 +FLAGS.SILENT (\\Flagged)").await.starts_with("OK"));
    assert_eq!(
        client.answer(&format!("STORE 2 (UNCHANGEDSINCE {modseq}) +FLAGS (\\Deleted)")).await,
        "OK [MODIFIED 2] Conditional STORE failed\r\n"
    );

    // COPY reports the pairs; the copy keeps its flags.
    let copied = client.exchange("COPY 1:2 Archive").await;
    let tagged = copied.last().unwrap();
    assert!(tagged.starts_with(&format!("OK [COPYUID {validity} {}:{} ", uids[0], uids[1])), "{tagged}");
    let archive = client.run("STATUS Archive (MESSAGES)").await;
    assert_eq!(status_item(&archive, "MESSAGES"), 2);
    assert_eq!(client.answer("COPY 1 Nowhere").await, "NO [TRYCREATE] Mailbox does not exist\r\n");

    // MOVE: the copy, then the source's EXPUNGE, then COPYUID on the tagged OK.
    let moved = client.exchange("MOVE 3 Archive").await;
    assert_eq!(moved[0], "* 3 EXPUNGE\r\n", "{moved:?}");
    assert!(moved[1].starts_with(&format!("OK [COPYUID {validity} {} ", uids[2])), "{moved:?}");
    assert!(moved[1].ends_with("] MOVE completed\r\n"), "{moved:?}");
    assert_eq!(moved.len(), 2);
    // A Bun oddity, as the Bun server has it: a copy filed in a user mailbox
    // is a received mail re-addressed to <mailbox>@<domain>, and INBOX lists
    // every received mail of the domain. So the two COPY copies and the MOVE
    // copy show in INBOX too, beside the two originals left there.
    let inbox = client.run("STATUS INBOX (MESSAGES)").await;
    assert_eq!(status_item(&inbox, "MESSAGES"), 5);

    // EXPUNGE removes what is \Deleted, announcing it by sequence number.
    let select = client.run("SELECT INBOX").await;
    assert!(select.contains(&"* 5 EXISTS\r\n".to_string()), "{select:?}");
    assert_eq!(client.answer("STORE 1 +FLAGS.SILENT (\\Deleted)").await, "OK STORE completed\r\n");
    assert_eq!(client.exchange("EXPUNGE").await, ["* 1 EXPUNGE\r\n", "OK EXPUNGE completed\r\n"]);
    assert_eq!(client.run("SEARCH ALL").await[0], "* SEARCH 1 2 3 4\r\n");
}

async fn scenario() {
    for username in ["existence", "casing", "uidnext", "failedread", "transient", "messages", "limits-other"] {
        new_user(username).await;
    }
    mailbox_existence().await;
    inbox_case().await;
    uidnext().await;
    select_failed_read().await;
    mailbox_exists_transient().await;
    mailbox_limits().await;
    message_commands().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mailbox_and_message_commands_run_on_the_store() {
    if std::env::var("MAIL_TEST_POSTGRES").as_deref() != Ok("1") {
        eprintln!("skipped: set MAIL_TEST_POSTGRES=1 to run against PostgreSQL");
        return;
    }
    let database = format!("imap_session_rs_{}", std::process::id());
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

    // APPEND parses into `./attachments/.incoming`.
    let work_directory = tempfile::tempdir().expect("a temporary directory");
    std::env::set_current_dir(work_directory.path()).expect("the working directory");

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
        scenario().await
    })
    .catch_unwind()
    .await;

    pool().end();
    admin(&format!("DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)")).await;
    if let Err(panic) = outcome {
        resume_unwind(panic);
    }
}
