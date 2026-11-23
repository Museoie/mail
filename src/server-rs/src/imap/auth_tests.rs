//! Tests of LOGIN and AUTHENTICATE PLAIN (auth.test.ts), against a fake
//! connection and a fake credential backend.
//!
//! The assertions are on the exact wire line: the status word and the
//! `[AUTHENTICATIONFAILED]` code are what clients branch on. The password
//! comparison runs for real (cost-4 digests keep it quick), and every call
//! into the backend is recorded, so a refusal that skipped the comparison
//! or forgot to count a failure shows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;
use crate::imap::test_support::{self, FakeAuthBackend as FakeBackend, FakeAuthCall as Call};
use crate::sso::{build_sso, ClientOverrides, SsoEnv};

const REMOTE_IP: &str = "203.0.113.7";
const CAPABILITIES: &str = "IMAP4rev1 AUTH=PLAIN";

/// A cost-4 digest of the empty string.
const EMPTY_PASSWORD_DIGEST: &str = "$2b$04$1ZJlW09Oj.28Pc9tQVIVleBOo11ISY6Ra1sWrn.NHENatHa9CdAz6";

#[derive(Default)]
struct FakeConnection {
    writes: Mutex<Vec<String>>,
    closed: AtomicBool,
    pending_sasl_tags: Mutex<Vec<String>>,
}

impl FakeConnection {
    fn writes(&self) -> Vec<String> {
        self.writes.lock().unwrap().clone()
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl AuthConnection for FakeConnection {
    fn remote_address(&self) -> String {
        REMOTE_IP.to_string()
    }

    fn remote_port(&self) -> u16 {
        45678
    }

    /// A write after the close never reaches the wire, so it is not recorded.
    fn write(&self, data: &str) -> bool {
        if self.closed() {
            return false;
        }
        self.writes.lock().unwrap().push(data.to_string());
        true
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn set_pending_sasl_tag(&self, tag: &str) {
        self.pending_sasl_tags.lock().unwrap().push(tag.to_string());
    }

    fn capabilities(&self) -> String {
        CAPABILITIES.to_string()
    }
}

/// A cost-4 digest: a real bcrypt digest without the cost-10 wait.
fn hash_of(password: &str) -> String {
    bcrypt::hash(password, 4).unwrap()
}

/// An account with a full identity, so it can be signed.
fn account(id: &str, username: &str, password_hash: &str) -> User {
    User {
        id: Some(id.into()),
        email: Some(format!("{username}@test.com")),
        username: Some(username.into()),
        password: Some(password_hash.into()),
        ..User::default()
    }
}

fn admin_with(password: &str) -> User {
    account("user-1", "admin", &hash_of(password))
}

fn plain(username: &str, password: &str) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("\0{username}\0{password}"))
}

fn b64(text: &str) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, text)
}

fn ip_and(account: &str) -> (String, Option<String>) {
    (REMOTE_IP.to_string(), Some(account.to_string()))
}

fn too_many(tag: &str) -> String {
    format!("{tag} NO [AUTHENTICATIONFAILED] Too many failed attempts\r\n")
}

fn invalid(tag: &str) -> String {
    format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials.\r\n")
}

/// The journal's audit line for `tag`.
fn audit_line(message: &str, tag: &str) -> Option<Value> {
    test_support::journal_entries()
        .into_iter()
        .find(|(level, logged, context)| level == "info" && logged == message && context["tag"] == tag)
        .map(|(_, _, context)| context)
}

async fn authenticate(
    tag: &str,
    mechanism: &str,
    response: Option<&str>,
    backend: &FakeBackend,
) -> (Option<AuthResult>, FakeConnection) {
    let connection = FakeConnection::default();
    let result = handle_authenticate(tag, mechanism, response, &connection, backend).await;
    (result, connection)
}

async fn login(tag: &str, args: &[&str], backend: &FakeBackend) -> (Result<Option<AuthResult>>, FakeConnection) {
    let connection = FakeConnection::default();
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let result = handle_login(tag, &args, &connection, backend).await;
    (result, connection)
}

// ---------------------------------------------------------------------------
// AUTHENTICATE
// ---------------------------------------------------------------------------

mod handle_authenticate {
    use super::*;

