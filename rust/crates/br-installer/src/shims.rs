// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Stable MCP relay copy, CLI shim placement/removal under `<home>\app\bin`,
//! and launcher-era install cleanup under `<home>\app`.

use std::fs;
use std::path::{Path, PathBuf};

use br_contracts::{DataPaths, InstallLayout};

use crate::InstallError;
use crate::ctx::Ctx;
use crate::log::log_line;

const OLD_SUFFIX: &str = ".old-";
const TMP_PREFIX: &str = ".tmp-";
const LEGACY_CURRENT: &str = "current";
const LEGACY_AUTOSTART_EXE: &str = "BetterRecall.exe";

/// Returns `<home>\app`.
pub fn app_dir(home: &Path) -> PathBuf {
    home.join("app")
}

/// Returns `<home>\app\bin`.
pub fn bin_dir(home: &Path) -> PathBuf {
    app_dir(home).join("bin")
}

/// Returns `<home>\app\bin\betterrecall-mcp.exe`.
pub fn relay_path(home: &Path) -> PathBuf {
    DataPaths::mcp_relay(home)
}

/// Returns `<home>\app\bin\betterrecall.cmd`.
pub fn cli_shim_path(home: &Path) -> PathBuf {
    bin_dir(home).join(InstallLayout::CLI_SHIM)
}

/// Copies `<install>\betterrecall-mcp.exe` to `<home>\app\bin\betterrecall-mcp.exe` (renaming
/// an in-use destination to `betterrecall-mcp.exe.old-<n>` first), sweeps stale `.old-*` files
/// in `<home>\app\bin`, and writes `<home>\app\bin\betterrecall.cmd`.
pub fn place_relay_and_shim(ctx: &Ctx) -> Result<(), InstallError> {
    let bin = bin_dir(&ctx.home);
    fs::create_dir_all(&bin).map_err(|e| {
        InstallError::io(
            format!("creating relay/shim folder {}", bin.display()),
            e,
        )
    })?;

    sweep_old_files(&bin, &ctx.home);

    let src_mcp = ctx.install_dir.join(InstallLayout::MCP_EXE);
    if src_mcp.is_file() {
        let dst_mcp = bin.join(InstallLayout::MCP_EXE);
        copy_with_rename_aside(&src_mcp, &dst_mcp, &ctx.home)?;
        log_line(
            &ctx.home,
            &format!("shims: updated MCP relay {}", dst_mcp.display()),
        );
    } else {
        log_line(
            &ctx.home,
            &format!(
                "shims: {} not present in install dir; skipped relay copy",
                InstallLayout::MCP_EXE
            ),
        );
    }

    let cli_exe = ctx.install_dir.join(InstallLayout::CLI_EXE);
    let shim_content = format!("@\"{}\" %*\r\n", cli_exe.display());
    let dst_shim = bin.join(InstallLayout::CLI_SHIM);
    write_with_rename_aside(&dst_shim, shim_content.as_bytes(), &ctx.home)?;
    log_line(
        &ctx.home,
        &format!("shims: wrote CLI shim {}", dst_shim.display()),
    );

    Ok(())
}

/// Removes `<home>\app\bin\betterrecall-mcp.exe` and `<home>\app\bin\betterrecall.cmd`
/// (renaming any in-use file to `.old-<n>` and logging; never fails the uninstall),
/// sweeps stale `.old-*` files, and removes `<home>\app\bin` and `<home>\app` if empty.
pub fn remove_relay_and_shim(ctx: &Ctx) {
    let bin = bin_dir(&ctx.home);
    if bin.is_dir() {
        for name in [InstallLayout::MCP_EXE, InstallLayout::CLI_SHIM] {
            remove_or_rename_aside(&bin.join(name), &ctx.home);
        }
        sweep_old_files(&bin, &ctx.home);
        let _ = fs::remove_dir(&bin);
    }
    let app = app_dir(&ctx.home);
    if app.is_dir() {
        let _ = fs::remove_dir(&app);
    }
}

