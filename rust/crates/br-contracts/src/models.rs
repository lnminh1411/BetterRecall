// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::json::{dotnet_base64, dotnet_float, utc_timestamp};

// ---- UI, public IPC, and Core domain models (src/BetterRecall.Contracts/Models.cs) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureKind {
    Screen,
    Audio,
    Replay,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    #[default]
    Hybrid,
    Text,
    Vector,
}

/// Row metadata. Never carries text or pixels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureMeta {
    pub id: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: CaptureKind,
    pub app: String,
    pub title: String,
    pub monitor: i32,
    pub duration_ms: i32,
    pub has_thumb: bool,
    pub embedded: bool,
    #[serde(default)]
    pub site: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_offset_ms: Option<i32>,
}

/// Screen capture handed from the agent to the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewScreenCapture {
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub app: String,
    pub exe_path: String,
    pub title: String,
    pub monitor: i32,
    pub width: i32,
    pub height: i32,
    #[serde(rename = "dHash")]
    pub d_hash: u64,
    #[serde(with = "dotnet_base64")]
    pub image: Vec<u8>,
    pub image_mime: String,
    pub keep_keyframe: bool,
    #[serde(default)]
    pub site: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "dotnet_float::ser_opt_f64"
    )]
    pub change: Option<f64>,
    #[serde(default)]
    pub switch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Store files on disk and the free pages inside them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageUse {
    pub file_bytes: i64,
    pub free_bytes: i64,
}

impl StorageUse {
    pub fn live_bytes(&self) -> i64 {
        (self.file_bytes - self.free_bytes).max(0)
    }
}

/// `export`: time window and optional app, written as a zip of decrypted images plus `captures.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    #[serde(with = "utc_timestamp")]
    pub from: DateTime<Utc>,
    #[serde(with = "utc_timestamp")]
    pub to: DateTime<Utc>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default)]
    pub jpeg: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub count: i32,
    pub images: i32,
    pub bytes: i64,
}

/// Frame sampled from a saved replay, handed from the agent to the store (kind replay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewReplayFrame {
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub replay_id: String,
    pub offset_ms: i32,
    pub app: String,
    pub title: String,
    pub monitor: i32,
    pub width: i32,
    pub height: i32,
    #[serde(with = "dotnet_base64")]
    pub image: Vec<u8>,
    pub image_mime: String,
    #[serde(default)]
    pub site: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub captured_at: Option<DateTime<Utc>>,
}

/// Speech segment after transcription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAudioCapture {
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub source: String,
    pub duration_ms: i32,
    pub text: String,
    pub language: String,
    pub app: String,
}

/// Work the scheduler hands to the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingJob {
    pub id: String,
    pub kind: CaptureKind,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "dotnet_base64::opt"
    )]
    pub image: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingInfo {
    pub model_id: String,
    pub revision: String,
    pub dim: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub q: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub from: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub to: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Vec<CaptureKind>>,
    #[serde(default)]
    pub mode: SearchMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub id: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: CaptureKind,
    pub app: String,
    pub title: String,
    #[serde(serialize_with = "dotnet_float::ser_f32")]
    pub score: f32,
    pub snippet: String,
    pub has_thumb: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "dotnet_float::ser_opt_f32"
    )]
    pub cosine: Option<f32>,
    #[serde(default)]
    pub text_match: bool,
    #[serde(default)]
    pub weak: bool,
    #[serde(default)]
    pub site: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_offset_ms: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub count: i32,
    pub truncated: bool,
    pub took_ms: i64,
    pub results: Vec<SearchHit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
}

/// Result of `get`: hit plus full stored text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureDetail {
    pub id: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: CaptureKind,
    pub app: String,
    pub title: String,
    pub text: String,
    pub has_thumb: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<i32>,
    #[serde(default)]
    pub site: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_offset_ms: Option<i32>,
}

