// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::{HashMap, HashSet};

use br_contracts::{
    BenchDevice, BenchReport, BenchResult, CaptureBench, EncoderBench, GpuInfo, HardwareInfo,
    MonitorInfo, NpuInfo, Recommendation, Settings, TokenBench,
};
use br_core::{Recommender, SettingsStore};
use chrono::{DateTime, Utc};

use common::TempHome;

const INTEL_IGPU: &str = "Intel(R) Graphics";
const RTX: &str = "NVIDIA GeForce RTX 3050";
const ARC_OV: &str = "Intel(R) Arc(TM) Graphics (iGPU)";

#[allow(clippy::too_many_arguments)]
fn enc(
    adapter: &str,
    codec: &str,
    layout: &str,
    w: i32,
    h: i32,
    fps: i32,
    p95: f64,
    cpu: f64,
    ok: bool,
) -> EncoderBench {
    EncoderBench {
        adapter: adapter.to_string(),
        codec: codec.to_string(),
        api: if codec == "av1" && adapter.starts_with("Intel") {
            "vpl"
        } else {
            "mf"
        }
        .to_string(),
        layout: layout.to_string(),
        width: w,
        height: h,
        fps,
        ok,
        encode_ms_p95: p95,
        cpu_percent: cpu,
        working_set_mb: 90,
        error: if ok { None } else { Some("failed".to_string()) },
        resolution: None,
    }
}

fn dev(
    device: &str,
    ep: &str,
    ok: bool,
    image_ms: f64,
    text_ms: f64,
    cosine: f64,
    error: Option<&str>,
) -> BenchDevice {
    BenchDevice {
        device: device.to_string(),
        ep: ep.to_string(),
        ok,
        image_ms,
        text_ms,
        cosine,
        error: error.map(ToOwned::to_owned),
        model: None,
        text_device: None,
        compile_ms: None,
        adapter: None,
        adapter_auto: false,
    }
}

fn tok(device: &str, image_tokens: i32, image_ms: f64) -> TokenBench {
    TokenBench {
        device: device.to_string(),
        image_tokens,
        image_ms,
    }
}

fn report(hw: HardwareInfo, inf: Option<BenchResult>, encoders: Vec<EncoderBench>) -> BenchReport {
    BenchReport {
        ts: "2026-11-10T08:00:00Z".parse::<DateTime<Utc>>().unwrap(),
        took_ms: 240_000,
        hardware: hw,
        inference: inf,
        encoders,
        capture: Some(CaptureBench {
            tick_ms: 1.2,
            keyframe_encode_ms: 6.5,
            cross_adapter_ms: 2.1,
            keyframe_encoder: None,
        }),
        search: None,
        recommendations: Vec::new(),
    }
}

