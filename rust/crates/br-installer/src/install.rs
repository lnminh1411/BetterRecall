// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Top-level install, uninstall, and modify orchestration.

use std::cmp::Ordering;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use br_contracts::{DataPaths, InstallLayout, PipeNames, Settings, full_path, strip_jsonc};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Profile, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::{GUID, PCWSTR};

use crate::InstallError;
use crate::ctx::Ctx;
use crate::extract::{
    available_disk_space, extract_to_staging_with_progress, read_install_marker,
};
use crate::log::log_line;
use crate::payload::{read_setup_manifest, read_trailer, stub_len};
use crate::progress::{NoProgress, Progress, Step};
use crate::registry;
use crate::running::{close_running, normalized_dir_prefix, path_is_under_prefix};
use crate::shortcut;
use crate::swap::{old_folder_path, rename_with_retry, swap_into_place};

/// Installed filename of the payload-stripped setup binary.
pub const SETUP_EXE: &str = "BetterRecall-Setup.exe";

/// Base single-instance mutex name (scoped via `PipeNames::scoped` for non-default homes).
pub const SETUP_MUTEX: &str = r"Local\BetterRecall.Setup";

/// Which AI models the user chose to install (recorded in HKCU; downloaded in I3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelMode {
    Full,
    Minimal,
    #[default]
    Later,
}

impl ModelMode {
    /// Canonical lowercase registry and CLI representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Minimal => "minimal",
            Self::Later => "later",
        }
    }

    /// Parses a case-insensitive model mode string (`full`, `minimal`, `later`).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "minimal" => Some(Self::Minimal),
            "later" => Some(Self::Later),
            _ => None,
        }
    }
}

/// Options for [`install`].
#[derive(Debug, Clone, Default)]
pub struct InstallOpts {
    pub allow_downgrade: bool,
    pub model_mode: ModelMode,
}

/// Options for [`modify_with_progress`].
#[derive(Debug, Clone, Default)]
pub struct ModifyOpts {
    pub model_mode: ModelMode,
}

/// Options for [`uninstall`].
#[derive(Debug, Clone, Default)]
pub struct UninstallOpts {
    pub delete_data: bool,
}

/// Result of an install, uninstall, or modify action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed { version: String },
    Modified { mode: ModelMode },
    Uninstalled { reboot_required: bool },
    NothingToDo,
    /// Uninstall handed over to a temp copy of the setup exe (see `cli`).
    Relaunched,
}

/// Runs the full installation flow.
pub fn install(ctx: &Ctx, exe: &Path, opts: &InstallOpts) -> Result<Outcome, InstallError> {
    install_with_progress(ctx, exe, opts, available_disk_space, &NoProgress)
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
    install_with_progress(ctx, exe, opts, free_space, &NoProgress)
}

/// Same as [`install_with_space`], with progress reporting and cooperative cancellation.
pub fn install_with_progress<F>(
    ctx: &Ctx,
    exe: &Path,
    opts: &InstallOpts,
    free_space: F,
    progress: &dyn Progress,
) -> Result<Outcome, InstallError>
where
    F: FnOnce(&Path) -> Result<u64, InstallError>,
{
    let _mutex = acquire_setup_mutex(&ctx.home)?;
    log_line(
        &ctx.home,
        &format!(
            "install start -> {} (models={})",
            ctx.install_dir.display(),
            opts.model_mode.as_str()
        ),
    );

    progress.step(Step::Verifying);
    if progress.cancelled() {
        log_line(&ctx.home, "install cancelled before verification");
        return Err(InstallError::Cancelled);
    }

    // The payload hash is checked once, by `extract_to_staging_with_progress`, before anything is
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

    let staged = extract_to_staging_with_progress(ctx, exe, &trailer, free_space, progress)?;
    let version = staged.manifest.version.clone();

    if progress.cancelled() {
        let _ = fs::remove_dir_all(&staged.dir);
        log_line(&ctx.home, "install cancelled after extraction");
        return Err(InstallError::Cancelled);
    }

    progress.step(Step::Closing);
    close_running(ctx)?;

    if progress.cancelled() {
        let _ = fs::remove_dir_all(&staged.dir);
        log_line(&ctx.home, "install cancelled before swap");
        return Err(InstallError::Cancelled);
    }

    progress.step(Step::Swapping);
    swap_into_place(ctx, staged)?;
    copy_stub_exe(exe, stub_len(&trailer), &ctx.install_dir)?;

    progress.step(Step::Registering);
    registry::register(ctx, &version)?;
    registry::write_model_mode(ctx, opts.model_mode)?;
    shortcut::create_shortcut(ctx)?;

    if ctx.start_after {
        let agent = ctx.install_dir.join(InstallLayout::AGENT_EXE);
        log_line(&ctx.home, &format!("starting {}", agent.display()));
        Command::new(&agent)
            .current_dir(&ctx.install_dir)
            .spawn()
            .map_err(|e| InstallError::io(format!("starting {}", agent.display()), e))?;
    }

    progress.step(Step::Done);
    log_line(&ctx.home, &format!("install complete: {version}"));
    Ok(Outcome::Installed { version })
}

