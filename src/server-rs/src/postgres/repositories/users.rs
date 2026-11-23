//! The `users` table.

use anyhow::{anyhow, Result};

use crate::params;
use crate::postgres::{pool, DbError, Param, Row, SOFT_DELETE_CONDITION};
use crate::verify_password::hash_password;

/// A row of `users`, registered or not.
#[derive(Debug, Clone)]
pub struct UserModel {
    pub user_id: String,
    pub username: String,
    /// NULL on a sign-up that has not set a password yet.
    pub password: Option<String>,
    /// NULL on rows that predate the credential generation; read it as 1.
    pub password_version: Option<i64>,
    pub email: Option<String>,
    pub expiry: Option<String>,
    pub token: Option<String>,
    pub updated: Option<String>,
    pub is_deleted: Option<bool>,
    pub imap_uid_validity: Option<i64>,
    pub auth_sub: Option<String>,
}

impl UserModel {
    pub fn from_row(row: &Row) -> UserModel {
        UserModel {
            user_id: row.text("user_id"),
            username: row.text("username"),
            password: row.opt_text("password"),
            password_version: row.opt_i64("password_version"),
            email: row.opt_text("email"),
            expiry: row.opt_text("expiry"),
            token: row.opt_text("token"),
            updated: row.opt_text("updated"),
            is_deleted: row.opt_bool("is_deleted"),
            imap_uid_validity: row.opt_i64("imap_uid_validity"),
            auth_sub: row.opt_text("auth_sub"),
        }
    }

    /// The row as an account, a sign-up still waiting for its password
    /// included: that one is found without a password, so its token can be
    /// issued again and set-info can complete it. Nothing signs in on it.
    pub fn to_user(&self) -> UserRecord {
        UserRecord {
            user_id: self.user_id.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            password_version: self.password_version,
            email: self.email.clone(),
            token: self.token.clone(),
            expiry: self.expiry.clone(),
        }
    }
}

/// An account as the repository returns it.
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub user_id: String,
    pub username: String,
    /// `None` on a sign-up that has not set a password yet.
    pub password: Option<String>,
    pub password_version: Option<i64>,
    pub email: Option<String>,
    pub token: Option<String>,
    pub expiry: Option<String>,
}

/// Which account to look for; every field given must match.
#[derive(Debug, Clone, Default)]
pub struct UserFilter {
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub email: Option<String>,
}

/// One live row matching every given column, read as a model.
async fn query_one(filters: &[(&str, &str)]) -> Result<Option<UserModel>, DbError> {
    let mut conditions = Vec::new();
    let mut values: Vec<Param> = Vec::new();
    for (column, value) in filters {
        values.push(Param::from(*value));
        conditions.push(format!("{column} = ${}", values.len()));
    }
    conditions.push(SOFT_DELETE_CONDITION.to_string());
    values.push(Param::from(1));
    let sql = format!(
        "SELECT * FROM users WHERE {} LIMIT ${}",
        conditions.join(" AND "),
        values.len()
    );
    let result = pool().query(&sql, &values).await?;
    Ok(result.rows.first().map(UserModel::from_row))
}

/// The account matching `filter`, an unfinished sign-up included, or `None`.
/// With no filter at all there is nothing to look for.
pub async fn search_user(filter: &UserFilter) -> Result<Option<UserRecord>> {
    let non_empty = |field: &Option<String>| field.clone().filter(|v| !v.is_empty());
    let mut filters: Vec<(&str, String)> = Vec::new();
    if let Some(id) = non_empty(&filter.user_id) {
        filters.push(("user_id", id));
    }
    if let Some(username) = non_empty(&filter.username) {
        filters.push(("username", username));
    }
    if let Some(email) = non_empty(&filter.email) {
        filters.push(("email", email));
    }
    if filters.is_empty() {
        return Ok(None);
    }
    let borrowed: Vec<(&str, &str)> = filters.iter().map(|(c, v)| (*c, v.as_str())).collect();
    Ok(query_one(&borrowed).await?.map(|model| model.to_user()))
}

/// The row with this id, registered or not.
pub async fn find_user_model_by_id(user_id: &str) -> Result<Option<UserModel>, DbError> {
    query_one(&[("user_id", user_id)]).await
}

/// The account with this username, including an unfinished sign-up.
pub async fn find_user_by_username(username: &str) -> Result<Option<UserModel>, DbError> {
    query_one(&[("username", username)]).await
}

/// The account a single sign-on subject is linked to, if any.
pub async fn find_user_by_auth_sub(sub: &str) -> Result<Option<UserModel>, DbError> {
    query_one(&[("auth_sub", sub)]).await
}

