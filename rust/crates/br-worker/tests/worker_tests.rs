// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use br_contracts::{
    BenchResult, BinaryFrame, DataPaths, OcrLine, OcrResult, PipeNames, Settings, TranscribeResult,
    WorkerHello, WorkerRequest, WorkerResponse, full_path,
};
use br_worker::args::WorkerArgs;
use br_worker::engine::{
    EngineFactory, FakeEngine, IWorkerEngine, ImageEmbedding, ImageInput, TextKind,
};
use br_worker::hosting::{Log, LogSink, WorkerSettings};
use br_worker::protocol::agent_pipe::win32_to_io;
use br_worker::protocol::{
    AgentPipe, LoopExit, RequestHandler, WorkerError, WorkerErrors, WorkerLoop,
};
use br_worker::vectors::VectorMath;
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, Utc};
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

fn create_overlapped_pipe_server(name: &str, out_buf: u32, in_buf: u32) -> (HANDLE, HANDLE) {
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

fn write_raw_overlapped(handle: HANDLE, mut data: &[u8]) {
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

struct PipeHarness {
    server: Option<AgentPipe>,
    server_raw: HANDLE,
    loop_thread: Option<JoinHandle<LoopExit>>,
    next_id: i64,
    _dir: ScopedDir,
}

impl PipeHarness {
    fn start(
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

    fn fake(dim: i32, idle: Duration) -> Self {
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

    fn server(&self) -> &AgentPipe {
        self.server.as_ref().expect("server already closed")
    }

    fn call(&mut self, mut r: WorkerRequest, payload: &[u8]) -> (WorkerResponse, Vec<u8>) {
        if r.id == 0 {
            self.next_id += 1;
            r.id = self.next_id;
        }
        let header = serde_json::to_vec(&r).expect("serialize WorkerRequest");
        self.call_raw(&header, payload)
    }

    fn call_raw(&self, header: &[u8], payload: &[u8]) -> (WorkerResponse, Vec<u8>) {
        use br_worker::protocol::worker_loop::{FrameRead, PipeIo};
        self.server().write_frame(header, payload).expect("write");
        let FrameRead::Frame(h, p) = self
            .server()
            .read_frame_idle(Duration::from_secs(30))
            .expect("read frame")
        else {
            panic!("unexpected eof or timeout waiting for reply");
        };
        let resp: WorkerResponse = serde_json::from_slice(&h).expect("deserialize WorkerResponse");
        (resp, p)
    }

    fn close_server(&mut self) {
        self.server.take();
    }

    fn join_loop(&mut self) -> LoopExit {
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

// ---- ProtocolTests ----

#[test]
fn server_pid_is_this_process() {
    let name = PipeNames::new_worker();
    let (server_raw, ev) = create_overlapped_pipe_server(&name, 4096, 4096);
    let client = AgentPipe::connect(&name, Duration::from_secs(5)).expect("connect");
    finish_connect(server_raw, ev);
    let _server = AgentPipe::from_handle(server_raw);

    let pid = client.server_process_id().expect("server_process_id");
    assert_eq!(std::process::id(), pid);
    let self_path = AgentPipe::image_path(pid).expect("image_path");
    assert!(
        client
            .verify_server(Path::new(&self_path))
            .expect("verify")
            .trusted
    );
    let bad = client
        .verify_server(Path::new(r"C:\nowhere\BetterRecall.Agent.exe"))
        .expect("verify bad");
    assert!(!bad.trusted);
    assert_eq!(Some(self_path), bad.actual);
}

#[test]
fn hello_reports_fake_and_dim() {
    let mut h = PipeHarness::fake(512, Duration::from_secs(60));
    let (r, p) = h.call(
        WorkerRequest {
            id: 7,
            op: "hello".into(),
            ..Default::default()
        },
        &[],
    );
    assert!(r.ok);
    assert_eq!(7, r.id);
    assert!(p.is_empty());
    let hello = r.hello.expect("hello");
    assert_eq!("fake", hello.ep);
    assert_eq!(512, hello.dim);
    assert_eq!(1, hello.protocol);
    assert!(hello.missing.is_empty());
}

#[test]
fn embed_text_returns_normalised_vector_at_index_dim() {
    for dim in [768, 512, 256] {
        let mut h = PipeHarness::fake(dim, Duration::from_secs(60));
        let (r, p) = h.call(
            WorkerRequest {
                op: "embed_text".into(),
                text: Some("quarterly sales".into()),
                kind: Some("query".into()),
                ..Default::default()
            },
            &[],
        );
        assert!(r.ok);
        assert_eq!(Some(dim), r.dim);
        assert!(r.ms.is_some());
        let v = BinaryFrame::bytes_to_floats(&p);
        assert_eq!(dim as usize, v.len());
        let norm = v.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norm={norm}");
    }
}

#[test]
fn fake_text_is_deterministic_and_query_document_close() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    let mut embed = |t: &str, k: &str| -> Vec<f32> {
        let (_, p) = h.call(
            WorkerRequest {
                op: "embed_text".into(),
                text: Some(t.into()),
                kind: Some(k.into()),
                ..Default::default()
            },
            &[],
        );
        BinaryFrame::bytes_to_floats(&p)
    };
    let q1 = embed("Budget meeting notes", "query");
    let q2 = embed("Budget meeting notes", "query");
    let d = embed("budget meeting notes", "document");
    let other = embed("holiday photos at the beach", "document");
    let partial = embed("notes from the budget review", "document");
    assert_eq!(q1, q2);
    let qd = VectorMath::cosine(&q1, &d).unwrap();
    let qo = VectorMath::cosine(&q1, &other).unwrap();
    let qp = VectorMath::cosine(&q1, &partial).unwrap();
    assert!(qd > 0.97, "query/doc {qd}");
    assert!(qo < 0.3, "query/other {qo}");
    assert!(qp > qo, "partial {qp} > other {qo}");
}

#[test]
fn embed_image_encoded_formats() {
    for format in ["webp", "jpeg", "png"] {
        let mut h = PipeHarness::fake(256, Duration::from_secs(60));
        let bytes = b"not really an image, the fake engine only hashes it";
        let (r, p) = h.call(
            WorkerRequest {
                op: "embed_image".into(),
                format: Some(format.into()),
                nsfw: Some(0.8),
                ..Default::default()
            },
            bytes,
        );
        assert!(r.ok);
        assert!(r.dropped.is_none());
        assert_eq!(Some(256), r.dim);
        assert_eq!(256 * 4, p.len());
        let (_, p2) = h.call(
            WorkerRequest {
                op: "embed_image".into(),
                format: Some(format.into()),
                ..Default::default()
            },
            bytes,
        );
        assert_eq!(p, p2);
    }
}

fn assert_bad(h: &mut PipeHarness, r: WorkerRequest, payload: &[u8]) {
    let (resp, p) = h.call(r, payload);
    assert!(!resp.ok);
    assert_eq!(Some(WorkerErrors::BAD_REQUEST.to_string()), resp.error);
    assert!(p.is_empty());
}

#[test]
fn embed_image_bgra8_and_errors() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    let px = bgra(8, 4, 10, 20, 30);
    let (ok, p) = h.call(
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("bgra8".into()),
            width: Some(8),
            height: Some(4),
            stride: Some(32),
            ..Default::default()
        },
        &px,
    );
    assert!(ok.ok);
    assert_eq!(768 * 4, p.len());

    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("bgra8".into()),
            width: Some(8),
            height: Some(4),
            stride: Some(31),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("bgra8".into()),
            width: Some(8),
            height: Some(5),
            stride: Some(32),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("bgra8".into()),
            width: Some(8),
            stride: Some(32),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("gif".into()),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("png".into()),
            ..Default::default()
        },
        &[],
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("png".into()),
            nsfw: Some(1.5),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("png".into()),
            nsfw: Some(0.0),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("png".into()),
            nsfw: Some(0.8),
            nsfw_detector: Some(0.0),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("png".into()),
            nsfw: Some(0.8),
            nsfw_detector: Some(-0.1),
            ..Default::default()
        },
        &px,
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("png".into()),
            nsfw: Some(0.8),
            nsfw_detector: Some(1.01),
            ..Default::default()
        },
        &px,
    );
    let (ok_det, p_det) = h.call(
        WorkerRequest {
            op: "embed_image".into(),
            format: Some("bgra8".into()),
            width: Some(8),
            height: Some(4),
            stride: Some(32),
            nsfw: Some(0.8),
            nsfw_detector: Some(1.0),
            ..Default::default()
        },
        &px,
    );
    assert!(ok_det.ok, "{:?}", ok_det.error);
    assert_eq!(768 * 4, p_det.len());
}

