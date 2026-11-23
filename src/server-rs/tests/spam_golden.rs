//! Holds the spam filter to the Bun server's answers.
//!
//! The fixtures in `tests/golden/spam` are written by `generate.ts` there,
//! which runs the Bun server's own spam code over a fixed corpus: unicode,
//! HTML, URLs, long words, every rule trigger and its near misses, corrupted
//! training counts. Each test below feeds the same inputs to this crate and
//! requires the same output. A difference means this crate is wrong.
//!
//! Regenerate with `bun src/server-rs/tests/golden/spam/generate.ts` from the
//! repository root. `resolver.json` has a generator of its own, `resolver.ts`:
//! it scripts a name server for Bun's resolver, and the last test here
//! replays that script to this crate's.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::future::{join_all, BoxFuture};
use serde_json::{json, Value};
use tokio::net::UdpSocket;

use mail_server::postgres::repositories::spam_training::{DocCounts, WordCounts};
use mail_server::spam::alignment::is_envelope_aligned;
use mail_server::spam::classifier::{
    classify_email_using, extract_tokens, tokenize, ClassifierDeps, ClassifierResult,
};
use mail_server::spam::dnsbl::{
    aggregate_verdicts, check_dnsbls_using, default_dnsbls, is_real_listing, to_ipv4, verdict_for_answer,
    verdict_for_dns_error, DnsLookup, DnsLookupError, DnsblAnswer, DnsblResult, DnsblVerdict,
    SystemDnsLookup,
};
use mail_server::spam::rules::{default_rules, evaluate_rules};
use mail_server::spam::service::{check_spam_using, CheckSpamDeps};
use mail_server::spam::types::{DnsBlocklist, EmailContext, SpamFilterConfig};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/spam").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn cases(fixture: &Value) -> &[Value] {
    fixture["cases"].as_array().expect("the fixture has cases")
}

fn email(value: &Value) -> EmailContext {
    serde_json::from_value(value.clone()).expect("the email deserializes")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("an array")
        .iter()
        .map(|item| item.as_str().expect("a string").to_string())
        .collect()
}

/// Collects every difference, so one run shows how far off the port is
/// rather than the first case only.
#[derive(Default)]
struct Differences {
    compared: usize,
    found: Vec<String>,
}

impl Differences {
    fn check<T: PartialEq + std::fmt::Debug>(&mut self, case: &dyn std::fmt::Display, rust: T, bun: T) {
        self.compared += 1;
        if rust != bun {
            let case: String = case.to_string().chars().take(600).collect();
            self.found.push(format!("case {case}\n   rust: {rust:?}\n   bun:  {bun:?}"));
        }
    }

    fn finish(self, minimum: usize) {
        assert!(
            self.found.is_empty(),
            "{} of {} comparisons differ from the Bun server; the first few:\n{}",
            self.found.len(),
            self.compared,
            self.found.iter().take(8).cloned().collect::<Vec<_>>().join("\n")
        );
        assert!(
            self.compared >= minimum,
            "only {} comparisons ran, expected at least {minimum}",
            self.compared
        );
    }
}

/// `str::to_lowercase` is what stands in for `String.prototype.toLowerCase`
/// in tokens, rule matching, alignment and stored allowlist patterns. The two
/// must agree on every character, and on the forms that depend on context.
#[test]
fn lowercasing_is_javascripts() {
    let fixture = fixture("lowercase.json");
    let mut expected: HashMap<u32, String> = HashMap::new();
    for mapping in fixture["mappings"].as_array().unwrap() {
        let lowered: String = mapping[1]
            .as_array()
            .unwrap()
            .iter()
            .map(|cp| char::from_u32(cp.as_u64().unwrap() as u32).unwrap())
            .collect();
        expected.insert(mapping[0].as_u64().unwrap() as u32, lowered);
    }
    assert!(expected.len() > 1400);

    let mut differences = Differences::default();
    for code_point in 0..=0x10FFFFu32 {
        let Some(character) = char::from_u32(code_point) else { continue };
        let source = character.to_string();
        let bun = expected.get(&code_point).cloned().unwrap_or_else(|| source.clone());
        differences.check(&format!("U+{code_point:04X}"), source.to_lowercase(), bun);
    }
    for context in fixture["contexts"].as_array().unwrap() {
        let input = context[0].as_str().unwrap();
        differences.check(&json!(input), input.to_lowercase(), context[1].as_str().unwrap().to_string());
    }
    differences.finish(1_112_064);
}

