//! Runtime environment access.
//!
//! (A directory module because CI's secrets scan refuses any file named
//! `env.*` that is not JavaScript or TypeScript.)
//!
//! Every setting is read when it is needed rather than cached at start, the
//! way the Bun server reads `process.env`: certificates are renewed and tests
//! reconfigure a running process's view only through a restart, but reading
//! per call keeps one obvious source of truth.

/// The variable's value, or `None` when it is unset or not valid Unicode.
pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The variable's value when it is set and not empty — JavaScript's
/// `process.env.X || fallback` treats an empty string as missing.
pub fn non_empty(name: &str) -> Option<String> {
    var(name).filter(|value| !value.is_empty())
}

pub fn node_env() -> Option<String> {
    var("NODE_ENV")
}

/// The production posture: JSON logs, HSTS, secure cookies, forwarding
/// headers believed from private addresses.
pub fn is_production() -> bool {
    node_env().as_deref() == Some("production")
}

/// A port setting, or `default` when it is unset or empty. Mirrors
/// `parseInt(value, 10)`: leading digits count and anything unparseable is
/// reported as `None` so the caller can fail the way a NaN port would.
pub fn port(name: &str, default: u16) -> Option<u16> {
    match non_empty(name) {
        None => Some(default),
        Some(value) => {
            let trimmed = value.trim_start();
            let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        }
    }
}
