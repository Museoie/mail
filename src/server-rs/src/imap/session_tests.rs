//! Tests of the session: session-version.test.ts, the registration wiring of
//! session-registry.test.ts, notify-mailbox-update.test.ts,
//! notify-flag-change.test.ts, the STARTTLS refusals of starttls.test.ts,
//! and what the store-backed commands answer before they reach the store.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::io::{AsyncReadExt, ReadHalf};

use super::*;
use crate::imap::parsers::command_parser::parse_command;
use crate::imap::session_registry::evict_imap_sessions;
use crate::imap::socket::BoxedStream;
use crate::imap::test_support::{self, FakeAuthBackend};
use crate::imap::types::ImapRequest;
use crate::models::User;

/// A session on an in-memory socket, with what it wrote collected.
struct Fixture {
    session: Arc<ImapSession>,
    received: Arc<Mutex<Vec<u8>>>,
    _read: ReadHalf<BoxedStream>,
}

fn fixture(is_tls: bool) -> Fixture {
    let (socket, read, mut client) = SocketHandle::for_test("203.0.113.7");
    let session = ImapSession::new(socket, is_tls);
    let received: Arc<Mutex<Vec<u8>>> = Arc::default();
    tokio::spawn({
        let received = received.clone();
        async move {
            let mut chunk = vec![0u8; 16 * 1024];
            while let Ok(read) = client.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                received.lock().unwrap().extend_from_slice(&chunk[..read]);
            }
        }
    });
    Fixture { session, received, _read: read }
}

impl Fixture {
    /// Everything written so far, once the writer has caught up.
    async fn wire(&self) -> String {
        tokio::time::sleep(Duration::from_millis(30)).await;
        String::from_utf8_lossy(&self.received.lock().unwrap()).into_owned()
    }
}

fn delay(ms: u64) -> tokio::time::Sleep {
    tokio::time::sleep(Duration::from_millis(ms))
}

/// Signed in as `authenticated_as`, stamped with credential generation
/// `stamped`.
fn prime(session: &ImapSession, authenticated_as: &str, stamped: i64) {
    let mut state = session.state();
    state.authenticated = true;
    state.store = Some(Store::new(SignedUser::new(Some("user-1".into()), Some(authenticated_as.into()), Some("u@test".into())), crate::imap::store::StoreScope::whole_store(&crate::domains::account_domain(authenticated_as))));
    state.authenticated_as = Some(authenticated_as.into());
    state.credential_password_version = Some(stamped);
}

// ---------------------------------------------------------------------------
// The credential-revocation fence (session-version.test.ts)
// ---------------------------------------------------------------------------

mod verify_credential_version {
    use super::*;

    #[tokio::test]
    async fn de_authenticates_and_answers_authenticationfailed_when_the_credential_rotated_after_sign_in() {
        let fixture = fixture(false);
        // The row is on generation 2; this connection was stamped with 1.
        prime(&fixture.session, "fence-alice-1", 1);
        register_authenticated_session(fixture.session.clone());

        let ok = fixture.session.verify_credential_version_with("A1", |_| async { Ok(Some(2)) }).await.unwrap();

        assert!(!ok);
        assert_eq!(fixture.wire().await, "A1 NO [AUTHENTICATIONFAILED] Credential changed, please re-authenticate\r\n");
        // The grant is gone: a follow-up command sees an unauthenticated
        // session, and the eviction sweep no longer reaches it.
        assert!(!fixture.session.is_authenticated());
        assert_eq!(evict_imap_sessions("fence-alice-1"), 0);
    }

    fn scoped(fixture: &Fixture, role: crate::postgres::repositories::domains::DomainRole) {
        fixture.session.state().scope = Some(crate::imap::auth::SessionScope {
            user_id: "user-1".into(),
            domain: "example.org".into(),
            role,
            owner_id: "owner-1".into(),
        });
    }

    fn access(
        role: crate::postgres::repositories::domains::DomainRole,
        owner_id: &str,
    ) -> crate::postgres::repositories::domains::DomainAccess {
        crate::postgres::repositories::domains::DomainAccess {
            domain_id: "domain-1".into(),
            name: "example.org".into(),
            owner_id: owner_id.into(),
            owner_username: "owner".into(),
            role,
            verified_at: None,
        }
    }

    #[tokio::test]
    async fn keeps_a_scoped_session_whose_role_is_unchanged() {
        use crate::postgres::repositories::domains::DomainRole;
        let fixture = fixture(false);
        prime(&fixture.session, "scope-alice-1", 1);
        scoped(&fixture, DomainRole::Viewer);
        let ok = fixture
            .session
            .verify_scope_with("S1", |user_id, domain| async move {
                assert_eq!((user_id.as_str(), domain.as_str()), ("user-1", "example.org"));
                Ok(Some(access(DomainRole::Viewer, "owner-1")))
            })
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(fixture.wire().await, "");
    }

