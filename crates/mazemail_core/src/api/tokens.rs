//! Tokens: `local_api`'s [`TokenStore`] on the store's `api_token` table, and the
//! [`TokenAuthorizer`] that makes [`CoreHandle::apply`](crate::CoreHandle::apply) obey
//! a token's profile.
//!
//! # Storage format
//!
//! The `api_token` row keeps `local_api`'s keyed BLAKE3 hash in `hash` (opaque bytes to
//! the store), the profile in `profile`, the expiry, label and revocation time in their
//! columns, and everything else of a [`TokenRecord`] in `scope_json`:
//!
//! ```json
//! { "id": "tk_…", "accounts": ["a_b"], "folders": null, "allow_send": false,
//!   "created_by": { "kind": "user" } }
//! ```
//!
//! Rows written by other code without the extra fields still decode: the public id is
//! then derived from the row id, `allow_send` is off and the creator is the user.
//!
//! # The token cache
//!
//! [`CommandAuthorizer::authorize`] is synchronous and must not block, so it can't read
//! the database. Every token that passes through [`StoreTokenStore`] (created, verified
//! on a request, listed or revoked) is remembered in a [`TokenCache`] shared with the
//! [`TokenAuthorizer`]. The dispatcher verifies a token through the store on every
//! request before the handler runs, so the cache always holds the token an agent
//! command arrives with; a token the cache doesn't know is refused (fail closed).

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr as _;
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use local_api::{
    Access, CreatedBy, Permission, Profile, TokenHash, TokenId as ApiTokenId, TokenRecord,
    TokenScope, TokenStore, TokenStoreError, access,
};
use mazemail_types::PublicId;
use serde::{Deserialize, Serialize};
use store::{ApiToken, NewApiToken, Queries as _, TokenId};
use time::OffsetDateTime;

use crate::api_handler::{Authorization, CommandAuthorizer};
use crate::commands::MailCommand;
use crate::handle::CoreHandle;

/// What the authorizer needs to know about a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedGrant {
    profile: Profile,
    allow_send: bool,
    expires_at: Option<OffsetDateTime>,
    revoked: bool,
}

#[derive(Debug, Default)]
struct CacheInner {
    grants: HashMap<TokenId, CachedGrant>,
    by_public: HashMap<ApiTokenId, TokenId>,
}

/// Tokens seen by [`StoreTokenStore`], by store id and public id. See the
/// [module docs](self).
#[derive(Debug, Default)]
pub struct TokenCache {
    inner: RwLock<CacheInner>,
}

impl TokenCache {
    fn remember(&self, id: TokenId, record: &TokenRecord) {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        inner.grants.insert(
            id,
            CachedGrant {
                profile: record.profile,
                allow_send: record.allow_send,
                expires_at: record.expires_at,
                revoked: record.revoked_at.is_some(),
            },
        );
        inner.by_public.insert(record.id.clone(), id);
    }

    fn mark_revoked(&self, id: TokenId) {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(grant) = inner.grants.get_mut(&id) {
            grant.revoked = true;
        }
    }

    fn grant(&self, id: TokenId) -> Option<CachedGrant> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .grants
            .get(&id)
            .copied()
    }

    /// The store id of a token the cache has seen.
    pub fn store_id(&self, public: &ApiTokenId) -> Option<TokenId> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .by_public
            .get(public)
            .copied()
    }
}

/// Decides agent commands in [`CoreHandle::apply`](crate::CoreHandle::apply) from the
/// token's profile, with the same matrix the local API dispatcher uses
/// ([`local_api::access`]): read-only tokens can't change mail, triage tokens get
/// proposals, writer tokens apply directly. Commands no local API command maps to
/// (deleting messages, folder management) are refused for every token.
///
/// Pass it as [`CoreConfig::authorizer`](crate::CoreConfig::authorizer) and hand the
/// same value to [`services`](super::services), which shares its [`TokenCache`].
#[derive(Debug, Clone, Default)]
pub struct TokenAuthorizer {
    cache: Arc<TokenCache>,
}

impl TokenAuthorizer {
    /// An authorizer with an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cache shared with the token store.
    pub fn cache(&self) -> &Arc<TokenCache> {
        &self.cache
    }
}

