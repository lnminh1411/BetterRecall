// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::io::Cursor;

use image::{ImageFormat, ImageReader};

use crate::engine::ImageInput;
use crate::models::manifest::{ImagePreprocessSpec, VisionGraph};
use crate::protocol::WorkerError;

/// 8-bit RGB, row-major, no padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbImage {
    pub width: i32,
    pub height: i32,
    pub rgb: Vec<u8>,
}

/// Patch form tensors for a Gemma 4 / EmbeddingGemma 2 vision pass.
#[derive(Debug, Clone)]
pub struct PatchInput {
    pub patches: Vec<f32>,
    pub positions: Vec<i64>,
    pub soft_tokens: i32,
    pub height: i32,
    pub width: i32,
    pub pool_weights: Option<Vec<f32>>,
}

pub struct ImagePipeline;

impl ImagePipeline {
    pub const MAX_PIXELS: i64 = 64 * 1024 * 1024;

    /// Request image to RGB. Alpha is dropped (like PIL `convert("RGB")`). Bad bytes throw `BadRequest`.
    pub fn decode(img: &ImageInput<'_>) -> Result<RgbImage, WorkerError> {
        if img.format == "bgra8" {
            return decode_bgra8(img);
        }

        let detected = image::guess_format(img.bytes)
            .map_err(|_| WorkerError::bad_request("undecodable image"))?;
        let expected = match img.format {
            "webp" => ImageFormat::WebP,
            "jpeg" => ImageFormat::Jpeg,
            "png" => ImageFormat::Png,
            _ => return Err(WorkerError::bad_request("format mismatch")),
        };
        if detected != expected {
            return Err(WorkerError::bad_request("format mismatch"));
        }

        let mut reader = ImageReader::new(Cursor::new(img.bytes));
        reader.set_format(expected);
        let (w, h) = reader
            .into_dimensions()
            .map_err(|_| WorkerError::bad_request("undecodable image"))?;
        if w == 0 || h == 0 || (w as i64) * (h as i64) > Self::MAX_PIXELS {
            return Err(WorkerError::bad_request("image size"));
        }

        let mut reader = ImageReader::new(Cursor::new(img.bytes));
        reader.set_format(expected);
        let decoded = reader
            .decode()
            .map_err(|_| WorkerError::bad_request("undecodable image"))?;
        let rgb = decoded.to_rgb8();
        Ok(RgbImage {
            width: w as i32,
            height: h as i32,
            rgb: rgb.into_raw(),
        })
    }

    /// Copies a rectangle out of a packed RGB image.
    pub fn crop(img: &RgbImage, x: i32, y: i32, w: i32, h: i32) -> Result<RgbImage, WorkerError> {
        if x < 0 || y < 0 || w <= 0 || h <= 0 || x + w > img.width || y + h > img.height {
            return Err(WorkerError::Internal(format!(
                "crop {x},{y} {w}x{h} outside {}x{}",
                img.width, img.height
            )));
        }
        let wu = w as usize;
        let hu = h as usize;
        let xu = x as usize;
        let yu = y as usize;
        let sw = img.width as usize;
        let mut dst = vec![0u8; wu * hu * 3];
        for r in 0..hu {
            let src_off = ((yu + r) * sw + xu) * 3;
            let dst_off = r * wu * 3;
            dst[dst_off..dst_off + wu * 3].copy_from_slice(&img.rgb[src_off..src_off + wu * 3]);
        }
        Ok(RgbImage {
            width: w,
            height: h,
            rgb: dst,
        })
    }

    /// Resize + rescale + normalise into a `[1,3,H,W]` float tensor (CHW).
    pub fn to_tensor(
        img: &RgbImage,
        height: i32,
        width: i32,
        spec: &ImagePreprocessSpec,
    ) -> Vec<f32> {
        let mut off_x = 0usize;
        let mut off_y = 0usize;
        let owned;
        let sized = if spec.fit == "pad" {
            let s = (width as f64 / img.width as f64).min(height as f64 / img.height as f64);
            let w = 1.max((img.width as f64 * s).round_ties_even() as i32);
            let h = 1.max((img.height as f64 * s).round_ties_even() as i32);
            owned = Self::resize(img, w, h, &spec.resample);
            off_x = ((width - w) / 2) as usize;
            off_y = ((height - h) / 2) as usize;
            &owned
        } else {
            owned = Self::resize(img, width, height, &spec.resample);
            &owned
        };

        let hu = height as usize;
        let wu = width as usize;
        let plane = hu * wu;
        let mut t = vec![0.0f32; 3 * plane];
        let sh = sized.height as usize;
        let sw = sized.width as usize;
        for c in 0..3 {
            let pad_n = (spec.pad_value * spec.rescale - spec.mean[c]) / spec.std[c];
            if spec.fit == "pad" {
                t[c * plane..(c + 1) * plane].fill(pad_n);
            }
            let scale = spec.rescale / spec.std[c];
            let bias = -spec.mean[c] / spec.std[c];
            for y in 0..sh {
                for x in 0..sw {
                    let v = sized.rgb[(y * sw + x) * 3 + c] as f32;
                    t[c * plane + (y + off_y) * wu + (x + off_x)] = v * scale + bias;
                }
            }
        }
        t
    }