#[test]
fn embed_text_errors() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_text".into(),
            kind: Some("query".into()),
            ..Default::default()
        },
        &[],
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_text".into(),
            text: Some("x".into()),
            kind: Some("passage".into()),
            ..Default::default()
        },
        &[],
    );
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "embed_text".into(),
            text: Some("x".into()),
            ..Default::default()
        },
        &[],
    );
}

#[test]
fn transcribe_speech_silence_and_errors() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    let speech = WorkerRequest {
        op: "transcribe".into(),
        format: Some("f32".into()),
        sample_rate: Some(16_000),
        channels: Some(1),
        source: Some("system".into()),
        language: Some("auto".into()),
        ..Default::default()
    };
    let (r, p) = h.call(speech.clone(), &pcm_sine(0.2, 16_000));
    assert!(r.ok);
    assert!(p.is_empty());
    let t = r.transcript.expect("transcript");
    assert!(t.speech);
    assert!(t.text.contains("system"));
    assert_eq!(1, t.segments.len());
    assert_eq!(1000, t.segments[0].end_ms);

    let mut vi = speech.clone();
    vi.language = Some("vi".into());
    let (s, _) = h.call(vi, &pcm_sine(0.0, 16_000));
    assert!(s.ok);
    let ts = s.transcript.expect("silence transcript");
    assert!(!ts.speech);
    assert_eq!("vi", ts.language);

    let pcm = pcm_sine(0.2, 16_000);
    let mut bad = speech.clone();
    bad.sample_rate = Some(44_100);
    assert_bad(&mut h, bad, &pcm);

    let mut bad = speech.clone();
    bad.channels = Some(2);
    assert_bad(&mut h, bad, &pcm);

    let mut bad = speech.clone();
    bad.format = Some("s16".into());
    assert_bad(&mut h, bad, &pcm);

    let mut bad = speech.clone();
    bad.source = Some("camera".into());
    assert_bad(&mut h, bad, &pcm);

    let mut bad = speech.clone();
    bad.language = Some("english".into());
    assert_bad(&mut h, bad, &pcm);

    assert_bad(&mut h, speech.clone(), &pcm[..pcm.len() - 1]);
    assert_bad(&mut h, speech.clone(), &[]);
    let nan = BinaryFrame::floats_to_bytes(&[0.1f32, f32::NAN]);
    assert_bad(&mut h, speech, &nan);
}

#[test]
fn bench_and_shutdown() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    let (b, _) = h.call(
        WorkerRequest {
            op: "bench".into(),
            ..Default::default()
        },
        &[],
    );
    assert!(b.ok);
    assert_eq!("CPU", b.bench.expect("bench").chosen);
    let (s, _) = h.call(
        WorkerRequest {
            id: 99,
            op: "shutdown".into(),
            ..Default::default()
        },
        &[],
    );
    assert!(s.ok);
    assert_eq!(99, s.id);
    assert_eq!(LoopExit::Shutdown, h.join_loop());
}

