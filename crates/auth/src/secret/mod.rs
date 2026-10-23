//! Account secrets: IMAP passwords, OAuth refresh tokens and SMTP passwords (PLAN §4.6,
//! §4.17, ADR-017).
//!
//! Every secret is addressed by a [`SecretKey`], `mazemail/<account-uuid>/<kind>`, and
//! kept by a [`SecretStore`]. The app keeps them in its encrypted store
//! (`mazemail_core::StoreSecretStore`, a table of the SQLCipher database), so the
//! storage master key is Maze's only keychain item and a launch asks the keychain
//! once, not once per account and secret.
//!
//! Before ADR-017 every secret was its own OS keychain item: on macOS a generic
//! password with service [`SERVICE`] (`mazemail`) and account `<account-uuid>/<kind>`
//! (`KeychainSecretStore`), on Linux a Secret Service item with the same attributes
//! (`SecretServiceStore`). Those stores remain only so the core can move existing
//! items into the encrypted store and delete them ([`legacy_system_secret_store`]).
//! [`MemorySecretStore`] serves tests, [`FileSecretStore`] scripted end-to-end runs.

use std::fmt;
use std::sync::Arc;

use mazemail_types::Sensitive;
use uuid::Uuid;

mod file;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod memory;

pub use file::FileSecretStore;
#[cfg(target_os = "linux")]
pub use linux::SecretServiceStore;
#[cfg(target_os = "macos")]
pub use macos::KeychainSecretStore;
pub use memory::MemorySecretStore;

/// The keychain service name of account secrets stored by Maze before ADR-017.
pub const SERVICE: &str = "mazemail";

/// Errors from a [`SecretStore`]. Never contain the secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SecretStoreError {
    /// The backing store (the encrypted database, the keychain, a file) reported an
    /// error.
    #[error("secret store error{}: {message}", .code.map(|c| format!(" {c}")).unwrap_or_default())]
    Platform {
        /// The platform status code (macOS `OSStatus`), if any.
        code: Option<i32>,
        /// The platform's message.
        message: String,
    },
    /// The stored item isn't valid UTF-8 (or JSON, for the file store), so it wasn't
    /// written by Maze.
    #[error("the stored secret is not valid UTF-8")]
    InvalidData,
    /// The blocking secret store task panicked or was cancelled.
    #[error("the secret store task failed: {0}")]
    Task(String),
}

/// What kind of secret an item holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretKind {
    /// The account's IMAP password.
    Password,
    /// The OAuth refresh token.
    OAuthRefresh,
    /// A separate SMTP password, for servers that use different credentials.
    SmtpPassword,
}

impl SecretKind {
    /// Every kind.
    pub const ALL: [Self; 3] = [Self::Password, Self::OAuthRefresh, Self::SmtpPassword];

    /// The name used in the key: `password`, `oauth-refresh` or `smtp-password`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::OAuthRefresh => "oauth-refresh",
            Self::SmtpPassword => "smtp-password",
        }
    }

    /// The item label people see in Keychain Access or Seahorse.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Password => "Maze account password",
            Self::OAuthRefresh => "Maze sign-in token",
            Self::SmtpPassword => "Maze SMTP password",
        }
    }
}

