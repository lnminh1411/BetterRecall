// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Plaintext ZIP exporter for searchable captures in `[from, to)`, optionally filtered by app.
//! Writes to `<path>.partial`, lists it in `ExportJournal`, and atomically moves it over `<path>`.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use br_contracts::{CaptureMeta, ExportRequest, ExportResult, PurgeNotice, ThumbData};
use chrono::{Datelike, Duration as ChronoDuration, Local, Timelike};
use zeroize::Zeroize;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime as ZipDateTime, ZipWriter};

use crate::core_json::{CoreJson, ExportEntry};
use crate::log::Log;
use crate::store::{EncryptedStore, ExportJournal, StoreError};
use crate::win32;

const PAGE: i32 = 2000;

pub struct CaptureExporter;

impl CaptureExporter {
    /// Rows in the window, newest first, filtered by app (process name, with or without `.exe`, any case).
    pub fn rows(store: &EncryptedStore, r: &ExportRequest) -> Result<Vec<CaptureMeta>, StoreError> {
        let app = normalize_app(r.app.as_deref());
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut to = r.to;
        loop {
            let page = store.timeline(r.from, to, PAGE)?;
            let page_len = page.len();
            let mut fresh = 0usize;
            for m in &page {
                if !seen.insert(m.id.clone()) {
                    continue;
                }
                fresh += 1;
                if app.is_none() || normalize_app(Some(&m.app)) == app {
                    out.push(m.clone());
                }
            }
            if (page_len as i32) < PAGE || fresh == 0 {
                break;
            }
            // Timeline is newest first and excludes `to`: step to just after the oldest row so equal timestamps are not lost.
            if let Some(last) = page.last() {
                to = last.ts + ChronoDuration::milliseconds(1);
            } else {
                break;
            }
        }
        Ok(out)
    }

    pub fn count(store: &EncryptedStore, r: &ExportRequest) -> Result<ExportResult, StoreError> {
        let mut n = 0i32;
        let mut images = 0i32;
        for m in Self::rows(store, r)? {
            n += 1;
            if m.has_thumb {
                images += 1;
            }
        }
        Ok(ExportResult {
            count: n,
            images,
            bytes: 0,
        })
    }

    /// Exports captures to a zip file. `transcode(keyframe, "jpeg")` converts when `r.jpeg` is set.
    pub fn write<F>(
        store: &EncryptedStore,
        r: &ExportRequest,
        transcode: F,
        cancel: Option<&AtomicBool>,
    ) -> Result<ExportResult, StoreError>
    where
        F: FnMut(ThumbData, &str) -> ThumbData,
    {
        Self::write_with_lock(store, r, transcode, None::<fn() -> bool>, cancel)
    }

