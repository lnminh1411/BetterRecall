// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Typed worker operations on top of [`WorkerSupervisor`] (`WorkerClient.cs`).

use std::sync::Arc;

use br_contracts::{BenchResult, BinaryFrame, OcrResult, TranscribeResult, WorkerRequest};
use br_core::{CancelToken, EmbedError, QueryEmbedder};
use zeroize::Zeroize;

use crate::worker::worker_connection::{WorkerError, WorkerReply};
use crate::worker::worker_supervisor::WorkerSupervisor;

#[derive(Debug, Clone)]
pub struct ImageEmbedResult {
    pub vector: Option<Vec<f32>>,
    pub dropped_nsfw: bool,
    pub ms: f64,
    pub model: Option<String>,
    pub revision: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TextEmbedResult {
    pub vector: Vec<f32>,
    pub model: Option<String>,
    pub revision: Option<String>,
}

/// Typed worker operations on top of [`WorkerSupervisor`]. Starts the worker on demand.
pub struct WorkerClient {
    supervisor: Arc<WorkerSupervisor>,
}

impl WorkerClient {
    pub fn new(supervisor: Arc<WorkerSupervisor>) -> Self {
        Self { supervisor }
    }

    pub fn supervisor(&self) -> &Arc<WorkerSupervisor> {
        &self.supervisor
    }

    fn call(
        &self,
        req: WorkerRequest,
        payload: &[u8],
        ct: &CancelToken,
        urgent: bool,
    ) -> Result<WorkerReply, WorkerError> {
        let conn = self.supervisor.ensure(ct)?;
        let reply = conn.send(req, payload, ct, urgent)?;
        if !reply.header.ok {
            return Err(WorkerError::Reply(
                reply
                    .header
                    .error
                    .unwrap_or_else(|| "Internal error.".to_string()),
            ));
        }
        self.supervisor.mark_healthy();
        self.supervisor.note_embedder(&reply.header);
        Ok(reply)
    }

    pub fn prewarm(&self, ct: &CancelToken) -> Result<(), WorkerError> {
        let _ = self.supervisor.ensure(ct)?;
        Ok(())
    }

    pub fn embed_image(
        &self,
        image: &[u8],
        mime: &str,
        nsfw_threshold: Option<f64>,
        nsfw_grid: i32,
        nsfw_detector: Option<f64>,
        ct: &CancelToken,
    ) -> Result<ImageEmbedResult, WorkerError> {
        let format = match mime {
            "image/webp" => "webp",
            "image/jpeg" => "jpeg",
            "image/png" => "png",
            _ => {
                return Err(WorkerError::Reply("Unsupported image type.".to_string()));
            }
        };
        let req = WorkerRequest {
            id: 0,
            op: "embed_image".to_string(),
            format: Some(format.to_string()),
            nsfw: nsfw_threshold,
            nsfw_grid: Some(nsfw_grid),
            nsfw_detector,
            ..Default::default()
        };
        let reply = self.call(req, image, ct, false)?;
        if reply.header.dropped.as_deref() == Some("nsfw") {
            return Ok(ImageEmbedResult {
                vector: None,
                dropped_nsfw: true,
                ms: reply.header.ms.unwrap_or(0.0),
                model: None,
                revision: None,
            });
        }
        Ok(ImageEmbedResult {
            vector: Some(BinaryFrame::bytes_to_floats(&reply.payload)),
            dropped_nsfw: false,
            ms: reply.header.ms.unwrap_or(0.0),
            model: reply.header.model,
            revision: reply.header.revision,
        })
    }

    pub fn embed_bgra(
        &self,
        bgra: &[u8],
        dims: (i32, i32),
        nsfw_threshold: Option<f64>,
        nsfw_grid: i32,
        nsfw_detector: Option<f64>,
        ct: &CancelToken,
    ) -> Result<ImageEmbedResult, WorkerError> {
        let (width, height) = dims;
        let req = WorkerRequest {
            id: 0,
            op: "embed_image".to_string(),
            format: Some("bgra8".to_string()),
            width: Some(width),
            height: Some(height),
            stride: Some(width * 4),
            nsfw: nsfw_threshold,
            nsfw_grid: Some(nsfw_grid),
            nsfw_detector,
            ..Default::default()
        };
        let reply = self.call(req, bgra, ct, false)?;
        if reply.header.dropped.as_deref() == Some("nsfw") {
            return Ok(ImageEmbedResult {
                vector: None,
                dropped_nsfw: true,
                ms: reply.header.ms.unwrap_or(0.0),
                model: None,
                revision: None,
            });
        }
        Ok(ImageEmbedResult {
            vector: Some(BinaryFrame::bytes_to_floats(&reply.payload)),
            dropped_nsfw: false,
            ms: reply.header.ms.unwrap_or(0.0),
            model: reply.header.model,
            revision: reply.header.revision,
        })
    }

