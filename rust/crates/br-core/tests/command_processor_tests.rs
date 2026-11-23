// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported `CommandProcessorTests.cs` + deferred command/password/describe/sweep tests from
//! Batch4Tests, Sweep2Tests, Sweep3ICoreTests, Sweep3JCoreTests, Sweep3KTests, Sweep3LCoreTests,
//! Sweep4UCoreTests, Sweep5UTests.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use br_contracts::{
    CaptureDetail, CaptureKind, Envelope, PipeNames, PurgeNotice, dotnet_base64,
};
use br_core::{
    AgentError, ApiKeyStore, CancelToken, ClientSession, CloudDescriber, CommandDeps,
    CommandProcessor, DescribeError, DescribeRedactor, EmbedError, KeyVault, OutgoingImage,
    PipeKind, SettingsStore, UnlockPassword,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};

use common::a3::{FakeAgent, FakeEmbedder, FakeHttpServer, FakeThumbs};
use common::{FAKE_WEBP, TempHome, TempStore, audio, screen, vec_info, vec_seeded};

#[allow(dead_code)]
struct CmdFixture {
    _t: TempStore,
    store: Arc<br_core::EncryptedStore>,
    home: TempHome,
    settings: Arc<SettingsStore>,
    vault: Arc<KeyVault>,
    agent: Arc<FakeAgent>,
    embedder: Arc<FakeEmbedder>,
    thumbs: Arc<FakeThumbs>,
    password: Arc<UnlockPassword>,
    ai_key: Arc<ApiKeyStore>,
    store_open: Arc<AtomicBool>,
    purge_all_calls: Arc<AtomicI32>,
    p: CommandProcessor,
}

impl CmdFixture {
    fn new() -> Self {
        Self::with_options("dpapi", true, None)
    }

    fn with_options(
        key_protection: &str,
        hello_available: bool,
        redactor: Option<Arc<dyn DescribeRedactor>>,
    ) -> Self {
        let mut t = TempStore::new();
        let store = Arc::new(t.store.take().unwrap());
        let home = TempHome::new(Some(key_protection));
        let settings = Arc::new(SettingsStore::load(home.path()));
        let s_for_vault = settings.clone();
        let vault = Arc::new(KeyVault::new(
            home.path(),
            move || s_for_vault.current(),
            None,
            false,
        ));
        vault.set_hello_available_for_test(hello_available);
        if key_protection == "dpapi" {
            let _ = vault.try_load_silent();
        }
        let agent = Arc::new(FakeAgent::new());
        let embedder = Arc::new(FakeEmbedder::new());
        let thumbs = Arc::new(FakeThumbs::new());
        let password = Arc::new(UnlockPassword::new(home.path()));
        let ai_key = Arc::new(ApiKeyStore::new(home.path()));
        let store_open = Arc::new(AtomicBool::new(true));
        let purge_all_calls = Arc::new(AtomicI32::new(0));

        let st_clone = store.clone();
        let open_clone = store_open.clone();
        let pa_clone = purge_all_calls.clone();
        let p = CommandProcessor::new(CommandDeps {
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
            thumbnailer: thumbs.clone(),
            purge_all: Arc::new(move |_| {
                pa_clone.fetch_add(1, Ordering::AcqRel);
                42
            }),
            purge_all_undeleted: None,
            blobs: None,
            home: Some(home.path().to_path_buf()),
            password: Some(password.clone()),
            ai_key: Some(ai_key.clone()),
            describer: Some(Arc::new(CloudDescriber::new())),
            redactor,
            chat_http: None,
            chat_providers: None,
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
            p,
        }
    }

    fn pub_sess() -> ClientSession {
        ClientSession::new(PipeKind::Public, 100, r"C:\tools\client.exe")
    }

    fn ui_sess() -> ClientSession {
        ClientSession::new(PipeKind::Ui, 101, r"C:\app\ui\BetterRecall.UI.exe")
    }

    fn mcp_sess() -> ClientSession {
        ClientSession::new(PipeKind::Mcp, 102, r"C:\app\betterrecall-mcp.exe")
    }

    fn set(&self, v: Value) {
        assert!(self.settings.try_update(&v).is_none());
    }

    fn call(&self, s: &mut ClientSession, line: &str) -> (bool, Option<Value>, Option<String>) {
        let r = self.p.handle(s, line, &CancelToken::none());
        assert!(!r.line.contains('\n'));
        Envelope::parse(&r.line).unwrap()
    }

    fn err(&self, s: &mut ClientSession, line: &str) -> Option<String> {
        self.call(s, line).2
    }
}

#[test]
fn ping_version_and_case_insensitive_verbs() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let (ok, d, _) = f.call(&mut pub_s, "PING");
    assert!(ok);
    assert!(d.unwrap()["pong"].as_bool().unwrap());

