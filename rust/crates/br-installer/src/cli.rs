// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CLI argument parsing and non-silent `MessageBoxW` reporting.

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::PCWSTR;

use crate::ctx::Ctx;
use crate::error::{InstallError, exit_code};
use crate::install::{InstallOpts, Outcome, UninstallOpts, install, modify, uninstall};
use crate::running::{normalized_dir_prefix, path_is_under_prefix};

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
    pub dev_root: Option<PathBuf>,
    pub dev_reg_prefix: Option<String>,
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
    let mut dev_root: Option<PathBuf> = None;
    let mut dev_reg_prefix: Option<String> = None;
    let mut relaunched = false;

    let mut iter = args.into_iter();
    while let Some(raw) = iter.next() {
        let arg = raw.as_ref();
        let flag = strip_flag_prefix(arg)
            .ok_or_else(|| InstallError::Usage(format!("unexpected argument: {arg}")))?;
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

    Ok(CliArgs {
        silent,
        action: action.unwrap_or(Action::Install),
        no_run,
        allow_downgrade,
        dev_root,
        dev_reg_prefix,
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

    let result = dispatch(&args, &raw_args);
    match result {
        Ok(outcome) => {
            if !args.silent {
                let msg = match outcome {
                    Outcome::Installed { version } => {
                        format!("BetterRecall {version} has been installed.")
                    }
                    Outcome::Uninstalled {
                        reboot_required: true,
                    } => "BetterRecall has been uninstalled. A reboot is needed to remove the remaining setup files.".to_string(),
                    Outcome::Uninstalled {
                        reboot_required: false,
                    } => "BetterRecall has been uninstalled.".to_string(),
                    Outcome::NothingToDo => "Nothing to modify.".to_string(),
                    // The temp copy carries on and reports; this process only started it.
                    Outcome::Relaunched => return exit_code::OK,
                };
                show_message(&msg, MB_OK | MB_ICONINFORMATION);
            }
            exit_code::OK
        }
        Err(e) => {
            if !args.silent {
                show_message(&e.to_string(), MB_OK | MB_ICONERROR);
            }
            e.exit_code()
        }
    }
}

fn dispatch(args: &CliArgs, raw_args: &[String]) -> Result<Outcome, InstallError> {
    let mut ctx = match (&args.dev_root, &args.dev_reg_prefix) {
        (Some(root), Some(prefix)) => Ctx::from_dev_root(root, prefix)?,
        _ => Ctx::production(),
    };
    if args.no_run {
        ctx.start_after = false;
    }

    match args.action {
        Action::Install => {
            let exe = std::env::current_exe()
                .map_err(|e| InstallError::io("resolving current executable path", e))?;
            install(
                &ctx,
                &exe,
                &InstallOpts {
                    allow_downgrade: args.allow_downgrade,
                },
            )
        }
        Action::Uninstall => {
            // A running exe cannot be deleted, and a per-user process may not schedule a delete
            // at reboot. So the installed BetterRecall-Setup.exe copies itself out of the install
            // folder and lets that copy uninstall; the copy is left in the temp folder.
            if !args.relaunched
                && let Ok(me) = std::env::current_exe()
                && path_is_under_prefix(&me, &normalized_dir_prefix(&ctx.install_dir))
            {
                relaunch_from_temp(&me, raw_args)?;
                return Ok(Outcome::Relaunched);
            }
            uninstall(&ctx, &UninstallOpts::default())
        }
        Action::Modify => modify(&ctx),
    }
}

fn relaunch_from_temp(me: &Path, raw_args: &[String]) -> Result<(), InstallError> {
    let copy = std::env::temp_dir().join(format!(
        "BetterRecall-uninstall-{}.exe",
        std::process::id()
    ));
    std::fs::copy(me, &copy)
        .map_err(|e| InstallError::io(format!("copying the uninstaller to {}", copy.display()), e))?;
    Command::new(&copy)
        .args(raw_args)
        .arg(RELAUNCHED_FLAG)
        .current_dir(std::env::temp_dir())
        .spawn()
        .map_err(|e| InstallError::io(format!("starting {}", copy.display()), e))?;
    Ok(())
}

fn show_message(text: &str, style: MESSAGEBOX_STYLE) {
    let title: Vec<u16> = OsString::from("BetterRecall Setup")
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
