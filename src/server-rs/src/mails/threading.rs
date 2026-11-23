//! Which stored mails form one conversation (common/threading.ts).
//!
//! Mail that carries `In-Reply-To`/`References` is threaded by those headers
//! alone: two mails are in one thread when one's Message-ID appears in the
//! other's headers, directly or through other mails in between.
//!
//! Mail with neither header — everything stored before these columns existed,
//! and mail from clients that don't send them — is matched by subject
//! instead, but only against other header-less mail: the same subject once
//! `Re:`/`Fwd:` prefixes are stripped, at least one of the two marked as a
//! reply or forward, and at least one shared participant other than the
//! user's own addresses. The user is on every mail they store, so their own
//! address says nothing about which conversation a mail belongs to; two
//! vendors' unrelated "Re: Invoice" mails share only that, and stay apart.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mime::charset::jsstr;

/// A stored mail, as far as threading needs it.
pub trait ThreadCandidate {
    fn mail_id(&self) -> &str;
    fn message_id(&self) -> &str;
    fn in_reply_to(&self) -> Option<&str>;
    fn reference_ids(&self) -> Option<&[String]>;
    fn subject(&self) -> &str;
    /// The stored `from_address` JSON: an entry or a list of entries, each
    /// with an `address`.
    #[allow(clippy::wrong_self_convention)] // Named after the column, like its neighbours.
    fn from_address(&self) -> Option<&Value>;
    fn to_address(&self) -> Option<&Value>;
    fn cc_address(&self) -> Option<&Value>;
}

/// A [`ThreadCandidate`] holding just the columns threading reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThreadMail {
    pub mail_id: String,
    pub message_id: String,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub reference_ids: Option<Vec<String>>,
    pub subject: String,
    #[serde(default)]
    pub from_address: Option<Value>,
    #[serde(default)]
    pub to_address: Option<Value>,
    #[serde(default)]
    pub cc_address: Option<Value>,
}

impl ThreadCandidate for ThreadMail {
    fn mail_id(&self) -> &str {
        &self.mail_id
    }

    fn message_id(&self) -> &str {
        &self.message_id
    }

    fn in_reply_to(&self) -> Option<&str> {
        self.in_reply_to.as_deref()
    }

    fn reference_ids(&self) -> Option<&[String]> {
        self.reference_ids.as_deref()
    }

    fn subject(&self) -> &str {
        &self.subject
    }

    fn from_address(&self) -> Option<&Value> {
        self.from_address.as_ref()
    }

    fn to_address(&self) -> Option<&Value> {
        self.to_address.as_ref()
    }

    fn cc_address(&self) -> Option<&Value> {
        self.cc_address.as_ref()
    }
}

const REPLY_WORDS: [&str; 10] = ["re", "fwd", "fw", "aw", "wg", "sv", "vs", "antw", "tr", "rv"];

/// The length of the reply and forward markers that open `subject`, with
/// the whitespace around them:
/// `/^\s*((re|fwd?|aw|wg|sv|vs|antw|tr|rv)\s*(\[\d+\])?\s*[:：]\s*)+/i`.
fn reply_prefix_length(subject: &str) -> Option<usize> {
    let skip_space = |at: usize| subject.len() - jsstr::trim_start(&subject[at..]).len();
    let mut at = skip_space(0);
    let mut matched = None;
    loop {
        let rest = &subject[at..];
        let Some(word) = REPLY_WORDS.iter().find(|word| {
            rest.len() >= word.len() && rest.as_bytes()[..word.len()].eq_ignore_ascii_case(word.as_bytes())
        }) else {
            return matched;
        };
        let mut next = skip_space(at + word.len());
        // A count, as in "Re[2]:".
        if let Some(count) = subject[next..].strip_prefix('[') {
            let digits = count.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && count[digits..].starts_with(']') {
                next = skip_space(next + digits + 2);
            }
        }
        let Some(colon) = subject[next..].chars().next().filter(|c| matches!(c, ':' | '：')) else {
            return matched;
        };
        at = skip_space(next + colon.len_utf8());
        matched = Some(at);
    }
}

