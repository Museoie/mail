//! The spam decision for one incoming mail (spam/service.ts).
//!
//! Four layers:
//! - Layer 0: allowlist (skip trusted senders whose identity corroborates)
//! - Layer 1: DNS blocklists (Spamhaus, SpamCop, Barracuda)
//! - Layer 2: rule engine (header and content analysis)
//! - Layer 3: the user-trained classifier
//!
//! Layer 1 is evaluated before Layer 0 because a blocklisted connection
//! withdraws the Layer 0 exemption.

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::json;

use crate::logger;
use crate::postgres::repositories::spam_allowlists;
use crate::spam::alignment::is_envelope_aligned;
use crate::spam::classifier::{self, ClassifierResult};
use crate::spam::dnsbl::{self, default_dnsbls, DnsblResult};
use crate::spam::rules::{default_rules, evaluate_rules};
use crate::spam::types::{truthy, DnsBlocklist, EmailContext, FlaggedBy, SpamCheckResult, SpamFilterConfig};

/// What a spam check consults outside the mail itself. Every method defaults
/// to the real implementation; a test overrides the ones it wants to answer
/// itself. An `Err` is a thrown lookup: it is logged and that layer states no
/// opinion.
///
/// ```ignore
/// struct NeverAllowlisted;
/// impl CheckSpamDeps for NeverAllowlisted {
///     fn is_allowlisted<'a>(&'a self, _user_id: &'a str, _address: &'a str) -> BoxFuture<'a, Result<bool>> {
///         Box::pin(async { Ok(false) })
///     }
/// }
/// let result = check_spam_using(user_id, &email, &SpamFilterConfig::default(), &NeverAllowlisted).await;
/// ```
pub trait CheckSpamDeps: Send + Sync {
    /// Whether the sender address is on the user's allowlist.
    fn is_allowlisted<'a>(&'a self, user_id: &'a str, email_address: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(spam_allowlists::is_allowlisted(user_id, email_address).await?) })
    }

    /// The user's classifier's verdict on the mail.
    fn classify_email<'a>(
        &'a self,
        user_id: &'a str,
        email: &'a EmailContext,
    ) -> BoxFuture<'a, Result<ClassifierResult>> {
        Box::pin(async move { Ok(classifier::classify_email(user_id, email).await) })
    }

    /// What the blocklists say about the connecting address.
    fn check_dnsbls<'a>(
        &'a self,
        ip: &'a str,
        dnsbls: &'a [DnsBlocklist],
    ) -> BoxFuture<'a, Result<DnsblResult>> {
        Box::pin(async move { Ok(dnsbl::check_dnsbls(ip, dnsbls).await) })
    }
}

/// The production wiring: the allowlist and training tables and the system resolver.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultCheckSpamDeps;

impl CheckSpamDeps for DefaultCheckSpamDeps {}

/// The default spam filter configuration.
impl Default for SpamFilterConfig {
    fn default() -> SpamFilterConfig {
        SpamFilterConfig {
            spam_threshold: 50,
            enable_dnsbl: true,
            dnsbls: default_dnsbls().to_vec(),
            enable_rules: true,
            custom_rules: Vec::new(),
        }
    }
}

/// Why an allowlisted sender is denied the Layer 0 exemption, or `None` to grant it.
///
/// The allowlist matches the `From:` header, which the sending client writes
/// freely. Granting a total bypass on that alone hands guaranteed delivery to
/// anyone who types an allowlisted address, so the exemption additionally
/// requires the two signals the server does not take on the sender's word: an
/// envelope sender that corroborates the header, and a connection a blocklist
/// was asked about and did not list. A blocklist that was never asked — the
/// address is outside IPv4, the query timed out, the resolver was refused —
/// withholds the second signal rather than supplying it.
fn exemption_refusal(email: &EmailContext, dnsbl_score: i64, dnsbl_evaluated: bool) -> Option<&'static str> {
    if !is_envelope_aligned(email.from_address.as_deref(), email.envelope_from_address.as_deref()) {
        return Some("Allowlisted sender not confirmed by the envelope sender");
    }
    if dnsbl_score > 0 {
        return Some("Allowlisted sender arrived from a blocklisted address");
    }
    if !dnsbl_evaluated {
        return Some("Allowlisted sender arrived from an address no blocklist answered for");
    }
    None
}

