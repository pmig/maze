//! The `MZB1` sealed blob format for raw messages, attachments and caches.
//!
//! The store compresses first (zstd) and then seals the compressed bytes here; this module
//! never compresses. Blobs are sealed with the `blob` subkey and bound to their blob id,
//! so a blob file can't be swapped for another one.
//!
//! # Byte layout
//!
//! ```text
//! offset  size  field
//! ------  ----  -----------------------------------------------------------------
//!      0     4  magic "MZB1"
//!      4     1  format version, 0x01
//!      5     4  key_id: master key generation, u32 big-endian, never 0
//!      9    19  nonce prefix, random per blob
//!     28     …  chunks, each = ciphertext ‖ 16-byte Poly1305 tag
//! ```
//!
//! - The plaintext is cut into chunks of [`CHUNK_LEN`] (64 KiB). Every chunk but the last
//!   holds exactly 64 KiB; the last holds 0 to 64 KiB. An empty plaintext is one empty
//!   last chunk, so a sealed blob is at least 44 bytes ([`HEADER_LEN`] + [`TAG_LEN`]).
//!   An exact multiple of 64 KiB ends with a full last chunk, not an extra empty one.
//! - Each chunk is sealed with XChaCha20-Poly1305 under the 24-byte nonce
//!   `nonce prefix (19) ‖ chunk index (u32 big-endian, 4) ‖ last flag (1)`, where the
//!   flag is `0x01` for the final chunk and `0x00` otherwise. This is the STREAM
//!   construction (Hoang, Reyhanitabar, Rogaway, Vizár 2015), byte-for-byte what
//!   `aead_stream::StreamBE32` produces (a test checks this).
//! - The associated data of every chunk is the 28 header bytes followed by the 32-byte
//!   blob id.
//!
//! # What fails closed
//!
//! Opening returns an error, and never unauthenticated plaintext, when any byte is
//! changed (header bytes are part of the associated data), the key or blob id differs,
//! chunks are reordered or dropped, the stream is cut anywhere (the final chunk must carry
//! the last flag), or bytes are appended after the final chunk.
//!
//! The streaming [`OpenReader`] releases each chunk's plaintext once that chunk's tag has
//! verified, so a reader that sees an error after some data (for example a truncated
//! file) must discard everything it read from that blob.
//!
//! # Limits
//!
//! A blob holds at most 2^32 chunks (256 TiB); the random 152-bit nonce prefix makes a
//! collision between blobs negligible for any realistic number of blobs.

use std::fmt;
use std::io::{self, Read, Write};

use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{KeyInit, Tag, XChaCha20Poly1305, XNonce};
use mazemail_types::BlobId;

use crate::{Error, KeyId, MasterKey, TAG_LEN, label};

/// Magic bytes at the start of every sealed blob.
pub const MAGIC: [u8; 4] = *b"MZB1";
/// The format version this build writes and reads.
pub const VERSION: u8 = 1;
/// Length of the random per-blob nonce prefix.
pub const NONCE_PREFIX_LEN: usize = 19;
/// Length of the header in front of the first chunk.
pub const HEADER_LEN: usize = 4 + 1 + 4 + NONCE_PREFIX_LEN;
/// Plaintext bytes per chunk.
pub const CHUNK_LEN: usize = 64 * 1024;

const SEALED_CHUNK_LEN: usize = CHUNK_LEN + TAG_LEN;
const AD_LEN: usize = HEADER_LEN + 32;

/// The parsed header of a sealed blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobHeader {
    key_id: KeyId,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
}

impl BlobHeader {
    fn random(key_id: KeyId) -> Result<Self, Error> {
        let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
        getrandom::fill(&mut nonce_prefix)?;
        Ok(Self {
            key_id,
            nonce_prefix,
        })
    }

    /// Parses the header at the start of `sealed`, for example to learn which key
    /// generation a blob needs before opening it.
    pub fn parse(sealed: &[u8]) -> Result<Self, Error> {
        if sealed.len() >= MAGIC.len() && sealed[..MAGIC.len()] != MAGIC {
            return Err(Error::BadMagic);
        }
        if sealed.len() < HEADER_LEN {
            return Err(Error::Truncated);
        }
        if sealed[4] != VERSION {
            return Err(Error::UnsupportedVersion(sealed[4]));
        }
        let key_id = KeyId::from_be_bytes([sealed[5], sealed[6], sealed[7], sealed[8]])
            .ok_or(Error::InvalidHeader("key id 0"))?;
        let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
        nonce_prefix.copy_from_slice(&sealed[9..HEADER_LEN]);
        Ok(Self {
            key_id,
            nonce_prefix,
        })
    }

