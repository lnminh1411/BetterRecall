// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! UI Automation search for password fields (`IsPassword`) in a window (`PasswordFieldProbe.cs`).

use std::ffi::c_void;
use std::time::Instant;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, TreeScope_Descendants,
    UIA_IsPasswordPropertyId,
};

use crate::native::{Rect, tick_count64};
use crate::util::AgentLog;

/// Must be created and used on a COM-initialised (MTA) thread.
pub struct PasswordFieldProbe {
    uia: IUIAutomation,
    cond: IUIAutomationCondition,
    last_hwnd: isize,
    last_at: i64,
    last: Option<Vec<Rect>>,
    last_ms: f64,
}

impl PasswordFieldProbe {
    pub const MAX_FIELDS: i32 = 32;
    pub const MIN_INTERVAL_MS: i64 = 1000;

    pub fn new() -> Result<Self, String> {
        let uia: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| format!("CoCreateInstance CUIAutomation: {e}"))?;
        let cond = unsafe {
            uia.CreatePropertyCondition(UIA_IsPasswordPropertyId, &VARIANT::from(true))
        }
        .map_err(|e| format!("CreatePropertyCondition IsPassword: {e}"))?;
        Ok(Self {
            uia,
            cond,
            last_hwnd: 0,
            last_at: i64::MIN,
            last: Some(Vec::new()),
            last_ms: 0.0,
        })
    }

    pub fn last_ms(&self) -> f64 {
        self.last_ms
    }

    /// Password field rectangles (desktop pixels, non-empty) of `hwnd`.
    /// Returns `None` when the search failed (fields unknown), `Some(vec![])` when none found.
    pub fn find(&mut self, hwnd: isize) -> Option<Vec<Rect>> {
        let now = tick_count64();
        if hwnd == self.last_hwnd
            && self.last_at != i64::MIN
            && now.saturating_sub(self.last_at) < Self::MIN_INTERVAL_MS
        {
            return self.last.clone();
        }
        let t0 = Instant::now();
        let r = self.probe(hwnd);
        self.last_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.last_hwnd = hwnd;
        self.last_at = now;
        self.last = r.clone();
        r
    }

    fn probe(&self, hwnd: isize) -> Option<Vec<Rect>> {
        let root = match unsafe { self.uia.ElementFromHandle(HWND(hwnd as *mut c_void)) } {
            Ok(r) => r,
            Err(e) => {
                AgentLog::debug(&format!("uia password search failed: {e}"));
                return None;
            }
        };
        let array = match unsafe { root.FindAll(TreeScope_Descendants, &self.cond) } {
            Ok(a) => a,
            Err(e) => {
                AgentLog::debug(&format!("uia password FindAll failed: {e}"));
                return None;
            }
        };
        let n = unsafe { array.Length() }.ok()?;
        if n <= 0 {
            return Some(Vec::new());
        }
        let mut list = Vec::new();
        for i in 0..n.min(Self::MAX_FIELDS) {
            let Ok(el) = (unsafe { array.GetElement(i) }) else {
                continue;
            };
            if let Ok(rc) = unsafe { el.CurrentBoundingRectangle() } {
                let r = Rect::from_win32(rc);
                if r.width() > 0 && r.height() > 0 {
                    list.push(r);
                }
            }
        }
        Some(list)
    }
}
