//! Types shared by the spam filter (spam/types.ts).
//!
//! Scores are whole numbers: every rule, blocklist and classifier verdict
//! scores in integers, and `mails.spam_score` is an `INTEGER` column.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Which layer flagged the mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlaggedBy {
    Allowlist,
    Dnsbl,
    Rules,
    Classifier,
}

impl FlaggedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            FlaggedBy::Allowlist => "allowlist",
            FlaggedBy::Dnsbl => "dnsbl",
            FlaggedBy::Rules => "rules",
            FlaggedBy::Classifier => "classifier",
        }
    }
}

/// Result of a spam check. Serializes with the field names and order the Bun
/// server's `SpamCheckResult` has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpamCheckResult {
    /// Cumulative spam score (higher = more likely spam).
    pub score: i64,
    /// Human-readable reasons for the score.
    pub reasons: Vec<String>,
    /// Whether the mail reaches the spam threshold.
    pub is_spam: bool,
    /// Which layer flagged the mail, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flagged_by: Option<FlaggedBy>,
}

/// The predicate of a rule. An `Err` is what a throwing check is in
/// JavaScript: the rule is logged and skipped, the others still run.
pub type RuleCheck = Arc<dyn Fn(&EmailContext) -> anyhow::Result<bool> + Send + Sync>;

/// Rule definition for the rule engine.
#[derive(Clone)]
pub struct SpamRule {
    /// Unique rule identifier.
    pub id: String,
    /// Human-readable rule name; it is the reason a match records.
    pub name: String,
    /// Points added to the spam score when the rule matches.
    pub score: i64,
    /// Whether the rule matches.
    pub check: RuleCheck,
}

impl SpamRule {
    /// A rule whose check cannot fail.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        score: i64,
        check: impl Fn(&EmailContext) -> bool + Send + Sync + 'static,
    ) -> SpamRule {
        SpamRule::fallible(id, name, score, move |email| Ok(check(email)))
    }

    /// A rule whose check can fail; a failure is logged and counts as no match.
    pub fn fallible(
        id: impl Into<String>,
        name: impl Into<String>,
        score: i64,
        check: impl Fn(&EmailContext) -> anyhow::Result<bool> + Send + Sync + 'static,
    ) -> SpamRule {
        SpamRule { id: id.into(), name: name.into(), score, check: Arc::new(check) }
    }
}

impl fmt::Debug for SpamRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpamRule")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("score", &self.score)
            .finish_non_exhaustive()
    }
}

/// Email context passed to the spam layers for evaluation.
///
/// A field that is `None` and a field that is an empty string read the same
/// everywhere, as `undefined` and `""` are both falsy in the Bun server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmailContext {
    /// From header address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_address: Option<String>,
    /// Envelope sender from the SMTP MAIL FROM command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope_from_address: Option<String>,
    /// From header display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
    /// Reply-To address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_address: Option<String>,
    /// Email subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Plain text body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// HTML body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// Sending IP address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_address: Option<String>,
    /// All headers as key-value pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

/// The value when it is truthy in JavaScript: present and not empty.
pub(crate) fn truthy(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

/// Allowlist entry for trusted senders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowlistEntry {
    pub id: String,
    pub user_id: String,
    /// Email pattern: exact (`user@example.com`) or domain wildcard (`*@example.com`).
    pub pattern: String,
    pub created_at: String,
}

/// DNS blocklist configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsBlocklist {
    /// Blocklist hostname, e.g. `zen.spamhaus.org`.
    pub hostname: String,
    /// Human-readable name.
    pub name: String,
    /// Points added when the address is listed.
    pub score: i64,
}

impl DnsBlocklist {
    pub fn new(hostname: impl Into<String>, name: impl Into<String>, score: i64) -> DnsBlocklist {
        DnsBlocklist { hostname: hostname.into(), name: name.into(), score }
    }
}

/// Configuration for the spam filter service. `Default` is the configuration
/// the Bun server runs with (`DEFAULT_CONFIG` in spam/service.ts); override a
/// field with struct update syntax, as a `Partial<SpamFilterConfig>` does.
///
/// ```ignore
/// let config = SpamFilterConfig { enable_dnsbl: false, ..Default::default() };
/// ```
#[derive(Debug, Clone)]
pub struct SpamFilterConfig {
    /// Score at which a mail is marked as spam (default: 50).
    pub spam_threshold: i64,
    /// Whether to ask the DNS blocklists (default: true).
    pub enable_dnsbl: bool,
    /// DNS blocklists to ask.
    pub dnsbls: Vec<DnsBlocklist>,
    /// Whether to run the rule engine (default: true).
    pub enable_rules: bool,
    /// Rules evaluated after the default rules.
    pub custom_rules: Vec<SpamRule>,
}
