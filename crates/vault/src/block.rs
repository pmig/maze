//! The `MZI1` block format behind the encrypted Tantivy `Directory`.
//!
//! Index files are split into fixed-size plaintext blocks (16 KiB by default) that are
//! sealed independently, so a read of any byte range decrypts only the blocks it touches.
//! Tantivy files are write-once, so blocks are never rewritten in place and a block's
//! nonce is never reused. Small files such as `meta.json` use the same format with a
//! single block ([`BlockCipher::seal_file`]).
//!
//! # Byte layout
//!
//! ```text
//! offset  size  field
//! ------  ----  -----------------------------------------------------------------
//!      0     4  magic "MZI1"
//!      4     1  format version, 0x01
//!      5     4  key_id: master key generation, u32 big-endian, never 0
//!      9     4  block size B: plaintext bytes per block, u32 big-endian
//!     13    16  nonce prefix, random per file
//!     29     …  blocks, each = ciphertext ‖ 16-byte Poly1305 tag
//! ```
//!
//! - Every block but the last holds exactly B plaintext bytes (B + 16 sealed); the last
//!   holds 0 to B. A file always has at least one block, so an empty file is header plus
//!   one 16-byte tag.
//! - Block `i` (0-based) of a file starting at byte 29 begins at `29 + i * (B + 16)`.
//! - Nonce of block `i` (24 bytes): `nonce prefix (16) ‖ i as u64 big-endian (8)`.
//! - Associated data of block `i`: the 29 header bytes ‖ `i` as u64 big-endian ‖ last
//!   flag (`0x01` for the final block, `0x00` otherwise).
//!
//! Binding the header means a changed key id, block size or nonce prefix fails to open;
//! binding the index means blocks can't be moved within a file; the last flag means a file
//! cut at a block boundary fails to open instead of reading as a shorter file. Blocks
//! from another file fail because the nonce prefix and header differ.

use std::fmt;

use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{KeyInit, Tag, XChaCha20Poly1305, XNonce};

use crate::{Error, KeyId, MasterKey, TAG_LEN, label};

/// Magic bytes at the start of every encrypted index file.
pub const MAGIC: [u8; 4] = *b"MZI1";
/// The format version this build writes and reads.
pub const VERSION: u8 = 1;
/// Length of the random per-file nonce prefix.
pub const NONCE_PREFIX_LEN: usize = 16;
/// Length of the file header in front of the first block.
pub const HEADER_LEN: usize = 4 + 1 + 4 + 4 + NONCE_PREFIX_LEN;
/// Default plaintext block size (16 KiB).
pub const DEFAULT_BLOCK_SIZE: u32 = 16 * 1024;
/// Smallest accepted block size.
pub const MIN_BLOCK_SIZE: u32 = 512;
/// Largest accepted block size; bounds the memory a crafted header can demand.
pub const MAX_BLOCK_SIZE: u32 = 1024 * 1024;

const AD_LEN: usize = HEADER_LEN + 8 + 1;

/// The header of an encrypted index file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeader {
    key_id: KeyId,
    block_size: u32,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
}

