// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Install-folder integrity checking and update manifest signing/verification (R8/R9, stage I7).
//!
//! When built with `BR_RELEASE_PUBKEY` pointing at a P-256 SubjectPublicKeyInfo PEM file, the key
//! is embedded at compile time and `cfg(br_release)` is set. Without it (dev and test builds),
//! [`check_install`] returns [`CheckOutcome::Skipped`].

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use br_contracts::DataPaths;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePrivateKey, DecodePublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::SystemInformation::GetSystemTime;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
use windows::core::PCWSTR;

/// Signed manifest filename at the install root.
pub const INTEGRITY_JSON: &str = "integrity.json";
/// Raw 64-byte IEEE P1363 ECDSA P-256 signature over `integrity.json`.
pub const INTEGRITY_SIG: &str = "integrity.sig";
/// Installer marker file allowed at the install root without being listed.
pub const INSTALL_MARKER: &str = ".install";
/// Signed update manifest filename (R8).
pub const UPDATE_JSON: &str = "update.json";
/// Raw 64-byte IEEE P1363 ECDSA P-256 signature over `update.json`.
pub const UPDATE_SIG: &str = "update.sig";
/// Current manifest format version.
pub const MANIFEST_FORMAT: u32 = 1;
/// Raw ECDSA P-256 IEEE P1363 signature length (`r || s`).
pub const SIGNATURE_LEN: usize = 64;
/// Process exit code used by [`enforce_or_exit`] when an integrity check fails.
pub const INTEGRITY_EXIT_CODE: i32 = 86;

const HASH_BUF_LEN: usize = 1024 * 1024;
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

#[cfg(br_release)]
const EMBEDDED_PUBKEY_BYTES: Option<&[u8]> =
    Some(include_bytes!(concat!(env!("OUT_DIR"), "/release_pubkey.pem")));

#[cfg(not(br_release))]
const EMBEDDED_PUBKEY_BYTES: Option<&[u8]> = None;

/// Returns the compile-time embedded release public key PEM, or `None` in dev/test builds.
pub fn embedded_public_key_pem() -> Option<&'static str> {
    EMBEDDED_PUBKEY_BYTES.and_then(|b| std::str::from_utf8(b).ok())
}

/// One file entry inside [`IntegrityManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Install-root integrity manifest (`integrity.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrityManifest {
    pub format: u32,
    pub version: String,
    pub files: Vec<ManifestFile>,
}

/// Signed release update manifest (`update.json`, R8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateManifest {
    pub format: u32,
    pub version: String,
    pub installer: String,
    pub size: u64,
    pub sha256: String,
}

/// Error returned by manifest parsing, signature verification, or directory checks.
#[derive(Debug, thiserror::Error)]
pub enum IntegrityError {
    #[error("missing file: {0}")]
    Missing(String),

    #[error("changed file: {0}")]
    Changed(String),

    #[error("unlisted file: {0}")]
    Unlisted(String),

    #[error("bad signature: {0}")]
    BadSignature(String),

    #[error("bad manifest: {0}")]
    BadManifest(String),

    #[error("I/O error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

impl IntegrityError {
    /// Returns the relative path or artifact name to report in user-facing messages.
    pub fn offending_path(&self) -> &str {
        match self {
            Self::Missing(p) | Self::Changed(p) | Self::Unlisted(p) => p,
            Self::BadSignature(_) => INTEGRITY_SIG,
            Self::BadManifest(_) => INTEGRITY_JSON,
            Self::Io { path, .. } => path,
        }
    }
}

/// Outcome of [`check_install`].
#[derive(Debug)]
pub enum CheckOutcome {
    /// Compiled without `BR_RELEASE_PUBKEY` (dev and test builds).
    Skipped,
    /// Release check passed.
    Ok,
    /// Release check failed.
    Failed(IntegrityError),
}

/// How [`enforce_or_exit`] notifies the user before exiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramKind {
    Gui,
    Headless,
}

