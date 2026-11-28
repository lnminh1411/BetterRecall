// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use br_contracts::{
    BinaryFrame, CaptureKind, CoreHostOptions, EmbeddingInfo, NewAudioCapture, NewScreenCapture,
    OcrResult, PendingJob, StorageUse, TranscribeResult, WorkerHello, WorkerRequest,
    WorkerResponse, to_json_string,
};
use br_core::{CancelSource, CancelToken, CoreHost, EncryptedStore, PipeStream};
use chrono::Utc;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::ERROR_PIPE_BUSY;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::PCWSTR;

use br_agent::audio::{AudioStore, AudioStoreError};
use br_agent::host::{AgentHost, AgentOptions, StorageStore};
use br_agent::native::{current_process_id, tick_count64, to_wide_z};
use br_agent::scheduling::{EmbeddingStore, GovernanceInputs, UserNotificationState};
use br_agent::worker::{WorkerError, WorkerLauncher, WorkerStream};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let root = std::env::var_os("BR_TEST_TMP")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("CARGO_TARGET_DIR")
                    .filter(|v| !v.is_empty())
                    .map(|p| PathBuf::from(p).join("test-tmp"))
            })
            .unwrap_or_else(std::env::temp_dir);
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = root.join(format!("br-agent-{prefix}-{pid}-{id}"));
        let _ = fs::create_dir_all(&path);
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for _ in 0..5 {
            if !self.path.exists() || fs::remove_dir_all(&self.path).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

pub fn idle_inputs() -> GovernanceInputs {
    GovernanceInputs {
        cpu_percent: 1.0,
        igpu_percent: Some(0.0),
        on_battery: false,
        notification: UserNotificationState::AcceptsNotifications,
        user_idle_ms: 0,
    }
}

pub fn busy_inputs() -> GovernanceInputs {
    GovernanceInputs {
        cpu_percent: 95.0,
        igpu_percent: Some(0.0),
        on_battery: false,
        notification: UserNotificationState::AcceptsNotifications,
        user_idle_ms: 0,
    }
}

pub fn until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !cond() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

pub fn cancel_after(d: Duration) -> Arc<CancelSource> {
    let cs = Arc::new(CancelSource::new());
    let bg = cs.clone();
    let _ = thread::spawn(move || {
        thread::sleep(d);
        bg.cancel();
    });
    cs
}

pub fn new_screen_capture(image: Vec<u8>) -> NewScreenCapture {
    NewScreenCapture {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "t".to_string(),
        monitor: 0,
        width: 10,
        height: 10,
        d_hash: 0,
        image,
        image_mime: "image/webp".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    }
}

pub fn new_audio_capture(text: &str) -> NewAudioCapture {
    NewAudioCapture {
        ts: Utc::now(),
        source: "mic".to_string(),
        duration_ms: 1000,
        text: text.to_string(),
        language: "en".to_string(),
        app: String::new(),
    }
}

/// Real `EncryptedStore` in a scratch folder with a random 32-byte key (never touches default home).
pub struct RealStore {
    pub dir: TempDir,
    pub store: Arc<EncryptedStore>,
}

impl RealStore {
    pub fn new(prefix: &str) -> Self {
        let dir = TempDir::new(prefix);
        let key = vec![0x42u8; 32];
        let store = Arc::new(
            EncryptedStore::open(dir.path(), move || key.clone(), None)
                .expect("open EncryptedStore"),
        );
        Self { dir, store }
    }

    pub fn add(&self, i: u8) -> String {
        let mut c = new_screen_capture(vec![1, 2, i]);
        c.ts = Utc::now() - chrono::Duration::minutes(10) + chrono::Duration::seconds(i as i64);
        c.title = format!("t{i}");
        self.store.add_screen_capture(&c).expect("add_screen_capture")
    }

    pub fn pending(&self) -> i32 {
        self.store.stats().expect("stats").pending_embeddings
    }
}

/// In-memory store implementing [`EmbeddingStore`] and [`StorageStore`].
pub struct FakeStoreInner {
    pub screens: HashMap<String, NewScreenCapture>,
    pub audio: HashMap<String, NewAudioCapture>,
    pub vectors: HashMap<String, Vec<f32>>,
    pub dropped: HashMap<String, String>,
    pub failed: HashMap<String, String>,
    pub pending: VecDeque<String>,
    pub space: Option<EmbeddingInfo>,
    pub storage_use: StorageUse,
    pub deletable: i32,
    pub freed_per_row: i64,
    pub delete_calls: i32,
}

impl Default for FakeStoreInner {
    fn default() -> Self {
        Self {
            screens: HashMap::new(),
            audio: HashMap::new(),
            vectors: HashMap::new(),
            dropped: HashMap::new(),
            failed: HashMap::new(),
            pending: VecDeque::new(),
            space: None,
            storage_use: StorageUse {
                file_bytes: 0,
                free_bytes: 0,
            },
            deletable: 0,
            freed_per_row: 0,
            delete_calls: 0,
        }
    }
}

pub struct FakeStore {
    next: AtomicUsize,
    pub inner: Mutex<FakeStoreInner>,
}

impl FakeStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next: AtomicUsize::new(0),
            inner: Mutex::new(FakeStoreInner::default()),
        })
    }

    pub fn add_screen(&self, c: NewScreenCapture) -> String {
        let id = format!("s{}", self.next.fetch_add(1, Ordering::AcqRel) + 1);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.screens.insert(id.clone(), c);
        g.pending.push_back(id.clone());
        id
    }

    pub fn add_audio(&self, c: NewAudioCapture) -> String {
        let id = format!("a{}", self.next.fetch_add(1, Ordering::AcqRel) + 1);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.audio.insert(id.clone(), c);
        g.pending.push_back(id.clone());
        id
    }
}