impl FileHeader {
    /// Parses the header at the start of an encrypted index file.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() >= MAGIC.len() && bytes[..MAGIC.len()] != MAGIC {
            return Err(Error::BadMagic);
        }
        if bytes.len() < HEADER_LEN {
            return Err(Error::Truncated);
        }
        if bytes[4] != VERSION {
            return Err(Error::UnsupportedVersion(bytes[4]));
        }
        let key_id = KeyId::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]])
            .ok_or(Error::InvalidHeader("key id 0"))?;
        let block_size = u32::from_be_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]);
        check_block_size(block_size)?;
        let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
        nonce_prefix.copy_from_slice(&bytes[13..HEADER_LEN]);
        Ok(Self {
            key_id,
            block_size,
            nonce_prefix,
        })
    }

    /// The serialized header.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5..9].copy_from_slice(&self.key_id.to_be_bytes());
        out[9..13].copy_from_slice(&self.block_size.to_be_bytes());
        out[13..].copy_from_slice(&self.nonce_prefix);
        out
    }

    /// The master key generation the file was sealed with.
    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// Plaintext bytes per block.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// The random per-file nonce prefix.
    pub fn nonce_prefix(&self) -> &[u8; NONCE_PREFIX_LEN] {
        &self.nonce_prefix
    }

    /// Bytes per sealed block (block size plus tag).
    pub fn sealed_block_len(&self) -> u64 {
        u64::from(self.block_size) + TAG_LEN as u64
    }

    /// Byte offset of block `index` within the file.
    pub fn block_offset(&self, index: u64) -> u64 {
        HEADER_LEN as u64 + index.saturating_mul(self.sealed_block_len())
    }

    /// Number of blocks in a well-formed file of `file_len` bytes (header included).
    pub fn block_count(&self, file_len: u64) -> Result<u64, Error> {
        let body = file_len
            .checked_sub(HEADER_LEN as u64)
            .ok_or(Error::Truncated)?;
        let count = body.div_ceil(self.sealed_block_len()).max(1);
        let last = body - (count - 1) * self.sealed_block_len();
        if last < TAG_LEN as u64 {
            return Err(Error::Truncated);
        }
        Ok(count)
    }

    /// Plaintext length of a well-formed file of `file_len` bytes (header included).
    pub fn plaintext_len(&self, file_len: u64) -> Result<u64, Error> {
        let count = self.block_count(file_len)?;
        Ok(file_len - HEADER_LEN as u64 - count * TAG_LEN as u64)
    }

    /// Size of a file holding `plaintext_len` bytes, header included.
    pub fn file_len(&self, plaintext_len: u64) -> u64 {
        let count = plaintext_len.div_ceil(u64::from(self.block_size)).max(1);
        (HEADER_LEN as u64)
            .saturating_add(plaintext_len)
            .saturating_add(count.saturating_mul(TAG_LEN as u64))
    }

    fn nonce(&self, index: u64) -> XNonce {
        let mut nonce = [0u8; 24];
        nonce[..NONCE_PREFIX_LEN].copy_from_slice(&self.nonce_prefix);
        nonce[NONCE_PREFIX_LEN..].copy_from_slice(&index.to_be_bytes());
        XNonce::from(nonce)
    }

    fn associated_data(&self, index: u64, last: bool) -> [u8; AD_LEN] {
        let mut ad = [0u8; AD_LEN];
        ad[..HEADER_LEN].copy_from_slice(&self.to_bytes());
        ad[HEADER_LEN..HEADER_LEN + 8].copy_from_slice(&index.to_be_bytes());
        ad[AD_LEN - 1] = u8::from(last);
        ad
    }
}

fn check_block_size(block_size: u32) -> Result<(), Error> {
    if (MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size) {
        Ok(())
    } else {
        Err(Error::InvalidHeader("block size out of range"))
    }
}

/// Seals and opens `MZI1` index blocks with the `index` subkey of one key generation.
///
/// Cheap to clone; the key is wiped when the last clone is dropped.
#[derive(Clone)]
pub struct BlockCipher {
    aead: XChaCha20Poly1305,
    key_id: KeyId,
}

impl BlockCipher {
    /// Creates a cipher keyed with the `index` subkey of `master`.
    pub fn new(master: &MasterKey) -> Self {
        let key = master.derive(label::INDEX);
        Self {
            aead: XChaCha20Poly1305::new(key.expose_secret().into()),
            key_id: key.key_id(),
        }
    }

    /// The master key generation this cipher seals with and can open.
    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// A header for a new file: this cipher's key id, `block_size`
    /// ([`DEFAULT_BLOCK_SIZE`] unless tuned) and a fresh random nonce prefix.
    pub fn new_file_header(&self, block_size: u32) -> Result<FileHeader, Error> {
        check_block_size(block_size)?;
        let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
        getrandom::fill(&mut nonce_prefix)?;
        Ok(FileHeader {
            key_id: self.key_id,
            block_size,
            nonce_prefix,
        })
    }

