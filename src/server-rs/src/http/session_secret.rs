//! The key that signs session cookies.

use anyhow::{bail, Result};
use serde_json::json;

use crate::{env, logger};

/// The key a non-production boot signs with when `SECRET` is unset. It stays
/// fixed: changing it would log out every user of a deployment that never
/// opted into the production posture.
pub const DEVELOPMENT_FALLBACK: &str = "secret";

/// Values this repository publishes; signing with one grants no integrity.
const PUBLISHED_VALUES: [&str; 2] = [DEVELOPMENT_FALLBACK, "inbox"];

const RECOMMENDED_LENGTH: usize = 32;

const GENERATE: &str = "generate one with `openssl rand -base64 32`";

/// Resolves the signing key. Production refuses to boot on a missing or
/// publicly known key; elsewhere nothing is fatal and a missing key resolves
/// to a fixed development value.
pub fn resolve_session_secret() -> Result<String> {
    let secret = env::var("SECRET").unwrap_or_default();
    let value = crate::js::trim(&secret);

    if value.is_empty() {
        if env::is_production() {
            bail!(
                "SECRET is not set, so session cookies would be signed with a key published in this repository — anyone could forge one. Set SECRET in the deployment environment ({GENERATE})."
            );
        }
        logger::warn(
            format!(
                "[CONFIG WARNING] SECRET is not set, so session cookies are signed with a fixed\n  development key that this repository publishes. Production refuses to boot without\n  a real one — {GENERATE} and set SECRET before deploying."
            ),
            json!({}),
        );
        return Ok(DEVELOPMENT_FALLBACK.to_string());
    }

    if PUBLISHED_VALUES.contains(&value) {
        if env::is_production() {
            bail!(
                "SECRET is set to a value published in this repository, so every session cookie is forgeable. Set a private SECRET in the deployment environment ({GENERATE})."
            );
        }
        logger::warn(
            format!(
                "[CONFIG WARNING] SECRET is a value published in this repository, so it grants session\n  cookies no integrity. Production refuses to boot on it — {GENERATE} and set SECRET\n  before deploying."
            ),
            json!({}),
        );
    } else if crate::js::utf16_len(value) < RECOMMENDED_LENGTH {
        logger::warn(
            format!(
                "[CONFIG WARNING] SECRET is shorter than the recommended {RECOMMENDED_LENGTH} characters, which\n  weakens the session cookie signature against an offline search — {GENERATE}."
            ),
            json!({ "length": crate::js::utf16_len(value) }),
        );
    }

    // The untrimmed value: trimming would change the signing key of a
    // deployment whose secret has surrounding whitespace.
    Ok(secret.clone())
}
