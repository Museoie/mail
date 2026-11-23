//! The SMTP listeners this server starts: which ports open with which
//! options, and the registry that reaches their connections. The transport
//! cases of `src/server/lib/smtp.test.ts` ("initializeSmtp configuration",
//! "evictSmtpConnections"), against real listeners.
//!
//! They set environment variables, so they take turns.

mod smtp_support;

use std::time::Duration;

use tokio::sync::{Mutex, MutexGuard};

use mail_server::smtp::listeners::{MAX_MESSAGE_BYTES, SMTP_CLOSE_TIMEOUT_MS, SMTP_MAX_CLIENTS};
use mail_server::smtp::{
    close_smtp_servers, evict_smtp_connections, initialize_smtp, SharedSmtpHandler, SmtpAddress, SmtpAuthRequest,
    SmtpAuthResponse, SmtpDataStream, SmtpHandler, SmtpHookError, SmtpListeners, SmtpServer, SmtpSession,
};
use smtp_support::{generate_certificate, Client, TestCertificate};

const VARIABLES: [&str; 7] = [
    "SSL_CERTIFICATE",
    "SSL_CERTIFICATE_KEY",
    "SMTP_PORT",
    "SMTPS_PORT",
    "SMTP_SUBMISSION_PORT",
    "PROXY_PROTOCOL",
    "NODE_ENV",
];

static TURN: Mutex<()> = Mutex::const_new(());

/// The environment of one test: no certificate, free ports, quiet logs.
/// What was set before comes back when it goes.
struct Environment {
    saved: Vec<(&'static str, Option<String>)>,
    _turn: MutexGuard<'static, ()>,
}

impl Environment {
    async fn new() -> Environment {
        let turn = TURN.lock().await;
        let saved = VARIABLES.iter().map(|name| (*name, std::env::var(name).ok())).collect();
        for name in ["SSL_CERTIFICATE", "SSL_CERTIFICATE_KEY", "PROXY_PROTOCOL"] {
            std::env::remove_var(name);
        }
        for name in ["SMTP_PORT", "SMTPS_PORT", "SMTP_SUBMISSION_PORT"] {
            std::env::set_var(name, "0");
        }
        std::env::set_var("NODE_ENV", "test");
        Environment { saved, _turn: turn }
    }

