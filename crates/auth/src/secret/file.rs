//! [`FileSecretStore`]: account secrets in a plain JSON file, for development and
//! scripted end-to-end runs only.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use mazemail_types::Sensitive;
use zeroize::Zeroizing;

use super::{SecretKey, SecretStore, SecretStoreError};

/// Keeps secrets **unencrypted** in a JSON file (mode 0600), keyed like the keychain
/// (`<account-uuid>/<kind>`).
///
/// This protects nothing: whoever can read the file has the passwords. It exists so
/// automated tests and demos can run without a keychain (which may prompt or be
/// locked), behind the binary's `MAZEMAIL_INSECURE_DEV_SECRETS=1` switch, the way the
/// development key store keeps the master key in a file. It is never the default, it
/// warns when created, and it logs every access at debug level (keys only, never the
/// secrets).
pub struct FileSecretStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl std::fmt::Debug for FileSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSecretStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// The file's content; the secrets are wiped from memory when it is dropped.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct Items(BTreeMap<String, String>);

impl Drop for Items {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        for secret in self.0.values_mut() {
            secret.zeroize();
        }
    }
}

fn platform(message: impl std::fmt::Display) -> SecretStoreError {
    SecretStoreError::Platform {
        code: None,
        message: message.to_string(),
    }
}

impl FileSecretStore {
    /// A store in `path` (created on the first write). Logs a warning.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        tracing::warn!(
            path = %path.display(),
            "INSECURE: account passwords and tokens are kept unencrypted in a plain file; \
             use this only for development and automated tests"
        );
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read(&self) -> Result<Items, SecretStoreError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                serde_json::from_slice(&bytes).map_err(|_| SecretStoreError::InvalidData)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Items::default()),
            Err(err) => Err(platform(format!("reading the secrets file: {err}"))),
        }
    }

    fn write(&self, items: &Items) -> Result<(), SecretStoreError> {
        let json = Zeroizing::new(serde_json::to_vec_pretty(items).map_err(platform)?);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|err| platform(format!("creating the secrets directory: {err}")))?;
        }
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let written = options
            .open(&tmp)
            .and_then(|mut file| file.write_all(&json).and_then(|()| file.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(platform(format!("writing the secrets file: {err}")));
        }
        Ok(())
    }
}

impl SecretStore for FileSecretStore {
    fn load(&self, key: &SecretKey) -> Result<Option<Sensitive<String>>, SecretStoreError> {
        let _guard = self.guard();
        tracing::debug!(%key, "INSECURE file secret store: load");
        Ok(self
            .read()?
            .0
            .get(&key.item_account())
            .map(|secret| Sensitive::new(secret.clone())))
    }

    fn store(&self, key: &SecretKey, secret: &Sensitive<String>) -> Result<(), SecretStoreError> {
        let _guard = self.guard();
        tracing::debug!(%key, "INSECURE file secret store: store");
        let mut items = self.read()?;
        items.0.insert(key.item_account(), secret.expose().clone());
        self.write(&items)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretStoreError> {
        let _guard = self.guard();
        tracing::debug!(%key, "INSECURE file secret store: delete");
        let mut items = self.read()?;
        if let Some(mut removed) = items.0.remove(&key.item_account()) {
            zeroize::Zeroize::zeroize(&mut removed);
            self.write(&items)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::SecretKind;

    fn unique() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn round_trip_survives_a_new_instance_and_is_private() {
        let dir = std::env::temp_dir().join(format!("mazemail-secrets-{}", unique()));
        let path = dir.join("secrets.json");
        let key = SecretKey::new(Uuid::from_u128(7), SecretKind::Password);
        let other = SecretKey::new(Uuid::from_u128(8), SecretKind::OAuthRefresh);
        let store = FileSecretStore::new(&path);
        assert_eq!(store.load(&key).unwrap(), None);
        store
            .store(&key, &Sensitive::new("hunter2".to_owned()))
            .unwrap();
        store
            .store(&other, &Sensitive::new("rt".to_owned()))
            .unwrap();

        let reopened = FileSecretStore::new(&path);
        assert_eq!(reopened.load(&key).unwrap().unwrap().expose(), "hunter2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        reopened.delete(&key).unwrap();
        reopened.delete(&key).unwrap();
        assert_eq!(store.load(&key).unwrap(), None);
        assert_eq!(store.load(&other).unwrap().unwrap().expose(), "rt");
        assert!(!format!("{store:?}").contains("rt\""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn garbage_is_invalid_data() {
        let dir = std::env::temp_dir().join(format!("mazemail-secrets-{}", unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.json");
        std::fs::write(&path, b"not json").unwrap();
        let store = FileSecretStore::new(&path);
        let key = SecretKey::new(Uuid::from_u128(1), SecretKind::Password);
        assert_eq!(store.load(&key), Err(SecretStoreError::InvalidData));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
