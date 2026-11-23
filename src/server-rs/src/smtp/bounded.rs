//! A DATA transaction's message, carried to the parser only as far as the
//! size ceiling and only while the sender keeps it moving
//! (`boundMessageSize` in `smtp.ts`).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::{Instant, Sleep};

use super::data_stream::{SmtpDataStream, SmtpDataTooLarge};
use super::types::SmtpHookError;
use crate::logger;

/// How long a DATA transaction may go without a byte before it is treated as
/// abandoned and its budget slot released.
///
/// The connection dying mid-DATA leaves a stream that never ends. A stream
/// whose connection closed is answered at once; this ceiling, re-armed on
/// every chunk, catches one that goes silent without closing, while a
/// slow-but-live sender is never cut off.
pub const DATA_IDLE_TIMEOUT: Duration = Duration::from_millis(65 * 1000);

/// Why a transaction was refused while its message was being read. Once one
/// is recorded the transaction's answer is decided: whatever the lane was
/// doing must not answer it again or carry on to its side effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// More than the listener's `size` arrived: 552.
    TooLarge(SmtpDataTooLarge),
    /// The connection closed, or went silent, before the end of DATA: 421.
    Abandoned,
    /// The stream itself failed: answered with its message, as an error with
    /// no reply code of its own is.
    Failed(String),
}

impl Refusal {
    /// The connection dropped mid-DATA — an abrupt RST, the idle-socket
    /// timeout, or a sender that went silent. RFC 5321 §3.8's 421 never
    /// reaches the peer in practice (the connection is already gone); it
    /// exists so this answers through the same machinery every other refusal
    /// does.
    pub const ABANDONED_CODE: u16 = 421;
    pub const ABANDONED_MESSAGE: &'static str = "4.4.2 Error: connection lost mid-transaction";

    /// The refusal as the hook's answer, and whether it carries its own reply
    /// code (one that does was logged where it was decided).
    pub fn to_hook_error(&self) -> (SmtpHookError, bool) {
        match self {
            Refusal::TooLarge(too_large) => (SmtpHookError::from(*too_large), true),
            Refusal::Abandoned => (SmtpHookError::with_code(Self::ABANDONED_CODE, Self::ABANDONED_MESSAGE), true),
            Refusal::Failed(message) => (SmtpHookError::new(message.clone()), false),
        }
    }
}

/// What the parser reads after a refusal: the read fails so the parser
/// settles (and closes the attachment file it was writing) instead of
/// waiting on a message that will never end.
fn refused_error() -> io::Error {
    io::Error::other("message refused")
}

/// The DATA stream of one admitted transaction, with the size ceiling
/// enforced and the idle watchdog armed.
pub struct BoundedMessage {
    stream: SmtpDataStream,
    idle: Pin<Box<Sleep>>,
    idle_timeout: Duration,
    refusal: Option<Refusal>,
    remote_address: String,
}

impl BoundedMessage {
    /// Starts holding `stream` to its listener's size and arms the idle
    /// watchdog: call once the transaction has its budget slot.
    pub fn new(stream: SmtpDataStream, remote_address: &str) -> BoundedMessage {
        BoundedMessage::with_idle_timeout(stream, remote_address, DATA_IDLE_TIMEOUT)
    }

    pub fn with_idle_timeout(
        mut stream: SmtpDataStream,
        remote_address: &str,
        idle_timeout: Duration,
    ) -> BoundedMessage {
        stream.enforce_size_limit();
        BoundedMessage {
            stream,
            idle: Box::pin(tokio::time::sleep(idle_timeout)),
            idle_timeout,
            refusal: None,
            remote_address: remote_address.to_string(),
        }
    }

    /// The refusal this transaction has met, if any. A message that has
    /// already grown past the ceiling counts, whether or not anything read
    /// that far: the ceiling refuses as the octets arrive.
    pub fn refusal(&mut self) -> Option<&Refusal> {
        if self.refusal.is_none() && self.stream.is_over_size_limit() {
            self.refuse_too_large(SmtpDataTooLarge { limit: self.stream.size_limit() });
        }
        self.refusal.as_ref()
    }

    fn refuse_too_large(&mut self, too_large: SmtpDataTooLarge) {
        logger::warn(
            "SMTP: refused a message over the maximum size",
            json!({ "remoteAddress": self.remote_address, "maxBytes": too_large.limit }),
        );
        self.refusal = Some(Refusal::TooLarge(too_large));
    }

    fn abandon(&mut self) {
        logger::warn(
            "SMTP: DATA transaction abandoned mid-transfer, releasing its budget slot",
            json!({ "remoteAddress": self.remote_address }),
        );
        self.refusal = Some(Refusal::Abandoned);
    }

