// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    IAudioClient, IAudioClock, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::core::PCWSTR;

/// Describes the WASAPI shared-mode mix format so Media Foundation can output matching audio.
#[derive(Debug, Clone, Copy)]
pub struct AudioMixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub block_align: u16,
    pub is_float: bool,
}

enum AudioCmd {
    Play,
    Pause,
    Flush { seq: u64, base_ms: i64 },
    Stop,
}

/// Pure audio clock state shared between the decode/UI callers and the WASAPI render loop.
///
/// Tracks seek/flush generations so an in-flight pre-flush WASAPI cycle cannot overwrite the
/// seek target with a stale hardware position, and subtracts silence frames written during
/// underruns so the master clock never drifts ahead of audio actually heard.
#[derive(Debug, Clone)]
struct AudioClockTracker {
    base_pts_ms: i64,
    clock_ms: i64,
    last_elapsed_ms: i64,
    played_frames: u64,
    silence_frames: u64,
    sample_rate: u32,
    flush_seq: u64,
    applied_flush_seq: u64,
}

impl AudioClockTracker {
    fn new(sample_rate: u32) -> Self {
        Self {
            base_pts_ms: 0,
            clock_ms: 0,
            last_elapsed_ms: 0,
            played_frames: 0,
            silence_frames: 0,
            sample_rate: sample_rate.max(1),
            flush_seq: 0,
            applied_flush_seq: 0,
        }
    }

    fn begin_flush(&mut self, new_base_ms: i64) -> u64 {
        self.flush_seq = self.flush_seq.wrapping_add(1);
        self.base_pts_ms = new_base_ms;
        self.clock_ms = new_base_ms;
        self.last_elapsed_ms = 0;
        self.played_frames = 0;
        self.silence_frames = 0;
        self.flush_seq
    }

    fn has_pending_flush(&self) -> bool {
        self.flush_seq != self.applied_flush_seq
    }

    fn complete_flush(&mut self, seq: u64, new_base_ms: i64) {
        if self.flush_seq == seq {
            self.applied_flush_seq = seq;
            self.base_pts_ms = new_base_ms;
            self.clock_ms = new_base_ms;
            self.last_elapsed_ms = 0;
            self.played_frames = 0;
            self.silence_frames = 0;
        }
    }

    fn record_submitted(&mut self, real_frames: u64, silence_frames: u64) {
        if !self.has_pending_flush() {
            self.played_frames = self.played_frames.saturating_add(real_frames);
            self.silence_frames = self.silence_frames.saturating_add(silence_frames);
        }
    }

    fn update_from_hw_clock(&mut self, rel_pos: u64, clock_freq: u64) -> i64 {
        if self.has_pending_flush() {
            return self.clock_ms;
        }
        let hw_elapsed_ms = if clock_freq > 0 {
            ((u128::from(rel_pos) * 1000) / u128::from(clock_freq)) as i64
        } else {
            0
        };
        let rate = u128::from(self.sample_rate.max(1));
        let silence_ms = ((u128::from(self.silence_frames) * 1000) / rate) as i64;
        let played_ms = ((u128::from(self.played_frames) * 1000) / rate) as i64;
        let net_elapsed_ms = hw_elapsed_ms
            .saturating_sub(silence_ms)
            .max(self.last_elapsed_ms)
            .min(played_ms);
        self.last_elapsed_ms = net_elapsed_ms;
        self.clock_ms = self.base_pts_ms.saturating_add(net_elapsed_ms);
        self.clock_ms
    }

    fn update_from_fallback(&mut self) -> i64 {
        if self.has_pending_flush() {
            return self.clock_ms;
        }
        let rate = u128::from(self.sample_rate.max(1));
        let played_ms = ((u128::from(self.played_frames) * 1000) / rate) as i64;
        self.last_elapsed_ms = self.last_elapsed_ms.max(played_ms);
        self.clock_ms = self.base_pts_ms.saturating_add(self.last_elapsed_ms);
        self.clock_ms
    }
}