    let (ok, d, _) = f.call(&mut pub_s, "version");
    assert!(ok);
    assert_eq!(1, d.unwrap()["protocol"].as_i64().unwrap());
}

#[test]
fn unknown_verbs_per_pipe() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let mut mcp_s = CmdFixture::mcp_sess();
    assert_eq!(
        Some("Unknown command 'unlock'.".to_string()),
        f.err(&mut pub_s, "unlock")
    );
    assert_eq!(
        Some("Unknown command 'settings'.".to_string()),
        f.err(&mut pub_s, "settings get")
    );
    assert_eq!(
        Some("Unknown command 'purge'.".to_string()),
        f.err(&mut mcp_s, "purge last 60")
    );
    assert_eq!(
        Some("Unknown command 'pause'.".to_string()),
        f.err(&mut mcp_s, "pause")
    );
    assert_eq!(
        Some("Unknown command 'help'.".to_string()),
        f.err(&mut mcp_s, "help")
    );
}

#[test]
fn help_lists_only_enabled_verbs() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let mut ui_s = CmdFixture::ui_sess();
    let verbs = |d: Value| -> Vec<String> {
        d["verbs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    let pub_v = verbs(f.call(&mut pub_s, "help").1.unwrap());
    assert!(!pub_v.contains(&"pause".to_string()));
    assert!(!pub_v.contains(&"purge".to_string()));
    assert!(!pub_v.contains(&"thumb".to_string()));

    f.set(json!({"ipc": {"allowControl": true, "allowPurge": true, "allowThumbnails": true}}));
    let pub_v2 = verbs(f.call(&mut pub_s, "help").1.unwrap());
    assert!(pub_v2.contains(&"pause".to_string()));
    assert!(pub_v2.contains(&"purge".to_string()));
    assert!(pub_v2.contains(&"thumb".to_string()));

    let ui_v = verbs(f.call(&mut ui_s, "help").1.unwrap());
    assert!(ui_v.contains(&"unlock".to_string()));
    assert!(ui_v.contains(&"settings".to_string()));
    assert!(ui_v.contains(&"chat".to_string()));
    assert!(ui_v.contains(&"describe".to_string()));
    assert!(ui_v.contains(&"bench full".to_string()));
    assert!(ui_v.contains(&"bench last".to_string()));
}

#[test]
fn status_comes_from_agent() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let (ok, d, _) = f.call(&mut pub_s, "status");
    assert!(ok);
    assert!(d.unwrap()["capturing"].as_bool().unwrap());
}

#[test]
fn search_free_text_and_json_and_result_shape() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let id = f
        .store
        .add_audio_capture(&audio(Utc::now(), "Acme invoice quarterly report", "teams.exe"))
        .unwrap();
    f.store.set_embedding(&id, &vec_seeded(1), &vec_info()).unwrap();
    f.embedder
        .map
        .lock()
        .unwrap()
        .insert("Acme".to_string(), vec_seeded(1));

    let (ok, d, _) = f.call(&mut pub_s, "search Acme");
    assert!(ok);
    let data = d.unwrap();
    assert_eq!(1, data["count"].as_i64().unwrap());
    assert_eq!(id, data["results"][0]["id"].as_str().unwrap());

    let (ok2, d2, _) = f.call(
        &mut pub_s,
        "search {\"q\":\"Acme\",\"mode\":\"text\",\"limit\":5}",
    );
    assert!(ok2);
    assert_eq!(1, d2.unwrap()["count"].as_i64().unwrap());
}

