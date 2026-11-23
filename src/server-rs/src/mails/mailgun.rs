//! Sending through Mailgun's HTTP API (mails/mailgun.ts).

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};

use crate::alarm::{http_client, spawn_alarm};
use crate::http::upload::UploadedFile;
use crate::util::get_user_domain;
use crate::{env, js, logger};

use super::send::{js_string, text_of_html, MailDataToSend};

const DEFAULT_MAILGUN_URL: &str = "https://api.mailgun.net";
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const MAX_BODY_BYTES: u64 = 50 * 1024 * 1024;

/// One upload per address against a 30 s client: at most this many at once.
pub const MAX_CONCURRENT_SENDS: usize = 5;

/// A file part of a Mailgun message, streamed from disk while the request uploads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailgunAttachment {
    pub path: String,
    pub filename: String,
    pub content_type: String,
}

/// The `POST /v3/{domain}/messages` parameters, in wire order. `subject` and
/// `html` keep the JSON value the client sent; a list repeats its field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MailgunMessage {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Option<String>,
    pub bcc: Option<String>,
    pub subject: Value,
    pub html: Value,
    pub text: Option<String>,
    pub attachment: Vec<MailgunAttachment>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

/// Mailgun's answer to an accepted message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MailgunResponse {
    pub status: u16,
    /// The queued message's id, when the answer was an object carrying one.
    pub id: Option<Value>,
    pub message: Option<Value>,
}

/// A failed Mailgun request. `status` is the HTTP status, or 400 when no
/// response arrived; `message` is the response's error or status text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailgunApiError {
    pub status: u16,
    pub message: String,
    pub details: String,
}

impl MailgunApiError {
    /// As the TypeScript constructor reads `body`: its `error` field, else
    /// the status text; `details` is a string body or its `message` field.
    pub fn new(status: u16, status_text: &str, body: &Value) -> MailgunApiError {
        let field = |name: &str| match body {
            Value::Object(fields) => fields.get(name).filter(|value| js::is_truthy(value)).map(js_string),
            _ => None,
        };
        let message = field("error").filter(|text| !text.is_empty()).unwrap_or_else(|| status_text.to_string());
        let details = match body {
            Value::String(text) => text.clone(),
            _ => field("message").unwrap_or_default(),
        };
        MailgunApiError { status, message, details }
    }
}

impl std::fmt::Display for MailgunApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MailgunApiError {}

fn attachments_of(files: &[UploadedFile]) -> Vec<MailgunAttachment> {
    files
        .iter()
        .map(|file| MailgunAttachment {
            path: file.temp_file_path.clone(),
            filename: file.name.clone(),
            content_type: file.mimetype.clone(),
        })
        .collect()
}

/// A quote or line break in a client's file name would end the part header
/// early; they are escaped the way browsers do it.
fn quote_filename(name: &str) -> String {
    name.replace('"', "%22").replace('\r', "%0D").replace('\n', "%0A")
}

/// A content type carrying any control character is replaced with the
/// generic binary type.
fn part_content_type(content_type: &str) -> &str {
    if !content_type.is_empty() && !content_type.chars().any(|c| (c as u32) < 0x20 || c == '\x7f') {
        content_type
    } else {
        "application/octet-stream"
    }
}

/// One piece of an encoded body: bytes, or a file read as the body is pulled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyPart {
    Bytes(Bytes),
    File { path: PathBuf, size: u64 },
}