/// Checks an incoming mail for spam with the default configuration and the
/// real allowlist, blocklists and classifier.
///
/// `user_id` is the recipient: the allowlist and the classifier are theirs.
/// A layer that fails is logged and skipped, so this always has an answer.
///
/// ```ignore
/// let result = check_spam(&user.id, &email).await;
/// // store result.score, result.reasons and result.is_spam with the mail
/// ```
pub async fn check_spam(user_id: &str, email: &EmailContext) -> SpamCheckResult {
    check_spam_using(user_id, email, &SpamFilterConfig::default(), &DefaultCheckSpamDeps).await
}

/// [`check_spam`] with a configuration and dependencies of the caller's choosing.
pub async fn check_spam_using(
    user_id: &str,
    email: &EmailContext,
    config: &SpamFilterConfig,
    deps: &dyn CheckSpamDeps,
) -> SpamCheckResult {
    let mut reasons: Vec<String> = Vec::new();
    let mut total_score: i64 = 0;
    let mut flagged_by: Option<FlaggedBy> = None;

    // Layer 1: DNS blocklist check
    let mut dnsbl_score: i64 = 0;
    let mut dnsbl_reasons: Vec<String> = Vec::new();
    // A layer turned off by config states no opinion, so it withholds nothing.
    // An enabled layer that never ran does, and must not read as a clearance.
    let mut dnsbl_evaluated = !config.enable_dnsbl;
    if let (true, Some(remote_address)) = (config.enable_dnsbl, truthy(&email.remote_address)) {
        match deps.check_dnsbls(remote_address, &config.dnsbls).await {
            Ok(result) => {
                dnsbl_score = result.score;
                dnsbl_reasons = result.reasons;
                dnsbl_evaluated = result.evaluated;
            }
            Err(error) => {
                logger::warn_with("[SpamFilter] DNSBL check failed", json!({}), &error);
                // Continue with other checks
            }
        }
    }

    // Layer 0: Allowlist check
    if let Some(from_address) = truthy(&email.from_address) {
        match deps.is_allowlisted(user_id, from_address).await {
            Ok(true) => match exemption_refusal(email, dnsbl_score, dnsbl_evaluated) {
                None => {
                    return SpamCheckResult {
                        score: 0,
                        reasons: vec!["Sender is allowlisted".to_string()],
                        is_spam: false,
                        flagged_by: Some(FlaggedBy::Allowlist),
                    };
                }
                Some(refusal) => reasons.push(refusal.to_string()),
            },
            Ok(false) => {}
            Err(error) => {
                logger::warn_with("[SpamFilter] Allowlist check failed", json!({}), &error);
                // Continue with other checks
            }
        }
    }

    if dnsbl_score > 0 {
        total_score += dnsbl_score;
        reasons.extend(dnsbl_reasons);
        flagged_by.get_or_insert(FlaggedBy::Dnsbl);
    }

    // Layer 2: Rule engine
    if config.enable_rules {
        // The default rules, then the custom ones: one list in the Bun server.
        let default_result = evaluate_rules(email, default_rules());
        let custom_result = evaluate_rules(email, &config.custom_rules);
        let rule_score = default_result.score + custom_result.score;
        if rule_score > 0 {
            total_score += rule_score;
            let matched_rules = default_result.matched_rules.iter().chain(&custom_result.matched_rules);
            reasons.extend(matched_rules.map(|rule| rule.name.clone()));
            flagged_by.get_or_insert(FlaggedBy::Rules);
        }
    }

    // Layer 3: Naive Bayes classifier (user-trained, per-user model)
    match deps.classify_email(user_id, email).await {
        // The classifier scores P(spam) * 100, where under 50 is a ham verdict
        // and carries no reason. Only a spam verdict contributes — otherwise a
        // ham-leaning 49 would still push the total past the threshold when
        // combined with other layers, flipping legitimate mail to spam.
        Ok(ClassifierResult { score, reason: Some(reason) }) => {
            total_score += score as i64;
            reasons.push(reason);
            flagged_by.get_or_insert(FlaggedBy::Classifier);
        }
        Ok(ClassifierResult { reason: None, .. }) => {}
        Err(error) => {
            logger::warn_with("[SpamFilter] Classifier check failed", json!({}), &error);
            // Continue — classifier failure is non-fatal
        }
    }

    let is_spam = total_score >= config.spam_threshold;

    SpamCheckResult {
        score: total_score,
        reasons,
        is_spam,
        flagged_by: if is_spam { flagged_by } else { None },
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    /// What the blocklist stub does when asked.
    #[derive(Clone)]
    enum Dnsbls {
        Answer { score: i64, reasons: Vec<String>, evaluated: bool },
        Throw,
    }

    struct Stub {
        allowed: bool,
        classifier: ClassifierResult,
        dnsbls: Dnsbls,
    }

    impl CheckSpamDeps for Stub {
        fn is_allowlisted<'a>(&'a self, _user_id: &'a str, _address: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async move { Ok(self.allowed) })
        }

        fn classify_email<'a>(
            &'a self,
            _user_id: &'a str,
            _email: &'a EmailContext,
        ) -> BoxFuture<'a, Result<ClassifierResult>> {
            Box::pin(async move { Ok(self.classifier.clone()) })
        }

        fn check_dnsbls<'a>(
            &'a self,
            _ip: &'a str,
            _dnsbls: &'a [DnsBlocklist],
        ) -> BoxFuture<'a, Result<DnsblResult>> {
            Box::pin(async move {
                match &self.dnsbls {
                    Dnsbls::Answer { score, reasons, evaluated } => Ok(DnsblResult {
                        score: *score,
                        listed_in: Vec::new(),
                        reasons: reasons.clone(),
                        evaluated: *evaluated,
                    }),
                    Dnsbls::Throw => Err(anyhow!("resolver unreachable")),
                }
            })
        }
    }

    fn stub_dnsbls(score: i64, reasons: &[&str], evaluated: bool) -> Dnsbls {
        Dnsbls::Answer {
            score,
            reasons: reasons.iter().map(|reason| reason.to_string()).collect(),
            evaluated,
        }
    }

    /// Not allowlisted, with the classifier answering as given.
    fn base_deps(score: f64, reason: Option<&str>) -> Stub {
        Stub {
            allowed: false,
            classifier: ClassifierResult { score, reason: reason.map(str::to_string) },
            dnsbls: stub_dnsbls(0, &[], true),
        }
    }

    /// Allowlisted, an untrained classifier, and a clean connecting address.
    fn allowlist_deps() -> Stub {
        Stub { allowed: true, classifier: ClassifierResult::none(), dnsbls: stub_dnsbls(0, &[], true) }
    }

    fn some(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    // The allowlisted domain a forger claims. The body scores on the rule engine
    // alone so the assertions never depend on the classifier or on a network lookup.
    fn spam_from_allowlisted_domain() -> EmailContext {
        EmailContext {
            from_address: some("spammer@ut-allow.example"),
            from_name: some("spammer@ut-allow.example"),
            subject: some("FREE MONEY WINNER ACT NOW!!!"),
            text: some("Click http://bit.ly/x to claim your free money prize winner. Act now!!!"),
            html: some("<p>Click <a href='http://bit.ly/x'>here</a> for your free money prize winner!!!</p>"),
            ..Default::default()
        }
    }

    // Two minor rules fire (reply-to-mismatch + html-only-no-text = 20 points).
    // No remote address, so the blocklist layer is skipped.
    fn subtle_ham_email() -> EmailContext {
        EmailContext {
            from_address: some("newsletter@company.com"),
            reply_to_address: some("marketing@othercompany.com"),
            from_name: some("Newsletter"),
            subject: some("Weekly newsletter"),
            html: some("<p>Read our weekly update. To unsubscribe click here.</p>"),
            ..Default::default()
        }
    }

    fn aligned_from(remote_address: Option<&str>) -> EmailContext {
        EmailContext {
            envelope_from_address: some("bounce@ut-allow.example"),
            remote_address: remote_address.map(str::to_string),
            ..spam_from_allowlisted_domain()
        }
    }

    async fn check(email: &EmailContext, deps: &Stub) -> SpamCheckResult {
        check_spam_using("user1", email, &SpamFilterConfig::default(), deps).await
    }

    fn has_reason(result: &SpamCheckResult, reason: &str) -> bool {
        result.reasons.iter().any(|candidate| candidate == reason)
    }

    // --- classifier scoring gate ---

    #[tokio::test]
    async fn ignores_a_ham_verdict_so_it_does_not_inflate_the_total() {
        let result = check(&subtle_ham_email(), &base_deps(49.0, None)).await;
        assert_eq!(result.score, 20);
        assert!(!result.is_spam);
        assert_eq!(result.flagged_by, None);
    }

    #[tokio::test]
    async fn adds_a_spam_verdict_to_the_total() {
        let reason = "Bayesian classifier: 75% spam probability (20 spam / 20 ham documents trained)";
        let result = check(&subtle_ham_email(), &base_deps(75.0, Some(reason))).await;
        // Rules 20 + classifier 75 = 95.
        assert_eq!(result.score, 95);
        assert!(result.is_spam);
        assert!(has_reason(&result, reason));
    }

    #[tokio::test]
    async fn adds_a_boundary_spam_verdict_to_the_total() {
        let clean_email = EmailContext {
            from_address: some("x@y.com"),
            subject: some("Hello there"),
            text: some("Hi friend, hope you are well."),
            ..Default::default()
        };
        let reason = "Bayesian classifier: 50% spam probability (10 spam / 10 ham documents trained)";
        let result = check(&clean_email, &base_deps(50.0, Some(reason))).await;
        assert_eq!(result.score, 50);
        assert!(result.is_spam);
        assert_eq!(result.flagged_by, Some(FlaggedBy::Classifier));
    }

    #[tokio::test]
    async fn treats_a_zero_classifier_score_as_untrained() {
        let result = check(&subtle_ham_email(), &base_deps(0.0, None)).await;
        assert_eq!(result.score, 20);
        assert!(!result.is_spam);
    }

    #[tokio::test]
    async fn does_not_flag_by_classifier_when_the_verdict_is_ham() {
        let email = EmailContext {
            from_address: some("x@y.com"),
            subject: some("Hello there"),
            text: some("hello there"),
            ..Default::default()
        };
        let result = check(&email, &base_deps(30.0, None)).await;
        assert_eq!(result.flagged_by, None);
    }

    // --- allowlist exemption requires a corroborated sender ---

    #[tokio::test]
    async fn exempts_an_allowlisted_sender_whose_envelope_sender_shares_its_domain() {
        let result = check(&aligned_from(Some("198.51.100.7")), &allowlist_deps()).await;
        assert_eq!(result.score, 0);
        assert!(!result.is_spam);
        assert_eq!(result.flagged_by, Some(FlaggedBy::Allowlist));
        assert_eq!(result.reasons, vec!["Sender is allowlisted"]);
    }

    #[tokio::test]
    async fn scores_a_forged_from_whose_envelope_sender_is_on_another_domain() {
        let email = EmailContext {
            envelope_from_address: some("envelope-sender@external.example"),
            ..spam_from_allowlisted_domain()
        };
        let result = check(&email, &allowlist_deps()).await;
        assert_eq!(result.score, 60);
        assert!(result.is_spam);
        assert_eq!(result.flagged_by, Some(FlaggedBy::Rules));
        assert!(has_reason(&result, "Allowlisted sender not confirmed by the envelope sender"));
        assert!(has_reason(&result, "Subject >50% uppercase"));
    }

    #[tokio::test]
    async fn scores_an_allowlisted_sender_that_arrived_with_no_envelope_sender() {
        let result = check(&spam_from_allowlisted_domain(), &allowlist_deps()).await;
        assert_eq!(result.score, 60);
        assert!(result.is_spam);
        assert!(has_reason(&result, "Allowlisted sender not confirmed by the envelope sender"));
    }

    #[tokio::test]
    async fn withdraws_the_exemption_when_the_connecting_address_is_blocklisted() {
        let deps = Stub { dnsbls: stub_dnsbls(40, &["Listed in Spamhaus ZEN"], true), ..allowlist_deps() };
        let result = check(&aligned_from(Some("198.51.100.7")), &deps).await;
        assert!(result.is_spam);
        assert_eq!(result.score, 100);
        assert!(has_reason(&result, "Allowlisted sender arrived from a blocklisted address"));
        assert!(has_reason(&result, "Listed in Spamhaus ZEN"));
    }

    #[tokio::test]
    async fn keeps_the_exemption_for_an_aligned_sender_on_a_clean_connecting_address() {
        let deps = Stub { dnsbls: stub_dnsbls(0, &[], true), ..allowlist_deps() };
        let result = check(&aligned_from(Some("198.51.100.7")), &deps).await;
        assert_eq!(result.score, 0);
        assert!(!result.is_spam);
    }

    #[tokio::test]
    async fn refuses_the_exemption_when_no_blocklist_could_answer_for_the_connecting_address() {
        let deps = Stub { dnsbls: stub_dnsbls(0, &[], false), ..allowlist_deps() };
        let result = check(&aligned_from(Some("2001:db8::1")), &deps).await;
        assert_eq!(result.score, 60);
        assert!(result.is_spam);
        assert!(has_reason(&result, "Allowlisted sender arrived from an address no blocklist answered for"));
    }

    #[tokio::test]
    async fn refuses_the_exemption_when_the_mail_carries_no_connecting_address() {
        let result = check(&aligned_from(None), &allowlist_deps()).await;
        assert_eq!(result.score, 60);
        assert!(result.is_spam);
        assert!(has_reason(&result, "Allowlisted sender arrived from an address no blocklist answered for"));
    }

    #[tokio::test]
    async fn refuses_the_exemption_when_the_blocklist_lookup_threw_instead_of_answering() {
        // A blocklist failure is swallowed so the other layers still run. The
        // exemption must not survive that: a lookup that never completed states
        // no opinion on the connection, and reading it as a clearance is the bypass.
        let deps = Stub { dnsbls: Dnsbls::Throw, ..allowlist_deps() };
        let result = check(&aligned_from(Some("198.51.100.7")), &deps).await;
        assert_eq!(result.score, 60);
        assert!(result.is_spam);
        assert_eq!(result.flagged_by, Some(FlaggedBy::Rules));
        assert!(has_reason(&result, "Allowlisted sender arrived from an address no blocklist answered for"));
    }

    #[tokio::test]
    async fn keeps_the_exemption_when_the_blocklist_layer_is_turned_off_by_config() {
        let config = SpamFilterConfig { enable_dnsbl: false, ..Default::default() };
        let result = check_spam_using("user1", &aligned_from(None), &config, &allowlist_deps()).await;
        assert_eq!(result.score, 0);
        assert!(!result.is_spam);
        assert_eq!(result.flagged_by, Some(FlaggedBy::Allowlist));
    }

    #[tokio::test]
    async fn still_scores_a_blocklisted_connection_that_is_not_allowlisted_at_all() {
        let deps = Stub {
            allowed: false,
            dnsbls: stub_dnsbls(40, &["Listed in Spamhaus ZEN"], true),
            ..allowlist_deps()
        };
        let result = check(&aligned_from(Some("198.51.100.7")), &deps).await;
        assert_eq!(result.score, 100);
        assert_eq!(result.flagged_by, Some(FlaggedBy::Dnsbl));
        assert!(!has_reason(&result, "Allowlisted sender arrived from a blocklisted address"));
    }

    // Beyond the TypeScript suite.

    #[test]
    fn the_default_configuration_is_the_bun_servers() {
        let config = SpamFilterConfig::default();
        assert_eq!(config.spam_threshold, 50);
        assert!(config.enable_dnsbl);
        assert!(config.enable_rules);
        assert_eq!(config.dnsbls, default_dnsbls());
        assert!(config.custom_rules.is_empty());
    }

    #[tokio::test]
    async fn custom_rules_run_after_the_default_rules() {
        let config = SpamFilterConfig {
            custom_rules: vec![crate::spam::types::SpamRule::new("custom", "Custom rule", 30, |_| true)],
            ..Default::default()
        };
        let result = check_spam_using("user1", &subtle_ham_email(), &config, &base_deps(0.0, None)).await;
        assert_eq!(result.score, 50);
        assert_eq!(
            result.reasons,
            vec!["Reply-To differs from From", "HTML-only email with no text alternative", "Custom rule"]
        );
        assert_eq!(result.flagged_by, Some(FlaggedBy::Rules));
    }

    #[test]
    fn the_result_serializes_as_the_bun_servers() {
        let flagged = SpamCheckResult {
            score: 60,
            reasons: vec!["Subject >50% uppercase".into()],
            is_spam: true,
            flagged_by: Some(FlaggedBy::Rules),
        };
        assert_eq!(
            serde_json::to_string(&flagged).unwrap(),
            r#"{"score":60,"reasons":["Subject >50% uppercase"],"isSpam":true,"flaggedBy":"rules"}"#
        );
        let clean = SpamCheckResult { score: 0, reasons: vec![], is_spam: false, flagged_by: None };
        assert_eq!(serde_json::to_string(&clean).unwrap(), r#"{"score":0,"reasons":[],"isSpam":false}"#);
    }
}
