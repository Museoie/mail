//! A stand-in OpenID Connect provider for the single sign-on tests, after
//! `scripts/fake-issuer.ts`: discovery, an Ed25519 key set, and token,
//! introspection and revocation endpoints whose answers each test sets.
//!
//! It listens on a loopback port. The client is pointed at it with
//! [`FAKE_ISSUER`] as the issuer URL (what tokens carry in `iss`) and the
//! listener as the back channel, so every call it makes arrives here, over
//! real HTTP. While `down` is set a connection is dropped unanswered, which
//! the client reads as an unreachable provider.
//!
//! The signing keys are generated per test; no key material is stored.

// Each test binary uses its own part of this module.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bytes::Bytes;
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

use mail_server::auth_client::{AuthClient, AuthClientOptions, Clock};
use mail_server::sso::{build_sso, ClientOverrides, Sso, SsoEnv};

pub const FAKE_ISSUER: &str = "https://auth.example.test";
pub const FAKE_CLIENT_ID: &str = "mail";
pub const FAKE_CLIENT_SECRET: &str = "test_key_not_real_do_not_use";
pub const FAKE_REDIRECT_URI: &str = "https://mail.example.test/api/auth/callback";

/// One request the client made.
#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub path: String,
    /// Header names in lower case.
    pub headers: HashMap<String, String>,
    pub body: String,
    pub form: Vec<(String, String)>,
}

impl Call {
    pub fn form(&self, name: &str) -> Option<&str> {
        self.form.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// What an endpoint answers.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub location: Option<String>,
}

pub fn json_reply(status: u16, body: Value) -> Reply {
    Reply { status, body: body.to_string(), location: None }
}

pub fn text_reply(status: u16, body: &str) -> Reply {
    Reply { status, body: body.to_string(), location: None }
}

type Handler = Arc<dyn Fn(&Call) -> Reply + Send + Sync>;

struct State {
    issuer: Mutex<String>,
    key: SigningKey,
    calls: Mutex<Vec<Call>>,
    down: AtomicBool,
    /// Milliseconds since the epoch; 0 means the real clock.
    now: AtomicI64,
    handlers: Mutex<HashMap<String, Handler>>,
    /// Milliseconds to wait before answering a path.
    delays: Mutex<HashMap<String, u64>>,
    discovery: Mutex<Map<String, Value>>,
}

impl State {
    fn now_ms(&self) -> i64 {
        match self.now.load(Ordering::SeqCst) {
            0 => chrono::Utc::now().timestamp_millis(),
            set => set,
        }
    }

    fn jwk(&self) -> Value {
        json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "x": URL_SAFE_NO_PAD.encode(self.key.verifying_key().to_bytes()),
            "kid": "k1",
            "alg": "EdDSA",
            "use": "sig",
        })
    }

    fn answer(&self, call: &Call) -> Reply {
        if let Some(handler) = self.handlers.lock().unwrap().get(&call.path).cloned() {
            return handler(call);
        }
        match call.path.as_str() {
            "/.well-known/openid-configuration" => {
                json_reply(200, Value::Object(self.discovery.lock().unwrap().clone()))
            }
            "/.well-known/jwks.json" => json_reply(200, json!({ "keys": [self.jwk()] })),
            "/oauth/token" => json_reply(500, json!({})),
            "/oauth/introspect" => json_reply(200, json!({ "active": false })),
            "/oauth/revoke" => text_reply(200, ""),
            _ => json_reply(404, json!({ "error": "not_found" })),
        }
    }
}

async fn serve(state: Arc<State>, request: Request<Incoming>) -> Result<Response<Full<Bytes>>, &'static str> {
    if state.down.load(Ordering::SeqCst) {
        // Hyper closes the connection without an answer.
        return Err("the fake provider is down");
    }
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_string(), value.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = request.into_body().collect().await.map_err(|_| "unreadable body")?.to_bytes();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let form = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
    let call = Call { method, path, headers, body, form };
    state.calls.lock().unwrap().push(call.clone());
    let delay = state.delays.lock().unwrap().get(&call.path).copied();
    if let Some(delay) = delay {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let reply = state.answer(&call);
    let mut response = Response::builder().status(reply.status).header("Content-Type", "application/json");
    if let Some(location) = &reply.location {
        response = response.header("Location", location);
    }
    Ok(response.body(Full::new(Bytes::from(reply.body))).expect("a valid response"))
}