    #[tokio::test]
    async fn refuses_a_mechanism_other_than_plain_without_consulting_the_user_store() {
        let backend = FakeBackend::default();
        let (result, connection) = authenticate("A1", "LOGIN", Some("anything"), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), ["A1 NO Only PLAIN authentication supported\r\n"]);
        assert!(backend.calls().is_empty());
        assert!(!connection.closed());
        // Matched case-sensitively.
        let (_, lower) = authenticate("A1", "plain", Some("anything"), &backend).await;
        assert_eq!(lower.writes(), ["A1 NO Only PLAIN authentication supported\r\n"]);
    }

    #[tokio::test]
    async fn issues_an_empty_continuation_and_remembers_the_tag_when_no_initial_response_is_sent() {
        let backend = FakeBackend::default();
        let (result, connection) = authenticate("A2", "PLAIN", None, &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), ["+ \r\n"]);
        assert_eq!(*connection.pending_sasl_tags.lock().unwrap(), ["A2"]);
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn closes_the_connection_for_an_already_rate_limited_address() {
        let backend = FakeBackend { rate_limited: true, ..FakeBackend::default() };
        let (result, connection) = authenticate("A3", "PLAIN", Some(&plain("admin", "hunter2")), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), [too_many("A3")]);
        assert!(connection.closed());
        assert_eq!(backend.calls(), [Call::IsLimited(REMOTE_IP.into(), Some("admin".into()))]);
    }

    #[tokio::test]
    async fn rejects_a_plain_response_that_is_not_three_nul_separated_fields() {
        for payload in [b64("admin\0hunter2"), b64("\0admin\0hunter2\0extra"), b64("admin")] {
            let backend = FakeBackend::default();
            let (result, connection) = authenticate("A4", "PLAIN", Some(&payload), &backend).await;

            assert!(result.is_none());
            assert_eq!(connection.writes(), ["A4 BAD Invalid PLAIN response format\r\n"]);
            assert!(backend.calls().is_empty(), "not counted as a failure");
            assert!(!connection.closed());
        }
    }

    #[tokio::test]
    async fn treats_an_empty_initial_response_as_absent_rather_than_as_malformed_framing() {
        let backend = FakeBackend::default();
        let (result, connection) = authenticate("A4b", "PLAIN", Some(""), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), ["+ \r\n"]);
        assert_eq!(*connection.pending_sasl_tags.lock().unwrap(), ["A4b"]);
    }

    #[tokio::test]
    async fn refuses_an_unknown_user_with_authenticationfailed_and_records_the_failure() {
        let backend = FakeBackend::default();
        let (result, connection) = authenticate("A5", "PLAIN", Some(&plain("nobody", "hunter2")), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), [invalid("A5")]);
        let (ip, account) = ip_and("nobody");
        assert!(backend.called(|call| *call == Call::Record(ip.clone(), account.clone())));
        assert!(!backend.called(|call| matches!(call, Call::Reset(..))));
        assert!(!connection.closed());
    }

    #[tokio::test]
    async fn refuses_a_wrong_password_for_a_user_that_does_exist() {
        let backend = FakeBackend::with_users(vec![admin_with("correct-horse")]);
        let (result, connection) = authenticate("A6", "PLAIN", Some(&plain("admin", "wrong")), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), [invalid("A6")]);
        let (ip, account) = ip_and("admin");
        assert!(backend.called(|call| *call == Call::Record(ip.clone(), account.clone())));
    }

    #[tokio::test]
    async fn refuses_an_empty_password_even_when_the_stored_hash_would_match_it() {
        let backend = FakeBackend::with_users(vec![account("user-1", "admin", EMPTY_PASSWORD_DIGEST)]);
        let (result, connection) = authenticate("A7", "PLAIN", Some(&plain("admin", "")), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), [invalid("A7")]);
    }

    #[tokio::test]
    async fn closes_the_connection_on_the_failure_that_crosses_the_threshold() {
        let backend = FakeBackend { record_limits: true, ..FakeBackend::default() };
        let (result, connection) = authenticate("A8", "PLAIN", Some(&plain("nobody", "hunter2")), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), [too_many("A8")]);
        assert!(connection.closed());
    }

    #[tokio::test]
    async fn completes_with_a_capability_list_and_the_signed_user() {
        let admin = admin_with("correct-horse");
        let signed = admin.get_signed().unwrap();
        let backend = FakeBackend::with_users(vec![admin]);
        let (result, connection) =
            authenticate("A9", "PLAIN", Some(&plain("admin", "correct-horse")), &backend).await;

        let result = result.expect("signed in");
        assert!(!result.is_read_only);
        assert_eq!(result.authenticated_as, "admin");
        assert_eq!(result.user, signed);
        // A row that predates the revocation fence is generation 1.
        assert_eq!(result.password_version, 1);
        assert_eq!(connection.writes(), [format!("A9 OK [CAPABILITY {CAPABILITIES}] AUTHENTICATE completed\r\n")]);
        let (ip, account) = ip_and("admin");
        assert!(backend.called(|call| *call == Call::Reset(ip.clone(), account.clone())));
        assert!(!backend.called(|call| matches!(call, Call::Record(..))));
        assert!(!connection.closed());
    }

    #[tokio::test]
    async fn stamps_the_credential_rows_generation() {
        let mut admin = admin_with("correct-horse");
        admin.password_version = Some(7);
        let backend = FakeBackend::with_users(vec![admin]);
        let (result, _) = authenticate("A9v", "PLAIN", Some(&plain("admin", "correct-horse")), &backend).await;
        assert_eq!(result.unwrap().password_version, 7);
    }

    #[tokio::test]
    async fn logs_the_successful_authentication_at_info_with_the_account_and_remote_peer() {
        let backend = FakeBackend::with_users(vec![admin_with("correct-horse")]);
        authenticate("A10-audit", "PLAIN", Some(&plain("admin", "correct-horse")), &backend).await;

        assert_eq!(
            audit_line("IMAP AUTHENTICATE success", "A10-audit"),
            Some(json!({
                "component": "imap",
                "tag": "A10-audit",
                "authenticatedAs": "admin",
                "effectiveUsername": "admin",
                "isReadOnly": false,
                "remote": format!("{REMOTE_IP}:45678"),
                "mechanism": "PLAIN",
            }))
        );
    }

    #[tokio::test]
    async fn answers_bad_rather_than_propagating_when_the_user_lookup_fails() {
        let backend = FakeBackend { lookup_fails: true, ..FakeBackend::default() };
        let (result, connection) = authenticate("A11", "PLAIN", Some(&plain("admin", "hunter2")), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), ["A11 BAD AUTHENTICATE failed\r\n"]);
        assert!(!connection.closed());
    }

    #[test]
    fn decodes_base64_the_way_buffer_from_does() {
        // Characters outside the alphabet are skipped, the URL-safe alphabet
        // is accepted, `=` ends the data, a partial group keeps its octets.
        // The expected octets are what Bun's `Buffer.from(text, "base64")`
        // gives.
        assert_eq!(decode_base64_lenient("AGFsaWNlAHB3"), b"\0alice\0pw");
        assert_eq!(decode_base64_lenient("YW Jj\nZGVm"), b"abcdef");
        assert_eq!(decode_base64_lenient("YWJj*ZGVm"), b"abcdef");
        assert_eq!(decode_base64_lenient("YWJjZA"), b"abcd");
        assert_eq!(decode_base64_lenient("YWJjZA==ZGVm"), b"abcd");
        assert_eq!(decode_base64_lenient("YWJj-_8"), [b'a', b'b', b'c', 0xfb, 0xff]);
        assert_eq!(decode_base64_lenient("Y"), b"");
        assert_eq!(decode_base64_lenient("!!!notbase64!!!"), [158, 139, 91, 106, 199, 186]);
    }
}

