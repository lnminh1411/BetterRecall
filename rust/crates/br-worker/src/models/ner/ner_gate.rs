// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::time::Instant;

use br_contracts::{OcrEntity, OcrLine};

use crate::hosting::Log;
use crate::protocol::WorkerError;

/// Runs name and address detection on OCR lines without ever losing lines on NER failure (C9).
/// Missing models are rechecked on the next frame; other failures back off for 10 minutes.
pub struct NerGate {
    now: Box<dyn FnMut() -> i64 + Send>,
    retry_at: i64,
    last_log: i64,
    pub last_error: Option<String>,
}

impl Default for NerGate {
    fn default() -> Self {
        Self::new()
    }
}

impl NerGate {
    pub const BACKOFF_MS: i64 = 10 * 60 * 1000;

    pub fn new() -> Self {
        let epoch = Instant::now();
        Self::with_clock(move || epoch.elapsed().as_millis() as i64)
    }

    pub fn with_clock<F>(now: F) -> Self
    where
        F: FnMut() -> i64 + Send + 'static,
    {
        Self {
            now: Box::new(now),
            retry_at: i64::MIN,
            last_log: i64::MIN,
            last_error: None,
        }
    }

    pub fn apply<F>(
        &mut self,
        lines: Vec<OcrLine>,
        mut ensure_and_find: F,
    ) -> (Vec<OcrLine>, bool)
    where
        F: FnMut(&[String]) -> Result<Vec<Option<Vec<OcrEntity>>>, WorkerError>,
    {
        if lines.is_empty() {
            return (lines, false);
        }
        let now = (self.now)();
        if now < self.retry_at {
            return (lines, true);
        }
        let texts: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
        match ensure_and_find(&texts) {
            Ok(found) if found.len() == lines.len() => {
                self.last_error = None;
                let enriched = lines
                    .into_iter()
                    .zip(found)
                    .map(|(mut l, ents)| {
                        l.entities = Some(ents.unwrap_or_default());
                        l
                    })
                    .collect();
                (enriched, false)
            }
            Err(WorkerError::ModelNotInstalled(_)) => (lines, true),
            Err(e) => {
                let kind = match &e {
                    WorkerError::DeviceFailed(_) => "DeviceFailedException",
                    WorkerError::OcrOverflow(_) => "OcrOverflowException",
                    WorkerError::BadRequest(_) => "BadRequestException",
                    WorkerError::Internal(_) => "InvalidDataException",
                    WorkerError::ModelNotInstalled(_) => "ModelNotInstalledException",
                };
                self.record_failure(now, kind);
                (lines, true)
            }
            Ok(_) => {
                self.record_failure(now, "InvalidDataException");
                (lines, true)
            }
        }
    }

    fn record_failure(&mut self, now: i64, kind: &str) {
        self.retry_at = now.saturating_add(Self::BACKOFF_MS);
        self.last_error = Some(kind.to_string());
        if self.last_log == i64::MIN || now.saturating_sub(self.last_log) >= Self::BACKOFF_MS {
            self.last_log = now;
            let mins = Self::BACKOFF_MS / 60_000;
            // Type only: a message could quote line text.
            Log::warn(&format!(
                "ner failed ({kind}): names and addresses not redacted, pattern redaction still runs; retry in {mins} min"
            ));
        }
    }
}
