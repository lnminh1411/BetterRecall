// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Intel AV1 hardware encoder through oneVPL (`libvpl.dll`) (`Replay\VplAv1Encoder.cs`).

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, ID3D11Resource,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::core::Interface;
use zeroize::Zeroize;

use crate::capture::gpu_devices::GpuDevice;
use crate::codecs::{MfxVariant, Vpl, VplApi};
use crate::native::qpc;
use crate::replay::bitstream::VideoCodec;
use crate::replay::video_encoders::{CodecBits, EncoderCreateParams, LatencyStats, PacketSink, VideoEncoder};
use crate::util::AgentLog;

struct VplSlot {
    bs: Box<[u8]>,
    ctrl: Box<[u8]>,
    data: Box<[u8]>,
    sync: usize, // *mut c_void
}

struct SyncQueue {
    pending: VecDeque<VplSlot>,
    closed: bool,
}

struct VplShared {
    api: &'static VplApi,
    dev: Arc<GpuDevice>,
    sink: PacketSink,
    loader: Mutex<usize>,  // *mut c_void
    session: Mutex<usize>, // *mut c_void
    video_memory: bool,
    name: String,
    width: i32,
    height: i32,
    surface_width: i32,
    surface_height: i32,
    free: Mutex<VecDeque<VplSlot>>,
    pending: Mutex<SyncQueue>,
    pending_cv: Condvar,
    order: Mutex<VecDeque<(i64, i64)>>,
    surface_textures: Mutex<HashMap<usize, ID3D11Texture2D>>,
    submit_gate: Mutex<()>,
    stopping: AtomicBool,
    frames_in: AtomicI64,
    packets_out: AtomicI64,
    frames_dropped: AtomicI64,
    forced_keyframes: AtomicI64,
    error: Mutex<Option<String>>,
    latency: LatencyStats,
}

unsafe impl Send for VplShared {}
unsafe impl Sync for VplShared {}

/// AV1 hardware encoder on Intel through oneVPL (`VplAv1Encoder`).
pub struct VplAv1Encoder {
    shared: Arc<VplShared>,
    sync_thread: Mutex<Option<JoinHandle<()>>>,
    disposed: AtomicBool,
}

impl VplAv1Encoder {
    const DEPTH: usize = 3;

    /// True when `libvpl.dll` loads and a hardware implementation with an AV1 encoder exists on `luid`.
    pub fn probe(luid: i64) -> bool {
        let Ok(api) = Vpl::api() else {
            return false;
        };
        let loader = unsafe { open_loader(api) };
        if loader.is_null() {
            return false;
        }
        let (idx, _) = unsafe { Vpl::find_implementation(api, loader, luid) };
        unsafe {
            (api.mfx_unload)(loader);
        }
        idx >= 0
    }

    pub fn create(
        dev: Arc<GpuDevice>,
        params: EncoderCreateParams,
        sink: PacketSink,
    ) -> Result<Self, String> {
        let api = Vpl::api()?;
        let mut last_err = "oneVPL AV1 init failed".to_string();
        for video in [true, false] {
            for low_power in [true, false] {
                match try_create(api, dev.clone(), params, sink.clone(), video, low_power) {
                    Ok(enc) => return Ok(enc),
                    Err((e, retry_sysmem)) => {
                        last_err = e;
                        if video && retry_sysmem {
                            break;
                        }
                    }
                }
            }
        }
        Err(last_err)
    }
}