// ---------------------------------------------------------------------------
// LOGIN
// ---------------------------------------------------------------------------

mod handle_login {
    use super::*;

    #[tokio::test]
    async fn rejects_a_login_missing_its_password_argument() {
        let backend = FakeBackend::default();
        let (result, connection) = login("B1", &["admin"], &backend).await;

        assert!(result.unwrap().is_none());
        assert_eq!(connection.writes(), ["B1 BAD LOGIN requires username and password\r\n"]);
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn closes_the_connection_for_an_already_rate_limited_address_before_reading_the_credentials() {
        let backend = FakeBackend { rate_limited: true, ..FakeBackend::default() };
        let (result, connection) = login("B2", &["admin", "hunter2"], &backend).await;

        assert!(result.unwrap().is_none());
        assert_eq!(connection.writes(), [too_many("B2")]);
        assert!(connection.closed());
        assert_eq!(backend.calls(), [Call::IsLimited(REMOTE_IP.into(), Some("admin".into()))]);
    }

    #[tokio::test]
    async fn hands_the_lookup_its_arguments_byte_identical_interior_quote_characters_included() {
        // What `a1 LOGIN "ad\"min" "corr\"ect-horse"` delivers once parsed.
        let (username, password) = ("ad\"min", "corr\"ect-horse");
        let backend = FakeBackend::with_users(vec![account("user-1", username, &hash_of(password))]);
        let (result, connection) = login("B3", &[username, password], &backend).await;

        assert!(backend.called(|call| *call == Call::GetUser(username.into())));
        let compared = backend.compared();
        assert_eq!(compared.len(), 1);
        assert_eq!(compared[0].0, password);
        assert!(result.unwrap().is_some());
        assert_eq!(connection.writes(), [format!("B3 OK [CAPABILITY {CAPABILITIES}] LOGIN completed\r\n")]);
    }

    #[tokio::test]
    async fn looks_up_and_compares_one_and_the_same_value_when_a_decoded_credential_is_itself_quoted() {
        let backend = FakeBackend::default();
        let _ = login("B4", &["admin", "\"hunter2\""], &backend).await;

        assert!(backend.called(|call| *call == Call::GetUser("admin".into())));
        // One pair of surrounding quotes comes off, as the Bun server's
        // `replace(/^"(.*)"$/, "$1")` takes it off.
        let compared = backend.compared();
        assert_eq!(compared.len(), 1);
        assert_eq!(compared[0].0, "hunter2");
    }

    #[tokio::test]
    async fn refuses_an_unknown_user_with_authenticationfailed_and_records_the_failure() {
        let backend = FakeBackend::default();
        let (result, connection) = login("B5", &["nobody", "hunter2"], &backend).await;

        assert!(result.unwrap().is_none());
        assert_eq!(connection.writes(), [invalid("B5")]);
        let (ip, account) = ip_and("nobody");
        assert!(backend.called(|call| *call == Call::Record(ip.clone(), account.clone())));
        assert!(!backend.called(|call| matches!(call, Call::Reset(..))));
        assert!(!connection.closed());
    }

    #[tokio::test]
    async fn refuses_a_wrong_password_for_a_user_that_does_exist() {
        let backend = FakeBackend::with_users(vec![admin_with("correct-horse")]);
        let (result, connection) = login("B10", &["admin", "wrong"], &backend).await;

        assert!(result.unwrap().is_none());
        assert_eq!(connection.writes(), [invalid("B10")]);
        let (ip, account) = ip_and("admin");
        assert!(backend.called(|call| *call == Call::Record(ip.clone(), account.clone())));
        assert!(!backend.called(|call| matches!(call, Call::Reset(..))));
    }

    #[tokio::test]
    async fn refuses_an_empty_password_even_when_the_stored_hash_would_match_it() {
        let backend = FakeBackend::with_users(vec![account("user-1", "admin", EMPTY_PASSWORD_DIGEST)]);
        let (result, connection) = login("B11", &["admin", ""], &backend).await;

        assert!(result.unwrap().is_none());
        assert_eq!(connection.writes(), [invalid("B11")]);
        let (ip, account) = ip_and("admin");
        assert!(backend.called(|call| *call == Call::Record(ip.clone(), account.clone())));
    }

    #[tokio::test]
    async fn refuses_an_account_with_no_signable_identity() {
        // No e-mail: the row matches the password and still has no identity
        // to bind a session to.
        let mut admin = admin_with("correct-horse");
        admin.email = None;
        let backend = FakeBackend::with_users(vec![admin]);
        let (result, connection) = login("B6", &["admin", "correct-horse"], &backend).await;

        assert!(result.unwrap().is_none());
        assert_eq!(connection.writes(), [invalid("B6")]);
    }

    #[tokio::test]
    async fn closes_the_connection_on_the_failure_that_crosses_the_threshold() {
        let backend = FakeBackend { record_limits: true, ..FakeBackend::default() };
        let (result, connection) = login("B7", &["nobody", "hunter2"], &backend).await;

        assert!(result.unwrap().is_none());
        assert_eq!(connection.writes(), [too_many("B7")]);
        assert!(connection.closed());
    }

    #[tokio::test]
    async fn propagates_a_failing_user_lookup_instead_of_answering_bad_unlike_authenticate() {
        let backend = FakeBackend { lookup_fails: true, ..FakeBackend::default() };
        let (result, connection) = login("B9", &["admin", "hunter2"], &backend).await;

        assert_eq!(result.unwrap_err().to_string(), "connection terminated");
        assert!(connection.writes().is_empty());
    }

    #[tokio::test]
    async fn completes_with_a_capability_list_the_signed_user_and_a_reset_failure_counter() {
        let admin = admin_with("correct-horse");
        let signed = admin.get_signed().unwrap();
        let backend = FakeBackend::with_users(vec![admin]);
        let (result, connection) = login("B8-audit", &["admin", "correct-horse"], &backend).await;

        let result = result.unwrap().expect("signed in");
        assert!(!result.is_read_only);
        assert_eq!(result.authenticated_as, "admin");
        assert_eq!(result.user, signed);
        assert_eq!(connection.writes(), [format!("B8-audit OK [CAPABILITY {CAPABILITIES}] LOGIN completed\r\n")]);
        let (ip, account) = ip_and("admin");
        assert!(backend.called(|call| *call == Call::Reset(ip.clone(), account.clone())));
        assert!(!backend.called(|call| matches!(call, Call::Record(..))));
        assert!(!connection.closed());
        assert_eq!(
            audit_line("IMAP LOGIN success", "B8-audit"),
            Some(json!({
                "component": "imap",
                "tag": "B8-audit",
                "authenticatedAs": "admin",
                "effectiveUsername": "admin",
                "isReadOnly": false,
                "remote": format!("{REMOTE_IP}:45678"),
            }))
        );
    }

    #[test]
    fn strips_one_pair_of_surrounding_quotes() {
        assert_eq!(strip_surrounding_quotes("\"hunter2\""), "hunter2");
        assert_eq!(strip_surrounding_quotes("\"\"x\"\""), "\"x\"");
        assert_eq!(strip_surrounding_quotes("ad\"min"), "ad\"min");
        assert_eq!(strip_surrounding_quotes("\""), "\"");
        assert_eq!(strip_surrounding_quotes("\"\""), "");
        // `.` does not cross a line terminator.
        assert_eq!(strip_surrounding_quotes("\"a\nb\""), "\"a\nb\"");
    }
}

// ---------------------------------------------------------------------------
// Username enumeration resistance
// ---------------------------------------------------------------------------

mod username_enumeration_resistance {
    use super::*;

