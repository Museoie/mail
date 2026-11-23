//! What the SMTP server and the code that handles its mail share: the
//! session, the envelope, the hooks and the listener options
//! (`smtp/types.ts`).

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub use super::data_stream::SmtpDataStream;

/// The value of one ESMTP parameter of `MAIL FROM` / `RCPT TO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmtpArgValue {
    /// A bare keyword, such as `SMTPUTF8` (`true` in the Bun server).
    Flag,
    /// `KEY=value`, with its xtext `+XX` escapes decoded.
    Value(String),
}

/// An address from `MAIL FROM` or `RCPT TO`, with its ESMTP parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpAddress {
    /// Empty for the null reverse-path `<>`.
    pub address: String,
    /// The parameters by upper-cased keyword; `None` when there were none
    /// (`false` in the Bun server).
    pub args: Option<BTreeMap<String, SmtpArgValue>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpEnvelope {
    /// `None` until `MAIL FROM` is accepted (`false` in the Bun server).
    pub mail_from: Option<SmtpAddress>,
    pub rcpt_to: Vec<SmtpAddress>,
    /// `7bit` or `8bitmime`, from the `BODY` parameter.
    pub body_type: String,
    pub smtp_utf8: bool,
    pub require_tls: bool,
}

impl Default for SmtpEnvelope {
    fn default() -> SmtpEnvelope {
        SmtpEnvelope {
            mail_from: None,
            rcpt_to: Vec::new(),
            body_type: "7bit".to_string(),
            smtp_utf8: false,
            require_tls: false,
        }
    }
}

/// Whatever the hooks want to remember about a connection: the Bun handlers
/// write extra properties (the credential version stamped at AUTH) onto the
/// session object, and this is where the same state goes. One value per type.
///
/// ```ignore
/// struct Credential { password_version: i64, read_only: bool }
/// session.state.insert(Credential { password_version: 3, read_only: false });
/// let version = session.state.get::<Credential>().map(|c| c.password_version);
/// ```
///
/// The server never reads or clears it: like the properties in the Bun
/// server it outlives RSET, a finished transaction and STARTTLS.
#[derive(Default)]
pub struct SmtpSessionState {
    values: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl SmtpSessionState {
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.values.get(&TypeId::of::<T>()).and_then(|value| value.downcast_ref())
    }

    pub fn get_mut<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        self.values.get_mut(&TypeId::of::<T>()).and_then(|value| value.downcast_mut())
    }

    /// Stores `value`, returning the one of the same type it replaces.
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) -> Option<T> {
        let previous = self.values.insert(TypeId::of::<T>(), Box::new(value));
        previous.and_then(|value| value.downcast().ok()).map(|value| *value)
    }

    pub fn remove<T: Any + Send + Sync>(&mut self) -> Option<T> {
        let previous = self.values.remove(&TypeId::of::<T>());
        previous.and_then(|value| value.downcast().ok()).map(|value| *value)
    }

    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
        self.values.contains_key(&TypeId::of::<T>())
    }
}

impl fmt::Debug for SmtpSessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SmtpSessionState({} values)", self.values.len())
    }
}

/// One client connection as the hooks see it.
#[derive(Debug)]
pub struct SmtpSession {
    /// Twenty hex digits, random per connection.
    pub id: String,
    /// The client: the PROXY header's when the listener takes one.
    pub remote_address: String,
    pub remote_port: u16,
    pub local_address: String,
    pub local_port: u16,
    /// Whether the connection is encrypted, by implicit TLS or STARTTLS.
    pub secure: bool,
    /// The username `on_auth` answered with, once AUTH succeeded.
    pub user: Option<String>,
    /// The HELO/EHLO argument, lowercased.
    pub host_name_appears_as: Option<String>,
    pub envelope: SmtpEnvelope,
    /// One more than the number of DATA transactions answered so far.
    pub transaction: u64,
    /// The hooks' own per-connection state.
    pub state: SmtpSessionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpAuthMethod {
    Plain,
    Login,
}

impl SmtpAuthMethod {
    /// `PLAIN` or `LOGIN`, as the Bun server names the method.
    pub fn as_str(self) -> &'static str {
        match self {
            SmtpAuthMethod::Plain => "PLAIN",
            SmtpAuthMethod::Login => "LOGIN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpAuthRequest {
    pub method: SmtpAuthMethod,
    pub username: String,
    pub password: String,
}

/// A refusal a hook answers with. `response_code` picks the reply code; left
/// `None` (or 0) each command falls back to its own: 535 for AUTH, 550 for
/// MAIL FROM and RCPT TO, 450 for DATA. The message is the reply text, and a
/// 421 closes the connection after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpHookError {
    pub response_code: Option<u16>,
    pub message: String,
}

impl SmtpHookError {
    /// A refusal with the command's default reply code.
    pub fn new(message: impl Into<String>) -> SmtpHookError {
        SmtpHookError { response_code: None, message: message.into() }
    }

