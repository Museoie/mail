//! One IMAP connection (imap/handler.ts): reading command lines and literals
//! off the socket, and handing each parsed command to the session.
//!
//! The Bun server appends every TCP segment to a buffer as it arrives and
//! pauses the socket once too much is held unread. This reads only when the
//! drain needs more, which bounds the same thing: a connection never holds
//! more than one command's worth of octets plus one read.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures_util::FutureExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, ReadHalf};

use super::body_budget::{get_body_budget_wait_ms, run_in_body_budget_context};
use super::capabilities::get_capabilities;
use super::command_budget::{acquire_command_budget, release_command_budget};
use super::command_budget_hold::{create_command_budget_hold, run_in_command_budget_context, CommandBudgetHold};
use super::idle_manager::SOCKET_TIMEOUT_MS;
use super::literal_spool::{remove_message_literals, LiteralSpool};
use super::log;
use super::parsers::command_parser::parse_command;
use super::session::ImapSession;
use super::session_buffer::SessionBuffer;
use super::session_registry::{register_connection, unregister_connection, RegisteredSession};
use super::socket::{log_socket_error, BoxedStream, SocketHandle};
use super::trace::{clip, imap_trace, redact_credentials, Direction};
use super::types::{BodySection, FetchDataItem, ImapRequest, Literal, MessageLiteral, SequenceRange, UidRequest};
use super::util::{js_is_whitespace, js_trim};
use crate::proxy_protocol::Peer;
use crate::tls::TlsAcceptor;
use crate::{js, logger, sso};

#[cfg(test)]
#[path = "handler_tests.rs"]
mod tests;

/// The largest whole RFC 5322 message the process will hold at once
/// (`lib/message-size.ts`): every surface that materializes a message shares
/// this ceiling, SMTP DATA and the IMAP APPEND literal alike.
pub const MAX_MESSAGE_BYTES: u64 = 35 * 1024 * 1024;

/// Whether a command goes through the global command budget.
///
/// The budgeted ones are those whose processing (database reads, mailbox and
/// message-list materialization, response formatting) has a real memory
/// footprint, and that the historical memory-pressure traffic shapes consist
/// of: several connections each running SELECT sweeps, bulk FETCH or rapid
/// STATUS. Protocol and handshake commands stay ungated; they are cheap. That
/// only keeps them off the GLOBAL budget, not off their own session's serial
/// drain.
///
/// A `match` with no wildcard, so a new request variant has to be placed.
fn is_budgeted(request: &ImapRequest) -> bool {
    match request {
        ImapRequest::Capability => false,
        ImapRequest::Noop => false,
        ImapRequest::Login(_) => false,
        ImapRequest::Authenticate(_) => false,
        ImapRequest::List(_) => true,
        ImapRequest::Lsub(_) => true,
        ImapRequest::Select(_) => true,
        ImapRequest::Examine(_) => true,
        ImapRequest::Create { .. } => false,
        ImapRequest::Delete { .. } => false,
        ImapRequest::Rename { .. } => false,
        ImapRequest::Subscribe { .. } => false,
        ImapRequest::Unsubscribe { .. } => false,
        ImapRequest::Status(_) => true,
        ImapRequest::Append(_) => true,
        ImapRequest::Idle => false,
        ImapRequest::Check => false,
        ImapRequest::Close => false,
        ImapRequest::Expunge => true,
        ImapRequest::Search(_) => true,
        ImapRequest::Fetch(_) => true,
        ImapRequest::Store(_) => true,
        ImapRequest::Copy(_) => true,
        ImapRequest::Move(_) => true,
        ImapRequest::Uid(_) => true,
        ImapRequest::Id => false,
        ImapRequest::Done => false,
        ImapRequest::Logout => false,
        ImapRequest::Starttls => false,
        ImapRequest::Namespace => false,
        ImapRequest::Enable { .. } => false,
        ImapRequest::Unselect => false,
        ImapRequest::Getquotaroot { .. } => false,
    }
}

// Per-command diagnostic log thresholds. A command is "interesting" (logged
// at INFO) if ANY of them is exceeded; otherwise the same payload drops to
// DEBUG, so the noise floor does not drown the tail during a client retry
// storm. A budgeted command that queued this long behind the command budget
// surfaces too, even if it then runs in a couple of milliseconds: that wait
// is the only signal that the budget is undersized.
pub const INTERESTING_RSS_DELTA_MB: i64 = 1;
pub const INTERESTING_DURATION_MS: u64 = 100;
pub const INTERESTING_RESPONSE_BYTES: u64 = 4096;

