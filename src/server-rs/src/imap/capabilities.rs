//! The CAPABILITY list (imap/capabilities.ts).

use crate::tls::is_tls_available;

/// The capability string for a connection.
///
/// `is_tls` is whether the connection is already encrypted, and
/// `sso_enabled` whether single sign-on is on (the Bun server's
/// `getSso() !== null`). Whether STARTTLS can be honoured is read from the
/// certificate settings.
///
/// ```ignore
/// write(&format!("* CAPABILITY {}\r\n", get_capabilities(false, sso_enabled)));
/// ```
pub fn get_capabilities(is_tls: bool, sso_enabled: bool) -> String {
    // The certificate is only looked at on the plain port.
    capabilities(is_tls, sso_enabled, !is_tls && is_tls_available())
}

/// The capability string, given whether a certificate is readable.
pub fn capabilities(is_tls: bool, sso_enabled: bool, tls_available: bool) -> String {
    let mut capabilities = vec![
        "IMAP4rev1",
        "LITERAL+",
        "SASL-IR",
        "LOGIN-REFERRALS",
        "ID",
        "ENABLE",
        "IDLE",
        "MOVE",
        "CONDSTORE",
        // No SPECIAL-USE on purpose. RFC 6154 §2 is explicit that the
        // attributes a plain LIST emits need no capability; the capability
        // denotes the LIST-EXTENDED selection and return options (RFC 5258),
        // which this server's parser rejects. Advertising it would make a
        // client send `LIST (SPECIAL-USE) "" "*"` and get BAD.
        "AUTH=PLAIN",
    ];

    // With single sign-on an app password is a full credential for the
    // protocol, so none is taken before TLS (RFC 3501 §6.2.3).
    if !is_tls && sso_enabled {
        if let Some(slot) = capabilities.iter_mut().find(|capability| **capability == "AUTH=PLAIN") {
            *slot = "LOGINDISABLED";
        }
    }

    // Plain port: advertise STARTTLS so clients can upgrade, but only when
    // the certificate the upgrade reads is one this process can read. RFC
    // 2595 §3: a server must not advertise a capability it cannot honour.
    // The TLS-wrapped port already has an encrypted channel.
    if !is_tls && tls_available {
        capabilities.push("STARTTLS");
    }

    capabilities.join(" ")
}
