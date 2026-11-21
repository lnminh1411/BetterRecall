// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Storage policy: change score and window/app switch flag per screen row, progressive thinning,
//! deletion for space (`storage.overwrite`), legacy month-file rebuild, and live/free byte counts.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use br_contracts::{DataPaths, RetentionSettings, StorageUse};
use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::log::Log;
use crate::sqlite::{Connection, Value};
use crate::store::StoreError;
use crate::store::encrypted_store::{
    EncryptedStore, WriteState, db_trio, shm_path, take_notice_locked, wal_path,
};
use crate::win32;

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy)]
struct ScoredRow {
    rid: i64,
    ts: i64,
    change: f64,
    is_switch: bool,
}

/// A legacy month rebuild paused between batches for a priority caller (S3).
pub(crate) struct MonthRebuild {
    pub(crate) month: String,
    pub(crate) tmp: PathBuf,
    pub(crate) dst: Option<Connection>,
    pub(crate) last: i64,
    pub(crate) rows: i64,
}

impl EncryptedStore {
    pub(crate) fn migrate_storage(w: &Connection, cols: &HashSet<String>) -> Result<(), StoreError> {
        if !cols.contains("change_pct") {
            w.exec_batch("ALTER TABLE captures ADD COLUMN change_pct REAL")?;
        }
        if !cols.contains("is_switch") {
            w.exec_batch("ALTER TABLE captures ADD COLUMN is_switch INTEGER")?;
        }
        w.exec_batch("CREATE TABLE IF NOT EXISTS thin_days(day INTEGER PRIMARY KEY, total INTEGER NOT NULL)")?;
        Ok(())
    }