/// What `write_user` creates or updates.
#[derive(Debug, Clone, Default)]
pub struct WriteUser {
    pub user_id: Option<String>,
    pub username: String,
    /// Plain text; hashed here. `None` leaves an existing row's stored password untouched.
    pub password: Option<String>,
    pub email: Option<String>,
    /// Carried onto an existing row only when given.
    pub password_version: Option<i64>,
}

/// Creates the account, or updates the row with the same `user_id`. Returns
/// the row's id.
pub async fn write_user(user: WriteUser) -> Result<Option<String>> {
    let hashed = match user.password.as_deref().filter(|p| !p.is_empty()) {
        Some(password) => Some(hash_password(password).await),
        None => None,
    };

    let mut columns = vec!["username".to_string()];
    let mut values: Vec<Param> = vec![Param::from(&user.username)];
    let mut add = |column: &str, value: Param, columns: &mut Vec<String>| {
        columns.push(column.to_string());
        values.push(value);
    };
    if let Some(hashed) = hashed {
        add("password", Param::from(hashed), &mut columns);
    }
    if let Some(user_id) = user.user_id.filter(|v| !v.is_empty()) {
        add("user_id", Param::from(user_id), &mut columns);
    }
    if let Some(email) = user.email.filter(|v| !v.is_empty()) {
        add("email", Param::from(email), &mut columns);
    }
    if let Some(version) = user.password_version {
        add("password_version", Param::from(version), &mut columns);
    }

    let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("${i}")).collect();
    let mut updates: Vec<String> = columns
        .iter()
        .filter(|column| *column != "user_id")
        .map(|column| format!("{column} = EXCLUDED.{column}"))
        .collect();
    updates.push("updated = CURRENT_TIMESTAMP".to_string());
    let sql = format!(
        "INSERT INTO users (updated, {}) VALUES (CURRENT_TIMESTAMP, {}) ON CONFLICT (user_id) DO UPDATE SET {} RETURNING *",
        columns.join(", "),
        placeholders.join(", "),
        updates.join(", ")
    );
    let result = pool().query(&sql, &values).await?;
    Ok(result.rows.first().map(|row| row.text("user_id")))
}

/// Rotates a credential and bumps its generation in one UPDATE, so two
/// concurrent rotations cannot land different hashes on one generation.
/// Returns the generation the row now carries, or `None` when it is gone.
pub async fn rotate_user_password(
    user_id: &str,
    password_hash: &str,
    username: &str,
    token: Option<&str>,
    expiry: Option<&str>,
) -> Result<Option<i64>, DbError> {
    let result = pool()
        .query(
            "UPDATE users SET password = $2, password_version = COALESCE(password_version, 1) + 1, username = $3, token = $4, expiry = $5, updated = CURRENT_TIMESTAMP WHERE user_id = $1 RETURNING password_version",
            &params![user_id, password_hash, username, token, expiry],
        )
        .await?;
    Ok(result.rows.first().and_then(|row| row.opt_i64("password_version")))
}

/// Sets columns on one row by primary key, stamping `updated`. Returns
/// whether the row exists.
pub async fn update_user_columns(user_id: &str, columns: &[(&str, Param)]) -> Result<bool, DbError> {
    if columns.is_empty() {
        return Ok(false);
    }
    let mut sets = vec!["updated = CURRENT_TIMESTAMP".to_string()];
    let mut values: Vec<Param> = Vec::new();
    for (column, value) in columns {
        values.push(value.clone());
        sets.push(format!("{column} = ${}", values.len()));
    }
    values.push(Param::from(user_id));
    let sql = format!(
        "UPDATE users SET {} WHERE user_id = ${} RETURNING user_id",
        sets.join(", "),
        values.len()
    );
    Ok(!pool().query(&sql, &values).await?.rows.is_empty())
}

/// Creates the placeholder row a sign-up token is issued against.
pub async fn insert_pending_user(
    user_id: &str,
    username: &str,
    email: &str,
    token: &str,
    expiry: &str,
) -> Result<(), DbError> {
    pool()
        .query(
            "INSERT INTO users (updated, user_id, username, email, token, expiry) VALUES (CURRENT_TIMESTAMP, $1, $2, $3, $4, $5) RETURNING user_id",
            &params![user_id, username, email, token, expiry],
        )
        .await?;
    Ok(())
}

/// Removes the row outright. Returns whether one was removed.
pub async fn hard_delete_user(user_id: &str) -> Result<bool, DbError> {
    let result = pool()
        .query("DELETE FROM users WHERE user_id = $1 RETURNING user_id", &params![user_id])
        .await?;
    Ok(result.any())
}