/// A trailing `{N}` / `{N+}`: a literal declaration (RFC 3501 §4.3, RFC
/// 7888). It has to stand as its own argument, so it is preceded by white
/// space or begins the line; without that anchor a password ending in `{5}`
/// reads as a declaration. Returns the declared size and whether it is
/// non-synchronizing.
///
/// `/(?:^|\s)\{(\d+)(\+?)\}\s*$/`
fn literal_declaration(line: &str) -> Option<(u64, bool)> {
    let line = line.trim_end_matches(js_is_whitespace);
    let rest = line.strip_suffix('}')?;
    let (rest, non_synchronizing) = match rest.strip_suffix('+') {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let digits = rest.bytes().rev().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let (before, size) = rest.split_at(rest.len() - digits);
    let before = before.strip_suffix('{')?;
    if !before.is_empty() && !before.chars().next_back().is_some_and(js_is_whitespace) {
        return None;
    }
    // More digits than fit is more octets than any cap allows.
    Some((size.parse().unwrap_or(u64::MAX), non_synchronizing))
}

/// A command whose grammar admits more arguments than it has consumed.
/// SEARCH is `1*(SP search-key)`, so parsing is not evidence that the client
/// is finished: `A1 SEARCH SUBJECT {3+}` is a whole command and still
/// legally chains ` FROM {3+}`. Every other literal-bearing command has
/// fixed arity. UID wraps its subcommand, so `UID SEARCH` inherits the list.
fn chains_unbounded_arguments(request: &ImapRequest) -> bool {
    let mut request = request;
    while let ImapRequest::Uid(uid) = request {
        request = uid.request();
    }
    matches!(request, ImapRequest::Search(_))
}

/// RFC 3501 §9 puts no length on a tag and UUID-shaped ones run 36 octets;
/// 128 clears every shape in use while staying far below a literal payload.
const MAX_COMMAND_TAG_BYTES: usize = 128;

/// Tag of a command line, and the only source of the tag on the wire: the
/// first token of the FIRST line.
///
/// Held to `1*<ASTRING-CHAR except "+">` and to a length, because not every
/// line reaching here is a command: a peer that ships the payload of a
/// refused synchronizing literal anyway has those octets read as command
/// text, and an unbounded first token would write the whole payload back
/// inside a BAD completion. A token over the ceiling does not match at all,
/// so the line is answered untagged rather than clipped and echoed.
///
/// `/^\s*([^\s(){%*"\\+\x00-\x1f\x7f-\uffff]{1,128})(?=\s|$)/`
fn command_tag(input: &str) -> Option<&str> {
    let input = input.trim_start_matches(js_is_whitespace);
    let is_tag_char = |c: char| {
        !js_is_whitespace(c) && !matches!(c, '(' | ')' | '{' | '%' | '*' | '"' | '\\' | '+') && (' '..'\u{7f}').contains(&c)
    };
    let end = input.find(|c| !is_tag_char(c)).unwrap_or(input.len());
    if end == 0 || end > MAX_COMMAND_TAG_BYTES {
        return None;
    }
    // The token has to end at white space or at the end of the line.
    if input[end..].chars().next().is_some_and(|next| !js_is_whitespace(next)) {
        return None;
    }
    Some(&input[..end])
}

/// What a line with no answerable tag is completed with. `BAD` reads like a
/// tag and is a legal one, so a peer holding a real BAD-tagged command would
/// match the completion to that one instead.
const UNTAGGED: &str = "*";

/// Verb of a command line, upper-cased: the second token of the first line.
///
/// `/^\s*\S+\s+(\S+)/`
fn command_verb(input: &str) -> String {
    let mut tokens = input.split(js_is_whitespace).filter(|token| !token.is_empty());
    tokens.next();
    tokens.next().map(str::to_uppercase).unwrap_or_default()
}

// Literal ceilings. Without them a single unauthenticated socket pins
// arbitrary heap: `a1 APPEND INBOX {999999999+}` makes the drain hold a
// gigabyte before LOGIN is ever parsed.
//
// APPEND carries a whole message, so its ceiling is the one every surface
// that materializes a message shares. It is offered only to an authenticated
// session: APPEND answers `NO Not authenticated` either way, so a
// declaration before sign-in buys nothing but heap.
const MAX_APPEND_LITERAL_BYTES: u64 = MAX_MESSAGE_BYTES;

// Every other literal is a mailbox name, a credential or a SEARCH string.
// RFC 2683 §3.2.1.5 asks servers to accept at least 8000 octets of command
// text; 8 KiB covers that with nothing left over for an attacker.
const MAX_LITERAL_BYTES: u64 = 8 * 1024;

/// Command text held that the drain has not consumed. Past this the
/// connection stops reading until the drain catches up. Sized to hold the
/// longest plausible real command (a UID set naming thousands of messages)
/// with room to spare.
const MAX_UNCONSUMED_COMMAND_BYTES: usize = 64 * 1024;

// The per-literal cap alone does not bound a COMMAND: declarations chain.
// These bound how many links one command may hold and how many octets;
// how fast a session may spend them is paced where the chain is read.
const MAX_LITERALS_PER_COMMAND: usize = 64;
const MAX_PENDING_COMMAND_BYTES: u64 = MAX_APPEND_LITERAL_BYTES + 64 * 1024;

/// Most octets taken off the socket in one read.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Verdict on a literal declaration. `Closed` is distinct from `Refused`
/// because the drain must stop reading, not just skip the declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiteralOutcome {
    Accepted,
    Refused,
    Closed,
}

/// Cap for a literal about to be declared. Only an APPEND's message
/// argument, in an authenticated session, is offered the message-sized
/// ceiling: it is the one literal spooled to disk.
fn literal_cap_for(is_message: bool, authenticated: bool) -> u64 {
    if authenticated && is_message {
        MAX_APPEND_LITERAL_BYTES
    } else {
        MAX_LITERAL_BYTES
    }
}

/// Whether the literal `command_text` has just declared is an APPEND's
/// message argument rather than, say, a mailbox name sent as a literal. A
/// probe stands in for the declared literal and the parser says where it
/// landed.
fn declares_append_message(command_text: &str, literals: &[Literal]) -> bool {
    if command_verb(command_text) != "APPEND" {
        return false;
    }
    let probe = MessageLiteral { path: String::new(), size: 0 };
    let mut with_probe = literals.to_vec();
    with_probe.push(Literal::Message(probe.clone()));
    let parsed = parse_command(command_text, Some(&with_probe));
    matches!(
        parsed.value.filter(|_| parsed.success).map(|parsed| parsed.request),
        Some(ImapRequest::Append(append)) if append.message == probe
    )
}

/// Short summary of a request for the per-command diagnostic log. Never
/// emits mail contents, and is capped so a runaway pipeline does not blow
/// the log volume; the sequence range and item names are enough to identify
/// the shapes that cost memory.
pub fn describe_imap_command(request: &ImapRequest) -> String {
    fn summary(text: String) -> String {
        if js::utf16_len(&text) > 180 {
            format!("{}...", js::utf16_slice(&text, 0, 177))
        } else {
            text
        }
    }
    fn ranges_of(ranges: &[SequenceRange]) -> String {
        let number = super::types::js_number_string;
        ranges
            .iter()
            .map(|range| match range.end {
                None => number(range.start),
                Some(end) => format!("{}:{}", number(range.start), number(end)),
            })
            .collect::<Vec<_>>()
            .join(",")
    }
    /// `JSON.stringify(request.data)`.
    fn data_json(request: &ImapRequest) -> String {
        let whole = request.to_json_string();
        let prefix = format!("{{\"type\":\"{}\",\"data\":", request.type_name());
        whole.strip_prefix(&prefix).and_then(|rest| rest.strip_suffix('}')).unwrap_or_default().to_string()
    }

    // `UID UID ... FETCH` nests as deep as the line is long.
    let mut prefix = String::new();
    let mut request = request;
    while let ImapRequest::Uid(uid) = request {
        prefix.push_str("UID ");
        request = uid.request();
    }

    let described = match request {
        ImapRequest::Fetch(data) => {
            let items: Vec<&str> = data.data_items.iter().map(FetchDataItem::type_name).collect();
            summary(format!("FETCH {} ({})", ranges_of(&data.sequence_set.ranges), items.join(" ")))
        }
        ImapRequest::Store(data) => summary(format!(
            "STORE {} {} {}",
            ranges_of(&data.sequence_set.ranges),
            data.operation.as_str(),
            data.flags.join(" ")
        )),
        ImapRequest::Search(data) => {
            // Criteria nest arbitrarily; only the head of them is kept.
            let mut criteria = String::from("[");
            for (index, criterion) in data.criteria.iter().enumerate() {
                if index > 0 {
                    criteria.push(',');
                }
                criterion.write_json(&mut criteria);
            }
            criteria.push(']');
            summary(format!("SEARCH {}", js::utf16_slice(&criteria, 0, 120)))
        }
        ImapRequest::Copy(data) => summary(format!("COPY {} {}", ranges_of(&data.sequence_set.ranges), data.mailbox)),
        ImapRequest::Move(data) => summary(format!("MOVE {} {}", ranges_of(&data.sequence_set.ranges), data.mailbox)),
        ImapRequest::Select(data) | ImapRequest::Examine(data) => {
            summary(format!("{} {}", request.type_name(), data.mailbox))
        }
        ImapRequest::List(data) | ImapRequest::Lsub(data) => {
            summary(format!("{} \"{}\" \"{}\"", request.type_name(), data.reference, data.pattern))
        }
        ImapRequest::Status(data) => {
            let items: Vec<&str> = data.items.iter().map(|item| item.as_str()).collect();
            summary(format!("STATUS {} ({})", data.mailbox, items.join(" ")))
        }
        ImapRequest::Append(data) => summary(format!("APPEND {} {}B", data.mailbox, data.message.size)),
        ImapRequest::Create { .. }
        | ImapRequest::Delete { .. }
        | ImapRequest::Rename { .. }
        | ImapRequest::Subscribe { .. }
        | ImapRequest::Unsubscribe { .. }
        | ImapRequest::Getquotaroot { .. } => {
            summary(format!("{} {}", request.type_name(), js::utf16_slice(&data_json(request), 0, 160)))
        }
        // NEVER the password.
        ImapRequest::Login(data) => summary(format!("LOGIN {}", data.username)),
        ImapRequest::Authenticate(data) => summary(format!("AUTHENTICATE {}", data.mechanism)),
        ImapRequest::Enable { capabilities } => summary(format!("ENABLE {}", capabilities.join(" "))),
        other => other.type_name().to_string(),
    };
    format!("{prefix}{described}")
}

