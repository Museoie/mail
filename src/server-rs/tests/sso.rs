//! Single sign-on above the provider client, against a fake provider over
//! real HTTP: the session re-check, app passwords, the two halves of a
//! browser sign-in, provider events, and signing out.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Map, Value};

use common::{cookie_value, json_reply, query_param, text_reply, FakeIssuer, FAKE_ISSUER};
use mail_server::auth_client::{AccessClaims, IdClaims, Login, LOGIN_COOKIE};
use mail_server::models::{SignedUser, SsoSession};
use mail_server::postgres::repositories::users::UserModel;
use mail_server::sso::accounts::{AccountDirectory, LocalAccount, SsoAccountOutcome};
use mail_server::sso::app_password::{check_app_password_with, AppPasswordCheck, AppPasswordScope, LinkedAccounts};
use mail_server::sso::events::{receive_sso_event_with, EventAnswer, EventEffects};
use mail_server::sso::session::{
    enforce_sso_session_at, sign_out_of_provider, sso_session_fields, EnforcedSession, MAX_RECHECK_DELAY_MS,
    RECHECK_INTERVAL_MS,
};
use mail_server::sso::sign_in::{finish_sign_in_with, start_sign_in, SignInRedirect, CLEAR_LOGIN_COOKIE};
use mail_server::sso::{sso_mode_of, Sso, SsoEnv, SsoMode};

const TOKEN: &str = "/oauth/token";
const INTROSPECT: &str = "/oauth/introspect";
const REVOKE: &str = "/oauth/revoke";

fn t0() -> i64 {
    Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap().timestamp_millis()
}

fn at(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).unwrap()
}

/// A provider whose clock stands at [`t0`].
async fn issuer_at_t0() -> FakeIssuer {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    issuer
}

fn local_login(on: bool) -> impl FnOnce(&mut SsoEnv) {
    move |env| env.auth_local_login = Some(on.to_string())
}

// ---------------------------------------------------------------------------
// The session re-check
// ---------------------------------------------------------------------------

/// A stored session for alice, as a request loads it.
struct FakeSession {
    user: Option<SignedUser>,
    auth: Option<SsoSession>,
    stored: bool,
    fail_destroy: bool,
}

impl FakeSession {
    fn of(auth: Option<SsoSession>) -> FakeSession {
        let user = SignedUser::new(Some("u1".into()), Some("alice".into()), Some("alice@example.com".into()));
        FakeSession { user: Some(user), auth, stored: true, fail_destroy: false }
    }

    fn anonymous() -> FakeSession {
        FakeSession { user: None, auth: None, stored: false, fail_destroy: false }
    }

    fn user_id(&self) -> Option<&str> {
        self.user.as_ref().map(|user| user.id.as_str())
    }
}

impl EnforcedSession for FakeSession {
    fn user(&self) -> Option<&SignedUser> {
        self.user.as_ref()
    }

    fn auth(&self) -> Option<&SsoSession> {
        self.auth.as_ref()
    }

    fn set_auth(&mut self, auth: Option<SsoSession>) {
        self.auth = auth;
    }

    async fn destroy(&mut self) -> Result<()> {
        if self.fail_destroy {
            return Err(anyhow!("connection lost"));
        }
        self.stored = false;
        Ok(())
    }

    fn clear_user(&mut self) {
        self.user = None;
    }
}

fn sso_auth(recheck_at: i64, refresh_token: Option<&str>) -> SsoSession {
    SsoSession { sid: "ses_alice".into(), refresh_token: refresh_token.map(str::to_string), recheck_at: at(recheck_at) }
}

fn expect_ended(session: &FakeSession) {
    assert!(session.user.is_none());
    assert!(session.auth.is_none());
    assert!(!session.stored);
}

#[test]
fn sso_session_fields_keep_the_provider_session_and_refresh_token_due_in_ten_minutes() {
    let claims = AccessClaims {
        sub: "usr_alice".into(),
        sid: "ses_alice".into(),
        username: "alice".into(),
        name: "Alice".into(),
        role: "user".into(),
        all: Map::new(),
    };
    let login = Login {
        claims,
        id_claims: IdClaims { sub: "usr_alice".into(), nonce: "n".into(), all: Map::new() },
        access_token: "at".into(),
        refresh_token: Some("rt_alice".into()),
        return_to: "/".into(),
        clear_cookie: CLEAR_LOGIN_COOKIE.into(),
    };
    assert_eq!(
        sso_session_fields(&login, t0()),
        SsoSession {
            sid: "ses_alice".into(),
            refresh_token: Some("rt_alice".into()),
            recheck_at: at(t0() + RECHECK_INTERVAL_MS),
        }
    );
    assert_eq!(sso_session_fields(&Login { refresh_token: None, ..login }, t0()).refresh_token, None);
    assert_eq!(RECHECK_INTERVAL_MS, 600_000);
    assert_eq!(MAX_RECHECK_DELAY_MS, 3_600_000);
}

