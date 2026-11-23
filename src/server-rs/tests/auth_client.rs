//! The provider client against a fake provider over real HTTP: discovery, the
//! code flow, token verification, the key set and its lifetimes, refresh,
//! revocation, introspection and its cache, events, and the logout URL.

mod common;

use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use common::{
    cookie_value, decode_segment, encode_segment, json_reply, new_key, query_param, sign_with, text_reply, FakeIssuer,
    Reply, FAKE_CLIENT_ID, FAKE_ISSUER, FAKE_REDIRECT_URI,
};
use mail_server::auth_client::{
    AuthClient, AuthError, AuthEvent, Introspection, LoginErrorCode, Prompt, StartLoginOptions, LOGIN_COOKIE,
};

const DISCOVERY: &str = "/.well-known/openid-configuration";
const JWKS: &str = "/.well-known/jwks.json";
const TOKEN: &str = "/oauth/token";
const INTROSPECT: &str = "/oauth/introspect";
const REVOKE: &str = "/oauth/revoke";
const CLEAR_COOKIE: &str = "__Host-login=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0";

const SECOND: i64 = 1000;
const MINUTE: i64 = 60 * SECOND;

fn t0() -> i64 {
    Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap().timestamp_millis()
}

fn invalid_token(message: &str) -> AuthError {
    AuthError::InvalidToken(message.to_string())
}

fn unavailable(message: &str) -> AuthError {
    AuthError::Unavailable(message.to_string())
}

fn other(message: &str) -> AuthError {
    AuthError::Other(message.to_string())
}

fn login_error(code: LoginErrorCode) -> AuthError {
    AuthError::Login { code, error: None }
}

/// A sign-in that has been started: what the browser now holds.
struct Started {
    redirect_url: String,
    state: String,
    /// The fields the login cookie carries.
    saved: Value,
}

impl Started {
    fn cookie(&self) -> String {
        format!("{LOGIN_COOKIE}={}", encode_segment(&self.saved))
    }

    fn callback(&self) -> String {
        format!("/api/auth/callback?code=c0de&state={}", self.state)
    }
}

async fn start(client: &AuthClient, options: StartLoginOptions) -> Started {
    let started = client.start_login(&options).await.expect("the sign-in starts");
    let state = query_param(&started.redirect_url, "state").expect("a state parameter");
    let saved = decode_segment(&cookie_value(&started.login_cookie, LOGIN_COOKIE));
    Started { redirect_url: started.redirect_url, state, saved }
}

/// Starts a sign-in and has the fake provider answer its code exchange.
async fn start_with_tokens(client: &AuthClient, issuer: &FakeIssuer, ids: Value, at: Value) -> Started {
    let started = start(client, StartLoginOptions { return_to: Some("/mail/42".into()), ..Default::default() }).await;
    issuer.issue_for(&started.redirect_url, ids, at);
    started
}

/// The error of a whole sign-in whose tokens carry these changes.
async fn sign_in_error(issuer: &FakeIssuer, ids: Value, at: Value) -> AuthError {
    let client = issuer.client();
    let started = start_with_tokens(&client, issuer, ids, at).await;
    client.finish_login(&started.callback(), Some(&started.cookie())).await.expect_err("the sign-in is refused")
}

/// The error of a sign-in whose token endpoint answers `answer(access_token, id_token)`.
async fn sign_in_error_with_answer(
    issuer: &FakeIssuer,
    answer: impl Fn(&FakeIssuer, &str) -> Value + Send + Sync + 'static,
) -> AuthError {
    let client = issuer.client();
    let started = start(&client, StartLoginOptions::default()).await;
    let nonce = started.saved["nonce"].as_str().unwrap().to_string();
    let body = answer(issuer, &nonce);
    issuer.on_token(move |_| json_reply(200, body.clone()));
    client.finish_login(&started.callback(), Some(&started.cookie())).await.expect_err("the sign-in is refused")
}

fn basic_credentials() -> String {
    format!("Basic {}", STANDARD.encode(format!("{FAKE_CLIENT_ID}:{}", common::FAKE_CLIENT_SECRET)))
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetches_the_discovery_document_once_through_the_back_channel() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    assert!(issuer.calls().is_empty(), "building the client contacts nothing");

    start(&client, StartLoginOptions::default()).await;
    start(&client, StartLoginOptions::default()).await;
    client.logout_url(None).await.unwrap();

    let calls = issuer.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "GET");
    assert_eq!(calls[0].path, DISCOVERY);
    assert_eq!(calls[0].header("accept"), Some("application/json"));
    assert_eq!(calls[0].header("authorization"), None);
}

#[tokio::test]
async fn shares_one_discovery_fetch_between_concurrent_callers() {
    let issuer = FakeIssuer::start().await;
    issuer.delay(DISCOVERY, 50);
    let client = issuer.client();
    let options = StartLoginOptions::default();
    let (a, b, c) = tokio::join!(client.start_login(&options), client.start_login(&options), client.logout_url(None));
    assert!(a.is_ok() && b.is_ok() && c.is_ok());
    assert_eq!(issuer.seen(DISCOVERY).len(), 1);
}

#[tokio::test]
async fn an_unreachable_provider_is_unavailable_and_is_asked_again_next_time() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    issuer.set_down(true);
    let options = StartLoginOptions::default();
    assert_eq!(client.start_login(&options).await.unwrap_err(), unavailable("auth is unreachable"));
    // Concurrent callers share the failure too.
    let (a, b) = tokio::join!(client.start_login(&options), client.logout_url(None));
    assert_eq!(a.unwrap_err(), unavailable("auth is unreachable"));
    assert_eq!(b.unwrap_err(), unavailable("auth is unreachable"));

    issuer.set_down(false);
    assert!(client.start_login(&options).await.is_ok());
    assert_eq!(issuer.seen(DISCOVERY).len(), 1);
}

#[tokio::test]
async fn a_discovery_answer_that_is_not_the_document_is_unavailable() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let options = StartLoginOptions::default();
    let cases: Vec<(Reply, &str)> = vec![
        (json_reply(500, json!({})), "auth answered 500"),
        (text_reply(503, "Service Unavailable"), "auth answered 503"),
        (json_reply(404, json!({})), "discovery answered 404"),
        (json_reply(401, json!({})), "discovery answered 401"),
        (text_reply(200, "<html>It works!</html>"), "auth answered 200 with a malformed body"),
        (text_reply(200, ""), "auth answered 200 with a malformed body"),
        (json_reply(200, json!([])), "auth answered 200 with a malformed body"),
        (json_reply(200, json!("text")), "auth answered 200 with a malformed body"),
        (json_reply(200, Value::Null), "auth answered 200 with a malformed body"),
    ];
    for (reply, message) in cases {
        issuer.on(DISCOVERY, move |_| reply.clone());
        assert_eq!(client.start_login(&options).await.unwrap_err(), unavailable(message));
    }
    // Nothing of that was kept.
    issuer.reset();
    assert!(client.start_login(&options).await.is_ok());
}

#[tokio::test]
async fn a_redirect_is_an_answer_not_followed() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    issuer.on(DISCOVERY, |_| Reply { status: 302, body: String::new(), location: Some("/elsewhere".into()) });
    let error = client.start_login(&StartLoginOptions::default()).await.unwrap_err();
    assert_eq!(error, unavailable("discovery answered 302"));
    assert_eq!(issuer.calls().iter().map(|call| call.path.as_str()).collect::<Vec<_>>(), [DISCOVERY]);
}

#[tokio::test]
async fn refuses_a_discovery_document_it_cannot_use_and_does_not_keep_it() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let options = StartLoginOptions::default();

    issuer.set_discovery("issuer", json!("https://someone-else.example.test"));
    let error = client.start_login(&options).await.unwrap_err();
    assert_eq!(error, other("the discovery document names a different issuer"));
    // The issuer must match to the letter.
    issuer.set_discovery("issuer", json!(format!("{FAKE_ISSUER}/")));
    let error = client.start_login(&options).await.unwrap_err();
    assert_eq!(error, other("the discovery document names a different issuer"));
    issuer.set_discovery("issuer", json!(FAKE_ISSUER));

    for field in ["authorization_endpoint", "token_endpoint", "jwks_uri"] {
        let kept = issuer.options();
        issuer.set_discovery(field, Value::Null);
        let error = client.start_login(&options).await.unwrap_err();
        assert_eq!(error, other(&format!("the discovery document has no {field}")));
        issuer.set_discovery(field, json!(42));
        let error = client.start_login(&options).await.unwrap_err();
        assert_eq!(error, other(&format!("the discovery document has no {field}")));
        issuer.set_issuer(&kept.issuer);
    }

    assert!(client.start_login(&options).await.is_ok());
    assert_eq!(issuer.seen(DISCOVERY).len(), 9);
}

#[tokio::test]
async fn a_provider_that_takes_longer_than_five_seconds_is_unavailable() {
    let issuer = FakeIssuer::start().await;
    issuer.delay(DISCOVERY, 20_000);
    let client = issuer.client();
    let begun = Instant::now();
    let error = client.start_login(&StartLoginOptions::default()).await.unwrap_err();
    assert_eq!(error, unavailable("auth is unreachable"));
    assert!(begun.elapsed() >= Duration::from_millis(4_500), "gave up after {:?}", begun.elapsed());
    assert!(begun.elapsed() < Duration::from_secs(12), "gave up after {:?}", begun.elapsed());
}

