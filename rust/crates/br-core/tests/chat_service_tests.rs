// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Chat redaction, share gate, ChatService, and chat verb tests.

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use br_contracts::{
    AiDefaults, AiShare, CaptureDetail, CaptureKind, Envelope, SearchHit, Settings, ThumbData,
};
use br_core::{
    ApiKeyStore, BasicTextRedactor, CancelToken, ChatDeps, ChatService, ChatShareGate,
    ClientSession, CommandDeps, CommandProcessor, DescribeError, DescribeRedactor, KeyVault,
    OutgoingImage, PassThroughRedactor, PipeKind, ProviderHttp, SettingsStore, ShareGateInput,
    SharePlan, SharedHit, TextRedactor,
};
use chrono::{DateTime, Utc};
use serde_json::json;

use common::a3::{FakeAgent, FakeEmbedder, FakeHttpServer, FakeThumbs, MockTransport};
use common::{TempHome, TempStore, audio, screen, vec_info, vec_seeded};

// ---- ChatRedactionTests ----

#[test]
fn redacts_sensitive_patterns() {
    let r = BasicTextRedactor;
    let cases = [
        ("password: MySecret1", "password: [redacted]"),
        ("api_key=abc123", "api_key=[redacted]"),
        ("contact alice@example.com now", "contact [redacted] now"),
        ("open https://github.com/org/secret?tok=1", "open https://github.com/[redacted]"),
        ("root https://github.com/", "root https://github.com"),
        ("IBAN DE89 3704 0044 0532 0130 00 ok", "IBAN [redacted] ok"),
        ("card 4111-2222-3333-4444 done", "card [redacted] done"),
        ("date 2026-11-10 kept", "date 2026-11-10 kept"),
        ("short 1234567 kept", "short 1234567 kept"),
        // The digits rule runs before the token rule, as in C#, so a long digit run inside a token is
        // masked on its own and the rest no longer counts as one token.
        ("tok abcdef1234567890ABCDEF1234 end", "tok abcdef[redacted]ABCDEF1234 end"),
        // ChatTests.cs Redacts_sensitive_patterns, verbatim.
        ("mail bob.smith+x@example.co.uk now", "mail [redacted] now"),
        ("see https://bank.example.com/acct/123?session=abc", "see https://bank.example.com/[redacted]"),
        ("open https://github.com", "open https://github.com"),
        ("password: hunter2 ok", "password: [redacted] ok"),
        ("API key=sk-live-abc", "API key=[redacted]"),
        ("card 4111 1111 1111 1111 exp", "card [redacted] exp"),
        ("call +44 20 7946 0958", "call [redacted]"),
        ("IBAN GB82 WEST 1234 5698 7654 32", "IBAN [redacted]"),
        ("token abcdefghijklmnopqrstuvwx1234", "token [redacted]"),
        ("meeting 2026-11-12 at 10:30 room 4012", "meeting 2026-11-12 at 10:30 room 4012"),
        ("", ""),
    ];
    for (input, expected) in cases {
        assert_eq!(expected, r.redact(input), "input={input}");
    }
}

// ---- ChatShareGateTests ----

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

struct RealImageRedactor {
    ok: bool,
    fail: bool,
}

impl DescribeRedactor for RealImageRedactor {
    fn redact(
        &self,
        _id: &str,
        mut image: OutgoingImage,
        _ct: &CancelToken,
    ) -> Result<OutgoingImage, DescribeError> {
        if self.fail {
            return Err(DescribeError::Message("ocr crashed".to_string()));
        }
        image.redacted = self.ok;
        Ok(image)
    }

    fn redacts_images(&self) -> bool {
        true
    }
}