    /// Exports captures to a zip file, aborting and deleting `.partial` on purge, lock, or cancellation.
    pub fn write_with_lock<F, L>(
        store: &EncryptedStore,
        r: &ExportRequest,
        mut transcode: F,
        mut is_locked: Option<L>,
        cancel: Option<&AtomicBool>,
    ) -> Result<ExportResult, StoreError>
    where
        F: FnMut(ThumbData, &str) -> ThumbData,
        L: FnMut() -> bool,
    {
        let target = PathBuf::from(&r.path);
        let dir = target.parent();
        if dir.is_none_or(|d| d.as_os_str().is_empty() || !d.is_dir()) {
            return Err(StoreError::DirectoryNotFound);
        }
        if is_locked.as_mut().is_some_and(|f| f()) {
            return Err(StoreError::ExportLocked);
        }

        let mut partial_os = target.as_os_str().to_os_string();
        partial_os.push(".partial");
        let partial = PathBuf::from(partial_os);

        let written = Arc::new(Mutex::new(HashSet::<String>::new()));
        let purged = Arc::new(AtomicBool::new(false));

        let listener_id = {
            let written_c = written.clone();
            let purged_c = purged.clone();
            let req_from = r.from;
            let req_to = r.to;
            store.on_purged(move |n: &PurgeNotice| {
                if !n.by_user() {
                    return;
                }
                let mut hit = n.reason == "all"
                    || matches!((n.from, n.to), (Some(f), Some(t)) if f < req_to && req_from <= t);
                if !hit {
                    let w = written_c.lock().expect("written");
                    hit = n.ids.iter().any(|id| w.contains(id));
                }
                if hit {
                    purged_c.store(true, Ordering::Release);
                }
            })
        };

        let home = Some(store.home());
        let mut created = false;

        let mut run = || -> Result<ExportResult, StoreError> {
            ExportJournal::add(home, &partial);
            let file = match OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .share_mode(0)
                .open(&partial)
            {
                Ok(f) => f,
                Err(e) => {
                    ExportJournal::forget(home, &partial);
                    return Err(StoreError::Io(e));
                }
            };
            created = true;

            let mut entries = Vec::new();
            let mut images = 0i32;

            {
                let mut zip = ZipWriter::new(file);
                let mut rows = Self::rows(store, r)?;
                rows.reverse();

                for m in rows {
                    check_abort(cancel, &purged, &mut is_locked)?;
                    written.lock().expect("written").insert(m.id.clone());
                    let Some(detail) = store.get(&m.id)? else {
                        continue;
                    };
                    let mut file_name: Option<String> = None;
                    if m.has_thumb
                        && let Some(mut kf) = store.get_keyframe(&m.id)?
                    {
                        let mut img = if r.jpeg {
                            let converted = transcode(kf.clone(), "jpeg");
                            kf.bytes.zeroize();
                            converted
                        } else {
                            kf
                        };
                        let name = format!(
                            "images/{}_{}.{}",
                            m.ts.with_timezone(&Local).format("%Y%m%d-%H%M%S-%3f"),
                            m.id,
                            extension(&img.mime)
                        );
                        let opts = SimpleFileOptions::default()
                            .compression_method(CompressionMethod::Stored)
                            .last_modified_time(to_zip_datetime(m.ts));
                        let write_res = zip
                            .start_file(&name, opts)
                            .and_then(|()| zip.write_all(&img.bytes).map_err(Into::into));
                        img.bytes.zeroize();
                        write_res.map_err(|e| StoreError::Zip(e.to_string()))?;
                        file_name = Some(name);
                        images += 1;
                    }
                    entries.push(ExportEntry {
                        id: m.id,
                        ts: m.ts,
                        kind: m.kind,
                        app: m.app,
                        title: detail.title,
                        site: detail.site,
                        text: detail.text,
                        file: file_name,
                        duration_s: detail.duration_s,
                    });
                }

                let json_opts =
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
                zip.start_file("captures.json", json_opts)
                    .map_err(|e| StoreError::Zip(e.to_string()))?;
                zip.write_all(b"[")?;
                for (i, entry) in entries.iter().enumerate() {
                    check_abort(cancel, &purged, &mut is_locked)?;
                    if i > 0 {
                        zip.write_all(b",")?;
                    }
                    let encoded = CoreJson::to_string(entry)?;
                    zip.write_all(encoded.as_bytes())?;
                }
                zip.write_all(b"]")?;
                zip.finish().map_err(|e| StoreError::Zip(e.to_string()))?;
            }

            check_abort(cancel, &purged, &mut is_locked)?;
            win32::move_replace(&partial, &target)?;
            ExportJournal::remove(home, &partial);

            let locked_after = is_locked.as_mut().is_some_and(|f| f());
            if purged.load(Ordering::Acquire) || locked_after {
                if let Err(e) = fs::remove_file(&target) {
                    Log::warn_err("export delete after abort", &e.to_string());
                }
                if purged.load(Ordering::Acquire) {
                    return Err(StoreError::ExportPurged);
                }
                return Err(StoreError::ExportLocked);
            }

            let bytes = fs::metadata(&target).map(|m| m.len() as i64).unwrap_or(0);
            Log::info(&format!(
                "export: {} captures, {images} images, {} KiB",
                entries.len(),
                bytes / 1024
            ));
            Ok(ExportResult {
                count: entries.len() as i32,
                images,
                bytes,
            })
        };

        let result = run();
        store.remove_purged_listener(listener_id);

        match result {
            Ok(res) => Ok(res),
            Err(err) => {
                if created {
                    let _ = fs::remove_file(&partial);
                    ExportJournal::remove(home, &partial);
                }
                if (purged.load(Ordering::Acquire) || matches!(err, StoreError::Disposed))
                    && !matches!(err, StoreError::ExportPurged | StoreError::ExportLocked)
                {
                    return Err(StoreError::ExportPurged);
                }
                Err(err)
            }
        }
    }
}

fn check_abort<L: FnMut() -> bool>(
    cancel: Option<&AtomicBool>,
    purged: &AtomicBool,
    is_locked: &mut Option<L>,
) -> Result<(), StoreError> {
    if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        return Err(StoreError::Cancelled);
    }
    if purged.load(Ordering::Acquire) {
        return Err(StoreError::ExportPurged);
    }
    if is_locked.as_mut().is_some_and(|f| f()) {
        return Err(StoreError::ExportLocked);
    }
    Ok(())
}

fn normalize_app(app: Option<&str>) -> Option<String> {
    let a = app?.trim();
    if a.is_empty() {
        return None;
    }
    let stripped = match br_contracts::strip_suffix_ci(a, ".exe") {
        Some(stem) if !stem.is_empty() => stem,
        _ => a,
    };
    Some(stripped.to_lowercase())
}

fn extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/avif" => "avif",
        "image/jxl" => "jxl",
        "image/png" => "png",
        _ => "webp",
    }
}

fn to_zip_datetime(ts: chrono::DateTime<chrono::Utc>) -> ZipDateTime {
    let year = ts.year().clamp(1980, 2107) as u16;
    ZipDateTime::from_date_and_time(
        year,
        ts.month() as u8,
        ts.day() as u8,
        ts.hour() as u8,
        ts.minute() as u8,
        ts.second() as u8,
    )
    .unwrap_or_default()
}