    fn screen_rows_locked(
        w: &Connection,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<ScoredRow>, StoreError> {
        let mut rows = Vec::new();
        let mut stmt = w.prepare(
            "SELECT rid, ts, monitor, app, title, dhash, change_pct, is_switch FROM captures
             WHERE kind=0 AND ts >= $f AND ts < $t ORDER BY monitor, ts",
        )?;
        stmt.bind_all(&[("$f", Value::Int64(from_ms)), ("$t", Value::Int64(to_ms))])?;
        let mut prev_monitor = i32::MIN;
        let mut prev_app = String::new();
        let mut prev_title = String::new();
        let mut prev_hash = 0i64;
        while stmt.step()? {
            let rid = stmt.get_i64(0)?;
            let ts = stmt.get_i64(1)?;
            let monitor = stmt.get_i32(2)?;
            let app = stmt.get_string(3)?;
            let title = stmt.get_string(4)?;
            let hash = stmt.get_i64(5)?;
            let change = stmt.get_opt_f64(6)?;
            let sw = stmt.get_opt_i32(7)?;
            let first = monitor != prev_monitor;
            let is_switch = match sw {
                Some(s) => s == 1,
                None => first || app != prev_app || title != prev_title,
            };
            let score = change.unwrap_or_else(|| {
                if first {
                    100.0
                } else {
                    f64::from(((prev_hash ^ hash) as u64).count_ones()) * 100.0 / 64.0
                }
            });
            rows.push(ScoredRow {
                rid,
                ts,
                change: score,
                is_switch,
            });
            prev_monitor = monitor;
            prev_app = app;
            prev_title = title;
            prev_hash = hash;
        }
        Ok(rows)
    }

    /// How many non-switch rows of a day survive at fraction `p` of the thinning period (`0` = all, `1` = none).
    pub fn thin_target(total: i32, p: f64) -> i32 {
        if p <= 0.0 {
            return total;
        }
        if p >= 1.0 {
            return 0;
        }
        (f64::from(total) * (1.0 - p)).ceil() as i32
    }

    pub(crate) fn thin_days_locked(
        w: &Connection,
        r: &RetentionSettings,
        now: DateTime<Utc>,
        cutoff_ms: i64,
    ) -> Result<Option<(i64, i64)>, StoreError> {
        let cutoff_day = cutoff_ms / DAY_MS;
        w.execute(
            "DELETE FROM thin_days WHERE day < $d",
            &[("$d", Value::Int64(cutoff_day))],
        )?;
        if r.progressive_after_days <= 0 {
            return Ok(None);
        }
        let last_day =
            (now.timestamp_millis() - i64::from(r.progressive_after_days) * DAY_MS) / DAY_MS;
        Ok(Some((cutoff_day, last_day)))
    }

    pub(crate) fn thin_day_locked(
        &self,
        st: &mut WriteState,
        r: &RetentionSettings,
        now: DateTime<Utc>,
        cutoff_ms: i64,
        day: i64,
    ) -> Result<usize, StoreError> {
        let now_ms = now.timestamp_millis();
        let over = r.progressive_over_days.max(1);
        let start = day * DAY_MS;
        let age_days = (now_ms - (start + DAY_MS / 2)) as f64 / (DAY_MS as f64);
        let p = (age_days - f64::from(r.progressive_after_days)) / f64::from(over);
        if p <= 0.0 {
            return Ok(0);
        }
        let mut candidates: Vec<ScoredRow> =
            Self::screen_rows_locked(st.w()?, start.max(cutoff_ms), start + DAY_MS)?
                .into_iter()
                .filter(|x| !x.is_switch)
                .collect();
        let total = match st
            .w()?
            .query_scalar_i64("SELECT total FROM thin_days WHERE day=$d", &[("$d", Value::Int64(day))])?
        {
            Some(t) => t,
            None => {
                if candidates.is_empty() {
                    return Ok(0);
                }
                let t = candidates.len() as i64;
                st.w()?.execute(
                    "INSERT INTO thin_days(day, total) VALUES($d, $n)",
                    &[("$d", Value::Int64(day)), ("$n", Value::Int64(t))],
                )?;
                t
            }
        };
        let keep = Self::thin_target(total.min(i64::from(i32::MAX)) as i32, p);
        let drop = (candidates.len() as i32) - keep;
        if drop <= 0 {
            return Ok(0);
        }
        candidates.sort_by(|a, b| a.change.total_cmp(&b.change).then_with(|| a.ts.cmp(&b.ts)));
        let rids: Vec<i64> = candidates
            .into_iter()
            .take(drop as usize)
            .map(|x| x.rid)
            .collect();
        self.delete_rows_locked(st, &rids)
    }

    fn space_order_locked(w: &Connection, max: usize) -> Result<Vec<i64>, StoreError> {
        let Some(oldest) = w.query_scalar_i64("SELECT min(ts) FROM captures", &[])? else {
            return Ok(Vec::new());
        };
        let start = (oldest / DAY_MS) * DAY_MS;
        let screen = Self::screen_rows_locked(w, start, start + DAY_MS)?;
        let mut non_sw: Vec<(i64, i64, f64)> = screen
            .iter()
            .filter(|x| !x.is_switch)
            .map(|x| (x.rid, x.ts, x.change))
            .collect();
        non_sw.sort_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.1.cmp(&b.1)));
        let mut order: Vec<(i64, i64)> = non_sw.into_iter().map(|(r, t, _)| (r, t)).collect();

