//! Errors of the transport and the outbox.

/// The fix-it text for Exchange's `554 5.2.252 SendAsDenied` (CONCEPT §8.6).
pub const SEND_AS_DENIED_EXPLANATION: &str = "Microsoft 365 refused to send from this \
     address (SendAsDenied). If it is an alias of your mailbox, your administrator has to \
     allow sending from aliases (Set-OrganizationConfig -SendFromAliasEnabled $true). If it \
     belongs to another or a shared mailbox, your user needs the \"Send As\" (or \"Send on \
     Behalf\") permission on it. The message stays in the outbox.";

/// Why a message couldn't be submitted.
///
/// Texts come from the server or the network stack, never from the message, so every
/// variant is safe to log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SmtpError {
    /// Exchange refused the From address (`554 5.2.252 SendAsDenied`).
    #[error("{SEND_AS_DENIED_EXPLANATION}")]
    SendAsDenied {
        /// The server's reply.
        reply: String,
    },
    /// The server rejected the login.
    #[error("the SMTP server rejected the login: {message}")]
    AuthFailed {
        /// The server's reply.
        message: String,
    },
    /// The server rejected the message or a recipient.
    #[error("the SMTP server refused the message ({code}): {message}")]
    Rejected {
        /// The reply code (`550`, `452`, …).
        code: u16,
        /// The server's reply text.
        message: String,
        /// A `5xx` reply: trying again won't help.
        permanent: bool,
    },
    /// The server can't be reached, or the connection broke.
    #[error("can't reach the SMTP server: {message}")]
    Connection {
        /// What failed.
        message: String,
    },
    /// TLS failed (certificate, handshake, no STARTTLS offered).
    #[error("the secure connection to the SMTP server failed: {message}")]
    Tls {
        /// What failed.
        message: String,
    },
    /// No answer in time.
    #[error("the SMTP server didn't answer in time")]
    Timeout,
    /// An address in the envelope isn't valid.
    #[error("not a valid address for sending: {message}")]
    InvalidAddress {
        /// What is wrong (no address text).
        message: String,
    },
    /// No credentials: no password stored, sign-in expired, keychain locked.
    #[error("{message}")]
    Credentials {
        /// What happened and what to do.
        message: String,
        /// The user has to act (sign in again, enter the password); retrying alone
        /// won't help.
        needs_user: bool,
    },
    /// The outbox item can't be sent as stored (blob missing, no envelope).
    #[error("the message can't be sent: {0}")]
    Invalid(String),
}

impl SmtpError {
    /// Trying again later won't help (the message, the address or the login is
    /// refused); the outbox marks the item failed instead of retrying.
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::SendAsDenied { .. }
            | Self::AuthFailed { .. }
            | Self::InvalidAddress { .. }
            | Self::Invalid(_) => true,
            Self::Rejected { permanent, .. } => *permanent,
            Self::Credentials { needs_user, .. } => *needs_user,
            Self::Connection { .. } | Self::Tls { .. } | Self::Timeout => false,
        }
    }

    /// The login was refused (or no credentials are available).
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::AuthFailed { .. } | Self::Credentials { .. })
    }

    /// A short name of the failure for logs. The server's reply text may name a
    /// recipient, so logs carry this instead.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SendAsDenied { .. } => "send-as-denied",
            Self::AuthFailed { .. } => "auth-failed",
            Self::Rejected { .. } => "rejected",
            Self::Connection { .. } => "connection",
            Self::Tls { .. } => "tls",
            Self::Timeout => "timeout",
            Self::InvalidAddress { .. } => "invalid-address",
            Self::Credentials { .. } => "credentials",
            Self::Invalid(_) => "invalid",
        }
    }

    /// The fix-it text for this error, if there is one beyond the message.
    pub fn fix_it(&self) -> Option<&'static str> {
        matches!(self, Self::SendAsDenied { .. }).then_some(SEND_AS_DENIED_EXPLANATION)
    }

    /// Classifies a server reply: `554 5.2.252` / `SendAsDenied` is
    /// [`SmtpError::SendAsDenied`], `535`/`534`/`530` an authentication failure, other
    /// codes [`SmtpError::Rejected`].
    pub fn from_reply(code: u16, message: &str) -> Self {
        let message = message.trim().to_owned();
        if (code == 554 || code == 550) && is_send_as_denied(&message) {
            return Self::SendAsDenied { reply: message };
        }
        match code {
            // 454 (temporary authentication failure, or TLS not available) stays a
            // temporary rejection: retrying may help.
            530 | 534 | 535 => Self::AuthFailed { message },
            _ => Self::Rejected {
                code,
                permanent: (500..600).contains(&code),
                message,
            },
        }
    }
}

fn is_send_as_denied(message: &str) -> bool {
    message.contains("5.2.252") || message.to_ascii_lowercase().contains("sendasdenied")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_classified() {
        let denied = SmtpError::from_reply(
            554,
            "5.2.252 SendAsDenied; anna@distr.example not allowed to send as \
             you@distr.example; STOREDRV.Submission.Exception:SendAsDeniedException",
        );
        assert!(matches!(denied, SmtpError::SendAsDenied { .. }));
        assert!(denied.is_permanent());
        assert_eq!(denied.fix_it(), Some(SEND_AS_DENIED_EXPLANATION));
        assert!(denied.to_string().contains("Send As"));

        let auth = SmtpError::from_reply(535, "5.7.8 Username and Password not accepted");
        assert!(auth.is_auth() && auth.is_permanent());

        let full = SmtpError::from_reply(452, "4.2.2 Mailbox full");
        assert!(!full.is_permanent());
        let unknown = SmtpError::from_reply(550, "5.1.1 User unknown");
        assert!(matches!(
            unknown,
            SmtpError::Rejected {
                code: 550,
                permanent: true,
                ..
            }
        ));
        assert!(
            !SmtpError::from_reply(454, "4.7.0 Temporary authentication failure").is_permanent()
        );
        assert!(!SmtpError::Timeout.is_permanent());
    }
}