impl AudioStore for FakeStore {
    fn add_audio_capture(&self, cap: &NewAudioCapture) -> Result<String, AudioStoreError> {
        Ok(self.add_audio(cap.clone()))
    }
}

impl EmbeddingStore for FakeStore {
    fn store_identity(&self) -> usize {
        (self as *const FakeStore) as usize
    }

    fn ensure_embedding_space(&self, info: &EmbeddingInfo) -> Result<(), String> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).space = Some(info.clone());
        Ok(())
    }

    fn take_pending_embeddings(&self, max: usize) -> Result<Vec<PendingJob>, String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        while out.len() < max {
            let Some(id) = g.pending.pop_front() else {
                break;
            };
            if let Some(s) = g.screens.get(&id) {
                out.push(PendingJob {
                    id: id.clone(),
                    kind: CaptureKind::Screen,
                    image: Some(s.image.clone()),
                    image_mime: Some(s.image_mime.clone()),
                    text: None,
                });
            } else if let Some(a) = g.audio.get(&id) {
                out.push(PendingJob {
                    id: id.clone(),
                    kind: CaptureKind::Audio,
                    image: None,
                    image_mime: None,
                    text: Some(a.text.clone()),
                });
            }
        }
        Ok(out)
    }

    fn set_embedding(&self, id: &str, vector: &[f32], _space: &EmbeddingInfo) -> Result<(), String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .vectors
            .insert(id.to_string(), vector.to_vec());
        Ok(())
    }

    fn fail_embedding(&self, id: &str, reason: &str) -> Result<(), String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failed
            .insert(id.to_string(), reason.to_string());
        Ok(())
    }

    fn drop_capture(&self, id: &str, reason: &str) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.dropped.insert(id.to_string(), reason.to_string());
        g.screens.remove(id);
        Ok(())
    }
}

impl StorageStore for FakeStore {
    fn get_storage_use(&self) -> StorageUse {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .storage_use
    }

    fn delete_for_space(&self, batch: i32) -> i32 {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.delete_calls += 1;
        let n = batch.min(g.deletable);
        g.deletable -= n;
        let new_free = (g.storage_use.free_bytes + (n as i64) * g.freed_per_row)
            .min(g.storage_use.file_bytes);
        g.storage_use.free_bytes = new_free;
        n
    }
}

/// Connects to a named pipe as a client (overlapped) for in-process fake workers.
pub fn connect_named_pipe(pipe_name: &str, timeout: Duration) -> io::Result<PipeStream> {
    let full = format!(r"\\.\pipe\{}", br_contracts::PipeNames::base(pipe_name));
    let full_w = to_wide_z(&full);
    let deadline = Instant::now() + timeout;
    loop {
        let res = unsafe {
            CreateFileW(
                PCWSTR(full_w.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        match res {
            Ok(h) => return unsafe { PipeStream::from_raw_handle(h) },
            Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe busy timeout"));
                }
                let _ = unsafe { WaitNamedPipeW(PCWSTR(full_w.as_ptr()), 50) };
            }
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(br_core::win32::win32_to_io(e)),
        }
    }
}

pub fn read_pipe_frame(
    pipe: &PipeStream,
    ct: &CancelToken,
) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    let first = pipe.read(&mut len_buf, None, ct)?;
    if first == 0 {
        return Ok(None);
    }
    let mut pos = first;
    while pos < 4 {
        let n = pipe.read(&mut len_buf[pos..], None, ct)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        pos += n;
    }
    let header_len = u32::from_le_bytes(len_buf) as usize;
    let mut header = vec![0u8; header_len];
    read_exact_pipe(pipe, &mut header, ct)?;
    read_exact_pipe(pipe, &mut len_buf, ct)?;
    let payload_len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; payload_len];
    read_exact_pipe(pipe, &mut payload, ct)?;
    Ok(Some((header, payload)))
}

fn read_exact_pipe(pipe: &PipeStream, mut buf: &mut [u8], ct: &CancelToken) -> io::Result<()> {
    while !buf.is_empty() {
        let n = pipe.read(buf, None, ct)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        buf = &mut buf[n..];
    }
    Ok(())
}

pub fn write_pipe_frame(pipe: &PipeStream, header: &[u8], payload: &[u8]) -> io::Result<()> {
    let head = BinaryFrame::build_head(header, payload.len())
        .map_err(|e| io::Error::other(e.to_string()))?;
    pipe.write_all(&head, None, &CancelToken::none())?;
    if !payload.is_empty() {
        pipe.write_all(payload, None, &CancelToken::none())?;
    }
    Ok(())
}

pub fn resp_ok(id: i64) -> WorkerResponse {
    WorkerResponse {
        id,
        ok: true,
        error: None,
        dim: None,
        ms: None,
        dropped: None,
        hello: None,
        transcript: None,
        bench: None,
        ocr: None,
        model: None,
        revision: None,
        device: None,
    }
}

pub fn resp_err(id: i64, msg: &str) -> WorkerResponse {
    WorkerResponse {
        ok: false,
        error: Some(msg.to_string()),
        ..resp_ok(id)
    }
}

