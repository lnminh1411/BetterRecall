// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Port of `tests\BetterRecall.Agent.Tests\BenchTests.cs` (8 tests).

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use br_agent::capture::{AdapterInfo, GpuDevices};
use br_agent::host::{BenchError, BenchRunner, BenchTarget, EncoderBenchSpec, EncoderOption};
use br_agent::replay::{EncoderFactory, VideoCodec};
use br_contracts::{BenchDevice, BenchResult, CaptureBench, EncoderBench, HardwareInfo};
use br_core::{CancelSource, CancelToken};
use chrono::Utc;

type HardwareFn = Box<dyn Fn() -> HardwareInfo + Send + Sync>;
type WorkerBenchFn = Box<dyn Fn(&CancelToken) -> Result<Option<BenchResult>, BenchError> + Send + Sync>;
type OptionsFn = Box<dyn Fn() -> Vec<EncoderOption> + Send + Sync>;
type IntFn = Box<dyn Fn() -> i32 + Send + Sync>;
type RunEncoderFn =
    Box<dyn Fn(&EncoderBenchSpec, Duration, &CancelToken) -> Result<EncoderBench, BenchError> + Send + Sync>;
type RunCaptureFn = Box<dyn Fn(&CancelToken) -> Result<Option<CaptureBench>, BenchError> + Send + Sync>;

struct FakeBenchTarget {
    pause_count: AtomicI32,
    resume_count: AtomicI32,
    run_encoder_calls: Mutex<Vec<EncoderBenchSpec>>,
    on_hardware: HardwareFn,
    on_worker_bench: WorkerBenchFn,
    on_encoder_options: OptionsFn,
    on_max_refresh_hz: IntFn,
    on_monitor_count: IntFn,
    on_run_encoder: RunEncoderFn,
    on_run_capture: RunCaptureFn,
}

impl Default for FakeBenchTarget {
    fn default() -> Self {
        Self {
            pause_count: AtomicI32::new(0),
            resume_count: AtomicI32::new(0),
            run_encoder_calls: Mutex::new(Vec::new()),
            on_hardware: Box::new(|| HardwareInfo {
                cpu: "FakeCPU".to_string(),
                cores: 4,
                threads: 8,
                ram_mb: 16384,
                has_battery: false,
                gpus: Vec::new(),
                npu: None,
                monitors: Vec::new(),
                os: "Windows".to_string(),
            }),
            on_worker_bench: Box::new(|_| {
                Ok(Some(BenchResult {
                    chosen: "npu".to_string(),
                    devices: vec![BenchDevice {
                        device: "npu".to_string(),
                        ep: "OpenVINO".to_string(),
                        ok: true,
                        image_ms: 10.0,
                        text_ms: 5.0,
                        cosine: 0.95,
                        error: None,
                        model: None,
                        text_device: None,
                        compile_ms: None,
                        adapter: None,
                        adapter_auto: false,
                    }],
                    tokens: None,
                    chosen_adapter: None,
                }))
            }),
            on_encoder_options: Box::new(|| {
                vec![
                    (
                        AdapterInfo {
                            luid: 1,
                            vendor_id: GpuDevices::VENDOR_INTEL,
                            name: "Intel".to_string(),
                            software: false,
                            integrated: false,
                            vram_mb: 0,
                            device_id: 0,
                        },
                        VideoCodec::Av1,
                        "vpl".to_string(),
                    ),
                    (
                        AdapterInfo {
                            luid: 2,
                            vendor_id: GpuDevices::VENDOR_NVIDIA,
                            name: "NVIDIA".to_string(),
                            software: false,
                            integrated: false,
                            vram_mb: 0,
                            device_id: 0,
                        },
                        VideoCodec::Hevc,
                        "mf".to_string(),
                    ),
                ]
            }),
            on_max_refresh_hz: Box::new(|| 60),
            on_monitor_count: Box::new(|| 2),
            on_run_encoder: Box::new(|spec, _, _| {
                Ok(EncoderBench {
                    adapter: spec.adapter.name.clone(),
                    codec: EncoderFactory::codec_name(spec.codec).to_string(),
                    api: spec.api.clone(),
                    layout: spec.layout.clone(),
                    width: 1920,
                    height: 1080,
                    fps: spec.fps,
                    ok: true,
                    encode_ms_p95: 4.5,
                    cpu_percent: 8.0,
                    working_set_mb: 400,
                    error: None,
                    resolution: None,
                })
            }),
            on_run_capture: Box::new(|_| {
                Ok(Some(CaptureBench {
                    tick_ms: 1.2,
                    keyframe_encode_ms: 3.4,
                    cross_adapter_ms: 0.8,
                    keyframe_encoder: None,
                }))
            }),
        }
    }
}

