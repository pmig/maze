use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mazemail_types::Sensitive;
use zeroize::Zeroizing;

use super::{SecretKey, SecretStore, SecretStoreError};

/// An in-memory [`SecretStore`] for tests and headless runs. Nothing survives the
/// process; values are zeroized when replaced or dropped.
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    items: Mutex<HashMap<SecretKey, Sensitive<Zeroizing<String>>>>,
}

impl MemorySecretStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many secrets are stored.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Whether a secret exists for `key`.
    pub fn contains(&self, key: &SecretKey) -> bool {
        self.lock().contains_key(key)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SecretKey, Sensitive<Zeroizing<String>>>> {
        // Every operation leaves the map consistent, so a poisoned lock is still usable.
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SecretStore for MemorySecretStore {
    fn load(&self, key: &SecretKey) -> Result<Option<Sensitive<String>>, SecretStoreError> {
        Ok(self
            .lock()
            .get(key)
            .map(|secret| Sensitive::new(secret.expose().to_string())))
    }

    fn store(&self, key: &SecretKey, secret: &Sensitive<String>) -> Result<(), SecretStoreError> {
        self.lock().insert(
            *key,
            Sensitive::new(Zeroizing::new(secret.expose().clone())),
        );
        Ok(())
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretStoreError> {
        self.lock().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::SecretKind;

    #[test]
    fn store_load_delete() {
        let store = MemorySecretStore::new();
        let key = SecretKey::new(Uuid::from_u128(7), SecretKind::OAuthRefresh);
        assert_eq!(store.load(&key).unwrap(), None);
        store
            .store(&key, &Sensitive::new("one".to_owned()))
            .unwrap();
        store
            .store(&key, &Sensitive::new("two".to_owned()))
            .unwrap();
        assert_eq!(store.load(&key).unwrap().unwrap().expose(), "two");
        assert!(store.contains(&key));
        store.delete(&key).unwrap();
        store.delete(&key).unwrap();
        assert!(store.is_empty());
        assert!(!format!("{store:?}").contains("two"));
    }
}
