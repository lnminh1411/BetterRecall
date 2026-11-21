// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use br_contracts::OcrLine;

use crate::inference::{DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession};
use crate::models::image_pipeline::{ImagePipeline, RgbImage};
use crate::models::manifest::{ModelManifest, OcrDetSpec, OcrRecSpec};
use crate::models::ocr::ocr_math::{OcrMath, TextBox};
use crate::protocol::WorkerError;

struct Recogniser {
    spec: OcrRecSpec,
    graph: Graph,
    table: Vec<String>,
}

struct Graph {
    backend: Arc<dyn IInferenceBackend>,
    device: DeviceInfo,
    path: PathBuf,
    input: String,
    cache_dir: Option<PathBuf>,
    dynamic: Option<Box<dyn IInferenceSession>>,
    by_shape: HashMap<String, Box<dyn IInferenceSession>>,
}

impl Graph {
    const MAX_SHAPES: usize = 48;

    fn new(
        backend: Arc<dyn IInferenceBackend>,
        device: DeviceInfo,
        path: PathBuf,
        input: String,
        cache_dir: Option<PathBuf>,
        per_shape: bool,
    ) -> Result<Self, WorkerError> {
        let dynamic = if per_shape {
            None
        } else {
            Some(backend.load(&path, &device, cache_dir.as_deref(), None)?)
        };
        Ok(Self {
            backend,
            device,
            path,
            input,
            cache_dir,
            dynamic,
            by_shape: HashMap::new(),
        })
    }

    fn count(&self) -> usize {
        if self.dynamic.is_some() {
            1
        } else {
            self.by_shape.len()
        }
    }

    fn session(&mut self, shape: &[i64]) -> Result<&dyn IInferenceSession, WorkerError> {
        if let Some(ref s) = self.dynamic {
            return Ok(s.as_ref());
        }
        let key: String = shape
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join("x");
        if !self.by_shape.contains_key(&key) {
            if self.by_shape.len() >= Self::MAX_SHAPES {
                return Err(WorkerError::DeviceFailed(format!(
                    "{} ocr: more than {} input shapes",
                    self.device.label(),
                    Self::MAX_SHAPES
                )));
            }
            let mut shapes = HashMap::new();
            shapes.insert(self.input.clone(), shape.to_vec());
            let sess = self.backend.load(
                &self.path,
                &self.device,
                self.cache_dir.as_deref(),
                Some(&shapes),
            )?;
            self.by_shape.insert(key.clone(), sess);
        }
        Ok(self.by_shape[&key].as_ref())
    }
}

/// PaddleOCR on one device: DB detector + main recogniser + optional language fallback recognisers.
pub struct PaddleOcr {
    det: OcrDetSpec,
    det_graph: Graph,
    recs: Vec<Recogniser>,
    fixed_shapes: bool,
    pub device: DeviceInfo,
}

impl PaddleOcr {
    pub const RETRY_BELOW: f32 = 0.85;
    pub const MAIN_BONUS: f32 = 0.05;
    pub const BATCH: usize = 8;
    pub const WIDTH_STEP: i32 = 64;

    pub fn new(
        backend: Arc<dyn IInferenceBackend>,
        device: DeviceInfo,
        manifest: &ModelManifest,
        recs_spec: &[OcrRecSpec],
        cache_dir: Option<&Path>,
        fixed_shapes: Option<bool>,
    ) -> Result<Self, WorkerError> {
        let spec = manifest
            .ocr
            .as_ref()
            .ok_or_else(|| WorkerError::Internal("ocr manifest lacks ocr".into()))?;
        let det = spec.det.clone();
        if recs_spec.is_empty() {
            return Err(WorkerError::Internal(
                "ocr manifest has no recogniser".into(),
            ));
        }
        let per_shape = device.kind == DeviceKind::Npu;
        let fixed_shapes = fixed_shapes.unwrap_or(device.kind != DeviceKind::Cpu);
        let cache_pb = cache_dir.map(Path::to_path_buf);

        let det_graph = Graph::new(
            Arc::clone(&backend),
            device.clone(),
            manifest.path_of(&det.file),
            det.input.clone(),
            cache_pb.clone(),
            per_shape,
        )?;

        let mut recs = Vec::with_capacity(recs_spec.len());
        for r in recs_spec {
            let dict = Self::load_dict(&manifest.path_of(&r.dict))
                .map_err(|e| WorkerError::Internal(format!("ocr dict {}: {e}", r.name)))?;
            let table = OcrMath::table(&dict);
            if r.classes > 0 && table.len() != r.classes as usize {
                return Err(WorkerError::Internal(format!(
                    "ocr {}: dictionary gives {} classes, manifest {}",
                    r.name,
                    table.len(),
                    r.classes
                )));
            }
            let graph = Graph::new(
                Arc::clone(&backend),
                device.clone(),
                manifest.path_of(&r.file),
                r.input.clone(),
                cache_pb.clone(),
                per_shape,
            )?;
            recs.push(Recogniser {
                spec: r.clone(),
                graph,
                table,
            });
        }

        Ok(Self {
            det,
            det_graph,
            recs,
            fixed_shapes,
            device,
        })
    }

