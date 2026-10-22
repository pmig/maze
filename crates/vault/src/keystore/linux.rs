use std::collections::HashMap;

use dbus_secret_service_keyring_store::Store;
use keyring_core::Entry;
use keyring_core::api::CredentialStoreApi;
use zeroize::Zeroizing;

use super::{Flavor, ITEM_LABEL, KEY_ACCOUNT, KeyStore};
use crate::{KeyStoreError, MasterKey};

/// The freedesktop Secret Service (GNOME Keyring, KWallet) as a [`KeyStore`].
///
/// The master key is an item with service [`Flavor::service`] and user [`KEY_ACCOUNT`]
/// in the default collection. Any process running as the same user can read an unlocked
/// collection, so on Linux encryption at rest protects copies and backups of the data
/// directory, not the live system (PLAN §4.17).
#[derive(Debug, Clone)]
pub struct SecretServiceStore {
    service: String,
    account: String,
}

impl SecretServiceStore {
    /// The store for `flavor`'s item.
    pub fn new(flavor: Flavor) -> Self {
        Self::with_service(flavor.service())
    }

    /// A store for an item with a custom service name (tests use a throwaway name).
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: KEY_ACCOUNT.to_owned(),
        }
    }

    fn entry(&self) -> Result<Entry, KeyStoreError> {
        let store = Store::new().map_err(platform_error)?;
        let modifiers = HashMap::from([("label", ITEM_LABEL)]);
        store
            .build(&self.service, &self.account, Some(&modifiers))
            .map_err(platform_error)
    }
}

impl KeyStore for SecretServiceStore {
    fn load(&self) -> Result<Option<MasterKey>, KeyStoreError> {
        match self.entry()?.get_secret() {
            Ok(secret) => {
                let secret = Zeroizing::new(secret);
                MasterKey::from_stored_bytes(&secret).map(Some)
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(err) => Err(platform_error(err)),
        }
    }

    fn store(&self, key: &MasterKey) -> Result<(), KeyStoreError> {
        self.entry()?
            .set_secret(&key.to_stored_bytes())
            .map_err(platform_error)
    }

    fn delete(&self) -> Result<(), KeyStoreError> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(err) => Err(platform_error(err)),
        }
    }
}

fn platform_error(err: keyring_core::Error) -> KeyStoreError {
    // Never include the error's debug form: `BadEncoding` and `BadDataFormat` carry the
    // secret bytes.
    let message = match err {
        keyring_core::Error::BadEncoding(_) | keyring_core::Error::BadDataFormat(..) => {
            "the stored secret has an unexpected format".to_owned()
        }
        other => other.to_string(),
    };
    KeyStoreError::Platform {
        code: None,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyId;

    /// Talks to the real Secret Service with a throwaway item. Run manually:
    /// `cargo nextest run -p vault --run-ignored only real_secret_service`.
    #[test]
    #[ignore = "needs a running Secret Service"]
    fn real_secret_service_round_trip() {
        let service = format!("dev.mazemail.maze-test.storage.{}", std::process::id());
        let store = SecretServiceStore::with_service(service);
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
        let key = MasterKey::generate(KeyId::FIRST).unwrap();
        store.store(&key).unwrap();
        assert!(store.load().unwrap().unwrap().ct_eq(&key));
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}
