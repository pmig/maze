//! Account secrets in the encrypted store (ADR-017).
//!
//! IMAP passwords, OAuth refresh tokens and SMTP passwords are rows of the SQLCipher
//! database ([`StoreSecretStore`]), encrypted under the master key like the mail they
//! unlock. The master key is the only item Maze keeps in the keychain, so a launch
//! reads the keychain once.

use std::fmt;

use auth::{SecretKey, SecretStore, SecretStoreError};
use mazemail_types::Sensitive;
use store::{Queries as _, Store};
use zeroize::Zeroizing;

use crate::Result;

/// Account secrets in the encrypted store (table `account_secret`, see the
/// [module docs](self)), as `auth`'s [`SecretStore`].
///
/// The calls block (a write waits for its commit), so async code goes through
/// [`auth::AccountSecrets`], which runs them on the blocking pool; never call
/// [`SecretStore::store`] or [`SecretStore::delete`] on an async worker thread.
/// Storing a secret for an account the store doesn't know fails; removing an account
/// removes its secrets with it.
#[derive(Clone)]
pub struct StoreSecretStore {
    store: Store,
}

impl fmt::Debug for StoreSecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreSecretStore").finish_non_exhaustive()
    }
}

impl StoreSecretStore {
    /// Secrets in `store`.
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

fn store_error(err: &store::Error) -> SecretStoreError {
    // Store errors never carry bound values, so no secret.
    SecretStoreError::Platform {
        code: None,
        message: format!("the encrypted store: {err}"),
    }
}

impl SecretStore for StoreSecretStore {
    fn load(&self, key: &SecretKey) -> Result<Option<Sensitive<String>>, SecretStoreError> {
        let secret = self
            .store
            .reader()
            .and_then(|reader| reader.account_secret(key.account, key.kind.as_str()))
            .map_err(|err| store_error(&err))?;
        Ok(secret.map(|secret| Sensitive::new(secret.as_str().to_owned())))
    }

    fn store(&self, key: &SecretKey, secret: &Sensitive<String>) -> Result<(), SecretStoreError> {
        let (account, kind) = (key.account, key.kind.as_str());
        let secret = Zeroizing::new(secret.expose().clone());
        self.store
            .write_blocking(move |w| w.set_account_secret(account, kind, &secret))
            .map_err(|err| store_error(&err))
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretStoreError> {
        let (account, kind) = (key.account, key.kind.as_str());
        self.store
            .write_blocking(move |w| w.delete_account_secret(account, kind))
            .map_err(|err| store_error(&err))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use auth::{SecretKind, Uuid};
    use pretty_assertions::assert_eq;
    use store::{AccountKind, NewAccount, StoreConfig};
    use vault::{KEY_LEN, KeyId, MasterKey, Vault};

    use super::*;

    fn open(dir: &Path) -> Store {
        let mut config = StoreConfig::new(dir);
        config.gc_interval = None;
        let vault = Vault::from_master_key(MasterKey::from_bytes(KeyId::FIRST, [3; KEY_LEN]));
        Store::open(&config, &vault).unwrap()
    }

    fn add_account(store: &Store, name: &str) -> Uuid {
        let name = name.to_owned();
        store
            .write_blocking(move |w| {
                let id = w.create_account(&NewAccount {
                    kind: AccountKind::Imap,
                    name,
                    config_json: "{}".to_owned(),
                    ordinal: 0,
                })?;
                Ok(w.account(id)?.map(|a| a.uuid))
            })
            .unwrap()
            .unwrap()
    }

    fn secret(text: &str) -> Sensitive<String> {
        Sensitive::new(text.to_owned())
    }

    #[test]
    fn the_store_keeps_secrets_per_account() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(tmp.path());
        let a = add_account(&store, "a");
        let b = add_account(&store, "b");
        let secrets = StoreSecretStore::new(store);
        let pa = SecretKey::new(a, SecretKind::Password);
        let pb = SecretKey::new(b, SecretKind::Password);
        assert_eq!(secrets.load(&pa).unwrap(), None);
        secrets.store(&pa, &secret("one")).unwrap();
        secrets.store(&pa, &secret("two")).unwrap();
        assert_eq!(secrets.load(&pa).unwrap().unwrap().expose(), "two");
        assert_eq!(secrets.load(&pb).unwrap(), None);
        secrets.delete(&pa).unwrap();
        secrets.delete(&pa).unwrap();
        assert_eq!(secrets.load(&pa).unwrap(), None);
        // Unknown accounts can't have secrets.
        let unknown = SecretKey::new(Uuid::from_u128(9), SecretKind::Password);
        assert!(secrets.store(&unknown, &secret("x")).is_err());
        assert!(!format!("{secrets:?}").contains("two"));
    }
}
