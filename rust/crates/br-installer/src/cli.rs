// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CLI argument parsing, silent execution, and interactive Aero Wizard dispatch.

use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use br_fetch::Catalog;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_OK, MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::PCWSTR;

use crate::ctx::Ctx;
use crate::error::{InstallError, exit_code};
use crate::install::{
    InstallOpts, ModelMode, ModelsResult, ModifyOpts, Outcome, UninstallOpts,
    detect_existing_model_mode, effective_model_mode, install, modify_with_progress, uninstall,
};
use crate::log::log_line;
use crate::progress::NoProgress;
use crate::running::{normalized_dir_prefix, path_is_under_prefix};
use crate::strings;
use crate::wizard;

/// Requested installer action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Uninstall,
    Modify,
}

/// Parsed command-line arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliArgs {
    pub silent: bool,
    pub action: Action,
    pub no_run: bool,
    pub allow_downgrade: bool,
    pub model_mode: Option<ModelMode>,
    pub delete_data: bool,
    pub dev_root: Option<PathBuf>,
    pub dev_reg_prefix: Option<String>,
    /// Test/dev seam: path to a catalog JSON file (only allowed with `--dev-root`).
    pub dev_models_catalog: Option<PathBuf>,
    /// Test/dev seam: local loopback base URL override (only allowed with `--dev-root`).
    pub dev_models_url: Option<String>,
    /// Internal: this process is the temp copy started by an uninstall from inside the install folder.
    pub relaunched: bool,
}

/// Internal flag passed to the temp copy of the uninstaller.
pub const RELAUNCHED_FLAG: &str = "--relaunched";

/// Parses arguments (without `argv[0]`).
pub fn parse_args<I, S>(args: I) -> Result<CliArgs, InstallError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut silent = false;
    let mut action: Option<Action> = None;
    let mut no_run = false;
    let mut allow_downgrade = false;
    let mut model_mode: Option<ModelMode> = None;
    let mut delete_data = false;
    let mut dev_root: Option<PathBuf> = None;
    let mut dev_reg_prefix: Option<String> = None;
    let mut dev_models_catalog: Option<PathBuf> = None;
    let mut dev_models_url: Option<String> = None;
    let mut relaunched = false;

    let mut iter = args.into_iter();
    while let Some(raw) = iter.next() {
        let arg = raw.as_ref();
        let flag = strip_flag_prefix(arg)
            .ok_or_else(|| InstallError::Usage(format!("unexpected argument: {arg}")))?;

        if let Some((name, val)) = flag.split_once('=')
            && name.eq_ignore_ascii_case("models")
        {
            let parsed = ModelMode::parse(val).ok_or_else(|| {
                InstallError::Usage(format!(
                    "invalid /models value '{val}' (expected full, minimal, or later)"
                ))
            })?;
            model_mode = Some(parsed);
            continue;
        }

        match flag.to_ascii_lowercase().as_str() {
            "s" => silent = true,
            "uninstall" => {
                if action.is_some_and(|a| a != Action::Uninstall) {
                    return Err(InstallError::Usage(
                        "cannot combine /uninstall and /modify".to_string(),
                    ));
                }
                action = Some(Action::Uninstall);
            }
            "modify" => {
                if action.is_some_and(|a| a != Action::Modify) {
                    return Err(InstallError::Usage(
                        "cannot combine /uninstall and /modify".to_string(),
                    ));
                }
                action = Some(Action::Modify);
            }
            "norun" => no_run = true,
            "allowdowngrade" => allow_downgrade = true,
            "deletedata" => delete_data = true,
            "relaunched" => relaunched = true,
            "dev-root" => {
                let val = iter.next().ok_or_else(|| {
                    InstallError::Usage("--dev-root requires a directory path".to_string())
                })?;
                dev_root = Some(PathBuf::from(val.as_ref()));
            }
            "dev-reg-prefix" => {
                let val = iter.next().ok_or_else(|| {
                    InstallError::Usage("--dev-reg-prefix requires a registry key path".to_string())
                })?;
                dev_reg_prefix = Some(val.as_ref().to_string());
            }
            "dev-models-catalog" => {
                let val = iter.next().ok_or_else(|| {
                    InstallError::Usage("--dev-models-catalog requires a file path".to_string())
                })?;
                dev_models_catalog = Some(PathBuf::from(val.as_ref()));
            }
            "dev-models-url" => {
                let val = iter.next().ok_or_else(|| {
                    InstallError::Usage("--dev-models-url requires a URL".to_string())
                })?;
                dev_models_url = Some(val.as_ref().to_string());
            }
            _ => {
                return Err(InstallError::Usage(format!("unknown flag: {arg}")));
            }
        }
    }

    if dev_root.is_some() != dev_reg_prefix.is_some() {
        return Err(InstallError::Usage(
            "--dev-root and --dev-reg-prefix must be supplied together".to_string(),
        ));
    }
    if (dev_models_catalog.is_some() || dev_models_url.is_some()) && dev_root.is_none() {
        return Err(InstallError::Usage(
            "--dev-models-catalog and --dev-models-url require --dev-root and --dev-reg-prefix"
                .to_string(),
        ));
    }
    if let Some(ref url) = dev_models_url
        && !br_fetch::is_allowed_url(url)
    {
        return Err(InstallError::Usage(format!(
            "--dev-models-url '{url}' is not an allowed URL"
        )));
    }

    let resolved_action = action.unwrap_or(Action::Install);
    if delete_data && resolved_action != Action::Uninstall {
        return Err(InstallError::Usage(
            "/deletedata can only be used with /uninstall".to_string(),
        ));
    }
    if model_mode.is_some() && resolved_action == Action::Uninstall {
        return Err(InstallError::Usage(
            "/models cannot be used with /uninstall".to_string(),
        ));
    }

    Ok(CliArgs {
        silent,
        action: resolved_action,
        no_run,
        allow_downgrade,
        model_mode,
        delete_data,
        dev_root,
        dev_reg_prefix,
        dev_models_catalog,
        dev_models_url,
        relaunched,
    })
}

