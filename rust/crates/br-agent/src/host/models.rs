// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Model catalog inspection and verified Hugging Face downloads for the Models page (R7, stage I4).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use br_contracts::{ModelComponent, ModelsList, ModelsProgress};
use br_core::{AgentError, CancelToken, CommandProcessor};
use br_fetch::{
    Catalog, Component, ComponentState, Downloader, FetchError, FetchProgress, InstallOptions,
    ModelSet, available_disk_space, install_components, installed_state,
};

use crate::util::AgentLog;

/// Display titles and one-line descriptions for every model component in the pinned catalog.
const COMPONENT_META: [(&str, &str, &str); 8] = [
    ("embeddinggemma-2", "EmbeddingGemma 2", "Search"),
    ("nsfw-classifier", "NSFW classifier", "Privacy filter"),
    ("nsfw-detector", "NSFW detector", "Privacy filter"),
    ("ocr", "OCR", "On-screen text"),
    ("ner", "NER", "Name and address redaction"),
    ("siglip2-base", "SigLIP 2", "NPU image search fallback"),
    ("whisper-large-v3-turbo", "Whisper", "Audio transcription"),
    ("silero-vad", "Silero VAD", "Speech detection for transcription"),
];

const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Returns `(title, description)` for a catalog component id.
pub fn component_meta(id: &str) -> (&str, &str) {
    for &(cid, title, desc) in &COMPONENT_META {
        if cid == id {
            return (title, desc);
        }
    }
    (id, "")
}

fn state_str(state: ComponentState) -> &'static str {
    match state {
        ComponentState::Missing => "missing",
        ComponentState::Partial => "partial",
        ComponentState::Complete => "complete",
    }
}

/// Builds the `ModelsList` snapshot for `models_dir` against `catalog`.
pub fn list_models(models_dir: &Path, catalog: &Catalog, downloading: bool) -> ModelsList {
    let states: HashMap<String, ComponentState> = installed_state(models_dir, catalog)
        .into_iter()
        .collect();
    let components = catalog
        .components
        .iter()
        .map(|c| {
            let (title, desc) = component_meta(&c.id);
            let state = states
                .get(&c.id)
                .copied()
                .unwrap_or(ComponentState::Missing);
            ModelComponent {
                id: c.id.clone(),
                title: title.to_string(),
                description: desc.to_string(),
                sets: c.sets.clone(),
                bytes: c.bytes,
                state: state_str(state).to_string(),
            }
        })
        .collect();

    ModelsList {
        models_dir: models_dir.to_string_lossy().into_owned(),
        repo: catalog.repo.clone(),
        revision: catalog.revision.clone(),
        components,
        full_bytes: catalog.bytes_for(ModelSet::Full),
        minimal_bytes: catalog.bytes_for(ModelSet::Minimal),
        free_bytes: available_disk_space(models_dir).unwrap_or(0),
        downloading,
    }
}

/// Resolves `"full"`, `"minimal"`, or a single component id against `catalog`.
fn resolve_target<'a>(catalog: &'a Catalog, target: &str) -> Option<Vec<&'a Component>> {
    if target.eq_ignore_ascii_case("full") {
        return Some(catalog.components_for(ModelSet::Full));
    }
    if target.eq_ignore_ascii_case("minimal") {
        return Some(catalog.components_for(ModelSet::Minimal));
    }
    catalog.component(target).map(|c| vec![c])
}

struct DownloadFlagGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for DownloadFlagGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

struct ThrottledSink<'a> {
    emit: RefCell<&'a mut dyn FnMut(ModelsProgress)>,
    done: RefCell<u64>,
    total: RefCell<u64>,
    file: RefCell<Option<String>>,
    last_emit: RefCell<Option<Instant>>,
}

impl<'a> ThrottledSink<'a> {
    fn new(emit: &'a mut dyn FnMut(ModelsProgress)) -> Self {
        Self {
            emit: RefCell::new(emit),
            done: RefCell::new(0),
            total: RefCell::new(0),
            file: RefCell::new(None),
            last_emit: RefCell::new(None),
        }
    }

    fn emit_now(&self, p: ModelsProgress) {
        (self.emit.borrow_mut())(p);
        *self.last_emit.borrow_mut() = Some(Instant::now());
    }

    fn snapshot(&self) -> (u64, u64, Option<String>) {
        (
            *self.done.borrow(),
            *self.total.borrow(),
            self.file.borrow().clone(),
        )
    }
}

