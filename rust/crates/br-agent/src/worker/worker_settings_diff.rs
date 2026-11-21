// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Worker-side settings diff and partial JSON extraction (`WorkerSettingsDiff.cs`).

use br_contracts::Settings;
use serde_json::{Map, Value};

pub struct WorkerSettingsDiff;

impl WorkerSettingsDiff {
    /// Pinned list of keys handed to the worker over `--settings-stdin` and watched for restarts.
    pub const KEY_NAMES: &'static [&'static str] = &[
        "inference.device",
        "inference.gpuAdapter",
        "inference.modelDir",
        "inference.imageTokens",
        "inference.idleExitS",
        "inference.fake",
        "index.dim",
        "audio.language",
        "governance.ecoQos",
        "security.memoryIsolation",
        "log.level",
        "privacy.nsfwFilter",
        "privacy.nsfwDetector",
        "privacy.nsfwDevice",
        "privacy.nsfwGpuAdapter",
        "ocr.enabled",
        "ocr.engine",
        "ocr.languages",
        "ocr.device",
    ];

    /// The first worker-side key that differs, or `None` when the running worker is still current.
    pub fn changed(a: &Settings, b: &Settings) -> Option<&'static str> {
        Self::KEY_NAMES.iter().find(|&&key| get_key(a, key) != get_key(b, key)).copied().map(|v| v as _)
    }

    /// The worker keys of `s` as a partial `settings.json` object for `--settings-stdin`.
    pub fn json(s: &Settings) -> String {
        let full = serde_json::to_value(s).unwrap_or(Value::Object(Map::new()));
        let mut out = Map::new();
        for &key in Self::KEY_NAMES {
            let Some((section, name)) = key.split_once('.') else {
                continue;
            };
            let Some(val) = full.get(section).and_then(|sec| sec.get(name)) else {
                continue;
            };
            let dst = out
                .entry(section.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(map) = dst {
                map.insert(name.to_string(), val.clone());
            }
        }
        serde_json::to_string(&Value::Object(out)).unwrap_or_else(|_| "{}".to_string())
    }
}

fn get_key(s: &Settings, key: &str) -> String {
    match key {
        "inference.device" => s.inference.device.clone(),
        "inference.gpuAdapter" => s.inference.gpu_adapter.clone(),
        "inference.modelDir" => s.inference.model_dir.clone(),
        "inference.imageTokens" => s.inference.image_tokens.to_string(),
        "inference.idleExitS" => s.inference.idle_exit_s.to_string(),
        "inference.fake" => s.inference.fake.to_string(),
        "index.dim" => s.index.dim.to_string(),
        "audio.language" => s.audio.language.clone(),
        "governance.ecoQos" => s.governance.eco_qos.to_string(),
        "security.memoryIsolation" => s.security.memory_isolation.to_string(),
        "log.level" => s.log.level.clone(),
        "privacy.nsfwFilter" => s.privacy.nsfw_filter.to_string(),
        "privacy.nsfwDetector" => s.privacy.nsfw_detector.to_string(),
        "privacy.nsfwDevice" => s.privacy.nsfw_device.clone(),
        "privacy.nsfwGpuAdapter" => s.privacy.nsfw_gpu_adapter.clone(),
        "ocr.enabled" => s.ocr.enabled.to_string(),
        "ocr.engine" => s.ocr.engine.clone(),
        "ocr.languages" => s.ocr.languages.clone(),
        "ocr.device" => s.ocr.device.clone(),
        _ => String::new(),
    }
}
