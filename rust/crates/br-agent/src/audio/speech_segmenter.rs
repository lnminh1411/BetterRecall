// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! PCM zeroization helpers, 16 kHz mono resampler, and energy-gated speech segmenter (`Audio\SpeechSegmenter.cs`).

use std::collections::VecDeque;
use std::sync::Arc;

use zeroize::Zeroize;

use crate::audio::wasapi::PcmFormat;

/// Raw PCM vec helpers that zero vacated capacity/elements (CA-S2).
pub struct Pcm;

impl Pcm {
    /// Zeroes the whole backing buffer (including spare capacity), then empties the vec.
    pub fn wipe(v: &mut Vec<f32>) {
        v.zeroize();
        let cap = v.capacity();
        if cap > 0 {
            unsafe {
                std::ptr::write_bytes(v.as_mut_ptr(), 0, cap);
            }
        }
        v.clear();
    }

    /// Removes the first `n` elements and zeroes the vacated tail.
    pub fn remove_head(v: &mut Vec<f32>, n: usize) {
        if n == 0 {
            return;
        }
        let n = n.min(v.len());
        let old = v.len();
        v.drain(..n);
        let now = v.len();
        if old > now {
            unsafe {
                std::ptr::write_bytes(v.as_mut_ptr().add(now), 0, old - now);
            }
        }
    }

    /// Appends `data`, explicitly zeroing the old backing buffer if reallocation is needed.
    pub fn append(v: &mut Vec<f32>, data: &[f32]) {
        if v.len() + data.len() > v.capacity() {
            let new_cap = (v.capacity() * 2).max(v.len() + data.len());
            let mut next = Vec::with_capacity(new_cap);
            next.extend_from_slice(v);
            Self::wipe(v);
            *v = next;
        }
        v.extend_from_slice(data);
    }
}

/// Converts interleaved PCM to 16 kHz mono float32 (`Audio.Resampler`).
pub struct Resampler {
    in_rate: i32,
    channels: i32,
    is_float: bool,
    bits: i32,
    pos: f64,
    mono: Vec<f32>,
}

impl Resampler {
    pub const OUT_RATE: i32 = 16_000;

    pub fn new(fmt: PcmFormat) -> Self {
        Self {
            in_rate: fmt.sample_rate,
            channels: fmt.channels,
            is_float: fmt.is_float,
            bits: fmt.bits_per_sample,
            pos: 0.0,
            mono: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        Pcm::wipe(&mut self.mono);
    }

    /// Appends resampled output for this chunk to `output`. Leftover input carries to the next call.
    pub fn process(&mut self, data: &[u8], frames: usize, silent: bool, output: &mut Vec<f32>) {
        let ch = self.channels.max(1) as usize;
        for f in 0..frames {
            let mut sum = 0.0f32;
            if !silent {
                for c in 0..ch {
                    let i = f * ch + c;
                    let sample = if self.is_float {
                        let off = i * 4;
                        f32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                    } else if self.bits == 16 {
                        let off = i * 2;
                        i16::from_le_bytes([data[off], data[off + 1]]) as f32 / 32768.0
                    } else {
                        let off = i * 4;
                        (i32::from_le_bytes([
                            data[off],
                            data[off + 1],
                            data[off + 2],
                            data[off + 3],
                        ]) >> 8) as f32
                            / 8_388_608.0
                    };
                    sum += sample;
                }
            }
            self.mono.push(sum / ch as f32);
        }
        if self.in_rate == Self::OUT_RATE {
            output.extend_from_slice(&self.mono);
            Pcm::wipe(&mut self.mono);
            return;
        }
        let step = self.in_rate as f64 / Self::OUT_RATE as f64;
        let ratio = step.round() as usize;
        let integer = (step - ratio as f64).abs() < 1e-9 && ratio >= 1;
        loop {
            let i0 = self.pos.floor() as usize;
            if integer {
                if i0 + ratio > self.mono.len() {
                    break;
                }
                let mut s = 0.0f32;
                for k in 0..ratio {
                    s += self.mono[i0 + k];
                }
                output.push(s / ratio as f32);
            } else {
                if i0 + 1 >= self.mono.len() {
                    break;
                }
                let frac = self.pos - i0 as f64;
                let a = self.mono[i0];
                let b = self.mono[i0 + 1];
                output.push((a as f64 + (b - a) as f64 * frac) as f32);
            }
            self.pos += step;
        }
        let consumed = self.mono.len().min(self.pos.floor() as usize);
        Pcm::remove_head(&mut self.mono, consumed);
        self.pos -= consumed as f64;
    }
}

/// One finished speech segment in 16 kHz mono float32.
#[derive(Debug, Clone)]
pub struct SpeechSegment {
    pub samples: Vec<f32>,
    pub start_qpc_100ns: i64,
    pub duration_ms: i32,
    pub app: String,
}

impl SpeechSegment {
    pub fn zeroize_samples(&mut self) {
        self.samples.zeroize();
    }
}

type MaxSegmentFn = Arc<dyn Fn() -> i32 + Send + Sync>;

/// Energy pre-gate and segmenter on 16 kHz mono float: 20 ms frames, adaptive noise floor,
/// segments end after 600 ms of silence or at `max_segment_s`.
pub struct SpeechSegmenter {
    max_segment_s: MaxSegmentFn,
    pending: Vec<f32>,
    pre_roll: VecDeque<Vec<f32>>,
    seg: Option<Vec<f32>>,
    seg_start: i64,
    speech_frames: i32,
    silence_run: i32,
    last_quiet_frame_end: isize,
    noise_db: f64,
    stream_pos: i64,
    stream_start_qpc: i64,
    threshold_db: f64,
}

impl SpeechSegmenter {
    pub const RATE: i32 = 16_000;
    pub const FRAME_SAMPLES: usize = (Self::RATE / 50) as usize; // 320 (20 ms)
    const PRE_ROLL_FRAMES: usize = 10; // 200 ms
    const END_SILENCE_FRAMES: i32 = 30; // 600 ms
    const MIN_SPEECH_FRAMES: i32 = 15; // 300 ms

