// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::Path;

use crate::inference::{DeviceKind, HostTensor, IInferenceBackend, IInferenceSession, TensorSpec};
use crate::protocol::WorkerError;

/// Silero VAD v5/v6 ONNX on the CPU (16 kHz): 512-sample windows with the previous 64 samples
/// prepended, recurrent state `[2,1,128]`. Segment rules follow silero-vad's `get_speech_timestamps`.
pub struct SileroVad {
    session: Box<dyn IInferenceSession>,
    in_name: String,
    state_name: String,
    sr_name: String,
    out_name: String,
    state_out_name: String,
    pub threshold: f32,
    pub min_speech_ms: i32,
    pub min_silence_ms: i32,
    pub speech_pad_ms: i32,
}

impl SileroVad {
    pub const WINDOW: usize = 512;
    pub const CONTEXT: usize = 64;
    pub const STATE_SIZE: usize = 2 * 128;

    pub fn new(cpu_backend: &dyn IInferenceBackend, model_path: &Path) -> Result<Self, WorkerError> {
        let cpu = cpu_backend
            .devices()
            .iter()
            .find(|d| d.kind == DeviceKind::Cpu)
            .ok_or_else(|| WorkerError::DeviceFailed("no CPU device for VAD".into()))?;
        let session = cpu_backend.load(model_path, cpu, None, None)?;
        let find = |list: &[TensorSpec], key: &str, fallback: usize| -> Result<String, WorkerError> {
            list.iter()
                .find(|s| s.name.eq_ignore_ascii_case(key))
                .or_else(|| list.get(fallback))
                .map(|s| s.name.clone())
                .ok_or_else(|| WorkerError::Internal("unexpected VAD graph".into()))
        };
        let in_name = find(session.inputs(), "input", 0)?;
        let state_name = find(session.inputs(), "state", 1)?;
        let sr_name = find(session.inputs(), "sr", 2)?;
        let out_name = find(session.outputs(), "output", 0)?;
        let state_out_name = find(session.outputs(), "stateN", 1)?;
        Ok(Self {
            session,
            in_name,
            state_name,
            sr_name,
            out_name,
            state_out_name,
            threshold: 0.5,
            min_speech_ms: 250,
            min_silence_ms: 100,
            speech_pad_ms: 30,
        })
    }

    /// Speech probability per 512-sample window (32 ms).
    pub fn probabilities(&self, pcm: &[f32]) -> Result<Vec<f32>, WorkerError> {
        let windows = pcm.len().div_ceil(Self::WINDOW);
        let mut probs = vec![0.0f32; windows];
        let mut state = vec![0.0f32; Self::STATE_SIZE];
        let mut buf = vec![0.0f32; Self::CONTEXT + Self::WINDOW];

        for (w, slot) in probs.iter_mut().enumerate() {
            let start = w * Self::WINDOW;
            let n = Self::WINDOW.min(pcm.len() - start);
            buf[Self::CONTEXT..].fill(0.0);
            buf[Self::CONTEXT..Self::CONTEXT + n].copy_from_slice(&pcm[start..start + n]);

            // `sr` is a 0-D INT64 scalar tensor (`shape = []`), matching `SileroVad.cs` and `vad.onnxt`.
            let outp = self.session.run(&[
                HostTensor::f(
                    self.in_name.clone(),
                    buf.clone(),
                    &[1, (Self::CONTEXT + Self::WINDOW) as i64],
                ),
                HostTensor::f(self.state_name.clone(), state, &[2, 1, 128]),
                HostTensor::l(self.sr_name.clone(), vec![16_000], &[]),
            ])?;
            let p = outp
                .get(&self.out_name)
                .and_then(|t| t.floats.as_deref())
                .filter(|f| !f.is_empty())
                .ok_or_else(|| WorkerError::DeviceFailed("VAD output missing".into()))?;
            let s = outp
                .get(&self.state_out_name)
                .and_then(|t| t.floats.as_ref())
                .ok_or_else(|| WorkerError::DeviceFailed("VAD output missing".into()))?;
            *slot = p[0];
            state = s.clone();
            buf.copy_within(Self::WINDOW..Self::WINDOW + Self::CONTEXT, 0);
        }
        Ok(probs)
    }

    pub fn segments(&self, pcm: &[f32]) -> Result<Vec<(i32, i32)>, WorkerError> {
        let probs = self.probabilities(pcm)?;
        Ok(self.segment(&probs, pcm.len()))
    }

    /// `get_speech_timestamps`: enter at `threshold`, leave below `threshold - 0.15` after
    /// `min_silence_ms`, drop segments shorter than `min_speech_ms`, pad by `speech_pad_ms`.
    pub fn segment(&self, probs: &[f32], total_samples: usize) -> Vec<(i32, i32)> {
        Self::segment_probs(
            probs,
            total_samples,
            self.threshold,
            self.min_speech_ms,
            self.min_silence_ms,
            self.speech_pad_ms,
        )
    }

    pub fn segment_probs(
        probs: &[f32],
        total_samples: usize,
        threshold: f32,
        min_speech_ms: i32,
        min_silence_ms: i32,
        speech_pad_ms: i32,
    ) -> Vec<(i32, i32)> {
        const SR: i32 = 16_000;
        let total = total_samples as i32;
        let neg = (threshold - 0.15).max(0.01);
        let min_speech = SR * min_speech_ms / 1000;
        let min_silence = SR * min_silence_ms / 1000;
        let pad = SR * speech_pad_ms / 1000;

        let mut raw = Vec::new();
        let mut triggered = false;
        let mut start = 0i32;
        let mut temp_end = 0i32;
        for (i, &p) in probs.iter().enumerate() {
            let cur = (i * Self::WINDOW) as i32;
            if p >= threshold && temp_end != 0 {
                temp_end = 0;
            }
            if p >= threshold && !triggered {
                triggered = true;
                start = cur;
                continue;
            }
            if p < neg && triggered {
                if temp_end == 0 {
                    temp_end = cur;
                }
                if cur - temp_end < min_silence {
                    continue;
                }
                if temp_end - start > min_speech {
                    raw.push((start, temp_end));
                }
                triggered = false;
                temp_end = 0;
            }
        }
        if triggered && total - start > min_speech {
            raw.push((start, total));
        }

        raw.into_iter()
            .map(|(s, e)| ((s - pad).max(0) * 1000 / SR, total.min(e + pad) * 1000 / SR))
            .collect()
    }
}
