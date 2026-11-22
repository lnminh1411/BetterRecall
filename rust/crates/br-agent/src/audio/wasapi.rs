// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! WASAPI shared-mode loopback and microphone capture (`Audio\Wasapi.cs`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, E_NOINTERFACE, HANDLE, S_OK, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceCompletionHandler, IAudioCaptureClient,
    IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, WAVEFORMATEX, eCapture, eConsole,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetEvent, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR};

use crate::capture::output_capture::OutputCapture;
use crate::native::{current_process_id, qpc, to_wide_z};
use crate::util::{AgentLog, HighResTimer};

pub const WAVE_FORMAT_PCM: u16 = 1;
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

pub fn wave_format_pcm16(rate: u32, channels: u16) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * channels as u32 * 2,
        nBlockAlign: channels * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

pub fn wave_format_float32(rate: u32, channels: u16) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * channels as u32 * 4,
        nBlockAlign: channels * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// Sample format of a capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PcmFormat {
    pub sample_rate: i32,
    pub channels: i32,
    pub is_float: bool,
    pub bits_per_sample: i32,
}

impl PcmFormat {
    pub const fn block_align(self) -> usize {
        (self.channels * self.bits_per_sample / 8) as usize
    }
}

/// Callback: interleaved PCM, frame count, QPC time of the first frame in 100 ns, silent flag.
pub type PcmHandler = Arc<dyn Fn(&[u8], usize, i64, bool) + Send + Sync>;

/// A running capture stream (WASAPI, or a fake in tests) (`ICaptureStream`).
pub trait CaptureStream: Send + Sync {
    /// The capture loop has exited: no more packets will come.
    fn ended(&self) -> bool;
    fn stop(&self);
}

struct ReadyState {
    ready: bool,
    error: Option<String>,
}

struct WasapiShared {
    source: String,
    requested: WAVEFORMATEX,
    handler: PcmHandler,
    startup_for_test: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    stop: AtomicBool,
    lost: AtomicBool,
    ended: AtomicBool,
    packets: AtomicI64,
    format: Mutex<PcmFormat>,
    ready: Mutex<ReadyState>,
    ready_cv: Condvar,
}

/// WASAPI shared-mode capture on a dedicated MTA thread (`WasapiCapture`).
pub struct WasapiCapture {
    shared: Arc<WasapiShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    disposed: AtomicBool,
}

impl std::fmt::Debug for WasapiCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiCapture")
            .field("source", &self.shared.source)
            .field("lost", &self.lost())
            .finish()
    }
}

impl WasapiCapture {
    /// Starts capture. Returns `Err` when the stream cannot be opened.
    pub fn start(
        source: &str,
        requested: WAVEFORMATEX,
        handler: PcmHandler,
    ) -> Result<Self, String> {
        Self::start_core(source, requested, handler, None, Duration::from_secs(5))
    }

    /// Test helper: runs `startup` on the capture thread without opening a real audio device (A_H1).
    pub fn start_for_test(
        source: &str,
        handler: PcmHandler,
        startup: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
        ready_timeout: Duration,
    ) -> Result<Self, String> {
        Self::start_core(
            source,
            wave_format_pcm16(48_000, 2),
            handler,
            Some(startup),
            ready_timeout,
        )
    }

    fn start_core(
        source: &str,
        requested: WAVEFORMATEX,
        handler: PcmHandler,
        startup_for_test: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
        ready_timeout: Duration,
    ) -> Result<Self, String> {
        let shared = Arc::new(WasapiShared {
            source: source.to_string(),
            requested,
            handler,
            startup_for_test,
            stop: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            packets: AtomicI64::new(0),
            format: Mutex::new(PcmFormat::default()),
            ready: Mutex::new(ReadyState {
                ready: false,
                error: None,
            }),
            ready_cv: Condvar::new(),
        });

        let bg = shared.clone();
        let name = format!("wasapi-{source}");
        let handle = thread::Builder::new()
            .name(name)
            .spawn(move || run_wasapi(bg))
            .map_err(|e| format!("spawn wasapi thread: {e}"))?;

        let cap = Self {
            shared: shared.clone(),
            thread: Mutex::new(Some(handle)),
            disposed: AtomicBool::new(false),
        };

        let mut g = shared.ready.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + ready_timeout;
        while !g.ready {
            let rem = deadline.saturating_duration_since(std::time::Instant::now());
            if rem.is_zero() {
                drop(g);
                cap.stop();
                return Err("Audio start timed out.".to_string());
            }
            let (next, _) = shared
                .ready_cv
                .wait_timeout(g, rem)
                .unwrap_or_else(|e| e.into_inner());
            g = next;
        }
        if let Some(err) = g.error.clone() {
            drop(g);
            cap.stop();
            return Err(err);
        }
        drop(g);
        Ok(cap)
    }

