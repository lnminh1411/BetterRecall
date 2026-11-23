#![allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::identity_op,
    clippy::cloned_ref_to_slice_refs
)]
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use br_contracts::{
    BenchDevice, DataPaths, WorkerRequest,
};
use br_worker::audio::SileroVad;
use br_worker::engine::{IWorkerEngine, ImageInput, RealEngine, TextKind};
use br_worker::hosting::WorkerSettings;
use br_worker::inference::device_selection::{DeviceCache, DeviceCacheFile, DeviceCacheKey, DeviceChoice, DevicePolicy};
use br_worker::inference::onnx::OnnxBackend;
use br_worker::inference::openvino::OpenVinoBackend;
use br_worker::inference::{
    DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession, TensorSpec,
};
use br_worker::models::catalog::ModelCatalog;
use br_worker::models::embedder::Embedder;
use br_worker::models::image_pipeline::RgbImage;
use br_worker::models::manifest::{
    FusionGraph, ModelManifest, PromptSpec, TextGraph, TokenizerSpec, VisionGraph,
};
use br_worker::protocol::{RequestHandler, WorkerError};
use br_worker::vectors::VectorMath;
use common::{
    PipeHarness, ScopedDir, bgra, copy_model_file, data_path,
    dev, dev_cpu, dev_igpu, dev_npu, install_embedding_gemma,
    install_embedding_gemma_table_layout, install_nsfw, install_nsfw_detector, install_siglip,
    install_vad, pcm_sine, persistent_npu_cache_dir, send_req, test_settings, write_manifest,
};

// ---- Mock backends used in GpuSelectionTests / Sweep2GTests / Sweep4W / Sweep5W ----

struct TwoGpuBackend {
    cpu: OnnxBackend,
    devices: Vec<DeviceInfo>,
}

impl TwoGpuBackend {
    fn new() -> Self {
        Self {
            cpu: OnnxBackend::new(true).expect("OnnxBackend"),
            devices: vec![
                dev(DeviceKind::Gpu, "GPU.0", "Fake iGPU", "OpenVINO", Some(true)),
                dev(DeviceKind::Gpu, "GPU.1", "Fake Arc dGPU", "OpenVINO", Some(false)),
                dev(DeviceKind::Cpu, "CPU", "CPU", "OpenVINO", None),
            ],
        }
    }
}

impl IInferenceBackend for TwoGpuBackend {
    fn name(&self) -> &str {
        "Fake"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn npu_driver_version(&self) -> &str {
        ""
    }
    fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }
    fn load(
        &self,
        model_path: &Path,
        _device: &DeviceInfo,
        _cache_dir: Option<&Path>,
        static_shapes: Option<&HashMap<String, Vec<i64>>>,
    ) -> Result<Box<dyn IInferenceSession>, WorkerError> {
        let cpu_dev = self.cpu.devices().last().unwrap();
        self.cpu.load(model_path, cpu_dev, None, static_shapes)
    }
}

