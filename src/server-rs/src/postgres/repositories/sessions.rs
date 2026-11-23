//! The `sessions` table and the session store built on it.

use chrono::{DateTime, Utc};
use serde_json::json;

use crate::models::{RuntimeCookie, RuntimeSession, SignedUser, SsoSession};
use crate::postgres::repositories::users::find_user_model_by_id;
use crate::postgres::{pool, DbError, Param, Row};
use crate::{js, logger, params};

/// A row of `sessions`.
#[derive(Debug, Clone)]
pub struct SessionModel {
    pub session_id: String,
    pub session_user_id: String,
    pub session_username: String,
    pub session_email: String,
    pub session_is_read_only: Option<bool>,
    pub session_authenticated_as: Option<String>,
    pub session_password_version: Option<i64>,
    pub auth_sid: Option<String>,
    pub auth_refresh_token: Option<String>,
    pub auth_recheck_at: Option<DateTime<Utc>>,
    pub cookie_original_max_age: Option<i64>,
    pub cookie_max_age: Option<i64>,
    pub cookie_signed: Option<bool>,
    pub cookie_expires: Option<DateTime<Utc>>,
    pub cookie_http_only: Option<bool>,
    pub cookie_path: Option<String>,
    pub cookie_domain: Option<String>,
    pub cookie_secure: Option<String>,
    pub cookie_same_site: Option<String>,
}

impl SessionModel {
    fn from_row(row: &Row) -> SessionModel {
        SessionModel {
            session_id: row.text("session_id"),
            session_user_id: row.text("session_user_id"),
            session_username: row.text("session_username"),
            session_email: row.text("session_email"),
            session_is_read_only: row.opt_bool("session_is_read_only"),
            session_authenticated_as: row.opt_text("session_authenticated_as"),
            session_password_version: row.opt_i64("session_password_version"),
            auth_sid: row.opt_text("auth_sid"),
            auth_refresh_token: row.opt_text("auth_refresh_token"),
            auth_recheck_at: row.opt_time("auth_recheck_at"),
            cookie_original_max_age: row.opt_i64("cookie_original_max_age"),
            cookie_max_age: row.opt_i64("cookie_max_age"),
            cookie_signed: row.opt_bool("cookie_signed"),
            cookie_expires: row.opt_time("cookie_expires"),
            cookie_http_only: row.opt_bool("cookie_http_only"),
            cookie_path: row.opt_text("cookie_path"),
            cookie_domain: row.opt_text("cookie_domain"),
            cookie_secure: row.opt_text("cookie_secure"),
            cookie_same_site: row.opt_text("cookie_same_site"),
        }
    }
}

/// The row with this id, or `None`. A failed read is an error, so a store
/// fault is never mistaken for a session that does not exist.
pub async fn search_session(session_id: &str) -> Result<Option<SessionModel>, DbError> {
    let result = pool()
        .query("SELECT * FROM sessions WHERE session_id = $1 LIMIT $2", &params![session_id, 1])
        .await?;
    Ok(result.rows.first().map(SessionModel::from_row))
}

/// What a `cookie_secure` or `cookie_same_site` column holds: JSON text, read
/// with `JSON.parse`. NULL and the empty string are no value at all. Text
/// that is not JSON is a fault of the store, as it is to the Bun server,
/// where the parse throws and the request carrying that session fails.
fn parse_cookie_json(column: &str, stored: Option<&str>) -> Result<Option<serde_json::Value>, DbError> {
    match stored.filter(|text| !text.is_empty()) {
        None => Ok(None),
        Some(text) => js::json_parse(text)
            .map(Some)
            .ok_or_else(|| DbError::new(format!("JSON Parse error: {column} holds {text:?}, which is not JSON"))),
    }
}

