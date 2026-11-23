//! The SMTP listener and its connections, driven over real sockets: the
//! cases of `src/server/lib/smtp/server.test.ts`.

mod smtp_support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Instant;

use mail_server::smtp::{
    create_smtp_server, evict_smtp_connections, register_smtp_server, unregister_smtp_server, SharedSmtpHandler,
    SmtpAddress, SmtpAuthRequest, SmtpAuthResponse, SmtpDataStream, SmtpDataTooLarge, SmtpEnvelope, SmtpHandler,
    SmtpHookError, SmtpServer, SmtpServerError, SmtpServerOptions, SmtpSession,
};
use smtp_support::{tls_source, Client};

const SIZE: u64 = 1024 * 1024;
const MESSAGE: &str = "Subject: hi\r\n\r\nbody\r\n";
const EHLO_REPLY: &str = "250-mx.test Nice to meet you, [127.0.0.1]\r\n250-PIPELINING\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250-AUTH LOGIN PLAIN\r\n250 SIZE 1048576";

#[derive(Debug, Clone)]
struct Delivery {
    secure: bool,
    remote_address: String,
    remote_port: u16,
    envelope: SmtpEnvelope,
    data: Vec<u8>,
    ended: bool,
}

/// What `on_data` does with the message.
#[derive(Clone, Default)]
enum OnData {
    /// Reads it and accepts.
    #[default]
    Collect,
    /// Reads it, then accepts once the gate opens.
    Gate(Arc<Notify>),
    /// Refuses at once, leaving the message unread.
    Refuse(SmtpHookError),
    /// Parks the stream where the test can reach it, and accepts once the
    /// test has read it out.
    Park(Arc<Mutex<Option<SmtpDataStream>>>, Arc<Notify>),
}

#[derive(Clone, Default)]
struct TestHandler {
    deliveries: Arc<Mutex<Vec<Delivery>>>,
    auth_error: Option<SmtpHookError>,
    rcpt_refusal: Option<SmtpHookError>,
    on_data: OnData,
}

impl SmtpHandler for TestHandler {
    async fn on_auth(
        &self,
        auth: SmtpAuthRequest,
        _session: &mut SmtpSession,
    ) -> Result<SmtpAuthResponse, SmtpHookError> {
        if let Some(error) = &self.auth_error {
            return Err(error.clone());
        }
        match auth.username == "user" && auth.password == "pass" {
            true => Ok(SmtpAuthResponse::accept("user")),
            false => Ok(SmtpAuthResponse::reject()),
        }
    }

    async fn on_mail_from(&self, _address: &SmtpAddress, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        Ok(())
    }

    async fn on_rcpt_to(&self, _address: &SmtpAddress, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        match &self.rcpt_refusal {
            Some(refusal) => Err(refusal.clone()),
            None => Ok(()),
        }
    }

    async fn on_data(&self, mut stream: SmtpDataStream, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        let gate = match &self.on_data {
            OnData::Refuse(refusal) => return Err(refusal.clone()),
            OnData::Park(slot, done) => {
                *slot.lock().unwrap() = Some(stream);
                done.notified().await;
                return Ok(());
            }
            OnData::Gate(gate) => Some(gate.clone()),
            OnData::Collect => None,
        };
        let index = {
            let mut deliveries = self.deliveries.lock().unwrap();
            deliveries.push(Delivery {
                secure: session.secure,
                remote_address: session.remote_address.clone(),
                remote_port: session.remote_port,
                envelope: session.envelope.clone(),
                data: Vec::new(),
                ended: false,
            });
            deliveries.len() - 1
        };
        let mut data = Vec::new();
        let ended = loop {
            match stream.next_chunk().await {
                Ok(Some(chunk)) => data.extend_from_slice(&chunk),
                Ok(None) => break true,
                Err(error) => {
                    assert!(SmtpDataStream::is_abandoned(&error));
                    break false;
                }
            }
        };
        {
            let mut deliveries = self.deliveries.lock().unwrap();
            deliveries[index].data = data;
            deliveries[index].ended = ended;
        }
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(())
    }
}

struct Started {
    server: SmtpServer,
    port: u16,
    deliveries: Arc<Mutex<Vec<Delivery>>>,
    errors: Arc<Mutex<Vec<SmtpServerError>>>,
}

impl Started {
    fn delivery(&self, index: usize) -> Delivery {
        self.deliveries.lock().unwrap()[index].clone()
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        // Stops the listener; the runtime ends with the test and takes the rest.
        drop(self.server.close());
    }
}

fn options(handler: TestHandler) -> SmtpServerOptions {
    SmtpServerOptions {
        secure: false,
        proxy_protocol: false,
        allow_insecure_auth: false,
        name: Some("mx.test".to_string()),
        tls: None,
        size: SIZE,
        max_clients: 10,
        close_timeout: Duration::from_millis(200),
        socket_timeout: None,
        handler: SharedSmtpHandler::new(handler),
    }
}

async fn start_with(handler: TestHandler, configure: impl FnOnce(&mut SmtpServerOptions)) -> Started {
    let deliveries = handler.deliveries.clone();
    let mut options = options(handler);
    configure(&mut options);
    let server = create_smtp_server(options);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let reported = errors.clone();
    server.on_error(move |error| reported.lock().unwrap().push(error.clone()));
    let port = server.listen(0).await.expect("a free port").port();
    Started { server, port, deliveries, errors }
}

async fn start() -> Started {
    start_with(TestHandler::default(), |_| {}).await
}

