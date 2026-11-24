// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Keyframe processing pipeline: dedup, OCR, redaction, replay sample hook, CPU-governed encode,
//! and encrypted store persistence (`Capture\KeyframePipeline.cs`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use br_contracts::{
    CaptureKind, CaptureMeta, NewScreenCapture, OcrResult, Settings,
};
use br_core::{CancelSource, CancelToken, EncryptedStore, StoreError};
use chrono::{DateTime, Utc};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadInformation, SetThreadPriority, THREAD_POWER_THROTTLING_CURRENT_VERSION,
    THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE,
    THREAD_PRIORITY_BELOW_NORMAL, ThreadPowerThrottling,
};
use zeroize::Zeroize;

use crate::capture::change_map::{ChangeMap, ChangeResult, KeepReason};
use crate::capture::dhash::{DHash, DedupFilter};
use crate::capture::encode_governor::{EncodeGovernor, EncodeLevel};
use crate::capture::image_codec::ImageCodec;
use crate::capture::keyframe_encoder::KeyframeEncoder;
use crate::capture::output_capture::KeyframeGrab;
use crate::guardrails::GuardrailPolicy;
use crate::native::{current_thread_cpu_ms, tick_count64};
use crate::privacy::{RedactionMasks, RedactionOptions, Redactor};
use crate::seams::KeyframeSubsystem;
use crate::util::AgentLog;
use crate::worker::{WorkerClient, WorkerError};

/// Reads text of a BGRA keyframe (worker's `ocr` op). Abstracted for tests (`IKeyframeReader`).
pub trait KeyframeReader: Send + Sync {
    fn read(
        &self,
        bgra: &[u8],
        width: i32,
        height: i32,
        stride: i32,
        entities: bool,
        ct: &CancelToken,
    ) -> Result<OcrResult, WorkerError>;

    /// True when the reader is already warm and `ready()` will return without blocking.
    fn is_ready(&self) -> bool {
        true
    }

    /// Starts the reader if not running and returns how many requests are queued ahead of it.
    fn ready(&self, _ct: &CancelToken) -> Result<i32, WorkerError> {
        Ok(0)
    }
}

/// Real [`KeyframeReader`] backed by [`WorkerClient`].
pub struct WorkerKeyframeReader {
    worker: Arc<WorkerClient>,
}

impl WorkerKeyframeReader {
    pub fn new(worker: Arc<WorkerClient>) -> Self {
        Self { worker }
    }
}

impl KeyframeReader for WorkerKeyframeReader {
    fn read(
        &self,
        bgra: &[u8],
        width: i32,
        height: i32,
        stride: i32,
        entities: bool,
        ct: &CancelToken,
    ) -> Result<OcrResult, WorkerError> {
        self.worker.ocr(bgra, width, height, stride, entities, ct)
    }

    fn is_ready(&self) -> bool {
        self.worker.supervisor().is_running()
    }

    fn ready(&self, ct: &CancelToken) -> Result<i32, WorkerError> {
        let conn = self.worker.supervisor().ensure(ct)?;
        Ok(conn.pending() as i32)
    }
}

/// Minimal store surface for [`KeyframePipeline`] (implemented for [`EncryptedStore`] and fakes).
pub trait KeyframeStore: Send + Sync {
    fn add_screen_capture(&self, cap: &NewScreenCapture) -> Result<String, KeyframeStoreError>;
}

#[derive(Debug, Clone)]
pub enum KeyframeStoreError {
    PurgedWindow,
    Other(String),
}

impl std::fmt::Display for KeyframeStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PurgedWindow => write!(f, "purged window"),
            Self::Other(s) => write!(f, "{s}"),
        }
    }
}

impl KeyframeStore for EncryptedStore {
    fn add_screen_capture(&self, cap: &NewScreenCapture) -> Result<String, KeyframeStoreError> {
        self.add_screen_capture(cap).map_err(|e| match e {
            StoreError::PurgedWindow(_) => KeyframeStoreError::PurgedWindow,
            other => KeyframeStoreError::Other(other.to_string()),
        })
    }
}

/// Redacted replay sample handed from [`KeyframePipeline`] to A6's `ReplaySampler` seam.
#[derive(Debug, Clone)]
pub struct ReplaySample {
    pub ts: DateTime<Utc>,
    pub monitor: i32,
    pub app: String,
    pub title: String,
    pub site: String,
    pub width: i32,
    pub height: i32,
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// Sink for redacted replay samples (real implementation lands in stage A6).
pub trait ReplaySampleSink: Send + Sync {
    fn add(&self, sample: ReplaySample, keep_seconds: i32);
}

/// Health of keyframe OCR (A21, C9): consecutive passing failures and NER availability.
pub struct OcrHealth {
    now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    streak: AtomicI64,
    entities_missing: AtomicBool,
    log_gate: Mutex<(i64, i64)>, // (last_fail_log, last_ner_log)
}

impl Default for OcrHealth {
    fn default() -> Self {
        Self::new(Arc::new(tick_count64))
    }
}

impl OcrHealth {
    pub const FAILING_AFTER: i64 = 5;
    const LOG_EVERY_MS: i64 = 600_000;

    pub fn new(now_ms: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            now_ms,
            streak: AtomicI64::new(0),
            entities_missing: AtomicBool::new(false),
            log_gate: Mutex::new((i64::MIN, i64::MIN)),
        }
    }

    pub fn failing(&self) -> bool {
        self.streak.load(Ordering::Acquire) >= Self::FAILING_AFTER
    }

    pub fn entities_missing(&self) -> bool {
        self.entities_missing.load(Ordering::Acquire)
    }

