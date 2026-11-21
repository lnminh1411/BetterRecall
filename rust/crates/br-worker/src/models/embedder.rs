// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::engine::TextKind;
use crate::hosting::Log;
use crate::inference::{DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession};
use crate::models::embedding_table::EmbeddingTable;
use crate::models::image_pipeline::{ImagePipeline, RgbImage};
use crate::models::manifest::{
    FusionGraph, ModelManifest, TextGraph, VisionGraph,
};
use crate::models::tokenizer::GemmaTokenizer;
use crate::protocol::WorkerError;
use crate::vectors::VectorMath;

/// Manifest-driven image/text embedder. Vision graphs run on `device`; text graphs (and the image's second pass)
/// on `text_device`, which equals `device` unless the benchmark chose a split (vision on NPU, text on CPU).
pub struct Embedder {
    backend: Arc<dyn IInferenceBackend>,
    cache_dir: Option<PathBuf>,
    tokenizer: GemmaTokenizer,
    table: Option<EmbeddingTable>,
    text: Vec<TextGraph>,
    vision: VisionGraph,
    sessions: HashMap<String, Box<dyn IInferenceSession>>,
    pub manifest: ModelManifest,
    pub device: DeviceInfo,
    pub text_device: DeviceInfo,
    /// Text lengths dropped because `text_device` failed to compile them (NPU compiler limits).
    pub dropped_lengths: Vec<i32>,
}

impl Embedder {
    pub fn new(
        backend: Arc<dyn IInferenceBackend>,
        device: DeviceInfo,
        manifest: ModelManifest,
        image_tokens: i32,
        cache_dir: Option<PathBuf>,
        text_device: Option<DeviceInfo>,
    ) -> Result<Self, WorkerError> {
        let text_device = text_device.unwrap_or_else(|| device.clone());
        let tok_spec = manifest.tokenizer.clone().ok_or_else(|| {
            WorkerError::Internal(format!(
                "manifest {} lacks tokenizer, text or vision graphs",
                manifest.name
            ))
        })?;
        if manifest.text.is_empty() || manifest.vision.is_empty() {
            return Err(WorkerError::Internal(format!(
                "manifest {} lacks tokenizer, text or vision graphs",
                manifest.name
            )));
        }
        let tokenizer = GemmaTokenizer::from_file(&manifest.path_of(&tok_spec.file), tok_spec)
            .map_err(WorkerError::Internal)?;
        let mut text = manifest.text.clone();
        text.sort_by_key(|t| t.seq);
        let vision = Self::pick_vision(&manifest.vision, image_tokens).clone();
        let need_table = text.iter().any(|t| t.inputs_embeds.is_some())
            || vision
                .fusion
                .as_ref()
                .is_some_and(|f| f.inputs_embeds.is_some());
        let table = if need_table {
            let spec = manifest
                .embeddings
                .as_ref()
                .ok_or_else(|| WorkerError::Internal("manifest needs an embedding table".into()))?;
            let path = manifest.path_of(&spec.file);
            Some(EmbeddingTable::open(&path, spec).map_err(WorkerError::Internal)?)
        } else {
            None
        };
        Ok(Self {
            backend,
            cache_dir,
            tokenizer,
            table,
            text,
            vision,
            sessions: HashMap::new(),
            manifest,
            device,
            text_device,
            dropped_lengths: Vec::new(),
        })
    }

    pub fn image_tokens(&self) -> i32 {
        self.vision.tokens
    }

    /// Token budgets this model offers (vision graphs), ascending.
    pub fn budgets(&self) -> Vec<i32> {
        let mut b: Vec<i32> = self
            .manifest
            .vision
            .iter()
            .map(|v| v.tokens)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        b.sort_unstable();
        b
    }

    pub fn pick_vision(graphs: &[VisionGraph], tokens: i32) -> &VisionGraph {
        graphs
            .iter()
            .find(|v| v.tokens == tokens)
            .unwrap_or_else(|| {
                graphs
                    .iter()
                    .min_by_key(|v| (v.tokens - tokens).abs())
                    .expect("vision graphs non-empty")
            })
    }

    /// Keys of the sessions compiled so far.
    pub fn session_keys(&self) -> HashSet<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Disposes every session not in `keep`: what a budget sweep compiled beyond the configured graphs (C13).
    pub fn drop_sessions_except(&mut self, keep: &HashSet<String>) {
        self.sessions.retain(|k, _| keep.contains(k));
    }