fn this_pc() -> BenchReport {
    report(
        HardwareInfo {
            cpu: "Intel(R) Core(TM) Ultra 7 265K".to_string(),
            cores: 20,
            threads: 20,
            ram_mb: 32768,
            has_battery: false,
            gpus: vec![
                GpuInfo {
                    name: INTEL_IGPU.to_string(),
                    vendor: "Intel".to_string(),
                    integrated: true,
                    vram_mb: 0,
                    luid: "0x1".to_string(),
                    driver: "32.0.101.6".to_string(),
                    codecs: vec!["av1".into(), "hevc".into(), "h264".into()],
                },
                GpuInfo {
                    name: "AMD Radeon RX 6600".to_string(),
                    vendor: "AMD".to_string(),
                    integrated: false,
                    vram_mb: 8192,
                    luid: "0x2".to_string(),
                    driver: "32.0.21".to_string(),
                    codecs: vec!["hevc".into(), "h264".into()],
                },
                GpuInfo {
                    name: RTX.to_string(),
                    vendor: "NVIDIA".to_string(),
                    integrated: false,
                    vram_mb: 8192,
                    luid: "0x3".to_string(),
                    driver: "581.42".to_string(),
                    codecs: vec!["hevc".into(), "h264".into()],
                },
            ],
            npu: Some(NpuInfo {
                name: "Intel(R) AI Boost".to_string(),
                driver: "32.0.100.4".to_string(),
            }),
            monitors: vec![
                MonitorInfo {
                    index: 0,
                    width: 2560,
                    height: 1440,
                    refresh_hz: 165,
                    adapter: "AMD Radeon RX 6600".to_string(),
                },
                MonitorInfo {
                    index: 1,
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                    adapter: "AMD Radeon RX 6600".to_string(),
                },
                MonitorInfo {
                    index: 2,
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                    adapter: RTX.to_string(),
                },
            ],
            os: "Windows 11 29704".to_string(),
        },
        Some(BenchResult {
            chosen: "NPU".to_string(),
            devices: vec![
                dev("NPU", "OpenVINO", true, 90.0, 12.0, 0.9995, None),
                dev("GPU", "OpenVINO", true, 110.0, 8.0, 0.9996, None),
                dev("CPU", "OpenVINO", true, 400.0, 30.0, 1.0, None),
            ],
            tokens: Some(vec![
                tok("NPU", 280, 650.0),
                tok("NPU", 140, 330.0),
                tok("NPU", 70, 170.0),
                tok("CPU", 140, 900.0),
            ]),
            chosen_adapter: None,
        }),
        vec![
            enc(INTEL_IGPU, "av1", "composited", 6400, 1440, 30, 9.0, 0.6, true),
            enc(INTEL_IGPU, "av1", "composited", 6400, 1440, 60, 9.5, 0.9, true),
            enc(INTEL_IGPU, "av1", "composited", 4800, 1080, 30, 5.0, 0.4, true),
            enc(INTEL_IGPU, "av1", "composited", 4800, 1080, 60, 5.5, 0.7, true),
            enc(INTEL_IGPU, "av1", "perMonitor", 2560, 1440, 30, 7.0, 0.9, true),
            enc(INTEL_IGPU, "hevc", "composited", 6400, 1440, 30, 8.0, 0.6, true),
            enc("AMD Radeon RX 6600", "hevc", "composited", 6400, 1440, 30, 4.0, 0.5, true),
            enc(RTX, "h264", "composited", 6400, 1440, 30, 3.0, 0.5, true),
        ],
    )
}

fn multi_gpu() -> BenchReport {
    let mut r = this_pc();
    let mut arc = dev("GPU", "OpenVINO", true, 500.0, 8.0, 0.9996, None);
    arc.adapter = Some(ARC_OV.to_string());
    arc.adapter_auto = true;
    let mut rtx = dev("GPU", "DirectML", true, 80.0, 6.0, 0.9997, None);
    rtx.adapter = Some(RTX.to_string());
    let mut amd = dev("GPU", "DirectML", false, 0.0, 0.0, 0.0, Some("cosine"));
    amd.adapter = Some("AMD Radeon RX 6600".to_string());
    r.inference = Some(BenchResult {
        chosen: "NPU".to_string(),
        devices: vec![
            dev("NPU", "OpenVINO", true, 330.0, 12.0, 0.9995, None),
            dev("CPU", "OpenVINO", true, 900.0, 30.0, 1.0, None),
            arc,
            rtx,
            amd,
        ],
        tokens: Some(vec![
            tok("NPU", 280, 650.0),
            tok("NPU", 140, 330.0),
            tok("NPU", 70, 170.0),
        ]),
        chosen_adapter: None,
    });
    r
}

fn slow_npu(mut rep: BenchReport) -> BenchReport {
    if let Some(ref mut inf) = rep.inference {
        for d in &mut inf.devices {
            if d.device == "NPU" {
                d.image_ms = 1200.0;
            }
        }
    }
    rep
}

