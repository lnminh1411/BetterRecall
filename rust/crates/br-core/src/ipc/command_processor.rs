// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Command processor for the UI, public IPC, and MCP pipes (`CommandProcessor.cs` + `CommandProcessor.Chat.cs`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{
    AgentStatus, AiDefaults, AiStatus, AuditEntry, BenchReport, CaptureDetail, CaptureKind,
    CaptureMeta, ChatDetail, ChatEvent as WireChatEvent, ChatSummary, DataPaths, DescribePreview,
    Envelope, ExportRequest, FacetsReply, HistoryEntry, ItemsReply, LineError, LineProtocol,
    ModelsProgress, PurgeConfirmReply, PurgeNotice, PurgeResult, PurgeScope, QueueState,
    RemovedEvent, ReplayClip, SearchBench, SearchMode, SearchRequest, SearchResponse, Settings,
    StoreStats, UnlockReply, UpdateProgress, VersionReply, is_abs_path,
};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::ai::{
    AiBudget, ApiKeyStore, AuditFn, ChatDeps, ChatService, CloudDescriber, DescribeError,
    DescribeRedactor, OutgoingImage, PassThroughRedactor, ProviderFactoryFn, ProviderHttp,
};
use crate::cancel::{CancelSource, CancelToken, ListenerGuard};
use crate::core_json::{
    CaptureEvent, CoreJson, HelpData, PongData, SubscribedCaptures, SubscribedStatus,
};
use crate::crypto::{BlobCipher, KeyVault, PasswordCheck, UnlockPassword};
use crate::ipc::audit_throttle::AuditThrottle;
use crate::ipc::pipe_server::{PipeConnection, PipeKind};
use crate::ipc::pipe_stream::{PipeLineReader, PipeStream};
use crate::log::Log;
use crate::recommender::Recommender;
use crate::settings_store::SettingsStore;
use crate::store::{CaptureExporter, EncryptedStore, StoreError};
use crate::traits::{AgentControl, AgentError, EmbedError, QueryEmbedder, Thumbnailer};
use crate::win32;

/// A line read on the helper thread, handed back with its reader.
type PendingRead = (PipeLineReader, Result<Option<String>, LineError>);

/// Per-connection state: who is connected, purge tokens, describe tokens.
pub struct ClientSession {
    pub kind: PipeKind,
    pub pid: i32,
    pub image: String,
    purge_tokens: HashMap<String, PurgeTicket>,
    describe_tokens: HashMap<String, DescribeTicket>,
}

impl ClientSession {
    pub fn new(kind: PipeKind, pid: i32, image: impl Into<String>) -> Self {
        Self {
            kind,
            pid,
            image: image.into(),
            purge_tokens: HashMap::new(),
            describe_tokens: HashMap::new(),
        }
    }

    pub fn pipe_label(&self) -> &'static str {
        match self.kind {
            PipeKind::Ui => "ui",
            PipeKind::Mcp => "mcp",
            PipeKind::Public => "ipc",
        }
    }
}

struct PurgeTicket {
    scope: Option<PurgeScope>,
    expires: DateTime<Utc>,
    span: Option<ChronoDuration>,
}

struct DescribeTicket {
    capture_id: String,
    image: OutgoingImage,
    expires: DateTime<Utc>,
    provider: String,
    endpoint: String,
    model: String,
}

