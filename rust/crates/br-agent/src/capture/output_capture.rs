// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Per-output DXGI Desktop Duplication thread (`Capture\OutputCapture.cs`).

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, Utc};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Resource,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutput5, IDXGIOutputDuplication,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::core::Interface;
use zeroize::Zeroize;

use crate::capture::change_gate::ChangeGate;
use crate::capture::gpu_devices::{GpuDevice, GpuDeviceCache, GpuDevices, OutputInfo};
use crate::capture::keyframe_pipeline::KeyframePipeline;
use crate::capture::output_rotation::OutputRotation;
use crate::capture::video_scaler::{VideoScaler, VideoScalerError, VideoScalerParams};
use crate::native::{Rect, qpc, tick_count64};
use crate::util::{AgentLog, HighResTimer, RetryBackoff};

/// Keyframe request sent from [`crate::capture::CaptureService`] to one [`OutputCapture`].
#[derive(Debug, Clone)]
pub struct KeyframeRequest {
    pub ts: DateTime<Utc>,
    pub app: String,
    pub exe_path: String,
    pub title: String,
    pub context: String,
    pub force: bool,
    pub max_width: i32,
    pub dirty_min_percent: f64,
    pub site: String,
    pub keyframe: bool,
    pub replay_sample: bool,
    pub trigger: bool,
    pub secrets: Option<Vec<Rect>>,
    pub bounds: Rect,
    pub mask_only: bool,
}

/// Grabbed, downscaled BGRA keyframe. Zeroes its pixel buffer on drop.
pub struct KeyframeGrab {
    pub monitor: i32,
    pub request: KeyframeRequest,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pixels: Vec<u8>,
    pub grab_ticks: i64,
    /// Optional live counter for tests that verify grabs are zeroed and freed promptly.
    live_counter: Option<Arc<AtomicI64>>,
}

impl KeyframeGrab {
    pub fn alloc(monitor: i32, request: KeyframeRequest, w: i32, h: i32, grab_ticks: i64) -> Self {
        let stamp = if grab_ticks <= 0 {
            KeyframePipeline::next_stamp()
        } else {
            grab_ticks
        };
        let stride = w * 4;
        Self {
            monitor,
            request,
            width: w,
            height: h,
            stride,
            pixels: vec![0u8; (stride as usize) * (h as usize)],
            grab_ticks: stamp,
            live_counter: None,
        }
    }

    pub fn with_live_counter(mut self, counter: Arc<AtomicI64>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        self.live_counter = Some(counter);
        self
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }
}

