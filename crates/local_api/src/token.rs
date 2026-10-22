//! Session tokens and profiles (PLAN §4.11, CONCEPT §10.2).
//!
//! - A token is 256 random bits, shown once as `mazemail_s_<base32>` and handed to the
//!   CLI through `MAZEMAIL_SESSION`.
//! - Maze stores only a BLAKE3 hash of it (derive-key mode with a fixed context), next
//!   to the profile, scope, expiry, label and who created it ([`TokenRecord`]), through
//!   the [`TokenStore`] trait that `mazemail_core` implements on the `api_token` table.
//! - [`verify`] turns a presented token into its record, or a [`TokenRejection`].

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use mazemail_types::{AccountId, FolderId};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use zeroize::Zeroizing;

use crate::pubid;

/// The prefix of every session token's string form.
pub const TOKEN_PREFIX: &str = "mazemail_s_";

/// The environment variable that carries the session token.
pub const SESSION_ENV: &str = "MAZEMAIL_SESSION";

/// Secret bytes per token (256 bits).
const SECRET_LEN: usize = 32;

/// BLAKE3 derive-key context for token hashes. Changing it invalidates every token.
const HASH_CONTEXT: &str = "mazemail 2026-10 local api session token v1";

/// A session token in its string form (`mazemail_s_…`).
///
/// The string is zeroized on drop and never shown by `Debug`. It may be malformed
/// (it's whatever the client sent); [`SessionToken::hash`] validates it.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken(Zeroizing<String>);

impl SessionToken {
    /// Wraps a token string without validating it.
    pub fn from_string(token: String) -> Self {
        Self(Zeroizing::new(token))
    }

    /// Reads the token from [`SESSION_ENV`]. Empty values count as unset.
    pub fn from_env() -> Option<Self> {
        std::env::var(SESSION_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(Self::from_string)
    }

    /// The token string, for `export MAZEMAIL_SESSION=…` and the request frame.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Generates a fresh random token.
    pub fn generate() -> Result<Self, TokenError> {
        let mut secret = Zeroizing::new([0u8; SECRET_LEN]);
        getrandom::fill(secret.as_mut_slice()).map_err(|_| TokenError::Random)?;
        let mut text = String::with_capacity(TOKEN_PREFIX.len() + 52);
        text.push_str(TOKEN_PREFIX);
        base32::encode_into(secret.as_slice(), &mut text);
        Ok(Self::from_string(text))
    }

    /// Validates the format and returns the hash that the store keeps.
    pub fn hash(&self) -> Result<TokenHash, TokenRejection> {
        let body = self
            .0
            .strip_prefix(TOKEN_PREFIX)
            .ok_or(TokenRejection::Malformed)?;
        let secret = Zeroizing::new(base32::decode(body).ok_or(TokenRejection::Malformed)?);
        if secret.len() != SECRET_LEN {
            return Err(TokenRejection::Malformed);
        }
        let mut hasher = blake3::Hasher::new_derive_key(HASH_CONTEXT);
        hasher.update(&secret);
        Ok(TokenHash(*hasher.finalize().as_bytes()))
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

impl Serialize for SessionToken {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from_string)
    }
}

/// The stored BLAKE3 hash of a token. Comparison is constant-time.
#[derive(Clone, Copy, Eq)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Lowercase hex, for storage.
    pub fn to_hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    /// Parses lowercase or uppercase hex.
    pub fn from_hex(hex: &str) -> Option<Self> {
        blake3::Hash::from_hex(hex)
            .ok()
            .map(|hash| Self(*hash.as_bytes()))
    }

    /// The raw hash bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl PartialEq for TokenHash {
    fn eq(&self, other: &Self) -> bool {
        // `blake3::Hash` compares in constant time.
        blake3::Hash::from_bytes(self.0) == blake3::Hash::from_bytes(other.0)
    }
}

impl std::hash::Hash for TokenHash {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenHash({}…)", &self.to_hex()[..8])
    }
}

impl Serialize for TokenHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for TokenHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(&text).ok_or_else(|| serde::de::Error::custom("invalid token hash"))
    }
}

/// The public, non-secret id of a token (`tk_…`), shown in lists and the audit log.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct TokenId(String);

impl TokenId {
    const PREFIX: &'static str = "tk_";

