// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Peer process token/image inspection and clock-step-aware PID reuse protection (`ProcessInfo.cs`).

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use br_contracts::{InstallLayout, PipeNames};
use chrono::{DateTime, Duration, Utc};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TOKEN_USER, TokenIntegrityLevel, TokenIsAppContainer, TokenUser,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

use crate::log::Log;
use crate::win32;

/// Facts about a peer process read from its token and image name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub pid: i32,
    pub session: u32,
    pub sid: Option<String>,
    pub integrity_rid: i32,
    pub app_container: bool,
    pub image: Option<String>,
    /// Start time of the process inspected, read from the same handle (PID reuse check).
    pub created_utc: Option<DateTime<Utc>>,
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

/// Process inspection and PID-reuse helpers (`ProcessInfo.cs`).
pub struct ProcessInfo;

impl ProcessInfo {
    /// Backward clock steps up to this size are left to the wall clock.
    pub const STEP_TOLERANCE_SECS: i64 = 10;

    pub fn current_session() -> u32 {
        static OWN_SESSION: OnceLock<u32> = OnceLock::new();
        *OWN_SESSION.get_or_init(|| {
            win32::process_id_to_session_id(win32::current_process_id()).unwrap_or(u32::MAX)
        })
    }

    pub fn current_sid() -> &'static str {
        static OWN_SID: OnceLock<String> = OnceLock::new();
        OWN_SID
            .get_or_init(|| PipeNames::user_sid().expect("current process user SID"))
            .as_str()
    }

    pub fn current_pid() -> i32 {
        win32::current_process_id() as i32
    }

    pub fn current_image() -> Option<String> {
        std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    pub fn session_of(pid: i32) -> u32 {
        if pid < 0 {
            return u32::MAX;
        }
        win32::process_id_to_session_id(pid as u32).unwrap_or(u32::MAX)
    }

    pub fn image_path_by_pid(pid: i32) -> Option<String> {
        if pid < 0 {
            return None;
        }
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) }.ok()?;
        let _guard = OwnedHandle(h);
        image_path_handle(h)
    }

    fn system_started() -> Option<DateTime<Utc>> {
        static SYS_STARTED: OnceLock<Option<DateTime<Utc>>> = OnceLock::new();
        *SYS_STARTED.get_or_init(|| {
            let pid4 = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, 4) }
                .ok()
                .and_then(|h| {
                    let _guard = OwnedHandle(h);
                    created_utc_handle(h)
                });
            if pid4.is_none() {
                Log::info("boot time not readable (standard user): clock-step checks count from now");
            }
            Some(Self::boot_anchor(pid4, Utc::now(), win32::tick_count64()))
        })
    }

    /// N3: the boot reference for clock-step detection. The System process start time when
    /// readable; otherwise (a standard user cannot open pid 4) wall time now minus uptime,
    /// taken once, so later backward clock steps are still seen.
    pub fn boot_anchor(
        pid4_started: Option<DateTime<Utc>>,
        wall_now_utc: DateTime<Utc>,
        ms_since_boot: i64,
    ) -> DateTime<Utc> {
        pid4_started.unwrap_or_else(|| wall_now_utc - Duration::milliseconds(ms_since_boot))
    }

    fn global_step_clock() -> &'static StepClock {
        static STEPS: OnceLock<StepClock> = OnceLock::new();
        STEPS.get_or_init(|| {
            StepClock::new(Some(Box::new(|step| {
                let secs = step.num_milliseconds() as f64 / 1000.0;
                Log::info(&format!(
                    "clock stepped back about {secs:.0} s: pid-reuse checks accept process start times up to that far ahead until the clock catches up"
                ));
            })))
        })
    }

    /// Latest start time a process that already ran at `wall_now_utc` can show (PID reuse checks).
    pub fn start_bound(wall_now_utc: DateTime<Utc>) -> DateTime<Utc> {
        Self::global_step_clock().bound(wall_now_utc, win32::tick_count64(), Self::system_started())
    }

    /// The rule on a fresh [`StepClock`] (one detection), for tests.
    pub fn start_bound_fresh(
        wall_now_utc: DateTime<Utc>,
        ms_since_boot: i64,
        system_started_utc: Option<DateTime<Utc>>,
    ) -> DateTime<Utc> {
        StepClock::new(None).bound(wall_now_utc, ms_since_boot, system_started_utc)
    }

    /// Image path of `pid`, only when that process started at or before `not_after_utc`.
    /// One handle for both `GetProcessTimes` and `QueryFullProcessImageNameW`.
    pub fn image_path_if_started_before(pid: i32, not_after_utc: DateTime<Utc>) -> Option<String> {
        if pid < 0 {
            return None;
        }
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) }.ok()?;
        let _guard = OwnedHandle(h);
        let created = created_utc_handle(h)?;
        if created > not_after_utc {
            return None;
        }
        image_path_handle(h)
    }

    /// Reads session, user SID, integrity RID, AppContainer flag, image path, and creation time.
    pub fn read(pid: i32, session: u32) -> PeerInfo {
        if pid < 0 {
            return PeerInfo {
                pid,
                session,
                sid: None,
                integrity_rid: -1,
                app_container: true,
                image: None,
                created_utc: None,
            };
        }
        let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) })
        else {
            return PeerInfo {
                pid,
                session,
                sid: None,
                integrity_rid: -1,
                app_container: true,
                image: None,
                created_utc: None,
            };
        };
        let _proc_guard = OwnedHandle(h);
        let image = image_path_handle(h);
        let created_utc = created_utc_handle(h);

        let mut sid: Option<String> = None;
        let mut rid: i32 = -1;
        let mut app_container = true;

        let mut token = HANDLE::default();
        if unsafe { OpenProcessToken(h, TOKEN_QUERY, &mut token) }.is_ok() {
            let _tok_guard = OwnedHandle(token);
            let mut buf = vec![0u8; 1024];
            let mut ret = 0u32;

            if unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    Some(buf.as_mut_ptr().cast::<c_void>()),
                    buf.len() as u32,
                    &mut ret,
                )
            }
            .is_ok()
            {
                let user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
                let mut pwstr = PWSTR::null();
                if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut pwstr) }.is_ok()
                    && !pwstr.0.is_null()
                {
                    unsafe {
                        let mut len = 0usize;
                        while *pwstr.0.add(len) != 0 {
                            len += 1;
                        }
                        let slice = std::slice::from_raw_parts(pwstr.0, len);
                        sid = Some(OsString::from_wide(slice).to_string_lossy().into_owned());
                        let _ = LocalFree(Some(HLOCAL(pwstr.0.cast::<c_void>())));
                    }
                }
            }

            if unsafe {
                GetTokenInformation(
                    token,
                    TokenIntegrityLevel,
                    Some(buf.as_mut_ptr().cast::<c_void>()),
                    buf.len() as u32,
                    &mut ret,
                )
            }
            .is_ok()
            {
                let label = unsafe { &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
                let psid = label.Label.Sid;
                if !psid.0.is_null() {
                    unsafe {
                        let count_ptr = GetSidSubAuthorityCount(psid);
                        if !count_ptr.is_null() && *count_ptr > 0 {
                            let sub_ptr = GetSidSubAuthority(psid, (*count_ptr - 1) as u32);
                            if !sub_ptr.is_null() {
                                rid = *sub_ptr as i32;
                            }
                        }
                    }
                }
            }

            let mut is_ac: u32 = 1;
            if unsafe {
                GetTokenInformation(
                    token,
                    TokenIsAppContainer,
                    Some((&raw mut is_ac).cast::<c_void>()),
                    std::mem::size_of::<u32>() as u32,
                    &mut ret,
                )
            }
            .is_ok()
            {
                app_container = is_ac != 0;
            }
        }

        PeerInfo {
            pid,
            session,
            sid,
            integrity_rid: rid,
            app_container,
            image,
            created_utc,
        }
    }

    pub fn normalize(path: &str) -> String {
        InstallLayout::real_path(Path::new(path))
            .to_string_lossy()
            .trim_end_matches('\\')
            .to_string()
    }

    pub fn same_path(a: Option<&str>, b: Option<&str>) -> bool {
        let (Some(a), Some(b)) = (a, b) else {
            return false;
        };
        Self::normalize(a).eq_ignore_ascii_case(&Self::normalize(b))
    }
}

