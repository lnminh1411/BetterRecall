// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

#![allow(clippy::type_complexity)] // test doubles hold boxed callbacks

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use br_contracts::{
    AgentStatus, AudioState, BenchDevice, BenchReport, BenchResult, CaptureBench, CaptureMeta,
    EncoderBench, GpuInfo, HardwareInfo, ModelsList, ModelsProgress, MonitorInfo, NpuInfo,
    QueueState, ReplayClip, StoreStats, ThumbData, UpdateProgress, UpdateStatus,
};
use br_core::{
    AgentControl, AgentError, CancelToken, EmbedError, HttpRequest, HttpResponse, HttpTransport,
    ListenerGuard, QueryEmbedder, Thumbnailer, TransportError,
};
use chrono::{DateTime, Utc};

use super::{vec_seeded};

/// Unique pipe name scoped to a test (never touches default unscoped pipe names).
pub fn test_pipe_name(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("BRtest.{}.{}.{tag}", std::process::id(), n)
}

/// Recorded HTTP request on [`FakeHttpServer`].
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct FakeServerState {
    status: u16,
    content_type: String,
    response_body: String,
    extra_headers: Vec<(String, String)>,
    requests: Vec<RecordedRequest>,
    /// Optional scripted responses (popped from front if non-empty).
    scripted: Vec<(u16, String, Vec<(String, String)>, String)>,
}

/// Minimal HTTP/1.1 server on `127.0.0.1:0` for cloud describe and chat tests (`FakeHttpServer.cs`).
pub struct FakeHttpServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<FakeServerState>>,
    handle: Option<JoinHandle<()>>,
}