    /// A fresh random id.
    pub fn generate() -> Result<Self, TokenError> {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|_| TokenError::Random)?;
        let mut id = String::from(Self::PREFIX);
        base32::encode_into(&bytes, &mut id);
        Ok(Self(id))
    }

    /// The id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TokenId {
    type Err = TokenIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let body = s.strip_prefix(Self::PREFIX).ok_or(TokenIdError)?;
        let base32 = |b: u8| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b);
        if body.is_empty() || body.len() > 26 || !body.bytes().all(base32) {
            return Err(TokenIdError);
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for TokenId {
    type Error = TokenIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<TokenId> for String {
    fn from(id: TokenId) -> Self {
        id.0
    }
}

/// A token id that isn't `tk_<base32>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a token id like tk_abcdefghijklm")]
pub struct TokenIdError;

/// What a token may do (CONCEPT §10.3). Ordered by rights: `ReadOnly < Triage < Writer`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    JsonSchema,
    clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// Search and read, nothing else.
    ReadOnly,
    /// Read, write drafts; archive/move/tag/flag become proposals; rules ask first.
    Triage,
    /// Read, write drafts and change mail directly; rules ask first.
    Writer,
}

impl Profile {
    /// Every profile, from fewest to most rights.
    pub const ALL: [Self; 3] = [Self::ReadOnly, Self::Triage, Self::Writer];

    /// The wire and CLI name (`read-only`, `triage`, `writer`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Triage => "triage",
            Self::Writer => "writer",
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which accounts and folders a token covers. `None` means "all": all accounts whose AI
/// policy allows external agents (checked on every request, see
/// [`AccountPolicy`](crate::AccountPolicy)), all folders not marked "never share".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenScope {
    /// Account ids (`a_…`), or `null` for all.
    #[serde(default, with = "pubid::option_set")]
    #[schemars(with = "Option<Vec<String>>")]
    pub accounts: Option<BTreeSet<AccountId>>,
    /// Folder ids (`f_…`), or `null` for all.
    #[serde(default, with = "pubid::option_set")]
    #[schemars(with = "Option<Vec<String>>")]
    pub folders: Option<BTreeSet<FolderId>>,
}

impl TokenScope {
    /// Every account and folder (subject to the AI policy).
    pub fn all() -> Self {
        Self::default()
    }

    /// Whether everything `self` covers is also covered by `other`.
    pub fn is_within(&self, other: &Self) -> bool {
        fn subset<T: Ord>(inner: Option<&BTreeSet<T>>, outer: Option<&BTreeSet<T>>) -> bool {
            match (inner, outer) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(inner), Some(outer)) => inner.is_subset(outer),
            }
        }
        subset(self.accounts.as_ref(), other.accounts.as_ref())
            && subset(self.folders.as_ref(), other.folders.as_ref())
    }

    /// A short human description ("all accounts", "2 accounts, 3 folders").
    pub fn describe(&self) -> String {
        let accounts = match &self.accounts {
            None => "all accounts".to_owned(),
            Some(set) => plural(set.len(), "account"),
        };
        match &self.folders {
            None => accounts,
            Some(set) => format!("{accounts}, {}", plural(set.len(), "folder")),
        }
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The rights of a token: what the user approves, and what a new token gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// Profile.
    pub profile: Profile,
    /// Accounts and folders.
    pub scope: TokenScope,
    /// Lifetime from creation; `None` never expires (only for tokens made in Settings).
    pub lifetime: Option<Duration>,
    /// Whether `send` is enabled (it then always asks, CONCEPT §10.2).
    pub allow_send: bool,
}

impl Grant {
    /// Whether this grant has no more rights than `other`.
    pub fn is_within(&self, other: &Self) -> bool {
        let lifetime_ok = match (self.lifetime, other.lifetime) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(inner), Some(outer)) => inner <= outer,
        };
        self.profile <= other.profile
            && self.scope.is_within(&other.scope)
            && lifetime_ok
            && (!self.allow_send || other.allow_send)
    }
}

/// Who created a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CreatedBy {
    /// The user: approved a `mazemail login`, or created it in Settings → Agents.
    User,
    /// Maze, for an agent session it started (`MAZEMAIL_SESSION`); revoked when the
    /// session ends.
    Session {
        /// The agent session's id.
        session: String,
    },
}

