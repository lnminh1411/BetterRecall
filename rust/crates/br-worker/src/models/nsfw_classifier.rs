// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::path::Path;

use crate::inference::{DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession};
use crate::models::image_pipeline::{ImagePipeline, RgbImage};
use crate::models::manifest::{ClassifierSpec, ModelManifest};
use crate::protocol::WorkerError;

/// Small image classifier run before embedding (INT8 graph on CPU, FP32 `acceleratorFile` on NPU/GPU).
pub struct NsfwClassifier {
    session: Box<dyn IInferenceSession>,
    spec: ClassifierSpec,
    block: Vec<usize>,
    pub manifest: ModelManifest,
}

impl NsfwClassifier {
    /// Loads the graph for `device`: `file` on CPU, `accelerator_file` (or `file`) elsewhere.
    /// The input is fixed to `[1,3,H,W]` so the NPU gets a static shape.
    pub fn new(
        backend: &dyn IInferenceBackend,
        device: &DeviceInfo,
        manifest: ModelManifest,
        cache_dir: Option<&Path>,
    ) -> Result<Self, WorkerError> {
        let spec = manifest
            .classifier
            .clone()
            .ok_or_else(|| WorkerError::Internal("nsfw manifest lacks classifier".into()))?;
        let block: Vec<usize> = spec
            .block_labels
            .iter()
            .filter_map(|l| spec.labels.iter().position(|x| x == l))
            .collect();
        if block.is_empty() {
            return Err(WorkerError::Internal(
                "nsfw manifest names no block label".into(),
            ));
        }
        let file = Self::file_for(&spec, device.kind);
        let mut shapes = HashMap::new();
        shapes.insert(
            spec.input.clone(),
            vec![1, 3, spec.height as i64, spec.width as i64],
        );
        let session = backend.load(&manifest.path_of(file), device, cache_dir, Some(&shapes))?;
        Ok(Self {
            session,
            spec,
            block,
            manifest,
        })
    }

    pub fn device(&self) -> &DeviceInfo {
        self.session.device()
    }

    /// The INT8 graph is for the CPU; accelerators take the FP32 graph when the manifest has one.
    pub fn file_for(spec: &ClassifierSpec, kind: DeviceKind) -> &str {
        if kind == DeviceKind::Cpu
            || spec
                .accelerator_file
                .as_deref()
                .is_none_or(str::is_empty)
        {
            &spec.file
        } else {
            spec.accelerator_file.as_deref().unwrap_or(&spec.file)
        }
    }

    /// One run on a zero frame: compiles lazily-built kernels and proves the device produces a finite score.
    pub fn warm_up(&self) -> Result<(), WorkerError> {
        let zeros = vec![0.0f32; 3 * (self.spec.height as usize) * (self.spec.width as usize)];
        let p = self.run(zeros)?;
        if !p.is_finite() {
            return Err(WorkerError::DeviceFailed(format!(
                "{} nsfw classifier output not finite",
                self.device().label()
            )));
        }
        Ok(())
    }

    /// Highest block score over the full frame and its tiles. Stops early once a score reaches `stop_at`.
    pub fn score(&self, image: &RgbImage, stop_at: f64, grid: i32) -> Result<f64, WorkerError> {
        let mut best = self.score_one(image)?;
        for (x, y, w, h) in Self::tiles(
            image.width,
            image.height,
            self.spec.width,
            self.spec.height,
            grid,
        ) {
            if best >= stop_at {
                break;
            }
            let tile = ImagePipeline::crop(image, x, y, w, h)?;
            best = best.max(self.score_one(&tile)?);
        }
        Ok(best)
    }

    /// Tiles scored after the full frame. Grid 2: four quadrants + centre tile over seams.
    /// Grid 3: 3x3 grid, or grid 2 when a ninth would be smaller than the model input.
    pub fn tiles(
        width: i32,
        height: i32,
        in_w: i32,
        in_h: i32,
        grid: i32,
    ) -> Vec<(i32, i32, i32, i32)> {
        if grid >= 3 && width / 3 >= in_w && height / 3 >= in_h {
            let w3 = width / 3;
            let h3 = height / 3;
            let mut out = Vec::with_capacity(9);
            for r in 0..3 {
                for c in 0..3 {
                    out.push((c * w3, r * h3, w3, h3));
                }
            }
            return out;
        }
        if grid < 2 {
            return Vec::new();
        }
        let w = width / 2;
        let h = height / 2;
        if w < in_w || h < in_h {
            return Vec::new();
        }
        vec![
            (0, 0, w, h),
            (w, 0, w, h),
            (0, h, w, h),
            (w, h, w, h),
            (w / 2, h / 2, w, h),
        ]
    }

    fn score_one(&self, image: &RgbImage) -> Result<f64, WorkerError> {
        let x = ImagePipeline::to_tensor(
            image,
            self.spec.height,
            self.spec.width,
            &self.spec.preprocess,
        );
        let p = self.run(x)?;
        // A NaN would compare false against the threshold and let the frame through: fail the request instead.
        if !p.is_finite() {
            return Err(WorkerError::DeviceFailed(format!(
                "{} nsfw classifier output not finite",
                self.device().label()
            )));
        }
        Ok(p)
    }

    fn run(&self, x: Vec<f32>) -> Result<f64, WorkerError> {
        let input = HostTensor::f(
            self.spec.input.clone(),
            x,
            &[1, 3, self.spec.height as i64, self.spec.width as i64],
        );
        let outp = self.session.run(&[input])?;
        let Some(logits) = outp.get(&self.spec.output).and_then(|t| t.floats.as_ref()) else {
            return Err(WorkerError::DeviceFailed(
                "nsfw classifier output missing".into(),
            ));
        };
        let n_labels = self.spec.labels.len();
        if logits.len() < n_labels {
            return Err(WorkerError::DeviceFailed(
                "nsfw classifier output missing".into(),
            ));
        }
        let z = &logits[..n_labels];
        if z.iter().any(|&v| !v.is_finite()) {
            return Ok(f64::NAN);
        }
        Ok(Self::block_probability(z, &self.block))
    }

    pub fn block_probability(logits: &[f32], block: &[usize]) -> f64 {
        let max = logits
            .iter()
            .fold(f64::NEG_INFINITY, |m, &l| m.max(l as f64));
        let mut sum = 0.0f64;
        let mut hit = 0.0f64;
        for (i, &l) in logits.iter().enumerate() {
            let e = ((l as f64) - max).exp();
            sum += e;
            if block.contains(&i) {
                hit += e;
            }
        }
        if sum > 0.0 { hit / sum } else { 0.0 }
    }
}
