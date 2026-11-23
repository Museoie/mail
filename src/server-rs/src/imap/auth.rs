//! IMAP authentication (imap/auth.ts): LOGIN and AUTHENTICATE PLAIN.
//!
//! These are free functions. They take the connection and the credential
//! backend as parameters and return what the session should become, rather
//! than changing a session themselves.

use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;

use super::log;
use crate::auth_rate_limit;
use crate::logger;
use crate::models::{SignedUser, User};
use crate::read_only;
use crate::sso::app_password::{check_app_password, AppPasswordCheck, AppPasswordScope};
use crate::sso::{self, Sso};
use crate::users::{get_user, UserQuery};
use crate::verify_password;

/// What a successful sign-in gives the session.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthResult {
    /// The account the session reads: the caller's own, or the one a
    /// read-only credential is mapped onto.
    pub user: SignedUser,
    /// True when the caller authenticated with the read-only credential.
    pub is_read_only: bool,
    /// The username the caller sent, before any session-scope remap.
    pub authenticated_as: String,
    /// The credential generation the session was minted from, read from the
    /// credential row at sign-in. The per-command gate compares it against
    /// the row's current version.
    pub password_version: i64,
}

/// The connection a sign-in is answered on.
pub trait AuthConnection: Sync {
    /// The client's address, which failed attempts are counted against.
    fn remote_address(&self) -> String;
    fn remote_port(&self) -> u16;
    fn write(&self, data: &str) -> bool;
    /// Closes the connection once what was written has gone out.
    fn close(&self);
    /// The next line from the client is the SASL response for `tag`.
    fn set_pending_sasl_tag(&self, tag: &str);
    fn capabilities(&self) -> String;
}

/// Where credentials are checked and failures counted.
pub trait AuthBackend: Sync {
    fn sso(&self) -> Option<Arc<Sso>>;
    fn is_auth_rate_limited(&self, ip: &str, account: Option<&str>) -> bool;
    fn record_auth_failure(&self, ip: &str, account: Option<&str>) -> impl Future<Output = bool> + Send;
    fn reset_auth_failures(&self, ip: &str, account: Option<&str>);
    fn get_user(&self, username: &str) -> impl Future<Output = Result<Option<User>>> + Send;
    fn verify_password(&self, password: &str, stored_hash: Option<&str>) -> impl Future<Output = bool> + Send;
    fn resolve_session_user(
        &self,
        signed_user: Option<SignedUser>,
    ) -> impl Future<Output = Result<Option<(SignedUser, bool)>>> + Send;
    fn check_app_password(
        &self,
        sso: &Sso,
        username: &str,
        password: &str,
        ip: &str,
    ) -> impl Future<Output = Result<AppPasswordCheck>> + Send;
}

/// The accounts in PostgreSQL, the process's rate limiter and its single
/// sign-on.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerAuthBackend;

impl AuthBackend for ServerAuthBackend {
    fn sso(&self) -> Option<Arc<Sso>> {
        sso::get_sso()
    }

    fn is_auth_rate_limited(&self, ip: &str, account: Option<&str>) -> bool {
        auth_rate_limit::is_auth_rate_limited(ip, account)
    }

    async fn record_auth_failure(&self, ip: &str, account: Option<&str>) -> bool {
        auth_rate_limit::record_auth_failure(ip, account).await
    }

    fn reset_auth_failures(&self, ip: &str, account: Option<&str>) {
        auth_rate_limit::reset_auth_failures(ip, account);
    }

    async fn get_user(&self, username: &str) -> Result<Option<User>> {
        get_user(&UserQuery::username(username)).await
    }

    async fn verify_password(&self, password: &str, stored_hash: Option<&str>) -> bool {
        verify_password::verify_password(Some(password), stored_hash).await
    }

    async fn resolve_session_user(&self, signed_user: Option<SignedUser>) -> Result<Option<(SignedUser, bool)>> {
        read_only::resolve_session_user(signed_user).await
    }

    async fn check_app_password(&self, sso: &Sso, username: &str, password: &str, ip: &str) -> Result<AppPasswordCheck> {
        check_app_password(sso, username, password, AppPasswordScope::Imap, ip).await
    }
}

struct Resolved {
    user: SignedUser,
    is_read_only: bool,
    password_version: i64,
}