/// A stored token: everything except the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Public id.
    pub id: TokenId,
    /// BLAKE3 hash of the secret.
    pub hash: TokenHash,
    /// Label shown in Settings and the audit log.
    pub label: Option<String>,
    /// Profile.
    pub profile: Profile,
    /// Accounts and folders.
    pub scope: TokenScope,
    /// Whether `send` is enabled (always asks).
    pub allow_send: bool,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Expiry; `None` never expires.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// Who created it.
    pub created_by: CreatedBy,
    /// When it was revoked, if it was.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
}

impl TokenRecord {
    /// Checks expiry and revocation at `now`.
    pub fn check(&self, now: OffsetDateTime) -> Result<(), TokenRejection> {
        if self.revoked_at.is_some() {
            return Err(TokenRejection::Revoked);
        }
        match self.expires_at {
            Some(expires_at) if now >= expires_at => Err(TokenRejection::Expired),
            _ => Ok(()),
        }
    }

    /// The token's rights as a grant (lifetime counted from creation).
    pub fn grant(&self) -> Grant {
        Grant {
            profile: self.profile,
            scope: self.scope.clone(),
            lifetime: self
                .expires_at
                .map(|at| (at - self.created_at).unsigned_abs()),
            allow_send: self.allow_send,
        }
    }

    /// The public view of the record.
    pub fn info(&self) -> TokenInfo {
        TokenInfo {
            id: self.id.clone(),
            label: self.label.clone(),
            profile: self.profile,
            scope: self.scope.clone(),
            allow_send: self.allow_send,
            created_at: self.created_at,
            expires_at: self.expires_at,
            created_by: self.created_by.clone(),
            revoked: self.revoked_at.is_some(),
        }
    }
}

/// A token as `token list` and `status` show it (no hash, no secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenInfo {
    /// Public id (`tk_…`).
    pub id: TokenId,
    /// Label.
    pub label: Option<String>,
    /// Profile.
    pub profile: Profile,
    /// Accounts and folders (`null` = all).
    pub scope: TokenScope,
    /// Whether `send` is enabled (always asks).
    pub allow_send: bool,
    /// Creation time (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    #[schemars(with = "String")]
    pub created_at: OffsetDateTime,
    /// Expiry (RFC 3339), `null` = never.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schemars(with = "Option<String>")]
    pub expires_at: Option<OffsetDateTime>,
    /// Who created it.
    pub created_by: CreatedBy,
    /// Whether it was revoked.
    pub revoked: bool,
}

/// Creates a token for `grant`. The caller stores the record and shows the secret
/// once; it can't be recovered afterwards.
pub fn mint(
    grant: &Grant,
    label: Option<String>,
    created_by: CreatedBy,
    now: OffsetDateTime,
) -> Result<(SessionToken, TokenRecord), TokenError> {
    let token = SessionToken::generate()?;
    let hash = token.hash().map_err(|_| TokenError::Random)?;
    let expires_at = match grant.lifetime {
        None => None,
        Some(lifetime) => Some(
            time::Duration::try_from(lifetime)
                .ok()
                .and_then(|lifetime| now.checked_add(lifetime))
                .ok_or(TokenError::LifetimeTooLong)?,
        ),
    };
    let record = TokenRecord {
        id: TokenId::generate()?,
        hash,
        label,
        profile: grant.profile,
        scope: grant.scope.clone(),
        allow_send: grant.allow_send,
        created_at: now,
        expires_at,
        created_by,
        revoked_at: None,
    };
    Ok((token, record))
}

/// Looks up and checks a presented token.
pub async fn verify(
    store: &dyn TokenStore,
    token: &SessionToken,
    now: OffsetDateTime,
) -> Result<TokenRecord, TokenRejection> {
    let hash = token.hash()?;
    let record = store
        .find_by_hash(&hash)
        .await
        .map_err(TokenRejection::Store)?
        .ok_or(TokenRejection::Unknown)?;
    record.check(now)?;
    Ok(record)
}

/// Why a presented token isn't accepted.
#[derive(Debug, thiserror::Error)]
pub enum TokenRejection {
    /// Not `mazemail_s_` followed by 256 bits of base32.
    #[error("the session token is malformed")]
    Malformed,
    /// No such token (never existed, or deleted).
    #[error("the session token is unknown")]
    Unknown,
    /// Past its expiry.
    #[error("the session token has expired")]
    Expired,
    /// Revoked in Settings or with `mazemail token revoke`.
    #[error("the session token was revoked")]
    Revoked,
    /// The store failed.
    #[error(transparent)]
    Store(TokenStoreError),
}

