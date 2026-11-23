//! The per-user Naive Bayes classifier (spam/classifier.ts).
//!
//! Layer 3 of the spam filter. It learns from user feedback (mark as spam /
//! not spam) to adapt to each user's own spam.
//!
//! Algorithm: Naive Bayes over the distinct words of subject and body, with
//! add-one smoothing per word (`(count + 1) / (documents + 2)`), summed in log
//! space. The score is the spam probability scaled to 0–100, matching
//! `SpamFilterConfig::spam_threshold`.
//!
//! The classifier only fires when enough training data exists; below that it
//! scores 0 so a new account sees no false positives.
//!
//! Tokens are primary-key material shared with the Bun server on one
//! database, and the arithmetic decides which side of the threshold a mail
//! lands on, so both are reproduced operation for operation and held to the
//! Bun server's output by `tests/spam_golden.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::json;

use crate::postgres::repositories::spam_training::{self, DocCounts, WordCounts};
use crate::spam::types::{truthy, EmailContext};
use crate::{js, logger};

/// Minimum number of spam + ham documents before the classifier will score.
const MIN_TRAINING_DOCS: i64 = 5;

/// Maximum number of distinct words to extract from a single document.
const MAX_WORDS_PER_DOCUMENT: usize = 500;

/// Words shorter than this are ignored (stop-word-like).
const MIN_WORD_LENGTH: usize = 3;

/// `/<[^>]+>/g`: a tag with at least one character between the brackets.
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new("<[^>]+>").expect("the tag pattern compiles"));

/// Splits text into lowercase words for feature extraction: HTML tags are
/// replaced by spaces, the rest is lowercased, and every run of three or more
/// letters `a`–`z` is a word. Distinct words only, in order of first
/// appearance, capped at 500 to keep the training write bounded.
///
/// A word has no upper length: one over the 100 characters the `word` column
/// holds is returned like any other, and fails the training statement.
///
/// ```ignore
/// tokenize("<b>Click</b> here for 50% OFF, click now"); // ["click", "here", "for", "off", "now"]
/// ```
pub fn tokenize(text: &str) -> Vec<String> {
    let stripped = TAG.replace_all(text, " ");
    // `toLowerCase` first: U+212A KELVIN SIGN becomes `k` and joins a word,
    // U+0130 becomes `i` followed by a combining dot that ends one.
    let lowered = stripped.to_lowercase();

    let mut seen: HashSet<&str> = HashSet::new();
    let mut unique: Vec<String> = Vec::new();
    let bytes = lowered.as_bytes();
    let mut index = 0;
    while index < bytes.len() && unique.len() < MAX_WORDS_PER_DOCUMENT {
        if !bytes[index].is_ascii_lowercase() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_lowercase() {
            index += 1;
        }
        // A run of ASCII letters starts and ends on character boundaries.
        let word = &lowered[start..index];
        if word.len() >= MIN_WORD_LENGTH && seen.insert(word) {
            unique.push(word.to_string());
        }
    }
    unique
}

/// The tokens of a mail: subject, text body and HTML body, joined by spaces
/// and tokenized as one text.
pub fn extract_tokens(email: &EmailContext) -> Vec<String> {
    let parts: Vec<&str> =
        [&email.subject, &email.text, &email.html].into_iter().filter_map(truthy).collect();
    tokenize(&parts.join(" "))
}

/// The storage the classifier reads and writes. Every method defaults to the
/// `spam_training` repository; a test overrides the ones it wants to answer
/// itself.
///
/// ```ignore
/// struct NoData;
/// impl ClassifierDeps for NoData {
///     fn get_classifier_doc_counts<'a>(&'a self, _user_id: &'a str) -> BoxFuture<'a, Result<DocCounts>> {
///         Box::pin(async { Ok(DocCounts::default()) })
///     }
/// }
/// let result = classify_email_using("user", &email, &NoData).await; // score 0
/// ```
pub trait ClassifierDeps: Send + Sync {
    fn train_classifier<'a>(
        &'a self,
        user_id: &'a str,
        words: &'a [String],
        is_spam: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Ok(spam_training::train_classifier(user_id, words, is_spam).await?) })
    }

    fn get_classifier_doc_counts<'a>(&'a self, user_id: &'a str) -> BoxFuture<'a, Result<DocCounts>> {
        Box::pin(async move { Ok(spam_training::get_classifier_doc_counts(user_id).await?) })
    }

    fn get_word_counts<'a>(
        &'a self,
        user_id: &'a str,
        words: &'a [String],
    ) -> BoxFuture<'a, Result<HashMap<String, WordCounts>>> {
        Box::pin(async move { Ok(spam_training::get_word_counts(user_id, words).await?) })
    }
}