    #[tokio::test]
    async fn ends_a_scoped_session_whose_role_was_revoked_or_moved() {
        use crate::postgres::repositories::domains::DomainRole;
        for (tag, now) in [
            ("S2", None),
            ("S3", Some(access(DomainRole::Owner, "owner-1"))),
            ("S4", Some(access(DomainRole::Viewer, "someone-else"))),
        ] {
            let fixture = fixture(false);
            prime(&fixture.session, "scope-alice-2", 1);
            scoped(&fixture, DomainRole::Viewer);
            let ok = fixture.session.verify_scope_with(tag, |_, _| async move { Ok(now) }).await.unwrap();
            assert!(!ok, "{tag}");
            assert!(!fixture.session.is_authenticated());
            assert_eq!(
                fixture.wire().await,
                format!("{tag} NO [AUTHENTICATIONFAILED] Your role on example.org changed, please re-authenticate\r\n")
            );
        }
    }

    #[tokio::test]
    async fn lets_the_command_through_when_the_stamped_version_is_current() {
        let fixture = fixture(false);
        prime(&fixture.session, "fence-alice-2", 2);

        let ok = fixture.session.verify_credential_version_with("A2", |_| async { Ok(Some(2)) }).await.unwrap();

        assert!(ok);
        assert_eq!(fixture.wire().await, "");
        assert!(fixture.session.is_authenticated());
    }

    #[tokio::test]
    async fn treats_a_missing_version_as_generation_1_on_both_sides() {
        let fixture = fixture(false);
        // A row that predates the fence reads back as 1, and so does the
        // stamp of a sign-in on such a row.
        prime(&fixture.session, "fence-alice-3", 1);
        assert!(fixture.session.verify_credential_version_with("A3", |_| async { Ok(Some(1)) }).await.unwrap());
        assert!(fixture.session.is_authenticated());
    }

    #[tokio::test]
    async fn de_authenticates_when_the_account_is_gone() {
        let fixture = fixture(false);
        // The credential row was deleted, or soft-deleted, after sign-in.
        prime(&fixture.session, "fence-alice-7", 1);
        register_authenticated_session(fixture.session.clone());

        let ok = fixture.session.verify_credential_version_with("A7", |_| async { Ok(None) }).await.unwrap();

        assert!(!ok);
        assert_eq!(fixture.wire().await, "A7 NO [AUTHENTICATIONFAILED] Credential changed, please re-authenticate\r\n");
        assert!(!fixture.session.is_authenticated());
        assert_eq!(evict_imap_sessions("fence-alice-7"), 0);
    }

    #[tokio::test]
    async fn does_not_touch_the_database_for_an_unauthenticated_session() {
        let fixture = fixture(false);
        let ok = fixture
            .session
            .verify_credential_version_with("A4", |_| async { panic!("no lookup for an unauthenticated session") })
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(fixture.wire().await, "");
    }

    #[tokio::test]
    async fn keys_the_comparison_on_the_credential_presented_not_a_remapped_user() {
        let fixture = fixture(false);
        prime(&fixture.session, "admin-ro", 1);
        fixture.session.state().store = Some(Store::new(SignedUser::new(Some("user-admin".into()), Some("admin".into()), Some("a@test".into())), crate::imap::store::StoreScope::whole_store(&crate::domains::account_domain("admin"))));

        let ok = fixture
            .session
            .verify_credential_version_with("A5", |username: &str| {
                assert_eq!(username, "admin-ro");
                async { Ok(Some(2)) }
            })
            .await
            .unwrap();

        assert!(!ok);
        assert!(!fixture.session.is_authenticated());
    }

    #[tokio::test]
    async fn a_failing_lookup_is_returned_rather_than_letting_the_grant_through() {
        let fixture = fixture(false);
        prime(&fixture.session, "fence-alice-6", 1);
        let outcome = fixture.session.verify_credential_version_with("A6", |_| async { Err(anyhow::anyhow!("down")) }).await;
        assert!(outcome.is_err());
        assert!(fixture.session.is_authenticated());
    }
}

// ---------------------------------------------------------------------------
// Registration (session-registry.test.ts)
// ---------------------------------------------------------------------------

mod registration_wiring {
    use super::*;

    const PASSWORD: &str = "correct-horse";

    fn accounts(username: &str) -> FakeAuthBackend {
        FakeAuthBackend::with_users(vec![User {
            id: Some(format!("id-{username}")),
            email: Some(format!("{username}@test.com")),
            username: Some(username.into()),
            password: Some(bcrypt::hash(PASSWORD, 4).unwrap()),
            ..User::default()
        }])
    }

