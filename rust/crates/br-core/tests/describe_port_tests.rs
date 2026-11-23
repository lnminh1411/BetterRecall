// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sweep 6 G3 ported tests for `ApiKeyStore`, `CloudDescriber`, and `describe preview`/`send`.

mod common;

use std::fs;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use br_contracts::{AiDefaults, DataPaths};
use br_core::{ApiKeyStore, CloudDescriber, CommandProcessor};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common::a3::FakeHttpServer;
use common::ports::{FixedRedactor, PortFixture, PortFixtureOpts};
use common::{audio, screen, vec_info, vec_seeded};

#[test]
fn api_key_encrypted_never_returned_and_legacy_provider_binding() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();

    // Legacy provider inference from key prefix.
    assert_eq!(
        Some(AiDefaults::CLAUDE),
        ApiKeyStore::legacy_provider("sk-ant-api03-abcdef")
    );
    assert_eq!(
        Some(AiDefaults::OPEN_AI),
        ApiKeyStore::legacy_provider("sk-proj-abcdef123456")
    );
    assert_eq!(
        Some(AiDefaults::GEMINI),
        ApiKeyStore::legacy_provider("AIzaSyDummyKey123456")
    );
    assert_eq!(None, ApiKeyStore::legacy_provider("unknown-shape-key"));

    // B9: Claude sk-ant- key must NEVER be treated as OpenAI.
    assert_ne!(
        Some(AiDefaults::OPEN_AI),
        ApiKeyStore::legacy_provider("sk-ant-12345")
    );

    // Invalid key containing whitespace is refused with ERR_API_KEY_RULE.
    let (ok, _, err) = f.call(&mut ui_s, "ai setkey has space");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_API_KEY_RULE, err.unwrap());

    // `ai setkey` stores key DPAPI-encrypted on disk, never returned by `ai`, `settings get`,
    // `settings.json`, or `audit`.
    let secret = "AIzaSy-SUPER-SECRET-KEY-98765";
    let (ok, st, _) = f.call(&mut ui_s, &format!("ai setkey {secret}"));
    assert!(ok);
    let st = st.unwrap();
    assert_eq!(true, st["keySet"]);
    assert!(!st.to_string().contains(secret));

    let raw_key_file = fs::read(DataPaths::ai_key_file(f.home.path())).unwrap();
    assert!(
        !raw_key_file
            .windows(secret.len())
            .any(|w| w == secret.as_bytes()),
        "key file must not store plaintext secret"
    );

    let (ok, settings_json, _) = f.call(&mut ui_s, "settings get");
    assert!(ok);
    assert!(!settings_json.unwrap().to_string().contains(secret));

    let settings_file_raw =
        fs::read_to_string(DataPaths::settings_file(f.home.path())).unwrap_or_default();
    assert!(!settings_file_raw.contains(secret));

    let audit = f.store.read_audit(20).unwrap();
    assert!(audit.iter().any(|a| a.verb == "ai setkey"));
    assert!(!serde_json::to_string(&audit).unwrap().contains(secret));

    // Bound to provider AND endpoint: changing either hides the key.
    let ai_gemini = f.current_settings().ai;
    assert_eq!(Some(secret.to_string()), f.ai_key.read(&ai_gemini));
    let mut ai_claude = ai_gemini.clone();
    ai_claude.provider = AiDefaults::CLAUDE.to_string();
    assert!(f.ai_key.read(&ai_claude).is_none());
    let mut ai_other_ep = ai_gemini.clone();
    ai_other_ep.endpoint = "http://127.0.0.1:9999".to_string();
    assert!(f.ai_key.read(&ai_other_ep).is_none());

    // `ai clearkey` deletes the key file.
    let (ok, st, _) = f.call(&mut ui_s, "ai clearkey");
    assert!(ok);
    assert_eq!(false, st.unwrap()["keySet"]);
    assert!(!DataPaths::ai_key_file(f.home.path()).exists());

    // Legacy BRA1 file on disk (no explicit binding): Claude `sk-ant-` key reads ONLY under Claude
    // (never OpenAI or Gemini), and an unknown-shape legacy key reads under NO provider.
    let write_legacy_bra1 = |raw: &str| {
        let blob = ApiKeyStore::protect(raw.as_bytes()).expect("dpapi protect");
        let mut file = Vec::with_capacity(4 + blob.len());
        file.extend_from_slice(b"BRA1");
        file.extend_from_slice(&blob);
        let p = DataPaths::ai_key_file(f.home.path());
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, file).unwrap();
    };
    let mut ai_g = f.current_settings().ai;
    ai_g.provider = AiDefaults::GEMINI.to_string();
    ai_g.endpoint.clear();
    let mut ai_c = ai_g.clone();
    ai_c.provider = AiDefaults::CLAUDE.to_string();
    let mut ai_o = ai_g.clone();
    ai_o.provider = AiDefaults::OPEN_AI.to_string();

    write_legacy_bra1("sk-ant-api03-legacy-claude-key");
    assert_eq!(
        Some("sk-ant-api03-legacy-claude-key".to_string()),
        f.ai_key.read(&ai_c)
    );
    assert!(f.ai_key.read(&ai_o).is_none(), "Claude legacy key must never read for OpenAI");
    assert!(f.ai_key.read(&ai_g).is_none(), "Claude legacy key must never read for Gemini");

    write_legacy_bra1("mystery-unrecognized-key-shape");
    assert!(f.ai_key.read(&ai_c).is_none());
    assert!(f.ai_key.read(&ai_o).is_none());
    assert!(f.ai_key.read(&ai_g).is_none());
}