pub fn default_hello(rev: &str, dim: i32, device: &str) -> WorkerHello {
    WorkerHello {
        protocol: 1,
        device: device.to_string(),
        ep: "fake".to_string(),
        model: "fake-model".to_string(),
        revision: rev.to_string(),
        dim,
        asr: true,
        vad: true,
        missing: if rev.is_empty() {
            vec!["embeddinggemma-2".to_string()]
        } else {
            Vec::new()
        },
        nsfw_device: None,
        ocr_device: None,
    }
}

pub type FakeHandler =
    Arc<dyn Fn(&WorkerRequest, &[u8]) -> Option<(WorkerResponse, Vec<u8>)> + Send + Sync>;
pub type ReqPred = Arc<dyn Fn(&WorkerRequest) -> bool + Send + Sync>;

pub fn default_worker_reply(req: &WorkerRequest, payload: &[u8]) -> (WorkerResponse, Vec<u8>) {
    match req.op.as_str() {
        "hello" => {
            let mut r = resp_ok(req.id);
            r.hello = Some(default_hello("r1", 4, "CPU"));
            (r, Vec::new())
        }
        "embed_text" => {
            let mut r = resp_ok(req.id);
            r.dim = Some(4);
            r.ms = Some(1.0);
            let len = req.text.as_deref().unwrap_or("").len() as f32;
            (r, BinaryFrame::floats_to_bytes(&[len, 0.0, 0.0, 1.0]))
        }
        "embed_image" if !payload.is_empty() && payload[0] == 0xEE && req.nsfw.is_some() => {
            let mut r = resp_ok(req.id);
            r.dropped = Some("nsfw".to_string());
            (r, Vec::new())
        }
        "embed_image" => {
            let mut r = resp_ok(req.id);
            r.dim = Some(4);
            r.ms = Some(2.0);
            (
                r,
                BinaryFrame::floats_to_bytes(&[payload.len() as f32, 1.0, 0.0, 0.0]),
            )
        }
        "transcribe" => {
            let mut r = resp_ok(req.id);
            r.transcript = Some(TranscribeResult {
                speech: true,
                text: format!("{} samples", payload.len() / 4),
                language: "en".to_string(),
                segments: Vec::new(),
                ms: 3,
            });
            (r, Vec::new())
        }
        _ => (resp_err(req.id, "Bad request."), Vec::new()),
    }
}

/// Wrapper implementing `WorkerLauncher` cleanly with shared `Arc<FakeWorkerState>`.
pub struct FakeWorkerState {
    pub starts: AtomicUsize,
    pub kills: AtomicUsize,
    pub last_pipe: Mutex<Option<String>>,
    pub handler: Mutex<Option<FakeHandler>>,
    pub no_reply: Mutex<Option<ReqPred>>,
    pub crash_on_next: AtomicBool,
    pub crash_when: Mutex<Option<ReqPred>>,
    pub keepalive_every: Mutex<Option<Duration>>,
    pub keepalive_max: Mutex<Duration>,
    /// Recorded `(op, text)` for every request received by the fake worker (CC P4).
    pub seen: Mutex<Vec<(String, Option<String>)>>,
    current_cts: Mutex<Option<Arc<CancelSource>>>,
    current_done: Mutex<Option<Arc<AtomicBool>>>,
    current_stream: Mutex<Option<PipeStream>>,
}

pub struct FakeWorker {
    pub state: Arc<FakeWorkerState>,
}

