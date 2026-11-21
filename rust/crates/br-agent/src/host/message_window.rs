// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Hidden top-level Win32 message window on the main STA thread: owner HWND for Windows Hello,
//! `RegisterHotKey` target, tray icon and menu, session lock notifications, and foreground WinEvent hook
//! (`MessageWindow.cs`).

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::HKEY_CURRENT_USER;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_BALLOONUSERCLICK, NIN_SELECT,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GetCursorPos, GetMessageW,
    GetSystemMetrics, HICON, IDI_APPLICATION, IMAGE_ICON, LR_LOADFROMFILE, LoadIconW, LoadImageW,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SM_CXSMICON, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_CONTEXTMENU, WM_DISPLAYCHANGE, WM_ENDSESSION, WM_HOTKEY, WM_LBUTTONUP,
    WM_QUERYENDSESSION, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_WTSSESSION_CHANGE,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WNDCLASSEXW,
};
use windows::core::PCWSTR;

use crate::host::hotkey_parser::{Hotkey, HotkeyParser};
use crate::host::shell::reg_get_dword;
use crate::native::to_wide_z;
use crate::util::AgentLog;

const WM_TRAY: u32 = WM_APP + 1;
const WM_INVOKE: u32 = WM_APP + 2;
const NIN_KEYSELECT: u32 = NIN_SELECT + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TrayCommand {
    Search = 1,
    Pause15 = 2,
    Pause60 = 3,
    PauseUntilResume = 4,
    Resume = 5,
    SaveReplay = 6,
    Panic = 7,
    Settings = 8,
    Quit = 9,
    Status = 10,
}

impl TrayCommand {
    fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => Self::Search,
            2 => Self::Pause15,
            3 => Self::Pause60,
            4 => Self::PauseUntilResume,
            5 => Self::Resume,
            6 => Self::SaveReplay,
            7 => Self::Panic,
            8 => Self::Settings,
            9 => Self::Quit,
            10 => Self::Status,
            _ => return None,
        })
    }
}

pub struct MenuStateSnapshot {
    pub status: String,
    pub paused: bool,
    pub replay_on: bool,
}

type InvokeFn = Box<dyn FnOnce() + Send + 'static>;
pub type HotkeyHandler = Arc<dyn Fn(&str) + Send + Sync>;
pub type CommandHandler = Arc<dyn Fn(TrayCommand) + Send + Sync>;
pub type SessionHandler = Arc<dyn Fn(i32) + Send + Sync>;
pub type SimpleHandler = Arc<dyn Fn() + Send + Sync>;
pub type MenuStateProvider = Arc<dyn Fn() -> MenuStateSnapshot + Send + Sync>;

struct WindowCallbacks {
    hotkey: Option<HotkeyHandler>,
    command: Option<CommandHandler>,
    balloon_clicked: Option<SimpleHandler>,
    session_changed: Option<SessionHandler>,
    display_changed: Option<SimpleHandler>,
    end_session: Option<SimpleHandler>,
    foreground_changed: Option<SimpleHandler>,
    menu_state: Option<MenuStateProvider>,
}

struct WindowInner {
    hwnd: AtomicIsize,
    win_event_hook: AtomicIsize,
    icon_added: AtomicBool,
    tray_icon: AtomicIsize,
    tray_light: AtomicBool,
    tip: Mutex<String>,
    hotkey_names: Mutex<HashMap<i32, String>>,
    invokes: Mutex<VecDeque<InvokeFn>>,
    callbacks: Mutex<WindowCallbacks>,
}

static ACTIVE_WINDOW: Mutex<Option<Arc<WindowInner>>> = Mutex::new(None);
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
static TASKBAR_MSG_CACHED: AtomicU32 = AtomicU32::new(0);

fn taskbar_created_msg() -> u32 {
    *TASKBAR_CREATED.get_or_init(|| {
        let w = to_wide_z("TaskbarCreated");
        let msg = unsafe { RegisterWindowMessageW(PCWSTR(w.as_ptr())) };
        TASKBAR_MSG_CACHED.store(msg, Ordering::Release);
        msg
    })
}

/// Hidden top-level Win32 message window. Must be created and pumped on one thread.
pub struct MessageWindow {
    inner: Arc<WindowInner>,
}