fn nvidia_desktop() -> BenchReport {
    report(
        HardwareInfo {
            cpu: "AMD Ryzen 7 7800X3D".to_string(),
            cores: 8,
            threads: 16,
            ram_mb: 65536,
            has_battery: false,
            gpus: vec![GpuInfo {
                name: "NVIDIA GeForce RTX 4070".to_string(),
                vendor: "NVIDIA".to_string(),
                integrated: false,
                vram_mb: 12288,
                luid: "0x1".to_string(),
                driver: "581.42".to_string(),
                codecs: vec!["av1".into(), "hevc".into(), "h264".into()],
            }],
            npu: None,
            monitors: vec![MonitorInfo {
                index: 0,
                width: 3840,
                height: 2160,
                refresh_hz: 144,
                adapter: "NVIDIA GeForce RTX 4070".to_string(),
            }],
            os: "Windows 11".to_string(),
        },
        Some(BenchResult {
            chosen: "GPU".to_string(),
            devices: vec![
                dev("GPU", "DirectML", true, 40.0, 5.0, 0.9998, None),
                dev("CPU", "CPU", true, 300.0, 25.0, 1.0, None),
            ],
            tokens: Some(vec![
                tok("GPU", 280, 80.0),
                tok("GPU", 140, 45.0),
                tok("GPU", 70, 30.0),
            ]),
            chosen_adapter: None,
        }),
        vec![
            enc("NVIDIA GeForce RTX 4070", "av1", "composited", 3840, 2160, 30, 3.0, 0.4, true),
            enc("NVIDIA GeForce RTX 4070", "av1", "composited", 3840, 2160, 60, 3.2, 0.6, true),
            enc("NVIDIA GeForce RTX 4070", "av1", "composited", 3840, 2160, 144, 3.3, 0.9, true),
        ],
    )
}

fn amd_laptop() -> BenchReport {
    report(
        HardwareInfo {
            cpu: "AMD Ryzen 7 7840U".to_string(),
            cores: 8,
            threads: 16,
            ram_mb: 7680,
            has_battery: true,
            gpus: vec![GpuInfo {
                name: "AMD Radeon 780M Graphics".to_string(),
                vendor: "AMD".to_string(),
                integrated: true,
                vram_mb: 2048,
                luid: "0x1".to_string(),
                driver: "32.0.12".to_string(),
                codecs: vec!["av1".into(), "hevc".into(), "h264".into()],
            }],
            npu: Some(NpuInfo {
                name: "AMD Ryzen AI".to_string(),
                driver: "1.2".to_string(),
            }),
            monitors: vec![MonitorInfo {
                index: 0,
                width: 2880,
                height: 1800,
                refresh_hz: 120,
                adapter: "AMD Radeon 780M Graphics".to_string(),
            }],
            os: "Windows 11".to_string(),
        },
        Some(BenchResult {
            chosen: "GPU".to_string(),
            devices: vec![
                dev("NPU", "VitisAI", false, 0.0, 0.0, 0.91, Some("cosine")),
                dev("GPU", "DirectML", true, 150.0, 20.0, 0.9997, None),
                dev("CPU", "CPU", true, 600.0, 40.0, 1.0, None),
            ],
            tokens: Some(vec![
                tok("GPU", 280, 420.0),
                tok("GPU", 140, 230.0),
                tok("GPU", 70, 140.0),
            ]),
            chosen_adapter: None,
        }),
        vec![
            enc("AMD Radeon 780M Graphics", "av1", "composited", 2880, 1800, 30, 12.0, 0.8, true),
            enc("AMD Radeon 780M Graphics", "av1", "composited", 2880, 1800, 60, 13.0, 1.2, true),
            enc("AMD Radeon 780M Graphics", "av1", "composited", 1728, 1080, 30, 6.0, 0.5, true),
            enc("AMD Radeon 780M Graphics", "av1", "composited", 1728, 1080, 60, 7.0, 0.8, true),
        ],
    )
}

