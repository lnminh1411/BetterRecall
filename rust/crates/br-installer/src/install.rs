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
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use br_contracts::{DataPaths, InstallLayout, PipeNames, Settings, full_path, strip_jsonc};
use br_fetch::{
    Catalog, FetchError, FetchProgress, InstallOptions, ModelSet, install_components,
    remove_components_from_catalog,
};
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
    available_disk_space, extract_to_staging_with_progress_and_extra, read_install_marker,
};
use crate::log::log_line;
use crate::payload::{read_setup_manifest, read_trailer, stub_len};
use crate::progress::{NoProgress, Progress, Step};
use crate::registry;
use crate::running::{agent_running, close_running, normalized_dir_prefix, path_is_under_prefix};
use crate::shims;
use crate::shortcut;
use crate::swap::{old_folder_path, rename_with_retry, swap_into_place};

/// Installed filename of the payload-stripped setup binary.
pub const SETUP_EXE: &str = "BetterRecall-Setup.exe";

/// Base single-instance mutex name (scoped via `PipeNames::scoped` for non-default homes).
pub const SETUP_MUTEX: &str = PipeNames::SETUP_MUTEX;
/// Base single-instance setup lock name (B1: installer-vs-installer exclusion).
pub const SETUP_LOCK: &str = PipeNames::SETUP_LOCK;

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

    /// Maps `Full`/`Minimal` to the corresponding [`ModelSet`]; returns `None` for `Later`.
    pub fn to_model_set(self) -> Option<ModelSet> {
        match self {
            Self::Full => Some(ModelSet::Full),
            Self::Minimal => Some(ModelSet::Minimal),
            Self::Later => None,
        }
    }
}

/// Outcome of the model download step during install or modify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsResult {
    Complete,
    Skipped,
    Cancelled,
    Offline,
    Failed(String),
}

/// Options for [`install`].
#[derive(Debug, Clone, Default)]
pub struct InstallOpts {
    pub allow_downgrade: bool,
    pub model_mode: ModelMode,
    /// When true, records `model_mode` in the registry without downloading or removing models
    /// (used by silent upgrades when `/models=` was not passed).
    pub skip_model_download: bool,
    /// Optional catalog override for tests; production always leaves this `None` (uses embedded).
    pub catalog: Option<Catalog>,
    /// Optional public key PEM override for tests; `None` uses the compile-time release key (or skips).
    pub integrity_key: Option<String>,
}

/// Options for [`modify_with_progress`].
#[derive(Debug, Clone, Default)]
pub struct ModifyOpts {
    pub model_mode: ModelMode,
    /// Optional catalog override for tests; production always leaves this `None` (uses embedded).
    pub catalog: Option<Catalog>,
    /// Optional public key PEM override for tests; `None` uses the compile-time release key (or skips).
    pub integrity_key: Option<String>,
}

/// Options for [`uninstall`].
#[derive(Debug, Clone, Default)]
pub struct UninstallOpts {
    pub delete_data: bool,
    /// Optional public key PEM override for tests; `None` uses the compile-time release key (or skips).
    pub integrity_key: Option<String>,
}

/// Result of an install, uninstall, or modify action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed {
        version: String,
        models: ModelsResult,
    },
    Modified {
        mode: ModelMode,
        models: ModelsResult,
    },
    Uninstalled {
        reboot_required: bool,
    },
    NothingToDo,
    /// Uninstall handed over to a temp copy of the setup exe (see `cli`).
    Relaunched,
}

/// Resolves the models directory for `home`, honoring `settings.json` when present.
pub fn resolve_models_dir(home: &Path) -> PathBuf {
    read_custom_models_dir(home)
        .unwrap_or_else(|| DataPaths::models_dir(home, &Settings::default()))
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
    let outcome = install_with_progress_inner(ctx, exe, opts, free_space, progress);
    let exit_code = match &outcome {
        Ok(_) => crate::error::exit_code::OK,
        Err(e) => e.exit_code(),
    };
    let version = match &outcome {
        Ok(Outcome::Installed { version, .. }) => version.clone(),
        _ => read_trailer(exe)
            .and_then(|t| read_setup_manifest(exe, &t))
            .map(|m| m.version)
            .unwrap_or_default(),
    };
    // C1: report the result to the agent at the end of every install/upgrade run
    report_last_install(&ctx.home, &version, exit_code);
    outcome
}

