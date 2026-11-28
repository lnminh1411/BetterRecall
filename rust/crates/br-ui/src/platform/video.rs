// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::VecDeque;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gpui::RenderImage;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::smallvec;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSourceReader, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_SUBTYPE, MF_PD_DURATION, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION,
    MFAudioFormat_Float, MFAudioFormat_PCM, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFShutdown, MFStartup,
    MFSTARTUP_NOSOCKET, MFVideoArea, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0, PropVariantClear,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::{VT_I8, VT_UI8};
use windows::core::{GUID, PCWSTR};

use super::audio::WasapiPlayer;

const MAX_QUEUED_FRAMES: usize = 3;

pub struct DecodedFrame {
    pub image: Arc<RenderImage>,
    pub pts_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct PlayerSnapshot {
    pub position_ms: i64,
    pub duration_ms: i64,
    pub playing: bool,
    pub eof: bool,
    pub decoded_fps: f32,
    pub dropped_frames: u64,
    pub queue_len: usize,
    pub has_audio: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
}

enum DecodeCmd {
    Play,
    Pause,
    Seek(i64),
    Stop,
}

struct SharedState {
    frames: Mutex<VecDeque<DecodedFrame>>,
    snapshot: Mutex<PlayerSnapshot>,
    stop: AtomicBool,
}

/// Media Foundation + WASAPI replay player running decode on a dedicated thread.
pub struct MediaPlayer {
    shared: Arc<SharedState>,
    cmd_tx: mpsc::Sender<DecodeCmd>,
    thread: Option<JoinHandle<()>>,
}

impl MediaPlayer {
    pub fn open(path: PathBuf, start_offset_ms: u64) -> Self {
        let shared = Arc::new(SharedState {
            frames: Mutex::new(VecDeque::with_capacity(MAX_QUEUED_FRAMES)),
            snapshot: Mutex::new(PlayerSnapshot {
                position_ms: start_offset_ms as i64,
                playing: true,
                ..Default::default()
            }),
            stop: AtomicBool::new(false),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let bg_shared = Arc::clone(&shared);

        let thread = match thread::Builder::new()
            .name("br-ui-mf-decode".into())
            .spawn(move || {
                run_decode_thread(path, start_offset_ms as i64, bg_shared, cmd_rx);
            }) {
            Ok(handle) => Some(handle),
            Err(e) => {
                set_error(&shared, format!("Failed to start decode thread: {e}"));
                None
            }
        };

        Self {
            shared,
            cmd_tx,
            thread,
        }
    }

    pub fn play(&self) {
        let _ = self.cmd_tx.send(DecodeCmd::Play);
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(DecodeCmd::Pause);
    }

    pub fn seek_ms(&self, ms: i64) {
        let _ = self.cmd_tx.send(DecodeCmd::Seek(ms.max(0)));
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        self.shared
            .snapshot
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Pop the next frame due at or before the current master clock, dropping late frames.
    pub fn take_due_frame(&self) -> Option<DecodedFrame> {
        let (clock_ms, is_eof) = self
            .shared
            .snapshot
            .lock()
            .map(|s| (s.position_ms, s.eof))
            .unwrap_or((0, false));

        let mut q = self.shared.frames.lock().ok()?;
        let mut chosen: Option<DecodedFrame> = None;
        let mut dropped = 0u64;

        while let Some(front) = q.front() {
            if is_eof || front.pts_ms <= clock_ms + 8 {
                if chosen.is_some() {
                    dropped += 1;
                }
                chosen = q.pop_front();
            } else {
                break;
            }
        }

        let qlen = q.len();
        drop(q);

        if let Ok(mut snap) = self.shared.snapshot.lock() {
            snap.dropped_frames += dropped;
            snap.queue_len = qlen;
        }
        chosen
    }
}

impl Drop for MediaPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(DecodeCmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn set_error(shared: &SharedState, msg: String) {
    if let Ok(mut s) = shared.snapshot.lock() {
        s.error = Some(msg);
        s.playing = false;
    }
}

fn run_decode_thread(
    path: PathBuf,
    start_offset_ms: i64,
    shared: Arc<SharedState>,
    cmd_rx: mpsc::Receiver<DecodeCmd>,
) {
    if !path.exists() {
        set_error(&shared, format!("Video file not found: {}", path.display()));
        return;
    }

    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) } {
        set_error(&shared, format!("MFStartup failed: {e}"));
        if com_ok {
            unsafe { CoUninitialize() };
        }
        return;
    }

    let result = decode_loop(&path, start_offset_ms, &shared, &cmd_rx);
    if let Err(e) = result {
        set_error(&shared, e);
    }

    unsafe {
        let _ = MFShutdown();
        if com_ok {
            CoUninitialize();
        }
    }
}

fn create_source_reader(path: &Path) -> Result<IMFSourceReader, String> {
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 2) }
        .map_err(|e| format!("MFCreateAttributes: {e}"))?;
    let attrs = attrs.ok_or_else(|| "MFCreateAttributes returned None".to_string())?;
    unsafe {
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
            .map_err(|e| format!("SetUINT32(ADVANCED_VIDEO_PROCESSING): {e}"))?;
        attrs
            .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
            .map_err(|e| format!("SetUINT32(HARDWARE_TRANSFORMS): {e}"))?;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();

    unsafe { MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), &attrs) }
        .map_err(|e| format!("MFCreateSourceReaderFromURL({}): {e}", path.display()))
}