    fn plain(username: &str, password: &str) -> String {
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("\0{username}\0{password}"))
    }

    #[tokio::test]
    async fn registers_a_session_that_completed_login_and_the_sweep_reaches_it() {
        let fixture = fixture(false);
        let username = "wiring-login";
        fixture.session.login_with("A1", username, PASSWORD, &accounts(username)).await.unwrap();
        assert!(fixture.wire().await.contains("A1 OK"));

        assert_eq!(evict_imap_sessions(username), 1);
        assert!(fixture.wire().await.contains("* BYE Credential changed, please re-authenticate\r\n"));
        assert_eq!(evict_imap_sessions(username), 0);
    }

    #[tokio::test]
    async fn registers_a_session_that_completed_authenticate_plain() {
        let fixture = fixture(false);
        let username = "wiring-plain";
        fixture.session.authenticate_with("A1", "PLAIN", Some(&plain(username, PASSWORD)), &accounts(username)).await;
        assert!(fixture.wire().await.contains("A1 OK"));

        assert_eq!(evict_imap_sessions(username), 1);
        assert!(fixture.wire().await.contains("* BYE Credential changed"));
    }

    #[tokio::test]
    async fn does_not_register_a_session_whose_login_failed() {
        let fixture = fixture(false);
        let username = "wiring-failed";
        fixture.session.login_with("A1", username, "wrong-password", &accounts(username)).await.unwrap();
        assert!(fixture.wire().await.contains("AUTHENTICATIONFAILED"));
        assert!(!fixture.session.is_authenticated());
        assert_eq!(evict_imap_sessions(username), 0);
    }

    #[tokio::test]
    async fn unregisters_on_cleanup_which_runs_on_every_socket_close() {
        let fixture = fixture(false);
        let username = "wiring-cleanup";
        fixture.session.login_with("A1", username, PASSWORD, &accounts(username)).await.unwrap();
        fixture.session.cleanup();
        assert_eq!(evict_imap_sessions(username), 0);
    }

    #[tokio::test]
    async fn stamps_the_sign_in_with_the_rows_generation() {
        let fixture = fixture(false);
        let username = "wiring-stamp";
        let mut backend = accounts(username);
        backend.users.get_mut(username).unwrap().password_version = Some(5);
        fixture.session.login_with("A1", username, PASSWORD, &backend).await.unwrap();
        assert_eq!(fixture.session.state().credential_password_version, Some(5));
        assert_eq!(fixture.session.get_authenticated_as().as_deref(), Some(username));
        fixture.session.cleanup();
    }
}

// ---------------------------------------------------------------------------
// IDLE notifications (notify-mailbox-update.test.ts, notify-flag-change.test.ts)
// ---------------------------------------------------------------------------

/// A session idling on INBOX under tag A1, having advertised `advertised`.
fn idling(advertised: &[i64]) -> Fixture {
    let fixture = fixture(false);
    {
        let mut state = fixture.session.state();
        state.authenticated = true;
        state.store = Some(Store::new(SignedUser::new(Some("user-1".into()), Some("alice".into()), Some("a@test".into())), crate::imap::store::StoreScope::whole_store(&crate::domains::account_domain("alice"))));
        state.selected_mailbox = Some("INBOX".into());
        set_sequence_mapping(&mut state.seq_state, advertised.to_vec());
        state.is_idling = true;
        state.idle_tag = Some("A1".into());
    }
    fixture
}

fn answer_uids(
    fixture: &Fixture,
    read: impl Fn() -> BoxFuture<'static, MailboxUids> + Send + Sync + 'static,
) {
    fixture.session.set_test_reads(TestReads { uids: Some(Arc::new(read)), ..TestReads::default() });
}

fn mapping(fixture: &Fixture) -> (Vec<i64>, Option<usize>) {
    let state = fixture.session.state();
    (state.seq_state.seq_to_uid.clone(), state.seq_state.uid_to_seq.get(&30).copied())
}

mod notify_mailbox_update {
    use super::*;

    #[tokio::test]
    async fn announces_departures_highest_first_then_the_new_count() {
        let fixture = idling(&[10, 20, 30, 40]);
        answer_uids(&fixture, || Box::pin(async { MailboxUids::Uids(vec![10, 30]) }));

        assert_eq!(fixture.session.notify_mailbox_update().await.unwrap(), Some(2));
        assert_eq!(fixture.wire().await, "* 4 EXPUNGE\r\n* 2 EXPUNGE\r\n* 2 EXISTS\r\n* 0 RECENT\r\n");
    }

