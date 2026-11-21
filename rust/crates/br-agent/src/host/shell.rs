// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Startup registry registration and UI launcher (`Shell.cs`).

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use br_contracts::{DataPaths, InstallLayout, PipeNames, full_path};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW,
};
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, OpenEventW, SetEvent,
};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
use windows::core::PCWSTR;

use crate::native::{get_process_image_path, list_processes, to_wide_z};
use crate::util::AgentLog;

pub struct StartupRegistration;

impl StartupRegistration {
    const RUN_KEY: &'static str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APP_KEY: &'static str = r"Software\BetterRecall";
    const VALUE_NAME: &'static str = "BetterRecall";

    /// Started by the single-file launcher, or from the installed layout (`<home>\app\...`),
    /// on the default data folder.
    pub fn is_installed(install_dir: &Path, home: &Path) -> bool {
        if !DataPaths::is_default_home(home) {
            return false;
        }
        if std::env::var_os(InstallLayout::LAUNCHER_ENV)
            .is_some_and(|v| !v.to_string_lossy().trim().is_empty())
        {
            return true;
        }
        let app_base = full_path(&home.join("app")).unwrap_or_else(|_| home.join("app"));
        let app = format!(
            "{}\\",
            app_base.to_string_lossy().trim_end_matches(['\\', '/'])
        );
        let inst = full_path(install_dir).unwrap_or_else(|_| install_dir.to_path_buf());
        let inst_s = inst.to_string_lossy();
        inst_s.len() >= app.len()
            && inst_s.is_char_boundary(app.len())
            && inst_s[..app.len()].eq_ignore_ascii_case(&app)
    }

    /// What autostart runs: `<root>\bin\BetterRecall.exe` first (if `bin\current` is valid),
    /// then `launcher`, then this agent exe.
    pub fn autostart_exe(install_dir: &Path, launcher: Option<&Path>) -> PathBuf {
        let install_full = full_path(install_dir).unwrap_or_else(|_| install_dir.to_path_buf());
        let install_trimmed = PathBuf::from(
            install_full
                .to_string_lossy()
                .trim_end_matches('\\')
                .to_string(),
        );
        if let Some(root) = install_trimmed.parent() {
            let stub = root
                .join(InstallLayout::BIN_DIR)
                .join(InstallLayout::AUTOSTART_EXE);
            if stub.is_file() && InstallLayout::current_install(root, None).is_some() {
                return stub;
            }
        }
        if let Some(l) = launcher
            && !l.as_os_str().is_empty()
            && l.is_file()
        {
            return full_path(l).unwrap_or_else(|_| l.to_path_buf());
        }
        InstallLayout::full(install_dir, InstallLayout::AGENT_EXE)
    }

    pub fn register(install_dir: &Path) -> Result<(), String> {
        let launcher = std::env::var_os(InstallLayout::LAUNCHER_ENV).map(PathBuf::from);
        let exe = Self::autostart_exe(install_dir, launcher.as_deref());
        let cmd = format!("\"{}\"", exe.to_string_lossy());
        let run_cur = reg_get_string(HKEY_CURRENT_USER, Self::RUN_KEY, Self::VALUE_NAME);
        if run_cur.as_deref() != Some(cmd.as_str()) {
            reg_set_string(HKEY_CURRENT_USER, Self::RUN_KEY, Self::VALUE_NAME, &cmd)?;
        }
        let inst_str = install_dir.to_string_lossy();
        let app_cur = reg_get_string(HKEY_CURRENT_USER, Self::APP_KEY, "InstallPath");
        if app_cur.as_deref() != Some(inst_str.as_ref()) {
            reg_set_string(HKEY_CURRENT_USER, Self::APP_KEY, "InstallPath", &inst_str)?;
        }
        AgentLog::info("startup registration updated");
        Ok(())
    }

    pub fn unregister() {
        let key_w = to_wide_z(Self::RUN_KEY);
        let val_w = to_wide_z(Self::VALUE_NAME);
        let mut hkey = HKEY::default();
        if unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key_w.as_ptr()),
                Some(0),
                KEY_SET_VALUE,
                &mut hkey,
            )
        }
        .is_ok()
        {
            unsafe {
                let _ = RegDeleteValueW(hkey, PCWSTR(val_w.as_ptr()));
                let _ = RegCloseKey(hkey);
            }
        }
    }

    pub fn is_registered() -> bool {
        reg_get_string(HKEY_CURRENT_USER, Self::RUN_KEY, Self::VALUE_NAME).is_some()
    }
}

