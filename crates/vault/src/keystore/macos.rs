use std::sync::atomic::{AtomicBool, Ordering};

use security_framework::access_control::{ProtectionMode, SecAccessControl};
use security_framework::base::Error as SecError;
use security_framework::passwords::{
    PasswordOptions, delete_generic_password_options, generic_password,
    set_generic_password_options,
};
use zeroize::Zeroizing;

use super::{Flavor, ITEM_LABEL, KEY_ACCOUNT, KeyStore};
use crate::{KeyStoreError, MasterKey};

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_MISSING_ENTITLEMENT: i32 = -34018;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    /// The data protection keychain, scoped to an access group.
    DataProtection,
    /// The file-based login keychain.
    Login,
}

/// The macOS Keychain as a [`KeyStore`], via `security-framework`.
///
/// The master key is a generic password item (service [`Flavor::service`], account
/// [`KEY_ACCOUNT`]), not synchronizable, so it never reaches iCloud Keychain.
///
/// - **With an access group** (`<TeamID>.dev.mazemail.maze`), the item goes into the data
///   protection keychain, readable only by apps signed with that Team ID, with
///   `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` (not restored to another Mac).
///   That needs the `keychain-access-groups` entitlement, which outside the App Store
///   requires an embedded provisioning profile. Without it the Keychain answers
///   `errSecMissingEntitlement` (-34018); the store then logs a warning once and uses the
///   login keychain for the rest of the process.
/// - **Without an access group** (or after that fallback), the item goes into the login
///   keychain. Its default access list trusts only the app that created the item (by its
///   designated requirement), so any other app triggers a system prompt.
///
/// [`KeyStore::load`] also looks in the login keychain when the data protection keychain
/// has no item, so a key created by an earlier build without the entitlement is still
/// found. [`KeyStore::delete`] removes the item from both.
#[derive(Debug)]
pub struct KeychainStore {
    service: String,
    account: String,
    access_group: Option<String>,
    data_protection_unavailable: AtomicBool,
}

impl KeychainStore {
    /// The store for `flavor`'s item. Pass the access group to prefer the data
    /// protection keychain.
    pub fn new(flavor: Flavor, access_group: Option<String>) -> Self {
        Self::with_service(flavor.service(), access_group)
    }

    /// A store for an item with a custom service name (tests use a throwaway name).
    pub fn with_service(service: impl Into<String>, access_group: Option<String>) -> Self {
        Self {
            service: service.into(),
            account: KEY_ACCOUNT.to_owned(),
            access_group,
            data_protection_unavailable: AtomicBool::new(false),
        }
    }

    /// The data protection access group, unless it is unset or known not to work.
    fn data_protection_group(&self) -> Option<&str> {
        if self.data_protection_unavailable.load(Ordering::Relaxed) {
            None
        } else {
            self.access_group.as_deref()
        }
    }

