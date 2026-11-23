//! The session a request carries, persisted in PostgreSQL and keyed by a
//! signed `connect.sid` cookie.
//!
//! Every request that arrives with a live session saves it again and gets
//! the cookie back with a renewed expiry, so a session lasts seven days past
//! its last use. A request without one gets neither a row nor a cookie until
//! a route attaches a user.

use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::js;
use crate::models::{RuntimeCookie, RuntimeSession, SignedUser, SsoSession};
use crate::postgres::repositories::sessions::PostgresSessionStore;
use crate::postgres::DbError;

pub const SESSION_COOKIE_NAME: &str = "connect.sid";

const SESSION_MAX_AGE_MS: i64 = 1000 * 60 * 60 * 24 * 7;

fn sign(value: &str, secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(value.as_bytes());
    let digest = base64::engine::general_purpose::STANDARD_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{value}.{digest}")
}

/// The session id inside a signed cookie value (`s:<id>.<signature>`), or
/// `None` when the value is unsigned or its signature does not verify.
pub fn unsign_session_cookie(value: &str, secret: &str) -> Option<String> {
    let input = value.strip_prefix("s:")?;
    // `slice(0, lastIndexOf("."))`: with no dot at all, that is all but the last character.
    let candidate = match input.rfind('.') {
        Some(at) => &input[..at],
        None => {
            let mut chars = input.chars();
            chars.next_back();
            chars.as_str()
        }
    };
    let expected = sign(candidate, secret);
    (expected.len() == input.len() && bool::from(expected.as_bytes().ct_eq(input.as_bytes())))
        .then(|| candidate.to_string())
}

pub fn sign_session_cookie(session_id: &str, secret: &str) -> String {
    format!("s:{}", sign(session_id, secret))
}

/// The first cookie of `name` in a `Cookie` header, unquoted and decoded.
pub fn read_cookie(header: Option<&str>, name: &str) -> Option<String> {
    for pair in header?.split(';') {
        let Some(eq) = pair.find('=') else { continue };
        if js::trim(&pair[..eq]) != name {
            continue;
        }
        let mut value = js::trim(&pair[eq + 1..]);
        if value.len() > 1 && value.starts_with('"') && value.ends_with('"') {
            value = &value[1..value.len() - 1];
        }
        return Some(js::decode_uri_component(value).unwrap_or_else(|| value.to_string()));
    }
    None
}

fn generate_session_id() -> String {
    let bytes: [u8; 24] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub secret: String,
    /// Sets the cookie's `Secure` attribute, and withholds it from a plain-HTTP request.
    pub secure: bool,
}

type Fingerprint = (Option<SignedUser>, Option<SsoSession>);

pub struct HttpSession {
    /// The authenticated identity; `None` while the session is anonymous.
    pub user: Option<SignedUser>,
    /// What a session started through single sign-on keeps from the provider.
    pub auth: Option<SsoSession>,
    id: String,
    cookie: RuntimeCookie,
    destroyed: bool,
    arrived_with_id: Option<String>,
    original_id: String,
    original_fingerprint: Fingerprint,
    store: PostgresSessionStore,
    options: SessionOptions,
}

fn fresh_cookie(options: &SessionOptions) -> RuntimeCookie {
    RuntimeCookie {
        original_max_age: Some(SESSION_MAX_AGE_MS),
        max_age: None,
        signed: None,
        expires: Some(Utc::now() + Duration::milliseconds(SESSION_MAX_AGE_MS)),
        http_only: Some(true),
        path: Some("/".to_string()),
        domain: None,
        secure: Some(serde_json::Value::Bool(options.secure)),
        same_site: Some(serde_json::Value::String("strict".to_string())),
    }
}

impl HttpSession {
    /// Resolves the session named by the request's cookie, or starts an
    /// anonymous one. A store fault is an error.
    pub async fn load(
        cookie_header: Option<&str>,
        store: PostgresSessionStore,
        options: SessionOptions,
    ) -> Result<HttpSession, DbError> {
        let raw = read_cookie(cookie_header, SESSION_COOKIE_NAME).filter(|raw| !raw.is_empty());
        let cookie_id = raw.and_then(|raw| unsign_session_cookie(&raw, &options.secret)).filter(|id| !id.is_empty());
        let stored = match &cookie_id {
            Some(id) => store.get(id).await?,
            None => None,
        };
        let (id, user, auth, cookie) = match (stored, &cookie_id) {
            (Some(session), Some(id)) => (id.clone(), Some(session.user), session.auth, session.cookie),
            _ => (generate_session_id(), None, None, fresh_cookie(&options)),
        };
        Ok(HttpSession {
            original_fingerprint: (user.clone(), auth.clone()),
            original_id: id.clone(),
            arrived_with_id: cookie_id,
            user,
            auth,
            id,
            cookie,
            destroyed: false,
            store,
            options,
        })
    }

    /// The id the session is stored under, for log correlation.
    pub fn session_id(&self) -> &str {
        &self.id
    }