    /// Records a successful read. Returns `true` when the NER-unavailable warning is due.
    pub fn read(&self, entities_asked: bool, entities_missing: bool) -> bool {
        self.streak.store(0, Ordering::Release);
        let missing = entities_asked && entities_missing;
        self.entities_missing.store(missing, Ordering::Release);
        missing && self.due_ner()
    }

    /// Records a transient failure. Returns `true` when the failing warning is due.
    pub fn failed(&self) -> bool {
        let s = self.streak.fetch_add(1, Ordering::AcqRel) + 1;
        s >= Self::FAILING_AFTER && self.due_fail()
    }

    pub fn reset(&self) {
        self.streak.store(0, Ordering::Release);
        self.entities_missing.store(false, Ordering::Release);
    }

    fn due_fail(&self) -> bool {
        let now = (self.now_ms)();
        let mut g = self.log_gate.lock().unwrap_or_else(|e| e.into_inner());
        if g.0 != i64::MIN && now - g.0 < Self::LOG_EVERY_MS {
            return false;
        }
        g.0 = now;
        true
    }

    fn due_ner(&self) -> bool {
        let now = (self.now_ms)();
        let mut g = self.log_gate.lock().unwrap_or_else(|e| e.into_inner());
        if g.1 != i64::MIN && now - g.1 < Self::LOG_EVERY_MS {
            return false;
        }
        g.1 = now;
        true
    }
}

static STAMP_SEQ: AtomicI64 = AtomicI64::new(0);
const MASK_CHECKED: i64 = i64::MIN;

struct MaskState {
    by_monitor: HashMap<i32, i64>,
    queued: HashMap<i32, (usize, i64)>,
    worker_starting: bool,
    all_unchecked_since: i64,
    no_text_masks: bool,
}

struct ChannelState {
    queue: VecDeque<KeyframeGrab>,
    closed: bool,
}

struct PendingEncode {
    grab: KeyframeGrab,
    hash: u64,
    change: ChangeResult,
    reason: KeepReason,
    title: String,
    text: Option<String>,
}

type StoreProvider = Arc<dyn Fn() -> Option<Arc<dyn KeyframeStore>> + Send + Sync>;
type SettingsProvider = Arc<dyn Fn() -> Settings + Send + Sync>;
type StoredCallback = Arc<dyn Fn(CaptureMeta) + Send + Sync>;
type OcrDroppedCallback = Arc<dyn Fn(i32) + Send + Sync>;

struct PipelineInner {
    store: StoreProvider,
    settings: SettingsProvider,
    stored: StoredCallback,
    reader: Option<Arc<dyn KeyframeReader>>,
    masks: Option<Arc<RedactionMasks>>,
    sampler: Mutex<Option<Arc<dyn ReplaySampleSink>>>,
    ocr_dropped: Mutex<Option<OcrDroppedCallback>>,
    channel: Mutex<ChannelState>,
    channel_cv: Condvar,
    dedup: Mutex<HashMap<i32, DedupFilter>>,
    mask_state: Mutex<MaskState>,
    any_unchecked: AtomicBool,
    any_unchecked_at_ms: AtomicI64,
    ocr_health: Arc<OcrHealth>,
    ocr_timeout: Duration,
    ocr_wait_ms: AtomicU64,
    encoder_name: Mutex<String>,
    flush_flag: AtomicBool,
    cts: CancelSource,
    // Counters
    kept: AtomicI64,
    skipped_similar: AtomicI64,
    dropped: AtomicI64,
    failed: AtomicI64,
    ocr_read: AtomicI64,
    ocr_failed: AtomicI64,
    ocr_unavailable: AtomicI64,
    redact_dropped: AtomicI64,
    redacted_spans: AtomicI64,
    sample_ocr_reads: AtomicI64,
    // Governor (locked for Offer/Pump/Budget/Clear)
    governor: Mutex<Option<EncodeGovernor<PendingEncode>>>,
    budget_deferred: AtomicU64,
    budget_dropped: AtomicU64,
    budget_webp: AtomicU64,
    last_encode_ms: AtomicU64,
    last_ocr_ms: AtomicU64,
    encode_ms_total: AtomicU64,
    encode_cpu_ms_total: AtomicU64,
    ocr_ms_total: AtomicU64,
    ocr_cpu_ms_total: AtomicU64,
    hash_ms_total: AtomicU64,
}

struct StoreKeyframeArgs<'a> {
    store: &'a dyn KeyframeStore,
    s: &'a Settings,
    g: &'a KeyframeGrab,
    hash: u64,
    change: ChangeResult,
    reason: KeepReason,
    title: String,
    text: Option<String>,
    bytes: Vec<u8>,
    format: &'a str,
    w: i32,
    h: i32,
}

impl PipelineInner {
    fn recompute_masks_locked(&self, ms: &mut MaskState) {
        let any = !ms.no_text_masks
            && (ms.by_monitor.is_empty() || ms.by_monitor.values().any(|&v| v != MASK_CHECKED));
        if any && !self.any_unchecked.load(Ordering::Acquire) {
            self.any_unchecked_at_ms
                .store(tick_count64(), Ordering::Release);
        }
        self.any_unchecked.store(any, Ordering::Release);
    }

    fn mark_unchecked_locked(&self, ms: &mut MaskState, monitor: i32, grab_stamp: i64) {
        let next = match ms.by_monitor.get(&monitor).copied() {
            Some(v) if v != MASK_CHECKED => v.max(grab_stamp),
            _ => grab_stamp,
        };
        ms.by_monitor.insert(monitor, next);
        self.recompute_masks_locked(ms);
    }

