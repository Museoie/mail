//! One IMAP connection's protocol state and the commands that live on it
//! (imap/session.ts).
//!
//! A session is shared: its connection runs commands on it, the IDLE manager
//! writes notifications to it, the registries close it. Everything it holds
//! is behind a short lock; `serial` is what keeps a notification out of the
//! middle of a command's response.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use futures_util::Stream;
use serde_json::json;

use super::auth::{
    handle_authenticate, handle_login, AuthBackend, AuthConnection, AuthResult, ServerAuthBackend, SessionScope,
};
use crate::postgres::repositories::domains::{get_domain_access, DomainAccess};
use super::capabilities::get_capabilities;
use super::chunked_write::{write_chunked_to_socket, write_stream_to_socket};
use super::close_socket::close_socket;
use super::credential_version::get_credential_password_version;
use super::idle_manager::{idle_manager, IdleFuture, IdleTarget, IDLE_SOCKET_TIMEOUT_MS, SOCKET_TIMEOUT_MS};
use super::message_ops::MailFlagUpdate;
use super::sequence_resolver::{reconcile_sequence_mapping, set_sequence_mapping, MailboxUids, SequenceState};
use super::session_registry::{
    register_authenticated_session, unregister_connection, unregister_session, RegisteredSession,
};
use super::socket::SocketHandle;
use super::store::Store;
use super::throttler::Throttler;
use super::trace::{imap_trace, Direction};
use super::types::{AppendRequest, CopyRequest, FetchRequest, MoveRequest, SearchRequest, StatusItem, StoreRequest};
use super::{mailbox_ops, message_ops};
#[cfg(test)]
use crate::models::SignedUser;
use crate::tls::{get_tls_credentials, TlsAcceptor, TlsCredentials};
use crate::{logger, sso};

/// What a session holds besides its socket.
#[derive(Default)]
pub struct SessionState {
    pub selected_mailbox: Option<String>,
    pub selected_mailbox_message_count: usize,
    pub mailbox_read_only: bool,
    /// The mailboxes and messages of the account the session reads, set by
    /// a successful sign-in (for a login scoped to a viewed domain, its
    /// owner's).
    pub store: Option<Store>,
    pub authenticated: bool,
    /// True when the login is scoped to a domain the person only views (the
    /// viewer role): the store is the owner's, and nothing in it is theirs to
    /// change. Every state-changing command then refuses with `NO
    /// [READ-ONLY]`; the flag is scoped to this one connection.
    pub is_read_only_user: bool,
    pub authenticated_as: Option<String>,
    /// The domain a `user@domain` login is scoped to and the role it rests
    /// on, re-checked on every command so a revoked or moved role ends the
    /// session.
    pub scope: Option<SessionScope>,
    /// The credential generation the connection was minted from, stamped at
    /// sign-in. The per-command gate re-checks it against the row's current
    /// version, so a connection that outlives a password rotation loses its
    /// grant at its next command.
    pub credential_password_version: Option<i64>,
    /// RFC 4551 CONDSTORE: once enabled, MODSEQ is emitted on every later
    /// FETCH response for the life of the session.
    pub condstore_enabled: bool,
    pub is_idling: bool,
    pub idle_tag: Option<String>,
    /// Sequence number mapping of the selected mailbox.
    pub seq_state: SequenceState,
    /// Set while the client owes the SASL response of an AUTHENTICATE.
    pub pending_sasl_tag: Option<String>,
    /// Set by a STARTTLS that was answered OK: the connection upgrades
    /// before it reads anything else.
    pub tls_upgrade: Option<TlsAcceptor>,
}

pub struct ImapSession {
    session_id: String,
    socket: SocketHandle,
    /// Whether the connection is inside TLS (the implicit-TLS port, or after
    /// STARTTLS, which starts a new session).
    is_tls: bool,
    this: Weak<ImapSession>,
    state: Mutex<SessionState>,
    serial: tokio::sync::Mutex<()>,
    // Mail clients pipeline aggressively during folder sync: iOS Mail sends
    // STATUS for every mailbox in one burst. 100 commands a second stays far
    // above any interactive rate while still bounding a runaway client.
    throttler: Mutex<Throttler>,
    bytes_written: AtomicU64,
    /// Command slots taken from the throttler, for tests of the pacing.
    #[cfg(test)]
    slots_taken: AtomicU64,
    /// Stand-ins for the store reads IDLE notifications make, for tests.
    #[cfg(test)]
    test_reads: Mutex<TestReads>,
}

/// What a test answers the mailbox reads of IDLE notifications with.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestReads {
    pub uids: Option<std::sync::Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, MailboxUids> + Send + Sync>>,
    #[allow(clippy::type_complexity)]
    pub flags: Option<
        std::sync::Arc<
            dyn Fn(Vec<String>) -> futures_util::future::BoxFuture<'static, Result<Option<Vec<MailFlagUpdate>>>>
                + Send
                + Sync,
        >,
    >,
}

