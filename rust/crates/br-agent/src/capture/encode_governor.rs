// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CPU budget governor for keyframe encoding (`Capture\EncodeGovernor.cs`).

use std::collections::VecDeque;

/// Keyframe encode levels, cheapest last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(usize)]
pub enum EncodeLevel {
    Normal = 0,
    Fast = 1,
    Reduced = 2,
    WebP = 3,
}

impl EncodeLevel {
    const ALL_AVIF: [EncodeLevel; 3] = [EncodeLevel::Normal, EncodeLevel::Fast, EncodeLevel::Reduced];

    pub const fn index(self) -> usize {
        self as usize
    }
}

type ClockFn = Box<dyn FnMut() -> i64 + Send>;
type EncodeFn<T> = Box<dyn FnMut(&mut T, EncodeLevel) -> f64 + Send>;
type DropFn<T> = Box<dyn FnMut(&mut T, &str) + Send>;

/// CPU budget for keyframe encoding: under 1% average over 60 s and at most 1.5% peak over 10 s
/// of total logical-processor time. Pure logic; clock and callbacks are injected.
pub struct EncodeGovernor<T> {
    clock_ms: ClockFn,
    encode: EncodeFn<T>,
    on_drop: DropFn<T>,
    samples: VecDeque<(i64, f64)>,
    waiting: VecDeque<(u64, T, i64)>,
    next_ticket: u64,
    estimate: [f64; 4],
    initial: [f64; 4],
    last_run: [i64; 4],
    used10: f64,
    used60: f64,
    limit10_ms: f64,
    limit60_ms: f64,
    level: EncodeLevel,
    sample_estimate: f64,
    pub encoded: u64,
    pub deferred: u64,
    pub dropped: u64,
    pub webp_fallbacks: u64,
    pub downgraded: u64,
    pub samples_deferred: u64,
}

impl<T> EncodeGovernor<T> {
    pub const AVG_PERCENT: f64 = 1.0;
    pub const PEAK_PERCENT: f64 = 1.5;
    pub const WINDOW10_TARGET: f64 = 0.8;
    pub const WINDOW60_TARGET: f64 = 0.9;
    pub const QUEUE_CAPACITY: usize = 6;
    pub const MAX_WAIT_MS: i64 = 30_000;
    pub const RELAX_AFTER_MS: i64 = 60_000;

    pub fn new(
        mut clock_ms: impl FnMut() -> i64 + Send + 'static,
        logical_processors: usize,
        encode: impl FnMut(&mut T, EncodeLevel) -> f64 + Send + 'static,
        on_drop: impl FnMut(&mut T, &str) + Send + 'static,
        initial_estimate_ms: [f64; 4],
    ) -> Self {
        let t0 = clock_ms();
        let procs = logical_processors.max(1) as f64;
        Self {
            clock_ms: Box::new(clock_ms),
            encode: Box::new(encode),
            on_drop: Box::new(on_drop),
            samples: VecDeque::new(),
            waiting: VecDeque::new(),
            next_ticket: 1,
            estimate: initial_estimate_ms,
            initial: initial_estimate_ms,
            last_run: [t0; 4],
            used10: 0.0,
            used60: 0.0,
            limit10_ms: Self::PEAK_PERCENT / 100.0 * procs * 10_000.0,
            limit60_ms: Self::AVG_PERCENT / 100.0 * procs * 60_000.0,
            level: EncodeLevel::Normal,
            sample_estimate: -1.0,
            encoded: 0,
            deferred: 0,
            dropped: 0,
            webp_fallbacks: 0,
            downgraded: 0,
            samples_deferred: 0,
        }
    }

    pub fn limit10_ms(&self) -> f64 {
        self.limit10_ms
    }

    pub fn limit60_ms(&self) -> f64 {
        self.limit60_ms
    }

    pub fn level(&self) -> EncodeLevel {
        self.level
    }

    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    pub fn estimate(&self, level: EncodeLevel) -> f64 {
        self.estimate[level.index()]
    }

    /// CPU ms of encoding in the last 10 s and 60 s.
    pub fn used(&mut self) -> (f64, f64) {
        let now = (self.clock_ms)();
        self.prune(now);
        (self.used10, self.used60)
    }

    /// Fuller of the two windows as a share of its target (0 idle, 1 at target).
    pub fn load(&mut self) -> f64 {
        let now = (self.clock_ms)();
        self.prune(now);
        (self.used10 / (Self::WINDOW10_TARGET * self.limit10_ms))
            .max(self.used60 / (Self::WINDOW60_TARGET * self.limit60_ms))
    }

