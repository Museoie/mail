//! The two ways an admitted DATA transaction goes: inbound mail stored for
//! the local recipients, and an authenticated user's submission sent out
//! (`onDataIncoming` / `onDataOutgoing` in `smtp.ts`).

use serde_json::{json, Value};

use super::bounded::BoundedMessage;
use super::handler::{refusals, CredentialStamp, DataFailure, MailDirectory, MailHandler};
use super::outgoing::{
    address_list, resolve_outgoing_sender, split_envelope_recipients, EnvelopeRecipients, OutgoingSender,
};
use super::transaction_dir::{new_transaction_dir, remove_transaction_dir};
use super::types::{SmtpEnvelope, SmtpSession};
use crate::http::upload::UploadedFile;
use crate::logger;
use crate::mails::receive::{
    incoming_mail_from_parsed, save_mail_handler, IncomingMailAddressValue, SaveMailHandlerOptions,
};
use crate::mails::send::{send_mail, MailDataToSend, SendMailOptions};
use crate::mails::text_to_html::plain_text_to_html;
use crate::mime::parser::{parse_message, ParseOptions, ParsedMessage};
use crate::models::SignedUser;
use crate::util::get_user_domain;

/// The refusal a lane met while reading, as its answer.
fn refused(message: &mut BoundedMessage) -> Option<DataFailure> {
    let (error, coded) = message.refusal()?.to_hook_error();
    Some(if coded { DataFailure::Refused(error) } else { DataFailure::Failed(anyhow::anyhow!(error.message)) })
}

impl<D: MailDirectory> MailHandler<D> {
    pub(crate) async fn incoming(&self, mut message: BoundedMessage, session: &SmtpSession) -> Result<(), DataFailure> {
        let dir = new_transaction_dir();
        let outcome = async {
            let parsed = parse_message(&mut message, &ParseOptions { attachment_dir: dir.clone() }).await;
            if let Some(refusal) = refused(&mut message) {
                return Err(refusal);
            }
            let stored = match parsed {
                Ok(parsed) => store_incoming(parsed, session).await,
                Err(error) => Err(error),
            };
            let stored = match stored {
                Ok(stored) => stored,
                Err(error) => {
                    logger::error_with("Error parsing email", json!({}), &error);
                    return Err(DataFailure::Failed(error));
                }
            };
            // RCPT TO refuses the recipients this cannot store for, so a zero
            // here is a mailbox that went away mid-transaction.
            if stored == 0 {
                logger::warn(
                    "SMTP: accepted DATA that stored into no mailbox",
                    json!({ "remoteAddress": session.remote_address }),
                );
                return Err(refusals::mail_not_stored().into());
            }
            Ok(())
        }
        .await;
        drop(message);
        remove_transaction_dir(&dir).await;
        outcome
    }

    pub(crate) async fn outgoing(&self, mut message: BoundedMessage, session: &SmtpSession) -> Result<(), DataFailure> {
        let dir = new_transaction_dir();
        let outcome = async {
            let username = session.user.clone();
            let user = match username.as_deref() {
                Some(username) => match self.directory.get_user(username).await {
                    Ok(user) => user,
                    Err(error) => return Err(refused(&mut message).unwrap_or(DataFailure::Failed(error))),
                },
                None => None,
            };
            if let Some(refusal) = refused(&mut message) {
                return Err(refusal);
            }
            let signed = user.as_ref().and_then(|user| user.get_signed());
            let (Some(username), Some(user), Some(signed)) = (username, user, signed) else {
                logger::warn("SMTP: Unauthenticated user attempted to send email.", json!({}));
                return Err(DataFailure::Failed(anyhow::anyhow!("User not authenticated")));
            };

            // Credential-revocation fence at the send gate: a rotation that
            // landed after MAIL FROM / RCPT TO still loses the grant here.
            let stamped = session.state.get::<CredentialStamp>().map_or(1, |stamp| stamp.password_version);
            if user.password_version.unwrap_or(1) > stamped {
                logger::info(
                    "SMTP: refused an outgoing message on a rotated credential",
                    json!({ "authenticatedAs": username }),
                );
                return Err(refusals::credential_changed().into());
            }

            let parsed = parse_message(&mut message, &ParseOptions { attachment_dir: dir.clone() }).await;
            if let Some(refusal) = refused(&mut message) {
                return Err(refusal);
            }
            let parsed = parsed.map_err(DataFailure::Failed)?;
            send_outgoing(parsed, &username, signed, session).await.map_err(DataFailure::Failed)
        }
        .await;
        drop(message);
        remove_transaction_dir(&dir).await;
        outcome
    }
}

