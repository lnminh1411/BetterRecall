// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::{Path, PathBuf};

use br_contracts::BenchResult;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hosting::Log;
use crate::inference::gpu_selection::GpuSelection;
use crate::inference::{DeviceInfo, DeviceKind};

/// Cache key of `cache\device.json` and `cache\compiled\`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCacheKey {
    pub runtime: String,
    pub runtime_version: String,
    pub npu_driver: String,
    pub model_hash: String,
    pub setting: String,
    pub image_tokens: i32,
}

impl DeviceCacheKey {
    pub fn id(&self) -> String {
        sha256_hex_16(&format!(
            "{}|{}|{}|{}|{}|{}",
            self.runtime,
            self.runtime_version,
            self.npu_driver,
            self.model_hash,
            self.setting,
            self.image_tokens
        ))
    }

    /// Compiled blobs depend on runtime, driver, and model only (not on the setting).
    pub fn compiled_id(&self) -> String {
        sha256_hex_16(&format!(
            "{}|{}|{}|{}",
            self.runtime, self.runtime_version, self.npu_driver, self.model_hash
        ))
    }

    /// Setting part of the key: `inference.device`, plus `|{gpuAdapter}` when non-auto.
    pub fn setting_of(device: &str, gpu_adapter: &str) -> String {
        if GpuSelection::is_auto(Some(gpu_adapter)) {
            device.to_string()
        } else {
            format!("{device}|{}", gpu_adapter.trim())
        }
    }
}

fn sha256_hex_16(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(16);
    for b in &d[..8] {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// `text_device_id` is set for a split choice (vision tower on `device_id`, text transformer on `text_device_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceChoice {
    pub device: String,
    pub device_id: String,
    pub ep: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCacheFile {
    #[serde(default)]
    pub key: Option<DeviceCacheKey>,
    #[serde(default)]
    pub chosen: Option<DeviceChoice>,
    #[serde(default)]
    pub bench: Option<BenchResult>,
    #[serde(default)]
    pub written_utc: String,
}

pub struct DeviceCache;

impl DeviceCache {
    /// Returns the cached file when it exists, parses, and its key matches `key`.
    pub fn read(path: &Path, key: &DeviceCacheKey) -> Option<DeviceCacheFile> {
        let bytes = std::fs::read(path).ok()?;
        let f: DeviceCacheFile = serde_json::from_slice(&bytes).ok()?;
        let f_key = f.key.as_ref()?;
        f.chosen.as_ref()?;
        if f_key == key { Some(f) } else { None }
    }

    pub fn write(path: &Path, f: &DeviceCacheFile) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        let bytes = serde_json::to_vec_pretty(f).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// `cache\compiled\<model>-<id>`. Deletes sibling directories for the same model with another id.
    pub fn compiled_dir(compiled_root: &Path, model_name: &str, id: &str) -> PathBuf {
        let dir = compiled_root.join(format!("{model_name}-{id}"));
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let prefix = format!("{model_name}-");
        if compiled_root.is_dir()
            && let Ok(entries) = std::fs::read_dir(compiled_root)
        {
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix)
                    && !name.eq_ignore_ascii_case(&dir_name)
                    && std::fs::remove_dir_all(&p).is_ok()
                {
                    Log::info(&format!("pruned stale compiled cache {name}"));
                }
            }
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            Log::warn(&format!("compiled cache: {}", e.kind()));
        }
        dir
    }
}

/// One device run in the benchmark.
#[derive(Debug, Clone)]
pub struct BenchOutcome {
    pub device: DeviceInfo,
    pub model: String,
    pub ok: bool,
    pub image_ms: f64,
    pub text_ms: f64,
    pub cosine: f64,
    pub error: Option<String>,
    pub compile_ms: f64,
    pub text_device: Option<DeviceInfo>,
    pub compile_cached: bool,
}

impl BenchOutcome {
    pub fn split(&self) -> bool {
        self.text_device
            .as_ref()
            .is_some_and(|td| td.id != self.device.id)
    }
}

