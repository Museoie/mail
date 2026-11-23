//! DNS blocklist lookups (spam/dnsbl.ts).
//!
//! Layer 1: the connecting address is looked up in DNS blocklists. A listing
//! marks a known spam source.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use futures_util::future::{join_all, BoxFuture};
use hickory_resolver::config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts};
use hickory_resolver::error::{ResolveError, ResolveErrorKind};
use hickory_resolver::proto::error::ProtoErrorKind;
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::{Name, TokioAsyncResolver};
use serde::Serialize;

use crate::spam::types::DnsBlocklist;

/// Timeout for one blocklist query, in milliseconds.
const DNSBL_TIMEOUT_MS: u64 = 2000;

static DEFAULT_DNSBLS: LazyLock<Vec<DnsBlocklist>> = LazyLock::new(|| {
    vec![
        DnsBlocklist::new("zen.spamhaus.org", "Spamhaus ZEN", 40),
        DnsBlocklist::new("bl.spamcop.net", "SpamCop", 30),
        DnsBlocklist::new("b.barracudacentral.org", "Barracuda", 25),
    ]
});

/// The DNS blocklists asked by default (`DEFAULT_DNSBLS`): reputable and
/// widely used.
pub fn default_dnsbls() -> &'static [DnsBlocklist] {
    &DEFAULT_DNSBLS
}

/// Prefix a dual-stack listener puts in front of an IPv4 peer address.
const IPV4_MAPPED_PREFIX: &str = "::ffff:";

/// `/^\d+$/.test(part) && parseInt(part) <= 255`: decimal digits only, of any
/// length, so `007` and `0000255` are octets too.
fn is_octet(part: &str) -> bool {
    if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let significant = part.trim_start_matches('0');
    significant.is_empty()
        || (significant.len() <= 3 && significant.parse::<u16>().is_ok_and(|value| value <= 255))
}

/// The dotted quad an address denotes, or `None` if it denotes none.
///
/// A listener bound without a host reports an IPv4 peer in the mapped form
/// `::ffff:198.51.100.7`, which no blocklist answers for. Unwrapping it is what
/// lets one host get the same verdict over either socket family.
///
/// The quad is returned as written, leading zeros included.
///
/// ```ignore
/// to_ipv4("::ffff:198.51.100.7"); // Some("198.51.100.7")
/// to_ipv4("2001:db8::1");         // None
/// ```
pub fn to_ipv4(ip: &str) -> Option<&str> {
    let mapped = ip
        .get(..IPV4_MAPPED_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(IPV4_MAPPED_PREFIX));
    let quad = if mapped { &ip[IPV4_MAPPED_PREFIX.len()..] } else { ip };
    let mut parts = 0;
    for part in quad.split('.') {
        parts += 1;
        if !is_octet(part) {
            return None;
        }
    }
    (parts == 4).then_some(quad)
}

/// Reverses a dotted quad for a blocklist query: `1.2.3.4` becomes `4.3.2.1`.
fn reverse_ip(ipv4: &str) -> String {
    let mut parts: Vec<&str> = ipv4.split('.').collect();
    parts.reverse();
    parts.join(".")
}

/// Whether a dotted quad is a private or local address, which no blocklist
/// carries: 127/8, 10/8, 172.16/12, 192.168/16 and link-local 169.254/16.
fn is_private_ip(ipv4: &str) -> bool {
    let parts: Vec<u32> =
        ipv4.split('.').map(|part| part.trim_start_matches('0').parse().unwrap_or(0)).collect();
    let (first, second) = (parts.first().copied().unwrap_or(0), parts.get(1).copied().unwrap_or(0));

    first == 127
        || first == 10
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 168)
        || (first == 169 && second == 254)
}

/// Whether a blocklist response represents a real listing.
///
/// Major blocklists (Spamhaus, SpamCop, Barracuda) encode genuine listings as
/// `127.0.0.X` records. Anything in the `127.255.X.X` range is a warning that
/// the query was rejected — most commonly Spamhaus's open-resolver response,
/// returned when queries arrive via a public resolver. Treating those warnings
/// as listings flags every incoming mail as spam.
///
/// Reference: <https://www.spamhaus.org/zen/> (Return Codes).
pub fn is_real_listing<S: AsRef<str>>(addresses: &[S]) -> bool {
    addresses.iter().any(|address| address.as_ref().starts_with("127.0."))
}

/// What one blocklist said about an address: it listed it, cleared it, or never answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsblVerdict {
    Listed,
    Clean,
    Unknown,
}

impl DnsblVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            DnsblVerdict::Listed => "listed",
            DnsblVerdict::Clean => "clean",
            DnsblVerdict::Unknown => "unknown",
        }
    }
}

/// DNS failures that mean "no such record" — how a blocklist spells "not listed".
const NOT_LISTED_DNS_CODES: [&str; 4] = ["ENOTFOUND", "ENODATA", "NOTFOUND", "NODATA"];

/// The verdict a blocklist's successful response carries.
///
/// A record came back, so the address is either listed or the response is one of
/// the warning codes [`is_real_listing`] rejects — a query the blocklist declined
/// to answer, which is not a clearance.
///
/// ```ignore
/// verdict_for_answer(&["127.0.0.2"]);       // Listed
/// verdict_for_answer(&["127.255.255.254"]); // Unknown — open-resolver warning
/// ```
pub fn verdict_for_answer<S: AsRef<str>>(addresses: &[S]) -> DnsblVerdict {
    if is_real_listing(addresses) {
        DnsblVerdict::Listed
    } else {
        DnsblVerdict::Unknown
    }
}

/// The verdict a failed blocklist query carries.
///
/// Only the no-such-record codes mean "not listed". A timeout carries no code,
/// and a refusal or server failure describes the resolver rather than the
/// address, so neither clears it.
///
/// ```ignore
/// verdict_for_dns_error(Some("ENOTFOUND")); // Clean
/// verdict_for_dns_error(Some("EREFUSED"));  // Unknown
/// verdict_for_dns_error(None);              // Unknown — the timeout path
/// ```
pub fn verdict_for_dns_error(code: Option<&str>) -> DnsblVerdict {
    match code {
        Some(code) if NOT_LISTED_DNS_CODES.contains(&code) => DnsblVerdict::Clean,
        _ => DnsblVerdict::Unknown,
    }
}

/// A failed DNS query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsLookupError {
    /// The code Node's resolver reports for this failure, such as `ENOTFOUND`
    /// or `ESERVFAIL`; `None` when the failure has none.
    pub code: Option<String>,
    pub message: String,
}

impl DnsLookupError {
    pub fn new(code: Option<&str>, message: impl Into<String>) -> DnsLookupError {
        DnsLookupError { code: code.map(str::to_string), message: message.into() }
    }
}

impl std::fmt::Display for DnsLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DnsLookupError {}

/// The DNS query a blocklist check makes: `dns.promises.resolve4`. Implemented
/// by [`SystemDnsLookup`] for the network and by a test for canned answers.
///
/// ```ignore
/// struct Listed;
/// impl DnsLookup for Listed {
///     fn resolve4<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, Result<Vec<String>, DnsLookupError>> {
///         Box::pin(async { Ok(vec!["127.0.0.2".to_string()]) })
///     }
/// }
/// let result = check_dnsbls_using("198.51.100.7", default_dnsbls(), &Listed).await; // score 95
/// ```
pub trait DnsLookup: Send + Sync {
    /// The A records of `name` as dotted quads, or why there are none.
    fn resolve4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Vec<String>, DnsLookupError>>;
}

