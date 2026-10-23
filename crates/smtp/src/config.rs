//! Where and how to submit mail.

use std::fmt;
use std::time::Duration;

use mazemail_types::Sensitive;

/// How long one SMTP conversation may take before it is given up.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Transport security of the submission connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Security {
    /// TLS from the first byte (port 465, RFC 8314).
    #[default]
    Tls,
    /// Plain TCP upgraded with `STARTTLS` (port 587); never falls back to plain text.
    StartTls,
    /// No encryption. Only for local test servers: the password travels in clear text.
    PlainInsecure,
}

impl Security {
    /// The usual port.
    pub fn default_port(self) -> u16 {
        match self {
            Self::Tls => 465,
            Self::StartTls => 587,
            Self::PlainInsecure => 25,
        }
    }
}

/// An SMTP submission server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    /// Host name.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Transport security.
    pub security: Security,
    /// Limit for connecting and for each command.
    pub timeout: Duration,
    /// The name sent with `EHLO`; `None` sends `[127.0.0.1]` like Thunderbird, so the
    /// computer's host name doesn't leak into every message's `Received` header.
    pub hello_name: Option<String>,
}

impl SmtpConfig {
    /// A server with the usual port for `security`.
    pub fn new(host: impl Into<String>, security: Security) -> Self {
        Self {
            host: host.into(),
            port: security.default_port(),
            security,
            timeout: DEFAULT_TIMEOUT,
            hello_name: None,
        }
    }

    /// The same server on `port`.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// `host:port`, for messages.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// How to log in. Passwords and tokens are [`Sensitive`] and never logged.
#[derive(Clone, PartialEq, Eq)]
pub enum Credentials {
    /// No `AUTH` (a relay that trusts the network).
    None,
    /// `AUTH PLAIN` or `AUTH LOGIN`, whichever the server offers (PLAIN first).
    Password {
        /// The login name.
        user: String,
        /// The password.
        password: Sensitive<String>,
    },
    /// `AUTH XOAUTH2` with an OAuth access token (Google, Microsoft).
    XOAuth2 {
        /// The account's address.
        user: String,
        /// The access token.
        access_token: Sensitive<String>,
    },
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "Credentials::None",
            Self::Password { .. } => "Credentials::Password(<redacted>)",
            Self::XOAuth2 { .. } => "Credentials::XOAuth2(<redacted>)",
        })
    }
}