#[test]
fn unknown_op_and_bad_json() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    assert_bad(
        &mut h,
        WorkerRequest {
            op: "explode".into(),
            ..Default::default()
        },
        &[],
    );
    let (r, _) = h.call_raw(br#"{"id":41,"op":"#, &[]);
    assert!(!r.ok);
    assert_eq!(Some("Bad request.".to_string()), r.error);
    let (r2, _) = h.call_raw(br#"{"id":42,"op":"hello","width":"wide"}"#, &[]);
    assert_eq!(42, r2.id);
    assert_eq!(Some("Bad request.".to_string()), r2.error);
    let (r3, _) = h.call_raw(b"[]", &[]);
    assert_eq!(0, r3.id);
    assert_eq!(Some("Bad request.".to_string()), r3.error);
    assert!(
        h.call(
            WorkerRequest {
                op: "hello".into(),
                ..Default::default()
            },
            &[]
        )
        .0
        .ok
    );
}

#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

#[test]
fn requests_answered_in_arrival_order() {
    use br_worker::protocol::worker_loop::{FrameRead, PipeIo};
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    let raw_handle = SendHandle(h.server_raw);
    let reqs: Vec<Vec<u8>> = (1..=20)
        .map(|i| {
            let r = WorkerRequest {
                id: i,
                op: "embed_text".into(),
                text: Some(format!("t{i}")),
                kind: Some("document".into()),
                ..Default::default()
            };
            let hdr = serde_json::to_vec(&r).unwrap();
            BinaryFrame::build_head(&hdr, 0).unwrap()
        })
        .collect();
    let writer = thread::spawn(move || {
        let handle = raw_handle;
        for frame in reqs {
            write_raw_overlapped(handle.0, &frame);
        }
    });
    for i in 1..=20 {
        let FrameRead::Frame(hdr, _) = h
            .server()
            .read_frame_idle(Duration::from_secs(30))
            .expect("read reply")
        else {
            panic!("unexpected eof");
        };
        let resp: WorkerResponse = serde_json::from_slice(&hdr).unwrap();
        assert_eq!(i, resp.id);
    }
    writer.join().unwrap();
    h.close_server();
}

#[test]
fn exits_when_pipe_closes() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    h.close_server();
    assert_eq!(LoopExit::PipeClosed, h.join_loop());
}

#[test]
fn exits_when_idle() {
    let mut h = PipeHarness::fake(768, Duration::from_millis(300));
    assert!(
        h.call(
            WorkerRequest {
                op: "hello".into(),
                ..Default::default()
            },
            &[]
        )
        .0
        .ok
    );
    assert_eq!(LoopExit::Idle, h.join_loop());
}

#[test]
fn oversized_header_is_a_protocol_error() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    let len = ((BinaryFrame::MAX_HEADER_BYTES as u32) + 1).to_le_bytes();
    write_raw_overlapped(h.server_raw, &len);
    assert_eq!(LoopExit::ProtocolError, h.join_loop());
}

struct ThrowingEngine;

impl IWorkerEngine for ThrowingEngine {
    fn hello(&self, _dim: i32) -> Result<WorkerHello, WorkerError> {
        Err(WorkerError::Internal("NotImplemented".into()))
    }
    fn embed_image(
        &self,
        _image: &ImageInput<'_>,
        _nsfw: Option<f64>,
        _grid: i32,
        _det: Option<f64>,
    ) -> Result<ImageEmbedding, WorkerError> {
        let mut v = vec![0.0f32; 768];
        v[0] = f32::NAN;
        Ok(ImageEmbedding {
            vector: Some(v),
            dropped: None,
        })
    }
    fn embed_text(&self, _text: &str, _kind: TextKind) -> Result<Vec<f32>, WorkerError> {
        Err(WorkerError::Internal("boom".into()))
    }
    fn transcribe(
        &self,
        _pcm: &[f32],
        _source: &str,
        _language: &str,
    ) -> Result<TranscribeResult, WorkerError> {
        Err(WorkerError::ModelNotInstalled(
            "whisper-large-v3-turbo".into(),
        ))
    }
    fn bench(&self) -> Result<BenchResult, WorkerError> {
        Err(WorkerError::DeviceFailed("NPU compile failed.".into()))
    }
    fn ocr(&self, _image: &ImageInput<'_>, _entities: bool) -> Result<OcrResult, WorkerError> {
        Err(WorkerError::ModelNotInstalled("ocr".into()))
    }
}

#[test]
fn engine_exceptions_map_to_wire_errors() {
    let dir = ScopedDir::new("errs");
    let s = test_settings(dir.path(), |_| {});
    let mut h = PipeHarness::start(
        Arc::new(ThrowingEngine),
        s,
        Duration::from_secs(60),
        WorkerLoop::KEEPALIVE_EVERY,
        WorkerLoop::KEEPALIVE_MAX,
        false,
    );
    assert_eq!(
        Some("Internal error.".to_string()),
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
    );
    assert_eq!(
        Some("Model not installed: whisper-large-v3-turbo.".to_string()),
        h.call(
            WorkerRequest {
                op: "transcribe".into(),
                format: Some("f32".into()),
                sample_rate: Some(16_000),
                channels: Some(1),
                source: Some("mic".into()),
                ..Default::default()
            },
            &pcm_sine(0.1, 16_000)
        )
        .0
        .error
    );
    assert_eq!(
        Some("Device failed: NPU compile failed.".to_string()),
        h.call(
            WorkerRequest {
                op: "bench".into(),
                ..Default::default()
            },
            &[]
        )
        .0
        .error
    );
    assert_eq!(
        Some("Device failed: model output contains NaN.".to_string()),
        h.call(
            WorkerRequest {
                op: "embed_image".into(),
                format: Some("png".into()),
                ..Default::default()
            },
            &[1, 2, 3]
        )
        .0
        .error
    );
}

// ---- UnitTests (W1 subset) ----

#[test]
fn matryoshka_truncates_and_renormalises() {
    let mut full: Vec<f32> = (1..=768).map(|i| (i as f64).sin() as f32).collect();
    VectorMath::l2_normalize_in_place(&mut full);
    for dim in [768usize, 512, 256] {
        let v = VectorMath::matryoshka(&full, dim).expect("matryoshka");
        assert_eq!(dim, v.len());
        let norm = v.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert!(VectorMath::cosine(&v, &full[..dim]).unwrap() > 0.99999);
    }
    assert!(VectorMath::matryoshka(&full, 1024).is_err());
}

