// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fs;
use std::path::Path;

use unicode_normalization::UnicodeNormalization;

use crate::models::manifest::TokenizerSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelType {
    Unigram = 1,
    Bpe = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PieceType {
    Normal = 1,
    Unknown = 2,
    Control = 3,
    UserDefined = 4,
    Unused = 5,
    Byte = 6,
}

struct ProtoModel {
    pieces: Vec<(String, f32, PieceType)>,
    model_type: ModelType,
    split_digits: bool,
    byte_fallback: bool,
    treat_whitespace_as_suffix: bool,
    unk_id: u32,
    norm_name: String,
    precompiled_charsmap: Vec<u8>,
    add_dummy_prefix: bool,
    remove_extra_whitespaces: bool,
    escape_whitespaces: bool,
}

struct TrieNode {
    id_and_score: Option<(u32, f64)>,
    children: Vec<(u8, u32)>,
}

struct PrefixTrie {
    nodes: Vec<TrieNode>,
}

impl PrefixTrie {
    fn new() -> Self {
        Self {
            nodes: vec![TrieNode {
                id_and_score: None,
                children: Vec::new(),
            }],
        }
    }

    fn insert(&mut self, piece: &str, id: u32, score: f64) {
        let mut cur = 0usize;
        for &b in piece.as_bytes() {
            if let Ok(pos) = self.nodes[cur]
                .children
                .binary_search_by_key(&b, |&(k, _)| k)
            {
                cur = self.nodes[cur].children[pos].1 as usize;
            } else {
                let next = self.nodes.len() as u32;
                self.nodes.push(TrieNode {
                    id_and_score: None,
                    children: Vec::new(),
                });
                let children = &mut self.nodes[cur].children;
                let ins = children.partition_point(|&(k, _)| k < b);
                children.insert(ins, (b, next));
                cur = next as usize;
            }
        }
        self.nodes[cur].id_and_score = Some((id, score));
    }

    fn lookup_prefixes<F>(&self, bytes: &[u8], mut f: F)
    where
        F: FnMut(usize, u32, f64),
    {
        let mut cur = 0usize;
        for (idx, &b) in bytes.iter().enumerate() {
            let children = &self.nodes[cur].children;
            let Ok(pos) = children.binary_search_by_key(&b, |&(k, _)| k) else {
                break;
            };
            cur = children[pos].1 as usize;
            if let Some((id, score)) = self.nodes[cur].id_and_score {
                f(idx + 1, id, score);
            }
        }
    }
}

struct CharsMap {
    trie: Vec<u32>,
    pool: Vec<u8>,
}

impl CharsMap {
    fn parse(blob: &[u8]) -> Option<Self> {
        if blob.len() < 4 {
            return None;
        }
        let trie_bytes = u32::from_le_bytes(blob[..4].try_into().ok()?) as usize;
        if trie_bytes == 0 || !trie_bytes.is_multiple_of(4) || 4 + trie_bytes > blob.len() {
            return None;
        }
        let trie = blob[4..4 + trie_bytes]
            .as_chunks::<4>().0.iter()
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let pool = blob[4 + trie_bytes..].to_vec();
        Some(Self { trie, pool })
    }

    fn normalize(&self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut pos = 0;
        while pos < bytes.len() {
            if let Some((matched, repl)) = self.common_prefix_search(&bytes[pos..]) {
                out.push_str(repl);
                pos += matched;
            } else {
                let ch = input[pos..].chars().next().unwrap();
                out.push(ch);
                pos += ch.len_utf8();
            }
        }
        out
    }

    fn common_prefix_search(&self, input: &[u8]) -> Option<(usize, &str)> {
        if self.trie.is_empty() {
            return None;
        }
        let offset = |u: u32| -> usize { ((u >> 10) << ((u & (1 << 9)) >> 6)) as usize };
        let mut node_pos = offset(self.trie[0]);
        let mut best: Option<(usize, &str)> = None;
        for (i, &b) in input.iter().enumerate() {
            node_pos ^= b as usize;
            if node_pos >= self.trie.len() {
                break;
            }
            let unit = self.trie[node_pos];
            if (unit & 0xFF) as u8 != b {
                break;
            }
            if (unit & (1 << 8)) != 0 {
                let leaf_pos = node_pos ^ offset(unit);
                if leaf_pos < self.trie.len() {
                    let val = (self.trie[leaf_pos] & 0x7FFF_FFFF) as usize;
                    if val < self.pool.len() {
                        let end = self.pool[val..]
                            .iter()
                            .position(|&x| x == 0)
                            .map(|n| val + n)
                            .unwrap_or(self.pool.len());
                        if let Ok(s) = std::str::from_utf8(&self.pool[val..end]) {
                            best = Some((i + 1, s));
                        }
                    }
                }
            }
            node_pos ^= offset(unit);
        }
        best
    }
}

/// Pure-Rust SentencePiece encoder supporting both Unigram and BPE `.model` protobufs.
pub struct GemmaTokenizer {
    spec: TokenizerSpec,
    model_type: ModelType,
    split_digits: bool,
    byte_fallback: bool,
    treat_whitespace_as_suffix: bool,
    unk_id: u32,
    unk_score: f64,
    norm_name: String,
    charsmap: Option<CharsMap>,
    add_dummy_prefix: bool,
    remove_extra_whitespaces: bool,
    escape_whitespaces: bool,
    vocab: HashMap<String, (u32, f32)>,
    trie: PrefixTrie,
    user_defined: Vec<(String, u32)>,
    byte_ids: [Option<u32>; 256],
}

impl GemmaTokenizer {
    pub fn from_file(model_file: &Path, spec: TokenizerSpec) -> Result<Self, String> {
        let bytes = fs::read(model_file).map_err(|e| format!("read tokenizer: {e}"))?;
        let model = parse_model_proto(&bytes)?;
        Ok(Self::from_proto(model, spec))
    }

    fn from_proto(model: ProtoModel, spec: TokenizerSpec) -> Self {
        let mut vocab = HashMap::new();
        let mut trie = PrefixTrie::new();
        let mut user_defined = Vec::new();
        let mut byte_ids = [None; 256];
        let mut min_score = 0.0f32;
        let mut has_normal = false;

        for (id_usize, (piece, score, ty)) in model.pieces.into_iter().enumerate() {
            let id = id_usize as u32;
            match ty {
                PieceType::Normal => {
                    if !has_normal || score < min_score {
                        min_score = score;
                        has_normal = true;
                    }
                    trie.insert(&piece, id, score as f64);
                    vocab.insert(piece, (id, score));
                }
                PieceType::UserDefined => {
                    if !piece.is_empty() {
                        user_defined.push((piece, id));
                    }
                }
                PieceType::Byte => {
                    if let Some(hex) = piece.strip_prefix("<0x").and_then(|s| s.strip_suffix('>'))
                        && let Ok(b) = u8::from_str_radix(hex, 16)
                    {
                        byte_ids[b as usize] = Some(id);
                    }
                }
                PieceType::Unknown | PieceType::Control | PieceType::Unused => {}
            }
        }
        // Longest user-defined piece first when splitting raw input segments.
        user_defined.sort_by_key(|e| std::cmp::Reverse(e.0.len()));

        let charsmap = if model.precompiled_charsmap.is_empty() {
            None
        } else {
            CharsMap::parse(&model.precompiled_charsmap)
        };

        Self {
            spec,
            model_type: model.model_type,
            split_digits: model.split_digits,
            byte_fallback: model.byte_fallback,
            treat_whitespace_as_suffix: model.treat_whitespace_as_suffix,
            unk_id: model.unk_id,
            unk_score: (min_score as f64) - 10.0,
            norm_name: model.norm_name,
            charsmap,
            add_dummy_prefix: model.add_dummy_prefix,
            remove_extra_whitespaces: model.remove_extra_whitespaces,
            escape_whitespaces: model.escape_whitespaces,
            vocab,
            trie,
            user_defined,
            byte_ids,
        }
    }

    pub fn pad_id(&self) -> i64 {
        self.spec.pad_id
    }

    /// Ids with BOS/EOS per spec, truncated to `max_len` (EOS kept as the last id).
    pub fn encode(&self, text: &str, max_len: usize) -> Vec<i64> {
        let owned;
        let input = if self.spec.lowercase {
            owned = text.to_lowercase();
            owned.as_str()
        } else {
            text
        };
        let body = self.encode_raw(input);
        let extra = usize::from(self.spec.add_bos) + usize::from(self.spec.add_eos);
        let take = body.len().min(max_len.saturating_sub(extra));
        let mut ids = Vec::with_capacity(take + extra);
        if self.spec.add_bos {
            ids.push(self.spec.bos_id);
        }
        for &id in &body[..take] {
            ids.push(id as i64);
        }
        if self.spec.add_eos {
            ids.push(self.spec.eos_id);
        }
        ids
    }

    fn encode_raw(&self, text: &str) -> Vec<u32> {
        let norm = self.normalize(text);
        if norm.is_empty() {
            return Vec::new();
        }
        if self.user_defined.is_empty() {
            return self.encode_segment(&norm);
        }
        let mut out = Vec::new();
        let mut start = 0;
        let mut pos = 0;
        while pos < norm.len() {
            if let Some((piece, id)) = self
                .user_defined
                .iter()
                .find(|(p, _)| norm[pos..].starts_with(p.as_str()))
            {
                if pos > start {
                    out.extend(self.encode_segment(&norm[start..pos]));
                }
                out.push(*id);
                pos += piece.len();
                start = pos;
            } else {
                let ch = norm[pos..].chars().next().unwrap();
                pos += ch.len_utf8();
            }
        }
        if pos > start {
            out.extend(self.encode_segment(&norm[start..pos]));
        }
        out
    }

    fn normalize(&self, text: &str) -> String {
        let mapped = if let Some(ref cm) = self.charsmap {
            cm.normalize(text)
        } else if self.norm_name.contains("nfkc") && self.norm_name != "identity" {
            text.nfkc().collect::<String>()
        } else {
            text.to_string()
        };

        let mut spaced = if self.remove_extra_whitespaces {
            let mut s = String::with_capacity(mapped.len());
            let mut prev_space = true;
            for ch in mapped.chars() {
                if ch == ' ' {
                    if !prev_space {
                        s.push(' ');
                        prev_space = true;
                    }
                } else {
                    s.push(ch);
                    prev_space = false;
                }
            }
            if s.ends_with(' ') {
                s.pop();
            }
            s
        } else {
            mapped
        };

        if !spaced.is_empty() && self.add_dummy_prefix {
            if self.treat_whitespace_as_suffix {
                spaced.push(' ');
            } else {
                spaced.insert(0, ' ');
            }
        }

        if self.escape_whitespaces {
            spaced.replace(' ', "\u{2581}")
        } else {
            spaced
        }
    }

    fn encode_segment(&self, seg: &str) -> Vec<u32> {
        match self.model_type {
            ModelType::Bpe => self.encode_bpe(seg),
            ModelType::Unigram => self.encode_unigram(seg),
        }
    }

    fn encode_bpe(&self, seg: &str) -> Vec<u32> {
        struct Symbol {
            text: String,
            prev: isize,
            next: isize,
            alive: bool,
            no_merge: bool,
        }

        #[derive(Clone, Copy)]
        struct Pair {
            score: f32,
            left: usize,
            right: usize,
            left_len: usize,
            right_len: usize,
        }

        impl PartialEq for Pair {
            fn eq(&self, other: &Self) -> bool {
                self.left == other.left
                    && self.right == other.right
                    && self.score.to_bits() == other.score.to_bits()
            }
        }
        impl Eq for Pair {}
        impl PartialOrd for Pair {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for Pair {
            fn cmp(&self, other: &Self) -> Ordering {
                self.score
                    .total_cmp(&other.score)
                    .then_with(|| other.left.cmp(&self.left))
            }
        }

        let chars: Vec<char> = seg.chars().collect();
        if chars.is_empty() {
            return Vec::new();
        }
        let n = chars.len();
        let mut syms: Vec<Symbol> = chars
            .into_iter()
            .enumerate()
            .map(|(i, ch)| Symbol {
                text: ch.to_string(),
                prev: i as isize - 1,
                next: if i + 1 < n { (i + 1) as isize } else { -1 },
                alive: true,
                no_merge: self.split_digits && ch.is_ascii_digit(),
            })
            .collect();

        let mut heap = BinaryHeap::new();
        let push_pair = |left: usize,
                         right: usize,
                         syms: &[Symbol],
                         heap: &mut BinaryHeap<Pair>| {
            if syms[left].no_merge || syms[right].no_merge {
                return;
            }
            let merged = format!("{}{}", syms[left].text, syms[right].text);
            if let Some(&(_, score)) = self.vocab.get(&merged) {
                heap.push(Pair {
                    score,
                    left,
                    right,
                    left_len: syms[left].text.len(),
                    right_len: syms[right].text.len(),
                });
            }
        };

        for i in 0..n - 1 {
            push_pair(i, i + 1, &syms, &mut heap);
        }

        while let Some(pair) = heap.pop() {
            let l = pair.left;
            let r = pair.right;
            if !syms[l].alive
                || !syms[r].alive
                || syms[l].next != r as isize
                || syms[l].text.len() != pair.left_len
                || syms[r].text.len() != pair.right_len
            {
                continue;
            }
            let right_text = std::mem::take(&mut syms[r].text);
            syms[l].text.push_str(&right_text);
            syms[r].alive = false;
            let next_after_r = syms[r].next;
            syms[l].next = next_after_r;
            if next_after_r >= 0 {
                syms[next_after_r as usize].prev = l as isize;
            }
            if syms[l].prev >= 0 {
                push_pair(syms[l].prev as usize, l, &syms, &mut heap);
            }
            if syms[l].next >= 0 {
                push_pair(l, syms[l].next as usize, &syms, &mut heap);
            }
        }

        let mut out = Vec::with_capacity(syms.len());
        let mut cur = 0isize;
        while cur >= 0 {
            let sym = &syms[cur as usize];
            if sym.alive {
                if let Some(&(id, _)) = self.vocab.get(&sym.text) {
                    out.push(id);
                } else {
                    self.emit_unk_piece(&sym.text, &mut out);
                }
            }
            cur = sym.next;
        }
        out
    }

    fn encode_unigram(&self, seg: &str) -> Vec<u32> {
        let len = seg.len();
        if len == 0 {
            return Vec::new();
        }
        let mut best_score = vec![f64::NEG_INFINITY; len + 1];
        let mut back_prev = vec![0usize; len + 1];
        let mut back_id = vec![self.unk_id; len + 1];
        let mut back_is_unk = vec![false; len + 1];
        best_score[0] = 0.0;

        let bytes = seg.as_bytes();
        let mut pos = 0usize;
        while pos < len {
            let ch = seg[pos..].chars().next().unwrap();
            let ch_len = ch.len_utf8();
            if best_score[pos] == f64::NEG_INFINITY {
                pos += ch_len;
                continue;
            }
            let starts_digit = self.split_digits && ch.is_ascii_digit();
            let mut has_single_char = false;
            self.trie.lookup_prefixes(&bytes[pos..], |m_len, id, score| {
                if starts_digit && m_len > ch_len {
                    return;
                }
                if self.split_digits
                    && m_len > ch_len
                    && seg[pos..pos + m_len].bytes().any(|b| b.is_ascii_digit())
                {
                    return;
                }
                if m_len == ch_len {
                    has_single_char = true;
                }
                let next_pos = pos + m_len;
                let cand = best_score[pos] + score;
                if cand > best_score[next_pos] {
                    best_score[next_pos] = cand;
                    back_prev[next_pos] = pos;
                    back_id[next_pos] = id;
                    back_is_unk[next_pos] = false;
                }
            });
            if !has_single_char {
                let next_pos = pos + ch_len;
                let cand = best_score[pos] + self.unk_score;
                if cand > best_score[next_pos] {
                    best_score[next_pos] = cand;
                    back_prev[next_pos] = pos;
                    back_id[next_pos] = self.unk_id;
                    back_is_unk[next_pos] = true;
                }
            }
            pos += ch_len;
        }

        let mut steps = Vec::new();
        let mut cur = len;
        while cur > 0 {
            let prev = back_prev[cur];
            steps.push((prev, cur, back_id[cur], back_is_unk[cur]));
            cur = prev;
        }
        steps.reverse();

        let mut out = Vec::with_capacity(steps.len());
        let mut prev_was_unk = false;
        for (start, end, id, is_unk) in steps {
            if is_unk {
                if self.byte_fallback {
                    self.emit_unk_piece(&seg[start..end], &mut out);
                    prev_was_unk = false;
                } else if !prev_was_unk {
                    out.push(self.unk_id);
                    prev_was_unk = true;
                }
            } else {
                out.push(id);
                prev_was_unk = false;
            }
        }
        out
    }

    fn emit_unk_piece(&self, s: &str, out: &mut Vec<u32>) {
        if self.byte_fallback {
            for &b in s.as_bytes() {
                out.push(self.byte_ids[b as usize].unwrap_or(self.unk_id));
            }
        } else {
            out.push(self.unk_id);
        }
    }
}

fn parse_model_proto(bytes: &[u8]) -> Result<ProtoModel, String> {
    let mut model = ProtoModel {
        pieces: Vec::new(),
        model_type: ModelType::Unigram,
        split_digits: false,
        byte_fallback: false,
        treat_whitespace_as_suffix: false,
        unk_id: 0,
        norm_name: String::new(),
        precompiled_charsmap: Vec::new(),
        add_dummy_prefix: true,
        remove_extra_whitespaces: true,
        escape_whitespaces: true,
    };

    let mut pos = 0usize;
    while pos < bytes.len() {
        let (field_num, wire_type) = read_tag(bytes, &mut pos)?;
        match (field_num, wire_type) {
            (1, 2) => {
                let slice = read_bytes(bytes, &mut pos)?;
                model.pieces.push(parse_piece(slice)?);
            }
            (2, 2) => {
                let slice = read_bytes(bytes, &mut pos)?;
                parse_trainer_spec(slice, &mut model)?;
            }
            (3, 2) => {
                let slice = read_bytes(bytes, &mut pos)?;
                parse_normalizer_spec(slice, &mut model)?;
            }
            (_, wt) => skip_field(bytes, &mut pos, wt)?,
        }
    }
    Ok(model)
}

fn parse_piece(bytes: &[u8]) -> Result<(String, f32, PieceType), String> {
    let mut piece = String::new();
    let mut score = 0.0f32;
    let mut ty = PieceType::Normal;
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (field_num, wire_type) = read_tag(bytes, &mut pos)?;
        match (field_num, wire_type) {
            (1, 2) => {
                let b = read_bytes(bytes, &mut pos)?;
                piece = String::from_utf8_lossy(b).into_owned();
            }
            (2, 5) => score = read_f32(bytes, &mut pos)?,
            (3, 0) => {
                ty = match read_varint(bytes, &mut pos)? {
                    1 => PieceType::Normal,
                    2 => PieceType::Unknown,
                    3 => PieceType::Control,
                    4 => PieceType::UserDefined,
                    5 => PieceType::Unused,
                    6 => PieceType::Byte,
                    _ => PieceType::Normal,
                };
            }
            (_, wt) => skip_field(bytes, &mut pos, wt)?,
        }
    }
    Ok((piece, score, ty))
}

fn parse_trainer_spec(bytes: &[u8], model: &mut ProtoModel) -> Result<(), String> {
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (field_num, wire_type) = read_tag(bytes, &mut pos)?;
        match (field_num, wire_type) {
            (3, 0) => {
                model.model_type = match read_varint(bytes, &mut pos)? {
                    2 => ModelType::Bpe,
                    _ => ModelType::Unigram,
                };
            }
            (24, 0) => model.treat_whitespace_as_suffix = read_varint(bytes, &mut pos)? != 0,
            (25, 0) => model.split_digits = read_varint(bytes, &mut pos)? != 0,
            (35, 0) => model.byte_fallback = read_varint(bytes, &mut pos)? != 0,
            (40, 0) => model.unk_id = read_varint(bytes, &mut pos)? as u32,
            (_, wt) => skip_field(bytes, &mut pos, wt)?,
        }
    }
    Ok(())
}

