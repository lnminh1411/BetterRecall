// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Progress reporting and cooperative cancellation for install, modify, and uninstall.

/// Coarse installation/modify/uninstall phase reported to [`Progress::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Step {
    Verifying = 1,
    Extracting = 2,
    Closing = 3,
    Swapping = 4,
    Registering = 5,
    SavingSettings = 6,
    Unregistering = 7,
    RemovingFiles = 8,
    DeletingData = 9,
    Done = 10,
    DownloadingModels = 11,
    RemovingModels = 12,
}

impl Step {
    /// Converts a raw `u32` (from a Win32 `WM_APP` message) back to a [`Step`].
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::Verifying),
            2 => Some(Self::Extracting),
            3 => Some(Self::Closing),
            4 => Some(Self::Swapping),
            5 => Some(Self::Registering),
            6 => Some(Self::SavingSettings),
            7 => Some(Self::Unregistering),
            8 => Some(Self::RemovingFiles),
            9 => Some(Self::DeletingData),
            10 => Some(Self::Done),
            11 => Some(Self::DownloadingModels),
            12 => Some(Self::RemovingModels),
            _ => None,
        }
    }
}

/// Callback sink for progress updates and cooperative cancellation.
pub trait Progress {
    /// Called when the operation enters a new [`Step`].
    fn step(&self, step: Step);
    /// Called during byte-counted phases (such as payload extraction).
    fn bytes(&self, done: u64, total: u64);
    /// Returns `true` when the caller has requested cancellation.
    fn cancelled(&self) -> bool;
    /// Point of no return. Returns `false` when a cancel arrived first (the run must abort);
    /// once it returned `true`, later Cancel presses are ignored and [`Progress::cancelled`]
    /// stays `false`.
    fn commit(&self) -> bool {
        !self.cancelled()
    }
    /// Returns `true` when the user asked to stop only the model download ("Stop download").
    fn stop_models_requested(&self) -> bool {
        self.cancelled()
    }
}

/// What a Cancel press on the Progress page did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// The run will abort at its next check (before the point of no return).
    Cancel,
    /// Only the model download stops; the install itself completes.
    StopDownload,
    /// Past the point of no return and not downloading: nothing to stop.
    Ignored,
}

/// Cancel state shared by the wizard UI thread and the install worker.
///
/// The worker owns the point of no return through [`CancelGate::commit`], so a Cancel pressed
/// while the worker is between its last check and the swap can no longer be lost (checkpoint 5
/// C1/X2). "Stop download" sets a separate flag that only the model download reads.
#[derive(Debug, Default)]
pub struct CancelGate {
    phase: std::sync::atomic::AtomicU8,
    stop_models: std::sync::atomic::AtomicBool,
    downloading: std::sync::atomic::AtomicBool,
}

const GATE_OPEN: u8 = 0;
const GATE_COMMITTED: u8 = 1;
const GATE_CANCELLED: u8 = 2;

impl CancelGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// UI side: the user pressed Cancel (or "Stop download").
    pub fn press(&self) -> Press {
        use std::sync::atomic::Ordering::SeqCst;
        if self.downloading.load(SeqCst) {
            self.stop_models.store(true, SeqCst);
            return Press::StopDownload;
        }
        match self
            .phase
            .compare_exchange(GATE_OPEN, GATE_CANCELLED, SeqCst, SeqCst)
        {
            Ok(_) | Err(GATE_CANCELLED) => Press::Cancel,
            Err(_) => Press::Ignored,
        }
    }

    /// Worker side: see [`Progress::commit`].
    pub fn commit(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        matches!(
            self.phase
                .compare_exchange(GATE_OPEN, GATE_COMMITTED, SeqCst, SeqCst),
            Ok(_) | Err(GATE_COMMITTED)
        )
    }

    pub fn cancelled(&self) -> bool {
        self.phase.load(std::sync::atomic::Ordering::SeqCst) == GATE_CANCELLED
    }

    pub fn stop_models_requested(&self) -> bool {
        self.stop_models.load(std::sync::atomic::Ordering::SeqCst) || self.cancelled()
    }

    /// Worker side: called synchronously on every step, so a press is routed by the step the
    /// worker is really in, not by the step the UI has seen (posted messages lag).
    pub fn on_step(&self, step: Step) {
        self.downloading.store(
            step == Step::DownloadingModels,
            std::sync::atomic::Ordering::SeqCst,
        );
    }
}

/// No-op [`Progress`] implementation for silent runs and tests that do not observe progress.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn step(&self, _step: Step) {}

    fn bytes(&self, _done: u64, _total: u64) {}

    fn cancelled(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_before_commit_aborts() {
        let g = CancelGate::new();
        assert_eq!(g.press(), Press::Cancel);
        assert!(g.cancelled());
        assert!(!g.commit());
        assert!(g.stop_models_requested());
    }

    #[test]
    fn cancel_after_commit_is_ignored() {
        let g = CancelGate::new();
        assert!(g.commit());
        assert_eq!(g.press(), Press::Ignored);
        assert!(!g.cancelled());
        assert!(!g.stop_models_requested());
        assert!(g.commit());
    }

    #[test]
    fn press_while_downloading_stops_models_only() {
        let g = CancelGate::new();
        assert!(g.commit());
        g.on_step(Step::DownloadingModels);
        assert_eq!(g.press(), Press::StopDownload);
        assert!(g.stop_models_requested());
        assert!(!g.cancelled());
        g.on_step(Step::Closing);
        assert_eq!(g.press(), Press::Ignored);
    }
}
