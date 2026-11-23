//! `/api/push/*`: the VAPID public key and browser push subscriptions.

use std::sync::OnceLock;

use super::route::{auth_required, dispatch, ApiResponse, Reply, Route};
use crate::http::request::HttpRequest;
use crate::http::respond::Response;
use crate::{env, push};
use crate::push_validation::validate_push_subscription;

async fn get_public_key(_req: &mut HttpRequest) -> anyhow::Result<Reply> {
    // The variable's own value, as the Bun server answers it: also when it
    // is set but empty (`.env.example` ships it that way), which the push
    // module reads as no key at all.
    let key = push::get_push_public_key().or_else(|| env::var("PUSH_VAPID_PUBLIC_KEY"));
    Ok(match key {
        Some(key) => ApiResponse::success_with(key),
        None => ApiResponse::success(),
    }
    .into())
}

async fn get_refresh(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let owner = req.session.user.as_ref().map(|user| user.id.clone());
    let refreshed = match owner {
        Some(user_id) => push::refresh_subscription(req.param("id"), &user_id).await,
        None => false,
    };
    Ok(if refreshed {
        ApiResponse::success()
    } else {
        ApiResponse::failed("No subscription found")
    }
    .into())
}

async fn post_subscribe(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = req.session.user.clone();
    let user = user.ok_or_else(|| anyhow::anyhow!("the route runs behind the session gate"))?;

    let Some(body) = req.body_object() else {
        return Ok(ApiResponse::failed("Invalid request body.").into());
    };
    let subscription = match validate_push_subscription(body.get("subscription")) {
        Ok(subscription) => subscription,
        Err(message) => return Ok(ApiResponse::failed(message).into()),
    };
    Ok(match push::store_subscription(&user.id, &subscription).await {
        Some(id) => ApiResponse::success_with(id),
        None => ApiResponse::failed("Failed to store subscription"),
    }
    .into())
}

// Scoped to the session user, so one user cannot delete another's by id.
async fn delete_subscribe(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = req.session.user.clone();
    let user = user.ok_or_else(|| anyhow::anyhow!("the route runs behind the session gate"))?;
    Ok(if push::delete_subscription_for_user(req.param("id"), &user.id).await {
        ApiResponse::success()
    } else {
        ApiResponse::failed("No subscription found")
    }
    .into())
}

fn routes() -> &'static [Route] {
    static ROUTES: OnceLock<Vec<Route>> = OnceLock::new();
    ROUTES.get_or_init(|| {
        vec![
            Route::new("GET", "/public-key", |req| Box::pin(get_public_key(req))),
            Route::new("GET", "/refresh/:id", |req| Box::pin(get_refresh(req))),
            Route::new("POST", "/subscribe", |req| Box::pin(post_subscribe(req))),
            Route::new("DELETE", "/subscribe/:id", |req| Box::pin(delete_subscribe(req))),
        ]
    })
}

/// `/api/push/*`, with `path` relative to that mount.
pub async fn handle_push(req: &mut HttpRequest, path: &str) -> anyhow::Result<Option<Response>> {
    // /public-key is needed before any subscription exists, and /refresh/:id
    // is called by a service worker holding only a subscription id.
    if path != "/public-key" && !path.starts_with("/refresh") {
        if let Some(denied) = auth_required(req) {
            return Ok(Some(denied));
        }
    }
    dispatch(routes(), req, path).await
}
