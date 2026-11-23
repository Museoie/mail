//! What a mail must carry before it is sent (mails/validation.ts).

use serde_json::Value;

use super::send::{js_string, MailDataToSend};
use crate::js;
use crate::users::is_valid_email;

/// RFC 2822 line limit, in UTF-16 units.
pub const MAX_SUBJECT_LENGTH: usize = 998;
pub const MAX_HTML_LENGTH: usize = 10 * 1024 * 1024;

/// A send refused before anything went out; the message is shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailValidationError(pub String);

impl std::fmt::Display for MailValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MailValidationError {}

/// The comma-separated list, trimmed, without empty items.
pub fn split_addresses(list: Option<&str>) -> Vec<&str> {
    list.unwrap_or("").split(',').map(js::trim).filter(|address| !address.is_empty()).collect()
}

fn validate_email_list(list: Option<&str>, field_name: &str) -> Result<(), String> {
    for email in split_addresses(list.filter(|list| !list.is_empty())) {
        if !is_valid_email(email) {
            return Err(format!("Invalid {field_name} address: {email}"));
        }
    }
    Ok(())
}

/// `value.length` as JavaScript reads it: UTF-16 units of a string, items of
/// a list, nothing for any other value.
fn js_length(value: &Value) -> Option<usize> {
    match value {
        Value::String(text) => Some(js::utf16_len(text)),
        Value::Array(items) => Some(items.len()),
        _ => None,
    }
}

fn is_sender_local_part(sender: &str) -> bool {
    !sender.is_empty() && sender.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The first rule the mail breaks, checked in this order: sender, sender
/// name, recipients, the To, Cc and Bcc addresses, subject, body.
pub fn validate_mail_data(data: &MailDataToSend) -> Result<(), MailValidationError> {
    let fail = |message: &str| Err(MailValidationError(message.to_string()));
    let sender = match &data.sender {
        Value::String(sender) if !sender.is_empty() => sender,
        _ => return fail("Sender is required"),
    };
    if !is_sender_local_part(sender) {
        return fail("Invalid sender format");
    }
    if js::is_truthy(&data.sender_full_name) && js_string(&data.sender_full_name).contains(['\r', '\n']) {
        return fail("Invalid sender name");
    }

    // A Bcc-only send carries no `To:` header.
    let is_address_list = |list: Option<&Value>| matches!(list, None | Some(Value::String(_)));
    let lists = [Some(&data.to), data.cc.as_ref(), data.bcc.as_ref()];
    let has_recipient = lists.iter().all(|list| is_address_list(*list))
        && lists.iter().any(|list| !split_addresses(list.and_then(Value::as_str)).is_empty());
    if !has_recipient {
        return fail("Recipient email address is required");
    }

    let checks = [(data.to_list(), "recipient"), (data.cc_list(), "CC"), (data.bcc_list(), "BCC")];
    for (list, field_name) in checks {
        validate_email_list(list, field_name).map_err(MailValidationError)?;
    }

    if js::is_truthy(&data.subject) && js_length(&data.subject).is_some_and(|length| length > MAX_SUBJECT_LENGTH) {
        return Err(MailValidationError(format!(
            "Subject exceeds maximum length of {MAX_SUBJECT_LENGTH} characters"
        )));
    }
    if js::is_truthy(&data.html) && js_length(&data.html).is_some_and(|length| length > MAX_HTML_LENGTH) {
        return fail("Email body exceeds maximum size of 10MB");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn mail(fields: Value) -> MailDataToSend {
        MailDataToSend::from_json(fields.as_object().unwrap())
    }

    fn check(fields: Value) -> Result<(), String> {
        validate_mail_data(&mail(fields)).map_err(|error| error.0)
    }

    #[test]
    fn the_sender_is_a_non_empty_local_part() {
        for sender in [json!(null), json!(""), json!(5), json!(true), json!(["box"]), json!({})] {
            assert_eq!(check(json!({ "sender": sender, "to": "a@b.co" })), Err("Sender is required".into()));
        }
        assert_eq!(check(json!({ "to": "a@b.co" })), Err("Sender is required".into()));
        for sender in ["a b", "a@b", "ü", "a+b", "box\n"] {
            assert_eq!(check(json!({ "sender": sender, "to": "a@b.co" })), Err("Invalid sender format".into()));
        }
        assert_eq!(check(json!({ "sender": "A.B-c_d9", "to": "a@b.co" })), Ok(()));
    }

    #[test]
    fn a_display_name_may_not_break_a_line() {
        for name in [json!("a\nb"), json!("\r"), json!(["a\nb"])] {
            assert_eq!(check(json!({ "sender": "s", "senderFullName": name, "to": "a@b.co" })), Err("Invalid sender name".into()));
        }
        for name in [json!("Tab\tName"), json!(null), json!(5), json!({ "a": 1 })] {
            assert_eq!(check(json!({ "sender": "s", "senderFullName": name, "to": "a@b.co" })), Ok(()));
        }
    }

    #[test]
    fn recipients_are_required_and_must_be_strings() {
        let required = Err("Recipient email address is required".to_string());
        assert_eq!(check(json!({ "sender": "s" })), required);
        assert_eq!(check(json!({ "sender": "s", "to": " , ," })), required);
        assert_eq!(check(json!({ "sender": "s", "to": "a@b.co", "cc": null })), required);
        assert_eq!(check(json!({ "sender": "s", "to": "a@b.co", "bcc": ["x@y.co"] })), required);
        // A falsy To takes the empty default.
        assert_eq!(check(json!({ "sender": "s", "to": 0, "cc": "c@d.co" })), Ok(()));
        assert_eq!(check(json!({ "sender": "s", "to": 5, "cc": "c@d.co" })), required);
        assert_eq!(check(json!({ "sender": "s", "bcc": "b@d.co" })), Ok(()));
    }

    #[test]
    fn the_first_bad_address_is_named_to_before_cc_before_bcc() {
        assert_eq!(check(json!({ "sender": "s", "to": "good@x.co, bad1, bad2" })), Err("Invalid recipient address: bad1".into()));
        assert_eq!(
            check(json!({ "sender": "s", "to": "a@b.co", "cc": "x@y.co,  also bad " })),
            Err("Invalid CC address: also bad".into())
        );
        assert_eq!(
            check(json!({ "sender": "s", "to": "bad-to", "cc": "bad-cc", "bcc": "bad-bcc" })),
            Err("Invalid recipient address: bad-to".into())
        );
        assert_eq!(check(json!({ "sender": "s", "to": "a@b.co", "bcc": "nope" })), Err("Invalid BCC address: nope".into()));
    }

    #[test]
    fn a_subject_is_measured_in_utf16_units() {
        let too_long = Err("Subject exceeds maximum length of 998 characters".to_string());
        assert_eq!(check(json!({ "sender": "s", "to": "a@b.co", "subject": "s".repeat(998) })), Ok(()));
        assert_eq!(check(json!({ "sender": "s", "to": "a@b.co", "subject": "s".repeat(999) })), too_long);
        assert_eq!(check(json!({ "sender": "s", "to": "a@b.co", "subject": "😀".repeat(499) })), Ok(()));
        assert_eq!(check(json!({ "sender": "s", "to": "a@b.co", "subject": "😀".repeat(500) })), too_long);
        assert_eq!(check(json!({ "sender": "s", "to": "a@b.co", "subject": { "a": 1 } })), Ok(()));
    }
}
