//! Seekable, crash-tolerant authenticated encryption for files that are
//! written once as a stream and later read at arbitrary offsets.
//!
//! A recording is written while it is being captured, and a recorder can be
//! killed at any moment (low memory, flat battery). Later the player needs to
//! seek around in it without decrypting the whole file. Both follow from
//! encrypting fixed-size chunks independently:
//!
//! ```text
//! header (48 bytes)   magic "EXVAULT1" | chunk_size u32 LE | reserved u32 | salt [32]
//! chunk 0 .. n-1      ciphertext [chunk_size] | tag [16]        (non-final)
//! chunk n             ciphertext [0 .. chunk_size) | tag [16]   (final)
//! ```
//!
//! * The file key is `HKDF-SHA256(master_key, salt)`, so every file has its own
//!   key and a plain chunk counter is a safe nonce.
//! * The nonce carries the chunk index and a "final" flag, so chunks cannot be
//!   reordered, dropped from the middle, or cut off the end without detection.
//! * The header is the associated data of every chunk.
//! * The final chunk is always shorter than `chunk_size`. A file that lacks it
//!   was interrupted: [`VaultReader`] still opens it, serves every complete
//!   chunk, and reports [`VaultReader::is_truncated`].

use std::io::{self, Read, Seek, SeekFrom, Write};

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

pub const KEY_LEN: usize = 32;
pub const HEADER_LEN: usize = 48;
pub const TAG_LEN: usize = 16;
/// About one second of 32 kbit/s audio: small enough that a crash loses very
/// little, large enough that the 16-byte tag is noise (0.4 %).
pub const DEFAULT_CHUNK_SIZE: u32 = 4096;
pub const MAX_CHUNK_SIZE: u32 = 1 << 20;

const MAGIC: [u8; 8] = *b"EXVAULT1";
const HKDF_INFO: &[u8] = b"examic.vault.v1 chacha20poly1305";

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("not a vault file or unsupported version")]
    BadHeader,
    #[error("chunk size {0} is outside 1..={MAX_CHUNK_SIZE}")]
    BadChunkSize(u32),
    #[error("authentication failed: wrong key or corrupted data")]
    Auth,
    #[error("system random number generator unavailable")]
    Rng,
}

impl From<VaultError> for io::Error {
    fn from(e: VaultError) -> Self {
        match e {
            VaultError::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other),
        }
    }
}

fn derive_cipher(master_key: &[u8; KEY_LEN], salt: &[u8]) -> ChaCha20Poly1305 {
    let mut file_key = Zeroizing::new([0u8; KEY_LEN]);
    Hkdf::<Sha256>::new(Some(salt), master_key)
        .expand(HKDF_INFO, file_key.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    ChaCha20Poly1305::new_from_slice(file_key.as_ref()).expect("key is 32 bytes")
}

fn nonce_for(chunk_index: u64, is_final: bool) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&chunk_index.to_le_bytes());
    nonce[8] = is_final as u8;
    Nonce::from(nonce)
}

/// Encrypts everything written to it into `inner`.
///
/// Call [`finish`](Self::finish) to seal the file. Dropping the writer without
/// finishing leaves a file that reads back as truncated, which is exactly what
/// a crash produces.
pub struct VaultWriter<W: Write> {
    inner: W,
    cipher: ChaCha20Poly1305,
    header: [u8; HEADER_LEN],
    chunk_size: usize,
    buf: Vec<u8>,
    next_index: u64,
    poisoned: bool,
}