    fn check_key_id(&self, header: &FileHeader) -> Result<(), Error> {
        if header.key_id == self.key_id {
            Ok(())
        } else {
            Err(Error::KeyIdMismatch {
                found: header.key_id.get(),
                expected: self.key_id,
            })
        }
    }

    /// Seals block `index` of the file described by `header`.
    ///
    /// `last` marks the file's final block. A non-final block must hold exactly
    /// `header.block_size()` bytes; the final one at most that many.
    pub fn seal_block(
        &self,
        header: &FileHeader,
        index: u64,
        last: bool,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let mut buf = Vec::with_capacity(plaintext.len() + TAG_LEN);
        buf.extend_from_slice(plaintext);
        self.seal_block_in_place(header, index, last, &mut buf)?;
        Ok(buf)
    }

    /// Like [`BlockCipher::seal_block`], encrypting `buf` in place and appending the tag.
    pub fn seal_block_in_place(
        &self,
        header: &FileHeader,
        index: u64,
        last: bool,
        buf: &mut Vec<u8>,
    ) -> Result<(), Error> {
        self.check_key_id(header)?;
        let block_size = header.block_size as usize;
        if buf.len() > block_size || (!last && buf.len() != block_size) {
            return Err(Error::InvalidBlock(
                "plaintext length doesn't fit the block size",
            ));
        }
        let ad = header.associated_data(index, last);
        let tag = self
            .aead
            .encrypt_inout_detached(&header.nonce(index), &ad, buf.as_mut_slice().into())
            .map_err(|_| Error::TooLarge)?;
        buf.extend_from_slice(&tag);
        Ok(())
    }

    /// Opens block `index` of the file described by `header`; `last` must say whether it
    /// is the file's final block (see [`FileHeader::block_count`]).
    pub fn open_block(
        &self,
        header: &FileHeader,
        index: u64,
        last: bool,
        sealed: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let mut buf = sealed.to_vec();
        self.open_block_in_place(header, index, last, &mut buf)?;
        Ok(buf)
    }

    /// Like [`BlockCipher::open_block`], decrypting `buf` in place and removing the tag.
    /// On failure the contents of `buf` are unspecified.
    pub fn open_block_in_place(
        &self,
        header: &FileHeader,
        index: u64,
        last: bool,
        buf: &mut Vec<u8>,
    ) -> Result<(), Error> {
        self.check_key_id(header)?;
        let sealed_len = header.block_size as usize + TAG_LEN;
        if buf.len() < TAG_LEN {
            return Err(Error::Truncated);
        }
        if buf.len() > sealed_len || (!last && buf.len() != sealed_len) {
            return Err(Error::InvalidBlock(
                "sealed length doesn't fit the block size",
            ));
        }
        let ciphertext_len = buf.len() - TAG_LEN;
        let tag = Tag::try_from(&buf[ciphertext_len..]).map_err(|_| Error::Truncated)?;
        let ad = header.associated_data(index, last);
        self.aead
            .decrypt_inout_detached(
                &header.nonce(index),
                &ad,
                (&mut buf[..ciphertext_len]).into(),
                &tag,
            )
            .map_err(|_| Error::Decrypt)?;
        buf.truncate(ciphertext_len);
        Ok(())
    }

    /// Seals a whole file in one go: header plus blocks. Meant for small files such as
    /// `meta.json` (one block when it fits), and for tests.
    pub fn seal_file(&self, plaintext: &[u8], block_size: u32) -> Result<Vec<u8>, Error> {
        let header = self.new_file_header(block_size)?;
        let mut out = Vec::with_capacity(header.file_len(plaintext.len() as u64) as usize);
        out.extend_from_slice(&header.to_bytes());
        let block_size = block_size as usize;
        let count = plaintext.len().div_ceil(block_size).max(1);
        for index in 0..count {
            let start = index * block_size;
            let end = (start + block_size).min(plaintext.len());
            let sealed = self.seal_block(
                &header,
                index as u64,
                index + 1 == count,
                &plaintext[start..end],
            )?;
            out.extend_from_slice(&sealed);
        }
        Ok(out)
    }

