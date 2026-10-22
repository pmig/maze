//! Connection settings for [`ImapClient`](crate::ImapClient).

use std::time::Duration;

use mazemail_types::Sensitive;
use rustls::pki_types::CertificateDer;

/// How the connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Implicit TLS from the first byte (port 993). The default.
    Tls,
    /// Plain TCP upgraded with `STARTTLS` before authenticating (port 143).
    /// Fails if the server does not offer `STARTTLS`; never falls back to plaintext.
    StartTls,
    /// Unencrypted TCP. Only for tests against a local server: credentials travel
    /// in clear text.
    PlainInsecure,
}

/// How server certificates are verified for [`Security::Tls`] and [`Security::StartTls`].
#[derive(Debug, Clone, Default)]
pub enum TlsVerification {
    /// The operating system's verifier and trust store (Keychain on macOS), via
    /// `rustls-platform-verifier`. The default.
    #[default]
    Platform,
    /// Mozilla's root store bundled at build time (`webpki-roots`). A fallback for
    /// systems without a usable platform verifier.
    WebPkiRoots,
    /// Only the given roots, e.g. a self-signed test server's certificate.
    CustomRoots(Vec<CertificateDer<'static>>),
}

/// How the client authenticates.
#[derive(Clone)]
pub enum ImapAuth {
    /// `AUTHENTICATE PLAIN` if offered, else `LOGIN`.
    Password {
        /// Login name.
        user: String,
        /// Password; never printed.
        password: Sensitive<String>,
    },
    /// `AUTHENTICATE XOAUTH2` (Gmail, Microsoft 365) with a bearer access token.
    XOAuth2 {
        /// Account address.
        user: String,
        /// OAuth2 access token; never printed.
        access_token: Sensitive<String>,
    },
}

impl std::fmt::Debug for ImapAuth {
    // The user name is an address, so it is redacted too.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { .. } => f
                .debug_struct("Password")
                .field("user", &"<redacted>")
                .field("password", &"<redacted>")
                .finish(),
            Self::XOAuth2 { .. } => f
                .debug_struct("XOAuth2")
                .field("user", &"<redacted>")
                .field("access_token", &"<redacted>")
                .finish(),
        }
    }
}

impl ImapAuth {
    /// Password authentication.
    pub fn password(user: impl Into<String>, password: impl Into<String>) -> Self {
        Self::Password {
            user: user.into(),
            password: Sensitive::new(password.into()),
        }
    }

    /// XOAUTH2 authentication.
    pub fn xoauth2(user: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self::XOAuth2 {
            user: user.into(),
            access_token: Sensitive::new(access_token.into()),
        }
    }
}

/// Everything needed to open an authenticated IMAP session.
#[derive(Debug, Clone)]
pub struct ImapConfig {
    /// Server host name; also used for TLS server name verification.
    pub host: String,
    /// Server port (993 for TLS, 143 for STARTTLS).
    pub port: u16,
    /// Transport security.
    pub security: Security,
    /// Credentials.
    pub auth: ImapAuth,
    /// Certificate verification.
    pub tls_verification: TlsVerification,
    /// Limit for TCP connect, TLS handshake, greeting and authentication, each.
    pub connect_timeout: Duration,
    /// Limit for each read or write while a command runs. Long transfers stay alive as
    /// long as data keeps flowing.
    pub command_timeout: Duration,
    /// Negotiate `COMPRESS=DEFLATE` (RFC 4978) when the server offers it.
    pub compress: bool,
    /// Send `ID` (RFC 2971) with these fields when the server offers it. Some
    /// providers (NetEase, 163.com) refuse access without it.
    pub client_id: Option<Vec<(String, String)>>,
}

impl ImapConfig {
    /// Settings with defaults: TLS, platform verifier, 30 s connect timeout,
    /// 60 s command timeout, compression off, `ID` with name "Maze" and the version.
    pub fn new(host: impl Into<String>, port: u16, auth: ImapAuth) -> Self {
        Self {
            host: host.into(),
            port,
            security: Security::Tls,
            auth,
            tls_verification: TlsVerification::Platform,
            connect_timeout: Duration::from_secs(30),
            command_timeout: Duration::from_secs(60),
            compress: false,
            client_id: Some(vec![
                ("name".to_owned(), mazemail_types::PRODUCT_NAME.to_owned()),
                ("version".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
            ]),
        }
    }

    /// Set [`ImapConfig::security`].
    #[must_use]
    pub fn with_security(mut self, security: Security) -> Self {
        self.security = security;
        self
    }

    /// Set [`ImapConfig::tls_verification`].
    #[must_use]
    pub fn with_tls_verification(mut self, verification: TlsVerification) -> Self {
        self.tls_verification = verification;
        self
    }

    /// Set [`ImapConfig::command_timeout`].
    #[must_use]
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// Set [`ImapConfig::connect_timeout`].
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set [`ImapConfig::compress`].
    #[must_use]
    pub fn with_compress(mut self, compress: bool) -> Self {
        self.compress = compress;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_secrets() {
        let cfg = ImapConfig::new(
            "imap.example.org",
            993,
            ImapAuth::password("alice@example.org", "hunter2"),
        );
        let printed = format!("{cfg:?}");
        assert!(!printed.contains("hunter2"));
        assert!(!printed.contains("alice@example.org"));
        let oauth = ImapAuth::xoauth2("bob@example.org", "ya29.token");
        let printed = format!("{oauth:?}");
        assert!(!printed.contains("ya29"));
        assert!(!printed.contains("bob@"));
    }
}
