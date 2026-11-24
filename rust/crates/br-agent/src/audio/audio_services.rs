// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! LoopbackHub, AudioReplayEncoder, TranscriptSource, AudioGuardTimeline, and Transcriber (`Audio\AudioServices.cs`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use br_contracts::{NewAudioCapture, Settings, TranscribeResult};
use br_core::{CancelSource, CancelToken, EncryptedStore, StoreError};
use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::audio::aac_encoder::AacEncoder;
use crate::audio::speech_segmenter::{Pcm, Resampler, SpeechSegment, SpeechSegmenter};
use crate::audio::wasapi::{
    CaptureStream, PcmFormat, PcmHandler, WasapiCapture, wave_format_float32, wave_format_pcm16,
};
use crate::capture::output_capture::OutputCapture;
use crate::guardrails::{AudioGate, AudioVerdict};
use crate::native::{qpc, tick_count64};
use crate::privacy::{RedactionOptions, Redactor};
use crate::replay::packet_ring::PacketRing;
use crate::replay::replay_mux::AudioRingSource;
use crate::util::{AgentLog, RetryBackoff};
use crate::worker::{WorkerClient, WorkerError};

type OpenStreamFn = Arc<dyn Fn(PcmHandler) -> Result<Arc<dyn CaptureStream>, String> + Send + Sync>;

struct HubState {
    subs: Vec<(u64, PcmHandler)>,
    capture: Option<Arc<dyn CaptureStream>>,
    restart: RetryBackoff,
    up_since_ms: i64,
    restarts: i32,
}

/// One process-loopback stream (48 kHz stereo 16-bit, own process tree excluded) shared by the
/// replay and system transcripts. Restarts with backoff when the stream ends (A13).
pub struct LoopbackHub {
    open: OpenStreamFn,
    state: Mutex<HubState>,
    next_id: AtomicU64,
}

impl Default for LoopbackHub {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopbackHub {
    pub fn new() -> Self {
        Self::with_opener(Arc::new(|h| {
            WasapiCapture::start("loopback", wave_format_pcm16(48_000, 2), h)
                .map(|c| Arc::new(c) as Arc<dyn CaptureStream>)
        }))
    }

    pub fn with_opener(open: OpenStreamFn) -> Self {
        Self {
            open,
            state: Mutex::new(HubState {
                subs: Vec::new(),
                capture: None,
                restart: RetryBackoff::new(1000, 60_000),
                up_since_ms: 0,
                restarts: 0,
            }),
            next_id: AtomicU64::new(1),
        }
    }

    pub const fn format(&self) -> PcmFormat {
        PcmFormat {
            sample_rate: 48_000,
            channels: 2,
            is_float: false,
            bits_per_sample: 16,
        }
    }

    pub fn running(&self) -> bool {
        let g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        g.capture.as_ref().is_some_and(|c| !c.ended())
    }

    pub fn restarts(&self) -> i32 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).restarts
    }

    pub fn subscribe(self: &Arc<Self>, h: PcmHandler) -> Result<u64, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut dead: Option<Arc<dyn CaptureStream>> = None;
        let res = {
            let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
            g.subs.push((id, h));
            if g.capture.as_ref().is_some_and(|c| c.ended()) {
                dead = g.capture.take();
            }
            if g.capture.is_none() {
                let weak = Arc::downgrade(self);
                let dispatch: PcmHandler = Arc::new(move |data, frames, qpc, silent| {
                    if let Some(hub) = weak.upgrade() {
                        hub.dispatch(data, frames, qpc, silent);
                    }
                });
                match (self.open)(dispatch) {
                    Ok(c) => {
                        g.capture = Some(c);
                        g.restart.succeeded();
                        Ok(id)
                    }
                    Err(e) => {
                        AgentLog::warn_err("loopback start", &e);
                        g.subs.retain(|(i, _)| *i != id);
                        Err(e)
                    }
                }
            } else {
                Ok(id)
            }
        };
        if let Some(d) = dead {
            d.stop();
        }
        res
    }

    /// Called from status tick: restarts an ended stream once backoff allows.
    pub fn check_health(self: &Arc<Self>, now_ms: i64) -> bool {
        // D1: recover if a panic poisoned the state mutex instead of staying "busy" forever.
        let Some(mut g) = crate::util::try_lock_recover(&self.state, "loopback_hub") else {
            return false;
        };
        if g.subs.is_empty() {
            return false;
        }
        let mut dead: Option<Arc<dyn CaptureStream>> = None;
        let mut restarted = false;
        if g.capture.as_ref().is_some_and(|c| c.ended()) {
            dead = g.capture.take();
            AgentLog::warn("loopback stream ended; restarting");
            g.restart.failed(now_ms);
        }
        if g.capture.is_none() && g.restart.due(now_ms) {
            let weak = Arc::downgrade(self);
            let dispatch: PcmHandler = Arc::new(move |data, frames, qpc, silent| {
                if let Some(hub) = weak.upgrade() {
                    hub.dispatch(data, frames, qpc, silent);
                }
            });
            match (self.open)(dispatch) {
                Ok(c) => {
                    g.capture = Some(c);
                    g.restarts += 1;
                    restarted = true;
                    g.up_since_ms = now_ms;
                    AgentLog::info("loopback stream restarted");
                }
                Err(e) => {
                    let next_s = g.restart.failed(now_ms) / 1000;
                    AgentLog::warn_err(&format!("loopback restart (next in {next_s} s)"), &e);
                }
            }
        } else if g.capture.as_ref().is_some_and(|c| !c.ended())
            && g.restart.failures() > 0
            && now_ms - g.up_since_ms >= 60_000
        {
            g.restart.succeeded();
        }
        drop(g);
        if let Some(d) = dead {
            d.stop();
        }
        restarted
    }

    pub fn unsubscribe(&self, id: u64) {
        let stop = {
            let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
            g.subs.retain(|(i, _)| *i != id);
            if g.subs.is_empty() {
                g.capture.take()
            } else {
                None
            }
        };
        if let Some(c) = stop {
            c.stop();
        }
    }

    fn dispatch(&self, data: &[u8], frames: usize, qpc: i64, silent: bool) {
        let subs: Vec<PcmHandler> = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subs
            .iter()
            .map(|(_, h)| h.clone())
            .collect();
        for s in subs {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                s(data, frames, qpc, silent);
            }));
        }
    }

    pub fn stop(&self) {
        let c = {
            let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
            g.subs.clear();
            g.capture.take()
        };
        if let Some(cap) = c {
            cap.stop();
        }
    }
}

