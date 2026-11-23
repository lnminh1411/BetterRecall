#![allow(clippy::too_many_arguments, clippy::identity_op, clippy::cloned_ref_to_slice_refs)] // test helpers and readable index math
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use br_contracts::{BenchDevice, BenchResult};
use br_worker::engine::RealEngine;
use br_worker::inference::device_selection::{
    BenchOutcome, DeviceCache, DeviceCacheFile, DeviceCacheKey, DeviceChoice, DevicePolicy,
};
use br_worker::inference::dxgi::{DxgiAdapter, DxgiAdapters};
use br_worker::inference::gpu_selection::GpuSelection;
use br_worker::inference::nsfw_plan::NsfwDevicePlan;
use br_worker::inference::onnx::OnnxBackend;
use br_worker::inference::{DeviceInfo, DeviceKind, IInferenceBackend};
use br_worker::models::image_pipeline::RgbImage;
use br_worker::models::manifest::{ClassifierSpec, DetectorSpec, ModelManifest};
use br_worker::models::nsfw_classifier::NsfwClassifier;
use br_worker::models::nsfw_detector::NsfwDetector;
use br_worker::protocol::WorkerError;
use common::{ScopedDir, adapter, dev};

// ---- UnitTests: NsfwGateTests ----

#[test]
fn device_order_matches_setting_and_available_devices() {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let igpu = dev(
        DeviceKind::Gpu,
        "GPU.0",
        "Intel(R) Arc(TM) Graphics (iGPU)",
        "OpenVINO",
        Some(true),
    );
    let ov_dgpu = dev(
        DeviceKind::Gpu,
        "GPU.1",
        "Intel(R) Arc(TM) A770 (dGPU)",
        "OpenVINO",
        Some(false),
    );
    let dml = dev(
        DeviceKind::Gpu,
        "DML:1",
        "NVIDIA GeForce RTX 3050",
        "DirectML",
        None,
    );
    let ov_cpu = dev(DeviceKind::Cpu, "CPU", "Intel CPU", "OpenVINO", None);
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "CPU", None);

    let all = [
        cpu.clone(),
        dml,
        ov_dgpu.clone(),
        igpu.clone(),
        npu,
        ov_cpu,
    ];
    let missing = [igpu, ov_dgpu, cpu];

    let cases: &[(&[DeviceInfo], &str, &str)] = &[
        (&all, "auto", "NPU,GPU.0,DML:1,CPU"),
        (&all, "npu", "NPU,CPU"),
        (&all, "igpu", "GPU.0,CPU"),
        (&all, "gpu", "DML:1,CPU"),
        (&all, "cpu", "CPU"),
        (&all, "bogus", "NPU,GPU.0,DML:1,CPU"),
        (&missing, "auto", "GPU.0,CPU"),
        (&missing, "npu", "CPU"),
        (&missing, "gpu", "CPU"),
        (&missing, "igpu", "GPU.0,CPU"),
    ];
    for &(avail, setting, expected) in cases {
        let ids: Vec<String> = NsfwDevicePlan::order(setting, avail)
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(expected, ids.join(","), "setting={setting}");
    }
}

#[test]
fn device_order_skips_unknown_gpu_type_and_openvino_cpu() {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let igpu = dev(
        DeviceKind::Gpu,
        "GPU.0",
        "Intel(R) Arc(TM) Graphics (iGPU)",
        "OpenVINO",
        Some(true),
    );
    let dml = dev(
        DeviceKind::Gpu,
        "DML:1",
        "NVIDIA GeForce RTX 3050",
        "DirectML",
        None,
    );
    let ov_cpu = dev(DeviceKind::Cpu, "CPU", "Intel CPU", "OpenVINO", None);
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "CPU", None);
    let unknown = dev(
        DeviceKind::Gpu,
        "GPU",
        "Intel(R) Graphics",
        "OpenVINO",
        None,
    );

    let eps: Vec<String> = NsfwDevicePlan::order("igpu", &[unknown, cpu.clone()])
        .into_iter()
        .map(|d| d.ep)
        .collect();
    assert_eq!("CPU", eps.join(","));
    assert!(NsfwDevicePlan::order("cpu", &[ov_cpu]).is_empty());
    let labels: Vec<String> = [npu, igpu, dml, cpu]
        .iter()
        .map(NsfwDevicePlan::label)
        .collect();
    assert_eq!(
        vec!["NPU", "iGPU", "GPU (NVIDIA GeForce RTX 3050)", "CPU"],
        labels
    );
}

