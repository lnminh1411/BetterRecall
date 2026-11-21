// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! 64-bit difference hash (`DHash`) and per-monitor `DedupFilter` (`Capture\DHash.cs`).

use crate::capture::change_map::{CapturePolicy, ChangeMap, ChangeResult, KeepReason};
use crate::capture::output_capture::KeyframeRequest;

/// 64-bit difference hash over a 9x8 grayscale box-averaged thumbnail.
pub struct DHash;

impl DHash {
    /// Computes dHash over 32-bit BGRA pixels (`stride` in bytes).
    pub fn compute(bgra: &[u8], width: i32, height: i32, stride: i32) -> Result<u64, String> {
        if width < 9 || height < 8 {
            return Err("Image too small for dHash.".to_string());
        }
        let mut gray = [0.0f64; 72];
        const SAMPLES: i32 = 8;
        for cy in 0..8 {
            let y0 = cy * height / 8;
            let y1 = (cy + 1) * height / 8;
            for cx in 0..9 {
                let x0 = cx * width / 9;
                let x1 = (cx + 1) * width / 9;
                let mut sum = 0.0f64;
                let mut n = 0i32;
                for sy in 0..SAMPLES {
                    let y = y0 + (((y1 - y0) as f64) * (sy as f64 + 0.5) / SAMPLES as f64) as i32;
                    let row = (y as usize) * (stride as usize);
                    for sx in 0..SAMPLES {
                        let x =
                            x0 + (((x1 - x0) as f64) * (sx as f64 + 0.5) / SAMPLES as f64) as i32;
                        let p = row + (x as usize) * 4;
                        sum += 0.114 * (bgra[p] as f64)
                            + 0.587 * (bgra[p + 1] as f64)
                            + 0.299 * (bgra[p + 2] as f64);
                        n += 1;
                    }
                }
                gray[(cy * 9 + cx) as usize] = sum / n as f64;
            }
        }
        let mut hash = 0u64;
        let mut bit = 0u32;
        for cy in 0..8 {
            for cx in 0..8 {
                if gray[cy * 9 + cx] < gray[cy * 9 + cx + 1] {
                    hash |= 1u64 << bit;
                }
                bit += 1;
            }
        }
        Ok(hash)
    }

    pub fn distance(a: u64, b: u64) -> i32 {
        (a ^ b).count_ones() as i32
    }
}

/// Per-monitor state of the last stored keyframe: dHash, app + title context, and change map.
#[derive(Default)]
pub struct DedupFilter {
    last_hash: Option<u64>,
    last_context: Option<String>,
    last_map: Option<ChangeMap>,
}

impl DedupFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_hash(&self) -> Option<u64> {
        self.last_hash
    }

    pub fn context_differs(&self, context: &str) -> bool {
        self.last_context.as_deref().is_some_and(|c| c != context)
    }

    pub fn decide(
        &self,
        map: &ChangeMap,
        hash: u64,
        req: &KeyframeRequest,
        dhash_threshold: i32,
        dirty_min_percent: f64,
    ) -> (KeepReason, ChangeResult) {
        let change = ChangeMap::compare(self.last_map.as_ref(), map);
        let distance = self
            .last_hash
            .map_or(64, |last| DHash::distance(last, hash));
        let reason = CapturePolicy::decide(
            req.force,
            req.trigger,
            self.context_differs(&req.context),
            change,
            distance,
            dhash_threshold,
            dirty_min_percent,
        );
        (reason, change)
    }

    pub fn accept(&mut self, hash: u64, context: String, map: ChangeMap) {
        self.last_hash = Some(hash);
        self.last_context = Some(context);
        if let Some(ref mut prev) = self.last_map {
            prev.clear();
        }
        self.last_map = Some(map);
    }

    pub fn reset(&mut self) {
        self.last_hash = None;
        self.last_context = None;
        if let Some(ref mut prev) = self.last_map {
            prev.clear();
        }
        self.last_map = None;
    }
}