    pub fn format(&self) -> PcmFormat {
        *self.shared.format.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn lost(&self) -> bool {
        self.shared.lost.load(Ordering::Acquire)
    }

    pub fn packets(&self) -> i64 {
        self.shared.packets.load(Ordering::Acquire)
    }

    pub fn source(&self) -> &str {
        &self.shared.source
    }
}

impl CaptureStream for WasapiCapture {
    fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::Acquire)
    }

    fn stop(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared.stop.store(true, Ordering::Release);
        // Do not block forever on a wedged startup thread (A_H1): detach if it doesn't finish quickly.
        if let Some(h) = self
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !h.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn signal_ready(shared: &WasapiShared, err: Option<String>) {
    let mut g = shared.ready.lock().unwrap_or_else(|e| e.into_inner());
    if g.ready {
        return;
    }
    g.ready = true;
    g.error = err;
    shared.ready_cv.notify_all();
}

fn run_wasapi(shared: Arc<WasapiShared>) {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    if let Some(ref test_fn) = shared.startup_for_test {
        match test_fn() {
            Ok(()) => signal_ready(&shared, None),
            Err(e) => signal_ready(&shared, Some(e)),
        }
        shared.ended.store(true, Ordering::Release);
        return;
    }

    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();

    let start_res = (|| -> Result<(IAudioClient, IAudioCaptureClient, PcmFormat), String> {
        let client = if shared.source == "loopback" {
            activate_process_loopback()?
        } else {
            activate_default_mic()?
        };
        let fmt = shared.requested;
        let mut flags =
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if shared.source == "loopback" {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                2_000_000,
                0,
                &fmt,
                None,
            )
        }
        .map_err(|e| format!("IAudioClient.Initialize failed ({e})"))?;

        let pcm_fmt = PcmFormat {
            sample_rate: fmt.nSamplesPerSec as i32,
            channels: fmt.nChannels as i32,
            is_float: fmt.wFormatTag == WAVE_FORMAT_IEEE_FLOAT,
            bits_per_sample: fmt.wBitsPerSample as i32,
        };
        let capture: IAudioCaptureClient = unsafe { client.GetService() }
            .map_err(|e| format!("GetService failed ({e})"))?;
        unsafe { client.Start() }.map_err(|e| format!("Start failed ({e})"))?;
        Ok((client, capture, pcm_fmt))
    })();

    let (client, capture, pcm_fmt) = match start_res {
        Ok(v) => {
            *shared.format.lock().unwrap_or_else(|e| e.into_inner()) = v.2;
            signal_ready(&shared, None);
            v
        }
        Err(e) => {
            signal_ready(&shared, Some(e));
            shared.ended.store(true, Ordering::Release);
            if com_ok {
                unsafe { CoUninitialize() };
            }
            return;
        }
    };

    if !shared.stop.load(Ordering::Acquire) {
        AgentLog::info(&format!(
            "audio {} started {} Hz {} ch",
            shared.source, pcm_fmt.sample_rate, pcm_fmt.channels
        ));
    }

    let block = pcm_fmt.block_align();
    let timer = HighResTimer::new();
    while !shared.stop.load(Ordering::Acquire) {
        timer.sleep(400_000); // 40 ms
        while !shared.stop.load(Ordering::Acquire) {
            let pending = match unsafe { capture.GetNextPacketSize() } {
                Ok(n) => n,
                Err(e) => {
                    AgentLog::warn(&format!(
                        "audio {} device lost 0x{:08X}",
                        shared.source,
                        e.code().0 as u32
                    ));
                    shared.lost.store(true, Ordering::Release);
                    shared.stop.store(true, Ordering::Release);
                    break;
                }
            };
            if pending == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut bflags = 0u32;
            let mut dev_pos = 0u64;
            let mut qpc_pos = 0u64;
            if let Err(e) = unsafe {
                capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut bflags,
                    Some(&mut dev_pos),
                    Some(&mut qpc_pos),
                )
            } {
                AgentLog::warn(&format!(
                    "audio {} GetBuffer 0x{:08X}",
                    shared.source,
                    e.code().0 as u32
                ));
                shared.lost.store(true, Ordering::Release);
                shared.stop.store(true, Ordering::Release);
                break;
            }
            let now_100 = OutputCapture::qpc_100ns(qpc());
            let t = if qpc_pos != 0 {
                qpc_pos as i64
            } else {
                now_100 - (frames as i64) * 10_000_000 / pcm_fmt.sample_rate as i64
            };
            shared.packets.fetch_add(1, Ordering::Relaxed);
            let byte_len = (frames as usize) * block;
            let slice = if !data.is_null() && byte_len > 0 {
                unsafe { std::slice::from_raw_parts(data, byte_len) }
            } else {
                &[]
            };
            let silent = (bflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (shared.handler)(slice, frames as usize, t, silent);
            }));
            let _ = unsafe { capture.ReleaseBuffer(frames) };
        }
    }

    shared.ended.store(true, Ordering::Release);
    let _ = unsafe { client.Stop() };
    drop(capture);
    drop(client);
    if com_ok {
        unsafe { CoUninitialize() };
    }
}

