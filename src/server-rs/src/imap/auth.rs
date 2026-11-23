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
use super::store::StoreScope;
use crate::auth_rate_limit;
use crate::domains::{default_domain, owned_domain_names, parse_scoped_login};
use crate::logger;
use crate::models::{SignedUser, User};
use crate::postgres::repositories::domains::{get_domain_access, DomainAccess, DomainRole};
use crate::sso::app_password::{check_app_password, AppPasswordCheck, AppPasswordScope};
use crate::sso::{self, Sso};
use crate::users::{get_user, UserQuery};
use crate::verify_password;

/// The domain a scoped login reads, the person's role on it, and its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionScope {
    /// The person's own user id, which the role is keyed by.
    pub user_id: String,
    pub domain: String,
    pub role: DomainRole,
    pub owner_id: String,
}

/// What a successful sign-in gives the session.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthResult {
    /// The account whose rows the session reads: the person's own, or a
    /// viewed domain's owner.
    pub user: SignedUser,
    /// Which of that account's mail the session sees.
    pub store_scope: StoreScope,
    /// True when the login is scoped to a domain the person only views.
    pub is_read_only: bool,
    /// The person who signed in: the username part of the login, which names
    /// the credential row.
    pub authenticated_as: String,
    /// The domain a `user@domain` login is scoped to, and the grant it rests on.
    pub scope: Option<SessionScope>,
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
    /// The account with this id: a viewed domain's owner.
    fn get_user_by_id(&self, id: &str) -> impl Future<Output = Result<Option<User>>> + Send;
    /// The names of the domains the user owns.
    fn owned_domain_names(&self, user_id: &str) -> impl Future<Output = Result<Vec<String>>> + Send;
    /// The user's role on a domain, `None` when they hold none.
    fn get_domain_access(
        &self,
        user_id: &str,
        domain: &str,
    ) -> impl Future<Output = Result<Option<DomainAccess>>> + Send;
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

    async fn get_user_by_id(&self, id: &str) -> Result<Option<User>> {
        get_user(&UserQuery::id(id)).await
    }

    async fn owned_domain_names(&self, user_id: &str) -> Result<Vec<String>> {
        Ok(owned_domain_names(user_id).await?)
    }

    async fn get_domain_access(&self, user_id: &str, domain: &str) -> Result<Option<DomainAccess>> {
        Ok(get_domain_access(user_id, domain).await?)
    }

    async fn check_app_password(&self, sso: &Sso, username: &str, password: &str, ip: &str) -> Result<AppPasswordCheck> {
        check_app_password(sso, username, password, AppPasswordScope::Imap, ip).await
    }
}

/// `undefined` fields are left out of a Bun log line; `null` stands for them here.
trait WithoutNulls {
    fn without_nulls(self) -> Self;
}

impl WithoutNulls for serde_json::Value {
    fn without_nulls(mut self) -> Self {
        if let Some(object) = self.as_object_mut() {
            object.retain(|_, value| !value.is_null());
        }
        self
    }
}

/// The session a login opens.
struct Resolved {
    /// The account whose rows the session reads.
    user: SignedUser,
    is_read_only: bool,
    /// The person's username, without any `@domain` scope.
    username: String,
    scope: Option<SessionScope>,
    store_scope: StoreScope,
    password_version: i64,
}

impl Resolved {
    fn into_auth_result(self) -> AuthResult {
        AuthResult {
            user: self.user,
            store_scope: self.store_scope,
            is_read_only: self.is_read_only,
            authenticated_as: self.username,
            scope: self.scope,
            password_version: self.password_version,
        }
    }
}

