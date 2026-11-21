// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Read gate and pause controller (`AgentState.cs`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};

use crate::native::tick_count64;

type WallClock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
type MonoClock = Arc<dyn Fn() -> i64 + Send + Sync>;

struct GateState {
    until: Option<DateTime<Utc>>,
    until_mono: i64,
    user_locked: bool,
}

/// Windows Hello / password read gate: open for `security.unlockTimeoutMin` after an unlock,
/// closed on lock or session lock. Timeout runs on a monotonic clock (counts sleep, ignores wall-clock steps).
pub struct ReadGate {
    now: WallClock,
    mono_ms: MonoClock,
    state: Mutex<GateState>,
}

impl Default for ReadGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadGate {
    pub fn new() -> Self {
        Self::with_clocks(Arc::new(Utc::now), Arc::new(tick_count64))
    }

    pub fn with_clock(now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        let wall: WallClock = Arc::new(now);
        let wall2 = wall.clone();
        Self::with_clocks(wall, Arc::new(move || wall2().timestamp_millis()))
    }

    pub fn with_clocks(now: WallClock, mono_ms: MonoClock) -> Self {
        Self {
            now,
            mono_ms,
            state: Mutex::new(GateState {
                until: None,
                until_mono: 0,
                user_locked: false,
            }),
        }
    }

    pub fn is_open(&self, require_unlock: bool) -> bool {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.user_locked {
            return false;
        }
        if !require_unlock {
            return true;
        }
        st.until.is_some() && (self.mono_ms)() < st.until_mono
    }

    pub fn until(&self) -> Option<DateTime<Utc>> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.user_locked
            && let Some(u) = st.until
            && (self.mono_ms)() < st.until_mono
        {
            Some(u)
        } else {
            None
        }
    }

    /// User pressed Lock (button, hotkey, tray, lock verb): closed until the next unlock,
    /// whatever `security.requireUnlock` says (D4).
    pub fn lock_by_user(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.until = None;
        st.user_locked = true;
    }

    /// Unlock with `security.requireUnlock` off: lift the user lock.
    pub fn release_user_lock(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.user_locked = false;
    }

    pub fn open(&self, timeout_min: i32) -> DateTime<Utc> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let span = Duration::minutes(timeout_min.max(1) as i64);
        st.user_locked = false;
        st.until_mono = (self.mono_ms)() + span.num_milliseconds();
        let u = (self.now)() + span;
        st.until = Some(u);
        u
    }

    pub fn close(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.until = None;
    }
}

pub type ChangedCallback = Arc<dyn Fn() + Send + Sync>;
pub type ExpiredCallback = Arc<dyn Fn(&str) + Send + Sync>;

struct PauseState {
    reasons: HashMap<String, Option<DateTime<Utc>>>,
    deadlines: HashMap<String, i64>,
    expired: Vec<String>,
}

/// Capture pause reasons with priority ordering and monotonic deadlines (`PauseController`).
pub struct PauseController {
    now: WallClock,
    mono_ms: MonoClock,
    state: Mutex<PauseState>,
    changed_listeners: Mutex<Vec<ChangedCallback>>,
    expired_listeners: Mutex<Vec<ExpiredCallback>>,
}

impl Default for PauseController {
    fn default() -> Self {
        Self::new()
    }
}

impl PauseController {
    const PRIORITY: &'static [&'static str] = &[
        "consent",
        "keylocked",
        "keylost",
        "storeerror",
        "lock",
        "storage",
        "user",
        "ipc",
        "fullscreen",
        "battery",
    ];

    pub fn new() -> Self {
        Self::with_clocks(Arc::new(Utc::now), Arc::new(tick_count64))
    }

    pub fn with_clock(now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        let wall: WallClock = Arc::new(now);
        let wall2 = wall.clone();
        Self::with_clocks(wall, Arc::new(move || wall2().timestamp_millis()))
    }

    pub fn with_clocks(now: WallClock, mono_ms: MonoClock) -> Self {
        Self {
            now,
            mono_ms,
            state: Mutex::new(PauseState {
                reasons: HashMap::new(),
                deadlines: HashMap::new(),
                expired: Vec::new(),
            }),
            changed_listeners: Mutex::new(Vec::new()),
            expired_listeners: Mutex::new(Vec::new()),
        }
    }

    pub fn on_changed(&self, cb: impl Fn() + Send + Sync + 'static) {
        self.changed_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(cb));
    }

    pub fn on_expired(&self, cb: impl Fn(&str) + Send + Sync + 'static) {
        self.expired_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(cb));
    }

    pub fn set(&self, reason: &str, until: Option<DateTime<Utc>>) {
        let changed = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(&old) = st.reasons.get(reason)
                && old == until
            {
                return;
            }
            st.reasons.insert(reason.to_string(), until);
            if let Some(u) = until {
                let delta_ms = (u - (self.now)()).num_milliseconds().max(0);
                st.deadlines
                    .insert(reason.to_string(), (self.mono_ms)() + delta_ms);
            } else {
                st.deadlines.remove(reason);
            }
            true
        };
        if changed {
            self.raise_changed();
        }
    }

    pub fn clear(&self, reason: &str) {
        let removed = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.deadlines.remove(reason);
            st.reasons.remove(reason).is_some()
        };
        if removed {
            self.raise_changed();
        }
    }

    pub fn has(&self, reason: &str) -> bool {
        let has = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.expire_locked(&mut st);
            st.reasons.contains_key(reason)
        };
        self.raise_expired();
        has
    }

    pub fn keyframes_paused(&self) -> bool {
        self.current().0.is_some()
    }

    pub fn continuous_paused(&self, during_fullscreen: bool) -> bool {
        let paused = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.expire_locked(&mut st);
            st.reasons.keys().any(|r| {
                if r == "fullscreen" && during_fullscreen {
                    return false;
                }
                !matches!(r.as_str(), "battery" | "storage")
            })
        };
        self.raise_expired();
        paused
    }

    pub fn current(&self) -> (Option<String>, Option<DateTime<Utc>>) {
        let (expired, result) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let expired = self.expire_locked(&mut st);
            let mut res: (Option<String>, Option<DateTime<Utc>>) = (None, None);
            for &p in Self::PRIORITY {
                if let Some(&until) = st.reasons.get(p) {
                    res = (Some(p.to_string()), until);
                    break;
                }
            }
            if res.0.is_none()
                && let Some((k, &v)) = st.reasons.iter().next()
            {
                res = (Some(k.clone()), v);
            }
            (expired, res)
        };
        self.raise_expired();
        if expired {
            self.raise_changed();
        }
        result
    }

    fn expire_locked(&self, st: &mut PauseState) -> bool {
        if st.deadlines.is_empty() {
            return false;
        }
        let t = (self.mono_ms)();
        let gone: Vec<String> = st
            .deadlines
            .iter()
            .filter_map(|(k, &v)| if t >= v { Some(k.clone()) } else { None })
            .collect();
        for g in &gone {
            st.reasons.remove(g);
            st.deadlines.remove(g);
        }
        let any = !gone.is_empty();
        st.expired.extend(gone);
        any
    }

    fn raise_changed(&self) {
        let listeners = self
            .changed_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for cb in listeners {
            cb();
        }
    }

    fn raise_expired(&self) {
        let list: Vec<String> = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.expired.is_empty() {
                return;
            }
            std::mem::take(&mut st.expired)
        };
        let listeners = self
            .expired_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for r in &list {
            for cb in &listeners {
                cb(r);
            }
        }
    }
}
