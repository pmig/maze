use std::fmt;
use std::io;

use mazemail_types::BlobId;
use zeroize::Zeroize;

use crate::{MasterKey, SubKey, label};

/// Computes content-addressed blob ids: keyed BLAKE3 over the raw (uncompressed) bytes,
/// keyed with the `blob-id` subkey.
///
/// Identical messages still get the same id, so the store deduplicates them, but without
/// the key nobody can confirm that a known message is in the store by hashing it.
pub struct BlobIdHasher {
    key: SubKey,
}

impl BlobIdHasher {
    /// Creates a hasher keyed with the `blob-id` subkey of `master`.
    pub fn new(master: &MasterKey) -> Self {
        Self {
            key: master.derive(label::BLOB_ID),
        }
    }

    /// The blob id of `content`.
    pub fn hash(&self, content: &[u8]) -> BlobId {
        BlobId(*blake3::keyed_hash(self.key.expose_secret(), content).as_bytes())
    }

    /// An incremental hasher for content that arrives in pieces (it implements
    /// [`io::Write`], so `io::copy` works). Gives the same id as [`BlobIdHasher::hash`]
    /// over the concatenated input.
    pub fn writer(&self) -> BlobIdWriter {
        BlobIdWriter {
            hasher: blake3::Hasher::new_keyed(self.key.expose_secret()),
        }
    }
}

impl fmt::Debug for BlobIdHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobIdHasher")
            .field("key_id", &self.key.key_id())
            .finish_non_exhaustive()
    }
}

/// Incremental blob id computation, created by [`BlobIdHasher::writer`].
pub struct BlobIdWriter {
    hasher: blake3::Hasher,
}

impl BlobIdWriter {
    /// Feeds more content.
    pub fn update(&mut self, content: &[u8]) -> &mut Self {
        self.hasher.update(content);
        self
    }

    /// The blob id of everything fed so far.
    pub fn finalize(&self) -> BlobId {
        BlobId(*self.hasher.finalize().as_bytes())
    }
}

impl io::Write for BlobIdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for BlobIdWriter {
    fn drop(&mut self) {
        // The hasher state contains the key.
        self.hasher.zeroize();
    }
}

impl fmt::Debug for BlobIdWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobIdWriter").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use proptest::prelude::*;

    use super::*;
    use crate::{KEY_LEN, KeyId};

    fn hasher(seed: u8) -> BlobIdHasher {
        BlobIdHasher::new(&MasterKey::from_bytes(KeyId::FIRST, [seed; KEY_LEN]))
    }

    #[test]
    fn id_is_keyed_and_deterministic() {
        let content = b"From: a@example.com\r\n\r\nhello";
        assert_eq!(hasher(1).hash(content), hasher(1).hash(content));
        assert_ne!(hasher(1).hash(content), hasher(2).hash(content));
        assert_ne!(
            hasher(1).hash(content),
            BlobId(*blake3::hash(content).as_bytes())
        );
        assert_ne!(hasher(1).hash(content), hasher(1).hash(b"other"));
    }

    #[test]
    fn id_uses_the_blob_id_subkey() {
        let master = MasterKey::from_bytes(KeyId::FIRST, [1; KEY_LEN]);
        let key = master.derive(label::BLOB_ID);
        let expected = blake3::keyed_hash(key.expose_secret(), b"x");
        assert_eq!(
            BlobIdHasher::new(&master).hash(b"x").0,
            *expected.as_bytes()
        );
    }

    proptest! {
        #[test]
        fn incremental_matches_one_shot(
            content in proptest::collection::vec(any::<u8>(), 0..20_000),
            split in any::<prop::sample::Index>(),
        ) {
            let hasher = hasher(9);
            let at = split.index(content.len() + 1);
            let mut writer = hasher.writer();
            writer.write_all(&content[..at]).unwrap();
            writer.update(&content[at..]);
            prop_assert_eq!(writer.finalize(), hasher.hash(&content));
        }
    }
}