#[test]
fn cosine_and_nan() {
    assert!((VectorMath::cosine(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]).unwrap() - 1.0).abs() < 1e-10);
    assert!((VectorMath::cosine(&[1.0, 0.0], &[-1.0, 0.0]).unwrap() + 1.0).abs() < 1e-10);
    assert!(VectorMath::cosine(&[1.0, f32::NAN], &[1.0, 1.0]).unwrap().is_nan());
    assert!(VectorMath::cosine(&[0.0, 0.0], &[1.0, 1.0]).unwrap().is_nan());
    assert!(VectorMath::has_non_finite(&[1.0, f32::INFINITY]));
    assert!(!VectorMath::has_non_finite(&[1.0, 2.0]));
}

#[test]
fn worker_settings_nsfw_keys_fall_back_on_bad_values() {
    let mut s = Settings::default();
    let mut w = WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir"));
    assert_eq!("auto", w.nsfw_device);
    assert_eq!("auto", w.nsfw_gpu_adapter);
    assert!(w.nsfw_filter && w.nsfw_detector);
    s.privacy.nsfw_device = " IGPU ".into();
    s.privacy.nsfw_gpu_adapter = " RTX 3050 ".into();
    s.privacy.nsfw_detector = false;
    w = WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir"));
    assert_eq!("igpu", w.nsfw_device);
    assert_eq!("RTX 3050", w.nsfw_gpu_adapter);
    assert!(!w.nsfw_detector);
    s.privacy.nsfw_device = "tpu".into();
    s.privacy.nsfw_gpu_adapter = "   ".into();
    w = WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir"));
    assert_eq!("auto", w.nsfw_device);
    assert_eq!("auto", w.nsfw_gpu_adapter);
}

#[test]
fn worker_settings_gpu_adapter_falls_back_on_bad_values() {
    let mut s = Settings::default();
    assert_eq!(
        "auto",
        WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir")).gpu_adapter
    );
    s.inference.gpu_adapter = " RTX 3050 ".into();
    assert_eq!(
        "RTX 3050",
        WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir")).gpu_adapter
    );
    s.inference.gpu_adapter = "   ".into();
    assert_eq!(
        "auto",
        WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir")).gpu_adapter
    );
    s.inference.gpu_adapter = "a".repeat(129);
    assert_eq!(
        "auto",
        WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir")).gpu_adapter
    );
}

