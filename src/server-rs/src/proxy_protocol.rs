//! PROXY protocol v1 on the mail ports.

use std::net::IpAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

use crate::env;
use crate::http::request::trust_loopback_and_private;

/// Where a connection came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub address: String,
    pub port: u16,
}

/// Whether every mail connection (SMTP and IMAP) starts with a PROXY protocol
/// v1 header naming the client, as a TCP proxy in front of the server sends
/// it. The header is then required.
pub fn is_proxy_protocol_enabled() -> bool {
    env::var("PROXY_PROTOCOL").as_deref() == Some("true")
}

/// The longest v1 header the specification allows, CRLF included.
const MAX_HEADER_BYTES: usize = 107;
const HEADER_TIMEOUT: Duration = Duration::from_millis(5000);

/// An address without the IPv4-mapped IPv6 prefix.
pub fn unmapped(address: &str) -> String {
    match address.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("::ffff:") => address[7..].to_string(),
        _ => address.to_string(),
    }
}

fn is_port(value: &str) -> bool {
    let well_formed = value == "0"
        || (!value.is_empty()
            && value.len() <= 5
            && !value.starts_with('0')
            && value.bytes().all(|b| b.is_ascii_digit()));
    well_formed && value.parse::<u32>().is_ok_and(|port| port <= 65535)
}

fn ip_version(address: &str) -> u8 {
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => 4,
        Ok(IpAddr::V6(_)) => 6,
        Err(_) => 0,
    }
}

/// Reads the client from a v1 header line (without its CRLF). `UNKNOWN` means
/// the proxy has no client to name, such as its own health check, so the
/// connection's own peer stands. `None` for anything malformed.
pub fn parse_proxy_header(line: &str, own: &Peer) -> Option<Peer> {
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.first() != Some(&"PROXY") {
        return None;
    }
    if parts.get(1) == Some(&"UNKNOWN") {
        return Some(own.clone());
    }
    let [_, family, source, destination, source_port, destination_port] = parts[..] else {
        return None;
    };
    let version = match family {
        "TCP4" => 4,
        "TCP6" => 6,
        _ => return None,
    };
    if ip_version(source) != version || ip_version(destination) != version {
        return None;
    }
    if !is_port(source_port) || !is_port(destination_port) {
        return None;
    }
    Some(Peer { address: unmapped(source), port: source_port.parse().ok()? })
}

/// A stream that first yields bytes already read off it, then the stream.
pub struct Prefixed<S> {
    prefix: Bytes,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Bytes, inner: S) -> Prefixed<S> {
        Prefixed { prefix, inner }
    }

    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let take = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix[..take]);
            self.prefix.advance(take);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Waits for the PROXY header at the start of `stream` and returns the client
/// it names, with the bytes that followed the header, which belong to
/// whatever reads the stream next (wrap the stream in [`Prefixed`]).
///
/// Fails when the connection is not from a trusted proxy, the header is
/// malformed or too long, or it does not arrive in time.
pub async fn read_proxy_header<S: AsyncRead + Unpin>(stream: &mut S, own: &Peer) -> Result<(Peer, Bytes)> {
    // A client anywhere else could name itself; only the proxy's network may.
    if !trust_loopback_and_private(&own.address) {
        bail!("PROXY header from untrusted address {}", own.address);
    }
    let read = async {
        let mut buffered: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                bail!("Connection closed before its PROXY header");
            }
            buffered.extend_from_slice(&chunk[..n]);
            let end = buffered.windows(2).position(|pair| pair == b"\r\n");
            match end {
                Some(end) if end + 2 <= MAX_HEADER_BYTES => {
                    let line: String = buffered[..end].iter().map(|&b| b as char).collect();
                    let peer = parse_proxy_header(&line, own).ok_or_else(|| anyhow!("Malformed PROXY header"))?;
                    return Ok((peer, Bytes::copy_from_slice(&buffered[end + 2..])));
                }
                _ if buffered.len() >= MAX_HEADER_BYTES => bail!("PROXY header too long"),
                _ => {}
            }
        }
    };
    match tokio::time::timeout(HEADER_TIMEOUT, read).await {
        Ok(result) => result,
        Err(_) => bail!("PROXY header timeout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own() -> Peer {
        Peer { address: "127.0.0.1".into(), port: 40000 }
    }

    #[test]
    fn header_lines() {
        let peer = parse_proxy_header("PROXY TCP4 203.0.113.7 10.0.0.1 51234 25", &own());
        assert_eq!(peer, Some(Peer { address: "203.0.113.7".into(), port: 51234 }));
        let peer = parse_proxy_header("PROXY TCP6 2001:db8::1 ::1 51234 25", &own());
        assert_eq!(peer, Some(Peer { address: "2001:db8::1".into(), port: 51234 }));
        assert_eq!(parse_proxy_header("PROXY UNKNOWN", &own()), Some(own()));
        assert_eq!(parse_proxy_header("PROXY UNKNOWN anything else", &own()), Some(own()));
        assert_eq!(parse_proxy_header("PROXY TCP4 2001:db8::1 10.0.0.1 1 2", &own()), None);
        assert_eq!(parse_proxy_header("PROXY TCP4 203.0.113.7 10.0.0.1 99999 25", &own()), None);
        assert_eq!(parse_proxy_header("PROXY TCP4 203.0.113.7 10.0.0.1 051 25", &own()), None);
        assert_eq!(parse_proxy_header("PROXY TCP4 203.0.113.7 10.0.0.1 51234", &own()), None);
        assert_eq!(parse_proxy_header("HELO there", &own()), None);
    }

    #[tokio::test]
    async fn reads_the_header_and_keeps_what_follows() {
        let mut stream: &[u8] = b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 25\r\nEHLO x\r\n";
        let (peer, rest) = read_proxy_header(&mut stream, &own()).await.unwrap();
        assert_eq!(peer.address, "203.0.113.7");
        assert_eq!(&rest[..], b"EHLO x\r\n");

        let mut untrusted: &[u8] = b"PROXY UNKNOWN\r\n";
        let from_outside = Peer { address: "8.8.8.8".into(), port: 1 };
        assert!(read_proxy_header(&mut untrusted, &from_outside).await.is_err());

        let long = [b'A'; 200];
        let mut stream: &[u8] = &long;
        assert!(read_proxy_header(&mut stream, &own()).await.is_err());
    }
}
