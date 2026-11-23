//! The hook cases of `smtp.test.ts` (`onAuth`, `onMailFrom`, `onRcptTo`, the
//! relay decision of `onData`), against a stand-in directory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;

use super::data_stream::data_stream;
use super::handler::{choose_lane, CredentialStamp, DataLane, DomainScope, MailDirectory, MailHandler};
use super::types::{
    SmtpAddress, SmtpAuthMethod, SmtpAuthRequest, SmtpAuthResponse, SmtpEnvelope, SmtpHandler, SmtpHookError,
    SmtpSession, SmtpSessionState,
};
use crate::auth_rate_limit::{
    is_auth_rate_limited, is_recipient_probe_rate_limited, record_auth_failure, record_recipient_probe,
};
use crate::domains::{domain_of_address, DeliveryDomain};
use crate::models::{SignedUser, User};
use crate::postgres::repositories::domains::DomainRole;
use crate::sso::app_password::AppPasswordCheck;
use crate::sso::{build_sso, ClientOverrides, Sso, SsoEnv};
use crate::verify_password::hash_password;

const PASSWORD: &str = "correct horse battery staple";
const APP_PASSWORD: &str = "abcd-efgh-ijkl-mnop";
const INVALID: SmtpAuthResponse = SmtpAuthResponse { user: None, message: None, response_code: None };

fn lookup_failed() -> SmtpHookError {
    SmtpHookError::with_code(451, "4.3.0 Error: temporary lookup failure")
}

/// One bcrypt digest of [`PASSWORD`], made once.
async fn digest() -> String {
    static DIGEST: OnceLock<String> = OnceLock::new();
    if let Some(digest) = DIGEST.get() {
        return digest.clone();
    }
    let digest = hash_password(PASSWORD).await;
    DIGEST.get_or_init(|| digest).clone()
}

/// A distinct client address per test: the rate limits are process-wide.
fn fresh_ip() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("198.18.{}.{}", n / 250, n % 250 + 1)
}

async fn account(username: &str, password_version: Option<i64>) -> User {
    User {
        id: Some(format!("id-{username}")),
        email: Some(format!("{username}@elsewhere.example")),
        username: Some(username.to_string()),
        password: Some(digest().await),
        token: None,
        expiry: None,
        password_version,
    }
}

#[derive(Default)]
struct Directory {
    /// By username; a name not here has no account, `Err` fails the lookup.
    users: Mutex<HashMap<String, Result<User, String>>>,
    lookups: Mutex<Vec<String>>,
    sso: Option<Arc<Sso>>,
    app_password: Mutex<Option<Result<AppPasswordCheck, String>>>,
    app_password_calls: Mutex<Vec<(String, String, String)>>,
    email_domain: Option<String>,
    /// Domain rows: the name and its owner's user id.
    domains: Mutex<HashMap<String, String>>,
    /// Roles on domains: (user id, domain, role).
    roles: Vec<(String, String, DomainRole)>,
}

impl Directory {
    fn serving(domain: &str) -> Directory {
        Directory { email_domain: Some(domain.to_string()), ..Directory::default() }
    }

    fn with_user(self, user: User) -> Directory {
        self.users.lock().unwrap().insert(user.username.clone().unwrap(), Ok(user));
        self
    }

    /// A domain row owned by `owner_id`.
    fn with_domain(self, domain: &str, owner_id: &str) -> Directory {
        self.domains.lock().unwrap().insert(domain.to_string(), owner_id.to_string());
        self
    }

    fn with_role(mut self, user_id: &str, domain: &str, role: DomainRole) -> Directory {
        self.roles.push((user_id.to_string(), domain.to_string(), role));
        self
    }

    fn failing(self, username: &str) -> Directory {
        self.users.lock().unwrap().insert(username.to_string(), Err("connection refused".into()));
        self
    }

