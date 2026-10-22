use std::collections::HashMap;

use dbus_secret_service_keyring_store::Store;
use keyring_core::Entry;
use keyring_core::api::CredentialStoreApi;
use mazemail_types::Sensitive;
use zeroize::Zeroizing;

use super::{SERVICE, SecretKey, SecretStore, SecretStoreError};

/// The freedesktop Secret Service (GNOME Keyring, KWallet) as a [`SecretStore`], via
/// `keyring-core` and `dbus-secret-service-keyring-store`.
///
/// Each secret is an item with service [`SERVICE`] and user `<account-uuid>/<kind>` in
/// the default collection. Any process running as the same user can read an unlocked
/// collection (PLAN §4.17).
#[derive(Debug, Clone)]
pub struct SecretServiceStore {
    service: String,
}

impl Default for SecretServiceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretServiceStore {
    /// The store for Maze's items (service [`SERVICE`]).
    pub fn new() -> Self {
        Self::with_service(SERVICE)
    }

    /// A store with a custom service name (tests use a throwaway one).
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: &SecretKey) -> Result<Entry, SecretStoreError> {
        let store = Store::new().map_err(platform_error)?;
        let modifiers = HashMap::from([("label", key.kind.label())]);
        store
            .build(&self.service, &key.item_account(), Some(&modifiers))
            .map_err(platform_error)
    }
}

impl SecretStore for SecretServiceStore {
    fn load(&self, key: &SecretKey) -> Result<Option<Sensitive<String>>, SecretStoreError> {
        match self.entry(key)?.get_secret() {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| SecretStoreError::InvalidData)?;
                Ok(Some(Sensitive::new(text.to_owned())))
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(err) => Err(platform_error(err)),
        }
    }

    fn store(&self, key: &SecretKey, secret: &Sensitive<String>) -> Result<(), SecretStoreError> {
        self.entry(key)?
            .set_secret(secret.expose().as_bytes())
            .map_err(platform_error)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretStoreError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(err) => Err(platform_error(err)),
        }
    }
}

fn platform_error(err: keyring_core::Error) -> SecretStoreError {
    // Never include the error's debug form: `BadEncoding` and `BadDataFormat` carry the
    // secret bytes.
    let message = match err {
        keyring_core::Error::BadEncoding(_) | keyring_core::Error::BadDataFormat(..) => {
            "the stored secret has an unexpected format".to_owned()
        }
        other => other.to_string(),
    };
    SecretStoreError::Platform {
        code: None,
        message,
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::SecretKind;

    /// Talks to the real Secret Service with a throwaway item. Run manually:
    /// `cargo nextest run -p auth --run-ignored only real_secret_service`.
    #[test]
    #[ignore = "needs a running Secret Service"]
    fn real_secret_service_round_trip() {
        let store = SecretServiceStore::with_service(format!(
            "dev.mazemail.maze-test.auth.{}",
            std::process::id()
        ));
        let key = SecretKey::new(Uuid::from_u128(42), SecretKind::Password);
        store.delete(&key).unwrap();
        assert_eq!(store.load(&key).unwrap(), None);
        store
            .store(&key, &Sensitive::new("first".to_owned()))
            .unwrap();
        store
            .store(&key, &Sensitive::new("second".to_owned()))
            .unwrap();
        assert_eq!(store.load(&key).unwrap().unwrap().expose(), "second");
        store.delete(&key).unwrap();
        assert_eq!(store.load(&key).unwrap(), None);
    }
}