#[test]
fn public_errors_and_long_query() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let cases = [
        ("search", CommandProcessor::USAGE_SEARCH),
        ("search {bad", CommandProcessor::ERR_JSON),
        ("search {}", "Invalid q."),
        ("search {\"q\":\"a\",\"limit\":0}", "Invalid limit."),
        ("search {\"q\":\"a\",\"mode\":\"nope\"}", "Invalid mode."),
        ("search {\"q\":\"a\",\"kind\":[\"nope\"]}", "Invalid kind."),
        ("get", CommandProcessor::USAGE_GET),
        ("subscribe bad", CommandProcessor::USAGE_SUBSCRIBE),
    ];
    for (line, expected) in cases {
        assert_eq!(Some(expected.to_string()), f.err(&mut pub_s, line), "line={line}");
    }
    let long_q = format!("search {}", "a".repeat(1025));
    assert_eq!(Some("Invalid q.".to_string()), f.err(&mut pub_s, &long_q));
}

#[test]
fn read_gate_closes_data_verbs_and_ipc_require_unlocked_false_opens_ipc_only() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let mut ui_s = CmdFixture::ui_sess();
    f.agent.unlocked.store(false, Ordering::Release);

    assert_eq!(Some(CommandProcessor::ERR_LOCKED.to_string()), f.err(&mut pub_s, "search hello"));
    assert_eq!(Some(CommandProcessor::ERR_LOCKED.to_string()), f.err(&mut pub_s, "get s_1"));
    assert_eq!(Some(CommandProcessor::ERR_LOCKED.to_string()), f.err(&mut ui_s, "search hello"));

    f.set(json!({"ipc": {"requireUnlocked": false}}));
    assert!(f.call(&mut pub_s, "search hello").0);
    assert_eq!(Some(CommandProcessor::ERR_LOCKED.to_string()), f.err(&mut ui_s, "search hello"));
}

#[test]
fn rate_limit_per_pipe_survives_reconnects() {
    let f = CmdFixture::new();
    f.set(json!({"ipc": {"maxSearchPerMinute": 2}}));
    let mut s1 = CmdFixture::pub_sess();
    let mut s2 = CmdFixture::pub_sess();
    assert!(f.call(&mut s1, "search a").0);
    assert!(f.call(&mut s2, "search b").0);
    assert_eq!(
        Some(CommandProcessor::ERR_RATE.to_string()),
        f.err(&mut s2, "search c")
    );
}

#[test]
fn embedder_failure_falls_back_to_text_in_hybrid_and_degrades_in_vector() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let id = f
        .store
        .add_audio_capture(&audio(Utc::now(), "unique_keyword_xyz", "teams.exe"))
        .unwrap();
    f.store.set_embedding(&id, &vec_seeded(1), &vec_info()).unwrap();
    f.embedder.fail.store(true, Ordering::Release);

    let (ok, d, _) = f.call(&mut pub_s, "search unique_keyword_xyz");
    assert!(ok);
    let data = d.unwrap();
    assert_eq!(1, data["count"].as_i64().unwrap());
    assert_eq!(Some("vector-unavailable"), data["degraded"].as_str());

    let (ok_v, d_v, _) = f.call(&mut pub_s, "search {\"q\":\"unique_keyword_xyz\",\"mode\":\"vector\"}");
    assert!(ok_v);
    let dv = d_v.unwrap();
    assert_eq!(0, dv["count"].as_i64().unwrap());
    assert_eq!(Some("vector-unavailable"), dv["degraded"].as_str());
}

