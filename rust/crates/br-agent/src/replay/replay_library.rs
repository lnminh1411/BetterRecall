// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Saved replay clip library: encrypted/plain writes, listing, mvhd duration, export, delete (`Replay\ReplayLibrary.cs`).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use br_contracts::{DataPaths, ReplayClip, Settings, full_path};
use br_core::{AgentError, BlobCipher, BlobWriter, CancelSource, CancelToken, ExportJournal};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, Utc};

use crate::util::AgentLog;

type SettingsProvider = Arc<dyn Fn() -> Settings + Send + Sync>;
type BoolProvider = Arc<dyn Fn() -> bool + Send + Sync>;

/// Writable encrypted blob stream (`BlobWriter` or test fake).
pub trait ReplayBlobWriter: Write + Send {
    fn finish_blob(self: Box<Self>) -> io::Result<()>;
}

impl ReplayBlobWriter for BlobWriter {
    fn finish_blob(self: Box<Self>) -> io::Result<()> {
        (*self).finish()
    }
}

/// Cipher abstraction for [`ReplayLibrary`] (implemented for [`BlobCipher`] and test fakes).
pub trait ReplayBlobCipher: Send + Sync {
    fn open_write(&self, path: &Path) -> Result<Box<dyn ReplayBlobWriter>, String>;
    fn open_read(&self, path: &Path) -> Result<Box<dyn Read + Send>, String>;
}

impl ReplayBlobCipher for BlobCipher {
    fn open_write(&self, path: &Path) -> Result<Box<dyn ReplayBlobWriter>, String> {
        BlobCipher::open_write(self, path)
            .map(|w| Box::new(w) as Box<dyn ReplayBlobWriter>)
            .map_err(|e| e.to_string())
    }

    fn open_read(&self, path: &Path) -> Result<Box<dyn Read + Send>, String> {
        BlobCipher::open_read(self, path)
            .map(|r| Box::new(r) as Box<dyn Read + Send>)
            .map_err(|e| e.to_string())
    }
}

/// Saved replay clips: `replays\<ts>.mp4.enc` (AES-256-GCM via [`BlobCipher`]) or plain `<ts>.mp4`.
pub struct ReplayLibrary {
    home: PathBuf,
    settings: SettingsProvider,
    cipher: Option<Arc<dyn ReplayBlobCipher>>,
    durations: Mutex<HashMap<PathBuf, (u64, i32)>>,
    reserved: Mutex<HashSet<String>>,
    purge_epoch: AtomicI32,
    purge_cts: Mutex<CancelSource>,
    purge_lock: Mutex<()>,
    purging_all: Mutex<Option<BoolProvider>>,
    locked: Mutex<Option<BoolProvider>>,
}

impl ReplayLibrary {
    /// Millisecond-resolution clip id format (`yyyyMMdd'T'HHmmssfff'Z'`).
    pub const TIME_FORMAT: &'static str = "%Y%m%dT%H%M%S%3fZ";
    /// Legacy one-second resolution clip id format (`yyyyMMdd'T'HHmmss'Z'`).
    pub const LEGACY_TIME_FORMAT: &'static str = "%Y%m%dT%H%M%SZ";

    pub fn new(
        home: PathBuf,
        settings: SettingsProvider,
        cipher: Option<Arc<dyn ReplayBlobCipher>>,
    ) -> Self {
        Self {
            home,
            settings,
            cipher,
            durations: Mutex::new(HashMap::new()),
            reserved: Mutex::new(HashSet::new()),
            purge_epoch: AtomicI32::new(0),
            purge_cts: Mutex::new(CancelSource::new()),
            purge_lock: Mutex::new(()),
            purging_all: Mutex::new(None),
            locked: Mutex::new(None),
        }
    }

    pub fn purge_epoch(&self) -> i32 {
        self.purge_epoch.load(Ordering::Acquire)
    }

    pub fn set_purging_all(&self, f: Option<BoolProvider>) {
        *self.purging_all.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    pub fn set_locked(&self, f: Option<BoolProvider>) {
        *self.locked.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    fn is_purging(&self) -> bool {
        self.purging_all
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|f| f())
    }

    fn is_locked(&self) -> bool {
        self.locked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|f| f())
    }