    /// Opens a whole file written by [`BlockCipher::seal_file`] or block by block.
    pub fn open_file(&self, file: &[u8]) -> Result<Vec<u8>, Error> {
        let header = FileHeader::parse(file)?;
        self.check_key_id(&header)?;
        let count = header.block_count(file.len() as u64)?;
        let mut out = Vec::with_capacity(header.plaintext_len(file.len() as u64)? as usize);
        let sealed_len = header.sealed_block_len() as usize;
        for (index, sealed) in file[HEADER_LEN..].chunks(sealed_len).enumerate() {
            let index = index as u64;
            out.extend(self.open_block(&header, index, index + 1 == count, sealed)?);
        }
        Ok(out)
    }
}

impl fmt::Debug for BlockCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockCipher")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::*;
    use crate::KEY_LEN;

    fn cipher() -> BlockCipher {
        BlockCipher::new(&MasterKey::from_bytes(KeyId::FIRST, [5; KEY_LEN]))
    }

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn header_round_trip_and_layout() {
        let header = cipher().new_file_header(DEFAULT_BLOCK_SIZE).unwrap();
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), 29);
        assert_eq!(&bytes[..4], b"MZI1");
        assert_eq!(bytes[4], 1);
        assert_eq!(&bytes[5..9], &[0, 0, 0, 1]);
        assert_eq!(&bytes[9..13], &16384u32.to_be_bytes());
        assert_eq!(FileHeader::parse(&bytes).unwrap(), header);
        assert_eq!(header.block_offset(2), 29 + 2 * (16384 + 16));
    }

    #[test]
    fn header_rejects_bad_fields() {
        let bytes = cipher()
            .new_file_header(DEFAULT_BLOCK_SIZE)
            .unwrap()
            .to_bytes();
        let mut bad = bytes;
        bad[0] = b'X';
        assert!(matches!(FileHeader::parse(&bad), Err(Error::BadMagic)));
        let mut bad = bytes;
        bad[4] = 9;
        assert!(matches!(
            FileHeader::parse(&bad),
            Err(Error::UnsupportedVersion(9))
        ));
        let mut bad = bytes;
        bad[9..13].copy_from_slice(&(MAX_BLOCK_SIZE + 1).to_be_bytes());
        assert!(matches!(
            FileHeader::parse(&bad),
            Err(Error::InvalidHeader(_))
        ));
        assert!(matches!(
            FileHeader::parse(&bytes[..20]),
            Err(Error::Truncated)
        ));
        assert!(cipher().new_file_header(MIN_BLOCK_SIZE - 1).is_err());
    }

    #[test]
    fn blocks_round_trip_and_bind_index_and_position() {
        let cipher = cipher();
        let header = cipher.new_file_header(1024).unwrap();
        let block = data(1024);
        let sealed = cipher.seal_block(&header, 7, false, &block).unwrap();
        assert_eq!(sealed.len(), 1024 + TAG_LEN);
        assert_eq!(
            cipher.open_block(&header, 7, false, &sealed).unwrap(),
            block
        );
        // Different index, last flag or file header: rejected.
        assert!(cipher.open_block(&header, 8, false, &sealed).is_err());
        assert!(cipher.open_block(&header, 7, true, &sealed).is_err());
        let other = cipher.new_file_header(1024).unwrap();
        assert!(cipher.open_block(&other, 7, false, &sealed).is_err());
        // Another key.
        let stranger = BlockCipher::new(&MasterKey::from_bytes(KeyId::FIRST, [6; KEY_LEN]));
        assert!(stranger.open_block(&header, 7, false, &sealed).is_err());
    }

    #[test]
    fn every_tampered_byte_is_rejected() {
        let cipher = cipher();
        let file = cipher.seal_file(&data(1500), 512).unwrap();
        for position in 0..file.len() {
            let mut bad = file.clone();
            bad[position] ^= 0x04;
            assert!(cipher.open_file(&bad).is_err(), "byte {position}");
        }
    }

    #[test]
    fn truncation_and_trailing_data_are_rejected() {
        let cipher = cipher();
        let header = cipher.new_file_header(512).unwrap();
        for len in [0, 100, 512, 1024, 1300] {
            let file = cipher.seal_file(&data(len), 512).unwrap();
            for cut in [HEADER_LEN, HEADER_LEN + 528, file.len() - 1] {
                if cut < file.len() {
                    assert!(
                        cipher.open_file(&file[..cut]).is_err(),
                        "len {len} cut {cut}"
                    );
                }
            }
            let mut extra = file.clone();
            extra.push(0);
            assert!(cipher.open_file(&extra).is_err());
            assert_eq!(header.plaintext_len(file.len() as u64).unwrap(), len as u64);
        }
    }

    #[test]
    fn wrong_lengths_are_refused() {
        let cipher = cipher();
        let header = cipher.new_file_header(512).unwrap();
        assert!(matches!(
            cipher.seal_block(&header, 0, false, &data(511)),
            Err(Error::InvalidBlock(_))
        ));
        assert!(matches!(
            cipher.seal_block(&header, 0, true, &data(513)),
            Err(Error::InvalidBlock(_))
        ));
        let sealed = cipher.seal_block(&header, 0, true, &data(100)).unwrap();
        assert!(matches!(
            cipher.open_block(&header, 0, false, &sealed),
            Err(Error::InvalidBlock(_))
        ));
        assert!(matches!(
            cipher.open_block(&header, 0, true, &sealed[..10]),
            Err(Error::Truncated)
        ));
    }

    #[test]
    fn other_key_generation_is_reported() {
        let newer = BlockCipher::new(&MasterKey::from_bytes(KeyId::new(2).unwrap(), [5; KEY_LEN]));
        let file = newer.seal_file(b"{}", DEFAULT_BLOCK_SIZE).unwrap();
        assert!(matches!(
            cipher().open_file(&file),
            Err(Error::KeyIdMismatch { found: 2, .. })
        ));
    }

    #[test]
    fn nonce_prefixes_are_unique_per_file() {
        let cipher = cipher();
        let prefixes: HashSet<[u8; NONCE_PREFIX_LEN]> = (0..2000)
            .map(|_| {
                *cipher
                    .new_file_header(DEFAULT_BLOCK_SIZE)
                    .unwrap()
                    .nonce_prefix()
            })
            .collect();
        assert_eq!(prefixes.len(), 2000);
    }

    #[test]
    fn empty_file_is_one_empty_block() {
        let cipher = cipher();
        let file = cipher.seal_file(&[], DEFAULT_BLOCK_SIZE).unwrap();
        assert_eq!(file.len(), HEADER_LEN + TAG_LEN);
        assert_eq!(cipher.open_file(&file).unwrap(), b"");
        assert!(cipher.open_file(&file[..HEADER_LEN]).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn file_round_trip(len in 0usize..70_000, block_size in prop::sample::select(vec![512u32, 4096, DEFAULT_BLOCK_SIZE, 65536])) {
            let cipher = cipher();
            let plaintext = data(len);
            let file = cipher.seal_file(&plaintext, block_size).unwrap();
            let header = FileHeader::parse(&file).unwrap();
            prop_assert_eq!(file.len() as u64, header.file_len(len as u64));
            prop_assert_eq!(header.plaintext_len(file.len() as u64).unwrap(), len as u64);
            prop_assert_eq!(cipher.open_file(&file).unwrap(), plaintext);
        }

        #[test]
        fn random_access_reads_single_blocks(len in 1usize..40_000, pick in any::<prop::sample::Index>()) {
            let cipher = cipher();
            let plaintext = data(len);
            let file = cipher.seal_file(&plaintext, 4096).unwrap();
            let header = FileHeader::parse(&file).unwrap();
            let count = header.block_count(file.len() as u64).unwrap();
            let index = pick.index(count as usize) as u64;
            let start = header.block_offset(index) as usize;
            let end = (start + header.sealed_block_len() as usize).min(file.len());
            let block = cipher.open_block(&header, index, index + 1 == count, &file[start..end]).unwrap();
            let plain_start = index as usize * 4096;
            prop_assert_eq!(&block[..], &plaintext[plain_start..(plain_start + 4096).min(len)]);
        }
    }
}