/// Shared failure reply: rate-limit exhaustion closes the connection,
/// anything else is a flat "Invalid credentials" that reveals nothing about
/// which check failed.
async fn fail_auth(
    tag: &str,
    ip: &str,
    connection: &impl AuthConnection,
    backend: &impl AuthBackend,
    account: Option<&str>,
) -> Option<Resolved> {
    let limited = backend.record_auth_failure(ip, account).await;
    if limited {
        connection.write(&format!("{tag} NO [AUTHENTICATIONFAILED] Too many failed attempts\r\n"));
        connection.close();
        return None;
    }
    connection.write(&format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials.\r\n"));
    None
}

/// The account's own stored password, with the read-only remap applied;
/// `None` when it doesn't match or the remap has no effective user.
async fn check_local_password(backend: &impl AuthBackend, username: &str, password: &str) -> Result<Option<Resolved>> {
    let user = backend.get_user(username).await?;
    let signed_user = user.as_ref().and_then(User::get_signed);

    // Always spent, against a stand-in digest when there is no account: an
    // answer that skips the comparison is a username oracle.
    let stored_hash = user.as_ref().and_then(|user| user.password.as_deref());
    let password_matches = backend.verify_password(password, stored_hash).await;

    let (Some(user), Some(signed_user), true) = (user, signed_user, password_matches) else {
        return Ok(None);
    };

    let Some((resolved, is_read_only)) = backend.resolve_session_user(Some(signed_user)).await? else {
        return Ok(None);
    };

    // Stamp the credential row's generation. Read here, at sign-in, so a
    // LOGIN that straddled a rotation stamps the stale generation and fails
    // its next comparison. Keyed on the credential the caller presented,
    // never the remapped effective user: the read-only credential is its own
    // row and survives a rotation on the account it reads.
    Ok(Some(Resolved { user: resolved, is_read_only, password_version: user.password_version.unwrap_or(1) }))
}

/// The authentication core both mechanisms go through, so the policy — the
/// rate-limit gate, the password check, the read-only session resolution and
/// the failure wording that must not tell "no such user" from "wrong
/// password" — is written once.
///
/// With single sign-on on, the password is checked as the provider's app
/// password; while `AUTH_LOCAL_LOGIN` keeps local passwords valid, the stored
/// one is tried first. A provider that cannot answer gets `NO [UNAVAILABLE]`,
/// which is not counted as a failed attempt.
///
/// On failure the tagged NO has been written (and the connection closed when
/// the rate limiter says so) and `None` is returned.
async fn authenticate_credentials(
    username: &str,
    password: &str,
    ip: &str,
    connection: &impl AuthConnection,
    backend: &impl AuthBackend,
    tag: &str,
) -> Result<Option<Resolved>> {
    // The attempted identity: failures accumulate per (ip, account) and a
    // success forgives only this account's own bucket. The per-address
    // aggregate is never reset here, so alternating accounts from one
    // address cannot wipe the counter.
    let account = auth_rate_limit::normalize_auth_account(Some(username));
    let account = account.as_deref();
    if backend.is_auth_rate_limited(ip, account) {
        connection.write(&format!("{tag} NO [AUTHENTICATIONFAILED] Too many failed attempts\r\n"));
        connection.close();
        return Ok(None);
    }

    let sso = backend.sso();
    // While single sign-on and local passwords are both on (a migration),
    // the local password goes first: it is never sent to the provider, and
    // the clients still using one don't spend the provider's limits on
    // failed introspections.
    if sso.as_ref().is_none_or(|sso| sso.config.local_login) {
        if let Some(local) = check_local_password(backend, username, password).await? {
            backend.reset_auth_failures(ip, account);
            return Ok(Some(local));
        }
    }
    let Some(sso) = sso else {
        return Ok(fail_auth(tag, ip, connection, backend, account).await);
    };

    match backend.check_app_password(&sso, username, password, ip).await? {
        AppPasswordCheck::Unavailable => {
            connection.write(&format!("{tag} NO [UNAVAILABLE] Authentication is temporarily unavailable\r\n"));
            Ok(None)
        }
        AppPasswordCheck::Rejected => Ok(fail_auth(tag, ip, connection, backend, account).await),
        AppPasswordCheck::Accepted { user, password_version } => {
            backend.reset_auth_failures(ip, account);
            Ok(Some(Resolved { user, is_read_only: false, password_version }))
        }
    }
}

/// `Buffer.from(text, "base64")`: characters outside the alphabet are
/// skipped, the URL-safe alphabet is accepted, `=` ends the data, and a
/// trailing partial group gives the octets it holds.
pub fn decode_base64_lenient(text: &str) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(text.len() / 4 * 3 + 2);
    let mut group = [0u8; 4];
    let mut held = 0;
    for byte in text.bytes() {
        let sextet = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        group[held] = sextet;
        held += 1;
        if held == 4 {
            decoded.push(group[0] << 2 | group[1] >> 4);
            decoded.push(group[1] << 4 | group[2] >> 2);
            decoded.push(group[2] << 6 | group[3]);
            held = 0;
        }
    }
    if held >= 2 {
        decoded.push(group[0] << 2 | group[1] >> 4);
    }
    if held == 3 {
        decoded.push(group[1] << 4 | group[2] >> 2);
    }
    decoded
}