fn strip_flag_prefix(arg: &str) -> Option<&str> {
    arg.strip_prefix("--")
        .or_else(|| arg.strip_prefix('/'))
        .or_else(|| arg.strip_prefix('-'))
}

/// Entry point called by `main.rs`. Returns the process exit code.
pub fn run_main() -> i32 {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&raw_args) {
        Ok(a) => a,
        Err(e) => {
            if !raw_args.iter().any(|a| {
                strip_flag_prefix(a).is_some_and(|f| f.eq_ignore_ascii_case("s"))
            }) {
                show_message(&e.to_string(), MB_OK | MB_ICONERROR);
            }
            return e.exit_code();
        }
    };

    let ctx = match build_ctx(&args) {
        Ok(c) => c,
        Err(e) => {
            if !args.silent {
                show_message(&e.to_string(), MB_OK | MB_ICONERROR);
            }
            return e.exit_code();
        }
    };

    if args.action == Action::Modify
        && let Some(key_pem) = br_integrity::embedded_public_key_pem()
        && let Err(e) = br_integrity::check_dir(&ctx.install_dir, key_pem)
    {
        let err = InstallError::Integrity(e.to_string());
        log_line(
            &ctx.home,
            &format!("modify refused before UI: install integrity check failed: {e}"),
        );
        if !args.silent {
            show_message(&err.to_string(), MB_OK | MB_ICONERROR);
        }
        return err.exit_code();
    }

    if args.action == Action::Uninstall && should_relaunch_uninstall(&ctx, &args) {
        let me = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                let err = InstallError::io("resolving current executable path", e);
                if !args.silent {
                    show_message(&err.to_string(), MB_OK | MB_ICONERROR);
                }
                return err.exit_code();
            }
        };
        if let Err(e) = relaunch_from_temp(&ctx.temp_dir, &ctx.home, &me, &raw_args) {
            if !args.silent {
                show_message(&e.to_string(), MB_OK | MB_ICONERROR);
            }
            return e.exit_code();
        }
        return exit_code::OK;
    }

    if args.relaunched
        && let Ok(me) = std::env::current_exe()
    {
        // C5: delete stale temp uninstall copies other than itself at start
        sweep_stale_uninstall_temp(&ctx.temp_dir, &me);
    }

    let code = if !args.silent {
        wizard::run_wizard(ctx.clone(), args.clone())
    } else {
        match dispatch_silent(&ctx, &args) {
            Ok(_) => exit_code::OK,
            Err(e) => e.exit_code(),
        }
    };

    if args.relaunched {
        // C5: schedule self-deletion at end of temp-copy run
        schedule_self_delete(&ctx.temp_dir);
    }

    code
}