/// The classifier's production wiring: the `spam_training` table.
#[derive(Debug, Clone, Copy, Default)]
pub struct DatabaseClassifierDeps;

impl ClassifierDeps for DatabaseClassifierDeps {}

/// Trains the classifier of a user with a mail they labeled.
///
/// `is_spam` is true when the user marked it as spam, false for "not spam".
/// A mail with no tokens trains nothing. A failed write is the caller's to
/// handle, as it is in the Bun server.
pub async fn train_with_email(user_id: &str, email: &EmailContext, is_spam: bool) -> Result<()> {
    train_with_email_using(user_id, email, is_spam, &DatabaseClassifierDeps).await
}

/// [`train_with_email`] against the given storage.
pub async fn train_with_email_using(
    user_id: &str,
    email: &EmailContext,
    is_spam: bool,
    deps: &dyn ClassifierDeps,
) -> Result<()> {
    let words = extract_tokens(email);
    if words.is_empty() {
        logger::warn(
            "[Classifier] No tokens extracted from email — skipping training",
            json!({ "userId": user_id }),
        );
        return Ok(());
    }
    deps.train_classifier(user_id, &words, is_spam).await?;
    logger::info(
        "[Classifier] Training sample recorded",
        json!({ "userId": user_id, "isSpam": is_spam, "wordCount": words.len() }),
    );
    Ok(())
}

/// The classifier's verdict on one mail.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifierResult {
    /// The spam probability scaled to a whole number in 0–100; 0 when there is
    /// not enough training data. It is a float because the Bun server's is:
    /// training counts corrupted below zero make it NaN there, and here.
    pub score: f64,
    /// Set only when the verdict leans spam (score of 50 or more).
    pub reason: Option<String>,
}

impl ClassifierResult {
    /// No verdict: not enough data, nothing to read, or a failed lookup.
    pub fn none() -> ClassifierResult {
        ClassifierResult { score: 0.0, reason: None }
    }
}

/// `Math.max` of two numbers: NaN when either is, where `f64::max` would
/// return the other one.
fn math_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b || (a == b && a.is_sign_positive()) {
        a
    } else {
        b
    }
}