    pub fn new(max_segment_s: MaxSegmentFn) -> Self {
        Self {
            max_segment_s,
            pending: Vec::new(),
            pre_roll: VecDeque::new(),
            seg: None,
            seg_start: 0,
            speech_frames: 0,
            silence_run: 0,
            last_quiet_frame_end: -1,
            noise_db: -60.0,
            stream_pos: 0,
            stream_start_qpc: i64::MIN,
            threshold_db: -45.0,
        }
    }

    pub fn threshold_db(&self) -> f64 {
        self.threshold_db
    }

    pub fn active(&self) -> bool {
        self.seg.is_some()
    }

    pub fn open_start_qpc(&self) -> Option<i64> {
        if self.seg.is_none() || self.speech_frames < Self::MIN_SPEECH_FRAMES {
            None
        } else {
            Some(self.qpc_at(self.seg_start))
        }
    }

    pub fn reanchor(&mut self) {
        self.stream_start_qpc = i64::MIN;
        Pcm::wipe(&mut self.pending);
    }

    pub fn push(&mut self, samples: &[f32], qpc_100ns: i64, output: &mut Vec<SpeechSegment>) {
        if self.stream_start_qpc == i64::MIN {
            self.stream_start_qpc = qpc_100ns - self.stream_pos * 10_000_000 / Self::RATE as i64;
        }
        let expected = self.stream_start_qpc
            + (self.stream_pos + self.pending.len() as i64) * 10_000_000 / Self::RATE as i64;
        if (qpc_100ns - expected).abs() > 5_000_000 {
            self.flush(output);
            Pcm::wipe(&mut self.pending);
            self.stream_start_qpc = qpc_100ns - self.stream_pos * 10_000_000 / Self::RATE as i64;
        }
        Pcm::append(&mut self.pending, samples);
        let mut offset = 0usize;
        let mut frame = [0.0f32; Self::FRAME_SAMPLES];
        while self.pending.len() - offset >= Self::FRAME_SAMPLES {
            frame.copy_from_slice(&self.pending[offset..offset + Self::FRAME_SAMPLES]);
            offset += Self::FRAME_SAMPLES;
            self.process_frame(&frame, output);
            self.stream_pos += Self::FRAME_SAMPLES as i64;
        }
        Pcm::remove_head(&mut self.pending, offset);
        frame.zeroize();
    }