#[test]
fn dxgi_pick_matches_setting_and_auto_skips_software_and_igpu() {
    let rx6600 = adapter(0, "AMD Radeon RX 6600", 8u64 << 30, 0, 0x1002);
    let rtx3050 = adapter(1, "NVIDIA GeForce RTX 3050", 6u64 << 30, 0, 0x10DE);
    let intel_igpu = adapter(2, "Intel(R) Arc(TM) Graphics", 128u64 << 20, 0, 0x8086);
    let basic = adapter(
        3,
        "Microsoft Basic Render Driver",
        0,
        DxgiAdapter::FLAG_SOFTWARE,
        0x1414,
    );
    let arc = adapter(4, "Intel(R) Arc(TM) A770 Graphics", 16u64 << 30, 0, 0x8086);
    let warp = adapter(
        5,
        "Some software adapter",
        0,
        DxgiAdapter::FLAG_SOFTWARE,
        0x1234,
    );
    let full = [rx6600.clone(), rtx3050.clone(), intel_igpu.clone(), basic.clone()];
    let igpu_and_basic = [intel_igpu.clone(), basic];
    let igpu_then_discrete = [intel_igpu.clone(), rtx3050, rx6600];
    let igpu_then_arc = [intel_igpu, arc];
    let warp_only = [warp];

    let cases: &[(&[DxgiAdapter], &str, i32)] = &[
        (&full, "auto", 0),
        (&full, "AUTO", 0),
        (&full, "", 0),
        (&full, "rtx 3050", 1),
        (&full, "  RTX  ", 1),
        (&full, "Intel", 2),
        (&full, "Basic Render", 3),
        (&full, "Quadro", -1),
        (&igpu_and_basic, "auto", -1),
        (&igpu_then_discrete, "auto", 1),
        (&igpu_then_arc, "auto", 4),
        (&warp_only, "auto", -1),
    ];
    for &(list, setting, expected) in cases {
        let got = DxgiAdapters::pick(Some(setting), list)
            .map(|a| a.index)
            .unwrap_or(-1);
        assert_eq!(expected, got, "setting={setting}");
    }
}

#[test]
fn dml_device_id() {
    for (id, expected) in [
        ("DML:0", 0),
        ("DML:3", 3),
        ("DML:x", 0),
        ("DML:-1", 0),
        ("CPU", 0),
    ] {
        assert_eq!(expected, OnnxBackend::dml_device_id(id));
    }
}

#[test]
fn dxgi_enumerate_does_not_throw() {
    let list = DxgiAdapters::enumerate();
    let indices: Vec<i32> = list.iter().map(|a| a.index).collect();
    let expected: Vec<i32> = (0..list.len() as i32).collect();
    assert_eq!(expected, indices);
}

#[test]
fn block_score_is_max_of_block_rows() {
    let mut o = [
        900.0f32, 900.0, 900.0, 900.0, // cx
        900.0, 900.0, 900.0, 900.0, // cy
        900.0, 900.0, 900.0, 900.0, // w
        900.0, 900.0, 900.0, 900.0, // h
        0.9, 0.1, 0.2, 0.3, // label 0
        0.1, 0.6, 0.2, 0.05, // label 1
        0.2, 0.3, 0.7, 0.1, // label 2
    ];
    assert!(
        (NsfwDetector::block_score(&o, 7, 4, &[5]).unwrap() - 0.6).abs() < 1e-6,
        "single row"
    );
    assert!(
        (NsfwDetector::block_score(&o, 7, 4, &[5, 6]).unwrap() - 0.7).abs() < 1e-6,
        "max of rows"
    );
    assert!((NsfwDetector::block_score(&o, 7, 4, &[]).unwrap() - 0.0).abs() < 1e-6);
    assert!((NsfwDetector::block_score(&o, 7, 4, &[5, 99]).unwrap() - 0.6).abs() < 1e-6);
    o[4 * 4 + 4 + 2] = f32::NAN;
    assert!(NsfwDetector::block_score(&o, 7, 4, &[5]).unwrap().is_nan());
    assert!(matches!(
        NsfwDetector::block_score(&o[..10], 7, 4, &[5]),
        Err(WorkerError::DeviceFailed(_))
    ));
}

#[test]
fn block_rows_follow_labels() {
    let spec = DetectorSpec {
        labels: common::NUDENET_LABELS.iter().map(|s| (*s).to_string()).collect(),
        block_labels: vec![
            "FEMALE_GENITALIA_COVERED".to_string(),
            "BUTTOCKS_COVERED".to_string(),
            "NOPE".to_string(),
        ],
        ..DetectorSpec::default()
    };
    assert_eq!(vec![4usize, 21usize], NsfwDetector::block_rows(&spec));
}

