// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Full hardware, worker inference, encoder, and capture benchmark (`Host\Bench.cs`).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{
    BenchReport, BenchResult, CaptureBench, EncoderBench, HardwareInfo, Settings,
};
use br_core::CancelToken;
use chrono::{DateTime, Utc};
use thiserror::Error;
use zeroize::Zeroize;

use crate::capture::{
    AdapterInfo, CaptureService, EncodeLevel, GpuDeviceCache, GpuDevices, KeyframeEncoder,
    KeyframeGrab, KeyframeRequest, OutputCapture,
};
use crate::host::{HardwareProbe, OutputDesc};
use crate::native::{Rect, process_cpu_ms, process_memory_mb, qpc, qpc_frequency};
use crate::replay::{
    EncoderCandidate, EncoderFactory, EncoderPlan, FramePacer, PipelineSpec, ReplayEngine,
    ReplayPipeline, VideoCodec, VideoStream,
};
use crate::util::{AgentLog, HighResTimer};

/// One encoder benchmark configuration (`EncoderBenchSpec`).
#[derive(Debug, Clone)]
pub struct EncoderBenchSpec {
    pub adapter: AdapterInfo,
    pub codec: VideoCodec,
    pub api: String,
    pub layout: String,
    pub resolution: String,
    pub fps: i32,
}

/// One usable `(adapter, codec, api)` option returned by [`BenchTarget::encoder_options`].
pub type EncoderOption = (AdapterInfo, VideoCodec, String);

/// Error returned by [`BenchTarget`] methods and [`BenchRunner::run`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BenchError {
    #[error("Operation cancelled.")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

/// Everything the benchmark touches, so the orchestration can be tested with fakes (`IBenchTarget`).
pub trait BenchTarget {
    fn hardware(&self) -> HardwareInfo;
    fn worker_bench(&self, ct: &CancelToken) -> Result<Option<BenchResult>, BenchError>;
    /// Usable adapter x codec pairs with the API to use (first available of `EncoderPlan::apis`).
    fn encoder_options(&self) -> Vec<EncoderOption>;
    fn max_refresh_hz(&self) -> i32;
    fn monitor_count(&self) -> i32;
    fn pause_replay(&self);
    fn resume_replay(&self);
    fn run_encoder(
        &self,
        spec: &EncoderBenchSpec,
        duration: Duration,
        ct: &CancelToken,
    ) -> Result<EncoderBench, BenchError>;
    fn run_capture(&self, ct: &CancelToken) -> Result<Option<CaptureBench>, BenchError>;
}

struct ResumeGuard<'a>(&'a dyn BenchTarget);

impl Drop for ResumeGuard<'_> {
    fn drop(&mut self) {
        self.0.resume_replay();
    }
}

/// Adaptive benchmark orchestrator (`BenchRunner`).
pub struct BenchRunner;

impl BenchRunner {
    pub const PROBE_FPS: i32 = 60;
    pub const FALLBACK_FPS: i32 = 30;
    /// Error prefix of a run whose encoder did not keep up with the requested frame rate.
    pub const SLOW_PREFIX: &'static str = "keeps up at ";

