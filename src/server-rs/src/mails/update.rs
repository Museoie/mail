//! Read, saved and delete operations on one mail (mails/update.ts).
//! Authorization is enforced at the repository layer via `user_id`.

use crate::postgres::repositories::mails;
use crate::postgres::DbResult;

pub async fn mark_read(user_id: &str, mail_id: &str) -> DbResult<bool> {
    mails::mark_mail_read(user_id, mail_id).await
}

pub async fn mark_saved(user_id: &str, mail_id: &str, save: bool) -> DbResult<bool> {
    mails::mark_mail_saved(user_id, mail_id, save).await
}

pub async fn delete_mail(user_id: &str, mail_id: &str) -> DbResult<bool> {
    mails::delete_mail(user_id, mail_id).await
}