impl fmt::Display for SecretKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The address of one secret: `mazemail/<account-uuid>/<kind>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretKey {
    /// The account's stable UUID.
    pub account: Uuid,
    /// Which secret.
    pub kind: SecretKind,
}

impl SecretKey {
    /// The key for `account`'s `kind` secret.
    pub const fn new(account: Uuid, kind: SecretKind) -> Self {
        Self { account, kind }
    }

    /// The keychain account name: `<account-uuid>/<kind>` (the service is
    /// [`SERVICE`]).
    pub fn item_account(&self) -> String {
        format!("{}/{}", self.account.hyphenated(), self.kind)
    }
}

impl fmt::Display for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SERVICE}/{}", self.item_account())
    }
}

/// Persistent storage for account secrets.
///
/// Implementations never log or print secrets. Calls may block (a database write, or
/// keychain IPC and a system prompt for the legacy stores), so async code goes through
/// [`AccountSecrets`], which runs them on the blocking thread pool.
pub trait SecretStore: fmt::Debug + Send + Sync {
    /// Reads a secret; `Ok(None)` if there is none.
    fn load(&self, key: &SecretKey) -> Result<Option<Sensitive<String>>, SecretStoreError>;

    /// Stores a secret, replacing any existing one.
    fn store(&self, key: &SecretKey, secret: &Sensitive<String>) -> Result<(), SecretStoreError>;

    /// Deletes a secret. Deleting one that doesn't exist succeeds.
    fn delete(&self, key: &SecretKey) -> Result<(), SecretStoreError>;
}

/// Where Maze kept account secrets before ADR-017: the login keychain on macOS, the
/// Secret Service on Linux; `None` elsewhere. Only for moving those items into the
/// encrypted store; nothing stores new secrets there.
pub fn legacy_system_secret_store() -> Option<Arc<dyn SecretStore>> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(KeychainSecretStore::new()))
    }
    #[cfg(target_os = "linux")]
    {
        Some(Arc::new(SecretServiceStore::new()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// One account's secrets, with async accessors that run the store's calls on Tokio's
/// blocking pool. Needs a Tokio runtime.
#[derive(Debug, Clone)]
pub struct AccountSecrets {
    store: Arc<dyn SecretStore>,
    account: Uuid,
}

impl AccountSecrets {
    /// The secrets of `account` in `store`.
    pub fn new(store: Arc<dyn SecretStore>, account: Uuid) -> Self {
        Self { store, account }
    }

    /// The account's UUID.
    pub fn account(&self) -> Uuid {
        self.account
    }

    /// Reads the `kind` secret.
    pub async fn load(
        &self,
        kind: SecretKind,
    ) -> Result<Option<Sensitive<String>>, SecretStoreError> {
        let key = SecretKey::new(self.account, kind);
        self.blocking(move |store| store.load(&key)).await
    }

    /// Stores the `kind` secret, replacing any existing one.
    pub async fn store(
        &self,
        kind: SecretKind,
        secret: Sensitive<String>,
    ) -> Result<(), SecretStoreError> {
        let key = SecretKey::new(self.account, kind);
        self.blocking(move |store| store.store(&key, &secret)).await
    }

    /// Deletes the `kind` secret; succeeds if there is none.
    pub async fn delete(&self, kind: SecretKind) -> Result<(), SecretStoreError> {
        let key = SecretKey::new(self.account, kind);
        self.blocking(move |store| store.delete(&key)).await
    }

    /// Deletes every secret of the account, e.g. when it is removed.
    pub async fn delete_all(&self) -> Result<(), SecretStoreError> {
        let account = self.account;
        self.blocking(move |store| {
            SecretKind::ALL
                .into_iter()
                .try_for_each(|kind| store.delete(&SecretKey::new(account, kind)))
        })
        .await
    }

    /// The IMAP password.
    pub async fn password(&self) -> Result<Option<Sensitive<String>>, SecretStoreError> {
        self.load(SecretKind::Password).await
    }

    /// Stores the IMAP password.
    pub async fn set_password(&self, password: Sensitive<String>) -> Result<(), SecretStoreError> {
        self.store(SecretKind::Password, password).await
    }

    /// Deletes the IMAP password.
    pub async fn delete_password(&self) -> Result<(), SecretStoreError> {
        self.delete(SecretKind::Password).await
    }

    /// The SMTP password, if it differs from the IMAP one.
    pub async fn smtp_password(&self) -> Result<Option<Sensitive<String>>, SecretStoreError> {
        self.load(SecretKind::SmtpPassword).await
    }

    /// Stores the SMTP password.
    pub async fn set_smtp_password(
        &self,
        password: Sensitive<String>,
    ) -> Result<(), SecretStoreError> {
        self.store(SecretKind::SmtpPassword, password).await
    }

    /// Deletes the SMTP password.
    pub async fn delete_smtp_password(&self) -> Result<(), SecretStoreError> {
        self.delete(SecretKind::SmtpPassword).await
    }

    async fn blocking<T, F>(&self, f: F) -> Result<T, SecretStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&dyn SecretStore) -> Result<T, SecretStoreError> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || f(store.as_ref()))
            .await
            .map_err(|err| SecretStoreError::Task(err.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const ACCOUNT: Uuid = Uuid::from_u128(0x67e5_5044_10b1_426f_9247_bb68_0e5f_e0c8);

    #[test]
    fn key_format_matches_the_plan() {
        let key = SecretKey::new(ACCOUNT, SecretKind::OAuthRefresh);
        assert_eq!(
            key.to_string(),
            "mazemail/67e55044-10b1-426f-9247-bb680e5fe0c8/oauth-refresh"
        );
        assert_eq!(
            SecretKey::new(ACCOUNT, SecretKind::Password).item_account(),
            "67e55044-10b1-426f-9247-bb680e5fe0c8/password"
        );
        assert_eq!(SecretKind::SmtpPassword.as_str(), "smtp-password");
    }

    #[tokio::test]
    async fn account_secrets_round_trip() {
        let store = Arc::new(MemorySecretStore::new());
        let secrets = AccountSecrets::new(store.clone(), ACCOUNT);
        assert_eq!(secrets.password().await.unwrap(), None);

        secrets
            .set_password(Sensitive::new("hunter2".to_owned()))
            .await
            .unwrap();
        secrets
            .set_smtp_password(Sensitive::new("smtp".to_owned()))
            .await
            .unwrap();
        assert_eq!(
            secrets.password().await.unwrap().unwrap().expose(),
            "hunter2"
        );
        assert_eq!(
            secrets.smtp_password().await.unwrap().unwrap().expose(),
            "smtp"
        );

        // Another account doesn't see them.
        let other = AccountSecrets::new(store.clone(), Uuid::from_u128(1));
        assert_eq!(other.password().await.unwrap(), None);

        secrets.delete_password().await.unwrap();
        secrets.delete_password().await.unwrap();
        assert_eq!(secrets.password().await.unwrap(), None);
        assert_eq!(store.len(), 1);

        secrets
            .store(SecretKind::OAuthRefresh, Sensitive::new("rt".to_owned()))
            .await
            .unwrap();
        secrets.delete_all().await.unwrap();
        assert!(store.is_empty());
    }
}