#[tokio::test]
async fn without_a_back_channel_the_issuer_itself_is_called() {
    let issuer = FakeIssuer::start().await;
    issuer.set_issuer(&issuer.back_channel_url);
    let client = issuer.client_with(|options| options.back_channel_url = None);
    let started = start_with_tokens(&client, &issuer, json!({}), json!({})).await;
    assert!(started.redirect_url.starts_with(&format!("{}/oauth/authorize?", issuer.back_channel_url)));
    let login = client.finish_login(&started.callback(), Some(&started.cookie())).await.unwrap();
    assert_eq!(login.claims.sub, "usr_alice");
    assert_eq!(issuer.calls().iter().map(|call| call.path.as_str()).collect::<Vec<_>>(), [DISCOVERY, TOKEN, JWKS]);
}

#[tokio::test]
async fn the_back_channel_replaces_the_issuers_whole_base_path_included() {
    let issuer = FakeIssuer::start().await;
    issuer.set_issuer("https://login.example.test/realms/mail/");
    let back = format!("{}//", issuer.back_channel_url);
    let client = issuer.client_with(|options| options.back_channel_url = Some(back));
    let started = start_with_tokens(&client, &issuer, json!({}), json!({})).await;
    // The browser is sent to the public address.
    assert!(started.redirect_url.starts_with("https://login.example.test/realms/mail/oauth/authorize?"));
    client.finish_login(&started.callback(), Some(&started.cookie())).await.unwrap();
    assert_eq!(issuer.calls().iter().map(|call| call.path.as_str()).collect::<Vec<_>>(), [DISCOVERY, TOKEN, JWKS]);
}

#[tokio::test]
async fn an_endpoint_outside_the_issuer_is_called_where_the_document_says() {
    let issuer = FakeIssuer::start().await;
    issuer.set_discovery("token_endpoint", json!(format!("{}/elsewhere/token", issuer.back_channel_url)));
    // Sharing a prefix with the issuer is not being under it.
    issuer.set_discovery("revocation_endpoint", json!(format!("{FAKE_ISSUER}.evil.example.test/revoke")));
    let client = issuer.client();
    let token = issuer.access_token(json!({}));
    issuer.on("/elsewhere/token", move |_| json_reply(200, json!({ "access_token": token })));
    assert_eq!(client.refresh("rt_alice").await.unwrap().claims.sub, "usr_alice");
    assert_eq!(issuer.seen("/elsewhere/token").len(), 1);
    assert_eq!(issuer.seen(TOKEN).len(), 0);

    assert_eq!(client.revoke("rt_alice").await.unwrap_err(), unavailable("auth is unreachable"));
    assert_eq!(issuer.seen(REVOKE).len(), 0);
}

// ---------------------------------------------------------------------------
// Starting a sign-in
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_login_sends_the_browser_to_the_provider_with_the_login_cookie() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0() + 999);
    let client = issuer.client();
    let started = client
        .start_login(&StartLoginOptions { return_to: Some("/mail/42?x=1#frag".into()), ..Default::default() })
        .await
        .unwrap();

    let url = url::Url::parse(&started.redirect_url).unwrap();
    assert_eq!(
        format!("{}{}", url.origin().ascii_serialization(), url.path()),
        format!("{FAKE_ISSUER}/oauth/authorize")
    );
    let params: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let names: Vec<&str> = params.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "response_type",
            "client_id",
            "redirect_uri",
            "scope",
            "state",
            "nonce",
            "code_challenge",
            "code_challenge_method"
        ]
    );
    let param = |name: &str| query_param(&started.redirect_url, name).unwrap();
    assert_eq!(param("response_type"), "code");
    assert_eq!(param("client_id"), "mail");
    assert_eq!(param("redirect_uri"), FAKE_REDIRECT_URI);
    assert_eq!(param("scope"), "openid profile email");
    assert_eq!(param("code_challenge_method"), "S256");
    assert!(started
        .redirect_url
        .contains("redirect_uri=https%3A%2F%2Fmail.example.test%2Fapi%2Fauth%2Fcallback&scope=openid+profile+email&"));

    let (pair, attributes) = started.login_cookie.split_once("; ").unwrap();
    assert_eq!(attributes, "Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=300");
    let value = pair.strip_prefix("__Host-login=").unwrap();
    let json = String::from_utf8(URL_SAFE_NO_PAD.decode(value).unwrap()).unwrap();
    let saved: Value = serde_json::from_str(&json).unwrap();
    // The fields, in the order the Bun server writes them.
    let state = saved["state"].as_str().unwrap();
    let nonce = saved["nonce"].as_str().unwrap();
    let verifier = saved["verifier"].as_str().unwrap();
    assert_eq!(
        json,
        format!(
            r#"{{"state":"{state}","nonce":"{nonce}","verifier":"{verifier}","returnTo":"/mail/42?x=1#frag","iat":{},"fresh":false}}"#,
            t0() / 1000
        )
    );
    assert_eq!(param("state"), state);
    assert_eq!(param("nonce"), nonce);
    assert_eq!(param("code_challenge"), URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())));
    for random in [state, nonce, verifier] {
        assert_eq!(random.len(), 43);
        assert!(random.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }
    assert!(state != nonce && nonce != verifier && state != verifier);

    let again = start(&client, StartLoginOptions::default()).await;
    assert_ne!(again.state, state);
    assert_ne!(again.saved["verifier"], saved["verifier"]);
}

#[tokio::test]
async fn start_login_keeps_only_a_local_path_to_return_to() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let kept = |return_to: Option<&str>| {
        let client = client.clone();
        let options = StartLoginOptions { return_to: return_to.map(str::to_string), ..Default::default() };
        async move { start(&client, options).await.saved["returnTo"].as_str().unwrap().to_string() }
    };
    for path in ["/mail/42", "/", "/mail/42?view=thread#m3", "/mail/é", "/say \"hi\"".replace(' ', "%20").as_str()] {
        assert_eq!(kept(Some(path)).await, path);
    }
    for not_local in
        ["https://evil.example/", "//evil.example/mail", "/\\evil.example", "mail/42", "", "/mail 42", "/mail\t42"]
    {
        assert_eq!(kept(Some(not_local)).await, "/", "{not_local:?}");
    }
    assert_eq!(kept(None).await, "/");
    assert_eq!(kept(Some(&format!("/{}", "a".repeat(2047)))).await.len(), 2048);
    assert_eq!(kept(Some(&format!("/{}", "a".repeat(2048)))).await, "/");
}

#[tokio::test]
async fn start_login_passes_on_what_was_asked_of_the_sign_in() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    let started = client
        .start_login(&StartLoginOptions {
            return_to: None,
            max_age: Some(0),
            prompt: Some(Prompt::Login),
            login_hint: Some("alice@example.com".into()),
        })
        .await
        .unwrap();
    assert!(started
        .redirect_url
        .ends_with("&code_challenge_method=S256&prompt=login&max_age=0&login_hint=alice%40example.com"));
    let json =
        String::from_utf8(URL_SAFE_NO_PAD.decode(cookie_value(&started.login_cookie, LOGIN_COOKIE)).unwrap()).unwrap();
    assert!(json.ends_with(&format!(r#""returnTo":"/","iat":{},"maxAge":0,"fresh":true}}"#, t0() / 1000)), "{json}");

    let none =
        start(&client, StartLoginOptions { prompt: Some(Prompt::None), max_age: Some(3600), ..Default::default() })
            .await;
    assert_eq!(query_param(&none.redirect_url, "prompt").as_deref(), Some("none"));
    assert_eq!(query_param(&none.redirect_url, "max_age").as_deref(), Some("3600"));
    assert_eq!(none.saved["maxAge"], json!(3600));
    assert_eq!(none.saved["fresh"], json!(false));

    for max_age in [-1, 9_007_199_254_740_992] {
        let error =
            client.start_login(&StartLoginOptions { max_age: Some(max_age), ..Default::default() }).await.unwrap_err();
        assert_eq!(error, other("maxAge must be a whole number of seconds"));
    }
}

#[tokio::test]
async fn start_login_keeps_the_parameters_the_authorization_endpoint_already_carries() {
    let issuer = FakeIssuer::start().await;
    issuer.set_discovery(
        "authorization_endpoint",
        json!(format!("{FAKE_ISSUER}/oauth/authorize?tenant=a%20b&scope=legacy&x=1&scope=again#top")),
    );
    let client = issuer.client();
    let started = client.start_login(&StartLoginOptions::default()).await.unwrap();
    assert!(started.redirect_url.starts_with(&format!(
        "{FAKE_ISSUER}/oauth/authorize?tenant=a+b&scope=openid+profile+email&x=1&response_type=code&client_id=mail&"
    )));
    assert!(started.redirect_url.ends_with("&code_challenge_method=S256#top"));

    issuer.set_discovery("authorization_endpoint", json!("/oauth/authorize"));
    let relative = issuer.client();
    assert!(matches!(relative.start_login(&StartLoginOptions::default()).await, Err(AuthError::Other(_))));
}

// ---------------------------------------------------------------------------
// Finishing a sign-in
// ---------------------------------------------------------------------------

#[tokio::test]
async fn finish_login_exchanges_the_code_and_returns_who_signed_in() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let started = start_with_tokens(&client, &issuer, json!({}), json!({})).await;
    issuer.clear_calls();

    let url = format!("https://mail.example.test{}&session_state=abc#fragment", started.callback());
    let login = client.finish_login(&url, Some(&started.cookie())).await.unwrap();

    assert_eq!(login.claims.sub, "usr_alice");
    assert_eq!(login.claims.sid, "ses_alice");
    assert_eq!(login.claims.username, "alice");
    assert_eq!(login.claims.name, "Alice");
    assert_eq!(login.claims.role, "user");
    assert_eq!(login.claims.all["aud"], json!("mail"));
    assert_eq!(login.id_claims.sub, "usr_alice");
    assert_eq!(login.id_claims.nonce, started.saved["nonce"].as_str().unwrap());
    assert_eq!(login.id_claims.email(), &json!("alice@example.com"));
    assert!(login.id_claims.email_verified());
    assert_eq!(login.id_claims.all["amr"], json!(["pwd"]));
    assert_eq!(login.refresh_token.as_deref(), Some("rt_alice"));
    assert_eq!(login.return_to, "/mail/42");
    assert_eq!(login.clear_cookie, CLEAR_COOKIE);
    assert_eq!(login.access_token.split('.').count(), 3);

    let calls = issuer.calls();
    assert_eq!(calls.iter().map(|call| call.path.as_str()).collect::<Vec<_>>(), [TOKEN, JWKS]);
    let token = &calls[0];
    assert_eq!(token.method, "POST");
    assert_eq!(
        token.form,
        [
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), "c0de".to_string()),
            ("redirect_uri".to_string(), FAKE_REDIRECT_URI.to_string()),
            ("code_verifier".to_string(), started.saved["verifier"].as_str().unwrap().to_string()),
        ]
    );
    assert_eq!(token.header("content-type"), Some("application/x-www-form-urlencoded"));
    assert_eq!(token.header("accept"), Some("application/json"));
    assert_eq!(token.header("authorization"), Some(basic_credentials().as_str()));
    assert_eq!(calls[1].method, "GET");
    assert_eq!(calls[1].header("accept"), Some("application/json"));
    assert_eq!(calls[1].header("authorization"), None);
}

