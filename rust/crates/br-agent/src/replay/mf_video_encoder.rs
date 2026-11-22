// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Media Foundation hardware video encoder MFT over D3D11 NV12 (`Replay\MfVideoEncoder.cs`).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformDrainComplete,
    METransformHaveOutput, METransformNeedInput, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_VERSION, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFShutdownObject,
    MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MF_TRANSFORM_ASYNC_UNLOCK, MFSampleExtension_CleanPoint,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use crate::capture::gpu_devices::GpuDevice;
use crate::native::qpc;
use crate::replay::bitstream::VideoCodec;
use crate::replay::video_encoders::{CodecBits, EncoderCreateParams, LatencyStats, PacketSink, VideoEncoder};
use crate::util::AgentLog;

/// Media Foundation startup, once per process (`MediaFoundationRuntime`).
pub struct MediaFoundationRuntime;

impl MediaFoundationRuntime {
    pub fn ensure_started() -> Result<(), String> {
        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        STARTED
            .get_or_init(|| {
                unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }
                    .map_err(|e| format!("MFStartup: {e}"))
            })
            .clone()
    }

    pub const fn fourcc(s: &[u8; 4]) -> GUID {
        let d1 = (s[0] as u32)
            | ((s[1] as u32) << 8)
            | ((s[2] as u32) << 16)
            | ((s[3] as u32) << 24);
        GUID::from_values(
            d1,
            0x0000,
            0x0010,
            [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
        )
    }

    pub const MF_VIDEO_FORMAT_NV12: GUID = Self::fourcc(b"NV12");
    pub const MF_VIDEO_FORMAT_H264: GUID = Self::fourcc(b"H264");
    pub const MF_VIDEO_FORMAT_HEVC: GUID = Self::fourcc(b"HEVC");
    pub const MF_VIDEO_FORMAT_AV1: GUID = Self::fourcc(b"AV01");
}

mod codec_api {
    use super::*;

    pub const IID_ICODEC_API: GUID = GUID::from_u128(0x901db4c7_31ce_41a2_85dc_8fa0bf41b8da);
    pub const RATE_CONTROL_MODE: GUID = GUID::from_u128(0x1c0608e9_370c_4710_8a58_cb6181c42423);
    pub const MEAN_BIT_RATE: GUID = GUID::from_u128(0xf7222374_2144_4815_b550_a37f8e12ee52);
    pub const MAX_BIT_RATE: GUID = GUID::from_u128(0x9651eae4_39b9_4ebf_85ef_d7f444ec7465);
    pub const GOP_SIZE: GUID = GUID::from_u128(0x95f31b26_95a4_41aa_9303_246a7fc6eef1);
    pub const B_PICTURE_COUNT: GUID = GUID::from_u128(0x8d390aac_dc5c_4200_b57f_814d04babab2);
    pub const LOW_LATENCY: GUID = GUID::from_u128(0x9c27891a_ed7a_40e1_88e8_b22727a024ee);
    pub const FORCE_KEY_FRAME: GUID = GUID::from_u128(0x398c1b98_8353_475a_9ef2_8f265d260345);
    pub const QUALITY_VS_SPEED: GUID = GUID::from_u128(0x98332df8_03cd_476b_89fa_3f9e442dec9f);
    pub const RATE_PEAK_VBR: u32 = 1;

    /// Calls `ICodecAPI::SetValue` (vtable slot 9) on a raw COM interface pointer.
    ///
    /// # Safety
    /// `codec_api` must be a valid non-null `ICodecAPI` COM pointer.
    pub(super) unsafe fn set_value(codec_api: *mut c_void, key: &GUID, value: &VARIANT) -> HRESULT {
        unsafe {
            let vtbl = *(codec_api as *const *const *const c_void);
            let fn_ptr: unsafe extern "system" fn(*mut c_void, *const GUID, *const VARIANT) -> HRESULT =
                std::mem::transmute(*vtbl.add(9));
            fn_ptr(codec_api, key, value)
        }
    }