#[tokio::test]
async fn leaves_a_session_alone_while_its_re_check_is_not_due_without_calling_the_provider() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let auth = sso_auth(t0() + 1, Some("rt_alice"));
    let mut session = FakeSession::of(Some(auth.clone()));
    enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
    assert_eq!(session.user_id(), Some("u1"));
    assert_eq!(session.auth, Some(auth));
    assert!(session.stored);
    assert!(issuer.calls().is_empty());
}

#[tokio::test]
async fn refreshes_a_due_session_and_moves_its_re_check_ten_minutes_on() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    for due_since in [0, 1] {
        issuer.clear_calls();
        let mut session = FakeSession::of(Some(sso_auth(t0() - due_since, Some("rt_alice"))));
        enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
        assert_eq!(session.user_id(), Some("u1"));
        assert_eq!(
            session.auth,
            Some(SsoSession {
                sid: "ses_alice".into(),
                refresh_token: Some("rt_alice".into()),
                recheck_at: at(t0() + RECHECK_INTERVAL_MS),
            })
        );
        assert!(session.stored);
        let refresh = &issuer.seen(TOKEN)[0];
        assert_eq!(refresh.form("grant_type"), Some("refresh_token"));
        assert_eq!(refresh.form("refresh_token"), Some("rt_alice"));
    }
}

#[tokio::test]
async fn stores_a_refresh_token_the_provider_rotated() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let token = issuer.access_token(json!({}));
    issuer.on_token(move |_| json_reply(200, json!({ "access_token": token, "refresh_token": "rt_next" })));
    let mut session = FakeSession::of(Some(sso_auth(t0() - 1, Some("rt_alice"))));
    enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
    assert_eq!(session.auth.unwrap().refresh_token.as_deref(), Some("rt_next"));
}

#[tokio::test]
async fn ends_the_session_when_the_provider_refuses_the_refresh_token() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    issuer.on_token(|_| json_reply(400, json!({ "error": "invalid_grant" })));
    let mut session = FakeSession::of(Some(sso_auth(t0() - 1, Some("rt_alice"))));
    enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
    expect_ended(&session);
}

#[tokio::test]
async fn serves_the_session_while_the_provider_is_unreachable_up_to_an_hour_past_the_re_check() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    issuer.set_down(true);
    let auth = sso_auth(t0() - MAX_RECHECK_DELAY_MS, Some("rt_alice"));
    let mut session = FakeSession::of(Some(auth.clone()));
    enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
    assert_eq!(session.user_id(), Some("u1"));
    assert_eq!(session.auth, Some(auth));
    assert!(session.stored);
}

#[tokio::test]
async fn serves_the_session_through_a_provider_failure_as_it_does_an_outage() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let auth = sso_auth(t0() - 1, Some("rt_alice"));
    let mut session = FakeSession::of(Some(auth.clone()));

    issuer.on_token(|_| json_reply(503, json!({})));
    enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
    assert_eq!(session.user_id(), Some("u1"));

    issuer.on_token(|_| json_reply(401, json!({ "error": "invalid_client" })));
    enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
    assert_eq!(session.user_id(), Some("u1"));

    // A token that does not verify says nothing about the session either.
    let for_someone_else = issuer.access_token(json!({ "aud": "another-service" }));
    issuer.on_token(move |_| json_reply(200, json!({ "access_token": for_someone_else })));
    enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
    assert_eq!(session.user_id(), Some("u1"));
    assert_eq!(session.auth, Some(auth));
    assert_eq!(issuer.seen(TOKEN).len(), 3);
}

#[tokio::test]
async fn ends_the_session_once_the_re_check_is_over_an_hour_late_whatever_kept_it_from_happening() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let too_late = || FakeSession::of(Some(sso_auth(t0() - MAX_RECHECK_DELAY_MS - 1, Some("rt_alice"))));

    issuer.on_token(|_| json_reply(401, json!({ "error": "invalid_client" })));
    let mut refused_client = too_late();
    enforce_sso_session_at(Some(&sso), &mut refused_client, t0()).await.unwrap();
    expect_ended(&refused_client);

    issuer.set_down(true);
    let mut unreachable = too_late();
    enforce_sso_session_at(Some(&sso), &mut unreachable, t0()).await.unwrap();
    expect_ended(&unreachable);
}

#[tokio::test]
async fn ends_a_due_session_that_has_no_refresh_token_to_re_check_with() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    for refresh_token in [None, Some("")] {
        let mut session = FakeSession::of(Some(sso_auth(t0() - 1, refresh_token)));
        enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
        expect_ended(&session);
    }
    assert!(issuer.calls().is_empty());

    // Until it is due, it is a session like any other.
    let mut not_due = FakeSession::of(Some(sso_auth(t0() + 1, None)));
    enforce_sso_session_at(Some(&sso), &mut not_due, t0()).await.unwrap();
    assert_eq!(not_due.user_id(), Some("u1"));
}