#[test]
fn engine_bench_measures_each_gpu_and_picks_the_chosen_adapter() {
    for (adapter, expected_id, expected_name) in [
        ("auto", "GPU.0", "Fake iGPU"),
        ("arc", "GPU.1", "Fake Arc dGPU"),
        ("nothing like it", "GPU.0", "Fake iGPU"),
    ] {
        let dir = ScopedDir::new("multigpu");
        let models = dir.path().join("models");
        install_embedding_gemma(&models, None);
        let s = test_settings(dir.path(), |x| {
            x.inference.device = "gpu".into();
            x.inference.gpu_adapter = adapter.into();
        });
        let engine = RealEngine::new_for_test_check(
            s,
            dir.path().to_path_buf(),
            Arc::new(TwoGpuBackend::new()),
            Arc::new(OnnxBackend::new(true).unwrap()),
        );
        let r = engine.bench().expect("bench");
        let mut gpu_rows: Vec<&BenchDevice> =
            r.devices.iter().filter(|d| d.device == "GPU").collect();
        gpu_rows.sort_by(|a, b| a.adapter.cmp(&b.adapter));
        let adapters: Vec<&str> = gpu_rows
            .iter()
            .map(|d| d.adapter.as_deref().unwrap())
            .collect();
        assert_eq!(vec!["Fake Arc dGPU", "Fake iGPU"], adapters);
        assert!(gpu_rows.iter().all(|d| d.ok));
        let autos: Vec<bool> = gpu_rows
            .iter()
            .map(|d| d.adapter_auto)
            .collect();
        // Alphabetically "Fake Arc dGPU" comes before "Fake iGPU".
        assert_eq!(vec![false, true], autos);
        assert_eq!("GPU", r.chosen);
        assert_eq!(Some(expected_name), r.chosen_adapter.as_deref());
        assert_eq!(1, r.devices.iter().filter(|d| d.device == "CPU").count());

        let file: DeviceCacheFile = serde_json::from_slice(
            &fs::read(DataPaths::device_cache(dir.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(expected_id, file.chosen.unwrap().device_id);
        assert_eq!(
            DeviceCacheKey::setting_of("gpu", adapter),
            file.key.unwrap().setting
        );
        for t in r.tokens.unwrap_or_default() {
            assert_eq!("GPU", t.device);
        }
    }
}

trait RealEngineTestExt {
    fn new_for_test_check(
        settings: WorkerSettings,
        home: PathBuf,
        main: Arc<dyn IInferenceBackend>,
        cpu: Arc<dyn IInferenceBackend>,
    ) -> RealEngine;
}

impl RealEngineTestExt for RealEngine {
    fn new_for_test_check(
        settings: WorkerSettings,
        home: PathBuf,
        main: Arc<dyn IInferenceBackend>,
        cpu: Arc<dyn IInferenceBackend>,
    ) -> RealEngine {
        RealEngine::with_backends(settings, home, main, cpu)
    }
}

#[test]
fn engine_bench_with_one_gpu_keeps_todays_rows() {
    let dir = ScopedDir::new("onegpu");
    let models = dir.path().join("models");
    install_embedding_gemma(&models, None);
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    let engine = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    );
    let r = engine.bench().expect("bench");
    assert!(r.devices.iter().all(|d| d.adapter.is_none()));
    assert!(r.chosen_adapter.is_none());
    assert_eq!("CPU", r.chosen);
}

// ---- SyntheticModelTests ----

#[test]
fn ort_and_openvino_cpu_agree_on_the_text_graph() {
    let dir = ScopedDir::new("agree");
    let path = dir.path().join("text_16.onnx");
    fs::copy(data_path("text_16.onnxt"), &path).unwrap();
    let mut ids = vec![0i64; 16];
    let mut mask = vec![0i64; 16];
    for i in 0..6 {
        ids[i] = 10 + i as i64;
        mask[i] = 1;
    }
    let ort = OnnxBackend::new(true).expect("OnnxBackend");
    let s1 = ort
        .load(&path, ort.devices().last().unwrap(), None, None)
        .unwrap();
    let a = s1
        .run(&[
            HostTensor::l("input_ids", ids.clone(), &[1, 16]),
            HostTensor::l("attention_mask", mask.clone(), &[1, 16]),
        ])
        .unwrap()
        .remove("embedding")
        .unwrap()
        .floats
        .unwrap();

    let ov = OpenVinoBackend::try_create().expect("OpenVINO runtime available");
    let ov_cpu = ov
        .devices()
        .iter()
        .find(|d| d.kind == DeviceKind::Cpu)
        .unwrap();
    let s2 = ov.load(&path, ov_cpu, None, None).unwrap();
    assert_eq!(vec![1, 16], s2.inputs()[0].shape);
    let b = s2
        .run(&[
            HostTensor::l("input_ids", ids, &[1, 16]),
            HostTensor::l("attention_mask", mask, &[1, 16]),
        ])
        .unwrap()
        .remove("embedding")
        .unwrap()
        .floats
        .unwrap();
    assert_eq!(768, a.len());
    assert!(VectorMath::cosine(&a, &b).unwrap() > 0.99999);
}

#[test]
fn real_engine_on_cpu_end_to_end_over_the_pipe() {
    let dir = ScopedDir::new("realcpu");
    let models = dir.path().join("models");
    install_embedding_gemma(&models, None);
    install_nsfw(&models);
    install_vad(&models);
    let s = test_settings(dir.path(), |x| {
        x.inference.device = "cpu".into();
        x.index.dim = 512;
    });
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    let engine = Arc::new(RealEngine::with_backends(
        s.clone(),
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    ));
    let mut h = PipeHarness::start_default(engine, s);

    let hello = h
        .call(
            WorkerRequest {
                op: "hello".into(),
                ..Default::default()
            },
            &[],
        )
        .0
        .hello
        .expect("hello");
    assert_eq!("CPU", hello.device);
    assert_eq!("synthetic/embeddinggemma-2", hello.model);
    assert_eq!(512, hello.dim);
    assert!(hello.vad);
    assert!(!hello.asr);
    assert_eq!(
        vec![
            ModelCatalog::SIGLIP,
            ModelCatalog::WHISPER,
            ModelCatalog::NSFW_DETECTOR,
            ModelCatalog::OCR,
            ModelCatalog::NER,
        ],
        hello.missing
    );
    assert!(DataPaths::device_cache(dir.path()).exists());

    let (q, qp) = h.call(
        WorkerRequest {
            op: "embed_text".into(),
            text: Some("quarterly sales".into()),
            kind: Some("query".into()),
            ..Default::default()
        },
        &[],
    );
    assert!(q.ok, "{:?}", q.error);
    assert_eq!(512 * 4, qp.len());
    let (_d, dp) = h.call(
        WorkerRequest {
            op: "embed_text".into(),
            text: Some("quarterly sales".into()),
            kind: Some("document".into()),
            ..Default::default()
        },
        &[],
    );
    assert_ne!(qp, dp);
    let long_txt = vec!["budget"; 200].join(" ");
    assert!(
        h.call(
            WorkerRequest {
                op: "embed_text".into(),
                text: Some(long_txt),
                kind: Some("document".into()),
                ..Default::default()
            },
            &[]
        )
        .0
        .ok
    );

    let blue = bgra(200, 120, 0, 0, 255);
    let (img, ip) = h.call(
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("bgra8".into()),
            width: Some(200),
            height: Some(120),
            stride: Some(800),
            nsfw: Some(0.8),
            ..Default::default()
        },
        &blue,
    );
    assert!(img.ok, "{:?}", img.error);
    assert!(img.dropped.is_none());
    assert_eq!(512 * 4, ip.len());

    let red = bgra(200, 120, 255, 0, 0);
    let (drop_resp, dpay) = h.call(
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("bgra8".into()),
            width: Some(200),
            height: Some(120),
            stride: Some(800),
            nsfw: Some(0.8),
            ..Default::default()
        },
        &red,
    );
    assert!(drop_resp.ok);
    assert_eq!(Some("nsfw"), drop_resp.dropped.as_deref());
    assert!(dpay.is_empty());
    assert!(
        h.call(
            WorkerRequest {
                op: "embed_image".into(),
                format: Some("bgra8".into()),
                width: Some(200),
                height: Some(120),
                stride: Some(800),
                ..Default::default()
            },
            &red
        )
        .0
        .dropped
        .is_none()
    );

    let (silence, _) = h.call(
        WorkerRequest {
            op: "transcribe".into(),
            format: Some("f32".into()),
            sample_rate: Some(16000),
            channels: Some(1),
            source: Some("mic".into()),
            language: Some("auto".into()),
            ..Default::default()
        },
        &pcm_sine(0.0, 16000),
    );
    assert!(silence.ok, "{:?}", silence.error);
    assert!(!silence.transcript.unwrap().speech);

    let (speech, _) = h.call(
        WorkerRequest {
            op: "transcribe".into(),
            format: Some("f32".into()),
            sample_rate: Some(16000),
            channels: Some(1),
            source: Some("mic".into()),
            language: Some("auto".into()),
            ..Default::default()
        },
        &pcm_sine(0.2, 16000),
    );
    assert_eq!(
        Some("Model not installed: whisper-large-v3-turbo."),
        speech.error.as_deref()
    );

    let bench = h
        .call(
            WorkerRequest {
                op: "bench".into(),
                ..Default::default()
            },
            &[],
        )
        .0
        .bench
        .expect("bench");
    assert_eq!("CPU", bench.chosen);
    assert!(bench.devices.iter().all(|b| b.ok));
}