#[tokio::test]
async fn the_clients_credentials_are_form_encoded_before_basic_encoding() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client_with(|options| {
        options.client_id = "mail app/1".into();
        options.client_secret = Some("s3cr3t +/:=~!*'()&%é".into());
    });
    let token = issuer.access_token(json!({ "aud": "mail app/1" }));
    issuer.on_token(move |_| json_reply(200, json!({ "access_token": token })));
    client.refresh("rt_alice").await.unwrap();
    let expected = STANDARD.encode("mail+app%2F1:s3cr3t+%2B%2F%3A%3D~!*'()%26%25%C3%A9");
    assert_eq!(issuer.seen(TOKEN)[0].header("authorization"), Some(format!("Basic {expected}").as_str()));
    assert_eq!(issuer.seen(TOKEN)[0].form("client_id"), None);
}

#[tokio::test]
async fn a_public_client_names_itself_in_the_body_instead() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client_with(|options| options.client_secret = None);
    assert_eq!(client.refresh("rt alice&x=1").await.unwrap().claims.sub, "usr_alice");
    let call = &issuer.seen(TOKEN)[0];
    assert_eq!(call.body, "grant_type=refresh_token&refresh_token=rt+alice%26x%3D1&client_id=mail");
    assert_eq!(call.header("authorization"), None);
}

#[tokio::test]
async fn finish_login_refuses_a_callback_that_cannot_complete_the_sign_in() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    let started = start_with_tokens(&client, &issuer, json!({}), json!({})).await;
    issuer.clear_calls();
    let finish = |url: String, cookie: Option<String>| {
        let client = client.clone();
        async move { client.finish_login(&url, cookie.as_deref()).await.expect_err("the callback is refused") }
    };
    let with = |change: fn(&mut Value)| {
        let mut saved = started.saved.clone();
        change(&mut saved);
        Some(format!("{LOGIN_COOKIE}={}", encode_segment(&saved)))
    };
    let no_cookie = login_error(LoginErrorCode::NoLoginCookie);

    // No login cookie, or one that is not what the client wrote.
    assert_eq!(finish(started.callback(), None).await, no_cookie);
    assert_eq!(finish(started.callback(), Some("theme=dark".into())).await, no_cookie);
    assert_eq!(finish(started.callback(), Some(format!("{LOGIN_COOKIE}="))).await, no_cookie);
    assert_eq!(finish(started.callback(), Some(format!("{}==", started.cookie()))).await, no_cookie);
    assert_eq!(
        finish(started.callback(), Some(format!("{LOGIN_COOKIE}={}", encode_segment(&json!({}))))).await,
        no_cookie
    );
    for change in [
        (|saved| void(saved.as_object_mut().unwrap().remove("state"))) as fn(&mut Value),
        |saved| void(saved.as_object_mut().unwrap().remove("nonce")),
        |saved| void(saved.as_object_mut().unwrap().remove("verifier")),
        |saved| void(saved.as_object_mut().unwrap().remove("iat")),
        |saved| saved["iat"] = json!(saved["iat"].to_string()),
        |saved| saved["state"] = json!(12345),
    ] {
        assert_eq!(finish(started.callback(), with(change)).await, no_cookie);
    }
    // A cookie that is base64url but not a JSON object fails as a token segment does.
    for not_an_object in [json!([started.saved.clone()]), json!("state"), Value::Null] {
        let cookie = format!("{LOGIN_COOKIE}={}", encode_segment(&not_an_object));
        assert_eq!(finish(started.callback(), Some(cookie)).await, invalid_token("malformed segment"));
    }

    // An error from the provider, cut to 64 characters, whatever else the callback carries.
    let denied =
        finish("/api/auth/callback?error=access_denied&state=wrong&code=c0de".into(), Some(started.cookie())).await;
    assert_eq!(
        denied,
        AuthError::Login { code: LoginErrorCode::AuthorizationError, error: Some("access_denied".into()) }
    );
    assert_eq!(denied.to_string(), "authorization_error: access_denied");
    let long = finish(format!("/api/auth/callback?error={}", "e".repeat(100)), Some(started.cookie())).await;
    assert_eq!(long, AuthError::Login { code: LoginErrorCode::AuthorizationError, error: Some("e".repeat(64)) });
    let empty = finish("/api/auth/callback?error=".into(), Some(started.cookie())).await;
    assert_eq!(empty, AuthError::Login { code: LoginErrorCode::AuthorizationError, error: Some(String::new()) });

    // A state that is not this sign-in's.
    let mismatch = login_error(LoginErrorCode::StateMismatch);
    assert_eq!(finish("/api/auth/callback?code=c0de&state=other".into(), Some(started.cookie())).await, mismatch);
    assert_eq!(finish("/api/auth/callback?code=c0de".into(), Some(started.cookie())).await, mismatch);
    assert_eq!(finish("/api/auth/callback".into(), Some(started.cookie())).await, mismatch);
    let prefix = &started.state[..started.state.len() - 1];
    assert_eq!(finish(format!("/api/auth/callback?code=c0de&state={prefix}"), Some(started.cookie())).await, mismatch);
    assert_eq!(
        finish(format!("/api/auth/callback?state=first&state={}&code=c0de", started.state), Some(started.cookie()))
            .await,
        mismatch
    );

    // No code.
    let missing = login_error(LoginErrorCode::MissingCode);
    assert_eq!(finish(format!("/api/auth/callback?state={}", started.state), Some(started.cookie())).await, missing);
    assert_eq!(
        finish(format!("/api/auth/callback?code=&state={}", started.state), Some(started.cookie())).await,
        missing
    );

    // None of that asked the provider anything.
    assert!(issuer.calls().is_empty());
    // And the sign-in is still good.
    assert!(client.finish_login(&started.callback(), Some(&started.cookie())).await.is_ok());
}

fn void<T>(_: T) {}

#[tokio::test]
async fn finish_login_gives_a_sign_in_five_minutes() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    let expired = login_error(LoginErrorCode::LoginExpired);
    let attempt = |started_at: i64| {
        let (client, issuer) = (client.clone(), &issuer);
        async move {
            issuer.set_now(started_at);
            let started = start_with_tokens(&client, issuer, json!({}), json!({})).await;
            issuer.set_now(t0());
            // The tokens are minted now, whenever the sign-in started.
            issuer.issue_for(&started.redirect_url, json!({}), json!({}));
            client.finish_login(&started.callback(), Some(&started.cookie())).await.map(|_| ())
        }
    };
    assert_eq!(attempt(t0() - 300 * SECOND).await, Ok(()));
    assert_eq!(attempt(t0() - 301 * SECOND).await, Err(expired.clone()));
    // The cookie's clock may run thirty seconds ahead of ours.
    assert_eq!(attempt(t0() + 30 * SECOND).await, Ok(()));
    assert_eq!(attempt(t0() + 31 * SECOND).await, Err(expired));
}