impl BodyPart {
    pub fn len(&self) -> u64 {
        match self {
            BodyPart::Bytes(bytes) => bytes.len() as u64,
            BodyPart::File { size, .. } => *size,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A message encoded as multipart/form-data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedMessage {
    pub parts: Vec<BodyPart>,
    pub length: u64,
    pub content_type: String,
}

/// The items a field sends: a list repeats the field once per item, any other
/// value is sent as its string. A falsy value sends nothing.
fn field_items(value: &Value) -> Vec<String> {
    if !js::is_truthy(value) {
        return Vec::new();
    }
    match value {
        Value::Array(items) => items.iter().map(js_string).collect(),
        other => vec![js_string(other)],
    }
}

/// Encodes a message as multipart/form-data with `boundary`. Empty fields are
/// left out, `to` repeats once per address, and attachments are read from
/// disk only as the body is sent.
pub fn encode_mailgun_message_with(message: &MailgunMessage, boundary: &str) -> std::io::Result<EncodedMessage> {
    let head = |disposition: &str, content_type: Option<&str>| {
        let mut text = format!("--{boundary}\r\nContent-Disposition: form-data; {disposition}\r\n");
        if let Some(content_type) = content_type {
            text.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        text.push_str("\r\n");
        BodyPart::Bytes(Bytes::from(text))
    };
    let crlf = || BodyPart::Bytes(Bytes::from_static(b"\r\n"));
    let mut parts = Vec::new();
    let field = |parts: &mut Vec<BodyPart>, name: &str, items: Vec<String>| {
        for item in items {
            parts.push(head(&format!("name=\"{name}\""), None));
            parts.push(BodyPart::Bytes(Bytes::from(item)));
            parts.push(crlf());
        }
    };
    let text_value = |value: &Option<String>| value.clone().filter(|text| !text.is_empty()).into_iter().collect::<Vec<_>>();

    field(&mut parts, "from", if message.from.is_empty() { vec![] } else { vec![message.from.clone()] });
    field(&mut parts, "to", message.to.clone());
    field(&mut parts, "cc", text_value(&message.cc));
    field(&mut parts, "bcc", text_value(&message.bcc));
    field(&mut parts, "subject", field_items(&message.subject));
    field(&mut parts, "html", field_items(&message.html));
    field(&mut parts, "text", text_value(&message.text));
    for file in &message.attachment {
        let disposition = format!("name=\"attachment\"; filename=\"{}\"", quote_filename(&file.filename));
        parts.push(head(&disposition, Some(part_content_type(&file.content_type))));
        let size = std::fs::metadata(&file.path)?.len();
        parts.push(BodyPart::File { path: PathBuf::from(&file.path), size });
        parts.push(crlf());
    }
    field(&mut parts, "h:In-Reply-To", text_value(&message.in_reply_to));
    field(&mut parts, "h:References", text_value(&message.references));
    parts.push(BodyPart::Bytes(Bytes::from(format!("--{boundary}--\r\n"))));

    let length = parts.iter().map(BodyPart::len).sum();
    Ok(EncodedMessage { parts, length, content_type: format!("multipart/form-data; boundary={boundary}") })
}

/// [`encode_mailgun_message_with`] under a fresh random boundary.
pub fn encode_mailgun_message(message: &MailgunMessage) -> std::io::Result<EncodedMessage> {
    let boundary = format!("--------------------------{}", uuid::Uuid::new_v4().simple());
    encode_mailgun_message_with(message, &boundary)
}

fn body_stream(parts: Vec<BodyPart>) -> reqwest::Body {
    let pieces = stream::iter(parts).flat_map(|part| match part {
        BodyPart::Bytes(bytes) => stream::once(async move { Ok::<Bytes, std::io::Error>(bytes) }).boxed(),
        BodyPart::File { path, .. } => stream::once(async move { tokio::fs::File::open(path).await })
            .map(|opened| match opened {
                Ok(file) => tokio_util::io::ReaderStream::new(file).boxed(),
                Err(error) => stream::once(async move { Err(error) }).boxed(),
            })
            .flatten()
            .boxed(),
    });
    reqwest::Body::wrap_stream(pieces)
}

/// The network failure as the Bun server's `fetch` names it.
fn network_failure(error: &reqwest::Error) -> MailgunApiError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    let mut code = None;
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            code = Some(match io.kind() {
                std::io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
                std::io::ErrorKind::ConnectionReset => "ECONNRESET",
                std::io::ErrorKind::ConnectionAborted => "ECONNABORTED",
                std::io::ErrorKind::TimedOut => "ETIMEDOUT",
                _ => "ConnectionClosed",
            });
            break;
        }
        source = current.source();
    }
    let code = code.unwrap_or(if error.is_connect() { "ConnectionRefused" } else { "ConnectionClosed" });
    MailgunApiError::new(400, code, &Value::String(error.to_string()))
}

/// The answer body: JSON when it parses, the text otherwise.
fn parse_body(text: &str) -> Value {
    js::json_parse(text).unwrap_or_else(|| Value::String(text.to_string()))
}

/// Sends one message through Mailgun's HTTP API, failing after 30 seconds or
/// without a request when the body would pass 50 MiB. The API root is
/// `MAILGUN_URL` when set and non-empty; it and `MAILGUN_KEY` are read per call.
pub async fn create_mailgun_message(domain: &str, message: &MailgunMessage) -> anyhow::Result<MailgunResponse> {
    let key = env::var("MAILGUN_KEY").unwrap_or_else(|| "mailgun_key".to_string());
    let api_root = env::non_empty("MAILGUN_URL").unwrap_or_else(|| DEFAULT_MAILGUN_URL.to_string());
    let url = format!("{}/v3/{domain}/messages", api_root.trim_end_matches('/'));
    let encoded = encode_mailgun_message(message)?;
    if encoded.length > MAX_BODY_BYTES {
        return Err(MailgunApiError::new(
            400,
            "ERR_FR_MAX_BODY_LENGTH_EXCEEDED",
            &json!("Request body larger than maxBodyLength limit"),
        )
        .into());
    }

    let authorization = {
        use base64::Engine;
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("api:{key}")))
    };
    let exchange = async {
        let response = http_client()
            .post(&url)
            .header("Authorization", authorization)
            .header("Content-Type", &encoded.content_type)
            .header("Content-Length", encoded.length.to_string())
            .body(body_stream(encoded.parts.clone()))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        Ok::<_, reqwest::Error>((status, text))
    };
    let (status, text) = match tokio::time::timeout(Duration::from_millis(REQUEST_TIMEOUT_MS), exchange).await {
        Err(_) => {
            return Err(MailgunApiError::new(
                400,
                "ECONNABORTED",
                &json!(format!("timeout of {REQUEST_TIMEOUT_MS}ms exceeded")),
            )
            .into())
        }
        Ok(Err(error)) => return Err(network_failure(&error).into()),
        Ok(Ok(answer)) => answer,
    };
    let status_text = status.canonical_reason().unwrap_or("");
    let data = parse_body(&text);
    let code = status.as_u16();
    if !status.is_success() {
        let body = if js::is_truthy(&data) { data } else { json!(format!("Request failed with status code {code}")) };
        return Err(MailgunApiError::new(code, status_text, &body).into());
    }
    if data == json!("Mailgun Magnificent API") {
        return Err(MailgunApiError::new(400, "Incorrect url", &data).into());
    }
    Ok(match data {
        Value::String(text) => MailgunResponse { status: code, id: None, message: Some(Value::String(text)) },
        Value::Object(fields) => MailgunResponse {
            status: code,
            id: fields.get("id").cloned(),
            message: fields.get("message").cloned(),
        },
        _ => MailgunResponse { status: code, id: None, message: None },
    })
}

