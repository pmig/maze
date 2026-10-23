//! Signed release manifests (PLAN §4.15).
//!
//! A manifest is a JSON object served at
//! `https://releases.mazemail.dev/{channel}/{os}-{arch}.json`. Its `signature` field is
//! the base64 Ed25519 signature over the [canonical JSON](canonical_json) of the object
//! without that field.
//!
//! Verification works on the JSON exactly as received, not on a re-serialization of
//! [`Manifest`], so fields added by newer releases are covered by the signature too and
//! signing ([`Manifest::sign`], used by `cargo xtask release-manifest`) and verifying
//! ([`verify_manifest`]) share one canonicalization.

use std::cmp::Ordering;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use release_channel::ReleaseChannel;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use url::Url;

/// The field holding the signature.
pub const SIGNATURE_FIELD: &str = "signature";

/// The default host of the update manifests.
pub const DEFAULT_MANIFEST_BASE_URL: &str = "https://releases.mazemail.dev";

/// A release manifest for one channel and platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The release version.
    pub version: Version,
    /// The short commit the release was built from.
    pub commit: String,
    /// The channel feed this manifest is published to (a stable release copied to the
    /// preview feed is signed with `preview`).
    pub channel: ReleaseChannel,
    /// The operating system of the artifact (`macos`, `linux`). Not in the PLAN
    /// example; it stops a validly signed manifest for one platform from being served
    /// on another platform's URL.
    pub os: String,
    /// The CPU architecture of the artifact (`aarch64`, `x86_64`).
    pub arch: String,
    /// When the release was published.
    #[serde(with = "time::serde::rfc3339")]
    pub published: OffsetDateTime,
    /// Where to download the artifact.
    pub url: Url,
    /// Artifact size in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the artifact.
    pub sha256: String,
    /// The oldest database schema this release can migrate from. If the installed
    /// schema is older, an intermediate release must be installed first. `0`: any.
    #[serde(default)]
    pub min_schema_from: u32,
    /// The release notes.
    pub notes_url: Url,
}

/// Why a manifest was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// Not JSON, not an object, or a field is missing or has the wrong type.
    #[error("malformed update manifest: {0}")]
    Malformed(String),
    /// There is no `signature` field.
    #[error("the update manifest is not signed")]
    MissingSignature,
    /// The signature isn't base64 of 64 bytes.
    #[error("the update manifest signature is malformed")]
    MalformedSignature,
    /// This build has no public key to verify with.
    #[error("this build has no update signing keys")]
    NoTrustedKeys,
    /// No trusted key produced the signature.
    #[error("the update manifest signature is invalid")]
    BadSignature,
    /// The manifest is for another channel.
    #[error("the update manifest is for the {found} channel, expected {expected}")]
    WrongChannel {
        /// The channel that was asked for.
        expected: ReleaseChannel,
        /// The channel in the manifest.
        found: ReleaseChannel,
    },
    /// The manifest is for another platform.
    #[error("the update manifest is for {found}, expected {expected}")]
    WrongPlatform {
        /// This platform.
        expected: Platform,
        /// The platform in the manifest.
        found: Platform,
    },
    /// The manifest offers an older version.
    #[error("the update manifest offers {offered}, older than the installed {current}")]
    Downgrade {
        /// The installed version.
        current: Version,
        /// The offered version.
        offered: Version,
    },
    /// `sha256` isn't 64 lowercase hex digits.
    #[error("the update manifest has an invalid sha256")]
    InvalidSha256,
    /// The artifact URL isn't http(s).
    #[error("the update manifest has an unsupported artifact URL scheme {0:?}")]
    InvalidUrl(String),
}

/// An operating system and CPU architecture, as used in manifest URLs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Platform {
    /// `macos` or `linux` (Rust's `std::env::consts::OS`).
    pub os: String,
    /// `aarch64` or `x86_64` (Rust's `std::env::consts::ARCH`).
    pub arch: String,
}

