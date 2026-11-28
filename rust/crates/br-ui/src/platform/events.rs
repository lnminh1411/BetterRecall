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
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, MUTEX_ALL_ACCESS,
    OpenEventW, OpenProcessToken, ResetEvent, SetEvent, WaitForMultipleObjects,
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

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_raw(mut self) -> HANDLE {
        let h = self.0;
        self.0 = HANDLE::default();
        h
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct OwnedLocalAcl(*mut ACL);

impl Drop for OwnedLocalAcl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.cast::<c_void>())));
            }
        }
    }
}

/// Build a `SECURITY_ATTRIBUTES` whose DACL grants `access_mask` exclusively to the
/// current process's user SID, then invoke `f` while the descriptor and ACL remain alive.
fn with_user_only_sa<T>(
    access_mask: u32,
    f: impl FnOnce(&SECURITY_ATTRIBUTES) -> Result<T, String>,
) -> Result<T, String> {
    let mut raw_token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) }
        .map_err(|e| format!("OpenProcessToken: {e}"))?;
    let token = OwnedHandle(raw_token);

    let mut needed = 0u32;
    let _ = unsafe { GetTokenInformation(token.raw(), TokenUser, None, 0, &mut needed) };
    // Use a `u64`-backed buffer so casting to `TOKEN_USER` meets pointer/struct alignment.
    let word_count = (needed as usize).div_ceil(size_of::<u64>()).max(8);
    let mut buf = vec![0u64; word_count];
    let buf_bytes = (buf.len() * size_of::<u64>()) as u32;
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            buf_bytes,
            &mut needed,
        )
    }
    .map_err(|e| format!("GetTokenInformation(TokenUser): {e}"))?;
    drop(token);

    let token_user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
    let user_sid = token_user.User.Sid;

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
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

    let mut dacl_ptr: *mut ACL = ptr::null_mut();
    let acl_err = unsafe { SetEntriesInAclW(Some(&[ea]), None, &mut dacl_ptr) };
    if acl_err.0 != 0 {
        return Err(format!("SetEntriesInAclW failed: {}", acl_err.0));
    }
    let _dacl_guard = OwnedLocalAcl(dacl_ptr);

    let mut sd = SECURITY_DESCRIPTOR::default();
    let p_sd = PSECURITY_DESCRIPTOR((&mut sd as *mut SECURITY_DESCRIPTOR).cast::<c_void>());
    unsafe { InitializeSecurityDescriptor(p_sd, SECURITY_DESCRIPTOR_REVISION) }
        .map_err(|e| format!("InitializeSecurityDescriptor: {e}"))?;
    unsafe { SetSecurityDescriptorDacl(p_sd, true, Some(dacl_ptr), false) }
        .map_err(|e| format!("SetSecurityDescriptorDacl: {e}"))?;

    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: p_sd.0,
        bInheritHandle: false.into(),
    };

    f(&sa)
}

/// Holds the process-wide single-instance mutex until dropped.
pub struct SingleInstanceGuard {
    _handle: OwnedHandle,
}

/// Acquire `Local\BetterRecall.UI.Gpui.mock` with a current-user-only DACL. If another
/// instance already holds it, signal `Local\BetterRecall.UI.ShowLauncher.mock` and return `None`.
pub fn acquire_single_instance() -> Option<SingleInstanceGuard> {
    let wide = to_wide(MUTEX_NAME);
    let created = with_user_only_sa(MUTEX_ALL_ACCESS.0, |sa| {
        let handle = unsafe { CreateMutexW(Some(sa), false, PCWSTR(wide.as_ptr())) }
            .map_err(|e| format!("CreateMutexW({MUTEX_NAME}): {e}"))?;
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok((OwnedHandle(handle), already_exists))
    });

    match created {
        Ok((handle, true)) => {
            signal_existing_launcher();
            drop(handle);
            None
        }
        Ok((handle, false)) => Some(SingleInstanceGuard { _handle: handle }),
        Err(err) => {
            eprintln!("br-ui: failed to acquire single-instance mutex: {err}");
            None
        }
    }
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
/// current user SID only, matching `LauncherSignal.cs`. Returns `(handle, already_exists)`.
fn create_user_only_event(name: &str, manual_reset: bool) -> Result<(OwnedHandle, bool), String> {
    let wide_name = to_wide(name);
    with_user_only_sa(SYNCHRONIZE.0 | EVENT_MODIFY_STATE.0, |sa| {
        let handle =
            unsafe { CreateEventW(Some(sa), manual_reset, false, PCWSTR(wide_name.as_ptr())) }
                .map_err(|e| format!("CreateEventW({name}): {e}"))?;
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok((OwnedHandle(handle), already_exists))
    })
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
        let (show_evt, _) = create_user_only_event(SHOW_LAUNCHER_EVENT, false)?;
        // Auto-reset so a signaled Quit event is consumed once; also clear any stale signal
        // if an existing kernel object was opened while this instance owns the mutex.
        let (quit_evt, quit_already_exists) = create_user_only_event(QUIT_EVENT, false)?;
        if quit_already_exists {
            unsafe {
                let _ = ResetEvent(quit_evt.raw());
            }
        }
        let stop_evt = OwnedHandle(
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
                .map_err(|e| format!("CreateEventW(stop): {e}"))?,
        );

        let show_s = SendHandle::from_handle(show_evt.raw());
        let quit_s = SendHandle::from_handle(quit_evt.raw());
        let stop_s = SendHandle::from_handle(stop_evt.raw());

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

        // Ownership transferred to the background thread (`show_evt`, `quit_evt`) and `Self` (`stop_evt`).
        let _ = show_evt.into_raw();
        let _ = quit_evt.into_raw();
        let _ = stop_evt.into_raw();

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