/// `value.replace(/^"(.*)"$/, "$1")`: one pair of surrounding quotes comes
/// off, unless the text between them holds a line terminator.
pub fn strip_surrounding_quotes(value: &str) -> &str {
    let Some(inner) = value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) else {
        return value;
    };
    if inner.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return value;
    }
    inner
}

/// AUTHENTICATE with the PLAIN mechanism.
///
/// Returns the sign-in on success. Otherwise the answer (a refusal, or the
/// empty challenge) has been written and `None` is returned.
pub async fn handle_authenticate(
    tag: &str,
    mechanism: &str,
    initial_response: Option<&str>,
    connection: &impl AuthConnection,
    backend: &impl AuthBackend,
) -> Option<AuthResult> {
    if mechanism != "PLAIN" {
        connection.write(&format!("{tag} NO Only PLAIN authentication supported\r\n"));
        return None;
    }

    let Some(initial_response) = initial_response.filter(|response| !response.is_empty()) else {
        connection.write("+ \r\n");
        connection.set_pending_sasl_tag(tag);
        return None;
    };

    let ip = connection.remote_address();

    let decoded = String::from_utf8_lossy(&decode_base64_lenient(initial_response)).into_owned();
    let parts: Vec<&str> = decoded.split('\0').collect();
    let [_, username, password] = parts[..] else {
        connection.write(&format!("{tag} BAD Invalid PLAIN response format\r\n"));
        return None;
    };

    let resolved = match authenticate_credentials(username, password, &ip, connection, backend, tag).await {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return None,
        Err(error) => {
            logger::error_with("AUTHENTICATE error", json!({ "component": "imap" }), &error);
            connection.write(&format!("{tag} BAD AUTHENTICATE failed\r\n"));
            return None;
        }
    };

    // Auth-audit line: never behind the per-command threshold gate, which
    // would drop a fast sign-in to DEBUG. `authenticatedAs` names the
    // original credential (unchanged by the read-only remap) so a
    // compromised read-only credential does not read in the log as admin.
    log::info(
        "IMAP AUTHENTICATE success",
        json!({
            "component": "imap",
            "tag": tag,
            "authenticatedAs": username,
            "effectiveUsername": resolved.user.username,
            "isReadOnly": resolved.is_read_only,
            "remote": format!("{ip}:{}", connection.remote_port()),
            "mechanism": "PLAIN",
        }),
    );
    connection.write(&format!("{tag} OK [CAPABILITY {}] AUTHENTICATE completed\r\n", connection.capabilities()));
    Some(AuthResult {
        user: resolved.user,
        is_read_only: resolved.is_read_only,
        authenticated_as: username.to_string(),
        password_version: resolved.password_version,
    })
}

/// LOGIN.
///
/// Returns the sign-in on success; a refusal has been written otherwise. An
/// unexpected fault is returned to the dispatch, which answers it as an
/// internal error.
pub async fn handle_login(
    tag: &str,
    args: &[String],
    connection: &impl AuthConnection,
    backend: &impl AuthBackend,
) -> Result<Option<AuthResult>> {
    let [username, password, ..] = args else {
        connection.write(&format!("{tag} BAD LOGIN requires username and password\r\n"));
        return Ok(None);
    };

    let ip = connection.remote_address();

    let clean_username = strip_surrounding_quotes(username);
    let clean_password = strip_surrounding_quotes(password);

    let Some(resolved) =
        authenticate_credentials(clean_username, clean_password, &ip, connection, backend, tag).await?
    else {
        return Ok(None);
    };

    // Auth-audit line, as for AUTHENTICATE.
    log::info(
        "IMAP LOGIN success",
        json!({
            "component": "imap",
            "tag": tag,
            "authenticatedAs": clean_username,
            "effectiveUsername": resolved.user.username,
            "isReadOnly": resolved.is_read_only,
            "remote": format!("{ip}:{}", connection.remote_port()),
        }),
    );
    connection.write(&format!("{tag} OK [CAPABILITY {}] LOGIN completed\r\n", connection.capabilities()));
    Ok(Some(AuthResult {
        user: resolved.user,
        is_read_only: resolved.is_read_only,
        authenticated_as: clean_username.to_string(),
        password_version: resolved.password_version,
    }))
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
