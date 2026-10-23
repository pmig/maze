//! Account secrets in the encrypted store, and the move out of the OS keychain
//! (ADR-017).
//!
//! IMAP passwords, OAuth refresh tokens and SMTP passwords are rows of the SQLCipher
//! database ([`StoreSecretStore`]), encrypted under the master key like the mail they
//! unlock. The master key is the only item Maze keeps in the keychain, so a launch
//! reads the keychain once.
//!
//! Earlier versions kept every secret as its own keychain item (one system prompt per
//! item and launch for a re-signed build). Schema v5 lists the accounts that existed
//! then; when the core opens, [`move_legacy_secrets`] copies each of their items into
//! the store and deletes it from the keychain. That costs one last round of keychain
//! prompts. It is idempotent: an account leaves the list once all of its secrets are in
//! the store, and the keychain is never touched again for it. A secret that can't be
//! read (keychain locked, prompt denied) or saved keeps the account on the list, so the
//! next start tries again; an item that was copied but can't be deleted is only
//! logged, so it never causes another prompt. Logs name the item
//! (`mazemail/<account-uuid>/<kind>`), never the secret.

use std::fmt;

use auth::{SecretKey, SecretKind, SecretStore, SecretStoreError};
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

/// What [`move_legacy_secrets`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegacyMoveReport {
    /// Secrets copied from the keychain into the store.
    pub moved: usize,
    /// Accounts whose keychain items are all handled (taken off the list).
    pub accounts_done: usize,
    /// Accounts left on the list because a secret couldn't be read or saved; the
    /// next start tries again.
    pub accounts_pending: usize,
    /// Keychain items that were copied (or superseded) but couldn't be deleted. They
    /// are not tried again.
    pub left_in_keychain: usize,
}

/// Moves the secrets of the accounts the store lists as possibly still in the OS
/// keychain from `legacy` into `target`, deleting each keychain item once the store has
/// the secret (see the [module docs](self)). Best effort: failures are logged (without
/// the secret) and counted. Blocking; touches `legacy` only if an account is listed.
///
/// # Errors
///
/// Only store errors while reading or updating the list.
pub fn move_legacy_secrets(
    store: &Store,
    target: &dyn SecretStore,
    legacy: &dyn SecretStore,
) -> Result<LegacyMoveReport> {
    let mut report = LegacyMoveReport::default();
    let accounts = store.reader()?.legacy_secret_accounts()?;
    if accounts.is_empty() {
        return Ok(report);
    }
    tracing::info!(
        accounts = accounts.len(),
        "moving account secrets from the keychain into the encrypted store"
    );
    for account in accounts {
        let mut complete = true;
        for kind in SecretKind::ALL {
            let key = SecretKey::new(account, kind);
            match move_one(&key, target, legacy) {
                Ok(Moved::Copied) => report.moved += 1,
                Ok(Moved::Nothing | Moved::AlreadyInStore) => {}
                Err(()) => {
                    complete = false;
                    continue;
                }
            }
            if let Err(err) = legacy.delete(&key) {
                tracing::warn!(
                    %key,
                    %err,
                    "the secret is in the encrypted store now, but its old keychain item \
                     couldn't be deleted; remove it in Keychain Access (or Seahorse)"
                );
                report.left_in_keychain += 1;
            }
        }
        if complete {
            store.write_blocking(move |w| w.finish_legacy_secrets(account))?;
            report.accounts_done += 1;
        } else {
            report.accounts_pending += 1;
        }
    }
    Ok(report)
}

enum Moved {
    /// Neither store has the secret: nothing to do.
    Nothing,
    /// The store has it already (an earlier, interrupted move, or a newer secret).
    AlreadyInStore,
    /// Copied from the keychain.
    Copied,
}