/// What runs a parsed command. The server's own is [`SessionDispatch`].
pub trait Dispatch: Send {
    fn handle_request(
        &mut self,
        session: &Arc<ImapSession>,
        tag: &str,
        request: ImapRequest,
    ) -> impl Future<Output = ()> + Send;

    /// The line a client sent in answer to an AUTHENTICATE challenge.
    fn sasl_response(
        &mut self,
        session: &Arc<ImapSession>,
        tag: &str,
        response: &str,
    ) -> impl Future<Output = ()> + Send {
        async move { session.authenticate(tag, "PLAIN", Some(response)).await }
    }
}

/// Hands each command to the session: [`handle_request`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionDispatch;

impl Dispatch for SessionDispatch {
    async fn handle_request(&mut self, session: &Arc<ImapSession>, tag: &str, request: ImapRequest) {
        handle_request(session, tag, request).await;
    }
}

/// How one pass over the buffered input ended.
enum Drained {
    /// Everything held was consumed, or what is held is not complete yet.
    NeedData,
    /// The session ended the connection; nothing more is to be read.
    Closed,
    /// STARTTLS was answered OK: the connection upgrades before it reads on.
    Upgrade(TlsAcceptor),
}

/// One IMAP connection: its socket, its session and the command text it is
/// in the middle of reading.
pub struct ImapRequestHandler<D: Dispatch = SessionDispatch> {
    dispatch: D,
    is_tls: bool,
    peer: Peer,
    session: Arc<ImapSession>,
    read_half: Option<ReadHalf<BoxedStream>>,

    // Octets, not a string: `{N}` counts OCTETS.
    buffer: SessionBuffer,

    // Literal continuation state. `pending_command` is the command text
    // assembled so far, holding the `{N}` markers but NOT the payloads;
    // `pending_literals` carries the payloads in wire order, and the two
    // travel together into the parser. `awaiting_literal` means octets are
    // still outstanding; cleared beside a pending command it means the
    // payload landed and the rest of that line is still to come.
    //
    // An APPEND message is the one payload not held in memory: `spool`
    // writes its octets to disk as they arrive, and what lands on
    // `pending_literals` is the file.
    pending_command: Option<String>,
    pending_literals: Vec<Literal>,
    spool: Option<LiteralSpool>,
    /// Octets held on `pending_literals`, carried rather than recomputed.
    pending_literal_bytes: u64,
    awaiting_literal: bool,
    literal_bytes_needed: u64,

    // Over-cap LITERAL+ recovery. A non-synchronizing payload is already
    // inbound when its declaration is read, so it cannot be refused, only
    // counted and thrown away; then the rest of that command line is
    // swallowed too, because resuming the line splitter mid-command would
    // hand the remaining arguments to the parser as a fresh command.
    discard_bytes_remaining: u64,
    discard_to_end_of_command: bool,

    /// Reading is held back because too much command text is unread.
    paused: bool,
}

impl ImapRequestHandler<SessionDispatch> {
    pub fn new(stream: BoxedStream, peer: Peer, is_tls: bool) -> ImapRequestHandler<SessionDispatch> {
        ImapRequestHandler::with_dispatch(stream, peer, is_tls, SessionDispatch)
    }
}

/// Opens the socket for `stream` and the session on it.
fn open_session(stream: BoxedStream, peer: &Peer, is_tls: bool) -> (Arc<ImapSession>, ReadHalf<BoxedStream>) {
    let (socket, read_half) = SocketHandle::open(stream, peer.clone());
    let session = ImapSession::new(socket.clone(), is_tls);
    register_connection(session.clone());

    // An inactivity timeout, so a connection cannot hang forever.
    socket.set_timeout(SOCKET_TIMEOUT_MS);
    let timed_out = Arc::downgrade(&session);
    socket.on_timeout(move || {
        if let Some(session) = timed_out.upgrade() {
            log::info("IMAP socket timeout", json!({ "component": "imap" }));
            session.write("* BYE Timeout\r\n");
            session.close();
        }
    });
    (session, read_half)
}

impl<D: Dispatch> ImapRequestHandler<D> {
    pub fn with_dispatch(stream: BoxedStream, peer: Peer, is_tls: bool, dispatch: D) -> ImapRequestHandler<D> {
        let (session, read_half) = open_session(stream, &peer, is_tls);
        ImapRequestHandler {
            dispatch,
            is_tls,
            peer,
            session,
            read_half: Some(read_half),
            buffer: SessionBuffer::new(),
            pending_command: None,
            pending_literals: Vec::new(),
            spool: None,
            pending_literal_bytes: 0,
            awaiting_literal: false,
            literal_bytes_needed: 0,
            discard_bytes_remaining: 0,
            discard_to_end_of_command: false,
            paused: false,
        }
    }

    /// The session currently on the connection. A STARTTLS upgrade replaces it.
    pub fn session(&self) -> &Arc<ImapSession> {
        &self.session
    }

    fn remote(&self) -> String {
        self.session.socket().remote()
    }

