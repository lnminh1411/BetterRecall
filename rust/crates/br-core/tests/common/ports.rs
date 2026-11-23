// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Shared test doubles and fixtures for sweep 6 G3 ported C# core tests.

#![allow(clippy::type_complexity)]
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use br_contracts::{Envelope, Settings};
use br_core::{
    ApiKeyStore, BlobCipher, CancelToken, ChatProvider, ChatRequest, ClientSession,
    CloudDescriber, CommandDeps, CommandProcessor, DescribeError, DescribeRedactor,
    EncryptedStore, HttpRequest, HttpResponse, HttpTransport, KeyVault, OutgoingImage, PipeKind,
    ProviderFactoryFn, ProviderHttp, ProviderTurn, SettingsStore, SseReadError, Thumbnailer,
    ToolCall, ToolOutput, TransportError, UnlockPassword,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Map, Value, json};

use super::a3::{FakeAgent, FakeEmbedder, FakeThumbs};
use super::{TempHome, TempStore};

/// Builder options for [`PortFixture`].
pub struct PortFixtureOpts {
    pub key_protection: &'static str,
    pub hello_available: bool,
    pub load_silent: bool,
    pub store_open: bool,
    pub redactor: Option<Arc<dyn DescribeRedactor>>,
    pub thumbnailer: Option<Arc<dyn Thumbnailer>>,
    pub describer: Option<Arc<CloudDescriber>>,
    pub chat_http: Option<Arc<ProviderHttp>>,
    pub chat_providers: Option<ProviderFactoryFn>,
    pub purge_all: Option<Arc<dyn Fn(&CancelToken) -> i32 + Send + Sync>>,
    pub purge_all_undeleted: Option<Arc<dyn Fn() -> i32 + Send + Sync>>,
    pub initial_now: DateTime<Utc>,
    pub initial_settings_raw: Option<String>,
}

impl Default for PortFixtureOpts {
    fn default() -> Self {
        Self {
            key_protection: "dpapi",
            hello_available: true,
            load_silent: true,
            store_open: true,
            redactor: None,
            thumbnailer: None,
            describer: None,
            chat_http: None,
            chat_providers: None,
            purge_all: None,
            purge_all_undeleted: None,
            initial_now: Utc::now(),
            initial_settings_raw: None,
        }
    }
}

/// Comprehensive fixture wiring `CommandProcessor` with injectable wall clock, fast Argon2
/// `UnlockPassword`, `KeyVault`, `SettingsStore`, `EncryptedStore`, `ApiKeyStore`, and fakes.
pub struct PortFixture {
    _t: TempStore,
    pub store: Arc<EncryptedStore>,
    pub home: TempHome,
    pub settings: Arc<SettingsStore>,
    pub vault: Arc<KeyVault>,
    pub agent: Arc<FakeAgent>,
    pub embedder: Arc<FakeEmbedder>,
    pub thumbs: Arc<FakeThumbs>,
    pub password: Arc<UnlockPassword>,
    pub ai_key: Arc<ApiKeyStore>,
    pub store_open: Arc<AtomicBool>,
    pub purge_all_calls: Arc<AtomicI32>,
    pub now: Arc<Mutex<DateTime<Utc>>>,
    pub p: Arc<CommandProcessor>,
}

impl PortFixture {
    pub fn new() -> Self {
        Self::with_opts(PortFixtureOpts::default())
    }

