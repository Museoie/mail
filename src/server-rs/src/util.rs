//! Domain helpers shared by every part of the server.

use std::future::Future;
use std::time::Duration;

use crate::env;

/// The mail domain this server serves (`EMAIL_DOMAIN`).
pub fn get_domain() -> String {
    env::non_empty("EMAIL_DOMAIN").unwrap_or_else(|| "mydomain".to_string())
}

/// The domain a user's addresses live under: the served domain for `admin`,
/// `<username>.<domain>` for everyone else.
pub fn get_user_domain(username: &str) -> String {
    let domain = get_domain();
    if username == "admin" {
        domain
    } else {
        format!("{username}.{domain}")
    }
}

fn domain_of(address: &str) -> String {
    address.rsplit('@').next().unwrap_or("").to_lowercase()
}

/// Whether this host is responsible for `address`: `domain` itself or any
/// subdomain under it.
pub fn is_local_address(address: &str, domain: &str) -> bool {
    let domain_in_address = domain_of(address);
    let target = domain.to_lowercase();
    domain_in_address == target || domain_in_address.ends_with(&format!(".{target}"))
}

/// The username whose mailbox receives mail for `address`. `x@<domain>` and
/// `x@admin.<domain>` are admin's, `x@bob.<domain>` is bob's. For an address
/// outside the served domain this is the address's own domain, which matches
/// no username.
pub fn address_to_username(address: &str) -> String {
    let domain = get_domain().to_lowercase();
    let domain_in_address = domain_of(address);
    // `"a.b".split(".b")[0]`: everything before the first occurrence.
    let needle = format!(".{domain}");
    let sub_domain = match domain_in_address.find(&needle) {
        Some(at) => domain_in_address[..at].to_string(),
        None => domain_in_address,
    };
    if sub_domain == domain {
        "admin".to_string()
    } else {
        sub_domain
    }
}

/// The username an envelope recipient is delivered to, `None` for an address
/// outside `domain`: [`is_local_address`], then [`address_to_username`].
/// Delivery files a message under each recipient's user by this, and the mail
/// lists show a received row's envelope only as far as it names the row's own
/// user by the same rule.
pub fn recipient_username(address: &str, domain: &str) -> Option<String> {
    is_local_address(address, domain).then(|| address_to_username(address))
}

/// Whether `username` can name a mailbox: a lower-case DNS label.
pub fn is_routable_username(username: &str) -> bool {
    let bytes = username.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes.iter().all(|&b| edge(b) || b == b'-')
}

/// `None` if `future` has not finished within `ms`.
pub async fn with_timeout<T>(future: impl Future<Output = T>, ms: u64) -> Option<T> {
    tokio::time::timeout(Duration::from_millis(ms), future).await.ok()
}

const UUID_GROUPS: [usize; 5] = [8, 4, 4, 4, 12];

/// Whether `value` is a UUID in the hyphenated 8-4-4-4-12 form, any case.
pub fn is_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    for expected in UUID_GROUPS {
        match parts.next() {
            Some(part) if part.len() == expected && part.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Eight random hex digits, the fallback id the shared models use.
pub fn get_random_id() -> String {
    let bytes: [u8; 4] = rand::random();
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routable_usernames() {
        assert!(is_routable_username("alice"));
        assert!(is_routable_username("a"));
        assert!(is_routable_username("a-1"));
        assert!(!is_routable_username("-a"));
        assert!(!is_routable_username("a-"));
        assert!(!is_routable_username("Alice"));
        assert!(!is_routable_username(""));
        assert!(!is_routable_username(&"a".repeat(64)));
    }

    #[test]
    fn uuids() {
        assert!(is_uuid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(is_uuid("123E4567-E89B-12D3-A456-426614174000"));
        assert!(!is_uuid("123e4567e89b12d3a456426614174000"));
        assert!(!is_uuid("123e4567-e89b-12d3-a456-42661417400g"));
        assert!(!is_uuid(""));
    }

    #[test]
    fn local_addresses() {
        assert!(is_local_address("a@mail.test", "mail.test"));
        assert!(is_local_address("a@BOB.mail.test", "Mail.Test"));
        assert!(!is_local_address("a@notmail.test", "mail.test"));
    }

    #[test]
    fn recipient_usernames() {
        let domain = get_domain();
        assert_eq!(recipient_username(&format!("x@bob.{domain}"), &domain).as_deref(), Some("bob"));
        assert_eq!(recipient_username(&format!("X@BOB.{}", domain.to_uppercase()), &domain).as_deref(), Some("bob"));
        assert_eq!(recipient_username(&format!("hi@{domain}"), &domain).as_deref(), Some("admin"));
        assert_eq!(recipient_username(&format!("hi@admin.{domain}"), &domain).as_deref(), Some("admin"));
        assert_eq!(recipient_username("x@remote.example", &domain), None);
        assert_eq!(recipient_username(&format!("x@bob.{domain}.evil"), &domain), None);
    }
}
