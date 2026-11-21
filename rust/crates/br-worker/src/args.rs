// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::{Path, PathBuf};

use br_contracts::full_path;

use crate::protocol::AgentPipe;

/// Parsed CLI arguments for `BetterRecall.Worker.exe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerArgs {
    pub pipe: String,
    pub home: PathBuf,
    pub settings_from_stdin: bool,
}

impl WorkerArgs {
    /// Parses `--pipe <name> --home <dir> [--settings-stdin]` (without argv[0]).
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Option<Self> {
        let mut pipe: Option<&str> = None;
        let mut home: Option<&str> = None;
        let mut settings_from_stdin = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_ref() {
                "--pipe" if i + 1 < args.len() => {
                    i += 1;
                    pipe = Some(args[i].as_ref());
                }
                "--home" if i + 1 < args.len() => {
                    i += 1;
                    home = Some(args[i].as_ref());
                }
                "--settings-stdin" => {
                    settings_from_stdin = true;
                }
                _ => return None,
            }
            i += 1;
        }
        let name = AgentPipe::normalize_name(pipe)?;
        let home_raw = home?;
        if home_raw.trim().is_empty() {
            return None;
        }
        let home_full = full_path(Path::new(home_raw)).ok()?;
        Some(Self {
            pipe: name,
            home: home_full,
            settings_from_stdin,
        })
    }
}