async fn greeted(port: u16) -> Client {
    let mut client = Client::connect(port).await;
    assert_eq!(client.reply(Duration::from_secs(2)).await, "220 mx.test ESMTP");
    client
}

async fn reply(client: &mut Client) -> String {
    client.reply(Duration::from_secs(2)).await
}

/// Greeting, EHLO, one sender, one recipient, DATA accepted.
async fn transact(port: u16) -> Client {
    let mut client = greeted(port).await;
    client.command("EHLO client").await;
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "250 Accepted");
    assert_eq!(client.command("DATA").await, "354 End data with <CR><LF>.<CR><LF>");
    client
}

fn address(address: &str) -> SmtpAddress {
    SmtpAddress { address: address.to_string(), args: None }
}

fn plain(user: &str, pass: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"))
}

fn base64(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text)
}

// ── SMTP greeting and EHLO ─────────────────────────────────────────────────

#[tokio::test]
async fn greets_then_lists_the_extensions_with_the_size_ceiling() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    assert_eq!(client.command("EHLO client.test").await, EHLO_REPLY);
}

#[tokio::test]
async fn refuses_a_client_that_talks_before_the_greeting() {
    let started = start().await;
    let mut client = Client::connect(started.port).await;
    client.send(b"EHLO early\r\n").await;
    assert_eq!(reply(&mut client).await, "421 mx.test You talk too soon");
}

#[tokio::test]
async fn refuses_an_http_request_line() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    assert_eq!(client.command("GET / HTTP/1.1").await, "421 HTTP requests not allowed");
}

#[tokio::test]
async fn requires_helo_before_a_transaction_and_checks_its_syntax() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "503 Error: send HELO/EHLO first");
    assert_eq!(client.command("EHLO").await, "501 Error: syntax: EHLO hostname");
    assert_eq!(client.command("HELO a b").await, "501 Error: Syntax: HELO hostname");
    assert_eq!(client.command("HELO client").await, "250 mx.test Nice to meet you, [127.0.0.1]");
}

#[tokio::test]
async fn closes_after_ten_unrecognized_commands() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    for i in 0..9 {
        assert_eq!(client.command(&format!("XYZ{i}")).await, "500 Error: command not recognized");
    }
    assert_eq!(client.command("XYZ9").await, "421 Error: too many unrecognized commands");
    assert_eq!(reply(&mut client).await, "<closed>");
}

#[tokio::test]
async fn refuses_a_command_line_past_the_length_limit() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    client.send(format!("NOOP {}", "x".repeat(20 * 1024)).as_bytes()).await;
    assert_eq!(reply(&mut client).await, "421 Error: line too long");
}

// ── SMTP transaction ───────────────────────────────────────────────────────

#[tokio::test]
async fn delivers_the_message_dot_unstuffed_with_the_terminator_removed() {
    let started = start().await;
    let mut client = transact(started.port).await;
    client.send(b"..leading dot\r\n.x\r\nlast\r\n.\r\n").await;
    assert_eq!(reply(&mut client).await, "250 OK: message queued");
    let delivery = started.delivery(0);
    assert_eq!(delivery.data, b".leading dot\r\n.x\r\nlast\r\n");
    assert_eq!(delivery.envelope.mail_from, Some(address("a@x.org")));
    assert_eq!(delivery.envelope.rcpt_to, vec![address("b@y.org")]);
}

#[tokio::test]
async fn finds_the_terminator_split_across_writes() {
    let started = start().await;
    let mut client = transact(started.port).await;
    for piece in ["Subject: x\r\n\r\nbody\r", "\n", ".", "\r", "\n"] {
        client.send(piece.as_bytes()).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(reply(&mut client).await, "250 OK: message queued");
    assert_eq!(started.delivery(0).data, b"Subject: x\r\n\r\nbody\r\n");
}

#[tokio::test]
async fn answers_pipelined_commands_in_order_holding_those_after_data_until_its_reply() {
    let gate = Arc::new(Notify::new());
    let handler = TestHandler { on_data: OnData::Gate(gate.clone()), ..Default::default() };
    let started = start_with(handler, |_| {}).await;
    let mut client = greeted(started.port).await;
    client.send(b"EHLO c\r\nMAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\n").await;
    assert!(reply(&mut client).await.starts_with("250-mx.test"));
    assert_eq!(reply(&mut client).await, "250 Accepted");
    assert_eq!(reply(&mut client).await, "250 Accepted");
    assert_eq!(reply(&mut client).await, "354 End data with <CR><LF>.<CR><LF>");
    client.send(format!("{MESSAGE}.\r\nNOOP\r\n").as_bytes()).await;
    assert_eq!(client.reply(Duration::from_millis(300)).await, "<timeout>");
    gate.notify_one();
    assert_eq!(reply(&mut client).await, "250 OK: message queued");
    assert_eq!(reply(&mut client).await, "250 OK");
}

#[tokio::test]
async fn refuses_a_declared_size_over_the_ceiling_at_mail_from() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    client.command("EHLO c").await;
    assert_eq!(
        client.command(&format!("MAIL FROM:<a@x.org> SIZE={}", SIZE + 1)).await,
        format!("552 Error: message exceeds fixed maximum message size {SIZE}")
    );
}