    pub fn with_code(response_code: u16, message: impl Into<String>) -> SmtpHookError {
        SmtpHookError { response_code: Some(response_code), message: message.into() }
    }

    /// `err.responseCode || fallback`.
    pub(crate) fn code_or(&self, fallback: u16) -> u16 {
        self.response_code.filter(|code| *code != 0).unwrap_or(fallback)
    }
}

impl fmt::Display for SmtpHookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SmtpHookError {}

/// What `on_auth` answers when the lookup itself worked. A `user` (not
/// empty) accepts the credentials and becomes `session.user`; without one
/// the attempt is refused with `response_code` (535 unless set) and `message`
/// (`Error: Authentication credentials invalid` unless set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmtpAuthResponse {
    pub user: Option<String>,
    pub message: Option<String>,
    pub response_code: Option<u16>,
}

impl SmtpAuthResponse {
    pub fn accept(user: impl Into<String>) -> SmtpAuthResponse {
        SmtpAuthResponse { user: Some(user.into()), ..Default::default() }
    }

    /// The default refusal: `535 Error: Authentication credentials invalid`.
    pub fn reject() -> SmtpAuthResponse {
        SmtpAuthResponse::default()
    }
}

/// The hooks an SMTP listener calls: the seam between the protocol and the
/// mail handling. Each is awaited before the command is answered, and no
/// further command of the client is handled meanwhile.
///
/// An `Err` is the refusal sent to the client (see [`SmtpHookError`]). A hook
/// keeps running to its end when the client goes away first; only its answer
/// is then dropped.
///
/// A hook runs on its connection's task until the connection is gone, so
/// work that would block a thread (a password hash) belongs in
/// `spawn_blocking`. One that answers without waiting on anything is
/// answered at once, the way a hook that calls back synchronously is in the
/// Bun server. A hook that panics is reported to the listener's `on_error`
/// and its connection dropped: the session went with it.
///
/// ```ignore
/// impl SmtpHandler for Mail {
///     async fn on_rcpt_to(&self, address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
///         if self.has_mailbox(&address.address).await { Ok(()) }
///         else { Err(SmtpHookError::with_code(550, "5.1.1 Error: no such mailbox here")) }
///     }
///     // ...
/// }
/// ```
pub trait SmtpHandler: Send + Sync + 'static {
    /// `AUTH PLAIN` / `AUTH LOGIN`, once the credentials are in hand.
    fn on_auth(
        &self,
        auth: SmtpAuthRequest,
        session: &mut SmtpSession,
    ) -> impl Future<Output = Result<SmtpAuthResponse, SmtpHookError>> + Send;

    /// `MAIL FROM`, after its syntax and parameters were checked. The
    /// envelope's `mail_from` is set only once this accepts.
    fn on_mail_from(
        &self,
        address: &SmtpAddress,
        session: &mut SmtpSession,
    ) -> impl Future<Output = Result<(), SmtpHookError>> + Send;

    /// `RCPT TO`. The address joins `rcpt_to` only once this accepts.
    fn on_rcpt_to(
        &self,
        address: &SmtpAddress,
        session: &mut SmtpSession,
    ) -> impl Future<Output = Result<(), SmtpHookError>> + Send;

    /// `DATA`: called before the client is told to send, with the message as
    /// it arrives — dot-unstuffed, terminator removed. The reply goes out
    /// once this has answered and the message has been read to its end; what
    /// a refusing hook leaves unread (by dropping `stream`) is discarded by
    /// the server.
    ///
    /// The listener's `size` is advertised and checked against a declared
    /// `SIZE=`, but the listener does not count the octets of DATA, as the
    /// Bun server's does not. A hook that holds the message to it calls
    /// [`SmtpDataStream::enforce_size_limit`]: a read past the ceiling then
    /// fails with [`super::SmtpDataTooLarge`], which converts into the 552 the
    /// Bun mail handling answers.
    fn on_data(
        &self,
        stream: SmtpDataStream,
        session: &mut SmtpSession,
    ) -> impl Future<Output = Result<(), SmtpHookError>> + Send;
}