    /// Greets the client and serves the connection until it closes.
    pub async fn run(mut self) {
        let greeting =
            format!("* OK [CAPABILITY {}] IMAP4rev1 Service Ready\r\n", get_capabilities(self.is_tls, sso::get_sso().is_some()));
        self.session.socket().write(Bytes::from(greeting));

        loop {
            // The IDLE notifier writes untagged responses from a delivery,
            // outside this loop; taking the session's turn for the whole
            // pass is what keeps its EXPUNGE out of the middle of a FETCH
            // response.
            let drained = {
                let session = self.session.clone();
                let _turn = session.serial().await;
                self.drain_commands().await
            };
            match drained {
                Drained::NeedData | Drained::Closed => {}
                Drained::Upgrade(acceptor) => {
                    if self.upgrade(acceptor).await {
                        continue;
                    }
                    break;
                }
            }
            if !self.read_more().await {
                break;
            }
        }

        self.finish().await;
    }

    /// Removes every spooled message no command has taken yet.
    async fn discard_held_literals(&mut self) {
        if let Some(spool) = self.spool.take() {
            spool.discard().await;
        }
        remove_message_literals(&self.pending_literals).await;
    }

    fn reset_framing(&mut self) {
        self.pending_command = None;
        self.pending_literals = Vec::new();
        self.pending_literal_bytes = 0;
        self.awaiting_literal = false;
        self.literal_bytes_needed = 0;
        self.discard_bytes_remaining = 0;
        self.discard_to_end_of_command = false;
    }

    /// Octets buffered that the drain has not agreed to receive. The one
    /// thing it has agreed to is an announced literal payload, whose
    /// declaration was already held to the cap for its command; what remains
    /// is command text nothing has read yet. A spooled payload is not
    /// exempt: it is written out as it arrives.
    fn unconsumed_command_bytes(&self) -> usize {
        let awaited = if self.awaiting_literal && self.spool.is_none() {
            usize::try_from(self.literal_bytes_needed).unwrap_or(usize::MAX)
        } else {
            0
        };
        self.buffer.len().saturating_sub(awaited)
    }

    /// Takes the next octets off the socket. False when there are no more to
    /// come: the peer closed, the socket failed, or the session ended it.
    async fn read_more(&mut self) -> bool {
        let socket = self.session.socket().clone();
        let over_cap = self.unconsumed_command_bytes() > MAX_UNCONSUMED_COMMAND_BYTES;
        let buffered_bytes = self.buffer.len();
        let Some(read_half) = self.read_half.as_mut() else {
            return false;
        };
        if socket.is_destroyed() {
            return false;
        }
        if socket.is_writable_ended() {
            linger(&socket, read_half).await;
            return false;
        }

        // Stop reading once more command text is held than any command can
        // be. Not closing: the peer is told nothing and the connection is
        // left for the inactivity timeout to end.
        if over_cap {
            if !self.paused {
                self.paused = true;
                log::info(
                    "IMAP unread command text over cap; pausing socket",
                    json!({ "component": "imap", "bufferedBytes": buffered_bytes, "remote": socket.remote() }),
                );
            }
            socket.ended().await;
            if !socket.is_destroyed() {
                linger(&socket, read_half).await;
            }
            return false;
        }
        self.paused = false;

        let mut chunk = BytesMut::with_capacity(READ_CHUNK_BYTES);
        let read = tokio::select! {
            biased;
            _ = socket.ended() => {
                if !socket.is_destroyed() {
                    linger(&socket, read_half).await;
                }
                return false;
            }
            read = read_half.read_buf(&mut chunk) => read,
        };
        match read {
            Ok(0) => {
                log::debug("IMAP connection closed", json!({ "component": "imap" }));
                false
            }
            Ok(read) => {
                socket.touch();
                // A short read should not pin the whole read buffer.
                let segment = if read < READ_CHUNK_BYTES / 4 { Bytes::copy_from_slice(&chunk) } else { chunk.freeze() };
                self.buffer.push(segment);
                true
            }
            Err(error) => {
                log_socket_error(&error, &socket.remote());
                socket.destroy();
                false
            }
        }
    }

    /// The connection is over: drop what was held for it, forget the
    /// session, and let what is still queued reach the peer.
    async fn finish(mut self) {
        self.discard_held_literals().await;
        self.session.cleanup();
        let socket = self.session.socket().clone();
        if !socket.is_destroyed() {
            // The peer closed first: flush, then close our side too.
            super::close_socket::close_socket(&socket);
            socket.write_side_closed().await;
        }
        socket.destroy();
    }

    /// STARTTLS was answered OK. Runs the TLS handshake on the stream and
    /// starts a clean session inside it. False when the connection is gone.
    ///
    /// RFC 2595 §2.1: a server MUST discard any knowledge obtained from the
    /// client before TLS. The rest of the cleartext input — commands an
    /// attacker can pipeline into the same segment as STARTTLS — is dropped
    /// here, never dispatched and never answered inside the victim's
    /// encrypted channel (CVE-2011-0411 class). The new session offers no
    /// STARTTLS, and a half-finished SASL exchange does not carry over.
    async fn upgrade(&mut self, acceptor: TlsAcceptor) -> bool {
        self.buffer.clear();
        self.discard_held_literals().await;
        self.reset_framing();

        let cleartext = self.session.clone();
        let write_half = cleartext.socket().handover().await;
        let registered: Arc<dyn RegisteredSession> = cleartext;
        unregister_connection(&registered);

        let (Some(read_half), Some(write_half)) = (self.read_half.take(), write_half) else {
            return false;
        };
        let stream = read_half.unsplit(write_half);
        let handshake = tokio::time::timeout(Duration::from_millis(SOCKET_TIMEOUT_MS), acceptor.accept(stream)).await;
        let stream = match handshake {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                log_socket_error(&error, &self.remote());
                return false;
            }
            Err(_) => {
                log::info("IMAP socket timeout", json!({ "component": "imap" }));
                return false;
            }
        };

