// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! RAM-only ring of encoded replay frame samples for searchable replays (`Replay\ReplaySampler.cs`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use br_contracts::NewReplayFrame;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use zeroize::Zeroize;

use crate::capture::keyframe_pipeline::{ReplaySample, ReplaySampleSink};

/// One sampled frame in [`ReplaySampler`].
#[derive(Debug, Clone)]
pub struct Sample {
    pub ts: DateTime<Utc>,
    pub monitor: i32,
    pub app: String,
    pub title: String,
    pub site: String,
    pub width: i32,
    pub height: i32,
    pub bytes: Vec<u8>,
    pub mime: String,
    pub grabbed: Option<DateTime<Utc>>,
}

struct SamplerInner {
    ring: Vec<Sample>,
    last: HashMap<i32, DateTime<Utc>>,
}

type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// RAM-only ring of encoded frames taken every `replay.sampleSeconds`.
pub struct ReplaySampler {
    inner: Mutex<SamplerInner>,
    now: ClockFn,
}

impl Default for ReplaySampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplaySampler {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(Utc::now))
    }

    pub fn with_clock(now: ClockFn) -> Self {
        Self {
            inner: Mutex::new(SamplerInner {
                ring: Vec::new(),
                last: HashMap::new(),
            }),
            now,
        }
    }

    pub fn count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ring
            .len()
    }

    pub fn bytes(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ring
            .iter()
            .map(|s| s.bytes.len())
            .sum()
    }

    /// True when the monitor has no sample in the last `sample_seconds` (minus 400 ms jitter slack).
    pub fn due(&self, monitor: i32, sample_seconds: i32) -> bool {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(&t) = g.last.get(&monitor) else {
            return true;
        };
        let threshold =
            ChronoDuration::seconds(sample_seconds as i64) - ChronoDuration::milliseconds(400);
        (self.now)() - t >= threshold
    }

    /// Keeps a copy of `s.bytes`; drops samples older than `keep_seconds`.
    pub fn add(&self, s: Sample, keep_seconds: i32) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mon = s.monitor;
        let ts = s.ts;
        g.ring.push(s);
        g.last.insert(mon, ts);
        self.trim_locked(&mut g, keep_seconds);
    }

    fn trim_locked(&self, g: &mut SamplerInner, keep_seconds: i32) {
        let cutoff = (self.now)() - ChronoDuration::seconds(keep_seconds.max(15) as i64);
        let stale_cutoff = cutoff - ChronoDuration::seconds(60);
        for i in (0..g.ring.len()).rev() {
            if g.ring[i].ts >= cutoff {
                continue;
            }
            let stale = g.ring[i].ts < stale_cutoff;
            let mon = g.ring[i].monitor;
            let newer_old_exists = g.ring[i + 1..]
                .iter()
                .any(|x| x.monitor == mon && x.ts < cutoff);
            if !stale && !newer_old_exists {
                continue;
            }
            g.ring[i].bytes.zeroize();
            g.ring.remove(i);
        }
    }

    /// Copies of samples belonging to `[start, end]`, plus per monitor the last sample before
    /// `start` when the monitor has none in its first `sample_seconds`.
    pub fn take(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        sample_seconds: i32,
    ) -> Vec<Sample> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<Sample> = g
            .ring
            .iter()
            .filter(|s| s.ts >= start && s.ts <= end)
            .cloned()
            .collect();
        let monitors: HashSet<i32> = g.ring.iter().map(|s| s.monitor).collect();
        let early_limit = start + ChronoDuration::seconds(sample_seconds as i64);
        for mon in monitors {
            let early = list
                .iter()
                .any(|s| s.monitor == mon && s.ts < early_limit);
            if early {
                continue;
            }
            if let Some(before) = g
                .ring
                .iter()
                .filter(|s| s.monitor == mon && s.ts < start)
                .max_by_key(|s| s.ts)
            {
                let mut carried = before.clone();
                carried.grabbed = Some(before.grabbed.unwrap_or(before.ts));
                carried.ts = start;
                list.push(carried);
            }
        }
        list.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.monitor.cmp(&b.monitor)));
        list
    }

    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for s in &mut g.ring {
            s.bytes.zeroize();
        }
        g.ring.clear();
        g.last.clear();
    }

    /// Converts samples to store frames: offset from `start`, clamped to `[0, duration_ms]`.
    pub fn to_frames(
        replay_id: &str,
        start: DateTime<Utc>,
        duration_ms: i32,
        samples: &[Sample],
    ) -> Vec<NewReplayFrame> {
        let max_ms = duration_ms.max(0) as i64;
        samples
            .iter()
            .map(|s| {
                let offset = (s.ts - start).num_milliseconds().clamp(0, max_ms) as i32;
                NewReplayFrame {
                    ts: s.ts,
                    replay_id: replay_id.to_string(),
                    offset_ms: offset,
                    app: s.app.clone(),
                    title: s.title.clone(),
                    monitor: s.monitor,
                    width: s.width,
                    height: s.height,
                    image: s.bytes.clone(),
                    image_mime: s.mime.clone(),
                    site: s.site.clone(),
                    captured_at: s.grabbed,
                }
            })
            .collect()
    }
}

impl ReplaySampleSink for ReplaySampler {
    fn add(&self, sample: ReplaySample, keep_seconds: i32) {
        ReplaySampler::add(
            self,
            Sample {
                ts: sample.ts,
                monitor: sample.monitor,
                app: sample.app,
                title: sample.title,
                site: sample.site,
                width: sample.width,
                height: sample.height,
                bytes: sample.bytes,
                mime: sample.mime,
                grabbed: None,
            },
            keep_seconds,
        );
    }
}
