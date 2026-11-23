//! A streamed body gives its body-budget slot and its stream-mutex key back
//! however its consumer ends (stream-slot-release.test.ts).
//!
//! The production shape, as phase 2 will build it: the key on the outside,
//! the budget slot inside, both held by the stream itself, so dropping the
//! stream (the consumer gave up on a dead socket) releases both.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::sync::Notify;

use super::body_budget::{acquire_body_budget, body_budget_capacity, reset_body_budget, test_lock, BodyBudgetSlot};
use super::chunked_write::{write_stream_to_socket, ChunkedWriteSocket};
use super::stream_mutex::acquire_stream_mutex;

/// A socket whose `teardown` is a destroy: writes stop being taken, and a
/// write waiting on the peer is woken.
#[derive(Default)]
struct FakeSocket {
    dead: AtomicBool,
    /// Tear down from inside the first write.
    die_on_write: AtomicBool,
    woken: Notify,
}

impl FakeSocket {
    fn teardown(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self.woken.notify_waiters();
    }
}

impl ChunkedWriteSocket for Arc<FakeSocket> {
    fn is_open(&self) -> bool {
        !self.dead.load(Ordering::SeqCst)
    }

    async fn write_chunk(&self, _chunk: Bytes) -> bool {
        if self.die_on_write.load(Ordering::SeqCst) {
            self.teardown();
        }
        self.is_open()
    }
}

/// A body stream of `chunks` chunks under `key`.
fn body_stream(key: &'static str, chunks: usize) -> impl Stream<Item = Bytes> {
    futures_util::stream::once(async move {
        let key_held = acquire_stream_mutex(key).await;
        let slot = acquire_body_budget().await;
        futures_util::stream::iter((0..chunks).map(move |n| {
            let _held = (&key_held, &slot);
            Bytes::from(format!("chunk-{n}"))
        }))
    })
    .flatten()
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// Every slot of the budget, held until dropped.
async fn fill_budget() -> Vec<BodyBudgetSlot> {
    let mut slots = Vec::new();
    for _ in 0..body_budget_capacity() {
        slots.push(acquire_body_budget().await);
    }
    slots
}

/// How many slots the budget can hand out right now; all of them, unless one
/// leaked.
async fn available_slots() -> usize {
    let mut slots = Vec::new();
    for _ in 0..body_budget_capacity() {
        match tokio::time::timeout(Duration::from_millis(50), acquire_body_budget()).await {
            Ok(slot) => slots.push(slot),
            Err(_) => break,
        }
    }
    slots.len()
}

/// Whether `key` is free: a new stream of it starts at once.
async fn key_is_free(key: &str) -> bool {
    tokio::time::timeout(Duration::from_millis(50), acquire_stream_mutex(key)).await.is_ok()
}

#[tokio::test]
async fn returns_the_slot_when_the_socket_dies_while_the_consumer_is_queued_on_the_budget() {
    let _lock = test_lock().await;
    reset_body_budget();
    let held = fill_budget().await;

    let socket = Arc::new(FakeSocket::default());
    let consumer = tokio::spawn({
        let socket = socket.clone();
        async move { write_stream_to_socket(&socket, body_stream("slot-queued", 2)).await }
    });
    settle().await; // parked on the budget

    socket.teardown();
    drop(held);
    consumer.await.unwrap();
    settle().await;

    assert_eq!(available_slots().await, body_budget_capacity());
    assert!(key_is_free("slot-queued").await);
}

#[tokio::test]
async fn returns_the_slot_when_the_socket_dies_after_the_first_chunk_is_written() {
    let _lock = test_lock().await;
    reset_body_budget();
    let socket = Arc::new(FakeSocket::default());
    let mut stream = Box::pin(body_stream("slot-midstream", 5));

    let mut delivered = 0;
    while let Some(chunk) = stream.next().await {
        delivered += 1;
        assert!(!chunk.is_empty());
        socket.teardown();
        if !socket.is_open() {
            break;
        }
    }
    drop(stream);

    assert_eq!(delivered, 1);
    assert_eq!(available_slots().await, body_budget_capacity());
    assert!(key_is_free("slot-midstream").await);
}

#[tokio::test]
async fn takes_no_slot_at_all_when_the_socket_is_already_gone_before_the_write_starts() {
    let _lock = test_lock().await;
    reset_body_budget();
    let socket = Arc::new(FakeSocket::default());
    socket.teardown();

    let written = write_stream_to_socket(&socket, body_stream("slot-dead", 3)).await;

    assert_eq!(written, 0);
    assert_eq!(available_slots().await, body_budget_capacity());
    assert!(key_is_free("slot-dead").await);
}

#[tokio::test]
async fn drops_the_stream_when_the_write_returns_early_mid_loop() {
    struct Finally(Arc<AtomicBool>);
    impl Drop for Finally {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let finally_ran = Arc::new(AtomicBool::new(false));
    let guard = Finally(finally_ran.clone());
    let chunks = futures_util::stream::iter([Bytes::from_static(b"first"), Bytes::from_static(b"second")]).map(move |chunk| {
        let _guard = &guard;
        chunk
    });
    // The socket dies the moment the first chunk lands.
    let socket = Arc::new(FakeSocket::default());
    socket.die_on_write.store(true, Ordering::SeqCst);

    let written = write_stream_to_socket(&socket, chunks).await;

    assert_eq!(written, "first".len());
    assert!(finally_ran.load(Ordering::SeqCst));
}

#[tokio::test]
async fn frees_the_queued_waiters_slot_for_the_next_fetch_so_capacity_does_not_decay() {
    let _lock = test_lock().await;
    reset_body_budget();
    // Each abandoned waiter costing one slot would decay the budget; run
    // the abandon twice and check every slot is still there.
    for key in ["slot-round-1", "slot-round-2"] {
        let held = fill_budget().await;
        let socket = Arc::new(FakeSocket::default());
        let consumer = tokio::spawn({
            let socket = socket.clone();
            async move { write_stream_to_socket(&socket, body_stream(key, 2)).await }
        });
        settle().await;
        socket.teardown();
        drop(held);
        consumer.await.unwrap();
        settle().await;
    }

    assert_eq!(available_slots().await, body_budget_capacity());
    assert!(key_is_free("slot-round-2").await);
}