fn install_with_progress_inner<F>(
    ctx: &Ctx,
    exe: &Path,
    opts: &InstallOpts,
    free_space: F,
    progress: &dyn Progress,
) -> Result<Outcome, InstallError>
where
    F: FnOnce(&Path) -> Result<u64, InstallError>,
{
    // B1: take SETUP_LOCK for installer-vs-installer exclusion
    let _lock = acquire_setup_lock(&ctx.home)?;
    // B1: take SETUP_MUTEX while app is replaced
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

    let model_plan = if opts.skip_model_download {
        None
    } else {
        match opts.model_mode.to_model_set() {
            Some(set) => {
                let cat = resolve_catalog(opts.catalog.as_ref())?;
                Some((cat, set))
            }
            None => None,
        }
    };
    let extra_model_bytes = model_plan
        .as_ref()
        .map_or(0, |(cat, set)| {
            // F2: count only components of the set that are not already installed and complete
            let models_dir = resolve_models_dir(&ctx.home);
            let states = br_fetch::installed_state(&models_dir, cat);
            cat.components_for(*set)
                .into_iter()
                .filter(|c| {
                    !states
                        .iter()
                        .any(|(id, st)| id == &c.id && *st == br_fetch::ComponentState::Complete)
                })
                .fold(0u64, |acc, c| acc.saturating_add(c.bytes))
        });

    let staged = extract_to_staging_with_progress_and_extra(
        ctx,
        exe,
        &trailer,
        extra_model_bytes,
        free_space,
        progress,
    )?;
    let version = staged.manifest.version.clone();

    if let Some(key_pem) = resolve_integrity_key(opts.integrity_key.as_deref())
        && let Err(e) = br_integrity::check_dir_with_exempt(&staged.dir, key_pem, &[SETUP_EXE])
    {
        let _ = fs::remove_dir_all(&staged.dir);
        log_line(
            &ctx.home,
            &format!("install integrity check failed on staging: {e}"),
        );
        return Err(InstallError::Integrity(e.to_string()));
    }

    if progress.cancelled() {
        let _ = fs::remove_dir_all(&staged.dir);
        log_line(&ctx.home, "install cancelled after extraction");
        return Err(InstallError::Cancelled);
    }

    progress.step(Step::Closing);
    let agent_was_running = close_running(ctx)?;
    // N2: any failure or cancel from here on restarts the agent that was running, so a
    // refused upgrade (folder in use, exit 3) does not leave BetterRecall stopped.
    let mut restart_on_exit = RestartAgentGuard {
        ctx,
        armed: agent_was_running,
        op: "install",
    };

    if progress.cancelled() {
        let _ = fs::remove_dir_all(&staged.dir);
        log_line(&ctx.home, "install cancelled before swap");
        return Err(InstallError::Cancelled);
    }

    shims::cleanup_launcher_installs(&ctx.home);

    progress.step(Step::Swapping);
    swap_into_place(ctx, staged)?;
    copy_stub_exe(exe, stub_len(&trailer), &ctx.install_dir)?;

    if let Some(key_pem) = resolve_integrity_key(opts.integrity_key.as_deref())
        && let Err(e) = br_integrity::check_single_file(&ctx.install_dir, key_pem, SETUP_EXE)
    {
        log_line(
            &ctx.home,
            &format!("install integrity check failed on {SETUP_EXE}: {e}"),
        );
        return Err(InstallError::Integrity(e.to_string()));
    }

    shims::place_relay_and_shim(ctx)?;

    progress.step(Step::Registering);
    registry::register(ctx, &version)?;
    registry::write_model_mode(ctx, opts.model_mode)?;
    shortcut::create_shortcut(ctx)?;

    let models = if let Some((ref cat, set)) = model_plan {
        progress.step(Step::DownloadingModels);
        download_models_set(&ctx.home, cat, set, progress)
    } else {
        ModelsResult::Skipped
    };

    restart_on_exit.armed = false;
    if ctx.start_after {
        start_agent(ctx)?;
    }

    progress.step(Step::Done);
    log_line(
        &ctx.home,
        &format!("install complete: {version} (models={models:?})"),
    );
    Ok(Outcome::Installed { version, models })
}