#[test]
fn metadata_sends_no_title_or_text() {
    for share in [AiShare::METADATA, "everything"] {
        let hit = sample_hit("s_1", CaptureKind::Screen);
        let plan = ChatShareGate::build(ShareGateInput {
            hits: &[hit],
            share,
            max_hits: 5,
            max_images: 3,
            detail: &|_| None,
            thumb: &|_, _, _| None,
            image_redactor: None,
            text_redactor: &BasicTextRedactor,
            ct: &CancelToken::none(),
        });
        assert_eq!(AiShare::METADATA, plan.share, "share={share}");
        assert!(!plan.hits[0].text.contains("Inbox"), "share={share}");
        assert!(!plan.hits[0].text.contains("password"), "share={share}");
        assert!(plan.hits[0].image.is_none(), "share={share}");
    }
}

#[test]
fn text_is_redacted_and_has_no_images() {
    let hit = sample_hit("s_1", CaptureKind::Screen);
    let plan = ChatShareGate::build(ShareGateInput {
        hits: &[hit],
        share: AiShare::TEXT,
        max_hits: 5,
        max_images: 3,
        detail: &|_| None,
        thumb: &|_, _, _| {
            Some(ThumbData {
                id: "s_1".into(),
                mime: "image/webp".into(),
                width: 768,
                height: 432,
                bytes: vec![1, 2, 3],
            })
        },
        image_redactor: Some(&RealImageRedactor { ok: true, fail: false }),
        text_redactor: &BasicTextRedactor,
        ct: &CancelToken::none(),
    });
    assert!(plan.hits[0].text.contains("[redacted]"));
    assert!(!plan.hits[0].text.contains("alice@example.com"));
    assert!(!plan.hits[0].text.contains("secret123"));
    assert!(plan.hits[0].image.is_none());
}

#[test]
fn images_need_a_redactor_that_redacts() {
    let hit = sample_hit("s_1", CaptureKind::Screen);
    let thumb = |_: &str, _: i32, _: &CancelToken| {
        Some(ThumbData {
            id: "s_1".into(),
            mime: "image/webp".into(),
            width: 768,
            height: 432,
            bytes: vec![1, 2, 3],
        })
    };
    let p_pass = ChatShareGate::build(ShareGateInput {
        hits: std::slice::from_ref(&hit),
        share: AiShare::TEXT_IMAGES,
        max_hits: 5,
        max_images: 3,
        detail: &|_| None,
        thumb: &thumb,
        image_redactor: Some(&PassThroughRedactor),
        text_redactor: &BasicTextRedactor,
        ct: &CancelToken::none(),
    });
    assert!(p_pass.hits[0].image.is_none());
    assert!(p_pass.images_unavailable);

    let p_real = ChatShareGate::build(ShareGateInput {
        hits: &[hit],
        share: AiShare::TEXT_IMAGES,
        max_hits: 5,
        max_images: 3,
        detail: &|_| None,
        thumb: &thumb,
        image_redactor: Some(&RealImageRedactor { ok: true, fail: false }),
        text_redactor: &BasicTextRedactor,
        ct: &CancelToken::none(),
    });
    assert!(p_real.hits[0].image.is_some());
    assert!(!p_real.images_unavailable);
}

#[test]
fn redactor_failure_or_unchecked_image_sends_text_only() {
    let hit = sample_hit("s_1", CaptureKind::Screen);
    let thumb = |_: &str, _: i32, _: &CancelToken| {
        Some(ThumbData {
            id: "s_1".into(),
            mime: "image/webp".into(),
            width: 768,
            height: 432,
            bytes: vec![1, 2, 3],
        })
    };
    let p = ChatShareGate::build(ShareGateInput {
        hits: &[hit],
        share: AiShare::TEXT_IMAGES,
        max_hits: 5,
        max_images: 3,
        detail: &|_| None,
        thumb: &thumb,
        image_redactor: Some(&RealImageRedactor { ok: false, fail: true }),
        text_redactor: &BasicTextRedactor,
        ct: &CancelToken::none(),
    });
    assert!(p.hits[0].image.is_none());
    assert!(p.images_unavailable);
}