/// Device policy of `docs/v1-design.md` section 4.
pub struct DevicePolicy;

impl DevicePolicy {
    pub const MIN_COSINE: f64 = 0.999;

    /// Devices to try, CPU always last.
    pub fn allowed(
        setting: &str,
        available: &[DeviceInfo],
        gpu_choice: Option<&DeviceInfo>,
    ) -> Vec<DeviceInfo> {
        let npu = available.iter().find(|d| d.kind == DeviceKind::Npu).cloned();
        let mut gpu = available.iter().find(|d| d.kind == DeviceKind::Gpu).cloned();
        if setting == "gpu"
            && let Some(choice) = gpu_choice
        {
            gpu = Some(choice.clone());
        }
        let cpu = available
            .iter()
            .find(|d| d.kind == DeviceKind::Cpu)
            .cloned()
            .expect("no CPU device");
        let mut list = Vec::new();
        match setting {
            "cpu" => {}
            "npu" => {
                if let Some(d) = npu {
                    list.push(d);
                }
            }
            "gpu" => {
                if let Some(d) = gpu {
                    list.push(d);
                }
            }
            _ => {
                if let Some(d) = npu {
                    list.push(d);
                } else if let Some(d) = gpu {
                    list.push(d);
                }
            }
        }
        list.push(cpu);
        list
    }

    pub fn whisper_allowed(setting: &str, available: &[DeviceInfo]) -> Vec<DeviceInfo> {
        let effective = if setting == "gpu" || setting == "cpu" {
            setting
        } else {
            "npu"
        };
        Self::allowed(effective, available, None)
    }

    /// Devices the bench op measures: allowed ones plus every other GPU when there is more than one GPU.
    pub fn bench_devices(allowed: &[DeviceInfo], gpus: &[DeviceInfo]) -> Vec<DeviceInfo> {
        if gpus.len() <= 1 {
            return allowed.to_vec();
        }
        let mut out = allowed.to_vec();
        for g in gpus {
            if !allowed.iter().any(|a| a.id == g.id) {
                out.push(g.clone());
            }
        }
        out
    }

    pub fn accept(cosine: f64, output_has_nan: bool) -> bool {
        !output_has_nan && !cosine.is_nan() && cosine >= Self::MIN_COSINE
    }

    /// Picks the best passing device outcome across primary and fallback models.
    pub fn choose<'a>(
        outcomes: &'a [BenchOutcome],
        primary: &str,
        fallback: &str,
    ) -> Option<&'a BenchOutcome> {
        let primary_failed_on_npu = outcomes
            .iter()
            .any(|o| o.model == primary && o.device.kind == DeviceKind::Npu && !o.ok)
            && !outcomes
                .iter()
                .any(|o| o.model == primary && o.device.kind == DeviceKind::Npu && o.ok);
        if primary_failed_on_npu
            && let Some(fb_npu) = outcomes
                .iter()
                .find(|o| o.model == fallback && o.device.kind == DeviceKind::Npu && o.ok)
        {
            return Some(fb_npu);
        }
        Self::best(outcomes.iter().filter(|o| o.model == primary && o.ok))
            .or_else(|| Self::best(outcomes.iter().filter(|o| o.model == fallback && o.ok)))
    }

    fn best<'a>(passing: impl Iterator<Item = &'a BenchOutcome>) -> Option<&'a BenchOutcome> {
        passing.min_by(|a, b| {
            let key = |o: &BenchOutcome| {
                (
                    if o.device.kind == DeviceKind::Cpu { 1u8 } else { 0u8 },
                    if o.split() { 1u8 } else { 0u8 },
                    o.image_ms + o.text_ms,
                )
            };
            let ka = key(a);
            let kb = key(b);
            (ka.0, ka.1)
                .cmp(&(kb.0, kb.1))
                .then_with(|| ka.2.total_cmp(&kb.2))
        })
    }
}