    fn mark_unchecked(&self, monitor: i32, grab_stamp: i64) {
        let mut ms = self.mask_state.lock().unwrap_or_else(|e| e.into_inner());
        self.mark_unchecked_locked(&mut ms, monitor, grab_stamp);
    }

    fn mark_checked(&self, monitor: i32, grab_stamp: i64) {
        let mut ms = self.mask_state.lock().unwrap_or_else(|e| e.into_inner());
        let since = ms
            .by_monitor
            .get(&monitor)
            .copied()
            .unwrap_or(ms.all_unchecked_since);
        if since == MASK_CHECKED || grab_stamp >= since {
            ms.by_monitor.insert(monitor, MASK_CHECKED);
        }
        self.recompute_masks_locked(&mut ms);
    }

    fn set_no_text_masks(&self, on: bool) {
        let mut ms = self.mask_state.lock().unwrap_or_else(|e| e.into_inner());
        if ms.no_text_masks == on {
            return;
        }
        ms.no_text_masks = on;
        self.recompute_masks_locked(&mut ms);
    }

    fn monitor_unchecked(&self, monitor: i32) -> bool {
        let ms = self.mask_state.lock().unwrap_or_else(|e| e.into_inner());
        if ms.no_text_masks {
            return false;
        }
        ms.by_monitor
            .get(&monitor)
            .is_none_or(|&v| v != MASK_CHECKED)
    }

    fn store_keyframe(&self, args: StoreKeyframeArgs<'_>) -> Result<(), KeyframeStoreError> {
        let StoreKeyframeArgs {
            store,
            s,
            g,
            hash,
            change,
            reason,
            title,
            text,
            mut bytes,
            format,
            w,
            h,
        } = args;
        let req = &g.request;
        let cap = NewScreenCapture {
            ts: req.ts,
            app: req.app.clone(),
            exe_path: req.exe_path.clone(),
            title: title.clone(),
            monitor: g.monitor,
            width: w,
            height: h,
            d_hash: hash,
            image: std::mem::take(&mut bytes),
            image_mime: ImageCodec::mime(format).to_string(),
            keep_keyframe: s.keyframes.enabled,
            site: req.site.clone(),
            change: if change.comparable {
                Some((change.score * 100.0).round() / 100.0)
            } else {
                None
            },
            switch: matches!(reason, KeepReason::First | KeepReason::Switch),
            text,
        };
        let mut cap = cap;
        let res = store.add_screen_capture(&cap);
        cap.image.zeroize();
        let id = res?;
        self.kept.fetch_add(1, Ordering::AcqRel);
        (self.stored)(CaptureMeta {
            id,
            ts: req.ts,
            kind: CaptureKind::Screen,
            app: req.app.clone(),
            title,
            monitor: g.monitor,
            duration_ms: 0,
            has_thumb: s.keyframes.enabled,
            embedded: false,
            site: req.site.clone(),
            replay_id: None,
            replay_offset_ms: None,
        });
        Ok(())
    }
}