    fn mark_data_protection_unavailable(&self) {
        if !self
            .data_protection_unavailable
            .swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                status = ERR_SEC_MISSING_ENTITLEMENT,
                "data protection keychain unavailable (missing keychain-access-groups \
                 entitlement); falling back to the login keychain"
            );
        }
    }

    /// Options identifying the item; `create` adds the attributes for a new item.
    fn options(&self, backend: Backend, create: bool) -> Result<PasswordOptions, SecError> {
        let mut options = PasswordOptions::new_generic_password(&self.service, &self.account);
        options.set_access_synchronized(Some(false));
        if backend == Backend::DataProtection {
            options.use_protected_keychain();
            if let Some(group) = &self.access_group {
                options.set_access_group(group);
            }
        }
        if create {
            options.set_label(ITEM_LABEL);
            options.set_description("Encrypts Maze's local mail storage");
            if backend == Backend::DataProtection {
                // Only the data protection keychain honours accessibility classes.
                options.set_access_control(SecAccessControl::create_with_protection(
                    Some(ProtectionMode::AccessibleAfterFirstUnlockThisDeviceOnly),
                    0,
                )?);
            }
        }
        Ok(options)
    }

    fn read(&self, backend: Backend) -> Result<Option<Zeroizing<Vec<u8>>>, SecError> {
        match generic_password(self.options(backend, false)?) {
            Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn write(&self, backend: Backend, stored: &[u8]) -> Result<(), SecError> {
        // An existing item keeps its attributes and only gets new data; matching it with
        // the creation attributes (access control) would fail the update.
        let create = self.read(backend)?.is_none();
        set_generic_password_options(stored, self.options(backend, create)?)
    }

    fn remove(&self, backend: Backend) -> Result<(), SecError> {
        match delete_generic_password_options(self.options(backend, false)?) {
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            other => other,
        }
    }
}

impl KeyStore for KeychainStore {
    fn load(&self) -> Result<Option<MasterKey>, KeyStoreError> {
        if self.data_protection_group().is_some() {
            match self.read(Backend::DataProtection) {
                Ok(Some(stored)) => return MasterKey::from_stored_bytes(&stored).map(Some),
                Ok(None) => {
                    tracing::debug!(
                        "no master key in the data protection keychain, trying the login keychain"
                    );
                }
                Err(err) if err.code() == ERR_SEC_MISSING_ENTITLEMENT => {
                    self.mark_data_protection_unavailable();
                }
                Err(err) => return Err(platform_error(err)),
            }
        }
        match self.read(Backend::Login).map_err(platform_error)? {
            Some(stored) => MasterKey::from_stored_bytes(&stored).map(Some),
            None => Ok(None),
        }
    }

    fn store(&self, key: &MasterKey) -> Result<(), KeyStoreError> {
        let stored = key.to_stored_bytes();
        if self.data_protection_group().is_some() {
            match self.write(Backend::DataProtection, &stored) {
                Ok(()) => return Ok(()),
                Err(err) if err.code() == ERR_SEC_MISSING_ENTITLEMENT => {
                    self.mark_data_protection_unavailable();
                }
                Err(err) => return Err(platform_error(err)),
            }
        }
        self.write(Backend::Login, &stored).map_err(platform_error)
    }

    fn delete(&self) -> Result<(), KeyStoreError> {
        if self.access_group.is_some() {
            match self.remove(Backend::DataProtection) {
                Ok(()) => {}
                Err(err) if err.code() == ERR_SEC_MISSING_ENTITLEMENT => {
                    self.mark_data_protection_unavailable();
                }
                Err(err) => return Err(platform_error(err)),
            }
        }
        self.remove(Backend::Login).map_err(platform_error)
    }
}

fn platform_error(err: SecError) -> KeyStoreError {
    KeyStoreError::Platform {
        code: Some(err.code()),
        message: err.message().unwrap_or_else(|| "Keychain error".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyId;

    /// Talks to the real login keychain with a throwaway item. Run manually:
    /// `cargo nextest run -p vault --run-ignored only real_keychain`.
    #[test]
    #[ignore = "touches the real keychain"]
    fn real_keychain_round_trip() {
        let service = format!("dev.mazemail.maze-test.storage.{}", std::process::id());
        // The access group can't work in an unsigned test binary, which exercises the
        // -34018 fallback to the login keychain.
        let store =
            KeychainStore::with_service(service, Some("XXXXXXXXXX.dev.mazemail.maze".to_owned()));
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());

        let first = MasterKey::generate(KeyId::FIRST).unwrap();
        store.store(&first).unwrap();
        assert!(store.load().unwrap().unwrap().ct_eq(&first));

        let second = MasterKey::generate(KeyId::FIRST.next().unwrap()).unwrap();
        store.store(&second).unwrap();
        assert!(store.load().unwrap().unwrap().ct_eq(&second));

        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
        assert!(
            store.data_protection_unavailable.load(Ordering::Relaxed),
            "an unsigned test binary should have fallen back to the login keychain"
        );
    }
}
