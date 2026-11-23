// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Background replay engine managing pipelines, video streams, backoff, and saving (`Replay\ReplayEngine.cs`).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};

use crate::capture::gpu_devices::{GpuDeviceCache, GpuDevices};
use crate::capture::output_capture::{FramePacer, OutputCapture};
use crate::native::{Rect, qpc, tick_count64};
use crate::replay::bitstream::VideoCodec;
use crate::replay::replay_layout::ReplayRate;
use crate::replay::replay_mux::{AudioRingSource, ReplayMux};
use crate::replay::replay_pipeline::{PipelineSpec, ReplayPipeline};
use crate::replay::video_encoders::{EncoderFactory, EncoderPlan};
use crate::replay::video_stream::VideoStream;
use crate::security::MemoryIsolation;
use crate::util::{AgentLog, HighResTimer, RetryBackoff};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayEngineConfig {
    pub enabled: bool,
    pub layout: String,
    pub fps: i32,
    pub kbps: i32,
    pub seconds: i32,
    pub encoder: String,
    pub encoder_adapter: String,
    pub resolution: String,
    pub scale_percent: i32,
    pub lock_memory: bool,
}

/// Point-in-time snapshot of a live replay pipeline's counters for diagnostics / `--selftest-replay`.
#[derive(Debug, Clone)]
pub struct PipelineDiagStats {
    pub key: String,
    pub codec: VideoCodec,
    pub api: String,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub frames_in: i64,
    pub packets_out: i64,
    pub frames_dropped: i64,
    pub busy_count: i64,
    pub forced_keyframes: i64,
    pub repeat_count: i64,
    pub copy_ticks: i64,
    pub copy_count: i64,
    pub latency_p95: f64,
    pub ring_committed_bytes: usize,
    pub ring_capacity: usize,
    pub ring_lock_failed: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct OutputSignature {
    ptr: usize,
    index: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    generation: i32,
}

type OutputsProvider = Arc<dyn Fn() -> Vec<Arc<OutputCapture>> + Send + Sync>;
type MasksProvider = Arc<dyn Fn() -> Vec<Rect> + Send + Sync>;

struct EngineState {
    current_config: Option<ReplayEngineConfig>,
    pending_config: Option<ReplayEngineConfig>,
    pending_config_time: i64,
    rebuild_requested: bool,
    rebuild_reason: Option<String>,
    last_build_attempt: i64,
    last_error: Option<String>,
    encoder_backoff: RetryBackoff,
    built_at_ms: i64,
    streams: HashMap<String, Arc<VideoStream>>,
    last_signatures: Vec<OutputSignature>,
    pipeline_stats: Vec<PipelineDiagStats>,
}

struct EngineShared {
    devices: Arc<GpuDeviceCache>,
    outputs: OutputsProvider,
    masks: MasksProvider,
    stop: AtomicBool,
    paused: AtomicBool,
    suspended: AtomicBool,
    ticking: AtomicBool,
    panic_requested: AtomicBool,
    pipelines_count: AtomicI32,
    encoder_failures: AtomicI32,
    /// Set by [`ReplayEngine::pipeline_stats`]; the engine thread fills the snapshot on its next tick and clears it.
    stats_requested: AtomicBool,
    state: Mutex<EngineState>,
}

/// Manages hardware replay pipelines and [`VideoStream`] packet rings on its own thread.
pub struct ReplayEngine {
    shared: Arc<EngineShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ReplayEngine {
    pub fn new(
        devices: Arc<GpuDeviceCache>,
        outputs: OutputsProvider,
        masks: MasksProvider,
    ) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(EngineShared {
                devices,
                outputs,
                masks,
                stop: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                suspended: AtomicBool::new(false),
                ticking: AtomicBool::new(false),
                panic_requested: AtomicBool::new(false),
                stats_requested: AtomicBool::new(false),
                pipelines_count: AtomicI32::new(0),
                encoder_failures: AtomicI32::new(0),
                state: Mutex::new(EngineState {
                    current_config: None,
                    pending_config: None,
                    pending_config_time: 0,
                    rebuild_requested: false,
                    rebuild_reason: None,
                    last_build_attempt: 0,
                    last_error: None,
                    encoder_backoff: RetryBackoff::new(1000, 60_000),
                    built_at_ms: 0,
                    streams: HashMap::new(),
                    last_signatures: Vec::new(),
                    pipeline_stats: Vec::new(),
                }),
            }),
            thread: Mutex::new(None),
        })
    }

    pub fn start(&self) {
        let mut slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let shared = self.shared.clone();
        *slot = thread::Builder::new()
            .name("replay-engine".to_string())
            .spawn(move || run_engine(shared))
            .ok();
    }

    pub fn configure(&self, cfg: ReplayEngineConfig) {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        st.pending_config = Some(cfg);
        st.pending_config_time = qpc();
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
    }

    pub fn suspend(&self) {
        self.shared.suspended.store(true, Ordering::Release);
        let freq = qpc_frequency();
        let start = qpc();
        while self.shared.ticking.load(Ordering::Acquire)
            && FramePacer::to_hundred_ns(qpc() - start, freq) < 20_000_000
        {
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn resume_after_suspend(&self) {
        self.shared.suspended.store(false, Ordering::Release);
        self.request_rebuild("resume");
    }

    pub fn request_rebuild(&self, why: &str) {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        st.rebuild_requested = true;
        st.rebuild_reason = Some(why.to_string());
    }

    pub fn pipelines_count(&self) -> i32 {
        self.shared.pipelines_count.load(Ordering::Acquire)
    }

    /// Snapshot of the live pipelines for `--selftest-replay`. Asks the engine thread for it and waits up to 1 s,
    /// so the agent pays nothing for these counters unless a self test reads them.
    pub fn pipeline_stats(&self) -> Vec<PipelineDiagStats> {
        self.shared.stats_requested.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while self.shared.stats_requested.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pipeline_stats
            .clone()
    }

    pub fn streams(&self) -> Vec<Arc<VideoStream>> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .values()
            .cloned()
            .collect()
    }

    pub fn last_error(&self) -> Option<String> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error
            .clone()
    }

    pub fn encoder_failures(&self) -> i32 {
        self.shared.encoder_failures.load(Ordering::Acquire)
    }

    pub fn save<W: Write + ?Sized>(
        &self,
        output: &mut W,
        audio: Option<&AudioRingSource>,
        seconds: i32,
    ) -> Result<i32, String> {
        let (streams, enabled) = {
            let st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let enabled = st.current_config.as_ref().is_some_and(|c| c.enabled);
            (st.streams.values().cloned().collect::<Vec<_>>(), enabled)
        };
        // D2: refuse when replay is disabled
        if !enabled {
            return Err("Instant replay is off.".to_string());
        }
        ReplayMux::save(output, &streams, audio, seconds)
    }

    pub fn is_enabled(&self) -> bool {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current_config
            .as_ref()
            .is_some_and(|c| c.enabled)
    }

    /// Bytes of one video ring for this bitrate and window.
    pub const fn ring_capacity(kbps: i32, seconds: i32) -> i64 {
        (kbps as i64) * 1000 / 8 * (seconds as i64 + 2) * 21 / 20
    }

    /// Locked memory of the replay: every video ring, snapshot copy, and audio ring (A11). Pure.
    pub const fn locked_bytes(kbps: i32, seconds: i32, streams: i32) -> i64 {
        let s = if streams > 1 { streams as i64 } else { 1 };
        Self::ring_capacity(kbps, seconds) * s * 2 + 8 * 1024 * 1024
    }

    /// Zeroes every ring now; the engine thread then rebuilds (fresh encoders, black canvas).
    pub fn panic(&self) {
        {
            let st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            for s in st.streams.values() {
                s.clear();
            }
        }
        self.shared.panic_requested.store(true, Ordering::Release);
        self.request_rebuild("panic");
    }

    pub fn ring_bytes(&self) -> usize {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .values()
            .map(|s| s.ring().committed_bytes())
            .sum()
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streams
            .clear();
        self.shared.pipelines_count.store(0, Ordering::Release);
    }
}