impl CommandAuthorizer for TokenAuthorizer {
    fn authorize(&self, token: TokenId, command: &MailCommand) -> Authorization {
        let Some(grant) = self.cache.grant(token) else {
            return deny("the token is unknown");
        };
        if grant.revoked {
            return deny("the token was revoked");
        }
        if grant
            .expires_at
            .is_some_and(|expires| OffsetDateTime::now_utc() >= expires)
        {
            return deny("the token has expired");
        }
        let Some(permission) = command_permission(command) else {
            return deny(format!("agents can't run `{}`", command.name()));
        };
        match access(grant.profile, permission, grant.allow_send) {
            Access::Allow => Authorization::Allow,
            // Nothing here may wait for the user: an `ask` becomes a proposal too.
            Access::Propose | Access::Ask => Authorization::Propose {
                title: proposal_title(command),
                reason: None,
            },
            Access::Deny => deny(format!(
                "the {} profile can't run `{}`",
                grant.profile,
                command.name()
            )),
        }
    }
}

fn deny(reason: impl Into<String>) -> Authorization {
    Authorization::Deny {
        reason: reason.into(),
    }
}

/// The local API permission class of a core command, `None` for commands no agent may
/// run.
fn command_permission(command: &MailCommand) -> Option<Permission> {
    match command {
        MailCommand::SetFlags { .. }
        | MailCommand::SetTags { .. }
        | MailCommand::Move { .. }
        | MailCommand::Copy { .. }
        | MailCommand::Archive { .. }
        | MailCommand::MarkFolderRead { .. } => Some(Permission::Propose),
        MailCommand::Refresh { .. } => Some(Permission::Read),
        MailCommand::Delete { .. }
        | MailCommand::CreateFolder { .. }
        | MailCommand::RenameFolder { .. }
        | MailCommand::DeleteFolder { .. } => None,
    }
}

/// A short title for the review UI ("Archive 3 messages"). No message content.
pub(crate) fn proposal_title(command: &MailCommand) -> String {
    use mazemail_types::Flags;

    fn messages(n: usize) -> String {
        if n == 1 {
            "1 message".to_owned()
        } else {
            format!("{n} messages")
        }
    }
    match command {
        MailCommand::Archive { ids } => format!("Archive {}", messages(ids.len())),
        MailCommand::Move { ids, .. } => format!("Move {}", messages(ids.len())),
        MailCommand::Copy { ids, .. } => format!("Copy {}", messages(ids.len())),
        MailCommand::SetTags { ids, .. } => format!("Retag {}", messages(ids.len())),
        MailCommand::SetFlags { ids, add, remove } => {
            let verb = if add.contains(Flags::SEEN) {
                "Mark read:"
            } else if remove.contains(Flags::SEEN) {
                "Mark unread:"
            } else if add.contains(Flags::FLAGGED) {
                "Flag"
            } else if remove.contains(Flags::FLAGGED) {
                "Unflag"
            } else {
                "Change flags of"
            };
            format!("{verb} {}", messages(ids.len()))
        }
        MailCommand::MarkFolderRead { .. } => "Mark a folder read".to_owned(),
        other => format!("Run `{}`", other.name()),
    }
}

/// What `scope_json` holds besides the scope. See the [module docs](self).
#[derive(Debug, Serialize, Deserialize)]
struct StoredToken {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<ApiTokenId>,
    #[serde(flatten)]
    scope: TokenScope,
    #[serde(default)]
    allow_send: bool,
    #[serde(default = "created_by_user")]
    created_by: CreatedBy,
}

fn created_by_user() -> CreatedBy {
    CreatedBy::User
}

/// The `tk_…` id derived from a row id, for rows without a stored public id.
#[derive(Debug, Clone, Copy)]
struct DerivedTokenId(i64);

impl PublicId for DerivedTokenId {
    const PREFIX: &'static str = "tk";
    fn raw(self) -> i64 {
        self.0
    }
    fn from_raw(raw: i64) -> Self {
        Self(raw)
    }
}

/// Why a stored row can't be turned into a [`TokenRecord`].
#[derive(Debug, thiserror::Error)]
enum DecodeError {
    #[error("unknown token profile")]
    Profile,
    #[error("invalid token scope: {0}")]
    Scope(serde_json::Error),
    #[error("invalid token timestamp")]
    Time,
    #[error("invalid token id")]
    Id,
}

fn profile_from_text(text: &str) -> Option<Profile> {
    Profile::ALL.into_iter().find(|p| p.as_str() == text)
}

fn timestamp(seconds: i64) -> Result<OffsetDateTime, DecodeError> {
    OffsetDateTime::from_unix_timestamp(seconds).map_err(|_| DecodeError::Time)
}