/// Bytes serialise as the JSON field `"base64"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThumbData {
    pub id: String,
    pub mime: String,
    pub width: i32,
    pub height: i32,
    #[serde(rename = "base64", with = "dotnet_base64")]
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreStats {
    pub rows: i64,
    pub bytes: i64,
    pub thumb_bytes: i64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub oldest: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub newest: Option<DateTime<Utc>>,
    pub pending_embeddings: i32,
    pub vectors: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioState {
    pub system: bool,
    pub mic: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueState {
    pub pending: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub capturing: bool,
    pub paused: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub paused_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
    pub locked: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub unlocked_until: Option<DateTime<Utc>>,
    pub audio: AudioState,
    pub queue: QueueState,
    pub device: String,
    pub store: StoreStats,
    pub retention_days: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkeys_failed: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
    #[serde(default)]
    pub user_paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlock_needs: Option<String>,
}

/// Stream event (`subscribe` and `subscribe captures`): captures left the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedEvent {
    pub event: String,
    pub reason: String,
    pub ids: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub from: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub pipe: String,
    pub client_pid: i32,
    pub client_image: String,
    pub verb: String,
    pub result_count: i32,
    pub query_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeResult {
    pub purged: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undeleted: Option<i32>,
}

/// `{"items":[...]}` replies (`timeline`, `audit`, `replay list`, `history`, `chat list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemsReply<T> {
    pub items: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetCount {
    pub name: String,
    pub count: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetsReply {
    pub apps: Vec<FacetCount>,
    pub sites: Vec<FacetCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub q: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockReply {
    pub unlocked: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeConfirmReply {
    pub confirm: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionReply {
    pub app: String,
    pub protocol: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayClip {
    pub id: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub duration_ms: i32,
    pub bytes: i64,
    pub encrypted: bool,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordStatus {
    pub set: bool,
    pub hello_available: bool,
    pub retry_after_s: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiUsage {
    pub day: String,
    pub requests: i32,
    pub tokens: i64,
    pub request_limit: i32,
    pub token_limit: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiStatus {
    pub enabled: bool,
    pub key_set: bool,
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AiUsage>,
    #[serde(default)]
    pub local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSource {
    pub id: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: CaptureKind,
    pub app: String,
    pub title: String,
    pub site: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_offset_ms: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatApprovalItem {
    pub id: String,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: CaptureKind,
    pub app: String,
    pub title: String,
    pub site: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_mime: Option<String>,
    #[serde(
        rename = "base64",
        default,
        skip_serializing_if = "Option::is_none",
        with = "dotnet_base64::opt"
    )]
    pub image: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<ChatSource>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<ChatApprovalItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "utc_timestamp::opt"
    )]
    pub retry_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AiUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    #[serde(with = "utc_timestamp")]
    pub created: DateTime<Utc>,
    #[serde(with = "utc_timestamp")]
    pub updated: DateTime<Utc>,
    pub messages: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub seq: i32,
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<ChatSource>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatDetail {
    pub chat: ChatSummary,
    pub messages: Vec<ChatMessage>,
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRecord {
    pub id: String,
    pub title: String,
    #[serde(with = "utc_timestamp")]
    pub created: DateTime<Utc>,
    #[serde(with = "utc_timestamp")]
    pub updated: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub transcript: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescribePreview {
    pub token: String,
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub mime: String,
    pub width: i32,
    pub height: i32,
    pub redacted: bool,
    #[serde(rename = "base64", with = "dotnet_base64")]
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescribeResult {
    pub text: String,
    pub provider: String,
    pub model: String,
    pub took_ms: i64,
}

// ---- Full benchmark and recommendations ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuInfo {
    pub name: String,
    pub vendor: String,
    pub integrated: bool,
    pub vram_mb: i64,
    pub luid: String,
    pub driver: String,
    pub codecs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NpuInfo {
    pub name: String,
    pub driver: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub index: i32,
    pub width: i32,
    pub height: i32,
    pub refresh_hz: i32,
    pub adapter: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareInfo {
    pub cpu: String,
    pub cores: i32,
    pub threads: i32,
    pub ram_mb: i64,
    pub has_battery: bool,
    pub gpus: Vec<GpuInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npu: Option<NpuInfo>,
    pub monitors: Vec<MonitorInfo>,
    pub os: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderBench {
    pub adapter: String,
    pub codec: String,
    pub api: String,
    pub layout: String,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub ok: bool,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub encode_ms_p95: f64,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub cpu_percent: f64,
    pub working_set_mb: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureBench {
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub tick_ms: f64,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub keyframe_encode_ms: f64,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub cross_adapter_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyframe_encoder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBench {
    pub rows: i64,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub vector_ms: f64,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub hybrid_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recommendation {
    pub key: String,
    pub value: String,
    pub current: String,
    pub reason: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchReport {
    #[serde(with = "utc_timestamp")]
    pub ts: DateTime<Utc>,
    pub took_ms: i64,
    pub hardware: HardwareInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference: Option<BenchResult>,
    pub encoders: Vec<EncoderBench>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureBench>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchBench>,
    pub recommendations: Vec<Recommendation>,
}

// ---- MCP tool list message types (matching BetterRecall.Mcp.McpServer ToolsJson) ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolList {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_type: Option<String>,
    pub tools: Vec<McpTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: McpInputSchema,
    pub annotations: McpToolAnnotations,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInputSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<McpSchemaProperties>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    pub additional_properties: bool,
}

/// Order-preserving JSON object for MCP `inputSchema.properties`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpSchemaProperties(pub Vec<(String, McpSchemaProperty)>);

impl Serialize for McpSchemaProperties {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for McpSchemaProperties {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PropsVisitor;
        impl<'de> Visitor<'de> for PropsVisitor {
            type Value = McpSchemaProperties;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object of MCP schema properties")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, McpSchemaProperty>()? {
                    entries.push((k, v));
                }
                Ok(McpSchemaProperties(entries))
            }
        }
        deserializer.deserialize_map(PropsVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSchemaProperty {
    #[serde(rename = "type")]
    pub prop_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<McpSchemaItems>,
    #[serde(rename = "enum", default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSchemaItems {
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(rename = "enum", default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    pub read_only_hint: bool,
    pub open_world_hint: bool,
}

// ---- Interfaces.cs shared value types ----

#[derive(Debug, Error)]
#[error("Capture at {} is inside a purged window.", utc_timestamp::format_utc(.0))]
pub struct PurgedWindowError(pub DateTime<Utc>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeScope {
    pub kind: String,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub app: Option<String>,
    pub site: Option<String>,
    pub id: Option<String>,
}

impl PurgeScope {
    pub fn for_site(site: impl Into<String>) -> Self {
        Self {
            kind: "site".to_string(),
            from: None,
            to: None,
            app: None,
            site: Some(site.into()),
            id: None,
        }
    }

    pub fn for_id(id: impl Into<String>) -> Self {
        Self {
            kind: "id".to_string(),
            from: None,
            to: None,
            app: None,
            site: None,
            id: Some(id.into()),
        }
    }

    pub fn last(span: Duration) -> Self {
        let now = Utc::now();
        Self {
            kind: "last".to_string(),
            from: Some(now - span),
            to: Some(now),
            app: None,
            site: None,
            id: None,
        }
    }

    pub fn range(from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        Self {
            kind: "range".to_string(),
            from: Some(from),
            to: Some(to),
            app: None,
            site: None,
            id: None,
        }
    }

    pub fn for_app(app: impl Into<String>) -> Self {
        Self {
            kind: "app".to_string(),
            from: None,
            to: None,
            app: Some(app.into()),
            site: None,
            id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeNotice {
    pub reason: String,
    pub ids: Vec<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub newest: Option<DateTime<Utc>>,
    pub scope: Option<String>,
}

impl PurgeNotice {
    pub fn by_user(&self) -> bool {
        self.reason == "purge" || self.reason == "all"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreHostOptions {
    pub home: PathBuf,
    pub install_dir: PathBuf,
}

// ---- worker protocol payloads (docs/v1-design.md section 4) ----

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

// ---- models page (R7, stage I4) ----

/// One model component entry returned by the UI-pipe `models list` verb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelComponent {
    pub id: String,
    pub title: String,
    pub description: String,
    pub sets: Vec<String>,
    pub bytes: u64,
    pub state: String,
}

/// Reply to the UI-pipe `models list` verb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsList {
    pub models_dir: String,
    pub repo: String,
    pub revision: String,
    pub components: Vec<ModelComponent>,
    pub full_bytes: u64,
    pub minimal_bytes: u64,
    pub free_bytes: u64,
    pub downloading: bool,
}

/// One streamed line of `models download <target>`.
/// `phase`: `"checking" | "downloading" | "done" | "cancelled" | "offline" | "failed"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsProgress {
    pub phase: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