/// Uninstalls BetterRecall from `ctx.install_dir`, optionally deleting allowlisted data in `ctx.home`.
pub fn uninstall(ctx: &Ctx, opts: &UninstallOpts) -> Result<Outcome, InstallError> {
    uninstall_with_progress(ctx, opts, &NoProgress)
}

/// Same as [`uninstall`], with progress reporting.
pub fn uninstall_with_progress(
    ctx: &Ctx,
    opts: &UninstallOpts,
    progress: &dyn Progress,
) -> Result<Outcome, InstallError> {
    if opts.delete_data {
        validate_delete_data_home(&ctx.home)?;
    }

    let _mutex = acquire_setup_mutex(&ctx.home)?;
    log_line(
        &ctx.home,
        &format!(
            "uninstall start: {} (delete_data={})",
            ctx.install_dir.display(),
            opts.delete_data
        ),
    );

    progress.step(Step::Closing);
    close_running(ctx)?;

    progress.step(Step::Unregistering);
    registry::unregister(ctx)?;
    shortcut::remove_shortcut(ctx)?;

    progress.step(Step::RemovingFiles);
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

    if opts.delete_data {
        progress.step(Step::DeletingData);
        delete_home_data(&ctx.home)?;
    }

    log_line(
        &ctx.home,
        &format!(
            "uninstall complete (reboot_required={reboot_required}, delete_data={})",
            opts.delete_data
        ),
    );

    if opts.delete_data {
        // Remove logs\ after the final log line is written; ignore failure.
        let _ = fs::remove_dir_all(DataPaths::logs_dir(&ctx.home));
        // Remove ctx.home only if it is now empty (leaves app\ or unknown files intact).
        let _ = fs::remove_dir(&ctx.home);
    }

    progress.step(Step::Done);
    Ok(Outcome::Uninstalled { reboot_required })
}

/// Modify action (I1 compatibility entry point; use [`modify_with_progress`] for I2+).
pub fn modify(ctx: &Ctx) -> Result<Outcome, InstallError> {
    log_line(&ctx.home, "modify: nothing to do in I1");
    Ok(Outcome::NothingToDo)
}

/// Records the chosen [`ModelMode`] in the registry (I3 will also download models here).
pub fn modify_with_progress(
    ctx: &Ctx,
    opts: &ModifyOpts,
    progress: &dyn Progress,
) -> Result<Outcome, InstallError> {
    let _mutex = acquire_setup_mutex(&ctx.home)?;
    log_line(
        &ctx.home,
        &format!("modify start (models={})", opts.model_mode.as_str()),
    );

    progress.step(Step::SavingSettings);
    registry::write_model_mode(ctx, opts.model_mode)?;

    progress.step(Step::Done);
    log_line(
        &ctx.home,
        &format!("modify complete (models={})", opts.model_mode.as_str()),
    );
    Ok(Outcome::Modified {
        mode: opts.model_mode,
    })
}

