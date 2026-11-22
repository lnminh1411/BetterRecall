// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Background MTA guardrail scanner (`GuardrailService.cs`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use br_contracts::Settings;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

use crate::guardrails::browser_badge::BrowserBadgeDetector;
use crate::guardrails::password_probe::PasswordFieldProbe;
use crate::guardrails::policy::{BadgeResult, GuardReason, GuardrailPolicy, WindowInfo};
use crate::guardrails::window_inspector::{GuardSnapshot, WindowInspector};
use crate::native::{MASK_EVERYTHING, Rect, current_process_id, tick_count64};
use crate::util::AgentLog;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioGate {
    Allowed,
    Blocked,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioVerdict {
    pub gate: AudioGate,
    pub app: String,
    pub epoch: i64,
}

impl AudioVerdict {
    pub fn allowed() -> Self {
        Self {
            gate: AudioGate::Allowed,
            app: String::new(),
            epoch: 0,
        }
    }
}

pub type ForegroundCallback = Arc<dyn Fn(Option<&WindowInfo>) + Send + Sync>;

struct GuardInner {
    settings: Arc<dyn Fn() -> Settings + Send + Sync>,
    session_locked: Arc<dyn Fn() -> bool + Send + Sync>,
    monitors: Arc<dyn Fn() -> Vec<(i32, Rect)> + Send + Sync>,
    install_dir: String,
    own_pid: u32,
    stop: AtomicBool,
    wake: Mutex<bool>,
    wake_cv: Condvar,
    snapshot: Mutex<Arc<GuardSnapshot>>,
    fg_epoch: AtomicI64,
    last_scan_ms: AtomicU64,
    last_badge_ms: AtomicU64,
    last_password_ms: AtomicU64,
    password_probe_failures: AtomicI64,
    password_warn_at: AtomicI64,
    fg_listeners: Mutex<Vec<ForegroundCallback>>,
}

/// Re-evaluates guardrails on every foreground change and twice a second, on its own MTA thread.
pub struct GuardrailService {
    inner: Arc<GuardInner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl GuardrailService {
    pub fn new(
        settings: impl Fn() -> Settings + Send + Sync + 'static,
        session_locked: impl Fn() -> bool + Send + Sync + 'static,
        monitors: impl Fn() -> Vec<(i32, Rect)> + Send + Sync + 'static,
        install_dir: &Path,
    ) -> Arc<Self> {
        let mut dir = install_dir
            .to_string_lossy()
            .trim_end_matches('\\')
            .to_string();
        dir.push('\\');
        Arc::new(Self {
            inner: Arc::new(GuardInner {
                settings: Arc::new(settings),
                session_locked: Arc::new(session_locked),
                monitors: Arc::new(monitors),
                install_dir: dir,
                own_pid: current_process_id(),
                stop: AtomicBool::new(false),
                wake: Mutex::new(false),
                wake_cv: Condvar::new(),
                snapshot: Mutex::new(Arc::new(GuardSnapshot::closed())),
                fg_epoch: AtomicI64::new(0),
                last_scan_ms: AtomicU64::new(0),
                last_badge_ms: AtomicU64::new(0),
                last_password_ms: AtomicU64::new(0),
                password_probe_failures: AtomicI64::new(0),
                password_warn_at: AtomicI64::new(i64::MIN),
                fg_listeners: Mutex::new(Vec::new()),
            }),
            thread: Mutex::new(None),
        })
    }

    pub fn on_foreground_changed(&self, cb: ForegroundCallback) {
        self.inner
            .fg_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(cb);
    }

    pub fn start(self: &Arc<Self>) {
        let mut slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let inner = self.inner.clone();
        *slot = thread::Builder::new()
            .name("guardrails".to_string())
            .spawn(move || run_guardrails(inner))
            .ok();
    }

    pub fn last_scan_ms(&self) -> f64 {
        f64::from_bits(self.inner.last_scan_ms.load(Ordering::Acquire))
    }

    pub fn last_badge_ms(&self) -> f64 {
        f64::from_bits(self.inner.last_badge_ms.load(Ordering::Acquire))
    }

    pub fn last_password_ms(&self) -> f64 {
        f64::from_bits(self.inner.last_password_ms.load(Ordering::Acquire))
    }

    pub fn password_probe_failures(&self) -> i64 {
        self.inner.password_probe_failures.load(Ordering::Acquire)
    }

    pub fn current(&self, max_age_ms: i64) -> Arc<GuardSnapshot> {
        let s = self
            .inner
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !s.is_closed_sentinel && tick_count64().saturating_sub(s.taken_at_ms) <= max_age_ms {
            s
        } else {
            Arc::new(GuardSnapshot::closed())
        }
    }

    pub fn set_snapshot_for_test(&self, snap: GuardSnapshot) {
        *self.inner.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(snap);
    }

    pub fn current_for_keyframe(&self, max_age_ms: i64) -> GuardSnapshot {
        let s = self.current(max_age_ms);
        Self::fresh_for_grab(
            &s,
            self.inner.fg_epoch.load(Ordering::Acquire),
            WindowInspector::foreground_root(),
        )
    }

    pub fn unscanned_foreground(&self, s: &GuardSnapshot) -> Option<Rect> {
        if s.is_closed_sentinel {
            return None;
        }
        let fg = WindowInspector::foreground_root();
        let bounds = if fg != 0 {
            WindowInspector::try_get_bounds(fg)
        } else {
            None
        };
        Self::unscanned_foreground_pure(s, self.inner.fg_epoch.load(Ordering::Acquire), fg, bounds)
    }

    pub fn unscanned_foreground_pure(
        s: &GuardSnapshot,
        fg_epoch_now: i64,
        foreground_now: isize,
        foreground_bounds: Option<Rect>,
    ) -> Option<Rect> {
        if foreground_now == 0 || s.is_closed_sentinel {
            return None;
        }
        let fresh = Self::fresh_for_grab(s, fg_epoch_now, foreground_now);
        if fresh.decision.reason != GuardReason::ForegroundChanged {
            return None;
        }
        Some(foreground_bounds.unwrap_or(MASK_EVERYTHING))
    }

    pub fn audio_now(&self) -> AudioVerdict {
        let s = self.current(2000);
        Self::audio_now_pure(&s, self.inner.fg_epoch.load(Ordering::Acquire))
    }

    pub fn audio_now_pure(s: &GuardSnapshot, fg_epoch_now: i64) -> AudioVerdict {
        if s.is_closed_sentinel {
            return AudioVerdict {
                gate: AudioGate::Blocked,
                app: String::new(),
                epoch: fg_epoch_now,
            };
        }
        let app = app_name(
            s.foreground
                .as_ref()
                .map(|w| w.exe_path.as_str())
                .unwrap_or(""),
        );
        if s.fg_epoch != fg_epoch_now {
            return AudioVerdict {
                gate: AudioGate::Unknown,
                app,
                epoch: fg_epoch_now,
            };
        }
        AudioVerdict {
            gate: if Self::blocks_audio(s.decision.reason) {
                AudioGate::Blocked
            } else {
                AudioGate::Allowed
            },
            app,
            epoch: s.fg_epoch,
        }
    }

    pub fn blocks_audio(r: GuardReason) -> bool {
        !matches!(
            r,
            GuardReason::None | GuardReason::NoForeground | GuardReason::KeyLocked
        )
    }

    pub fn fresh_for_grab(
        s: &GuardSnapshot,
        fg_epoch_now: i64,
        foreground_now: isize,
    ) -> GuardSnapshot {
        if s.is_closed_sentinel {
            return s.clone();
        }
        let fg_hwnd = s.foreground.as_ref().map(|w| w.hwnd).unwrap_or(0);
        if s.fg_epoch != fg_epoch_now || fg_hwnd != foreground_now {
            GuardSnapshot::foreground_moved()
        } else {
            s.clone()
        }
    }

    pub fn on_foreground_event(&self) {
        self.inner.fg_epoch.fetch_add(1, Ordering::AcqRel);
        self.wake();
    }

    pub fn refresh(&self) {
        self.wake();
    }

    fn wake(&self) {
        if self.inner.stop.load(Ordering::Acquire) {
            return;
        }
        let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
        *w = true;
        self.inner.wake_cv.notify_one();
    }

    /// First warning, then one per 10 minutes. `i64::MIN` (never warned) is always due.
    pub fn warn_due(last_warn_at: i64, now: i64) -> bool {
        last_warn_at == i64::MIN || now.saturating_sub(last_warn_at) >= 600_000
    }

    pub fn stop(&self) {
        if self.inner.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
            *w = true;
            self.inner.wake_cv.notify_all();
        }
        if let Some(h) = self
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            // N5: a UI Automation call into a hung window (e.g. a suspended UI) can block for a long
            // time; quit must not wait on it. Wait up to STOP_WAIT, then leave the thread to finish
            // on its own (it sees `stop` and exits once the call returns).
            let deadline = std::time::Instant::now() + Self::STOP_WAIT;
            while !h.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(std::time::Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                AgentLog::warn(&format!(
                    "guard thread still busy after {} ms (UI Automation call), not waiting",
                    Self::STOP_WAIT.as_millis()
                ));
            }
        }
    }

    /// Longest `stop` waits for the guard thread.
    pub const STOP_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);
}