    /// Queries `ICodecAPI` from an `IUnknown` raw pointer.
    ///
    /// # Safety
    /// `unk` must be a valid non-null `IUnknown` COM pointer.
    pub(super) unsafe fn query(unk: *mut c_void) -> *mut c_void {
        unsafe {
            let vtbl = *(unk as *const *const *const c_void);
            let qi: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT =
                std::mem::transmute(*vtbl.add(0));
            let mut out = std::ptr::null_mut();
            let hr = qi(unk, &IID_ICODEC_API, &mut out);
            if hr.is_ok() { out } else { std::ptr::null_mut() }
        }
    }

    /// Calls `IUnknown::Release` on a raw COM pointer.
    ///
    /// # Safety
    /// `ptr` must be null or a valid `IUnknown` COM pointer with an owned reference count.
    pub(super) unsafe fn release(ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            let vtbl = *(ptr as *const *const *const c_void);
            let rel: unsafe extern "system" fn(*mut c_void) -> u32 =
                std::mem::transmute(*vtbl.add(2));
            rel(ptr);
        }
    }
}

struct MfShared {
    activate: IMFActivate,
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    _dev_manager: IMFDXGIDeviceManager,
    codec_api: usize, // *mut c_void
    codec: VideoCodec,
    name: String,
    width: i32,
    height: i32,
    frame_duration: i64,
    sink: PacketSink,
    submit_ticks: Mutex<HashMap<i64, i64>>,
    need_input: AtomicI32,
    stopping: AtomicBool,
    frames_in: AtomicI64,
    packets_out: AtomicI64,
    frames_dropped: AtomicI64,
    forced_keyframes: AtomicI64,
    error: Mutex<Option<String>>,
    latency: LatencyStats,
}

// SAFETY: MF async MFT and event generator are thread-safe COM objects; `codec_api` is released on dispose.
unsafe impl Send for MfShared {}
unsafe impl Sync for MfShared {}

/// Hardware video encoder MFT on one adapter (matched by `MFT_ENUM_ADAPTER_LUID`, else by vendor id).
pub struct MfVideoEncoder {
    shared: Arc<MfShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    disposed: AtomicBool,
}

impl MfVideoEncoder {
    pub const fn subtype(c: VideoCodec) -> GUID {
        match c {
            VideoCodec::Av1 => MediaFoundationRuntime::MF_VIDEO_FORMAT_AV1,
            VideoCodec::Hevc => MediaFoundationRuntime::MF_VIDEO_FORMAT_HEVC,
            VideoCodec::H264 => MediaFoundationRuntime::MF_VIDEO_FORMAT_H264,
        }
    }

    /// Friendly name of the hardware encoder MFT for this adapter and codec, or `None`.
    pub fn find(luid: i64, vendor_id: u32, codec: VideoCodec) -> Option<String> {
        let (act, name) = find_activate(luid, vendor_id, codec)?;
        let res = unsafe { act.ActivateObject::<IMFTransform>() };
        match res {
            Ok(mft) => {
                if let Ok(unk) = mft.cast::<IUnknown>() {
                    let _ = unsafe { MFShutdownObject(&unk) };
                }
                let _ = unsafe { act.ShutdownObject() };
                Some(name)
            }
            Err(e) => {
                let first_line = e.to_string();
                let first_line = first_line.lines().next().unwrap_or("").trim();
                AgentLog::info(&format!(
                    "encoder MFT '{name}' enumerates but does not activate: {first_line}"
                ));
                let _ = unsafe { act.ShutdownObject() };
                None
            }
        }
    }

