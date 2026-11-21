// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sub-millisecond sleeps without raising the global timer resolution (`HighResTimer.cs`).

use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    WaitForSingleObject,
};

const TIMER_ALL_ACCESS: u32 = 0x001F_0003;

/// Sub-millisecond waitable timer backed by `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`.
pub struct HighResTimer {
    handle: HANDLE,
}

unsafe impl Send for HighResTimer {}

impl Default for HighResTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl HighResTimer {
    pub fn new() -> Self {
        let h = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        }
        .or_else(|_| unsafe { CreateWaitableTimerExW(None, None, 0, TIMER_ALL_ACCESS) })
        .unwrap_or(HANDLE::default());
        Self { handle: h }
    }

    /// Sleeps for the given number of 100 ns units.
    pub fn sleep(&self, hundred_ns: i64) {
        if hundred_ns <= 0 {
            return;
        }
        if self.handle.is_invalid() || self.handle.0.is_null() {
            thread::sleep(Duration::from_millis((hundred_ns / 10_000).max(1) as u64));
            return;
        }
        let due = -hundred_ns;
        let ok = unsafe { SetWaitableTimer(self.handle, &due, 0, None, None, false) };
        if ok.is_err() {
            thread::sleep(Duration::from_millis((hundred_ns / 10_000).max(1) as u64));
            return;
        }
        unsafe {
            let _ = WaitForSingleObject(self.handle, INFINITE);
        }
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        if !self.handle.is_invalid() && !self.handle.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
            self.handle = HANDLE::default();
        }
    }
}
