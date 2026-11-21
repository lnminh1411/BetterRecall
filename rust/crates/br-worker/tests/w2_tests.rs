#![allow(clippy::too_many_arguments, clippy::identity_op, clippy::cloned_ref_to_slice_refs)] // test helpers and readable index math
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use br_contracts::{
    BenchDevice, BenchResult, BinaryFrame, DataPaths, PipeNames, Settings, WorkerRequest,
    WorkerResponse,
};
use br_worker::engine::{IWorkerEngine, ImageInput, RealEngine, TextKind};
use br_worker::hosting::WorkerSettings;
use br_worker::inference::device_selection::{
    BenchOutcome, DeviceCache, DeviceCacheFile, DeviceCacheKey, DeviceChoice, DevicePolicy,
};
use br_worker::inference::dxgi::{DxgiAdapter, DxgiAdapters};
use br_worker::inference::gpu_selection::GpuSelection;
use br_worker::inference::nsfw_plan::NsfwDevicePlan;
use br_worker::inference::onnx::OnnxBackend;
use br_worker::inference::openvino::OpenVinoBackend;
use br_worker::inference::{
    DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession, TensorSpec,
};
use br_worker::models::catalog::ModelCatalog;
use br_worker::models::embedder::Embedder;
use br_worker::models::image_pipeline::{ImagePipeline, RgbImage};
use br_worker::models::manifest::{
    ClassifierSpec, DetectorSpec, EmbeddingTableSpec, FusionGraph, ImagePreprocessSpec,
    ManifestFile, ModelManifest, PromptSpec, TextGraph, TokenizerSpec, VisionGraph,
};
use br_worker::models::nsfw_classifier::NsfwClassifier;
use br_worker::models::nsfw_detector::NsfwDetector;
use br_worker::models::tokenizer::GemmaTokenizer;
use br_worker::protocol::{AgentPipe, LoopExit, RequestHandler, WorkerError, WorkerLoop};
use br_worker::vectors::VectorMath;
use image::{ImageFormat, Rgb, RgbImage as ImgRgb};
use serde::Deserialize;
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::PCWSTR;

// ---- TestEnv helpers ----

struct ScopedDir {
    path: PathBuf,
}

