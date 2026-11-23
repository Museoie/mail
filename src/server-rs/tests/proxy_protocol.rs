//! PROXY protocol v1 as the mail listeners read it: the cases of
//! `src/server/lib/proxy-protocol.test.ts`.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use mail_server::proxy_protocol::{parse_proxy_header, read_proxy_header, Peer, Prefixed};

fn own() -> Peer {
    Peer { address: "10.0.0.2".to_string(), port: 40000 }
}

fn peer(address: &str, port: u16) -> Peer {
    Peer { address: address.to_string(), port }
}

// ── parseProxyHeader ───────────────────────────────────────────────────────

#[test]
fn reads_the_client_of_a_tcp4_and_a_tcp6_header() {
    assert_eq!(
        parse_proxy_header("PROXY TCP4 203.0.113.7 10.0.0.2 51000 25", &own()),
        Some(peer("203.0.113.7", 51000))
    );
    assert_eq!(
        parse_proxy_header("PROXY TCP6 2001:db8::7 fd00::2 51000 993", &own()),
        Some(peer("2001:db8::7", 51000))
    );
}

#[test]
fn keeps_the_connections_own_peer_for_unknown() {
    assert_eq!(parse_proxy_header("PROXY UNKNOWN", &own()), Some(own()));
    assert_eq!(parse_proxy_header("PROXY UNKNOWN ffff::1 ffff::2 1 2", &own()), Some(own()));
}

#[test]
fn refuses_anything_malformed() {
    for line in [
        "",
        "EHLO mx.example",
        "PROXY",
        "PROXY TCP4 203.0.113.7 10.0.0.2 51000",
        "PROXY TCP4 203.0.113.7 10.0.0.2 51000 25 extra",
        "PROXY TCP4 2001:db8::7 10.0.0.2 51000 25",
        "PROXY TCP6 203.0.113.7 fd00::2 51000 25",
        "PROXY TCP4 203.0.113.256 10.0.0.2 51000 25",
        "PROXY TCP4 203.0.113.7 10.0.0.2 65536 25",
        "PROXY TCP4 203.0.113.7 10.0.0.2 051000 25",
        "PROXY TCP4 203.0.113.7 10.0.0.2 -1 25",
        "PROXY UDP4 203.0.113.7 10.0.0.2 51000 25",
        "proxy TCP4 203.0.113.7 10.0.0.2 51000 25",
    ] {
        assert_eq!(parse_proxy_header(line, &own()), None, "{line:?}");
    }
}

// ── readProxyHeader ────────────────────────────────────────────────────────

/// The connection's own peer: a proxy on the loopback, which is trusted.
fn proxy() -> Peer {
    Peer { address: "127.0.0.1".to_string(), port: 40000 }
}

/// Reads the header off a connection the given writes arrive on, 20 ms
/// apart; then what the connection yields next, up to a line ending.
async fn open(writes: &[&str]) -> Result<(Peer, String), String> {
    let (mut client, mut server) = tokio::io::duplex(4096);
    let writes: Vec<String> = writes.iter().map(|write| write.to_string()).collect();
    let writing = tokio::spawn(async move {
        for write in writes {
            client.write_all(write.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        client
    });
    let outcome = read_proxy_header(&mut server, &proxy()).await;
    let client = writing.await.unwrap();
    let (peer, rest) = outcome.map_err(|error| error.to_string())?;
    // Whatever reads the connection next starts on the client's first byte.
    let mut after = Prefixed::new(rest, server);
    let mut text = Vec::new();
    while !text.ends_with(b"\n") {
        let mut byte = [0u8; 1];
        if after.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        text.push(byte[0]);
    }
    drop(client);
    Ok((peer, String::from_utf8(text).unwrap()))
}

#[tokio::test]
async fn returns_the_client_and_puts_back_what_followed_the_header_in_the_same_write() {
    let (client, after) = open(&["PROXY TCP4 203.0.113.7 10.0.0.2 51000 25\r\nEHLO early\r\n"]).await.unwrap();
    assert_eq!(client, peer("203.0.113.7", 51000));
    assert_eq!(after, "EHLO early\r\n");
}

#[tokio::test]
async fn reads_a_header_split_across_writes() {
    let (client, after) =
        open(&["PROXY TCP4 203.0.", "113.7 10.0.0.2 51000 25\r", "\nA1 CAPABILITY\r\n"]).await.unwrap();
    assert_eq!(client, peer("203.0.113.7", 51000));
    assert_eq!(after, "A1 CAPABILITY\r\n");
}

#[tokio::test]
async fn rejects_a_connection_that_starts_without_a_header() {
    assert_eq!(open(&["EHLO mx.example\r\n"]).await.unwrap_err(), "Malformed PROXY header");
}

#[tokio::test]
async fn rejects_a_header_past_the_specifications_length() {
    let long = format!("PROXY {}", "x".repeat(120));
    assert_eq!(open(&[&long]).await.unwrap_err(), "PROXY header too long");
}

#[tokio::test(start_paused = true)]
async fn rejects_when_no_header_arrives_in_time() {
    let (_client, mut server) = tokio::io::duplex(64);
    let error = read_proxy_header(&mut server, &proxy()).await.unwrap_err();
    assert_eq!(error.to_string(), "PROXY header timeout");
}

#[tokio::test]
async fn rejects_a_connection_that_closes_before_its_header() {
    let (client, mut server) = tokio::io::duplex(64);
    drop(client);
    let error = read_proxy_header(&mut server, &proxy()).await.unwrap_err();
    assert_eq!(error.to_string(), "Connection closed before its PROXY header");
}

#[tokio::test]
async fn rejects_a_connection_from_outside_the_private_networks_before_reading() {
    let (_client, mut server) = tokio::io::duplex(64);
    let outside = Peer { address: "8.8.8.8".to_string(), port: 1 };
    let error = read_proxy_header(&mut server, &outside).await.unwrap_err();
    assert_eq!(error.to_string(), "PROXY header from untrusted address 8.8.8.8");
}