fn build_ctx(args: &CliArgs) -> Result<Ctx, InstallError> {
    let mut ctx = match (&args.dev_root, &args.dev_reg_prefix) {
        (Some(root), Some(prefix)) => Ctx::from_dev_root(root, prefix)?,
        _ => Ctx::production(),
    };
    if args.no_run {
        ctx.start_after = false;
    }
    Ok(ctx)
}

fn should_relaunch_uninstall(ctx: &Ctx, args: &CliArgs) -> bool {
    // A running exe cannot be deleted, and a per-user process may not schedule a delete
    // at reboot. So the installed BetterRecall-Setup.exe copies itself out of the install
    // folder and lets that copy uninstall; the copy is left in `ctx.temp_dir`.
    !args.relaunched
        && std::env::current_exe()
            .ok()
            .is_some_and(|me| path_is_under_prefix(&me, &normalized_dir_prefix(&ctx.install_dir)))
}

pub(crate) fn load_dev_catalog(args: &CliArgs) -> Result<Option<Catalog>, InstallError> {
    match (&args.dev_models_catalog, &args.dev_models_url) {
        (None, None) => Ok(None),
        (Some(path), url_opt) => {
            let raw = fs::read_to_string(path).map_err(|e| {
                InstallError::io(format!("reading dev catalog {}", path.display()), e)
            })?;
            let mut cat =
                Catalog::from_json(&raw).map_err(|e| InstallError::Models(e.to_string()))?;
            if let Some(url) = url_opt {
                cat = cat.with_base_url_for_test(url.as_str());
            }
            Ok(Some(cat))
        }
        (None, Some(url)) => {
            let cat = Catalog::embedded()
                .map_err(|e| InstallError::Models(e.to_string()))?
                .with_base_url_for_test(url.as_str());
            Ok(Some(cat))
        }
    }
}

fn dispatch_silent(ctx: &Ctx, args: &CliArgs) -> Result<Outcome, InstallError> {
    let exe = std::env::current_exe()
        .map_err(|e| InstallError::io("resolving current executable path", e))?;
    dispatch_silent_for_exe(ctx, args, &exe)
}

