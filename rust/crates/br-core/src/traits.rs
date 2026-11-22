// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Host-side traits implemented by the agent (and test fakes) and consumed by `br-core`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use br_contracts::{
    AgentStatus, BenchReport, BenchResult, CaptureMeta, ModelsList, ModelsProgress, ReplayClip,
    ThumbData,
};
use thiserror::Error;

use crate::cancel::{CancelToken, ListenerGuard};

/// Errors returned by [`AgentControl`] operations.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("Not found.")]
    NotFound,
    #[error("Store is locked.")]
    Locked,
    #[error("Operation cancelled.")]
    Cancelled,
    #[error("{0}")]
    InvalidOperation(String),
    #[error("Not supported.")]
    NotSupported,
    #[error("{0}")]
    Other(String),
}

/// Errors returned by [`QueryEmbedder::embed_query`].
#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("Operation cancelled.")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

/// Agent state the command processor reads and changes (`IAgentControl`).
pub trait AgentControl: Send + Sync {
    fn get_status(&self) -> AgentStatus;
    fn pause(&self, duration: Option<Duration>, reason: &str);
    fn resume(&self, reason: &str);
    fn is_unlocked(&self) -> bool;
    fn unlock(&self, ct: &CancelToken) -> bool;
    fn lock(&self);
    fn consent(&self);
    fn save_replay(&self, ct: &CancelToken) -> Result<ReplayClip, AgentError>;
    fn list_replays(&self) -> Vec<ReplayClip>;
    fn export_replay(&self, id: &str, path: &Path, ct: &CancelToken) -> Result<(), AgentError>;
    fn delete_replay(&self, id: &str) -> Result<(), AgentError>;
    fn panic(&self);
    fn bench(&self, ct: &CancelToken) -> Result<BenchResult, AgentError>;

    fn bench_full(&self, _ct: &CancelToken) -> Result<BenchReport, AgentError> {
        Err(AgentError::NotSupported)
    }

    fn models_list(&self) -> Result<ModelsList, AgentError> {
        Err(AgentError::NotSupported)
    }

    fn models_download(
        &self,
        _target: &str,
        _emit: &mut dyn FnMut(ModelsProgress),
        _ct: &CancelToken,
    ) -> Result<(), AgentError> {
        Err(AgentError::NotSupported)
    }

    fn open_read_gate(&self) {}

    fn on_panicked(&self, _cb: Arc<dyn Fn() + Send + Sync>) -> ListenerGuard {
        ListenerGuard::noop()
    }

    fn on_status_changed(
        &self,
        _cb: Arc<dyn Fn(&AgentStatus) + Send + Sync>,
    ) -> ListenerGuard {
        ListenerGuard::noop()
    }

    fn on_capture_stored(
        &self,
        _cb: Arc<dyn Fn(&CaptureMeta) + Send + Sync>,
    ) -> ListenerGuard {
        ListenerGuard::noop()
    }
}

/// Embeds a search query with the text tower (`IQueryEmbedder`).
pub trait QueryEmbedder: Send + Sync {
    fn embed_query(&self, text: &str, ct: &CancelToken) -> Result<Vec<f32>, EmbedError>;

    /// `false` while the worker is not running (cold start takes 10-15 s).
    fn warm(&self) -> bool {
        true
    }
}

/// On-demand preview from the stored keyframe, cached in RAM only (`IThumbnailer`).
pub trait Thumbnailer: Send + Sync {
    /// `width` 64..1280. WebP. `None` when the row or keyframe is gone.
    fn get_thumb(&self, id: &str, width: i32, ct: &CancelToken) -> Option<ThumbData>;
    fn clear_cache(&self);

    /// Stored keyframe to a format the reader can decode (`"ui"`, `"jpeg"`).
    fn transcode(&self, keyframe: ThumbData, _target: &str) -> ThumbData {
        keyframe
    }
}