/// Keyframe processing pipeline (`KeyframePipeline.cs`).
pub struct KeyframePipeline {
    inner: Arc<PipelineInner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl KeyframePipeline {
    pub const OCR_TIMEOUT: Duration = Duration::from_secs(30);
    pub const MAX_OCR_TIMEOUT_FACTOR: u32 = 4;
    pub const CHANNEL_CAPACITY: usize = 6;

    pub fn next_stamp() -> i64 {
        STAMP_SEQ.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Adaptive OCR timeout rule. Pure.
    pub fn next_ocr_wait(
        baseline: Duration,
        current: Duration,
        took: Duration,
        timed_out: bool,
    ) -> Duration {
        let max = baseline * Self::MAX_OCR_TIMEOUT_FACTOR;
        let next = if timed_out { current * 2 } else { took * 2 };
        next.clamp(baseline, max)
    }

    pub fn new(
        store: StoreProvider,
        settings: SettingsProvider,
        stored: StoredCallback,
        reader: Option<Arc<dyn KeyframeReader>>,
        masks: Option<Arc<RedactionMasks>>,
        ocr_timeout: Option<Duration>,
    ) -> Arc<Self> {
        let timeout = ocr_timeout.unwrap_or(Self::OCR_TIMEOUT);
        let inner = Arc::new(PipelineInner {
            store,
            settings,
            stored,
            reader,
            masks,
            sampler: Mutex::new(None),
            ocr_dropped: Mutex::new(None),
            channel: Mutex::new(ChannelState {
                queue: VecDeque::with_capacity(Self::CHANNEL_CAPACITY),
                closed: false,
            }),
            channel_cv: Condvar::new(),
            dedup: Mutex::new(HashMap::new()),
            mask_state: Mutex::new(MaskState {
                by_monitor: HashMap::new(),
                queued: HashMap::new(),
                worker_starting: false,
                all_unchecked_since: 0,
                no_text_masks: false,
            }),
            any_unchecked: AtomicBool::new(true),
            any_unchecked_at_ms: AtomicI64::new(tick_count64()),
            ocr_health: Arc::new(OcrHealth::default()),
            ocr_timeout: timeout,
            ocr_wait_ms: AtomicU64::new(timeout.as_millis() as u64),
            encoder_name: Mutex::new(String::new()),
            flush_flag: AtomicBool::new(false),
            cts: CancelSource::new(),
            kept: AtomicI64::new(0),
            skipped_similar: AtomicI64::new(0),
            dropped: AtomicI64::new(0),
            failed: AtomicI64::new(0),
            ocr_read: AtomicI64::new(0),
            ocr_failed: AtomicI64::new(0),
            ocr_unavailable: AtomicI64::new(0),
            redact_dropped: AtomicI64::new(0),
            redacted_spans: AtomicI64::new(0),
            sample_ocr_reads: AtomicI64::new(0),
            governor: Mutex::new(None),
            budget_deferred: AtomicU64::new(0),
            budget_dropped: AtomicU64::new(0),
            budget_webp: AtomicU64::new(0),
            last_encode_ms: AtomicU64::new(0),
            last_ocr_ms: AtomicU64::new(0),
            encode_ms_total: AtomicU64::new(0),
            encode_cpu_ms_total: AtomicU64::new(0),
            ocr_ms_total: AtomicU64::new(0),
            ocr_cpu_ms_total: AtomicU64::new(0),
            hash_ms_total: AtomicU64::new(0),
        });

        let inner_for_enc = inner.clone();
        let procs = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4);
        let gov = EncodeGovernor::new(
            tick_count64,
            procs,
            move |pending: &mut PendingEncode, level: EncodeLevel| {
                encode_and_store(&inner_for_enc, pending, level)
            },
            |_pending, why| {
                AgentLog::warn(&format!("keyframe dropped by the CPU budget ({why})"));
            },
            KeyframeEncoder::INITIAL_ESTIMATE_MS,
        );
        *inner.governor.lock().unwrap_or_else(|e| e.into_inner()) = Some(gov);

        let inner_bg = inner.clone();
        let handle = thread::Builder::new()
            .name("keyframes".to_string())
            .spawn(move || run_pipeline(inner_bg))
            .ok();

        Arc::new(Self {
            inner,
            thread: Mutex::new(handle),
        })
    }

    pub fn set_sampler(&self, sampler: Option<Arc<dyn ReplaySampleSink>>) {
        *self
            .inner
            .sampler
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = sampler;
    }

    pub fn set_ocr_dropped(&self, cb: Option<OcrDroppedCallback>) {
        *self
            .inner
            .ocr_dropped
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = cb;
    }

    pub fn post(&self, grab: KeyframeGrab) {
        // D4: bench requests skip guardrails and password masks; refuse them at the pipeline entry.
        if grab.request.bench {
            AgentLog::warn("keyframe pipeline: refused bench keyframe grab");
            return;
        }
        let monitor = grab.monitor;
        let grab_ticks = grab.grab_ticks;
        let queued = {
            let mut ch = self.inner.channel.lock().unwrap_or_else(|e| e.into_inner());
            let mut ms = self
                .inner
                .mask_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !ch.closed && ch.queue.len() < Self::CHANNEL_CAPACITY {
                ch.queue.push_back(grab);
                let entry = ms.queued.entry(monitor).or_insert((0, 0));
                entry.0 += 1;
                entry.1 = entry.1.max(grab_ticks);
                if ms.worker_starting {
                    self.inner.mark_unchecked_locked(&mut ms, monitor, grab_ticks);
                }
                self.inner.channel_cv.notify_one();
                true
            } else {
                self.inner.mark_unchecked_locked(&mut ms, monitor, grab_ticks);
                drop(grab);
                false
            }
        };
        if !queued {
            self.inner.dropped.fetch_add(1, Ordering::AcqRel);
            if let Some(cb) = self
                .inner
                .ocr_dropped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                cb(monitor);
            }
        }
    }

    pub fn track_monitors(&self, monitors: &[i32]) {
        let mut ms = self
            .inner
            .mask_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ms.by_monitor.clear();
        let since = ms.all_unchecked_since;
        for &m in monitors {
            ms.by_monitor.insert(m, since);
        }
        self.inner.recompute_masks_locked(&mut ms);
    }

    pub fn mark_all_unchecked(&self) {
        let stamp = Self::next_stamp();
        let mut ms = self
            .inner
            .mask_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ms.all_unchecked_since = stamp;
        for v in ms.by_monitor.values_mut() {
            *v = stamp;
        }
        self.inner.recompute_masks_locked(&mut ms);
    }

    pub fn masks_unchecked(&self) -> bool {
        self.inner.any_unchecked.load(Ordering::Acquire)
    }

    pub fn unchecked_for_ms(&self) -> i64 {
        if self.inner.any_unchecked.load(Ordering::Acquire) {
            tick_count64() - self.inner.any_unchecked_at_ms.load(Ordering::Acquire)
        } else {
            0
        }
    }

    /// Test helper: backdate the unchecked start timestamp to verify `WarnAfterMs`.
    pub fn set_any_unchecked_at_ms_for_test(&self, ms: i64) {
        self.inner.any_unchecked_at_ms.store(ms, Ordering::Release);
    }

    pub fn monitor_unchecked(&self, monitor: i32) -> bool {
        self.inner.monitor_unchecked(monitor)
    }

    pub fn reset_dedup(&self) {
        let mut map = self.inner.dedup.lock().unwrap_or_else(|e| e.into_inner());
        for d in map.values_mut() {
            d.reset();
        }
    }

    pub fn flush(&self) {
        self.inner.flush_flag.store(true, Ordering::Release);
        self.inner.channel_cv.notify_one();
    }

