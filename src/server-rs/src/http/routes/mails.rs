//! `/api/mails/*` (http/routes/mails/index.ts).

mod headers;
pub mod list_page;
mod read;
mod spam;
mod write;

use std::sync::OnceLock;

use super::route::{auth_required, dispatch, mounted_path, Route};
use crate::http::request::HttpRequest;
use crate::http::respond::Response;
use crate::http::upload::{parse_uploads, remove_temp_files};
use crate::models::SignedUser;

fn routes() -> &'static [Route] {
    static ROUTES: OnceLock<Vec<Route>> = OnceLock::new();
    ROUTES.get_or_init(|| {
        vec![
            Route::new("GET", "/domain", |req| Box::pin(read::get_domain(req))),
            Route::new("GET", "/accounts", |req| Box::pin(read::get_accounts(req))),
            Route::new("GET", "/headers/:account", |req| Box::pin(headers::get_headers(req))),
            Route::new("GET", "/unified", |req| Box::pin(headers::get_unified_headers(req))),
            Route::new("GET", "/body/:id", |req| Box::pin(read::get_body(req))),
            Route::new("GET", "/thread/:id", |req| Box::pin(read::get_thread(req))),
            Route::new("GET", "/attachment/:id", |req| Box::pin(read::get_attachment(req))),
            Route::new("GET", "/search/:value", |req| Box::pin(read::get_search(req))),
            Route::new("GET", "/search-accounts/:value", |req| Box::pin(read::get_search_accounts(req))),
            Route::new("POST", "/mark", |req| Box::pin(write::post_mark(req))),
            Route::new("POST", "/send", |req| Box::pin(write::post_send(req))),
            Route::new("DELETE", "/:id", |req| Box::pin(write::delete_mail(req))),
            Route::new("POST", "/spam/mark", |req| Box::pin(spam::post_spam_mark(req))),
            Route::new("GET", "/spam-allowlist", |req| Box::pin(spam::get_allowlist(req))),
            Route::new("POST", "/spam-allowlist", |req| Box::pin(spam::post_allowlist(req))),
            Route::new("DELETE", "/spam-allowlist/:pattern", |req| Box::pin(spam::delete_allowlist(req))),
        ]
    })
}

/// The session user of a route behind the gate.
fn session_user(req: &HttpRequest) -> anyhow::Result<SignedUser> {
    req.session.user.clone().ok_or_else(|| anyhow::anyhow!("the route runs behind the session gate"))
}

/// `/api/mails/*`, with `path` relative to that mount.
pub async fn handle_mails(req: &mut HttpRequest, path: &str) -> anyhow::Result<Option<Response>> {
    // Every route requires a session except /domain (public info), in
    // exactly that spelling.
    if path != "/domain" {
        if let Some(denied) = auth_required(req) {
            return Ok(Some(denied));
        }
    }

    // Behind the gate, so an unauthenticated multipart body is never parsed
    // to disk. /send is the only route that reads the files.
    if mounted_path(path, "/send").is_none() {
        return dispatch(routes(), req, path).await;
    }
    let outcome = parse_uploads(req).await?;
    let result = match outcome.rejection {
        Some(rejection) => Ok(Some(rejection)),
        None => dispatch(routes(), req, path).await,
    };
    remove_temp_files(outcome.temp_files);
    result
}