#[test]
fn get_returns_text_and_fit_detail_cuts_to_budget() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let id = f
        .store
        .add_audio_capture(&audio(Utc::now(), "full transcript body", "teams.exe"))
        .unwrap();
    let (ok, d, _) = f.call(&mut pub_s, &format!("get {id}"));
    assert!(ok);
    assert_eq!("full transcript body", d.unwrap()["text"].as_str().unwrap());

    let big = CaptureDetail {
        id: "s_1".to_string(),
        ts: Utc::now(),
        kind: CaptureKind::Audio,
        app: "teams.exe".to_string(),
        title: "t".to_string(),
        text: "x".repeat(50_000),
        has_thumb: false,
        duration_s: None,
        site: String::new(),
        replay_id: None,
        replay_offset_ms: None,
    };
    let fitted = CommandProcessor::fit_detail(&big, 4096);
    assert!(fitted.len() <= 4096);
}

#[test]
fn thumb_needs_allow_thumbnails_on_public_and_validates_width() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let mut ui_s = CmdFixture::ui_sess();
    assert_eq!(
        Some(CommandProcessor::ERR_THUMBS_OFF.to_string()),
        f.err(&mut pub_s, "thumb s_1")
    );
    f.set(json!({"ipc": {"allowThumbnails": true}}));
    assert_eq!(Some("Invalid width.".to_string()), f.err(&mut pub_s, "thumb s_1 32"));
    assert_eq!(Some("Invalid width.".to_string()), f.err(&mut pub_s, "thumb s_1 2000"));
    assert!(f.call(&mut pub_s, "thumb s_1 320").0);
    assert_eq!(
        Some(CommandProcessor::ERR_NOT_FOUND.to_string()),
        f.err(&mut ui_s, "thumb missing")
    );

    // Sweep3L: UI thumbs are not audited, public thumbs are.
    let before = f.store.read_audit(50).unwrap().len();
    assert!(f.call(&mut ui_s, "thumb s_1 320").0);
    assert_eq!(before, f.store.read_audit(50).unwrap().len());
    assert!(f.call(&mut pub_s, "thumb s_1 320").0);
    assert!(f.store.read_audit(50).unwrap().iter().any(|a| a.verb == "thumb" && a.pipe == "ipc"));
}

#[test]
fn pause_resume_need_allow_control_and_s10_public_resume_is_ipc() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    assert_eq!(Some(CommandProcessor::ERR_CONTROL_OFF.to_string()), f.err(&mut pub_s, "pause"));
    assert_eq!(Some(CommandProcessor::ERR_CONTROL_OFF.to_string()), f.err(&mut pub_s, "resume"));

    f.set(json!({"ipc": {"allowControl": true}}));
    assert!(f.call(&mut pub_s, "pause 120").0);
    assert!(f.call(&mut pub_s, "resume").0);
    let calls = f.agent.calls.lock().unwrap().clone();
    assert!(calls.contains(&"pause 120 ipc".to_string()));
    assert!(calls.contains(&"resume ipc".to_string()));
}

#[test]
fn purge_two_step_tokens_and_purge_all_recent_unlock() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    let mut ui_s = CmdFixture::ui_sess();
    f.set(json!({"ipc": {"allowControl": true, "allowPurge": true}}));

    let id = f
        .store
        .add_audio_capture(&audio(Utc::now(), "to delete", "teams.exe"))
        .unwrap();
    let (ok, d, _) = f.call(&mut pub_s, &format!("purge id {id}"));
    assert!(ok);
    let tok = d.unwrap()["confirm"].as_str().unwrap().to_string();

    // Token belongs to pub_s, not ui_s.
    assert_eq!(
        Some(CommandProcessor::ERR_TOKEN.to_string()),
        f.err(&mut ui_s, &format!("purge confirm {tok}"))
    );
    let (ok2, d2, _) = f.call(&mut pub_s, &format!("purge confirm {tok}"));
    assert!(ok2);
    assert_eq!(1, d2.unwrap()["purged"].as_i64().unwrap());

    // purge all needs recent unlock.
    assert_eq!(
        Some(CommandProcessor::ERR_UNLOCK_REQUIRED.to_string()),
        f.err(&mut ui_s, "purge all")
    );
    assert!(f.call(&mut ui_s, "unlock").0);
    let (ok3, d3, _) = f.call(&mut ui_s, "purge all");
    assert!(ok3);
    let all_tok = d3.unwrap()["confirm"].as_str().unwrap().to_string();
    let (ok4, d4, _) = f.call(&mut ui_s, &format!("purge confirm {all_tok}"));
    assert!(ok4);
    assert_eq!(42, d4.unwrap()["purged"].as_i64().unwrap());
    assert_eq!(1, f.purge_all_calls.load(Ordering::Acquire));
}