/// The subject without leading reply/forward markers, case-folded.
pub fn normalize_subject(subject: &str) -> String {
    let rest = &subject[reply_prefix_length(subject).unwrap_or(0)..];
    let words: Vec<&str> = rest.split(jsstr::is_space).filter(|word| !word.is_empty()).collect();
    words.join(" ").to_lowercase()
}

/// Whether the subject marks a reply or a forward.
pub fn is_reply_subject(subject: &str) -> bool {
    reply_prefix_length(subject).is_some()
}

/// A Message-ID without its angle brackets. Rows stored over SMTP keep the
/// brackets; rows appended over IMAP store the bare id. Comparing the bare
/// form matches both.
pub fn bare_message_id(id: &str) -> &str {
    let id = jsstr::trim(id);
    let id = id.strip_prefix('<').unwrap_or(id);
    id.strip_suffix('>').unwrap_or(id)
}

/// Both stored spellings of each id, for matching against the columns.
pub fn stored_id_forms<I, S>(ids: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    let mut forms = Vec::new();
    for id in ids {
        let bare = bare_message_id(id.as_ref());
        if bare.is_empty() {
            continue;
        }
        for form in [bare.to_string(), format!("<{bare}>")] {
            if seen.insert(form.clone()) {
                forms.push(form);
            }
        }
    }
    forms
}

/// Whether a mail carries either threading header.
pub fn has_thread_headers<T: ThreadCandidate + ?Sized>(mail: &T) -> bool {
    mail.in_reply_to().is_some_and(|id| !id.is_empty()) || mail.reference_ids().is_some_and(|ids| !ids.is_empty())
}

