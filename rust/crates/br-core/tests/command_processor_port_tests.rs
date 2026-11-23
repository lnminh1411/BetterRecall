// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sweep 6 G3 ported tests for `CommandProcessor` verbs, search, purge, settings,
//! history, facets, export, timeline, replay, audit, and unreadable-settings recovery.

mod common;

use std::fs::File;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{
    CaptureDetail, CaptureKind, CoreHostOptions, Envelope, ExportRequest, NewReplayFrame,
    PurgeNotice, ThumbData,
};
use br_core::{
    AgentError, CancelSource, CancelToken, CommandProcessor, CoreHost, EmbedError, PipeKind,
    SettingsStore, Thumbnailer,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use zip::ZipArchive;

use common::a3::{FakeAgent, FakeEmbedder};
use common::ports::{PortFixture, PortFixtureOpts};
use common::{FAKE_WEBP, TempHome, audio, screen, vec_info, vec_seeded};

#[test]
fn ping_version_trim_semver_and_unknown_verbs_per_pipe() {
    let f = PortFixture::new();
    let mut pub_s = PortFixture::pub_sess();
    let mut ui_s = PortFixture::ui();
    let mut mcp_s = PortFixture::mcp();

    let (ok, data, _) = f.call(&mut pub_s, "  ping \r");
    assert!(ok);
    assert_eq!(true, data.unwrap()["pong"]);

    let (ok, data, _) = f.call(&mut pub_s, "VERSION");
    assert!(ok);
    let data = data.unwrap();
    assert_eq!(1, data["protocol"]);
    let app = data["app"].as_str().unwrap();
    let parts: Vec<&str> = app.split('.').collect();
    assert!(parts.len() >= 3, "app version should be semver: {app}");
    assert!(parts[0].parse::<u32>().is_ok(), "major: {app}");
    assert!(parts[1].parse::<u32>().is_ok(), "minor: {app}");

    let (ok, _, err) = f.call(&mut pub_s, "   ");
    assert!(!ok);
    assert_eq!("Unknown command ''.", err.as_deref().unwrap());

    let (ok, _, err) = f.call(&mut pub_s, "nope");
    assert!(!ok);
    assert!(err.as_deref().unwrap().contains("Unknown command"));

    // Public pipe does not expose UI-only verbs.
    for v in [
        "unlock",
        "lock",
        "consent",
        "settings get",
        "timeline",
        "keyframe x",
        "replay list",
        "panic",
        "audit",
        "bench",
        "facets",
        "history",
        "password",
        "passphrase verify {}",
        "ai",
        "describe preview x",
        "export {}",
        "chat list",
        "models list",
        "update status",
    ] {
        let (ok, _, err) = f.call(&mut pub_s, v);
        assert!(!ok, "public verb should fail: {v}");
        assert!(
            err.as_deref().unwrap().contains("Unknown command"),
            "public verb {v} got {err:?}"
        );
    }

    // MCP pipe only exposes ping, version, status, search, get (not help/thumb/pause/purge/UI).
    let (ok, _, _) = f.call(&mut mcp_s, "ping");
    assert!(ok);
    let (ok, _, _) = f.call(&mut mcp_s, "version");
    assert!(ok);
    let (ok, _, _) = f.call(&mut mcp_s, "status");
    assert!(ok);
    for v in ["help", "thumb x", "pause", "purge last 60", "settings get", "subscribe"] {
        let (ok, _, err) = f.call(&mut mcp_s, v);
        assert!(!ok, "mcp verb should fail: {v}");
        let msg = err.unwrap();
        if v == "subscribe" {
            assert_eq!("Unknown command 'subscribe'.", msg);
        } else {
            assert!(
                msg.contains("Unknown command"),
                "mcp verb {v} got {msg:?}"
            );
        }
    }

    // Help lists public verbs on public pipe and includes UI verbs on UI pipe.
    let (ok, d_pub, _) = f.call(&mut pub_s, "help");
    assert!(ok);
    let pub_verbs: Vec<String> = d_pub.unwrap()["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(pub_verbs.contains(&"search".to_string()));
    assert!(!pub_verbs.contains(&"settings".to_string()));

    let (ok, d_ui, _) = f.call(&mut ui_s, "help");
    assert!(ok);
    let ui_verbs: Vec<String> = d_ui.unwrap()["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(ui_verbs.contains(&"settings".to_string()));
    assert!(ui_verbs.contains(&"chat".to_string()));
}

#[test]
fn search_result_shape_weak_threshold_ocr_filters_and_audit_hash() {
    let f = PortFixture::new();
    let base = Utc::now() - ChronoDuration::minutes(10);

    // 1) Screen capture with OCR text and vector.
    let v1 = vec_seeded(11);
    let mut s1 = screen(base, "chrome.exe", "Dashboard", true);
    s1.site = "docs.example.com".to_string();
    s1.text = Some("Quarterly budget zebra-token-99 summary".to_string());
    let id1 = f.store.add_screen_capture(&s1).unwrap();
    f.store.set_embedding(&id1, &v1, &vec_info()).unwrap();

    // 2) Audio capture.
    let v2 = vec_seeded(22);
    let id2 = f
        .store
        .add_audio_capture(&audio(base + ChronoDuration::minutes(1), "Standup notes", "teams.exe"))
        .unwrap();
    f.store.set_embedding(&id2, &v2, &vec_info()).unwrap();

    f.embedder
        .map
        .lock()
        .unwrap()
        .insert("zebra-token-99".to_string(), v1.clone());
    f.embedder
        .map
        .lock()
        .unwrap()
        .insert("unrelated query".to_string(), vec_seeded(999));

    let mut pub_s = PortFixture::pub_sess();
    let mut ui_s = PortFixture::ui();

    // OCR text match returns snippet containing the OCR text and textMatch=true.
    let (ok, data, _) = f.call(&mut pub_s, "search zebra-token-99");
    assert!(ok);
    let data = data.unwrap();
    assert!(data["count"].as_i64().unwrap() >= 1);
    let first = &data["results"][0];
    assert_eq!(id1, first["id"]);
    assert_eq!(true, first["textMatch"]);
    assert_eq!(false, first["weak"]);
    assert!(first["snippet"].as_str().unwrap().contains("zebra-token-99"));
    assert_eq!("docs.example.com", first["site"]);

    // JSON search with kind & app filters.
    let (ok, data, _) = f.call(
        &mut pub_s,
        r#"search {"q":"Standup","kind":["audio"],"app":"teams.exe","mode":"text"}"#,
    );
    assert!(ok);
    let results = data.unwrap()["results"].as_array().unwrap().clone();
    assert_eq!(1, results.len());
    assert_eq!(id2, results[0]["id"]);
    assert_eq!("audio", results[0]["kind"]);

    // Pure vector search with orthogonal query marks weak results (cosine below threshold).
    let (ok, data, _) = f.call(
        &mut pub_s,
        r#"search {"q":"unrelated query","mode":"vector"}"#,
    );
    assert!(ok);
    let results = data.unwrap()["results"].as_array().unwrap().clone();
    assert!(!results.is_empty());
    assert_eq!(true, results[0]["weak"]);

    // Audit row records keyed query hash (not plaintext) and result count.
    let expected_hash = f.p.query_hash("zebra-token-99");
    assert_ne!(expected_hash, CommandProcessor::sha256_hex("zebra-token-99"));
    let (ok, audit, _) = f.call(&mut ui_s, "audit 20");
    assert!(ok);
    let items = audit.unwrap()["items"].as_array().unwrap().clone();
    assert!(
        items
            .iter()
            .any(|i| i["verb"] == "search" && i["querySha256"] == expected_hash),
        "expected keyed query hash in audit: {items:?}"
    );
    let serialized = serde_json::to_string(&items).unwrap();
    assert!(!serialized.contains("zebra-token-99"));

    // Public search respects maxResults cap (set to 1; defaultResults must be <= maxResults).
    f.set_settings(json!({"ipc": {"maxResults": 1, "defaultResults": 1}}));
    let (ok, data, _) = f.call(&mut pub_s, r#"search {"q":"unrelated query","limit":10}"#);
    assert!(ok);
    assert!(data.unwrap()["results"].as_array().unwrap().len() <= 1);
}

#[test]
fn public_errors_all_19_cases() {
    let f = PortFixture::new();
    let mut s = PortFixture::pub_sess();

    // 1. empty
    let (ok, _, e) = f.call(&mut s, "");
    assert!(!ok);
    assert_eq!("Unknown command ''.", e.unwrap());
    // 2. unknown
    let (ok, _, e) = f.call(&mut s, "bogus");
    assert!(!ok);
    assert!(e.unwrap().contains("Unknown command"));
    // 3. search no args
    let (ok, _, e) = f.call(&mut s, "search");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_SEARCH, e.unwrap());
    // 4. search invalid JSON
    let (ok, _, e) = f.call(&mut s, "search {bad");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_JSON, e.unwrap());
    // 5. search empty q
    let (ok, _, e) = f.call(&mut s, r#"search {"q":"  "}"#);
    assert!(!ok);
    assert_eq!("Invalid q.", e.unwrap());
    // 6. search bad mode
    let (ok, _, e) = f.call(&mut s, r#"search {"q":"a","mode":"magic"}"#);
    assert!(!ok);
    assert_eq!("Invalid mode.", e.unwrap());
    // 7. search bad kind
    let (ok, _, e) = f.call(&mut s, r#"search {"q":"a","kind":["video"]}"#);
    assert!(!ok);
    assert_eq!("Invalid kind.", e.unwrap());
    // 8. search bad limit
    let (ok, _, e) = f.call(&mut s, r#"search {"q":"a","limit":0}"#);
    assert!(!ok);
    assert_eq!("Invalid limit.", e.unwrap());
    // 9. search bad from/to range
    let (ok, _, e) = f.call(
        &mut s,
        r#"search {"q":"a","from":"2026-11-10T12:00:00Z","to":"2026-11-10T11:00:00Z"}"#,
    );
    assert!(!ok);
    assert_eq!("Invalid from.", e.unwrap());
    let (ok, _, e) = f.call(&mut s, r#"search {"q":"a","to":12}"#);
    assert!(!ok);
    assert_eq!("Invalid to.", e.unwrap());
    // 10. get missing arg
    let (ok, _, e) = f.call(&mut s, "get");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_GET, e.unwrap());
    // 11. get not found
    let (ok, _, e) = f.call(&mut s, "get s_nope");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_FOUND, e.unwrap());
    // 12. thumb off on public
    let (ok, _, e) = f.call(&mut s, "thumb s_1");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_THUMBS_OFF, e.unwrap());
    // 13. pause off on public
    let (ok, _, e) = f.call(&mut s, "pause 60");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_CONTROL_OFF, e.unwrap());
    // 14. resume off on public
    let (ok, _, e) = f.call(&mut s, "resume");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_CONTROL_OFF, e.unwrap());
    // 15. purge off on public: allowControl checked first, then allowPurge.
    let (ok, _, e) = f.call(&mut s, "purge last 60");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_CONTROL_OFF, e.unwrap());
    f.set_settings(json!({"ipc": {"allowControl": true, "allowPurge": false}}));
    let (ok, _, e) = f.call(&mut s, "purge last 60");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PURGE_OFF, e.unwrap());
    f.set_settings(json!({"ipc": {"allowControl": false}}));
    // 16. subscribe bad arg
    let (ok, _, e) = f.call(&mut s, "subscribe bogus");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_SUBSCRIBE, e.unwrap());
    // 17. extra malformed search/get cases from Public_errors (19 rows) + locked subscribe captures
    for (cmd, want) in [
        (r#"search {"q":5}"#, "Invalid q."),
        (r#"search {"q":"a","limit":"3"}"#, "Invalid limit."),
        (r#"search {"q":"a","from":"nope"}"#, "Invalid from."),
        (r#"search {"q":"a","app":""}"#, "Invalid app."),
        (r#"search {"q":"a","kind":"screen"}"#, "Invalid kind."),
        ("get a b", CommandProcessor::USAGE_GET),
    ] {
        let (ok, _, e) = f.call(&mut s, cmd);
        assert!(!ok, "expected failure for {cmd}");
        assert_eq!(want, e.unwrap(), "cmd={cmd}");
    }
    // 18. store locked (blocks search and subscribe captures)
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, e) = f.call(&mut s, "search hello");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, e.unwrap());
    let (ok, _, e) = f.call(&mut s, "subscribe captures");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, e.unwrap());
    f.agent.unlocked.store(true, Ordering::Release);
    // 19. store not open
    f.store_open.store(false, Ordering::Release);
    let (ok, _, e) = f.call(&mut s, "search hello");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_OPEN, e.unwrap());
}

