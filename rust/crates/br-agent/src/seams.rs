// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Explicit seams for stage A5/A6 subsystems.
//! In stage A5, capture, keyframes, image decoding, thumbnails and describe redaction are wired
//! with real implementations; replay and audio remain behind `Unavailable*` seams for stage A6.

use std::path::Path;

use br_contracts::{AudioState, ReplayClip, Settings};
use br_core::{AgentError, CancelToken};

use crate::native::Rect;

// ---------------- A5 traits (implemented by CaptureService, KeyframePipeline, ImageCodecDecoder) ----------------

/// Trait surface for `CaptureService`.
pub trait CaptureSubsystem: Send + Sync {
    fn start(&self);
    fn apply_settings(&self);
    fn rebuild(&self);
    fn retry_all(&self);
    fn on_foreground_changed(&self);
    fn forget_recent(&self);
    fn panic(&self);
    fn monitor_bounds(&self) -> Vec<(i32, Rect)>;
    fn replay_text_unchecked(&self) -> bool;
    fn replay_active(&self) -> bool;
    fn replay_last_error(&self) -> Option<String>;
    fn diagnostics(&self) -> Option<String>;
    fn stop(&self);
}

/// Trait surface for `KeyframePipeline`.
pub trait KeyframeSubsystem: Send + Sync {
    fn mark_all_unchecked(&self);
    fn ocr_failing(&self) -> bool;
    fn entities_missing(&self) -> bool;
    fn budget_status(&self) -> Option<String>;
    fn diagnostics(&self) -> Option<String>;
    fn stop(&self);
}

/// Formats the worker can decode directly (`ImageCodec.CommonlyDecodable`).
pub fn commonly_decodable(mime: &str) -> bool {
    matches!(mime, "image/webp" | "image/jpeg" | "image/png")
}

/// Trait surface for `ImageCodec::decode_bgra` (consumed by `EmbeddingScheduler`).
pub trait ImageDecoder: Send + Sync {
    fn decode_bgra(&self, bytes: &[u8], mime: &str) -> Result<(Vec<u8>, i32, i32), String>;
}

/// Fallback decoder for unit tests that do not exercise AVIF/JXL decoding.
pub struct UnavailableImageDecoder;

impl ImageDecoder for UnavailableImageDecoder {
    fn decode_bgra(&self, _bytes: &[u8], mime: &str) -> Result<(Vec<u8>, i32, i32), String> {
        Err(format!("Image codec not available for {mime}."))
    }
}

// ---------------- A6: Replay ----------------

/// Seam for `ReplayLibrary` / `ReplayEngine` / `ReplaySampler` (stage A6).
pub trait ReplaySubsystem: Send + Sync {
    fn delete_partials(&self);
    fn save_replay(&self, ct: &CancelToken) -> Result<ReplayClip, AgentError>;
    fn list_replays(&self) -> Vec<ReplayClip>;
    fn export_replay(&self, id: &str, path: &Path, ct: &CancelToken) -> Result<(), AgentError>;
    fn delete_replay(&self, id: &str) -> Result<(), AgentError>;
    fn on_purge_all(&self);
    fn discard_saves_in_flight(&self);
    fn clear_sampler(&self);
}

/// A4 placeholder: replay is not wired yet.
pub struct UnavailableReplay;

impl ReplaySubsystem for UnavailableReplay {
    fn delete_partials(&self) {}

    fn save_replay(&self, _ct: &CancelToken) -> Result<ReplayClip, AgentError> {
        Err(AgentError::InvalidOperation(
            "Replay is not running.".to_string(),
        ))
    }

    fn list_replays(&self) -> Vec<ReplayClip> {
        Vec::new()
    }

    fn export_replay(&self, _id: &str, _path: &Path, _ct: &CancelToken) -> Result<(), AgentError> {
        Err(AgentError::NotFound)
    }

    fn delete_replay(&self, _id: &str) -> Result<(), AgentError> {
        Err(AgentError::NotFound)
    }

    fn on_purge_all(&self) {}
    fn discard_saves_in_flight(&self) {}
    fn clear_sampler(&self) {}
}

// ---------------- A6: Audio ----------------

/// Seam for `LoopbackHub`, `TranscriptSource`, and `Transcriber` (stage A6).
pub trait AudioSubsystem: Send + Sync {
    fn sync_sources(&self, settings: &Settings, capture_started: bool);
    fn needs_sync(&self, settings: &Settings, capture_started: bool) -> bool;
    fn check_health(&self, now_ms: i64);
    fn flush(&self);
    fn clear(&self);
    fn flush_held(&self);
    fn state(&self) -> AudioState;
    fn stop(&self);
}

/// A4 placeholder: audio capture and transcription are not wired yet.
pub struct UnavailableAudio;

impl AudioSubsystem for UnavailableAudio {
    fn sync_sources(&self, _settings: &Settings, _capture_started: bool) {}
    fn needs_sync(&self, _settings: &Settings, _capture_started: bool) -> bool {
        false
    }
    fn check_health(&self, _now_ms: i64) {}
    fn flush(&self) {}
    fn clear(&self) {}
    fn flush_held(&self) {}
    fn state(&self) -> AudioState {
        AudioState {
            system: false,
            mic: false,
        }
    }
    fn stop(&self) {}
}