fn parse_normalizer_spec(bytes: &[u8], model: &mut ProtoModel) -> Result<(), String> {
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (field_num, wire_type) = read_tag(bytes, &mut pos)?;
        match (field_num, wire_type) {
            (1, 2) => {
                let b = read_bytes(bytes, &mut pos)?;
                model.norm_name = String::from_utf8_lossy(b).into_owned();
            }
            (2, 2) => model.precompiled_charsmap = read_bytes(bytes, &mut pos)?.to_vec(),
            (3, 0) => model.add_dummy_prefix = read_varint(bytes, &mut pos)? != 0,
            (4, 0) => model.remove_extra_whitespaces = read_varint(bytes, &mut pos)? != 0,
            (5, 0) => model.escape_whitespaces = read_varint(bytes, &mut pos)? != 0,
            (_, wt) => skip_field(bytes, &mut pos, wt)?,
        }
    }
    Ok(())
}

fn read_tag(bytes: &[u8], pos: &mut usize) -> Result<(u32, u8), String> {
    let v = read_varint(bytes, pos)?;
    Ok(((v >> 3) as u32, (v & 7) as u8))
}

fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut val = 0u64;
    let mut shift = 0u32;
    while *pos < bytes.len() {
        let b = bytes[*pos];
        *pos += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            return Ok(val);
        }
        shift += 7;
        if shift >= 64 {
            return Err("varint overflow".to_string());
        }
    }
    Err("unexpected eof reading varint".to_string())
}

