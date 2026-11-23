//! Storing mail: an incoming message into each local recipient's mailbox,
//! and the sender's copy of a sent one (mails/receive.ts).
//!
//! The SMTP server builds an [`IncomingMail`] from the parsed message with
//! [`incoming_mail_from_parsed`], adds the envelope, and hands it to
//! [`save_mail_handler`]:
//!
//! ```ignore
//! let mut mail = incoming_mail_from_parsed(&parsed);
//! mail.envelope_from = mail_from.map(|address| vec![IncomingMailAddressValue::address(address)]);
//! mail.envelope_to = Some(rcpt_to.iter().map(|a| IncomingMailAddressValue::address(a)).collect());
//! let stored = save_mail_handler(&mail, &SaveMailHandlerOptions { remote_address }).await?;
//! if stored == 0 { /* refuse: no mailbox took it */ }
//! ```

use std::path::Path;

use serde::Serialize;
use serde_json::{json, Map, Value};

use super::util::{get_attachment_file_path, get_attachment_id, get_text, ATTACHMENT_FOLDER};
use crate::alarm::spawn_alarm;
use crate::imap::util::{account_to_box, account_to_sent_box};
use crate::mime::address::{OneOrMany, ParsedAddress, ParsedAddressValue};
use crate::mime::parser::ParsedMessage;
use crate::models::User;
use crate::postgres::repositories::mails::{
    get_account_uid_next, get_domain_uid_next, save_mail as repository_save_mail, SaveMailInput, SavedMail,
    INBOX_VIEW,
};
use crate::spam::service::check_spam;
use crate::spam::types::{EmailContext, SpamCheckResult};
use crate::users::{get_user, UserQuery};
use crate::domains::{resolve_delivery_domain, DeliveryDomain};
use crate::util::get_random_id;
use crate::{js, logger, push};

/// One mailbox of an incoming address header or envelope, or a group with
/// its members.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IncomingMailAddressValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<Vec<IncomingMailAddressValue>>,
}

impl IncomingMailAddressValue {
    /// An envelope address: an address with no name.
    pub fn address(address: impl Into<String>) -> IncomingMailAddressValue {
        IncomingMailAddressValue { address: Some(address.into()), name: None, group: None }
    }
}

impl From<&ParsedAddressValue> for IncomingMailAddressValue {
    fn from(value: &ParsedAddressValue) -> IncomingMailAddressValue {
        IncomingMailAddressValue {
            address: value.address.clone(),
            name: Some(value.name.clone()),
            group: value.group.as_ref().map(|members| members.iter().map(IncomingMailAddressValue::from).collect()),
        }
    }
}

/// One address header as parsed: its entries and its text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IncomingMailAddress {
    pub value: Vec<IncomingMailAddressValue>,
    pub text: String,
}

impl From<&ParsedAddress> for IncomingMailAddress {
    fn from(address: &ParsedAddress) -> IncomingMailAddress {
        IncomingMailAddress {
            value: address.value.iter().map(IncomingMailAddressValue::from).collect(),
            text: address.text.clone(),
        }
    }
}

/// A file holding an attachment's decoded content. It belongs to the caller,
/// so saving links or copies it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingAttachment {
    pub path: String,
    pub content_type: String,
    pub filename: String,
    pub size: u64,
}

/// A message on its way into storage. An address header that repeats holds
/// one entry per occurrence; the envelope is the receiving protocol's to add.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingMail {
    pub attachments: Option<Vec<IncomingAttachment>>,
    pub from: Option<Vec<IncomingMailAddress>>,
    pub to: Option<Vec<IncomingMailAddress>>,
    pub cc: Option<Vec<IncomingMailAddress>>,
    pub bcc: Option<Vec<IncomingMailAddress>>,
    pub reply_to: Option<Vec<IncomingMailAddress>>,
    pub envelope_from: Option<Vec<IncomingMailAddressValue>>,
    pub envelope_to: Option<Vec<IncomingMailAddressValue>>,
    /// ISO time; the time of the save when absent.
    pub date: Option<String>,
    pub html: Option<String>,
    pub text: Option<String>,
    pub subject: Option<String>,
    pub message_id: Option<String>,
    /// The Message-ID this mail answers.
    pub in_reply_to: Option<String>,
    /// The conversation's Message-IDs, oldest first.
    pub references: Option<Vec<String>>,
}