impl MessageWindow {
    pub fn new() -> Result<Arc<Self>, String> {
        let _ = taskbar_created_msg();
        let inner = Arc::new(WindowInner {
            hwnd: AtomicIsize::new(0),
            win_event_hook: AtomicIsize::new(0),
            icon_added: AtomicBool::new(false),
            tray_icon: AtomicIsize::new(0),
            tray_light: AtomicBool::new(false),
            tip: Mutex::new("BetterRecall".to_string()),
            hotkey_names: Mutex::new(HashMap::new()),
            invokes: Mutex::new(VecDeque::new()),
            callbacks: Mutex::new(WindowCallbacks {
                hotkey: None,
                command: None,
                balloon_clicked: None,
                session_changed: None,
                display_changed: None,
                end_session: None,
                foreground_changed: None,
                menu_state: None,
            }),
        });

        *ACTIVE_WINDOW.lock().unwrap_or_else(|e| e.into_inner()) = Some(inner.clone());

        let cls_w = to_wide_z("BetterRecall.Agent.Window");
        let hinst = unsafe { GetModuleHandleW(PCWSTR::null()) }
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(cls_w.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassExW(&wc) };
        if atom == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_CLASS_ALREADY_EXISTS {
                *ACTIVE_WINDOW.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Err(format!("RegisterClassEx failed ({}).", err.0));
            }
        }

        // Top-level hidden window (not HWND_MESSAGE) so it receives TaskbarCreated & WM_DISPLAYCHANGE.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(cls_w.as_ptr()),
                PCWSTR(cls_w.as_ptr()),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                None,
            )
        }
        .map_err(|e| {
            *ACTIVE_WINDOW.lock().unwrap_or_else(|e| e.into_inner()) = None;
            format!("CreateWindowEx failed: {e}")
        })?;

        inner.hwnd.store(hwnd.0 as isize, Ordering::Release);
        unsafe {
            let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
            let hook = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(on_win_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            inner.win_event_hook.store(hook.0 as isize, Ordering::Release);
        }

        Ok(Arc::new(Self { inner }))
    }

    pub fn handle(&self) -> isize {
        self.inner.hwnd.load(Ordering::Acquire)
    }

    pub fn set_on_hotkey(&self, cb: HotkeyHandler) {
        self.inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hotkey = Some(cb);
    }

    pub fn set_on_command(&self, cb: CommandHandler) {
        self.inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .command = Some(cb);
    }

    pub fn set_on_balloon_clicked(&self, cb: SimpleHandler) {
        self.inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .balloon_clicked = Some(cb);
    }

    pub fn set_on_session_changed(&self, cb: SessionHandler) {
        self.inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_changed = Some(cb);
    }

    pub fn set_on_display_changed(&self, cb: SimpleHandler) {
        self.inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .display_changed = Some(cb);
    }

    pub fn set_on_end_session(&self, cb: SimpleHandler) {
        self.inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .end_session = Some(cb);
    }

    pub fn set_on_foreground_changed(&self, cb: SimpleHandler) {
        self.inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .foreground_changed = Some(cb);
    }

    pub fn set_menu_state(&self, cb: MenuStateProvider) {
        self.inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .menu_state = Some(cb);
    }

    /// Runs the Win32 message loop until `quit()`.
    pub fn run(&self) {
        let mut msg = MSG::default();
        loop {
            let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if ret.0 <= 0 {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub fn quit(&self) {
        self.invoke(|| unsafe {
            PostQuitMessage(0);
        });
    }

    /// Posts an action to run on the message-window thread.
    pub fn invoke(&self, f: impl FnOnce() + Send + 'static) {
        let hwnd = self.inner.hwnd.load(Ordering::Acquire);
        if hwnd == 0 {
            return;
        }
        self.inner
            .invokes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(Box::new(f));
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut c_void)),
                WM_INVOKE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }

    pub fn register_hotkey(&self, id: i32, name: &str, hk: Hotkey) -> bool {
        let hwnd = HWND(self.inner.hwnd.load(Ordering::Acquire) as *mut c_void);
        if hwnd.0.is_null() {
            return false;
        }
        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
        let mods = HOT_KEY_MODIFIERS(hk.modifiers) | MOD_NOREPEAT;
        let ok = unsafe { RegisterHotKey(Some(hwnd), id, mods, hk.virtual_key) }.is_ok();
        if ok {
            self.inner
                .hotkey_names
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, name.to_string());
        } else {
            let err = unsafe { GetLastError().0 };
            AgentLog::warn(&format!(
                "hotkey {name} ({}) not registered: error {err}",
                HotkeyParser::format(hk)
            ));
        }
        ok
    }

    pub fn unregister_hotkey(&self, id: i32) {
        let hwnd = HWND(self.inner.hwnd.load(Ordering::Acquire) as *mut c_void);
        if !hwnd.0.is_null() {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), id);
            }
        }
        self.inner
            .hotkey_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    pub fn show_tray(&self, tip: &str) {
        show_tray_inner(&self.inner, tip);
    }

    pub fn update_tip(&self, tip: &str) {
        if !self.inner.icon_added.load(Ordering::Acquire) {
            return;
        }
        {
            let mut cur = self.inner.tip.lock().unwrap_or_else(|e| e.into_inner());
            if *cur == tip {
                return;
            }
            *cur = tip.to_string();
        }
        let mut d = new_notify_data(&self.inner);
        d.uFlags = NIF_TIP | NIF_SHOWTIP;
        set_wide_buf(&mut d.szTip, tip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }

    pub fn notify(&self, title: &str, text: &str) {
        if !self.inner.icon_added.load(Ordering::Acquire) {
            return;
        }
        let mut d = new_notify_data(&self.inner);
        d.uFlags = NIF_INFO;
        d.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
        set_wide_buf(&mut d.szInfoTitle, title);
        set_wide_buf(&mut d.szInfo, text);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }

    pub fn dispose(&self) {
        let hwnd_raw = self.inner.hwnd.swap(0, Ordering::AcqRel);
        if hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(hwnd_raw as *mut c_void);
        if self.inner.icon_added.swap(false, Ordering::AcqRel) {
            let d = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &d);
            }
        }
        let icon = self.inner.tray_icon.swap(0, Ordering::AcqRel);
        if icon != 0 {
            unsafe {
                let _ = DestroyIcon(HICON(icon as *mut c_void));
            }
        }
        let ids: Vec<i32> = self
            .inner
            .hotkey_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(k, _)| k)
            .collect();
        for id in ids {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), id);
            }
        }
        let hook = self.inner.win_event_hook.swap(0, Ordering::AcqRel);
        if hook != 0 {
            unsafe {
                let _ = UnhookWinEvent(HWINEVENTHOOK(hook as *mut c_void));
            }
        }
        unsafe {
            let _ = WTSUnRegisterSessionNotification(hwnd);
            let _ = DestroyWindow(hwnd);
        }
        let mut active = ACTIVE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        if active
            .as_ref()
            .is_some_and(|a| Arc::ptr_eq(a, &self.inner))
        {
            *active = None;
        }
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn new_notify_data(inner: &WindowInner) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(inner.hwnd.load(Ordering::Acquire) as *mut c_void),
        uID: 1,
        ..Default::default()
    }
}