#[tokio::test]
async fn checks_mail_from_syntax_and_parameters() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    client.command("EHLO c").await;
    assert_eq!(client.command("MAIL FROM:a@x.org").await, "501 Error: Bad sender address syntax");
    assert_eq!(
        client.command("MAIL FROM:<a@x.org> BODY=BINARYMIME").await,
        "501 Invalid BODY parameter value. Must be 7BIT or 8BITMIME"
    );
    assert_eq!(
        client.command("MAIL FROM:<a@x.org> REQUIRETLS").await,
        "530 REQUIRETLS not permitted on non-TLS connections"
    );
    assert_eq!(client.command("MAIL FROM:<>").await, "250 Accepted");
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "503 Error: nested MAIL command");
}

#[tokio::test]
async fn checks_rcpt_to_ordering_syntax_and_the_recipient_limit() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    client.command("EHLO c").await;
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "503 Error: need MAIL command");
    client.command("MAIL FROM:<a@x.org>").await;
    assert_eq!(client.command("RCPT TO:<>").await, "501 Error: Bad recipient address syntax");
    for i in 0..100 {
        client.command(&format!("RCPT TO:<r{i}@y.org>")).await;
    }
    assert_eq!(client.command("RCPT TO:<R0@Y.ORG>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<r100@y.org>").await, "452 Error: too many recipients");
}

#[tokio::test]
async fn answers_a_hooks_refusal_with_its_response_code() {
    let refusal = SmtpHookError::with_code(550, "5.1.1 no such mailbox");
    let handler = TestHandler { rcpt_refusal: Some(refusal), ..Default::default() };
    let started = start_with(handler, |_| {}).await;
    let mut client = greeted(started.port).await;
    client.command("EHLO c").await;
    client.command("MAIL FROM:<a@x.org>").await;
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "550 5.1.1 no such mailbox");
    assert_eq!(client.command("DATA").await, "503 Error: need RCPT command");
}

#[tokio::test]
async fn answers_a_refused_data_transaction_once_the_message_has_been_read_out() {
    let refusal = SmtpHookError::with_code(552, "too big");
    let handler = TestHandler { on_data: OnData::Refuse(refusal), ..Default::default() };
    let started = start_with(handler, |_| {}).await;
    let mut client = transact(started.port).await;
    client.send(MESSAGE.as_bytes()).await;
    assert_eq!(client.reply(Duration::from_millis(300)).await, "<timeout>");
    client.send(b".\r\n").await;
    assert_eq!(reply(&mut client).await, "552 too big");
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
}

#[tokio::test]
async fn forgets_the_envelope_on_rset() {
    let started = start().await;
    let mut client = transact(started.port).await;
    client.send(format!("{MESSAGE}.\r\n").as_bytes()).await;
    reply(&mut client).await;
    client.command("MAIL FROM:<a@x.org>").await;
    client.command("RCPT TO:<b@y.org>").await;
    assert_eq!(client.command("RSET").await, "250 Flushed");
    assert_eq!(client.command("DATA").await, "503 Error: need RCPT command");
}

#[tokio::test]
async fn closes_the_data_stream_without_ending_it_when_the_client_drops_mid_message() {
    let started = start().await;
    let mut client = transact(started.port).await;
    client.send(b"Subject: partial\r\n\r\nsome of the bo").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.destroy();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let delivery = started.delivery(0);
    assert!(!delivery.ended);
    assert_eq!(delivery.data, b"Subject: partial\r\n\r\nsome of the bo");
}

#[tokio::test]
async fn ends_data_only_at_crlf_dot_crlf_delivering_bare_lf_and_cr_dot_lines_as_content() {
    let started = start().await;
    let mut client = transact(started.port).await;
    let content = concat!(
        "Subject: x\r\n\r\nA\n.\r\nMAIL FROM:<smuggled@x.org>\r\n",
        "B\r\n.\nRCPT TO:<smuggled@x.org>\r\nC\n.\nD\r.\rE\r\n"
    );
    client.send(format!("{content}.\r\nNOOP\r\n").as_bytes()).await;
    assert_eq!(reply(&mut client).await, "250 OK: message queued");
    assert_eq!(reply(&mut client).await, "250 OK");
    assert_eq!(client.reply(Duration::from_millis(300)).await, "<timeout>");
    assert_eq!(started.deliveries.lock().unwrap().len(), 1);
    assert_eq!(started.delivery(0).data, content.as_bytes());
}

