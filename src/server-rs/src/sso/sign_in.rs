//! Sign-in failure codes (sso/sign-in.ts), and the two halves of a browser
//! sign-in as the `/api/auth/login` and `/api/auth/callback` routes run them,
//! in plain values: a location to redirect to and `Set-Cookie` strings.

use serde_json::json;

use crate::auth_client::{AuthError, LoginErrorCode, StartLoginOptions};
use crate::models::{SignedUser, SsoSession};
use crate::sso::accounts::{
    resolve_sso_account, AccountDirectory, PostgresAccountDirectory, SsoAccountOutcome, SsoAccountRefusal,
    SsoAccountResult, SsoIdentity,
};
use crate::sso::session::sso_session_fields;
use crate::sso::Sso;
use crate::{alarm, js, logger};

/// Why a single sign-on did not complete, as the sign-in page reads it from
/// `?sso_error=`. A short code, never a provider message or a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsoErrorCode {
    AccountExists,
    InvalidUsername,
    AccessDenied,
    LoginFailed,
    InvalidToken,
    Unavailable,
    Failed,
}

impl SsoErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            SsoErrorCode::AccountExists => "account_exists",
            SsoErrorCode::InvalidUsername => "invalid_username",
            SsoErrorCode::AccessDenied => "access_denied",
            SsoErrorCode::LoginFailed => "login_failed",
            SsoErrorCode::InvalidToken => "invalid_token",
            SsoErrorCode::Unavailable => "unavailable",
            SsoErrorCode::Failed => "failed",
        }
    }
}

impl From<SsoAccountRefusal> for SsoErrorCode {
    fn from(refusal: SsoAccountRefusal) -> SsoErrorCode {
        match refusal {
            SsoAccountRefusal::AccountExists => SsoErrorCode::AccountExists,
            SsoAccountRefusal::InvalidUsername => SsoErrorCode::InvalidUsername,
        }
    }
}

/// The code for a failure of the provider client.
pub fn sso_error_code_of_auth(error: &AuthError) -> SsoErrorCode {
    match error {
        AuthError::Login { code: LoginErrorCode::AuthorizationError, error: Some(error) }
            if error == "access_denied" =>
        {
            SsoErrorCode::AccessDenied
        }
        AuthError::Login { .. } | AuthError::InvalidGrant(_) => SsoErrorCode::LoginFailed,
        AuthError::InvalidToken(_) => SsoErrorCode::InvalidToken,
        AuthError::Unavailable(_) => SsoErrorCode::Unavailable,
        AuthError::Other(_) => SsoErrorCode::Failed,
    }
}

/// The code for a failure the provider client raised; `failed` for anything else.
pub fn sso_error_code_of(error: &anyhow::Error) -> SsoErrorCode {
    error.downcast_ref::<AuthError>().map_or(SsoErrorCode::Failed, sso_error_code_of_auth)
}

pub fn sign_in_path_for(code: SsoErrorCode) -> String {
    format!("/sign-in?sso_error={}", code.as_str())
}

/// Deletes the login cookie the provider client set when the sign-in started.
/// The client hands this value back only for a sign-in that completes; a
/// failed one would otherwise leave the cookie behind until it expires.
pub const CLEAR_LOGIN_COOKIE: &str = "__Host-login=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0";

/// What a sign-in route answers: `302` to `location` with `Cache-Control:
/// no-store`, setting each of `cookies`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInRedirect {
    pub location: String,
    pub cookies: Vec<String>,
}

/// `GET /api/auth/login`: sends the browser to the provider, carrying the
/// local path to come back to in the login cookie. `return_to` is the first
/// `returnTo` query parameter. A provider that cannot be reached lands on the
/// sign-in page with `unavailable`; any other failure alarms and lands on
/// `failed`.
pub async fn start_sign_in(sso: &Sso, return_to: Option<&str>) -> SignInRedirect {
    let options = StartLoginOptions { return_to: return_to.map(str::to_string), ..Default::default() };
    match sso.client.start_login(&options).await {
        Ok(started) => SignInRedirect { location: started.redirect_url, cookies: vec![started.login_cookie] },
        Err(error) => {
            let code = sso_error_code_of_auth(&error);
            let reason = error.to_string();
            if code == SsoErrorCode::Unavailable {
                logger::warn("Could not start a single sign-on", json!({ "component": "sso", "reason": reason }));
            } else {
                logger::error_with("Starting a single sign-on failed", json!({ "component": "sso" }), &error);
                alarm::spawn_alarm("Single sign-on failed", format!("**Error:** {reason}"));
            }
            SignInRedirect { location: sign_in_path_for(code), cookies: Vec::new() }
        }
    }
}