impl ImapSession {
    pub fn new(socket: SocketHandle, is_tls: bool) -> Arc<ImapSession> {
        let random: [u8; 8] = rand::random();
        Arc::new_cyclic(|this| ImapSession {
            session_id: format!("session_{}", hex::encode(random)),
            socket,
            is_tls,
            this: this.clone(),
            state: Mutex::new(SessionState::default()),
            serial: tokio::sync::Mutex::new(()),
            throttler: Mutex::new(Throttler::new(100, 1000)),
            bytes_written: AtomicU64::new(0),
            #[cfg(test)]
            slots_taken: AtomicU64::new(0),
            #[cfg(test)]
            test_reads: Mutex::new(TestReads::default()),
        })
    }

    fn this(&self) -> Option<Arc<ImapSession>> {
        self.this.upgrade()
    }

    /// The session's state, for a short read or change. Never held across
    /// an `await`.
    pub fn state(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn socket(&self) -> &SocketHandle {
        &self.socket
    }

    pub fn is_tls(&self) -> bool {
        self.is_tls
    }

    pub fn get_capabilities(&self) -> String {
        get_capabilities(self.is_tls, sso::get_sso().is_some())
    }

    /// Waits for everything already queued on this session, then holds the
    /// session until the guard is dropped.
    ///
    /// The connection's command drain and the IDLE notifier both write
    /// untagged responses, and the notifier reaches the socket from a
    /// delivery, not from the drain. Without a shared turn a notification
    /// parked on its mailbox read resumes in the middle of another command's
    /// response: RFC 3501 §7.4.1 forbids an EXPUNGE while answering FETCH,
    /// STORE or SEARCH, and renumbering mid-FETCH shifts the very sequence
    /// numbers that response is reporting.
    pub async fn serial(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.serial.lock().await
    }

    /// Re-advertises the selected mailbox to an IDLE client, announcing
    /// whatever left it first. Returns the new message count, or `None`
    /// when the session is no longer idling, there is no mailbox to report
    /// on, or it could not be read; nothing reaches the wire then and the
    /// client keeps the mapping it already has.
    pub async fn notify_mailbox_update(&self) -> Result<Option<usize>> {
        let _turn = self.serial().await;
        if !self.is_in_idle_mode() {
            return Ok(None);
        }

        // RFC 3501 §7.4.1 forbids an EXPUNGE with no command in progress, so
        // the announcement is held back until the session is known to still
        // be idling at the moment it would reach the wire. IDLE can end
        // while this waits its turn (DONE) or while its mailbox read is
        // parked (the heartbeat's force-terminate, which takes no turn).
        let advertised = self.state().seq_state.seq_to_uid.clone();
        let read = self.read_selected_mailbox_uids().await;
        let mut pending: Vec<String> = Vec::new();
        let total = {
            let mut state = self.state();
            reconcile_sequence_mapping(&mut state.seq_state, read, &mut |data| pending.push(data.to_string()))
        };
        let Some(total) = total else {
            return Ok(None);
        };
        if !self.is_in_idle_mode() {
            set_sequence_mapping(&mut self.state().seq_state, advertised);
            return Ok(None);
        }

        for response in &pending {
            self.write(response);
        }
        self.write(&format!("* {total} EXISTS\r\n"));
        self.write("* 0 RECENT\r\n");
        Ok(Some(total))
    }

    /// Pushes untagged FETCH (FLAGS) updates for messages whose flags
    /// changed out of band (an HTTP mark). Only IDLE sessions are told, in
    /// the session's turn so it never interleaves with a command response.
    /// Messages not in the session's view are skipped.
    pub async fn notify_flag_change(&self, mail_ids: &[String]) -> Result<()> {
        let _turn = self.serial().await;
        let selected = {
            let state = self.state();
            if !state.is_idling || state.store.is_none() {
                return Ok(());
            }
            state.selected_mailbox.clone()
        };
        let Some(selected_mailbox) = selected else {
            return Ok(());
        };
        let Some(updates) = self.read_mail_flag_updates(&selected_mailbox, mail_ids).await? else {
            return Ok(());
        };
        for update in updates {
            let seq = self.state().seq_state.uid_to_seq.get(&update.uid).copied();
            let Some(seq) = seq else {
                continue;
            };
            self.write(&format!("* {seq} FETCH (FLAGS ({}))\r\n", update.flags().join(" ")));
        }
        Ok(())
    }

    /// The UIDs of the selected mailbox, for the sequence mapping.
    async fn read_selected_mailbox_uids(&self) -> MailboxUids {
        #[cfg(test)]
        {
            let read = self.test_reads.lock().unwrap().uids.clone();
            if let Some(read) = read {
                return read().await;
            }
        }
        message_ops::read_selected_mailbox_uids(self).await
    }

    /// The current flags of `mail_ids` as `mailbox` sees them.
    async fn read_mail_flag_updates(&self, mailbox: &str, mail_ids: &[String]) -> Result<Option<Vec<MailFlagUpdate>>> {
        #[cfg(test)]
        {
            let read = self.test_reads.lock().unwrap().flags.clone();
            if let Some(read) = read {
                return read(mail_ids.to_vec()).await;
            }
        }
        message_ops::read_mail_flag_updates(self, mailbox, mail_ids).await
    }

    #[cfg(test)]
    pub(crate) fn set_test_reads(&self, reads: TestReads) {
        *self.test_reads.lock().unwrap() = reads;
    }

    /// Octets queued to the client so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::SeqCst)
    }