#[tokio::test]
async fn holds_the_sender_back_while_the_data_consumer_is_not_reading() {
    let slot = Arc::new(Mutex::new(None));
    let done = Arc::new(Notify::new());
    let handler = TestHandler { on_data: OnData::Park(slot.clone(), done.clone()), ..Default::default() };
    let started = start_with(handler, |_| {}).await;
    let mut client = transact(started.port).await;

    let chunk = vec![0x61u8; 64 * 1024];
    let chunks = 64;
    let mut payload = chunk.repeat(chunks);
    payload.extend_from_slice(b"\r\n.\r\n");
    // The server stops reading, so the write only finishes once the consumer reads.
    let sending = tokio::spawn(async move {
        client.send(&payload).await;
        client
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut stream = slot.lock().unwrap().take().expect("on_data was called");
    // What the server read ahead is a socket read or two, not the message.
    assert!(stream.buffered() < 1024 * 1024, "{} bytes read ahead", stream.buffered());

    let mut total = 0;
    while let Some(piece) = stream.next_chunk().await.expect("the message ends") {
        total += piece.len();
    }
    done.notify_one();
    let mut client = sending.await.unwrap();
    assert_eq!(client.reply(Duration::from_secs(5)).await, "250 OK: message queued");
    assert_eq!(total, chunks * chunk.len() + 2);
}

// ── SMTP AUTH ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn refuses_auth_in_cleartext_unless_the_listener_allows_it() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    client.command("EHLO c").await;
    assert_eq!(
        client.command(&format!("AUTH PLAIN {}", plain("user", "pass"))).await,
        "538 Error: Must issue a STARTTLS command first"
    );
}

#[tokio::test]
async fn authenticates_plain_and_login_and_refuses_a_second_identity() {
    let started = start_with(TestHandler::default(), |options| options.allow_insecure_auth = true).await;
    let mut client = greeted(started.port).await;
    client.command("EHLO c").await;
    assert_eq!(
        client.command(&format!("AUTH PLAIN {}", plain("user", "wrong"))).await,
        "535 Error: Authentication credentials invalid"
    );
    assert_eq!(client.command("AUTH LOGIN").await, "334 VXNlcm5hbWU6");
    assert_eq!(client.command(&base64("user")).await, "334 UGFzc3dvcmQ6");
    assert_eq!(client.command(&base64("pass")).await, "235 Authentication successful");
    assert_eq!(
        client.command(&format!("AUTH PLAIN {}", plain("user", "pass"))).await,
        "503 Error: No identity changes permitted"
    );
    assert!(!client.command("EHLO c").await.contains("AUTH"));
}

#[tokio::test]
async fn handles_the_plain_continuation_abort_and_malformed_tokens() {
    let started = start_with(TestHandler::default(), |options| options.allow_insecure_auth = true).await;
    let mut client = greeted(started.port).await;
    client.command("EHLO c").await;
    assert_eq!(client.command("AUTH PLAIN").await, "334 ");
    assert_eq!(client.command("*").await, "501 Authentication aborted");
    assert_eq!(client.command(&format!("AUTH PLAIN {}", base64("nonul"))).await, "500 Error: invalid userdata");
    assert_eq!(client.command("AUTH CRAM-MD5").await, "504 Error: Unrecognized authentication type");
}

#[tokio::test]
async fn answers_an_on_auth_error_with_its_code() {
    let limited = SmtpHookError::new("Too many failed authentication attempts");
    let handler = TestHandler { auth_error: Some(limited), ..Default::default() };
    let started = start_with(handler, |options| options.allow_insecure_auth = true).await;
    let mut client = greeted(started.port).await;
    client.command("EHLO c").await;
    assert_eq!(
        client.command(&format!("AUTH PLAIN {}", plain("user", "pass"))).await,
        "535 Too many failed authentication attempts"
    );
}

// ── SMTP credential eviction ───────────────────────────────────────────────

#[tokio::test]
async fn closes_a_live_authenticated_connection_when_its_credential_rotates() {
    let started = start_with(TestHandler::default(), |options| options.allow_insecure_auth = true).await;
    register_smtp_server(&started.server);
    let mut authed = greeted(started.port).await;
    let mut anonymous = greeted(started.port).await;
    authed.command("EHLO c").await;
    assert_eq!(
        authed.command(&format!("AUTH PLAIN {}", plain("user", "pass"))).await,
        "235 Authentication successful"
    );

    assert_eq!(evict_smtp_connections("user"), 1);
    assert_eq!(reply(&mut authed).await, "421 Credential changed, please re-authenticate");
    assert_eq!(reply(&mut authed).await, "<closed>");
    assert_eq!(anonymous.command("NOOP").await, "250 OK");
    unregister_smtp_server(&started.server);
}

// ── The client's FIN ───────────────────────────────────────────────────────

/// Greets, then sends `commands` and the client's FIN in one segment, so the
/// server finds both when it reads. Everything the server answered to them,
/// up to its own FIN.
async fn send_with_the_fin(port: u16, commands: &str) -> String {
    use std::os::fd::AsRawFd;

    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut seen = Vec::new();
    let mut chunk = [0u8; 4096];
    for (send, expect) in [("", "220 mx.test ESMTP\r\n"), ("EHLO c\r\n", "250 SIZE 1048576\r\n")] {
        socket.write_all(send.as_bytes()).await.unwrap();
        while !seen.ends_with(expect.as_bytes()) {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the server hung up early");
            seen.extend_from_slice(&chunk[..read]);
        }
    }
    seen.clear();

    let on: libc::c_int = 1;
    // SAFETY: the descriptor is this open socket's, and the option value is
    // a c_int that outlives the call.
    let failed = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_CORK,
            std::ptr::from_ref(&on).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    } != 0;
    assert!(!failed, "TCP_CORK could not be set");
    socket.write_all(commands.as_bytes()).await.unwrap();
    // The FIN leaves with the corked data.
    socket.shutdown().await.unwrap();
    socket.read_to_end(&mut seen).await.unwrap();
    String::from_utf8(seen).unwrap()
}

#[tokio::test]
async fn answers_every_command_of_a_client_whose_fin_arrives_with_them() {
    let started = start().await;
    let session = format!("MAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\n{MESSAGE}.\r\nNOOP\r\nQUIT\r\n");
    assert_eq!(
        send_with_the_fin(started.port, &session).await,
        "250 Accepted\r\n250 Accepted\r\n354 End data with <CR><LF>.<CR><LF>\r\n250 OK: message queued\r\n250 OK\r\n221 Bye\r\n"
    );
    let delivery = started.delivery(0);
    assert!(delivery.ended);
    assert_eq!(delivery.data, MESSAGE.as_bytes());
}

