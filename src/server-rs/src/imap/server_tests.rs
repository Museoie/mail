//! Tests of the listeners: index.test.ts, proxy.test.ts and shutdown.test.ts.

use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context as TaskContext, Poll};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};

use super::super::close_socket::CLOSE_FLUSH_TIMEOUT_MS;
use super::super::handler::ImapRequestHandler;
use super::super::session::ImapSession;
use super::super::session_registry::{get_connected_session_count, reset_connected_sessions};
use super::super::test_support;
use super::*;

const BYE: &str = "* BYE Server shutting down\r\n";
const HEADER: &str = "PROXY TCP4 203.0.113.7 10.0.0.2 51000 143\r\n";

/// A listener on an ephemeral loopback port.
async fn serve_with(tls: Option<AcceptorSource>, proxied: bool, serve: Serve) -> (ImapServers, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (ImapServers { servers: vec![start(listener, tls, proxied, serve)] }, port)
}

/// A plain IMAP listener on an ephemeral loopback port.
async fn serve(proxied: bool) -> (ImapServers, u16) {
    serve_with(None, proxied, serve_imap()).await
}

/// The test certificate, as the listener takes it.
fn test_acceptor() -> Option<AcceptorSource> {
    let acceptor = test_support::acceptor()?;
    Some(Arc::new(move || Ok(acceptor.clone())))
}

async fn connect(port: u16) -> BufReader<TcpStream> {
    BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap())
}

async fn line(client: &mut (impl AsyncBufReadExt + Unpin)) -> String {
    let mut line = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), client.read_line(&mut line)).await;
    line
}

/// What a connection looked like when it was handed over.
struct Seen {
    peer: Peer,
    secure: bool,
    /// The first line the client sent once it was handed over.
    line: Arc<Mutex<Option<String>>>,
}

/// A stand-in for IMAP that records each connection it is handed, greets
/// it with `* OK ready` and reads its first line.
fn recording() -> (Serve, Arc<Mutex<Vec<Seen>>>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let serve: Serve = Arc::new({
        let seen = seen.clone();
        move |mut stream: BoxedStream, peer, secure| {
            let first = Arc::new(Mutex::new(None));
            seen.lock().unwrap().push(Seen { peer, secure, line: first.clone() });
            Box::pin(async move {
                let _ = stream.write_all(b"* OK ready\r\n").await;
                let _ = stream.flush().await;
                let mut reader = BufReader::new(stream);
                let mut received = String::new();
                if reader.read_line(&mut received).await.is_ok() {
                    *first.lock().unwrap() = Some(received);
                }
                // Held open until the client goes.
                let mut rest = Vec::new();
                let _ = reader.read_to_end(&mut rest).await;
            })
        }
    });
    (serve, seen)
}