    /// Adaptive encoder plan, a handful of runs instead of a full grid (the recommender scales
    /// from the nearest row):
    /// 1. every adapter x codec option: composited, native size, 60 fps; 30 fps when 60 does not keep up;
    /// 2. per adapter, its first passing option per monitor (only with more than one monitor);
    /// 3. the first option that held 60 fps at the highest refresh rate, when that is above 60.
    ///
    /// Every run feeds moving synthetic content, so a static desktop does not make encoders look free.
    pub fn run(
        target: &dyn BenchTarget,
        per_run: Duration,
        clock: impl Fn() -> DateTime<Utc>,
        ct: &CancelToken,
    ) -> Result<BenchReport, BenchError> {
        let sw = Instant::now();
        let ts = clock();

        let hardware = target.hardware();

        let inference = match target.worker_bench(ct) {
            Ok(res) => res,
            Err(BenchError::Cancelled) => return Err(BenchError::Cancelled),
            Err(BenchError::Failed(msg)) => {
                AgentLog::warn_err("worker bench", &msg);
                None
            }
        };
        AgentLog::info(&format!(
            "bench: inference done at {} ms",
            sw.elapsed().as_millis()
        ));

        let mut encoders: Vec<EncoderBench> = Vec::new();
        let mut capture: Option<CaptureBench> = None;

        target.pause_replay();
        {
            let _resume = ResumeGuard(target);

            let mut run_one = |spec: EncoderBenchSpec| -> Result<EncoderBench, BenchError> {
                if ct.is_cancelled() {
                    return Err(BenchError::Cancelled);
                }
                let r = match target.run_encoder(&spec, per_run, ct) {
                    Ok(r) => r,
                    Err(BenchError::Cancelled) => return Err(BenchError::Cancelled),
                    Err(BenchError::Failed(msg)) => EncoderBench {
                        adapter: spec.adapter.name.clone(),
                        codec: EncoderFactory::codec_name(spec.codec).to_string(),
                        api: spec.api.clone(),
                        layout: spec.layout.clone(),
                        width: 0,
                        height: 0,
                        fps: spec.fps,
                        ok: false,
                        encode_ms_p95: 0.0,
                        cpu_percent: 0.0,
                        working_set_mb: 0,
                        error: Some(msg),
                        resolution: Some(spec.resolution.clone()),
                    },
                };
                let summary = if r.ok {
                    format!("p95 {:.1} ms, cpu {:.1}%", r.encode_ms_p95, r.cpu_percent)
                } else {
                    format!("failed: {}", r.error.as_deref().unwrap_or(""))
                };
                AgentLog::info(&format!(
                    "bench encoder {} {} {} {} {}x{}@{}: {summary}",
                    spec.adapter.name, spec.api, r.codec, spec.layout, r.width, r.height, spec.fps
                ));
                encoders.push(r.clone());
                Ok(r)
            };

            let mut passed: Vec<(EncoderOption, i32)> = Vec::new();
            for opt in target.encoder_options() {
                let r = run_one(EncoderBenchSpec {
                    adapter: opt.0.clone(),
                    codec: opt.1,
                    api: opt.2.clone(),
                    layout: "composited".to_string(),
                    resolution: "native".to_string(),
                    fps: Self::PROBE_FPS,
                })?;
                let mut ok_fps = if r.ok { Self::PROBE_FPS } else { 0 };
                if !r.ok
                    && r.error
                        .as_deref()
                        .is_some_and(|e| e.starts_with(Self::SLOW_PREFIX))
                {
                    let retry = run_one(EncoderBenchSpec {
                        adapter: opt.0.clone(),
                        codec: opt.1,
                        api: opt.2.clone(),
                        layout: "composited".to_string(),
                        resolution: "native".to_string(),
                        fps: Self::FALLBACK_FPS,
                    })?;
                    if retry.ok {
                        ok_fps = Self::FALLBACK_FPS;
                    }
                }
                if ok_fps > 0 {
                    passed.push((opt, ok_fps));
                }
            }

            if target.monitor_count() > 1 {
                let mut seen_luids: Vec<i64> = Vec::new();
                for (opt, fps) in &passed {
                    if !seen_luids.contains(&opt.0.luid) {
                        seen_luids.push(opt.0.luid);
                        let _ = run_one(EncoderBenchSpec {
                            adapter: opt.0.clone(),
                            codec: opt.1,
                            api: opt.2.clone(),
                            layout: "perMonitor".to_string(),
                            resolution: "native".to_string(),
                            fps: *fps,
                        })?;
                    }
                }
            }

            let max_hz = target.max_refresh_hz();
            if max_hz > Self::PROBE_FPS
                && let Some((opt, _)) = passed.iter().find(|(_, fps)| *fps == Self::PROBE_FPS)
            {
                let _ = run_one(EncoderBenchSpec {
                    adapter: opt.0.clone(),
                    codec: opt.1,
                    api: opt.2.clone(),
                    layout: "composited".to_string(),
                    resolution: "native".to_string(),
                    fps: max_hz,
                })?;
            }
            AgentLog::info(&format!(
                "bench: {} encoder runs done at {} ms",
                encoders.len(),
                sw.elapsed().as_millis()
            ));

            match target.run_capture(ct) {
                Ok(c) => capture = c,
                Err(BenchError::Cancelled) => return Err(BenchError::Cancelled),
                Err(BenchError::Failed(msg)) => AgentLog::warn_err("capture bench", &msg),
            }
        }

        Ok(BenchReport {
            ts,
            took_ms: sw.elapsed().as_millis() as i64,
            hardware,
            inference,
            encoders,
            capture,
            search: None,
            recommendations: Vec::new(),
        })
    }
}