// ---------------------------------------------------------------------------
// The system resolver
//
// A verdict depends on more than the DNS answer: on which name server was
// asked, on how long it was given, on whether an earlier answer was reused.
// The Bun server leaves all of that to its runtime's resolver (c-ares, as Bun
// builds and drives it), so that resolver's conduct is reproduced here.
// `tests/golden/spam/resolver.ts` records it against scripted name servers
// and `tests/spam_golden.rs` replays the record to this code. In short:
//
// - Whatever a name server answers is the answer, a failure code included.
//   Only silence, or a server that cannot be reached, moves on to the next.
// - The name servers are asked in the order given. One that did not answer
//   goes to the back until it answers again; after five seconds, one question
//   in ten also goes to it on the side, to see whether it has recovered.
// - A server never heard from gets two seconds (or resolv.conf's `timeout`).
//   One with a record gets five times its average response time, at least a
//   quarter second, at most five. The wait doubles with each pass over the
//   servers, and the timers are checked once a second.
// - A question is sent four times per name server before it fails. Each
//   sending replaces the one before: an answer to an earlier one is lost.
// - An answer is kept for as long as its records say, a missing name for as
//   long as the zone's SOA record says, an empty answer for an hour, each
//   for an hour at most. Failures are not kept.
// - A question outlives the check that asked it. What the resolver learns
//   after the two-second timeout is there for the next mail.
// ---------------------------------------------------------------------------

/// Sendings of one question per name server before it fails.
const TRIES: usize = 4;
/// The wait for a name server with no record of answering, in milliseconds.
const DEFAULT_TIMEOUT_MS: u64 = 2000;
/// Bounds of the wait for a name server with a record, in milliseconds.
const MIN_TIMEOUT_MS: u64 = 250;
const MAX_TIMEOUT_MS: u64 = 5000;
/// The wait is this many times the average response time.
const TIMEOUT_PER_AVERAGE: u64 = 5;
/// Responses needed in a period before their average counts.
const MIN_SAMPLES: u64 = 3;
/// How long a name server that did not answer is left alone, and the chance
/// (one in this many questions) of trying it on the side after that.
const RETRY_DELAY: Duration = Duration::from_secs(5);
const RETRY_CHANCE: u16 = 10;
/// The longest any answer is kept.
const CACHE_MAX_TTL: Duration = Duration::from_secs(3600);
/// Answers kept at once; the cache is emptied of expired ones when it fills.
const CACHE_CAPACITY: usize = 16_384;

/// The periods response times are averaged over, in seconds, shortest first;
/// zero is "since the start". The first one with enough responses decides.
const LATENCY_PERIODS: [u64; 5] = [60, 900, 3600, 86_400, 0];

/// Response times within one period: the current stretch and the one before.
#[derive(Debug, Clone, Copy, Default)]
struct LatencyBucket {
    slot: u64,
    count: u64,
    total_ms: u64,
    previous_slot: u64,
    previous_count: u64,
    previous_total_ms: u64,
}

/// A name server's response times, kept the way the Bun server's resolver
/// keeps them.
#[derive(Debug, Clone, Copy, Default)]
struct Latency {
    buckets: [LatencyBucket; 5],
}

impl Latency {
    /// Which stretch of a period a moment falls in; never zero, so an unused
    /// bucket matches none.
    fn slot(period: u64, now_secs: u64) -> u64 {
        now_secs.checked_div(period).map_or(1, |stretch| stretch + 1)
    }

    fn record(&mut self, now_secs: u64, elapsed: Duration) {
        let ms = (elapsed.as_millis() as u64).max(1);
        for (bucket, period) in self.buckets.iter_mut().zip(LATENCY_PERIODS) {
            let slot = Latency::slot(period, now_secs);
            if bucket.slot != slot {
                bucket.previous_slot = bucket.slot;
                bucket.previous_count = bucket.count;
                bucket.previous_total_ms = bucket.total_ms;
                bucket.slot = slot;
                bucket.count = 0;
                bucket.total_ms = 0;
            }
            bucket.count += 1;
            bucket.total_ms += ms;
        }
    }

    /// The average response time in milliseconds, once there are enough
    /// responses to speak of one.
    fn average_ms(&self, now_secs: u64) -> Option<u64> {
        self.buckets.iter().zip(LATENCY_PERIODS).find_map(|(bucket, period)| {
            let slot = Latency::slot(period, now_secs);
            if bucket.slot == slot && bucket.count >= MIN_SAMPLES {
                Some(bucket.total_ms / bucket.count)
            } else if bucket.previous_slot + 1 == slot && bucket.previous_count >= MIN_SAMPLES {
                Some(bucket.previous_total_ms / bucket.previous_count)
            } else {
                None
            }
        })
    }
}

#[derive(Debug, Default)]
struct ServerState {
    /// Questions in a row this server did not answer.
    consecutive_failures: u32,
    /// When it may be tried on the side again.
    retry_at: Option<Instant>,
    probe_pending: bool,
    latency: Latency,
}

struct NameServer {
    resolver: TokioAsyncResolver,
    state: Mutex<ServerState>,
}

impl NameServer {
    fn new(address: SocketAddr) -> NameServer {
        let mut config = ResolverConfig::new();
        for protocol in [Protocol::Udp, Protocol::Tcp] {
            config.add_name_server(NameServerConfig::new(address, protocol));
        }
        // One sending per call, never from a cache or the hosts file; how long
        // to wait and whom to ask next is decided here, per question.
        let mut options = ResolverOpts::default();
        options.timeout = Duration::from_secs(60);
        options.attempts = 0;
        options.cache_size = 0;
        options.use_hosts_file = false;
        NameServer { resolver: TokioAsyncResolver::tokio(config, options), state: Mutex::default() }
    }

    fn state(&self) -> MutexGuard<'_, ServerState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How long this sending waits: the server's own timeout, doubled for
    /// each pass already made over the servers, on the resolver's
    /// once-a-second timer.
    fn wait(&self, default_ms: u64, passes: u32, now_secs: u64) -> Duration {
        let timeout_ms = match self.state().latency.average_ms(now_secs) {
            Some(average) => (average * TIMEOUT_PER_AVERAGE).clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
            None => default_ms,
        };
        Duration::from_secs((timeout_ms << passes.min(5)).div_ceil(1000))
    }
}

/// What a question comes to: the addresses, or why there are none.
type Outcome = Result<Vec<String>, DnsLookupError>;

/// What one sending to one name server came to.
enum Reply {
    /// The server answered, with addresses or with a code. `keep_for` is how
    /// long the answer may be reused; `timed` whether it says how fast the
    /// server is, which only an answer or a missing name does.
    Answered { outcome: Outcome, keep_for: Option<Duration>, timed: bool },
    /// Nothing came back in time, or the server could not be reached.
    Unanswered(DnsLookupError),
}

struct Resolver {
    servers: Vec<NameServer>,
    default_timeout_ms: u64,
    /// Answers still good, by lowercased name, each with the moment it stops being so.
    cache: Mutex<HashMap<String, (Instant, Outcome)>>,
    started: Instant,
}

