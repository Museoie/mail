//! Spam marks and the spam allowlist (http/routes/mails/post-spam-mark.ts,
//! get-allowlist.ts, post-allowlist.ts, delete-allowlist.ts).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Value};

use super::session_user;
use crate::http::request::HttpRequest;
use crate::http::routes::route::{ApiResponse, Reply};
use crate::logger;
use crate::mails::spam::mark_spam;
use crate::postgres::repositories::mails::get_mail_by_id;
use crate::postgres::repositories::spam_allowlists::{
    add_allowlist_entry, get_allowlist_for_user, remove_allowlist_entry, AddAllowlistEntryResult, SpamAllowlistModel,
    ALLOWLIST_COUNT_MAX, ALLOWLIST_PATTERN_MAX_BYTES,
};
use crate::read_only::refuse_read_only;
use crate::spam::classifier::train_with_email;
use crate::spam::types::EmailContext;
use crate::util::is_uuid;

const NOT_FOUND: &str = "Mail not found or you don't have permission";

/// JavaScript's `[^\s@]`: `\s` is its fixed whitespace list, not Unicode's.
const NOT_SPACE_OR_AT: &str = r"[^\t\n\x0B\x0C\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}@]";

static EXACT_EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^{NOT_SPACE_OR_AT}+@{NOT_SPACE_OR_AT}+\\.{NOT_SPACE_OR_AT}+$")).expect("a valid pattern")
});
static DOMAIN_WILDCARD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^\\*@{NOT_SPACE_OR_AT}+\\.{NOT_SPACE_OR_AT}+$")).expect("a valid pattern")
});

/// An exact address (`user@example.com`) or a domain wildcard (`*@example.com`).
pub fn is_allowlist_pattern(pattern: &str) -> bool {
    EXACT_EMAIL.is_match(pattern) || DOMAIN_WILDCARD.is_match(pattern)
}

fn entry_json(entry: &SpamAllowlistModel) -> Value {
    json!({ "id": entry.allowlist_id, "pattern": entry.pattern, "createdAt": entry.created_at })
}

/// Marks or unmarks a mail as spam, and trains the user's classifier on it
/// when the flag actually flipped (one sample per change, not per call).
pub async fn post_spam_mark(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    if let Some(message) = refuse_read_only(Some(&user), "Marking spam") {
        return Ok(ApiResponse::failed(message).into());
    }
    let Some(is_spam) = req.body.get("is_spam").and_then(Value::as_bool) else {
        return Ok(ApiResponse::failed("is_spam must be a boolean").into());
    };
    let Some(mail_id) = req.body.get("mail_id").and_then(Value::as_str).filter(|id| is_uuid(id)).map(str::to_string) else {
        return Ok(ApiResponse::failed(NOT_FOUND).into());
    };

    let result = mark_spam(&user.id, &mail_id, is_spam).await?;
    if !result.found {
        return Ok(ApiResponse::failed(NOT_FOUND).into());
    }

    if result.changed {
        let user_id = user.id.clone();
        tokio::spawn(async move {
            let trained = async {
                let Some(mail) = get_mail_by_id(&user_id, &mail_id).await? else {
                    return Ok(());
                };
                let from_address = match &mail.from_address {
                    Some(Value::Array(entries)) => entries
                        .first()
                        .and_then(|entry| entry.get("address"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                };
                let context = EmailContext {
                    subject: Some(mail.subject),
                    text: Some(mail.text),
                    html: Some(mail.html),
                    from_address,
                    ..Default::default()
                };
                train_with_email(&user_id, &context, is_spam).await
            };
            if let Err(error) = trained.await {
                logger::warn_with(
                    "[SpamFilter] Classifier training failed for feedback",
                    json!({ "mail_id": mail_id }),
                    &error,
                );
            }
        });
    }

    Ok(ApiResponse::success().into())
}

pub async fn get_allowlist(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    let entries = get_allowlist_for_user(&user.id).await?;
    Ok(ApiResponse::success_with(entries.iter().map(entry_json).collect::<Vec<_>>()).into())
}

pub async fn post_allowlist(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    if let Some(message) = refuse_read_only(Some(&user), "Adding to the spam allowlist") {
        return Ok(ApiResponse::failed(message).into());
    }
    let Some(pattern) = req.body.get("pattern").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_string)
    else {
        return Ok(ApiResponse::failed("pattern is required").into());
    };
    if !is_allowlist_pattern(&pattern) {
        return Ok(ApiResponse::failed(
            "Pattern must be an email address (user@example.com) or domain wildcard (*@example.com)",
        )
        .into());
    }

    Ok(match add_allowlist_entry(&user.id, &pattern).await? {
        AddAllowlistEntryResult::Exists => ApiResponse::failed("Entry already exists"),
        AddAllowlistEntryResult::TooLong => {
            ApiResponse::failed(format!("Pattern must be {ALLOWLIST_PATTERN_MAX_BYTES} bytes or fewer"))
        }
        AddAllowlistEntryResult::AtLimit => {
            ApiResponse::failed(format!("Allowlist limit of {ALLOWLIST_COUNT_MAX} entries reached"))
        }
        AddAllowlistEntryResult::Created(entry) => ApiResponse::success_with(entry_json(&entry)),
    }
    .into())
}

pub async fn delete_allowlist(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    if let Some(message) = refuse_read_only(Some(&user), "Removing from the spam allowlist") {
        return Ok(ApiResponse::failed(message).into());
    }
    let pattern = req.param("pattern").to_string();
    Ok(if remove_allowlist_entry(&user.id, &pattern).await? {
        ApiResponse::success()
    } else {
        ApiResponse::failed("Entry not found")
    }
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_patterns_are_addresses_or_domain_wildcards() {
        assert!(is_allowlist_pattern("user@example.com"));
        assert!(is_allowlist_pattern("*@example.com"));
        assert!(is_allowlist_pattern("*@sub.example.co.uk"));
        assert!(is_allowlist_pattern("ü@bü.co"));
        // A no-break space is JavaScript whitespace; NEL is not.
        assert!(!is_allowlist_pattern("a\u{a0}b@c.de"));
        assert!(is_allowlist_pattern("a\u{85}b@c.de"));
        for pattern in ["user", "user@example", "@example.com", "a b@c.de", "a@b@c.de", "*@example", "user@.com x", "*@*.com x"] {
            assert!(!is_allowlist_pattern(pattern), "{pattern:?}");
        }
    }
}