impl DescribeTicket {
    fn matches(&self, ai: &br_contracts::AiSettings) -> bool {
        self.provider == ai.provider
            && self.endpoint == AiDefaults::effective_endpoint(ai)
            && self.model == AiDefaults::effective_model(ai)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subscription {
    None,
    Status,
    Captures,
}

pub type StreamProducer = Box<dyn FnOnce(&mut dyn FnMut(String)) + Send>;

/// One response line, plus optional subscription switch, close flag, or multi-line stream producer.
pub struct Reply {
    pub line: String,
    pub subscribe: Subscription,
    pub close: bool,
    pub stream: Option<StreamProducer>,
}

impl Reply {
    pub fn ok(json: String) -> Self {
        Self {
            line: Envelope::ok_raw(&json),
            subscribe: Subscription::None,
            close: false,
            stream: None,
        }
    }

    pub fn err(msg: &str) -> Self {
        Self {
            line: Envelope::error(msg),
            subscribe: Subscription::None,
            close: false,
            stream: None,
        }
    }
}

pub type StoreFn = Arc<dyn Fn() -> Option<Arc<EncryptedStore>> + Send + Sync>;
pub type PurgeAllFn = Arc<dyn Fn(&CancelToken) -> i32 + Send + Sync>;
pub type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
pub type MonotonicClockFn = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Everything the command processor reads or calls (`CommandDeps`).
pub struct CommandDeps {
    pub settings: Arc<SettingsStore>,
    pub store: StoreFn,
    pub keys: Arc<KeyVault>,
    pub agent: Arc<dyn AgentControl>,
    pub embedder: Arc<dyn QueryEmbedder>,
    pub thumbnailer: Arc<dyn Thumbnailer>,
    pub purge_all: PurgeAllFn,
    pub purge_all_undeleted: Option<Arc<dyn Fn() -> i32 + Send + Sync>>,
    pub blobs: Option<Arc<BlobCipher>>,
    pub home: Option<PathBuf>,
    pub password: Option<Arc<UnlockPassword>>,
    pub ai_key: Option<Arc<ApiKeyStore>>,
    pub describer: Option<Arc<CloudDescriber>>,
    pub redactor: Option<Arc<dyn DescribeRedactor>>,
    pub chat_http: Option<Arc<ProviderHttp>>,
    pub chat_providers: Option<ProviderFactoryFn>,
}

type RemovedHandler = Arc<dyn Fn(&PurgeNotice) + Send + Sync>;

/// Verbs, usage strings and errors of research 7.6 and v1-design section 5.
pub struct CommandProcessor {
    d: CommandDeps,
    connect_audit: AuditThrottle,
    last_unlock: Mutex<DateTime<Utc>>,
    last_no_hello_purge: Mutex<DateTime<Utc>>,
    now: Mutex<ClockFn>,
    status_heartbeat_ms: AtomicI64,
    idle_timeout_override_ms: AtomicI64,
    cold_embed_wait_ms: AtomicI64,
    chat: Option<Arc<ChatService>>,
    public_searches: Mutex<VecDeque<Instant>>,
    mcp_searches: Mutex<VecDeque<Instant>>,
    monotonic_now: Mutex<MonotonicClockFn>,
    purge_all_epoch: AtomicI32,
    removed_listeners: Arc<Mutex<Vec<(u64, RemovedHandler)>>>,
    next_removed_id: AtomicU64,
    lock_cts: Arc<Mutex<CancelSource>>,
    unlocked_with_passphrase: AtomicBool,
    active_exports: AtomicUsize,
    _guards: Vec<ListenerGuard>,
}

impl CommandProcessor {
    pub const APP_VERSION: &'static str = env!("CARGO_PKG_VERSION");
    pub const PROTOCOL: i32 = 1;
    pub const UI_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
    pub const UI_MAX_RESULTS: i32 = 500;
    pub const MAX_REMOVED_IDS: usize = 500;
    pub const COLD_EMBED_WAIT: Duration = Duration::from_secs(3);

    pub const USAGE_SEARCH: &'static str = "Usage: search <text> | search <json>";
    pub const USAGE_GET: &'static str = "Usage: get <id>";
    pub const USAGE_PAUSE: &'static str = "Usage: pause [seconds]";
    pub const USAGE_PURGE: &'static str = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge site <host> | purge id <id> | purge confirm <token>";
    pub const USAGE_PURGE_UI: &'static str = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge site <host> | purge id <id> | purge all | purge confirm <token>";
    pub const USAGE_FACETS: &'static str = "Usage: facets [<from> <to>]";
    pub const USAGE_HISTORY: &'static str = "Usage: history | history add <text> | history delete <text> | history clear";
    pub const USAGE_SUBSCRIBE: &'static str = "Usage: subscribe [captures]";
    pub const USAGE_THUMB: &'static str = "Usage: thumb <id> [width]";
    pub const USAGE_KEYFRAME: &'static str = "Usage: keyframe <id>";
    pub const USAGE_TIMELINE: &'static str = "Usage: timeline <from> <to> [limit]";
    pub const USAGE_SETTINGS: &'static str = "Usage: settings get | settings set <json> | settings reset";
    pub const ERR_NOTHING_TO_RESET: &'static str = "settings.json loaded; there is nothing to reset.";
    pub const USAGE_REPLAY: &'static str = "Usage: replay save | replay list | replay export <id> <path> | replay delete <id>";
    pub const USAGE_AUDIT: &'static str = "Usage: audit [limit]";
    pub const USAGE_BENCH: &'static str = "Usage: bench | bench full | bench last";
    pub const USAGE_EXPORT: &'static str = "Usage: export <json>";
    pub const ERR_LOCKED: &'static str = "Store is locked.";
    pub const ERR_NOT_OPEN: &'static str = "Store is not open.";
    pub const ERR_NOT_FOUND: &'static str = "Not found.";
    pub const ERR_INTERNAL: &'static str = "Internal error.";
    pub const ERR_RATE: &'static str = "Rate limit exceeded.";
    pub const ERR_TOKEN: &'static str = "Invalid or expired token.";
    pub const ERR_JSON: &'static str = "Invalid JSON.";
    pub const ERR_CONTROL_OFF: &'static str = "Control is off (config ipc.allowControl).";
    pub const ERR_PURGE_OFF: &'static str = "Purge is off (config ipc.allowPurge).";
    pub const ERR_THUMBS_OFF: &'static str = "Thumbnails are off (config ipc.allowThumbnails).";
    pub const ERR_UNLOCK_CANCELLED: &'static str = "Unlock cancelled.";
    pub const ERR_WRONG_PASSPHRASE: &'static str = "Wrong passphrase.";
    pub const ERR_UNLOCK_REQUIRED: &'static str = "Unlock required.";
    pub const USAGE_PASSWORD: &'static str = "Usage: password | password set <json> | password clear <json>";
    pub const USAGE_AI: &'static str = "Usage: ai | ai setkey <key> | ai clearkey";
    pub const USAGE_DESCRIBE: &'static str = "Usage: describe preview <id> | describe send <token>";
    pub const USAGE_CHAT: &'static str = "Usage: chat list | chat get <id> | chat send <json> | chat approve <json> | chat deny <token> | chat rename <json> | chat delete <id> | chat clear";
    pub const USAGE_MODELS: &'static str = "Usage: models list | models download <full|minimal|<id>>";
    pub const ERR_MODELS_BUSY: &'static str = "A model download is already running.";
    pub const USAGE_UPDATE: &'static str = "Usage: update status | update check | update download | update apply";
    pub const ERR_UPDATE_BUSY: &'static str = "An update download is already running.";
    pub const ERR_UPDATE_RUNNING_FOR_MODELS: &'static str = "An update download is running.";
    pub const ERR_MODELS_RUNNING_FOR_UPDATE: &'static str = "A model download is running.";
    pub const ERR_WRONG_PASSWORD: &'static str = "Wrong password.";
    pub const ERR_NO_PASSWORD: &'static str = "No unlock password is set.";
    pub const ERR_PASSWORD_RULE: &'static str = "Password must be 6 to 256 characters.";
    pub const ERR_PASSWORD_IN_USE: &'static str = "Password in use (config security.unlockMethod).";
    pub const ERR_PASSWORD_SAVE: &'static str = "Could not save the password.";
    pub const ERR_PASSWORD_REMOVE: &'static str = "Could not remove the password.";
    pub const ERR_HELLO_MISSING: &'static str = "Windows Hello is not available: unlock with the password (config security.unlockMethod).";
    pub const ERR_PASSPHRASE_RULE: &'static str = "A new passphrase must be 8 to 1024 characters.";
    pub const ERR_PASSPHRASE_FIRST: &'static str = "Set the passphrase first: with security.unlockMethod hello, unlock with the new passphrase, then switch to password.";
    pub const ERR_PASSPHRASE_NEEDED: &'static str = "Unlock with the data-key passphrase to change security.keyProtection.";
    pub const ERR_PASSPHRASE_NEEDS_HELLO: &'static str = "Passphrase key protection with security.unlockMethod hello requires Windows Hello.";
    pub const USAGE_PASSPHRASE: &'static str = "Usage: passphrase verify <json>";
    pub const ERR_TOO_MANY_PREFIX: &'static str = "Too many attempts. Try again in ";
    pub const ERR_AI_OFF: &'static str = "Cloud describe is off (config ai.enabled).";
    pub const ERR_NO_API_KEY: &'static str = "API key not set.";
    pub const ERR_API_KEY_RULE: &'static str = "Invalid API key.";
    pub const ERR_NO_IMAGE: &'static str = "No image to describe.";
    pub const ERR_DESCRIBE_CHANGED: &'static str = "The AI settings changed after the preview. Open the preview again.";
    pub const ERR_PROVIDER_UNREACHABLE: &'static str = "Provider unreachable.";
    pub const ERR_PROVIDER_TIMEOUT: &'static str = "Provider timed out.";
    pub const ERR_PURGED_DURING: &'static str = "A purge ran during the export; nothing was written.";
    pub const ERR_REPLAY_OFF: &'static str = "Instant replay is off.";

    pub const REPLAY_PURGE_REFUSALS: [&'static str; 2] = [
        "Purge all is running.",
        "A purge ran during the replay save; the clip was discarded.",
    ];

    const PUBLIC_VERBS: [&'static str; 11] = [
        "ping", "version", "help", "status", "search", "get", "thumb", "subscribe", "pause",
        "resume", "purge",
    ];
    const MCP_VERBS: [&'static str; 5] = ["ping", "version", "status", "search", "get"];
    const UI_ONLY_VERBS: [&'static str; 20] = [
        "unlock", "lock", "consent", "settings", "timeline", "keyframe", "replay", "panic",
        "audit", "bench", "facets", "history", "password", "passphrase", "ai", "describe",
        "export", "chat", "models", "update",
    ];
    const GUARDED_SETTING_GROUPS: [&'static str; 12] = [
        "security", "ai", "ipc", "mcp", "privacy", "ocr", "retention", "storage", "replay",
        "index", "audio", "keyframes",
    ];
    const GUARDED_SETTING_KEYS: [(&'static str, &'static str); 1] = [("inference", "modelDir")];
    const SENDABLE_MIMES: [&'static str; 4] = [
        "image/jpeg", "image/png", "image/webp", "image/gif",
    ];
    const BENCH_WORDS: [&'static str; 10] = [
        "report", "invoice", "meeting", "email", "design", "photo", "budget", "release", "draft",
        "ticket",
    ];

    pub fn err_too_many(seconds: i32) -> String {
        format!("{}{seconds} s.", Self::ERR_TOO_MANY_PREFIX)
    }

    pub fn new(deps: CommandDeps) -> Self {
        let lock_cts = Arc::new(Mutex::new(CancelSource::new()));
        let mut guards = Vec::new();

        let lock_cts_status = lock_cts.clone();
        let lock_cts_panic = lock_cts.clone();

        let chat = deps.ai_key.as_ref().map(|keys| {
            let settings_fn = deps.settings.clone();
            let store_fn = deps.store.clone();
            let agent_for_store = deps.agent.clone();
            let keys_for_api = keys.clone();
            let settings_for_api = deps.settings.clone();
            Arc::new(ChatService::new(ChatDeps {
                settings: Arc::new(move || settings_fn.current()),
                store: Arc::new(move || {
                    if !agent_for_store.is_unlocked() {
                        return None;
                    }
                    store_fn()
                }),
                embedder: deps.embedder.clone(),
                thumbnailer: deps.thumbnailer.clone(),
                api_key: Arc::new(move || keys_for_api.read(&settings_for_api.current().ai)),
                http: deps
                    .chat_http
                    .clone()
                    .unwrap_or_else(|| Arc::new(ProviderHttp::new())),
                providers: deps
                    .chat_providers
                    .clone()
                    .unwrap_or_else(ChatDeps::default_providers),
                image_redactor: deps.redactor.clone(),
                text_redactor: ChatDeps::default_text_redactor(),
                audit: ChatDeps::noop_audit(),
            }))
        });

        let chat_for_status = chat.clone();
        guards.push(deps.agent.on_status_changed(Arc::new(move |st| {
            if st.locked {
                cancel_lock_cts(&lock_cts_status);
                if let Some(ref c) = chat_for_status {
                    c.stop_all();
                    c.forget_approvals();
                }
            }
        })));

        let chat_for_panic = chat.clone();
        let store_for_panic = deps.store.clone();
        guards.push(deps.agent.on_panicked(Arc::new(move || {
            cancel_lock_cts(&lock_cts_panic);
            if let Some(ref c) = chat_for_panic {
                c.clear();
            }
            if let Some(st) = store_for_panic()
                && let Err(e) = st.delete_chats(None)
            {
                Log::warn_err("panic chat delete", &e.to_string());
            }
        })));

        if let Some(ref c) = chat {
            let chat_for_settings = c.clone();
            guards.push(deps.settings.on_changed_guarded(move |s| {
                if !s.ai.enabled {
                    chat_for_settings.stop_all();
                }
            }));
        }

        Self {
            d: deps,
            // B4: monotonic clock so backward wall-clock steps do not suppress connect audits.
            connect_audit: AuditThrottle::new(Duration::from_secs(60), 30, 512),
            last_unlock: Mutex::new(DateTime::<Utc>::MIN_UTC),
            last_no_hello_purge: Mutex::new(DateTime::<Utc>::MIN_UTC),
            now: Mutex::new(Arc::new(Utc::now)),
            status_heartbeat_ms: AtomicI64::new(60_000),
            idle_timeout_override_ms: AtomicI64::new(0),
            cold_embed_wait_ms: AtomicI64::new(Self::COLD_EMBED_WAIT.as_millis() as i64),
            chat,
            public_searches: Mutex::new(VecDeque::new()),
            mcp_searches: Mutex::new(VecDeque::new()),
            monotonic_now: Mutex::new(Arc::new(Instant::now)),
            purge_all_epoch: AtomicI32::new(0),
            removed_listeners: Arc::new(Mutex::new(Vec::new())),
            next_removed_id: AtomicU64::new(1),
            lock_cts,
            unlocked_with_passphrase: AtomicBool::new(false),
            active_exports: AtomicUsize::new(0),
            _guards: guards,
        }
    }

    pub fn is_export_running(&self) -> bool {
        self.active_exports.load(Ordering::Acquire) > 0
    }

    pub fn set_now_for_test(&self, now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) {
        *self.now.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(now);
    }

    /// B3: Injectable monotonic clock seam for rate limiter tests.
    pub fn set_monotonic_now_for_test(&self, now: impl Fn() -> Instant + Send + Sync + 'static) {
        *self.monotonic_now.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(now);
    }

    fn monotonic_now(&self) -> Instant {
        let f = self.monotonic_now.lock().unwrap_or_else(|e| e.into_inner()).clone();
        f()
    }

    pub fn set_status_heartbeat_for_test(&self, dur: Duration) {
        self.status_heartbeat_ms
            .store(dur.as_millis() as i64, Ordering::Release);
    }

    /// For tests: override the non-UI pipe idle-connection timeout (`None` uses `ipc.idleTimeoutS`).
    pub fn set_idle_timeout_for_test(&self, dur: Option<Duration>) {
        let ms = dur.map(|d| d.as_millis().max(1) as i64).unwrap_or(0);
        self.idle_timeout_override_ms.store(ms, Ordering::Release);
    }

    /// For tests: override `COLD_EMBED_WAIT` when waiting on a cold query embedder.
    pub fn set_cold_embed_wait_for_test(&self, dur: Duration) {
        self.cold_embed_wait_ms
            .store(dur.as_millis().max(1) as i64, Ordering::Release);
    }

    /// E1: For tests: override chat service idle timeout.
    pub fn set_chat_idle_timeout_for_test(&self, dur: Duration) {
        if let Some(ref c) = self.chat {
            c.set_idle_timeout_for_test(dur);
        }
    }

    fn now(&self) -> DateTime<Utc> {
        let f = self.now.lock().unwrap_or_else(|e| e.into_inner()).clone();
        f()
    }

    fn settings(&self) -> Settings {
        self.d.settings.current()
    }

    /// Rows left the store for any reason (`ICoreHost.Purged`).
    pub fn on_store_purged(&self, n: &PurgeNotice) {
        if n.reason == "all" {
            self.purge_all_epoch.fetch_add(1, Ordering::AcqRel);
        }
        if !n.ids.is_empty() || n.from.is_some() {
            let handlers: Vec<RemovedHandler> = self
                .removed_listeners
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(_, h)| h.clone())
                .collect();
            for h in handlers {
                h(n);
            }
        }
        if let Some(ref chat) = self.chat
            && (!n.ids.is_empty() || n.from.is_some())
        {
            let gone: HashSet<String> = n.ids.iter().cloned().collect();
            chat.forget_captures(|id| gone.contains(id), n.from, n.to);
        }
    }

    pub fn removed_event_for(n: &PurgeNotice) -> RemovedEvent {
        if n.ids.len() > Self::MAX_REMOVED_IDS {
            RemovedEvent {
                event: "removed".to_string(),
                reason: n.reason.clone(),
                ids: Vec::new(),
                from: n.from,
                to: n.to,
                truncated: true,
            }
        } else {
            RemovedEvent {
                event: "removed".to_string(),
                reason: n.reason.clone(),
                ids: n.ids.clone(),
                from: n.from,
                to: n.to,
                truncated: false,
            }
        }
    }

    pub fn purge_app_name(name: &str) -> String {
        let mut n = name.trim();
        if let Some(pos) = n.rfind(['\\', '/']) {
            n = &n[pos + 1..];
        }
        // X1: never slice inside a multi-byte character.
        if let Some(stem) = br_contracts::strip_suffix_ci(n, ".exe") {
            n = stem;
        }
        n.trim().to_string()
    }

    pub fn purge_site_host(arg: &str) -> String {
        let a = arg.trim();
        let candidate = if a.contains("://") {
            Some(a.to_string())
        } else if a.contains(['/', ':', '?', '#']) {
            Some(format!("http://{a}"))
        } else {
            None
        };
        if let Some(url) = candidate
            && let Some((_, rest)) = url.split_once("://")
        {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let host_port = authority.rsplit('@').next().unwrap_or("");
            let host = if host_port.starts_with('[') {
                host_port
                    .find(']')
                    .map(|end| &host_port[..=end])
                    .unwrap_or(host_port)
            } else {
                host_port.split(':').next().unwrap_or("")
            };
            if !host.is_empty() {
                return host.to_string();
            }
        }
        a.to_string()
    }

    // ---- connection loop ----

    /// Request loop for one trust-checked connection, including subscribe streams and idle timeout.
    pub fn serve(&self, conn: PipeConnection, ct: &CancelToken) {
        let mut session = ClientSession::new(
            conn.kind,
            conn.peer.pid,
            conn.peer.image.unwrap_or_default(),
        );
        let key = format!("{}|{}", session.pipe_label(), session.image);
        let merged = self.connect_audit.admit(&key, self.monotonic_now());
        if merged > 0 {
            self.audit(&session, "connect", merged, "");
        }
        let stream = conn.stream;
        let mut reader = Some(PipeLineReader::new(stream.clone(), 8192));
        let mut pending_rx: Option<mpsc::Receiver<PendingRead>> = None;
        let max_line = if conn.kind == PipeKind::Ui {
            LineProtocol::MAX_UI_REQUEST_BYTES
        } else {
            LineProtocol::MAX_REQUEST_BYTES
        };

        while !ct.is_cancelled() && !stream.is_closed() {
            let idle_timeout = if conn.kind != PipeKind::Ui {
                let override_ms = self.idle_timeout_override_ms.load(Ordering::Acquire);
                if override_ms > 0 {
                    Some(Duration::from_millis(override_ms as u64))
                } else {
                    Some(Duration::from_secs(
                        self.settings().ipc.idle_timeout_s.max(1) as u64,
                    ))
                }
            } else {
                None
            };

            let read_res = if let Some(rx) = pending_rx.take() {
                match rx.recv() {
                    Ok((r, res)) => {
                        reader = Some(r);
                        res
                    }
                    Err(_) => return,
                }
            } else {
                reader
                    .as_mut()
                    .expect("reader present")
                    .read_line(max_line, idle_timeout, ct)
            };

            let line = match read_res {
                Ok(Some(l)) => l,
                Ok(None) => return,
                Err(LineError::TooLong(max)) => {
                    let _ = stream.write_line(
                        &Envelope::error(&format!("Line exceeds {max} bytes.")),
                        Some(Duration::from_secs(2)),
                        ct,
                    );
                    return;
                }
                Err(LineError::Io(_)) => return,
            };

            let reply = if Self::is_long_running(conn.kind, &line) {
                let req_cts = Arc::new(CancelSource::linked(ct));
                let req_ct = req_cts.token();
                let mut bg_reader = reader.take().expect("reader present");
                let (tx, rx) = mpsc::sync_channel(1);
                let ct_bg = ct.clone();
                let req_cts_bg = req_cts.clone();
                let _ = thread::Builder::new()
                    .name("BetterRecall.PipeLongRead".to_string())
                    .spawn(move || {
                        let res = bg_reader.read_line(max_line, None, &ct_bg);
                        let eof_or_err = matches!(&res, Ok(None) | Err(_));
                        if eof_or_err {
                            req_cts_bg.cancel();
                        }
                        let _ = tx.send((bg_reader, res));
                    });
                pending_rx = Some(rx);

                let r = self.handle(&mut session, &line, &req_ct);
                if let Some(producer) = r.stream {
                    if !Self::run_stream(&stream, producer, &req_ct) {
                        return;
                    }
                    continue;
                }
                r
            } else {
                self.handle(&mut session, &line, ct)
            };

            if reply.subscribe != Subscription::None {
                self.stream_subscription(&stream, reply.subscribe, &reply.line, ct, conn.kind);
                return;
            }
            if stream.write_line(&reply.line, None, ct).is_err() {
                return;
            }
            if reply.close {
                return;
            }
        }
    }

    /// Writes a multi-line reply as the producer emits it. Returns `false` when the client went away.
    pub fn run_stream(
        stream: &PipeStream,
        producer: StreamProducer,
        ct: &CancelToken,
    ) -> bool {
        let (tx, rx) = mpsc::channel::<String>();
        let handle = thread::Builder::new()
            .name("BetterRecall.StreamProducer".to_string())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut emit = |l: String| {
                        let _ = tx.send(l);
                    };
                    producer(&mut emit);
                }));
                if res.is_err() {
                    Log::error("stream producer failed");
                    let _ = tx.send(Envelope::error(Self::ERR_INTERNAL));
                }
            });

        let mut ok = true;
        for line in rx {
            if ct.is_cancelled() || stream.write_line(&line, None, ct).is_err() {
                ok = false;
                break;
            }
        }
        if let Ok(h) = handle {
            let _ = h.join();
        }
        ok
    }

    /// Verbs that may run for minutes (UI pipe only). Closing the pipe cancels them.
    pub fn is_long_running(kind: PipeKind, line: &str) -> bool {
        if kind != PipeKind::Ui {
            return false;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[0].eq_ignore_ascii_case("export") {
            return true;
        }
        if parts.len() >= 3
            && parts[0].eq_ignore_ascii_case("replay")
            && parts[1].eq_ignore_ascii_case("export")
        {
            return true;
        }
        if parts.len() >= 2
            && parts[0].eq_ignore_ascii_case("chat")
            && matches!(
                parts[1].to_ascii_lowercase().as_str(),
                "send" | "approve" | "deny"
            )
        {
            return true;
        }
        if parts.len() >= 2 && parts[0].eq_ignore_ascii_case("search") {
            return true;
        }
        if parts.len() == 3
            && parts[0].eq_ignore_ascii_case("describe")
            && parts[1].eq_ignore_ascii_case("send")
        {
            return true;
        }
        if parts.len() >= 2
            && parts[0].eq_ignore_ascii_case("models")
            && parts[1].eq_ignore_ascii_case("download")
        {
            return true;
        }
        if parts.len() >= 2
            && parts[0].eq_ignore_ascii_case("update")
            && (parts[1].eq_ignore_ascii_case("download") || parts[1].eq_ignore_ascii_case("check"))
        {
            return true;
        }
        parts.len() == 2
            && parts[0].eq_ignore_ascii_case("bench")
            && parts[1].eq_ignore_ascii_case("full")
    }

    /// One-way stream after `subscribe` (research 7.7). Input is ignored; 64 queued messages disconnect.
    pub fn stream_subscription(
        &self,
        stream: &PipeStream,
        sub: Subscription,
        ack: &str,
        ct: &CancelToken,
        kind: PipeKind,
    ) {
        let (tx, rx) = mpsc::sync_channel::<String>(64);
        let cts = Arc::new(CancelSource::linked(ct));
        let sub_ct = cts.token();

        let tx_push = tx.clone();
        let cts_push = cts.clone();
        let push = Arc::new(move |line: String| {
            if tx_push.try_send(line).is_err() && !cts_push.is_cancelled() {
                Log::info("subscriber too slow, disconnected");
                cts_push.cancel();
            }
        });

        let mut guards = Vec::new();
        let last_sent = Arc::new(Mutex::new(Option::<String>::None));

        if sub == Subscription::Status {
            let push_s = push.clone();
            let last_s = last_sent.clone();
            let d_settings = self.d.settings.clone();
            let d_agent = self.d.agent.clone();
            let d_store = self.d.store.clone();
            let d_keys = self.d.keys.clone();
            let d_pw = self.d.password.clone();
            guards.push(self.d.agent.on_status_changed(Arc::new(move |st| {
                let shaped = status_for_parts(
                    kind,
                    st.clone(),
                    &d_settings.current(),
                    d_agent.as_ref(),
                    &d_store,
                    d_keys.as_ref(),
                    d_pw.as_deref(),
                );
                let Ok(line) = Envelope::ok(&shaped) else {
                    return;
                };
                let locked = lock_error_parts(
                    kind,
                    &d_settings.current(),
                    d_agent.as_ref(),
                    &d_store,
                    d_keys.as_ref(),
                )
                .is_some();
                if kind != PipeKind::Ui && locked {
                    let prev = last_s.lock().unwrap_or_else(|e| e.into_inner());
                    if prev.as_deref() == Some(line.as_str()) {
                        return;
                    }
                }
                *last_s.lock().unwrap_or_else(|e| e.into_inner()) = Some(line.clone());
                push_s(line);
            })));
        } else {
            let push_c = push.clone();
            let d_settings = self.d.settings.clone();
            let d_agent = self.d.agent.clone();
            let d_store = self.d.store.clone();
            let d_keys = self.d.keys.clone();
            guards.push(self.d.agent.on_capture_stored(Arc::new(move |m: &CaptureMeta| {
                if lock_error_parts(
                    kind,
                    &d_settings.current(),
                    d_agent.as_ref(),
                    &d_store,
                    d_keys.as_ref(),
                )
                .is_some()
                {
                    return;
                }
                let ev = CaptureEvent {
                    id: m.id.clone(),
                    ts: m.ts,
                    kind: m.kind,
                    app: m.app.clone(),
                };
                if let Ok(line) = Envelope::ok(&ev) {
                    push_c(line);
                }
            })));
        }

        let removed_events = sub == Subscription::Captures || kind == PipeKind::Ui;
        if removed_events {
            let push_r = push.clone();
            let d_settings = self.d.settings.clone();
            let d_agent = self.d.agent.clone();
            let d_store = self.d.store.clone();
            let d_keys = self.d.keys.clone();
            let id = self.next_removed_id.fetch_add(1, Ordering::Relaxed);
            self.removed_listeners
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((
                    id,
                    Arc::new(move |n: &PurgeNotice| {
                        let s = d_settings.current();
                        let blocked = if n.reason == "all" {
                            (kind == PipeKind::Ui || s.ipc.require_unlocked)
                                && !d_agent.is_unlocked()
                        } else {
                            lock_error_parts(kind, &s, d_agent.as_ref(), &d_store, d_keys.as_ref())
                                .is_some()
                        };
                        if blocked {
                            return;
                        }
                        if let Ok(line) = Envelope::ok(&Self::removed_event_for(n)) {
                            push_r(line);
                        }
                    }),
                ));
            let list = self.removed_listeners.clone();
            guards.push(ListenerGuard::new(move || {
                list.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(lid, _)| *lid != id);
            }));
        }

        if stream.write_line(ack, None, ct).is_err() {
            drop(guards);
            return;
        }

        // Drain input until client closes pipe.
        let drain_stream = stream.clone();
        let drain_cts = cts.clone();
        let drain_ct = sub_ct.clone();
        let drain_handle = thread::Builder::new()
            .name("BetterRecall.SubDrain".to_string())
            .spawn(move || {
                let mut buf = [0u8; 256];
                while let Ok(n) = drain_stream.read(&mut buf, None, &drain_ct) {
                    if n == 0 {
                        break;
                    }
                }
                drain_cts.cancel();
            });

        let mut last_status = Instant::now();
        while !sub_ct.is_cancelled() {
            let hb = Duration::from_millis(self.status_heartbeat_ms.load(Ordering::Acquire).max(10) as u64);
            let wait = if sub == Subscription::Status {
                let rem = hb.saturating_sub(last_status.elapsed());
                let min_step = (hb / 60).max(Duration::from_millis(10));
                rem.max(min_step).min(Duration::from_millis(100))
            } else {
                Duration::from_millis(100)
            };

            let msg = match rx.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if sub == Subscription::Status && last_status.elapsed() >= hb {
                        let st = self.status_for(kind, self.d.agent.get_status());
                        Envelope::ok(&st).ok()
                    } else {
                        continue;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            if sub_ct.is_cancelled() {
                break;
            }
            if let Some(m) = msg {
                if sub == Subscription::Status {
                    last_status = Instant::now();
                }
                if stream.write_line(&m, None, &sub_ct).is_err() {
                    break;
                }
            }
        }

        drop(guards);
        cts.cancel();
        if let Ok(h) = drain_handle {
            let _ = h.join();
        }
    }

    // ---- dispatch ----

    /// Handles one request line. Never panics.
    pub fn handle(&self, session: &mut ClientSession, line: &str, ct: &CancelToken) -> Reply {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.dispatch(session, line, ct)
        })) {
            Ok(r) => r,
            Err(_) => {
                Log::error("command failed (panic)");
                Reply::err(Self::ERR_INTERNAL)
            }
        }
    }

    fn allowed(kind: PipeKind, verb: &str) -> bool {
        match kind {
            PipeKind::Mcp => Self::MCP_VERBS.contains(&verb),
            PipeKind::Public => Self::PUBLIC_VERBS.contains(&verb),
            PipeKind::Ui => Self::PUBLIC_VERBS.contains(&verb) || Self::UI_ONLY_VERBS.contains(&verb),
        }
    }

    fn dispatch(&self, s: &mut ClientSession, line: &str, ct: &CancelToken) -> Reply {
        let (verb_raw, rest) = split_verb(line);
        let verb = verb_raw.to_ascii_lowercase();
        if !Self::allowed(s.kind, &verb) {
            return Reply::err(&format!("Unknown command '{verb_raw}'."));
        }
        let args: Vec<&str> = rest.split_whitespace().collect();
        match verb.as_str() {
            "ping" => Reply::ok(CoreJson::to_string(&PongData { pong: true }).unwrap()),
            "version" => Reply::ok(
                CoreJson::to_string(&VersionReply {
                    app: Self::APP_VERSION.to_string(),
                    protocol: Self::PROTOCOL,
                })
                .unwrap(),
            ),
            "help" => Reply::ok(
                CoreJson::to_string(&HelpData {
                    verbs: self.help_verbs(s.kind),
                })
                .unwrap(),
            ),
            "status" => Reply::ok(
                CoreJson::to_string(&self.status_for(s.kind, self.d.agent.get_status())).unwrap(),
            ),
            "search" => self.search(s, rest, ct),
            "get" => self.get_capture(s, &args),
            "thumb" => self.thumb(s, &args, ct),
            "subscribe" => self.subscribe(s, &args),
            "pause" => self.pause(s, &args),
            "resume" => {
                if s.kind == PipeKind::Public && !self.settings().ipc.allow_control {
                    return Reply::err(Self::ERR_CONTROL_OFF);
                }
                if !args.is_empty() {
                    return Reply::err("Usage: resume");
                }
                self.d
                    .agent
                    .resume(if s.kind == PipeKind::Ui { "user" } else { "ipc" });
                Reply::ok("null".to_string())
            }
            "purge" => self.purge(s, &args, ct),
            "unlock" => self.unlock(s, raw_rest(line), ct),
            "lock" => {
                self.d.agent.lock();
                self.unlocked_with_passphrase.store(false, Ordering::Release);
                cancel_lock_cts(&self.lock_cts);
                Reply::ok("null".to_string())
            }
            "consent" => {
                if self.d.settings.load_error().is_some() {
                    return Reply::err(SettingsStore::ERR_UNREADABLE);
                }
                self.d.agent.consent();
                Reply::ok("null".to_string())
            }
            "panic" => {
                self.d.agent.panic();
                Reply::ok("null".to_string())
            }
            "bench" => self.bench(s, &args, ct),
            "settings" => self.settings_verb(s, rest),
            "timeline" => self.timeline(s, &args),
            "keyframe" => self.keyframe(s, &args),
            "replay" => self.replay(s, rest, ct),
            "audit" => self.audit_verb(s, &args),
            "facets" => self.facets(s, &args),
            "history" => self.history(s, rest),
            "password" => self.password_verb(s, raw_rest(line)),
            "passphrase" => self.passphrase_verb(s, raw_rest(line)),
            "ai" => self.ai_verb(s, raw_rest(line)),
            "describe" => self.describe(s, &args, ct),
            "export" => self.export(s, rest, ct),
            "chat" => self.chat_verb(s, rest, ct),
            "models" => self.models_verb(&args, ct),
            "update" => self.update_verb(&args, ct),
            _ => Reply::err(&format!("Unknown command '{verb_raw}'.")),
        }
    }

    pub fn help_verbs(&self, kind: PipeKind) -> Vec<String> {
        if kind == PipeKind::Mcp {
            return Self::MCP_VERBS.iter().map(|s| (*s).to_string()).collect();
        }
        let mut list: Vec<String> = ["ping", "version", "help", "status", "search", "get"]
            .into_iter()
            .map(String::from)
            .collect();
        let ipc = self.settings().ipc;
        let ui = kind == PipeKind::Ui;
        if ui || ipc.allow_thumbnails {
            list.push("thumb".to_string());
        }
        list.push("subscribe".to_string());
        if ui || ipc.allow_control {
            list.push("pause".to_string());
            list.push("resume".to_string());
        }
        if ui || (ipc.allow_control && ipc.allow_purge) {
            list.push("purge".to_string());
        }
        if ui {
            for v in Self::UI_ONLY_VERBS {
                list.push(v.to_string());
                if v == "bench" {
                    list.push("bench full".to_string());
                    list.push("bench last".to_string());
                }
            }
        }
        list
    }

    // ---- gate, audit ----

    pub fn is_locked(&self, kind: PipeKind) -> bool {
        self.lock_error(kind).is_some()
    }

    pub fn lock_error(&self, kind: PipeKind) -> Option<&'static str> {
        lock_error_parts(
            kind,
            &self.settings(),
            self.d.agent.as_ref(),
            &self.d.store,
            self.d.keys.as_ref(),
        )
    }

    pub fn status_for(&self, kind: PipeKind, st: AgentStatus) -> AgentStatus {
        status_for_parts(
            kind,
            st,
            &self.settings(),
            self.d.agent.as_ref(),
            &self.d.store,
            self.d.keys.as_ref(),
            self.d.password.as_deref(),
        )
    }

    fn key_needs_passphrase(&self) -> bool {
        self.d.keys.needs_passphrase()
    }

    fn not_open_error(&self) -> &'static str {
        if self.key_needs_passphrase() {
            Self::ERR_LOCKED
        } else {
            Self::ERR_NOT_OPEN
        }
    }

    fn uses_password(&self) -> bool {
        self.settings().security.unlock_method == "password"
    }

    fn hello_missing(&self) -> bool {
        !self.d.keys.hello_available() && self.d.keys.hello_checked()
    }

    fn no_unlock_path(&self) -> bool {
        self.settings().security.require_unlock
            && self.hello_missing()
            && (!self.uses_password()
                || !self.d.password.as_ref().is_some_and(|p| p.is_set()))
    }

    fn lock_token(&self, kind: PipeKind) -> CancelToken {
        if self.is_locked(kind) {
            return CancelToken::cancelled();
        }
        let guard = self.lock_cts.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_locked(kind) {
            CancelToken::cancelled()
        } else {
            guard.token()
        }
    }

    fn audit(&self, s: &ClientSession, verb: &str, count: i32, sha: &str) {
        if let Some(st) = (self.d.store)()
            && let Err(e) = st.append_audit(&AuditEntry {
                ts: self.now(),
                pipe: s.pipe_label().to_string(),
                client_pid: s.pid,
                client_image: s.image.clone(),
                verb: verb.to_string(),
                result_count: count,
                query_sha256: sha.to_string(),
            })
        {
            Log::warn_err("audit append failed", &e.to_string());
        }
    }

    pub fn sha256_hex(text: &str) -> String {
        hex_lower(&Sha256::digest(text.as_bytes()))
    }

    pub fn query_hash(&self, q: &str) -> String {
        if !self.d.keys.key_loaded() {
            return Self::sha256_hex(q);
        }
        let Ok(mut key) = self.d.keys.derive("betterrecall-audit-query") else {
            return String::new();
        };
        let mac = win32::hmac_sha256(&key, q.as_bytes());
        key.zeroize();
        hex_lower(&mac)
    }

    fn max_response_bytes(&self, k: PipeKind) -> usize {
        if k == PipeKind::Ui {
            Self::UI_MAX_RESPONSE_BYTES
        } else {
            self.settings().ipc.max_response_bytes.max(0) as usize
        }
    }

    // ---- search ----

    pub fn parse_search(
        rest: &str,
        default_limit: i32,
        max_limit: i32,
    ) -> Result<SearchRequest, &'static str> {
        let text = rest.trim();
        if text.is_empty() {
            return Err(Self::USAGE_SEARCH);
        }
        if !text.starts_with('{') {
            if text.encode_utf16().count() > 1024 {
                return Err("Invalid q.");
            }
            return Ok(SearchRequest {
                q: text.to_string(),
                limit: Some(default_limit),
                from: None,
                to: None,
                app: None,
                kind: None,
                mode: SearchMode::Hybrid,
                site: None,
            });
        }
        let root: Value = serde_json::from_str(text).map_err(|_| Self::ERR_JSON)?;
        let Some(obj) = root.as_object() else {
            return Err(Self::ERR_JSON);
        };
        let mut q: Option<String> = None;
        let mut limit = default_limit;
        let mut from: Option<DateTime<Utc>> = None;
        let mut to: Option<DateTime<Utc>> = None;
        let mut app: Option<String> = None;
        let mut kinds: Option<Vec<CaptureKind>> = None;
        let mut mode = SearchMode::Hybrid;
        let mut site: Option<String> = None;

        for (k, v) in obj {
            match k.as_str() {
                "q" => {
                    let Some(s) = v.as_str() else {
                        return Err("Invalid q.");
                    };
                    q = Some(s.trim().to_string());
                }
                "limit" => {
                    let Some(l) = v.as_i64() else {
                        return Err("Invalid limit.");
                    };
                    if l < 1 {
                        return Err("Invalid limit.");
                    }
                    limit = l.min(max_limit as i64) as i32;
                }
                "from" => {
                    let Some(s) = v.as_str() else {
                        return Err("Invalid from.");
                    };
                    let Some(dt) = Self::try_iso(s) else {
                        return Err("Invalid from.");
                    };
                    from = Some(dt);
                }
                "to" => {
                    let Some(s) = v.as_str() else {
                        return Err("Invalid to.");
                    };
                    let Some(dt) = Self::try_iso(s) else {
                        return Err("Invalid to.");
                    };
                    to = Some(dt);
                }
                "app" => {
                    if v.is_null() {
                        continue;
                    }
                    let Some(s) = v.as_str() else {
                        return Err("Invalid app.");
                    };
                    if s.trim().is_empty() || s.encode_utf16().count() > 260 {
                        return Err("Invalid app.");
                    }
                    app = Some(s.trim().to_string());
                }
                "site" => {
                    if v.is_null() {
                        continue;
                    }
                    let Some(s) = v.as_str() else {
                        return Err("Invalid site.");
                    };
                    let norm = EncryptedStore::normalize_site(Some(s));
                    if norm.is_empty() {
                        return Err("Invalid site.");
                    }
                    site = Some(norm);
                }
                "kind" => {
                    let Some(arr) = v.as_array() else {
                        return Err("Invalid kind.");
                    };
                    let mut ks = Vec::new();
                    for item in arr {
                        let Some(sv) = item.as_str() else {
                            return Err("Invalid kind.");
                        };
                        let ck = match sv.to_ascii_lowercase().as_str() {
                            "screen" => CaptureKind::Screen,
                            "audio" => CaptureKind::Audio,
                            "replay" => CaptureKind::Replay,
                            _ => return Err("Invalid kind."),
                        };
                        if !ks.contains(&ck) {
                            ks.push(ck);
                        }
                    }
                    if ks.is_empty() {
                        return Err("Invalid kind.");
                    }
                    kinds = Some(ks);
                }
                "mode" => {
                    let Some(sv) = v.as_str() else {
                        return Err("Invalid mode.");
                    };
                    mode = match sv.to_ascii_lowercase().as_str() {
                        "hybrid" => SearchMode::Hybrid,
                        "text" => SearchMode::Text,
                        "vector" => SearchMode::Vector,
                        _ => return Err("Invalid mode."),
                    };
                }
                _ => {}
            }
        }
        let Some(q_val) = q else {
            return Err("Invalid q.");
        };
        let q_len = q_val.encode_utf16().count();
        if q_len == 0 || q_len > 1024 {
            return Err("Invalid q.");
        }
        if let (Some(f), Some(t)) = (from, to)
            && f >= t
        {
            return Err("Invalid from.");
        }
        Ok(SearchRequest {
            q: q_val,
            limit: Some(limit),
            from,
            to,
            app,
            kind: kinds,
            mode,
            site,
        })
    }

    pub fn try_iso(s: &str) -> Option<DateTime<Utc>> {
        let trimmed = s.trim();
        if trimmed.is_empty() || !trimmed.as_bytes()[0].is_ascii_digit() {
            return None;
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
            return Some(dt.with_timezone(&Utc));
        }
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f")
            .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f"))
            .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S"))
            .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S"))
        {
            return Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc));
        }
        if let Ok(d) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
            let ndt = d.and_hms_opt(0, 0, 0)?;
            return Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc));
        }
        None
    }

    // B3: Use monotonic clock so backward wall-clock step does not pin search rate limit
    fn rate_limited(&self, s: &ClientSession) -> bool {
        if s.kind == PipeKind::Ui {
            return false;
        }
        let now = self.monotonic_now();
        let queue = if s.kind == PipeKind::Mcp {
            &self.mcp_searches
        } else {
            &self.public_searches
        };
        let mut times = queue.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(&front) = times.front() {
            if now.saturating_duration_since(front) >= Duration::from_secs(60) {
                times.pop_front();
            } else {
                break;
            }
        }
        if times.len() >= self.settings().ipc.max_search_per_minute.max(0) as usize {
            return true;
        }
        times.push_back(now);
        false
    }

    fn search(&self, s: &ClientSession, rest: &str, ct: &CancelToken) -> Reply {
        let sw = Instant::now();
        let ipc = self.settings().ipc;
        let max = if s.kind == PipeKind::Ui {
            ipc.max_results.max(Self::UI_MAX_RESULTS)
        } else {
            ipc.max_results
        };
        let req = match Self::parse_search(rest, ipc.default_results.min(max), max) {
            Ok(r) => r,
            Err(e) => return Reply::err(e),
        };
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        if self.rate_limited(s) {
            return Reply::err(Self::ERR_RATE);
        }
        let Some(mut store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };

        let mut qv: Vec<f32> = Vec::new();
        let mut degraded: Option<String> = None;
        if req.mode != SearchMode::Text {
            let worker_cold = !self.d.embedder.warm();
            let cold = worker_cold && req.mode != SearchMode::Vector;
            let wait_dur = if cold {
                Duration::from_millis(self.cold_embed_wait_ms.load(Ordering::Acquire).max(1) as u64)
            } else {
                Duration::from_secs(15)
            };
            let to = CancelSource::with_timeout(ct, wait_dur);
            match self.d.embedder.embed_query(&req.q, &to.token()) {
                Ok(v) => qv = v,
                Err(EmbedError::Cancelled) if ct.is_cancelled() => {
                    return Reply {
                        line: Envelope::error(Self::ERR_INTERNAL),
                        subscribe: Subscription::None,
                        close: true,
                        stream: None,
                    };
                }
                Err(err) => {
                    let is_timeout = matches!(err, EmbedError::Cancelled);
                    Log::warn(if req.mode == SearchMode::Vector {
                        "query embedding failed, vector search has no answer"
                    } else {
                        "query embedding failed, text-only search"
                    });
                    qv.clear();
                    degraded = Some(
                        if (cold || (req.mode == SearchMode::Vector && worker_cold)) && is_timeout {
                            "vector".to_string()
                        } else {
                            "vector-unavailable".to_string()
                        },
                    );
                }
            }
            if let Some(locked) = self.lock_error(s.kind) {
                return Reply::err(locked);
            }
            let Some(st2) = (self.d.store)() else {
                return Reply::err(self.not_open_error());
            };
            store = st2;
        }

        let mut resp = if req.mode == SearchMode::Vector && qv.is_empty() {
            SearchResponse {
                count: 0,
                truncated: false,
                took_ms: 0,
                results: Vec::new(),
                degraded: None,
            }
        } else {
            match store.search(&req, &qv, max, self.max_response_bytes(s.kind)) {
                Ok(r) => r,
                Err(e) => {
                    Log::error_err("search failed", &e.to_string());
                    return Reply::err(Self::ERR_INTERNAL);
                }
            }
        };
        if degraded.is_some() {
            resp.degraded = degraded;
        }
        let weak_below = self.settings().index.weak_match_below;
        if weak_below > 0.0 {
            for h in &mut resp.results {
                h.weak = !h.text_match && f64::from(h.cosine.unwrap_or(0.0)) < weak_below;
            }
        }
        resp.took_ms = sw.elapsed().as_millis() as i64;
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        self.audit(s, "search", resp.count, &self.query_hash(&req.q));
        Reply::ok(CoreJson::to_string(&resp).unwrap())
    }

    // ---- get, thumb, keyframe, timeline ----

    fn get_capture(&self, s: &ClientSession, args: &[&str]) -> Reply {
        if args.len() != 1 {
            return Reply::err(Self::USAGE_GET);
        }
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let Some(store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };
        let d = match store.get(args[0]) {
            Ok(v) => v,
            Err(_) => return Reply::err(Self::ERR_INTERNAL),
        };
        self.audit(s, "get", i32::from(d.is_some()), "");
        let Some(detail) = d else {
            return Reply::err(Self::ERR_NOT_FOUND);
        };
        let budget = self.max_response_bytes(s.kind).saturating_sub(32);
        Reply::ok(Self::fit_detail(&detail, budget))
    }

    pub fn fit_detail(d: &CaptureDetail, budget: usize) -> String {
        let mut json = CoreJson::to_string(d).unwrap();
        if json.len() <= budget {
            return json;
        }
        let mut cur = d.clone();
        for _ in 0..24 {
            if cur.text.is_empty() || json.len() <= budget {
                break;
            }
            let over = json.len() - budget;
            let char_count = cur.text.chars().count();
            let cut = (over / 2 + 1).clamp(1, char_count);
            cur.text = cur.text.chars().take(char_count - cut).collect();
            json = CoreJson::to_string(&cur).unwrap();
        }
        json
    }

    fn thumb(&self, s: &ClientSession, args: &[&str], ct: &CancelToken) -> Reply {
        if s.kind == PipeKind::Public && !self.settings().ipc.allow_thumbnails {
            return Reply::err(Self::ERR_THUMBS_OFF);
        }
        if args.is_empty() || args.len() > 2 {
            return Reply::err(Self::USAGE_THUMB);
        }
        let mut width = 320i32;
        if args.len() == 2 {
            if !args[1].bytes().all(|b| b.is_ascii_digit()) {
                return Reply::err(Self::USAGE_THUMB);
            }
            let Ok(w) = args[1].parse::<i32>() else {
                return Reply::err(Self::USAGE_THUMB);
            };
            if !(64..=1280).contains(&w) {
                return Reply::err("Invalid width.");
            }
            width = w;
        }
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let t = self.d.thumbnailer.get_thumb(args[0], width, ct);
        if s.kind != PipeKind::Ui {
            self.audit(s, "thumb", i32::from(t.is_some()), "");
        }
        let Some(thumb) = t else {
            return Reply::err(Self::ERR_NOT_FOUND);
        };
        Reply::ok(CoreJson::to_string(&thumb).unwrap())
    }

    fn keyframe(&self, s: &ClientSession, args: &[&str]) -> Reply {
        if args.len() != 1 {
            return Reply::err(Self::USAGE_KEYFRAME);
        }
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let Some(store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };
        let t = match store.get_keyframe(args[0]) {
            Ok(v) => v,
            Err(_) => return Reply::err(Self::ERR_INTERNAL),
        };
        self.audit(s, "keyframe", i32::from(t.is_some()), "");
        let Some(kf) = t else {
            return Reply::err(Self::ERR_NOT_FOUND);
        };
        let mut ui = self.d.thumbnailer.transcode(kf, "ui");
        let reply = Reply::ok(CoreJson::to_string(&ui).unwrap());
        ui.bytes.zeroize();
        reply
    }

    fn export(&self, s: &ClientSession, rest: &str, ct: &CancelToken) -> Reply {
        if rest.is_empty() {
            return Reply::err(Self::USAGE_EXPORT);
        }
        let r: ExportRequest = match serde_json::from_str(rest) {
            Ok(v) => v,
            Err(_) => return Reply::err(Self::ERR_JSON),
        };
        if r.from >= r.to {
            return Reply::err("Invalid from.");
        }
        if !r.dry_run
            && (r.path.trim().is_empty()
                || !is_abs_path(&r.path)
                || !r.path.to_ascii_lowercase().ends_with(".zip")
                || r.path.chars().any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*')))
        {
            return Reply::err("Invalid path.");
        }
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let Some(store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };
        if r.dry_run {
            return match CaptureExporter::count(&store, &r) {
                Ok(res) => Reply::ok(CoreJson::to_string(&res).unwrap()),
                Err(_) => Reply::err(Self::ERR_INTERNAL),
            };
        }

        let lock_tok = self.lock_token(s.kind);
        let linked = CancelSource::linked2(ct, &lock_tok);
        let linked_tok = linked.token();
        let cancel_flag = Arc::new(AtomicBool::new(linked_tok.is_cancelled()));
        let cancel_flag_cb = cancel_flag.clone();
        let _reg = linked_tok.on_cancel(move || cancel_flag_cb.store(true, Ordering::Release));

        let thumbs = self.d.thumbnailer.clone();
        struct ActiveExportGuard<'a>(&'a AtomicUsize);
        impl Drop for ActiveExportGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        self.active_exports.fetch_add(1, Ordering::AcqRel);
        let _export_guard = ActiveExportGuard(&self.active_exports);

        let is_locked = || self.is_locked(s.kind);
        let res = CaptureExporter::write_with_lock(
            &store,
            &r,
            move |kf, target| thumbs.transcode(kf, target),
            Some(is_locked),
            Some(&cancel_flag),
        );
        match res {
            Ok(result) => {
                if self.is_locked(s.kind) {
                    let _ = fs::remove_file(&r.path);
                    return Reply::err(Self::ERR_LOCKED);
                }
                self.audit(s, "export", result.count, "");
                Reply::ok(CoreJson::to_string(&result).unwrap())
            }
            Err(StoreError::ExportLocked) => Reply::err(Self::ERR_LOCKED),
            Err(StoreError::Cancelled) if !ct.is_cancelled() => Reply::err(Self::ERR_LOCKED),
            Err(StoreError::Cancelled) => Reply {
                line: Envelope::error(Self::ERR_INTERNAL),
                subscribe: Subscription::None,
                close: true,
                stream: None,
            },
            Err(StoreError::ExportPurged) => {
                Log::info("export aborted: a purge ran during it");
                Reply::err(Self::ERR_PURGED_DURING)
            }
            Err(StoreError::Io(e)) => {
                Log::warn_err("export failed", &e.to_string());
                Reply::err("Export failed.")
            }
            Err(StoreError::DirectoryNotFound) => {
                Log::warn("export failed: directory not found");
                Reply::err("Export failed.")
            }
            Err(e) => {
                Log::error_err("export failed", &e.to_string());
                Reply::err(Self::ERR_INTERNAL)
            }
        }
    }

    fn timeline(&self, s: &ClientSession, args: &[&str]) -> Reply {
        if !(2..=3).contains(&args.len()) {
            return Reply::err(Self::USAGE_TIMELINE);
        }
        let (Some(from), Some(to)) = (Self::try_iso(args[0]), Self::try_iso(args[1])) else {
            return Reply::err(Self::USAGE_TIMELINE);
        };
        let mut limit = 500i32;
        if args.len() == 3 {
            if !args[2].bytes().all(|b| b.is_ascii_digit()) {
                return Reply::err(Self::USAGE_TIMELINE);
            }
            let Ok(l) = args[2].parse::<i32>() else {
                return Reply::err(Self::USAGE_TIMELINE);
            };
            if l < 1 {
                return Reply::err("Invalid limit.");
            }
            limit = l.min(5000);
        }
        if from >= to {
            return Reply::err("Invalid from.");
        }
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let Some(store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };
        let Ok(items) = store.timeline(from, to, limit) else {
            return Reply::err(Self::ERR_INTERNAL);
        };
        self.audit(s, "timeline", items.len() as i32, "");
        Reply::ok(CoreJson::to_string(&ItemsReply { items }).unwrap())
    }

    fn audit_verb(&self, s: &ClientSession, args: &[&str]) -> Reply {
        let mut limit = 200i32;
        if args.len() > 1 {
            return Reply::err(Self::USAGE_AUDIT);
        }
        if args.len() == 1 {
            if !args[0].bytes().all(|b| b.is_ascii_digit()) {
                return Reply::err(Self::USAGE_AUDIT);
            }
            let Ok(l) = args[0].parse::<i32>() else {
                return Reply::err(Self::USAGE_AUDIT);
            };
            limit = l;
        }
        if !(1..=10_000).contains(&limit) {
            return Reply::err("Invalid limit.");
        }
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let Some(store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };
        let Ok(items) = store.read_audit(limit) else {
            return Reply::err(Self::ERR_INTERNAL);
        };
        Reply::ok(CoreJson::to_string(&ItemsReply { items }).unwrap())
    }

    fn facets(&self, s: &ClientSession, args: &[&str]) -> Reply {
        let (from, to) = if args.len() == 2 {
            let (Some(f), Some(t)) = (Self::try_iso(args[0]), Self::try_iso(args[1])) else {
                return Reply::err(Self::USAGE_FACETS);
            };
            if f >= t {
                return Reply::err("Invalid from.");
            }
            (Some(f), Some(t))
        } else if args.is_empty() {
            (None, None)
        } else {
            return Reply::err(Self::USAGE_FACETS);
        };
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let Some(store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };
        let Ok(r): Result<FacetsReply, _> = store.facets(from, to, 30) else {
            return Reply::err(Self::ERR_INTERNAL);
        };
        Reply::ok(CoreJson::to_string(&r).unwrap())
    }

    fn history(&self, s: &ClientSession, rest: &str) -> Reply {
        let (sub, text) = split_verb(rest);
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let Some(store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };
        let p = self.settings().privacy;
        let max = p.search_history_max.clamp(10, 1000);
        match sub.to_ascii_lowercase().as_str() {
            "" => {
                let items: Vec<HistoryEntry> = if p.search_history {
                    store.history(max).unwrap_or_default()
                } else {
                    Vec::new()
                };
                Reply::ok(CoreJson::to_string(&ItemsReply { items }).unwrap())
            }
            "add" if !text.trim().is_empty() && text.trim().encode_utf16().count() <= 1024 => {
                if p.search_history {
                    let _ = store.add_history(text.trim(), self.now(), max);
                }
                Reply::ok("null".to_string())
            }
            "delete" if !text.trim().is_empty() => {
                let _ = store.delete_history(Some(text.trim()));
                Reply::ok("null".to_string())
            }
            "clear" if text.is_empty() => {
                let _ = store.delete_history(None);
                Reply::ok("null".to_string())
            }
            _ => Reply::err(Self::USAGE_HISTORY),
        }
    }

    // ---- subscribe, pause ----

    fn subscribe(&self, s: &ClientSession, args: &[&str]) -> Reply {
        if args.is_empty() {
            return Reply {
                line: Envelope::ok(&SubscribedStatus { subscribed: true }).unwrap(),
                subscribe: Subscription::Status,
                close: false,
                stream: None,
            };
        }
        if args.len() == 1 && args[0].eq_ignore_ascii_case("captures") {
            if let Some(locked) = self.lock_error(s.kind) {
                return Reply::err(locked);
            }
            self.audit(s, "subscribe captures", 0, "");
            return Reply {
                line: Envelope::ok(&SubscribedCaptures {
                    subscribed: "captures".to_string(),
                })
                .unwrap(),
                subscribe: Subscription::Captures,
                close: false,
                stream: None,
            };
        }
        Reply::err(Self::USAGE_SUBSCRIBE)
    }

    fn pause(&self, s: &ClientSession, args: &[&str]) -> Reply {
        if s.kind == PipeKind::Public && !self.settings().ipc.allow_control {
            return Reply::err(Self::ERR_CONTROL_OFF);
        }
        if args.len() > 1 {
            return Reply::err(Self::USAGE_PAUSE);
        }
        let mut duration = None;
        if args.len() == 1 {
            if !args[0].bytes().all(|b| b.is_ascii_digit()) {
                return Reply::err(Self::USAGE_PAUSE);
            }
            let Ok(sec) = args[0].parse::<i64>() else {
                return Reply::err(Self::USAGE_PAUSE);
            };
            let max = if s.kind == PipeKind::Ui {
                604_800i64
            } else {
                self.settings().ipc.max_pause_s as i64
            };
            if sec < 1 || sec > max {
                return Reply::err("Invalid seconds.");
            }
            duration = Some(Duration::from_secs(sec as u64));
        }
        self.d
            .agent
            .pause(duration, if s.kind == PipeKind::Ui { "user" } else { "ipc" });
        Reply::ok("null".to_string())
    }

    // ---- purge ----

    fn new_purge_token(&self, s: &mut ClientSession, scope: Option<PurgeScope>) -> String {
        let now = self.now();
        s.purge_tokens.retain(|_, v| v.expires > now);
        let mut bytes = [0u8; 16];
        win32::fill_random(&mut bytes);
        let token = hex_lower(&bytes);
        let span = scope.as_ref().and_then(|sc| {
            if sc.kind == "last"
                && let (Some(f), Some(t)) = (sc.from, sc.to)
            {
                Some(t.signed_duration_since(f))
            } else {
                None
            }
        });
        s.purge_tokens.insert(
            token.clone(),
            PurgeTicket {
                scope,
                expires: now + ChronoDuration::seconds(30),
                span,
            },
        );
        token
    }

    fn purge(&self, s: &mut ClientSession, args: &[&str], ct: &CancelToken) -> Reply {
        let ui = s.kind == PipeKind::Ui;
        if !ui {
            if !self.settings().ipc.allow_control {
                return Reply::err(Self::ERR_CONTROL_OFF);
            }
            if !self.settings().ipc.allow_purge {
                return Reply::err(Self::ERR_PURGE_OFF);
            }
            if let Some(locked) = self.lock_error(s.kind) {
                return Reply::err(locked);
            }
        }
        let usage = if ui {
            Self::USAGE_PURGE_UI
        } else {
            Self::USAGE_PURGE
        };
        if args.is_empty() {
            return Reply::err(usage);
        }
        let sub = args[0].to_ascii_lowercase();
        if ui
            && sub != "all"
            && sub != "confirm"
            && let Some(locked) = self.lock_error(s.kind)
        {
            return Reply::err(locked);
        }

        let scope = match sub.as_str() {
            "confirm" => {
                if args.len() != 2 {
                    return Reply::err(usage);
                }
                let now = self.now();
                let Some(ticket) = s.purge_tokens.remove(args[1]) else {
                    return Reply::err(Self::ERR_TOKEN);
                };
                if ticket.expires <= now {
                    return Reply::err(Self::ERR_TOKEN);
                }
                if ui
                    && ticket.scope.is_some()
                    && let Some(locked) = self.lock_error(s.kind)
                {
                    return Reply::err(locked);
                }
                let Some(sc) = ticket.scope else {
                    let all = (self.d.purge_all)(ct);
                    if let Some(ref chat) = self.chat {
                        chat.clear();
                    }
                    let undeleted = self
                        .d
                        .purge_all_undeleted
                        .as_ref()
                        .map(|f| f())
                        .filter(|&v| v > 0);
                    return Reply::ok(
                        CoreJson::to_string(&PurgeResult {
                            purged: all,
                            undeleted,
                        })
                        .unwrap(),
                    );
                };
                let Some(st) = (self.d.store)() else {
                    return Reply::err(self.not_open_error());
                };
                let scope2 = match ticket.span {
                    Some(span) => PurgeScope::last(span),
                    None => sc,
                };
                let audit_queries = if scope2.kind == "app" || scope2.kind == "site" {
                    Some(audit_queries_for(&st, &scope2))
                } else {
                    None
                };
                let Ok(n) = st.purge(&scope2) else {
                    return Reply::err(Self::ERR_INTERNAL);
                };
                if let Some(queries) = audit_queries
                    && !queries.is_empty()
                {
                    let mut hashes: Vec<String> = Vec::new();
                    let mut seen = HashSet::new();
                    for q in queries {
                        for h in [self.query_hash(&q), Self::sha256_hex(&q)] {
                            if !h.is_empty() && seen.insert(h.clone()) {
                                hashes.push(h);
                            }
                        }
                    }
                    let _ = st.clear_audit_query_hashes(&hashes);
                }
                if let Some(ref chat) = self.chat {
                    let window = scope2.kind == "last" || scope2.kind == "range";
                    chat.forget_captures(
                        |id| st.get(id).ok().flatten().is_none(),
                        if window { scope2.from } else { None },
                        if window { scope2.to } else { None },
                    );
                }
                self.audit(s, "purge", n as i32, "");
                return Reply::ok(
                    CoreJson::to_string(&PurgeResult {
                        purged: n as i32,
                        undeleted: None,
                    })
                    .unwrap(),
                );
            }
            "all" if ui => {
                if args.len() != 1 {
                    return Reply::err(usage);
                }
                let now = self.now();
                let last_nh = *self
                    .last_no_hello_purge
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let no_hello_purge = self.hello_missing()
                    && (self.d.store)().is_none()
                    && (!self.d.keys.key_loaded() || self.key_needs_passphrase())
                    && now.signed_duration_since(last_nh) <= ChronoDuration::seconds(60);
                let last_u = *self.last_unlock.lock().unwrap_or_else(|e| e.into_inner());
                if !no_hello_purge && now.signed_duration_since(last_u) > ChronoDuration::seconds(60)
                {
                    return Reply::err(Self::ERR_UNLOCK_REQUIRED);
                }
                let confirm = self.new_purge_token(s, None);
                return Reply::ok(
                    CoreJson::to_string(&PurgeConfirmReply {
                        confirm,
                        rows: None,
                    })
                    .unwrap(),
                );
            }
            "last" => {
                if args.len() == 2
                    && !args[1].is_empty()
                    && args[1].bytes().all(|b| b.is_ascii_digit())
                    && args[1].parse::<i64>().is_err()
                {
                    return Reply::err("Invalid seconds.");
                }
                if args.len() != 2 || !args[1].bytes().all(|b| b.is_ascii_digit()) {
                    return Reply::err(usage);
                }
                let Ok(secs) = args[1].parse::<i64>() else {
                    return Reply::err(usage);
                };
                if !(1..=3650 * 86_400).contains(&secs) {
                    return Reply::err("Invalid seconds.");
                }
                PurgeScope::last(ChronoDuration::seconds(secs))
            }
            "range" => {
                if args.len() != 3 {
                    return Reply::err(usage);
                }
                let (Some(from), Some(to)) = (Self::try_iso(args[1]), Self::try_iso(args[2]))
                else {
                    return Reply::err(usage);
                };
                if from >= to {
                    return Reply::err("Invalid from.");
                }
                PurgeScope::range(from, to)
            }
            "app" => {
                if args.len() < 2 {
                    return Reply::err(usage);
                }
                let app_name = Self::purge_app_name(&args[1..].join(" "));
                if app_name.is_empty() {
                    return Reply::err(usage);
                }
                PurgeScope::for_app(app_name)
            }
            "site" => {
                if args.len() != 2 {
                    return Reply::err(usage);
                }
                let host = Self::purge_site_host(args[1]);
                if EncryptedStore::normalize_site(Some(&host)).is_empty() {
                    return Reply::err(usage);
                }
                PurgeScope::for_site(host)
            }
            "id" => {
                if args.len() != 2 || args[1].len() > 64 {
                    return Reply::err(usage);
                }
                PurgeScope::for_id(args[1])
            }
            _ => return Reply::err(usage),
        };

        let Some(store) = (self.d.store)() else {
            return Reply::err(self.not_open_error());
        };
        let Ok(rows) = store.count_purge(&scope) else {
            return Reply::err(Self::ERR_INTERNAL);
        };
        let confirm = self.new_purge_token(s, Some(scope));
        Reply::ok(
            CoreJson::to_string(&PurgeConfirmReply {
                confirm,
                rows: Some(rows as i32),
            })
            .unwrap(),
        )
    }

    // ---- unlock ----

    fn unlock(&self, s: &ClientSession, rest: &str, ct: &CancelToken) -> Reply {
        let keys = &self.d.keys;
        let was_unlocked = self.d.agent.is_unlocked();
        let mut rewrap = false;
        let mut hello_fallback = false;
        let mut passphrase_checked = false;

        let uses_pw = self.uses_password();
        let key_needs = self.key_needs_passphrase();
        let key_prot_setting = self.settings().security.key_protection;
        let require_passphrase = !uses_pw
            && (key_needs
                || (keys.key_loaded()
                    && (keys.protection() == KeyVault::MODE_PASSPHRASE
                        || key_prot_setting == KeyVault::MODE_PASSPHRASE)));

        let passphrase_try = !rest.is_empty()
            && (key_needs
                || (!uses_pw
                    && keys.key_loaded()
                    && keys.protection() == KeyVault::MODE_PASSPHRASE));
        if passphrase_try {
            let pass_wait = keys.passphrase_retry_after_seconds();
            if pass_wait > 0 {
                return Reply::err(&Self::err_too_many(pass_wait));
            }
        }
        if !rest.is_empty()
            && (key_needs || (!uses_pw && !self.hello_missing()))
            && keys.would_set_passphrase()
            && !KeyVault::valid_new_passphrase(rest)
        {
            return Reply::err(Self::ERR_PASSPHRASE_RULE);
        }

        if key_needs {
            if !rest.is_empty() {
                if !keys.unlock_with_passphrase(rest) {
                    return Reply::err(Self::ERR_WRONG_PASSPHRASE);
                }
                passphrase_checked = true;
                if uses_pw {
                    self.unlocked_with_passphrase.store(true, Ordering::Release);
                    return self.open_verified(s, "unlock passphrase");
                }
            }
        } else if uses_pw {
            let check = self
                .d
                .password
                .as_ref()
                .map_or(PasswordCheck::NotSet, |pw| pw.verify(Some(rest)));
            match check {
                PasswordCheck::NotSet => {
                    Log::warn(
                        "unlock password file missing or damaged: trying Windows Hello instead",
                    );
                    hello_fallback = true;
                }
                PasswordCheck::Throttled => {
                    let wait = self
                        .d
                        .password
                        .as_ref()
                        .map_or(1, |p| p.retry_after_seconds().max(1));
                    return Reply::err(&Self::err_too_many(wait));
                }
                PasswordCheck::Wrong => {
                    self.audit(s, "unlock password", 0, "");
                    let wait = self
                        .d
                        .password
                        .as_ref()
                        .map_or(0, |p| p.retry_after_seconds());
                    return if wait > 0 {
                        Reply::err(&Self::err_too_many(wait))
                    } else {
                        Reply::err(Self::ERR_WRONG_PASSWORD)
                    };
                }
                PasswordCheck::Ok => {}
            }
            if !hello_fallback {
                self.unlocked_with_passphrase.store(false, Ordering::Release);
                return self.open_verified(s, "unlock password");
            }
        } else if !rest.is_empty() && keys.key_loaded() {
            if keys.protection() == KeyVault::MODE_PASSPHRASE {
                if !keys.unlock_with_passphrase(rest) {
                    return Reply::err(Self::ERR_WRONG_PASSPHRASE);
                }
                passphrase_checked = true;
            } else {
                rewrap = key_prot_setting == KeyVault::MODE_PASSPHRASE;
            }
        }

        if !self.d.agent.unlock(ct) {
            if hello_fallback {
                return Reply::err(Self::ERR_NO_PASSWORD);
            }
            if (self.settings().security.require_unlock
                || key_needs
                || (rest.is_empty() && !keys.key_loaded()))
                && self.hello_missing()
            {
                return self.hello_missing_unlock(s, rest, passphrase_checked);
            }
            return Reply::err(Self::ERR_UNLOCK_CANCELLED);
        }

        if rewrap && !keys.unlock_with_passphrase(rest) {
            return Reply::err(Self::ERR_WRONG_PASSPHRASE);
        }
        if hello_fallback {
            self.audit(s, "unlock hello (no password file)", 1, "");
        }
        self.unlocked_with_passphrase
            .store(passphrase_checked || rewrap, Ordering::Release);
        *self.last_unlock.lock().unwrap_or_else(|e| e.into_inner()) = self.now();

        if (key_needs || require_passphrase) && rest.is_empty() {
            if !was_unlocked && self.d.agent.is_unlocked() {
                self.d.agent.lock();
            }
            return Reply::ok(
                CoreJson::to_string(&UnlockReply {
                    unlocked: false,
                    until: None,
                })
                .unwrap(),
            );
        }
        let until = self.d.agent.get_status().unlocked_until;
        Reply::ok(
            CoreJson::to_string(&UnlockReply {
                unlocked: true,
                until,
            })
            .unwrap(),
        )
    }

    fn hello_missing_unlock(
        &self,
        s: &ClientSession,
        rest: &str,
        passphrase_checked: bool,
    ) -> Reply {
        if passphrase_checked {
            self.unlocked_with_passphrase.store(true, Ordering::Release);
            return self.open_verified(s, "unlock passphrase");
        }
        let pw = self.d.password.as_deref();
        if rest.is_empty() {
            if (self.d.store)().is_none()
                && (!self.d.keys.key_loaded() || self.key_needs_passphrase())
            {
                *self
                    .last_no_hello_purge
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = self.now();
            }
            return if pw.is_some_and(UnlockPassword::is_set) {
                Reply::err(Self::ERR_HELLO_MISSING)
            } else {
                Reply::err(Self::ERR_NO_PASSWORD)
            };
        }
        let Some(pw) = pw.filter(|p| p.is_set()) else {
            return Reply::err(Self::ERR_NO_PASSWORD);
        };
        match pw.verify(Some(rest)) {
            PasswordCheck::NotSet => return Reply::err(Self::ERR_NO_PASSWORD),
            PasswordCheck::Throttled => {
                return Reply::err(&Self::err_too_many(pw.retry_after_seconds().max(1)));
            }
            PasswordCheck::Wrong => {
                self.audit(s, "unlock password", 0, "");
                let wait = pw.retry_after_seconds();
                return if wait > 0 {
                    Reply::err(&Self::err_too_many(wait))
                } else {
                    Reply::err(Self::ERR_WRONG_PASSWORD)
                };
            }
            PasswordCheck::Ok => {}
        }
        self.unlocked_with_passphrase.store(false, Ordering::Release);
        self.open_verified(s, "unlock password (no Hello)")
    }

    fn open_verified(&self, s: &ClientSession, what: &str) -> Reply {
        self.d.agent.open_read_gate();
        *self.last_unlock.lock().unwrap_or_else(|e| e.into_inner()) = self.now();
        self.audit(s, what, 1, "");
        let until = self.d.agent.get_status().unlocked_until;
        Reply::ok(
            CoreJson::to_string(&UnlockReply {
                unlocked: true,
                until,
            })
            .unwrap(),
        )
    }

    // ---- password, passphrase ----

    fn password_verb(&self, s: &ClientSession, rest: &str) -> Reply {
        let pw = self.d.password.as_deref();
        let (sub, json) = split_verb(rest);
        match sub.to_ascii_lowercase().as_str() {
            "" if json.is_empty() => Reply::ok(
                CoreJson::to_string(&br_contracts::PasswordStatus {
                    set: pw.is_some_and(UnlockPassword::is_set),
                    hello_available: self.d.keys.hello_available(),
                    retry_after_s: pw.map_or(0, UnlockPassword::retry_after_seconds),
                })
                .unwrap(),
            ),
            "set" | "clear" if !json.is_empty() => {
                let Some(pw) = pw else {
                    return Reply::err(Self::ERR_NO_PASSWORD);
                };
                let Ok(root) = serde_json::from_str::<Value>(json) else {
                    return Reply::err(Self::ERR_JSON);
                };
                if !root.is_object() {
                    return Reply::err(Self::ERR_JSON);
                }
                let clear = sub.eq_ignore_ascii_case("clear");
                if pw.is_set() {
                    match pw.verify(json_str(&root, "current")) {
                        PasswordCheck::Throttled => {
                            return Reply::err(&Self::err_too_many(
                                pw.retry_after_seconds().max(1),
                            ));
                        }
                        PasswordCheck::Wrong => {
                            self.audit(s, if clear { "password clear" } else { "password set" }, 0, "");
                            let wait = pw.retry_after_seconds();
                            return if wait > 0 {
                                Reply::err(&Self::err_too_many(wait))
                            } else {
                                Reply::err(Self::ERR_WRONG_PASSWORD)
                            };
                        }
                        PasswordCheck::Ok | PasswordCheck::NotSet => {}
                    }
                } else if clear {
                    return Reply::err(Self::ERR_NO_PASSWORD);
                } else if self.settings().security.require_unlock
                    && !self.d.agent.is_unlocked()
                    && (self.d.keys.hello_available() || !self.d.keys.hello_checked())
                {
                    return Reply::err(Self::ERR_UNLOCK_REQUIRED);
                }

                if clear {
                    if self.uses_password() {
                        return Reply::err(Self::ERR_PASSWORD_IN_USE);
                    }
                    // E2: return error reply and audit 0 if delete fails
                    if let Err(e) = pw.clear() {
                        Log::warn_err("password clear failed", &e.to_string());
                        self.audit(s, "password clear", 0, "");
                        return Reply::err(Self::ERR_PASSWORD_REMOVE);
                    }
                    self.audit(s, "password clear", 1, "");
                    return Reply::ok("null".to_string());
                }
                let Some(next) = json_str(&root, "new") else {
                    return Reply::err(Self::ERR_PASSWORD_RULE);
                };
                if !UnlockPassword::valid_new(next) {
                    return Reply::err(Self::ERR_PASSWORD_RULE);
                }
                // E2: return error reply and audit 0 if write fails
                if let Err(e) = pw.set(next) {
                    Log::warn_err("password set failed", &e.to_string());
                    self.audit(s, "password set", 0, "");
                    return Reply::err(Self::ERR_PASSWORD_SAVE);
                }
                self.audit(s, "password set", 1, "");
                Reply::ok("null".to_string())
            }
            _ => Reply::err(Self::USAGE_PASSWORD),
        }
    }

    fn passphrase_verb(&self, s: &ClientSession, rest: &str) -> Reply {
        let (sub, json) = split_verb(rest);
        if !sub.eq_ignore_ascii_case("verify") || json.is_empty() {
            return Reply::err(Self::USAGE_PASSPHRASE);
        }
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        let Ok(root) = serde_json::from_str::<Value>(json) else {
            return Reply::err(Self::ERR_JSON);
        };
        if !root.is_object() {
            return Reply::err(Self::ERR_JSON);
        }
        let Some(pass) = json_str(&root, "passphrase") else {
            return Reply::err(Self::USAGE_PASSPHRASE);
        };
        let keys = &self.d.keys;
        let wait = keys.passphrase_retry_after_seconds();
        if wait > 0 {
            return Reply::err(&Self::err_too_many(wait));
        }
        if !keys.key_loaded() {
            return Reply::err(Self::ERR_NOT_OPEN);
        }
        if keys.protection() != KeyVault::MODE_PASSPHRASE {
            return Reply::ok("null".to_string());
        }
        if !keys.unlock_with_passphrase(pass) {
            self.audit(s, "unlock passphrase", 0, "");
            let after = keys.passphrase_retry_after_seconds();
            return if after > 0 {
                Reply::err(&Self::err_too_many(after))
            } else {
                Reply::err(Self::ERR_WRONG_PASSPHRASE)
            };
        }
        self.unlocked_with_passphrase.store(true, Ordering::Release);
        self.audit(s, "unlock passphrase", 1, "");
        Reply::ok("null".to_string())
    }

    // ---- ai, describe ----

    fn ai_state(&self) -> AiStatus {
        let a = self.settings().ai;
        let usage = if !self.is_locked(PipeKind::Ui) {
            (self.d.store)().map(|st| AiBudget::usage(&st, &a, self.now()))
        } else {
            None
        };
        let endpoint = AiDefaults::effective_endpoint(&a);
        let local = ProviderHttp::is_loopback(&endpoint);
        AiStatus {
            enabled: a.enabled,
            key_set: self.d.ai_key.as_ref().is_some_and(|k| k.read(&a).is_some()),
            provider: a.provider.clone(),
            model: AiDefaults::effective_model(&a),
            endpoint,
            usage,
            local,
        }
    }

    fn ai_verb(&self, s: &ClientSession, rest: &str) -> Reply {
        let (sub, tail) = split_verb(rest);
        match sub.to_ascii_lowercase().as_str() {
            "" if tail.is_empty() => Reply::ok(CoreJson::to_string(&self.ai_state()).unwrap()),
            "setkey" if !tail.is_empty() => {
                let Some(ref store) = self.d.ai_key else {
                    return Reply::err(Self::ERR_AI_OFF);
                };
                if let Some(locked) = self.lock_error(s.kind) {
                    return Reply::err(locked);
                }
                if !ApiKeyStore::valid(Some(tail)) {
                    return Reply::err(Self::ERR_API_KEY_RULE);
                }
                if store.set(tail, &self.settings().ai).is_err() {
                    return Reply::err(Self::ERR_INTERNAL);
                }
                self.audit(s, "ai setkey", 1, "");
                Reply::ok(CoreJson::to_string(&self.ai_state()).unwrap())
            }
            "clearkey" if tail.is_empty() => {
                if let Some(locked) = self.lock_error(s.kind) {
                    return Reply::err(locked);
                }
                if let Some(ref store) = self.d.ai_key {
                    store.clear();
                }
                self.audit(s, "ai clearkey", 1, "");
                Reply::ok(CoreJson::to_string(&self.ai_state()).unwrap())
            }
            _ => Reply::err(Self::USAGE_AI),
        }
    }

    fn describe(&self, s: &mut ClientSession, args: &[&str], ct: &CancelToken) -> Reply {
        if args.len() != 2 {
            return Reply::err(Self::USAGE_DESCRIBE);
        }
        let ai = self.settings().ai;
        let (Some(ai_key), Some(describer)) = (self.d.ai_key.as_ref(), self.d.describer.as_ref())
        else {
            return Reply::err(Self::ERR_AI_OFF);
        };
        if !ai.enabled {
            return Reply::err(Self::ERR_AI_OFF);
        }
        let key_needed = ai.provider != AiDefaults::OPEN_AI;
        let now = self.now();
        s.describe_tokens.retain(|_, v| v.expires > now);

        match args[0].to_ascii_lowercase().as_str() {
            "preview" => {
                if key_needed && ai_key.read(&ai).is_none() {
                    return Reply::err(Self::ERR_NO_API_KEY);
                }
                if let Some(locked) = self.lock_error(s.kind) {
                    return Reply::err(locked);
                }
                let Some(store) = (self.d.store)() else {
                    return Reply::err(self.not_open_error());
                };
                let id = args[1];
                let mut img = store.get_keyframe(id).ok().flatten();
                if let Some(ref kf) = img
                    && !Self::SENDABLE_MIMES.contains(&kf.mime.as_str())
                {
                    img = self.d.thumbnailer.get_thumb(id, 1280, ct);
                }
                let Some(img) = img else {
                    return Reply::err(Self::ERR_NO_IMAGE);
                };
                let outgoing = OutgoingImage {
                    bytes: img.bytes,
                    mime: img.mime,
                    width: img.width,
                    height: img.height,
                    redacted: false,
                };
                let redactor: &dyn DescribeRedactor = self
                    .d
                    .redactor
                    .as_deref()
                    .unwrap_or(&PassThroughRedactor);
                let outgoing = match redactor.redact(id, outgoing, ct) {
                    Ok(o) => o,
                    Err(DescribeError::Message(msg)) => {
                        self.audit(s, "describe preview", 0, "");
                        return Reply::err(&msg);
                    }
                    Err(e) => {
                        self.audit(s, "describe preview", 0, "");
                        return Reply::err(&e.to_string());
                    }
                };
                while s.describe_tokens.len() >= 4 {
                    if let Some(oldest_k) = s
                        .describe_tokens
                        .iter()
                        .min_by_key(|(_, v)| v.expires)
                        .map(|(k, _)| k.clone())
                    {
                        s.describe_tokens.remove(&oldest_k);
                    }
                }
                let mut tok_bytes = [0u8; 16];
                win32::fill_random(&mut tok_bytes);
                let token = hex_lower(&tok_bytes);
                let endpoint = AiDefaults::effective_endpoint(&ai);
                let model = AiDefaults::effective_model(&ai);
                let preview = DescribePreview {
                    token: token.clone(),
                    provider: ai.provider.clone(),
                    model: model.clone(),
                    endpoint: endpoint.clone(),
                    mime: outgoing.mime.clone(),
                    width: outgoing.width,
                    height: outgoing.height,
                    redacted: outgoing.redacted,
                    bytes: outgoing.bytes.clone(),
                };
                s.describe_tokens.insert(
                    token,
                    DescribeTicket {
                        capture_id: id.to_string(),
                        image: outgoing,
                        expires: now + ChronoDuration::seconds(120),
                        provider: ai.provider,
                        endpoint,
                        model,
                    },
                );
                self.audit(s, "describe preview", 1, "");
                Reply::ok(CoreJson::to_string(&preview).unwrap())
            }
            "send" => {
                let Some(ticket) = s.describe_tokens.remove(args[1]) else {
                    return Reply::err(Self::ERR_TOKEN);
                };
                if ticket.expires <= now {
                    return Reply::err(Self::ERR_TOKEN);
                }
                if let Some(locked) = self.lock_error(s.kind) {
                    return Reply::err(locked);
                }
                if !ticket.matches(&ai) {
                    return Reply::err(Self::ERR_DESCRIBE_CHANGED);
                }
                let key = ai_key.read(&ai);
                if key_needed && key.is_none() {
                    return Reply::err(Self::ERR_NO_API_KEY);
                }
                let verb = format!("describe {}", ai.provider);
                let sha = hex_lower(&Sha256::digest(&ticket.image.bytes));
                let Some(store) = (self.d.store)() else {
                    return Reply::err(self.not_open_error());
                };
                if store.get(&ticket.capture_id).ok().flatten().is_none() {
                    return Reply::err(Self::ERR_NOT_FOUND);
                }
                if let Some(over) = AiBudget::check(&AiBudget::usage(&store, &ai, now)) {
                    return Reply::err(&over.message);
                }
                match describer.describe(&ai, key.as_deref(), &ticket.image, ct) {
                    Ok(result) => {
                        let _ = store.add_ai_usage(&AiBudget::today(now), 1, 0);
                        self.audit(s, &verb, 1, &sha);
                        Reply::ok(CoreJson::to_string(&result).unwrap())
                    }
                    Err(DescribeError::Message(msg)) => {
                        self.audit(s, &verb, 0, &sha);
                        Reply::err(&msg)
                    }
                    Err(DescribeError::Unreachable) => {
                        self.audit(s, &verb, 0, &sha);
                        Reply::err(Self::ERR_PROVIDER_UNREACHABLE)
                    }
                    Err(DescribeError::Timeout) => {
                        self.audit(s, &verb, 0, &sha);
                        Reply::err(Self::ERR_PROVIDER_TIMEOUT)
                    }
                    Err(DescribeError::Cancelled) => {
                        if ct.is_cancelled() {
                            Reply {
                                line: Envelope::error(Self::ERR_INTERNAL),
                                subscribe: Subscription::None,
                                close: true,
                                stream: None,
                            }
                        } else {
                            self.audit(s, &verb, 0, &sha);
                            Reply::err(Self::ERR_PROVIDER_TIMEOUT)
                        }
                    }
                }
            }
            _ => Reply::err(Self::USAGE_DESCRIBE),
        }
    }

    // ---- settings ----

    pub fn touches_guarded_settings(root: &Value) -> bool {
        let Some(obj) = root.as_object() else {
            return false;
        };
        for (g_name, g_val) in obj {
            if Self::GUARDED_SETTING_GROUPS.contains(&g_name.as_str()) {
                return true;
            }
            if let Some(sub) = g_val.as_object() {
                for k_name in sub.keys() {
                    if Self::GUARDED_SETTING_KEYS
                        .iter()
                        .any(|&(g, k)| g == g_name && k == k_name)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn key_protection_error(&self, root: &Value) -> Option<&'static str> {
        let sec = root.get("security").and_then(Value::as_object)?;
        let protection = sec.get("keyProtection").and_then(Value::as_str);
        let method = sec.get("unlockMethod").and_then(Value::as_str);
        if protection.is_none() && method.is_none() {
            return None;
        }
        let s = self.settings();
        let file_mode = self.d.keys.protection();
        let next_prot = protection.unwrap_or(&s.security.key_protection);
        let next_method = method.unwrap_or(&s.security.unlock_method);
        if next_prot == KeyVault::MODE_PASSPHRASE
            && next_method == "password"
            && file_mode != KeyVault::MODE_PASSPHRASE
        {
            return Some(Self::ERR_PASSPHRASE_FIRST);
        }
        if next_prot == KeyVault::MODE_PASSPHRASE
            && next_method == "hello"
            && self.hello_missing()
            && file_mode != KeyVault::MODE_PASSPHRASE
        {
            return Some(Self::ERR_PASSPHRASE_NEEDS_HELLO);
        }
        if let Some(prot) = protection
            && prot != KeyVault::MODE_PASSPHRASE
            && file_mode == KeyVault::MODE_PASSPHRASE
            && self.d.keys.key_loaded()
            && !self.unlocked_with_passphrase.load(Ordering::Acquire)
        {
            return Some(Self::ERR_PASSPHRASE_NEEDED);
        }
        None
    }

    fn settings_verb(&self, session: &ClientSession, rest: &str) -> Reply {
        let (sub, json) = split_verb(rest);
        match sub.to_ascii_lowercase().as_str() {
            "get" if json.is_empty() => {
                Reply::ok(CoreJson::to_string(&self.settings()).unwrap())
            }
            "reset" if json.is_empty() => {
                if self.d.settings.load_error().is_none() {
                    return Reply::err(if self.lock_error(PipeKind::Ui) == Some(Self::ERR_LOCKED) {
                        Self::ERR_LOCKED
                    } else {
                        Self::ERR_NOTHING_TO_RESET
                    });
                }
                match self.d.settings.reset_unreadable() {
                    Ok(true) => {
                        self.audit(session, "settings reset", 1, "");
                        Reply::ok(CoreJson::to_string(&self.settings()).unwrap())
                    }
                    Ok(false) => Reply::err(Self::ERR_NOTHING_TO_RESET),
                    Err(e) => Reply::err(&e.to_string()),
                }
            }
            "set" if !json.is_empty() => {
                let Ok(root) = serde_json::from_str::<Value>(json) else {
                    return Reply::err(Self::ERR_JSON);
                };
                if let Some(locked) = self.lock_error(PipeKind::Ui)
                    && Self::touches_guarded_settings(&root)
                    && !(self.no_unlock_path() && only_unlock_method(&root))
                {
                    return Reply::err(locked);
                }
                if let Some(sec) = root.get("security")
                    && json_str(sec, "unlockMethod") == Some("password")
                    && !self.d.password.as_ref().is_some_and(|p| p.is_set())
                {
                    return Reply::err(Self::ERR_NO_PASSWORD);
                }
                if let Some(key_err) = self.key_protection_error(&root) {
                    return Reply::err(key_err);
                }
                if let Some(err) = self.d.settings.try_update(&root) {
                    return Reply::err(&err);
                }
                let s = self.settings();
                if !s.privacy.search_history
                    && let Some(st) = (self.d.store)()
                    && let Err(e) = st.delete_history(None)
                {
                    Log::warn_err("history clear failed", &e.to_string());
                }
                if !s.ai.chat_history
                    && !self.is_locked(PipeKind::Ui)
                    && let Some(st) = (self.d.store)()
                    && let Err(e) = st.delete_chats(None)
                {
                    Log::warn_err("chat clear failed", &e.to_string());
                }
                Reply::ok(CoreJson::to_string(&s).unwrap())
            }
            _ => Reply::err(Self::USAGE_SETTINGS),
        }
    }

    // ---- replay ----

    fn replay(&self, s: &ClientSession, rest: &str, ct: &CancelToken) -> Reply {
        let (sub, tail) = split_verb(rest);
        let args: Vec<&str> = tail.split_whitespace().collect();
        let purge_epoch = self.purge_all_epoch.load(Ordering::Acquire);
        let sub_lower = sub.to_ascii_lowercase();

        let res: Result<Reply, AgentError> = match sub_lower.as_str() {
            "save" if args.is_empty() => {
                if self.key_needs_passphrase() {
                    return Reply::err(Self::ERR_LOCKED);
                }
                self.d
                    .agent
                    .save_replay(ct)
                    .map(|clip: ReplayClip| Reply::ok(CoreJson::to_string(&clip).unwrap()))
            }
            "list" if args.is_empty() => {
                if let Some(locked) = self.lock_error(s.kind) {
                    return Reply::err(locked);
                }
                let items = self.d.agent.list_replays();
                self.audit(s, "replay list", items.len() as i32, "");
                Ok(Reply::ok(CoreJson::to_string(&ItemsReply { items }).unwrap()))
            }
            "export" if args.len() >= 2 => {
                let (id, path_raw) = split_verb(tail);
                let path = path_raw.trim();
                if !is_abs_path(path) {
                    return Reply::err(Self::USAGE_REPLAY);
                }
                if let Some(locked) = self.lock_error(s.kind) {
                    return Reply::err(locked);
                }
                let lock_tok = self.lock_token(s.kind);
                let linked = CancelSource::linked2(ct, &lock_tok);
                self.d
                    .agent
                    .export_replay(id, Path::new(path), &linked.token())
                    .map(|()| {
                        if self.is_locked(s.kind) {
                            let _ = fs::remove_file(path);
                            return Reply::err(Self::ERR_LOCKED);
                        }
                        self.audit(s, "replay export", 1, "");
                        Reply::ok("null".to_string())
                    })
            }
            "delete" if args.len() == 1 => {
                if let Some(locked) = self.lock_error(s.kind) {
                    return Reply::err(locked);
                }
                self.d
                    .agent
                    .delete_replay(args[0])
                    .map(|()| Reply::ok("null".to_string()))
            }
            _ => return Reply::err(Self::USAGE_REPLAY),
        };

        match res {
            Ok(reply) => reply,
            Err(ref e)
                if self.purge_all_epoch.load(Ordering::Acquire) != purge_epoch
                    || is_replay_purge_refusal(e) =>
            {
                Log::info(&format!("replay {sub_lower}: purge all ran during it"));
                Reply::err(Self::ERR_PURGED_DURING)
            }
            Err(AgentError::Cancelled) if !ct.is_cancelled() => Reply::err(Self::ERR_LOCKED),
            Err(AgentError::Cancelled) => Reply {
                line: Envelope::error(Self::ERR_INTERNAL),
                subscribe: Subscription::None,
                close: true,
                stream: None,
            },
            Err(AgentError::Locked) => Reply::err(Self::ERR_LOCKED),
            Err(AgentError::InvalidOperation(ref m)) if m == Self::ERR_LOCKED => {
                Reply::err(Self::ERR_LOCKED)
            }
            // B2: user disabled instant replay; return the refusal text without an error log.
            Err(AgentError::InvalidOperation(ref m)) if m == Self::ERR_REPLAY_OFF => {
                Log::info(&format!("replay {sub_lower}: instant replay is off"));
                Reply::err(Self::ERR_REPLAY_OFF)
            }
            Err(AgentError::NotFound) => Reply::err(Self::ERR_NOT_FOUND),
            Err(e) => {
                Log::error_err("replay command failed", &e.to_string());
                Reply::err(Self::ERR_INTERNAL)
            }
        }
    }

    // ---- bench ----

    fn bench(&self, s: &ClientSession, args: &[&str], ct: &CancelToken) -> Reply {
        if args.is_empty() {
            return match self.d.agent.bench(ct) {
                Ok(r) => Reply::ok(CoreJson::to_string(&r).unwrap()),
                Err(AgentError::Cancelled) if ct.is_cancelled() => Reply {
                    line: Envelope::error(Self::ERR_INTERNAL),
                    subscribe: Subscription::None,
                    close: true,
                    stream: None,
                },
                Err(_) => Reply::err(Self::ERR_INTERNAL),
            };
        }
        if args.len() != 1 {
            return Reply::err(Self::USAGE_BENCH);
        }
        match args[0].to_ascii_lowercase().as_str() {
            "full" => {
                let sw = Instant::now();
                let mut report = match self.d.agent.bench_full(ct) {
                    Ok(r) => r,
                    Err(AgentError::Cancelled) if ct.is_cancelled() => {
                        return Reply {
                            line: Envelope::error(Self::ERR_INTERNAL),
                            subscribe: Subscription::None,
                            close: true,
                            stream: None,
                        };
                    }
                    Err(_) => return Reply::err(Self::ERR_INTERNAL),
                };
                if let Some(search) = self.run_search_bench(s.kind) {
                    report.search = Some(search);
                }
                report.took_ms = report.took_ms.max(sw.elapsed().as_millis() as i64);
                report.recommendations = Recommender::recommend(&report, &self.settings());
                self.save_report(&report);
                Reply::ok(CoreJson::to_string(&report).unwrap())
            }
            "last" => match self.load_report() {
                Some(r) => Reply::ok(CoreJson::to_string(&r).unwrap()),
                None => Reply::err(Self::ERR_NOT_FOUND),
            },
            _ => Reply::err(Self::USAGE_BENCH),
        }
    }

    pub fn run_search_bench(&self, kind: PipeKind) -> Option<SearchBench> {
        let store = (self.d.store)()?;
        if self.is_locked(kind) {
            return None;
        }
        let rows = store.stats().ok()?.rows;
        if rows == 0 {
            return None;
        }
        let dim = store
            .space()
            .map_or(self.settings().index.dim, |sp| sp.dim)
            .max(1) as usize;
        let mut seed = 2026_1110_u64;
        let mut next_f32 = || -> f32 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let u = ((seed >> 11) as f64) / ((1u64 << 53) as f64);
            (u * 2.0 - 1.0) as f32
        };
        let mut vec_ms = Vec::with_capacity(20);
        let mut hyb_ms = Vec::with_capacity(20);
        for i in 0..20 {
            let mut q = vec![0.0f32; dim];
            for x in &mut q {
                *x = next_f32();
            }
            let t = Instant::now();
            let _ = store.search(
                &SearchRequest {
                    q: "x".to_string(),
                    limit: Some(10),
                    from: None,
                    to: None,
                    app: None,
                    kind: None,
                    mode: SearchMode::Vector,
                    site: None,
                },
                &q,
                10,
                Self::UI_MAX_RESPONSE_BYTES,
            );
            vec_ms.push(t.elapsed().as_secs_f64() * 1000.0);

            let t = Instant::now();
            let _ = store.search(
                &SearchRequest {
                    q: Self::BENCH_WORDS[i % Self::BENCH_WORDS.len()].to_string(),
                    limit: Some(10),
                    from: None,
                    to: None,
                    app: None,
                    kind: None,
                    mode: SearchMode::Hybrid,
                    site: None,
                },
                &q,
                10,
                Self::UI_MAX_RESPONSE_BYTES,
            );
            hyb_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        Some(SearchBench {
            rows,
            vector_ms: (median(&mut vec_ms) * 100.0).round() / 100.0,
            hybrid_ms: (median(&mut hyb_ms) * 100.0).round() / 100.0,
        })
    }

    fn save_report(&self, report: &BenchReport) {
        let (Some(blobs), Some(home)) = (self.d.blobs.as_ref(), self.d.home.as_ref()) else {
            Log::info("bench report not saved (no key)");
            return;
        };
        if !self.d.keys.key_loaded() {
            Log::info("bench report not saved (no key)");
            return;
        }
        let path = DataPaths::bench_report(home);
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        let Ok(json) = CoreJson::to_string(report) else {
            return;
        };
        let res = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut w = blobs.open_write(&tmp)?;
            w.write_all(json.as_bytes())?;
            w.finish()?;
            win32::move_replace(&tmp, &path)?;
            Ok(())
        })();
        if let Err(e) = res {
            Log::warn_err("bench report save failed", &e.to_string());
        }
    }

    fn load_report(&self) -> Option<BenchReport> {
        let (Some(blobs), Some(home)) = (self.d.blobs.as_ref(), self.d.home.as_ref()) else {
            return None;
        };
        if !self.d.keys.key_loaded() {
            return None;
        }
        let path = DataPaths::bench_report(home);
        if !path.exists() {
            return None;
        }
        let mut r = match blobs.open_read(&path) {
            Ok(r) => r,
            Err(e) => {
                Log::warn_err("bench report unreadable", &e.to_string());
                return None;
            }
        };
        let mut buf = Vec::new();
        if std::io::Read::read_to_end(&mut r, &mut buf).is_err() {
            Log::warn("bench report unreadable");
            return None;
        }
        match serde_json::from_slice(&buf) {
            Ok(rep) => Some(rep),
            Err(e) => {
                Log::warn_err("bench report unreadable", &e.to_string());
                None
            }
        }
    }

    // ---- chat verb (CommandProcessor.Chat.cs) ----

    fn chat_verb(&self, s: &ClientSession, rest: &str, ct: &CancelToken) -> Reply {
        let Some(ref chat) = self.chat else {
            return Reply::err(Self::ERR_AI_OFF);
        };
        let (sub_raw, tail) = split_verb(rest);
        let sub = sub_raw.to_ascii_lowercase();
        if let Some(locked) = self.lock_error(s.kind) {
            return Reply::err(locked);
        }
        match sub.as_str() {
            "list" if tail.is_empty() => {
                let items: Vec<ChatSummary> = chat.list(200);
                Reply::ok(CoreJson::to_string(&ItemsReply { items }).unwrap())
            }
            "get" if !tail.is_empty() => match chat.get(tail.trim()) {
                Some(detail) => {
                    let detail: ChatDetail = detail;
                    Reply::ok(CoreJson::to_string(&detail).unwrap())
                }
                None => Reply::err(Self::ERR_NOT_FOUND),
            },
            "rename" if !tail.is_empty() => {
                let Ok(root) = serde_json::from_str::<Value>(tail) else {
                    return Reply::err(Self::ERR_JSON);
                };
                let (Some(id), Some(title)) = (json_str(&root, "id"), json_str(&root, "title"))
                else {
                    return Reply::err(Self::ERR_JSON);
                };
                if id.is_empty() {
                    return Reply::err(Self::ERR_JSON);
                }
                if chat.rename(id, title) {
                    Reply::ok("null".to_string())
                } else {
                    Reply::err(Self::ERR_NOT_FOUND)
                }
            }
            "delete" if !tail.is_empty() => {
                let n = chat.delete(Some(tail.trim()));
                self.audit(s, "chat delete", n, "");
                if n > 0 {
                    Reply::ok("null".to_string())
                } else {
                    Reply::err(Self::ERR_NOT_FOUND)
                }
            }
            "clear" if tail.is_empty() => {
                let n = chat.delete(None);
                self.audit(s, "chat clear", n, "");
                Reply::ok("null".to_string())
            }
            "send" | "approve" | "deny" if !tail.is_empty() => {
                let ai = self.settings().ai;
                if !ai.enabled {
                    return Reply::err(Self::ERR_AI_OFF);
                }
                if ai.provider != AiDefaults::OPEN_AI
                    && self.d.ai_key.as_ref().and_then(|k| k.read(&ai)).is_none()
                {
                    return Reply::err(Self::ERR_NO_API_KEY);
                }
                let store_for_audit = self.d.store.clone();
                let now_for_audit = self.now.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let pipe_label = s.pipe_label();
                let pid = s.pid;
                let image = s.image.clone();
                let audit: AuditFn = Arc::new(move |verb, n, sha| {
                    if let Some(st) = store_for_audit() {
                        let _ = st.append_audit(&AuditEntry {
                            ts: now_for_audit(),
                            pipe: pipe_label.to_string(),
                            client_pid: pid,
                            client_image: image.clone(),
                            verb: verb.to_string(),
                            result_count: n,
                            query_sha256: sha.to_string(),
                        });
                    }
                });

                let chat_arc = chat.clone();
                let ct_owned = ct.clone();
                if sub == "deny" {
                    let token = tail.trim().to_string();
                    return Reply {
                        line: String::new(),
                        subscribe: Subscription::None,
                        close: false,
                        stream: Some(Box::new(move |emit_line| {
                            let mut emit_ev = |e: WireChatEvent| {
                                if let Ok(line) = Envelope::ok(&e) {
                                    emit_line(line);
                                }
                            };
                            chat_arc.deny(&token, &mut emit_ev, &ct_owned, Some(audit));
                        })),
                    };
                }

                let Ok(root) = serde_json::from_str::<Value>(tail) else {
                    return Reply::err(Self::ERR_JSON);
                };
                if !root.is_object() {
                    return Reply::err(Self::ERR_JSON);
                }
                if sub == "send" {
                    let chat_id = json_str(&root, "chat")
                        .filter(|c| !c.is_empty())
                        .map(String::from);
                    let text = json_str(&root, "text").unwrap_or("").to_string();
                    return Reply {
                        line: String::new(),
                        subscribe: Subscription::None,
                        close: false,
                        stream: Some(Box::new(move |emit_line| {
                            let mut emit_ev = |e: WireChatEvent| {
                                if let Ok(line) = Envelope::ok(&e) {
                                    emit_line(line);
                                }
                            };
                            chat_arc.send(
                                chat_id.as_deref(),
                                &text,
                                &mut emit_ev,
                                &ct_owned,
                                Some(audit),
                            );
                        })),
                    };
                }

                let Some(tok) = json_str(&root, "token").filter(|t| !t.is_empty()).map(String::from)
                else {
                    return Reply::err(Self::ERR_JSON);
                };
                let remove: HashSet<String> = root
                    .get("remove")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                Reply {
                    line: String::new(),
                    subscribe: Subscription::None,
                    close: false,
                    stream: Some(Box::new(move |emit_line| {
                        let mut emit_ev = |e: WireChatEvent| {
                            if let Ok(line) = Envelope::ok(&e) {
                                emit_line(line);
                            }
                        };
                        chat_arc.approve(&tok, &remove, &mut emit_ev, &ct_owned, Some(audit));
                    })),
                }
            }
            _ => Reply::err(Self::USAGE_CHAT),
        }
    }

    // ---- models verb (R7, stage I4) ----

    fn models_verb(&self, args: &[&str], ct: &CancelToken) -> Reply {
        if args.is_empty() {
            return Reply::err(Self::USAGE_MODELS);
        }
        match args[0].to_ascii_lowercase().as_str() {
            "list" if args.len() == 1 => match self.d.agent.models_list() {
                Ok(list) => Reply::ok(CoreJson::to_string(&list).unwrap()),
                Err(AgentError::InvalidOperation(msg)) => Reply::err(&msg),
                Err(AgentError::NotSupported) => Reply::err("Not supported."),
                Err(e) => {
                    Log::error_err("models list failed", &e.to_string());
                    Reply::err(Self::ERR_INTERNAL)
                }
            },
            "download" if args.len() == 2 => {
                let target = args[1].to_string();
                let agent = self.d.agent.clone();
                let ct_owned = ct.clone();
                Reply {
                    line: String::new(),
                    subscribe: Subscription::None,
                    close: false,
                    stream: Some(Box::new(move |emit_line| {
                        let mut emit_progress = |p: ModelsProgress| {
                            if let Ok(line) = Envelope::ok(&p) {
                                emit_line(line);
                            }
                        };
                        match agent.models_download(&target, &mut emit_progress, &ct_owned) {
                            Ok(()) => {}
                            Err(AgentError::InvalidOperation(msg)) => {
                                emit_line(Envelope::error(&msg));
                            }
                            Err(AgentError::NotSupported) => {
                                emit_line(Envelope::error("Not supported."));
                            }
                            Err(AgentError::Cancelled) => {
                                if let Ok(line) = Envelope::ok(&ModelsProgress {
                                    phase: "cancelled".to_string(),
                                    done_bytes: 0,
                                    total_bytes: 0,
                                    file: None,
                                    error: None,
                                }) {
                                    emit_line(line);
                                }
                            }
                            Err(e) => {
                                Log::error_err("models download failed", &e.to_string());
                                emit_line(Envelope::error(Self::ERR_INTERNAL));
                            }
                        }
                    })),
                }
            }
            _ => Reply::err(Self::USAGE_MODELS),
        }
    }

    // ---- update verb (R8, stage I5) ----

    fn update_verb(&self, args: &[&str], ct: &CancelToken) -> Reply {
        if args.len() != 1 {
            return Reply::err(Self::USAGE_UPDATE);
        }
        match args[0].to_ascii_lowercase().as_str() {
            "status" => match self.d.agent.update_status() {
                Ok(st) => Reply::ok(CoreJson::to_string(&st).unwrap()),
                Err(AgentError::InvalidOperation(msg)) => Reply::err(&msg),
                Err(AgentError::NotSupported) => Reply::err("Not supported."),
                Err(e) => {
                    Log::error_err("update status failed", &e.to_string());
                    Reply::err(Self::ERR_INTERNAL)
                }
            },
            "check" => match self.d.agent.update_check(ct) {
                Ok(st) => Reply::ok(CoreJson::to_string(&st).unwrap()),
                Err(AgentError::InvalidOperation(msg)) => Reply::err(&msg),
                Err(AgentError::NotSupported) => Reply::err("Not supported."),
                Err(AgentError::Cancelled) => Reply {
                    line: Envelope::error(Self::ERR_INTERNAL),
                    subscribe: Subscription::None,
                    close: true,
                    stream: None,
                },
                Err(e) => {
                    Log::error_err("update check failed", &e.to_string());
                    Reply::err(Self::ERR_INTERNAL)
                }
            },
            "download" => {
                let agent = self.d.agent.clone();
                let ct_owned = ct.clone();
                Reply {
                    line: String::new(),
                    subscribe: Subscription::None,
                    close: false,
                    stream: Some(Box::new(move |emit_line| {
                        let mut emit_progress = |p: UpdateProgress| {
                            if let Ok(line) = Envelope::ok(&p) {
                                emit_line(line);
                            }
                        };
                        match agent.update_download(&mut emit_progress, &ct_owned) {
                            Ok(()) => {}
                            Err(AgentError::InvalidOperation(msg)) => {
                                emit_line(Envelope::error(&msg));
                            }
                            Err(AgentError::NotSupported) => {
                                emit_line(Envelope::error("Not supported."));
                            }
                            Err(AgentError::Cancelled) => {
                                if let Ok(line) = Envelope::ok(&UpdateProgress {
                                    phase: "cancelled".to_string(),
                                    done_bytes: 0,
                                    total_bytes: 0,
                                    error: None,
                                }) {
                                    emit_line(line);
                                }
                            }
                            Err(e) => {
                                Log::error_err("update download failed", &e.to_string());
                                emit_line(Envelope::error(Self::ERR_INTERNAL));
                            }
                        }
                    })),
                }
            }
            "apply" => match self.d.agent.update_apply() {
                Ok(st) => Reply::ok(CoreJson::to_string(&st).unwrap()),
                Err(AgentError::InvalidOperation(msg)) => Reply::err(&msg),
                Err(AgentError::NotSupported) => Reply::err("Not supported."),
                Err(e) => {
                    Log::error_err("update apply failed", &e.to_string());
                    Reply::err(Self::ERR_INTERNAL)
                }
            },
            _ => Reply::err(Self::USAGE_UPDATE),
        }
    }
}