#[test]
fn caps_hits_and_images_and_tool_text_for_empty_and_declined() {
    let hits: Vec<SearchHit> = (0..10)
        .map(|i| sample_hit(&format!("s_{i}"), CaptureKind::Screen))
        .collect();
    let thumb = |id: &str, _: i32, _: &CancelToken| {
        Some(ThumbData {
            id: id.into(),
            mime: "image/webp".into(),
            width: 768,
            height: 432,
            bytes: vec![1, 2, 3],
        })
    };
    let plan = ChatShareGate::build(ShareGateInput {
        hits: &hits,
        share: AiShare::TEXT_IMAGES,
        max_hits: 4,
        max_images: 2,
        detail: &|_| {
            Some(CaptureDetail {
                id: "s".into(),
                ts: Utc::now(),
                kind: CaptureKind::Screen,
                app: "chrome.exe".into(),
                title: "t".into(),
                text: "stored body".into(),
                has_thumb: true,
                duration_s: None,
                site: String::new(),
                replay_id: None,
                replay_offset_ms: None,
            })
        },
        thumb: &thumb,
        image_redactor: Some(&RealImageRedactor { ok: true, fail: false }),
        text_redactor: &BasicTextRedactor,
        ct: &CancelToken::none(),
    });
    assert_eq!(4, plan.hits.len());
    assert_eq!(2, plan.hits.iter().filter(|h| h.image.is_some()).count());

    let empty = SharePlan {
        share: AiShare::METADATA.into(),
        hits: Vec::<SharedHit>::new(),
        images_unavailable: false,
    };
    assert!(ChatShareGate::tool_text(&empty, false).contains("No matching captures"));
    assert!(ChatShareGate::tool_text(&empty, true).contains("did not allow sharing"));
}

// ---- ChatServiceTests & ChatVerbTests ----

fn gemini_tool_call_sse(query: &str) -> String {
    format!(
        "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"functionCall\":{{\"name\":\"search_history\",\"args\":{{\"query\":\"{query}\"}}}}}}]}},\"finishReason\":\"STOP\"}}],\"usageMetadata\":{{\"promptTokenCount\":10,\"candidatesTokenCount\":5}}}}\n\n"
    )
}

fn gemini_answer_sse(text: &str) -> String {
    format!(
        "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{text}\"}}]}},\"finishReason\":\"STOP\"}}],\"usageMetadata\":{{\"promptTokenCount\":20,\"candidatesTokenCount\":10}}}}\n\n"
    )
}