    pub fn context_changed(&self, monitor: i32, context: &str) -> bool {
        self.inner
            .dedup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&monitor)
            .is_some_and(|d| d.context_differs(context))
    }

    pub fn ocr_wait(&self) -> Duration {
        Duration::from_millis(self.inner.ocr_wait_ms.load(Ordering::Acquire))
    }

    pub fn ocr_health(&self) -> &Arc<OcrHealth> {
        &self.inner.ocr_health
    }

    pub fn kept(&self) -> i64 {
        self.inner.kept.load(Ordering::Acquire)
    }

    pub fn skipped_similar(&self) -> i64 {
        self.inner.skipped_similar.load(Ordering::Acquire)
    }

    pub fn dropped(&self) -> i64 {
        self.inner.dropped.load(Ordering::Acquire)
    }

    pub fn failed(&self) -> i64 {
        self.inner.failed.load(Ordering::Acquire)
    }

    pub fn ocr_read(&self) -> i64 {
        self.inner.ocr_read.load(Ordering::Acquire)
    }

    pub fn ocr_failed(&self) -> i64 {
        self.inner.ocr_failed.load(Ordering::Acquire)
    }

    pub fn ocr_unavailable(&self) -> i64 {
        self.inner.ocr_unavailable.load(Ordering::Acquire)
    }

    pub fn redact_dropped(&self) -> i64 {
        self.inner.redact_dropped.load(Ordering::Acquire)
    }

    pub fn redacted_spans(&self) -> i64 {
        self.inner.redacted_spans.load(Ordering::Acquire)
    }

    pub fn sample_ocr_reads(&self) -> i64 {
        self.inner.sample_ocr_reads.load(Ordering::Acquire)
    }

    pub fn encoder(&self) -> String {
        self.inner
            .encoder_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn budget(&self) -> (f64, f64, usize, EncodeLevel) {
        let mut g = self
            .inner
            .governor
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(ref mut gov) = *g else {
            return (0.0, 0.0, 0, EncodeLevel::Normal);
        };
        let (u10, u60) = gov.used();
        let procs = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4) as f64;
        (
            u10 / (procs * 10_000.0) * 100.0,
            u60 / (procs * 60_000.0) * 100.0,
            gov.waiting(),
            gov.level(),
        )
    }

    pub fn stop(&self) {
        {
            let mut ch = self.inner.channel.lock().unwrap_or_else(|e| e.into_inner());
            ch.closed = true;
            self.inner.channel_cv.notify_all();
        }
        self.inner.cts.cancel();
        if let Some(h) = self
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        {
            let mut ch = self.inner.channel.lock().unwrap_or_else(|e| e.into_inner());
            ch.queue.clear();
        }
        {
            let mut ms = self
                .inner
                .mask_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            ms.queued.clear();
        }
        if let Some(ref mut gov) = *self
            .inner
            .governor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            gov.clear();
        }
    }
}

impl KeyframeSubsystem for KeyframePipeline {
    fn mark_all_unchecked(&self) {
        KeyframePipeline::mark_all_unchecked(self);
    }

    fn ocr_failing(&self) -> bool {
        self.inner.ocr_health.failing()
    }

    fn entities_missing(&self) -> bool {
        self.inner.ocr_health.entities_missing()
    }

    fn budget_status(&self) -> Option<String> {
        let (_, _, waiting, level) = self.budget();
        if level != EncodeLevel::Normal || waiting > 0 {
            Some(format!(
                "Capturing (keyframe budget: {:?}, waiting {waiting})",
                level
            ))
        } else {
            None
        }
    }

    fn diagnostics(&self) -> Option<String> {
        let (p10, p60, waiting, level) = self.budget();
        let kept = self.kept();
        let enc_total = f64::from_bits(self.inner.encode_ms_total.load(Ordering::Relaxed));
        let enc_cpu = f64::from_bits(self.inner.encode_cpu_ms_total.load(Ordering::Relaxed));
        let ocr_total = f64::from_bits(self.inner.ocr_ms_total.load(Ordering::Relaxed));
        let avg_enc = if kept > 0 { enc_total / kept as f64 } else { 0.0 };
        Some(format!(
            "keyframes: kept {kept}, skipped {}, dropped {}, failed {}, ocr read {}/failed {}/unavail {}, redact dropped {}, encoder '{}', encode avg {avg_enc:.1} ms (cpu {enc_cpu:.0} ms), ocr {ocr_total:.0} ms, budget 10s {p10:.2}% 60s {p60:.2}% waiting {waiting} level {level:?}\n",
            self.skipped_similar(),
            self.dropped(),
            self.failed(),
            self.ocr_read(),
            self.ocr_failed(),
            self.ocr_unavailable(),
            self.redact_dropped(),
            self.encoder()
        ))
    }

    fn stop(&self) {
        KeyframePipeline::stop(self);
    }
}

impl Drop for KeyframePipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn apply_thread_eco_qos() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        let state = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            (&raw const state).cast(),
            std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn add_f64_atomic(atom: &AtomicU64, delta: f64) {
    let _ = atom.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
        Some((f64::from_bits(bits) + delta).to_bits())
    });
}

fn run_pipeline(inner: Arc<PipelineInner>) {
    apply_thread_eco_qos();
    let ct = inner.cts.token();
    let mut last_missing_log = -1i64;

    while !ct.is_cancelled() {
        if inner.flush_flag.swap(false, Ordering::AcqRel) {
            flush_queued(&inner);
        }

        let maybe_grab = {
            let mut ch = inner.channel.lock().unwrap_or_else(|e| e.into_inner());
            let mut ms = inner.mask_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(g) = ch.queue.pop_front() {
                if let Some(entry) = ms.queued.get_mut(&g.monitor) {
                    if entry.0 <= 1 {
                        ms.queued.remove(&g.monitor);
                    } else {
                        entry.0 -= 1;
                    }
                }
                Some(g)
            } else {
                None
            }
        };

        if let Some(grab) = maybe_grab {
            process_grab(&inner, grab, &ct, &mut last_missing_log);
            continue;
        }

        let waiting = {
            let mut g = inner.governor.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut gov) = *g {
                gov.pump();
                inner
                    .budget_deferred
                    .store(gov.deferred, Ordering::Relaxed);
                inner.budget_dropped.store(gov.dropped, Ordering::Relaxed);
                inner
                    .budget_webp
                    .store(gov.webp_fallbacks, Ordering::Relaxed);
                gov.waiting()
            } else {
                0
            }
        };

        let mut ch = inner.channel.lock().unwrap_or_else(|e| e.into_inner());
        if !ch.queue.is_empty() || inner.flush_flag.load(Ordering::Acquire) {
            continue;
        }
        if ch.closed || ct.is_cancelled() {
            break;
        }
        if waiting > 0 {
            let (next, _) = inner
                .channel_cv
                .wait_timeout(ch, Duration::from_millis(500))
                .unwrap_or_else(|e| e.into_inner());
            ch = next;
        } else {
            ch = inner
                .channel_cv
                .wait(ch)
                .unwrap_or_else(|e| e.into_inner());
        }
        if ch.closed && ch.queue.is_empty() {
            break;
        }
    }
}