    fn with_sso(self, local_login: bool) -> Directory {
        let env = SsoEnv {
            auth_issuer: Some("https://auth.example.test".into()),
            auth_client_id: Some("mail".into()),
            auth_client_secret: Some("test_key_not_real_do_not_use".into()),
            app_hostname: Some("mail.example.test".into()),
            auth_local_login: Some(local_login.to_string()),
            ..SsoEnv::default()
        };
        let sso = build_sso(&env, ClientOverrides::default()).expect("valid settings").expect("on");
        Directory { sso: Some(Arc::new(sso)), ..self }
    }

    fn answering(self, check: Result<AppPasswordCheck, String>) -> Directory {
        *self.app_password.lock().unwrap() = Some(check);
        self
    }

    fn lookups(&self) -> Vec<String> {
        self.lookups.lock().unwrap().clone()
    }

    fn app_password_calls(&self) -> usize {
        self.app_password_calls.lock().unwrap().len()
    }
}

impl MailDirectory for Arc<Directory> {
    async fn get_user(&self, username: &str) -> Result<Option<User>> {
        self.lookups.lock().unwrap().push(username.to_string());
        match self.users.lock().unwrap().get(username) {
            None => Ok(None),
            Some(Ok(user)) => Ok(Some(user.clone())),
            Some(Err(message)) => Err(anyhow::anyhow!(message.clone())),
        }
    }

    fn sso(&self) -> Option<Arc<Sso>> {
        self.sso.clone()
    }

    async fn check_app_password(
        &self,
        _sso: &Sso,
        username: &str,
        password: &str,
        remote_address: &str,
    ) -> Result<AppPasswordCheck> {
        let call = (username.to_string(), password.to_string(), remote_address.to_string());
        self.app_password_calls.lock().unwrap().push(call);
        match self.app_password.lock().unwrap().clone() {
            None => Ok(AppPasswordCheck::Rejected),
            Some(Ok(check)) => Ok(check),
            Some(Err(message)) => Err(anyhow::anyhow!(message)),
        }
    }

    fn email_domain(&self) -> Option<String> {
        self.email_domain.clone()
    }

    async fn domain_role(&self, user_id: &str, domain: &str) -> Result<Option<(DomainRole, String)>> {
        Ok(self
            .roles
            .iter()
            .find(|(holder, name, _)| holder == user_id && name == domain)
            .map(|(_, name, role)| (*role, name.clone())))
    }

    /// As `resolveDeliveryDomain`: a domain row decides, and an account's own
    /// name under the served zone is looked up by its label.
    async fn resolve_delivery(&self, address: &str) -> Result<Option<DeliveryDomain>> {
        let domain = domain_of_address(address);
        if let Some(owner_id) = self.domains.lock().unwrap().get(&domain) {
            return Ok(Some(DeliveryDomain { domain: domain.clone(), owner_id: owner_id.clone() }));
        }
        let Some(zone) = &self.email_domain else { return Ok(None) };
        let Some(label) = domain.strip_suffix(&format!(".{}", zone.to_lowercase())) else { return Ok(None) };
        Ok(self.get_user(label).await?.and_then(|user| user.id).map(|owner_id| DeliveryDomain { domain, owner_id }))
    }

    async fn has_domain(&self, domain: &str) -> Result<bool> {
        Ok(self.domains.lock().unwrap().contains_key(domain))
    }

    async fn owned_domains(&self, user_id: &str) -> Result<Vec<String>> {
        Ok(self
            .domains
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, owner)| owner.as_str() == user_id)
            .map(|(name, _)| name.clone())
            .collect())
    }
}

fn handler(directory: Directory) -> (MailHandler<Arc<Directory>>, Arc<Directory>) {
    let directory = Arc::new(directory);
    (MailHandler::with_directory(directory.clone()), directory)
}

fn session(ip: &str) -> SmtpSession {
    SmtpSession {
        id: "0123456789abcdef0123".into(),
        remote_address: ip.to_string(),
        remote_port: 40000,
        local_address: "127.0.0.1".into(),
        local_port: 465,
        secure: true,
        user: None,
        host_name_appears_as: Some("client.example".into()),
        envelope: SmtpEnvelope::default(),
        transaction: 1,
        state: SmtpSessionState::default(),
    }
}

