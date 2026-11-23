// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sweep 6 G3 ported tests for `ChatShareGate`, `ProviderErrors`, `ProviderHttp`,
//! `AiBudget`, `ChatService`, and `CommandProcessor` `chat` verbs.

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{
    AiDefaults, AiShare, AiUsage, CaptureDetail, CaptureKind, ChatEvent, PurgeScope, SearchHit,
    Settings, ThumbData,
};
use br_core::{
    AiBudget, BasicTextRedactor, CancelToken, ChatDeps, ChatFailure, ChatProvider, ChatService,
    ChatShareGate, CommandProcessor, DescribeError, DescribeRedactor, OutgoingImage,
    PassThroughRedactor, ProviderErrors, ProviderHttp, ProviderStreamError, ShareGateInput,
    SseReadError, TransportError,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::json;

use common::a3::{FakeEmbedder, FakeHttpServer, FakeThumbs};
use common::ports::{FixedRedactor, PortFixture, PortFixtureOpts, RecordingTransport, ScriptProvider};
use common::{TempStore, screen, vec_info, vec_seeded};

fn sample_hit(id: &str, kind: CaptureKind) -> SearchHit {
    SearchHit {
        id: id.to_string(),
        ts: DateTime::parse_from_rfc3339("2026-11-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        kind,
        app: "chrome.exe".to_string(),
        title: "Inbox alice@example.com".to_string(),
        score: 1.0,
        snippet: "password: secret123".to_string(),
        has_thumb: true,
        duration_s: None,
        cosine: Some(0.9),
        text_match: true,
        weak: false,
        site: "mail.example.com".to_string(),
        replay_id: None,
        replay_offset_ms: None,
    }
}

struct ChatHarness {
    _t: TempStore,
    store: Arc<br_core::EncryptedStore>,
    settings: Arc<Mutex<Settings>>,
    embedder: Arc<FakeEmbedder>,
    _thumbs: Arc<FakeThumbs>,
    transport: Arc<RecordingTransport>,
    provider: Arc<ScriptProvider>,
    now: Arc<Mutex<DateTime<Utc>>>,
    _api_key: Arc<Mutex<Option<String>>>,
    service: Arc<ChatService>,
}

impl ChatHarness {
    fn new(redactor: Option<Arc<dyn DescribeRedactor>>) -> Self {
        let mut t = TempStore::new();
        let store = Arc::new(t.store.take().unwrap());
        let mut s = Settings::default();
        s.ai.enabled = true;
        s.ai.provider = AiDefaults::GEMINI.to_string();
        s.ai.endpoint = "https://example.invalid".to_string();
        s.ai.model = "gemini-2.5-flash".to_string();
        s.ai.share = AiShare::TEXT.to_string();
        s.ai.chat_max_hits = 5;
        s.ai.chat_max_images = 3;
        s.ai.chat_history = true;
        let settings = Arc::new(Mutex::new(s));
        let embedder = Arc::new(FakeEmbedder::new());
        let thumbs = Arc::new(FakeThumbs::new());
        let transport = Arc::new(RecordingTransport::new());
        let provider = Arc::new(ScriptProvider::new());
        let now = Arc::new(Mutex::new(
            DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        let api_key = Arc::new(Mutex::new(Some("AIzaSy-test".to_string())));

        let now_http = now.clone();
        let http = Arc::new(ProviderHttp::with_transport(
            transport.clone(),
            Some(Arc::new(move || *now_http.lock().unwrap())),
            Some(Arc::new(|_, ct| !ct.is_cancelled())),
        ));
        let s_fn = settings.clone();
        let st_fn = store.clone();
        let key_fn = api_key.clone();
        let prov_arc: Arc<dyn ChatProvider> = provider.clone();
        let service = Arc::new(ChatService::new(ChatDeps {
            settings: Arc::new(move || s_fn.lock().unwrap().clone()),
            store: Arc::new(move || Some(st_fn.clone())),
            embedder: embedder.clone(),
            thumbnailer: thumbs.clone(),
            api_key: Arc::new(move || key_fn.lock().unwrap().clone()),
            http,
            providers: Arc::new(move |_| Ok(prov_arc.clone())),
            image_redactor: redactor,
            text_redactor: ChatDeps::default_text_redactor(),
            audit: ChatDeps::noop_audit(),
        }));

        Self {
            _t: t,
            store,
            settings,
            embedder,
            _thumbs: thumbs,
            transport,
            provider,
            now,
            _api_key: api_key,
            service,
        }
    }

    fn send(&self, chat_id: Option<&str>, q: &str) -> Vec<ChatEvent> {
        let mut evs = Vec::new();
        self.service
            .send(chat_id, q, &mut |e| evs.push(e), &CancelToken::none(), None);
        evs
    }

    fn approve(&self, token: &str, remove: &[&str]) -> Vec<ChatEvent> {
        let rem: HashSet<String> = remove.iter().map(|s| (*s).to_string()).collect();
        let mut evs = Vec::new();
        self.service
            .approve(token, &rem, &mut |e| evs.push(e), &CancelToken::none(), None);
        evs
    }

    fn deny(&self, token: &str) -> Vec<ChatEvent> {
        let mut evs = Vec::new();
        self.service
            .deny(token, &mut |e| evs.push(e), &CancelToken::none(), None);
        evs
    }
}

#[test]
fn chat_share_gate_metadata_text_redactor_mix_and_stored_text() {
    // 1) Mixed redactor: first capture redacted=true, second capture redacted=false -> only first
    // gets image and plan.images_unavailable is true.
    struct AlternatingRedactor(AtomicI32);
    impl DescribeRedactor for AlternatingRedactor {
        fn redact(
            &self,
            _id: &str,
            mut image: OutgoingImage,
            _ct: &CancelToken,
        ) -> Result<OutgoingImage, DescribeError> {
            let n = self.0.fetch_add(1, Ordering::AcqRel);
            image.redacted = n == 0;
            image.bytes = FixedRedactor::REDACTED_BYTES.to_vec();
            Ok(image)
        }
        fn redacts_images(&self) -> bool {
            true
        }
    }

    let h1 = sample_hit("s_1", CaptureKind::Screen);
    let h2 = sample_hit("s_2", CaptureKind::Screen);
    let h_audio = sample_hit("a_1", CaptureKind::Audio);
    let red = AlternatingRedactor(AtomicI32::new(0));
    let plan = ChatShareGate::build(ShareGateInput {
        hits: &[h1.clone(), h2.clone(), h_audio],
        share: AiShare::TEXT_IMAGES,
        max_hits: 5,
        max_images: 3,
        detail: &|id| {
            Some(CaptureDetail {
                id: id.to_string(),
                ts: Utc::now(),
                kind: CaptureKind::Screen,
                app: "chrome.exe".into(),
                title: "Page".into(),
                text: "OCR text secret: mypass123".into(),
                has_thumb: true,
                duration_s: None,
                site: "example.com".into(),
                replay_id: None,
                replay_offset_ms: None,
            })
        },
        thumb: &|id, w, _| {
            Some(ThumbData {
                id: id.into(),
                mime: "image/webp".into(),
                width: w,
                height: w * 9 / 16,
                bytes: vec![1, 2, 3],
            })
        },
        image_redactor: Some(&red),
        text_redactor: &BasicTextRedactor,
        ct: &CancelToken::none(),
    });
    assert_eq!(AiShare::TEXT_IMAGES, plan.share);
    assert!(plan.hits[0].image.is_some());
    assert!(plan.hits[1].image.is_none());
    assert!(plan.hits[2].image.is_none(), "audio never gets an image");
    assert!(plan.images_unavailable);
    assert!(!plan.hits[0].text.contains("mypass123"));
    assert!(plan.hits[0].text.contains("[redacted]"));

    // 2) Text-only share uses stored detail text and makes 0 image-redactor calls.
    let red_count = AlternatingRedactor(AtomicI32::new(0));
    let plan_txt = ChatShareGate::build(ShareGateInput {
        hits: std::slice::from_ref(&h1),
        share: AiShare::TEXT,
        max_hits: 5,
        max_images: 3,
        detail: &|id| {
            Some(CaptureDetail {
                id: id.to_string(),
                ts: Utc::now(),
                kind: CaptureKind::Screen,
                app: "chrome.exe".into(),
                title: "Page".into(),
                text: "Stored OCR text only".into(),
                has_thumb: true,
                duration_s: None,
                site: "example.com".into(),
                replay_id: None,
                replay_offset_ms: None,
            })
        },
        thumb: &|id, w, _| {
            Some(ThumbData {
                id: id.into(),
                mime: "image/webp".into(),
                width: w,
                height: 432,
                bytes: vec![1, 2, 3],
            })
        },
        image_redactor: Some(&red_count),
        text_redactor: &BasicTextRedactor,
        ct: &CancelToken::none(),
    });
    assert_eq!(0, red_count.0.load(Ordering::Acquire));
    assert!(plan_txt.hits[0].text.contains("Stored OCR text only"));
    assert!(plan_txt.hits[0].image.is_none());

    // 3) PassThroughRedactor (redacts_images() == false) sends NO images even on textImages.
    let plan_pt = ChatShareGate::build(ShareGateInput {
        hits: &[h1],
        share: AiShare::TEXT_IMAGES,
        max_hits: 5,
        max_images: 3,
        detail: &|_| None,
        thumb: &|id, w, _| {
            Some(ThumbData {
                id: id.into(),
                mime: "image/webp".into(),
                width: w,
                height: 432,
                bytes: vec![1, 2, 3],
            })
        },
        image_redactor: Some(&PassThroughRedactor),
        text_redactor: &BasicTextRedactor,
        ct: &CancelToken::none(),
    });
    assert!(plan_pt.hits[0].image.is_none());
    assert!(plan_pt.images_unavailable);
}

#[test]
fn provider_errors_16_row_table_billing_retry_after_stream_policy_low_quota_and_budget() {
    let now = DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // 16-row classification table matching ChatTests.cs.
    let cases: &[(u16, &str, &str)] = &[
        (401, r#"{"error":{"type":"authentication_error","message":"bad key"}}"#, "auth"),
        (400, r#"{"error":{"details":[{"reason":"API_KEY_INVALID"}]}}"#, "auth"),
        (402, r#"{"error":{"message":"billing"}}"#, "quota"),
        (429, r#"{"error":{"type":"insufficient_quota","message":"no credit"}}"#, "quota"),
        (400, r#"{"error":{"type":"billing_error","message":"card declined"}}"#, "quota"),
        (400, r#"{"error":{"code":"credit_balance_exhausted"}}"#, "quota"),
        (400, r#"{"error":{"code":"organization_spend_limit_exceeded"}}"#, "quota"),
        (429, r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"quota exceeded"}}"#, "quota"),
        (413, r#"{"error":{"message":"too big"}}"#, "context"),
        (400, r#"{"error":{"code":"context_length_exceeded"}}"#, "context"),
        (400, r#"{"error":{"message":"prompt is too long"}}"#, "context"),
        (403, r#"{"error":{"type":"permission_error"}}"#, "forbidden"),
        (404, r#"{"error":{"code":"model_not_found"}}"#, "model"),
        (429, r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#, "rate"),
        (529, r#"{"error":{"type":"overloaded_error"}}"#, "overloaded"),
        (500, r#"{"error":{"message":"boom"}}"#, "server"),
    ];
    for &(status, body, want_code) in cases {
        let f = ProviderErrors::from_http(status, body, &[], "m", now, Some("SECRET"));
        assert_eq!(want_code, f.code, "status={status} body={body}");
    }

    // Retry-After header parsing & clamping.
    let f_ra = ProviderErrors::from_http(
        429,
        "{}",
        &[("Retry-After".to_string(), "12".to_string())],
        "m",
        now,
        None,
    );
    assert_eq!(Some(now + ChronoDuration::seconds(12)), f_ra.retry_at);
    assert_eq!(
        Some(Duration::from_secs(12)),
        ProviderErrors::retry_delay(&f_ra, 0, now)
    );
    // Retry delay > MAX_RETRY_WAIT (20s) returns None (do not stall UI).
    let f_long = ChatFailure::new("rate", "wait", Some(now + ChronoDuration::seconds(60)));
    assert!(ProviderErrors::retry_delay(&f_long, 0, now).is_none());
    assert!(ProviderErrors::retry_delay(&f_ra, ProviderErrors::MAX_RETRIES, now).is_none());

    // Stream errors map to expected codes.
    assert_eq!(
        "overloaded",
        ProviderErrors::from_stream(&ProviderStreamError::new("overloaded_error", "busy"), "m", now, None).code
    );
    assert_eq!(
        "rate",
        ProviderErrors::from_stream(&ProviderStreamError::new("RESOURCE_EXHAUSTED", "limit"), "m", now, None).code
    );
    assert_eq!(
        "auth",
        ProviderErrors::from_stream(&ProviderStreamError::new("authentication_error", "bad"), "m", now, None).code
    );

    // Low quota warning triggers below 10% remaining, stays None at >= 10%.
    let low_hdrs = vec![
        ("x-ratelimit-limit-requests".to_string(), "100".to_string()),
        ("x-ratelimit-remaining-requests".to_string(), "5".to_string()),
    ];
    assert!(ProviderErrors::low_quota_warning(&low_hdrs).is_some());
    let ok_hdrs = vec![
        ("x-ratelimit-limit-requests".to_string(), "100".to_string()),
        ("x-ratelimit-remaining-requests".to_string(), "50".to_string()),
    ];
    assert!(ProviderErrors::low_quota_warning(&ok_hdrs).is_none());

    // AiBudget warning at >= 80% and stop at 100% for both requests and tokens.
    let u_ok = AiUsage {
        day: "2026-11-10".into(),
        requests: 7,
        tokens: 500,
        request_limit: 10,
        token_limit: 1000,
    };
    assert!(AiBudget::check(&u_ok).is_none());
    assert!(AiBudget::warning(&u_ok).is_none());

    let u_warn = AiUsage {
        requests: 8,
        ..u_ok.clone()
    };
    assert!(AiBudget::check(&u_warn).is_none());
    assert!(AiBudget::warning(&u_warn).is_some());

    let u_over_req = AiUsage {
        requests: 10,
        ..u_ok.clone()
    };
    assert_eq!("budget", AiBudget::check(&u_over_req).unwrap().code);

    let u_over_tok = AiUsage {
        requests: 1,
        tokens: 1000,
        ..u_ok
    };
    assert_eq!("budget", AiBudget::check(&u_over_tok).unwrap().code);
}

#[test]
fn provider_http_retries_server_error_recovery_and_no_retry_cases() {
    let now = DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let tr = Arc::new(RecordingTransport::new());
    tr.push_status(503, r#"{"error":{"message":"busy"}}"#, Vec::new());
    tr.push_ok("recovered", Vec::new());

    let http = ProviderHttp::with_transport(
        tr.clone(),
        Some(Arc::new(move || now)),
        Some(Arc::new(|_, _| true)),
    );
    let mut retries = 0;
    let mut hook = |_: &ChatFailure, _: Duration| retries += 1;
    let resp = http
        .send(
            || br_core::HttpRequest::post_json("https://example.invalid/x", b"{}".to_vec()),
            "https://example.invalid",
            "m",
            None,
            Some(&mut hook),
            &CancelToken::none(),
        )
        .expect("should recover on retry");
    assert_eq!(200, resp.status);
    assert_eq!(1, retries);
    assert_eq!(2, tr.recorded().len());

    // Non-retryable 401 fails immediately with 0 retries.
    tr.push_status(401, r#"{"error":{"message":"bad key"}}"#, Vec::new());
    let mut retries2 = 0;
    let mut hook2 = |_: &ChatFailure, _: Duration| retries2 += 1;
    let err = http
        .send(
            || br_core::HttpRequest::post_json("https://example.invalid/x", b"{}".to_vec()),
            "https://example.invalid",
            "m",
            None,
            Some(&mut hook2),
            &CancelToken::none(),
        )
        .unwrap_err();
    assert_eq!("auth", err.0.code);
    assert_eq!(0, retries2);
}

#[test]
fn chat_service_first_share_same_chat_new_chat_deny_and_share_levels() {
    let h = ChatHarness::new(Some(Arc::new(FixedRedactor::ok())));
    let t = *h.now.lock().unwrap() - ChronoDuration::minutes(10);
    let id = h
        .store
        .add_screen_capture(&screen(t, "chrome.exe", "Acme invoice", true))
        .unwrap();
    h.store
        .set_embedding(&id, &vec_seeded(1), &vec_info())
        .unwrap();
    h.embedder
        .map
        .lock()
        .unwrap()
        .insert("invoice".to_string(), vec_seeded(1));

    // Share level = metadata: approval card item hides title (`""`) and omits title text.
    h.settings.lock().unwrap().ai.share = AiShare::METADATA.to_string();
    h.provider.push(ScriptProvider::search("invoice", 1));
    h.provider.push(ScriptProvider::answer("Metadata answer", "end"));
    let evs_meta = h.send(None, "find my invoice");
    let chat_id = evs_meta
        .iter()
        .find(|e| e.event_type == "start")
        .unwrap()
        .chat_id
        .clone()
        .unwrap();
    let app_meta = evs_meta
        .iter()
        .find(|e| e.event_type == "approval")
        .expect("approval event");
    let meta_item = &app_meta.items.as_ref().unwrap()[0];
    assert_eq!("", meta_item.title, "metadata card must hide capture title");
    assert!(!meta_item.text.contains("Acme invoice"));
    assert_eq!(1, h.transport.recorded().len());
    let tok_meta = app_meta.token.clone().unwrap();
    let evs_meta_ok = h.approve(&tok_meta, &[]);
    assert!(evs_meta_ok.iter().any(|e| e.event_type == "done"));
    assert_eq!(2, h.transport.recorded().len());

    // Approval at metadata does NOT cover higher share level (text): must ask again!
    h.settings.lock().unwrap().ai.share = AiShare::TEXT.to_string();
    h.provider.push(ScriptProvider::search("invoice", 1));
    h.provider.push(ScriptProvider::answer("Found Acme invoice", "end"));
    let evs_up = h.send(Some(&chat_id), "show text now");
    let app_text = evs_up
        .iter()
        .find(|e| e.event_type == "approval")
        .expect("raising share level must ask approval again");
    assert!(app_text.items.as_ref().unwrap()[0].title.contains("Acme invoice"));
    let tok_text = app_text.token.clone().unwrap();
    let evs_text_ok = h.approve(&tok_text, &[]);
    assert!(evs_text_ok.iter().any(|e| e.event_type == "text" && e.text.as_deref() == Some("Found Acme invoice")));
    assert!(evs_text_ok.iter().any(|e| e.event_type == "done"));

    // Follow-up in the SAME chat at the SAME share level citing the SAME approved capture skips approval!
    h.provider.push(ScriptProvider::search("invoice", 1));
    h.provider.push(ScriptProvider::answer("Same invoice again", "end"));
    let evs3 = h.send(Some(&chat_id), "check again");
    assert!(!evs3.iter().any(|e| e.event_type == "approval"));
    assert!(evs3.iter().any(|e| e.event_type == "done"));

    // A NEW chat must ask for approval again.
    h.provider.push(ScriptProvider::search("invoice", 1));
    let evs_new = h.send(None, "new chat invoice");
    let tok_deny = evs_new
        .iter()
        .find(|e| e.event_type == "approval")
        .expect("new chat needs approval")
        .token
        .clone()
        .unwrap();

    // Deny tells model the user declined and sends no capture details.
    h.provider.push(ScriptProvider::answer("Okay, skipped.", "end"));
    let evs_denied = h.deny(&tok_deny);
    assert!(evs_denied.iter().any(|e| e.event_type == "done"));
    let last_tool = h.provider.tool_outputs.lock().unwrap().last().unwrap().clone();
    assert!(!last_tool.text.contains("Acme invoice"));
}

#[test]
fn chat_service_images_redactor_and_stored_redacted_text() {
    let h = ChatHarness::new(Some(Arc::new(FixedRedactor::ok())));
    h.settings.lock().unwrap().ai.share = AiShare::TEXT_IMAGES.to_string();
    let t = *h.now.lock().unwrap() - ChronoDuration::minutes(5);
    let mut s = screen(t, "chrome.exe", "Bank email alice@example.com", true);
    s.text = Some("password: secret123".to_string());
    let id = h.store.add_screen_capture(&s).unwrap();
    h.store
        .set_embedding(&id, &vec_seeded(3), &vec_info())
        .unwrap();
    h.embedder
        .map
        .lock()
        .unwrap()
        .insert("Bank".to_string(), vec_seeded(3));

    h.provider.push(ScriptProvider::search("Bank", 1));
    h.provider.push(ScriptProvider::answer("Reviewed", "end"));
    let evs = h.send(None, "check bank");
    let app = evs.iter().find(|e| e.event_type == "approval").unwrap();
    let item = &app.items.as_ref().unwrap()[0];
    assert!(!item.text.contains("alice@example.com"));
    assert!(!item.text.contains("secret123"));
    assert_eq!(Some(FixedRedactor::REDACTED_BYTES.to_vec()), item.image);

    let tok = app.token.as_deref().unwrap();
    let evs_app = h.approve(tok, &[]);
    assert!(evs_app.iter().any(|e| e.event_type == "done"));
    let out = h.provider.tool_outputs.lock().unwrap().last().unwrap().clone();
    assert_eq!(1, out.images.len());
    assert_eq!(FixedRedactor::REDACTED_BYTES, out.images[0].bytes.as_slice());
}

#[test]
fn chat_service_budget_retries_auth_offline_low_quota_order_and_rollback() {
    let h = ChatHarness::new(None);
    h.settings.lock().unwrap().ai.daily_requests = 5;

    // 1) G5 Quota_warning_comes_before_the_answer: low-quota header emits `warning` (code="ratelimit")
    // BEFORE any `text` event.
    h.transport.push_ok(
        "",
        vec![
            ("x-ratelimit-limit-requests".into(), "100".into()),
            ("x-ratelimit-remaining-requests".into(), "2".into()),
        ],
    );
    h.provider.push(ScriptProvider::answer("Low quota answer", "end"));
    let evs = h.send(None, "hello quota");
    let warn_idx = evs
        .iter()
        .position(|e| e.event_type == "warning" && e.code.as_deref() == Some("ratelimit"))
        .expect("ratelimit warning event");
    let text_idx = evs
        .iter()
        .position(|e| e.event_type == "text")
        .expect("text event");
    assert!(
        warn_idx < text_idx,
        "ratelimit warning ({warn_idx}) must precede text ({text_idx})"
    );

    // 2) Retries on 503 then succeeds, emitting a retry warning event.
    h.transport
        .push_status(503, r#"{"error":{"message":"temp"}}"#, Vec::new());
    h.transport.push_ok("", Vec::new());
    h.provider.push(ScriptProvider::answer("After retry", "end"));
    let evs = h.send(None, "retry please");
    assert!(evs.iter().any(|e| e.event_type == "warning" && e.code.as_deref() == Some("retry")));
    assert!(evs.iter().any(|e| e.event_type == "done"));

    // 3) Repeated 429 rate-limit exhausts retries, emits final `rate` error with `retry_at`,
    // and does not save the failed chat.
    let chats_before_rate = h.service.list(50).len();
    for _ in 0..=ProviderErrors::MAX_RETRIES {
        h.transport.push_status(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
            vec![("Retry-After".into(), "5".into())],
        );
    }
    let evs_rate = h.send(None, "rate limited");
    let retry_warns = evs_rate
        .iter()
        .filter(|e| e.event_type == "warning" && e.code.as_deref() == Some("retry"))
        .count();
    assert_eq!(ProviderErrors::MAX_RETRIES as usize, retry_warns);
    let rate_err = evs_rate
        .iter()
        .find(|e| e.event_type == "error" && e.code.as_deref() == Some("rate"))
        .expect("final rate error");
    let now = *h.now.lock().unwrap();
    assert_eq!(Some(now + ChronoDuration::seconds(5)), rate_err.retry_at);
    assert_eq!(chats_before_rate, h.service.list(50).len());

    // 4) 401 auth error scrubs the API key from the error event, does not retry, and rolls back.
    let chats_before = h.service.list(50).len();
    let reqs_before_auth = h.transport.recorded().len();
    h.transport.push_status(
        401,
        r#"{"error":{"message":"invalid key AIzaSy-test rejected"}}"#,
        Vec::new(),
    );
    let evs = h.send(None, "will fail auth");
    let auth_ev = evs
        .iter()
        .find(|e| e.event_type == "error" && e.code.as_deref() == Some("auth"))
        .expect("auth error");
    assert!(
        !auth_ev.text.as_deref().unwrap_or("").contains("AIzaSy-test"),
        "API key must be scrubbed from error event: {:?}",
        auth_ev.text
    );
    assert_eq!(reqs_before_auth + 1, h.transport.recorded().len(), "auth must not retry");
    assert_eq!(chats_before, h.service.list(50).len());

    // 5) Offline transport emits offline error.
    h.transport.push_err(|| TransportError::Offline("down".into()));
    let evs = h.send(None, "offline test");
    assert!(evs.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("offline")));

    // 6) Exhaust daily request budget -> blocked before hitting HTTP transport.
    let today = AiBudget::today(*h.now.lock().unwrap());
    h.store.add_ai_usage(&today, 5, 0).unwrap();
    let reqs_before = h.transport.recorded().len();
    let evs = h.send(None, "over budget");
    assert!(evs.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("budget")));
    assert_eq!(reqs_before, h.transport.recorded().len());
}

#[test]
fn chat_service_refusal_truncation_rounds_too_many_calls_locked_and_validation() {
    let h = ChatHarness::new(None);
    let t = *h.now.lock().unwrap() - ChronoDuration::minutes(5);
    let id = h
        .store
        .add_screen_capture(&screen(t, "app.exe", "token-expiry-doc", false))
        .unwrap();
    h.store.set_embedding(&id, &vec_seeded(77), &vec_info()).unwrap();
    h.embedder
        .map
        .lock()
        .unwrap()
        .insert("token-expiry-doc".into(), vec_seeded(77));

    // Validation: empty, > 4000 chars, unknown chat id, bad token.
    let evs = h.send(None, "   ");
    assert!(evs.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("bad_request")));
    let big = "a".repeat(ChatService::MAX_QUESTION_CHARS + 1);
    let evs = h.send(None, &big);
    assert!(evs.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("bad_request")));
    let evs_unk = h.send(Some("no-such-chat"), "hello");
    assert!(evs_unk.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("not_found")));
    let evs_bad_tok = h.approve("no-such-token", &[]);
    assert!(evs_bad_tok.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("token")));

    // Approval token expires after APPROVAL_LIFETIME_SECS + 1 with only 1 HTTP request recorded.
    let reqs_before_exp = h.transport.recorded().len();
    h.provider.push(ScriptProvider::search("token-expiry-doc", 1));
    let evs_exp = h.send(None, "find expiry doc");
    let tok_exp = evs_exp
        .iter()
        .find(|e| e.event_type == "approval")
        .unwrap()
        .token
        .clone()
        .unwrap();
    assert_eq!(reqs_before_exp + 1, h.transport.recorded().len());
    *h.now.lock().unwrap() += ChronoDuration::seconds(ChatService::APPROVAL_LIFETIME_SECS + 1);
    let evs_exp_res = h.approve(&tok_exp, &[]);
    assert!(evs_exp_res.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("token")));
    assert_eq!(reqs_before_exp + 1, h.transport.recorded().len());

    // Refusal stop_reason with empty text emits an error with code="refused".
    h.provider.push(ScriptProvider::answer("", "refusal"));
    let evs = h.send(None, "refuse me");
    assert!(evs.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("refused")));

    // max_tokens stop_reason emits a truncation warning.
    h.provider.push(ScriptProvider::answer("Cut off", "max_tokens"));
    let evs = h.send(None, "truncate me");
    assert!(evs.iter().any(|e| e.event_type == "warning" && e.code.as_deref() == Some("truncated")));

    // Too many calls in one round: only MAX_CALLS_PER_ROUND searches run and excess get "Too many searches".
    let h_calls = ChatHarness::new(None);
    h_calls
        .provider
        .push(ScriptProvider::search("nonexistent-term-xyz", ChatService::MAX_CALLS_PER_ROUND + 2));
    h_calls.provider.push(ScriptProvider::answer("done after cap", "end"));
    let evs_cap = h_calls.send(None, "too many calls");
    let search_count = evs_cap.iter().filter(|e| e.event_type == "search").count();
    assert_eq!(ChatService::MAX_CALLS_PER_ROUND, search_count);
    let outputs = h_calls.provider.tool_outputs.lock().unwrap().clone();
    assert_eq!(ChatService::MAX_CALLS_PER_ROUND + 2, outputs.len());
    assert!(
        outputs.last().unwrap().text.contains("Too many searches"),
        "expected excess tool output to mention 'Too many searches', got {:?}",
        outputs.last().unwrap().text
    );
    assert_eq!("done", evs_cap.last().unwrap().event_type);

    // Round limit: MAX_ROUNDS consecutive tool-call rounds emits warning with code="rounds".
    let h_rounds = ChatHarness::new(None);
    for _ in 0..ChatService::MAX_ROUNDS {
        h_rounds
            .provider
            .push(ScriptProvider::search("nonexistent-term-xyz", 1));
    }
    let evs_rounds = h_rounds.send(None, "loop forever");
    assert!(
        evs_rounds
            .iter()
            .any(|e| e.event_type == "warning" && e.code.as_deref() == Some("rounds")),
        "expected warning code='rounds': {evs_rounds:?}"
    );
    assert_eq!("done", evs_rounds.last().unwrap().event_type);

    // Mid-stream ProviderStreamError emits error event.
    let h_err = ChatHarness::new(None);
    h_err
        .provider
        .push_err(SseReadError::Stream(ProviderStreamError::new(
            "overloaded_error",
            "stream overloaded",
        )));
    let evs = h_err.send(None, "stream fail");
    assert!(evs.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("overloaded")));
}

#[test]
fn chat_service_later_searches_removed_capture_history_off_and_model_change() {
    let h = ChatHarness::new(None);
    let t = *h.now.lock().unwrap() - ChronoDuration::minutes(10);
    let id1 = h
        .store
        .add_screen_capture(&screen(t, "app1.exe", "alpha project", false))
        .unwrap();
    let id2 = h
        .store
        .add_screen_capture(&screen(t + ChronoDuration::minutes(1), "app2.exe", "beta project", false))
        .unwrap();
    h.store.set_embedding(&id1, &vec_seeded(1), &vec_info()).unwrap();
    h.store.set_embedding(&id2, &vec_seeded(2), &vec_info()).unwrap();
    h.embedder.map.lock().unwrap().insert("alpha".into(), vec_seeded(1));
    h.embedder.map.lock().unwrap().insert("beta".into(), vec_seeded(2));

    // Turn 1: search alpha -> approve id1.
    h.provider.push(ScriptProvider::search_in("alpha", "app1.exe"));
    h.provider.push(ScriptProvider::answer("Saw alpha", "end"));
    let evs = h.send(None, "alpha?");
    let chat_id = evs.iter().find(|e| e.event_type == "start").unwrap().chat_id.clone().unwrap();
    let tok = evs.iter().find(|e| e.event_type == "approval").unwrap().token.clone().unwrap();
    let evs_a0 = h.approve(&tok, &[]);
    assert!(evs_a0.iter().any(|e| e.event_type == "done"));

    // B19 (within ONE turn): round 1 searches approved id1 (no card), round 2 searches unreviewed id2
    // -> MUST ask approval listing ONLY id2 with MORE_CAPTURES_NOTE, and NO body has "beta project" before approve!
    h.provider.push(ScriptProvider::search_in("alpha", "app1.exe"));
    h.provider.push(ScriptProvider::search_in("beta", "app2.exe"));
    h.provider.push(ScriptProvider::answer("Declined beta", "end"));
    let evs_b19_deny = h.send(Some(&chat_id), "check alpha then beta (decline)");
    let app_b19 = evs_b19_deny
        .iter()
        .find(|e| e.event_type == "approval")
        .expect("round-2 unreviewed capture must ask approval");
    assert_eq!(Some(ChatService::MORE_CAPTURES_NOTE), app_b19.text.as_deref());
    let b19_ids: Vec<&str> = app_b19
        .items
        .as_ref()
        .unwrap()
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(vec![id2.as_str()], b19_ids);
    assert!(
        h.transport
            .recorded()
            .iter()
            .all(|r| !r.body.contains("beta project")),
        "unapproved beta project leaked before approval"
    );
    // Deny round-2 approval -> "beta project" stays absent from ALL request bodies.
    let evs_declined = h.deny(app_b19.token.as_deref().unwrap());
    assert!(evs_declined.iter().any(|e| e.event_type == "done"));
    assert!(
        h.transport
            .recorded()
            .iter()
            .all(|r| !r.body.contains("beta project")),
        "declined beta project leaked into request body"
    );

    // B19 approve variant: round 1 approves alpha, round 2 unreviewed beta asks approval.
    // After approval, last body contains "beta project" and turn ends done.
    // Re-running both searches in a later turn asks NO approval and ends done.
    let h_b19 = ChatHarness::new(None);
    let b_id1 = h_b19.store.add_screen_capture(&screen(t, "app1.exe", "alpha project", false)).unwrap();
    let b_id2 = h_b19.store.add_screen_capture(&screen(t, "app2.exe", "beta project", false)).unwrap();
    h_b19.store.set_embedding(&b_id1, &vec_seeded(1), &vec_info()).unwrap();
    h_b19.store.set_embedding(&b_id2, &vec_seeded(2), &vec_info()).unwrap();
    h_b19.embedder.map.lock().unwrap().insert("alpha".into(), vec_seeded(1));
    h_b19.embedder.map.lock().unwrap().insert("beta".into(), vec_seeded(2));

    h_b19.provider.push(ScriptProvider::search_in("alpha", "app1.exe"));
    h_b19.provider.push(ScriptProvider::answer("Saw alpha", "end"));
    let evs1 = h_b19.send(None, "alpha?");
    let cid_b19 = evs1.iter().find(|e| e.event_type == "start").unwrap().chat_id.clone().unwrap();
    let tok1 = evs1.iter().find(|e| e.event_type == "approval").unwrap().token.clone().unwrap();
    let evs_a1 = h_b19.approve(&tok1, &[]);
    assert!(evs_a1.iter().any(|e| e.event_type == "done"));

    h_b19.provider.push(ScriptProvider::search_in("alpha", "app1.exe"));
    h_b19.provider.push(ScriptProvider::search_in("beta", "app2.exe"));
    h_b19.provider.push(ScriptProvider::answer("Approved beta answer", "end"));
    let evs2_b = h_b19.send(Some(&cid_b19), "check alpha then beta (approve)");
    let app_b2 = evs2_b
        .iter()
        .find(|e| e.event_type == "approval")
        .expect("round-2 unreviewed capture must ask approval");
    let tok_r2 = app_b2.token.as_ref().unwrap();
    let evs_a2 = h_b19.approve(tok_r2, &[]);
    assert_eq!("done", evs_a2.last().unwrap().event_type);
    assert!(
        h_b19.transport.recorded().last().unwrap().body.contains("beta project"),
        "approved beta project must be in last request body"
    );

    // A later turn that re-runs both searches asks no approval and ends done.
    h_b19.provider.push(ScriptProvider::search_in("alpha", "app1.exe"));
    h_b19.provider.push(ScriptProvider::search_in("beta", "app2.exe"));
    h_b19.provider.push(ScriptProvider::answer("Both already approved", "end"));
    let evs3_b = h_b19.send(Some(&cid_b19), "both again");
    assert!(!evs3_b.iter().any(|e| e.event_type == "approval"));
    assert_eq!("done", evs3_b.last().unwrap().event_type);

    // S3 B3 + S3 B2: first search of a later turn finds unreviewed id2 -> remove id2 on card ->
    // id2 stays excluded across TWO subsequent turns WITHOUT showing a card; after forget_approvals()
    // a fresh card offers id2 again!
    h.provider.push(ScriptProvider::search_in("beta", "app2.exe"));
    h.provider.push(ScriptProvider::answer("Beta removed", "end"));
    let evs2 = h.send(Some(&chat_id), "now beta?");
    let app2 = evs2
        .iter()
        .find(|e| e.event_type == "approval")
        .expect("later turn unreviewed capture must ask approval");
    assert_eq!(Some(ChatService::MORE_CAPTURES_NOTE), app2.text.as_deref());
    let tok2 = app2.token.clone().unwrap();
    let evs_app2 = h.approve(&tok2, &[&id2]);
    assert!(evs_app2.iter().any(|e| e.event_type == "done"));
    assert!(
        h.transport
            .recorded()
            .iter()
            .all(|r| !r.body.contains("beta project")),
        "removed id2 must not be sent"
    );
    assert!(
        h.transport
            .recorded()
            .iter()
            .any(|r| r.body.contains("alpha project")),
        "approved alpha project must still be present in transcript"
    );

    for turn_i in 0..2 {
        h.provider.push(ScriptProvider::search_in("beta", "app2.exe"));
        h.provider.push(ScriptProvider::answer("Still no beta", "end"));
        let evs_later = h.send(Some(&chat_id), &format!("beta again {turn_i}"));
        assert!(
            !evs_later.iter().any(|e| e.event_type == "approval"),
            "removed capture must not re-prompt on later turn {turn_i}"
        );
        assert!(
            h.transport
                .recorded()
                .iter()
                .all(|r| !r.body.contains("beta project")),
            "removed capture must stay out on later turn {turn_i}"
        );
    }

    h.service.forget_approvals();
    h.provider.push(ScriptProvider::search_in("beta", "app2.exe"));
    let evs_reoffer = h.send(Some(&chat_id), "beta after forget");
    let app_reoffer = evs_reoffer
        .iter()
        .find(|e| e.event_type == "approval")
        .expect("forget_approvals must re-offer id2 on a fresh card");
    assert!(app_reoffer.items.as_ref().unwrap().iter().any(|i| i.id == id2));

    // B5: Model change starts fresh with warning code="fresh" + FRESH_NOTE, and new request body
    // lacks both the old question ("alpha?") and the old capture text ("alpha project").
    h.settings.lock().unwrap().ai.model = "gemini-2.5-pro".to_string();
    h.provider.push(ScriptProvider::answer("Fresh model reply", "end"));
    let evs_fresh = h.send(Some(&chat_id), "after model change");
    let fresh_warn = evs_fresh
        .iter()
        .find(|e| e.event_type == "warning" && e.code.as_deref() == Some("fresh"))
        .expect("fresh warning");
    assert_eq!(Some(ChatService::FRESH_NOTE), fresh_warn.text.as_deref());
    let recorded = h.transport.recorded();
    assert!(
        recorded[..recorded.len() - 1].iter().any(|r| r.body.contains("alpha?")),
        "the old transcript carried the first question before the model change"
    );
    let last_body = &recorded.last().unwrap().body;
    assert!(!last_body.contains("alpha?"), "fresh transcript leaked old question");
    assert!(!last_body.contains("alpha project"), "fresh transcript leaked old capture");
    assert!(last_body.contains("after model change"), "fresh transcript must contain current prompt");

    // chatHistory=false keeps chats in RAM (list/get/rename/delete work) without persisting to DB.
    h.settings.lock().unwrap().ai.chat_history = false;
    h.provider.push(ScriptProvider::answer("Ephemeral", "end"));
    let evs_mem = h.send(None, "in memory only");
    let mem_id = evs_mem.iter().find(|e| e.event_type == "start").unwrap().chat_id.clone().unwrap();
    assert!(h.store.get_chat(&mem_id).unwrap().is_none(), "not in DB");
    assert!(h.service.get(&mem_id).is_some(), "visible in RAM");
    assert!(h.service.rename(&mem_id, "Renamed"));
    assert_eq!("Renamed", h.service.get(&mem_id).unwrap().chat.title);
    assert_eq!(1, h.service.delete(Some(&mem_id)));
    assert!(h.service.get(&mem_id).is_none());
}

#[test]
fn chat_service_purge_drops_pending_resume_rechecks_unsaved_and_running_turn() {
    let h = ChatHarness::new(None);
    let t = *h.now.lock().unwrap() - ChronoDuration::minutes(10);
    let id = h
        .store
        .add_screen_capture(&screen(t, "app.exe", "purge-target", false))
        .unwrap();
    h.store.set_embedding(&id, &vec_seeded(10), &vec_info()).unwrap();
    h.embedder.map.lock().unwrap().insert("purge-target".into(), vec_seeded(10));

    // 1) B4 #1: Purge drops pending approval token citing deleted capture and sends nothing more.
    h.provider.push(ScriptProvider::search("purge-target", 1));
    let evs = h.send(None, "find purge-target");
    let tok = evs.iter().find(|e| e.event_type == "approval").unwrap().token.clone().unwrap();
    let reqs_before = h.transport.recorded().len();
    h.service.forget_captures(|cid| cid == id, None, None);
    let evs_after = h.approve(&tok, &[]);
    assert!(evs_after.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("token")));
    assert_eq!(reqs_before, h.transport.recorded().len(), "refused approve must not send HTTP");

    // 2) B4 #3: Purge drops unsaved (in-memory) chats by id AND by time window, keeping other chats.
    h.settings.lock().unwrap().ai.chat_history = false;
    h.provider.push(ScriptProvider::answer("Unrelated chat", "end"));
    let evs_other = h.send(None, "unrelated");
    let other_id = evs_other.iter().find(|e| e.event_type == "start").unwrap().chat_id.clone().unwrap();

    h.provider.push(ScriptProvider::search("purge-target", 1));
    h.provider.push(ScriptProvider::answer("Cited it", "end"));
    let evs = h.send(None, "unsaved cite");
    let mem_id = evs.iter().find(|e| e.event_type == "start").unwrap().chat_id.clone().unwrap();
    let tok2 = evs.iter().find(|e| e.event_type == "approval").unwrap().token.clone().unwrap();
    let evs_a2 = h.approve(&tok2, &[]);
    assert!(evs_a2.iter().any(|e| e.event_type == "done"));
    assert!(h.service.get(&mem_id).is_some());
    h.service.forget_captures(|cid| cid == id, None, None);
    assert!(h.service.get(&mem_id).is_none());
    assert!(h.service.get(&other_id).is_some(), "unrelated in-memory chat must be kept");

    // Time-window drop on in-memory chat while keeping a chat outside the window.
    *h.now.lock().unwrap() += ChronoDuration::hours(2);
    let win_t = *h.now.lock().unwrap();
    h.provider.push(ScriptProvider::answer("In window", "end"));
    let evs_win = h.send(None, "window chat");
    let win_id = evs_win.iter().find(|e| e.event_type == "start").unwrap().chat_id.clone().unwrap();
    h.service.forget_captures(
        |_| false,
        Some(win_t - ChronoDuration::minutes(1)),
        Some(win_t + ChronoDuration::minutes(1)),
    );
    assert!(h.service.get(&win_id).is_none(), "in-window unsaved chat must be dropped");
    assert!(h.service.get(&other_id).is_some(), "out-of-window unsaved chat must stay");

    // 3) Clear_forgets_approvals_and_pending_tokens: service.clear() voids pending tokens and approvals.
    h.settings.lock().unwrap().ai.chat_history = true;
    h.provider.push(ScriptProvider::search("purge-target", 1));
    h.provider.push(ScriptProvider::answer("Approved once", "end"));
    let evs_c1 = h.send(None, "approve then clear");
    let cid_c = evs_c1.iter().find(|e| e.event_type == "start").unwrap().chat_id.clone().unwrap();
    let tok_c = evs_c1.iter().find(|e| e.event_type == "approval").unwrap().token.clone().unwrap();
    let evs_ac = h.approve(&tok_c, &[]);
    assert!(evs_ac.iter().any(|e| e.event_type == "done"));
    assert!(h.service.get(&cid_c).unwrap().approved);
    h.provider.push(ScriptProvider::search("purge-target", 1));
    let evs_pend = h.send(None, "pending before clear");
    let tok_pend = evs_pend.iter().find(|e| e.event_type == "approval").unwrap().token.clone().unwrap();
    h.service.clear();
    assert!(!h.service.get(&cid_c).unwrap().approved, "clear must drop approval");
    let evs_cleared_tok = h.approve(&tok_pend, &[]);
    assert!(evs_cleared_tok.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("token")));
    h.provider.push(ScriptProvider::search("purge-target", 1));
    let evs_reask = h.send(Some(&cid_c), "after clear");
    assert!(evs_reask.iter().any(|e| e.event_type == "approval"), "after clear, chat must ask again");

    // 4) B4 #4: Purge stops a running turn citing the purged capture with error code="stopped" and saves nothing.
    h.provider.push(ScriptProvider::search("purge-target", 1));
    let evs = h.send(None, "running cite");
    let run_chat_id = evs.iter().find(|e| e.event_type == "start").unwrap().chat_id.clone().unwrap();
    let tok3 = evs.iter().find(|e| e.event_type == "approval").unwrap().token.clone().unwrap();

    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let entered_bg = entered.clone();
    *h.provider.on_stream.lock().unwrap() = Some(Arc::new(move |ct| {
        {
            let (m, cv) = &*entered_bg;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        ct.wait_timeout(Duration::from_secs(5));
    }));
    h.provider.push(ScriptProvider::answer("Should be cancelled", "end"));
    let svc_bg = h.service.clone();
    let run_h = thread::spawn(move || {
        let mut out = Vec::new();
        svc_bg.approve(&tok3, &HashSet::new(), &mut |e| out.push(e), &CancelToken::none(), None);
        out
    });
    {
        let (m, cv) = &*entered;
        let g = m.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |v| !*v)
            .unwrap();
        assert!(*g);
    }
    h.service.forget_captures(|cid| cid == id, None, None);
    let evs_run = run_h.join().unwrap();
    assert!(!evs_run.iter().any(|e| e.event_type == "done"));
    let stopped = evs_run
        .iter()
        .find(|e| e.event_type == "error" && e.code.as_deref() == Some("stopped"))
        .expect("expected error code='stopped'");
    assert_eq!(
        Some("The answer was stopped: captures this chat quotes were deleted."),
        stopped.text.as_deref()
    );
    assert!(
        h.store.get_chat(&run_chat_id).unwrap().is_none(),
        "stopped turn must not save the chat"
    );
    *h.provider.on_stream.lock().unwrap() = None;

    // 5) B4 #2: Store purge WITHOUT calling forget_captures — resume rechecks store and never sends deleted capture!
    let h5 = ChatHarness::new(None);
    let id_recheck = h5
        .store
        .add_screen_capture(&screen(t, "app.exe", "SECRET-RECHECK-TITLE", false))
        .unwrap();
    h5.store
        .set_embedding(&id_recheck, &vec_seeded(11), &vec_info())
        .unwrap();
    h5.embedder
        .map
        .lock()
        .unwrap()
        .insert("recheck".into(), vec_seeded(11));
    h5.provider.push(ScriptProvider::search("recheck", 1));
    h5.provider.push(ScriptProvider::answer("Checked", "end"));
    let evs_rc = h5.send(None, "find recheck");
    let tok_rc = evs_rc
        .iter()
        .find(|e| e.event_type == "approval")
        .unwrap()
        .token
        .clone()
        .unwrap();
    // Purge in store directly — do NOT call forget_captures().
    h5.store.purge(&PurgeScope::for_id(&id_recheck)).unwrap();
    let evs_rc_app = h5.approve(&tok_rc, &[]);
    assert!(evs_rc_app.iter().any(|e| e.event_type == "done"));
    let recorded5 = h5.transport.recorded();
    assert_eq!(2, recorded5.len(), "the approve must resume the turn with one more request");
    assert!(
        recorded5.iter().all(|r| !r.body.contains("SECRET-RECHECK-TITLE")),
        "no request may carry the purged capture"
    );
    let last_req = recorded5.last().unwrap();
    assert!(
        !last_req.body.contains("SECRET-RECHECK-TITLE"),
        "resume must recheck store and omit purged capture: {}",
        last_req.body
    );
}

#[test]
fn chat_verb_gates_gemini_e2e_lock_endpoint_ai_off_and_panic() {
    let srv = FakeHttpServer::start();
    let srv2 = FakeHttpServer::start();
    let f = PortFixture::with_opts(PortFixtureOpts::default());
    let mut ui_s = PortFixture::ui();

    // Seed an embedded capture so Gemini tool calls can cite it.
    let cap_id = f
        .store
        .add_screen_capture(&screen(
            Utc::now() - ChronoDuration::minutes(5),
            "chrome.exe",
            "Gemini Invoice 42",
            false,
        ))
        .unwrap();
    f.store
        .set_embedding(&cap_id, &vec_seeded(42), &vec_info())
        .unwrap();
    f.embedder
        .map
        .lock()
        .unwrap()
        .insert("invoice".into(), vec_seeded(42));

    // Item 38: point the provider at the local fake server before the first chat verb, so even a
    // regression in the off gate cannot reach a real provider host.
    f.set_settings(json!({
        "ai": {
            "enabled": false,
            "provider": "gemini",
            "endpoint": srv.base_url(),
            "model": "gemini-2.5-flash"
        }
    }));

    // AI off refuses chat send.
    let evs = f.call_stream(&mut ui_s, r#"chat send {"text":"hi"}"#, &CancelToken::none());
    assert!(!evs[0].0);
    assert_eq!(CommandProcessor::ERR_AI_OFF, evs[0].2.as_deref().unwrap());

    // Configure Gemini against local FakeHttpServer.
    let api_key = "AIzaSy-e2e-SECRET-KEY";
    f.set_settings(json!({
        "ai": {
            "enabled": true,
            "provider": "gemini",
            "endpoint": srv.base_url(),
            "model": "gemini-2.5-flash",
            "chatHistory": true
        }
    }));
    let (ok, _, _) = f.call(&mut ui_s, &format!("ai setkey {api_key}"));
    assert!(ok);

    // 1) Gemini end-to-end with tool call + thoughtSignature `sig1` + approve + header key check.
    let sse_call = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"search_history\",\"args\":{\"query\":\"invoice\"}},\"thoughtSignature\":\"sig1\"}]},\"finishReason\":\"STOP\"}]}\n\n";
    let sse_ans = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Found Invoice 42\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":5}}\n\n";
    srv.push_response(200, "text/event-stream", Vec::new(), sse_call);
    srv.push_response(200, "text/event-stream", Vec::new(), sse_ans);

    let evs = f.call_stream(&mut ui_s, r#"chat send {"text":"find invoice"}"#, &CancelToken::none());
    assert!(evs.iter().all(|(ok, _, _)| *ok));
    let ev_types: Vec<&str> = evs
        .iter()
        .filter_map(|(_, d, _)| d.as_ref().and_then(|v| v["type"].as_str()))
        .collect();
    assert_eq!(vec!["start", "search", "approval"], ev_types);
    let chat_id = evs
        .iter()
        .find_map(|(_, d, _)| {
            d.as_ref()
                .filter(|v| v["type"] == "start")
                .and_then(|v| v["chatId"].as_str())
                .map(String::from)
        })
        .expect("start event with chatId");
    let tok = evs
        .iter()
        .find_map(|(_, d, _)| {
            d.as_ref()
                .filter(|v| v["type"] == "approval")
                .and_then(|v| v["token"].as_str())
                .map(String::from)
        })
        .expect("approval token");

    let evs_app = f.call_stream(
        &mut ui_s,
        &format!("chat approve {}", json!({"token": tok})),
        &CancelToken::none(),
    );
    assert_eq!(
        "done",
        evs_app.last().unwrap().1.as_ref().unwrap()["type"],
        "done must be last event"
    );
    assert!(evs_app.iter().any(|(_, d, _)| {
        d.as_ref()
            .is_some_and(|v| v["type"] == "text" && v["text"] == "Found Invoice 42")
    }));
    // Key absent from every stream line, request URL, and audit log; present in x-goog-api-key header; sig1 echoed back.
    for (_, d, e) in evs.iter().chain(evs_app.iter()) {
        assert!(!d.as_ref().map(|v| v.to_string()).unwrap_or_default().contains(api_key));
        assert!(!e.as_deref().unwrap_or_default().contains(api_key));
    }
    let reqs = srv.requests();
    assert_eq!(2, reqs.len());
    for req in &reqs {
        assert_eq!(Some(api_key), req.header("x-goog-api-key"));
        assert!(!req.path.contains(api_key), "API key must never appear in URL: {}", req.path);
    }
    assert!(reqs[1].body.contains("sig1"), "Gemini thoughtSignature must round-trip");
    assert!(reqs[1].body.contains("functionResponse"), "reqs[1] must contain functionResponse");
    assert!(reqs[1].body.contains("Gemini Invoice 42"), "reqs[1] must contain capture title");
    let audit_entries = f.store.read_audit(50).unwrap();
    assert!(audit_entries.iter().any(|a| a.verb == "chat gemini" && a.result_count == 1));
    let audit_str = serde_json::to_string(&audit_entries).unwrap();
    assert!(!audit_str.contains(api_key));

    // Chat is now approved; list/get/rename work.
    let (ok, detail, _) = f.call(&mut ui_s, &format!("chat get {chat_id}"));
    assert!(ok);
    let detail = detail.unwrap();
    assert_eq!(true, detail["approved"]);
    assert_eq!(2, detail["messages"].as_array().unwrap().len());
    let (ok, _, _) = f.call(
        &mut ui_s,
        &format!("chat rename {}", json!({"id": chat_id, "title": "My Chat"})),
    );
    assert!(ok);
    let (ok, detail2, _) = f.call(&mut ui_s, &format!("chat get {chat_id}"));
    assert!(ok);
    assert_eq!("My Chat", detail2.unwrap()["chat"]["title"]);

    // AI usage requests is 2 after the initial turn.
    let (ok, ai_st, _) = f.call(&mut ui_s, "ai");
    assert!(ok);
    assert_eq!(2, ai_st.unwrap()["usage"]["requests"].as_i64().unwrap());

    // 2) B8 (`Lock_forgets_chat_approvals`): agent status lock event clears chat approvals;
    // next `chat send` on the same chat asks for approval again.
    f.agent.status.lock().unwrap().locked = true;
    f.agent.raise_status();
    let (ok, d_after_lock, _) = f.call(&mut ui_s, &format!("chat get {chat_id}"));
    assert!(ok);
    assert_eq!(false, d_after_lock.unwrap()["approved"]);
    f.agent.status.lock().unwrap().locked = false;
    srv.push_response(200, "text/event-stream", Vec::new(), sse_call);
    srv.push_response(200, "text/event-stream", Vec::new(), sse_ans);
    let evs_reask = f.call_stream(
        &mut ui_s,
        &format!("chat send {}", json!({"chat": chat_id, "text": "again after lock"})),
        &CancelToken::none(),
    );
    let tok_reask = evs_reask
        .iter()
        .find_map(|(_, d, _)| {
            d.as_ref()
                .filter(|v| v["type"] == "approval")
                .and_then(|v| v["token"].as_str())
                .map(String::from)
        })
        .expect("lock status event must force fresh approval");
    let evs_reapp = f.call_stream(
        &mut ui_s,
        &format!("chat approve {}", json!({"token": tok_reask})),
        &CancelToken::none(),
    );
    assert!(evs_reapp.iter().any(|(_, d, _)| d.as_ref().is_some_and(|v| v["type"] == "done")));
    let (ok, d_reapp, _) = f.call(&mut ui_s, &format!("chat get {chat_id}"));
    assert!(ok);
    assert_eq!(true, d_reapp.unwrap()["approved"]);

    // 3) A8 (`Changing_the_endpoint_asks_for_approval_and_the_key_again`):
    // switching ai.endpoint to srv2 drops approval and key binding without hitting any server.
    let reqs_before_ep = srv.requests().len();
    f.set_settings(json!({"ai": {"endpoint": srv2.base_url()}}));
    let (ok, d_ep, _) = f.call(&mut ui_s, &format!("chat get {chat_id}"));
    assert!(ok);
    assert_eq!(false, d_ep.unwrap()["approved"]);
    let evs_no_key = f.call_stream(
        &mut ui_s,
        &format!("chat send {}", json!({"chat": chat_id, "text": "after ep change"})),
        &CancelToken::none(),
    );
    assert!(!evs_no_key[0].0);
    assert_eq!(CommandProcessor::ERR_NO_API_KEY, evs_no_key[0].2.as_deref().unwrap());
    assert_eq!(reqs_before_ep, srv.requests().len());
    assert!(srv2.requests().is_empty());

    // Restore endpoint to srv1 and test locked gate on chat verbs.
    f.set_settings(json!({"ai": {"endpoint": srv.base_url()}}));
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, "chat list");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    let evs_lock = f.call_stream(&mut ui_s, r#"chat send {"text":"locked"}"#, &CancelToken::none());
    assert!(!evs_lock[0].0);
    assert_eq!(CommandProcessor::ERR_LOCKED, evs_lock[0].2.as_deref().unwrap());
    f.agent.unlocked.store(true, Ordering::Release);

    // 4) A7 (`Panic_deletes_saved_chats_and_approvals`): saved chat exists, lock + raise_panic -> store.chats() empty.
    assert!(!f.store.chats(50).unwrap().is_empty());
    f.agent.unlocked.store(false, Ordering::Release);
    f.agent.raise_panic();
    assert!(f.store.chats(50).unwrap().is_empty(), "panic must delete saved chats");
    f.agent.unlocked.store(true, Ordering::Release);

    // 5) A7 (`Ai_off_drops_a_turn_waiting_for_approval`): pending approval + ai off/on -> approve fails,
    // only 1 HTTP request sent, store.chats() stays empty.
    let reqs_before_off = srv.requests().len();
    srv.push_response(200, "text/event-stream", Vec::new(), sse_call);
    let evs_wait = f.call_stream(&mut ui_s, r#"chat send {"text":"pending turn"}"#, &CancelToken::none());
    let tok_wait = evs_wait
        .iter()
        .find_map(|(_, d, _)| {
            d.as_ref()
                .filter(|v| v["type"] == "approval")
                .and_then(|v| v["token"].as_str())
                .map(String::from)
        })
        .expect("pending approval token");
    assert_eq!(reqs_before_off + 1, srv.requests().len());
    let chats_before_off = f.store.chats(50).unwrap().len();
    f.set_settings(json!({"ai": {"enabled": false}}));
    f.set_settings(json!({"ai": {"enabled": true}}));
    let evs_dropped = f.call_stream(
        &mut ui_s,
        &format!("chat approve {}", json!({"token": tok_wait})),
        &CancelToken::none(),
    );
    assert!(evs_dropped.iter().any(|(_, d, _)| {
        d.as_ref().is_some_and(|v| v["type"] == "error" && v["code"] == "token")
    }));
    assert_eq!(reqs_before_off + 1, srv.requests().len());
    assert_eq!(chats_before_off, f.store.chats(50).unwrap().len());
    assert!(f.store.chats(50).unwrap().is_empty());

    // 6) chat delete then chat get = ERR_NOT_FOUND, plus its audit row.
    srv.push_response(200, "text/event-stream", Vec::new(), sse_ans);
    let evs_del = f.call_stream(&mut ui_s, r#"chat send {"text":"for delete"}"#, &CancelToken::none());
    let del_chat_id = evs_del
        .iter()
        .find_map(|(_, d, _)| d.as_ref().and_then(|v| v["chatId"].as_str()))
        .expect("chatId");
    let (ok, _, _) = f.call(&mut ui_s, &format!("chat delete {del_chat_id}"));
    assert!(ok);
    let (ok, _, err) = f.call(&mut ui_s, &format!("chat get {del_chat_id}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_FOUND, err.unwrap());
    let audit_after_del = f.store.read_audit(50).unwrap();
    assert!(audit_after_del.iter().any(|a| a.verb == "chat delete"));

    // Cover C# Auth_error's audit row ("chat gemini", 0) with a 401 in the verb test.
    srv.push_response(
        401,
        "application/json",
        Vec::new(),
        r#"{"error":{"message":"invalid API key"}}"#,
    );
    let evs_401 = f.call_stream(&mut ui_s, r#"chat send {"text":"will 401"}"#, &CancelToken::none());
    assert!(evs_401.iter().any(|(_, d, _)| d.as_ref().is_some_and(|v| v["type"] == "error")));
    let audit_after_401 = f.store.read_audit(50).unwrap();
    assert!(audit_after_401.iter().any(|a| a.verb == "chat gemini" && a.result_count == 0));
}
