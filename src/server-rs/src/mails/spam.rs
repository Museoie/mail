//! Spam marks on one mail (mails/spam.ts).

use crate::postgres::repositories::mails::{self, MarkMailSpamResult};
use crate::postgres::DbResult;

/// Marks or unmarks a mail as spam. `found` is false when the user holds no
/// such mail; `changed` when the flag flipped.
pub async fn mark_spam(user_id: &str, mail_id: &str, is_spam: bool) -> DbResult<MarkMailSpamResult> {
    mails::mark_mail_spam(user_id, mail_id, is_spam).await
}