    #[tokio::test]
    async fn runs_the_comparison_when_authenticate_names_no_known_user() {
        let backend = FakeBackend::default();
        authenticate("C1", "PLAIN", Some(&plain("nobody", "hunter2")), &backend).await;
        // With no digest the comparison runs against the stand-in, a real
        // digest of the app's cost (verify_password.rs).
        assert_eq!(backend.compared(), [("hunter2".to_string(), None)]);
    }

    #[tokio::test]
    async fn runs_the_comparison_when_login_names_no_known_user() {
        let backend = FakeBackend::default();
        let _ = login("C2", &["nobody", "hunter2"], &backend).await;
        assert_eq!(backend.compared(), [("hunter2".to_string(), None)]);
    }

    #[tokio::test]
    async fn spends_the_same_number_of_comparisons_on_an_unknown_user_as_on_a_wrong_password() {
        let unknown = FakeBackend::default();
        let (_, unknown_connection) = authenticate("C3", "PLAIN", Some(&plain("nobody", "hunter2")), &unknown).await;
        let known = FakeBackend::with_users(vec![admin_with("correct-horse")]);
        let (_, known_connection) = authenticate("C4", "PLAIN", Some(&plain("admin", "hunter2")), &known).await;

        assert_eq!(unknown.compared().len(), 1);
        assert_eq!(known.compared().len(), 1);
        let known_writes: Vec<String> = known_connection.writes().iter().map(|line| line.replace("C4", "C3")).collect();
        assert_eq!(unknown_connection.writes(), known_writes);
    }

