// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Appends timestamped lines to `<home>\logs\setup.log`. Never logs file contents.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use br_contracts::DataPaths;
use windows::Win32::System::SystemInformation::GetSystemTime;

/// Appends one UTC-timestamped line to `<home>\logs\setup.log`.
/// Errors are ignored so logging never masks a real install error.
pub fn log_line(home: &Path, message: &str) {
    let dir = DataPaths::logs_dir(home);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("setup.log");
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let ts = utc_now();
    let clean: String = message
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    let _ = writeln!(file, "{ts} {clean}");
}

fn utc_now() -> String {
    let st = unsafe { GetSystemTime() };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}
