// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Win32 helpers shared across host, guardrails, scheduling, security, and worker modules.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSFreeMemory, WTSQuerySessionInformationW,
    WTSSessionInfoEx,
};
use windows::Win32::System::SystemInformation::{GetTickCount, GetTickCount64};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThread, GetProcessTimes, GetThreadTimes,
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST,
};
use windows::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
};
use windows::core::PWSTR;

/// Desktop rectangle in pixels (`Win32.RECT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn width(self) -> i32 {
        self.right - self.left
    }

    pub const fn height(self) -> i32 {
        self.bottom - self.top
    }

    pub const fn from_win32(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

/// Sentinel mask rectangle covering the whole canvas (`CaptureService.MaskEverything`).
pub const MASK_EVERYTHING: Rect = Rect::new(i32::MIN / 4, i32::MIN / 4, i32::MAX / 4, i32::MAX / 4);

pub const WTS_ACTIVE: i32 = 0;
pub const WTS_SESSIONSTATE_LOCK: i32 = 0;
pub const WTS_SESSIONSTATE_UNLOCK: i32 = 1;

pub const WTS_CONSOLE_CONNECT: i32 = 1;
pub const WTS_CONSOLE_DISCONNECT: i32 = 2;
pub const WTS_REMOTE_CONNECT: i32 = 3;
pub const WTS_REMOTE_DISCONNECT: i32 = 4;
pub const WTS_SESSION_LOGON: i32 = 5;
pub const WTS_SESSION_LOGOFF: i32 = 6;
pub const WTS_SESSION_LOCK: i32 = 7;
pub const WTS_SESSION_UNLOCK: i32 = 8;

/// Monotonic milliseconds since boot (`GetTickCount64`, counts sleep).
pub fn tick_count64() -> i64 {
    unsafe { GetTickCount64() as i64 }
}

/// Milliseconds of `QueryUnbiasedInterruptTime`: skips sleep/hibernation (`LivenessClock.NowMs`).
pub fn liveness_now_ms() -> i64 {
    let mut t = 0u64;
    unsafe {
        let _ = QueryUnbiasedInterruptTime(&mut t);
    }
    (t / 10_000) as i64
}

pub fn current_process_id() -> u32 {
    unsafe { GetCurrentProcessId() }
}

pub fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

pub fn qpc_frequency() -> i64 {
    let mut f = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1)
}

pub fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

/// CPU time (kernel + user) of the current process in milliseconds.
pub fn process_cpu_ms() -> f64 {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok.is_ok() {
        (filetime_to_u64(kernel) + filetime_to_u64(user)) as f64 / 10_000.0
    } else {
        0.0
    }
}

/// CPU time (kernel + user) of another process by PID in milliseconds, or `None` if inaccessible.
pub fn process_cpu_ms_by_pid(pid: u32) -> Option<f64> {
    if pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if ok.is_ok() {
        Some((filetime_to_u64(kernel) + filetime_to_u64(user)) as f64 / 10_000.0)
    } else {
        None
    }
}

/// Working set and private memory of the current process in MiB.
pub fn process_memory_mb() -> (i64, i64) {
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&raw mut pmc).cast(),
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if ok.is_ok() {
        (
            (pmc.WorkingSetSize / 1_048_576) as i64,
            (pmc.PrivateUsage / 1_048_576) as i64,
        )
    } else {
        (0, 0)
    }
}

/// Current foreground window handle (`GetForegroundWindow`).
pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow().0 as isize }
}

/// Logical processor count (at least 1).
pub fn logical_processor_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Sets the calling thread priority to below-normal (`THREAD_PRIORITY_BELOW_NORMAL`).
pub fn set_current_thread_below_normal() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Sets the calling thread priority to lowest (`THREAD_PRIORITY_LOWEST`).
pub fn set_current_thread_lowest() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }
}

/// CPU time (kernel + user) of the calling thread in milliseconds.
pub fn current_thread_cpu_ms() -> f64 {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok.is_ok() {
        (filetime_to_u64(kernel) + filetime_to_u64(user)) as f64 / 10_000.0
    } else {
        0.0
    }
}

/// Milliseconds since last user input (`GetLastInputInfo`).
pub fn idle_milliseconds() -> i64 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        let now = unsafe { GetTickCount() };
        now.wrapping_sub(info.dwTime) as i64
    } else {
        0
    }
}

