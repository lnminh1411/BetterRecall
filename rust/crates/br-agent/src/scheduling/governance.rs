// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Pure scheduling governance decisions and system load probe (`Governance.cs`).

use std::collections::HashMap;
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::time::Duration;

use br_contracts::{CaptureSettings, GovernanceSettings};
use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::Threading::{
    GetCurrentThread, GetSystemTimes, IDLE_PRIORITY_CLASS,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetPriorityClass,
    SetProcessInformation, SetThreadInformation, ThreadPowerThrottling,
};
use windows::Win32::UI::Shell::SHQueryUserNotificationState;
use windows::core::PCWSTR;

use crate::native::{filetime_to_u64, to_wide_z};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum UserNotificationState {
    Unknown = 0,
    NotPresent = 1,
    Busy = 2,
    FullscreenD3D = 3,
    Presentation = 4,
    AcceptsNotifications = 5,
    QuietTime = 6,
    App = 7,
}

impl UserNotificationState {
    pub fn from_raw(v: i32) -> Self {
        match v {
            1 => Self::NotPresent,
            2 => Self::Busy,
            3 => Self::FullscreenD3D,
            4 => Self::Presentation,
            5 => Self::AcceptsNotifications,
            6 => Self::QuietTime,
            7 => Self::App,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GovernanceInputs {
    pub cpu_percent: f64,
    pub igpu_percent: Option<f64>,
    pub on_battery: bool,
    pub notification: UserNotificationState,
    pub user_idle_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceDecision {
    pub run: bool,
    pub reason: &'static str,
    pub batch_size: usize,
    pub next_check: Duration,
}

impl GovernanceDecision {
    pub const fn defer(reason: &'static str, next: Duration) -> Self {
        Self {
            run: false,
            reason,
            batch_size: 0,
            next_check: next,
        }
    }
}

pub struct Governance;

impl Governance {
    pub const IDLE_DRAIN_MS: i64 = 60_000;

    pub fn decide(
        i: GovernanceInputs,
        g: &GovernanceSettings,
        c: &CaptureSettings,
    ) -> GovernanceDecision {
        if g.pause_on_battery && i.on_battery {
            return GovernanceDecision::defer("battery", Duration::from_secs(30));
        }
        if c.pause_on_fullscreen
            && matches!(
                i.notification,
                UserNotificationState::FullscreenD3D | UserNotificationState::Presentation
            )
        {
            return GovernanceDecision::defer("fullscreen", Duration::from_secs(15));
        }
        if i.cpu_percent > g.defer_cpu_percent as f64 {
            return GovernanceDecision::defer("cpu", Duration::from_secs(10));
        }
        if let Some(gpu) = i.igpu_percent
            && gpu > g.defer_gpu_percent as f64
        {
            return GovernanceDecision::defer("gpu", Duration::from_secs(10));
        }
        let idle = i.user_idle_ms >= Self::IDLE_DRAIN_MS;
        GovernanceDecision {
            run: true,
            reason: if idle { "idle-drain" } else { "ok" },
            batch_size: if idle { 32 } else { 4 },
            next_check: if idle {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(3)
            },
        }
    }

    /// Keyframes pause in fullscreen D3D and presentation mode when `capture.pauseOnFullscreen`.
    pub fn pause_keyframes(n: UserNotificationState, c: &CaptureSettings) -> bool {
        c.pause_on_fullscreen
            && matches!(
                n,
                UserNotificationState::FullscreenD3D | UserNotificationState::Presentation
            )
    }

    /// Applies EcoQoS (and optionally `IDLE_PRIORITY_CLASS`) to `process`.
    ///
    /// # Safety
    /// `process` must be a valid Win32 process handle with `PROCESS_SET_INFORMATION`.
    pub unsafe fn apply_eco_qos(process: HANDLE, idle_priority: bool) -> bool {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let ok = unsafe {
            SetProcessInformation(
                process,
                ProcessPowerThrottling,
                (&raw const state).cast::<c_void>(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        }
        .is_ok();
        if idle_priority {
            unsafe {
                let _ = SetPriorityClass(process, IDLE_PRIORITY_CLASS);
            }
        }
        ok
    }

    /// EcoQoS for the calling thread only (keyframe encoding).
    pub fn apply_eco_qos_to_current_thread() -> bool {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        unsafe {
            SetThreadInformation(
                GetCurrentThread(),
                ThreadPowerThrottling,
                (&raw const state).cast::<c_void>(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        }
        .is_ok()
    }
}

/// Samples system load for governance. Not thread-safe; one owner (or wrap in `Mutex`).
pub struct SystemProbe {
    last_idle: u64,
    last_total: u64,
    pdh_query: PDH_HQUERY,
    pdh_counter: PDH_HCOUNTER,
    pdh_failed: bool,
    igpu_luid: Option<String>,
}

unsafe impl Send for SystemProbe {}

impl SystemProbe {
    pub fn new(igpu_luid_instance_token: Option<String>) -> Self {
        let mut p = Self {
            last_idle: 0,
            last_total: 0,
            pdh_query: PDH_HQUERY::default(),
            pdh_counter: PDH_HCOUNTER::default(),
            pdh_failed: false,
            igpu_luid: igpu_luid_instance_token,
        };
        let _ = p.cpu_percent();
        p
    }

    pub fn cpu_percent(&mut self) -> f64 {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_err() {
            return 0.0;
        }
        let idle_v = filetime_to_u64(idle);
        let total = filetime_to_u64(kernel).wrapping_add(filetime_to_u64(user));
        let d_idle = idle_v.wrapping_sub(self.last_idle);
        let d_total = total.wrapping_sub(self.last_total);
        self.last_idle = idle_v;
        self.last_total = total;
        if d_total == 0 {
            return 0.0;
        }
        (100.0 * d_total.saturating_sub(d_idle) as f64 / d_total as f64).clamp(0.0, 100.0)
    }

    pub fn on_battery() -> bool {
        let mut s = SYSTEM_POWER_STATUS::default();
        if unsafe { GetSystemPowerStatus(&mut s) }.is_err() {
            return false;
        }
        s.ACLineStatus == 0
    }

    pub fn notification() -> UserNotificationState {
        match unsafe { SHQueryUserNotificationState() } {
            Ok(st) => UserNotificationState::from_raw(st.0),
            Err(_) => UserNotificationState::Unknown,
        }
    }

    /// Highest 3D/compute engine utilisation on the iGPU from PDH GPU Engine counters.
    pub fn igpu_percent(&mut self) -> Option<f64> {
        let luid = self.igpu_luid.as_deref()?;
        if self.pdh_failed {
            return None;
        }
        if self.pdh_query.is_invalid() {
            let mut q = PDH_HQUERY::default();
            let mut c = PDH_HCOUNTER::default();
            if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut q) } != 0 {
                self.pdh_failed = true;
                return None;
            }
            let counter_path = to_wide_z(r"\GPU Engine(*)\Utilization Percentage");
            if unsafe { PdhAddEnglishCounterW(q, PCWSTR(counter_path.as_ptr()), 0, &mut c) } != 0 {
                unsafe {
                    let _ = PdhCloseQuery(q);
                }
                self.pdh_failed = true;
                return None;
            }
            self.pdh_query = q;
            self.pdh_counter = c;
            unsafe {
                let _ = PdhCollectQueryData(q);
            }
            return None; // rate counter needs two samples
        }
        if unsafe { PdhCollectQueryData(self.pdh_query) } != 0 {
            return None;
        }
        let mut size = 0u32;
        let mut count = 0u32;
        let rc = unsafe {
            PdhGetFormattedCounterArrayW(
                self.pdh_counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                None,
            )
        };
        if rc != PDH_MORE_DATA || size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let rc = unsafe {
            PdhGetFormattedCounterArrayW(
                self.pdh_counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>()),
            )
        };
        if rc != 0 {
            return None;
        }
        let luid_lower = luid.to_ascii_lowercase();
        let items = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                count as usize,
            )
        };
        let mut per_engine: HashMap<String, f64> = HashMap::new();
        for item in items {
            if item.szName.is_null() {
                continue;
            }
            let name = unsafe {
                let mut len = 0usize;
                while *item.szName.0.add(len) != 0 {
                    len += 1;
                }
                OsString::from_wide(std::slice::from_raw_parts(item.szName.0, len))
                    .to_string_lossy()
                    .into_owned()
            };
            let lower = name.to_ascii_lowercase();
            if !lower.contains(&luid_lower) {
                continue;
            }
            if !lower.ends_with("engtype_3d") && !lower.ends_with("engtype_compute") {
                continue;
            }
            let key = match name.find("_eng_") {
                Some(e) => name[e..].to_string(),
                None => name,
            };
            let val = unsafe { item.FmtValue.Anonymous.doubleValue };
            *per_engine.entry(key).or_insert(0.0) += val;
        }
        let max = per_engine.values().copied().fold(0.0_f64, f64::max);
        Some(max.min(100.0))
    }
}

impl Drop for SystemProbe {
    fn drop(&mut self) {
        if !self.pdh_query.is_invalid() {
            unsafe {
                let _ = PdhCloseQuery(self.pdh_query);
            }
            self.pdh_query = PDH_HQUERY::default();
        }
    }
}