impl<W: Write> VaultWriter<W> {
    pub fn create(
        mut inner: W,
        master_key: &[u8; KEY_LEN],
        chunk_size: u32,
    ) -> Result<Self, VaultError> {
        if chunk_size == 0 || chunk_size > MAX_CHUNK_SIZE {
            return Err(VaultError::BadChunkSize(chunk_size));
        }
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(&MAGIC);
        header[8..12].copy_from_slice(&chunk_size.to_le_bytes());
        getrandom::fill(&mut header[16..]).map_err(|_| VaultError::Rng)?;
        inner.write_all(&header)?;
        Ok(Self {
            cipher: derive_cipher(master_key, &header[16..]),
            inner,
            header,
            chunk_size: chunk_size as usize,
            buf: Vec::with_capacity(chunk_size as usize + TAG_LEN),
            next_index: 0,
            poisoned: false,
        })
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    fn seal(&mut self, is_final: bool) -> io::Result<()> {
        // If writing a chunk fails half-way, `buf` holds ciphertext and the
        // file may hold a partial chunk; nothing sensible can follow.
        self.poisoned = true;
        let tag = self
            .cipher
            .encrypt_inout_detached(
                &nonce_for(self.next_index, is_final),
                &self.header,
                self.buf.as_mut_slice().into(),
            )
            .map_err(|_| io::Error::other("chunk encryption failed"))?;
        self.buf.extend_from_slice(&tag);
        self.inner.write_all(&self.buf)?;
        self.buf.clear();
        self.next_index += 1;
        self.poisoned = false;
        Ok(())
    }

    fn check_poisoned(&self) -> io::Result<()> {
        if self.poisoned {
            Err(io::Error::other("vault writer failed earlier"))
        } else {
            Ok(())
        }
    }

    /// Writes the final chunk and returns the underlying writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.check_poisoned()?;
        self.seal(true)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for VaultWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.check_poisoned()?;
        let take = data.len().min(self.chunk_size - self.buf.len());
        self.buf.extend_from_slice(&data[..take]);
        if self.buf.len() == self.chunk_size {
            self.seal(false)?;
        }
        Ok(take)
    }

    /// Flushes complete chunks to the underlying writer. Bytes of a chunk that
    /// is still filling up stay in memory until the chunk is complete.
    fn flush(&mut self) -> io::Result<()> {
        self.check_poisoned()?;
        self.inner.flush()
    }
}

/// Random-access reader for a vault file.
pub struct VaultReader<R: Read + Seek> {
    inner: R,
    cipher: ChaCha20Poly1305,
    header: [u8; HEADER_LEN],
    chunk_size: usize,
    /// Number of complete non-final chunks.
    full_chunks: u64,
    /// Plaintext length of the final chunk, if the file has a valid one.
    final_len: Option<usize>,
    truncated: bool,
    cache: Vec<u8>,
    cache_index: Option<u64>,
    pos: u64,
}

impl<R: Read + Seek> VaultReader<R> {
    pub fn open(mut inner: R, master_key: &[u8; KEY_LEN]) -> Result<Self, VaultError> {
        let file_len = inner.seek(SeekFrom::End(0))?;
        if file_len < HEADER_LEN as u64 {
            return Err(VaultError::BadHeader);
        }
        let mut header = [0u8; HEADER_LEN];
        inner.seek(SeekFrom::Start(0))?;
        inner.read_exact(&mut header)?;
        if header[..8] != MAGIC || header[12..16] != [0; 4] {
            return Err(VaultError::BadHeader);
        }
        let chunk_size = u32::from_le_bytes(header[8..12].try_into().unwrap());
        if chunk_size == 0 || chunk_size > MAX_CHUNK_SIZE {
            return Err(VaultError::BadHeader);
        }

        let stride = chunk_size as u64 + TAG_LEN as u64;
        let body = file_len - HEADER_LEN as u64;
        let full_chunks = body / stride;
        let remainder = (body % stride) as usize;

        let mut reader = Self {
            cipher: derive_cipher(master_key, &header[16..]),
            inner,
            header,
            chunk_size: chunk_size as usize,
            full_chunks,
            final_len: None,
            truncated: true,
            cache: Vec::with_capacity(chunk_size as usize + TAG_LEN),
            cache_index: None,
            pos: 0,
        };

        // A wrong key must be reported as such, not as a truncated file, so
        // the first chunk has to authenticate before the tail is judged.
        if full_chunks > 0 {
            reader.load_chunk(0)?;
        }
        if remainder >= TAG_LEN {
            match reader.read_chunk(full_chunks, remainder, true) {
                Ok(()) => {
                    reader.final_len = Some(remainder - TAG_LEN);
                    reader.truncated = false;
                    reader.cache_index = Some(full_chunks);
                }
                // With no complete chunk before it, a failing tail is the only
                // evidence there is, and a wrong key is the likelier cause.
                Err(VaultError::Auth) if full_chunks == 0 => return Err(VaultError::Auth),
                // Otherwise the key is known good: the tail is a torn write.
                Err(VaultError::Auth) => reader.cache_index = None,
                Err(e) => return Err(e),
            }
        }
        Ok(reader)
    }

