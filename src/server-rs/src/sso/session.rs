//! Holding a session to the sign-in mode (sso/session.ts).

use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;

use crate::auth_client::{AuthError, Login};
use crate::models::{SignedUser, SsoSession};
use crate::sso::{get_sso, post_logout_redirect, Sso};
use crate::{js, logger};

/// How long a session goes before its access is re-checked with the provider.
pub const RECHECK_INTERVAL_MS: i64 = 10 * 60 * 1000;

/// How long past a due re-check a session keeps serving while the provider
/// cannot be reached: the staleness a provider outage is allowed to cost.
pub const MAX_RECHECK_DELAY_MS: i64 = 60 * 60 * 1000;

fn time_at(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_default()
}

/// The provider fields a session started by `login` keeps, with its first
/// re-check ten minutes after `now_ms`.
pub fn sso_session_fields(login: &Login, now_ms: i64) -> SsoSession {
    SsoSession {
        sid: login.claims.sid.clone(),
        refresh_token: login.refresh_token.clone(),
        recheck_at: time_at(now_ms + RECHECK_INTERVAL_MS),
    }
}

/// What [`enforce_sso_session`] needs of the session a request arrived with.
/// The HTTP layer implements it for its session type.
pub trait EnforcedSession: Send {
    /// The authenticated identity; `None` while the session is anonymous.
    fn user(&self) -> Option<&SignedUser>;
    /// What the session keeps from the provider; `None` on a password session.
    fn auth(&self) -> Option<&SsoSession>;
    /// Replaces the provider fields. The session is written back with the
    /// response, as any change to it is.
    fn set_auth(&mut self, auth: Option<SsoSession>);
    /// Deletes the stored session, so that the response sets no session cookie.
    fn destroy(&mut self) -> impl Future<Output = Result<()>> + Send;
    /// Makes the session anonymous for the rest of the request.
    fn clear_user(&mut self);
}

async fn end_session<S: EnforcedSession>(session: &mut S, reason: &str) -> Result<()> {
    let user_id = session.user().map(|user| user.id.clone());
    logger::info("Ended a session", json!({ "component": "sso", "reason": reason, "userId": user_id }));
    session.destroy().await?;
    session.clear_user();
    session.set_auth(None);
    Ok(())
}

/// Holds the session a request arrived with to the sign-in mode, before any
/// route reads it:
///
/// - a session from single sign-on ends once the feature is off, since nothing
///   can re-check it any more, and a password session ends while password
///   sign-in is off;
/// - a session from single sign-on whose re-check is due refreshes its token
///   first. A refused token ends the session. An unreachable provider leaves it
///   serving, to try again on the next request, until the re-check is
///   [`MAX_RECHECK_DELAY_MS`] late.
///
/// Costs nothing on a request whose re-check is not due. An `Err` is a failure
/// to delete an ended session; the request fails with it.
pub async fn enforce_sso_session<S: EnforcedSession>(session: &mut S) -> Result<()> {
    let sso = get_sso();
    enforce_sso_session_at(sso.as_deref(), session, js::now_ms()).await
}

/// [`enforce_sso_session`] against a given single sign-on (`None` when it is
/// off) at a given time, in milliseconds since the epoch.
pub async fn enforce_sso_session_at<S: EnforcedSession>(sso: Option<&Sso>, session: &mut S, now: i64) -> Result<()> {
    if session.user().is_none() {
        return Ok(());
    }
    let Some(auth) = session.auth().cloned() else {
        if sso.is_some_and(|sso| !sso.config.local_login) {
            end_session(session, "password sign-in is off").await?;
        }
        return Ok(());
    };
    let Some(sso) = sso else {
        return end_session(session, "single sign-on is off").await;
    };
    let recheck_at = auth.recheck_at.timestamp_millis();
    if recheck_at > now {
        return Ok(());
    }
    let Some(refresh_token) = auth.refresh_token.as_deref().filter(|token| !token.is_empty()) else {
        return end_session(session, "no refresh token to re-check access with").await;
    };

    match sso.client.refresh(refresh_token).await {
        Ok(refreshed) => {
            session.set_auth(Some(SsoSession {
                sid: auth.sid.clone(),
                refresh_token: Some(refreshed.refresh_token.unwrap_or_else(|| refresh_token.to_string())),
                recheck_at: time_at(now + RECHECK_INTERVAL_MS),
            }));
        }
        Err(AuthError::InvalidGrant(_)) => return end_session(session, "the provider refused the session").await,
        Err(error) => {
            if now > recheck_at + MAX_RECHECK_DELAY_MS {
                return end_session(session, "access could not be re-checked for too long").await;
            }
            let user_id = session.user().map(|user| user.id.clone());
            if error.is_unavailable() {
                logger::warn(
                    "Could not re-check a session's access; serving it meanwhile",
                    json!({ "component": "sso", "userId": user_id, "reason": error.to_string() }),
                );
            } else {
                logger::error_with(
                    "Re-checking a session's access failed; serving it meanwhile",
                    json!({ "component": "sso", "userId": user_id }),
                    &error,
                );
            }
        }
    }
    Ok(())
}

/// The provider's side of signing a single sign-on session out, once the
/// session here is destroyed: revokes its refresh token without waiting for
/// the answer, and returns the URL that ends the provider's session too, or
/// `None` when the provider has none or cannot say.
pub async fn sign_out_of_provider(sso: &Arc<Sso>, auth: &SsoSession) -> Option<String> {
    // Best effort and off the response path: the session here is already gone,
    // and the provider's logout ends the token along with its own session.
    if let Some(refresh_token) = auth.refresh_token.clone().filter(|token| !token.is_empty()) {
        let sso = Arc::clone(sso);
        tokio::spawn(async move {
            if let Err(error) = sso.client.revoke(&refresh_token).await {
                logger::warn(
                    "Could not revoke a refresh token at sign-out",
                    json!({ "component": "sso", "reason": error.to_string() }),
                );
            }
        });
    }
    let redirect = post_logout_redirect(&sso.config.redirect_uri);
    sso.client.logout_url(Some(&redirect)).await.ok().filter(|url| !url.is_empty())
}
