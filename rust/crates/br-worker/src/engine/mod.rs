// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod fake;
pub mod real;

use std::path::Path;

use br_contracts::{BenchResult, OcrResult, TranscribeResult, WorkerHello};

use crate::hosting::WorkerSettings;
use crate::protocol::WorkerError;

pub use fake::FakeEngine;
pub use real::RealEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Query,
    Document,
}

/// Decoded request image before preprocessing. Bytes are the request payload.
#[derive(Debug, Clone, Copy)]
pub struct ImageInput<'a> {
    pub format: &'a str,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub bytes: &'a [u8],
}

/// Vector at the model's native dimension (768). `None` when the NSFW gate dropped the image.
#[derive(Debug, Clone)]
pub struct ImageEmbedding {
    pub vector: Option<Vec<f32>>,
    pub dropped: Option<String>,
}

/// Identity of the embedder that served a request (sweep 4 A-M3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedderIdentity {
    pub model: String,
    pub revision: String,
    pub device: String,
}

/// Inference interface used by `RequestHandler`. Calls are serialized by `WorkerLoop`.
pub trait IWorkerEngine: Sync {
    fn hello(&self, dim: i32) -> Result<WorkerHello, WorkerError>;

    fn embed_image(
        &self,
        image: &ImageInput<'_>,
        nsfw_threshold: Option<f64>,
        nsfw_grid: i32,
        nsfw_detector: Option<f64>,
    ) -> Result<ImageEmbedding, WorkerError>;

    fn embed_text(&self, text: &str, kind: TextKind) -> Result<Vec<f32>, WorkerError>;

    fn transcribe(
        &self,
        pcm_16k_mono: &[f32],
        source: &str,
        language: &str,
    ) -> Result<TranscribeResult, WorkerError>;

    fn bench(&self) -> Result<BenchResult, WorkerError>;

    fn ocr(&self, image: &ImageInput<'_>, entities: bool) -> Result<OcrResult, WorkerError>;

    fn embedder(&self) -> Option<EmbedderIdentity> {
        None
    }
}

/// Factory returning `FakeEngine` when `settings.fake` is true, else `RealEngine`.
pub struct EngineFactory;

impl EngineFactory {
    pub fn create(settings: &WorkerSettings, home: &Path) -> Box<dyn IWorkerEngine> {
        if settings.fake {
            Box::new(FakeEngine)
        } else {
            Box::new(RealEngine::new(settings.clone(), home.to_path_buf()))
        }
    }
}