#[test]
fn letterbox_pads_bottom_and_right_with_black() {
    let wide = RgbImage {
        width: 4,
        height: 2,
        rgb: vec![200u8; 4 * 2 * 3],
    };
    let t = NsfwDetector::letterbox(&wide, 4);
    assert_eq!(3 * 16, t.len());
    for c in 0..3 {
        for y in 0..4 {
            for x in 0..4 {
                let want = if y < 2 { 200.0 / 255.0 } else { 0.0 };
                assert!((t[c * 16 + y * 4 + x] - want).abs() < 1e-5);
            }
        }
    }

    let tall = RgbImage {
        width: 2,
        height: 4,
        rgb: vec![255u8; 2 * 4 * 3],
    };
    let t = NsfwDetector::letterbox(&tall, 4);
    for y in 0..4 {
        for x in 0..4 {
            let want = if x < 2 { 1.0f32 } else { 0.0 };
            assert!((t[16 + y * 4 + x] - want).abs() < 1e-5);
        }
    }

    let big = RgbImage {
        width: 8,
        height: 4,
        rgb: vec![100u8; 8 * 4 * 3],
    };
    let t = NsfwDetector::letterbox(&big, 4);
    for x in 0..4 {
        assert!((t[x] - 100.0 / 255.0).abs() < 1e-5);
        assert!((t[3 * 4 + x] - 0.0).abs() < 1e-5);
    }
}

#[test]
fn classifier_file_per_device() {
    let spec = ClassifierSpec {
        file: "model_int8.onnx".to_string(),
        accelerator_file: Some("model_fp32.onnx".to_string()),
        ..ClassifierSpec::default()
    };
    assert_eq!(
        "model_int8.onnx",
        NsfwClassifier::file_for(&spec, DeviceKind::Cpu)
    );
    assert_eq!(
        "model_fp32.onnx",
        NsfwClassifier::file_for(&spec, DeviceKind::Npu)
    );
    assert_eq!(
        "model_fp32.onnx",
        NsfwClassifier::file_for(&spec, DeviceKind::Gpu)
    );
    let cpu_only = ClassifierSpec {
        file: "model_int8.onnx".to_string(),
        ..ClassifierSpec::default()
    };
    assert_eq!(
        "model_int8.onnx",
        NsfwClassifier::file_for(&cpu_only, DeviceKind::Npu)
    );
}

#[test]
fn manifest_detector_section_round_trips() {
    let json = r#"{"name":"nsfw-detector","detector":{"file":"model.onnx","input":"images","output":"output0","size":640,"labels":["A","B"],"blockLabels":["B"]},"classifier":{"file":"model_int8.onnx","acceleratorFile":"model_fp32.onnx"},"embeddings":{"dType":"bf16"}}"#;
    let m: ModelManifest = serde_json::from_str(json).expect("parse manifest");
    assert_eq!(640, m.detector.as_ref().unwrap().size);
    assert_eq!(vec!["B"], m.detector.as_ref().unwrap().block_labels);
    assert_eq!(
        Some("model_fp32.onnx"),
        m.classifier.as_ref().unwrap().accelerator_file.as_deref()
    );
    assert_eq!("bf16", m.embeddings.as_ref().unwrap().dtype);
    let m2: ModelManifest = serde_json::from_str(r#"{"classifier":{"file":"x"}}"#).unwrap();
    assert!(m2.classifier.unwrap().accelerator_file.is_none());
}

#[test]
fn manifest_onnx_map_picks_the_onnx_twin_only_on_onnx_runtime() {
    let m: ModelManifest = serde_json::from_str(
        r#"{"name":"embeddinggemma-2","onnx":{"text.xml":"text.onnx","vision_70.xml":"vision.onnx"}}"#,
    )
    .unwrap();
    assert_eq!("text.onnx", m.file_for("text.xml", true));
    assert_eq!("vision.onnx", m.file_for("vision_70.xml", true));
    assert_eq!("text.xml", m.file_for("text.xml", false));
    assert_eq!("vision_140.xml", m.file_for("vision_140.xml", true));
    assert_eq!("text.xml", ModelManifest::default().file_for("text.xml", true));
}

// ---- UnitTests: DevicePolicyTests ----

fn outcome(
    d: &DeviceInfo,
    model: &str,
    ok: bool,
    img_ms: f64,
    txt_ms: f64,
    cos: f64,
    err: Option<&str>,
    text_dev: Option<&DeviceInfo>,
) -> BenchOutcome {
    BenchOutcome {
        device: d.clone(),
        model: model.to_string(),
        ok,
        image_ms: img_ms,
        text_ms: txt_ms,
        cosine: cos,
        error: err.map(ToString::to_string),
        compile_ms: 0.0,
        text_device: text_dev.cloned(),
        compile_cached: false,
    }
}

#[test]
fn allowed_devices_respects_setting_and_resolved_gpu() {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let gpu = dev(
        DeviceKind::Gpu,
        "GPU.0",
        "Intel(R) Arc(TM) 140T GPU (16GB) (iGPU)",
        "OpenVINO",
        Some(true),
    );
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "OpenVINO", None);
    let rtx = GpuSelection::dml(&adapter(
        1,
        "NVIDIA GeForce RTX 3050",
        6u64 << 30,
        0,
        0x10DE,
    ));

    for (setting, has_npu, has_gpu, resolved, expected) in [
        ("auto", true, true, None, "NPU,CPU"),
        ("auto", false, true, None, "GPU.0,CPU"),
        ("auto", false, false, None, "CPU"),
        ("npu", true, true, None, "NPU,CPU"),
        ("npu", false, true, None, "CPU"),
        ("gpu", true, true, None, "GPU.0,CPU"),
        ("cpu", true, true, None, "CPU"),
        ("gpu", true, true, Some(&rtx), "DML:1,CPU"),
        ("auto", true, true, Some(&rtx), "NPU,CPU"),
        ("cpu", true, true, Some(&rtx), "CPU"),
    ] {
        let mut avail = Vec::new();
        if has_npu {
            avail.push(npu.clone());
        }
        if has_gpu {
            avail.push(gpu.clone());
        }
        avail.push(cpu.clone());
        let ids: Vec<String> = DevicePolicy::allowed(setting, &avail, resolved)
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(expected, ids.join(","), "setting={setting}");
    }
}

