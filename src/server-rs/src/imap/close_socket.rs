//! Closing an IMAP connection (imap/close-socket.ts).

use std::time::Duration;

use super::socket::SocketHandle;

/// Grace period for a final response to reach the wire before the socket is
/// destroyed. Generous for a peer that is merely slow, short enough that a
/// peer which has stopped reading cannot hold the connection, and short
/// enough to fit inside the shutdown budget the IMAP servers close under.
pub const CLOSE_FLUSH_TIMEOUT_MS: u64 = 2000;

/// Closes an IMAP connection, letting everything already written reach the
/// wire.
///
/// This is the single teardown primitive for the protocol: `ImapSession::close`
/// delegates to it, and so do the authentication rate-limit refusals.
///
/// Destroying the socket would drop whatever is still queued, and a final
/// response is written on exactly the peers likeliest to lose it: one torn
/// down for over-buffering, or one refused for repeated failures. RFC 3501
/// §7.1.5's untagged `BYE` is what marks the disconnect as the server's
/// decision rather than a network fault.
///
/// Ending the write side flushes the queue ahead of the FIN; the timer is
/// what stops a peer that never reads from holding the socket open. The
/// connection stops reading the moment the write side is ended, so a peer
/// being torn down cannot keep feeding the parser through the flush window.
pub fn close_socket(socket: &SocketHandle) {
    if socket.is_destroyed() || socket.is_writable_ended() {
        return;
    }

    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            // Armed before the `end` that can satisfy it.
            let flushing = socket.clone();
            runtime.spawn(async move {
                tokio::select! {
                    _ = flushing.destroyed() => {}
                    _ = tokio::time::sleep(Duration::from_millis(CLOSE_FLUSH_TIMEOUT_MS)) => flushing.destroy(),
                }
            });
            socket.end();
        }
        // Nothing can flush without a runtime.
        Err(_) => socket.destroy(),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_final_line_is_flushed_and_a_silent_peer_is_dropped_at_the_bound() {
        let (socket, _read, mut client) = SocketHandle::for_test("203.0.113.7");
        socket.write(Bytes::from_static(b"* BYE Server shutting down\r\n"));
        close_socket(&socket);
        assert!(!socket.is_writable());

        let mut received = String::new();
        client.read_to_string(&mut received).await.unwrap();
        assert_eq!(received, "* BYE Server shutting down\r\n");

        // The peer keeps its side open; the timer is what ends it.
        tokio::time::sleep(Duration::from_millis(CLOSE_FLUSH_TIMEOUT_MS - 100)).await;
        assert!(!socket.is_destroyed());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(socket.is_destroyed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_never_reads_is_destroyed_at_the_bound() {
        let (server, _client) = tokio::io::duplex(16);
        let peer = crate::proxy_protocol::Peer { address: "203.0.113.7".into(), port: 1 };
        let (socket, _read) = SocketHandle::open(Box::new(server), peer);
        socket.write(Bytes::from(vec![b'x'; 64 * 1024]));
        socket.write(Bytes::from_static(b"* BYE Timeout\r\n"));
        close_socket(&socket);
        tokio::time::sleep(Duration::from_millis(CLOSE_FLUSH_TIMEOUT_MS + 100)).await;
        assert!(socket.is_destroyed());
    }

    #[tokio::test]
    async fn closing_twice_is_closing_once() {
        let (socket, _read, _client) = SocketHandle::for_test("203.0.113.7");
        close_socket(&socket);
        close_socket(&socket);
        assert!(socket.is_writable_ended());
    }
}
