// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! HKCU Uninstall, Run, and InstallPath registry entries (prefixed by `ctx.reg_prefix`).

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use br_contracts::InstallLayout;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_READ, KEY_SET_VALUE, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_DWORD, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW,
    RegDeleteKeyW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

use crate::InstallError;
use crate::ctx::Ctx;
use crate::install::{ModelMode, SETUP_EXE};
use crate::running::{normalized_dir_prefix, path_is_under_prefix};

pub const UNINSTALL_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\BetterRecall";
pub const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
// F4: HKCU RunOnce subkey for delayed cleanup.
pub const RUNONCE_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
pub const CLEANUP_VALUE_NAME: &str = "BetterRecallCleanup";
pub const APP_SUBKEY: &str = r"Software\BetterRecall";
pub const MODEL_MODE_VALUE: &str = "ModelMode";

pub const DISPLAY_NAME: &str = "BetterRecall";
pub const PUBLISHER: &str = "BetterRecall contributors";
pub const URL_INFO_ABOUT: &str = "https://github.com/lnminh1411/BetterRecall";

/// Writes the Uninstall key, Run value, and `Software\BetterRecall\InstallPath` under `HKCU\<reg_prefix>`.
pub fn register(ctx: &Ctx, version: &str) -> Result<(), InstallError> {
    let install_str = ctx.install_dir.to_string_lossy().into_owned();
    let agent_exe = ctx.install_dir.join(InstallLayout::AGENT_EXE);
    let setup_exe = ctx.install_dir.join(SETUP_EXE);

    let display_icon = format!("\"{}\",0", agent_exe.display());
    let uninstall_cmd = format!("\"{}\" /uninstall", setup_exe.display());
    let quiet_uninstall_cmd = format!("\"{}\" /uninstall /S", setup_exe.display());
    let modify_cmd = format!("\"{}\" /modify", setup_exe.display());
    let estimated_kib = estimated_size_kib(&ctx.install_dir);

    let uninstall_key = ctx.reg_key(UNINSTALL_SUBKEY);
    {
        let hkey = create_key(&uninstall_key)?;
        set_reg_sz(hkey.0, "DisplayName", DISPLAY_NAME)?;
        set_reg_sz(hkey.0, "DisplayVersion", version)?;
        set_reg_sz(hkey.0, "Publisher", PUBLISHER)?;
        set_reg_sz(hkey.0, "InstallLocation", &install_str)?;
        set_reg_sz(hkey.0, "DisplayIcon", &display_icon)?;
        set_reg_sz(hkey.0, "UninstallString", &uninstall_cmd)?;
        set_reg_sz(hkey.0, "QuietUninstallString", &quiet_uninstall_cmd)?;
        set_reg_sz(hkey.0, "ModifyPath", &modify_cmd)?;
        set_reg_dword(hkey.0, "NoRepair", 1)?;
        set_reg_dword(hkey.0, "EstimatedSize", estimated_kib)?;
        set_reg_sz(hkey.0, "URLInfoAbout", URL_INFO_ABOUT)?;
    }

    let run_key = ctx.reg_key(RUN_SUBKEY);
    {
        let hkey = create_key(&run_key)?;
        let run_val = format!("\"{}\"", agent_exe.display());
        set_reg_sz(hkey.0, "BetterRecall", &run_val)?;
    }

    let app_key = ctx.reg_key(APP_SUBKEY);
    {
        let hkey = create_key(&app_key)?;
        set_reg_sz(hkey.0, "InstallPath", &install_str)?;
    }

    Ok(())
}

/// Writes `ModelMode` (`full`, `minimal`, or `later`) as `REG_SZ` under `HKCU\<reg_prefix>\Software\BetterRecall`.
pub fn write_model_mode(ctx: &Ctx, mode: ModelMode) -> Result<(), InstallError> {
    write_reg_sz(&ctx.reg_key(APP_SUBKEY), MODEL_MODE_VALUE, mode.as_str())
}

/// Reads `ModelMode` from `HKCU\<reg_prefix>\Software\BetterRecall`, returning `None` if absent or invalid.
pub fn read_model_mode(ctx: &Ctx) -> Option<ModelMode> {
    let raw = read_reg_sz(&ctx.reg_key(APP_SUBKEY), MODEL_MODE_VALUE).ok()??;
    ModelMode::parse(&raw)
}

