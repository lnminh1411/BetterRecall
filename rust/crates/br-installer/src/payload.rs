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
const DOS_HEADER_MIN_LEN: u64 = 0x40;
const PE_SIGNATURE: &[u8; 4] = b"PE\0\0";
const PE32_MAGIC: u16 = 0x010B;
const PE32_PLUS_MAGIC: u16 = 0x020B;
const PE_CHECKSUM_OFFSET: u64 = 64;
const PE_SECURITY_DIR_INDEX: u32 = 4;
const MAX_CERT_ALIGN_PAD: u64 = 7;

/// File offsets of the PE header fields modified by Authenticode signing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StubHeaderFixups {
    /// File offset of the 4-byte optional header `CheckSum` field.
    pub checksum_offset: u64,
    /// File offset of the 8-byte `IMAGE_DIRECTORY_ENTRY_SECURITY` data directory entry.
    pub security_dir_offset: u64,
    /// `(file_offset, size)` stored in `IMAGE_DIRECTORY_ENTRY_SECURITY`.
    pub security_dir: (u64, u64),
}

/// Parses a PE32/PE32+ file to locate the Authenticode security directory
/// (`IMAGE_DIRECTORY_ENTRY_SECURITY`, index 4) and the optional header `CheckSum` field.
///
/// Returns `Ok(None)` if the file is not a valid PE32/PE32+ image or has no non-zero
/// security directory.
pub fn unsigned_stub_header_fixups(file: &mut File) -> Result<Option<StubHeaderFixups>, InstallError> {
    let file_len = file
        .metadata()
        .map_err(|e| InstallError::io("reading PE metadata", e))?
        .len();
    if file_len < DOS_HEADER_MIN_LEN {
        return Ok(None);
    }

    file.seek(SeekFrom::Start(0))
        .map_err(|e| InstallError::io("seeking to DOS header", e))?;
    let mut dos = [0u8; DOS_HEADER_MIN_LEN as usize];
    file.read_exact(&mut dos)
        .map_err(|e| InstallError::io("reading DOS header", e))?;
    if &dos[0..2] != b"MZ" {
        return Ok(None);
    }

    let e_lfanew = u64::from(u32::from_le_bytes([
        dos[0x3C], dos[0x3D], dos[0x3E], dos[0x3F],
    ]));
    let Some(opt_off) = e_lfanew.checked_add(24) else {
        return Ok(None);
    };
    if opt_off > file_len {
        return Ok(None);
    }

    file.seek(SeekFrom::Start(e_lfanew))
        .map_err(|e| InstallError::io("seeking to PE header", e))?;
    let mut pe_hdr = [0u8; 24];
    file.read_exact(&mut pe_hdr)
        .map_err(|e| InstallError::io("reading PE header", e))?;
    if &pe_hdr[0..4] != PE_SIGNATURE {
        return Ok(None);
    }

    let opt_size = u64::from(u16::from_le_bytes([pe_hdr[20], pe_hdr[21]]));
    let Some(opt_end) = opt_off.checked_add(opt_size) else {
        return Ok(None);
    };
    if opt_size < 2 || opt_end > file_len {
        return Ok(None);
    }

    let mut magic_buf = [0u8; 2];
    file.read_exact(&mut magic_buf)
        .map_err(|e| InstallError::io("reading PE optional header magic", e))?;
    let magic = u16::from_le_bytes(magic_buf);
    let dd_rel: u64 = match magic {
        PE32_MAGIC => 96,
        PE32_PLUS_MAGIC => 112,
        _ => return Ok(None),
    };
    let sec_rel = dd_rel + u64::from(PE_SECURITY_DIR_INDEX) * 8;
    if opt_size < sec_rel + 8 {
        return Ok(None);
    }

    let num_rva_off = opt_off + (dd_rel - 4);
    file.seek(SeekFrom::Start(num_rva_off))
        .map_err(|e| InstallError::io("seeking to PE NumberOfRvaAndSizes", e))?;
    let mut num_rva_buf = [0u8; 4];
    file.read_exact(&mut num_rva_buf)
        .map_err(|e| InstallError::io("reading PE NumberOfRvaAndSizes", e))?;
    let num_rva = u32::from_le_bytes(num_rva_buf);
    if num_rva <= PE_SECURITY_DIR_INDEX {
        return Ok(None);
    }

    let security_dir_offset = opt_off + sec_rel;
    file.seek(SeekFrom::Start(security_dir_offset))
        .map_err(|e| InstallError::io("seeking to PE security directory", e))?;
    let mut sec_buf = [0u8; 8];
    file.read_exact(&mut sec_buf)
        .map_err(|e| InstallError::io("reading PE security directory", e))?;
    let sec_offset = u64::from(u32::from_le_bytes([
        sec_buf[0], sec_buf[1], sec_buf[2], sec_buf[3],
    ]));
    let sec_size = u64::from(u32::from_le_bytes([
        sec_buf[4], sec_buf[5], sec_buf[6], sec_buf[7],
    ]));
    if sec_size == 0 {
        return Ok(None);
    }

    Ok(Some(StubHeaderFixups {
        checksum_offset: opt_off + PE_CHECKSUM_OFFSET,
        security_dir_offset,
        security_dir: (sec_offset, sec_size),
    }))
}

