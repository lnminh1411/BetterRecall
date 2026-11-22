// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Per-output capture coordinator, keyframe clock, event triggers, and replay mask merger (`Capture\CaptureService.cs`).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use br_contracts::{ReplaySettings, Settings};
use chrono::Utc;
use windows::Win32::System::SystemInformation::GetTickCount;

use crate::audio::{AudioReplayEncoder, LoopbackHub};
use crate::capture::change_map::CapturePolicy;
use crate::capture::gpu_devices::{GpuDeviceCache, GpuDevices, OutputInfo};
use crate::capture::keyframe_pipeline::KeyframePipeline;
use crate::capture::monitor_selection::MonitorSelection;
use crate::capture::output_capture::{KeyframeRequest, OutputCapture};
use crate::guardrails::{
    AudioGate, GuardReason, GuardSnapshot, GuardrailService, WindowInfo, WindowInspector,
};
use crate::native::{MASK_EVERYTHING, Rect, idle_milliseconds, tick_count64};
use crate::privacy::{RedactionMasks, RedactionOptions};
use crate::replay::{MediaFoundationRuntime, ReplayEngine, ReplayEngineConfig};
use crate::seams::CaptureSubsystem;
use crate::util::{AgentLog, RetryBackoff};

type SettingsProvider = Arc<dyn Fn() -> Settings + Send + Sync>;
type BoolProvider = Arc<dyn Fn() -> bool + Send + Sync>;

struct TickState {
    last_keyframe_tick: i64,
    last_sample_tick: i64,
    last_event_trigger: i64,
    last_input_tick: u32,
    input_burst: bool,
    was_paused: bool,
    pause_sample_grabs: i64,
    pause_ocr_at_start: i64,
    pause_start_ms: i64,
}

struct OutputsState {
    outputs: Vec<Arc<OutputCapture>>,
    layout_gen: i32,
}

/// Owns per-output DXGI capture threads, keyframe clock, event triggers, replay engine, and replay audio.
pub struct CaptureService {
    settings: SettingsProvider,
    keyframes_paused: BoolProvider,
    replay_paused_fn: BoolProvider,
    samples_paused: Option<BoolProvider>,
    guard: Arc<GuardrailService>,
    pipeline: Option<Arc<KeyframePipeline>>,
    redactions: Option<Arc<RedactionMasks>>,
    loopback: Arc<LoopbackHub>,
    monitor_selection_file: Option<PathBuf>,
    devices: Arc<GpuDeviceCache>,
    engine: Mutex<Option<Arc<ReplayEngine>>>,
    audio: Mutex<Option<Arc<AudioReplayEncoder>>>,
    audio_retry: Mutex<RetryBackoff>,
    outputs: Mutex<OutputsState>,
    tick_state: Mutex<TickState>,
    pending_trigger_at: AtomicI64,
    replay_paused_cached: AtomicBool,
    skipped_by_guard: AtomicI64,
    last_guard_reason: Mutex<GuardReason>,
    lock_memory: AtomicBool,
    timer_stop: Arc<(Mutex<bool>, Condvar)>,
    timer_thread: Mutex<Option<JoinHandle<()>>>,
    // Unused in A5; kept for tick state parity.
    _last_input_tick_atomic: AtomicU32,
}

/// Optional configuration for [`CaptureService::with_options`].
pub struct CaptureServiceOptions {
    pub lock_memory: bool,
    pub samples_paused: Option<BoolProvider>,
    pub monitor_selection_file: Option<PathBuf>,
    pub loopback: Option<Arc<LoopbackHub>>,
}

impl CaptureService {
    /// Whole-canvas sentinel mask rectangle (`CaptureService.MaskEverything`).
    pub const MASK_EVERYTHING: Rect = MASK_EVERYTHING;

    /// Status warning waits this long so the first read after start does not flash it (S11).
    pub const WARN_AFTER_MS: i64 = 5000;