/// Restarts the agent from the install folder when an install that closed it does not finish.
struct RestartAgentGuard<'a> {
    ctx: &'a Ctx,
    armed: bool,
    op: &'static str,
}

impl Drop for RestartAgentGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let agent = self.ctx.install_dir.join(InstallLayout::AGENT_EXE);
        if !agent.is_file() {
            log_line(
                &self.ctx.home,
                &format!("{} did not finish; no agent to restart", self.op),
            );
            return;
        }
        log_line(
            &self.ctx.home,
            &format!(
                "{} did not finish; restarting the agent that was running",
                self.op
            ),
        );
        if let Err(e) = start_agent(self.ctx) {
            log_line(&self.ctx.home, &format!("agent restart failed: {e}"));
        }
    }
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

    // B1: take SETUP_LOCK for installer-vs-installer exclusion
    let _lock = acquire_setup_lock(&ctx.home)?;
    // B1: take SETUP_MUTEX while app is closed and removed
    let _mutex = acquire_setup_mutex(&ctx.home)?;
    log_line(
        &ctx.home,
        &format!(
            "uninstall start: {} (delete_data={})",
            ctx.install_dir.display(),
            opts.delete_data
        ),
    );

    if let Some(key_pem) = resolve_integrity_key(opts.integrity_key.as_deref())
        && let Err(e) = br_integrity::check_dir(&ctx.install_dir, key_pem)
    {
        log_line(
            &ctx.home,
            &format!("uninstall: install integrity check failed ({e}); continuing removal"),
        );
    }

    progress.step(Step::Closing);
    let agent_was_running = close_running(ctx)?;
    // C3: restart agent if rename fails
    let mut restart_on_exit = RestartAgentGuard {
        ctx,
        armed: agent_was_running,
        op: "uninstall",
    };

    progress.step(Step::RemovingFiles);
    let mut reboot_required = false;
    if ctx.install_dir.exists() {
        let self_inside = std::env::current_exe().ok().is_some_and(|me| {
            let prefix = normalized_dir_prefix(&ctx.install_dir);
            path_is_under_prefix(&me, &prefix)
        });

        if self_inside {
            schedule_or_delete_tree(ctx, &ctx.install_dir);
            reboot_required = ctx.install_dir.exists();
            restart_on_exit.armed = false;
            progress.step(Step::Unregistering);
            registry::unregister(ctx)?;
            shortcut::remove_shortcut(ctx)?;
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
            // Folder moved aside successfully: disarm agent restart
            restart_on_exit.armed = false;

            // C3: unregister and remove shortcut AFTER rename succeeds
            progress.step(Step::Unregistering);
            registry::unregister(ctx)?;
            shortcut::remove_shortcut(ctx)?;

            if let Err(e) = fs::remove_dir_all(&old_dir) {
                log_line(
                    &ctx.home,
                    &format!(
                        "uninstall: remove_dir_all failed on {}: {e}; scheduling on reboot",
                        old_dir.display()
                    ),
                );
                // F4: schedule cleanup with RunOnce fallback
                reboot_required = schedule_old_folder_cleanup(ctx, &old_dir);
            }
        }
    } else {
        restart_on_exit.armed = false;
        progress.step(Step::Unregistering);
        registry::unregister(ctx)?;
        shortcut::remove_shortcut(ctx)?;
    }

    shims::remove_relay_and_shim(ctx);

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

