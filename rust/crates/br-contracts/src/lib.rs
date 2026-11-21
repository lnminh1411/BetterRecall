// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

//! Wire types, binary pipe framing, worker settings schema, data paths, and
//! Whisper language table shared with the BetterRecall worker process.

pub mod framing;
pub mod layout;
pub mod models;
pub mod settings;
pub mod whisper_languages;

pub use framing::{BinaryFrame, FrameError};
pub use layout::{DataPaths, InstallLayout, PipeNames, full_path};
pub use models::{
    BenchDevice, BenchResult, OcrEntity, OcrLine, OcrResult, TokenBench, TranscribeResult,
    TranscriptSegment, WorkerHello, WorkerRequest, WorkerResponse,
};
pub use settings::{
    AudioSettings, GovernanceSettings, IndexSettings, InferenceSettings, LogSettings, OcrSettings,
    PrivacySettings, SecuritySettings, Settings,
};
pub use whisper_languages::WhisperLanguages;
