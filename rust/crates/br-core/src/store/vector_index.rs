// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! In-RAM binary vector index (sign-bit Hamming scan + float prefix cosine).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::RwLock;

/// Filter for the first-stage scan. `app_id` / `site_id` `-1` means any; `-2` matches nothing.
/// `kind_mask` bit 0 screen, bit 1 audio, bit 2 replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorFilter {
    pub from_ms: i64,
    pub to_ms: i64,
    pub kind_mask: i32,
    pub app_id: i32,
    pub site_id: i32,
}

impl VectorFilter {
    pub const ALL: Self = Self {
        from_ms: i64::MIN,
        to_ms: i64::MAX,
        kind_mask: 3,
        app_id: -1,
        site_id: -1,
    };

    pub const fn new(from_ms: i64, to_ms: i64, kind_mask: i32, app_id: i32, site_id: i32) -> Self {
        Self {
            from_ms,
            to_ms,
            kind_mask,
            app_id,
            site_id,
        }
    }
}

/// Candidate from the binary scan: store row id, Hamming distance, bits compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorCandidate {
    pub rid: i64,
    pub distance: i32,
    pub dim: i32,
}

/// First-stage vector search.
pub trait VectorIndex: Send + Sync {
    fn count(&self) -> usize;
    fn upsert(&self, rid: i64, vector: &[f32], ts_ms: i64, kind: i32, app_lower: &str, site: &str);
    #[allow(clippy::too_many_arguments)] // one field per index column, as in C#
    fn upsert_bits(
        &self,
        rid: i64,
        bits: &[u64],
        dim: usize,
        ts_ms: i64,
        kind: i32,
        app_lower: &str,
        site: &str,
    ) {
        let _ = (rid, bits, dim, ts_ms, kind, app_lower, site);
    }
    fn remove(&self, rid: i64);
    fn clear(&self);
    /// Top-N rows by Hamming distance between sign bits; each row compared over `min(query dim, row dim)`.
    fn scan(&self, query: &[f32], filter: VectorFilter, top_n: usize) -> Vec<VectorCandidate>;
    fn app_id(&self, app_lower: &str) -> i32;
    fn site_id(&self, _site: &str) -> i32 {
        -2
    }
}

struct Segment {
    dim: usize,
    words: usize,
    bits: Vec<u64>,
    ts: Vec<i64>,
    rid: Vec<i64>,
    kind: Vec<u8>,
    app: Vec<i32>,
    site: Vec<i32>,
    alive: Vec<bool>,
    count: usize,
    dead: usize,
}

impl Segment {
    fn new(dim: usize) -> Self {
        let words = dim.div_ceil(64);
        let cap = 1024;
        Self {
            dim,
            words,
            bits: vec![0u64; cap * words],
            ts: vec![0i64; cap],
            rid: vec![0i64; cap],
            kind: vec![0u8; cap],
            app: vec![0i32; cap],
            site: vec![0i32; cap],
            alive: vec![false; cap],
            count: 0,
            dead: 0,
        }
    }

    fn grow(&mut self) {
        let cap = self.ts.len() * 2;
        self.bits.resize(cap * self.words, 0);
        self.ts.resize(cap, 0);
        self.rid.resize(cap, 0);
        self.kind.resize(cap, 0);
        self.app.resize(cap, 0);
        self.site.resize(cap, 0);
        self.alive.resize(cap, false);
    }
}

#[derive(Default)]
struct IndexState {
    segments: HashMap<usize, Segment>,
    where_map: HashMap<i64, (usize, usize)>,
    apps: HashMap<String, i32>,
    sites: HashMap<String, i32>,
    app_refs: HashMap<i32, (String, i32)>,
    site_refs: HashMap<i32, (String, i32)>,
    next_app: i32,
    next_site: i32,
}

