//! Writing a large payload at the pace the peer reads it
//! (imap/chunked-write.ts).

use std::future::Future;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use super::socket::SocketHandle;

/// Matches a typical TCP high-water mark.
pub const CHUNK_BYTES: usize = 64 * 1024;

/// The part of a socket a paced write needs.
pub trait ChunkedWriteSocket {
    /// False once the socket is destroyed or no longer takes writes.
    fn is_open(&self) -> bool;

    /// Writes one chunk and waits until the socket can take the next. False
    /// when the socket went away instead.
    fn write_chunk(&self, chunk: Bytes) -> impl Future<Output = bool> + Send;
}

impl ChunkedWriteSocket for SocketHandle {
    fn is_open(&self) -> bool {
        !self.is_destroyed() && self.is_writable()
    }

    async fn write_chunk(&self, chunk: Bytes) -> bool {
        self.write_flushed(chunk).await
    }
}

/// Writes `payload` in [`CHUNK_BYTES`] pieces, each one waiting for the
/// socket to drain the last. Returns the octets written; fewer than the
/// payload when the socket died on the way, in which case it does not hang.
pub async fn write_chunked_to_socket(socket: &impl ChunkedWriteSocket, payload: Bytes) -> usize {
    if !socket.is_open() {
        return 0;
    }
    let mut written = 0;
    let mut offset = 0;
    while offset < payload.len() {
        let end = (offset + CHUNK_BYTES).min(payload.len());
        // A view of the payload, not a copy.
        let alive = socket.write_chunk(payload.slice(offset..end)).await;
        written += end - offset;
        if !alive {
            return written;
        }
        offset = end;
    }
    written
}