fn set_wide_buf(dst: &mut [u16], s: &str) {
    let max = dst.len().saturating_sub(1);
    let mut i = 0;
    for ch in s.encode_utf16().take(max) {
        dst[i] = ch;
        i += 1;
    }
    if i < dst.len() {
        dst[i] = 0;
    }
}

fn taskbar_is_light() -> bool {
    reg_get_dword(
        HKEY_CURRENT_USER,
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "SystemUsesLightTheme",
    )
    .is_some_and(|v| v != 0)
}

fn tray_icon(inner: &WindowInner) -> HICON {
    let light = taskbar_is_light();
    let cur = inner.tray_icon.load(Ordering::Acquire);
    if cur != 0 && inner.tray_light.load(Ordering::Acquire) == light {
        return HICON(cur as *mut c_void);
    }
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let base_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_default();
    let path = base_dir
        .join("icons")
        .join(if light { "tray-light.ico" } else { "tray-dark.ico" });
    let loaded = if path.is_file() {
        let w = to_wide_z(&path.to_string_lossy());
        unsafe {
            LoadImageW(
                None,
                PCWSTR(w.as_ptr()),
                IMAGE_ICON,
                size,
                size,
                LR_LOADFROMFILE,
            )
        }
        .ok()
        .map(|h| HICON(h.0))
    } else {
        None
    };
    let Some(icon) = loaded else {
        return unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    };
    let old = inner.tray_icon.swap(icon.0 as isize, Ordering::AcqRel);
    if old != 0 {
        unsafe {
            let _ = DestroyIcon(HICON(old as *mut c_void));
        }
    }
    inner.tray_light.store(light, Ordering::Release);
    icon
}

fn show_tray_inner(inner: &WindowInner, tip: &str) {
    *inner.tip.lock().unwrap_or_else(|e| e.into_inner()) = tip.to_string();
    let mut d = new_notify_data(inner);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = tray_icon(inner);
    set_wide_buf(&mut d.szTip, tip);
    if !inner.icon_added.load(Ordering::Acquire) {
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &d) }.as_bool();
        inner.icon_added.store(added, Ordering::Release);
        d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
        }
    } else {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }
}

