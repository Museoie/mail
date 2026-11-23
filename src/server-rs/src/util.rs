//! Domain helpers shared by every part of the server.

use std::future::Future;
use std::time::Duration;

use crate::env;

/// The mail domain this server serves (`EMAIL_DOMAIN`).
pub fn get_domain() -> String {
    env::non_empty("EMAIL_DOMAIN").unwrap_or_else(|| "mydomain".to_string())
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
}