/// Errors creating tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The OS random number generator failed.
    #[error("the system random number generator failed")]
    Random,
    /// The expiry doesn't fit into a timestamp.
    #[error("token lifetime is too long")]
    LifetimeTooLong,
}

/// An error from the token storage backend.
#[derive(Debug, thiserror::Error)]
#[error("token store: {0}")]
pub struct TokenStoreError(pub Box<dyn std::error::Error + Send + Sync>);

impl TokenStoreError {
    /// Wraps a backend error.
    pub fn new(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(error.into())
    }
}

/// Persistence for tokens; `mazemail_core` implements it on the encrypted `api_token`
/// table.
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Stores a new token.
    async fn insert(&self, record: TokenRecord) -> Result<(), TokenStoreError>;
    /// Finds the token with `hash`, including expired and revoked ones.
    async fn find_by_hash(&self, hash: &TokenHash) -> Result<Option<TokenRecord>, TokenStoreError>;
    /// All tokens, newest first.
    async fn list(&self) -> Result<Vec<TokenRecord>, TokenStoreError>;
    /// Marks a token revoked at `at`. Returns `false` if there is no such token or it
    /// was already revoked.
    async fn revoke(&self, id: &TokenId, at: OffsetDateTime) -> Result<bool, TokenStoreError>;
}

/// An in-memory [`TokenStore`] for tests and development.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    tokens: Mutex<HashMap<TokenId, TokenRecord>>,
}

impl MemoryTokenStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TokenId, TokenRecord>> {
        // A panic while holding the lock can't leave the map inconsistent.
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl TokenStore for MemoryTokenStore {
    async fn insert(&self, record: TokenRecord) -> Result<(), TokenStoreError> {
        let mut tokens = self.lock();
        if tokens.contains_key(&record.id) {
            return Err(TokenStoreError::new("duplicate token id"));
        }
        tokens.insert(record.id.clone(), record);
        Ok(())
    }

    async fn find_by_hash(&self, hash: &TokenHash) -> Result<Option<TokenRecord>, TokenStoreError> {
        Ok(self
            .lock()
            .values()
            .find(|record| record.hash == *hash)
            .cloned())
    }

    async fn list(&self) -> Result<Vec<TokenRecord>, TokenStoreError> {
        let mut records: Vec<_> = self.lock().values().cloned().collect();
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        Ok(records)
    }

    async fn revoke(&self, id: &TokenId, at: OffsetDateTime) -> Result<bool, TokenStoreError> {
        Ok(match self.lock().get_mut(id) {
            Some(record) if record.revoked_at.is_none() => {
                record.revoked_at = Some(at);
                true
            }
            _ => false,
        })
    }
}

/// A token lifetime as typed on the command line: `90s`, `30m`, `8h`, `7d`, `2w`, or
/// combinations like `1h30m`. Serialized in the same canonical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(with = "String")]
pub struct Ttl(Duration);

impl Ttl {
    /// Wraps a duration (whole seconds are kept).
    pub fn new(duration: Duration) -> Self {
        Self(Duration::from_secs(duration.as_secs()))
    }

    /// The duration.
    pub fn duration(self) -> Duration {
        self.0
    }
}

impl Default for Ttl {
    /// Eight hours, the CLI's default for `mazemail login`.
    fn default() -> Self {
        Self(Duration::from_secs(8 * 3600))
    }
}

const TTL_UNITS: [(char, u64); 5] = [
    ('w', 7 * 86_400),
    ('d', 86_400),
    ('h', 3600),
    ('m', 60),
    ('s', 1),
];

impl FromStr for Ttl {
    type Err = TtlError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(TtlError);
        }
        let mut total: u64 = 0;
        let mut number = String::new();
        for c in s.chars() {
            if c.is_ascii_digit() {
                number.push(c);
                continue;
            }
            let (_, unit) = TTL_UNITS
                .iter()
                .find(|(name, _)| *name == c)
                .ok_or(TtlError)?;
            let value: u64 = number.parse().map_err(|_| TtlError)?;
            number.clear();
            total = value
                .checked_mul(*unit)
                .and_then(|secs| total.checked_add(secs))
                .ok_or(TtlError)?;
        }
        if !number.is_empty() || total == 0 {
            return Err(TtlError);
        }
        Ok(Self(Duration::from_secs(total)))
    }
}