impl FakeHttpServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on listener");
        let addr = listener.local_addr().expect("local_addr");
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(FakeServerState {
            status: 200,
            content_type: "application/json".to_string(),
            response_body: "{}".to_string(),
            extra_headers: Vec::new(),
            requests: Vec::new(),
            scripted: Vec::new(),
        }));

        let stop_bg = stop.clone();
        let state_bg = state.clone();
        let handle = thread::Builder::new()
            .name("FakeHttpServer".to_string())
            .spawn(move || {
                while !stop_bg.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let st = state_bg.clone();
                            let _ = thread::spawn(move || serve_http(stream, st));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn FakeHttpServer");

        Self {
            addr,
            stop,
            state,
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    pub fn set_status(&self, status: u16) {
        self.state.lock().unwrap().status = status;
    }

    pub fn set_response_body(&self, body: impl Into<String>) {
        self.state.lock().unwrap().response_body = body.into();
    }

    pub fn set_content_type(&self, ct: impl Into<String>) {
        self.state.lock().unwrap().content_type = ct.into();
    }

    pub fn push_response(
        &self,
        status: u16,
        content_type: impl Into<String>,
        headers: Vec<(String, String)>,
        body: impl Into<String>,
    ) {
        self.state
            .lock()
            .unwrap()
            .scripted
            .push((status, content_type.into(), headers, body.into()));
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Drop for FakeHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve_http(mut stream: TcpStream, state: Arc<Mutex<FakeServerState>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos;
                }
            }
        }
    };
    let head_str = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head_str.split("\r\n");
    let first_line = lines.next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    for h in lines {
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let content_len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_len {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
    }
    body.truncate(content_len);

    let (status, content_type, extra_headers, resp_body) = {
        let mut st = state.lock().unwrap();
        st.requests.push(RecordedRequest {
            method,
            path,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        if !st.scripted.is_empty() {
            st.scripted.remove(0)
        } else {
            (
                st.status,
                st.content_type.clone(),
                st.extra_headers.clone(),
                st.response_body.clone(),
            )
        }
    };

    let payload = resp_body.as_bytes();
    let mut resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    for (k, v) in extra_headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str("\r\n");
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(payload);
    let _ = stream.flush();
}

/// Scriptable mock [`HttpTransport`] for unit tests that need custom headers, retries, or offline/timeout errors.
pub struct MockTransport {
    handler: Mutex<Box<dyn FnMut(HttpRequest, Duration, &CancelToken) -> Result<HttpResponse, TransportError> + Send>>,
}

impl MockTransport {
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut(HttpRequest, Duration, &CancelToken) -> Result<HttpResponse, TransportError>
            + Send
            + 'static,
    {
        Self {
            handler: Mutex::new(Box::new(f)),
        }
    }

    pub fn ok_text(body: impl Into<String>, headers: Vec<(String, String)>) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers,
            body: Box::new(Cursor::new(body.into().into_bytes())),
        }
    }

    pub fn status_text(
        status: u16,
        body: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> HttpResponse {
        HttpResponse {
            status,
            headers,
            body: Box::new(Cursor::new(body.into().into_bytes())),
        }
    }
}

impl HttpTransport for MockTransport {
    fn send(
        &self,
        req: HttpRequest,
        header_timeout: Duration,
        ct: &CancelToken,
    ) -> Result<HttpResponse, TransportError> {
        let mut h = self.handler.lock().unwrap();
        h(req, header_timeout, ct)
    }
}

/// Fake [`AgentControl`] matching `TestSupport.cs`.
pub struct FakeAgent {
    pub unlocked: AtomicBool,
    pub unlock_result: AtomicBool,
    pub calls: Mutex<Vec<String>>,
    pub status: Mutex<AgentStatus>,
    pub replays: Mutex<Vec<ReplayClip>>,
    pub full_report: Mutex<Option<BenchReport>>,
    pub block_full_bench: AtomicBool,
    pub full_bench_started: Arc<(Mutex<bool>, Condvar)>,
    pub full_bench_cancelled: Arc<(Mutex<bool>, Condvar)>,
    pub on_save_replay: Mutex<Option<Box<dyn Fn(&CancelToken) -> Result<ReplayClip, AgentError> + Send + Sync>>>,
    pub on_export_replay: Mutex<Option<Box<dyn Fn(&str, &Path, &CancelToken) -> Result<(), AgentError> + Send + Sync>>>,
    pub models_list_reply: Mutex<Option<ModelsList>>,
    pub on_models_download: Mutex<Option<Arc<dyn Fn(&str, &mut dyn FnMut(ModelsProgress), &CancelToken) -> Result<(), AgentError> + Send + Sync>>>,
    pub update_status_reply: Mutex<Option<UpdateStatus>>,
    pub on_update_check: Mutex<Option<Arc<dyn Fn(&CancelToken) -> Result<UpdateStatus, AgentError> + Send + Sync>>>,
    pub on_update_download: Mutex<Option<Arc<dyn Fn(&mut dyn FnMut(UpdateProgress), &CancelToken) -> Result<(), AgentError> + Send + Sync>>>,
    pub on_update_apply: Mutex<Option<Arc<dyn Fn() -> Result<UpdateStatus, AgentError> + Send + Sync>>>,
    next_id: AtomicU64,
    status_listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(&AgentStatus) + Send + Sync>)>>>,
    capture_listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(&CaptureMeta) + Send + Sync>)>>>,
    panic_listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn() + Send + Sync>)>>>,
}