#[test]
fn rate_limit_window_resets_and_ui_never_limited() {
    let f = PortFixture::new();
    f.set_settings(json!({"ipc": {"maxSearchPerMinute": 3}}));
    let mut pub_s = PortFixture::pub_sess();
    let mut ui_s = PortFixture::ui();

    for _ in 0..3 {
        let (ok, _, _) = f.call(&mut pub_s, r#"search {"q":"hello","mode":"text"}"#);
        assert!(ok);
    }
    let (ok, _, err) = f.call(&mut pub_s, r#"search {"q":"hello","mode":"text"}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_RATE, err.unwrap());

    // UI session is never rate-limited.
    for _ in 0..10 {
        let (ok, _, _) = f.call(&mut ui_s, r#"search {"q":"hello","mode":"text"}"#);
        assert!(ok);
    }

    // Advancing clock past 60 s resets the public session window.
    f.advance(ChronoDuration::seconds(61));
    let (ok, _, _) = f.call(&mut pub_s, r#"search {"q":"hello","mode":"text"}"#);
    assert!(ok);
}

#[test]
fn embedder_text_mode_skips_embedder_and_degraded_flags() {
    let f = PortFixture::new();
    let id = f
        .store
        .add_screen_capture(&screen(Utc::now(), "code.exe", "alpha report", true))
        .unwrap();
    f.store
        .set_embedding(&id, &vec_seeded(7), &vec_info())
        .unwrap();
    let mut s = PortFixture::ui();

    // Text mode never calls the embedder and reports no `degraded`.
    let (ok, data, _) = f.call(&mut s, r#"search {"q":"alpha","mode":"text"}"#);
    assert!(ok);
    let data = data.unwrap();
    assert_eq!(0, f.embedder.calls.load(Ordering::Acquire));
    assert!(data.get("degraded").is_none());

    // Cold + failed embedder -> degraded = "vector-unavailable".
    f.embedder.warm.store(false, Ordering::Release);
    f.embedder.fail.store(true, Ordering::Release);
    let (ok, data, _) = f.call(&mut s, "search alpha");
    assert!(ok);
    assert_eq!("vector-unavailable", data.unwrap()["degraded"]);

    // Warm + failed embedder -> degraded = "vector-unavailable".
    f.embedder.warm.store(true, Ordering::Release);
    let (ok, data, _) = f.call(&mut s, "search alpha");
    assert!(ok);
    assert_eq!("vector-unavailable", data.unwrap()["degraded"]);

    // Cold + slow embedder exceeds cold wait -> degraded = "vector".
    f.embedder.fail.store(false, Ordering::Release);
    f.embedder.warm.store(false, Ordering::Release);
    f.p.set_cold_embed_wait_for_test(Duration::from_millis(80));
    *f.embedder.on_embed.lock().unwrap() = Some(Box::new(|_, ct| {
        ct.wait_timeout(Duration::from_secs(5));
        Err(EmbedError::Cancelled)
    }));
    let (ok, data, _) = f.call(&mut s, "search alpha");
    assert!(ok);
    assert_eq!("vector", data.unwrap()["degraded"]);
}

#[test]
fn fit_detail_cuts_multibyte_utf8_to_budget() {
    let d = CaptureDetail {
        id: "s_1".to_string(),
        ts: Utc::now(),
        kind: CaptureKind::Screen,
        app: "notes.exe".to_string(),
        title: "UTF-8 test".to_string(),
        text: "é".repeat(100_000),
        has_thumb: false,
        duration_s: None,
        site: String::new(),
        replay_id: None,
        replay_offset_ms: None,
    };
    let fitted = CommandProcessor::fit_detail(&d, 4096);
    assert!(
        fitted.len() <= 4096,
        "fitted json must fit within 4096 bytes, got {}",
        fitted.len()
    );
    assert!(fitted.contains("\"text\":\""));
    let parsed: Value = serde_json::from_str(&fitted).expect("valid json after UTF-8 truncation");
    assert!(parsed["text"].as_str().unwrap().chars().all(|c| c == 'é'));
}

#[test]
fn thumb_width_edges_default_usage_and_ui_bypass() {
    let f = PortFixture::new();
    let mut pub_s = PortFixture::pub_sess();
    let mut ui_s = PortFixture::ui();

    // Public denied when allowThumbnails=false, UI bypasses.
    let (ok, _, err) = f.call(&mut pub_s, "thumb s_1");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_THUMBS_OFF, err.unwrap());

    let (ok, data, _) = f.call(&mut ui_s, "thumb s_1");
    assert!(ok);
    let data = data.unwrap();
    assert_eq!(320, data["width"]);
    assert_eq!("AQID", data["base64"]);

    // Enable public thumbnails and check edges & usage.
    f.set_settings(json!({"ipc": {"allowThumbnails": true}}));
    let (ok, data, _) = f.call(&mut pub_s, "thumb s_1 640");
    assert!(ok);
    assert_eq!(640, data.unwrap()["width"]);

    for bad in ["thumb s_1 63", "thumb s_1 1281"] {
        let (ok, _, err) = f.call(&mut pub_s, bad);
        assert!(!ok);
        assert_eq!("Invalid width.", err.unwrap());
    }
    for bad in ["thumb", "thumb s_1 wide", "thumb s_1 320 extra"] {
        let (ok, _, err) = f.call(&mut pub_s, bad);
        assert!(!ok);
        assert_eq!(CommandProcessor::USAGE_THUMB, err.unwrap());
    }
    let (ok, _, err) = f.call(&mut pub_s, "thumb missing");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_FOUND, err.unwrap());
}

#[test]
fn pause_and_resume_bounds_usage_and_reasons() {
    let f = PortFixture::new();
    f.set_settings(json!({"ipc": {"allowControl": true, "maxPauseS": 600}}));
    let mut pub_s = PortFixture::pub_sess();
    let mut ui_s = PortFixture::ui();

    for bad in ["pause 0", "pause 601"] {
        let (ok, _, err) = f.call(&mut pub_s, bad);
        assert!(!ok);
        assert_eq!("Invalid seconds.", err.unwrap());
    }
    for bad in ["pause soon", "pause 1 2"] {
        let (ok, _, err) = f.call(&mut pub_s, bad);
        assert!(!ok);
        assert_eq!(CommandProcessor::USAGE_PAUSE, err.unwrap());
    }

    let (ok, _, _) = f.call(&mut pub_s, "pause");
    assert!(ok);
    assert!(
        f.agent
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("pause ") && c.ends_with("ipc")),
        "bare pause should call agent.pause(None, 'ipc')"
    );

    let (ok, _, _) = f.call(&mut pub_s, "pause 120");
    assert!(ok);
    let (ok, _, _) = f.call(&mut pub_s, "resume");
    assert!(ok);
    let (ok, _, _) = f.call(&mut ui_s, "pause 30");
    assert!(ok);
    let (ok, _, _) = f.call(&mut ui_s, "resume");
    assert!(ok);

    let calls = f.agent.calls.lock().unwrap().clone();
    assert_eq!(
        vec![
            "pause  ipc",
            "pause 120 ipc",
            "resume ipc",
            "pause 30 user",
            "resume user",
        ],
        calls
    );
}

#[test]
fn purge_last_confirm_covers_captures_stored_after_preview() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    let t0 = *f.now.lock().unwrap();
    let id_before = f
        .store
        .add_screen_capture(&screen(t0 - ChronoDuration::seconds(30), "app.exe", "before", false))
        .unwrap();

    let (ok, preview, _) = f.call(&mut ui_s, "purge last 300");
    assert!(ok);
    let token = preview.unwrap()["confirm"].as_str().unwrap().to_string();

    // Capture arrives after preview and before confirm.
    f.advance(ChronoDuration::seconds(5));
    let t1 = *f.now.lock().unwrap();
    let id_after = f
        .store
        .add_screen_capture(&screen(t1, "app.exe", "after-preview", false))
        .unwrap();

    let (ok, res, _) = f.call(&mut ui_s, &format!("purge confirm {token}"));
    assert!(ok);
    assert!(res.unwrap()["purged"].as_i64().unwrap() >= 2);
    assert!(f.store.get(&id_before).unwrap().is_none());
    assert!(f.store.get(&id_after).unwrap().is_none());
}

#[test]
fn purge_two_step_tokens_usage_expiry_single_use_and_d13_overflow() {
    let f = PortFixture::new();
    let mut pub_s = PortFixture::pub_sess();
    let mut ui_s = PortFixture::ui();

    // Public purge requires both allowPurge and allowControl.
    f.set_settings(json!({"ipc": {"allowPurge": true, "allowControl": false}}));
    let (ok, _, err) = f.call(&mut pub_s, "purge all");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_CONTROL_OFF, err.unwrap());

    f.set_settings(json!({"ipc": {"allowPurge": true, "allowControl": true}}));

    let t = *f.now.lock().unwrap();
    let id1 = f
        .store
        .add_screen_capture(&screen(t - ChronoDuration::seconds(20), "a.exe", "t1", false))
        .unwrap();
    f.store.set_embedding(&id1, &vec_seeded(1), &vec_info()).unwrap();
    let id2 = f
        .store
        .add_screen_capture(&screen(t - ChronoDuration::seconds(10), "b.exe", "t2", false))
        .unwrap();
    f.store.set_embedding(&id2, &vec_seeded(2), &vec_info()).unwrap();

    // Usage errors (exact USAGE_PURGE on public pipe).
    for bad in [
        "purge",
        "purge bogus",
        "purge app",
        "purge last",
        "purge last abc",
        "purge range x y",
        "purge confirm",
        "purge id",
    ] {
        let (ok, _, err) = f.call(&mut pub_s, bad);
        assert!(!ok, "expected error for {bad}");
        assert_eq!(CommandProcessor::USAGE_PURGE, err.unwrap(), "cmd={bad}");
    }
    let (ok, _, err) = f.call(&mut pub_s, "purge last 0");
    assert!(!ok);
    assert_eq!("Invalid seconds.", err.unwrap());
    let (ok, _, err) = f.call(
        &mut pub_s,
        &format!(
            "purge range {} {}",
            t.to_rfc3339(),
            (t - ChronoDuration::minutes(5)).to_rfc3339()
        ),
    );
    assert!(!ok);
    assert_eq!("Invalid from.", err.unwrap());

    // Public pipe cannot use `purge all`.
    let (ok, _, err) = f.call(&mut pub_s, "purge all");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_PURGE, err.unwrap());

    // Two-step token works once, returns preview `rows`, then invalid on reuse.
    let (ok, p1, _) = f.call(&mut pub_s, &format!("purge id {id1}"));
    assert!(ok);
    let p1 = p1.unwrap();
    assert_eq!(1, p1["rows"]);
    let tok1 = p1["confirm"].as_str().unwrap().to_string();
    let (ok, _, _) = f.call(&mut pub_s, &format!("purge confirm {tok1}"));
    assert!(ok);
    assert!(f.store.get(&id1).unwrap().is_none());
    let (ok, _, err) = f.call(&mut pub_s, &format!("purge confirm {tok1}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());

    // Token expires after 31 s (30 s lifetime) and cannot be confirmed from another connection.
    let (ok, p2, _) = f.call(&mut pub_s, &format!("purge id {id2}"));
    assert!(ok);
    let p2 = p2.unwrap();
    assert_eq!(1, p2["rows"]);
    let tok2 = p2["confirm"].as_str().unwrap().to_string();
    let mut pub_other = PortFixture::pub_sess();
    let (ok, _, err) = f.call(&mut pub_other, &format!("purge confirm {tok2}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());
    f.advance(ChronoDuration::seconds(31));
    let (ok, _, err) = f.call(&mut pub_s, &format!("purge confirm {tok2}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());
    assert!(f.store.get(&id2).unwrap().is_some());

    // D13: integer overflow on `purge last` seconds is rejected cleanly with "Invalid seconds.",
    // and exe/url normalization works.
    let (ok, _, err) = f.call(&mut ui_s, "purge last 999999999999999999999");
    assert!(!ok);
    assert_eq!("Invalid seconds.", err.unwrap());
    let (ok, p_app, _) = f.call(&mut ui_s, "purge app b.exe");
    assert!(ok);
    let p_app = p_app.unwrap();
    assert_eq!(1, p_app["rows"]);
    let (ok, _, _) = f.call(&mut ui_s, &format!("purge confirm {}", p_app["confirm"].as_str().unwrap()));
    assert!(ok);
    assert!(f.store.get(&id2).unwrap().is_none());

    // D13 normalization end-to-end:
    // 1) purge app claude.exe hitting row stored as "claude"
    let id_c1 = f.store.add_screen_capture(&screen(t, "claude", "c1", false)).unwrap();
    let (ok, p_c1, _) = f.call(&mut ui_s, "purge app claude.exe");
    assert!(ok);
    let p_c1 = p_c1.unwrap();
    assert_eq!(1, p_c1["rows"]);
    let tok_c1 = p_c1["confirm"].as_str().unwrap().to_string();
    assert!(f.call(&mut ui_s, &format!("purge confirm {tok_c1}")).0);
    assert!(f.store.get(&id_c1).unwrap().is_none());

    // 2) purge app chrome hitting row stored as "chrome.exe"
    let id_c2 = f.store.add_screen_capture(&screen(t, "chrome.exe", "c2", false)).unwrap();
    let (ok, p_c2, _) = f.call(&mut ui_s, "purge app chrome");
    assert!(ok);
    let p_c2 = p_c2.unwrap();
    assert_eq!(1, p_c2["rows"]);
    let tok_c2 = p_c2["confirm"].as_str().unwrap().to_string();
    assert!(f.call(&mut ui_s, &format!("purge confirm {tok_c2}")).0);
    assert!(f.store.get(&id_c2).unwrap().is_none());

    // 3) purge site https://www.google.com/path hitting row with site "google.com"
    let mut s_site = screen(t, "browser.exe", "c3", false);
    s_site.site = "google.com".to_string();
    let id_c3 = f.store.add_screen_capture(&s_site).unwrap();
    let (ok, p_c3, _) = f.call(&mut ui_s, "purge site https://www.google.com/path");
    assert!(ok);
    let p_c3 = p_c3.unwrap();
    assert_eq!(1, p_c3["rows"]);
    let tok_c3 = p_c3["confirm"].as_str().unwrap().to_string();
    assert!(f.call(&mut ui_s, &format!("purge confirm {tok_c3}")).0);
    assert!(f.store.get(&id_c3).unwrap().is_none());
}

#[test]
fn purge_all_no_rows_in_preview_and_expires_after_61s() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();

    // Without a recent unlock, `purge all` returns ERR_UNLOCK_REQUIRED.
    let (ok, _, err) = f.call(&mut ui_s, "purge all");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_UNLOCK_REQUIRED, err.unwrap());

    // After unlock, `purge all` returns a confirm token with no `rows` field.
    let (ok, _, _) = f.call(&mut ui_s, "unlock");
    assert!(ok);
    let (ok, preview, _) = f.call(&mut ui_s, "purge all");
    assert!(ok);
    let preview = preview.unwrap();
    assert!(preview.get("rows").is_none());
    let tok_expired = preview["confirm"].as_str().unwrap().to_string();

    // After 61 s both the token and the recent-unlock window expire.
    f.advance(ChronoDuration::seconds(61));
    let (ok, _, err) = f.call(&mut ui_s, &format!("purge confirm {tok_expired}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());
    let (ok, _, err) = f.call(&mut ui_s, "purge all");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_UNLOCK_REQUIRED, err.unwrap());
    assert_eq!(0, f.purge_all_calls.load(Ordering::Acquire));

    let (ok, _, _) = f.call(&mut ui_s, "unlock");
    assert!(ok);
    let (ok, preview2, _) = f.call(&mut ui_s, "purge all");
    assert!(ok);
    let tok_ok = preview2.unwrap()["confirm"].as_str().unwrap().to_string();
    let (ok, res, _) = f.call(&mut ui_s, &format!("purge confirm {tok_ok}"));
    assert!(ok);
    assert_eq!(42, res.unwrap()["purged"]);
    assert_eq!(1, f.purge_all_calls.load(Ordering::Acquire));
}

#[test]
fn settings_get_set_validation_batch4_bad_settings_reserved_pipes_and_chat_bounds() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();

    let (ok, initial, _) = f.call(&mut ui_s, "settings get");
    assert!(ok);
    let initial = initial.unwrap();
    assert_eq!(30, initial["retention"]["days"]);

    // Valid partial merge preserves unrelated keys (G5: SettingsSet_MergesAndValidates).
    let (ok, updated, _) = f.call(&mut ui_s, r#"settings set {"retention":{"days":14}}"#);
    assert!(ok);
    let updated = updated.unwrap();
    assert_eq!(14, updated["retention"]["days"]);
    assert_eq!(initial["capture"]["intervalMs"], updated["capture"]["intervalMs"]);

    // Reserved pipe names rejected.
    for pipe in [
        r"\\.\pipe\BetterRecall.ui",
        "BetterRecall.ui",
        "BetterRecall.mcp",
        "BetterRecall.worker.x",
        "bad/name",
        "",
    ] {
        let (ok, _, err) = f.call(
            &mut ui_s,
            &format!("settings set {}", json!({"ipc": {"pipeName": pipe}})),
        );
        assert!(!ok, "reserved/invalid pipeName should fail: {pipe}");
        assert!(err.is_some());
    }

    // Exact syntax/usage errors on settings verb.
    let (ok, _, err) = f.call(&mut ui_s, "settings bogus");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_SETTINGS, err.unwrap());
    let (ok, _, err) = f.call(&mut ui_s, "settings set {not-json");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_JSON, err.unwrap());
    let (ok, _, err) = f.call(&mut ui_s, r#"settings set {"retention":{"days":0}}"#);
    assert!(!ok);
    assert_eq!("Invalid retention.days.", err.unwrap());

    // Batch 4 bad settings: out-of-range values rejected and previous settings kept intact.
    let bad_patches = [
        json!({"capture": {"intervalMs": 100}}),
        json!({"retention": {"days": 0}}),
        json!({"retention": {"days": 9999}}),
        json!({"storage": {"maxGb": 0}}),
        json!({"ipc": {"maxResults": 0}}),
        json!({"ipc": {"maxSearchPerMinute": 0}}),
        json!({"security": {"unlockTimeoutMin": 0}}),
        json!({"ocr": {"languages": "not-a-valid-lang-tag-!!!"}}),
    ];
    for patch in bad_patches {
        let (ok, _, err) = f.call(&mut ui_s, &format!("settings set {patch}"));
        assert!(!ok, "bad settings patch should fail: {patch}");
        assert!(err.is_some());
    }
    assert_eq!(14, f.current_settings().retention.days);

    // G5 ChatSettingsTests.Ranges_match_the_agent: UI SettingsSchema bounds vs SettingsStore::validate.
    let bounds: [(&str, i64, i64); 4] = [
        ("chatMaxHits", 1, 20),
        ("chatMaxImages", 0, 8),
        ("dailyRequests", 0, 100_000),
        ("dailyTokens", 0, 1_000_000_000),
    ];
    for (field, min, max) in bounds {
        for valid in [min, max] {
            let patch = json!({"ai": {field: valid}});
            let (ok, _, err) = f.call(&mut ui_s, &format!("settings set {patch}"));
            assert!(ok, "{field}={valid} should pass, got {err:?}");
        }
        for invalid in [min - 1, max + 1] {
            let patch = json!({"ai": {field: invalid}});
            let (ok, _, _) = f.call(&mut ui_s, &format!("settings set {patch}"));
            assert!(!ok, "{field}={invalid} should fail");
        }
    }

    // Also verify SettingsStore::validate directly.
    let mut s = f.current_settings();
    s.ai.chat_max_hits = 21;
    assert!(SettingsStore::validate(&s).is_some());
    s.ai.chat_max_hits = 20;
    s.ai.chat_max_images = 9;
    assert!(SettingsStore::validate(&s).is_some());
    s.ai.chat_max_images = 8;
    s.ai.daily_requests = 100_001;
    assert!(SettingsStore::validate(&s).is_some());
    s.ai.daily_requests = 100_000;
    s.ai.daily_tokens = 1_000_000_001;
    assert!(SettingsStore::validate(&s).is_some());
    s.ai.daily_tokens = 1_000_000_000;
    assert!(SettingsStore::validate(&s).is_none());
}

#[test]
fn timeline_keyframe_audit_replay_usage_and_order_and_replay_search() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let t1 = t0 + ChronoDuration::minutes(10);
    let t2 = t0 + ChronoDuration::minutes(20);
    let t3 = t0 + ChronoDuration::minutes(30);

    let id1 = f
        .store
        .add_screen_capture(&screen(t1, "app1.exe", "first", true))
        .unwrap();
    f.store.set_embedding(&id1, &vec_seeded(1), &vec_info()).unwrap();
    let id2 = f
        .store
        .add_screen_capture(&screen(t2, "app2.exe", "second", true))
        .unwrap();
    f.store.set_embedding(&id2, &vec_seeded(2), &vec_info()).unwrap();
    let id_out = f
        .store
        .add_screen_capture(&screen(t3, "app3.exe", "outside", true))
        .unwrap();
    f.store.set_embedding(&id_out, &vec_seeded(3), &vec_info()).unwrap();

    // Add a replay frame inside the timeline range (G5: Replay_matches_carry_replay_and_offset_and_skip_timeline).
    let rf_vec = vec_seeded(55);
    let rf_id = f
        .store
        .add_replay_frame(&NewReplayFrame {
            ts: t1 + ChronoDuration::minutes(5),
            replay_id: "rep-99".to_string(),
            offset_ms: 42_500,
            app: "game.exe".to_string(),
            title: "Boss fight replay frame".to_string(),
            monitor: 0,
            width: 1280,
            height: 720,
            image: FAKE_WEBP.to_vec(),
            image_mime: "image/webp".to_string(),
            site: String::new(),
            captured_at: None,
        })
        .unwrap();
    f.store
        .set_embedding(&rf_id, &rf_vec, &vec_info())
        .unwrap();

    // G5: Timeline_NewestFirst_WithinRange + skips replay frames.
    let from = (t0 + ChronoDuration::minutes(5)).to_rfc3339();
    let to = (t0 + ChronoDuration::minutes(25)).to_rfc3339();
    let (ok, tl, _) = f.call(&mut ui_s, &format!("timeline {from} {to} 10"));
    assert!(ok);
    let items = tl.unwrap()["items"].as_array().unwrap().clone();
    assert_eq!(2, items.len(), "timeline should exclude outside and replay rows: {items:?}");
    assert_eq!(id2, items[0]["id"], "newest first");
    assert_eq!(id1, items[1]["id"]);

    // Search returns the replay frame with replayId and replayOffsetMs.
    f.embedder
        .map
        .lock()
        .unwrap()
        .insert("Boss fight".to_string(), rf_vec);
    let (ok, sr, _) = f.call(&mut ui_s, r#"search {"q":"Boss fight","mode":"text"}"#);
    assert!(ok);
    let hits = sr.unwrap()["results"].as_array().unwrap().clone();
    let rf_hit = hits.iter().find(|h| h["id"] == rf_id).expect("replay frame hit");
    assert_eq!("rep-99", rf_hit["replayId"]);
    assert_eq!(42_500, rf_hit["replayOffsetMs"]);

    // Keyframe returns stored image bytes.
    let (ok, kf, _) = f.call(&mut ui_s, &format!("keyframe {id1}"));
    assert!(ok);
    let kf_data: ThumbData = serde_json::from_value(kf.unwrap()).unwrap();
    assert_eq!(FAKE_WEBP, kf_data.bytes.as_slice());

    // Usage & validation checks for timeline, keyframe, audit, replay.
    let (ok, _, err) = f.call(&mut ui_s, "timeline");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_TIMELINE, err.unwrap());

    let (ok, _, err) = f.call(&mut ui_s, &format!("timeline {to} {from}"));
    assert!(!ok);
    assert_eq!("Invalid from.", err.unwrap());

    let (ok, _, err) = f.call(&mut ui_s, &format!("timeline {from} {to} 0"));
    assert!(!ok);
    assert_eq!("Invalid limit.", err.unwrap());

    let (ok, _, err) = f.call(&mut ui_s, "keyframe");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_KEYFRAME, err.unwrap());

    let (ok, _, err) = f.call(&mut ui_s, "audit nope");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_AUDIT, err.unwrap());

    let (ok, _, err) = f.call(&mut ui_s, "replay bogus");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_REPLAY, err.unwrap());

    let (ok, _, err) = f.call(&mut ui_s, "replay export r1 relative.mp4");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_REPLAY, err.unwrap());

    let (ok, _, err) = f.call(&mut ui_s, r"replay export nope C:\x.mp4");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_FOUND, err.unwrap());

    // Replay save, list, export, delete flow.
    let (ok, saved, _) = f.call(&mut ui_s, "replay save");
    assert!(ok);
    assert_eq!("r1", saved.unwrap()["id"]);
    let (ok, listed, _) = f.call(&mut ui_s, "replay list");
    assert!(ok);
    assert_eq!(1, listed.unwrap()["items"].as_array().unwrap().len());
    let export_mp4 = f.home.path().join("clip with spaces.mp4");
    let (ok, _, _) = f.call(
        &mut ui_s,
        &format!("replay export r1 {}", export_mp4.display()),
    );
    assert!(ok);
    assert!(
        f.agent
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == &format!("export r1 {}", export_mp4.display())),
        "export path with spaces must be passed verbatim to agent"
    );
    let (ok, _, _) = f.call(&mut ui_s, "replay delete r1");
    assert!(ok);
    let (ok, _, err) = f.call(&mut ui_s, "replay delete r1");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_FOUND, err.unwrap());
}