impl ScopedDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::var_os("BR_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base
            .join("betterrecall-worker-rust")
            .join(format!("{tag}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create test temp dir");
        Self { path: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn data_path(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/BetterRecall.Worker.Tests/TestData")
        .join(file)
}

fn test_settings(home: &Path, edit: impl FnOnce(&mut Settings)) -> WorkerSettings {
    let mut s = Settings::default();
    s.inference.model_dir = home.join("models").to_string_lossy().into_owned();
    edit(&mut s);
    fs::create_dir_all(home).expect("create home dir");
    let json = serde_json::to_vec(&s).expect("serialize settings");
    fs::write(DataPaths::settings_file(home), json).expect("write settings.json");
    WorkerSettings::from(&s, home)
}

fn bgra(w: usize, h: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut p = vec![0u8; w * h * 4];
    for i in 0..(w * h) {
        p[i * 4] = b;
        p[i * 4 + 1] = g;
        p[i * 4 + 2] = r;
        p[i * 4 + 3] = 255;
    }
    p
}

fn pcm_sine(amplitude: f32, samples: usize) -> Vec<u8> {
    let pcm: Vec<f32> = (0..samples)
        .map(|i| amplitude * ((i as f32) * 0.05).sin())
        .collect();
    BinaryFrame::floats_to_bytes(&pcm)
}

fn copy_model_file(dir: &Path, src: &str, dst: &str) -> ManifestFile {
    fs::create_dir_all(dir).expect("create model dir");
    let to = dir.join(dst);
    fs::copy(data_path(src), &to).expect("copy test data file");
    let size = fs::metadata(&to).expect("stat copied file").len() as i64;
    ManifestFile {
        file: dst.to_string(),
        size,
        sha256: String::new(),
        source: None,
    }
}

fn write_manifest(dir: &Path, m: &ModelManifest) {
    let json = serde_json::to_vec_pretty(m).expect("serialize manifest");
    fs::write(dir.join("manifest.json"), json).expect("write manifest.json");
}

fn install_embedding_gemma(model_dir: &Path, corrupt_vision: Option<&str>) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::EMBEDDING_GEMMA);
    let mut files = vec![
        copy_model_file(&dir, "sp.model", "tokenizer.model"),
        copy_model_file(&dir, "text_16.onnxt", "text_16.onnx"),
        copy_model_file(&dir, "text_32.onnxt", "text_32.onnx"),
        copy_model_file(&dir, "vision_28.onnxt", "vision_70.onnx"),
        copy_model_file(&dir, "fusion_28.onnxt", "fusion_70.onnx"),
    ];
    if let Some(corrupt) = corrupt_vision {
        fs::write(dir.join("vision_70.onnx"), corrupt).expect("corrupt vision");
    }
    files[3].size = fs::metadata(dir.join("vision_70.onnx"))
        .expect("vision size")
        .len() as i64;
    let mut ids = vec![300i64; 32];
    ids[0] = 2;
    ids[31] = 1;
    let m = ModelManifest {
        name: ModelCatalog::EMBEDDING_GEMMA.to_string(),
        id: "synthetic/embeddinggemma-2".to_string(),
        revision: "syn1".to_string(),
        dim: 768,
        tokenizer: Some(TokenizerSpec {
            file: "tokenizer.model".to_string(),
            add_bos: true,
            add_eos: true,
            bos_id: 2,
            eos_id: 1,
            pad_id: 0,
            ..TokenizerSpec::default()
        }),
        prompts: Some(PromptSpec {
            query: "task: search result | query: {text}".to_string(),
            document: "title: none | text: {text}".to_string(),
        }),
        text: vec![
            TextGraph {
                file: "text_16.onnx".to_string(),
                seq: 16,
                ..TextGraph::default()
            },
            TextGraph {
                file: "text_32.onnx".to_string(),
                seq: 32,
                ..TextGraph::default()
            },
        ],
        vision: vec![VisionGraph {
            tokens: 70,
            file: "vision_70.onnx".to_string(),
            height: 64,
            width: 112,
            output: "image_embeds".to_string(),
            fusion: Some(FusionGraph {
                file: "fusion_70.onnx".to_string(),
                input_ids_input: Some("input_ids".to_string()),
                attention_mask_input: Some("attention_mask".to_string()),
                input_ids: ids,
                ..FusionGraph::default()
            }),
            ..VisionGraph::default()
        }],
        files,
        ..ModelManifest::default()
    };
    write_manifest(&dir, &m);
}

fn install_embedding_gemma_table_layout(model_dir: &Path) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::EMBEDDING_GEMMA);
    let files = vec![
        copy_model_file(&dir, "sp.model", "tokenizer.model"),
        copy_model_file(&dir, "embed_tokens.bf16", "embed_tokens.bf16"),
        copy_model_file(&dir, "text_dyn.onnxt", "text.onnx"),
        copy_model_file(&dir, "vision_28.onnxt", "vision_140.onnx"),
    ];
    let m = ModelManifest {
        name: ModelCatalog::EMBEDDING_GEMMA.to_string(),
        id: "synthetic/embeddinggemma-2-table".to_string(),
        revision: "syn1".to_string(),
        dim: 768,
        tokenizer: Some(TokenizerSpec {
            file: "tokenizer.model".to_string(),
            add_bos: true,
            add_eos: true,
            bos_id: 2,
            eos_id: 1,
            pad_id: 0,
            ..TokenizerSpec::default()
        }),
        embeddings: Some(EmbeddingTableSpec {
            file: "embed_tokens.bf16".to_string(),
            rows: 320,
            dim: 32,
            dtype: "bf16".to_string(),
            scale: 1.5,
        }),
        prompts: Some(PromptSpec {
            query: "task: search result | query: {text}".to_string(),
            document: "title: none | text: {text}".to_string(),
        }),
        text: vec![
            TextGraph {
                file: "text.onnx".to_string(),
                seq: 16,
                inputs_embeds: Some("inputs_embeds".to_string()),
                ..TextGraph::default()
            },
            TextGraph {
                file: "text.onnx".to_string(),
                seq: 64,
                inputs_embeds: Some("inputs_embeds".to_string()),
                ..TextGraph::default()
            },
        ],
        vision: vec![VisionGraph {
            tokens: 28,
            file: "vision_140.onnx".to_string(),
            height: 64,
            width: 112,
            output: "image_embeds".to_string(),
            preprocess: ImagePreprocessSpec {
                resample: "bicubic".to_string(),
                rescale: 1.0 / 255.0,
                mean: vec![0.0, 0.0, 0.0],
                std: vec![1.0, 1.0, 1.0],
                ..ImagePreprocessSpec::default()
            },
            fusion: Some(FusionGraph {
                file: "text.onnx".to_string(),
                inputs_embeds: Some("inputs_embeds".to_string()),
                attention_mask_input: Some("attention_mask".to_string()),
                prefix_ids: vec![2, 300],
                suffix_ids: vec![301, 1],
                ..FusionGraph::default()
            }),
            ..VisionGraph::default()
        }],
        files,
        ..ModelManifest::default()
    };
    write_manifest(&dir, &m);
}

