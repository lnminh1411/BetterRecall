// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};

use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_KERNEL_OBJECT, SetSecurityInfo,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::ErrorReporting::WerAddExcludedApplication;
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
    LOAD_LIBRARY_SEARCH_USER_DIRS, SetDefaultDllDirectories,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessMitigationPolicy, OpenProcessToken,
    PROCESS_MITIGATION_POLICY, PROCESS_MODE_BACKGROUND_BEGIN,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    ProcessChildProcessPolicy, ProcessImageLoadPolicy, ProcessPowerThrottling,
    ProcessStrictHandleCheckPolicy, SetPriorityClass, SetProcessInformation,
    SetProcessMitigationPolicy,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use crate::hosting::Log;

/// Result of applying best-effort process hardening policies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HardeningReport {
    pub applied: Vec<String>,
    pub failed: Vec<String>,
}

/// `security.memoryIsolation` and `governance.ecoQos` for the worker process.
/// Every step is best effort: a failure is logged and reported, never fatal.
pub struct ProcessHardening;

impl ProcessHardening {
    pub fn apply(memory_isolation: bool, eco_qos: bool) -> HardeningReport {
        let mut r = HardeningReport::default();
        if memory_isolation {
            step(&mut r, "dll-search-app+system32", || unsafe {
                SetDefaultDllDirectories(
                    LOAD_LIBRARY_SEARCH_APPLICATION_DIR
                        | LOAD_LIBRARY_SEARCH_SYSTEM32
                        | LOAD_LIBRARY_SEARCH_USER_DIRS,
                )
                .map_err(win32_code_from_error)
            });
            // No child processes.
            step(&mut r, "no-child-process", || {
                set_policy(ProcessChildProcessPolicy, 0x1)
            });
            // Strict handle checks: raise on invalid handle use, permanently.
            step(&mut r, "strict-handle-checks", || {
                set_policy(ProcessStrictHandleCheckPolicy, 0x1 | 0x2)
            });
            // Image loads: no remote (UNC) images, no low-integrity images. PreferSystem32Images stays off on purpose:
            // System32 has its own onnxruntime.dll and DirectML.dll, which would shadow the bundled newer ones.
            // Inherited policy cannot be set again (sweep 3 D19): holding both flags counts as applied.
            step(&mut r, "image-load-no-remote-no-low-il", || {
                if has_policy(ProcessImageLoadPolicy, 0x1 | 0x2) {
                    Ok(())
                } else {
                    set_policy(ProcessImageLoadPolicy, 0x1 | 0x2)
                }
            });
            step(&mut r, "process-dacl", apply_process_dacl);
            step(&mut r, "wer-excluded", || {
                let exe = std::env::current_exe()
                    .map(|p| p.as_os_str().to_os_string())
                    .unwrap_or_else(|_| OsString::from("BetterRecall.Worker.exe"));
                let wide: Vec<u16> = exe.encode_wide().chain(Some(0)).collect();
                unsafe { WerAddExcludedApplication(PCWSTR(wide.as_ptr()), false) }
                    .map_err(win32_code_from_error)
            });
        }
        if eco_qos {
            step(&mut r, "ecoqos", enable_eco_qos);
            step(&mut r, "background-mode", || unsafe {
                SetPriorityClass(GetCurrentProcess(), PROCESS_MODE_BACKGROUND_BEGIN)
                    .map_err(win32_code_from_error)
            });
        }
        Log::info(&format!(
            "hardening applied=[{}] failed=[{}]",
            r.applied.join(","),
            r.failed.join(",")
        ));
        r
    }
}

fn step<F>(r: &mut HardeningReport, name: &str, f: F)
where
    F: FnOnce() -> Result<(), u32>,
{
    match f() {
        Ok(()) => r.applied.push(name.to_string()),
        Err(code) => r.failed.push(format!("{name}:{code}")),
    }
}

fn set_policy(policy: PROCESS_MITIGATION_POLICY, flags: u32) -> Result<(), u32> {
    let v = flags;
    unsafe {
        SetProcessMitigationPolicy(
            policy,
            (&raw const v).cast::<c_void>(),
            std::mem::size_of::<u32>(),
        )
        .map_err(win32_code_from_error)
    }
}

fn has_policy(policy: PROCESS_MITIGATION_POLICY, flags: u32) -> bool {
    let mut v = 0u32;
    let ok = unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            policy,
            (&raw mut v).cast::<c_void>(),
            std::mem::size_of::<u32>(),
        )
        .is_ok()
    };
    ok && (v & flags) == flags
}

fn enable_eco_qos() -> Result<(), u32> {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: 1,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    };
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast::<c_void>(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .map_err(win32_code_from_error)
    }
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct LocalMem(*mut c_void);
impl Drop for LocalMem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0)));
            }
        }
    }
}

fn current_user_sid() -> Result<String, u32> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(win32_code_from_error)?;
        let _token_guard = OwnedHandle(token);

        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return Err(GetLastError().0);
        }
        let mut buf = vec![0u8; needed as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            needed,
            &mut needed,
        )
        .map_err(win32_code_from_error)?;

        let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
        let mut sid_pwstr = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid_pwstr).map_err(win32_code_from_error)?;
        let _sid_guard = LocalMem(sid_pwstr.0.cast::<c_void>());
        let mut len = 0usize;
        while *sid_pwstr.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(sid_pwstr.0, len);
        Ok(OsString::from_wide(slice).to_string_lossy().into_owned())
    }
}

/// Process DACL: SYSTEM full; the user (and OWNER RIGHTS, so the owner gets no implicit WRITE_DAC)
/// only query, synchronize, terminate. Denies VM read/write, create thread, dup handle to non-SYSTEM.
fn apply_process_dacl() -> Result<(), u32> {
    let sid = current_user_sid()?;
    // 0x00121401 = SYNCHRONIZE | READ_CONTROL | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE
    let sddl = format!("D:P(A;;GA;;;SY)(A;;0x00121401;;;{sid})(A;;0x00121401;;;OW)");
    let sddl_w: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .map_err(win32_code_from_error)?;
        let _sd_guard = LocalMem(sd.0);

        let mut present = BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut defaulted = BOOL(0);
        GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted)
            .map_err(win32_code_from_error)?;
        let dacl_opt = if present.as_bool() && !dacl.is_null() {
            Some(dacl.cast_const())
        } else {
            None
        };

        let err = SetSecurityInfo(
            GetCurrentProcess(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            dacl_opt,
            None,
        );
        if err.0 != 0 {
            return Err(err.0);
        }
        Ok(())
    }
}

fn win32_code_from_error(e: windows::core::Error) -> u32 {
    let raw = e.code().0 as u32;
    if (raw & 0xFFFF_0000) == 0x8007_0000 {
        raw & 0xFFFF
    } else {
        raw
    }
}