#[test]
fn closed_store_error_becomes_internal_error() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    f.store.close();
    let (ok, _, err) = f.call(&mut ui_s, "get s_any");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_INTERNAL, err.unwrap());
}

#[test]
fn history_facets_site_search_and_export_guards_and_zip() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    let t = Utc::now() - ChronoDuration::minutes(30);

    let mut s1 = screen(t, "chrome.exe", "PR #1", true);
    s1.site = "github.com".to_string();
    let id1 = f.store.add_screen_capture(&s1).unwrap();
    f.store.set_embedding(&id1, &vec_seeded(1), &vec_info()).unwrap();

    let mut s2 = screen(t + ChronoDuration::minutes(2), "code.exe", "Main.rs", true);
    s2.site = String::new();
    let id2 = f.store.add_screen_capture(&s2).unwrap();
    f.store.set_embedding(&id2, &vec_seeded(2), &vec_info()).unwrap();

    // Facets list apps & sites with exact counts.
    let (ok, facets, _) = f.call(&mut ui_s, "facets");
    assert!(ok);
    let facets = facets.unwrap();
    let apps = facets["apps"].as_array().unwrap();
    let sites = facets["sites"].as_array().unwrap();
    assert!(apps.iter().any(|a| a["name"] == "chrome.exe" && a["count"] == 1));
    assert!(apps.iter().any(|a| a["name"] == "code.exe" && a["count"] == 1));
    assert!(sites.iter().any(|s| s["name"] == "github.com" && s["count"] == 1));

    // Site-filtered search (exact host, `www.` normalization, and invalid site rejection).
    let (ok, sr, _) = f.call(&mut ui_s, r#"search {"q":"PR","site":"github.com","mode":"text"}"#);
    assert!(ok);
    let hits = sr.unwrap()["results"].as_array().unwrap().clone();
    assert_eq!(1, hits.len());
    assert_eq!(id1, hits[0]["id"]);

    let (ok, sr_www, _) = f.call(&mut ui_s, r#"search {"q":"PR","site":"www.github.com","mode":"text"}"#);
    assert!(ok);
    let hits_www = sr_www.unwrap()["results"].as_array().unwrap().clone();
    assert_eq!(1, hits_www.len());
    assert_eq!(id1, hits_www[0]["id"]);

    let (ok, _, err) = f.call(&mut ui_s, r#"search {"q":"PR","site":"   ","mode":"text"}"#);
    assert!(!ok);
    assert_eq!("Invalid site.", err.unwrap());

    // History add, list, delete, clear, usage, and privacy.searchHistory=false clearing & stopping saves.
    let (ok, _, _) = f.call(&mut ui_s, "history add invoice");
    assert!(ok);
    let (ok, _, _) = f.call(&mut ui_s, "history add roadmap");
    assert!(ok);
    let (ok, list, _) = f.call(&mut ui_s, "history");
    assert!(ok);
    assert_eq!(2, list.unwrap()["items"].as_array().unwrap().len());
    let (ok, _, _) = f.call(&mut ui_s, "history delete invoice");
    assert!(ok);
    let (ok, list, _) = f.call(&mut ui_s, "history");
    assert!(ok);
    assert_eq!(1, list.unwrap()["items"].as_array().unwrap().len());

    // Turning privacy.searchHistory=false deletes saved history and prevents new entries.
    let (ok, _, err) = f.call(&mut ui_s, r#"settings set {"privacy":{"searchHistory":false}}"#);
    assert!(ok, "disabling searchHistory should succeed: {err:?}");
    assert!(
        f.store.history(50).unwrap().is_empty(),
        "disabling searchHistory must delete saved searches"
    );
    let (ok, _, _) = f.call(&mut ui_s, "history add secret-term");
    assert!(ok);
    let (ok, list, _) = f.call(&mut ui_s, "history");
    assert!(ok);
    assert_eq!(0, list.unwrap()["items"].as_array().unwrap().len());
    assert!(f.store.history(50).unwrap().is_empty());

    let (ok, _, _) = f.call(&mut ui_s, r#"settings set {"privacy":{"searchHistory":true}}"#);
    assert!(ok);
    let (ok, _, _) = f.call(&mut ui_s, "history add keep-then-clear");
    assert!(ok);
    let (ok, _, _) = f.call(&mut ui_s, "history clear");
    assert!(ok);
    let (ok, list, _) = f.call(&mut ui_s, "history");
    assert!(ok);
    assert_eq!(0, list.unwrap()["items"].as_array().unwrap().len());
    let (ok, _, err) = f.call(&mut ui_s, "history bogus");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_HISTORY, err.unwrap());

    // Export guards: empty, bad JSON, bad range, non-abs/non-zip path, locked pre-check.
    let (ok, _, err) = f.call(&mut ui_s, "export");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_EXPORT, err.unwrap());
    let (ok, _, err) = f.call(&mut ui_s, "export {bad");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_JSON, err.unwrap());
    let (ok, _, err) = f.call(
        &mut ui_s,
        &format!(
            "export {}",
            json!({
                "from": (t + ChronoDuration::hours(1)).to_rfc3339(),
                "to": t.to_rfc3339(),
                "path": f.home.path().join("x.zip").to_string_lossy(),
            })
        ),
    );
    assert!(!ok);
    assert_eq!("Invalid from.", err.unwrap());
    for bad_path in ["relative.zip", r"C:\tmp\notzip.txt", r"C:\tmp\bad?.zip"] {
        let (ok, _, err) = f.call(
            &mut ui_s,
            &format!(
                "export {}",
                json!({
                    "from": (t - ChronoDuration::minutes(5)).to_rfc3339(),
                    "to": (t + ChronoDuration::minutes(10)).to_rfc3339(),
                    "path": bad_path,
                })
            ),
        );
        assert!(!ok, "bad path should fail: {bad_path}");
        assert_eq!("Invalid path.", err.unwrap());
    }

    let zip_path = f.home.path().join("out.zip");
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f.call(
        &mut ui_s,
        &format!(
            "export {}",
            json!({
                "from": (t - ChronoDuration::minutes(5)).to_rfc3339(),
                "to": (t + ChronoDuration::minutes(10)).to_rfc3339(),
                "path": zip_path.to_string_lossy(),
            })
        ),
    );
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    assert!(!zip_path.exists());
    f.agent.unlocked.store(true, Ordering::Release);

    // Dry-run export counts rows without writing a file.
    let (ok, dry, _) = f.call(
        &mut ui_s,
        &format!(
            "export {}",
            json!({
                "from": (t - ChronoDuration::minutes(5)).to_rfc3339(),
                "to": (t + ChronoDuration::minutes(10)).to_rfc3339(),
                "path": zip_path.to_string_lossy(),
                "dryRun": true,
            })
        ),
    );
    assert!(ok);
    assert_eq!(2, dry.unwrap()["count"]);
    assert!(!zip_path.exists());

    // Real export creates a valid zip containing captures.json and image entries, and writes an audit row.
    let (ok, real, _) = f.call(
        &mut ui_s,
        &format!(
            "export {}",
            json!({
                "from": (t - ChronoDuration::minutes(5)).to_rfc3339(),
                "to": (t + ChronoDuration::minutes(10)).to_rfc3339(),
                "path": zip_path.to_string_lossy(),
            })
        ),
    );
    assert!(ok);
    let real = real.unwrap();
    assert_eq!(2, real["count"]);
    assert_eq!(2, real["images"]);
    let export_audit = f.store.read_audit(50).unwrap();
    assert!(
        export_audit
            .iter()
            .any(|a| a.verb == "export" && a.result_count == 2),
        "real export must write an audit row: {export_audit:?}"
    );
    let mut z = ZipArchive::new(File::open(&zip_path).unwrap()).unwrap();
    let names: Vec<String> = (0..z.len())
        .map(|i| z.by_index(i).unwrap().name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "captures.json"), "zip entries: {names:?}");
    assert!(
        names.iter().filter(|n| n.starts_with("images/")).count() >= 2,
        "zip entries: {names:?}"
    );
}

