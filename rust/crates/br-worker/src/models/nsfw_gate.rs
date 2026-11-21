// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::PathBuf;
use std::time::Instant;

use crate::hosting::Log;
use crate::inference::nsfw_plan::NsfwDevicePlan;
use crate::inference::{DeviceInfo, IInferenceBackend};
use crate::models::manifest::ModelManifest;
use crate::models::nsfw_classifier::NsfwClassifier;
use crate::models::nsfw_detector::NsfwDetector;
use crate::protocol::{WorkerError, WorkerErrors};

/// One NSFW gate candidate: a device, its backend, and optional compiled-model cache dir (OpenVINO only).
pub struct NsfwCandidate<'a> {
    pub device: DeviceInfo,
    pub backend: &'a dyn IInferenceBackend,
    pub cache_dir: Option<PathBuf>,
}

/// The NSFW classifier and (optionally) detector, both on the same device.
pub struct NsfwGate {
    pub device: DeviceInfo,
    pub device_label: String,
    pub classifier: NsfwClassifier,
    pub detector: Option<NsfwDetector>,
}

impl NsfwGate {
    /// Tries candidates in order: loads classifier and optional detector and runs warm-up inference on each.
    /// Logs failures and moves to the next candidate.
    pub fn load(
        candidates: &[NsfwCandidate<'_>],
        classifier: &ModelManifest,
        detector: Option<&ModelManifest>,
    ) -> Result<Self, WorkerError> {
        let mut last: Option<String> = None;
        for c in candidates {
            let started = Instant::now();
            let attempt = (|| -> Result<(NsfwClassifier, Option<NsfwDetector>), WorkerError> {
                let cls = NsfwClassifier::new(
                    c.backend,
                    &c.device,
                    classifier.clone(),
                    c.cache_dir.as_deref(),
                )?;
                cls.warm_up()?;
                let det = match detector {
                    Some(dm) => {
                        let d = NsfwDetector::new(
                            c.backend,
                            &c.device,
                            dm.clone(),
                            c.cache_dir.as_deref(),
                        )?;
                        d.warm_up()?;
                        Some(d)
                    }
                    None => None,
                };
                Ok((cls, det))
            })();

            match attempt {
                Ok((cls, det)) => {
                    let device_label = NsfwDevicePlan::label(&c.device);
                    let has_det = det.is_some();
                    let ms = started.elapsed().as_millis();
                    Log::info(&format!(
                        "nsfw gate on {device_label} ({} {}): classifier{}, load {ms} ms",
                        c.device.ep,
                        c.device.id,
                        if has_det { " + detector" } else { "" },
                    ));
                    return Ok(Self {
                        device: c.device.clone(),
                        device_label,
                        classifier: cls,
                        detector: det,
                    });
                }
                Err(e) => {
                    let msg = match &e {
                        WorkerError::DeviceFailed(detail) => WorkerErrors::clean(detail),
                        other => format!("{} {other}", c.device.label()),
                    };
                    let label = NsfwDevicePlan::label(&c.device);
                    Log::warn(&format!(
                        "nsfw gate on {label} ({} {}) failed: {msg}",
                        c.device.ep, c.device.id
                    ));
                    last = Some(msg);
                }
            }
        }
        Err(WorkerError::DeviceFailed(match last {
            None => "nsfw gate has no device".to_string(),
            Some(l) => format!("nsfw gate failed on every device ({l})"),
        }))
    }
}