impl Default for FakeAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeAgent {
    pub fn new() -> Self {
        Self {
            unlocked: AtomicBool::new(true),
            unlock_result: AtomicBool::new(true),
            calls: Mutex::new(Vec::new()),
            status: Mutex::new(AgentStatus {
                capturing: true,
                paused: false,
                paused_until: None,
                pause_reason: None,
                locked: false,
                unlocked_until: None,
                audio: AudioState {
                    system: false,
                    mic: false,
                },
                queue: QueueState { pending: 0 },
                device: "CPU".to_string(),
                store: StoreStats::default(),
                retention_days: 30,
                hotkeys_failed: None,
                warnings: None,
                user_paused: false,
                unlock_needs: None,
            }),
            replays: Mutex::new(Vec::new()),
            full_report: Mutex::new(None),
            block_full_bench: AtomicBool::new(false),
            full_bench_started: Arc::new((Mutex::new(false), Condvar::new())),
            full_bench_cancelled: Arc::new((Mutex::new(false), Condvar::new())),
            on_save_replay: Mutex::new(None),
            on_export_replay: Mutex::new(None),
            models_list_reply: Mutex::new(None),
            on_models_download: Mutex::new(None),
            update_status_reply: Mutex::new(None),
            on_update_check: Mutex::new(None),
            on_update_download: Mutex::new(None),
            on_update_apply: Mutex::new(None),
            next_id: AtomicU64::new(1),
            status_listeners: Arc::new(Mutex::new(Vec::new())),
            capture_listeners: Arc::new(Mutex::new(Vec::new())),
            panic_listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn raise_status(&self) {
        let st = self.status.lock().unwrap().clone();
        let list = self.status_listeners.lock().unwrap().clone();
        for (_, cb) in list {
            cb(&st);
        }
    }

    pub fn raise_capture(&self, m: &CaptureMeta) {
        let list = self.capture_listeners.lock().unwrap().clone();
        for (_, cb) in list {
            cb(m);
        }
    }

    pub fn raise_panic(&self) {
        let list = self.panic_listeners.lock().unwrap().clone();
        for (_, cb) in list {
            cb();
        }
    }

    pub fn wait_flag(flag: &(Mutex<bool>, Condvar), timeout: Duration) -> bool {
        let (lock, cv) = flag;
        let guard = lock.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(guard, timeout, |v| !*v)
            .unwrap();
        *g
    }

    fn set_flag(flag: &(Mutex<bool>, Condvar)) {
        let (lock, cv) = flag;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

impl AgentControl for FakeAgent {
    fn get_status(&self) -> AgentStatus {
        self.status.lock().unwrap().clone()
    }

    fn pause(&self, duration: Option<Duration>, reason: &str) {
        let d = duration.map(|d| d.as_secs().to_string()).unwrap_or_default();
        self.calls.lock().unwrap().push(format!("pause {d} {reason}"));
    }

    fn resume(&self, reason: &str) {
        self.calls.lock().unwrap().push(format!("resume {reason}"));
    }

    fn is_unlocked(&self) -> bool {
        self.unlocked.load(Ordering::Acquire)
    }

    fn unlock(&self, _ct: &CancelToken) -> bool {
        self.calls.lock().unwrap().push("unlock".to_string());
        let ok = self.unlock_result.load(Ordering::Acquire);
        if ok {
            self.unlocked.store(true, Ordering::Release);
        }
        ok
    }

    fn lock(&self) {
        self.calls.lock().unwrap().push("lock".to_string());
        self.unlocked.store(false, Ordering::Release);
    }

    fn open_read_gate(&self) {
        self.calls.lock().unwrap().push("opengate".to_string());
        self.unlocked.store(true, Ordering::Release);
    }

    fn consent(&self) {
        self.calls.lock().unwrap().push("consent".to_string());
    }

    fn save_replay(&self, ct: &CancelToken) -> Result<ReplayClip, AgentError> {
        if let Some(ref f) = *self.on_save_replay.lock().unwrap() {
            return f(ct);
        }
        let c = ReplayClip {
            id: "r1".to_string(),
            ts: Utc::now(),
            duration_ms: 120_000,
            bytes: 1000,
            encrypted: true,
            path: r"C:\x.mp4.enc".to_string(),
        };
        self.replays.lock().unwrap().push(c.clone());
        Ok(c)
    }

    fn list_replays(&self) -> Vec<ReplayClip> {
        self.replays.lock().unwrap().clone()
    }

    fn export_replay(&self, id: &str, path: &Path, ct: &CancelToken) -> Result<(), AgentError> {
        if let Some(ref f) = *self.on_export_replay.lock().unwrap() {
            return f(id, path, ct);
        }
        self.calls
            .lock()
            .unwrap()
            .push(format!("export {id} {}", path.display()));
        if self.replays.lock().unwrap().iter().any(|r| r.id == id) {
            Ok(())
        } else {
            Err(AgentError::NotFound)
        }
    }

    fn delete_replay(&self, id: &str) -> Result<(), AgentError> {
        let mut list = self.replays.lock().unwrap();
        let before = list.len();
        list.retain(|r| r.id != id);
        if list.len() == before {
            Err(AgentError::NotFound)
        } else {
            Ok(())
        }
    }

    fn panic(&self) {
        self.calls.lock().unwrap().push("panic".to_string());
    }

    fn bench(&self, _ct: &CancelToken) -> Result<BenchResult, AgentError> {
        Ok(BenchResult {
            chosen: "CPU".to_string(),
            devices: vec![BenchDevice {
                device: "CPU".to_string(),
                ep: "fake".to_string(),
                ok: true,
                image_ms: 1.0,
                text_ms: 1.0,
                cosine: 1.0,
                error: None,
                model: None,
                text_device: None,
                compile_ms: None,
                adapter: None,
                adapter_auto: false,
            }],
            tokens: None,
            chosen_adapter: None,
        })
    }

    fn bench_full(&self, ct: &CancelToken) -> Result<BenchReport, AgentError> {
        Self::set_flag(&self.full_bench_started);
        if self.block_full_bench.load(Ordering::Acquire) {
            while !ct.is_cancelled() {
                ct.wait_timeout(Duration::from_millis(50));
            }
            Self::set_flag(&self.full_bench_cancelled);
            return Err(AgentError::Cancelled);
        }
        self.full_report
            .lock()
            .unwrap()
            .clone()
            .ok_or(AgentError::NotSupported)
    }

    fn models_list(&self) -> Result<ModelsList, AgentError> {
        self.models_list_reply
            .lock()
            .unwrap()
            .clone()
            .ok_or(AgentError::NotSupported)
    }

    fn models_download(
        &self,
        target: &str,
        emit: &mut dyn FnMut(ModelsProgress),
        ct: &CancelToken,
    ) -> Result<(), AgentError> {
        // Clone the hook out so the lock is not held while it runs (concurrent downloads).
        let hook = self.on_models_download.lock().unwrap().clone();
        if let Some(f) = hook {
            return f(target, emit, ct);
        }
        Err(AgentError::NotSupported)
    }

    fn update_status(&self) -> Result<UpdateStatus, AgentError> {
        self.update_status_reply
            .lock()
            .unwrap()
            .clone()
            .ok_or(AgentError::NotSupported)
    }

    fn update_check(&self, ct: &CancelToken) -> Result<UpdateStatus, AgentError> {
        let hook = self.on_update_check.lock().unwrap().clone();
        if let Some(f) = hook {
            return f(ct);
        }
        Err(AgentError::NotSupported)
    }

    fn update_download(
        &self,
        emit: &mut dyn FnMut(UpdateProgress),
        ct: &CancelToken,
    ) -> Result<(), AgentError> {
        let hook = self.on_update_download.lock().unwrap().clone();
        if let Some(f) = hook {
            return f(emit, ct);
        }
        Err(AgentError::NotSupported)
    }

    fn update_apply(&self) -> Result<UpdateStatus, AgentError> {
        let hook = self.on_update_apply.lock().unwrap().clone();
        if let Some(f) = hook {
            return f();
        }
        Err(AgentError::NotSupported)
    }

    fn on_panicked(&self, cb: Arc<dyn Fn() + Send + Sync>) -> ListenerGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.panic_listeners.lock().unwrap().push((id, cb));
        let list = self.panic_listeners.clone();
        ListenerGuard::new(move || list.lock().unwrap().retain(|(lid, _)| *lid != id))
    }

    fn on_status_changed(&self, cb: Arc<dyn Fn(&AgentStatus) + Send + Sync>) -> ListenerGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.status_listeners.lock().unwrap().push((id, cb));
        let list = self.status_listeners.clone();
        ListenerGuard::new(move || list.lock().unwrap().retain(|(lid, _)| *lid != id))
    }

    fn on_capture_stored(&self, cb: Arc<dyn Fn(&CaptureMeta) + Send + Sync>) -> ListenerGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.capture_listeners.lock().unwrap().push((id, cb));
        let list = self.capture_listeners.clone();
        ListenerGuard::new(move || list.lock().unwrap().retain(|(lid, _)| *lid != id))
    }
}

/// Fake [`QueryEmbedder`] matching `TestSupport.cs`.
pub struct FakeEmbedder {
    pub map: Mutex<HashMap<String, Vec<f32>>>,
    pub fail: AtomicBool,
    pub warm: AtomicBool,
    pub calls: AtomicI32,
    pub delay_ms: AtomicU64,
    pub on_embed: Mutex<Option<Box<dyn Fn(&str, &CancelToken) -> Result<Vec<f32>, EmbedError> + Send + Sync>>>,
}

impl Default for FakeEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeEmbedder {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            fail: AtomicBool::new(false),
            warm: AtomicBool::new(true),
            calls: AtomicI32::new(0),
            delay_ms: AtomicU64::new(0),
            on_embed: Mutex::new(None),
        }
    }
}

