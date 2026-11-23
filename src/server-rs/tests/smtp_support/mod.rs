//! What the SMTP integration tests share: a throwaway certificate and a
//! scripted client that reports what it saw the way the recorder's does.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::time::Instant;

use mail_server::smtp::SmtpTlsSource;

/// A self-signed certificate for `localhost`, made with the `openssl` CLI in
/// a temporary directory that goes with the process. No key is ever committed.
pub struct TestCertificate {
    _dir: tempfile::TempDir,
    pub cert: PathBuf,
    pub key: PathBuf,
}

pub fn generate_certificate() -> TestCertificate {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let status = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "key.pem", "-out", "cert.pem"])
        .args(["-days", "1", "-subj", "/CN=localhost"])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the openssl CLI is installed");
    assert!(status.success(), "openssl could not create a test certificate");
    TestCertificate { cert: dir.path().join("cert.pem"), key: dir.path().join("key.pem"), _dir: dir }
}

/// The certificate of this test process, with `SSL_CERTIFICATE` and
/// `SSL_CERTIFICATE_KEY` pointing at it so `mail_server::tls::acceptor`
/// serves it.
pub fn certificate() -> &'static TestCertificate {
    static CERTIFICATE: OnceLock<TestCertificate> = OnceLock::new();
    CERTIFICATE.get_or_init(|| {
        let certificate = generate_certificate();
        std::env::set_var("SSL_CERTIFICATE", &certificate.cert);
        std::env::set_var("SSL_CERTIFICATE_KEY", &certificate.key);
        certificate
    })
}

