// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Cooperative thread-safe cancellation tokens with instant callbacks and Win32 event wakeup.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A registration guard that removes its callback when dropped.
pub struct CancelReg {
    inner: Option<Arc<CancelInner>>,
    id: u64,
}

impl Drop for CancelReg {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.callbacks.retain(|(cid, _)| *cid != self.id);
        }
    }
}

/// Generic listener guard that unsubscribes on drop.
pub struct ListenerGuard {
    on_drop: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl ListenerGuard {
    pub fn new(on_drop: impl FnOnce() + Send + 'static) -> Self {
        Self {
            on_drop: Mutex::new(Some(Box::new(on_drop))),
        }
    }

    pub fn noop() -> Self {
        Self {
            on_drop: Mutex::new(None),
        }
    }
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.lock().unwrap_or_else(|e| e.into_inner()).take() {
            f();
        }
    }
}

struct CancelState {
    callbacks: Vec<(u64, Box<dyn FnOnce() + Send>)>,
}

struct CancelInner {
    cancelled: AtomicBool,
    state: Mutex<CancelState>,
    cv: Condvar,
}

static NEXT_CB_ID: AtomicU64 = AtomicU64::new(1);

/// Producer side of a cooperative cancellation token.
pub struct CancelSource {
    inner: Arc<CancelInner>,
    _links: Vec<CancelReg>,
}

impl Default for CancelSource {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelSource {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancelInner {
                cancelled: AtomicBool::new(false),
                state: Mutex::new(CancelState {
                    callbacks: Vec::new(),
                }),
                cv: Condvar::new(),
            }),
            _links: Vec::new(),
        }
    }

    /// Creates a source that is already cancelled.
    pub fn cancelled() -> Self {
        let s = Self::new();
        s.cancel();
        s
    }

    /// Creates a child source that cancels whenever `parent` cancels.
    pub fn linked(parent: &CancelToken) -> Self {
        let child = Self::new();
        let child_inner = child.inner.clone();
        let reg = parent.on_cancel(move || cancel_inner(&child_inner));
        Self {
            inner: child.inner,
            _links: vec![reg],
        }
    }

    /// Creates a child source that cancels whenever either `a` or `b` cancels.
    pub fn linked2(a: &CancelToken, b: &CancelToken) -> Self {
        let child = Self::new();
        let inner_a = child.inner.clone();
        let inner_b = child.inner.clone();
        let reg_a = a.on_cancel(move || cancel_inner(&inner_a));
        let reg_b = b.on_cancel(move || cancel_inner(&inner_b));
        Self {
            inner: child.inner,
            _links: vec![reg_a, reg_b],
        }
    }

    /// Creates a child source that cancels when `parent` cancels or after `timeout` elapses.
    pub fn with_timeout(parent: &CancelToken, timeout: Duration) -> Self {
        let child = Self::linked(parent);
        let tok = child.token();
        let inner = child.inner.clone();
        let _ = std::thread::Builder::new()
            .name("BetterRecall.CancelTimer".to_string())
            .spawn(move || {
                if !tok.wait_timeout(timeout) {
                    cancel_inner(&inner);
                }
            });
        child
    }

    pub fn token(&self) -> CancelToken {
        CancelToken {
            inner: self.inner.clone(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub fn cancel(&self) {
        cancel_inner(&self.inner);
    }
}

fn cancel_inner(inner: &CancelInner) {
    if inner.cancelled.swap(true, Ordering::AcqRel) {
        return;
    }
    let cbs = {
        let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut st.callbacks)
    };
    inner.cv.notify_all();
    for (_, cb) in cbs {
        cb();
    }
}

/// Cloneable consumer handle for cooperative cancellation.
#[derive(Clone)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::none()
    }
}

impl CancelToken {
    /// A token that is never cancelled.
    pub fn none() -> Self {
        CancelSource::new().token()
    }

    /// A token that is already cancelled.
    pub fn cancelled() -> Self {
        CancelSource::cancelled().token()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Registers a callback invoked immediately if already cancelled, or upon cancellation.
    /// Dropping the returned [`CancelReg`] unregisters the callback.
    pub fn on_cancel(&self, f: impl FnOnce() + Send + 'static) -> CancelReg {
        if self.is_cancelled() {
            f();
            return CancelReg { inner: None, id: 0 };
        }
        let id = NEXT_CB_ID.fetch_add(1, Ordering::Relaxed);
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.inner.cancelled.load(Ordering::Acquire) {
            drop(st);
            f();
            return CancelReg { inner: None, id: 0 };
        }
        st.callbacks.push((id, Box::new(f)));
        CancelReg {
            inner: Some(self.inner.clone()),
            id,
        }
    }

    /// Blocks until cancelled or `timeout` elapses. Returns `true` if cancelled.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        let deadline = Instant::now() + timeout;
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        while !self.inner.cancelled.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, res) = self
                .inner
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            st = guard;
            if res.timed_out() {
                return self.inner.cancelled.load(Ordering::Acquire);
            }
        }
        true
    }
}
