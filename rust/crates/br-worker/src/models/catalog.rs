// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::{Path, PathBuf};

use crate::hosting::Log;
use crate::models::manifest::ModelManifest;

/// Folder names under `inference.modelDir` and names used in `hello.missing` and
/// `"Model not installed: <name>."` errors.
pub struct ModelCatalog;

impl ModelCatalog {
    pub const EMBEDDING_GEMMA: &'static str = "embeddinggemma-2";
    pub const SIGLIP: &'static str = "siglip2-base";
    pub const WHISPER: &'static str = "whisper-large-v3-turbo";
    pub const VAD: &'static str = "silero-vad";
    pub const NSFW: &'static str = "nsfw-classifier";
    pub const NSFW_DETECTOR: &'static str = "nsfw-detector";
    pub const OCR: &'static str = "ocr";
    pub const NER: &'static str = "ner";
    /// Name in `Model not installed` when `ocr.engine` is `windows` and no Windows OCR language is installed.
    pub const WINDOWS_OCR: &'static str = "windows-ocr";

    pub const VAD_FILE: &'static str = "silero_vad.onnx";

    pub fn dir(model_dir: &Path, name: &str) -> PathBuf {
        model_dir.join(name)
    }

    /// Manifest when present and every listed file exists with expected size, else `None`.
    pub fn load(model_dir: &Path, name: &str) -> Option<ModelManifest> {
        let m = match ModelManifest::try_load(&Self::dir(model_dir, name)) {
            Ok(Some(m)) => m,
            Ok(None) => return None,
            Err(e) => {
                Log::warn(&format!("model {name}: manifest unreadable ({e})"));
                return None;
            }
        };
        if let Err(missing) = m.files_present() {
            Log::warn(&format!("model {name}: file {missing} missing or wrong size"));
            return None;
        }
        Some(m)
    }

    /// OpenVINO GenAI Whisper folder: encoder, decoder, tokenizer and detokenizer IR.
    pub fn whisper_present(model_dir: &Path) -> bool {
        let d = Self::dir(model_dir, Self::WHISPER);
        d.join("openvino_encoder_model.xml").is_file()
            && d.join("openvino_decoder_model.xml").is_file()
            && d.join("openvino_tokenizer.xml").is_file()
            && d.join("openvino_detokenizer.xml").is_file()
    }

    pub fn vad_path(model_dir: &Path) -> Option<PathBuf> {
        let p = Self::dir(model_dir, Self::VAD).join(Self::VAD_FILE);
        if p.is_file() { Some(p) } else { None }
    }
}
