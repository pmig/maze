//! Encryption at rest for Maze (PLAN §4.17, ADR-012).
//!
//! Everything Maze writes under `<data_dir>/mail/` is encrypted with keys that come from
//! this crate:
//!
//! - One random 256-bit [`MasterKey`] lives in the OS keychain behind the [`KeyStore`]
//!   trait ([`KeychainStore`] on macOS, `SecretServiceStore` on Linux, [`MemoryKeyStore`]
//!   and [`FileKeyStore`] for tests and development).
//! - [`Vault`] loads or creates that key at startup and refuses to silently replace it
//!   when encrypted data already exists ([`VaultState`]).
//! - Purpose-bound subkeys are derived with HKDF-SHA256 ([`MasterKey::derive`], [`label`]):
//!   the SQLCipher raw key ([`Vault::sqlcipher_pragma_key`]), the blob key, the keyed
//!   BLAKE3 blob-id key and the index key.
//! - [`blob`] implements the `MZB1` format for raw messages, attachments and caches
//!   (XChaCha20-Poly1305 STREAM in 64 KiB chunks, one-shot and streaming).
//! - [`block`] implements the `MZI1` block format for the encrypted Tantivy directory.
//! - [`RecoveryKey`] renders the master key as a grouped Crockford base32 string that the
//!   user can keep in a password manager.
//!
//! Every encrypted header carries the [`KeyId`] (master key generation) so the key can be
//! rotated later (M7).
//!
//! # Order of operations
//!
//! Callers compress first, then encrypt: the store runs zstd over a raw message and hands
//! the compressed bytes to [`BlobCipher::seal`]. Ciphertext does not compress, so the
//! opposite order would waste the compression step.
//!
//! # Throughput
//!
//! Sealing and opening are bound by XChaCha20-Poly1305. The `blob::tests::throughput`
//! test (ignored by default) reports MB/s; run it with
//! `cargo nextest run -p vault --release --run-ignored only throughput`. The target is at
//! least 500 MB/s per core on Apple Silicon in a release build.

mod blob_id;
mod error;
mod key;
mod keystore;
mod recovery;
mod secret;
mod vault;

pub mod blob;
pub mod block;

pub use blob::{BlobCipher, OpenReader, SealWriter};
pub use blob_id::{BlobIdHasher, BlobIdWriter};
pub use block::BlockCipher;
pub use error::{Error, KeyStoreError, RecoveryKeyError};
pub use key::{KEY_LEN, KeyId, MasterKey, SubKey, label};
#[cfg(target_os = "macos")]
pub use keystore::KeychainStore;
#[cfg(target_os = "linux")]
pub use keystore::SecretServiceStore;
pub use keystore::{FileKeyStore, Flavor, KEY_ACCOUNT, KeyStore, MemoryKeyStore};
pub use recovery::RecoveryKey;
pub use secret::SecretString;
pub use vault::{Vault, VaultState};

/// Length of a Poly1305 authentication tag, appended to every sealed chunk and block.
pub const TAG_LEN: usize = 16;
