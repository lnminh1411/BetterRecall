// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(lp_version_information: *mut OSVERSIONINFOW) -> i32;
}

/// Open `url` in the user's default browser via `ShellExecuteW(open)`.
pub fn open_url(url: &str) -> Result<(), String> {
    let wide: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let h = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = h.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW failed with code {code}"))
    }
}

/// Return the Windows version string as `major.minor.build.UBR` (or `major.minor.build` if UBR is unavailable).
pub fn windows_build_string() -> String {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    let status = unsafe { RtlGetVersion(&mut info) };
    if status != 0 {
        return "unknown".to_string();
    }

    let mut ubr: u32 = 0;
    let mut ubr_size = size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("UBR"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut ubr as *mut u32).cast::<c_void>()),
            Some(&mut ubr_size),
        )
    };
    if err.is_ok() {
        format!(
            "{}.{}.{}.{}",
            info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber, ubr
        )
    } else {
        format!(
            "{}.{}.{}",
            info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
        )
    }
}
