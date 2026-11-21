// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall Core library (stages A1 + A2 + A3: settings, logging, CoreJson, crypto,
//! hand-written SQLite3MC binding, encrypted store, vector index, export journal/exporter,
//! recommender, overlapped Win32 named pipes, command processor, CoreHost, and cloud AI).

pub mod ai;
pub mod cancel;
pub mod core_host;
pub mod core_json;
pub mod crypto;
pub mod ipc;
pub mod log;
pub mod recommender;
pub mod settings_store;
pub mod sqlite;
pub mod store;
pub mod traits;
pub mod win32;

pub use ai::{
    AiBudget, ApiKeyStore, AuditFn, BasicTextRedactor, ChatDeps, ChatFailure, ChatFailureError,
    ChatProvider, ChatProviders, ChatRequest, ChatService, ChatShareGate, ClaudeChat,
    CloudDescriber, DescribeError, DescribeRedactor, GeminiChat, HttpRequest, HttpResponse,
    HttpTransport, OpenAiChat, OutgoingImage, ParsedErrorBody, PassThroughRedactor,
    ProviderErrors, ProviderFactoryFn, ProviderHttp, ProviderStreamError, ProviderTurn,
    ShareGateInput, SharePlan, SharedHit, SseReadError, SseReader, TextRedactor, ToolCall,
    ToolOutput, ToolSpec, TransportError, UreqTransport,
};
pub use cancel::{CancelReg, CancelSource, CancelToken, ListenerGuard};
pub use core_host::CoreHost;
pub use core_json::{
    CaptureEvent, CoreJson, ExportEntry, HelpData, PongData, SubscribedCaptures, SubscribedStatus,
};
pub use crypto::{
    BlobCipher, BlobReader, BlobWriter, CryptoError, HelloInterop, KeyVault, PasswordCheck,
    SecureKey, UnlockPassword,
};
pub use ipc::{
    AgentPresence, AuditThrottle, CliPurge, CliPurgeOptions, ClientSession, ClientsChangedFn,
    CommandDeps, CommandProcessor, ConnectedChangedFn, ConnectionHandlerFn, ImageAllowedFn,
    PeerInfo, PipeClient, PipeConnectResult, PipeConnection, PipeKind, PipeLineReader, PipeManager,
    PipeNameFn, PipeServer, PipeStream, ProcessInfo, RejectHandlerFn, Reply, StepClock,
    StoreGetterFn, StreamProducer, Subscription,
};
pub use log::{Log, LogLevel, LogSink};
pub use recommender::{EncoderProbe, Recommender};
pub use settings_store::SettingsStore;
pub use sqlite::{Connection, SqliteError, Statement, Transaction, Value};
pub use store::{
    BinaryVectorIndex, CaptureExporter, EncryptedStore, ExportJournal, Ids, StoreError,
    VectorCandidate, VectorFilter, VectorIndex,
};
pub use traits::{AgentControl, AgentError, EmbedError, QueryEmbedder, Thumbnailer};
