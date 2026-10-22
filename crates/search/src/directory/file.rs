//! Reading and writing one encrypted index file.
//!
//! Plaintext layout of every file, before it is cut into `MZI1` blocks:
//!
//! ```text
//! offset  size  field
//! ------  ----  ---------------------------------------------------------------
//!      0    32  name tag: BLAKE3 derive_key("…search index file name v1", path)
//!     32     …  the bytes Tantivy wrote
//! ```
//!
//! The vault block format binds blocks to their file (nonce prefix, header and index)
//! but not to the file's *name*, so on its own a segment file could be swapped for
//! another one sealed with the same key. The name tag sits inside the authenticated
//! plaintext, and the reader checks it against the name it was asked to open.

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tantivy::HasLen;
use tantivy::directory::{AntiCallToken, FileHandle, OwnedBytes, TerminatingWrite};
use vault::block::{FileHeader, HEADER_LEN};
use vault::{BlockCipher, TAG_LEN};

use super::cache::{BlockCache, BlockKey};

/// Length of the name tag at the start of every file's plaintext.
pub(crate) const NAME_TAG_LEN: usize = 32;

/// Reads touching more blocks than this are decrypted in one pass and cached as a whole
/// range rather than block by block: Tantivy re-reads the same large slices (field
/// norms, fast field columns) on every query.
const LARGE_READ_BLOCKS: u64 = 16;

const NAME_TAG_CONTEXT: &str = "mazemail 2026-10 search index file name v1";

/// The name tag for the file at `path` (relative to the index directory).
pub(crate) fn name_tag(path: &Path) -> [u8; NAME_TAG_LEN] {
    let mut hasher = blake3::Hasher::new_derive_key(NAME_TAG_CONTEXT);
    hasher.update(path.as_os_str().as_encoded_bytes());
    *hasher.finalize().as_bytes()
}

pub(crate) fn invalid_data(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

fn wrong_name() -> io::Error {
    invalid_data("index file does not belong under this name (swapped or renamed file)")
}

/// Reads exactly `buf.len()` bytes at `offset` without moving a shared cursor.
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            match file.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => done += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

/// A read-only, random-access view of one encrypted file.
pub(crate) struct EncryptedFile {
    path: PathBuf,
    file: File,
    header: FileHeader,
    file_len: u64,
    block_count: u64,
    /// Logical length: plaintext minus the name tag.
    len: usize,
    cipher: BlockCipher,
    cache: Arc<BlockCache>,
}

impl EncryptedFile {
    /// Opens `file`, stored under the relative name `path`, and checks its header and
    /// name tag.
    pub(crate) fn open(
        path: &Path,
        file: File,
        cipher: BlockCipher,
        cache: Arc<BlockCache>,
    ) -> io::Result<Self> {
        let file_len = file.metadata()?.len();
        let mut header_bytes = [0u8; HEADER_LEN];
        if file_len < HEADER_LEN as u64 {
            return Err(invalid_data(vault::Error::Truncated));
        }
        read_exact_at(&file, &mut header_bytes, 0)?;
        let header = FileHeader::parse(&header_bytes).map_err(invalid_data)?;
        if header.key_id() != cipher.key_id() {
            return Err(invalid_data(vault::Error::KeyIdMismatch {
                found: header.key_id().get(),
                expected: cipher.key_id(),
            }));
        }
        let block_count = header.block_count(file_len).map_err(invalid_data)?;
        let plaintext_len = header.plaintext_len(file_len).map_err(invalid_data)?;
        let len = plaintext_len
            .checked_sub(NAME_TAG_LEN as u64)
            .and_then(|len| usize::try_from(len).ok())
            .ok_or_else(wrong_name)?;
        let this = Self {
            path: path.to_owned(),
            file,
            header,
            file_len,
            block_count,
            len,
            cipher,
            cache,
        };
        let first = this.block(0)?;
        if first.get(..NAME_TAG_LEN) != Some(&name_tag(path)[..]) {
            return Err(wrong_name());
        }
        Ok(this)
    }

    fn block_size(&self) -> u64 {
        u64::from(self.header.block_size())
    }

    /// Byte range of sealed block `index` within the file.
    fn sealed_range(&self, index: u64) -> Range<u64> {
        let start = self.header.block_offset(index);
        let end = if index + 1 == self.block_count {
            self.file_len
        } else {
            start + self.header.sealed_block_len()
        };
        start..end
    }

    /// Decrypts block `index` from disk into `buf` (which is overwritten).
    fn decrypt_block(&self, index: u64, buf: &mut Vec<u8>) -> io::Result<()> {
        let range = self.sealed_range(index);
        let len = usize::try_from(range.end - range.start).map_err(invalid_data)?;
        buf.clear();
        buf.resize(len, 0);
        read_exact_at(&self.file, buf, range.start)?;
        self.cipher
            .open_block_in_place(&self.header, index, index + 1 == self.block_count, buf)
            .map_err(invalid_data)
    }

    /// Block `index`, from the cache or decrypted.
    fn block(&self, index: u64) -> io::Result<Arc<[u8]>> {
        let key = BlockKey::Block {
            file: *self.header.nonce_prefix(),
            index,
        };
        self.cache.get_or_load(key, || {
            let mut buf = Vec::with_capacity(self.header.sealed_block_len() as usize);
            self.decrypt_block(index, &mut buf)?;
            Ok(Arc::from(buf))
        })
    }

    /// Reads a large span straight from disk, decrypting block by block, without the
    /// cache.
    fn read_uncached(&self, first: u64, last: u64, physical: Range<u64>) -> io::Result<Vec<u8>> {
        let block_size = self.block_size();
        let out_len = usize::try_from(physical.end - physical.start).map_err(invalid_data)?;
        let mut out = Vec::with_capacity(out_len);
        let start = self.sealed_range(first).start;
        let end = self.sealed_range(last).end;
        let mut sealed = vec![0u8; usize::try_from(end - start).map_err(invalid_data)?];
        read_exact_at(&self.file, &mut sealed, start)?;
        let sealed_len = self.header.sealed_block_len() as usize;
        let mut scratch = Vec::with_capacity(sealed_len);
        for (i, chunk) in sealed.chunks(sealed_len).enumerate() {
            let index = first + i as u64;
            scratch.clear();
            scratch.extend_from_slice(chunk);
            self.cipher
                .open_block_in_place(
                    &self.header,
                    index,
                    index + 1 == self.block_count,
                    &mut scratch,
                )
                .map_err(invalid_data)?;
            let block_start = index * block_size;
            let from = physical.start.max(block_start) - block_start;
            let to = physical.end.min(block_start + scratch.len() as u64) - block_start;
            out.extend_from_slice(&scratch[from as usize..to as usize]);
        }
        Ok(out)
    }
}

impl FileHandle for EncryptedFile {
    fn read_bytes(&self, range: Range<usize>) -> io::Result<OwnedBytes> {
        if range.start > range.end || range.end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read past the end of an index file",
            ));
        }
        if range.is_empty() {
            return Ok(OwnedBytes::empty());
        }
        let block_size = self.block_size();
        let physical = (range.start + NAME_TAG_LEN) as u64..(range.end + NAME_TAG_LEN) as u64;
        let first = physical.start / block_size;
        let last = (physical.end - 1) / block_size;
        if first == last {
            let block = self.block(first)?;
            let base = first * block_size;
            let slice = (physical.start - base) as usize..(physical.end - base) as usize;
            if slice.end > block.len() {
                return Err(invalid_data(vault::Error::Truncated));
            }
            return Ok(OwnedBytes::new(block).slice(slice));
        }
        if last - first + 1 > LARGE_READ_BLOCKS {
            let len = physical.end - physical.start;
            // Bigger ranges (merges reading whole segments) bypass the cache.
            if len > self.cache.max_entry() {
                return Ok(OwnedBytes::new(self.read_uncached(first, last, physical)?));
            }
            let key = BlockKey::Range {
                file: *self.header.nonce_prefix(),
                start: physical.start,
                end: physical.end,
            };
            let bytes = self.cache.get_or_load(key, || {
                Ok(Arc::from(self.read_uncached(
                    first,
                    last,
                    physical.clone(),
                )?))
            })?;
            return Ok(OwnedBytes::new(bytes));
        }
        let mut out = Vec::with_capacity(range.len());
        for index in first..=last {
            let block = self.block(index)?;
            let base = index * block_size;
            let from = (physical.start.max(base) - base) as usize;
            let to = (physical.end.min(base + block.len() as u64) - base) as usize;
            out.extend_from_slice(
                block
                    .get(from..to)
                    .ok_or_else(|| invalid_data(vault::Error::Truncated))?,
            );
        }
        Ok(OwnedBytes::new(out))
    }
}