/// Updates the chosen [`ModelMode`] in the registry and downloads or removes model components.
pub fn modify_with_progress(
    ctx: &Ctx,
    opts: &ModifyOpts,
    progress: &dyn Progress,
) -> Result<Outcome, InstallError> {
    // B1: take SETUP_LOCK for installer-vs-installer exclusion (NOT SETUP_MUTEX)
    let _lock = acquire_setup_lock(&ctx.home)?;
    let prev_mode = registry::read_model_mode(ctx);
    log_line(
        &ctx.home,
        &format!(
            "modify start (prev={:?}, models={})",
            prev_mode.map(ModelMode::as_str),
            opts.model_mode.as_str()
        ),
    );

    if let Some(key_pem) = resolve_integrity_key(opts.integrity_key.as_deref())
        && let Err(e) = br_integrity::check_dir(&ctx.install_dir, key_pem)
    {
        log_line(
            &ctx.home,
            &format!("modify refused: install integrity check failed: {e}"),
        );
        return Err(InstallError::Integrity(e.to_string()));
    }

    progress.step(Step::SavingSettings);
    registry::write_model_mode(ctx, opts.model_mode)?;
    if ctx.install_dir.is_dir() {
        shims::place_relay_and_shim(ctx)?;
    }

    let mut restart_agent = false;
    let models = match opts.model_mode.to_model_set() {
        None => ModelsResult::Skipped,
        Some(set) => {
            let cat = resolve_catalog(opts.catalog.as_ref())?;
            let models_dir = resolve_models_dir(&ctx.home);

            // C6: Modify to Minimal removes full-only components present on disk regardless of prev_mode
            if opts.model_mode == ModelMode::Minimal {
                let full_only: Vec<&str> = cat
                    .components_for(ModelSet::Full)
                    .into_iter()
                    .filter(|c| !c.sets.iter().any(|s| s == ModelSet::Minimal.as_str()))
                    .map(|c| c.id.as_str())
                    .filter(|id| models_dir.join(id).exists())
                    .collect();
                if !full_only.is_empty() {
                    // B1: hold SETUP_MUTEX only around close_running, model removal and restart
                    let _mutex = acquire_setup_mutex(&ctx.home)?;
                    if close_running(ctx)? {
                        restart_agent = true;
                    }
                    log_line(
                        &ctx.home,
                        &format!(
                            "modify: removing full-only models {:?} from {}",
                            full_only,
                            models_dir.display()
                        ),
                    );
                    if let Err(e) = remove_components_from_catalog(&models_dir, &cat, &full_only) {
                        log_line(&ctx.home, &format!("modify: failed to remove models: {e}"));
                        if restart_agent && ctx.start_after {
                            start_agent(ctx)?;
                        }
                        progress.step(Step::Done);
                        return Ok(Outcome::Modified {
                            mode: opts.model_mode,
                            models: ModelsResult::Failed(e.to_string()),
                        });
                    }
                    if restart_agent && ctx.start_after {
                        start_agent(ctx)?;
                        restart_agent = false;
                    }
                }
            }

            // B1: download_models_set runs WITHOUT holding SETUP_MUTEX
            progress.step(Step::DownloadingModels);
            let result = download_models_set(&ctx.home, &cat, set, progress);
            // A running worker loads models at start: restart the app so it picks up new ones.
            if result == ModelsResult::Complete && !restart_agent && agent_running(ctx) {
                // B1: hold SETUP_MUTEX around restart
                let _mutex = acquire_setup_mutex(&ctx.home)?;
                if close_running(ctx)? && ctx.start_after {
                    start_agent(ctx)?;
                }
            }
            result
        }
    };
    if restart_agent && ctx.start_after {
        let _mutex = acquire_setup_mutex(&ctx.home)?;
        start_agent(ctx)?;
    }

    progress.step(Step::Done);
    log_line(
        &ctx.home,
        &format!(
            "modify complete (models={}, result={models:?})",
            opts.model_mode.as_str()
        ),
    );
    Ok(Outcome::Modified {
        mode: opts.model_mode,
        models,
    })
}

fn resolve_integrity_key(override_key: Option<&str>) -> Option<&str> {
    match override_key {
        Some(k) => Some(k),
        None => br_integrity::embedded_public_key_pem(),
    }
}

fn start_agent(ctx: &Ctx) -> Result<(), InstallError> {
    let agent = ctx.install_dir.join(InstallLayout::AGENT_EXE);
    log_line(&ctx.home, &format!("starting {}", agent.display()));
    Command::new(&agent)
        .current_dir(&ctx.install_dir)
        .spawn()
        .map_err(|e| InstallError::io(format!("starting {}", agent.display()), e))?;
    Ok(())
}