    pub fn recognisers(&self) -> Vec<String> {
        self.recs.iter().map(|r| r.spec.name.clone()).collect()
    }

    pub fn compiled_graphs(&self) -> usize {
        self.det_graph.count() + self.recs.iter().map(|r| r.graph.count()).sum::<usize>()
    }

    /// Dictionary file: one entry per line, UTF-8 (strips UTF-8 BOM and accepts CRLF).
    pub fn load_dict(path: &Path) -> std::io::Result<Vec<String>> {
        let raw = fs::read_to_string(path)?;
        let no_bom = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
        let norm = no_bom.replace("\r\n", "\n");
        Ok(norm.split('\n').map(ToString::to_string).collect())
    }

    /// One read of a small synthetic frame (dark bars on white).
    pub fn warm_up(&mut self) -> Result<(), WorkerError> {
        const W: i32 = 320;
        const H: i32 = 96;
        let mut rgb = vec![255u8; (W * H * 3) as usize];
        for y in 40..56 {
            for x in 20..300 {
                if (x / 6) % 2 == 0 {
                    let o = ((y * W + x) * 3) as usize;
                    rgb[o] = 20;
                    rgb[o + 1] = 20;
                    rgb[o + 2] = 20;
                }
            }
        }
        self.read(&RgbImage {
            width: W,
            height: H,
            rgb,
        })
        .map(|_| ())
    }

    /// False when any value is NaN or infinite.
    pub fn all_finite(values: &[f32]) -> bool {
        values.iter().all(|v| v.is_finite())
    }