impl fmt::Display for Ttl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rest = self.0.as_secs();
        if rest == 0 {
            return f.write_str("0s");
        }
        for (name, unit) in TTL_UNITS {
            if rest >= unit {
                write!(f, "{}{name}", rest / unit)?;
                rest %= unit;
            }
        }
        Ok(())
    }
}

impl Serialize for Ttl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Ttl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A lifetime that isn't like `30m`, `8h` or `1h30m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a duration like 30m, 8h, 7d or 1h30m")]
pub struct TtlError;

/// Lowercase RFC 4648 base32 without padding.
mod base32 {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

    pub(super) fn encode_into(bytes: &[u8], out: &mut String) {
        let mut buffer: u32 = 0;
        let mut bits = 0;
        for &byte in bytes {
            buffer = (buffer << 8) | u32::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
            }
        }
        if bits > 0 {
            out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
        }
    }

    /// Decodes canonical input only: unused trailing bits must be zero.
    pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(text.len() * 5 / 8);
        let mut buffer: u32 = 0;
        let mut bits = 0;
        for byte in text.bytes() {
            let value = ALPHABET.iter().position(|&c| c == byte)?;
            buffer = (buffer << 5) | u32::try_from(value).ok()?;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
            }
            buffer &= (1 << bits) - 1;
        }
        // Leftover bits must be padding zeros, and a full extra character is invalid.
        (bits < 5 && buffer == 0).then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use time::macros::datetime;

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-10-22 12:00 UTC);

    fn grant(profile: Profile) -> Grant {
        Grant {
            profile,
            scope: TokenScope::all(),
            lifetime: Some(Duration::from_secs(3600)),
            allow_send: false,
        }
    }

    #[test]
    fn token_format() {
        let token = SessionToken::generate().unwrap();
        let text = token.expose();
        assert!(text.starts_with("mazemail_s_"));
        // 256 bits in base32 without padding: 52 characters.
        assert_eq!(text.len(), TOKEN_PREFIX.len() + 52);
        assert!(
            text[TOKEN_PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        );
        assert_ne!(SessionToken::generate().unwrap(), token);
        assert_eq!(format!("{token:?}"), "SessionToken(<redacted>)");
    }

    #[test]
    fn hash_is_stable_and_validates_format() {
        let token = SessionToken::generate().unwrap();
        let hash = token.hash().unwrap();
        assert_eq!(token.hash().unwrap(), hash);
        assert_eq!(TokenHash::from_hex(&hash.to_hex()), Some(hash));
        assert_ne!(SessionToken::generate().unwrap().hash().unwrap(), hash);

        let body = &token.expose()[TOKEN_PREFIX.len()..];
        for bad in [
            String::new(),
            body.to_owned(),
            format!("mazemail_x_{body}"),
            format!("{TOKEN_PREFIX}{}", &body[..51]),
            format!("{TOKEN_PREFIX}{body}a"),
            format!("{TOKEN_PREFIX}{}", body.to_uppercase()),
            format!("{TOKEN_PREFIX}{}1", &body[..51]),
        ] {
            let result = SessionToken::from_string(bad.clone()).hash();
            assert!(matches!(result, Err(TokenRejection::Malformed)), "{bad}");
        }
    }

    #[test]
    fn base32_round_trip_and_canonical() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let mut text = String::new();
            base32::encode_into(&bytes, &mut text);
            assert_eq!(base32::decode(&text), Some(bytes));
        }
        // One byte takes two characters; the second one's low two bits are padding and
        // must be zero, and a lone character carries no full byte.
        assert_eq!(base32::decode("aa"), Some(vec![0]));
        assert_eq!(base32::decode("ab"), None);
        assert_eq!(base32::decode("a"), None);
    }

    #[tokio::test]
    async fn verify_accepts_valid_and_rejects_others() {
        let store = MemoryTokenStore::new();
        let (token, record) = mint(&grant(Profile::Triage), None, CreatedBy::User, NOW).unwrap();
        assert_eq!(record.expires_at, Some(NOW + time::Duration::hours(1)));
        store.insert(record.clone()).await.unwrap();

        assert_eq!(verify(&store, &token, NOW).await.unwrap(), record);
        // Expiry is exclusive.
        let at_expiry = NOW + time::Duration::hours(1);
        assert!(matches!(
            verify(&store, &token, at_expiry).await,
            Err(TokenRejection::Expired)
        ));
        let unknown = SessionToken::generate().unwrap();
        assert!(matches!(
            verify(&store, &unknown, NOW).await,
            Err(TokenRejection::Unknown)
        ));
        assert!(store.revoke(&record.id, NOW).await.unwrap());
        assert!(!store.revoke(&record.id, NOW).await.unwrap());
        assert!(matches!(
            verify(&store, &token, NOW).await,
            Err(TokenRejection::Revoked)
        ));
    }

    #[test]
    fn records_never_contain_the_secret() {
        let (token, record) = mint(
            &grant(Profile::Writer),
            Some("ci".into()),
            CreatedBy::User,
            NOW,
        )
        .unwrap();
        let json = serde_json::to_string(&record).unwrap();
        let secret_body = &token.expose()[TOKEN_PREFIX.len()..];
        assert!(!json.contains(secret_body));
        assert!(!format!("{record:?}").contains(secret_body));
        let back: TokenRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
        assert_eq!(record.grant(), grant(Profile::Writer));
    }

    #[test]
    fn non_expiring_tokens() {
        let mut grant = grant(Profile::ReadOnly);
        grant.lifetime = None;
        let (_, record) = mint(
            &grant,
            None,
            CreatedBy::Session {
                session: "s1".into(),
            },
            NOW,
        )
        .unwrap();
        assert_eq!(record.expires_at, None);
        assert!(record.check(NOW + time::Duration::days(10_000)).is_ok());
    }

    #[test]
    fn scope_and_grant_ordering() {
        let a = |ids: &[i64]| Some(ids.iter().copied().map(AccountId).collect::<BTreeSet<_>>());
        let one = TokenScope {
            accounts: a(&[1]),
            folders: None,
        };
        let two = TokenScope {
            accounts: a(&[1, 2]),
            folders: None,
        };
        assert!(one.is_within(&two));
        assert!(!two.is_within(&one));
        assert!(two.is_within(&TokenScope::all()));
        assert!(!TokenScope::all().is_within(&two));
        assert_eq!(two.describe(), "2 accounts");
        assert_eq!(TokenScope::all().describe(), "all accounts");

        let base = grant(Profile::Triage);
        assert!(grant(Profile::ReadOnly).is_within(&base));
        assert!(!grant(Profile::Writer).is_within(&base));
        let mut longer = base.clone();
        longer.lifetime = Some(Duration::from_secs(7200));
        assert!(!longer.is_within(&base));
        let mut send = base.clone();
        send.allow_send = true;
        assert!(!send.is_within(&base));
        assert!(base.is_within(&send));
    }

    #[test]
    fn scope_serializes_public_ids() {
        let scope = TokenScope {
            accounts: Some([AccountId(1)].into()),
            folders: None,
        };
        let json = serde_json::to_value(&scope).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "accounts": ["a_b"], "folders": null })
        );
        assert_eq!(serde_json::from_value::<TokenScope>(json).unwrap(), scope);
        assert!(
            serde_json::from_value::<TokenScope>(serde_json::json!({ "accounts": ["f_b"] }))
                .is_err()
        );
    }

    #[test]
    fn token_ids() {
        let id = TokenId::generate().unwrap();
        assert!(id.as_str().starts_with("tk_"));
        assert_eq!(id.as_str().parse::<TokenId>().unwrap(), id);
        assert!("tk_".parse::<TokenId>().is_err());
        assert!("xx_abc".parse::<TokenId>().is_err());
        assert!("tk_ABC".parse::<TokenId>().is_err());
    }

    #[test]
    fn ttl_parse_and_display() {
        for (text, secs, canonical) in [
            ("90s", 90, "1m30s"),
            ("30m", 1800, "30m"),
            ("8h", 28_800, "8h"),
            ("1h30m", 5400, "1h30m"),
            ("7d", 604_800, "1w"),
            ("2w", 1_209_600, "2w"),
        ] {
            let ttl: Ttl = text.parse().unwrap();
            assert_eq!(ttl.duration().as_secs(), secs, "{text}");
            assert_eq!(ttl.to_string(), canonical);
            assert_eq!(
                serde_json::to_string(&ttl).unwrap(),
                format!("\"{canonical}\"")
            );
        }
        for bad in ["", "8", "h", "0h", "8x", "-1h", "99999999999999999999w"] {
            assert!(bad.parse::<Ttl>().is_err(), "{bad}");
        }
        assert_eq!(Ttl::default().to_string(), "8h");
    }
}
