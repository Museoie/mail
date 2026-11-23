//! Password hashing and constant-cost verification for every credential
//! surface: HTTP login, IMAP LOGIN/AUTHENTICATE and SMTP AUTH.
//!
//! A comparison skipped when the account does not exist answers in
//! microseconds where a real account takes tens of milliseconds, which turns
//! an unauthenticated listener into a username oracle. So a full bcrypt round
//! is always spent, against a stand-in digest when there is no real one.

/// bcrypt cost for every stored digest, and for the stand-in.
const BCRYPT_COST: u32 = 10;

const DUMMY_HASH: &str = "$2b$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy";

/// bcrypt reads at most 72 bytes of key; longer passwords are cut there, the
/// way the digests already stored were made.
const BCRYPT_MAX_KEY_BYTES: usize = 72;

fn bcrypt_key(password: &str) -> &[u8] {
    let bytes = password.as_bytes();
    &bytes[..bytes.len().min(BCRYPT_MAX_KEY_BYTES)]
}

/// Hashes a password for storage as a `$2b$` bcrypt digest.
pub async fn hash_password(password: &str) -> String {
    let key = bcrypt_key(password).to_vec();
    tokio::task::spawn_blocking(move || {
        bcrypt::hash_with_result(key, BCRYPT_COST)
            .map(|parts| parts.format_for_version(bcrypt::Version::TwoB))
            .expect("bcrypt accepts a key of at most 72 bytes")
    })
    .await
    .expect("the hashing task does not panic")
}

/// Compares a submitted password against a stored digest, spending one full
/// bcrypt round whether or not there is anything to compare against. A
/// missing password, a missing digest or a stored value that is not a digest
/// all yield false.
pub async fn verify_password(password: Option<&str>, stored_hash: Option<&str>) -> bool {
    let has_password = password.is_some_and(|p| !p.is_empty());
    let has_hash = stored_hash.is_some_and(|h| !h.is_empty());
    let key = bcrypt_key(password.unwrap_or("")).to_vec();
    let hash = stored_hash.filter(|h| !h.is_empty()).unwrap_or(DUMMY_HASH).to_string();
    let matches = tokio::task::spawn_blocking(move || bcrypt::verify(key, &hash).unwrap_or(false))
        .await
        .unwrap_or(false);
    has_password && has_hash && matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let hash = hash_password("correct horse").await;
        assert!(hash.starts_with("$2b$10$"));
        assert!(verify_password(Some("correct horse"), Some(&hash)).await);
        assert!(!verify_password(Some("wrong"), Some(&hash)).await);
        assert!(!verify_password(None, Some(&hash)).await);
        assert!(!verify_password(Some("correct horse"), None).await);
        assert!(!verify_password(Some("x"), Some("not-a-digest")).await);
        assert!(!verify_password(Some(""), Some(&hash)).await);
    }

    #[tokio::test]
    async fn long_passwords_are_cut_at_72_bytes() {
        let long = "a".repeat(100);
        let hash = hash_password(&long).await;
        assert!(verify_password(Some(&"a".repeat(72)), Some(&hash)).await);
        assert!(!verify_password(Some(&"a".repeat(71)), Some(&hash)).await);
    }
}
