// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Media Foundation AAC-LC software MFT encoder (`Audio\AacEncoder.cs`).

use std::sync::{Arc, Mutex};

use windows::Win32::Media::MediaFoundation::{
    AACMFTEncoder, IMFMediaType, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Audio, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use zeroize::Zeroize;

use crate::audio::audio_timeline::AudioTimeline;
use crate::replay::mf_video_encoder::MediaFoundationRuntime;
use crate::replay::packet_ring::PacketRing;
use crate::util::AgentLog;

struct AacInner {
    mft: IMFTransform,
    ring: Arc<PacketRing>,
    out_size: u32,
    timeline: AudioTimeline,
    out_frames: i64,
    in_pts: i64,
    silence: Vec<u8>,
    gaps: i64,
    frames_in: i64,
    disposed: bool,
}

unsafe impl Send for AacInner {}

/// AAC-LC encoder (Media Foundation software MFT, synchronous) from 48 kHz stereo 16-bit PCM
/// into a [`PacketRing`].
pub struct AacEncoder {
    inner: Mutex<AacInner>,
}

impl AacEncoder {
    pub const SAMPLE_RATE: i32 = 48_000;
    pub const CHANNELS: i32 = 2;
    const AAC_FRAME: i64 = 1024;

    /// `AudioSpecificConfig` for AAC-LC 48 kHz stereo.
    pub const AUDIO_SPECIFIC_CONFIG: [u8; 2] = [0x11, 0x90];

    pub fn new(ring: Arc<PacketRing>) -> Result<Self, String> {
        MediaFoundationRuntime::ensure_started()?;
        let mft: IMFTransform =
            unsafe { CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| format!("CoCreateInstance AAC MFT: {e}"))?;

        let in_type: IMFMediaType =
            unsafe { MFCreateMediaType() }.map_err(|e| format!("MFCreateMediaType in: {e}"))?;
        unsafe {
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| e.to_string())?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, Self::SAMPLE_RATE as u32)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, Self::CHANNELS as u32)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, (Self::CHANNELS * 2) as u32)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(
                    &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                    (Self::SAMPLE_RATE * Self::CHANNELS * 2) as u32,
                )
                .map_err(|e| e.to_string())?;
        }

        let out_type: IMFMediaType =
            unsafe { MFCreateMediaType() }.map_err(|e| format!("MFCreateMediaType out: {e}"))?;
        unsafe {
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| e.to_string())?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, Self::SAMPLE_RATE as u32)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, Self::CHANNELS as u32)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
                .map_err(|e| e.to_string())?;
        }

        unsafe {
            mft.SetInputType(0, &in_type, 0)
                .map_err(|e| format!("AAC SetInputType: {e}"))?;
            mft.SetOutputType(0, &out_type, 0)
                .map_err(|e| format!("AAC SetOutputType: {e}"))?;
        }
        let info = unsafe { mft.GetOutputStreamInfo(0) }
            .map_err(|e| format!("AAC GetOutputStreamInfo: {e}"))?;
        let out_size = info.cbSize.max(8192);
        unsafe {
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| e.to_string())?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| e.to_string())?;
        }

        Ok(Self {
            inner: Mutex::new(AacInner {
                mft,
                ring,
                out_size,
                timeline: AudioTimeline::new(Self::SAMPLE_RATE),
                out_frames: 0,
                in_pts: 0,
                silence: vec![0u8; (Self::SAMPLE_RATE / 50 * 4) as usize],
                gaps: 0,
                frames_in: 0,
                disposed: false,
            }),
        })
    }

    pub fn gaps(&self) -> i64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).gaps
    }

    pub fn frames_in(&self) -> i64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .frames_in
    }

    pub fn runs(&self) -> i64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .timeline
            .runs()
    }

    /// Feeds interleaved 16-bit stereo PCM at 48 kHz with QPC timestamp in 100 ns units.
    pub fn push(&self, pcm: &[u8], qpc: i64, silent: bool) {
        if pcm.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.disposed {
            return;
        }
        let step = g.timeline.plan(qpc, (pcm.len() / 4) as i32, silent);
        if step.end_run {
            end_run(&mut g);
        }
        if step.start_run {
            g.out_frames = 0;
        }
        g.timeline.commit(step, qpc);
        if !g.timeline.in_run() {
            return;
        }
        if step.fill_frames > 0 {
            feed_silence(&mut g, step.fill_frames);
            g.gaps += 1;
        }
        if step.feed_frames > 0 {
            if step.feed_silent {
                feed_silence(&mut g, step.feed_frames as i64);
            } else {
                let start = (step.skip_frames as usize) * 4;
                let len = (step.feed_frames as usize) * 4;
                feed(&mut g, &pcm[start..start + len]);
            }
        }
    }

    pub fn dispose(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.disposed {
            return;
        }
        g.disposed = true;
        let _ = unsafe { g.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0) };
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn feed_silence(g: &mut AacInner, mut frames: i64) {
    let chunk_frames = (g.silence.len() / 4) as i64;
    while frames > 0 {
        let n = frames.min(chunk_frames) as usize;
        let sil = vec![0u8; n * 4];
        feed(g, &sil);
        frames -= n as i64;
    }
}

