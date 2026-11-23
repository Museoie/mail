//! The IMAP server.
//!
//! `initialize_imap` opens the listeners (`server.rs`). Each connection is
//! driven by `handler.rs`, which reads commands off the socket and hands
//! them to the connection's `session.rs`; the sessions are known to
//! `session_registry.rs` (credential sweeps, shutdown) and, while in IDLE,
//! to `idle_manager.rs`.

pub mod auth;
pub mod body_budget;
pub mod capabilities;
pub mod chunked_write;
pub mod close_socket;
pub mod command_budget;
pub mod command_budget_hold;
pub mod credential_version;
pub mod fetch_format;
pub mod fetch_stream;
pub mod handler;
pub mod idle_manager;
pub mod literal_spool;
mod log;
pub mod mailbox_ops;
pub mod message_ops;
pub mod parsers;
pub mod sequence_resolver;
pub mod server;
pub mod session;
pub mod session_buffer;
pub mod session_format;
pub mod session_registry;
pub mod store;
pub mod socket;
pub mod stream_mutex;
#[cfg(test)]
mod stream_slot_tests;
#[cfg(test)]
pub(crate) mod test_support;
pub mod throttler;
pub mod trace;
pub mod types;
pub mod util;

pub use server::{close_imap_servers, initialize_imap, ImapServers, IMAP_SHUTDOWN_TIMEOUT_MS};