#[test]
fn long_running_export_describe_chat_and_cancelled_export_closes() {
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "export {}"));
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "describe send tok"));
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "chat send {}"));
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "chat approve {}"));
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "chat deny tok"));
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "bench full"));
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, r"replay export r1 C:\x.mp4"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Ui, "replay save"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Ui, "replay list"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Ui, "export"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Ui, "ping"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Public, "export {}"));

    let f = PortFixture::new();
    let t = Utc::now() - ChronoDuration::minutes(10);
    f.store
        .add_screen_capture(&screen(t, "chrome.exe", "cancel me", true))
        .unwrap();
    let zip_path = f.home.path().join("cancelled.zip");
    let mut ui_s = PortFixture::ui();
    let cts = CancelSource::new();
    cts.cancel();
    let req = ExportRequest {
        from: t - ChronoDuration::minutes(5),
        to: t + ChronoDuration::minutes(5),
        path: zip_path.to_string_lossy().into_owned(),
        app: None,
        jpeg: false,
        dry_run: false,
    };
    let reply = f.p.handle(
        &mut ui_s,
        &format!("export {}", serde_json::to_string(&req).unwrap()),
        &cts.token(),
    );
    assert!(reply.close, "cancelled export must close the connection");
    assert!(!zip_path.exists());
}

