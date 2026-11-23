//! Who an authenticated submission is sent as, and to whom in which field
//! (the pure half of `onDataOutgoing` in `smtp.ts`).

use std::collections::HashSet;

use crate::js;
use crate::mime::address::{OneOrMany, ParsedAddress, ParsedAddressValue};

/// `local@domain` split at its last `@`, when both halves are there and the
/// local part holds no other `@`.
fn split_address(address: &str) -> Option<(&str, &str)> {
    let at = address.rfind('@')?;
    let (local, domain) = (&address[..at], &address[at + 1..]);
    if local.is_empty() || domain.is_empty() || local.contains('@') {
        return None;
    }
    Some((local, domain))
}

/// `address.trim().toLowerCase()`.
fn normalize(address: &str) -> String {
    js::trim(address).to_lowercase()
}

/// Every address an address header names, groups flattened, empty ones
/// dropped (`addressList` in `smtp.ts`).
pub fn address_list(header: Option<&OneOrMany<ParsedAddress>>) -> Vec<String> {
    fn flatten(entries: &[ParsedAddressValue], into: &mut Vec<String>) {
        for entry in entries {
            match &entry.group {
                Some(group) => flatten(group, into),
                None => into.push(entry.address.clone().unwrap_or_default()),
            }
        }
    }
    let mut addresses = Vec::new();
    for object in header.map(OneOrMany::as_slice).unwrap_or_default() {
        flatten(&object.value, &mut addresses);
    }
    addresses.retain(|address| !address.is_empty());
    addresses
}

/// The address a submission is sent as, and the recipients left once that
/// choice took one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingSender {
    pub sender: String,
    /// The domain the sender's address is on: one of the domains it may send from.
    pub domain: String,
    pub recipients: Vec<String>,
}

/// Resolves which of the user's addresses an SMTP submission is sent as, given
/// the domains it may send from (`domains`, the default first) and the parsed
/// `To:` addresses in `addressed_to` (`resolveOutgoingSender`).
///
/// The default identity is `<username>@<default domain>`. A `From` on any of
/// the domains picks that address; a client that can only send as the default
/// identity picks another one by adding it as a Cc or Bcc recipient, which is
/// then dropped from the delivery.
///
/// Clients strip `Bcc:` before DATA and carry those addresses only in the
/// envelope, so a Cc and a Bcc cannot be told apart here and both select.
/// `To:` is the one recipient field that always survives into the message,
/// which is what makes excluding it possible.
pub fn resolve_outgoing_sender(
    username: &str,
    domains: &[String],
    from_header: Option<&str>,
    from_envelope: Option<&str>,
    recipients: &[String],
    addressed_to: &[String],
) -> OutgoingSender {
    let sendable: HashSet<String> = domains.iter().map(|domain| domain.to_lowercase()).collect();
    let default_domain = domains.first().map(|domain| domain.to_lowercase()).unwrap_or_default();
    // (local, domain) of an address on one of the sendable domains.
    let account_of = |address: Option<&str>| -> Option<(String, String)> {
        let normalized = normalize(address.unwrap_or(""));
        let (local, domain) = split_address(&normalized)?;
        sendable.contains(domain).then(|| (local.to_string(), domain.to_string()))
    };
    let is_default_identity = |(local, domain): &(String, String)| local == username && *domain == default_domain;

    // `accountOf(header) || accountOf(envelope)`: an empty local part cannot
    // come out of `split_address`, so `Option::or` is the same.
    let from_account = account_of(from_header).or_else(|| account_of(from_envelope));
    if let Some((local, domain)) = from_account.clone().filter(|account| !is_default_identity(account)) {
        return OutgoingSender { sender: local, domain, recipients: recipients.to_vec() };
    }

    let addressed: HashSet<String> = addressed_to.iter().map(|address| normalize(address)).collect();
    let selected = recipients.iter().position(|address| {
        account_of(Some(address))
            .is_some_and(|account| !is_default_identity(&account) && !addressed.contains(&normalize(address)))
    });

    match selected {
        Some(selected) if recipients.len() != 1 => {
            let (sender, domain) = account_of(Some(&recipients[selected])).expect("the selected recipient has an account");
            OutgoingSender {
                sender,
                domain,
                recipients: recipients
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != selected)
                    .map(|(_, r)| r.clone())
                    .collect(),
            }
        }
        _ => {
            let envelope_local = from_envelope
                .map(normalize)
                .and_then(|normalized| split_address(&normalized).map(|(local, _)| local.to_string()));
            let (from_local, from_domain) = match from_account {
                Some((local, domain)) => (Some(local), Some(domain)),
                None => (None, None),
            };
            OutgoingSender {
                sender: from_local.or(envelope_local).unwrap_or_else(|| username.to_string()),
                domain: from_domain.unwrap_or(default_domain),
                recipients: recipients.to_vec(),
            }
        }
    }
}

/// The envelope recipients split into To, Cc and Bcc
/// (`splitEnvelopeRecipients`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvelopeRecipients {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

