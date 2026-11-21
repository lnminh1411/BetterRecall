// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use serde::{Deserialize, Deserializer, Serialize};

fn deser_string_default_on_null<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

fn deser_or_default<'de, T, D>(d: D) -> Result<T, D::Error>
where
    T: Deserialize<'de> + Default,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Worker-relevant sections of `settings.json`. Unknown sections and keys are ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    #[serde(deserialize_with = "deser_or_default")]
    pub index: IndexSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub audio: AudioSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub privacy: PrivacySettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub inference: InferenceSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub governance: GovernanceSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub security: SecuritySettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub log: LogSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub ocr: OcrSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OcrSettings {
    pub enabled: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub engine: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub languages: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub device: String,
}

impl Default for OcrSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: "onnx".to_string(),
            languages: "auto".to_string(),
            device: "auto".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexSettings {
    pub dim: i32,
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self { dim: 768 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioSettings {
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub language: String,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            language: "auto".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrivacySettings {
    pub nsfw_filter: bool,
    pub nsfw_detector: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub nsfw_device: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub nsfw_gpu_adapter: String,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self {
            nsfw_filter: true,
            nsfw_detector: true,
            nsfw_device: "auto".to_string(),
            nsfw_gpu_adapter: "auto".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InferenceSettings {
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub device: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub gpu_adapter: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub model_dir: String,
    pub image_tokens: i32,
    pub idle_exit_s: i32,
    pub fake: bool,
}

impl Default for InferenceSettings {
    fn default() -> Self {
        Self {
            device: "auto".to_string(),
            gpu_adapter: "auto".to_string(),
            model_dir: String::new(),
            image_tokens: 140,
            idle_exit_s: 120,
            fake: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GovernanceSettings {
    pub eco_qos: bool,
}

impl Default for GovernanceSettings {
    fn default() -> Self {
        Self { eco_qos: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecuritySettings {
    pub memory_isolation: bool,
}

impl Default for SecuritySettings {
    fn default() -> Self {
        Self {
            memory_isolation: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogSettings {
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub level: String,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}