/// The fake provider. Dropping it stops nothing by itself; the listener ends
/// with the test's runtime.
pub struct FakeIssuer {
    state: Arc<State>,
    pub back_channel_url: String,
}

fn merged(mut base: Map<String, Value>, overrides: Value) -> Map<String, Value> {
    if let Value::Object(overrides) = overrides {
        for (name, value) in overrides {
            base.insert(name, value);
        }
    }
    base
}

fn segment(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(value.to_string())
}

/// Signs `header.payload` with `key`, whatever the header claims.
pub fn sign_with(key: &SigningKey, header: &Value, payload: &Value) -> String {
    let signed = format!("{}.{}", segment(header), segment(payload));
    let signature = key.sign(signed.as_bytes());
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

pub fn new_key() -> SigningKey {
    SigningKey::from_bytes(&rand::random::<[u8; 32]>())
}

impl FakeIssuer {
    pub async fn start() -> FakeIssuer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free loopback port");
        let port = listener.local_addr().expect("a bound listener").port();
        let state = Arc::new(State {
            issuer: Mutex::new(FAKE_ISSUER.to_string()),
            key: new_key(),
            calls: Mutex::new(Vec::new()),
            down: AtomicBool::new(false),
            now: AtomicI64::new(0),
            handlers: Mutex::new(HashMap::new()),
            delays: Mutex::new(HashMap::new()),
            discovery: Mutex::new(Map::new()),
        });
        let issuer = FakeIssuer { state: Arc::clone(&state), back_channel_url: format!("http://127.0.0.1:{port}") };
        issuer.set_issuer(FAKE_ISSUER);
        issuer.reset();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let service = service_fn(move |request| serve(Arc::clone(&state), request));
                    let connection =
                        hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service);
                    let _ = connection.await;
                });
            }
        });
        issuer
    }

    /// The issuer URL tokens carry and the discovery document names, with
    /// every endpoint under it.
    pub fn set_issuer(&self, issuer: &str) {
        *self.state.issuer.lock().unwrap() = issuer.to_string();
        let base = issuer.trim_end_matches('/');
        let at = |path: &str| Value::from(format!("{base}{path}"));
        let mut discovery = Map::new();
        discovery.insert("issuer".into(), Value::from(issuer));
        discovery.insert("authorization_endpoint".into(), at("/oauth/authorize"));
        discovery.insert("token_endpoint".into(), at("/oauth/token"));
        discovery.insert("revocation_endpoint".into(), at("/oauth/revoke"));
        discovery.insert("introspection_endpoint".into(), at("/oauth/introspect"));
        discovery.insert("jwks_uri".into(), at("/.well-known/jwks.json"));
        discovery.insert("end_session_endpoint".into(), at("/logout"));
        *self.state.discovery.lock().unwrap() = discovery;
    }

    pub fn issuer(&self) -> String {
        self.state.issuer.lock().unwrap().clone()
    }

    /// Changes one field of the discovery document; `Value::Null` removes it.
    pub fn set_discovery(&self, field: &str, value: Value) {
        let mut discovery = self.state.discovery.lock().unwrap();
        if value.is_null() {
            discovery.remove(field);
        } else {
            discovery.insert(field.to_string(), value);
        }
    }

    /// Back to the default answers, an empty call log, the real clock, and up.
    pub fn reset(&self) {
        self.state.calls.lock().unwrap().clear();
        self.state.down.store(false, Ordering::SeqCst);
        self.state.now.store(0, Ordering::SeqCst);
        self.state.handlers.lock().unwrap().clear();
        self.state.delays.lock().unwrap().clear();
        let state = Arc::clone(&self.state);
        self.on("/oauth/token", move |_| {
            let token = sign_access(&state, json!({}));
            json_reply(200, json!({ "access_token": token, "token_type": "Bearer" }))
        });
    }

    /// Answers every request to `path` with `handler` from now on.
    pub fn on(&self, path: &str, handler: impl Fn(&Call) -> Reply + Send + Sync + 'static) {
        self.state.handlers.lock().unwrap().insert(path.to_string(), Arc::new(handler));
    }

    pub fn on_token(&self, handler: impl Fn(&Call) -> Reply + Send + Sync + 'static) {
        self.on("/oauth/token", handler);
    }

    pub fn on_introspect(&self, handler: impl Fn(&Call) -> Reply + Send + Sync + 'static) {
        self.on("/oauth/introspect", handler);
    }

    pub fn on_revoke(&self, handler: impl Fn(&Call) -> Reply + Send + Sync + 'static) {
        self.on("/oauth/revoke", handler);
    }

    /// Makes `path` take `ms` milliseconds to answer.
    pub fn delay(&self, path: &str, ms: u64) {
        self.state.delays.lock().unwrap().insert(path.to_string(), ms);
    }

    pub fn set_down(&self, down: bool) {
        self.state.down.store(down, Ordering::SeqCst);
    }

    /// Fixes the clock the tokens are minted by, and the one [`FakeIssuer::clock`] reads.
    pub fn set_now(&self, ms: i64) {
        self.state.now.store(ms, Ordering::SeqCst);
    }

    pub fn now_ms(&self) -> i64 {
        self.state.now_ms()
    }

    pub fn now_s(&self) -> i64 {
        self.state.now_ms() / 1000
    }

    /// The clock to give the client, so it and the provider agree on the time.
    pub fn clock(&self) -> Clock {
        let state = Arc::clone(&self.state);
        Arc::new(move || state.now_ms())
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.calls.lock().unwrap().clone()
    }

    pub fn seen(&self, path: &str) -> Vec<Call> {
        self.calls().into_iter().filter(|call| call.path == path).collect()
    }

    pub fn clear_calls(&self) {
        self.state.calls.lock().unwrap().clear();
    }

    /// The options that point a client at this provider.
    pub fn options(&self) -> AuthClientOptions {
        AuthClientOptions {
            issuer: self.issuer(),
            back_channel_url: Some(self.back_channel_url.clone()),
            client_id: FAKE_CLIENT_ID.to_string(),
            client_secret: Some(FAKE_CLIENT_SECRET.to_string()),
            redirect_uri: FAKE_REDIRECT_URI.to_string(),
            audience: None,
            transport: None,
            now: Some(self.clock()),
        }
    }

    pub fn client(&self) -> AuthClient {
        AuthClient::new(self.options()).expect("valid options")
    }

    pub fn client_with(&self, change: impl FnOnce(&mut AuthClientOptions)) -> AuthClient {
        let mut options = self.options();
        change(&mut options);
        AuthClient::new(options).expect("valid options")
    }

    /// The environment that points single sign-on at this provider.
    pub fn env(&self) -> SsoEnv {
        SsoEnv {
            auth_issuer: Some(self.issuer()),
            auth_backchannel_url: Some(self.back_channel_url.clone()),
            auth_client_id: Some(FAKE_CLIENT_ID.to_string()),
            auth_client_secret: Some(FAKE_CLIENT_SECRET.to_string()),
            app_hostname: Some("mail.example.test".to_string()),
            ..Default::default()
        }
    }

    /// Single sign-on against this provider, without touching the process-wide one.
    pub fn sso(&self, change: impl FnOnce(&mut SsoEnv)) -> Sso {
        let mut env = self.env();
        change(&mut env);
        build_sso(&env, ClientOverrides { transport: None, now: Some(self.clock()) })
            .expect("valid settings")
            .expect("single sign-on is on")
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.state.key
    }

    pub fn jwk(&self) -> Value {
        self.state.jwk()
    }

    /// A token signed by the provider's key, with this header and payload.
    pub fn sign(&self, header: Value, payload: Value) -> String {
        sign_with(&self.state.key, &header, &payload)
    }

    /// The claims of alice's access token.
    pub fn access_claims(&self) -> Map<String, Value> {
        access_claims(&self.state)
    }

    /// The claims of alice's ID token, with nonce "n".
    pub fn id_claims(&self) -> Map<String, Value> {
        id_claims(&self.state)
    }

    /// An access token for alice; `claims` adds to or replaces hers.
    pub fn access_token(&self, claims: Value) -> String {
        sign_access(&self.state, claims)
    }

    /// An access token with exactly these claims.
    pub fn access_token_of(&self, claims: Map<String, Value>) -> String {
        self.sign(json!({ "alg": "EdDSA", "typ": "at+jwt", "kid": "k1" }), Value::Object(claims))
    }

    /// An ID token for alice; `claims` adds to or replaces hers.
    pub fn id_token(&self, claims: Value) -> String {
        sign_id(&self.state, claims)
    }

    /// An ID token with exactly these claims.
    pub fn id_token_of(&self, claims: Map<String, Value>) -> String {
        self.sign(json!({ "alg": "EdDSA", "typ": "JWT", "kid": "k1" }), Value::Object(claims))
    }

    /// The claims of a security event token carrying `events`.
    pub fn event_claims(&self, events: Value) -> Map<String, Value> {
        let payload = json!({
            "iss": self.issuer(),
            "aud": FAKE_CLIENT_ID,
            "iat": self.now_s(),
            "jti": "evt_1",
            "events": events,
        });
        payload.as_object().cloned().expect("an object")
    }

    /// A security event token carrying `events`; `claims` adds to or replaces its own.
    pub fn event_token(&self, events: Value, claims: Value) -> String {
        self.event_token_of(merged(self.event_claims(events), claims))
    }

    /// A security event token with exactly these claims.
    pub fn event_token_of(&self, claims: Map<String, Value>) -> String {
        self.sign(json!({ "alg": "EdDSA", "typ": "secevent+jwt", "kid": "k1" }), Value::Object(claims))
    }

    /// Makes the token endpoint answer the code exchange as the provider
    /// would: it checks the PKCE verifier against the authorize request, then
    /// issues tokens carrying that request's nonce. `ids` and `at` add to or
    /// replace the ID and access token claims.
    pub fn issue_for(&self, authorize_url: &str, ids: Value, at: Value) {
        let query: HashMap<String, String> =
            url::Url::parse(authorize_url).expect("an absolute URL").query_pairs().into_owned().collect();
        let state = Arc::clone(&self.state);
        self.on_token(move |call| {
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(call.form("code_verifier").unwrap_or("").as_bytes()));
            if call.form("code") != Some("c0de") || Some(&challenge) != query.get("code_challenge") {
                return json_reply(400, json!({ "error": "invalid_grant" }));
            }
            let nonce = json!({ "nonce": query.get("nonce") });
            let id_token = sign_id(&state, Value::Object(merged(merged(Map::new(), nonce), ids.clone())));
            json_reply(
                200,
                json!({
                    "access_token": sign_access(&state, at.clone()),
                    "id_token": id_token,
                    "refresh_token": "rt_alice",
                    "token_type": "Bearer",
                    "expires_in": 600,
                }),
            )
        });
    }
}

