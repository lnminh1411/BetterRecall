// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! UI Automation lookup of Chromium private-mode badges and address-bar site (`BrowserBadgeDetector.cs`).

use std::collections::HashMap;
use std::ffi::c_void;
use std::time::Instant;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
    TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_EditControlTypeId, UIA_NamePropertyId,
    UIA_ToolBarControlTypeId, UIA_ValueValuePropertyId,
};
use windows::core::BSTR;

use crate::guardrails::policy::{BadgeResult, GuardrailPolicy};
use crate::native::tick_count64;
use crate::util::AgentLog;

/// Must be created and used on a COM-initialised (MTA) thread.
pub struct BrowserBadgeDetector {
    uia: IUIAutomation,
    badge_condition: IUIAutomationCondition,
    toolbar_condition: IUIAutomationCondition,
    edit_condition: IUIAutomationCondition,
    cache: HashMap<isize, (BadgeResult, i64)>,
    sites: HashMap<isize, (String, i64)>,
    last_check_ms: f64,
    short_cache: bool,
}

impl BrowserBadgeDetector {
    pub const BADGE_NAMES: &'static [&'static str] = &[
        "Incognito",
        "InPrivate",
        "InPrivate browsing",
        "Private",
        "Private browsing",
        "Private window",
        "Inkognito",
        "Incógnito",
        "Navigation privée",
        "Privat",
        "Privado",
        "隐身",
        "シークレット",
        "시크릿",
    ];

    pub fn new() -> Result<Self, String> {
        let uia: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| format!("CoCreateInstance CUIAutomation: {e}"))?;
        let toolbar_condition = unsafe {
            uia.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_ToolBarControlTypeId.0),
            )
        }
        .map_err(|e| format!("CreatePropertyCondition toolbar: {e}"))?;
        let edit_condition = unsafe {
            uia.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_EditControlTypeId.0),
            )
        }
        .map_err(|e| format!("CreatePropertyCondition edit: {e}"))?;

        let mut cond: Option<IUIAutomationCondition> = None;
        for &name in Self::BADGE_NAMES {
            let c = unsafe {
                uia.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(BSTR::from(name)))
            }
            .map_err(|e| format!("CreatePropertyCondition name: {e}"))?;
            cond = Some(match cond {
                None => c,
                Some(prev) => unsafe { uia.CreateOrCondition(&prev, &c) }
                    .map_err(|e| format!("CreateOrCondition: {e}"))?,
            });
        }
        let badge_condition = cond.ok_or_else(|| "empty badge list".to_string())?;
        Ok(Self {
            uia,
            badge_condition,
            toolbar_condition,
            edit_condition,
            cache: HashMap::new(),
            sites: HashMap::new(),
            last_check_ms: 0.0,
            short_cache: false,
        })
    }

    pub fn last_check_ms(&self) -> f64 {
        self.last_check_ms
    }

    pub fn check(&mut self, hwnd: isize) -> BadgeResult {
        let now = tick_count64();
        if let Some(&(res, at)) = self.cache.get(&hwnd) {
            let ttl = match res {
                BadgeResult::Private => 600_000,
                BadgeResult::CheckFailed => 5_000,
                _ => 30_000,
            };
            if now.saturating_sub(at) < ttl {
                return res;
            }
        }
        let t0 = Instant::now();
        self.short_cache = false;
        let r = self.probe(hwnd);
        self.last_check_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if self.cache.len() > 256 {
            self.cache.clear();
        }
        let stamp = if self.short_cache && r == BadgeResult::NoBadge {
            now - 25_000
        } else {
            now
        };
        self.cache.insert(hwnd, (r, stamp));
        r
    }

    fn probe(&mut self, hwnd: isize) -> BadgeResult {
        let root = match unsafe { self.uia.ElementFromHandle(HWND(hwnd as *mut c_void)) } {
            Ok(el) => el,
            Err(e) => {
                AgentLog::debug(&format!("uia badge check failed: {e}"));
                return BadgeResult::CheckFailed;
            }
        };
        let root_name = element_name(&root);
        if Self::root_name_is_private(Some(&root_name)) {
            return BadgeResult::Private;
        }
        let toolbar = match unsafe { root.FindFirst(TreeScope_Descendants, &self.toolbar_condition) }
        {
            Ok(tb) => tb,
            Err(_) => {
                // FindFirst returns Ok(null/empty) or Err when not found? In windows-rs, a null out-pointer returns Err(E_POINTER or S_OK null).
                // Distinguish COM failure vs not found by checking hr == S_OK (0).
                self.short_cache = true;
                return Self::no_toolbar_result(Some(&root_name));
            }
        };
        match unsafe { toolbar.FindFirst(TreeScope_Descendants, &self.badge_condition) } {
            Ok(_) => BadgeResult::Private,
            Err(e) if e.code().is_ok() => BadgeResult::NoBadge,
            Err(_) => BadgeResult::NoBadge,
        }
    }

    /// A window with no toolbar (fullscreen, kiosk, web app, DevTools, PiP, or initialising):
    /// readable name without private tag is `NoBadge`; empty/unreadable name fails closed.
    pub fn no_toolbar_result(root_name: Option<&str>) -> BadgeResult {
        if Self::root_name_is_private(root_name) {
            BadgeResult::Private
        } else if root_name.is_none_or(|s| s.trim().is_empty()) {
            BadgeResult::CheckFailed
        } else {
            BadgeResult::NoBadge
        }
    }

    /// True when a browser window's root name ends in a private-mode tag in `(...)` or `[...]`.
    pub fn root_name_is_private(name: Option<&str>) -> bool {
        let Some(name) = name else { return false };
        let n = name.trim_end();
        let mut chars = n.chars();
        let Some(last) = chars.next_back() else {
            return false;
        };
        if n.len() < 3 || (last != ')' && last != ']') {
            return false;
        }
        let open_ch = if last == ')' { '(' } else { '[' };
        let Some(open) = n.rfind(open_ch) else {
            return false;
        };
        let tag = n[open + 1..n.len() - 1].trim();
        Self::BADGE_NAMES
            .iter()
            .any(|b| b.eq_ignore_ascii_case(tag))
    }

    /// Host in the address bar of a Chromium window, cached for 1.5 s per window.
    pub fn read_site(&mut self, hwnd: isize) -> String {
        let now = tick_count64();
        if let Some((site, at)) = self.sites.get(&hwnd)
            && now.saturating_sub(*at) < 1500
        {
            return site.clone();
        }
        let site = self.probe_site(hwnd);
        if self.sites.len() > 256 {
            self.sites.clear();
        }
        self.sites.insert(hwnd, (site.clone(), now));
        site
    }

    fn probe_site(&self, hwnd: isize) -> String {
        let Ok(root) = (unsafe { self.uia.ElementFromHandle(HWND(hwnd as *mut c_void)) }) else {
            return String::new();
        };
        let Ok(toolbar) = (unsafe { root.FindFirst(TreeScope_Descendants, &self.toolbar_condition) })
        else {
            return String::new();
        };
        let Ok(edit) = (unsafe { toolbar.FindFirst(TreeScope_Descendants, &self.edit_condition) })
        else {
            return String::new();
        };
        let Ok(val) = (unsafe { edit.GetCurrentPropertyValue(UIA_ValueValuePropertyId) }) else {
            return String::new();
        };
        let s = variant_to_string(&val);
        GuardrailPolicy::site_from_address(Some(&s))
    }
}

fn element_name(element: &IUIAutomationElement) -> String {
    match unsafe { element.GetCurrentPropertyValue(UIA_NamePropertyId) } {
        Ok(v) => variant_to_string(&v),
        Err(_) => String::new(),
    }
}

fn variant_to_string(v: &VARIANT) -> String {
    unsafe {
        if v.Anonymous.Anonymous.vt != VT_BSTR {
            return String::new();
        }
        v.Anonymous.Anonymous.Anonymous.bstrVal.to_string()
    }
}