fn activate_default_mic() -> Result<IAudioClient, String> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("CoCreateInstance MMDeviceEnumerator: {e}"))?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) }
        .map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))?;
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|e| format!("IMMDevice.Activate: {e}"))
}

fn activate_process_loopback() -> Result<IAudioClient, String> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: current_process_id(),
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let mut pv = PROPVARIANT::default();
    // Populate VT_BLOB directly (same layout as C# VARIANT VT_BLOB: vt at 0, cbSize at 8, pBlobData at 16).
    unsafe {
        let raw = (&raw mut pv).cast::<u8>();
        std::ptr::write_unaligned(raw.cast::<u16>(), VT_BLOB.0);
        std::ptr::write_unaligned(
            raw.add(8).cast::<u32>(),
            std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        );
        std::ptr::write_unaligned(
            raw.add(16).cast::<*mut c_void>(),
            (&raw mut params).cast::<c_void>(),
        );
    }

    let handler_raw = completion_handler::create()?;
    let handler_iface: IActivateAudioInterfaceCompletionHandler =
        unsafe { IActivateAudioInterfaceCompletionHandler::from_raw(handler_raw.cast()) };
    let path_w = to_wide_z("VAD\\Process_Loopback");

    let op = unsafe {
        ActivateAudioInterfaceAsync(
            PCWSTR(path_w.as_ptr()),
            &IAudioClient::IID,
            Some(&pv),
            &handler_iface,
        )
    }
    .map_err(|e| format!("ActivateAudioInterfaceAsync: {e}"))?;

    // Clear pv without calling PropVariantClear on stack-owned blob pointer.
    std::mem::forget(pv);

    if !completion_handler::wait(handler_raw, 5000) {
        return Err("Process loopback activation timed out.".to_string());
    }
    let mut activate_hr = S_OK;
    let mut unk: Option<IUnknown> = None;
    unsafe { op.GetActivateResult(&mut activate_hr, &mut unk) }
        .map_err(|e| format!("GetActivateResult: {e}"))?;
    activate_hr
        .ok()
        .map_err(|e| format!("Process loopback activation: {e}"))?;
    let unk = unk.ok_or_else(|| "No IAudioClient.".to_string())?;
    unk.cast::<IAudioClient>()
        .map_err(|e| format!("QueryInterface IAudioClient: {e}"))
}

mod completion_handler {
    use super::*;

    const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
    const IID_HANDLER: GUID = GUID::from_u128(0x41d949ab_9862_444a_80f6_c261334da5eb);
    const IID_IAGILE_OBJECT: GUID = GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);

    #[repr(C)]
    struct Vtbl {
        qi: unsafe extern "system" fn(*mut Obj, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut Obj) -> u32,
        release: unsafe extern "system" fn(*mut Obj) -> u32,
        activate_completed: unsafe extern "system" fn(*mut Obj, *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    pub struct Obj {
        vtbl: *const Vtbl,
        ref_count: AtomicU32,
        event: HANDLE,
    }

    static VTBL: Vtbl = Vtbl {
        qi: query_interface,
        add_ref,
        release,
        activate_completed,
    };

    pub fn create() -> Result<*mut Obj, String> {
        let ev = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| format!("CreateEventW: {e}"))?;
        let obj = Box::new(Obj {
            vtbl: &VTBL,
            ref_count: AtomicU32::new(1),
            event: ev,
        });
        Ok(Box::into_raw(obj))
    }

    pub fn wait(obj: *mut Obj, ms: u32) -> bool {
        let ev = unsafe { (*obj).event };
        unsafe { WaitForSingleObject(ev, ms) == WAIT_OBJECT_0 }
    }

    unsafe extern "system" fn query_interface(
        self_ptr: *mut Obj,
        iid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT {
        let g = unsafe { *iid };
        if g == IID_IUNKNOWN || g == IID_HANDLER || g == IID_IAGILE_OBJECT {
            unsafe {
                (*self_ptr).ref_count.fetch_add(1, Ordering::Relaxed);
                *ppv = self_ptr.cast();
            }
            S_OK
        } else {
            unsafe {
                *ppv = std::ptr::null_mut();
            }
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn add_ref(self_ptr: *mut Obj) -> u32 {
        unsafe { (*self_ptr).ref_count.fetch_add(1, Ordering::Relaxed) + 1 }
    }

    unsafe extern "system" fn release(self_ptr: *mut Obj) -> u32 {
        let prev = unsafe { (*self_ptr).ref_count.fetch_sub(1, Ordering::AcqRel) };
        let next = prev.saturating_sub(1);
        if next == 0 {
            let obj = unsafe { Box::from_raw(self_ptr) };
            unsafe {
                let _ = CloseHandle(obj.event);
            }
        }
        next
    }

    unsafe extern "system" fn activate_completed(self_ptr: *mut Obj, _op: *mut c_void) -> HRESULT {
        unsafe {
            let _ = SetEvent((*self_ptr).event);
        }
        S_OK
    }
}