#[derive(Debug, Clone, Copy)]
struct VideoFormat {
    width: u32,
    height: u32,
    stride: i32,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
}

fn read_video_format(reader: &IMFSourceReader) -> Result<VideoFormat, String> {
    let current = unsafe {
        reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
    }
    .map_err(|e| format!("GetCurrentMediaType(video): {e}"))?;

    let frame_size = unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|e| format!("GetUINT64(MF_MT_FRAME_SIZE): {e}"))?;
    let width = (frame_size >> 32) as u32;
    let height = (frame_size & 0xFFFF_FFFF) as u32;
    if width == 0 || height == 0 {
        return Err("Video stream reported 0x0 frame size".into());
    }

    let stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|v| v as i32)
        .unwrap_or((width * 4) as i32);

    let (mut crop_x, mut crop_y, mut crop_w, mut crop_h) = (0u32, 0u32, width, height);
    let mut area_bytes = [0u8; std::mem::size_of::<MFVideoArea>()];
    let mut blob_len = 0u32;
    if unsafe {
        current.GetBlob(
            &MF_MT_MINIMUM_DISPLAY_APERTURE,
            &mut area_bytes,
            Some(&mut blob_len),
        )
    }
    .is_ok()
        && blob_len as usize >= std::mem::size_of::<MFVideoArea>()
    {
        let area: MFVideoArea =
            unsafe { std::ptr::read_unaligned(area_bytes.as_ptr().cast::<MFVideoArea>()) };
        let ox = area.OffsetX.value.max(0) as u32;
        let oy = area.OffsetY.value.max(0) as u32;
        let aw = area.Area.cx.max(0) as u32;
        let ah = area.Area.cy.max(0) as u32;
        if aw > 0 && ah > 0 && ox.saturating_add(aw) <= width && oy.saturating_add(ah) <= height {
            crop_x = ox;
            crop_y = oy;
            crop_w = aw;
            crop_h = ah;
        }
    }

    Ok(VideoFormat {
        width,
        height,
        stride,
        crop_x,
        crop_y,
        crop_w,
        crop_h,
    })
}

fn configure_video_stream(reader: &IMFSourceReader) -> Result<VideoFormat, String> {
    let out_type: IMFMediaType =
        unsafe { MFCreateMediaType() }.map_err(|e| format!("MFCreateMediaType(video): {e}"))?;
    unsafe {
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("SetGUID(Video): {e}"))?;
        out_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(|e| format!("SetGUID(RGB32): {e}"))?;
        reader
            .SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &out_type,
            )
            .map_err(|e| {
                format!(
                    "SetCurrentMediaType(RGB32) failed (missing codec/decoder?): {e}"
                )
            })?;
    }

    read_video_format(reader)
}

fn deselect_audio_stream(reader: &IMFSourceReader) {
    unsafe {
        let _ = reader.SetStreamSelection(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, false);
    }
}

fn configure_audio_stream(reader: &IMFSourceReader) -> Option<WasapiPlayer> {
    let wasapi = WasapiPlayer::open().ok()?;
    let fmt = wasapi.format;

    let out_type: IMFMediaType = unsafe { MFCreateMediaType() }.ok()?;
    let subtype = if fmt.is_float {
        &MFAudioFormat_Float
    } else {
        &MFAudioFormat_PCM
    };
    let avg_bytes = fmt.sample_rate.saturating_mul(u32::from(fmt.block_align));

    let ok = unsafe {
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).is_ok()
            && out_type.SetGUID(&MF_MT_SUBTYPE, subtype).is_ok()
            && out_type
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, fmt.sample_rate)
                .is_ok()
            && out_type
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(fmt.channels))
                .is_ok()
            && out_type
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, u32::from(fmt.bits_per_sample))
                .is_ok()
            && out_type
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, u32::from(fmt.block_align))
                .is_ok()
            && out_type
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes)
                .is_ok()
            && reader
                .SetCurrentMediaType(
                    MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                    None,
                    &out_type,
                )
                .is_ok()
    };

    if ok { Some(wasapi) } else { None }
}