impl IndexState {
    fn add_ref(
        names: &mut HashMap<String, i32>,
        refs: &mut HashMap<i32, (String, i32)>,
        next: &mut i32,
        name: &str,
    ) -> i32 {
        let id = if let Some(&existing) = names.get(name) {
            existing
        } else {
            let id = *next;
            *next += 1;
            names.insert(name.to_string(), id);
            refs.insert(id, (name.to_string(), 0));
            id
        };
        if let Some(entry) = refs.get_mut(&id) {
            entry.1 += 1;
        }
        id
    }

    fn release(
        names: &mut HashMap<String, i32>,
        refs: &mut HashMap<i32, (String, i32)>,
        id: i32,
    ) {
        let drop_name = match refs.get_mut(&id) {
            Some(entry) if entry.1 > 1 => {
                entry.1 -= 1;
                return;
            }
            Some(_) => refs.remove(&id).map(|(n, _)| n),
            None => return,
        };
        if let Some(name) = drop_name {
            names.remove(&name);
        }
    }

    fn remove_locked(&mut self, rid: i64) {
        let Some((dim, slot)) = self.where_map.remove(&rid) else {
            return;
        };
        let Some(seg) = self.segments.get_mut(&dim) else {
            return;
        };
        seg.alive[slot] = false;
        // A purged row's sign signature does not stay in RAM until compaction (B19).
        let start = slot * seg.words;
        seg.bits[start..start + seg.words].fill(0);
        seg.ts[slot] = 0;
        let app_id = seg.app[slot];
        let site_id = seg.site[slot];
        Self::release(&mut self.apps, &mut self.app_refs, app_id);
        Self::release(&mut self.sites, &mut self.site_refs, site_id);
        seg.dead += 1;
        if seg.dead > 4096 && seg.dead * 4 > seg.count {
            compact_segment(seg, &mut self.where_map);
        }
    }
}

fn compact_segment(seg: &mut Segment, where_map: &mut HashMap<i64, (usize, usize)>) {
    let w = seg.words;
    let mut dst = 0usize;
    for src in 0..seg.count {
        if !seg.alive[src] {
            continue;
        }
        if dst != src {
            seg.bits.copy_within(src * w..src * w + w, dst * w);
            seg.ts[dst] = seg.ts[src];
            seg.rid[dst] = seg.rid[src];
            seg.kind[dst] = seg.kind[src];
            seg.app[dst] = seg.app[src];
            seg.site[dst] = seg.site[src];
            seg.alive[dst] = true;
            where_map.insert(seg.rid[dst], (seg.dim, dst));
        }
        dst += 1;
    }
    seg.alive[dst..seg.count].fill(false);
    seg.bits[dst * w..seg.count * w].fill(0);
    seg.ts[dst..seg.count].fill(0);
    seg.count = dst;
    seg.dead = 0;
}

/// In-RAM binary copy (one sign bit per dimension) per row, grouped by row dim.
pub struct BinaryVectorIndex {
    lock: RwLock<IndexState>,
}

impl Default for BinaryVectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl BinaryVectorIndex {
    pub fn new() -> Self {
        Self {
            lock: RwLock::new(IndexState::default()),
        }
    }

    /// Tests: app and site names currently held.
    pub fn name_counts(&self) -> (usize, usize) {
        let st = self.lock.read().expect("vector index read lock");
        (st.apps.len(), st.sites.len())
    }

    /// Tests: true when any bit word of the segment of this dim beyond its live rows, or of a dead row, is set.
    pub fn has_stale_bits(&self, dim: usize) -> bool {
        let st = self.lock.read().expect("vector index read lock");
        let Some(seg) = st.segments.get(&dim) else {
            return false;
        };
        for i in 0..seg.ts.len() {
            if (i >= seg.count || !seg.alive[i])
                && seg.bits[i * seg.words..(i + 1) * seg.words]
                    .iter()
                    .any(|&w| w != 0)
            {
                return true;
            }
        }
        false
    }