impl FakeWorker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(FakeWorkerState {
                starts: AtomicUsize::new(0),
                kills: AtomicUsize::new(0),
                last_pipe: Mutex::new(None),
                handler: Mutex::new(None),
                no_reply: Mutex::new(None),
                crash_on_next: AtomicBool::new(false),
                crash_when: Mutex::new(None),
                keepalive_every: Mutex::new(None),
                keepalive_max: Mutex::new(Duration::from_secs(900)),
                seen: Mutex::new(Vec::new()),
                current_cts: Mutex::new(None),
                current_done: Mutex::new(None),
                current_stream: Mutex::new(None),
            }),
        })
    }

    pub fn seen_count(&self, op: &str, text: Option<&str>) -> usize {
        self.state
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(o, t)| o == op && (text.is_none() || t.as_deref() == text))
            .count()
    }

    pub fn wait_seen(&self, op: &str, text: Option<&str>, timeout: Duration) -> bool {
        until(|| self.seen_count(op, text) > 0, timeout)
    }

    /// Sends one keepalive frame (`id = 0`) over the active pipe stream (for deterministic liveness tests).
    pub fn send_keepalive(&self) -> bool {
        let stream = self
            .state
            .current_stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(stream) = stream else {
            return false;
        };
        let ka = to_json_string(&resp_ok(0)).unwrap().into_bytes();
        write_pipe_frame(&stream, &ka, &[]).is_ok()
    }

    pub fn set_handler(
        &self,
        f: impl Fn(&WorkerRequest, &[u8]) -> Option<(WorkerResponse, Vec<u8>)> + Send + Sync + 'static,
    ) {
        *self.state.handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    pub fn set_no_reply(&self, f: impl Fn(&WorkerRequest) -> bool + Send + Sync + 'static) {
        *self
            .state
            .no_reply
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    pub fn set_crash_when(&self, f: impl Fn(&WorkerRequest) -> bool + Send + Sync + 'static) {
        *self
            .state
            .crash_when
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    pub fn set_keepalive(&self, every: Duration, max: Duration) {
        *self
            .state
            .keepalive_every
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(every);
        *self
            .state
            .keepalive_max
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = max;
    }

    pub fn starts(&self) -> usize {
        self.state.starts.load(Ordering::Acquire)
    }

    pub fn kills(&self) -> usize {
        self.state.kills.load(Ordering::Acquire)
    }

    pub fn last_pipe(&self) -> Option<String> {
        self.state
            .last_pipe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl WorkerLauncher for FakeWorker {
    fn start(&self, pipe_name: &str, _home: &Path) -> Result<u32, WorkerError> {
        self.state.starts.fetch_add(1, Ordering::AcqRel);
        *self
            .state
            .last_pipe
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(pipe_name.to_string());
        let cts = Arc::new(CancelSource::new());
        let done = Arc::new(AtomicBool::new(false));
        *self
            .state
            .current_cts
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(cts.clone());
        *self
            .state
            .current_done
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(done.clone());

        let st = self.state.clone();
        let pipe = pipe_name.to_string();
        let _ = thread::spawn(move || {
            let ct = cts.token();
            if let Ok(stream) = connect_named_pipe(&pipe, Duration::from_secs(5)) {
                *st.current_stream
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(stream.clone());
                let stream_close = stream.clone();
                let _reg = ct.on_cancel(move || stream_close.close());
                while !ct.is_cancelled() {
                    let Ok(Some((hdr, payload))) = read_pipe_frame(&stream, &ct) else {
                        break;
                    };
                    let Ok(req) = serde_json::from_slice::<WorkerRequest>(&hdr) else {
                        break;
                    };
                    st.seen
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((req.op.clone(), req.text.clone()));
                    if st.crash_on_next.swap(false, Ordering::AcqRel) {
                        break;
                    }
                    if st
                        .crash_when
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .is_some_and(|f| f(&req))
                    {
                        break;
                    }
                    if req.op == "shutdown" {
                        break;
                    }
                    if st
                        .no_reply
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .is_some_and(|f| f(&req))
                    {
                        continue;
                    }
                    let handler = st.handler.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let every = *st.keepalive_every.lock().unwrap_or_else(|e| e.into_inner());
                    let max_ka = *st.keepalive_max.lock().unwrap_or_else(|e| e.into_inner());
                    let custom = if let (Some(every), Some(h)) = (every, handler.clone()) {
                        let req_c = req.clone();
                        let pay_c = payload.clone();
                        let res_slot = Arc::new((Mutex::new(None), Condvar::new()));
                        let res_bg = res_slot.clone();
                        let _ = thread::spawn(move || {
                            let r = h(&req_c, &pay_c);
                            let (l, cv) = &*res_bg;
                            *l.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                            cv.notify_all();
                        });
                        let until_ms = tick_count64() + max_ka.as_millis() as i64;
                        let (l, cv) = &*res_slot;
                        let mut g = l.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(r) = g.take() {
                                break r;
                            }
                            if ct.is_cancelled() {
                                break None;
                            }
                            let (g2, _) = cv
                                .wait_timeout(g, every)
                                .unwrap_or_else(|e| e.into_inner());
                            g = g2;
                            if g.is_none() && tick_count64() < until_ms && !ct.is_cancelled() {
                                let ka = to_json_string(&resp_ok(0)).unwrap().into_bytes();
                                let _ = write_pipe_frame(&stream, &ka, &[]);
                            }
                        }
                    } else {
                        handler.and_then(|h| h(&req, &payload))
                    };
                    if ct.is_cancelled() {
                        break;
                    }
                    let (resp, out_pay) =
                        custom.unwrap_or_else(|| default_worker_reply(&req, &payload));
                    let hdr_bytes = to_json_string(&resp).unwrap().into_bytes();
                    if write_pipe_frame(&stream, &hdr_bytes, &out_pay).is_err() {
                        break;
                    }
                }
                *st.current_stream
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                stream.close();
            }
            done.store(true, Ordering::Release);
        });
        Ok(current_process_id())
    }

    fn kill(&self, _pid: u32) {
        self.state.kills.fetch_add(1, Ordering::AcqRel);
        if let Some(c) = self
            .state
            .current_cts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            c.cancel();
        }
    }

    fn has_exited(&self, _pid: u32) -> bool {
        self.state
            .current_done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|d| d.load(Ordering::Acquire))
    }
}

/// Fake worker with a unique fake pid per start that lingers after shutdown (`PidWorkerLauncher`).
pub struct PidWorkerLauncher {
    next_pid: AtomicU32,
    pub alive: Arc<Mutex<HashMap<u32, Arc<CancelSource>>>>,
    pub killed: Arc<Mutex<Vec<u32>>>,
}

impl PidWorkerLauncher {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_pid: AtomicU32::new(1000),
            alive: Arc::new(Mutex::new(HashMap::new())),
            killed: Arc::new(Mutex::new(Vec::new())),
        })
    }
}