    /// Total number of plaintext bytes available.
    pub fn len(&self) -> u64 {
        self.full_chunks * self.chunk_size as u64 + self.final_len.unwrap_or(0) as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True if the writer never finished the file (crash or power loss). All
    /// data up to the last complete chunk is still readable.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Reads and authenticates the chunk at `index` into the cache.
    fn read_chunk(
        &mut self,
        index: u64,
        stored_len: usize,
        is_final: bool,
    ) -> Result<(), VaultError> {
        self.cache_index = None;
        let stride = (self.chunk_size + TAG_LEN) as u64;
        self.inner
            .seek(SeekFrom::Start(HEADER_LEN as u64 + index * stride))?;
        self.cache.resize(stored_len, 0);
        self.inner.read_exact(&mut self.cache)?;
        let tag = Tag::try_from(&self.cache[stored_len - TAG_LEN..]).expect("tag is 16 bytes");
        self.cache.truncate(stored_len - TAG_LEN);
        self.cipher
            .decrypt_inout_detached(
                &nonce_for(index, is_final),
                &self.header,
                self.cache.as_mut_slice().into(),
                &tag,
            )
            .map_err(|_| VaultError::Auth)?;
        Ok(())
    }

    fn load_chunk(&mut self, index: u64) -> Result<(), VaultError> {
        if self.cache_index == Some(index) {
            return Ok(());
        }
        if index < self.full_chunks {
            self.read_chunk(index, self.chunk_size + TAG_LEN, false)?;
        } else {
            let final_len = self.final_len.ok_or(VaultError::Auth)?;
            self.read_chunk(index, final_len + TAG_LEN, true)?;
        }
        self.cache_index = Some(index);
        Ok(())
    }

    /// Reads up to `out.len()` bytes starting at plaintext `offset`. Returns
    /// fewer bytes only at the end of the data, and 0 at or past the end.
    pub fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, VaultError> {
        let len = self.len();
        let mut done = 0;
        while done < out.len() {
            let at = offset + done as u64;
            if at >= len {
                break;
            }
            self.load_chunk(at / self.chunk_size as u64)?;
            let start = (at % self.chunk_size as u64) as usize;
            let n = (self.cache.len() - start).min(out.len() - done);
            out[done..done + n].copy_from_slice(&self.cache[start..start + n]);
            done += n;
        }
        Ok(done)
    }
}

impl<R: Read + Seek> Read for VaultReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.read_at(self.pos, out)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for VaultReader<R> {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let new_pos = match target {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len().checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = new_pos
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek outside the file"))?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const KEY: [u8; KEY_LEN] = [7; KEY_LEN];
    const CHUNK: u32 = 64;
    const STRIDE: usize = CHUNK as usize + TAG_LEN;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn seal(data: &[u8]) -> Vec<u8> {
        let mut w = VaultWriter::create(Vec::new(), &KEY, CHUNK).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap()
    }

    fn open(file: Vec<u8>) -> Result<VaultReader<Cursor<Vec<u8>>>, VaultError> {
        VaultReader::open(Cursor::new(file), &KEY)
    }

    fn read_all(r: &mut VaultReader<Cursor<Vec<u8>>>) -> Vec<u8> {
        let mut out = Vec::new();
        r.seek(SeekFrom::Start(0)).unwrap();
        r.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn round_trips_every_length_around_chunk_boundaries() {
        for len in [0, 1, 63, 64, 65, 127, 128, 129, 1000] {
            let data = pattern(len);
            let mut r = open(seal(&data)).unwrap();
            assert_eq!(r.len(), len as u64, "len {len}");
            assert!(!r.is_truncated(), "len {len}");
            assert_eq!(read_all(&mut r), data, "len {len}");
        }
    }

    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let data = vec![0xAB; 500];
        let file = seal(&data);
        assert!(!file.windows(16).any(|w| w == [0xAB; 16]));
    }

    #[test]
    fn two_files_with_the_same_content_differ() {
        let data = pattern(200);
        assert_ne!(seal(&data)[HEADER_LEN..], seal(&data)[HEADER_LEN..]);
    }

