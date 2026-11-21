// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Windows job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`KillOnCloseJob.cs`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};

/// Every process assigned to this job (and its children) is killed when the handle closes,
/// which the OS also does when the agent exits or crashes.
pub struct KillOnCloseJob {
    handle: AtomicIsize,
}

impl KillOnCloseJob {
    pub fn try_create() -> Option<Self> {
        let h = unsafe { CreateJobObjectW(None, None) }.ok()?;
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        let ok = unsafe {
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast::<c_void>(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok.is_err() {
            unsafe {
                let _ = CloseHandle(h);
            }
            return None;
        }
        Some(Self {
            handle: AtomicIsize::new(h.0 as isize),
        })
    }

    /// Assigns a running process handle to the job.
    ///
    /// # Safety
    /// `process_handle` must be a valid Win32 process handle.
    pub unsafe fn assign(&self, process_handle: HANDLE) -> bool {
        let h = self.handle.load(Ordering::Acquire);
        if h == 0 {
            return false;
        }
        unsafe { AssignProcessToJobObject(HANDLE(h as *mut c_void), process_handle) }.is_ok()
    }

    /// Closes the job handle immediately, killing every process in it.
    pub fn close(&self) {
        let h = self.handle.swap(0, Ordering::AcqRel);
        if h != 0 {
            unsafe {
                let _ = CloseHandle(HANDLE(h as *mut c_void));
            }
        }
    }

    /// Native size of `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` (144 bytes on x64).
    pub const fn extended_limit_info_size() -> usize {
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        self.close();
    }
}
