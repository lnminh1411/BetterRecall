// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Port of `tests\BetterRecall.Agent.Tests\BenchTests.cs` (8 tests).

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use br_agent::capture::{AdapterInfo, GpuDevices, OutputInfo};
use br_agent::host::{BenchError, BenchRunner, BenchTarget, EncoderBenchSpec, EncoderOption};
use br_agent::replay::{
    CanvasLayout, EncoderFactory, EncoderPlan, MonitorPlacement, VideoCodec,
};
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

fn make_output(index: i32, name: &str, luid: i64, vendor_id: u32, adapter_name: &str, left: i32) -> OutputInfo {
    OutputInfo {
        index,
        device_name: name.to_string(),
        adapter_luid: luid,
        vendor_id,
        adapter_name: adapter_name.to_string(),
        left,
        top: 0,
        width: 1920,
        height: 1080,
        monitor: 0,
        refresh_hz: 60,
    }
}

#[test]
fn encoder_plan_candidates_selection_order() {
    // (a) Intel iGPU (Integrated) + NVIDIA + AMD
    let intel = AdapterInfo {
        luid: 1,
        vendor_id: GpuDevices::VENDOR_INTEL,
        name: "Intel Iris Xe".to_string(),
        software: false,
        integrated: true,
        vram_mb: 0,
        device_id: 0,
    };
    let nvidia = AdapterInfo {
        luid: 2,
        vendor_id: GpuDevices::VENDOR_NVIDIA,
        name: "NVIDIA GeForce RTX 4080".to_string(),
        software: false,
        integrated: false,
        vram_mb: 16384,
        device_id: 0,
    };
    let amd = AdapterInfo {
        luid: 3,
        vendor_id: GpuDevices::VENDOR_AMD,
        name: "AMD Radeon RX 7900".to_string(),
        software: false,
        integrated: false,
        vram_mb: 24576,
        device_id: 0,
    };
    let adapters = [intel.clone(), nvidia.clone(), amd.clone()];
    let outputs = [
        make_output(0, "Display1", 3, GpuDevices::VENDOR_AMD, "AMD Radeon RX 7900", 0),
        make_output(1, "Display2", 2, GpuDevices::VENDOR_NVIDIA, "NVIDIA GeForce RTX 4080", 1920),
        make_output(2, "Display3", 2, GpuDevices::VENDOR_NVIDIA, "NVIDIA GeForce RTX 4080", 3840),
    ];

    let available_a = |a: &AdapterInfo, c: VideoCodec, api: &str| {
        matches!(
            (a.luid, c, api),
            (1, VideoCodec::Av1, "vpl")
                | (1, VideoCodec::Hevc, "mf")
                | (1, VideoCodec::H264, "mf")
                | (2, VideoCodec::Hevc, "mf")
                | (2, VideoCodec::H264, "mf")
                | (3, VideoCodec::Hevc, "mf")
                | (3, VideoCodec::H264, "mf")
        )
    };

    let candidates_a =
        EncoderPlan::candidates(&adapters, &outputs, Some("auto"), Some("auto"), available_a);
    let expected_a = [
        (1i64, VideoCodec::Av1, "vpl"),
        (1, VideoCodec::Hevc, "mf"),
        (1, VideoCodec::H264, "mf"),
        (2, VideoCodec::Hevc, "mf"),
        (2, VideoCodec::H264, "mf"),
        (3, VideoCodec::Hevc, "mf"),
        (3, VideoCodec::H264, "mf"),
    ];
    assert_eq!(expected_a.len(), candidates_a.len());
    for (i, &(luid, codec, api)) in expected_a.iter().enumerate() {
        assert_eq!(luid, candidates_a[i].adapter.luid);
        assert_eq!(codec, candidates_a[i].codec);
        assert_eq!(api, candidates_a[i].api);
    }

    // (b) NVIDIA-only (not integrated)
    let outputs_b = [make_output(
        0,
        "Display1",
        2,
        GpuDevices::VENDOR_NVIDIA,
        "NVIDIA GeForce RTX 4080",
        0,
    )];
    let available_b = |a: &AdapterInfo, c: VideoCodec, api: &str| {
        matches!(
            (a.luid, c, api),
            (2, VideoCodec::Hevc, "mf") | (2, VideoCodec::H264, "mf")
        )
    };
    let candidates_b = EncoderPlan::candidates(
        std::slice::from_ref(&nvidia),
        &outputs_b,
        Some("auto"),
        Some("auto"),
        available_b,
    );
    let expected_b = [(2i64, VideoCodec::Hevc, "mf"), (2, VideoCodec::H264, "mf")];
    assert_eq!(expected_b.len(), candidates_b.len());
    for (i, &(luid, codec, api)) in expected_b.iter().enumerate() {
        assert_eq!(luid, candidates_b[i].adapter.luid);
        assert_eq!(codec, candidates_b[i].codec);
        assert_eq!(api, candidates_b[i].api);
    }

    // (c) AMD-only with av1:mf available
    let outputs_c = [make_output(
        0,
        "Display1",
        3,
        GpuDevices::VENDOR_AMD,
        "AMD Radeon RX 7900",
        0,
    )];
    let available_c = |a: &AdapterInfo, c: VideoCodec, api: &str| {
        matches!(
            (a.luid, c, api),
            (3, VideoCodec::Av1, "mf") | (3, VideoCodec::Hevc, "mf") | (3, VideoCodec::H264, "mf")
        )
    };
    let candidates_c = EncoderPlan::candidates(
        std::slice::from_ref(&amd),
        &outputs_c,
        Some("auto"),
        Some("auto"),
        available_c,
    );
    let expected_c = [
        (3i64, VideoCodec::Av1, "mf"),
        (3, VideoCodec::Hevc, "mf"),
        (3, VideoCodec::H264, "mf"),
    ];
    assert_eq!(expected_c.len(), candidates_c.len());
    for (i, &(luid, codec, api)) in expected_c.iter().enumerate() {
        assert_eq!(luid, candidates_c[i].adapter.luid);
        assert_eq!(codec, candidates_c[i].codec);
        assert_eq!(api, candidates_c[i].api);
    }

    // (d) no hardware encoder -> empty list
    let candidates_d =
        EncoderPlan::candidates(&adapters, &outputs, Some("auto"), Some("auto"), |_, _, _| false);
    assert!(candidates_d.is_empty());

    // (e) encoderAdapter "geforce" on (a) -> NVIDIA candidates first; encoder "h264" -> only h264
    let candidates_e1 =
        EncoderPlan::candidates(&adapters, &outputs, Some("geforce"), Some("auto"), available_a);
    let expected_e1 = [
        (2i64, VideoCodec::Hevc, "mf"),
        (2, VideoCodec::H264, "mf"),
        (1, VideoCodec::Av1, "vpl"),
        (1, VideoCodec::Hevc, "mf"),
        (1, VideoCodec::H264, "mf"),
        (3, VideoCodec::Hevc, "mf"),
        (3, VideoCodec::H264, "mf"),
    ];
    assert_eq!(expected_e1.len(), candidates_e1.len());
    for (i, &(luid, codec, api)) in expected_e1.iter().enumerate() {
        assert_eq!(luid, candidates_e1[i].adapter.luid);
        assert_eq!(codec, candidates_e1[i].codec);
        assert_eq!(api, candidates_e1[i].api);
    }

    let candidates_e2 =
        EncoderPlan::candidates(&adapters, &outputs, Some("auto"), Some("h264"), available_a);
    let expected_e2 = [
        (1i64, VideoCodec::H264, "mf"),
        (2, VideoCodec::H264, "mf"),
        (3, VideoCodec::H264, "mf"),
    ];
    assert_eq!(expected_e2.len(), candidates_e2.len());
    for (i, &(luid, codec, api)) in expected_e2.iter().enumerate() {
        assert_eq!(luid, candidates_e2[i].adapter.luid);
        assert_eq!(codec, candidates_e2[i].codec);
        assert_eq!(api, candidates_e2[i].api);
    }
}

