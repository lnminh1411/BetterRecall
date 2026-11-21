// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use serde::{Deserialize, Serialize, Serializer};

/// Serializes finite integer-valued floats without `.0` (matching System.Text.Json / ContractsJson).
mod dotnet_float {
    use super::*;

    const MAX_EXACT_F64_INT: f64 = 9_007_199_254_740_991.0;
    const MAX_EXACT_F32_INT: f32 = 16_777_215.0;

    pub fn ser_f64<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        let x = *v;
        if x.is_finite()
            && x.fract() == 0.0
            && x.abs() <= MAX_EXACT_F64_INT
            && !(x == 0.0 && x.is_sign_negative())
        {
            s.serialize_i64(x as i64)
        } else {
            s.serialize_f64(x)
        }
    }

    pub fn ser_opt_f64<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(x) => ser_f64(x, s),
            None => s.serialize_none(),
        }
    }

    pub fn ser_f32<S: Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
        let x = *v;
        if x.is_finite()
            && x.fract() == 0.0
            && x.abs() <= MAX_EXACT_F32_INT
            && !(x == 0.0 && x.is_sign_negative())
        {
            s.serialize_i64(x as i64)
        } else {
            s.serialize_f32(x)
        }
    }
}

/// Worker hello reply payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerHello {
    pub protocol: i32,
    pub device: String,
    pub ep: String,
    pub model: String,
    pub revision: String,
    pub dim: i32,
    pub asr: bool,
    pub vad: bool,
    pub missing: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsfw_device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_device: Option<String>,
}

/// Entity found by the NER model in an OCR line: UTF-16 Start and Length in the line text.
/// Label: person | address | location.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrEntity {
    pub start: i32,
    pub length: i32,
    pub label: String,
    #[serde(serialize_with = "dotnet_float::ser_f32")]
    pub score: f32,
}

/// One recognised text line in keyframe pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrLine {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(serialize_with = "dotnet_float::ser_f32")]
    pub confidence: f32,
    pub cuts: Vec<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entities: Option<Vec<OcrEntity>>,
}

/// OCR reply: lines top to bottom, the engine that read them ("onnx" | "windows") and the time taken.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrResult {
    pub lines: Vec<OcrLine>,
    pub engine: String,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub ms: f64,
    #[serde(default)]
    pub entities_missing: bool,
}

/// One timestamped segment in a speech transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSegment {
    pub start_ms: i32,
    pub end_ms: i32,
    pub text: String,
}

/// Speech transcription result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeResult {
    pub speech: bool,
    pub text: String,
    pub language: String,
    pub segments: Vec<TranscriptSegment>,
    pub ms: i64,
}

/// One device benchmark row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchDevice {
    pub device: String,
    pub ep: String,
    pub ok: bool,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub image_ms: f64,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub text_ms: f64,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub cosine: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_device: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "dotnet_float::ser_opt_f64"
    )]
    pub compile_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default)]
    pub adapter_auto: bool,
}

/// Per device and image-token budget, measured by the worker bench.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenBench {
    pub device: String,
    pub image_tokens: i32,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub image_ms: f64,
}

/// Result of the `bench` worker op.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchResult {
    pub chosen: String,
    pub devices: Vec<BenchDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Vec<TokenBench>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen_adapter: Option<String>,
}

/// Request header for the worker pipe. Unused fields stay None.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRequest {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stride: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "dotnet_float::ser_opt_f64"
    )]
    pub nsfw: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsfw_grid: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "dotnet_float::ser_opt_f64"
    )]
    pub nsfw_detector: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entities: Option<bool>,
}

/// Response header for the worker pipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerResponse {
    pub id: i64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "dotnet_float::ser_opt_f64"
    )]
    pub ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello: Option<WorkerHello>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<TranscribeResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<BenchResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr: Option<OcrResult>,
    // embed_image, embed_text, bench (sweep 4 A-M3): the embedder that served the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl WorkerResponse {
    /// Constructs a successful response header with optional fields cleared.
    pub fn ok(id: i64) -> Self {
        Self {
            id,
            ok: true,
            error: None,
            dim: None,
            ms: None,
            dropped: None,
            hello: None,
            transcript: None,
            bench: None,
            ocr: None,
            model: None,
            revision: None,
            device: None,
        }
    }

    /// Constructs a failure response header with an error string.
    pub fn err(id: i64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            error: Some(error.into()),
            dim: None,
            ms: None,
            dropped: None,
            hello: None,
            transcript: None,
            bench: None,
            ocr: None,
            model: None,
            revision: None,
            device: None,
        }
    }
}
