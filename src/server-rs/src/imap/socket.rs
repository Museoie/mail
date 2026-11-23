//! One IMAP connection's socket, as the session and the registries hold it.
//!
//! The Bun server hands its session a `net.Socket`: a write that never waits,
//! `end`, `destroy`, an inactivity timeout and the peer's address. This is
//! that surface over a tokio stream. Everything written goes through one
//! queue to one writer task, so a response is never interleaved with another
//! and a write can be issued from anywhere — a command, an IDLE notification,
//! the credential sweep, shutdown — without waiting for the peer.
//!
//! The reading half stays with the connection's own task (`handler.rs`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::logger;
use crate::proxy_protocol::Peer;

/// Anything a connection can run over: TCP, TCP behind a PROXY header, TLS.
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncStream for T {}

pub type BoxedStream = Box<dyn AsyncStream>;

/// Small writes waiting in the queue are sent as one, up to this many octets.
const COALESCE_BYTES: usize = 256 * 1024;

enum Command {
    Write(Bytes),
    /// Answered once everything queued before it has been handed to the kernel.
    Flushed(oneshot::Sender<()>),
    /// Flush, then close the write side.
    End,
    /// Flush, then give the write half back (STARTTLS).
    Handover(oneshot::Sender<WriteHalf<BoxedStream>>),
}

struct Shared {
    peer: Peer,
    commands: mpsc::UnboundedSender<Command>,
    ended: AtomicBool,
    ended_notify: Notify,
    destroyed: CancellationToken,
    /// Set once the write side has been shut down, or the writer is gone.
    write_closed: CancellationToken,
    epoch: Instant,
    last_activity_ms: AtomicU64,
    timeout_ms: AtomicU64,
    timer_changed: Notify,
    on_timeout: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

/// A connection's socket. Cheap to clone; every clone is the same socket.
#[derive(Clone)]
pub struct SocketHandle(Arc<Shared>);

impl SocketHandle {
    /// Splits `stream`, starts its writer and its inactivity timer, and
    /// returns the socket with the half the connection reads from.
    pub fn open(stream: BoxedStream, peer: Peer) -> (SocketHandle, ReadHalf<BoxedStream>) {
        let (read_half, write_half) = tokio::io::split(stream);
        let (commands, queue) = mpsc::unbounded_channel();
        let socket = SocketHandle(Arc::new(Shared {
            peer,
            commands,
            ended: AtomicBool::new(false),
            ended_notify: Notify::new(),
            destroyed: CancellationToken::new(),
            write_closed: CancellationToken::new(),
            epoch: Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            timeout_ms: AtomicU64::new(0),
            timer_changed: Notify::new(),
            on_timeout: OnceLock::new(),
        }));
        tokio::spawn(write_loop(write_half, queue, socket.clone()));
        tokio::spawn(watch_inactivity(socket.clone()));
        (socket, read_half)
    }

    pub fn remote_address(&self) -> &str {
        &self.0.peer.address
    }

    pub fn remote_port(&self) -> u16 {
        self.0.peer.port
    }

    /// `address:port` of the peer, for log lines.
    pub fn remote(&self) -> String {
        format!("{}:{}", self.0.peer.address, self.0.peer.port)
    }

    pub fn is_destroyed(&self) -> bool {
        self.0.destroyed.is_cancelled()
    }

    /// False once the write side was ended or the socket destroyed.
    pub fn is_writable(&self) -> bool {
        !self.is_writable_ended() && !self.is_destroyed()
    }

    pub fn is_writable_ended(&self) -> bool {
        self.0.ended.load(Ordering::SeqCst)
    }

    /// Queues `data`. Returns false, having queued nothing, when the socket
    /// no longer takes writes.
    pub fn write(&self, data: Bytes) -> bool {
        if !self.is_writable() {
            return false;
        }
        self.0.commands.send(Command::Write(data)).is_ok()
    }

    /// Queues `data` and waits until it has been handed to the kernel, which
    /// is how a large response is paced by the peer. False when the socket
    /// went away first.
    pub async fn write_flushed(&self, data: Bytes) -> bool {
        if !self.write(data) {
            return false;
        }
        let (reply, flushed) = oneshot::channel();
        if self.0.commands.send(Command::Flushed(reply)).is_err() {
            return false;
        }
        tokio::select! {
            biased;
            _ = self.0.destroyed.cancelled() => false,
            answered = flushed => answered.is_ok() && !self.is_destroyed(),
        }
    }