    pub fn create(
        dev: Arc<GpuDevice>,
        codec: VideoCodec,
        params: EncoderCreateParams,
        sink: PacketSink,
    ) -> Result<Self, String> {
        let EncoderCreateParams {
            width: w,
            height: h,
            fps,
            kbps,
        } = params;
        let Some((activate, name)) = find_activate(dev.luid, dev.vendor_id, codec) else {
            return Err(format!(
                "no {codec} MFT on adapter vendor 0x{:04X}",
                dev.vendor_id
            ));
        };

        let init_res = (|| -> Result<Self, String> {
            let mft: IMFTransform = unsafe { activate.ActivateObject() }
                .map_err(|e| format!("{codec} mf init (activate, '{name}'): {e}"))?;

            let attrs = unsafe { mft.GetAttributes() }
                .map_err(|e| format!("{codec} mf init (attributes, '{name}'): {e}"))?;
            unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .map_err(|e| format!("{codec} mf init (attributes, '{name}'): {e}"))?;
            let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };

            let mut reset_token = 0u32;
            let mut dev_manager: Option<IMFDXGIDeviceManager> = None;
            unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut dev_manager) }
                .map_err(|e| format!("{codec} mf init (d3d manager, '{name}'): {e}"))?;
            let dev_manager = dev_manager
                .ok_or_else(|| format!("{codec} mf init (d3d manager, '{name}'): null manager"))?;
            let dev_unk: IUnknown = dev
                .device
                .cast()
                .map_err(|e| format!("{codec} mf init (d3d manager, '{name}'): {e}"))?;
            unsafe { dev_manager.ResetDevice(&dev_unk, reset_token) }
                .map_err(|e| format!("{codec} mf init (d3d manager, '{name}'): {e}"))?;
            unsafe {
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, dev_manager.as_raw() as usize)
            }
            .map_err(|e| format!("{codec} mf init (d3d manager, '{name}'): {e}"))?;

            let events: IMFMediaEventGenerator = mft
                .cast()
                .map_err(|e| format!("{codec} mf init (configure, '{name}'): {e}"))?;

            let codec_api = configure_mft(&mft, codec, w, h, fps, kbps)
                .map_err(|e| format!("{codec} mf init (configure, '{name}'): {e}"))?;

            let shared = Arc::new(MfShared {
                activate: activate.clone(),
                mft: mft.clone(),
                events,
                _dev_manager: dev_manager,
                codec_api: codec_api as usize,
                codec,
                name: name.clone(),
                width: w,
                height: h,
                frame_duration: 10_000_000 / fps.max(1) as i64,
                sink,
                submit_ticks: Mutex::new(HashMap::new()),
                need_input: AtomicI32::new(0),
                stopping: AtomicBool::new(false),
                frames_in: AtomicI64::new(0),
                packets_out: AtomicI64::new(0),
                frames_dropped: AtomicI64::new(0),
                forced_keyframes: AtomicI64::new(0),
                error: Mutex::new(None),
                latency: LatencyStats::new(),
            });

            let bg = shared.clone();
            let handle = thread::Builder::new()
                .name(format!("enc-mf-{codec}"))
                .spawn(move || event_loop(bg))
                .map_err(|e| format!("{codec} mf init (start, '{name}'): {e}"))?;

            unsafe {
                mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                    .map_err(|e| format!("{codec} mf init (start, '{name}'): {e}"))?;
                mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    .map_err(|e| format!("{codec} mf init (start, '{name}'): {e}"))?;
            }

            AgentLog::info(&format!(
                "encoder {codec} (mf '{name}') ready ({w}x{h}@{fps}, {kbps} kbps)"
            ));

            Ok(Self {
                shared,
                thread: Mutex::new(Some(handle)),
                disposed: AtomicBool::new(false),
            })
        })();

        if init_res.is_err() {
            let _ = unsafe { activate.ShutdownObject() };
        }
        init_res
    }
}

impl VideoEncoder for MfVideoEncoder {
    fn codec(&self) -> VideoCodec {
        self.shared.codec
    }

    fn api(&self) -> &str {
        "mf"
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
        self.shared.width
    }

    fn surface_height(&self) -> i32 {
        self.shared.height
    }

    fn copies_input(&self) -> bool {
        false
    }

    fn ready_for_input(&self) -> bool {
        self.shared.need_input.load(Ordering::Acquire) > 0
            && !self.shared.stopping.load(Ordering::Acquire)
    }