#[tokio::test]
async fn ends_a_password_session_while_password_sign_in_is_off_and_keeps_it_while_it_is_on() {
    let issuer = issuer_at_t0().await;
    let off = issuer.sso(local_login(false));
    assert_eq!(sso_mode_of(Some(&off)), SsoMode { enabled: true, local_login: false });
    let mut ended = FakeSession::of(None);
    enforce_sso_session_at(Some(&off), &mut ended, t0()).await.unwrap();
    expect_ended(&ended);

    let on = issuer.sso(local_login(true));
    assert_eq!(sso_mode_of(Some(&on)), SsoMode { enabled: true, local_login: true });
    let mut kept = FakeSession::of(None);
    enforce_sso_session_at(Some(&on), &mut kept, t0()).await.unwrap();
    assert_eq!(kept.user_id(), Some("u1"));
    assert!(kept.stored);
    assert!(issuer.calls().is_empty());
}

#[tokio::test]
async fn with_single_sign_on_off_keeps_password_sessions_and_ends_the_ones_single_sign_on_started() {
    let mut local = FakeSession::of(None);
    enforce_sso_session_at(None, &mut local, t0()).await.unwrap();
    assert_eq!(local.user_id(), Some("u1"));
    assert!(local.stored);

    let mut from_sso = FakeSession::of(Some(sso_auth(t0() + RECHECK_INTERVAL_MS, Some("rt_alice"))));
    enforce_sso_session_at(None, &mut from_sso, t0()).await.unwrap();
    expect_ended(&from_sso);
}

#[tokio::test]
async fn does_nothing_for_an_anonymous_session() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let mut session = FakeSession::anonymous();
    enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap();
    assert!(session.user.is_none());
    assert!(issuer.calls().is_empty());
}

#[tokio::test]
async fn a_session_that_cannot_be_deleted_fails_the_request_and_is_not_made_anonymous() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let mut session = FakeSession { fail_destroy: true, ..FakeSession::of(None) };
    let error = enforce_sso_session_at(Some(&sso), &mut session, t0()).await.unwrap_err();
    assert_eq!(error.to_string(), "connection lost");
    assert!(session.stored);
}

// ---------------------------------------------------------------------------
// App passwords
// ---------------------------------------------------------------------------

const APP_PASSWORD: &str = "abcd-efgh-ijkl-mnop";
const IP: &str = "203.0.113.7";

/// The account lookup, answering one account and recording what was asked.
struct FakeAccounts {
    account: Mutex<Option<UserModel>>,
    asked: Mutex<Vec<String>>,
    fail: bool,
}

impl FakeAccounts {
    fn with(account: Option<UserModel>) -> FakeAccounts {
        FakeAccounts { account: Mutex::new(account), asked: Mutex::new(Vec::new()), fail: false }
    }

    fn changed_at(&self, ms: i64) {
        *self.account.lock().unwrap() = Some(account(ms));
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl LinkedAccounts for FakeAccounts {
    async fn find_by_auth_sub(&self, sub: &str) -> Result<Option<UserModel>> {
        self.asked.lock().unwrap().push(sub.to_string());
        if self.fail {
            return Err(anyhow!("connection lost"));
        }
        Ok(self.account.lock().unwrap().clone())
    }
}

/// Alice's account, last changed at `updated_ms`.
fn account(updated_ms: i64) -> UserModel {
    UserModel {
        user_id: "u1".into(),
        username: "alice".into(),
        password: Some("$2b$04$not-a-real-digest".into()),
        password_version: Some(3),
        email: Some("alice@example.com".into()),
        expiry: None,
        token: None,
        updated: Some(at(updated_ms).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        is_deleted: Some(false),
        imap_uid_validity: None,
        auth_sub: Some("usr_alice".into()),
    }
}

/// Alice's account, unchanged for an hour.
fn settled() -> FakeAccounts {
    FakeAccounts::with(Some(account(t0() - 3_600_000)))
}

fn active(issuer: &FakeIssuer, scope: &'static str) {
    issuer.on_introspect(move |_| {
        json_reply(
            200,
            json!({ "active": true, "sub": "usr_alice", "username": "alice", "role": "user", "scope": scope, "jti": "apw_1" }),
        )
    });
}

async fn check(
    sso: &Sso,
    accounts: &FakeAccounts,
    username: &str,
    scope: AppPasswordScope,
    now: i64,
) -> AppPasswordCheck {
    check_app_password_with(sso, username, APP_PASSWORD, scope, IP, accounts, now).await.expect("the lookup answers")
}

fn accepted(result: &AppPasswordCheck) -> bool {
    matches!(result, AppPasswordCheck::Accepted { .. })
}

#[tokio::test]
async fn accepts_an_active_app_password_for_the_linked_account_stamping_its_generation() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    active(&issuer, "imap smtp");

    let result = check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await;
    let AppPasswordCheck::Accepted { user, password_version } = result else {
        panic!("expected the app password to be accepted, not {result:?}");
    };
    assert_eq!(password_version, 3);
    assert_eq!(
        serde_json::to_value(&user).unwrap(),
        json!({ "id": "u1", "email": "alice@example.com", "username": "alice", "passwordVersion": 3 })
    );
    let call = &issuer.seen(INTROSPECT)[0];
    assert_eq!(call.form("token"), Some(APP_PASSWORD));
    assert_eq!(call.header("x-real-ip"), Some(IP));
    assert_eq!(accounts.asked(), ["usr_alice"]);
}

#[tokio::test]
async fn checks_the_scope_of_the_protocol_asking() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    active(&issuer, "smtp");
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Rejected);
    assert!(accepted(&check(&sso, &accounts, "alice", AppPasswordScope::Smtp, t0()).await));
}