    #[tokio::test]
    async fn does_not_reuse_a_real_accounts_digest_for_the_unknown_user_comparison() {
        let known = FakeBackend::with_users(vec![admin_with("correct-horse")]);
        let _ = login("C5", &["admin", "hunter2"], &known).await;
        let real_digest = known.compared()[0].1.clone();
        assert!(real_digest.is_some());

        let unknown = FakeBackend::default();
        let _ = login("C6", &["nobody", "hunter2"], &unknown).await;
        assert_ne!(unknown.compared()[0].1, real_digest);
    }
}

// ---------------------------------------------------------------------------
// Domain-scoped login
// ---------------------------------------------------------------------------

mod domain_scoped_login {
    use super::*;
    use crate::postgres::repositories::domains::{DomainAccess, DomainRole};

    const PASSWORD: &str = "auditor-secret";

    fn access(role: DomainRole, name: &str) -> DomainAccess {
        let (owner_id, owner_username) =
            if role == DomainRole::Owner { ("user-auditor", "auditor") } else { ("user-owner", "owner") };
        DomainAccess {
            domain_id: "domain-1".into(),
            name: name.into(),
            owner_id: owner_id.into(),
            owner_username: owner_username.into(),
            role,
            verified_at: None,
        }
    }

    /// The person (with a password) and the owner of the domains they view.
    fn backend(roles: Vec<DomainAccess>) -> FakeBackend {
        let users = vec![account("user-auditor", "auditor", &hash_of(PASSWORD)), account("user-owner", "owner", "unused")];
        FakeBackend {
            domains: roles.into_iter().map(|access| ("user-auditor".to_string(), access)).collect(),
            ..FakeBackend::with_users(users)
        }
    }