#[test]
fn accept_rejects_low_cosine_and_nan() {
    for (cos, nan, ok) in [
        (0.999, false, true),
        (0.9999, false, true),
        (0.9989, false, false),
        (1.0, true, false),
        (f64::NAN, false, false),
        (0.67, false, false),
    ] {
        assert_eq!(ok, DevicePolicy::accept(cos, nan));
    }
}

#[test]
fn choose_fastest_passing_primary() {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let gpu = dev(DeviceKind::Gpu, "GPU.0", "Intel(R) Graphics", "OpenVINO", None);
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "OpenVINO", None);

    let mut o = vec![
        outcome(&cpu, "embeddinggemma-2", true, 2700.0, 60.0, 1.0, None, None),
        outcome(&npu, "embeddinggemma-2", true, 1300.0, 37.0, 0.9995, None, None),
    ];
    assert_eq!(
        "NPU",
        DevicePolicy::choose(&o, "embeddinggemma-2", "siglip2-base")
            .unwrap()
            .device
            .id
    );
    // Passing NPU wins even when CPU is faster (keep CPU free).
    o[0].image_ms = 10.0;
    o[0].text_ms = 1.0;
    assert_eq!(
        "NPU",
        DevicePolicy::choose(&o, "embeddinggemma-2", "siglip2-base")
            .unwrap()
            .device
            .id
    );
    let g_out = outcome(&gpu, "embeddinggemma-2", true, 400.0, 10.0, 0.9999, None, None);
    let with_gpu = vec![o[0].clone(), o[1].clone(), g_out];
    assert_eq!(
        "GPU.0",
        DevicePolicy::choose(&with_gpu, "embeddinggemma-2", "siglip2-base")
            .unwrap()
            .device
            .id
    );
    o[1].ok = false;
    o[1].cosine = 0.67;
    assert_eq!(
        "CPU",
        DevicePolicy::choose(&o, "embeddinggemma-2", "siglip2-base")
            .unwrap()
            .device
            .id
    );
}

#[test]
fn split_beats_siglip_and_cpu_but_not_a_full_npu_pass() {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "OpenVINO", None);

    let full = outcome(
        &npu,
        "embeddinggemma-2",
        false,
        0.0,
        0.0,
        0.81,
        Some("cosine 0.8100 below 0.999"),
        Some(&npu),
    );
    let mut split = outcome(
        &npu,
        "embeddinggemma-2",
        true,
        1400.0,
        30.0,
        0.9999,
        None,
        Some(&cpu),
    );
    let cpu_o = outcome(
        &cpu,
        "embeddinggemma-2",
        true,
        1300.0,
        30.0,
        1.0,
        None,
        Some(&cpu),
    );
    let sig = outcome(&npu, "siglip2-base", true, 50.0, 5.0, 0.9999, None, Some(&npu));
    assert!(split.split());
    assert!(!full.split());

    let list1 = [cpu_o.clone(), full.clone(), split.clone(), sig.clone()];
    let c1 = DevicePolicy::choose(&list1, "embeddinggemma-2", "siglip2-base").unwrap();
    assert!(c1.split());

    let mut full_ok = full.clone();
    full_ok.ok = true;
    full_ok.cosine = 0.9995;
    full_ok.image_ms = 1500.0;
    full_ok.error = None;
    let list2 = [cpu_o.clone(), full_ok, split.clone()];
    let c2 = DevicePolicy::choose(&list2, "embeddinggemma-2", "siglip2-base").unwrap();
    assert!(!c2.split());

    split.ok = false;
    let list3 = [cpu_o, full, split, sig];
    let c3 = DevicePolicy::choose(&list3, "embeddinggemma-2", "siglip2-base").unwrap();
    assert_eq!("siglip2-base", c3.model);
}