fn refresh_tray_icon(inner: &WindowInner) {
    if !inner.icon_added.load(Ordering::Acquire)
        || taskbar_is_light() == inner.tray_light.load(Ordering::Acquire)
    {
        return;
    }
    let mut d = new_notify_data(inner);
    d.uFlags = NIF_ICON;
    d.hIcon = tray_icon(inner);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

fn show_menu(inner: &WindowInner) {
    let state = {
        let provider = inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .menu_state
            .clone();
        provider
            .map(|f| f())
            .unwrap_or(MenuStateSnapshot {
                status: "BetterRecall".to_string(),
                paused: false,
                replay_on: false,
            })
    };
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let append = |flags, id: usize, text: Option<&str>| {
        let wide = text.map(to_wide_z);
        let ptr = wide.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr()));
        unsafe {
            let _ = AppendMenuW(menu, flags, id, ptr);
        }
    };
    append(MF_STRING | MF_GRAYED, TrayCommand::Status as usize, Some(&state.status));
    append(MF_SEPARATOR, 0, None);
    append(MF_STRING, TrayCommand::Search as usize, Some("Search"));
    if state.paused {
        append(MF_STRING, TrayCommand::Resume as usize, Some("Resume capture"));
    }
    append(MF_STRING, TrayCommand::Pause15 as usize, Some("Pause for 15 minutes"));
    append(MF_STRING, TrayCommand::Pause60 as usize, Some("Pause for 1 hour"));
    append(MF_STRING, TrayCommand::PauseUntilResume as usize, Some("Pause until resumed"));
    let replay_flags = if state.replay_on {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    append(replay_flags, TrayCommand::SaveReplay as usize, Some("Save replay"));
    append(MF_STRING, TrayCommand::Panic as usize, Some("Panic: flush memory now"));
    append(MF_SEPARATOR, 0, None);
    append(MF_STRING, TrayCommand::Settings as usize, Some("Settings"));
    append(MF_STRING, TrayCommand::Quit as usize, Some("Quit BetterRecall"));

    let mut pt = POINT::default();
    let hwnd = HWND(inner.hwnd.load(Ordering::Acquire) as *mut c_void);
    let cmd = unsafe {
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        TrackPopupMenuEx(
            menu,
            (TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        )
    };
    unsafe {
        let _ = DestroyMenu(menu);
    }
    if cmd.0 > 0
        && let Some(tc) = TrayCommand::from_u32(cmd.0 as u32)
    {
        let cb = inner
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .command
            .clone();
        if let Some(cb) = cb {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(tc)));
        }
    }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != 0 {
        return;
    }
    let inner = ACTIVE_WINDOW
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(w) = inner {
        let cb = w
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .foreground_changed
            .clone();
        if let Some(cb) = cb {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb()));
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let inner = ACTIVE_WINDOW
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(w) = inner else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    if hwnd.0 as isize != w.hwnd.load(Ordering::Acquire) {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    match msg {
        WM_HOTKEY => {
            let name = w
                .hotkey_names
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(wparam.0 as i32))
                .cloned();
            if let Some(name) = name {
                let cb = w
                    .callbacks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .hotkey
                    .clone();
                if let Some(cb) = cb {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(&name)));
                }
            }
            return LRESULT(0);
        }
        WM_TRAY => {
            let ev = (lparam.0 as u32) & 0xFFFF;
            if ev == WM_CONTEXTMENU || ev == WM_RBUTTONUP {
                show_menu(&w);
            } else if ev == NIN_SELECT || ev == NIN_KEYSELECT || ev == WM_LBUTTONUP {
                let cb = w
                    .callbacks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .command
                    .clone();
                if let Some(cb) = cb {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(TrayCommand::Search)
                    }));
                }
            } else if ev == NIN_BALLOONUSERCLICK {
                let cb = w
                    .callbacks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .balloon_clicked
                    .clone();
                if let Some(cb) = cb {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb()));
                }
            }
            return LRESULT(0);
        }
        WM_INVOKE => {
            loop {
                let next = w
                    .invokes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pop_front();
                let Some(f) = next else { break };
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            }
            return LRESULT(0);
        }
        WM_WTSSESSION_CHANGE => {
            let cb = w
                .callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .session_changed
                .clone();
            if let Some(cb) = cb {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(wparam.0 as i32)
                }));
            }
            return LRESULT(0);
        }
        WM_CLOSE => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE => {
            let cb = w
                .callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .display_changed
                .clone();
            if let Some(cb) = cb {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb()));
            }
            return LRESULT(0);
        }
        WM_QUERYENDSESSION => {
            return LRESULT(1);
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                let cb = w
                    .callbacks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .end_session
                    .clone();
                if let Some(cb) = cb {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb()));
                }
            }
            return LRESULT(0);
        }
        WM_SETTINGCHANGE => {
            refresh_tray_icon(&w);
        }
        _ => {}
    }

    let tb = TASKBAR_MSG_CACHED.load(Ordering::Acquire);
    if tb != 0 && msg == tb {
        w.icon_added.store(false, Ordering::Release);
        let tip = w.tip.lock().unwrap_or_else(|e| e.into_inner()).clone();
        show_tray_inner(&w, &tip);
        return LRESULT(0);
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