async fn first_line_of(seen: &Arc<Mutex<Vec<Seen>>>) -> Option<String> {
    for _ in 0..200 {
        if let Some(line) = seen.lock().unwrap().first().and_then(|seen| seen.line.lock().unwrap().clone()) {
            return Some(line);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// A client stream whose first write carries the PROXY header in front of
/// what the client writes, so the header and the ClientHello travel in one
/// segment.
struct HeaderInFront {
    inner: TcpStream,
    header: Option<Vec<u8>>,
}

impl AsyncRead for HeaderInFront {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for HeaderInFront {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let Some(header) = self.header.clone() else {
            return Pin::new(&mut self.inner).poll_write(cx, buf);
        };
        let joined = [header.as_slice(), buf].concat();
        match Pin::new(&mut self.inner).poll_write(cx, &joined) {
            Poll::Ready(Ok(written)) => {
                // A fresh loopback socket takes a few hundred octets whole.
                assert!(written > header.len(), "the header and the first write went out together");
                self.header = None;
                Poll::Ready(Ok(written - header.len()))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// index.test.ts
// ---------------------------------------------------------------------------

#[test]
fn ports_follow_number_or_default() {
    assert_eq!(port_setting(None, 143).unwrap(), 143);
    assert_eq!(port_setting(Some("1143"), 143).unwrap(), 1143);
    assert_eq!(port_setting(Some(" 1143 "), 143).unwrap(), 1143);
    // Not a number at all, or zero: the default.
    assert_eq!(port_setting(Some("not-a-port"), 143).unwrap(), 143);
    assert_eq!(port_setting(Some("143abc"), 993).unwrap(), 993);
    assert_eq!(port_setting(Some(""), 993).unwrap(), 993);
    assert_eq!(port_setting(Some("0"), 143).unwrap(), 143);
    // A number that is no port cannot be listened on.
    assert!(port_setting(Some("70000"), 143).is_err());
    assert!(port_setting(Some("-1"), 143).is_err());
    assert!(port_setting(Some("143.5"), 143).is_err());
}

mod index_parsers {
    use serde_json::{json, Value};

    use super::super::super::parsers::command_parser::parse_command;

    /// The parsed command as the Bun server's JSON has it, with its tag.
    fn parsed(line: &str) -> (String, Value, usize) {
        let result = parse_command(line, None);
        assert!(result.success, "{line}");
        let consumed = result.consumed;
        let parsed = result.value.unwrap();
        (parsed.tag, serde_json::from_str(&parsed.request.to_json_string()).unwrap(), consumed)
    }

    #[test]
    fn parses_a_silent_uid_store() {
        let (tag, request, consumed) = parsed("1.1 UID STORE 1 +FLAGS.SILENT (\\Seen)");
        assert_eq!(tag, "1.1");
        assert_eq!(consumed, 37);
        assert_eq!(request["type"], "UID");
        assert_eq!(request["data"]["command"], "STORE");
        let store = &request["data"]["request"]["data"];
        assert_eq!(store["operation"], "+FLAGS.SILENT");
        assert_eq!(store["flags"], json!(["\\Seen"]));
        assert_eq!(store["silent"], true);
    }

    #[test]
    fn parses_a_uid_store_of_answered() {
        let (tag, request, _) = parsed("1.2 UID STORE 1 +FLAGS (\\Answered)");
        assert_eq!(tag, "1.2");
        let store = &request["data"]["request"]["data"];
        assert_eq!(store["operation"], "+FLAGS");
        assert_eq!(store["flags"], json!(["\\Answered"]));
        assert_eq!(store["silent"], false);
    }

    #[test]
    fn parses_a_fetch_of_a_range() {
        let (tag, request, _) = parsed("2.1 FETCH 1:15 (FLAGS ENVELOPE)");
        assert_eq!(tag, "2.1");
        assert_eq!(request["type"], "FETCH");
        assert_eq!(request["data"]["sequenceSet"]["ranges"][0], json!({ "start": 1, "end": 15 }));
    }

    #[test]
    fn parses_a_uid_search_of_comma_separated_numbers() {
        let (tag, request, _) = parsed("3.1 UID SEARCH 1,577,5084,9591");
        assert_eq!(tag, "3.1");
        let criterion = &request["data"]["request"]["data"]["criteria"][0];
        assert_eq!(criterion["type"], "SEQ");
        assert_eq!(
            criterion["sequenceSet"],
            json!({ "type": "sequence", "ranges": [{ "start": 1 }, { "start": 577 }, { "start": 5084 }, { "start": 9591 }] })
        );
    }

    #[test]
    fn parses_a_uid_search_with_a_uid_range_to_the_end() {
        let (tag, request, _) = parsed("4.1 UID SEARCH UID 5091:*");
        assert_eq!(tag, "4.1");
        let criterion = &request["data"]["request"]["data"]["criteria"][0];
        assert_eq!(criterion["type"], "UID");
        assert_eq!(criterion["sequenceSet"], json!({ "type": "sequence", "ranges": [{ "start": 5091, "end": 9007199254740991_i64 }] }));
    }

    #[test]
    fn parses_a_fetch_to_the_end() {
        let (tag, request, _) = parsed("5.1 FETCH 1:* (FLAGS)");
        assert_eq!(tag, "5.1");
        assert_eq!(request["data"]["sequenceSet"]["ranges"][0], json!({ "start": 1, "end": 9007199254740991_i64 }));
    }

    #[test]
    fn parses_a_store_to_the_end() {
        let (tag, request, _) = parsed("6.1 STORE 1:* +FLAGS (\\Seen)");
        assert_eq!(tag, "6.1");
        assert_eq!(request["type"], "STORE");
        assert_eq!(request["data"]["sequenceSet"]["ranges"][0], json!({ "start": 1, "end": 9007199254740991_i64 }));
    }

    #[test]
    fn parses_a_copy_of_one_message() {
        let (tag, request, _) = parsed("7.1 COPY 123 \"INBOX/Archive\"");
        assert_eq!(tag, "7.1");
        assert_eq!(request["data"]["sequenceSet"]["ranges"][0], json!({ "start": 123 }));
        assert_eq!(request["data"]["mailbox"], "INBOX/Archive");
    }

    #[test]
    fn parses_a_uid_copy_of_a_range() {
        let (tag, request, _) = parsed("8.1 UID COPY 1:10 \"Sent Messages/Archive\"");
        assert_eq!(tag, "8.1");
        assert_eq!(request["data"]["command"], "COPY");
        let copy = &request["data"]["request"]["data"];
        assert_eq!(copy["sequenceSet"]["ranges"][0], json!({ "start": 1, "end": 10 }));
        assert_eq!(copy["mailbox"], "Sent Messages/Archive");
    }

    #[test]
    fn parses_a_copy_of_several_messages() {
        let (tag, request, _) = parsed("9.1 COPY 1,5,10 \"INBOX\"");
        assert_eq!(tag, "9.1");
        assert_eq!(request["data"]["sequenceSet"]["ranges"], json!([{ "start": 1 }, { "start": 5 }, { "start": 10 }]));
        assert_eq!(request["data"]["mailbox"], "INBOX");
    }
}

#[tokio::test]
async fn greets_and_serves_a_connection() {
    let _exclusive = test_support::exclusive().await;
    let (servers, port) = serve(false).await;
    let mut client = connect(port).await;
    assert!(line(&mut client).await.starts_with("* OK [CAPABILITY IMAP4rev1 "));
    client.get_mut().write_all(b"a1 NOOP\r\n").await.unwrap();
    assert_eq!(line(&mut client).await, "a1 OK NOOP completed\r\n");
    assert!(close_imap_servers(servers).await);
}

#[tokio::test]
async fn the_connection_past_the_limit_is_closed_without_a_greeting() {
    let _exclusive = test_support::exclusive().await;
    let (servers, port) = serve(false).await;
    let mut clients = Vec::new();
    for _ in 0..IMAP_MAX_CONNECTIONS {
        let mut client = connect(port).await;
        assert!(line(&mut client).await.starts_with("* OK"));
        clients.push(client);
    }
    let mut extra = connect(port).await;
    let mut said = String::new();
    // Closed at once: end of stream, or a reset, and not one octet.
    let _ = extra.read_to_string(&mut said).await;
    assert_eq!(said, "");

    // A slot comes back when a connection leaves.
    drop(clients.pop());
    let mut greeted = false;
    for _ in 0..50 {
        let mut next = connect(port).await;
        let mut greeting = String::new();
        if next.read_line(&mut greeting).await.is_ok() && greeting.starts_with("* OK") {
            greeted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(greeted);
    assert!(close_imap_servers(servers).await);
}

// ---------------------------------------------------------------------------
// proxy.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hands_a_connection_straight_to_imap_when_no_proxy_is_in_front() {
    let (serve, seen) = recording();
    let (_servers, port) = serve_with(None, false, serve).await;
    let mut client = connect(port).await;
    assert_eq!(line(&mut client).await, "* OK ready\r\n");
    assert_eq!(seen.lock().unwrap()[0].peer.address, "127.0.0.1");
}

#[tokio::test]
async fn waits_for_the_proxy_header_when_a_proxy_is_in_front() {
    let (serve, seen) = recording();
    let (_servers, port) = serve_with(None, true, serve).await;
    let mut client = connect(port).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(seen.lock().unwrap().is_empty());
    client.get_mut().write_all(HEADER.as_bytes()).await.unwrap();
    assert_eq!(line(&mut client).await, "* OK ready\r\n");
    assert_eq!(seen.lock().unwrap()[0].peer.address, "203.0.113.7");
}

#[tokio::test]
async fn hands_over_the_socket_reporting_the_headers_client_with_what_followed_the_header() {
    let (serve, seen) = recording();
    let (_servers, port) = serve_with(None, true, serve).await;
    let mut client = connect(port).await;
    client.get_mut().write_all(format!("{HEADER}A1 NOOP\r\n").as_bytes()).await.unwrap();
    assert_eq!(line(&mut client).await, "* OK ready\r\n");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].peer, Peer { address: "203.0.113.7".into(), port: 51000 });
        assert!(!seen[0].secure);
    }
    assert_eq!(first_line_of(&seen).await.as_deref(), Some("A1 NOOP\r\n"));
}

#[tokio::test]
async fn closes_a_connection_that_does_not_start_with_a_header_never_reaching_imap() {
    let (serve, seen) = recording();
    let (_servers, port) = serve_with(None, true, serve).await;
    let mut client = connect(port).await;
    client.get_mut().write_all(b"A1 CAPABILITY\r\n").await.unwrap();
    let mut said = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut said)).await;
    assert_eq!(said, "");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_proxied_imap_listener_waits_for_the_header_and_then_serves_imap() {
    let _exclusive = test_support::exclusive().await;
    let (servers, port) = serve(true).await;
    let mut client = connect(port).await;
    // Nothing is said before the header.
    let early = tokio::time::timeout(Duration::from_millis(100), client.read_line(&mut String::new())).await;
    assert!(early.is_err());
    client.get_mut().write_all(b"PROXY TCP4 203.0.113.7 10.0.0.1 51000 143\r\nA1 NOOP\r\n").await.unwrap();
    assert!(line(&mut client).await.starts_with("* OK [CAPABILITY "));
    assert_eq!(line(&mut client).await, "A1 OK NOOP completed\r\n");
    assert!(close_imap_servers(servers).await);
}

#[tokio::test]
async fn runs_the_implicit_tls_handshake_after_the_header_even_sent_in_one_write_with_the_client_hello() {
    let Some(tls) = test_acceptor() else {
        return;
    };
    let (serve, seen) = recording();
    let (_servers, port) = serve_with(Some(tls), true, serve).await;
    let raw = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let raw = HeaderInFront { inner: raw, header: Some(b"PROXY TCP6 2001:db8::7 fd00::2 51000 993\r\n".to_vec()) };
    let client = crate::tls::insecure_connector().connect(test_support::server_name(), raw).await.expect("the handshake");
    let mut client = BufReader::new(client);
    assert_eq!(line(&mut client).await, "* OK ready\r\n");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].peer.address, "2001:db8::7");
        assert!(seen[0].secure);
    }
    client.get_mut().write_all(b"A1 NOOP\r\n").await.unwrap();
    client.get_mut().flush().await.unwrap();
    assert_eq!(first_line_of(&seen).await.as_deref(), Some("A1 NOOP\r\n"));
}

#[tokio::test]
async fn runs_the_tls_handshake_after_the_header_on_the_implicit_tls_listener_behind_a_proxy() {
    let Some(tls) = test_acceptor() else {
        return;
    };
    let (serve, seen) = recording();
    let (_servers, port) = serve_with(Some(tls), true, serve).await;
    let mut raw = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    raw.write_all(b"PROXY TCP4 203.0.113.7 10.0.0.2 51000 993\r\n").await.unwrap();
    let client = crate::tls::insecure_connector().connect(test_support::server_name(), raw).await.expect("the handshake");
    let mut client = BufReader::new(client);
    assert_eq!(line(&mut client).await, "* OK ready\r\n");
    let seen = seen.lock().unwrap();
    assert!(seen[0].secure);
    assert_eq!(seen[0].peer.address, "203.0.113.7");
}

#[tokio::test]
async fn serves_tls_from_the_first_byte_without_a_proxy() {
    let Some(tls) = test_acceptor() else {
        return;
    };
    let (serve, seen) = recording();
    let (_servers, port) = serve_with(Some(tls), false, serve).await;
    let raw = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let client = crate::tls::insecure_connector().connect(test_support::server_name(), raw).await.expect("the handshake");
    let mut client = BufReader::new(client);
    assert_eq!(line(&mut client).await, "* OK ready\r\n");
    let seen = seen.lock().unwrap();
    assert!(seen[0].secure);
    assert_eq!(seen[0].peer.address, "127.0.0.1");
}

#[tokio::test]
async fn an_implicit_tls_listener_serves_imap_inside_tls() {
    let _exclusive = test_support::exclusive().await;
    let Some(tls) = test_acceptor() else {
        return;
    };
    let (servers, port) = serve_with(Some(tls), false, serve_imap()).await;
    let raw = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let client = crate::tls::insecure_connector().connect(test_support::server_name(), raw).await.expect("the handshake");
    let mut client = BufReader::new(client);
    let greeting = line(&mut client).await;
    // Inside TLS there is no STARTTLS to offer.
    assert!(greeting.starts_with("* OK [CAPABILITY IMAP4rev1 "), "{greeting:?}");
    assert!(!greeting.contains("STARTTLS"));
    client.get_mut().write_all(b"a1 STARTTLS\r\n").await.unwrap();
    client.get_mut().flush().await.unwrap();
    assert_eq!(line(&mut client).await, "a1 BAD STARTTLS not permitted on a TLS connection\r\n");
    assert!(close_imap_servers(servers).await);
}

// ---------------------------------------------------------------------------
// shutdown.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_tells_an_idle_client_bye_and_closes_promptly() {
    let _exclusive = test_support::exclusive().await;
    reset_connected_sessions();
    let (servers, port) = serve(false).await;
    let mut client = connect(port).await;
    assert!(line(&mut client).await.starts_with("* OK"));
    assert_eq!(get_connected_session_count(), 1);

    // A client that closes its side as soon as the server's FIN arrives.
    let reading = tokio::spawn(async move {
        let mut rest = String::new();
        let _ = client.read_to_string(&mut rest).await;
        rest
    });
    let started = std::time::Instant::now();
    let drained = close_imap_servers(servers).await;
    let elapsed = started.elapsed();

    // Without the sweep the listener would wait out the inactivity timeout
    // and this would come back false at the bound.
    assert!(drained);
    // Such a client goes as soon as the BYE lands, without the flush window.
    assert!(elapsed < Duration::from_millis(CLOSE_FLUSH_TIMEOUT_MS), "{elapsed:?}");
    assert_eq!(reading.await.unwrap(), BYE);
    // The port is closed.
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(get_connected_session_count(), 0);
}