    #[test]
    fn reads_at_arbitrary_offsets() {
        let data = pattern(1000);
        let mut r = open(seal(&data)).unwrap();
        for (offset, len) in [
            (0, 10),
            (60, 10),
            (64, 64),
            (500, 300),
            (990, 50),
            (1000, 5),
        ] {
            let mut out = vec![0u8; len];
            let n = r.read_at(offset as u64, &mut out).unwrap();
            let expected = &data[offset.min(1000)..(offset + len).min(1000)];
            assert_eq!(&out[..n], expected, "offset {offset} len {len}");
        }
        assert_eq!(r.seek(SeekFrom::End(-10)).unwrap(), 990);
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &data[990..]);
    }

    #[test]
    fn unfinished_file_keeps_every_complete_chunk() {
        let data = pattern(200);
        let mut w = VaultWriter::create(Vec::new(), &KEY, CHUNK).unwrap();
        w.write_all(&data).unwrap();
        let file = w.get_ref().clone(); // what is on disk when the process dies
        let mut r = open(file).unwrap();
        assert!(r.is_truncated());
        assert_eq!(r.len(), 192);
        assert_eq!(read_all(&mut r), &data[..192]);
    }

    #[test]
    fn torn_last_write_is_dropped_and_reported() {
        let data = pattern(300);
        let file = seal(&data);
        for cut in [1, 10, TAG_LEN, 50, STRIDE + 3] {
            let mut r = open(file[..file.len() - cut].to_vec()).unwrap();
            assert!(r.is_truncated(), "cut {cut}");
            let kept = r.len() as usize;
            assert!(
                kept < 300 && kept.is_multiple_of(CHUNK as usize),
                "cut {cut}"
            );
            assert_eq!(read_all(&mut r), &data[..kept], "cut {cut}");
        }
    }

    #[test]
    fn cutting_a_finished_file_at_a_chunk_boundary_is_detected() {
        let file = seal(&pattern(128)); // two full chunks + empty final chunk
        assert_eq!(file.len(), HEADER_LEN + 2 * STRIDE + TAG_LEN);
        let r = open(file[..HEADER_LEN + 2 * STRIDE].to_vec()).unwrap();
        assert!(r.is_truncated());
        assert_eq!(r.len(), 128);
    }

    #[test]
    fn wrong_key_is_rejected() {
        for len in [10, 500] {
            let file = seal(&pattern(len));
            let result = VaultReader::open(Cursor::new(file), &[8; KEY_LEN]);
            assert!(matches!(result, Err(VaultError::Auth)), "len {len}");
        }
    }

    #[test]
    fn flipped_bit_is_detected() {
        let data = pattern(500);
        let mut file = seal(&data);
        file[HEADER_LEN + 3 * STRIDE + 5] ^= 1;
        let mut r = open(file).unwrap();
        let mut out = vec![0u8; 64];
        assert_eq!(r.read_at(0, &mut out).unwrap(), 64);
        assert!(matches!(r.read_at(3 * 64, &mut out), Err(VaultError::Auth)));
    }

    #[test]
    fn swapped_chunks_are_detected() {
        let mut file = seal(&pattern(500));
        let (a, b) = (HEADER_LEN + STRIDE, HEADER_LEN + 2 * STRIDE);
        let first = file[a..b].to_vec();
        file.copy_within(b..b + STRIDE, a);
        file[b..b + STRIDE].copy_from_slice(&first);
        let mut r = open(file).unwrap();
        let mut out = vec![0u8; 64];
        assert!(matches!(r.read_at(64, &mut out), Err(VaultError::Auth)));
    }

    #[test]
    fn tampered_header_is_detected() {
        let mut file = seal(&pattern(500));
        file[20] ^= 1; // salt
        assert!(matches!(open(file), Err(VaultError::Auth)));

        let mut file = seal(&pattern(500));
        file[0] = b'X';
        assert!(matches!(open(file), Err(VaultError::BadHeader)));
    }

    #[test]
    fn rejects_bad_chunk_sizes() {
        for size in [0, MAX_CHUNK_SIZE + 1] {
            let result = VaultWriter::create(Vec::new(), &KEY, size);
            assert!(matches!(result, Err(VaultError::BadChunkSize(_))));
        }
    }
}
