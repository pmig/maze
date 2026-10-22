use std::fmt;

use crate::{
    BlobCipher, BlobIdHasher, BlockCipher, Error, KeyId, KeyStore, MasterKey, RecoveryKey,
    SecretString, SubKey,
};

/// What startup found, and what Maze must do about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VaultState {
    /// No key and no encrypted data: a fresh key was (or may be) created.
    Created,
    /// A key was found in the key store.
    Loaded,
    /// Encrypted data exists but there is no key. Maze offers "Enter recovery key" or
    /// "Start fresh and re-sync" and never creates a key silently, since that would
    /// orphan the existing data.
    MissingKeyForExistingData,
}

impl VaultState {
    /// The startup decision. `data_exists` is the caller's answer to "is there any
    /// encrypted data under `mail/`?" (for example: does `mazemail.db` exist).
    pub const fn decide(key_found: bool, data_exists: bool) -> Self {
        match (key_found, data_exists) {
            (true, _) => Self::Loaded,
            (false, false) => Self::Created,
            (false, true) => Self::MissingKeyForExistingData,
        }
    }
}

/// The unlocked master key, and the entry point to every key-derived primitive.
///
/// Read it once at startup (off the UI thread, since keychain calls block) and share it;
/// it is `Send + Sync`. `Debug` shows only the key generation and state.
pub struct Vault {
    master: MasterKey,
    state: VaultState,
}

impl Vault {
    /// Loads the master key from `store`, or creates and stores a new one when there is
    /// neither a key nor encrypted data.
    ///
    /// Returns [`Error::MissingKeyForExistingData`] when `data_exists` is true but the
    /// store has no key; recover with [`Vault::from_recovery_key`] + [`Vault::persist`],
    /// or move the old data aside and call [`Vault::start_fresh`].
    ///
    /// A newly created key is read back from the store before it is used, so data is
    /// never sealed with a key that didn't persist.
    pub fn open_or_create(store: &dyn KeyStore, data_exists: bool) -> Result<Self, Error> {
        let Some(master) = store.load()? else {
            // Only create a key when the decision says so; anything else fails closed.
            return match VaultState::decide(false, data_exists) {
                VaultState::Created => Self::create(store),
                VaultState::Loaded | VaultState::MissingKeyForExistingData => {
                    tracing::warn!("encrypted data exists but the master key is missing");
                    Err(Error::MissingKeyForExistingData)
                }
            };
        };
        tracing::debug!(key_id = %master.key_id(), "master key loaded");
        Ok(Self {
            master,
            state: VaultState::Loaded,
        })
    }

    /// Creates and stores a new master key for "Start fresh and re-sync".
    ///
    /// Fails with [`Error::KeyAlreadyExists`] if the store already holds a key. The caller
    /// is responsible for moving the old encrypted data aside first; this never deletes
    /// anything.
    pub fn start_fresh(store: &dyn KeyStore) -> Result<Self, Error> {
        if store.load()?.is_some() {
            return Err(Error::KeyAlreadyExists);
        }
        Self::create(store)
    }

    /// Unlocks with a recovery key the user typed. Nothing is stored yet: check that the
    /// key opens the existing data (for example the database), then call
    /// [`Vault::persist`].
    pub fn from_recovery_key(input: &str) -> Result<Self, Error> {
        Ok(Self::from_master_key(MasterKey::from_recovery_key(input)?))
    }

    /// Wraps an existing key, for tests and for key rotation.
    pub fn from_master_key(master: MasterKey) -> Self {
        Self {
            master,
            state: VaultState::Loaded,
        }
    }

    /// Saves this vault's key in `store` and reads it back to verify.
    ///
    /// Succeeds without writing if the store already holds this exact key; fails with
    /// [`Error::KeyAlreadyExists`] if it holds a different one.
    pub fn persist(&self, store: &dyn KeyStore) -> Result<(), Error> {
        match store.load()? {
            Some(existing) if existing.ct_eq(&self.master) => Ok(()),
            Some(_) => Err(Error::KeyAlreadyExists),
            None => store_verified(store, &self.master),
        }
    }