    /// The master key generation the blob was sealed with.
    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// The serialized header.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5..9].copy_from_slice(&self.key_id.to_be_bytes());
        out[9..].copy_from_slice(&self.nonce_prefix);
        out
    }
}

/// Size of the sealed form of `plaintext_len` bytes.
///
/// Saturates at `u64::MAX` for lengths far beyond the format's 256 TiB limit.
pub fn sealed_len(plaintext_len: u64) -> u64 {
    let chunks = plaintext_len.div_ceil(CHUNK_LEN as u64).max(1);
    (HEADER_LEN as u64)
        .saturating_add(plaintext_len)
        .saturating_add(chunks.saturating_mul(TAG_LEN as u64))
}

/// Size of the plaintext inside a sealed blob of `sealed_len` bytes, or `None` if no
/// well-formed blob has that size.
pub fn plaintext_len(sealed_len: u64) -> Option<u64> {
    let body = sealed_len.checked_sub(HEADER_LEN as u64)?;
    let chunks = body.div_ceil(SEALED_CHUNK_LEN as u64).max(1);
    let last = body - (chunks - 1) * SEALED_CHUNK_LEN as u64;
    if last < TAG_LEN as u64 {
        return None;
    }
    Some(body - chunks * TAG_LEN as u64)
}

/// Per-blob STREAM state: nonce prefix, associated data and the chunk counter.
struct StreamState {
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
    ad: [u8; AD_LEN],
    position: u32,
}

impl StreamState {
    fn new(header: &BlobHeader, blob_id: &BlobId) -> Self {
        let mut ad = [0u8; AD_LEN];
        ad[..HEADER_LEN].copy_from_slice(&header.to_bytes());
        ad[HEADER_LEN..].copy_from_slice(&blob_id.0);
        Self {
            nonce_prefix: header.nonce_prefix,
            ad,
            position: 0,
        }
    }

    /// The STREAM nonce for the current position, and advances the counter.
    fn next_nonce(&mut self, last: bool) -> Result<XNonce, Error> {
        // A non-final chunk may not use the maximum counter value, or no final chunk
        // could follow it.
        if !last && self.position == u32::MAX {
            return Err(Error::TooLarge);
        }
        let mut nonce = [0u8; 24];
        nonce[..NONCE_PREFIX_LEN].copy_from_slice(&self.nonce_prefix);
        nonce[NONCE_PREFIX_LEN..23].copy_from_slice(&self.position.to_be_bytes());
        nonce[23] = u8::from(last);
        if !last {
            self.position += 1;
        }
        Ok(XNonce::from(nonce))
    }

    /// Encrypts `chunk` in place and returns its tag.
    fn seal_chunk(
        &mut self,
        aead: &XChaCha20Poly1305,
        chunk: &mut [u8],
        last: bool,
    ) -> Result<Tag, Error> {
        let nonce = self.next_nonce(last)?;
        aead.encrypt_inout_detached(&nonce, &self.ad, chunk.into())
            .map_err(|_| Error::TooLarge)
    }

    /// Verifies `tag` and decrypts `chunk` in place. On failure `chunk` is unspecified.
    fn open_chunk(
        &mut self,
        aead: &XChaCha20Poly1305,
        chunk: &mut [u8],
        tag: &[u8],
        last: bool,
    ) -> Result<(), Error> {
        let tag = Tag::try_from(tag).map_err(|_| Error::Truncated)?;
        let nonce = self.next_nonce(last)?;
        aead.decrypt_inout_detached(&nonce, &self.ad, chunk.into(), &tag)
            .map_err(|_| Error::Decrypt)
    }
}

/// Seals and opens `MZB1` blobs with the `blob` subkey of one master key generation.
///
/// Cheap to clone; the key is wiped when the last clone is dropped.
#[derive(Clone)]
pub struct BlobCipher {
    aead: XChaCha20Poly1305,
    key_id: KeyId,
}

impl BlobCipher {
    /// Creates a cipher keyed with the `blob` subkey of `master`.
    pub fn new(master: &MasterKey) -> Self {
        let key = master.derive(label::BLOB);
        Self {
            aead: XChaCha20Poly1305::new(key.expose_secret().into()),
            key_id: key.key_id(),
        }
    }

    /// The master key generation this cipher seals with and can open.
    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    fn check_key_id(&self, header: &BlobHeader) -> Result<(), Error> {
        if header.key_id == self.key_id {
            Ok(())
        } else {
            Err(Error::KeyIdMismatch {
                found: header.key_id.get(),
                expected: self.key_id,
            })
        }
    }