impl Drop for LoopbackHub {
    fn drop(&mut self) {
        self.stop();
    }
}

type BoolProvider = Arc<dyn Fn() -> bool + Send + Sync>;

/// Desktop audio for the replay: AAC into a RAM ring with QPC timestamps (`AudioReplayEncoder`).
pub struct AudioReplayEncoder {
    aac: Arc<AacEncoder>,
    ring: Arc<PacketRing>,
    source: AudioRingSource,
    hub: Arc<LoopbackHub>,
    sub_id: u64,
    seconds: i32,
    paused: Arc<AtomicBool>,
    frames_captured: Arc<AtomicI64>,
    muted_packets: Arc<AtomicI64>,
    silent_packets: Arc<AtomicI64>,
    disposed: AtomicBool,
}

impl AudioReplayEncoder {
    pub fn new(
        hub: Arc<LoopbackHub>,
        seconds: i32,
        lock_memory: bool,
        muted: BoolProvider,
    ) -> Result<Self, String> {
        let cap = (16_000usize) * ((seconds + 4) as usize) * 5 / 4;
        let ring = Arc::new(PacketRing::new(
            cap,
            Duration::from_secs(seconds as u64),
            lock_memory,
        )?);
        let aac = Arc::new(AacEncoder::new(ring.clone())?);
        let source = AudioRingSource {
            ring: ring.clone(),
            sample_rate: AacEncoder::SAMPLE_RATE,
            channels: AacEncoder::CHANNELS,
            audio_specific_config: AacEncoder::AUDIO_SPECIFIC_CONFIG.to_vec(),
        };

        let paused = Arc::new(AtomicBool::new(false));
        let frames_captured = Arc::new(AtomicI64::new(0));
        let muted_packets = Arc::new(AtomicI64::new(0));
        let silent_packets = Arc::new(AtomicI64::new(0));

        let aac_cb = aac.clone();
        let paused_cb = paused.clone();
        let frames_cb = frames_captured.clone();
        let muted_cb = muted_packets.clone();
        let silent_cb = silent_packets.clone();

        let handler: PcmHandler = Arc::new(move |data, frames, qpc, silent| {
            if paused_cb.load(Ordering::Acquire) {
                return;
            }
            frames_cb.fetch_add(frames as i64, Ordering::Relaxed);
            let is_muted = muted();
            if is_muted {
                muted_cb.fetch_add(1, Ordering::Relaxed);
            }
            let quiet = Self::quiet(silent, is_muted, data);
            if quiet {
                silent_cb.fetch_add(1, Ordering::Relaxed);
            }
            aac_cb.push(data, qpc, quiet);
        });

        let sub_id = hub.subscribe(handler)?;
        Ok(Self {
            aac,
            ring,
            source,
            hub,
            sub_id,
            seconds,
            paused,
            frames_captured,
            muted_packets,
            silent_packets,
            disposed: AtomicBool::new(false),
        })
    }

    pub fn ring(&self) -> &Arc<PacketRing> {
        &self.ring
    }

    pub fn source(&self) -> AudioRingSource {
        self.source.clone()
    }