#[tokio::test]
async fn says_nothing_more_once_the_fin_is_reached_while_a_hook_is_still_running() {
    let gate = Arc::new(Notify::new());
    let handler = TestHandler { on_data: OnData::Gate(gate.clone()), ..Default::default() };
    let started = start_with(handler, |_| {}).await;
    let session = format!("MAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\n{MESSAGE}.\r\nNOOP\r\nQUIT\r\n");
    // The message is read in full, but the hook has not answered when the
    // client's FIN is reached.
    assert_eq!(
        send_with_the_fin(started.port, &session).await,
        "250 Accepted\r\n250 Accepted\r\n354 End data with <CR><LF>.<CR><LF>\r\n"
    );
    gate.notify_one();
    let delivery = started.delivery(0);
    assert!(delivery.ended);
    assert_eq!(delivery.data, MESSAGE.as_bytes());
}

// ── SMTP reply backpressure ────────────────────────────────────────────────

/// Counts the reply lines arriving on `socket` until it closes or `stop` is told.
async fn count_replies(socket: &mut (impl AsyncReadExt + Unpin), expected: usize, within: Duration) -> usize {
    let deadline = Instant::now() + within;
    let mut replies = 0;
    let mut chunk = vec![0u8; 256 * 1024];
    while replies < expected {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, socket.read(&mut chunk)).await {
            Ok(Ok(read)) if read > 0 => replies += chunk[..read].iter().filter(|&&byte| byte == b'\n').count(),
            _ => break,
        }
    }
    replies
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stops_reading_commands_while_the_client_is_not_reading_its_replies() {
    let started = start().await;
    let socket = TcpStream::connect(("127.0.0.1", started.port)).await.unwrap();
    let (mut reader, mut writer) = socket.into_split();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let commands = 1_000_000;
    let flood = tokio::spawn(async move {
        let _ = writer.write_all(&b"NOOP\r\n".repeat(commands)).await;
        writer
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Without backpressure every reply to the flood would be queued here: ~8 MB.
    assert!(started.server.queued_reply_bytes() < 256 * 1024, "{} bytes queued", started.server.queued_reply_bytes());

    let replies = count_replies(&mut reader, commands + 1, Duration::from_secs(30)).await;
    assert_eq!(replies, commands + 1);
    drop(flood.await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn takes_commands_again_after_each_of_repeated_stalls() {
    let started = start().await;
    let socket = TcpStream::connect(("127.0.0.1", started.port)).await.unwrap();
    let (mut reader, mut writer) = socket.into_split();
    assert_eq!(count_replies(&mut reader, 1, Duration::from_secs(2)).await, 1);

    let burst = b"NOOP\r\n".repeat(200_000);
    for round in 0..12 {
        // The client reads nothing while it writes, so the server stalls on its replies.
        let burst = burst.clone();
        let sending = tokio::spawn(async move {
            let _ = writer.write_all(&burst).await;
            writer
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        let replies = count_replies(&mut reader, 200_000, Duration::from_secs(20)).await;
        assert_eq!(replies, 200_000, "round {round}");
        writer = sending.await.unwrap();
    }
    assert_eq!(started.server.connection_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drops_a_connection_that_stops_reading_once_the_socket_timeout_passes() {
    let started =
        start_with(TestHandler::default(), |options| options.socket_timeout = Some(Duration::from_millis(500))).await;
    let socket = TcpStream::connect(("127.0.0.1", started.port)).await.unwrap();
    let (mut reader, mut writer) = socket.into_split();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let flood = tokio::spawn(async move {
        let _ = writer.write_all(&b"NOOP\r\n".repeat(1_000_000)).await;
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while started.server.connection_count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(started.server.connection_count(), 0);

    // What the client now reads ends: the server let go of the socket.
    let mut chunk = vec![0u8; 256 * 1024];
    let closed = loop {
        match tokio::time::timeout(Duration::from_secs(5), reader.read(&mut chunk)).await {
            Ok(Ok(read)) if read > 0 => continue,
            Ok(_) => break true,
            Err(_) => break false,
        }
    };
    assert!(closed);
    flood.abort();
}

// ── SMTP limits and shutdown ───────────────────────────────────────────────

#[tokio::test]
async fn answers_new_commands_421_once_closing_and_closes_idle_ones_at_the_deadline() {
    let started = start().await;
    let mut client = greeted(started.port).await;
    let mut idle = greeted(started.port).await;
    let closing = tokio::spawn(started.server.close());
    assert_eq!(client.command("NOOP").await, "421 Server shutting down");
    assert_eq!(idle.reply(Duration::from_secs(1)).await, "421 Server shutting down");
    tokio::time::timeout(Duration::from_secs(1), closing).await.expect("close returns at its deadline").unwrap();
}

#[tokio::test]
async fn refuses_connections_past_max_clients() {
    let started = start_with(TestHandler::default(), |options| options.max_clients = 1).await;
    let _first = greeted(started.port).await;
    let mut extra = Client::connect(started.port).await;
    assert_eq!(reply(&mut extra).await, "421 mx.test Too many connected clients, try again in a moment");
}

#[tokio::test]
async fn closes_a_connection_that_stays_idle_past_the_socket_timeout() {
    let started =
        start_with(TestHandler::default(), |options| options.socket_timeout = Some(Duration::from_millis(200))).await;
    let mut client = greeted(started.port).await;
    assert_eq!(client.reply(Duration::from_secs(1)).await, "421 Timeout - closing connection");
}

// ── SMTP over TLS ──────────────────────────────────────────────────────────

fn with_tls(options: &mut SmtpServerOptions) {
    options.tls = Some(tls_source());
}

#[tokio::test]
async fn offers_starttls_and_discards_plaintext_pipelined_after_it() {
    let started = start_with(TestHandler::default(), with_tls).await;
    let mut client = greeted(started.port).await;
    assert!(client.command("EHLO c").await.contains("250-STARTTLS"));
    client.send(b"STARTTLS\r\nMAIL FROM:<injected@x.org>\r\nRCPT TO:<b@y.org>\r\n").await;
    assert_eq!(reply(&mut client).await, "220 Ready to start TLS");
    assert_eq!(client.start_tls().await, "secure");
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "503 Error: send HELO/EHLO first");
    assert!(!client.command("EHLO c").await.contains("STARTTLS"));
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "250 Accepted");
    assert_eq!(client.command("DATA").await, "354 End data with <CR><LF>.<CR><LF>");
    client.send(format!("{MESSAGE}.\r\n").as_bytes()).await;
    assert_eq!(reply(&mut client).await, "250 OK: message queued");
    let delivery = started.delivery(0);
    assert_eq!(delivery.envelope.mail_from, Some(address("a@x.org")));
    assert!(delivery.secure);
    assert_eq!(client.command("STARTTLS").await, "503 Error: TLS already active");
}

#[tokio::test]
async fn runs_the_handshake_before_the_greeting_on_an_implicit_tls_listener() {
    let started = start_with(TestHandler::default(), |options| {
        with_tls(options);
        options.secure = true;
    })
    .await;
    let mut client = Client::connect(started.port).await;
    assert_eq!(client.start_tls().await, "secure");
    assert_eq!(reply(&mut client).await, "220 mx.test ESMTP");
    assert!(!client.command("EHLO c").await.contains("STARTTLS"));
}

#[tokio::test]
async fn reports_a_failed_implicit_tls_handshake_tagged_as_pre_session() {
    let started = start_with(TestHandler::default(), |options| {
        with_tls(options);
        options.secure = true;
    })
    .await;
    let mut client = Client::connect(started.port).await;
    client.send(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    client.reply(Duration::from_millis(500)).await;
    let errors = started.errors.lock().unwrap().clone();
    assert!(!errors.is_empty());
    let meta = errors[0].meta.as_ref().expect("a handshake failure carries where it happened");
    assert_eq!(meta.stage, "connect");
    assert_eq!(meta.remote_address.as_deref(), Some("127.0.0.1"));
}

#[tokio::test]
async fn reports_a_failed_starttls_handshake_with_the_peer_on_the_error() {
    let started = start_with(TestHandler::default(), with_tls).await;
    let mut client = greeted(started.port).await;
    client.command("EHLO client.example").await;
    client.command("STARTTLS").await;
    client.send(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    client.reply(Duration::from_millis(500)).await;
    let errors = started.errors.lock().unwrap().clone();
    assert!(!errors.is_empty());
    assert_eq!(errors[0].remote_address.as_deref(), Some("127.0.0.1"));
}

#[tokio::test]
async fn asks_for_the_certificate_again_at_every_handshake() {
    // The listener does not keep a TLS context: each handshake asks its
    // source, so a certificate that cannot be loaded right now refuses that
    // STARTTLS, and one that is back serves the next.
    let (loads, failing) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
    let source = {
        let (loads, failing, on_disk) = (loads.clone(), failing.clone(), tls_source());
        move || {
            loads.fetch_add(1, Ordering::SeqCst);
            match failing.load(Ordering::SeqCst) {
                true => Err(anyhow::anyhow!("the certificate is being renewed")),
                false => on_disk(),
            }
        }
    };
    let started = start_with(TestHandler::default(), |options| options.tls = Some(Arc::new(source))).await;
    let mut client = greeted(started.port).await;
    assert!(client.command("EHLO c").await.contains("250-STARTTLS"));
    assert_eq!(loads.load(Ordering::SeqCst), 0);

    failing.store(true, Ordering::SeqCst);
    assert_eq!(client.command("STARTTLS").await, "454 Error: TLS not available");
    failing.store(false, Ordering::SeqCst);
    assert_eq!(client.command("STARTTLS").await, "220 Ready to start TLS");
    assert_eq!(client.start_tls().await, "secure");
    assert!(client.command("EHLO c").await.starts_with("250-mx.test"));
    assert_eq!(loads.load(Ordering::SeqCst), 2);

    let mut second = greeted(started.port).await;
    assert_eq!(second.command("STARTTLS").await, "220 Ready to start TLS");
    assert_eq!(second.start_tls().await, "secure");
    assert_eq!(loads.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn drops_an_implicit_tls_connection_whose_certificate_cannot_be_loaded() {
    let failing = Arc::new(AtomicBool::new(true));
    let source = {
        let (failing, on_disk) = (failing.clone(), tls_source());
        move || match failing.load(Ordering::SeqCst) {
            true => Err(anyhow::anyhow!("the certificate is being renewed")),
            false => on_disk(),
        }
    };
    let started = start_with(TestHandler::default(), |options| {
        options.tls = Some(Arc::new(source));
        options.secure = true;
    })
    .await;
    let mut client = Client::connect(started.port).await;
    assert_eq!(client.closed(Duration::from_secs(2)).await, "closed");
    assert!(started.errors.lock().unwrap().is_empty());

    failing.store(false, Ordering::SeqCst);
    let mut client = Client::connect(started.port).await;
    assert_eq!(client.start_tls().await, "secure");
    assert_eq!(reply(&mut client).await, "220 mx.test ESMTP");
}

// ── SMTP behind a PROXY protocol proxy ─────────────────────────────────────

const HEADER: &str = "PROXY TCP4 203.0.113.7 10.0.0.2 51000 25\r\n";

fn with_proxy(options: &mut SmtpServerOptions) {
    options.proxy_protocol = true;
}

#[tokio::test]
async fn takes_the_client_from_the_header_for_the_session_and_the_ehlo_reply() {
    let started = start_with(TestHandler::default(), with_proxy).await;
    let mut client = Client::connect(started.port).await;
    client.send(HEADER.as_bytes()).await;
    assert_eq!(reply(&mut client).await, "220 mx.test ESMTP");
    assert!(client.command("EHLO c").await.contains("[203.0.113.7]"));
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "250 Accepted");
    assert_eq!(client.command("DATA").await, "354 End data with <CR><LF>.<CR><LF>");
    client.send(format!("{MESSAGE}.\r\n").as_bytes()).await;
    assert_eq!(reply(&mut client).await, "250 OK: message queued");
    let delivery = started.delivery(0);
    assert_eq!(delivery.remote_address, "203.0.113.7");
    assert_eq!(delivery.remote_port, 51000);
}

#[tokio::test]
async fn drops_a_connection_that_does_not_start_with_a_header_without_reporting_it() {
    let started = start_with(TestHandler::default(), with_proxy).await;
    let mut client = Client::connect(started.port).await;
    client.send(b"EHLO c\r\n").await;
    assert_eq!(reply(&mut client).await, "<closed>");
    assert!(started.errors.lock().unwrap().is_empty());
}

#[tokio::test]
async fn still_refuses_a_client_that_talks_before_the_greeting() {
    let started = start_with(TestHandler::default(), with_proxy).await;
    let mut client = Client::connect(started.port).await;
    client.send(format!("{HEADER}EHLO early\r\n").as_bytes()).await;
    assert_eq!(reply(&mut client).await, "421 mx.test You talk too soon");
}

// ── SMTP over TLS behind a PROXY protocol proxy ────────────────────────────

#[tokio::test]
async fn runs_the_implicit_tls_handshake_after_the_header_even_sent_in_one_write_with_the_client_hello() {
    let started = start_with(TestHandler::default(), |options| {
        with_tls(options);
        options.secure = true;
        options.proxy_protocol = true;
    })
    .await;
    let mut client = Client::connect(started.port).await;
    assert_eq!(client.start_tls_behind(b"PROXY TCP6 2001:db8::7 fd00::2 51000 465\r\n").await, "secure");
    assert_eq!(reply(&mut client).await, "220 mx.test ESMTP");
    assert!(client.command("EHLO c").await.contains("[2001:db8::7]"));
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "250 Accepted");
    assert_eq!(client.command("DATA").await, "354 End data with <CR><LF>.<CR><LF>");
    client.send(format!("{MESSAGE}.\r\n").as_bytes()).await;
    assert_eq!(reply(&mut client).await, "250 OK: message queued");
    let delivery = started.delivery(0);
    assert_eq!(delivery.remote_address, "2001:db8::7");
    assert!(delivery.secure);
}

#[tokio::test]
async fn keeps_the_client_from_the_header_after_starttls() {
    let started = start_with(TestHandler::default(), |options| {
        with_tls(options);
        options.proxy_protocol = true;
    })
    .await;
    let mut client = Client::connect(started.port).await;
    client.send(b"PROXY TCP4 203.0.113.7 10.0.0.2 51000 587\r\n").await;
    assert_eq!(reply(&mut client).await, "220 mx.test ESMTP");
    client.command("EHLO c").await;
    assert_eq!(client.command("STARTTLS").await, "220 Ready to start TLS");
    assert_eq!(client.start_tls().await, "secure");
    client.command("EHLO c").await;
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "250 Accepted");
    assert_eq!(client.command("DATA").await, "354 End data with <CR><LF>.<CR><LF>");
    client.send(format!("{MESSAGE}.\r\n").as_bytes()).await;
    assert_eq!(reply(&mut client).await, "250 OK: message queued");
    let delivery = started.delivery(0);
    assert_eq!(delivery.remote_address, "203.0.113.7");
    assert!(delivery.secure);
}

// ── Hooks beyond what the Bun tests drive ──────────────────────────────────

/// The message can be handed to a parser that takes any `AsyncRead`, on any
/// task, and the session can cross tasks with the hook that holds it.
#[test]
fn the_data_stream_and_the_session_can_move_between_tasks() {
    fn readable<T: tokio::io::AsyncRead + Unpin + Send + Sync + 'static>() {}
    fn chunked<T: futures_util::Stream<Item = std::io::Result<bytes::Bytes>> + Unpin + Send + 'static>() {}
    fn movable<T: Send + 'static>() {}
    readable::<SmtpDataStream>();
    chunked::<SmtpDataStream>();
    movable::<SmtpSession>();
    movable::<SmtpServer>();
}

/// Holds the message to the listener's ceiling, as the Bun mail handling does
/// in front of its parser, and keeps what it read.
struct BoundedHandler(Arc<Mutex<Vec<Result<usize, String>>>>);

impl SmtpHandler for BoundedHandler {
    async fn on_auth(
        &self,
        _auth: SmtpAuthRequest,
        _session: &mut SmtpSession,
    ) -> Result<SmtpAuthResponse, SmtpHookError> {
        Ok(SmtpAuthResponse::reject())
    }

    async fn on_mail_from(&self, _address: &SmtpAddress, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        Ok(())
    }

    async fn on_rcpt_to(&self, _address: &SmtpAddress, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        Ok(())
    }

    async fn on_data(&self, mut stream: SmtpDataStream, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        stream.enforce_size_limit();
        let mut message = Vec::new();
        match stream.read_to_end(&mut message).await {
            Ok(read) => {
                self.0.lock().unwrap().push(Ok(read));
                Ok(())
            }
            Err(error) if SmtpDataStream::is_too_large(&error) => {
                self.0.lock().unwrap().push(Err(error.to_string()));
                let too_large = *error.get_ref().and_then(|inner| inner.downcast_ref::<SmtpDataTooLarge>()).unwrap();
                Err(too_large.into())
            }
            Err(error) => panic!("an unexpected stream error: {error}"),
        }
    }
}

#[tokio::test]
async fn refuses_a_message_past_the_ceiling_a_hook_enforces_once_the_message_is_read_out() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut options = options(TestHandler::default());
    options.handler = SharedSmtpHandler::new(BoundedHandler(seen.clone()));
    let server = create_smtp_server(options);
    let port = server.listen(0).await.unwrap().port();

    // Exactly the ceiling, counted after dot-unstuffing, passes.
    let mut client = transact(port).await;
    let line = format!("..{}\r\n", "a".repeat(1021));
    let body = line.repeat(SIZE as usize / 1024);
    client.send(body.as_bytes()).await;
    client.send(b".\r\n").await;
    assert_eq!(client.reply(Duration::from_secs(5)).await, "250 OK: message queued");

    // One byte more is refused, after the terminator, and the session goes on.
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "250 Accepted");
    assert_eq!(client.command("DATA").await, "354 End data with <CR><LF>.<CR><LF>");
    client.send(body.as_bytes()).await;
    client.send(b"x").await;
    assert_eq!(client.reply(Duration::from_millis(300)).await, "<timeout>");
    client.send(&b"more after the ceiling\r\n".repeat(10_000)).await;
    client.send(b".\r\n").await;
    assert_eq!(
        client.reply(Duration::from_secs(5)).await,
        format!("552 Error: message exceeds fixed maximum message size {SIZE}")
    );
    assert_eq!(client.command("NOOP").await, "250 OK");

    let message = format!("Error: message exceeds fixed maximum message size {SIZE}");
    assert_eq!(*seen.lock().unwrap(), vec![Ok(SIZE as usize), Err(message)]);
    drop(server.close());
}

/// What a hook stores on the session is there for the hooks that follow.
struct Stamped(u32);

struct StampingHandler(Arc<Mutex<Vec<Option<u32>>>>);

impl SmtpHandler for StampingHandler {
    async fn on_auth(
        &self,
        _auth: SmtpAuthRequest,
        session: &mut SmtpSession,
    ) -> Result<SmtpAuthResponse, SmtpHookError> {
        session.state.insert(Stamped(7));
        Ok(SmtpAuthResponse::accept("user"))
    }

    async fn on_mail_from(&self, _address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.0.lock().unwrap().push(session.state.get::<Stamped>().map(|stamped| stamped.0));
        if let Some(stamped) = session.state.get_mut::<Stamped>() {
            stamped.0 += 1;
        }
        Ok(())
    }

    async fn on_rcpt_to(&self, _address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.0.lock().unwrap().push(session.state.get::<Stamped>().map(|stamped| stamped.0));
        Ok(())
    }

    async fn on_data(&self, _stream: SmtpDataStream, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        panic!("this hook has a bug");
    }
}

#[tokio::test]
async fn keeps_what_a_hook_stores_on_the_session() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut options = options(TestHandler::default());
    options.allow_insecure_auth = true;
    options.handler = SharedSmtpHandler::new(StampingHandler(seen.clone()));
    let server = create_smtp_server(options);
    let port = server.listen(0).await.unwrap().port();
    let mut client = greeted(port).await;
    client.command("EHLO c").await;
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    client.command("RSET").await;
    assert_eq!(client.command(&format!("AUTH PLAIN {}", plain("user", "pass"))).await, "235 Authentication successful");
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<b@y.org>").await, "250 Accepted");
    assert_eq!(*seen.lock().unwrap(), vec![None, Some(7), Some(8)]);
    drop(server.close());
}

#[tokio::test]
async fn reports_a_hook_that_panics_and_drops_its_connection() {
    let mut options = options(TestHandler::default());
    options.handler = SharedSmtpHandler::new(StampingHandler(Arc::default()));
    let server = create_smtp_server(options);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let reported = errors.clone();
    server.on_error(move |error| reported.lock().unwrap().push(error.clone()));
    let port = server.listen(0).await.unwrap().port();
    let mut client = transact_on(port).await;
    assert_eq!(client.closed(Duration::from_secs(2)).await, "closed");
    let errors = errors.lock().unwrap().clone();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "SMTP hook panicked: this hook has a bug");
    assert_eq!(errors[0].remote_address.as_deref(), Some("127.0.0.1"));
    // The listener goes on serving.
    let mut next = greeted(port).await;
    assert_eq!(next.command("NOOP").await, "250 OK");
    drop(server.close());
}

/// As `transact`, up to the DATA command being sent.
async fn transact_on(port: u16) -> Client {
    let mut client = greeted(port).await;
    client.command("EHLO client").await;
    client.command("MAIL FROM:<a@x.org>").await;
    client.command("RCPT TO:<b@y.org>").await;
    client.send(b"DATA\r\n").await;
    client
}