impl VideoEncoder for VplAv1Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::Av1
    }

    fn api(&self) -> &str {
        "vpl"
    }

    fn name(&self) -> &str {
        &self.shared.name
    }

    fn width(&self) -> i32 {
        self.shared.width
    }

    fn height(&self) -> i32 {
        self.shared.height
    }

    fn surface_width(&self) -> i32 {
        self.shared.surface_width
    }

    fn surface_height(&self) -> i32 {
        self.shared.surface_height
    }

    fn copies_input(&self) -> bool {
        true
    }

    fn ready_for_input(&self) -> bool {
        !self.shared.stopping.load(Ordering::Acquire)
            && !self
                .shared
                .free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }

    fn try_submit(&self, nv12: &ID3D11Texture2D, pts_100ns: i64, force_key: bool) -> bool {
        if self.shared.stopping.load(Ordering::Acquire) {
            return false;
        }
        let _gate = self
            .shared
            .submit_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = *self.shared.session.lock().unwrap_or_else(|e| e.into_inner()) as *mut c_void;
        if session.is_null() {
            return false;
        }
        let Some(mut slot) = self
            .shared
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        else {
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };

        let mut surf: *mut u8 = std::ptr::null_mut();
        let st = unsafe { (self.shared.api.mfx_memory_get_surface_for_encode)(session, &mut surf) };
        if st < 0 || surf.is_null() {
            self.shared
                .free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(slot);
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
            *self.shared.error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(format!("GetSurfaceForEncode {st}"));
            return false;
        }

        let res = (|| -> bool {
            if !fill_surface(&self.shared, surf, nv12) {
                self.shared
                    .free
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(slot);
                self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            slot.ctrl.fill(0);
            if force_key {
                unsafe {
                    Vpl::w16(
                        slot.ctrl.as_mut_ptr(),
                        Vpl::CTRL_FRAME_TYPE,
                        Vpl::FRAME_TYPE_I | Vpl::FRAME_TYPE_IDR | Vpl::FRAME_TYPE_REF,
                    );
                }
                self.shared.forced_keyframes.fetch_add(1, Ordering::Relaxed);
            }
            unsafe {
                Vpl::w32(slot.bs.as_mut_ptr(), Vpl::BS_DATA_LENGTH, 0);
                Vpl::w32(slot.bs.as_mut_ptr(), Vpl::BS_DATA_OFFSET, 0);
            }
            let mut sync: *mut c_void = std::ptr::null_mut();
            self.shared
                .order
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((pts_100ns, qpc()));
            let ctrl_ptr = if force_key {
                slot.ctrl.as_mut_ptr()
            } else {
                std::ptr::null_mut()
            };
            let st = unsafe {
                (self.shared.api.mfx_video_encode_encode_frame_async)(
                    session,
                    ctrl_ptr,
                    surf,
                    slot.bs.as_mut_ptr(),
                    &mut sync,
                )
            };
            if st == Vpl::ERR_MORE_DATA {
                self.shared
                    .free
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(slot);
                self.shared.frames_in.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            if st < 0 || st == Vpl::WRN_DEVICE_BUSY || sync.is_null() {
                self.shared
                    .order
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pop_back();
                self.shared
                    .free
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(slot);
                self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
                if st < 0 {
                    *self.shared.error.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("EncodeFrameAsync {st}"));
                }
                return false;
            }
            slot.sync = sync as usize;
            {
                let mut pq = self
                    .shared
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                pq.pending.push_back(slot);
                self.shared.pending_cv.notify_one();
            }
            self.shared.frames_in.fetch_add(1, Ordering::Relaxed);
            true
        })();

        unsafe {
            let _ = Vpl::surface_call(surf, Vpl::FI_RELEASE);
        }
        res
    }

    fn frames_in(&self) -> i64 {
        self.shared.frames_in.load(Ordering::Acquire)
    }

    fn packets_out(&self) -> i64 {
        self.shared.packets_out.load(Ordering::Acquire)
    }

    fn frames_dropped(&self) -> i64 {
        self.shared.frames_dropped.load(Ordering::Acquire)
    }

    fn forced_keyframes(&self) -> i64 {
        self.shared.forced_keyframes.load(Ordering::Acquire)
    }

    fn error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn latency(&self) -> &LatencyStats {
        &self.shared.latency
    }

    fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        let sw = Instant::now();
        {
            let _gate = self
                .shared
                .submit_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let session =
                *self.shared.session.lock().unwrap_or_else(|e| e.into_inner()) as *mut c_void;
            if !session.is_null() {
                for _ in 0..8 {
                    let deadline = Instant::now() + Duration::from_secs(1);
                    let mut maybe_slot = None;
                    while Instant::now() < deadline {
                        if let Some(s) = self
                            .shared
                            .free
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .pop_front()
                        {
                            maybe_slot = Some(s);
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    let Some(mut slot) = maybe_slot else {
                        break;
                    };
                    let mut sync: *mut c_void = std::ptr::null_mut();
                    let st = unsafe {
                        (self.shared.api.mfx_video_encode_encode_frame_async)(
                            session,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            slot.bs.as_mut_ptr(),
                            &mut sync,
                        )
                    };
                    if st < 0 || sync.is_null() {
                        self.shared
                            .free
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push_back(slot);
                        break;
                    }
                    slot.sync = sync as usize;
                    let mut pq = self
                        .shared
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    pq.pending.push_back(slot);
                    self.shared.pending_cv.notify_one();
                }
            }
            self.shared.stopping.store(true, Ordering::Release);
        }
        {
            let mut pq = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pq.closed = true;
            self.shared.pending_cv.notify_all();
        }
        if let Some(h) = self
            .sync_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        let session = {
            let mut s = self.shared.session.lock().unwrap_or_else(|e| e.into_inner());
            let ptr = *s as *mut c_void;
            *s = 0;
            ptr
        };
        if !session.is_null() {
            unsafe {
                let _ = (self.shared.api.mfx_video_encode_close)(session);
            }
        }
        {
            let _dg = self.shared.dev.gate.lock().unwrap_or_else(|e| e.into_inner());
            self.shared
                .surface_textures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        if !session.is_null() {
            unsafe {
                let _ = (self.shared.api.mfx_close)(session);
            }
        }
        let loader = {
            let mut l = self.shared.loader.lock().unwrap_or_else(|e| e.into_inner());
            let ptr = *l as *mut c_void;
            *l = 0;
            ptr
        };
        if !loader.is_null() {
            unsafe {
                (self.shared.api.mfx_unload)(loader);
            }
        }
        for mut s in self
            .shared
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            s.data.zeroize();
        }
        AgentLog::debug(&format!(
            "encoder Av1 (vpl) disposed in {} ms",
            sw.elapsed().as_millis()
        ));
    }
}

impl Drop for VplAv1Encoder {
    fn drop(&mut self) {
        self.dispose();
    }
}

unsafe fn open_loader(api: &VplApi) -> *mut c_void {
    unsafe {
        let loader = (api.mfx_load)();
        if loader.is_null() {
            return std::ptr::null_mut();
        }
        let c1 = (api.mfx_create_config)(loader);
        let c2 = (api.mfx_create_config)(loader);
        let c3 = (api.mfx_create_config)(loader);
        Vpl::set_filter(
            api,
            c1,
            "mfxImplDescription.Impl",
            MfxVariant::u32_val(Vpl::IMPL_TYPE_HARDWARE),
        );
        Vpl::set_filter(
            api,
            c2,
            "mfxImplDescription.AccelerationMode",
            MfxVariant::u32_val(Vpl::ACCEL_D3D11),
        );
        Vpl::set_filter(
            api,
            c3,
            "mfxImplDescription.mfxEncoderDescription.encoder.CodecID",
            MfxVariant::u32_val(Vpl::CODEC_AV1),
        );
        loader
    }
}

const fn align(v: i32, a: i32) -> i32 {
    (v + a - 1) / a * a
}

fn try_create(
    api: &'static VplApi,
    dev: Arc<GpuDevice>,
    params: EncoderCreateParams,
    sink: PacketSink,
    video_memory: bool,
    low_power: bool,
) -> Result<VplAv1Encoder, (String, bool)> {
    let EncoderCreateParams {
        width: w,
        height: h,
        fps,
        kbps,
    } = params;
    let loader = unsafe { open_loader(api) };
    if loader.is_null() {
        return Err(("MFXLoad failed".to_string(), false));
    }
    let mut session: *mut c_void = std::ptr::null_mut();
    let cleanup = |sess: *mut c_void| unsafe {
        if !sess.is_null() {
            let _ = (api.mfx_close)(sess);
        }
        (api.mfx_unload)(loader);
    };

    let (idx, count) = unsafe { Vpl::find_implementation(api, loader, dev.luid) };
    if idx < 0 {
        cleanup(session);
        return Err((format!("no oneVPL AV1 implementation ({count} found)"), false));
    }
    let st = unsafe { (api.mfx_create_session)(loader, idx as u32, &mut session) };
    if st < 0 {
        cleanup(session);
        return Err((format!("MFXCreateSession {st}"), false));
    }
    let st = unsafe {
        (api.mfx_video_core_set_handle)(session, Vpl::HANDLE_D3D11_DEVICE, dev.device.as_raw())
    };
    if st < 0 {
        cleanup(session);
        return Err((format!("SetHandle {st}"), false));
    }

    let sw = align(w, 16);
    let sh = align(h, 16);
    let mut par = [0u8; Vpl::PAR_SIZE];
    unsafe {
        let p = par.as_mut_ptr();
        Vpl::w16(p, Vpl::P_ASYNC_DEPTH, 1);
        Vpl::w16(p, Vpl::P_LOW_POWER, if low_power { 0x10 } else { 0 });
        Vpl::w16(p, Vpl::P_BRC_MULTIPLIER, 1);
        Vpl::w32(p, Vpl::P_FOURCC, Vpl::FOURCC_NV12);
        Vpl::w16(p, Vpl::P_BIT_DEPTH_LUMA, 8);
        Vpl::w16(p, Vpl::P_BIT_DEPTH_CHROMA, 8);
        Vpl::w16(p, Vpl::P_WIDTH, sw as u16);
        Vpl::w16(p, Vpl::P_HEIGHT, sh as u16);
        Vpl::w16(p, Vpl::P_CROP_W, w as u16);
        Vpl::w16(p, Vpl::P_CROP_H, h as u16);
        Vpl::w32(p, Vpl::P_FRAME_RATE_N, fps as u32);
        Vpl::w32(p, Vpl::P_FRAME_RATE_D, 1);
        Vpl::w16(p, Vpl::P_ASPECT_W, 1);
        Vpl::w16(p, Vpl::P_ASPECT_H, 1);
        Vpl::w16(p, Vpl::P_PIC_STRUCT, 1);
        Vpl::w16(p, Vpl::P_CHROMA_FORMAT, 1);
        Vpl::w32(p, Vpl::P_CODEC_ID, Vpl::CODEC_AV1);
        Vpl::w16(p, Vpl::P_CODEC_PROFILE, 1);
        Vpl::w16(p, Vpl::P_TARGET_USAGE, 7);
        Vpl::w16(p, Vpl::P_GOP_PIC_SIZE, fps.min(65535) as u16);
        Vpl::w16(p, Vpl::P_GOP_REF_DIST, 1);
        Vpl::w16(p, Vpl::P_GOP_OPT_FLAG, 1);
        Vpl::w16(p, Vpl::P_IDR_INTERVAL, 0);
        Vpl::w16(p, Vpl::P_RATE_CONTROL, 2);
        let k = kbps.clamp(100, 65535) as u16;
        Vpl::w16(p, Vpl::P_TARGET_KBPS, k * 3 / 4);
        Vpl::w16(p, Vpl::P_MAX_KBPS, k);
        Vpl::w16(p, Vpl::P_NUM_REF_FRAME, 1);
        Vpl::w16(p, Vpl::P_IO_PATTERN, if video_memory { 0x01 } else { 0x02 });
    }

    let mut q = par;
    let st = unsafe { (api.mfx_video_encode_query)(session, par.as_mut_ptr(), q.as_mut_ptr()) };
    if st < 0 {
        cleanup(session);
        return Err((format!("AV1 Query {st} at {w}x{h} (lowPower {low_power})"), false));
    }
    let st = unsafe { (api.mfx_video_encode_init)(session, par.as_mut_ptr()) };
    if st < 0 {
        cleanup(session);
        return Err((format!("AV1 Init {st} at {w}x{h} (lowPower {low_power})"), false));
    }

    q.fill(0);
    let st = unsafe { (api.mfx_video_encode_get_video_param)(session, q.as_mut_ptr()) };
    if st < 0 {
        unsafe {
            let _ = (api.mfx_video_encode_close)(session);
        }
        cleanup(session);
        return Err((format!("GetVideoParam {st}"), false));
    }
    let got_codec = unsafe { Vpl::r32(q.as_ptr(), Vpl::P_CODEC_ID) };
    let got_crop_w = unsafe { Vpl::r16(q.as_ptr(), Vpl::P_CROP_W) } as i32;
    if got_codec != Vpl::CODEC_AV1 || got_crop_w != w {
        unsafe {
            let _ = (api.mfx_video_encode_close)(session);
        }
        cleanup(session);
        return Err(("mfxVideoParam layout mismatch".to_string(), false));
    }
    let buf_kb = unsafe { Vpl::r16(q.as_ptr(), Vpl::P_BUFFER_SIZE_KB) as usize }
        * unsafe { Vpl::r16(q.as_ptr(), Vpl::P_BRC_MULTIPLIER) as usize }.max(1);
    let max_length = (buf_kb * 1000).max((w as usize) * (h as usize));

    if video_memory && let Err(why) = probe_video_surface(api, session, sw, sh) {
        unsafe {
            let _ = (api.mfx_video_encode_close)(session);
        }
        cleanup(session);
        return Err((format!("video-memory surfaces unusable ({why})"), true));
    }

    let name = format!(
        "oneVPL AV1{}{}",
        if low_power { " VDENC" } else { "" },
        if video_memory { "" } else { " sysmem" }
    );
    let mut free = VecDeque::with_capacity(VplAv1Encoder::DEPTH);
    for _ in 0..VplAv1Encoder::DEPTH {
        let mut bs = vec![0u8; Vpl::BS_SIZE].into_boxed_slice();
        let ctrl = vec![0u8; Vpl::CTRL_SIZE].into_boxed_slice();
        let mut data = vec![0u8; max_length].into_boxed_slice();
        unsafe {
            std::ptr::write_unaligned(
                bs.as_mut_ptr().add(Vpl::BS_DATA).cast::<*mut u8>(),
                data.as_mut_ptr(),
            );
            Vpl::w32(bs.as_mut_ptr(), Vpl::BS_MAX_LENGTH, max_length as u32);
        }
        free.push_back(VplSlot {
            bs,
            ctrl,
            data,
            sync: 0,
        });
    }

    let shared = Arc::new(VplShared {
        api,
        dev,
        sink,
        loader: Mutex::new(loader as usize),
        session: Mutex::new(session as usize),
        video_memory,
        name: name.clone(),
        width: w,
        height: h,
        surface_width: sw,
        surface_height: sh,
        free: Mutex::new(free),
        pending: Mutex::new(SyncQueue {
            pending: VecDeque::new(),
            closed: false,
        }),
        pending_cv: Condvar::new(),
        order: Mutex::new(VecDeque::new()),
        surface_textures: Mutex::new(HashMap::new()),
        submit_gate: Mutex::new(()),
        stopping: AtomicBool::new(false),
        frames_in: AtomicI64::new(0),
        packets_out: AtomicI64::new(0),
        frames_dropped: AtomicI64::new(0),
        forced_keyframes: AtomicI64::new(0),
        error: Mutex::new(None),
        latency: LatencyStats::new(),
    });

    let bg = shared.clone();
    let sync_thread = thread::Builder::new()
        .name("enc-vpl-av1".to_string())
        .spawn(move || sync_loop(bg))
        .ok();

    AgentLog::info(&format!(
        "encoder Av1 ({name}) ready ({w}x{h}@{fps}, {kbps} kbps, buffer {} KiB)",
        max_length / 1024
    ));

    Ok(VplAv1Encoder {
        shared,
        sync_thread: Mutex::new(sync_thread),
        disposed: AtomicBool::new(false),
    })
}

fn probe_video_surface(
    api: &VplApi,
    session: *mut c_void,
    sw: i32,
    sh: i32,
) -> Result<(), String> {
    let mut surf: *mut u8 = std::ptr::null_mut();
    let st = unsafe { (api.mfx_memory_get_surface_for_encode)(session, &mut surf) };
    if st < 0 || surf.is_null() {
        return Err(format!("GetSurfaceForEncode {st}"));
    }
    let res = (|| -> Result<(), String> {
        let raw = get_texture_ptr(surf)?;
        let tex = unsafe { ID3D11Texture2D::from_raw_borrowed(&raw) }
            .ok_or_else(|| "null texture".to_string())?;
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut d) };
        if d.ArraySize != 1
            || (d.Width as i32) < sw
            || (d.Height as i32) < sh
            || d.Format != DXGI_FORMAT_NV12
        {
            return Err(format!(
                "texture {}x{} array {} {:?}",
                d.Width, d.Height, d.ArraySize, d.Format
            ));
        }
        Ok(())
    })();
    unsafe {
        let _ = Vpl::surface_call(surf, Vpl::FI_RELEASE);
    }
    res
}