    /// Sign bits of `v[0..dim)`, little-endian bit order within each 64-bit word.
    pub fn sign_bits(v: &[f32], words: &mut [u64]) {
        words.fill(0);
        for (i, &x) in v.iter().enumerate() {
            if x > 0.0 {
                words[i >> 6] |= 1u64 << (i & 63);
            }
        }
    }

    pub fn sign_bits_bytes(v: &[f32]) -> Vec<u8> {
        let mut words = vec![0u64; v.len().div_ceil(64)];
        Self::sign_bits(v, &mut words);
        let mut out = Vec::with_capacity(words.len() * 8);
        for w in words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// Cosine of the first `dim` components of `a` and `b`, each renormalised over that prefix.
    pub fn prefix_cosine(a: &[f32], b: &[f32], dim: usize) -> f32 {
        let a = &a[..dim];
        let b = &b[..dim];
        let mut dot = 0.0f32;
        let mut na = 0.0f32;
        let mut nb = 0.0f32;
        for (&x, &y) in a.iter().zip(b.iter()) {
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na <= 0.0 || nb <= 0.0 {
            return 0.0;
        }
        dot / (na * nb).sqrt()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct HeapItem(VectorCandidate);

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap by distance so `peek()` returns the worst (largest) distance in top-N.
        self.0
            .distance
            .cmp(&other.0.distance)
            .then_with(|| self.0.rid.cmp(&other.0.rid))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl VectorIndex for BinaryVectorIndex {
    fn count(&self) -> usize {
        self.lock
            .read()
            .expect("vector index read lock")
            .where_map
            .len()
    }

    fn app_id(&self, app_lower: &str) -> i32 {
        self.lock
            .read()
            .expect("vector index read lock")
            .apps
            .get(app_lower)
            .copied()
            .unwrap_or(-2)
    }

    fn site_id(&self, site: &str) -> i32 {
        self.lock
            .read()
            .expect("vector index read lock")
            .sites
            .get(site)
            .copied()
            .unwrap_or(-2)
    }

    fn upsert(&self, rid: i64, vector: &[f32], ts_ms: i64, kind: i32, app_lower: &str, site: &str) {
        let words = vector.len().div_ceil(64);
        let mut stack_buf = [0u64; 64];
        let mut heap_buf;
        let bits: &mut [u64] = if words <= 64 {
            &mut stack_buf[..words]
        } else {
            heap_buf = vec![0u64; words];
            &mut heap_buf
        };
        Self::sign_bits(vector, bits);
        self.upsert_bits(rid, bits, vector.len(), ts_ms, kind, app_lower, site);
    }

    fn upsert_bits(
        &self,
        rid: i64,
        bits: &[u64],
        dim: usize,
        ts_ms: i64,
        kind: i32,
        app_lower: &str,
        site: &str,
    ) {
        let mut guard = self.lock.write().expect("vector index write lock");
        let st = &mut *guard;
        st.remove_locked(rid);
        let app_id = IndexState::add_ref(&mut st.apps, &mut st.app_refs, &mut st.next_app, app_lower);
        let site_id = IndexState::add_ref(&mut st.sites, &mut st.site_refs, &mut st.next_site, site);
        let seg = st.segments.entry(dim).or_insert_with(|| Segment::new(dim));
        if seg.count == seg.ts.len() {
            seg.grow();
        }
        let slot = seg.count;
        seg.count += 1;
        let w = seg.words;
        seg.bits[slot * w..slot * w + w].copy_from_slice(&bits[..w]);
        seg.ts[slot] = ts_ms;
        seg.rid[slot] = rid;
        seg.kind[slot] = 1u8 << (kind as u32);
        seg.app[slot] = app_id;
        seg.site[slot] = site_id;
        seg.alive[slot] = true;
        st.where_map.insert(rid, (dim, slot));
    }

    fn remove(&self, rid: i64) {
        let mut st = self.lock.write().expect("vector index write lock");
        st.remove_locked(rid);
    }

    fn clear(&self) {
        let mut st = self.lock.write().expect("vector index write lock");
        st.segments.clear();
        st.where_map.clear();
        st.apps.clear();
        st.sites.clear();
        st.app_refs.clear();
        st.site_refs.clear();
        st.next_app = 0;
        st.next_site = 0;
    }

    fn scan(&self, query: &[f32], filter: VectorFilter, top_n: usize) -> Vec<VectorCandidate> {
        if top_n == 0 || query.is_empty() || filter.app_id == -2 || filter.site_id == -2 {
            return Vec::new();
        }
        let q_words = query.len().div_ceil(64);
        let mut q = vec![0u64; q_words];
        Self::sign_bits(query, &mut q);
        let mut heap: BinaryHeap<HeapItem> = BinaryHeap::with_capacity(top_n + 1);
        {
            let st = self.lock.read().expect("vector index read lock");
            for seg in st.segments.values() {
                let dim = seg.dim.min(query.len());
                let full_words = dim / 64;
                let tail_bits = dim % 64;
                let tail_mask = if tail_bits == 0 {
                    0
                } else {
                    (1u64 << tail_bits) - 1
                };
                scan_segment(
                    seg, &q, full_words, tail_bits, tail_mask, dim, filter, top_n, &mut heap,
                );
            }
        }
        let mut result: Vec<VectorCandidate> = heap.into_iter().map(|h| h.0).collect();
        result.sort_by_key(|c| c.distance);
        result
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_segment(
    seg: &Segment,
    q: &[u64],
    full_words: usize,
    tail_bits: usize,
    tail_mask: u64,
    dim: usize,
    f: VectorFilter,
    top_n: usize,
    heap: &mut BinaryHeap<HeapItem>,
) {
    let w = seg.words;
    let mut worst = if heap.len() == top_n {
        heap.peek().map_or(i32::MAX, |t| t.0.distance)
    } else {
        i32::MAX
    };
    let scale = (768 * 1024) / dim.max(1) as i64;
    for i in 0..seg.count {
        if !seg.alive[i] {
            continue;
        }
        let t = seg.ts[i];
        if t < f.from_ms || t >= f.to_ms {
            continue;
        }
        if (i32::from(seg.kind[i]) & f.kind_mask) == 0 {
            continue;
        }
        if f.app_id >= 0 && seg.app[i] != f.app_id {
            continue;
        }
        if f.site_id >= 0 && seg.site[i] != f.site_id {
            continue;
        }
        let row = &seg.bits[i * w..i * w + w];
        let mut d = hamming(row, q, full_words);
        if tail_bits != 0 {
            d += ((row[full_words] ^ q[full_words]) & tail_mask).count_ones() as i32;
        }
        let nd = ((i64::from(d) * scale) >> 10) as i32;
        if heap.len() < top_n {
            heap.push(HeapItem(VectorCandidate {
                rid: seg.rid[i],
                distance: nd,
                dim: dim as i32,
            }));
            if heap.len() == top_n {
                worst = heap.peek().map_or(i32::MAX, |t| t.0.distance);
            }
        } else if nd < worst {
            heap.pop();
            heap.push(HeapItem(VectorCandidate {
                rid: seg.rid[i],
                distance: nd,
                dim: dim as i32,
            }));
            worst = heap.peek().map_or(i32::MAX, |t| t.0.distance);
        }
    }
}

#[inline]
fn hamming(a: &[u64], b: &[u64], words: usize) -> i32 {
    let mut d = 0u32;
    let mut j = 0usize;
    while j + 4 <= words {
        d += (a[j] ^ b[j]).count_ones()
            + (a[j + 1] ^ b[j + 1]).count_ones()
            + (a[j + 2] ^ b[j + 2]).count_ones()
            + (a[j + 3] ^ b[j + 3]).count_ones();
        j += 4;
    }
    while j < words {
        d += (a[j] ^ b[j]).count_ones();
        j += 1;
    }
    d as i32
}