impl IntegrityManifest {
    /// Parses and strictly validates `integrity.json` bytes (UTF-8, no BOM).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IntegrityError> {
        if bytes.starts_with(&UTF8_BOM) {
            return Err(IntegrityError::BadManifest(
                "manifest must not have a UTF-8 BOM".to_string(),
            ));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|e| IntegrityError::BadManifest(format!("invalid UTF-8: {e}")))?;
        let manifest: Self = serde_json::from_str(text)
            .map_err(|e| IntegrityError::BadManifest(format!("invalid JSON: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validates all invariants on the manifest.
    pub fn validate(&self) -> Result<(), IntegrityError> {
        if self.format != MANIFEST_FORMAT {
            return Err(IntegrityError::BadManifest(format!(
                "unsupported format {} (expected {MANIFEST_FORMAT})",
                self.format
            )));
        }
        if self.version.trim().is_empty() {
            return Err(IntegrityError::BadManifest(
                "version must not be empty".to_string(),
            ));
        }
        let mut prev_path: Option<&str> = None;
        for entry in &self.files {
            validate_relative_path(&entry.path)?;
            if is_allowed_root_file(&entry.path) {
                return Err(IntegrityError::BadManifest(format!(
                    "manifest must not list excluded file '{}'",
                    entry.path
                )));
            }
            if !is_valid_sha256_hex(&entry.sha256) {
                return Err(IntegrityError::BadManifest(format!(
                    "invalid sha256 for '{}'",
                    entry.path
                )));
            }
            if let Some(prev) = prev_path {
                match cmp_ordinal_ignore_case(prev, &entry.path) {
                    Ordering::Less => {}
                    Ordering::Equal => {
                        return Err(IntegrityError::BadManifest(format!(
                            "duplicate path '{}'",
                            entry.path
                        )));
                    }
                    Ordering::Greater => {
                        return Err(IntegrityError::BadManifest(format!(
                            "paths are not sorted: '{}' after '{prev}'",
                            entry.path
                        )));
                    }
                }
            }
            prev_path = Some(&entry.path);
        }
        Ok(())
    }

    /// Serializes the manifest deterministically as compact UTF-8 JSON (no BOM, trailing LF).
    pub fn to_bytes(&self) -> Result<Vec<u8>, IntegrityError> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self)
            .map_err(|e| IntegrityError::BadManifest(format!("serialize failed: {e}")))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

impl UpdateManifest {
    /// Parses and strictly validates `update.json` bytes (UTF-8, no BOM).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IntegrityError> {
        if bytes.starts_with(&UTF8_BOM) {
            return Err(IntegrityError::BadManifest(
                "update manifest must not have a UTF-8 BOM".to_string(),
            ));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|e| IntegrityError::BadManifest(format!("invalid UTF-8: {e}")))?;
        let manifest: Self = serde_json::from_str(text)
            .map_err(|e| IntegrityError::BadManifest(format!("invalid JSON: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validates all invariants on the update manifest.
    pub fn validate(&self) -> Result<(), IntegrityError> {
        if self.format != MANIFEST_FORMAT {
            return Err(IntegrityError::BadManifest(format!(
                "unsupported format {} (expected {MANIFEST_FORMAT})",
                self.format
            )));
        }
        if self.version.trim().is_empty() {
            return Err(IntegrityError::BadManifest(
                "version must not be empty".to_string(),
            ));
        }
        validate_single_filename(&self.installer)?;
        if self.size == 0 {
            return Err(IntegrityError::BadManifest(
                "installer size must be non-zero".to_string(),
            ));
        }
        if !is_valid_sha256_hex(&self.sha256) {
            return Err(IntegrityError::BadManifest(
                "invalid installer sha256".to_string(),
            ));
        }
        Ok(())
    }

    /// Serializes the update manifest deterministically as compact UTF-8 JSON (no BOM, trailing LF).
    pub fn to_bytes(&self) -> Result<Vec<u8>, IntegrityError> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self)
            .map_err(|e| IntegrityError::BadManifest(format!("serialize failed: {e}")))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Builds an [`UpdateManifest`] by hashing `installer_path` on disk.
    pub fn from_installer(installer_path: &Path, version: &str) -> Result<Self, IntegrityError> {
        let name = installer_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                IntegrityError::BadManifest(format!(
                    "invalid installer filename '{}'",
                    installer_path.display()
                ))
            })?;
        validate_single_filename(name)?;
        let meta = fs::metadata(installer_path).map_err(|e| IntegrityError::Io {
            path: name.to_string(),
            source: e,
        })?;
        let mut buf = vec![0u8; HASH_BUF_LEN];
        let sha256 = hash_file(installer_path, name, &mut buf)?;
        let manifest = Self {
            format: MANIFEST_FORMAT,
            version: version.to_string(),
            installer: name.to_string(),
            size: meta.len(),
            sha256,
        };
        manifest.validate()?;
        Ok(manifest)
    }
}

/// Verifies a raw 64-byte IEEE P1363 ECDSA P-256 / SHA-256 signature over `manifest_bytes`.
pub fn verify_signature(
    manifest_bytes: &[u8],
    sig_bytes: &[u8],
    public_key_pem: &str,
) -> Result<(), IntegrityError> {
    if sig_bytes.len() != SIGNATURE_LEN {
        return Err(IntegrityError::BadSignature(format!(
            "expected {SIGNATURE_LEN}-byte signature, got {}",
            sig_bytes.len()
        )));
    }
    let verifying_key = VerifyingKey::from_public_key_pem(public_key_pem.trim())
        .map_err(|e| IntegrityError::BadSignature(format!("invalid public key PEM: {e}")))?;
    let signature = Signature::from_slice(sig_bytes)
        .map_err(|e| IntegrityError::BadSignature(format!("invalid signature bytes: {e}")))?;
    verifying_key
        .verify(manifest_bytes, &signature)
        .map_err(|_| IntegrityError::BadSignature("signature verification failed".to_string()))
}

/// Signs `manifest_bytes` with a PKCS#8 P-256 private key PEM, returning the raw 64-byte IEEE P1363 signature.
pub fn sign_manifest(
    manifest_bytes: &[u8],
    private_key_pem: &str,
) -> Result<[u8; SIGNATURE_LEN], IntegrityError> {
    let signing_key = SigningKey::from_pkcs8_pem(private_key_pem.trim())
        .map_err(|e| IntegrityError::BadSignature(format!("invalid private key PEM: {e}")))?;
    let sig: Signature = signing_key.sign(manifest_bytes);
    let raw = sig.to_bytes();
    let mut out = [0u8; SIGNATURE_LEN];
    out.copy_from_slice(raw.as_slice());
    Ok(out)
}

/// Signs an [`UpdateManifest`], returning `(update.json bytes, update.sig bytes)`.
pub fn sign_update_manifest(
    manifest: &UpdateManifest,
    private_key_pem: &str,
) -> Result<(Vec<u8>, [u8; SIGNATURE_LEN]), IntegrityError> {
    let bytes = manifest.to_bytes()?;
    let sig = sign_manifest(&bytes, private_key_pem)?;
    Ok((bytes, sig))
}

/// Verifies `update.sig` over `update.json` and parses + validates the [`UpdateManifest`].
pub fn verify_update_manifest(
    manifest_bytes: &[u8],
    sig_bytes: &[u8],
    public_key_pem: &str,
) -> Result<UpdateManifest, IntegrityError> {
    verify_signature(manifest_bytes, sig_bytes, public_key_pem)?;
    UpdateManifest::from_bytes(manifest_bytes)
}

/// Walks `root`, hashing every file except `integrity.json`, `integrity.sig`, and `.install`.
pub fn build_manifest(root: &Path, version: &str) -> Result<IntegrityManifest, IntegrityError> {
    let mut rel_paths = Vec::new();
    collect_files(root, "", &mut rel_paths)?;
    rel_paths.sort_by(|a, b| cmp_ordinal_ignore_case(a, b));

    let mut buf = vec![0u8; HASH_BUF_LEN];
    let mut files = Vec::with_capacity(rel_paths.len());
    for rel in rel_paths {
        let full = root.join(&rel);
        let meta = fs::metadata(&full).map_err(|e| IntegrityError::Io {
            path: rel.clone(),
            source: e,
        })?;
        let sha256 = hash_file(&full, &rel, &mut buf)?;
        files.push(ManifestFile {
            path: rel,
            size: meta.len(),
            sha256,
        });
    }

    let manifest = IntegrityManifest {
        format: MANIFEST_FORMAT,
        version: version.to_string(),
        files,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// Verifies `integrity.sig`, `integrity.json`, every listed file's size and SHA-256, and that no unlisted files exist.
pub fn check_dir(root: &Path, public_key_pem: &str) -> Result<(), IntegrityError> {
    check_dir_with_exempt(root, public_key_pem, &[])
}

/// Same as [`check_dir`], except relative paths in `allow_missing` are permitted to be absent.
/// If an exempt path is present on disk, its size and SHA-256 are still verified.
pub fn check_dir_with_exempt(
    root: &Path,
    public_key_pem: &str,
    allow_missing: &[&str],
) -> Result<(), IntegrityError> {
    let manifest = read_and_verify_manifest(root, public_key_pem)?;

    let is_exempt = |path: &str| {
        allow_missing
            .iter()
            .any(|ex| ex.eq_ignore_ascii_case(path))
    };

    // Pass 1: check existence and size of every listed file first so missing/resized files fail fast.
    let mut present_indices = Vec::with_capacity(manifest.files.len());
    for (idx, entry) in manifest.files.iter().enumerate() {
        let full = root.join(&entry.path);
        match fs::metadata(&full) {
            Ok(meta) => {
                if !meta.is_file() || meta.len() != entry.size {
                    return Err(IntegrityError::Changed(entry.path.clone()));
                }
                present_indices.push(idx);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if !is_exempt(&entry.path) {
                    return Err(IntegrityError::Missing(entry.path.clone()));
                }
            }
            Err(e) => {
                return Err(IntegrityError::Io {
                    path: entry.path.clone(),
                    source: e,
                });
            }
        }
    }

    // Pass 2: streamed SHA-256 check (1 MiB buffer) of every present listed file.
    let mut buf = vec![0u8; HASH_BUF_LEN];
    for idx in present_indices {
        let entry = &manifest.files[idx];
        let full = root.join(&entry.path);
        let actual = hash_file(&full, &entry.path, &mut buf)?;
        if actual != entry.sha256 {
            return Err(IntegrityError::Changed(entry.path.clone()));
        }
    }

    // Pass 3: walk the install root and reject any unlisted file.
    let listed: HashSet<String> = manifest
        .files
        .iter()
        .map(|f| f.path.to_ascii_lowercase())
        .collect();
    check_unlisted(root, "", &listed)?;

    Ok(())
}

/// Verifies `integrity.sig` / `integrity.json` in `root` and checks the size and SHA-256 of one listed file.
pub fn check_single_file(
    root: &Path,
    public_key_pem: &str,
    rel_path: &str,
) -> Result<(), IntegrityError> {
    let manifest = read_and_verify_manifest(root, public_key_pem)?;
    let entry = manifest
        .files
        .iter()
        .find(|f| f.path.eq_ignore_ascii_case(rel_path))
        .ok_or_else(|| IntegrityError::Unlisted(rel_path.to_string()))?;

    let full = root.join(&entry.path);
    let meta = match fs::metadata(&full) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(IntegrityError::Missing(entry.path.clone()));
        }
        Err(e) => {
            return Err(IntegrityError::Io {
                path: entry.path.clone(),
                source: e,
            });
        }
    };
    if !meta.is_file() || meta.len() != entry.size {
        return Err(IntegrityError::Changed(entry.path.clone()));
    }
    let mut buf = vec![0u8; HASH_BUF_LEN];
    let actual = hash_file(&full, &entry.path, &mut buf)?;
    if actual != entry.sha256 {
        return Err(IntegrityError::Changed(entry.path.clone()));
    }
    Ok(())
}

/// Checks `root` using the compile-time embedded release key, or returns [`CheckOutcome::Skipped`] in dev/test builds.
pub fn check_install(root: &Path) -> CheckOutcome {
    let Some(pem) = embedded_public_key_pem() else {
        return CheckOutcome::Skipped;
    };
    match check_dir(root, pem) {
        Ok(()) => CheckOutcome::Ok,
        Err(e) => CheckOutcome::Failed(e),
    }
}

/// Enforces install integrity at program start.
///
/// In dev/test builds (no `BR_RELEASE_PUBKEY`), returns immediately. On failure in a release build:
/// appends one line to `<home>\logs\integrity.log` (ignoring log errors), shows one `MessageBoxW`
/// for [`ProgramKind::Gui`] or writes to stderr for [`ProgramKind::Headless`], and exits with code 86.
pub fn enforce_or_exit(root: &Path, kind: ProgramKind) {
    let CheckOutcome::Failed(err) = check_install(root) else {
        return;
    };

    let msg = format!(
        "BetterRecall's files were changed or are damaged ({}). Reinstall BetterRecall.",
        err.offending_path()
    );
    append_integrity_log(root, kind, &err);

    match kind {
        ProgramKind::Gui => show_error_dialog(&msg),
        ProgramKind::Headless => eprintln!("{msg}"),
    }
    std::process::exit(INTEGRITY_EXIT_CODE);
}

fn read_and_verify_manifest(
    root: &Path,
    public_key_pem: &str,
) -> Result<IntegrityManifest, IntegrityError> {
    let manifest_bytes = read_required_file(&root.join(INTEGRITY_JSON), INTEGRITY_JSON)?;
    let sig_bytes = read_required_file(&root.join(INTEGRITY_SIG), INTEGRITY_SIG)?;
    verify_signature(&manifest_bytes, &sig_bytes, public_key_pem)?;
    IntegrityManifest::from_bytes(&manifest_bytes)
}

fn read_required_file(path: &Path, label: &str) -> Result<Vec<u8>, IntegrityError> {
    match fs::read(path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(IntegrityError::Missing(label.to_string()))
        }
        Err(e) => Err(IntegrityError::Io {
            path: label.to_string(),
            source: e,
        }),
    }
}

fn hash_file(path: &Path, rel: &str, buf: &mut [u8]) -> Result<String, IntegrityError> {
    let mut file = File::open(path).map_err(|e| IntegrityError::Io {
        path: rel.to_string(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    loop {
        let n = file.read(buf).map_err(|e| IntegrityError::Io {
            path: rel.to_string(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(encode_hex_lower(&hasher.finalize()))
}

fn collect_files(root: &Path, prefix: &str, out: &mut Vec<String>) -> Result<(), IntegrityError> {
    let dir = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    let entries = fs::read_dir(&dir).map_err(|e| IntegrityError::Io {
        path: if prefix.is_empty() {
            ".".to_string()
        } else {
            prefix.to_string()
        },
        source: e,
    })?;

    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| IntegrityError::Io {
            path: if prefix.is_empty() {
                ".".to_string()
            } else {
                prefix.to_string()
            },
            source: e,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}\\{name}")
        };
        let ft = entry.file_type().map_err(|e| IntegrityError::Io {
            path: rel.clone(),
            source: e,
        })?;
        children.push((rel, ft));
    }
    children.sort_by(|a, b| cmp_ordinal_ignore_case(&a.0, &b.0));

    for (rel, ft) in children {
        if ft.is_dir() {
            collect_files(root, &rel, out)?;
        } else {
            if prefix.is_empty() && is_allowed_root_file(&rel) {
                continue;
            }
            out.push(rel);
        }
    }
    Ok(())
}

fn check_unlisted(
    root: &Path,
    prefix: &str,
    listed: &HashSet<String>,
) -> Result<(), IntegrityError> {
    let dir = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    let entries = fs::read_dir(&dir).map_err(|e| IntegrityError::Io {
        path: if prefix.is_empty() {
            ".".to_string()
        } else {
            prefix.to_string()
        },
        source: e,
    })?;

    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| IntegrityError::Io {
            path: if prefix.is_empty() {
                ".".to_string()
            } else {
                prefix.to_string()
            },
            source: e,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}\\{name}")
        };
        let ft = entry.file_type().map_err(|e| IntegrityError::Io {
            path: rel.clone(),
            source: e,
        })?;
        children.push((rel, ft));
    }
    children.sort_by(|a, b| cmp_ordinal_ignore_case(&a.0, &b.0));

    for (rel, ft) in children {
        if ft.is_dir() {
            check_unlisted(root, &rel, listed)?;
        } else {
            if prefix.is_empty() && is_allowed_root_file(&rel) {
                continue;
            }
            if !listed.contains(&rel.to_ascii_lowercase()) {
                return Err(IntegrityError::Unlisted(rel));
            }
        }
    }
    Ok(())
}

fn is_allowed_root_file(rel: &str) -> bool {
    rel.eq_ignore_ascii_case(INTEGRITY_JSON)
        || rel.eq_ignore_ascii_case(INTEGRITY_SIG)
        || rel.eq_ignore_ascii_case(INSTALL_MARKER)
}

fn validate_relative_path(path: &str) -> Result<(), IntegrityError> {
    if path.is_empty() {
        return Err(IntegrityError::BadManifest(
            "file path must not be empty".to_string(),
        ));
    }
    if path.contains('\0') {
        return Err(IntegrityError::BadManifest(
            "file path must not contain NUL".to_string(),
        ));
    }
    if path.contains('/') {
        return Err(IntegrityError::BadManifest(format!(
            "file path must use '\\' separators: '{path}'"
        )));
    }
    if path.starts_with('\\') || path.ends_with('\\') {
        return Err(IntegrityError::BadManifest(format!(
            "file path must not start or end with '\\': '{path}'"
        )));
    }
    if path.contains(':') {
        return Err(IntegrityError::BadManifest(format!(
            "file path must not contain drive or ':' : '{path}'"
        )));
    }
    for seg in path.split('\\') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(IntegrityError::BadManifest(format!(
                "invalid path segment in '{path}'"
            )));
        }
    }
    Ok(())
}

fn validate_single_filename(name: &str) -> Result<(), IntegrityError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('\0')
        || name.contains('\\')
        || name.contains('/')
        || name.contains(':')
    {
        return Err(IntegrityError::BadManifest(format!(
            "invalid installer filename '{name}'"
        )));
    }
    Ok(())
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn cmp_ordinal_ignore_case(a: &str, b: &str) -> Ordering {
    let mut ia = a.chars().map(|c| c.to_ascii_lowercase());
    let mut ib = b.chars().map(|c| c.to_ascii_lowercase());
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                let ord = ca.cmp(&cb);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

fn encode_hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn append_integrity_log(root: &Path, kind: ProgramKind, err: &IntegrityError) {
    let home = DataPaths::home();
    let logs_dir = DataPaths::logs_dir(&home);
    let _ = fs::create_dir_all(&logs_dir);
    let log_path = logs_dir.join("integrity.log");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let kind_str = match kind {
            ProgramKind::Gui => "gui",
            ProgramKind::Headless => "headless",
        };
        let _ = writeln!(
            f,
            "{} [{kind_str}] root={} error={err}",
            utc_now(),
            root.display()
        );
    }
}

fn utc_now() -> String {
    let st = unsafe { GetSystemTime() };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}

fn show_error_dialog(msg: &str) {
    let title: Vec<u16> = OsString::from("BetterRecall")
        .encode_wide()
        .chain(Some(0))
        .collect();
    let body: Vec<u16> = OsString::from(msg).encode_wide().chain(Some(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            Some(HWND::default()),
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}