/// A sign-in the provider and the account rules accepted. The route starts
/// the app's own session from it and then answers [`CompletedSignIn::redirect`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedSignIn {
    /// The identity to attach to a freshly issued session.
    pub user: SignedUser,
    /// The provider fields the session keeps.
    pub auth: SsoSession,
    pub outcome: SsoAccountOutcome,
    /// The local path the sign-in started from, or "/".
    pub return_to: String,
    /// The `Set-Cookie` value that deletes the login cookie.
    pub clear_cookie: String,
}

impl CompletedSignIn {
    /// The answer once the session is stored. Logs the completed sign-in.
    pub fn redirect(&self) -> SignInRedirect {
        logger::info(
            "Single sign-on completed",
            json!({
                "component": "sso",
                "outcome": self.outcome.as_str(),
                "userId": self.user.id,
                "username": self.user.username,
            }),
        );
        SignInRedirect { location: self.return_to.clone(), cookies: vec![self.clear_cookie.clone()] }
    }
}

/// The answer to a callback that failed with `error`, in the provider client,
/// the account rules, or while the route was starting the session: the
/// sign-in page with a code saying why, and the login cookie deleted. A
/// failure that is nobody's but the server's or the provider's alarms.
pub fn sign_in_failure(error: &anyhow::Error) -> SignInRedirect {
    let code = sso_error_code_of(error);
    if code == SsoErrorCode::Failed {
        logger::error_with("Single sign-on failed", json!({ "component": "sso" }), error);
        alarm::spawn_alarm("Single sign-on failed", format!("**Error:** {error}"));
    } else {
        logger::warn("Single sign-on did not complete", json!({ "component": "sso", "reason": code.as_str() }));
    }
    SignInRedirect { location: sign_in_path_for(code), cookies: vec![CLEAR_LOGIN_COOKIE.to_string()] }
}

/// `GET /api/auth/callback`, up to the point where the app's own session
/// starts: completes the sign-in with the provider, then finds, links or
/// creates the local account. `request_url` is the URL the browser came back
/// to (path and query are enough) and `cookie_header` its `Cookie` header.
///
/// `Err` is the finished answer for a sign-in that did not complete.
pub async fn finish_sign_in(
    sso: &Sso,
    request_url: &str,
    cookie_header: Option<&str>,
) -> Result<CompletedSignIn, SignInRedirect> {
    finish_sign_in_with(sso, request_url, cookie_header, &PostgresAccountDirectory, js::now_ms()).await
}