pub(crate) type HookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`SmtpHandler`] with its futures boxed, so listeners need not be generic.
pub(crate) trait DynSmtpHandler: Send + Sync + 'static {
    fn on_auth<'a>(
        &'a self,
        auth: SmtpAuthRequest,
        session: &'a mut SmtpSession,
    ) -> HookFuture<'a, Result<SmtpAuthResponse, SmtpHookError>>;

    fn on_mail_from<'a>(
        &'a self,
        address: &'a SmtpAddress,
        session: &'a mut SmtpSession,
    ) -> HookFuture<'a, Result<(), SmtpHookError>>;

    fn on_rcpt_to<'a>(
        &'a self,
        address: &'a SmtpAddress,
        session: &'a mut SmtpSession,
    ) -> HookFuture<'a, Result<(), SmtpHookError>>;

    fn on_data<'a>(
        &'a self,
        stream: SmtpDataStream,
        session: &'a mut SmtpSession,
    ) -> HookFuture<'a, Result<(), SmtpHookError>>;
}

impl<H: SmtpHandler> DynSmtpHandler for H {
    fn on_auth<'a>(
        &'a self,
        auth: SmtpAuthRequest,
        session: &'a mut SmtpSession,
    ) -> HookFuture<'a, Result<SmtpAuthResponse, SmtpHookError>> {
        Box::pin(SmtpHandler::on_auth(self, auth, session))
    }

    fn on_mail_from<'a>(
        &'a self,
        address: &'a SmtpAddress,
        session: &'a mut SmtpSession,
    ) -> HookFuture<'a, Result<(), SmtpHookError>> {
        Box::pin(SmtpHandler::on_mail_from(self, address, session))
    }

    fn on_rcpt_to<'a>(
        &'a self,
        address: &'a SmtpAddress,
        session: &'a mut SmtpSession,
    ) -> HookFuture<'a, Result<(), SmtpHookError>> {
        Box::pin(SmtpHandler::on_rcpt_to(self, address, session))
    }

    fn on_data<'a>(
        &'a self,
        stream: SmtpDataStream,
        session: &'a mut SmtpSession,
    ) -> HookFuture<'a, Result<(), SmtpHookError>> {
        Box::pin(SmtpHandler::on_data(self, stream, session))
    }
}

/// The hooks of a listener; several listeners share one handler.
#[derive(Clone)]
pub struct SharedSmtpHandler(pub(crate) Arc<dyn DynSmtpHandler>);

impl SharedSmtpHandler {
    pub fn new<H: SmtpHandler>(handler: H) -> SharedSmtpHandler {
        SharedSmtpHandler(Arc::new(handler))
    }
}

impl<H: SmtpHandler> From<Arc<H>> for SharedSmtpHandler {
    fn from(handler: Arc<H>) -> SharedSmtpHandler {
        SharedSmtpHandler(handler)
    }
}

impl fmt::Debug for SharedSmtpHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedSmtpHandler")
    }
}

/// Where a listener gets its TLS acceptor: called for every handshake, so a
/// renewed certificate is served without a restart. The listeners of this
/// server pass [`crate::tls::acceptor`].
pub type SmtpTlsSource = Arc<dyn Fn() -> anyhow::Result<crate::tls::TlsAcceptor> + Send + Sync>;

#[derive(Clone)]
pub struct SmtpServerOptions {
    /// Implicit TLS: the handshake runs before the greeting (port 465).
    pub secure: bool,
    /// Each connection starts with a PROXY protocol v1 header naming the client.
    pub proxy_protocol: bool,
    /// Accept AUTH before STARTTLS on a cleartext connection.
    pub allow_insecure_auth: bool,
    /// Server name for the greeting; the host name when unset.
    pub name: Option<String>,
    /// Set when the listener serves TLS (`key` and `cert` in the Bun server):
    /// STARTTLS is then offered, and an implicit-TLS listener can handshake.
    pub tls: Option<SmtpTlsSource>,
    /// Advertised in EHLO and enforced against a `MAIL FROM ... SIZE=` declaration.
    pub size: u64,
    pub max_clients: usize,
    /// How long `close` waits for open connections before answering them 421.
    pub close_timeout: Duration,
    /// Idle time after which a connection is answered 421 and closed; a
    /// minute when unset.
    pub socket_timeout: Option<Duration>,
    pub handler: SharedSmtpHandler,
}

impl fmt::Debug for SmtpServerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpServerOptions")
            .field("secure", &self.secure)
            .field("proxy_protocol", &self.proxy_protocol)
            .field("allow_insecure_auth", &self.allow_insecure_auth)
            .field("name", &self.name)
            .field("tls", &self.tls.is_some())
            .field("size", &self.size)
            .field("max_clients", &self.max_clients)
            .field("close_timeout", &self.close_timeout)
            .field("socket_timeout", &self.socket_timeout)
            .finish()
    }
}
