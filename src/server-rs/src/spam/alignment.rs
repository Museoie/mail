//! Sender alignment checks (spam/alignment.ts).
//!
//! SMTP authenticates neither the `From:` header nor the envelope `MAIL FROM`,
//! so a match on either one alone records a claim rather than an identity.
//! Requiring the two to agree costs a forger control of the envelope as well as
//! the header, which a sender relaying through someone else's MTA does not have.
//! Agreement here means an exact domain match — DMARC's *strict* alignment
//! (RFC 7489 3.1.2). Relaxed alignment compares Organizational Domains instead
//! and would need a public-suffix list; until it exists, a bounce domain that is
//! a subdomain of the header domain does not align.

/// Domain half of an email address, lowercased.
///
/// `None` for anything that is not exactly one local part and one domain, so a
/// malformed address fails alignment rather than matching loosely.
fn domain_of(address: Option<&str>) -> Option<String> {
    let address = address.filter(|address| !address.is_empty())?;
    let mut parts = address.split('@');
    let (local, domain) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || local.is_empty() || domain.is_empty() {
        return None;
    }
    Some(domain.to_lowercase())
}

/// Whether the envelope sender and the header sender share a domain.
///
/// Absent or malformed on either side counts as unaligned — a missing envelope
/// sender (`MAIL FROM:<>`) carries no identity to corroborate the header with.
///
/// ```ignore
/// is_envelope_aligned(Some("news@github.com"), Some("bounces+9f@github.com")); // true
/// is_envelope_aligned(Some("a@example.com"), Some("b@mail.example.com"));      // false
/// ```
pub fn is_envelope_aligned(header_from_address: Option<&str>, envelope_from_address: Option<&str>) -> bool {
    match (domain_of(header_from_address), domain_of(envelope_from_address)) {
        (Some(header_domain), Some(envelope_domain)) => header_domain == envelope_domain,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aligned(header: &str, envelope: &str) -> bool {
        is_envelope_aligned(Some(header), Some(envelope))
    }

    #[test]
    fn aligns_an_envelope_sender_that_shares_the_header_senders_domain() {
        assert!(aligned("notifications@github.com", "bounces+9f@github.com"));
    }

    #[test]
    fn aligns_identical_addresses() {
        assert!(aligned("a@example.com", "a@example.com"));
    }

    #[test]
    fn compares_domains_case_insensitively() {
        assert!(aligned("Spammer@UT-Allow.Example", "bounce@ut-allow.example"));
    }

    #[test]
    fn refuses_an_envelope_sender_on_a_different_domain() {
        assert!(!aligned("spammer@ut-allow.example", "envelope-sender@external.example"));
    }

    #[test]
    fn refuses_a_subdomain_of_the_header_domain() {
        assert!(!aligned("a@example.com", "b@mail.example.com"));
    }

    #[test]
    fn refuses_a_header_domain_that_merely_ends_with_the_envelope_domain() {
        assert!(!aligned("a@notexample.com", "b@example.com"));
    }

    #[test]
    fn refuses_a_missing_envelope_sender() {
        assert!(!is_envelope_aligned(Some("a@example.com"), None));
    }

    #[test]
    fn refuses_an_empty_envelope_sender() {
        assert!(!aligned("a@example.com", ""));
    }

    #[test]
    fn refuses_a_missing_header_sender() {
        assert!(!is_envelope_aligned(None, Some("a@example.com")));
    }

    #[test]
    fn refuses_an_address_with_no_domain_part() {
        assert!(!aligned("a@example.com", "postmaster"));
    }

    #[test]
    fn refuses_an_address_with_a_trailing_at_and_empty_domain() {
        assert!(!aligned("a@example.com", "b@"));
    }

    #[test]
    fn refuses_an_address_with_an_empty_local_part() {
        assert!(!aligned("a@example.com", "@example.com"));
    }

    #[test]
    fn refuses_a_multi_at_address_whose_middle_component_would_otherwise_align() {
        assert!(!aligned("a@example.com@evil.test", "b@example.com"));
        assert!(!aligned("a@example.com", "b@example.com@evil.test"));
        assert!(!aligned("a@example.com@evil.test", "b@example.com@evil.test"));
    }
}