    fn try_submit(&self, nv12: &ID3D11Texture2D, pts_100ns: i64, force_key: bool) -> bool {
        if self.shared.stopping.load(Ordering::Acquire) {
            return false;
        }
        if self.shared.need_input.load(Ordering::Acquire) <= 0 {
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if force_key && self.shared.codec_api != 0 {
            let v = VARIANT::from(1u32);
            let hr = unsafe {
                codec_api::set_value(
                    self.shared.codec_api as *mut c_void,
                    &codec_api::FORCE_KEY_FRAME,
                    &v,
                )
            };
            if hr.is_ok() {
                self.shared.forced_keyframes.fetch_add(1, Ordering::Relaxed);
            }
        }
        let Ok(unk) = nv12.cast::<IUnknown>() else {
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let Ok(buffer) =
            (unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &unk, 0, false) })
        else {
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let Ok(sample) = (unsafe { MFCreateSample() }) else {
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if unsafe { sample.AddBuffer(&buffer) }.is_err()
            || unsafe { sample.SetSampleTime(pts_100ns) }.is_err()
            || unsafe { sample.SetSampleDuration(self.shared.frame_duration) }.is_err()
        {
            self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        {
            self.shared
                .submit_ticks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pts_100ns, qpc());
        }
        match unsafe { self.shared.mft.ProcessInput(0, &sample, 0) } {
            Ok(()) => {
                self.shared.need_input.fetch_sub(1, Ordering::AcqRel);
                self.shared.frames_in.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(e) => {
                self.shared
                    .submit_ticks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&pts_100ns);
                self.shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
                AgentLog::debug(&format!("encoder ProcessInput 0x{:08X}", e.code().0 as u32));
                false
            }
        }
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
        unsafe {
            let _ = self
                .shared
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.shared.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        // Let the drain finish briefly before forcing shutdown.
        thread::sleep(std::time::Duration::from_millis(50));
        self.shared.stopping.store(true, Ordering::Release);
        if let Ok(unk) = self.shared.mft.cast::<IUnknown>() {
            let _ = unsafe { MFShutdownObject(&unk) };
        }
        if let Some(h) = self
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        if self.shared.codec_api != 0 {
            unsafe { codec_api::release(self.shared.codec_api as *mut c_void) };
        }
        let _ = unsafe { self.shared.activate.ShutdownObject() };
        AgentLog::debug(&format!(
            "encoder {} (mf) disposed in {} ms",
            self.shared.codec,
            sw.elapsed().as_millis()
        ));
    }
}

impl Drop for MfVideoEncoder {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn find_activate(luid: i64, vendor_id: u32, codec: VideoCodec) -> Option<(IMFActivate, String)> {
    MediaFoundationRuntime::ensure_started().ok()?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MediaFoundationRuntime::MF_VIDEO_FORMAT_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MfVideoEncoder::subtype(codec),
    };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    if unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut ptr,
            &mut count,
        )
    }
    .is_err()
        || ptr.is_null()
        || count == 0
    {
        if !ptr.is_null() {
            unsafe { CoTaskMemFree(Some(ptr.cast())) };
        }
        return None;
    }

    let vendor_hex = format!("{vendor_id:04X}");
    let mut by_luid: Option<(IMFActivate, String)> = None;
    let mut by_vendor: Option<(IMFActivate, String)> = None;

    let slice = unsafe { std::slice::from_raw_parts_mut(ptr, count as usize) };
    for slot in slice.iter_mut() {
        let Some(act) = slot.take() else {
            continue;
        };
        let fn_name = get_mf_string(&act, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
        if let Ok(l) = unsafe { act.GetUINT64(&MFT_ENUM_ADAPTER_LUID) } {
            if l as i64 == luid && by_luid.is_none() {
                by_luid = Some((act, fn_name));
            }
            continue;
        }
        let vendor =
            get_mf_string(&act, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute).unwrap_or_default();
        if by_vendor.is_none()
            && vendor
                .to_ascii_uppercase()
                .contains(vendor_hex.as_str())
        {
            by_vendor = Some((act, fn_name));
        }
    }
    unsafe { CoTaskMemFree(Some(ptr.cast())) };
    by_luid.or(by_vendor)
}

fn get_mf_string(act: &IMFActivate, key: &GUID) -> Option<String> {
    let len = unsafe { act.GetStringLength(key) }.ok()?;
    let mut buf = vec![0u16; len as usize + 1];
    let mut written = 0u32;
    unsafe { act.GetString(key, &mut buf, Some(&mut written)) }.ok()?;
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

fn configure_mft(
    mft: &IMFTransform,
    codec: VideoCodec,
    width: i32,
    height: i32,
    fps: i32,
    kbps: i32,
) -> Result<*mut c_void, String> {
    let frame_size = ((width as u64) << 32) | (height as u32 as u64);
    let frame_rate = ((fps as u64) << 32) | 1u64;
    let par = (1u64 << 32) | 1u64;

    let out_type: IMFMediaType =
        unsafe { MFCreateMediaType() }.map_err(|e| format!("MFCreateMediaType: {e}"))?;
    unsafe {
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| e.to_string())?;
        out_type
            .SetGUID(&MF_MT_SUBTYPE, &MfVideoEncoder::subtype(codec))
            .map_err(|e| e.to_string())?;
        out_type
            .SetUINT32(&MF_MT_AVG_BITRATE, (kbps * 1000) as u32)
            .map_err(|e| e.to_string())?;
        out_type
            .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
            .map_err(|e| e.to_string())?;
        out_type
            .SetUINT64(&MF_MT_FRAME_RATE, frame_rate)
            .map_err(|e| e.to_string())?;
        out_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, par)
            .map_err(|e| e.to_string())?;
        out_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, 2)
            .map_err(|e| e.to_string())?;
        out_type
            .SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, fps as u32)
            .map_err(|e| e.to_string())?;
        if codec == VideoCodec::H264 {
            out_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, 100)
                .map_err(|e| e.to_string())?;
        } else if codec == VideoCodec::Hevc {
            out_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, 1)
                .map_err(|e| e.to_string())?;
        }
        mft.SetOutputType(0, &out_type, 0)
            .map_err(|e| format!("SetOutputType: {e}"))?;
    }

    let mut in_type: Option<IMFMediaType> = None;
    let mut i = 0u32;
    while let Ok(t) = unsafe { mft.GetInputAvailableType(0, i) } {
        i += 1;
        if let Ok(sub) = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }
            && sub == MediaFoundationRuntime::MF_VIDEO_FORMAT_NV12
        {
            in_type = Some(t);
            break;
        }
    }
    let in_type = match in_type {
        Some(t) => t,
        None => unsafe { MFCreateMediaType() }.map_err(|e| format!("MFCreateMediaType: {e}"))?,
    };
    unsafe {
        in_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| e.to_string())?;
        in_type
            .SetGUID(&MF_MT_SUBTYPE, &MediaFoundationRuntime::MF_VIDEO_FORMAT_NV12)
            .map_err(|e| e.to_string())?;
        in_type
            .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
            .map_err(|e| e.to_string())?;
        in_type
            .SetUINT64(&MF_MT_FRAME_RATE, frame_rate)
            .map_err(|e| e.to_string())?;
        in_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, par)
            .map_err(|e| e.to_string())?;
        in_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, 2)
            .map_err(|e| e.to_string())?;
        mft.SetInputType(0, &in_type, 0)
            .map_err(|e| format!("SetInputType: {e}"))?;
    }

    let api_ptr = unsafe { codec_api::query(mft.as_raw()) };
    if !api_ptr.is_null() {
        let bps = (kbps * 1000) as u32;
        let try_set = |key: &GUID, v: VARIANT| {
            let hr = unsafe { codec_api::set_value(api_ptr, key, &v) };
            if hr.is_err() {
                AgentLog::debug(&format!("ICodecAPI {:?} -> 0x{:08X}", key, hr.0 as u32));
            }
        };
        try_set(&codec_api::RATE_CONTROL_MODE, VARIANT::from(codec_api::RATE_PEAK_VBR));
        try_set(&codec_api::MEAN_BIT_RATE, VARIANT::from(bps * 3 / 4));
        try_set(&codec_api::MAX_BIT_RATE, VARIANT::from(bps));
        try_set(&codec_api::GOP_SIZE, VARIANT::from(fps as u32));
        try_set(&codec_api::B_PICTURE_COUNT, VARIANT::from(0u32));
        try_set(&codec_api::LOW_LATENCY, VARIANT::from(true));
        try_set(&codec_api::QUALITY_VS_SPEED, VARIANT::from(33u32));
    }
    Ok(api_ptr)
}