#[test]
fn args_and_pipe_names() {
    let name = PipeNames::new_worker();
    assert_eq!(
        Some(name.clone()),
        AgentPipe::normalize_name(Some(&format!(r"\\.\pipe\{name}")))
    );
    assert_eq!(None, AgentPipe::normalize_name(Some("BetterRecall.ui.S-1-5")));
    assert_eq!(
        None,
        AgentPipe::normalize_name(Some("BetterRecall.worker.XYZ"))
    );
    assert!(WorkerArgs::parse(&["--pipe", &name, "--home", r"C:\x"]).is_some());
    assert!(WorkerArgs::parse(&["--pipe", &name]).is_none());
    assert!(WorkerArgs::parse(&["--pipe", "evil", "--home", r"C:\x"]).is_none());
    assert!(WorkerArgs::parse(&["--pipe", &name, "--home", r"C:\x", "--extra"]).is_none());
    assert_eq!(
        PathBuf::from(r"C:\Prog\BR\BetterRecall.Agent.exe"),
        AgentPipe::expected_agent_image(Path::new(r"C:\Prog\BR\worker\"))
    );
}

#[test]
fn settings_out_of_range_fall_back() {
    let mut s = Settings::default();
    s.index.dim = 128;
    s.inference.image_tokens = 1120;
    s.inference.device = "TPU".into();
    let w = WorkerSettings::from(&s, Path::new(r"C:\home"));
    assert_eq!(768, w.dim);
    assert_eq!(140, w.image_tokens);
    assert_eq!("auto", w.device);
    assert_eq!(PathBuf::from(r"C:\home\models"), w.model_dir);
}

#[test]
fn final_path_resolves_a_junction() {
    let dir = ScopedDir::new("junction");
    let real = dir.path().join("real");
    let link = dir.path().join("link");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("a.exe"), [1u8, 2, 3]).unwrap();

    let status = Command::new("cmd.exe")
        .args([
            "/c",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &real.to_string_lossy(),
        ])
        .status();
    let Ok(st) = status else { return };
    if !st.success() || !link.exists() {
        return;
    }

    let real_final = AgentPipe::final_path(&real.join("a.exe"));
    let link_final = AgentPipe::final_path(&link.join("a.exe"));
    assert!(
        real_final
            .to_string_lossy()
            .eq_ignore_ascii_case(&link_final.to_string_lossy()),
        "{} != {}",
        real_final.display(),
        link_final.display()
    );
    assert!(!real_final.to_string_lossy().starts_with(r"\\?\"));
    assert_eq!(
        full_path(&dir.path().join("missing.exe")).unwrap(),
        AgentPipe::final_path(&dir.path().join("missing.exe"))
    );
}

// ---- Sweep2GTests (W1 subset) ----

#[test]
fn idle_timer_firing_mid_frame_finishes_the_frame() {
    use br_worker::protocol::worker_loop::{FrameRead, PipeIo};
    let dir = ScopedDir::new("idle-mid");
    let s = test_settings(dir.path(), |x| x.inference.fake = true);
    let mut h = PipeHarness::start(
        Arc::new(FakeEngine),
        s,
        Duration::from_millis(300),
        WorkerLoop::KEEPALIVE_EVERY,
        WorkerLoop::KEEPALIVE_MAX,
        true, // 0-byte buffer like the agent's pipe
    );
    let header = serde_json::to_vec(&WorkerRequest {
        id: 5,
        op: "hello".into(),
        ..Default::default()
    })
    .unwrap();
    write_raw_overlapped(h.server_raw, &(header.len() as u32).to_le_bytes());
    thread::sleep(Duration::from_millis(800));
    write_raw_overlapped(h.server_raw, &header);
    write_raw_overlapped(h.server_raw, &0u32.to_le_bytes());

    let FrameRead::Frame(resp_h, _) = h
        .server()
        .read_frame_idle(Duration::from_secs(10))
        .expect("read reply")
    else {
        panic!("expected reply");
    };
    let r: WorkerResponse = serde_json::from_slice(&resp_h).unwrap();
    assert!(r.ok, "{:?}", r.error);
    assert_eq!(5, r.id);
    assert_eq!(LoopExit::Idle, h.join_loop());
}

#[test]
fn language_must_be_a_whisper_code() {
    let cases = [
        ("auto", true),
        ("en", true),
        ("vi", true),
        ("haw", true),
        ("yue", true),
        ("xx", false),
        ("zz", false),
        ("eng", false),
        ("EN", false),
    ];
    for (lang, ok) in cases {
        assert_eq!(ok, RequestHandler::is_language(lang), "lang={lang}");
    }
}

#[test]
fn unknown_language_in_settings_falls_back_to_auto() {
    let mut s = Settings::default();
    s.audio.language = "xx".into();
    assert_eq!(
        "auto",
        WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir")).audio_language
    );
    s.audio.language = "DE".into();
    assert_eq!(
        "de",
        WorkerSettings::from(&s, Path::new(r"Z:\no-such-dir")).audio_language
    );
}

#[test]
fn unknown_language_is_a_bad_request_not_a_device_failure() {
    let mut h = PipeHarness::fake(768, Duration::from_secs(60));
    let (r, _) = h.call(
        WorkerRequest {
            op: "transcribe".into(),
            format: Some("f32".into()),
            sample_rate: Some(16_000),
            channels: Some(1),
            source: Some("mic".into()),
            language: Some("xx".into()),
            ..Default::default()
        },
        &pcm_sine(0.2, 16_000),
    );
    assert_eq!(Some(WorkerErrors::BAD_REQUEST.to_string()), r.error);
}

#[test]
fn settings_with_bom_comments_and_trailing_commas_are_read() {
    let dir = ScopedDir::new("lenient");
    let json = concat!(
        "\u{FEFF}{\n",
        "  // set by hand\n",
        "  \"inference\": { \"device\": \"cpu\", \"idleExitS\": 77, },\n",
        "  \"log\": { \"level\": \"debug\" },\n",
        "}\n"
    );
    fs::write(DataPaths::settings_file(dir.path()), json.as_bytes()).unwrap();
    let s = WorkerSettings::load(dir.path());
    assert_eq!("cpu", s.device);
    assert_eq!(77, s.idle_exit_s);
    assert_eq!("debug", s.log_level);
}

#[test]
fn a_key_of_the_wrong_type_keeps_only_that_default() {
    let parsed = WorkerSettings::parse(
        r#"{ "inference": { "device": "cpu", "idleExitS": "soon" }, "index": { "dim": 512 } }"#,
    )
    .expect("parse");
    assert_eq!("cpu", parsed.inference.device);
    assert_eq!(
        Settings::default().inference.idle_exit_s,
        parsed.inference.idle_exit_s
    );
    assert_eq!(512, parsed.index.dim);
}

// ---- Sweep3ITests (W1 subset) ----

struct OcrStubEngine {
    lines: Mutex<Box<dyn Fn() -> Vec<OcrLine> + Send + Sync>>,
    throw_err: Option<&'static str>,
}

impl IWorkerEngine for OcrStubEngine {
    fn hello(&self, dim: i32) -> Result<WorkerHello, WorkerError> {
        Ok(WorkerHello {
            protocol: 1,
            device: "CPU".into(),
            ep: "fake".into(),
            model: "fake-model".into(),
            revision: "r1".into(),
            dim,
            asr: true,
            vad: true,
            missing: Vec::new(),
            nsfw_device: None,
            ocr_device: None,
        })
    }
    fn embed_image(
        &self,
        _: &ImageInput<'_>,
        _: Option<f64>,
        _: i32,
        _: Option<f64>,
    ) -> Result<ImageEmbedding, WorkerError> {
        Err(WorkerError::Internal("unused".into()))
    }
    fn embed_text(&self, _: &str, _: TextKind) -> Result<Vec<f32>, WorkerError> {
        Err(WorkerError::Internal("unused".into()))
    }
    fn transcribe(&self, _: &[f32], _: &str, _: &str) -> Result<TranscribeResult, WorkerError> {
        Err(WorkerError::Internal("unused".into()))
    }
    fn bench(&self) -> Result<BenchResult, WorkerError> {
        Err(WorkerError::Internal("unused".into()))
    }
    fn ocr(&self, _: &ImageInput<'_>, _: bool) -> Result<OcrResult, WorkerError> {
        if let Some(msg) = self.throw_err {
            return Err(WorkerError::OcrOverflow(msg.into()));
        }
        let f = self.lines.lock().unwrap();
        Ok(OcrResult {
            lines: f(),
            engine: "onnx".into(),
            ms: 1.0,
            entities_missing: false,
        })
    }
}

fn ocr_screen(lines: usize, chars: usize, c: char) -> Vec<OcrLine> {
    let text: String = std::iter::repeat_n(c, chars).collect();
    let cuts: Vec<i32> = (0..=chars).map(|i| (i * 8) as i32).collect();
    (0..lines)
        .map(|i| OcrLine {
            text: text.clone(),
            x: 0,
            y: (i * 16) as i32,
            w: (chars * 8) as i32,
            h: 16,
            confidence: 0.98,
            cuts: cuts.clone(),
            entities: None,
        })
        .collect()
}

fn ocr_req() -> WorkerRequest {
    WorkerRequest {
        op: "ocr".into(),
        format: Some("bgra8".into()),
        width: Some(2),
        height: Some(2),
        stride: Some(8),
        ..Default::default()
    }
}

#[test]
fn c1_dense_screen_ocr_reply_reaches_the_agent() {
    let dir = ScopedDir::new("s3i-c1");
    let s = test_settings(dir.path(), |x| x.inference.fake = true);
    let eng = Arc::new(OcrStubEngine {
        lines: Mutex::new(Box::new(|| ocr_screen(100, 120, 'ệ'))),
        throw_err: None,
    });
    let mut h = PipeHarness::start(
        eng,
        s,
        Duration::from_secs(60),
        WorkerLoop::KEEPALIVE_EVERY,
        WorkerLoop::KEEPALIVE_MAX,
        false,
    );
    let (r, _) = h.call(ocr_req(), &[0u8; 16]);
    assert!(r.ok, "{:?}", r.error);
    assert_eq!(100, r.ocr.unwrap().lines.len());
    assert!(!h.loop_thread.as_ref().unwrap().is_finished());
}

#[test]
fn c1_reply_over_the_header_limit_is_an_error_and_the_pipe_stays_in_step() {
    let dir = ScopedDir::new("s3i-c1-over");
    let s = test_settings(dir.path(), |x| x.inference.fake = true);
    let eng = Arc::new(OcrStubEngine {
        lines: Mutex::new(Box::new(|| ocr_screen(400, 2000, 'x'))),
        throw_err: None,
    });
    let eng_ref = eng.clone();
    let mut h = PipeHarness::start(
        eng,
        s,
        Duration::from_secs(60),
        WorkerLoop::KEEPALIVE_EVERY,
        WorkerLoop::KEEPALIVE_MAX,
        false,
    );
    let (r, p) = h.call(ocr_req(), &[0u8; 16]);
    assert!(!r.ok);
    assert_eq!(Some(WorkerErrors::INTERNAL.to_string()), r.error);
    assert!(p.is_empty());
    assert!(!h.loop_thread.as_ref().unwrap().is_finished());

    *eng_ref.lines.lock().unwrap() = Box::new(|| ocr_screen(2, 10, 'y'));
    let (ok, _) = h.call(ocr_req(), &[0u8; 16]);
    assert!(ok.ok);
    assert_eq!(2, ok.ocr.unwrap().lines.len());
    let (hello, _) = h.call(
        WorkerRequest {
            op: "hello".into(),
            ..Default::default()
        },
        &[],
    );
    assert!(hello.ok);
}

#[test]
fn c17_overflow_reaches_the_agent_as_device_failed() {
    let dir = ScopedDir::new("s3i-c17");
    let s = test_settings(dir.path(), |x| x.inference.fake = true);
    let eng = Arc::new(OcrStubEngine {
        lines: Mutex::new(Box::new(Vec::new)),
        throw_err: Some("ocr found more than 3000 text boxes"),
    });
    let mut h = PipeHarness::start(
        eng,
        s,
        Duration::from_secs(60),
        WorkerLoop::KEEPALIVE_EVERY,
        WorkerLoop::KEEPALIVE_MAX,
        false,
    );
    let (r, _) = h.call(ocr_req(), &[0u8; 16]);
    assert!(!r.ok);
    assert!(r.error.unwrap().starts_with("Device failed: "));
    assert!(!h.loop_thread.as_ref().unwrap().is_finished());
}

// ---- Sweep3KKeepaliveTests ----

struct SlowEngine {
    delay_ms: u64,
}

impl IWorkerEngine for SlowEngine {
    fn hello(&self, dim: i32) -> Result<WorkerHello, WorkerError> {
        FakeEngine.hello(dim)
    }
    fn embed_image(
        &self,
        image: &ImageInput<'_>,
        t: Option<f64>,
        grid: i32,
        det: Option<f64>,
    ) -> Result<ImageEmbedding, WorkerError> {
        FakeEngine.embed_image(image, t, grid, det)
    }
    fn embed_text(&self, text: &str, kind: TextKind) -> Result<Vec<f32>, WorkerError> {
        if self.delay_ms > 0 {
            thread::sleep(Duration::from_millis(self.delay_ms));
        }
        FakeEngine.embed_text(text, kind)
    }
    fn transcribe(&self, pcm: &[f32], src: &str, lang: &str) -> Result<TranscribeResult, WorkerError> {
        FakeEngine.transcribe(pcm, src, lang)
    }
    fn bench(&self) -> Result<BenchResult, WorkerError> {
        FakeEngine.bench()
    }
    fn ocr(&self, image: &ImageInput<'_>, entities: bool) -> Result<OcrResult, WorkerError> {
        FakeEngine.ocr(image, entities)
    }
}

fn run_keepalive_case(handler_ms: u64, every: Duration, max: Duration) -> Vec<i64> {
    use br_worker::protocol::worker_loop::{FrameRead, PipeIo};
    let dir = ScopedDir::new("keepalive");
    let s = test_settings(dir.path(), |x| x.inference.fake = true);
    let mut h = PipeHarness::start(
        Arc::new(SlowEngine {
            delay_ms: handler_ms,
        }),
        s,
        Duration::from_secs(30),
        every,
        max,
        true,
    );
    let req = serde_json::to_vec(&WorkerRequest {
        id: 7,
        op: "embed_text".into(),
        text: Some("abc".into()),
        kind: Some("query".into()),
        ..Default::default()
    })
    .unwrap();
    h.server().write_frame(&req, &[]).unwrap();

    let mut ids = Vec::new();
    loop {
        let FrameRead::Frame(hdr, payload) = h
            .server()
            .read_frame_idle(Duration::from_secs(20))
            .expect("read frame")
        else {
            panic!("unexpected eof or timeout");
        };
        let resp: WorkerResponse = serde_json::from_slice(&hdr).unwrap();
        ids.push(resp.id);
        if resp.id == 0 {
            assert!(resp.ok);
            assert!(payload.is_empty());
            continue;
        }
        assert!(resp.ok, "{:?}", resp.error);
        assert!(!payload.is_empty());
        assert_eq!(0, payload.len() % 4);
        break;
    }
    h.close_server();
    assert_eq!(LoopExit::PipeClosed, h.join_loop());
    ids
}

#[test]
fn long_request_sends_keepalives_before_its_reply() {
    let ids = run_keepalive_case(
        700,
        Duration::from_millis(100),
        Duration::from_secs(60),
    );
    assert_eq!(Some(&7), ids.last());
    let zero_count = ids.iter().filter(|&&i| i == 0).count();
    assert!(zero_count >= 4, "keepalives: {zero_count}");
    assert!(ids[..ids.len() - 1].iter().all(|&i| i == 0));
}

#[test]
fn fast_request_sends_no_keepalive() {
    let ids = run_keepalive_case(0, Duration::from_millis(300), Duration::from_secs(60));
    assert_eq!(vec![7i64], ids);
}

#[test]
fn keepalives_stop_after_their_cap() {
    let ids = run_keepalive_case(
        900,
        Duration::from_millis(100),
        Duration::from_millis(250),
    );
    assert_eq!(Some(&7), ids.last());
    let zero_count = ids.iter().filter(|&&i| i == 0).count();
    assert!((1..=3).contains(&zero_count), "zero_count={zero_count}");
}

// ---- Sweep3LWorkerTests ----

#[test]
fn missing_model_failure_is_logged_once_per_op_and_model() {
    let dir = ScopedDir::new("s3l");
    let s = test_settings(dir.path(), |_| {});
    let h = RequestHandler::new(&FakeEngine, move || s.clone());
    let missing = WorkerErrors::model_not_installed("ocr-paddle");
    assert!(h.should_log_failure("ocr", &missing));
    assert!(!h.should_log_failure("ocr", &missing));
    assert!(!h.should_log_failure("ocr", &missing));
    assert!(h.should_log_failure("ner", &missing));
    assert!(h.should_log_failure("ocr", &WorkerErrors::model_not_installed("ocr-windows")));
    assert!(h.should_log_failure("ocr", "Bad request."));
    assert!(h.should_log_failure("ocr", "Bad request."));
    let s2 = WorkerSettings::from(&Settings::default(), dir.path());
    assert!(
        RequestHandler::new(&FakeEngine, move || s2.clone()).should_log_failure("ocr", &missing)
    );
}

// ---- Sweep4WWorkerTests (W1 subset) ----

fn send_req(h: &RequestHandler<'_>, r: &WorkerRequest, payload: &[u8]) -> WorkerResponse {
    let bytes = serde_json::to_vec(r).unwrap();
    h.handle(&bytes, payload).header
}

#[test]
fn embed_and_bench_replies_name_the_embedder() {
    let dir = ScopedDir::new("s4w-id");
    let s = test_settings(dir.path(), |x| x.index.dim = 256);
    let h = RequestHandler::new(&FakeEngine, move || s.clone());
    let text = send_req(
        &h,
        &WorkerRequest {
            id: 1,
            op: "embed_text".into(),
            text: Some("hello".into()),
            kind: Some("query".into()),
            ..Default::default()
        },
        &[],
    );
    let image = send_req(
        &h,
        &WorkerRequest {
            id: 2,
            op: "embed_image".into(),
            format: Some("bgra8".into()),
            width: Some(2),
            height: Some(2),
            stride: Some(8),
            ..Default::default()
        },
        &[0u8; 16],
    );
    let bench = send_req(
        &h,
        &WorkerRequest {
            id: 3,
            op: "bench".into(),
            ..Default::default()
        },
        &[],
    );
    for r in [text, image, bench] {
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(Some(FakeEngine::MODEL_ID.to_string()), r.model);
        assert_eq!(Some(FakeEngine::REVISION.to_string()), r.revision);
        assert_eq!(Some("CPU".to_string()), r.device);
    }
    let shut = send_req(
        &h,
        &WorkerRequest {
            id: 4,
            op: "shutdown".into(),
            ..Default::default()
        },
        &[],
    );
    assert!(shut.model.is_none());
}

#[test]
fn hello_without_an_embedder_has_an_empty_revision_and_names_nothing() {
    let dir = ScopedDir::new("s4w-noemb");
    let s = test_settings(dir.path(), |x| {
        x.inference.fake = false;
        x.privacy.nsfw_filter = false;
        x.ocr.enabled = false;
    });
    let engine = EngineFactory::create(&s, dir.path());
    let s_clone = s.clone();
    let h = RequestHandler::new(engine.as_ref(), move || s_clone.clone());
    let hello = send_req(
        &h,
        &WorkerRequest {
            id: 1,
            op: "hello".into(),
            ..Default::default()
        },
        &[],
    )
    .hello
    .expect("hello");
    assert_eq!("", hello.revision);
    assert!(hello.missing.contains(&"embeddinggemma-2".to_string()));
    assert!(engine.embedder().is_none());

    let embed = send_req(
        &h,
        &WorkerRequest {
            id: 2,
            op: "embed_text".into(),
            text: Some("x".into()),
            kind: Some("query".into()),
            ..Default::default()
        },
        &[],
    );
    assert!(!embed.ok);
    assert!(embed.error.unwrap().starts_with("Model not installed"));
    assert!(embed.model.is_none());
}

#[test]
fn agent_settings_input_wins_over_settings_json_and_fails_closed() {
    let dir = ScopedDir::new("s4w-args");
    let file = test_settings(dir.path(), |x| {
        x.index.dim = 768;
        x.inference.device = "cpu".into();
        x.ocr.languages = "de".into();
    });
    assert_eq!(768, file.dim);
    let s = WorkerSettings::try_from_json(
        r#"{"index":{"dim":512},"ocr":{"languages":"en"}}"#,
        dir.path(),
    )
    .expect("try_from_json");
    assert_eq!(512, s.dim);
    assert_eq!("en", s.ocr_languages);
    assert_eq!("auto", s.device);
    assert_eq!(140, s.image_tokens);
    assert!(WorkerSettings::try_from_json("not json", dir.path()).is_err());
    assert!(WorkerSettings::try_from_json("[1,2]", dir.path()).is_err());

    let read = |b: &[u8]| -> Result<String, String> {
        let mut cur = Cursor::new(b);
        WorkerSettings::read_agent_input(&mut cur)
    };
    assert_eq!(Ok("{}".to_string()), read(b"{}"));
    assert_eq!(Err("missing".to_string()), read(&[]));
    let oversized = vec![b' '; WorkerSettings::MAX_AGENT_INPUT_BYTES + 1];
    assert!(read(&oversized).unwrap_err().starts_with("larger than"));
    assert!(read(&[0xC3, 0x28]).is_err());

    let name = PipeNames::new_worker();
    let home_str = dir.path().to_string_lossy().into_owned();
    assert!(
        WorkerArgs::parse(&["--pipe", &name, "--home", &home_str, "--settings-stdin"])
            .unwrap()
            .settings_from_stdin
    );
    assert!(
        !WorkerArgs::parse(&["--pipe", &name, "--home", &home_str])
            .unwrap()
            .settings_from_stdin
    );
    assert!(WorkerArgs::parse(&["--pipe", &name, "--home", &home_str, "--settings", "{}"])
        .is_none());
    assert!(WorkerArgs::parse(&["--pipe", &name, "--home"]).is_none());
}

// ---- Sweep5WWorkerTests (W1 log rotation/retention) ----

fn to_system_time(dt: DateTime<FixedOffset>) -> SystemTime {
    let secs = dt.timestamp().max(0) as u64;
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn c8_worker_log_rotates_at_10mib_and_prunes_beyond_14_files() {
    let dir = ScopedDir::new("s5w-log");
    let utc = FixedOffset::east_opt(0).unwrap();
    let start_day = Utc::now().date_naive().and_hms_opt(10, 0, 0).unwrap();
    let now_base = DateTime::<FixedOffset>::from_naive_utc_and_offset(start_day, utc);
    let clock_val = Arc::new(Mutex::new(now_base));

    let mut old_names = Vec::new();
    for d in 1..=18 {
        let old_date = now_base - ChronoDuration::days(30 - d);
        let name = format!("worker-{}.log", old_date.format("%Y%m%d"));
        old_names.push(name.clone());
        let old_path = dir.path().join(&name);
        fs::write(&old_path, "old\n").unwrap();
        let f = OpenOptions::new().write(true).open(&old_path).unwrap();
        f.set_modified(to_system_time(old_date)).unwrap();
    }

    let today_file = dir
        .path()
        .join(format!("worker-{}.log", now_base.format("%Y%m%d")));
    {
        let f = File::create(&today_file).unwrap();
        f.set_len(Log::MAX_FILE_BYTES).unwrap();
        f.set_modified(to_system_time(now_base)).unwrap();
    }

    let c = clock_val.clone();
    let mut sink = LogSink::with_clock(move || *c.lock().unwrap());
    sink.init(Some(dir.path().to_path_buf()), "debug");

    let list_names = || -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("worker-") && n.ends_with(".log"))
            .collect();
        v.sort();
        v
    };

    let after_init: HashSet<String> = list_names().into_iter().collect();
    assert_eq!(Log::KEEP_FILES, after_init.len());
    assert!(!after_init.contains(&old_names[4]));
    assert!(after_init.contains(&old_names[5]));

    sink.write(0, "DBG", "first-rotated");
    let first_rot = dir
        .path()
        .join(format!("worker-{}-100000.log", now_base.format("%Y%m%d")));
    assert!(first_rot.exists());
    assert!(fs::read_to_string(&first_rot).unwrap().contains("first-rotated"));
    assert_eq!(Log::MAX_FILE_BYTES, fs::metadata(&today_file).unwrap().len());
    let after_first: HashSet<String> = list_names().into_iter().collect();
    assert_eq!(Log::KEEP_FILES, after_first.len());
    assert!(!after_first.contains(&old_names[5]));

    {
        let f = OpenOptions::new().write(true).open(&first_rot).unwrap();
        f.set_len(Log::MAX_FILE_BYTES).unwrap();
    }
    *clock_val.lock().unwrap() = now_base + ChronoDuration::seconds(1);
    sink.write(1, "INF", "second-rotated");
    let second_rot = dir
        .path()
        .join(format!("worker-{}-100001.log", now_base.format("%Y%m%d")));
    assert!(second_rot.exists());
    assert!(
        fs::read_to_string(&second_rot)
            .unwrap()
            .contains("second-rotated")
    );
    assert_eq!(Log::KEEP_FILES, list_names().len());
}

#[test]
fn c8_static_log_facade_writes_and_prunes_beyond_14_files() {
    let dir = ScopedDir::new("s5w-log-static");
    let utc = FixedOffset::east_opt(0).unwrap();
    let today = Utc::now().date_naive().and_hms_opt(0, 0, 0).unwrap();
    let today_dt = DateTime::<FixedOffset>::from_naive_utc_and_offset(today, utc);
    let mut oldest_path: Option<PathBuf> = None;
    for d in 1..=16 {
        let old_date = today_dt - ChronoDuration::days(30 - d);
        let old = dir
            .path()
            .join(format!("worker-{}.log", old_date.format("%Y%m%d")));
        if oldest_path.is_none() {
            oldest_path = Some(old.clone());
        }
        fs::write(&old, "old\n").unwrap();
        let f = OpenOptions::new().write(true).open(&old).unwrap();
        f.set_modified(to_system_time(old_date)).unwrap();
    }

    Log::init(Some(dir.path().to_path_buf()), "info");
    Log::info("static-probe");
    Log::init(None, "info");

    let count = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("worker-") && n.ends_with(".log")
        })
        .count();
    assert_eq!(Log::KEEP_FILES, count);
    assert!(!oldest_path.unwrap().exists());
}