    pub fn seconds(&self) -> i32 {
        self.seconds
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn frames_captured(&self) -> i64 {
        self.frames_captured.load(Ordering::Acquire)
    }

    pub fn gaps(&self) -> i64 {
        self.aac.gaps()
    }

    pub fn muted_packets(&self) -> i64 {
        self.muted_packets.load(Ordering::Acquire)
    }

    pub fn silent_packets(&self) -> i64 {
        self.silent_packets.load(Ordering::Acquire)
    }

    /// The packet goes into the ring as silence: no sound, near silence, or muted by the guard (D1). Pure.
    pub fn quiet(silent: bool, muted: bool, data: &[u8]) -> bool {
        silent || muted || Self::is_near_silent_16(data)
    }

    pub fn is_all_zero(data: &[u8]) -> bool {
        data.iter().all(|&b| b == 0)
    }

    /// Peak below about -54 dBFS on 16-bit PCM: dither and idle-device noise.
    pub fn is_near_silent_16(data: &[u8]) -> bool {
        for chunk in data.chunks_exact(2) {
            let v = i16::from_le_bytes([chunk[0], chunk[1]]);
            if !(-64..=64).contains(&v) {
                return false;
            }
        }
        true
    }

    pub fn stop(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.hub.unsubscribe(self.sub_id);
        self.aac.dispose();
    }
}

impl Drop for AudioReplayEncoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Guard verdicts over time for one audio source (A-H2).
#[derive(Default)]
pub struct AudioGuardTimeline {
    entries: Vec<(i64, AudioVerdict)>,
}

impl AudioGuardTimeline {
    pub const KEEP_FOR_100NS: i64 = 10 * 60 * 10_000_000;
    pub const MAX_ENTRIES: usize = 4096;
    const START_SLACK_100NS: i64 = 5_000_000;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }

    pub fn note(&mut self, qpc_100ns: i64, v: AudioVerdict) {
        if self.entries.last().is_some_and(|(_, last)| *last == v) {
            return;
        }
        self.entries.push((qpc_100ns, v));
        let mut drop_n = 0usize;
        while drop_n + 1 < self.entries.len()
            && (qpc_100ns - self.entries[drop_n + 1].0 > Self::KEEP_FOR_100NS
                || self.entries.len() - drop_n > Self::MAX_ENTRIES)
        {
            drop_n += 1;
        }
        if drop_n > 0 {
            self.entries.drain(..drop_n);
        }
    }

    /// Blocked when any verdict in `[start, end]` blocks, Unknown when one is still unresolved,
    /// else Allowed with the app in front for most of the span.
    pub fn check(&self, start: i64, end: i64) -> AudioVerdict {
        if self.entries.is_empty() || start < self.entries[0].0 - Self::START_SLACK_100NS {
            return AudioVerdict {
                gate: AudioGate::Blocked,
                app: String::new(),
                epoch: 0,
            };
        }
        let mut i = 0usize;
        while i + 1 < self.entries.len() && self.entries[i + 1].0 <= start {
            i += 1;
        }
        let mut gate = AudioGate::Allowed;
        let mut time: HashMap<String, i64> = HashMap::new();
        let mut app = String::new();
        let mut best = -1i64;
        let mut j = i;
        while j < self.entries.len() && (j == i || self.entries[j].0 <= end) {
            let v = self.resolve(j);
            if v.gate == AudioGate::Blocked {
                return AudioVerdict {
                    gate: AudioGate::Blocked,
                    app: v.app,
                    epoch: 0,
                };
            }
            if v.gate == AudioGate::Unknown {
                gate = AudioGate::Unknown;
            }
            let to = if j + 1 < self.entries.len() {
                end.min(self.entries[j + 1].0)
            } else {
                end
            };
            let dt = (to - start.max(self.entries[j].0)).max(0);
            let t = {
                let entry = time.entry(v.app.clone()).or_insert(0);
                *entry += dt;
                *entry
            };
            if t > best {
                best = t;
                app = v.app;
            }
            j += 1;
        }
        AudioVerdict {
            gate,
            app,
            epoch: 0,
        }
    }

    fn resolve(&self, j: usize) -> AudioVerdict {
        let u = &self.entries[j].1;
        if u.gate != AudioGate::Unknown {
            return u.clone();
        }
        for (_, v) in &self.entries[j + 1..] {
            if v.gate == AudioGate::Unknown {
                continue;
            }
            return if v.epoch == u.epoch {
                v.clone()
            } else {
                AudioVerdict {
                    gate: AudioGate::Blocked,
                    app: u.app.clone(),
                    epoch: u.epoch,
                }
            };
        }
        u.clone()
    }
}

type SettingsProvider = Arc<dyn Fn() -> Settings + Send + Sync>;
type GuardProvider = Arc<dyn Fn() -> AudioVerdict + Send + Sync>;

struct QueueInner {
    items: VecDeque<SpeechSegment>,
    closed: bool,
}

struct TranscriptInner {
    segmenter: SpeechSegmenter,
    buf: Vec<f32>,
    out: Vec<SpeechSegment>,
    resampler: Option<Resampler>,
    guard_timeline: AudioGuardTimeline,
    clear_cts: CancelSource,
    evict_warn_at: i64,
}

/// Speech transcripts for one source (`"system"` or `"mic"`): 16 kHz mono, energy gate and segmenter.
pub struct TranscriptSource {
    source: String,
    paused: BoolProvider,
    guard: GuardProvider,
    queue: Mutex<QueueInner>,
    queue_cv: Condvar,
    inner: Mutex<TranscriptInner>,
    hub: Option<Arc<LoopbackHub>>,
    hub_sub_id: Mutex<Option<u64>>,
    mic: Mutex<Option<WasapiCapture>>,
    clear_gen: AtomicI32,
    has_buffered: AtomicBool,
    segments_count: AtomicI64,
    evicted: AtomicI64,
    disposed: AtomicBool,
}

