//! One mail's body for the viewer (mails/body.ts).

use serde_json::{Map, Value};

use super::text_to_html::body_html;
use crate::postgres::repositories::mails::get_mail_by_id;
use crate::util::get_random_id;

/// The body of the user's mail `mail_id`, in the client's key order:
/// `id, html, attachments?, messageId, insight`. `None` when the user holds
/// no such mail.
/// An expunged row survives only so the delta can report it gone.
pub async fn get_mail_body(user_id: &str, mail_id: &str) -> anyhow::Result<Option<Map<String, Value>>> {
    let Some(mail) = get_mail_by_id(user_id, mail_id).await?.filter(|mail| !mail.expunged) else {
        return Ok(None);
    };
    let mut body = Map::new();
    body.insert("id".into(), Value::String(mail.mail_id));
    // A plain-text mail is stored with its text in `html`; convert it so the
    // viewer keeps its line breaks and shows `&`/`<` literally.
    body.insert("html".into(), Value::String(body_html(Some(&mail.html), Some(&mail.text))));
    if let Some(attachments @ Value::Array(_)) = mail.attachments {
        body.insert("attachments".into(), attachments);
    }
    // An empty Message-ID takes the model's random fallback.
    let message_id = if mail.message_id.is_empty() { get_random_id() } else { mail.message_id };
    body.insert("messageId".into(), Value::String(message_id));
    body.insert("insight".into(), mail.insight.unwrap_or(Value::Null));
    Ok(Some(body))
}