fn end_run(g: &mut AacInner) {
    if !g.timeline.in_run() {
        return;
    }
    let _ = unsafe { g.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) };
    drain(g);
    let _ = unsafe { g.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) };
}

fn feed(g: &mut AacInner, pcm: &[u8]) {
    let frames = (pcm.len() / 4) as i64;
    let Ok(buffer) = (unsafe { MFCreateMemoryBuffer(pcm.len() as u32) }) else {
        return;
    };
    let mut ptr: *mut u8 = std::ptr::null_mut();
    if unsafe { buffer.Lock(&mut ptr, None, None) }.is_err() || ptr.is_null() {
        return;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(pcm.as_ptr(), ptr, pcm.len());
        let _ = buffer.Unlock();
        let _ = buffer.SetCurrentLength(pcm.len() as u32);
    }
    let Ok(sample) = (unsafe { MFCreateSample() }) else {
        return;
    };
    let dur = frames * 10_000_000 / AacEncoder::SAMPLE_RATE as i64;
    if unsafe { sample.AddBuffer(&buffer) }.is_err()
        || unsafe { sample.SetSampleTime(g.in_pts) }.is_err()
        || unsafe { sample.SetSampleDuration(dur) }.is_err()
    {
        return;
    }
    if unsafe { g.mft.ProcessInput(0, &sample, 0) }.is_ok() {
        g.in_pts += dur;
        g.frames_in += frames;
        drain(g);
    }
}

fn drain(g: &mut AacInner) {
    loop {
        let Ok(out_buf) = (unsafe { MFCreateMemoryBuffer(g.out_size) }) else {
            return;
        };
        let Ok(out_sample) = (unsafe { MFCreateSample() }) else {
            return;
        };
        if unsafe { out_sample.AddBuffer(&out_buf) }.is_err() {
            return;
        }
        let mut odb = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(Some(out_sample)),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let res = unsafe { g.mft.ProcessOutput(0, &mut odb, &mut status) };
        let _sample = unsafe { std::mem::ManuallyDrop::take(&mut odb[0].pSample) };
        let _events = unsafe { std::mem::ManuallyDrop::take(&mut odb[0].pEvents) };
        if let Err(e) = res {
            if e.code() != MF_E_TRANSFORM_NEED_MORE_INPUT {
                AgentLog::debug(&format!("aac ProcessOutput 0x{:08X}", e.code().0 as u32));
            }
            return;
        }
        let t =
            g.timeline.run_start() + g.out_frames * AacEncoder::AAC_FRAME * 10_000_000 / AacEncoder::SAMPLE_RATE as i64;
        g.out_frames += 1;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        if unsafe { out_buf.Lock(&mut ptr, None, Some(&mut cur_len)) }.is_ok() {
            if !ptr.is_null() && cur_len > 0 {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr, cur_len as usize) };
                g.ring.write(slice, t, true, 0);
                slice.zeroize();
            }
            let _ = unsafe { out_buf.Unlock() };
        }
    }
}
