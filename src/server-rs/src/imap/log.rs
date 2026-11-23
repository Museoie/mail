//! The IMAP server's DEBUG and INFO lines. Tests read back what reached the
//! journal: that is how they keep credentials out of it and pin the audit
//! lines.

use serde_json::Value;

use crate::logger;

pub(crate) fn debug(message: &str, context: Value) {
    #[cfg(test)]
    super::test_support::journal("debug", message, &context);
    logger::debug(message, context);
}

pub(crate) fn info(message: &str, context: Value) {
    #[cfg(test)]
    super::test_support::journal("info", message, &context);
    logger::info(message, context);
}