#[derive(Clone, Copy)]
struct SendHandle(usize);

impl SendHandle {
    fn from_handle(h: HANDLE) -> Self {
        Self(h.0 as usize)
    }

    fn as_handle(self) -> HANDLE {
        HANDLE(self.0 as *mut c_void)
    }
}

/// RAII guard for a Win32 event `HANDLE` so setup errors cannot leak it.
struct EventHandle(HANDLE);

impl EventHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct SharedAudio {
    pcm_queue: Mutex<VecDeque<u8>>,
    max_queue_bytes: usize,
    playing: AtomicBool,
    clock: Mutex<AudioClockTracker>,
    wake_event: Mutex<Option<SendHandle>>,
    failed: AtomicBool,
    stop: AtomicBool,
}

impl SharedAudio {
    fn wake(&self) {
        if let Ok(guard) = self.wake_event.lock()
            && let Some(h) = *guard
        {
            unsafe {
                let _ = SetEvent(h.as_handle());
            }
        }
    }
}

pub struct WasapiPlayer {
    pub format: AudioMixFormat,
    shared: Arc<SharedAudio>,
    cmd_tx: mpsc::Sender<AudioCmd>,
    thread: Option<JoinHandle<()>>,
}

impl WasapiPlayer {
    /// Open the default audio render endpoint in event-driven shared mode.
    pub fn open() -> Result<Self, String> {
        let (init_tx, init_rx) = mpsc::channel::<Result<AudioMixFormat, String>>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();

        let shared_slot: Arc<Mutex<Option<Arc<SharedAudio>>>> = Arc::new(Mutex::new(None));
        let shared_slot_bg = Arc::clone(&shared_slot);

        let thread = thread::Builder::new()
            .name("br-ui-wasapi".into())
            .spawn(move || {
                run_wasapi_thread(init_tx, cmd_rx, shared_slot_bg);
            })
            .map_err(|e| format!("spawn wasapi thread: {e}"))?;

        let format = init_rx
            .recv()
            .map_err(|_| "wasapi thread exited before init".to_string())??;

        let shared = shared_slot
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| "wasapi shared state missing".to_string())?;

        Ok(Self {
            format,
            shared,
            cmd_tx,
            thread: Some(thread),
        })
    }

    pub fn push_bytes(&self, data: &[u8]) {
        if let Ok(mut q) = self.shared.pcm_queue.lock() {
            if q.len() + data.len() > self.shared.max_queue_bytes {
                let excess = (q.len() + data.len()) - self.shared.max_queue_bytes;
                let drain = excess.min(q.len());
                q.drain(..drain);
            }
            q.extend(data.iter().copied());
        }
    }

    pub fn queued_bytes(&self) -> usize {
        self.shared.pcm_queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn clock_ms(&self) -> i64 {
        self.shared
            .clock
            .lock()
            .map(|c| c.clock_ms)
            .unwrap_or(0)
    }

    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }

    pub fn set_playing(&self, playing: bool) {
        self.shared.playing.store(playing, Ordering::Relaxed);
        let _ = self
            .cmd_tx
            .send(if playing { AudioCmd::Play } else { AudioCmd::Pause });
        self.shared.wake();
    }

    pub fn flush(&self, new_base_ms: i64) {
        if let Ok(mut q) = self.shared.pcm_queue.lock() {
            q.clear();
        }
        let seq = self
            .shared
            .clock
            .lock()
            .map(|mut c| c.begin_flush(new_base_ms))
            .unwrap_or(0);
        let _ = self.cmd_tx.send(AudioCmd::Flush {
            seq,
            base_ms: new_base_ms,
        });
        self.shared.wake();
    }
}

