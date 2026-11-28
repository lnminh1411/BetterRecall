// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, IAudioClient, IAudioClock,
    IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
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
    Flush(i64),
    Stop,
}

struct SharedAudio {
    pcm_queue: Mutex<VecDeque<u8>>,
    max_queue_bytes: usize,
    playing: AtomicBool,
    clock_ms: AtomicI64,
    base_pts_ms: AtomicI64,
    played_frames: AtomicU64,
    sample_rate: u32,
    stop: AtomicBool,
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
        self.shared.clock_ms.load(Ordering::Relaxed)
    }

    pub fn set_playing(&self, playing: bool) {
        self.shared.playing.store(playing, Ordering::Relaxed);
        let _ = self
            .cmd_tx
            .send(if playing { AudioCmd::Play } else { AudioCmd::Pause });
    }

    pub fn flush(&self, new_base_ms: i64) {
        if let Ok(mut q) = self.shared.pcm_queue.lock() {
            q.clear();
        }
        self.shared.base_pts_ms.store(new_base_ms, Ordering::Relaxed);
        self.shared.played_frames.store(0, Ordering::Relaxed);
        self.shared.clock_ms.store(new_base_ms, Ordering::Relaxed);
        let _ = self.cmd_tx.send(AudioCmd::Flush(new_base_ms));
    }
}

impl Drop for WasapiPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(AudioCmd::Stop);
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

fn run_wasapi_thread(
    init_tx: mpsc::Sender<Result<AudioMixFormat, String>>,
    cmd_rx: mpsc::Receiver<AudioCmd>,
    shared_slot: Arc<Mutex<Option<Arc<SharedAudio>>>>,
) {
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();

    let setup = (|| -> Result<(IAudioClient, IAudioRenderClient, Option<IAudioClock>, AudioMixFormat, u32, windows::Win32::Foundation::HANDLE), String> {
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

        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| format!("CreateEventW(wasapi): {e}"))?;
        if let Err(e) = unsafe { client.SetEventHandle(event) } {
            unsafe {
                let _ = CloseHandle(event);
            }
            return Err(format!("IAudioClient::SetEventHandle: {e}"));
        }

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
        clock_ms: AtomicI64::new(0),
        base_pts_ms: AtomicI64::new(0),
        played_frames: AtomicU64::new(0),
        sample_rate: format.sample_rate.max(1),
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

    while !shared.stop.load(Ordering::Relaxed) {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCmd::Play => {
                    if !started {
                        if unsafe { client.Start() }.is_ok() {
                            started = true;
                        }
                        unsafe {
                            let _ = SetEvent(event);
                        }
                    }
                }
                AudioCmd::Pause => {
                    if started {
                        let _ = unsafe { client.Stop() };
                        started = false;
                    }
                }
                AudioCmd::Flush(new_base) => {
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
                    shared.base_pts_ms.store(new_base, Ordering::Relaxed);
                    shared.played_frames.store(0, Ordering::Relaxed);
                    shared.clock_ms.store(new_base, Ordering::Relaxed);
                    if was_started && unsafe { client.Start() }.is_ok() {
                        started = true;
                    }
                }
                AudioCmd::Stop => {
                    shared.stop.store(true, Ordering::Relaxed);
                }
            }
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }

        let wait_ms = if started { 25 } else { 40 };
        let wait_res = unsafe { WaitForSingleObject(event, wait_ms) };
        if !started {
            continue;
        }

        if wait_res == WAIT_OBJECT_0 || started {
            let padding = unsafe { client.GetCurrentPadding() }.unwrap_or(0);
            let avail = buffer_frames.saturating_sub(padding);
            if avail > 0
                && let Ok(buf_ptr) = unsafe { render.GetBuffer(avail) }
            {
                let byte_count = avail as usize * format.block_align as usize;
                let mut filled = 0usize;
                if let Ok(mut q) = shared.pcm_queue.lock() {
                    let take = byte_count.min(q.len());
                    // Only copy whole frames.
                    let aligned = take - (take % (format.block_align.max(1) as usize));
                    if aligned > 0 {
                        let dst = unsafe { std::slice::from_raw_parts_mut(buf_ptr, aligned) };
                        for b in dst.iter_mut() {
                            *b = q.pop_front().unwrap_or(0);
                        }
                        filled = aligned;
                    }
                }
                if filled < byte_count {
                    unsafe {
                        ptr::write_bytes(buf_ptr.add(filled), 0, byte_count - filled);
                    }
                }
                let _ = unsafe { render.ReleaseBuffer(avail, 0) };
                if filled > 0 {
                    let frames_written = (filled / format.block_align.max(1) as usize) as u64;
                    shared
                        .played_frames
                        .fetch_add(frames_written, Ordering::Relaxed);
                }
            }

            // Update master audio clock.
            let base = shared.base_pts_ms.load(Ordering::Relaxed);
            let elapsed_ms = if let (Some(c), Some(freq)) = (&clock, clock_freq) {
                let mut pos = 0u64;
                if unsafe { c.GetPosition(&mut pos, None) }.is_ok() {
                    let rel = pos.saturating_sub(clock_offset_pos);
                    ((rel as u128 * 1000) / (freq as u128)) as i64
                } else {
                    (shared.played_frames.load(Ordering::Relaxed) * 1000
                        / u64::from(shared.sample_rate)) as i64
                }
            } else {
                (shared.played_frames.load(Ordering::Relaxed) * 1000
                    / u64::from(shared.sample_rate)) as i64
            };
            shared.clock_ms.store(base + elapsed_ms, Ordering::Relaxed);
        }
    }

    if started {
        let _ = unsafe { client.Stop() };
    }
    unsafe {
        let _ = CloseHandle(event);
    }
    if com_ok {
        unsafe { CoUninitialize() };
    }
}