#[tokio::test]
async fn refuses_an_app_password_presented_under_another_accounts_username() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    active(&issuer, "imap smtp");
    for username in ["bob", "Alice", "alice@alice.example.com", ""] {
        assert_eq!(check(&sso, &accounts, username, AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Rejected);
    }
}

#[tokio::test]
async fn refuses_an_inactive_answer_and_an_owner_no_account_is_linked_to() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Rejected);
    assert!(accounts.asked().is_empty());

    active(&issuer, "imap smtp");
    let nobody = FakeAccounts::with(None);
    assert_eq!(check(&sso, &nobody, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Rejected);
    assert_eq!(nobody.asked(), ["usr_alice"]);
}

#[tokio::test]
async fn is_unavailable_not_refused_while_the_provider_cannot_answer() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    issuer.set_down(true);
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Unavailable);
    issuer.set_down(false);
    issuer.on_introspect(|_| json_reply(502, json!({})));
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Unavailable);
    issuer.on_introspect(|_| json_reply(401, json!({ "error": "invalid_client" })));
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Unavailable);
    assert!(accounts.asked().is_empty());

    // A provider that is rate limiting the question has not vouched for the password.
    issuer.on_introspect(|_| text_reply(429, ""));
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Rejected);
}

#[tokio::test]
async fn asks_again_past_the_cache_for_an_account_changed_moments_ago() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    active(&issuer, "imap smtp");
    assert!(accepted(&check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await));
    assert_eq!(issuer.seen(INTROSPECT).len(), 1);

    // Revoked through an event another copy received: this copy still holds
    // the cached answer, and only the account row says something changed.
    issuer.on_introspect(|_| json_reply(200, json!({ "active": false })));
    accounts.changed_at(t0() - 1_000);
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Rejected);
    assert_eq!(issuer.seen(INTROSPECT).len(), 2);
}

#[tokio::test]
async fn asks_again_for_an_account_changed_while_a_stale_answer_could_still_be_served() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    active(&issuer, "imap smtp");
    assert!(accepted(&check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await));

    // Past the client's fresh minute, with the provider unreachable, the
    // client serves that answer stale for up to ten minutes.
    let later = t0() + 150_000;
    issuer.set_now(later);
    issuer.set_down(true);
    assert!(accepted(&check(&sso, &accounts, "alice", AppPasswordScope::Imap, later).await));

    // The account changed after it was cached (an event another copy
    // received), so it is asked for afresh, and with nobody to answer,
    // nothing is accepted.
    accounts.changed_at(t0() + 30_000);
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, later).await, AppPasswordCheck::Unavailable);
}

#[tokio::test]
async fn serves_the_cached_answer_for_an_account_that_has_not_changed() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    active(&issuer, "imap smtp");
    check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await;
    issuer.on_introspect(|_| json_reply(200, json!({ "active": false })));
    assert!(accepted(&check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await));
    assert_eq!(issuer.seen(INTROSPECT).len(), 1);

    // Ten minutes is where "changed moments ago" ends.
    accounts.changed_at(t0() - 600_000);
    assert!(accepted(&check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await));
    assert_eq!(issuer.seen(INTROSPECT).len(), 1);
    accounts.changed_at(t0() - 599_999);
    assert_eq!(check(&sso, &accounts, "alice", AppPasswordScope::Imap, t0()).await, AppPasswordCheck::Rejected);
    assert_eq!(issuer.seen(INTROSPECT).len(), 2);
}