/// Stores an inbound message for its local recipients; how many mailboxes
/// it went into.
async fn store_incoming(parsed: ParsedMessage, session: &SmtpSession) -> anyhow::Result<usize> {
    let mut mail = incoming_mail_from_parsed(&parsed);
    // `envelope.mailFrom || undefined`: the null sender `<>` is an empty
    // address, which the save drops, leaving no envelope sender.
    mail.envelope_from =
        session.envelope.mail_from.as_ref().map(|from| vec![IncomingMailAddressValue::address(from.address.clone())]);
    mail.envelope_to =
        Some(session.envelope.rcpt_to.iter().map(|to| IncomingMailAddressValue::address(to.address.clone())).collect());
    // The client address, for the spam check's DNS blocklist lookups.
    let options = SaveMailHandlerOptions { remote_address: Some(session.remote_address.clone()) };
    save_mail_handler(&mail, &options).await
}

/// The first entry of the From header: its address and display name.
fn first_from(parsed: &ParsedMessage) -> (Option<&str>, Option<&str>) {
    let first = parsed.from.as_ref().and_then(|from| from.value.first());
    (first.and_then(|entry| entry.address.as_deref()), first.map(|entry| entry.name.as_str()))
}

/// What a submission is sent as, from the parsed message and the envelope.
pub(crate) fn outgoing_mail(
    parsed: &ParsedMessage,
    username: &str,
    user_domain: &str,
    envelope: &SmtpEnvelope,
) -> MailDataToSend {
    let (from_address, from_name) = first_from(parsed);
    let envelope_from = envelope.mail_from.as_ref().map(|from| from.address.as_str());
    let addressed_to = address_list(parsed.to.as_ref());
    let rcpt_to: Vec<String> = envelope.rcpt_to.iter().map(|to| to.address.clone()).collect();
    let OutgoingSender { sender, recipients } =
        resolve_outgoing_sender(username, user_domain, from_address, envelope_from, &rcpt_to, &addressed_to);
    let EnvelopeRecipients { to, cc, bcc } =
        split_envelope_recipients(&recipients, &addressed_to, &address_list(parsed.cc.as_ref()));

    let list = |addresses: Vec<String>| (!addresses.is_empty()).then(|| Value::String(addresses.join(",")));
    let non_empty = |text: Option<&str>| text.filter(|text| !text.is_empty()).map(str::to_string);
    // A text-only submission is sent as HTML, so convert it rather than let
    // the recipient's client collapse its line breaks.
    let html = non_empty(parsed.html.as_deref())
        .or_else(|| non_empty(parsed.text.as_deref()).map(|text| plain_text_to_html(&text)))
        .unwrap_or_default();
    let sender_full_name = non_empty(from_name).unwrap_or_else(|| sender.clone());
    MailDataToSend {
        // The model's defaults: a falsy subject is "No Subject".
        subject: Value::String(non_empty(parsed.subject.as_deref()).unwrap_or_else(|| "No Subject".to_string())),
        to: Value::String(to.join(",")),
        cc: list(cc),
        bcc: list(bcc),
        html: Value::String(html),
        sender_full_name: Value::String(sender_full_name),
        sender: Value::String(sender),
        // A reply written in a mail client keeps its place in the
        // conversation: the headers go out with it and onto the Sent copy.
        in_reply_to: parsed.in_reply_to.clone().map(Value::String),
        references: parsed.references.clone(),
    }
}

/// A submission's parsed attachments as the files of an HTTP send.
pub(crate) fn outgoing_files(parsed: &ParsedMessage) -> Vec<UploadedFile> {
    parsed
        .attachments
        .iter()
        .map(|attachment| UploadedFile {
            name: attachment.filename.clone().filter(|name| !name.is_empty()).unwrap_or_else(|| "attachment".into()),
            mimetype: attachment.content_type.clone(),
            size: attachment.size,
            temp_file_path: attachment.path.to_string_lossy().into_owned(),
        })
        .collect()
}