impl Drop for KeyframeGrab {
    fn drop(&mut self) {
        self.pixels.zeroize();
        if let Some(ref c) = self.live_counter {
            c.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Dirty rectangle helpers (`Replay.DirtyMapper`).
pub struct DirtyMapper;

impl DirtyMapper {
    pub const fn is_empty(r: Rect) -> bool {
        r.right <= r.left || r.bottom <= r.top
    }

    pub const fn union(a: Rect, b: Rect) -> Rect {
        if Self::is_empty(a) {
            return b;
        }
        if Self::is_empty(b) {
            return a;
        }
        let left = if a.left < b.left { a.left } else { b.left };
        let top = if a.top < b.top { a.top } else { b.top };
        let right = if a.right > b.right { a.right } else { b.right };
        let bottom = if a.bottom > b.bottom {
            a.bottom
        } else {
            b.bottom
        };
        Rect::new(left, top, right, bottom)
    }
}

/// Drift-free frame pacer on QPC ticks (`Replay.FramePacer`).
pub struct FramePacer {
    fps: i32,
    frequency: i64,
    base: i64,
    index: i64,
}

impl FramePacer {
    pub fn new(fps: i32, frequency: i64) -> Self {
        Self {
            fps,
            frequency,
            base: 0,
            index: 0,
        }
    }

    pub fn base(&self) -> i64 {
        self.base
    }

    pub fn next_deadline(&self) -> i64 {
        self.base + ((self.index as i128) * (self.frequency as i128) / (self.fps as i128)) as i64
    }

    pub fn reset(&mut self, now: i64) {
        self.base = now;
        self.index = 0;
    }

    pub fn try_tick(&mut self, now: i64) -> Option<i64> {
        if self.base == 0 {
            self.reset(now);
        }
        let next = self.next_deadline();
        if now < next {
            return None;
        }
        if now - next > 4 * self.frequency / self.fps as i64 {
            self.reset(now);
        }
        let pts = Self::to_hundred_ns(self.base, self.frequency)
            + (((self.index as i128) * 20_000_000 + (self.fps as i128)) / (2 * self.fps as i128))
                as i64;
        self.index += 1;
        Some(pts)
    }

    pub fn to_hundred_ns(ticks: i64, frequency: i64) -> i64 {
        ((ticks as i128) * 10_000_000 / (frequency as i128)) as i64
    }
}

type GrabCallback = Arc<dyn Fn(KeyframeGrab) + Send + Sync>;

struct SharedCapture {
    info: OutputInfo,
    devices: Arc<GpuDeviceCache>,
    on_grab: GrabCallback,
    stop: AtomicBool,
    pending_keyframe: Mutex<Option<KeyframeRequest>>,
    retry_keyframe: AtomicBool,
    replay_poll_fps: AtomicI32,
    grab_override: Mutex<Option<GrabCallback>>,
    state: Mutex<String>,
    feed_dirty: Mutex<Rect>,
    dev_shared: Mutex<Option<Arc<GpuDevice>>>,
    latest_shared: Mutex<Option<ID3D11Texture2D>>,
    src_w: AtomicI32,
    src_h: AtomicI32,
    rotation: AtomicI32,
    feed_generation: AtomicI32,
    disposed: AtomicBool,
    acquire_count: AtomicU64,
    frame_count: AtomicU64,
    access_lost: AtomicU64,
    grab_ticks: AtomicU64,
    grab_count: AtomicU64,
    skipped_no_change: AtomicU64,
}

unsafe impl Send for SharedCapture {}
unsafe impl Sync for SharedCapture {}

/// Desktop Duplication of one output (`OutputCapture.cs`).
pub struct OutputCapture {
    shared: Arc<SharedCapture>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl OutputCapture {
    pub const BARREN_MS: i64 = 5000;
    pub const BARREN_MAX_MS: i64 = 15_000;

    pub fn new(info: OutputInfo, devices: Arc<GpuDeviceCache>, on_grab: GrabCallback) -> Self {
        Self {
            shared: Arc::new(SharedCapture {
                info,
                devices,
                on_grab,
                stop: AtomicBool::new(false),
                pending_keyframe: Mutex::new(None),
                retry_keyframe: AtomicBool::new(false),
                replay_poll_fps: AtomicI32::new(0),
                grab_override: Mutex::new(None),
                state: Mutex::new("starting".to_string()),
                feed_dirty: Mutex::new(Rect::default()),
                dev_shared: Mutex::new(None),
                latest_shared: Mutex::new(None),
                src_w: AtomicI32::new(0),
                src_h: AtomicI32::new(0),
                rotation: AtomicI32::new(0),
                feed_generation: AtomicI32::new(0),
                disposed: AtomicBool::new(false),
                acquire_count: AtomicU64::new(0),
                frame_count: AtomicU64::new(0),
                access_lost: AtomicU64::new(0),
                grab_ticks: AtomicU64::new(0),
                grab_count: AtomicU64::new(0),
                skipped_no_change: AtomicU64::new(0),
            }),
            thread: Mutex::new(None),
        }
    }

    pub fn info(&self) -> &OutputInfo {
        &self.shared.info
    }

    pub fn start(&self) {
        let mut slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let shared = self.shared.clone();
        let name = format!("capture-{}", shared.info.index);
        *slot = thread::Builder::new()
            .name(name)
            .spawn(move || run_capture(shared))
            .ok();
    }

    pub fn set_pending_keyframe(&self, req: Option<KeyframeRequest>) {
        *self
            .shared
            .pending_keyframe
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = req;
    }

    pub fn pending_keyframe(&self) -> Option<KeyframeRequest> {
        self.shared
            .pending_keyframe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// TH1: next request grabs again even when no new desktop frame arrived.
    pub fn retry_keyframe(&self) {
        self.shared.retry_keyframe.store(true, Ordering::Release);
    }

    pub fn is_retry_keyframe_set(&self) -> bool {
        self.shared.retry_keyframe.load(Ordering::Acquire)
    }

    pub fn set_replay_poll_fps(&self, fps: i32) {
        self.shared.replay_poll_fps.store(fps, Ordering::Release);
    }

    pub fn replay_poll_fps(&self) -> i32 {
        self.shared.replay_poll_fps.load(Ordering::Acquire)
    }

    pub fn device(&self) -> Option<Arc<GpuDevice>> {
        self.shared
            .dev_shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn latest(&self) -> Option<ID3D11Texture2D> {
        self.shared
            .latest_shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn disposed(&self) -> bool {
        self.shared.disposed.load(Ordering::Acquire)
    }

    pub fn state(&self) -> String {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn frame_count(&self) -> u64 {
        self.shared.frame_count.load(Ordering::Acquire)
    }

    pub fn access_lost(&self) -> u64 {
        self.shared.access_lost.load(Ordering::Acquire)
    }

    pub fn grab_count(&self) -> u64 {
        self.shared.grab_count.load(Ordering::Acquire)
    }

    pub fn skipped_no_change(&self) -> u64 {
        self.shared.skipped_no_change.load(Ordering::Acquire)
    }

    pub fn source_width(&self) -> i32 {
        self.shared.src_w.load(Ordering::Acquire)
    }

    pub fn source_height(&self) -> i32 {
        self.shared.src_h.load(Ordering::Acquire)
    }

    pub fn rotation(&self) -> i32 {
        self.shared.rotation.load(Ordering::Acquire)
    }

    pub fn feed_generation(&self) -> i32 {
        self.shared.feed_generation.load(Ordering::Acquire)
    }

    pub fn take_feed_dirty(&self) -> Rect {
        let mut g = self
            .shared
            .feed_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *g)
    }

    pub fn mark_feed_full(&self) {
        *self
            .shared
            .feed_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            Rect::new(0, 0, self.source_width(), self.source_height());
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = h.join();
        }
        self.shared.disposed.store(true, Ordering::Release);
    }

    /// True if frame textures made for `old` sizes and rotation serve `now`. Pure (T3).
    pub const fn same_frames(
        have_frames: bool,
        old: (i32, i32, i32, i32, i32),
        now: (i32, i32, i32, i32, i32),
    ) -> bool {
        have_frames
            && old.0 == now.0
            && old.1 == now.1
            && old.2 == now.2
            && old.3 == now.3
            && old.4 == now.4
    }

    /// A duplication lost before delivering a frame and within `BARREN_MS` of coming up. Pure.
    pub const fn barren(frames_since_up: i64, lived_ms: i64) -> bool {
        frames_since_up == 0 && lived_ms < Self::BARREN_MS
    }

    /// A new duplication may be tried: 1 s after `last_try` and past `barren_next_at`. Pure.
    pub const fn retry_due(now: i64, last_try: i64, barren_next_at: i64) -> bool {
        now - last_try >= 1000 && now >= barren_next_at
    }

    pub fn qpc_100ns(qpc_ticks: i64) -> i64 {
        FramePacer::to_hundred_ns(qpc_ticks, qpc_frequency())
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn qpc_frequency() -> i64 {
    let mut f = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1)
}

struct PendingGrab {
    req: KeyframeRequest,
    w: i32,
    h: i32,
    stamp: i64,
}

struct CaptureWorker {
    shared: Arc<SharedCapture>,
    timer: HighResTimer,
    dev: Option<Arc<GpuDevice>>,
    dup: Option<IDXGIOutputDuplication>,
    latest: Option<ID3D11Texture2D>,
    src_w: i32,
    src_h: i32,
    tex_w: i32,
    tex_h: i32,
    rotation: i32,
    rotation_blocked: bool,
    raw: Option<ID3D11Texture2D>,
    rotator: Option<VideoScaler>,
    kf_scaler: Option<VideoScaler>,
    kf_scaler_dev: Option<Arc<GpuDevice>>,
    kf_scaler_width: i32,
    kf_scaler_src_w: i32,
    kf_scaler_src_h: i32,
    dirty_buf: Vec<RECT>,
    move_buf: Vec<DXGI_OUTDUPL_MOVE_RECT>,
    last_retry: i64,
    keyframe_gate: ChangeGate,
    sample_gate: ChangeGate,
    poll_pacer: Option<FramePacer>,
    last_poll_fps: i32,
    pending_grab: Option<PendingGrab>,
    resets: i64,
    reset_logged_at: i64,
    up_at: i64,
    frames_at_up: u64,
    lost_why: Option<String>,
    barren: RetryBackoff,
}

impl CaptureWorker {
    fn new(shared: Arc<SharedCapture>) -> Self {
        Self {
            shared,
            timer: HighResTimer::new(),
            dev: None,
            dup: None,
            latest: None,
            src_w: 0,
            src_h: 0,
            tex_w: 0,
            tex_h: 0,
            rotation: 0,
            rotation_blocked: false,
            raw: None,
            rotator: None,
            kf_scaler: None,
            kf_scaler_dev: None,
            kf_scaler_width: 0,
            kf_scaler_src_w: 0,
            kf_scaler_src_h: 0,
            dirty_buf: vec![RECT::default(); 64],
            move_buf: vec![DXGI_OUTDUPL_MOVE_RECT::default(); 16],
            last_retry: 0,
            keyframe_gate: ChangeGate::new(),
            sample_gate: ChangeGate::new(),
            poll_pacer: None,
            last_poll_fps: 0,
            pending_grab: None,
            resets: 0,
            reset_logged_at: 0,
            up_at: 0,
            frames_at_up: 0,
            lost_why: None,
            barren: RetryBackoff::new(1000, OutputCapture::BARREN_MAX_MS),
        }
    }

    fn set_state(&self, s: &str) {
        *self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = s.to_string();
    }

    fn run(&mut self) {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
        }
        let freq = qpc_frequency();
        while !self.shared.stop.load(Ordering::Acquire) {
            if self.dup.is_none() && !self.try_create_duplication() {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            let poll_fps = self.shared.replay_poll_fps.load(Ordering::Acquire);
            if poll_fps > 0 {
                if self.poll_pacer.is_none() || self.last_poll_fps != poll_fps {
                    self.poll_pacer = Some(FramePacer::new(poll_fps, freq));
                    self.last_poll_fps = poll_fps;
                }
                let now = qpc();
                let mut deadline = self
                    .poll_pacer
                    .as_ref()
                    .map_or(now, |p| if p.base() == 0 { now } else { p.next_deadline() });
                if self.pending_grab.is_some() {
                    deadline = deadline.min(now + freq / 30);
                }
                if deadline > now {
                    self.timer
                        .sleep(((deadline - now) as i128 * 10_000_000 / freq as i128) as i64);
                }
                if self.pending_grab.is_some() {
                    self.finish_grab();
                }
                if let Err(e) = self.acquire(0) {
                    self.handle_error(&e);
                    continue;
                }
                if let Some(ref mut p) = self.poll_pacer {
                    let _ = p.try_tick(qpc());
                }
            } else {
                self.poll_pacer = None;
                self.last_poll_fps = 0;
                let now = qpc();
                let has_pending = self
                    .shared
                    .pending_keyframe
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                let mut deadline = if has_pending { now } else { now + freq / 4 };
                if self.pending_grab.is_some() {
                    deadline = deadline.min(now + freq / 30);
                }
                if deadline > now {
                    self.timer
                        .sleep(((deadline - now) as i128 * 10_000_000 / freq as i128) as i64);
                }
                if self.pending_grab.is_some() {
                    self.finish_grab();
                }
                if let Err(e) = self.acquire(0) {
                    self.handle_error(&e);
                    continue;
                }
            }

            let req = self
                .shared
                .pending_keyframe
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(req) = req
                && let Err(e) = self.grab(req)
            {
                self.handle_error(&e);
            }
        }
        self.reset_duplication("stop");
        self.dispose_frame_resources();
    }

    fn handle_error(&mut self, msg: &str) {
        AgentLog::warn(&format!("capture {}: {msg}", self.shared.info.index));
        self.reset_duplication("error");
        self.drop_device_if_removed();
        thread::sleep(Duration::from_millis(500));
    }

    fn try_create_duplication(&mut self) -> bool {
        let now = tick_count64();
        if !OutputCapture::retry_due(now, self.last_retry, self.barren.next_at()) {
            return false;
        }
        self.last_retry = now;

        if let Some(ref held) = self.dev
            && !self.shared.devices.is_current(held)
        {
            self.drop_device();
        }
        let dev = match self.dev.clone() {
            Some(d) => d,
            None => match self.shared.devices.get(self.shared.info.adapter_luid) {
                Ok(d) => {
                    self.dev = Some(d.clone());
                    *self
                        .shared
                        .dev_shared
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(d.clone());
                    d
                }
                Err(e) => {
                    self.set_state("unavailable");
                    AgentLog::debug(&format!(
                        "duplication output {} failed: {e}",
                        self.shared.info.index
                    ));
                    return false;
                }
            },
        };

        let res = (|| -> Result<IDXGIOutputDuplication, String> {
            let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }
                .map_err(|e| format!("0x{:08X}", e.code().0 as u32))?;
            let adapter = GpuDevices::find_adapter(&factory, self.shared.info.adapter_luid)
                .ok_or_else(|| "adapter gone".to_string())?;
            let mut found: Option<IDXGIOutput> = None;
            let mut i = 0u32;
            while let Ok(o) = unsafe { adapter.EnumOutputs(i) } {
                i += 1;
                if let Ok(od) = unsafe { o.GetDesc() } {
                    let len = od
                        .DeviceName
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(od.DeviceName.len());
                    let name = String::from_utf16_lossy(&od.DeviceName[..len]);
                    if name == self.shared.info.device_name {
                        found = Some(o);
                        break;
                    }
                }
            }
            let output = found.ok_or_else(|| "output gone".to_string())?;
            let dup = if let Ok(o5) = output.cast::<IDXGIOutput5>() {
                unsafe { o5.DuplicateOutput1(&dev.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) }
            } else {
                let o1: IDXGIOutput1 = output
                    .cast()
                    .map_err(|e| format!("0x{:08X}", e.code().0 as u32))?;
                unsafe { o1.DuplicateOutput(&dev.device) }
            }
            .map_err(|e| format!("0x{:08X}", e.code().0 as u32))?;

            let desc = unsafe { dup.GetDesc() };
            let desk = unsafe { output.GetDesc() }
                .map_err(|e| format!("0x{:08X}", e.code().0 as u32))?
                .DesktopCoordinates;
            let rotation = OutputRotation::degrees(desc.Rotation);
            let (tex_w, tex_h) = OutputRotation::texture_size_for(
                desc.ModeDesc.Width as i32,
                desc.ModeDesc.Height as i32,
                desk.right - desk.left,
                desk.bottom - desk.top,
                rotation,
            );
            let (desk_w, desk_h) = OutputRotation::desktop_size(tex_w, tex_h, rotation);
            self.setup_frames(&dev, tex_w, tex_h, desk_w, desk_h, rotation)?;
            Ok(dup)
        })();

        match res {
            Ok(dup) => {
                self.dup = Some(dup);
                self.keyframe_gate.invalidate();
                self.sample_gate.invalidate();
                self.set_state(if self.rotation_blocked {
                    "rotation unsupported"
                } else {
                    "running"
                });
                self.up_at = tick_count64();
                self.frames_at_up = self.shared.frame_count.load(Ordering::Acquire);
                let rot_note = if self.rotation != 0 {
                    format!(", rotated {}", self.rotation)
                } else {
                    String::new()
                };
                let lost_note = match self.lost_why.take() {
                    Some(w) => format!(", again after {w} #{}", self.resets),
                    None => String::new(),
                };
                AgentLog::info(&format!(
                    "duplication up on output {} ({}x{}{rot_note}, vendor 0x{:04X}{lost_note})",
                    self.shared.info.index, self.src_w, self.src_h, self.shared.info.vendor_id
                ));
                if self.rotation_blocked {
                    AgentLog::warn(&format!(
                        "output {} is rotated {} degrees and the GPU's video processor cannot rotate: it is recorded black and gets no keyframes",
                        self.shared.info.index, self.rotation
                    ));
                }
                true
            }
            Err(e) => {
                self.set_state("unavailable");
                AgentLog::debug(&format!(
                    "duplication output {} failed: {e}",
                    self.shared.info.index
                ));
                self.drop_device_if_removed();
                false
            }
        }
    }

    fn setup_frames(
        &mut self,
        dev: &Arc<GpuDevice>,
        tex_w: i32,
        tex_h: i32,
        desk_w: i32,
        desk_h: i32,
        rotation: i32,
    ) -> Result<(), String> {
        let _gate = dev.gate.lock().unwrap_or_else(|e| e.into_inner());
        if OutputCapture::same_frames(
            self.latest.is_some(),
            (self.tex_w, self.tex_h, self.src_w, self.src_h, self.rotation),
            (tex_w, tex_h, desk_w, desk_h, rotation),
        ) {
            *self
                .shared
                .feed_dirty
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Rect::new(0, 0, self.src_w, self.src_h);
            return Ok(());
        }
        self.dispose_frame_resources();
        self.tex_w = tex_w;
        self.tex_h = tex_h;
        self.src_w = desk_w;
        self.src_h = desk_h;
        self.rotation = rotation;
        self.rotation_blocked = false;
        self.shared.src_w.store(desk_w, Ordering::Release);
        self.shared.src_h.store(desk_h, Ordering::Release);
        self.shared.rotation.store(rotation, Ordering::Release);

        let desc = D3D11_TEXTURE2D_DESC {
            Width: desk_w as u32,
            Height: desk_h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut latest = None;
        unsafe { dev.device.CreateTexture2D(&desc, None, Some(&mut latest)) }
            .map_err(|e| format!("CreateTexture2D latest: {e}"))?;
        let latest = latest.ok_or_else(|| "latest texture null".to_string())?;

        if rotation != 0 {
            match VideoScaler::new(
                dev.clone(),
                VideoScalerParams {
                    src_w: tex_w,
                    src_h: tex_h,
                    dst_w: desk_w,
                    dst_h: desk_h,
                    dst_format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    with_staging: false,
                    rotation_degrees: rotation,
                },
            ) {
                Ok(rotator) => {
                    let raw_desc = D3D11_TEXTURE2D_DESC {
                        Width: tex_w as u32,
                        Height: tex_h as u32,
                        ..desc
                    };
                    let mut raw = None;
                    unsafe { dev.device.CreateTexture2D(&raw_desc, None, Some(&mut raw)) }
                        .map_err(|e| format!("CreateTexture2D raw: {e}"))?;
                    self.rotator = Some(rotator);
                    self.raw = raw;
                }
                Err(VideoScalerError::RotationUnsupported) => {
                    self.rotation_blocked = true;
                    clear_texture_black(dev, &latest);
                }
                Err(VideoScalerError::Failed(e)) => return Err(e),
            }
        }
        self.latest = Some(latest.clone());
        *self
            .shared
            .latest_shared
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(latest);
        self.shared.feed_generation.fetch_add(1, Ordering::AcqRel);
        *self
            .shared
            .feed_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Rect::new(0, 0, self.src_w, self.src_h);
        Ok(())
    }

    fn dispose_frame_resources(&mut self) {
        self.pending_grab = None;
        self.kf_scaler = None;
        self.kf_scaler_dev = None;
        self.rotator = None;
        self.raw = None;
        self.latest = None;
        *self
            .shared
            .latest_shared
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn drop_device_if_removed(&mut self) {
        if let Some(ref d) = self.dev
            && self.shared.devices.invalidate_if_removed(d)
        {
            self.drop_device();
        }
    }

    fn drop_device(&mut self) {
        if let Some(d) = self.dev.take() {
            let _g = d.gate.lock().unwrap_or_else(|e| e.into_inner());
            self.dispose_frame_resources();
            *self
                .shared
                .dev_shared
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    fn reset_duplication(&mut self, why: &str) {
        if self.dup.take().is_none() {
            return;
        }
        self.set_state(why);
        if why == "stop" {
            return;
        }
        self.resets += 1;
        self.lost_why = Some(why.to_string());
        let now = tick_count64();
        let lived = now - self.up_at;
        let frames = self
            .shared
            .frame_count
            .load(Ordering::Acquire)
            .saturating_sub(self.frames_at_up) as i64;
        let delay = if OutputCapture::barren(frames, lived) {
            self.barren.failed(now)
        } else {
            self.barren.succeeded();
            0
        };
        if self.resets == 1 || now - self.reset_logged_at >= 60_000 || self.barren.failures() == 2 {
            self.reset_logged_at = now;
            let suffix = if self.barren.failures() > 1 {
                format!(
                    ", {} before a frame in a row, next try in {} s)",
                    self.barren.failures(),
                    delay / 1000
                )
            } else {
                ")".to_string()
            };
            AgentLog::info(&format!(
                "duplication on output {} lost: {why} after {lived} ms, {frames} frames ({} so far{suffix}",
                self.shared.info.index, self.resets
            ));
        }
    }

    fn acquire(&mut self, timeout_ms: u32) -> Result<(), String> {
        let Some(dup) = self.dup.clone() else {
            return Ok(());
        };
        self.shared.acquire_count.fetch_add(1, Ordering::Relaxed);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        let r = unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) };
        if let Err(e) = r {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(());
            }
            if e.code() == DXGI_ERROR_ACCESS_LOST {
                self.shared.access_lost.fetch_add(1, Ordering::Relaxed);
                self.reset_duplication("access lost");
                return Ok(());
            }
            return Err(format!("AcquireNextFrame: {e}"));
        }

        let process_res = (|| -> Result<(), String> {
            if info.LastPresentTime == 0 || info.AccumulatedFrames == 0 {
                return Ok(()); // pointer-only update
            }
            self.shared.frame_count.fetch_add(1, Ordering::Relaxed);
            let res = resource.ok_or_else(|| "AcquireNextFrame resource null".to_string())?;
            let tex: ID3D11Texture2D = res
                .cast()
                .map_err(|e| format!("AcquireNextFrame texture cast: {e}"))?;
            let Some(dev) = self.dev.clone() else {
                return Ok(());
            };
            let _g = dev.gate.lock().unwrap_or_else(|e| e.into_inner());
            if self.rotation_blocked {
                return Ok(());
            }
            let Some(latest) = self.latest.clone() else {
                return Ok(());
            };
            if let Some(ref mut rotator) = self.rotator {
                let mut td = D3D11_TEXTURE2D_DESC::default();
                unsafe { tex.GetDesc(&mut td) };
                if td.Width != self.tex_w as u32 || td.Height != self.tex_h as u32 {
                    self.rotation_blocked = true;
                    clear_texture_black(&dev, &latest);
                    AgentLog::warn(&format!(
                        "output {}: rotated frame is {}x{}, expected {}x{}; recorded black",
                        self.shared.info.index, td.Width, td.Height, self.tex_w, self.tex_h
                    ));
                    return Ok(());
                }
                if let Some(ref raw) = self.raw {
                    let raw_res: ID3D11Resource = raw.cast().map_err(|e| e.to_string())?;
                    let tex_res: ID3D11Resource = tex.cast().map_err(|e| e.to_string())?;
                    unsafe { dev.context.CopyResource(&raw_res, &tex_res) };
                    rotator.run(raw, false)?;
                    let latest_res: ID3D11Resource = latest.cast().map_err(|e| e.to_string())?;
                    let out_res: ID3D11Resource =
                        rotator.output.cast().map_err(|e| e.to_string())?;
                    unsafe { dev.context.CopyResource(&latest_res, &out_res) };
                }
            } else {
                let latest_res: ID3D11Resource = latest.cast().map_err(|e| e.to_string())?;
                let tex_res: ID3D11Resource = tex.cast().map_err(|e| e.to_string())?;
                unsafe { dev.context.CopyResource(&latest_res, &tex_res) };
            }

            let (dirty_rect, dirty_pct) = self.process_changes(&dup, info.TotalMetadataBufferSize);
            {
                let mut fd = self
                    .shared
                    .feed_dirty
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *fd = DirtyMapper::union(*fd, dirty_rect);
            }
            self.keyframe_gate.frame(dirty_pct);
            self.sample_gate.frame(dirty_pct);
            Ok(())
        })();

        let _ = unsafe { dup.ReleaseFrame() };
        process_res
    }

    fn process_changes(&mut self, dup: &IDXGIOutputDuplication, metadata_size: u32) -> (Rect, f64) {
        if metadata_size == 0 {
            return (Rect::new(0, 0, self.src_w, self.src_h), 100.0);
        }
        let rect_sz = std::mem::size_of::<RECT>();
        let dirty_count = ((metadata_size as usize) / rect_sz).max(1);
        if self.dirty_buf.len() < dirty_count {
            let next = dirty_count.max(self.dirty_buf.len() * 2);
            self.dirty_buf.resize(next, RECT::default());
        }
        let mut dirty_req = 0u32;
        if unsafe {
            dup.GetFrameDirtyRects(
                (self.dirty_buf.len() * rect_sz) as u32,
                self.dirty_buf.as_mut_ptr(),
                &mut dirty_req,
            )
        }
        .is_err()
        {
            return (Rect::new(0, 0, self.src_w, self.src_h), 100.0);
        }

        let move_sz = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        let move_count = ((metadata_size as usize) / move_sz).max(1);
        if self.move_buf.len() < move_count {
            let next = move_count.max(self.move_buf.len() * 2);
            self.move_buf.resize(next, DXGI_OUTDUPL_MOVE_RECT::default());
        }
        let mut move_req = 0u32;
        if unsafe {
            dup.GetFrameMoveRects(
                (self.move_buf.len() * move_sz) as u32,
                self.move_buf.as_mut_ptr(),
                &mut move_req,
            )
        }
        .is_err()
        {
            return (Rect::new(0, 0, self.src_w, self.src_h), 100.0);
        }

        let dirty_n = (dirty_req as usize) / rect_sz;
        let move_n = (move_req as usize) / move_sz;

        let mut union_rect = Rect::default();
        let mut dirty_area = 0i64;
        for dr in &self.dirty_buf[..dirty_n] {
            dirty_area += (dr.right - dr.left) as i64 * (dr.bottom - dr.top) as i64;
            union_rect = DirtyMapper::union(union_rect, Rect::from_win32(*dr));
        }
        for mr in &self.move_buf[..move_n] {
            union_rect = DirtyMapper::union(union_rect, Rect::from_win32(mr.DestinationRect));
        }
        let pct = 100.0 * dirty_area as f64 / ((self.src_w as i64) * (self.src_h as i64)).max(1) as f64;
        let dirty_pct = if dirty_n == 0 && move_n == 0 { 1.0 } else { pct };
        if self.rotation != 0 && !DirtyMapper::is_empty(union_rect) {
            union_rect =
                OutputRotation::texture_to_desktop(union_rect, self.tex_w, self.tex_h, self.rotation);
        }
        (union_rect, dirty_pct)
    }

    fn grab(&mut self, req: KeyframeRequest) -> Result<(), String> {
        let Some(latest) = self.latest.clone() else {
            return Ok(());
        };
        let Some(dev) = self.dev.clone() else {
            return Ok(());
        };
        if self.rotation_blocked {
            return Ok(());
        }
        if self.pending_grab.is_some() {
            self.finish_grab();
        }
        let stamp = KeyframePipeline::next_stamp();
        if self.shared.retry_keyframe.swap(false, Ordering::AcqRel) {
            self.keyframe_gate.invalidate();
            self.sample_gate.invalidate();
        }
        let changed = (req.keyframe && self.keyframe_gate.changed(req.dirty_min_percent))
            || (req.replay_sample && self.sample_gate.changed(req.dirty_min_percent));
        if !req.force && !changed {
            self.shared.skipped_no_change.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let t0 = qpc();
        let w = req.max_width.min(self.src_w) & !1;
        let h = ((((self.src_h as i64) * (w as i64) / (self.src_w as i64).max(1)) as i32) & !1)
            .max(2);
        {
            let _g = dev.gate.lock().unwrap_or_else(|e| e.into_inner());
            let need_new = self.kf_scaler.is_none()
                || self.kf_scaler_width != w
                || self.kf_scaler.as_ref().is_some_and(|s| s.height != h)
                || self
                    .kf_scaler_dev
                    .as_ref()
                    .is_none_or(|d| !Arc::ptr_eq(d, &dev))
                || self.kf_scaler_src_w != self.src_w
                || self.kf_scaler_src_h != self.src_h;
            if need_new {
                self.kf_scaler = None;
                let scaler = VideoScaler::new(
                    dev.clone(),
                    VideoScalerParams {
                        src_w: self.src_w,
                        src_h: self.src_h,
                        dst_w: w,
                        dst_h: h,
                        dst_format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        with_staging: true,
                        rotation_degrees: 0,
                    },
                )
                .map_err(|e| e.to_string())?;
                self.kf_scaler = Some(scaler);
                self.kf_scaler_width = w;
                self.kf_scaler_dev = Some(dev.clone());
                self.kf_scaler_src_w = self.src_w;
                self.kf_scaler_src_h = self.src_h;
            }
            if let Some(ref mut scaler) = self.kf_scaler {
                scaler.run(&latest, true)?;
            }
        }
        if req.keyframe || req.force {
            self.keyframe_gate.reset();
        }
        if req.replay_sample || req.force {
            self.sample_gate.reset();
        }
        self.pending_grab = Some(PendingGrab { req, w, h, stamp });
        self.shared
            .grab_ticks
            .fetch_add((qpc() - t0).max(0) as u64, Ordering::Relaxed);
        Ok(())
    }

    fn finish_grab(&mut self) {
        let Some(pg) = self.pending_grab.take() else {
            return;
        };
        let Some(ref dev) = self.dev else {
            return;
        };
        if self
            .kf_scaler_dev
            .as_ref()
            .is_none_or(|d| !Arc::ptr_eq(d, dev))
        {
            return;
        }
        let Some(staging) = self.kf_scaler.as_ref().and_then(|s| s.staging.clone()) else {
            return;
        };
        let t0 = qpc();
        let mut grab = KeyframeGrab::alloc(self.shared.info.index, pg.req, pg.w, pg.h, pg.stamp);
        let mapped_ok = {
            let _g = dev.gate.lock().unwrap_or_else(|e| e.into_inner());
            let Ok(st_res) = staging.cast::<ID3D11Resource>() else {
                return;
            };
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            if unsafe { dev.context.Map(&st_res, 0, D3D11_MAP_READ, 0, Some(&mut m)) }.is_err() {
                return;
            }
            let src_ptr = m.pData as *const u8;
            let row_pitch = m.RowPitch as usize;
            let row_bytes = (pg.w as usize) * 4;
            let dst_stride = grab.stride as usize;
            let dst = grab.pixels_mut();
            for y in 0..(pg.h as usize) {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src_ptr.add(y * row_pitch),
                        dst.as_mut_ptr().add(y * dst_stride),
                        row_bytes,
                    );
                }
            }
            unsafe { dev.context.Unmap(&st_res, 0) };
            true
        };
        if !mapped_ok {
            return;
        }
        self.shared
            .grab_ticks
            .fetch_add((qpc() - t0).max(0) as u64, Ordering::Relaxed);
        self.shared.grab_count.fetch_add(1, Ordering::Relaxed);
        let deliver = self
            .shared
            .grab_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = deliver {
            cb(grab);
        } else {
            (self.shared.on_grab)(grab);
        }
    }
}

fn run_capture(shared: Arc<SharedCapture>) {
    let mut worker = CaptureWorker::new(shared);
    worker.run();
}

fn clear_texture_black(dev: &GpuDevice, tex: &ID3D11Texture2D) {
    if let Ok(res) = tex.cast::<ID3D11Resource>() {
        let mut rtv = None;
        if unsafe { dev.device.CreateRenderTargetView(&res, None, Some(&mut rtv)) }.is_ok()
            && let Some(rtv) = rtv
        {
            unsafe {
                dev.context
                    .ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            }
        }
    }
}