/// The comma-separated list, trimmed, without empty items.
pub fn split_addresses(list: Option<&str>) -> Vec<String> {
    list.unwrap_or("").split(',').map(|address| js::trim(address).to_string()).filter(|a| !a.is_empty()).collect()
}

/// Runs `send` for each address, [`MAX_CONCURRENT_SENDS`] at a time, and
/// answers every outcome in the order of `addresses`.
pub async fn send_batched<T, F, Fut>(addresses: &[String], send: F) -> Vec<anyhow::Result<T>>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut results = Vec::with_capacity(addresses.len());
    for batch in addresses.chunks(MAX_CONCURRENT_SENDS) {
        results.extend(futures_util::future::join_all(batch.iter().cloned().map(&send)).await);
    }
    results
}

/// Who a send goes to through Mailgun: the visible To, the Cc and Bcc that
/// ride along with it, or one message per Bcc address when no outside
/// address may be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailgunPlan {
    /// Every recipient is on the served domain: nothing to send.
    Local,
    One { to: Vec<String>, cc: Option<String>, bcc: Option<String> },
    PerBcc(Vec<String>),
}

/// Decides [`MailgunPlan`] for the lists of a send. Only addresses outside
/// `@<email_domain>` (a case-sensitive suffix test) are relayed.
pub fn plan_mailgun_send(to: Option<&str>, cc: Option<&str>, bcc: Option<&str>, email_domain: &str) -> MailgunPlan {
    let suffix = format!("@{email_domain}");
    let is_external = |address: &String| !address.ends_with(&suffix);
    let (to, cc, bcc) = (split_addresses(to), split_addresses(cc), split_addresses(bcc));
    let recipients: Vec<&String> = to.iter().chain(&cc).chain(&bcc).collect();
    if !recipients.is_empty() && !recipients.iter().any(|address| is_external(address)) {
        return MailgunPlan::Local;
    }
    let external_to: Vec<String> = to.into_iter().filter(is_external).collect();
    let external_cc: Vec<String> = cc.into_iter().filter(is_external).collect();
    let external_bcc: Vec<String> = bcc.into_iter().filter(is_external).collect();
    let non_empty = |text: String| (!text.is_empty()).then_some(text);
    if !external_to.is_empty() {
        return MailgunPlan::One {
            cc: non_empty(external_cc.join(",")),
            bcc: non_empty(external_bcc.join(",")),
            to: external_to,
        };
    }
    if !external_cc.is_empty() {
        // A promoted Cc already is the visible To; it is not repeated as Cc.
        return MailgunPlan::One { to: external_cc, cc: None, bcc: non_empty(external_bcc.join(",")) };
    }
    MailgunPlan::PerBcc(external_bcc)
}