fn read_duration_ms(reader: &IMFSourceReader) -> i64 {
    let Ok(mut prop) = (unsafe {
        reader.GetPresentationAttribute(
            MF_SOURCE_READER_MEDIASOURCE.0 as u32,
            &MF_PD_DURATION,
        )
    }) else {
        return 0;
    };
    let val_100ns = unsafe {
        let inner = &prop.Anonymous.Anonymous;
        let v = if inner.vt == VT_UI8 {
            inner.Anonymous.uhVal
        } else if inner.vt == VT_I8 {
            inner.Anonymous.hVal.max(0) as u64
        } else {
            0
        };
        let _ = PropVariantClear(&mut prop);
        v
    };
    (val_100ns / 10_000) as i64
}

pub(crate) fn clamp_offset_ms(offset_ms: i64, duration_ms: i64) -> i64 {
    if duration_ms > 0 {
        offset_ms.clamp(0, duration_ms)
    } else {
        offset_ms.max(0)
    }
}

fn seek_reader(reader: &IMFSourceReader, target_ms: i64) -> Result<(), String> {
    let pos_100ns = target_ms.max(0).saturating_mul(10_000);
    let prop = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { hVal: pos_100ns },
            }),
        },
    };
    unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &prop) }
        .map_err(|e| format!("SetCurrentPosition({target_ms}ms): {e}"))
}

fn rgb32_to_render_image(src: &[u8], fmt: VideoFormat) -> Option<Arc<RenderImage>> {
    let full_w = fmt.width as usize;
    let full_h = fmt.height as usize;
    let full_row_bytes = full_w * 4;
    let abs_stride = fmt.stride.unsigned_abs() as usize;
    if abs_stride < full_row_bytes || src.len() < abs_stride * full_h {
        return None;
    }

    let cx = fmt.crop_x as usize;
    let cy = fmt.crop_y as usize;
    let cw = fmt.crop_w as usize;
    let ch = fmt.crop_h as usize;
    if cw == 0 || ch == 0 || cx + cw > full_w || cy + ch > full_h {
        return None;
    }

    let out_row_bytes = cw * 4;
    let mut dst = vec![0u8; out_row_bytes * ch];
    let bottom_up = fmt.stride < 0;

    for y in 0..ch {
        let frame_y = cy + y;
        let src_y = if bottom_up { full_h - 1 - frame_y } else { frame_y };
        let src_offset = src_y * abs_stride + cx * 4;
        let srow = &src[src_offset..src_offset + out_row_bytes];
        let drow = &mut dst[y * out_row_bytes..(y + 1) * out_row_bytes];
        drow.copy_from_slice(srow);
        // MFVideoFormat_RGB32 leaves the 4th byte as 0 (XRGB); force alpha = 0xFF for GPUI BGRA.
        for px in drow.as_chunks_mut::<4>().0 {
            px[3] = 0xFF;
        }
    }

    let img_buf: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(fmt.crop_w, fmt.crop_h, dst)?;
    let frame = Frame::new(img_buf);
    Some(Arc::new(RenderImage::new(smallvec![frame])))
}

