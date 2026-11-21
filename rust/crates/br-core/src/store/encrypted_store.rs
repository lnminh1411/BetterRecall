// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Encrypted store: `store.db` plus `keyframes\YYYY-MM.db`, SQLite3MultipleCiphers with the
//! SQLCipher scheme (`legacy=4`) and a raw 256-bit key, WAL, `secure_delete=ON`.
//! Rows stay invisible to Search, Get, Timeline and GetKeyframe until `state = ready`.
//! All public methods are thread-safe.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{
    AuditEntry, CaptureDetail, CaptureKind, CaptureMeta, DataPaths, EmbeddingInfo, FacetCount,
    FacetsReply, HistoryEntry, NewAudioCapture, NewReplayFrame, NewScreenCapture, PendingJob,
    PurgeNotice, PurgeScope, PurgedWindowError, SearchHit, SearchMode, SearchRequest,
    SearchResponse, Settings, StoreStats, ThumbData,
};
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use zeroize::{Zeroize, Zeroizing};

use crate::core_json::CoreJson;
use crate::crypto::KeyVault;
use crate::log::Log;
use crate::sqlite::{Connection, SqliteError, Statement, Transaction, Value};
use crate::store::ids::Ids;
use crate::store::storage::MonthRebuild;
use crate::store::vector_index::{BinaryVectorIndex, VectorFilter, VectorIndex};
use crate::store::{StoreError, TimedMutex, TimedMutexGuard};

pub(crate) const STATE_PENDING: i32 = 0;
pub(crate) const STATE_FAILED: i32 = 2;

pub(crate) const KIND_SCREEN: i32 = 0;
pub(crate) const KIND_AUDIO: i32 = 1;
pub(crate) const KIND_REPLAY: i32 = 2;

/// Every kind bit (`VectorFilter.kind_mask`, FTS filter): screen 1, audio 2, replay 4.
pub(crate) const ALL_KINDS: i32 = 7;

const RRF_K: usize = 60;
const FTS_CANDIDATES: usize = 200;
const FTS_FILTERED_CANDIDATES: usize = 4000;
const VECTOR_SCAN_TOP: usize = 400;
const VECTOR_KEEP: usize = 200;
const SNIPPET_MAX: usize = 300;

pub(crate) type KeyFn = Arc<dyn Fn() -> Result<Zeroizing<Vec<u8>>, StoreError> + Send + Sync>;
pub(crate) type PurgeListener = Arc<dyn Fn(&PurgeNotice) + Send + Sync>;
type MonoClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;
pub(crate) type RebuildHook = Arc<dyn Fn() + Send + Sync>;

pub(crate) fn kind_of(k: i32) -> CaptureKind {
    match k {
        KIND_AUDIO => CaptureKind::Audio,
        KIND_REPLAY => CaptureKind::Replay,
        _ => CaptureKind::Screen,
    }
}

pub(crate) fn kind_bit(k: CaptureKind) -> i32 {
    match k {
        CaptureKind::Audio => 2,
        CaptureKind::Replay => 4,
        CaptureKind::Screen => 1,
    }
}

pub(crate) struct WriteState {
    pub(crate) w: Option<Connection>,
    pub(crate) kf: HashMap<String, Connection>,
    pub(crate) inflight: HashMap<i64, i64>,
    pub(crate) space: Option<EmbeddingInfo>,
    pub(crate) disposed: bool,
    pub(crate) fences: Vec<(i64, i64, i64)>,
    pub(crate) scope_fences: Vec<(String, String, String, i64, i64)>,
    pub(crate) deleted: Vec<(String, i64)>,
    pub(crate) rebuild: Option<MonthRebuild>,
    pub(crate) rebuild_pauses: usize,
    pub(crate) orphans_swept: bool,
    pub(crate) mono_ms: MonoClockFn,
    pub(crate) after_rebuild_batch: Option<RebuildHook>,
}

impl WriteState {
    pub(crate) fn w(&self) -> Result<&Connection, StoreError> {
        if self.disposed {
            return Err(StoreError::Disposed);
        }
        self.w.as_ref().ok_or(StoreError::Disposed)
    }

    pub(crate) fn check_open(&self) -> Result<(), StoreError> {
        if self.disposed {
            return Err(StoreError::Disposed);
        }
        Ok(())
    }
}

/// Encrypted SQLite3MC store with monthly keyframe DBs and an in-RAM binary vector index.
pub struct EncryptedStore {
    pub(crate) home: PathBuf,
    pub(crate) kf_dir: PathBuf,
    pub(crate) key_fn: KeyFn,
    pub(crate) index: Arc<dyn VectorIndex>,
    pub(crate) wl: TimedMutex<WriteState>,
    pub(crate) rl: Mutex<Option<Connection>>,
    pub(crate) vl: Mutex<Option<Connection>>,
    pub(crate) priority_waiters: AtomicI32,
    pub(crate) maintenance_cancelled: AtomicBool,
    pub(crate) embedding_lease_timeout_ms: AtomicI64,
    pub(crate) vacuum_ram_budget_bytes: AtomicI64,
    pub(crate) legacy_rebuild_min_free_bytes: AtomicI64,
    pub(crate) rebuild_batch_rows: AtomicUsize,
    listeners: Mutex<Vec<(u64, PurgeListener)>>,
    next_listener_id: AtomicU64,
    last_timings: Mutex<Option<String>>,
    last_vector_timings: Mutex<Option<String>>,
    boot_instant: Instant,
}

impl EncryptedStore {
    /// A range purge ending this close to now (or later) fences its window like "purge last N".
    pub const FENCE_SLACK_MS: i64 = 5 * 60 * 1000;
    /// How long a fence refuses in-flight items: longer than OCR, encode and transcription can hold one.
    pub const FENCE_LIFETIME_MS: i64 = 15 * 60 * 1000;

    /// Opens (or creates) the store using a raw 32-byte key callback.
    pub fn open(
        home: &Path,
        key: impl Fn() -> Vec<u8> + Send + Sync + 'static,
        index: Option<Arc<dyn VectorIndex>>,
    ) -> Result<Self, StoreError> {
        let key_fn: KeyFn = Arc::new(move || Ok(Zeroizing::new(key())));
        Self::open_inner(home, key_fn, index)
    }

    /// Opens (or creates) the store using a `KeyVault` (`betterrecall-sqlite` HKDF subkey).
    /// Maps `SQLITE_NOTADB` (26) to `StoreError::KeyLost` so callers can distinguish a swapped/wrong key.
    pub fn open_with_vault(
        home: &Path,
        vault: Arc<KeyVault>,
        index: Option<Arc<dyn VectorIndex>>,
    ) -> Result<Self, StoreError> {
        if !vault.key_loaded() {
            return Err(StoreError::NotOpen);
        }
        let key_fn: KeyFn = Arc::new(move || {
            vault
                .derive("betterrecall-sqlite")
                .map_err(StoreError::Crypto)
        });
        Self::open_inner(home, key_fn, index).map_err(|e| {
            if e.is_not_a_db() {
                StoreError::KeyLost("The data key does not open this store.".to_string())
            } else {
                e
            }
        })
    }