    pub fn new(
        settings: SettingsProvider,
        keyframes_paused: BoolProvider,
        replay_paused: BoolProvider,
        guard: Arc<GuardrailService>,
        pipeline: Option<Arc<KeyframePipeline>>,
        redactions: Option<Arc<RedactionMasks>>,
        lock_memory: bool,
    ) -> Arc<Self> {
        Self::with_options(
            settings,
            keyframes_paused,
            replay_paused,
            guard,
            pipeline,
            redactions,
            CaptureServiceOptions {
                lock_memory,
                samples_paused: None,
                monitor_selection_file: None,
                loopback: None,
            },
        )
    }

    pub fn with_options(
        settings: SettingsProvider,
        keyframes_paused: BoolProvider,
        replay_paused: BoolProvider,
        guard: Arc<GuardrailService>,
        pipeline: Option<Arc<KeyframePipeline>>,
        redactions: Option<Arc<RedactionMasks>>,
        opts: CaptureServiceOptions,
    ) -> Arc<Self> {
        let devices = Arc::new(GpuDeviceCache::new());
        let cs = Arc::new(Self {
            settings,
            keyframes_paused,
            replay_paused_fn: replay_paused,
            samples_paused: opts.samples_paused,
            guard,
            pipeline: pipeline.clone(),
            redactions,
            loopback: opts.loopback.unwrap_or_else(|| Arc::new(LoopbackHub::new())),
            monitor_selection_file: opts.monitor_selection_file,
            devices: devices.clone(),
            engine: Mutex::new(None),
            audio: Mutex::new(None),
            audio_retry: Mutex::new(RetryBackoff::new(2000, 120_000)),
            outputs: Mutex::new(OutputsState {
                outputs: Vec::new(),
                layout_gen: 0,
            }),
            tick_state: Mutex::new(TickState {
                last_keyframe_tick: 0,
                last_sample_tick: 0,
                last_event_trigger: 0,
                last_input_tick: 0,
                input_burst: false,
                was_paused: false,
                pause_sample_grabs: 0,
                pause_ocr_at_start: 0,
                pause_start_ms: 0,
            }),
            pending_trigger_at: AtomicI64::new(0),
            replay_paused_cached: AtomicBool::new(false),
            skipped_by_guard: AtomicI64::new(0),
            last_guard_reason: Mutex::new(GuardReason::None),
            lock_memory: AtomicBool::new(opts.lock_memory),
            timer_stop: Arc::new((Mutex::new(false), Condvar::new())),
            timer_thread: Mutex::new(None),
            _last_input_tick_atomic: AtomicU32::new(0),
        });
        let weak_out = Arc::downgrade(&cs);
        let weak_masks = Arc::downgrade(&cs);
        let engine = ReplayEngine::new(
            devices,
            Arc::new(move || weak_out.upgrade().map(|c| c.outputs()).unwrap_or_default()),
            Arc::new(move || weak_masks.upgrade().map(|c| c.masks()).unwrap_or_default()),
        );
        *cs.engine.lock().unwrap_or_else(|e| e.into_inner()) = Some(engine);
        if let Some(ref p) = pipeline {
            let weak = Arc::downgrade(&cs);
            p.set_ocr_dropped(Some(Arc::new(move |monitor| {
                if let Some(c) = weak.upgrade() {
                    c.retry_keyframe(monitor);
                }
            })));
        }
        cs
    }