#[test]
fn tokenize_matches_the_bun_server() {
    let fixture = fixture("tokenize.json");
    let mut differences = Differences::default();
    for case in cases(&fixture) {
        let input = case["input"].as_str().unwrap();
        differences.check(&json!(input), tokenize(input), strings(&case["tokens"]));
    }
    differences.finish(600);
}

#[test]
fn extract_tokens_matches_the_bun_server() {
    let fixture = fixture("extract_tokens.json");
    let mut differences = Differences::default();
    for case in cases(&fixture) {
        differences.check(&case["email"], extract_tokens(&email(&case["email"])), strings(&case["tokens"]));
    }
    differences.finish(300);
}

#[test]
fn default_rules_are_the_bun_servers() {
    let fixture = fixture("rules.json");
    let rust: Vec<Value> = default_rules()
        .iter()
        .map(|rule| json!({ "id": rule.id, "name": rule.name, "score": rule.score }))
        .collect();
    assert_eq!(json!(rust), fixture["defaultRules"]);
}

#[test]
fn evaluate_rules_matches_the_bun_server() {
    let fixture = fixture("rules.json");
    let mut differences = Differences::default();
    for case in cases(&fixture) {
        let result = evaluate_rules(&email(&case["email"]), default_rules());
        let matched: Vec<String> = result.matched_rules.iter().map(|rule| rule.id.clone()).collect();
        differences.check(
            &case["email"],
            (result.score, matched),
            (case["score"].as_i64().unwrap(), strings(&case["matched"])),
        );
    }
    differences.finish(2000);
}

#[test]
fn alignment_matches_the_bun_server() {
    let fixture = fixture("alignment.json");
    let mut differences = Differences::default();
    for case in cases(&fixture) {
        let aligned = is_envelope_aligned(case["header"].as_str(), case["envelope"].as_str());
        differences.check(case, aligned, case["aligned"].as_bool().unwrap());
    }
    differences.finish(1500);
}

/// Answers the classifier from a fixture case; a `null` lookup throws.
struct ClassifierStub {
    doc_counts: Option<DocCounts>,
    word_counts: Option<HashMap<String, WordCounts>>,
}

impl ClassifierStub {
    fn from_case(case: &Value) -> ClassifierStub {
        let count = |value: &Value| value.as_i64().expect("an integer count");
        ClassifierStub {
            doc_counts: case["docCounts"].as_object().map(|counts| DocCounts {
                spam_docs: count(&counts["spamDocs"]),
                ham_docs: count(&counts["hamDocs"]),
            }),
            word_counts: case["wordCounts"].as_object().map(|words| {
                words
                    .iter()
                    .map(|(word, counts)| {
                        let counts = WordCounts {
                            spam_count: count(&counts["spamCount"]),
                            ham_count: count(&counts["hamCount"]),
                        };
                        (word.clone(), counts)
                    })
                    .collect()
            }),
        }
    }
}

impl ClassifierDeps for ClassifierStub {
    fn get_classifier_doc_counts<'a>(&'a self, _user_id: &'a str) -> BoxFuture<'a, Result<DocCounts>> {
        Box::pin(async move { self.doc_counts.ok_or_else(|| anyhow!("DB error")) })
    }

    fn get_word_counts<'a>(
        &'a self,
        _user_id: &'a str,
        _words: &'a [String],
    ) -> BoxFuture<'a, Result<HashMap<String, WordCounts>>> {
        Box::pin(async move { self.word_counts.clone().ok_or_else(|| anyhow!("DB error")) })
    }
}

/// A score as the fixture spells it: a number, or the string `NaN`.
fn spelled(score: f64) -> Value {
    if score.is_nan() {
        json!("NaN")
    } else {
        json!(score)
    }
}

#[tokio::test]
async fn classify_email_matches_the_bun_server() {
    let fixture = fixture("classifier.json");
    let emails: Vec<EmailContext> = fixture["emails"].as_array().unwrap().iter().map(email).collect();
    let mut differences = Differences::default();
    let (mut spam_verdicts, mut not_a_number) = (0, 0);
    for case in cases(&fixture) {
        let email = &emails[case["email"].as_u64().unwrap() as usize];
        let result = classify_email_using("user", email, &ClassifierStub::from_case(case)).await;
        spam_verdicts += usize::from(result.reason.is_some());
        not_a_number += usize::from(result.score.is_nan());
        let bun_score = match &case["result"]["score"] {
            Value::String(text) if text == "NaN" => json!("NaN"),
            score => json!(score.as_f64().expect("a numeric score")),
        };
        differences.check(
            case,
            (spelled(result.score), result.reason),
            (bun_score, case["result"]["reason"].as_str().map(str::to_string)),
        );
    }
    // The corpus reaches both verdicts and the corrupted-count arithmetic.
    assert!(spam_verdicts > 100, "{spam_verdicts}");
    assert!(not_a_number > 20, "{not_a_number}");
    differences.finish(1000);
}