impl FakeBenchTarget {
    fn pause_count(&self) -> i32 {
        self.pause_count.load(Ordering::Acquire)
    }

    fn resume_count(&self) -> i32 {
        self.resume_count.load(Ordering::Acquire)
    }

    fn calls(&self) -> Vec<EncoderBenchSpec> {
        self.run_encoder_calls.lock().unwrap().clone()
    }
}

impl BenchTarget for FakeBenchTarget {
    fn hardware(&self) -> HardwareInfo {
        (self.on_hardware)()
    }

    fn worker_bench(&self, ct: &CancelToken) -> Result<Option<BenchResult>, BenchError> {
        (self.on_worker_bench)(ct)
    }

    fn encoder_options(&self) -> Vec<EncoderOption> {
        (self.on_encoder_options)()
    }

    fn max_refresh_hz(&self) -> i32 {
        (self.on_max_refresh_hz)()
    }

    fn monitor_count(&self) -> i32 {
        (self.on_monitor_count)()
    }

    fn pause_replay(&self) {
        self.pause_count.fetch_add(1, Ordering::AcqRel);
    }

    fn resume_replay(&self) {
        self.resume_count.fetch_add(1, Ordering::AcqRel);
    }

    fn run_encoder(
        &self,
        spec: &EncoderBenchSpec,
        duration: Duration,
        ct: &CancelToken,
    ) -> Result<EncoderBench, BenchError> {
        self.run_encoder_calls.lock().unwrap().push(spec.clone());
        (self.on_run_encoder)(spec, duration, ct)
    }

    fn run_capture(&self, ct: &CancelToken) -> Result<Option<CaptureBench>, BenchError> {
        (self.on_run_capture)(ct)
    }
}

#[test]
fn run_adaptive_plan_probes_each_option_then_per_monitor_then_max_refresh() {
    let fake = FakeBenchTarget {
        on_max_refresh_hz: Box::new(|| 144),
        on_monitor_count: Box::new(|| 3),
        ..Default::default()
    };
    let report = BenchRunner::run(&fake, Duration::from_secs(1), Utc::now, &CancelToken::none())
        .unwrap();

    let calls = fake.calls();
    assert_eq!(5, calls.len());
    for c in &calls[..2] {
        assert!(c.layout == "composited" && c.fps == 60 && c.resolution == "native");
    }
    let pm_adapters: Vec<&str> = calls[2..4].iter().map(|c| c.adapter.name.as_str()).collect();
    assert_eq!(vec!["Intel", "NVIDIA"], pm_adapters);
    for c in &calls[2..4] {
        assert_eq!("perMonitor", c.layout);
    }
    assert_eq!(
        ("Intel", 144, "composited"),
        (
            calls[4].adapter.name.as_str(),
            calls[4].fps,
            calls[4].layout.as_str()
        )
    );
    assert_eq!(5, report.encoders.len());
}

#[test]
fn run_slow_at_60_retries_at_30_and_single_monitor_skips_per_monitor() {
    let fake = FakeBenchTarget {
        on_monitor_count: Box::new(|| 1),
        on_run_encoder: Box::new(|spec, _, _| {
            let slow = spec.adapter.name == "Intel" && spec.fps == 60;
            Ok(EncoderBench {
                adapter: spec.adapter.name.clone(),
                codec: EncoderFactory::codec_name(spec.codec).to_string(),
                api: spec.api.clone(),
                layout: spec.layout.clone(),
                width: 1920,
                height: 1080,
                fps: spec.fps,
                ok: !slow,
                encode_ms_p95: 4.0,
                cpu_percent: 5.0,
                working_set_mb: 300,
                error: if slow {
                    Some(format!("{}41 of 60 fps", BenchRunner::SLOW_PREFIX))
                } else {
                    None
                },
                resolution: None,
            })
        }),
        ..Default::default()
    };
    BenchRunner::run(&fake, Duration::from_secs(1), Utc::now, &CancelToken::none()).unwrap();

    let seq: Vec<(String, i32)> = fake
        .calls()
        .into_iter()
        .map(|c| (c.adapter.name, c.fps))
        .collect();
    assert_eq!(
        vec![
            ("Intel".to_string(), 60),
            ("Intel".to_string(), 30),
            ("NVIDIA".to_string(), 60),
        ],
        seq
    );
}