    pub fn read(&mut self, img: &RgbImage) -> Result<Vec<OcrLine>, WorkerError> {
        let (dw, dh) = OcrMath::det_size(img.width, img.height, self.det.max_side);
        let sized = ImagePipeline::resize(img, dw, dh, "bilinear");
        let tensor = OcrMath::bgr_tensor(&sized, &self.det.mean, &self.det.std)?;
        let outp = self
            .det_graph
            .session(&[1, 3, dh as i64, dw as i64])?
            .run(&[HostTensor::f(
                self.det.input.clone(),
                tensor,
                &[1, 3, dh as i64, dw as i64],
            )])?;
        let prob = outp
            .values()
            .next()
            .and_then(|t| t.floats.as_deref())
            .ok_or_else(|| WorkerError::DeviceFailed("ocr detector output missing".into()))?;
        let map_len = (dw * dh) as usize;
        if prob.len() < map_len {
            return Err(WorkerError::DeviceFailed(
                "ocr detector output too small".into(),
            ));
        }
        // Whole map checked: an FP16 overflow anywhere must fail rather than hide unredacted text (C12).
        if !Self::all_finite(&prob[..map_len]) {
            return Err(WorkerError::DeviceFailed(format!(
                "{} ocr detector output not finite",
                self.device.label()
            )));
        }
        let boxes = OcrMath::boxes(
            prob,
            dw,
            dh,
            img.width,
            img.height,
            self.det.thresh,
            self.det.box_thresh,
            self.det.unclip,
            OcrMath::DEFAULT_MAX_BOXES,
        )?;
        if boxes.is_empty() {
            return Ok(Vec::new());
        }

        let crops: Vec<RgbImage> = boxes
            .iter()
            .map(|b| ImagePipeline::crop(img, b.x, b.y, b.w, b.h))
            .collect::<Result<Vec<_>, _>>()?;
        let mut best = recognise_with(
            &mut self.recs[0],
            &self.device,
            self.fixed_shapes,
            &crops,
            &boxes,
        )?;
        if self.recs.len() > 1 {
            let retry: Vec<usize> = best
                .iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    if l.as_ref().is_none_or(|x| x.confidence < Self::RETRY_BELOW) {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect();
            let mut score: Vec<f32> = best
                .iter()
                .map(|l| {
                    l.as_ref()
                        .map(|x| x.confidence + Self::MAIN_BONUS)
                        .unwrap_or(-1.0)
                })
                .collect();
            if !retry.is_empty() {
                let retry_crops: Vec<RgbImage> =
                    retry.iter().map(|&i| crops[i].clone()).collect();
                let retry_boxes: Vec<TextBox> = retry.iter().map(|&i| boxes[i]).collect();
                for r in &mut self.recs[1..] {
                    let alt = recognise_with(
                        r,
                        &self.device,
                        self.fixed_shapes,
                        &retry_crops,
                        &retry_boxes,
                    )?;
                    for (k, &orig_idx) in retry.iter().enumerate() {
                        if let Some(a) = alt[k].as_ref()
                            && a.confidence > score[orig_idx]
                        {
                            score[orig_idx] = a.confidence;
                            best[orig_idx] = Some(a.clone());
                        }
                    }
                }
            }
        }
        Ok(best
            .into_iter()
            .flatten()
            .filter(|l| !l.text.trim().is_empty())
            .collect())
    }
}

fn recognise_with(
    r: &mut Recogniser,
    device: &DeviceInfo,
    fixed_shapes: bool,
    crops: &[RgbImage],
    boxes: &[TextBox],
) -> Result<Vec<Option<OcrLine>>, WorkerError> {
    let height = r.spec.height;
    let mut result: Vec<Option<OcrLine>> = vec![None; crops.len()];
    let mut order: Vec<usize> = (0..crops.len()).collect();
    order.sort_by(|&a, &b| {
        let ra = crops[a].width as f64 / crops[a].height as f64;
        let rb = crops[b].width as f64 / crops[b].height as f64;
        ra.total_cmp(&rb)
    });

    for chunk in order.chunks(PaddleOcr::BATCH) {
        let widths: Vec<i32> = chunk
            .iter()
            .map(|&i| OcrMath::rec_width(crops[i].width, crops[i].height, height, 3200))
            .collect();
        let max_w = *widths.iter().max().unwrap_or(&8);
        let width = if fixed_shapes {
            OcrMath::rec_bucket(max_w)
        } else {
            3200.min(((max_w + PaddleOcr::WIDTH_STEP - 1) / PaddleOcr::WIDTH_STEP) * PaddleOcr::WIDTH_STEP)
        };
        let n = if fixed_shapes {
            PaddleOcr::BATCH
        } else {
            chunk.len()
        };
        let resized: Vec<RgbImage> = chunk
            .iter()
            .enumerate()
            .map(|(k, &i)| ImagePipeline::resize(&crops[i], widths[k].min(width), height, "bilinear"))
            .collect();
        let tensor = OcrMath::rec_tensor(&resized, height, width, n)?;
        let outp = r
            .graph
            .session(&[n as i64, 3, height as i64, width as i64])?
            .run(&[HostTensor::f(
                r.spec.input.clone(),
                tensor,
                &[n as i64, 3, height as i64, width as i64],
            )])?;
        let t = outp
            .values()
            .next()
            .ok_or_else(|| WorkerError::DeviceFailed("ocr recogniser output missing".into()))?;
        let floats = t
            .floats
            .as_deref()
            .ok_or_else(|| WorkerError::DeviceFailed("ocr recogniser output shape".into()))?;
        if t.shape.len() != 3 || t.shape[0] != n as i64 {
            return Err(WorkerError::DeviceFailed(
                "ocr recogniser output shape".into(),
            ));
        }
        let steps = t.shape[1] as usize;
        let classes = t.shape[2] as usize;
        if classes != r.table.len() {
            return Err(WorkerError::DeviceFailed(format!(
                "ocr {}: {classes} classes, dictionary {}",
                r.spec.name,
                r.table.len()
            )));
        }
        let row_elems = steps * classes;
        for (k, &orig_idx) in chunk.iter().enumerate() {
            let start = k * row_elems;
            let probs = floats
                .get(start..start + row_elems)
                .ok_or_else(|| WorkerError::DeviceFailed("ocr recogniser output shape".into()))?;
            if !PaddleOcr::all_finite(probs) {
                return Err(WorkerError::DeviceFailed(format!(
                    "{} ocr recogniser output not finite",
                    device.label()
                )));
            }
            let ctc = OcrMath::ctc(probs, steps, classes, &r.table);
            if ctc.text.is_empty() {
                continue;
            }
            if !ctc.confidence.is_finite() {
                return Err(WorkerError::DeviceFailed(format!(
                    "{} ocr recogniser output not finite",
                    device.label()
                )));
            }
            let b = boxes[orig_idx];
            let cuts = OcrMath::cuts(&ctc, steps, width, resized[k].width, b.x, b.w);
            result[orig_idx] = Some(OcrLine {
                text: ctc.text,
                x: b.x,
                y: b.y,
                w: b.w,
                h: b.h,
                confidence: ctc.confidence,
                cuts,
                entities: None,
            });
        }
    }
    Ok(result)
}