    #[tokio::test]
    async fn opens_the_owners_store_read_only_for_a_domain_the_person_views() {
        let backend = backend(vec![access(DomainRole::Viewer, "example.com")]);
        let (result, connection) =
            authenticate("E1", "PLAIN", Some(&plain("auditor@example.com", PASSWORD)), &backend).await;

        let result = result.expect("signed in");
        assert!(result.is_read_only);
        // The credential row, and the rate-limit bucket, are the person's.
        assert_eq!(result.authenticated_as, "auditor");
        assert!(backend.called(|call| *call == Call::DomainAccess("user-auditor".into(), "example.com".into())));
        assert_eq!(result.user.id, "user-owner");
        assert_eq!(result.store_scope.domains, Some(vec!["example.com".to_string()]));
        assert_eq!(
            result.scope,
            Some(SessionScope {
                user_id: "user-auditor".into(),
                domain: "example.com".into(),
                role: DomainRole::Viewer,
                owner_id: "user-owner".into(),
            })
        );
        let (ip, account) = ip_and("auditor");
        assert!(backend.called(|call| *call == Call::Reset(ip.clone(), account.clone())));
        assert_eq!(connection.writes(), [format!("E1 OK [CAPABILITY {CAPABILITIES}] AUTHENTICATE completed\r\n")]);
    }

    #[tokio::test]
    async fn attributes_the_audit_line_to_the_person_and_names_the_domain() {
        let backend = backend(vec![access(DomainRole::Viewer, "example.com")]);
        authenticate("E2-audit", "PLAIN", Some(&plain("auditor@example.com", PASSWORD)), &backend).await;

        assert_eq!(
            audit_line("IMAP AUTHENTICATE success", "E2-audit"),
            Some(json!({
                "component": "imap",
                "tag": "E2-audit",
                "authenticatedAs": "auditor",
                "scopeDomain": "example.com",
                "effectiveUsername": "owner",
                "isReadOnly": true,
                "remote": format!("{REMOTE_IP}:45678"),
                "mechanism": "PLAIN",
            }))
        );
    }

    #[tokio::test]
    async fn opens_the_persons_own_store_filtered_to_the_domain_for_a_domain_they_own() {
        let backend = backend(vec![access(DomainRole::Owner, "example.com")]);
        let (result, _) = authenticate("E3", "PLAIN", Some(&plain("auditor@example.com", PASSWORD)), &backend).await;

        let result = result.expect("signed in");
        assert!(!result.is_read_only);
        assert_eq!(result.user.id, "user-auditor");
        assert_eq!(result.store_scope.domains, Some(vec!["example.com".to_string()]));
        assert_eq!(result.store_scope.primary_domain, "example.com");
    }

    #[tokio::test]
    async fn refuses_a_domain_the_person_holds_no_role_on_like_a_wrong_password() {
        let backend = backend(vec![]);
        let (result, connection) =
            authenticate("E4", "PLAIN", Some(&plain("auditor@example.com", PASSWORD)), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), [invalid("E4")]);
        let (ip, account) = ip_and("auditor");
        assert!(backend.called(|call| *call == Call::Record(ip.clone(), account.clone())));
        assert!(!backend.called(|call| matches!(call, Call::Reset(..))));
    }