impl Platform {
    /// The platform this binary was built for.
    pub fn current() -> Self {
        Self::new(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// A platform from its parts.
    pub fn new(os: impl Into<String>, arch: impl Into<String>) -> Self {
        Self {
            os: os.into(),
            arch: arch.into(),
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.os, self.arch)
    }
}

/// The manifest URL for a channel and platform:
/// `{base}/{channel}/{os}-{arch}.json`.
pub fn manifest_url(base: &str, channel: ReleaseChannel, platform: &Platform) -> String {
    format!(
        "{}/{}/{}.json",
        base.trim_end_matches('/'),
        channel.name(),
        platform
    )
}

/// What to do with a verified manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    /// The manifest offers a newer version.
    Newer(Box<Manifest>),
    /// The installed version is the offered one.
    UpToDate,
}

impl Manifest {
    /// The `.dmg` published next to a macOS `.app.tar.zst` artifact (same release, same
    /// name), for installs Maze can't update in place. `None` for other artifacts.
    pub fn dmg_url(&self) -> Option<Url> {
        let path = self.url.path().strip_suffix(".app.tar.zst")?;
        let mut url = self.url.clone();
        url.set_path(&format!("{path}.dmg"));
        url.set_query(None);
        Some(url)
    }

    /// Signs the manifest and returns it as pretty-printed JSON with a `signature`
    /// field (pretty so release scripts can grep it; verification canonicalizes).
    pub fn sign(&self, key: &SigningKey) -> Result<String, ManifestError> {
        #[derive(Serialize)]
        struct Signed<'a> {
            #[serde(flatten)]
            manifest: &'a Manifest,
            signature: String,
        }

        let value =
            serde_json::to_value(self).map_err(|err| ManifestError::Malformed(err.to_string()))?;
        let signature = key.sign(&canonical_json(&value));
        // Written in field order, for people; verification canonicalizes.
        let mut json = serde_json::to_string_pretty(&Signed {
            manifest: self,
            signature: STANDARD.encode(signature.to_bytes()),
        })
        .map_err(|err| ManifestError::Malformed(err.to_string()))?;
        json.push('\n');
        Ok(json)
    }
}

/// Checks the signature of a manifest as received and parses it.
///
/// The signature must verify (strictly, rejecting malleable signatures) with one of
/// `trusted_keys`. Nothing is parsed into a [`Manifest`] before that.
pub fn verify_manifest(
    bytes: &[u8],
    trusted_keys: &[VerifyingKey],
) -> Result<Manifest, ManifestError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| ManifestError::Malformed(err.to_string()))?;
    let Value::Object(mut object) = value else {
        return Err(ManifestError::Malformed("not a JSON object".to_owned()));
    };
    let signature = match object.remove(SIGNATURE_FIELD) {
        Some(Value::String(signature)) => signature,
        Some(_) => return Err(ManifestError::MalformedSignature),
        None => return Err(ManifestError::MissingSignature),
    };
    let signature = STANDARD
        .decode(signature.trim())
        .ok()
        .and_then(|bytes| <[u8; 64]>::try_from(bytes.as_slice()).ok())
        .map(|bytes| Signature::from_bytes(&bytes))
        .ok_or(ManifestError::MalformedSignature)?;
    if trusted_keys.is_empty() {
        return Err(ManifestError::NoTrustedKeys);
    }

    let value = Value::Object(object);
    let message = canonical_json(&value);
    if !trusted_keys
        .iter()
        .any(|key| key.verify_strict(&message, &signature).is_ok())
    {
        return Err(ManifestError::BadSignature);
    }

    let manifest: Manifest =
        serde_json::from_value(value).map_err(|err| ManifestError::Malformed(err.to_string()))?;
    if manifest.sha256.len() != 64
        || !manifest
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ManifestError::InvalidSha256);
    }
    if !matches!(manifest.url.scheme(), "https" | "http") {
        return Err(ManifestError::InvalidUrl(manifest.url.scheme().to_owned()));
    }
    Ok(manifest)
}