    #[tokio::test]
    async fn writes_nothing_when_the_mailbox_cannot_be_read() {
        let fixture = idling(&[10, 20, 30]);
        answer_uids(&fixture, || Box::pin(async { MailboxUids::Unreadable }));

        assert_eq!(fixture.session.notify_mailbox_update().await.unwrap(), None);
        assert_eq!(fixture.wire().await, "");
    }

    #[tokio::test]
    async fn leaves_the_mapping_alone_when_the_mailbox_cannot_be_read() {
        let fixture = idling(&[10, 20, 30]);
        answer_uids(&fixture, || Box::pin(async { MailboxUids::Unreadable }));

        fixture.session.notify_mailbox_update().await.unwrap();
        assert_eq!(mapping(&fixture), (vec![10, 20, 30], Some(3)));
    }

    #[tokio::test]
    async fn without_a_store_nothing_is_read_and_nothing_is_sent() {
        // No store means nothing to read: the notification stops before the
        // database and the client keeps the mapping it has.
        let fixture = idling(&[10, 20, 30]);
        fixture.session.state().store = None;
        assert_eq!(fixture.session.notify_mailbox_update().await.unwrap(), None);
        assert_eq!(fixture.wire().await, "");
        assert_eq!(mapping(&fixture), (vec![10, 20, 30], Some(3)));
    }

    #[tokio::test]
    async fn announces_a_departure_once_when_two_notifications_overlap() {
        let fixture = idling(&[10, 20, 30]);
        answer_uids(&fixture, || {
            Box::pin(async {
                delay(10).await;
                MailboxUids::Uids(vec![10, 30, 40])
            })
        });

        let (first, second) = tokio::join!(fixture.session.notify_mailbox_update(), fixture.session.notify_mailbox_update());
        first.unwrap();
        second.unwrap();
        assert_eq!(fixture.wire().await, "* 2 EXPUNGE\r\n* 3 EXISTS\r\n* 0 RECENT\r\n* 3 EXISTS\r\n* 0 RECENT\r\n");
    }

    #[tokio::test]
    async fn does_not_splice_its_responses_into_a_command_already_on_the_wire() {
        let fixture = idling(&[10, 20, 30]);
        answer_uids(&fixture, || {
            Box::pin(async {
                delay(5).await;
                MailboxUids::Uids(vec![10, 30])
            })
        });

        // A FETCH response written in pieces around an await, where RFC 3501
        // §7.4.1 forbids an EXPUNGE.
        let session = fixture.session.clone();
        let command = async move {
            let _turn = session.serial().await;
            session.write("* 1 FETCH (BODY[] {4}\r\n");
            delay(20).await;
            session.write("done)\r\n");
        };
        let (_, notified) = tokio::join!(command, fixture.session.notify_mailbox_update());
        notified.unwrap();

        assert_eq!(fixture.wire().await, "* 1 FETCH (BODY[] {4}\r\ndone)\r\n* 2 EXPUNGE\r\n* 2 EXISTS\r\n* 0 RECENT\r\n");
    }

    #[tokio::test]
    async fn skips_the_mailbox_read_entirely_when_the_session_is_no_longer_idling() {
        let fixture = idling(&[10, 20, 30]);
        let reads = Arc::new(AtomicUsize::new(0));
        answer_uids(&fixture, {
            let reads = reads.clone();
            move || {
                reads.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { MailboxUids::Uids(vec![10, 20]) })
            }
        });
        fixture.session.end_idle(None);
        let ended = fixture.wire().await;

        assert_eq!(fixture.session.notify_mailbox_update().await.unwrap(), None);
        assert_eq!(fixture.wire().await, ended);
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn does_not_announce_after_a_done_queued_ahead_of_it() {
        let fixture = idling(&[10, 20, 30]);
        let reads = Arc::new(Mutex::new(vec![vec![10, 20, 30], vec![10, 20]]));
        answer_uids(&fixture, move || {
            let reads = reads.clone();
            Box::pin(async move {
                delay(10).await;
                let next = { reads.lock().unwrap().drain(..1).next().unwrap_or_default() };
                MailboxUids::Uids(next)
            })
        });

        let session = fixture.session.clone();
        let done = async move {
            let _turn = session.serial().await;
            session.end_idle(None);
        };
        let (first, _, second) =
            tokio::join!(fixture.session.notify_mailbox_update(), done, fixture.session.notify_mailbox_update());

        assert_eq!(first.unwrap(), Some(3));
        assert_eq!(second.unwrap(), None);
        assert_eq!(fixture.wire().await, "* 3 EXISTS\r\n* 0 RECENT\r\nA1 OK IDLE terminated\r\n");
        assert_eq!(mapping(&fixture).0, [10, 20, 30]);
    }