/// [`finish_sign_in`] against a given account directory, at a given time.
pub async fn finish_sign_in_with<D: AccountDirectory + ?Sized>(
    sso: &Sso,
    request_url: &str,
    cookie_header: Option<&str>,
    directory: &D,
    now_ms: i64,
) -> Result<CompletedSignIn, SignInRedirect> {
    let login = match sso.client.finish_login(request_url, cookie_header).await {
        Ok(login) => login,
        Err(error) => return Err(sign_in_failure(&error.into())),
    };
    let identity = SsoIdentity {
        sub: login.claims.sub.clone(),
        username: login.claims.username.clone(),
        email: login.id_claims.email().clone(),
        email_verified: login.id_claims.email_verified(),
    };
    let (account, outcome) = match resolve_sso_account(&identity, &sso.config.link_users, directory).await {
        Ok(SsoAccountResult::Found { account, outcome }) => (account, outcome),
        Ok(SsoAccountResult::Refused(reason)) => {
            logger::info(
                "Refused a single sign-on",
                json!({ "component": "sso", "reason": reason.as_str(), "username": login.claims.username }),
            );
            return Err(SignInRedirect {
                location: sign_in_path_for(reason.into()),
                cookies: vec![CLEAR_LOGIN_COOKIE.to_string()],
            });
        }
        Err(error) => return Err(sign_in_failure(&error)),
    };
    Ok(CompletedSignIn {
        user: account.to_signed_user(),
        auth: sso_session_fields(&login, now_ms),
        outcome,
        return_to: login.return_to,
        clear_cookie: login.clear_cookie,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn login_error(code: LoginErrorCode, error: Option<&str>) -> AuthError {
        AuthError::Login { code, error: error.map(str::to_string) }
    }

    #[test]
    fn names_each_failure_of_the_provider_client_with_a_short_code() {
        let code_of = |error: AuthError| sso_error_code_of(&error.into());
        assert_eq!(
            code_of(login_error(LoginErrorCode::AuthorizationError, Some("access_denied"))),
            SsoErrorCode::AccessDenied
        );
        assert_eq!(
            code_of(login_error(LoginErrorCode::AuthorizationError, Some("login_required"))),
            SsoErrorCode::LoginFailed
        );
        assert_eq!(code_of(login_error(LoginErrorCode::AuthorizationError, None)), SsoErrorCode::LoginFailed);
        // Only the callback's error parameter makes a denial, never the words on another failure.
        assert_eq!(
            code_of(login_error(LoginErrorCode::StateMismatch, Some("access_denied"))),
            SsoErrorCode::LoginFailed
        );
        for code in [
            LoginErrorCode::NoLoginCookie,
            LoginErrorCode::LoginExpired,
            LoginErrorCode::StateMismatch,
            LoginErrorCode::MissingCode,
        ] {
            assert_eq!(code_of(login_error(code, None)), SsoErrorCode::LoginFailed);
        }
        assert_eq!(code_of(AuthError::InvalidGrant("refused".into())), SsoErrorCode::LoginFailed);
        assert_eq!(code_of(AuthError::InvalidToken("bad signature".into())), SsoErrorCode::InvalidToken);
        assert_eq!(code_of(AuthError::Unavailable("down".into())), SsoErrorCode::Unavailable);
        assert_eq!(
            code_of(AuthError::Other("the token endpoint refused the request (400)".into())),
            SsoErrorCode::Failed
        );
    }

    #[test]
    fn names_anything_else_failed_whatever_its_message_says() {
        assert_eq!(sso_error_code_of(&anyhow!("rt_secret leaked into a message")), SsoErrorCode::Failed);
        assert_eq!(sso_error_code_of(&anyhow!("access_denied")), SsoErrorCode::Failed);
        // A provider failure keeps its code through added context.
        let wrapped = anyhow::Error::from(AuthError::Unavailable("down".into())).context("finishing a sign-in");
        assert_eq!(sso_error_code_of(&wrapped), SsoErrorCode::Unavailable);
    }

    #[test]
    fn the_sign_in_path_is_the_sign_in_page_with_the_code() {
        assert_eq!(sign_in_path_for(SsoErrorCode::AccountExists), "/sign-in?sso_error=account_exists");
        assert_eq!(sign_in_path_for(SsoAccountRefusal::InvalidUsername.into()), "/sign-in?sso_error=invalid_username");
        for (code, text) in [
            (SsoErrorCode::AccessDenied, "access_denied"),
            (SsoErrorCode::LoginFailed, "login_failed"),
            (SsoErrorCode::InvalidToken, "invalid_token"),
            (SsoErrorCode::Unavailable, "unavailable"),
            (SsoErrorCode::Failed, "failed"),
        ] {
            assert_eq!(code.as_str(), text);
        }
    }

    #[tokio::test]
    async fn a_failed_callback_deletes_the_login_cookie_and_names_the_code() {
        let unavailable = sign_in_failure(&AuthError::Unavailable("auth is unreachable".into()).into());
        assert_eq!(
            unavailable,
            SignInRedirect {
                location: "/sign-in?sso_error=unavailable".into(),
                cookies: vec!["__Host-login=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0".into()],
            }
        );
        // No code or token in the URL it lands on, whatever the fault says.
        let fault = sign_in_failure(&anyhow!("connection lost while writing rt_secret"));
        assert_eq!(fault.location, "/sign-in?sso_error=failed");
        assert_eq!(fault.cookies, [CLEAR_LOGIN_COOKIE]);
    }
}