#[test]
fn run_when_run_encoder_throws_for_one_adapter_records_ok_false_and_continues() {
    let fake = FakeBenchTarget {
        on_run_encoder: Box::new(|spec, _, _| {
            if spec.adapter.name == "Intel" {
                return Err(BenchError::Failed("Intel encoder crashed".to_string()));
            }
            Ok(EncoderBench {
                adapter: spec.adapter.name.clone(),
                codec: EncoderFactory::codec_name(spec.codec).to_string(),
                api: spec.api.clone(),
                layout: spec.layout.clone(),
                width: 1920,
                height: 1080,
                fps: spec.fps,
                ok: true,
                encode_ms_p95: 4.0,
                cpu_percent: 5.0,
                working_set_mb: 300,
                error: None,
                resolution: None,
            })
        }),
        ..Default::default()
    };

    let report = BenchRunner::run(&fake, Duration::from_secs(1), Utc::now, &CancelToken::none())
        .unwrap();

    assert_eq!(3, report.encoders.len());
    for enc in &report.encoders {
        if enc.adapter == "Intel" {
            assert!(!enc.ok);
            assert_eq!(Some("Intel encoder crashed"), enc.error.as_deref());
        } else {
            assert!(enc.ok);
        }
    }

    assert_eq!(1, fake.pause_count());
    assert_eq!(1, fake.resume_count());
    assert!(report.search.is_none());
    assert!(report.recommendations.is_empty());
    let inf = report.inference.as_ref().expect("inference present");
    assert_eq!("npu", inf.chosen);
}

#[test]
fn run_when_worker_bench_throws_continues_with_null_inference() {
    let fake = FakeBenchTarget {
        on_worker_bench: Box::new(|_| {
            Err(BenchError::Failed("Worker connection lost".to_string()))
        }),
        ..Default::default()
    };

    let report = BenchRunner::run(&fake, Duration::from_secs(1), Utc::now, &CancelToken::none())
        .unwrap();

    assert!(report.inference.is_none());
    assert_eq!(4, report.encoders.len());
    assert_eq!(1, fake.pause_count());
    assert_eq!(1, fake.resume_count());
}

#[test]
fn run_when_cancelled_throws_and_resumes_replay() {
    let cts = Arc::new(CancelSource::new());
    let cts_cb = cts.clone();
    let call_count = Arc::new(AtomicI32::new(0));
    let count_cb = call_count.clone();

    let fake = FakeBenchTarget {
        on_run_encoder: Box::new(move |spec, _, _| {
            let n = count_cb.fetch_add(1, Ordering::AcqRel) + 1;
            if n == 3 {
                cts_cb.cancel();
                return Err(BenchError::Cancelled);
            }
            Ok(EncoderBench {
                adapter: spec.adapter.name.clone(),
                codec: EncoderFactory::codec_name(spec.codec).to_string(),
                api: spec.api.clone(),
                layout: spec.layout.clone(),
                width: 1920,
                height: 1080,
                fps: spec.fps,
                ok: true,
                encode_ms_p95: 4.0,
                cpu_percent: 5.0,
                working_set_mb: 300,
                error: None,
                resolution: None,
            })
        }),
        ..Default::default()
    };

    let err = BenchRunner::run(&fake, Duration::from_secs(1), Utc::now, &cts.token())
        .expect_err("should cancel");
    assert_eq!(BenchError::Cancelled, err);
    assert_eq!(1, fake.pause_count());
    assert_eq!(1, fake.resume_count());
}

#[test]
fn run_when_run_capture_throws_capture_null_and_resumes_replay() {
    let fake = FakeBenchTarget {
        on_run_capture: Box::new(|_| Err(BenchError::Failed("Capture failed".to_string()))),
        ..Default::default()
    };

    let report = BenchRunner::run(&fake, Duration::from_secs(1), Utc::now, &CancelToken::none())
        .unwrap();

    assert!(report.capture.is_none());
    assert_eq!(1, fake.pause_count());
    assert_eq!(1, fake.resume_count());
}