    pub fn with_opts(opts: PortFixtureOpts) -> Self {
        let mut t = TempStore::new();
        let store = Arc::new(t.store.take().expect("store"));
        let home = if opts.initial_settings_raw.is_some() {
            TempHome::new(None)
        } else {
            TempHome::new(Some(opts.key_protection))
        };
        if let Some(raw) = opts.initial_settings_raw {
            std::fs::write(br_contracts::DataPaths::settings_file(home.path()), raw)
                .expect("write raw settings");
        }
        let settings = Arc::new(SettingsStore::load(home.path()));
        let now = Arc::new(Mutex::new(opts.initial_now));

        let s_for_vault = settings.clone();
        let vault = Arc::new(KeyVault::new(
            home.path(),
            move || s_for_vault.current(),
            None,
            false,
        ));
        let now_for_vault = now.clone();
        vault.set_now_for_test(move || *now_for_vault.lock().unwrap());
        vault.set_hello_available_for_test(opts.hello_available);
        if opts.load_silent && opts.key_protection == "dpapi" {
            vault.try_load_silent().expect("vault silent load in setup");
        }

        let agent = Arc::new(FakeAgent::new());
        let embedder = Arc::new(FakeEmbedder::new());
        let thumbs = Arc::new(FakeThumbs::new());
        let thumb_trait: Arc<dyn Thumbnailer> =
            opts.thumbnailer.unwrap_or_else(|| thumbs.clone());

        let now_for_pw = now.clone();
        let password = Arc::new(UnlockPassword::with_options(
            home.path(),
            Arc::new(move || *now_for_pw.lock().unwrap()),
            1024,
            1,
            1,
        ));
        let ai_key = Arc::new(ApiKeyStore::new(home.path()));
        let store_open = Arc::new(AtomicBool::new(opts.store_open));
        let purge_all_calls = Arc::new(AtomicI32::new(0));

        let st_clone = store.clone();
        let open_clone = store_open.clone();
        let pa_clone = purge_all_calls.clone();
        let purge_all_fn = opts.purge_all.unwrap_or_else(|| {
            Arc::new(move |_| {
                pa_clone.fetch_add(1, Ordering::AcqRel);
                42
            })
        });
        let blobs = if vault.key_loaded() {
            Some(Arc::new(BlobCipher::from_vault(vault.clone())))
        } else {
            None
        };

        let p = Arc::new(CommandProcessor::new(CommandDeps {
            settings: settings.clone(),
            store: Arc::new(move || {
                if open_clone.load(Ordering::Acquire) {
                    Some(st_clone.clone())
                } else {
                    None
                }
            }),
            keys: vault.clone(),
            agent: agent.clone(),
            embedder: embedder.clone(),
            thumbnailer: thumb_trait,
            purge_all: purge_all_fn,
            purge_all_undeleted: opts.purge_all_undeleted,
            blobs,
            home: Some(home.path().to_path_buf()),
            password: Some(password.clone()),
            ai_key: Some(ai_key.clone()),
            describer: Some(opts.describer.unwrap_or_else(|| Arc::new(CloudDescriber::new()))),
            redactor: opts.redactor,
            chat_http: opts.chat_http,
            chat_providers: opts.chat_providers,
        }));
        let now_for_proc = now.clone();
        p.set_now_for_test(move || *now_for_proc.lock().unwrap());
        // B3: the rate limiter uses a monotonic clock; drive it from the same fake time.
        let now_for_mono = now.clone();
        let wall0 = *now.lock().unwrap();
        let mono0 = std::time::Instant::now();
        p.set_monotonic_now_for_test(move || {
            let d = (*now_for_mono.lock().unwrap() - wall0).to_std().unwrap_or_default();
            mono0 + d
        });
        let p_weak = Arc::downgrade(&p);
        store.on_purged(move |n| {
            if let Some(proc) = p_weak.upgrade() {
                proc.on_store_purged(n);
            }
        });

        Self {
            _t: t,
            store,
            home,
            settings,
            vault,
            agent,
            embedder,
            thumbs,
            password,
            ai_key,
            store_open,
            purge_all_calls,
            now,
            p,
        }
    }

    pub fn ui() -> ClientSession {
        ClientSession::new(PipeKind::Ui, 101, r"C:\app\ui\BetterRecall.UI.exe")
    }

    pub fn pub_sess() -> ClientSession {
        ClientSession::new(PipeKind::Public, 100, r"C:\tools\client.exe")
    }

    pub fn mcp() -> ClientSession {
        ClientSession::new(PipeKind::Mcp, 102, r"C:\app\betterrecall-mcp.exe")
    }

    pub fn call(&self, s: &mut ClientSession, line: &str) -> (bool, Option<Value>, Option<String>) {
        let r = self.p.handle(s, line, &CancelToken::none());
        assert!(!r.line.contains('\n'), "reply contains newline: {}", r.line);
        Envelope::parse(&r.line).expect("parse envelope")
    }

    pub fn call_stream(
        &self,
        s: &mut ClientSession,
        line: &str,
        ct: &CancelToken,
    ) -> Vec<(bool, Option<Value>, Option<String>)> {
        let r = self.p.handle(s, line, ct);
        let mut out = Vec::new();
        if let Some(stream) = r.stream {
            stream(&mut |raw| {
                assert!(!raw.contains('\n'), "stream line contains newline: {raw}");
                out.push(Envelope::parse(&raw).expect("parse stream envelope"));
            });
        } else {
            assert!(!r.line.contains('\n'));
            out.push(Envelope::parse(&r.line).expect("parse envelope"));
        }
        out
    }

