// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::protocol::WorkerError;

/// One token with its UTF-16 span `[start, end)` in the encoded text (empty for the virtual
/// word-start marker and for bos/eos).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BpeToken {
    pub id: i32,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy)]
struct Sym {
    id: i32,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MergeCandidate {
    rank: i32,
    left: usize,
    right: usize,
    ver_l: i32,
    ver_r: i32,
    id: i32,
}

impl Ord for MergeCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap by (rank, left): reverse ordering on BinaryHeap (max-heap).
        other
            .rank
            .cmp(&self.rank)
            .then_with(|| other.left.cmp(&self.left))
    }
}

impl PartialOrd for MergeCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Byte-fallback BPE from a Hugging Face `tokenizer.json` of the Gemma family (as the NER model
/// ships it). Matches C# `BpeTokenizer` and Python `tokenizers` on all worker tests.
pub struct BpeTokenizer {
    vocab: HashMap<String, i32>,
    merges: HashMap<u64, (i32, i32)>,
    added: Vec<(String, usize, i32)>, // (text, utf16_len, id), longest first
    byte_ids: [i32; 256],
    meta_id: i32,
    pub bos_id: i32,
    pub eos_id: i32,
    pub unk_id: i32,
}

impl BpeTokenizer {
    const META: char = '\u{2581}';

    pub fn load(path: &Path) -> Result<Self, WorkerError> {
        let bytes = fs::read(path).map_err(|e| WorkerError::Internal(format!("read tokenizer.json: {e}")))?;
        let root: Value = serde_json::from_slice(&bytes)
            .map_err(|e| WorkerError::Internal(format!("parse tokenizer.json: {e}")))?;
        let model = root
            .get("model")
            .ok_or_else(|| WorkerError::Internal("tokenizer.json: missing model".into()))?;
        if model.get("type").and_then(Value::as_str) != Some("BPE") {
            return Err(WorkerError::Internal("tokenizer.json: not a BPE model".into()));
        }

        let vocab_obj = model
            .get("vocab")
            .and_then(Value::as_object)
            .ok_or_else(|| WorkerError::Internal("tokenizer.json: missing vocab".into()))?;
        let mut vocab = HashMap::with_capacity(vocab_obj.len() + 16);
        for (k, v) in vocab_obj {
            let id = v
                .as_i64()
                .ok_or_else(|| WorkerError::Internal("tokenizer.json: non-int vocab id".into()))?;
            vocab.insert(k.clone(), id as i32);
        }

        let mut added_map: HashMap<String, i32> = HashMap::new();
        if let Some(at) = root.get("added_tokens").and_then(Value::as_array) {
            for a in at {
                let content = a
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let id = a
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| WorkerError::Internal("tokenizer.json: added_tokens id".into()))?
                    as i32;
                added_map.insert(content.clone(), id);
                vocab.entry(content).or_insert(id);
            }
        }

        let mut added: Vec<(String, usize, i32)> = added_map
            .into_iter()
            .filter(|(k, _)| !k.is_empty())
            .map(|(k, id)| {
                let u16_len = k.encode_utf16().count();
                (k, u16_len, id)
            })
            .collect();
        added.sort_by(|a, b| b.1.cmp(&a.1));

        let merges_arr = model
            .get("merges")
            .and_then(Value::as_array)
            .ok_or_else(|| WorkerError::Internal("tokenizer.json: missing merges".into()))?;
        let mut merges = HashMap::with_capacity(merges_arr.len());
        for (rank, m) in merges_arr.iter().enumerate() {
            let (a, b): (&str, &str) = if let Some(pair) = m.as_array() {
                let Some(a) = pair.first().and_then(Value::as_str) else {
                    return Err(WorkerError::Internal("tokenizer.json: bad merge entry".into()));
                };
                let Some(b) = pair.get(1).and_then(Value::as_str) else {
                    return Err(WorkerError::Internal("tokenizer.json: bad merge entry".into()));
                };
                (a, b)
            } else if let Some(s) = m.as_str() {
                s.split_once(' ')
                    .ok_or_else(|| WorkerError::Internal("tokenizer.json: bad merge string".into()))?
            } else {
                return Err(WorkerError::Internal("tokenizer.json: unsupported merge".into()));
            };
            if let (Some(&ia), Some(&ib)) = (vocab.get(a), vocab.get(b)) {
                let merged = format!("{a}{b}");
                if let Some(&ic) = vocab.get(&merged) {
                    let key = ((ia as u64) << 32) | (ib as u32 as u64);
                    merges.entry(key).or_insert((rank as i32, ic));
                }
            }
        }

        let id_of = |t: &str, fallback: i32| *vocab.get(t).unwrap_or(&fallback);
        let bos_id = id_of("<bos>", 2);
        let eos_id = id_of("<eos>", 1);
        let unk_id = id_of("<unk>", 3);
        let mut byte_ids = [unk_id; 256];
        for (b, slot) in byte_ids.iter_mut().enumerate() {
            let key = format!("<0x{b:02X}>");
            if let Some(&id) = vocab.get(&key) {
                *slot = id;
            }
        }
        let mut meta_buf = [0u8; 4];
        let meta_str = Self::META.encode_utf8(&mut meta_buf);
        let meta_id = *vocab.get(meta_str).unwrap_or(&unk_id);

        Ok(Self {
            vocab,
            merges,
            added,
            byte_ids,
            meta_id,
            bos_id,
            eos_id,
            unk_id,
        })
    }

    /// Tokens of `text` without bos/eos.
    pub fn encode(&self, text: &str) -> Vec<BpeToken> {
        let mut tokens = Vec::new();
        let mut byte_pos = 0usize;
        let mut u16_pos = 0usize;
        let mut seg_byte = 0usize;
        let mut seg_u16 = 0usize;

        while byte_pos < text.len() {
            let tail = &text[byte_pos..];
            if let Some((a, a_u16, id)) = self
                .added
                .iter()
                .find(|(a, _, _)| tail.starts_with(a.as_str()))
            {
                self.encode_segment(&text[seg_byte..byte_pos], seg_u16, &mut tokens);
                tokens.push(BpeToken {
                    id: *id,
                    start: u16_pos,
                    end: u16_pos + *a_u16,
                });
                byte_pos += a.len();
                u16_pos += *a_u16;
                seg_byte = byte_pos;
                seg_u16 = u16_pos;
            } else {
                let ch = tail.chars().next().unwrap();
                byte_pos += ch.len_utf8();
                u16_pos += ch.len_utf16();
            }
        }
        self.encode_segment(&text[seg_byte..], seg_u16, &mut tokens);
        tokens
    }

    fn encode_segment(&self, seg: &str, base_u16: usize, tokens: &mut Vec<BpeToken>) {
        if seg.is_empty() {
            return;
        }
        let mut word_byte = 0usize;
        let mut word_u16 = base_u16;
        let mut cur_u16 = base_u16;
        let first_ch = seg.chars().next().unwrap();
        let mut virtual_meta = first_ch != ' ' && first_ch != Self::META;

        for (idx, ch) in seg.char_indices() {
            if idx > 0 && (ch == ' ' || ch == Self::META) {
                self.encode_word(&seg[word_byte..idx], word_u16, virtual_meta, tokens);
                virtual_meta = false;
                word_byte = idx;
                word_u16 = cur_u16;
            }
            cur_u16 += ch.len_utf16();
        }
        self.encode_word(&seg[word_byte..], word_u16, virtual_meta, tokens);
    }

    fn encode_word(&self, word: &str, word_u16: usize, virtual_meta: bool, tokens: &mut Vec<BpeToken>) {
        let mut syms = Vec::with_capacity(word.len() + 1);
        if virtual_meta {
            syms.push(Sym {
                id: self.meta_id,
                start: word_u16,
                end: word_u16,
            });
        }
        let mut u16_pos = word_u16;
        let mut buf = [0u8; 4];
        for ch in word.chars() {
            let u16_len = ch.len_utf16();
            let key: &str = if ch == ' ' {
                Self::META.encode_utf8(&mut buf)
            } else {
                ch.encode_utf8(&mut buf)
            };
            if let Some(&id) = self.vocab.get(key) {
                syms.push(Sym {
                    id,
                    start: u16_pos,
                    end: u16_pos + u16_len,
                });
            } else {
                for &b in key.as_bytes() {
                    syms.push(Sym {
                        id: self.byte_ids[b as usize],
                        start: u16_pos,
                        end: u16_pos + u16_len,
                    });
                }
            }
            u16_pos += u16_len;
        }
        self.merge(&mut syms);
        for s in syms {
            tokens.push(BpeToken {
                id: s.id,
                start: s.start,
                end: s.end,
            });
        }
    }

    /// Priority-queue merge matching lowest rank first, leftmost on ties (C9).
    fn merge(&self, syms: &mut Vec<Sym>) {
        let n = syms.len();
        if n < 2 {
            return;
        }
        let mut next: Vec<isize> = (0..n)
            .map(|i| if i + 1 < n { (i + 1) as isize } else { -1 })
            .collect();
        let mut prev: Vec<isize> = (0..n).map(|i| i as isize - 1).collect();
        let mut ver = vec![0i32; n];
        let mut heap = BinaryHeap::new();

        let push = |l: isize,
                    syms: &[Sym],
                    next: &[isize],
                    ver: &[i32],
                    heap: &mut BinaryHeap<MergeCandidate>| {
            if l < 0 {
                return;
            }
            let lu = l as usize;
            let r = next[lu];
            if r < 0 {
                return;
            }
            let ru = r as usize;
            let key = ((syms[lu].id as u64) << 32) | (syms[ru].id as u32 as u64);
            if let Some(&(rank, id)) = self.merges.get(&key) {
                heap.push(MergeCandidate {
                    rank,
                    left: lu,
                    right: ru,
                    ver_l: ver[lu],
                    ver_r: ver[ru],
                    id,
                });
            }
        };

        for i in 0..(n - 1) {
            push(i as isize, syms, &next, &ver, &mut heap);
        }

        while let Some(e) = heap.pop() {
            if ver[e.left] != e.ver_l
                || ver[e.right] != e.ver_r
                || next[e.left] != e.right as isize
            {
                continue;
            }
            syms[e.left] = Sym {
                id: e.id,
                start: syms[e.left].start,
                end: syms[e.right].end,
            };
            ver[e.left] += 1;
            ver[e.right] = -1;
            let nr = next[e.right];
            next[e.left] = nr;
            if nr >= 0 {
                prev[nr as usize] = e.left as isize;
            }
            push(prev[e.left], syms, &next, &ver, &mut heap);
            push(e.left as isize, syms, &next, &ver, &mut heap);
        }

        let mut out = Vec::with_capacity(n);
        let mut cur = 0isize;
        while cur >= 0 {
            out.push(syms[cur as usize]);
            cur = next[cur as usize];
        }
        *syms = out;
    }
}