/// Writes `BetterRecallCleanup` command into `HKCU\<reg_prefix>\Software\Microsoft\Windows\CurrentVersion\RunOnce` (F4).
pub fn write_run_once_cleanup(ctx: &Ctx, command: &str) -> Result<(), InstallError> {
    write_reg_sz(&ctx.reg_key(RUNONCE_SUBKEY), CLEANUP_VALUE_NAME, command)
}

/// Deletes an entire HKCU subkey if it has no values and no subkeys left (C8).
pub fn delete_key_if_empty(subkey: &str) -> Result<(), InstallError> {
    let Some(hkey) = open_key_opt(subkey, KEY_READ)? else {
        return Ok(());
    };
    let mut subkeys = 0u32;
    let mut values = 0u32;
    let err = unsafe {
        RegQueryInfoKeyW(
            hkey.0,
            None,
            None,
            None,
            Some(&mut subkeys),
            None,
            None,
            Some(&mut values),
            None,
            None,
            None,
            None,
        )
    };
    drop(hkey);
    if err.is_ok() && subkeys == 0 && values == 0 {
        let wide = to_wide_nul(subkey);
        let del_err = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr())) };
        if del_err.is_err() && del_err != ERROR_FILE_NOT_FOUND && del_err != ERROR_PATH_NOT_FOUND {
            return Err(reg_err(format!("deleting empty registry key HKCU\\{subkey}"), del_err));
        }
    }
    Ok(())
}

/// Removes the Uninstall key, the `InstallPath` and `ModelMode` values, and the `Run` value only
/// if it points into `ctx.install_dir`.
pub fn unregister(ctx: &Ctx) -> Result<(), InstallError> {
    delete_key_tree(&ctx.reg_key(UNINSTALL_SUBKEY))?;

    let app_key = ctx.reg_key(APP_SUBKEY);
    if let Some(hkey) = open_key_opt(&app_key, KEY_SET_VALUE)? {
        delete_value_opt(hkey.0, "InstallPath")?;
        delete_value_opt(hkey.0, MODEL_MODE_VALUE)?;
    }
    // C8: delete Software\BetterRecall if it has no values and no subkeys left
    delete_key_if_empty(&app_key)?;

    let run_key = ctx.reg_key(RUN_SUBKEY);
    if let Some(hkey) = open_key_opt(&run_key, KEY_QUERY_VALUE | KEY_SET_VALUE)?
        && let Some(val) = query_reg_sz_opt(hkey.0, "BetterRecall")?
        && run_value_points_into(&val, &ctx.install_dir)
    {
        delete_value_opt(hkey.0, "BetterRecall")?;
    }

    Ok(())
}

/// Deletes an entire HKCU subkey tree if it exists.
pub fn delete_key_tree(subkey: &str) -> Result<(), InstallError> {
    let wide = to_wide_nul(subkey);
    let err = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr())) };
    if err.is_ok() || err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
        Ok(())
    } else {
        Err(reg_err(format!("deleting registry key HKCU\\{subkey}"), err))
    }
}

/// Reads a `REG_SZ` value under `HKCU\<subkey>`, returning `None` if the key or value does not exist.
pub fn read_reg_sz(subkey: &str, name: &str) -> Result<Option<String>, InstallError> {
    let Some(hkey) = open_key_opt(subkey, KEY_READ)? else {
        return Ok(None);
    };
    query_reg_sz_opt(hkey.0, name)
}

/// Reads a `REG_DWORD` value under `HKCU\<subkey>`, returning `None` if the key or value does not exist.
pub fn read_reg_dword(subkey: &str, name: &str) -> Result<Option<u32>, InstallError> {
    let Some(hkey) = open_key_opt(subkey, KEY_READ)? else {
        return Ok(None);
    };
    let wide_name = to_wide_nul(name);
    let mut ty = REG_VALUE_TYPE(0);
    let mut val = 0u32;
    let mut len = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegQueryValueExW(
            hkey.0,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut ty),
            Some((&mut val as *mut u32).cast::<u8>()),
            Some(&mut len),
        )
    };
    if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    if err.is_err() {
        return Err(reg_err(
            format!("reading DWORD '{name}' from HKCU\\{subkey}"),
            err,
        ));
    }
    if ty != REG_DWORD || len as usize != std::mem::size_of::<u32>() {
        return Ok(None);
    }
    Ok(Some(val))
}

/// Writes a `REG_SZ` value under `HKCU\<subkey>`, creating the key if needed.
pub fn write_reg_sz(subkey: &str, name: &str, value: &str) -> Result<(), InstallError> {
    let hkey = create_key(subkey)?;
    set_reg_sz(hkey.0, name, value)
}