/// Answers a spam check's lookups from a fixture case; `"throw"` fails.
struct ServiceStub<'c> {
    deps: &'c Value,
}

impl CheckSpamDeps for ServiceStub<'_> {
    fn is_allowlisted<'a>(&'a self, _user_id: &'a str, _address: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(
            async move { self.deps["allowlisted"].as_bool().ok_or_else(|| anyhow!("allowlist unreachable")) },
        )
    }

    fn classify_email<'a>(
        &'a self,
        _user_id: &'a str,
        _email: &'a EmailContext,
    ) -> BoxFuture<'a, Result<ClassifierResult>> {
        Box::pin(async move {
            let stub =
                self.deps["classifier"].as_object().ok_or_else(|| anyhow!("classifier unreachable"))?;
            Ok(ClassifierResult {
                score: stub["score"].as_f64().expect("a score"),
                reason: stub["reason"].as_str().map(str::to_string),
            })
        })
    }

    fn check_dnsbls<'a>(
        &'a self,
        _ip: &'a str,
        _dnsbls: &'a [DnsBlocklist],
    ) -> BoxFuture<'a, Result<DnsblResult>> {
        Box::pin(async move {
            let stub = self.deps["dnsbl"].as_object().ok_or_else(|| anyhow!("resolver unreachable"))?;
            Ok(DnsblResult {
                score: stub["score"].as_i64().expect("a score"),
                listed_in: Vec::new(),
                reasons: strings(&stub["reasons"]),
                evaluated: stub["evaluated"].as_bool().expect("evaluated"),
            })
        })
    }
}

/// The default configuration with a case's overrides applied, as
/// `{ ...DEFAULT_CONFIG, ...config }` does.
fn config_with(overrides: &Value) -> SpamFilterConfig {
    let mut config = SpamFilterConfig::default();
    for (key, value) in overrides.as_object().expect("config overrides") {
        match key.as_str() {
            "spamThreshold" => config.spam_threshold = value.as_i64().unwrap(),
            "enableDnsbl" => config.enable_dnsbl = value.as_bool().unwrap(),
            "enableRules" => config.enable_rules = value.as_bool().unwrap(),
            "dnsbls" => config.dnsbls = serde_json::from_value(value.clone()).unwrap(),
            other => panic!("unknown config key {other}"),
        }
    }
    config
}

#[tokio::test]
async fn check_spam_matches_the_bun_server() {
    let fixture = fixture("service.json");
    let emails: Vec<EmailContext> = fixture["emails"].as_array().unwrap().iter().map(email).collect();
    let mut differences = Differences::default();
    for case in cases(&fixture) {
        let email = &emails[case["email"].as_u64().unwrap() as usize];
        let stub = ServiceStub { deps: &case["deps"] };
        let result = check_spam_using("user", email, &config_with(&case["config"]), &stub).await;
        differences.check(case, serde_json::to_value(&result).unwrap(), case["result"].clone());
    }
    differences.finish(900);
}

/// The fixture's blocklist cases need no network; a lookup would be a bug.
struct NoNetwork;

impl DnsLookup for NoNetwork {
    fn resolve4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Vec<String>, DnsLookupError>> {
        panic!("the golden corpus asked the network for {name}");
    }
}

fn verdict(value: &Value) -> DnsblVerdict {
    match value.as_str().expect("a verdict") {
        "listed" => DnsblVerdict::Listed,
        "clean" => DnsblVerdict::Clean,
        "unknown" => DnsblVerdict::Unknown,
        other => panic!("unknown verdict {other}"),
    }
}