impl WorkerLauncher for PidWorkerLauncher {
    fn start(&self, pipe_name: &str, _home: &Path) -> Result<u32, WorkerError> {
        let pid = self.next_pid.fetch_add(1, Ordering::AcqRel) + 1;
        let cts = Arc::new(CancelSource::new());
        self.alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid, cts.clone());
        let alive = self.alive.clone();
        let pipe = pipe_name.to_string();
        let _ = thread::spawn(move || {
            let ct = cts.token();
            if let Ok(stream) = connect_named_pipe(&pipe, Duration::from_secs(5)) {
                let sc = stream.clone();
                let _reg = ct.on_cancel(move || sc.close());
                while !ct.is_cancelled() {
                    let Ok(Some((hdr, _))) = read_pipe_frame(&stream, &ct) else {
                        break;
                    };
                    let Ok(req) = serde_json::from_slice::<WorkerRequest>(&hdr) else {
                        break;
                    };
                    if req.op == "shutdown" {
                        thread::sleep(Duration::from_millis(600));
                        break;
                    }
                    let mut resp = resp_ok(req.id);
                    let mut pay = Vec::new();
                    if req.op == "hello" {
                        resp.hello = Some(default_hello("r", 4, "CPU"));
                    } else {
                        resp.dim = Some(4);
                        resp.ms = Some(1.0);
                        if req.op == "embed_text" {
                            pay = BinaryFrame::floats_to_bytes(&[1.0, 0.0, 0.0, 0.0]);
                        }
                    }
                    let h = to_json_string(&resp).unwrap().into_bytes();
                    if write_pipe_frame(&stream, &h, &pay).is_err() {
                        break;
                    }
                }
                stream.close();
            }
            alive.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
        });
        Ok(pid)
    }

    fn kill(&self, pid: u32) {
        self.killed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pid);
        if let Some(c) = self
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pid)
        {
            c.cancel();
        }
    }

    fn has_exited(&self, pid: u32) -> bool {
        !self
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&pid)
    }
}

/// Slow fake worker for A2 tests (`SlowWorkerLauncher`).
pub struct SlowWorkerLauncher {
    next_pid: AtomicU32,
    pub transcribe_ms: u64,
    pub garbage_on_ocr: bool,
    pub started: Arc<Mutex<Vec<u32>>>,
    pub killed: Arc<Mutex<Vec<u32>>>,
    pub transcribe_seen: Arc<AtomicUsize>,
}

impl SlowWorkerLauncher {
    pub fn new(transcribe_ms: u64, garbage_on_ocr: bool) -> Arc<Self> {
        Arc::new(Self {
            next_pid: AtomicU32::new(41000),
            transcribe_ms,
            garbage_on_ocr,
            started: Arc::new(Mutex::new(Vec::new())),
            killed: Arc::new(Mutex::new(Vec::new())),
            transcribe_seen: Arc::new(AtomicUsize::new(0)),
        })
    }
}

impl WorkerLauncher for SlowWorkerLauncher {
    fn start(&self, pipe_name: &str, _home: &Path) -> Result<u32, WorkerError> {
        let pid = self.next_pid.fetch_add(1, Ordering::AcqRel) + 1;
        self.started
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pid);
        let pipe = pipe_name.to_string();
        let tr_ms = self.transcribe_ms;
        let garbage = self.garbage_on_ocr;
        let tr_seen = self.transcribe_seen.clone();
        let _ = thread::spawn(move || {
            if let Ok(stream) = connect_named_pipe(&pipe, Duration::from_secs(5)) {
                while let Ok(Some((hdr, _))) = read_pipe_frame(&stream, &CancelToken::none()) {
                    let Ok(req) = serde_json::from_slice::<WorkerRequest>(&hdr) else {
                        break;
                    };
                    if req.op == "transcribe" {
                        tr_seen.fetch_add(1, Ordering::AcqRel);
                        thread::sleep(Duration::from_millis(tr_ms));
                    }
                    if req.op == "ocr" && garbage {
                        let _ = write_pipe_frame(&stream, b"not json", &[]);
                        continue;
                    }
                    let mut resp = resp_ok(req.id);
                    match req.op.as_str() {
                        "hello" => resp.hello = Some(default_hello("r", 4, "CPU")),
                        "transcribe" => {
                            resp.transcript = Some(TranscribeResult {
                                speech: true,
                                text: "hi".to_string(),
                                language: "en".to_string(),
                                segments: Vec::new(),
                                ms: 3,
                            });
                        }
                        _ => {
                            resp.ocr = Some(OcrResult {
                                lines: Vec::new(),
                                engine: "fake".to_string(),
                                ms: 1.0,
                                entities_missing: false,
                            });
                        }
                    }
                    let h = to_json_string(&resp).unwrap().into_bytes();
                    if write_pipe_frame(&stream, &h, &[]).is_err() {
                        break;
                    }
                }
                stream.close();
            }
        });
        Ok(pid)
    }

    fn kill(&self, pid: u32) {
        self.killed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pid);
    }

    fn has_exited(&self, _pid: u32) -> bool {
        false
    }
}

/// In-memory duplex stream with a gated first write (`GatedDuplexStream`).
pub struct GatedDuplexStream {
    inbound: Mutex<(VecDeque<u8>, bool)>,
    inbound_cv: Condvar,
    first_entered: Mutex<bool>,
    first_entered_cv: Condvar,
    first_released: Mutex<bool>,
    first_released_cv: Condvar,
    writes: AtomicUsize,
    written_ids: Mutex<Vec<i64>>,
}