impl Resolver {
    fn now_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<String, (Instant, Outcome)>> {
        self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn remembered(&self, key: &str) -> Option<Outcome> {
        let mut cache = self.cache();
        match cache.get(key) {
            Some((until, outcome)) if *until > Instant::now() => Some(outcome.clone()),
            Some(_) => {
                cache.remove(key);
                None
            }
            None => None,
        }
    }

    fn remember(&self, key: &str, outcome: &Outcome, keep_for: Duration) {
        if keep_for.is_zero() {
            return;
        }
        let now = Instant::now();
        let mut cache = self.cache();
        if cache.len() >= CACHE_CAPACITY {
            cache.retain(|_, (until, _)| *until > now);
            if cache.len() >= CACHE_CAPACITY {
                cache.clear();
            }
        }
        cache.insert(key.to_string(), (now + keep_for.min(CACHE_MAX_TTL), outcome.clone()));
    }

    /// The name server to ask: the one with the fewest unanswered questions
    /// in a row, the earlier one on a tie.
    fn choose(&self) -> usize {
        (0..self.servers.len())
            .min_by_key(|index| (self.servers[*index].state().consecutive_failures, *index))
            .unwrap_or(0)
    }

    /// Sends the question to one name server and waits for it; the outcome is
    /// entered in that server's record.
    async fn send(&self, index: usize, query: &Name, name: &str, wait: Duration) -> Reply {
        let server = &self.servers[index];
        let sent = Instant::now();
        // Dropping the lookup at the timeout closes its socket: a later
        // answer to this sending has nowhere to arrive.
        let reply = match tokio::time::timeout(wait, server.resolver.ipv4_lookup(query.clone())).await {
            Err(_) => {
                Reply::Unanswered(DnsLookupError::new(Some("ETIMEOUT"), format!("queryA ETIMEOUT {name}")))
            }
            Ok(Ok(lookup)) => {
                let addresses: Vec<String> = lookup.iter().map(|address| address.to_string()).collect();
                let keep_for = lookup.valid_until().checked_duration_since(Instant::now());
                let outcome = if addresses.is_empty() {
                    // An answer holding no A record, such as a bare CNAME.
                    Err(DnsLookupError::new(Some("ENODATA"), format!("queryA ENODATA {name}")))
                } else {
                    Ok(addresses)
                };
                Reply::Answered { outcome, keep_for, timed: true }
            }
            Ok(Err(error)) => match error.kind() {
                ResolveErrorKind::NoRecordsFound { response_code, negative_ttl, .. } => {
                    let keep_for = match response_code {
                        ResponseCode::NXDomain => negative_ttl.map(|ttl| Duration::from_secs(u64::from(ttl))),
                        // An empty answer carries no record to take a
                        // lifetime from, and is kept for the longest there is.
                        ResponseCode::NoError => Some(CACHE_MAX_TTL),
                        _ => None,
                    };
                    let code = response_code_name(*response_code);
                    let outcome = Err(DnsLookupError::new(Some(code), format!("queryA {code} {name}")));
                    let timed = matches!(response_code, ResponseCode::NXDomain | ResponseCode::NoError);
                    Reply::Answered { outcome, keep_for, timed }
                }
                _ => Reply::Unanswered(DnsLookupError::new(transport_code(&error), error.to_string())),
            },
        };

        let mut state = server.state();
        match &reply {
            Reply::Answered { timed, .. } => {
                state.consecutive_failures = 0;
                if *timed {
                    state.latency.record(self.now_secs(), sent.elapsed());
                }
            }
            Reply::Unanswered(_) => {
                state.consecutive_failures += 1;
                state.retry_at = Some(Instant::now() + RETRY_DELAY);
            }
        }
        reply
    }

    /// With the question going to a healthy name server, now and then sends
    /// it to one that stopped answering as well, to learn whether it is back.
    fn probe(self: &Arc<Self>, asked: usize, query: &Name, name: &str) {
        if self.servers[asked].state().consecutive_failures > 0 {
            return;
        }
        let now = Instant::now();
        let due = |state: &ServerState| {
            state.consecutive_failures > 0
                && !state.probe_pending
                && state.retry_at.is_none_or(|at| at <= now)
        };
        let Some(index) = (0..self.servers.len()).find(|index| due(&self.servers[*index].state())) else {
            return;
        };
        if !rand::random::<u16>().is_multiple_of(RETRY_CHANCE) {
            return;
        }
        self.servers[index].state().probe_pending = true;
        let (resolver, query, name) = (self.clone(), query.clone(), name.to_string());
        tokio::spawn(async move {
            let wait = resolver.servers[index].wait(resolver.default_timeout_ms, 0, resolver.now_secs());
            resolver.send(index, &query, &name, wait).await;
            resolver.servers[index].state().probe_pending = false;
        });
    }

    /// Asks the name servers until one answers or every sending is used up.
    async fn ask(self: Arc<Self>, query: Name, name: String, key: String) -> Outcome {
        let mut last = DnsLookupError::new(Some("ETIMEOUT"), format!("queryA ETIMEOUT {name}"));
        for sending in 0..TRIES * self.servers.len() {
            let index = self.choose();
            if sending == 0 {
                self.probe(index, &query, &name);
            }
            let passes = (sending / self.servers.len()) as u32;
            let wait = self.servers[index].wait(self.default_timeout_ms, passes, self.now_secs());
            match self.send(index, &query, &name, wait).await {
                Reply::Answered { outcome, keep_for, .. } => {
                    if let Some(keep_for) = keep_for {
                        self.remember(&key, &outcome, keep_for);
                    }
                    return outcome;
                }
                Reply::Unanswered(error) => last = error,
            }
        }
        Err(last)
    }
}

/// The code Node's resolver has for a response code. Bun's reads the
/// remaining codes of the header's four bits as a failed server from 12 up,
/// and below that as an answer that happens to hold no address.
fn response_code_name(code: ResponseCode) -> &'static str {
    match code {
        ResponseCode::NXDomain => "ENOTFOUND",
        ResponseCode::NoError => "ENODATA",
        ResponseCode::ServFail => "ESERVFAIL",
        ResponseCode::Refused => "EREFUSED",
        ResponseCode::FormErr => "EFORMERR",
        ResponseCode::NotImp => "ENOTIMP",
        ResponseCode::Unknown(12..=15) => "ESERVFAIL",
        _ => "ENODATA",
    }
}

/// The code Node's resolver has for a question that got no response, where one corresponds.
fn transport_code(error: &ResolveError) -> Option<&'static str> {
    let io_code = |error: &std::io::Error| match error.kind() {
        std::io::ErrorKind::ConnectionRefused => Some("ECONNREFUSED"),
        std::io::ErrorKind::TimedOut => Some("ETIMEOUT"),
        _ => None,
    };
    match error.kind() {
        ResolveErrorKind::Timeout => Some("ETIMEOUT"),
        ResolveErrorKind::Io(error) => io_code(error),
        ResolveErrorKind::Proto(error) => match error.kind() {
            ProtoErrorKind::Timeout => Some("ETIMEOUT"),
            ProtoErrorKind::Io(error) => io_code(error),
            _ => None,
        },
        _ => None,
    }
}

/// `options timeout:N` of resolv.conf or of `RES_OPTIONS`, in milliseconds;
/// the default when neither sets one. (`attempts` is not read: the Bun
/// server's resolver sends four times whatever it says.)
fn configured_timeout_ms() -> u64 {
    let resolv_conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let from_file = resolv_conf
        .lines()
        .filter_map(|line| line.trim().strip_prefix("options"))
        .flat_map(|options| options.split_whitespace().map(str::to_string).collect::<Vec<_>>());
    let from_env = std::env::var("RES_OPTIONS").unwrap_or_default();
    let seconds = from_file
        .chain(from_env.split_whitespace().map(str::to_string))
        .filter_map(|option| option.strip_prefix("timeout:").and_then(|value| value.parse::<u64>().ok()))
        .next_back();
    match seconds {
        Some(seconds) if seconds > 0 => seconds.min(30) * 1000,
        _ => DEFAULT_TIMEOUT_MS,
    }
}

/// DNS over the network, through the system's name servers, asked the way
/// the Bun server's resolver asks them (see the notes above).
pub struct SystemDnsLookup {
    resolver: Arc<Resolver>,
}