/// Runs the silent CLI action using an explicit installer executable path (exposed for tests).
pub fn dispatch_silent_for_exe(
    ctx: &Ctx,
    args: &CliArgs,
    exe: &Path,
) -> Result<Outcome, InstallError> {
    let dev_catalog = load_dev_catalog(args)?;
    let outcome = match args.action {
        Action::Install => {
            let (model_mode, skip_model_download) = match args.model_mode {
                Some(explicit) => (explicit, false),
                None => match detect_existing_model_mode(ctx, dev_catalog.as_ref()) {
                    Some(existing) => (existing, true),
                    None => (ModelMode::Later, false),
                },
            };
            install(
                ctx,
                exe,
                &InstallOpts {
                    allow_downgrade: args.allow_downgrade,
                    model_mode,
                    skip_model_download,
                    catalog: dev_catalog,
                    integrity_key: None,
                },
            )?
        }
        Action::Uninstall => uninstall(
            ctx,
            &UninstallOpts {
                delete_data: args.delete_data,
                integrity_key: None,
            },
        )?,
        Action::Modify => {
            // C2: without /models=, use the effective mode from disk + registry so nothing is removed.
            let model_mode = args
                .model_mode
                .unwrap_or_else(|| effective_model_mode(ctx, dev_catalog.as_ref()));
            modify_with_progress(
                ctx,
                &ModifyOpts {
                    model_mode,
                    catalog: dev_catalog,
                    integrity_key: None,
                },
                &NoProgress,
            )?
        }
    };

    match &outcome {
        Outcome::Installed { models, .. } | Outcome::Modified { models, .. } => match models {
            ModelsResult::Complete | ModelsResult::Skipped => Ok(outcome),
            ModelsResult::Offline => {
                log_line(&ctx.home, "silent run finished with models offline (exit 0)");
                Ok(outcome)
            }
            ModelsResult::Cancelled => {
                log_line(
                    &ctx.home,
                    "silent run finished with model download cancelled (exit 0)",
                );
                Ok(outcome)
            }
            ModelsResult::Failed(reason) => {
                log_line(
                    &ctx.home,
                    &format!("silent run finished with model download failure (exit 9): {reason}"),
                );
                Err(InstallError::Models(reason.clone()))
            }
        },
        _ => Ok(outcome),
    }
}

fn relaunch_from_temp(
    temp_dir: &Path,
    home: &Path,
    me: &Path,
    raw_args: &[String],
) -> Result<(), InstallError> {
    fs::create_dir_all(temp_dir).map_err(|e| {
        InstallError::io(
            format!("creating temp folder {}", temp_dir.display()),
            e,
        )
    })?;
    let copy = temp_dir.join(format!(
        "BetterRecall-uninstall-{}.exe",
        std::process::id()
    ));
    fs::copy(me, &copy)
        .map_err(|e| InstallError::io(format!("copying the uninstaller to {}", copy.display()), e))?;
    // C5: The parent cannot wait for the temp copy (its own image locks the install folder).
    // The exit code cannot be returned because the parent process must exit immediately
    // so its executable handle is closed, allowing the uninstaller to rename/remove the folder.
    log_line(home, &format!("uninstall handed over to {}", copy.display()));
    Command::new(&copy)
        .args(raw_args)
        .arg(RELAUNCHED_FLAG)
        .current_dir(temp_dir)
        .spawn()
        .map_err(|e| InstallError::io(format!("starting {}", copy.display()), e))?;
    Ok(())
}

/// Matches the exact name pattern `BetterRecall-uninstall-<pid>.exe` (C5).
pub fn is_uninstall_temp_file_name(name: &str) -> bool {
    name.strip_prefix("BetterRecall-uninstall-")
        .and_then(|rest| rest.strip_suffix(".exe"))
        .is_some_and(|pid_str| !pid_str.is_empty() && pid_str.chars().all(|c| c.is_ascii_digit()))
}

/// Sweeps stale `%TEMP%\BetterRecall-uninstall-*.exe` files other than itself (C5).
pub fn sweep_stale_uninstall_temp(temp_dir: &Path, current_exe: &Path) {
    let Ok(entries) = fs::read_dir(temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && is_uninstall_temp_file_name(name)
            && path != current_exe
        {
            let _ = fs::remove_file(&path);
        }
    }
}

fn schedule_self_delete(temp_dir: &Path) {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let Some(file_name) = me.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if !is_uninstall_temp_file_name(file_name) {
        return;
    }
    let is_in_temp = me.parent().is_some_and(|p| {
        p == temp_dir || std::env::var_os("TEMP").map(PathBuf::from).as_deref() == Some(p)
    });
    if !is_in_temp {
        return;
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let raw_args = format!("/d /c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"", me.display());
    let _ = Command::new("cmd.exe")
        .raw_arg(raw_args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

fn show_message(text: &str, style: MESSAGEBOX_STYLE) {
    let title: Vec<u16> = OsString::from(strings::WINDOW_TITLE)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let body: Vec<u16> = OsString::from(text).encode_wide().chain(Some(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            Some(HWND::default()),
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
        );
    }
}