/// Sends `mail` from `username`'s domain through Mailgun. `None` when every
/// recipient is local and nothing was sent.
pub async fn send_mailgun_mail(
    username: &str,
    mail: &MailDataToSend,
    files: &[UploadedFile],
) -> anyhow::Result<Option<MailgunResponse>> {
    // Read at call time: the filter is only meaningful against the domain
    // the process is actually serving.
    let email_domain = env::var("EMAIL_DOMAIN").unwrap_or_else(|| "mydomain".to_string());
    let plan = plan_mailgun_send(mail.to_list(), mail.cc_list(), mail.bcc_list(), &email_domain);
    if plan == MailgunPlan::Local {
        logger::info("All recipients are to myself, skipping Mailgun sending.", json!({}));
        return Ok(None);
    }

    let text = text_of_html(&mail.html)?;
    let from_address = format!("{}@{}", js_string(&mail.sender), get_user_domain(username));
    let from = if js::is_truthy(&mail.sender_full_name) {
        format!("{} <{from_address}>", js_string(&mail.sender_full_name))
    } else {
        from_address
    };
    let attachment = attachments_of(files);
    let message = |to: Vec<String>, cc: Option<String>, bcc: Option<String>| MailgunMessage {
        from: from.clone(),
        to,
        cc,
        bcc,
        subject: mail.subject.clone(),
        html: mail.html.clone(),
        text: Some(text.clone()),
        attachment: attachment.clone(),
        in_reply_to: mail.in_reply_to_text(),
        references: mail.references.as_ref().filter(|ids| !ids.is_empty()).map(|ids| ids.join(" ")),
    };

    let bcc = match plan {
        MailgunPlan::Local => unreachable!("answered above"),
        MailgunPlan::One { to, cc, bcc } => {
            let data = create_mailgun_message(&email_domain, &message(to, cc, bcc)).await?;
            logger::info("Email sending request succeeded", json!({}));
            return Ok(Some(data));
        }
        MailgunPlan::PerBcc(bcc) => bcc,
    };

    let results = send_batched(&bcc, |address| {
        let message = message(vec![address], None, None);
        let domain = email_domain.clone();
        async move { create_mailgun_message(&domain, &message).await }
    })
    .await;
    let failed: Vec<&str> = bcc
        .iter()
        .zip(&results)
        .filter(|(_, result)| result.is_err())
        .map(|(address, _)| address.as_str())
        .collect();
    let failed_list = failed.join(", ");
    let failed_json = json!(failed);
    let mut delivered = None;
    let mut first_rejection = None;
    for result in results {
        match result {
            Ok(response) if delivered.is_none() => delivered = Some(response),
            Err(error) if first_rejection.is_none() => first_rejection = Some(error),
            _ => {}
        }
    }
    let Some(delivered) = delivered else {
        return Err(first_rejection.unwrap_or_else(|| anyhow::anyhow!("No deliverable recipient")));
    };

    // Some recipients already hold the message: keep the send successful and
    // alarm the gap instead.
    if !failed_list.is_empty() {
        logger::error("Some Bcc recipients were not delivered", json!({ "failed": failed_json }));
        spawn_alarm("Mail Send Partially Failed", format!("**Undelivered Bcc recipients:** {failed_list}"));
    }
    logger::info("Email sending request succeeded", json!({}));
    Ok(Some(delivered))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(encoded: &EncodedMessage) -> String {
        encoded
            .parts
            .iter()
            .map(|part| match part {
                BodyPart::Bytes(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
                BodyPart::File { path, .. } => std::fs::read_to_string(path).unwrap(),
            })
            .collect()
    }

    #[test]
    fn fields_go_out_in_wire_order_and_empty_ones_are_left_out() {
        let message = MailgunMessage {
            from: "Box <box@bob.mail.test>".into(),
            to: vec!["a@x.example".into(), "b@x.example".into()],
            cc: Some("".into()),
            bcc: Some("c@x.example".into()),
            subject: json!("Hi"),
            html: json!(""),
            text: Some("plain".into()),
            in_reply_to: Some("<r@x>".into()),
            references: None,
            ..Default::default()
        };
        let encoded = encode_mailgun_message_with(&message, "B").unwrap();
        let part = |name: &str, value: &str| format!("--B\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n");
        let expected = [
            part("from", "Box <box@bob.mail.test>"),
            part("to", "a@x.example"),
            part("to", "b@x.example"),
            part("bcc", "c@x.example"),
            part("subject", "Hi"),
            part("text", "plain"),
            part("h:In-Reply-To", "<r@x>"),
            "--B--\r\n".to_string(),
        ]
        .concat();
        assert_eq!(wire(&encoded), expected);
        assert_eq!(encoded.length as usize, expected.len());
        assert_eq!(encoded.content_type, "multipart/form-data; boundary=B");
    }

    #[test]
    fn other_json_values_are_sent_as_their_strings_and_a_list_repeats_the_field() {
        let message = MailgunMessage {
            from: "f".into(),
            to: vec!["t".into()],
            subject: json!(["a", 5, null, { "x": 1 }]),
            html: json!(true),
            ..Default::default()
        };
        let text = wire(&encode_mailgun_message_with(&message, "B").unwrap());
        for value in ["\r\n\r\na\r\n", "\r\n\r\n5\r\n", "\r\n\r\nnull\r\n", "\r\n\r\n[object Object]\r\n", "name=\"html\"\r\n\r\ntrue\r\n"] {
            assert!(text.contains(value), "{value:?} in {text:?}");
        }
        let numeric = MailgunMessage { subject: json!(0), html: json!(1.5), ..message };
        let text = wire(&encode_mailgun_message_with(&numeric, "B").unwrap());
        assert!(!text.contains("name=\"subject\""));
        assert!(text.contains("name=\"html\"\r\n\r\n1.5\r\n"));
    }

    #[test]
    fn attachments_stream_from_disk_with_escaped_names_and_safe_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"content").unwrap();
        let path = path.to_string_lossy().into_owned();
        let message = MailgunMessage {
            from: "f".into(),
            to: vec!["t".into()],
            subject: json!("s"),
            attachment: vec![
                MailgunAttachment { path: path.clone(), filename: "a\"b\r\n.txt".into(), content_type: "text/plain".into() },
                MailgunAttachment { path: path.clone(), filename: "c".into(), content_type: "text/x\u{1}y".into() },
                MailgunAttachment { path, filename: "d".into(), content_type: "".into() },
            ],
            in_reply_to: Some("<r>".into()),
            ..Default::default()
        };
        let encoded = encode_mailgun_message_with(&message, "B").unwrap();
        let text = wire(&encoded);
        assert!(text.contains(
            "--B\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"a%22b%0D%0A.txt\"\r\nContent-Type: text/plain\r\n\r\ncontent\r\n"
        ));
        assert_eq!(text.matches("Content-Type: application/octet-stream\r\n").count(), 2);
        // Attachments come before the reply headers.
        assert!(text.find("name=\"attachment\"").unwrap() < text.find("h:In-Reply-To").unwrap());
        assert_eq!(encoded.length as usize, text.len());
    }

    #[test]
    fn errors_read_the_error_field_or_fall_back_to_the_status_text() {
        let error = MailgunApiError::new(400, "Bad Request", &json!({ "error": "custom", "message": "m" }));
        assert_eq!((error.message.as_str(), error.details.as_str()), ("custom", "m"));
        let error = MailgunApiError::new(400, "Bad Request", &json!({ "message": "m" }));
        assert_eq!(error.message, "Bad Request");
        let error = MailgunApiError::new(400, "ECONNREFUSED", &json!("connect failed"));
        assert_eq!((error.message.as_str(), error.details.as_str()), ("ECONNREFUSED", "connect failed"));
        assert_eq!(MailgunApiError::new(400, "X", &json!({ "error": 5 })).message, "5");
    }

    #[test]
    fn only_outside_addresses_are_relayed() {
        let d = "mail.test";
        assert_eq!(plan_mailgun_send(Some("a@mail.test"), Some("b@mail.test"), None, d), MailgunPlan::Local);
        assert_eq!(
            plan_mailgun_send(Some("a@mail.test, x@out.example"), Some("y@out.example,c@mail.test"), Some("z@out.example"), d),
            MailgunPlan::One { to: vec!["x@out.example".into()], cc: Some("y@out.example".into()), bcc: Some("z@out.example".into()) }
        );
        // The suffix test is case-sensitive, and a user subdomain is outside.
        assert_eq!(
            plan_mailgun_send(Some("a@MAIL.TEST"), None, Some("b@bob.mail.test"), d),
            MailgunPlan::One { to: vec!["a@MAIL.TEST".into()], cc: None, bcc: Some("b@bob.mail.test".into()) }
        );
        // With no outside To, the outside Cc becomes the To.
        assert_eq!(
            plan_mailgun_send(Some("me@mail.test"), Some("c1@out.example, c2@out.example"), None, d),
            MailgunPlan::One { to: vec!["c1@out.example".into(), "c2@out.example".into()], cc: None, bcc: None }
        );
        assert_eq!(
            plan_mailgun_send(None, None, Some("b1@out.example,b2@out.example"), d),
            MailgunPlan::PerBcc(vec!["b1@out.example".into(), "b2@out.example".into()])
        );
        assert_eq!(plan_mailgun_send(Some(" , "), None, None, d), MailgunPlan::PerBcc(vec![]));
    }

    #[tokio::test]
    async fn batches_run_five_at_a_time_and_keep_their_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let addresses: Vec<String> = (0..12).map(|i| format!("a{i}")).collect();
        let results = send_batched(&addresses, |address| {
            let (running, peak) = (running.clone(), peak.clone());
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                running.fetch_sub(1, Ordering::SeqCst);
                if address == "a3" {
                    anyhow::bail!("refused");
                }
                Ok(address)
            }
        })
        .await;
        assert_eq!(peak.load(Ordering::SeqCst), 5);
        assert_eq!(results.len(), 12);
        assert!(results[3].is_err());
        assert_eq!(results[11].as_ref().unwrap(), "a11");
    }
}