type OutputsFn = Arc<dyn Fn() -> Vec<Arc<OutputCapture>> + Send + Sync>;
type WorkerBenchFn = Arc<dyn Fn(&CancelToken) -> Result<Option<BenchResult>, BenchError> + Send + Sync>;
type SettingsFn = Arc<dyn Fn() -> Settings + Send + Sync>;
type EncoderCacheKey = (i64, VideoCodec, String, String, i32, String);

struct PollResetGuard<'a>(&'a [Arc<OutputCapture>]);

impl Drop for PollResetGuard<'_> {
    fn drop(&mut self) {
        for s in self.0 {
            s.set_replay_poll_fps(0);
        }
    }
}

struct GrabOverrideGuard<'a>(&'a [Arc<OutputCapture>]);

impl Drop for GrabOverrideGuard<'_> {
    fn drop(&mut self) {
        for o in self.0 {
            // D4: drop any unconsumed bench request before removing the override.
            o.clear_pending_bench_keyframe();
            o.set_grab_override(None);
        }
    }
}

/// Live [`BenchTarget`] backed by [`OutputCapture`]s, [`ReplayEngine`], and worker client (`AgentBenchTarget`).
pub struct AgentBenchTarget {
    outputs: OutputsFn,
    engine: Arc<ReplayEngine>,
    worker_bench: WorkerBenchFn,
    settings: SettingsFn,
    devices: Arc<GpuDeviceCache>,
    encoder_cache: Mutex<HashMap<EncoderCacheKey, EncoderBench>>,
}

impl AgentBenchTarget {
    pub fn from_capture(
        capture: Arc<CaptureService>,
        worker_bench: WorkerBenchFn,
        settings: SettingsFn,
        devices: Arc<GpuDeviceCache>,
    ) -> Result<Self, BenchError> {
        let engine = capture
            .engine()
            .ok_or_else(|| BenchError::Failed("Replay engine is not running.".to_string()))?;
        let cap_out = capture;
        Ok(Self::new(
            Arc::new(move || cap_out.outputs()),
            engine,
            worker_bench,
            settings,
            devices,
        ))
    }

    /// Without a `CaptureService`: the dev self-test drives its own captures and replay engine.
    pub fn new(
        outputs: OutputsFn,
        engine: Arc<ReplayEngine>,
        worker_bench: WorkerBenchFn,
        settings: SettingsFn,
        devices: Arc<GpuDeviceCache>,
    ) -> Self {
        Self {
            outputs,
            engine,
            worker_bench,
            settings,
            devices,
            encoder_cache: Mutex::new(HashMap::new()),
        }
    }
}

impl BenchTarget for AgentBenchTarget {
    fn hardware(&self) -> HardwareInfo {
        let descs: Vec<OutputDesc> = (self.outputs)()
            .iter()
            .map(|o| {
                let info = o.info();
                OutputDesc {
                    index: info.index,
                    width: info.width,
                    height: info.height,
                    refresh_hz: info.refresh_hz,
                    adapter_name: info.adapter_name.clone(),
                }
            })
            .collect();
        HardwareProbe::collect(&descs)
    }

    fn worker_bench(&self, ct: &CancelToken) -> Result<Option<BenchResult>, BenchError> {
        (self.worker_bench)(ct)
    }

