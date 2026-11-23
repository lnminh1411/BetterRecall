// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use br_core::{Log, LogLevel, LogSink};
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, NaiveDate, Utc};

struct ScopedDir {
    path: PathBuf,
}

impl ScopedDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = common::test_tmp_root()
            .join("br-core-log")
            .join(format!("{tag}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create log test dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn to_system_time(dt: DateTime<FixedOffset>) -> SystemTime {
    let secs = dt.timestamp().max(0) as u64;
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

// ---- Sweep3LCoreTests.cs ----

#[test]
fn core_log_stamps_local_time_with_offset() {
    let offset = FixedOffset::east_opt(7 * 3600).unwrap();
    let naive = NaiveDate::from_ymd_opt(2026, 11, 16)
        .unwrap()
        .and_hms_milli_opt(4, 5, 6, 789)
        .unwrap();
    let t = DateTime::<FixedOffset>::from_naive_utc_and_offset(naive - ChronoDuration::hours(7), offset);
    assert_eq!("2026-11-16T04:05:06.789+07:00", Log::stamp(t));
}

#[test]
fn core_log_rotates_at_10mib_and_prunes_beyond_14_files() {
    let dir = ScopedDir::new("rotate");
    let utc = FixedOffset::east_opt(0).unwrap();
    let start_day = Utc::now().date_naive().and_hms_opt(10, 0, 0).unwrap();
    let now_base = DateTime::<FixedOffset>::from_naive_utc_and_offset(start_day, utc);
    let clock_val = Arc::new(Mutex::new(now_base));

    let mut old_names = Vec::new();
    for d in 1..=18 {
        let old_date = now_base - ChronoDuration::days(30 - d);
        let name = format!("core-{}.log", old_date.format("%Y%m%d"));
        old_names.push(name.clone());
        let old_path = dir.path().join(&name);
        fs::write(&old_path, "old\n").unwrap();
        let f = OpenOptions::new().write(true).open(&old_path).unwrap();
        f.set_modified(to_system_time(old_date)).unwrap();
    }

    let today_file = dir
        .path()
        .join(format!("core-{}.log", now_base.format("%Y%m%d")));
    {
        let f = File::create(&today_file).unwrap();
        f.set_len(Log::MAX_FILE_BYTES + 1).unwrap();
        f.set_modified(to_system_time(now_base)).unwrap();
    }

    let c = clock_val.clone();
    let mut sink = LogSink::with_clock(move || *c.lock().unwrap());
    sink.set_level(LogLevel::Debug);
    sink.init(Some(dir.path().to_path_buf()), "core".to_string());

    let list_names = || -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("core-") && n.ends_with(".log"))
            .collect();
        v.sort();
        v
    };

    let after_init: HashSet<String> = list_names().into_iter().collect();
    assert_eq!(Log::KEEP_FILES, after_init.len());
    assert!(!after_init.contains(&old_names[4]));
    assert!(after_init.contains(&old_names[5]));

    sink.write(LogLevel::Debug, "first-rotated", None);
    let first_rot = dir
        .path()
        .join(format!("core-{}-100000.log", now_base.format("%Y%m%d")));
    assert!(first_rot.exists());
    assert!(fs::read_to_string(&first_rot).unwrap().contains("first-rotated"));
    assert_eq!(Log::KEEP_FILES, list_names().len());
}