fn flush_queued(inner: &PipelineInner) {
    let n = {
        let mut ch = inner.channel.lock().unwrap_or_else(|e| e.into_inner());
        let len = ch.queue.len();
        ch.queue.clear();
        len
    };
    {
        let mut ms = inner.mask_state.lock().unwrap_or_else(|e| e.into_inner());
        ms.queued.clear();
    }
    if let Some(ref mut gov) = *inner.governor.lock().unwrap_or_else(|e| e.into_inner()) {
        gov.clear();
    }
    AgentLog::info(&format!("keyframes flushed ({n} queued grabs)"));
}

fn process_grab(
    inner: &PipelineInner,
    mut g: KeyframeGrab,
    ct: &CancelToken,
    last_missing_log: &mut i64,
) {
    // D4: defense in depth — never store a bench request.
    if g.request.bench {
        return;
    }
    let s = (inner.settings)();
    let Some(store) = (inner.store)() else {
        return;
    };
    let req = g.request.clone();
    let sampler = inner
        .sampler
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let sample = req.replay_sample && !req.mask_only && sampler.is_some() && s.replay.searchable;
    let mut keep = req.keyframe && !req.mask_only;

    let mut map: Option<ChangeMap> = None;
    let mut hash = 0u64;
    let mut change = ChangeResult::default();
    let mut reason = KeepReason::Skip;

    let sw = Instant::now();
    if keep {
        let h = match DHash::compute(g.pixels(), g.width, g.height, g.stride) {
            Ok(h) => h,
            Err(e) => {
                inner.failed.fetch_add(1, Ordering::AcqRel);
                AgentLog::warn_err("keyframe dhash", &e);
                return;
            }
        };
        let mut cm = match ChangeMap::compute(g.pixels(), g.width, g.height, g.stride) {
            Ok(m) => m,
            Err(e) => {
                inner.failed.fetch_add(1, Ordering::AcqRel);
                AgentLog::warn_err("keyframe changemap", &e);
                return;
            }
        };
        hash = h;
        let (r, c) = {
            let mut dmap = inner.dedup.lock().unwrap_or_else(|e| e.into_inner());
            let d = dmap.entry(g.monitor).or_default();
            d.decide(
                &cm,
                hash,
                &req,
                s.capture.dhash_threshold,
                s.capture.dirty_min_percent.clamp(0.0, 100.0),
            )
        };
        reason = r;
        change = c;
        add_f64_atomic(&inner.hash_ms_total, sw.elapsed().as_secs_f64() * 1000.0);
        if reason == KeepReason::Skip {
            cm.clear();
            inner.skipped_similar.fetch_add(1, Ordering::AcqRel);
            keep = false;
        } else {
            map = Some(cm);
        }
    }

    let mask_only = !keep
        && !sample
        && (req.replay_sample
            || req.mask_only
            || (inner.monitor_unchecked(g.monitor) && s.ocr.enabled && inner.reader.is_some()));
    if !keep && !sample && !mask_only {
        return;
    }

    let red = RedactionOptions::from_privacy(&s.privacy);
    let has_bounds = req.bounds.width() > 0 && req.bounds.height() > 0;
    let secrets = if red.passwords && has_bounds {
        req.secrets
            .as_deref()
            .filter(|sr| !sr.is_empty())
            .map(|sr| RedactionMasks::to_keyframe(sr, req.bounds, g.width, g.height))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut ocr: Option<OcrResult> = None;
    if s.ocr.enabled
        && let Some(ref reader) = inner.reader
    {
        let cpu0 = current_thread_cpu_ms();
        let ocr_sw = Instant::now();
        let (result, permanent, why) = read_ocr(inner, &**reader, &g, red.entities(), ct);
        let ocr_ms = ocr_sw.elapsed().as_secs_f64() * 1000.0;
        inner.last_ocr_ms.store(ocr_ms.to_bits(), Ordering::Relaxed);
        add_f64_atomic(&inner.ocr_ms_total, ocr_ms);
        add_f64_atomic(
            &inner.ocr_cpu_ms_total,
            (current_thread_cpu_ms() - cpu0).max(0.0),
        );
        if ct.is_cancelled() {
            if let Some(ref mut m) = map {
                m.clear();
            }
            return;
        }
        if let Some(res) = result {
            inner.set_no_text_masks(false);
            inner.mark_checked(g.monitor, g.grab_ticks);
            inner.ocr_read.fetch_add(1, Ordering::AcqRel);
            if !req.keyframe {
                inner.sample_ocr_reads.fetch_add(1, Ordering::AcqRel);
            }
            if inner.ocr_health.read(red.entities(), res.entities_missing) {
                AgentLog::warn(
                    "name and address detector unavailable (model not installed or failed to load): names and addresses are not redacted (pattern redaction still runs)",
                );
            }
            ocr = Some(res);
        } else if permanent {
            inner.ocr_health.reset();
            inner.set_no_text_masks(true);
            inner.ocr_unavailable.fetch_add(1, Ordering::AcqRel);
            let now = tick_count64();
            if *last_missing_log < 0 || now - *last_missing_log > 600_000 {
                *last_missing_log = now;
                AgentLog::warn(&format!(
                    "ocr unavailable ({why}): keyframes stored without text; text redaction off until it is installed"
                ));
            }
        } else {
            inner.set_no_text_masks(false);
            if red.any_text() {
                inner.mark_unchecked(g.monitor, g.grab_ticks);
            }
            if inner.ocr_health.failed() {
                AgentLog::warn(&format!(
                    "text recognition failing ({}+ reads in a row, last: {why}): {}",
                    OcrHealth::FAILING_AFTER,
                    if red.any_text() {
                        "keyframes are dropped while it fails"
                    } else {
                        "keyframes are stored without text"
                    }
                ));
            }
            if red.any_text() {
                if let Some(ref mut m) = map {
                    m.clear();
                }
                inner.redact_dropped.fetch_add(1, Ordering::AcqRel);
                AgentLog::debug(&format!("keyframe dropped: ocr failed ({why})"));
                if let Some(cb) = inner
                    .ocr_dropped
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                {
                    cb(g.monitor);
                }
                return;
            }
            inner.ocr_failed.fetch_add(1, Ordering::AcqRel);
        }
    }

    let title = GuardrailPolicy::filter_title(&req.title, &s.privacy);
    let eff_red = if ocr.is_none() {
        RedactionOptions {
            names: false,
            addresses: false,
            ..red
        }
    } else {
        red
    };
    let lines = ocr.as_ref().map(|o| o.lines.as_slice()).unwrap_or(&[]);
    let r = Redactor::apply(lines, &secrets, eff_red, &title);
    if !r.boxes.is_empty() {
        let w = g.width;
        let h = g.height;
        let st = g.stride;
        unsafe {
            RedactionMasks::fill(g.pixels_mut().as_mut_ptr(), w, h, st, &r.boxes);
        }
    }
    if has_bounds && let Some(ref masks) = inner.masks {
        masks.set(
            g.monitor,
            RedactionMasks::to_desktop(&r.boxes, g.width, g.height, req.bounds),
        );
    }
    inner
        .redacted_spans
        .fetch_add(r.spans as i64, Ordering::AcqRel);
    let text = if ocr.is_some() { Some(r.text) } else { None };

    let sample_format = if s.keyframes.format == "jpeg" {
        "jpeg"
    } else {
        "webp"
    };
    let mut sample_bytes: Option<Vec<u8>> = None;
    let sample_fits = if sample {
        let mut gov_g = inner.governor.lock().unwrap_or_else(|e| e.into_inner());
        gov_g
            .as_mut()
            .is_none_or(EncodeGovernor::try_admit_sample)
    } else {
        true
    };
    if sample
        && sample_fits
        && let Some(ref sink) = sampler
    {
        let c0 = current_thread_cpu_ms();
        let quality = s.keyframes.quality.clamp(30, 95);
        match unsafe {
            ImageCodec::encode_bgra(
                g.pixels().as_ptr(),
                g.width,
                g.height,
                g.stride,
                sample_format,
                quality,
            )
        } {
            Ok(bytes) => {
                let cpu = current_thread_cpu_ms() - c0;
                if let Some(ref mut gov) =
                    *inner.governor.lock().unwrap_or_else(|e| e.into_inner())
                {
                    gov.charge_sample(cpu);
                }
                sink.add(
                    ReplaySample {
                        ts: req.ts,
                        monitor: g.monitor,
                        app: req.app.clone(),
                        title: r.title.clone(),
                        site: req.site.clone(),
                        width: g.width,
                        height: g.height,
                        bytes: bytes.clone(),
                        mime: ImageCodec::mime(sample_format).to_string(),
                    },
                    s.replay.seconds.clamp(15, 600) + 10,
                );
                sample_bytes = Some(bytes);
            }
            Err(e) => {
                inner.failed.fetch_add(1, Ordering::AcqRel);
                AgentLog::warn_err("replay sample encode", &e);
            }
        }
    }

    if !keep {
        if let Some(ref mut sb) = sample_bytes {
            sb.zeroize();
        }
        return;
    }

    if let Some(cm) = map {
        let mut dmap = inner.dedup.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = dmap.get_mut(&g.monitor) {
            d.accept(hash, req.context.clone(), cm);
        }
    }

    if let Some(mut sb) = sample_bytes {
        if ImageCodec::resolve(&s.keyframes.format) == sample_format {
            let copy = sb.clone();
            sb.zeroize();
            match inner.store_keyframe(StoreKeyframeArgs {
                store: &*store,
                s: &s,
                g: &g,
                hash,
                change,
                reason,
                title: r.title,
                text,
                bytes: copy,
                format: sample_format,
                w: g.width,
                h: g.height,
            }) {
                Ok(()) => {}
                Err(KeyframeStoreError::PurgedWindow) => {
                    AgentLog::debug("keyframe inside a purged window dropped");
                }
                Err(e) => {
                    inner.failed.fetch_add(1, Ordering::AcqRel);
                    AgentLog::warn_err("keyframe store", &e.to_string());
                }
            }
            return;
        }
        sb.zeroize();
    }

    let pending = PendingEncode {
        grab: g,
        hash,
        change,
        reason,
        title: r.title,
        text,
    };
    let mut gov_g = inner.governor.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref mut gov) = *gov_g {
        gov.offer(pending);
        inner
            .budget_deferred
            .store(gov.deferred, Ordering::Relaxed);
        inner.budget_dropped.store(gov.dropped, Ordering::Relaxed);
        inner
            .budget_webp
            .store(gov.webp_fallbacks, Ordering::Relaxed);
    }
}