#[test]
fn audit_query_hash_keyed_and_cleared_by_window_and_site_purge() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    let mut pub_s = PortFixture::pub_sess();
    let t = *f.now.lock().unwrap();

    let mut s1 = screen(t - ChronoDuration::seconds(15), "chrome.exe", "secretbank statement", false);
    s1.site = "secretbank.example".to_string();
    let id1 = f.store.add_screen_capture(&s1).unwrap();
    f.store.set_embedding(&id1, &vec_seeded(3), &vec_info()).unwrap();

    // Sweep 3J B9: seed search history term + 3 searches (2 site-related, 1 unrelated).
    let (ok, _, _) = f.call(&mut ui_s, "history add secretbank statements");
    assert!(ok);
    let (ok, _, _) = f.call(&mut pub_s, r#"search {"q":"secretbank","mode":"text"}"#);
    assert!(ok);
    let (ok, _, _) = f.call(&mut pub_s, r#"search {"q":"secretbank statements","mode":"text"}"#);
    assert!(ok);
    let (ok, _, _) = f.call(&mut pub_s, r#"search {"q":"invoice","mode":"text"}"#);
    assert!(ok);

    let h_stem = f.p.query_hash("secretbank");
    let h_phrase = f.p.query_hash("secretbank statements");
    let h_invoice = f.p.query_hash("invoice");
    let rows_before = f.store.read_audit(50).unwrap();
    assert!(rows_before.iter().any(|r| r.query_sha256 == h_stem));
    assert!(rows_before.iter().any(|r| r.query_sha256 == h_phrase));
    assert!(rows_before.iter().any(|r| r.query_sha256 == h_invoice));

    // Purge by site clears the 2 matching term hashes (setting them blank) while keeping "invoice".
    let (ok, prev, _) = f.call(&mut ui_s, "purge site secretbank.example");
    assert!(ok);
    let tok = prev.unwrap()["confirm"].as_str().unwrap().to_string();
    let (ok, _, _) = f.call(&mut ui_s, &format!("purge confirm {tok}"));
    assert!(ok);
    let rows_after_site = f.store.read_audit(50).unwrap();
    let search_rows: Vec<_> = rows_after_site
        .iter()
        .filter(|r| r.verb == "search")
        .collect();
    assert_eq!(3, search_rows.len(), "expected 3 search audit rows");
    assert_eq!(
        2,
        search_rows.iter().filter(|r| r.query_sha256.is_empty()).count(),
        "2 secretbank search audit rows must be blanked: {search_rows:?}"
    );
    assert!(!rows_after_site.iter().any(|r| r.query_sha256 == h_stem));
    assert!(!rows_after_site.iter().any(|r| r.query_sha256 == h_phrase));
    assert!(
        rows_after_site.iter().any(|r| r.query_sha256 == h_invoice),
        "unrelated invoice audit hash must stay intact"
    );
    assert!(
        !f.store
            .history(50)
            .unwrap()
            .iter()
            .any(|h| h.q.contains("secretbank")),
        "site purge must drop saved searches naming the site's terms"
    );

    // Search inside a time window; purge last clears that window's audit hashes too.
    let (ok, _, _) = f.call(&mut pub_s, r#"search {"q":"window-secret","mode":"text"}"#);
    assert!(ok);
    let h_win = f.p.query_hash("window-secret");
    assert!(f.store.read_audit(50).unwrap().iter().any(|r| r.query_sha256 == h_win));

    let (ok, prev2, _) = f.call(&mut ui_s, "purge last 300");
    assert!(ok);
    let tok2 = prev2.unwrap()["confirm"].as_str().unwrap().to_string();
    let (ok, _, _) = f.call(&mut ui_s, &format!("purge confirm {tok2}"));
    assert!(ok);
    assert!(!f.store.read_audit(50).unwrap().iter().any(|r| r.query_sha256 == h_win));
}

#[test]
fn unreadable_settings_refuse_saves_consent_and_reset_audit_and_lock() {
    let f = PortFixture::with_opts(PortFixtureOpts {
        initial_settings_raw: Some("{not valid json!!!".to_string()),
        ..Default::default()
    });
    assert!(f.settings.load_error().is_some());
    let mut ui_s = PortFixture::ui();

    // Settings set refuses while unreadable.
    let (ok, _, err) = f.call(&mut ui_s, r#"settings set {"retention":{"days":7}}"#);
    assert!(!ok);
    assert!(err.unwrap().contains("settings.json"));

    // Consent refuses while unreadable and NEVER calls agent.consent().
    f.agent.calls.lock().unwrap().clear();
    let (ok, _, err) = f.call(&mut ui_s, "consent");
    assert!(!ok);
    assert!(err.unwrap().contains("settings.json"));
    assert!(
        !f.agent.calls.lock().unwrap().contains(&"consent".to_string()),
        "agent.consent() must not be called while settings.json is unreadable"
    );

    // Reset works even while locked when settings.json is unreadable (Sweep 4U S6 + D4),
    // clears load_error, and writes an audit row with pipe=="ui".
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, data, err) = f.call(&mut ui_s, "settings reset");
    assert!(ok, "reset while locked when unreadable must succeed: {err:?}");
    assert!(data.is_some());
    assert!(f.settings.load_error().is_none());
    let audit = f.store.read_audit(20).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.verb == "settings reset" && a.pipe == "ui"),
        "settings reset audit row must record pipe='ui': {audit:?}"
    );

    // When healthy: reset while locked returns ERR_LOCKED; reset while unlocked returns ERR_NOTHING_TO_RESET.
    let (ok, _, err) = f.call(&mut ui_s, "settings reset");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());

    f.agent.unlocked.store(true, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, "settings reset");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOTHING_TO_RESET, err.unwrap());
}