/// Returns `Some((offset, size))` from `IMAGE_DIRECTORY_ENTRY_SECURITY` (data directory 4)
/// when `file` is a PE32/PE32+ image with a non-zero security directory, or `None` otherwise.
pub fn pe_security_dir(file: &mut File) -> Result<Option<(u64, u64)>, InstallError> {
    Ok(unsigned_stub_header_fixups(file)?.map(|f| f.security_dir))
}

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

/// Reads and validates the 64-byte trailer at the end of `path` (or immediately before
/// the trailing Authenticode certificate table, allowing 0..=7 zero alignment bytes).
pub fn read_trailer(path: &Path) -> Result<Trailer, InstallError> {
    let mut file = File::open(path)
        .map_err(|e| InstallError::io(format!("opening {}", path.display()), e))?;
    let file_len = file
        .metadata()
        .map_err(|e| InstallError::io(format!("reading metadata for {}", path.display()), e))?
        .len();

    let (trailer_pos, buf) = match pe_security_dir(&mut file)? {
        Some((sec_offset, sec_size)) => {
            if sec_offset.checked_add(sec_size) != Some(file_len) {
                return Err(InstallError::Payload(
                    "certificate table is not at the end of the file".to_string(),
                ));
            }
            if sec_offset < TRAILER_LEN {
                return Err(InstallError::Payload(
                    "file is shorter than the 64-byte trailer".to_string(),
                ));
            }
            let max_pad = MAX_CERT_ALIGN_PAD.min(sec_offset - TRAILER_LEN);
            let window_len = (TRAILER_LEN + max_pad) as usize;
            let window_start = sec_offset - (window_len as u64);
            file.seek(SeekFrom::Start(window_start))
                .map_err(|e| InstallError::io("seeking to payload trailer", e))?;
            let mut window = [0u8; (TRAILER_LEN + MAX_CERT_ALIGN_PAD) as usize];
            file.read_exact(&mut window[..window_len])
                .map_err(|e| InstallError::io("reading payload trailer", e))?;

            let mut found = None;
            for pad in 0..=(max_pad as usize) {
                let end_in_window = window_len - pad;
                if pad > 0 && window[end_in_window] != 0 {
                    break;
                }
                let start_in_window = end_in_window - (TRAILER_LEN as usize);
                if &window[start_in_window..start_in_window + 8] == TRAILER_MAGIC {
                    let mut candidate = [0u8; TRAILER_LEN as usize];
                    candidate.copy_from_slice(&window[start_in_window..end_in_window]);
                    found = Some((window_start + (start_in_window as u64), candidate));
                    break;
                }
            }
            found.ok_or_else(|| {
                InstallError::Payload("missing BRSETUP1 payload trailer".to_string())
            })?
        }
        None => {
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
            (trailer_pos, buf)
        }
    };

    if !buf[56..64].iter().all(|&b| b == 0) {
        return Err(InstallError::Payload(
            "non-zero padding in payload trailer".to_string(),
        ));
    }

    let offset = u64::from_le_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]);
    let length = u64::from_le_bytes([
        buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
    ]);
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(&buf[24..56]);

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

    match pe_security_dir(&mut file)? {
        Some((sec_offset, sec_size)) => {
            if sec_offset.checked_add(sec_size) != Some(file_len) {
                return Err(InstallError::Payload(
                    "certificate table is not at the end of the file".to_string(),
                ));
            }
            if expected_end > sec_offset || sec_offset - expected_end > MAX_CERT_ALIGN_PAD {
                return Err(InstallError::Payload(
                    "file length no longer matches trailer".to_string(),
                ));
            }
            let pad_len = (sec_offset - expected_end) as usize;
            if pad_len > 0 {
                file.seek(SeekFrom::Start(expected_end))
                    .map_err(|e| InstallError::io("seeking to certificate alignment padding", e))?;
                let mut pad = [0u8; MAX_CERT_ALIGN_PAD as usize];
                file.read_exact(&mut pad[..pad_len])
                    .map_err(|e| InstallError::io("reading certificate alignment padding", e))?;
                if !pad[..pad_len].iter().all(|&b| b == 0) {
                    return Err(InstallError::Payload(
                        "file length no longer matches trailer".to_string(),
                    ));
                }
            }
        }
        None => {
            if file_len != expected_end {
                return Err(InstallError::Payload(
                    "file length no longer matches trailer".to_string(),
                ));
            }
        }
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
