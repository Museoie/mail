//! `GET /headers/:account` and `GET /unified`: the mail lists, whole, paged
//! or as a delta (http/routes/mails/get-headers.ts, get-unified-headers.ts).

use super::list_page::{encode_list_cursor, parse_list_page_params, ListPageParams};
use super::reader::resolve_mail_reader;
use super::session_user;
use crate::domains::{own_address_domains, AddressOnDomains};
use crate::http::request::{HttpRequest, QueryValue};
use crate::http::respond::{JsonList, ListPage};
use crate::http::routes::route::{ApiResponse, Reply};
use crate::js;
use crate::mails::accounts::get_accounts;
use crate::mails::headers::{
    for_each_mail_header, get_mail_header_ids, get_mail_header_page, get_mail_headers_delta,
    get_unified_mail_header_ids, get_unified_mail_header_page, get_unified_mail_headers_delta, GetMailsOptions,
};

const INVALID_SINCE: &str = "Invalid `since` parameter: expected an ISO timestamp.";

/// `?since=` when present: `Ok(Some(since))`, or the refusal for one that is
/// not a single parseable time. A malformed cursor must neither fall back to
/// a full fetch nor reach the SQL.
fn since_of(req: &HttpRequest) -> Result<Option<String>, ApiResponse> {
    match req.query.get("since") {
        None => Ok(None),
        Some(QueryValue::One(since)) if js::date_parse(since).is_some() => Ok(Some(since.clone())),
        Some(_) => Err(ApiResponse::failed(INVALID_SINCE)),
    }
}

/// Streams the header rows of `ids` in `store_id`'s rows as the list body,
/// with the page's `next_cursor` first when `page` is given.
async fn list_of(
    store_id: &str,
    readers: &AddressOnDomains,
    ids: &[String],
    page: Option<ListPage>,
) -> anyhow::Result<Reply> {
    let mut list = JsonList::new(page);
    for_each_mail_header(store_id, ids, readers, |header| list.push(&header)).await?;
    Ok(list.finish().into())
}

fn page_fields(next: Option<&crate::postgres::repositories::mails::http::ListCursor>) -> ListPage {
    ListPage { list_key: "headers".into(), fields: vec![("next_cursor".into(), encode_list_cursor(next))] }
}

pub async fn get_headers(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    let account = req.param("account").to_string();
    // The account's domain decides whose rows are read: the user's own for a
    // domain they own, its owner's (filtered to it) for one they view.
    let Some(reader) = resolve_mail_reader(&user, &account).await? else {
        return Ok(ApiResponse::failed("Invalid request. You may not look at other users' emails.").into());
    };

    let options = GetMailsOptions {
        sent: req.query.truthy("sent"),
        saved: req.query.truthy("saved"),
        spam: req.query.truthy("spam"),
        domains: reader.domains.clone(),
    };

    match since_of(req) {
        Err(refusal) => return Ok(refusal.into()),
        Ok(Some(since)) => {
            let delta = get_mail_headers_delta(&reader.store_id, &account, options, &since, &reader.readers).await;
            return Ok(ApiResponse::success_with(delta.to_json()).into());
        }
        Ok(None) => {}
    }

    match parse_list_page_params(&req.query) {
        ListPageParams::Invalid(message) => Ok(ApiResponse::failed(message).into()),
        ListPageParams::Paged { limit, after } => {
            let page = get_mail_header_page(&reader.store_id, &account, options, limit, after).await?;
            list_of(&reader.store_id, &reader.readers, &page.ids, Some(page_fields(page.next.as_ref()))).await
        }
        ListPageParams::Unpaged => {
            let ids = get_mail_header_ids(&reader.store_id, &account, options).await?;
            list_of(&reader.store_id, &reader.readers, &ids, None).await
        }
    }
}

/// The "All accounts" list: received mail across every address the
/// accounts payload lists as received, unread first (newest first when paged).
pub async fn get_unified_headers(req: &mut HttpRequest) -> anyhow::Result<Reply> {
    let user = session_user(req)?;
    let (accounts, own) = tokio::join!(get_accounts(&user), own_address_domains(&user.id));
    let addresses: Vec<String> = accounts?.received.into_iter().map(|account| account.key).collect();
    let is_own = AddressOnDomains::new(&own?);

    match since_of(req) {
        Err(refusal) => return Ok(refusal.into()),
        Ok(Some(since)) => {
            let delta = get_unified_mail_headers_delta(&user, &addresses, &since, &is_own).await;
            return Ok(ApiResponse::success_with(delta.to_json()).into());
        }
        Ok(None) => {}
    }

    match parse_list_page_params(&req.query) {
        ListPageParams::Invalid(message) => Ok(ApiResponse::failed(message).into()),
        ListPageParams::Paged { limit, after } => {
            let page = get_unified_mail_header_page(&user, &addresses, limit, after).await?;
            list_of(&user.id, &is_own, &page.ids, Some(page_fields(page.next.as_ref()))).await
        }
        ListPageParams::Unpaged => {
            let ids = get_unified_mail_header_ids(&user, &addresses).await?;
            list_of(&user.id, &is_own, &ids, None).await
        }
    }
}
