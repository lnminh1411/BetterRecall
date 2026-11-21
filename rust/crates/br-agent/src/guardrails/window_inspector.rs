// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Win32 window enumeration for guardrails (`WindowInspector.cs`).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex};

use br_contracts::PrivacySettings;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOT, GWL_EXSTYLE, GetAncestor, GetForegroundWindow, GetWindowDisplayAffinity,
    GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    WDA_NONE, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

use crate::guardrails::policy::{BadgeResult, GuardDecision, GuardReason, GuardrailPolicy, WindowInfo};
use crate::native::{
    Rect, get_class_name, get_process_image_path, get_window_text, is_cloaked, tick_count64,
};

#[derive(Debug, Clone)]
pub struct MonitorTop {
    pub monitor: i32,
    pub window: Option<WindowInfo>,
    pub denied_visible: bool,
}

#[derive(Debug, Clone)]
pub struct GuardSnapshot {
    pub taken_at_ms: i64,
    pub foreground: Option<WindowInfo>,
    pub decision: GuardDecision,
    pub badge: BadgeResult,
    pub any_affinity: bool,
    pub mask_rects: Vec<Rect>,
    pub per_monitor: HashMap<i32, MonitorTop>,
    pub site: String,
    pub secrets: Option<Vec<Rect>>,
    pub fg_epoch: i64,
    pub is_closed_sentinel: bool,
}

impl GuardSnapshot {
    pub fn closed() -> Self {
        Self {
            taken_at_ms: 0,
            foreground: None,
            decision: GuardDecision::deny(GuardReason::NoForeground),
            badge: BadgeResult::NotChecked,
            any_affinity: false,
            mask_rects: Vec::new(),
            per_monitor: HashMap::new(),
            site: String::new(),
            secrets: None,
            fg_epoch: 0,
            is_closed_sentinel: true,
        }
    }

    pub fn foreground_moved() -> Self {
        Self {
            decision: GuardDecision::deny(GuardReason::ForegroundChanged),
            ..Self::closed()
        }
    }

    pub fn secret_rects(&self) -> &[Rect] {
        self.secrets.as_deref().unwrap_or(&[])
    }
}

pub struct ScanResult {
    pub any_affinity: bool,
    pub masks: Vec<Rect>,
    pub per_monitor: HashMap<i32, MonitorTop>,
}

static PATH_CACHE: LazyLock<Mutex<HashMap<u32, (String, i64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    "NotifyIconOverflowWindow",
];

pub struct WindowInspector;

impl WindowInspector {
    pub fn describe(hwnd: isize) -> Option<WindowInfo> {
        if hwnd == 0 {
            return None;
        }
        let h = HWND(hwnd as *mut c_void);
        let root = unsafe { GetAncestor(h, GA_ROOT) };
        let eff = if root.0.is_null() { hwnd } else { root.0 as isize };
        let mut pid = 0u32;
        unsafe {
            let _ = GetWindowThreadProcessId(HWND(eff as *mut c_void), Some(&mut pid));
        }
        Some(WindowInfo {
            hwnd: eff,
            pid,
            exe_path: Self::process_path(pid),
            title: get_window_text(eff),
            class_name: get_class_name(eff),
        })
    }

    pub fn foreground() -> Option<WindowInfo> {
        let hwnd = unsafe { GetForegroundWindow() };
        Self::describe(hwnd.0 as isize)
    }