#[test]
fn public_pipe_may_not_take_a_reserved_name() {
    let f = CmdFixture::new();
    for name in ["BetterRecall.ui", "BetterRecall.mcp", "BetterRecall.worker"] {
        assert!(PipeNames::reserved(name));
        assert_eq!(
            Some("Invalid ipc.pipeName.".to_string()),
            f.settings.try_update(&json!({"ipc": {"pipeName": name}})),
            "name={name}"
        );
    }
}

#[test]
fn unlock_lock_consent_panic_and_bench_verbs() {
    let f = CmdFixture::new();
    let mut ui_s = CmdFixture::ui_sess();
    assert!(f.call(&mut ui_s, "lock").0);
    assert!(!f.agent.unlocked.load(Ordering::Acquire));
    assert!(f.call(&mut ui_s, "unlock").0);
    assert!(f.agent.unlocked.load(Ordering::Acquire));
    assert!(f.call(&mut ui_s, "consent").0);
    assert!(f.call(&mut ui_s, "panic").0);
    let (ok, d, _) = f.call(&mut ui_s, "bench");
    assert!(ok);
    assert_eq!(Some("CPU"), d.unwrap()["chosen"].as_str());

    let calls = f.agent.calls.lock().unwrap().clone();
    assert!(calls.contains(&"lock".to_string()));
    assert!(calls.contains(&"unlock".to_string()));
    assert!(calls.contains(&"consent".to_string()));
    assert!(calls.contains(&"panic".to_string()));
}

#[test]
fn timeline_keyframe_audit_and_replay_verbs() {
    let f = CmdFixture::new();
    let mut ui_s = CmdFixture::ui_sess();
    let now = Utc::now();
    let mut sc = screen(now, "chrome.exe", "page", true);
    sc.image_mime = "image/avif".to_string();
    let id = f.store.add_screen_capture(&sc).unwrap();
    f.store.set_embedding(&id, &vec_seeded(2), &vec_info()).unwrap();
    let (ok, d, _) = f.call(&mut ui_s, &format!("keyframe {id}"));
    assert!(ok);
    assert_eq!("image/webp", d.unwrap()["mime"].as_str().unwrap());

    let from = (now - ChronoDuration::minutes(5)).to_rfc3339();
    let to = (now + ChronoDuration::minutes(5)).to_rfc3339();
    let (ok_tl, d_tl, _) = f.call(&mut ui_s, &format!("timeline {from} {to}"));
    assert!(ok_tl);
    assert!(
        d_tl.unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"].as_str() == Some(id.as_str()))
    );
    let (ok_au, d_au, _) = f.call(&mut ui_s, "audit 10");
    assert!(ok_au);
    assert!(d_au.unwrap()["items"].is_array());

    let (ok_rs, d_rs, _) = f.call(&mut ui_s, "replay save");
    assert!(ok_rs);
    assert_eq!(Some("r1"), d_rs.unwrap()["id"].as_str());
    let (ok_rl, d_rl, _) = f.call(&mut ui_s, "replay list");
    assert!(ok_rl);
    assert_eq!(1, d_rl.unwrap()["items"].as_array().unwrap().len());
    let export_path = f.home.path().join("clip.mp4");
    assert!(
        f.call(&mut ui_s, &format!("replay export r1 {}", export_path.display()))
            .0
    );
    assert!(
        f.agent
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == &format!("export r1 {}", export_path.display()))
    );
    assert!(f.call(&mut ui_s, "replay delete r1").0);
    assert!(f.agent.replays.lock().unwrap().is_empty());
}

