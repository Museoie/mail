//! `/api/users/*`: who is signed in, password sign-in and sign-out, and the
//! sign-up / password-reset token flow.

use std::sync::OnceLock;

use serde_json::{json, Value};

use super::route::{dispatch, mounted_path, ApiResponse, Reply, Route};
use crate::http::issue_session::issue_authenticated_session;
use crate::http::rate_limit::{get_client_ip, login_identity_of, login_limiter, token_limiter, RateLimiter};
use crate::http::request::HttpRequest;
use crate::http::respond::{json, Response};
use crate::http::sso_hooks::{is_local_login_allowed, sign_out_of_provider, sso_mode};
use crate::postgres::initialize::VERSION;
use crate::read_only::{
    delivers_to_admin_mailbox, is_reserved_username, resolve_session_user, ADMIN_RO_USERNAME,
};
use crate::users::{
    create_authentication_mail, create_token, get_user, is_valid_email, set_user_info, start_timer, UserQuery,
};
use crate::verify_password::verify_password;
use crate::logger;

pub const LOCAL_SIGN_IN_OFF_MESSAGE: &str =
    "Password sign-in is turned off on this server. Sign in with SSO instead.";

const INVALID_CREDENTIALS: &str = "Invalid credentials.";

async fn get_login(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = req.session.user.clone();
    let mut body = serde_json::Map::new();
    if let Some(user) = &user {
        body.insert("user".into(), serde_json::to_value(user)?);
    }
    body.insert("app".into(), json!({ "version": VERSION }));
    body.insert("sso".into(), serde_json::to_value(sso_mode())?);
    Ok(ApiResponse {
        status: "success",
        body: Some(Value::Object(body)),
        message: user.is_none().then(|| "Not logged in.".to_string()),
    }
    .into())
}