        self.is_tls = true;
        let (session, read_half) = open_session(Box::new(stream), &self.peer, true);
        self.session = session;
        self.read_half = Some(read_half);
        self.paused = false;
        true
    }

    /// Parses and dispatches one complete command. `input` is the assembled
    /// command text — `{N}` markers, never payloads, which travel on
    /// `literals`.
    async fn execute_command(&mut self, input: &str, literals: Option<Vec<Literal>>) {
        let parse_result = parse_command(input, literals.as_deref());
        let tag = command_tag(input);
        match parse_result.value.filter(|_| parse_result.success) {
            Some(parsed) => match tag {
                // Refused before dispatch, not merely worded differently: a
                // line whose tag has no answer would land its side effect
                // and then be retried by a peer whose tag never completed.
                None => {
                    self.session.write(&format!("{UNTAGGED} BAD Command tag too long\r\n"));
                }
                Some(tag) => {
                    let session = self.session.clone();
                    let dispatched =
                        AssertUnwindSafe(self.dispatch.handle_request(&session, tag, parsed.request)).catch_unwind().await;
                    if dispatched.is_err() {
                        logger::error("Error processing command", json!({ "component": "imap" }));
                        session.write(&format!("{tag} BAD Internal server error\r\n"));
                    }
                }
            },
            None => {
                log::debug(
                    "Parse failed",
                    json!({
                        "component": "imap.parser",
                        "input": clip(&redact_credentials(input)),
                        "error": parse_result.error,
                    }),
                );
                let error = parse_result.error.filter(|error| !error.is_empty());
                let error = error.as_deref().unwrap_or("Invalid command syntax");
                self.session.write(&format!("{} BAD {}\r\n", tag.unwrap_or(UNTAGGED), clip(error)));
            }
        }
        if let Some(literals) = literals {
            remove_message_literals(&literals).await;
        }
    }

    /// Enforces the literal ceiling on a declaration. Anything but
    /// `Accepted` means the caller must NOT start accumulating, and `Closed`
    /// means the session is gone.
    ///
    /// `chain_budget` is what a chain has left before
    /// `MAX_PENDING_COMMAND_BYTES`: a granted continuation may not carry the
    /// pending command past it, so the cap is the smaller of the two.
    async fn refuse_oversized_literal(
        &mut self,
        command_text: &str,
        declared_bytes: u64,
        is_synchronizing: bool,
        is_message: bool,
        chain_budget: Option<u64>,
    ) -> LiteralOutcome {
        let cap = literal_cap_for(is_message, self.session.is_authenticated()).min(chain_budget.unwrap_or(u64::MAX));
        if declared_bytes <= cap {
            return LiteralOutcome::Accepted;
        }

        // A LITERAL+ payload is already in flight, so the only recovery is
        // to count its octets out of the stream, and past the ceiling any
        // real command fits under that recovery buys nothing: the peer must
        // ship every octet before one further command is served, each
        // segment resetting the inactivity timeout on the way. It ends the
        // session on the same terms as the chain cap.
        if !is_synchronizing && declared_bytes > MAX_PENDING_COMMAND_BYTES {
            log::info(
                "IMAP literal past the command ceiling; closing session",
                json!({
                    "component": "imap",
                    "cmd": command_verb(command_text),
                    "declaredBytes": declared_bytes,
                    "cap": MAX_PENDING_COMMAND_BYTES,
                    "remote": self.remote(),
                }),
            );
            self.session.write("* BYE Command too long\r\n");
            self.session.close();
            return LiteralOutcome::Closed;
        }

        log::info(
            "IMAP literal over cap; refusing",
            json!({
                "component": "imap",
                "cmd": command_verb(command_text),
                "declaredBytes": declared_bytes,
                "cap": cap,
                "synchronizing": is_synchronizing,
                "remote": self.remote(),
            }),
        );
        // `[TOOBIG]` is the response code for a literal the server will not
        // accept, so a client can tell this from a generic failure and stop
        // retrying the same oversized message.
        self.session.write(&format!(
            "{} NO [TOOBIG] Literal exceeds {cap} octets\r\n",
            command_tag(command_text).unwrap_or(UNTAGGED)
        ));

        self.discard_held_literals().await;
        self.pending_command = None;
        self.pending_literals = Vec::new();
        self.pending_literal_bytes = 0;
        self.awaiting_literal = false;
        self.literal_bytes_needed = 0;

        if !is_synchronizing {
            // LITERAL+: the payload is already on its way. It can only be
            // counted out of the stream and dropped, along with the rest of
            // the command line.
            self.discard_bytes_remaining = declared_bytes;
            self.discard_to_end_of_command = true;
        }
        // Synchronizing `{N}`: the continuation was withheld, so a conforming
        // client never sends the payload. Discarding here would swallow its
        // NEXT command instead.
        LiteralOutcome::Refused
    }

    /// The STARTTLS a command just run asked for, if any.
    fn upgrade_requested(&self) -> Option<Drained> {
        self.session.take_tls_upgrade().map(Drained::Upgrade)
    }

    fn spool_failed(&mut self, error: &std::io::Error) -> Drained {
        logger::error_with("Error processing data", json!({ "component": "imap" }), error);
        self.session.socket().destroy();
        Drained::Closed
    }

    /// Consumes what is buffered: literal payloads, command lines, the
    /// responses of an AUTHENTICATE exchange, DONE. Commands run one at a
    /// time, in arrival order, each to completion before the next is read:
    /// RFC 3501 permits serial execution, and a second command's response
    /// written into the middle of a literal still streaming for the first
    /// desyncs the client.
    async fn drain_commands(&mut self) -> Drained {
        loop {
            // Recovery from an over-cap LITERAL+ declaration: swallow the
            // announced octets, then the rest of that command line, without
            // holding any of it.
            if self.discard_bytes_remaining > 0 {
                let take = self.discard_bytes_remaining.min(self.buffer.len() as u64);
                self.buffer.consume(take as usize);
                self.discard_bytes_remaining -= take;
                if self.discard_bytes_remaining > 0 {
                    return Drained::NeedData;
                }
                continue;
            }
            if self.discard_to_end_of_command {
                // The tail can arrive in pieces. Drop what is here and wait
                // for the terminator rather than keeping it: nothing
                // accumulates, so this state needs no length cap.
                match self.buffer.index_of_crlf() {
                    None => {
                        self.buffer.clear();
                        return Drained::NeedData;
                    }
                    Some(end) => {
                        self.buffer.consume(end + 2);
                        self.discard_to_end_of_command = false;
                        continue;
                    }
                }
            }

            // Literal octets are payload, never commands. They are consumed
            // before the line splitter can reach them: `LOGIN {5+}\r\nadmin
            // {8+}\r\npassword` otherwise splits into three "commands", and
            // the password is echoed back as `password BAD Invalid command`.
            // `awaiting_literal` rather than a count: a `{0}` literal is
            // legal and its payload is the empty string.
            if let (Some(pending_command), true) = (self.pending_command.clone(), self.awaiting_literal) {
                // The declared count, not a re-measure of the decoded text:
                // an invalid UTF-8 sequence decodes to U+FFFD and re-encodes
                // to three octets that were never on the wire.
                let (payload, payload_bytes) = if self.spool.is_some() {
                    let wanted = usize::try_from(self.literal_bytes_needed).unwrap_or(usize::MAX);
                    let piece = self.buffer.head(wanted);
                    if !piece.is_empty() {
                        let written = match self.spool.as_mut() {
                            Some(spool) => spool.write(&piece).await,
                            None => Ok(()),
                        };
                        if let Err(error) = written {
                            return self.spool_failed(&error);
                        }
                        self.buffer.consume(piece.len());
                        self.literal_bytes_needed -= piece.len() as u64;
                    }
                    if self.literal_bytes_needed > 0 {
                        if self.buffer.is_empty() {
                            return Drained::NeedData;
                        }
                        continue;
                    }
                    let Some(spool) = self.spool.take() else {
                        continue;
                    };
                    match spool.finish().await {
                        Ok(literal) => {
                            let size = literal.size;
                            (Literal::Message(literal), size)
                        }
                        Err(error) => return self.spool_failed(&error),
                    }
                } else {
                    if (self.buffer.len() as u64) < self.literal_bytes_needed {
                        return Drained::NeedData;
                    }
                    let size = self.literal_bytes_needed as usize;
                    let text = self.buffer.decode(0, size);
                    self.buffer.consume(size);
                    (Literal::Text(text), size as u64)
                };
                self.pending_literals.push(payload);
                self.pending_literal_bytes += payload_bytes;
                self.literal_bytes_needed = 0;
                self.awaiting_literal = false;

                // A payload that consumed its own line terminator (the
                // client counted the CRLF into `{N}`, or declared the last
                // argument and sent nothing after it) leaves no tail to
                // read. Dispatch now rather than wait on a CRLF that is
                // never coming.
                //
                // An empty buffer alone is NOT that condition: it also means
                // "the rest has not arrived yet". Whether the command is
                // complete is a question about STRUCTURE, so the parser is
                // asked: `A1 LOGIN {7+}` + ["admin\r\n"] fails (keep
                // waiting), `a1 APPEND INBOX {13+}` + ["Hello World\r\n"]
                // succeeds.
                //
                // Parsing is necessary but not sufficient: a command with an
                // unbounded argument list parses while more of it is still
                // in flight.
                let pending = parse_command(&pending_command, Some(&self.pending_literals));
                let complete = pending.success
                    && pending.value.as_ref().is_some_and(|parsed| !chains_unbounded_arguments(&parsed.request));
                if self.buffer.is_empty() && complete {
                    let literals = std::mem::take(&mut self.pending_literals);
                    self.pending_command = None;
                    self.pending_literal_bytes = 0;
                    self.execute_command(&pending_command, Some(literals)).await;
                    if let Some(upgrade) = self.upgrade_requested() {
                        return upgrade;
                    }
                }
                continue;
            }

            let Some(line_end) = self.buffer.index_of_crlf() else {
                return Drained::NeedData;
            };
            let line = self.buffer.decode(0, line_end);
            self.buffer.consume(line_end + 2);

            // Text following a consumed payload on the same line: either it
            // declares the next literal (LOGIN chains two, one per
            // credential) or the command is complete. Handled ahead of the
            // SASL / IDLE / blank checks below because a completing tail is
            // usually the empty string.
            if let Some(pending_command) = self.pending_command.clone() {
                let complete = parse_command(&pending_command, Some(&self.pending_literals)).success;
                // Ending in `{N}` is not enough to call `line` a
                // continuation: a pipelined NEXT command declares a literal
                // the same way. What separates them is the SP between two
                // astrings (RFC 3501 §4.3): an argument tail carries one,
                // while a payload that consumed its own terminator leaves
                // the next command flush against the start. Completeness
                // alone cannot decide it either: `SEARCH SUBJECT {3+}`
                // parses on its own yet legally chains ` FROM {3+}`.
                let chained = if complete && !line.starts_with(js_is_whitespace) { None } else { literal_declaration(&line) };
                if let Some((declared_bytes, non_synchronizing)) = chained {
                    // Pace the chain, not just the header line it hangs off:
                    // an unpaced declaration costs a peer nothing while
                    // drawing a continuation, moving a payload through the
                    // buffer and re-entering the parser.
                    self.session.wait_for_command_slot().await;
                    let is_synchronizing = !non_synchronizing;
                    // The payloads live on `pending_literals`; the command
                    // text keeps only the markers. So the byte ceiling sums
                    // the payloads themselves.
                    let held_bytes = self.pending_literal_bytes + pending_command.len() as u64;
                    // A LITERAL+ declaration's octets are in flight already,
                    // so they are held whatever the session decides. A
                    // synchronizing one is held by nothing until the
                    // continuation is granted, and it is granted below only
                    // within what the chain has left.
                    let pending_bytes = held_bytes.saturating_add(if is_synchronizing { 0 } else { declared_bytes });
                    // A chain that has outgrown any real command is an
                    // accumulator attack, so it ends the session rather
                    // than answering `NO` and inviting the next one.
                    if self.pending_literals.len() >= MAX_LITERALS_PER_COMMAND || pending_bytes > MAX_PENDING_COMMAND_BYTES {
                        log::info(
                            "IMAP literal chain over cap; closing session",
                            json!({
                                "component": "imap",
                                "literals": self.pending_literals.len(),
                                "pendingBytes": pending_bytes,
                                "remote": self.remote(),
                            }),
                        );
                        self.session.write("* BYE Command too long\r\n");
                        self.session.close();
                        return Drained::Closed;
                    }
                    let extended = format!("{pending_command}{}", line.trim_end_matches(js_is_whitespace));
                    let is_message = declares_append_message(&extended, &self.pending_literals);
                    // Cap-check before changing any state: the verb and the
                    // tag both come off the command assembled so far, not
                    // off this tail line.
                    let outcome = self
                        .refuse_oversized_literal(
                            &pending_command,
                            declared_bytes,
                            is_synchronizing,
                            is_message,
                            Some(MAX_PENDING_COMMAND_BYTES - held_bytes),
                        )
                        .await;
                    match outcome {
                        LiteralOutcome::Closed => return Drained::Closed,
                        LiteralOutcome::Refused => continue,
                        LiteralOutcome::Accepted => {}
                    }
                    self.pending_command = Some(extended);
                    self.literal_bytes_needed = declared_bytes;
                    self.awaiting_literal = true;
                    if is_message {
                        self.spool = Some(LiteralSpool::new());
                    }
                    if is_synchronizing {
                        self.session.write("+ go ahead\r\n");
                    }
                    continue;
                }
                // If the command already parses without this line, the
                // payload carried its own terminator and `line` is not its
                // tail: it is the NEXT command, pipelined behind it.
                // Dispatch what is complete, then read `line` as the fresh
                // command it is.
                let input = if complete { pending_command } else { format!("{pending_command}{line}") };
                let literals = std::mem::take(&mut self.pending_literals);
                self.pending_command = None;
                self.pending_literal_bytes = 0;
                self.execute_command(&input, Some(literals)).await;
                // The command just run may have been STARTTLS (the parser
                // accepts `a1 STARTTLS {N+}`): `line` rode in the cleartext
                // segment and must not be answered inside TLS.
                if let Some(upgrade) = self.upgrade_requested() {
                    return upgrade;
                }
                if !complete {
                    continue;
                }
            }

            // The response to an AUTHENTICATE challenge.
            if let Some(tag) = self.session.take_pending_sasl_tag() {
                let response = js_trim(&line);
                // The client may send "*" to cancel.
                if response == "*" {
                    self.session.write(&format!("{tag} BAD Authentication cancelled\r\n"));
                } else {
                    let session = self.session.clone();
                    self.dispatch.sasl_response(&session, &tag, response).await;
                }
                continue;
            }

            let command_line = js_trim(&line);
            if command_line.is_empty() {
                continue;
            }

            // The only valid client input during IDLE is DONE. This line
            // buffer already reassembles split and pipelined input, so DONE
            // is handled here, and a command pipelined after it is read on
            // the next pass. Anything else during IDLE is ignored (RFC 2177).
            if self.session.is_in_idle_mode() {
                if command_line.to_uppercase() == "DONE" {
                    self.session.end_idle(None);
                }
                continue;
            }

            log::debug(
                "IMAP command received",
                json!({
                    "component": "imap",
                    "command": clip(&redact_credentials(command_line)),
                    "mailbox": self.session.selected_mailbox(),
                }),
            );
            imap_trace(Direction::In, self.session.get_session_id(), command_line);

            // Pace pipelined bursts. RFC 3501 §7 requires a tagged
            // completion for every command, so commands over the limit are
            // delayed, never skipped.
            self.session.wait_for_command_slot().await;

            // A command line ending in `{N}` / `{N+}` declares a literal:
            // the next N octets are payload, not a command. RFC 3501 permits
            // a literal wherever an astring is legal, and LITERAL+ lets a
            // client send it without waiting for the continuation.
            if let Some((declared_bytes, non_synchronizing)) = literal_declaration(&line) {
                let header = line.trim_end_matches(js_is_whitespace);
                let is_message = declares_append_message(header, &[]);
                let outcome =
                    self.refuse_oversized_literal(&line, declared_bytes, !non_synchronizing, is_message, None).await;
                match outcome {
                    LiteralOutcome::Closed => return Drained::Closed,
                    LiteralOutcome::Refused => continue,
                    LiteralOutcome::Accepted => {}
                }
                self.pending_command = Some(header.to_string());
                self.pending_literals = Vec::new();
                self.pending_literal_bytes = 0;
                self.literal_bytes_needed = declared_bytes;
                self.awaiting_literal = true;
                if is_message {
                    self.spool = Some(LiteralSpool::new());
                }
                // A synchronizing literal waits for a continuation request.
                if !non_synchronizing {
                    self.session.write("+ go ahead\r\n");
                }
                continue;
            }

            self.execute_command(&line, None).await;
            if let Some(upgrade) = self.upgrade_requested() {
                return upgrade;
            }
        }
    }
}

