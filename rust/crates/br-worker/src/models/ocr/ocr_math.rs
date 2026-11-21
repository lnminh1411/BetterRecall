// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashSet;

use crate::models::image_pipeline::RgbImage;
use crate::models::manifest::OcrRecSpec;
use crate::protocol::WorkerError;

/// Axis-aligned text box in image pixels with the detector's score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextBox {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub score: f32,
}

/// One CTC decode: text, mean probability of its characters and, per UTF-16 unit, the first and last
/// time step it covered.
#[derive(Debug, Clone, PartialEq)]
pub struct CtcResult {
    pub text: String,
    pub confidence: f32,
    pub step_start: Vec<i32>,
    pub step_end: Vec<i32>,
}

/// Pre- and post-processing of the PaddleOCR detector and recognisers. Pure: no model, testable
/// with synthetic tensors.
pub struct OcrMath;

impl OcrMath {
    pub const DEFAULT_MAX_BOXES: usize = 3000;
    pub const REC_BUCKETS: [i32; 7] = [256, 512, 768, 1024, 1536, 2048, 3200];

    /// Detector input size: scaled down so its longer side is at most `max_side`, each side
    /// rounded to a multiple of 32 (at least 32).
    pub fn det_size(w: i32, h: i32, max_side: i32) -> (i32, i32) {
        let s = (max_side as f64 / w.max(h) as f64).min(1.0);
        let dw = ((w as f64 * s / 32.0).round() as i32 * 32).max(32);
        let dh = ((h as f64 * s / 32.0).round() as i32 * 32).max(32);
        (dw, dh)
    }

    /// `[1,3,H,W]` BGR tensor, `(x/255 - mean[c]) / std[c]`, `mean` and `std` in BGR order.
    pub fn bgr_tensor(img: &RgbImage, mean: &[f32], std: &[f32]) -> Result<Vec<f32>, WorkerError> {
        if mean.len() < 3 || std.len() < 3 {
            return Err(WorkerError::Internal(
                "ocr detector manifest needs 3-element mean and std".into(),
            ));
        }
        let plane = (img.width * img.height) as usize;
        let mut t = vec![0.0f32; 3 * plane];
        for c in 0..3 {
            let src = 2 - c; // channel 0 = B
            let scale = 1.0f32 / 255.0 / std[c];
            let bias = -mean[c] / std[c];
            for i in 0..plane {
                t[c * plane + i] = (img.rgb[i * 3 + src] as f32) * scale + bias;
            }
        }
        Ok(t)
    }