/// One stored address. `name` is any JSON value: a received mail's is the
/// parsed display name, a sent mail's whatever the client sent.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MailAddressValue {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<Value>,
}

impl MailAddressValue {
    pub fn new(address: impl Into<String>, name: Option<Value>) -> MailAddressValue {
        MailAddressValue { address: address.into(), name }
    }

    /// The `{address, name?}` object stored in the address columns.
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("address".into(), Value::String(self.address.clone()));
        if let Some(name) = &self.name {
            object.insert("name".into(), name.clone());
        }
        Value::Object(object)
    }
}

/// A stored address header: its entries and its text.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MailAddress {
    pub value: Vec<MailAddressValue>,
    pub text: String,
}

fn values_json(values: &[MailAddressValue]) -> Value {
    Value::Array(values.iter().map(MailAddressValue::to_json).collect())
}

/// The stored reference to an attachment file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    /// `{ data: <attachment id> }`, the file at `./attachments/<id>`.
    pub content: AttachmentContent,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AttachmentContent {
    pub data: String,
}

impl Attachment {
    /// As the shared model fills a missing field.
    pub fn new(filename: &str, content_type: &str, id: String, size: u64) -> Attachment {
        Attachment {
            filename: if filename.is_empty() { "unnamed_file".into() } else { filename.into() },
            content_type: if content_type.is_empty() { "text/plain".into() } else { content_type.into() },
            content: AttachmentContent { data: id },
            size,
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// The UIDs reserved for a mail: in the domain-wide and the per-account sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MailUid {
    pub domain: i64,
    pub account: i64,
}

/// A mail ready to be stored.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mail {
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<String>>,
    pub attachments: Vec<Attachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<MailAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<MailAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc: Option<MailAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bcc: Option<MailAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MailAddress>,
    pub envelope_to: Vec<MailAddressValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope_from: Option<Vec<MailAddressValue>>,
    pub text: String,
    pub date: String,
    pub html: String,
    pub subject: String,
    pub read: bool,
    pub saved: bool,
    pub sent: bool,
    pub uid: MailUid,
    pub deleted: bool,
    pub draft: bool,
    pub answered: bool,
}

/// Options of [`save_mail_handler`].
#[derive(Debug, Clone, Default)]
pub struct SaveMailHandlerOptions {
    /// The client's address, for the blocklist checks of the spam filter.
    pub remote_address: Option<String>,
}

/// Stores an incoming message with the owner of every domain its envelope
/// recipients are delivered into, once per owner, and answers how many it
/// wrote. A recipient need not be delivered anywhere (a foreign address, or a
/// name under the served zone with no account), and one that is not is
/// skipped rather than failing the call; zero tells the caller the message
/// landed nowhere.
pub async fn save_mail_handler(data: &IncomingMail, options: &SaveMailHandlerOptions) -> anyhow::Result<usize> {
    let envelope_to = data.envelope_to.as_deref().and_then(convert_address_value).map(|values| values_json(&values));
    let from = data.from.as_deref().and_then(convert_mail_address).map(|address| values_json(&address.value));
    logger::info(
        "Received an email",
        json!({
            "timestamp": js::now_iso(),
            "envelopeTo": envelope_to.map(|value| value.to_string()),
            "from": from.map(|value| value.to_string()),
        }),
    );

    let deliveries = resolve_deliveries(data).await?;
    if deliveries.is_empty() {
        logger::warn("Recipient is not valid. Mails is not saved.", json!({}));
        return Ok(0);
    }

    let saves = deliveries.iter().map(|delivery| save_incoming_mail(&delivery.owner_id, data, options));
    let saved = futures_util::future::join_all(saves).await;
    let mut usernames = Vec::new();
    for result in saved {
        if let Some(username) = result? {
            usernames.push(username);
        }
    }
    let stored_count = usernames.len();
    if stored_count == 0 {
        logger::warn(
            "No recipient resolved to a mailbox. Mail is not saved.",
            json!({ "envelopeTo": data.envelope_to.as_deref().and_then(convert_address_value).map(|values| values_json(&values).to_string()) }),
        );
        return Ok(0);
    }
    logger::info("Successfully saved an email", json!({}));

    let mailboxes: Vec<String> =
        deliveries.iter().flat_map(|delivery| delivery.addresses.iter().map(|address| account_to_box(address))).collect();
    push::notify_new_mails(&usernames, Some(&mailboxes)).await;
    logger::info(format!("Sent push notifications to users: [{}]", usernames.join(",")), json!({}));
    Ok(stored_count)
}

/// Stores the message with `owner_id`, answering the owner's username when
/// it was stored.
async fn save_incoming_mail(
    owner_id: &str,
    incoming: &IncomingMail,
    options: &SaveMailHandlerOptions,
) -> anyhow::Result<Option<String>> {
    let user = get_user(&UserQuery { id: Some(owner_id.to_string()), ..Default::default() }).await?;
    let Some((user, username)) =
        user.and_then(|user| user.username.clone().filter(|name| !name.is_empty()).map(|name| (user, name)))
    else {
        logger::warn(format!("User not found for id: {owner_id}"), json!({}));
        logger::warn("Skipping saving mail", json!({ "incoming": incoming }));
        return Ok(None);
    };
    let mail = convert_mail(&user, incoming).await?;

    let spam_result = match user.id.as_deref().filter(|id| !id.is_empty()) {
        Some(user_id) => {
            let context = EmailContext {
                from_address: first_address(mail.from.as_ref()),
                envelope_from_address: mail.envelope_from.as_ref().and_then(|values| values.first()).map(|v| v.address.clone()),
                from_name: mail.from.as_ref().map(|from| from.text.clone()),
                reply_to_address: first_address(mail.reply_to.as_ref()),
                subject: incoming.subject.clone(),
                text: incoming.text.clone(),
                html: incoming.html.clone(),
                remote_address: options.remote_address.clone(),
                ..Default::default()
            };
            let result = check_spam(user_id, &context).await;
            if result.is_spam {
                logger::info(
                    format!("[SpamFilter] Email marked as spam for user {username}"),
                    json!({ "score": result.score, "reasons": result.reasons }),
                );
            }
            Some(result)
        }
        None => None,
    };

    let saved = save_mail(&mail, user.id.as_deref(), spam_result.as_ref()).await?;
    Ok(saved.map(|_| username))
}

fn first_address(address: Option<&MailAddress>) -> Option<String> {
    address.and_then(|address| address.value.first()).map(|value| value.address.clone())
}

/// Stores `mail` for `user_id`: a received mail under its first envelope
/// recipient's account, a sent one under its sender's Sent account. `None`
/// without a user. A failure is alarmed, dumped to `./error/<ms>` and
/// returned.
pub async fn save_mail(
    mail: &Mail,
    user_id: Option<&str>,
    spam_result: Option<&SpamCheckResult>,
) -> anyhow::Result<Option<SavedMail>> {
    let Some(user_id) = user_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let scope_address = if mail.sent {
        mail.envelope_from.as_ref().and_then(|values| values.first()).map(|value| value.address.as_str())
    } else {
        mail.envelope_to.first().map(|value| value.address.as_str())
    };
    let mailbox = scope_address
        .filter(|address| !address.is_empty())
        .map(|address| if mail.sent { account_to_sent_box(address) } else { account_to_box(address) });

    let address_value = |address: &Option<MailAddress>| address.as_ref().map(|a| values_json(&a.value));
    let address_text = |address: &Option<MailAddress>| address.as_ref().map(|a| a.text.clone());
    let input = SaveMailInput {
        user_id: user_id.to_string(),
        message_id: mail.message_id.clone(),
        in_reply_to: mail.in_reply_to.clone(),
        reference_ids: mail.references.clone().filter(|ids| !ids.is_empty()),
        subject: Some(mail.subject.clone()),
        date: Some(mail.date.clone()),
        html: Some(mail.html.clone()),
        text: Some(mail.text.clone()),
        from_address: address_value(&mail.from),
        from_text: address_text(&mail.from),
        to_address: address_value(&mail.to),
        to_text: address_text(&mail.to),
        cc_address: address_value(&mail.cc),
        cc_text: address_text(&mail.cc),
        bcc_address: address_value(&mail.bcc),
        bcc_text: address_text(&mail.bcc),
        reply_to_address: address_value(&mail.reply_to),
        reply_to_text: address_text(&mail.reply_to),
        envelope_from: mail.envelope_from.as_deref().map(values_json),
        envelope_to: Some(values_json(&mail.envelope_to)),
        attachments: Some(Value::Array(mail.attachments.iter().map(Attachment::to_json).collect())),
        read: mail.read,
        saved: mail.saved,
        sent: mail.sent,
        deleted: mail.deleted,
        draft: mail.draft,
        uid_domain: Some(mail.uid.domain),
        uid_mailbox: Some(mail.uid.account),
        mailbox,
        spam_score: Some(spam_result.map(|result| result.score as f64).unwrap_or(0.0)),
        spam_reasons: spam_result.map(|result| result.reasons.clone()),
        is_spam: spam_result.is_some_and(|result| result.is_spam),
        // Received mail is in the INBOX tree; whether the written row may
        // hold that membership is the repository's question.
        domain_mailbox: Some(INBOX_VIEW.to_string()),
        ..Default::default()
    };

    match repository_save_mail(&input).await {
        Ok(saved) => Ok(saved),
        Err(error) => {
            logger::error_with("Error saving mail", json!({}), &error);
            let title = if mail.sent { "Mail Send Save Failed" } else { "Mail Receive Failed" };
            spawn_alarm(title, format!("**Error:** {error}"));
            if let Err(dump_error) = dump_failed_mail(mail, &error) {
                logger::warn_with("Failed to persist mail error dump", json!({}), &dump_error);
            }
            Err(error)
        }
    }
}

/// Writes the mail that failed to save to `./error/<ms>`, for a later look.
fn dump_failed_mail(mail: &Mail, error: &anyhow::Error) -> std::io::Result<()> {
    let mut content = match serde_json::to_value(mail) {
        Ok(Value::Object(fields)) => fields,
        _ => Map::new(),
    };
    let mut detail = Map::new();
    if let Some(db) = error.downcast_ref::<crate::postgres::DbError>() {
        detail.insert("name".into(), json!("error"));
        if let Some(code) = &db.code {
            detail.insert("code".into(), json!(code));
        }
        if let Some(constraint) = &db.constraint {
            detail.insert("constraint".into(), json!(constraint));
        }
    }
    content.insert("error".into(), Value::Object(detail));
    std::fs::create_dir_all("./error")?;
    std::fs::write(format!("./error/{}", js::now_ms()), Value::Object(content).to_string())
}

/// The mail to store for `user` from an incoming message: addresses
/// lower-cased, attachments saved, the next domain and account UIDs reserved.
pub async fn convert_mail(user: &User, incoming: &IncomingMail) -> anyhow::Result<Mail> {
    let from = incoming.from.as_deref().and_then(convert_mail_address);
    let to = incoming.to.as_deref().and_then(convert_mail_address);
    let cc = incoming.cc.as_deref().and_then(convert_mail_address);
    let bcc = incoming.bcc.as_deref().and_then(convert_mail_address);
    let reply_to = incoming.reply_to.as_deref().and_then(convert_mail_address);

    let envelope_from = incoming.envelope_from.as_deref().and_then(convert_address_value);
    let envelope_to = incoming.envelope_to.as_deref().and_then(convert_address_value).unwrap_or_default();

    let attachments = match &incoming.attachments {
        Some(attachments) => convert_attachments(attachments).await?,
        None => Vec::new(),
    };

    let subject = incoming.subject.clone().unwrap_or_default();
    let date = incoming.date.clone().filter(|date| !date.is_empty()).unwrap_or_else(js::now_iso);
    let html = incoming.html.clone().unwrap_or_default();
    let text = match &incoming.text {
        Some(text) => text.clone(),
        None => get_text(&html),
    };
    let message_id = incoming.message_id.clone().filter(|id| !id.is_empty()).unwrap_or_else(get_random_id);
    let envelope_to_address = envelope_to.first().map(|value| value.address.clone()).unwrap_or_default();

    let Some(user_id) = user.id.as_deref().filter(|id| !id.is_empty()) else {
        anyhow::bail!("User ID is required to save mail");
    };
    let (domain_uid, account_uid) =
        tokio::join!(get_domain_uid_next(user_id, false), get_account_uid_next(user_id, &envelope_to_address, false));

    Ok(Mail {
        message_id,
        in_reply_to: incoming.in_reply_to.clone(),
        references: incoming.references.clone(),
        attachments,
        to,
        from,
        cc,
        bcc,
        reply_to,
        envelope_to,
        envelope_from,
        text,
        date,
        html,
        subject,
        uid: MailUid { domain: domain_uid?, account: account_uid? },
        ..Default::default()
    })
}

/// The fields of an incoming mail a parsed message carries on its own. The
/// envelope is the receiving protocol's to add. A message with no HTML part
/// carries its text as the HTML.
pub fn incoming_mail_from_parsed(parsed: &ParsedMessage) -> IncomingMail {
    let one = |address: &Option<ParsedAddress>| address.as_ref().map(|a| vec![IncomingMailAddress::from(a)]);
    let many = |addresses: &Option<OneOrMany<ParsedAddress>>| {
        addresses.as_ref().map(|list| list.iter().map(IncomingMailAddress::from).collect::<Vec<_>>())
    };
    let html = parsed.html.clone().filter(|html| !html.is_empty()).or_else(|| parsed.text.clone());
    IncomingMail {
        message_id: parsed.message_id.clone(),
        in_reply_to: parsed.in_reply_to.clone(),
        references: parsed.references.clone(),
        from: one(&parsed.from),
        to: many(&parsed.to),
        cc: many(&parsed.cc),
        bcc: many(&parsed.bcc),
        reply_to: one(&parsed.reply_to),
        subject: parsed.subject.clone(),
        date: parsed.date.map(|date| date.to_iso_string()),
        html,
        text: parsed.text.clone(),
        attachments: Some(
            parsed
                .attachments
                .iter()
                .map(|attachment| IncomingAttachment {
                    filename: attachment.filename.clone().filter(|name| !name.is_empty()).unwrap_or_else(|| "attachment".into()),
                    content_type: attachment.content_type.clone(),
                    path: attachment.path.to_string_lossy().into_owned(),
                    size: attachment.size,
                })
                .collect(),
        ),
        envelope_from: None,
        envelope_to: None,
    }
}

/// An address header as stored: its addresses lower-cased, and the text of
/// every occurrence joined by `", "`. `None` when no address survives.
pub fn convert_mail_address(incoming: &[IncomingMailAddress]) -> Option<MailAddress> {
    if incoming.is_empty() {
        return None;
    }
    let entries: Vec<IncomingMailAddressValue> = incoming.iter().flat_map(|address| address.value.clone()).collect();
    let value = convert_address_value(&entries)?;
    let text = incoming.iter().map(|address| address.text.as_str()).collect::<Vec<_>>().join(", ");
    Some(MailAddress { value, text })
}

/// The addresses of a list, lower-cased and trimmed: group members in place
/// of their group, an address holding commas split, empty ones dropped.
/// `None` when nothing is left.
pub fn convert_address_value(incoming: &[IncomingMailAddressValue]) -> Option<Vec<MailAddressValue>> {
    fn push(entry: &IncomingMailAddressValue, out: &mut Vec<MailAddressValue>) {
        if let Some(members) = &entry.group {
            for member in members {
                push(member, out);
            }
            return;
        }
        let address = entry.address.as_deref().map(|address| js::trim(&address.to_lowercase()).to_string());
        let Some(address) = address.filter(|address| !address.is_empty()) else {
            return;
        };
        let name = entry.name.clone().map(Value::String);
        if address.contains(',') {
            for part in address.split(',').map(js::trim).filter(|part| !part.is_empty()) {
                out.push(MailAddressValue::new(part, name.clone()));
            }
            return;
        }
        out.push(MailAddressValue::new(address, name));
    }
    let mut out = Vec::new();
    for entry in incoming {
        push(entry, &mut out);
    }
    (!out.is_empty()).then_some(out)
}

/// Stores each attachment's file and answers what the mail records.
pub async fn convert_attachments(incoming: &[IncomingAttachment]) -> anyhow::Result<Vec<Attachment>> {
    let mut attachments = Vec::with_capacity(incoming.len());
    for attachment in incoming {
        let id = save_file(Path::new(&attachment.path)).await?;
        attachments.push(Attachment::new(&attachment.filename, &attachment.content_type, id, attachment.size));
    }
    Ok(attachments)
}

/// Stores the file at `source_path` as a new attachment and answers its id.
/// The source is left in place: hard-linked where the filesystem allows,
/// copied otherwise.
pub async fn save_file(source_path: &Path) -> anyhow::Result<String> {
    let id = get_attachment_id();
    tokio::fs::create_dir_all(ATTACHMENT_FOLDER).await?;
    let target = get_attachment_file_path(Some(&id));
    if tokio::fs::hard_link(source_path, &target).await.is_err() {
        tokio::fs::copy(source_path, &target).await?;
    }
    Ok(id)
}

/// Stores `bytes` as a new attachment and answers its id.
pub async fn save_buffer(bytes: &[u8]) -> anyhow::Result<String> {
    let id = get_attachment_id();
    tokio::fs::create_dir_all(ATTACHMENT_FOLDER).await?;
    tokio::fs::write(get_attachment_file_path(Some(&id)), bytes).await?;
    Ok(id)
}

/// An owner an incoming message is stored with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IncomingDelivery {
    owner_id: String,
    /// The envelope recipients delivered into this owner's domains.
    addresses: Vec<String>,
}

