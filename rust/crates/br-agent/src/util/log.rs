// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Agent logging facade backed by `br_core::Log`, with `--console` echo support.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use br_core::{Log, LogLevel};
use chrono::{DateTime, FixedOffset, Local};

static ALSO_CONSOLE: AtomicBool = AtomicBool::new(false);

pub struct AgentLog;

impl AgentLog {
    pub fn set_also_console(v: bool) {
        ALSO_CONSOLE.store(v, Ordering::Relaxed);
    }

    pub fn also_console() -> bool {
        ALSO_CONSOLE.load(Ordering::Relaxed)
    }

    pub fn init(logs_dir: impl Into<PathBuf>, level: &str) {
        Log::set_level(Log::parse_level(Some(level)));
        Log::init(logs_dir, "agent");
    }

    pub fn set_level(level: &str) {
        Log::set_level(Log::parse_level(Some(level)));
    }

    pub fn stamp(t: DateTime<FixedOffset>) -> String {
        Log::stamp(t)
    }

    pub fn debug(msg: &str) {
        Log::debug(msg);
        Self::echo(LogLevel::Debug, msg, None);
    }

    pub fn info(msg: &str) {
        Log::info(msg);
        Self::echo(LogLevel::Info, msg, None);
    }

    pub fn warn(msg: &str) {
        Log::warn(msg);
        Self::echo(LogLevel::Warn, msg, None);
    }

    pub fn warn_err(msg: &str, err: &str) {
        Log::warn_err(msg, err);
        Self::echo(LogLevel::Warn, msg, Some(err));
    }

    pub fn error(msg: &str) {
        Log::error(msg);
        Self::echo(LogLevel::Error, msg, None);
    }

    pub fn error_err(msg: &str, err: &str) {
        Log::error_err(msg, err);
        Self::echo(LogLevel::Error, msg, Some(err));
    }

    pub fn flush() {
        // br_core::Log flushes on every write.
    }

    fn echo(level: LogLevel, msg: &str, err: Option<&str>) {
        if !ALSO_CONSOLE.load(Ordering::Relaxed) || level < Log::level() {
            return;
        }
        let tag = match level {
            LogLevel::Debug => "DBG",
            LogLevel::Info => "INF",
            LogLevel::Warn => "WRN",
            LogLevel::Error => "ERR",
        };
        let ts = Log::stamp(Local::now().fixed_offset());
        match err {
            Some(e) => println!("{ts} {tag} {msg} | {e}"),
            None => println!("{ts} {tag} {msg}"),
        }
    }
}