/// The current session is locked or disconnected right now (`WTSSessionInfoEx`); `None` when unknown.
pub fn session_locked_now() -> Option<bool> {
    let mut buf = PWSTR::null();
    let mut bytes = 0u32;
    let ok = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buf,
            &mut bytes,
        )
    };
    if ok.is_err() || buf.0.is_null() {
        return None;
    }
    let result = unsafe {
        // WTSINFOEXW: DWORD Level at 0, then WTSINFOEX_LEVEL1_W union (8-aligned):
        // SessionId at 8, SessionState at 12, SessionFlags at 16.
        let ptr = buf.0.cast::<u8>();
        if (bytes as usize) < 20 || *(ptr.cast::<i32>()) != 1 {
            None
        } else {
            let state = *(ptr.add(12).cast::<i32>());
            let flags = *(ptr.add(16).cast::<i32>());
            session_locked(state, flags)
        }
    };
    unsafe {
        WTSFreeMemory(buf.0.cast::<c_void>());
    }
    result
}

/// Pure decision for `session_locked_now`: locked when session flags say lock, or session is not active.
pub fn session_locked(state: i32, flags: i32) -> Option<bool> {
    if state != WTS_ACTIVE {
        return Some(true);
    }
    match flags {
        WTS_SESSIONSTATE_LOCK => Some(true),
        WTS_SESSIONSTATE_UNLOCK => Some(false),
        _ => None,
    }
}

pub fn get_window_text(hwnd: isize) -> String {
    let h = HWND(hwnd as *mut c_void);
    let len = unsafe { GetWindowTextLengthW(h) };
    if len <= 0 {
        return String::new();
    }
    let len = (len as usize).min(1024);
    let mut buf = vec![0u16; len + 1];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    if n <= 0 {
        String::new()
    } else {
        OsString::from_wide(&buf[..n as usize])
            .to_string_lossy()
            .into_owned()
    }
}

pub fn get_class_name(hwnd: isize) -> String {
    let h = HWND(hwnd as *mut c_void);
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    if n <= 0 {
        String::new()
    } else {
        OsString::from_wide(&buf[..n as usize])
            .to_string_lossy()
            .into_owned()
    }
}

pub fn get_process_image_path_and_creation(pid: u32) -> Option<(String, Option<u64>)> {
    if pid == 0 {
        return None;
    }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let creation_time = if unsafe {
        GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user)
    }
    .is_ok()
    {
        Some(filetime_to_u64(creation))
    } else {
        None
    };
    let mut buf = vec![0u16; 1024];
    let mut size = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size)
    };
    unsafe {
        let _ = CloseHandle(h);
    }
    if ok.is_ok() && size > 0 {
        Some((
            OsString::from_wide(&buf[..size as usize])
                .to_string_lossy()
                .into_owned(),
            creation_time,
        ))
    } else {
        None
    }
}

/// D1: the process creation time (FILETIME ticks), used with the PID as the path cache key.
pub fn get_process_creation_time(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe { GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) };
    unsafe {
        let _ = CloseHandle(h);
    }
    ok.ok().map(|_| filetime_to_u64(creation))
}

pub fn get_process_image_path(pid: u32) -> Option<String> {
    get_process_image_path_and_creation(pid).map(|(p, _)| p)
}

pub fn is_cloaked(hwnd: isize) -> bool {
    let h = HWND(hwnd as *mut c_void);
    let mut cloaked: i32 = 0;
    let hr = unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast::<c_void>(),
            std::mem::size_of::<i32>() as u32,
        )
    };
    hr.is_ok() && cloaked != 0
}

/// Lists `(pid, exe_file_name)` for all running processes via `CreateToolhelp32Snapshot`.
pub fn list_processes() -> Vec<(u32, String)> {
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snap, &mut entry) }.is_ok() {
        loop {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = OsString::from_wide(&entry.szExeFile[..len])
                .to_string_lossy()
                .into_owned();
            out.push((entry.th32ProcessID, name));
            if unsafe { Process32NextW(snap, &mut entry) }.is_err() {
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(snap);
    }
    out
}

/// Encodes a Rust string as a null-terminated UTF-16 `Vec<u16>`.
pub fn to_wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