/// Copies one secret unless the store has it. `Err` (logged) if it should be tried
/// again at the next start.
fn move_one(
    key: &SecretKey,
    target: &dyn SecretStore,
    legacy: &dyn SecretStore,
) -> std::result::Result<Moved, ()> {
    match target.load(key) {
        Ok(Some(_)) => return Ok(Moved::AlreadyInStore),
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(%key, %err, "reading a secret from the encrypted store");
            return Err(());
        }
    }
    let secret = match legacy.load(key) {
        Ok(Some(secret)) => secret,
        Ok(None) => return Ok(Moved::Nothing),
        Err(err) => {
            tracing::warn!(
                %key,
                %err,
                "couldn't read the secret from the keychain to move it into the encrypted \
                 store; trying again at the next start"
            );
            return Err(());
        }
    };
    match target.store(key, &secret) {
        Ok(()) => Ok(Moved::Copied),
        Err(err) => {
            tracing::warn!(
                %key,
                %err,
                "couldn't save the secret from the keychain in the encrypted store; \
                 trying again at the next start"
            );
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::Mutex;

    use auth::{MemorySecretStore, Uuid};
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

    /// Puts accounts on the legacy list, as the v5 migration does for accounts that
    /// existed before it.
    fn list_as_legacy(store: &Store, accounts: &[Uuid]) {
        let accounts = accounts.to_vec();
        store
            .write_blocking(move |w| {
                for account in accounts {
                    w.list_legacy_secrets(account)?;
                }
                Ok(())
            })
            .unwrap();
    }

    fn secret(text: &str) -> Sensitive<String> {
        Sensitive::new(text.to_owned())
    }

    /// A fake keychain that can fail reads or deletes of chosen items.
    #[derive(Debug, Default)]
    struct FakeKeychain {
        items: MemorySecretStore,
        fail_load: Mutex<HashSet<SecretKey>>,
        fail_delete: Mutex<HashSet<SecretKey>>,
        loads: Mutex<usize>,
    }

    impl SecretStore for FakeKeychain {
        fn load(&self, key: &SecretKey) -> Result<Option<Sensitive<String>>, SecretStoreError> {
            *self.loads.lock().unwrap() += 1;
            if self.fail_load.lock().unwrap().contains(key) {
                return Err(SecretStoreError::Platform {
                    code: Some(-128),
                    message: "the user denied the prompt".to_owned(),
                });
            }
            self.items.load(key)
        }

        fn store(
            &self,
            key: &SecretKey,
            secret: &Sensitive<String>,
        ) -> Result<(), SecretStoreError> {
            self.items.store(key, secret)
        }

        fn delete(&self, key: &SecretKey) -> Result<(), SecretStoreError> {
            if self.fail_delete.lock().unwrap().contains(key) {
                return Err(SecretStoreError::Platform {
                    code: Some(-25293),
                    message: "not allowed".to_owned(),
                });
            }
            self.items.delete(key)
        }
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

    #[test]
    fn moves_keychain_items_once_and_deletes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(tmp.path());
        let a = add_account(&store, "a");
        let b = add_account(&store, "b");
        let later = add_account(&store, "added after the upgrade");
        list_as_legacy(&store, &[a, b]);
        let target = StoreSecretStore::new(store.clone());
        let keychain = FakeKeychain::default();
        let a_password = SecretKey::new(a, SecretKind::Password);
        let a_smtp = SecretKey::new(a, SecretKind::SmtpPassword);
        let b_refresh = SecretKey::new(b, SecretKind::OAuthRefresh);
        keychain
            .items
            .store(&a_password, &secret("hunter2"))
            .unwrap();
        keychain.items.store(&a_smtp, &secret("smtp")).unwrap();
        keychain.items.store(&b_refresh, &secret("1//rt")).unwrap();
        // Not listed, so never looked at.
        let later_password = SecretKey::new(later, SecretKind::Password);
        keychain
            .items
            .store(&later_password, &secret("other"))
            .unwrap();

        let report = move_legacy_secrets(&store, &target, &keychain).unwrap();
        assert_eq!(
            report,
            LegacyMoveReport {
                moved: 3,
                accounts_done: 2,
                accounts_pending: 0,
                left_in_keychain: 0,
            }
        );
        assert_eq!(
            target.load(&a_password).unwrap().unwrap().expose(),
            "hunter2"
        );
        assert_eq!(target.load(&a_smtp).unwrap().unwrap().expose(), "smtp");
        assert_eq!(target.load(&b_refresh).unwrap().unwrap().expose(), "1//rt");
        assert_eq!(target.load(&later_password).unwrap(), None);
        assert_eq!(keychain.items.len(), 1);
        assert!(keychain.items.contains(&later_password));
        assert!(
            store
                .reader()
                .unwrap()
                .legacy_secret_accounts()
                .unwrap()
                .is_empty()
        );

        // Again: nothing listed, so the keychain isn't touched at all.
        let loads = *keychain.loads.lock().unwrap();
        let again = move_legacy_secrets(&store, &target, &keychain).unwrap();
        assert_eq!(again, LegacyMoveReport::default());
        assert_eq!(*keychain.loads.lock().unwrap(), loads);
        assert_eq!(
            target.load(&a_password).unwrap().unwrap().expose(),
            "hunter2"
        );
    }

    #[test]
    fn failures_are_retried_or_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(tmp.path());
        let a = add_account(&store, "a");
        let b = add_account(&store, "b");
        list_as_legacy(&store, &[a, b]);
        let target = StoreSecretStore::new(store.clone());
        let keychain = FakeKeychain::default();
        let a_password = SecretKey::new(a, SecretKind::Password);
        let a_refresh = SecretKey::new(a, SecretKind::OAuthRefresh);
        let b_password = SecretKey::new(b, SecretKind::Password);
        keychain.items.store(&a_password, &secret("pw-a")).unwrap();
        keychain.items.store(&a_refresh, &secret("rt-a")).unwrap();
        keychain.items.store(&b_password, &secret("pw-b")).unwrap();
        // The user denies reading one of a's items, and deleting b's.
        keychain.fail_load.lock().unwrap().insert(a_refresh);
        keychain.fail_delete.lock().unwrap().insert(b_password);
        // The store already has a newer password for a: it wins.
        target.store(&a_password, &secret("newer")).unwrap();

        let report = move_legacy_secrets(&store, &target, &keychain).unwrap();
        assert_eq!(
            report,
            LegacyMoveReport {
                moved: 1,
                accounts_done: 1,
                accounts_pending: 1,
                left_in_keychain: 1,
            }
        );
        assert_eq!(target.load(&a_password).unwrap().unwrap().expose(), "newer");
        assert_eq!(target.load(&a_refresh).unwrap(), None);
        assert_eq!(target.load(&b_password).unwrap().unwrap().expose(), "pw-b");
        // a's stale password item is gone, the unreadable one stays for the retry; b
        // is done although its item couldn't be deleted.
        assert!(!keychain.items.contains(&a_password));
        assert!(keychain.items.contains(&a_refresh));
        assert!(keychain.items.contains(&b_password));
        assert_eq!(
            store.reader().unwrap().legacy_secret_accounts().unwrap(),
            vec![a]
        );

        // Next start, the prompt is allowed: a finishes.
        keychain.fail_load.lock().unwrap().clear();
        let report = move_legacy_secrets(&store, &target, &keychain).unwrap();
        assert_eq!(
            report,
            LegacyMoveReport {
                moved: 1,
                accounts_done: 1,
                accounts_pending: 0,
                left_in_keychain: 0,
            }
        );
        assert_eq!(target.load(&a_refresh).unwrap().unwrap().expose(), "rt-a");
        assert!(!keychain.items.contains(&a_refresh));
        assert!(
            store
                .reader()
                .unwrap()
                .legacy_secret_accounts()
                .unwrap()
                .is_empty()
        );
        // b's leftover item was never tried again.
        assert!(keychain.items.contains(&b_password));
    }
}
