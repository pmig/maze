use std::sync::{Mutex, MutexGuard, PoisonError};

use zeroize::Zeroizing;

use super::KeyStore;
use crate::{KeyStoreError, MasterKey};

/// An in-memory [`KeyStore`] for tests. Nothing survives the process.
#[derive(Debug, Default)]
pub struct MemoryKeyStore {
    stored: Mutex<Option<Zeroizing<Vec<u8>>>>,
}

impl MemoryKeyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a key is stored.
    pub fn contains_key(&self) -> bool {
        self.lock().is_some()
    }

    fn lock(&self) -> MutexGuard<'_, Option<Zeroizing<Vec<u8>>>> {
        // The guarded value is always consistent, so a poisoned lock is still usable.
        self.stored.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl KeyStore for MemoryKeyStore {
    fn load(&self) -> Result<Option<MasterKey>, KeyStoreError> {
        self.lock()
            .as_ref()
            .map(|stored| MasterKey::from_stored_bytes(stored))
            .transpose()
    }

    fn store(&self, key: &MasterKey) -> Result<(), KeyStoreError> {
        *self.lock() = Some(key.to_stored_bytes());
        Ok(())
    }

    fn delete(&self) -> Result<(), KeyStoreError> {
        *self.lock() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyId;

    #[test]
    fn store_load_delete() {
        let store = MemoryKeyStore::new();
        assert!(store.load().unwrap().is_none());
        let key = MasterKey::generate(KeyId::FIRST).unwrap();
        store.store(&key).unwrap();
        assert!(store.contains_key());
        assert!(store.load().unwrap().unwrap().ct_eq(&key));
        store.delete().unwrap();
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}