#[test]
fn choose_siglip_on_npu_when_primary_vision_fails_there() {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "OpenVINO", None);
    let mut o = vec![
        outcome(&cpu, "embeddinggemma-2", true, 2700.0, 60.0, 1.0, None, None),
        outcome(
            &npu,
            "embeddinggemma-2",
            false,
            0.0,
            0.0,
            f64::NAN,
            Some("NPU compile failed: ConvertReduceToPooling"),
            None,
        ),
        outcome(&cpu, "siglip2-base", true, 300.0, 20.0, 1.0, None, None),
        outcome(&npu, "siglip2-base", true, 400.0, 15.0, 0.9997, None, None),
    ];
    let c1 = DevicePolicy::choose(&o, "embeddinggemma-2", "siglip2-base").unwrap();
    assert_eq!(("siglip2-base", "NPU"), (c1.model.as_str(), c1.device.id.as_str()));

    o[3].ok = false;
    let c2 = DevicePolicy::choose(&o, "embeddinggemma-2", "siglip2-base").unwrap();
    assert_eq!(
        ("embeddinggemma-2", "CPU"),
        (c2.model.as_str(), c2.device.id.as_str())
    );
    assert!(DevicePolicy::choose(&[o[1].clone(), o[3].clone()], "embeddinggemma-2", "siglip2-base").is_none());
}

// ---- UnitTests: DeviceCacheTests ----

#[test]
fn roundtrip_and_invalidation() {
    let dir = ScopedDir::new("devcache");
    let path = dir.path().join("cache").join("device.json");
    let key = DeviceCacheKey {
        runtime: "OpenVINO".to_string(),
        runtime_version: "2026.4.1-22982".to_string(),
        npu_driver: "1005540".to_string(),
        model_hash: "abc+def".to_string(),
        setting: "auto".to_string(),
        image_tokens: 140,
    };
    assert!(DeviceCache::read(&path, &key).is_none());
    let file = DeviceCacheFile {
        key: Some(key.clone()),
        chosen: Some(DeviceChoice {
            device: "NPU".to_string(),
            device_id: "NPU".to_string(),
            ep: "OpenVINO".to_string(),
            model: "embeddinggemma-2".to_string(),
            text_device_id: None,
        }),
        bench: Some(BenchResult {
            chosen: "NPU".to_string(),
            devices: vec![BenchDevice {
                device: "NPU".to_string(),
                ep: "OpenVINO".to_string(),
                ok: true,
                image_ms: 1.0,
                text_ms: 2.0,
                cosine: 0.9995,
                error: None,
                model: None,
                text_device: None,
                compile_ms: None,
                adapter: None,
                adapter_auto: false,
            }],
            tokens: None,
            chosen_adapter: None,
        }),
        written_utc: "2026-01-01T00:00:00Z".to_string(),
    };
    DeviceCache::write(&path, &file).expect("write device cache");
    let back = DeviceCache::read(&path, &key).expect("read cache back");
    assert_eq!("NPU", back.chosen.as_ref().unwrap().device);
    assert!((back.bench.as_ref().unwrap().devices[0].cosine - 0.9995).abs() < 1e-6);

    assert!(
        DeviceCache::read(
            &path,
            &DeviceCacheKey {
                runtime_version: "2026.5.0".to_string(),
                ..key.clone()
            }
        )
        .is_none()
    );
    assert!(
        DeviceCache::read(
            &path,
            &DeviceCacheKey {
                npu_driver: "1005600".to_string(),
                ..key.clone()
            }
        )
        .is_none()
    );
    assert!(
        DeviceCache::read(
            &path,
            &DeviceCacheKey {
                model_hash: "abc+zzz".to_string(),
                ..key.clone()
            }
        )
        .is_none()
    );
    assert!(
        DeviceCache::read(
            &path,
            &DeviceCacheKey {
                setting: "cpu".to_string(),
                ..key.clone()
            }
        )
        .is_none()
    );
    assert!(
        DeviceCache::read(
            &path,
            &DeviceCacheKey {
                image_tokens: 70,
                ..key.clone()
            }
        )
        .is_none()
    );
    assert!(
        DeviceCache::read(
            &path,
            &DeviceCacheKey {
                runtime: "ONNX Runtime".to_string(),
                ..key.clone()
            }
        )
        .is_none()
    );

    fs::write(&path, "{ not json").unwrap();
    assert!(DeviceCache::read(&path, &key).is_none());
}