/// Cleans up launcher-era installs under `<home>\app`:
/// - version folders (`InstallLayout::is_version_folder`)
/// - `.tmp-*` leftovers in `<home>\app` and `<home>\app\bin`
/// - `bin\current` and `bin\BetterRecall.exe`
///
/// Anything in use is skipped and logged on one line; never fails the install.
/// Never touches anything else in `<home>`.
pub fn cleanup_launcher_installs(home: &Path) {
    let app = app_dir(home);
    if !app.is_dir() {
        return;
    }

    let images = crate::running::running_images();
    if let Ok(entries) = fs::read_dir(&app) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir()
                && (InstallLayout::is_version_folder(name_str) || name_str.starts_with(TMP_PREFIX))
            {
                if crate::running::any_image_under(&images, &path) {
                    log_line(
                        home,
                        &format!("cleanup: skipped folder a running program uses {}", path.display()),
                    );
                    continue;
                }
                match fs::remove_dir_all(&path) {
                    Ok(()) => log_line(
                        home,
                        &format!("cleanup: removed launcher folder {}", path.display()),
                    ),
                    Err(e) => log_line(
                        home,
                        &format!("cleanup: skipped in-use folder {}: {e}", path.display()),
                    ),
                }
            } else if meta.is_file() && name_str.starts_with(TMP_PREFIX) {
                match fs::remove_file(&path) {
                    Ok(()) => log_line(
                        home,
                        &format!("cleanup: removed leftover file {}", path.display()),
                    ),
                    Err(e) => log_line(
                        home,
                        &format!("cleanup: skipped in-use file {}: {e}", path.display()),
                    ),
                }
            }
        }
    }

    let bin = bin_dir(home);
    if bin.is_dir() {
        for legacy in [LEGACY_CURRENT, LEGACY_AUTOSTART_EXE] {
            let path = bin.join(legacy);
            if path.exists() {
                match fs::remove_file(&path) {
                    Ok(()) => log_line(
                        home,
                        &format!("cleanup: removed launcher file {}", path.display()),
                    ),
                    Err(e) => log_line(
                        home,
                        &format!("cleanup: skipped in-use file {}: {e}", path.display()),
                    ),
                }
            }
        }
        if let Ok(entries) = fs::read_dir(&bin) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if !name_str.starts_with(TMP_PREFIX) {
                    continue;
                }
                let path = entry.path();
                let res = if entry.metadata().is_ok_and(|m| m.is_dir()) {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
                match res {
                    Ok(()) => log_line(
                        home,
                        &format!("cleanup: removed leftover {}", path.display()),
                    ),
                    Err(e) => log_line(
                        home,
                        &format!("cleanup: skipped in-use {}: {e}", path.display()),
                    ),
                }
            }
        }
    }
}

/// Best-effort deletion of stale `*.old-*` files in `dir`.
pub fn sweep_old_files(dir: &Path, home: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.contains(OLD_SUFFIX) {
            continue;
        }
        let path = entry.path();
        if entry.metadata().is_ok_and(|m| m.is_file()) {
            match fs::remove_file(&path) {
                Ok(()) => log_line(
                    home,
                    &format!("shims: swept stale file {}", path.display()),
                ),
                Err(e) => {
                    // C8: Still in use by an open relay session; log and leave for next run.
                    log_line(
                        home,
                        &format!("shims: skipped locked relay file {}: {e}", path.display()),
                    );
                }
            }
        }
    }
}

fn copy_with_rename_aside(src: &Path, dst: &Path, home: &Path) -> Result<(), InstallError> {
    match fs::copy(src, dst) {
        Ok(_) => Ok(()),
        Err(_) if dst.exists() => {
            let aside = next_old_path(dst);
            fs::rename(dst, &aside).map_err(|e| {
                InstallError::io(
                    format!(
                        "renaming in-use {} to {}",
                        dst.display(),
                        aside.display()
                    ),
                    e,
                )
            })?;
            log_line(
                home,
                &format!(
                    "shims: renamed in-use {} to {}",
                    dst.display(),
                    aside.display()
                ),
            );
            fs::copy(src, dst).map_err(|e| {
                InstallError::io(format!("copying {} to {}", src.display(), dst.display()), e)
            })?;
            Ok(())
        }
        Err(e) => Err(InstallError::io(
            format!("copying {} to {}", src.display(), dst.display()),
            e,
        )),
    }
}

fn write_with_rename_aside(dst: &Path, data: &[u8], home: &Path) -> Result<(), InstallError> {
    match fs::write(dst, data) {
        Ok(()) => Ok(()),
        Err(_) if dst.exists() => {
            let aside = next_old_path(dst);
            fs::rename(dst, &aside).map_err(|e| {
                InstallError::io(
                    format!(
                        "renaming in-use {} to {}",
                        dst.display(),
                        aside.display()
                    ),
                    e,
                )
            })?;
            log_line(
                home,
                &format!(
                    "shims: renamed in-use {} to {}",
                    dst.display(),
                    aside.display()
                ),
            );
            fs::write(dst, data)
                .map_err(|e| InstallError::io(format!("writing {}", dst.display()), e))
        }
        Err(e) => Err(InstallError::io(format!("writing {}", dst.display()), e)),
    }
}

fn remove_or_rename_aside(path: &Path, home: &Path) {
    if !path.exists() {
        return;
    }
    match fs::remove_file(path) {
        Ok(()) => log_line(home, &format!("shims: removed {}", path.display())),
        Err(remove_err) => {
            let aside = next_old_path(path);
            match fs::rename(path, &aside) {
                Ok(()) => log_line(
                    home,
                    &format!(
                        "shims: renamed in-use {} to {} during uninstall ({remove_err})",
                        path.display(),
                        aside.display()
                    ),
                ),
                Err(rename_err) => log_line(
                    home,
                    &format!(
                        "shims: could not remove or rename {} during uninstall ({remove_err}; {rename_err})",
                        path.display()
                    ),
                ),
            }
        }
    }
}

fn next_old_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 1u64.. {
        let cand = parent.join(format!("{base}{OLD_SUFFIX}{n}"));
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!("u64 counter exhausted")
}