/// Checks a verified manifest against what this installation expects.
pub fn check_manifest(
    manifest: Manifest,
    channel: ReleaseChannel,
    platform: &Platform,
    current: &Version,
) -> Result<UpdateDecision, ManifestError> {
    if manifest.channel != channel {
        return Err(ManifestError::WrongChannel {
            expected: channel,
            found: manifest.channel,
        });
    }
    if manifest.os != platform.os || manifest.arch != platform.arch {
        return Err(ManifestError::WrongPlatform {
            expected: platform.clone(),
            found: Platform::new(manifest.os, manifest.arch),
        });
    }
    match compare_versions(&manifest.version, current) {
        Ordering::Greater => Ok(UpdateDecision::Newer(Box::new(manifest))),
        Ordering::Equal => Ok(UpdateDecision::UpToDate),
        Ordering::Less => Err(ManifestError::Downgrade {
            current: current.clone(),
            offered: manifest.version,
        }),
    }
}

/// Orders versions by semver precedence, including prereleases
/// (`0.3.0-alpha.2 < 0.3.0-alpha.10 < 0.3.0-beta.1 < 0.3.0`). Build metadata is
/// ignored.
pub fn compare_versions(a: &Version, b: &Version) -> Ordering {
    a.cmp_precedence(b)
}

/// Serializes JSON canonically: object keys sorted by their UTF-8 bytes at every
/// level, no whitespace, strings and numbers as `serde_json` writes them.
///
/// Sorting is done here rather than relying on `serde_json`'s map order, which changes
/// if any crate in the build enables its `preserve_order` feature.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(object) => write_object(object, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        // Scalars: serde_json's compact output is already deterministic.
        scalar => out.extend_from_slice(scalar.to_string().as_bytes()),
    }
}

fn write_object(object: &Map<String, Value>, out: &mut Vec<u8>) {
    let mut entries: Vec<_> = object.iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
    out.push(b'{');
    for (i, (key, value)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(Value::String(key.clone()).to_string().as_bytes());
        out.push(b':');
        write_canonical(value, out);
    }
    out.push(b'}');
}