impl GatedDuplexStream {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inbound: Mutex::new((VecDeque::new(), false)),
            inbound_cv: Condvar::new(),
            first_entered: Mutex::new(false),
            first_entered_cv: Condvar::new(),
            first_released: Mutex::new(false),
            first_released_cv: Condvar::new(),
            writes: AtomicUsize::new(0),
            written_ids: Mutex::new(Vec::new()),
        })
    }

    pub fn wait_first_write_entered(&self, timeout: Duration) -> bool {
        let g = self.first_entered.lock().unwrap_or_else(|e| e.into_inner());
        if *g {
            return true;
        }
        let (g2, _) = self
            .first_entered_cv
            .wait_timeout(g, timeout)
            .unwrap_or_else(|e| e.into_inner());
        *g2
    }

    pub fn release_first_write(&self) {
        *self
            .first_released
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
        self.first_released_cv.notify_all();
    }

    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::Acquire)
    }

    pub fn written_ids(&self) -> Vec<i64> {
        self.written_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn push_reply(&self, resp: &WorkerResponse, payload: &[u8]) {
        let hdr = to_json_string(resp).unwrap().into_bytes();
        let head = BinaryFrame::build_head(&hdr, payload.len()).unwrap();
        let mut g = self.inbound.lock().unwrap_or_else(|e| e.into_inner());
        g.0.extend(head);
        g.0.extend(payload);
        self.inbound_cv.notify_all();
    }

    pub fn close_inbound(&self) {
        let mut g = self.inbound.lock().unwrap_or_else(|e| e.into_inner());
        g.1 = true;
        self.inbound_cv.notify_all();
    }
}

impl WorkerStream for GatedDuplexStream {
    fn read(&self, buf: &mut [u8], ct: &CancelToken) -> io::Result<usize> {
        let mut g = self.inbound.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if !g.0.is_empty() {
                let take = buf.len().min(g.0.len());
                for b in buf.iter_mut().take(take) {
                    *b = g.0.pop_front().unwrap();
                }
                return Ok(take);
            }
            if g.1 {
                return Ok(0);
            }
            if ct.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let (g2, _) = self
                .inbound_cv
                .wait_timeout(g, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner());
            g = g2;
        }
    }

    fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        let n = self.writes.fetch_add(1, Ordering::AcqRel) + 1;
        if n == 1 {
            *self.first_entered.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.first_entered_cv.notify_all();
            let mut rel = self
                .first_released
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            while !*rel {
                rel = self
                    .first_released_cv
                    .wait(rel)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
        if buf.len() >= 8 {
            let header_len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
            if header_len > 0 && 4 + header_len <= buf.len()
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[4..4 + header_len])
                && let Some(id) = v.get("id").and_then(|x| x.as_i64())
            {
                self.written_ids
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(id);
            }
        }
        Ok(())
    }

    fn close(&self) {
        self.release_first_write();
        self.close_inbound();
    }
}

/// Resolves the Rust worker exe for tests that spawn the real worker (`BR_WORKER_DIR`, else target dir).
pub fn resolve_worker_exe() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("BR_WORKER_DIR")
        && !dir.trim().is_empty()
    {
        let p = PathBuf::from(&dir);
        for name in ["BetterRecall.Worker.exe", "br-worker.exe"] {
            let c = p.join(name);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(deps) = exe.parent()
        && let Some(profile_dir) = deps.parent()
    {
        for name in ["br-worker.exe", "BetterRecall.Worker.exe"] {
            let c = profile_dir.join(name);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

// ---------------- A5 test helpers ----------------

use br_agent::capture::{
    ImageCodec, KeyframeReader, KeyframeStore, KeyframeStoreError, ReplaySample, ReplaySampleSink,
};
use br_agent::thumbs::KeyframeBlobStore;
use br_contracts::{OcrEntity, OcrLine, ThumbData};

impl KeyframeStore for FakeStore {
    fn add_screen_capture(&self, cap: &NewScreenCapture) -> Result<String, KeyframeStoreError> {
        Ok(self.add_screen(cap.clone()))
    }
}

impl KeyframeBlobStore for FakeStore {
    fn get_keyframe(&self, id: &str) -> Result<Option<ThumbData>, String> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(g.screens.get(id).map(|c| ThumbData {
            id: id.to_string(),
            mime: c.image_mime.clone(),
            width: c.width,
            height: c.height,
            bytes: c.image.clone(),
        }))
    }
}

pub struct FakeSampleSink {
    pub samples: Mutex<Vec<ReplaySample>>,
}

impl FakeSampleSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            samples: Mutex::new(Vec::new()),
        })
    }

    pub fn count(&self) -> usize {
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn snapshot(&self) -> Vec<ReplaySample> {
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl ReplaySampleSink for FakeSampleSink {
    fn add(&self, sample: ReplaySample, _keep_seconds: i32) {
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(sample);
    }
}

type AnswerFn = Arc<dyn Fn() -> Result<OcrResult, WorkerError> + Send + Sync>;
type ReadyFn = Arc<dyn Fn(&CancelToken) -> Result<i32, WorkerError> + Send + Sync>;

pub struct SwitchReader {
    answer: Mutex<AnswerFn>,
    ready_fn: Mutex<Option<ReadyFn>>,
    is_ready_flag: AtomicBool,
    delay_ms: AtomicU64,
    pub calls: AtomicUsize,
    pub last_w: Mutex<i32>,
    pub last_h: Mutex<i32>,
    pub last_entities: Mutex<Option<bool>>,
}

impl SwitchReader {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(Arc::new(|| {
                Ok(OcrResult {
                    lines: Vec::new(),
                    engine: "fake".to_string(),
                    ms: 1.0,
                    entities_missing: false,
                })
            })),
            ready_fn: Mutex::new(None),
            is_ready_flag: AtomicBool::new(true),
            delay_ms: AtomicU64::new(0),
            calls: AtomicUsize::new(0),
            last_w: Mutex::new(0),
            last_h: Mutex::new(0),
            last_entities: Mutex::new(None),
        })
    }

    pub fn with_answer(
        f: impl Fn() -> Result<OcrResult, WorkerError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        let r = Self::new();
        r.set_answer(f);
        r
    }

    pub fn set_answer(
        &self,
        f: impl Fn() -> Result<OcrResult, WorkerError> + Send + Sync + 'static,
    ) {
        *self.answer.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(f);
    }

    pub fn set_ready(
        &self,
        is_ready: bool,
        f: impl Fn(&CancelToken) -> Result<i32, WorkerError> + Send + Sync + 'static,
    ) {
        self.is_ready_flag.store(is_ready, Ordering::Release);
        *self.ready_fn.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    pub fn set_is_ready(&self, ready: bool) {
        self.is_ready_flag.store(ready, Ordering::Release);
    }

    pub fn set_delay(&self, d: Duration) {
        self.delay_ms.store(d.as_millis() as u64, Ordering::Release);
    }
}

