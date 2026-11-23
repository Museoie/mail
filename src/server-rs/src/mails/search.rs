//! Full-text search over the user's mail (mails/search.ts).

use serde_json::{Map, Value};

use super::headers::to_search_header_data;
use crate::js;
use crate::models::SignedUser;
use crate::postgres::repositories::mails::http::search_mails;

/// The user's mail matching `value`, best match first. A blank value finds
/// nothing. Spam is included and labelled; drafts and expunged mail are not.
pub async fn search_mail(user: &SignedUser, value: &str) -> Vec<Map<String, Value>> {
    let value = js::trim(value);
    if value.is_empty() {
        return Vec::new();
    }
    search_mails(&user.id, value)
        .await
        .iter()
        .map(|result| to_search_header_data(&result.row, &result.highlight, &user.username))
        .collect()
}