fn install_siglip(model_dir: &Path) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::SIGLIP);
    let files = vec![
        copy_model_file(&dir, "sp.model", "tokenizer.model"),
        copy_model_file(&dir, "text_16.onnxt", "text.onnx"),
        copy_model_file(&dir, "siglip_vision.onnxt", "vision.onnx"),
    ];
    let m = ModelManifest {
        name: ModelCatalog::SIGLIP.to_string(),
        id: "synthetic/siglip2-base".to_string(),
        revision: "syn1".to_string(),
        dim: 768,
        tokenizer: Some(TokenizerSpec {
            add_bos: false,
            add_eos: true,
            eos_id: 1,
            pad_id: 0,
            pad_to_max: true,
            lowercase: true,
            ..TokenizerSpec::default()
        }),
        text: vec![TextGraph {
            file: "text.onnx".to_string(),
            seq: 16,
            ..TextGraph::default()
        }],
        vision: vec![VisionGraph {
            tokens: 196,
            file: "vision.onnx".to_string(),
            height: 64,
            width: 64,
            output: "embedding".to_string(),
            ..VisionGraph::default()
        }],
        files,
        ..ModelManifest::default()
    };
    write_manifest(&dir, &m);
}

fn install_nsfw(model_dir: &Path) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::NSFW);
    let f = copy_model_file(&dir, "nsfw.onnxt", "model_int8.onnx");
    let m = ModelManifest {
        name: ModelCatalog::NSFW.to_string(),
        id: "synthetic/nsfw".to_string(),
        classifier: Some(ClassifierSpec {
            file: "model_int8.onnx".to_string(),
            output: "logits".to_string(),
            labels: vec!["normal".to_string(), "nsfw".to_string()],
            block_labels: vec!["nsfw".to_string()],
            ..ClassifierSpec::default()
        }),
        files: vec![f],
        ..ModelManifest::default()
    };
    write_manifest(&dir, &m);
}

const NUDENET_LABELS: [&str; 18] = [
    "FEMALE_GENITALIA_COVERED",
    "FACE_FEMALE",
    "BUTTOCKS_EXPOSED",
    "FEMALE_BREAST_EXPOSED",
    "FEMALE_GENITALIA_EXPOSED",
    "MALE_BREAST_EXPOSED",
    "ANUS_EXPOSED",
    "FEET_EXPOSED",
    "BELLY_COVERED",
    "FEET_COVERED",
    "ARMPITS_COVERED",
    "ARMPITS_EXPOSED",
    "FACE_MALE",
    "BELLY_EXPOSED",
    "MALE_GENITALIA_EXPOSED",
    "ANUS_COVERED",
    "FEMALE_BREAST_COVERED",
    "BUTTOCKS_COVERED",
];

fn install_nsfw_detector(model_dir: &Path) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::NSFW_DETECTOR);
    let f = copy_model_file(&dir, "nsfw_detector.onnxt", "model.onnx");
    let m = ModelManifest {
        name: ModelCatalog::NSFW_DETECTOR.to_string(),
        id: "synthetic/nsfw-detector".to_string(),
        detector: Some(DetectorSpec {
            file: "model.onnx".to_string(),
            size: 64,
            labels: NUDENET_LABELS.iter().map(|s| (*s).to_string()).collect(),
            block_labels: vec![
                "FEMALE_BREAST_EXPOSED".to_string(),
                "BUTTOCKS_COVERED".to_string(),
            ],
            ..DetectorSpec::default()
        }),
        files: vec![f],
        ..ModelManifest::default()
    };
    write_manifest(&dir, &m);
}

fn install_vad(model_dir: &Path) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::VAD);
    copy_model_file(&dir, "vad.onnxt", ModelCatalog::VAD_FILE);
}

fn send_req(h: &RequestHandler<'_>, r: &WorkerRequest, payload: &[u8]) -> WorkerResponse {
    let json = serde_json::to_vec(r).expect("serialize request");
    h.handle(&json, payload).header
}

// ---- Pipe harness for SyntheticModelTests ----

fn create_overlapped_pipe_server(name: &str) -> (HANDLE, HANDLE) {
    let full_name = format!(r"\\.\pipe\{name}");
    let wide: Vec<u16> = full_name.encode_utf16().chain(Some(0)).collect();
    let server = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            65_536,
            65_536,
            0,
            None,
        )
    };
    assert!(!server.is_invalid(), "CreateNamedPipeW failed");
    let ev = unsafe { CreateEventW(None, true, false, None) }.expect("CreateEventW");
    let mut ov = OVERLAPPED {
        hEvent: ev,
        ..Default::default()
    };
    if let Err(e) = unsafe { ConnectNamedPipe(server, Some(&mut ov)) } {
        assert_eq!(e.code(), ERROR_IO_PENDING.to_hresult());
    }
    (server, ev)
}