/// Writes the session row. Columns the cookie leaves unset keep their stored value.
pub async fn update_session(session_id: &str, session: &RuntimeSession) -> Result<(), DbError> {
    let RuntimeSession { user, cookie, auth } = session;
    let mut columns: Vec<(&str, Param)> = vec![
        ("session_id", Param::from(session_id)),
        ("session_user_id", Param::from(&user.id)),
        ("session_username", Param::from(&user.username)),
        ("session_email", Param::from(&user.email)),
        // Kept for a previous version still reading these rows; this version
        // issues no read-only session.
        ("session_is_read_only", Param::Null),
        ("session_authenticated_as", Param::Null),
        // The generation the caller authenticated against, never a fresh read
        // of the user row: a login that straddled a rotation must stamp the
        // stale generation and fail its next comparison.
        ("session_password_version", Param::from(user.password_version.unwrap_or(1))),
        ("auth_sid", Param::from(auth.as_ref().map(|a| a.sid.clone()))),
        ("auth_refresh_token", Param::from(auth.as_ref().and_then(|a| a.refresh_token.clone()))),
        ("auth_recheck_at", Param::from(auth.as_ref().map(|a| a.recheck_at))),
        ("cookie_original_max_age", Param::from(cookie.original_max_age)),
    ];
    let mut optional = |column: &'static str, value: Option<Param>| {
        if let Some(value) = value {
            columns.push((column, value));
        }
    };
    optional("cookie_max_age", cookie.max_age.map(Param::from));
    optional("cookie_signed", cookie.signed.map(Param::from));
    optional("cookie_expires", cookie.expires.map(Param::from));
    optional("cookie_http_only", cookie.http_only.map(Param::from));
    optional("cookie_path", cookie.path.clone().map(Param::from));
    optional("cookie_domain", cookie.domain.clone().map(Param::from));
    // `JSON.stringify` of whatever the value is, the form the columns store.
    optional("cookie_secure", cookie.secure.as_ref().map(|secure| Param::from(js::json_stringify(secure))));
    optional("cookie_same_site", cookie.same_site.as_ref().map(|value| Param::from(js::json_stringify(value))));

    let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
    let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("${i}")).collect();
    let mut updates: Vec<String> = names
        .iter()
        .filter(|name| **name != "session_id")
        .map(|name| format!("{name} = EXCLUDED.{name}"))
        .collect();
    updates.push("updated = CURRENT_TIMESTAMP".to_string());
    let sql = format!(
        "INSERT INTO sessions (updated, {}) VALUES (CURRENT_TIMESTAMP, {}) ON CONFLICT (session_id) DO UPDATE SET {} RETURNING *",
        names.join(", "),
        placeholders.join(", "),
        updates.join(", ")
    );
    let values: Vec<Param> = columns.into_iter().map(|(_, value)| value).collect();
    pool().query(&sql, &values).await?;
    Ok(())
}

/// Returns whether a row was deleted.
pub async fn delete_session(session_id: &str) -> Result<bool, DbError> {
    let result = pool()
        .query("DELETE FROM sessions WHERE session_id = $1 RETURNING session_id", &params![session_id])
        .await?;
    Ok(result.any())
}

async fn delete_where(condition: &str, values: &[Param]) -> Result<u64, DbError> {
    let sql = format!("DELETE FROM sessions WHERE {condition} RETURNING session_id");
    Ok(pool().query(&sql, values).await?.row_count.unwrap_or(0))
}

/// Deletes every expired session and returns how many there were.
pub async fn purge_sessions() -> Result<u64, DbError> {
    delete_where("cookie_expires IS NOT NULL AND cookie_expires <= $1", &params![js::now_iso()]).await
}

/// Deletes every session issued to a user, whichever credential minted it.
pub async fn delete_sessions_for_user(user_id: &str) -> Result<u64, DbError> {
    delete_where("session_user_id = $1", &params![user_id]).await
}

/// Deletes every session started through the single sign-on session `sid`.
pub async fn delete_sessions_by_auth_sid(sid: &str) -> Result<u64, DbError> {
    delete_where("auth_sid = $1", &params![sid]).await
}

/// Persists HTTP sessions in PostgreSQL.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresSessionStore;

impl PostgresSessionStore {
    /// Creates the store and starts the hourly sweep of expired sessions.
    pub fn start() -> PostgresSessionStore {
        tokio::spawn(async {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
            loop {
                interval.tick().await;
                if let Err(error) = purge_sessions().await {
                    logger::error_with("Failed to purge expired sessions", json!({}), &error);
                }
            }
        });
        PostgresSessionStore
    }

