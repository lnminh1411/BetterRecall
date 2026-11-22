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