    fn session(
        &mut self,
        file: &str,
        shapes: Option<&BTreeMap<String, Vec<i64>>>,
        device: &DeviceInfo,
    ) -> Result<&dyn IInferenceSession, WorkerError> {
        let file = self
            .manifest
            .file_for(file, self.backend.is_onnx_backend())
            .to_string();
        let mut raw_key = format!("{}|{}", device.id, file);
        if let Some(sh) = shapes {
            raw_key.push('|');
            for (idx, (k, dims)) in sh.iter().enumerate() {
                if idx > 0 {
                    raw_key.push(';');
                }
                raw_key.push_str(k);
                raw_key.push('=');
                for (di, d) in dims.iter().enumerate() {
                    if di > 0 {
                        raw_key.push('x');
                    }
                    raw_key.push_str(&d.to_string());
                }
            }
        }
        let key = raw_key.to_ascii_lowercase();
        if !self.sessions.contains_key(&key) {
            let map: Option<HashMap<String, Vec<i64>>> =
                shapes.map(|sh| sh.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
            let sess = self.backend.load(
                &self.manifest.path_of(&file),
                device,
                self.cache_dir.as_deref(),
                map.as_ref(),
            )?;
            self.sessions.insert(key.clone(), sess);
        }
        Ok(self.sessions.get(&key).expect("session inserted").as_ref())
    }

    fn embed_shapes(
        embeds_name: Option<&str>,
        mask_name: Option<&str>,
        seq: i32,
        dim: i32,
    ) -> Option<BTreeMap<String, Vec<i64>>> {
        let embeds = embeds_name?;
        let mut d = BTreeMap::new();
        d.insert(embeds.to_string(), vec![1, seq as i64, dim as i64]);
        if let Some(mask) = mask_name {
            d.insert(mask.to_string(), vec![1, seq as i64]);
        }
        Some(d)
    }

    fn text_shapes(&self, g: &TextGraph) -> Option<BTreeMap<String, Vec<i64>>> {
        let dim = self.table.as_ref().map(|t| t.dim).unwrap_or(0);
        Self::embed_shapes(g.inputs_embeds.as_deref(), g.attention_mask.as_deref(), g.seq, dim)
    }

    fn max_soft_tokens(v: &VisionGraph) -> i32 {
        if v.positions_input.is_some() {
            v.tokens
        } else if v.soft_tokens > 0 {
            v.soft_tokens
        } else {
            v.tokens
        }
    }

    fn fusion_length(f: &FusionGraph, n: i32) -> i32 {
        f.prefix_ids.len() as i32 + n + f.suffix_ids.len() as i32
    }

    fn fusion_seq(&self, f: &FusionGraph, n: i32) -> i32 {
        let len = Self::fusion_length(f, n);
        self.text
            .iter()
            .filter(|t| {
                t.inputs_embeds.is_some()
                    && t.file.eq_ignore_ascii_case(&f.file)
                    && t.seq >= len
            })
            .map(|t| t.seq)
            .min()
            .unwrap_or(len)
    }

    /// Compiles every graph this embedder will use. On an accelerator, a text length that fails to
    /// compile is dropped as long as one length stays.
    pub fn load_all(&mut self, text: bool, vision: bool) -> Result<(), WorkerError> {
        let dev = self.device.clone();
        let text_dev = self.text_device.clone();
        if vision {
            let v_file = self.vision.file.clone();
            self.session(&v_file, None, &dev)?;
        }
        if text {
            let snapshot = self.text.clone();
            for t in snapshot {
                let shapes = self.text_shapes(&t);
                let can_drop = text_dev.kind != DeviceKind::Cpu
                    && t.inputs_embeds.is_some()
                    && self.text.len() > 1
                    && self.text.last().is_some_and(|last| last.seq != t.seq);
                if let Err(e) = self.session(&t.file, shapes.as_ref(), &text_dev).map(|_| ()) {
                    match e {
                        WorkerError::DeviceFailed(_) if can_drop => {
                            self.text.retain(|x| x.seq != t.seq);
                            self.dropped_lengths.push(t.seq);
                            Log::info(&format!(
                                "{}: text length {} does not compile on {}; using longer lengths",
                                self.manifest.name, t.seq, text_dev.id
                            ));
                        }
                        other => return Err(other),
                    }
                }
            }
        }
        if vision && let Some(ref f) = self.vision.fusion.clone() {
            let seq = self.fusion_seq(f, Self::max_soft_tokens(&self.vision));
            let dim = self.table.as_ref().map(|t| t.dim).unwrap_or(0);
            let shapes = Self::embed_shapes(
                f.inputs_embeds.as_deref(),
                f.attention_mask_input.as_deref(),
                seq,
                dim,
            );
            self.session(&f.file, shapes.as_ref(), &text_dev)?;
        }
        Ok(())
    }

    pub fn embed_text(&mut self, text: &str, kind: TextKind) -> Result<Vec<f32>, WorkerError> {
        let prompts = self.manifest.prompts.clone().unwrap_or_default();
        let template = if kind == TextKind::Query {
            &prompts.query
        } else {
            &prompts.document
        };
        let prompt = template.replace("{text}", text);
        let max_seq = self.text.last().map(|t| t.seq as usize).unwrap_or(0);
        let ids = self.tokenizer.encode(&prompt, max_seq);
        let pad_to_max = self
            .manifest
            .tokenizer
            .as_ref()
            .is_some_and(|t| t.pad_to_max);
        let graph = if pad_to_max {
            self.text.last().expect("text graphs non-empty").clone()
        } else {
            self.text
                .iter()
                .find(|t| t.seq as usize >= ids.len())
                .cloned()
                .ok_or_else(|| WorkerError::Internal("encoded text exceeds longest graph".into()))?
        };
        let seq = graph.seq as usize;
        let mut padded = vec![self.tokenizer.pad_id(); seq];
        let mut mask = vec![0i64; seq];
        for (i, &id) in ids.iter().enumerate() {
            padded[i] = id;
            mask[i] = 1;
        }

        let mut feed = Vec::new();
        if let Some(ref embeds_name) = graph.inputs_embeds {
            let table = self
                .table
                .as_ref()
                .ok_or_else(|| WorkerError::Internal("missing embedding table".into()))?;
            let dim = table.dim as usize;
            let mut emb = vec![0.0f32; seq * dim];
            table.gather(&padded, &mut emb).map_err(WorkerError::Internal)?;
            feed.push(HostTensor::f(
                embeds_name.clone(),
                emb,
                &[1, graph.seq as i64, dim as i64],
            ));
        } else {
            feed.push(HostTensor::l(
                graph.input_ids.clone(),
                padded,
                &[1, graph.seq as i64],
            ));
        }
        if let Some(ref mask_name) = graph.attention_mask {
            feed.push(HostTensor::l(
                mask_name.clone(),
                mask,
                &[1, graph.seq as i64],
            ));
        }
        let shapes = self.text_shapes(&graph);
        let text_dev = self.text_device.clone();
        let outs = self.session(&graph.file, shapes.as_ref(), &text_dev)?.run(&feed)?;
        self.take(&outs, &graph.output)
    }

    /// Embeds an image at the configured token budget, or at `tokens` (bench) when given.
    pub fn embed_image(
        &mut self,
        image: &RgbImage,
        tokens: Option<i32>,
    ) -> Result<Vec<f32>, WorkerError> {
        let v = match tokens {
            Some(t) => Self::pick_vision(&self.manifest.vision, t).clone(),
            None => self.vision.clone(),
        };
        let dev = self.device.clone();
        let (vout, n) = if let Some(ref pos_name) = v.positions_input {
            let p = ImagePipeline::to_patches(image, &v)?;
            let m = v.patch_budget() as i64;
            let patch_dim = (p.patches.len() as i64) / m;
            let mut inputs = vec![
                HostTensor::f(v.input.clone(), p.patches, &[1, m, patch_dim]),
                HostTensor::l(pos_name.clone(), p.positions, &[1, m, 2]),
            ];
            if let Some(ref pool_name) = v.pool_input
                && let Some(pool) = p.pool_weights
            {
                inputs.push(HostTensor::f(
                    pool_name.clone(),
                    pool,
                    &[1, v.tokens as i64, m],
                ));
            }
            let out = self.session(&v.file, None, &dev)?.run(&inputs)?;
            (out, p.soft_tokens)
        } else {
            let pixels = ImagePipeline::to_tensor(image, v.height, v.width, &v.preprocess);
            let inputs = [HostTensor::f(
                v.input.clone(),
                pixels,
                &[1, 3, v.height as i64, v.width as i64],
            )];
            let out = self.session(&v.file, None, &dev)?.run(&inputs)?;
            (out, Self::max_soft_tokens(&v))
        };

        let Some(ref f) = v.fusion else {
            return self.take(&vout, &v.output);
        };
        let soft = vout
            .get(&v.output)
            .and_then(|t| t.floats.as_ref())
            .ok_or_else(|| WorkerError::DeviceFailed(format!("vision output {} missing", v.output)))?;

        let text_dev = self.text_device.clone();
        if let Some(ref embeds_name) = f.inputs_embeds {
            let table = self
                .table
                .as_ref()
                .ok_or_else(|| WorkerError::Internal("missing embedding table".into()))?;
            let dim = table.dim as usize;
            let slots = (soft.len() / dim) as i32;
            let max_soft = Self::max_soft_tokens(&v);
            if slots != max_soft {
                return Err(WorkerError::DeviceFailed(format!(
                    "vision tower gave {slots} soft tokens, manifest says {max_soft}"
                )));
            }
            if n > slots {
                return Err(WorkerError::DeviceFailed(format!(
                    "image needs {n} soft tokens, graph has {slots}"
                )));
            }
            let nu = n as usize;
            if VectorMath::has_non_finite(&soft[..nu * dim]) {
                return Err(WorkerError::DeviceFailed(format!(
                    "{} vision tower output contains NaN",
                    self.device.label()
                )));
            }
            let len = Self::fusion_length(f, n) as usize;
            let seq = self.fusion_seq(f, n);
            let sequ = seq as usize;
            let ids = vec![self.tokenizer.pad_id(); sequ];
            let mut emb = vec![0.0f32; sequ * dim];
            table
                .gather(&f.prefix_ids, &mut emb)
                .map_err(WorkerError::Internal)?;
            let prefix_off = f.prefix_ids.len() * dim;
            emb[prefix_off..prefix_off + nu * dim].copy_from_slice(&soft[..nu * dim]);
            table
                .gather(&f.suffix_ids, &mut emb[(f.prefix_ids.len() + nu) * dim..])
                .map_err(WorkerError::Internal)?;
            if sequ > len {
                table
                    .gather(&ids[len..], &mut emb[len * dim..])
                    .map_err(WorkerError::Internal)?;
            }
            // Keep `ids` mute warning away (`ids` acts as constant pad-id slice when sequ > len).
            let _ = ids.len();
            let mut mask = vec![0i64; sequ];
            mask[..len].fill(1);
            let mut feed = vec![HostTensor::f(
                embeds_name.clone(),
                emb,
                &[1, seq as i64, dim as i64],
            )];
            if let Some(ref mask_name) = f.attention_mask_input {
                feed.push(HostTensor::l(mask_name.clone(), mask, &[1, seq as i64]));
            }
            let shapes = Self::embed_shapes(
                f.inputs_embeds.as_deref(),
                f.attention_mask_input.as_deref(),
                seq,
                table.dim,
            );
            let out = self
                .session(&f.file, shapes.as_ref(), &text_dev)?
                .run(&feed)?;
            self.take(&out, &f.output)
        } else {
            if VectorMath::has_non_finite(soft) {
                return Err(WorkerError::DeviceFailed(format!(
                    "{} vision tower output contains NaN",
                    self.device.label()
                )));
            }
            let mut feed = vec![HostTensor::f(
                f.image_embeds_input.clone(),
                soft.clone(),
                &vout[&v.output].shape,
            )];
            if let Some(ref ids_name) = f.input_ids_input {
                feed.push(HostTensor::l(
                    ids_name.clone(),
                    f.input_ids.clone(),
                    &[1, f.input_ids.len() as i64],
                ));
            }
            if let Some(ref mask_name) = f.attention_mask_input {
                let mask = vec![1i64; f.input_ids.len()];
                let mask_len = mask.len() as i64;
                feed.push(HostTensor::l(mask_name.clone(), mask, &[1, mask_len]));
            }
            let out = self.session(&f.file, None, &text_dev)?.run(&feed)?;
            self.take(&out, &f.output)
        }
    }

    fn take(
        &self,
        outputs: &HashMap<String, HostTensor>,
        name: &str,
    ) -> Result<Vec<f32>, WorkerError> {
        let t = outputs
            .get(name)
            .and_then(|x| x.floats.as_ref())
            .ok_or_else(|| WorkerError::DeviceFailed(format!("output {name} missing")))?;
        let dim = if self.manifest.dim > 0 {
            self.manifest.dim as usize
        } else {
            t.len()
        };
        if t.len() < dim {
            return Err(WorkerError::DeviceFailed(format!(
                "output {name} shorter than {dim}"
            )));
        }
        let mut v = t[..dim].to_vec();
        VectorMath::l2_normalize_in_place(&mut v);
        Ok(v)
    }
}