#[tokio::test]
async fn finish_login_classifies_what_the_token_endpoint_answers() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let cases: Vec<(Reply, AuthError)> = vec![
        (
            json_reply(400, json!({ "error": "invalid_grant" })),
            AuthError::InvalidGrant("the provider refused the grant".into()),
        ),
        (
            json_reply(401, json!({ "error": "invalid_grant" })),
            AuthError::InvalidGrant("the provider refused the grant".into()),
        ),
        (
            json_reply(429, json!({ "error": "invalid_grant" })),
            AuthError::InvalidGrant("the provider refused the grant".into()),
        ),
        (
            json_reply(429, json!({ "error": "slow_down" })),
            unavailable("the token endpoint is rate limiting this client"),
        ),
        (text_reply(429, ""), unavailable("auth answered 429 with a malformed body")),
        (json_reply(500, json!({ "error": "invalid_grant" })), unavailable("auth answered 500")),
        (text_reply(200, "OK"), unavailable("auth answered 200 with a malformed body")),
        (text_reply(400, "Bad Request"), unavailable("auth answered 400 with a malformed body")),
        (json_reply(200, json!({})), unavailable("the token endpoint answered without an access token or ID token")),
        (
            json_reply(200, json!({ "access_token": "a.b.c" })),
            unavailable("the token endpoint answered without an access token or ID token"),
        ),
        (
            json_reply(200, json!({ "access_token": 42, "id_token": "a.b.c" })),
            unavailable("the token endpoint answered without an access token or ID token"),
        ),
        (
            json_reply(401, json!({ "error": "invalid_client" })),
            other("the token endpoint refused the request (invalid_client)"),
        ),
        (
            json_reply(400, json!({ "error": "INVALID_GRANT" })),
            other("the token endpoint refused the request (INVALID_GRANT)"),
        ),
        (json_reply(400, json!({ "error": "not a code" })), other("the token endpoint refused the request (400)")),
        (json_reply(403, json!({ "message": "nope" })), other("the token endpoint refused the request (403)")),
        (json_reply(404, json!({})), other("the token endpoint refused the request (404)")),
    ];
    for (reply, expected) in cases {
        let started = start(&client, StartLoginOptions::default()).await;
        let answer = reply.clone();
        issuer.on_token(move |_| answer.clone());
        let error = client.finish_login(&started.callback(), Some(&started.cookie())).await.unwrap_err();
        assert_eq!(error, expected, "{reply:?}");
    }

    let started = start(&client, StartLoginOptions::default()).await;
    issuer.set_down(true);
    let error = client.finish_login(&started.callback(), Some(&started.cookie())).await.unwrap_err();
    assert_eq!(error, unavailable("auth is unreachable"));
}

#[tokio::test]
async fn finish_login_refuses_a_code_redeemed_with_another_verifier() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let other_sign_in = start(&client, StartLoginOptions::default()).await;
    let mut started = start_with_tokens(&client, &issuer, json!({}), json!({})).await;
    started.saved["verifier"] = other_sign_in.saved["verifier"].clone();
    let error = client.finish_login(&started.callback(), Some(&started.cookie())).await.unwrap_err();
    assert_eq!(error, AuthError::InvalidGrant("the provider refused the grant".into()));
}

#[tokio::test]
async fn finish_login_refuses_tokens_that_do_not_verify() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let now_s = issuer.now_s();

    // The access token.
    let access_cases: Vec<(Value, &str)> = vec![
        (json!({ "iss": "https://someone-else.example.test" }), "wrong issuer"),
        (json!({ "iss": format!("{FAKE_ISSUER}/") }), "wrong issuer"),
        (json!({ "iss": 42 }), "wrong issuer"),
        (json!({ "aud": "another-service" }), "wrong audience"),
        (json!({ "aud": ["another-service", "Mail"] }), "wrong audience"),
        (json!({ "aud": null }), "wrong audience"),
        (json!({ "iat": now_s + 31 }), "issued in the future"),
        (json!({ "iat": now_s.to_string() }), "issued in the future"),
        (json!({ "exp": now_s - 31 }), "expired"),
        (json!({ "exp": (now_s + 600).to_string() }), "expired"),
        (json!({ "sub": 42 }), "no sub claim"),
        (json!({ "sid": null }), "no sid claim"),
        (json!({ "username": ["alice"] }), "no username claim"),
        (json!({ "name": false }), "no name claim"),
        (json!({ "role": null }), "no role claim"),
    ];
    for (claims, message) in access_cases {
        assert_eq!(sign_in_error(&issuer, json!({}), claims.clone()).await, invalid_token(message), "{claims}");
    }
    for claim in ["iss", "aud", "iat", "exp", "sub", "sid", "username", "name", "role"] {
        let error = sign_in_error_with_answer(&issuer, move |issuer, nonce| {
            let mut claims = issuer.access_claims();
            claims.remove(claim);
            json!({ "access_token": issuer.access_token_of(claims), "id_token": issuer.id_token(json!({ "nonce": nonce })) })
        })
        .await;
        assert!(matches!(error, AuthError::InvalidToken(_)), "{claim}: {error:?}");
    }

    // The ID token.
    let id_cases: Vec<(Value, &str)> = vec![
        (json!({ "iss": "https://someone-else.example.test" }), "wrong issuer"),
        (json!({ "aud": "another-client" }), "wrong audience"),
        (json!({ "aud": [] }), "wrong audience"),
        (json!({ "aud": ["mail", "another-client"] }), "wrong authorized party"),
        (json!({ "aud": ["mail", "another-client"], "azp": "another-client" }), "wrong authorized party"),
        (json!({ "nonce": "another" }), "nonce mismatch"),
        (json!({ "nonce": 42 }), "nonce mismatch"),
        (json!({ "sub": "usr_mallory" }), "the ID token names another subject"),
        (json!({ "sub": null }), "the ID token names another subject"),
        (json!({ "exp": now_s - 31 }), "expired"),
        (json!({ "iat": now_s + 31 }), "issued in the future"),
    ];
    for (claims, message) in id_cases {
        assert_eq!(sign_in_error(&issuer, claims.clone(), json!({})).await, invalid_token(message), "{claims}");
    }
}

#[tokio::test]
async fn finish_login_accepts_tokens_at_the_edge_of_what_it_allows() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let now_s = issuer.now_s();
    let accepted: Vec<(Value, Value)> = vec![
        // Thirty seconds of leeway on both ends.
        (json!({ "exp": now_s - 30, "iat": now_s + 30 }), json!({ "exp": now_s - 30, "iat": now_s + 30 })),
        // Fractional times are numbers too.
        (json!({ "iat": now_s as f64 + 0.5 }), json!({ "exp": now_s as f64 + 600.25 })),
        // An audience list that includes this app; several ID audiences with the client as authorized party.
        (json!({ "aud": ["mail", "another-client"], "azp": "mail" }), json!({ "aud": ["another-service", "mail"] })),
        (json!({ "aud": ["mail"] }), json!({})),
        // Without the optional claims.
        (json!({ "auth_time": null, "amr": null, "email": null, "email_verified": null }), json!({})),
    ];
    for (ids, at) in accepted {
        let client = issuer.client();
        let started = start_with_tokens(&client, &issuer, ids.clone(), at.clone()).await;
        let login = client.finish_login(&started.callback(), Some(&started.cookie())).await;
        assert!(login.is_ok(), "{ids} {at}: {login:?}");
    }
}

#[tokio::test]
async fn finish_login_refuses_tokens_that_are_not_the_tokens_it_asked_for() {
    let issuer = FakeIssuer::start().await;
    let header = |change: Value| {
        let mut header = json!({ "alg": "EdDSA", "typ": "at+jwt", "kid": "k1" });
        for (name, value) in change.as_object().unwrap() {
            if value == "<absent>" {
                header.as_object_mut().unwrap().remove(name);
            } else {
                header[name] = value.clone();
            }
        }
        header
    };
    let header_cases: Vec<(Value, &str)> = vec![
        (json!({ "alg": "none" }), "unexpected alg"),
        (json!({ "alg": "HS256" }), "unexpected alg"),
        (json!({ "alg": "RS256" }), "unexpected alg"),
        (json!({ "alg": "eddsa" }), "unexpected alg"),
        (json!({ "alg": "<absent>" }), "unexpected alg"),
        (json!({ "typ": "JWT" }), "unexpected typ"),
        (json!({ "typ": "application/at+jwt" }), "unexpected typ"),
        (json!({ "typ": 7 }), "unexpected typ"),
        (json!({ "typ": "<absent>" }), "unexpected typ"),
        (json!({ "crit": ["exp"] }), "unsupported critical header"),
        (json!({ "crit": null }), "unsupported critical header"),
        (json!({ "kid": "<absent>" }), "no kid"),
        (json!({ "kid": 1 }), "no kid"),
        (json!({ "kid": "k2" }), "signed with an unknown key"),
    ];
    for (change, message) in header_cases {
        let token_header = header(change.clone());
        let error = sign_in_error_with_answer(&issuer, move |issuer, nonce| {
            let access_token = issuer.sign(token_header.clone(), Value::Object(issuer.access_claims()));
            json!({ "access_token": access_token, "id_token": issuer.id_token(json!({ "nonce": nonce })) })
        })
        .await;
        assert_eq!(error, invalid_token(message), "{change}");
    }

    // An ID token where the access token belongs, and the other way round.
    let swapped = sign_in_error_with_answer(&issuer, |issuer, nonce| {
        json!({ "access_token": issuer.id_token(json!({ "nonce": nonce })), "id_token": issuer.access_token(json!({})) })
    })
    .await;
    assert_eq!(swapped, invalid_token("unexpected typ"));
    let access_as_id = sign_in_error_with_answer(&issuer, |issuer, _| {
        json!({ "access_token": issuer.access_token(json!({})), "id_token": issuer.access_token(json!({})) })
    })
    .await;
    assert_eq!(access_as_id, invalid_token("unexpected typ"));

    // Not a JWT at all.
    for not_a_jwt in ["opaque", "a.b", "a.b.c.d", "", "!!!.###.$$$", "a..c"] {
        let token = not_a_jwt.to_string();
        let error = sign_in_error_with_answer(&issuer, move |issuer, nonce| {
            json!({ "access_token": token, "id_token": issuer.id_token(json!({ "nonce": nonce })) })
        })
        .await;
        assert_eq!(error, invalid_token("not a JWT"), "{not_a_jwt:?}");
    }
    let padded = sign_in_error_with_answer(&issuer, |issuer, nonce| {
        json!({ "access_token": format!("{}=", issuer.access_token(json!({}))), "id_token": issuer.id_token(json!({ "nonce": nonce })) })
    })
    .await;
    assert_eq!(padded, invalid_token("not a JWT"));
    let not_json = sign_in_error_with_answer(&issuer, |issuer, nonce| {
        let array = encode_segment(&json!([]));
        json!({ "access_token": format!("{array}.{array}.AAAA"), "id_token": issuer.id_token(json!({ "nonce": nonce })) })
    })
    .await;
    assert_eq!(not_json, invalid_token("malformed segment"));
}

