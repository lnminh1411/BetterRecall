// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

#![allow(dead_code)]

pub mod a3;
pub mod ports;

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use br_contracts::{
    EmbeddingInfo, NewAudioCapture, NewScreenCapture, SearchMode, SearchRequest, SearchResponse,
};
use br_core::{EncryptedStore, VectorIndex, win32};
use chrono::{DateTime, Utc};

pub const FAKE_WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 MARKER-PIXELS-0123456789";

pub fn test_tmp_root() -> PathBuf {
    if let Some(t) = std::env::var_os("BR_TEST_TMP")
        && !t.is_empty()
    {
        return PathBuf::from(t);
    }
    if let Some(t) = std::env::var_os("CARGO_TARGET_DIR")
        && !t.is_empty()
    {
        return PathBuf::from(t).join("test-tmp");
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.join("test-tmp")))
        .expect("unable to determine cargo target dir for test temp folder")
}

pub struct TempHome {
    path: PathBuf,
}

impl TempHome {
    pub fn new(key_protection: Option<&str>) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let root = test_tmp_root().join("br-core-store");
        fs::create_dir_all(&root).expect("create temp root");
        // A dir left by an earlier run with the same pid may hold a store.db under another key;
        // `create_dir` fails on an existing path, so every home starts empty.
        let path = loop {
            let id = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = root.join(format!("h-{}-{id}", std::process::id()));
            match fs::create_dir(&p) {
                Ok(()) => break p,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create temp home: {e}"),
            }
        };
        if let Some(kp) = key_protection {
            let settings_file = br_contracts::DataPaths::settings_file(&path);
            fs::write(
                settings_file,
                format!("{{\"security\":{{\"keyProtection\":\"{kp}\"}}}}"),
            )
            .expect("write settings");
        }
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        for _ in 0..5 {
            if !self.path.exists() || fs::remove_dir_all(&self.path).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

pub struct TempStore {
    pub home: TempHome,
    pub key: Vec<u8>,
    pub store: Option<EncryptedStore>,
}

impl TempStore {
    pub fn new() -> Self {
        let home = TempHome::new(None);
        let mut key = vec![0u8; 32];
        win32::fill_random(&mut key);
        let store = Self::open_with(&home, &key, None);
        Self {
            home,
            key,
            store: Some(store),
        }
    }

    pub fn open_with(
        home: &TempHome,
        key: &[u8],
        index: Option<Arc<dyn VectorIndex>>,
    ) -> EncryptedStore {
        let k = key.to_vec();
        EncryptedStore::open(home.path(), move || k.clone(), index).expect("open store")
    }

    pub fn store(&self) -> &EncryptedStore {
        self.store.as_ref().expect("store is closed")
    }

    pub fn close(&mut self) {
        self.store = None;
    }

    pub fn reopen(&mut self) {
        self.store = None;
        self.store = Some(Self::open_with(&self.home, &self.key, None));
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        self.store = None;
    }
}

pub fn vec_info() -> EmbeddingInfo {
    EmbeddingInfo {
        model_id: "fake-gemma".to_string(),
        revision: "r1".to_string(),
        dim: 768,
    }
}

struct Lcg(u64);

impl Lcg {
    fn new(seed: i32) -> Self {
        Self((seed as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407))
    }

    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }

    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (self.0 >> 56) as u8;
        }
    }
}

pub fn seeded_bytes(seed: i32, len: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut buf = vec![0u8; len];
    r.fill_bytes(&mut buf);
    buf
}

pub fn vec_seeded(seed: i32) -> Vec<f32> {
    vec_seeded_dim(seed, 768)
}

pub fn vec_seeded_dim(seed: i32, dim: usize) -> Vec<f32> {
    let mut r = Lcg::new(seed);
    let mut v = vec![0.0f32; dim];
    let mut n = 0.0f64;
    for x in &mut v {
        let val = (r.next_f64() * 2.0 - 1.0) as f32;
        *x = val;
        n += f64::from(val) * f64::from(val);
    }
    let inv = (1.0 / n.sqrt()) as f32;
    for x in &mut v {
        *x *= inv;
    }
    v
}

pub fn vec_near(v: &[f32], seed: i32) -> Vec<f32> {
    let mut r = Lcg::new(seed);
    let scale = 0.15f32 / (v.len() as f32).sqrt() * 4.0;
    v.iter()
        .map(|&x| x + (r.next_f64() * 2.0 - 1.0) as f32 * scale)
        .collect()
}

pub fn screen(ts: DateTime<Utc>, app: &str, title: &str, keep: bool) -> NewScreenCapture {
    NewScreenCapture {
        ts,
        app: app.to_string(),
        exe_path: format!(r"C:\Apps\{app}"),
        title: title.to_string(),
        monitor: 0,
        width: 1280,
        height: 720,
        d_hash: 0x1234,
        image: FAKE_WEBP.to_vec(),
        image_mime: "image/webp".to_string(),
        keep_keyframe: keep,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    }
}

pub fn audio(ts: DateTime<Utc>, text: &str, app: &str) -> NewAudioCapture {
    NewAudioCapture {
        ts,
        source: "system".to_string(),
        duration_ms: 4200,
        text: text.to_string(),
        language: "en".to_string(),
        app: app.to_string(),
    }
}

pub fn add_ready(
    s: &EncryptedStore,
    ts: DateTime<Utc>,
    title: &str,
    v: &[f32],
    app: &str,
    keep: bool,
) -> String {
    let id = s
        .add_screen_capture(&screen(ts, app, title, keep))
        .expect("add screen");
    let jobs = s.take_pending_embeddings(10).expect("take jobs");
    let job = jobs.iter().find(|j| j.id == id).expect("job for id");
    assert!(job.image.is_some());
    s.set_embedding(&id, v, &vec_info()).expect("set embedding");
    id
}

pub fn find(
    s: &EncryptedStore,
    q: &str,
    qv: &[f32],
    mode: SearchMode,
    max: i32,
    bytes: usize,
) -> SearchResponse {
    s.search(
        &SearchRequest {
            q: q.to_string(),
            limit: Some(max),
            from: None,
            to: None,
            app: None,
            kind: None,
            mode,
            site: None,
        },
        qv,
        max,
        bytes,
    )
    .expect("search")
}

pub fn raw_bytes(path: &Path) -> Vec<u8> {
    let mut f = OpenOptions::new()
        .read(true)
        .share_mode(0x0000_0001 | 0x0000_0002 | 0x0000_0004)
        .open(path)
        .expect("open raw");
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).expect("read raw");
    buf
}

pub fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02X}");
    }
    out
}
