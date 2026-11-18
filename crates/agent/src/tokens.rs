//! Session tokens for agents Maze starts (PLAN §4.11, §4.12; CONCEPT §10.4).
//!
//! Each session gets its own `mazemail_s_…` token in `MAZEMAIL_SESSION`, created by
//! Maze (`CreatedBy::Session`), revoked when the session ends. It covers every account
//! (accounts added during the session too), with the profile of `agent.session_profile`.

use std::time::Duration;

use local_api::{
    CreatedBy, Grant, Profile, SessionToken, TokenId, TokenRecord, TokenScope, TokenStore, mint,
};
use settings::SessionProfile;
use time::OffsetDateTime;

/// How long a session token lives at most (sessions revoke theirs when they end).
pub const SESSION_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 3600);

/// The local API profile for a settings profile.
pub fn profile(profile: SessionProfile) -> Profile {
    match profile {
        SessionProfile::Read => Profile::Read,
        SessionProfile::Triage => Profile::Triage,
        SessionProfile::Write => Profile::Write,
    }
}

/// Creating a token failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionTokenError {
    /// Minting failed.
    #[error(transparent)]
    Mint(#[from] local_api::TokenError),
    /// Storing failed.
    #[error(transparent)]
    Store(#[from] local_api::TokenStoreError),
}

/// A minted, stored session token.
#[derive(Debug, Clone)]
pub struct MintedToken {
    /// The secret for `MAZEMAIL_SESSION`.
    pub token: SessionToken,
    /// The stored record.
    pub record: TokenRecord,
}

/// Mints and stores a token for agent session `session`, for every account.
pub async fn mint_session_token(
    store: &dyn TokenStore,
    profile: Profile,
    label: &str,
    session: &str,
) -> Result<MintedToken, SessionTokenError> {
    let grant = Grant {
        profile,
        scope: TokenScope::all(),
        lifetime: Some(SESSION_TOKEN_LIFETIME),
        allow_send: false,
    };
    // Whole seconds: the store keeps timestamps in seconds.
    let now = OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp())
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let (token, record) = mint(
        &grant,
        Some(label.to_owned()),
        CreatedBy::Session {
            session: session.to_owned(),
        },
        now,
    )?;
    store.insert(record.clone()).await?;
    Ok(MintedToken { token, record })
}

/// Revokes a session's token.
pub async fn revoke(store: &dyn TokenStore, id: &TokenId) {
    if let Err(error) = store.revoke(id, OffsetDateTime::now_utc()).await {
        tracing::warn!(%error, "could not revoke an agent session token");
    }
}

/// Revokes session tokens left over from a previous run (Maze quit or crashed while a
/// session ran). Call at startup, before any session starts. Returns how many.
pub async fn revoke_stale_session_tokens(store: &dyn TokenStore) -> usize {
    let records = match store.list().await {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "could not list tokens");
            return 0;
        }
    };
    let now = OffsetDateTime::now_utc();
    let mut revoked = 0;
    for record in records {
        if matches!(record.created_by, CreatedBy::Session { .. })
            && record.revoked_at.is_none()
            && store.revoke(&record.id, now).await.unwrap_or(false)
        {
            revoked += 1;
        }
    }
    if revoked > 0 {
        tracing::info!(revoked, "revoked agent session tokens of an earlier run");
    }
    revoked
}

#[cfg(test)]
mod tests {
    use local_api::{MemoryTokenStore, verify};

    use super::*;

    #[tokio::test]
    async fn a_session_token_covers_every_account_until_revoked() {
        let store = MemoryTokenStore::new();
        let minted = mint_session_token(&store, Profile::Triage, "Claude Code", "s_1")
            .await
            .unwrap();
        assert_eq!(minted.record.scope, TokenScope::all());
        assert_eq!(
            minted.record.created_by,
            CreatedBy::Session {
                session: "s_1".to_owned()
            }
        );
        let record = verify(&store, &minted.token, OffsetDateTime::now_utc())
            .await
            .unwrap();
        revoke(&store, &record.id).await;
        assert!(
            verify(&store, &minted.token, OffsetDateTime::now_utc())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn leftover_session_tokens_are_revoked_at_startup() {
        let store = MemoryTokenStore::new();
        let minted = mint_session_token(&store, Profile::Read, "x", "s_old")
            .await
            .unwrap();
        // A user token stays.
        let (_, user) = mint(
            &Grant {
                profile: Profile::Read,
                scope: TokenScope::all(),
                lifetime: None,
                allow_send: false,
            },
            None,
            CreatedBy::User,
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        store.insert(user.clone()).await.unwrap();
        assert_eq!(revoke_stale_session_tokens(&store).await, 1);
        assert_eq!(revoke_stale_session_tokens(&store).await, 0);
        let records = store.list().await.unwrap();
        let find = |id: &TokenId| records.iter().find(|r| &r.id == id).unwrap().clone();
        assert!(find(&minted.record.id).revoked_at.is_some());
        assert!(find(&user.id).revoked_at.is_none());
    }
}
