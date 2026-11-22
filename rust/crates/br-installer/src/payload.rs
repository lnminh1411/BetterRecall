// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Appended zip payload and 64-byte checked trailer.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::InstallError;

/// 8-byte trailer magic at the start of the 64-byte trailer.
pub const TRAILER_MAGIC: &[u8; 8] = b"BRSETUP1";

/// Fixed size of the trailer at the end of a self-extracting installer exe.
pub const TRAILER_LEN: u64 = 64;

/// Root metadata entry inside the payload zip.
pub const SETUP_JSON: &str = "setup.json";

/// Root licence entry inside the payload zip.
pub const LICENSE_ENTRY: &str = "LICENSE";

const HASH_BUF_LEN: usize = 1024 * 1024;

/// Parsed 64-byte trailer located at the end of the installer executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trailer {
    pub offset: u64,
    pub length: u64,
    pub sha256: [u8; 32],
}

impl Trailer {
    /// Returns the payload SHA-256 digest as 64 lowercase hex characters.
    pub fn sha256_hex(&self) -> String {
        hex_lower(&self.sha256)
    }
}

/// Metadata stored in `setup.json` at the root of the payload zip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupManifest {
    pub version: String,
    pub files: u64,
    pub bytes: u64,
}

/// Returns the byte length of the PE stub preceding the appended payload.
pub fn stub_len(trailer: &Trailer) -> u64 {
    trailer.offset
}

/// Reads and validates the 64-byte trailer at the end of `path`.
pub fn read_trailer(path: &Path) -> Result<Trailer, InstallError> {
    let mut file = File::open(path)
        .map_err(|e| InstallError::io(format!("opening {}", path.display()), e))?;
    let file_len = file
        .metadata()
        .map_err(|e| InstallError::io(format!("reading metadata for {}", path.display()), e))?
        .len();
    if file_len < TRAILER_LEN {
        return Err(InstallError::Payload(
            "file is shorter than the 64-byte trailer".to_string(),
        ));
    }
    let trailer_pos = file_len - TRAILER_LEN;
    file.seek(SeekFrom::Start(trailer_pos))
        .map_err(|e| InstallError::io("seeking to payload trailer", e))?;
    let mut buf = [0u8; TRAILER_LEN as usize];
    file.read_exact(&mut buf)
        .map_err(|e| InstallError::io("reading payload trailer", e))?;

    if &buf[0..8] != TRAILER_MAGIC {
        return Err(InstallError::Payload(
            "missing BRSETUP1 payload trailer".to_string(),
        ));
    }
    if !buf[56..64].iter().all(|&b| b == 0) {
        return Err(InstallError::Payload(
            "non-zero padding in payload trailer".to_string(),
        ));
    }

    let offset = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let length = u64::from_le_bytes(buf[16..24].try_into().unwrap());
    let sha256: [u8; 32] = buf[24..56].try_into().unwrap();

    if length == 0 {
        return Err(InstallError::Payload("payload length is zero".to_string()));
    }
    if offset.checked_add(length) != Some(trailer_pos) {
        return Err(InstallError::Payload(format!(
            "payload bounds ({offset} + {length}) do not end at trailer ({trailer_pos})"
        )));
    }

    Ok(Trailer {
        offset,
        length,
        sha256,
    })
}