fn cancel_lock_cts(lock_cts: &Mutex<CancelSource>) {
    let old = {
        let mut g = lock_cts.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *g)
    };
    old.cancel();
}

fn lock_error_parts(
    kind: PipeKind,
    s: &Settings,
    agent: &dyn AgentControl,
    store_fn: &StoreFn,
    keys: &KeyVault,
) -> Option<&'static str> {
    if (kind == PipeKind::Ui || s.ipc.require_unlocked) && !agent.is_unlocked() {
        return Some(CommandProcessor::ERR_LOCKED);
    }
    if store_fn().is_none() {
        return Some(if keys.needs_passphrase() {
            CommandProcessor::ERR_LOCKED
        } else {
            CommandProcessor::ERR_NOT_OPEN
        });
    }
    None
}

fn status_for_parts(
    kind: PipeKind,
    mut st: AgentStatus,
    s: &Settings,
    agent: &dyn AgentControl,
    store_fn: &StoreFn,
    keys: &KeyVault,
    password: Option<&UnlockPassword>,
) -> AgentStatus {
    let key_locked = keys.needs_passphrase();
    if key_locked && !st.locked {
        st.locked = true;
        st.unlocked_until = None;
    }
    if kind == PipeKind::Ui {
        st.unlock_needs = Some(unlock_needs_parts(key_locked, s, agent, keys, password).to_string());
        return st;
    }
    st.unlock_needs = None;
    if lock_error_parts(kind, s, agent, store_fn, keys) == Some(CommandProcessor::ERR_LOCKED) {
        st.store = StoreStats::default();
        st.queue = QueueState { pending: 0 };
    }
    st
}