impl FetchProgress for ThrottledSink<'_> {
    fn total(&self, bytes: u64) {
        *self.total.borrow_mut() = bytes;
        if bytes > 0 {
            self.emit_now(ModelsProgress {
                phase: "downloading".to_string(),
                done_bytes: 0,
                total_bytes: bytes,
                file: self.file.borrow().clone(),
                error: None,
            });
        }
    }

    fn file(&self, rel_path: &str) {
        *self.file.borrow_mut() = Some(rel_path.to_string());
    }

    fn bytes(&self, delta: u64) {
        let done = {
            let mut d = self.done.borrow_mut();
            *d = d.saturating_add(delta);
            *d
        };
        let due = self
            .last_emit
            .borrow()
            .is_none_or(|t| t.elapsed() >= PROGRESS_MIN_INTERVAL);
        if due {
            let total = *self.total.borrow();
            let file = self.file.borrow().clone();
            self.emit_now(ModelsProgress {
                phase: "downloading".to_string(),
                done_bytes: done,
                total_bytes: total,
                file,
                error: None,
            });
        }
    }
}

fn sanitize_error(err: &FetchError) -> String {
    match err {
        FetchError::Http { status, .. } => format!("Download failed (HTTP {status})."),
        FetchError::Verify { path } => {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            format!("File verification failed for {name}.")
        }
        FetchError::NotEnoughSpace { .. } => "Not enough free disk space.".to_string(),
        FetchError::Offline => "No internet connection.".to_string(),
        FetchError::Cancelled => "Download cancelled.".to_string(),
        FetchError::Io { context, .. } => format!("Disk error while {context}."),
        FetchError::Catalog(msg) => format!("Invalid model catalog: {msg}"),
        FetchError::InUse => "Another download is writing this file.".to_string(),
    }
}

/// Inputs for [`download_models`].
pub struct DownloadContext<'a> {
    pub models_dir: &'a Path,
    pub catalog: &'a Catalog,
    pub fast_retries: bool,
    pub downloading_flag: &'a AtomicBool,
}

/// Runs a single model download against `ctx.catalog` into `ctx.models_dir`, streaming throttled
/// [`ModelsProgress`] updates. Calls `on_fetched` when the download finishes and at least one
/// file was fetched.
pub fn download_models(
    ctx: &DownloadContext<'_>,
    target: &str,
    emit: &mut dyn FnMut(ModelsProgress),
    ct: &CancelToken,
    on_fetched: impl FnOnce(),
) -> Result<(), AgentError> {
    let Some(components) = resolve_target(ctx.catalog, target) else {
        return Err(AgentError::InvalidOperation(
            CommandProcessor::USAGE_MODELS.to_string(),
        ));
    };

    if ctx
        .downloading_flag
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(AgentError::InvalidOperation(
            CommandProcessor::ERR_MODELS_BUSY.to_string(),
        ));
    }
    let _guard = DownloadFlagGuard {
        flag: ctx.downloading_flag,
    };

    AgentLog::info(&format!("models download start: target={target}"));

    let sink = ThrottledSink::new(emit);
    sink.emit_now(ModelsProgress {
        phase: "checking".to_string(),
        done_bytes: 0,
        total_bytes: 0,
        file: None,
        error: None,
    });

    let mut opts = InstallOptions::for_catalog(ctx.catalog);
    if ctx.fast_retries {
        opts = opts.with_downloader(
            Downloader::new().with_retry_delays(vec![Duration::from_millis(10)]),
        );
    }

    let cancel = || ct.is_cancelled();
    match install_components(ctx.models_dir, &components, &opts, &sink, &cancel) {
        Ok(report) => {
            AgentLog::info(&format!(
                "models download done: target={target}, fetched={} files, skipped={} files, bytes={}",
                report.downloaded_files, report.skipped_files, report.bytes_downloaded
            ));
            if report.downloaded_files > 0 {
                on_fetched();
            }
            let (_, total, _) = sink.snapshot();
            sink.emit_now(ModelsProgress {
                phase: "done".to_string(),
                done_bytes: total,
                total_bytes: total,
                file: None,
                error: None,
            });
        }
        Err(FetchError::Cancelled) => {
            AgentLog::info(&format!("models download cancelled: target={target}"));
            let (done, total, file) = sink.snapshot();
            sink.emit_now(ModelsProgress {
                phase: "cancelled".to_string(),
                done_bytes: done,
                total_bytes: total,
                file,
                error: None,
            });
        }
        Err(FetchError::Offline) => {
            AgentLog::warn(&format!("models download offline: target={target}"));
            let (done, total, file) = sink.snapshot();
            sink.emit_now(ModelsProgress {
                phase: "offline".to_string(),
                done_bytes: done,
                total_bytes: total,
                file,
                error: Some("No internet connection.".to_string()),
            });
        }
        Err(e) => {
            let msg = sanitize_error(&e);
            AgentLog::warn(&format!(
                "models download failed: target={target}, error={msg}"
            ));
            let (done, total, file) = sink.snapshot();
            sink.emit_now(ModelsProgress {
                phase: "failed".to_string(),
                done_bytes: done,
                total_bytes: total,
                file,
                error: Some(msg),
            });
        }
    }
    Ok(())
}
