//! The session-scoped read-only role.
//!
//! Authenticating as `admin-ro` produces a session whose effective identity is
//! `admin`, so every read path returns admin's data, while every mutating
//! surface refuses the write.

use anyhow::Result;

use crate::models::SignedUser;
use crate::users::{get_user, UserQuery};
use crate::util::address_to_username;

/// Reserved username for the read-only administrative role.
pub const ADMIN_RO_USERNAME: &str = "admin-ro";

/// Reserved username for the primary administrative role.
pub const ADMIN_USERNAME: &str = "admin";

/// True for the boot-seeded accounts, which take no part in the
/// `/token` → `/set-info` email flow.
pub fn is_reserved_username(username: Option<&str>) -> bool {
    matches!(username, Some(ADMIN_USERNAME) | Some(ADMIN_RO_USERNAME))
}

/// True for an address this server delivers into admin's own mailbox.
pub fn delivers_to_admin_mailbox(email: &str) -> bool {
    address_to_username(email) == ADMIN_USERNAME
}

/// The refusal message when `user` is a read-only session, otherwise `None`.
///
/// ```ignore
/// if let Some(message) = refuse_read_only(Some(&user), "Deleting mail") {
///     return Ok(failed(message));
/// }
/// ```
pub fn refuse_read_only(user: Option<&SignedUser>, context: &str) -> Option<String> {
    let user = user.filter(|user| user.read_only())?;
    let attributed_to = user
        .authenticated_as
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(ADMIN_RO_USERNAME);
    Some(format!("{context} not permitted for read-only user ({attributed_to})."))
}

/// The effective user's identity, attributed to the read-only credential the
/// caller authenticated with. The effective identity's password-reset token
/// is dropped rather than copied across.
pub fn remap_read_only_session(effective: &SignedUser, authenticated_as: &str) -> SignedUser {
    let mut remapped = effective.clone();
    remapped.is_read_only = Some(true);
    remapped.authenticated_as = Some(authenticated_as.to_string());
    remapped.token = None;
    remapped.expiry = None;
    remapped
}

/// The identity to attach to a session for an authenticated credential: for
/// `admin-ro`, admin's identity marked read-only; for anyone else, their own.
/// `None` when the read-only credential authenticated but no admin row exists.
pub async fn resolve_session_user(signed_user: Option<SignedUser>) -> Result<Option<(SignedUser, bool)>> {
    let Some(signed_user) = signed_user else {
        return Ok(None);
    };
    if signed_user.username != ADMIN_RO_USERNAME {
        return Ok(Some((signed_user, false)));
    }
    let admin = get_user(&UserQuery::username(ADMIN_USERNAME)).await?;
    Ok(admin
        .and_then(|admin| admin.get_signed())
        .map(|admin| (remap_read_only_session(&admin, ADMIN_RO_USERNAME), true)))
}