/// The store a person's login opens. A plain login reads their own store, all
/// their domains. A `user@domain` login reads only that domain: from their
/// own store when they own it, read-only from its owner's when they view it,
/// and not at all (`None`) when they hold no role on it.
async fn resolve_scope(
    backend: &impl AuthBackend,
    person: SignedUser,
    username: &str,
    scope_domain: Option<&str>,
    password_version: i64,
) -> Result<Option<Resolved>> {
    let Some(scope_domain) = scope_domain else {
        let owned = backend.owned_domain_names(&person.id).await?;
        let primary_domain = default_domain(&owned, username).unwrap_or_default();
        // Account folders are named by local part and read back on the
        // primary domain, so only that domain's addresses are listed: a
        // folder for an address on another domain would open a different
        // mailbox. A login scoped to that domain lists its folders.
        let account_domains = if primary_domain.is_empty() { Vec::new() } else { vec![primary_domain.clone()] };
        return Ok(Some(Resolved {
            user: person,
            is_read_only: false,
            username: username.to_string(),
            scope: None,
            store_scope: StoreScope { domains: None, account_domains, primary_domain },
            password_version,
        }));
    };
    let Some(access) = backend.get_domain_access(&person.id, scope_domain).await? else {
        return Ok(None);
    };
    let scope = SessionScope {
        user_id: person.id.clone(),
        domain: access.name.clone(),
        role: access.role,
        owner_id: access.owner_id.clone(),
    };
    let store_scope = StoreScope {
        domains: Some(vec![access.name.clone()]),
        account_domains: vec![access.name.clone()],
        primary_domain: access.name.clone(),
    };
    let (user, is_read_only) = match access.role {
        DomainRole::Owner => (person, false),
        DomainRole::Viewer => {
            let owner = backend.get_user_by_id(&access.owner_id).await?.as_ref().and_then(User::get_signed);
            let Some(owner) = owner else {
                return Ok(None);
            };
            (owner, true)
        }
    };
    Ok(Some(Resolved {
        user,
        is_read_only,
        username: username.to_string(),
        scope: Some(scope),
        store_scope,
        password_version,
    }))
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

/// The account's own stored password and the credential generation it
/// carries; `None` when it doesn't match.
async fn check_local_password(
    backend: &impl AuthBackend,
    username: &str,
    password: &str,
) -> Result<Option<(SignedUser, i64)>> {
    let user = backend.get_user(username).await?;
    let signed_user = user.as_ref().and_then(User::get_signed);

    // Always spent, against a stand-in digest when there is no account: an
    // answer that skips the comparison is a username oracle.
    let stored_hash = user.as_ref().and_then(|user| user.password.as_deref());
    let password_matches = backend.verify_password(password, stored_hash).await;

    let (Some(user), Some(signed_user), true) = (user, signed_user, password_matches) else {
        return Ok(None);
    };

    // Stamp the credential row's generation. Read here, at sign-in, so a
    // LOGIN that straddled a rotation stamps the stale generation and fails
    // its next comparison.
    Ok(Some((signed_user, user.password_version.unwrap_or(1))))
}

/// The authentication core both mechanisms go through, so the policy — the
/// rate-limit gate, the password check, the scope resolution and the failure
/// wording that must not tell "no such user" from "wrong password" — is
/// written once.
///
/// A `user@domain` login is scoped to that domain; a scope the person holds
/// no role on fails like a wrong password, so the answer says nothing about
/// which half of the login was wrong.
///
/// With single sign-on on, the password is checked as the provider's app
/// password; while `AUTH_LOCAL_LOGIN` keeps local passwords valid, the stored
/// one is tried first. A provider that cannot answer gets `NO [UNAVAILABLE]`,
/// which is not counted as a failed attempt.
///
/// On failure the tagged NO has been written (and the connection closed when
/// the rate limiter says so) and `None` is returned.
async fn authenticate_credentials(
    login: &str,
    password: &str,
    ip: &str,
    connection: &impl AuthConnection,
    backend: &impl AuthBackend,
    tag: &str,
) -> Result<Option<Resolved>> {
    let scoped = parse_scoped_login(login);
    let username = scoped.username.as_str();
    let scope_domain = scoped.domain.as_deref();
    // The attempted identity: failures accumulate per (ip, account) and a
    // success forgives only this account's own bucket. The per-address
    // aggregate is never reset here, so alternating accounts from one
    // address cannot wipe the counter.
    let account = auth_rate_limit::normalize_auth_account(Some(username));
    let account = account.as_deref();
    let granted = async |person: SignedUser, password_version: i64| -> Result<Option<Resolved>> {
        let Some(resolved) = resolve_scope(backend, person, username, scope_domain, password_version).await? else {
            return Ok(fail_auth(tag, ip, connection, backend, account).await);
        };
        // Scoped reset: forgive only the authenticating account's own bucket.
        backend.reset_auth_failures(ip, account);
        Ok(Some(resolved))
    };
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
        if let Some((person, password_version)) = check_local_password(backend, username, password).await? {
            return granted(person, password_version).await;
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
        AppPasswordCheck::Accepted { user, password_version } => granted(user, password_version).await,
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
    // would drop a fast sign-in to DEBUG. `authenticatedAs` names the person,
    // and `effectiveUsername` the account whose rows a viewer's session reads.
    log::info(
        "IMAP AUTHENTICATE success",
        json!({
            "component": "imap",
            "tag": tag,
            "authenticatedAs": resolved.username,
            "scopeDomain": resolved.scope.as_ref().map(|scope| scope.domain.clone()),
            "effectiveUsername": resolved.user.username,
            "isReadOnly": resolved.is_read_only,
            "remote": format!("{ip}:{}", connection.remote_port()),
            "mechanism": "PLAIN",
        })
        .without_nulls(),
    );
    connection.write(&format!("{tag} OK [CAPABILITY {}] AUTHENTICATE completed\r\n", connection.capabilities()));
    Some(resolved.into_auth_result())
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
            "authenticatedAs": resolved.username,
            "scopeDomain": resolved.scope.as_ref().map(|scope| scope.domain.clone()),
            "effectiveUsername": resolved.user.username,
            "isReadOnly": resolved.is_read_only,
            "remote": format!("{ip}:{}", connection.remote_port()),
        })
        .without_nulls(),
    );
    connection.write(&format!("{tag} OK [CAPABILITY {}] LOGIN completed\r\n", connection.capabilities()));
    Ok(Some(resolved.into_auth_result()))
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