#[tokio::test]
async fn finish_login_refuses_a_signature_that_is_not_the_providers() {
    let issuer = FakeIssuer::start().await;
    let stranger = new_key();
    let forged = sign_in_error_with_answer(&issuer, move |issuer, nonce| {
        let header = json!({ "alg": "EdDSA", "typ": "at+jwt", "kid": "k1" });
        let access_token = sign_with(&stranger, &header, &Value::Object(issuer.access_claims()));
        json!({ "access_token": access_token, "id_token": issuer.id_token(json!({ "nonce": nonce })) })
    })
    .await;
    assert_eq!(forged, invalid_token("bad signature"));

    // A signature of the wrong length is refused before the key is used.
    for change in [|token: String| format!("{token}AA"), |token: String| token[..token.len() - 2].to_string()] {
        let error = sign_in_error_with_answer(&issuer, move |issuer, nonce| {
            json!({ "access_token": change(issuer.access_token(json!({}))), "id_token": issuer.id_token(json!({ "nonce": nonce })) })
        })
        .await;
        assert_eq!(error, invalid_token("bad signature"));
    }
    // The payload is what was signed: change it and the signature no longer fits.
    let edited = sign_in_error_with_answer(&issuer, |issuer, nonce| {
        let token = issuer.access_token(json!({}));
        let parts: Vec<&str> = token.split('.').collect();
        let mut claims = issuer.access_claims();
        claims.insert("username".into(), json!("mallory"));
        let payload = encode_segment(&Value::Object(claims));
        json!({ "access_token": format!("{}.{payload}.{}", parts[0], parts[2]), "id_token": issuer.id_token(json!({ "nonce": nonce })) })
    })
    .await;
    assert_eq!(edited, invalid_token("bad signature"));
}

#[tokio::test]
async fn finish_login_holds_the_sign_in_to_the_recency_its_cookie_asks_for() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let now_s = issuer.now_s();
    let attempt = |options: StartLoginOptions, ids: Value| {
        let issuer = &issuer;
        async move {
            let client = issuer.client();
            let started = start(&client, options).await;
            issuer.issue_for(&started.redirect_url, ids, json!({}));
            client.finish_login(&started.callback(), Some(&started.cookie())).await.map(|_| ())
        }
    };
    let max_age = |seconds: i64| StartLoginOptions { max_age: Some(seconds), ..Default::default() };
    let fresh = || StartLoginOptions { prompt: Some(Prompt::Login), ..Default::default() };

    assert_eq!(attempt(max_age(60), json!({})).await, Ok(()));
    assert_eq!(attempt(max_age(60), json!({ "auth_time": now_s - 90 })).await, Ok(()));
    assert_eq!(
        attempt(max_age(60), json!({ "auth_time": now_s - 91 })).await,
        Err(invalid_token("the sign-in is older than max_age"))
    );
    assert_eq!(attempt(max_age(0), json!({ "auth_time": null })).await, Err(invalid_token("no auth_time")));
    assert_eq!(
        attempt(max_age(3600), json!({ "auth_time": now_s.to_string() })).await,
        Err(invalid_token("no auth_time"))
    );

    assert_eq!(attempt(fresh(), json!({})).await, Ok(()));
    assert_eq!(attempt(fresh(), json!({ "auth_time": now_s - 30 })).await, Ok(()));
    assert_eq!(
        attempt(fresh(), json!({ "auth_time": now_s - 31 })).await,
        Err(invalid_token("the sign-in is older than the login request"))
    );
    assert_eq!(attempt(fresh(), json!({ "auth_time": null })).await, Err(invalid_token("no auth_time")));

    // A sign-in that asked for neither takes any auth_time, or none.
    assert_eq!(attempt(StartLoginOptions::default(), json!({ "auth_time": 1 })).await, Ok(()));
    assert_eq!(attempt(StartLoginOptions::default(), json!({ "auth_time": null })).await, Ok(()));
}

#[tokio::test]
async fn finish_login_returns_to_the_root_when_the_cookies_path_is_not_local() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    for (return_to, expected) in [
        (json!("https://evil.example/"), "/"),
        (json!("//evil.example"), "/"),
        (json!(42), "/"),
        (Value::Null, "/"),
        (json!("/elsewhere?x=1"), "/elsewhere?x=1"),
    ] {
        let mut started = start_with_tokens(&client, &issuer, json!({}), json!({})).await;
        started.saved["returnTo"] = return_to;
        let login = client.finish_login(&started.callback(), Some(&started.cookie())).await.unwrap();
        assert_eq!(login.return_to, expected);
    }
}

#[tokio::test]
async fn the_audience_of_access_tokens_can_differ_from_the_client_id() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client_with(|options| options.audience = Some("mailbox".into()));
    let started = start_with_tokens(&client, &issuer, json!({}), json!({ "aud": "mailbox" })).await;
    assert!(client.finish_login(&started.callback(), Some(&started.cookie())).await.is_ok());

    // An access token for the client id is not one for the service, and the ID token is always the client's.
    let for_client = start_with_tokens(&client, &issuer, json!({}), json!({})).await;
    let error = client.finish_login(&for_client.callback(), Some(&for_client.cookie())).await.unwrap_err();
    assert_eq!(error, invalid_token("wrong audience"));
    let id_for_service =
        start_with_tokens(&client, &issuer, json!({ "aud": "mailbox" }), json!({ "aud": "mailbox" })).await;
    let error = client.finish_login(&id_for_service.callback(), Some(&id_for_service.cookie())).await.unwrap_err();
    assert_eq!(error, invalid_token("wrong audience"));

    // Events are for the service too.
    assert!(client.read_event(&issuer.event_token(json!({ "x.y": {} }), json!({ "aud": "mailbox" }))).await.is_ok());
    assert_eq!(
        client.read_event(&issuer.event_token(json!({ "x.y": {} }), json!({}))).await.unwrap_err(),
        invalid_token("wrong audience")
    );
}

// ---------------------------------------------------------------------------
// The key set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_set_is_fetched_once_and_again_after_an_hour() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    for _ in 0..3 {
        client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap();
    }
    assert_eq!(issuer.seen(JWKS).len(), 1);

    issuer.set_now(t0() + 60 * MINUTE - 1);
    client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap();
    assert_eq!(issuer.seen(JWKS).len(), 1);

    issuer.set_now(t0() + 60 * MINUTE);
    client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap();
    assert_eq!(issuer.seen(JWKS).len(), 2);
}

#[tokio::test]
async fn concurrent_verifications_share_one_key_fetch() {
    let issuer = FakeIssuer::start().await;
    issuer.delay(JWKS, 50);
    let client = issuer.client();
    let token = issuer.access_token(json!({}));
    let (a, b, c) = tokio::join!(
        client.verify_access_token(&token),
        client.verify_access_token(&token),
        client.verify_access_token(&token)
    );
    assert!(a.is_ok() && b.is_ok() && c.is_ok());
    assert_eq!(issuer.seen(JWKS).len(), 1);
}

#[tokio::test]
async fn an_unknown_key_id_fetches_the_key_set_at_most_every_thirty_seconds() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap();

    // The provider starts signing with a new key.
    let rotated = new_key();
    let jwk = json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "x": URL_SAFE_NO_PAD.encode(rotated.verifying_key().to_bytes()),
        "kid": "k2",
    });
    let published = json!({ "keys": [issuer.jwk(), jwk] });
    issuer.on(JWKS, move |_| json_reply(200, published.clone()));
    let header = json!({ "alg": "EdDSA", "typ": "at+jwt", "kid": "k2" });
    let token = sign_with(&rotated, &header, &Value::Object(issuer.access_claims()));

    // Made-up key ids cannot make every request fetch.
    issuer.set_now(t0() + 29 * SECOND);
    assert_eq!(client.verify_access_token(&token).await.unwrap_err(), invalid_token("signed with an unknown key"));
    assert_eq!(issuer.seen(JWKS).len(), 1);

    issuer.set_now(t0() + 30 * SECOND);
    assert_eq!(client.verify_access_token(&token).await.unwrap().sub, "usr_alice");
    assert_eq!(issuer.seen(JWKS).len(), 2);
    // The old key is still published, and still good.
    client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap();

    // A key id nobody publishes is refused, and asked about once per interval.
    let unknown =
        issuer.sign(json!({ "alg": "EdDSA", "typ": "at+jwt", "kid": "k9" }), Value::Object(issuer.access_claims()));
    assert_eq!(client.verify_access_token(&unknown).await.unwrap_err(), invalid_token("signed with an unknown key"));
    assert_eq!(issuer.seen(JWKS).len(), 2);
    issuer.set_now(t0() + 61 * SECOND);
    assert_eq!(client.verify_access_token(&unknown).await.unwrap_err(), invalid_token("signed with an unknown key"));
    assert_eq!(issuer.seen(JWKS).len(), 3);
}