/// Error returned by [`TranscriptSource::recv_timeout`] when the queue is closed and empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueClosed;

impl TranscriptSource {
    pub const QUEUE_SEGMENTS: usize = 8;
    pub const GUARD_PAD_100NS: i64 = 2_500_000;

    pub fn new(
        source: &str,
        hub: Option<Arc<LoopbackHub>>,
        settings: SettingsProvider,
        paused: BoolProvider,
        guard: GuardProvider,
    ) -> Result<Arc<Self>, String> {
        let settings_max = settings.clone();
        let ts = Arc::new(Self {
            source: source.to_string(),
            paused,
            guard,
            queue: Mutex::new(QueueInner {
                items: VecDeque::with_capacity(Self::QUEUE_SEGMENTS),
                closed: false,
            }),
            queue_cv: Condvar::new(),
            inner: Mutex::new(TranscriptInner {
                segmenter: SpeechSegmenter::new(Arc::new(move || {
                    settings_max().audio.max_segment_s
                })),
                buf: Vec::new(),
                out: Vec::new(),
                resampler: None,
                guard_timeline: AudioGuardTimeline::new(),
                clear_cts: CancelSource::new(),
                evict_warn_at: i64::MIN,
            }),
            hub: hub.clone(),
            hub_sub_id: Mutex::new(None),
            mic: Mutex::new(None),
            clear_gen: AtomicI32::new(0),
            has_buffered: AtomicBool::new(false),
            segments_count: AtomicI64::new(0),
            evicted: AtomicI64::new(0),
            disposed: AtomicBool::new(false),
        });

        let weak = Arc::downgrade(&ts);
        let handler: PcmHandler = Arc::new(move |data, frames, qpc, silent| {
            if let Some(src) = weak.upgrade() {
                src.on_pcm(data, frames, qpc, silent);
            }
        });

        if source == "system" {
            let h = hub.ok_or_else(|| "LoopbackHub required for system source.".to_string())?;
            ts.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .resampler = Some(Resampler::new(h.format()));
            let sub_id = h.subscribe(handler)?;
            *ts.hub_sub_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(sub_id);
        } else {
            let mic = WasapiCapture::start("mic", wave_format_float32(16_000, 1), handler)?;
            ts.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .resampler = Some(Resampler::new(mic.format()));
            *ts.mic.lock().unwrap_or_else(|e| e.into_inner()) = Some(mic);
        }
        Ok(ts)
    }

    /// Creates a detached source for unit tests (no WASAPI device opened).
    pub fn new_detached(
        source: &str,
        settings: SettingsProvider,
        paused: BoolProvider,
        guard: GuardProvider,
        fmt: Option<PcmFormat>,
    ) -> Arc<Self> {
        let settings_max = settings;
        Arc::new(Self {
            source: source.to_string(),
            paused,
            guard,
            queue: Mutex::new(QueueInner {
                items: VecDeque::with_capacity(Self::QUEUE_SEGMENTS),
                closed: false,
            }),
            queue_cv: Condvar::new(),
            inner: Mutex::new(TranscriptInner {
                segmenter: SpeechSegmenter::new(Arc::new(move || {
                    settings_max().audio.max_segment_s
                })),
                buf: Vec::new(),
                out: Vec::new(),
                resampler: fmt.map(Resampler::new),
                guard_timeline: AudioGuardTimeline::new(),
                clear_cts: CancelSource::new(),
                evict_warn_at: i64::MIN,
            }),
            hub: None,
            hub_sub_id: Mutex::new(None),
            mic: Mutex::new(None),
            clear_gen: AtomicI32::new(0),
            has_buffered: AtomicBool::new(false),
            segments_count: AtomicI64::new(0),
            evicted: AtomicI64::new(0),
            disposed: AtomicBool::new(false),
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn mic_lost(&self) -> bool {
        self.mic
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(WasapiCapture::lost)
    }

    pub fn segments(&self) -> i64 {
        self.segments_count.load(Ordering::Acquire)
    }

    pub fn evicted(&self) -> i64 {
        self.evicted.load(Ordering::Acquire)
    }

    pub fn clear_gen(&self) -> i32 {
        self.clear_gen.load(Ordering::Acquire)
    }

    pub fn generation(&self) -> (i32, CancelToken) {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (self.clear_gen.load(Ordering::Acquire), g.clear_cts.token())
    }

    pub fn open_segment_start(&self) -> Option<i64> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .segmenter
            .open_start_qpc()
    }

    pub fn segmenter_active(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .segmenter
            .active()
    }

    pub fn has_buffered_pcm(&self) -> bool {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.segmenter.active() || !g.buf.is_empty() || self.has_buffered.load(Ordering::Acquire)
    }

    pub fn note_guard(&self, qpc_100ns: i64, v: AudioVerdict) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .guard_timeline
            .note(qpc_100ns, v);
    }

    pub fn guard_for(&self, s: &SpeechSegment) -> AudioVerdict {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.guard_timeline.check(
            s.start_qpc_100ns - Self::GUARD_PAD_100NS,
            s.start_qpc_100ns + (s.duration_ms as i64) * 10_000 + Self::GUARD_PAD_100NS,
        )
    }

    /// Feeds PCM directly (used by WASAPI callback and tests).
    pub fn on_pcm(&self, data: &[u8], frames: usize, qpc: i64, silent: bool) {
        if (self.paused)() {
            if self.has_buffered.load(Ordering::Acquire) {
                self.flush();
            }
            return;
        }
        let verdict = (self.guard)();
        let quiet = silent || AudioReplayEncoder::is_all_zero(data);
        let mut produced = Vec::new();
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if (self.paused)() {
                self.flush_locked(&mut g, &mut produced);
            } else {
                g.guard_timeline.note(qpc, verdict);
                if quiet && !g.segmenter.active() {
                    if let Some(ref mut r) = g.resampler {
                        r.reset();
                    }
                    g.segmenter.reanchor();
                    return;
                }
                self.has_buffered.store(true, Ordering::Release);
                Pcm::wipe(&mut g.buf);
                let inner_ref = &mut *g;
                if let Some(ref mut r) = inner_ref.resampler {
                    r.process(data, frames, quiet, &mut inner_ref.buf);
                }
                inner_ref.out.clear();
                inner_ref
                    .segmenter
                    .push(&inner_ref.buf, qpc, &mut inner_ref.out);
                produced.append(&mut inner_ref.out);
                Pcm::wipe(&mut inner_ref.buf);
            }
        }
        for seg in produced {
            self.enqueue_segment(seg);
        }
    }