impl SystemDnsLookup {
    /// A resolver over the name servers of `/etc/resolv.conf`, or over
    /// `127.0.0.1` when the file is unreadable or names none.
    pub fn from_system_conf() -> SystemDnsLookup {
        let mut addresses: Vec<SocketAddr> = Vec::new();
        if let Ok((config, _)) = hickory_resolver::system_conf::read_system_conf() {
            for name_server in config.name_servers() {
                if !addresses.contains(&name_server.socket_addr) {
                    addresses.push(name_server.socket_addr);
                }
            }
        }
        if addresses.is_empty() {
            addresses.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53));
        }
        SystemDnsLookup::build(&addresses, configured_timeout_ms())
    }

    /// A resolver over the given name servers, in that order, each asked over
    /// UDP and over TCP for an answer too long for it.
    pub fn with_name_servers(name_servers: &[SocketAddr]) -> SystemDnsLookup {
        SystemDnsLookup::build(name_servers, DEFAULT_TIMEOUT_MS)
    }

    fn build(name_servers: &[SocketAddr], default_timeout_ms: u64) -> SystemDnsLookup {
        SystemDnsLookup {
            resolver: Arc::new(Resolver {
                servers: name_servers.iter().copied().map(NameServer::new).collect(),
                default_timeout_ms,
                cache: Mutex::default(),
                started: Instant::now(),
            }),
        }
    }
}

/// The characters the Bun server's resolver accepts in a name.
fn is_hostname_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'/' | b'*')
}

/// The labels the Bun server's resolver puts on the wire for a name, or
/// `None` for a name it refuses to ask: an empty label, a label over 63
/// bytes, a name over 255 characters, or a character outside
/// `[A-Za-z0-9-._/*]`. A backslash escapes the next character, or three
/// decimal digits naming a byte; one trailing dot is the root, not a label.
fn wire_labels(name: &str) -> Option<Vec<Vec<u8>>> {
    let mut labels: Vec<Vec<u8>> = vec![Vec::new()];
    let mut bytes = name.bytes();
    while let Some(byte) = bytes.next() {
        let literal = match byte {
            b'.' => {
                labels.push(Vec::new());
                continue;
            }
            b'\\' => {
                let escaped = bytes.next()?;
                if escaped.is_ascii_digit() {
                    let mut value = u32::from(escaped - b'0');
                    for _ in 0..2 {
                        let digit = bytes.next().filter(u8::is_ascii_digit)?;
                        value = value * 10 + u32::from(digit - b'0');
                    }
                    u8::try_from(value).ok()?
                } else {
                    escaped
                }
            }
            byte => byte,
        };
        if !is_hostname_byte(literal) {
            return None;
        }
        labels.last_mut()?.push(literal);
    }

    if labels.last().is_some_and(Vec::is_empty) {
        labels.pop();
    }
    if labels.len() == 1 && labels[0].is_empty() {
        // "." alone is the root.
        labels.clear();
    }
    if labels.iter().any(|label| label.is_empty() || label.len() > 63) {
        return None;
    }
    let length: usize = labels.iter().map(Vec::len).sum::<usize>() + labels.len().saturating_sub(1);
    (length <= 255).then_some(labels)
}

impl DnsLookup for SystemDnsLookup {
    fn resolve4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Vec<String>, DnsLookupError>> {
        Box::pin(async move {
            let Some(labels) = wire_labels(name) else {
                // The Bun server's resolver reports a name it cannot ask as not
                // found, which the blocklist check reads as "not listed". A
                // malformed blocklist hostname therefore clears every address
                // there, and has to here.
                return Err(DnsLookupError::new(Some("ENOTFOUND"), format!("queryA ENOTFOUND {name}")));
            };
            let key = labels
                .iter()
                .map(|label| String::from_utf8_lossy(label).to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(".");
            if let Some(outcome) = self.resolver.remembered(&key) {
                return outcome;
            }
            // Absolute, so no search list ever applies. A name of 254 or 255
            // characters is one the Bun server's resolver sends although DNS
            // has no room for it; this one will not.
            let mut query =
                Name::from_labels(labels).map_err(|error| DnsLookupError::new(None, error.to_string()))?;
            query.set_fqdn(true);
            // On a task of its own: the check gives up after two seconds, the
            // question does not, and what it learns is there for the next one.
            let asking = tokio::spawn(self.resolver.clone().ask(query, name.to_string(), key));
            asking.await.unwrap_or_else(|error| Err(DnsLookupError::new(None, error.to_string())))
        })
    }
}

/// The process-wide resolver, created from the system configuration on first use.
fn system_lookup() -> &'static SystemDnsLookup {
    static LOOKUP: OnceLock<SystemDnsLookup> = OnceLock::new();
    LOOKUP.get_or_init(SystemDnsLookup::from_system_conf)
}

/// Asks one blocklist about a dotted quad, giving up after
/// [`DNSBL_TIMEOUT_MS`] so a silent resolver cannot hold a delivery.
async fn check_dnsbl(ipv4: &str, dnsbl: &DnsBlocklist, lookup: &dyn DnsLookup) -> DnsblVerdict {
    let query = format!("{}.{}", reverse_ip(ipv4), dnsbl.hostname);
    match tokio::time::timeout(Duration::from_millis(DNSBL_TIMEOUT_MS), lookup.resolve4(&query)).await {
        Ok(Ok(addresses)) => verdict_for_answer(&addresses),
        Ok(Err(error)) => verdict_for_dns_error(error.code.as_deref()),
        // The timeout carries no code.
        Err(_) => verdict_for_dns_error(None),
    }
}

/// One blocklist's answer about an address, as the per-blocklist queries settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsblAnswer {
    Fulfilled {
        dnsbl: DnsBlocklist,
        verdict: DnsblVerdict,
    },
    /// The query failed outside the lookup itself; `reason` says how.
    Rejected {
        reason: String,
    },
}

/// What the blocklists said about an address.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsblResult {
    /// Sum of the scores of the blocklists that list the address.
    pub score: i64,
    pub listed_in: Vec<DnsBlocklist>,
    /// `Listed in <name>` for each listing.
    pub reasons: Vec<String>,
    /// Whether every blocklist reached a verdict: the score is a verdict, not
    /// an absence of one.
    pub evaluated: bool,
}

/// Folds the per-blocklist answers into one result.
///
/// `score` is an OR over the answers — one listing is enough to score.
/// `evaluated` is an AND: it reports whether every blocklist reached a verdict,
/// so a caller that treats `score: 0` as authorization can tell a clearance
/// from an absence of one. An answer that never arrived withholds the signal
/// rather than supplying it, which is why a single `Unknown` or a rejected
/// query clears it. No answers at all is unevaluated: nothing was asked.
pub fn aggregate_verdicts(answers: &[DnsblAnswer]) -> DnsblResult {
    let mut result = DnsblResult { evaluated: !answers.is_empty(), ..Default::default() };

    for answer in answers {
        match answer {
            DnsblAnswer::Rejected { .. } | DnsblAnswer::Fulfilled { verdict: DnsblVerdict::Unknown, .. } => {
                result.evaluated = false;
            }
            DnsblAnswer::Fulfilled { dnsbl, verdict: DnsblVerdict::Listed } => {
                result.listed_in.push(dnsbl.clone());
                result.score += dnsbl.score;
                result.reasons.push(format!("Listed in {}", dnsbl.name));
            }
            DnsblAnswer::Fulfilled { verdict: DnsblVerdict::Clean, .. } => {}
        }
    }

    result
}

/// Checks an address against the given blocklists through the system
/// resolver; pass [`default_dnsbls`] for the default set.
///
/// `evaluated` reports whether the score is a verdict or an absence of one, so a
/// caller that treats a clean address as authorization can tell the two apart.
pub async fn check_dnsbls(ip: &str, dnsbls: &[DnsBlocklist]) -> DnsblResult {
    check_dnsbls_using(ip, dnsbls, system_lookup()).await
}