    fn create(store: &dyn KeyStore) -> Result<Self, Error> {
        let master = MasterKey::generate(KeyId::FIRST)?;
        store_verified(store, &master)?;
        tracing::info!(key_id = %master.key_id(), "created a new master key");
        Ok(Self {
            master,
            state: VaultState::Created,
        })
    }

    /// How this vault was opened: [`VaultState::Created`] or [`VaultState::Loaded`].
    pub fn state(&self) -> VaultState {
        self.state
    }

    /// Generation of the master key.
    pub fn key_id(&self) -> KeyId {
        self.master.key_id()
    }

    /// The master key, for building ciphers of a specific generation.
    pub fn master_key(&self) -> &MasterKey {
        &self.master
    }

    /// Derives a subkey for `label` (see [`crate::label`]).
    pub fn derive(&self, label: &str) -> SubKey {
        self.master.derive(label)
    }

    /// The SQLCipher raw key, `x'<64 hex>'`; use as `PRAGMA key = "x'…'"`.
    pub fn sqlcipher_pragma_key(&self) -> SecretString {
        self.master.sqlcipher_pragma_key()
    }

    /// The cipher for `MZB1` blobs and caches.
    pub fn blob_cipher(&self) -> BlobCipher {
        BlobCipher::new(&self.master)
    }

    /// The keyed hasher for blob ids.
    pub fn blob_id_hasher(&self) -> BlobIdHasher {
        BlobIdHasher::new(&self.master)
    }

    /// The cipher for `MZI1` index blocks.
    pub fn block_cipher(&self) -> BlockCipher {
        BlockCipher::new(&self.master)
    }