fn old_intel() -> BenchReport {
    report(
        HardwareInfo {
            cpu: "Intel(R) Core(TM) i7-8700".to_string(),
            cores: 6,
            threads: 12,
            ram_mb: 16384,
            has_battery: false,
            gpus: vec![GpuInfo {
                name: "Intel(R) UHD Graphics 630".to_string(),
                vendor: "Intel".to_string(),
                integrated: true,
                vram_mb: 0,
                luid: "0x1".to_string(),
                driver: "31.0.101".to_string(),
                codecs: vec!["hevc".into(), "h264".into()],
            }],
            npu: None,
            monitors: vec![
                MonitorInfo {
                    index: 0,
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                    adapter: "Intel(R) UHD Graphics 630".to_string(),
                },
                MonitorInfo {
                    index: 1,
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                    adapter: "Intel(R) UHD Graphics 630".to_string(),
                },
            ],
            os: "Windows 11".to_string(),
        },
        Some(BenchResult {
            chosen: "GPU".to_string(),
            devices: vec![
                dev("GPU", "OpenVINO", true, 2600.0, 90.0, 0.9993, None),
                dev("CPU", "OpenVINO", true, 3100.0, 120.0, 1.0, None),
            ],
            tokens: Some(vec![
                tok("GPU", 280, 5000.0),
                tok("GPU", 140, 2600.0),
                tok("GPU", 70, 2000.0),
            ]),
            chosen_adapter: None,
        }),
        vec![
            enc("Intel(R) UHD Graphics 630", "hevc", "composited", 3840, 1080, 30, 18.0, 1.5, true),
            enc("Intel(R) UHD Graphics 630", "hevc", "perMonitor", 1920, 1080, 30, 7.0, 0.7, true),
            enc("Intel(R) UHD Graphics 630", "hevc", "perMonitor", 1920, 1080, 60, 7.5, 1.4, true),
            enc("Intel(R) UHD Graphics 630", "h264", "perMonitor", 1920, 1080, 30, 5.0, 0.6, true),
        ],
    )
}

fn no_encoder() -> BenchReport {
    report(
        HardwareInfo {
            cpu: "Intel(R) Xeon(R)".to_string(),
            cores: 4,
            threads: 4,
            ram_mb: 16384,
            has_battery: false,
            gpus: vec![GpuInfo {
                name: "Microsoft Basic Display Adapter".to_string(),
                vendor: "Microsoft".to_string(),
                integrated: false,
                vram_mb: 0,
                luid: "0x1".to_string(),
                driver: "10.0".to_string(),
                codecs: Vec::new(),
            }],
            npu: None,
            monitors: vec![MonitorInfo {
                index: 0,
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                adapter: "Microsoft Basic Display Adapter".to_string(),
            }],
            os: "Windows 11".to_string(),
        },
        Some(BenchResult {
            chosen: "CPU".to_string(),
            devices: vec![dev("CPU", "CPU", true, 700.0, 50.0, 1.0, None)],
            tokens: Some(vec![tok("CPU", 140, 700.0)]),
            chosen_adapter: None,
        }),
        Vec::new(),
    )
}

fn ultrawide() -> BenchReport {
    report(
        HardwareInfo {
            cpu: "cpu".to_string(),
            cores: 8,
            threads: 8,
            ram_mb: 16384,
            has_battery: false,
            gpus: vec![GpuInfo {
                name: "iGPU".to_string(),
                vendor: "Intel".to_string(),
                integrated: true,
                vram_mb: 0,
                luid: "0x1".to_string(),
                driver: "1".to_string(),
                codecs: vec!["hevc".into()],
            }],
            npu: None,
            monitors: vec![MonitorInfo {
                index: 0,
                width: 5120,
                height: 1440,
                refresh_hz: 60,
                adapter: "iGPU".to_string(),
            }],
            os: "Windows 11".to_string(),
        },
        None,
        vec![
            enc("iGPU", "hevc", "composited", 5120, 1440, 30, 20.0, 0.9, true),
            enc("iGPU", "hevc", "composited", 3840, 1080, 30, 8.0, 0.6, true),
        ],
    )
}

