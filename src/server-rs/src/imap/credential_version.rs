//! The credential generation a grant is checked against
//! (imap/credential-version.ts).

use anyhow::Result;

use crate::users::{get_user, UserQuery};

/// The credential row's current `password_version` for `username`: the
/// generation the revocation fence compares a grant's stamped version
/// against.
///
/// Rows that predate the fence read back as generation 1, the default the
/// migration gives every existing row. A missing row is `None`: the account
/// was deleted (or soft-deleted, which the read leaves out), and every grant
/// it issued is revoked with it.
pub async fn get_credential_password_version(username: &str) -> Result<Option<i64>> {
    let user = get_user(&UserQuery::username(username)).await?;
    Ok(user.map(|user| user.password_version.unwrap_or(1)))
}