#[test]
fn nsfw_detector_drops_what_the_classifier_passes() {
    let dir = ScopedDir::new("nsfwdet");
    let models = dir.path().join("models");
    install_embedding_gemma(&models, None);
    install_nsfw(&models);
    install_nsfw_detector(&models);
    let s = test_settings(dir.path(), |x| {
        x.inference.device = "cpu".into();
        x.privacy.nsfw_device = "auto".into();
    });
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    let engine = Arc::new(RealEngine::with_backends(
        s.clone(),
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    ));
    let mut h = PipeHarness::start_default(engine, s);

    let hello = h
        .call(
            WorkerRequest {
                op: "hello".into(),
                ..Default::default()
            },
            &[],
        )
        .0
        .hello
        .unwrap();
    assert_eq!(Some("CPU"), hello.nsfw_device.as_deref());
    assert!(!hello.missing.contains(&ModelCatalog::NSFW_DETECTOR.to_string()));

    let green = bgra(200, 120, 0, 255, 0);
    let blue = bgra(200, 120, 0, 0, 255);
    let red = bgra(200, 120, 255, 0, 0);
    let req = |nsfw: Option<f64>, det: Option<f64>| WorkerRequest {
        op: "embed_image".into(),
        format: Some("bgra8".into()),
        width: Some(200),
        height: Some(120),
        stride: Some(800),
        nsfw,
        nsfw_detector: det,
        ..Default::default()
    };

    let (g, gp) = h.call(req(Some(0.8), Some(0.5)), &green);
    assert!(g.ok, "{:?}", g.error);
    assert_eq!(Some("nsfw"), g.dropped.as_deref());
    assert!(gp.is_empty());

    let (g2, g2p) = h.call(req(Some(0.8), None), &green);
    assert!(g2.dropped.is_none());
    assert_eq!(768 * 4, g2p.len());

    let (b, bp) = h.call(req(Some(0.8), Some(0.5)), &blue);
    assert!(b.dropped.is_none());
    assert_eq!(768 * 4, bp.len());

    assert_eq!(
        Some("nsfw"),
        h.call(req(Some(0.8), Some(0.5)), &red).0.dropped.as_deref()
    );
    assert_eq!(
        Some("nsfw"),
        h.call(req(None, Some(0.5)), &green).0.dropped.as_deref()
    );
}

#[test]
fn nsfw_detector_missing_fails_closed_only_when_requested() {
    let dir = ScopedDir::new("nsfwnodet");
    let models = dir.path().join("models");
    install_embedding_gemma(&models, None);
    install_nsfw(&models);
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    let engine = Arc::new(RealEngine::with_backends(
        s.clone(),
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    ));
    let mut h = PipeHarness::start_default(engine, s);

    let hello = h
        .call(
            WorkerRequest {
                op: "hello".into(),
                ..Default::default()
            },
            &[],
        )
        .0
        .hello
        .unwrap();
    assert_eq!(Some("CPU"), hello.nsfw_device.as_deref());
    let blue = bgra(8, 8, 0, 0, 255);
    assert_eq!(
        Some("Model not installed: nsfw-detector."),
        h.call(
            WorkerRequest {
                op: "embed_image".into(),
                format: Some("bgra8".into()),
                width: Some(8),
                height: Some(8),
                stride: Some(32),
                nsfw: Some(0.8),
                nsfw_detector: Some(0.5),
                ..Default::default()
            },
            &blue
        )
        .0
        .error
        .as_deref()
    );
    assert!(
        h.call(
            WorkerRequest {
                op: "embed_image".into(),
                format: Some("bgra8".into()),
                width: Some(8),
                height: Some(8),
                stride: Some(32),
                nsfw: Some(0.8),
                ..Default::default()
            },
            &blue
        )
        .0
        .ok
    );
}

#[test]
fn missing_models_and_broken_models_give_the_wire_errors() {
    let dir = ScopedDir::new("missing");
    let models = dir.path().join("models");
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    {
        let ort = Arc::new(OnnxBackend::new(true).unwrap());
        let engine = Arc::new(RealEngine::with_backends(
            s.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
            ort,
        ));
        let mut h = PipeHarness::start_default(engine, s.clone());
        let hello = h
            .call(
                WorkerRequest {
                    op: "hello".into(),
                    ..Default::default()
                },
                &[],
            )
            .0
            .hello
            .unwrap();
        assert_eq!(8, hello.missing.len());
        assert!(hello.ocr_device.is_none());
        assert_eq!(
            Some("Model not installed: ocr."),
            h.call(
                WorkerRequest {
                    op: "ocr".into(),
                    format: Some("bgra8".into()),
                    width: Some(2),
                    height: Some(2),
                    stride: Some(8),
                    ..Default::default()
                },
                &bgra(2, 2, 1, 2, 3)
            )
            .0
            .error
            .as_deref()
        );
        assert_eq!(
            Some("Model not installed: embeddinggemma-2."),
            h.call(
                WorkerRequest {
                    op: "embed_text".into(),
                    text: Some("x".into()),
                    kind: Some("query".into()),
                    ..Default::default()
                },
                &[]
            )
            .0
            .error
            .as_deref()
        );
        assert_eq!(
            Some("Model not installed: nsfw-classifier."),
            h.call(
                WorkerRequest {
                    op: "embed_image".into(),
                    format: Some("bgra8".into()),
                    width: Some(2),
                    height: Some(2),
                    stride: Some(8),
                    nsfw: Some(0.8),
                    ..Default::default()
                },
                &bgra(2, 2, 1, 2, 3)
            )
            .0
            .error
            .as_deref()
        );
        assert_eq!(
            Some("Model not installed: silero-vad."),
            h.call(
                WorkerRequest {
                    op: "transcribe".into(),
                    format: Some("f32".into()),
                    sample_rate: Some(16000),
                    channels: Some(1),
                    source: Some("mic".into()),
                    ..Default::default()
                },
                &pcm_sine(0.2, 16000)
            )
            .0
            .error
            .as_deref()
        );
        assert_eq!(
            Some("Bad request."),
            h.call(
                WorkerRequest {
                    op: "embed_image".into(),
                    format: Some("png".into()),
                    ..Default::default()
                },
                &[1, 2, 3, 4]
            )
            .0
            .error
            .as_deref()
        );
    }

    install_embedding_gemma(&models, Some("this is not an onnx file"));
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    let engine = Arc::new(RealEngine::with_backends(
        s.clone(),
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    ));
    let mut h = PipeHarness::start_default(engine, s);
    let err = h
        .call(
            WorkerRequest {
                op: "embed_text".into(),
                text: Some("x".into()),
                kind: Some("query".into()),
                ..Default::default()
            },
            &[],
        )
        .0
        .error
        .unwrap();
    assert!(err.starts_with("Device failed: CPU "), "err={err}");
    assert!(err.ends_with('.'));
}