    fn open_inner(
        home: &Path,
        key_fn: KeyFn,
        index: Option<Arc<dyn VectorIndex>>,
    ) -> Result<Self, StoreError> {
        let kf_dir = DataPaths::keyframes_dir(home);
        fs::create_dir_all(home)?;
        fs::create_dir_all(&kf_dir)?;

        let boot_instant = Instant::now();
        let default_mono: MonoClockFn = Arc::new(move || boot_instant.elapsed().as_millis() as i64);

        let store = Self {
            home: home.to_path_buf(),
            kf_dir,
            key_fn,
            index: index.unwrap_or_else(|| Arc::new(BinaryVectorIndex::new())),
            wl: TimedMutex::new(WriteState {
                w: None,
                kf: HashMap::new(),
                inflight: HashMap::new(),
                space: None,
                disposed: false,
                fences: Vec::new(),
                scope_fences: Vec::new(),
                deleted: Vec::new(),
                rebuild: None,
                rebuild_pauses: 0,
                orphans_swept: false,
                mono_ms: default_mono,
                after_rebuild_batch: None,
            }),
            rl: Mutex::new(None),
            vl: Mutex::new(None),
            priority_waiters: AtomicI32::new(0),
            maintenance_cancelled: AtomicBool::new(false),
            embedding_lease_timeout_ms: AtomicI64::new(2 * 60 * 1000),
            vacuum_ram_budget_bytes: AtomicI64::new(128 * 1024 * 1024),
            legacy_rebuild_min_free_bytes: AtomicI64::new(64 * 1024 * 1024),
            rebuild_batch_rows: AtomicUsize::new(32),
            listeners: Mutex::new(Vec::new()),
            next_listener_id: AtomicU64::new(1),
            last_timings: Mutex::new(None),
            last_vector_timings: Mutex::new(None),
            boot_instant,
        };
        store.open_main()?;
        Ok(store)
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn index(&self) -> &Arc<dyn VectorIndex> {
        &self.index
    }

    pub fn space(&self) -> Option<EmbeddingInfo> {
        self.wl.lock().space.clone()
    }

    pub fn embedding_lease_timeout(&self) -> Duration {
        Duration::from_millis(self.embedding_lease_timeout_ms.load(Ordering::Relaxed).max(0) as u64)
    }

    pub fn set_embedding_lease_timeout(&self, timeout: Duration) {
        self.embedding_lease_timeout_ms
            .store(timeout.as_millis() as i64, Ordering::Relaxed);
    }

    pub fn set_vacuum_ram_budget_bytes(&self, bytes: i64) {
        self.vacuum_ram_budget_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn set_legacy_rebuild_min_free_bytes(&self, bytes: i64) {
        self.legacy_rebuild_min_free_bytes
            .store(bytes, Ordering::Relaxed);
    }

    pub fn set_rebuild_batch_rows(&self, rows: usize) {
        self.rebuild_batch_rows.store(rows.max(1), Ordering::Relaxed);
    }

    pub fn set_after_rebuild_batch(&self, hook: Option<RebuildHook>) {
        self.wl.lock().after_rebuild_batch = hook;
    }

    pub fn rebuild_pauses(&self) -> usize {
        self.wl.lock().rebuild_pauses
    }

    pub fn priority_waiters(&self) -> i32 {
        self.priority_waiters.load(Ordering::Acquire)
    }

    pub fn set_mono_ms_for_test(&self, clock: impl Fn() -> i64 + Send + Sync + 'static) {
        self.wl.lock().mono_ms = Arc::new(clock);
    }

    pub fn last_timings(&self) -> Option<String> {
        self.last_timings.lock().expect("timings").clone()
    }

    pub fn last_vector_timings(&self) -> Option<String> {
        self.last_vector_timings.lock().expect("vec timings").clone()
    }

    /// Registers a purge listener (called outside the store lock). Returns a subscription id.
    pub fn on_purged(&self, listener: impl Fn(&PurgeNotice) + Send + Sync + 'static) -> u64 {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners
            .lock()
            .expect("listeners")
            .push((id, Arc::new(listener)));
        id
    }

    pub fn remove_purged_listener(&self, id: u64) {
        self.listeners
            .lock()
            .expect("listeners")
            .retain(|(lid, _)| *lid != id);
    }

    // ---------------------------------------------------------------- connections

    pub(crate) fn open_encrypted(
        &self,
        path: &Path,
        incremental_vacuum: bool,
    ) -> Result<Connection, StoreError> {
        let conn = Connection::open(path)?;
        let key = (self.key_fn)()?;
        let mut hex = Zeroizing::new(String::with_capacity(key.len() * 2 + 16));
        hex.push_str("PRAGMA key=\"x'");
        for &b in key.as_slice() {
            let _ = write!(&mut *hex, "{b:02X}");
        }
        hex.push_str("'\"");
        conn.exec_batch("PRAGMA cipher='sqlcipher'")?;
        conn.exec_batch("PRAGMA legacy=4")?;
        conn.exec_batch(&hex)?;
        drop(hex);
        drop(key);

        let _ = conn.query_scalar_i64("SELECT count(*) FROM sqlite_master", &[])?;
        if incremental_vacuum {
            conn.exec_batch("PRAGMA auto_vacuum=INCREMENTAL")?;
        }
        conn.exec_batch("PRAGMA journal_mode=WAL")?;
        conn.exec_batch("PRAGMA secure_delete=ON")?;
        conn.exec_batch("PRAGMA synchronous=NORMAL")?;
        conn.busy_timeout(5000)?;
        Ok(conn)
    }

    fn open_main(&self) -> Result<(), StoreError> {
        let path = DataPaths::store_db(&self.home);
        let w = self.open_encrypted(&path, false)?;
        w.exec_batch(
            "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS captures(
               rid INTEGER PRIMARY KEY,
               id TEXT NOT NULL UNIQUE,
               ts INTEGER NOT NULL,
               kind INTEGER NOT NULL,
               app TEXT NOT NULL,
               app_lc TEXT NOT NULL,
               exe TEXT NOT NULL,
               title TEXT NOT NULL,
               text TEXT,
               source TEXT,
               language TEXT,
               monitor INTEGER NOT NULL,
               width INTEGER NOT NULL,
               height INTEGER NOT NULL,
               dhash INTEGER NOT NULL,
               duration_ms INTEGER NOT NULL,
               state INTEGER NOT NULL,
               has_keyframe INTEGER NOT NULL,
               keep_keyframe INTEGER NOT NULL,
               kf_month TEXT);
             CREATE INDEX IF NOT EXISTS ix_captures_state_ts ON captures(state, ts);
             CREATE INDEX IF NOT EXISTS ix_captures_ts ON captures(ts);
             CREATE INDEX IF NOT EXISTS ix_captures_app ON captures(app_lc, ts);
             CREATE INDEX IF NOT EXISTS ix_captures_month ON captures(kf_month);
             CREATE TABLE IF NOT EXISTS vectors(
               rid INTEGER PRIMARY KEY,
               model_id TEXT NOT NULL,
               revision TEXT NOT NULL,
               dim INTEGER NOT NULL,
               vec BLOB NOT NULL,
               bin BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS jobs(rid INTEGER PRIMARY KEY, created INTEGER NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(title, app, text, tokenize='unicode61 remove_diacritics 2');
             CREATE TABLE IF NOT EXISTS audit(
               seq INTEGER PRIMARY KEY,
               ts INTEGER NOT NULL,
               pipe TEXT NOT NULL,
               pid INTEGER NOT NULL,
               image TEXT NOT NULL,
               verb TEXT NOT NULL,
               count INTEGER NOT NULL,
               sha TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS ix_audit_ts ON audit(ts);",
        )?;
        w.exec_batch("INSERT OR IGNORE INTO meta(k, v) VALUES('schema', '1')")?;
        self.migrate(&w)?;

        let r = self.open_encrypted(&path, false)?;
        r.exec_batch("PRAGMA query_only=ON")?;
        r.exec_batch("PRAGMA cache_size=-8192")?;

        let rv = self.open_encrypted(&path, false)?;
        rv.exec_batch("PRAGMA query_only=ON")?;
        rv.exec_batch("PRAGMA cache_size=-16384")?;

        let space = read_space(&w)?;
        let mut st = self.wl.lock();
        st.space = space;
        st.w = Some(w);
        *self.rl.lock().expect("rl") = Some(r);
        *self.vl.lock().expect("vl") = Some(rv);
        self.load_index_locked(&st)?;
        Ok(())
    }

    fn migrate(&self, w: &Connection) -> Result<(), StoreError> {
        let mut cols = HashSet::new();
        {
            let mut stmt = w.prepare("SELECT name FROM pragma_table_info('captures')")?;
            while stmt.step()? {
                cols.insert(stmt.get_string(0)?);
            }
        }
        if !cols.contains("site") {
            w.exec_batch("ALTER TABLE captures ADD COLUMN site TEXT NOT NULL DEFAULT ''")?;
        }
        Self::migrate_storage(w, &cols)?;
        w.exec_batch(
            "CREATE INDEX IF NOT EXISTS ix_captures_site ON captures(site, ts);
             CREATE TABLE IF NOT EXISTS search_history(q TEXT PRIMARY KEY, ts INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS ix_history_ts ON search_history(ts);
             UPDATE meta SET v='2' WHERE k='schema' AND v='1';",
        )?;
        w.exec_batch(
            "CREATE TABLE IF NOT EXISTS replay_frames(rid INTEGER PRIMARY KEY, replay_id TEXT NOT NULL, offset_ms INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS ix_replay_frames_replay ON replay_frames(replay_id);",
        )?;
        Self::migrate_chats(w)?;
        Self::migrate_fts(w)?;
        Ok(())
    }

    fn migrate_fts(w: &Connection) -> Result<(), StoreError> {
        if w.query_scalar_string("SELECT v FROM meta WHERE k='fts_secure_delete'", &[])?
            .is_some()
        {
            return Ok(());
        }
        w.exec_batch("INSERT INTO fts(fts, rank) VALUES('secure-delete', 1)")?;
        w.exec_batch("INSERT INTO fts(fts) VALUES('optimize')")?;
        w.exec_batch("INSERT OR REPLACE INTO meta(k, v) VALUES('fts_secure_delete', '1')")?;
        w.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }

    pub(crate) fn kf_conn<'a>(
        &self,
        st: &'a mut WriteState,
        month: &str,
        create: bool,
    ) -> Result<Option<&'a Connection>, StoreError> {
        if !st.kf.contains_key(month) {
            let path = self.kf_path(month);
            let exists = path.exists();
            if !create && !exists {
                return Ok(None);
            }
            let c = self.open_encrypted(&path, !exists)?;
            c.exec_batch(
                "CREATE TABLE IF NOT EXISTS kf(rid INTEGER PRIMARY KEY, mime TEXT NOT NULL, width INTEGER NOT NULL, height INTEGER NOT NULL, data BLOB NOT NULL)",
            )?;
            st.kf.insert(month.to_string(), c);
        }
        Ok(st.kf.get(month))
    }

    pub(crate) fn kf_path(&self, month: &str) -> PathBuf {
        self.kf_dir.join(format!("{month}.db"))
    }

    pub(crate) fn month_str(ts_ms: i64) -> String {
        let dt = Utc
            .timestamp_millis_opt(ts_ms)
            .single()
            .unwrap_or_else(Utc::now);
        dt.format("%Y-%m").to_string()
    }

    pub(crate) fn kf_months(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.kf_dir) else {
            return Vec::new();
        };
        let mut months = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Match `????-??.db`
            if name.len() == 10
                && name.as_bytes()[4] == b'-'
                && name[7..].eq_ignore_ascii_case(".db")
            {
                months.push(name[..7].to_string());
            }
        }
        months.sort();
        months
    }

    pub(crate) fn delete_kf_file(&self, st: &mut WriteState, month: &str) {
        if let Some(c) = st.kf.remove(month) {
            let _ = c.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            drop(c);
        }
        let path = self.kf_path(month);
        for p in db_trio(&path) {
            if p.exists()
                && let Err(e) = fs::remove_file(&p)
            {
                Log::warn_err("keyframe file delete failed", &e.to_string());
            }
        }
    }

    pub(crate) fn begin_write<'a>(w: &'a Connection) -> Result<Transaction<'a>, StoreError> {
        rollback_leftover(w);
        match w.begin_transaction() {
            Ok(tx) => Ok(tx),
            Err(e) => {
                rollback_leftover(w);
                Err(e.into())
            }
        }
    }

    /// Warms SQLite3MC loading and transaction paths on a throwaway in-memory database.
    pub fn warm_native_callbacks() -> Result<(), StoreError> {
        let c = Connection::open_in_memory()?;
        {
            let tx = c.begin_transaction()?;
            c.exec_batch("CREATE TEMP TABLE warm(x)")?;
            tx.rollback()?;
        }
        {
            let _tx = c.begin_transaction()?;
        }
        Ok(())
    }

    /// Test helper: leaves an uncommitted `BEGIN` on `_w` to verify `begin_write` recovers cleanly.
    pub fn begin_raw_for_test(&self) -> Result<(), StoreError> {
        let st = self.wl.lock();
        st.w()?.exec_batch("BEGIN")?;
        Ok(())
    }

    // ---------------------------------------------------------------- embedding space & index

    fn load_index_locked(&self, st: &WriteState) -> Result<(), StoreError> {
        self.index.clear();
        let Some(ref space) = st.space else {
            return Ok(());
        };
        let w = st.w()?;
        let sw = Instant::now();
        let mut stmt = w.prepare(
            "SELECT v.rid, v.dim, v.bin, c.ts, c.kind, c.app_lc, c.site
             FROM vectors v JOIN captures c ON c.rid = v.rid
             WHERE c.state = 1 AND v.model_id = $m AND v.revision = $r",
        )?;
        stmt.bind_all(&[
            ("$m", Value::Text(&space.model_id)),
            ("$r", Value::Text(&space.revision)),
        ])?;
        let mut n = 0usize;
        let mut words = Vec::new();
        while stmt.step()? {
            let rid = stmt.get_i64(0)?;
            let dim = stmt.get_i32(1)? as usize;
            let bin = stmt.get_blob(2)?;
            let ts = stmt.get_i64(3)?;
            let kind = stmt.get_i32(4)?;
            let app_lc = stmt.get_string(5)?;
            let site = stmt.get_string(6)?;
            words.clear();
            for chunk in bin.chunks_exact(8) {
                words.push(u64::from_le_bytes(chunk.try_into().unwrap()));
            }
            self.index
                .upsert_bits(rid, &words, dim, ts, kind, &app_lc, &site);
            n += 1;
        }
        Log::info(&format!(
            "vector index loaded: {n} rows in {} ms",
            sw.elapsed().as_millis()
        ));
        Ok(())
    }

    fn in_space(space: &Option<EmbeddingInfo>, model: &str, revision: &str) -> bool {
        space
            .as_ref()
            .is_some_and(|s| s.model_id == model && s.revision == revision)
    }

    pub fn ensure_embedding_space(&self, info: &EmbeddingInfo) -> Result<(), StoreError> {
        if info.dim <= 0 || info.dim > 8192 {
            return Err(StoreError::InvalidArgument("Bad dim.".to_string()));
        }
        let mut st = self.wl.lock();
        st.check_open()?;
        let old = st.space.clone();
        if let Some(ref old_space) = old
            && old_space.model_id == info.model_id
            && old_space.revision == info.revision
        {
            if info.dim < old_space.dim {
                Self::truncate_vectors(st.w()?, info.dim as usize)?;
            }
            if info.dim > old_space.dim {
                Self::queue_short_vectors(st.w()?, info)?;
            }
            if info.dim != old_space.dim {
                write_space(st.w()?, info)?;
                st.space = Some(info.clone());
                Log::info(&format!("embedding dim {} -> {}", old_space.dim, info.dim));
            }
            if info.dim < old_space.dim {
                let _rl = self.rl.lock().expect("rl");
                self.load_index_locked(&st)?;
            }
            return Ok(());
        }

        write_space(st.w()?, info)?;
        st.space = Some(info.clone());
        if old.is_none() {
            let other = st
                .w()?
                .query_scalar_i64(
                    "SELECT count(*) FROM vectors WHERE model_id<>$m OR revision<>$r",
                    &[
                        ("$m", Value::Text(&info.model_id)),
                        ("$r", Value::Text(&info.revision)),
                    ],
                )?
                .unwrap_or(0);
            if other == 0 {
                self.load_index_locked(&st)?;
                return Ok(());
            }
        }

        let now = Utc::now().timestamp_millis();
        {
            let w = st.w()?;
            let tx = Self::begin_write(w)?;
            w.execute(
                "INSERT OR IGNORE INTO jobs(rid, created)
                 SELECT c.rid, $now FROM captures c LEFT JOIN vectors v ON v.rid = c.rid
                 WHERE c.state = 1
                   AND (v.rid IS NULL OR v.model_id <> $m OR v.revision <> $r)
                   AND ((c.kind IN (0, 2) AND c.has_keyframe = 1) OR (c.kind = 1 AND c.text IS NOT NULL AND c.text <> ''))",
                &[
                    ("$now", Value::Int64(now)),
                    ("$m", Value::Text(&info.model_id)),
                    ("$r", Value::Text(&info.revision)),
                ],
            )?;
            tx.commit()?;
        }
        Log::info("embedding model changed, rows queued for re-embedding");
        let _rl = self.rl.lock().expect("rl");
        self.load_index_locked(&st)?;
        Ok(())
    }

    fn queue_short_vectors(w: &Connection, info: &EmbeddingInfo) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let tx = Self::begin_write(w)?;
        let n = w.execute(
            "INSERT OR IGNORE INTO jobs(rid, created)
             SELECT c.rid, $now FROM captures c JOIN vectors v ON v.rid = c.rid
             WHERE c.state = 1 AND v.model_id = $m AND v.revision = $r AND v.dim < $d
               AND ((c.kind IN (0, 2) AND c.has_keyframe = 1) OR (c.kind = 1 AND c.text IS NOT NULL AND c.text <> ''))",
            &[
                ("$now", Value::Int64(now)),
                ("$m", Value::Text(&info.model_id)),
                ("$r", Value::Text(&info.revision)),
                ("$d", Value::Int(info.dim)),
            ],
        )?;
        tx.commit()?;
        Log::info(&format!(
            "{n} rows with shorter vectors queued for re-embedding at {}-d",
            info.dim
        ));
        Ok(())
    }

    fn truncate_vectors(w: &Connection, dim: usize) -> Result<(), StoreError> {
        let mut rows = Vec::new();
        {
            let mut stmt = w.prepare("SELECT rid, vec FROM vectors WHERE dim > $d")?;
            stmt.bind_named("$d", Value::Int(dim as i32))?;
            while stmt.step()? {
                rows.push((stmt.get_i64(0)?, stmt.get_blob(1)?));
            }
        }
        let tx = Self::begin_write(w)?;
        for (rid, vec_bytes) in &rows {
            let floats = to_floats(vec_bytes);
            let v = normalize(&floats[..dim.min(floats.len())]);
            let vb = to_bytes(&v);
            let bb = BinaryVectorIndex::sign_bits_bytes(&v);
            w.execute(
                "UPDATE vectors SET dim=$d, vec=$v, bin=$b WHERE rid=$rid",
                &[
                    ("$d", Value::Int(dim as i32)),
                    ("$v", Value::Blob(&vb)),
                    ("$b", Value::Blob(&bb)),
                    ("$rid", Value::Int64(*rid)),
                ],
            )?;
        }
        tx.commit()?;
        Log::info(&format!("truncated {} vectors to {dim}-d", rows.len()));
        Ok(())
    }

    // ---------------------------------------------------------------- ingest

    pub fn add_screen_capture(&self, c: &NewScreenCapture) -> Result<String, StoreError> {
        let mut st = self.wl.lock();
        st.check_open()?;
        throw_if_fenced(&mut st, c.ts, 0, Some(&c.app), Some(&c.site))?;
        let id = Ids::new('s', c.ts);
        let ts = c.ts.timestamp_millis();
        let month = Self::month_str(ts);
        let site = Self::normalize_site(Some(&c.site));
        let text_opt = c
            .text
            .as_deref()
            .filter(|t| !t.trim().is_empty());
        let chg = c.change.map(|v| v.clamp(0.0, 100.0));
        let sw = if c.change.is_none() && !c.switch {
            None
        } else {
            Some(i32::from(c.switch))
        };
        let app_lc = c.app.to_lowercase();
        let now = Utc::now().timestamp_millis();

        let rid = {
            let w = st.w()?;
            let tx = Self::begin_write(w)?;
            let rid = w
                .query_scalar_i64(
                    "INSERT INTO captures(id, ts, kind, app, app_lc, exe, title, text, monitor, width, height, dhash, duration_ms, state, has_keyframe, keep_keyframe, kf_month, site, change_pct, is_switch)
                     VALUES($id, $ts, 0, $app, $applc, $exe, $title, $text, $mon, $w, $h, $dh, 0, 0, 1, $keep, $month, $site, $chg, $sw) RETURNING rid",
                    &[
                        ("$site", Value::Text(&site)),
                        ("$text", text_opt.into()),
                        ("$chg", chg.into()),
                        ("$sw", sw.into()),
                        ("$id", Value::Text(&id)),
                        ("$ts", Value::Int64(ts)),
                        ("$app", Value::Text(&c.app)),
                        ("$applc", Value::Text(&app_lc)),
                        ("$exe", Value::Text(&c.exe_path)),
                        ("$title", Value::Text(&c.title)),
                        ("$mon", Value::Int(c.monitor)),
                        ("$w", Value::Int(c.width)),
                        ("$h", Value::Int(c.height)),
                        ("$dh", Value::Int64(c.d_hash as i64)),
                        ("$keep", Value::Int(i32::from(c.keep_keyframe))),
                        ("$month", Value::Text(&month)),
                    ],
                )?
                .ok_or_else(|| StoreError::InvalidArgument("INSERT RETURNING rid failed".to_string()))?;
            w.execute(
                "INSERT INTO jobs(rid, created) VALUES($rid, $now)",
                &[("$rid", Value::Int64(rid)), ("$now", Value::Int64(now))],
            )?;
            tx.commit()?;
            rid
        };

        let mime = if c.image_mime.is_empty() {
            "image/webp"
        } else {
            c.image_mime.as_str()
        };
        let kf_res = self.kf_conn(&mut st, &month, true).and_then(|kc_opt| {
            let kc = kc_opt.ok_or_else(|| StoreError::InvalidArgument("keyframe db missing".to_string()))?;
            kc.execute(
                "INSERT OR REPLACE INTO kf(rid, mime, width, height, data) VALUES($rid, $mime, $w, $h, $data)",
                &[
                    ("$rid", Value::Int64(rid)),
                    ("$mime", Value::Text(mime)),
                    ("$w", Value::Int(c.width)),
                    ("$h", Value::Int(c.height)),
                    ("$data", Value::Blob(&c.image)),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = kf_res {
            if let Ok(w) = st.w() {
                let _ = w.execute("DELETE FROM jobs WHERE rid=$rid", &[("$rid", Value::Int64(rid))]);
                let _ = w.execute("DELETE FROM captures WHERE rid=$rid", &[("$rid", Value::Int64(rid))]);
            }
            return Err(e);
        }
        Ok(id)
    }

    pub fn add_replay_frame(&self, f: &NewReplayFrame) -> Result<String, StoreError> {
        if f.replay_id.trim().is_empty() || f.replay_id.len() > 64 {
            return Err(StoreError::InvalidArgument("Bad replay id.".to_string()));
        }
        let mut st = self.wl.lock();
        st.check_open()?;
        throw_if_fenced(&mut st, f.ts, 0, Some(&f.app), Some(&f.site))?;
        if let Some(grabbed) = f.captured_at
            && grabbed < f.ts
        {
            throw_if_fenced(&mut st, grabbed, 0, Some(&f.app), Some(&f.site))?;
        }
        let id = Ids::new('r', f.ts);
        let ts = f.ts.timestamp_millis();
        let month = Self::month_str(ts);
        let site = Self::normalize_site(Some(&f.site));
        let app_lc = f.app.to_lowercase();
        let now = Utc::now().timestamp_millis();

        let rid = {
            let w = st.w()?;
            let tx = Self::begin_write(w)?;
            let rid = w
                .query_scalar_i64(
                    "INSERT INTO captures(id, ts, kind, app, app_lc, exe, title, monitor, width, height, dhash, duration_ms, state, has_keyframe, keep_keyframe, kf_month, site)
                     VALUES($id, $ts, 2, $app, $applc, '', $title, $mon, $w, $h, 0, 0, 0, 1, 1, $month, $site) RETURNING rid",
                    &[
                        ("$site", Value::Text(&site)),
                        ("$id", Value::Text(&id)),
                        ("$ts", Value::Int64(ts)),
                        ("$app", Value::Text(&f.app)),
                        ("$applc", Value::Text(&app_lc)),
                        ("$title", Value::Text(&f.title)),
                        ("$mon", Value::Int(f.monitor)),
                        ("$w", Value::Int(f.width)),
                        ("$h", Value::Int(f.height)),
                        ("$month", Value::Text(&month)),
                    ],
                )?
                .ok_or_else(|| StoreError::InvalidArgument("INSERT RETURNING rid failed".to_string()))?;
            w.execute(
                "INSERT INTO replay_frames(rid, replay_id, offset_ms) VALUES($rid, $rep, $off)",
                &[
                    ("$rid", Value::Int64(rid)),
                    ("$rep", Value::Text(&f.replay_id)),
                    ("$off", Value::Int(f.offset_ms.max(0))),
                ],
            )?;
            w.execute(
                "INSERT INTO jobs(rid, created) VALUES($rid, $now)",
                &[("$rid", Value::Int64(rid)), ("$now", Value::Int64(now))],
            )?;
            tx.commit()?;
            rid
        };

        let mime = if f.image_mime.is_empty() {
            "image/webp"
        } else {
            f.image_mime.as_str()
        };
        let kf_res = self.kf_conn(&mut st, &month, true).and_then(|kc_opt| {
            let kc = kc_opt.ok_or_else(|| StoreError::InvalidArgument("keyframe db missing".to_string()))?;
            kc.execute(
                "INSERT OR REPLACE INTO kf(rid, mime, width, height, data) VALUES($rid, $mime, $w, $h, $data)",
                &[
                    ("$rid", Value::Int64(rid)),
                    ("$mime", Value::Text(mime)),
                    ("$w", Value::Int(f.width)),
                    ("$h", Value::Int(f.height)),
                    ("$data", Value::Blob(&f.image)),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = kf_res {
            let before = st.deleted.len();
            let _ = self.delete_rows_locked(&mut st, &[rid]);
            st.deleted.truncate(before);
            return Err(e);
        }
        Ok(id)
    }

    pub fn delete_replay_frames(&self, replay_id: &str) -> Result<usize, StoreError> {
        if replay_id.trim().is_empty() {
            return Ok(0);
        }
        let (n, notice) = {
            let mut st = self.wl.lock();
            st.check_open()?;
            let mut rids = Vec::new();
            {
                let mut stmt = st
                    .w()?
                    .prepare("SELECT rid FROM replay_frames WHERE replay_id=$r")?;
                stmt.bind_named("$r", Value::Text(replay_id))?;
                while stmt.step()? {
                    rids.push(stmt.get_i64(0)?);
                }
            }
            let n = self.delete_rows_locked(&mut st, &rids)?;
            if n > 0 {
                Log::info(&format!("replay deleted: {n} replay frames removed"));
            }
            let notice = take_notice_locked(&mut st, "replay", None, None, None);
            (n, notice)
        };
        self.raise(notice);
        Ok(n)
    }

    pub fn add_audio_capture(&self, c: &NewAudioCapture) -> Result<String, StoreError> {
        let mut st = self.wl.lock();
        st.check_open()?;
        throw_if_fenced(&mut st, c.ts, c.duration_ms.max(0), Some(&c.app), None)?;
        let id = Ids::new('a', c.ts);
        let ts = c.ts.timestamp_millis();
        let app_lc = c.app.to_lowercase();
        let now = Utc::now().timestamp_millis();

        let w = st.w()?;
        let tx = Self::begin_write(w)?;
        let rid = w
            .query_scalar_i64(
                "INSERT INTO captures(id, ts, kind, app, app_lc, exe, title, text, source, language, monitor, width, height, dhash, duration_ms, state, has_keyframe, keep_keyframe, kf_month)
                 VALUES($id, $ts, 1, $app, $applc, '', '', $text, $src, $lang, -1, 0, 0, 0, $dur, 1, 0, 0, NULL) RETURNING rid",
                &[
                    ("$id", Value::Text(&id)),
                    ("$ts", Value::Int64(ts)),
                    ("$app", Value::Text(&c.app)),
                    ("$applc", Value::Text(&app_lc)),
                    ("$text", Value::Text(&c.text)),
                    ("$src", Value::Text(&c.source)),
                    ("$lang", Value::Text(&c.language)),
                    ("$dur", Value::Int(c.duration_ms)),
                ],
            )?
            .ok_or_else(|| StoreError::InvalidArgument("INSERT RETURNING rid failed".to_string()))?;
        w.execute(
            "INSERT INTO fts(rowid, title, app, text) VALUES($rid, '', $app, $text)",
            &[
                ("$rid", Value::Int64(rid)),
                ("$app", Value::Text(&c.app)),
                ("$text", Value::Text(&c.text)),
            ],
        )?;
        if !c.text.trim().is_empty() {
            w.execute(
                "INSERT INTO jobs(rid, created) VALUES($rid, $now)",
                &[("$rid", Value::Int64(rid)), ("$now", Value::Int64(now))],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    // ---------------------------------------------------------------- embedding jobs

    fn is_leased(&self, st: &WriteState, rid: i64, now: i64) -> bool {
        let timeout = self.embedding_lease_timeout_ms.load(Ordering::Relaxed);
        st.inflight.get(&rid).is_some_and(|&at| now - at < timeout)
    }

    pub fn take_pending_embeddings(&self, max: usize) -> Result<Vec<PendingJob>, StoreError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut st = self.wl.lock();
        st.check_open()?;
        let lease_now = self.boot_instant.elapsed().as_millis() as i64;
        let timeout = self.embedding_lease_timeout_ms.load(Ordering::Relaxed);
        st.inflight.retain(|_, &mut at| lease_now - at < timeout);

        let limit = (max + st.inflight.len()) as i64;
        let mut rows = Vec::new();
        {
            let mut stmt = st.w()?.prepare(
                "SELECT j.rid, c.id, c.kind, c.text, c.kf_month, c.has_keyframe
                 FROM jobs j JOIN captures c ON c.rid = j.rid
                 ORDER BY c.ts LIMIT $n",
            )?;
            stmt.bind_named("$n", Value::Int64(limit))?;
            while stmt.step()? {
                rows.push((
                    stmt.get_i64(0)?,
                    stmt.get_string(1)?,
                    stmt.get_i32(2)?,
                    stmt.get_opt_string(3)?,
                    stmt.get_opt_string(4)?,
                    stmt.get_i32(5)? == 1,
                ));
            }
        }

        let mut jobs = Vec::new();
        for (rid, id, kind, text, month, has_kf) in rows {
            if jobs.len() >= max {
                break;
            }
            if self.is_leased(&st, rid, lease_now) {
                continue;
            }
            if kind == KIND_AUDIO {
                jobs.push(PendingJob {
                    id,
                    kind: CaptureKind::Audio,
                    image: None,
                    image_mime: None,
                    text: Some(text.unwrap_or_default()),
                });
                st.inflight.insert(rid, lease_now);
                continue;
            }
            let mut img: Option<(Vec<u8>, String)> = None;
            if has_kf
                && let Some(ref m) = month
                && let Some(kc) = self.kf_conn(&mut st, m, false)?
            {
                let mut stmt = kc.prepare("SELECT data, mime FROM kf WHERE rid=$rid")?;
                stmt.bind_named("$rid", Value::Int64(rid))?;
                if stmt.step()? {
                    img = Some((stmt.get_blob(0)?, stmt.get_string(1)?));
                }
            }
            let Some((data, mime)) = img else {
                self.fail_locked(&mut st, rid, "keyframe missing")?;
                continue;
            };
            jobs.push(PendingJob {
                id,
                kind: kind_of(kind),
                image: Some(data),
                image_mime: Some(mime),
                text: None,
            });
            st.inflight.insert(rid, lease_now);
        }
        Ok(jobs)
    }

    pub fn set_embedding(
        &self,
        id: &str,
        vector: &[f32],
        info: &EmbeddingInfo,
    ) -> Result<(), StoreError> {
        if vector.len() != info.dim as usize || vector.is_empty() {
            return Err(StoreError::InvalidArgument(
                "Vector length must equal info.Dim.".to_string(),
            ));
        }
        let v = normalize(vector);
        let mut st = self.wl.lock();
        st.check_open()?;
        let row = {
            let mut stmt = st.w()?.prepare(
                "SELECT rid, state, kind, title, app, text, keep_keyframe, kf_month, ts, app_lc, site
                 FROM captures WHERE id=$id",
            )?;
            stmt.bind_named("$id", Value::Text(id))?;
            if !stmt.step()? {
                Log::debug("SetEmbedding for a row that no longer exists");
                return Ok(());
            }
            (
                stmt.get_i64(0)?,
                stmt.get_i32(1)?,
                stmt.get_i32(2)?,
                stmt.get_string(3)?,
                stmt.get_string(4)?,
                stmt.get_opt_string(5)?,
                stmt.get_i32(6)? == 1,
                stmt.get_opt_string(7)?,
                stmt.get_i64(8)?,
                stmt.get_string(9)?,
                stmt.get_string(10)?,
            )
        };
        let (rid, state, kind, title, app, text, keep, month, ts, app_lc, site) = row;
        st.inflight.remove(&rid);
        if state == STATE_FAILED {
            st.w()?
                .execute("DELETE FROM jobs WHERE rid=$rid", &[("$rid", Value::Int64(rid))])?;
            return Ok(());
        }
        if st.space.is_none() {
            write_space(st.w()?, info)?;
            st.space = Some(info.clone());
        }
        let vb = to_bytes(&v);
        let bb = BinaryVectorIndex::sign_bits_bytes(&v);
        {
            let w = st.w()?;
            let tx = Self::begin_write(w)?;
            w.execute(
                "INSERT INTO vectors(rid, model_id, revision, dim, vec, bin) VALUES($rid, $m, $r, $d, $v, $b)
                 ON CONFLICT(rid) DO UPDATE SET model_id=excluded.model_id, revision=excluded.revision, dim=excluded.dim, vec=excluded.vec, bin=excluded.bin",
                &[
                    ("$rid", Value::Int64(rid)),
                    ("$m", Value::Text(&info.model_id)),
                    ("$r", Value::Text(&info.revision)),
                    ("$d", Value::Int(info.dim)),
                    ("$v", Value::Blob(&vb)),
                    ("$b", Value::Blob(&bb)),
                ],
            )?;
            w.execute("DELETE FROM jobs WHERE rid=$rid", &[("$rid", Value::Int64(rid))])?;
            if state == STATE_PENDING {
                w.execute(
                    "UPDATE captures SET state=1 WHERE rid=$rid",
                    &[("$rid", Value::Int64(rid))],
                )?;
                w.execute(
                    "INSERT INTO fts(rowid, title, app, text) VALUES($rid, $t, $a, $x)",
                    &[
                        ("$rid", Value::Int64(rid)),
                        ("$t", Value::Text(&title)),
                        ("$a", Value::Text(&app)),
                        ("$x", text.as_deref().into()),
                    ],
                )?;
            }
            if kind == KIND_SCREEN && !keep {
                w.execute(
                    "UPDATE captures SET has_keyframe=0 WHERE rid=$rid",
                    &[("$rid", Value::Int64(rid))],
                )?;
            }
            tx.commit()?;
        }
        if kind == KIND_SCREEN
            && !keep
            && let Some(ref m) = month
            && let Some(kc) = self.kf_conn(&mut st, m, false)?
        {
            kc.execute("DELETE FROM kf WHERE rid=$rid", &[("$rid", Value::Int64(rid))])?;
        }
        if Self::in_space(&st.space, &info.model_id, &info.revision) {
            self.index.upsert(rid, &v, ts, kind, &app_lc, &site);
        } else {
            self.index.remove(rid);
        }
        Ok(())
    }

    pub fn fail_embedding(&self, id: &str, reason: &str) -> Result<(), StoreError> {
        let mut st = self.wl.lock();
        st.check_open()?;
        let Some(rid) = rid_of(st.w()?, id)? else {
            return Ok(());
        };
        self.fail_locked(&mut st, rid, reason)
    }

    fn fail_locked(&self, st: &mut WriteState, rid: i64, reason: &str) -> Result<(), StoreError> {
        st.inflight.remove(&rid);
        let row = {
            let mut stmt = st
                .w()?
                .prepare("SELECT state, kind, keep_keyframe, kf_month FROM captures WHERE rid=$rid")?;
            stmt.bind_named("$rid", Value::Int64(rid))?;
            if !stmt.step()? {
                return Ok(());
            }
            (
                stmt.get_i32(0)?,
                stmt.get_i32(1)?,
                stmt.get_i32(2)? == 1,
                stmt.get_opt_string(3)?,
            )
        };
        let (state, kind, keep, month) = row;
        st.w()?
            .execute("DELETE FROM jobs WHERE rid=$rid", &[("$rid", Value::Int64(rid))])?;
        if state == STATE_PENDING {
            st.w()?.execute(
                "UPDATE captures SET state=2 WHERE rid=$rid",
                &[("$rid", Value::Int64(rid))],
            )?;
            if kind == KIND_SCREEN
                && !keep
                && let Some(ref m) = month
                && let Some(kc) = self.kf_conn(st, m, false)?
            {
                kc.execute("DELETE FROM kf WHERE rid=$rid", &[("$rid", Value::Int64(rid))])?;
                st.w()?.execute(
                    "UPDATE captures SET has_keyframe=0 WHERE rid=$rid",
                    &[("$rid", Value::Int64(rid))],
                )?;
            }
        }
        Log::info(&format!(
            "embedding failed for a row ({})",
            safe_reason(reason)
        ));
        Ok(())
    }

    pub fn drop_capture(&self, id: &str, reason: &str) -> Result<(), StoreError> {
        let notice = {
            let mut st = self.wl.lock();
            st.check_open()?;
            let Some(rid) = rid_of(st.w()?, id)? else {
                return Ok(());
            };
            self.delete_rows_locked(&mut st, &[rid])?;
            Log::info(&format!("capture dropped ({})", safe_reason(reason)));
            take_notice_locked(&mut st, "drop", None, None, None)
        };
        self.raise(notice);
        Ok(())
    }

    // ---------------------------------------------------------------- deletes & purge

    pub(crate) fn delete_rows_locked(
        &self,
        st: &mut WriteState,
        rids: &[i64],
    ) -> Result<usize, StoreError> {
        let mut total = 0usize;
        for chunk in rids.chunks(500) {
            let list = in_list(chunk);
            Self::delete_chats_citing_locked(st.w()?, chunk)?;
            let mut chunk_deleted = Vec::new();
            {
                let mut stmt = st
                    .w()?
                    .prepare(&format!("SELECT id, ts FROM captures WHERE rid IN ({list})"))?;
                while stmt.step()? {
                    chunk_deleted.push((stmt.get_string(0)?, stmt.get_i64(1)?));
                }
            }
            st.deleted.extend(chunk_deleted);
            let mut months: HashMap<String, Vec<i64>> = HashMap::new();
            {
                let mut stmt = st.w()?.prepare(&format!(
                    "SELECT rid, kf_month FROM captures WHERE rid IN ({list}) AND +kf_month IS NOT NULL"
                ))?;
                while stmt.step()? {
                    let rid = stmt.get_i64(0)?;
                    let m = stmt.get_string(1)?;
                    months.entry(m).or_default().push(rid);
                }
            }
            {
                let w = st.w()?;
                let tx = Self::begin_write(w)?;
                w.exec_batch(&format!("DELETE FROM fts WHERE rowid IN ({list})"))?;
                w.exec_batch(&format!("DELETE FROM vectors WHERE rid IN ({list})"))?;
                w.exec_batch(&format!("DELETE FROM jobs WHERE rid IN ({list})"))?;
                w.exec_batch(&format!("DELETE FROM replay_frames WHERE rid IN ({list})"))?;
                total += w.execute(&format!("DELETE FROM captures WHERE rid IN ({list})"), &[])?;
                tx.commit()?;
            }
            for (m, l) in months {
                if let Some(kc) = self.kf_conn(st, &m, false)? {
                    kc.exec_batch(&format!("DELETE FROM kf WHERE rid IN ({})", in_list(&l)))?;
                }
            }
            for &rid in chunk {
                self.index.remove(rid);
                st.inflight.remove(&rid);
            }
        }
        Ok(total)
    }

    fn select_scope(w: &Connection, scope: &PurgeScope) -> Result<Vec<i64>, StoreError> {
        let kind_lc = scope.kind.to_lowercase();
        let mut rids = Vec::new();
        match kind_lc.as_str() {
            "last" | "range" => {
                let (Some(from), Some(to)) = (scope.from, scope.to) else {
                    return Err(StoreError::InvalidArgument(
                        "Range needs from and to.".to_string(),
                    ));
                };
                let upper = upper_bound_ms(scope, from, to);
                let mut stmt = w.prepare("SELECT rid FROM captures WHERE ts >= $from AND ts <= $to")?;
                stmt.bind_all(&[
                    ("$from", Value::Int64(from.timestamp_millis())),
                    ("$to", Value::Int64(upper)),
                ])?;
                while stmt.step()? {
                    rids.push(stmt.get_i64(0)?);
                }
            }
            "app" => {
                let Some(ref app) = scope.app else {
                    return Err(StoreError::InvalidArgument("App needed.".to_string()));
                };
                let app_lc = app.trim().to_lowercase();
                if app_lc.is_empty() {
                    return Err(StoreError::InvalidArgument("App needed.".to_string()));
                }
                let app2 = if let Some(stripped) = app_lc.strip_suffix(".exe") {
                    stripped.to_string()
                } else {
                    format!("{app_lc}.exe")
                };
                let mut stmt =
                    w.prepare("SELECT rid FROM captures WHERE (app_lc = $app OR app_lc = $app2)")?;
                stmt.bind_all(&[("$app", Value::Text(&app_lc)), ("$app2", Value::Text(&app2))])?;
                while stmt.step()? {
                    rids.push(stmt.get_i64(0)?);
                }
            }
            "site" => {
                let Some(ref site) = scope.site else {
                    return Err(StoreError::InvalidArgument("Site needed.".to_string()));
                };
                if site.trim().is_empty() {
                    return Err(StoreError::InvalidArgument("Site needed.".to_string()));
                }
                let norm = Self::normalize_site(Some(site));
                let mut stmt = w.prepare("SELECT rid FROM captures WHERE site = $site")?;
                stmt.bind_named("$site", Value::Text(&norm))?;
                while stmt.step()? {
                    rids.push(stmt.get_i64(0)?);
                }
            }
            "id" => {
                let Some(ref id) = scope.id else {
                    return Err(StoreError::InvalidArgument("Id needed.".to_string()));
                };
                let trimmed = id.trim();
                if trimmed.is_empty() {
                    return Err(StoreError::InvalidArgument("Id needed.".to_string()));
                }
                let mut stmt = w.prepare("SELECT rid FROM captures WHERE id = $id")?;
                stmt.bind_named("$id", Value::Text(trimmed))?;
                while stmt.step()? {
                    rids.push(stmt.get_i64(0)?);
                }
            }
            "all" => {
                let mut stmt = w.prepare("SELECT rid FROM captures WHERE 1=1")?;
                while stmt.step()? {
                    rids.push(stmt.get_i64(0)?);
                }
            }
            _ => {
                return Err(StoreError::InvalidArgument("Unknown purge scope.".to_string()));
            }
        }
        Ok(rids)
    }

    pub fn count_purge(&self, scope: &PurgeScope) -> Result<usize, StoreError> {
        let st = self.wl.lock();
        st.check_open()?;
        Ok(Self::select_scope(st.w()?, scope)?.len())
    }

    /// Deletes the scope, then `wal_checkpoint(TRUNCATE)` on every open file.
    pub fn purge(&self, scope: &PurgeScope) -> Result<usize, StoreError> {
        let (n, notice) = {
            let mut st = self.enter_write_priority();
            st.check_open()?;
            let rids = Self::select_scope(st.w()?, scope)?;
            let n = self.delete_rows_locked(&mut st, &rids)?;
            fence_locked(&mut st, scope);
            if matches!(scope.kind.as_str(), "last" | "range")
                && let (Some(from), Some(to)) = (scope.from, scope.to)
            {
                let upper = upper_bound_ms(scope, from, to);
                let from_ms = from.timestamp_millis();
                st.w()?.execute(
                    "DELETE FROM search_history WHERE ts >= $from AND ts <= $to",
                    &[("$from", Value::Int64(from_ms)), ("$to", Value::Int64(upper))],
                )?;
                st.w()?.execute(
                    "UPDATE audit SET sha = '' WHERE verb = 'search' AND ts >= $from AND ts <= $to",
                    &[("$from", Value::Int64(from_ms)), ("$to", Value::Int64(upper))],
                )?;
                Self::delete_chats_in_window_locked(st.w()?, from_ms, upper)?;
            }
            if matches!(scope.kind.as_str(), "app" | "site") {
                delete_history_containing_locked(st.w()?, &Self::history_needles(scope))?;
            }
            checkpoint_locked(&st);
            Log::info(&format!("purge ({}) removed {n} rows", scope.kind));
            let window = matches!(scope.kind.as_str(), "last" | "range");
            let notice_scope = if matches!(scope.kind.as_str(), "app" | "site") {
                Some(scope.kind.clone())
            } else {
                None
            };
            let notice = take_notice_locked(
                &mut st,
                "purge",
                if window { scope.from } else { None },
                if window { scope.to } else { None },
                notice_scope,
            );
            (n, notice)
        };
        self.raise(notice);
        Ok(n)
    }

    pub fn clear_audit_query_hashes(&self, hashes: &[String]) -> Result<usize, StoreError> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let st = self.wl.lock();
        st.check_open()?;
        let w = st.w()?;
        let mut n = 0usize;
        for h in hashes {
            n += w.execute(
                "UPDATE audit SET sha = '' WHERE verb = 'search' AND sha = $sha",
                &[("$sha", Value::Text(h))],
            )?;
        }
        Ok(n)
    }

    /// Purge all (A-H1): the store opened after it refuses anything stamped up to `instant`.
    pub fn fence_up_to(&self, instant: DateTime<Utc>) -> Result<(), StoreError> {
        let mut st = self.wl.lock();
        st.check_open()?;
        let mono = (st.mono_ms)();
        st.fences.push((i64::MIN, instant.timestamp_millis(), mono));
        Ok(())
    }

    pub(crate) fn raise(&self, notice: Option<PurgeNotice>) {
        let Some(notice) = notice else {
            return;
        };
        let callbacks: Vec<PurgeListener> = self
            .listeners
            .lock()
            .expect("listeners")
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for cb in callbacks {
            if catch_unwind(AssertUnwindSafe(|| cb(&notice))).is_err() {
                Log::warn("purge listener failed");
            }
        }
    }

    /// `wal_checkpoint(TRUNCATE)` on `store.db` and every open keyframe file.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        let st = self.enter_write_priority();
        st.check_open()?;
        checkpoint_locked(&st);
        Ok(())
    }

    /// Priority `wal_checkpoint(TRUNCATE)` with a lock-acquisition timeout (`WM_ENDSESSION`, C4).
    /// Returns `Ok(false)` if the write lock was still held when `timeout` expired.
    pub fn checkpoint_timeout(&self, timeout: Duration) -> Result<bool, StoreError> {
        self.priority_waiters.fetch_add(1, Ordering::AcqRel);
        let guard = self.wl.try_lock_for(timeout);
        self.priority_waiters.fetch_sub(1, Ordering::AcqRel);
        let Some(st) = guard else {
            return Ok(false);
        };
        st.check_open()?;
        checkpoint_locked(&st);
        Ok(true)
    }

    // ---------------------------------------------------------------- reads

    pub fn timeline(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: i32,
    ) -> Result<Vec<CaptureMeta>, StoreError> {
        let limit = limit.clamp(1, 100_000);
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let mut stmt = r.prepare(
            "SELECT c.id, c.ts, c.kind, c.app, c.title, c.monitor, c.duration_ms, c.has_keyframe, v.rid, c.site
             FROM captures c LEFT JOIN vectors v ON v.rid = c.rid
             WHERE c.state = 1 AND c.ts >= $from AND c.ts < $to AND c.kind <> 2 ORDER BY c.ts DESC LIMIT $n",
        )?;
        stmt.bind_all(&[
            ("$from", Value::Int64(from.timestamp_millis())),
            ("$to", Value::Int64(to.timestamp_millis())),
            ("$n", Value::Int(limit)),
        ])?;
        let mut list = Vec::new();
        while stmt.step()? {
            list.push(read_meta(&stmt)?);
        }
        Ok(list)
    }

    pub fn get(&self, id: &str) -> Result<Option<CaptureDetail>, StoreError> {
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let mut stmt = r.prepare(
            "SELECT c.id, c.ts, c.kind, c.app, c.title, c.text, c.has_keyframe, c.duration_ms, c.site, r.replay_id, r.offset_ms
             FROM captures c LEFT JOIN replay_frames r ON r.rid = c.rid WHERE c.id=$id AND c.state=1",
        )?;
        stmt.bind_named("$id", Value::Text(id))?;
        if !stmt.step()? {
            return Ok(None);
        }
        let kind_i = stmt.get_i32(2)?;
        let audio = kind_i == KIND_AUDIO;
        let title = stmt.get_string(4)?;
        let raw_text = stmt.get_opt_string(5)?;
        let text = if audio {
            raw_text.unwrap_or_default()
        } else {
            match raw_text {
                Some(t) if !t.is_empty() => t,
                _ => title.clone(),
            }
        };
        let dur_ms = stmt.get_i32(7)?;
        Ok(Some(CaptureDetail {
            id: stmt.get_string(0)?,
            ts: ms_to_utc(stmt.get_i64(1)?),
            kind: kind_of(kind_i),
            app: stmt.get_string(3)?,
            title,
            text,
            has_thumb: stmt.get_i32(6)? == 1,
            duration_s: if audio { Some((dur_ms + 500) / 1000) } else { None },
            site: stmt.get_string(8)?,
            replay_id: stmt.get_opt_string(9)?,
            replay_offset_ms: stmt.get_opt_i32(10)?,
        }))
    }

    pub fn get_keyframe(&self, id: &str) -> Result<Option<ThumbData>, StoreError> {
        let (rid, month) = {
            let rl = self.rl.lock().expect("rl");
            let r = rl.as_ref().ok_or(StoreError::Disposed)?;
            let mut stmt = r.prepare(
                "SELECT rid, kf_month FROM captures WHERE id=$id AND state=1 AND has_keyframe=1 AND kf_month IS NOT NULL",
            )?;
            stmt.bind_named("$id", Value::Text(id))?;
            if !stmt.step()? {
                return Ok(None);
            }
            (stmt.get_i64(0)?, stmt.get_string(1)?)
        };
        let mut st = self.enter_write_priority();
        st.check_open()?;
        let Some(kc) = self.kf_conn(&mut st, &month, false)? else {
            return Ok(None);
        };
        let mut stmt = kc.prepare("SELECT mime, width, height, data FROM kf WHERE rid=$rid")?;
        stmt.bind_named("$rid", Value::Int64(rid))?;
        if !stmt.step()? {
            return Ok(None);
        }
        Ok(Some(ThumbData {
            id: id.to_string(),
            mime: stmt.get_string(0)?,
            width: stmt.get_i32(1)?,
            height: stmt.get_i32(2)?,
            bytes: stmt.get_blob(3)?,
        }))
    }

    // ---------------------------------------------------------------- search

    pub fn tokens(q: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        for ch in q.chars() {
            if is_token_char(ch) {
                cur.push(ch);
            } else if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                if out.len() >= 32 {
                    return out;
                }
            }
        }
        if !cur.is_empty() && out.len() < 32 {
            out.push(cur);
        }
        out
    }

    pub fn fts_query(q: &str) -> Option<String> {
        let tokens = Self::tokens(q);
        if tokens.is_empty() {
            return None;
        }
        let mut sb = String::new();
        for (i, tok) in tokens.iter().enumerate() {
            if i > 0 {
                sb.push_str(" OR ");
            }
            sb.push('"');
            sb.push_str(&tok.replace('"', "\"\""));
            sb.push('"');
            if i == tokens.len() - 1 && tok.chars().count() >= 2 {
                sb.push('*');
            }
        }
        Some(sb)
    }

    pub fn search(
        &self,
        request: &SearchRequest,
        query_vector: &[f32],
        max_results: i32,
        max_response_bytes: usize,
    ) -> Result<SearchResponse, StoreError> {
        let sw = Instant::now();
        let max_results = max_results.max(1);
        let limit = request.limit.unwrap_or(max_results).clamp(1, max_results) as usize;
        let from = request.from.map_or(i64::MIN, |d| d.timestamp_millis());
        let to = request.to.map_or(i64::MAX, |d| d.timestamp_millis());
        let kind_mask = match request.kind.as_deref() {
            None | Some([]) => ALL_KINDS,
            Some(ks) => ks.iter().fold(0, |acc, &k| acc | kind_bit(k)),
        };
        let app_lc = request
            .app
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let site = request
            .site
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| Self::normalize_site(Some(s)));
        let use_text = request.mode != SearchMode::Vector;
        let use_vec = request.mode != SearchMode::Text && !query_vector.is_empty();

        let (text_res, vec_res, t_vec_ms, t_text_ms) = thread::scope(|scope| {
            let text_handle = if use_text {
                let q = request.q.as_str();
                let app_ref = app_lc.as_deref();
                let site_ref = site.as_deref();
                Some(scope.spawn(move || {
                    self.fts_search(q, from, to, kind_mask, app_ref, site_ref)
                }))
            } else {
                None
            };
            let vec_res = if use_vec {
                self.vector_search(
                    query_vector,
                    from,
                    to,
                    kind_mask,
                    app_lc.as_deref(),
                    site.as_deref(),
                )
            } else {
                Ok(Vec::new())
            };
            let t_vec_ms = sw.elapsed().as_secs_f64() * 1000.0;
            let text_res = match text_handle {
                Some(h) => h.join().unwrap_or_else(|_| Ok(Vec::new())),
                None => Ok(Vec::new()),
            };
            let t_text_ms = sw.elapsed().as_secs_f64() * 1000.0;
            (text_res, vec_res, t_vec_ms, t_text_ms)
        });
        let text_ranked = text_res?;
        let vec_scored = vec_res?;

        let mut cos_by_id = HashMap::with_capacity(vec_scored.len());
        for &(vr, vc) in &vec_scored {
            cos_by_id.insert(vr, vc);
        }
        let text_set: HashSet<i64> = text_ranked.iter().copied().collect();

        let lists = usize::from(use_text) + usize::from(use_vec);
        let mut fused: HashMap<i64, f64> = HashMap::new();
        for (i, &rid) in text_ranked.iter().enumerate() {
            *fused.entry(rid).or_insert(0.0) += 1.0 / ((RRF_K + i + 1) as f64);
        }
        for (i, &(rid, _)) in vec_scored.iter().enumerate() {
            *fused.entry(rid).or_insert(0.0) += 1.0 / ((RRF_K + i + 1) as f64);
        }
        let mut norm = if lists == 0 {
            1.0
        } else {
            (lists as f64) * (1.0 / ((RRF_K + 1) as f64))
        };
        if use_vec && (lists == 1 || text_ranked.is_empty()) {
            fused.clear();
            for &(rid, cos) in &vec_scored {
                fused.insert(rid, f64::from(cos.max(0.0)));
            }
            norm = 1.0;
        }
        let mut top: Vec<(i64, f64)> = fused.into_iter().collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
        top.truncate(limit);

        let mut hits = Vec::with_capacity(top.len());
        if !top.is_empty() {
            let rids: Vec<i64> = top.iter().map(|&(rid, _)| rid).collect();
            let mut meta = HashMap::with_capacity(rids.len());
            {
                let rl = self.rl.lock().expect("rl");
                let r = rl.as_ref().ok_or(StoreError::Disposed)?;
                let mut stmt = r.prepare(&format!(
                    "SELECT c.rid, c.id, c.ts, c.kind, c.app, c.title, c.text, c.has_keyframe, c.duration_ms, c.site, r.replay_id, r.offset_ms
                     FROM captures c LEFT JOIN replay_frames r ON r.rid = c.rid WHERE c.rid IN ({}) AND +c.state=1",
                    in_list(&rids)
                ))?;
                while stmt.step()? {
                    meta.insert(
                        stmt.get_i64(0)?,
                        (
                            stmt.get_string(1)?,
                            stmt.get_i64(2)?,
                            stmt.get_i32(3)?,
                            stmt.get_string(4)?,
                            stmt.get_string(5)?,
                            stmt.get_opt_string(6)?,
                            stmt.get_i32(7)? == 1,
                            stmt.get_i32(8)?,
                            stmt.get_string(9)?,
                            stmt.get_opt_string(10)?,
                            stmt.get_opt_i32(11)?,
                        ),
                    );
                }
            }
            let tokens = Self::tokens(&request.q);
            for (rid, score) in top {
                let Some((id, ts, kind_i, app, title, text, kf, dur, site, replay, offset)) =
                    meta.remove(&rid)
                else {
                    continue;
                };
                let audio = kind_i == KIND_AUDIO;
                let snippet = if audio {
                    Self::snippet(text.as_deref().unwrap_or(""), &tokens)
                } else {
                    Self::screen_snippet(&title, text.as_deref(), &tokens)
                };
                let norm_score = ((score / norm).min(1.0) * 10_000.0).round() / 10_000.0;
                let cosine = cos_by_id
                    .get(&rid)
                    .map(|&c| ((f64::from(c) * 10_000.0).round() / 10_000.0) as f32);
                hits.push(SearchHit {
                    id,
                    ts: ms_to_utc(ts),
                    kind: kind_of(kind_i),
                    app,
                    title,
                    score: norm_score as f32,
                    snippet,
                    has_thumb: kf && !audio,
                    duration_s: if audio { Some((dur + 500) / 1000) } else { None },
                    cosine,
                    text_match: text_set.contains(&rid),
                    weak: false,
                    site,
                    replay_id: replay,
                    replay_offset_ms: offset,
                });
            }
        }

        let t_meta_ms = sw.elapsed().as_secs_f64() * 1000.0;
        let mut truncated = false;
        if max_response_bytes > 0 {
            let empty = SearchResponse {
                count: 0,
                truncated: true,
                took_ms: sw.elapsed().as_millis() as i64,
                results: Vec::new(),
                degraded: None,
            };
            let empty_len = CoreJson::to_string(&empty)?.len();
            let budget = (max_response_bytes as i64) - (empty_len as i64) - 64;
            let mut used = 0i64;
            let mut keep = 0usize;
            for h in &hits {
                let size = CoreJson::to_string(h)?.len() as i64 + 1;
                if used + size > budget {
                    break;
                }
                used += size;
                keep += 1;
            }
            if keep < hits.len() {
                hits.truncate(keep);
                truncated = true;
            }
        }
        let total_ms = sw.elapsed().as_secs_f64() * 1000.0;
        *self.last_timings.lock().expect("timings") = Some(format!(
            "vec {t_vec_ms:.1} text {t_text_ms:.1} meta {t_meta_ms:.1} total {total_ms:.1}"
        ));
        Ok(SearchResponse {
            count: hits.len() as i32,
            truncated,
            took_ms: sw.elapsed().as_millis() as i64,
            results: hits,
            degraded: None,
        })
    }

    fn fts_search(
        &self,
        q: &str,
        from: i64,
        to: i64,
        kind_mask: i32,
        app_lc: Option<&str>,
        site: Option<&str>,
    ) -> Result<Vec<i64>, StoreError> {
        let Some(match_q) = Self::fts_query(q) else {
            return Ok(Vec::new());
        };
        let filtered = from != i64::MIN
            || to != i64::MAX
            || kind_mask != ALL_KINDS
            || app_lc.is_some()
            || site.is_some();
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let mut ranked = Vec::new();
        {
            let mut stmt = r.prepare(
                "SELECT rowid FROM fts WHERE fts MATCH $q ORDER BY bm25(fts, 4.0, 2.0, 1.0) LIMIT $n",
            )?;
            stmt.bind_all(&[
                ("$q", Value::Text(&match_q)),
                (
                    "$n",
                    Value::Int((if filtered { FTS_FILTERED_CANDIDATES } else { FTS_CANDIDATES }) as i32),
                ),
            ])?;
            while stmt.step()? {
                ranked.push(stmt.get_i64(0)?);
            }
        }
        if !filtered || ranked.is_empty() {
            return Ok(ranked);
        }

        let mut sql = format!("SELECT rid FROM captures WHERE rid IN ({})", in_list(&ranked));
        let mut params: Vec<(&str, Value<'_>)> = Vec::new();
        if from != i64::MIN {
            sql.push_str(" AND +ts >= $from");
            params.push(("$from", Value::Int64(from)));
        }
        if to != i64::MAX {
            sql.push_str(" AND +ts < $to");
            params.push(("$to", Value::Int64(to)));
        }
        if kind_mask != ALL_KINDS {
            let mut kinds = Vec::with_capacity(3);
            if (kind_mask & 1) != 0 {
                kinds.push("0");
            }
            if (kind_mask & 2) != 0 {
                kinds.push("1");
            }
            if (kind_mask & 4) != 0 {
                kinds.push("2");
            }
            sql.push_str(" AND +kind IN (");
            if kinds.is_empty() {
                sql.push_str("-1");
            } else {
                sql.push_str(&kinds.join(","));
            }
            sql.push(')');
        }
        if let Some(a) = app_lc {
            sql.push_str(" AND +app_lc = $app");
            params.push(("$app", Value::Text(a)));
        }
        if let Some(s) = site {
            sql.push_str(" AND +site = $site");
            params.push(("$site", Value::Text(s)));
        }
        let mut keep = HashSet::new();
        {
            let mut stmt = r.prepare(&sql)?;
            stmt.bind_all(&params)?;
            while stmt.step()? {
                keep.insert(stmt.get_i64(0)?);
            }
        }
        Ok(ranked
            .into_iter()
            .filter(|rid| keep.contains(rid))
            .take(FTS_CANDIDATES)
            .collect())
    }

    fn vector_search(
        &self,
        q: &[f32],
        from: i64,
        to: i64,
        kind_mask: i32,
        app_lc: Option<&str>,
        site: Option<&str>,
    ) -> Result<Vec<(i64, f32)>, StoreError> {
        let app_id = app_lc.map_or(-1, |a| self.index.app_id(a));
        let site_id = site.map_or(-1, |s| self.index.site_id(s));
        let vsw = Instant::now();
        let cands = self.index.scan(
            q,
            VectorFilter::new(from, to, kind_mask, app_id, site_id),
            VECTOR_SCAN_TOP,
        );
        let scan_ms = vsw.elapsed().as_secs_f64() * 1000.0;
        if cands.is_empty() {
            *self.last_vector_timings.lock().expect("vec timings") =
                Some(format!("scan {scan_ms:.1}"));
            return Ok(Vec::new());
        }
        let mut by_rid: HashMap<i64, usize> = HashMap::with_capacity(cands.len());
        let mut sorted_rids: Vec<i64> = Vec::with_capacity(cands.len());
        for c in &cands {
            by_rid.insert(c.rid, c.dim as usize);
            sorted_rids.push(c.rid);
        }
        sorted_rids.sort_unstable();
        let mut scored = Vec::with_capacity(cands.len());
        {
            let vl = self.vl.lock().expect("vl");
            let rv = vl.as_ref().ok_or(StoreError::Disposed)?;
            let mut stmt = rv.prepare(&format!(
                "SELECT rid, vec FROM vectors WHERE rid IN ({})",
                in_list(&sorted_rids)
            ))?;
            while stmt.step()? {
                let rid = stmt.get_i64(0)?;
                let blob = stmt.get_blob(1)?;
                let row = to_floats(&blob);
                let dim = by_rid[&rid].min(row.len()).min(q.len());
                scored.push((rid, BinaryVectorIndex::prefix_cosine(q, &row, dim)));
            }
        }
        let rerank_ms = vsw.elapsed().as_secs_f64() * 1000.0;
        *self.last_vector_timings.lock().expect("vec timings") =
            Some(format!("scan {scan_ms:.1} rerank-done {rerank_ms:.1}"));
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(VECTOR_KEEP);
        Ok(scored)
    }

    /// Lower-case host without `"www."`, at most 253 characters; anything else becomes empty.
    pub fn normalize_site(site: Option<&str>) -> String {
        let Some(site) = site else {
            return String::new();
        };
        let trimmed = site.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let mut s = trimmed.to_lowercase();
        if let Some(rest) = s.strip_prefix("www.") {
            s = rest.to_string();
        }
        let char_len = s.chars().count();
        if (1..=253).contains(&char_len)
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || (c as u32) > 127)
        {
            s
        } else {
            String::new()
        }
    }

    // ---------------------------------------------------------------- facets & search history

    pub fn facets(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: i32,
    ) -> Result<FacetsReply, StoreError> {
        let limit = limit.clamp(1, 200);
        let f = from.map_or(i64::MIN, |d| d.timestamp_millis());
        let t = to.map_or(i64::MAX, |d| d.timestamp_millis());
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let read_facets = |sql: &str| -> Result<Vec<FacetCount>, StoreError> {
            let mut list = Vec::new();
            let mut stmt = r.prepare(sql)?;
            stmt.bind_all(&[
                ("$from", Value::Int64(f)),
                ("$to", Value::Int64(t)),
                ("$n", Value::Int(limit)),
            ])?;
            while stmt.step()? {
                list.push(FacetCount {
                    name: stmt.get_string(0)?,
                    count: stmt.get_i32(1)?,
                });
            }
            Ok(list)
        };
        let apps = read_facets(
            "SELECT max(app), count(*) AS n FROM captures WHERE state = 1 AND ts >= $from AND ts < $to AND app <> ''
             GROUP BY app_lc ORDER BY n DESC, 1 LIMIT $n",
        )?;
        let sites = read_facets(
            "SELECT site, count(*) AS n FROM captures WHERE state = 1 AND ts >= $from AND ts < $to AND site <> ''
             GROUP BY site ORDER BY n DESC, 1 LIMIT $n",
        )?;
        Ok(FacetsReply { apps, sites })
    }

    pub fn history(&self, limit: i32) -> Result<Vec<HistoryEntry>, StoreError> {
        let limit = limit.clamp(1, 1000);
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let mut stmt = r.prepare("SELECT q, ts FROM search_history ORDER BY ts DESC LIMIT $n")?;
        stmt.bind_named("$n", Value::Int(limit))?;
        let mut list = Vec::new();
        while stmt.step()? {
            list.push(HistoryEntry {
                q: stmt.get_string(0)?,
                ts: ms_to_utc(stmt.get_i64(1)?),
            });
        }
        Ok(list)
    }

    pub fn add_history(&self, q: &str, ts: DateTime<Utc>, max: i32) -> Result<(), StoreError> {
        let q = q.trim();
        let len = q.chars().count();
        if len == 0 || len > 1024 {
            return Ok(());
        }
        let st = self.wl.lock();
        let w = st.w()?;
        w.execute(
            "INSERT INTO search_history(q, ts) VALUES($q, $ts) ON CONFLICT(q) DO UPDATE SET ts=excluded.ts",
            &[("$q", Value::Text(q)), ("$ts", Value::Int64(ts.timestamp_millis()))],
        )?;
        w.execute(
            "DELETE FROM search_history WHERE q NOT IN (SELECT q FROM search_history ORDER BY ts DESC LIMIT $n)",
            &[("$n", Value::Int(max.max(1)))],
        )?;
        Ok(())
    }

    pub fn delete_history(&self, q: Option<&str>) -> Result<usize, StoreError> {
        let st = self.wl.lock();
        let w = st.w()?;
        let n = match q {
            None => {
                let n = w.execute("DELETE FROM search_history", &[])?;
                checkpoint_locked(&st);
                n
            }
            Some(term) => w.execute(
                "DELETE FROM search_history WHERE q=$q",
                &[("$q", Value::Text(term.trim()))],
            )?,
        };
        Ok(n)
    }

    pub fn history_needles(scope: &PurgeScope) -> Vec<String> {
        let mut needles: Vec<String> = Vec::new();
        let mut add = |w: &str| {
            let w = w.trim();
            if w.chars().count() >= 3 && !needles.iter().any(|x| x.eq_ignore_ascii_case(w)) {
                needles.push(w.to_string());
            }
        };
        if scope.kind == "app"
            && let Some(ref app) = scope.app
            && !app.trim().is_empty()
        {
            let a = app.trim();
            add(a);
            if a.len() > 4 && a[a.len() - 4..].eq_ignore_ascii_case(".exe") {
                add(&a[..a.len() - 4]);
            }
        } else if scope.kind == "site" {
            let host = Self::normalize_site(scope.site.as_deref());
            if !host.is_empty() {
                let host = host.strip_prefix("www.").unwrap_or(&host);
                add(host);
                let labels: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
                if labels.len() >= 2 {
                    let mut i = labels.len() - 2;
                    if labels.len() >= 3
                        && matches!(
                            labels[labels.len() - 2],
                            "co" | "com" | "net" | "org" | "gov" | "edu" | "ac"
                        )
                    {
                        i = i.saturating_sub(1);
                    }
                    add(labels[i]);
                }
            }
        }
        needles
    }

    pub(crate) fn cut(s: &str, max: usize) -> String {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= max {
            s.to_string()
        } else {
            let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
            out.push('…');
            out
        }
    }

    pub fn screen_snippet(title: &str, text: Option<&str>, tokens: &[String]) -> String {
        let Some(text) = text else {
            return Self::cut(title, SNIPPET_MAX);
        };
        if text.is_empty() || tokens.is_empty() {
            return Self::cut(title, SNIPPET_MAX);
        }
        let title_lc = title.to_lowercase();
        let text_lc = text.to_lowercase();
        let in_title = tokens.iter().any(|t| title_lc.contains(&t.to_lowercase()));
        let in_text = tokens.iter().any(|t| text_lc.contains(&t.to_lowercase()));
        if in_text && !in_title {
            Self::snippet(text, tokens)
        } else {
            Self::cut(title, SNIPPET_MAX)
        }
    }

    pub fn snippet(text: &str, tokens: &[String]) -> String {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= SNIPPET_MAX {
            return text.to_string();
        }
        let text_lc = text.to_lowercase();
        let mut at_char: Option<usize> = None;
        for t in tokens {
            let t_lc = t.to_lowercase();
            if let Some(byte_pos) = text_lc.find(&t_lc) {
                at_char = Some(text_lc[..byte_pos].chars().count());
                break;
            }
        }
        let Some(at) = at_char else {
            return Self::cut(text, SNIPPET_MAX);
        };
        let start = at.saturating_sub(100);
        let len = (SNIPPET_MAX - 2).min(chars.len() - start);
        let mut s = String::new();
        if start > 0 {
            s.push('…');
        }
        for &c in &chars[start..start + len] {
            s.push(c);
        }
        if start + len < chars.len() {
            s.push('…');
        }
        let s_chars: Vec<char> = s.chars().collect();
        if s_chars.len() <= SNIPPET_MAX {
            s
        } else {
            s_chars[..SNIPPET_MAX].iter().collect()
        }
    }

    // ---------------------------------------------------------------- audit & stats

    pub fn append_audit(&self, e: &AuditEntry) -> Result<(), StoreError> {
        let st = self.wl.lock();
        st.w()?.execute(
            "INSERT INTO audit(ts, pipe, pid, image, verb, count, sha) VALUES($ts, $p, $pid, $img, $v, $n, $sha)",
            &[
                ("$ts", Value::Int64(e.ts.timestamp_millis())),
                ("$p", Value::Text(&e.pipe)),
                ("$pid", Value::Int(e.client_pid)),
                ("$img", Value::Text(&e.client_image)),
                ("$v", Value::Text(&e.verb)),
                ("$n", Value::Int(e.result_count)),
                ("$sha", Value::Text(&e.query_sha256)),
            ],
        )?;
        Ok(())
    }

    pub fn read_audit(&self, limit: i32) -> Result<Vec<AuditEntry>, StoreError> {
        let limit = limit.clamp(1, 10_000);
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let mut stmt = r.prepare(
            "SELECT ts, pipe, pid, image, verb, count, sha FROM audit ORDER BY seq DESC LIMIT $n",
        )?;
        stmt.bind_named("$n", Value::Int(limit))?;
        let mut list = Vec::new();
        while stmt.step()? {
            list.push(AuditEntry {
                ts: ms_to_utc(stmt.get_i64(0)?),
                pipe: stmt.get_string(1)?,
                client_pid: stmt.get_i32(2)?,
                client_image: stmt.get_string(3)?,
                verb: stmt.get_string(4)?,
                result_count: stmt.get_i32(5)?,
                query_sha256: stmt.get_string(6)?,
            });
        }
        Ok(list)
    }

    pub(crate) fn file_size(p: &Path) -> i64 {
        fs::metadata(p).map(|m| m.len() as i64).unwrap_or(0)
    }

    fn keyframe_bytes(&self) -> i64 {
        let mut t = 0i64;
        for m in self.kf_months() {
            let p = self.kf_path(&m);
            t += Self::file_size(&p) + Self::file_size(&wal_path(&p));
        }
        t
    }

    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let rows = r
            .query_scalar_i64("SELECT count(*) FROM captures WHERE state=1", &[])?
            .unwrap_or(0);
        let oldest = r.query_scalar_i64("SELECT min(ts) FROM captures WHERE state=1", &[])?;
        let newest = r.query_scalar_i64("SELECT max(ts) FROM captures WHERE state=1", &[])?;
        let pending = r
            .query_scalar_i64("SELECT count(*) FROM jobs", &[])?
            .unwrap_or(0);
        let vectors = r
            .query_scalar_i64("SELECT count(*) FROM vectors", &[])?
            .unwrap_or(0);
        let db = DataPaths::store_db(&self.home);
        let kf_bytes = self.keyframe_bytes();
        Ok(StoreStats {
            rows,
            bytes: Self::file_size(&db) + Self::file_size(&wal_path(&db)) + kf_bytes,
            thumb_bytes: kf_bytes,
            oldest: oldest.map(ms_to_utc),
            newest: newest.map(ms_to_utc),
            pending_embeddings: pending as i32,
            vectors,
        })
    }

    // ---------------------------------------------------------------- maintenance

    /// Aborts running maintenance between slices so `WM_ENDSESSION` `Checkpoint` does not wait behind it (C4).
    pub fn cancel_maintenance(&self) {
        self.maintenance_cancelled.store(true, Ordering::Release);
    }

    pub fn run_maintenance(&self, settings: &Settings, retention: bool) -> Result<(), StoreError> {
        self.run_maintenance_cancellable(settings, retention, None)
    }

    pub fn run_maintenance_cancellable(
        &self,
        settings: &Settings,
        retention: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<(), StoreError> {
        let res = self.run_maintenance_steps(settings, retention, cancel);
        let notice = {
            let mut st = self.wl.lock();
            if !st.disposed {
                take_notice_locked(&mut st, "retention", None, None, None)
            } else {
                None
            }
        };
        self.raise(notice);
        match res {
            Err(StoreError::Cancelled)
                if self.maintenance_cancelled.load(Ordering::Acquire)
                    || cancel.is_some_and(|c| c.load(Ordering::Acquire)) =>
            {
                Ok(())
            }
            other => other,
        }
    }

    fn run_maintenance_steps(
        &self,
        settings: &Settings,
        retention: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<(), StoreError> {
        let sw = Instant::now();
        let now = Utc::now();
        let cutoff = (now - ChronoDuration::days(i64::from(settings.retention.days))).timestamp_millis();
        let cutoff_month = Self::month_str(cutoff);

        // 1. Retention: rows older than retention.days, 5000 per slice.
        let mut expired = 0usize;
        while retention
            && self.maintenance_slice(cancel, |st| {
                let mut rids = Vec::new();
                {
                    let mut stmt = st
                        .w()?
                        .prepare("SELECT rid FROM captures WHERE ts < $c LIMIT 5000")?;
                    stmt.bind_named("$c", Value::Int64(cutoff))?;
                    while stmt.step()? {
                        rids.push(stmt.get_i64(0)?);
                    }
                }
                if rids.is_empty() {
                    return Ok(false);
                }
                expired += self.delete_rows_locked(st, &rids)?;
                Ok(true)
            })?
        {}
        if retention {
            self.maintenance_slice(cancel, |st| {
                st.w()?
                    .execute("DELETE FROM audit WHERE ts < $c", &[("$c", Value::Int64(cutoff))])?;
                st.w()?.execute(
                    "DELETE FROM search_history WHERE ts < $c",
                    &[("$c", Value::Int64(cutoff))],
                )?;
                Self::delete_chats_older_locked(st.w()?, cutoff)?;
                Ok(())
            })?;
        }

        // 2. Whole keyframe files from months before the cutoff month are empty now: delete the files.
        let months = self.maintenance_slice(cancel, |_| Ok(self.kf_months()))?;
        for m in months.into_iter().filter(|m| m < &cutoff_month) {
            self.maintenance_slice(cancel, |st| {
                let left = st
                    .w()?
                    .query_scalar_i64(
                        "SELECT count(*) FROM captures WHERE kf_month=$m AND has_keyframe=1",
                        &[("$m", Value::Text(&m))],
                    )?
                    .unwrap_or(0);
                if left == 0 {
                    self.delete_kf_file(st, &m);
                }
                Ok(())
            })?;
        }

        // 2b. Once per open: keyframe images whose row is gone.
        let need_orphans = self.maintenance_slice(cancel, |st| Ok(!st.orphans_swept))?;
        if need_orphans {
            let mut orphans = 0usize;
            let months = self.maintenance_slice(cancel, |_| Ok(self.kf_months()))?;
            for m in months {
                self.maintenance_slice(cancel, |st| {
                    orphans += self.delete_orphan_keyframes_locked(st, &m)?;
                    Ok(())
                })?;
            }
            self.maintenance_slice(cancel, |st| {
                st.orphans_swept = true;
                Ok(())
            })?;
            if orphans > 0 {
                Log::info(&format!("maintenance: {orphans} orphan keyframes deleted"));
            }
        }

        // 3. Progressive thinning, one day per slice.
        let mut thinned = 0usize;
        if retention
            && let Some((first_day, last_day)) =
                self.maintenance_slice(cancel, |st| {
                    Self::thin_days_locked(st.w()?, &settings.retention, now, cutoff)
                })?
        {
            for d in first_day..=last_day {
                self.maintenance_slice(cancel, |st| {
                    thinned += self.thin_day_locked(st, &settings.retention, now, cutoff, d)?;
                    Ok(())
                })?;
            }
        }
        if thinned > 0 {
            Log::info(&format!("maintenance: {thinned} rows thinned"));
        }

        let months = self.maintenance_slice(cancel, |_| Ok(self.kf_months()))?;
        for m in months {
            while self.maintenance_slice(cancel, |st| self.release_free_pages_one_locked(st, &m, true))?
            {}
        }

        self.maintenance_slice(cancel, |st| {
            checkpoint_locked(st);
            let _rl = self.rl.lock().expect("rl");
            let w = st.w()?;
            let free = w.query_scalar_i64("PRAGMA freelist_count", &[])?.unwrap_or(0);
            let pages = w.query_scalar_i64("PRAGMA page_count", &[])?.unwrap_or(0);
            let page_size = w.query_scalar_i64("PRAGMA page_size", &[])?.unwrap_or(4096);
            if pages > 0 && free * 4 > pages && free * page_size > 16 * 1024 * 1024 {
                let live = (pages - free) * page_size;
                let budget = self.vacuum_ram_budget_bytes.load(Ordering::Relaxed);
                if live <= budget {
                    w.exec_batch("VACUUM")?;
                    Log::info("store.db vacuumed");
                } else {
                    Log::info(&format!(
                        "store.db not vacuumed: {} MiB live is over the RAM budget",
                        live / (1024 * 1024)
                    ));
                }
            }
            let _ = w.exec_batch("PRAGMA optimize");
            Ok(())
        })?;
        Log::info(&format!(
            "maintenance: {expired} rows expired in {} ms",
            sw.elapsed().as_millis()
        ));
        Ok(())
    }

    fn maintenance_slice<T>(
        &self,
        cancel: Option<&AtomicBool>,
        step: impl FnOnce(&mut WriteState) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let wait = Instant::now();
        while self.priority_waiters.load(Ordering::Acquire) > 0
            && !self.maintenance_cancelled.load(Ordering::Acquire)
            && !cancel.is_some_and(|c| c.load(Ordering::Acquire))
            && wait.elapsed() < Duration::from_secs(30)
        {
            thread::sleep(Duration::from_millis(2));
        }
        if self.maintenance_cancelled.load(Ordering::Acquire)
            || cancel.is_some_and(|c| c.load(Ordering::Acquire))
        {
            return Err(StoreError::Cancelled);
        }
        let mut st = self.wl.lock();
        st.check_open()?;
        if self.maintenance_cancelled.load(Ordering::Acquire)
            || cancel.is_some_and(|c| c.load(Ordering::Acquire))
        {
            return Err(StoreError::Cancelled);
        }
        step(&mut st)
    }

    pub(crate) fn enter_write_priority(&self) -> TimedMutexGuard<'_, WriteState> {
        self.priority_waiters.fetch_add(1, Ordering::AcqRel);
        let guard = self.wl.lock();
        self.priority_waiters.fetch_sub(1, Ordering::AcqRel);
        guard
    }

    fn delete_orphan_keyframes_locked(
        &self,
        st: &mut WriteState,
        month: &str,
    ) -> Result<usize, StoreError> {
        let mut rows = HashSet::new();
        {
            let mut stmt = st
                .w()?
                .prepare("SELECT rid FROM captures WHERE kf_month=$m")?;
            stmt.bind_named("$m", Value::Text(month))?;
            while stmt.step()? {
                rows.insert(stmt.get_i64(0)?);
            }
        }
        let Some(kc) = self.kf_conn(st, month, false)? else {
            return Ok(0);
        };
        let mut orphans = Vec::new();
        {
            let mut stmt = kc.prepare("SELECT rid FROM kf")?;
            while stmt.step()? {
                let rid = stmt.get_i64(0)?;
                if !rows.contains(&rid) {
                    orphans.push(rid);
                }
            }
        }
        for chunk in orphans.chunks(500) {
            kc.exec_batch(&format!("DELETE FROM kf WHERE rid IN ({})", in_list(chunk)))?;
        }
        Ok(orphans.len())
    }

    // ---------------------------------------------------------------- test hooks

    pub fn keyframe_count_for_test(&self) -> Result<i64, StoreError> {
        let mut st = self.wl.lock();
        let mut n = 0i64;
        for m in self.kf_months() {
            if let Some(c) = self.kf_conn(&mut st, &m, false)? {
                n += c.query_scalar_i64("SELECT count(*) FROM kf", &[])?.unwrap_or(0);
            }
        }
        Ok(n)
    }

    pub fn delete_row_keep_image_for_test(&self, id: &str) -> Result<(), StoreError> {
        let st = self.wl.lock();
        let w = st.w()?;
        let Some(rid) = rid_of(w, id)? else {
            return Ok(());
        };
        w.execute("DELETE FROM vectors WHERE rid=$r", &[("$r", Value::Int64(rid))])?;
        w.execute("DELETE FROM jobs WHERE rid=$r", &[("$r", Value::Int64(rid))])?;
        w.execute("DELETE FROM captures WHERE rid=$r", &[("$r", Value::Int64(rid))])?;
        self.index.remove(rid);
        Ok(())
    }

    pub fn exec_month_for_test(&self, month: &str, sql: &str) -> Result<(), StoreError> {
        let mut st = self.wl.lock();
        let kc = self
            .kf_conn(&mut st, month, false)?
            .ok_or_else(|| StoreError::InvalidArgument(format!("month {month} not found")))?;
        kc.exec_batch(sql)?;
        Ok(())
    }

    pub fn query_month_i64_for_test(&self, month: &str, sql: &str) -> Result<Option<i64>, StoreError> {
        let mut st = self.wl.lock();
        let kc = self
            .kf_conn(&mut st, month, false)?
            .ok_or_else(|| StoreError::InvalidArgument(format!("month {month} not found")))?;
        Ok(kc.query_scalar_i64(sql, &[])?)
    }

    pub fn release_free_pages_paused_for_test(&self, month: &str) -> Result<bool, StoreError> {
        self.priority_waiters.store(1, Ordering::Release);
        let res = {
            let mut st = self.wl.lock();
            self.release_free_pages_one_locked(&mut st, month, true)
        };
        self.priority_waiters.store(0, Ordering::Release);
        res
    }

    pub fn insert_synthetic(
        &self,
        rows: impl IntoIterator<Item = (DateTime<Utc>, String, String, Vec<f32>)>,
        info: &EmbeddingInfo,
    ) -> Result<(), StoreError> {
        let mut st = self.wl.lock();
        st.check_open()?;
        if st.space.is_none() {
            write_space(st.w()?, info)?;
            st.space = Some(info.clone());
        }
        let in_space = Self::in_space(&st.space, &info.model_id, &info.revision);
        let w = st.w()?;
        let tx = Self::begin_write(w)?;
        let mut ins = w.prepare(
            "INSERT INTO captures(id, ts, kind, app, app_lc, exe, title, monitor, width, height, dhash, duration_ms, state, has_keyframe, keep_keyframe, kf_month)
             VALUES($id, $ts, 0, $app, $applc, '', $title, 0, 0, 0, 0, 0, 1, 0, 1, NULL) RETURNING rid",
        )?;
        let mut vec_stmt = w.prepare(
            "INSERT INTO vectors(rid, model_id, revision, dim, vec, bin) VALUES($rid, $m, $r, $d, $v, $b)",
        )?;
        let mut fts_stmt =
            w.prepare("INSERT INTO fts(rowid, title, app, text) VALUES($rid, $t, $a, NULL)")?;
        for (ts, app, title, vector) in rows {
            let v = normalize(&vector);
            let t = ts.timestamp_millis();
            let id = Ids::new('s', ts);
            let app_lc = app.to_lowercase();
            ins.reset()?;
            ins.bind_all(&[
                ("$id", Value::Text(&id)),
                ("$ts", Value::Int64(t)),
                ("$app", Value::Text(&app)),
                ("$applc", Value::Text(&app_lc)),
                ("$title", Value::Text(&title)),
            ])?;
            ins.step()?;
            let rid = ins.get_i64(0)?;
            ins.reset()?;

            let vb = to_bytes(&v);
            let bb = BinaryVectorIndex::sign_bits_bytes(&v);
            vec_stmt.reset()?;
            vec_stmt.bind_all(&[
                ("$rid", Value::Int64(rid)),
                ("$m", Value::Text(&info.model_id)),
                ("$r", Value::Text(&info.revision)),
                ("$d", Value::Int(info.dim)),
                ("$v", Value::Blob(&vb)),
                ("$b", Value::Blob(&bb)),
            ])?;
            vec_stmt.execute()?;

            fts_stmt.reset()?;
            fts_stmt.bind_all(&[
                ("$rid", Value::Int64(rid)),
                ("$t", Value::Text(&title)),
                ("$a", Value::Text(&app)),
            ])?;
            fts_stmt.execute()?;

            if in_space {
                self.index.upsert(rid, &v, t, KIND_SCREEN, &app_lc, "");
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------------- lifecycle

    /// Closes every connection and deletes `store.db` and all keyframe files (purge all).
    pub fn delete_all_files(&self) {
        self.close_for_purge_all();
        let _wl = self.wl.lock();
        let _rl = self.rl.lock().expect("rl");
        Self::delete_files(&self.home);
    }

    /// Purge all, first half: notifies listeners `"all"` before closing every connection.
    pub fn close_for_purge_all(&self) {
        self.raise(Some(PurgeNotice {
            reason: "all".to_string(),
            ids: Vec::new(),
            from: Some(DateTime::<Utc>::MIN_UTC),
            to: Some(Utc::now()),
            newest: None,
            scope: None,
        }));
        let mut st = self.wl.lock();
        let mut rl = self.rl.lock().expect("rl");
        self.close_locked(&mut st, &mut rl);
    }

    /// Deletes `store.db` (+ `-wal`, `-shm`) and every keyframe file of a data folder without opening them.
    pub fn delete_files(home: &Path) {
        let db = DataPaths::store_db(home);
        for p in db_trio(&db) {
            if p.exists()
                && let Err(e) = fs::remove_file(&p)
            {
                Log::error_err("store file delete failed", &e.to_string());
            }
        }
        let kf_dir = DataPaths::keyframes_dir(home);
        if let Ok(entries) = fs::read_dir(&kf_dir) {
            for entry in entries.flatten() {
                if let Err(e) = fs::remove_file(entry.path()) {
                    Log::error_err("keyframe file delete failed", &e.to_string());
                }
            }
        }
    }

    pub fn close(&self) {
        let mut st = self.wl.lock();
        let mut rl = self.rl.lock().expect("rl");
        self.close_locked(&mut st, &mut rl);
    }

    fn close_locked(&self, st: &mut WriteState, rl: &mut Option<Connection>) {
        if st.disposed {
            return;
        }
        st.disposed = true;
        if let Some(ref w) = st.w {
            let _ = w.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        }
        Self::abort_rebuild_locked(st, None);
        for (_, c) in st.kf.drain() {
            let _ = c.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            drop(c);
        }
        {
            let mut vl = self.vl.lock().expect("vl");
            *vl = None;
        }
        *rl = None;
        st.w = None;
        self.index.clear();
        st.inflight.clear();
    }
}

impl Drop for EncryptedStore {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------- module helpers

pub(crate) fn ms_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

pub(crate) fn wal_path(p: &Path) -> PathBuf {
    let mut os = p.as_os_str().to_os_string();
    os.push("-wal");
    PathBuf::from(os)
}

pub(crate) fn shm_path(p: &Path) -> PathBuf {
    let mut os = p.as_os_str().to_os_string();
    os.push("-shm");
    PathBuf::from(os)
}

pub(crate) fn db_trio(p: &Path) -> [PathBuf; 3] {
    [p.to_path_buf(), wal_path(p), shm_path(p)]
}

pub(crate) fn in_list(rids: &[i64]) -> String {
    let mut out = String::with_capacity(rids.len() * 6);
    for (i, &r) in rids.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(&mut out, "{r}");
    }
    out
}

pub(crate) fn checkpoint_locked(st: &WriteState) {
    if let Some(ref w) = st.w
        && let Err(e) = w.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)")
    {
        Log::warn_err("checkpoint failed", &e.to_string());
    }
    for c in st.kf.values() {
        if let Err(e) = c.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)") {
            Log::warn_err("keyframe checkpoint failed", &e.to_string());
        }
    }
}

pub(crate) fn take_notice_locked(
    st: &mut WriteState,
    reason: &str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    scope: Option<String>,
) -> Option<PurgeNotice> {
    let window = matches!((from, to), (Some(f), Some(t)) if t > f);
    if st.deleted.is_empty() && !window && scope.is_none() {
        return None;
    }
    let newest = st
        .deleted
        .iter()
        .map(|(_, ts)| *ts)
        .max()
        .map(ms_to_utc);
    let ids: Vec<String> = st.deleted.drain(..).map(|(id, _)| id).collect();
    Some(PurgeNotice {
        reason: reason.to_string(),
        ids,
        from: if window { from } else { None },
        to: if window { to } else { None },
        newest,
        scope,
    })
}

fn rollback_leftover(c: &Connection) {
    if !c.is_autocommit() {
        match c.exec_batch("ROLLBACK") {
            Ok(()) => Log::warn("store: rolled back a transaction left open by an earlier error"),
            Err(e) => Log::warn_err("store: rollback of a leftover transaction failed", &e.to_string()),
        }
    }
}

fn read_space(w: &Connection) -> Result<Option<EmbeddingInfo>, StoreError> {
    let Some(s) = w.query_scalar_string("SELECT v FROM meta WHERE k='space'", &[])? else {
        return Ok(None);
    };
    let parts: Vec<&str> = s.split('\n').collect();
    if parts.len() == 3
        && let Ok(dim) = parts[2].parse::<i32>()
    {
        return Ok(Some(EmbeddingInfo {
            model_id: parts[0].to_string(),
            revision: parts[1].to_string(),
            dim,
        }));
    }
    Ok(None)
}

fn write_space(w: &Connection, info: &EmbeddingInfo) -> Result<(), StoreError> {
    let v = format!("{}\n{}\n{}", info.model_id, info.revision, info.dim);
    w.execute(
        "INSERT INTO meta(k, v) VALUES('space', $v) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        &[("$v", Value::Text(&v))],
    )?;
    Ok(())
}

fn rid_of(w: &Connection, id: &str) -> Result<Option<i64>, StoreError> {
    Ok(w.query_scalar_i64(
        "SELECT rid FROM captures WHERE id=$id",
        &[("$id", Value::Text(id))],
    )?)
}

fn upper_bound_ms(scope: &PurgeScope, from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
    if scope.kind == "last" && (to - from) >= ChronoDuration::seconds(1) {
        i64::MAX
    } else {
        to.timestamp_millis()
    }
}

fn fence_locked(st: &mut WriteState, scope: &PurgeScope) {
    let now = Utc::now();
    let mono = (st.mono_ms)();
    let life = EncryptedStore::FENCE_LIFETIME_MS;
    st.fences.retain(|&(_, _, set_at)| mono - set_at < life);
    st.scope_fences
        .retain(|(_, _, _, _, set_at)| mono - set_at < life);

    if scope.kind == "app"
        && let Some(ref app) = scope.app
    {
        let a = app.trim().to_lowercase();
        if !a.is_empty() {
            let b = if let Some(stripped) = a.strip_suffix(".exe") {
                stripped.to_string()
            } else {
                format!("{a}.exe")
            };
            st.scope_fences
                .push(("app".to_string(), a, b, now.timestamp_millis(), mono));
            return;
        }
    }
    if scope.kind == "site" {
        let site = EncryptedStore::normalize_site(scope.site.as_deref());
        if !site.is_empty() {
            st.scope_fences.push((
                "site".to_string(),
                site.clone(),
                site,
                now.timestamp_millis(),
                mono,
            ));
            return;
        }
    }
    let Some(to) = scope.to else {
        return;
    };
    if scope.kind == "last" {
        st.fences.push((i64::MIN, to.timestamp_millis(), mono));
    } else if scope.kind == "range"
        && let Some(from) = scope.from
        && to >= now - ChronoDuration::milliseconds(EncryptedStore::FENCE_SLACK_MS)
    {
        let end = to.min(now);
        if end > from {
            st.fences
                .push((from.timestamp_millis(), end.timestamp_millis(), mono));
        }
    }
}

fn throw_if_fenced(
    st: &mut WriteState,
    ts: DateTime<Utc>,
    duration_ms: i32,
    app: Option<&str>,
    site: Option<&str>,
) -> Result<(), StoreError> {
    if st.fences.is_empty() && st.scope_fences.is_empty() {
        return Ok(());
    }
    let mono = (st.mono_ms)();
    let life = EncryptedStore::FENCE_LIFETIME_MS;
    st.fences.retain(|&(_, _, set_at)| mono - set_at < life);
    st.scope_fences
        .retain(|(_, _, _, _, set_at)| mono - set_at < life);
    let t = ts.timestamp_millis();
    let end = t.saturating_add(i64::from(duration_ms));
    for &(from, to, _) in &st.fences {
        if t <= to && end >= from {
            return Err(StoreError::PurgedWindow(PurgedWindowError(ts)));
        }
    }
    if st.scope_fences.is_empty() {
        return Ok(());
    }
    let app_lc = app.unwrap_or("").to_lowercase();
    let site_n = EncryptedStore::normalize_site(site);
    for (kind, a, b, to, _) in &st.scope_fences {
        if t > *to {
            continue;
        }
        let hit = if kind == "app" {
            !app_lc.is_empty() && (app_lc == *a || app_lc == *b)
        } else {
            !site_n.is_empty() && site_n == *a
        };
        if hit {
            return Err(StoreError::PurgedWindow(PurgedWindowError(ts)));
        }
    }
    Ok(())
}

fn delete_history_containing_locked(w: &Connection, needles: &[String]) -> Result<(), StoreError> {
    if needles.is_empty() {
        return Ok(());
    }
    let needles_lc: Vec<String> = needles.iter().map(|n| n.to_lowercase()).collect();
    let mut drop_list = Vec::new();
    {
        let mut stmt = w.prepare("SELECT q FROM search_history")?;
        while stmt.step()? {
            let q = stmt.get_string(0)?;
            let q_lc = q.to_lowercase();
            if needles_lc.iter().any(|n| q_lc.contains(n.as_str())) {
                drop_list.push(q);
            }
        }
    }
    for q in drop_list {
        w.execute("DELETE FROM search_history WHERE q=$q", &[("$q", Value::Text(&q))])?;
    }
    Ok(())
}

fn read_meta(stmt: &Statement<'_>) -> Result<CaptureMeta, SqliteError> {
    Ok(CaptureMeta {
        id: stmt.get_string(0)?,
        ts: ms_to_utc(stmt.get_i64(1)?),
        kind: kind_of(stmt.get_i32(2)?),
        app: stmt.get_string(3)?,
        title: stmt.get_string(4)?,
        monitor: stmt.get_i32(5)?,
        duration_ms: stmt.get_i32(6)?,
        has_thumb: stmt.get_i32(7)? == 1,
        embedded: !stmt.is_null(8),
        site: stmt.get_string(9)?,
        replay_id: None,
        replay_offset_ms: None,
    })
}

fn safe_reason(reason: &str) -> String {
    if reason.is_empty() {
        return "no reason".to_string();
    }
    reason
        .chars()
        .filter(|&ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_'))
        .take(40)
        .collect()
}

fn to_floats(blob: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(blob.len() / 4);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_le_bytes(chunk.try_into().unwrap()));
    }
    out
}

fn to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for &x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let mut r = v.to_vec();
    let mut n = 0.0f64;
    for &x in &r {
        n += f64::from(x) * f64::from(x);
    }
    if n > 0.0 {
        let inv = (1.0 / n.sqrt()) as f32;
        for x in &mut r {
            *x *= inv;
        }
    }
    r
}

fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || is_unicode_mark(c)
}

fn is_unicode_mark(c: char) -> bool {
    let u = c as u32;
    matches!(
        u,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x20D0..=0x20FF
            | 0xFE20..=0xFE2F
    )
}

/// Zeroizes a byte slice (used by callers scrubbing plaintext keyframe buffers).
#[allow(dead_code)]
pub(crate) fn zeroize_bytes(bytes: &mut [u8]) {
    bytes.zeroize();
}
