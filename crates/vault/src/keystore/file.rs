use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use super::KeyStore;
use crate::{KeyStoreError, MasterKey};

/// A [`KeyStore`] that keeps the master key in a plain file. **Insecure.**
///
/// Anyone who can read the file can read all mail, and a copy of the file travels with
/// every backup that includes it. Use it only for tests, CI and development setups that
/// have no keychain, and never put the file inside the data directory it protects.
///
/// On Unix the file is created with mode `0600` and replaced atomically (write to a
/// temporary file, fsync, rename, fsync the directory).
#[derive(Debug, Clone)]
pub struct FileKeyStore {
    path: PathBuf,
}

impl FileKeyStore {
    /// A store backed by the file at `path` (created on first [`KeyStore::store`]).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The key file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn temp_path(&self) -> Result<PathBuf, KeyStoreError> {
        let suffix = getrandom::u64().map_err(|err| io::Error::other(err.to_string()))?;
        let mut name = self
            .path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "key path has no file name")
            })?
            .to_os_string();
        name.push(format!(".tmp-{suffix:016x}"));
        Ok(self.path.with_file_name(name))
    }
}

impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<MasterKey>, KeyStoreError> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        warn_if_readable_by_others(&file);
        let mut stored = Zeroizing::new(Vec::with_capacity(64));
        // Cap the read: a valid key file is 40 bytes, anything larger is malformed.
        Read::by_ref(&mut file)
            .take(1024)
            .read_to_end(&mut stored)?;
        MasterKey::from_stored_bytes(&stored).map(Some)
    }

    fn store(&self, key: &MasterKey) -> Result<(), KeyStoreError> {
        let temp = self.temp_path()?;
        let result = write_new_private(&temp, &key.to_stored_bytes())
            .and_then(|()| fs::rename(&temp, &self.path))
            .and_then(|()| sync_parent(&self.path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(KeyStoreError::from)
    }

    fn delete(&self) -> Result<(), KeyStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

fn write_new_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => File::open(dir)?.sync_all(),
        None => File::open(".")?.sync_all(),
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn warn_if_readable_by_others(file: &File) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = file.metadata() {
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!(
                mode = format_args!("{:o}", mode & 0o777),
                "master key file is accessible by other users"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_file: &File) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyId;

    #[test]
    fn store_load_replace_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::new(dir.path().join("key"));
        assert!(store.load().unwrap().is_none());

        let first = MasterKey::generate(KeyId::FIRST).unwrap();
        store.store(&first).unwrap();
        assert!(store.load().unwrap().unwrap().ct_eq(&first));

        let second = MasterKey::generate(KeyId::FIRST.next().unwrap()).unwrap();
        store.store(&second).unwrap();
        assert!(store.load().unwrap().unwrap().ct_eq(&second));

        store.delete().unwrap();
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
        // No temporary files left behind.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::new(dir.path().join("key"));
        store
            .store(&MasterKey::generate(KeyId::FIRST).unwrap())
            .unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn garbage_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        fs::write(&path, b"not a key").unwrap();
        assert!(matches!(
            FileKeyStore::new(&path).load(),
            Err(KeyStoreError::Malformed)
        ));
        fs::write(&path, vec![0u8; 4096]).unwrap();
        assert!(matches!(
            FileKeyStore::new(&path).load(),
            Err(KeyStoreError::Malformed)
        ));
    }
}