    #[tokio::test]
    async fn does_not_announce_when_idle_ends_while_its_mailbox_read_is_parked() {
        let fixture = idling(&[10, 20, 30]);
        let session = Arc::downgrade(&fixture.session);
        answer_uids(&fixture, move || {
            // What the heartbeat's force-terminate does, off the turn.
            if let Some(session) = session.upgrade() {
                session.end_idle(Some("timeout"));
            }
            Box::pin(async {
                delay(10).await;
                MailboxUids::Uids(vec![10, 20])
            })
        });

        assert_eq!(fixture.session.notify_mailbox_update().await.unwrap(), None);
        assert_eq!(fixture.wire().await, "A1 OK IDLE terminated (timeout)\r\n");
        assert_eq!(mapping(&fixture), (vec![10, 20, 30], Some(3)));
    }
}

mod notify_flag_change {
    use super::*;

    fn update(uid: i64, read: bool, saved: bool) -> MailFlagUpdate {
        MailFlagUpdate { uid, read, saved, deleted: false, draft: false, answered: false }
    }

    fn answer_flags(
        fixture: &Fixture,
        read: impl Fn(Vec<String>) -> BoxFuture<'static, Result<Option<Vec<MailFlagUpdate>>>> + Send + Sync + 'static,
    ) {
        fixture.session.set_test_reads(TestReads { flags: Some(Arc::new(read)), ..TestReads::default() });
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[tokio::test]
    async fn emits_untagged_fetch_flags_with_sequence_numbers_not_uids() {
        let fixture = idling(&[10, 20, 30]);
        answer_flags(&fixture, |_| Box::pin(async { Ok(Some(vec![update(10, true, false), update(30, true, true)])) }));

        fixture.session.notify_flag_change(&ids(&["mail-10", "mail-30"])).await.unwrap();
        assert_eq!(fixture.wire().await, "* 1 FETCH (FLAGS (\\Seen))\r\n* 3 FETCH (FLAGS (\\Seen \\Flagged))\r\n");
    }

    #[tokio::test]
    async fn skips_updates_for_mails_outside_the_sessions_current_view() {
        let fixture = idling(&[10, 20]);
        answer_flags(&fixture, |_| Box::pin(async { Ok(Some(vec![update(10, true, false), update(99, true, false)])) }));

        fixture.session.notify_flag_change(&ids(&["mail-10", "mail-99"])).await.unwrap();
        assert_eq!(fixture.wire().await, "* 1 FETCH (FLAGS (\\Seen))\r\n");
    }

    #[tokio::test]
    async fn writes_nothing_when_the_session_is_not_idling() {
        let fixture = idling(&[10]);
        fixture.session.state().is_idling = false;
        answer_flags(&fixture, |_| Box::pin(async { Ok(Some(vec![update(10, true, false)])) }));

        fixture.session.notify_flag_change(&ids(&["mail-10"])).await.unwrap();
        assert_eq!(fixture.wire().await, "");
    }

    #[tokio::test]
    async fn writes_nothing_when_the_flag_read_fails() {
        let fixture = idling(&[10]);
        answer_flags(&fixture, |_| Box::pin(async { Ok(None) }));

        fixture.session.notify_flag_change(&ids(&["mail-10"])).await.unwrap();
        assert_eq!(fixture.wire().await, "");
    }

    #[tokio::test]
    async fn serializes_concurrent_notifications_so_fetch_lines_never_interleave() {
        let fixture = idling(&[10, 20]);
        answer_flags(&fixture, |_| {
            Box::pin(async {
                delay(10).await;
                Ok(Some(vec![update(10, true, false), update(20, true, false)]))
            })
        });

        let mail = ids(&["mail-10", "mail-20"]);
        let (first, second) =
            tokio::join!(fixture.session.notify_flag_change(&mail), fixture.session.notify_flag_change(&mail));
        first.unwrap();
        second.unwrap();
        assert_eq!(
            fixture.wire().await,
            "* 1 FETCH (FLAGS (\\Seen))\r\n* 2 FETCH (FLAGS (\\Seen))\r\n* 1 FETCH (FLAGS (\\Seen))\r\n* 2 FETCH (FLAGS (\\Seen))\r\n"
        );
    }
}

// ---------------------------------------------------------------------------
// STARTTLS (starttls.test.ts)
// ---------------------------------------------------------------------------

mod starttls_refusals {
    use super::*;

    fn unreadable() -> TlsCredentials {
        TlsCredentials::Unreadable { cert: "/nonexistent/cert.pem".into(), key: "/nonexistent/key.pem".into() }
    }

    fn available() -> TlsCredentials {
        TlsCredentials::Available { cert: "cert.pem".into(), key: "key.pem".into() }
    }

    fn never() -> anyhow::Result<TlsAcceptor> {
        panic!("the TLS setup is not read for a refusal")
    }

