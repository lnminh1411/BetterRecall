// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Error types for `br-fetch`.

use std::io;
use std::path::PathBuf;

/// Errors returned by catalog loading, downloads, verification, and disk-space checks.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("no internet connection")]
    Offline,

    #[error("HTTP {status} for {url}")]
    Http { status: u16, url: String },

    #[error("verification failed for {}", path.display())]
    Verify { path: PathBuf },

    #[error("not enough disk space: need {needed} bytes, only {available} bytes available")]
    NotEnoughSpace { needed: u64, available: u64 },

    #[error("download cancelled")]
    Cancelled,

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    #[error("invalid model catalog: {0}")]
    Catalog(String),
}

impl FetchError {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