    #[tokio::test]
    async fn closes_the_connection_when_that_refusal_crosses_the_threshold() {
        let backend = FakeBackend { record_limits: true, ..backend(vec![]) };
        let (result, connection) =
            authenticate("E5", "PLAIN", Some(&plain("auditor@example.com", PASSWORD)), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), [too_many("E5")]);
        assert!(connection.closed());
    }

    #[tokio::test]
    async fn never_reads_the_role_when_the_password_is_wrong() {
        let backend = backend(vec![access(DomainRole::Viewer, "example.com")]);
        let (result, _) = authenticate("E6", "PLAIN", Some(&plain("auditor@example.com", "wrong")), &backend).await;

        assert!(result.is_none());
        assert!(!backend.called(|call| matches!(call, Call::DomainAccess(..))));
    }

    #[tokio::test]
    async fn reads_the_whole_store_of_a_plain_login_listing_its_primary_domain() {
        let own = crate::domains::account_domain("auditor");
        let backend = backend(vec![
            access(DomainRole::Owner, &own),
            access(DomainRole::Owner, "example.org"),
            access(DomainRole::Viewer, "example.net"),
        ]);
        let (result, _) = authenticate("E7", "PLAIN", Some(&plain("auditor", PASSWORD)), &backend).await;

        let result = result.expect("signed in");
        assert!(!result.is_read_only);
        assert_eq!(result.scope, None);
        assert_eq!(result.user.id, "user-auditor");
        assert!(!backend.called(|call| matches!(call, Call::DomainAccess(..))));
        // Account folders are read back on the primary domain, so only its
        // addresses are listed.
        assert_eq!(result.store_scope, crate::imap::store::StoreScope::whole_store(&own));
    }

    #[tokio::test]
    async fn applies_the_same_scoping_to_login() {
        let backend = backend(vec![access(DomainRole::Viewer, "example.com")]);
        let (result, connection) = login("E8", &["auditor@example.com", PASSWORD], &backend).await;

        let result = result.unwrap().expect("signed in");
        assert!(result.is_read_only);
        assert_eq!(result.authenticated_as, "auditor");
        assert_eq!(result.user.id, "user-owner");
        assert_eq!(connection.writes(), [format!("E8 OK [CAPABILITY {CAPABILITIES}] LOGIN completed\r\n")]);
    }
}

// ---------------------------------------------------------------------------
// Single sign-on
// ---------------------------------------------------------------------------

mod single_sign_on {
    use super::*;

    const APP_PASSWORD: &str = "abcd-efgh-ijkl-mnop";

    /// Settings only: the provider is never contacted.
    fn sso(local_login: bool) -> Arc<Sso> {
        let env = SsoEnv {
            auth_issuer: Some("https://auth.example.test".into()),
            auth_client_id: Some("mail".into()),
            auth_client_secret: Some("test_key_not_real_do_not_use".into()),
            app_hostname: Some("mail.example.test".into()),
            auth_local_login: Some(local_login.to_string()),
            ..SsoEnv::default()
        };
        Arc::new(build_sso(&env, ClientOverrides::default()).unwrap().unwrap())
    }

    fn alice() -> SignedUser {
        let mut alice = account("user-alice", "alice", "unused").get_signed().unwrap();
        alice.password_version = Some(4);
        alice
    }

    fn accepted() -> Option<AppPasswordCheck> {
        Some(AppPasswordCheck::Accepted { user: alice(), password_version: 4 })
    }

    fn app_password_call() -> Call {
        Call::AppPassword("alice".into(), APP_PASSWORD.into(), REMOTE_IP.into())
    }

    #[tokio::test]
    async fn never_asks_the_provider_while_it_is_off() {
        let backend = FakeBackend { app_password: accepted(), ..FakeBackend::default() };
        authenticate("S0", "PLAIN", Some(&plain("alice", APP_PASSWORD)), &backend).await;
        assert!(!backend.called(|call| matches!(call, Call::AppPassword(..))));
        assert!(backend.called(|call| matches!(call, Call::GetUser(..))));
    }

    #[tokio::test]
    async fn signs_in_with_an_app_password_bound_to_the_linked_account_and_its_generation() {
        let backend = FakeBackend { sso: Some(sso(false)), app_password: accepted(), ..FakeBackend::default() };
        let (result, connection) = authenticate("S1", "PLAIN", Some(&plain("alice", APP_PASSWORD)), &backend).await;

        let result = result.expect("signed in");
        assert_eq!(result.user, alice());
        assert!(!result.is_read_only);
        assert_eq!(result.authenticated_as, "alice");
        assert_eq!(result.password_version, 4);
        assert_eq!(connection.writes(), [format!("S1 OK [CAPABILITY {CAPABILITIES}] AUTHENTICATE completed\r\n")]);
        assert_eq!(
            backend.calls(),
            [
                Call::IsLimited(REMOTE_IP.into(), Some("alice".into())),
                app_password_call(),
                Call::Reset(REMOTE_IP.into(), Some("alice".into())),
            ]
        );
    }

    #[tokio::test]
    async fn signs_in_with_an_app_password_over_login_too() {
        let backend = FakeBackend { sso: Some(sso(false)), app_password: accepted(), ..FakeBackend::default() };
        let (result, connection) = login("S2", &["alice", APP_PASSWORD], &backend).await;

        let result = result.unwrap().expect("signed in");
        assert_eq!(result.user, alice());
        assert_eq!(result.password_version, 4);
        assert_eq!(connection.writes(), [format!("S2 OK [CAPABILITY {CAPABILITIES}] LOGIN completed\r\n")]);
    }