#[tokio::test]
async fn shutdown_closes_within_the_flush_window_when_the_client_keeps_its_side_open() {
    let _exclusive = test_support::exclusive().await;
    let (servers, port) = serve(false).await;
    let mut client = connect(port).await;
    assert!(line(&mut client).await.starts_with("* OK"));

    // The client reads the BYE but never closes its own side.
    let started = std::time::Instant::now();
    let drained = close_imap_servers(servers).await;
    assert!(drained);
    assert!(started.elapsed() < Duration::from_millis(IMAP_SHUTDOWN_TIMEOUT_MS));
    assert_eq!(line(&mut client).await, BYE);
}

#[tokio::test]
async fn shutdown_sends_an_idle_session_one_bye_when_the_idle_sweep_already_closed_it() {
    let _exclusive = test_support::exclusive().await;
    // The IDLE manager says BYE to its sessions first; the connection sweep
    // must not say it a second time.
    let session: Arc<Mutex<Option<Arc<ImapSession>>>> = Arc::default();
    let serve: Serve = Arc::new({
        let session = session.clone();
        move |stream, peer, is_tls| {
            let handler = ImapRequestHandler::new(stream, peer, is_tls);
            *session.lock().unwrap() = Some(handler.session().clone());
            Box::pin(handler.run())
        }
    });
    let (servers, port) = serve_with(None, false, serve).await;
    let mut client = connect(port).await;
    assert!(line(&mut client).await.starts_with("* OK"));
    let idling = session.lock().unwrap().clone().unwrap();
    idle_manager().add_idle_session("shutdown-idle", idling, "t1", "INBOX", "alice");

    idle_manager().shutdown();
    let drained = close_imap_servers(servers).await;

    assert!(drained);
    let mut rest = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut rest)).await;
    assert_eq!(rest.matches("* BYE").count(), 1, "{rest:?}");
}

#[tokio::test]
async fn shutdown_stops_waiting_at_the_bound_on_a_connection_no_session_owns() {
    let _exclusive = test_support::exclusive().await;
    // Behind a proxy a connection has no session until its PROXY header
    // arrives; this one never sends it.
    let (servers, port) = serve(true).await;
    let _silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    let drained = close_imap_servers(servers).await;
    assert!(!drained);
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(IMAP_SHUTDOWN_TIMEOUT_MS - 50), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(IMAP_SHUTDOWN_TIMEOUT_MS + 1000), "{elapsed:?}");
}
