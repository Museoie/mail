//! Turning an anonymous caller into an authenticated one.

use crate::http::request::HttpRequest;
use crate::models::SignedUser;
use crate::postgres::DbError;

/// Attaches `user` to a freshly generated session id. The id the caller
/// arrived with is never carried across the privilege transition: a cookie
/// planted before the call would otherwise come back authenticated. The
/// assignment waits for the reissue, so a session that could not be reissued
/// is left anonymous.
pub async fn issue_authenticated_session(req: &mut HttpRequest, user: SignedUser) -> Result<(), DbError> {
    req.session.regenerate().await?;
    req.session.user = Some(user);
    Ok(())
}