    /// `transformers` `get_aspect_ratio_preserving_size` (Gemma 4).
    pub fn aspect_size(
        height: i32,
        width: i32,
        patch: i32,
        max_patches: i32,
        pool: i32,
    ) -> Result<(i32, i32), WorkerError> {
        let target_px = (max_patches as f64) * (patch as f64) * (patch as f64);
        let factor = (target_px / ((height as f64) * (width as f64))).sqrt();
        let side = pool * patch;
        let mut th = ((factor * height as f64 / side as f64).floor() as i32) * side;
        let mut tw = ((factor * width as f64 / side as f64).floor() as i32) * side;
        if th == 0 && tw == 0 {
            return Err(WorkerError::bad_request("image size"));
        }
        let max_side = (max_patches / (pool * pool)) * side;
        if th == 0 {
            th = side;
            tw = ((width / height) * side).min(max_side);
        } else if tw == 0 {
            tw = side;
            th = ((height / width) * side).min(max_side);
        }
        Ok((th, tw))
    }

    /// Patch form input, same as the Gemma 4 image processor.
    pub fn to_patches(img: &RgbImage, v: &VisionGraph) -> Result<PatchInput, WorkerError> {
        let p = v.patch_size;
        let m = v.patch_budget();
        let dim_p = (p * p * 3) as usize;
        let (h, w) = Self::aspect_size(img.height, img.width, p, m, v.pool)?;
        let sized = Self::resize(img, w, h, &v.preprocess.resample);
        let nh = (h / p) as usize;
        let nw = (w / p) as usize;
        let real = (nh * nw) as i32;
        let mu = m as usize;
        let pu = p as usize;
        let wu = w as usize;
        let pool_u = v.pool as usize;
        let mut patches = vec![0.0f32; mu * dim_p];
        let mut pos = vec![-1i64; mu * 2];
        let mut weights = v
            .pool_input
            .as_ref()
            .map(|_| vec![0.0f32; (v.tokens as usize) * mu]);
        let spec = &v.preprocess;
        let mut scale = [0.0f32; 3];
        let mut bias = [0.0f32; 3];
        for c in 0..3 {
            scale[c] = spec.rescale / spec.std[c];
            bias[c] = -spec.mean[c] / spec.std[c];
        }
        let rgb = &sized.rgb;
        let pool_val = 1.0f32 / (v.pool * v.pool) as f32;
        for py in 0..nh {
            for px in 0..nw {
                let idx = py * nw + px;
                pos[idx * 2] = px as i64;
                pos[idx * 2 + 1] = py as i64;
                if let Some(ref mut wt) = weights {
                    wt[(px / pool_u + (nw / pool_u) * (py / pool_u)) * mu + idx] = pool_val;
                }
                let o = idx * dim_p;
                for dy in 0..pu {
                    let src = ((py * pu + dy) * wu + px * pu) * 3;
                    for k in 0..pu * 3 {
                        patches[o + dy * pu * 3 + k] =
                            (rgb[src + k] as f32) * scale[k % 3] + bias[k % 3];
                    }
                }
            }
        }
        Ok(PatchInput {
            patches,
            positions: pos,
            soft_tokens: real / (v.pool * v.pool),
            height: h,
            width: w,
            pool_weights: weights,
        })
    }