/// What the listeners of the real server pass as their TLS source: the
/// certificate on disk, read again for every handshake.
pub fn tls_source() -> SmtpTlsSource {
    certificate();
    Arc::new(mail_server::tls::acceptor)
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

type BoxedStream = Box<dyn Stream>;

/// A stream whose first write carries `preface` in front of it, in the same
/// segment: a PROXY header and the TLS ClientHello arriving in one read.
struct Prefaced<S> {
    preface: Vec<u8>,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefaced<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefaced<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.preface.is_empty() {
            return std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        }
        let mut joined = self.preface.clone();
        joined.extend_from_slice(buf);
        let preface = self.preface.len();
        match std::pin::Pin::new(&mut self.inner).poll_write(cx, &joined) {
            std::task::Poll::Ready(Ok(written)) if written >= preface => {
                self.preface.clear();
                std::task::Poll::Ready(Ok(written - preface))
            }
            std::task::Poll::Ready(Ok(written)) => {
                self.preface.drain(..written);
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// One byte per character, as the recorder writes what it sends and receives.
pub fn binary(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

/// The bytes of a "binary" string.
pub fn bytes_of(text: &str) -> Vec<u8> {
    text.chars().map(|c| u8::try_from(u32::from(c)).expect("a binary string holds bytes")).collect()
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

/// A scripted SMTP client. Its `reply`, `closed` and `silence` answer in the
/// words `record.ts` records, so a replay compares string for string.
pub struct Client {
    reader: Option<ReadHalf<BoxedStream>>,
    writer: Option<WriteHalf<BoxedStream>>,
    raw: Option<TcpStream>,
    buffer: Vec<u8>,
    ended: bool,
    pub local_port: u16,
}

impl Client {
    pub async fn connect(port: u16) -> Client {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.expect("the server accepts connections");
        let local_port = stream.local_addr().expect("a connected socket has an address").port();
        Client { reader: None, writer: None, raw: Some(stream), buffer: Vec::new(), ended: false, local_port }
    }

    fn halves(&mut self) -> (&mut ReadHalf<BoxedStream>, &mut WriteHalf<BoxedStream>) {
        if let Some(raw) = self.raw.take() {
            let (reader, writer) = tokio::io::split(Box::new(raw) as BoxedStream);
            self.reader = Some(reader);
            self.writer = Some(writer);
        }
        (self.reader.as_mut().expect("the client is connected"), self.writer.as_mut().expect("the client is connected"))
    }

    /// Reads once, for at most `wait`. False when nothing arrived in time.
    async fn fill(&mut self, wait: Duration) -> bool {
        if self.ended {
            tokio::time::sleep(wait).await;
            return false;
        }
        let mut chunk = [0u8; 16 * 1024];
        let (reader, _) = self.halves();
        match tokio::time::timeout(wait, reader.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {
                self.ended = true;
                true
            }
            Ok(Ok(read)) => {
                self.buffer.extend_from_slice(&chunk[..read]);
                true
            }
            Err(_) => false,
        }
    }

    /// Sends without waiting for the server to take it, reading meanwhile so
    /// a server that answers before it has read everything is not held up.
    pub async fn send(&mut self, data: &[u8]) {
        let mut sent = 0;
        let mut chunk = [0u8; 16 * 1024];
        while sent < data.len() {
            let ended = self.ended;
            let (reader, writer) = self.halves();
            tokio::select! {
                written = writer.write(&data[sent..]) => match written {
                    Ok(written) if written > 0 => sent += written,
                    _ => return,
                },
                read = reader.read(&mut chunk), if !ended => match read {
                    Ok(0) | Err(_) => self.ended = true,
                    Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                },
            }
        }
        let (_, writer) = self.halves();
        let _ = writer.flush().await;
    }

    pub async fn command(&mut self, line: &str) -> String {
        self.send(format!("{line}\r\n").as_bytes()).await;
        self.reply(Duration::from_secs(3)).await
    }

    /// A complete reply: its lines up to the one whose code is not followed by a dash.
    fn take_reply(&mut self) -> Option<String> {
        let mut start = 0;
        loop {
            let end = start + self.buffer[start..].windows(2).position(|pair| pair == b"\r\n")?;
            let line = &self.buffer[start..end];
            let is_last = line.len() >= 3
                && line[..3].iter().all(u8::is_ascii_digit)
                && (line.len() == 3 || line[3] == b' ');
            if is_last {
                let reply = binary(&self.buffer[..end]);
                self.buffer.drain(..end + 2);
                return Some(reply);
            }
            start = end + 2;
        }
    }

    fn take_rest(&mut self) -> String {
        binary(&std::mem::take(&mut self.buffer))
    }

    /// The next reply, `<closed>` when the server hung up instead, or
    /// `<timeout>`.
    pub async fn reply(&mut self, within: Duration) -> String {
        let deadline = Instant::now() + within;
        loop {
            if let Some(reply) = self.take_reply() {
                return reply;
            }
            if self.ended {
                return match self.buffer.is_empty() {
                    true => "<closed>".to_string(),
                    false => format!("<closed after {}>", quoted(&self.take_rest())),
                };
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return "<timeout>".to_string();
            }
            self.fill(left.min(Duration::from_millis(25))).await;
        }
    }

    /// `count` replies, as their number and a digest: for floods.
    pub async fn replies(&mut self, count: usize, within: Duration) -> String {
        let deadline = Instant::now() + within;
        let mut hash = Sha256::new();
        for index in 0..count {
            let reply = self.reply(deadline.saturating_duration_since(Instant::now())).await;
            if reply.starts_with('<') {
                return format!("{reply} after {index} replies");
            }
            hash.update(bytes_of(&reply));
            hash.update(b"\r\n");
        }
        format!("{count} replies sha256:{}", hex::encode(hash.finalize()))
    }

    /// Waits for the server to close its side: `closed` or `open`, followed
    /// by whatever it sent that no step read.
    pub async fn closed(&mut self, within: Duration) -> String {
        let deadline = Instant::now() + within;
        while !self.ended {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            self.fill(left.min(Duration::from_millis(25))).await;
        }
        let rest = self.take_rest();
        let state = if self.ended { "closed" } else { "open" };
        match rest.is_empty() {
            true => state.to_string(),
            false => format!("{state} after {}", quoted(&rest)),
        }
    }

    /// What arrived over `duration`: `silence`, `closed`, or the data.
    pub async fn silence(&mut self, duration: Duration) -> String {
        let deadline = Instant::now() + duration;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            self.fill(left).await;
        }
        let rest = self.take_rest();
        if !rest.is_empty() {
            return format!("data {}", quoted(&rest));
        }
        if self.ended { "closed" } else { "silence" }.to_string()
    }

    pub fn is_closed(&self) -> bool {
        self.ended
    }

    /// The client side of a TLS handshake, accepting any certificate.
    pub async fn start_tls(&mut self) -> String {
        self.start_tls_behind(&[]).await
    }

    /// A TLS handshake whose ClientHello leaves in one write with `preface`
    /// in front of it.
    pub async fn start_tls_behind(&mut self, preface: &[u8]) -> String {
        let stream: BoxedStream = match self.raw.take() {
            Some(raw) => Box::new(raw),
            None => {
                let reader = self.reader.take().expect("the client is connected");
                reader.unsplit(self.writer.take().expect("the client is connected"))
            }
        };
        let stream: BoxedStream = Box::new(Prefaced { preface: preface.to_vec(), inner: stream });
        let connector = mail_server::tls::insecure_connector();
        let name = rustls::pki_types::ServerName::try_from("localhost").expect("a valid server name");
        match connector.connect(name, stream).await {
            Ok(secure) => {
                let (reader, writer) = tokio::io::split(Box::new(secure) as BoxedStream);
                self.reader = Some(reader);
                self.writer = Some(writer);
                "secure".to_string()
            }
            Err(_) => {
                self.ended = true;
                "failed".to_string()
            }
        }
    }

    /// Half-closes: the client's FIN (after a TLS close_notify, over TLS).
    pub async fn end(&mut self) {
        if self.reader.is_none() && self.raw.is_none() {
            return;
        }
        let (_, writer) = self.halves();
        let _ = writer.shutdown().await;
    }

    /// Closes the socket as it is.
    pub fn destroy(&mut self) {
        self.reader = None;
        self.writer = None;
        self.raw = None;
        self.ended = true;
    }
}