#[test]
fn describe_gates_preview_expiry_settings_changed_locked_purged_and_cross_connection() {
    let srv = FakeHttpServer::start();
    let srv2 = FakeHttpServer::start();
    srv.set_response_body(
        json!({"candidates": [{"content": {"parts": [{"text": "Unchanged ok"}]}}]}).to_string(),
    );
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    let mut ui_other = PortFixture::ui();
    let id = f
        .store
        .add_screen_capture(&screen(Utc::now(), "chrome.exe", "invoice", true))
        .unwrap();
    f.store.set_embedding(&id, &vec_seeded(1), &vec_info()).unwrap();
    let id_no_img = f
        .store
        .add_audio_capture(&audio(Utc::now(), "speech without image", "teams.exe"))
        .unwrap();

    // Item 38: every provider path points at the local fake server before the first call, so
    // even a regression in the off gate cannot reach a real provider host.
    f.set_settings(json!({
        "ai": {
            "enabled": false,
            "provider": "gemini",
            "endpoint": srv.base_url(),
            "model": "gemini-2.5-flash"
        }
    }));

    // Off -> ERR_AI_OFF.
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_AI_OFF, err.unwrap());

    // On, Gemini pointing at local FakeHttpServer (never real Google endpoint), no key -> ERR_NO_API_KEY.
    f.set_settings(json!({
        "ai": {
            "enabled": true,
            "provider": "gemini",
            "endpoint": srv.base_url(),
            "model": "gemini-2.5-flash"
        }
    }));
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NO_API_KEY, err.unwrap());

    // Set key -> unknown token rejected.
    let (ok, _, _) = f.call(&mut ui_s, "ai setkey AIzaSy-test-key");
    assert!(ok);
    let (ok, _, err) = f.call(&mut ui_s, "describe send unknown-tok");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());

    // Capture without keyframe -> ERR_NO_IMAGE.
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe preview {id_no_img}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NO_IMAGE, err.unwrap());

    // Preview expires after 121 s.
    let (ok, prev, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok_exp = prev.unwrap()["token"].as_str().unwrap().to_string();
    f.advance(ChronoDuration::seconds(121));
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_exp}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());

    // B2: token created on session A cannot be sent on session B.
    let (ok, prev2, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok_sess = prev2.unwrap()["token"].as_str().unwrap().to_string();
    let (ok, _, err) = f.call(&mut ui_other, &format!("describe send {tok_sess}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());

    // B18 (model change): changing model after preview refuses send AND uses up the token.
    f.set_settings(json!({"ai": {"model": "gemini-2.5-pro"}}));
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_sess}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_DESCRIBE_CHANGED, err.unwrap());
    // Re-trying even after reverting settings fails with ERR_TOKEN (ticket was consumed).
    f.set_settings(json!({"ai": {"model": "gemini-2.5-flash"}}));
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_sess}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());

    // B18 (provider change): changing provider after preview refuses send AND consumes token.
    let (ok, prev_p, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok_prov = prev_p.unwrap()["token"].as_str().unwrap().to_string();
    f.set_settings(json!({"ai": {"provider": "claude", "endpoint": srv.base_url()}}));
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_prov}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_DESCRIBE_CHANGED, err.unwrap());
    f.set_settings(json!({"ai": {"provider": "gemini", "endpoint": srv.base_url()}}));
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_prov}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());

    // B18 (endpoint change): changing endpoint after preview refuses send AND consumes token.
    let (ok, prev_e, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok_ep = prev_e.unwrap()["token"].as_str().unwrap().to_string();
    f.set_settings(json!({"ai": {"endpoint": srv2.base_url()}}));
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_ep}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_DESCRIBE_CHANGED, err.unwrap());
    f.set_settings(json!({"ai": {"endpoint": srv.base_url()}}));
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_ep}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());

    // Locked gate blocks preview and send.
    let (ok, prev3, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok_lock = prev3.unwrap()["token"].as_str().unwrap().to_string();
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_lock}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    f.agent.unlocked.store(true, Ordering::Release);

    // B8: purging capture after preview prevents send with ERR_NOT_FOUND.
    let (ok, prev4, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok_purge = prev4.unwrap()["token"].as_str().unwrap().to_string();
    f.store
        .purge(&br_contracts::PurgeScope::for_id(&id))
        .unwrap();
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok_purge}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_FOUND, err.unwrap());

    // Zero HTTP requests across ALL refusals above.
    assert!(srv.requests().is_empty(), "refusals must not send any HTTP request");
    assert!(srv2.requests().is_empty(), "refusals must not send any HTTP request to srv2");

    // Unchanged preview + send succeeds and records exactly one HTTP request.
    let id_ok = f
        .store
        .add_screen_capture(&screen(Utc::now(), "chrome.exe", "ok", true))
        .unwrap();
    f.store.set_embedding(&id_ok, &vec_seeded(9), &vec_info()).unwrap();
    let (ok, prev_ok, _) = f.call(&mut ui_s, &format!("describe preview {id_ok}"));
    assert!(ok);
    let tok_ok = prev_ok.unwrap()["token"].as_str().unwrap().to_string();
    let (ok, res_ok, _) = f.call(&mut ui_s, &format!("describe send {tok_ok}"));
    assert!(ok);
    assert_eq!("Unchanged ok", res_ok.unwrap()["text"]);
    assert_eq!(1, srv.requests().len());
    assert!(srv2.requests().is_empty());
}

