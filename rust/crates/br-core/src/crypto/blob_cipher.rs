// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use br_contracts::full_path;
use windows::Win32::Storage::FileSystem::{FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_READ};
use zeroize::{Zeroize, Zeroizing};

use crate::crypto::{CryptoError, KeyVault};
use crate::win32;

type KeyProvider = Arc<dyn Fn() -> Result<Zeroizing<Vec<u8>>, CryptoError> + Send + Sync>;

/// AES-256-GCM file encryption in 64 KiB chunks. Key = HKDF(data key, `"betterrecall-blob"`).
///
/// File format: header `"BRB1"` | version `1` | 3 reserved zero bytes | 12-byte random file nonce,
/// then chunks of ciphertext + 16-byte tag. Chunk nonce = file nonce XOR big-endian chunk index in
/// the last 8 bytes. AAD = header | final flag (`0` or `1`).
#[derive(Clone)]
pub struct BlobCipher {
    key_provider: KeyProvider,
}

impl BlobCipher {
    pub const CHUNK_BYTES: usize = 64 * 1024;
    pub const TAG_BYTES: usize = 16;
    pub const HEADER_BYTES: usize = 20;
    pub const MAGIC: [u8; 4] = *b"BRB1";

    pub fn new(
        key_provider: impl Fn() -> Result<Zeroizing<Vec<u8>>, CryptoError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            key_provider: Arc::new(key_provider),
        }
    }

    pub fn from_vault(vault: Arc<KeyVault>) -> Self {
        Self::new(move || vault.derive("betterrecall-blob"))
    }

    pub fn open_write(&self, path: &Path) -> Result<BlobWriter, CryptoError> {
        let resolved = full_path(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(dir) = resolved.parent() {
            fs::create_dir_all(dir)?;
        }
        let key = (self.key_provider)()?;
        if key.len() != 32 {
            return Err(CryptoError::InvalidArgument(
                "Key must be 32 bytes.".to_string(),
            ));
        }
        let fs = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(path)?;
        BlobWriter::new(fs, key)
    }

    pub fn open_read(&self, path: &Path) -> Result<BlobReader, CryptoError> {
        let key = (self.key_provider)()?;
        if key.len() != 32 {
            return Err(CryptoError::InvalidArgument(
                "Key must be 32 bytes.".to_string(),
            ));
        }
        let fs = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN.0)
            .open(path)?;
        BlobReader::new(fs, key)
    }

    pub fn chunk_nonce(file_nonce: &[u8; 12], index: i64) -> [u8; 12] {
        let mut nonce = *file_nonce;
        let idx = index.to_be_bytes();
        for (n, &b) in nonce[4..12].iter_mut().zip(&idx) {
            *n ^= b;
        }
        nonce
    }

    pub fn aad(header: &[u8; Self::HEADER_BYTES], is_final: bool) -> [u8; Self::HEADER_BYTES + 1] {
        let mut aad = [0u8; Self::HEADER_BYTES + 1];
        aad[..Self::HEADER_BYTES].copy_from_slice(header);
        aad[Self::HEADER_BYTES] = u8::from(is_final);
        aad
    }
}

pub struct BlobWriter {
    fs: File,
    gcm: Aes256Gcm,
    _key: Zeroizing<Vec<u8>>,
    header: [u8; BlobCipher::HEADER_BYTES],
    buf: Vec<u8>,
    fill: usize,
    index: i64,
    done: bool,
}

impl BlobWriter {
    fn new(mut fs: File, key: Zeroizing<Vec<u8>>) -> Result<Self, CryptoError> {
        let gcm = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| CryptoError::InvalidArgument("Invalid AES-256 key.".to_string()))?;
        let mut header = [0u8; BlobCipher::HEADER_BYTES];
        header[..4].copy_from_slice(&BlobCipher::MAGIC);
        header[4] = 1;
        win32::fill_random(&mut header[8..20]);
        fs.write_all(&header)?;
        Ok(Self {
            fs,
            gcm,
            _key: key,
            header,
            buf: vec![0u8; BlobCipher::CHUNK_BYTES],
            fill: 0,
            index: 0,
            done: false,
        })
    }

    fn emit_chunk(&mut self, is_final: bool) -> io::Result<()> {
        let mut file_nonce = [0u8; 12];
        file_nonce.copy_from_slice(&self.header[8..20]);
        let nonce_bytes = BlobCipher::chunk_nonce(&file_nonce, self.index);
        let aad = BlobCipher::aad(&self.header, is_final);

        let mut ct = self.buf[..self.fill].to_vec();
        let tag = self
            .gcm
            .encrypt_in_place_detached(Nonce::from_slice(&nonce_bytes), &aad, &mut ct)
            .map_err(|_| io::Error::other("AES-GCM encryption failed."))?;
        self.fs.write_all(&ct)?;
        self.fs.write_all(tag.as_slice())?;
        self.buf[..self.fill].zeroize();
        self.index += 1;
        self.fill = 0;
        Ok(())
    }

    /// Emits the final authenticated chunk and flushes to disk.
    pub fn finish(mut self) -> io::Result<()> {
        if !self.done {
            self.done = true;
            self.emit_chunk(true)?;
            self.fs.sync_all()?;
        }
        Ok(())
    }
}