fn read_ocr(
    inner: &PipelineInner,
    reader: &dyn KeyframeReader,
    g: &KeyframeGrab,
    entities: bool,
    ct: &CancelToken,
) -> (Option<OcrResult>, bool, String) {
    let mut copy = g.pixels().to_vec();
    let res = (|| -> (Option<OcrResult>, bool, String) {
        let needs_start = !reader.is_ready();
        if needs_start {
            let mut ms = inner.mask_state.lock().unwrap_or_else(|e| e.into_inner());
            ms.worker_starting = true;
            inner.mark_unchecked_locked(&mut ms, g.monitor, g.grab_ticks);
            let queued_pairs: Vec<(i32, i64)> = ms
                .queued
                .iter()
                .filter(|(_, (count, _))| *count > 0)
                .map(|(&m, &(_, max_s))| (m, max_s))
                .collect();
            for (m, max_s) in queued_pairs {
                inner.mark_unchecked_locked(&mut ms, m, max_s);
            }
        }
        let ready_res = reader.ready(ct);
        if needs_start {
            let mut ms = inner.mask_state.lock().unwrap_or_else(|e| e.into_inner());
            ms.worker_starting = false;
        }
        if let Err(e) = ready_res {
            return map_worker_err(e);
        }

        let wait = Duration::from_millis(inner.ocr_wait_ms.load(Ordering::Acquire));
        let cs = Arc::new(CancelSource::new());
        let cs_parent = cs.clone();
        let _parent_guard = ct.on_cancel(move || cs_parent.cancel());
        let cs_timer = cs.clone();
        let _ = thread::Builder::new()
            .name("BetterRecall.KfOcrTimeout".to_string())
            .spawn(move || {
                let tok = cs_timer.token();
                if !tok.wait_timeout(wait) {
                    cs_timer.cancel();
                }
            });
        let tok = cs.token();
        let sw = Instant::now();
        let read_res = reader.read(&copy, g.width, g.height, g.stride, entities, &tok);
        cs.cancel();
        match read_res {
            Ok(r) => {
                let next = KeyframePipeline::next_ocr_wait(
                    inner.ocr_timeout,
                    wait,
                    sw.elapsed(),
                    false,
                );
                inner
                    .ocr_wait_ms
                    .store(next.as_millis() as u64, Ordering::Release);
                (Some(r), false, String::new())
            }
            Err(WorkerError::Cancelled) if !ct.is_cancelled() => {
                let next = KeyframePipeline::next_ocr_wait(inner.ocr_timeout, wait, wait, true);
                inner
                    .ocr_wait_ms
                    .store(next.as_millis() as u64, Ordering::Release);
                if next > wait {
                    AgentLog::info(&format!(
                        "ocr took over {:.0} s: next keyframes wait up to {:.0} s",
                        wait.as_secs_f64(),
                        next.as_secs_f64()
                    ));
                }
                (
                    None,
                    false,
                    format!("timeout after {:.0} s", wait.as_secs_f64()),
                )
            }
            Err(e) => map_worker_err(e),
        }
    })();
    copy.zeroize();
    res
}

