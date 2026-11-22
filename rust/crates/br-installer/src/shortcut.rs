// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Start menu shortcut (`BetterRecall.lnk`) creation and removal via `IShellLinkW` + `IPersistFile`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use br_contracts::InstallLayout;
use windows::Win32::Foundation::{MAX_PATH, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IPersistFile, STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
use windows::core::{Interface, PCWSTR};

use crate::InstallError;
use crate::ctx::Ctx;

/// File name of the Start menu shortcut.
pub const SHORTCUT_NAME: &str = "BetterRecall.lnk";

/// Path of the Start menu shortcut for `ctx`.
pub fn shortcut_path(ctx: &Ctx) -> PathBuf {
    ctx.start_menu_dir.join(SHORTCUT_NAME)
}

/// Creates `<start_menu_dir>\BetterRecall.lnk` pointing at `<install_dir>\BetterRecall.Agent.exe`
/// with working directory `<install_dir>`.
pub fn create_shortcut(ctx: &Ctx) -> Result<PathBuf, InstallError> {
    fs::create_dir_all(&ctx.start_menu_dir).map_err(|e| {
        InstallError::io(
            format!("creating Start menu folder {}", ctx.start_menu_dir.display()),
            e,
        )
    })?;

    let lnk_path = shortcut_path(ctx);
    let target = ctx.install_dir.join(InstallLayout::AGENT_EXE);
    let target_w = to_wide_nul(&target);
    let dir_w = to_wide_nul(&ctx.install_dir);
    let desc_w: Vec<u16> = "BetterRecall".encode_utf16().chain(Some(0)).collect();
    let lnk_w = to_wide_nul(&lnk_path);

    let _com = ComGuard::init()?;
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| InstallError::Other(format!("CoCreateInstance(ShellLink) failed: {e}")))?;

    unsafe {
        link.SetPath(PCWSTR(target_w.as_ptr()))
            .map_err(|e| InstallError::Other(format!("IShellLinkW::SetPath failed: {e}")))?;
        link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr()))
            .map_err(|e| {
                InstallError::Other(format!("IShellLinkW::SetWorkingDirectory failed: {e}"))
            })?;
        link.SetDescription(PCWSTR(desc_w.as_ptr()))
            .map_err(|e| InstallError::Other(format!("IShellLinkW::SetDescription failed: {e}")))?;
        link.SetIconLocation(PCWSTR(target_w.as_ptr()), 0)
            .map_err(|e| {
                InstallError::Other(format!("IShellLinkW::SetIconLocation failed: {e}"))
            })?;
    }

    let persist: IPersistFile = link
        .cast()
        .map_err(|e| InstallError::Other(format!("QueryInterface(IPersistFile) failed: {e}")))?;
    unsafe {
        persist
            .Save(PCWSTR(lnk_w.as_ptr()), true)
            .map_err(|e| InstallError::Other(format!("IPersistFile::Save failed: {e}")))?;
    }

    Ok(lnk_path)
}

/// Removes `<start_menu_dir>\BetterRecall.lnk` if present.
pub fn remove_shortcut(ctx: &Ctx) -> Result<(), InstallError> {
    let path = shortcut_path(ctx);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(InstallError::io(
            format!("removing shortcut {}", path.display()),
            e,
        )),
    }
}

/// Reads the target path stored inside an existing `.lnk` file.
pub fn read_shortcut_target(lnk_path: &Path) -> Result<PathBuf, InstallError> {
    let lnk_w = to_wide_nul(lnk_path);
    let _com = ComGuard::init()?;
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| InstallError::Other(format!("CoCreateInstance(ShellLink) failed: {e}")))?;
    let persist: IPersistFile = link
        .cast()
        .map_err(|e| InstallError::Other(format!("QueryInterface(IPersistFile) failed: {e}")))?;
    unsafe {
        persist
            .Load(PCWSTR(lnk_w.as_ptr()), STGM_READ)
            .map_err(|e| InstallError::Other(format!("IPersistFile::Load failed: {e}")))?;
    }
    let mut buf = vec![0u16; MAX_PATH as usize * 2];
    unsafe {
        link.GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .map_err(|e| InstallError::Other(format!("IShellLinkW::GetPath failed: {e}")))?;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(OsString::from_wide(&buf[..end])))
}

fn to_wide_nul(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init() -> Result<Self, InstallError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr == S_OK || hr == S_FALSE {
            Ok(Self { should_uninit: true })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self {
                should_uninit: false,
            })
        } else {
            Err(InstallError::Other(format!(
                "CoInitializeEx failed: {hr:?}"
            )))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe { CoUninitialize() };
        }
    }
}