fn get_texture_ptr(surf: *mut u8) -> Result<*mut c_void, String> {
    let mut res: *mut c_void = std::ptr::null_mut();
    let mut ty = 0i32;
    let st = unsafe { Vpl::surface_get_native_handle(surf, &mut res, &mut ty) };
    if st < 0 || res.is_null() || ty != Vpl::RESOURCE_DX11_TEXTURE {
        return Err(format!("GetNativeHandle {st} type {ty}"));
    }
    Ok(res)
}

fn fill_surface(shared: &VplShared, surf: *mut u8, src: &ID3D11Texture2D) -> bool {
    if shared.video_memory {
        let Ok(raw) = get_texture_ptr(surf) else {
            return false;
        };
        let tex = {
            let mut map = shared
                .surface_textures
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(t) = map.get(&(raw as usize)) {
                t.clone()
            } else {
                let Some(borrowed) = (unsafe { ID3D11Texture2D::from_raw_borrowed(&raw) }) else {
                    return false;
                };
                let owned = borrowed.clone();
                map.insert(raw as usize, owned.clone());
                owned
            }
        };
        let Ok(dst_res) = tex.cast::<ID3D11Resource>() else {
            return false;
        };
        let Ok(src_res) = src.cast::<ID3D11Resource>() else {
            return false;
        };
        let bx = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: shared.surface_width as u32,
            bottom: shared.surface_height as u32,
            back: 1,
        };
        let _g = shared.dev.gate.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            shared
                .dev
                .context
                .CopySubresourceRegion(&dst_res, 0, 0, 0, 0, &src_res, 0, Some(&bx));
        }
        return true;
    }

    if unsafe { Vpl::surface_call_map(surf, Vpl::MAP_WRITE) } < 0 {
        return false;
    }
    let ok = (|| -> bool {
        let y = unsafe { *surf.add(Vpl::S_Y).cast::<*mut u8>() };
        let uv = unsafe { *surf.add(Vpl::S_UV).cast::<*mut u8>() };
        let pitch = (unsafe { Vpl::r16(surf, Vpl::S_PITCH_LOW) } as usize)
            | ((unsafe { Vpl::r16(surf, Vpl::S_PITCH_HIGH) } as usize) << 16);
        let Ok(src_res) = src.cast::<ID3D11Resource>() else {
            return false;
        };
        let _g = shared.dev.gate.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        if unsafe { shared.dev.context.Map(&src_res, 0, D3D11_MAP_READ, 0, Some(&mut m)) }.is_err()
        {
            return false;
        }
        let s = m.pData.cast::<u8>();
        let sp = m.RowPitch as usize;
        let sw = shared.surface_width as usize;
        let sh = shared.surface_height as usize;
        unsafe {
            for r in 0..sh {
                std::ptr::copy_nonoverlapping(s.add(r * sp), y.add(r * pitch), sw);
            }
            let suv = s.add(sp * sh);
            for r in 0..(sh / 2) {
                std::ptr::copy_nonoverlapping(suv.add(r * sp), uv.add(r * pitch), sw);
            }
            shared.dev.context.Unmap(&src_res, 0);
        }
        true
    })();
    unsafe {
        let _ = Vpl::surface_call(surf, Vpl::FI_UNMAP);
    }
    ok
}

