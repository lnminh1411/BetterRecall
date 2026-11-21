// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{
    BinaryFrame, CaptureKind, EmbeddingInfo, NewAudioCapture, NewScreenCapture, OcrResult,
    PendingJob, StorageUse, TranscribeResult, WorkerHello, WorkerRequest, WorkerResponse,
    to_json_string,
};
use br_core::{CancelSource, CancelToken, EncryptedStore, PipeStream};
use chrono::Utc;
use windows::Win32::Foundation::ERROR_PIPE_BUSY;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::PCWSTR;

use br_agent::host::StorageStore;
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
    current_cts: Mutex<Option<Arc<CancelSource>>>,
    current_done: Mutex<Option<Arc<AtomicBool>>>,
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
                current_cts: Mutex::new(None),
                current_done: Mutex::new(None),
            }),
        })
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
                let stream_close = stream.clone();
                let _reg = ct.on_cancel(move || stream_close.close());
                while !ct.is_cancelled() {
                    let Ok(Some((hdr, payload))) = read_pipe_frame(&stream, &ct) else {
                        break;
                    };
                    let Ok(req) = serde_json::from_slice::<WorkerRequest>(&hdr) else {
                        break;
                    };
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
}

impl SlowWorkerLauncher {
    pub fn new(transcribe_ms: u64, garbage_on_ocr: bool) -> Arc<Self> {
        Arc::new(Self {
            next_pid: AtomicU32::new(41000),
            transcribe_ms,
            garbage_on_ocr,
            started: Arc::new(Mutex::new(Vec::new())),
            killed: Arc::new(Mutex::new(Vec::new())),
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
        let _ = thread::spawn(move || {
            if let Ok(stream) = connect_named_pipe(&pipe, Duration::from_secs(5)) {
                while let Ok(Some((hdr, _))) = read_pipe_frame(&stream, &CancelToken::none()) {
                    let Ok(req) = serde_json::from_slice::<WorkerRequest>(&hdr) else {
                        break;
                    };
                    if req.op == "transcribe" {
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
