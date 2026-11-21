// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::path::Path;

use crate::inference::{DeviceInfo, HostTensor, IInferenceBackend, IInferenceSession};
use crate::models::image_pipeline::{ImagePipeline, RgbImage};
use crate::models::manifest::{DetectorSpec, ImagePreprocessSpec, ModelManifest};
use crate::protocol::WorkerError;

/// Second NSFW model: NudeNet YOLOv8 detector, run at 640 px on the whole frame after the classifier passed it.
pub struct NsfwDetector {
    session: Box<dyn IInferenceSession>,
    spec: DetectorSpec,
    block_rows: Vec<usize>,
    pub manifest: ModelManifest,
}

impl NsfwDetector {
    fn pre_spec() -> ImagePreprocessSpec {
        ImagePreprocessSpec {
            resample: "bilinear".to_string(),
            rescale: 1.0 / 255.0,
            mean: vec![0.0, 0.0, 0.0],
            std: vec![1.0, 1.0, 1.0],
            fit: "stretch".to_string(),
            pad_value: 0.0,
        }
    }

    /// Input fixed to `[1,3,Size,Size]` (the NPU needs a static shape).
    pub fn new(
        backend: &dyn IInferenceBackend,
        device: &DeviceInfo,
        manifest: ModelManifest,
        cache_dir: Option<&Path>,
    ) -> Result<Self, WorkerError> {
        let spec = manifest
            .detector
            .clone()
            .ok_or_else(|| WorkerError::Internal("nsfw-detector manifest lacks detector".into()))?;
        if spec.size <= 0 || spec.labels.is_empty() {
            return Err(WorkerError::Internal(
                "nsfw-detector manifest has no size or labels".into(),
            ));
        }
        let block_rows = Self::block_rows(&spec);
        if block_rows.is_empty() {
            return Err(WorkerError::Internal(
                "nsfw-detector manifest names no block label".into(),
            ));
        }
        let mut shapes = HashMap::new();
        shapes.insert(
            spec.input.clone(),
            vec![1, 3, spec.size as i64, spec.size as i64],
        );
        let session = backend.load(
            &manifest.path_of(&spec.file),
            device,
            cache_dir,
            Some(&shapes),
        )?;
        Ok(Self {
            session,
            spec,
            block_rows,
            manifest,
        })
    }

    pub fn device(&self) -> &DeviceInfo {
        self.session.device()
    }

    /// Output rows of the block labels: 4 box rows come first.
    pub fn block_rows(spec: &DetectorSpec) -> Vec<usize> {
        let mut rows = Vec::new();
        for l in &spec.block_labels {
            if let Some(idx) = spec.labels.iter().position(|x| x == l) {
                let row = 4 + idx;
                if !rows.contains(&row) {
                    rows.push(row);
                }
            }
        }
        rows
    }

    pub fn warm_up(&self) -> Result<(), WorkerError> {
        let zeros = vec![0.0f32; 3 * (self.spec.size as usize) * (self.spec.size as usize)];
        let s = self.run(zeros)?;
        if !s.is_finite() {
            return Err(WorkerError::DeviceFailed(format!(
                "{} nsfw detector output not finite",
                self.device().label()
            )));
        }
        Ok(())
    }

    pub fn score(&self, image: &RgbImage) -> Result<f64, WorkerError> {
        let x = Self::letterbox(image, self.spec.size);
        let s = self.run(x)?;
        if !s.is_finite() {
            return Err(WorkerError::DeviceFailed(format!(
                "{} nsfw detector output not finite",
                self.device().label()
            )));
        }
        Ok(s)
    }

    fn run(&self, x: Vec<f32>) -> Result<f64, WorkerError> {
        let sz = self.spec.size as i64;
        let input = HostTensor::f(self.spec.input.clone(), x, &[1, 3, sz, sz]);
        let outp = self.session.run(&[input])?;
        let Some(t) = outp.get(&self.spec.output) else {
            return Err(WorkerError::DeviceFailed(
                "nsfw detector output missing".into(),
            ));
        };
        let Some(ref floats) = t.floats else {
            return Err(WorkerError::DeviceFailed(
                "nsfw detector output missing".into(),
            ));
        };
        if t.shape.len() != 3 {
            return Err(WorkerError::DeviceFailed(
                "nsfw detector output missing".into(),
            ));
        }
        let rows = t.shape[1] as usize;
        let anchors = t.shape[2] as usize;
        let expected_rows = 4 + self.spec.labels.len();
        if rows != expected_rows {
            return Err(WorkerError::DeviceFailed(format!(
                "nsfw detector output has {rows} rows, manifest {expected_rows}"
            )));
        }
        Self::block_score(floats, rows, anchors, &self.block_rows)
    }

    /// Square canvas of side `max(w, h)`, black, image at the top left; bilinear resize to `size x size`;
    /// RGB / 255 in CHW order (`[1,3,size,size]`).
    pub fn letterbox(image: &RgbImage, size: i32) -> Vec<f32> {
        let pre = Self::pre_spec();
        if image.width == image.height {
            return ImagePipeline::to_tensor(image, size, size, &pre);
        }
        let side = image.width.max(image.height);
        let su = side as usize;
        let wu = image.width as usize;
        let hu = image.height as usize;
        let mut rgb = vec![0u8; su * su * 3];
        for y in 0..hu {
            let src_off = y * wu * 3;
            let dst_off = y * su * 3;
            rgb[dst_off..dst_off + wu * 3].copy_from_slice(&image.rgb[src_off..src_off + wu * 3]);
        }
        let square = RgbImage {
            width: side,
            height: side,
            rgb,
        };
        ImagePipeline::to_tensor(&square, size, size, &pre)
    }

    /// Highest value of the block rows over all anchors of a `[rows, anchors]` output (batch 1).
    /// `NaN` when a block value is not finite; `0` when no block row exists.
    pub fn block_score(
        output: &[f32],
        rows: usize,
        anchors: usize,
        block_rows: &[usize],
    ) -> Result<f64, WorkerError> {
        if output.len() < rows.saturating_mul(anchors) {
            return Err(WorkerError::DeviceFailed(
                "nsfw detector output too short".into(),
            ));
        }
        let mut best = 0.0f64;
        for &r in block_rows {
            if r >= rows {
                continue;
            }
            for &v in &output[r * anchors..(r + 1) * anchors] {
                if !v.is_finite() {
                    return Ok(f64::NAN);
                }
                let vf = v as f64;
                if vf > best {
                    best = vf;
                }
            }
        }
        Ok(best)
    }
}