    /// Enqueues a segment directly (used by `on_pcm`, `flush`, and tests).
    pub fn enqueue_segment(&self, seg: SpeechSegment) {
        let _ = self.enqueue_segment_for_test(seg);
    }

    /// Enqueues a segment and, if an oldest segment was evicted, returns it after zeroization
    /// along with its original sample count (for tests).
    pub fn enqueue_segment_for_test(&self, seg: SpeechSegment) -> Option<(SpeechSegment, usize)> {
        self.segments_count.fetch_add(1, Ordering::AcqRel);
        let evicted = {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            if q.closed {
                let mut s = seg;
                s.zeroize_samples();
                return None;
            }
            let ev = if q.items.len() >= Self::QUEUE_SEGMENTS {
                q.items.pop_front()
            } else {
                None
            };
            q.items.push_back(seg);
            self.queue_cv.notify_one();
            ev
        };
        if let Some(mut old) = evicted {
            let orig_len = old.samples.len();
            old.zeroize_samples();
            let n = self.evicted.fetch_add(1, Ordering::AcqRel) + 1;
            let now = tick_count64();
            let should_warn = {
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if g.evict_warn_at == i64::MIN || now - g.evict_warn_at >= 600_000 {
                    g.evict_warn_at = now;
                    true
                } else {
                    false
                }
            };
            if should_warn {
                AgentLog::warn(&format!(
                    "transcribe {}: the worker is behind, {} segments wait and the oldest was dropped ({n} so far)",
                    self.source,
                    Self::QUEUE_SEGMENTS
                ));
            }
            return Some((old, orig_len));
        }
        None
    }

    /// Waits up to `timeout` for a segment; returns `Ok(Some(seg))`, `Ok(None)` on timeout,
    /// or `Err(QueueClosed)` when the queue is closed and empty.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<SpeechSegment>, QueueClosed> {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seg) = q.items.pop_front() {
            return Ok(Some(seg));
        }
        if q.closed {
            return Err(QueueClosed);
        }
        if timeout.is_zero() {
            return Ok(None);
        }
        let (mut q2, _) = self
            .queue_cv
            .wait_timeout(q, timeout)
            .unwrap_or_else(|e| e.into_inner());
        if let Some(seg) = q2.items.pop_front() {
            Ok(Some(seg))
        } else if q2.closed {
            Err(QueueClosed)
        } else {
            Ok(None)
        }
    }

    pub fn try_recv(&self) -> Option<SpeechSegment> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .pop_front()
    }

    pub fn flush(&self) {
        let mut produced = Vec::new();
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            self.flush_locked(&mut g, &mut produced);
        }
        for seg in produced {
            self.enqueue_segment(seg);
        }
    }

    fn flush_locked(&self, g: &mut TranscriptInner, produced: &mut Vec<SpeechSegment>) {
        if !self.has_buffered.load(Ordering::Acquire) {
            return;
        }
        g.out.clear();
        g.segmenter.flush(&mut g.out);
        g.segmenter.clear();
        g.segmenter.reanchor();
        Pcm::wipe(&mut g.buf);
        if let Some(ref mut r) = g.resampler {
            r.reset();
        }
        produced.append(&mut g.out);
        self.has_buffered.store(false, Ordering::Release);
    }

    /// Panic or purge reaching replay window: drops buffered & queued audio and cancels waits.
    pub fn clear(&self) {
        let old = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            self.clear_gen.fetch_add(1, Ordering::AcqRel);
            std::mem::replace(&mut g.clear_cts, CancelSource::new())
        };
        old.cancel();
        self.wipe();
    }

    fn wipe(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.segmenter.clear();
        Pcm::wipe(&mut g.buf);
        if let Some(ref mut r) = g.resampler {
            r.reset();
        }
        {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            while let Some(mut s) = q.items.pop_front() {
                s.zeroize_samples();
            }
        }
        self.has_buffered.store(false, Ordering::Release);
    }

    pub fn stop(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let (Some(hub), Some(id)) = (
            self.hub.as_ref(),
            self.hub_sub_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        ) {
            hub.unsubscribe(id);
        }
        if let Some(mic) = self.mic.lock().unwrap_or_else(|e| e.into_inner()).take() {
            mic.stop();
        }
        {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.closed = true;
            self.queue_cv.notify_all();
        }
        self.wipe();
    }
}