impl Write for BlobWriter {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        if self.done {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "BlobWriter already finished.",
            ));
        }
        let total = data.len();
        while !data.is_empty() {
            if self.fill == BlobCipher::CHUNK_BYTES {
                self.emit_chunk(false)?;
            }
            let n = (BlobCipher::CHUNK_BYTES - self.fill).min(data.len());
            self.buf[self.fill..self.fill + n].copy_from_slice(&data[..n]);
            self.fill += n;
            data = &data[n..];
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if !self.done {
            self.done = true;
            let _ = self.emit_chunk(true);
            let _ = self.fs.sync_all();
        }
        self.buf.zeroize();
    }
}

pub struct BlobReader {
    fs: File,
    gcm: Aes256Gcm,
    _key: Zeroizing<Vec<u8>>,
    header: [u8; BlobCipher::HEADER_BYTES],
    cur: Vec<u8>,
    next: Vec<u8>,
    plain: Vec<u8>,
    next_len: usize,
    plain_len: usize,
    plain_pos: usize,
    index: i64,
    final_seen: bool,
}

impl BlobReader {
    fn new(mut fs: File, key: Zeroizing<Vec<u8>>) -> Result<Self, CryptoError> {
        let gcm = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| CryptoError::InvalidArgument("Invalid AES-256 key.".to_string()))?;
        let mut header = [0u8; BlobCipher::HEADER_BYTES];
        if read_full(&mut fs, &mut header)? != BlobCipher::HEADER_BYTES
            || header[..4] != BlobCipher::MAGIC
            || header[4] != 1
        {
            return Err(CryptoError::Cryptographic("Blob is damaged.".to_string()));
        }
        let mut next = vec![0u8; BlobCipher::CHUNK_BYTES + BlobCipher::TAG_BYTES];
        let next_len = read_full(&mut fs, &mut next)?;
        if next_len < BlobCipher::TAG_BYTES {
            return Err(CryptoError::Cryptographic("Blob is damaged.".to_string()));
        }
        Ok(Self {
            fs,
            gcm,
            _key: key,
            header,
            cur: vec![0u8; BlobCipher::CHUNK_BYTES + BlobCipher::TAG_BYTES],
            next,
            plain: vec![0u8; BlobCipher::CHUNK_BYTES],
            next_len,
            plain_len: 0,
            plain_pos: 0,
            index: 0,
            final_seen: false,
        })
    }

    fn next_chunk(&mut self) -> io::Result<bool> {
        if self.final_seen {
            return Ok(false);
        }
        let cur_len = self.next_len;
        self.cur[..cur_len].copy_from_slice(&self.next[..cur_len]);
        self.next_len = if cur_len == self.cur.len() {
            read_full(&mut self.fs, &mut self.next)?
        } else {
            0
        };
        let is_final = self.next_len == 0;
        if !is_final && self.next_len < BlobCipher::TAG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Blob is damaged.",
            ));
        }

        let mut file_nonce = [0u8; 12];
        file_nonce.copy_from_slice(&self.header[8..20]);
        let nonce_bytes = BlobCipher::chunk_nonce(&file_nonce, self.index);
        let aad = BlobCipher::aad(&self.header, is_final);

        let len = cur_len - BlobCipher::TAG_BYTES;
        self.plain[..len].copy_from_slice(&self.cur[..len]);
        let tag = Tag::from_slice(&self.cur[len..cur_len]);
        self.gcm
            .decrypt_in_place_detached(
                Nonce::from_slice(&nonce_bytes),
                &aad,
                &mut self.plain[..len],
                tag,
            )
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Blob is damaged."))?;
        self.plain_len = len;
        self.plain_pos = 0;
        self.index += 1;
        self.final_seen = is_final;
        Ok(true)
    }
}

impl Read for BlobReader {
    fn read(&mut self, dest: &mut [u8]) -> io::Result<usize> {
        if dest.is_empty() {
            return Ok(0);
        }
        while self.plain_pos == self.plain_len {
            if !self.next_chunk()? {
                return Ok(0);
            }
        }
        let n = dest.len().min(self.plain_len - self.plain_pos);
        dest[..n].copy_from_slice(&self.plain[self.plain_pos..self.plain_pos + n]);
        self.plain_pos += n;
        Ok(n)
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        self.plain.zeroize();
    }
}

fn read_full(fs: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match fs.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}