#[test]
fn siglip_fallback_when_primary_fails_everywhere() {
    let dir = ScopedDir::new("siglip");
    let models = dir.path().join("models");
    install_embedding_gemma(&models, Some("broken"));
    install_siglip(&models);
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    let engine = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    );
    let hello = engine.hello(768).unwrap();
    assert_eq!("synthetic/siglip2-base", hello.model);
    let v = engine.embed_text("Some Query", TextKind::Query).unwrap();
    assert_eq!(768, v.len());
    let bytes = bgra(64, 64, 9, 99, 199);
    let img_v = engine
        .embed_image(
            &ImageInput {
                format: "bgra8",
                width: 64,
                height: 64,
                stride: 256,
                bytes: &bytes,
            },
            None,
            1,
            None,
        )
        .unwrap()
        .vector;
    assert!(img_v.is_some());
}

#[test]
fn device_cache_is_reused_and_invalidated_by_model_change() {
    let dir = ScopedDir::new("reuse");
    let models = dir.path().join("models");
    install_embedding_gemma(&models, None);
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    {
        let e1 = RealEngine::with_backends(
            s.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
            Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        );
        e1.hello(768).unwrap();
    }
    let path = DataPaths::device_cache(dir.path());
    let t1 = fs::metadata(&path).unwrap().modified().unwrap();
    thread::sleep(Duration::from_millis(50));
    {
        let e2 = RealEngine::with_backends(
            s.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
            Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        );
        e2.hello(768).unwrap();
    }
    assert_eq!(t1, fs::metadata(&path).unwrap().modified().unwrap());

    let mpath = ModelCatalog::dir(&models, ModelCatalog::EMBEDDING_GEMMA).join("manifest.json");
    let txt = fs::read_to_string(&mpath).unwrap().replace("\"syn1\"", "\"syn2\"");
    fs::write(&mpath, txt).unwrap();
    let e3 = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    );
    assert_eq!("syn2", e3.hello(768).unwrap().revision);
    assert_ne!(t1, fs::metadata(&path).unwrap().modified().unwrap());
}

#[test]
fn table_layout_agrees_across_runtimes_and_npu() {
    let dir = ScopedDir::new("table");
    let models = dir.path().join("models");
    install_embedding_gemma_table_layout(&models);
    let m = ModelCatalog::load(&models, ModelCatalog::EMBEDDING_GEMMA).expect("load table manifest");
    let rgb: Vec<u8> = (0..320 * 180 * 3).map(|i| (i * 31 % 251) as u8).collect();
    let img = RgbImage {
        width: 320,
        height: 180,
        rgb,
    };
    let ort: Arc<dyn IInferenceBackend> = Arc::new(OnnxBackend::new(true).unwrap());
    let ov: Arc<dyn IInferenceBackend> =
        Arc::new(OpenVinoBackend::try_create().expect("OpenVINO available"));

    let mut targets: Vec<(Arc<dyn IInferenceBackend>, DeviceInfo)> = vec![(
        Arc::clone(&ort),
        ort.devices().last().unwrap().clone(),
    )];
    for d in ov.devices() {
        if d.kind != DeviceKind::Gpu {
            targets.push((Arc::clone(&ov), d.clone()));
        }
    }

    let cache_root = persistent_npu_cache_dir("table-layout");
    let mut results = Vec::new();
    let long_doc = vec!["budget meeting"; 20].join(" ");
    for (backend, dev) in targets {
        let mut e = Embedder::new(
            Arc::clone(&backend),
            dev.clone(),
            m.clone(),
            140,
            Some(cache_root.join(&dev.id)),
            None,
        )
        .unwrap();
        e.load_all(true, true).unwrap();
        let t = e.embed_text("quarterly sales", TextKind::Query).unwrap();
        let lt = e.embed_text(&long_doc, TextKind::Document).unwrap();
        let im = e.embed_image(&img, None).unwrap();
        results.push((format!("{}/{}", backend.name(), dev.id), t, lt, im));
    }

    let cpu = &results[0];
    for r in &results {
        let c = [
            VectorMath::cosine(&r.1, &cpu.1).unwrap(),
            VectorMath::cosine(&r.2, &cpu.2).unwrap(),
            VectorMath::cosine(&r.3, &cpu.3).unwrap(),
        ]
        .into_iter()
        .fold(f64::INFINITY, f64::min);
        assert!(c > 0.999, "{} cosine {c}", r.0);
    }
    assert!(VectorMath::cosine(&cpu.1, &cpu.2).unwrap() < 0.999);
}