impl KeyframeReader for SwitchReader {
    fn read(
        &self,
        _bgra: &[u8],
        width: i32,
        height: i32,
        _stride: i32,
        entities: bool,
        ct: &CancelToken,
    ) -> Result<OcrResult, WorkerError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        *self.last_w.lock().unwrap_or_else(|e| e.into_inner()) = width;
        *self.last_h.lock().unwrap_or_else(|e| e.into_inner()) = height;
        *self.last_entities.lock().unwrap_or_else(|e| e.into_inner()) = Some(entities);
        let d = self.delay_ms.load(Ordering::Acquire);
        if d > 0 && ct.wait_timeout(Duration::from_millis(d)) {
            return Err(WorkerError::Cancelled);
        }
        if ct.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let f = self.answer.lock().unwrap_or_else(|e| e.into_inner()).clone();
        f()
    }

    fn is_ready(&self) -> bool {
        self.is_ready_flag.load(Ordering::Acquire)
    }

    fn ready(&self, ct: &CancelToken) -> Result<i32, WorkerError> {
        let f = self
            .ready_fn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match f {
            Some(cb) => cb(ct),
            None => Ok(0),
        }
    }
}

/// Deterministic LCG so synthetic screen generators match C# tests without extra crates.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }

    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next_u32() as i32).rem_euclid(hi - lo)
    }
}

pub fn screen_flat(w: i32, h: i32, v: u8) -> Vec<u8> {
    let mut px = vec![0u8; (w as usize) * (h as usize) * 4];
    for c in px.as_chunks_mut::<4>().0 {
        c[0] = v;
        c[1] = v;
        c[2] = v;
        c[3] = 255;
    }
    px
}

pub fn screen_rect(
    px: &mut [u8],
    w: i32,
    x0: i32,
    y0: i32,
    rw: i32,
    rh: i32,
    bgr: (u8, u8, u8),
) {
    for y in y0..(y0 + rh) {
        for x in x0..(x0 + rw) {
            let i = ((y * w + x) as usize) * 4;
            px[i] = bgr.0;
            px[i + 1] = bgr.1;
            px[i + 2] = bgr.2;
        }
    }
}

pub fn screen_text(px: &mut [u8], w: i32, x0: i32, y0: i32, rw: i32, rh: i32, seed: u64) {
    let mut rnd = Lcg::new(seed);
    let mut line = y0;
    while line + 10 <= y0 + rh {
        let mut x = x0;
        while x < x0 + rw - 2 {
            let width = rnd.range(1, 3);
            let top = line + rnd.range(0, 3);
            for y in top..(line + 10) {
                for dx in 0..width {
                    let i = ((y * w + x + dx) as usize) * 4;
                    px[i] = 20;
                    px[i + 1] = 20;
                    px[i + 2] = 20;
                }
            }
            x += rnd.range(4, 7);
        }
        line += 16;
    }
}

pub fn screen_gradient(px: &mut [u8], w: i32, x0: i32, y0: i32, rw: i32, rh: i32) {
    for y in y0..(y0 + rh) {
        for x in x0..(x0 + rw) {
            let i = ((y * w + x) as usize) * 4;
            px[i] = (x * 255 / w) as u8;
            px[i + 1] = (y * 255 / (y0 + rh)) as u8;
            px[i + 2] = 128;
        }
    }
}

pub fn screen_ui(w: i32, h: i32) -> Vec<u8> {
    let mut px = screen_flat(w, h, 250);
    screen_rect(&mut px, w, 0, 0, w, 40, (60, 40, 30));
    screen_text(&mut px, w, 40, 80, w - 80, h - 200, 5);
    screen_gradient(&mut px, w, w / 2, h - 110, w / 2 - 10, 100);
    px
}

pub fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let mut sum = 0u64;
    let mut n = 0u64;
    for (ca, cb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0.iter()) {
        for c in 0..3 {
            sum += ca[c].abs_diff(cb[c]) as u64;
            n += 1;
        }
    }
    sum as f64 / n.max(1) as f64
}

pub fn white_webp(w: i32, h: i32) -> Vec<u8> {
    let px = screen_flat(w, h, 255);
    unsafe { ImageCodec::encode_bgra(px.as_ptr(), w, h, w * 4, "webp", 100) }.expect("encode webp")
}

pub fn ocr_line(text: &str, x: i32, y: i32, h: i32, entities: Option<Vec<OcrEntity>>) -> OcrLine {
    let len = text.chars().count();
    let cuts: Vec<i32> = (0..=len).map(|i| x + (i as i32) * 10).collect();
    OcrLine {
        text: text.to_string(),
        x,
        y,
        w: (len as i32) * 10,
        h,
        confidence: 0.95,
        cuts,
        entities,
    }
}

