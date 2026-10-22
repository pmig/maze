//! Helpers shared by the integration tests: fixtures, test keys, signing.
//!
//! Keys are generated per test run and messages are signed with `mail-auth`'s own
//! signer, so the tests are hermetic: no network, no checked-in private keys.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used)]

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use mail_auth::arc::ArcSealer;
use mail_auth::common::crypto::{Ed25519Key, RsaKey, Sha256, SigningKey};
use mail_auth::common::headers::HeaderWriter;
use mail_auth::dkim::DkimSigner;
use mail_auth::{ArcOutput, AuthenticatedMessage, AuthenticationResults};

pub const SIGNED_HEADERS: [&str; 5] = ["From", "To", "Subject", "Date", "Message-ID"];

/// A fixture with CRLF line endings.
pub fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    text.replace("\r\n", "\n")
        .replace('\n', "\r\n")
        .into_bytes()
}

/// A signing key and its DNS TXT record.
pub struct TestKey<K> {
    pub key: K,
    pub record: String,
}

pub fn ed25519_key() -> TestKey<Ed25519Key> {
    let pkcs8 = Ed25519Key::generate_pkcs8().unwrap();
    let key = Ed25519Key::from_pkcs8_der(&pkcs8).unwrap();
    let record = format!("v=DKIM1; k=ed25519; p={}", BASE64.encode(key.public_key()));
    TestKey { key, record }
}

pub fn rsa_key() -> TestKey<RsaKey<Sha256>> {
    use aws_lc_rs::encoding::AsDer;
    let pair = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
    let der = pair.as_der().unwrap();
    #[allow(deprecated)]
    let key = RsaKey::<Sha256>::from_pkcs8_der(der.as_ref()).unwrap();
    let record = format!("v=DKIM1; k=rsa; p={}", BASE64.encode(key.public_key()));
    TestKey { key, record }
}

/// Prepend a DKIM-Signature for `domain`/`selector` to `message`.
pub fn dkim_sign<K: SigningKey>(key: K, domain: &str, selector: &str, message: &[u8]) -> Vec<u8> {
    let signature = DkimSigner::from_key(key)
        .domain(domain)
        .selector(selector)
        .headers(SIGNED_HEADERS)
        .sign(message)
        .unwrap();
    [signature.to_header().as_bytes(), message].concat()
}

/// Seal `message` as the first ARC hop at `domain`/`selector`.
pub fn arc_seal<K: SigningKey<Hasher = Sha256>>(
    key: K,
    domain: &str,
    selector: &str,
    message: &[u8],
) -> Vec<u8> {
    let parsed = AuthenticatedMessage::parse(message).unwrap();
    let results = AuthenticationResults::new(domain);
    let set = ArcSealer::from_key(key)
        .domain(domain)
        .selector(selector)
        .headers(SIGNED_HEADERS)
        .seal(&parsed, &results, &ArcOutput::default())
        .unwrap();
    [set.to_header().as_bytes(), message].concat()
}

/// Insert `header` (a full line without CRLF) at the top of `message`.
pub fn prepend(header: &str, message: &[u8]) -> Vec<u8> {
    [header.as_bytes(), b"\r\n", message].concat()
}

/// Replace the first occurrence of `from` with `to`.
pub fn replace(message: &[u8], from: &str, to: &str) -> Vec<u8> {
    let text = String::from_utf8(message.to_vec()).unwrap();
    assert!(text.contains(from), "{from:?} not in message");
    text.replacen(from, to, 1).into_bytes()
}