/// The write side was ended: nothing more is read as input, and the socket
/// stays open only for the peer to close its side, or for the flush timer.
async fn linger(socket: &SocketHandle, read_half: &mut ReadHalf<BoxedStream>) {
    let mut discarded = [0u8; 4096];
    loop {
        tokio::select! {
            biased;
            _ = socket.destroyed() => return,
            read = read_half.read(&mut discarded) => match read {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            },
        }
    }
}

/// Serves one accepted connection to its end.
pub async fn serve_connection(stream: BoxedStream, peer: Peer, is_tls: bool) {
    ImapRequestHandler::new(stream, peer, is_tls).run().await;
}

/// Resident set size of the process in bytes, where the platform tells.
fn resident_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| statm.split(' ').nth(1).and_then(|pages| pages.parse::<u64>().ok()))
        .map_or(0, |pages| pages * 4096)
}

/// Gives the command's budget slot back when the command is over, however it
/// ends. Reads the hold's own flag: a slot that was yielded and not yet
/// reacquired is not this command's to hand back twice.
struct ReleaseOnDrop(Arc<CommandBudgetHold>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if self.0.held() {
            release_command_budget();
        }
    }
}

/// Runs one parsed command on the session: the command budget, the
/// credential-revocation fence, the command itself, and its diagnostic line.
pub async fn handle_request(session: &Arc<ImapSession>, tag: &str, request: ImapRequest) {
    // Acquired BEFORE the diagnostic timer starts, so a command queued
    // behind the budget does not attribute the other in-flight commands'
    // memory growth, or its own queueing, to itself; the hold carries the
    // wait separately.
    let budgeted = is_budgeted(&request);
    let waited = if budgeted { acquire_command_budget().await } else { Duration::ZERO };
    let hold = create_command_budget_hold(budgeted, waited);
    let _release = ReleaseOnDrop(hold.clone());

    // Re-checked after the budget wait: a saturated budget is the longest
    // window a peer has to disconnect in, and building a response for a
    // socket that refuses every write spends a slot other sessions can use.
    // Logged rather than dropped silently: these are the longest waits, and
    // silence would make the budget look better sized the more it is
    // saturated.
    let socket = session.socket();
    if budgeted && (socket.is_destroyed() || !socket.is_writable()) {
        log::info(
            "IMAP command dropped",
            json!({
                "component": "imap",
                "tag": tag,
                "cmd": describe_imap_command(&request),
                "remote": socket.remote(),
                "waitedForCommandBudgetMs": hold.waited_ms().round(),
            }),
        );
        return;
    }

    let started_at = Instant::now();
    let rss_before = resident_bytes();
    let bytes_before = session.bytes_written();

    // The body-budget ledger and the command-budget hold are bound to this
    // command, so a wait made deeper in the FETCH path is attributed to it,
    // and can give this command's slot up for its duration.
    run_in_command_budget_context(
        hold.clone(),
        run_in_body_budget_context(async {
            let outcome = AssertUnwindSafe(dispatch(session, tag, &request)).catch_unwind().await;
            let failure = match outcome {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(_) => Some("the command panicked".to_string()),
            };
            if let Some(failure) = failure {
                logger::error_with(
                    "Error handling IMAP request",
                    json!({ "component": "imap", "tag": tag, "type": request.type_name() }),
                    &failure,
                );
                session.write(&format!("{tag} BAD Internal server error\r\n"));
            }

            log_completion(session, tag, &request, &hold, started_at, rss_before, bytes_before);
        }),
    )
    .await;
}