    pub fn engine(&self) -> Option<Arc<ReplayEngine>> {
        self.engine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn audio(&self) -> Option<Arc<AudioReplayEncoder>> {
        self.audio
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn devices(&self) -> &Arc<GpuDeviceCache> {
        &self.devices
    }

    pub fn outputs(&self) -> Vec<Arc<OutputCapture>> {
        self.outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outputs
            .clone()
    }

    /// Test helper: installs synthetic [`OutputCapture`]s and bumps layout generation.
    pub fn set_outputs_for_test(&self, list: Vec<Arc<OutputCapture>>, bump_gen: bool) {
        let mut st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        st.outputs = list;
        if bump_gen {
            st.layout_gen = st.layout_gen.wrapping_add(1);
        }
    }

    pub fn set_lock_memory(&self, on: bool) {
        self.lock_memory.store(on, Ordering::Release);
    }

    pub fn skipped_by_guard(&self) -> i64 {
        self.skipped_by_guard.load(Ordering::Acquire)
    }

    pub fn last_guard_reason(&self) -> GuardReason {
        *self
            .last_guard_reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn guard_key(layout_gen: i32, index: i32) -> i32 {
        layout_gen.wrapping_mul(64).wrapping_add(index)
    }

    fn retry_keyframe(&self, monitor: i32) {
        let st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        for o in &st.outputs {
            if o.info().index == monitor {
                o.retry_keyframe();
            }
        }
    }

    pub fn start(self: &Arc<Self>) {
        if let Err(e) = MediaFoundationRuntime::ensure_started() {
            AgentLog::warn_err("MediaFoundation startup", &e);
        }
        self.rebuild();
        if let Some(eng) = self.engine() {
            eng.start();
        }
        let mut slot = self.timer_thread.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let weak = Arc::downgrade(self);
        let stop_pair = self.timer_stop.clone();
        *slot = thread::Builder::new()
            .name("BetterRecall.CaptureTick".to_string())
            .spawn(move || {
                loop {
                    let (lock, cv) = &*stop_pair;
                    let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if *g {
                        return;
                    }
                    let (g2, _) = cv
                        .wait_timeout(g, Duration::from_millis(250))
                        .unwrap_or_else(|e| e.into_inner());
                    if *g2 {
                        return;
                    }
                    drop(g2);
                    let Some(cs) = weak.upgrade() else { return };
                    cs.tick();
                }
            })
            .ok();
    }

    pub fn rebuild(&self) {
        let old: Vec<Arc<OutputCapture>> = {
            let mut st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut st.outputs)
        };
        for o in old {
            o.stop();
        }
        let s = (self.settings)();
        let all = GpuDevices::outputs();
        let pinned = MonitorSelection::load(self.monitor_selection_file.as_deref());
        let (selected, pin) = MonitorSelection::resolve(
            &s.capture.monitors,
            &all,
            MonitorSelection::stable_id,
            pinned.as_ref(),
        );
        if let Some(ref p) = pin {
            MonitorSelection::save(self.monitor_selection_file.as_deref(), p);
        }
        let mut list = Vec::with_capacity(selected.len());
        let mut indices = Vec::with_capacity(selected.len());
        for o in selected {
            indices.push(o.index);
            let pipe = self.pipeline.clone();
            let cap = Arc::new(OutputCapture::new(
                o,
                self.devices.clone(),
                Arc::new(move |grab| {
                    if let Some(ref p) = pipe {
                        p.post(grab);
                    }
                }),
            ));
            list.push(cap);
        }
        {
            let mut st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
            st.outputs = list.clone();
            st.layout_gen = st.layout_gen.wrapping_add(1);
        }
        if let Some(ref p) = self.pipeline {
            p.track_monitors(&indices);
        }
        self.apply_settings();
        for o in &list {
            o.start();
        }
        if let Some(eng) = self.engine() {
            eng.request_rebuild("outputs");
        }
        AgentLog::info(&format!("capturing {} output(s)", list.len()));
        self.guard.refresh();
    }

    pub fn apply_settings(&self) {
        let s = (self.settings)();
        let r = &s.replay;
        let clamped_seconds = r.seconds.clamp(15, 600);
        if let Some(eng) = self.engine() {
            eng.configure(ReplayEngineConfig {
                enabled: r.enabled,
                layout: r.layout.clone(),
                fps: r.fps,
                kbps: r.bitrate_kbps.clamp(500, 50_000),
                seconds: clamped_seconds,
                encoder: r.encoder.clone(),
                encoder_adapter: r.encoder_adapter.clone(),
                resolution: r.resolution.clone(),
                scale_percent: r.scale_percent,
                lock_memory: self.lock_memory.load(Ordering::Acquire),
            });
            let rp = (self.replay_paused_fn)();
            self.replay_paused_cached.store(rp, Ordering::Release);
            eng.set_paused(rp);
            self.sync_replay_audio(&s, rp);
        } else {
            let rp = (self.replay_paused_fn)();
            self.replay_paused_cached.store(rp, Ordering::Release);
        }
    }

    fn sync_replay_audio(&self, s: &Settings, replay_paused: bool) {
        let r = &s.replay;
        let clamped_seconds = r.seconds.clamp(15, 600);
        let want_audio = r.enabled && r.audio;
        let mut audio_slot = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        let retry = self.audio_retry.lock().unwrap_or_else(|e| e.into_inner());
        if want_audio
            && audio_slot
                .as_ref()
                .is_none_or(|a| a.seconds() != clamped_seconds)
        {
            let now = tick_count64();
            if audio_slot.is_some() || retry.due(now) {
                if let Some(old) = audio_slot.take() {
                    old.stop();
                }
                let guard = self.guard.clone();
                let muted: BoolProvider =
                    Arc::new(move || guard.audio_now().gate != AudioGate::Allowed);
                match AudioReplayEncoder::new(
                    self.loopback.clone(),
                    clamped_seconds,
                    self.lock_memory.load(Ordering::Acquire),
                    muted,
                ) {
                    Ok(enc) => {
                        *audio_slot = Some(Arc::new(enc));
                        retry.succeeded();
                    }
                    Err(e) => {
                        let next_s = retry.failed(now) / 1000;
                        AgentLog::warn_err(
                            &format!("replay audio unavailable (retry in {next_s} s)"),
                            &e,
                        );
                    }
                }
            }
        } else if !want_audio
            && let Some(old) = audio_slot.take()
        {
            old.stop();
        }
        if !want_audio {
            retry.succeeded();
        }
        if let Some(ref a) = *audio_slot {
            a.set_paused(replay_paused);
        }
    }

    /// A-H2 (D1): replay audio is silence while a denied, private, or unscanned window is in front.
    pub fn replay_audio_muted(&self) -> bool {
        self.guard.audio_now().gate != AudioGate::Allowed
    }

    /// Writes the last `replay.seconds` of every pipeline plus desktop audio as fragmented MP4.
    pub fn save_replay(&self, output: &mut dyn Write) -> Result<i32, String> {
        let s = (self.settings)();
        let eng = self
            .engine()
            .ok_or_else(|| "Replay is not running.".to_string())?;
        let audio_src = self.audio().map(|a| a.source());
        eng.save(output, audio_src.as_ref(), s.replay.seconds.clamp(15, 600))
    }

    pub fn replay_ring_bytes(&self) -> usize {
        let v = self.engine().map_or(0, |e| e.ring_bytes());
        let a = self.audio().map_or(0, |a| a.ring().committed_bytes());
        v + a
    }

    /// Computes replay masks: denied windows, password fields, keyframe OCR boxes,
    /// unscanned foreground window, and foreground window while text masks are unchecked (C2).
    pub fn masks(&self) -> Vec<Rect> {
        let snap = self.guard.current(2000);
        let s = (self.settings)();
        let red_all = self
            .redactions
            .as_ref()
            .map(|r| r.all())
            .unwrap_or_default();
        let unscanned = self.guard.unscanned_foreground(&snap);
        let mut masks = Self::merge_masks(&snap, s.privacy.redact_passwords, &red_all, unscanned);
        let unchecked = self
            .pipeline
            .as_ref()
            .is_some_and(|p| p.masks_unchecked());
        if !unchecked || !Self::text_masks_needed(&s) {
            return masks;
        }
        let fg = WindowInspector::foreground_root();
        if fg == 0 {
            masks.push(Self::MASK_EVERYTHING);
            return masks;
        }
        let Some(b) = WindowInspector::try_get_bounds(fg) else {
            masks.push(Self::MASK_EVERYTHING);
            return masks;
        };
        if self.foreground_on_unchecked_monitor(b) {
            masks.push(b);
        }
        masks
    }

    fn foreground_on_unchecked_monitor(&self, w: Rect) -> bool {
        let st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        if st.outputs.is_empty() {
            return true;
        }
        for o in &st.outputs {
            let i = o.info();
            if w.left < i.left + i.width
                && i.left < w.right
                && w.top < i.top + i.height
                && i.top < w.bottom
                && self
                    .pipeline
                    .as_ref()
                    .is_some_and(|p| p.monitor_unchecked(i.index))
            {
                return true;
            }
        }
        false
    }

    pub fn text_masks_needed(s: &Settings) -> bool {
        s.ocr.enabled && RedactionOptions::from_privacy(&s.privacy).any_text()
    }

    /// C2 (D2): replay is recording while text masks have been unchecked for `WARN_AFTER_MS`+.
    pub fn replay_text_unchecked(&self) -> bool {
        let s = (self.settings)();
        s.replay.enabled
            && !self.replay_paused_cached.load(Ordering::Acquire)
            && self
                .pipeline
                .as_ref()
                .is_some_and(|p| p.masks_unchecked() && p.unchecked_for_ms() >= Self::WARN_AFTER_MS)
            && Self::text_masks_needed(&s)
    }

    /// Pure mask merge (`CaptureService.MergeMasks`).
    pub fn merge_masks(
        snap: &GuardSnapshot,
        passwords: bool,
        redactions: &[Rect],
        unscanned_foreground: Option<Rect>,
    ) -> Vec<Rect> {
        if snap.is_closed_sentinel {
            return vec![Self::MASK_EVERYTHING];
        }
        let secrets = if passwords { snap.secret_rects() } else { &[] };
        if secrets.is_empty() && redactions.is_empty() && unscanned_foreground.is_none() {
            return snap.mask_rects.clone();
        }
        let mut out = Vec::with_capacity(
            snap.mask_rects.len()
                + secrets.len()
                + redactions.len()
                + usize::from(unscanned_foreground.is_some()),
        );
        out.extend_from_slice(&snap.mask_rects);
        out.extend_from_slice(secrets);
        out.extend_from_slice(redactions);
        if let Some(fg) = unscanned_foreground {
            out.push(fg);
        }
        out
    }

    pub fn on_foreground_changed(&self) {
        if !(self.settings)().capture.event_triggers {
            return;
        }
        self.pending_trigger_at
            .store(tick_count64() + 600, Ordering::Release);
    }

    fn tick(&self) {
        let s = (self.settings)();
        let now = tick_count64();
        let replay_paused = (self.replay_paused_fn)();
        self.replay_paused_cached
            .store(replay_paused, Ordering::Release);
        if let Some(eng) = self.engine() {
            eng.set_paused(replay_paused);
        }
        if let Some(a) = self.audio() {
            a.set_paused(replay_paused);
        } else if s.replay.enabled && s.replay.audio {
            self.sync_replay_audio(&s, replay_paused);
        }

        let kf_paused = (self.keyframes_paused)();
        self.note_pause_samples(kf_paused);

        let samples_paused = self.samples_paused.as_ref().is_some_and(|f| f());
        let pipelines = self.engine().map_or(0, |e| e.pipelines_count());
        let since_last_sample = {
            let ts = self.tick_state.lock().unwrap_or_else(|e| e.into_inner());
            now - ts.last_sample_tick
        };
        let sample_due = Self::sample_due(
            &s.replay,
            replay_paused,
            samples_paused,
            pipelines,
            since_last_sample,
            kf_paused && Self::text_masks_needed(&s),
        );
        if kf_paused {
            if sample_due {
                {
                    let mut ts = self.tick_state.lock().unwrap_or_else(|e| e.into_inner());
                    ts.last_sample_tick = now;
                    ts.pause_sample_grabs += 1;
                }
                self.request_keyframes(false, false, true, false);
            }
            return;
        }

        let interval = s.capture.interval_ms.clamp(500, 60_000) as i64;
        let (due, trigger) = {
            let mut ts = self.tick_state.lock().unwrap_or_else(|e| e.into_inner());
            let due = now - ts.last_keyframe_tick >= interval;
            let mut trigger = false;
            if s.capture.event_triggers {
                let pending = self.pending_trigger_at.load(Ordering::Acquire);
                if pending != 0 && now >= pending {
                    trigger = true;
                    self.pending_trigger_at.store(0, Ordering::Release);
                }
                let idle = idle_milliseconds().max(0) as u32;
                let last_input = unsafe { GetTickCount() }.wrapping_sub(idle);
                if last_input != ts.last_input_tick {
                    ts.last_input_tick = last_input;
                    ts.input_burst = true;
                } else if ts.input_burst && idle >= 700 {
                    ts.input_burst = false;
                    trigger = true;
                }
                if trigger && now - ts.last_event_trigger < 1000 {
                    trigger = false;
                }
            }
            if !due && !trigger {
                if sample_due {
                    ts.last_sample_tick = now;
                }
                (false, false)
            } else {
                if due {
                    ts.last_keyframe_tick = now;
                }
                if trigger {
                    ts.last_event_trigger = now;
                }
                if sample_due {
                    ts.last_sample_tick = now;
                }
                (due, trigger)
            }
        };

        if !due && !trigger {
            if sample_due {
                self.request_keyframes(false, false, true, false);
            }
            return;
        }
        self.request_keyframes(false, true, sample_due, trigger);
    }

    /// Applies guardrails and posts a [`KeyframeRequest`] to each allowed output.
    pub fn request_keyframes(&self, force: bool, keyframe: bool, sample: bool, trigger: bool) {
        let s = (self.settings)();
        let snap = self.guard.current_for_keyframe(2000);
        if !snap.decision.allow {
            self.skipped_by_guard.fetch_add(1, Ordering::Relaxed);
            *self
                .last_guard_reason
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = snap.decision.reason;
            if (keyframe || sample) && Self::refresh_masks_while_denied(snap.decision.reason) {
                self.request_mask_refresh(&snap, &s);
            }
            return;
        }
        *self
            .last_guard_reason
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = GuardReason::None;
        let ts = Utc::now();
        let st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        for o in &st.outputs {
            let key = Self::guard_key(st.layout_gen, o.info().index);
            let Some(mt) = snap.per_monitor.get(&key) else {
                self.skipped_by_guard.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if mt.denied_visible {
                self.skipped_by_guard.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let w: Option<&WindowInfo> = mt.window.as_ref().or_else(|| {
                snap.foreground
                    .as_ref()
                    .filter(|fg| is_on_monitor(fg, o.info()))
            });
            let exe = w.map(|x| x.exe_path.as_str()).unwrap_or("");
            let app = if exe.is_empty() {
                String::new()
            } else {
                std::path::Path::new(exe)
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            let title = w.map(|x| x.title.as_str()).unwrap_or("");
            let site = if w.is_some()
                && snap.foreground.as_ref().is_some_and(|f| Some(f.hwnd) == w.map(|x| x.hwnd))
            {
                snap.site.clone()
            } else {
                String::new()
            };
            let info = o.info();
            let bounds = Rect::new(info.left, info.top, info.left + info.width, info.top + info.height);
            let req = KeyframeRequest {
                ts,
                app: app.clone(),
                exe_path: exe.to_string(),
                title: title.to_string(),
                context: format!("{app}\u{0001}{title}"),
                force,
                max_width: s.capture.max_width.clamp(640, 3840),
                dirty_min_percent: s
                    .capture
                    .dirty_min_percent
                    .clamp(0.0, 100.0)
                    .min(CapturePolicy::TRIGGER_MIN_SCORE),
                site,
                keyframe,
                replay_sample: sample,
                trigger,
                secrets: if s.privacy.redact_passwords {
                    snap.secrets.clone()
                } else {
                    None
                },
                bounds,
                mask_only: false,
            };
            o.set_pending_keyframe(Some(req));
        }
    }

    /// Denied/private foreground still refreshes replay text masks of other monitors (S2). Pure.
    pub const fn refresh_masks_while_denied(r: GuardReason) -> bool {
        matches!(
            r,
            GuardReason::DenyProcess
                | GuardReason::DenyPath
                | GuardReason::DenyTitle
                | GuardReason::PrivateBadge
                | GuardReason::BadgeCheckFailed
        )
    }

    fn request_mask_refresh(&self, snap: &GuardSnapshot, s: &Settings) {
        if !s.replay.enabled || !Self::text_masks_needed(s) {
            return;
        }
        let ts = Utc::now();
        let st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        for o in &st.outputs {
            let key = Self::guard_key(st.layout_gen, o.info().index);
            let Some(mt) = snap.per_monitor.get(&key) else {
                continue;
            };
            if mt.denied_visible {
                continue;
            }
            let info = o.info();
            let bounds = Rect::new(info.left, info.top, info.left + info.width, info.top + info.height);
            o.set_pending_keyframe(Some(KeyframeRequest {
                ts,
                app: String::new(),
                exe_path: String::new(),
                title: String::new(),
                context: String::new(),
                force: false,
                max_width: s.capture.max_width.clamp(640, 3840),
                dirty_min_percent: s
                    .capture
                    .dirty_min_percent
                    .clamp(0.0, 100.0)
                    .min(CapturePolicy::TRIGGER_MIN_SCORE),
                site: String::new(),
                keyframe: false,
                replay_sample: true,
                trigger: false,
                secrets: None,
                bounds,
                mask_only: true,
            }));
        }
    }

    fn note_pause_samples(&self, paused: bool) {
        let mut ts = self.tick_state.lock().unwrap_or_else(|e| e.into_inner());
        if paused == ts.was_paused {
            return;
        }
        ts.was_paused = paused;
        let ocr_now = self
            .pipeline
            .as_ref()
            .map_or(0, |p| p.sample_ocr_reads());
        if paused {
            ts.pause_sample_grabs = 0;
            ts.pause_ocr_at_start = ocr_now;
            ts.pause_start_ms = tick_count64();
            return;
        }
        let reads = ocr_now - ts.pause_ocr_at_start;
        if ts.pause_sample_grabs > 0 || reads > 0 {
            AgentLog::info(&format!(
                "keyframe pause ended after {} s: {} sample requests, {reads} sample OCR reads (replay masks)",
                (tick_count64() - ts.pause_start_ms) / 1000,
                ts.pause_sample_grabs
            ));
        }
    }

    /// Pure replay-sample schedule decision (`CaptureService.SampleDue`).
    pub fn sample_due(
        r: &ReplaySettings,
        replay_paused: bool,
        samples_paused: bool,
        pipelines: i32,
        since_last_ms: i64,
        mask_refresh: bool,
    ) -> bool {
        r.enabled
            && (r.searchable || mask_refresh)
            && !replay_paused
            && !samples_paused
            && pipelines > 0
            && since_last_ms >= (r.sample_seconds.clamp(2, 60) as i64) * 1000
    }

    pub fn retry_all(&self) {
        let st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        for o in &st.outputs {
            o.retry_keyframe();
        }
    }

    pub fn forget_recent(&self) {
        if let Some(eng) = self.engine() {
            eng.panic();
        }
        if let Some(a) = self.audio() {
            a.ring().clear();
        }
        if let Some(ref p) = self.pipeline {
            p.flush();
        }
    }

    pub fn panic(&self) {
        if let Some(eng) = self.engine() {
            eng.panic();
        }
        if let Some(a) = self.audio() {
            a.ring().clear();
        }
        if let Some(ref p) = self.pipeline {
            p.reset_dedup();
            p.flush();
        }
    }

    pub fn monitor_bounds(&self) -> Vec<(i32, Rect)> {
        let st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        st.outputs
            .iter()
            .map(|o| {
                let i = o.info();
                (
                    Self::guard_key(st.layout_gen, i.index),
                    Rect::new(i.left, i.top, i.left + i.width, i.top + i.height),
                )
            })
            .collect()
    }

    pub fn stop(&self) {
        {
            let (lock, cv) = &*self.timer_stop;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cv.notify_all();
        }
        if let Some(h) = self
            .timer_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        if let Some(eng) = self.engine.lock().unwrap_or_else(|e| e.into_inner()).take() {
            eng.stop();
        }
        let old: Vec<Arc<OutputCapture>> = {
            let mut st = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut st.outputs)
        };
        for o in old {
            o.stop();
        }
        if let Some(a) = self.audio.lock().unwrap_or_else(|e| e.into_inner()).take() {
            a.stop();
        }
        self.devices.clear();
    }
}

impl CaptureSubsystem for CaptureService {
    fn start(&self) {
        // Note: `AgentHost` calls `CaptureService::start(&Arc<Self>)` directly; trait method is no-op fallback.
    }

    fn apply_settings(&self) {
        CaptureService::apply_settings(self);
    }

    fn rebuild(&self) {
        CaptureService::rebuild(self);
    }

    fn retry_all(&self) {
        CaptureService::retry_all(self);
    }

    fn on_foreground_changed(&self) {
        CaptureService::on_foreground_changed(self);
    }

    fn forget_recent(&self) {
        CaptureService::forget_recent(self);
    }

    fn panic(&self) {
        CaptureService::panic(self);
    }

    fn monitor_bounds(&self) -> Vec<(i32, Rect)> {
        CaptureService::monitor_bounds(self)
    }

    fn replay_text_unchecked(&self) -> bool {
        CaptureService::replay_text_unchecked(self)
    }

    fn replay_active(&self) -> bool {
        self.engine().is_some_and(|e| e.pipelines_count() > 0)
    }

    fn replay_last_error(&self) -> Option<String> {
        self.engine().and_then(|e| e.last_error())
    }

    fn diagnostics(&self) -> Option<String> {
        let outs = self.outputs();
        let mut s = format!(
            "capture: {} output(s), skipped_by_guard {}\n",
            outs.len(),
            self.skipped_by_guard()
        );
        for o in outs {
            let i = o.info();
            s.push_str(&format!(
                "  output {} ({}x{}): state {}, frames {}, grabs {}, skipped_no_change {}, access_lost {}\n",
                i.index,
                i.width,
                i.height,
                o.state(),
                o.frame_count(),
                o.grab_count(),
                o.skipped_no_change(),
                o.access_lost()
            ));
        }
        Some(s)
    }

    fn stop(&self) {
        CaptureService::stop(self);
    }
}

impl Drop for CaptureService {
    fn drop(&mut self) {
        self.stop();
    }
}

fn is_on_monitor(w: &WindowInfo, o: &OutputInfo) -> bool {
    WindowInspector::try_get_bounds(w.hwnd).is_some_and(|r| {
        r.left < o.left + o.width
            && o.left < r.right
            && r.top < o.top + o.height
            && o.top < r.bottom
    })
}