/// `Math.round`: to the nearest integer, a tie going up.
fn math_round(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// Scores a mail with the user's trained classifier.
///
/// Returns 0 with no reason when there is not enough training data, when the
/// mail has no tokens, or when the lookup fails; a failure is logged, never
/// returned.
pub async fn classify_email(user_id: &str, email: &EmailContext) -> ClassifierResult {
    classify_email_using(user_id, email, &DatabaseClassifierDeps).await
}

/// [`classify_email`] against the given storage.
pub async fn classify_email_using(
    user_id: &str,
    email: &EmailContext,
    deps: &dyn ClassifierDeps,
) -> ClassifierResult {
    match classify(user_id, email, deps).await {
        Ok(result) => result,
        Err(error) => {
            logger::warn_with("[Classifier] Classification failed", json!({}), &error);
            ClassifierResult::none()
        }
    }
}

async fn classify(
    user_id: &str,
    email: &EmailContext,
    deps: &dyn ClassifierDeps,
) -> Result<ClassifierResult> {
    let DocCounts { spam_docs, ham_docs } = deps.get_classifier_doc_counts(user_id).await?;
    let (spam_docs, ham_docs) = (spam_docs as f64, ham_docs as f64);
    let total_docs = spam_docs + ham_docs;

    if total_docs < MIN_TRAINING_DOCS as f64 || spam_docs == 0.0 || ham_docs == 0.0 {
        // Not enough data to make a reliable prediction.
        return Ok(ClassifierResult::none());
    }

    let words = extract_tokens(email);
    if words.is_empty() {
        return Ok(ClassifierResult::none());
    }

    // Prior probabilities, in log space to avoid underflow.
    let log_prior_spam = (spam_docs / total_docs).ln();
    let log_prior_ham = (ham_docs / total_docs).ln();

    // Counts of the words present in this mail.
    let word_count_map = deps.get_word_counts(user_id, &words).await?;

    // P(word | class) is approximated per document: (count + 1) / (documents + 2).
    let mut log_p_spam = log_prior_spam;
    let mut log_p_ham = log_prior_ham;

    for word in &words {
        let counts = word_count_map.get(word).copied().unwrap_or_default();
        let spam_word_count = counts.spam_count as f64 + 1.0;
        let ham_word_count = counts.ham_count as f64 + 1.0;
        let spam_denom = spam_docs + 2.0;
        let ham_denom = ham_docs + 2.0;

        log_p_spam += (spam_word_count / spam_denom).ln();
        log_p_ham += (ham_word_count / ham_denom).ln();
    }

    // P(spam | words) = exp(logPSpam) / (exp(logPSpam) + exp(logPHam)), with
    // the larger exponent subtracted from both for numerical stability.
    let max_log = math_max(log_p_spam, log_p_ham);
    let exp_spam = (log_p_spam - max_log).exp();
    let exp_ham = (log_p_ham - max_log).exp();
    let p_spam = exp_spam / (exp_spam + exp_ham);

    // Scaled to [0, 100] to match the spam threshold.
    let score = math_round(p_spam * 100.0);

    let reason = (score >= 50.0).then(|| {
        format!(
            "Bayesian classifier: {}% spam probability ({} spam / {} ham documents trained)",
            js::number_to_string(score),
            js::number_to_string(spam_docs),
            js::number_to_string(ham_docs),
        )
    });

    Ok(ClassifierResult { score, reason })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;

    /// Answers from fixed counts; `None` doc counts is a failing lookup.
    #[derive(Default)]
    struct Stub {
        doc_counts: Option<DocCounts>,
        word_counts: HashMap<String, WordCounts>,
        trained: Mutex<Vec<(String, Vec<String>, bool)>>,
    }

    impl Stub {
        fn counts(spam_docs: i64, ham_docs: i64, words: &[(&str, i64, i64)]) -> Stub {
            Stub {
                doc_counts: Some(DocCounts { spam_docs, ham_docs }),
                word_counts: words
                    .iter()
                    .map(|(word, spam_count, ham_count)| {
                        (word.to_string(), WordCounts { spam_count: *spam_count, ham_count: *ham_count })
                    })
                    .collect(),
                trained: Mutex::default(),
            }
        }
    }

    impl ClassifierDeps for Stub {
        fn train_classifier<'a>(
            &'a self,
            user_id: &'a str,
            words: &'a [String],
            is_spam: bool,
        ) -> BoxFuture<'a, Result<()>> {
            self.trained.lock().unwrap().push((user_id.to_string(), words.to_vec(), is_spam));
            Box::pin(async { Ok(()) })
        }

        fn get_classifier_doc_counts<'a>(&'a self, _user_id: &'a str) -> BoxFuture<'a, Result<DocCounts>> {
            Box::pin(async move { self.doc_counts.ok_or_else(|| anyhow!("DB error")) })
        }

        fn get_word_counts<'a>(
            &'a self,
            _user_id: &'a str,
            _words: &'a [String],
        ) -> BoxFuture<'a, Result<HashMap<String, WordCounts>>> {
            Box::pin(async move { Ok(self.word_counts.clone()) })
        }
    }

    fn subject(subject: &str) -> EmailContext {
        EmailContext { subject: Some(subject.to_string()), ..Default::default() }
    }

    fn has(tokens: &[String], word: &str) -> bool {
        tokens.iter().any(|token| token == word)
    }

    // --- tokenize ---

    #[test]
    fn tokenize_lowercases_and_extracts_words_of_three_or_more_letters() {
        let result = tokenize("Hello World foo");
        assert!(has(&result, "hello"));
        assert!(has(&result, "world"));
        assert!(has(&result, "foo"));
    }

    #[test]
    fn tokenize_strips_html_tags_before_tokenizing() {
        let result = tokenize("<b>Click</b> here for <a href='x'>offers</a>");
        assert!(has(&result, "click"));
        assert!(has(&result, "here"));
        assert!(has(&result, "offers"));
        assert!(!has(&result, "href"));
        assert!(!has(&result, "<b>"));
    }

    #[test]
    fn tokenize_filters_out_words_shorter_than_three_characters() {
        let result = tokenize("hi me it go the to");
        assert!(result.iter().all(|word| word.len() >= 3));
        assert!(has(&result, "the"));
    }

    #[test]
    fn tokenize_deduplicates_words() {
        let result = tokenize("spam spam spam");
        assert_eq!(result.iter().filter(|word| *word == "spam").count(), 1);
    }

    #[test]
    fn tokenize_ignores_numbers_and_punctuation() {
        let result = tokenize("Buy now! 50% off. Call 1-800-SPAM today.");
        for word in ["buy", "now", "off", "call", "spam", "today"] {
            assert!(has(&result, word), "{word}");
        }
        assert!(!has(&result, "800"));
        assert!(!has(&result, "50"));
    }

    #[test]
    fn tokenize_returns_nothing_for_empty_input() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn tokenize_caps_output_at_500_words() {
        let many_words: Vec<String> = (0..600).map(|i| format!("word{i}")).collect();
        let result = tokenize(&many_words.join(" "));
        assert!(result.len() <= 500);
    }

    // --- extract_tokens ---

    #[test]
    fn extract_tokens_combines_subject_text_and_html() {
        let tokens = extract_tokens(&EmailContext {
            subject: Some("Win prizes".into()),
            text: Some("Click here to claim your reward".into()),
            html: Some("<p>Visit our <b>website</b></p>".into()),
            ..Default::default()
        });
        for word in ["win", "prizes", "click", "claim", "visit", "website"] {
            assert!(has(&tokens, word), "{word}");
        }
    }

    #[test]
    fn extract_tokens_works_when_only_subject_is_present() {
        let tokens = extract_tokens(&subject("Hello there"));
        assert!(has(&tokens, "hello"));
        assert!(has(&tokens, "there"));
    }

    #[test]
    fn extract_tokens_returns_nothing_when_all_fields_are_empty() {
        assert!(extract_tokens(&EmailContext::default()).is_empty());
    }

    // --- train_with_email ---

    #[tokio::test]
    async fn train_with_email_calls_train_classifier_with_extracted_tokens() {
        let stub = Stub::default();
        let email = EmailContext {
            subject: Some("Buy cheap meds".into()),
            text: Some("Click here now".into()),
            ..Default::default()
        };
        train_with_email_using("user1", &email, true, &stub).await.unwrap();

        let calls = stub.trained.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (user_id, words, is_spam) = &calls[0];
        assert_eq!(user_id, "user1");
        assert!(*is_spam);
        assert!(has(words, "cheap"));
        assert!(has(words, "click"));
    }

    #[tokio::test]
    async fn train_with_email_skips_training_when_no_tokens_can_be_extracted() {
        let stub = Stub::default();
        train_with_email_using("user1", &EmailContext::default(), false, &stub).await.unwrap();
        assert!(stub.trained.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn train_with_email_trains_as_ham_when_not_spam() {
        let stub = Stub::default();
        train_with_email_using("user2", &subject("Team standup tomorrow"), false, &stub).await.unwrap();
        assert!(!stub.trained.lock().unwrap()[0].2);
    }

    // --- classify_email ---

    #[tokio::test]
    async fn classify_returns_zero_when_not_enough_training_data() {
        let result = classify_email_using("user1", &subject("Win a prize"), &Stub::counts(2, 1, &[])).await;
        assert_eq!(result.score, 0.0);
        assert_eq!(result.reason, None);
    }

    #[tokio::test]
    async fn classify_returns_zero_when_no_spam_docs_trained() {
        let result = classify_email_using("user1", &subject("Win a prize"), &Stub::counts(0, 10, &[])).await;
        assert_eq!(result.score, 0.0);
    }

    #[tokio::test]
    async fn classify_returns_zero_when_no_ham_docs_trained() {
        let result = classify_email_using("user1", &subject("Hello team"), &Stub::counts(10, 0, &[])).await;
        assert_eq!(result.score, 0.0);
    }

    #[tokio::test]
    async fn classify_returns_zero_when_email_has_no_extractable_tokens() {
        let result =
            classify_email_using("user1", &EmailContext::default(), &Stub::counts(10, 10, &[])).await;
        assert_eq!(result.score, 0.0);
    }

    #[tokio::test]
    async fn classify_assigns_high_score_to_spam_like_email_with_sufficient_training() {
        let stub = Stub::counts(20, 20, &[("win", 18, 1), ("prize", 16, 1), ("free", 15, 1)]);
        let result = classify_email_using("user1", &subject("Win a free prize today"), &stub).await;
        assert!(result.score > 50.0);
        assert!(result.reason.is_some());
    }

    #[tokio::test]
    async fn classify_assigns_low_score_to_ham_like_email_with_sufficient_training() {
        let stub = Stub::counts(20, 20, &[("standup", 0, 15), ("meeting", 1, 18), ("tomorrow", 0, 17)]);
        let result = classify_email_using("user1", &subject("Team standup meeting tomorrow"), &stub).await;
        assert!(result.score < 50.0);
        assert_eq!(result.reason, None);
    }

    #[tokio::test]
    async fn classify_returns_zero_and_no_error_when_the_database_fails() {
        let stub = Stub { doc_counts: None, ..Default::default() };
        let result = classify_email_using("user1", &subject("Hello there"), &stub).await;
        assert_eq!(result.score, 0.0);
        assert_eq!(result.reason, None);
    }

    #[tokio::test]
    async fn classify_returns_a_score_between_0_and_100() {
        let stub = Stub::counts(10, 10, &[("click", 8, 2)]);
        let result = classify_email_using("user1", &subject("Click here"), &stub).await;
        assert!(result.score >= 0.0);
        assert!(result.score <= 100.0);
    }

    // Beyond the TypeScript suite.

    #[tokio::test]
    async fn classify_words_the_reason_as_the_bun_server_does() {
        let stub = Stub::counts(20, 20, &[("win", 18, 1), ("prize", 16, 1), ("free", 15, 1)]);
        let result = classify_email_using("user1", &subject("Win a free prize today"), &stub).await;
        assert_eq!(result.score, 100.0);
        assert_eq!(
            result.reason.as_deref(),
            Some("Bayesian classifier: 100% spam probability (20 spam / 20 ham documents trained)")
        );
    }

    #[test]
    fn tokenize_keeps_first_appearance_order_and_has_no_upper_word_length() {
        let long = "a".repeat(150);
        assert_eq!(
            tokenize(&format!("zeta Alpha {long} zeta ALPHA beta")),
            vec!["zeta", "alpha", long.as_str(), "beta"]
        );
    }

    #[test]
    fn tokenize_lowercases_the_way_javascript_does() {
        // KELVIN SIGN lowercases to `k`; İ to `i` plus a combining dot.
        assert_eq!(tokenize("clic\u{212A} \u{0130}STANBUL"), vec!["click", "stanbul"]);
        assert_eq!(tokenize("naïve café ΣΟΦΟΣ über"), vec!["caf", "ber"]);
    }

    #[test]
    fn math_helpers_follow_javascript() {
        assert!(math_max(f64::NAN, 1.0).is_nan());
        assert!(math_max(1.0, f64::NAN).is_nan());
        assert_eq!(math_max(-3.0, -2.0), -2.0);
        assert_eq!(math_round(2.5), 3.0);
        assert_eq!(math_round(49.5), 50.0);
        assert_eq!(math_round(49.499_999_999_999_99), 49.0);
        assert_eq!(math_round(-2.5), -2.0);
        assert_eq!(math_round(0.499_999_999_999_999_94), 0.0);
        assert!(math_round(f64::NAN).is_nan());
    }
}