/// [`check_dnsbls`] through the given lookup. The blocklists are asked in
/// parallel, each bounded by its own timeout.
pub async fn check_dnsbls_using(ip: &str, dnsbls: &[DnsBlocklist], lookup: &dyn DnsLookup) -> DnsblResult {
    // An address outside IPv4 is one the blocklists cannot be asked about.
    let Some(ipv4) = to_ipv4(ip) else {
        return DnsblResult::default();
    };
    // A private or local address names no internet host, so no blocklist carries
    // it — "not listed" is a definite answer here, not a missing one.
    if is_private_ip(ipv4) {
        return DnsblResult { evaluated: true, ..Default::default() };
    }

    let answers = join_all(dnsbls.iter().map(|dnsbl| async move {
        DnsblAnswer::Fulfilled { dnsbl: dnsbl.clone(), verdict: check_dnsbl(ipv4, dnsbl, lookup).await }
    }))
    .await;

    aggregate_verdicts(&answers)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Instant;

    use tokio::net::UdpSocket;

    use super::*;

    fn zen() -> DnsBlocklist {
        DnsBlocklist::new("zen.spamhaus.org", "Spamhaus ZEN", 40)
    }

    fn spamcop() -> DnsBlocklist {
        DnsBlocklist::new("bl.spamcop.net", "SpamCop", 30)
    }

    fn answered(dnsbl: DnsBlocklist, verdict: DnsblVerdict) -> DnsblAnswer {
        DnsblAnswer::Fulfilled { dnsbl, verdict }
    }

    fn threw() -> DnsblAnswer {
        DnsblAnswer::Rejected { reason: "query failed".into() }
    }

    // --- checkDnsbls ---

    #[tokio::test]
    async fn skips_private_ip_addresses() {
        for ip in ["127.0.0.1", "10.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.1.1"] {
            let result = check_dnsbls(ip, default_dnsbls()).await;
            assert_eq!(result.score, 0, "{ip}");
            assert!(result.listed_in.is_empty(), "{ip}");
            assert!(result.reasons.is_empty(), "{ip}");
        }
    }

    #[tokio::test]
    async fn handles_invalid_ip_addresses_gracefully() {
        for ip in ["not-an-ip", "256.1.1.1", "1.2.3", "1.2.3.4.5", ""] {
            let result = check_dnsbls(ip, default_dnsbls()).await;
            assert_eq!(result.score, 0, "{ip}");
            assert!(result.listed_in.is_empty(), "{ip}");
        }
    }

    // The next three reach the network, as they do in the Bun suite. Each
    // uses a resolver of its own: a test's runtime ends with the test, and a
    // shared resolver would keep sockets that belong to an ended one.

    #[tokio::test]
    async fn uses_the_default_dnsbls_on_a_public_address() {
        // Cloudflare DNS (1.1.1.1) should not be on any blocklist.
        let result =
            check_dnsbls_using("1.1.1.1", default_dnsbls(), &SystemDnsLookup::from_system_conf()).await;
        assert_eq!(result.score, 0);
    }

    #[tokio::test]
    async fn aggregates_into_the_result_structure() {
        let result =
            check_dnsbls_using("8.8.8.8", default_dnsbls(), &SystemDnsLookup::from_system_conf()).await;
        assert!(result.score >= 0);
        assert_eq!(result.listed_in.len(), result.reasons.len());
    }

    #[tokio::test]
    async fn handles_dns_query_failures_gracefully() {
        let fake = [DnsBlocklist::new("invalid.nonexistent.dnsbl.local", "Fake DNSBL", 100)];
        let result = check_dnsbls_using("8.8.8.8", &fake, &SystemDnsLookup::from_system_conf()).await;
        assert_eq!(result.score, 0);
        assert!(result.listed_in.is_empty());
    }

    // --- toIpv4 ---

    #[test]
    fn to_ipv4_unwraps_the_ipv4_mapped_form_a_dual_stack_listener_reports() {
        assert_eq!(to_ipv4("::ffff:198.51.100.7"), Some("198.51.100.7"));
        assert_eq!(to_ipv4("::FFFF:203.0.113.5"), Some("203.0.113.5"));
    }

    #[test]
    fn to_ipv4_passes_a_plain_dotted_quad_through_unchanged() {
        assert_eq!(to_ipv4("198.51.100.7"), Some("198.51.100.7"));
        assert_eq!(to_ipv4("0.0.0.0"), Some("0.0.0.0"));
        assert_eq!(to_ipv4("255.255.255.255"), Some("255.255.255.255"));
    }

    #[test]
    fn to_ipv4_returns_none_for_an_address_that_denotes_no_ipv4_host() {
        for ip in
            ["2001:db8::1", "::1", "::ffff:2001:db8::1", "not-an-ip", "256.1.1.1", "1.2.3", "1.2.3.4.5", ""]
        {
            assert_eq!(to_ipv4(ip), None, "{ip}");
        }
    }

    // --- checkDnsbls evaluability ---

    #[tokio::test]
    async fn reports_an_address_outside_ipv4_as_unevaluated() {
        for ip in ["2001:db8::1", "not-an-ip", "1.2.3", ""] {
            let result = check_dnsbls(ip, default_dnsbls()).await;
            assert_eq!(result.score, 0, "{ip}");
            assert!(!result.evaluated, "{ip}");
        }
    }

    #[tokio::test]
    async fn reports_a_private_address_as_evaluated() {
        for ip in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "::ffff:10.0.0.1"] {
            let result = check_dnsbls(ip, default_dnsbls()).await;
            assert_eq!(result.score, 0, "{ip}");
            assert!(result.evaluated, "{ip}");
        }
    }

    #[tokio::test]
    async fn reports_an_empty_blocklist_set_as_unevaluated() {
        let result = check_dnsbls("198.51.100.7", &[]).await;
        assert_eq!(result.score, 0);
        assert!(!result.evaluated);
    }

    // --- aggregateVerdicts ---

    #[test]
    fn aggregate_reports_evaluated_when_every_blocklist_reached_a_verdict() {
        let result = aggregate_verdicts(&[
            answered(zen(), DnsblVerdict::Listed),
            answered(spamcop(), DnsblVerdict::Clean),
        ]);
        assert_eq!(result.score, 40);
        assert_eq!(result.listed_in, vec![zen()]);
        assert_eq!(result.reasons, vec!["Listed in Spamhaus ZEN"]);
        assert!(result.evaluated);
    }

    #[test]
    fn aggregate_withdraws_evaluated_when_one_blocklist_never_answered() {
        // A clearance from the others is not a clearance from the one that was
        // never asked. Reading this as evaluated hands the allowlist exemption to
        // a sender whose connection no blocklist has an opinion on.
        let result = aggregate_verdicts(&[
            answered(zen(), DnsblVerdict::Clean),
            answered(spamcop(), DnsblVerdict::Unknown),
        ]);
        assert_eq!(result.score, 0);
        assert!(!result.evaluated);
    }

    #[test]
    fn aggregate_keeps_a_listings_score_while_withdrawing_evaluated() {
        let result = aggregate_verdicts(&[
            answered(zen(), DnsblVerdict::Listed),
            answered(spamcop(), DnsblVerdict::Unknown),
        ]);
        assert_eq!(result.score, 40);
        assert!(!result.evaluated);
    }

    #[test]
    fn aggregate_withdraws_evaluated_when_a_query_rejected_instead_of_answering() {
        let result = aggregate_verdicts(&[answered(zen(), DnsblVerdict::Clean), threw()]);
        assert_eq!(result.score, 0);
        assert!(!result.evaluated);
    }

    #[test]
    fn aggregate_reports_an_empty_answer_set_as_unevaluated() {
        assert_eq!(
            aggregate_verdicts(&[]),
            DnsblResult { score: 0, listed_in: vec![], reasons: vec![], evaluated: false }
        );
    }

    #[test]
    fn aggregate_sums_every_listing_rather_than_stopping_at_the_first() {
        let result = aggregate_verdicts(&[
            answered(zen(), DnsblVerdict::Listed),
            answered(spamcop(), DnsblVerdict::Listed),
        ]);
        assert_eq!(result.score, 70);
        assert_eq!(result.listed_in, vec![zen(), spamcop()]);
        assert!(result.evaluated);
    }

    // --- isRealListing ---

    #[test]
    fn real_listing_treats_127_0_0_x_addresses_as_real_listings() {
        assert!(is_real_listing(&["127.0.0.2"])); // Spamhaus SBL
        assert!(is_real_listing(&["127.0.0.4"])); // Spamhaus CSS
        assert!(is_real_listing(&["127.0.0.10"])); // Spamhaus PBL
        assert!(is_real_listing(&["127.0.0.2", "127.0.0.10"]));
    }

    #[test]
    fn real_listing_ignores_open_resolver_warnings() {
        // Spamhaus returns these when the query arrives via a public resolver.
        // They are warnings, not listings — treating them as listings flags
        // every incoming mail as spam.
        for address in ["127.255.255.252", "127.255.255.253", "127.255.255.254", "127.255.255.255"] {
            assert!(!is_real_listing(&[address]), "{address}");
        }
    }

    #[test]
    fn real_listing_ignores_mixed_responses_that_contain_only_warnings() {
        assert!(!is_real_listing(&["127.255.255.252", "127.255.255.254"]));
    }

    #[test]
    fn real_listing_counts_when_at_least_one_listing_record_is_present() {
        // Defensive: if a response returns both a real listing code and a
        // warning code, the real listing wins.
        assert!(is_real_listing(&["127.0.0.2", "127.255.255.254"]));
    }

    #[test]
    fn real_listing_is_false_on_empty_results() {
        assert!(!is_real_listing::<&str>(&[]));
    }

    #[test]
    fn real_listing_ignores_responses_outside_127_0() {
        // Some ISPs and captive portals return a content address for NXDOMAIN.
        assert!(!is_real_listing(&["192.0.2.1"]));
        assert!(!is_real_listing(&["10.0.0.1"]));
    }

    // --- verdictForAnswer ---

    #[test]
    fn answer_reads_a_127_0_0_x_record_as_a_listing() {
        assert_eq!(verdict_for_answer(&["127.0.0.2"]), DnsblVerdict::Listed);
        assert_eq!(verdict_for_answer(&["127.0.0.10"]), DnsblVerdict::Listed);
        assert_eq!(verdict_for_answer(&["127.0.0.2", "127.255.255.254"]), DnsblVerdict::Listed);
    }

    #[test]
    fn answer_reads_an_open_resolver_warning_as_no_answer_never_as_a_clearance() {
        // Reading it as clean hands the allowlist exemption to any sender whose
        // query the blocklist refused to answer.
        assert_eq!(verdict_for_answer(&["127.255.255.254"]), DnsblVerdict::Unknown);
        assert_eq!(verdict_for_answer(&["127.255.255.252"]), DnsblVerdict::Unknown);
        assert_eq!(verdict_for_answer(&["127.255.255.252", "127.255.255.254"]), DnsblVerdict::Unknown);
    }

    #[test]
    fn answer_reads_a_hijacked_nxdomain_response_as_no_answer() {
        assert_eq!(verdict_for_answer(&["192.0.2.1"]), DnsblVerdict::Unknown);
        assert_eq!(verdict_for_answer::<&str>(&[]), DnsblVerdict::Unknown);
    }

    // --- verdictForDnsError ---

    #[test]
    fn dns_error_clears_the_address_only_on_a_no_such_record_code() {
        for code in ["ENOTFOUND", "ENODATA", "NOTFOUND", "NODATA"] {
            assert_eq!(verdict_for_dns_error(Some(code)), DnsblVerdict::Clean, "{code}");
        }
    }

    #[test]
    fn dns_error_withholds_a_verdict_when_the_resolver_failed_rather_than_answered() {
        for code in ["ETIMEOUT", "ESERVFAIL", "EREFUSED", "ECONNREFUSED"] {
            assert_eq!(verdict_for_dns_error(Some(code)), DnsblVerdict::Unknown, "{code}");
        }
    }

    #[test]
    fn dns_error_withholds_a_verdict_when_the_rejection_carries_no_code() {
        // The timeout rejects with no code, so this is the shape condition 2 of
        // the allowlist gate leads with.
        assert_eq!(verdict_for_dns_error(None), DnsblVerdict::Unknown);
        assert_eq!(verdict_for_dns_error(Some("")), DnsblVerdict::Unknown);
    }

    // --- DEFAULT_DNSBLS ---

    #[test]
    fn default_dnsbls_have_the_expected_structure() {
        assert!(!default_dnsbls().is_empty());
        for dnsbl in default_dnsbls() {
            assert!(!dnsbl.hostname.is_empty());
            assert!(!dnsbl.name.is_empty());
            assert!(dnsbl.score > 0);
        }
    }

    #[test]
    fn default_dnsbls_include_the_major_blocklists() {
        let hostnames: Vec<&str> = default_dnsbls().iter().map(|dnsbl| dnsbl.hostname.as_str()).collect();
        assert!(hostnames.contains(&"zen.spamhaus.org"));
        assert!(hostnames.contains(&"bl.spamcop.net"));
        assert_eq!(
            default_dnsbls(),
            [
                DnsBlocklist::new("zen.spamhaus.org", "Spamhaus ZEN", 40),
                DnsBlocklist::new("bl.spamcop.net", "SpamCop", 30),
                DnsBlocklist::new("b.barracudacentral.org", "Barracuda", 25),
            ]
        );
    }

    // --- Beyond the TypeScript suite: the lookup seam and the resolver ---

    /// Answers by query name and records what was asked.
    #[derive(Default)]
    struct Canned {
        answers: HashMap<String, Result<Vec<String>, DnsLookupError>>,
        delay: Duration,
        asked: Mutex<Vec<String>>,
    }

    impl Canned {
        fn answer(mut self, name: &str, addresses: &[&str]) -> Canned {
            self.answers.insert(name.to_string(), Ok(addresses.iter().map(|a| a.to_string()).collect()));
            self
        }

        fn fail(mut self, name: &str, code: Option<&str>) -> Canned {
            self.answers.insert(name.to_string(), Err(DnsLookupError::new(code, "failed")));
            self
        }
    }

    impl DnsLookup for Canned {
        fn resolve4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Vec<String>, DnsLookupError>> {
            Box::pin(async move {
                self.asked.lock().unwrap().push(name.to_string());
                tokio::time::sleep(self.delay).await;
                match self.answers.get(name) {
                    Some(answer) => answer.clone(),
                    None => Err(DnsLookupError::new(Some("ENOTFOUND"), format!("queryA ENOTFOUND {name}"))),
                }
            })
        }
    }

    #[tokio::test]
    async fn asks_each_blocklist_for_the_reversed_address() {
        let lookup = Canned::default();
        let result = check_dnsbls_using("198.51.100.7", default_dnsbls(), &lookup).await;
        assert_eq!(result, DnsblResult { evaluated: true, ..Default::default() });
        let mut asked = lookup.asked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(
            asked,
            vec![
                "7.100.51.198.b.barracudacentral.org",
                "7.100.51.198.bl.spamcop.net",
                "7.100.51.198.zen.spamhaus.org",
            ]
        );
    }

    #[tokio::test]
    async fn scores_listings_in_blocklist_order_and_unwraps_a_mapped_address() {
        let lookup = Canned::default()
            .answer("7.100.51.198.zen.spamhaus.org", &["127.0.0.2", "127.0.0.4"])
            .answer("7.100.51.198.b.barracudacentral.org", &["127.0.0.2"]);
        let result = check_dnsbls_using("::FFFF:198.51.100.7", default_dnsbls(), &lookup).await;
        assert_eq!(result.score, 65);
        assert_eq!(result.reasons, vec!["Listed in Spamhaus ZEN", "Listed in Barracuda"]);
        assert_eq!(result.listed_in.len(), 2);
        assert!(result.evaluated);
    }

    #[tokio::test]
    async fn an_unanswered_blocklist_withholds_evaluated_but_not_the_score() {
        let lookup = Canned::default()
            .answer("7.100.51.198.zen.spamhaus.org", &["127.0.0.2"])
            .answer("7.100.51.198.bl.spamcop.net", &["127.255.255.254"])
            .fail("7.100.51.198.b.barracudacentral.org", Some("ESERVFAIL"));
        let result = check_dnsbls_using("198.51.100.7", default_dnsbls(), &lookup).await;
        assert_eq!(result.score, 40);
        assert!(!result.evaluated);

        let refused = Canned::default().fail("7.100.51.198.zen.spamhaus.org", None);
        let result = check_dnsbls_using("198.51.100.7", &[zen()], &refused).await;
        assert_eq!(result, DnsblResult::default());
    }

    #[tokio::test]
    async fn keeps_the_octets_as_written() {
        let lookup = Canned::default().answer("004.003.002.001.zen.spamhaus.org", &["127.0.0.2"]);
        let result = check_dnsbls_using("001.002.003.004", &[zen()], &lookup).await;
        assert_eq!(result.score, 40);
        // 010 is ten, not eight: the private check reads the octets as decimal.
        let result = check_dnsbls_using("010.1.2.3", &[zen()], &Canned::default()).await;
        assert!(result.evaluated);
        assert!(lookup.asked.lock().unwrap().len() == 1);
    }

    #[tokio::test]
    async fn asks_in_parallel_and_gives_up_after_the_timeout() {
        // Three lookups that never answer in time settle together at the
        // two-second timeout, not one after another.
        assert_eq!(DNSBL_TIMEOUT_MS, 2000);
        let lookup = Canned { delay: Duration::from_millis(DNSBL_TIMEOUT_MS + 3000), ..Default::default() };
        let started = Instant::now();
        let result = check_dnsbls_using("198.51.100.7", default_dnsbls(), &lookup).await;
        let elapsed = started.elapsed();
        assert_eq!(result, DnsblResult::default());
        assert!(elapsed >= Duration::from_millis(DNSBL_TIMEOUT_MS), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(DNSBL_TIMEOUT_MS + 1500), "{elapsed:?}");
        assert_eq!(lookup.asked.lock().unwrap().len(), 3);
    }

    #[test]
    fn names_go_on_the_wire_as_the_bun_servers_resolver_puts_them() {
        // Observed from Bun's resolver against a name server that logs its questions.
        let labels = |name: &str| {
            wire_labels(name).map(|labels| {
                labels.iter().map(|label| String::from_utf8_lossy(label).to_string()).collect::<Vec<_>>()
            })
        };
        let asked = |name: &str, expected: &[&str]| {
            assert_eq!(labels(name).as_deref(), Some(&words(expected)[..]), "{name}")
        };
        asked("4.3.2.1.zen.spamhaus.org", &["4", "3", "2", "1", "zen", "spamhaus", "org"]);
        asked("004.003.002.001.zen.test", &["004", "003", "002", "001", "zen", "test"]);
        asked("trailing.test.", &["trailing", "test"]);
        asked("7.100.51.198.", &["7", "100", "51", "198"]);
        asked("localhost", &["localhost"]);
        asked(".", &[]);
        asked("MiXeD.TeSt", &["MiXeD", "TeSt"]);
        asked("under_score.test", &["under_score", "test"]);
        asked("-dash.test", &["-dash", "test"]);
        asked("*.wild.test", &["*", "wild", "test"]);
        asked("a/b.test", &["a/b", "test"]);
        asked("back\\slash.test", &["backslash", "test"]);
        asked("escaped\\.dot.test", &["escaped.dot", "test"]);
        asked("\\065.test", &["A", "test"]);
        asked("a\\046b.test", &["a.b", "test"]);
        asked("a\\045b.test", &["a-b", "test"]);
        asked("a\\*b.test", &["a*b", "test"]);
        asked("abc.test\\.", &["abc", "test."]);
        asked("a.\\..b", &["a", ".", "b"]);
        let longest_label = "x".repeat(63);
        asked(&format!("{longest_label}.test"), &[longest_label.as_str(), "test"]);

        for refused in [
            "bad..test",
            "7.100.51.198..",
            "..",
            ".leading.test",
            "trailing.test..",
            "a b.test",
            "a@b.test",
            "a+b.test",
            "a:b.test",
            "ünï.test",
            "中文.test",
            "a\0b.test",
            "tab\t.test",
            "q\"uote.test",
            "a\\032b.test",
            "a\\@b.test",
            "a\\255b.test",
            "a\\256b.test",
            "a\\12.test",
            "test.a\\1",
            "abc.test\\",
            "a\\06a.test",
            "a\\\\b.test",
        ] {
            assert_eq!(labels(refused), None, "{refused}");
        }
        assert_eq!(labels(&format!("{}.test", "x".repeat(64))), None);

        // 255 characters are asked, 256 are not.
        let label = |length: usize| "a".repeat(length);
        let at_limit = [label(63), label(63), label(63), label(63)].join(".");
        assert_eq!(at_limit.len(), 255);
        assert_eq!(wire_labels(&at_limit).map(|labels| labels.len()), Some(4));
        assert_eq!(wire_labels(&format!("{at_limit}.")).map(|labels| labels.len()), Some(4));
        let over_limit = [label(63), label(63), label(63), label(62), label(1)].join(".");
        assert_eq!(over_limit.len(), 256);
        assert_eq!(wire_labels(&over_limit), None);
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| word.to_string()).collect()
    }

    #[tokio::test]
    async fn a_name_that_cannot_be_asked_reads_as_not_listed() {
        // What the Bun server does with a malformed blocklist hostname: its
        // resolver reports ENOTFOUND without asking anyone.
        let lookup = SystemDnsLookup::with_name_servers(&["127.0.0.1:9".parse().unwrap()]);
        for hostname in ["bad..test", "has space.test", "bücher.test"] {
            let error = lookup.resolve4(&format!("7.100.51.198.{hostname}")).await.unwrap_err();
            assert_eq!(error.code.as_deref(), Some("ENOTFOUND"), "{hostname}");
            let result =
                check_dnsbls_using("198.51.100.7", &[DnsBlocklist::new(hostname, "Bad", 40)], &lookup).await;
            assert_eq!(result, DnsblResult { evaluated: true, ..Default::default() }, "{hostname}");
        }
    }

    #[test]
    fn octets_are_decimal_digit_runs_up_to_255() {
        for octet in ["0", "7", "007", "255", "0000000255", "00"] {
            assert!(is_octet(octet), "{octet}");
        }
        for octet in ["", "256", "1000", "99999999999999999999", "1a", "-1", "+1", " 1", "1 ", "٣"] {
            assert!(!is_octet(octet), "{octet}");
        }
    }

    #[test]
    fn a_name_server_is_given_five_times_its_average_response_time() {
        // The waits Bun's resolver was measured to give a server that had
        // answered in so many milliseconds, before moving on from its silence.
        let server = NameServer::new("127.0.0.1:9".parse().unwrap());
        let wait = |server: &NameServer| server.wait(DEFAULT_TIMEOUT_MS, 0, 0).as_millis();
        // Never heard from, or heard from twice: the default.
        assert_eq!(wait(&server), 2000);
        for _ in 0..2 {
            server.state().latency.record(0, Duration::from_millis(1));
        }
        assert_eq!(wait(&server), 2000);
        // A third response makes a record: a quarter second at least, on a
        // timer that is checked once a second.
        server.state().latency.record(0, Duration::from_millis(1));
        assert_eq!(wait(&server), 1000);

        for (average_ms, expected_ms) in [
            (60, 1000),
            (150, 1000),
            (200, 1000),
            (202, 2000),
            (392, 2000),
            (412, 3000),
            (902, 5000),
            (5000, 5000),
        ] {
            let server = NameServer::new("127.0.0.1:9".parse().unwrap());
            for _ in 0..3 {
                server.state().latency.record(0, Duration::from_millis(average_ms));
            }
            assert_eq!(wait(&server), expected_ms, "{average_ms} ms");
        }

        // Each pass over the name servers doubles the wait.
        let unheard = NameServer::new("127.0.0.1:9".parse().unwrap());
        let waits: Vec<u64> =
            (0..4).map(|pass| unheard.wait(DEFAULT_TIMEOUT_MS, pass, 0).as_secs()).collect();
        assert_eq!(waits, vec![2, 4, 8, 16]);
    }

    #[test]
    fn response_times_are_averaged_over_the_latest_period_that_has_enough() {
        let mut latency = Latency::default();
        assert_eq!(latency.average_ms(0), None);
        for elapsed in [10, 20, 60] {
            latency.record(5, Duration::from_millis(elapsed));
        }
        assert_eq!(latency.average_ms(30), Some(30));
        // Once that minute is over the quarter hour speaks for it, and much
        // later everything since the start.
        assert_eq!(latency.average_ms(90), Some(30));
        assert_eq!(latency.average_ms(600), Some(30));
        assert_eq!(latency.average_ms(10_000_000), Some(30));
        // New responses take over once there are three of them.
        for elapsed in [300, 300] {
            latency.record(10_000_000, Duration::from_millis(elapsed));
        }
        assert_eq!(latency.average_ms(10_000_000), Some((10 + 20 + 60 + 300 + 300) / 5));
        latency.record(10_000_000, Duration::from_millis(300));
        assert_eq!(latency.average_ms(10_000_000), Some(300));
        // A response too fast to measure counts as a millisecond.
        let mut instant = Latency::default();
        for _ in 0..3 {
            instant.record(0, Duration::ZERO);
        }
        assert_eq!(instant.average_ms(0), Some(1));
    }

    #[test]
    fn the_resolv_conf_timeout_is_read_in_seconds() {
        // Whatever this machine's resolv.conf says, it is a whole number of seconds.
        let timeout = configured_timeout_ms();
        assert!(timeout >= 1000 && timeout.is_multiple_of(1000), "{timeout}");
    }

    /// A DNS server on a loopback UDP port that answers every A query from
    /// `answer`: a response code and the records to return.
    async fn name_server(
        answer: impl Fn(&str) -> Option<(u8, Vec<[u8; 4]>)> + Send + 'static,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buffer = [0u8; 1500];
            loop {
                let Ok((length, peer)) = socket.recv_from(&mut buffer).await else { return };
                let query = &buffer[..length];
                // The question starts after the 12-byte header: labels, type, class.
                let mut labels = Vec::new();
                let mut at = 12;
                while at < query.len() && query[at] != 0 {
                    let size = query[at] as usize;
                    labels.push(String::from_utf8_lossy(&query[at + 1..at + 1 + size]).to_string());
                    at += 1 + size;
                }
                let question_end = at + 5;
                let Some((rcode, records)) = answer(&labels.join(".")) else { continue };

                let mut response = Vec::new();
                response.extend_from_slice(&query[..2]);
                response.extend_from_slice(&[0x81, 0x80 | rcode]);
                response.extend_from_slice(&[0, 1]);
                response.extend_from_slice(&(records.len() as u16).to_be_bytes());
                response.extend_from_slice(&[0, 0, 0, 0]);
                response.extend_from_slice(&query[12..question_end]);
                for record in records {
                    response.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
                    response.extend_from_slice(&record);
                }
                let _ = socket.send_to(&response, peer).await;
            }
        });
        (address, task)
    }

    #[tokio::test]
    async fn the_resolver_reports_what_nodes_resolver_reports() {
        let (address, server) = name_server(|name| match name {
            "listed.example" => Some((0, vec![[127, 0, 0, 2], [127, 0, 0, 10]])),
            "nodata.example" => Some((0, vec![])),
            "nxdomain.example" => Some((3, vec![])),
            "servfail.example" => Some((2, vec![])),
            "refused.example" => Some((5, vec![])),
            _ => None,
        })
        .await;
        let lookup = SystemDnsLookup::with_name_servers(&[address]);

        let mut listed = lookup.resolve4("listed.example").await.unwrap();
        listed.sort();
        assert_eq!(listed, vec!["127.0.0.10", "127.0.0.2"]);
        // A trailing dot is the same absolute name.
        assert!(lookup.resolve4("listed.example.").await.is_ok());

        let code = |result: Result<Vec<String>, DnsLookupError>| result.unwrap_err().code;
        assert_eq!(code(lookup.resolve4("nodata.example").await).as_deref(), Some("ENODATA"));
        assert_eq!(code(lookup.resolve4("nxdomain.example").await).as_deref(), Some("ENOTFOUND"));
        assert_eq!(code(lookup.resolve4("servfail.example").await).as_deref(), Some("ESERVFAIL"));
        assert_eq!(code(lookup.resolve4("refused.example").await).as_deref(), Some("EREFUSED"));
        server.abort();
    }

    #[tokio::test]
    async fn the_resolver_drives_the_verdicts_end_to_end() {
        let (address, server) = name_server(|name| match name {
            "7.100.51.198.listed.test" => Some((0, vec![[127, 0, 0, 2]])),
            "7.100.51.198.warning.test" => Some((0, vec![[127, 255, 255, 254]])),
            "7.100.51.198.clean.test" => Some((3, vec![])),
            "7.100.51.198.nodata.test" => Some((0, vec![])),
            "7.100.51.198.servfail.test" => Some((2, vec![])),
            // silent.test never answers.
            _ => None,
        })
        .await;
        let lookup = SystemDnsLookup::with_name_servers(&[address]);
        let list = |host: &str, score: i64| DnsBlocklist::new(host, host, score);

        let result =
            check_dnsbls_using("198.51.100.7", &[list("listed.test", 40), list("clean.test", 30)], &lookup)
                .await;
        assert_eq!(result.score, 40);
        assert_eq!(result.reasons, vec!["Listed in listed.test"]);
        assert!(result.evaluated);

        let result =
            check_dnsbls_using("198.51.100.7", &[list("clean.test", 30), list("nodata.test", 25)], &lookup)
                .await;
        assert_eq!(result, DnsblResult { evaluated: true, ..Default::default() });

        for unanswered in ["warning.test", "servfail.test"] {
            let result =
                check_dnsbls_using("198.51.100.7", &[list("clean.test", 30), list(unanswered, 25)], &lookup)
                    .await;
            assert_eq!(result, DnsblResult::default(), "{unanswered}");
        }

        let started = Instant::now();
        let result =
            check_dnsbls_using("198.51.100.7", &[list("listed.test", 40), list("silent.test", 25)], &lookup)
                .await;
        assert_eq!(result.score, 40);
        assert!(!result.evaluated);
        assert!(started.elapsed() >= Duration::from_millis(DNSBL_TIMEOUT_MS - 100));
        assert!(started.elapsed() < Duration::from_millis(DNSBL_TIMEOUT_MS + 1500));
        server.abort();
    }
}