fn run(r: &BenchReport, s: Option<&Settings>) -> HashMap<String, Recommendation> {
    let default = Settings::default();
    Recommender::recommend(r, s.unwrap_or(&default))
        .into_iter()
        .map(|rec| (rec.key.clone(), rec))
        .collect()
}

fn apply(s: &mut Settings, r: &Recommendation) {
    let mut parts = r.key.split('.');
    let sec = parts.next().unwrap();
    let key = parts.next().unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&format!("{{\"{sec}\":{{\"{key}\":{}}}}}", r.value)).expect("json");
    let home = TempHome::new(None);
    let store = SettingsStore::load(home.path());
    assert!(store.replace(s.clone()).expect("replace").is_none());
    assert!(store.try_update(&json).is_none());
    let cur = store.current();
    s.inference = cur.inference;
    s.capture = cur.capture;
    s.replay = cur.replay;
    s.index = cur.index;
    s.governance = cur.governance;
}

#[test]
fn duty_cycle_maths() {
    assert!((Recommender::expected_keyframes_per_second(3, 2000) - 0.45).abs() < 1e-6);
    assert!((Recommender::expected_keyframes_per_second(1, 2000) - 0.15).abs() < 1e-6);
    assert!((Recommender::expected_keyframes_per_second(2, 10000) - 0.06).abs() < 1e-6);
    assert!((Recommender::duty(650.0, 0.45) - 0.2925).abs() < 1e-6);
    assert!(Recommender::duty(1111.0, 0.45) <= 0.5);
    assert!(Recommender::duty(1112.0, 0.45) > 0.5);
}

#[test]
fn realtime_and_bitrate_maths() {
    assert!((Recommender::period_ms(60) - 1000.0 / 60.0).abs() < 1e-9);
    assert!(Recommender::realtime(8.33, 60, Recommender::REALTIME_SHARE));
    assert!(!Recommender::realtime(8.34, 60, Recommender::REALTIME_SHARE));
    assert!(Recommender::realtime(16.6, 30, Recommender::REALTIME_SHARE));
    assert!(!Recommender::realtime(1.75, 144, 0.25));
    assert!(Recommender::realtime(1.73, 144, 0.25));
    assert!(!Recommender::realtime(0.0, 30, Recommender::REALTIME_SHARE));
    assert_eq!(Recommender::bitrate_kbps(1920, 1080, 30, "h264"), 5000);
    assert_eq!(Recommender::bitrate_kbps(1920, 1080, 30, "hevc"), 3000);
    assert_eq!(Recommender::bitrate_kbps(3840, 2160, 60, "av1"), 17500);
    assert_eq!(Recommender::bitrate_kbps(640, 360, 10, "av1"), 500);
    assert_eq!(Recommender::bitrate_kbps(7680, 4320, 240, "h264"), 50000);
}

#[test]
fn cost_estimate_scales_by_pixels_and_failed_rows_fail() {
    let rows = vec![
        enc("A", "hevc", "composited", 3840, 2160, 30, 8.0, 1.0, true),
        enc("A", "hevc", "composited", 1920, 1080, 60, 0.0, 0.0, false),
    ];
    let est = Recommender::cost(&rows, "A", "hevc", "composited", 2560, 1440, 30).expect("est");
    assert!(!est.measured);
    assert!((est.p95 - 8.0 * (2560.0 * 1440.0) / (3840.0 * 2160.0)).abs() < 1e-6);
    assert!(Recommender::cost(&rows, "A", "hevc", "composited", 1920, 1080, 60).is_none());
    assert!(Recommender::cost(&rows, "A", "av1", "composited", 3840, 2160, 30).is_none());
    assert!(Recommender::cost(&rows, "A", "hevc", "composited", 3840, 2160, 30).expect("ex").measured);
}