fn read_bytes<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<&'a [u8], String> {
    let len = read_varint(bytes, pos)? as usize;
    let end = pos
        .checked_add(len)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| "unexpected eof reading bytes".to_string())?;
    let slice = &bytes[*pos..end];
    *pos = end;
    Ok(slice)
}

fn read_f32(bytes: &[u8], pos: &mut usize) -> Result<f32, String> {
    let end = pos
        .checked_add(4)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| "unexpected eof reading f32".to_string())?;
    let v = f32::from_le_bytes([
        bytes[*pos],
        bytes[*pos + 1],
        bytes[*pos + 2],
        bytes[*pos + 3],
    ]);
    *pos = end;
    Ok(v)
}

fn skip_field(bytes: &[u8], pos: &mut usize, wire_type: u8) -> Result<(), String> {
    match wire_type {
        0 => {
            read_varint(bytes, pos)?;
            Ok(())
        }
        1 => {
            *pos = pos
                .checked_add(8)
                .filter(|&e| e <= bytes.len())
                .ok_or_else(|| "unexpected eof skipping fixed64".to_string())?;
            Ok(())
        }
        2 => {
            read_bytes(bytes, pos)?;
            Ok(())
        }
        5 => {
            read_f32(bytes, pos)?;
            Ok(())
        }
        wt => Err(format!("unsupported protobuf wire type {wt}")),
    }
}