    /// Seals `plaintext` (already compressed, if it should be) for `blob_id`.
    pub fn seal(&self, plaintext: &[u8], blob_id: &BlobId) -> Result<Vec<u8>, Error> {
        let header = BlobHeader::random(self.key_id)?;
        let mut state = StreamState::new(&header, blob_id);
        let chunks = plaintext.len().div_ceil(CHUNK_LEN).max(1);
        let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + chunks * TAG_LEN);
        out.extend_from_slice(&header.to_bytes());
        if plaintext.is_empty() {
            let tag = state.seal_chunk(&self.aead, &mut [], true)?;
            out.extend_from_slice(&tag);
            return Ok(out);
        }
        for (index, chunk) in plaintext.chunks(CHUNK_LEN).enumerate() {
            let start = out.len();
            out.extend_from_slice(chunk);
            let tag = state.seal_chunk(&self.aead, &mut out[start..], index + 1 == chunks)?;
            out.extend_from_slice(&tag);
        }
        Ok(out)
    }

    /// Opens a blob sealed for `blob_id` and returns the plaintext.
    pub fn open(&self, sealed: &[u8], blob_id: &BlobId) -> Result<Vec<u8>, Error> {
        let header = BlobHeader::parse(sealed)?;
        self.check_key_id(&header)?;
        let body = &sealed[HEADER_LEN..];
        if body.len() < TAG_LEN {
            return Err(Error::Truncated);
        }
        let chunks = body.len().div_ceil(SEALED_CHUNK_LEN);
        let mut state = StreamState::new(&header, blob_id);
        let mut out = Vec::with_capacity(body.len() - chunks * TAG_LEN);
        for (index, chunk) in body.chunks(SEALED_CHUNK_LEN).enumerate() {
            let Some(ciphertext_len) = chunk.len().checked_sub(TAG_LEN) else {
                return Err(Error::Truncated);
            };
            let (ciphertext, tag) = chunk.split_at(ciphertext_len);
            let start = out.len();
            out.extend_from_slice(ciphertext);
            state.open_chunk(&self.aead, &mut out[start..], tag, index + 1 == chunks)?;
        }
        Ok(out)
    }

    /// Starts sealing a blob into `inner` chunk by chunk; the header is written right away.
    ///
    /// Call [`SealWriter::finish`] at the end. A writer dropped without `finish` leaves
    /// a blob without its final chunk, which will never open.
    pub fn seal_writer<W: Write>(
        &self,
        blob_id: &BlobId,
        inner: W,
    ) -> Result<SealWriter<W>, Error> {
        SealWriter::new(self.clone(), blob_id, inner)
    }

    /// Starts opening a blob from `inner`; the header is read and checked right away.
    pub fn open_reader<R: Read>(&self, blob_id: &BlobId, inner: R) -> Result<OpenReader<R>, Error> {
        OpenReader::new(self.clone(), blob_id, inner)
    }
}

impl fmt::Debug for BlobCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobCipher")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// Streaming sealer created by [`BlobCipher::seal_writer`].
///
/// Buffers at most one 64 KiB chunk. [`Write::flush`] flushes the inner writer but can't
/// emit a partial chunk; [`SealWriter::finish`] writes the final chunk. After any error
/// the writer is poisoned and every later call fails.
pub struct SealWriter<W: Write> {
    inner: W,
    cipher: BlobCipher,
    state: StreamState,
    buf: Vec<u8>,
    poisoned: bool,
}

impl<W: Write> SealWriter<W> {
    fn new(cipher: BlobCipher, blob_id: &BlobId, mut inner: W) -> Result<Self, Error> {
        let header = BlobHeader::random(cipher.key_id)?;
        inner.write_all(&header.to_bytes())?;
        Ok(Self {
            inner,
            state: StreamState::new(&header, blob_id),
            cipher,
            buf: Vec::with_capacity(SEALED_CHUNK_LEN),
            poisoned: false,
        })
    }

    /// Writes the final chunk, flushes and returns the inner writer.
    pub fn finish(mut self) -> Result<W, Error> {
        if self.poisoned {
            return Err(poisoned_error().into());
        }
        self.emit(true)?;
        self.inner.flush()?;
        Ok(self.inner)
    }