impl Drop for TranscriptSource {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Minimal store surface for [`Transcriber`] (implemented for [`EncryptedStore`] and fakes).
pub trait AudioStore: Send + Sync {
    fn add_audio_capture(&self, cap: &NewAudioCapture) -> Result<String, AudioStoreError>;
}

#[derive(Debug, Clone)]
pub enum AudioStoreError {
    PurgedWindow,
    Other(String),
}

impl std::fmt::Display for AudioStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PurgedWindow => write!(f, "purged window"),
            Self::Other(s) => write!(f, "{s}"),
        }
    }
}

impl AudioStore for EncryptedStore {
    fn add_audio_capture(&self, cap: &NewAudioCapture) -> Result<String, AudioStoreError> {
        self.add_audio_capture(cap).map_err(|e| match e {
            StoreError::PurgedWindow(_) => AudioStoreError::PurgedWindow,
            other => AudioStoreError::Other(other.to_string()),
        })
    }
}

/// Transcription backend (implemented for [`WorkerClient`] and test fakes).
pub trait TranscribeBackend: Send + Sync {
    fn transcribe(
        &self,
        pcm_16k_mono: &[f32],
        source: &str,
        language: &str,
        ct: &CancelToken,
    ) -> Result<TranscribeResult, WorkerError>;
}

impl TranscribeBackend for WorkerClient {
    fn transcribe(
        &self,
        pcm_16k_mono: &[f32],
        source: &str,
        language: &str,
        ct: &CancelToken,
    ) -> Result<TranscribeResult, WorkerError> {
        WorkerClient::transcribe(self, pcm_16k_mono, source, language, ct)
    }
}

struct HeldTranscript {
    ts: DateTime<Utc>,
    duration_ms: i32,
    raw: String,
    language: String,
    app: String,
    clear_gen: i32,
    end_qpc: i64,
    due_at: i64,
    held_at: i64,
    forced: Vec<(usize, usize)>,
}

#[derive(Default)]
struct HeldSlot {
    held: Mutex<Option<HeldTranscript>>,
}

impl HeldSlot {
    fn has_held(&self) -> bool {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn due_and_meta(&self) -> Option<(i64, i64, i64)> {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|h| (h.due_at, h.end_qpc, h.held_at))
    }

    fn set_due_at(&self, due_at: i64) {
        if let Some(ref mut h) = *self.held.lock().unwrap_or_else(|e| e.into_inner()) {
            h.due_at = due_at;
        }
    }

    fn take(&self) -> Option<HeldTranscript> {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn put(&self, h: HeldTranscript) {
        *self.held.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }
}

type AudioStoreProvider = Arc<dyn Fn() -> Option<Arc<dyn AudioStore>> + Send + Sync>;
type RetryDelayFn = Arc<dyn Fn(i32) -> Duration + Send + Sync>;
type HeldSlotMap = HashMap<usize, (Arc<TranscriptSource>, Arc<HeldSlot>)>;
/// Character-index span pair `(first_spans, second_spans)` returned by [`Transcriber::boundary_spans`].
pub type BoundarySpanPair = (Vec<(usize, usize)>, Vec<(usize, usize)>);

/// Sends speech segments to the worker and stores redacted transcripts (`Transcriber`).
pub struct Transcriber {
    worker: Arc<dyn TranscribeBackend>,
    store: AudioStoreProvider,
    settings: SettingsProvider,
    stored: Arc<dyn Fn() + Send + Sync>,
    retry_delay: RetryDelayFn,
    guard_wait: Duration,
    hold_for_ms: i64,
    slots: Mutex<HeldSlotMap>,
    retried: AtomicI64,
    gave_up: AtomicI64,
    blocked: AtomicI64,
}

impl Transcriber {
    pub const MAX_ATTEMPTS: i32 = 7;
    pub const HOLD_MS: i64 = 4000;
    pub const JOIN_GAP_100NS: i64 = 30_000_000;
    pub const MAX_HOLD_MS: i64 = 120_000;

    pub fn new(
        worker: Arc<dyn TranscribeBackend>,
        store: AudioStoreProvider,
        settings: SettingsProvider,
        stored: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            worker,
            store,
            settings,
            stored,
            retry_delay: Arc::new(|n| {
                let shift = (n - 1).clamp(0, 5) as u32;
                Duration::from_secs((2u64 << shift).min(60))
            }),
            guard_wait: Duration::from_secs(2),
            hold_for_ms: Self::HOLD_MS,
            slots: Mutex::new(HashMap::new()),
            retried: AtomicI64::new(0),
            gave_up: AtomicI64::new(0),
            blocked: AtomicI64::new(0),
        }
    }