    /// The recovery key to show in Settings → Privacy.
    pub fn recovery_key(&self) -> RecoveryKey {
        self.master.to_recovery_key()
    }
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vault")
            .field("key_id", &self.master.key_id())
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

fn store_verified(store: &dyn KeyStore, key: &MasterKey) -> Result<(), Error> {
    store.store(key)?;
    match store.load()? {
        Some(read_back) if read_back.ct_eq(key) => Ok(()),
        _ => Err(Error::KeyStoreVerification),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KeyStoreError, MemoryKeyStore};

    #[test]
    fn decide_covers_every_case() {
        assert_eq!(VaultState::decide(true, true), VaultState::Loaded);
        assert_eq!(VaultState::decide(true, false), VaultState::Loaded);
        assert_eq!(VaultState::decide(false, false), VaultState::Created);
        assert_eq!(
            VaultState::decide(false, true),
            VaultState::MissingKeyForExistingData
        );
    }

    #[test]
    fn first_launch_creates_then_loads() {
        let store = MemoryKeyStore::new();
        let created = Vault::open_or_create(&store, false).unwrap();
        assert_eq!(created.state(), VaultState::Created);
        assert_eq!(created.key_id(), KeyId::FIRST);

        let loaded = Vault::open_or_create(&store, true).unwrap();
        assert_eq!(loaded.state(), VaultState::Loaded);
        assert!(loaded.master_key().ct_eq(created.master_key()));
    }

    #[test]
    fn missing_key_with_existing_data_never_creates_a_key() {
        let store = MemoryKeyStore::new();
        let err = Vault::open_or_create(&store, true).unwrap_err();
        assert!(matches!(err, Error::MissingKeyForExistingData), "{err:?}");
        assert!(!store.contains_key());
    }

    #[test]
    fn recovery_flow_restores_the_old_key() {
        let store = MemoryKeyStore::new();
        let original = Vault::open_or_create(&store, false).unwrap();
        let sealed = original
            .blob_cipher()
            .seal(b"mail", &mazemail_types::BlobId([1; 32]))
            .unwrap();
        let recovery = original.recovery_key();
        store.delete().unwrap();

        assert!(matches!(
            Vault::open_or_create(&store, true),
            Err(Error::MissingKeyForExistingData)
        ));
        let recovered = Vault::from_recovery_key(recovery.expose_secret()).unwrap();
        assert_eq!(
            recovered
                .blob_cipher()
                .open(&sealed, &mazemail_types::BlobId([1; 32]))
                .unwrap(),
            b"mail"
        );
        recovered.persist(&store).unwrap();
        recovered.persist(&store).unwrap();
        let loaded = Vault::open_or_create(&store, true).unwrap();
        assert!(loaded.master_key().ct_eq(original.master_key()));
    }

    #[test]
    fn persist_refuses_to_replace_a_different_key() {
        let store = MemoryKeyStore::new();
        Vault::open_or_create(&store, false).unwrap();
        let other = Vault::from_master_key(MasterKey::generate(KeyId::FIRST).unwrap());
        assert!(matches!(
            other.persist(&store),
            Err(Error::KeyAlreadyExists)
        ));
    }

    #[test]
    fn start_fresh_only_without_a_key() {
        let store = MemoryKeyStore::new();
        let fresh = Vault::start_fresh(&store).unwrap();
        assert_eq!(fresh.state(), VaultState::Created);
        assert!(matches!(
            Vault::start_fresh(&store),
            Err(Error::KeyAlreadyExists)
        ));
    }

    /// A store that accepts writes but forgets them.
    #[derive(Debug)]
    struct ForgetfulStore;

    impl KeyStore for ForgetfulStore {
        fn load(&self) -> Result<Option<MasterKey>, KeyStoreError> {
            Ok(None)
        }
        fn store(&self, _key: &MasterKey) -> Result<(), KeyStoreError> {
            Ok(())
        }
        fn delete(&self) -> Result<(), KeyStoreError> {
            Ok(())
        }
    }

    #[test]
    fn a_key_that_did_not_persist_is_never_used() {
        assert!(matches!(
            Vault::open_or_create(&ForgetfulStore, false),
            Err(Error::KeyStoreVerification)
        ));
    }

    /// A store that fails every call.
    #[derive(Debug)]
    struct BrokenStore;

    impl KeyStore for BrokenStore {
        fn load(&self) -> Result<Option<MasterKey>, KeyStoreError> {
            Err(KeyStoreError::Platform {
                code: Some(-25308),
                message: "interaction not allowed".to_owned(),
            })
        }
        fn store(&self, _key: &MasterKey) -> Result<(), KeyStoreError> {
            unreachable!("must not store after a failed load")
        }
        fn delete(&self) -> Result<(), KeyStoreError> {
            Ok(())
        }
    }

    #[test]
    fn a_failing_store_is_an_error_not_a_new_key() {
        let err = Vault::open_or_create(&BrokenStore, false).unwrap_err();
        assert!(matches!(err, Error::KeyStore(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            "key store error -25308: interaction not allowed"
        );
    }

    #[test]
    fn primitives_share_the_key_generation() {
        let vault = Vault::from_master_key(MasterKey::generate(KeyId::FIRST).unwrap());
        assert_eq!(vault.blob_cipher().key_id(), KeyId::FIRST);
        assert_eq!(vault.block_cipher().key_id(), KeyId::FIRST);
        assert_eq!(vault.derive("x").key_id(), KeyId::FIRST);
        assert_eq!(
            format!("{vault:?}"),
            "Vault { key_id: KeyId(1), state: Loaded, .. }"
        );
        assert!(
            MasterKey::from_recovery_key(vault.recovery_key().expose_secret())
                .unwrap()
                .ct_eq(vault.master_key())
        );
        assert_eq!(vault.sqlcipher_pragma_key().expose_secret().len(), 67);
    }

    #[test]
    fn vault_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Vault>();
        assert_send_sync::<BlobCipher>();
        assert_send_sync::<BlockCipher>();
    }
}
