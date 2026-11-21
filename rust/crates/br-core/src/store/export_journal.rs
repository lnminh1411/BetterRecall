// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Tracks in-flight `.partial` export files and undeleted replay files in `<home>\exports.pending`
//! so a killed agent's leftovers are swept at the next start.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use br_contracts::{DataPaths, full_path};

use crate::log::Log;
use crate::win32;

const SUFFIX: &str = ".partial";
static GATE: Mutex<()> = Mutex::new(());

pub struct ExportJournal;

impl ExportJournal {
    pub fn path_for(home: &Path) -> PathBuf {
        home.join("exports.pending")
    }

    /// Only these are ever deleted: `.partial` files, and files directly in `<home>\replays`.
    fn deletable(home: &Path, path: &Path) -> bool {
        let s = path.to_string_lossy();
        if s.len() >= SUFFIX.len()
            && s[s.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
        {
            return true;
        }
        let replays_dir = full_path(&DataPaths::replays_dir(home))
            .unwrap_or_else(|_| DataPaths::replays_dir(home));
        let Some(parent) = path.parent() else {
            return false;
        };
        parent
            .to_string_lossy()
            .eq_ignore_ascii_case(&replays_dir.to_string_lossy())
    }

    /// Records a partial file before it is created, or a replay file purge all could not delete.
    /// No-op when `home` is `None`.
    pub fn add(home: Option<&Path>, partial: &Path) {
        let Some(home) = home else {
            return;
        };
        let full = full_path(partial).unwrap_or_else(|_| partial.to_path_buf());
        if !Self::deletable(home, &full) {
            return;
        }
        let full_str = full.to_string_lossy().into_owned();
        let _guard = GATE.lock().expect("export journal gate");
        let mut list = read_entries(home);
        if !list.iter().any(|p| p.eq_ignore_ascii_case(&full_str)) {
            list.push(full_str);
        }
        if let Err(e) = write_entries(home, &list) {
            Log::warn_err("export journal", &e.to_string());
        }
    }

    /// Forgets a partial file once it is gone (renamed to the target or deleted).
    /// One that still exists stays listed, so the next start deletes it.
    pub fn remove(home: Option<&Path>, partial: &Path) {
        let Some(home) = home else {
            return;
        };
        let full = full_path(partial).unwrap_or_else(|_| partial.to_path_buf());
        if full.exists() {
            return;
        }
        let full_str = full.to_string_lossy().into_owned();
        let _guard = GATE.lock().expect("export journal gate");
        let mut list = read_entries(home);
        let before = list.len();
        list.retain(|p| !p.eq_ignore_ascii_case(&full_str));
        if list.len() != before
            && let Err(e) = write_entries(home, &list)
        {
            Log::warn_err("export journal", &e.to_string());
        }
    }

    /// Forgets a path whether or not the file exists (the partial could not be created, so a file
    /// of that name is not ours).
    pub fn forget(home: Option<&Path>, partial: &Path) {
        let Some(home) = home else {
            return;
        };
        let full = full_path(partial).unwrap_or_else(|_| partial.to_path_buf());
        let full_str = full.to_string_lossy().into_owned();
        let _guard = GATE.lock().expect("export journal gate");
        let mut list = read_entries(home);
        let before = list.len();
        list.retain(|p| !p.eq_ignore_ascii_case(&full_str));
        if list.len() != before
            && let Err(e) = write_entries(home, &list)
        {
            Log::warn_err("export journal", &e.to_string());
        }
    }

    /// Start only (no export runs yet): deletes every listed file that is still there.
    /// Only `.partial` files and files in `<home>\replays` are ever deleted, never a finished export.
    /// Returns how many were deleted.
    pub fn sweep(home: &Path) -> usize {
        let mut n = 0usize;
        {
            let _guard = GATE.lock().expect("export journal gate");
            let list = read_entries(home);
            if list.is_empty() {
                return 0;
            }
            let mut left = Vec::new();
            for p in list {
                let path = PathBuf::from(&p);
                if !Self::deletable(home, &path) {
                    continue;
                }
                if path.exists() {
                    match fs::remove_file(&path) {
                        Ok(()) => n += 1,
                        Err(e) => {
                            Log::warn_err("export partial delete failed", &e.to_string());
                            left.push(p);
                        }
                    }
                }
            }
            if let Err(e) = write_entries(home, &left) {
                Log::warn_err("export journal", &e.to_string());
            }
        }
        if n > 0 {
            Log::info(&format!(
                "exports: {n} unfinished export or leftover replay file(s) deleted"
            ));
        }
        n
    }
}

fn read_entries(home: &Path) -> Vec<String> {
    let f = ExportJournal::path_for(home);
    let Ok(content) = fs::read_to_string(&f) else {
        return Vec::new();
    };
    content
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn write_entries(home: &Path, list: &[String]) -> std::io::Result<()> {
    let f = ExportJournal::path_for(home);
    if list.is_empty() {
        if f.exists() {
            fs::remove_file(&f)?;
        }
        return Ok(());
    }
    fs::create_dir_all(home)?;
    let mut tmp_os = f.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    let mut text = String::new();
    for line in list {
        text.push_str(line);
        text.push_str("\r\n");
    }
    fs::write(&tmp, text.as_bytes())?;
    win32::move_replace(&tmp, &f)?;
    Ok(())
}
