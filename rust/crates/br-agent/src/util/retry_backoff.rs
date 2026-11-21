// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Exponential retry backoff (`RetryBackoff.cs`).

use std::sync::Mutex;

struct State {
    failures: i32,
    next_at: i64,
}

/// Exponential retry delay for a resource that failed (encoder, audio device):
/// first retry after `initial_ms`, doubling per failure in a row up to `max_ms`;
/// a success resets it. Times are milliseconds on any monotonic clock. Thread-safe.
pub struct RetryBackoff {
    initial_ms: i64,
    max_ms: i64,
    state: Mutex<State>,
}

impl RetryBackoff {
    pub fn new(initial_ms: i64, max_ms: i64) -> Self {
        Self {
            initial_ms,
            max_ms,
            state: Mutex::new(State {
                failures: 0,
                next_at: i64::MIN,
            }),
        }
    }

    pub fn failures(&self) -> i32 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
    }

    pub fn next_at(&self) -> i64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).next_at
    }

    pub fn due(&self, now_ms: i64) -> bool {
        now_ms >= self.state.lock().unwrap_or_else(|e| e.into_inner()).next_at
    }

    /// Records a failure at `now_ms` and returns the delay before the next attempt.
    pub fn failed(&self, now_ms: i64) -> i64 {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.failures += 1;
        let shift = (st.failures - 1).min(20) as u32;
        let delay = self.max_ms.min(self.initial_ms.wrapping_shl(shift));
        st.next_at = now_ms.saturating_add(delay);
        delay
    }

    pub fn succeeded(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.failures = 0;
        st.next_at = i64::MIN;
    }
}