/// Splits the SMTP envelope recipients into To / Cc / Bcc.
///
/// A client puts Bcc addresses only in `RCPT TO` and strips the `Bcc:`
/// header out of DATA, so an envelope recipient that neither header names is
/// a Bcc. The envelope decides who is delivered to; the headers only decide
/// which of the three lists each recipient belongs in.
pub fn split_envelope_recipients(
    recipients: &[String],
    addressed_to: &[String],
    addressed_cc: &[String],
) -> EnvelopeRecipients {
    let to_header: HashSet<String> = addressed_to.iter().map(|address| normalize(address)).collect();
    let cc_header: HashSet<String> = addressed_cc.iter().map(|address| normalize(address)).collect();
    let mut split = EnvelopeRecipients::default();
    for address in recipients {
        let key = normalize(address);
        if to_header.contains(&key) {
            split.to.push(address.clone());
        } else if cc_header.contains(&key) {
            split.cc.push(address.clone());
        } else {
            split.bcc.push(address.clone());
        }
    }
    split
}

#[cfg(test)]
mod tests {
    //! The cases of `resolveOutgoingSender` and `splitEnvelopeRecipients` in
    //! `smtp.test.ts`.

    use super::*;
    use crate::mime::address::parse_address_header;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    /// `resolveOutgoingSender("admin", ["test.com"], from, recipients, addressedTo)`.
    fn resolve(
        header: Option<&str>,
        envelope: Option<&str>,
        recipients: &[&str],
        addressed_to: &[&str],
    ) -> OutgoingSender {
        resolve_outgoing_sender("admin", &strings(&["test.com"]), header, envelope, &strings(recipients), &strings(addressed_to))
    }

    fn sent_as(sender: &str, recipients: &[&str]) -> OutgoingSender {
        OutgoingSender { sender: sender.to_string(), domain: "test.com".into(), recipients: strings(recipients) }
    }

    #[test]
    fn promotes_the_first_same_domain_recipient_to_sender_and_drops_it() {
        assert_eq!(
            resolve(
                Some("admin@test.com"),
                Some("admin@test.com"),
                &["outside@other.com", "sales@test.com", "later@other.com"],
                &[]
            ),
            sent_as("sales", &["outside@other.com", "later@other.com"])
        );
    }

    #[test]
    fn selects_only_the_first_same_domain_recipient_and_keeps_the_rest() {
        assert_eq!(
            resolve(None, Some("admin@test.com"), &["sales@test.com", "support@test.com"], &[]),
            sent_as("sales", &["support@test.com"])
        );
    }

    #[test]
    fn keeps_a_from_that_already_names_another_account_of_the_domain() {
        assert_eq!(
            resolve(Some("sales@test.com"), Some("admin@test.com"), &["outside@other.com", "support@test.com"], &[]),
            sent_as("sales", &["outside@other.com", "support@test.com"])
        );
    }

    #[test]
    fn treats_the_login_account_among_the_recipients_as_a_real_recipient() {
        assert_eq!(
            resolve(None, Some("admin@test.com"), &["outside@other.com", "admin@test.com"], &[]),
            sent_as("admin", &["outside@other.com", "admin@test.com"])
        );
    }

    #[test]
    fn matches_the_domain_case_insensitively_and_normalizes_the_account() {
        assert_eq!(
            resolve(None, Some("Admin@TEST.com"), &["Sales@Test.com", "outside@other.com"], &[]),
            sent_as("sales", &["outside@other.com"])
        );
    }

    #[test]
    fn ignores_a_from_header_outside_the_user_domain_and_falls_back_to_the_envelope() {
        assert_eq!(
            resolve(Some("spoofed@evil.com"), Some("admin@test.com"), &["outside@other.com"], &[]),
            sent_as("admin", &["outside@other.com"])
        );
    }

    #[test]
    fn never_lets_a_from_header_outside_the_user_domain_supply_the_sender() {
        assert_eq!(
            resolve(Some("ceo@irs.gov"), None, &["victim@other.com"], &[]),
            sent_as("admin", &["victim@other.com"])
        );
        assert_eq!(
            resolve(Some("ceo@irs.gov"), Some("@test.com"), &["victim@other.com"], &[]),
            sent_as("admin", &["victim@other.com"])
        );
    }

    #[test]
    fn lowercases_a_sender_taken_from_the_envelope() {
        assert_eq!(
            resolve(None, Some("Bob@Test.com"), &["outside@other.com"], &[]),
            sent_as("bob", &["outside@other.com"])
        );
    }

    #[test]
    fn leaves_a_to_recipient_of_the_users_domain_addressed_not_promoted() {
        assert_eq!(
            resolve(
                None,
                Some("admin@test.com"),
                &["bob@test.com", "friend@other.com"],
                &["bob@test.com", "friend@other.com"]
            ),
            sent_as("admin", &["bob@test.com", "friend@other.com"])
        );
    }

    #[test]
    fn promotes_a_cc_account_while_a_to_account_of_the_same_domain_stays_addressed() {
        assert_eq!(
            resolve(None, Some("admin@test.com"), &["bob@test.com", "sales@test.com"], &["bob@test.com"]),
            sent_as("sales", &["bob@test.com"])
        );
    }