#[tokio::test]
async fn a_key_the_provider_withdrew_stops_verifying_once_the_set_is_fetched_again() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    let token = issuer.access_token(json!({ "exp": t0() / 1000 + 7200 }));
    client.verify_access_token(&token).await.unwrap();

    issuer.on(JWKS, |_| json_reply(200, json!({ "keys": [] })));
    issuer.set_now(t0() + 59 * MINUTE);
    client.verify_access_token(&token).await.unwrap();
    issuer.set_now(t0() + 60 * MINUTE);
    assert_eq!(client.verify_access_token(&token).await.unwrap_err(), invalid_token("signed with an unknown key"));
}

#[tokio::test]
async fn verification_carries_on_with_the_keys_it_has_while_the_provider_is_unreachable() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap();

    issuer.set_down(true);
    // Past the hour the set is due, cannot be had, and the old one still serves.
    issuer.set_now(t0() + 3 * 60 * MINUTE);
    assert_eq!(client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap().sub, "usr_alice");
    // A key that was never seen is not excused by the outage.
    let unknown =
        issuer.sign(json!({ "alg": "EdDSA", "typ": "at+jwt", "kid": "k9" }), Value::Object(issuer.access_claims()));
    assert_eq!(client.verify_access_token(&unknown).await.unwrap_err(), invalid_token("signed with an unknown key"));

    issuer.set_down(false);
    issuer.on(JWKS, |_| json_reply(503, json!({})));
    issuer.set_now(t0() + 4 * 60 * MINUTE);
    assert_eq!(client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap().sub, "usr_alice");
}

#[tokio::test]
async fn without_any_key_set_an_unreachable_provider_is_unavailable_for_thirty_seconds_at_a_time() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    let no_keys = unavailable("auth's signing keys are unavailable");

    issuer.set_down(true);
    assert_eq!(client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap_err(), no_keys);
    issuer.set_down(false);

    // Back up, but the failed attempt counts toward the interval: nothing is asked.
    issuer.set_now(t0() + 29 * SECOND);
    assert_eq!(client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap_err(), no_keys);
    assert!(issuer.calls().is_empty());
    // A token that fails before its key is needed is refused all the same.
    assert_eq!(client.verify_access_token("not-a-token").await.unwrap_err(), invalid_token("not a JWT"));

    issuer.set_now(t0() + 30 * SECOND);
    assert_eq!(client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap().sub, "usr_alice");
    assert_eq!(issuer.seen(JWKS).len(), 1);
}

#[tokio::test]
async fn a_key_endpoint_that_fails_is_unavailable_and_a_refused_discovery_document_is_a_fault() {
    for reply in [json_reply(502, json!({})), text_reply(404, "Not Found"), text_reply(200, "keys")] {
        let issuer = FakeIssuer::start().await;
        let client = issuer.client();
        issuer.on(JWKS, move |_| reply.clone());
        assert_eq!(
            client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap_err(),
            unavailable("auth's signing keys are unavailable")
        );
    }

    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    issuer.set_discovery("issuer", json!("https://someone-else.example.test"));
    let token = issuer.access_token(json!({ "iss": FAKE_ISSUER }));
    assert_eq!(
        client.verify_access_token(&token).await.unwrap_err(),
        other("the discovery document names a different issuer")
    );
}

#[tokio::test]
async fn a_key_set_without_usable_keys_verifies_nothing() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let mut for_encryption = issuer.jwk();
    for_encryption["use"] = json!("enc");
    let mut another_alg = issuer.jwk();
    another_alg["alg"] = json!("ES256");
    let entries = json!({ "keys": [null, "text", { "kty": "RSA", "kid": "k1" }, for_encryption, another_alg] });
    issuer.on(JWKS, move |_| json_reply(200, entries.clone()));
    assert_eq!(
        client.verify_access_token(&issuer.access_token(json!({}))).await.unwrap_err(),
        invalid_token("signed with an unknown key")
    );

    let no_list = FakeIssuer::start().await;
    no_list.on(JWKS, |_| json_reply(200, json!({ "keys": "none" })));
    assert_eq!(
        no_list.client().verify_access_token(&no_list.access_token(json!({}))).await.unwrap_err(),
        invalid_token("signed with an unknown key")
    );
}

// ---------------------------------------------------------------------------
// Refresh and revocation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refresh_exchanges_the_refresh_token_for_a_verified_access_token() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let refreshed = client.refresh("rt_alice").await.unwrap();
    assert_eq!(refreshed.claims.sub, "usr_alice");
    assert_eq!(refreshed.claims.sid, "ses_alice");
    assert_eq!(refreshed.refresh_token, None);
    assert_eq!(refreshed.access_token.split('.').count(), 3);

    let call = &issuer.seen(TOKEN)[0];
    assert_eq!(
        call.form,
        [
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), "rt_alice".to_string())
        ]
    );
    assert_eq!(call.header("authorization"), Some(basic_credentials().as_str()));

    let token = issuer.access_token(json!({}));
    issuer.on_token(move |_| json_reply(200, json!({ "access_token": token, "refresh_token": "rt_next" })));
    assert_eq!(client.refresh("rt_alice").await.unwrap().refresh_token.as_deref(), Some("rt_next"));
    let token = issuer.access_token(json!({}));
    issuer.on_token(move |_| json_reply(200, json!({ "access_token": token, "refresh_token": 42 })));
    assert_eq!(client.refresh("rt_alice").await.unwrap().refresh_token, None);
}

#[tokio::test]
async fn refresh_tells_a_refused_token_from_a_provider_that_cannot_answer() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let cases: Vec<(Reply, AuthError)> = vec![
        (
            json_reply(400, json!({ "error": "invalid_grant" })),
            AuthError::InvalidGrant("the provider refused the grant".into()),
        ),
        (json_reply(503, json!({})), unavailable("auth answered 503")),
        (json_reply(429, json!({})), unavailable("the token endpoint is rate limiting this client")),
        (
            json_reply(200, json!({ "token_type": "Bearer" })),
            unavailable("the token endpoint answered without an access token"),
        ),
        (
            json_reply(401, json!({ "error": "invalid_client" })),
            other("the token endpoint refused the request (invalid_client)"),
        ),
        (json_reply(200, json!({ "access_token": "opaque" })), invalid_token("not a JWT")),
    ];
    for (reply, expected) in cases {
        let answer = reply.clone();
        issuer.on_token(move |_| answer.clone());
        assert_eq!(client.refresh("rt_alice").await.unwrap_err(), expected, "{reply:?}");
    }
    let expired = issuer.access_token(json!({ "exp": 1 }));
    issuer.on_token(move |_| json_reply(200, json!({ "access_token": expired })));
    assert_eq!(client.refresh("rt_alice").await.unwrap_err(), invalid_token("expired"));

    issuer.set_down(true);
    assert_eq!(client.refresh("rt_alice").await.unwrap_err(), unavailable("auth is unreachable"));
}

#[tokio::test]
async fn revoke_posts_the_refresh_token_to_the_revocation_endpoint() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    client.revoke("rt_alice").await.unwrap();
    let call = &issuer.seen(REVOKE)[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.body, "token=rt_alice&token_type_hint=refresh_token");
    assert_eq!(call.header("authorization"), Some(basic_credentials().as_str()));
    assert_eq!(call.header("content-type"), Some("application/x-www-form-urlencoded"));

    issuer.on_revoke(|_| json_reply(400, json!({ "error": "unsupported_token_type" })));
    assert_eq!(client.revoke("rt_alice").await.unwrap_err(), other("revocation refused the request (400)"));
    issuer.on_revoke(|_| json_reply(503, json!({})));
    assert_eq!(client.revoke("rt_alice").await.unwrap_err(), unavailable("auth answered 503"));
    // The answer's body is not read: anything in the 200s is done.
    issuer.on_revoke(|_| text_reply(204, ""));
    assert_eq!(client.revoke("rt_alice").await, Ok(()));

    let without = FakeIssuer::start().await;
    without.set_discovery("revocation_endpoint", Value::Null);
    assert_eq!(
        without.client().revoke("rt_alice").await.unwrap_err(),
        other("the provider has no revocation endpoint")
    );
    assert!(without.seen(REVOKE).is_empty());
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

fn active(scope: &str) -> Reply {
    json_reply(
        200,
        json!({ "active": true, "sub": "usr_alice", "username": "alice", "role": "user", "scope": scope, "jti": "apw_1" }),
    )
}

fn alice_answer() -> Introspection {
    Introspection {
        sub: "usr_alice".into(),
        username: "alice".into(),
        role: "user".into(),
        scopes: vec!["imap".into(), "smtp".into()],
        id: Some("apw_1".into()),
    }
}

