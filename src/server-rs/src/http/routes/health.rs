//! `GET /api/health`: the database, and every mail listener the
//! configuration calls for.

use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::http::request::HttpRequest;
use crate::http::respond::{json, Response};
use crate::postgres::pool;
use crate::proxy_protocol::is_proxy_protocol_enabled;
use crate::tls::is_tls_available;
use crate::{env, js};

// Behind a TCP proxy every connection must open with a PROXY header, so the
// probes send one too; UNKNOWN keeps the probe's own address.
const PROBE_HEADER: &[u8] = b"PROXY UNKNOWN\r\n";

async fn connect(port: u16, timeout: Duration) -> Option<TcpStream> {
    tokio::time::timeout(timeout, TcpStream::connect(("127.0.0.1", port))).await.ok()?.ok()
}

/// TCP check, for plain or STARTTLS ports: the port accepts a connection.
async fn check_port(port: u16) -> bool {
    let Some(mut socket) = connect(port, Duration::from_millis(1000)).await else {
        return false;
    };
    if is_proxy_protocol_enabled() {
        let _ = socket.write_all(PROBE_HEADER).await;
        let _ = socket.shutdown().await;
    }
    true
}

/// TLS check, for implicit TLS ports: completes the handshake and closes
/// cleanly, so the server does not log an aborted handshake.
async fn check_tls_port(port: u16) -> bool {
    let probe = async {
        let mut socket = connect(port, Duration::from_millis(3000)).await?;
        if is_proxy_protocol_enabled() {
            socket.write_all(PROBE_HEADER).await.ok()?;
        }
        let connector = crate::tls::insecure_connector();
        let name = rustls::pki_types::ServerName::try_from("localhost").ok()?;
        let mut stream = connector.connect(name, socket).await.ok()?;
        let _ = stream.shutdown().await;
        Some(())
    };
    tokio::time::timeout(Duration::from_millis(3000), probe).await.ok().flatten().is_some()
}

/// `process.env.X ? parseInt(X, 10) : default`, as a port to probe.
fn port_setting(name: &str, default: u16) -> Option<u16> {
    env::port(name, default)
}

/// `/api/health`, with `path` relative to that mount.
pub async fn handle_health(req: &mut HttpRequest, path: &str) -> anyhow::Result<Option<Response>> {
    if (req.method != "GET" && req.method != "HEAD") || path != "/" {
        return Ok(None);
    }

    let mut checks = Map::new();
    let mut all_healthy = true;
    let record = |checks: &mut Map<String, Value>, key: String, ok: bool| {
        checks.insert(key, json!(if ok { "ok" } else { "unhealthy" }));
        ok
    };

    let database_ok = pool().query("SELECT 1", &[]).await.is_ok();
    all_healthy &= record(&mut checks, "database".into(), database_ok);

    // This request proves the HTTP server is up.
    checks.insert("http".into(), json!("ok"));

    let label = |kind: &str, port: Option<u16>| match port {
        Some(port) => format!("{kind}:{port}"),
        None => format!("{kind}:NaN"),
    };
    let probe = |port: Option<u16>| async move {
        match port {
            Some(port) => check_port(port).await,
            None => false,
        }
    };
    let probe_tls = |port: Option<u16>| async move {
        match port {
            Some(port) => check_tls_port(port).await,
            None => false,
        }
    };

    let smtp_port = port_setting("SMTP_PORT", 25);
    all_healthy &= record(&mut checks, label("smtp", smtp_port), probe(smtp_port).await);

    let imap_port = port_setting("IMAP_PORT", 143);
    all_healthy &= record(&mut checks, label("imap", imap_port), probe(imap_port).await);

    // The TLS listeners only exist when a certificate is readable; without
    // one they are reported as not configured and left out of the rollup.
    let smtps_port = port_setting("SMTPS_PORT", 465);
    let submission_port = port_setting("SMTP_SUBMISSION_PORT", 587);
    let imap_tls_port = port_setting("IMAP_TLS_PORT", 993);
    if is_tls_available() {
        all_healthy &= record(&mut checks, label("smtp", smtps_port), probe_tls(smtps_port).await);
        all_healthy &= record(&mut checks, label("smtp", submission_port), probe(submission_port).await);
        all_healthy &= record(&mut checks, label("imap", imap_tls_port), probe_tls(imap_tls_port).await);
    } else {
        checks.insert(label("smtp", smtps_port), json!("not_configured"));
        checks.insert(label("smtp", submission_port), json!("not_configured"));
        checks.insert(label("imap", imap_tls_port), json!("not_configured"));
    }

    let body = json!({
        "status": if all_healthy { "success" } else { "error" },
        "body": { "healthy": all_healthy, "checks": checks, "timestamp": js::now_ms() },
    });
    Ok(Some(json(&body, if all_healthy { 200 } else { 503 })))
}