    fn encoder_options(&self) -> Vec<EncoderOption> {
        let mut list = Vec::new();
        let codecs = [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];
        for adapter in GpuDevices::adapters() {
            if adapter.software {
                continue;
            }
            for codec in codecs {
                for api in EncoderPlan::apis(&adapter, codec) {
                    if EncoderFactory::available(&adapter, codec, api) {
                        list.push((adapter.clone(), codec, api.to_string()));
                        break;
                    }
                }
            }
        }
        list
    }

    fn monitor_count(&self) -> i32 {
        (self.outputs)()
            .iter()
            .filter(|o| o.source_width() > 0 && !o.disposed())
            .count() as i32
    }

    fn max_refresh_hz(&self) -> i32 {
        let outputs = (self.outputs)();
        if outputs.is_empty() {
            return 60;
        }
        outputs
            .iter()
            .map(|o| o.info().refresh_hz)
            .max()
            .unwrap_or(60)
    }

    fn pause_replay(&self) {
        self.engine.suspend();
    }

    fn resume_replay(&self) {
        for o in (self.outputs)() {
            o.set_replay_poll_fps(0);
        }
        self.engine.resume_after_suspend();
    }

    fn run_encoder(
        &self,
        spec: &EncoderBenchSpec,
        duration: Duration,
        ct: &CancelToken,
    ) -> Result<EncoderBench, BenchError> {
        let sources: Vec<Arc<OutputCapture>> = (self.outputs)()
            .into_iter()
            .filter(|o| o.source_width() > 0 && o.device().is_some() && !o.disposed())
            .collect();
        if sources.is_empty() {
            return Err(BenchError::Failed("no captured monitor".to_string()));
        }

        let mut groups: Vec<(String, Vec<Arc<OutputCapture>>)> = Vec::new();
        if spec.layout.eq_ignore_ascii_case("perMonitor") {
            for s in &sources {
                groups.push((format!("bench:{}", s.info().index), vec![s.clone()]));
            }
        } else {
            groups.push(("bench".to_string(), sources.clone()));
        }

        let bpp = match spec.codec {
            VideoCodec::Av1 => 0.035,
            VideoCodec::Hevc => 0.05,
            _ => 0.08,
        };
        let formula_kbps = |pixels: i32, fps: i32| -> i32 {
            let raw = pixels as f64 * fps as f64 * bpp / 1000.0;
            let rounded = (raw / 500.0).round() as i32 * 500;
            rounded.clamp(500, 50_000)
        };

        let mut streams: Vec<Arc<VideoStream>> = Vec::new();
        let mut pipelines: Vec<ReplayPipeline> = Vec::new();
        let mut create_error: Option<String> = None;

        for (key, group) in &groups {
            let candidate = EncoderCandidate {
                adapter: spec.adapter.clone(),
                codec: spec.codec,
                api: spec.api.clone(),
            };
            let stream = match VideoStream::new(key.clone(), 64 << 20, Duration::from_secs(10), false)
            {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    create_error = Some(e);
                    break;
                }
            };
            streams.push(stream.clone());

            let pspec = PipelineSpec {
                key: key.clone(),
                sources: group.clone(),
                candidate: candidate.clone(),
                fps: spec.fps,
                kbps: 6000,
                resolution: spec.resolution.clone(),
                scale_percent: 100,
            };
            let mut pipeline = match ReplayPipeline::create(pspec, stream, &self.devices) {
                Ok(p) => p,
                Err(e) => {
                    create_error = Some(e);
                    break;
                }
            };

            let pixels = pipeline.layout.width * pipeline.layout.height;
            let formula = formula_kbps(pixels, spec.fps);
            if ((formula - 6000).abs() as f64 / 6000.0) > 0.10 {
                drop(pipeline);
                streams.pop();
                let stream2 =
                    match VideoStream::new(key.clone(), 64 << 20, Duration::from_secs(10), false) {
                        Ok(s) => Arc::new(s),
                        Err(e) => {
                            create_error = Some(e);
                            break;
                        }
                    };
                streams.push(stream2.clone());
                let pspec2 = PipelineSpec {
                    key: key.clone(),
                    sources: group.clone(),
                    candidate,
                    fps: spec.fps,
                    kbps: formula,
                    resolution: spec.resolution.clone(),
                    scale_percent: 100,
                };
                pipeline = match ReplayPipeline::create(pspec2, stream2, &self.devices) {
                    Ok(p) => p,
                    Err(e) => {
                        create_error = Some(e);
                        break;
                    }
                };
            }

            pipelines.push(pipeline);
        }