fn finish_connect(server: HANDLE, ev: HANDLE) {
    let ov = OVERLAPPED {
        hEvent: ev,
        ..Default::default()
    };
    let mut dummy = 0u32;
    unsafe { GetOverlappedResult(server, &ov, &mut dummy, true) }.expect("wait ConnectNamedPipe");
    unsafe {
        let _ = CloseHandle(ev);
    }
}

struct PipeHarness {
    server: Option<AgentPipe>,
    loop_thread: Option<JoinHandle<LoopExit>>,
    next_id: i64,
}

impl PipeHarness {
    fn start(engine: Arc<dyn IWorkerEngine + Send + Sync>, settings: WorkerSettings) -> Self {
        let name = PipeNames::new_worker();
        let (server_raw, ev) = create_overlapped_pipe_server(&name);
        let client = AgentPipe::connect(&name, Duration::from_secs(5)).expect("connect pipe");
        finish_connect(server_raw, ev);
        let server = AgentPipe::from_handle(server_raw);
        let loop_thread = thread::spawn(move || {
            let s_clone = settings.clone();
            let handler = RequestHandler::new(engine.as_ref(), move || s_clone.clone());
            WorkerLoop::run(
                &client,
                &handler,
                Duration::from_secs(60),
                WorkerLoop::KEEPALIVE_EVERY,
                WorkerLoop::KEEPALIVE_MAX,
            )
        });
        Self {
            server: Some(server),
            loop_thread: Some(loop_thread),
            next_id: 0,
        }
    }

    fn call(&mut self, mut r: WorkerRequest, payload: &[u8]) -> (WorkerResponse, Vec<u8>) {
        use br_worker::protocol::worker_loop::{FrameRead, PipeIo};
        if r.id == 0 {
            self.next_id += 1;
            r.id = self.next_id;
        }
        let header = serde_json::to_vec(&r).expect("serialize request");
        let server = self.server.as_ref().expect("server active");
        server
            .write_frame(&header, payload)
            .expect("write frame");
        let FrameRead::Frame(h, p) = server
            .read_frame_idle(Duration::from_secs(120))
            .expect("read frame")
        else {
            panic!("unexpected eof or timeout");
        };
        let resp: WorkerResponse = serde_json::from_slice(&h).expect("deserialize WorkerResponse");
        (resp, p)
    }
}

impl Drop for PipeHarness {
    fn drop(&mut self) {
        self.server.take();
        if let Some(t) = self.loop_thread.take() {
            let _ = t.join();
        }
    }
}

// ---- UnitTests: NsfwGateTests ----

fn adapter(
    index: i32,
    description: &str,
    dedicated_video_memory: u64,
    flags: u32,
    vendor_id: u32,
) -> DxgiAdapter {
    DxgiAdapter {
        index,
        description: description.to_string(),
        dedicated_video_memory,
        flags,
        vendor_id,
        device_id: 0,
    }
}

fn dev(kind: DeviceKind, id: &str, name: &str, ep: &str, integrated: Option<bool>) -> DeviceInfo {
    DeviceInfo::new(kind, id, name, ep, integrated)
}

#[test]
fn device_order_all_present() {
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
    let avail = [cpu, dml, ov_dgpu, igpu, npu, ov_cpu];

    for (setting, expected) in [
        ("auto", "NPU,GPU.0,DML:1,CPU"),
        ("npu", "NPU,CPU"),
        ("igpu", "GPU.0,CPU"),
        ("gpu", "DML:1,CPU"),
        ("cpu", "CPU"),
        ("bogus", "NPU,GPU.0,DML:1,CPU"),
    ] {
        let ids: Vec<String> = NsfwDevicePlan::order(setting, &avail)
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(expected, ids.join(","));
    }
}

