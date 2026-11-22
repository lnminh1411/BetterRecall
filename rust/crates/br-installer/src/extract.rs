// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Verified payload extraction into a per-PID staging folder with cleanup on failure.

use std::fs::{self, File};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::PCWSTR;
use zip::ZipArchive;

use crate::InstallError;
use crate::ctx::Ctx;
use crate::log::log_line;
use crate::payload::{
    SETUP_JSON, SetupManifest, Trailer, open_payload_reader, read_setup_manifest, verify,
};
use crate::progress::{NoProgress, Progress, Step};

/// Prefix for staging folders created beside `install_dir`.
pub const STAGING_PREFIX: &str = ".BetterRecall.staging-";

/// Marker file written inside an extracted install folder.
pub const INSTALL_MARKER: &str = ".install";

/// Result of a verified extraction into a staging folder.
#[derive(Debug, Clone)]
pub struct Staged {
    pub dir: PathBuf,
    pub manifest: SetupManifest,
    pub sha256_hex: String,
    pub file_count: u64,
}

/// Parsed contents of `<install_dir>\.install`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallMarker {
    pub version: String,
    pub sha256_hex: String,
    pub file_count: u64,
}

/// Reads `<install_dir>\.install` if present and well-formed.
pub fn read_install_marker(install_dir: &Path) -> Option<InstallMarker> {
    let text = fs::read_to_string(install_dir.join(INSTALL_MARKER)).ok()?;
    let mut lines = text.lines();
    let version = lines.next()?.trim().to_string();
    let sha256_hex = lines.next()?.trim().to_string();
    let file_count: u64 = lines.next()?.trim().parse().ok()?;
    if version.is_empty() || sha256_hex.is_empty() {
        return None;
    }
    Some(InstallMarker {
        version,
        sha256_hex,
        file_count,
    })
}

/// Queries available free bytes for the caller on the drive containing `path`
/// (walking up to the nearest existing ancestor).
pub fn available_disk_space(path: &Path) -> Result<u64, InstallError> {
    let mut cur = Some(path);
    let existing = loop {
        match cur {
            Some(p) if p.exists() => break p.to_path_buf(),
            Some(p) => cur = p.parent(),
            None => break path.to_path_buf(),
        }
    };
    let wide: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free_available = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_available),
            None,
            None,
        )
    }
    .map_err(|e| {
        InstallError::io(
            format!("checking free disk space for {}", existing.display()),
            io::Error::from_raw_os_error(e.code().0),
        )
    })?;
    Ok(free_available)
}

/// Verifies the payload, checks free space (with 10% margin), and extracts to a staging folder.
pub fn extract_to_staging(
    ctx: &Ctx,
    exe: &Path,
    trailer: &Trailer,
) -> Result<Staged, InstallError> {
    extract_to_staging_with_progress(ctx, exe, trailer, available_disk_space, &NoProgress)
}

/// Same as [`extract_to_staging`], with an injectable free-space source for tests.
pub fn extract_to_staging_with_space<F>(
    ctx: &Ctx,
    exe: &Path,
    trailer: &Trailer,
    free_space: F,
) -> Result<Staged, InstallError>
where
    F: FnOnce(&Path) -> Result<u64, InstallError>,
{
    extract_to_staging_with_progress(ctx, exe, trailer, free_space, &NoProgress)
}

/// Same as [`extract_to_staging_with_space`], with progress reporting and cooperative cancellation.
pub fn extract_to_staging_with_progress<F>(
    ctx: &Ctx,
    exe: &Path,
    trailer: &Trailer,
    free_space: F,
    progress: &dyn Progress,
) -> Result<Staged, InstallError>
where
    F: FnOnce(&Path) -> Result<u64, InstallError>,
{
    if progress.cancelled() {
        return Err(InstallError::Cancelled);
    }
    verify(exe, trailer)?;
    if progress.cancelled() {
        return Err(InstallError::Cancelled);
    }
    let manifest = read_setup_manifest(exe, trailer)?;

    let parent = ctx
        .install_dir
        .parent()
        .ok_or_else(|| InstallError::Other("install_dir has no parent directory".to_string()))?;
    fs::create_dir_all(parent).map_err(|e| {
        InstallError::io(
            format!("creating parent folder {}", parent.display()),
            e,
        )
    })?;

    let needed = manifest.bytes.saturating_add(manifest.bytes.div_ceil(10));
    let available = free_space(parent)?;
    if available < needed {
        return Err(InstallError::NotEnoughSpace { needed, available });
    }

    sweep_stale_staging(parent, &ctx.home);

    let pid = std::process::id();
    let staging_dir = parent.join(format!("{STAGING_PREFIX}{pid}"));
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).map_err(|e| {
            InstallError::io(
                format!("removing leftover staging folder {}", staging_dir.display()),
                e,
            )
        })?;
    }
    fs::create_dir(&staging_dir).map_err(|e| {
        InstallError::io(
            format!("creating staging folder {}", staging_dir.display()),
            e,
        )
    })?;

    let mut guard = StagingGuard {
        dir: staging_dir.clone(),
        home: ctx.home.clone(),
        keep: false,
    };

    progress.step(Step::Extracting);
    let sha256_hex = trailer.sha256_hex();
    let extracted_files = extract_entries(exe, trailer, &staging_dir, progress)?;

    let marker_content = format!("{}\n{}\n{}\n", manifest.version, sha256_hex, extracted_files);
    fs::write(staging_dir.join(INSTALL_MARKER), marker_content).map_err(|e| {
        InstallError::io(
            format!("writing {INSTALL_MARKER} marker in staging folder"),
            e,
        )
    })?;

    guard.keep = true;
    Ok(Staged {
        dir: staging_dir,
        manifest,
        sha256_hex,
        file_count: extracted_files,
    })
}

