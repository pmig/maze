use mazemail_types::Sensitive;
use security_framework::base::Error as SecError;
use security_framework::passwords::{
    PasswordOptions, delete_generic_password_options, generic_password,
    set_generic_password_options,
};
use zeroize::Zeroizing;

use super::{SERVICE, SecretKey, SecretStore, SecretStoreError};

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// The macOS login keychain as a [`SecretStore`], via `security-framework`.
///
/// Each secret is a generic password item (service [`SERVICE`], account
/// `<account-uuid>/<kind>`), not synchronizable, so it never reaches iCloud Keychain.
/// The item's default access list trusts only the app that created it; other apps get
/// a system prompt. Unlike the storage master key (`vault`), account secrets don't
/// need the data protection keychain (PLAN §4.17).
#[derive(Debug, Clone)]
pub struct KeychainSecretStore {
    service: String,
}

impl Default for KeychainSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeychainSecretStore {
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

    fn options(&self, key: &SecretKey, create: bool) -> PasswordOptions {
        let mut options = PasswordOptions::new_generic_password(&self.service, &key.item_account());
        options.set_access_synchronized(Some(false));
        if create {
            options.set_label(key.kind.label());
        }
        options
    }

    fn read(&self, key: &SecretKey) -> Result<Option<Zeroizing<Vec<u8>>>, SecError> {
        match generic_password(self.options(key, false)) {
            Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl SecretStore for KeychainSecretStore {
    fn load(&self, key: &SecretKey) -> Result<Option<Sensitive<String>>, SecretStoreError> {
        let Some(bytes) = self.read(key).map_err(platform_error)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| SecretStoreError::InvalidData)?;
        Ok(Some(Sensitive::new(text.to_owned())))
    }

    fn store(&self, key: &SecretKey, secret: &Sensitive<String>) -> Result<(), SecretStoreError> {
        // An existing item keeps its attributes and only gets new data.
        let create = self.read(key).map_err(platform_error)?.is_none();
        set_generic_password_options(secret.expose().as_bytes(), self.options(key, create))
            .map_err(platform_error)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretStoreError> {
        match delete_generic_password_options(self.options(key, false)) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(err) => Err(platform_error(err)),
        }
    }
}

fn platform_error(err: SecError) -> SecretStoreError {
    SecretStoreError::Platform {
        code: Some(err.code()),
        message: err.message().unwrap_or_else(|| "Keychain error".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::SecretKind;

    /// Talks to the real login keychain with a throwaway item. Run manually:
    /// `cargo nextest run -p auth --run-ignored only real_keychain`.
    #[test]
    #[ignore = "touches the real keychain"]
    fn real_keychain_round_trip() {
        let store = KeychainSecretStore::with_service(format!(
            "dev.mazemail.maze-test.auth.{}",
            std::process::id()
        ));
        let key = SecretKey::new(Uuid::from_u128(42), SecretKind::OAuthRefresh);
        store.delete(&key).unwrap();
        assert_eq!(store.load(&key).unwrap(), None);

        store
            .store(&key, &Sensitive::new("first".to_owned()))
            .unwrap();
        assert_eq!(store.load(&key).unwrap().unwrap().expose(), "first");
        store
            .store(&key, &Sensitive::new("second".to_owned()))
            .unwrap();
        assert_eq!(store.load(&key).unwrap().unwrap().expose(), "second");

        store.delete(&key).unwrap();
        store.delete(&key).unwrap();
        assert_eq!(store.load(&key).unwrap(), None);
    }
}