/// Streams the payload slice through SHA-256 (1 MiB buffer) and compares in constant time.
pub fn verify(path: &Path, trailer: &Trailer) -> Result<(), InstallError> {
    let mut file = File::open(path)
        .map_err(|e| InstallError::io(format!("opening {}", path.display()), e))?;
    let file_len = file
        .metadata()
        .map_err(|e| InstallError::io(format!("reading metadata for {}", path.display()), e))?
        .len();
    let expected_end = trailer
        .offset
        .checked_add(trailer.length)
        .and_then(|end| end.checked_add(TRAILER_LEN))
        .ok_or_else(|| InstallError::Payload("payload length overflow".to_string()))?;
    if file_len != expected_end {
        return Err(InstallError::Payload(
            "file length no longer matches trailer".to_string(),
        ));
    }

    file.seek(SeekFrom::Start(trailer.offset))
        .map_err(|e| InstallError::io("seeking to payload start", e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUF_LEN];
    let mut remaining = trailer.length;
    while remaining > 0 {
        let want = (buf.len() as u64).min(remaining) as usize;
        let n = file
            .read(&mut buf[..want])
            .map_err(|e| InstallError::io("reading payload for verification", e))?;
        if n == 0 {
            return Err(InstallError::Payload(
                "unexpected end of file while verifying payload".to_string(),
            ));
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }

    let digest: [u8; 32] = hasher.finalize().into();
    if !ct_eq(&digest, &trailer.sha256) {
        return Err(InstallError::Payload(
            "payload SHA-256 verification failed".to_string(),
        ));
    }
    Ok(())
}

/// Writes `stub_exe` + `zip` + 64-byte trailer to `out`.
pub fn append_payload(stub_exe: &Path, zip: &Path, out: &Path) -> Result<Trailer, InstallError> {
    let mut stub_file = File::open(stub_exe)
        .map_err(|e| InstallError::io(format!("opening stub {}", stub_exe.display()), e))?;
    let mut zip_file = File::open(zip)
        .map_err(|e| InstallError::io(format!("opening zip {}", zip.display()), e))?;
    let mut out_file = File::create(out)
        .map_err(|e| InstallError::io(format!("creating {}", out.display()), e))?;

    let offset = io::copy(&mut stub_file, &mut out_file)
        .map_err(|e| InstallError::io("copying stub executable", e))?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUF_LEN];
    let mut length = 0u64;
    loop {
        let n = zip_file
            .read(&mut buf)
            .map_err(|e| InstallError::io("reading payload zip", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out_file
            .write_all(&buf[..n])
            .map_err(|e| InstallError::io("writing payload zip", e))?;
        length += n as u64;
    }

    if length == 0 {
        return Err(InstallError::Payload("payload zip is empty".to_string()));
    }

    let sha256: [u8; 32] = hasher.finalize().into();
    let mut trailer_bytes = [0u8; TRAILER_LEN as usize];
    trailer_bytes[0..8].copy_from_slice(TRAILER_MAGIC);
    trailer_bytes[8..16].copy_from_slice(&offset.to_le_bytes());
    trailer_bytes[16..24].copy_from_slice(&length.to_le_bytes());
    trailer_bytes[24..56].copy_from_slice(&sha256);
    // Bytes 56..64 remain zero.

    out_file
        .write_all(&trailer_bytes)
        .map_err(|e| InstallError::io("writing payload trailer", e))?;
    out_file
        .flush()
        .map_err(|e| InstallError::io("flushing output executable", e))?;

    Ok(Trailer {
        offset,
        length,
        sha256,
    })
}

/// Reads and parses `setup.json` from the payload zip without extracting other files.
pub fn read_setup_manifest(path: &Path, trailer: &Trailer) -> Result<SetupManifest, InstallError> {
    let reader = open_payload_reader(path, trailer)?;
    let mut archive = ZipArchive::new(reader)
        .map_err(|e| InstallError::Payload(format!("invalid payload zip: {e}")))?;
    let mut entry = archive
        .by_name(SETUP_JSON)
        .map_err(|e| InstallError::Payload(format!("missing {SETUP_JSON} in payload: {e}")))?;
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .map_err(|e| InstallError::Payload(format!("reading {SETUP_JSON}: {e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| InstallError::Payload(format!("invalid {SETUP_JSON}: {e}")))
}

/// Reads the root `LICENSE` entry from the appended payload zip, normalizing line endings to CRLF
/// for Win32 multi-line edit controls. Returns `Ok(None)` if the entry is missing.
pub fn read_license_text(path: &Path) -> Result<Option<String>, InstallError> {
    let trailer = read_trailer(path)?;
    let reader = open_payload_reader(path, &trailer)?;
    let mut archive = ZipArchive::new(reader)
        .map_err(|e| InstallError::Payload(format!("invalid payload zip: {e}")))?;
    let mut entry = match archive.by_name(LICENSE_ENTRY) {
        Ok(e) => e,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => {
            return Err(InstallError::Payload(format!(
                "reading {LICENSE_ENTRY} from payload: {e}"
            )));
        }
    };
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .map_err(|e| InstallError::Payload(format!("reading {LICENSE_ENTRY}: {e}")))?;
    Ok(Some(normalize_crlf(&text)))
}

fn normalize_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Opens a bounded `SliceReader` covering only `[trailer.offset .. trailer.offset + trailer.length]`.
pub fn open_payload_reader(path: &Path, trailer: &Trailer) -> Result<SliceReader, InstallError> {
    let file = File::open(path)
        .map_err(|e| InstallError::io(format!("opening {}", path.display()), e))?;
    SliceReader::new(file, trailer.offset, trailer.length)
        .map_err(|e| InstallError::io("positioning payload reader", e))
}

/// Bounded `Read + Seek` view over a byte slice of a single `File`, so the zip reader
/// never observes the preceding PE image or the trailing 64-byte trailer.
pub struct SliceReader {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl SliceReader {
    pub fn new(mut file: File, start: u64, len: u64) -> io::Result<Self> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            file,
            start,
            len,
            pos: 0,
        })
    }
}

impl Read for SliceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let rem = self.len.saturating_sub(self.pos);
        if rem == 0 {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(rem) as usize;
        let n = self.file.read(&mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SliceReader {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        let next: i128 = match style {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(delta) => i128::from(self.len) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.pos) + i128::from(delta),
        };
        if next < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative position",
            ));
        }
        let next_u64 = u64::try_from(next)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seek position overflow"))?;
        let clamped = next_u64.min(self.len);
        let underlying = self
            .start
            .checked_add(clamped)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek offset overflow"))?;
        self.file.seek(SeekFrom::Start(underlying))?;
        self.pos = next_u64;
        Ok(self.pos)
    }
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}
