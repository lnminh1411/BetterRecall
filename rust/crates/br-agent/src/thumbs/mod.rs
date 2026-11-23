// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Byte-bounded RAM-only LRU thumbnail cache and [`AgentThumbnailer`] (`Thumbs\Thumbnailer.cs`).

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use br_contracts::{Settings, ThumbData};
use br_core::{CancelToken, EncryptedStore, Thumbnailer};
use zeroize::Zeroize;

use crate::capture::ImageCodec;

/// Minimal store surface needed by [`AgentThumbnailer`] and [`crate::privacy::AgentDescribeRedactor`].
pub trait KeyframeBlobStore: Send + Sync {
    fn get_keyframe(&self, id: &str) -> Result<Option<ThumbData>, String>;
}

impl KeyframeBlobStore for EncryptedStore {
    fn get_keyframe(&self, id: &str) -> Result<Option<ThumbData>, String> {
        self.get_keyframe(id).map_err(|e| e.to_string())
    }
}

struct LruInner<K> {
    max_bytes: usize,
    bytes: usize,
    order: VecDeque<K>,
    map: HashMap<K, ThumbData>,
}

/// Byte-bounded LRU cache. Cleared values are zeroed in place. Thread-safe.
pub struct ByteLruCache<K> {
    inner: Mutex<LruInner<K>>,
}

impl<K: Eq + Hash + Clone> ByteLruCache<K> {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(LruInner {
                max_bytes,
                bytes: 0,
                order: VecDeque::new(),
                map: HashMap::new(),
            }),
        }
    }

    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }

    pub fn count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }

    pub fn try_get(&self, key: &K) -> Option<ThumbData> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let val = g.map.get(key)?.clone();
        if let Some(pos) = g.order.iter().position(|k| k == key) {
            g.order.remove(pos);
        }
        g.order.push_front(key.clone());
        Some(val)
    }

    pub fn put(&self, key: K, value: ThumbData) {
        let size = value.bytes.len();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = g.map.remove(&key) {
            if let Some(pos) = g.order.iter().position(|k| k == &key) {
                g.order.remove(pos);
            }
            g.bytes = g.bytes.saturating_sub(old.bytes.len());
        }
        if size > g.max_bytes {
            return;
        }
        g.order.push_front(key.clone());
        g.map.insert(key, value);
        g.bytes += size;
        Self::trim_locked(&mut g);
    }

    pub fn resize(&self, max_bytes: usize) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.max_bytes = max_bytes;
        Self::trim_locked(&mut g);
    }

    pub fn clear(&self) {
        let _ = self.clear_for_test();
    }

    /// Clears the cache, zeroizing all cached byte buffers and returning each zeroized
    /// buffer paired with its original length before drop (for tests).
    pub fn clear_for_test(&self) -> Vec<(Vec<u8>, usize)> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::with_capacity(g.map.len());
        for (_, mut v) in g.map.drain() {
            let orig_len = v.bytes.len();
            v.bytes.zeroize();
            out.push((v.bytes, orig_len));
        }
        g.order.clear();
        g.bytes = 0;
        out
    }

    fn trim_locked(g: &mut LruInner<K>) {
        while g.bytes > g.max_bytes {
            let Some(last_key) = g.order.pop_back() else {
                break;
            };
            if let Some(removed) = g.map.remove(&last_key) {
                g.bytes = g.bytes.saturating_sub(removed.bytes.len());
            }
        }
    }
}

struct Semaphore {
    count: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(max: usize) -> Self {
        Self {
            count: Mutex::new(max),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self, ct: &CancelToken) -> bool {
        let mut c = self.count.lock().unwrap_or_else(|e| e.into_inner());
        while *c == 0 {
            if ct.is_cancelled() {
                return false;
            }
            let (next, _) = self
                .cv
                .wait_timeout(c, std::time::Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner());
            c = next;
        }
        *c -= 1;
        true
    }

    fn release(&self) {
        let mut c = self.count.lock().unwrap_or_else(|e| e.into_inner());
        *c += 1;
        self.cv.notify_one();
    }
}

struct SemGuard<'a>(&'a Semaphore);
impl Drop for SemGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

type BlobStoreProvider = Arc<dyn Fn() -> Option<Arc<dyn KeyframeBlobStore>> + Send + Sync>;
type SettingsProvider = Arc<dyn Fn() -> Settings + Send + Sync>;

/// On-demand previews from stored keyframes. RAM only, never written to disk (`Thumbnailer.cs`).
pub struct AgentThumbnailer {
    store: BlobStoreProvider,
    settings: SettingsProvider,
    cache: ByteLruCache<(String, i32)>,
    encode_slots: Semaphore,
    generation: AtomicI64,
}

impl AgentThumbnailer {
    pub fn new(store: BlobStoreProvider, settings: SettingsProvider) -> Self {
        let cap_mb = settings().keyframes.thumb_cache_mb.max(0) as usize;
        Self {
            store,
            settings,
            cache: ByteLruCache::new(cap_mb * 1024 * 1024),
            encode_slots: Semaphore::new(2),
            generation: AtomicI64::new(0),
        }
    }

    pub fn cache(&self) -> &ByteLruCache<(String, i32)> {
        &self.cache
    }
}

impl Thumbnailer for AgentThumbnailer {
    fn get_thumb(&self, id: &str, width: i32, ct: &CancelToken) -> Option<ThumbData> {
        let width = width.clamp(64, 1280);
        let cap_mb = (self.settings)().keyframes.thumb_cache_mb.max(0) as usize;
        self.cache.resize(cap_mb * 1024 * 1024);

        let generation = self.generation.load(Ordering::Acquire);
        let key = (id.to_string(), width);
        if let Some(hit) = self.cache.try_get(&key) {
            return Some(hit);
        }
        let store = (self.store)()?;
        if !self.encode_slots.acquire(ct) {
            return None;
        }
        let _guard = SemGuard(&self.encode_slots);
        let mut kf = store.get_keyframe(id).ok()??;
        let resized = ImageCodec::resize_to_webp(&kf.bytes, &kf.mime, width, 70);
        kf.bytes.zeroize();
        let (bytes, w, h) = resized.ok()?;
        let thumb = ThumbData {
            id: id.to_string(),
            mime: "image/webp".to_string(),
            width: w,
            height: h,
            bytes,
        };
        // A purge (`clear_cache`) ran while this was read: do not cache a possibly-purged row (A10).
        if self.generation.load(Ordering::Acquire) == generation {
            self.cache.put(key, thumb.clone());
        }
        Some(thumb)
    }

    fn clear_cache(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.cache.clear();
    }

    fn transcode(&self, keyframe: ThumbData, target: &str) -> ThumbData {
        let to = match target {
            "jpeg" if keyframe.mime != "image/jpeg" => Some("jpeg"),
            "ui" if !ImageCodec::commonly_decodable(&keyframe.mime) => Some("webp"),
            _ => None,
        };
        let Some(to_fmt) = to else {
            return keyframe;
        };
        match ImageCodec::transcode(&keyframe.bytes, &keyframe.mime, to_fmt, 90) {
            Ok((bytes, mime, width, height)) => ThumbData {
                id: keyframe.id,
                mime: mime.to_string(),
                width,
                height,
                bytes,
            },
            Err(_) => keyframe,
        }
    }
}