impl QueryEmbedder for FakeEmbedder {
    fn embed_query(&self, text: &str, ct: &CancelToken) -> Result<Vec<f32>, EmbedError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Some(ref f) = *self.on_embed.lock().unwrap() {
            return f(text, ct);
        }
        let d = self.delay_ms.load(Ordering::Acquire);
        if d > 0 && ct.wait_timeout(Duration::from_millis(d)) {
            return Err(EmbedError::Cancelled);
        }
        if self.fail.load(Ordering::Acquire) {
            return Err(EmbedError::Failed("worker down".to_string()));
        }
        if let Some(v) = self.map.lock().unwrap().get(text).cloned() {
            return Ok(v);
        }
        let mut h = 2166136261u32;
        for b in text.as_bytes() {
            h ^= *b as u32;
            h = h.wrapping_mul(16777619);
        }
        Ok(vec_seeded(h as i32))
    }

    fn warm(&self) -> bool {
        self.warm.load(Ordering::Acquire)
    }
}

/// Fake [`Thumbnailer`] matching `TestSupport.cs`.
pub struct FakeThumbs {
    pub cleared: AtomicI32,
    pub transcodes: Mutex<Vec<String>>,
}

impl Default for FakeThumbs {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeThumbs {
    pub fn new() -> Self {
        Self {
            cleared: AtomicI32::new(0),
            transcodes: Mutex::new(Vec::new()),
        }
    }
}

impl Thumbnailer for FakeThumbs {
    fn get_thumb(&self, id: &str, width: i32, _ct: &CancelToken) -> Option<ThumbData> {
        if id == "missing" {
            None
        } else {
            Some(ThumbData {
                id: id.to_string(),
                mime: "image/webp".to_string(),
                width,
                height: width * 9 / 16,
                bytes: vec![1, 2, 3],
            })
        }
    }

