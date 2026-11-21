// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use thiserror::Error;

/// Error strings of `docs/v1-design.md` section 4. Sent verbatim in `WorkerResponse.error`.
pub struct WorkerErrors;

impl WorkerErrors {
    pub const BAD_REQUEST: &'static str = "Bad request.";
    pub const INTERNAL: &'static str = "Internal error.";

    pub fn model_not_installed(name: &str) -> String {
        format!("Model not installed: {name}.")
    }

    pub fn device_failed(detail: &str) -> String {
        format!("Device failed: {}.", Self::clean(detail))
    }

    /// One line, no trailing period, bounded length, so the wire string keeps the `Device failed: x.` shape.
    pub fn clean(detail: &str) -> String {
        let single_line = detail.replace(['\r', '\n'], " ");
        let mut trimmed = single_line.trim();
        while let Some(stripped) = trimmed.strip_suffix('.') {
            trimmed = stripped.trim_end();
        }
        let capped: String = trimmed.chars().take(300).collect();
        if capped.is_empty() {
            "unknown".to_string()
        } else {
            capped
        }
    }
}

/// Errors whose wire message goes back to the agent.
#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("{}", WorkerErrors::BAD_REQUEST)]
    BadRequest(Option<String>),

    #[error("{}", WorkerErrors::model_not_installed(.0))]
    ModelNotInstalled(String),

    #[error("{}", WorkerErrors::device_failed(.0))]
    DeviceFailed(String),

    /// Input, not the device, exceeded the cap (OCR box cap): `Device failed: ...` on the wire.
    #[error("{}", WorkerErrors::device_failed(.0))]
    OcrOverflow(String),

    #[error("{}", WorkerErrors::INTERNAL)]
    Internal(String),
}

impl WorkerError {
    pub fn bad_request(why: impl Into<String>) -> Self {
        Self::BadRequest(Some(why.into()))
    }

    pub fn bad_request_why(&self) -> Option<&str> {
        match self {
            Self::BadRequest(Some(why)) => Some(why.as_str()),
            _ => None,
        }
    }

    pub fn wire_message(&self) -> String {
        self.to_string()
    }
}