#[test]
fn openvino_auto_benchmark_uses_the_npu_when_present() {
    let ov = Arc::new(OpenVinoBackend::try_create().expect("OpenVINO available"));
    // Persist the home under BR_TEST_TMP so compiled NPU blobs are reused across runs;
    // delete only the device cache (device.json) so the benchmark itself still runs.
    let home = persistent_npu_cache_dir("ovnpu-home");
    let _ = fs::remove_file(DataPaths::device_cache(&home));
    let models = home.join("models");
    install_embedding_gemma(&models, None);
    let s = test_settings(&home, |_| {});
    let has_npu = ov.devices().iter().any(|d| d.kind == DeviceKind::Npu);
    let engine = RealEngine::with_backends(
        s,
        home,
        ov as Arc<dyn IInferenceBackend>,
        Arc::new(OnnxBackend::new(true).unwrap()),
    );
    let bench = engine.bench().expect("bench");
    assert!(bench.devices.iter().any(|d| d.device == "CPU" && d.ok));
    if has_npu {
        let npu_rows: Vec<&BenchDevice> =
            bench.devices.iter().filter(|d| d.device == "NPU").collect();
        assert_eq!(1, npu_rows.len());
        assert!(npu_rows[0].ok, "{:?}", npu_rows[0].error);
        assert_eq!("NPU", bench.chosen);
        let bytes = bgra(320, 180, 40, 90, 160);
        let v = engine
            .embed_image(
                &ImageInput {
                    format: "bgra8",
                    width: 320,
                    height: 180,
                    stride: 1280,
                    bytes: &bytes,
                },
                None,
                1,
                None,
            )
            .unwrap()
            .vector
            .unwrap();
        assert!(!VectorMath::has_non_finite(&v));
    }
    assert!(!bench.devices.iter().any(|d| d.device == "GPU"));
}

// ---- FlakyGpuBackend / EgBreakingBackend (Sweep2GTests, Sweep4WWorkerTests, Sweep5WWorkerTests) ----

struct FlakyState {
    broken: AtomicBool,
    cpu: OnnxBackend,
    devices: Vec<DeviceInfo>,
}

struct FlakySession {
    inner: Box<dyn IInferenceSession>,
    device: DeviceInfo,
    owner: Arc<FlakyState>,
}

impl IInferenceSession for FlakySession {
    fn device(&self) -> &DeviceInfo {
        &self.device
    }
    fn inputs(&self) -> &[TensorSpec] {
        self.inner.inputs()
    }
    fn outputs(&self) -> &[TensorSpec] {
        self.inner.outputs()
    }
    fn run(&self, inputs: &[HostTensor]) -> Result<HashMap<String, HostTensor>, WorkerError> {
        if self.owner.broken.load(Ordering::Acquire) {
            return Err(WorkerError::DeviceFailed("GPU device removed".into()));
        }
        self.inner.run(inputs)
    }
}

struct FlakyGpuBackend {
    state: Arc<FlakyState>,
}

impl FlakyGpuBackend {
    fn new() -> Self {
        Self {
            state: Arc::new(FlakyState {
                broken: AtomicBool::new(false),
                cpu: OnnxBackend::new(true).unwrap(),
                devices: vec![
                    dev(DeviceKind::Gpu, "GPU.0", "Fake GPU", "OpenVINO", Some(true)),
                    dev(DeviceKind::Cpu, "CPU", "CPU", "OpenVINO", None),
                ],
            }),
        }
    }

    fn set_broken(&self, v: bool) {
        self.state.broken.store(v, Ordering::Release);
    }
}

impl IInferenceBackend for FlakyGpuBackend {
    fn name(&self) -> &str {
        "Fake"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn npu_driver_version(&self) -> &str {
        ""
    }
    fn devices(&self) -> &[DeviceInfo] {
        &self.state.devices
    }
    fn load(
        &self,
        model_path: &Path,
        device: &DeviceInfo,
        _cache_dir: Option<&Path>,
        static_shapes: Option<&HashMap<String, Vec<i64>>>,
    ) -> Result<Box<dyn IInferenceSession>, WorkerError> {
        if self.state.broken.load(Ordering::Acquire) && device.kind == DeviceKind::Gpu {
            return Err(WorkerError::DeviceFailed("GPU device removed".into()));
        }
        let cpu_dev = self.state.cpu.devices().last().unwrap();
        let inner = self
            .state
            .cpu
            .load(model_path, cpu_dev, None, static_shapes)?;
        if device.kind == DeviceKind::Gpu {
            Ok(Box::new(FlakySession {
                inner,
                device: device.clone(),
                owner: Arc::clone(&self.state),
            }))
        } else {
            Ok(inner)
        }
    }
}

struct ModelFailBackend {
    inner: Arc<FlakyGpuBackend>,
    eg_broken: AtomicBool,
}

impl ModelFailBackend {
    fn new() -> Self {
        Self {
            inner: Arc::new(FlakyGpuBackend::new()),
            eg_broken: AtomicBool::new(false),
        }
    }
    fn set_gpu_broken(&self, v: bool) {
        self.inner.set_broken(v);
    }
    fn set_eg_broken(&self, v: bool) {
        self.eg_broken.store(v, Ordering::Release);
    }
}

impl IInferenceBackend for ModelFailBackend {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn version(&self) -> &str {
        self.inner.version()
    }
    fn npu_driver_version(&self) -> &str {
        self.inner.npu_driver_version()
    }
    fn devices(&self) -> &[DeviceInfo] {
        self.inner.devices()
    }
    fn load(
        &self,
        model_path: &Path,
        device: &DeviceInfo,
        cache_dir: Option<&Path>,
        static_shapes: Option<&HashMap<String, Vec<i64>>>,
    ) -> Result<Box<dyn IInferenceSession>, WorkerError> {
        if self.eg_broken.load(Ordering::Acquire)
            && model_path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(ModelCatalog::EMBEDDING_GEMMA)
        {
            return Err(WorkerError::DeviceFailed("runtime broken".into()));
        }
        self.inner.load(model_path, device, cache_dir, static_shapes)
    }
}

// ---- Sweep2GTests (W2 replan tests) ----

#[test]
fn device_lost_after_load_is_planned_again_on_the_next_device() {
    let dir = ScopedDir::new("devlost");
    install_embedding_gemma(&dir.path().join("models"), None);
    let s = test_settings(dir.path(), |x| x.inference.device = "gpu".into());
    let gpu = Arc::new(FlakyGpuBackend::new());
    let engine = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::clone(&gpu) as Arc<dyn IInferenceBackend>,
        Arc::new(OnnxBackend::new(true).unwrap()),
    );
    assert_eq!("GPU", engine.hello(768).unwrap().device);
    assert!(!engine.embed_text("before", TextKind::Query).unwrap().is_empty());