    /// Top-level window that owns the foreground (`0` when none).
    pub fn foreground_root() -> isize {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return 0;
        }
        let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        if root.0.is_null() {
            hwnd.0 as isize
        } else {
            root.0 as isize
        }
    }

    pub fn process_path(pid: u32) -> String {
        let now = tick_count64();
        {
            let cache = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((path, at)) = cache.get(&pid)
                && now.saturating_sub(*at) < 30_000
            {
                return path.clone();
            }
        }
        let path = get_process_image_path(pid).unwrap_or_default();
        let mut cache = PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(pid, (path.clone(), now));
        path
    }

    pub fn top_level_windows() -> Vec<isize> {
        let mut list: Vec<isize> = Vec::with_capacity(256);
        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let vec = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
            vec.push(hwnd.0 as isize);
            BOOL(1)
        }
        unsafe {
            let _ = EnumWindows(Some(enum_proc), LPARAM((&raw mut list) as isize));
        }
        list
    }

    pub fn try_get_bounds(hwnd: isize) -> Option<Rect> {
        let h = HWND(hwnd as *mut c_void);
        let mut rect = RECT::default();
        let ok = unsafe {
            DwmGetWindowAttribute(
                h,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&raw mut rect).cast::<c_void>(),
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok()
                || GetWindowRect(h, &mut rect).is_ok()
        };
        if ok {
            let r = Rect::from_win32(rect);
            if r.width() > 0 && r.height() > 0 {
                return Some(r);
            }
        }
        None
    }

    pub fn scan<F>(
        p: &PrivacySettings,
        install_dir: &str,
        own_pid: u32,
        monitors: &[(i32, Rect)],
        mut is_private: Option<F>,
    ) -> ScanResult
    where
        F: FnMut(&WindowInfo) -> bool,
    {
        let mut any_affinity = false;
        let mut masks = Vec::new();
        let mut tops: HashMap<i32, WindowInfo> = HashMap::new();
        let mut denied: HashMap<i32, bool> = HashMap::new();

        for hwnd in Self::top_level_windows() {
            let h = HWND(hwnd as *mut c_void);
            if !unsafe { IsWindowVisible(h) }.as_bool()
                || unsafe { IsIconic(h) }.as_bool()
                || is_cloaked(hwnd)
            {
                continue;
            }
            let Some(r) = Self::try_get_bounds(hwnd) else {
                continue;
            };
            let mut pid = 0u32;
            unsafe {
                let _ = GetWindowThreadProcessId(h, Some(&mut pid));
            }
            let exe = Self::process_path(pid);
            let own = pid == own_pid
                || (!install_dir.is_empty()
                    && exe.len() >= install_dir.len()
                    && exe.is_char_boundary(install_dir.len())
                    && exe[..install_dir.len()].eq_ignore_ascii_case(install_dir));
            let mut aff = 0u32;
            if !own
                && unsafe { GetWindowDisplayAffinity(h, &mut aff) }.is_ok()
                && aff != WDA_NONE.0
            {
                any_affinity = true;
            }
            let cls = get_class_name(hwnd);
            if SHELL_CLASSES.contains(&cls.as_str()) {
                continue;
            }
            let title = get_window_text(hwnd);
            let info = WindowInfo {
                hwnd,
                pid,
                exe_path: exe,
                title: title.clone(),
                class_name: cls,
            };
            let deny = GuardrailPolicy::match_deny_lists(&info, p) != GuardReason::None
                || (!own && is_private.as_mut().is_some_and(|f| f(&info)));
            if deny {
                masks.push(r);
            }
            let ex_style = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
            let tool = (ex_style & WS_EX_TOOLWINDOW.0) != 0;
            for &(index, mb) in monitors {
                if !intersects(r, mb) {
                    continue;
                }
                if deny {
                    denied.insert(index, true);
                }
                if !tops.contains_key(&index)
                    && !tool
                    && !own
                    && !title.is_empty()
                    && covers(r, mb)
                {
                    tops.insert(index, info.clone());
                }
            }
        }

        let mut per_monitor = HashMap::new();
        for &(index, _) in monitors {
            per_monitor.insert(
                index,
                MonitorTop {
                    monitor: index,
                    window: tops.remove(&index),
                    denied_visible: denied.get(&index).copied().unwrap_or(false),
                },
            );
        }
        ScanResult {
            any_affinity,
            masks,
            per_monitor,
        }
    }
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn covers(w: Rect, m: Rect) -> bool {
    let cx = (m.left + m.right) / 2;
    let cy = (m.top + m.bottom) / 2;
    w.left <= cx && cx < w.right && w.top <= cy && cy < w.bottom
}