fn hash_from_bytes(bytes: &[u8; 32]) -> Option<TokenHash> {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        // Writing into a String can't fail.
        let _ = write!(hex, "{byte:02x}");
    }
    TokenHash::from_hex(&hex)
}

fn decode(row: &ApiToken) -> Result<TokenRecord, DecodeError> {
    let stored: StoredToken = serde_json::from_str(&row.scope_json).map_err(DecodeError::Scope)?;
    let id = match stored.id {
        Some(id) => id,
        None => ApiTokenId::from_str(&DerivedTokenId(row.id.0).to_public())
            .map_err(|_| DecodeError::Id)?,
    };
    Ok(TokenRecord {
        id,
        hash: hash_from_bytes(&row.hash).ok_or(DecodeError::Id)?,
        label: row.label.clone(),
        profile: profile_from_text(&row.profile).ok_or(DecodeError::Profile)?,
        scope: stored.scope,
        allow_send: stored.allow_send,
        created_at: timestamp(row.created)?,
        expires_at: row.expires.map(timestamp).transpose()?,
        created_by: stored.created_by,
        revoked_at: row.revoked.map(timestamp).transpose()?,
    })
}

fn encode(record: &TokenRecord) -> Result<NewApiToken, TokenStoreError> {
    let stored = StoredToken {
        id: Some(record.id.clone()),
        scope: record.scope.clone(),
        allow_send: record.allow_send,
        created_by: record.created_by.clone(),
    };
    Ok(NewApiToken {
        hash: *record.hash.as_bytes(),
        profile: record.profile.as_str().to_owned(),
        scope_json: serde_json::to_string(&stored).map_err(TokenStoreError::new)?,
        expires: record.expires_at.map(OffsetDateTime::unix_timestamp),
        label: record.label.clone(),
    })
}

fn store_error(error: impl Into<crate::Error>) -> TokenStoreError {
    TokenStoreError::new(error.into())
}

/// [`TokenStore`] on the encrypted `api_token` table. See the [module docs](self).
#[derive(Clone)]
pub struct StoreTokenStore {
    core: CoreHandle,
    cache: Arc<TokenCache>,
}

impl fmt::Debug for StoreTokenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreTokenStore").finish_non_exhaustive()
    }
}

impl StoreTokenStore {
    /// A token store over `core`'s database that remembers tokens in `cache`.
    pub fn new(core: CoreHandle, cache: Arc<TokenCache>) -> Self {
        Self { core, cache }
    }

    /// Every decodable row with its store id. Undecodable rows are skipped (and
    /// logged): they can't authenticate anyone.
    fn rows(&self) -> Result<Vec<(TokenId, TokenRecord)>, TokenStoreError> {
        let rows = self
            .core
            .store()
            .reader()
            .and_then(|reader| reader.api_tokens())
            .map_err(store_error)?;
        Ok(rows
            .iter()
            .filter_map(|row| match decode(row) {
                Ok(record) => Some((row.id, record)),
                Err(error) => {
                    tracing::warn!(token = row.id.0, %error, "skipping an unreadable API token");
                    None
                }
            })
            .collect())
    }
}

#[async_trait]
impl TokenStore for StoreTokenStore {
    async fn insert(&self, record: TokenRecord) -> Result<(), TokenStoreError> {
        let row = encode(&record)?;
        let id = self
            .core
            .store()
            .write(move |w| w.create_api_token(&row))
            .await
            .map_err(store_error)?;
        self.cache.remember(id, &record);
        Ok(())
    }