    gpu.set_broken(true);
    for _ in 0..br_worker::engine::real::RUN_FAILURES_BEFORE_REPLAN {
        assert!(matches!(
            engine.embed_text("lost", TextKind::Query),
            Err(WorkerError::DeviceFailed(_))
        ));
    }
    assert!(engine.failed_devices().contains(&"GPU.0".to_string()));
    assert!(!engine.embed_text("after", TextKind::Query).unwrap().is_empty());
    assert_eq!("CPU", engine.hello(768).unwrap().device);

    let file: DeviceCacheFile =
        serde_json::from_slice(&fs::read(DataPaths::device_cache(dir.path())).unwrap()).unwrap();
    assert_eq!("GPU.0", file.chosen.unwrap().device_id);
}

#[test]
fn one_failed_run_does_not_move_the_device() {
    let dir = ScopedDir::new("devonce");
    install_embedding_gemma(&dir.path().join("models"), None);
    let s = test_settings(dir.path(), |x| x.inference.device = "gpu".into());
    let gpu = Arc::new(FlakyGpuBackend::new());
    let engine = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::clone(&gpu) as Arc<dyn IInferenceBackend>,
        Arc::new(OnnxBackend::new(true).unwrap()),
    );
    assert_eq!("GPU", engine.hello(768).unwrap().device);
    gpu.set_broken(true);
    assert!(matches!(
        engine.embed_text("glitch", TextKind::Query),
        Err(WorkerError::DeviceFailed(_))
    ));
    gpu.set_broken(false);
    assert!(!engine.embed_text("fine", TextKind::Query).unwrap().is_empty());
    assert!(engine.failed_devices().is_empty());
    assert_eq!("GPU", engine.hello(768).unwrap().device);
}

// ---- Sweep4WWorkerTests (W2 replan tests) ----

#[test]
fn replan_after_a_device_loss_keeps_the_model() {
    let dir = ScopedDir::new("s4w-replan");
    let models = dir.path().join("models");
    install_embedding_gemma(&models, None);
    install_siglip(&models);
    let s = test_settings(dir.path(), |x| {
        x.inference.device = "gpu".into();
        x.privacy.nsfw_filter = false;
        x.ocr.enabled = false;
    });
    {
        let first = RealEngine::with_backends(
            s.clone(),
            dir.path().to_path_buf(),
            Arc::new(FlakyGpuBackend::new()),
            Arc::new(OnnxBackend::new(true).unwrap()),
        );
        assert_eq!("synthetic/embeddinggemma-2", first.hello(768).unwrap().model);
    }

    let cache_path = DataPaths::device_cache(dir.path());
    let mut cache: DeviceCacheFile =
        serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!("GPU.0", cache.chosen.as_ref().unwrap().device_id);
    cache.chosen.as_mut().unwrap().model = ModelCatalog::SIGLIP.to_string();
    DeviceCache::write(&cache_path, &cache).unwrap();

    let gpu = Arc::new(FlakyGpuBackend::new());
    let engine = RealEngine::with_backends(
        s.clone(),
        dir.path().to_path_buf(),
        Arc::clone(&gpu) as Arc<dyn IInferenceBackend>,
        Arc::new(OnnxBackend::new(true).unwrap()),
    );
    let h = RequestHandler::new(&engine, || s.clone());
    assert_eq!(
        "synthetic/siglip2-base",
        send_req(
            &h,
            &WorkerRequest {
                id: 1,
                op: "hello".into(),
                ..Default::default()
            },
            &[]
        )
        .hello
        .unwrap()
        .model
    );
    gpu.set_broken(true);
    for i in 0..br_worker::engine::real::RUN_FAILURES_BEFORE_REPLAN {
        assert!(
            !send_req(
                &h,
                &WorkerRequest {
                    id: 2 + i as i64,
                    op: "embed_text".into(),
                    text: Some("lost".into()),
                    kind: Some("query".into()),
                    ..Default::default()
                },
                &[]
            )
            .ok
        );
    }
    let after = send_req(
        &h,
        &WorkerRequest {
            id: 9,
            op: "embed_text".into(),
            text: Some("after".into()),
            kind: Some("query".into()),
            ..Default::default()
        },
        &[],
    );
    assert!(after.ok, "{:?}", after.error);
    assert_eq!(Some("synthetic/siglip2-base"), after.model.as_deref());
    assert_eq!(Some("CPU"), after.device.as_deref());
}

fn run_gpu_failure_replan_case(
    label: &str,
    break_eg_before_failures: bool,
    tail: impl FnOnce(&RealEngine, &RequestHandler<'_>, &ModelFailBackend),
) -> (ScopedDir, WorkerSettings) {
    let dir = ScopedDir::new(label);
    let models = dir.path().join("models");
    install_embedding_gemma(&models, None);
    install_siglip(&models);
    let s = test_settings(dir.path(), |x| {
        x.inference.device = "gpu".into();
        x.privacy.nsfw_filter = false;
        x.ocr.enabled = false;
    });
    {
        let first = RealEngine::with_backends(
            s.clone(),
            dir.path().to_path_buf(),
            Arc::new(ModelFailBackend::new()),
            Arc::new(OnnxBackend::new(true).unwrap()),
        );
        assert_eq!("synthetic/embeddinggemma-2", first.hello(768).unwrap().model);
    }

    let b = Arc::new(ModelFailBackend::new());
    {
        let engine = RealEngine::with_backends(
            s.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&b) as Arc<dyn IInferenceBackend>,
            Arc::new(OnnxBackend::new(true).unwrap()),
        );
        let h = RequestHandler::new(&engine, || s.clone());
        assert_eq!(
            "synthetic/embeddinggemma-2",
            send_req(
                &h,
                &WorkerRequest {
                    id: 1,
                    op: "hello".into(),
                    ..Default::default()
                },
                &[]
            )
            .hello
            .unwrap()
            .model
        );
        b.set_gpu_broken(true);
        if break_eg_before_failures {
            b.set_eg_broken(true);
        }
        for i in 0..br_worker::engine::real::RUN_FAILURES_BEFORE_REPLAN {
            assert!(
                !send_req(
                    &h,
                    &WorkerRequest {
                        id: 2 + i as i64,
                        op: "embed_text".into(),
                        text: Some("lost".into()),
                        kind: Some("query".into()),
                        ..Default::default()
                    },
                    &[]
                )
                .ok
            );
        }
        tail(&engine, &h, &b);
    }
    (dir, s)
}

