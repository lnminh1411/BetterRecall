// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Process hardening (`security.memoryIsolation`, `MemoryIsolation.cs`).
//!
//! Applied in two stages:
//! - Early (before devices and encoders load): process DACL, WER exclusion, strict handle checks, image load policy.
//! - Late (after capture/audio/UIA initialise): dynamic code prohibited (ACG).
//!
//! Each step logs its result; failures never stop the agent.

use std::ffi::c_void;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use br_contracts::{InstallLayout, PipeNames};
use windows::Win32::Foundation::{GetLastError, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SetKernelObjectSecurity,
};
use windows::Win32::System::ErrorReporting::WerAddExcludedApplication;
use windows::Win32::System::Memory::{
    GetProcessWorkingSetSizeEx, SETPROCESSWORKINGSETSIZEEX_FLAGS, SetProcessWorkingSetSizeEx,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessDynamicCodePolicy, ProcessImageLoadPolicy,
    ProcessStrictHandleCheckPolicy, SetProcessMitigationPolicy,
};
use windows::core::PCWSTR;

use crate::native::to_wide_z;
use crate::util::AgentLog;

static APPLIED: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static SKIPPED: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub struct MemoryIsolation;

impl MemoryIsolation {
    pub fn applied() -> Vec<String> {
        APPLIED.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn skipped() -> Vec<String> {
        SKIPPED.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn apply_early(_install_dir: &Path) {
        step("process-dacl", set_process_dacl);
        step("wer-exclusion", || {
            for exe in [
                InstallLayout::AGENT_EXE,
                InstallLayout::CLI_EXE,
                InstallLayout::MCP_EXE,
                "BetterRecall.Worker.exe",
                "BetterRecall.UI.exe",
            ] {
                let name = Path::new(exe)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(exe);
                let w = to_wide_z(name);
                if unsafe { WerAddExcludedApplication(PCWSTR(w.as_ptr()), false) }.is_err() {
                    return false;
                }
            }
            true
        });
        step("strict-handles", || {
            let flags: u32 = 0x1 | 0x2; // RaiseExceptionOnInvalidHandleReference | HandleExceptionsPermanentlyEnabled
            unsafe {
                SetProcessMitigationPolicy(
                    ProcessStrictHandleCheckPolicy,
                    (&raw const flags).cast::<c_void>(),
                    std::mem::size_of::<u32>(),
                )
            }
            .is_ok()
        });
        step("image-load", || {
            // NoRemoteImages | NoLowMandatoryLabelImages | PreferSystem32Images
            let flags: u32 = 0x1 | 0x2 | 0x4;
            unsafe {
                SetProcessMitigationPolicy(
                    ProcessImageLoadPolicy,
                    (&raw const flags).cast::<c_void>(),
                    std::mem::size_of::<u32>(),
                )
            }
            .is_ok()
        });
    }

    /// Arbitrary Code Guard: no new executable memory.
    pub fn apply_late(dynamic_code: bool) {
        if !dynamic_code {
            SKIPPED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("dynamic-code (disabled by caller)".to_string());
            return;
        }
        step("dynamic-code", || {
            let flags: u32 = 0x1; // ProhibitDynamicCode
            unsafe {
                SetProcessMitigationPolicy(
                    ProcessDynamicCodePolicy,
                    (&raw const flags).cast::<c_void>(),
                    std::mem::size_of::<u32>(),
                )
            }
            .is_ok()
        });
    }

    /// Raises the working-set minimum so `VirtualLock` can pin replay rings (soft limits).
    pub fn reserve_working_set(locked_bytes: i64) -> bool {
        let self_proc = unsafe { GetCurrentProcess() };
        let mut min: usize = 0;
        let mut max: usize = 0;
        let mut flags: u32 = 0;
        if !unsafe { GetProcessWorkingSetSizeEx(self_proc, &mut min, &mut max, &mut flags) }
            .as_bool()
        {
            return false;
        }
        let want_min = (min as i64).max(locked_bytes + 96 * 1024 * 1024) as usize;
        let want_max = (max as i64).max(want_min as i64 + 512 * 1024 * 1024) as usize;
        const MIN_DISABLE: u32 = 0x2;
        const MAX_DISABLE: u32 = 0x8;
        let ok = unsafe {
            SetProcessWorkingSetSizeEx(
                self_proc,
                want_min,
                want_max,
                SETPROCESSWORKINGSETSIZEEX_FLAGS(MIN_DISABLE | MAX_DISABLE),
            )
        };
        if ok.is_err() {
            let err = unsafe { GetLastError().0 };
            AgentLog::warn(&format!("SetProcessWorkingSetSizeEx failed ({err})"));
            false
        } else {
            true
        }
    }
}

fn set_process_dacl() -> bool {
    let Ok(sid) = PipeNames::user_sid() else {
        return false;
    };
    // 0x121401 = TERMINATE | QUERY_INFORMATION | QUERY_LIMITED_INFORMATION | READ_CONTROL | SYNCHRONIZE
    let sddl = format!("D:P(A;;GA;;;SY)(A;;0x121401;;;{sid})(A;;RC;;;OW)");
    let sddl_w = to_wide_z(&sddl);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .is_err()
    {
        return false;
    }
    let ok = unsafe {
        SetKernelObjectSecurity(GetCurrentProcess(), DACL_SECURITY_INFORMATION, sd).is_ok()
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    ok
}

fn step(name: &str, action: impl FnOnce() -> bool) {
    if action() {
        APPLIED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(name.to_string());
        AgentLog::info(&format!("isolation {name} applied"));
    } else {
        let err = unsafe { GetLastError().0 };
        SKIPPED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{name} (error {err})"));
        AgentLog::warn(&format!("isolation {name} failed ({err})"));
    }
}