    /// Closes the write side once everything queued has gone out. Later
    /// writes are refused. The socket stays open for the peer to close its
    /// side; see `close_socket` for the bound on that.
    pub fn end(&self) {
        if self.0.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.0.commands.send(Command::End);
        self.0.ended_notify.notify_one();
    }

    /// Drops the connection now, with whatever is still queued.
    pub fn destroy(&self) {
        self.0.destroyed.cancel();
        self.0.ended_notify.notify_one();
    }

    /// Resolves when the socket is destroyed.
    pub async fn destroyed(&self) {
        self.0.destroyed.cancelled().await;
    }

    /// Resolves when everything queued has gone out and the write side is
    /// shut down, or the socket is destroyed.
    pub async fn write_side_closed(&self) {
        tokio::select! {
            _ = self.0.write_closed.cancelled() => {}
            _ = self.0.destroyed.cancelled() => {}
        }
    }

    /// Resolves when the write side was ended (or the socket destroyed).
    pub async fn ended(&self) {
        loop {
            if self.is_writable_ended() || self.is_destroyed() {
                return;
            }
            self.0.ended_notify.notified().await;
        }
    }

    /// Sets the inactivity timeout and restarts it. Zero turns it off.
    pub fn set_timeout(&self, timeout_ms: u64) {
        self.0.timeout_ms.store(timeout_ms, Ordering::SeqCst);
        self.touch();
        self.0.timer_changed.notify_one();
    }

    /// What runs when the socket has been inactive for its timeout.
    pub fn on_timeout(&self, handler: impl Fn() + Send + Sync + 'static) {
        let _ = self.0.on_timeout.set(Box::new(handler));
    }

    /// Records activity, which restarts the inactivity timeout.
    pub fn touch(&self) {
        let elapsed = self.0.epoch.elapsed().as_millis() as u64;
        self.0.last_activity_ms.store(elapsed, Ordering::SeqCst);
    }

    /// Takes the write half back once everything queued has gone out, for a
    /// STARTTLS upgrade. The handle is dead afterwards. `None` when the
    /// socket went away first.
    pub async fn handover(&self) -> Option<WriteHalf<BoxedStream>> {
        let (reply, half) = oneshot::channel();
        self.0.commands.send(Command::Handover(reply)).ok()?;
        let half = tokio::select! {
            biased;
            _ = self.0.destroyed.cancelled() => None,
            half = half => half.ok(),
        };
        self.0.ended.store(true, Ordering::SeqCst);
        self.0.destroyed.cancel();
        half
    }

    /// The inactivity timeout in force, in milliseconds.
    #[cfg(test)]
    pub(crate) fn timeout_ms(&self) -> u64 {
        self.0.timeout_ms.load(Ordering::SeqCst)
    }