#[test]
fn describe_provider_request_shapes_redactor_error_and_unreachable() {
    let srv = FakeHttpServer::start();
    let f = PortFixture::with_opts(PortFixtureOpts {
        redactor: Some(Arc::new(FixedRedactor::ok())),
        ..Default::default()
    });
    let mut ui_s = PortFixture::ui();
    let id = f
        .store
        .add_screen_capture(&screen(Utc::now(), "chrome.exe", "page", true))
        .unwrap();
    f.store.set_embedding(&id, &vec_seeded(2), &vec_info()).unwrap();

    // 1) Gemini: path, PROMPT, inlineData with redacted bytes, x-goog-api-key, audit sha256, single-use token.
    f.set_settings(json!({
        "ai": {
            "enabled": true,
            "provider": "gemini",
            "endpoint": srv.base_url(),
            "model": "gemini-2.5-flash"
        }
    }));
    let (ok, _, _) = f.call(&mut ui_s, "ai setkey AIzaSy-gem-1");
    assert!(ok);
    srv.set_response_body(
        json!({"candidates": [{"content": {"parts": [{"text": "A browser window"}]}}]}).to_string(),
    );
    let (ok, prev, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let prev = prev.unwrap();
    assert_eq!(true, prev["redacted"]);
    let tok = prev["token"].as_str().unwrap().to_string();

    let (ok, res, _) = f.call(&mut ui_s, &format!("describe send {tok}"));
    assert!(ok);
    assert_eq!("A browser window", res.unwrap()["text"]);

    // Token is single-use.
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_TOKEN, err.unwrap());

    let reqs = srv.requests();
    assert_eq!(1, reqs.len());
    assert!(reqs[0].path.contains("/v1beta/models/gemini-2.5-flash:generateContent"));
    assert_eq!(Some("AIzaSy-gem-1"), reqs[0].header("x-goog-api-key"));
    assert!(reqs[0].body.contains(CloudDescriber::PROMPT));
    assert!(reqs[0].body.contains("inlineData"));
    let expected_sha: String = Sha256::digest(FixedRedactor::REDACTED_BYTES)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let audit = f.store.read_audit(20).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.verb == "describe gemini" && a.result_count == 1 && a.query_sha256 == expected_sha)
    );

    // 2) OpenAI-compatible: works WITHOUT an API key (local server), then WITH Bearer key.
    f.set_settings(json!({
        "ai": {
            "enabled": true,
            "provider": "openai-compatible",
            "endpoint": format!("{}/v1", srv.base_url()),
            "model": "gpt-4o-mini"
        }
    }));
    let (ok, _, _) = f.call(&mut ui_s, "ai clearkey");
    assert!(ok);
    srv.set_response_body(
        json!({"choices": [{"message": {"content": "Local OpenAI reply"}}]}).to_string(),
    );
    let (ok, prev, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok = prev.unwrap()["token"].as_str().unwrap().to_string();
    let (ok, res, _) = f.call(&mut ui_s, &format!("describe send {tok}"));
    assert!(ok);
    assert_eq!("Local OpenAI reply", res.unwrap()["text"]);
    assert!(srv.requests().last().unwrap().header("authorization").is_none());

    let (ok, _, _) = f.call(&mut ui_s, "ai setkey sk-openai-test");
    assert!(ok);
    let (ok, prev, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok = prev.unwrap()["token"].as_str().unwrap().to_string();
    let (ok, _, _) = f.call(&mut ui_s, &format!("describe send {tok}"));
    assert!(ok);
    assert_eq!(
        Some("Bearer sk-openai-test"),
        srv.requests().last().unwrap().header("authorization")
    );

    // 3) Claude: headers (x-api-key + anthropic-version), max_tokens, image-then-text content order.
    f.set_settings(json!({
        "ai": {
            "enabled": true,
            "provider": "claude",
            "endpoint": srv.base_url(),
            "model": "claude-sonnet-4-5"
        }
    }));
    let (ok, _, _) = f.call(&mut ui_s, "ai setkey sk-ant-test-key");
    assert!(ok);
    srv.set_response_body(
        json!({"content": [{"type": "text", "text": "Claude description"}]}).to_string(),
    );
    let (ok, prev, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok = prev.unwrap()["token"].as_str().unwrap().to_string();
    let (ok, res, _) = f.call(&mut ui_s, &format!("describe send {tok}"));
    assert!(ok);
    assert_eq!("Claude description", res.unwrap()["text"]);
    let claude_req = srv.requests().last().unwrap().clone();
    assert_eq!(Some("sk-ant-test-key"), claude_req.header("x-api-key"));
    assert_eq!(
        Some(CloudDescriber::CLAUDE_VERSION),
        claude_req.header("anthropic-version")
    );
    let body: Value = serde_json::from_str(&claude_req.body).unwrap();
    assert!(body["max_tokens"].as_i64().unwrap() > 0);
    let content = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!("image", content[0]["type"]);
    assert_eq!("text", content[1]["type"]);

    // 4) Provider error reply scrubs key and audits result_count=0.
    srv.set_status(401);
    srv.set_response_body(
        json!({"error": {"message": "bad key sk-ant-test-key rejected"}}).to_string(),
    );
    let (ok, prev, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok = prev.unwrap()["token"].as_str().unwrap().to_string();
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok}"));
    assert!(!ok);
    let err = err.unwrap();
    assert!(err.contains("HTTP 401"));
    assert!(!err.contains("sk-ant-test-key"));
    let audit = f.store.read_audit(20).unwrap();
    assert!(audit.iter().any(|a| a.verb == "describe claude" && a.result_count == 0));

    // 5) Unreachable localhost port with a bound listener that immediately closes incoming streams -> ERR_PROVIDER_UNREACHABLE.
    let held_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_port = held_listener.local_addr().unwrap().port();
    let _closer = std::thread::spawn(move || {
        while let Ok((stream, _)) = held_listener.accept() {
            drop(stream);
        }
    });
    f.set_settings(json!({
        "ai": {
            "enabled": true,
            "provider": "openai-compatible",
            "endpoint": format!("http://127.0.0.1:{closed_port}/v1"),
            "model": "local"
        }
    }));
    let (ok, prev, _) = f.call(&mut ui_s, &format!("describe preview {id}"));
    assert!(ok);
    let tok = prev.unwrap()["token"].as_str().unwrap().to_string();
    let (ok, _, err) = f.call(&mut ui_s, &format!("describe send {tok}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PROVIDER_UNREACHABLE, err.unwrap());
}

#[test]
fn cloud_describer_parse_text_and_error_text_shapes() {
    // Gemini parts concatenation.
    assert_eq!(
        Some("Hello world".to_string()),
        CloudDescriber::parse_text(
            AiDefaults::GEMINI,
            r#"{"candidates":[{"content":{"parts":[{"text":"Hello "},{"text":"world"}]}}]}"#
        )
    );
    // OpenAI string content and array content.
    assert_eq!(
        Some("Simple".to_string()),
        CloudDescriber::parse_text(
            AiDefaults::OPEN_AI,
            r#"{"choices":[{"message":{"content":"Simple"}}]}"#
        )
    );
    assert_eq!(
        Some("Part1Part2".to_string()),
        CloudDescriber::parse_text(
            AiDefaults::OPEN_AI,
            r#"{"choices":[{"message":{"content":[{"type":"text","text":"Part1"},{"type":"text","text":"Part2"}]}}]}"#
        )
    );
    // Claude skips non-text blocks.
    assert_eq!(
        Some("Shown".to_string()),
        CloudDescriber::parse_text(
            AiDefaults::CLAUDE,
            r#"{"content":[{"type":"thinking","text":"hidden"},{"type":"text","text":"Shown"}]}"#
        )
    );
    // Unknown shape returns None.
    assert_eq!(None, CloudDescriber::parse_text(AiDefaults::GEMINI, "{}"));
    assert_eq!(None, CloudDescriber::parse_text(AiDefaults::OPEN_AI, "not json"));

    // error_text scrubs API key, strips control chars, caps length, handles array error wrapper.
    let scrubbed = CloudDescriber::error_text(
        401,
        r#"[{"error":{"message":"invalid key SECRET123\n\tbad"}}]"#,
        Some("SECRET123"),
    );
    assert_eq!("Provider error (HTTP 401): invalid key ***bad", scrubbed);
    assert!(!scrubbed.contains("SECRET123"));
    assert_eq!(
        "Provider error (HTTP 500)",
        CloudDescriber::error_text(500, "plain text", None)
    );
}
