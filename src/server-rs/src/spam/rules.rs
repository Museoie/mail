//! Rule-based spam scoring (spam/rules.ts).
//!
//! Layer 2: configurable rules over the mail's content and headers. Every
//! comparison here is on text the Bun server lowercases with
//! `String.prototype.toLowerCase`; `str::to_lowercase` applies the same
//! Unicode mappings, which `tests/spam_golden.rs` holds it to.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::json;

use crate::logger;
use crate::spam::types::{truthy, EmailContext, SpamRule};

/// URL shortener domains commonly used in spam.
const URL_SHORTENERS: [&str; 11] = [
    "bit.ly",
    "tinyurl.com",
    "t.co",
    "goo.gl",
    "ow.ly",
    "is.gd",
    "buff.ly",
    "adf.ly",
    "cutt.ly",
    "rebrand.ly",
    "short.link",
];

/// `/<[^>]*>/g`: a tag, which may be empty (`<>`) and may span lines.
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new("<[^>]*>").expect("the tag pattern compiles"));

/// The text the content rules read: the plain body, or the HTML body with
/// its tags replaced by spaces when there is no plain body.
fn text_content(email: &EmailContext) -> Cow<'_, str> {
    if let Some(text) = truthy(&email.text) {
        return Cow::Borrowed(text);
    }
    match email.html.as_deref() {
        Some(html) => TAG.replace_all(html, " "),
        None => Cow::Borrowed(""),
    }
}

/// Whether more than half of the ASCII letters are uppercase. Fewer than five
/// letters is too little to judge.
fn has_excessive_uppercase(text: &str) -> bool {
    let letters = text.bytes().filter(u8::is_ascii_alphabetic).count();
    if letters < 5 {
        return false;
    }
    let uppercase = text.bytes().filter(u8::is_ascii_uppercase).count();
    uppercase as f64 / letters as f64 > 0.5
}

