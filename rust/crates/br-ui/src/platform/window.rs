// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::ffi::c_void;
use std::mem::size_of;

use gpui::{App, DisplayId, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetCursorPos, GetWindowLongPtrW, HWND_TOPMOST, SPI_GETCLIENTAREAANIMATION,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos,
    SystemParametersInfoW, WDA_EXCLUDEFROMCAPTURE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
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
    let ok = unsafe { GetCursorPos(&mut pt) }.is_ok();
    if !ok {
        return None;
    }

    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let has_info = unsafe { GetMonitorInfoW(hmon, &mut info) }.as_bool();

    let displays = cx.displays();
    if displays.is_empty() {
        return None;
    }

    // Match by monitor origin/bounds containing the cursor in logical/physical coordinates.
    if has_info {
        let m_left = info.rcMonitor.left as f32;
        let m_top = info.rcMonitor.top as f32;
        let m_right = info.rcMonitor.right as f32;
        let m_bottom = info.rcMonitor.bottom as f32;
        let cx_pt = pt.x as f32;
        let cy_pt = pt.y as f32;

        // First try exact physical containment against GPUI display bounds.
        for d in &displays {
            let b = d.bounds();
            let left = f32::from(b.origin.x);
            let top = f32::from(b.origin.y);
            let right = left + f32::from(b.size.width);
            let bottom = top + f32::from(b.size.height);
            if cx_pt >= left && cx_pt < right && cy_pt >= top && cy_pt < bottom {
                return Some(d.id());
            }
        }
        // Fallback: closest monitor center to the Win32 monitor center.
        let target_x = (m_left + m_right) * 0.5;
        let target_y = (m_top + m_bottom) * 0.5;
        return displays
            .iter()
            .min_by(|a, b| {
                let ab = a.bounds();
                let bb = b.bounds();
                let ax = f32::from(ab.origin.x) + f32::from(ab.size.width) * 0.5;
                let ay = f32::from(ab.origin.y) + f32::from(ab.size.height) * 0.5;
                let bx = f32::from(bb.origin.x) + f32::from(bb.size.width) * 0.5;
                let by = f32::from(bb.origin.y) + f32::from(bb.size.height) * 0.5;
                let da = (ax - target_x).powi(2) + (ay - target_y).powi(2);
                let db = (bx - target_x).powi(2) + (by - target_y).powi(2);
                da.total_cmp(&db)
            })
            .map(|d| d.id());
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

/// Re-assert `HWND_TOPMOST` on the launcher each time it is shown.
pub fn set_launcher_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
    }
}

/// Apply `WDA_EXCLUDEFROMCAPTURE` unless `capturable` is true.
/// Must only be called when the window is visible (never on a hidden window).
pub fn apply_display_affinity(hwnd: HWND, capturable: bool) {
    if capturable {
        return;
    }
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
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