#[tokio::test]
async fn names_the_clients_address_only_when_it_is_one() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let accounts = settled();
    active(&issuer, "imap smtp");
    let from = |address: &'static str| {
        check_app_password_with(&sso, "alice", APP_PASSWORD, AppPasswordScope::Imap, address, &accounts, t0())
    };
    from("::ffff:198.51.100.4").await.unwrap();
    assert_eq!(issuer.seen(INTROSPECT)[0].header("x-real-ip"), Some("198.51.100.4"));
    sso.client.forget(Some("usr_alice"), None);
    from("unknown").await.unwrap();
    assert_eq!(issuer.seen(INTROSPECT)[1].header("x-real-ip"), None);
}

#[tokio::test]
async fn lets_a_failed_account_lookup_fail_the_check() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    active(&issuer, "imap smtp");
    let accounts = FakeAccounts { fail: true, ..settled() };
    let error = check_app_password_with(&sso, "alice", APP_PASSWORD, AppPasswordScope::Imap, IP, &accounts, t0())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "connection lost");
}

// ---------------------------------------------------------------------------
// A browser sign-in
// ---------------------------------------------------------------------------

/// The directory as the callback sees it when alice is already linked, or as
/// a test changes it.
struct FakeDirectory {
    by_sub: Option<LocalAccount>,
    by_username: Option<LocalAccount>,
    fail: bool,
}

fn alice_account() -> LocalAccount {
    LocalAccount {
        user_id: "u1".into(),
        username: "alice".into(),
        email: Some("alice@example.com".into()),
        auth_sub: Some("usr_alice".into()),
        password_version: 2,
    }
}

impl FakeDirectory {
    fn linked() -> FakeDirectory {
        FakeDirectory { by_sub: Some(alice_account()), by_username: None, fail: false }
    }

    fn check(&self) -> Result<()> {
        if self.fail {
            return Err(anyhow!("connection lost"));
        }
        Ok(())
    }
}

impl AccountDirectory for FakeDirectory {
    async fn find_by_sub(&self, _sub: &str) -> Result<Option<LocalAccount>> {
        self.check()?;
        Ok(self.by_sub.clone())
    }

    async fn find_by_username(&self, _username: &str) -> Result<Option<LocalAccount>> {
        self.check()?;
        Ok(self.by_username.clone())
    }

    async fn find_unlinked_by_email(&self, _email: &str) -> Result<Vec<LocalAccount>> {
        self.check()?;
        Ok(Vec::new())
    }

    async fn others_can_read(&self, _email: &str, _user_id: &str) -> Result<bool> {
        self.check()?;
        Ok(false)
    }

    async fn link(&self, _user_id: &str, _sub: &str) -> Result<Option<LocalAccount>> {
        self.check()?;
        Ok(None)
    }

    async fn create(&self, _username: &str, _email: &str, _sub: &str) -> Result<Option<LocalAccount>> {
        self.check()?;
        Ok(None)
    }

    async fn update_email(&self, _user_id: &str, _email: &str) -> Result<()> {
        self.check()
    }
}

/// What the browser holds after `GET /api/auth/login`.
struct StartedSignIn {
    redirect: SignInRedirect,
    state: String,
    cookie_header: String,
}

/// Starts a sign-in and answers the code exchange for it, as the provider would.
async fn start_for(sso: &Sso, issuer: &FakeIssuer, return_to: &str, ids: Value) -> StartedSignIn {
    let redirect = start_sign_in(sso, Some(return_to)).await;
    issuer.issue_for(&redirect.location, ids, json!({}));
    let state = query_param(&redirect.location, "state").expect("a state parameter");
    let cookie_header = format!("{LOGIN_COOKIE}={}", cookie_value(&redirect.cookies[0], LOGIN_COOKIE));
    StartedSignIn { redirect, state, cookie_header }
}

fn failure(code: &str) -> SignInRedirect {
    SignInRedirect { location: format!("/sign-in?sso_error={code}"), cookies: vec![CLEAR_LOGIN_COOKIE.to_string()] }
}

#[tokio::test]
async fn start_sign_in_sends_the_browser_to_the_provider_with_the_login_cookie() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let started = start_for(&sso, &issuer, "/mail/42", json!({})).await;
    let location = &started.redirect.location;
    assert!(location.starts_with(&format!("{FAKE_ISSUER}/oauth/authorize?")));
    assert_eq!(query_param(location, "client_id").as_deref(), Some("mail"));
    assert_eq!(query_param(location, "redirect_uri").as_deref(), Some("https://mail.example.test/api/auth/callback"));
    assert_eq!(started.redirect.cookies.len(), 1);
    assert!(started.redirect.cookies[0].starts_with("__Host-login="));
    assert!(started.redirect.cookies[0].ends_with("; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=300"));
}