/// Whether the text names a URL shortener as a complete host.
///
/// A plain substring search finds `t.co` inside `wealthfront.com`,
/// `marriott.com` and `att.com`. The character before a match must therefore
/// not continue a host name (letter, digit, dot, hyphen), and the character
/// after it must not continue a label (letter, digit, hyphen) — the pattern
/// `(?:^|[^a-z0-9.-])<shortener>(?![a-z0-9-])` on the lowercased text.
fn contains_url_shortener(text: &str) -> bool {
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    URL_SHORTENERS.iter().any(|shortener| {
        let mut from = 0;
        while let Some(offset) = lower[from..].find(shortener) {
            let start = from + offset;
            let end = start + shortener.len();
            // A byte outside ASCII belongs to a character neither class holds.
            let before_ok =
                start == 0 || !matches!(bytes[start - 1], b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-');
            let after_ok = end == bytes.len() || !matches!(bytes[end], b'a'..=b'z' | b'0'..=b'9' | b'-');
            if before_ok && after_ok {
                return true;
            }
            from = start + 1;
        }
        false
    })
}

/// Whether the mail reads like marketing but offers no way out.
fn is_marketing_without_unsubscribe(email: &EmailContext) -> bool {
    let text = text_content(email).to_lowercase();
    let has_marketing_indicators =
        ["special offer", "limited time", "act now", "buy now", "click here", "free gift", "exclusive deal"]
            .iter()
            .any(|phrase| text.contains(phrase));
    let has_unsubscribe = ["unsubscribe", "opt out", "opt-out", "manage preferences"]
        .iter()
        .any(|phrase| text.contains(phrase));
    has_marketing_indicators && !has_unsubscribe
}

/// Whether the lowercased content holds any of the phrases.
fn content_has_any(email: &EmailContext, phrases: &[&str]) -> bool {
    let text = text_content(email).to_lowercase();
    phrases.iter().any(|phrase| text.contains(phrase))
}

/// What `String.prototype.trim` strips: ECMAScript WhiteSpace and
/// LineTerminator. Unlike `char::is_whitespace` it includes U+FEFF and
/// excludes U+0085.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// The second `@`-separated component, lowercased: `address.split("@")[1]?.toLowerCase()`.
fn second_component(address: &str) -> Option<String> {
    address.split('@').nth(1).map(str::to_lowercase)
}

static DEFAULT_RULES: LazyLock<Vec<SpamRule>> = LazyLock::new(|| {
    vec![
        SpamRule::new("missing-from", "Missing From header", 20, |email| {
            truthy(&email.from_address).is_none() && truthy(&email.from_name).is_none()
        }),
        SpamRule::new("reply-to-mismatch", "Reply-To differs from From", 15, |email| {
            let (Some(reply_to), Some(from)) = (truthy(&email.reply_to_address), truthy(&email.from_address))
            else {
                return false;
            };
            // Different domains is suspicious.
            second_component(from) != second_component(reply_to)
        }),
        SpamRule::new("excessive-caps-subject", "Subject >50% uppercase", 10, |email| {
            has_excessive_uppercase(email.subject.as_deref().unwrap_or(""))
        }),
        SpamRule::new("url-shortener", "URL shorteners in body", 25, |email| {
            contains_url_shortener(&text_content(email))
        }),
        SpamRule::new(
            "marketing-no-unsubscribe",
            "Marketing without unsubscribe",
            15,
            is_marketing_without_unsubscribe,
        ),
        SpamRule::new("empty-subject", "Empty subject line", 10, |email| {
            truthy(&email.subject).is_none_or(|subject| subject.chars().all(is_js_whitespace))
        }),
        SpamRule::new("excessive-exclamation", "Excessive exclamation marks in subject", 10, |email| {
            let subject = email.subject.as_deref().unwrap_or("");
            subject.bytes().filter(|byte| *byte == b'!').count() >= 3
        }),
        SpamRule::new("suspicious-phrases", "Contains suspicious spam phrases", 15, |email| {
            content_has_any(
                email,
                &[
                    "congratulations you have won",
                    "you are a winner",
                    "claim your prize",
                    "urgent action required",
                    "your account will be suspended",
                    "verify your account immediately",
                    "nigerian prince",
                    "wire transfer",
                    "lottery winner",
                ],
            )
        }),
        SpamRule::new("cryptocurrency-spam", "Cryptocurrency spam indicators", 20, |email| {
            content_has_any(
                email,
                &[
                    "bitcoin investment",
                    "crypto millionaire",
                    "guaranteed returns",
                    "double your bitcoin",
                    "free crypto",
                ],
            )
        }),
        SpamRule::new("html-only-no-text", "HTML-only email with no text alternative", 5, |email| {
            truthy(&email.html).is_some() && truthy(&email.text).is_none()
        }),
    ]
});

/// The default spam detection rules (`DEFAULT_RULES`), in evaluation order.
/// Each checks one spam indicator and assigns a score.
pub fn default_rules() -> &'static [SpamRule] {
    &DEFAULT_RULES
}

/// The rules that matched a mail and the sum of their scores.
#[derive(Debug, Clone)]
pub struct RuleEvaluation<'a> {
    pub score: i64,
    /// In the order the rules were given.
    pub matched_rules: Vec<&'a SpamRule>,
}