    fn snapshot(&self) -> RuntimeSession {
        let mut cookie = self.cookie.clone();
        cookie.max_age = cookie.expires.map(|expires| (expires - Utc::now()).num_milliseconds());
        RuntimeSession {
            // A session saved before any user was attached stores the placeholders.
            user: self.user.clone().unwrap_or_else(|| SignedUser::new(None, None, None)),
            cookie,
            auth: self.auth.clone(),
        }
    }

    fn is_modified(&self) -> bool {
        self.original_id != self.id || self.original_fingerprint != (self.user.clone(), self.auth.clone())
    }

    /// Deletes the stored session and continues under a new, empty one.
    pub async fn regenerate(&mut self) -> Result<(), DbError> {
        let previous = std::mem::replace(&mut self.id, generate_session_id());
        self.user = None;
        self.auth = None;
        self.cookie = fresh_cookie(&self.options);
        self.store.destroy(&previous).await
    }

    /// Writes the session now rather than when the response completes.
    pub async fn save(&self) -> Result<(), DbError> {
        self.store.set(&self.id, &self.snapshot()).await
    }

    /// Deletes the stored session. The response sets no cookie.
    pub async fn destroy(&mut self) -> Result<(), DbError> {
        self.destroyed = true;
        self.store.destroy(&self.id).await
    }

    /// Persists the session as the response completes and returns the
    /// `Set-Cookie` value it should carry, if any, plus the save's failure,
    /// which does not withhold the cookie. `secure_request` says whether the
    /// request arrived over HTTPS; a `Secure` cookie is never sent otherwise.
    pub async fn commit(&mut self, secure_request: bool) -> (Option<String>, Option<DbError>) {
        if self.destroyed {
            return (None, None);
        }
        let arrived_with_this_id = self.arrived_with_id.as_deref() == Some(self.id.as_str());
        if !(arrived_with_this_id || self.is_modified()) {
            return (None, None);
        }

        let max_age = self.cookie.original_max_age.unwrap_or(SESSION_MAX_AGE_MS);
        self.cookie.expires = Some(Utc::now() + Duration::milliseconds(max_age));
        let save_error = self.save().await.err();

        if self.cookie.is_secure() && !secure_request {
            return (None, save_error);
        }
        (Some(self.serialize_cookie()), save_error)
    }

    fn serialize_cookie(&self) -> String {
        let signed = sign_session_cookie(&self.id, &self.options.secret);
        let mut parts = vec![format!("{SESSION_COOKIE_NAME}={}", js::encode_uri_component(&signed))];
        if let Some(path) = self.cookie.path.as_deref().filter(|path| !path.is_empty()) {
            parts.push(format!("Path={path}"));
        }
        if let Some(expires) = self.cookie.expires {
            parts.push(format!("Expires={}", js::utc_string(&expires)));
        }
        if self.cookie.http_only == Some(true) {
            parts.push("HttpOnly".to_string());
        }
        if self.cookie.is_secure() {
            parts.push("Secure".to_string());
        }
        if let Some(same_site) = self.cookie.same_site_attribute() {
            parts.push(format!("SameSite={same_site}"));
        }
        parts.join("; ")
    }
}

/// When a stored cookie's expiry is next due, for tests and diagnostics.
pub fn expiry_of(session: &HttpSession) -> Option<DateTime<Utc>> {
    session.cookie.expires
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_cookies_round_trip() {
        let signed = sign_session_cookie("abc123", "secret");
        assert!(signed.starts_with("s:abc123."));
        assert_eq!(unsign_session_cookie(&signed, "secret").as_deref(), Some("abc123"));
        assert_eq!(unsign_session_cookie(&signed, "other"), None);
        assert_eq!(unsign_session_cookie("abc123", "secret"), None);
        assert_eq!(unsign_session_cookie("s:abc123", "secret"), None);
        assert_eq!(unsign_session_cookie(&format!("{signed}x"), "secret"), None);
    }

    #[test]
    fn signature_matches_the_bun_server() {
        // HMAC-SHA256("abc", key "secret"), base64 without padding.
        assert_eq!(sign("abc", "secret"), "abc.mUba1OAOkT/Ivo5dP34RCkqegy+D+wnDRShdeGONig4");
    }

    #[test]
    fn cookie_header_parsing() {
        assert_eq!(read_cookie(Some("a=1; connect.sid=s%3Ax.y; b=2"), "connect.sid").as_deref(), Some("s:x.y"));
        assert_eq!(read_cookie(Some("connect.sid=\"quoted\""), "connect.sid").as_deref(), Some("quoted"));
        assert_eq!(read_cookie(Some("connect.sid=%E0%A4%A"), "connect.sid").as_deref(), Some("%E0%A4%A"));
        assert_eq!(read_cookie(Some("other=1"), "connect.sid"), None);
        assert_eq!(read_cookie(None, "connect.sid"), None);
        assert_eq!(read_cookie(Some("connect.sid=a; connect.sid=b"), "connect.sid").as_deref(), Some("a"));
    }
}