#[test]
fn replan_switch_and_a5_bench_persist_device_cache() {
    // Tail 1: replan that switches model is persisted.
    let (dir, s) = run_gpu_failure_replan_case("s4w-switch", true, |_engine, h, _b| {
        let after = send_req(
            h,
            &WorkerRequest {
                id: 9,
                op: "embed_text".into(),
                text: Some("after".into()),
                kind: Some("query".into()),
                ..Default::default()
            },
            &[],
        );
        assert!(after.ok, "{:?}", after.error);
        assert_eq!(Some("synthetic/siglip2-base"), after.model.as_deref());
    });
    let cache: DeviceCacheFile =
        serde_json::from_slice(&fs::read(DataPaths::device_cache(dir.path())).unwrap()).unwrap();
    assert_eq!(ModelCatalog::SIGLIP, cache.chosen.unwrap().model);
    let next = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::new(ModelFailBackend::new()),
        Arc::new(OnnxBackend::new(true).unwrap()),
    );
    assert_eq!("synthetic/siglip2-base", next.hello(768).unwrap().model);

    // Tail 2 (Sweep5W A5): bench updates device cache when failed_devices is non-empty.
    let (dir, s) = run_gpu_failure_replan_case("s5w-a5", false, |engine, h, b| {
        assert!(engine.failed_devices().contains(&"GPU.0".to_string()));
        b.set_eg_broken(true);
        let bench = send_req(
            h,
            &WorkerRequest {
                id: 10,
                op: "bench".into(),
                ..Default::default()
            },
            &[],
        );
        assert!(bench.ok, "{:?}", bench.error);
        assert_eq!(Some("synthetic/siglip2-base"), bench.model.as_deref());
        assert_eq!(Some("CPU"), bench.device.as_deref());
    });
    let cache: DeviceCacheFile =
        serde_json::from_slice(&fs::read(DataPaths::device_cache(dir.path())).unwrap()).unwrap();
    let chosen = cache.chosen.unwrap();
    assert_eq!(ModelCatalog::SIGLIP, chosen.model);
    assert_eq!("CPU", chosen.device_id);
    let next = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::new(ModelFailBackend::new()),
        Arc::new(OnnxBackend::new(true).unwrap()),
    );
    let next_hello = next.hello(768).unwrap();
    assert_eq!("synthetic/siglip2-base", next_hello.model);
    assert_eq!("CPU", next_hello.device);
}

// ---- Sweep5WWorkerTests (A5_Bench_*) ----

#[test]
fn a5_bench_retests_failed_accelerator_and_updates_device_cache_on_recovery() {
    let dir = ScopedDir::new("s5w-a5-recover");
    install_embedding_gemma(&dir.path().join("models"), None);
    let s = test_settings(dir.path(), |x| {
        x.inference.device = "gpu".into();
        x.privacy.nsfw_filter = false;
        x.ocr.enabled = false;
    });

    let b = Arc::new(ModelFailBackend::new());
    let engine = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::clone(&b) as Arc<dyn IInferenceBackend>,
        Arc::new(OnnxBackend::new(true).unwrap()),
    );
    assert_eq!("GPU", engine.hello(768).unwrap().device);

    b.set_gpu_broken(true);
    for _ in 0..br_worker::engine::real::RUN_FAILURES_BEFORE_REPLAN {
        assert!(matches!(
            engine.embed_text("lost", TextKind::Query),
            Err(WorkerError::DeviceFailed(_))
        ));
    }
    assert!(engine.failed_devices().contains(&"GPU.0".to_string()));

    assert!(!engine.embed_text("cpu-fallback", TextKind::Query).unwrap().is_empty());
    let cache_path = DataPaths::device_cache(dir.path());
    let mut stale: DeviceCacheFile =
        serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    stale.chosen = Some(DeviceChoice {
        device: "CPU".to_string(),
        device_id: "CPU".to_string(),
        ep: "OpenVINO".to_string(),
        model: ModelCatalog::EMBEDDING_GEMMA.to_string(),
        text_device_id: None,
    });
    DeviceCache::write(&cache_path, &stale).unwrap();

    b.set_gpu_broken(false);
    let bench = engine.bench().expect("bench");
    assert_eq!("GPU", bench.chosen);
    assert!(!engine.failed_devices().contains(&"GPU.0".to_string()));

    let updated: DeviceCacheFile =
        serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!("GPU.0", updated.chosen.unwrap().device_id);
}

// ---- Audio & budget tests (from w3) ----

#[test]
fn whisper_auto_skips_the_gpu() {
    let npu = dev_npu();
    let gpu = dev_igpu();
    let cpu = dev_cpu();

    let a1 = DevicePolicy::whisper_allowed("auto", &[npu.clone(), gpu.clone(), cpu.clone()]);
    assert_eq!(vec!["NPU", "CPU"], a1.iter().map(|d| d.id.as_str()).collect::<Vec<_>>());

    let a2 = DevicePolicy::whisper_allowed("auto", &[gpu.clone(), cpu.clone()]);
    assert_eq!(vec!["CPU"], a2.iter().map(|d| d.id.as_str()).collect::<Vec<_>>());

    let g = DevicePolicy::whisper_allowed("gpu", &[npu.clone(), gpu.clone(), cpu.clone()]);
    assert_eq!(vec!["GPU.0", "CPU"], g.iter().map(|d| d.id.as_str()).collect::<Vec<_>>());

    let c = DevicePolicy::whisper_allowed("cpu", &[npu, gpu, cpu]);
    assert_eq!(vec!["CPU"], c.iter().map(|d| d.id.as_str()).collect::<Vec<_>>());
}

