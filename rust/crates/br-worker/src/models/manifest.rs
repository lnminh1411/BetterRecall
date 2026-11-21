// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hosting::settings::strip_jsonc;

/// `models\<name>\manifest.json`, written by `Scripts\models\*.py` next to the exported graphs.
/// Its SHA-256 is the "model hash" in the device and compiled-model cache keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelManifest {
    pub name: String,
    pub id: String,
    pub revision: String,
    pub licence: String,
    pub source: String,
    pub dim: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokenizer: Option<TokenizerSpec>,
    /// Token embedding table kept outside the graphs (EmbeddingGemma 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embeddings: Option<EmbeddingTableSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptSpec>,
    pub text: Vec<TextGraph>,
    pub vision: Vec<VisionGraph>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<ClassifierSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detector: Option<DetectorSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr: Option<OcrSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ner: Option<NerSpec>,
    pub files: Vec<ManifestFile>,
    /// OpenVINO IR file -> ONNX graph of the same model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onnx: Option<HashMap<String, String>>,
    /// Set by the loader: SHA-256 of `manifest.json` bytes, lower-case hex.
    #[serde(skip)]
    pub hash: String,
    #[serde(skip)]
    pub dir: PathBuf,
}

impl ModelManifest {
    pub fn path_of(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    /// The file a backend loads for a graph: its ONNX twin on ONNX Runtime when the manifest has one.
    pub fn file_for<'a>(&'a self, file: &'a str, onnx_runtime: bool) -> &'a str {
        if onnx_runtime
            && let Some(ref map) = self.onnx
            && let Some(twin) = map.get(file)
        {
            return twin.as_str();
        }
        file
    }

    pub fn try_load(dir: &Path) -> Result<Option<Self>, io::Error> {
        let path = dir.join("manifest.json");
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let text = String::from_utf8(bytes.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let clean = strip_jsonc(&text);
        let mut m: Self = serde_json::from_str(&clean)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let digest = Sha256::digest(&bytes);
        m.hash = digest.iter().map(|b| format!("{b:02x}")).collect();
        m.dir = dir.to_path_buf();
        Ok(Some(m))
    }

    /// Every file the manifest lists exists with the listed size.
    pub fn files_present(&self) -> Result<(), String> {
        for f in &self.files {
            let p = self.path_of(&f.file);
            match fs::metadata(&p) {
                Ok(meta) if meta.is_file() => {
                    if f.size > 0 && meta.len() != f.size as u64 {
                        return Err(f.file.clone());
                    }
                }
                _ => return Err(f.file.clone()),
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ManifestFile {
    pub file: String,
    pub size: i64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenizerSpec {
    pub file: String,
    pub add_bos: bool,
    pub add_eos: bool,
    pub bos_id: i64,
    pub eos_id: i64,
    pub pad_id: i64,
    pub lowercase: bool,
    /// SigLIP-style: always pad to the graph length (attention mask absent).
    pub pad_to_max: bool,
}

impl Default for TokenizerSpec {
    fn default() -> Self {
        Self {
            file: "tokenizer.model".to_string(),
            add_bos: true,
            add_eos: true,
            bos_id: 2,
            eos_id: 1,
            pad_id: 0,
            lowercase: false,
            pad_to_max: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptSpec {
    pub query: String,
    pub document: String,
}

impl Default for PromptSpec {
    fn default() -> Self {
        Self {
            query: "{text}".to_string(),
            document: "{text}".to_string(),
        }
    }
}

/// Text graph with one static sequence length.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct TextGraph {
    pub file: String,
    pub seq: i32,
    pub input_ids: String,
    #[serde(
        default = "default_attention_mask",
        skip_serializing_if = "Option::is_none"
    )]
    pub attention_mask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs_embeds: Option<String>,
    pub output: String,
}

impl Default for TextGraph {
    fn default() -> Self {
        Self {
            file: String::new(),
            seq: 0,
            input_ids: "input_ids".to_string(),
            attention_mask: default_attention_mask(),
            inputs_embeds: None,
            output: "embedding".to_string(),
        }
    }
}

fn default_attention_mask() -> Option<String> {
    Some("attention_mask".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct EmbeddingTableSpec {
    pub file: String,
    pub rows: i32,
    pub dim: i32,
    // C# `DType` serializes under CamelCase as "dType".
    #[serde(rename = "dType", default = "default_bf16")]
    pub dtype: String,
    pub scale: f32,
}

impl Default for EmbeddingTableSpec {
    fn default() -> Self {
        Self {
            file: "embed_tokens.bf16".to_string(),
            rows: 0,
            dim: 0,
            dtype: default_bf16(),
            scale: 1.0,
        }
    }
}

fn default_bf16() -> String {
    "bf16".to_string()
}

/// Vision graph for one token budget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct VisionGraph {
    pub tokens: i32,
    pub soft_tokens: i32,
    pub file: String,
    pub input: String,
    pub output: String,
    pub height: i32,
    pub width: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub positions_input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_input: Option<String>,
    pub patch_size: i32,
    pub pool: i32,
    pub max_patches: i32,
    pub preprocess: ImagePreprocessSpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fusion: Option<FusionGraph>,
}

impl Default for VisionGraph {
    fn default() -> Self {
        Self {
            tokens: 0,
            soft_tokens: 0,
            file: String::new(),
            input: "pixel_values".to_string(),
            output: "image_embeds".to_string(),
            height: 0,
            width: 0,
            positions_input: None,
            pool_input: None,
            patch_size: 16,
            pool: 3,
            max_patches: 0,
            preprocess: ImagePreprocessSpec::default(),
            fusion: None,
        }
    }
}

impl VisionGraph {
    pub fn patch_budget(&self) -> i32 {
        if self.max_patches > 0 {
            self.max_patches
        } else {
            self.tokens * self.pool * self.pool
        }
    }
}

/// Second pass of an image: the text transformer over the image soft tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct FusionGraph {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs_embeds: Option<String>,
    pub prefix_ids: Vec<i64>,
    pub suffix_ids: Vec<i64>,
    pub image_embeds_input: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_ids_input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attention_mask_input: Option<String>,
    pub input_ids: Vec<i64>,
    pub output: String,
}

impl Default for FusionGraph {
    fn default() -> Self {
        Self {
            file: String::new(),
            inputs_embeds: None,
            prefix_ids: Vec::new(),
            suffix_ids: Vec::new(),
            image_embeds_input: "image_embeds".to_string(),
            input_ids_input: None,
            attention_mask_input: None,
            input_ids: Vec::new(),
            output: "embedding".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ImagePreprocessSpec {
    pub resample: String,
    pub rescale: f32,
    pub mean: Vec<f32>,
    pub std: Vec<f32>,
    pub fit: String,
    pub pad_value: f32,
}

impl Default for ImagePreprocessSpec {
    fn default() -> Self {
        Self {
            resample: "bicubic".to_string(),
            rescale: 1.0 / 255.0,
            mean: vec![0.5, 0.5, 0.5],
            std: vec![0.5, 0.5, 0.5],
            fit: "stretch".to_string(),
            pad_value: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ClassifierSpec {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accelerator_file: Option<String>,
    pub input: String,
    pub output: String,
    pub height: i32,
    pub width: i32,
    pub preprocess: ImagePreprocessSpec,
    pub labels: Vec<String>,
    pub block_labels: Vec<String>,
}

impl Default for ClassifierSpec {
    fn default() -> Self {
        Self {
            file: String::new(),
            accelerator_file: None,
            input: "pixel_values".to_string(),
            output: "logits".to_string(),
            height: 224,
            width: 224,
            preprocess: ImagePreprocessSpec {
                resample: "bilinear".to_string(),
                ..ImagePreprocessSpec::default()
            },
            labels: Vec::new(),
            block_labels: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct DetectorSpec {
    pub file: String,
    pub input: String,
    pub output: String,
    pub size: i32,
    pub labels: Vec<String>,
    pub block_labels: Vec<String>,
}

impl Default for DetectorSpec {
    fn default() -> Self {
        Self {
            file: String::new(),
            input: "images".to_string(),
            output: "output0".to_string(),
            size: 640,
            labels: Vec::new(),
            block_labels: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct OcrSpec {
    pub det: OcrDetSpec,
    pub rec: Vec<OcrRecSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct OcrDetSpec {
    pub file: String,
    pub input: String,
    pub thresh: f32,
    pub box_thresh: f32,
    pub unclip: f32,
    pub mean: Vec<f32>,
    pub std: Vec<f32>,
    pub max_side: i32,
}

impl Default for OcrDetSpec {
    fn default() -> Self {
        Self {
            file: "det.onnx".to_string(),
            input: "x".to_string(),
            thresh: 0.2,
            box_thresh: 0.45,
            unclip: 1.4,
            mean: vec![0.485, 0.456, 0.406],
            std: vec![0.229, 0.224, 0.225],
            max_side: 1920,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct OcrRecSpec {
    pub name: String,
    pub file: String,
    pub dict: String,
    pub input: String,
    pub height: i32,
    pub languages: Vec<String>,
    pub classes: i32,
}

impl Default for OcrRecSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            file: String::new(),
            dict: String::new(),
            input: "x".to_string(),
            height: 48,
            languages: Vec::new(),
            classes: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct NerSpec {
    pub file: String,
    pub tokenizer: String,
    pub input_ids: String,
    pub attention_mask: String,
    pub output: String,
    pub max_tokens: i32,
    pub labels: Vec<String>,
    pub entities: HashMap<String, String>,
}

impl Default for NerSpec {
    fn default() -> Self {
        Self {
            file: "model.onnx".to_string(),
            tokenizer: "tokenizer.json".to_string(),
            input_ids: "input_ids".to_string(),
            attention_mask: "attention_mask".to_string(),
            output: "logits".to_string(),
            max_tokens: 512,
            labels: Vec::new(),
            entities: HashMap::new(),
        }
    }
}
