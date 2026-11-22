// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Top-level install, uninstall, and modify orchestration.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use br_contracts::{InstallLayout, PipeNames};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::PCWSTR;

use crate::InstallError;
use crate::ctx::Ctx;
use crate::extract::{
    available_disk_space, extract_to_staging_with_space, read_install_marker,
};
use crate::log::log_line;
use crate::payload::{read_setup_manifest, read_trailer, stub_len};
use crate::registry;
use crate::running::{close_running, normalized_dir_prefix, path_is_under_prefix};
use crate::shortcut;
use crate::swap::{old_folder_path, rename_with_retry, swap_into_place};

/// Installed filename of the payload-stripped setup binary.
pub const SETUP_EXE: &str = "BetterRecall-Setup.exe";

/// Base single-instance mutex name (scoped via `PipeNames::scoped` for non-default homes).
pub const SETUP_MUTEX: &str = r"Local\BetterRecall.Setup";

/// Options for [`install`].
#[derive(Debug, Clone, Default)]
pub struct InstallOpts {
    pub allow_downgrade: bool,
}

/// Options for [`uninstall`].
#[derive(Debug, Clone, Default)]
pub struct UninstallOpts {}

/// Result of an install, uninstall, or modify action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed { version: String },
    Uninstalled { reboot_required: bool },
    NothingToDo,
    /// Uninstall handed over to a temp copy of the setup exe (see `cli`).
    Relaunched,
}

/// Runs the full installation flow.
pub fn install(ctx: &Ctx, exe: &Path, opts: &InstallOpts) -> Result<Outcome, InstallError> {
    install_with_space(ctx, exe, opts, available_disk_space)
}

/// Same as [`install`], with an injectable free-space source for tests.
pub fn install_with_space<F>(
    ctx: &Ctx,
    exe: &Path,
    opts: &InstallOpts,
    free_space: F,
) -> Result<Outcome, InstallError>
where
    F: FnOnce(&Path) -> Result<u64, InstallError>,
{
    let _mutex = acquire_setup_mutex(&ctx.home)?;
    log_line(&ctx.home, &format!("install start -> {}", ctx.install_dir.display()));

    // The payload hash is checked once, by `extract_to_staging_with_space`, before anything is
    // extracted; the version read here only decides whether to refuse a downgrade.
    let trailer = read_trailer(exe)?;
    let manifest = read_setup_manifest(exe, &trailer)?;

    if let Some(installed) = read_install_marker(&ctx.install_dir)
        && !opts.allow_downgrade
        && compare_versions(&manifest.version, &installed.version) == Ordering::Less
    {
        log_line(
            &ctx.home,
            &format!(
                "downgrade refused: installed={} incoming={}",
                installed.version, manifest.version
            ),
        );
        return Err(InstallError::Downgrade {
            installed: installed.version,
            incoming: manifest.version,
        });
    }

    let staged = extract_to_staging_with_space(ctx, exe, &trailer, free_space)?;
    let version = staged.manifest.version.clone();

    close_running(ctx)?;
    swap_into_place(ctx, staged)?;
    copy_stub_exe(exe, stub_len(&trailer), &ctx.install_dir)?;

    registry::register(ctx, &version)?;
    shortcut::create_shortcut(ctx)?;

    if ctx.start_after {
        let agent = ctx.install_dir.join(InstallLayout::AGENT_EXE);
        log_line(&ctx.home, &format!("starting {}", agent.display()));
        Command::new(&agent)
            .current_dir(&ctx.install_dir)
            .spawn()
            .map_err(|e| InstallError::io(format!("starting {}", agent.display()), e))?;
    }

    log_line(&ctx.home, &format!("install complete: {version}"));
    Ok(Outcome::Installed { version })
}

/// Uninstalls BetterRecall from `ctx.install_dir` without touching `ctx.home`.
pub fn uninstall(ctx: &Ctx, _opts: &UninstallOpts) -> Result<Outcome, InstallError> {
    let _mutex = acquire_setup_mutex(&ctx.home)?;
    log_line(
        &ctx.home,
        &format!("uninstall start: {}", ctx.install_dir.display()),
    );

    close_running(ctx)?;
    registry::unregister(ctx)?;
    shortcut::remove_shortcut(ctx)?;

    let mut reboot_required = false;
    if ctx.install_dir.exists() {
        let self_inside = std::env::current_exe().ok().is_some_and(|me| {
            let prefix = normalized_dir_prefix(&ctx.install_dir);
            path_is_under_prefix(&me, &prefix)
        });

        if self_inside {
            schedule_or_delete_tree(&ctx.install_dir, &ctx.home);
            reboot_required = ctx.install_dir.exists();
        } else {
            let parent = ctx.install_dir.parent().ok_or_else(|| {
                InstallError::Other("install_dir has no parent directory".to_string())
            })?;
            let old_dir = old_folder_path(parent);
            if let Err(e) = rename_with_retry(&ctx.install_dir, &old_dir) {
                log_line(
                    &ctx.home,
                    &format!(
                        "uninstall: failed to rename {} aside: {e}",
                        ctx.install_dir.display()
                    ),
                );
                return Err(InstallError::InUse {
                    path: ctx.install_dir.clone(),
                });
            }
            if let Err(e) = fs::remove_dir_all(&old_dir) {
                log_line(
                    &ctx.home,
                    &format!(
                        "uninstall: remove_dir_all failed on {}: {e}; scheduling on reboot",
                        old_dir.display()
                    ),
                );
                schedule_or_delete_tree(&old_dir, &ctx.home);
                reboot_required = old_dir.exists();
            }
        }
    }

    log_line(
        &ctx.home,
        &format!("uninstall complete (reboot_required={reboot_required})"),
    );
    Ok(Outcome::Uninstalled { reboot_required })
}