#[tokio::test]
async fn start_sign_in_lands_on_the_sign_in_page_when_the_provider_cannot_be_reached() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    issuer.set_down(true);
    assert_eq!(
        start_sign_in(&sso, None).await,
        SignInRedirect { location: "/sign-in?sso_error=unavailable".into(), cookies: Vec::new() }
    );
    issuer.set_down(false);
    issuer.set_discovery("issuer", json!("https://someone-else.example.test"));
    assert_eq!(
        start_sign_in(&sso, Some("/mail")).await,
        SignInRedirect { location: "/sign-in?sso_error=failed".into(), cookies: Vec::new() }
    );
}

#[tokio::test]
async fn finish_sign_in_hands_the_route_the_account_and_the_session_fields_to_start() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let started = start_for(&sso, &issuer, "/mail/42", json!({})).await;
    let url = format!("/api/auth/callback?code=c0de&state={}", started.state);
    let done = finish_sign_in_with(&sso, &url, Some(&started.cookie_header), &FakeDirectory::linked(), t0())
        .await
        .expect("the sign-in completes");

    assert_eq!(
        serde_json::to_value(&done.user).unwrap(),
        json!({ "id": "u1", "email": "alice@example.com", "username": "alice", "passwordVersion": 2 })
    );
    assert_eq!(
        done.auth,
        SsoSession {
            sid: "ses_alice".into(),
            refresh_token: Some("rt_alice".into()),
            recheck_at: at(t0() + 10 * 60 * 1000),
        }
    );
    assert_eq!(done.outcome, SsoAccountOutcome::Existing);
    assert_eq!(done.return_to, "/mail/42");
    assert_eq!(
        done.redirect(),
        SignInRedirect { location: "/mail/42".into(), cookies: vec![CLEAR_LOGIN_COOKIE.to_string()] }
    );
}

#[tokio::test]
async fn finish_sign_in_refuses_an_account_the_link_rules_refuse() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let started = start_for(&sso, &issuer, "/mail/42", json!({})).await;
    let url = format!("/api/auth/callback?code=c0de&state={}", started.state);
    // An account has her username, and it is not linked to her.
    let taken = FakeDirectory {
        by_sub: None,
        by_username: Some(LocalAccount { auth_sub: None, ..alice_account() }),
        fail: false,
    };
    let refused = finish_sign_in_with(&sso, &url, Some(&started.cookie_header), &taken, t0()).await;
    assert_eq!(refused, Err(failure("account_exists")));
}

#[tokio::test]
async fn finish_sign_in_names_why_a_sign_in_did_not_complete() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let directory = FakeDirectory::linked();
    let finish = |url: String, cookie: Option<String>| {
        let (sso, directory) = (&sso, &directory);
        async move { finish_sign_in_with(sso, &url, cookie.as_deref(), directory, t0()).await }
    };

    let no_cookie = finish("/api/auth/callback?code=c0de&state=s".into(), None).await;
    assert_eq!(no_cookie, Err(failure("login_failed")));

    let denied = start_for(&sso, &issuer, "/mail/42", json!({})).await;
    let url = format!("/api/auth/callback?error=access_denied&state={}", denied.state);
    assert_eq!(finish(url, Some(denied.cookie_header)).await, Err(failure("access_denied")));

    let wrong_code = start_for(&sso, &issuer, "/mail/42", json!({})).await;
    let url = format!("/api/auth/callback?code=other&state={}", wrong_code.state);
    assert_eq!(finish(url, Some(wrong_code.cookie_header)).await, Err(failure("login_failed")));

    let other_nonce = start_for(&sso, &issuer, "/", json!({ "nonce": "another" })).await;
    let url = format!("/api/auth/callback?code=c0de&state={}", other_nonce.state);
    assert_eq!(finish(url, Some(other_nonce.cookie_header)).await, Err(failure("invalid_token")));

    let down = start_for(&sso, &issuer, "/mail/42", json!({})).await;
    issuer.set_down(true);
    let url = format!("/api/auth/callback?code=c0de&state={}", down.state);
    assert_eq!(finish(url, Some(down.cookie_header)).await, Err(failure("unavailable")));
}

#[tokio::test]
async fn finish_sign_in_puts_no_code_or_token_in_the_url_it_lands_on() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let started = start_for(&sso, &issuer, "/mail/42", json!({})).await;
    let url = format!("/api/auth/callback?code=c0de&state={}", started.state);
    let broken = FakeDirectory { fail: true, ..FakeDirectory::linked() };
    let failed = finish_sign_in_with(&sso, &url, Some(&started.cookie_header), &broken, t0()).await;
    assert_eq!(failed, Err(failure("failed")));

    // An email claim the account rules cannot read is the same fault.
    let odd_email = start_for(&sso, &issuer, "/mail/42", json!({ "email": 42 })).await;
    let url = format!("/api/auth/callback?code=c0de&state={}", odd_email.state);
    let unlinked = FakeDirectory { by_sub: None, by_username: None, fail: false };
    let failed = finish_sign_in_with(&sso, &url, Some(&odd_email.cookie_header), &unlinked, t0()).await;
    assert_eq!(failed, Err(failure("failed")));
}