    /// The inner writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Seals the buffered chunk and writes it out; poisons the writer on failure.
    fn emit(&mut self, last: bool) -> Result<(), Error> {
        let result = self
            .state
            .seal_chunk(&self.cipher.aead, &mut self.buf, last)
            .and_then(|tag| {
                self.buf.extend_from_slice(&tag);
                self.inner.write_all(&self.buf).map_err(Error::from)
            });
        self.buf.clear();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
}

impl<W: Write> Write for SealWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.poisoned {
            return Err(poisoned_error());
        }
        if data.is_empty() {
            return Ok(0);
        }
        // Only seal a full buffer once more data arrives: the last chunk must carry the
        // last flag, and only `finish` knows which chunk that is.
        if self.buf.len() == CHUNK_LEN {
            self.emit(false)?;
        }
        let take = (CHUNK_LEN - self.buf.len()).min(data.len());
        self.buf.extend_from_slice(&data[..take]);
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> fmt::Debug for SealWriter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealWriter")
            .field("key_id", &self.cipher.key_id)
            .field("chunks_written", &self.state.position)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

fn poisoned_error() -> io::Error {
    io::Error::other("blob writer failed earlier; the blob is incomplete")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Reading,
    Done,
    Failed,
}

/// Streaming opener created by [`BlobCipher::open_reader`].
///
/// Each [`Read::read`] returns plaintext of chunks whose tag has verified. Errors surface
/// as [`io::ErrorKind::InvalidData`] wrapping an [`Error`]; after an error the reader
/// keeps failing. The reader checks that nothing follows the final chunk before it
/// reports end of file.
pub struct OpenReader<R: Read> {
    inner: R,
    cipher: BlobCipher,
    state: StreamState,
    /// Ciphertext read ahead: up to one sealed chunk plus one byte, to tell whether the
    /// current chunk is the last.
    ciphertext: Vec<u8>,
    plaintext: Vec<u8>,
    pos: usize,
    phase: Phase,
}

impl<R: Read> OpenReader<R> {
    fn new(cipher: BlobCipher, blob_id: &BlobId, mut inner: R) -> Result<Self, Error> {
        let mut header = [0u8; HEADER_LEN];
        let read = read_full(&mut inner, &mut header)?;
        let header = BlobHeader::parse(&header[..read])?;
        cipher.check_key_id(&header)?;
        Ok(Self {
            inner,
            state: StreamState::new(&header, blob_id),
            cipher,
            ciphertext: Vec::with_capacity(SEALED_CHUNK_LEN + 1),
            plaintext: Vec::with_capacity(CHUNK_LEN),
            pos: 0,
            phase: Phase::Reading,
        })
    }

    /// Returns the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Reads ahead until one sealed chunk plus one byte is buffered, or end of input.
    fn fill(&mut self) -> io::Result<()> {
        let target = SEALED_CHUNK_LEN + 1;
        let mut filled = self.ciphertext.len();
        self.ciphertext.resize(target, 0);
        let result = loop {
            if filled == target {
                break Ok(());
            }
            match self.inner.read(&mut self.ciphertext[filled..]) {
                Ok(0) => break Ok(()),
                Ok(n) => filled += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => break Err(err),
            }
        };
        self.ciphertext.truncate(filled);
        result
    }

    /// Decrypts the next chunk into `self.plaintext`.
    fn next_chunk(&mut self) -> Result<(), Error> {
        self.fill()?;
        let (len, last) = if self.ciphertext.len() > SEALED_CHUNK_LEN {
            (SEALED_CHUNK_LEN, false)
        } else {
            (self.ciphertext.len(), true)
        };
        let ciphertext_len = len.checked_sub(TAG_LEN).ok_or(Error::Truncated)?;
        self.plaintext.clear();
        self.plaintext
            .extend_from_slice(&self.ciphertext[..ciphertext_len]);
        self.state.open_chunk(
            &self.cipher.aead,
            &mut self.plaintext,
            &self.ciphertext[ciphertext_len..len],
            last,
        )?;
        self.ciphertext.drain(..len);
        self.pos = 0;
        if last {
            self.phase = Phase::Done;
        }
        Ok(())
    }
}

impl<R: Read> Read for OpenReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.plaintext.len() {
                let n = (self.plaintext.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.plaintext[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.phase {
                Phase::Done => return Ok(0),
                Phase::Failed => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "blob failed to open earlier",
                    ));
                }
                Phase::Reading => {
                    if let Err(err) = self.next_chunk() {
                        self.phase = Phase::Failed;
                        self.plaintext.clear();
                        self.pos = 0;
                        return Err(err.into());
                    }
                }
            }
        }
    }
}

impl<R: Read> fmt::Debug for OpenReader<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenReader")
            .field("key_id", &self.cipher.key_id)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

/// Reads until `buf` is full or the input ends; returns the number of bytes read.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests;
