// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Tile signature (`ChangeMap`) and smart-capture decision (`CapturePolicy`) (`Capture\ChangeMap.cs`).

use zeroize::Zeroize;

/// Change of a grab against the previous stored keyframe of its monitor.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChangeResult {
    pub changed_percent: f64,
    pub text_percent: f64,
    pub score: f64,
    pub comparable: bool,
}

impl ChangeResult {
    pub const FIRST: Self = Self {
        changed_percent: 100.0,
        text_percent: 0.0,
        score: 100.0,
        comparable: false,
    };

    pub const fn new(changed_percent: f64, text_percent: f64, score: f64, comparable: bool) -> Self {
        Self {
            changed_percent,
            text_percent,
            score,
            comparable,
        }
    }
}

/// 32 px tile signature of a keyframe: 4x4 cell luma means and a text-like flag.
#[derive(Debug, Clone)]
pub struct ChangeMap {
    pub width: i32,
    pub height: i32,
    pub cols: usize,
    pub rows: usize,
    pub means: Vec<u8>,
    pub text: Vec<bool>,
}

impl ChangeMap {
    pub const TILE: usize = 32;
    pub const CELLS: usize = 4;
    pub const CELL_PX: usize = Self::TILE / Self::CELLS;
    pub const CELL_DELTA: i32 = 6;
    pub const EDGE_STEP: i32 = 48;
    pub const TEXT_EDGE_MIN: f64 = 0.06;
    pub const TEXT_EDGE_MAX: f64 = 0.5;
    pub const TEXT_WEIGHT: f64 = 5.0;

    pub fn tiles(&self) -> usize {
        self.cols * self.rows
    }

    /// Computes tile signature over 32-bit BGRA pixels (`stride` in bytes).
    pub fn compute(bgra: &[u8], width: i32, height: i32, stride: i32) -> Result<Self, String> {
        if width <= 0 || height <= 0 {
            return Err("Empty image.".to_string());
        }
        let w = width as usize;
        let h = height as usize;
        let st = stride as usize;
        let cols = w.div_ceil(Self::TILE);
        let rows = h.div_ceil(Self::TILE);
        let tiles = cols * rows;
        let mut sums = vec![0i32; tiles * Self::CELLS * Self::CELLS];
        let mut counts = vec![0i32; tiles * Self::CELLS * Self::CELLS];
        let mut edges = vec![0i32; tiles];
        let mut pairs = vec![0i32; tiles];

        let mut y = 0usize;
        while y < h {
            let row = y * st;
            let ty = y / Self::TILE;
            let cy = (y % Self::TILE) / Self::CELL_PX;
            let mut prev_l = -1i32;
            for x in 0..w {
                let p = row + x * 4;
                let l = (29 * (bgra[p] as i32)
                    + 150 * (bgra[p + 1] as i32)
                    + 77 * (bgra[p + 2] as i32))
                    >> 8;
                let tx = x / Self::TILE;
                let t = ty * cols + tx;
                let c = t * 16 + cy * Self::CELLS + (x % Self::TILE) / Self::CELL_PX;
                sums[c] += l;
                counts[c] += 1;
                if prev_l >= 0 && !x.is_multiple_of(Self::TILE) {
                    pairs[t] += 1;
                    if (l - prev_l).abs() >= Self::EDGE_STEP {
                        edges[t] += 1;
                    }
                }
                prev_l = l;
            }
            y += 2;
        }

        let mut means = vec![0u8; sums.len()];
        for i in 0..sums.len() {
            means[i] = if counts[i] == 0 {
                0
            } else {
                (sums[i] / counts[i]) as u8
            };
        }
        let mut text = vec![false; tiles];
        for t in 0..tiles {
            if pairs[t] == 0 {
                continue;
            }
            let d = edges[t] as f64 / pairs[t] as f64;
            text[t] = (Self::TEXT_EDGE_MIN..=Self::TEXT_EDGE_MAX).contains(&d);
        }
        sums.zeroize();
        Ok(Self {
            width,
            height,
            cols,
            rows,
            means,
            text,
        })
    }

    pub fn compare(prev: Option<&Self>, cur: &Self) -> ChangeResult {
        let Some(prev) = prev else {
            return ChangeResult::FIRST;
        };
        if prev.width != cur.width || prev.height != cur.height {
            return ChangeResult::FIRST;
        }
        let mut changed = 0usize;
        let mut text_changed = 0usize;
        for t in 0..cur.tiles() {
            let start = t * 16;
            let mut diff = false;
            for c in start..start + 16 {
                if (prev.means[c] as i32 - cur.means[c] as i32).abs() > Self::CELL_DELTA {
                    diff = true;
                    break;
                }
            }
            if !diff {
                continue;
            }
            changed += 1;
            if prev.text[t] || cur.text[t] {
                text_changed += 1;
            }
        }
        let n = cur.tiles() as f64;
        let changed_pct = changed as f64 * 100.0 / n;
        let text_pct = text_changed as f64 * 100.0 / n;
        let score = (((changed - text_changed) as f64 * 100.0 / n) + Self::TEXT_WEIGHT * text_pct)
            .min(100.0);
        ChangeResult::new(changed_pct, text_pct, score, true)
    }

    pub fn clear(&mut self) {
        self.means.zeroize();
        self.text.fill(false);
    }
}

/// Why a grab is kept (or skipped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    Skip,
    Force,
    First,
    Switch,
    Trigger,
    Change,
    Hash,
}

/// Smart capture policy (`CapturePolicy`).
pub struct CapturePolicy;

impl CapturePolicy {
    pub const TRIGGER_MIN_SCORE: f64 = 0.5;

    pub fn decide(
        force: bool,
        trigger: bool,
        context_changed: bool,
        change: ChangeResult,
        hash_distance: i32,
        dhash_threshold: i32,
        dirty_min_percent: f64,
    ) -> KeepReason {
        if force {
            return KeepReason::Force;
        }
        if !change.comparable {
            return KeepReason::First;
        }
        if context_changed {
            return KeepReason::Switch;
        }
        let hash = dhash_threshold > 0 && hash_distance >= dhash_threshold;
        if trigger {
            return if change.score >= Self::TRIGGER_MIN_SCORE || hash {
                KeepReason::Trigger
            } else {
                KeepReason::Skip
            };
        }
        if change.score > 0.0 && change.score >= dirty_min_percent {
            return KeepReason::Change;
        }
        if hash {
            KeepReason::Hash
        } else {
            KeepReason::Skip
        }
    }
}