/// The IMAP UIDVALIDITY for a user, initialized to the current Unix time on
/// first use so it is unique and only ever increases.
pub async fn get_imap_uid_validity(user_id: &str) -> Result<i64> {
    let model = find_user_model_by_id(user_id)
        .await?
        .ok_or_else(|| anyhow!("User not found: {user_id}"))?;
    if let Some(validity) = model.imap_uid_validity {
        return Ok(validity);
    }
    let validity = crate::js::now_ms() / 1000;
    update_user_columns(user_id, &[("imap_uid_validity", Param::from(validity))]).await?;
    Ok(validity)
}

/// Signed-up accounts with this address ignoring case, not linked to a single
/// sign-on subject and not deleted, minus `excluded_usernames`. At most two:
/// enough to tell "exactly one" from "more than one".
pub async fn find_unlinked_users_by_email(
    email: &str,
    excluded_usernames: &[String],
) -> Result<Vec<UserModel>, DbError> {
    let sql = format!(
        "SELECT * FROM users WHERE LOWER(email) = LOWER($1) AND auth_sub IS NULL AND password IS NOT NULL AND username <> ALL($2::text[]) AND {SOFT_DELETE_CONDITION} LIMIT 2"
    );
    let result = pool().query(&sql, &params![email, excluded_usernames]).await?;
    Ok(result.rows.iter().map(UserModel::from_row).collect())
}

/// Links an account that is not linked yet. `None` when another sign-in
/// linked the account, or this subject, first.
pub async fn link_user_auth_sub(user_id: &str, sub: &str) -> Result<Option<UserModel>, DbError> {
    let result = pool()
        .query(
            "UPDATE users SET auth_sub = $1, updated = CURRENT_TIMESTAMP WHERE user_id = $2 AND auth_sub IS NULL AND is_deleted IS NOT TRUE RETURNING *",
            &params![sub, user_id],
        )
        .await;
    match result {
        Ok(result) => Ok(result.rows.first().map(UserModel::from_row)),
        Err(error) if error.is_unique_violation() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Creates the account a first single sign-on makes. Its password is the hash
/// of a secret that exists nowhere, so no password sign-in can present it.
/// `None` when the username or the subject is taken.
pub async fn insert_sso_user(username: &str, email: &str, sub: &str) -> Result<Option<UserModel>, DbError> {
    let secret: [u8; 32] = rand::random();
    let password_hash = hash_password(&hex::encode(secret)).await;
    let result = pool()
        .query(
            "INSERT INTO users (updated, username, password, email, auth_sub) VALUES (CURRENT_TIMESTAMP, $1, $2, $3, $4) ON CONFLICT DO NOTHING RETURNING *",
            &params![username, password_hash, email, sub],
        )
        .await?;
    Ok(result.rows.first().map(UserModel::from_row))
}

pub async fn update_user_email(user_id: &str, email: &str) -> Result<(), DbError> {
    update_user_columns(user_id, &[("email", Param::from(email))]).await?;
    Ok(())
}

/// Moves the linked account's credential generation on: every session and
/// mail connection stamped with an older one fails its next check. Returns
/// `(user_id, username)`, or `None` when no account is linked.
pub async fn bump_password_version_by_auth_sub(sub: &str) -> Result<Option<(String, String)>, DbError> {
    let result = pool()
        .query(
            "UPDATE users SET password_version = COALESCE(password_version, 1) + 1, updated = CURRENT_TIMESTAMP WHERE auth_sub = $1 RETURNING user_id, username, password_version",
            &params![sub],
        )
        .await?;
    Ok(result.rows.first().map(|row| (row.text("user_id"), row.text("username"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_sign_up() -> UserModel {
        UserModel {
            user_id: "u-1".into(),
            username: "user_abc12345".into(),
            password: None,
            password_version: Some(1),
            email: Some("a@b.c".into()),
            expiry: None,
            token: Some("tok".into()),
            updated: None,
            is_deleted: Some(false),
            imap_uid_validity: None,
            auth_sub: None,
        }
    }

    #[test]
    fn a_sign_up_without_a_password_reads_as_an_account_without_one() {
        // It used to be an error, which made every route that met the row a
        // 500 and left the sign-up impossible to finish.
        let record = pending_sign_up().to_user();
        assert_eq!(record.password, None);
        assert_eq!(record.username, "user_abc12345");
        assert_eq!(record.token.as_deref(), Some("tok"));
    }
}
