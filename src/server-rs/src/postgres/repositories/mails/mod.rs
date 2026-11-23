//! The `mails` table and everything keyed on it.

pub mod core;
pub mod core_query;
pub mod counters;
pub mod domain_scope;
pub mod http;
pub mod http_query;
pub mod imap;
pub mod imap_query;
pub mod inflight;
pub mod mapping_decisions;
pub mod model;
pub mod pivot_decisions;
mod rfc822_size;
pub mod set_flags_query;
pub mod table;
pub mod thread;
pub mod views;

#[cfg(test)]
mod golden_tests;

pub use self::core::*;
pub use self::counters::*;
pub use self::model::*;
pub use self::views::*;
