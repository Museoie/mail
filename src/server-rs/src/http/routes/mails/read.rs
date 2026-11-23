//! The read routes: domain, accounts, one mail's body, its conversation, an
//! attachment, and search (http/routes/mails/get-*.ts).

use std::path::PathBuf;

use serde_json::{json, Value};

use super::session_user;
use crate::http::request::HttpRequest;
use crate::http::respond::send_file;
use crate::http::routes::route::{ApiResponse, Reply, AUTH_ERROR_MESSAGE};
use crate::http::static_files::stat_file;
use crate::mails::accounts::{get_accounts as accounts_of, search_accounts};
use crate::mails::body::get_mail_body;
use crate::mails::search::search_mail;
use crate::mails::thread::get_thread as thread_of;
use crate::mails::update::mark_read;
use crate::mails::util::get_attachment_file_path;
use crate::postgres::repositories::mails::mails_table;
use crate::postgres::repositories::mails::table::SelectFilter;
use crate::read_only::refuse_read_only;
use crate::util::{get_domain as domain, is_uuid};
use crate::{logger, push};

const NO_EMAIL: &str = "No email is found.";

pub async fn get_domain(_req: &mut HttpRequest) -> anyhow::Result<Reply> {
    Ok(ApiResponse::success_with(domain()).into())
}

pub async fn get_accounts(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    Ok(ApiResponse::success_with(accounts_of(&user).await).into())
}

pub async fn get_body(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    let id = req.param("id").to_string();
    if !is_uuid(&id) {
        return Ok(ApiResponse::failed(NO_EMAIL).into());
    }
    let Some(mail) = get_mail_body(&user.id, &id).await? else {
        return Ok(ApiResponse::failed(NO_EMAIL).into());
    };

    // Reading a mail over HTTP marks it read, the HTTP equivalent of IMAP's
    // implicit \Seen. Best-effort: a failed mark must not fail the read.
    // Read-only sessions read without marking.
    if refuse_read_only(Some(&user), "Marking mail").is_none() {
        match mark_read(&user.id, &id).await {
            Ok(true) => {
                let (usernames, ids) = (vec![user.username.clone()], vec![id.clone()]);
                tokio::spawn(async move { push::notify_flag_change(&usernames, &ids).await });
            }
            Ok(false) => {}
            Err(error) => {
                logger::error_with("Failed to mark mail read on HTTP body fetch", json!({ "mail_id": id }), &error);
            }
        }
    }
    Ok(ApiResponse::success_with(Value::Object(mail)).into())
}

pub async fn get_thread(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    let id = req.param("id").to_string();
    if !is_uuid(&id) {
        return Ok(ApiResponse::failed(NO_EMAIL).into());
    }
    let thread = thread_of(&user, &id).await?;
    if thread.is_empty() {
        return Ok(ApiResponse::failed(NO_EMAIL).into());
    }
    Ok(ApiResponse::success_with(thread).into())
}

pub async fn get_attachment(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let Some(user) = req.session.user.clone() else {
        return Ok(ApiResponse::failed(AUTH_ERROR_MESSAGE).into());
    };
    let attachment_id = req.param("id").to_string();

    // One query: the attachment is on one of this user's mails. An expunged
    // mail is gone for every by-id route, its files with it; a copy of it that
    // is still live (an IMAP MOVE) names the same id and serves it.
    let filters = [
        ("user_id", SelectFilter::from(user.id.as_str())),
        ("attachments", SelectFilter::Contains(json!([{ "content": { "data": attachment_id } }]))),
        ("expunged", SelectFilter::from(false)),
    ];
    if mails_table().query_one(&filters, None).await?.is_none() {
        return Ok(ApiResponse::failed("Not found").into());
    }

    let path = PathBuf::from(get_attachment_file_path(Some(&attachment_id)));
    let Some((size, modified_ms)) = stat_file(&path).await else {
        return Ok(ApiResponse::failed_silently().into());
    };
    Ok(send_file(path, size, modified_ms, "application/octet-stream").into())
}

pub async fn get_search(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    // `?field=` is accepted and ignored.
    let value = req.param("value").to_string();
    Ok(ApiResponse::success_with(search_mail(&user, &value).await).into())
}

pub async fn get_search_accounts(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    let value = req.param("value").to_string();
    Ok(ApiResponse::success_with(search_accounts(&user, &value).await).into())
}
