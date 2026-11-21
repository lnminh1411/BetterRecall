// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use br_contracts::{CaptureKind, to_json_string, to_pretty_json_string, utc_timestamp};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---- Core-only wire shapes (src/BetterRecall.Core/CoreJson.cs) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PongData {
    pub pong: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelpData {
    pub verbs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribedStatus {
    pub subscribed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribedCaptures {
    pub subscribed: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureEvent {
    pub id: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: CaptureKind,
    pub app: String,
}

/// One `captures.json` row of an export. `file` is the image path inside the zip, `None` without an image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportEntry {
    pub id: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: CaptureKind,
    pub app: String,
    pub title: String,
    pub site: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<i32>,
}

/// JSON serializer matching `BetterRecall.Core.CoreJson` and `PrettySettingsJson`.
pub struct CoreJson;

impl CoreJson {
    /// Compact JSON matching `CoreJson.Default` (camelCase, nulls omitted, UTC `Z` timestamps, .NET escaping).
    pub fn to_string<T: Serialize>(value: &T) -> serde_json::Result<String> {
        to_json_string(value)
    }

    /// Indented JSON matching `PrettySettingsJson.Default` (2-space indent, `\r\n`, camelCase, .NET escaping).
    pub fn to_pretty_string<T: Serialize>(value: &T) -> serde_json::Result<String> {
        to_pretty_json_string(value)
    }
}