        if create_error.is_some() || pipelines.len() < groups.len() {
            drop(pipelines);
            drop(streams);
            return Ok(EncoderBench {
                adapter: spec.adapter.name.clone(),
                codec: EncoderFactory::codec_name(spec.codec).to_string(),
                api: spec.api.clone(),
                layout: spec.layout.clone(),
                width: 0,
                height: 0,
                fps: spec.fps,
                ok: false,
                encode_ms_p95: 0.0,
                cpu_percent: 0.0,
                working_set_mb: 0,
                error: Some(create_error.unwrap_or_else(|| "pipeline creation failed".to_string())),
                resolution: None,
            });
        }

        let sizes_key = pipelines
            .iter()
            .map(|p| format!("{}x{}", p.layout.width, p.layout.height))
            .collect::<Vec<_>>()
            .join(";");
        let cache_key: EncoderCacheKey = (
            spec.adapter.luid,
            spec.codec,
            spec.api.clone(),
            spec.layout.clone(),
            spec.fps,
            sizes_key,
        );
        if let Some(cached) = self
            .encoder_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&cache_key)
            .cloned()
        {
            drop(pipelines);
            drop(streams);
            return Ok(cached);
        }

        for s in &sources {
            s.set_replay_poll_fps(spec.fps);
        }
        let _poll_reset = PollResetGuard(&sources);
        for p in &pipelines {
            p.synthetic_motion.store(true, Ordering::Release);
        }

        let freq = qpc_frequency();
        let mut packets_at_warm: i64 = 0;
        let timer = HighResTimer::new();
        let sw = Instant::now();
        let mut warmed_up = false;
        let mut cpu_start_ms = 0.0f64;
        let mut wall_start_ticks: i64 = 0;

        while sw.elapsed() < duration {
            if ct.is_cancelled() {
                return Err(BenchError::Cancelled);
            }
            let now = qpc();

            if !warmed_up && sw.elapsed() >= Duration::from_millis(500) {
                warmed_up = true;
                for p in &pipelines {
                    p.encoder.latency().reset();
                }
                packets_at_warm = pipelines.iter().map(|p| p.encoder.packets_out()).sum();
                for p in &pipelines {
                    p.motion_ticks_at_warm
                        .store(p.motion_ticks.load(Ordering::Acquire), Ordering::Release);
                }
                cpu_start_ms = process_cpu_ms();
                wall_start_ticks = qpc();
            }

            for p in &mut pipelines {
                if let Some(pts) = p.pacer.try_tick(now) {
                    p.tick(pts, &[], false);
                }
            }

            let earliest_deadline = pipelines
                .iter()
                .map(|p| p.pacer.next_deadline())
                .min()
                .unwrap_or(i64::MAX);

            let now_after = qpc();
            if earliest_deadline > now_after {
                let hundred_ns = FramePacer::to_hundred_ns(earliest_deadline - now_after, freq);
                timer.sleep(hundred_ns);
            }
        }

        let cpu_now = process_cpu_ms();
        let mut cpu_elapsed_ms = if warmed_up {
            cpu_now - cpu_start_ms
        } else {
            cpu_now
        };
        let motion_ticks: i64 = pipelines
            .iter()
            .map(|p| {
                p.motion_ticks.load(Ordering::Acquire)
                    - p.motion_ticks_at_warm.load(Ordering::Acquire)
            })
            .sum();
        cpu_elapsed_ms -= motion_ticks as f64 * 1000.0 / freq as f64;
        if cpu_elapsed_ms < 0.0 {
            cpu_elapsed_ms = 0.0;
        }
        let wall_ms = if warmed_up {
            (qpc() - wall_start_ticks) as f64 * 1000.0 / freq as f64
        } else {
            sw.elapsed().as_secs_f64() * 1000.0
        };
        let cpu_percent = if wall_ms > 0.0 {
            cpu_elapsed_ms / wall_ms * 100.0
        } else {
            0.0
        };

        let encode_ms_p95 = pipelines
            .iter()
            .map(|p| p.encoder.latency().percentile(95.0))
            .fold(0.0f64, f64::max);
        let (working_set_mb, _) = process_memory_mb();
        let width = pipelines[0].layout.width;
        let height = pipelines[0].layout.height;

        let mut ok = true;
        let mut error: Option<String> = None;
        let total_packets: i64 = pipelines.iter().map(|p| p.encoder.packets_out()).sum();
        let achieved = if wall_ms > 0.0 {
            (total_packets - packets_at_warm) as f64 / pipelines.len() as f64 / (wall_ms / 1000.0)
        } else {
            0.0
        };
        if achieved < spec.fps as f64 * 0.9 {
            ok = false;
            error = Some(format!(
                "{}{achieved:.0} of {} fps",
                BenchRunner::SLOW_PREFIX,
                spec.fps
            ));
        }
        for p in &pipelines {
            if p.encoder.packets_out() <= 0 {
                ok = false;
                error = Some(
                    p.encoder
                        .error()
                        .unwrap_or_else(|| "no packets produced".to_string()),
                );
                break;
            }
            if let Some(err) = p.encoder.error()
                && !err.is_empty()
            {
                ok = false;
                error = Some(err);
                break;
            }
        }

        let result = EncoderBench {
            adapter: spec.adapter.name.clone(),
            codec: EncoderFactory::codec_name(spec.codec).to_string(),
            api: spec.api.clone(),
            layout: spec.layout.clone(),
            width,
            height,
            fps: spec.fps,
            ok,
            encode_ms_p95,
            cpu_percent,
            working_set_mb,
            error,
            resolution: Some(spec.resolution.clone()),
        };

        self.encoder_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(cache_key, result.clone());
        Ok(result)
    }

    fn run_capture(&self, ct: &CancelToken) -> Result<Option<CaptureBench>, BenchError> {
        let settings = (self.settings)();
        let outputs: Vec<Arc<OutputCapture>> = (self.outputs)()
            .into_iter()
            .filter(|o| o.source_width() > 0 && o.device().is_some() && !o.disposed())
            .collect();
        if outputs.is_empty() {
            return Ok(None);
        }

        let collected: Arc<Mutex<Vec<KeyframeGrab>>> = Arc::new(Mutex::new(Vec::new()));
        let initial_stats: Vec<(Arc<OutputCapture>, u64, u64)> = outputs
            .iter()
            .map(|o| (o.clone(), o.grab_ticks(), o.grab_count()))
            .collect();

        for o in &outputs {
            let bag = collected.clone();
            o.set_grab_override(Some(Arc::new(move |grab| {
                bag.lock().unwrap_or_else(|e| e.into_inner()).push(grab);
            })));
        }
        let _override_guard = GrabOverrideGuard(&outputs);

        let max_width = settings.capture.max_width.clamp(640, 3840);
        for i in 0..3 {
            if ct.is_cancelled() {
                return Err(BenchError::Cancelled);
            }
            let req = KeyframeRequest {
                ts: Utc::now(),
                app: String::new(),
                exe_path: String::new(),
                title: String::new(),
                context: String::new(),
                force: true,
                max_width,
                dirty_min_percent: 0.0,
                site: String::new(),
                keyframe: true,
                replay_sample: false,
                trigger: false,
                secrets: None,
                bounds: Rect::default(),
                mask_only: false,
                bench: true,
            };
            for o in &outputs {
                o.set_pending_keyframe(Some(req.clone()));
            }
            if i < 2 {
                for _ in 0..6 {
                    if ct.is_cancelled() {
                        return Err(BenchError::Cancelled);
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }

        let freq = qpc_frequency();
        let wait_start = qpc();
        let expected = outputs.len() * 3;
        while qpc() - wait_start < freq * 3 {
            if ct.is_cancelled() {
                return Err(BenchError::Cancelled);
            }
            if collected.lock().unwrap_or_else(|e| e.into_inner()).len() >= expected {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let mut sum_tick_ms = 0.0f64;
        let mut active_outputs = 0usize;
        for (o, init_ticks, init_count) in &initial_stats {
            let d_ticks = o.grab_ticks().saturating_sub(*init_ticks);
            let d_count = o.grab_count().saturating_sub(*init_count);
            if d_count > 0 {
                sum_tick_ms += (d_ticks as f64 * 1000.0 / freq as f64) / d_count as f64;
                active_outputs += 1;
            }
        }
        let tick_ms = if active_outputs > 0 {
            sum_tick_ms / active_outputs as f64
        } else {
            0.0
        };

        let grabs: Vec<KeyframeGrab> = std::mem::take(
            &mut *collected.lock().unwrap_or_else(|e| e.into_inner()),
        );
        let mut total_encode_ms = 0.0f64;
        let mut encode_count = 0usize;
        let mut keyframe_encoder = String::new();
        for grab in grabs {
            let t0 = qpc();
            let enc = unsafe {
                KeyframeEncoder::encode(
                    grab.pixels().as_ptr(),
                    grab.width,
                    grab.height,
                    grab.stride,
                    &settings,
                    EncodeLevel::Normal,
                )
            };
            if let Ok((mut bytes, format, _, _)) = enc {
                bytes.zeroize();
                keyframe_encoder = KeyframeEncoder::describe(format, EncodeLevel::Normal);
                total_encode_ms += (qpc() - t0) as f64 * 1000.0 / freq as f64;
                encode_count += 1;
            }
        }
        let keyframe_encode_ms = if encode_count > 0 {
            total_encode_ms / encode_count as f64
        } else {
            0.0
        };

        let mut cross_adapter_ms = 0.0f64;
        let options = self.encoder_options();
        if let Some(first) = options.first()
            && let Ok(cross_stream) =
                VideoStream::new("bench:cross".to_string(), 64 << 20, Duration::from_secs(10), false)
        {
            let cross_stream = Arc::new(cross_stream);
            let spec = PipelineSpec {
                key: "bench:cross".to_string(),
                sources: outputs.clone(),
                candidate: EncoderCandidate {
                    adapter: first.0.clone(),
                    codec: first.1,
                    api: first.2.clone(),
                },
                fps: 30,
                kbps: 6000,
                resolution: "native".to_string(),
                scale_percent: 100,
            };
            if let Ok(mut cross_pipeline) =
                ReplayPipeline::create(spec, cross_stream, &self.devices)
            {
                for o in &outputs {
                    o.set_replay_poll_fps(30);
                }
                let _poll_reset = PollResetGuard(&outputs);
                let timer = HighResTimer::new();
                let sw = Instant::now();
                while sw.elapsed() < Duration::from_secs(2) {
                    if ct.is_cancelled() {
                        return Err(BenchError::Cancelled);
                    }
                    let now = qpc();
                    if let Some(pts) = cross_pipeline.pacer.try_tick(now) {
                        cross_pipeline.tick(pts, &[], true);
                    }
                    let deadline = cross_pipeline.pacer.next_deadline();
                    let now_after = qpc();
                    if deadline > now_after {
                        let hundred_ns = FramePacer::to_hundred_ns(deadline - now_after, freq);
                        timer.sleep(hundred_ns);
                    }
                }
                if cross_pipeline.copy_count > 0 {
                    cross_adapter_ms = (cross_pipeline.copy_ticks as f64 * 1000.0 / freq as f64)
                        / cross_pipeline.copy_count as f64;
                }
            }
        }

        Ok(Some(CaptureBench {
            tick_ms,
            keyframe_encode_ms,
            cross_adapter_ms,
            keyframe_encoder: Some(keyframe_encoder),
        }))
    }
}