/// The Message-IDs a mail names, bare: its own and the ones it answers.
pub fn linked_ids<T: ThreadCandidate + ?Sized>(mail: &T) -> Vec<String> {
    let references = mail.reference_ids().unwrap_or(&[]).iter().map(String::as_str);
    [mail.message_id(), mail.in_reply_to().unwrap_or("")]
        .into_iter()
        .chain(references)
        .map(bare_message_id)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// The addresses of a stored address column: one entry or a list of them.
fn addresses_of(value: Option<&Value>) -> Vec<String> {
    let entries = match value {
        Some(Value::Array(entries)) => entries.as_slice(),
        Some(entry) => std::slice::from_ref(entry),
        None => &[],
    };
    entries
        .iter()
        .filter_map(|entry| entry.as_object()?.get("address")?.as_str())
        .filter(|address| !address.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Every address on a mail's From, To and Cc that isn't the user's own.
pub fn other_participants_of<T, F>(mail: &T, is_own: F) -> HashSet<String>
where
    T: ThreadCandidate + ?Sized,
    F: Fn(&str) -> bool,
{
    [mail.from_address(), mail.to_address(), mail.cc_address()]
        .into_iter()
        .flat_map(addresses_of)
        .filter(|address| !is_own(address))
        .collect()
}

/// Whether two header-less mails read as one conversation.
pub fn matches_by_subject<T, F>(a: &T, b: &T, is_own: F) -> bool
where
    T: ThreadCandidate + ?Sized,
    F: Fn(&str) -> bool,
{
    if has_thread_headers(a) || has_thread_headers(b) {
        return false;
    }
    let subject = normalize_subject(a.subject());
    if jsstr::len(&subject) < 3 || subject != normalize_subject(b.subject()) {
        return false;
    }
    if !is_reply_subject(a.subject()) && !is_reply_subject(b.subject()) {
        return false;
    }
    let shared = other_participants_of(a, &is_own);
    other_participants_of(b, &is_own)
        .iter()
        .any(|address| shared.contains(address))
}

/// Whether one mail names the other in its thread headers.
pub fn linked_by_headers<T: ThreadCandidate + ?Sized>(a: &T, b: &T) -> bool {
    let ids: HashSet<String> = linked_ids(a).into_iter().collect();
    linked_ids(b).iter().any(|id| ids.contains(id))
}

/// The mails in `candidates` connected to `seed`, the seed included, by
/// headers or (between header-less mails) by subject, following chains.
pub fn collect_thread<'a, T, F>(seed: &'a T, candidates: &'a [T], is_own: F) -> Vec<&'a T>
where
    T: ThreadCandidate,
    F: Fn(&str) -> bool,
{
    let mut thread = vec![seed];
    let mut in_thread: HashSet<&str> = HashSet::from([seed.mail_id()]);
    let mut next = 0;
    while let Some(&current) = thread.get(next) {
        next += 1;
        for other in candidates {
            if in_thread.contains(other.mail_id()) {
                continue;
            }
            if linked_by_headers(current, other) || matches_by_subject(current, other, &is_own) {
                in_thread.insert(other.mail_id());
                thread.push(other);
            }
        }
    }
    thread
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // The user is on every stored mail, as here: me@inbox.test on To or From.
    fn is_own(address: &str) -> bool {
        address.to_lowercase().ends_with("@inbox.test")
    }

    fn mail(id: &str) -> ThreadMail {
        ThreadMail {
            mail_id: id.to_string(),
            message_id: format!("<{id}@x>"),
            subject: "Plan".to_string(),
            from_address: Some(json!([{ "address": "alice@x.com" }])),
            to_address: Some(json!([{ "address": "me@inbox.test" }])),
            ..ThreadMail::default()
        }
    }

    fn ids(thread: &[&ThreadMail]) -> Vec<String> {
        thread.iter().map(|mail| mail.mail_id.clone()).collect()
    }

    #[test]
    fn strips_stacked_reply_and_forward_markers() {
        assert_eq!(normalize_subject("Re: RE: Fwd: Project  plan"), "project plan");
        assert_eq!(normalize_subject("AW: Re[2]: Hallo"), "hallo");
    }

    #[test]
    fn leaves_other_subjects_alone() {
        assert_eq!(normalize_subject("Return policy"), "return policy");
    }

    #[test]
    fn recognises_reply_and_forward_prefixes() {
        assert!(is_reply_subject("Re: x"));
        assert!(is_reply_subject("Fwd: x"));
        assert!(!is_reply_subject("Regarding x"));
    }

    #[test]
    fn compares_ids_without_their_angle_brackets() {
        assert_eq!(bare_message_id("<a@x>"), "a@x");
        assert_eq!(bare_message_id("a@x"), "a@x");
    }

    #[test]
    fn matches_both_stored_spellings() {
        let mut forms = stored_id_forms(["<a@x>", "b@y"]);
        forms.sort();
        assert_eq!(forms, ["<a@x>", "<b@y>", "a@x", "b@y"]);
    }

    #[test]
    fn links_a_reply_to_the_message_it_answers() {
        let reply = ThreadMail {
            in_reply_to: Some("<a@x>".into()),
            ..mail("b")
        };
        assert!(linked_by_headers(&mail("a"), &reply));
    }

    #[test]
    fn links_a_reply_to_an_appended_original_stored_without_brackets() {
        let appended = ThreadMail {
            message_id: "a@x".into(),
            ..mail("a")
        };
        let reply = ThreadMail {
            in_reply_to: Some("<a@x>".into()),
            ..mail("b")
        };
        assert!(linked_by_headers(&appended, &reply));
    }

    #[test]
    fn links_two_replies_that_share_a_references_entry() {
        let one = ThreadMail {
            reference_ids: Some(vec!["<a@x>".into()]),
            ..mail("b")
        };
        let two = ThreadMail {
            reference_ids: Some(vec!["<a@x>".into(), "<b@x>".into()]),
            ..mail("c")
        };
        assert!(linked_by_headers(&one, &two));
    }

    #[test]
    fn does_not_link_unrelated_mail() {
        assert!(!linked_by_headers(&mail("a"), &mail("b")));
    }

    #[test]
    fn matches_a_header_less_reply_with_the_same_subject_and_correspondent() {
        let original = ThreadMail {
            subject: "Plan".into(),
            ..mail("a")
        };
        let reply = ThreadMail {
            subject: "Re: Plan".into(),
            from_address: Some(json!([{ "address": "me@inbox.test" }])),
            to_address: Some(json!([{ "address": "Alice@x.com" }])),
            ..mail("b")
        };
        assert!(matches_by_subject(&original, &reply, is_own));
    }

    // Two vendors answering two separate quotes, both to the same mailbox:
    // the only address they share is the user's own.
    #[test]
    fn keeps_apart_replies_from_different_correspondents_to_the_same_mailbox() {
        let a = ThreadMail {
            subject: "Re: Invoice".into(),
            from_address: Some(json!([{ "address": "billing@vendor-a.com" }])),
            ..mail("a")
        };
        let b = ThreadMail {
            subject: "RE: invoice".into(),
            from_address: Some(json!([{ "address": "ar@vendor-b.com" }])),
            ..mail("b")
        };
        assert!(!matches_by_subject(&a, &b, is_own));
    }

    #[test]
    fn never_matches_mail_that_carries_thread_headers() {
        let a = ThreadMail {
            subject: "Re: Invoice".into(),
            in_reply_to: Some("<q1@inbox.test>".into()),
            ..mail("a")
        };
        let b = ThreadMail {
            subject: "Re: Invoice".into(),
            in_reply_to: Some("<q2@inbox.test>".into()),
            ..mail("b")
        };
        assert!(!matches_by_subject(&a, &b, is_own));
        let c = ThreadMail {
            subject: "Re: Invoice".into(),
            ..mail("c")
        };
        assert!(!matches_by_subject(&c, &a, is_own));
    }

    #[test]
    fn does_not_match_two_non_replies_with_one_subject() {
        let a = ThreadMail {
            subject: "Weekly digest".into(),
            ..mail("a")
        };
        let b = ThreadMail {
            subject: "Weekly digest".into(),
            ..mail("b")
        };
        assert!(!matches_by_subject(&a, &b, is_own));
    }

    #[test]
    fn does_not_match_very_short_subjects() {
        let a = ThreadMail {
            subject: "Re: hi".into(),
            ..mail("a")
        };
        let b = ThreadMail {
            subject: "hi".into(),
            ..mail("b")
        };
        assert!(!matches_by_subject(&a, &b, is_own));
    }

    #[test]
    fn follows_a_chain_of_replies_from_any_message_in_it() {
        let a = mail("a");
        let b = ThreadMail {
            in_reply_to: Some("<a@x>".into()),
            reference_ids: Some(vec!["<a@x>".into()]),
            ..mail("b")
        };
        let c = ThreadMail {
            in_reply_to: Some("<b@x>".into()),
            reference_ids: Some(vec!["<a@x>".into(), "<b@x>".into()]),
            ..mail("c")
        };
        let other = ThreadMail {
            subject: "Unrelated".into(),
            ..mail("z")
        };
        let candidates = [a, b, c.clone(), other];
        let mut thread = ids(&collect_thread(&c, &candidates, is_own));
        thread.sort();
        assert_eq!(thread, ["a", "b", "c"]);
    }

    #[test]
    fn keeps_two_header_linked_conversations_with_one_subject_apart() {
        let a = ThreadMail {
            subject: "Re: Invoice".into(),
            in_reply_to: Some("<q1@inbox.test>".into()),
            from_address: Some(json!([{ "address": "billing@vendor-a.com" }])),
            ..mail("a")
        };
        let b = ThreadMail {
            subject: "RE: invoice".into(),
            in_reply_to: Some("<q2@inbox.test>".into()),
            from_address: Some(json!([{ "address": "ar@vendor-b.com" }])),
            ..mail("b")
        };
        let candidates = [a.clone(), b];
        assert_eq!(ids(&collect_thread(&a, &candidates, is_own)), ["a"]);
    }

    #[test]
    fn returns_the_seed_alone_when_nothing_matches() {
        let a = mail("a");
        let candidates = [
            a.clone(),
            ThreadMail {
                subject: "Other".into(),
                ..mail("z")
            },
        ];
        assert_eq!(collect_thread(&a, &candidates, is_own), [&a]);
    }
}