/// Writes each chunk a stream produces with the same pacing, pulling the
/// next one only once the last is on its way: the whole body is never held.
/// Chunk boundaries are the producer's. Stops pulling, and drops the stream,
/// when the socket dies.
pub async fn write_stream_to_socket(
    socket: &impl ChunkedWriteSocket,
    chunks: impl Stream<Item = Bytes>,
) -> usize {
    if !socket.is_open() {
        return 0;
    }
    let mut chunks = std::pin::pin!(chunks);
    let mut written = 0;
    while let Some(chunk) = chunks.next().await {
        if !socket.is_open() {
            return written;
        }
        let size = chunk.len();
        let alive = socket.write_chunk(chunk).await;
        written += size;
        if !alive {
            return written;
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::Notify;

    use super::*;

    /// Records writes; with `hold` set, a write waits for `drain` (or for
    /// the socket to be closed) before it reports back.
    #[derive(Default)]
    struct FakeSocket {
        open: AtomicBool,
        hold: AtomicBool,
        drain: Notify,
        written: Mutex<Vec<Bytes>>,
    }

    impl FakeSocket {
        fn new() -> Arc<FakeSocket> {
            let socket = FakeSocket::default();
            socket.open.store(true, Ordering::SeqCst);
            Arc::new(socket)
        }

        fn chunk_sizes(&self) -> Vec<usize> {
            self.written.lock().unwrap().iter().map(Bytes::len).collect()
        }

        fn close(&self) {
            self.open.store(false, Ordering::SeqCst);
            self.drain.notify_one();
        }
    }

    impl ChunkedWriteSocket for Arc<FakeSocket> {
        fn is_open(&self) -> bool {
            self.open.load(Ordering::SeqCst)
        }

        async fn write_chunk(&self, chunk: Bytes) -> bool {
            self.written.lock().unwrap().push(chunk);
            if self.hold.load(Ordering::SeqCst) {
                self.drain.notified().await;
            }
            self.is_open()
        }
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn emits_the_payload_as_chunks_that_concatenate_back_to_it() {
        let socket = FakeSocket::new();
        let payload: Bytes = (0..CHUNK_BYTES * 2 + 100).map(|i| (i % 251) as u8).collect::<Vec<u8>>().into();
        let written = write_chunked_to_socket(&socket, payload.clone()).await;
        assert_eq!(written, payload.len());
        assert_eq!(socket.chunk_sizes(), vec![CHUNK_BYTES, CHUNK_BYTES, 100]);
        let joined: Vec<u8> = socket.written.lock().unwrap().iter().flat_map(|chunk| chunk.to_vec()).collect();
        assert_eq!(joined, payload.to_vec());
    }

    #[tokio::test]
    async fn waits_for_the_drain_before_the_next_chunk() {
        let socket = FakeSocket::new();
        socket.hold.store(true, Ordering::SeqCst);
        let payload = Bytes::from(vec![b'x'; CHUNK_BYTES * 2]);
        let writing = tokio::spawn({
            let socket = socket.clone();
            async move { write_chunked_to_socket(&socket, payload).await }
        });
        settle().await;
        assert_eq!(socket.chunk_sizes().len(), 1, "the second chunk waits for the first to drain");
        socket.drain.notify_one();
        settle().await;
        assert_eq!(socket.chunk_sizes().len(), 2);
        socket.drain.notify_one();
        assert_eq!(writing.await.unwrap(), CHUNK_BYTES * 2);
    }

    #[tokio::test]
    async fn stops_when_the_socket_closes_mid_drain() {
        let socket = FakeSocket::new();
        socket.hold.store(true, Ordering::SeqCst);
        let payload = Bytes::from(vec![b'x'; CHUNK_BYTES * 3]);
        let writing = tokio::spawn({
            let socket = socket.clone();
            async move { write_chunked_to_socket(&socket, payload).await }
        });
        settle().await;
        socket.close();
        // It returns, with the one chunk it wrote, rather than waiting on a
        // drain that is never coming.
        assert_eq!(writing.await.unwrap(), CHUNK_BYTES);
        assert_eq!(socket.chunk_sizes().len(), 1);
    }

    #[tokio::test]
    async fn writes_nothing_to_a_socket_that_is_already_gone() {
        let socket = FakeSocket::new();
        socket.close();
        assert_eq!(write_chunked_to_socket(&socket, Bytes::from_static(b"payload")).await, 0);
        assert_eq!(write_stream_to_socket(&socket, futures_util::stream::iter([Bytes::from_static(b"x")])).await, 0);
        assert!(socket.chunk_sizes().is_empty());
    }

    #[tokio::test]
    async fn a_stream_is_written_chunk_by_chunk_as_produced() {
        let socket = FakeSocket::new();
        let chunks = futures_util::stream::iter([
            Bytes::from_static(b"one"),
            Bytes::from_static(b"three"),
            Bytes::from_static(b""),
            Bytes::from_static(b"sixsix"),
        ]);
        assert_eq!(write_stream_to_socket(&socket, chunks).await, 14);
        assert_eq!(socket.chunk_sizes(), vec![3, 5, 0, 6]);
    }

    #[tokio::test]
    async fn a_stream_is_not_pulled_past_a_dead_socket() {
        let socket = FakeSocket::new();
        let pulled = Arc::new(AtomicUsize::new(0));
        let chunks = futures_util::stream::iter(0..10).map({
            let (pulled, socket) = (pulled.clone(), socket.clone());
            move |n| {
                pulled.fetch_add(1, Ordering::SeqCst);
                if n == 1 {
                    // The socket dies while the second chunk is being produced.
                    socket.close();
                }
                Bytes::from_static(b"chunk")
            }
        });
        assert_eq!(write_stream_to_socket(&socket, chunks).await, 5);
        assert_eq!(pulled.load(Ordering::SeqCst), 2);
        assert_eq!(socket.chunk_sizes(), vec![5]);
    }

    #[tokio::test]
    async fn a_real_socket_paces_a_payload_larger_than_the_peer_has_read() {
        use tokio::io::AsyncReadExt;

        let (server, mut client) = tokio::io::duplex(1024);
        let peer = crate::proxy_protocol::Peer { address: "203.0.113.7".into(), port: 1 };
        let (socket, _read) = SocketHandle::open(Box::new(server), peer);
        let payload = Bytes::from(vec![b'z'; CHUNK_BYTES + 5000]);
        let writing = tokio::spawn({
            let socket = socket.clone();
            let payload = payload.clone();
            async move { write_chunked_to_socket(&socket, payload).await }
        });
        settle().await;
        assert!(!writing.is_finished(), "the write waits for the peer to read");
        let mut received = vec![0u8; payload.len()];
        client.read_exact(&mut received).await.unwrap();
        assert_eq!(writing.await.unwrap(), payload.len());
        assert_eq!(received, payload.to_vec());
    }
}
