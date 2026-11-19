//! Gravatar (ADR-032): the picture someone registered for their address at
//! gravatar.com, for contacts without a photo of their own, looked up only when the
//! user turned it on (`contacts.gravatar`). Gravatar learns the address's SHA-256 hash
//! (of the trimmed, lowercase address, as it asks for it) and the user's IP address;
//! the picture is fetched like a contact photo, without credentials, cookies or
//! `Referer` ([`crate::fetch_photo`]).

use sha2::{Digest as _, Sha256};

/// Gravatar's base URL.
pub const GRAVATAR: &str = "https://gravatar.com";

/// The size asked for, in pixels (Google's contact photos are asked for at 96 too).
const SIZE: u32 = 96;

/// The URL of the Gravatar of `email` at `base` ([`GRAVATAR`], or a mock server in
/// tests). Gravatar answers 404 when there is none (`d=404`), so no generated picture
/// replaces the initials.
pub fn gravatar_url(base: &str, email: &str) -> String {
    let hash = Sha256::digest(email.trim().to_lowercase().as_bytes());
    let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}/avatar/{hex}?s={SIZE}&d=404", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_the_trimmed_lowercase_address() {
        let url = "https://gravatar.com/avatar/\
                   f7d54c22dda5b1780276601cd005909b70543fc4a6ba72d735b4cb9bbb611f01?s=96&d=404";
        assert_eq!(gravatar_url(GRAVATAR, "anna@example.com"), url);
        assert_eq!(
            gravatar_url("https://gravatar.com/", "  Anna@Example.COM\n"),
            url
        );
    }
}
