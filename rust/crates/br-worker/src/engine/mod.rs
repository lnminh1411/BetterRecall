// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod fake;

use std::path::Path;

use br_contracts::{BenchResult, OcrResult, TranscribeResult, WorkerHello};

use crate::hosting::WorkerSettings;
use crate::protocol::WorkerError;

pub use fake::FakeEngine;

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

/// Factory returning `FakeEngine` when `settings.fake` is true, else the W1 real-engine stub.
pub struct EngineFactory;

impl EngineFactory {
    pub fn create(settings: &WorkerSettings, _home: &Path) -> Box<dyn IWorkerEngine> {
        if settings.fake {
            Box::new(FakeEngine)
        } else {
            // TODO(W2): instantiate RealEngine with OpenVINO / ONNX Runtime backends.
            Box::new(UnimplementedRealEngine {
                ocr_engine: settings.ocr_engine.clone(),
            })
        }
    }
}

/// Model names matching `ModelCatalog` in C# (`hello.missing` and `Model not installed: <name>.`).
const MISSING_MODELS_ALL: [&str; 8] = [
    "embeddinggemma-2",
    "siglip2-base",
    "whisper-large-v3-turbo",
    "silero-vad",
    "nsfw-classifier",
    "nsfw-detector",
    "ocr",
    "ner",
];

/// W1 placeholder for `RealEngine`: returns the exact same errors and empty-revision `hello`
/// that the C# `RealEngine` returns when no models are installed, until W2/W3 wire real inference.
pub struct UnimplementedRealEngine {
    ocr_engine: String,
}

impl IWorkerEngine for UnimplementedRealEngine {
    fn hello(&self, dim: i32) -> Result<WorkerHello, WorkerError> {
        // TODO(W2): probe ModelCatalog, load embedder, preload NSFW gate, and report runtime devices.
        // TODO(W3): preload OCR engine when ocr.enabled is true.
        Ok(WorkerHello {
            protocol: 1,
            device: "CPU".to_string(),
            ep: "CPU".to_string(),
            model: "google/embeddinggemma-2".to_string(),
            revision: String::new(),
            dim,
            asr: false,
            vad: false,
            missing: MISSING_MODELS_ALL.iter().map(|&s| s.to_string()).collect(),
            nsfw_device: None,
            ocr_device: None,
        })
    }

    fn embed_image(
        &self,
        _image: &ImageInput<'_>,
        nsfw_threshold: Option<f64>,
        _nsfw_grid: i32,
        nsfw_detector: Option<f64>,
    ) -> Result<ImageEmbedding, WorkerError> {
        // TODO(W2): decode image, run NSFW gate if requested, and run image embedder.
        if nsfw_threshold.is_some() || nsfw_detector.is_some() {
            return Err(WorkerError::ModelNotInstalled("nsfw-classifier".to_string()));
        }
        Err(WorkerError::ModelNotInstalled(
            "embeddinggemma-2".to_string(),
        ))
    }

    fn embed_text(&self, _text: &str, _kind: TextKind) -> Result<Vec<f32>, WorkerError> {
        // TODO(W2): tokenize text and run text embedder.
        Err(WorkerError::ModelNotInstalled(
            "embeddinggemma-2".to_string(),
        ))
    }

    fn transcribe(
        &self,
        _pcm_16k_mono: &[f32],
        _source: &str,
        _language: &str,
    ) -> Result<TranscribeResult, WorkerError> {
        // TODO(W3): run Silero VAD + Whisper OpenVINO GenAI pipeline.
        Err(WorkerError::ModelNotInstalled("silero-vad".to_string()))
    }

    fn bench(&self) -> Result<BenchResult, WorkerError> {
        // TODO(W2): benchmark available inference devices and write cache\device.json.
        Err(WorkerError::ModelNotInstalled(
            "embeddinggemma-2".to_string(),
        ))
    }

    fn ocr(&self, _image: &ImageInput<'_>, _entities: bool) -> Result<OcrResult, WorkerError> {
        // TODO(W3): decode image and run PaddleOCR / Windows OCR + optional PII NER.
        if self.ocr_engine == "windows" {
            Err(WorkerError::ModelNotInstalled("windows-ocr".to_string()))
        } else {
            Err(WorkerError::ModelNotInstalled("ocr".to_string()))
        }
    }
}
