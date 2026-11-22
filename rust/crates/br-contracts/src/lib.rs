// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

//! Wire types, pipe framing (line and binary), full settings schema and validation,
//! install/data layout, and Whisper language table shared across BetterRecall crates.

pub mod framing;
pub mod json;
pub mod layout;
pub mod models;
pub mod settings;
pub mod text;
pub mod whisper_languages;

pub use framing::{
    BinaryFrame, Envelope, ErrorReply, Frame, FrameError, LineError, LineProtocol, LineReader,
    LineTooLongError, PipeEnvelope,
};
pub use json::{
    DotNetFormatter, DotNetPrettyFormatter, dotnet_base64, dotnet_float, merge_section,
    parse_jsonc_object, strip_jsonc, to_json_string, to_pretty_json_string, utc_timestamp,
};
pub use layout::{DataPaths, InstallLayout, PipeNames, full_path};
pub use text::strip_suffix_ci;
pub use models::{
    AgentStatus, AiStatus, AiUsage, AuditEntry, AudioState, BenchDevice, BenchReport, BenchResult,
    CaptureBench, CaptureDetail, CaptureKind, CaptureMeta, ChatApprovalItem, ChatDetail, ChatEvent,
    ChatMessage, ChatRecord, ChatSource, ChatSummary, CoreHostOptions, DescribePreview,
    DescribeResult, EmbeddingInfo, EncoderBench, ExportRequest, ExportResult, FacetCount,
    FacetsReply, GpuInfo, HardwareInfo, HistoryEntry, ItemsReply, McpInputSchema, McpSchemaItems,
    McpSchemaProperties, McpSchemaProperty, McpTool, McpToolAnnotations, McpToolList,
    ModelComponent, ModelsList, ModelsProgress, MonitorInfo,
    NewAudioCapture, NewReplayFrame, NewScreenCapture, NpuInfo, OcrEntity, OcrLine, OcrResult,
    PasswordStatus, PendingJob, PurgeConfirmReply, PurgeNotice, PurgeResult, PurgeScope,
    PurgedWindowError, QueueState, Recommendation, RemovedEvent, ReplayClip, SearchBench,
    SearchHit, SearchMode, SearchRequest, SearchResponse, StorageUse, StoreStats, ThumbData,
    TokenBench, TranscribeResult, TranscriptSegment, UnlockReply, UpdateProgress, UpdateStatus,
    VersionReply, WorkerHello, WorkerRequest, WorkerResponse,
};
pub use settings::{
    AiDefaults, AiSettings, AiShare, AudioSettings, CaptureSettings, GovernanceSettings,
    HotkeySettings, IndexSettings, InferenceSettings, IpcSettings, KeyframeSettings, LogSettings,
    McpSettings, OcrSettings, PrivacySettings, ReplaySettings, RetentionSettings, SecuritySettings,
    Settings, StorageSettings, UiSettings, UpdateSettings, all_hotkeys, hotkey_ok, is_abs_path,
    valid_ai_endpoint, valid_hotkey, valid_ocr_languages, validate_settings,
};
pub use whisper_languages::WhisperLanguages;
