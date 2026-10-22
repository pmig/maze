use std::fmt;

use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// A string that holds key material: zeroed on drop, never printed.
///
/// Like `mazemail_types::Sensitive<String>`, `Debug` prints `<redacted>`; unlike it, the
/// buffer is wiped when the value is dropped, and there is no `Display`, `Clone` or
/// `Serialize`. Call [`SecretString::expose_secret`] at the single place that needs the
/// text (for example when building the SQLCipher `PRAGMA key` statement).
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// Returns the secret text.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl Zeroize for SecretString {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl ZeroizeOnDrop for SecretString {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let secret = SecretString::new("x'00ff'".to_owned());
        assert_eq!(format!("{secret:?}"), "SecretString(<redacted>)");
        assert_eq!(secret.expose_secret(), "x'00ff'");
    }
}