    fn certificate(&self, certificate: &TestCertificate) {
        std::env::set_var("SSL_CERTIFICATE", &certificate.cert);
        std::env::set_var("SSL_CERTIFICATE_KEY", &certificate.key);
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// Accepts any user whose password is `pass`, and every address but `nobody@`.
struct Hooks;

impl SmtpHandler for Hooks {
    async fn on_auth(
        &self,
        auth: SmtpAuthRequest,
        _session: &mut SmtpSession,
    ) -> Result<SmtpAuthResponse, SmtpHookError> {
        match auth.password == "pass" {
            true => Ok(SmtpAuthResponse::accept(auth.username)),
            false => Ok(SmtpAuthResponse::reject()),
        }
    }

    async fn on_mail_from(&self, _address: &SmtpAddress, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        Ok(())
    }

    async fn on_rcpt_to(&self, address: &SmtpAddress, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        match address.address.starts_with("nobody@") {
            true => Err(SmtpHookError::with_code(550, "5.1.1 Error: no such mailbox here")),
            false => Ok(()),
        }
    }

    async fn on_data(&self, _stream: SmtpDataStream, _session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        Ok(())
    }
}

async fn initialize(single_sign_on: bool) -> anyhow::Result<Vec<SmtpServer>> {
    initialize_smtp(SmtpListeners { handler: SharedSmtpHandler::new(Hooks), single_sign_on }).await
}

fn port(server: &SmtpServer) -> u16 {
    server.address().expect("the server is listening").port()
}

async fn greeted(port: u16) -> Client {
    let mut client = Client::connect(port).await;
    assert!(client.reply(Duration::from_secs(2)).await.starts_with("220 "));
    client
}

fn plain(user: &str, pass: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"))
}

/// Ports that were free a moment ago.
async fn free_ports<const N: usize>() -> [u16; N] {
    let mut listeners = Vec::new();
    let mut ports = [0; N];
    for port in ports.iter_mut() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        *port = listener.local_addr().unwrap().port();
        listeners.push(listener);
    }
    ports
}

// ── initializeSmtp configuration ───────────────────────────────────────────

#[tokio::test]
async fn starts_one_plaintext_server_when_ssl_is_not_configured() {
    let _environment = Environment::new().await;
    let servers = initialize(false).await.unwrap();
    assert_eq!(servers.len(), 1);
    assert!(!servers[0].options().secure);
    assert!(!servers[0].tls_available());

    let mut client = greeted(port(&servers[0])).await;
    assert!(!client.command("EHLO c").await.contains("STARTTLS"));
    assert_eq!(client.command("STARTTLS").await, "454 Error: TLS not available");
    assert_eq!(
        client.command(&format!("AUTH PLAIN {}", plain("alice", "pass"))).await,
        "538 Error: Must issue a STARTTLS command first"
    );
    drop(client);
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn falls_back_to_plaintext_when_the_certificate_files_are_unreadable() {
    let _environment = Environment::new().await;
    std::env::set_var("SSL_CERTIFICATE", "/nonexistent/cert.pem");
    std::env::set_var("SSL_CERTIFICATE_KEY", "/nonexistent/key.pem");
    let servers = initialize(false).await.unwrap();
    assert_eq!(servers.len(), 1);
    assert!(!servers[0].tls_available());
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn starts_three_servers_when_the_certificate_exists() {
    let environment = Environment::new().await;
    let certificate = generate_certificate();
    environment.certificate(&certificate);
    let servers = initialize(false).await.unwrap();

    // One plaintext (25) + SMTPS (465) + submission (587) = 3
    assert_eq!(servers.len(), 3);
    assert_eq!(servers.iter().map(|server| server.options().secure).collect::<Vec<_>>(), [false, true, false]);
    assert!(servers.iter().all(SmtpServer::tls_available));

    // Port 25: STARTTLS on offer, AUTH only once it has run.
    let mut relay = greeted(port(&servers[0])).await;
    assert!(relay.command("EHLO c").await.contains("250-STARTTLS"));
    assert_eq!(
        relay.command(&format!("AUTH PLAIN {}", plain("alice", "pass"))).await,
        "538 Error: Must issue a STARTTLS command first"
    );
    assert_eq!(relay.command("STARTTLS").await, "220 Ready to start TLS");
    assert_eq!(relay.start_tls().await, "secure");
    relay.command("EHLO c").await;
    assert_eq!(
        relay.command(&format!("AUTH PLAIN {}", plain("alice", "pass"))).await,
        "235 Authentication successful"
    );

    // Port 465: the handshake comes first.
    let mut implicit = Client::connect(port(&servers[1])).await;
    assert_eq!(implicit.start_tls().await, "secure");
    assert!(implicit.reply(Duration::from_secs(2)).await.starts_with("220 "));
    assert!(!implicit.command("EHLO c").await.contains("STARTTLS"));

    // Port 587: STARTTLS on offer, and AUTH before it while single sign-on is off.
    let mut submission = greeted(port(&servers[2])).await;
    assert!(submission.command("EHLO c").await.contains("250-STARTTLS"));
    assert_eq!(
        submission.command(&format!("AUTH PLAIN {}", plain("alice", "pass"))).await,
        "235 Authentication successful"
    );
    drop((relay, implicit, submission));
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn uses_custom_smtp_port_smtps_port_and_smtp_submission_port_values() {
    let environment = Environment::new().await;
    let certificate = generate_certificate();
    environment.certificate(&certificate);
    let [smtp, smtps, submission] = free_ports().await;
    std::env::set_var("SMTP_PORT", smtp.to_string());
    std::env::set_var("SMTPS_PORT", smtps.to_string());
    std::env::set_var("SMTP_SUBMISSION_PORT", submission.to_string());

    let servers = initialize(false).await.unwrap();
    assert_eq!(servers.iter().map(port).collect::<Vec<_>>(), [smtp, smtps, submission]);
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn takes_auth_before_starttls_on_the_submission_port_only_while_single_sign_on_is_off() {
    let environment = Environment::new().await;
    let certificate = generate_certificate();
    environment.certificate(&certificate);

    let without = initialize(false).await.unwrap();
    let with = initialize(true).await.unwrap();
    // 25, 465 and 587 for each start: only the submission port ever takes
    // AUTH in cleartext, and only without single sign-on.
    let allowed = |servers: &[SmtpServer]| -> Vec<bool> {
        servers.iter().map(|server| server.options().allow_insecure_auth).collect()
    };
    assert_eq!(allowed(&without), [false, false, true]);
    assert_eq!(allowed(&with), [false, false, false]);
    close_smtp_servers(&without).await;
    close_smtp_servers(&with).await;
}

#[tokio::test]
async fn asks_every_listener_for_a_proxy_header_only_when_proxy_protocol_is_true() {
    let _environment = Environment::new().await;
    let mut asked = Vec::new();
    for setting in [None, Some("false"), Some("true")] {
        match setting {
            Some(value) => std::env::set_var("PROXY_PROTOCOL", value),
            None => std::env::remove_var("PROXY_PROTOCOL"),
        }
        let servers = initialize(false).await.unwrap();
        asked.push(servers[0].options().proxy_protocol);
        close_smtp_servers(&servers).await;
    }
    assert_eq!(asked, [false, false, true]);
}

// A recipient check the listener was never handed refuses nothing. Driving
// the listener is what ties the hooks to it.
#[tokio::test]
async fn hands_the_listeners_a_recipient_check_that_refuses_a_mailbox_it_does_not_hold() {
    let _environment = Environment::new().await;
    let servers = initialize(false).await.unwrap();
    let mut client = greeted(port(&servers[0])).await;
    client.command("EHLO c").await;
    assert_eq!(client.command("MAIL FROM:<a@x.org>").await, "250 Accepted");
    assert_eq!(client.command("RCPT TO:<nobody@test.com>").await, "550 5.1.1 Error: no such mailbox here");
    assert_eq!(client.command("RCPT TO:<someone@test.com>").await, "250 Accepted");
    drop(client);
    close_smtp_servers(&servers).await;
}

// `size` is what makes the listener advertise `SIZE` in EHLO and refuse a
// `MAIL FROM` that declares more — the half a sending MTA acts on.
#[tokio::test]
async fn declares_the_message_ceiling_and_the_limits_on_every_listener_it_starts() {
    let environment = Environment::new().await;
    let certificate = generate_certificate();
    environment.certificate(&certificate);
    let servers = initialize(false).await.unwrap();
    assert_eq!(servers.len(), 3);
    assert_eq!(MAX_MESSAGE_BYTES, 35 * 1024 * 1024);
    for server in &servers {
        assert_eq!(server.options().size, MAX_MESSAGE_BYTES);
        assert_eq!(server.options().max_clients, SMTP_MAX_CLIENTS);
        assert_eq!(server.options().close_timeout, Duration::from_millis(SMTP_CLOSE_TIMEOUT_MS));
        assert_eq!(server.options().socket_timeout, None);
        assert_eq!(server.options().name, None);
    }
    assert_eq!((SMTP_MAX_CLIENTS, SMTP_CLOSE_TIMEOUT_MS), (100, 5000));

    let mut client = greeted(port(&servers[0])).await;
    assert!(client.command("EHLO c").await.ends_with("250 SIZE 36700160"));
    assert_eq!(
        client.command("MAIL FROM:<a@x.org> SIZE=36700161").await,
        "552 Error: message exceeds fixed maximum message size 36700160"
    );
    drop(client);
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn does_not_start_with_a_certificate_it_cannot_use() {
    let _environment = Environment::new().await;
    let directory = tempfile::tempdir().unwrap();
    let (cert, key) = (directory.path().join("cert.pem"), directory.path().join("key.pem"));
    std::fs::write(&cert, "DUMMY CERT").unwrap();
    std::fs::write(&key, "DUMMY KEY").unwrap();
    std::env::set_var("SSL_CERTIFICATE", &cert);
    std::env::set_var("SSL_CERTIFICATE_KEY", &key);

    let error = initialize(false).await.err().expect("a certificate that is not one stops the start");
    assert!(error.to_string().contains("SSL certificate could not be loaded"), "{error}");
}

#[tokio::test]
async fn reports_a_port_it_cannot_listen_on() {
    let _environment = Environment::new().await;
    let taken = tokio::net::TcpListener::bind(("0.0.0.0", 0)).await.unwrap();
    std::env::set_var("SMTP_PORT", taken.local_addr().unwrap().port().to_string());
    let error = initialize(false).await.err().expect("a taken port stops the start");
    assert!(error.to_string().contains("could not listen on port"), "{error}");

    std::env::set_var("SMTP_PORT", "not a port");
    let error = initialize(false).await.err().expect("a port that is not a number stops the start");
    assert!(error.to_string().contains("SMTP_PORT"), "{error}");
}

// ── evictSmtpConnections ───────────────────────────────────────────────────

async fn authenticated(port: u16, user: &str) -> Client {
    let mut client = greeted(port).await;
    client.command("EHLO c").await;
    assert_eq!(client.command(&format!("AUTH PLAIN {}", plain(user, "pass"))).await, "235 Authentication successful");
    client
}

/// A listener that takes AUTH in cleartext: the submission port.
async fn submission(environment: &Environment) -> (Vec<SmtpServer>, u16, TestCertificate) {
    let certificate = generate_certificate();
    environment.certificate(&certificate);
    let servers = initialize(false).await.unwrap();
    let port = port(&servers[2]);
    (servers, port, certificate)
}

#[tokio::test]
async fn closes_only_the_connections_authenticated_as_the_rotated_username() {
    let environment = Environment::new().await;
    let (servers, port, _certificate) = submission(&environment).await;
    let mut alice = authenticated(port, "alice").await;
    let mut other_alice = authenticated(port, "alice").await;
    let mut bob = authenticated(port, "bob").await;
    let mut anonymous = greeted(port).await;

    assert_eq!(evict_smtp_connections("alice"), 2);

    for client in [&mut alice, &mut other_alice] {
        assert_eq!(client.reply(Duration::from_secs(2)).await, "421 Credential changed, please re-authenticate");
        assert_eq!(client.closed(Duration::from_secs(2)).await, "closed");
    }
    assert_eq!(bob.command("NOOP").await, "250 OK");
    assert_eq!(anonymous.command("NOOP").await, "250 OK");
    drop((alice, other_alice, bob, anonymous));
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn reaches_the_connections_of_every_listening_port() {
    let environment = Environment::new().await;
    let (servers, submission_port, _certificate) = submission(&environment).await;
    let mut on_submission = authenticated(submission_port, "carol").await;
    let mut on_implicit = Client::connect(port(&servers[1])).await;
    assert_eq!(on_implicit.start_tls().await, "secure");
    on_implicit.reply(Duration::from_secs(2)).await;
    on_implicit.command("EHLO c").await;
    assert_eq!(
        on_implicit.command(&format!("AUTH PLAIN {}", plain("carol", "pass"))).await,
        "235 Authentication successful"
    );

    assert_eq!(evict_smtp_connections("carol"), 2);
    for client in [&mut on_submission, &mut on_implicit] {
        assert_eq!(client.reply(Duration::from_secs(2)).await, "421 Credential changed, please re-authenticate");
    }
    drop((on_submission, on_implicit));
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn keeps_sweeping_past_a_connection_that_is_already_gone() {
    let environment = Environment::new().await;
    let (servers, port, _certificate) = submission(&environment).await;
    let mut gone = authenticated(port, "dave").await;
    let mut healthy = authenticated(port, "dave").await;
    gone.destroy();

    assert!(evict_smtp_connections("dave") >= 1);
    assert_eq!(healthy.reply(Duration::from_secs(2)).await, "421 Credential changed, please re-authenticate");
    drop(healthy);
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn reports_nothing_evicted_when_no_connection_matches() {
    let environment = Environment::new().await;
    let (servers, port, _certificate) = submission(&environment).await;
    let mut bob = authenticated(port, "bob").await;
    assert_eq!(evict_smtp_connections("alice"), 0);
    assert_eq!(evict_smtp_connections(""), 0);
    assert_eq!(bob.command("NOOP").await, "250 OK");
    drop(bob);
    close_smtp_servers(&servers).await;
}

#[tokio::test]
async fn stops_reaching_a_server_that_has_closed() {
    let environment = Environment::new().await;
    let (servers, port, _certificate) = submission(&environment).await;
    let mut alice = authenticated(port, "erin").await;

    // The client leaves, the listeners close: the registry lets go of them.
    alice.destroy();
    close_smtp_servers(&servers).await;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err());
    assert_eq!(evict_smtp_connections("erin"), 0);
}
