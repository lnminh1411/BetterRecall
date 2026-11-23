// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{
    BenchResult, BinaryFrame, DataPaths, OcrLine, OcrResult, PipeNames, Settings, TranscribeResult,
    WorkerHello, WorkerRequest, WorkerResponse,
};
use br_worker::engine::{EngineFactory, FakeEngine, IWorkerEngine, ImageEmbedding, ImageInput, TextKind};
use br_worker::hosting::WorkerSettings;
use br_worker::protocol::{
    AgentPipe, LoopExit, RequestHandler, WorkerError, WorkerErrors, WorkerLoop,
};
use br_worker::vectors::VectorMath;
use common::{
    PipeHarness, ScopedDir, bgra, create_overlapped_pipe_server, finish_connect, pcm_sine,
    send_req, test_settings, write_raw_overlapped,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};

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

    // Unknown 2-letter code is a bad_request, not a device failure (Sweep2G).
    let mut bad = speech.clone();
    bad.source = Some("mic".into());
    bad.language = Some("xx".into());
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


// ---- Sweep2GTests (W1 subset) ----

#[test]
fn idle_timer_firing_mid_frame_finishes_the_frame() {
    use br_worker::protocol::worker_loop::{FrameRead, PipeIo};
    let dir = ScopedDir::new("idle-mid");
    let s = test_settings(dir.path(), |x| x.inference.fake = true);
    let mut h = PipeHarness::start(
        Arc::new(FakeEngine),
        s,
        Duration::from_millis(200),
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
    thread::sleep(Duration::from_millis(500));
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

fn run_keepalive_case(handler_ms: u64, every: Duration, max: Duration) -> Vec<(i64, Duration)> {
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
    let started = Instant::now();
    h.server().write_frame(&req, &[]).unwrap();

    let mut arrivals = Vec::new();
    loop {
        let FrameRead::Frame(hdr, payload) = h
            .server()
            .read_frame_idle(Duration::from_secs(20))
            .expect("read frame")
        else {
            panic!("unexpected eof or timeout");
        };
        let arrived = started.elapsed();
        let resp: WorkerResponse = serde_json::from_slice(&hdr).unwrap();
        arrivals.push((resp.id, arrived));
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
    arrivals
}

#[test]
fn long_request_sends_keepalives_before_its_reply() {
    let arrivals = run_keepalive_case(
        700,
        Duration::from_millis(100),
        Duration::from_secs(60),
    );
    assert_eq!(Some(7), arrivals.last().map(|&(id, _)| id));
    let zero_count = arrivals.iter().filter(|&&(id, _)| id == 0).count();
    assert!(zero_count >= 4, "keepalives: {zero_count}");
    assert!(arrivals[..arrivals.len() - 1].iter().all(|&(id, _)| id == 0));
}

#[test]
fn fast_request_sends_no_keepalive() {
    let arrivals = run_keepalive_case(0, Duration::from_millis(300), Duration::from_secs(60));
    let ids: Vec<i64> = arrivals.into_iter().map(|(id, _)| id).collect();
    assert_eq!(vec![7i64], ids);
}

#[test]
fn keepalives_stop_after_their_cap() {
    let every = Duration::from_millis(100);
    let max = Duration::from_millis(250);
    let arrivals = run_keepalive_case(1500, every, max);
    assert_eq!(Some(7), arrivals.last().map(|&(id, _)| id));
    let cutoff = max + every * 5;
    for &(id, at) in &arrivals {
        if id == 0 {
            assert!(
                at <= cutoff,
                "keepalive arrived at {at:?}, after cap + 5 intervals {cutoff:?}"
            );
        }
    }
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


// ---- Worker process exit-code tests (Proposal 9: CARGO_BIN_EXE_br-worker) ----

fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll worker child") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("br-worker did not exit within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn worker_exe_exits_2_on_bad_settings_stdin() {
    use std::io::Write;
    use std::process::Stdio;

    let exe = env!("CARGO_BIN_EXE_br-worker");
    let huge = format!(r#"{{"log":{{"level":"{}"}}}}"#, "a".repeat(70_000));
    let cases: [(&str, &[u8]); 3] = [
        ("missing", b""),
        ("garbage", b"{ not json"),
        ("huge", huge.as_bytes()),
    ];

    for (kind, payload) in cases {
        let dir = ScopedDir::new(&format!("proc-bad-{kind}"));
        let home = dir.path().join("home");
        let pipe = PipeNames::new_worker();
        let mut child = Command::new(exe)
            .args([
                "--pipe",
                &pipe,
                "--home",
                &home.to_string_lossy(),
                "--settings-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn br-worker");
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload);
        }
        let status = wait_with_timeout(&mut child, Duration::from_secs(30));
        assert_eq!(Some(2), status.code(), "kind={kind}");

        let logs_dir = DataPaths::logs_dir(&home);
        let log_file = fs::read_dir(&logs_dir)
            .expect("logs dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                let n = p.file_name().unwrap().to_string_lossy();
                n.starts_with("worker-") && n.ends_with(".log")
            })
            .expect("worker log file");
        let text = fs::read_to_string(&log_file).expect("read worker log");
        assert!(
            text.contains("settings from the agent"),
            "kind={kind} log={text}"
        );
        assert!(
            !text.contains("start settings="),
            "kind={kind} log={text}"
        );
    }
}

#[test]
fn worker_exe_rejects_non_agent_pipe_server_and_bad_pipe_name() {
    use std::process::Stdio;

    let exe = env!("CARGO_BIN_EXE_br-worker");
    let dir = ScopedDir::new("proc-reject");
    let home = dir.path().join("home");
    let _ = test_settings(&home, |x| x.inference.fake = true);

    // 1. A pipe server that is not BetterRecall.Agent.exe -> exit 4.
    let pipe = PipeNames::new_worker();
    let (server_raw, ev) = create_overlapped_pipe_server(&pipe, 65_536, 65_536);
    let mut child = Command::new(exe)
        .args(["--pipe", &pipe, "--home", &home.to_string_lossy()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn br-worker");
    connect_or_fail(server_raw, ev, &mut child, Duration::from_secs(30));
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    unsafe {
        let _ = CloseHandle(server_raw);
    }
    assert_eq!(Some(4), status.code(), "non-agent pipe server must exit 4");

    // 2. A pipe name outside the BetterRecall worker prefix -> exit 2.
    let mut bad = Command::new(exe)
        .args([
            "--pipe",
            "not-a-worker-pipe",
            "--home",
            &home.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn br-worker with bad pipe");
    let bad_status = wait_with_timeout(&mut bad, Duration::from_secs(30));
    assert_eq!(Some(2), bad_status.code(), "bad pipe name must exit 2");
}

/// Waits for the worker to connect to `server`, failing (and killing the worker) if it exits
/// first or does not connect within `limit`, instead of blocking forever.
fn connect_or_fail(server: HANDLE, ev: HANDLE, child: &mut std::process::Child, limit: Duration) {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::WaitForSingleObject;
    let start = std::time::Instant::now();
    loop {
        if unsafe { WaitForSingleObject(ev, 100) } == WAIT_OBJECT_0 {
            finish_connect(server, ev);
            return;
        }
        if let Some(st) = child.try_wait().expect("try_wait") {
            panic!("worker exited before connecting: {st}");
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            panic!("worker did not connect within {limit:?}");
        }
    }
}
