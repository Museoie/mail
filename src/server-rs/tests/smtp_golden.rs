//! Holds the Rust SMTP server to what the Bun server does on the wire.
//!
//! `tests/golden/smtp/record.ts` ran every conversation in
//! `tests/golden/smtp/conversations.ts` against the Bun `SmtpServer` and wrote
//! down what the client saw at each step and what the hooks were called
//! with. This replays each conversation against the Rust server, with hooks
//! that answer the same way, and expects the same on both counts.
//!
//! Only three things are not compared literally: the session id and the
//! ephemeral ports (the recorder already wrote placeholders for them), the
//! host name in the greeting of the one listener that has no name, and the
//! TLS alert a TLS library may send before it gives up on a handshake that
//! was never one.

mod smtp_support;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use mail_server::smtp::address_command::parse_address_command;
use mail_server::smtp::connection::decode_base64_lenient;
use mail_server::smtp::{
    create_smtp_server, evict_smtp_connections, register_smtp_server, unregister_smtp_server, SharedSmtpHandler,
    SmtpAddress, SmtpArgValue, SmtpAuthRequest, SmtpAuthResponse, SmtpDataStream, SmtpHandler, SmtpHookError,
    SmtpServerOptions, SmtpSession,
};
use smtp_support::{binary, bytes_of, tls_source, Client};

/// How long a "slow" hook takes, as in `record.ts`.
const SLOW: Duration = Duration::from_millis(300);

fn golden(file: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/smtp").join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn address_json(address: &SmtpAddress) -> Value {
    let args = match &address.args {
        None => Value::Bool(false),
        Some(args) => Value::Object(
            args.iter()
                .map(|(key, value)| {
                    let value = match value {
                        SmtpArgValue::Flag => Value::Bool(true),
                        SmtpArgValue::Value(text) => Value::String(text.clone()),
                    };
                    (key.clone(), value)
                })
                .collect::<Map<String, Value>>(),
        ),
    };
    json!({ "address": address.address, "args": args })
}

/// The recipients of an envelope, or their number and digest when there are
/// many — the form `recipientsJson` of `record.ts` gives them.
fn recipients_json(recipients: &[SmtpAddress]) -> Value {
    if recipients.len() <= 8 {
        return Value::Array(recipients.iter().map(address_json).collect());
    }
    let lines: Vec<String> = recipients
        .iter()
        .map(|recipient| {
            let sorted: BTreeMap<&String, &SmtpArgValue> = recipient.args.iter().flatten().collect();
            let parameters: String = sorted
                .into_iter()
                .map(|(key, value)| match value {
                    SmtpArgValue::Flag => format!(";{key}"),
                    SmtpArgValue::Value(text) => format!(";{key}={text}"),
                })
                .collect();
            format!("{}{parameters}", recipient.address)
        })
        .collect();
    json!({ "length": recipients.len(), "sha256": hex::encode(Sha256::digest(lines.join("\n"))) })
}

/// The bytes of a message, or their size and digest when there are many.
fn data_json(data: &[u8]) -> Value {
    match data.len() > 2000 {
        true => json!({ "length": data.len(), "sha256": hex::encode(Sha256::digest(data)) }),
        false => Value::String(binary(data)),
    }
}

/// What the recording hooks of the Bun run stamp on the session at AUTH.
struct PasswordVersion(i64);

#[derive(Default)]
struct Recording {
    calls: Mutex<Vec<Value>>,
    /// Hooks still at work: a conversation is over once this is back to zero.
    pending: AtomicUsize,
    client_ports: Mutex<HashSet<u16>>,
    port: AtomicU16,
}

impl Recording {
    fn session_json(&self, session: &SmtpSession) -> Value {
        let is_id = session.id.len() == 20 && session.id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let remote_port = match self.client_ports.lock().unwrap().contains(&session.remote_port) {
            true => json!("<port>"),
            false => json!(session.remote_port),
        };
        let local_port = match session.local_port == self.port.load(Ordering::SeqCst) {
            true => json!("<port>"),
            false => json!(session.local_port),
        };
        json!({
            "id": if is_id { "<id>" } else { session.id.as_str() },
            "remoteAddress": session.remote_address,
            "remotePort": remote_port,
            "localAddress": session.local_address,
            "localPort": local_port,
            "secure": session.secure,
            "user": session.user,
            "hostNameAppearsAs": session.host_name_appears_as,
            "envelope": {
                "mailFrom": session.envelope.mail_from.as_ref().map_or(Value::Bool(false), address_json),
                "rcptTo": recipients_json(&session.envelope.rcpt_to),
                "bodyType": session.envelope.body_type,
                "smtpUtf8": session.envelope.smtp_utf8,
                "requireTLS": session.envelope.require_tls,
            },
            "transaction": session.transaction,
            "passwordVersion": session.state.get::<PasswordVersion>().map(|version| version.0),
        })
    }