/// Modify action (I1 placeholder; I3 adds model management).
pub fn modify(ctx: &Ctx) -> Result<Outcome, InstallError> {
    log_line(&ctx.home, "modify: nothing to do in I1");
    Ok(Outcome::NothingToDo)
}

/// Compares two semantic version strings (`major.minor.patch[-prerelease][+build]`).
/// Pre-release versions (e.g. `0.3.0-dev`) sort strictly below the matching release (`0.3.0`).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let pa = ParsedVersion::parse(a);
    let pb = ParsedVersion::parse(b);
    pa.cmp(&pb)
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Option<String>,
}

impl ParsedVersion {
    fn parse(raw: &str) -> Self {
        let without_build = raw.trim().split('+').next().unwrap_or("");
        let (core, prerelease) = match without_build.split_once('-') {
            Some((c, pre)) => (c, Some(pre.to_string())),
            None => (without_build, None),
        };
        let mut parts = core.split('.');
        let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        Self {
            major,
            minor,
            patch,
            prerelease,
        }
    }
}

impl Ord for ParsedVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.prerelease, &other.prerelease) {
                (None, None) => Ordering::Equal,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(a), Some(b)) => compare_prerelease(a, b),
            })
    }
}

impl PartialOrd for ParsedVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_prerelease(a: &str, b: &str) -> Ordering {
    let mut ia = a.split('.');
    let mut ib = b.split('.');
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(sa), Some(sb)) => {
                let ord = match (sa.parse::<u64>(), sb.parse::<u64>()) {
                    (Ok(na), Ok(nb)) => na.cmp(&nb),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => sa.cmp(sb),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

fn copy_stub_exe(exe: &Path, len: u64, install_dir: &Path) -> Result<(), InstallError> {
    let tmp_path = install_dir.join(format!("{SETUP_EXE}.tmp"));
    let final_path = install_dir.join(SETUP_EXE);

    let copy_res = (|| -> Result<(), InstallError> {
        let mut src = File::open(exe)
            .map_err(|e| InstallError::io(format!("opening {}", exe.display()), e))?;
        let mut dst = File::create(&tmp_path)
            .map_err(|e| InstallError::io(format!("creating {}", tmp_path.display()), e))?;
        let mut limited = Read::by_ref(&mut src).take(len);
        let copied = io::copy(&mut limited, &mut dst)
            .map_err(|e| InstallError::io("copying installer stub", e))?;
        if copied != len {
            return Err(InstallError::Payload(format!(
                "short read copying installer stub ({copied} of {len} bytes)"
            )));
        }
        dst.flush()
            .map_err(|e| InstallError::io("flushing installer stub", e))?;
        Ok(())
    })();

    if let Err(e) = copy_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    if final_path.exists() {
        let _ = fs::remove_file(&final_path);
    }
    if let Err(e) = fs::rename(&tmp_path, &final_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(InstallError::io(
            format!("renaming stub into {}", final_path.display()),
            e,
        ));
    }
    Ok(())
}

fn schedule_or_delete_tree(dir: &Path, home: &Path) {
    let mut dirs_postorder = Vec::new();
    collect_and_delete(dir, home, &mut dirs_postorder);
    for d in dirs_postorder {
        if fs::remove_dir(&d).is_err() {
            schedule_delete_on_reboot(&d, home);
        }
    }
}

fn collect_and_delete(dir: &Path, home: &Path, dirs_postorder: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                collect_and_delete(&path, home, dirs_postorder);
            } else if fs::remove_file(&path).is_err() {
                schedule_delete_on_reboot(&path, home);
            }
        }
    }
    dirs_postorder.push(dir.to_path_buf());
}

fn schedule_delete_on_reboot(path: &Path, home: &Path) {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if let Err(e) = unsafe { MoveFileExW(PCWSTR(wide.as_ptr()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) } {
        log_line(
            home,
            &format!(
                "MoveFileExW(DELAY_UNTIL_REBOOT) failed for {}: {e}",
                path.display()
            ),
        );
    }
}

fn acquire_setup_mutex(home: &Path) -> Result<OwnedMutex, InstallError> {
    let name = PipeNames::scoped(SETUP_MUTEX, home);
    let wide: Vec<u16> = OsString::from(&name).encode_wide().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }.map_err(|e| {
        InstallError::io(
            format!("creating setup mutex '{name}'"),
            io::Error::from_raw_os_error(e.code().0),
        )
    })?;
    let last = unsafe { GetLastError() };
    if last == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(InstallError::AlreadyRunning);
    }
    Ok(OwnedMutex(handle))
}

struct OwnedMutex(HANDLE);

impl Drop for OwnedMutex {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}