// ---------------------------------------------------------------------------
// Signing out
// ---------------------------------------------------------------------------

async fn wait_for_revocation(issuer: &FakeIssuer) {
    for _ in 0..200 {
        if !issuer.seen(REVOKE).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the refresh token was not revoked");
}

const LOGOUT_URL: &str =
    "https://auth.example.test/logout?client_id=mail&post_logout_redirect_uri=https%3A%2F%2Fmail.example.test%2F";

#[tokio::test]
async fn signing_out_revokes_the_refresh_token_and_returns_the_providers_logout_url() {
    let issuer = issuer_at_t0().await;
    let sso = Arc::new(issuer.sso(|_| {}));
    let logout_url = sign_out_of_provider(&sso, &sso_auth(t0(), Some("rt_alice"))).await;
    assert_eq!(logout_url.as_deref(), Some(LOGOUT_URL));
    // Revocation is not awaited; give it its round trip.
    wait_for_revocation(&issuer).await;
    assert_eq!(issuer.seen(REVOKE)[0].form("token"), Some("rt_alice"));
    assert_eq!(issuer.seen(REVOKE)[0].form("token_type_hint"), Some("refresh_token"));
}

#[tokio::test]
async fn signing_out_goes_through_whatever_the_provider_does() {
    let issuer = issuer_at_t0().await;
    let sso = Arc::new(issuer.sso(|_| {}));
    issuer.on_revoke(|_| json_reply(503, json!({})));
    let logout_url = sign_out_of_provider(&sso, &sso_auth(t0(), Some("rt_alice"))).await;
    assert_eq!(logout_url.as_deref(), Some(LOGOUT_URL));
    wait_for_revocation(&issuer).await;

    // Without a refresh token there is nothing to revoke.
    issuer.clear_calls();
    for refresh_token in [None, Some("")] {
        let logout_url = sign_out_of_provider(&sso, &sso_auth(t0(), refresh_token)).await;
        assert_eq!(logout_url.as_deref(), Some(LOGOUT_URL));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(issuer.calls().is_empty());

    // A provider that was never reached cannot say where its logout is.
    let unreachable = issuer_at_t0().await;
    unreachable.set_down(true);
    let sso = Arc::new(unreachable.sso(|_| {}));
    assert_eq!(sign_out_of_provider(&sso, &sso_auth(t0(), Some("rt_alice"))).await, None);
    // Nor can one without an end-session endpoint.
    let without = issuer_at_t0().await;
    without.set_discovery("end_session_endpoint", Value::Null);
    let sso = Arc::new(without.sso(|_| {}));
    assert_eq!(sign_out_of_provider(&sso, &sso_auth(t0(), None)).await, None);
}

// ---------------------------------------------------------------------------
// Provider events
// ---------------------------------------------------------------------------

/// Counts what an event did, and fails on demand.
#[derive(Default)]
struct CountedEffects {
    revoked: Mutex<Vec<String>>,
    ended: Mutex<Vec<String>>,
    evicted: AtomicUsize,
    fail: bool,
}

impl EventEffects for CountedEffects {
    async fn bump_password_version_by_auth_sub(&self, sub: &str) -> Result<Option<(String, String)>> {
        if self.fail {
            return Err(anyhow!("connection lost"));
        }
        self.revoked.lock().unwrap().push(sub.to_string());
        Ok(Some(("u1".into(), "alice".into())))
    }

    async fn delete_sessions_for_user(&self, _user_id: &str) -> Result<u64> {
        Ok(2)
    }

    async fn delete_sessions_by_auth_sid(&self, sid: &str) -> Result<u64> {
        self.ended.lock().unwrap().push(sid.to_string());
        Ok(1)
    }

    fn evict_imap_sessions(&self, _username: &str) -> usize {
        self.evicted.fetch_add(1, Ordering::SeqCst);
        1
    }

    fn evict_smtp_connections(&self, _username: &str) -> usize {
        self.evicted.fetch_add(1, Ordering::SeqCst);
        1
    }
}

fn answer(status: u16, body: Value) -> EventAnswer {
    EventAnswer { status, body }
}

#[tokio::test]
async fn an_event_is_applied_and_acknowledged() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let effects = CountedEffects::default();
    let token = issuer.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), json!({}));
    for _ in 0..3 {
        let received = receive_sso_event_with(&sso, &token, &effects).await.unwrap();
        assert_eq!(received, answer(202, json!({ "status": "success" })));
    }
    // Delivered again, it is applied again.
    assert_eq!(*effects.revoked.lock().unwrap(), ["usr_alice", "usr_alice", "usr_alice"]);
    assert_eq!(effects.evicted.load(Ordering::SeqCst), 6);

    let ended = issuer.event_token(json!({ "session.ended": { "sid": "ses_alice" } }), json!({}));
    assert_eq!(receive_sso_event_with(&sso, &ended, &effects).await.unwrap().status, 202);
    assert_eq!(*effects.ended.lock().unwrap(), ["ses_alice"]);
}

