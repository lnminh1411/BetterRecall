// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, GetTokenInformation, InitializeSecurityDescriptor, NO_INHERITANCE, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, OpenEventW,
    OpenProcessToken, SetEvent, WaitForMultipleObjects,
};
use windows::core::{PCWSTR, PWSTR};

pub const MUTEX_NAME: &str = r"Local\BetterRecall.UI.Gpui.mock";
pub const SHOW_LAUNCHER_EVENT: &str = r"Local\BetterRecall.UI.ShowLauncher.mock";
pub const QUIT_EVENT: &str = r"Local\BetterRecall.UI.Quit.mock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSignal {
    ShowLauncher,
    Quit,
}

fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

/// Holds the process-wide single-instance mutex until dropped.
pub struct SingleInstanceGuard {
    handle: HANDLE,
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

/// Acquire `Local\BetterRecall.UI.Gpui.mock`. If another instance already holds it,
/// signal `Local\BetterRecall.UI.ShowLauncher.mock` and return `None`.
pub fn acquire_single_instance() -> Option<SingleInstanceGuard> {
    let wide = to_wide(MUTEX_NAME);
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }.ok()?;
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        signal_existing_launcher();
        unsafe {
            let _ = CloseHandle(handle);
        }
        return None;
    }
    Some(SingleInstanceGuard { handle })
}

fn signal_existing_launcher() {
    let wide = to_wide(SHOW_LAUNCHER_EVENT);
    if let Ok(evt) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(wide.as_ptr())) } {
        unsafe {
            let _ = SetEvent(evt);
            let _ = CloseHandle(evt);
        }
    }
}

/// Create a named event whose DACL grants `SYNCHRONIZE | EVENT_MODIFY_STATE` to the
/// current user SID only, matching `LauncherSignal.cs`.
fn create_user_only_event(name: &str, manual_reset: bool) -> Result<HANDLE, String> {
    let wide_name = to_wide(name);

    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| format!("OpenProcessToken: {e}"))?;

    let mut needed = 0u32;
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
    let mut buf = vec![0u8; needed.max(64) as usize];
    let info_res = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            buf.len() as u32,
            &mut needed,
        )
    };
    unsafe {
        let _ = CloseHandle(token);
    }
    info_res.map_err(|e| format!("GetTokenInformation(TokenUser): {e}"))?;

    let token_user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
    let user_sid = token_user.User.Sid;

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: SYNCHRONIZE.0 | EVENT_MODIFY_STATE.0,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: Default::default(),
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: PWSTR(user_sid.0.cast::<u16>()),
        },
    };

    let mut dacl: *mut ACL = ptr::null_mut();
    let acl_err = unsafe { SetEntriesInAclW(Some(&[ea]), None, &mut dacl) };
    if acl_err.0 != 0 {
        return Err(format!("SetEntriesInAclW failed: {}", acl_err.0));
    }

    let mut sd = SECURITY_DESCRIPTOR::default();
    let p_sd = PSECURITY_DESCRIPTOR((&mut sd as *mut SECURITY_DESCRIPTOR).cast::<c_void>());

    let init_res = unsafe { InitializeSecurityDescriptor(p_sd, SECURITY_DESCRIPTOR_REVISION) };
    if let Err(e) = init_res {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(dacl.cast::<c_void>())));
        }
        return Err(format!("InitializeSecurityDescriptor: {e}"));
    }

    let dacl_res = unsafe { SetSecurityDescriptorDacl(p_sd, true, Some(dacl), false) };
    if let Err(e) = dacl_res {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(dacl.cast::<c_void>())));
        }
        return Err(format!("SetSecurityDescriptorDacl: {e}"));
    }

    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: p_sd.0,
        bInheritHandle: false.into(),
    };

    let evt_res =
        unsafe { CreateEventW(Some(&sa), manual_reset, false, PCWSTR(wide_name.as_ptr())) };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(dacl.cast::<c_void>())));
    }
    evt_res.map_err(|e| format!("CreateEventW({name}): {e}"))
}

#[derive(Clone, Copy)]
struct SendHandle(usize);
impl SendHandle {
    fn from_handle(h: HANDLE) -> Self {
        Self(h.0 as usize)
    }
    fn as_handle(self) -> HANDLE {
        HANDLE(self.0 as *mut c_void)
    }
}

/// Background thread listening on `ShowLauncher` and `Quit` named events.
pub struct EventListener {
    stop_event: SendHandle,
    thread: Option<JoinHandle<()>>,
}

impl EventListener {
    pub fn start(tx: async_channel::Sender<AppSignal>) -> Result<Self, String> {
        let show_evt = create_user_only_event(SHOW_LAUNCHER_EVENT, false)?;
        let quit_evt = match create_user_only_event(QUIT_EVENT, true) {
            Ok(h) => h,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(show_evt);
                }
                return Err(e);
            }
        };
        let stop_evt = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(h) => h,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(show_evt);
                    let _ = CloseHandle(quit_evt);
                }
                return Err(format!("CreateEventW(stop): {e}"));
            }
        };

        let show_s = SendHandle::from_handle(show_evt);
        let quit_s = SendHandle::from_handle(quit_evt);
        let stop_s = SendHandle::from_handle(stop_evt);

        let thread = thread::Builder::new()
            .name("br-ui-events".into())
            .spawn(move || {
                let handles = [show_s.as_handle(), quit_s.as_handle(), stop_s.as_handle()];
                loop {
                    let res = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
                    if res == WAIT_OBJECT_0 {
                        let _ = tx.try_send(AppSignal::ShowLauncher);
                    } else if res.0 == WAIT_OBJECT_0.0 + 1 {
                        let _ = tx.try_send(AppSignal::Quit);
                        break;
                    } else {
                        break;
                    }
                }
                unsafe {
                    let _ = CloseHandle(show_s.as_handle());
                    let _ = CloseHandle(quit_s.as_handle());
                }
            })
            .map_err(|e| format!("spawn event thread: {e}"))?;

        Ok(Self {
            stop_event: stop_s,
            thread: Some(thread),
        })
    }
}

impl Drop for EventListener {
    fn drop(&mut self) {
        unsafe {
            let _ = SetEvent(self.stop_event.as_handle());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let _ = CloseHandle(self.stop_event.as_handle());
        }
    }
}