    /// Queues `data` for the client. False, with nothing queued, when the
    /// socket no longer takes writes.
    pub fn write(&self, data: &str) -> bool {
        if self.socket.is_destroyed() || !self.socket.is_writable() {
            logger::warn("Attempted to write to destroyed/unwritable socket", json!({ "component": "imap" }));
            return false;
        }
        let ok = self.socket.write(Bytes::copy_from_slice(data.as_bytes()));
        self.bytes_written.fetch_add(data.len() as u64, Ordering::SeqCst);
        imap_trace(Direction::Out, &self.session_id, data);
        ok
    }

    /// Tears the connection down through the shared IMAP teardown primitive.
    pub fn close(&self) {
        close_socket(&self.socket);
    }

    /// Writes a large payload in chunks, each waiting for the socket to
    /// drain the last, so the outbound queue never holds a whole multi-MB
    /// body. Resolves once every chunk is on its way.
    pub async fn write_chunked(&self, payload: Bytes) {
        if self.socket.is_destroyed() || !self.socket.is_writable() {
            logger::warn(
                "Attempted to writeChunked to destroyed/unwritable socket",
                json!({ "component": "imap" }),
            );
            return;
        }
        let written = write_chunked_to_socket(&self.socket, payload).await;
        self.bytes_written.fetch_add(written as u64, Ordering::SeqCst);
    }

    /// Streams chunks straight to the socket with the same pacing: the body
    /// is never held whole, each chunk is produced, written and released
    /// before the next.
    pub async fn write_stream(&self, chunks: impl Stream<Item = Bytes>) {
        if self.socket.is_destroyed() || !self.socket.is_writable() {
            logger::warn(
                "Attempted to writeStream to destroyed/unwritable socket",
                json!({ "component": "imap" }),
            );
            return;
        }
        let written = write_stream_to_socket(&self.socket, chunks).await;
        self.bytes_written.fetch_add(written as u64, Ordering::SeqCst);
    }