    #[tokio::test]
    async fn answers_no_when_no_certificate_is_configured() {
        let fixture = fixture(false);
        fixture.session.start_tls_with("A1", || TlsCredentials::Unconfigured, never);
        assert_eq!(fixture.wire().await, "A1 NO STARTTLS is not available\r\n");
        assert!(fixture.session.take_tls_upgrade().is_none());
    }

    #[tokio::test]
    async fn answers_no_from_the_process_settings_when_nothing_is_configured() {
        // Unit tests run with no certificate in the environment.
        let fixture = fixture(false);
        fixture.session.start_tls("A1");
        assert_eq!(fixture.wire().await, "A1 NO STARTTLS is not available\r\n");
        assert!(fixture.session.take_tls_upgrade().is_none());
    }

    #[tokio::test]
    async fn answers_no_when_the_configured_certificate_files_are_absent() {
        let fixture = fixture(false);
        fixture.session.start_tls_with("A1", unreadable, never);
        assert_eq!(fixture.wire().await, "A1 NO STARTTLS is not available\r\n");
        assert!(fixture.session.take_tls_upgrade().is_none());
    }

    #[tokio::test]
    async fn answers_bad_on_the_implicit_tls_port_even_with_a_usable_certificate() {
        let fixture = fixture(true);
        fixture.session.start_tls_with("A1", available, never);
        assert_eq!(fixture.wire().await, "A1 BAD STARTTLS not permitted on a TLS connection\r\n");
        assert!(fixture.session.take_tls_upgrade().is_none());
    }

    #[tokio::test]
    async fn answers_bad_after_authentication() {
        let fixture = fixture(false);
        prime(&fixture.session, "alice", 1);
        fixture.session.start_tls_with("A1", available, never);
        assert_eq!(fixture.wire().await, "A1 BAD STARTTLS not permitted after authentication\r\n");
        assert!(fixture.session.take_tls_upgrade().is_none());
    }

    #[tokio::test]
    async fn answers_no_without_upgrading_when_the_files_are_not_a_key_pair() {
        let fixture = fixture(false);
        fixture.session.start_tls_with("A1", available, || Err(anyhow::anyhow!("no private key found")));
        // Nothing on the connection has changed: the refusal is a clean NO.
        assert_eq!(fixture.wire().await, "A1 NO STARTTLS is not available\r\n");
        assert!(fixture.session.take_tls_upgrade().is_none());
    }

    #[tokio::test]
    async fn answers_ok_in_the_clear_and_leaves_the_upgrade_for_the_connection() {
        let Some(acceptor) = test_support::acceptor() else {
            return;
        };
        let fixture = fixture(false);
        fixture.session.start_tls_with("A1", available, move || Ok(acceptor));
        assert_eq!(fixture.wire().await, "A1 OK Begin TLS negotiation now\r\n");
        assert!(fixture.session.take_tls_upgrade().is_some());
        // Taken once.
        assert!(fixture.session.take_tls_upgrade().is_none());
    }
}

// ---------------------------------------------------------------------------
// The store-backed commands before the store (phase 1)
// ---------------------------------------------------------------------------

mod store_backed_commands {
    use super::*;

    fn parsed(line: &str) -> ImapRequest {
        parse_command(line, None).into_result().expect("the command parses").request
    }

    /// Runs `line` on the session the way the dispatch does, without the
    /// command budget.
    async fn run(fixture: &Fixture, line: &str) -> String {
        let before = fixture.wire().await.len();
        let request = parsed(line);
        let tag = line.split(' ').next().unwrap();
        crate::imap::handler::handle_request(&fixture.session, tag, request).await;
        fixture.wire().await[before..].to_string()
    }

    fn selected(mailbox: &str, read_only_user: bool, examined: bool) -> Fixture {
        let fixture = fixture(false);
        {
            let mut state = fixture.session.state();
            state.authenticated = true;
            // A read-only session is a login scoped to a domain the person
            // only views: the owner's store, filtered to that domain.
            let scope = if read_only_user {
                let domains = vec!["example.com".to_string()];
                crate::imap::store::StoreScope {
                    domains: Some(domains.clone()),
                    account_domains: domains,
                    primary_domain: "example.com".into(),
                }
            } else {
                crate::imap::store::StoreScope::whole_store(&crate::domains::account_domain("tester"))
            };
            state.store = Some(Store::new(
                SignedUser::new(Some("user-1".into()), Some("tester".into()), Some("t@test".into())),
                scope,
            ));
            state.authenticated_as = Some(if read_only_user { "auditor" } else { "tester" }.into());
            state.is_read_only_user = read_only_user;
            state.scope = read_only_user.then(|| crate::imap::auth::SessionScope {
                user_id: "viewer-id".into(),
                domain: "example.com".into(),
                role: crate::postgres::repositories::domains::DomainRole::Viewer,
                owner_id: "user-1".into(),
            });
            state.selected_mailbox = (!mailbox.is_empty()).then(|| mailbox.to_string());
            state.mailbox_read_only = examined || read_only_user;
        }
        fixture
    }