/// A field that must be a string when present: `Err` for any other type.
fn optional_string(body: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, ()> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

async fn post_login(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let Some(body) = req.body_object() else {
        return Ok(ApiResponse::failed("Invalid request body.").into());
    };
    let Some(password) = body.get("password").and_then(Value::as_str).filter(|p| !p.is_empty()) else {
        return Ok(ApiResponse::failed(INVALID_CREDENTIALS).into());
    };
    let password = password.to_string();
    let (Ok(email), Ok(username)) = (optional_string(body, "email"), optional_string(body, "username")) else {
        return Ok(ApiResponse::failed(INVALID_CREDENTIALS).into());
    };

    let user = get_user(&UserQuery { id: None, username, email }).await?;
    let signed_user = user.as_ref().and_then(|user| user.get_signed());
    let stored_hash = user.as_ref().and_then(|user| user.password.clone());
    let pw_matches = verify_password(Some(&password), stored_hash.as_deref()).await;

    let ip = get_client_ip(req);
    // Failures accumulate per (address, account); a success forgives only
    // this account's own bucket, never the address's aggregate.
    let account = login_identity_of(&req.body);

    if !pw_matches || signed_user.is_none() {
        login_limiter().record_failure(&ip, account.as_deref());
        return Ok(ApiResponse::failed(INVALID_CREDENTIALS).into());
    }

    // The read-only credential signs in as admin's identity, marked read-only.
    let Some((session_user, is_read_only)) = resolve_session_user(signed_user).await? else {
        login_limiter().record_failure(&ip, account.as_deref());
        return Ok(ApiResponse::failed(INVALID_CREDENTIALS).into());
    };
    if is_read_only {
        logger::info(
            "HTTP LOGIN success (read-only)",
            json!({
                "component": "http",
                "authenticatedAs": ADMIN_RO_USERNAME,
                "effectiveUserId": session_user.id,
                "ip": ip,
            }),
        );
    }

    issue_authenticated_session(req, session_user.clone()).await?;
    // The store write is what makes the issued cookie mean anything, so it
    // lands before the response claims the login succeeded.
    req.session.save().await?;

    login_limiter().reset(&ip, account.as_deref());
    Ok(ApiResponse::success_with(session_user).into())
}

async fn delete_login(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let auth = req.session.auth.clone();
    req.session.destroy().await?;
    // A session from single sign-on also ends at the provider: the refresh
    // token is revoked, and the browser is told where to end that session.
    Ok(match sign_out_of_provider(auth).await {
        Some(Some(logout_url)) => ApiResponse::success_with(json!({ "logoutUrl": logout_url })),
        _ => ApiResponse::success(),
    }
    .into())
}

async fn post_token(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let ip = get_client_ip(req);
    let Some(body) = req.body_object() else {
        token_limiter().record_failure(&ip, None);
        return Ok(ApiResponse::failed("Invalid request body.").into());
    };
    let Some(email) = body.get("email").and_then(Value::as_str).filter(|email| is_valid_email(email)) else {
        token_limiter().record_failure(&ip, None);
        return Ok(ApiResponse::failed("Signup failed because email is invalid.").into());
    };
    let email = email.to_string();

    // The boot-seeded accounts take no part in this flow: without the gate an
    // outside caller mints a live reset token for admin from the address
    // alone. Same-shape success as a normal send, so no probe signal.
    let existing = get_user(&UserQuery::email(&email)).await?;
    if is_reserved_username(existing.as_ref().and_then(|user| user.username.as_deref())) {
        token_limiter().record_failure(&ip, None);
        return Ok(ApiResponse::success().into());
    }
    // Same for an address delivered into admin's own mailbox.
    if delivers_to_admin_mailbox(&email) {
        token_limiter().record_failure(&ip, None);
        return Ok(ApiResponse::success().into());
    }

    let admin_query = UserQuery::username("admin");
    let (admin, created) = tokio::join!(get_user(&admin_query), create_token(&email));
    let (admin, created) = (admin?, created?);
    let signed_admin = admin
        .and_then(|admin| admin.get_signed())
        .ok_or_else(|| anyhow::anyhow!("Admin user does not exist."))?;

    let mail = create_authentication_mail(&email, &created.token, created.username.as_deref());
    // No Sent record: the body carries a live token, and the sending identity
    // is admin, whose mailbox the read-only role reads by design.
    crate::mails::send::send_authentication_mail(&signed_admin, mail).await?;

    start_timer(&created.id);

    // Each successful send takes one slot of the per-address quota; a fault
    // above never reaches this line, so it does not burn the quota.
    token_limiter().record_failure(&ip, None);
    Ok(ApiResponse::success().into())
}

async fn post_set_info(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let Some(body) = req.body_object() else {
        return Ok(ApiResponse::failed("Invalid request body.").into());
    };
    let required = |key: &str| body.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    let Some(email) = required("email") else {
        return Ok(ApiResponse::failed("email is required and must be a string.").into());
    };
    let Some(username) = required("username") else {
        return Ok(ApiResponse::failed("username is required and must be a string.").into());
    };
    let Some(password) = required("password") else {
        return Ok(ApiResponse::failed("password is required and must be a string.").into());
    };
    let Ok(token) = optional_string(body, "token") else {
        return Ok(ApiResponse::failed("token must be a string.").into());
    };

    // Refused before the write: set_user_info updates the row of an existing
    // user unconditionally.
    let existing = get_user(&UserQuery::email(&email)).await?;
    if is_reserved_username(existing.as_ref().and_then(|user| user.username.as_deref())) {
        return Ok(ApiResponse::failed(INVALID_CREDENTIALS).into());
    }

    let user = set_user_info(&email, &username, &password, token.as_deref()).await?;
    issue_authenticated_session(req, user.clone()).await?;
    Ok(ApiResponse::success_with(user).into())
}

struct UserRoutes {
    all: Vec<Route>,
    /// Password sign-in, sign-up and reset: `(method, path)` of the routes
    /// single sign-on turns off unless local login is kept.
    local_sign_in: Vec<Route>,
}

fn routes() -> &'static UserRoutes {
    static ROUTES: OnceLock<UserRoutes> = OnceLock::new();
    ROUTES.get_or_init(|| UserRoutes {
        all: vec![
            Route::new("GET", "/login", |req| Box::pin(get_login(req))),
            Route::new("POST", "/login", |req| Box::pin(post_login(req))),
            Route::new("DELETE", "/login", |req| Box::pin(delete_login(req))),
            Route::new("POST", "/token", |req| Box::pin(post_token(req))),
            Route::new("POST", "/set-info", |req| Box::pin(post_set_info(req))),
        ],
        local_sign_in: vec![
            Route::new("POST", "/login", |req| Box::pin(post_login(req))),
            Route::new("POST", "/token", |req| Box::pin(post_token(req))),
            Route::new("POST", "/set-info", |req| Box::pin(post_set_info(req))),
        ],
    })
}

/// `/api/users/*`, with `path` relative to that mount.
pub async fn handle_users(req: &mut HttpRequest, path: &str) -> anyhow::Result<Option<Response>> {
    let routes = routes();
    if !is_local_login_allowed() {
        for route in &routes.local_sign_in {
            if route.matches(&req.method, path)?.is_some() {
                return Ok(Some(json(&json!({ "status": "failed", "message": LOCAL_SIGN_IN_OFF_MESSAGE }), 403)));
            }
        }
    }
    // The limiters apply to POST only.
    if req.method == "POST" {
        let limiters: [(&str, &RateLimiter); 2] = [("/login", login_limiter()), ("/token", token_limiter())];
        for (prefix, limiter) in limiters {
            if mounted_path(path, prefix).is_some() {
                if let Some(refused) = limiter.check(req) {
                    return Ok(Some(refused));
                }
            }
        }
    }
    dispatch(&routes.all, req, path).await
}