impl Drop for ReplayEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn qpc_frequency() -> i64 {
    let mut f = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1)
}

fn run_engine(shared: Arc<EngineShared>) {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let mut timer = HighResTimer::new();
    let mut pipelines: Vec<ReplayPipeline> = Vec::new();
    let freq = qpc_frequency();

    while !shared.stop.load(Ordering::Acquire) {
        if shared.panic_requested.swap(false, Ordering::AcqRel) {
            pipelines.clear();
            shared.pipelines_count.store(0, Ordering::Release);
        }

        apply_pending_config(&shared, &mut pipelines, freq);

        let not_running = {
            let st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            st.current_config.as_ref().is_none_or(|c| !c.enabled)
                || shared.paused.load(Ordering::Acquire)
                || shared.suspended.load(Ordering::Acquire)
        };
        if not_running {
            for o in (shared.outputs)() {
                o.set_replay_poll_fps(0);
            }
            shared.ticking.store(false, Ordering::Release);
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        check_rebuild(&shared, &mut pipelines, freq);
        tick_pipelines(&shared, &mut pipelines, &mut timer, freq);
        if shared.stats_requested.load(Ordering::Acquire) {
            update_pipeline_stats(&shared, &pipelines);
            shared.stats_requested.store(false, Ordering::Release);
        }
    }

    pipelines.clear();
    shared.pipelines_count.store(0, Ordering::Release);
    update_pipeline_stats(&shared, &pipelines);
}

fn update_pipeline_stats(shared: &EngineShared, pipelines: &[ReplayPipeline]) {
    let snap: Vec<PipelineDiagStats> = pipelines
        .iter()
        .map(|p| PipelineDiagStats {
            key: p.spec.key.clone(),
            codec: p.encoder.codec(),
            api: p.encoder.api().to_string(),
            name: p.encoder.name().to_string(),
            width: p.encoder.width(),
            height: p.encoder.height(),
            fps: p.spec.fps,
            frames_in: p.encoder.frames_in(),
            packets_out: p.encoder.packets_out(),
            frames_dropped: p.encoder.frames_dropped(),
            busy_count: p.busy_count,
            forced_keyframes: p.encoder.forced_keyframes(),
            repeat_count: p.repeat_count,
            copy_ticks: p.copy_ticks,
            copy_count: p.copy_count,
            latency_p95: p.encoder.latency().percentile(95.0),
            ring_committed_bytes: p.stream.ring().committed_bytes(),
            ring_capacity: p.stream.ring().capacity(),
            ring_lock_failed: p.stream.ring().lock_failed(),
        })
        .collect();
    shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pipeline_stats = snap;
}

fn apply_pending_config(
    shared: &EngineShared,
    pipelines: &mut Vec<ReplayPipeline>,
    freq: i64,
) {
    let pending_opt = {
        let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.pending_config.is_some() && st.pending_config == st.current_config {
            st.pending_config = None;
            if pipelines.is_empty() {
                st.rebuild_requested = true;
            }
        }
        let Some(ref pending) = st.pending_config else {
            return;
        };
        let immediate = pipelines.is_empty()
            || st.current_config.as_ref().is_none_or(|cur| {
                pending.enabled != cur.enabled
                    || !pending.layout.eq_ignore_ascii_case(&cur.layout)
            });
        if immediate {
            st.pending_config.take()
        } else {
            let now_ticks = qpc();
            let elapsed_100ns = FramePacer::to_hundred_ns(now_ticks - st.pending_config_time, freq);
            let now_100ns = FramePacer::to_hundred_ns(now_ticks, freq);
            let gop_boundary = pipelines.iter().all(|p| {
                let half_frame = 5_000_000 / p.spec.fps.max(1) as i64;
                now_100ns - p.stream.last_key_pts() >= 10_000_000 - 2 * half_frame
            });
            if gop_boundary || elapsed_100ns >= 15_000_000 {
                st.pending_config.take()
            } else {
                None
            }
        }
    };

    if let Some(cfg) = pending_opt {
        let is_disabled = !cfg.enabled;
        {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            // D2: when the applied config has enabled=false, clear streams the same way stop does
            if is_disabled {
                st.streams.clear();
            }
            st.current_config = Some(cfg);
        }
        pipelines.clear();
        shared.pipelines_count.store(0, Ordering::Release);
        build_pipelines(shared, pipelines);
    }
}

fn check_rebuild(shared: &EngineShared, pipelines: &mut Vec<ReplayPipeline>, freq: i64) {
    let current_outputs = (shared.outputs)();
    let current_sigs: Vec<OutputSignature> = current_outputs
        .iter()
        .map(|o| OutputSignature {
            ptr: Arc::as_ptr(o) as usize,
            index: o.info().index,
            left: o.info().left,
            top: o.info().top,
            width: o.source_width(),
            height: o.source_height(),
            generation: o.feed_generation(),
        })
        .collect();

    let mut failed_info: Option<(String, String, String, String, Arc<crate::capture::gpu_devices::GpuDevice>)> = None;
    let mut any_needs_rebuild = false;
    for p in pipelines.iter() {
        if let Some(err) = p.encoder_failure() {
            failed_info = Some((
                p.encoder.codec().to_string(),
                p.encoder.api().to_string(),
                p.encoder.name().to_string(),
                format!("{}: {err}", p.spec.key),
                p.encoder_device().clone(),
            ));
            any_needs_rebuild = true;
            break;
        }
        if p.needs_rebuild() {
            any_needs_rebuild = true;
        }
    }

    let now_ms = tick_count64();
    if let Some((codec, api, name, key_err, enc_dev)) = failed_info {
        let delay = {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let d = st.encoder_backoff.failed(now_ms);
            shared
                .encoder_failures
                .store(st.encoder_backoff.failures(), Ordering::Release);
            st.last_error = Some(format!("encoder failed ({key_err}), retrying"));
            st.rebuild_requested = true;
            st.built_at_ms = 0;
            d
        };
        AgentLog::warn(&format!(
            "replay encoder {codec}/{api} '{name}' for {key_err}; rebuilding in {:.0} s (failure {} in a row)",
            delay as f64 / 1000.0,
            shared.encoder_failures.load(Ordering::Acquire)
        ));
        shared.devices.invalidate_if_removed(&enc_dev);
        pipelines.clear();
        shared.pipelines_count.store(0, Ordering::Release);
        return;
    }

    let (sigs_changed, should_rebuild, reason) = {
        let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.encoder_backoff.failures() > 0
            && st.built_at_ms != 0
            && now_ms - st.built_at_ms >= 60_000
            && !pipelines.is_empty()
        {
            st.encoder_backoff.succeeded();
            shared.encoder_failures.store(0, Ordering::Release);
            AgentLog::info("replay encoder healthy again");
        }
        let sigs_changed = st.last_signatures != current_sigs;
        let should = st.rebuild_requested || any_needs_rebuild || sigs_changed;
        if !should || !st.encoder_backoff.due(now_ms) {
            return;
        }
        let elapsed = FramePacer::to_hundred_ns(qpc() - st.last_build_attempt, freq);
        if pipelines.is_empty() && elapsed < 10_000_000 {
            return;
        }
        st.rebuild_requested = false;
        let r = if sigs_changed {
            "outputs changed".to_string()
        } else if any_needs_rebuild {
            "pipeline asked".to_string()
        } else {
            st.rebuild_reason
                .clone()
                .unwrap_or_else(|| "requested".to_string())
        };
        (sigs_changed, should, r)
    };
    let _ = (sigs_changed, should_rebuild);

    if !pipelines.is_empty() {
        AgentLog::info(&format!("replay rebuild: {reason}"));
    }
    pipelines.clear();
    shared.pipelines_count.store(0, Ordering::Release);
    build_pipelines(shared, pipelines);
}

fn build_pipelines(shared: &EngineShared, pipelines: &mut Vec<ReplayPipeline>) {
    let cfg = {
        let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        st.last_build_attempt = qpc();
        match st.current_config.clone() {
            Some(c) if c.enabled => c,
            _ => return,
        }
    };

    let current_outputs = (shared.outputs)();
    let sigs: Vec<OutputSignature> = current_outputs
        .iter()
        .map(|o| OutputSignature {
            ptr: Arc::as_ptr(o) as usize,
            index: o.info().index,
            left: o.info().left,
            top: o.info().top,
            width: o.source_width(),
            height: o.source_height(),
            generation: o.feed_generation(),
        })
        .collect();
    {
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_signatures = sigs;
    }

    let outputs: Vec<Arc<OutputCapture>> = current_outputs
        .into_iter()
        .filter(|o| o.source_width() > 0 && o.device().is_some() && !o.disposed())
        .collect();
    if outputs.is_empty() {
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error = Some("source not ready".to_string());
        return;
    }

    let out_infos: Vec<_> = outputs.iter().map(|o| o.info().clone()).collect();
    let candidates = EncoderPlan::candidates(
        &GpuDevices::adapters(),
        &out_infos,
        Some(&cfg.encoder_adapter),
        Some(&cfg.encoder),
        EncoderFactory::available,
    );
    if candidates.is_empty() {
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error = Some("no hardware encoder".to_string());
        return;
    }

    let per_monitor = cfg.layout.eq_ignore_ascii_case("perMonitor");
    let mut groups: Vec<(String, Vec<Arc<OutputCapture>>, i32)> = Vec::new();
    if per_monitor {
        for o in &outputs {
            let key = format!("mon:{}", o.info().device_name);
            let fps = ReplayRate::effective(cfg.fps, o.info().refresh_hz);
            groups.push((key, vec![o.clone()], fps));
        }
    } else {
        let max_refresh = outputs
            .iter()
            .map(|o| o.info().refresh_hz)
            .max()
            .unwrap_or(60);
        let fps = ReplayRate::effective(cfg.fps, max_refresh);
        groups.push(("canvas".to_string(), outputs, fps));
    }

    if cfg.lock_memory {
        MemoryIsolation::reserve_working_set(ReplayEngine::locked_bytes(
            cfg.kbps,
            cfg.seconds,
            groups.len() as i32,
        ));
    }

    let mut new_pipelines = Vec::new();
    let mut failure_error: Option<String> = None;

    for (key, group, fps) in &groups {
        let capacity = ReplayEngine::ring_capacity(cfg.kbps, cfg.seconds) as usize;
        let stream = {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let need_new = st
                .streams
                .get(key)
                .is_none_or(|s| s.ring().capacity() != capacity);
            if need_new {
                match VideoStream::new(
                    key.clone(),
                    capacity,
                    Duration::from_secs(cfg.seconds as u64),
                    cfg.lock_memory,
                ) {
                    Ok(s) => {
                        let arc = Arc::new(s);
                        st.streams.insert(key.clone(), arc.clone());
                        arc
                    }
                    Err(e) => {
                        failure_error = Some(e);
                        break;
                    }
                }
            } else {
                st.streams.get(key).unwrap().clone()
            }
        };

        let mut built: Option<ReplayPipeline> = None;
        let mut last_err = "pipeline creation failed".to_string();
        for candidate in &candidates {
            let spec = PipelineSpec {
                key: key.clone(),
                sources: group.clone(),
                candidate: candidate.clone(),
                fps: *fps,
                kbps: cfg.kbps,
                resolution: cfg.resolution.clone(),
                scale_percent: cfg.scale_percent,
            };
            match ReplayPipeline::create(spec, stream.clone(), &shared.devices) {
                Ok(p) => {
                    built = Some(p);
                    break;
                }
                Err(e) => {
                    AgentLog::info(&format!(
                        "replay pipeline candidate {}/{} failed for {key}: {e}",
                        candidate.codec, candidate.api
                    ));
                    last_err = e;
                }
            }
        }
        match built {
            Some(p) => new_pipelines.push(p),
            None => {
                failure_error = Some(last_err);
                break;
            }
        }
    }

    if let Some(err) = failure_error {
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error = Some(err);
        drop(new_pipelines);
        return;
    }

    let active_keys: HashSet<String> = groups.iter().map(|(k, _, _)| k.clone()).collect();
    {
        let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        st.last_error = None;
        st.built_at_ms = tick_count64();
        st.streams.retain(|k, _| active_keys.contains(k));
    }

    for p in &new_pipelines {
        for src in &p.spec.sources {
            src.set_replay_poll_fps(p.spec.fps);
        }
        AgentLog::info(&format!(
            "replay pipeline {}: {} {} '{}' {}x{}@{} {} kbps",
            p.spec.key,
            p.encoder.codec(),
            p.encoder.api(),
            p.encoder.name(),
            p.layout.width,
            p.layout.height,
            p.spec.fps,
            p.spec.kbps
        ));
    }
    pipelines.extend(new_pipelines);
    shared
        .pipelines_count
        .store(pipelines.len() as i32, Ordering::Release);
}

fn tick_pipelines(
    shared: &EngineShared,
    pipelines: &mut [ReplayPipeline],
    timer: &mut HighResTimer,
    freq: i64,
) {
    if pipelines.is_empty() {
        shared.ticking.store(false, Ordering::Release);
        thread::sleep(Duration::from_millis(100));
        return;
    }

    shared.ticking.store(true, Ordering::Release);
    let now = qpc();
    let masks = (shared.masks)();

    for p in pipelines.iter_mut() {
        for src in &p.spec.sources {
            if src.replay_poll_fps() != p.spec.fps {
                src.set_replay_poll_fps(p.spec.fps);
            }
        }
        if let Some(pts) = p.pacer.try_tick(now) {
            p.tick(pts, &masks, false);
        }
    }

    let earliest = pipelines
        .iter()
        .map(|p| p.pacer.next_deadline())
        .min()
        .unwrap_or(now);
    let max_deadline = now + freq / 10;
    let target = earliest.min(max_deadline);
    if target > now {
        let hundred_ns = FramePacer::to_hundred_ns(target - now, freq);
        timer.sleep(hundred_ns);
    }
}
