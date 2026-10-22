//! SASL initial responses for OAuth bearer tokens, shared by `imap` and `smtp`.
//!
//! The plain functions return the **raw, unencoded** initial response; SASL
//! frameworks (e.g. `async-imap`'s `authenticate`) base64-encode it themselves. The
//! `_base64` variants return it encoded, for protocols where the caller writes the
//! line itself (SMTP `AUTH XOAUTH2 <base64>`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mazemail_types::Sensitive;

/// Input that can't go into a SASL response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SaslError {
    /// A field contains a character that would break the message framing.
    #[error("the {0} contains characters not allowed in a SASL response")]
    InvalidCharacter(&'static str),
    /// A field is empty.
    #[error("the {0} is empty")]
    Empty(&'static str),
}

/// The XOAUTH2 initial response (Google, Microsoft):
/// `user=<email>\x01auth=Bearer <token>\x01\x01`, unencoded.
pub fn xoauth2(user: &str, access_token: &str) -> Result<Sensitive<String>, SaslError> {
    check_user(user)?;
    check_token(access_token)?;
    Ok(Sensitive::new(format!(
        "user={user}\x01auth=Bearer {access_token}\x01\x01"
    )))
}

/// [`xoauth2`], base64-encoded.
pub fn xoauth2_base64(user: &str, access_token: &str) -> Result<Sensitive<String>, SaslError> {
    xoauth2(user, access_token).map(|raw| encode(&raw))
}

/// The OAUTHBEARER initial response (RFC 7628 §3.1), unencoded:
/// `n,a=<user>,\x01host=<host>\x01port=<port>\x01auth=Bearer <token>\x01\x01`.
/// `host` and `port` are optional; `,` and `=` in the user are escaped as the GS2
/// header requires (RFC 5801).
pub fn oauthbearer(
    user: &str,
    access_token: &str,
    host: Option<&str>,
    port: Option<u16>,
) -> Result<Sensitive<String>, SaslError> {
    check_user(user)?;
    check_token(access_token)?;
    let mut message = format!("n,a={},\x01", user.replace('=', "=3D").replace(',', "=2C"));
    if let Some(host) = host {
        if host.chars().any(char::is_control) {
            return Err(SaslError::InvalidCharacter("host"));
        }
        message.push_str(&format!("host={host}\x01"));
    }
    if let Some(port) = port {
        message.push_str(&format!("port={port}\x01"));
    }
    message.push_str(&format!("auth=Bearer {access_token}\x01\x01"));
    Ok(Sensitive::new(message))
}

/// [`oauthbearer`], base64-encoded.
pub fn oauthbearer_base64(
    user: &str,
    access_token: &str,
    host: Option<&str>,
    port: Option<u16>,
) -> Result<Sensitive<String>, SaslError> {
    oauthbearer(user, access_token, host, port).map(|raw| encode(&raw))
}

fn encode(raw: &Sensitive<String>) -> Sensitive<String> {
    Sensitive::new(STANDARD.encode(raw.expose()))
}

fn check_user(user: &str) -> Result<(), SaslError> {
    if user.is_empty() {
        return Err(SaslError::Empty("user name"));
    }
    if user.chars().any(char::is_control) {
        return Err(SaslError::InvalidCharacter("user name"));
    }
    Ok(())
}

/// RFC 6750 §2.1 `b64token`: `[A-Za-z0-9-._~+/]+=*`, checked loosely as printable
/// ASCII without spaces, so provider quirks still pass.
fn check_token(token: &str) -> Result<(), SaslError> {
    if token.is_empty() {
        return Err(SaslError::Empty("access token"));
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(SaslError::InvalidCharacter("access token"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn xoauth2_string() {
        let raw = xoauth2(
            "someuser@example.com",
            "ya29.vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg",
        )
        .unwrap();
        assert_eq!(
            raw.expose(),
            "user=someuser@example.com\x01auth=Bearer ya29.vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg\x01\x01"
        );
        // Google's documented example.
        let encoded = xoauth2_base64(
            "someuser@example.com",
            "ya29.vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg",
        )
        .unwrap();
        assert_eq!(
            encoded.expose(),
            "dXNlcj1zb21ldXNlckBleGFtcGxlLmNvbQFhdXRoPUJlYXJlciB5YTI5LnZGOWRmdDRxbVRjMk52YjNSbGNrQmhkSFJoZG1semRHRXVZMjl0Q2cBAQ=="
        );
        assert_eq!(format!("{encoded:?}"), "<redacted>");
    }

    #[test]
    fn oauthbearer_string() {
        // RFC 7628 §4.1, without the line breaks.
        let raw = oauthbearer(
            "user@example.com",
            "vF9dft4qmTc2Nvb3RlckBhbHRhdmlzdGEuY29tCg==",
            Some("server.example.com"),
            Some(143),
        )
        .unwrap();
        assert_eq!(
            raw.expose(),
            "n,a=user@example.com,\x01host=server.example.com\x01port=143\x01auth=Bearer vF9dft4qmTc2Nvb3RlckBhbHRhdmlzdGEuY29tCg==\x01\x01"
        );
        let raw = oauthbearer("a,b=c", "t", None, None).unwrap();
        assert_eq!(raw.expose(), "n,a=a=2Cb=3Dc,\x01auth=Bearer t\x01\x01");
        assert!(oauthbearer_base64("u", "t", None, None).is_ok());
    }

    #[test]
    fn rejects_framing_characters() {
        assert_eq!(
            xoauth2("bob\x01auth=Bearer x", "t"),
            Err(SaslError::InvalidCharacter("user name"))
        );
        assert_eq!(
            xoauth2("bob", "t\x01"),
            Err(SaslError::InvalidCharacter("access token"))
        );
        assert_eq!(
            xoauth2("bob", "a b"),
            Err(SaslError::InvalidCharacter("access token"))
        );
        assert_eq!(xoauth2("", "t"), Err(SaslError::Empty("user name")));
        assert_eq!(xoauth2("bob", ""), Err(SaslError::Empty("access token")));
        assert_eq!(
            oauthbearer("bob", "t", Some("h\x01"), None),
            Err(SaslError::InvalidCharacter("host"))
        );
    }
}