fn image_path_handle(h: HANDLE) -> Option<String> {
    let mut buf = vec![0u16; 32_768];
    let mut size = buf.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size)
    }
    .ok()?;
    Some(
        OsString::from_wide(&buf[..size as usize])
            .to_string_lossy()
            .into_owned(),
    )
}

fn created_utc_handle(h: HANDLE) -> Option<DateTime<Utc>> {
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(h, &mut created, &mut exit, &mut kernel, &mut user) }.ok()?;
    let ticks = ((created.dwHighDateTime as u64) << 32) | (created.dwLowDateTime as u64);
    if ticks == 0 {
        return None;
    }
    filetime_to_utc(ticks)
}

/// Converts Windows FILETIME (100 ns ticks since 1601-01-01 UTC) to `DateTime<Utc>`.
fn filetime_to_utc(ticks: u64) -> Option<DateTime<Utc>> {
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    let total_secs = (ticks / 10_000_000) as i64 - EPOCH_DIFF_SECS;
    let nanos = ((ticks % 10_000_000) * 100) as u32;
    DateTime::<Utc>::from_timestamp(total_secs, nanos)
}

struct StepClockState {
    gap: Duration,
    until: Option<DateTime<Utc>>,
}

/// Detects a backward clock step and bounds process start times for its catch-up period only (`StepClock`).
pub struct StepClock {
    gate: Mutex<StepClockState>,
    on_step: Option<Box<dyn Fn(Duration) + Send + Sync>>,
}

impl StepClock {
    pub fn new(on_step: Option<Box<dyn Fn(Duration) + Send + Sync>>) -> Self {
        Self {
            gate: Mutex::new(StepClockState {
                gap: Duration::zero(),
                until: None,
            }),
            on_step,
        }
    }

    pub fn bound(
        &self,
        wall_now_utc: DateTime<Utc>,
        ms_since_boot: i64,
        system_started_utc: Option<DateTime<Utc>>,
    ) -> DateTime<Utc> {
        let Some(boot) = system_started_utc else {
            return wall_now_utc;
        };
        let gap = (boot + Duration::milliseconds(ms_since_boot)).signed_duration_since(wall_now_utc);
        let tolerance = Duration::seconds(ProcessInfo::STEP_TOLERANCE_SECS);
        let mut step: Option<Duration> = None;
        let bound = {
            let mut st = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            if gap > st.gap + tolerance {
                let s = gap - st.gap;
                step = Some(s);
                let next = wall_now_utc + s;
                st.until = Some(match st.until {
                    Some(u) if u > next => u,
                    _ => next,
                });
                st.gap = gap;
            } else if gap < st.gap {
                st.gap = gap;
            }
            match st.until {
                Some(until) if wall_now_utc < until => until,
                _ => wall_now_utc,
            }
        };
        if let (Some(s), Some(cb)) = (step, &self.on_step) {
            cb(s);
        }
        bound
    }
}