fn extract_entries(
    exe: &Path,
    trailer: &Trailer,
    staging_dir: &Path,
    progress: &dyn Progress,
) -> Result<u64, InstallError> {
    let reader = open_payload_reader(exe, trailer)?;
    let mut archive = ZipArchive::new(reader)
        .map_err(|e| InstallError::Payload(format!("invalid payload zip: {e}")))?;

    let mut total_bytes = 0u64;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| InstallError::Payload(format!("reading zip entry {i}: {e}")))?;
        if entry.is_dir() || entry.name() == SETUP_JSON {
            continue;
        }
        total_bytes = total_bytes.saturating_add(entry.size());
    }

    if total_bytes > 0 {
        progress.bytes(0, total_bytes);
    }

    let mut file_count = 0u64;
    let mut done_bytes = 0u64;
    for i in 0..archive.len() {
        if progress.cancelled() {
            return Err(InstallError::Cancelled);
        }
        let mut entry = archive
            .by_index(i)
            .map_err(|e| InstallError::Payload(format!("reading zip entry {i}: {e}")))?;

        if entry.is_symlink() {
            return Err(InstallError::Payload(format!(
                "zip entry '{}' is a symlink",
                entry.name()
            )));
        }

        let rel = safe_relative_path(entry.name(), entry.enclosed_name())?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if entry.is_dir() {
            let dest_dir = staging_dir.join(&rel);
            fs::create_dir_all(&dest_dir).map_err(|e| {
                InstallError::io(format!("creating directory {}", dest_dir.display()), e)
            })?;
            continue;
        }
        if rel == Path::new(SETUP_JSON) {
            continue;
        }

        let entry_size = entry.size();
        let dest = staging_dir.join(&rel);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)
                .map_err(|e| InstallError::io(format!("creating directory {}", p.display()), e))?;
        }
        let mut out = File::create(&dest)
            .map_err(|e| InstallError::io(format!("creating {}", dest.display()), e))?;
        io::copy(&mut entry, &mut out)
            .map_err(|e| InstallError::io(format!("extracting {}", dest.display()), e))?;
        file_count += 1;
        done_bytes = done_bytes.saturating_add(entry_size);
        if entry_size > 0 && done_bytes < total_bytes {
            progress.bytes(done_bytes, total_bytes);
        }
    }

    if progress.cancelled() {
        return Err(InstallError::Cancelled);
    }
    progress.bytes(total_bytes, total_bytes);
    Ok(file_count)
}

fn safe_relative_path(raw: &str, enclosed: Option<PathBuf>) -> Result<PathBuf, InstallError> {
    if raw.contains('\0')
        || raw.starts_with('/')
        || raw.starts_with('\\')
        || raw.as_bytes().get(1) == Some(&b':')
    {
        return Err(InstallError::Payload(format!(
            "unsafe zip entry path: '{raw}'"
        )));
    }
    for segment in raw.split(['/', '\\']) {
        if segment == ".." {
            return Err(InstallError::Payload(format!(
                "zip entry escapes staging folder: '{raw}'"
            )));
        }
    }
    let rel = enclosed
        .ok_or_else(|| InstallError::Payload(format!("unsafe zip entry path: '{raw}'")))?;
    if rel.is_absolute() {
        return Err(InstallError::Payload(format!(
            "absolute zip entry path: '{raw}'"
        )));
    }
    for comp in rel.components() {
        if !matches!(comp, Component::Normal(_)) {
            return Err(InstallError::Payload(format!(
                "unsafe path component in zip entry: '{raw}'"
            )));
        }
    }
    Ok(rel)
}

fn sweep_stale_staging(parent: &Path, home: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let my_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some(pid_str) = name_str.strip_prefix(STAGING_PREFIX) else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == my_pid || is_process_alive(pid) {
            continue;
        }
        if let Err(e) = fs::remove_dir_all(entry.path()) {
            log_line(
                home,
                &format!(
                    "failed to remove stale staging folder {}: {e}",
                    entry.path().display()
                ),
            );
        }
    }
}

fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    let mut exit_code = 0u32;
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) }.is_ok();
    unsafe {
        let _ = CloseHandle(handle);
    }
    ok && exit_code == STILL_ACTIVE.0 as u32
}

struct StagingGuard {
    dir: PathBuf,
    home: PathBuf,
    keep: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.keep
            && self.dir.exists()
            && let Err(e) = fs::remove_dir_all(&self.dir)
        {
            log_line(
                &self.home,
                &format!(
                    "cleanup: failed to remove staging folder {}: {e}",
                    self.dir.display()
                ),
            );
        }
    }
}