/// Refuses unsafe `home` paths before deleting BetterRecall data:
/// empty paths, drive/UNC roots, the user profile folder, and `%LOCALAPPDATA%` itself.
pub fn validate_delete_data_home(home: &Path) -> Result<(), InstallError> {
    if home.as_os_str().is_empty() {
        return Err(InstallError::Usage(
            "refusing to delete data: data folder path is empty".to_string(),
        ));
    }
    let normalized = full_path(home).map_err(|e| {
        InstallError::Usage(format!(
            "refusing to delete data from invalid path {}: {e}",
            home.display()
        ))
    })?;

    if !normalized
        .components()
        .any(|c| matches!(c, Component::Normal(_)))
    {
        return Err(InstallError::Usage(format!(
            "refusing to delete data from drive or root folder {}",
            normalized.display()
        )));
    }

    let norm_str = trim_trailing_seps(&normalized);

    let forbidden: [Option<PathBuf>; 4] = [
        known_folder(&FOLDERID_Profile),
        env::var_os("USERPROFILE").map(PathBuf::from),
        known_folder(&FOLDERID_LocalAppData),
        env::var_os("LOCALAPPDATA").map(PathBuf::from),
    ];
    for candidate in forbidden.into_iter().flatten() {
        let full = full_path(&candidate).unwrap_or(candidate);
        if norm_str.eq_ignore_ascii_case(&trim_trailing_seps(&full)) {
            return Err(InstallError::Usage(format!(
                "refusing to delete data from protected folder {}",
                normalized.display()
            )));
        }
    }

    Ok(())
}

fn delete_home_data(home: &Path) -> Result<(), InstallError> {
    validate_delete_data_home(home)?;
    if !home.exists() {
        return Ok(());
    }

    // Read custom models_dir from settings.json before removing settings.json.
    let custom_models_dir = read_custom_models_dir(home);

    let default_models_dir = DataPaths::models_dir(home, &Settings::default());
    let keys_dir = DataPaths::key_file(home)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join("keys"));
    let cache_dir = DataPaths::compiled_cache(home)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join("cache"));

    let files = [
        DataPaths::store_db(home),
        home.join("store.db-wal"),
        home.join("store.db-shm"),
        DataPaths::settings_file(home),
        DataPaths::agent_pid_file(home),
    ];
    for file in files {
        remove_file_if_exists(&file)?;
    }

    let dirs = [
        keys_dir,
        DataPaths::keyframes_dir(home),
        DataPaths::replays_dir(home),
        default_models_dir.clone(),
        cache_dir,
    ];
    for dir in dirs {
        remove_dir_all_if_exists(&dir)?;
    }

    if let Some(custom_models) = custom_models_dir {
        let home_prefix = normalized_dir_prefix(home);
        if path_is_under_prefix(&custom_models, &home_prefix)
            && !trim_trailing_seps(&custom_models)
                .eq_ignore_ascii_case(&trim_trailing_seps(&default_models_dir))
        {
            remove_dir_all_if_exists(&custom_models)?;
        }
    }

    Ok(())
}

fn read_custom_models_dir(home: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(DataPaths::settings_file(home)).ok()?;
    let stripped = strip_jsonc(&raw);
    let settings: Settings = serde_json::from_str(&stripped).ok()?;
    Some(DataPaths::models_dir(home, &settings))
}

fn remove_file_if_exists(path: &Path) -> Result<(), InstallError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(InstallError::io(
            format!("removing {}", path.display()),
            e,
        )),
    }
}

fn remove_dir_all_if_exists(path: &Path) -> Result<(), InstallError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(InstallError::io(
            format!("removing {}", path.display()),
            e,
        )),
    }
}

fn trim_trailing_seps(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string()
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let pwstr = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let mut len = 0usize;
        while *pwstr.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(pwstr.0, len);
        let s = OsString::from_wide(slice);
        CoTaskMemFree(Some(pwstr.0.cast()));
        Some(PathBuf::from(s))
    }
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
