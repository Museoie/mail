//! `Transfer-Encoding: chunked` on the answer to a HEAD request.
//!
//! The Bun server answers HEAD for a streamed body (a mail list) with
//! `Transfer-Encoding: chunked` and no `Content-Length`, as it would GET. The
//! HTTP library used here drops that header from every HEAD answer before it
//! writes one, and offers no way to keep it. So the answer carries a stand-in
//! header of the same length under a name nobody can guess, and the
//! connection's writer puts the real name in its place as the bytes go out.
//!
//! The stand-in name is made up once per process and never reaches a client,
//! so nothing a client stores — a mail body, a file — can contain it.

use std::io;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{ready, Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const REAL_NAME: &[u8; 17] = b"Transfer-Encoding";

/// The stand-in header name, as the HTTP library is given it (lower case).
pub fn stand_in_header() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let random: [u8; 7] = rand::random();
        // "x-s" and 14 hex digits: as long as the real name, and spelled the
        // same after the library capitalizes it ("X-S…").
        format!("x-s{}", hex::encode(random))
    })
}

/// Where the stand-in name is in `bytes`, in whatever letter case the HTTP
/// library wrote it ("X-S…" when it capitalizes header names).
fn find_stand_in(bytes: &[u8]) -> Option<usize> {
    let name = stand_in_header().as_bytes();
    bytes.windows(name.len()).position(|window| window.eq_ignore_ascii_case(name))
}

/// A connection whose writes have the stand-in header name replaced by
/// `Transfer-Encoding`. Reads pass through untouched.
pub struct HeadStream<T> {
    inner: T,
    /// How many bytes of a replaced name an earlier, short write has sent:
    /// the next write starts with the rest of that name.
    sent_of_name: usize,
}

impl<T> HeadStream<T> {
    pub fn new(inner: T) -> HeadStream<T> {
        HeadStream { inner, sent_of_name: 0 }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for HeadStream<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for HeadStream<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if self.sent_of_name == 0 && find_stand_in(buf).is_none() {
            return Pin::new(&mut self.inner).poll_write(cx, buf);
        }

        let mut rewritten = buf.to_vec();
        // Where each replaced name starts, relative to this buffer.
        let mut names: Vec<isize> = Vec::new();
        if self.sent_of_name > 0 {
            let rest = &REAL_NAME[self.sent_of_name..];
            let take = rest.len().min(rewritten.len());
            rewritten[..take].copy_from_slice(&rest[..take]);
            names.push(-(self.sent_of_name as isize));
        }
        let mut from = 0;
        while let Some(found) = find_stand_in(&rewritten[from..]) {
            let at = from + found;
            rewritten[at..at + REAL_NAME.len()].copy_from_slice(REAL_NAME);
            names.push(at as isize);
            from = at + REAL_NAME.len();
        }

        let written = ready!(Pin::new(&mut self.inner).poll_write(cx, &rewritten))?;
        let end = written as isize;
        self.sent_of_name = names
            .iter()
            .find(|&&start| start < end && end < start + REAL_NAME.len() as isize)
            .map_or(0, |&start| (end - start) as usize);
        Poll::Ready(Ok(written))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Accepts at most `step` bytes per write.
    struct Trickle {
        step: usize,
        written: Vec<u8>,
    }

    impl AsyncWrite for Trickle {
        fn poll_write(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            let take = self.step.min(buf.len());
            self.written.extend_from_slice(&buf[..take]);
            Poll::Ready(Ok(take))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn the_stand_in_is_as_long_as_the_real_name() {
        assert_eq!(stand_in_header().len(), REAL_NAME.len());
        assert!(stand_in_header().starts_with("x-s"));
        assert!(http::HeaderName::from_bytes(stand_in_header().as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn the_name_is_replaced_however_the_writes_are_cut() {
        // As the library writes it with and without capitalized header names.
        let capitalized = stand_in_header().replacen("x-s", "X-S", 1);
        for stand_in in [stand_in_header().to_string(), capitalized] {
            let head = format!("HTTP/1.1 200 OK\r\nETag: x\r\n{stand_in}: chunked\r\nDate: now\r\n\r\n");
            let twice = format!("{head}{head}");
            let expected = twice.replace(&stand_in, "Transfer-Encoding");
            for step in 1..=twice.len() {
                let mut stream = HeadStream::new(Trickle { step, written: Vec::new() });
                stream.write_all(twice.as_bytes()).await.unwrap();
                assert_eq!(String::from_utf8(stream.inner.written).unwrap(), expected, "writes of {step} bytes");
            }
        }
    }

    #[tokio::test]
    async fn everything_else_passes_through() {
        let text = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Stream: no\r\n\r\nhello";
        for step in [1, 7, 1000] {
            let mut stream = HeadStream::new(Trickle { step, written: Vec::new() });
            stream.write_all(text.as_bytes()).await.unwrap();
            assert_eq!(stream.inner.written, text.as_bytes());
        }
    }
}