    pub fn with_timing(
        mut self,
        retry_delay: RetryDelayFn,
        guard_wait: Duration,
        hold_for_ms: i64,
    ) -> Self {
        self.retry_delay = retry_delay;
        self.guard_wait = guard_wait;
        self.hold_for_ms = hold_for_ms;
        self
    }

    pub fn retried(&self) -> i64 {
        self.retried.load(Ordering::Acquire)
    }

    pub fn gave_up(&self) -> i64 {
        self.gave_up.load(Ordering::Acquire)
    }

    pub fn blocked(&self) -> i64 {
        self.blocked.load(Ordering::Acquire)
    }

    /// Stores every held transcript now (session end, shutdown).
    pub fn flush_held(&self) {
        let pairs: Vec<(Arc<TranscriptSource>, Arc<HeldSlot>)> = self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for (src, slot) in pairs {
            if let Some(h) = slot.take() {
                self.store_held(&src, h);
            }
        }
    }

    /// Blocking consumer loop for one [`TranscriptSource`].
    pub fn run(&self, src: Arc<TranscriptSource>, ct: &CancelToken) {
        let key = Arc::as_ptr(&src) as usize;
        let slot = {
            let mut map = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(key)
                .or_insert_with(|| (src.clone(), Arc::new(HeldSlot::default())))
                .1
                .clone()
        };

        loop {
            if ct.is_cancelled() {
                if let Some(h) = slot.take() {
                    self.store_held(&src, h);
                }
                break;
            }
            let wait = if let Some((due_at, end_qpc, held_at)) = slot.due_and_meta() {
                let rem = due_at - tick_count64();
                if rem <= 0 {
                    let now = tick_count64();
                    if let Some(open) = src.open_segment_start()
                        && open - end_qpc <= Self::JOIN_GAP_100NS
                        && now - held_at < Self::MAX_HOLD_MS
                    {
                        slot.set_due_at(now + 500);
                        Duration::from_millis(100)
                    } else {
                        if let Some(h) = slot.take() {
                            self.store_held(&src, h);
                        }
                        continue;
                    }
                } else {
                    Duration::from_millis((rem as u64).min(100))
                }
            } else {
                Duration::from_millis(100)
            };

            match src.recv_timeout(wait) {
                Ok(Some(seg)) => {
                    self.process_segment(&src, seg, &slot, ct);
                }
                Ok(None) => continue,
                Err(QueueClosed) => {
                    if let Some(last) = slot.take() {
                        self.store_held(&src, last);
                    }
                    break;
                }
            }
        }

        let _ = slot.has_held();
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }

