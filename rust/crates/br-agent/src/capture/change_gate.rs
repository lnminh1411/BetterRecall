// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Pre-grab change gate of one output (`Capture\ChangeGate.cs`).

/// Tracks whether any frame arrived and how much of the screen got dirty since the last grab
/// of this kind (keyframes and replay samples keep one each, A8).
#[derive(Debug, Clone)]
pub struct ChangeGate {
    frames: bool,
    dirty_pct: f64,
}

impl Default for ChangeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeGate {
    pub fn new() -> Self {
        Self {
            frames: true,
            dirty_pct: 100.0,
        }
    }

    /// A new frame came in with this share of the screen dirty (100 when unknown).
    pub fn frame(&mut self, dirty_pct: f64) {
        self.frames = true;
        self.dirty_pct = (self.dirty_pct + dirty_pct).min(100.0);
    }

    /// Duplication restarted: everything counts as changed.
    pub fn invalidate(&mut self) {
        self.frames = true;
        self.dirty_pct = 100.0;
    }

    pub fn changed(&self, min_percent: f64) -> bool {
        self.frames && self.dirty_pct >= min_percent
    }

    pub fn reset(&mut self) {
        self.frames = false;
        self.dirty_pct = 0.0;
    }
}
