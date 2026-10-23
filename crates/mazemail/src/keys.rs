//! Where the master key and the account secrets live (PLAN §4.6, §4.17).
//!
//! Production builds keep it in the platform key store: the macOS keychain or the
//! Secret Service on Linux, with a separate item for development builds so a dev build
//! never touches the real key ([`vault::Flavor`]).
//!
//! For demos and scripted end-to-end runs, `MAZEMAIL_INSECURE_DEV_KEYSTORE=1` keeps the
//! key in a file inside the data directory instead ([`DEV_KEY_FILE`], mode 0600). That
//! file sits next to the data it decrypts, so it protects nothing: it is never the
//! default, and every start with it logs a warning.
//!
//! The master key is Maze's only keychain item (ADR-017). Account passwords and OAuth
//! refresh tokens live in the encrypted store, under that key
//! ([`mazemail_core::secrets`]); keychain items of earlier versions (service
//! `mazemail`, [`auth::legacy_system_secret_store`]) are moved there once, at the first
//! start of a version with schema v5. For automated end-to-end runs that plant or
//! inspect secrets from outside, `MAZEMAIL_INSECURE_DEV_SECRETS=1` keeps them
//! **unencrypted** in a file in the data directory ([`DEV_SECRETS_FILE`], mode 0600)
//! instead, with the same warnings.

use std::path::Path;
use std::sync::Arc;

use mazemail_core::AccountSecretsLocation;
use release_channel::ReleaseChannel;
use vault::{FileKeyStore, Flavor, KeyStore};

/// Set to `1` to keep the master key in a plain file in the data directory.
pub const INSECURE_DEV_KEYSTORE_ENV: &str = "MAZEMAIL_INSECURE_DEV_KEYSTORE";

/// The key file name inside the data directory with [`INSECURE_DEV_KEYSTORE_ENV`].
pub const DEV_KEY_FILE: &str = "insecure-dev-master-key";

/// Set to `1` to keep account passwords and tokens in a plain file in the data
/// directory.
pub const INSECURE_DEV_SECRETS_ENV: &str = "MAZEMAIL_INSECURE_DEV_SECRETS";

/// The secrets file name inside the data directory with [`INSECURE_DEV_SECRETS_ENV`].
pub const DEV_SECRETS_FILE: &str = "insecure-dev-secrets.json";

/// Whether [`INSECURE_DEV_SECRETS_ENV`] is set to `1`.
pub fn insecure_dev_secrets_requested() -> bool {
    std::env::var(INSECURE_DEV_SECRETS_ENV).is_ok_and(|value| value.trim() == "1")
}

/// Where account secrets live: the file store if [`INSECURE_DEV_SECRETS_ENV`] is set,
/// else the encrypted store, with the keychain of earlier versions to move them from.
pub fn secret_store(data_dir: &Path) -> AccountSecretsLocation {
    if insecure_dev_secrets_requested() {
        let path = data_dir.join(DEV_SECRETS_FILE);
        tracing::warn!(
            path = %path.display(),
            "INSECURE: account passwords and tokens are kept unencrypted in a plain file \
             ({INSECURE_DEV_SECRETS_ENV}=1); use this only for development and tests"
        );
        return AccountSecretsLocation::Custom(Arc::new(auth::FileSecretStore::new(path)));
    }
    AccountSecretsLocation::Encrypted {
        legacy: auth::legacy_system_secret_store(),
    }
}

/// No key store is available on this platform.
#[derive(Debug, thiserror::Error)]
#[error(
    "this platform has no supported key store for the storage key; set \
     {INSECURE_DEV_KEYSTORE_ENV}=1 for a development setup"
)]
pub struct NoKeyStore;

/// Whether [`INSECURE_DEV_KEYSTORE_ENV`] is set to `1`.
pub fn insecure_dev_keystore_requested() -> bool {
    std::env::var(INSECURE_DEV_KEYSTORE_ENV).is_ok_and(|value| value.trim() == "1")
}

/// The key store for `data_dir`: the file store if [`INSECURE_DEV_KEYSTORE_ENV`] is
/// set, else the platform key store for this build's channel.
///
/// # Errors
///
/// [`NoKeyStore`] on platforms without a supported key store.
pub fn key_store(data_dir: &Path) -> Result<Arc<dyn KeyStore>, NoKeyStore> {
    if insecure_dev_keystore_requested() {
        return Ok(insecure_dev_key_store(data_dir));
    }
    platform_key_store(flavor(release_channel::RELEASE_CHANNEL))
}

/// The development file key store in `data_dir`, with the warning.
pub fn insecure_dev_key_store(data_dir: &Path) -> Arc<dyn KeyStore> {
    let path = data_dir.join(DEV_KEY_FILE);
    tracing::warn!(
        path = %path.display(),
        "INSECURE: the storage key is kept in a plain file next to the mail data \
         ({INSECURE_DEV_KEYSTORE_ENV}=1); use this only for development and demos"
    );
    Arc::new(FileKeyStore::new(path))
}

/// Dev builds use their own keychain item.
fn flavor(channel: ReleaseChannel) -> Flavor {
    match channel {
        ReleaseChannel::Dev => Flavor::Dev,
        ReleaseChannel::Preview | ReleaseChannel::Stable => Flavor::Release,
    }
}

#[cfg(target_os = "macos")]
fn platform_key_store(flavor: Flavor) -> Result<Arc<dyn KeyStore>, NoKeyStore> {
    // The data protection keychain needs a signed build with the keychain access
    // group entitlement; until release signing provides it, the login keychain is used.
    Ok(Arc::new(vault::KeychainStore::new(flavor, None)))
}

#[cfg(target_os = "linux")]
fn platform_key_store(flavor: Flavor) -> Result<Arc<dyn KeyStore>, NoKeyStore> {
    Ok(Arc::new(vault::SecretServiceStore::new(flavor)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_key_store(_flavor: Flavor) -> Result<Arc<dyn KeyStore>, NoKeyStore> {
    Err(NoKeyStore)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_builds_use_their_own_item() {
        assert_eq!(flavor(ReleaseChannel::Dev), Flavor::Dev);
        assert_eq!(flavor(ReleaseChannel::Stable), Flavor::Release);
        assert_eq!(flavor(ReleaseChannel::Preview), Flavor::Release);
    }

    #[test]
    fn the_dev_secret_store_is_a_file_in_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        use auth::SecretStore as _;
        let store = auth::FileSecretStore::new(dir.path().join(DEV_SECRETS_FILE));
        let key = auth::SecretKey::new(auth::Uuid::from_u128(1), auth::SecretKind::Password);
        store
            .store(&key, &mazemail_types::Sensitive::new("pw".to_owned()))
            .unwrap();
        assert!(dir.path().join(DEV_SECRETS_FILE).is_file());
        // Without the switch the encrypted store is used (the keychain isn't touched
        // here).
        if !insecure_dev_secrets_requested() {
            assert!(matches!(
                secret_store(dir.path()),
                AccountSecretsLocation::Encrypted { .. }
            ));
        }
    }

    #[test]
    fn the_dev_key_store_is_a_file_in_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = insecure_dev_key_store(dir.path());
        assert!(store.load().unwrap().is_none());
        let key = vault::MasterKey::generate(vault::KeyId::FIRST).unwrap();
        store.store(&key).unwrap();
        assert!(dir.path().join(DEV_KEY_FILE).is_file());
        assert!(store.load().unwrap().is_some());
    }
}