    pub fn advance(&self, delta: ChronoDuration) {
        let mut g = self.now.lock().unwrap();
        *g += delta;
    }

    pub fn set_settings(&self, patch: Value) {
        assert!(
            self.settings.try_update(&patch).is_none(),
            "settings update failed for {patch}"
        );
    }

    pub fn current_settings(&self) -> Settings {
        self.settings.current()
    }
}

/// Fixed image redactor returning deterministic bytes and `redacted = self.0`.
pub struct FixedRedactor {
    pub redacted: bool,
    pub bytes: Vec<u8>,
}

impl FixedRedactor {
    pub const REDACTED_BYTES: &'static [u8] = b"REDACTED-IMAGE-BYTES";

    pub fn ok() -> Self {
        Self {
            redacted: true,
            bytes: Self::REDACTED_BYTES.to_vec(),
        }
    }

    pub fn unchecked() -> Self {
        Self {
            redacted: false,
            bytes: Self::REDACTED_BYTES.to_vec(),
        }
    }
}

impl DescribeRedactor for FixedRedactor {
    fn redact(
        &self,
        _capture_id: &str,
        mut image: OutgoingImage,
        _ct: &CancelToken,
    ) -> Result<OutgoingImage, DescribeError> {
        image.bytes = self.bytes.clone();
        image.redacted = self.redacted;
        Ok(image)
    }

    fn redacts_images(&self) -> bool {
        true
    }
}

/// Recorded outgoing HTTP request on [`RecordingTransport`].
#[derive(Debug, Clone)]
pub struct RecordedHttp {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedHttp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

type ScriptedResponse =
    Box<dyn FnOnce(&HttpRequest, &CancelToken) -> Result<HttpResponse, TransportError> + Send>;

/// Queue-backed [`HttpTransport`] matching C#'s `ScriptHttp` for `ChatService` and `ProviderHttp` tests.
pub struct RecordingTransport {
    pub requests: Mutex<Vec<RecordedHttp>>,
    queue: Mutex<VecDeque<ScriptedResponse>>,
    pub offline: AtomicBool,
    pub on_send: Mutex<Option<Arc<dyn Fn(&HttpRequest, &CancelToken) + Send + Sync>>>,
}

impl Default for RecordingTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingTransport {
    pub fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            queue: Mutex::new(VecDeque::new()),
            offline: AtomicBool::new(false),
            on_send: Mutex::new(None),
        }
    }

    pub fn push_ok(&self, body: impl Into<String>, headers: Vec<(String, String)>) {
        let body = body.into();
        self.push_fn(move |_, _| {
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(Cursor::new(body.into_bytes())),
            })
        });
    }

    pub fn push_status(
        &self,
        status: u16,
        body: impl Into<String>,
        headers: Vec<(String, String)>,
    ) {
        let body = body.into();
        self.push_fn(move |_, _| {
            Ok(HttpResponse {
                status,
                headers,
                body: Box::new(Cursor::new(body.into_bytes())),
            })
        });
    }

    pub fn push_err(&self, err: impl FnOnce() -> TransportError + Send + 'static) {
        self.push_fn(move |_, _| Err(err()));
    }

    pub fn push_fn<F>(&self, f: F)
    where
        F: FnOnce(&HttpRequest, &CancelToken) -> Result<HttpResponse, TransportError>
            + Send
            + 'static,
    {
        self.queue.lock().unwrap().push_back(Box::new(f));
    }

    pub fn recorded(&self) -> Vec<RecordedHttp> {
        self.requests.lock().unwrap().clone()
    }
}

impl HttpTransport for RecordingTransport {
    fn send(
        &self,
        req: HttpRequest,
        _header_timeout: Duration,
        ct: &CancelToken,
    ) -> Result<HttpResponse, TransportError> {
        self.requests.lock().unwrap().push(RecordedHttp {
            url: req.url.clone(),
            headers: req.headers.clone(),
            body: String::from_utf8_lossy(&req.body).into_owned(),
        });
        let hook = self.on_send.lock().unwrap().clone();
        if let Some(h) = hook {
            h(&req, ct);
        }
        if ct.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        if self.offline.load(Ordering::Acquire) {
            return Err(TransportError::Offline("offline".to_string()));
        }
        let next = self.queue.lock().unwrap().pop_front();
        match next {
            Some(f) => f(&req, ct),
            None => Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::new(Cursor::new(Vec::new())),
            }),
        }
    }
}