fn sync_loop(shared: Arc<VplShared>) {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    loop {
        let mut slot = {
            let mut pq = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            while pq.pending.is_empty() && !pq.closed {
                pq = shared
                    .pending_cv
                    .wait(pq)
                    .unwrap_or_else(|e| e.into_inner());
            }
            let Some(s) = pq.pending.pop_front() else {
                break;
            };
            s
        };
        let session = *shared.session.lock().unwrap_or_else(|e| e.into_inner()) as *mut c_void;
        if session.is_null() {
            shared
                .free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(slot);
            break;
        }
        let mut st;
        loop {
            st = unsafe {
                (shared.api.mfx_video_core_sync_operation)(session, slot.sync as *mut c_void, 1000)
            };
            if st != Vpl::WRN_IN_EXECUTION || shared.stopping.load(Ordering::Acquire) {
                break;
            }
        }
        let now = qpc();
        let order = shared
            .order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or((0, now));
        let off = unsafe { Vpl::r32(slot.bs.as_ptr(), Vpl::BS_DATA_OFFSET) } as usize;
        let len = unsafe { Vpl::r32(slot.bs.as_ptr(), Vpl::BS_DATA_LENGTH) } as usize;
        if st >= 0 {
            if len > 0 && off + len <= slot.data.len() {
                let data = &slot.data[off..off + len];
                let ft = unsafe { Vpl::r16(slot.bs.as_ptr(), Vpl::BS_FRAME_TYPE) };
                let mut key = (ft & (Vpl::FRAME_TYPE_I | Vpl::FRAME_TYPE_IDR)) != 0
                    && CodecBits::detect_key(VideoCodec::Av1, data);
                if !key && (ft & Vpl::FRAME_TYPE_IDR) != 0 {
                    key = true;
                }
                shared.latency.add(CodecBits::ticks_to_ms(now - order.1));
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (shared.sink)(data, order.0, key);
                }))
                .is_err()
                {
                    AgentLog::warn("replay sink panicked");
                }
                shared.packets_out.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            let msg = format!("SyncOperation {st}");
            *shared.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
            AgentLog::warn(&format!("AV1 encode task failed: {st}"));
        }
        let wipe_len = slot.data.len().min(off.saturating_add(len));
        slot.data[..wipe_len].zeroize();
        unsafe {
            Vpl::w32(slot.bs.as_mut_ptr(), Vpl::BS_DATA_LENGTH, 0);
            Vpl::w32(slot.bs.as_mut_ptr(), Vpl::BS_DATA_OFFSET, 0);
        }
        shared
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(slot);
    }
}