/// Whether a command's completion line is logged at INFO rather than DEBUG:
/// ANY axis past its threshold is enough. A budgeted command that queued
/// long behind the command budget counts even if it then ran fast, since
/// that wait is the only sign the budget is undersized.
fn is_interesting_command(
    rss_delta_mb: i64,
    duration_ms: u64,
    response_bytes: u64,
    waited_for_command_budget_ms: u64,
) -> bool {
    rss_delta_mb.abs() >= INTERESTING_RSS_DELTA_MB
        || duration_ms >= INTERESTING_DURATION_MS
        || response_bytes >= INTERESTING_RESPONSE_BYTES
        || waited_for_command_budget_ms >= INTERESTING_DURATION_MS
}

/// The per-command diagnostic: memory delta, octets sent and wall time, so a
/// memory spike can be attributed to a specific command.
fn log_completion(
    session: &ImapSession,
    tag: &str,
    request: &ImapRequest,
    hold: &CommandBudgetHold,
    started_at: Instant,
    rss_before: u64,
    bytes_before: u64,
) {
    const MB: f64 = 1_048_576.0;
    let rss_after = resident_bytes();
    let rss_delta_mb = ((rss_after as f64 - rss_before as f64) / MB).round() as i64;
    let response_bytes = session.bytes_written().saturating_sub(bytes_before);
    let duration_ms = started_at.elapsed().as_secs_f64() * 1000.0;
    let duration_ms = duration_ms.round() as u64;
    let waited_for_body_budget_ms = get_body_budget_wait_ms().round() as u64;
    let waited_for_command_budget_ms = hold.waited_ms().round() as u64;
    let is_interesting =
        is_interesting_command(rss_delta_mb, duration_ms, response_bytes, waited_for_command_budget_ms);

    // Which BODY[] shape a FETCH used: a partial `BODY[]<start.length>`, or
    // the whole body (`BODY[]`, or RFC822, which has no partial form).
    let mut body_full_partial = 0;
    let mut body_full_stream = 0;
    let mut inner = request;
    while let ImapRequest::Uid(uid) = inner {
        inner = uid.request();
    }
    if let ImapRequest::Fetch(fetch) = inner {
        for item in &fetch.data_items {
            match item {
                FetchDataItem::Body(body) if matches!(body.section, BodySection::Full) => {
                    if body.partial.is_some() {
                        body_full_partial += 1;
                    } else {
                        body_full_stream += 1;
                    }
                }
                FetchDataItem::Rfc822 => body_full_stream += 1,
                _ => {}
            }
        }
    }

    let payload = json!({
        "component": "imap",
        "tag": tag,
        "cmd": describe_imap_command(request),
        "mailbox": session.selected_mailbox(),
        "remote": session.socket().remote(),
        "rssBeforeMB": (rss_before as f64 / MB).round() as u64,
        "rssAfterMB": (rss_after as f64 / MB).round() as u64,
        "rssDeltaMB": rss_delta_mb,
        "bodyFullPartial": body_full_partial,
        "bodyFullStream": body_full_stream,
        "responseBytes": response_bytes,
        "durationMs": duration_ms,
        "waitedForBodyBudgetMs": waited_for_body_budget_ms,
        "waitedForCommandBudgetMs": waited_for_command_budget_ms,
    });
    if is_interesting {
        log::info("IMAP command completed", payload);
    } else {
        log::debug("IMAP command completed", payload);
    }
}