    async fn find_by_hash(&self, hash: &TokenHash) -> Result<Option<TokenRecord>, TokenStoreError> {
        let row = self
            .core
            .store()
            .reader()
            .and_then(|reader| reader.api_token_by_hash(hash.as_bytes()))
            .map_err(store_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let record = decode(&row).map_err(TokenStoreError::new)?;
        self.cache.remember(row.id, &record);
        Ok(Some(record))
    }

    async fn list(&self) -> Result<Vec<TokenRecord>, TokenStoreError> {
        let mut rows = self.rows()?;
        for (id, record) in &rows {
            self.cache.remember(*id, record);
        }
        rows.sort_by(|(a_id, a), (b_id, b)| {
            b.created_at.cmp(&a.created_at).then(b_id.0.cmp(&a_id.0))
        });
        Ok(rows.into_iter().map(|(_, record)| record).collect())
    }

    async fn revoke(&self, id: &ApiTokenId, _at: OffsetDateTime) -> Result<bool, TokenStoreError> {
        let Some((store_id, record)) = self.rows()?.into_iter().find(|(_, r)| r.id == *id) else {
            return Ok(false);
        };
        if record.revoked_at.is_some() {
            self.cache.remember(store_id, &record);
            return Ok(false);
        }
        // The store records its own clock as the revocation time.
        self.core
            .store()
            .write(move |w| w.revoke_api_token(store_id))
            .await
            .map_err(store_error)?;
        self.cache.remember(store_id, &record);
        self.cache.mark_revoked(store_id);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use local_api::{Grant, mint};
    use mazemail_types::{AccountId, Flags, MessageId};

    use super::*;

    fn record(profile: Profile) -> TokenRecord {
        let grant = Grant {
            profile,
            scope: TokenScope {
                accounts: Some(BTreeSet::from([AccountId(3)])),
                folders: None,
            },
            lifetime: Some(Duration::from_secs(3600)),
            allow_send: false,
        };
        // Whole seconds, as the store keeps them.
        let now = OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp())
            .unwrap();
        mint(&grant, Some("label".into()), CreatedBy::User, now)
            .unwrap()
            .1
    }

    #[test]
    fn rows_round_trip() {
        let original = record(Profile::Triage);
        let new = encode(&original).unwrap();
        let row = ApiToken {
            id: TokenId(9),
            hash: new.hash,
            profile: new.profile,
            scope_json: new.scope_json,
            expires: new.expires,
            label: new.label,
            created: original.created_at.unix_timestamp(),
            last_used: None,
            revoked: None,
        };
        assert_eq!(decode(&row).unwrap(), original);

        // A row without the extra fields gets a derived id and safe defaults.
        let bare = ApiToken {
            scope_json: "{}".into(),
            ..row
        };
        let decoded = decode(&bare).unwrap();
        assert_eq!(decoded.id.as_str(), "tk_j");
        assert_eq!(decoded.scope, TokenScope::all());
        assert!(!decoded.allow_send);
        assert_eq!(decoded.created_by, CreatedBy::User);
    }

    #[test]
    fn authorizer_follows_the_profile_matrix_and_fails_closed() {
        let authorizer = TokenAuthorizer::new();
        let archive = MailCommand::Archive {
            ids: vec![MessageId(1), MessageId(2)],
        };
        // Unknown tokens are refused.
        assert!(matches!(
            authorizer.authorize(TokenId(1), &archive),
            Authorization::Deny { .. }
        ));

        for (id, profile) in [
            (1, Profile::ReadOnly),
            (2, Profile::Triage),
            (3, Profile::Writer),
        ] {
            authorizer.cache().remember(TokenId(id), &record(profile));
        }
        assert!(matches!(
            authorizer.authorize(TokenId(1), &archive),
            Authorization::Deny { .. }
        ));
        assert_eq!(
            authorizer.authorize(TokenId(2), &archive),
            Authorization::Propose {
                title: "Archive 2 messages".into(),
                reason: None
            }
        );
        assert_eq!(
            authorizer.authorize(TokenId(3), &archive),
            Authorization::Allow
        );
        // No token may delete or manage folders.
        let delete = MailCommand::Delete {
            ids: vec![MessageId(1)],
            permanent: false,
        };
        assert!(matches!(
            authorizer.authorize(TokenId(3), &delete),
            Authorization::Deny { .. }
        ));
        // Revocation takes effect at once.
        authorizer.cache().mark_revoked(TokenId(3));
        assert!(matches!(
            authorizer.authorize(TokenId(3), &archive),
            Authorization::Deny { .. }
        ));
        // Expired tokens are refused.
        let mut expired = record(Profile::Writer);
        expired.expires_at = Some(OffsetDateTime::UNIX_EPOCH);
        authorizer.cache().remember(TokenId(4), &expired);
        assert!(matches!(
            authorizer.authorize(TokenId(4), &archive),
            Authorization::Deny { .. }
        ));
    }

    #[test]
    fn titles_carry_no_content() {
        let read = MailCommand::SetFlags {
            ids: vec![MessageId(1)],
            add: Flags::SEEN,
            remove: Flags::empty(),
        };
        assert_eq!(proposal_title(&read), "Mark read: 1 message");
    }
}