    /// DB post-processing with axis-aligned boxes: throws `WorkerError::OcrOverflow` when more than
    /// `max_boxes` boxes are found so the scan never stops part-way and leaves text unredacted (C17).
    #[allow(clippy::too_many_arguments)]
    pub fn boxes(
        prob: &[f32],
        pw: i32,
        ph: i32,
        img_w: i32,
        img_h: i32,
        thresh: f32,
        box_thresh: f32,
        unclip: f32,
        max_boxes: usize,
    ) -> Result<Vec<TextBox>, WorkerError> {
        if pw <= 0 || ph <= 0 || prob.len() < (pw * ph) as usize {
            return Err(WorkerError::Internal("probability map too short".into()));
        }
        let pwu = pw as usize;
        let phu = ph as usize;
        let total = pwu * phu;
        let mut label = vec![0i32; total];
        let mut stack = Vec::new();
        let mut boxes = Vec::new();
        let sx = img_w as f64 / pw as f64;
        let sy = img_h as f64 / ph as f64;
        let mut next = 0i32;

        for start in 0..total {
            if label[start] != 0 || prob[start] <= thresh || prob[start].is_nan() {
                continue;
            }
            next += 1;
            let mut x0 = i32::MAX;
            let mut y0 = i32::MAX;
            let mut x1 = -1i32;
            let mut y1 = -1i32;
            label[start] = next;
            stack.push(start);

            while let Some(p) = stack.pop() {
                let x = (p % pwu) as i32;
                let y = (p / pwu) as i32;
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
                if x > 0 {
                    visit(p - 1, prob, &mut label, next, thresh, &mut stack);
                }
                if x + 1 < pw {
                    visit(p + 1, prob, &mut label, next, thresh, &mut stack);
                }
                if y > 0 {
                    visit(p - pwu, prob, &mut label, next, thresh, &mut stack);
                }
                if y + 1 < ph {
                    visit(p + pwu, prob, &mut label, next, thresh, &mut stack);
                }
            }

            let bw = x1 - x0 + 1;
            let bh = y1 - y0 + 1;
            if bw.min(bh) < 3 {
                continue;
            }
            let mut sum = 0.0f64;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    sum += prob[(y as usize) * pwu + (x as usize)] as f64;
                }
            }
            let score = (sum / (bw * bh) as f64) as f32;
            if score < box_thresh {
                continue;
            }
            let d = (bw * bh) as f64 * (unclip as f64) / (2.0 * (bw + bh) as f64);
            let l = ((x0 as f64 - d) * sx).max(0.0);
            let t = ((y0 as f64 - d) * sy).max(0.0);
            let r = ((x1 as f64 + 1.0 + d) * sx).min(img_w as f64);
            let b = ((y1 as f64 + 1.0 + d) * sy).min(img_h as f64);
            let il = l.floor() as i32;
            let it = t.floor() as i32;
            let ir = r.ceil() as i32;
            let ib = b.ceil() as i32;
            if ir - il < 3 || ib - it < 3 {
                continue;
            }
            if boxes.len() >= max_boxes {
                return Err(WorkerError::OcrOverflow(format!(
                    "ocr found more than {max_boxes} text boxes"
                )));
            }
            boxes.push(TextBox {
                x: il,
                y: it,
                w: ir - il,
                h: ib - it,
                score,
            });
        }
        boxes.sort_by(|a, b| a.y.cmp(&b.y).then_with(|| a.x.cmp(&b.x)));
        Ok(boxes)
    }

    pub fn rec_width(w: i32, h: i32, height: i32, max_width: i32) -> i32 {
        (((height as f64) * (w as f64) / h.max(1) as f64).ceil() as i32).clamp(8, max_width)
    }

    pub fn rec_bucket(width: i32) -> i32 {
        for &b in &Self::REC_BUCKETS {
            if width <= b {
                return b;
            }
        }
        Self::REC_BUCKETS[Self::REC_BUCKETS.len() - 1]
    }

    /// Batch tensor `[batch,3,height,width]` (zero to the right / extra rows = mid grey after `2x-1`).
    pub fn rec_tensor(
        resized: &[RgbImage],
        height: i32,
        width: i32,
        batch: usize,
    ) -> Result<Vec<f32>, WorkerError> {
        let hu = height as usize;
        let wu = width as usize;
        let plane = hu * wu;
        let batch = batch.max(resized.len());
        let mut t = vec![0.0f32; batch * 3 * plane];
        for (n, img) in resized.iter().enumerate() {
            if img.height != height || img.width > width {
                return Err(WorkerError::Internal(
                    "crop not resized to the batch shape".into(),
                ));
            }
            let base_n = n * 3 * plane;
            let img_w = img.width as usize;
            for c in 0..3 {
                let src = 2 - c;
                for y in 0..hu {
                    for x in 0..img_w {
                        t[base_n + c * plane + y * wu + x] =
                            (img.rgb[(y * img_w + x) * 3 + src] as f32) / 127.5 - 1.0;
                    }
                }
            }
        }
        Ok(t)
    }

    /// Greedy CTC over `[steps, classes]` probabilities: argmax per step, repeats collapsed,
    /// blank (class 0) dropped. Tracks one step interval per UTF-16 code unit for C#/Contracts parity.
    pub fn ctc(probs: &[f32], steps: usize, classes: usize, table: &[String]) -> CtcResult {
        if classes == 0 {
            return CtcResult {
                text: String::new(),
                confidence: 0.0,
                step_start: Vec::new(),
                step_end: Vec::new(),
            };
        }
        let safe_steps = steps.min(probs.len() / classes);
        let mut text = String::new();
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        let mut sum = 0.0f64;
        let mut kept = 0usize;
        let mut prev = 0usize;
        let mut last_units = 0usize;

        for t in 0..safe_steps {
            let row = &probs[t * classes..(t + 1) * classes];
            let mut best = 0usize;
            let mut bp = row[0];
            for (k, &val) in row.iter().enumerate().skip(1) {
                if val > bp {
                    bp = val;
                    best = k;
                }
            }
            if best != 0 && best == prev && last_units > 0 {
                let len = ends.len();
                let from = len.saturating_sub(last_units);
                ends[from..len].fill(t as i32);
            } else if best != 0 && best < table.len() {
                let s = &table[best];
                let units = s.encode_utf16().count();
                for _ in 0..units {
                    starts.push(t as i32);
                    ends.push(t as i32);
                }
                last_units = units;
                text.push_str(s);
                sum += bp as f64;
                kept += 1;
            }
            prev = best;
        }
        CtcResult {
            text,
            confidence: if kept == 0 {
                0.0
            } else {
                (sum / kept as f64) as f32
            },
            step_start: starts,
            step_end: ends,
        }
    }

    /// Character boundaries in image pixels (`utf16_len + 1` values) with outer cuts pinned to box edges (C1).
    pub fn cuts(
        r: &CtcResult,
        steps: usize,
        input_width: i32,
        content_width: i32,
        crop_x: i32,
        crop_w: i32,
    ) -> Vec<i32> {
        let n = r.text.encode_utf16().count();
        let mut cuts = vec![0i32; n + 1];
        if n == 0 {
            cuts[0] = crop_x;
            return cuts;
        }
        if steps == 0 || r.step_start.len() < n || r.step_end.len() < n {
            for (i, slot) in cuts.iter_mut().enumerate() {
                *slot = crop_x + ((crop_w as i64 * i as i64) / n as i64) as i32;
            }
            return cuts;
        }
        let step = input_width as f64 / steps as f64;
        let to_crop = crop_w as f64 / content_width.max(1) as f64;
        let px = |input_x: f64| -> i32 {
            crop_x + (input_x.clamp(0.0, content_width as f64) * to_crop).round_ties_even() as i32
        };
        cuts[0] = crop_x;
        for (i, slot) in cuts[1..n].iter_mut().enumerate() {
            *slot = px(((r.step_end[i] + 1 + r.step_start[i + 1]) as f64) * step / 2.0);
        }
        cuts[n] = crop_x + crop_w;
        let mut prev = cuts[0];
        for slot in &mut cuts[1..] {
            if *slot < prev {
                *slot = prev;
            }
            prev = *slot;
        }
        cuts
    }

    /// Class table: `""` (blank), dictionary entries, then `" "`.
    pub fn table(dict: &[String]) -> Vec<String> {
        let mut t = Vec::with_capacity(dict.len() + 2);
        t.push(String::new());
        t.extend_from_slice(dict);
        t.push(" ".to_string());
        t
    }

    /// Recognisers to run for `ocr.languages`: first ("main") always, plus each other matching a base code.
    pub fn pick_recognisers(all: &[OcrRecSpec], languages: &[String]) -> Vec<OcrRecSpec> {
        let mut picked = Vec::new();
        if all.is_empty() {
            return picked;
        }
        picked.push(all[0].clone());
        let codes: HashSet<String> = languages
            .iter()
            .filter_map(|l| {
                let base = l.trim().to_ascii_lowercase();
                let head = base.split('-').next().unwrap_or("");
                if head.is_empty() {
                    None
                } else {
                    Some(head.to_string())
                }
            })
            .collect();
        for r in &all[1..] {
            if r.languages.iter().any(|lang| codes.contains(lang)) {
                picked.push(r.clone());
            }
        }
        picked
    }
}

fn visit(
    q: usize,
    prob: &[f32],
    label: &mut [i32],
    id: i32,
    thresh: f32,
    stack: &mut Vec<usize>,
) {
    if label[q] != 0 || prob[q] <= thresh || prob[q].is_nan() {
        return;
    }
    label[q] = id;
    stack.push(q);
}