/// Runs the command on the session.
async fn dispatch(session: &Arc<ImapSession>, tag: &str, request: &ImapRequest) -> anyhow::Result<()> {
    // Credential-revocation fence: every grant proves itself current on
    // every command, not only at sign-in. LOGIN and AUTHENTICATE mint the
    // grant and LOGOUT ends it; every other command on an authenticated
    // session compares the stamped generation against the credential row's
    // current one. This is what reaches the connections a sweep cannot.
    if session.is_authenticated()
        && !matches!(request, ImapRequest::Login(_) | ImapRequest::Authenticate(_) | ImapRequest::Logout)
        && !session.verify_credential_version(tag).await?
    {
        return Ok(());
    }

    match request {
        ImapRequest::Capability => session.capability(tag),
        ImapRequest::Noop => session.noop(tag),
        ImapRequest::Login(data) => session.login(tag, &data.username, &data.password).await?,
        ImapRequest::Authenticate(data) => {
            session.authenticate(tag, &data.mechanism, data.initial_response.as_deref()).await;
        }
        ImapRequest::List(data) => session.list_mailboxes(tag, &data.reference, &data.pattern).await,
        ImapRequest::Lsub(data) => session.list_subscribed_mailboxes(tag, &data.reference, &data.pattern).await,
        ImapRequest::Select(data) => session.select_mailbox(tag, &data.mailbox, false).await,
        ImapRequest::Examine(data) => session.examine_mailbox(tag, &data.mailbox).await,
        ImapRequest::Create { mailbox } => session.create_mailbox(tag, mailbox).await,
        ImapRequest::Delete { mailbox } => session.delete_mailbox(tag, mailbox).await,
        ImapRequest::Rename { old_name, new_name } => session.rename_mailbox(tag, old_name, new_name).await,
        ImapRequest::Subscribe { mailbox } => session.subscribe_mailbox(tag, mailbox).await,
        ImapRequest::Unsubscribe { mailbox } => session.unsubscribe_mailbox(tag, mailbox).await,
        ImapRequest::Status(data) => session.status_mailbox(tag, &data.mailbox, &data.items).await,
        ImapRequest::Append(data) => session.append_message(tag, data).await,
        ImapRequest::Idle => session.start_idle(tag),
        ImapRequest::Check => session.check(tag),
        ImapRequest::Fetch(data) => session.fetch_messages_typed(tag, data, false).await,
        ImapRequest::Search(data) => session.search_typed(tag, data, false).await,
        ImapRequest::Store(data) => session.store_flags_typed(tag, data, false).await,
        ImapRequest::Copy(data) => session.copy_message_typed(tag, data, false).await,
        ImapRequest::Move(data) => session.move_message_typed(tag, data, false).await,
        ImapRequest::Uid(data) => handle_uid_command(session, tag, data).await,
        ImapRequest::Close => session.close_mailbox(tag, false),
        ImapRequest::Expunge => session.expunge(tag).await,
        ImapRequest::Logout => session.logout(tag),
        ImapRequest::Id => {
            session.write(&format!("* ID NIL\r\n{tag} OK ID completed\r\n"));
        }
        ImapRequest::Starttls => session.start_tls(tag),
        // RFC 2342: a single personal namespace, no other or shared ones.
        ImapRequest::Namespace => {
            session.write(&format!("* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n{tag} OK NAMESPACE completed\r\n"));
        }
        // RFC 5161 / RFC 4551 §3.7: CONDSTORE is the one extension that can
        // be enabled; what was activated is echoed back.
        ImapRequest::Enable { capabilities } => session.enable(tag, capabilities),
        // RFC 3691: like CLOSE but without expunging.
        ImapRequest::Unselect => session.close_mailbox(tag, true),
        // RFC 2087: quota is not supported.
        ImapRequest::Getquotaroot { .. } => {
            session.write(&format!("{tag} NO Quota not supported\r\n"));
        }
        // DONE outside IDLE is not a command.
        ImapRequest::Done => {
            session.write(&format!("{tag} BAD Unknown command\r\n"));
        }
    }
    Ok(())
}

/// A UID command: the wrapped command, addressed by UID.
async fn handle_uid_command(session: &Arc<ImapSession>, tag: &str, data: &UidRequest) {
    match data.request() {
        ImapRequest::Fetch(request) => session.fetch_messages_typed(tag, request, true).await,
        ImapRequest::Search(request) => session.search_typed(tag, request, true).await,
        ImapRequest::Store(request) => session.store_flags_typed(tag, request, true).await,
        ImapRequest::Copy(request) => session.copy_message_typed(tag, request, true).await,
        ImapRequest::Move(request) => session.move_message_typed(tag, request, true).await,
        _ => {
            session.write(&format!("{tag} BAD UID {} not supported\r\n", data.command));
        }
    }
}
