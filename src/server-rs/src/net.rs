//! Listening sockets shared by the mail servers.

use tokio::net::TcpListener;

/// Binds `port` on every address: dual-stack IPv6 where the machine has
/// IPv6, otherwise the IPv4 wildcard.
pub async fn bind_all(port: u16) -> std::io::Result<TcpListener> {
    match TcpListener::bind(("::", port)).await {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind(("0.0.0.0", port)).await,
    }
}