#[test]
fn this_pc_profile() {
    let r = run(&this_pc(), None);
    assert!(!r.contains_key("inference.device"));
    assert_eq!(r["inference.imageTokens"].value, "280");
    assert_eq!(r["inference.imageTokens"].current, "140");
    assert_eq!(r["inference.imageTokens"].reason, Recommender::R_TOKENS);
    assert_eq!(r["inference.imageTokens"].args, vec!["280", "650", "0.45", "29.3"]);
    assert!(!r.contains_key("capture.intervalMs"));
    assert_eq!(r["replay.encoder"].value, "\"av1\"");
    assert_eq!(r["replay.encoder"].current, "\"auto\"");
    assert_eq!(r["replay.encoder"].reason, Recommender::R_ENCODER);
    assert_eq!(r["replay.encoderAdapter"].value, "\"Intel(R) Graphics\"");
    assert!(!r.contains_key("replay.layout"));
    assert!(!r.contains_key("replay.resolution"));
    assert!(!r.contains_key("replay.fps"));
    assert_eq!(r["replay.bitrateKbps"].value, "9500");
    assert_eq!(r["replay.bitrateKbps"].reason, Recommender::R_BITRATE);
    assert!(!r.contains_key("replay.seconds"));
    assert!(!r.contains_key("index.dim"));
    assert!(!r.contains_key("governance.pauseOnBattery"));
    assert!(!r.contains_key("replay.enabled"));
}

#[test]
fn multi_gpu_keeps_auto_when_the_npu_keeps_up() {
    let r = run(&multi_gpu(), None);
    assert!(!r.contains_key("inference.device"));
    assert!(!r.contains_key("inference.gpuAdapter"));
    let mut s = Settings::default();
    s.inference.device = "gpu".to_string();
    let r = run(&multi_gpu(), Some(&s));
    assert_eq!(r["inference.device"].value, "\"auto\"");
    assert_eq!(r["inference.device"].args, vec!["NPU", "330"]);
}

#[test]
fn multi_gpu_recommends_the_fastest_graphics_card_by_name() {
    let rep = slow_npu(multi_gpu());
    let r = run(&rep, None);
    assert_eq!(r["inference.device"].value, "\"gpu\"");
    assert_eq!(r["inference.device"].args, vec!["GPU (NVIDIA GeForce RTX 3050)", "80"]);
    assert_eq!(r["inference.gpuAdapter"].value, "\"NVIDIA GeForce RTX 3050\"");
    assert_eq!(r["inference.gpuAdapter"].current, "\"auto\"");
    assert_eq!(r["inference.gpuAdapter"].reason, Recommender::R_GPU_ADAPTER);
    assert_eq!(r["inference.gpuAdapter"].args, vec![RTX, "80"]);
    assert!(!r.contains_key("inference.imageTokens"));

    let mut s = Settings::default();
    apply(&mut s, &r["inference.device"]);
    apply(&mut s, &r["inference.gpuAdapter"]);
    assert_eq!((s.inference.device.as_str(), s.inference.gpu_adapter.as_str()), ("gpu", RTX));
    assert!(!Recommender::recommend(&rep, &s).iter().any(|x| x.key.starts_with("inference.")));

    s.inference.gpu_adapter = "rtx 3050".to_string();
    assert!(!Recommender::recommend(&rep, &s).iter().any(|x| x.key == "inference.gpuAdapter"));
}

