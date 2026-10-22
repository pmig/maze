//! Where the store keeps its files under the data dir (PLAN §4.17 "Data dir layout").

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use mazemail_types::BlobId;

use crate::{Error, Result};

/// File name of the main database inside `mail/`.
pub const DATABASE_FILE: &str = "mazemail.db";
/// File name of the send log inside `mail/`.
pub const SENDLOG_FILE: &str = "sendlog.db";

/// The paths of everything the store owns, derived from the data dir.
///
/// The data dir itself is discovered by the `settings` crate; the store only receives
/// it. Everything here lives in `<data_dir>/mail/`, which is created with mode `0700`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataLayout {
    mail: PathBuf,
}

impl DataLayout {
    /// The layout for `data_dir` (nothing is created yet).
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            mail: data_dir.as_ref().join("mail"),
        }
    }

    /// `<data_dir>/mail/`, the encrypted area.
    pub fn mail_dir(&self) -> &Path {
        &self.mail
    }

    /// `mail/mazemail.db`.
    pub fn database(&self) -> PathBuf {
        self.mail.join(DATABASE_FILE)
    }

    /// `mail/sendlog.db`, never restored by a rollback.
    pub fn sendlog(&self) -> PathBuf {
        self.mail.join(SENDLOG_FILE)
    }

    /// `mail/blobs/`.
    pub fn blobs_dir(&self) -> PathBuf {
        self.mail.join("blobs")
    }

    /// `mail/cache/`, droppable at any time.
    pub fn cache_dir(&self) -> PathBuf {
        self.mail.join("cache")
    }

    /// `mail/backups/`, pre-migration database copies.
    pub fn backups_dir(&self) -> PathBuf {
        self.mail.join("backups")
    }

    /// `mail/tmp/`, staging area for atomic blob writes (same filesystem as `blobs/`).
    pub fn tmp_dir(&self) -> PathBuf {
        self.mail.join("tmp")
    }

    /// `mail/blobs/<2 hex>/<2 hex>/<blob id hex>.mzb`.
    pub fn blob_path(&self, id: &BlobId) -> PathBuf {
        sharded(&self.blobs_dir(), &id.to_hex())
    }

    /// Whether any encrypted data exists (the input to `vault::VaultState::decide`).
    pub fn data_exists(&self) -> bool {
        self.database().exists() || self.blobs_dir().exists()
    }

    /// Creates `mail/` (mode `0700`) and its subdirectories.
    pub fn create_dirs(&self) -> Result<()> {
        create_private_dir(&self.mail)?;
        for dir in [
            self.blobs_dir(),
            self.cache_dir(),
            self.backups_dir(),
            self.tmp_dir(),
        ] {
            create_private_dir(&dir)?;
        }
        Ok(())
    }
}

/// `<root>/<hex[0..2]>/<hex[2..4]>/<hex>.mzb`.
pub(crate) fn sharded(root: &Path, hex: &str) -> PathBuf {
    let (a, b) = (
        &hex[..2.min(hex.len())],
        &hex[2.min(hex.len())..4.min(hex.len())],
    );
    root.join(a).join(b).join(format!("{hex}.mzb"))
}

/// Creates `dir` and its parents; the leaf gets mode `0700` on Unix, also if it already
/// existed with wider permissions.
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(Error::io("creating a store directory"))?;
    restrict_permissions(dir).map_err(Error::io("restricting a store directory to 0700"))
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_paths_are_sharded() {
        let layout = DataLayout::new("/data");
        let id = BlobId([0xab; 32]);
        let path = layout.blob_path(&id);
        assert_eq!(
            path,
            PathBuf::from(format!("/data/mail/blobs/ab/ab/{}.mzb", id.to_hex()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn mail_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(tmp.path());
        assert!(!layout.data_exists());
        layout.create_dirs().unwrap();
        for dir in [layout.mail_dir().to_owned(), layout.blobs_dir()] {
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
        assert!(layout.data_exists());
    }
}