    pub fn embed_text(
        &self,
        text: &str,
        kind: &str,
        ct: &CancelToken,
    ) -> Result<Vec<f32>, WorkerError> {
        Ok(self.embed_text_labelled(text, kind, ct)?.vector)
    }

    pub fn embed_text_labelled(
        &self,
        text: &str,
        kind: &str,
        ct: &CancelToken,
    ) -> Result<TextEmbedResult, WorkerError> {
        let req = WorkerRequest {
            id: 0,
            op: "embed_text".to_string(),
            text: Some(text.to_string()),
            kind: Some(kind.to_string()),
            ..Default::default()
        };
        let reply = self.call(req, &[], ct, kind == "query")?;
        Ok(TextEmbedResult {
            vector: BinaryFrame::bytes_to_floats(&reply.payload),
            model: reply.header.model,
            revision: reply.header.revision,
        })
    }

    pub fn transcribe(
        &self,
        pcm_16k_mono: &[f32],
        source: &str,
        language: &str,
        ct: &CancelToken,
    ) -> Result<TranscribeResult, WorkerError> {
        let mut bytes = BinaryFrame::floats_to_bytes(pcm_16k_mono);
        let req = WorkerRequest {
            id: 0,
            op: "transcribe".to_string(),
            sample_rate: Some(16_000),
            channels: Some(1),
            format: Some("f32".to_string()),
            source: Some(source.to_string()),
            language: Some(language.to_string()),
            ..Default::default()
        };
        let res = self.call(req, &bytes, ct, false);
        bytes.zeroize();
        let reply = res?;
        reply
            .header
            .transcript
            .ok_or_else(|| WorkerError::Transport("Internal error.".to_string()))
    }

    pub fn ocr(
        &self,
        bgra: &[u8],
        width: i32,
        height: i32,
        stride: i32,
        entities: bool,
        ct: &CancelToken,
    ) -> Result<OcrResult, WorkerError> {
        let req = WorkerRequest {
            id: 0,
            op: "ocr".to_string(),
            format: Some("bgra8".to_string()),
            width: Some(width),
            height: Some(height),
            stride: Some(stride),
            entities: if entities { Some(true) } else { None },
            ..Default::default()
        };
        let reply = self.call(req, bgra, ct, false)?;
        reply
            .header
            .ocr
            .ok_or_else(|| WorkerError::Transport("Internal error.".to_string()))
    }

    /// Cancelling a bench aborts the worker without backoff so it does not block subsequent embeds.
    pub fn bench(&self, ct: &CancelToken) -> Result<BenchResult, WorkerError> {
        let conn = self.supervisor.ensure(ct)?;
        let bench_req = WorkerRequest {
            id: 0,
            op: "bench".to_string(),
            ..Default::default()
        };
        let reply = match conn.send(bench_req, &[], ct, false) {
            Ok(r) => r,
            Err(WorkerError::Cancelled) if ct.is_cancelled() => {
                self.supervisor.abort(&conn, "bench cancelled");
                return Err(WorkerError::Cancelled);
            }
            Err(e) => return Err(e),
        };
        if !reply.header.ok {
            return Err(WorkerError::Reply(
                reply
                    .header
                    .error
                    .unwrap_or_else(|| "Internal error.".to_string()),
            ));
        }
        self.supervisor.mark_healthy();
        self.supervisor.note_embedder(&reply.header);
        reply
            .header
            .bench
            .ok_or_else(|| WorkerError::Transport("Internal error.".to_string()))
    }
}

/// Implements `br_core::QueryEmbedder` by calling `WorkerClient::embed_text(..., "query")`.
pub struct QueryEmbedderImpl {
    worker: Arc<WorkerClient>,
}

impl QueryEmbedderImpl {
    pub fn new(worker: Arc<WorkerClient>) -> Self {
        Self { worker }
    }
}

impl QueryEmbedder for QueryEmbedderImpl {
    fn embed_query(&self, text: &str, ct: &CancelToken) -> Result<Vec<f32>, EmbedError> {
        self.worker
            .embed_text(text, "query", ct)
            .map_err(|e| match e {
                WorkerError::Cancelled => EmbedError::Cancelled,
                other => EmbedError::Failed(other.message().to_string()),
            })
    }

    fn warm(&self) -> bool {
        self.worker.supervisor().is_running()
    }
}