#[tokio::test]
async fn dnsbl_matches_the_bun_server() {
    let fixture = fixture("dnsbl.json");
    assert_eq!(serde_json::to_value(default_dnsbls()).unwrap(), fixture["defaultDnsbls"]);

    let mut differences = Differences::default();
    for case in cases(&fixture) {
        match case["kind"].as_str().unwrap() {
            "ip" => {
                let ip = case["ip"].as_str().unwrap();
                differences.check(case, to_ipv4(ip), case["toIpv4"].as_str());
                let dnsbls = if case["usedDefaults"].as_bool().unwrap() { default_dnsbls() } else { &[] };
                let result = check_dnsbls_using(ip, dnsbls, &NoNetwork).await;
                differences.check(case, serde_json::to_value(&result).unwrap(), case["checkDnsbls"].clone());
            }
            "answer" => {
                let addresses = strings(&case["addresses"]);
                differences.check(
                    case,
                    is_real_listing(&addresses),
                    case["isRealListing"].as_bool().unwrap(),
                );
                differences.check(case, verdict_for_answer(&addresses), verdict(&case["verdict"]));
            }
            "error" => {
                differences.check(
                    case,
                    verdict_for_dns_error(case["code"].as_str()),
                    verdict(&case["verdict"]),
                );
            }
            "aggregate" => {
                let answers: Vec<DnsblAnswer> = case["answers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|answer| match answer["status"].as_str().unwrap() {
                        "fulfilled" => DnsblAnswer::Fulfilled {
                            dnsbl: serde_json::from_value(answer["dnsbl"].clone()).unwrap(),
                            verdict: verdict(&answer["verdict"]),
                        },
                        _ => DnsblAnswer::Rejected { reason: "query failed".into() },
                    })
                    .collect();
                let result = aggregate_verdicts(&answers);
                differences.check(case, serde_json::to_value(&result).unwrap(), case["result"].clone());
            }
            other => panic!("unknown kind {other}"),
        }
    }
    differences.finish(350);
}

/// A name server `resolver.ts` scripts for Bun's resolver, on a loopback UDP
/// port. Each blocklist host under the reversed address has one answer, or
/// one per question in turn: a response code with A records and perhaps an
/// SOA record, sent after its delay if it has one, or `null` for no reply at
/// all. Any other name does not exist.
struct ScriptedNameServer {
    address: SocketAddr,
    /// Questions received so far, by blocklist host.
    asked: Arc<Mutex<HashMap<String, usize>>>,
}

fn wire_name(name: &str) -> Vec<u8> {
    let mut wire = Vec::new();
    for label in name.split('.') {
        wire.push(label.len() as u8);
        wire.extend_from_slice(label.as_bytes());
    }
    wire.push(0);
    wire
}

impl ScriptedNameServer {
    async fn start(script: Value, reversed: String) -> ScriptedNameServer {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("a loopback port"));
        let address = socket.local_addr().expect("the bound address");
        let asked: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let counts = asked.clone();
        tokio::spawn(async move {
            let mut buffer = [0u8; 1500];
            loop {
                let Ok((length, peer)) = socket.recv_from(&mut buffer).await else { return };
                let query = &buffer[..length];
                // The question follows the 12-byte header: labels, a zero, type and class.
                let mut labels = Vec::new();
                let mut at = 12;
                while at < query.len() && query[at] != 0 {
                    let size = query[at] as usize;
                    labels.push(String::from_utf8_lossy(&query[at + 1..at + 1 + size]).to_lowercase());
                    at += 1 + size;
                }
                let name = labels.join(".");
                let host = name.strip_prefix(&format!("{reversed}.")).unwrap_or("").to_string();
                let turn = {
                    let mut counts = counts.lock().unwrap();
                    let count = counts.entry(host.clone()).or_insert(0);
                    *count += 1;
                    *count - 1
                };
                let answer = match script.get(&host) {
                    Some(Value::Array(turns)) => turns[turn.min(turns.len() - 1)].clone(),
                    Some(answer) => answer.clone(),
                    None => json!({ "rcode": 3, "records": [] }),
                };
                if answer.is_null() {
                    continue;
                }

                let records = strings(&answer["records"]);
                let soa = answer.get("soa");
                let mut response = query[..2].to_vec();
                response.extend_from_slice(&[0x81, 0x80 | answer["rcode"].as_u64().unwrap() as u8, 0, 1]);
                response.extend_from_slice(&(records.len() as u16).to_be_bytes());
                response.extend_from_slice(&[0, u8::from(soa.is_some()), 0, 0]);
                response.extend_from_slice(&query[12..at + 5]);
                let ttl = answer["ttl"].as_u64().unwrap_or(60) as u32;
                for record in records {
                    response.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1]);
                    response.extend_from_slice(&ttl.to_be_bytes());
                    response.extend_from_slice(&[0, 4]);
                    response.extend(record.split('.').map(|octet| octet.parse::<u8>().unwrap()));
                }
                if let Some(soa) = soa {
                    let mut rdata = [wire_name("ns.test"), wire_name("hostmaster.test")].concat();
                    for value in [1, 3600, 600, 86_400, soa["minimum"].as_u64().unwrap() as u32] {
                        rdata.extend_from_slice(&value.to_be_bytes());
                    }
                    response.extend(wire_name("test"));
                    response.extend_from_slice(&[0, 6, 0, 1]);
                    response.extend_from_slice(&(soa["ttl"].as_u64().unwrap() as u32).to_be_bytes());
                    response.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
                    response.extend(rdata);
                }

                let delay = Duration::from_millis(answer["delayMs"].as_u64().unwrap_or(0));
                let socket = socket.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = socket.send_to(&response, peer).await;
                });
            }
        });
        ScriptedNameServer { address, asked }
    }

    /// Questions received so far about a blocklist host, however it was spelled.
    fn asked(&self, hostname: &str) -> usize {
        let host = hostname.to_lowercase();
        self.asked.lock().unwrap().get(host.trim_end_matches('.')).copied().unwrap_or(0)
    }
}

