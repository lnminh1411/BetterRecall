#![allow(
    dead_code,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::identity_op,
    clippy::cloned_ref_to_slice_refs
)]
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use br_contracts::{BinaryFrame, DataPaths, PipeNames, Settings, WorkerRequest, WorkerResponse};
use br_worker::engine::{FakeEngine, IWorkerEngine};
use br_worker::hosting::WorkerSettings;
use br_worker::inference::dxgi::DxgiAdapter;
use br_worker::inference::{
    DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession, TensorSpec,
    TensorType,
};
use br_worker::models::catalog::ModelCatalog;
use br_worker::models::manifest::{
    ClassifierSpec, DetectorSpec, EmbeddingTableSpec, FusionGraph, ImagePreprocessSpec,
    ManifestFile, ModelManifest, PromptSpec, TextGraph, TokenizerSpec, VisionGraph,
};
use br_worker::protocol::agent_pipe::win32_to_io;
use br_worker::protocol::{AgentPipe, LoopExit, RequestHandler, WorkerError, WorkerLoop};
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, WriteFile,
};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::PCWSTR;

pub struct ScopedDir {
    pub path: PathBuf,
}

impl ScopedDir {
    pub fn new(tag: &str) -> Self {
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

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn data_path(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/BetterRecall.Worker.Tests/TestData")
        .join(file)
}

pub fn test_settings(home: &Path, edit: impl FnOnce(&mut Settings)) -> WorkerSettings {
    let mut s = Settings::default();
    s.inference.model_dir = home.join("models").to_string_lossy().into_owned();
    edit(&mut s);
    fs::create_dir_all(home).expect("create home dir");
    let json = serde_json::to_vec(&s).expect("serialize settings");
    fs::write(DataPaths::settings_file(home), json).expect("write settings.json");
    WorkerSettings::from(&s, home)
}

pub fn bgra(w: usize, h: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut p = vec![0u8; w * h * 4];
    for i in 0..(w * h) {
        p[i * 4] = b;
        p[i * 4 + 1] = g;
        p[i * 4 + 2] = r;
        p[i * 4 + 3] = 255;
    }
    p
}

pub fn pcm_sine(amplitude: f32, samples: usize) -> Vec<u8> {
    let pcm: Vec<f32> = (0..samples)
        .map(|i| amplitude * ((i as f32) * 0.05).sin())
        .collect();
    BinaryFrame::floats_to_bytes(&pcm)
}

pub fn copy_model_file(dir: &Path, src: &str, dst: &str) -> ManifestFile {
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

pub fn write_manifest(dir: &Path, m: &ModelManifest) {
    let json = serde_json::to_vec_pretty(m).expect("serialize manifest");
    fs::write(dir.join("manifest.json"), json).expect("write manifest.json");
}

pub fn install_embedding_gemma(model_dir: &Path, corrupt_vision: Option<&str>) {
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

pub fn install_embedding_gemma_table_layout(model_dir: &Path) {
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

pub fn install_siglip(model_dir: &Path) {
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

pub fn install_nsfw(model_dir: &Path) {
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

pub const NUDENET_LABELS: [&str; 18] = [
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

pub fn install_nsfw_detector(model_dir: &Path) {
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

pub fn install_vad(model_dir: &Path) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::VAD);
    copy_model_file(&dir, "vad.onnxt", ModelCatalog::VAD_FILE);
}

pub fn send_req(h: &RequestHandler<'_>, r: &WorkerRequest, payload: &[u8]) -> WorkerResponse {
    let json = serde_json::to_vec(r).expect("serialize request");
    h.handle(&json, payload).header
}


pub fn adapter(
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

pub fn dev(kind: DeviceKind, id: &str, name: &str, ep: &str, integrated: Option<bool>) -> DeviceInfo {
    DeviceInfo::new(kind, id, name, ep, integrated)
}


pub fn write_stub_file(dir: &Path, name: &str, bytes: &[u8]) -> ManifestFile {
    fs::create_dir_all(dir).expect("create model dir");
    let p = dir.join(name);
    fs::write(&p, bytes).expect("write stub file");
    ManifestFile {
        file: name.to_string(),
        size: bytes.len() as i64,
        sha256: String::new(),
        source: None,
    }
}


pub fn dev_cpu() -> DeviceInfo {
    DeviceInfo::new(DeviceKind::Cpu, "CPU", "CPU", "CPU", None)
}

pub fn dev_igpu() -> DeviceInfo {
    DeviceInfo::new(DeviceKind::Gpu, "GPU.0", "Intel Arc", "OpenVINO", Some(true))
}

pub fn dev_npu() -> DeviceInfo {
    DeviceInfo::new(DeviceKind::Npu, "NPU", "Intel AI Boost", "OpenVINO", None)
}

pub fn spec(name: &str, ty: TensorType, shape: &[i64]) -> TensorSpec {
    TensorSpec {
        name: name.to_string(),
        ty,
        shape: shape.to_vec(),
    }
}

pub struct FnSession {
    pub device: DeviceInfo,
    pub inputs: Vec<TensorSpec>,
    pub outputs: Vec<TensorSpec>,
    pub handler: Box<dyn Fn(&[HostTensor]) -> Result<HashMap<String, HostTensor>, WorkerError> + Send + Sync>,
}

impl IInferenceSession for FnSession {
    fn device(&self) -> &DeviceInfo {
        &self.device
    }
    fn inputs(&self) -> &[TensorSpec] {
        &self.inputs
    }
    fn outputs(&self) -> &[TensorSpec] {
        &self.outputs
    }
    fn run(&self, inputs: &[HostTensor]) -> Result<HashMap<String, HostTensor>, WorkerError> {
        (self.handler)(inputs)
    }
}

pub struct MockBackend {
    pub name: &'static str,
    pub is_ov: bool,
    pub devices: Vec<DeviceInfo>,
    pub loader: Box<
        dyn Fn(&Path, &DeviceInfo, Option<&HashMap<String, Vec<i64>>>) -> Result<Box<dyn IInferenceSession>, WorkerError>
            + Send
            + Sync,
    >,
}

impl IInferenceBackend for MockBackend {
    fn name(&self) -> &str {
        self.name
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn npu_driver_version(&self) -> &str {
        ""
    }
    fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }
    fn is_openvino_backend(&self) -> bool {
        self.is_ov
    }
    fn is_onnx_backend(&self) -> bool {
        !self.is_ov
    }
    fn load(
        &self,
        model_path: &Path,
        device: &DeviceInfo,
        _cache_dir: Option<&Path>,
        static_shapes: Option<&HashMap<String, Vec<i64>>>,
    ) -> Result<Box<dyn IInferenceSession>, WorkerError> {
        (self.loader)(model_path, device, static_shapes)
    }
}


pub fn create_overlapped_pipe_server(name: &str, out_buf: u32, in_buf: u32) -> (HANDLE, HANDLE) {
    let full_name = format!(r"\\.\pipe\{name}");
    let wide: Vec<u16> = full_name.encode_utf16().chain(Some(0)).collect();
    let server = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            out_buf,
            in_buf,
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
    let res = unsafe { ConnectNamedPipe(server, Some(&mut ov)) };
    if let Err(e) = res {
        assert_eq!(
            e.code(),
            ERROR_IO_PENDING.to_hresult(),
            "ConnectNamedPipe failed: {e}"
        );
    }
    (server, ev)
}

pub fn finish_connect(server: HANDLE, ev: HANDLE) {
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

pub fn write_raw_overlapped(handle: HANDLE, mut data: &[u8]) {
    while !data.is_empty() {
        let ev = unsafe { CreateEventW(None, true, false, None) }.expect("CreateEventW");
        let mut ov = OVERLAPPED {
            hEvent: ev,
            ..Default::default()
        };
        let res = unsafe { WriteFile(handle, Some(data), None, Some(&mut ov)) };
        if let Err(e) = res {
            assert_eq!(e.code(), ERROR_IO_PENDING.to_hresult(), "WriteFile: {e}");
        }
        let mut n = 0u32;
        unsafe { GetOverlappedResult(handle, &ov, &mut n, true) }
            .map_err(win32_to_io)
            .expect("GetOverlappedResult write");
        unsafe {
            let _ = CloseHandle(ev);
        }
        data = &data[n as usize..];
    }
}

pub struct PipeHarness {
    pub server: Option<AgentPipe>,
    pub server_raw: HANDLE,
    pub loop_thread: Option<JoinHandle<LoopExit>>,
    pub next_id: i64,
    pub _dir: ScopedDir,
}

impl PipeHarness {
    pub fn start(
        engine: Arc<dyn IWorkerEngine + Send + Sync>,
        settings: WorkerSettings,
        idle: Duration,
        keepalive_every: Duration,
        keepalive_max: Duration,
        zero_buf: bool,
    ) -> Self {
        let dir = ScopedDir::new("pipe");
        let name = PipeNames::new_worker();
        let buf = if zero_buf { 0 } else { 65_536 };
        let (server_raw, ev) = create_overlapped_pipe_server(&name, buf, buf);
        let client = AgentPipe::connect(&name, Duration::from_secs(5)).expect("connect pipe");
        finish_connect(server_raw, ev);
        let server = AgentPipe::from_handle(server_raw);
        let loop_thread = thread::spawn(move || {
            let s_clone = settings.clone();
            let handler = RequestHandler::new(engine.as_ref(), move || s_clone.clone());
            WorkerLoop::run(&client, &handler, idle, keepalive_every, keepalive_max)
        });
        Self {
            server: Some(server),
            server_raw,
            loop_thread: Some(loop_thread),
            next_id: 0,
            _dir: dir,
        }
    }

    pub fn start_default(
        engine: Arc<dyn IWorkerEngine + Send + Sync>,
        settings: WorkerSettings,
    ) -> Self {
        Self::start(
            engine,
            settings,
            Duration::from_secs(60),
            WorkerLoop::KEEPALIVE_EVERY,
            WorkerLoop::KEEPALIVE_MAX,
            false,
        )
    }

    pub fn fake(dim: i32, idle: Duration) -> Self {
        let dir = ScopedDir::new("proto");
        let s = test_settings(dir.path(), |x| {
            x.inference.fake = true;
            x.index.dim = dim;
        });
        Self::start(
            Arc::new(FakeEngine),
            s,
            idle,
            WorkerLoop::KEEPALIVE_EVERY,
            WorkerLoop::KEEPALIVE_MAX,
            false,
        )
    }

    pub fn server(&self) -> &AgentPipe {
        self.server.as_ref().expect("server already closed")
    }

    pub fn call(&mut self, mut r: WorkerRequest, payload: &[u8]) -> (WorkerResponse, Vec<u8>) {
        if r.id == 0 {
            self.next_id += 1;
            r.id = self.next_id;
        }
        let header = serde_json::to_vec(&r).expect("serialize WorkerRequest");
        self.call_raw(&header, payload)
    }

    pub fn call_raw(&self, header: &[u8], payload: &[u8]) -> (WorkerResponse, Vec<u8>) {
        use br_worker::protocol::worker_loop::{FrameRead, PipeIo};
        self.server().write_frame(header, payload).expect("write");
        let FrameRead::Frame(h, p) = self
            .server()
            .read_frame_idle(Duration::from_secs(120))
            .expect("read frame")
        else {
            panic!("unexpected eof or timeout waiting for reply");
        };
        let resp: WorkerResponse = serde_json::from_slice(&h).expect("deserialize WorkerResponse");
        (resp, p)
    }

    pub fn close_server(&mut self) {
        self.server.take();
    }

    pub fn join_loop(&mut self) -> LoopExit {
        self.loop_thread
            .take()
            .expect("loop already joined")
            .join()
            .expect("loop thread panicked")
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


pub fn persistent_npu_cache_dir(label: &str) -> PathBuf {
    let base = std::env::var_os("BR_TEST_TMP")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    // One cache per checkout: runs from different worktrees must not share (and race on) it.
    let checkout = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        env!("CARGO_MANIFEST_DIR").to_ascii_lowercase().hash(&mut h);
        format!("{:016x}", h.finish())
    };
    let path = base.join("br-worker-npu-cache").join(checkout).join(label);
    fs::create_dir_all(&path).expect("create NPU cache dir");
    path
}