struct BlockingTranscodeThumbs {
    entered: Arc<(Mutex<bool>, Condvar)>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl Thumbnailer for BlockingTranscodeThumbs {
    fn get_thumb(&self, _id: &str, _width: i32, _ct: &CancelToken) -> Option<ThumbData> {
        None
    }

    fn clear_cache(&self) {}

    fn transcode(&self, keyframe: ThumbData, _target: &str) -> ThumbData {
        {
            let (m, cv) = &*self.entered;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        let (m, cv) = &*self.release;
        let g = m.lock().unwrap();
        let _ = cv
            .wait_timeout_while(g, Duration::from_secs(5), |done| !*done)
            .unwrap();
        keyframe
    }
}

#[test]
fn purge_all_during_capture_export_and_replay_save_or_export() {
    // Part 1 (BC_S2): CoreHost purge_all running while an export is mid-write aborts the export
    // with ERR_PURGED_DURING and leaves no zip file behind.
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    host.open_store(&CancelToken::none()).expect("open store");
    let t = Utc::now() - ChronoDuration::minutes(10);
    let exp_id = host
        .store()
        .unwrap()
        .add_screen_capture(&screen(t, "chrome.exe", "export target", true))
        .unwrap();
    host.store()
        .unwrap()
        .set_embedding(&exp_id, &vec_seeded(9), &vec_info())
        .unwrap();

    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let blocking_thumbs = Arc::new(BlockingTranscodeThumbs {
        entered: entered.clone(),
        release: release.clone(),
    });
    let agent = Arc::new(FakeAgent::new());
    host.start_pipes(agent, Arc::new(FakeEmbedder::new()), blocking_thumbs);
    let proc = host.processor().expect("processor");

    let zip_path = h.path().join("aborted.zip");
    let zip_path_bg = zip_path.clone();
    let proc_bg = proc.clone();
    let exp_thread = thread::spawn(move || {
        let mut s = PortFixture::ui();
        let cmd = format!(
            "export {}",
            json!({
                "from": (t - ChronoDuration::minutes(5)).to_rfc3339(),
                "to": (t + ChronoDuration::minutes(5)).to_rfc3339(),
                "path": zip_path_bg.to_string_lossy(),
                "jpeg": true,
            })
        );
        let r = proc_bg.handle(&mut s, &cmd, &CancelToken::none());
        Envelope::parse(&r.line).unwrap()
    });

    // Wait until the export is inside transcode(), then run purge_all in parallel and release.
    {
        let (m, cv) = &*entered;
        let g = m.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |v| !*v)
            .unwrap();
        assert!(*g, "export never reached transcode");
    }
    // Register an on_purged listener to ensure purge_all has fired "all" before releasing transcode.
    let purged_all_seen = Arc::new((Mutex::new(false), Condvar::new()));
    let pa_seen_clone = purged_all_seen.clone();
    host.store().unwrap().on_purged(move |notice| {
        if notice.reason == "all" {
            let (m, cv) = &*pa_seen_clone;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
    });

    let host_pa = host.clone();
    let pa_thread = thread::spawn(move || host_pa.purge_all(&CancelToken::none()));
    // Wait until purge_all starts closing/notifying with reason="all", then let transcode return.
    {
        let (m, cv) = &*purged_all_seen;
        let g = m.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |v| !*v)
            .unwrap();
        assert!(*g, "on_purged(all) was never seen before release");
    }
    assert!(host.purging_all());
    {
        let (m, cv) = &*release;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }
    let (ok, _, err) = exp_thread.join().unwrap();
    let _purged = pa_thread.join().expect("join purge_all thread");
    assert!(!ok, "export interrupted by purge_all must fail");
    assert_eq!(
        CommandProcessor::ERR_PURGED_DURING,
        err.as_deref().unwrap_or("")
    );
    assert!(!zip_path.exists(), "aborted export must not leave a zip");
    host.shutdown();

    // Part 2 (P_purge_all_during_a_replay_save_or_export): replay save/export refuses cleanly
    // with REPLAY_PURGE_REFUSALS while purge_all is running or ran during save, and distinct from
    // "Replay is off.".
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    for msg in CommandProcessor::REPLAY_PURGE_REFUSALS {
        let m = msg.to_string();
        *f.agent.on_save_replay.lock().unwrap() =
            Some(Box::new(move |_| Err(AgentError::InvalidOperation(m.clone()))));
        let (ok, _, err) = f.call(&mut ui_s, "replay save");
        assert!(!ok);
        assert_eq!(CommandProcessor::ERR_PURGED_DURING, err.as_deref().unwrap());
        assert_ne!("Replay is off.", err.as_deref().unwrap());
    }

    // Hold `replay save` in flight, fire `on_store_purged(all)`, then release returning
    // AgentError::NotFound so notice decides answer -> maps to ERR_PURGED_DURING.
    let save_started = Arc::new((Mutex::new(false), Condvar::new()));
    let save_release = Arc::new((Mutex::new(false), Condvar::new()));
    let ss_bg = save_started.clone();
    let sr_bg = save_release.clone();
    *f.agent.on_save_replay.lock().unwrap() = Some(Box::new(move |_| {
        {
            let (m, cv) = &*ss_bg;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        let (m, cv) = &*sr_bg;
        let g = m.lock().unwrap();
        let _ = cv
            .wait_timeout_while(g, Duration::from_secs(5), |done| !*done)
            .unwrap();
        Err(AgentError::NotFound)
    }));
    let p_save = f.p.clone();
    let save_h = thread::spawn(move || {
        let mut s = PortFixture::ui();
        let r = p_save.handle(&mut s, "replay save", &CancelToken::none());
        Envelope::parse(&r.line).unwrap()
    });
    {
        let (m, cv) = &*save_started;
        let g = m.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |v| !*v)
            .unwrap();
        assert!(*g, "in-flight replay save never entered");
    }
    f.p.on_store_purged(&PurgeNotice {
        reason: "all".to_string(),
        ids: Vec::new(),
        from: None,
        to: None,
        newest: None,
        scope: None,
    });
    {
        let (m, cv) = &*save_release;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }
    let (ok, _, err) = save_h.join().unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PURGED_DURING, err.unwrap());