#[test]
fn key_ids_change_with_every_part_and_adapter_setting() {
    let key = DeviceCacheKey {
        runtime: "OpenVINO".to_string(),
        runtime_version: "2026.4.1-22982".to_string(),
        npu_driver: "1005540".to_string(),
        model_hash: "abc+def".to_string(),
        setting: "auto".to_string(),
        image_tokens: 140,
    };
    let ids: HashSet<String> = [
        key.clone(),
        DeviceCacheKey {
            runtime_version: "x".to_string(),
            ..key.clone()
        },
        DeviceCacheKey {
            npu_driver: "x".to_string(),
            ..key.clone()
        },
        DeviceCacheKey {
            model_hash: "x".to_string(),
            ..key.clone()
        },
    ]
    .iter()
    .map(DeviceCacheKey::compiled_id)
    .collect();
    assert_eq!(4, ids.len());
    assert_eq!(
        key.compiled_id(),
        DeviceCacheKey {
            setting: "cpu".to_string(),
            image_tokens: 70,
            ..key.clone()
        }
        .compiled_id()
    );
    assert_ne!(
        key.id(),
        DeviceCacheKey {
            setting: "cpu".to_string(),
            ..key
        }
        .id()
    );

    // Non-auto GPU adapter is folded into setting_of and changes the cache key id.
    assert_eq!("gpu", DeviceCacheKey::setting_of("gpu", "auto"));
    assert_eq!("auto", DeviceCacheKey::setting_of("auto", "AUTO"));
    assert_eq!("gpu|RTX 3050", DeviceCacheKey::setting_of("gpu", " RTX 3050 "));
    let k = DeviceCacheKey {
        runtime: "OpenVINO".to_string(),
        runtime_version: "1".to_string(),
        npu_driver: "2".to_string(),
        model_hash: "h".to_string(),
        setting: DeviceCacheKey::setting_of("gpu", "auto"),
        image_tokens: 140,
    };
    assert_ne!(
        k.id(),
        DeviceCacheKey {
            setting: DeviceCacheKey::setting_of("gpu", "RTX 3050"),
            ..k.clone()
        }
        .id()
    );
    assert_ne!(
        DeviceCacheKey {
            setting: DeviceCacheKey::setting_of("gpu", "RX 6600"),
            ..k.clone()
        }
        .id(),
        DeviceCacheKey {
            setting: DeviceCacheKey::setting_of("gpu", "RTX 3050"),
            ..k
        }
        .id()
    );
}

#[test]
fn compiled_dir_prunes_stale_blobs_of_the_same_model() {
    let root = ScopedDir::new("compiled");
    let old = DeviceCache::compiled_dir(root.path(), "embeddinggemma-2", "1111111111111111");
    fs::write(old.join("blob"), "x").unwrap();
    let other =
        DeviceCache::compiled_dir(root.path(), "whisper-large-v3-turbo", "3333333333333333");
    let now = DeviceCache::compiled_dir(root.path(), "embeddinggemma-2", "2222222222222222");
    assert!(now.exists());
    assert!(!old.exists());
    assert!(other.exists());
}


#[test]
fn bench_device_reports_model_split_and_compile_fields() {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "OpenVINO", None);
    let mut split_o = outcome(
        &npu,
        "embeddinggemma-2",
        true,
        480.04,
        30.0,
        0.99991,
        None,
        Some(&cpu),
    );
    split_o.compile_ms = 41234.6;
    let split = RealEngine::to_bench_device(&split_o, &[], None);
    assert_eq!("NPU", split.device);
    assert_eq!("OpenVINO", split.ep);
    assert_eq!(Some("embeddinggemma-2"), split.model.as_deref());
    assert_eq!(Some("CPU"), split.text_device.as_deref());
    assert_eq!(Some(41235.0), split.compile_ms);

    let mut cached_o = outcome(&npu, "siglip2-base", true, 50.0, 5.0, 1.0, None, Some(&npu));
    cached_o.compile_ms = 900.0;
    cached_o.compile_cached = true;
    let cached = RealEngine::to_bench_device(&cached_o, &[], None);
    assert!(cached.text_device.is_none());
    assert!(cached.compile_ms.is_none());
    assert_eq!(Some("siglip2-base"), cached.model.as_deref());
}

