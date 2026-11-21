// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Keyframe image codecs: WebP/JPEG via `libSkiaSharp.dll`, AVIF via `avif.dll`, JPEG XL via `jxl.dll` (`Capture\ImageCodec.cs`).

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use zeroize::Zeroize;

use crate::codecs::avif::AvifEncodeParams;
use crate::codecs::{Avif, Jxl, Skia};
use crate::seams::ImageDecoder;
use crate::util::AgentLog;

static AVIF_SPEED: AtomicI32 = AtomicI32::new(7);
static JXL_EFFORT: AtomicI32 = AtomicI32::new(3);
static WARNED_AVIF: AtomicBool = AtomicBool::new(false);
static WARNED_JXL: AtomicBool = AtomicBool::new(false);

/// Keyframe image codecs and format resolution (`ImageCodec.cs`).
pub struct ImageCodec;

impl ImageCodec {
    /// `keyframes.quality` is tuned for WebP; AVIF reaches the same look 25 points lower.
    pub const AVIF_QUALITY_OFFSET: i32 = 25;

    pub fn avif_speed() -> i32 {
        AVIF_SPEED.load(Ordering::Relaxed)
    }

    pub fn set_avif_speed(speed: i32) {
        AVIF_SPEED.store(speed, Ordering::Relaxed);
    }

    pub fn jxl_effort() -> i32 {
        JXL_EFFORT.load(Ordering::Relaxed)
    }

    pub fn set_jxl_effort(effort: i32) {
        JXL_EFFORT.store(effort, Ordering::Relaxed);
    }

    pub fn mime(format: &str) -> &'static str {
        match format {
            "jpeg" => "image/jpeg",
            "avif" => "image/avif",
            "jxl" => "image/jxl",
            "png" => "image/png",
            _ => "image/webp",
        }
    }

    /// Mime types the UI and worker decode directly.
    pub fn commonly_decodable(mime: &str) -> bool {
        matches!(mime, "image/webp" | "image/jpeg" | "image/png")
    }

    /// Loads `avif.dll`, `jxl.dll` + `jxl_threads.dll`, and `libSkiaSharp.dll` before ACG is applied.
    pub fn preload() {
        if !Avif::try_load() {
            AgentLog::warn(&format!(
                "AVIF codec not loaded: {}",
                Avif::load_error().unwrap_or_default()
            ));
        }
        if !Jxl::try_load() {
            AgentLog::warn(&format!(
                "JPEG XL codec not loaded: {}",
                Jxl::load_error().unwrap_or_default()
            ));
        }
        if !Skia::try_load() {
            AgentLog::warn(&format!(
                "SkiaSharp not loaded: {}",
                Skia::load_error().unwrap_or_default()
            ));
        }
    }

    /// Resolves `keyframes.format` to the format actually written (`avif`/`jxl` fall back to `webp`).
    pub fn resolve(format: &str) -> &'static str {
        match format {
            "jpeg" => "jpeg",
            "avif" => {
                if Avif::try_load() {
                    "avif"
                } else {
                    if !WARNED_AVIF.swap(true, Ordering::AcqRel) {
                        AgentLog::warn(&format!(
                            "AVIF unavailable ({}); keyframes are WebP",
                            Avif::load_error().unwrap_or_default()
                        ));
                    }
                    "webp"
                }
            }
            "jxl" => {
                if Jxl::try_load() {
                    "jxl"
                } else {
                    if !WARNED_JXL.swap(true, Ordering::AcqRel) {
                        AgentLog::warn(&format!(
                            "JPEG XL unavailable ({}); keyframes are WebP",
                            Jxl::load_error().unwrap_or_default()
                        ));
                    }
                    "webp"
                }
            }
            _ => "webp",
        }
    }

    /// Encodes BGRA8888 pixels to the requested format (`"avif"`, `"jxl"`, `"jpeg"`, or `"webp"`).
    ///
    /// # Safety
    /// `bgra` must point to at least `height * stride` readable bytes with `width * 4 <= stride`.
    pub unsafe fn encode_bgra(
        bgra: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        format: &str,
        quality: i32,
    ) -> Result<Vec<u8>, String> {
        match format {
            "avif" => unsafe {
                Avif::encode(
                    bgra,
                    AvifEncodeParams {
                        width,
                        height,
                        stride,
                        quality: (quality - Self::AVIF_QUALITY_OFFSET).clamp(0, 100),
                        speed: Self::avif_speed(),
                        threads: 0,
                        tune_content: None,
                    },
                )
            },
            "jxl" => unsafe {
                Jxl::encode(bgra, width, height, stride, quality, Self::jxl_effort())
            },
            "jpeg" => unsafe { Skia::encode_jpeg(bgra, width, height, stride, quality) },
            _ => unsafe { Skia::encode_webp(bgra, width, height, stride, quality) },
        }
    }

    /// Decodes any stored keyframe to tightly packed BGRA (`stride = width * 4`).
    pub fn decode_bgra(encoded: &[u8], mime: &str) -> Result<(Vec<u8>, i32, i32), String> {
        match mime {
            "image/avif" => Avif::decode(encoded),
            "image/jxl" => Jxl::decode(encoded),
            _ => Skia::decode_bgra(encoded),
        }
    }

    /// Decodes, scales to `width` (never up), and encodes lossy WebP.
    pub fn resize_to_webp(
        encoded: &[u8],
        mime: &str,
        width: i32,
        quality: i32,
    ) -> Result<(Vec<u8>, i32, i32), String> {
        let (mut px, w0, h0) = Self::decode_bgra(encoded, mime)?;
        let w = width.min(w0);
        let h = ((h0 as f64) * (w as f64) / (w0 as f64)).round().max(1.0) as i32;
        
        if w != w0 {
            let mut scaled = unsafe { Skia::scale_bgra(px.as_ptr(), w0, h0, w0 * 4, w, h, true) };
            px.zeroize();
            match scaled {
                Ok(ref mut s_px) => {
                    let enc = unsafe { Skia::encode_webp(s_px.as_ptr(), w, h, w * 4, quality) };
                    s_px.zeroize();
                    enc.map(|b| (b, w, h))
                }
                Err(e) => Err(e),
            }
        } else {
            let enc = unsafe { Skia::encode_webp(px.as_ptr(), w0, h0, w0 * 4, quality) };
            px.zeroize();
            enc.map(|b| (b, w0, h0))
        }
    }

    /// Re-encodes a keyframe as `"webp"` or `"jpeg"` at its original size.
    pub fn transcode(
        encoded: &[u8],
        mime: &str,
        to_format: &str,
        quality: i32,
    ) -> Result<(Vec<u8>, &'static str, i32, i32), String> {
        let (mut px, w, h) = Self::decode_bgra(encoded, mime)?;
        let enc = unsafe { Self::encode_bgra(px.as_ptr(), w, h, w * 4, to_format, quality) };
        px.zeroize();
        enc.map(|bytes| (bytes, Self::mime(to_format), w, h))
    }
}

/// Real [`ImageDecoder`] backed by [`ImageCodec::decode_bgra`], wired into `EmbeddingScheduler`.
pub struct ImageCodecDecoder;

impl ImageDecoder for ImageCodecDecoder {
    fn decode_bgra(&self, bytes: &[u8], mime: &str) -> Result<(Vec<u8>, i32, i32), String> {
        ImageCodec::decode_bgra(bytes, mime)
    }
}