#[test]
fn history_and_facets_and_export_verbs() {
    let f = CmdFixture::new();
    let mut ui_s = CmdFixture::ui_sess();
    assert!(f.call(&mut ui_s, "history add hello world").0);
    let (ok, d, _) = f.call(&mut ui_s, "history");
    assert!(ok);
    assert_eq!(1, d.unwrap()["items"].as_array().unwrap().len());
    assert!(f.call(&mut ui_s, "history delete hello world").0);
    let (ok_after_del, d_after_del, _) = f.call(&mut ui_s, "history");
    assert!(ok_after_del);
    assert!(d_after_del.unwrap()["items"].as_array().unwrap().is_empty());

    assert!(f.call(&mut ui_s, "history add second query").0);
    assert!(f.call(&mut ui_s, "history clear").0);
    let (ok_after_clr, d_after_clr, _) = f.call(&mut ui_s, "history");
    assert!(ok_after_clr);
    assert!(d_after_clr.unwrap()["items"].as_array().unwrap().is_empty());

    let now = Utc::now();
    let id = f
        .store
        .add_screen_capture(&screen(now, "chrome.exe", "t", true))
        .unwrap();
    f.store.set_embedding(&id, &vec_seeded(3), &vec_info()).unwrap();

    let (ok_f, d_f, _) = f.call(&mut ui_s, "facets");
    assert!(ok_f);
    let fd = d_f.unwrap();
    assert!(
        fd["apps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"].as_str() == Some("chrome.exe"))
    );

    let req = json!({
        "from": (now - ChronoDuration::minutes(5)).to_rfc3339(),
        "to": (now + ChronoDuration::minutes(5)).to_rfc3339(),
        "path": f.home.path().join("out.zip").to_string_lossy(),
        "dryRun": true
    });
    let (ok, d, _) = f.call(&mut ui_s, &format!("export {req}"));
    assert!(ok);
    assert_eq!(1, d.unwrap()["count"].as_i64().unwrap());
}

#[test]
fn password_method_and_unlock_flow() {
    let f = CmdFixture::new();
    let mut ui_s = CmdFixture::ui_sess();
    // Cannot switch to password unlockMethod without a password set.
    assert_eq!(
        Some(CommandProcessor::ERR_NO_PASSWORD.to_string()),
        f.err(&mut ui_s, "settings set {\"security\":{\"unlockMethod\":\"password\"}}")
    );
    // Set password while unlocked.
    assert!(f.call(&mut ui_s, "password set {\"new\":\"secret123\"}").0);
    assert!(f.call(&mut ui_s, "settings set {\"security\":{\"unlockMethod\":\"password\"}}").0);
    assert!(f.call(&mut ui_s, "lock").0);

    // Wrong password rejected.
    assert_eq!(
        Some(CommandProcessor::ERR_WRONG_PASSWORD.to_string()),
        f.err(&mut ui_s, "unlock wrongpass")
    );
    // Correct password opens gate without Hello.
    assert!(f.call(&mut ui_s, "unlock secret123").0);
    assert!(f.agent.unlocked.load(Ordering::Acquire));

    // Cannot clear password while unlockMethod is password.
    assert_eq!(
        Some(CommandProcessor::ERR_PASSWORD_IN_USE.to_string()),
        f.err(&mut ui_s, "password clear {\"current\":\"secret123\"}")
    );
}

struct StubRedactor;
impl DescribeRedactor for StubRedactor {
    fn redact(
        &self,
        _capture_id: &str,
        mut image: OutgoingImage,
        _ct: &CancelToken,
    ) -> Result<OutgoingImage, DescribeError> {
        image.bytes = b"REDACTED-IMAGE".to_vec();
        image.redacted = true;
        Ok(image)
    }
    fn redacts_images(&self) -> bool {
        true
    }
}