    #[tokio::test]
    async fn answers_unavailable_without_counting_a_failure_while_the_provider_cannot_answer() {
        let backend = FakeBackend {
            sso: Some(sso(false)),
            app_password: Some(AppPasswordCheck::Unavailable),
            ..FakeBackend::default()
        };
        let (result, connection) = authenticate("S3", "PLAIN", Some(&plain("alice", APP_PASSWORD)), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), ["S3 NO [UNAVAILABLE] Authentication is temporarily unavailable\r\n"]);
        assert!(!backend.called(|call| matches!(call, Call::Record(..) | Call::GetUser(..))));
        assert!(!connection.closed());
    }

    #[tokio::test]
    async fn refuses_anything_else_as_a_failed_attempt_while_local_passwords_are_off() {
        let backend = FakeBackend { sso: Some(sso(false)), ..FakeBackend::default() };
        let (result, connection) =
            authenticate("S4", "PLAIN", Some(&plain("alice", "a-local-password")), &backend).await;

        assert!(result.is_none());
        assert_eq!(connection.writes(), [invalid("S4")]);
        let (ip, account) = ip_and("alice");
        assert!(backend.called(|call| *call == Call::Record(ip.clone(), account.clone())));
        assert!(!backend.called(|call| matches!(call, Call::GetUser(..) | Call::Verify(..))));
    }

    #[tokio::test]
    async fn still_takes_a_local_password_while_local_login_keeps_them() {
        let admin = admin_with("correct-horse");
        let signed = admin.get_signed().unwrap();
        let backend = FakeBackend { sso: Some(sso(true)), app_password: accepted(), ..FakeBackend::with_users(vec![admin]) };
        let (result, connection) =
            authenticate("S5", "PLAIN", Some(&plain("admin", "correct-horse")), &backend).await;

        // A local password that matches is never sent to the provider.
        assert!(!backend.called(|call| matches!(call, Call::AppPassword(..))));
        assert_eq!(result.unwrap().user, signed);
        assert_eq!(connection.writes(), [format!("S5 OK [CAPABILITY {CAPABILITIES}] AUTHENTICATE completed\r\n")]);
    }

    #[tokio::test]
    async fn checks_an_app_password_after_the_local_one_while_local_login_keeps_them() {
        let backend = FakeBackend {
            sso: Some(sso(true)),
            app_password: accepted(),
            ..FakeBackend::with_users(vec![account("user-alice", "alice", &hash_of("correct-horse"))])
        };
        let (result, connection) = authenticate("S6", "PLAIN", Some(&plain("alice", APP_PASSWORD)), &backend).await;

        assert!(backend.called(|call| matches!(call, Call::GetUser(..))));
        assert_eq!(
            backend.calls().into_iter().filter(|call| matches!(call, Call::AppPassword(..))).collect::<Vec<_>>(),
            [app_password_call()]
        );
        assert_eq!(result.unwrap().user, alice());
        assert_eq!(connection.writes(), [format!("S6 OK [CAPABILITY {CAPABILITIES}] AUTHENTICATE completed\r\n")]);
    }

    #[tokio::test]
    async fn checks_the_app_password_for_the_person_and_scopes_it_like_a_local_password() {
        use crate::postgres::repositories::domains::{DomainAccess, DomainRole};
        let owner = account("user-owner", "owner", "unused");
        let viewer = DomainAccess {
            domain_id: "domain-1".into(),
            name: "example.com".into(),
            owner_id: "user-owner".into(),
            owner_username: "owner".into(),
            role: DomainRole::Viewer,
            verified_at: None,
        };
        let backend = FakeBackend {
            sso: Some(sso(false)),
            app_password: accepted(),
            domains: vec![("user-alice".into(), viewer)],
            ..FakeBackend::with_users(vec![owner])
        };
        let (result, _) =
            authenticate("S7", "PLAIN", Some(&plain("alice@example.com", APP_PASSWORD)), &backend).await;

        // The provider is asked about the person, without the domain.
        assert!(backend.called(|call| matches!(call, Call::AppPassword(username, ..) if username == "alice")));
        assert!(backend.called(|call| *call == Call::DomainAccess("user-alice".into(), "example.com".into())));
        let result = result.expect("signed in");
        assert!(result.is_read_only);
        assert_eq!(result.user.id, "user-owner");
        assert_eq!(result.password_version, 4);
    }
}