    /// Backpressure for pipelined bursts: resolves once the connection is
    /// within its command rate, counting the command against the window.
    /// RFC 3501 §7 requires a tagged completion for every command, so a
    /// command over the limit is delayed, never dropped. Returns early when
    /// the socket dies mid-wait: there is nobody left to answer.
    pub async fn wait_for_command_slot(&self) {
        let mut wait = self.throttle().ms_until_free();
        while wait > 0 {
            if self.socket.is_destroyed() || !self.socket.is_writable() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(wait)).await;
            wait = self.throttle().ms_until_free();
        }
        self.throttle().record();
        #[cfg(test)]
        self.slots_taken.fetch_add(1, Ordering::SeqCst);
    }

    /// How many command slots the session has taken.
    #[cfg(test)]
    pub(crate) fn slots_taken(&self) -> u64 {
        self.slots_taken.load(Ordering::SeqCst)
    }

    fn throttle(&self) -> MutexGuard<'_, Throttler> {
        self.throttler.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -----------------------------------------------------------------------
    // Simple commands
    // -----------------------------------------------------------------------

    pub fn capability(&self, tag: &str) {
        self.write(&format!("* CAPABILITY {}\r\n{tag} OK CAPABILITY completed\r\n", self.get_capabilities()));
    }

    pub fn noop(&self, tag: &str) {
        self.write(&format!("{tag} OK NOOP completed\r\n"));
    }

    pub fn check(&self, tag: &str) {
        self.write(&format!("{tag} OK CHECK completed\r\n"));
    }

    /// RFC 5161 ENABLE with RFC 4551 §3.7: acknowledges the extensions that
    /// can be turned on and echoes them in one `* ENABLED` line (empty when
    /// none match). CONDSTORE is the only one.
    pub fn enable(&self, tag: &str, capabilities: &[String]) {
        let mut enabled: Vec<&str> = Vec::new();
        {
            let mut state = self.state();
            for capability in capabilities {
                if capability.to_uppercase() == "CONDSTORE" && !state.condstore_enabled {
                    state.condstore_enabled = true;
                    enabled.push("CONDSTORE");
                }
            }
        }
        let suffix = if enabled.is_empty() { String::new() } else { format!(" {}", enabled.join(" ")) };
        self.write(&format!("* ENABLED{suffix}\r\n{tag} OK ENABLE completed\r\n"));
    }

    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------

    /// With single sign-on on, no password is taken before TLS: an app
    /// password is a full credential for the protocol. Writes the refusal
    /// and returns true when the caller must stop.
    fn refuses_cleartext_auth(&self, tag: &str) -> bool {
        if self.is_tls || sso::get_sso().is_none() {
            return false;
        }
        self.write(&format!("{tag} NO [PRIVACYREQUIRED] Use STARTTLS or the TLS port to sign in\r\n"));
        true
    }

    fn signed_in(&self, result: AuthResult) {
        {
            let mut state = self.state();
            state.store = Some(Store::new(result.user, result.store_scope));
            state.authenticated = true;
            state.is_read_only_user = result.is_read_only;
            state.authenticated_as = Some(result.authenticated_as);
            state.scope = result.scope;
            state.credential_password_version = Some(result.password_version);
        }
        if let Some(this) = self.this() {
            register_authenticated_session(this);
        }
    }

    pub async fn authenticate(&self, tag: &str, mechanism: &str, initial_response: Option<&str>) {
        self.authenticate_with(tag, mechanism, initial_response, &ServerAuthBackend).await;
    }

    /// AUTHENTICATE, with credentials checked by `backend`.
    pub async fn authenticate_with(
        &self,
        tag: &str,
        mechanism: &str,
        initial_response: Option<&str>,
        backend: &impl AuthBackend,
    ) {
        if self.refuses_cleartext_auth(tag) {
            return;
        }
        if let Some(result) = handle_authenticate(tag, mechanism, initial_response, self, backend).await {
            self.signed_in(result);
        }
    }

    pub async fn login(&self, tag: &str, username: &str, password: &str) -> Result<()> {
        self.login_with(tag, username, password, &ServerAuthBackend).await
    }

    /// LOGIN, with credentials checked by `backend`.
    pub async fn login_with(&self, tag: &str, username: &str, password: &str, backend: &impl AuthBackend) -> Result<()> {
        if self.refuses_cleartext_auth(tag) {
            return Ok(());
        }
        let args = [username.to_string(), password.to_string()];
        if let Some(result) = handle_login(tag, &args, self, backend).await? {
            self.signed_in(result);
        }
        Ok(())
    }

    /// Guard for state-changing commands. True when the caller may go on;
    /// false after writing `NO [READ-ONLY]` for a login scoped to a domain
    /// the person only views. The refusal names the person, never the
    /// domain's owner whose store the session reads.
    fn allow_mutation(&self, tag: &str, command: &str) -> bool {
        let (attributed_to, domain) = {
            let state = self.state();
            if !state.is_read_only_user {
                return true;
            }
            (
                state.authenticated_as.clone().filter(|name| !name.is_empty()).unwrap_or_else(|| "read-only".to_string()),
                state.scope.as_ref().map(|scope| scope.domain.clone()).unwrap_or_else(|| "this domain".to_string()),
            )
        };
        self.write(&format!("{tag} NO [READ-ONLY] {command} not permitted: {attributed_to} can only read {domain}.\r\n"));
        false
    }

    /// Guard for the commands that manage user-created mailboxes. Those hold
    /// the whole store's mail, so a login scoped to one domain neither lists
    /// nor manages them.
    fn allow_mailbox_management(&self, tag: &str, command: &str) -> bool {
        let scoped = self.state().store.as_ref().is_some_and(Store::is_domain_scoped);
        if !scoped {
            return true;
        }
        self.write(&format!("{tag} NO [CANNOT] {command} is not available to a login scoped to one domain.\r\n"));
        false
    }

    /// The signed-in account's store, or `None` after answering `NO Not
    /// authenticated.`
    fn require_store(&self, tag: &str) -> Option<Store> {
        let store = {
            let state = self.state();
            if state.authenticated {
                state.store.clone()
            } else {
                None
            }
        };
        if store.is_none() {
            self.write(&format!("{tag} NO Not authenticated.\r\n"));
        }
        store
    }

    /// The signed-in account and the selected mailbox, or `None` after
    /// answering which of the two is missing.
    fn require_selected(&self, tag: &str) -> Option<(Store, String)> {
        let store = self.require_store(tag)?;
        let selected = self.state().selected_mailbox.clone();
        match selected {
            Some(mailbox) => Some((store, mailbox)),
            None => {
                self.write(&format!("{tag} BAD No mailbox selected\r\n"));
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Mailbox operations
    // -----------------------------------------------------------------------

    pub async fn create_mailbox(&self, tag: &str, mailbox: &str) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        if !self.allow_mutation(tag, "CREATE") {
            return;
        }
        if !self.allow_mailbox_management(tag, "CREATE") {
            return;
        }
        mailbox_ops::create_mailbox(tag, mailbox, &store, self).await;
    }

    pub async fn delete_mailbox(&self, tag: &str, mailbox: &str) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        if !self.allow_mutation(tag, "DELETE") {
            return;
        }
        if !self.allow_mailbox_management(tag, "DELETE") {
            return;
        }
        mailbox_ops::delete_mailbox(tag, mailbox, &store, self).await;
    }

    pub async fn rename_mailbox(&self, tag: &str, old_name: &str, new_name: &str) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        if !self.allow_mutation(tag, "RENAME") {
            return;
        }
        if !self.allow_mailbox_management(tag, "RENAME") {
            return;
        }
        mailbox_ops::rename_mailbox(tag, old_name, new_name, &store, self).await;
    }

    pub async fn subscribe_mailbox(&self, tag: &str, mailbox: &str) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        if !self.allow_mutation(tag, "SUBSCRIBE") {
            return;
        }
        mailbox_ops::subscribe_mailbox(tag, mailbox, &store, self).await;
    }

    pub async fn unsubscribe_mailbox(&self, tag: &str, mailbox: &str) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        if !self.allow_mutation(tag, "UNSUBSCRIBE") {
            return;
        }
        mailbox_ops::unsubscribe_mailbox(tag, mailbox, &store, self).await;
    }

    pub async fn status_mailbox(&self, tag: &str, mailbox: &str, items: &[StatusItem]) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        mailbox_ops::status_mailbox(tag, mailbox, items, &store, self).await;
    }

    pub async fn list_mailboxes(&self, tag: &str, reference: &str, pattern: &str) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        mailbox_ops::list_mailboxes(tag, reference, pattern, &store, self).await;
    }

    pub async fn list_subscribed_mailboxes(&self, tag: &str, reference: &str, pattern: &str) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        mailbox_ops::list_subscribed_mailboxes(tag, reference, pattern, &store, self).await;
    }

    pub async fn examine_mailbox(&self, tag: &str, name: &str) {
        self.select_mailbox(tag, name, true).await;
    }

    pub async fn select_mailbox(&self, tag: &str, name: &str, is_examine: bool) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        // A read-only user cannot change any mailbox, so its SELECT must
        // announce `[READ-ONLY]` too; otherwise clients queue flag writes
        // that every changing command then refuses.
        let read_only = {
            let mut state = self.state();
            state.mailbox_read_only = is_examine || state.is_read_only_user;
            state.mailbox_read_only
        };
        let command = if is_examine { "EXAMINE" } else { "SELECT" };
        mailbox_ops::select_mailbox(tag, name, read_only, command, &store, self).await;
    }

    /// Records which mailbox is selected and how many messages it holds.
    pub fn set_selected(&self, mailbox: Option<&str>, count: usize) {
        let mut state = self.state();
        state.selected_mailbox = mailbox.map(str::to_string);
        state.selected_mailbox_message_count = count;
    }

    pub fn clear_seq_state(&self) {
        let mut state = self.state();
        state.seq_state.seq_to_uid = Vec::new();
        state.seq_state.uid_to_seq.clear();
    }

    // -----------------------------------------------------------------------
    // Message operations
    // -----------------------------------------------------------------------

    pub async fn fetch_messages_typed(&self, tag: &str, fetch_request: &FetchRequest, is_uid_command: bool) {
        let Some((store, selected_mailbox)) = self.require_selected(tag) else {
            return;
        };
        // RFC 4551 §3.3.1: the CHANGEDSINCE modifier turns CONDSTORE on for
        // the session; later FETCH and STORE responses carry MODSEQ, and so
        // does this one.
        let (condstore_enabled, is_read_only_user) = {
            let mut state = self.state();
            if fetch_request.changed_since.is_some() {
                state.condstore_enabled = true;
            }
            (state.condstore_enabled, state.is_read_only_user)
        };
        message_ops::fetch_messages_typed(
            tag,
            fetch_request,
            is_uid_command,
            &store,
            &selected_mailbox,
            self,
            condstore_enabled,
            is_read_only_user,
        )
        .await;
    }

    pub async fn search_typed(&self, tag: &str, search_request: &SearchRequest, is_uid_command: bool) {
        let Some((store, selected_mailbox)) = self.require_selected(tag) else {
            return;
        };
        message_ops::search_typed(tag, search_request, is_uid_command, &store, &selected_mailbox, self).await;
    }

    pub async fn store_flags_typed(&self, tag: &str, store_request: &StoreRequest, is_uid_command: bool) {
        let Some((store, selected_mailbox)) = self.require_selected(tag) else {
            return;
        };
        if !self.allow_mutation(tag, if is_uid_command { "UID STORE" } else { "STORE" }) {
            return;
        }
        // RFC 7162 §3.1.3: like CHANGEDSINCE on FETCH, an UNCHANGEDSINCE
        // modifier turns CONDSTORE on, so a client that never sent ENABLE
        // CONDSTORE still gets MODSEQ from here on.
        let (condstore_enabled, mailbox_read_only) = {
            let mut state = self.state();
            if store_request.unchanged_since.is_some() {
                state.condstore_enabled = true;
            }
            (state.condstore_enabled, state.mailbox_read_only)
        };
        message_ops::store_flags_typed(
            tag,
            store_request,
            is_uid_command,
            &store,
            &selected_mailbox,
            mailbox_read_only,
            self,
            condstore_enabled,
        )
        .await;
    }

    /// The signed-in account and the selected mailbox for COPY and MOVE,
    /// which answer both missing states with one line.
    fn require_copy_source(&self, tag: &str) -> Option<(Store, String)> {
        let found = {
            let state = self.state();
            match (state.authenticated, &state.store, &state.selected_mailbox) {
                (true, Some(store), Some(mailbox)) => Some((store.clone(), mailbox.clone())),
                _ => None,
            }
        };
        if found.is_none() {
            self.write(&format!("{tag} NO Not authenticated or no mailbox selected.\r\n"));
        }
        found
    }

    pub async fn copy_message_typed(&self, tag: &str, copy_request: &CopyRequest, is_uid_command: bool) {
        let Some((store, selected_mailbox)) = self.require_copy_source(tag) else {
            return;
        };
        if !self.allow_mutation(tag, if is_uid_command { "UID COPY" } else { "COPY" }) {
            return;
        }
        message_ops::copy_message_typed(tag, copy_request, is_uid_command, &store, &selected_mailbox, self).await;
    }

    pub async fn move_message_typed(&self, tag: &str, move_request: &MoveRequest, is_uid_command: bool) {
        let Some((store, selected_mailbox)) = self.require_copy_source(tag) else {
            return;
        };
        if !self.allow_mutation(tag, if is_uid_command { "UID MOVE" } else { "MOVE" }) {
            return;
        }
        let mailbox_read_only = self.state().mailbox_read_only;
        message_ops::move_message_typed(
            tag,
            move_request,
            is_uid_command,
            &store,
            &selected_mailbox,
            mailbox_read_only,
            self,
        )
        .await;
    }

    pub async fn append_message(&self, tag: &str, append_request: &AppendRequest) {
        let Some(store) = self.require_store(tag) else {
            return;
        };
        if !self.allow_mutation(tag, "APPEND") {
            return;
        }
        let selected_mailbox = self.state().selected_mailbox.clone();
        message_ops::append_message(tag, append_request, &store, selected_mailbox.as_deref(), self).await;
    }

    pub async fn expunge(&self, tag: &str) {
        let Some((store, selected_mailbox)) = self.require_selected(tag) else {
            return;
        };
        if !self.allow_mutation(tag, "EXPUNGE") {
            return;
        }
        let mailbox_read_only = self.state().mailbox_read_only;
        message_ops::expunge(tag, &store, &selected_mailbox, mailbox_read_only, self).await;
    }

    // -----------------------------------------------------------------------
    // Mailbox close / deselect
    // -----------------------------------------------------------------------

    pub fn close_mailbox(&self, tag: &str, unselect: bool) {
        if self.require_selected(tag).is_none() {
            return;
        }
        {
            let mut state = self.state();
            state.selected_mailbox = None;
            state.selected_mailbox_message_count = 0;
            state.seq_state.seq_to_uid = Vec::new();
            state.seq_state.uid_to_seq.clear();
        }
        let verb = if unselect { "UNSELECT" } else { "CLOSE" };
        self.write(&format!("{tag} OK {verb} completed\r\n"));
    }

    // -----------------------------------------------------------------------
    // LOGOUT
    // -----------------------------------------------------------------------

    pub fn logout(&self, tag: &str) {
        if self.is_in_idle_mode() {
            self.end_idle(None);
        }
        {
            let mut state = self.state();
            state.store = None;
            state.selected_mailbox = None;
            state.selected_mailbox_message_count = 0;
            state.seq_state.seq_to_uid = Vec::new();
            state.seq_state.uid_to_seq.clear();
            state.authenticated = false;
        }
        self.write("* BYE IMAP4rev1 Server logging out\r\n");
        self.write(&format!("{tag} OK LOGOUT completed\r\n"));
        self.close();
    }

    // -----------------------------------------------------------------------
    // IDLE
    // -----------------------------------------------------------------------

    pub fn start_idle(&self, tag: &str) {
        let entered = {
            let mut state = self.state();
            let watching = match (state.authenticated, &state.store, &state.selected_mailbox) {
                (true, Some(store), Some(mailbox)) => Some((store.get_user().username.clone(), mailbox.clone())),
                _ => None,
            };
            match watching {
                None => Err("NO Not authenticated or no mailbox selected"),
                Some(_) if state.is_idling => Err("BAD Already in IDLE mode"),
                Some(watching) => {
                    state.is_idling = true;
                    state.idle_tag = Some(tag.to_string());
                    Ok(watching)
                }
            }
        };
        let (username, mailbox) = match entered {
            Ok(watching) => watching,
            Err(refusal) => {
                self.write(&format!("{tag} {refusal}\r\n"));
                return;
            }
        };

        // A client in IDLE may send nothing for the whole window; raise the
        // socket's inactivity timeout past the idle manager's
        // force-terminate so the socket doesn't tear the connection down
        // mid-IDLE. Reset when IDLE ends.
        self.socket.set_timeout(IDLE_SOCKET_TIMEOUT_MS);

        if let Some(this) = self.this() {
            idle_manager().add_idle_session(&self.session_id, this, tag, &mailbox, &username);
        }

        self.write("+ idling\r\n");
        // DONE is read by the connection's own line buffer, which
        // reassembles split and pipelined input.
    }

    pub fn end_idle(&self, reason: Option<&str>) {
        let tag = {
            let mut state = self.state();
            if !state.is_idling || state.idle_tag.is_none() {
                return;
            }
            state.is_idling = false;
            state.idle_tag.take().unwrap_or_default()
        };
        idle_manager().remove_idle_session(&self.session_id);
        // Back to normal command mode: restore the short inactivity timeout.
        self.socket.set_timeout(SOCKET_TIMEOUT_MS);
        let suffix = reason.map(|reason| format!(" ({reason})")).unwrap_or_default();
        self.write(&format!("{tag} OK IDLE terminated{suffix}\r\n"));
    }

    pub fn is_in_idle_mode(&self) -> bool {
        self.state().is_idling
    }

    pub fn is_authenticated(&self) -> bool {
        self.state().authenticated
    }

    pub fn get_session_id(&self) -> &str {
        &self.session_id
    }

    pub fn get_authenticated_as(&self) -> Option<String> {
        self.state().authenticated_as.clone()
    }

    pub fn selected_mailbox(&self) -> Option<String> {
        self.state().selected_mailbox.clone()
    }

    /// The tag of an AUTHENTICATE still waiting for its response line, taken.
    pub fn take_pending_sasl_tag(&self) -> Option<String> {
        self.state().pending_sasl_tag.take()
    }

    /// The TLS setup of a STARTTLS that was just answered OK, taken.
    pub fn take_tls_upgrade(&self) -> Option<TlsAcceptor> {
        self.state().tls_upgrade.take()
    }

    /// Credential-revocation fence. Compares the generation the connection
    /// was stamped with at sign-in against the credential row's current
    /// version. On a mismatch — the password was rotated after this
    /// connection signed in, or the account was deleted — the session is
    /// de-authenticated and unregistered, and the command is answered `NO
    /// [AUTHENTICATIONFAILED]` so the client signs in again instead of
    /// retrying the same grant. True when the caller may go on.
    ///
    /// This is what reaches the connections no sweep can: another replica's
    /// registry, or a LOGIN whose row read preceded the rotation.
    ///
    /// A lookup fault is returned: the dispatch answers it as an internal
    /// error rather than letting through a grant that could not be proven
    /// current.
    pub async fn verify_credential_version(&self, tag: &str) -> Result<bool> {
        // An unstamped session carries no grant to re-check, its scope included.
        let stamped = {
            let state = self.state();
            state.authenticated && state.authenticated_as.is_some() && state.credential_password_version.is_some()
        };
        if !stamped {
            return Ok(true);
        }
        let current = self
            .verify_credential_version_with(tag, |username: &str| {
                let username = username.to_string();
                async move { get_credential_password_version(&username).await }
            })
            .await?;
        if !current {
            return Ok(false);
        }
        self.verify_scope_with(tag, |user_id: String, domain: String| async move {
            Ok(get_domain_access(&user_id, &domain).await?)
        })
        .await
    }

    /// The role a scoped login rests on, re-read on every command: a viewer
    /// whose role was revoked, or a domain that moved to another owner, loses
    /// the session at its next command on every server, not only where the
    /// change was made. Same refusal shape as a rotated credential. True when
    /// the caller may go on.
    pub async fn verify_scope_with<F, Fut>(&self, tag: &str, access_of: F) -> Result<bool>
    where
        F: FnOnce(String, String) -> Fut,
        Fut: std::future::Future<Output = Result<Option<DomainAccess>>>,
    {
        let (scope, authenticated_as) = {
            let state = self.state();
            match (&state.scope, state.authenticated) {
                (Some(scope), true) => (scope.clone(), state.authenticated_as.clone()),
                _ => return Ok(true),
            }
        };
        let access = access_of(scope.user_id.clone(), scope.domain.clone()).await?;
        if access.is_some_and(|access| access.role == scope.role && access.owner_id == scope.owner_id) {
            return Ok(true);
        }
        {
            let mut state = self.state();
            state.authenticated = false;
            state.credential_password_version = None;
        }
        if let Some(this) = self.this() {
            let registered: Arc<dyn RegisteredSession> = this;
            unregister_session(&registered);
        }
        logger::info(
            "IMAP session de-authenticated: domain role changed after auth",
            json!({
                "component": "imap",
                "authenticatedAs": authenticated_as,
                "scopeDomain": scope.domain,
                "sessionId": self.session_id,
            }),
        );
        self.write(&format!(
            "{tag} NO [AUTHENTICATIONFAILED] Your role on {} changed, please re-authenticate\r\n",
            scope.domain
        ));
        Ok(false)
    }

    /// [`ImapSession::verify_credential_version`], reading the credential
    /// row's version with `current_version`.
    pub async fn verify_credential_version_with<F, Fut>(&self, tag: &str, current_version: F) -> Result<bool>
    where
        F: FnOnce(&str) -> Fut,
        Fut: std::future::Future<Output = Result<Option<i64>>>,
    {
        let (authenticated_as, stamped) = {
            let state = self.state();
            match (state.authenticated, &state.authenticated_as, state.credential_password_version) {
                (true, Some(authenticated_as), Some(stamped)) => (authenticated_as.clone(), stamped),
                _ => return Ok(true),
            }
        };
        let current = current_version(&authenticated_as).await?;
        // A credential whose row is gone is revoked, as a rotated one is.
        if current.is_some_and(|current| current <= stamped) {
            return Ok(true);
        }
        {
            let mut state = self.state();
            state.authenticated = false;
            state.credential_password_version = None;
        }
        if let Some(this) = self.this() {
            let registered: Arc<dyn RegisteredSession> = this;
            unregister_session(&registered);
        }
        logger::info(
            if current.is_none() {
                "IMAP session de-authenticated: account removed after auth"
            } else {
                "IMAP session de-authenticated: credential rotated after auth"
            },
            json!({ "component": "imap", "authenticatedAs": authenticated_as, "sessionId": self.session_id }),
        );
        self.write(&format!("{tag} NO [AUTHENTICATIONFAILED] Credential changed, please re-authenticate\r\n"));
        Ok(false)
    }

    /// Runs when the connection's socket has closed.
    pub fn cleanup(&self) {
        if let Some(this) = self.this() {
            let registered: Arc<dyn RegisteredSession> = this;
            unregister_session(&registered);
            unregister_connection(&registered);
        }
        let was_idling = {
            let mut state = self.state();
            let was_idling = state.is_idling;
            state.is_idling = false;
            if was_idling {
                state.idle_tag = None;
            }
            was_idling
        };
        if was_idling {
            idle_manager().remove_idle_session(&self.session_id);
            logger::debug(
                "IDLE session cleaned up on socket close",
                json!({ "component": "imap", "sessionId": self.session_id }),
            );
        }
    }

    // -----------------------------------------------------------------------
    // STARTTLS
    // -----------------------------------------------------------------------

    /// Answers STARTTLS. When the answer is OK the TLS setup is left for the
    /// connection to pick up (`take_tls_upgrade`): it stops reading
    /// cleartext, runs the handshake and starts a clean session on the
    /// encrypted stream.
    pub fn start_tls(&self, tag: &str) {
        self.start_tls_with(tag, get_tls_credentials, crate::tls::acceptor);
    }

    /// STARTTLS, with the certificate settings and the TLS setup read by the
    /// two functions given; each is called only once it is needed.
    pub fn start_tls_with(
        &self,
        tag: &str,
        credentials: impl FnOnce() -> TlsCredentials,
        acceptor: impl FnOnce() -> anyhow::Result<TlsAcceptor>,
    ) {
        // RFC 3501 §6.2.1 / RFC 2595 §3: STARTTLS is only valid on a
        // cleartext connection in the not-authenticated state.
        if self.is_tls {
            self.write(&format!("{tag} BAD STARTTLS not permitted on a TLS connection\r\n"));
            return;
        }
        if self.is_authenticated() {
            self.write(&format!("{tag} BAD STARTTLS not permitted after authentication\r\n"));
            return;
        }

        // CAPABILITY does not offer STARTTLS without a usable certificate,
        // so reaching here means the client asked for an extension it was
        // not offered.
        if !matches!(credentials(), TlsCredentials::Available { .. }) {
            self.write(&format!("{tag} NO STARTTLS is not available\r\n"));
            return;
        }

        // Read the key pair before answering: a present but unparseable pair
        // is the last failure that can still be reported as a clean tagged
        // NO, because nothing on the connection has changed yet.
        let acceptor = match acceptor() {
            Ok(acceptor) => acceptor,
            Err(error) => {
                logger::error_with(
                    "IMAP STARTTLS could not be prepared",
                    json!({ "component": "imap", "tag": tag }),
                    &format!("{error:#}"),
                );
                self.write(&format!("{tag} NO STARTTLS is not available\r\n"));
                return;
            }
        };

        // The tagged OK goes out in CLEARTEXT and BEFORE the handshake (RFC
        // 3501 §6.2.1: negotiation begins right after its CRLF). A
        // conforming client holds its ClientHello until it reads this line.
        self.write(&format!("{tag} OK Begin TLS negotiation now\r\n"));
        self.state().tls_upgrade = Some(acceptor);
    }

    #[cfg(test)]
    pub(crate) fn sign_in_for_test(&self, username: &str, selected_mailbox: Option<&str>) {
        let mut state = self.state();
        state.authenticated = true;
        state.store = Some(Store::new(SignedUser::new(Some("user-1".into()), Some(username.into()), Some("u@test".into())), crate::imap::store::StoreScope::whole_store(&crate::domains::account_domain(username))));
        state.authenticated_as = Some(username.to_string());
        state.selected_mailbox = selected_mailbox.map(str::to_string);
    }
}