/// Scriptable [`ChatProvider`] matching C#'s `ScriptProvider` in `ChatTests.cs`.
pub struct ScriptProvider {
    turns: Mutex<VecDeque<Result<ProviderTurn, SseReadError>>>,
    pub tool_outputs: Mutex<Vec<ToolOutput>>,
    pub requests: Mutex<Vec<ChatRequest>>,
    pub on_stream: Mutex<Option<Arc<dyn Fn(&CancelToken) + Send + Sync>>>,
}

impl Default for ScriptProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptProvider {
    pub fn new() -> Self {
        Self {
            turns: Mutex::new(VecDeque::new()),
            tool_outputs: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            on_stream: Mutex::new(None),
        }
    }

    pub fn push(&self, turn: ProviderTurn) {
        self.turns.lock().unwrap().push_back(Ok(turn));
    }

    pub fn push_err(&self, err: SseReadError) {
        self.turns.lock().unwrap().push_back(Err(err));
    }

    pub fn answer(text: &str, stop_reason: &str) -> ProviderTurn {
        ProviderTurn {
            assistant: json!({ "role": "assistant", "text": text }),
            text: text.to_string(),
            calls: Vec::new(),
            input_tokens: 100,
            output_tokens: 50,
            stop_reason: stop_reason.to_string(),
        }
    }

    pub fn search(query: &str, count: usize) -> ProviderTurn {
        let mut calls = Vec::with_capacity(count);
        for i in 0..count {
            let mut args = Map::new();
            args.insert("query".to_string(), Value::String(query.to_string()));
            calls.push(ToolCall {
                id: format!("c{}", i + 1),
                name: "search_history".to_string(),
                args,
            });
        }
        ProviderTurn {
            assistant: json!({ "role": "assistant", "calls": count }),
            text: String::new(),
            calls,
            input_tokens: 80,
            output_tokens: 20,
            stop_reason: "tool".to_string(),
        }
    }

    pub fn search_in(query: &str, app: &str) -> ProviderTurn {
        let mut args = Map::new();
        args.insert("query".to_string(), Value::String(query.to_string()));
        args.insert("app".to_string(), Value::String(app.to_string()));
        ProviderTurn {
            assistant: json!({ "role": "assistant", "calls": 1 }),
            text: String::new(),
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "search_history".to_string(),
                args,
            }],
            input_tokens: 80,
            output_tokens: 20,
            stop_reason: "tool".to_string(),
        }
    }
}

impl ChatProvider for ScriptProvider {
    fn name(&self) -> &'static str {
        "script"
    }

    fn build_request(&self, request: &ChatRequest) -> HttpRequest {
        self.requests.lock().unwrap().push(request.clone());
        let body = json!({
            "system": request.system,
            "transcript": request.transcript,
        })
        .to_string()
        .into_bytes();
        let mut req = HttpRequest::post_json(
            format!("{}/script", request.endpoint.trim_end_matches('/')),
            body,
        );
        if let Some(ref k) = request.api_key
            && !k.is_empty()
        {
            req.headers.push(("x-api-key".to_string(), k.clone()));
        }
        req
    }

    fn read_stream(
        &self,
        body: &mut dyn Read,
        on_text: &mut dyn FnMut(&str),
        ct: &CancelToken,
    ) -> Result<ProviderTurn, SseReadError> {
        let mut sink = Vec::new();
        let _ = body.read_to_end(&mut sink);
        let hook = self.on_stream.lock().unwrap().clone();
        if let Some(h) = hook {
            h(ct);
        }
        if ct.is_cancelled() {
            return Err(SseReadError::Cancelled);
        }
        let next = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(Self::answer("default", "end")));
        if let Ok(ref turn) = next
            && !turn.text.is_empty()
        {
            on_text(&turn.text);
        }
        next
    }

    fn append_user(&self, transcript: &mut Vec<Value>, text: &str) {
        transcript.push(json!({ "role": "user", "text": text }));
    }

    fn append_assistant(&self, transcript: &mut Vec<Value>, turn: &ProviderTurn) {
        transcript.push(turn.assistant.clone());
    }

    fn append_tool_results(&self, transcript: &mut Vec<Value>, results: &[ToolOutput]) {
        self.tool_outputs.lock().unwrap().extend(results.iter().cloned());
        let items: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "id": r.call_id,
                    "text": r.text,
                    "images": r.images.len(),
                })
            })
            .collect();
        transcript.push(json!({ "role": "tool", "results": items }));
    }
}