#[test]
fn chat_service_first_share_asks_then_approves_and_remembers() {
    let mut t = TempStore::new();
    let store = Arc::new(t.store.take().unwrap());
    let id1 = store
        .add_audio_capture(&audio(Utc::now(), "project alpha launch plan", "teams.exe"))
        .unwrap();
    store.set_embedding(&id1, &vec_seeded(1), &vec_info()).unwrap();

    let responses = Arc::new(Mutex::new(vec![
        gemini_tool_call_sse("alpha"),
        gemini_answer_sse("Alpha launches soon [S1]."),
        gemini_tool_call_sse("alpha"),
        gemini_answer_sse("Still alpha [S1]."),
        gemini_tool_call_sse("alpha"),
    ]));
    let resp_cb = responses.clone();
    let transport = Arc::new(MockTransport::new(move |_, _, _| {
        let body = resp_cb.lock().unwrap().remove(0);
        Ok(MockTransport::ok_text(body, vec![]))
    }));
    let http = Arc::new(ProviderHttp::with_transport(
        transport,
        None,
        Some(Arc::new(|_, _| true)),
    ));

    let settings = Arc::new(Mutex::new({
        let mut s = Settings::default();
        s.ai.enabled = true;
        s.ai.provider = AiDefaults::GEMINI.to_string();
        s.ai.share = AiShare::TEXT.to_string();
        s
    }));
    let s_fn = settings.clone();
    let st_fn = store.clone();
    let svc = ChatService::new(ChatDeps {
        settings: Arc::new(move || s_fn.lock().unwrap().clone()),
        store: Arc::new(move || Some(st_fn.clone())),
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        api_key: Arc::new(|| Some("fake-key".to_string())),
        http,
        providers: ChatDeps::default_providers(),
        image_redactor: None,
        text_redactor: ChatDeps::default_text_redactor(),
        audit: ChatDeps::noop_audit(),
    });

    let mut evs1 = Vec::new();
    svc.send(
        None,
        "when does alpha launch?",
        &mut |e| evs1.push(e),
        &CancelToken::none(),
        None,
    );
    let approval = evs1
        .iter()
        .find(|e| e.event_type == "approval")
        .expect("first share must ask approval");
    let token = approval.token.clone().unwrap();
    let chat_id = approval.chat_id.clone().unwrap();

    let mut evs2 = Vec::new();
    svc.approve(
        &token,
        &HashSet::new(),
        &mut |e| evs2.push(e),
        &CancelToken::none(),
        None,
    );
    assert!(evs2.iter().any(|e| e.event_type == "done"));
    assert!(evs2.iter().any(|e| e.event_type == "sources"));

    // Second turn in same chat with same reviewed capture does NOT ask again.
    let mut evs3 = Vec::new();
    svc.send(
        Some(&chat_id),
        "remind me again",
        &mut |e| evs3.push(e),
        &CancelToken::none(),
        None,
    );
    assert!(!evs3.iter().any(|e| e.event_type == "approval"));
    assert!(evs3.iter().any(|e| e.event_type == "done"));

    // Forget approvals makes the next send ask for approval again.
    svc.forget_approvals();
    assert!(!svc.get(&chat_id).unwrap().approved);
    let mut evs4 = Vec::new();
    svc.send(
        Some(&chat_id),
        "ask once more",
        &mut |e| evs4.push(e),
        &CancelToken::none(),
        None,
    );
    assert!(
        evs4.iter().any(|e| e.event_type == "approval"),
        "expected approval prompt after forget_approvals"
    );
}

#[test]
fn chat_service_deny_and_failed_turn_rollback_and_history_off() {
    let mut t = TempStore::new();
    let store = Arc::new(t.store.take().unwrap());
    let id1 = store
        .add_audio_capture(&audio(Utc::now(), "secret topic", "teams.exe"))
        .unwrap();
    store.set_embedding(&id1, &vec_seeded(1), &vec_info()).unwrap();

    let responses = Arc::new(Mutex::new(vec![
        Ok(MockTransport::ok_text(gemini_tool_call_sse("secret"), vec![])),
        Ok(MockTransport::ok_text(gemini_answer_sse("No captures used."), vec![])),
        Ok(MockTransport::status_text(401, "{}", vec![])),
    ]));
    let resp_cb = responses.clone();
    let transport = Arc::new(MockTransport::new(move |_, _, _| {
        resp_cb.lock().unwrap().remove(0)
    }));
    let http = Arc::new(ProviderHttp::with_transport(
        transport,
        None,
        Some(Arc::new(|_, _| true)),
    ));

    let settings = Arc::new(Mutex::new({
        let mut s = Settings::default();
        s.ai.enabled = true;
        s.ai.chat_history = false;
        s
    }));
    let s_fn = settings.clone();
    let st_fn = store.clone();
    let svc = ChatService::new(ChatDeps {
        settings: Arc::new(move || s_fn.lock().unwrap().clone()),
        store: Arc::new(move || Some(st_fn.clone())),
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        api_key: Arc::new(|| Some("fake-key".to_string())),
        http,
        providers: ChatDeps::default_providers(),
        image_redactor: None,
        text_redactor: ChatDeps::default_text_redactor(),
        audit: ChatDeps::noop_audit(),
    });

    let mut evs = Vec::new();
    svc.send(None, "find secret", &mut |e| evs.push(e), &CancelToken::none(), None);
    let tok = evs
        .iter()
        .find(|e| e.event_type == "approval")
        .unwrap()
        .token
        .clone()
        .unwrap();
    let cid = evs[0].chat_id.clone().unwrap();

    let mut evs2 = Vec::new();
    svc.deny(&tok, &mut |e| evs2.push(e), &CancelToken::none(), None);
    assert!(evs2.iter().any(|e| e.event_type == "done"));
    // Stored in memory only (chatHistory=false).
    assert!(store.get_chat(&cid).unwrap().is_none());
    assert_eq!(2, svc.get(&cid).unwrap().messages.len());

    // Failed turn leaves chat messages unchanged.
    let mut evs3 = Vec::new();
    svc.send(Some(&cid), "second question", &mut |e| evs3.push(e), &CancelToken::none(), None);
    assert!(evs3.iter().any(|e| e.event_type == "error" && e.code.as_deref() == Some("auth")));
    assert_eq!(2, svc.get(&cid).unwrap().messages.len());
}

