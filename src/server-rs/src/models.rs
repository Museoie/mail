//! Models shared by the HTTP API, the session store and the mail servers.
//!
//! Field order is the order the API serializes them in.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::util::get_random_id;

/// An account as the user repository returns it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct User {
    pub id: Option<String>,
    pub email: Option<String>,
    pub username: Option<String>,
    /// The stored bcrypt digest.
    pub password: Option<String>,
    /// A sign-up or password-reset token in flight.
    pub token: Option<String>,
    /// When `token` stops being accepted, as an ISO timestamp.
    pub expiry: Option<String>,
    /// The credential generation the row is on.
    pub password_version: Option<i64>,
}

impl User {
    /// The identity a session carries, or `None` for an account that has not
    /// finished signing up.
    pub fn get_signed(&self) -> Option<SignedUser> {
        let filled = |field: &Option<String>| field.as_deref().is_some_and(|v| !v.is_empty());
        if !(filled(&self.id) && filled(&self.username) && filled(&self.password) && filled(&self.email)) {
            return None;
        }
        Some(SignedUser {
            id: self.id.clone()?,
            email: self.email.clone()?,
            username: self.username.clone()?,
            token: self.token.clone(),
            expiry: self.expiry.clone(),
            password_version: self.password_version,
            is_read_only: None,
            authenticated_as: None,
        })
    }
}

/// The authenticated identity of a session, and the body the login routes return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedUser {
    pub id: String,
    pub email: String,
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expiry: Option<String>,
    /// The credential generation the session was minted from. A grant minted
    /// before a password rotation fails its next comparison.
    #[serde(rename = "passwordVersion", skip_serializing_if = "Option::is_none", default)]
    pub password_version: Option<i64>,
    /// True when the session authenticated as the read-only account: every
    /// read sees the owner's data, every mutating surface refuses.
    #[serde(rename = "isReadOnly", skip_serializing_if = "Option::is_none", default)]
    pub is_read_only: Option<bool>,
    /// The username the caller authenticated as, before the read-only remap.
    #[serde(rename = "authenticatedAs", skip_serializing_if = "Option::is_none", default)]
    pub authenticated_as: Option<String>,
}

impl SignedUser {
    /// An identity with the placeholders the shared model fills in for
    /// missing fields.
    pub fn new(id: Option<String>, username: Option<String>, email: Option<String>) -> SignedUser {
        let or = |value: Option<String>, fallback: String| value.filter(|v| !v.is_empty()).unwrap_or(fallback);
        SignedUser {
            id: or(id, get_random_id()),
            email: or(email, "unknown".to_string()),
            username: or(username, "unknown".to_string()),
            token: None,
            expiry: None,
            password_version: None,
            is_read_only: None,
            authenticated_as: None,
        }
    }

    pub fn read_only(&self) -> bool {
        self.is_read_only == Some(true)
    }
}

/// What a session started through single sign-on keeps from the provider.
/// It stays on the session and never reaches the browser.
#[derive(Debug, Clone, PartialEq)]
pub struct SsoSession {
    /// The provider's session id, which its `session.ended` event names.
    pub sid: String,
    /// `None` when the provider issued none; the session then ends at its re-check.
    pub refresh_token: Option<String>,
    /// When access is next re-checked with the provider.
    pub recheck_at: DateTime<Utc>,
}

/// A session cookie's attributes while the server handles a request.
///
/// `secure` and `same_site` are stored as JSON text and held here as the
/// values that text parses to, whatever they are: a row is written back the
/// way it was read. Anything truthy in `secure` means a `Secure` cookie; of
/// `same_site` only `true`, `"strict"`, `"lax"` and `"none"` name an attribute.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeCookie {
    pub original_max_age: Option<i64>,
    pub max_age: Option<i64>,
    pub signed: Option<bool>,
    pub expires: Option<DateTime<Utc>>,
    pub http_only: Option<bool>,
    pub path: Option<String>,
    pub domain: Option<String>,
    pub secure: Option<serde_json::Value>,
    pub same_site: Option<serde_json::Value>,
}

impl RuntimeCookie {
    /// Whether the cookie carries the `Secure` attribute.
    pub fn is_secure(&self) -> bool {
        self.secure.as_ref().is_some_and(crate::js::is_truthy)
    }

    /// The value of the `SameSite` attribute, when the cookie has one.
    pub fn same_site_attribute(&self) -> Option<&'static str> {
        match self.same_site.as_ref()? {
            serde_json::Value::Bool(true) => Some("Strict"),
            serde_json::Value::String(value) => match value.as_str() {
                "strict" => Some("Strict"),
                "lax" => Some("Lax"),
                "none" => Some("None"),
                _ => None,
            },
            _ => None,
        }
    }
}

/// A stored session, as the store hands it to the request pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeSession {
    pub user: SignedUser,
    pub cookie: RuntimeCookie,
    pub auth: Option<SsoSession>,
}
