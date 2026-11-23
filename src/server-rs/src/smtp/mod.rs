//! The SMTP server.
//!
//! The protocol — listeners, connections and the command state machine — is
//! in `server`, `connection` and `listeners`. What happens to the mail is
//! behind [`SmtpHandler`], the hooks a listener calls for AUTH, MAIL FROM,
//! RCPT TO and DATA; [`MailHandler`] (`handler`, `lanes`) is this server's.

pub mod address_command;
pub mod bounded;
pub mod connection;
pub mod data_budget;
pub mod data_stream;
pub mod handler;
#[cfg(test)]
mod handler_tests;
mod lanes;
pub mod listeners;
pub mod outgoing;
pub mod registry;
pub mod server;
pub mod transaction_dir;
pub mod types;

pub use data_budget::{with_smtp_data_budget, DataBudgetUnavailableError};
pub use data_stream::{SmtpDataAbandoned, SmtpDataStream, SmtpDataTooLarge};
pub use handler::{initialize_mail_smtp, MailHandler};
pub use listeners::{close_smtp_servers, initialize_smtp, SmtpListeners};
pub use registry::{evict_smtp_connections, register_smtp_server, unregister_smtp_server};
pub use server::{create_smtp_server, HandshakeErrorMeta, SmtpServer, SmtpServerError};
pub use types::{
    SharedSmtpHandler, SmtpAddress, SmtpArgValue, SmtpAuthMethod, SmtpAuthRequest, SmtpAuthResponse, SmtpEnvelope,
    SmtpHandler, SmtpHookError, SmtpServerOptions, SmtpSession, SmtpSessionState, SmtpTlsSource,
};