#[tokio::test]
async fn introspect_asks_the_provider_about_a_secret() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    issuer.on_introspect(|_| active("imap smtp"));
    assert_eq!(client.introspect("abcd-efgh", Some("203.0.113.7")).await.unwrap(), Some(alice_answer()));

    let call = &issuer.seen(INTROSPECT)[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.form, [("token".to_string(), "abcd-efgh".to_string())]);
    assert_eq!(call.header("x-real-ip"), Some("203.0.113.7"));
    assert_eq!(call.header("authorization"), Some(basic_credentials().as_str()));
    assert_eq!(call.header("content-type"), Some("application/x-www-form-urlencoded"));
    assert_eq!(call.header("accept"), Some("application/json"));

    // Without an address, none is named.
    issuer.on_introspect(|_| {
        json_reply(200, json!({ "active": true, "sub": "usr_bob", "username": "bob", "role": "admin", "scope": " imap  smtp\tx ", "jti": 7 }))
    });
    let bob = client.introspect("another", None).await.unwrap().unwrap();
    assert_eq!(issuer.seen(INTROSPECT)[1].header("x-real-ip"), None);
    // Scopes are separated by single spaces only, and an id is a string or nothing.
    assert_eq!(bob.scopes, ["imap", "smtp\tx"]);
    assert_eq!(bob.id, None);
    assert_eq!(bob.role, "admin");

    issuer
        .on_introspect(|_| json_reply(200, json!({ "active": true, "sub": "usr_c", "username": "c", "role": "user" })));
    assert_eq!(client.introspect("no-scope", None).await.unwrap().unwrap().scopes, Vec::<String>::new());
}

#[tokio::test]
async fn introspect_is_for_confidential_clients_and_real_addresses() {
    let issuer = FakeIssuer::start().await;
    let public = issuer.client_with(|options| options.client_secret = None);
    assert_eq!(
        public.introspect("secret", None).await.unwrap_err(),
        other("introspection needs a confidential client")
    );
    let client = issuer.client();
    for not_ip in ["unknown", "", "203.0.113.256"] {
        assert_eq!(client.introspect("secret", Some(not_ip)).await.unwrap_err(), other("ip must be an IP address"));
    }
    assert!(issuer.calls().is_empty());
}

#[tokio::test]
async fn introspect_reads_anything_but_an_active_answer_as_no() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    assert_eq!(client.introspect("secret", None).await, Ok(None));
    for not_active in [json!({ "active": "true" }), json!({ "active": 1 }), json!({}), json!({ "active": null })] {
        issuer.on_introspect(move |_| json_reply(200, not_active.clone()));
        assert_eq!(client.introspect("secret", None).await, Ok(None));
    }
    // A provider that is rate limiting the question has not answered it.
    issuer.on_introspect(|_| text_reply(429, "slow down"));
    assert_eq!(client.introspect("secret", None).await, Ok(None));
}

#[tokio::test]
async fn introspect_fails_when_the_provider_cannot_or_will_not_answer() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let cases: Vec<(Reply, AuthError)> = vec![
        (json_reply(502, json!({})), unavailable("auth answered 502")),
        (text_reply(200, "OK"), unavailable("auth answered 200 with a malformed body")),
        (text_reply(401, "Unauthorized"), unavailable("auth answered 401 with a malformed body")),
        (
            json_reply(401, json!({ "error": "invalid_client" })),
            other("introspection refused the request (invalid_client)"),
        ),
        (json_reply(403, json!({})), other("introspection refused the request (403)")),
        (
            json_reply(200, json!({ "active": true, "username": "alice", "role": "user" })),
            unavailable("introspection answered without sub, username or role"),
        ),
        (
            json_reply(200, json!({ "active": true, "sub": "usr_alice", "username": "alice", "role": 1 })),
            unavailable("introspection answered without sub, username or role"),
        ),
    ];
    for (reply, expected) in cases {
        let answer = reply.clone();
        issuer.on_introspect(move |_| answer.clone());
        assert_eq!(client.introspect("secret", None).await.unwrap_err(), expected, "{reply:?}");
    }
    issuer.set_down(true);
    assert_eq!(client.introspect("secret", None).await.unwrap_err(), unavailable("auth is unreachable"));

    let without = FakeIssuer::start().await;
    without.set_discovery("introspection_endpoint", Value::Null);
    assert_eq!(
        without.client().introspect("secret", None).await.unwrap_err(),
        other("the provider has no introspection endpoint")
    );
}

#[tokio::test]
async fn introspect_serves_an_answer_from_memory_for_a_minute() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    issuer.on_introspect(|_| active("imap smtp"));
    assert_eq!(client.introspect("secret", None).await.unwrap(), Some(alice_answer()));

    // Whatever the provider would say now.
    issuer.on_introspect(|_| json_reply(200, json!({ "active": false })));
    issuer.set_now(t0() + MINUTE - 1);
    assert_eq!(client.introspect("secret", None).await.unwrap(), Some(alice_answer()));
    assert_eq!(issuer.seen(INTROSPECT).len(), 1);
    // The answer is kept by secret: another secret is another question.
    assert_eq!(client.introspect("another", None).await, Ok(None));
    assert_eq!(issuer.seen(INTROSPECT).len(), 2);

    issuer.set_now(t0() + MINUTE);
    assert_eq!(client.introspect("secret", None).await, Ok(None));
    assert_eq!(issuer.seen(INTROSPECT).len(), 3);
    // An inactive answer is not remembered, and takes the old one with it.
    issuer.set_down(true);
    assert_eq!(client.introspect("secret", None).await.unwrap_err(), unavailable("auth is unreachable"));
}

#[tokio::test]
async fn introspect_serves_a_stale_answer_for_ten_minutes_while_the_provider_is_unreachable() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    issuer.on_introspect(|_| active("imap smtp"));
    client.introspect("secret", None).await.unwrap();

    issuer.set_down(true);
    issuer.set_now(t0() + 10 * MINUTE - 1);
    assert_eq!(client.introspect("secret", None).await.unwrap(), Some(alice_answer()));
    issuer.set_now(t0() + 10 * MINUTE);
    assert_eq!(client.introspect("secret", None).await.unwrap_err(), unavailable("auth is unreachable"));

    // A failing provider is as good as an unreachable one; a refusing one is not.
    issuer.set_down(false);
    issuer.set_now(t0() + 5 * MINUTE);
    issuer.on_introspect(|_| json_reply(503, json!({})));
    assert_eq!(client.introspect("secret", None).await.unwrap(), Some(alice_answer()));
    issuer.on_introspect(|_| json_reply(401, json!({ "error": "invalid_client" })));
    assert_eq!(
        client.introspect("secret", None).await.unwrap_err(),
        other("introspection refused the request (invalid_client)")
    );
    // Rate limited is a no, even with an answer in memory.
    issuer.on_introspect(|_| text_reply(429, ""));
    assert_eq!(client.introspect("secret", None).await, Ok(None));

    // A fresh answer restarts both clocks.
    issuer.on_introspect(|_| active("imap smtp"));
    assert_eq!(client.introspect("secret", None).await.unwrap(), Some(alice_answer()));
    issuer.set_down(true);
    issuer.set_now(t0() + 14 * MINUTE);
    assert_eq!(client.introspect("secret", None).await.unwrap(), Some(alice_answer()));
}

#[tokio::test]
async fn forget_drops_the_answers_about_a_person_or_a_credential() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    issuer.on_introspect(|call| {
        let (sub, id) = match call.form("token") {
            Some("alice-1") => ("usr_alice", "apw_1"),
            Some("alice-2") => ("usr_alice", "apw_2"),
            _ => ("usr_bob", "apw_3"),
        };
        json_reply(
            200,
            json!({ "active": true, "sub": sub, "username": sub, "role": "user", "scope": "imap", "jti": id }),
        )
    });
    for secret in ["alice-1", "alice-2", "bob-1"] {
        client.introspect(secret, None).await.unwrap();
    }
    assert_eq!(issuer.seen(INTROSPECT).len(), 3);
    let asked_again = |secret: &'static str| {
        let (client, issuer) = (client.clone(), &issuer);
        async move {
            let before = issuer.seen(INTROSPECT).len();
            client.introspect(secret, None).await.unwrap();
            issuer.seen(INTROSPECT).len() > before
        }
    };

    client.forget(None, None);
    client.forget(Some("usr_nobody"), Some("apw_none"));
    assert!(!asked_again("alice-1").await && !asked_again("alice-2").await && !asked_again("bob-1").await);

    client.forget(None, Some("apw_2"));
    assert!(!asked_again("alice-1").await);
    assert!(asked_again("alice-2").await);

    client.forget(Some("usr_alice"), None);
    assert!(asked_again("alice-1").await && asked_again("alice-2").await);
    assert!(!asked_again("bob-1").await);
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

fn event(event_type: &str) -> AuthEvent {
    AuthEvent { event_type: event_type.into(), jti: "evt_1".into(), ..Default::default() }
}