fn resolve_catalog(override_cat: Option<&Catalog>) -> Result<Catalog, InstallError> {
    match override_cat {
        Some(c) => Ok(c.clone()),
        None => Catalog::embedded().map_err(|e| InstallError::Models(e.to_string())),
    }
}

struct InstallerFetchProgress<'a> {
    progress: &'a dyn Progress,
    done: AtomicU64,
    total: AtomicU64,
}

impl FetchProgress for InstallerFetchProgress<'_> {
    fn total(&self, bytes: u64) {
        self.total.store(bytes, AtomicOrdering::Relaxed);
        self.progress.bytes(0, bytes);
    }

    fn bytes(&self, delta: u64) {
        let done = self.done.fetch_add(delta, AtomicOrdering::Relaxed) + delta;
        let total = self.total.load(AtomicOrdering::Relaxed);
        self.progress.bytes(done, total);
    }
}

fn download_models_set(
    home: &Path,
    catalog: &Catalog,
    set: ModelSet,
    progress: &dyn Progress,
) -> ModelsResult {
    let models_dir = resolve_models_dir(home);
    let components = catalog.components_for(set);
    let fetch_opts = InstallOptions::for_catalog(catalog);
    let bridge = InstallerFetchProgress {
        progress,
        done: AtomicU64::new(0),
        total: AtomicU64::new(0),
    };
    log_line(
        home,
        &format!(
            "downloading {} model components ({} bytes) into {}",
            components.len(),
            catalog.bytes_for(set),
            models_dir.display()
        ),
    );
    match install_components(
        &models_dir,
        &components,
        &fetch_opts,
        &bridge,
        &|| progress.cancelled(),
    ) {
        Ok(report) => {
            log_line(
                home,
                &format!(
                    "model download complete: downloaded={}, skipped={}, bytes={}",
                    report.downloaded_files, report.skipped_files, report.bytes_downloaded
                ),
            );
            ModelsResult::Complete
        }
        Err(FetchError::Offline) => {
            log_line(home, "model download skipped: offline");
            ModelsResult::Offline
        }
        Err(FetchError::Cancelled) => {
            log_line(home, "model download cancelled by user");
            ModelsResult::Cancelled
        }
        Err(e) => {
            log_line(home, &format!("model download failed: {e}"));
            ModelsResult::Failed(e.to_string())
        }
    }
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
        home.join("settings.json.bad"), // C8
        home.join("exports.pending"),   // C8
        DataPaths::agent_pid_file(home),
    ];
    for file in files {
        remove_file_if_exists(&file)?;
    }

    // C8: check DataPaths::updates_dir; skip if cache is already removed whole
    let updates_dir = DataPaths::updates_dir(home);
    if updates_dir.exists() {
        remove_dir_all_if_exists(&updates_dir)?;
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

    // C8: remove <home>\app\bin\*.old-* relay copies (best effort; locked logged and skipped)
    let bin_dir = home.join("app").join("bin");
    if bin_dir.is_dir() {
        shims::sweep_old_files(&bin_dir, home);
        let _ = fs::remove_dir(&bin_dir);
        let _ = fs::remove_dir(home.join("app"));
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

// F4: schedule cleanup for old install folder left behind, with RunOnce fallback
pub fn schedule_old_folder_cleanup(ctx: &Ctx, old_dir: &Path) -> bool {
    let mut dirs_postorder = Vec::new();
    let all_scheduled = collect_and_delete(ctx, old_dir, &mut dirs_postorder);
    let mut dirs_scheduled = true;
    for d in dirs_postorder {
        if fs::remove_dir(&d).is_err() && !schedule_delete_on_reboot(ctx, &d) {
            dirs_scheduled = false;
        }
    }
    if !old_dir.exists() {
        return false;
    }
    if all_scheduled && dirs_scheduled {
        return true;
    }
    // F4: MoveFileExW failed; fall back to HKCU RunOnce BetterRecallCleanup
    let is_old_folder = old_dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(crate::swap::OLD_PREFIX));
    let is_parent_match = old_dir.parent() == ctx.install_dir.parent();
    if is_old_folder && is_parent_match {
        let cmd = format!("cmd.exe /d /c rmdir /s /q \"{}\"", old_dir.display());
        match registry::write_run_once_cleanup(ctx, &cmd) {
            Ok(()) => {
                log_line(
                    &ctx.home,
                    &format!(
                        "uninstall: scheduled RunOnce cleanup for {}",
                        old_dir.display()
                    ),
                );
                return true;
            }
            Err(e) => {
                log_line(
                    &ctx.home,
                    &format!("uninstall: failed to write RunOnce cleanup: {e}"),
                );
            }
        }
    }
    log_line(
        &ctx.home,
        &format!(
            "uninstall: folder {} left behind (could not schedule cleanup)",
            old_dir.display()
        ),
    );
    false
}

fn schedule_or_delete_tree(ctx: &Ctx, dir: &Path) {
    let mut dirs_postorder = Vec::new();
    collect_and_delete(ctx, dir, &mut dirs_postorder);
    for d in dirs_postorder {
        if fs::remove_dir(&d).is_err() {
            schedule_delete_on_reboot(ctx, &d);
        }
    }
}

fn collect_and_delete(ctx: &Ctx, dir: &Path, dirs_postorder: &mut Vec<PathBuf>) -> bool {
    let mut all_ok = true;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                if !collect_and_delete(ctx, &path, dirs_postorder) {
                    all_ok = false;
                }
            } else if fs::remove_file(&path).is_err() && !schedule_delete_on_reboot(ctx, &path) {
                all_ok = false;
            }
        }
    }
    dirs_postorder.push(dir.to_path_buf());
    all_ok
}