    #[tokio::test]
    async fn each_command_answers_its_own_text_before_sign_in() {
        let _budget = crate::imap::command_budget::test_lock().await;
        let fixture = fixture(false);
        for (line, answer) in [
            ("a LIST \"\" *", "a NO Not authenticated.\r\n"),
            ("a LSUB \"\" *", "a NO Not authenticated.\r\n"),
            ("a SELECT INBOX", "a NO Not authenticated.\r\n"),
            ("a EXAMINE INBOX", "a NO Not authenticated.\r\n"),
            ("a CREATE x", "a NO Not authenticated.\r\n"),
            ("a DELETE x", "a NO Not authenticated.\r\n"),
            ("a RENAME x y", "a NO Not authenticated.\r\n"),
            ("a SUBSCRIBE x", "a NO Not authenticated.\r\n"),
            ("a UNSUBSCRIBE x", "a NO Not authenticated.\r\n"),
            ("a STATUS INBOX (MESSAGES)", "a NO Not authenticated.\r\n"),
            ("a FETCH 1 FLAGS", "a NO Not authenticated.\r\n"),
            ("a UID FETCH 1 FLAGS", "a NO Not authenticated.\r\n"),
            ("a SEARCH ALL", "a NO Not authenticated.\r\n"),
            ("a STORE 1 +FLAGS (\\Seen)", "a NO Not authenticated.\r\n"),
            ("a EXPUNGE", "a NO Not authenticated.\r\n"),
            ("a CLOSE", "a NO Not authenticated.\r\n"),
            ("a UNSELECT", "a NO Not authenticated.\r\n"),
            ("a COPY 1 Trash", "a NO Not authenticated or no mailbox selected.\r\n"),
            ("a MOVE 1 Trash", "a NO Not authenticated or no mailbox selected.\r\n"),
            ("a UID MOVE 1 Trash", "a NO Not authenticated or no mailbox selected.\r\n"),
            ("a IDLE", "a NO Not authenticated or no mailbox selected\r\n"),
            ("a UID EXPUNGE 1", "a BAD UID EXPUNGE not supported\r\n"),
        ] {
            assert_eq!(run(&fixture, line).await, answer, "{line}");
        }
    }

    #[tokio::test]
    async fn the_message_commands_need_a_selected_mailbox() {
        let _budget = crate::imap::command_budget::test_lock().await;
        let fixture = selected("", false, false);
        for (line, answer) in [
            ("a FETCH 1 FLAGS", "a BAD No mailbox selected\r\n"),
            ("a SEARCH ALL", "a BAD No mailbox selected\r\n"),
            ("a STORE 1 +FLAGS (\\Seen)", "a BAD No mailbox selected\r\n"),
            ("a EXPUNGE", "a BAD No mailbox selected\r\n"),
            ("a CLOSE", "a BAD No mailbox selected\r\n"),
            ("a UNSELECT", "a BAD No mailbox selected\r\n"),
            ("a COPY 1 Trash", "a NO Not authenticated or no mailbox selected.\r\n"),
            ("a MOVE 1 Trash", "a NO Not authenticated or no mailbox selected.\r\n"),
            ("a IDLE", "a NO Not authenticated or no mailbox selected\r\n"),
        ] {
            assert_eq!(run(&fixture, line).await, answer, "{line}");
        }
    }

    #[tokio::test]
    async fn a_read_only_credential_is_refused_every_change() {
        let _budget = crate::imap::command_budget::test_lock().await;
        let fixture = selected("INBOX", true, false);
        let refusal = |command: &str| format!("a NO [READ-ONLY] {command} not permitted: auditor can only read example.com.\r\n");
        for (line, command) in [
            ("a CREATE x", "CREATE"),
            ("a DELETE x", "DELETE"),
            ("a RENAME x y", "RENAME"),
            ("a SUBSCRIBE x", "SUBSCRIBE"),
            ("a UNSUBSCRIBE x", "UNSUBSCRIBE"),
            ("a STORE 1 +FLAGS (\\Seen)", "STORE"),
            ("a UID STORE 1 +FLAGS (\\Seen)", "UID STORE"),
            ("a COPY 1 Trash", "COPY"),
            ("a UID COPY 1 Trash", "UID COPY"),
            ("a MOVE 1 Trash", "MOVE"),
            ("a UID MOVE 1 Trash", "UID MOVE"),
            ("a EXPUNGE", "EXPUNGE"),
        ] {
            assert_eq!(run(&fixture, line).await, refusal(command), "{line}");
        }
    }

