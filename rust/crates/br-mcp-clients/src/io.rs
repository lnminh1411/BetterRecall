// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! File I/O helpers: UTF-8 BOM and CRLF/LF preservation, backup creation, and atomic replacement.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ClientError;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Preserved file-level formatting attributes (BOM and line endings).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FileStyle {
    pub has_bom: bool,
    pub crlf: bool,
}

/// Decoded contents of an existing text file alongside its original style.
pub(crate) struct TextFile {
    pub body: String,
    pub style: FileStyle,
}

/// Returns `<path>.bak-betterrecall`.
pub(crate) fn backup_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".bak-betterrecall");
    PathBuf::from(os)
}

/// Reads an existing UTF-8 file, returning `Ok(None)` if it does not exist.
pub(crate) fn read_text_file(path: &Path) -> Result<Option<TextFile>, ClientError> {
    let raw = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ClientError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let (has_bom, slice) = if let Some(rest) = raw.strip_prefix(UTF8_BOM) {
        (true, rest)
    } else {
        (false, raw.as_slice())
    };

    let text = std::str::from_utf8(slice).map_err(|e| ClientError::Malformed {
        path: path.to_path_buf(),
        detail: format!("file is not valid UTF-8: {e}"),
    })?;

    let crlf = text.contains("\r\n");
    Ok(Some(TextFile {
        body: text.to_string(),
        style: FileStyle { has_bom, crlf },
    }))
}

/// Normalizes line endings in `body` to match `style`, re-attaches BOM if needed, creates a backup
/// if `existed` is true, and atomically replaces `path` via a temp file in the same directory.
pub(crate) fn write_text_file(
    path: &Path,
    body: &str,
    style: FileStyle,
    existed: bool,
) -> Result<Option<PathBuf>, ClientError> {
    let parent = path.parent().ok_or_else(|| ClientError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config path has no parent directory",
        ),
    })?;

    fs::create_dir_all(parent).map_err(|source| ClientError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let backup = if existed {
        let bak = backup_path(path);
        fs::copy(path, &bak).map_err(|source| ClientError::Io {
            path: bak.clone(),
            source,
        })?;
        Some(bak)
    } else {
        None
    };

    let normalized = normalize_newlines(body, style.crlf);
    let mut bytes = Vec::with_capacity(normalized.len() + if style.has_bom { 3 } else { 0 });
    if style.has_bom {
        bytes.extend_from_slice(UTF8_BOM);
    }
    bytes.extend_from_slice(normalized.as_bytes());

    let (tmp_path, mut file) = create_temp_file(parent)?;
    let write_res = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(source) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(ClientError::Io {
            path: tmp_path,
            source,
        });
    }

    // On Windows, `std::fs::rename` replaces an existing destination file atomically on the same
    // volume (via SetFileInformationByHandle / MoveFileExW with REPLACE_EXISTING).
    if let Err(source) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ClientError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(backup)
}

fn normalize_newlines(text: &str, crlf: bool) -> String {
    if crlf {
        let lf_only = text.replace("\r\n", "\n");
        lf_only.replace('\n', "\r\n")
    } else {
        text.replace("\r\n", "\n")
    }
}

fn create_temp_file(dir: &Path) -> Result<(PathBuf, fs::File), ClientError> {
    let pid = std::process::id();
    for _ in 0..64 {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(".betterrecall-tmp-{pid}-{seq}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(ClientError::Io {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Err(ClientError::Io {
        path: dir.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate unique temporary file",
        ),
    })
}
