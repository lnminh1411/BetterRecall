// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Explicit seams for stages A5 (capture, keyframes, thumbnails, image decode) and A6 (replay, audio).
//! Each subsystem has a trait plus an "unavailable" implementation so the A4 agent reports them as off
//! in the same way the C# agent does when their settings are off.

use std::path::Path;

use br_contracts::{AudioState, ReplayClip, Settings, ThumbData};
use br_core::{AgentError, CancelToken, Thumbnailer};

use crate::native::Rect;

// ---------------- A5: Capture ----------------

/// Seam for `CaptureService` (stage A5).
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

/// A4 placeholder: capture is not wired yet.
pub struct UnavailableCapture;

impl CaptureSubsystem for UnavailableCapture {
    fn start(&self) {}
    fn apply_settings(&self) {}
    fn rebuild(&self) {}
    fn retry_all(&self) {}
    fn on_foreground_changed(&self) {}
    fn forget_recent(&self) {}
    fn panic(&self) {}
    fn monitor_bounds(&self) -> Vec<(i32, Rect)> {
        Vec::new()
    }
    fn replay_text_unchecked(&self) -> bool {
        false
    }
    fn replay_active(&self) -> bool {
        false
    }
    fn replay_last_error(&self) -> Option<String> {
        None
    }
    fn diagnostics(&self) -> Option<String> {
        None
    }
    fn stop(&self) {}
}

// ---------------- A5: Keyframes ----------------

/// Seam for `KeyframePipeline` (stage A5).
pub trait KeyframeSubsystem: Send + Sync {
    fn mark_all_unchecked(&self);
    fn ocr_failing(&self) -> bool;
    fn entities_missing(&self) -> bool;
    fn budget_status(&self) -> Option<String>;
    fn diagnostics(&self) -> Option<String>;
    fn stop(&self);
}

/// A4 placeholder: keyframe pipeline is not wired yet.
pub struct UnavailableKeyframes;

impl KeyframeSubsystem for UnavailableKeyframes {
    fn mark_all_unchecked(&self) {}
    fn ocr_failing(&self) -> bool {
        false
    }
    fn entities_missing(&self) -> bool {
        false
    }
    fn budget_status(&self) -> Option<String> {
        None
    }
    fn diagnostics(&self) -> Option<String> {
        None
    }
    fn stop(&self) {}
}

// ---------------- A5: Image decode & Thumbnailer ----------------

/// Formats the worker can decode directly (`ImageCodec.CommonlyDecodable`).
pub fn commonly_decodable(mime: &str) -> bool {
    matches!(mime, "image/webp" | "image/jpeg" | "image/png")
}

/// Seam for `ImageCodec::DecodeBgra` (AVIF / JPEG XL decoding in stage A5).
pub trait ImageDecoder: Send + Sync {
    fn decode_bgra(&self, bytes: &[u8], mime: &str) -> Result<(Vec<u8>, i32, i32), String>;
}

/// A4 placeholder: AVIF/JXL decoding is not wired yet.
pub struct UnavailableImageDecoder;

impl ImageDecoder for UnavailableImageDecoder {
    fn decode_bgra(&self, _bytes: &[u8], mime: &str) -> Result<(Vec<u8>, i32, i32), String> {
        Err(format!("Image codec not available for {mime}."))
    }
}

/// A4 placeholder for `br_core::Thumbnailer` (stage A5 wires real WebP resize + LRU cache).
/// Returns `None` for `get_thumb` so `CommandProcessor` replies `"Not found."` when no image exists.
pub struct UnavailableThumbnailer;

impl Thumbnailer for UnavailableThumbnailer {
    fn get_thumb(&self, _id: &str, _width: i32, _ct: &CancelToken) -> Option<ThumbData> {
        None
    }

    fn clear_cache(&self) {}

    fn transcode(&self, keyframe: ThumbData, _target: &str) -> ThumbData {
        keyframe
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