#[test]
fn bench_drops_the_graphs_of_other_budgets() {
    let dir = ScopedDir::new("s3k-c13");
    let model_dir = dir.path().join("models");
    let gdir = ModelCatalog::dir(&model_dir, ModelCatalog::EMBEDDING_GEMMA);
    let files = vec![
        copy_model_file(&gdir, "sp.model", "tokenizer.model"),
        copy_model_file(&gdir, "text_16.onnxt", "text_16.onnx"),
        copy_model_file(&gdir, "text_32.onnxt", "text_32.onnx"),
        copy_model_file(&gdir, "vision_28.onnxt", "vision_70.onnx"),
        copy_model_file(&gdir, "fusion_28.onnxt", "fusion_70.onnx"),
        copy_model_file(&gdir, "vision_28.onnxt", "vision_140.onnx"),
        copy_model_file(&gdir, "fusion_28.onnxt", "fusion_140.onnx"),
        copy_model_file(&gdir, "vision_28.onnxt", "vision_280.onnx"),
        copy_model_file(&gdir, "fusion_28.onnxt", "fusion_280.onnx"),
    ];
    let mut ids = vec![300i64; 32];
    ids[0] = 2;
    ids[31] = 1;
    let vision = |tokens: i32, suffix: &str| VisionGraph {
        tokens,
        file: format!("vision_{suffix}.onnx"),
        height: 64,
        width: 112,
        output: "image_embeds".into(),
        fusion: Some(FusionGraph {
            file: format!("fusion_{suffix}.onnx"),
            input_ids_input: Some("input_ids".into()),
            attention_mask_input: Some("attention_mask".into()),
            input_ids: ids.clone(),
            ..FusionGraph::default()
        }),
        ..VisionGraph::default()
    };
    let manifest = ModelManifest {
        name: ModelCatalog::EMBEDDING_GEMMA.into(),
        id: "synthetic/embeddinggemma-2".into(),
        revision: "syn1".into(),
        dim: 768,
        tokenizer: Some(TokenizerSpec {
            file: "tokenizer.model".into(),
            add_bos: true,
            add_eos: true,
            bos_id: 2,
            eos_id: 1,
            pad_id: 0,
            ..TokenizerSpec::default()
        }),
        prompts: Some(PromptSpec {
            query: "task: search result | query: {text}".into(),
            document: "title: none | text: {text}".into(),
        }),
        text: vec![
            TextGraph {
                file: "text_16.onnx".into(),
                seq: 16,
                ..TextGraph::default()
            },
            TextGraph {
                file: "text_32.onnx".into(),
                seq: 32,
                ..TextGraph::default()
            },
        ],
        vision: vec![vision(70, "70"), vision(140, "140"), vision(280, "280")],
        files,
        ..ModelManifest::default()
    };
    write_manifest(&gdir, &manifest);

    let s = test_settings(dir.path(), |x| {
        x.inference.device = "cpu".into();
        x.inference.image_tokens = 140;
        x.privacy.nsfw_filter = false;
        x.ocr.enabled = false;
    });
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    let engine = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    );
    let res = engine.bench().expect("bench");
    assert_eq!(3, res.tokens.as_ref().unwrap().len());

    let keys = engine.embedder_session_keys();
    assert!(keys.iter().any(|k| k.contains("vision_140.onnx")));
    assert!(keys.iter().any(|k| k.contains("fusion_140.onnx")));
    assert!(!keys.iter().any(|k| k.contains("70.onnx")));
    assert!(!keys.iter().any(|k| k.contains("280.onnx")));
}


// ---- UnitTests: SmallPartsTests.Vad_segmenter_rules ----

#[test]
fn vad_segmenter_rules() {
    // C# UnitTests.Vad_segmenter_rules on synthetic vad.onnxt: 1 s silence, 1 s tone, 1 s silence.
    let ort = OnnxBackend::new(true).unwrap();
    let vad = SileroVad::new(&ort, &data_path("vad.onnxt")).unwrap();
    let mut pcm = vec![0.0f32; 48_000];
    for (i, sample) in pcm.iter_mut().enumerate().take(32_000).skip(16_000) {
        *sample = 0.2 * ((i as f32) * 0.05).sin();
    }
    let segs = vad.segments(&pcm).unwrap();
    assert_eq!(1, segs.len());
    assert!((900..=1000).contains(&segs[0].0), "start={}", segs[0].0);
    assert!((2000..=2100).contains(&segs[0].1), "end={}", segs[0].1);

    assert!(vad.segments(&[0.0f32; 16_000]).unwrap().is_empty());

    // Too short to count (100 ms).
    let mut blip = vec![0.0f32; 16_000];
    blip[8000..9600].fill(0.2);
    assert!(vad.segments(&blip).unwrap().is_empty());

    // Pure segmenter edge rules: silence, short blip, hysteresis + pad (end = temp_end + pad = 640 + 30 = 670 ms), and multi-segment split.
    assert!(SileroVad::segment_probs(&[0.0; 100], 100 * 512, 0.5, 250, 100, 30).is_empty());

    let mut short = vec![0.0f32; 40];
    short[5..10].fill(0.9);
    assert!(SileroVad::segment_probs(&short, 40 * 512, 0.5, 250, 100, 30).is_empty());

    let mut one = vec![0.0f32; 50];
    one[10..20].fill(0.9);
    one[15] = 0.4; // >= threshold - 0.15 (0.35): stays triggered
    let rule_segs = SileroVad::segment_probs(&one, 50 * 512, 0.5, 250, 100, 30);
    assert_eq!(vec![(290, 670)], rule_segs);

    let mut two = vec![0.0f32; 80];
    two[5..16].fill(0.9);
    two[25..36].fill(0.9);
    let segs2 = SileroVad::segment_probs(&two, 80 * 512, 0.5, 250, 100, 30);
    assert_eq!(2, segs2.len());
    assert!(segs2[0].1 <= segs2[1].0);
}