fn reversed(ip: &str) -> String {
    ip.split('.').rev().collect::<Vec<_>>().join(".")
}

/// The network path of the blocklist check: what this crate's resolver makes
/// of listings, warnings, missing names, failures, refusals, silence, answers
/// on either side of the timeout and names that cannot be asked, against what
/// Bun's resolver made of the same name server.
#[tokio::test]
async fn the_resolver_path_matches_the_bun_server() {
    let fixture = fixture("resolver.json");
    let ip = fixture["ip"].as_str().unwrap();
    let name_server = ScriptedNameServer::start(fixture["script"].clone(), reversed(ip)).await;
    let lookup = SystemDnsLookup::with_name_servers(&[name_server.address]);

    // Together, as the generator runs them: the cases that meet silence each
    // wait out the two-second timeout.
    let results = join_all(cases(&fixture).iter().map(|case| {
        let lookup = &lookup;
        async move {
            let dnsbls: Vec<DnsBlocklist> = serde_json::from_value(case["dnsbls"].clone()).unwrap();
            serde_json::to_value(check_dnsbls_using(ip, &dnsbls, lookup).await).unwrap()
        }
    }))
    .await;

    let mut differences = Differences::default();
    for (case, rust) in cases(&fixture).iter().zip(results) {
        differences.check(&case["dnsbls"], rust, case["result"].clone());
    }
    differences.finish(25);
}

/// What the resolver carries from one question to the next: answers it
/// keeps, a question it repeats, a name server it stops asking once another
/// has to answer for it. Each step is one check; the verdict and how often
/// each name server was asked must be what Bun's resolver did.
#[tokio::test]
async fn the_resolver_remembers_what_the_bun_servers_does() {
    let fixture = fixture("resolver.json");
    let ip = fixture["ip"].as_str().unwrap();
    let sequences = fixture["sequences"].as_array().expect("sequences");
    assert_eq!(sequences.len(), 3);

    // The sequences are independent of each other: each has name servers and a resolver of its own.
    let replayed = join_all(sequences.iter().map(|sequence| async move {
        let mut servers = Vec::new();
        for script in sequence["servers"].as_array().unwrap() {
            servers.push(ScriptedNameServer::start(script.clone(), reversed(ip)).await);
        }
        let addresses: Vec<SocketAddr> = servers.iter().map(|server| server.address).collect();
        let lookup = SystemDnsLookup::with_name_servers(&addresses);

        let mut steps = Vec::new();
        for step in sequence["steps"].as_array().unwrap() {
            let hostname = step["hostname"].as_str().unwrap();
            let before: Vec<usize> = servers.iter().map(|server| server.asked(hostname)).collect();
            let result = check_dnsbls_using(ip, &[DnsBlocklist::new(hostname, hostname, 40)], &lookup).await;
            // What the resolver still does once the check has its answer counts too.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let asked: Vec<usize> =
                servers.iter().zip(before).map(|(server, before)| server.asked(hostname) - before).collect();
            steps.push(json!({ "hostname": hostname, "result": result, "asked": asked }));
        }
        steps
    }))
    .await;

    let mut differences = Differences::default();
    for (sequence, steps) in sequences.iter().zip(replayed) {
        for (bun, rust) in sequence["steps"].as_array().unwrap().iter().zip(steps) {
            let step = format!("{}: {}", sequence["name"], bun["hostname"]);
            differences.check(&step, rust, bun.clone());
        }
    }
    differences.finish(25);
}
