// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::Path;

use br_contracts::OcrEntity;

use crate::inference::{DeviceInfo, HostTensor, IInferenceBackend, IInferenceSession};
use crate::models::manifest::{ModelManifest, NerSpec};
use crate::models::ner::bpe_tokenizer::{BpeToken, BpeTokenizer};
use crate::models::ocr::PaddleOcr;
use crate::protocol::WorkerError;

/// Per label index: mapped entity (`person` | `address`), base label, and whether it begins a span (`B-` / `S-`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEntry {
    pub entity: String,
    pub base: String,
    pub begin: bool,
}

/// Person names and postal addresses in OCR lines: token classification over joined lines,
/// cut into windows at line ends when possible. Recall first (`MinTagProbability = 0.3`).
pub struct PiiNer {
    spec: NerSpec,
    session: Box<dyn IInferenceSession>,
    tok: BpeTokenizer,
    tags: Vec<Option<TagEntry>>,
}

impl PiiNer {
    pub const MIN_TAG_PROBABILITY: f32 = 0.3;

    pub fn new(
        backend: &dyn IInferenceBackend,
        device: &DeviceInfo,
        manifest: &ModelManifest,
        cache_dir: Option<&Path>,
    ) -> Result<Self, WorkerError> {
        let spec = manifest
            .ner
            .clone()
            .ok_or_else(|| WorkerError::Internal("ner manifest lacks ner".into()))?;
        if spec.labels.is_empty() {
            return Err(WorkerError::Internal("ner manifest has no labels".into()));
        }
        let tags = Self::tags(&spec);
        let tok = BpeTokenizer::load(&manifest.path_of(&spec.tokenizer))?;
        let session = backend.load(&manifest.path_of(&spec.file), device, cache_dir, None)?;
        Ok(Self {
            spec,
            session,
            tok,
            tags,
        })
    }

    pub fn device(&self) -> &DeviceInfo {
        self.session.device()
    }

    pub fn tags(spec: &NerSpec) -> Vec<Option<TagEntry>> {
        spec.labels
            .iter()
            .map(|l| {
                let b = l.as_bytes();
                let prefixed =
                    b.len() > 2 && b[1] == b'-' && matches!(b[0], b'B' | b'I' | b'E' | b'S');
                let base = if prefixed { &l[2..] } else { l.as_str() };
                spec.entities.get(base).map(|e| TagEntry {
                    entity: e.clone(),
                    base: base.to_string(),
                    begin: !prefixed || matches!(b[0], b'B' | b'S'),
                })
            })
            .collect()
    }