pub struct UiLauncher;

impl UiLauncher {
    pub fn show_launcher(install_dir: &Path, home: &Path) -> bool {
        Self::allow_running_ui(install_dir);
        let ev_name = PipeNames::show_launcher_event_for(home);
        let ev_w = to_wide_z(&ev_name);
        if let Ok(ev) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(ev_w.as_ptr())) } {
            let ok = unsafe { SetEvent(ev) }.is_ok();
            unsafe {
                let _ = CloseHandle(ev);
            }
            return ok;
        }
        Self::start(install_dir, &[])
    }

    pub fn start(install_dir: &Path, args: &[&str]) -> bool {
        let exe = InstallLayout::full(install_dir, InstallLayout::UI_EXE);
        Self::allow_running_ui(install_dir);
        if !exe.is_file() {
            AgentLog::warn("UI exe not found");
            return false;
        }
        let mut cmd = Command::new(&exe);
        cmd.args(args);
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        match cmd.spawn() {
            Ok(child) => {
                unsafe {
                    let _ = AllowSetForegroundWindow(child.id());
                }
                true
            }
            Err(e) => {
                AgentLog::warn_err("UI launch", &e.to_string());
                false
            }
        }
    }

    fn allow_running_ui(install_dir: &Path) {
        let exe = InstallLayout::full(install_dir, InstallLayout::UI_EXE);
        let Some(exe_name) = exe.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        let exe_str = exe.to_string_lossy();
        for (pid, name) in list_processes() {
            if !name.eq_ignore_ascii_case(exe_name) {
                continue;
            }
            if let Some(path) = get_process_image_path(pid) {
                let full = full_path(Path::new(&path)).unwrap_or_else(|_| PathBuf::from(&path));
                if full.to_string_lossy().eq_ignore_ascii_case(&exe_str) {
                    unsafe {
                        let _ = AllowSetForegroundWindow(pid);
                    }
                }
            }
        }
    }
}

pub(crate) fn reg_get_string(root: HKEY, subkey: &str, value_name: &str) -> Option<String> {
    let key_w = to_wide_z(subkey);
    let val_w = to_wide_z(value_name);
    let mut hkey = HKEY::default();
    unsafe {
        RegOpenKeyExW(
            root,
            PCWSTR(key_w.as_ptr()),
            Some(0),
            KEY_QUERY_VALUE,
            &mut hkey,
        )
    }
    .ok()
    .ok()?;

    let mut ty = REG_VALUE_TYPE::default();
    let mut bytes = 0u32;
    let query_res = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(val_w.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut bytes),
        )
    };
    if query_res.is_err() || ty != REG_SZ || bytes < 2 {
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let ok = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(val_w.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut bytes),
        )
    }
    .is_ok();
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if !ok {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(OsString::from_wide(&buf[..len]).to_string_lossy().into_owned())
}

pub(crate) fn reg_get_dword(root: HKEY, subkey: &str, value_name: &str) -> Option<u32> {
    let key_w = to_wide_z(subkey);
    let val_w = to_wide_z(value_name);
    let mut hkey = HKEY::default();
    unsafe {
        RegOpenKeyExW(
            root,
            PCWSTR(key_w.as_ptr()),
            Some(0),
            KEY_QUERY_VALUE,
            &mut hkey,
        )
    }
    .ok()
    .ok()?;
    let mut val = 0u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(val_w.as_ptr()),
            None,
            None,
            Some((&raw mut val).cast::<u8>()),
            Some(&mut bytes),
        )
    }
    .is_ok();
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if ok { Some(val) } else { None }
}

fn reg_set_string(root: HKEY, subkey: &str, value_name: &str, value: &str) -> Result<(), String> {
    let key_w = to_wide_z(subkey);
    let val_w = to_wide_z(value_name);
    let data_w = to_wide_z(value);
    let mut hkey = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            root,
            PCWSTR(key_w.as_ptr()),
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    }
    .ok()
    .map_err(|e| format!("RegCreateKeyExW: {e}"))?;

    let bytes = unsafe {
        std::slice::from_raw_parts(data_w.as_ptr().cast::<u8>(), data_w.len() * 2)
    };
    let res = unsafe { RegSetValueExW(hkey, PCWSTR(val_w.as_ptr()), Some(0), REG_SZ, Some(bytes)) };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    res.ok().map_err(|e| format!("RegSetValueExW: {e}"))
}