#[test]
fn chat_verb_gemini_end_to_end_over_fake_http_server() {
    let srv = FakeHttpServer::start();
    srv.push_response(
        200,
        "text/event-stream",
        vec![],
        gemini_answer_sse("Hello from local fake server."),
    );

    let mut t = TempStore::new();
    let store = Arc::new(t.store.take().unwrap());
    let _ = store.add_screen_capture(&screen(Utc::now(), "chrome.exe", "hi", true)).unwrap();

    let home = TempHome::new(Some("dpapi"));
    let settings = Arc::new(SettingsStore::load(home.path()));
    assert!(
        settings
            .try_update(&json!({
                "ai": {
                    "enabled": true,
                    "provider": "gemini",
                    "endpoint": srv.base_url()
                }
            }))
            .is_none()
    );
    let s_for_v = settings.clone();
    let vault = Arc::new(KeyVault::new(
        home.path(),
        move || s_for_v.current(),
        None,
        false,
    ));
    let _ = vault.try_load_silent();
    let ai_key = Arc::new(ApiKeyStore::new(home.path()));
    ai_key.set("AIza-fake-chat-key", &settings.current().ai).unwrap();
    let agent = Arc::new(FakeAgent::new());

    let st_clone = store.clone();
    let p = CommandProcessor::new(CommandDeps {
        settings: settings.clone(),
        store: Arc::new(move || Some(st_clone.clone())),
        keys: vault,
        agent: agent.clone(),
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: Some(home.path().to_path_buf()),
        password: None,
        ai_key: Some(ai_key),
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    });

    let mut ui = ClientSession::new(PipeKind::Ui, 1, "ui.exe");
    let reply = p.handle(
        &mut ui,
        r#"chat send {"text":"Say hi"}"#,
        &CancelToken::none(),
    );
    let producer = reply.stream.expect("streaming reply");
    let mut lines = Vec::new();
    producer(&mut |l| lines.push(l));

    assert!(lines.iter().any(|l| l.contains("Hello from local fake server.")));
    assert!(lines.iter().any(|l| l.contains("\"type\":\"done\"")));

    // Chat is listed and retrievable.
    let list_r = p.handle(&mut ui, "chat list", &CancelToken::none());
    let (ok, d, _) = Envelope::parse(&list_r.line).unwrap();
    assert!(ok);
    let items = d.unwrap()["items"].as_array().unwrap().clone();
    assert_eq!(1, items.len());
    let cid = items[0]["id"].as_str().unwrap().to_string();
    assert!(
        Envelope::parse(&p.handle(&mut ui, &format!("chat get {cid}"), &CancelToken::none()).line)
            .unwrap()
            .0
    );

    // Turning chat history off deletes saved chats.
    assert!(
        Envelope::parse(
            &p.handle(
                &mut ui,
                "settings set {\"ai\":{\"chatHistory\":false}}",
                &CancelToken::none()
            )
            .line
        )
        .unwrap()
        .0
    );
    assert!(store.chats(10).unwrap().is_empty());
}