/// The envelope recipients, trimmed and lower-cased, each once.
fn envelope_addresses(data: &IncomingMail) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    data.envelope_to
        .iter()
        .flatten()
        .filter_map(|entry| entry.address.as_deref())
        .map(|address| js::trim(address).to_lowercase())
        .filter(|address| !address.is_empty() && seen.insert(address.clone()))
        .collect()
}

/// The owners an incoming message is stored with, one entry per owner however
/// many of their domains or addresses it names, so a message to two of one
/// person's domains is stored once.
async fn resolve_deliveries(data: &IncomingMail) -> anyhow::Result<Vec<IncomingDelivery>> {
    let addresses = envelope_addresses(data);
    let resolved = futures_util::future::join_all(addresses.iter().map(|address| resolve_delivery_domain(address))).await;
    let mut deliveries: Vec<IncomingDelivery> = Vec::new();
    for (address, delivery) in addresses.into_iter().zip(resolved) {
        let Some(DeliveryDomain { owner_id, .. }) = delivery? else { continue };
        match deliveries.iter_mut().find(|existing| existing.owner_id == owner_id) {
            Some(existing) => existing.addresses.push(address),
            None => deliveries.push(IncomingDelivery { owner_id, addresses: vec![address] }),
        }
    }
    Ok(deliveries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(address: &str, name: &str) -> IncomingMailAddressValue {
        IncomingMailAddressValue { address: Some(address.into()), name: Some(name.into()), group: None }
    }

    #[test]
    fn addresses_are_lower_cased_trimmed_and_split_on_commas() {
        let converted = convert_address_value(&[
            value(" Alice@X.Example ", "Alice"),
            value("b@x.example, c@x.example,", "Both"),
            value("  ", "Blank"),
            IncomingMailAddressValue { address: None, name: Some("None".into()), group: None },
        ])
        .unwrap();
        let json: Vec<Value> = converted.iter().map(MailAddressValue::to_json).collect();
        assert_eq!(
            json,
            vec![
                json!({ "address": "alice@x.example", "name": "Alice" }),
                json!({ "address": "b@x.example", "name": "Both" }),
                json!({ "address": "c@x.example", "name": "Both" }),
            ]
        );
        assert_eq!(convert_address_value(&[]), None);
        assert_eq!(convert_address_value(&[value("", "x")]), None);
    }

    #[test]
    fn group_members_take_the_place_of_their_group() {
        let group = IncomingMailAddressValue {
            address: None,
            name: Some("Team".into()),
            group: Some(vec![value("a@x.example", "A"), value("B@x.example", "")]),
        };
        let converted = convert_address_value(&[group, IncomingMailAddressValue::address("env@x.example")]).unwrap();
        let json: Vec<Value> = converted.iter().map(MailAddressValue::to_json).collect();
        assert_eq!(
            json,
            vec![
                json!({ "address": "a@x.example", "name": "A" }),
                json!({ "address": "b@x.example", "name": "" }),
                json!({ "address": "env@x.example" }),
            ]
        );
        let empty_group = IncomingMailAddressValue { group: Some(vec![]), ..value("x@y.z", "") };
        assert_eq!(convert_address_value(&[empty_group]), None);
    }

    #[test]
    fn a_repeated_header_joins_its_texts() {
        let headers = vec![
            IncomingMailAddress { value: vec![value("a@x.example", "")], text: "a@x.example".into() },
            IncomingMailAddress { value: vec![], text: "nobody".into() },
            IncomingMailAddress { value: vec![value("B@x.example", "B")], text: "B <B@x.example>".into() },
        ];
        let converted = convert_mail_address(&headers).unwrap();
        assert_eq!(converted.text, "a@x.example, nobody, B <B@x.example>");
        assert_eq!(converted.value.len(), 2);
        assert_eq!(convert_mail_address(&[]), None);
        assert_eq!(convert_mail_address(&[IncomingMailAddress { value: vec![], text: "x".into() }]), None);
    }

    #[test]
    fn a_parsed_message_becomes_an_incoming_mail() {
        let parsed = ParsedMessage {
            message_id: Some("<m@x>".into()),
            subject: Some("Hi".into()),
            text: Some("plain".into()),
            html: Some(String::new()),
            from: Some(ParsedAddress {
                value: vec![ParsedAddressValue { address: Some("a@x".into()), name: "A".into(), group: None }],
                text: "A <a@x>".into(),
            }),
            to: Some(OneOrMany::Many(vec![ParsedAddress::default(), ParsedAddress::default()])),
            date: crate::mime::parser::MessageDate::from_millis(0),
            attachments: vec![crate::mime::parser::ParsedAttachment {
                path: "/tmp/a".into(),
                filename: None,
                content_type: "image/png".into(),
                size: 3,
                cid: None,
            }],
            ..Default::default()
        };
        let incoming = incoming_mail_from_parsed(&parsed);
        // No HTML part: the text stands in.
        assert_eq!(incoming.html.as_deref(), Some("plain"));
        assert_eq!(incoming.date.as_deref(), Some("1970-01-01T00:00:00.000Z"));
        assert_eq!(incoming.to.as_ref().unwrap().len(), 2);
        assert_eq!(incoming.from.as_ref().unwrap()[0].value[0].name.as_deref(), Some("A"));
        let attachment = &incoming.attachments.as_ref().unwrap()[0];
        assert_eq!((attachment.filename.as_str(), attachment.path.as_str()), ("attachment", "/tmp/a"));
        assert_eq!(incoming.envelope_to, None);
    }

    #[test]
    fn envelope_recipients_are_trimmed_lower_cased_and_taken_once() {
        let mail = IncomingMail {
            envelope_to: Some(vec![
                IncomingMailAddressValue::address(" Box@Bob.Mail.Test "),
                IncomingMailAddressValue::address("box@bob.mail.test"),
                IncomingMailAddressValue::address(""),
                IncomingMailAddressValue::address("x@remote.example"),
            ]),
            ..Default::default()
        };
        assert_eq!(envelope_addresses(&mail), vec!["box@bob.mail.test", "x@remote.example"]);
        assert!(envelope_addresses(&IncomingMail::default()).is_empty());
    }

    #[test]
    fn attachments_take_the_model_defaults() {
        let attachment = Attachment::new("", "", "id".into(), 0);
        assert_eq!(
            attachment.to_json(),
            json!({ "filename": "unnamed_file", "contentType": "text/plain", "content": { "data": "id" }, "size": 0 })
        );
    }

    #[tokio::test]
    async fn files_are_stored_under_new_ids() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"bytes").unwrap();
        let id = save_file(&source).await.unwrap();
        let stored = std::fs::read(get_attachment_file_path(Some(&id))).unwrap();
        assert_eq!(stored, b"bytes");
        assert!(source.exists());
        let id2 = save_buffer(b"more").await.unwrap();
        assert_ne!(id, id2);
        assert_eq!(std::fs::read(get_attachment_file_path(Some(&id2))).unwrap(), b"more");
        std::fs::remove_file(get_attachment_file_path(Some(&id))).unwrap();
        std::fs::remove_file(get_attachment_file_path(Some(&id2))).unwrap();
        // Leaves no folder behind unless something else keeps files there.
        let _ = std::fs::remove_dir(ATTACHMENT_FOLDER);
    }
}