fn schedule_delete_on_reboot(ctx: &Ctx, path: &Path) -> bool {
    // In dev / test prefix runs, never touch real HKLM PendingFileRenameOperations.
    if !ctx.reg_prefix.is_empty() {
        return false;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let res = unsafe { MoveFileExW(PCWSTR(wide.as_ptr()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    if let Err(e) = res {
        log_line(
            &ctx.home,
            &format!(
                "MoveFileExW(DELAY_UNTIL_REBOOT) failed for {}: {e}",
                path.display()
            ),
        );
        false
    } else {
        true
    }
}

// B1: acquire_setup_lock for installer-vs-installer exclusion
fn acquire_setup_lock(home: &Path) -> Result<OwnedMutex, InstallError> {
    let name = PipeNames::setup_lock_for(home);
    let wide: Vec<u16> = OsString::from(&name).encode_wide().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }.map_err(|e| {
        InstallError::io(
            format!("creating setup lock '{name}'"),
            io::Error::from_raw_os_error(e.code().0),
        )
    })?;
    let last = unsafe { GetLastError() };
    if last == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        // N4: setup.log line `setup already running (mutex held)`; make sure the log is opened before that check so the line lands.
        log_line(home, "setup already running (mutex held)");
        return Err(InstallError::AlreadyRunning);
    }
    Ok(OwnedMutex(handle))
}

fn acquire_setup_mutex(home: &Path) -> Result<OwnedMutex, InstallError> {
    let name = PipeNames::setup_mutex_for(home);
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
        log_line(home, "setup already running (mutex held)");
        return Err(InstallError::AlreadyRunning);
    }
    Ok(OwnedMutex(handle))
}

// C1 installer half: report result to agent
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LastInstallReport<'a> {
    version: &'a str,
    exit_code: i32,
    utc: String,
}

pub fn report_last_install(home: &Path, version: &str, exit_code: i32) {
    let updates_dir = DataPaths::updates_dir(home);
    if fs::create_dir_all(&updates_dir).is_err() {
        return;
    }
    let target = updates_dir.join("last-install.json");
    let tmp = updates_dir.join(format!("last-install-{}.tmp", std::process::id()));
    let report = LastInstallReport {
        version,
        exit_code,
        utc: crate::log::utc_now(),
    };
    let Ok(json) = serde_json::to_string(&report) else {
        return;
    };
    if fs::write(&tmp, json.as_bytes()).is_err() {
        return;
    }
    if fs::rename(&tmp, &target).is_err() {
        let _ = fs::remove_file(&target);
        let _ = fs::rename(&tmp, &target);
    }
    let _ = fs::remove_file(&tmp);
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