#[tokio::test]
async fn read_event_returns_the_event_a_signed_token_carries() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let read = |events: Value| {
        let (client, token) = (client.clone(), issuer.event_token(events, json!({})));
        async move { client.read_event(&token).await.unwrap() }
    };
    let some = |text: &str| Some(text.to_string());

    assert_eq!(
        read(json!({ "session.ended": { "sid": "ses_alice", "sub": "ignored" } })).await,
        AuthEvent { sid: some("ses_alice"), ..event("session.ended") }
    );
    assert_eq!(
        read(json!({ "access.changed": { "sub": "usr_alice", "role": null } })).await,
        AuthEvent { sub: some("usr_alice"), role: Some(None), ..event("access.changed") }
    );
    assert_eq!(
        read(json!({ "access.changed": { "sub": "usr_alice", "role": "admin" } })).await,
        AuthEvent { sub: some("usr_alice"), role: Some(some("admin")), ..event("access.changed") }
    );
    assert_eq!(
        read(json!({ "user.disabled": { "sub": "usr_alice" } })).await,
        AuthEvent { sub: some("usr_alice"), ..event("user.disabled") }
    );
    assert_eq!(
        read(json!({ "user.deleted": { "sub": "usr_alice" } })).await,
        AuthEvent { sub: some("usr_alice"), ..event("user.deleted") }
    );
    assert_eq!(
        read(json!({ "invite.accepted": { "invite": "inv_1", "sub": "usr_bob" } })).await,
        AuthEvent { sub: some("usr_bob"), invite: some("inv_1"), ..event("invite.accepted") }
    );
    assert_eq!(
        read(json!({ "credential.revoked": { "sub": "usr_alice", "id": "apw_1" } })).await,
        AuthEvent { sub: some("usr_alice"), id: some("apw_1"), ..event("credential.revoked") }
    );
    // A type it does not know comes back with its name only, whatever it carries.
    assert_eq!(read(json!({ "something.new": { "sub": "usr_alice", "sid": "s" } })).await, event("something.new"));
    assert_eq!(read(json!({ "something.new": {} })).await, event("something.new"));
}

#[tokio::test]
async fn read_event_takes_a_token_with_white_space_around_it_and_no_expiry() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    let token = issuer.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), json!({}));
    assert!(client.read_event(&format!("\r\n \u{feff}{token}\n\t ")).await.is_ok());

    // Only the signature dates an event: no expiry is needed, and none is checked.
    let now_s = issuer.now_s();
    for claims in [json!({ "exp": 1 }), json!({ "iat": now_s - 365 * 24 * 3600 }), json!({ "iat": now_s + 30 })] {
        let token = issuer.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), claims);
        assert!(client.read_event(&token).await.is_ok());
    }
    let future = issuer.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), json!({ "iat": now_s + 31 }));
    assert_eq!(client.read_event(&future).await.unwrap_err(), invalid_token("issued in the future"));
}

#[tokio::test]
async fn read_event_refuses_anything_that_is_not_one_well_formed_event() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let refusal = |token: String| {
        let client = client.clone();
        async move { client.read_event(&token).await.expect_err("the event is refused") }
    };
    for not_a_token in ["", "   ", "not-a-token", "{}"] {
        assert_eq!(refusal(not_a_token.to_string()).await, invalid_token("not a JWT"));
    }
    assert_eq!(refusal(issuer.access_token(json!({}))).await, invalid_token("unexpected typ"));
    let disabled = json!({ "user.disabled": { "sub": "usr_alice" } });
    assert_eq!(
        refusal(issuer.event_token(disabled.clone(), json!({ "aud": "another-service" }))).await,
        invalid_token("wrong audience")
    );
    assert_eq!(
        refusal(issuer.event_token(disabled.clone(), json!({ "iss": "https://someone-else.example.test" }))).await,
        invalid_token("wrong issuer")
    );
    let stranger = new_key();
    let header = json!({ "alg": "EdDSA", "typ": "secevent+jwt", "kid": "k1" });
    let forged = sign_with(&stranger, &header, &Value::Object(issuer.event_claims(disabled.clone())));
    assert_eq!(refusal(forged).await, invalid_token("bad signature"));

    for jti in [Value::Null, json!(""), json!(7)] {
        let token = issuer.event_token(disabled.clone(), json!({ "jti": jti }));
        assert_eq!(refusal(token).await, invalid_token("no jti"));
    }
    for events in [
        Value::Null,
        json!([{ "user.disabled": { "sub": "usr_alice" } }]),
        json!("user.disabled"),
        json!({}),
        json!({ "user.disabled": { "sub": "usr_alice" }, "session.ended": { "sid": "s" } }),
    ] {
        let token = issuer.event_token(events.clone(), json!({}));
        assert_eq!(refusal(token).await, invalid_token("a SET carries exactly one event"), "{events}");
    }
    for payload in [Value::Null, json!("usr_alice"), json!(["usr_alice"]), json!(true)] {
        for event_type in ["user.disabled", "something.new"] {
            let token = issuer.event_token(json!({ event_type: payload.clone() }), json!({}));
            assert_eq!(refusal(token).await, invalid_token("malformed event payload"));
        }
    }
    let missing: Vec<(Value, &str)> = vec![
        (json!({ "session.ended": { "sub": "usr_alice" } }), "session.ended without sid"),
        (json!({ "session.ended": { "sid": 7 } }), "session.ended without sid"),
        (json!({ "access.changed": { "role": null } }), "access.changed without sub"),
        (json!({ "access.changed": { "sub": "usr_alice" } }), "access.changed without role"),
        (json!({ "access.changed": { "sub": "usr_alice", "role": 0 } }), "access.changed without role"),
        (json!({ "user.disabled": {} }), "user.disabled without sub"),
        (json!({ "user.deleted": { "sub": null } }), "user.deleted without sub"),
        (json!({ "credential.revoked": { "sub": "usr_alice" } }), "credential.revoked without id"),
        (json!({ "credential.revoked": { "id": "apw_1" } }), "credential.revoked without sub"),
        (json!({ "invite.accepted": { "sub": "usr_bob" } }), "invite.accepted without invite"),
        (json!({ "invite.accepted": { "invite": "inv_1" } }), "invite.accepted without sub"),
    ];
    for (events, message) in missing {
        let token = issuer.event_token(events.clone(), json!({}));
        assert_eq!(refusal(token).await, invalid_token(message), "{events}");
    }
    // A type named like a member of every JavaScript object is a fault, as it is for the Bun server.
    for event_type in ["constructor", "toString", "__proto__"] {
        let token = issuer.event_token(json!({ event_type: { "sub": "usr_alice" } }), json!({}));
        assert!(matches!(refusal(token).await, AuthError::Other(_)), "{event_type}");
    }
}

#[tokio::test]
async fn read_event_is_unavailable_while_the_keys_cannot_be_fetched() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    let token = issuer.event_token(json!({ "user.disabled": { "sub": "usr_alice" } }), json!({}));
    issuer.set_down(true);
    assert_eq!(client.read_event(&token).await.unwrap_err(), unavailable("auth's signing keys are unavailable"));
    // What is not a token needs no key to be refused.
    assert_eq!(client.read_event("not-a-token").await.unwrap_err(), invalid_token("not a JWT"));
}

#[tokio::test]
async fn read_event_forgets_the_answers_the_event_is_about() {
    let issuer = FakeIssuer::start().await;
    issuer.set_now(t0());
    let client = issuer.client();
    let asked = || issuer.seen(INTROSPECT).len();
    issuer.on_introspect(|_| active("imap smtp"));

    for (events, forgets) in [
        (json!({ "credential.revoked": { "sub": "usr_someone", "id": "apw_1" } }), true),
        (json!({ "credential.revoked": { "sub": "usr_alice", "id": "apw_other" } }), false),
        (json!({ "access.changed": { "sub": "usr_alice", "role": "admin" } }), true),
        (json!({ "user.disabled": { "sub": "usr_alice" } }), true),
        (json!({ "user.deleted": { "sub": "usr_alice" } }), true),
        (json!({ "user.deleted": { "sub": "usr_bob" } }), false),
        (json!({ "session.ended": { "sid": "ses_alice" } }), false),
        (json!({ "invite.accepted": { "invite": "inv_1", "sub": "usr_alice" } }), false),
        (json!({ "something.new": { "sub": "usr_alice", "id": "apw_1" } }), false),
    ] {
        client.introspect("secret", None).await.unwrap();
        let before = asked();
        client.read_event(&issuer.event_token(events.clone(), json!({}))).await.unwrap();
        client.introspect("secret", None).await.unwrap();
        assert_eq!(asked() > before, forgets, "{events}");
    }
}

// ---------------------------------------------------------------------------
// The logout URL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logout_url_is_the_providers_end_session_endpoint_for_this_client() {
    let issuer = FakeIssuer::start().await;
    let client = issuer.client();
    assert_eq!(
        client.logout_url(Some("https://mail.example.test/")).await.unwrap(),
        format!("{FAKE_ISSUER}/logout?client_id=mail&post_logout_redirect_uri=https%3A%2F%2Fmail.example.test%2F")
    );
    assert_eq!(client.logout_url(None).await.unwrap(), format!("{FAKE_ISSUER}/logout?client_id=mail"));
    // It is built, not fetched.
    assert_eq!(issuer.calls().len(), 1);

    let with_query = FakeIssuer::start().await;
    with_query.set_discovery(
        "end_session_endpoint",
        json!(format!("{FAKE_ISSUER}/logout?ui=mini%20one&client_id=old&client_id=older#bye")),
    );
    assert_eq!(
        with_query.client().logout_url(Some("https://mail.example.test/")).await.unwrap(),
        format!("{FAKE_ISSUER}/logout?ui=mini+one&client_id=mail&post_logout_redirect_uri=https%3A%2F%2Fmail.example.test%2F#bye")
    );

    let without = FakeIssuer::start().await;
    without.set_discovery("end_session_endpoint", Value::Null);
    assert_eq!(without.client().logout_url(None).await.unwrap_err(), other("the provider has no end_session_endpoint"));
    let down = FakeIssuer::start().await;
    down.set_down(true);
    assert_eq!(down.client().logout_url(None).await.unwrap_err(), unavailable("auth is unreachable"));
}
