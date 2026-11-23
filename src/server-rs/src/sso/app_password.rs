//! App passwords for IMAP and SMTP (sso/app-password.ts).

use std::future::Future;

use anyhow::Result;
use serde_json::json;

use crate::auth_client::{is_ip, AuthError, Introspection};
use crate::models::SignedUser;
use crate::postgres::repositories::users::{find_user_by_auth_sub, UserModel};
use crate::sso::Sso;
use crate::{js, logger};

/// The protocol asking, which is also the scope an app password needs for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPasswordScope {
    Imap,
    Smtp,
}

impl AppPasswordScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AppPasswordScope::Imap => "imap",
            AppPasswordScope::Smtp => "smtp",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppPasswordCheck {
    /// `password_version` is the credential generation to stamp the connection with.
    Accepted {
        user: SignedUser,
        password_version: i64,
    },
    Rejected,
    /// The provider could not answer, so nothing was learnt about the password.
    Unavailable,
}

/// How long the provider client may serve a cached answer: a minute while the
/// provider answers, and up to ten minutes while it can't be reached. An
/// account changed more recently than this may have had the credential
/// revoked by an event another copy of the server received, so its answer is
/// asked for afresh rather than served from a cache that predates the change.
const CACHED_ANSWER_MS: i64 = 600_000;

/// The client's address for the provider's limits, when it is an address.
fn ip_option(address: &str) -> Option<&str> {
    let mapped = address.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("::ffff:"));
    let ip = if mapped { &address[7..] } else { address };
    is_ip(ip).then_some(ip)
}

/// Where the account linked to a provider subject is looked up.
pub trait LinkedAccounts: Sync {
    fn find_by_auth_sub(&self, sub: &str) -> impl Future<Output = Result<Option<UserModel>>> + Send;
}

/// The `users` table.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresLinkedAccounts;

impl LinkedAccounts for PostgresLinkedAccounts {
    async fn find_by_auth_sub(&self, sub: &str) -> Result<Option<UserModel>> {
        Ok(find_user_by_auth_sub(sub).await?)
    }
}

/// Checks `password` as an app password from the identity provider, for a
/// protocol that can carry nothing but a username and a password. It is
/// accepted when the provider vouches for it, it belongs to the account
/// linked to its owner, `username` names that account, and its scopes cover
/// the protocol.
///
/// `Unavailable` means the provider could not answer, so nothing was learnt
/// about the password. A failed lookup of the local account is an `Err`.
pub async fn check_app_password(
    sso: &Sso,
    username: &str,
    password: &str,
    scope: AppPasswordScope,
    remote_address: &str,
) -> Result<AppPasswordCheck> {
    check_app_password_with(sso, username, password, scope, remote_address, &PostgresLinkedAccounts, js::now_ms()).await
}

/// [`check_app_password`] against given accounts, at a given time.
pub async fn check_app_password_with<A: LinkedAccounts + ?Sized>(
    sso: &Sso,
    username: &str,
    password: &str,
    scope: AppPasswordScope,
    remote_address: &str,
    accounts: &A,
    now: i64,
) -> Result<AppPasswordCheck> {
    // `Err(())` when the provider could not say.
    let introspect = || async {
        match sso.client.introspect(password, ip_option(remote_address)).await {
            Ok(answer) => Ok::<Option<Introspection>, ()>(answer),
            Err(AuthError::Unavailable(reason)) => {
                logger::warn(
                    "Could not check an app password",
                    json!({ "component": scope.as_str(), "reason": reason }),
                );
                Err(())
            }
            Err(error) => {
                logger::error_with("Checking an app password failed", json!({ "component": scope.as_str() }), &error);
                Err(())
            }
        }
    };

    let mut answer = match introspect().await {
        Err(()) => return Ok(AppPasswordCheck::Unavailable),
        Ok(None) => return Ok(AppPasswordCheck::Rejected),
        Ok(Some(answer)) => answer,
    };
    let Some(account) = accounts.find_by_auth_sub(&answer.sub).await? else {
        return Ok(AppPasswordCheck::Rejected);
    };
    let changed_at = account.updated.as_deref().and_then(js::date_parse);
    if changed_at.is_some_and(|changed_at| now - changed_at < CACHED_ANSWER_MS) {
        sso.client.forget(Some(&answer.sub), None);
        answer = match introspect().await {
            Err(()) => return Ok(AppPasswordCheck::Unavailable),
            Ok(None) => return Ok(AppPasswordCheck::Rejected),
            Ok(Some(answer)) => answer,
        };
    }
    if account.username != username || !answer.scopes.iter().any(|granted| granted == scope.as_str()) {
        return Ok(AppPasswordCheck::Rejected);
    }

    logger::info(
        "App password accepted",
        json!({ "component": scope.as_str(), "authenticatedAs": username, "credential": answer.id }),
    );
    let password_version = account.password_version.unwrap_or(1);
    let mut user = SignedUser::new(Some(account.user_id), Some(account.username), account.email);
    user.password_version = Some(password_version);
    Ok(AppPasswordCheck::Accepted { user, password_version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_clients_address_only_when_it_is_one() {
        assert_eq!(ip_option("203.0.113.7"), Some("203.0.113.7"));
        assert_eq!(ip_option("::ffff:198.51.100.4"), Some("198.51.100.4"));
        assert_eq!(ip_option("::FFFF:198.51.100.4"), Some("198.51.100.4"));
        assert_eq!(ip_option("2001:db8::1"), Some("2001:db8::1"));
        assert_eq!(ip_option("unknown"), None);
        assert_eq!(ip_option(""), None);
        assert_eq!(ip_option("::ffff:"), None);
        assert_eq!(ip_option("::ffff:not-an-address"), None);
    }

    #[test]
    fn the_scope_is_the_protocols_name() {
        assert_eq!(AppPasswordScope::Imap.as_str(), "imap");
        assert_eq!(AppPasswordScope::Smtp.as_str(), "smtp");
    }
}
