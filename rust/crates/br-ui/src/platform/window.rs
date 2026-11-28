// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::ffi::c_void;
use std::mem::size_of;

use gpui::{App, DisplayId, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetCursorPos, GetWindowLongPtrW, HWND_TOPMOST, SPI_GETCLIENTAREAANIMATION,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos,
    SW_SHOWNORMAL, SetForegroundWindow, ShowWindow, SystemParametersInfoW, WDA_EXCLUDEFROMCAPTURE,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

/// Extract the Win32 `HWND` from a GPUI `Window` via `HasWindowHandle`.
pub fn hwnd_from_window(window: &Window) -> Option<HWND> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut c_void)),
        _ => None,
    }
}

/// Find the GPUI `DisplayId` of the monitor currently under the mouse cursor.
pub fn display_for_cursor(cx: &App) -> Option<DisplayId> {
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return None;
    }

    let displays = cx.displays();
    if displays.is_empty() {
        return None;
    }

    // Match by Win32 HMONITOR handle directly (GPUI WindowsDisplay encodes HMONITOR as DisplayId),
    // avoiding physical-vs-logical coordinate mixing on mixed-DPI setups.
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    if !hmon.is_invalid() {
        let target_id = hmon.0 as u64;
        if let Some(d) = displays.iter().find(|d| u64::from(d.id()) == target_id) {
            return Some(d.id());
        }
    }

    displays.first().map(|d| d.id())
}

/// Hide the launcher window from Alt-Tab and the taskbar by setting `WS_EX_TOOLWINDOW`
/// and clearing `WS_EX_APPWINDOW`.
pub fn configure_launcher_hwnd(hwnd: HWND) {
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_style =
            (ex_style | (WS_EX_TOOLWINDOW.0 as isize)) & !(WS_EX_APPWINDOW.0 as isize);
        if new_style != ex_style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

/// Re-assert `HWND_TOPMOST` on the launcher without showing it prematurely.
pub fn set_launcher_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Apply `WDA_EXCLUDEFROMCAPTURE` unless `capturable` is true.
/// Called before the window's first show so no frame can be captured.
pub fn apply_display_affinity(hwnd: HWND, capturable: bool) {
    if capturable {
        return;
    }
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
}

/// Show a window that GPUI created hidden (`show: false`) and bring it to the foreground.
///
/// GPUI keeps the placement of a hidden-created window and `Window::set_visible(true)` only
/// re-applies that placement, which leaves the window hidden (checkpoint 5b sandbox run). The
/// display affinity must be applied before this call so the first frame is excluded too.
pub fn show_hwnd(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Query Windows "Animation effects" (`SPI_GETCLIENTAREAANIMATION`).
pub fn client_area_animation_enabled() -> bool {
    let mut enabled: i32 = 1;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut i32 as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    if ok { enabled != 0 } else { true }
}

/// Return current process working set in MiB via `GetProcessMemoryInfo`.
pub fn working_set_mb() -> f64 {
    let mut pmc = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    }
    .is_ok();
    if ok {
        pmc.WorkingSetSize as f64 / (1024.0 * 1024.0)
    } else {
        0.0
    }
}
