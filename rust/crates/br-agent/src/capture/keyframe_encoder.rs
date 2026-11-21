// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Keyframe encoding at one [`EncodeLevel`] on the calling thread (`Capture\KeyframeEncoder.cs`).

use br_contracts::Settings;
use zeroize::Zeroize;

use crate::capture::encode_governor::EncodeLevel;
use crate::capture::image_codec::ImageCodec;
use crate::codecs::avif::AvifEncodeParams;
use crate::codecs::{Avif, Skia};
use crate::util::AgentLog;

/// Keyframe encoder using single-threaded libaom for AVIF so CPU usage is measured on the calling thread.
pub struct KeyframeEncoder;

impl KeyframeEncoder {
    pub const NORMAL_SPEED: i32 = 7;
    pub const FAST_SPEED: i32 = 8;
    pub const REDUCED_SPEED: i32 = 9;
    pub const REDUCED_SCALE: f64 = 0.75;
    pub const REDUCED_QUALITY_DROP: i32 = 10;
    pub const TUNE_CONTENT: Option<&'static str> = None;

    /// Expected CPU ms per level for a 1280 px frame before real encodes are measured.
    pub const INITIAL_ESTIMATE_MS: [f64; 4] = [130.0, 65.0, 15.0, 35.0];

    pub fn describe(format: &str, level: EncodeLevel) -> String {
        match (format, level) {
            (_, EncodeLevel::WebP) => "WebP (CPU budget fallback)".to_string(),
            ("avif", EncodeLevel::Normal) => format!("libaom speed {}", Self::NORMAL_SPEED),
            ("avif", EncodeLevel::Fast) => {
                format!("libaom speed {} (CPU budget)", Self::FAST_SPEED)
            }
            ("avif", EncodeLevel::Reduced) => format!(
                "libaom speed {}, {:.0}% size (CPU budget)",
                Self::REDUCED_SPEED,
                Self::REDUCED_SCALE * 100.0
            ),
            (_, EncodeLevel::Reduced) => {
                format!("{format}, {:.0}% size (CPU budget)", Self::REDUCED_SCALE * 100.0)
            }
            _ => format.to_string(),
        }
    }

    /// Encodes BGRA pixels at `level`, returning `(bytes, format, width, height)`.
    ///
    /// # Safety
    /// `bgra` must point to at least `height * stride` readable bytes with `width * 4 <= stride`.
    pub unsafe fn encode(
        bgra: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        settings: &Settings,
        level: EncodeLevel,
    ) -> Result<(Vec<u8>, &'static str, i32, i32), String> {
        let quality = settings.keyframes.quality.clamp(30, 95);
        let format = if level == EncodeLevel::WebP {
            "webp"
        } else {
            ImageCodec::resolve(&settings.keyframes.format)
        };
        if level != EncodeLevel::Reduced {
            let speed = if level == EncodeLevel::Fast {
                Self::FAST_SPEED
            } else {
                Self::NORMAL_SPEED
            };
            let bytes = unsafe { encode_at(bgra, width, height, stride, format, quality, speed) }?;
            return Ok((bytes, format, width, height));
        }

        let w = (((width as f64 * Self::REDUCED_SCALE) as i32) & !1).max(2);
        let h = (((height as f64 * Self::REDUCED_SCALE) as i32) & !1).max(2);
        let mut small = unsafe { Skia::scale_bgra(bgra, width, height, stride, w, h, false) }?;
        let res = unsafe {
            encode_at(
                small.as_ptr(),
                w,
                h,
                w * 4,
                format,
                (quality - Self::REDUCED_QUALITY_DROP).max(30),
                Self::REDUCED_SPEED,
            )
        };
        small.zeroize();
        res.map(|bytes| (bytes, format, w, h))
    }

    pub fn log_change(last: &mut String, now: &str) {
        if now == last.as_str() {
            return;
        }
        AgentLog::info(&format!("keyframe encoder: {now}"));
        last.clear();
        last.push_str(now);
    }
}

unsafe fn encode_at(
    bgra: *const u8,
    w: i32,
    h: i32,
    stride: i32,
    format: &str,
    quality: i32,
    speed: i32,
) -> Result<Vec<u8>, String> {
    if format == "avif" {
        unsafe {
            Avif::encode(
                bgra,
                AvifEncodeParams {
                    width: w,
                    height: h,
                    stride,
                    quality: (quality - ImageCodec::AVIF_QUALITY_OFFSET).clamp(0, 100),
                    speed,
                    threads: 1,
                    tune_content: KeyframeEncoder::TUNE_CONTENT,
                },
            )
        }
    } else {
        unsafe { ImageCodec::encode_bgra(bgra, w, h, stride, format, quality) }
    }
}