    fn record(&self, call: Value) -> usize {
        let mut calls = self.calls.lock().unwrap();
        calls.push(call);
        calls.len() - 1
    }

    async fn later(&self) {
        self.pending.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(SLOW).await;
        self.pending.fetch_sub(1, Ordering::SeqCst);
    }

    /// Notes how the message of an `onData` call ended.
    fn settle(&self, call: usize, data: Option<&[u8]>, ended: bool) {
        let mut calls = self.calls.lock().unwrap();
        calls[call]["ended"] = json!(ended);
        if let Some(data) = data {
            calls[call]["data"] = data_json(data);
        }
        drop(calls);
        self.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

fn local_part(address: Option<&SmtpAddress>) -> &str {
    address.map_or("", |address| address.address.split('@').next().unwrap_or(""))
}

fn refusal(message: &str, code: u16) -> SmtpHookError {
    SmtpHookError::with_code(code, message)
}

/// `recordingHooks` of `record.ts`: the same answers to the same calls.
struct RecordingHandler(Arc<Recording>);

impl SmtpHandler for RecordingHandler {
    async fn on_auth(
        &self,
        auth: SmtpAuthRequest,
        session: &mut SmtpSession,
    ) -> Result<SmtpAuthResponse, SmtpHookError> {
        self.0.record(json!({
            "hook": "onAuth",
            "method": auth.method.as_str(),
            "username": auth.username,
            "password": auth.password,
            "session": self.0.session_json(session),
        }));
        let mut accept = |user: &str, version: i64| {
            session.state.insert(PasswordVersion(version));
            Ok(SmtpAuthResponse::accept(user))
        };
        if auth.username.starts_with("evict") && auth.password == "pass" {
            return accept(&auth.username, 1);
        }
        match auth.username.as_str() {
            "user" if auth.password == "pass" => accept("user", 3),
            "user" => Ok(SmtpAuthResponse::reject()),
            "alias" => accept("canonical", 7),
            "slow" => {
                self.0.later().await;
                accept("slow", 5)
            }
            "limited" => Err(SmtpHookError::new("Too many failed authentication attempts")),
            "unavailable" => Err(refusal("4.7.0 Temporary authentication failure", 454)),
            "lookup" => Err(refusal("4.3.0 Error: temporary lookup failure", 451)),
            "gone" => Err(refusal("4.4.2 Credential changed, please re-authenticate", 421)),
            "custom" => Ok(SmtpAuthResponse {
                user: None,
                message: Some("5.7.8 Custom refusal".to_string()),
                response_code: Some(534),
            }),
            "blank" => Err(SmtpHookError::new("")),
            _ => Ok(SmtpAuthResponse::reject()),
        }
    }

    async fn on_mail_from(&self, address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.0.record(json!({
            "hook": "onMailFrom",
            "address": address_json(address),
            "session": self.0.session_json(session),
        }));
        match local_part(Some(address)) {
            "refused" => Err(refusal("5.7.1 Sender refused", 550)),
            "nocode" => Err(SmtpHookError::new("Sender refused without a code")),
            "lookup" => Err(refusal("4.3.0 Error: temporary lookup failure", 451)),
            "gone" => Err(refusal("4.4.2 Credential changed, please re-authenticate", 421)),
            "slow" => {
                self.0.later().await;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn on_rcpt_to(&self, address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.0.record(json!({
            "hook": "onRcptTo",
            "address": address_json(address),
            "session": self.0.session_json(session),
        }));
        match local_part(Some(address)) {
            "nobody" => Err(refusal("5.1.1 Error: no such mailbox here", 550)),
            "nocode" => Err(SmtpHookError::new("Recipient refused without a code")),
            "probes" => Err(refusal("4.7.0 Error: too many unknown recipients, try again later", 451)),
            "gone" => Err(refusal("4.4.2 Credential changed, please re-authenticate", 421)),
            "slow" => {
                self.0.later().await;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn on_data(&self, mut stream: SmtpDataStream, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        let call = self.0.record(json!({
            "hook": "onData",
            "session": self.0.session_json(session),
            "data": null,
            "ended": null,
        }));
        let mode = local_part(session.envelope.mail_from.as_ref()).to_string();

        // Refusals that do not wait for the message: the server reads it out.
        match mode.as_str() {
            "relay" => return Err(refusal("Error: relay access denied", 550)),
            "unconfigured" => return Err(SmtpHookError::new("Email service not configured")),
            // Never reads and never answers.
            "ignore" => {
                std::future::pending::<()>().await;
                drop(stream);
                return Ok(());
            }
            _ => {}
        }

        self.0.pending.fetch_add(1, Ordering::SeqCst);
        let mut message = Vec::new();
        let ended = loop {
            match stream.next_chunk().await {
                Ok(Some(chunk)) => {
                    message.extend_from_slice(&chunk);
                    // A ceiling enforced by the hook: refused as soon as it is
                    // crossed. The rest is still watched, to note how it ended.
                    if mode == "toobig" && message.len() > 100 {
                        let recording = self.0.clone();
                        tokio::spawn(async move {
                            let ended = loop {
                                match stream.next_chunk().await {
                                    Ok(Some(_)) => {}
                                    Ok(None) => break true,
                                    Err(_) => break false,
                                }
                            };
                            recording.settle(call, None, ended);
                        });
                        return Err(refusal("Error: message exceeds fixed maximum message size 100", 552));
                    }
                }
                Ok(None) => break true,
                Err(error) => {
                    assert!(SmtpDataStream::is_abandoned(&error), "an unexpected stream error: {error}");
                    break false;
                }
            }
        };
        self.0.settle(call, Some(&message), ended);
        if !ended {
            return Err(SmtpHookError::new("the connection is gone, and this goes nowhere"));
        }
        match mode.as_str() {
            "notstored" => Err(refusal("4.2.1 Error: mailbox unavailable, try again later", 451)),
            "busy" => Err(refusal("4.3.2 Error: too many concurrent deliveries, try again later", 421)),
            "parsefail" => Err(SmtpHookError::new("Parser failed")),
            "slowdata" => {
                self.0.later().await;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// A TLS library that gives up on a handshake may say so with an alert
/// record (`15 03 0x 00 02 ...`) before it closes. Which alert, and whether
/// one is sent at all, is the library's business, so one at the end of what
/// a `closed` step saw is not compared.
fn without_tls_alert(observation: &Value) -> Value {
    let Some(text) = observation.as_str() else {
        return observation.clone();
    };
    let Some(seen) = text.strip_prefix("closed after ") else {
        return observation.clone();
    };
    let Ok(seen) = serde_json::from_str::<String>(seen) else {
        return observation.clone();
    };
    let bytes = bytes_of(&seen);
    let is_alert = bytes.len() == 7 && bytes[0] == 0x15 && bytes[1] == 0x03 && bytes[3..5] == [0x00, 0x02];
    if is_alert { json!("closed") } else { observation.clone() }
}

fn option<'a>(server: &'a Value, key: &str) -> &'a Value {
    server.get(key).unwrap_or_else(|| panic!("the recording names the server's {key}"))
}

struct Replay {
    observed: Vec<Value>,
    calls: Vec<Value>,
    errors: Vec<Value>,
}

async fn replay(conversation: &Value) -> Replay {
    let spec = &conversation["server"];
    let recording = Arc::new(Recording::default());
    let server = create_smtp_server(SmtpServerOptions {
        secure: option(spec, "secure").as_bool().unwrap(),
        proxy_protocol: option(spec, "proxyProtocol").as_bool().unwrap(),
        allow_insecure_auth: option(spec, "allowInsecureAuth").as_bool().unwrap(),
        name: option(spec, "name").as_str().map(str::to_string),
        tls: option(spec, "tls").as_bool().unwrap().then(tls_source),
        size: option(spec, "size").as_u64().unwrap(),
        max_clients: option(spec, "maxClients").as_u64().unwrap() as usize,
        close_timeout: Duration::from_millis(option(spec, "closeTimeout").as_u64().unwrap()),
        socket_timeout: option(spec, "socketTimeout").as_u64().map(Duration::from_millis),
        handler: SharedSmtpHandler::new(RecordingHandler(recording.clone())),
    });
    let errors = Arc::new(Mutex::new(Vec::new()));
    let reported = errors.clone();
    server.on_error(move |error| {
        let meta = error.meta.as_ref();
        reported.lock().unwrap().push(json!({
            "stage": meta.map(|meta| meta.stage),
            "remoteAddress": meta.and_then(|meta| meta.remote_address.clone()).or(error.remote_address.clone()),
        }));
    });
    let port = server.listen(0).await.expect("a free port").port();
    recording.port.store(port, Ordering::SeqCst);

    let mut clients: HashMap<String, Client> = HashMap::new();
    let mut registered = false;
    let mut closing: Option<tokio::task::JoinHandle<()>> = None;
    let mut observed = Vec::new();

    for step in conversation["steps"].as_array().expect("steps") {
        let name = step.get("on").and_then(Value::as_str).unwrap_or("c").to_string();
        let within = |default: u64| Duration::from_millis(step.get("within").and_then(Value::as_u64).unwrap_or(default));
        let mut seen = Value::Null;
        if let Some(name) = step.get("open").and_then(Value::as_str) {
            let client = Client::connect(port).await;
            recording.client_ports.lock().unwrap().insert(client.local_port);
            clients.insert(name.to_string(), client);
        } else if let Some(data) = step.get("send").and_then(Value::as_str) {
            let times = step.get("times").and_then(Value::as_u64).unwrap_or(1) as usize;
            clients.get_mut(&name).expect("an open connection").send(&bytes_of(data).repeat(times)).await;
        } else if step.get("reply").is_some() {
            seen = json!(clients.get_mut(&name).expect("an open connection").reply(within(3000)).await);
        } else if let Some(count) = step.get("replies").and_then(Value::as_u64) {
            let client = clients.get_mut(&name).expect("an open connection");
            seen = json!(client.replies(count as usize, within(20000)).await);
        } else if step.get("closed").is_some() {
            seen = json!(clients.get_mut(&name).expect("an open connection").closed(within(3000)).await);
        } else if let Some(ms) = step.get("silence").and_then(Value::as_u64) {
            let client = clients.get_mut(&name).expect("an open connection");
            seen = json!(client.silence(Duration::from_millis(ms)).await);
        } else if step.get("tls").is_some() {
            seen = json!(clients.get_mut(&name).expect("an open connection").start_tls().await);
        } else if step.get("end").is_some() {
            clients.get_mut(&name).expect("an open connection").end().await;
        } else if step.get("destroy").is_some() {
            clients.get_mut(&name).expect("an open connection").destroy();
        } else if let Some(ms) = step.get("wait").and_then(Value::as_u64) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        } else if let Some(username) = step.get("evict").and_then(Value::as_str) {
            if !registered {
                register_smtp_server(&server);
                registered = true;
            }
            seen = json!(evict_smtp_connections(username));
        } else if step.get("server").and_then(Value::as_str) == Some("close") {
            closing = Some(tokio::spawn(server.close()));
        } else if step.get("server").and_then(Value::as_str) == Some("closed") {
            let deadline = Instant::now() + within(3000);
            let closing = closing.as_ref().expect("the server is closing");
            while !closing.is_finished() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            seen = json!(if closing.is_finished() { "closed" } else { "open" });
        } else {
            panic!("an unknown step: {step}");
        }
        observed.push(seen);
    }

    // The conversation is over: every client goes, and the server is given
    // the time to notice and its hooks the time to finish.
    for client in clients.values_mut() {
        client.destroy();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while (server.connection_count() > 0 || recording.pending.load(Ordering::SeqCst) > 0) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    if registered {
        unregister_smtp_server(&server);
    }
    match closing {
        Some(closing) => closing.await.expect("close does not panic"),
        None => server.close().await,
    }

    if spec["name"].is_null() {
        for seen in observed.iter_mut() {
            if let Some(text) = seen.as_str() {
                *seen = json!(text.replace(server.name(), "<hostname>"));
            }
        }
    }
    let calls = recording.calls.lock().unwrap().clone();
    let errors = errors.lock().unwrap().clone();
    Replay { observed, calls, errors }
}

/// Everything in which a replay differs from its recording, in words.
fn differences(conversation: &Value, replayed: &Replay) -> Vec<String> {
    let mut found = Vec::new();
    let steps = conversation["steps"].as_array().expect("steps");
    let expected = conversation["observed"].as_array().expect("observed");
    for (index, (expected, actual)) in expected.iter().zip(&replayed.observed).enumerate() {
        if without_tls_alert(expected) != without_tls_alert(actual) {
            found.push(format!("step {index} {}\n      Bun:  {expected}\n      Rust: {actual}", steps[index]));
        }
    }
    let calls = conversation["calls"].as_array().expect("calls");
    for index in 0..calls.len().max(replayed.calls.len()) {
        let (expected, actual) = (calls.get(index), replayed.calls.get(index));
        if expected != actual {
            let show = |call: Option<&Value>| call.map_or("(no call)".to_string(), Value::to_string);
            found.push(format!("hook call {index}\n      Bun:  {}\n      Rust: {}", show(expected), show(actual)));
        }
    }
    if let Some(errors) = conversation.get("errors").and_then(Value::as_array) {
        if errors != &replayed.errors {
            found.push(format!(
                "reported errors\n      Bun:  {}\n      Rust: {}",
                Value::Array(errors.clone()),
                Value::Array(replayed.errors.clone())
            ));
        }
    }
    found
}

/// Conversations whose timing a busy machine would stretch are replayed on
/// their own; the rest a few at a time.
fn is_heavy(conversation: &Value) -> bool {
    let name = conversation["name"].as_str().unwrap_or("");
    name.contains("flood") || name.contains("thousand") || name.contains("large message")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conversations_match_the_bun_server() {
    let recorded = golden("conversations.json");
    let recorded = recorded.as_array().expect("an array of conversations");
    assert!(recorded.len() >= 150, "the recording holds {} conversations", recorded.len());
    let only = std::env::var("SMTP_GOLDEN_ONLY").ok();
    let selected: Vec<&Value> = recorded
        .iter()
        .filter(|conversation| only.as_ref().is_none_or(|only| conversation["name"].as_str().unwrap().contains(only)))
        .collect();

    let mut failures = Vec::new();
    let (heavy, light): (Vec<&Value>, Vec<&Value>) = selected.into_iter().partition(|c| is_heavy(c));
    for batch in light.chunks(6) {
        let replays = futures_util::future::join_all(batch.iter().map(|conversation| replay(conversation))).await;
        for (conversation, replayed) in batch.iter().zip(&replays) {
            let found = differences(conversation, replayed);
            if !found.is_empty() {
                failures.push(format!("  {}\n    {}", conversation["name"], found.join("\n    ")));
            }
        }
    }
    for conversation in heavy {
        let found = differences(conversation, &replay(conversation).await);
        if !found.is_empty() {
            failures.push(format!("  {}\n    {}", conversation["name"], found.join("\n    ")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} conversations differ from the Bun server:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn base64_decodes_as_buffer_from_does() {
    let table = golden("base64.json");
    let table = table.as_array().expect("an array of inputs");
    assert!(table.len() > 1000);
    for entry in table {
        let input = entry["input"].as_str().expect("input");
        assert_eq!(hex::encode(decode_base64_lenient(input)), entry["hex"].as_str().expect("hex"), "{input:?}");
    }
}

#[test]
fn address_commands_parse_as_in_the_bun_server() {
    let table = golden("address-command.json");
    let table = table.as_array().expect("an array of lines");
    assert!(table.len() > 1000);
    let mut failures = Vec::new();
    for entry in table {
        let (verb, line) = (entry["verb"].as_str().expect("verb"), entry["line"].as_str().expect("line"));
        let parsed = parse_address_command(verb, line).as_ref().map_or(Value::Bool(false), address_json);
        if parsed != entry["parsed"] {
            failures.push(format!("  {verb:?} {line:?}\n    Bun:  {}\n    Rust: {parsed}", entry["parsed"]));
        }
    }
    assert!(failures.is_empty(), "{} lines parse differently:\n{}", failures.len(), failures.join("\n"));
}
