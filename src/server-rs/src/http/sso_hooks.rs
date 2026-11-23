//! The points where the request pipeline and the user routes consult single
//! sign-on. Kept in one place so the HTTP layer names `sso` in one file.

use crate::http::session::HttpSession;
use crate::models::{SignedUser, SsoSession};
use crate::sso::session::EnforcedSession;

pub use crate::sso::{is_local_login_allowed, sso_mode, SsoMode};

impl EnforcedSession for HttpSession {
    fn user(&self) -> Option<&SignedUser> {
        self.user.as_ref()
    }

    fn auth(&self) -> Option<&SsoSession> {
        self.auth.as_ref()
    }

    fn set_auth(&mut self, auth: Option<SsoSession>) {
        self.auth = auth;
    }

    async fn destroy(&mut self) -> anyhow::Result<()> {
        Ok(HttpSession::destroy(self).await?)
    }

    fn clear_user(&mut self) {
        self.user = None;
    }
}

/// Holds the session a request arrived with to the sign-in mode, before any
/// route reads it.
pub async fn enforce_sso_session(session: &mut HttpSession) -> anyhow::Result<()> {
    crate::sso::session::enforce_sso_session(session).await
}

/// Ends the provider's side of a session that was just destroyed. `None` when
/// the session did not come from single sign-on, or single sign-on is off;
/// otherwise where to send the browser to end the provider session too, when
/// the provider offers that.
pub async fn sign_out_of_provider(auth: Option<SsoSession>) -> Option<Option<String>> {
    let auth = auth?;
    let sso = crate::sso::get_sso()?;
    Some(crate::sso::session::sign_out_of_provider(&sso, &auth).await)
}