impl AuthConnection for ImapSession {
    fn remote_address(&self) -> String {
        self.socket.remote_address().to_string()
    }

    fn remote_port(&self) -> u16 {
        self.socket.remote_port()
    }

    fn write(&self, data: &str) -> bool {
        ImapSession::write(self, data)
    }

    fn close(&self) {
        close_socket(&self.socket);
    }

    fn set_pending_sasl_tag(&self, tag: &str) {
        self.state().pending_sasl_tag = Some(tag.to_string());
    }

    fn capabilities(&self) -> String {
        self.get_capabilities()
    }
}

impl RegisteredSession for ImapSession {
    fn authenticated_as(&self) -> Option<String> {
        self.get_authenticated_as()
    }

    fn session_id(&self) -> String {
        self.session_id.clone()
    }

    fn write(&self, data: &str) -> bool {
        ImapSession::write(self, data)
    }

    fn close(&self) {
        ImapSession::close(self);
    }

    fn is_closing(&self) -> bool {
        self.socket.is_destroyed() || self.socket.is_writable_ended()
    }
}

impl IdleTarget for ImapSession {
    fn notify_mailbox_update(&self) -> IdleFuture<'_, Option<usize>> {
        Box::pin(ImapSession::notify_mailbox_update(self))
    }

    fn notify_flag_change<'a>(&'a self, mail_ids: &'a [String]) -> IdleFuture<'a, ()> {
        Box::pin(ImapSession::notify_flag_change(self, mail_ids))
    }

    fn end_idle(&self, reason: Option<&str>) {
        ImapSession::end_idle(self, reason);
    }

    fn write(&self, data: &str) -> bool {
        ImapSession::write(self, data)
    }

    fn close(&self) {
        ImapSession::close(self);
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