    fn refuse_for(&mut self, error: &io::Error) {
        if SmtpDataStream::is_too_large(error) {
            let limit = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<SmtpDataTooLarge>())
                .map_or_else(|| self.stream.size_limit(), |too_large| too_large.limit);
            self.refuse_too_large(SmtpDataTooLarge { limit });
        } else if SmtpDataStream::is_abandoned(error) {
            self.abandon();
        } else {
            logger::error_with("SMTP: DATA stream failed", json!({}), error);
            self.refusal = Some(Refusal::Failed(error.to_string()));
        }
    }
}

impl AsyncRead for BoundedMessage {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.refusal().is_some() {
            return Poll::Ready(Err(refused_error()));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.stream).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > before {
                    let deadline = Instant::now() + this.idle_timeout;
                    this.idle.as_mut().reset(deadline);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => {
                this.refuse_for(&error);
                Poll::Ready(Err(refused_error()))
            }
            Poll::Pending => {
                if this.idle.as_mut().poll(cx).is_ready() {
                    this.abandon();
                    return Poll::Ready(Err(refused_error()));
                }
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::smtp::data_stream::data_stream;

    #[tokio::test]
    async fn delivers_a_message_under_the_ceiling_whole() {
        let (writer, stream) = data_stream(10);
        let mut message = BoundedMessage::new(stream, "192.0.2.1");
        writer.write(Bytes::from_static(b"12345"));
        writer.write(Bytes::from_static(b"67890"));
        writer.end();
        let mut read = Vec::new();
        message.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, b"1234567890");
        assert_eq!(message.refusal(), None);
    }

    #[tokio::test]
    async fn refuses_at_the_ceiling_with_552_and_fails_the_parsers_read() {
        let (writer, stream) = data_stream(10);
        let mut message = BoundedMessage::new(stream, "192.0.2.1");
        writer.write(Bytes::from_static(b"12345"));
        let mut chunk = [0u8; 5];
        message.read_exact(&mut chunk).await.unwrap();
        writer.write(Bytes::from_static(b"678901"));
        assert!(message.read(&mut chunk).await.is_err());
        let refusal = message.refusal().cloned().expect("refused");
        assert_eq!(refusal, Refusal::TooLarge(SmtpDataTooLarge { limit: 10 }));
        let (error, coded) = refusal.to_hook_error();
        assert_eq!(error, SmtpHookError::with_code(552, "Error: message exceeds fixed maximum message size 10"));
        assert!(coded);
        // Every later read fails too, so nothing reaches the parser past the cut.
        assert!(message.read(&mut chunk).await.is_err());
    }

    #[tokio::test]
    async fn notices_a_message_past_the_ceiling_that_nothing_has_read_yet() {
        let (writer, stream) = data_stream(4);
        let mut message = BoundedMessage::new(stream, "192.0.2.1");
        writer.write(Bytes::from_static(b"12345"));
        assert_eq!(message.refusal(), Some(&Refusal::TooLarge(SmtpDataTooLarge { limit: 4 })));
    }

    #[tokio::test]
    async fn answers_a_connection_lost_mid_data_as_abandoned() {
        let (writer, stream) = data_stream(1024);
        let mut message = BoundedMessage::new(stream, "192.0.2.1");
        writer.write(Bytes::from_static(b"partial"));
        drop(writer);
        let mut read = Vec::new();
        assert!(message.read_to_end(&mut read).await.is_err());
        assert_eq!(read, b"partial");
        let refusal = message.refusal().cloned().expect("refused");
        assert_eq!(refusal, Refusal::Abandoned);
        assert_eq!(
            refusal.to_hook_error(),
            (SmtpHookError::with_code(421, "4.4.2 Error: connection lost mid-transaction"), true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn abandons_a_sender_that_goes_silent_past_the_idle_ceiling() {
        let (writer, stream) = data_stream(1024);
        let mut message = BoundedMessage::new(stream, "192.0.2.1");
        let started = Instant::now();
        writer.write(Bytes::from_static(b"x"));
        let mut chunk = [0u8; 16];
        assert_eq!(message.read(&mut chunk).await.unwrap(), 1);
        assert!(message.read(&mut chunk).await.is_err());
        assert_eq!(message.refusal(), Some(&Refusal::Abandoned));
        assert_eq!(started.elapsed(), DATA_IDLE_TIMEOUT);
        drop(writer);
    }

    #[tokio::test(start_paused = true)]
    async fn re_arms_the_watchdog_on_every_chunk_so_a_slow_but_live_sender_completes() {
        let (writer, stream) = data_stream(1024);
        let mut message = BoundedMessage::new(stream, "192.0.2.1");
        let feeder = tokio::spawn(async move {
            for _ in 0..4 {
                tokio::time::sleep(DATA_IDLE_TIMEOUT - Duration::from_secs(1)).await;
                writer.write(Bytes::from_static(b"ab"));
            }
            writer.end();
        });
        let mut read = Vec::new();
        message.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, b"abababab");
        assert_eq!(message.refusal(), None);
        feeder.await.unwrap();
    }
}