#[test]
fn device_order_missing_devices() {
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
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "CPU", None);
    let avail = [igpu, ov_dgpu, cpu];
    for (setting, expected) in [
        ("auto", "GPU.0,CPU"),
        ("npu", "CPU"),
        ("gpu", "CPU"),
        ("igpu", "GPU.0,CPU"),
    ] {
        let ids: Vec<String> = NsfwDevicePlan::order(setting, &avail)
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(expected, ids.join(","));
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
fn dxgi_pick() {
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
    let list = [rx6600, rtx3050, intel_igpu, basic];

    for (setting, expected) in [
        ("auto", 0),
        ("AUTO", 0),
        ("", 0),
        ("rtx 3050", 1),
        ("  RTX  ", 1),
        ("Intel", 2),
        ("Basic Render", 3),
        ("Quadro", -1),
    ] {
        let got = DxgiAdapters::pick(Some(setting), &list)
            .map(|a| a.index)
            .unwrap_or(-1);
        assert_eq!(expected, got, "setting={setting}");
    }
}

#[test]
fn dxgi_auto_skips_software_basic_and_intel_integrated() {
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
    assert!(DxgiAdapters::pick(Some("auto"), &[intel_igpu.clone(), basic]).is_none());
    assert_eq!(
        1,
        DxgiAdapters::pick(Some("auto"), &[intel_igpu.clone(), rtx3050, rx6600])
            .expect("discrete")
            .index
    );
    let arc = adapter(4, "Intel(R) Arc(TM) A770 Graphics", 16u64 << 30, 0, 0x8086);
    assert_eq!(
        4,
        DxgiAdapters::pick(Some("auto"), &[intel_igpu, arc])
            .expect("arc discrete")
            .index
    );
    let warp = adapter(
        5,
        "Some software adapter",
        0,
        DxgiAdapter::FLAG_SOFTWARE,
        0x1234,
    );
    assert!(DxgiAdapters::pick(Some("auto"), &[warp]).is_none());
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
        labels: NUDENET_LABELS.iter().map(|s| (*s).to_string()).collect(),
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
fn allowed_devices() {
    let npu = dev(DeviceKind::Npu, "NPU", "Intel(R) AI Boost", "OpenVINO", None);
    let gpu = dev(DeviceKind::Gpu, "GPU.0", "Intel(R) Graphics", "OpenVINO", None);
    let cpu = dev(DeviceKind::Cpu, "CPU", "CPU", "OpenVINO", None);

    for (setting, has_npu, has_gpu, expected) in [
        ("auto", true, true, "NPU,CPU"),
        ("auto", false, true, "GPU.0,CPU"),
        ("auto", false, false, "CPU"),
        ("npu", true, true, "NPU,CPU"),
        ("npu", false, true, "CPU"),
        ("gpu", true, true, "GPU.0,CPU"),
        ("cpu", true, true, "CPU"),
    ] {
        let mut avail = Vec::new();
        if has_npu {
            avail.push(npu.clone());
        }
        if has_gpu {
            avail.push(gpu.clone());
        }
        avail.push(cpu.clone());
        let ids: Vec<String> = DevicePolicy::allowed(setting, &avail, None)
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(expected, ids.join(","));
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
fn key_ids_change_with_every_part() {
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

// ---- UnitTests: SmallPartsTests (W2 parts) ----

#[test]
fn tokenizer_matches_python_sentencepiece() {
    #[derive(Deserialize)]
    struct SpCase {
        text: String,
        ids: Vec<i64>,
    }

    let spec = TokenizerSpec {
        add_bos: false,
        add_eos: false,
        ..TokenizerSpec::default()
    };
    let tok = GemmaTokenizer::from_file(&data_path("sp.model"), spec).expect("load sp.model");
    let cases: Vec<SpCase> =
        serde_json::from_str(&fs::read_to_string(data_path("sp_expected.json")).unwrap())
            .expect("parse sp_expected.json");
    for c in cases {
        assert_eq!(c.ids, tok.encode(&c.text, 1000), "text={:?}", c.text);
    }
}

#[test]
fn real_gemma_tokenizer_matches_sentencepiece_when_available() {
    #[derive(Deserialize)]
    struct SpCase {
        text: String,
        ids: Vec<i64>,
    }
    let (Some(model), Some(expected)) = (
        std::env::var_os("BR_GEMMA_TOKENIZER"),
        std::env::var_os("BR_GEMMA_EXPECTED"),
    ) else {
        return;
    };
    let model = PathBuf::from(model);
    let expected = PathBuf::from(expected);
    if !model.is_file() || !expected.is_file() {
        return;
    }
    let tok = GemmaTokenizer::from_file(
        &model,
        TokenizerSpec {
            add_bos: false,
            add_eos: false,
            ..TokenizerSpec::default()
        },
    )
    .expect("load real tokenizer");
    let cases: Vec<SpCase> =
        serde_json::from_str(&fs::read_to_string(expected).unwrap()).unwrap();
    for c in cases {
        assert_eq!(c.ids, tok.encode(&c.text, 100_000));
    }
}

#[test]
fn tokenizer_adds_bos_eos_and_truncates() {
    let tok = GemmaTokenizer::from_file(
        &data_path("sp.model"),
        TokenizerSpec {
            add_bos: true,
            add_eos: true,
            bos_id: 2,
            eos_id: 1,
            ..TokenizerSpec::default()
        },
    )
    .expect("load sp.model");
    let ids = tok.encode(
        "title: none | text: a spreadsheet with quarterly sales figures and much more text",
        8,
    );
    assert_eq!(8, ids.len());
    assert_eq!(2, ids[0]);
    assert_eq!(1, *ids.last().unwrap());
}

#[test]
fn resize_matches_pillow_within_one_level() {
    #[derive(Deserialize)]
    struct ResizeRefEntry {
        mode: String,
        w: i32,
        h: i32,
        rgb: Vec<u8>,
    }
    #[derive(Deserialize)]
    struct ResizeRefRoot {
        w: i32,
        h: i32,
        rgb: Vec<u8>,
        refs: Vec<ResizeRefEntry>,
    }

    let root: ResizeRefRoot =
        serde_json::from_str(&fs::read_to_string(data_path("resize_ref.json")).unwrap()).unwrap();
    let src = RgbImage {
        width: root.w,
        height: root.h,
        rgb: root.rgb,
    };
    for r in root.refs {
        let got = ImagePipeline::resize(&src, r.w, r.h, &r.mode);
        let max_diff = r
            .rgb
            .iter()
            .zip(got.rgb.iter())
            .map(|(&a, &b)| (a as i32 - b as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_diff <= 1,
            "{} {}x{}: max diff {max_diff}",
            r.mode,
            r.w,
            r.h
        );
    }
}

#[test]
fn decode_png_and_webp_through_image() {
    let img_buf = ImgRgb::from_pixel(16, 8, Rgb([200, 100, 50]));
    let dyn_img = image::DynamicImage::ImageRgb8(img_buf);
    for (fmt_str, enc_fmt) in [
        ("png", ImageFormat::Png),
        ("webp", ImageFormat::WebP),
        ("jpeg", ImageFormat::Jpeg),
    ] {
        let mut encoded = Vec::new();
        dyn_img
            .write_to(&mut Cursor::new(&mut encoded), enc_fmt)
            .expect("encode image");
        let decoded = ImagePipeline::decode(&ImageInput {
            format: fmt_str,
            width: 0,
            height: 0,
            stride: 0,
            bytes: &encoded,
        })
        .expect("decode image");
        assert_eq!(16, decoded.width);
        assert!((195..=205).contains(&decoded.rgb[0]));
        assert!((95..=105).contains(&decoded.rgb[1]));
    }
    assert!(matches!(
        ImagePipeline::decode(&ImageInput {
            format: "png",
            width: 0,
            height: 0,
            stride: 0,
            bytes: &[1, 2, 3],
        }),
        Err(WorkerError::BadRequest(_))
    ));
    let mut png = Vec::new();
    dyn_img
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .unwrap();
    assert!(matches!(
        ImagePipeline::decode(&ImageInput {
            format: "webp",
            width: 0,
            height: 0,
            stride: 0,
            bytes: &png,
        }),
        Err(WorkerError::BadRequest(_))
    ));
}

#[test]
fn nsfw_softmax() {
    assert!((NsfwClassifier::block_probability(&[0.0, 0.0], &[1]) - 0.5).abs() < 1e-6);
    assert!(NsfwClassifier::block_probability(&[0.0, 10.0], &[1]) > 0.9999);
}

#[test]
fn nsfw_grid2_quadrants_and_centre() {
    let t = NsfwClassifier::tiles(1920, 1080, 224, 224, 2);
    assert_eq!(
        vec![
            (0, 0, 960, 540),
            (960, 0, 960, 540),
            (0, 540, 960, 540),
            (960, 540, 960, 540),
            (480, 270, 960, 540),
        ],
        t
    );
    for (x, y, w, h) in NsfwClassifier::tiles(1281, 1025, 224, 224, 2) {
        assert!(w == 640 && h == 512 && x + w <= 1281 && y + h <= 1025);
    }
}

#[test]
fn nsfw_grid3_ninths_inside_frame() {
    let t = NsfwClassifier::tiles(1920, 1080, 224, 224, 3);
    assert_eq!(9, t.len());
    assert_eq!((1280, 720, 640, 360), *t.last().unwrap());
    let uniq: HashSet<(i32, i32, i32, i32)> = t.into_iter().collect();
    assert_eq!(9, uniq.len());
}

#[test]
fn nsfw_grid_falls_back_on_small_frames() {
    assert!(NsfwClassifier::tiles(1920, 1080, 224, 224, 1).is_empty());
    assert_eq!(5, NsfwClassifier::tiles(600, 600, 224, 224, 3).len());
    assert!(NsfwClassifier::tiles(447, 447, 224, 224, 3).is_empty());
    assert!(NsfwClassifier::tiles(400, 1080, 224, 224, 2).is_empty());
    assert_eq!(5, NsfwClassifier::tiles(448, 448, 224, 224, 2).len());
}

#[test]
fn crop_copies_rectangle() {
    let rgb: Vec<u8> = (0..(4 * 3 * 3) as u8).collect();
    let img = RgbImage {
        width: 4,
        height: 3,
        rgb,
    };
    let c = ImagePipeline::crop(&img, 1, 1, 2, 2).expect("valid crop");
    assert_eq!(
        vec![15u8, 16, 17, 18, 19, 20, 27, 28, 29, 30, 31, 32],
        c.rgb
    );
    assert!(ImagePipeline::crop(&img, 3, 0, 2, 1).is_err());
}

// ---- UnitTests: PatchFormTests ----

#[test]
fn aspect_size_matches_transformers() {
    for (h, w, budget, eh, ew) in [
        (720, 1280, 70, 288, 528),
        (720, 1280, 280, 576, 1056),
        (768, 1024, 140, 480, 624),
        (768, 1024, 280, 672, 912),
        (1080, 2560, 280, 480, 1200),
        (1920, 1080, 140, 720, 384),
        (10, 4000, 70, 48, 3360),
        (4000, 10, 280, 13440, 48),
        (48, 48, 70, 384, 384),
    ] {
        assert_eq!(
            (eh, ew),
            ImagePipeline::aspect_size(h, w, 16, budget * 9, 3).unwrap()
        );
    }
}

#[test]
fn patches_follow_the_gemma4_processor_layout() {
    let v = VisionGraph {
        tokens: 70,
        positions_input: Some("position_ids".to_string()),
        pool_input: Some("pool_weights".to_string()),
        preprocess: ImagePreprocessSpec {
            mean: vec![0.0, 0.0, 0.0],
            std: vec![1.0, 1.0, 1.0],
            ..ImagePreprocessSpec::default()
        },
        ..VisionGraph::default()
    };
    const H: i32 = 288;
    const W: i32 = 528;
    assert_eq!(
        (H, W),
        ImagePipeline::aspect_size(H, W, 16, v.patch_budget(), 3).unwrap()
    );
    let mut rgb = vec![0u8; (W * H * 3) as usize];
    for y in 0..H {
        for x in 0..W {
            for c in 0..3 {
                rgb[((y * W + x) * 3 + c) as usize] = ((x + 2 * y + 50 * c) % 256) as u8;
            }
        }
    }
    let out = ImagePipeline::to_patches(
        &RgbImage {
            width: W,
            height: H,
            rgb,
        },
        &v,
    )
    .unwrap();
    assert_eq!((H, W), (out.height, out.width));
    let nh = (H / 16) as usize;
    let nw = (W / 16) as usize;
    assert_eq!((nh * nw / 9) as i32, out.soft_tokens);
    assert_eq!(630 * 768, out.patches.len());

    let idx = nw + 2;
    assert_eq!(2, out.positions[idx * 2]);
    assert_eq!(1, out.positions[idx * 2 + 1]);
    let want_elem = ((37 + 2 * 19 + 50) % 256) as f32 / 255.0;
    assert!((out.patches[idx * 768 + (3 * 16 + 5) * 3 + 1] - want_elem).abs() < 1e-6);
    assert_eq!(-1, out.positions[(nh * nw) * 2]);
    assert_eq!(-1, *out.positions.last().unwrap());
    assert_eq!(0.0f32, *out.patches.last().unwrap());

    let pool = out.pool_weights.expect("pool weights");
    assert_eq!(70 * 630, pool.len());
    assert!((pool[idx] - 1.0 / 9.0).abs() < 1e-6);
    let idx34 = 3 * nw + 4;
    let slot34 = 4 / 3 + (nw / 3) * (3 / 3);
    assert!((pool[slot34 * 630 + idx34] - 1.0 / 9.0).abs() < 1e-6);
    assert_eq!(0.0f32, pool[idx34]);

    for p_idx in 0..(nh * nw) {
        let col_sum: f32 = (0..70).map(|s| pool[s * 630 + p_idx]).sum();
        assert!((col_sum - 1.0 / 9.0).abs() < 1e-6);
    }
    let pad_idx = nh * nw;
    let pad_col_sum: f32 = (0..70).map(|s| pool[s * 630 + pad_idx]).sum();
    assert_eq!(0.0f32, pad_col_sum);

    let nonzero_slots: usize = (0..70)
        .filter(|&s| (0..630).any(|p_idx| pool[s * 630 + p_idx] != 0.0))
        .count();
    assert_eq!(out.soft_tokens as usize, nonzero_slots);
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

#[test]
fn resolve_matches_openvino_then_dxgi() {
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
    let ov_main = [npu, igpu, ov_cpu];
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
    let ov_main = [npu, igpu, ov_cpu];
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

    for setting in ["Quadro", "Basic Render"] {
        let (picked, warning) = GpuSelection::resolve(Some(setting), &ov_main, &dxgi, true);
        assert_eq!("OpenVINO:GPU.0", dev_id(picked.as_ref()));
        assert!(warning.unwrap_or_default().contains("matches no GPU"));
    }
}

#[test]
fn resolve_edge_cases() {
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
    let ov_main = [npu, igpu, ov_cpu.clone()];
    let rx6600 = adapter(0, "AMD Radeon RX 6600", 8u64 << 30, 0, 0x1002);
    let rtx3050 = adapter(1, "NVIDIA GeForce RTX 3050", 6u64 << 30, 0, 0x10DE);
    let intel_igpu = adapter(2, "Intel(R) Arc(TM) 140T GPU (16GB)", 128u64 << 20, 0, 0x8086);
    let basic = adapter(
        3,
        "Microsoft Basic Render Driver",
        0,
        DxgiAdapter::FLAG_SOFTWARE,
        0x1414,
    );
    let dxgi = [rx6600.clone(), rtx3050, intel_igpu, basic];

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
    let ov_main = [npu, igpu, ov_cpu];
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
fn allowed_uses_the_resolved_gpu_only_for_gpu() {
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
    let ov_main = [npu, igpu, ov_cpu];
    let rtx = GpuSelection::dml(&adapter(
        1,
        "NVIDIA GeForce RTX 3050",
        6u64 << 30,
        0,
        0x10DE,
    ));
    let ids = |v: Vec<DeviceInfo>| v.into_iter().map(|d| d.id).collect::<Vec<_>>().join(",");
    assert_eq!(
        "DML:1,CPU",
        ids(DevicePolicy::allowed("gpu", &ov_main, Some(&rtx)))
    );
    assert_eq!(
        "GPU.0,CPU",
        ids(DevicePolicy::allowed("gpu", &ov_main, None))
    );
    assert_eq!(
        "NPU,CPU",
        ids(DevicePolicy::allowed("auto", &ov_main, Some(&rtx)))
    );
    assert_eq!(
        "CPU",
        ids(DevicePolicy::allowed("cpu", &ov_main, Some(&rtx)))
    );
}

#[test]
fn bench_devices_add_every_other_gpu_only_with_several_gpus() {
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
    let ov_main = [npu, igpu.clone(), ov_cpu];
    let dxgi = [
        adapter(0, "AMD Radeon RX 6600", 8u64 << 30, 0, 0x1002),
        adapter(1, "NVIDIA GeForce RTX 3050", 6u64 << 30, 0, 0x10DE),
        adapter(2, "Intel(R) Arc(TM) 140T GPU (16GB)", 128u64 << 20, 0, 0x8086),
    ];
    let gpus = GpuSelection::all(&ov_main, &dxgi, true);
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
    let ov_main = [npu.clone(), igpu.clone(), ov_cpu];
    let dxgi = [
        adapter(0, "AMD Radeon RX 6600", 8u64 << 30, 0, 0x1002),
        adapter(1, "NVIDIA GeForce RTX 3050", 6u64 << 30, 0, 0x10DE),
        adapter(2, "Intel(R) Arc(TM) 140T GPU (16GB)", 128u64 << 20, 0, 0x8086),
    ];
    let gpus = GpuSelection::all(&ov_main, &dxgi, true);
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
fn cache_key_setting_includes_a_non_auto_adapter() {
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
fn onnx_runtime_refuses_openvino_ir_with_a_clear_message() {
    let ort = OnnxBackend::new(true).expect("OnnxBackend");
    let cpu = ort.devices().last().unwrap();
    let err = ort
        .load(Path::new(r"Z:\no-such-dir\text.xml"), cpu, None, None)
        .err()
        .expect("expected DeviceFailed");
    assert!(err.wire_message().contains("OpenVINO IR"));
}

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
    let mut h = PipeHarness::start(engine, s);

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

    // Note: W3 implements SileroVad; W2 verifies transcribe reports missing whisper once VAD is on disk.
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
    let mut h = PipeHarness::start(engine, s);

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
    let mut h = PipeHarness::start(engine, s);

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
        let mut h = PipeHarness::start(engine, s.clone());
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
    let mut h = PipeHarness::start(engine, s);
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

    let mut results = Vec::new();
    let long_doc = vec!["budget meeting"; 20].join(" ");
    for (backend, dev) in targets {
        let mut e = Embedder::new(
            Arc::clone(&backend),
            dev.clone(),
            m.clone(),
            140,
            Some(dir.path().join("cache").join(&dev.id)),
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
    let dir = ScopedDir::new("ovnpu");
    let models = dir.path().join("models");
    install_embedding_gemma(&models, None);
    let s = test_settings(dir.path(), |_| {});
    let has_npu = ov.devices().iter().any(|d| d.kind == DeviceKind::Npu);
    let engine = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
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

#[test]
fn replan_that_switches_model_is_persisted() {
    let dir = ScopedDir::new("s4w-switch");
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
        b.set_eg_broken(true);
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
    }

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
}

// ---- Sweep5WWorkerTests (A5_Bench_*) ----

#[test]
fn a5_bench_updates_device_cache_when_failed_devices_is_non_empty() {
    let dir = ScopedDir::new("s5w-a5");
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
        assert!(engine.failed_devices().contains(&"GPU.0".to_string()));

        b.set_eg_broken(true);
        let bench = send_req(
            &h,
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
    }

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