    #[test]
    fn keeps_a_lone_same_domain_recipient_rather_than_emptying_the_recipient_list() {
        assert_eq!(
            resolve(None, Some("admin@test.com"), &["sales@test.com"], &[]),
            sent_as("admin", &["sales@test.com"])
        );
    }

    #[test]
    fn does_not_read_a_local_part_out_of_a_two_at_address() {
        assert_eq!(
            resolve(Some("sales@evil.com@test.com"), Some("admin@test.com"), &["outside@other.com"], &[]),
            sent_as("admin", &["outside@other.com"])
        );
    }

    #[test]
    fn falls_back_to_the_username_when_no_address_carries_a_local_part() {
        assert_eq!(
            resolve(None, Some("@test.com"), &["outside@other.com"], &[]),
            sent_as("admin", &["outside@other.com"])
        );
    }

    #[test]
    fn scopes_selection_to_the_callers_own_domain_not_the_bare_mail_domain() {
        assert_eq!(
            resolve_outgoing_sender(
                "alice",
                &strings(&["alice.test.com"]),
                None,
                Some("alice@alice.test.com"),
                &strings(&["outside@other.com", "team@alice.test.com", "admin@test.com"]),
                &[]
            ),
            OutgoingSender {
                sender: "team".into(),
                domain: "alice.test.com".into(),
                recipients: strings(&["outside@other.com", "admin@test.com"]),
            }
        );
    }

    #[test]
    fn sends_from_whichever_owned_domain_the_from_names_and_defaults_to_the_first() {
        let domains = strings(&["alice.test.com", "example.org"]);
        let sent = |header: Option<&str>, envelope: Option<&str>, recipients: &[&str]| {
            resolve_outgoing_sender("alice", &domains, header, envelope, &strings(recipients), &[])
        };
        // The same local part on another of the user's domains is another identity.
        let from_org = sent(Some("Alice@Example.org"), Some("alice@alice.test.com"), &["x@remote.example"]);
        assert_eq!((from_org.sender.as_str(), from_org.domain.as_str()), ("alice", "example.org"));
        let info = sent(None, Some("alice@alice.test.com"), &["x@remote.example", "info@example.org"]);
        assert_eq!((info.sender.as_str(), info.domain.as_str()), ("info", "example.org"));
        assert_eq!(info.recipients, strings(&["x@remote.example"]));
        let plain = sent(Some("spoof@elsewhere.example"), None, &["x@remote.example"]);
        assert_eq!((plain.sender.as_str(), plain.domain.as_str()), ("alice", "alice.test.com"));
    }

    #[test]
    fn assigns_each_envelope_recipient_by_the_header_that_names_it() {
        let split = split_envelope_recipients(
            &strings(&["a@x.com", "b@x.com", "c@x.com"]),
            &strings(&["a@x.com"]),
            &strings(&["b@x.com"]),
        );
        assert_eq!(split.to, strings(&["a@x.com"]));
        assert_eq!(split.cc, strings(&["b@x.com"]));
        assert_eq!(split.bcc, strings(&["c@x.com"]));
    }

    #[test]
    fn matches_a_header_address_to_an_envelope_address_case_insensitively() {
        let split = split_envelope_recipients(&strings(&["Bob@X.com"]), &strings(&[" bob@x.COM "]), &[]);
        assert_eq!(split.to, strings(&["Bob@X.com"]));
    }

    #[test]
    fn ignores_a_header_address_that_the_envelope_never_named() {
        let split =
            split_envelope_recipients(&strings(&["a@x.com"]), &strings(&["ghost@x.com"]), &strings(&["other@x.com"]));
        assert_eq!(split, EnvelopeRecipients { to: vec![], cc: vec![], bcc: strings(&["a@x.com"]) });
    }

    #[test]
    fn assigns_an_address_named_by_both_headers_to_to_not_cc() {
        let split = split_envelope_recipients(&strings(&["a@x.com"]), &strings(&["a@x.com"]), &strings(&["a@x.com"]));
        assert_eq!(split.to, strings(&["a@x.com"]));
        assert!(split.cc.is_empty());
    }

    #[test]
    fn returns_three_empty_lists_for_an_empty_envelope() {
        assert_eq!(split_envelope_recipients(&[], &strings(&["a@x.com"]), &[]), EnvelopeRecipients::default());
    }

    #[test]
    fn reads_addresses_out_of_groups_and_repeated_headers() {
        let one = OneOrMany::One(parse_address_header("Friends: a@x.com, B <b@x.com>;, c@x.com"));
        assert_eq!(address_list(Some(&one)), strings(&["a@x.com", "b@x.com", "c@x.com"]));
        let many = OneOrMany::Many(vec![parse_address_header("a@x.com"), parse_address_header("Empty:;")]);
        assert_eq!(address_list(Some(&many)), strings(&["a@x.com"]));
        assert!(address_list(None).is_empty());
    }
}