#[test]
fn describe_preview_send_redactor_and_fake_http_server() {
    let srv = FakeHttpServer::start();
    srv.set_response_body(r#"{"candidates":[{"content":{"parts":[{"text":"A browser window."}]}}]}"#);

    let f = CmdFixture::with_options("dpapi", true, Some(Arc::new(StubRedactor)));
    let mut ui_s = CmdFixture::ui_sess();
    let id = f
        .store
        .add_screen_capture(&screen(Utc::now(), "chrome.exe", "title", true))
        .unwrap();
    f.store.set_embedding(&id, &vec_seeded(4), &vec_info()).unwrap();

    // Off by default.
    assert_eq!(
        Some(CommandProcessor::ERR_AI_OFF.to_string()),
        f.err(&mut ui_s, &format!("describe preview {id}"))
    );

    f.set(json!({"ai": {"enabled": true, "provider": "gemini", "endpoint": srv.base_url()}}));
    assert!(f.call(&mut ui_s, "ai setkey FAKE-GEMINI-KEY").0);

    let (ok, d, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let prev = d.unwrap();
    assert!(prev["redacted"].as_bool().unwrap());
    let tok = prev["token"].as_str().unwrap().to_string();

    let (ok2, d2, _) = f.call(&mut ui_s, &format!("describe send {tok}"));
    assert!(ok2);
    assert_eq!("A browser window.", d2.unwrap()["text"].as_str().unwrap());

    let reqs = srv.requests();
    assert_eq!(1, reqs.len());
    assert_eq!(Some("FAKE-GEMINI-KEY"), reqs[0].header("x-goog-api-key"));
    assert!(reqs[0].path.contains(":generateContent"));
    assert!(
        reqs[0].body.contains(&dotnet_base64::encode(b"REDACTED-IMAGE")),
        "describe send did not send redacted image bytes"
    );
    assert!(
        !reqs[0].body.contains(&dotnet_base64::encode(FAKE_WEBP)),
        "describe send leaked unredacted original image bytes"
    );
}

/// Regression D13.
#[test]
fn purge_app_name_and_site_host() {
    assert_eq!("claude", CommandProcessor::purge_app_name(r"C:\Apps\claude.exe"));
    assert_eq!("CHROME", CommandProcessor::purge_app_name("CHROME.EXE"));
    // X1: multi-byte names must not panic on the ".exe" check.
    assert_eq!("日日", CommandProcessor::purge_app_name("日日"));
    assert_eq!("日本語", CommandProcessor::purge_app_name(r"D:\日本語.exe"));
    assert_eq!("é", CommandProcessor::purge_app_name("é"));
    assert_eq!(
        "www.google.com",
        CommandProcessor::purge_site_host("https://www.google.com/path?a=1")
    );
    assert_eq!("github.com", CommandProcessor::purge_site_host("github.com/org/repo"));
    assert_eq!("example.org", CommandProcessor::purge_site_host("example.org:8080"));
}

#[test]
fn guarded_settings_refused_while_locked() {
    let f = CmdFixture::new();
    let mut ui_s = CmdFixture::ui_sess();
    f.agent.unlocked.store(false, Ordering::Release);

    assert_eq!(
        Some(CommandProcessor::ERR_LOCKED.to_string()),
        f.err(&mut ui_s, "settings set {\"retention\":{\"days\":7}}")
    );
    assert_eq!(
        Some(CommandProcessor::ERR_LOCKED.to_string()),
        f.err(&mut ui_s, "settings set {\"inference\":{\"modelDir\":\"C:\\\\Models\"}}")
    );
}

/// Regression D12.
#[test]
fn public_status_hides_store_stats_while_locked() {
    let f = CmdFixture::new();
    let mut pub_s = CmdFixture::pub_sess();
    f.agent.unlocked.store(false, Ordering::Release);
    f.agent.status.lock().unwrap().store.rows = 999;
    f.agent.status.lock().unwrap().queue.pending = 12;
    let (ok, d, _) = f.call(&mut pub_s, "status");
    assert!(ok);
    let data = d.unwrap();
    assert_eq!(0, data["store"]["rows"].as_i64().unwrap());
    assert_eq!(0, data["queue"]["pending"].as_i64().unwrap());
}

/// Regression S5.
#[test]
fn replay_purge_refusal_becomes_err_purged_during() {
    let f = CmdFixture::new();
    let mut ui_s = CmdFixture::ui_sess();
    *f.agent.on_save_replay.lock().unwrap() = Some(Box::new(|_| {
        Err(AgentError::InvalidOperation(
            "Purge all is running.".to_string(),
        ))
    }));
    assert_eq!(
        Some(CommandProcessor::ERR_PURGED_DURING.to_string()),
        f.err(&mut ui_s, "replay save")
    );
}

/// Regression B10.
#[test]
fn removed_event_caps_ids_at_max_removed_ids() {
    let notice = PurgeNotice {
        reason: "purge".to_string(),
        ids: (0..600).map(|i| format!("s_{i}")).collect(),
        from: None,
        to: None,
        newest: None,
        scope: None,
    };
    let ev = CommandProcessor::removed_event_for(&notice);
    assert!(ev.truncated);
    assert!(ev.ids.is_empty());
}

/// Regression Sweep3K: cold worker falls back to text results within `COLD_EMBED_WAIT` and flags `degraded: "vector"`.
#[test]
fn cold_worker_gives_text_results_quickly_and_flags_them() {
    let f = CmdFixture::new();
    f.p.set_cold_embed_wait_for_test(Duration::from_millis(200));
    let mut pub_s = CmdFixture::pub_sess();
    let _ = f
        .store
        .add_audio_capture(&audio(Utc::now(), "cold_worker_token", "teams.exe"))
        .unwrap();
    f.embedder.warm.store(false, Ordering::Release);
    *f.embedder.on_embed.lock().unwrap() = Some(Box::new(|_, ct| {
        ct.wait_timeout(Duration::from_secs(10));
        Err(EmbedError::Cancelled)
    }));
    let started = Instant::now();
    let (ok, d, _) = f.call(&mut pub_s, "search cold_worker_token");
    let elapsed = started.elapsed();
    assert!(ok);
    let data = d.unwrap();
    assert_eq!(1, data["count"].as_i64().unwrap());
    assert_eq!(Some(true), data["results"][0]["textMatch"].as_bool());
    assert_eq!(Some("vector"), data["degraded"].as_str());
    assert!(
        elapsed < Duration::from_secs(2),
        "cold worker search took {elapsed:?} (expected bounded by injected 200 ms wait)"
    );
}

/// Regressions B3 and B4 (sweep 5u): missing Hello refuses passphrase protection; inline `passphrase verify`
/// is a no-op on DPAPI and validates the passphrase on a passphrase-wrapped key.
#[test]
fn hello_missing_refuses_passphrase_protection_and_inline_verify() {
    let f = CmdFixture::with_options("dpapi", false, None);
    let mut ui_s = CmdFixture::ui_sess();
    assert_eq!(
        Some(CommandProcessor::ERR_PASSPHRASE_NEEDS_HELLO.to_string()),
        f.err(&mut ui_s, "settings set {\"security\":{\"keyProtection\":\"passphrase\"}}")
    );

    // B4: passphrase verify succeeds on dpapi (no-op).
    assert!(f.call(&mut ui_s, "passphrase verify {\"passphrase\":\"anything\"}").0);

    // B4: passphrase verify validates against a real passphrase-wrapped key.
    let fp = CmdFixture::with_options("passphrase", true, None);
    assert!(fp.vault.unlock_with_passphrase("correct-passphrase-123"));
    let mut ui_p = CmdFixture::ui_sess();
    assert!(
        !fp.call(&mut ui_p, "passphrase verify {\"passphrase\":\"wrong-passphrase\"}")
            .0
    );
    assert!(
        fp.call(
            &mut ui_p,
            "passphrase verify {\"passphrase\":\"correct-passphrase-123\"}"
        )
        .0
    );
}