    #[tokio::test]
    async fn a_login_scoped_to_a_domain_it_owns_cannot_manage_user_mailboxes() {
        let _budget = crate::imap::command_budget::test_lock().await;
        let fixture = selected("INBOX", false, false);
        {
            let mut state = fixture.session.state();
            let domains = vec!["example.org".to_string()];
            state.store = Some(Store::new(
                SignedUser::new(Some("user-1".into()), Some("tester".into()), Some("t@test".into())),
                crate::imap::store::StoreScope {
                    domains: Some(domains.clone()),
                    account_domains: domains,
                    primary_domain: "example.org".into(),
                },
            ));
        }
        for (line, command) in [("a CREATE x", "CREATE"), ("a DELETE x", "DELETE"), ("a RENAME x y", "RENAME")] {
            assert_eq!(
                run(&fixture, line).await,
                format!("a NO [CANNOT] {command} is not available to a login scoped to one domain.\r\n"),
                "{line}"
            );
        }
    }

    #[tokio::test]
    async fn an_examined_mailbox_refuses_store_expunge_and_move() {
        let _budget = crate::imap::command_budget::test_lock().await;
        let fixture = selected("INBOX", false, true);
        for line in ["a STORE 1 +FLAGS (\\Seen)", "a UID STORE 1 +FLAGS (\\Seen)", "a EXPUNGE", "a MOVE 1 Trash"] {
            assert_eq!(run(&fixture, line).await, "a NO [READ-ONLY] Mailbox is read-only\r\n", "{line}");
        }
    }

    #[tokio::test]
    async fn the_checks_made_before_the_store_answer_as_the_bun_server_does() {
        let _budget = crate::imap::command_budget::test_lock().await;
        let fixture = selected("INBOX", false, false);
        for (line, answer) in [
            ("a MOVE 1 inbox", "a OK MOVE completed\r\n"),
            ("a LIST \"Ref\" \"\"", "* LIST (\\Noselect) \"/\" \"Ref\"\r\na OK LIST completed\r\n"),
            ("a LSUB \"\" \"\"", "* LSUB (\\Noselect) \"/\" \"\"\r\na OK LSUB completed\r\n"),
            ("a SELECT \"\"", "a NO Empty mailbox name\r\n"),
            ("a SELECT INBOX/accounts", "a NO [CANNOT] INBOX/accounts is not selectable\r\n"),
            ("a EXAMINE \"Sent Messages/accounts\"", "a NO [CANNOT] Sent Messages/accounts is not selectable\r\n"),
            ("a CREATE inbox", "a NO [ALREADYEXISTS] Mailbox already exists\r\n"),
            ("a CREATE Trash", "a NO [ALREADYEXISTS] Mailbox already exists\r\n"),
            ("a CREATE \"\"", "a NO Empty mailbox name\r\n"),
            ("a DELETE INBOX", "a NO [CANNOT] Cannot delete INBOX\r\n"),
            ("a DELETE Junk", "a NO [CANNOT] Cannot delete system mailbox\r\n"),
            ("a RENAME INBOX x", "a NO [CANNOT] RENAME INBOX is not supported\r\n"),
            ("a RENAME Drafts x", "a NO [CANNOT] Cannot rename system mailbox\r\n"),
            ("a RENAME x Starred", "a NO [ALREADYEXISTS] Target mailbox already exists\r\n"),
            ("a SEARCH UID 1", "a NO Not supported\r\n"),
            ("a GETQUOTAROOT INBOX", "a NO Quota not supported\r\n"),
        ] {
            assert_eq!(run(&fixture, line).await, answer, "{line}");
        }
        let long = format!("a CREATE {}", "m".repeat(256));
        assert_eq!(run(&fixture, &long).await, "a NO [LIMIT] Mailbox name exceeds 255 bytes\r\n");
    }

    // A Bun oddity, kept: SELECT and EXAMINE set the read-only mode before
    // their checks, and a refused one leaves the mailbox selected before it
    // selected, now in the refused command's mode.
    #[tokio::test]
    async fn a_refused_examine_leaves_the_selected_mailbox_read_only() {
        let _budget = crate::imap::command_budget::test_lock().await;
        let fixture = selected("INBOX", false, false);
        assert_eq!(
            run(&fixture, "a EXAMINE INBOX/accounts").await,
            "a NO [CANNOT] INBOX/accounts is not selectable\r\n"
        );
        assert_eq!(fixture.session.selected_mailbox().as_deref(), Some("INBOX"));
        assert_eq!(run(&fixture, "a MOVE 1 Trash").await, "a NO [READ-ONLY] Mailbox is read-only\r\n");
    }
}