    fn process_segment(
        &self,
        src: &TranscriptSource,
        mut seg: SpeechSegment,
        slot: &HeldSlot,
        ct: &CancelToken,
    ) {
        let (clear_gen, cleared) = src.generation();
        let combined = Arc::new(CancelSource::new());
        let c1 = combined.clone();
        let _g1 = ct.on_cancel(move || c1.cancel());
        let c2 = combined.clone();
        let _g2 = cleared.on_cancel(move || c2.cancel());
        let tok = combined.token();

        'handle: {
            let verdict = self.wait_guard(src, &seg, &tok);
            if tok.is_cancelled() {
                if cleared.is_cancelled() && !ct.is_cancelled() {
                    AgentLog::debug(&format!(
                        "transcribe {}: segment cleared (panic or purge) while it waited",
                        src.source()
                    ));
                }
                break 'handle;
            }
            if verdict.gate != AudioGate::Allowed {
                self.blocked.fetch_add(1, Ordering::AcqRel);
                AgentLog::debug(&format!(
                    "transcribe {}: segment spoken while the guard kept audio out ({:?}), dropped",
                    src.source(),
                    verdict.gate
                ));
                break 'handle;
            }
            let res = match self.transcribe_with_retry(src, &seg, clear_gen, &tok) {
                Ok(Some(r)) => r,
                Ok(None) => break 'handle,
                Err(e) => {
                    if cleared.is_cancelled() && !ct.is_cancelled() {
                        AgentLog::debug(&format!(
                            "transcribe {}: segment cleared (panic or purge) while it waited",
                            src.source()
                        ));
                    } else if !ct.is_cancelled() {
                        AgentLog::warn_err(&format!("transcribe {}", src.source()), &e);
                    }
                    break 'handle;
                }
            };
            if !res.speech || res.text.trim().is_empty() {
                break 'handle;
            }
            let end = seg.start_qpc_100ns + (seg.duration_ms as i64) * 10_000;
            let now_ms = tick_count64();
            let mut cur = HeldTranscript {
                ts: Self::qpc_to_wall(seg.start_qpc_100ns),
                duration_ms: seg.duration_ms,
                raw: res.text.trim().to_string(),
                language: res.language,
                app: verdict.app,
                clear_gen,
                end_qpc: end,
                due_at: now_ms + self.hold_for_ms,
                held_at: now_ms,
                forced: Vec::new(),
            };
            if let Some(mut held) = slot.take() {
                if held.clear_gen == clear_gen
                    && seg.start_qpc_100ns - held.end_qpc <= Self::JOIN_GAP_100NS
                {
                    let opts = RedactionOptions::from_privacy(&(self.settings)().privacy);
                    let (a, b) = Self::boundary_spans(&held.raw, &cur.raw, opts);
                    held.forced.extend(a);
                    cur.forced.extend(b);
                }
                self.store_held(src, held);
            }
            slot.put(cur);
        }
        seg.zeroize_samples();
    }

    fn wait_guard(
        &self,
        src: &TranscriptSource,
        seg: &SpeechSegment,
        ct: &CancelToken,
    ) -> AudioVerdict {
        let mut v = src.guard_for(seg);
        let until = tick_count64() + self.guard_wait.as_millis() as i64;
        while v.gate == AudioGate::Unknown && tick_count64() < until {
            if ct.wait_timeout(Duration::from_millis(100)) {
                break;
            }
            v = src.guard_for(seg);
        }
        v
    }

    fn transcribe_with_retry(
        &self,
        src: &TranscriptSource,
        seg: &SpeechSegment,
        clear_gen: i32,
        ct: &CancelToken,
    ) -> Result<Option<TranscribeResult>, String> {
        let mut attempt = 1;
        loop {
            let lang = (self.settings)().audio.language;
            match self.worker.transcribe(&seg.samples, src.source(), &lang, ct) {
                Ok(r) => return Ok(Some(r)),
                Err(WorkerError::Reply(msg)) => return Err(msg),
                Err(WorkerError::Cancelled) if ct.is_cancelled() => return Ok(None),
                Err(e) if attempt < Self::MAX_ATTEMPTS => {
                    self.retried.fetch_add(1, Ordering::AcqRel);
                    let delay = (self.retry_delay)(attempt);
                    AgentLog::debug(&format!(
                        "transcribe {}: {} Segment kept, try {} in {:.1} s",
                        src.source(),
                        e.message(),
                        attempt + 1,
                        delay.as_secs_f64()
                    ));
                    if ct.wait_timeout(delay) {
                        return Ok(None);
                    }
                    if src.clear_gen() != clear_gen {
                        AgentLog::debug(&format!(
                            "transcribe {}: segment cleared while it waited for the worker",
                            src.source()
                        ));
                        return Ok(None);
                    }
                    attempt += 1;
                }
                Err(e) => {
                    self.gave_up.fetch_add(1, Ordering::AcqRel);
                    return Err(format!(
                        "segment dropped after {} tries: {}",
                        Self::MAX_ATTEMPTS,
                        e.message()
                    ));
                }
            }
        }
    }

    fn store_held(&self, src: &TranscriptSource, h: HeldTranscript) {
        if src.clear_gen() != h.clear_gen {
            AgentLog::debug(&format!("transcribe {}: held transcript cleared", src.source()));
            return;
        }
        let Some(store) = (self.store)() else {
            return;
        };
        let opts = RedactionOptions::from_privacy(&(self.settings)().privacy);
        let text = Self::redact(&h.raw, opts, Some(&h.forced));
        if text.is_empty() {
            return;
        }
        let cap = NewAudioCapture {
            ts: h.ts,
            source: src.source().to_string(),
            duration_ms: h.duration_ms,
            text,
            language: h.language,
            app: h.app,
        };
        match store.add_audio_capture(&cap) {
            Ok(_) => (self.stored)(),
            Err(AudioStoreError::PurgedWindow) => {
                AgentLog::debug(&format!(
                    "transcribe {}: segment inside a purged window dropped",
                    src.source()
                ));
            }
            Err(e) => {
                AgentLog::warn_err(&format!("transcribe {}: store", src.source()), &e.to_string());
            }
        }
    }

    /// Transcript text with the text detectors' spans and `forced` spans left out. Pure.
    pub fn redact(raw: &str, o: RedactionOptions, forced: Option<&[(usize, usize)]>) -> String {
        let mut spans = Redactor::transcript_spans(raw, o);
        if let Some(extra) = forced
            && !extra.is_empty()
        {
            spans.extend_from_slice(extra);
            spans = Redactor::merge(&spans, raw.chars().count());
        }
        Redactor::remove(raw, &spans)
    }

    /// Spans of two neighbouring transcripts found in their joined text, split back onto each (A-M7). Pure.
    pub fn boundary_spans(
        first: &str,
        second: &str,
        o: RedactionOptions,
    ) -> BoundarySpanPair {
        let mut a = Vec::new();
        let mut b = Vec::new();
        let first_len = first.chars().count();
        let cut = first_len + 1;
        let joined = format!("{first} {second}");
        for (s, e) in Redactor::transcript_spans(&joined, o) {
            if s < first_len {
                a.push((s, e.min(first_len)));
            }
            if e > cut {
                b.push((s.saturating_sub(cut), e - cut));
            }
        }
        (a, b)
    }

    /// Maps a QPC time (100 ns) to wall clock using the current offset.
    pub fn qpc_to_wall(qpc_100ns: i64) -> DateTime<Utc> {
        let now = OutputCapture::qpc_100ns(qpc());
        let delta_ns = (now - qpc_100ns).saturating_mul(100);
        Utc::now() - ChronoDuration::nanoseconds(delta_ns)
    }
}