fn event_loop(shared: Arc<MfShared>) {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    while !shared.stopping.load(Ordering::Acquire) {
        let ev =
            match unsafe { shared.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
            Ok(ev) => ev,
            Err(e) => {
                if !shared.stopping.load(Ordering::Acquire) {
                    let msg = format!("GetEvent 0x{:08X}", e.code().0 as u32);
                    AgentLog::warn(&format!("encoder event loop ended: {msg}"));
                    *shared.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
                }
                break;
            }
        };
        let ev_type = unsafe { ev.GetType() }.unwrap_or(0) as i32;
        if ev_type == METransformNeedInput.0 {
            shared.need_input.fetch_add(1, Ordering::AcqRel);
        } else if ev_type == METransformHaveOutput.0 {
            drain_output(&shared);
        } else if ev_type == METransformDrainComplete.0 {
            break;
        } else {
            let st = unsafe { ev.GetStatus() }.unwrap_or(E_FAIL);
            if st.is_err() {
                let msg = format!("0x{:08X}", st.0 as u32);
                AgentLog::warn(&format!("encoder event {ev_type} status {msg}"));
                *shared.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
            }
        }
    }
}

fn drain_output(shared: &MfShared) {
    loop {
        let mut odb = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let res = unsafe { shared.mft.ProcessOutput(0, &mut odb, &mut status) };
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut odb[0].pSample) };
        let _events = unsafe { std::mem::ManuallyDrop::take(&mut odb[0].pEvents) };
        if let Err(e) = res {
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return;
            }
            if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                if let Ok(t) = unsafe { shared.mft.GetOutputAvailableType(0, 0) } {
                    let _ = unsafe { shared.mft.SetOutputType(0, &t, 0) };
                }
                continue;
            }
            AgentLog::debug(&format!("encoder ProcessOutput 0x{:08X}", e.code().0 as u32));
            return;
        }
        let Some(sample) = sample else {
            return;
        };
        write_packet(shared, &sample);
        return;
    }
}

fn write_packet(shared: &MfShared, sample: &IMFSample) {
    let pts = unsafe { sample.GetSampleTime() }.unwrap_or(0);
    let mut key = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
        .is_ok_and(|cp| cp != 0);
    let now = qpc();
    {
        let mut map = shared
            .submit_ticks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(t0) = map.remove(&pts) {
            shared.latency.add(CodecBits::ticks_to_ms(now - t0));
        }
        if map.len() > 256 {
            map.clear();
        }
    }
    let Ok(buf) = (unsafe { sample.ConvertToContiguousBuffer() }) else {
        return;
    };
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut cur_len = 0u32;
    if unsafe { buf.Lock(&mut ptr, None, Some(&mut cur_len)) }.is_err() {
        return;
    }
    if !ptr.is_null() && cur_len > 0 {
        let data = unsafe { std::slice::from_raw_parts(ptr, cur_len as usize) };
        if !key {
            key = CodecBits::detect_key(shared.codec, data);
        }
        (shared.sink)(data, pts, key);
        shared.packets_out.fetch_add(1, Ordering::Relaxed);
    }
    let _ = unsafe { buf.Unlock() };
}
