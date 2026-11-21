// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Visual C++ 2015-2022 Redistributable (x64) presence probe (`VcRuntime.cs`).

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex};

use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

use crate::native::tick_count64;

static WORKER_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));
static CHECKED_AT: AtomicI64 = AtomicI64::new(i64::MIN);
static PRESENT: AtomicBool = AtomicBool::new(true);

pub struct VcRuntime;

impl VcRuntime {
    pub const MISSING_MESSAGE: &'static str = "The Microsoft Visual C++ 2015-2022 Redistributable (x64) is not installed: search, text recognition and transcription need it.";

    pub const DLLS: &'static [&'static str] =
        &["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"];

    pub fn set_worker_dir(dir: Option<PathBuf>) {
        *WORKER_DIR.lock().unwrap_or_else(|e| e.into_inner()) = dir;
        CHECKED_AT.store(i64::MIN, Ordering::Release);
    }

    pub fn worker_dir() -> Option<PathBuf> {
        WORKER_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Cached for 60 seconds so status polling stays cheap and an install made meanwhile is seen.
    pub fn present() -> bool {
        let now = tick_count64();
        if Self::fresh(CHECKED_AT.load(Ordering::Acquire), now) {
            return PRESENT.load(Ordering::Acquire);
        }
        let dir = Self::worker_dir();
        let sys = system_directory();
        let ok = Self::probe(dir.as_deref(), &sys, None);
        PRESENT.store(ok, Ordering::Release);
        CHECKED_AT.store(now, Ordering::Release);
        ok
    }

    /// True when the cached answer is under a minute old (`i64::MIN` is never fresh).
    pub fn fresh(checked_at: i64, now: i64) -> bool {
        checked_at != i64::MIN && now.wrapping_sub(checked_at) < 60_000
    }

    /// Every DLL is a file in either `worker_dir` or `system_dir`. A file check, not a load.
    pub fn probe(worker_dir: Option<&Path>, system_dir: &Path, dlls: Option<&[&str]>) -> bool {
        let list = dlls.unwrap_or(Self::DLLS);
        for dll in list {
            let found = worker_dir.is_some_and(|d| d.join(dll).is_file())
                || system_dir.join(dll).is_file();
            if !found {
                return false;
            }
        }
        true
    }
}

fn system_directory() -> PathBuf {
    let mut buf = [0u16; 512];
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) };
    if n > 0 && (n as usize) <= buf.len() {
        PathBuf::from(OsString::from_wide(&buf[..n as usize]))
    } else {
        PathBuf::from(r"C:\Windows\System32")
    }
}
