// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::time::Instant;

use br_contracts::{
    BenchDevice, BenchResult, OcrLine, OcrResult, TranscribeResult, TranscriptSegment, WorkerHello,
};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::engine::{EmbedderIdentity, IWorkerEngine, ImageEmbedding, ImageInput, TextKind};
use crate::protocol::WorkerError;
use crate::vectors::VectorMath;

/// `inference.fake`: no model files, deterministic output.
/// Text: bag of hashed lower-case words plus a small kind-specific offset.
/// Images: hash of the pixels' bytes and shape.
/// Transcribe: echo of size and source, `speech` when RMS > 1e-3.
pub struct FakeEngine;

impl FakeEngine {
    pub const NATIVE_DIM: usize = 768;
    pub const MODEL_ID: &'static str = "fake/embeddinggemma-2";
    pub const REVISION: &'static str = "fake-1";
    const KIND_WEIGHT: f32 = 0.12;

    /// Splits NFKC-normalized text into lower-cased alphanumeric word runs.
    pub fn words(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        for ch in text.nfkc() {
            if ch.is_alphanumeric() {
                for lc in ch.to_lowercase() {
                    cur.push(lc);
                }
            } else if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// 768 floats in `[-1, 1)` from a seed, via SHA-256 in counter mode.
    pub fn hash_vector(seed: &[u8]) -> Vec<f32> {
        let mut v = vec![0.0f32; Self::NATIVE_DIM];
        let mut block = Vec::with_capacity(seed.len() + 4);
        block.extend_from_slice(seed);
        block.resize(seed.len() + 4, 0);
        let mut filled = 0usize;
        let mut ctr = 0u32;
        while filled < v.len() {
            block[seed.len()..].copy_from_slice(&ctr.to_le_bytes());
            let d = Sha256::digest(&block);
            let mut j = 0;
            while j + 1 < d.len() && filled < v.len() {
                let sample = i16::from_le_bytes([d[j], d[j + 1]]) as f32 / 32768.0;
                v[filled] = sample;
                filled += 1;
                j += 2;
            }
            ctr += 1;
        }
        v
    }
}

impl IWorkerEngine for FakeEngine {
    fn hello(&self, dim: i32) -> Result<WorkerHello, WorkerError> {
        Ok(WorkerHello {
            protocol: 1,
            device: "CPU".to_string(),
            ep: "fake".to_string(),
            model: Self::MODEL_ID.to_string(),
            revision: Self::REVISION.to_string(),
            dim,
            asr: true,
            vad: true,
            missing: Vec::new(),
            nsfw_device: None,
            ocr_device: None,
        })
    }

    fn embed_image(
        &self,
        image: &ImageInput<'_>,
        _nsfw_threshold: Option<f64>,
        _nsfw_grid: i32,
        _nsfw_detector: Option<f64>,
    ) -> Result<ImageEmbedding, WorkerError> {
        let mut h = Sha256::new();
        h.update(format!("img|{}|{}x{}|", image.format, image.width, image.height).as_bytes());
        h.update(image.bytes);
        let mut v = Self::hash_vector(&h.finalize());
        VectorMath::l2_normalize_in_place(&mut v);
        Ok(ImageEmbedding {
            vector: Some(v),
            dropped: None,
        })
    }

    fn embed_text(&self, text: &str, kind: TextKind) -> Result<Vec<f32>, WorkerError> {
        let mut v = vec![0.0f32; Self::NATIVE_DIM];
        let mut words = 0usize;
        for w in Self::words(text) {
            let w_seed = Sha256::digest(format!("w|{w}").as_bytes());
            let wv = Self::hash_vector(&w_seed);
            for (acc, &val) in v.iter_mut().zip(wv.iter()) {
                *acc += val;
            }
            words += 1;
        }
        if words == 0 {
            v = Self::hash_vector(&Sha256::digest(b"empty"));
        }
        VectorMath::l2_normalize_in_place(&mut v);

        let kind_seed = match kind {
            TextKind::Query => b"kind|query".as_slice(),
            TextKind::Document => b"kind|document".as_slice(),
        };
        let mut k = Self::hash_vector(&Sha256::digest(kind_seed));
        VectorMath::l2_normalize_in_place(&mut k);
        for (acc, &kv) in v.iter_mut().zip(k.iter()) {
            *acc += Self::KIND_WEIGHT * kv;
        }
        VectorMath::l2_normalize_in_place(&mut v);
        Ok(v)
    }

    fn transcribe(
        &self,
        pcm: &[f32],
        source: &str,
        language: &str,
    ) -> Result<TranscribeResult, WorkerError> {
        let sw = Instant::now();
        let sum: f64 = pcm.iter().map(|&x| (x as f64) * (x as f64)).sum();
        let rms = if pcm.is_empty() {
            0.0
        } else {
            (sum / pcm.len() as f64).sqrt()
        };
        let ms = ((pcm.len() as i64) * 1000 / 16_000) as i32;
        let lang = if language == "auto" {
            "en".to_string()
        } else {
            language.to_string()
        };
        if rms <= 1e-3 {
            return Ok(TranscribeResult {
                speech: false,
                text: String::new(),
                language: lang,
                segments: Vec::new(),
                ms: sw.elapsed().as_millis() as i64,
            });
        }
        let mut hasher = Sha256::new();
        for &x in pcm {
            hasher.update(x.to_le_bytes());
        }
        let digest = hasher.finalize();
        let hash = format!(
            "{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3]
        );
        let text = format!("fake transcript {source} {ms} ms {hash}");
        Ok(TranscribeResult {
            speech: true,
            text: text.clone(),
            language: lang,
            segments: vec![TranscriptSegment {
                start_ms: 0,
                end_ms: ms,
                text,
            }],
            ms: sw.elapsed().as_millis() as i64,
        })
    }

    fn bench(&self) -> Result<BenchResult, WorkerError> {
        Ok(BenchResult {
            chosen: "CPU".to_string(),
            devices: vec![BenchDevice {
                device: "CPU".to_string(),
                ep: "fake".to_string(),
                ok: true,
                image_ms: 0.0,
                text_ms: 0.0,
                cosine: 1.0,
                error: None,
                model: None,
                text_device: None,
                compile_ms: None,
                adapter: None,
                adapter_auto: false,
            }],
            tokens: None,
            chosen_adapter: None,
        })
    }

    fn embedder(&self) -> Option<EmbedderIdentity> {
        Some(EmbedderIdentity {
            model: Self::MODEL_ID.to_string(),
            revision: Self::REVISION.to_string(),
            device: "CPU".to_string(),
        })
    }

    /// One line `"fake ocr WxH"` at the top left, 10 px per character; entities: none.
    fn ocr(&self, image: &ImageInput<'_>, entities: bool) -> Result<OcrResult, WorkerError> {
        let text = format!("fake ocr {}x{}", image.width, image.height);
        let len = text.chars().count() as i32;
        let cuts: Vec<i32> = (0..=len).map(|i| 8 + i * 10).collect();
        Ok(OcrResult {
            lines: vec![OcrLine {
                text,
                x: 8,
                y: 8,
                w: len * 10,
                h: 16,
                confidence: 1.0,
                cuts,
                entities: if entities { Some(Vec::new()) } else { None },
            }],
            engine: "fake".to_string(),
            ms: 0.0,
            entities_missing: false,
        })
    }
}