impl Drop for GuardrailService {
    fn drop(&mut self) {
        self.stop();
    }
}

fn app_name(exe: &str) -> String {
    if exe.is_empty() {
        return String::new();
    }
    Path::new(exe)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

fn run_guardrails(inner: Arc<GuardInner>) {
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let mut badge: Option<BrowserBadgeDetector> = None;
    let mut passwords: Option<PasswordFieldProbe> = None;
    let mut last_fg: isize = -1;

    while !inner.stop.load(Ordering::Acquire) {
        let s = (inner.settings)();
        if (s.privacy.browser_badge_detection || s.privacy.record_site) && badge.is_none() {
            match BrowserBadgeDetector::new() {
                Ok(b) => badge = Some(b),
                Err(e) => AgentLog::warn_err("UI Automation unavailable", &e),
            }
        }
        let t0 = Instant::now();
        let epoch = inner.fg_epoch.load(Ordering::Acquire);
        let fg = WindowInspector::foreground();
        let monitors = (inner.monitors)();

        let scan = if s.privacy.browser_badge_detection {
            WindowInspector::scan(
                &s.privacy,
                &inner.install_dir,
                inner.own_pid,
                &monitors,
                Some(|w: &WindowInfo| {
                    let probe = badge.as_mut().map(|b| move |h: isize| b.check(h));
                    GuardrailPolicy::hides_browser_window(w, probe)
                }),
            )
        } else {
            WindowInspector::scan::<fn(&WindowInfo) -> bool>(
                &s.privacy,
                &inner.install_dir,
                inner.own_pid,
                &monitors,
                None,
            )
        };
        inner.last_scan_ms.store(
            (t0.elapsed().as_secs_f64() * 1000.0).to_bits(),
            Ordering::Release,
        );

        let mut badge_result = BadgeResult::NotBrowser;
        if let Some(ref fg_w) = fg
            && s.privacy.browser_badge_detection
            && GuardrailPolicy::is_chromium_browser(fg_w.process_name())
        {
            if let Some(ref mut b) = badge {
                badge_result = b.check(fg_w.hwnd);
                inner
                    .last_badge_ms
                    .store(b.last_check_ms().to_bits(), Ordering::Release);
            } else {
                badge_result = BadgeResult::CheckFailed;
            }
        }

        let mut site = String::new();
        if let Some(ref fg_w) = fg
            && s.privacy.record_site
            && badge_result != BadgeResult::Private
            && GuardrailPolicy::is_chromium_browser(fg_w.process_name())
            && let Some(ref mut b) = badge
        {
            site = b.read_site(fg_w.hwnd);
        }

        let decision = GuardrailPolicy::evaluate(
            fg.as_ref(),
            &s.privacy,
            (inner.session_locked)(),
            scan.any_affinity,
            badge_result,
        );
        let mut masks = scan.masks;
        if GuardrailPolicy::badge_hides(badge_result)
            && let Some(ref fg_w) = fg
            && let Some(fr) = WindowInspector::try_get_bounds(fg_w.hwnd)
        {
            masks.push(fr);
        }

        let mut secrets = None;
        if s.privacy.redact_passwords && decision.allow
            && let Some(ref fg_w) = fg
        {
            if passwords.is_none() {
                match PasswordFieldProbe::new() {
                    Ok(p) => passwords = Some(p),
                    Err(e) => {
                        AgentLog::warn_err("UI Automation unavailable for password fields", &e);
                    }
                }
            }
            if let Some(ref mut p) = passwords {
                secrets = p.find(fg_w.hwnd);
                inner
                    .last_password_ms
                    .store(p.last_ms().to_bits(), Ordering::Release);
                if secrets.is_none() {
                    let count = inner
                        .password_probe_failures
                        .fetch_add(1, Ordering::AcqRel)
                        + 1;
                    let now = tick_count64();
                    let prev = inner.password_warn_at.load(Ordering::Acquire);
                    if GuardrailService::warn_due(prev, now) {
                        inner.password_warn_at.store(now, Ordering::Release);
                        AgentLog::warn(&format!(
                            "password field search failed for {} (UI Automation); its password fields are not masked ({count} failures so far)",
                            fg_w.process_name()
                        ));
                    }
                }
            }
        }

        let fg_hwnd = fg.as_ref().map(|w| w.hwnd).unwrap_or(0);
        let snap = Arc::new(GuardSnapshot {
            taken_at_ms: tick_count64(),
            foreground: fg.clone(),
            decision,
            badge: badge_result,
            any_affinity: scan.any_affinity,
            mask_rects: masks,
            per_monitor: scan.per_monitor,
            site,
            secrets,
            fg_epoch: epoch,
            is_closed_sentinel: false,
        });
        *inner.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = snap;

        if fg_hwnd != last_fg {
            last_fg = fg_hwnd;
            let listeners = inner
                .fg_listeners
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for cb in listeners {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cb(fg.as_ref());
                }));
            }
        }

        let mut w = inner.wake.lock().unwrap_or_else(|e| e.into_inner());
        if !*w && !inner.stop.load(Ordering::Acquire) {
            let (mut g, _) = inner
                .wake_cv
                .wait_timeout(w, Duration::from_millis(500))
                .unwrap_or_else(|e| e.into_inner());
            *g = false;
        } else {
            *w = false;
        }
    }

    drop(badge);
    drop(passwords);
    if com_ok {
        unsafe { CoUninitialize() };
    }
}