        let mut rest: Vec<(i64, i64)> = screen
            .iter()
            .filter(|x| x.is_switch)
            .map(|x| (x.rid, x.ts))
            .collect();
        {
            let mut stmt = w.prepare(
                "SELECT rid, ts FROM captures WHERE kind<>0 AND ts >= $f AND ts < $t",
            )?;
            stmt.bind_all(&[("$f", Value::Int64(start)), ("$t", Value::Int64(start + DAY_MS))])?;
            while stmt.step()? {
                rest.push((stmt.get_i64(0)?, stmt.get_i64(1)?));
            }
        }
        rest.sort_by_key(|&(_, ts)| ts);
        order.extend(rest);
        Ok(order.into_iter().take(max).map(|(rid, _)| rid).collect())
    }

    pub fn delete_for_space(&self, max_rows: usize) -> Result<usize, StoreError> {
        if max_rows == 0 {
            return Ok(0);
        }
        let (total, notice) = {
            let mut st = self.wl.lock();
            st.check_open()?;
            let mut total = 0usize;
            while total < max_rows {
                let rids = Self::space_order_locked(st.w()?, max_rows - total)?;
                if rids.is_empty() {
                    break;
                }
                total += self.delete_rows_locked(&mut st, &rids)?;
            }
            if total > 0 {
                self.drop_empty_month_files_locked(&mut st)?;
                for m in self.kf_months() {
                    let _ = self.release_free_pages_one_locked(&mut st, &m, false)?;
                }
                Log::info(&format!("storage: {total} oldest rows deleted for space"));
            }
            let notice = take_notice_locked(&mut st, "space", None, None, None);
            (total, notice)
        };
        self.raise(notice);
        Ok(total)
    }

    fn drop_empty_month_files_locked(&self, st: &mut WriteState) -> Result<(), StoreError> {
        for m in self.kf_months() {
            let left = st
                .w()?
                .query_scalar_i64(
                    "SELECT count(*) FROM captures WHERE kf_month=$m AND has_keyframe=1",
                    &[("$m", Value::Text(&m))],
                )?
                .unwrap_or(0);
            if left != 0 {
                continue;
            }
            st.w()?.execute(
                "UPDATE captures SET state=2 WHERE kf_month=$m AND state=0",
                &[("$m", Value::Text(&m))],
            )?;
            st.w()?.execute(
                "DELETE FROM jobs WHERE rid IN (SELECT rid FROM captures WHERE kf_month=$m AND kind=0)",
                &[("$m", Value::Text(&m))],
            )?;
            self.delete_kf_file(st, &m);
        }
        Ok(())
    }

    /// Compacts one month file. Returns `Ok(true)` when a legacy rebuild paused for a priority caller.
    pub(crate) fn release_free_pages_one_locked(
        &self,
        st: &mut WriteState,
        m: &str,
        vacuum_legacy: bool,
    ) -> Result<bool, StoreError> {
        if st.rebuild.as_ref().is_some_and(|r| r.month == m) {
            if !vacuum_legacy {
                return Ok(false);
            }
            if self.kf_conn(st, m, false)?.is_some() {
                return Ok(self.rebuild_month_locked(st, m));
            }
            Self::abort_rebuild_locked(st, Some("the month file is gone"));
            return Ok(false);
        }

        let Some(c) = self.kf_conn(st, m, false)? else {
            return Ok(false);
        };
        let free = match c.query_scalar_i64("PRAGMA freelist_count", &[]) {
            Ok(v) => v.unwrap_or(0),
            Err(e) => {
                Log::warn_err("keyframe file compaction failed", &e.to_string());
                return Ok(false);
            }
        };
        if free == 0 {
            return Ok(false);
        }
        let mode = match c.query_scalar_i64("PRAGMA auto_vacuum", &[]) {
            Ok(v) => v.unwrap_or(0),
            Err(e) => {
                Log::warn_err("keyframe file compaction failed", &e.to_string());
                return Ok(false);
            }
        };
        if mode == 2 {
            if let Err(e) = c
                .exec_batch("PRAGMA incremental_vacuum")
                .and_then(|_| c.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)"))
            {
                Log::warn_err("keyframe file compaction failed", &e.to_string());
            }
            return Ok(false);
        }
        if !vacuum_legacy {
            return Ok(false);
        }
        let pages = c.query_scalar_i64("PRAGMA page_count", &[]).ok().flatten().unwrap_or(0);
        let page_size = c.query_scalar_i64("PRAGMA page_size", &[]).ok().flatten().unwrap_or(4096);
        let min_free = self.legacy_rebuild_min_free_bytes.load(Ordering::Relaxed);
        if pages > 0 && free * 4 > pages && free * page_size > min_free {
            return Ok(self.rebuild_month_locked(st, m));
        }
        Ok(false)
    }

    pub(crate) fn abort_rebuild_locked(st: &mut WriteState, why: Option<&str>) {
        let Some(mut r) = st.rebuild.take() else {
            return;
        };
        if let Some(reason) = why {
            Log::info(&format!("keyframe file rebuild dropped: {reason}"));
        }
        r.dst = None;
        delete_db_files(&r.tmp);
    }

    fn catch_up_rebuild_locked(old: &Connection, r: &mut MonthRebuild) -> Result<bool, StoreError> {
        let dst = r.dst.as_ref().ok_or(StoreError::Disposed)?;
        let copied = dst.query_scalar_i64("SELECT count(*) FROM kf", &[])?.unwrap_or(0);
        let still = old
            .query_scalar_i64(
                "SELECT count(*) FROM kf WHERE rid <= $r",
                &[("$r", Value::Int64(r.last))],
            )?
            .unwrap_or(0);
        if still == copied {
            return Ok(true);
        }
        if still > copied {
            return Ok(false);
        }
        let mut keep = HashSet::new();
        {
            let mut stmt = old.prepare("SELECT rid FROM kf WHERE rid <= $r")?;
            stmt.bind_named("$r", Value::Int64(r.last))?;
            while stmt.step()? {
                keep.insert(stmt.get_i64(0)?);
            }
        }
        let mut gone = Vec::new();
        {
            let mut stmt = dst.prepare("SELECT rid FROM kf")?;
            while stmt.step()? {
                let rid = stmt.get_i64(0)?;
                if !keep.contains(&rid) {
                    gone.push(rid);
                }
            }
        }
        let tx = dst.begin_transaction()?;
        for &rid in &gone {
            dst.execute("DELETE FROM kf WHERE rid=$r", &[("$r", Value::Int64(rid))])?;
        }
        tx.commit()?;
        r.rows -= gone.len() as i64;
        Ok(true)
    }

    fn rebuild_month_locked(&self, st: &mut WriteState, m: &str) -> bool {
        let path = self.kf_path(m);
        let res = self.rebuild_month_inner(st, m, &path);
        match res {
            Ok(paused) => paused,
            Err(e) => {
                Log::warn_err("keyframe file rebuild failed", &e.to_string());
                Self::abort_rebuild_locked(st, None);
                false
            }
        }
    }

    fn rebuild_month_inner(
        &self,
        st: &mut WriteState,
        m: &str,
        path: &Path,
    ) -> Result<bool, StoreError> {
        if let Some(mut existing) = st.rebuild.take() {
            let ok = existing.month == m
                && st
                    .kf
                    .get(m)
                    .is_some_and(|old| Self::catch_up_rebuild_locked(old, &mut existing).unwrap_or(false));
            if ok {
                st.rebuild = Some(existing);
            } else {
                existing.dst = None;
                delete_db_files(&existing.tmp);
                Log::info("keyframe file rebuild dropped: the month file changed while it was paused");
            }
        }

        if st.rebuild.is_none() {
            let fresh = self.kf_dir.join(format!("{m}.rebuild.tmp"));
            delete_db_files(&fresh);
            let dst = self.open_encrypted(&fresh, true)?;
            dst.exec_batch(
                "CREATE TABLE IF NOT EXISTS kf(rid INTEGER PRIMARY KEY, mime TEXT NOT NULL, width INTEGER NOT NULL, height INTEGER NOT NULL, data BLOB NOT NULL)",
            )?;
            st.rebuild = Some(MonthRebuild {
                month: m.to_string(),
                tmp: fresh,
                dst: Some(dst),
                last: i64::MIN,
                rows: 0,
            });
        }

        let batch_rows = self.rebuild_batch_rows.load(Ordering::Relaxed) as i64;
        loop {
            let n = {
                let Some(old) = st.kf.get(m) else {
                    return Err(StoreError::InvalidArgument("month conn missing".to_string()));
                };
                let r = st.rebuild.as_mut().expect("rebuild active");
                let dst = r.dst.as_ref().expect("rebuild dst");
                let tx = dst.begin_transaction()?;
                let mut stmt = old.prepare(
                    "SELECT rid, mime, width, height, data FROM kf WHERE rid > $r ORDER BY rid LIMIT $n",
                )?;
                stmt.bind_all(&[
                    ("$r", Value::Int64(r.last)),
                    ("$n", Value::Int64(batch_rows)),
                ])?;
                let mut copied = 0i64;
                while stmt.step()? {
                    let rid = stmt.get_i64(0)?;
                    let mime = stmt.get_string(1)?;
                    let w = stmt.get_i64(2)?;
                    let h = stmt.get_i64(3)?;
                    let data = stmt.get_blob(4)?;
                    r.last = rid;
                    dst.execute(
                        "INSERT INTO kf(rid, mime, width, height, data) VALUES($rid, $mime, $w, $h, $data)",
                        &[
                            ("$rid", Value::Int64(rid)),
                            ("$mime", Value::Text(&mime)),
                            ("$w", Value::Int64(w)),
                            ("$h", Value::Int64(h)),
                            ("$data", Value::Blob(&data)),
                        ],
                    )?;
                    copied += 1;
                }
                tx.commit()?;
                r.rows += copied;
                copied
            };
            if n == 0 {
                break;
            }
            if let Some(ref hook) = st.after_rebuild_batch {
                let h = hook.clone();
                h();
            }
            if self.priority_waiters.load(Ordering::Acquire) > 0 {
                st.rebuild_pauses += 1;
                return Ok(true);
            }
        }

        let mut r = st.rebuild.take().expect("rebuild active");
        if let Some(ref dst) = r.dst {
            dst.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        }
        r.dst = None;
        let tmp = r.tmp.clone();
        let rows = r.rows;

        if let Some(old) = st.kf.remove(m) {
            let _ = old.exec_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            drop(old);
        }
        for x in [wal_path(path), shm_path(path), wal_path(&tmp), shm_path(&tmp)] {
            if x.exists() {
                fs::remove_file(&x)?;
            }
        }
        win32::move_replace(&tmp, path)?;
        Log::info(&format!("keyframe file rebuilt ({rows} keyframes)"));
        Ok(false)
    }

    pub fn get_storage_use(&self) -> Result<StorageUse, StoreError> {
        let mut st = self.wl.lock();
        st.check_open()?;
        let db = DataPaths::store_db(&self.home);
        let mut files = Self::file_size(&db) + Self::file_size(&wal_path(&db));
        let mut free = free_bytes(st.w()?);
        for m in self.kf_months() {
            let p = self.kf_path(&m);
            files += Self::file_size(&p) + Self::file_size(&wal_path(&p));
            if let Some(c) = self.kf_conn(&mut st, &m, false)? {
                free += free_bytes(c);
            }
        }
        Ok(StorageUse {
            file_bytes: files,
            free_bytes: free.min(files),
        })
    }

    /// Tests: change score and switch flag as stored (`None` when unknown).
    pub fn stored_score(&self, id: &str) -> Result<(Option<f64>, Option<bool>), StoreError> {
        let st = self.wl.lock();
        let mut stmt = st
            .w()?
            .prepare("SELECT change_pct, is_switch FROM captures WHERE id=$id")?;
        stmt.bind_named("$id", Value::Text(id))?;
        if !stmt.step()? {
            return Ok((None, None));
        }
        Ok((
            stmt.get_opt_f64(0)?,
            stmt.get_opt_i32(1)?.map(|v| v == 1),
        ))
    }

    /// Tests: runs only the thinning step at a given clock.
    pub fn thin_for_test(
        &self,
        r: &RetentionSettings,
        now: DateTime<Utc>,
    ) -> Result<usize, StoreError> {
        let mut st = self.wl.lock();
        let cutoff = (now - ChronoDuration::days(i64::from(r.days.max(1)))).timestamp_millis();
        let mut deleted = 0usize;
        if let Some((first, last)) = Self::thin_days_locked(st.w()?, r, now, cutoff)? {
            for day in first..=last {
                deleted += self.thin_day_locked(&mut st, r, now, cutoff, day)?;
            }
        }
        Ok(deleted)
    }
}

fn free_bytes(c: &Connection) -> i64 {
    let free = c.query_scalar_i64("PRAGMA freelist_count", &[]).ok().flatten().unwrap_or(0);
    let page = c.query_scalar_i64("PRAGMA page_size", &[]).ok().flatten().unwrap_or(4096);
    free * page
}

fn delete_db_files(path: &Path) {
    for x in db_trio(path) {
        if x.exists()
            && let Err(e) = fs::remove_file(&x)
        {
            Log::warn_err("temporary file delete failed", &e.to_string());
        }
    }
}
