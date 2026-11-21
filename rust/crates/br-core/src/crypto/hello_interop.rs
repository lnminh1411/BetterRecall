// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Windows Hello via WinRT `Windows.Security.Credentials.UI.UserConsentVerifier` and
//! `IUserConsentVerifierInterop::RequestVerificationForWindowAsync` for HWND-owned prompts.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use windows::Security::Credentials::UI::{
    UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoGetActivationFactory, RoInitialize};
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface};
use windows_future::{AsyncStatus, IAsyncInfo, IAsyncOperation};

use crate::log::Log;
use crate::win32;

const CLASS_NAME: &str = "Windows.Security.Credentials.UI.UserConsentVerifier";
const IID_INTEROP: GUID = GUID::from_u128(0x39E050C3_4E74_441A_8DC0_B81104DF949C);
const IASYNC_OP_PIID: &str = "{9fc2b0bb-e446-44e2-aa61-9cab8f636af2}";
const WINRT_PARAM_NS: u128 = 0x11f47ad5_7b73_42c0_abae_878b1e16adee;

pub struct HelloInterop;

impl HelloInterop {
    /// IID of `IAsyncOperation<UserConsentVerificationResult>` (parameterized WinRT interface IID).
    pub fn iid_async_op_result() -> GUID {
        Self::parameterized_iid(&format!(
            "pinterface({IASYNC_OP_PIID};enum(Windows.Security.Credentials.UI.UserConsentVerificationResult;i4))"
        ))
    }

    /// WinRT parameterized IID: SHA-1 over the WinRT namespace GUID and the type signature, UUID v5.
    pub fn parameterized_iid(signature: &str) -> GUID {
        let mut data = Vec::with_capacity(16 + signature.len());
        data.extend_from_slice(&WINRT_PARAM_NS.to_be_bytes());
        data.extend_from_slice(signature.as_bytes());
        let mut hash = win32::sha1(&data);
        hash[6] = (hash[6] & 0x0F) | 0x50;
        hash[8] = (hash[8] & 0x3F) | 0x80;
        let mut guid_bytes = [0u8; 16];
        guid_bytes.copy_from_slice(&hash[..16]);
        GUID::from_u128(u128::from_be_bytes(guid_bytes))
    }

    /// True when `UserConsentVerifier` reports `Available`. False on any failure. Never opens a prompt.
    pub fn check_available() -> bool {
        run_mta(|| {
            let Ok(op) = UserConsentVerifier::CheckAvailabilityAsync() else {
                return false;
            };
            op.join()
                .map(|avail| avail == UserConsentVerifierAvailability::Available)
                .unwrap_or(false)
        })
    }

    /// Shows the Windows Hello prompt owned by `hwnd` (or unowned when `hwnd == 0`).
    /// Returns `true` only for `Verified`. False when unavailable, cancelled, or on error.
    pub fn verify(hwnd: isize, message: &str, cancel: Option<&AtomicBool>) -> bool {
        let msg = message.to_string();
        let cancelled_ptr = cancel.map(|c| c as *const AtomicBool as usize);
        run_mta(move || {
            let hmsg = HSTRING::from(msg.as_str());
            let op: IAsyncOperation<UserConsentVerificationResult> = if hwnd != 0 {
                let Some(op) = request_for_window(hwnd, &hmsg) else {
                    return false;
                };
                op
            } else {
                match UserConsentVerifier::RequestVerificationAsync(&hmsg) {
                    Ok(op) => op,
                    Err(e) => {
                        Log::warn(&format!(
                            "Hello RequestVerificationAsync failed 0x{:08X}",
                            e.code().0 as u32
                        ));
                        return false;
                    }
                }
            };
            wait_result(&op, cancelled_ptr)
        })
    }
}

fn run_mta<F>(body: F) -> bool
where
    F: FnOnce() -> bool + Send + 'static,
{
    thread::Builder::new()
        .name("BetterRecall.Hello".to_string())
        .spawn(move || {
            unsafe {
                let _ = RoInitialize(RO_INIT_MULTITHREADED);
            }
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or_else(|_| {
                Log::warn("Hello call failed");
                false
            })
        })
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or(false)
}

fn request_for_window(
    hwnd: isize,
    message: &HSTRING,
) -> Option<IAsyncOperation<UserConsentVerificationResult>> {
    let class_id = HSTRING::from(CLASS_NAME);
    let factory: IUnknown = unsafe { RoGetActivationFactory(&class_id) }.ok()?;
    let mut interop_raw: *mut c_void = std::ptr::null_mut();
    let qi_hr = unsafe { factory.query(&IID_INTEROP, &mut interop_raw) };
    if qi_hr.is_err() || interop_raw.is_null() {
        return None;
    }
    let _interop_guard = unsafe { IUnknown::from_raw(interop_raw) };

    type RequestForWindowFn = unsafe extern "system" fn(
        this: *mut c_void,
        app_window: isize,
        message: *mut c_void,
        riid: *const GUID,
        async_operation: *mut *mut c_void,
    ) -> HRESULT;

    let riid = HelloInterop::iid_async_op_result();
    let mut op_raw: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
        let vtable = *(interop_raw as *const *const *const c_void);
        let func: RequestForWindowFn = std::mem::transmute(*vtable.add(6));
        func(
            interop_raw,
            hwnd,
            std::mem::transmute_copy(message),
            &riid,
            &mut op_raw,
        )
    };
    if hr.is_err() || op_raw.is_null() {
        Log::warn(&format!(
            "Hello RequestVerificationForWindowAsync failed 0x{:08X}",
            hr.0 as u32
        ));
        return None;
    }
    Some(unsafe { IAsyncOperation::from_raw(op_raw) })
}

fn wait_result(
    op: &IAsyncOperation<UserConsentVerificationResult>,
    cancelled_ptr: Option<usize>,
) -> bool {
    let Ok(info) = op.cast::<IAsyncInfo>() else {
        return false;
    };
    loop {
        let Ok(status) = info.Status() else {
            return false;
        };
        if status == AsyncStatus::Completed {
            break;
        }
        if status == AsyncStatus::Canceled || status == AsyncStatus::Error {
            return false;
        }
        if let Some(ptr) = cancelled_ptr {
            // SAFETY: caller awaits `run_mta` thread join before `cancel` reference expires.
            let flag = unsafe { &*(ptr as *const AtomicBool) };
            if flag.load(Ordering::Relaxed) {
                let _ = info.Cancel();
                return false;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    op.GetResults()
        .map(|r| r == UserConsentVerificationResult::Verified)
        .unwrap_or(false)
}