    /// PIL-compatible separable resample (`Image.resize` with `BICUBIC` a=-0.5 or `BILINEAR`):
    /// the filter support widens with the downscale factor (antialias), horizontal pass first,
    /// rounding to 8 bit between passes.
    pub fn resize(img: &RgbImage, width: i32, height: i32, resample: &str) -> RgbImage {
        if img.width == width && img.height == height {
            return img.clone();
        }
        let (filter, support): (fn(f64) -> f64, f64) = if resample == "bilinear" {
            (triangle, 1.0)
        } else {
            (bicubic, 2.0)
        };
        let tmp = if img.width == width {
            img.clone()
        } else {
            pass(img, width, true, filter, support)
        };
        if tmp.height == height {
            tmp
        } else {
            pass(&tmp, height, false, filter, support)
        }
    }
}

fn decode_bgra8(img: &ImageInput<'_>) -> Result<RgbImage, WorkerError> {
    if img.width <= 0
        || img.height <= 0
        || (img.width as i64) * (img.height as i64) > ImagePipeline::MAX_PIXELS
        || img.stride < img.width * 4
    {
        return Err(WorkerError::bad_request("image size"));
    }
    let w = img.width as usize;
    let h = img.height as usize;
    let stride = img.stride as usize;
    let needed = (h - 1) * stride + w * 4;
    if img.bytes.len() < needed {
        return Err(WorkerError::bad_request("bgra8 buffer short"));
    }
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        let row = &img.bytes[y * stride..y * stride + w * 4];
        let mut o = y * w * 3;
        for x in 0..w {
            rgb[o] = row[x * 4 + 2];
            rgb[o + 1] = row[x * 4 + 1];
            rgb[o + 2] = row[x * 4];
            o += 3;
        }
    }
    Ok(RgbImage {
        width: img.width,
        height: img.height,
        rgb,
    })
}

fn triangle(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 { 1.0 - x } else { 0.0 }
}

fn bicubic(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = x.abs();
    if x < 1.0 {
        ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        (((x - 5.0) * x + 8.0) * x - 4.0) * A
    } else {
        0.0
    }
}

fn pass(
    src: &RgbImage,
    out_size: i32,
    horizontal: bool,
    filter: fn(f64) -> f64,
    support: f64,
) -> RgbImage {
    let in_size = if horizontal { src.width } else { src.height };
    let scale = (in_size as f64) / (out_size as f64);
    let fscale = scale.max(1.0);
    let sup = support * fscale;
    let ksize = (sup.ceil() as usize) * 2 + 1;
    let out_u = out_size as usize;
    let mut bounds = vec![(0usize, 0usize); out_u];
    let mut weights = vec![0.0f64; out_u * ksize];
    for i in 0..out_u {
        let center = (i as f64 + 0.5) * scale;
        let xmin = ((center - sup + 0.5) as i32).max(0) as usize;
        let xmax = (((center + sup + 0.5) as i32).min(in_size) as usize).saturating_sub(xmin);
        let mut total = 0.0f64;
        for j in 0..xmax {
            let w = filter(((j + xmin) as f64 - center + 0.5) / fscale);
            weights[i * ksize + j] = w;
            total += w;
        }
        if total != 0.0 {
            for j in 0..xmax {
                weights[i * ksize + j] /= total;
            }
        }
        bounds[i] = (xmin, xmax);
    }

    let ow = if horizontal { out_size } else { src.width };
    let oh = if horizontal { src.height } else { out_size };
    let ow_u = ow as usize;
    let oh_u = oh as usize;
    let sw_u = src.width as usize;
    let mut dst = vec![0u8; ow_u * oh_u * 3];
    let s = &src.rgb;
    for y in 0..oh_u {
        for x in 0..ow_u {
            let i = if horizontal { x } else { y };
            let (min, count) = bounds[i];
            let (mut r, mut g, mut b) = (0.0f64, 0.0f64, 0.0f64);
            for j in 0..count {
                let w = weights[i * ksize + j];
                let sx = if horizontal { min + j } else { x };
                let sy = if horizontal { y } else { min + j };
                let p = (sy * sw_u + sx) * 3;
                r += (s[p] as f64) * w;
                g += (s[p + 1] as f64) * w;
                b += (s[p + 2] as f64) * w;
            }
            let o = (y * ow_u + x) * 3;
            dst[o] = clip(r);
            dst[o + 1] = clip(g);
            dst[o + 2] = clip(b);
        }
    }
    RgbImage {
        width: ow,
        height: oh,
        rgb: dst,
    }
}

fn clip(v: f64) -> u8 {
    // Rust `f64::round` uses round-half-away-from-zero, matching C# `MidpointRounding.AwayFromZero`.
    v.round().clamp(0.0, 255.0) as u8
}