    // Hold `replay export` in flight, fire `on_store_purged(all)`, then release returning
    // `AgentError::NotFound` -> epoch bump must turn NotFound into ERR_PURGED_DURING.
    let export_started = Arc::new((Mutex::new(false), Condvar::new()));
    let export_release = Arc::new((Mutex::new(false), Condvar::new()));
    let es_bg = export_started.clone();
    let er_bg = export_release.clone();
    *f.agent.on_export_replay.lock().unwrap() = Some(Box::new(move |_, _, _| {
        {
            let (m, cv) = &*es_bg;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        let (m, cv) = &*er_bg;
        let g = m.lock().unwrap();
        let _ = cv
            .wait_timeout_while(g, Duration::from_secs(5), |done| !*done)
            .unwrap();
        Err(AgentError::NotFound)
    }));
    let p_bg = f.p.clone();
    let dst = f.home.path().join("r1.mp4");
    let dst_str = dst.to_string_lossy().into_owned();
    let exp_h = thread::spawn(move || {
        let mut s = PortFixture::ui();
        let r = p_bg.handle(&mut s, &format!("replay export r1 {dst_str}"), &CancelToken::none());
        Envelope::parse(&r.line).unwrap()
    });
    {
        let (m, cv) = &*export_started;
        let g = m.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |v| !*v)
            .unwrap();
        assert!(*g, "in-flight replay export never entered");
    }
    f.p.on_store_purged(&PurgeNotice {
        reason: "all".to_string(),
        ids: Vec::new(),
        from: None,
        to: None,
        newest: None,
        scope: None,
    });
    {
        let (m, cv) = &*export_release;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }
    let (ok, _, err) = exp_h.join().unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PURGED_DURING, err.unwrap());

    // Without a purge notice, `AgentError::NotFound` answers ERR_NOT_FOUND.
    let mut ui_s2 = PortFixture::ui();
    *f.agent.on_export_replay.lock().unwrap() = Some(Box::new(|_, _, _| Err(AgentError::NotFound)));
    let (ok, _, err) = f.call(&mut ui_s2, &format!("replay export r1 {}", dst.display()));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_FOUND, err.unwrap());
}