    fn clear_cache(&self) {
        self.cleared.fetch_add(1, Ordering::AcqRel);
    }

    fn transcode(&self, mut keyframe: ThumbData, target: &str) -> ThumbData {
        self.transcodes
            .lock()
            .unwrap()
            .push(format!("{}>{target}", keyframe.mime));
        if target == "ui" && keyframe.mime == "image/avif" {
            keyframe.mime = "image/webp".to_string();
            keyframe.bytes = vec![9, 9];
            keyframe
        } else if target == "jpeg" {
            keyframe.mime = "image/jpeg".to_string();
            keyframe.bytes = vec![0xFF, 0xD8];
            keyframe
        } else {
            keyframe
        }
    }
}

/// Representative hardware profile used by `BenchVerbTests` (`Profiles.ThisPc()`).
pub fn profile_this_pc() -> BenchReport {
    BenchReport {
        ts: DateTime::parse_from_rfc3339("2026-11-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        took_ms: 1000,
        hardware: HardwareInfo {
            cpu: "Intel(R) Core(TM) Ultra 7 265K".to_string(),
            cores: 20,
            threads: 20,
            ram_mb: 32_768,
            has_battery: false,
            gpus: vec![GpuInfo {
                name: "Intel(R) Graphics".to_string(),
                vendor: "Intel".to_string(),
                integrated: true,
                vram_mb: 0,
                luid: "0x1".to_string(),
                driver: "32.0.101.6".to_string(),
                codecs: vec!["av1".into(), "hevc".into(), "h264".into()],
            }],
            npu: Some(NpuInfo {
                name: "Intel(R) AI Boost".to_string(),
                driver: "32.0.100.4".to_string(),
            }),
            monitors: vec![MonitorInfo {
                index: 0,
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                adapter: "Intel(R) Graphics".to_string(),
            }],
            os: "Windows 11 29704".to_string(),
        },
        inference: Some(BenchResult {
            chosen: "NPU".to_string(),
            devices: vec![BenchDevice {
                device: "NPU".to_string(),
                ep: "OpenVINO".to_string(),
                ok: true,
                image_ms: 90.0,
                text_ms: 12.0,
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
        encoders: vec![EncoderBench {
            adapter: "Intel(R) Graphics".to_string(),
            codec: "av1".to_string(),
            api: "vpl".to_string(),
            layout: "composited".to_string(),
            width: 1920,
            height: 1080,
            fps: 30,
            ok: true,
            encode_ms_p95: 8.5,
            cpu_percent: 1.5,
            working_set_mb: 90,
            error: None,
            resolution: None,
        }],
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