impl Drop for WasapiPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(AudioCmd::Stop);
        self.shared.wake();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn parse_mix_format(fmt_ptr: *const WAVEFORMATEX) -> AudioMixFormat {
    let wf = unsafe { *fmt_ptr };
    let is_float = if u32::from(wf.wFormatTag) == WAVE_FORMAT_EXTENSIBLE {
        let ext = unsafe { *(fmt_ptr.cast::<WAVEFORMATEXTENSIBLE>()) };
        let sub = ext.SubFormat;
        sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        u32::from(wf.wFormatTag) == WAVE_FORMAT_IEEE_FLOAT
    };
    AudioMixFormat {
        sample_rate: wf.nSamplesPerSec,
        channels: wf.nChannels,
        bits_per_sample: wf.wBitsPerSample,
        block_align: wf.nBlockAlign,
        is_float,
    }
}

type WasapiSetup = (
    IAudioClient,
    IAudioRenderClient,
    Option<IAudioClock>,
    AudioMixFormat,
    u32,
    EventHandle,
);

fn record_wasapi_error(err: &windows::core::Error, consecutive_errors: &mut u32) -> bool {
    if err.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        return true;
    }
    *consecutive_errors += 1;
    *consecutive_errors >= 3
}

fn run_wasapi_thread(
    init_tx: mpsc::Sender<Result<AudioMixFormat, String>>,
    cmd_rx: mpsc::Receiver<AudioCmd>,
    shared_slot: Arc<Mutex<Option<Arc<SharedAudio>>>>,
) {
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();

    let setup = (|| -> Result<WasapiSetup, String> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator): {e}"))?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| format!("IMMDevice::Activate(IAudioClient): {e}"))?;

        let mix_ptr = unsafe { client.GetMixFormat() }
            .map_err(|e| format!("IAudioClient::GetMixFormat: {e}"))?;
        let format = parse_mix_format(mix_ptr);

        // 100ms shared buffer (in 100ns units).
        let init_res = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                1_000_000,
                0,
                mix_ptr,
                None,
            )
        };
        unsafe { CoTaskMemFree(Some(mix_ptr.cast())) };
        init_res.map_err(|e| format!("IAudioClient::Initialize: {e}"))?;

        let raw_event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| format!("CreateEventW(wasapi): {e}"))?;
        let event = EventHandle(raw_event);
        unsafe { client.SetEventHandle(event.raw()) }
            .map_err(|e| format!("IAudioClient::SetEventHandle: {e}"))?;

        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|e| format!("IAudioClient::GetBufferSize: {e}"))?;
        let render: IAudioRenderClient = unsafe { client.GetService() }
            .map_err(|e| format!("IAudioClient::GetService(IAudioRenderClient): {e}"))?;
        let clock: Option<IAudioClock> = unsafe { client.GetService() }.ok();

        Ok((client, render, clock, format, buffer_frames, event))
    })();

    let (client, render, clock, format, buffer_frames, event) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            if com_ok {
                unsafe { CoUninitialize() };
            }
            return;
        }
    };

    let bytes_per_sec = format.sample_rate as usize * format.block_align as usize;
    let shared = Arc::new(SharedAudio {
        pcm_queue: Mutex::new(VecDeque::with_capacity(bytes_per_sec)),
        max_queue_bytes: bytes_per_sec.max(65536),
        playing: AtomicBool::new(false),
        clock: Mutex::new(AudioClockTracker::new(format.sample_rate)),
        wake_event: Mutex::new(Some(SendHandle::from_handle(event.raw()))),
        failed: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });

    if let Ok(mut slot) = shared_slot.lock() {
        *slot = Some(Arc::clone(&shared));
    }
    let _ = init_tx.send(Ok(format));

    let clock_freq = clock
        .as_ref()
        .and_then(|c| unsafe { c.GetFrequency() }.ok())
        .filter(|&f| f > 0);
    let mut clock_offset_pos: u64 = 0;
    let mut started = false;
    let mut consecutive_errors: u32 = 0;

    while !shared.stop.load(Ordering::Relaxed) && !shared.failed.load(Ordering::Relaxed) {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCmd::Play => {
                    if !started {
                        match unsafe { client.Start() } {
                            Ok(()) => {
                                started = true;
                                consecutive_errors = 0;
                                unsafe {
                                    let _ = SetEvent(event.raw());
                                }
                            }
                            Err(e) => {
                                if record_wasapi_error(&e, &mut consecutive_errors) {
                                    shared.failed.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }
                    }
                }
                AudioCmd::Pause => {
                    if started {
                        let _ = unsafe { client.Stop() };
                        started = false;
                    }
                }
                AudioCmd::Flush { seq, base_ms } => {
                    let was_started = started;
                    if started {
                        let _ = unsafe { client.Stop() };
                        started = false;
                    }
                    let _ = unsafe { client.Reset() };
                    clock_offset_pos = 0;
                    if let Some(ref c) = clock {
                        let mut pos = 0u64;
                        if unsafe { c.GetPosition(&mut pos, None) }.is_ok() {
                            clock_offset_pos = pos;
                        }
                    }
                    if let Ok(mut clk) = shared.clock.lock() {
                        clk.complete_flush(seq, base_ms);
                    }
                    if was_started {
                        match unsafe { client.Start() } {
                            Ok(()) => started = true,
                            Err(e) => {
                                if record_wasapi_error(&e, &mut consecutive_errors) {
                                    shared.failed.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }
                    }
                }
                AudioCmd::Stop => {
                    shared.stop.store(true, Ordering::Relaxed);
                }
            }
        }
        if shared.stop.load(Ordering::Relaxed) || shared.failed.load(Ordering::Relaxed) {
            break;
        }

        let wait_ms = if started { 25 } else { 40 };
        let wait_res = unsafe { WaitForSingleObject(event.raw(), wait_ms) };
        if !started {
            continue;
        }

        // If a flush was queued while we were waiting, drain commands before rendering/clocking.
        if shared
            .clock
            .lock()
            .is_ok_and(|c| c.has_pending_flush())
        {
            continue;
        }

        if wait_res == WAIT_OBJECT_0 || started {
            let mut loop_had_error = false;
            match unsafe { client.GetCurrentPadding() } {
                Ok(padding) => {
                    let avail = buffer_frames.saturating_sub(padding);
                    if avail > 0 {
                        match unsafe { render.GetBuffer(avail) } {
                            Ok(buf_ptr) => {
                                let block_align = format.block_align.max(1) as usize;
                                let byte_count = avail as usize * block_align;
                                let mut filled = 0usize;
                                if let Ok(mut q) = shared.pcm_queue.lock() {
                                    let take = byte_count.min(q.len());
                                    // Only copy whole frames.
                                    let aligned = take - (take % block_align);
                                    if aligned > 0 {
                                        let dst = unsafe {
                                            std::slice::from_raw_parts_mut(buf_ptr, aligned)
                                        };
                                        for b in dst.iter_mut() {
                                            *b = q.pop_front().unwrap_or(0);
                                        }
                                        filled = aligned;
                                    }
                                }
                                if filled < byte_count {
                                    unsafe {
                                        ptr::write_bytes(
                                            buf_ptr.add(filled),
                                            0,
                                            byte_count - filled,
                                        );
                                    }
                                }
                                if let Err(e) = unsafe { render.ReleaseBuffer(avail, 0) } {
                                    loop_had_error = true;
                                    if record_wasapi_error(&e, &mut consecutive_errors) {
                                        shared.failed.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                } else {
                                    let real_frames = (filled / block_align) as u64;
                                    let silence_frames =
                                        u64::from(avail).saturating_sub(real_frames);
                                    if let Ok(mut clk) = shared.clock.lock() {
                                        clk.record_submitted(real_frames, silence_frames);
                                    }
                                }
                            }
                            Err(e) => {
                                loop_had_error = true;
                                if record_wasapi_error(&e, &mut consecutive_errors) {
                                    shared.failed.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    loop_had_error = true;
                    if record_wasapi_error(&e, &mut consecutive_errors) {
                        shared.failed.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }

            // Update master audio clock (guarded by flush generation and silence subtraction).
            if let (Some(c), Some(freq)) = (&clock, clock_freq) {
                let mut pos = 0u64;
                match unsafe { c.GetPosition(&mut pos, None) } {
                    Ok(()) => {
                        let rel = pos.saturating_sub(clock_offset_pos);
                        if let Ok(mut clk) = shared.clock.lock() {
                            clk.update_from_hw_clock(rel, freq);
                        }
                    }
                    Err(e) => {
                        loop_had_error = true;
                        if record_wasapi_error(&e, &mut consecutive_errors) {
                            shared.failed.store(true, Ordering::Relaxed);
                            break;
                        }
                        if let Ok(mut clk) = shared.clock.lock() {
                            clk.update_from_fallback();
                        }
                    }
                }
            } else if let Ok(mut clk) = shared.clock.lock() {
                clk.update_from_fallback();
            }
            if !loop_had_error {
                consecutive_errors = 0;
            }
        }
    }

    if started {
        let _ = unsafe { client.Stop() };
    }
    if let Ok(mut guard) = shared.wake_event.lock() {
        *guard = None;
    }
    drop(event);
    if com_ok {
        unsafe { CoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_resets_clock_atomically_and_ignores_stale_cycle() {
        let mut clk = AudioClockTracker::new(48_000);
        // Play 2 seconds of audio cleanly.
        clk.record_submitted(96_000, 0);
        assert_eq!(clk.update_from_hw_clock(96_000, 48_000), 2_000);

        // Seek to 10_000 ms; first clock read must immediately return the seek target.
        let seq = clk.begin_flush(10_000);
        assert_eq!(clk.clock_ms, 10_000);
        assert!(clk.has_pending_flush());

        // A stale in-flight WASAPI cycle reporting the old hardware position must NOT overwrite clock_ms.
        clk.record_submitted(4_800, 0);
        assert_eq!(clk.update_from_hw_clock(100_800, 48_000), 10_000);
        assert_eq!(clk.update_from_fallback(), 10_000);

        // Once the WASAPI thread processes Flush and resets the hardware offset, clock advances from 10_000.
        clk.complete_flush(seq, 10_000);
        assert!(!clk.has_pending_flush());
        assert_eq!(clk.clock_ms, 10_000);
        clk.record_submitted(24_000, 0);
        assert_eq!(clk.update_from_hw_clock(24_000, 48_000), 10_500);
    }

    #[test]
    fn silence_underrun_does_not_advance_clock() {
        let mut clk = AudioClockTracker::new(48_000);
        // 100 ms initial underrun silence before real audio arrives.
        clk.record_submitted(0, 4_800);
        assert_eq!(clk.update_from_hw_clock(2_400, 48_000), 0);
        assert_eq!(clk.update_from_hw_clock(4_800, 48_000), 0);

        // Now 200 ms of real audio plays; hardware pos includes the 100 ms silence.
        clk.record_submitted(9_600, 0);
        assert_eq!(clk.update_from_hw_clock(9_600, 48_000), 100);
        assert_eq!(clk.update_from_hw_clock(14_400, 48_000), 200);

        // Mid-playback 100 ms silence underrun: clock must not jump backward or drift forward.
        clk.record_submitted(0, 4_800);
        assert_eq!(clk.update_from_hw_clock(14_400, 48_000), 200);
        assert_eq!(clk.update_from_hw_clock(16_800, 48_000), 200);
        assert_eq!(clk.update_from_hw_clock(19_200, 48_000), 200);

        // Resume with 100 ms real audio: clock advances smoothly to 300 ms (no drift).
        clk.record_submitted(4_800, 0);
        assert_eq!(clk.update_from_hw_clock(24_000, 48_000), 300);
    }

    #[test]
    fn fallback_clock_ignores_silence_frames() {
        let mut clk = AudioClockTracker::new(48_000);
        clk.record_submitted(0, 48_000);
        assert_eq!(clk.update_from_fallback(), 0);
        clk.record_submitted(24_000, 48_000);
        assert_eq!(clk.update_from_fallback(), 500);
    }
}