    fn qpc_at(&self, stream_sample: i64) -> i64 {
        self.stream_start_qpc + stream_sample * 10_000_000 / Self::RATE as i64
    }

    fn process_frame(&mut self, frame: &[f32], output: &mut Vec<SpeechSegment>) {
        let sum: f64 = frame.iter().map(|&v| (v as f64) * (v as f64)).sum();
        let rms = (sum / frame.len() as f64).sqrt();
        let db = 20.0 * rms.max(1e-9).log10();
        let speech = db > (-50.0f64).max(self.noise_db + 12.0);
        self.noise_db = if db < self.noise_db {
            self.noise_db * 0.9 + db * 0.1
        } else {
            self.noise_db * 0.999 + db * 0.001
        };
        self.threshold_db = (-50.0f64).max(self.noise_db + 12.0);

        if self.seg.is_none() {
            self.pre_roll.push_back(frame.to_vec());
            if self.pre_roll.len() > Self::PRE_ROLL_FRAMES
                && let Some(mut old) = self.pre_roll.pop_front()
            {
                Pcm::wipe(&mut old);
            }
            if !speech {
                return;
            }
            let mut seg = Vec::with_capacity((Self::RATE * 4) as usize);
            self.seg_start =
                self.stream_pos - (self.pre_roll.len() as i64 - 1) * Self::FRAME_SAMPLES as i64;
            for f in &self.pre_roll {
                Pcm::append(&mut seg, f);
            }
            self.clear_pre_roll();
            self.seg = Some(seg);
            self.speech_frames = 1;
            self.silence_run = 0;
            self.last_quiet_frame_end = -1;
            return;
        }

        let seg_len = {
            let seg = self.seg.as_mut().unwrap();
            Pcm::append(seg, frame);
            seg.len()
        };
        if speech {
            self.speech_frames += 1;
            self.silence_run = 0;
        } else {
            self.silence_run += 1;
            self.last_quiet_frame_end = seg_len as isize;
        }

        if self.silence_run >= Self::END_SILENCE_FRAMES {
            let drop_frames = (self.silence_run - Self::PRE_ROLL_FRAMES as i32) as usize;
            let keep = seg_len.saturating_sub(drop_frames * Self::FRAME_SAMPLES);
            self.emit(keep.max(Self::FRAME_SAMPLES), output);
            if let Some(mut s) = self.seg.take() {
                Pcm::wipe(&mut s);
            }
            return;
        }

        let max = ((self.max_segment_s)().max(5) * Self::RATE) as usize;
        if seg_len >= max {
            let cut = if self.last_quiet_frame_end > (max * 2 / 3) as isize {
                self.last_quiet_frame_end as usize
            } else {
                seg_len
            };
            self.emit(cut, output);
        }
    }

    fn emit(&mut self, length: usize, output: &mut Vec<SpeechSegment>) {
        let Some(mut seg) = self.seg.take() else {
            return;
        };
        let length = length.min(seg.len());
        if self.speech_frames >= Self::MIN_SPEECH_FRAMES {
            let samples = seg[..length].to_vec();
            output.push(SpeechSegment {
                samples,
                start_qpc_100ns: self.qpc_at(self.seg_start),
                duration_ms: (length as i64 * 1000 / Self::RATE as i64) as i32,
                app: String::new(),
            });
        }
        if length < seg.len() {
            let mut rest = Vec::with_capacity(seg.capacity());
            rest.extend_from_slice(&seg[length..]);
            self.seg_start += length as i64;
            self.seg = Some(rest);
            self.speech_frames = 1;
            self.silence_run = 0;
            self.last_quiet_frame_end = -1;
        } else {
            self.clear_pre_roll();
        }
        Pcm::wipe(&mut seg);
    }

    fn clear_pre_roll(&mut self) {
        while let Some(mut f) = self.pre_roll.pop_front() {
            Pcm::wipe(&mut f);
        }
    }

    pub fn flush(&mut self, output: &mut Vec<SpeechSegment>) {
        if let Some(ref seg) = self.seg {
            let len = seg.len();
            self.emit(len, output);
        }
    }

    pub fn clear(&mut self) {
        if let Some(mut seg) = self.seg.take() {
            Pcm::wipe(&mut seg);
        }
        Pcm::wipe(&mut self.pending);
        self.clear_pre_roll();
    }
}
