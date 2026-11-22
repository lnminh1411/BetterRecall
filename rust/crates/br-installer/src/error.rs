// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::io;
use std::path::PathBuf;

/// Exit codes returned by `BetterRecall-Setup.exe`.
pub mod exit_code {
    pub const OK: i32 = 0;
    pub const OTHER: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const IN_USE: i32 = 3;
    pub const DOWNGRADE: i32 = 4;
    pub const PAYLOAD: i32 = 5;
    pub const NOT_ENOUGH_SPACE: i32 = 6;
    pub const ALREADY_RUNNING: i32 = 7;
    pub const CANCELLED: i32 = 8;
    pub const MODELS: i32 = 9;
    pub const INTEGRITY: i32 = 10;
}

/// Installer error with one variant per CLI exit code plus `Io`.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{0}")]
    Other(String),

    #[error("invalid arguments: {0}")]
    Usage(String),

    #[error("BetterRecall is still in use: {}", path.display())]
    InUse { path: PathBuf },

    #[error(
        "installed version {installed} is newer than {incoming} (pass /allowdowngrade to override)"
    )]
    Downgrade { installed: String, incoming: String },

    #[error("installer payload is missing or damaged: {0}")]
    Payload(String),

    #[error("not enough disk space: need {needed} bytes, only {available} bytes available")]
    NotEnoughSpace { needed: u64, available: u64 },

    #[error("another BetterRecall installer is already running")]
    AlreadyRunning,

    #[error("setup was cancelled")]
    Cancelled,

    #[error("model download failed: {0}")]
    Models(String),

    #[error("integrity check failed: {0}")]
    Integrity(String),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl InstallError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Other(_) | Self::Io { .. } => exit_code::OTHER,
            Self::Usage(_) => exit_code::USAGE,
            Self::InUse { .. } => exit_code::IN_USE,
            Self::Downgrade { .. } => exit_code::DOWNGRADE,
            Self::Payload(_) => exit_code::PAYLOAD,
            Self::NotEnoughSpace { .. } => exit_code::NOT_ENOUGH_SPACE,
            Self::AlreadyRunning => exit_code::ALREADY_RUNNING,
            Self::Cancelled => exit_code::CANCELLED,
            Self::Models(_) => exit_code::MODELS,
            Self::Integrity(_) => exit_code::INTEGRITY,
        }
    }

    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