fn access_claims(state: &State) -> Map<String, Value> {
    let now_s = state.now_ms() / 1000;
    let claims = json!({
        "iss": state.issuer.lock().unwrap().clone(),
        "sub": "usr_alice",
        "aud": FAKE_CLIENT_ID,
        "iat": now_s,
        "exp": now_s + 600,
        "sid": "ses_alice",
        "username": "alice",
        "name": "Alice",
        "role": "user",
    });
    claims.as_object().cloned().expect("an object")
}

fn id_claims(state: &State) -> Map<String, Value> {
    let now_s = state.now_ms() / 1000;
    let claims = json!({
        "iss": state.issuer.lock().unwrap().clone(),
        "sub": "usr_alice",
        "aud": FAKE_CLIENT_ID,
        "iat": now_s,
        "exp": now_s + 600,
        "nonce": "n",
        "auth_time": now_s,
        "amr": ["pwd"],
        "email": "alice@example.com",
        "email_verified": true,
    });
    claims.as_object().cloned().expect("an object")
}

fn sign_access(state: &State, claims: Value) -> String {
    sign_with(
        &state.key,
        &json!({ "alg": "EdDSA", "typ": "at+jwt", "kid": "k1" }),
        &Value::Object(merged(access_claims(state), claims)),
    )
}

fn sign_id(state: &State, claims: Value) -> String {
    sign_with(
        &state.key,
        &json!({ "alg": "EdDSA", "typ": "JWT", "kid": "k1" }),
        &Value::Object(merged(id_claims(state), claims)),
    )
}

/// The value of the cookie `name` in a `Set-Cookie` string.
pub fn cookie_value(set_cookie: &str, name: &str) -> String {
    let pair = set_cookie.split(';').next().unwrap_or("");
    let (key, value) = pair.split_once('=').unwrap_or(("", ""));
    assert_eq!(key, name, "the Set-Cookie value sets another cookie");
    value.to_string()
}

/// A query parameter of `url`.
pub fn query_param(url: &str, name: &str) -> Option<String> {
    url::Url::parse(url).ok()?.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
}

/// The JSON a base64url segment holds.
pub fn decode_segment(segment: &str) -> Value {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segment).expect("base64url")).expect("JSON")
}

pub fn encode_segment(value: &Value) -> String {
    segment(value)
}