fn map_worker_err(e: WorkerError) -> (Option<OcrResult>, bool, String) {
    let msg = e.message().to_string();
    let permanent = msg.starts_with("Model not installed");
    (None, permanent, msg)
}

fn encode_and_store(inner: &PipelineInner, p: &mut PendingEncode, level: EncodeLevel) -> f64 {
    let s = (inner.settings)();
    let Some(store) = (inner.store)() else {
        return 0.0;
    };
    let g = &p.grab;
    let cpu0 = current_thread_cpu_ms();
    let sw = Instant::now();
    let enc = unsafe { KeyframeEncoder::encode(g.pixels().as_ptr(), g.width, g.height, g.stride, &s, level) };
    let (bytes, format, w, h) = match enc {
        Ok(v) => v,
        Err(e) => {
            inner.failed.fetch_add(1, Ordering::AcqRel);
            AgentLog::warn_err("keyframe encode", &e);
            return (current_thread_cpu_ms() - cpu0).max(0.0);
        }
    };
    let cpu = (current_thread_cpu_ms() - cpu0).max(0.0);
    let wall_ms = sw.elapsed().as_secs_f64() * 1000.0;
    inner
        .last_encode_ms
        .store(wall_ms.to_bits(), Ordering::Relaxed);
    add_f64_atomic(&inner.encode_ms_total, wall_ms);
    add_f64_atomic(&inner.encode_cpu_ms_total, cpu);
    {
        let mut last = inner
            .encoder_name
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        KeyframeEncoder::log_change(&mut last, &KeyframeEncoder::describe(format, level));
    }
    match inner.store_keyframe(StoreKeyframeArgs {
        store: &*store,
        s: &s,
        g,
        hash: p.hash,
        change: p.change,
        reason: p.reason,
        title: p.title.clone(),
        text: p.text.clone(),
        bytes,
        format,
        w,
        h,
    }) {
        Ok(()) => {}
        Err(KeyframeStoreError::PurgedWindow) => {
            AgentLog::debug("keyframe inside a purged window dropped");
        }
        Err(e) => {
            inner.failed.fetch_add(1, Ordering::AcqRel);
            AgentLog::warn_err("keyframe store", &e.to_string());
        }
    }
    cpu
}
