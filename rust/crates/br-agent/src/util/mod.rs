// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod high_res_timer;
pub mod log;
pub mod retry_backoff;

use std::sync::{Mutex, MutexGuard, TryLockError};

pub use high_res_timer::HighResTimer;
pub use log::AgentLog;
pub use retry_backoff::RetryBackoff;

/// D1: non-blocking mutex acquire that recovers from poisoning (logs once and clears poison)
/// while still treating `WouldBlock` as busy.
pub fn try_lock_recover<'a, T>(mutex: &'a Mutex<T>, name: &str) -> Option<MutexGuard<'a, T>> {
    match mutex.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(e)) => {
            mutex.clear_poison();
            AgentLog::warn(&format!("{name}: recovering poisoned lock"));
            Some(e.into_inner())
        }
        Err(TryLockError::WouldBlock) => None,
    }
}