// ---- GpuSelectionTests ----

fn dev_id(d: Option<&DeviceInfo>) -> String {
    match d {
        None => "none".to_string(),
        Some(x) => format!("{}:{}", x.ep, x.id),
    }
}

fn gpu_selection_fixture() -> ([DeviceInfo; 3], [DxgiAdapter; 4]) {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let igpu = dev(
        DeviceKind::Gpu,
        "GPU.0",
        "Intel(R) Arc(TM) 140T GPU (16GB) (iGPU)",
        "OpenVINO",
        Some(true),
    );
    let ov_cpu = dev(
        DeviceKind::Cpu,
        "CPU",
        "Intel(R) Core(TM) Ultra 7",
        "OpenVINO",
        None,
    );
    let dxgi = [
        adapter(0, "AMD Radeon RX 6600", 8u64 << 30, 0, 0x1002),
        adapter(1, "NVIDIA GeForce RTX 3050", 6u64 << 30, 0, 0x10DE),
        adapter(2, "Intel(R) Arc(TM) 140T GPU (16GB)", 128u64 << 20, 0, 0x8086),
        adapter(
            3,
            "Microsoft Basic Render Driver",
            0,
            DxgiAdapter::FLAG_SOFTWARE,
            0x1414,
        ),
    ];
    ([npu, igpu, ov_cpu], dxgi)
}

#[test]
fn resolve_matches_openvino_then_dxgi() {
    let (ov_main, dxgi) = gpu_selection_fixture();

    for (setting, expected) in [
        ("auto", "OpenVINO:GPU.0"),
        ("AUTO", "OpenVINO:GPU.0"),
        ("", "OpenVINO:GPU.0"),
        ("Intel", "OpenVINO:GPU.0"),
        ("iGPU", "OpenVINO:GPU.0"),
        ("rtx 3050", "DirectML:DML:1"),
        (" RX 6600 ", "DirectML:DML:0"),
        ("NVIDIA GeForce RTX 3050", "DirectML:DML:1"),
    ] {
        let (picked, warning) = GpuSelection::resolve(Some(setting), &ov_main, &dxgi, true);
        assert_eq!(expected, dev_id(picked.as_ref()));
        assert!(warning.is_none());
    }
}

#[test]
fn resolve_no_match_warns_and_falls_back_to_auto() {
    let (ov_main, dxgi) = gpu_selection_fixture();

    for setting in ["Quadro", "Basic Render"] {
        let (picked, warning) = GpuSelection::resolve(Some(setting), &ov_main, &dxgi, true);
        assert_eq!("OpenVINO:GPU.0", dev_id(picked.as_ref()));
        assert!(warning.unwrap_or_default().contains("matches no GPU"));
    }
}

#[test]
fn resolve_edge_cases() {
    let (ov_main, dxgi) = gpu_selection_fixture();
    let ov_cpu = ov_main[2].clone();
    let rx6600 = dxgi[0].clone();

    let arc = adapter(2, "Intel(R) Graphics Family", 128u64 << 20, 0, 0x8086);
    let (p1, w1) = GpuSelection::resolve(Some("Graphics Family"), &ov_main, &[rx6600, arc], true);
    assert_eq!("OpenVINO:GPU.0", dev_id(p1.as_ref()));
    assert!(w1.is_none());

    let (p2, w2) = GpuSelection::resolve(Some("RTX 3050"), &ov_main, &dxgi, false);
    assert_eq!("OpenVINO:GPU.0", dev_id(p2.as_ref()));
    assert!(w2.unwrap_or_default().contains("no DirectML provider"));

    let ort_main = [
        dev(
            DeviceKind::Gpu,
            "DML:0",
            "DirectML default adapter",
            "DirectML",
            None,
        ),
        dev(DeviceKind::Cpu, "CPU", "CPU", "CPU", None),
    ];
    let (p3, _) = GpuSelection::resolve(Some("auto"), &ort_main, &dxgi, true);
    assert_eq!("DirectML:DML:0", dev_id(p3.as_ref()));
    let (p4, _) = GpuSelection::resolve(Some("3050"), &ort_main, &dxgi, true);
    assert_eq!("DirectML:DML:1", dev_id(p4.as_ref()));

    assert!(GpuSelection::resolve(Some("auto"), &[ov_cpu.clone()], &[], true).0.is_none());
    let (p5, w3) = GpuSelection::resolve(Some("RTX"), &[ov_cpu], &[], true);
    assert!(p5.is_none());
    assert!(w3.is_some());

    let (p6, _) = GpuSelection::resolve(Some("3050"), &ov_main, &dxgi, true);
    assert_eq!("NVIDIA GeForce RTX 3050", p6.unwrap().name);
}

