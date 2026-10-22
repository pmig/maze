//! Where the master key lives between launches.

use std::fmt;

use crate::{KeyStoreError, MasterKey};

mod file;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod memory;

pub use file::FileKeyStore;
#[cfg(target_os = "linux")]
pub use linux::SecretServiceStore;
#[cfg(target_os = "macos")]
pub use macos::KeychainStore;
pub use memory::MemoryKeyStore;

/// Account name of the master key item in the platform key store.
pub const KEY_ACCOUNT: &str = "master-key";

/// Persistent storage for the [`MasterKey`].
///
/// Implementations store [`MasterKey::to_stored_bytes`] and must never log or print
/// them. Calls may block (keychain IPC), so call them off the UI thread.
pub trait KeyStore: fmt::Debug + Send + Sync {
    /// Reads the master key; `Ok(None)` if there is none.
    fn load(&self) -> Result<Option<MasterKey>, KeyStoreError>;

    /// Stores `key`, replacing any existing key.
    ///
    /// Replacing a key makes everything sealed with the old one unreadable; [`Vault`]
    /// only calls this when no key exists.
    ///
    /// [`Vault`]: crate::Vault
    fn store(&self, key: &MasterKey) -> Result<(), KeyStoreError>;

    /// Deletes the master key. Deleting a key that doesn't exist succeeds.
    fn delete(&self) -> Result<(), KeyStoreError>;
}

/// Which build the key belongs to. Dev builds use their own keychain item, like their
/// own data directory, so a dev build never touches the real key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// Signed release builds (stable, preview, nightly).
    Release,
    /// Local development builds.
    Dev,
}

impl Flavor {
    /// The keychain service name of the master key item.
    pub const fn service(self) -> &'static str {
        match self {
            Self::Release => "dev.mazemail.maze.storage",
            Self::Dev => "dev.mazemail.maze-dev.storage",
        }
    }
}

/// Label shown for the item in Keychain Access and Secret Service UIs.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(crate) const ITEM_LABEL: &str = "Maze storage key";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_names_follow_the_bundle_id() {
        assert_eq!(Flavor::Release.service(), "dev.mazemail.maze.storage");
        assert_eq!(Flavor::Dev.service(), "dev.mazemail.maze-dev.storage");
    }
}