// ---------------- Shared HTTP server, AgentHost fixture, and WrongPidLauncher ----------------

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[derive(Debug, Clone)]
pub struct RecordedReq {
    pub path: String,
    pub range: Option<String>,
}

#[derive(Default)]
struct HttpServerState {
    files: HashMap<String, Vec<u8>>,
    slow_path: Option<(String, usize, Duration)>,
    requests: Vec<RecordedReq>,
}

/// Loopback HTTP test server supporting Range resume and optional slow-drip responses.
pub struct LocalHttpServer {
    addr: String,
    state: Arc<Mutex<HttpServerState>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LocalHttpServer {
    pub fn start(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let addr = format!("127.0.0.1:{port}");
        let state = Arc::new(Mutex::new(HttpServerState {
            files,
            slow_path: None,
            requests: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let st_c = state.clone();
        let stop_c = stop.clone();
        let handle = thread::spawn(move || {
            for incoming in listener.incoming() {
                if stop_c.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = incoming {
                    let st_req = st_c.clone();
                    thread::spawn(move || serve_http_one(stream, &st_req));
                }
            }
        });
        Self {
            addr,
            state,
            stop,
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    pub fn feed_url(&self) -> String {
        format!("http://{}/releases", self.addr)
    }

    pub fn release_prefix(&self) -> String {
        format!("http://{}/releases/", self.addr)
    }

    pub fn asset_url(&self, path: &str) -> String {
        let clean = path.trim_start_matches('/');
        format!("http://{}/{clean}", self.addr)
    }

    pub fn set_file(&self, path: &str, data: Vec<u8>) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files
            .insert(path.to_string(), data);
    }

    pub fn set_slow(&self, path: &str, first_chunk: usize, delay: Duration) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slow_path = Some((path.to_string(), first_chunk, delay));
    }

    pub fn clear_slow(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slow_path = None;
    }

    pub fn requests(&self) -> Vec<RecordedReq> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .requests
            .clone()
    }

    pub fn request_paths(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.path).collect()
    }
}

impl Drop for LocalHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve_http_one(mut stream: TcpStream, state: &Arc<Mutex<HttpServerState>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
        return;
    }
    let path = req_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    let mut range: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':')
            && k.eq_ignore_ascii_case("range")
        {
            range = Some(v.trim().to_string());
        }
    }

    let (data_opt, slow) = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.requests.push(RecordedReq {
            path: path.clone(),
            range: range.clone(),
        });
        let slow = st
            .slow_path
            .as_ref()
            .filter(|(p, _, _)| p == &path)
            .map(|(_, chunk, delay)| (*chunk, *delay));
        (st.files.get(&path).cloned(), slow)
    };

    let Some(data) = data_opt else {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    };

    let (status_line, slice, extra) = if let Some(ref r) = range
        && let Some(start_str) = r.strip_prefix("bytes=").and_then(|s| s.strip_suffix('-'))
        && let Ok(start) = start_str.parse::<usize>()
        && start < data.len()
    {
        let end = data.len() - 1;
        (
            "HTTP/1.1 206 Partial Content",
            &data[start..],
            format!("Content-Range: bytes {start}-{end}/{}\r\n", data.len()),
        )
    } else {
        ("HTTP/1.1 200 OK", data.as_slice(), String::new())
    };

    let _ = write!(
        stream,
        "{status_line}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        slice.len()
    );
    if let Some((first_chunk, delay)) = slow {
        let split = first_chunk.min(slice.len());
        let _ = stream.write_all(&slice[..split]);
        let _ = stream.flush();
        thread::sleep(delay);
        let _ = stream.write_all(&slice[split..]);
    } else {
        let _ = stream.write_all(slice);
    }
    let _ = stream.flush();
}

/// Builds an isolated `AgentHost` + `CoreHost` in `dir` with hello probing disabled.
pub fn make_test_host(dir: &TempDir) -> Arc<AgentHost> {
    let home = dir.path().join("home");
    let install = dir.path().join("install");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&install).unwrap();
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: home.clone(),
            install_dir: install.clone(),
        },
        false,
    );
    AgentHost::new(
        core,
        AgentOptions {
            install_dir: install,
            home,
            autostart: false,
        },
    )
}

/// Launcher that connects from the current process while reporting a different PID,
/// exercising `WorkerSupervisor`'s `verify_client_pid` rejection path.
pub struct WrongPidLauncher {
    pub reported_pid: u32,
    pub killed: Mutex<Vec<u32>>,
}

impl WrongPidLauncher {
    pub fn new() -> Arc<Self> {
        let reported_pid = current_process_id().wrapping_add(1).max(1);
        Arc::new(Self {
            reported_pid,
            killed: Mutex::new(Vec::new()),
        })
    }
}

impl WorkerLauncher for WrongPidLauncher {
    fn start(&self, pipe_name: &str, _home: &Path) -> Result<u32, WorkerError> {
        let pipe = pipe_name.to_string();
        let _ = thread::spawn(move || {
            if let Ok(stream) = connect_named_pipe(&pipe, Duration::from_secs(5)) {
                let _ = read_pipe_frame(&stream, &CancelToken::none());
                stream.close();
            }
        });
        Ok(self.reported_pid)
    }

    fn kill(&self, pid: u32) {
        self.killed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pid);
    }

    fn has_exited(&self, _pid: u32) -> bool {
        true
    }
}