impl HasLen for EncryptedFile {
    fn len(&self) -> usize {
        self.len
    }
}

impl fmt::Debug for EncryptedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedFile")
            .field("path", &self.path)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// Writes one encrypted file block by block. Tantivy wraps it in a `BufWriter`.
pub(crate) struct EncryptedWriter {
    file: File,
    cipher: BlockCipher,
    header: FileHeader,
    block_size: usize,
    /// Plaintext of the current block; becomes the sealed block in place.
    buf: Vec<u8>,
    next_block: u64,
    terminated: bool,
}

impl EncryptedWriter {
    /// Starts a new file under the relative name `path`.
    pub(crate) fn new(
        path: &Path,
        mut file: File,
        cipher: BlockCipher,
        block_size: u32,
    ) -> io::Result<Self> {
        let header = cipher
            .new_file_header(block_size)
            .map_err(io::Error::from)?;
        file.write_all(&header.to_bytes())?;
        let block_size = block_size as usize;
        let mut buf = Vec::with_capacity(block_size + TAG_LEN);
        buf.extend_from_slice(&name_tag(path));
        Ok(Self {
            file,
            cipher,
            header,
            block_size,
            buf,
            next_block: 0,
            terminated: false,
        })
    }

    fn seal_and_write(&mut self, last: bool) -> io::Result<()> {
        self.cipher
            .seal_block_in_place(&self.header, self.next_block, last, &mut self.buf)
            .map_err(io::Error::from)?;
        self.file.write_all(&self.buf)?;
        self.buf.clear();
        self.next_block += 1;
        Ok(())
    }
}

impl Write for EncryptedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.terminated {
            return Err(io::Error::other("write after terminate"));
        }
        let mut rest = data;
        while !rest.is_empty() {
            // A full block is sealed only once more data arrives, so the final block
            // (sealed with `last = true` in `terminate`) is never empty unless the whole
            // file is.
            if self.buf.len() == self.block_size {
                self.seal_and_write(false)?;
            }
            let take = (self.block_size - self.buf.len()).min(rest.len());
            self.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Partial blocks can't be sealed early; data becomes durable in `terminate`.
        Ok(())
    }
}

impl TerminatingWrite for EncryptedWriter {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        if self.terminated {
            return Ok(());
        }
        self.seal_and_write(true)?;
        self.terminated = true;
        self.file.flush()?;
        self.file.sync_data()
    }
}