fn signed_in(ip: &str, username: &str, password_version: Option<i64>) -> SmtpSession {
    let mut session = session(ip);
    session.user = Some(username.to_string());
    if let Some(password_version) = password_version {
        session.state.insert(CredentialStamp { password_version });
    }
    session
}

fn credentials(username: &str, password: &str) -> SmtpAuthRequest {
    SmtpAuthRequest { method: SmtpAuthMethod::Plain, username: username.into(), password: password.into() }
}

fn address(address: &str) -> SmtpAddress {
    SmtpAddress { address: address.to_string(), args: None }
}

fn stamp(session: &SmtpSession) -> Option<i64> {
    session.state.get::<CredentialStamp>().map(|stamp| stamp.password_version)
}

fn accepted(username: &str, password_version: i64) -> AppPasswordCheck {
    let user = SignedUser::new(Some(format!("id-{username}")), Some(username.into()), None);
    AppPasswordCheck::Accepted { user, password_version }
}

// --- AUTH -------------------------------------------------------------------

#[tokio::test]
async fn returns_the_existing_session_user_without_re_authenticating() {
    let (handler, directory) = handler(Directory::serving("test.com"));
    let mut session = signed_in(&fresh_ip(), "existing-user", None);
    let answer = handler.on_auth(credentials("new-user", "password"), &mut session).await;
    assert_eq!(answer, Ok(SmtpAuthResponse::accept("existing-user")));
    assert!(directory.lookups().is_empty());
}

#[tokio::test(start_paused = true)]
async fn refuses_an_unknown_user_and_counts_the_failure() {
    let (handler, _) = handler(Directory::serving("test.com"));
    let ip = fresh_ip();
    let answer = handler.on_auth(credentials("nonexistent", "password"), &mut session(&ip)).await;
    assert_eq!(answer, Ok(INVALID));
    for _ in 0..9 {
        handler.on_auth(credentials("nonexistent", "password"), &mut session(&ip)).await.unwrap();
    }
    assert!(is_auth_rate_limited(&ip, None));
}

#[tokio::test]
async fn answers_a_transient_refusal_when_the_credential_lookup_fails() {
    let (handler, _) = handler(Directory::serving("test.com").failing("pending"));
    let ip = fresh_ip();
    let started = tokio::time::Instant::now();
    for _ in 0..12 {
        let answer = handler.on_auth(credentials("pending", "anything"), &mut session(&ip)).await;
        assert_eq!(answer, Err(lookup_failed()));
    }
    // No failure counted, and no half-second delay.
    assert!(!is_auth_rate_limited(&ip, Some("pending")));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test(start_paused = true)]
async fn refuses_an_empty_or_wrong_password_and_another_letter_case() {
    let (handler, _) = handler(Directory::serving("test.com").with_user(account("alice", Some(1)).await));
    let mut session = session(&fresh_ip());
    assert_eq!(handler.on_auth(credentials("alice", ""), &mut session).await, Ok(INVALID));
    assert_eq!(handler.on_auth(credentials("alice", "wrong"), &mut session).await, Ok(INVALID));
    assert_eq!(handler.on_auth(credentials("Alice", PASSWORD), &mut session).await, Ok(INVALID));
    assert_eq!(stamp(&session), None);
}

#[tokio::test]
async fn accepts_the_right_password_and_stamps_the_credential_generation() {
    let (handler, _) = handler(Directory::serving("test.com").with_user(account("alice", Some(4)).await));
    let mut session = session(&fresh_ip());
    let answer = handler.on_auth(credentials("alice", PASSWORD), &mut session).await;
    assert_eq!(answer, Ok(SmtpAuthResponse::accept("alice")));
    assert_eq!(stamp(&session), Some(4));
}

#[tokio::test]
async fn stamps_generation_1_when_the_row_predates_the_fence() {
    let (handler, _) = handler(Directory::serving("test.com").with_user(account("old", None).await));
    let mut session = session(&fresh_ip());
    handler.on_auth(credentials("old", PASSWORD), &mut session).await.unwrap();
    assert_eq!(stamp(&session), Some(1));
}

