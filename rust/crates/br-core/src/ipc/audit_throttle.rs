// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Rate limit for audit rows and log lines that a client can trigger at will (`AuditThrottle.cs`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Entry {
    start: Instant,
    merged: i32,
}

struct State {
    keys: HashMap<String, Entry>,
    window_start: Option<Instant>,
    rows: i32,
}

/// At most one row per key and window, and at most `max_per_window` rows per window in total.
/// Repeats are merged into the key's next row.
pub struct AuditThrottle {
    window: Duration,
    max_per_window: i32,
    max_keys: usize,
    gate: Mutex<State>,
}

impl AuditThrottle {
    pub fn new(window: Duration, max_per_window: i32, max_keys: usize) -> Self {
        Self {
            window,
            max_per_window,
            max_keys,
            gate: Mutex::new(State {
                keys: HashMap::new(),
                window_start: None,
                rows: 0,
            }),
        }
    }

    /// `0`: skip this event. Otherwise write one row standing for that many events (this one plus
    /// repeats merged since the key's last row).
    /// B4: uses monotonic `Instant` so backward wall-clock steps never pin the window.
    pub fn admit(&self, key: &str, now: Instant) -> i32 {
        let lower = key.to_ascii_lowercase();
        let mut st = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if st
            .window_start
            .is_none_or(|ws| now.saturating_duration_since(ws) >= self.window)
        {
            st.window_start = Some(now);
            st.rows = 0;
        }
        if let Some(e) = st.keys.get_mut(&lower)
            && now.saturating_duration_since(e.start) < self.window
        {
            e.merged += 1;
            return 0;
        }
        let merged = st.keys.get(&lower).map_or(0, |e| e.merged);
        if st.rows >= self.max_per_window {
            // Over the total budget: remember the event for this key's next row.
            if st.keys.len() < self.max_keys || st.keys.contains_key(&lower) {
                st.keys.insert(
                    lower,
                    Entry {
                        start: now,
                        merged: merged + 1,
                    },
                );
            }
            return 0;
        }
        if st.keys.len() >= self.max_keys && !st.keys.contains_key(&lower) {
            st.keys
                .retain(|_, v| !(now.saturating_duration_since(v.start) >= self.window && v.merged == 0));
            if st.keys.len() >= self.max_keys {
                return 0;
            }
        }
        st.keys.insert(lower, Entry { start: now, merged: 0 });
        st.rows += 1;
        merged + 1
    }
}