/// Sends an authenticated submission, attachments included, through Mailgun
/// and records it in Sent.
async fn send_outgoing(
    parsed: ParsedMessage,
    username: &str,
    user: SignedUser,
    session: &SmtpSession,
) -> anyhow::Result<()> {
    let mail = outgoing_mail(&parsed, username, &get_user_domain(username), &session.envelope);
    let files = outgoing_files(&parsed);
    send_mail(&user, mail, &files, SendMailOptions::default()).await.map_err(anyhow::Error::new)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! What a submission is sent as (`onDataOutgoing` in `smtp.test.ts`).

    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::smtp::types::SmtpAddress;

    async fn parse(message: &str) -> ParsedMessage {
        let dir = tempfile::tempdir().unwrap();
        let options = ParseOptions { attachment_dir: dir.path().join("tx") };
        parse_message(message.as_bytes(), &options).await.unwrap()
    }

    fn envelope(from: &str, to: &[&str]) -> SmtpEnvelope {
        let address = |address: &str| SmtpAddress { address: address.to_string(), args: None };
        SmtpEnvelope {
            mail_from: Some(address(from)),
            rcpt_to: to.iter().map(|to| address(to)).collect(),
            ..SmtpEnvelope::default()
        }
    }

    #[tokio::test]
    async fn sends_a_text_only_message_as_html_under_the_users_own_name() {
        let parsed =
            parse("From: Alice Example <alice@alice.test.com>\r\nTo: bob@remote.example\r\nSubject: Hi\r\n\r\nline one\r\nline two\r\n")
                .await;
        let envelope = envelope("alice@alice.test.com", &["bob@remote.example"]);
        let mail = outgoing_mail(&parsed, "alice", "alice.test.com", &envelope);
        assert_eq!(mail.sender, json!("alice"));
        assert_eq!(mail.sender_full_name, json!("Alice Example"));
        assert_eq!(mail.to, json!("bob@remote.example"));
        assert_eq!((mail.cc.clone(), mail.bcc.clone()), (None, None));
        assert_eq!(mail.subject, json!("Hi"));
        assert_eq!(mail.html, json!(plain_text_to_html(parsed.text.as_deref().unwrap())));
        assert!(mail.html.as_str().unwrap().starts_with("<div style=\"white-space: pre-wrap\">"));
        assert_eq!((mail.in_reply_to, mail.references), (None, None));
    }

    #[tokio::test]
    async fn splits_the_envelope_into_to_cc_and_bcc_and_defaults_the_subject() {
        let parsed =
            parse("From: shop@alice.test.com\r\nTo: a@remote.example\r\nCc: B <b@remote.example>\r\n\r\nbody\r\n")
                .await;
        let rcpt = ["a@remote.example", "b@remote.example", "hidden@remote.example"];
        let mail = outgoing_mail(&parsed, "alice", "alice.test.com", &envelope("alice@alice.test.com", &rcpt));
        // A From header naming another account of the user's domain sends as it.
        assert_eq!(mail.sender, json!("shop"));
        assert_eq!(mail.sender_full_name, json!("shop"));
        assert_eq!(mail.to, json!("a@remote.example"));
        assert_eq!(mail.cc, Some(json!("b@remote.example")));
        assert_eq!(mail.bcc, Some(json!("hidden@remote.example")));
        assert_eq!(mail.subject, json!("No Subject"));
    }

    #[tokio::test]
    async fn sends_a_header_less_submission_entirely_as_bcc_and_keeps_the_html() {
        let parsed = parse("Content-Type: text/html\r\n\r\n<p>hi</p>\r\n").await;
        let mail =
            outgoing_mail(&parsed, "admin", "test.com", &envelope("", &["x@remote.example", "y@remote.example"]));
        assert_eq!(mail.sender, json!("admin"));
        assert_eq!(mail.to, json!(""));
        assert_eq!(mail.bcc, Some(json!("x@remote.example,y@remote.example")));
        assert_eq!(mail.html, json!(parsed.html.clone().unwrap()));
    }

    #[tokio::test]
    async fn hands_on_every_attachment_as_a_file_an_http_send_would_upload() {
        let dir = tempfile::tempdir().unwrap();
        let options = ParseOptions { attachment_dir: dir.path().join("tx") };
        let message = concat!(
            "From: alice@alice.test.com\r\nTo: bob@remote.example\r\nSubject: files\r\n",
            "Content-Type: multipart/mixed; boundary=\"mix\"\r\n\r\n",
            "--mix\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n",
            "--mix\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"contract.pdf\"\r\n\r\n%PDF\r\n",
            "--mix\r\nContent-Type: image/png\r\n\r\npng\r\n",
            "--mix--\r\n"
        );
        let parsed = parse_message(message.as_bytes(), &options).await.unwrap();
        let files = outgoing_files(&parsed);
        let described: Vec<(&str, &str, u64)> =
            files.iter().map(|file| (file.name.as_str(), file.mimetype.as_str(), file.size)).collect();
        // A part that names no file is called what a received one is.
        assert_eq!(described, vec![("contract.pdf", "application/pdf", 4), ("attachment", "image/png", 3)]);
        assert_eq!(std::fs::read(&files[0].temp_file_path).unwrap(), b"%PDF");
        assert!(files.iter().all(|file| Path::new(&file.temp_file_path).starts_with(dir.path().join("tx"))));
        assert!(outgoing_files(&parse("Subject: none\r\n\r\nbody\r\n").await).is_empty());
    }

    #[tokio::test]
    async fn carries_a_replys_in_reply_to_and_references() {
        let parsed = parse(concat!(
            "From: alice@alice.test.com\r\nTo: bob@remote.example\r\nSubject: Re: x\r\n",
            "In-Reply-To: <b@remote.example>\r\nReferences: <a@remote.example> <b@remote.example>\r\n\r\nok\r\n"
        ))
        .await;
        let envelope = envelope("alice@alice.test.com", &["bob@remote.example"]);
        let mail = outgoing_mail(&parsed, "alice", "alice.test.com", &envelope);
        assert_eq!(mail.in_reply_to, Some(json!("<b@remote.example>")));
        assert_eq!(mail.references, Some(vec!["<a@remote.example>".to_string(), "<b@remote.example>".to_string()]));
    }
}