#[test]
fn all_lists_every_gpu_once_preferring_openvino_for_intel() {
    let (ov_main, dxgi) = gpu_selection_fixture();

    let join_ids = |v: Vec<DeviceInfo>| -> String {
        v.iter().map(|d| format!("{}:{}", d.ep, d.id)).collect::<Vec<_>>().join(",")
    };
    assert_eq!(
        "OpenVINO:GPU.0,DirectML:DML:0,DirectML:DML:1",
        join_ids(GpuSelection::all(&ov_main, &dxgi, true))
    );
    assert_eq!(
        "OpenVINO:GPU.0",
        join_ids(GpuSelection::all(&ov_main, &dxgi, false))
    );
    let ort_main = [
        dev(
            DeviceKind::Gpu,
            "DML:0",
            "DirectML default adapter",
            "DirectML",
            None,
        ),
        dev(DeviceKind::Cpu, "CPU", "CPU", "CPU", None),
    ];
    assert_eq!(
        "DirectML:DML:0",
        join_ids(GpuSelection::all(&ort_main, &[], true))
    );
    assert_eq!(
        "DirectML:DML:0,DirectML:DML:1,DirectML:DML:2",
        join_ids(GpuSelection::all(&ort_main, &dxgi, true))
    );
}

#[test]
fn bench_devices_add_every_other_gpu_only_with_several_gpus() {
    let (ov_main, dxgi) = gpu_selection_fixture();
    let igpu = ov_main[1].clone();
    let gpus = GpuSelection::all(&ov_main, &dxgi[..3], true);
    let allowed = DevicePolicy::allowed("auto", &ov_main, None);
    let ids = |v: Vec<DeviceInfo>| v.into_iter().map(|d| d.id).collect::<Vec<_>>().join(",");
    assert_eq!(
        "NPU,CPU,GPU.0,DML:0,DML:1",
        ids(DevicePolicy::bench_devices(&allowed, &gpus))
    );
    let gpu_allowed = DevicePolicy::allowed("gpu", &ov_main, Some(&gpus[2]));
    assert_eq!(
        "DML:1,CPU,GPU.0,DML:0",
        ids(DevicePolicy::bench_devices(&gpu_allowed, &gpus))
    );
    assert_eq!(
        allowed,
        DevicePolicy::bench_devices(&allowed, &[igpu])
    );
    assert_eq!(allowed, DevicePolicy::bench_devices(&allowed, &[]));
}

#[test]
fn bench_rows_carry_adapter_names_with_several_gpus() {
    let (ov_main, dxgi) = gpu_selection_fixture();
    let npu = ov_main[0].clone();
    let igpu = ov_main[1].clone();
    let gpus = GpuSelection::all(&ov_main, &dxgi[..3], true);
    let rtx = RealEngine::to_bench_device(
        &outcome(&gpus[2], "embeddinggemma-2", true, 90.0, 9.0, 0.9999, None, None),
        &gpus,
        Some(&igpu),
    );
    assert_eq!("GPU", rtx.device);
    assert_eq!("DirectML", rtx.ep);
    assert_eq!(Some("NVIDIA GeForce RTX 3050"), rtx.adapter.as_deref());
    assert!(!rtx.adapter_auto);

    let igpu_row = RealEngine::to_bench_device(
        &outcome(&igpu, "embeddinggemma-2", true, 300.0, 9.0, 0.9999, None, None),
        &gpus,
        Some(&igpu),
    );
    assert_eq!(Some(igpu.name.as_str()), igpu_row.adapter.as_deref());
    assert!(igpu_row.adapter_auto);

    let npu_row = RealEngine::to_bench_device(
        &outcome(&npu, "embeddinggemma-2", true, 500.0, 9.0, 0.9999, None, None),
        &gpus,
        Some(&igpu),
    );
    assert!(npu_row.adapter.is_none());

    let single = RealEngine::to_bench_device(
        &outcome(&igpu, "embeddinggemma-2", true, 300.0, 9.0, 0.9999, None, None),
        &[igpu.clone()],
        Some(&igpu),
    );
    assert!(single.adapter.is_none());
    assert!(!single.adapter_auto);
}

#[test]
fn onnx_runtime_refuses_openvino_ir_with_a_clear_message() {
    let ort = OnnxBackend::new(true).expect("OnnxBackend");
    let cpu = ort.devices().last().unwrap();
    let err = ort
        .load(Path::new(r"Z:\no-such-dir\text.xml"), cpu, None, None)
        .err()
        .expect("expected DeviceFailed");
    assert!(err.wire_message().contains("OpenVINO IR"));
}