    /// The live session with this id. An expired row, one whose user is gone,
    /// or one minted before the user's credential was last rotated, is
    /// deleted and reads as absent.
    pub async fn get(&self, session_id: &str) -> Result<Option<RuntimeSession>, DbError> {
        let Some(model) = search_session(session_id).await? else {
            return Ok(None);
        };

        let expired = model.cookie_expires.map_or(true, |expires| expires < Utc::now());
        if expired {
            let id = session_id.to_string();
            tokio::spawn(async move {
                if let Err(error) = delete_session(&id).await {
                    logger::error_with("Failed to delete session", json!({ "session_id": id }), &error);
                }
            });
            return Ok(None);
        }

        // A session the retired read-only admin login issued carries admin's
        // user id. This version has no read-only session, so serving it would
        // hand that login admin's whole account: it ends here instead. A
        // previous version still running during a rolling deploy may write one.
        if model.session_is_read_only == Some(true) {
            logger::info("Session rejected: issued by the retired read-only login", json!({ "session_id": session_id }));
            if let Err(error) = delete_session(session_id).await {
                logger::error_with("Failed to delete session", json!({ "session_id": session_id }), &error);
            }
            return Ok(None);
        }

        // The revocation fence: a grant proves itself current on every read.
        let stamped_version = model.session_password_version.unwrap_or(1);
        // A session ends with its account; the read leaves out a soft-deleted
        // row, so a deleted and a soft-deleted user both find nothing.
        let Some(user_row) = find_user_model_by_id(&model.session_user_id).await? else {
            logger::info(
                "Session rejected: its user no longer exists",
                json!({ "session_id": session_id, "user_id": model.session_user_id }),
            );
            if let Err(error) = delete_session(session_id).await {
                logger::error_with("Failed to delete session", json!({ "session_id": session_id }), &error);
            }
            return Ok(None);
        };
        let current_version = user_row.password_version.unwrap_or(1);
        if stamped_version < current_version {
            logger::info(
                "Session rejected: credential rotated after the session was issued",
                json!({ "session_id": session_id, "user_id": model.session_user_id }),
            );
            if let Err(error) = delete_session(session_id).await {
                logger::error_with("Failed to delete session", json!({ "session_id": session_id }), &error);
            }
            return Ok(None);
        }

        let mut user = SignedUser::new(
            Some(model.session_user_id.clone()),
            Some(model.session_username.clone()),
            Some(model.session_email.clone()),
        );
        // Stored values are used as they are, even an empty one.
        user.id = model.session_user_id;
        user.username = model.session_username;
        user.email = model.session_email;
        user.password_version = Some(stamped_version);

        let cookie = RuntimeCookie {
            original_max_age: model.cookie_original_max_age.filter(|age| *age != 0),
            max_age: model.cookie_max_age,
            signed: model.cookie_signed,
            expires: model.cookie_expires,
            http_only: model.cookie_http_only,
            path: model.cookie_path,
            domain: model.cookie_domain,
            secure: parse_cookie_json("cookie_secure", model.cookie_secure.as_deref())?,
            same_site: parse_cookie_json("cookie_same_site", model.cookie_same_site.as_deref())?,
        };

        let auth = match (model.auth_sid, model.auth_recheck_at) {
            (Some(sid), Some(recheck_at)) if !sid.is_empty() => Some(SsoSession {
                sid,
                refresh_token: model.auth_refresh_token,
                recheck_at,
            }),
            _ => None,
        };

        Ok(Some(RuntimeSession { user, cookie, auth }))
    }

    pub async fn set(&self, session_id: &str, session: &RuntimeSession) -> Result<(), DbError> {
        update_session(session_id, session).await
    }

    pub async fn destroy(&self, session_id: &str) -> Result<(), DbError> {
        delete_session(session_id).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(secure: Option<&str>, same_site: Option<&str>) -> RuntimeCookie {
        RuntimeCookie {
            original_max_age: None,
            max_age: None,
            signed: None,
            expires: None,
            http_only: None,
            path: None,
            domain: None,
            secure: parse_cookie_json("cookie_secure", secure).unwrap(),
            same_site: parse_cookie_json("cookie_same_site", same_site).unwrap(),
        }
    }

    #[test]
    fn cookie_columns_hold_json() {
        assert!(cookie(Some("true"), None).is_secure());
        assert!(!cookie(Some("false"), None).is_secure());
        assert!(!cookie(None, None).is_secure());
        assert!(!cookie(Some(""), None).is_secure());
        // Whatever JSON.parse makes of the text, by JavaScript's truthiness.
        assert!(cookie(Some("\"auto\""), None).is_secure());
        assert!(cookie(Some("1"), None).is_secure());
        assert!(!cookie(Some("0"), None).is_secure());
        assert!(!cookie(Some("null"), None).is_secure());

        assert_eq!(cookie(None, Some("\"strict\"")).same_site_attribute(), Some("Strict"));
        assert_eq!(cookie(None, Some("true")).same_site_attribute(), Some("Strict"));
        assert_eq!(cookie(None, Some("\"lax\"")).same_site_attribute(), Some("Lax"));
        assert_eq!(cookie(None, Some("\"none\"")).same_site_attribute(), Some("None"));
        for no_attribute in [None, Some(""), Some("false"), Some("\"other\""), Some("null"), Some("1")] {
            assert_eq!(cookie(None, no_attribute).same_site_attribute(), None, "{no_attribute:?}");
        }
    }

    #[test]
    fn cookie_columns_that_are_not_json_are_a_fault() {
        assert!(parse_cookie_json("cookie_secure", Some("nonsense")).is_err());
        assert!(parse_cookie_json("cookie_same_site", Some("strict")).is_err());
        assert!(parse_cookie_json("cookie_same_site", Some("\"strict")).is_err());
    }

    #[test]
    fn cookie_columns_are_written_back_as_they_were_read() {
        for text in ["true", "false", "\"strict\"", "\"auto\"", "1", "null"] {
            let parsed = parse_cookie_json("cookie_secure", Some(text)).unwrap().unwrap();
            assert_eq!(js::json_stringify(&parsed), text);
        }
    }
}