#[tokio::test(start_paused = true)]
async fn refuses_a_rate_limited_address_without_a_lookup() {
    let (handler, directory) = handler(Directory::serving("test.com").with_user(account("alice", Some(1)).await));
    let ip = fresh_ip();
    for _ in 0..10 {
        record_auth_failure(&ip, Some("someone")).await;
    }
    let answer = handler.on_auth(credentials("alice", PASSWORD), &mut session(&ip)).await;
    assert_eq!(answer, Err(SmtpHookError::new("Too many failed authentication attempts")));
    assert!(directory.lookups().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_success_forgives_only_its_own_account_never_the_address() {
    let (handler, _) = handler(Directory::serving("test.com").with_user(account("alice", Some(1)).await));
    let ip = fresh_ip();
    for _ in 0..9 {
        handler.on_auth(credentials("alice", "wrong"), &mut session(&ip)).await.unwrap();
    }
    handler.on_auth(credentials("alice", PASSWORD), &mut session(&ip)).await.unwrap();
    assert!(!is_auth_rate_limited(&ip, Some("alice")));
    handler.on_auth(credentials("mallory", "wrong"), &mut session(&ip)).await.unwrap();
    assert!(is_auth_rate_limited(&ip, None), "the per-address count was never reset");
}

#[tokio::test(start_paused = true)]
async fn counts_under_unknown_when_there_is_no_client_address() {
    let (handler, _) = handler(Directory::serving("test.com"));
    handler.on_auth(credentials("nobody-without-ip", "x"), &mut session("")).await.unwrap();
    for _ in 0..9 {
        record_auth_failure("unknown", Some("nobody-without-ip")).await;
    }
    assert!(is_auth_rate_limited("unknown", Some("nobody-without-ip")));
    assert!(!is_auth_rate_limited("", Some("nobody-without-ip")));
}

#[tokio::test(start_paused = true)]
async fn refuses_an_account_that_has_not_finished_signing_up() {
    let mut unfinished = account("half", Some(1)).await;
    unfinished.email = None;
    let (handler, _) = handler(Directory::serving("test.com").with_user(unfinished));
    assert_eq!(handler.on_auth(credentials("half", PASSWORD), &mut session(&fresh_ip())).await, Ok(INVALID));
}

#[tokio::test]
async fn scopes_a_user_at_domain_login_to_a_domain_the_person_owns() {
    let directory = Directory::serving("test.com")
        .with_user(account("alice", Some(1)).await)
        .with_role("id-alice", "example.org", DomainRole::Owner);
    let (handler, _) = handler(directory);
    let mut session = session(&fresh_ip());
    let answer = handler.on_auth(credentials("alice@Example.org", PASSWORD), &mut session).await;
    // The session belongs to the person; the scope rides along.
    assert_eq!(answer, Ok(SmtpAuthResponse::accept("alice")));
    let scope = session.state.get::<DomainScope>().expect("a scope");
    assert_eq!(scope.scope_domain.as_deref(), Some("example.org"));
    assert_eq!(stamp(&session), Some(1));
}

#[tokio::test(start_paused = true)]
async fn refuses_a_scope_the_person_only_views_or_holds_no_role_on_like_a_wrong_password() {
    let directory = Directory::serving("test.com")
        .with_user(account("alice", Some(1)).await)
        .with_role("id-alice", "example.org", DomainRole::Viewer);
    let (handler, _) = handler(directory);
    for login in ["alice@example.org", "alice@example.net"] {
        let mut session = session(&fresh_ip());
        assert_eq!(handler.on_auth(credentials(login, PASSWORD), &mut session).await, Ok(INVALID));
        assert!(session.state.get::<CredentialStamp>().is_none());
    }
}

// --- AUTH with single sign-on ------------------------------------------------

#[tokio::test(start_paused = true)]
async fn never_asks_the_provider_while_single_sign_on_is_off() {
    let (handler, directory) = handler(Directory::serving("test.com"));
    handler.on_auth(credentials("alice", APP_PASSWORD), &mut session(&fresh_ip())).await.unwrap();
    assert_eq!(directory.app_password_calls(), 0);
}

#[tokio::test]
async fn accepts_an_app_password_and_stamps_the_accounts_generation() {
    let (handler, directory) =
        handler(Directory::serving("test.com").with_sso(false).answering(Ok(accepted("alice", 3))));
    let ip = fresh_ip();
    let mut session = session(&ip);
    let answer = handler.on_auth(credentials("alice", APP_PASSWORD), &mut session).await;
    assert_eq!(answer, Ok(SmtpAuthResponse::accept("alice")));
    assert_eq!(stamp(&session), Some(3));
    assert!(directory.lookups().is_empty(), "no local password is tried while they are off");
    let calls = directory.app_password_calls.lock().unwrap().clone();
    assert_eq!(calls, vec![("alice".to_string(), APP_PASSWORD.to_string(), ip)]);
}

#[tokio::test]
async fn answers_454_without_counting_a_failure_while_the_provider_cannot_answer() {
    let (handler, _) =
        handler(Directory::serving("test.com").with_sso(false).answering(Ok(AppPasswordCheck::Unavailable)));
    let ip = fresh_ip();
    for _ in 0..12 {
        let answer = handler.on_auth(credentials("alice", APP_PASSWORD), &mut session(&ip)).await;
        assert_eq!(answer, Err(SmtpHookError::with_code(454, "4.7.0 Temporary authentication failure")));
    }
    assert!(!is_auth_rate_limited(&ip, Some("alice")));
}

#[tokio::test]
async fn answers_451_when_the_account_lookup_behind_the_check_fails() {
    let (handler, _) =
        handler(Directory::serving("test.com").with_sso(false).answering(Err("database is down".into())));
    let answer = handler.on_auth(credentials("alice", APP_PASSWORD), &mut session(&fresh_ip())).await;
    assert_eq!(answer, Err(lookup_failed()));
}

#[tokio::test(start_paused = true)]
async fn refuses_anything_else_as_a_failed_attempt_while_local_passwords_are_off() {
    let (handler, directory) =
        handler(Directory::serving("test.com").with_sso(false).with_user(account("alice", Some(1)).await));
    let ip = fresh_ip();
    // Even the local password is put to the provider, and refused.
    assert_eq!(handler.on_auth(credentials("alice", PASSWORD), &mut session(&ip)).await, Ok(INVALID));
    assert!(directory.lookups().is_empty());
    assert_eq!(directory.app_password_calls(), 1);
    for _ in 0..9 {
        handler.on_auth(credentials("alice", PASSWORD), &mut session(&ip)).await.unwrap();
    }
    assert!(is_auth_rate_limited(&ip, Some("alice")));
}

#[tokio::test]
async fn still_takes_a_local_password_while_auth_local_login_keeps_them() {
    let (handler, directory) =
        handler(Directory::serving("test.com").with_sso(true).with_user(account("alice", Some(2)).await));
    let mut session = session(&fresh_ip());
    let answer = handler.on_auth(credentials("alice", PASSWORD), &mut session).await;
    assert_eq!(answer, Ok(SmtpAuthResponse::accept("alice")));
    assert_eq!(stamp(&session), Some(2));
    assert_eq!(directory.app_password_calls(), 0, "a local password is never sent to the provider");
}

#[tokio::test]
async fn checks_an_app_password_after_the_local_one_while_auth_local_login_keeps_them() {
    let (handler, directory) = handler(
        Directory::serving("test.com")
            .with_sso(true)
            .with_user(account("alice", Some(2)).await)
            .answering(Ok(accepted("alice", 5))),
    );
    let mut session = session(&fresh_ip());
    let answer = handler.on_auth(credentials("alice", APP_PASSWORD), &mut session).await;
    assert_eq!(answer, Ok(SmtpAuthResponse::accept("alice")));
    assert_eq!(stamp(&session), Some(5));
    assert_eq!(directory.lookups(), vec!["alice".to_string()]);
    assert_eq!(directory.app_password_calls(), 1);
}

// --- MAIL FROM ----------------------------------------------------------------

#[tokio::test]
async fn passes_mail_from_for_a_current_credential_and_for_no_credential() {
    let (handler, directory) = handler(Directory::serving("test.com").with_user(account("alice", Some(2)).await));
    assert_eq!(handler.on_mail_from(&address("x@test.com"), &mut session(&fresh_ip())).await, Ok(()));
    assert!(directory.lookups().is_empty(), "an unauthenticated session carries no grant to check");
    let mut current = signed_in(&fresh_ip(), "alice", Some(2));
    assert_eq!(handler.on_mail_from(&address("x@alice.test.com"), &mut current).await, Ok(()));
}

#[tokio::test]
async fn refuses_mail_from_with_421_when_the_credential_rotated_after_auth() {
    let (handler, _) = handler(Directory::serving("test.com").with_user(account("alice", Some(3)).await));
    let mut stale = signed_in(&fresh_ip(), "alice", Some(2));
    let refused = handler.on_mail_from(&address("x@alice.test.com"), &mut stale).await;
    assert_eq!(refused, Err(SmtpHookError::with_code(421, "4.4.2 Credential changed, please re-authenticate")));
    // No stamp reads as generation 1.
    let mut unstamped = signed_in(&fresh_ip(), "alice", None);
    assert!(handler.on_mail_from(&address("x@alice.test.com"), &mut unstamped).await.is_err());
}

#[tokio::test]
async fn refuses_mail_from_with_421_when_the_account_is_gone() {
    // Deleted, or soft-deleted (which the lookup leaves out), after AUTH.
    let (handler, directory) = handler(Directory::serving("test.com"));
    let mut orphaned = signed_in(&fresh_ip(), "alice", Some(1));
    let refused = handler.on_mail_from(&address("x@alice.test.com"), &mut orphaned).await;
    assert_eq!(refused, Err(SmtpHookError::with_code(421, "4.4.2 Credential changed, please re-authenticate")));
    assert_eq!(directory.lookups(), vec!["alice".to_string()]);
}

#[tokio::test]
async fn answers_mail_from_transiently_when_the_version_lookup_fails() {
    let (handler, _) = handler(Directory::serving("test.com").failing("alice"));
    let mut session = signed_in(&fresh_ip(), "alice", Some(1));
    assert_eq!(handler.on_mail_from(&address("x@alice.test.com"), &mut session).await, Err(lookup_failed()));
}

// --- RCPT TO ------------------------------------------------------------------

#[tokio::test]
async fn passes_a_foreign_or_look_alike_recipient_without_a_lookup() {
    let (handler, directory) = handler(Directory::serving("test.com"));
    let mut session = session(&fresh_ip());
    assert_eq!(handler.on_rcpt_to(&address("a@remote.example"), &mut session).await, Ok(()));
    assert_eq!(handler.on_rcpt_to(&address("a@nottest.com"), &mut session).await, Ok(()));
    assert!(directory.lookups().is_empty());
}

#[tokio::test]
async fn passes_a_domain_with_an_owner_and_an_accounts_own_name_and_records_them() {
    let directory = Directory::serving("test.com")
        .with_domain("test.com", "id-owner")
        .with_domain("example.org", "id-owner")
        .with_user(account("bob", Some(1)).await);
    let (handler, directory) = handler(directory);
    let mut session = session(&fresh_ip());
    assert_eq!(handler.on_rcpt_to(&address("hello@test.com"), &mut session).await, Ok(()));
    assert_eq!(handler.on_rcpt_to(&address("Hello@Example.org "), &mut session).await, Ok(()));
    assert_eq!(handler.on_rcpt_to(&address("hello@bob.test.com"), &mut session).await, Ok(()));
    assert_eq!(directory.lookups(), vec!["bob".to_string()]);
    let served = &session.state.get::<DomainScope>().expect("a scope").served_recipients;
    for recipient in ["hello@test.com", "hello@example.org", "hello@bob.test.com"] {
        assert!(served.contains(recipient), "{recipient}");
    }
}

#[tokio::test]
async fn passes_another_domain_without_a_row_for_the_relay_decision_and_charges_nothing() {
    let (handler, _) = handler(Directory::serving("test.com"));
    let ip = fresh_ip();
    for _ in 0..12 {
        assert_eq!(handler.on_rcpt_to(&address("x@example.org"), &mut session(&ip)).await, Ok(()));
    }
    assert!(!is_recipient_probe_rate_limited(&ip));
}

#[tokio::test(start_paused = true)]
async fn refuses_a_name_under_the_zone_that_owns_no_mailbox_and_charges_the_budget() {
    let (handler, directory) = handler(Directory::serving("test.com"));
    let ip = fresh_ip();
    let refused = handler.on_rcpt_to(&address("x@ghost.test.com"), &mut session(&ip)).await;
    assert_eq!(refused, Err(SmtpHookError::with_code(550, "5.1.1 Error: no such mailbox here")));
    // The apex with no owner is refused like any name under the zone.
    assert!(handler.on_rcpt_to(&address("x@test.com"), &mut session(&ip)).await.is_err());
    assert_eq!(directory.lookups(), vec!["ghost".to_string()]);
    for _ in 0..8 {
        handler.on_rcpt_to(&address("x@ghost.test.com"), &mut session(&ip)).await.unwrap_err();
    }
    assert!(is_recipient_probe_rate_limited(&ip));
}

#[tokio::test(start_paused = true)]
async fn refuses_without_a_lookup_once_the_probe_budget_is_spent() {
    let (handler, directory) = handler(Directory::serving("test.com").with_user(account("bob", Some(1)).await));
    let ip = fresh_ip();
    for _ in 0..10 {
        record_recipient_probe(&ip).await;
    }
    let refused = handler.on_rcpt_to(&address("x@bob.test.com"), &mut session(&ip)).await;
    let too_many = SmtpHookError::with_code(451, "4.7.0 Error: too many unknown recipients, try again later");
    assert_eq!(refused, Err(too_many));
    // A foreign recipient never consults the budget.
    assert_eq!(handler.on_rcpt_to(&address("x@remote.example"), &mut session(&ip)).await, Ok(()));
    assert!(directory.lookups().is_empty());
}

#[tokio::test]
async fn does_not_charge_a_recipient_that_resolves() {
    let (handler, _) = handler(Directory::serving("test.com").with_user(account("bob", Some(1)).await));
    let ip = fresh_ip();
    for _ in 0..12 {
        handler.on_rcpt_to(&address("x@bob.test.com"), &mut session(&ip)).await.unwrap();
    }
    assert!(!is_recipient_probe_rate_limited(&ip));
}

#[tokio::test]
async fn passes_every_recipient_when_no_domain_is_served() {
    let (handler, directory) = handler(Directory::default());
    assert_eq!(handler.on_rcpt_to(&address("x@ghost.test.com"), &mut session(&fresh_ip())).await, Ok(()));
    assert!(directory.lookups().is_empty());
}

#[tokio::test]
async fn answers_a_failed_mailbox_lookup_transiently_and_charges_nothing() {
    let (handler, _) = handler(Directory::serving("test.com").failing("pending"));
    let ip = fresh_ip();
    for _ in 0..12 {
        let refused = handler.on_rcpt_to(&address("x@pending.test.com"), &mut session(&ip)).await;
        assert_eq!(refused, Err(lookup_failed()));
    }
    assert!(!is_recipient_probe_rate_limited(&ip));
}

#[tokio::test]
async fn answers_a_rotated_credential_at_rcpt_to_with_451_not_421() {
    // The Bun handler's catch-all turns the credential refusal into the
    // transient lookup failure.
    let (handler, _) = handler(Directory::serving("test.com").with_user(account("alice", Some(3)).await));
    let mut stale = signed_in(&fresh_ip(), "alice", Some(2));
    assert_eq!(handler.on_rcpt_to(&address("x@remote.example"), &mut stale).await, Err(lookup_failed()));
}

// --- DATA ---------------------------------------------------------------------

fn envelope(from: &str, to: &[&str]) -> SmtpEnvelope {
    SmtpEnvelope {
        mail_from: Some(address(from)),
        rcpt_to: to.iter().map(|to| address(to)).collect(),
        ..SmtpEnvelope::default()
    }
}

#[test]
fn chooses_the_lane_by_the_local_parties_and_the_sign_in() {
    let lane =
        |from: &str, to: &[&str], user: Option<&str>| choose_lane(&envelope(from, to), user, "test.com", None);
    assert_eq!(lane("a@remote.example", &["x@test.com"], None), Some(DataLane::Incoming));
    assert_eq!(lane("a@remote.example", &["x@remote.example", "y@bob.test.com"], None), Some(DataLane::Incoming));
    assert_eq!(lane("a@remote.example", &["x@remote.example"], None), None);
    // A local sender selects submission only on a signed-in session.
    assert_eq!(lane("a@bob.test.com", &["x@remote.example"], None), None);
    assert_eq!(lane("a@test.com", &["x@test.com"], None), Some(DataLane::Incoming));
    assert_eq!(lane("a@bob.test.com", &["x@remote.example"], Some("bob")), Some(DataLane::Outgoing));
    assert_eq!(lane("a@bob.test.com", &["x@test.com"], Some("bob")), Some(DataLane::Outgoing));
    // Signed in, but neither party local: relay.
    assert_eq!(lane("a@remote.example", &["x@remote.example"], Some("bob")), None);
    assert_eq!(lane("a@remote.example", &["x@test.com"], Some("bob")), Some(DataLane::Incoming));
    // The null sender is never local.
    assert_eq!(lane("", &["x@remote.example"], Some("bob")), None);
    // A domain that merely ends in the served one is foreign.
    assert_eq!(lane("a@remote.example", &["x@eviltest.com"], None), None);
}

#[test]
fn chooses_the_lane_by_the_domains_rcpt_to_and_mail_from_found_served() {
    let mut scope = DomainScope::default();
    scope.served_recipients.insert("x@example.org".into());
    let lane = |from: &str, to: &[&str], user: Option<&str>, scope: &DomainScope| {
        choose_lane(&envelope(from, to), user, "test.com", Some(scope))
    };
    assert_eq!(lane("a@remote.example", &[" X@Example.org"], None, &scope), Some(DataLane::Incoming));
    assert_eq!(lane("a@remote.example", &["y@example.org"], None, &scope), None);
    let sender = DomainScope { mail_from_served: true, ..DomainScope::default() };
    assert_eq!(lane("a@example.org", &["x@remote.example"], Some("bob"), &sender), Some(DataLane::Outgoing));
    assert_eq!(lane("a@example.org", &["x@remote.example"], None, &sender), None);
}

#[tokio::test]
async fn refuses_data_with_450_while_no_domain_is_served() {
    let (handler, _) = handler(Directory::default());
    let (_writer, stream) = data_stream(1024);
    let mut session = session(&fresh_ip());
    session.envelope = envelope("a@remote.example", &["x@test.com"]);
    let refused = handler.on_data(stream, &mut session).await;
    assert_eq!(refused, Err(SmtpHookError::new("Email service not configured")));
}

#[tokio::test]
async fn refuses_an_unauthenticated_relay_permanently() {
    let (handler, _) = handler(Directory::serving("test.com"));
    let (_writer, stream) = data_stream(1024);
    let mut session = session(&fresh_ip());
    session.envelope = envelope("a@bob.test.com", &["x@remote.example"]);
    let refused = handler.on_data(stream, &mut session).await;
    assert_eq!(refused, Err(SmtpHookError::with_code(550, "Error: relay access denied")));
}
