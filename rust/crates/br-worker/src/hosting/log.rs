// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use br_contracts::full_path;
use chrono::{DateTime, FixedOffset, Local};

/// Rolling daily log in `<home>\logs\worker-YYYYMMDD.log`. Never logs captured content, titles,
/// query or transcript text: callers pass only op names, sizes, timings, device names and error types.
/// One file per day, 10 MiB cap, 14 files kept.
pub struct Log;

impl Log {
    pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
    pub const KEEP_FILES: usize = 14;

    pub fn init(logs_dir: Option<PathBuf>, level: &str) {
        sink().lock().expect("log lock").init(logs_dir, level);
    }

    pub fn debug(m: &str) {
        sink().lock().expect("log lock").write(0, "DBG", m);
    }

    pub fn info(m: &str) {
        sink().lock().expect("log lock").write(1, "INF", m);
    }

    pub fn warn(m: &str) {
        sink().lock().expect("log lock").write(2, "WRN", m);
    }

    pub fn error(m: &str) {
        sink().lock().expect("log lock").write(3, "ERR", m);
    }
}

fn sink() -> &'static Mutex<LogSink> {
    static SINK: LazyLock<Mutex<LogSink>> = LazyLock::new(|| Mutex::new(LogSink::new()));
    &SINK
}

type ClockFn = Box<dyn Fn() -> DateTime<FixedOffset> + Send>;

pub struct LogSink {
    now: ClockFn,
    dir: Option<PathBuf>,
    level: u8, // 0 debug, 1 info, 2 warn, 3 error
    current_path: Option<PathBuf>,
    current_day: Option<String>,
}

impl Default for LogSink {
    fn default() -> Self {
        Self::new()
    }
}

impl LogSink {
    pub fn new() -> Self {
        Self::with_clock(|| Local::now().fixed_offset())
    }

    pub fn with_clock<F>(clock: F) -> Self
    where
        F: Fn() -> DateTime<FixedOffset> + Send + 'static,
    {
        Self {
            now: Box::new(clock),
            dir: None,
            level: 1,
            current_path: None,
            current_day: None,
        }
    }

    pub fn init(&mut self, logs_dir: Option<PathBuf>, level: &str) {
        self.level = match level {
            "debug" => 0,
            "warn" => 2,
            "error" => 3,
            _ => 1,
        };
        self.current_path = None;
        self.current_day = None;
        let Some(dir) = logs_dir else {
            self.dir = None;
            return;
        };
        let resolved = full_path(&dir).unwrap_or(dir);
        if fs::create_dir_all(&resolved).is_err() {
            self.dir = None;
            return;
        }
        self.dir = Some(resolved);
        self.prune_locked();
    }

    pub fn write(&mut self, lvl: u8, tag: &str, m: &str) {
        if lvl < self.level {
            return;
        }
        let Some(ref dir) = self.dir else {
            return;
        };
        let now = (self.now)();
        let pid = std::process::id();
        let line = format!(
            "{} {tag} [{pid}] {m}\r\n",
            now.format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        );
        let day = now.format("%Y%m%d").to_string();
        let need_pick = self.current_path.is_none()
            || self.current_day.as_deref() != Some(day.as_str())
            || self.current_path.as_deref().is_some_and(is_over_limit);
        if need_pick {
            self.current_path = Some(pick_path(dir, &day, now));
            self.current_day = Some(day);
        }
        if let Some(ref path) = self.current_path
            && let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = f.write_all(line.as_bytes());
        }
        if need_pick {
            self.prune_locked();
        }
    }

    fn prune_locked(&self) {
        let Some(ref dir) = self.dir else {
            return;
        };
        let current_name = self
            .current_path
            .as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());

        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        struct LogEntry {
            path: PathBuf,
            name: String,
            is_current: bool,
            modified: SystemTime,
        }

        let mut files: Vec<LogEntry> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !is_worker_log_name(&name) {
                    return None;
                }
                let meta = e.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let is_current = current_name
                    .as_deref()
                    .is_some_and(|c| name.eq_ignore_ascii_case(c));
                Some(LogEntry {
                    path: e.path(),
                    name,
                    is_current,
                    modified,
                })
            })
            .collect();

        // Sort descending: current file first, then newest mtime, then ordinal name descending.
        files.sort_by(|a, b| {
            b.is_current
                .cmp(&a.is_current)
                .then_with(|| b.modified.cmp(&a.modified))
                .then_with(|| b.name.cmp(&a.name))
        });

        for f in files.into_iter().skip(Log::KEEP_FILES) {
            let _ = fs::remove_file(f.path);
        }
    }
}

fn is_worker_log_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("worker-") && lower.ends_with(".log")
}

fn is_over_limit(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.len() >= Log::MAX_FILE_BYTES)
        .unwrap_or(false)
}

fn pick_path(dir: &Path, day: &str, now: DateTime<FixedOffset>) -> PathBuf {
    let base = dir.join(format!("worker-{day}.log"));
    if !is_over_limit(&base) {
        return base;
    }
    let stamp = now.format("%H%M%S");
    let candidate = dir.join(format!("worker-{day}-{stamp}.log"));
    if !is_over_limit(&candidate) {
        return candidate;
    }
    for i in 1usize.. {
        let next = dir.join(format!("worker-{day}-{stamp}-{i}.log"));
        if !is_over_limit(&next) {
            return next;
        }
    }
    unreachable!()
}