/// Runs every rule against a mail. A rule whose check fails is logged and
/// counts as no match; the others still run.
///
/// ```ignore
/// let result = evaluate_rules(&email, default_rules());
/// let reasons: Vec<&str> = result.matched_rules.iter().map(|rule| rule.name.as_str()).collect();
/// ```
pub fn evaluate_rules<'a>(email: &EmailContext, rules: &'a [SpamRule]) -> RuleEvaluation<'a> {
    let mut matched_rules = Vec::new();
    let mut score = 0;

    for rule in rules {
        match (rule.check)(email) {
            Ok(true) => {
                matched_rules.push(rule);
                score += rule.score;
            }
            Ok(false) => {}
            Err(error) => {
                logger::warn_with(format!("[SpamFilter] Rule '{}' threw error", rule.id), json!({}), &error);
            }
        }
    }

    RuleEvaluation { score, matched_rules }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(fields: &[(&str, &str)]) -> EmailContext {
        let mut email = EmailContext::default();
        for (field, value) in fields {
            let value = Some(value.to_string());
            match *field {
                "from" => email.from_address = value,
                "from_name" => email.from_name = value,
                "reply_to" => email.reply_to_address = value,
                "subject" => email.subject = value,
                "text" => email.text = value,
                "html" => email.html = value,
                other => panic!("unknown field {other}"),
            }
        }
        email
    }

    fn matched(email: &EmailContext, id: &str) -> bool {
        evaluate_rules(email, default_rules()).matched_rules.iter().any(|rule| rule.id == id)
    }

    #[test]
    fn missing_from_flags_emails_without_from_address() {
        assert!(matched(&email(&[("subject", "Test"), ("text", "Hello")]), "missing-from"));
    }

    #[test]
    fn missing_from_does_not_flag_emails_with_from_address() {
        let email = email(&[("from", "test@example.com"), ("subject", "Test"), ("text", "Hello")]);
        assert!(!matched(&email, "missing-from"));
    }

    #[test]
    fn reply_to_mismatch_flags_a_reply_to_on_another_domain() {
        let email = email(&[
            ("from", "sender@company.com"),
            ("reply_to", "hidden@phishing.com"),
            ("subject", "Test"),
            ("text", "Hello"),
        ]);
        assert!(matched(&email, "reply-to-mismatch"));
    }

    #[test]
    fn reply_to_mismatch_does_not_flag_a_reply_to_on_the_from_domain() {
        let email = email(&[
            ("from", "sender@company.com"),
            ("reply_to", "support@company.com"),
            ("subject", "Test"),
            ("text", "Hello"),
        ]);
        assert!(!matched(&email, "reply-to-mismatch"));
    }

    #[test]
    fn excessive_caps_flags_subjects_over_half_uppercase() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "THIS IS ALL CAPS Message"),
            ("text", "Hello"),
        ]);
        assert!(matched(&email, "excessive-caps-subject"));
    }

    #[test]
    fn excessive_caps_does_not_flag_normal_subjects() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "This is a normal subject line"),
            ("text", "Hello"),
        ]);
        assert!(!matched(&email, "excessive-caps-subject"));
    }

    #[test]
    fn url_shortener_flags_bit_ly_links() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "Check this out"),
            ("text", "Click here: https://bit.ly/abc123"),
        ]);
        assert!(matched(&email, "url-shortener"));
    }

    #[test]
    fn url_shortener_does_not_flag_normal_urls() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "Check this out"),
            ("text", "Visit our website: https://example.com/page"),
        ]);
        assert!(!matched(&email, "url-shortener"));
    }

    #[test]
    fn url_shortener_does_not_flag_a_shortener_substring_inside_a_host() {
        for (label, text) in [
            ("wealthfront.com (contains 't.co')", "https://email.wealthfront.com/ls/click?upn=abc"),
            ("marriott.com (contains 't.co')", "https://www.marriott.com/hotels/reservation"),
            ("att.com (contains 't.co')", "Sign in: https://www.att.com/myaccount"),
            ("scott.com (contains 't.co')", "Check https://scott.com/orders"),
            ("abbott.com (contains 't.co')", "https://abbott.com/labs"),
            ("elliot.com (contains 't.co')", "https://elliot.com/products"),
        ] {
            let email = email(&[
                ("from", "no-reply@em.wealthfront.com"),
                ("subject", "Account update"),
                ("text", text),
            ]);
            assert!(!matched(&email, "url-shortener"), "{label}");
        }
    }

    #[test]
    fn url_shortener_flags_actual_shorteners() {
        for (label, text) in [
            ("t.co", "https://t.co/abc123"),
            ("goo.gl", "Map: https://goo.gl/maps/xyz"),
            ("tinyurl.com", "https://tinyurl.com/foobar"),
            ("bare bit.ly without protocol", "Click bit.ly/scam now"),
            ("ow.ly with surrounding punctuation", "Link (https://ow.ly/abc)."),
        ] {
            let email = email(&[("from", "test@example.com"), ("subject", "Check this out"), ("text", text)]);
            assert!(matched(&email, "url-shortener"), "{label}");
        }
    }

    #[test]
    fn url_shortener_flags_when_shortener_and_legit_url_coexist() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "Update"),
            ("text", "Visit https://wealthfront.com or https://bit.ly/foo"),
        ]);
        assert!(matched(&email, "url-shortener"));
    }

    #[test]
    fn marketing_flags_marketing_without_unsubscribe() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "Special Offer"),
            ("text", "Limited time offer! Buy now and save 50%! Act now before it's too late!"),
        ]);
        assert!(matched(&email, "marketing-no-unsubscribe"));
    }

    #[test]
    fn marketing_does_not_flag_marketing_with_unsubscribe() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "Special Offer"),
            ("text", "Limited time offer! Buy now and save 50%! Click here to unsubscribe."),
        ]);
        assert!(!matched(&email, "marketing-no-unsubscribe"));
    }

    #[test]
    fn suspicious_phrases_flags_lottery_scam_phrases() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "Winner Notification"),
            ("text", "Congratulations you have won the lottery! Claim your prize now!"),
        ]);
        assert!(matched(&email, "suspicious-phrases"));
    }

    #[test]
    fn suspicious_phrases_does_not_flag_normal_emails() {
        let email = email(&[
            ("from", "test@example.com"),
            ("subject", "Meeting Tomorrow"),
            ("text", "Let's meet at 3pm to discuss the project."),
        ]);
        assert!(!matched(&email, "suspicious-phrases"));
    }

    #[test]
    fn accumulates_scores_from_multiple_rules() {
        // No from address (+20); >50% caps (+10) and 3+ exclamation marks (+10);
        // URL shortener (+25) and a suspicious phrase (+15).
        let email = email(&[
            ("subject", "URGENT!!!! YOU HAVE WON!!!!"),
            ("text", "Click bit.ly/scam now! Congratulations you have won!"),
        ]);
        let result = evaluate_rules(&email, default_rules());
        assert!(result.score >= 80);
        assert!(result.matched_rules.len() > 3);
    }

    #[test]
    fn returns_zero_score_for_clean_emails() {
        let email = email(&[
            ("from", "colleague@work.com"),
            ("subject", "Quick question about the report"),
            ("text", "Hi, can you send me the latest version of the quarterly report? Thanks!"),
        ]);
        let result = evaluate_rules(&email, default_rules());
        assert_eq!(result.score, 0);
        assert!(result.matched_rules.is_empty());
    }

    // Beyond the TypeScript suite: the seams this port adds.

    #[test]
    fn a_failing_rule_is_skipped_and_the_others_still_run() {
        let rules = vec![
            SpamRule::fallible("broken", "Broken", 100, |_| Err(anyhow::anyhow!("boom"))),
            SpamRule::new("always", "Always", 7, |_| true),
        ];
        let result = evaluate_rules(&EmailContext::default(), &rules);
        assert_eq!(result.score, 7);
        assert_eq!(result.matched_rules.len(), 1);
        assert_eq!(result.matched_rules[0].id, "always");
    }

    #[test]
    fn default_rules_keep_their_order_names_and_scores() {
        let rules: Vec<(&str, &str, i64)> =
            default_rules().iter().map(|rule| (rule.id.as_str(), rule.name.as_str(), rule.score)).collect();
        assert_eq!(
            rules,
            vec![
                ("missing-from", "Missing From header", 20),
                ("reply-to-mismatch", "Reply-To differs from From", 15),
                ("excessive-caps-subject", "Subject >50% uppercase", 10),
                ("url-shortener", "URL shorteners in body", 25),
                ("marketing-no-unsubscribe", "Marketing without unsubscribe", 15),
                ("empty-subject", "Empty subject line", 10),
                ("excessive-exclamation", "Excessive exclamation marks in subject", 10),
                ("suspicious-phrases", "Contains suspicious spam phrases", 15),
                ("cryptocurrency-spam", "Cryptocurrency spam indicators", 20),
                ("html-only-no-text", "HTML-only email with no text alternative", 5),
            ]
        );
    }

    #[test]
    fn trim_whitespace_is_the_javascript_set() {
        assert!(is_js_whitespace('\u{FEFF}'));
        assert!(!is_js_whitespace('\u{0085}'));
        assert!(matched(&email(&[("from", "a@b.c"), ("subject", "\u{FEFF}\u{3000} \t")]), "empty-subject"));
        assert!(!matched(&email(&[("from", "a@b.c"), ("subject", "\u{0085}")]), "empty-subject"));
    }
}