fn run_value_points_into(value: &str, install_dir: &Path) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let exe_str = if let Some(rest) = trimmed.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else {
        trimmed.split_whitespace().next().unwrap_or("")
    };
    if exe_str.is_empty() {
        return false;
    }
    let prefix = normalized_dir_prefix(install_dir);
    path_is_under_prefix(Path::new(exe_str), &prefix)
}

fn estimated_size_kib(dir: &Path) -> u32 {
    let total = dir_size_bytes(dir);
    let kib = total.div_ceil(1024);
    u32::try_from(kib).unwrap_or(u32::MAX)
}

fn dir_size_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            total = total.saturating_add(dir_size_bytes(&entry.path()));
        }
    }
    total
}

fn create_key(subkey: &str) -> Result<OwnedKey, InstallError> {
    let wide = to_wide_nul(subkey);
    let mut hkey = HKEY::default();
    let err = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide.as_ptr()),
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    if err.is_err() {
        return Err(reg_err(
            format!("creating registry key HKCU\\{subkey}"),
            err,
        ));
    }
    Ok(OwnedKey(hkey))
}

fn open_key_opt(
    subkey: &str,
    access: windows::Win32::System::Registry::REG_SAM_FLAGS,
) -> Result<Option<OwnedKey>, InstallError> {
    let wide = to_wide_nul(subkey);
    let mut hkey = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide.as_ptr()),
            Some(0),
            access,
            &mut hkey,
        )
    };
    if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    if err.is_err() {
        return Err(reg_err(
            format!("opening registry key HKCU\\{subkey}"),
            err,
        ));
    }
    Ok(Some(OwnedKey(hkey)))
}

fn set_reg_sz(hkey: HKEY, name: &str, value: &str) -> Result<(), InstallError> {
    let wide_name = to_wide_nul(name);
    let wide_val = to_wide_nul(value);
    let bytes = unsafe {
        std::slice::from_raw_parts(wide_val.as_ptr().cast::<u8>(), wide_val.len() * 2)
    };
    let err = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            Some(0),
            REG_SZ,
            Some(bytes),
        )
    };
    if err.is_err() {
        return Err(reg_err(format!("writing registry string '{name}'"), err));
    }
    Ok(())
}

fn set_reg_dword(hkey: HKEY, name: &str, value: u32) -> Result<(), InstallError> {
    let wide_name = to_wide_nul(name);
    let bytes = value.to_le_bytes();
    let err = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            Some(0),
            REG_DWORD,
            Some(&bytes),
        )
    };
    if err.is_err() {
        return Err(reg_err(format!("writing registry DWORD '{name}'"), err));
    }
    Ok(())
}

fn query_reg_sz_opt(hkey: HKEY, name: &str) -> Result<Option<String>, InstallError> {
    let wide_name = to_wide_nul(name);
    let mut ty = REG_VALUE_TYPE(0);
    let mut byte_len = 0u32;
    let err = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut byte_len),
        )
    };
    if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    if err.is_err() {
        return Err(reg_err(format!("querying registry string '{name}'"), err));
    }
    if ty != REG_SZ {
        return Ok(None);
    }
    let u16_len = (byte_len as usize).div_ceil(2);
    let mut buf = vec![0u16; u16_len];
    let mut actual = (buf.len() * 2) as u32;
    let err = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut actual),
        )
    };
    if err.is_err() {
        return Err(reg_err(format!("reading registry string '{name}'"), err));
    }
    let words = (actual as usize) / 2;
    let slice = &buf[..words.min(buf.len())];
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    Ok(Some(
        OsString::from_wide(&slice[..end])
            .to_string_lossy()
            .into_owned(),
    ))
}

fn delete_value_opt(hkey: HKEY, name: &str) -> Result<(), InstallError> {
    let wide_name = to_wide_nul(name);
    let err = unsafe { RegDeleteValueW(hkey, PCWSTR(wide_name.as_ptr())) };
    if err.is_ok() || err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
        Ok(())
    } else {
        Err(reg_err(format!("deleting registry value '{name}'"), err))
    }
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    OsString::from(s).encode_wide().chain(Some(0)).collect()
}

fn reg_err(context: String, err: WIN32_ERROR) -> InstallError {
    InstallError::io(context, io::Error::from_raw_os_error(err.0 as i32))
}

struct OwnedKey(HKEY);

impl Drop for OwnedKey {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
}
