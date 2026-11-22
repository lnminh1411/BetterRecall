// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Subsystem trait surfaces for `CaptureService`, `KeyframePipeline`, and `ImageDecoder`.
//! Replay and audio seams from A4/A5 were replaced by real implementations in stage A6.

use crate::native::Rect;

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