fn unlock_needs_parts(
    key_locked: bool,
    s: &Settings,
    agent: &dyn AgentControl,
    keys: &KeyVault,
    password: Option<&UnlockPassword>,
) -> &'static str {
    if key_locked {
        return "passphrase";
    }
    if agent.is_unlocked() {
        return "none";
    }
    let pw_set = password.is_some_and(UnlockPassword::is_set);
    let uses_pw = s.security.unlock_method == "password";
    let hello_missing = !keys.hello_available() && keys.hello_checked();
    if uses_pw {
        return if pw_set { "password" } else { "hello" };
    }
    if s.security.require_unlock
        && hello_missing
        && pw_set
        && keys.protection() != KeyVault::MODE_PASSPHRASE
    {
        return "password";
    }
    if keys.key_loaded() && s.security.key_protection == KeyVault::MODE_PASSPHRASE {
        return "passphrase";
    }
    if s.security.require_unlock && hello_missing && pw_set {
        return "password";
    }
    "hello"
}

fn audit_queries_for(store: &EncryptedStore, scope: &PurgeScope) -> Vec<String> {
    let needles = EncryptedStore::history_needles(scope);
    let mut queries = HashSet::new();
    for n in &needles {
        queries.insert(n.clone());
        queries.insert(n.to_lowercase());
    }
    if !needles.is_empty()
        && let Ok(hist) = store.history(1000)
    {
        for h in hist {
            let hl = h.q.to_lowercase();
            if needles.iter().any(|n| hl.contains(&n.to_lowercase())) {
                queries.insert(h.q);
            }
        }
    }
    queries.into_iter().collect()
}

fn is_replay_purge_refusal(e: &AgentError) -> bool {
    matches!(e, AgentError::InvalidOperation(m) if CommandProcessor::REPLAY_PURGE_REFUSALS.contains(&m.as_str()))
}

fn only_unlock_method(root: &Value) -> bool {
    let Some(obj) = root.as_object() else {
        return false;
    };
    if obj.len() != 1 {
        return false;
    }
    let Some(sec) = obj.get("security").and_then(Value::as_object) else {
        return false;
    };
    sec.len() == 1 && sec.get("unlockMethod").is_some_and(Value::is_string)
}

fn split_verb(line: &str) -> (&str, &str) {
    let t = line.trim();
    match t.find(' ') {
        Some(sp) => (&t[..sp], t[sp + 1..].trim_start_matches(' ')),
        None => (t, ""),
    }
}

fn raw_rest(line: &str) -> &str {
    let t = line.trim_start();
    match t.find(' ') {
        Some(sp) => &t[sp + 1..],
        None => "",
    }
}

fn json_str<'a>(root: &'a Value, name: &str) -> Option<&'a str> {
    root.as_object()?.get(name)?.as_str()
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}
