//! Concurrency limits read from the environment.

use serde_json::json;

use crate::logger;

/// A concurrency limit from `raw`: `default_value` when unset or empty, and
/// also — with a warning — when it does not parse to a whole number of at
/// least 1. Leading digits count, as with `parseInt`.
pub fn parse_concurrency_value(raw: Option<&str>, env_var: &str, default_value: usize, label: &str) -> usize {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return default_value;
    };
    let trimmed = raw.trim_start();
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let leading: String = digits.chars().take_while(char::is_ascii_digit).collect();
    match leading.parse::<usize>() {
        Ok(parsed) if !negative && parsed >= 1 => parsed,
        _ => {
            logger::warn(
                format!("[{label}] {env_var} invalid, falling back to default"),
                json!({ "raw": raw, "default": default_value }),
            );
            default_value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values() {
        assert_eq!(parse_concurrency_value(None, "X", 3, "t"), 3);
        assert_eq!(parse_concurrency_value(Some(""), "X", 3, "t"), 3);
        assert_eq!(parse_concurrency_value(Some("8"), "X", 3, "t"), 8);
        assert_eq!(parse_concurrency_value(Some(" 5x"), "X", 3, "t"), 5);
        assert_eq!(parse_concurrency_value(Some("0"), "X", 3, "t"), 3);
        assert_eq!(parse_concurrency_value(Some("-2"), "X", 3, "t"), 3);
        assert_eq!(parse_concurrency_value(Some("abc"), "X", 3, "t"), 3);
    }
}