#[test]
fn multi_gpu_auto_pick_sets_adapter_auto_and_tokens_follow_the_chosen_adapter() {
    let mut rep = slow_npu(multi_gpu());
    if let Some(ref mut inf) = rep.inference {
        for d in &mut inf.devices {
            if d.adapter.as_deref() == Some(ARC_OV) {
                d.image_ms = 60.0;
            }
        }
        inf.chosen = "GPU".to_string();
        inf.chosen_adapter = Some(ARC_OV.to_string());
        inf.tokens = Some(vec![
            tok("GPU", 280, 120.0),
            tok("GPU", 140, 60.0),
            tok("GPU", 70, 40.0),
        ]);
    }
    let mut s = Settings::default();
    s.inference.gpu_adapter = "RTX 3050".to_string();
    let r = run(&rep, Some(&s));
    assert_eq!(r["inference.device"].value, "\"gpu\"");
    assert_eq!(r["inference.gpuAdapter"].value, "\"auto\"");
    assert_eq!(r["inference.imageTokens"].value, "280");

    let mut no_npu = rep.clone();
    no_npu.hardware.npu = None;
    if let Some(ref mut inf) = no_npu.inference {
        inf.devices.retain(|d| d.device != "NPU");
    }
    let r = run(&no_npu, Some(&s));
    assert!(!r.contains_key("inference.device"));
    assert_eq!(r["inference.gpuAdapter"].value, "\"auto\"");

    let mut rtx_fastest = rep.clone();
    if let Some(ref mut inf) = rtx_fastest.inference {
        for d in &mut inf.devices {
            if d.adapter.as_deref() == Some(ARC_OV) {
                d.image_ms = 500.0;
            }
        }
    }
    assert!(!run(&rtx_fastest, None).contains_key("inference.imageTokens"));
}

#[test]
fn nvidia_desktop_without_npu() {
    let r = run(&nvidia_desktop(), None);
    assert!(!r.contains_key("inference.device"));
    assert_eq!(r["inference.imageTokens"].value, "280");
    assert_eq!(r["replay.encoderAdapter"].value, "\"NVIDIA GeForce RTX 4070\"");
    assert_eq!(r["replay.encoder"].value, "\"av1\"");
    assert_eq!(r["replay.fps"].value, "60");
    assert_eq!(r["replay.fps"].reason, Recommender::R_FPS);
    assert_eq!(r["replay.bitrateKbps"].value, "17500");
}

#[test]
fn nvidia_desktop_high_refresh_with_headroom() {
    let mut rep = nvidia_desktop();
    for e in &mut rep.encoders {
        if e.fps == 144 {
            e.encode_ms_p95 = 1.5;
        }
    }
    let r = run(&rep, None);
    assert_eq!(r["replay.fps"].value, "144");
}

#[test]
fn amd_laptop_battery_low_ram() {
    let mut s = Settings::default();
    s.retention.days = 90;
    s.governance.pause_on_battery = false;
    let r = run(&amd_laptop(), Some(&s));
    assert_eq!(r["inference.device"].value, "\"gpu\"");
    assert_eq!(r["inference.device"].reason, Recommender::R_DEVICE);
    assert_eq!(r["inference.device"].args, vec!["GPU", "150"]);
    assert_eq!(r["inference.imageTokens"].value, "280");
    assert_eq!(r["replay.seconds"].value, "60");
    assert_eq!(r["replay.seconds"].reason, Recommender::R_SECONDS);
    assert_eq!(r["index.dim"].value, "512");
    assert_eq!(r["index.dim"].reason, Recommender::R_DIM);
    assert_eq!(r["governance.pauseOnBattery"].value, "true");
    assert_eq!(r["governance.pauseOnBattery"].current, "false");
    assert_eq!(r["governance.pauseOnBattery"].reason, Recommender::R_BATTERY);
    assert!(!r.contains_key("replay.fps"));
    assert_eq!(r["replay.bitrateKbps"].value, "5500");

    let r2 = run(&amd_laptop(), None);
    assert!(!r2.contains_key("index.dim"));
    assert!(!r2.contains_key("governance.pauseOnBattery"));
}