    /// Purge all: running decrypted exports stop and delete their output; saves in flight see a new epoch.
    pub fn on_purge_all(&self) {
        let _g = self.purge_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.purge_epoch.fetch_add(1, Ordering::AcqRel);
        let old = {
            let mut slot = self.purge_cts.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *slot, CancelSource::new())
        };
        old.cancel();
    }

    /// A user purge reached the replay ring: saves in flight discard their clip.
    pub fn discard_saves_in_flight(&self) {
        let _g = self.purge_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.purge_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub fn encrypted_dir(&self) -> PathBuf {
        DataPaths::replays_dir(&self.home)
    }

    pub fn plain_dir(&self) -> PathBuf {
        let d = (self.settings)().replay.export_dir;
        if d.trim().is_empty() {
            DataPaths::replays_dir(&self.home)
        } else {
            PathBuf::from(d)
        }
    }

    pub fn new_id(ts: DateTime<Utc>) -> String {
        ts.format(Self::TIME_FORMAT).to_string()
    }

    pub fn try_parse_id(id: &str) -> Option<DateTime<Utc>> {
        if id.len() == 19
            && id.ends_with('Z')
            && let Ok(ndt) = NaiveDateTime::parse_from_str(id, "%Y%m%dT%H%M%S%3fZ")
        {
            return Some(DateTime::from_naive_utc_and_offset(ndt, Utc));
        }
        if id.len() == 16
            && id.ends_with('Z')
            && let Ok(ndt) = NaiveDateTime::parse_from_str(id, "%Y%m%dT%H%M%SZ")
        {
            return Some(DateTime::from_naive_utc_and_offset(ndt, Utc));
        }
        None
    }

    /// Returns an id for a clip saved at `ts` that no clip, partial file, or in-progress save uses.
    pub fn unique_id(&self, ts: DateTime<Utc>) -> String {
        let mut res = self.reserved.lock().unwrap_or_else(|e| e.into_inner());
        let mut i = 0i64;
        loop {
            let id = Self::new_id(ts + ChronoDuration::milliseconds(i));
            if !res.contains(&id) && !self.taken(&id) {
                res.insert(id.clone());
                return id;
            }
            i += 1;
        }
    }

    fn taken(&self, id: &str) -> bool {
        let candidates = [
            self.encrypted_dir().join(format!("{id}.mp4.enc")),
            self.plain_dir().join(format!("{id}.mp4")),
        ];
        for p in &candidates {
            if p.exists() || partial_path(p).exists() {
                return true;
            }
        }
        false
    }

    /// Deletes `*.partial` files left by a failed or interrupted save (start only).
    pub fn delete_partials(&self) -> usize {
        let mut n = 0usize;
        let targets = [
            (self.encrypted_dir(), ".mp4.enc.partial"),
            (self.plain_dir(), ".mp4.partial"),
        ];
        for (dir, suffix) in &targets {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if !name.ends_with(suffix) {
                    continue;
                }
                match fs::remove_file(&p) {
                    Ok(()) => n += 1,
                    Err(e) => AgentLog::warn_err(
                        &format!("replay partial {name}"),
                        &e.to_string(),
                    ),
                }
            }
        }
        if n > 0 {
            AgentLog::info(&format!("replays: {n} unfinished save(s) deleted"));
        }
        n
    }

    /// Writes a clip through `save` into `path + ".partial"` (encrypted or plain), then renames
    /// it to `path`. A failed save or concurrent purge deletes the partial instead of renaming.
    pub fn write_clip(
        &self,
        path: &Path,
        encrypted: bool,
        save: impl FnOnce(&mut dyn Write) -> Result<i32, String>,
        ct: &CancelToken,
    ) -> Result<i32, AgentError> {
        let partial = partial_path(path);
        let epoch = self.purge_epoch();
        if self.is_purging() {
            return Err(AgentError::InvalidOperation(
                "Purge all is running.".to_string(),
            ));
        }
        if ct.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let res = (|| -> Result<i32, AgentError> {
            let ms = if encrypted {
                let cipher = self
                    .cipher
                    .as_ref()
                    .ok_or_else(|| AgentError::InvalidOperation("Store is locked.".to_string()))?;
                let mut writer = cipher.open_write(&partial).map_err(AgentError::Other)?;
                let ms = save(&mut *writer).map_err(AgentError::InvalidOperation)?;
                writer
                    .finish_blob()
                    .map_err(|e| AgentError::Other(e.to_string()))?;
                ms
            } else {
                let mut f = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .share_mode(0)
                    .open(&partial)
                    .map_err(|e| AgentError::Other(e.to_string()))?;
                let ms = save(&mut f).map_err(AgentError::InvalidOperation)?;
                f.flush().map_err(|e| AgentError::Other(e.to_string()))?;
                ms
            };

            if ct.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            let _g = self.purge_lock.lock().unwrap_or_else(|e| e.into_inner());
            if self.purge_epoch() != epoch || self.is_purging() {
                return Err(AgentError::InvalidOperation(
                    "A purge ran during the replay save; the clip was discarded.".to_string(),
                ));
            }
            if path.exists() {
                return Err(AgentError::Other("Replay clip already exists.".to_string()));
            }
            fs::rename(&partial, path).map_err(|e| AgentError::Other(e.to_string()))?;
            Ok(ms)
        })();

        if res.is_err()
            && partial.exists()
            && let Err(e) = fs::remove_file(&partial)
        {
            AgentLog::warn_err("replay partial delete", &e.to_string());
        }
        res
    }

    pub fn target_for(&self, id: &str) -> (PathBuf, bool) {
        let enc = (self.settings)().replay.encrypt_exports;
        if enc {
            (self.encrypted_dir().join(format!("{id}.mp4.enc")), true)
        } else {
            (self.plain_dir().join(format!("{id}.mp4")), false)
        }
    }

    pub fn list(&self) -> Vec<ReplayClip> {
        let mut list = Vec::new();
        self.add_dir(&mut list, &self.encrypted_dir(), ".mp4.enc", true);
        self.add_dir(&mut list, &self.plain_dir(), ".mp4", false);
        list.sort_by_key(|c| std::cmp::Reverse(c.ts));
        list
    }

    fn add_dir(&self, list: &mut Vec<ReplayClip>, dir: &Path, suffix: &str, encrypted: bool) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(id) = br_contracts::strip_suffix_ci(name, suffix).filter(|id| !id.is_empty())
            else {
                continue;
            };
            let Some(ts) = Self::try_parse_id(id) else {
                continue;
            };
            if list.iter().any(|c| c.id == id) {
                continue;
            }
            let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let duration_ms = self.duration(&path, len, encrypted);
            list.push(ReplayClip {
                id: id.to_string(),
                ts,
                duration_ms,
                bytes: len as i64,
                encrypted,
                path: path.to_string_lossy().into_owned(),
            });
        }
    }

    pub fn find(&self, id: &str) -> Option<ReplayClip> {
        self.list().into_iter().find(|c| c.id == id)
    }

    fn duration(&self, path: &Path, length: u64, encrypted: bool) -> i32 {
        {
            let map = self.durations.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(&(l, ms)) = map.get(path)
                && l == length
            {
                return ms;
            }
        }
        let ms = if encrypted {
            self.cipher
                .as_ref()
                .and_then(|c| c.open_read(path).ok())
                .map(|mut r| Self::read_mvhd_duration_ms(&mut r))
                .unwrap_or(0)
        } else {
            File::open(path)
                .map(|mut f| Self::read_mvhd_duration_ms(&mut f))
                .unwrap_or(0)
        };
        self.durations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_path_buf(), (length, ms));
        ms
    }

    /// Walks top-level boxes to `moov`/`mvhd`. Reads at most the first 64 KiB.
    pub fn read_mvhd_duration_ms<R: Read + ?Sized>(s: &mut R) -> i32 {
        let mut buf = vec![0u8; 65536];
        let mut n = 0usize;
        while n < buf.len() {
            match s.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(r) => n += r,
                Err(_) => break,
            }
        }
        let span = &buf[..n];
        let mut pos = 0usize;
        while pos + 8 <= span.len() {
            let size = u32::from_be_bytes([span[pos], span[pos + 1], span[pos + 2], span[pos + 3]])
                as usize;
            let ty = &span[pos + 4..pos + 8];
            if size < 8 {
                break;
            }
            if ty == b"moov" {
                let mut inner = pos + 8;
                let end = span.len().min(pos + size);
                while inner + 8 <= end {
                    let isz = u32::from_be_bytes([
                        span[inner],
                        span[inner + 1],
                        span[inner + 2],
                        span[inner + 3],
                    ]) as usize;
                    let ity = &span[inner + 4..inner + 8];
                    if isz < 8 {
                        break;
                    }
                    if ity == b"mvhd" && inner + 28 <= end {
                        let version = span[inner + 8];
                        if version == 0 {
                            let timescale = u32::from_be_bytes([
                                span[inner + 20],
                                span[inner + 21],
                                span[inner + 22],
                                span[inner + 23],
                            ]) as i64;
                            let duration = u32::from_be_bytes([
                                span[inner + 24],
                                span[inner + 25],
                                span[inner + 26],
                                span[inner + 27],
                            ]) as i64;
                            return if timescale == 0 {
                                0
                            } else {
                                (duration * 1000 / timescale) as i32
                            };
                        }
                        if inner + 40 <= end {
                            let ts1 = u32::from_be_bytes([
                                span[inner + 28],
                                span[inner + 29],
                                span[inner + 30],
                                span[inner + 31],
                            ]) as i64;
                            let dur1 = u64::from_be_bytes([
                                span[inner + 32],
                                span[inner + 33],
                                span[inner + 34],
                                span[inner + 35],
                                span[inner + 36],
                                span[inner + 37],
                                span[inner + 38],
                                span[inner + 39],
                            ]) as i64;
                            return if ts1 == 0 {
                                0
                            } else {
                                (dur1 * 1000 / ts1) as i32
                            };
                        }
                        return 0;
                    }
                    inner += isz;
                }
                return 0;
            }
            pos += size;
        }
        0
    }

    pub fn export(&self, id: &str, target: &Path, ct: &CancelToken) -> Result<(), AgentError> {
        let purge = {
            let _g = self.purge_lock.lock().unwrap_or_else(|e| e.into_inner());
            self.purge_cts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .token()
        };
        if self.is_purging() {
            return Err(AgentError::NotFound);
        }
        if self.is_locked() {
            return Err(AgentError::InvalidOperation("Store is locked.".to_string()));
        }
        let clip = self.find(id).ok_or(AgentError::NotFound)?;
        let full = full_path(target).unwrap_or_else(|_| target.to_path_buf());
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).map_err(|e| AgentError::Other(e.to_string()))?;
        }
        let partial = partial_path(&full);
        let mut created = false;

        let res = (|| -> Result<(), AgentError> {
            let mut src: Box<dyn Read> = if clip.encrypted {
                let cipher = self
                    .cipher
                    .as_ref()
                    .ok_or_else(|| AgentError::InvalidOperation("Store is locked.".to_string()))?;
                cipher
                    .open_read(Path::new(&clip.path))
                    .map_err(AgentError::Other)?
            } else {
                Box::new(File::open(&clip.path).map_err(|e| AgentError::Other(e.to_string()))?)
            };

            if self.is_purging() {
                return Err(AgentError::NotFound);
            }
            if self.is_locked() {
                return Err(AgentError::InvalidOperation("Store is locked.".to_string()));
            }
            ExportJournal::add(Some(&self.home), &partial);
            let mut dst = match OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .share_mode(0)
                .open(&partial)
            {
                Ok(f) => f,
                Err(e) => {
                    ExportJournal::forget(Some(&self.home), &partial);
                    return Err(AgentError::Other(e.to_string()));
                }
            };
            created = true;

            let mut buf = vec![0u8; 65536];
            loop {
                if ct.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                if purge.is_cancelled() {
                    AgentLog::info("replay export aborted: purge all ran during it");
                    return Err(AgentError::NotFound);
                }
                if self.is_locked() {
                    return Err(AgentError::InvalidOperation("Store is locked.".to_string()));
                }
                let n = src
                    .read(&mut buf)
                    .map_err(|e| AgentError::Other(e.to_string()))?;
                if n == 0 {
                    break;
                }
                dst.write_all(&buf[..n])
                    .map_err(|e| AgentError::Other(e.to_string()))?;
            }
            dst.flush().map_err(|e| AgentError::Other(e.to_string()))?;
            drop(dst);
            drop(src);

            {
                let _g = self.purge_lock.lock().unwrap_or_else(|e| e.into_inner());
                if ct.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                if purge.is_cancelled() {
                    AgentLog::info("replay export aborted: purge all ran during it");
                    return Err(AgentError::NotFound);
                }
                if self.is_locked() {
                    return Err(AgentError::InvalidOperation("Store is locked.".to_string()));
                }
                br_core::win32::move_replace(&partial, &full)
                    .map_err(|e| AgentError::Other(e.to_string()))?;
            }
            ExportJournal::remove(Some(&self.home), &partial);
            if self.is_locked() {
                let _ = fs::remove_file(&full);
                return Err(AgentError::InvalidOperation("Store is locked.".to_string()));
            }
            Ok(())
        })();

        if res.is_err() && created {
            let _ = fs::remove_file(&partial);
            ExportJournal::remove(Some(&self.home), &partial);
        }
        res
    }

    pub fn delete(&self, id: &str) -> Result<(), AgentError> {
        let clip = self.find(id).ok_or(AgentError::NotFound)?;
        let path = PathBuf::from(&clip.path);
        fs::remove_file(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                AgentError::NotFound
            } else {
                AgentError::Other(e.to_string())
            }
        })?;
        self.durations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&path);
        Ok(())
    }
}

fn partial_path(p: &Path) -> PathBuf {
    let mut os = p.as_os_str().to_os_string();
    os.push(".partial");
    PathBuf::from(os)
}