fn decode_loop(
    path: &Path,
    start_offset_ms: i64,
    shared: &Arc<SharedState>,
    cmd_rx: &mpsc::Receiver<DecodeCmd>,
) -> Result<(), String> {
    let reader = create_source_reader(path)?;
    let mut video_fmt = configure_video_stream(&reader)?;
    let duration_ms = read_duration_ms(&reader);
    let mut audio = configure_audio_stream(&reader);
    if audio.is_none() {
        deselect_audio_stream(&reader);
    }

    let mut playing = true;
    let mut eof = false;
    let mut audio_eof = audio.is_none();
    let mut mono_base_ms = clamp_offset_ms(start_offset_ms, duration_ms);
    let mut mono_start = Instant::now();

    if mono_base_ms > 0 {
        let _ = seek_reader(&reader, mono_base_ms);
    }
    if let Some(ref a) = audio {
        a.flush(mono_base_ms);
        a.set_playing(true);
    }

    if let Ok(mut snap) = shared.snapshot.lock() {
        snap.duration_ms = duration_ms;
        snap.position_ms = mono_base_ms;
        snap.playing = true;
        snap.eof = false;
        snap.has_audio = audio.is_some();
        snap.notice = None;
        snap.error = None;
    }

    let mut fps_window_start = Instant::now();
    let mut fps_decoded_count: u32 = 0;
    let mut consecutive_convert_errors: u32 = 0;

    while !shared.stop.load(Ordering::Relaxed) {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                DecodeCmd::Play => {
                    if !playing {
                        playing = true;
                        mono_start = Instant::now();
                        if let Some(ref a) = audio {
                            a.set_playing(true);
                        }
                        if eof {
                            eof = false;
                            audio_eof = audio.is_none();
                            mono_base_ms = 0;
                            let _ = seek_reader(&reader, 0);
                            if let Some(ref a) = audio {
                                a.flush(0);
                            }
                            if let Ok(mut snap) = shared.snapshot.lock() {
                                snap.eof = false;
                            }
                        }
                    }
                }
                DecodeCmd::Pause => {
                    if playing {
                        mono_base_ms = current_clock_ms(audio.as_ref(), mono_base_ms, mono_start, true);
                        playing = false;
                        if let Some(ref a) = audio {
                            a.set_playing(false);
                        }
                    }
                }
                DecodeCmd::Seek(target) => {
                    let clamped = clamp_offset_ms(target, duration_ms);
                    if let Ok(mut q) = shared.frames.lock() {
                        q.clear();
                    }
                    let _ = seek_reader(&reader, clamped);
                    eof = false;
                    audio_eof = audio.is_none();
                    mono_base_ms = clamped;
                    mono_start = Instant::now();
                    if let Some(ref a) = audio {
                        a.flush(clamped);
                    }
                    if let Ok(mut snap) = shared.snapshot.lock() {
                        snap.position_ms = clamped;
                        snap.eof = false;
                        snap.queue_len = 0;
                    }
                }
                DecodeCmd::Stop => {
                    shared.stop.store(true, Ordering::Relaxed);
                }
            }
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }

        if audio.as_ref().is_some_and(WasapiPlayer::failed) {
            if let Some(ref a) = audio {
                mono_base_ms = a.clock_ms();
            }
            mono_start = Instant::now();
            audio = None;
            audio_eof = true;
            deselect_audio_stream(&reader);
            if let Ok(mut snap) = shared.snapshot.lock() {
                snap.has_audio = false;
                snap.notice = Some("Audio device lost; playing without sound".into());
            }
        }

        let clock_ms = current_clock_ms(audio.as_ref(), mono_base_ms, mono_start, playing);
        if let Ok(mut snap) = shared.snapshot.lock() {
            snap.position_ms = if duration_ms > 0 {
                clock_ms.min(duration_ms)
            } else {
                clock_ms
            };
            snap.playing = playing;
            snap.eof = eof;
        }

        if !playing || eof {
            thread::sleep(Duration::from_millis(12));
            continue;
        }

        // Read each stream on its own: audio while its buffer is below ~250 ms, video only while the
        // 3-frame queue has room. Reading MF_SOURCE_READER_ANY_STREAM let audio demand push video far
        // ahead of the clock, so queued frames were evicted before they were ever due.
        let vq_len = shared.frames.lock().map(|q| q.len()).unwrap_or(0);
        let audio_low = !audio_eof
            && audio.as_ref().is_some_and(|a| {
                let target = a.format.sample_rate as usize * a.format.block_align as usize / 4;
                a.queued_bytes() < target
            });
        let is_video = if audio_low {
            false
        } else if vq_len < MAX_QUEUED_FRAMES {
            true
        } else {
            thread::sleep(Duration::from_millis(4));
            continue;
        };
        let stream = if is_video {
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
        } else {
            MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32
        };

        let mut flags = 0u32;
        let mut ts_100ns = 0i64;
        let mut sample = None;

        let read_res = unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts_100ns),
                Some(&mut sample),
            )
        };
        if let Err(e) = read_res {
            return Err(format!("IMFSourceReader::ReadSample: {e}"));
        }

        if is_video && (flags & (MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32)) != 0 {
            video_fmt = read_video_format(&reader)?;
        }

        if (flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32)) != 0 {
            if !is_video {
                audio_eof = true;
                continue;
            }
            if audio.is_none() {
                let end_ms = current_clock_ms(None, mono_base_ms, mono_start, true);
                mono_base_ms = if duration_ms > 0 {
                    end_ms.min(duration_ms)
                } else {
                    end_ms
                };
            }
            eof = true;
            playing = false;
            if let Some(ref a) = audio {
                a.set_playing(false);
            }
            if let Ok(mut snap) = shared.snapshot.lock() {
                snap.eof = true;
                snap.playing = false;
                let end_clock_ms =
                    current_clock_ms(audio.as_ref(), mono_base_ms, mono_start, false);
                snap.position_ms = if duration_ms > 0 {
                    end_clock_ms.min(duration_ms)
                } else {
                    end_clock_ms
                };
            }
            continue;
        }

        let Some(sample) = sample else {
            continue;
        };

        let Ok(buffer) = (unsafe { sample.ConvertToContiguousBuffer() }) else {
            continue;
        };
        let mut ptr = std::ptr::null_mut();
        let mut cur_len = 0u32;
        if unsafe { buffer.Lock(&mut ptr, None, Some(&mut cur_len)) }.is_err() {
            continue;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, cur_len as usize) };

        if is_video {
            let pts_ms = ts_100ns / 10_000;
            fps_decoded_count += 1;
            let elapsed = fps_window_start.elapsed().as_secs_f32();
            if elapsed >= 0.5 {
                let dfps = fps_decoded_count as f32 / elapsed;
                if let Ok(mut snap) = shared.snapshot.lock() {
                    snap.decoded_fps = dfps;
                }
                fps_decoded_count = 0;
                fps_window_start = Instant::now();
            }

            // Drop late frames before converting/allocating when far behind master clock.
            let now_ms = current_clock_ms(audio.as_ref(), mono_base_ms, mono_start, playing);
            if pts_ms + 50 < now_ms {
                unsafe {
                    let _ = buffer.Unlock();
                }
                if let Ok(mut snap) = shared.snapshot.lock() {
                    snap.dropped_frames += 1;
                }
                continue;
            }

            let rendered = rgb32_to_render_image(bytes, video_fmt);
            unsafe {
                let _ = buffer.Unlock();
            }

            if let Some(image) = rendered {
                consecutive_convert_errors = 0;
                if let Ok(mut q) = shared.frames.lock() {
                    while q.len() >= MAX_QUEUED_FRAMES {
                        q.pop_front();
                        if let Ok(mut snap) = shared.snapshot.lock() {
                            snap.dropped_frames += 1;
                        }
                    }
                    q.push_back(DecodedFrame { image, pts_ms });
                    let qlen = q.len();
                    if let Ok(mut snap) = shared.snapshot.lock() {
                        snap.queue_len = qlen;
                    }
                }
            } else {
                consecutive_convert_errors += 1;
                if consecutive_convert_errors >= 30 {
                    return Err(format!(
                        "Video frame conversion failed {consecutive_convert_errors} times in a row"
                    ));
                }
            }
        } else {
            if let Some(ref a) = audio {
                a.push_bytes(bytes);
            }
            unsafe {
                let _ = buffer.Unlock();
            }
        }
    }

    Ok(())
}

fn current_clock_ms(
    audio: Option<&WasapiPlayer>,
    mono_base_ms: i64,
    mono_start: Instant,
    playing: bool,
) -> i64 {
    if let Some(a) = audio {
        a.clock_ms()
    } else if playing {
        mono_base_ms + mono_start.elapsed().as_millis() as i64
    } else {
        mono_base_ms
    }
}

#[cfg(test)]
mod tests {
    use super::clamp_offset_ms;

    #[test]
    fn clamp_offset_respects_known_duration() {
        assert_eq!(clamp_offset_ms(-500, 10_000), 0);
        assert_eq!(clamp_offset_ms(0, 10_000), 0);
        assert_eq!(clamp_offset_ms(4_200, 10_000), 4_200);
        assert_eq!(clamp_offset_ms(10_000, 10_000), 10_000);
        assert_eq!(clamp_offset_ms(25_000, 10_000), 10_000);
    }

    #[test]
    fn clamp_offset_allows_non_negative_when_duration_unknown() {
        assert_eq!(clamp_offset_ms(-100, 0), 0);
        assert_eq!(clamp_offset_ms(7_500, 0), 7_500);
        assert_eq!(clamp_offset_ms(7_500, -1), 7_500);
    }
}
