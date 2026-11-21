// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use br_contracts::full_path;
use chrono::{DateTime, FixedOffset, Local};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl LogLevel {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Debug,
            2 => Self::Warn,
            3 => Self::Error,
            _ => Self::Info,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Debug => "DBG",
            Self::Info => "INF",
            Self::Warn => "WRN",
            Self::Error => "ERR",
        }
    }
}

/// Rolling file log in `<home>\logs`. Callers must never pass captured content: no titles, queries,
/// transcripts, paths of captured windows or key material. One file per day per prefix, 10 MiB cap, 14 files kept.
pub struct Log;

static GLOBAL_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

fn global_sink() -> &'static Mutex<LogSink> {
    static SINK: LazyLock<Mutex<LogSink>> = LazyLock::new(|| Mutex::new(LogSink::new()));
    &SINK
}

impl Log {
    pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
    pub const KEEP_FILES: usize = 14;

    pub fn level() -> LogLevel {
        LogLevel::from_u8(GLOBAL_LEVEL.load(Ordering::Relaxed))
    }

    pub fn set_level(level: LogLevel) {
        GLOBAL_LEVEL.store(level as u8, Ordering::Relaxed);
        if let Ok(mut s) = global_sink().lock() {
            s.set_level(level);
        }
    }

    pub fn initialized() -> bool {
        global_sink()
            .lock()
            .map(|s| s.initialized())
            .unwrap_or(false)
    }

    /// Starts file logging. Safe to call more than once; the last call wins.
    pub fn init(logs_dir: impl Into<PathBuf>, prefix: impl Into<String>) {
        if let Ok(mut s) = global_sink().lock() {
            s.init(Some(logs_dir.into()), prefix.into());
        }
    }

    /// Stops file logging (primarily for tests).
    pub fn disable() {
        if let Ok(mut s) = global_sink().lock() {
            s.init(None, "core".to_string());
        }
    }

    pub fn parse_level(s: Option<&str>) -> LogLevel {
        match s.map(|v| v.to_ascii_lowercase()).as_deref() {
            Some("debug") => LogLevel::Debug,
            Some("warn") => LogLevel::Warn,
            Some("error") => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }

    pub fn debug(msg: &str) {
        Self::write(LogLevel::Debug, msg, None);
    }

    pub fn info(msg: &str) {
        Self::write(LogLevel::Info, msg, None);
    }

    pub fn warn(msg: &str) {
        Self::write(LogLevel::Warn, msg, None);
    }

    pub fn warn_err(msg: &str, err_kind: &str) {
        Self::write(LogLevel::Warn, msg, Some(err_kind));
    }

    pub fn error(msg: &str) {
        Self::write(LogLevel::Error, msg, None);
    }

    pub fn error_err(msg: &str, err_kind: &str) {
        Self::write(LogLevel::Error, msg, Some(err_kind));
    }

    /// Local time with its UTC offset, matching Core, Agent, and Worker logs (`yyyy-MM-ddTHH:mm:ss.fffzzz`).
    pub fn stamp(t: DateTime<FixedOffset>) -> String {
        t.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
    }

    fn write(level: LogLevel, msg: &str, err_kind: Option<&str>) {
        if (level as u8) < GLOBAL_LEVEL.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut s) = global_sink().lock() {
            s.write(level, msg, err_kind);
        }
    }
}

type ClockFn = Box<dyn Fn() -> DateTime<FixedOffset> + Send>;

pub struct LogSink {
    now: ClockFn,
    dir: Option<PathBuf>,
    prefix: String,
    level: LogLevel,
    writer: Option<File>,
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
            prefix: "core".to_string(),
            level: LogLevel::Info,
            writer: None,
            current_path: None,
            current_day: None,
        }
    }

    pub fn initialized(&self) -> bool {
        self.dir.is_some()
    }

    pub fn level(&self) -> LogLevel {
        self.level
    }

    pub fn set_level(&mut self, level: LogLevel) {
        self.level = level;
    }

    pub fn init(&mut self, logs_dir: Option<PathBuf>, prefix: String) {
        self.close_writer();
        self.prefix = prefix;
        let Some(dir) = logs_dir else {
            self.dir = None;
            return;
        };
        let resolved = full_path(&dir).unwrap_or(dir);
        self.dir = Some(resolved.clone());
        let _ = fs::create_dir_all(&resolved);
        self.prune_locked();
    }

    pub fn write(&mut self, level: LogLevel, msg: &str, err_kind: Option<&str>) {
        if level < self.level {
            return;
        }
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let now = (self.now)();
        let tid = current_thread_num();
        let mut line = format!("{} {} {} {}", Log::stamp(now), level.tag(), tid, msg);
        if let Some(kind) = err_kind {
            line.push_str(" | ");
            line.push_str(kind);
        }
        line.push_str("\r\n");

        let day = now.format("%Y%m%d").to_string();
        let need_rotate = self.writer.is_none()
            || self.current_day.as_deref() != Some(day.as_str())
            || self
                .writer
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .is_some_and(|m| m.len() > Log::MAX_FILE_BYTES);

        if need_rotate {
            self.close_writer();
            let path = pick_path(&dir, &self.prefix, &day, now);
            let share = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
            if let Ok(f) = OpenOptions::new()
                .create(true)
                .append(true)
                .share_mode(share)
                .open(&path)
            {
                self.writer = Some(f);
                self.current_path = Some(path);
                self.current_day = Some(day);
                self.prune_locked();
            }
        }
        if let Some(ref mut f) = self.writer {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    fn close_writer(&mut self) {
        self.writer = None;
        self.current_path = None;
        self.current_day = None;
    }

    fn prune_locked(&self) {
        let Some(dir) = self.dir.clone() else {
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
        let prefix_dash = format!("{}-", self.prefix.to_ascii_lowercase());
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
                let lower = name.to_ascii_lowercase();
                if !lower.starts_with(&prefix_dash) || !lower.ends_with(".log") {
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

fn is_over_limit(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.len() > Log::MAX_FILE_BYTES)
        .unwrap_or(false)
}

fn pick_path(dir: &Path, prefix: &str, day: &str, now: DateTime<FixedOffset>) -> PathBuf {
    let base = dir.join(format!("{prefix}-{day}.log"));
    if !is_over_limit(&base) {
        return base;
    }
    let stamp = now.format("%H%M%S");
    let candidate = dir.join(format!("{prefix}-{day}-{stamp}.log"));
    if !is_over_limit(&candidate) {
        return candidate;
    }
    for i in 1usize.. {
        let next = dir.join(format!("{prefix}-{day}-{stamp}-{i}.log"));
        if !is_over_limit(&next) {
            return next;
        }
    }
    unreachable!()
}

fn current_thread_num() -> u64 {
    // ThreadId Debug is "ThreadId(N)"; parse N for clean numeric thread id matching C# logs.
    let s = format!("{:?}", std::thread::current().id());
    s.strip_prefix("ThreadId(")
        .and_then(|r| r.strip_suffix(')'))
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}
