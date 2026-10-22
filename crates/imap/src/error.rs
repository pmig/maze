//! Error type of the IMAP layer.

use std::io;
use std::time::Duration;

use thiserror::Error;

/// Result alias for this crate.
pub type Result<T, E = ImapError> = std::result::Result<T, E>;

/// Everything that can go wrong talking to an IMAP server.
///
/// Error messages never contain credentials or message content. Server response
/// text (`NO`/`BAD`) is included because it is needed to diagnose problems; it
/// describes the failure, not the mail.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ImapError {
    /// Network-level failure (DNS, TCP, reading or writing the socket).
    #[error("network error: {0}")]
    Io(#[source] io::Error),

    /// TLS setup or handshake failed (untrusted certificate, protocol mismatch, …).
    #[error("TLS error: {0}")]
    Tls(String),

    /// An operation did not finish in time. The session is unusable afterwards.
    #[error("{operation} timed out after {after:?}")]
    Timeout {
        /// What was running, e.g. `connect` or `UID FETCH`.
        operation: &'static str,
        /// The limit that was hit.
        after: Duration,
    },

    /// The server rejected the credentials, or no supported mechanism was offered.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The server answered `NO`.
    #[error("{command} failed: {text}")]
    No {
        /// The command verb, e.g. `SELECT`.
        command: &'static str,
        /// The server's response code, if any (e.g. `TRYCREATE`, `NONEXISTENT`).
        code: Option<String>,
        /// Human-readable server text.
        text: String,
    },

    /// The server answered `BAD`: the command was malformed or not allowed now.
    #[error("{command} rejected as bad: {text}")]
    Bad {
        /// The command verb.
        command: &'static str,
        /// Human-readable server text.
        text: String,
    },

    /// The server closed the session (`BYE`) or the connection dropped.
    #[error("connection closed by server{}", .0.as_deref().map(|t| format!(": {t}")).unwrap_or_default())]
    ConnectionLost(Option<String>),

    /// A previous error (timeout, I/O failure, protocol violation) left the session in
    /// an unknown state. Reconnect.
    #[error("session is broken after an earlier error; reconnect")]
    Broken,

    /// The server sent something the client could not understand. Raw response bytes
    /// are deliberately not included because they may contain mail data.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The operation needs a capability the server does not advertise.
    #[error("server does not support {0}")]
    Unsupported(&'static str),

    /// A caller-provided value cannot be sent to the server (e.g. a keyword with spaces).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The requested message does not exist (anymore) in the selected folder.
    #[error("message UID {uid} not found")]
    MessageNotFound {
        /// The UID that was requested.
        uid: u32,
    },

    /// The message exists, but the requested body section does not.
    #[error("body section {section:?} of message UID {uid} not found")]
    PartNotFound {
        /// The message UID.
        uid: u32,
        /// The requested section.
        section: String,
    },

    /// The command needs a selected folder, but none is selected.
    #[error("no folder selected")]
    NoFolderSelected,
}

impl ImapError {
    /// Whether the session can still be used after this error.
    ///
    /// `NO` responses, unsupported features and invalid input leave the connection
    /// intact; network failures, timeouts and protocol errors do not.
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::No { .. }
                | Self::Bad { .. }
                | Self::Unsupported(_)
                | Self::InvalidInput(_)
                | Self::MessageNotFound { .. }
                | Self::PartNotFound { .. }
                | Self::NoFolderSelected
        )
    }

    /// Whether retrying later (on a fresh connection) may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Io(_) | Self::Timeout { .. } | Self::ConnectionLost(_) | Self::Broken
        )
    }

    /// Map an I/O error from the response stream. async-imap reports unparseable
    /// responses as `io::Error`s whose message embeds the raw response bytes, which
    /// may be mail content; those are replaced by a content-free protocol error.
    pub(crate) fn from_stream_io(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::Other && err.to_string().contains("during parsing of") {
            Self::Protocol("unparseable server response".to_owned())
        } else if err.kind() == io::ErrorKind::UnexpectedEof {
            Self::ConnectionLost(None)
        } else {
            Self::Io(err)
        }
    }
}

impl From<async_imap::error::Error> for ImapError {
    fn from(err: async_imap::error::Error) -> Self {
        use async_imap::error::Error as E;
        match err {
            E::Io(io) => Self::from_stream_io(io),
            E::Bad(text) => Self::Bad {
                command: "command",
                text,
            },
            E::No(text) => Self::No {
                command: "command",
                code: None,
                text,
            },
            E::ConnectionLost => Self::ConnectionLost(None),
            E::Parse(_) => Self::Protocol("unparseable server response".to_owned()),
            E::Validate(v) => Self::InvalidInput(format!("character {:?} not allowed", v.0)),
            E::Append => Self::Protocol("APPEND was not accepted".to_owned()),
            _ => Self::Protocol("unexpected client error".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_errors_do_not_leak_response_bytes() {
        let io =
            io::Error::other("Error(..) during parsing of \"* 1 FETCH (BODY[] {5}\r\nhello)\"");
        let err = ImapError::from_stream_io(io);
        assert!(matches!(err, ImapError::Protocol(_)));
        assert!(!err.to_string().contains("hello"));
    }

    #[test]
    fn classification() {
        assert!(ImapError::Unsupported("MOVE").is_recoverable());
        assert!(!ImapError::Broken.is_recoverable());
        assert!(ImapError::ConnectionLost(None).is_transient());
        assert_eq!(
            ImapError::ConnectionLost(Some("bye".into())).to_string(),
            "connection closed by server: bye"
        );
    }
}