#[test]
fn old_intel_without_av1() {
    let r = run(&old_intel(), None);
    assert!(!r.contains_key("inference.device"));
    assert_eq!(r["inference.imageTokens"].value, "70");
    assert_eq!(r["capture.intervalMs"].value, "3000");
    assert_eq!(r["capture.intervalMs"].reason, Recommender::R_INTERVAL);
    assert_eq!(r["capture.intervalMs"].args, vec!["3000", "2000", "0.2", "40"]);
    assert_eq!(r["replay.encoder"].value, "\"hevc\"");
    assert_eq!(r["replay.layout"].value, "\"perMonitor\"");
    assert_eq!(r["replay.layout"].reason, Recommender::R_LAYOUT);
    assert!(!r.contains_key("replay.fps"));
    assert_eq!(r["replay.bitrateKbps"].value, "3000");
}

#[test]
fn no_hardware_encoder_disables_replay() {
    let r = run(&no_encoder(), None);
    assert_eq!(r["replay.enabled"].value, "false");
    assert_eq!(r["replay.enabled"].reason, Recommender::R_UNSUPPORTED);
    assert!(!r.contains_key("replay.encoder"));
    assert!(!r.contains_key("inference.device"));
}

#[test]
fn encoders_that_all_fail_disable_replay() {
    let mut rep = old_intel();
    for e in &mut rep.encoders {
        e.ok = false;
        e.encode_ms_p95 = 0.0;
        e.error = Some("MFT init failed".to_string());
    }
    assert_eq!(run(&rep, None)["replay.enabled"].value, "false");
}

#[test]
fn slow_encoder_that_scales_to_720p15_keeps_replay() {
    let mut rep = old_intel();
    for e in &mut rep.encoders {
        e.encode_ms_p95 = 40.0;
    }
    let r = run(&rep, None);
    assert!(!r.contains_key("replay.enabled"));
    assert_eq!(r["replay.resolution"].value, "\"720p\"");
    assert_eq!(r["replay.fps"].value, "24");
}

#[test]
fn lower_resolution_when_native_fails() {
    let r = run(&ultrawide(), None);
    assert_eq!(r["replay.resolution"].value, "\"1080p\"");
    assert_eq!(r["replay.resolution"].reason, Recommender::R_RESOLUTION);
    assert!(!r.contains_key("replay.layout"));
    assert!(!r.contains_key("inference.device"));
}

#[test]
fn only_differences_are_emitted() {
    let rep = this_pc();
    let mut s = Settings::default();
    for rec in Recommender::recommend(&rep, &s) {
        apply(&mut s, &rec);
    }
    assert!(Recommender::recommend(&rep, &s).is_empty());
}

#[test]
fn every_reason_code_is_covered_by_a_profile() {
    let mut seen = HashSet::new();
    let mut laptop = Settings::default();
    laptop.retention.days = 90;
    laptop.governance.pause_on_battery = false;
    let cases = vec![
        (this_pc(), Settings::default()),
        (nvidia_desktop(), Settings::default()),
        (amd_laptop(), laptop),
        (old_intel(), Settings::default()),
        (no_encoder(), Settings::default()),
        (ultrawide(), Settings::default()),
        (multi_gpu(), Settings::default()),
        (slow_npu(multi_gpu()), Settings::default()),
    ];
    for (rep, s) in &cases {
        for rec in Recommender::recommend(rep, s) {
            seen.insert(rec.reason);
        }
    }
    let expected: HashSet<String> = [
        Recommender::R_DEVICE,
        Recommender::R_TOKENS,
        Recommender::R_INTERVAL,
        Recommender::R_ENCODER,
        Recommender::R_LAYOUT,
        Recommender::R_FPS,
        Recommender::R_RESOLUTION,
        Recommender::R_BITRATE,
        Recommender::R_SECONDS,
        Recommender::R_UNSUPPORTED,
        Recommender::R_DIM,
        Recommender::R_BATTERY,
        Recommender::R_GPU_ADAPTER,
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect();
    assert_eq!(seen, expected);
}