    pub fn offer(&mut self, item: T) {
        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.wrapping_add(1);
        let now = (self.clock_ms)();
        self.waiting.push_back((ticket, item, now));
        self.pump();
        if self.waiting.iter().any(|(tid, _, _)| *tid == ticket) {
            self.deferred += 1;
        }
    }

    pub fn pump(&mut self) {
        while !self.waiting.is_empty() {
            let now = (self.clock_ms)();
            self.prune(now);
            if let Some(level) = self.choose() {
                if let Some((_, item, _)) = self.waiting.pop_front() {
                    self.run_item(item, level);
                }
                continue;
            }
            let full = self.waiting.len() > Self::QUEUE_CAPACITY;
            let stale = self
                .waiting
                .front()
                .is_some_and(|(_, _, since)| now - *since > Self::MAX_WAIT_MS);
            if !full && !stale {
                return;
            }
            let Some((_, mut item, _)) = self.waiting.pop_front() else {
                return;
            };
            if self.fits(EncodeLevel::WebP) {
                self.webp_fallbacks += 1;
                self.run_item(item, EncodeLevel::WebP);
            } else {
                self.dropped += 1;
                let reason = if full { "queue full" } else { "waited too long" };
                (self.on_drop)(&mut item, reason);
                drop(item);
            }
        }
    }

    pub fn choose(&mut self) -> Option<EncodeLevel> {
        let now = (self.clock_ms)();
        self.relax(now);
        let load = self.load();
        let start = if load < 0.5 {
            0
        } else if load < 0.75 {
            1
        } else {
            2
        };
        EncodeLevel::ALL_AVIF[start..].iter().find(|&&l| self.fits(l)).copied()
    }

    fn relax(&mut self, now: i64) {
        for i in 0..self.estimate.len() {
            if now - self.last_run[i] < Self::RELAX_AFTER_MS {
                continue;
            }
            self.last_run[i] = now;
            if self.estimate[i] > self.initial[i] {
                self.estimate[i] = (self.estimate[i] + self.initial[i]) / 2.0;
            }
        }
    }

    fn fits(&self, level: EncodeLevel) -> bool {
        let est = self.estimate[level.index()];
        self.used10 + est <= Self::WINDOW10_TARGET * self.limit10_ms
            && self.used60 + est <= Self::WINDOW60_TARGET * self.limit60_ms
    }

    /// True if a replay sample encode fits both windows and no keyframe is waiting (A19).
    pub fn try_admit_sample(&mut self) -> bool {
        let now = (self.clock_ms)();
        self.prune(now);
        let est = if self.sample_estimate >= 0.0 {
            self.sample_estimate
        } else {
            self.estimate[EncodeLevel::WebP.index()]
        };
        let fits = self.waiting.is_empty()
            && self.used10 + est <= Self::WINDOW10_TARGET * self.limit10_ms
            && self.used60 + est <= Self::WINDOW60_TARGET * self.limit60_ms;
        if !fits {
            self.samples_deferred += 1;
        }
        fits
    }

    pub fn charge_sample(&mut self, ms: f64) {
        let ms = ms.max(0.0);
        self.sample_estimate = if self.sample_estimate < 0.0 {
            ms
        } else {
            self.sample_estimate * 0.7 + ms * 0.3
        };
        self.charge(ms);
    }

    pub fn charge(&mut self, ms: f64) {
        if ms <= 0.0 {
            return;
        }
        let now = (self.clock_ms)();
        self.prune(now);
        self.samples.push_back((now, ms));
        self.used10 += ms;
        self.used60 += ms;
    }

    fn run_item(&mut self, mut item: T, level: EncodeLevel) {
        let ms = (self.encode)(&mut item, level).max(0.0);
        drop(item);
        let now = (self.clock_ms)();
        self.samples.push_back((now, ms));
        self.used10 += ms;
        self.used60 += ms;
        let idx = level.index();
        self.estimate[idx] = self.estimate[idx] * 0.7 + ms * 0.3;
        self.last_run[idx] = now;
        if level != EncodeLevel::Normal {
            self.downgraded += 1;
        }
        self.encoded += 1;
        self.level = level;
    }

    fn prune(&mut self, now: i64) {
        while let Some(&(t, ms)) = self.samples.front() {
            if now - t >= 60_000 {
                self.samples.pop_front();
                self.used60 -= ms;
            } else {
                break;
            }
        }
        if self.samples.is_empty() {
            self.used60 = 0.0;
        }
        let mut s10 = 0.0;
        for &(t, ms) in &self.samples {
            if now - t < 10_000 {
                s10 += ms;
            }
        }
        self.used10 = s10;
    }

    pub fn clear(&mut self) {
        self.waiting.clear();
    }
}