    /// Runs the inactivity handler now, as if the timeout had run out.
    #[cfg(test)]
    pub(crate) fn fire_timeout(&self) {
        if let Some(handler) = self.0.on_timeout.get() {
            handler();
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(address: &str) -> (SocketHandle, ReadHalf<BoxedStream>, tokio::io::DuplexStream) {
        let (server, client) = tokio::io::duplex(4 * 1024 * 1024);
        let peer = Peer { address: address.to_string(), port: 45678 };
        let (socket, read_half) = SocketHandle::open(Box::new(server), peer);
        (socket, read_half, client)
    }
}

fn is_quiet_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionAborted
    )
}

/// The TLS alerts a peer sends when it refuses this server's certificate.
const PEER_CERT_REJECTION_ALERTS: [&str; 6] = [
    "ALERT_BAD_CERTIFICATE",
    "ALERT_UNSUPPORTED_CERTIFICATE",
    "ALERT_CERTIFICATE_REVOKED",
    "ALERT_CERTIFICATE_EXPIRED",
    "ALERT_CERTIFICATE_UNKNOWN",
    "ALERT_UNKNOWN_CA",
];

/// A peer rejecting this server's certificate (expired, wrong host name,
/// unknown CA) is a server-side defect even though it surfaces as a TLS
/// error (lib/peer-cert-rejection.ts).
pub fn is_peer_cert_rejection(error: &std::io::Error) -> bool {
    // OpenSSL names the alert it received: `sslv3 alert bad certificate`.
    let text = error.to_string().to_uppercase().replace(' ', "_");
    PEER_CERT_REJECTION_ALERTS.iter().any(|alert| text.contains(alert))
}

/// What a socket fault is logged with: the peer is named only when it
/// refused the certificate, which is the one fault an operator has to chase
/// down by client.
pub fn socket_error_context(error: &std::io::Error, remote: &str) -> serde_json::Value {
    if is_peer_cert_rejection(error) {
        json!({ "component": "imap", "remote": remote })
    } else {
        json!({ "component": "imap" })
    }
}

/// Logs a socket fault the way the Bun server's `error` listener does: a
/// reset by the peer is not worth a line.
pub fn log_socket_error(error: &std::io::Error, remote: &str) {
    if !is_quiet_error(error) {
        logger::error_with("IMAP socket error", socket_error_context(error, remote), error);
    }
}

async fn write_all(half: &mut WriteHalf<BoxedStream>, chunks: &[Bytes]) -> std::io::Result<()> {
    match chunks {
        [only] => half.write_all(only).await?,
        many => {
            let mut joined = BytesMut::with_capacity(many.iter().map(Bytes::len).sum());
            for chunk in many {
                joined.extend_from_slice(chunk);
            }
            half.write_all(&joined).await?;
        }
    }
    half.flush().await
}

async fn write_loop(
    mut half: WriteHalf<BoxedStream>,
    mut queue: mpsc::UnboundedReceiver<Command>,
    socket: SocketHandle,
) {
    let destroyed = socket.0.destroyed.clone();
    let mut held: Option<Command> = None;
    loop {
        let command = match held.take() {
            Some(command) => command,
            None => tokio::select! {
                biased;
                _ = destroyed.cancelled() => break,
                command = queue.recv() => match command {
                    Some(command) => command,
                    None => break,
                },
            },
        };
        match command {
            Command::Write(first) => {
                let mut total = first.len();
                let mut chunks = vec![first];
                while total < COALESCE_BYTES {
                    match queue.try_recv() {
                        Ok(Command::Write(more)) => {
                            total += more.len();
                            chunks.push(more);
                        }
                        Ok(other) => {
                            held = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let written = tokio::select! {
                    biased;
                    _ = destroyed.cancelled() => break,
                    written = write_all(&mut half, &chunks) => written,
                };
                if let Err(error) = written {
                    log_socket_error(&error, &socket.remote());
                    break;
                }
                socket.touch();
            }
            Command::Flushed(reply) => {
                let _ = reply.send(());
            }
            Command::End => {
                tokio::select! {
                    biased;
                    _ = destroyed.cancelled() => {}
                    _ = half.shutdown() => {
                        socket.0.write_closed.cancel();
                        // Half-closed: the peer closing its side, or the
                        // flush timer, is what destroys the socket.
                        destroyed.cancelled().await;
                    }
                }
                break;
            }
            Command::Handover(reply) => {
                let _ = reply.send(half);
                return;
            }
        }
    }
    socket.0.write_closed.cancel();
    socket.destroy();
}

async fn watch_inactivity(socket: SocketHandle) {
    let shared = &socket.0;
    loop {
        let timeout_ms = shared.timeout_ms.load(Ordering::SeqCst);
        if timeout_ms == 0 {
            tokio::select! {
                biased;
                _ = shared.destroyed.cancelled() => return,
                _ = shared.timer_changed.notified() => continue,
            }
        }
        let last_activity_ms = shared.last_activity_ms.load(Ordering::SeqCst);
        let deadline = shared.epoch + Duration::from_millis(last_activity_ms.saturating_add(timeout_ms));
        tokio::select! {
            biased;
            _ = shared.destroyed.cancelled() => return,
            _ = shared.timer_changed.notified() => continue,
            _ = tokio::time::sleep_until(deadline) => {}
        }
        // Activity since the sleep began moved the deadline.
        let quiet_since = shared.last_activity_ms.load(Ordering::SeqCst);
        let timeout_ms = shared.timeout_ms.load(Ordering::SeqCst);
        if timeout_ms == 0 || (shared.epoch.elapsed().as_millis() as u64) < quiet_since.saturating_add(timeout_ms) {
            continue;
        }
        if let Some(handler) = shared.on_timeout.get() {
            handler();
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    async fn read_some(client: &mut tokio::io::DuplexStream, octets: usize) -> String {
        let mut received = vec![0u8; octets];
        client.read_exact(&mut received).await.unwrap();
        String::from_utf8(received).unwrap()
    }

    #[tokio::test]
    async fn writes_reach_the_peer_in_the_order_they_were_queued() {
        let (socket, _read, mut client) = SocketHandle::for_test("203.0.113.7");
        assert!(socket.write(Bytes::from_static(b"one\r\n")));
        assert!(socket.write(Bytes::from_static(b"two\r\n")));
        assert!(socket.write_flushed(Bytes::from_static(b"three\r\n")).await);
        assert_eq!(read_some(&mut client, 17).await, "one\r\ntwo\r\nthree\r\n");
        assert_eq!(socket.remote(), "203.0.113.7:45678");
    }

    #[tokio::test]
    async fn end_flushes_what_was_queued_then_refuses_writes() {
        let (socket, _read, mut client) = SocketHandle::for_test("203.0.113.7");
        socket.write(Bytes::from_static(b"* BYE\r\n"));
        socket.end();
        assert!(!socket.is_writable());
        assert!(socket.is_writable_ended());
        assert!(!socket.write(Bytes::from_static(b"late\r\n")));
        let mut received = String::new();
        client.read_to_string(&mut received).await.unwrap();
        assert_eq!(received, "* BYE\r\n");
        assert!(!socket.is_destroyed(), "the peer has not closed its side and no timer ran");
        socket.destroy();
        assert!(socket.is_destroyed());
    }

    #[tokio::test]
    async fn a_destroyed_socket_fails_a_write_waiting_on_the_peer() {
        let (server, _client) = tokio::io::duplex(16);
        let peer = Peer { address: "203.0.113.7".into(), port: 1 };
        let (socket, _read) = SocketHandle::open(Box::new(server), peer);
        let waiting = tokio::spawn({
            let socket = socket.clone();
            async move { socket.write_flushed(Bytes::from(vec![b'x'; 4096])).await }
        });
        tokio::task::yield_now().await;
        socket.destroy();
        assert!(!waiting.await.unwrap());
        assert!(!socket.write(Bytes::from_static(b"x")));
    }

    #[tokio::test(start_paused = true)]
    async fn the_inactivity_timeout_fires_once_and_activity_postpones_it() {
        let (socket, _read, _client) = SocketHandle::for_test("203.0.113.7");
        let fired = Arc::new(AtomicU64::new(0));
        socket.on_timeout({
            let fired = fired.clone();
            move || {
                fired.fetch_add(1, Ordering::SeqCst);
            }
        });
        socket.set_timeout(1000);
        tokio::time::sleep(Duration::from_millis(600)).await;
        socket.touch();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 0);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(5000)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_timeout_replaces_the_one_running() {
        let (socket, _read, _client) = SocketHandle::for_test("203.0.113.7");
        let fired = Arc::new(AtomicU64::new(0));
        socket.on_timeout({
            let fired = fired.clone();
            move || {
                fired.fetch_add(1, Ordering::SeqCst);
            }
        });
        socket.set_timeout(1000);
        tokio::time::sleep(Duration::from_millis(500)).await;
        socket.set_timeout(10_000);
        tokio::time::sleep(Duration::from_millis(9000)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 0);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn handover_returns_the_write_half_after_the_queue_drained() {
        let (socket, read_half, mut client) = SocketHandle::for_test("203.0.113.7");
        socket.write(Bytes::from_static(b"A1 OK Begin TLS negotiation now\r\n"));
        let write_half = socket.handover().await.expect("the socket is alive");
        assert!(!socket.write(Bytes::from_static(b"late")));
        assert_eq!(read_some(&mut client, 33).await, "A1 OK Begin TLS negotiation now\r\n");
        // The two halves are one stream again.
        let mut stream = read_half.unsplit(write_half);
        stream.write_all(b"raw").await.unwrap();
        assert_eq!(read_some(&mut client, 3).await, "raw");
    }
}
