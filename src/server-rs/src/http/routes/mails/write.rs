//! The write routes: mark, send, delete (http/routes/mails/post-mark.ts,
//! post-send.ts, delete.ts).

use serde_json::{Map, Value};

use super::session_user;
use crate::http::request::HttpRequest;
use crate::http::routes::route::{ApiResponse, Reply};
use crate::http::upload::MAX_ATTACHMENTS_PER_MAIL;
use crate::mails::body::get_mail_body;
use crate::mails::send::{send_mail, MailDataToSend, SendMailError, SendMailOptions};
use crate::mails::update::{delete_mail as delete, mark_read, mark_saved};
use crate::util::is_uuid;
use crate::push;

const NOT_YOUR_MAIL: &str = "Invalid request. You may not manipulate other users' email";

fn spawn_flag_change(username: &str, mail_id: &str) {
    let (usernames, ids) = (vec![username.to_string()], vec![mail_id.to_string()]);
    tokio::spawn(async move { push::notify_flag_change(&usernames, &ids).await });
}

pub async fn post_mark(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    let Some(body) = req.body_object() else {
        return Ok(ApiResponse::failed("Invalid request body.").into());
    };
    let Some(mail_id) = body.get("mail_id").and_then(Value::as_str).filter(|id| is_uuid(id)).map(str::to_string) else {
        return Ok(ApiResponse::failed("mail_id must be a valid id").into());
    };
    let read = body.get("read").cloned();
    let save = body.get("save").and_then(Value::as_bool);

    if get_mail_body(&user.id, &mail_id).await?.is_none() {
        return Ok(ApiResponse::failed(NOT_YOUR_MAIL).into());
    }

    // `false` means the row stopped matching between the check above and the
    // write; that is reported rather than answered as a success.
    if read == Some(Value::Bool(true)) {
        if !mark_read(&user.id, &mail_id).await? {
            return Ok(ApiResponse::failed("Failed to mark the mail read").into());
        }
        let badge_user = user.clone();
        tokio::spawn(async move { push::decrement_badge_count(&[badge_user]).await });
        spawn_flag_change(&user.username, &mail_id);
    }

    if let Some(save) = save {
        if !mark_saved(&user.id, &mail_id, save).await? {
            return Ok(ApiResponse::failed("Failed to update the saved flag").into());
        }
        spawn_flag_change(&user.username, &mail_id);
    }

    Ok(ApiResponse::success().into())
}

pub async fn post_send(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    let files = req.files.as_ref().map(|files| files.get("attachments").to_vec()).unwrap_or_default();
    if files.len() > MAX_ATTACHMENTS_PER_MAIL {
        return Ok(ApiResponse::failed(format!("A mail may carry at most {MAX_ATTACHMENTS_PER_MAIL} attachments")).into());
    }

    // A list body spreads into index keys, which name no field.
    let fields = req.body_object().cloned().unwrap_or_else(Map::new);
    match send_mail(&user, MailDataToSend::from_json(&fields), &files, SendMailOptions::default()).await {
        Ok(_) => Ok(ApiResponse::success().into()),
        Err(SendMailError::Validation(error)) => Ok(ApiResponse::failed(error.0).into()),
        Err(SendMailError::Sending(error)) => Ok(ApiResponse::failed(error.0).into()),
        Err(SendMailError::Internal(error)) => Err(error),
    }
}

/// Also reached without a session, as `DELETE /domain` (the one path the
/// gate lets through), where the id check refuses it.
pub async fn delete_mail(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let mail_id = req.param("id").to_string();
    if !is_uuid(&mail_id) {
        return Ok(ApiResponse::failed(NOT_YOUR_MAIL).into());
    }
    let user = session_user(req)?;
    if get_mail_body(&user.id, &mail_id).await?.is_none() {
        return Ok(ApiResponse::failed(NOT_YOUR_MAIL).into());
    }
    // Deliberately not gated on the result: `false` means a concurrent
    // delete reached the end state the caller asked for.
    delete(&user.id, &mail_id).await?;
    Ok(ApiResponse::success().into())
}