#[tokio::test]
async fn an_event_it_does_not_act_on_is_acknowledged() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let effects = CountedEffects::default();
    let token = issuer.event_token(json!({ "invite.accepted": { "invite": "inv_1", "sub": "usr_bob" } }), json!({}));
    assert_eq!(
        receive_sso_event_with(&sso, &token, &effects).await.unwrap(),
        answer(202, json!({ "status": "success" }))
    );
    assert!(effects.revoked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_body_that_is_not_a_verified_event_is_refused_with_400() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let effects = CountedEffects::default();
    let invalid = answer(400, json!({ "status": "failed", "message": "Invalid event." }));
    let forged = issuer.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), json!({ "aud": "other" }));
    for body in ["not-a-token", "", forged.as_str()] {
        assert_eq!(receive_sso_event_with(&sso, body, &effects).await.unwrap(), invalid);
    }
    assert!(effects.revoked.lock().unwrap().is_empty());
    // The body is exactly what the Bun server writes, field order included.
    assert_eq!(invalid.body.to_string(), r#"{"status":"failed","message":"Invalid event."}"#);
}

#[tokio::test]
async fn an_event_is_answered_503_while_the_providers_keys_cannot_be_fetched_so_it_retries() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let effects = CountedEffects::default();
    issuer.set_down(true);
    let token = issuer.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), json!({}));
    let received = receive_sso_event_with(&sso, &token, &effects).await.unwrap();
    assert_eq!(received, answer(503, json!({ "status": "error", "message": "Try again later." })));
    assert_eq!(received.body.to_string(), r#"{"status":"error","message":"Try again later."}"#);
    assert!(effects.revoked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_event_that_cannot_be_applied_is_a_fault_so_it_retries() {
    let issuer = issuer_at_t0().await;
    let sso = issuer.sso(|_| {});
    let effects = CountedEffects { fail: true, ..Default::default() };
    let token = issuer.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), json!({}));
    let error = receive_sso_event_with(&sso, &token, &effects).await.unwrap_err();
    assert_eq!(error.to_string(), "connection lost");

    // So is a discovery document the client refuses, which is not an outage.
    let refused = issuer_at_t0().await;
    refused.set_discovery("issuer", json!("https://someone-else.example.test"));
    let sso = refused.sso(|_| {});
    let token = refused.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), json!({ "iss": FAKE_ISSUER }));
    let error = receive_sso_event_with(&sso, &token, &CountedEffects::default()).await.unwrap_err();
    assert_eq!(error.to_string(), "the discovery document names a different issuer");
}

// ---------------------------------------------------------------------------
// What the HTTP layer and the mail servers await
// ---------------------------------------------------------------------------

fn assert_send<T: Send>(_future: &T) {}

/// The request pipeline and the mail servers run on a multi-threaded runtime:
/// every future they are handed has to be able to move between threads.
#[test]
fn the_futures_the_servers_await_can_cross_threads() {
    use mail_server::auth_client::AuthEvent;
    use mail_server::sso::app_password::check_app_password;
    use mail_server::sso::events::{handle_sso_event, receive_sso_event};
    use mail_server::sso::session::enforce_sso_session;
    use mail_server::sso::sign_in::finish_sign_in;
    use mail_server::sso::{build_sso, ClientOverrides};

    let env = SsoEnv {
        auth_issuer: Some(FAKE_ISSUER.into()),
        auth_client_id: Some("mail".into()),
        auth_client_secret: Some(common::FAKE_CLIENT_SECRET.into()),
        app_hostname: Some("mail.example.test".into()),
        ..Default::default()
    };
    let sso = Arc::new(build_sso(&env, ClientOverrides::default()).unwrap().unwrap());
    let auth = sso_auth(t0(), Some("rt_alice"));
    let event = AuthEvent::default();
    let mut session = FakeSession::of(None);

    assert_send(&start_sign_in(&sso, Some("/mail")));
    assert_send(&finish_sign_in(&sso, "/api/auth/callback", None));
    assert_send(&enforce_sso_session(&mut session));
    assert_send(&sign_out_of_provider(&sso, &auth));
    assert_send(&receive_sso_event(&sso, ""));
    assert_send(&handle_sso_event(&event));
    assert_send(&check_app_password(&sso, "alice", "secret", AppPasswordScope::Imap, "127.0.0.1"));
    assert_send(&sso.client.refresh("rt_alice"));
    assert_send(&sso.client.introspect("secret", None));
}