#[test]
fn canvas_layout_per_monitor_scaling() {
    let (max_w, max_h) = (7680, 4320);
    let mon = [MonitorPlacement {
        index: 0,
        left: 0,
        top: 0,
        width: 2560,
        height: 1440,
    }];

    let l_native = CanvasLayout::compute(&mon, Some("native"), 100, max_w, max_h).unwrap();
    assert_eq!(2560, l_native.width);
    assert_eq!(1440, l_native.height);

    let l_2160 = CanvasLayout::compute(&mon, Some("2160p"), 100, max_w, max_h).unwrap();
    assert_eq!(2560, l_2160.width);
    assert_eq!(1440, l_2160.height);

    let l_1440 = CanvasLayout::compute(&mon, Some("1440p"), 100, max_w, max_h).unwrap();
    assert_eq!(2560, l_1440.width);
    assert_eq!(1440, l_1440.height);

    let l_1080 = CanvasLayout::compute(&mon, Some("1080p"), 100, max_w, max_h).unwrap();
    assert_eq!(1920, l_1080.width);
    assert_eq!(1080, l_1080.height);

    let l_900 = CanvasLayout::compute(&mon, Some("900p"), 100, max_w, max_h).unwrap();
    assert_eq!(1600, l_900.width);
    assert_eq!(900, l_900.height);

    let l_720 = CanvasLayout::compute(&mon, Some("720p"), 100, max_w, max_h).unwrap();
    assert_eq!(1280, l_720.width);
    assert_eq!(720, l_720.height);

    let l_540 = CanvasLayout::compute(&mon, Some("540p"), 100, max_w, max_h).unwrap();
    assert_eq!(960, l_540.width);
    assert_eq!(540, l_540.height);

    let l_scale = CanvasLayout::compute(&mon, Some("scale"), 75, max_w, max_h).unwrap();
    assert_eq!(1920, l_scale.width);
    assert_eq!(1080, l_scale.height);

    let pair = [
        MonitorPlacement {
            index: 0,
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        },
        MonitorPlacement {
            index: 1,
            left: 1920,
            top: 0,
            width: 1280,
            height: 1024,
        },
    ];
    let l_pair = CanvasLayout::compute(&pair, Some("720p"), 100, max_w, max_h).unwrap();
    assert_eq!(2132, l_pair.width);
    assert_eq!(720, l_pair.height);
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
