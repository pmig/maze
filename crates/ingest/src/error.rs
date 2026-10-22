/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors from ingesting a message.
///
/// No variant carries message content, so every error is safe to log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The bytes aren't a message (no header could be read).
    #[error("not a message: {0}")]
    Parse(#[from] mazemail_mime::ParseError),

    /// The store rejected the write or the blob.
    #[error(transparent)]
    Store(#[from] store::Error),

    /// The input can't be ingested as given (for example a message without locations).
    #[error("invalid input: {0}")]
    Invalid(&'static str),
}