    /// Entities per line (`None` for lines without entities).
    pub fn find(&self, lines: &[String]) -> Result<Vec<Option<Vec<OcrEntity>>>, WorkerError> {
        let mut result: Vec<Option<Vec<OcrEntity>>> = vec![None; lines.len()];
        if lines.is_empty() {
            return Ok(result);
        }
        let mut line_start_u16 = vec![0usize; lines.len()];
        let mut line_len_u16 = vec![0usize; lines.len()];
        let mut doc = String::new();
        let mut cur_u16 = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                doc.push('\n');
                cur_u16 += 1;
            }
            line_start_u16[i] = cur_u16;
            let sanitized = line.replace('\n', " ");
            let u16_len = sanitized.encode_utf16().count();
            line_len_u16[i] = u16_len;
            doc.push_str(&sanitized);
            cur_u16 += u16_len;
        }
        let doc_u16: Vec<u16> = doc.encode_utf16().collect();
        let tokens = self.tok.encode(&doc);
        let mut spans = Vec::new();
        let max = ((self.spec.max_tokens - 2).max(16)) as usize;
        let mut from = 0usize;
        while from < tokens.len() {
            let mut to = (from + max).min(tokens.len());
            if to < tokens.len() {
                for k in ((from + max / 2 + 1)..to).rev() {
                    let t = &tokens[k];
                    if t.end <= doc_u16.len()
                        && t.start < t.end
                        && doc_u16[t.start..t.end].contains(&(b'\n' as u16))
                    {
                        to = k + 1;
                        break;
                    }
                }
            }
            spans.extend(self.run_window(&tokens, from, to)?);
            from = to;
        }
        for (s, e, entity, score) in spans {
            for i in 0..lines.len() {
                let ls = line_start_u16[i];
                let le = ls + line_len_u16[i];
                let a = s.max(ls);
                let b = e.min(le);
                if b <= a {
                    continue;
                }
                let list = result[i].get_or_insert_with(Vec::new);
                list.push(OcrEntity {
                    start: (a - ls) as i32,
                    length: (b - a) as i32,
                    label: entity.clone(),
                    score,
                });
            }
        }
        Ok(result)
    }

    fn run_window(
        &self,
        tokens: &[BpeToken],
        from: usize,
        to: usize,
    ) -> Result<Vec<(usize, usize, String, f32)>, WorkerError> {
        let n = to - from + 2;
        let mut ids = vec![0i64; n];
        let mask = vec![1i64; n];
        ids[0] = self.tok.bos_id as i64;
        ids[n - 1] = self.tok.eos_id as i64;
        for (i, tok) in tokens[from..to].iter().enumerate() {
            ids[i + 1] = tok.id as i64;
        }
        let mut feed = vec![HostTensor::l(
            self.spec.input_ids.clone(),
            ids,
            &[1, n as i64],
        )];
        if self
            .session
            .inputs()
            .iter()
            .any(|x| x.name == self.spec.attention_mask)
        {
            feed.push(HostTensor::l(
                self.spec.attention_mask.clone(),
                mask,
                &[1, n as i64],
            ));
        }
        let outp = self.session.run(&feed)?;
        let Some(t) = outp.get(&self.spec.output) else {
            return Err(WorkerError::DeviceFailed("ner output missing".into()));
        };
        let Some(ref floats) = t.floats else {
            return Err(WorkerError::DeviceFailed("ner output missing".into()));
        };
        if t.shape.len() != 3 || t.shape[1] != n as i64 {
            return Err(WorkerError::DeviceFailed("ner output missing".into()));
        }
        let labels = t.shape[2] as usize;
        if labels != self.spec.labels.len() {
            return Err(WorkerError::DeviceFailed(format!(
                "ner output has {labels} labels, manifest {}",
                self.spec.labels.len()
            )));
        }
        let check_len = floats.len().min(n * labels);
        if !PaddleOcr::all_finite(&floats[..check_len]) {
            return Err(WorkerError::DeviceFailed("ner output not finite".into()));
        }
        let mut tags = Vec::with_capacity(to - from);
        for i in 0..(to - from) {
            let row = floats
                .get((i + 1) * labels..(i + 2) * labels)
                .ok_or_else(|| WorkerError::DeviceFailed("ner output missing".into()))?;
            tags.push(Self::pick(row, &self.tags)?);
        }
        Ok(Self::spans(tokens, from, &tags, &self.tags))
    }

    /// Softmax over one token's logits; the most likely mapped tag when its probability reaches
    /// `MIN_TAG_PROBABILITY`, else `(-1, 0.0)`.
    pub fn pick(logits: &[f32], map: &[Option<TagEntry>]) -> Result<(i32, f32), WorkerError> {
        let mut mx = f32::NEG_INFINITY;
        for &v in logits {
            if !v.is_finite() {
                return Err(WorkerError::DeviceFailed("ner output not finite".into()));
            }
            if v > mx {
                mx = v;
            }
        }
        if !mx.is_finite() {
            return Err(WorkerError::DeviceFailed("ner output not finite".into()));
        }
        let sum: f64 = logits.iter().map(|&v| ((v - mx) as f64).exp()).sum();
        let mut best = -1i32;
        let mut bp = 0.0f64;
        for (k, &v) in logits.iter().enumerate() {
            if map[k].is_none() {
                continue;
            }
            let p = ((v - mx) as f64).exp() / sum;
            if p > bp {
                bp = p;
                best = k as i32;
            }
        }
        if bp >= Self::MIN_TAG_PROBABILITY as f64 {
            Ok((best, bp as f32))
        } else {
            Ok((-1, 0.0))
        }
    }

    /// Consecutive tagged tokens of the same base label form one span (a `B-`/`S-` tag starts a new one).
    pub fn spans(
        tokens: &[BpeToken],
        from: usize,
        tags: &[(i32, f32)],
        map: &[Option<TagEntry>],
    ) -> Vec<(usize, usize, String, f32)> {
        let mut spans = Vec::new();
        let mut s = 0usize;
        let mut e = 0usize;
        let mut ent: Option<&str> = None;
        let mut bas: Option<&str> = None;
        let mut score = 0.0f64;
        let mut count = 0usize;

        let flush = |spans: &mut Vec<(usize, usize, String, f32)>,
                     s: usize,
                     e: usize,
                     ent: &mut Option<&str>,
                     bas: &mut Option<&str>,
                     score: &mut f64,
                     count: &mut usize| {
            if let Some(entity) = *ent
                && e > s
            {
                spans.push((s, e, entity.to_string(), (*score / *count as f64) as f32));
            }
            *ent = None;
            *bas = None;
            *score = 0.0;
            *count = 0;
        };

        for (i, &(tag, p)) in tags.iter().enumerate() {
            let tok = tokens[from + i];
            let Some(m) = (if tag >= 0 {
                map.get(tag as usize).and_then(Option::as_ref)
            } else {
                None
            }) else {
                flush(&mut spans, s, e, &mut ent, &mut bas, &mut score, &mut count);
                continue;
            };
            if ent.is_none() || m.begin || bas != Some(m.base.as_str()) {
                flush(&mut spans, s, e, &mut ent, &mut bas, &mut score, &mut count);
                s = tok.start;
                e = tok.start;
                ent = Some(m.entity.as_str());
                bas = Some(m.base.as_str());
            }
            e = e.max(tok.end);
            score += p as f64;
            count += 1;
        }
        flush(&mut spans, s, e, &mut ent, &mut bas, &mut score, &mut count);
        spans
    }
}