#[cfg(test)]
pub(crate) mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    pub(crate) fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub(crate) fn sample_manifest() -> Manifest {
        Manifest {
            version: Version::parse("0.4.0").unwrap(),
            commit: "9f2c1e7".to_owned(),
            channel: ReleaseChannel::Stable,
            os: "macos".to_owned(),
            arch: "aarch64".to_owned(),
            published: datetime!(2026-11-02 09:00:00 UTC),
            url: Url::parse(
                "https://github.com/pmig/mazemail/releases/download/0.4.0/Maze-0.4.0-macos-aarch64.app.tar.zst",
            )
            .unwrap(),
            size: 41_231_872,
            sha256: "ab".repeat(32),
            min_schema_from: 3,
            notes_url: Url::parse("https://github.com/pmig/mazemail/releases/tag/0.4.0")
                .unwrap(),
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn derives_the_dmg_of_a_macos_artifact() {
        let mut manifest = sample_manifest();
        assert_eq!(
            manifest.dmg_url().unwrap().as_str(),
            "https://github.com/pmig/mazemail/releases/download/0.4.0/Maze-0.4.0-macos-aarch64.dmg"
        );
        manifest.url = Url::parse(
            "https://github.com/pmig/mazemail/releases/download/0.4.0/Maze-0.4.0-linux-x86_64.tar.zst",
        )
        .unwrap();
        assert_eq!(manifest.dmg_url(), None);
    }

    #[test]
    fn prerelease_ordering() {
        let ordered = ["0.3.0-alpha.2", "0.3.0-alpha.10", "0.3.0-beta.1", "0.3.0"];
        for pair in ordered.windows(2) {
            assert_eq!(
                compare_versions(&v(pair[0]), &v(pair[1])),
                Ordering::Less,
                "{} < {}",
                pair[0],
                pair[1]
            );
            assert_eq!(
                compare_versions(&v(pair[1]), &v(pair[0])),
                Ordering::Greater
            );
        }
        // Numeric, not lexical, prerelease identifiers.
        assert_eq!(
            compare_versions(&v("0.3.0-alpha.10"), &v("0.3.0-alpha.9")),
            Ordering::Greater
        );
        // Build metadata doesn't make a version newer.
        assert_eq!(
            compare_versions(&v("0.3.0+abc"), &v("0.3.0+def")),
            Ordering::Equal
        );
    }

    #[test]
    fn canonical_json_sorts_and_compacts() {
        let value = json!({ "b": [1, { "z": null, "a": "x\"y" }], "a": 1.5, "é": true, "Z": "ü" });
        assert_eq!(
            String::from_utf8(canonical_json(&value)).unwrap(),
            r#"{"Z":"ü","a":1.5,"b":[1,{"a":"x\"y","z":null}],"é":true}"#
        );
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let key = test_key(1);
        let manifest = sample_manifest();
        let signed = manifest.sign(&key).unwrap();
        // Release scripts read the version with `sed` from its own line.
        assert!(
            signed.starts_with("{\n  \"version\": \"0.4.0\",\n"),
            "{signed}"
        );
        let verified = verify_manifest(signed.as_bytes(), &[key.verifying_key()]).unwrap();
        assert_eq!(verified, manifest);
    }

    #[test]
    fn verification_ignores_formatting_and_key_order() {
        let key = test_key(1);
        let signed: Value = serde_json::from_str(&sample_manifest().sign(&key).unwrap()).unwrap();
        // Re-serialize compactly with keys in another order.
        let mut entries: Vec<_> = signed.as_object().unwrap().iter().collect();
        entries.reverse();
        let reordered = format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{}: {}", json!(k), v))
                .collect::<Vec<_>>()
                .join(",\n ")
        );
        verify_manifest(reordered.as_bytes(), &[key.verifying_key()]).unwrap();
    }

    #[test]
    fn key_rotation_accepts_either_key() {
        let old = test_key(1);
        let new = test_key(2);
        let trusted = [old.verifying_key(), new.verifying_key()];
        for key in [&old, &new] {
            let signed = sample_manifest().sign(key).unwrap();
            verify_manifest(signed.as_bytes(), &trusted).unwrap();
        }
    }

    #[test]
    fn rejects_bad_signatures() {
        let key = test_key(1);
        let other = test_key(2);
        let signed = sample_manifest().sign(&key).unwrap();

        assert_eq!(
            verify_manifest(signed.as_bytes(), &[other.verifying_key()]),
            Err(ManifestError::BadSignature)
        );
        assert_eq!(
            verify_manifest(signed.as_bytes(), &[]),
            Err(ManifestError::NoTrustedKeys)
        );

        // Any change to a signed field breaks the signature.
        for (field, value) in [
            ("version", json!("9.9.9")),
            ("sha256", json!("cd".repeat(32))),
            ("url", json!("https://evil.example/Maze.app.tar.zst")),
            ("size", json!(1)),
            ("channel", json!("preview")),
            ("extra", json!("added")),
        ] {
            let mut value_json: Value = serde_json::from_str(&signed).unwrap();
            value_json[field] = value;
            let tampered = serde_json::to_vec(&value_json).unwrap();
            assert_eq!(
                verify_manifest(&tampered, &[key.verifying_key()]),
                Err(ManifestError::BadSignature),
                "{field}"
            );
        }

        let mut unsigned: Value = serde_json::from_str(&signed).unwrap();
        unsigned.as_object_mut().unwrap().remove(SIGNATURE_FIELD);
        assert_eq!(
            verify_manifest(
                &serde_json::to_vec(&unsigned).unwrap(),
                &[key.verifying_key()]
            ),
            Err(ManifestError::MissingSignature)
        );
        unsigned[SIGNATURE_FIELD] = json!("AAAA");
        assert_eq!(
            verify_manifest(
                &serde_json::to_vec(&unsigned).unwrap(),
                &[key.verifying_key()]
            ),
            Err(ManifestError::MalformedSignature)
        );
        assert!(matches!(
            verify_manifest(b"not json", &[key.verifying_key()]),
            Err(ManifestError::Malformed(_))
        ));
        assert!(matches!(
            verify_manifest(b"[1]", &[key.verifying_key()]),
            Err(ManifestError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_invalid_signed_fields() {
        let key = test_key(1);
        let mut manifest = sample_manifest();
        manifest.sha256 = "AB".repeat(32);
        let signed = manifest.sign(&key).unwrap();
        assert_eq!(
            verify_manifest(signed.as_bytes(), &[key.verifying_key()]),
            Err(ManifestError::InvalidSha256)
        );

        let mut manifest = sample_manifest();
        manifest.url = Url::parse("file:///etc/passwd").unwrap();
        let signed = manifest.sign(&key).unwrap();
        assert_eq!(
            verify_manifest(signed.as_bytes(), &[key.verifying_key()]),
            Err(ManifestError::InvalidUrl("file".to_owned()))
        );
    }

    #[test]
    fn checks_channel_platform_and_version() {
        let platform = Platform::new("macos", "aarch64");
        let manifest = sample_manifest();

        assert_eq!(
            check_manifest(
                manifest.clone(),
                ReleaseChannel::Stable,
                &platform,
                &v("0.3.9")
            ),
            Ok(UpdateDecision::Newer(Box::new(manifest.clone())))
        );
        assert_eq!(
            check_manifest(
                manifest.clone(),
                ReleaseChannel::Stable,
                &platform,
                &v("0.4.0")
            ),
            Ok(UpdateDecision::UpToDate)
        );
        // A stable release is newer than its own prereleases.
        assert!(matches!(
            check_manifest(
                manifest.clone(),
                ReleaseChannel::Stable,
                &platform,
                &v("0.4.0-rc.1")
            ),
            Ok(UpdateDecision::Newer(_))
        ));
        assert_eq!(
            check_manifest(
                manifest.clone(),
                ReleaseChannel::Stable,
                &platform,
                &v("0.5.0-alpha.1")
            ),
            Err(ManifestError::Downgrade {
                current: v("0.5.0-alpha.1"),
                offered: v("0.4.0"),
            })
        );
        assert_eq!(
            check_manifest(
                manifest.clone(),
                ReleaseChannel::Preview,
                &platform,
                &v("0.1.0")
            ),
            Err(ManifestError::WrongChannel {
                expected: ReleaseChannel::Preview,
                found: ReleaseChannel::Stable,
            })
        );
        assert!(matches!(
            check_manifest(
                manifest,
                ReleaseChannel::Stable,
                &Platform::new("linux", "x86_64"),
                &v("0.1.0")
            ),
            Err(ManifestError::WrongPlatform { .. })
        ));
    }

    #[test]
    fn manifest_urls() {
        let platform = Platform::new("macos", "aarch64");
        assert_eq!(
            manifest_url(DEFAULT_MANIFEST_BASE_URL, ReleaseChannel::Stable, &platform),
            "https://releases.mazemail.dev/stable/macos-aarch64.json"
        );
        assert_eq!(
            manifest_url("http://127.0.0.1:9/", ReleaseChannel::Preview, &platform),
            "http://127.0.0.1:9/preview/macos-aarch64.json"
        );
    }

    #[test]
    fn min_schema_from_defaults_to_zero() {
        let key = test_key(1);
        let mut value = serde_json::to_value(sample_manifest()).unwrap();
        value.as_object_mut().unwrap().remove("min_schema_from");
        let message = canonical_json(&value);
        value[SIGNATURE_FIELD] = json!(STANDARD.encode(key.sign(&message).to_bytes()));
        let manifest =
            verify_manifest(&serde_json::to_vec(&value).unwrap(), &[key.verifying_key()]).unwrap();
        assert_eq!(manifest.min_schema_from, 0);
    }
}
