// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for `AgentHost` `models_list` and `models_download` (stage I4)
//! against a local loopback HTTP server. No test ever contacts the internet.

mod common;

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use br_contracts::{DataPaths, ModelsProgress};
use br_core::{AgentControl, AgentError, CancelSource, CancelToken, CommandProcessor};
use br_fetch::{Catalog, CatalogFile, Component};
use br_integrity::{UPDATE_JSON, UPDATE_SIG, UpdateManifest, sign_update_manifest};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rand_core::OsRng;

use br_agent::host::UpdateTestConfig;
use common::{LocalHttpServer, TempDir, cancel_after, make_test_host, sha256_hex, until};

fn catalog_file(path: &str, data: &[u8]) -> CatalogFile {
    CatalogFile {
        path: path.to_string(),
        size: data.len() as u64,
        sha256: sha256_hex(data),
    }
}

fn make_test_catalog(base_url: &str, components: Vec<Component>) -> Catalog {
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let cat = Catalog {
        repo: "test/models".to_string(),
        revision: rev.to_string(),
        base_url: format!("https://huggingface.co/test/models/resolve/{rev}/"),
        components,
    };
    cat.validate().expect("test catalog must validate");
    cat.with_base_url_for_test(base_url)
}

#[test]
fn models_list_reports_missing_partial_and_complete_on_test_home() {
    let dir = TempDir::new("models-list");
    let host = make_test_host(&dir);

    let m_a = br#"{"id":"ocr"}"#.to_vec();
    let w_b = b"partial-weights".to_vec();
    let m_b = br#"{"id":"ner"}"#.to_vec();
    let m_c = br#"{"id":"silero-vad"}"#.to_vec();

    let catalog = make_test_catalog(
        "http://127.0.0.1:1/",
        vec![
            Component {
                id: "ocr".to_string(),
                sets: vec!["full".to_string(), "minimal".to_string()],
                bytes: m_a.len() as u64,
                files: vec![catalog_file("manifest.json", &m_a)],
            },
            Component {
                id: "ner".to_string(),
                sets: vec!["full".to_string(), "minimal".to_string()],
                bytes: (w_b.len() + m_b.len()) as u64,
                files: vec![
                    catalog_file("weights.bin", &w_b),
                    catalog_file("manifest.json", &m_b),
                ],
            },
            Component {
                id: "silero-vad".to_string(),
                sets: vec!["full".to_string()],
                bytes: m_c.len() as u64,
                files: vec![catalog_file("manifest.json", &m_c)],
            },
        ],
    );
    host.set_catalog_for_test(Some(catalog));

    let models_dir = DataPaths::models_dir(&dir.path().join("home"), &Default::default());
    // Partial: weights without manifest.json.
    fs::create_dir_all(models_dir.join("ner")).unwrap();
    fs::write(models_dir.join("ner").join("weights.bin"), &w_b).unwrap();
    // Complete: valid manifest.json.
    fs::create_dir_all(models_dir.join("silero-vad")).unwrap();
    fs::write(models_dir.join("silero-vad").join("manifest.json"), &m_c).unwrap();

    let list = host.models_list().expect("models_list ok");
    assert_eq!(list.repo, "test/models");
    assert_eq!(list.revision, "0123456789abcdef0123456789abcdef01234567");
    assert!(!list.downloading);
    assert!(list.free_bytes > 0);
    assert_eq!(list.components.len(), 3);

    assert_eq!(list.components[0].id, "ocr");
    assert_eq!(list.components[0].title, "OCR");
    assert_eq!(list.components[0].state, "missing");

    assert_eq!(list.components[1].id, "ner");
    assert_eq!(list.components[1].title, "NER");
    assert_eq!(list.components[1].state, "partial");

    assert_eq!(list.components[2].id, "silero-vad");
    assert_eq!(list.components[2].title, "Silero VAD");
    assert_eq!(list.components[2].state, "complete");

    host.shutdown();
}

#[test]
fn models_download_writes_manifest_last_and_restarts_worker_only_when_fetched() {
    let dir = TempDir::new("models-dl");
    let host = make_test_host(&dir);

    let weights = vec![0xABu8; 4096];
    let manifest = br#"{"id":"silero-vad","version":1}"#.to_vec();

    let mut files = HashMap::new();
    files.insert("/silero-vad/silero_vad.onnx".to_string(), weights.clone());
    files.insert("/silero-vad/manifest.json".to_string(), manifest.clone());
    let server = LocalHttpServer::start(files);

    let catalog = make_test_catalog(
        &server.base_url(),
        vec![Component {
            id: "silero-vad".to_string(),
            sets: vec!["full".to_string()],
            bytes: (weights.len() + manifest.len()) as u64,
            files: vec![
                catalog_file("manifest.json", &manifest),
                catalog_file("silero_vad.onnx", &weights),
            ],
        }],
    );
    host.set_catalog_for_test(Some(catalog));

    let restarts = Arc::new(AtomicUsize::new(0));
    let restarts_cb = restarts.clone();
    host.set_on_worker_restart_for_test(Some(Arc::new(move |why| {
        assert_eq!(why, "models");
        restarts_cb.fetch_add(1, Ordering::AcqRel);
    })));

    // First download: fetches both files, manifest.json last, restarts worker once.
    let mut events: Vec<ModelsProgress> = Vec::new();
    host.models_download("silero-vad", &mut |p| events.push(p), &CancelToken::none())
        .expect("first download ok");

    assert_eq!(events.first().map(|e| e.phase.as_str()), Some("checking"));
    assert_eq!(events.last().map(|e| e.phase.as_str()), Some("done"));
    assert_eq!(restarts.load(Ordering::Acquire), 1);

    let reqs = server.request_paths();
    assert_eq!(
        reqs,
        vec![
            "/silero-vad/silero_vad.onnx".to_string(),
            "/silero-vad/manifest.json".to_string(),
        ]
    );

    let models_dir = DataPaths::models_dir(&dir.path().join("home"), &Default::default());
    assert_eq!(
        fs::read(models_dir.join("silero-vad").join("silero_vad.onnx")).unwrap(),
        weights
    );
    assert_eq!(
        fs::read(models_dir.join("silero-vad").join("manifest.json")).unwrap(),
        manifest
    );

    // Second download: everything already verified on disk -> 0 files fetched -> NO worker restart.
    let mut events2: Vec<ModelsProgress> = Vec::new();
    host.models_download("silero-vad", &mut |p| events2.push(p), &CancelToken::none())
        .expect("second download ok");
    assert_eq!(events2.last().map(|e| e.phase.as_str()), Some("done"));
    assert_eq!(restarts.load(Ordering::Acquire), 1);

    // Unknown target returns usage error.
    let err = host
        .models_download("nonexistent", &mut |_| {}, &CancelToken::none())
        .unwrap_err();
    match err {
        AgentError::InvalidOperation(msg) => assert_eq!(msg, CommandProcessor::USAGE_MODELS),
        other => panic!("expected InvalidOperation usage error, got {other:?}"),
    }

    // Pre-cancelled token emits cancelled phase and does not restart worker.
    let cts = CancelSource::new();
    cts.cancel();
    let mut events3: Vec<ModelsProgress> = Vec::new();
    host.models_download("silero-vad", &mut |p| events3.push(p), &cts.token())
        .expect("cancelled download returns ok after emitting cancelled");
    assert_eq!(events3.last().map(|e| e.phase.as_str()), Some("cancelled"));
    assert_eq!(restarts.load(Ordering::Acquire), 1);

    host.shutdown();
}

// F2: single-download guard in src\host\models.rs (~line 234) and mid-download cancel.
#[test]
fn models_download_rejects_concurrent_download_with_busy_error() {
    let dir = TempDir::new("models-busy");
    let host = make_test_host(&dir);

    let weights = vec![0xCDu8; 8192];
    let manifest = br#"{"id":"silero-vad","version":1}"#.to_vec();
    let mut files = HashMap::new();
    files.insert("/silero-vad/silero_vad.onnx".to_string(), weights.clone());
    files.insert("/silero-vad/manifest.json".to_string(), manifest.clone());
    let server = LocalHttpServer::start(files);
    server.set_slow("/silero-vad/silero_vad.onnx", 512, Duration::from_millis(2000));

    let catalog = make_test_catalog(
        &server.base_url(),
        vec![Component {
            id: "silero-vad".to_string(),
            sets: vec!["full".to_string()],
            bytes: (weights.len() + manifest.len()) as u64,
            files: vec![
                catalog_file("manifest.json", &manifest),
                catalog_file("silero_vad.onnx", &weights),
            ],
        }],
    );
    host.set_catalog_for_test(Some(catalog));

    let cts = CancelSource::new();
    let tok = cts.token();
    let host_bg = host.clone();
    let first = thread::spawn(move || {
        let mut events = Vec::new();
        let res = host_bg.models_download("silero-vad", &mut |p| events.push(p), &tok);
        (res, events)
    });

    assert!(
        until(
            || server
                .request_paths()
                .iter()
                .any(|p| p == "/silero-vad/silero_vad.onnx"),
            Duration::from_secs(5),
        ),
        "first model download never started fetching"
    );
    assert!(host.is_busy_for_auto_update());

    // Second concurrent download must be refused with ERR_MODELS_BUSY.
    let err = host
        .models_download("silero-vad", &mut |_| {}, &CancelToken::none())
        .expect_err("concurrent models_download must fail");
    match err {
        AgentError::InvalidOperation(msg) => assert_eq!(msg, CommandProcessor::ERR_MODELS_BUSY),
        other => panic!("expected InvalidOperation ERR_MODELS_BUSY, got {other:?}"),
    }

    cts.cancel();
    let (res, events) = first.join().unwrap();
    res.expect("cancelled download returns Ok");
    assert_eq!(events.last().map(|e| e.phase.as_str()), Some("cancelled"));
    assert!(!host.is_busy_for_auto_update());

    host.shutdown();
}

// F2: models_download refuses while an update download is running (src\host\agent_host.rs ~line 2473).
#[test]
fn models_download_refuses_while_update_download_is_running() {
    let dir = TempDir::new("models-vs-upd");
    let host = make_test_host(&dir);

    let sk = SigningKey::random(&mut OsRng);
    let priv_pem = sk.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
    let pub_pem = sk.verifying_key().to_public_key_pem(LineEnding::LF).unwrap();

    let payload = vec![0x5Au8; 16384];
    let manifest = UpdateManifest {
        format: br_integrity::MANIFEST_FORMAT,
        version: "0.3.0".to_string(),
        installer: "Setup.exe".to_string(),
        size: payload.len() as u64,
        sha256: sha256_hex(&payload),
    };
    let (manifest_bytes, sig) = sign_update_manifest(&manifest, &priv_pem).unwrap();

    let server = LocalHttpServer::start(HashMap::new());
    let json_path = format!("/releases/download/v0.3.0/{UPDATE_JSON}");
    let sig_path = format!("/releases/download/v0.3.0/{UPDATE_SIG}");
    let exe_path = "/releases/download/v0.3.0/Setup.exe";
    server.set_file(&json_path, manifest_bytes.clone());
    server.set_file(&sig_path, sig.to_vec());
    server.set_file(exe_path, payload.clone());
    server.set_slow(exe_path, 512, Duration::from_millis(2000));

    let feed = vec![serde_json::json!({
        "tag_name": "v0.3.0",
        "draft": false,
        "prerelease": false,
        "html_url": format!("{}tag/v0.3.0", server.release_prefix()),
        "published_at": "2026-11-22T10:00:00Z",
        "assets": [
            { "name": UPDATE_JSON, "size": manifest_bytes.len(), "browser_download_url": server.asset_url(&json_path) },
            { "name": UPDATE_SIG, "size": sig.len(), "browser_download_url": server.asset_url(&sig_path) },
            { "name": "Setup.exe", "size": payload.len(), "browser_download_url": server.asset_url(exe_path) },
        ],
    })];
    server.set_file("/releases", serde_json::to_vec(&feed).unwrap());

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: None,
    }));
    // If the guard broke, the download must hit this local server, never the built-in catalog.
    host.set_catalog_for_test(Some(make_test_catalog(
        &server.base_url(),
        vec![Component {
            id: "silero-vad".to_string(),
            sets: vec!["full".to_string()],
            bytes: 1,
            files: vec![catalog_file("manifest.json", b"{}")],
        }],
    )));

    let cts = CancelSource::new();
    let tok = cts.token();
    let host_bg = host.clone();
    let upd = thread::spawn(move || host_bg.update_download(&mut |_| {}, &tok));

    assert!(
        until(
            || server.request_paths().iter().any(|p| p == exe_path),
            Duration::from_secs(5),
        ),
        "update download never started fetching installer"
    );

    let err = host
        .models_download("silero-vad", &mut |_| {}, &cancel_after(Duration::from_secs(30)).token())
        .expect_err("models_download must refuse while update download runs");
    match err {
        AgentError::InvalidOperation(msg) => {
            assert_eq!(msg, CommandProcessor::ERR_UPDATE_RUNNING_FOR_MODELS);
        }
        other => panic!("expected InvalidOperation ERR_UPDATE_RUNNING_FOR_MODELS, got {other:?}"),
    }

    cts.cancel();
    upd.join().unwrap().expect("cancelled update returns Ok");
    host.shutdown();
}

#[test]
fn f2_ui_bytes_count_only_incomplete_components() {
    let dir = TempDir::new("f2-ui-bytes");
    let host = make_test_host(&dir);

    let m_a = br#"{"id":"ocr"}"#.to_vec();
    let m_b = br#"{"id":"ner"}"#.to_vec();
    let m_c = br#"{"id":"silero-vad"}"#.to_vec();

    let catalog = make_test_catalog(
        "http://127.0.0.1:1/",
        vec![
            Component {
                id: "ocr".to_string(),
                sets: vec!["minimal".to_string(), "full".to_string()],
                bytes: 100,
                files: vec![catalog_file("manifest.json", &m_a)],
            },
            Component {
                id: "ner".to_string(),
                sets: vec!["minimal".to_string(), "full".to_string()],
                bytes: 200,
                files: vec![catalog_file("manifest.json", &m_b)],
            },
            Component {
                id: "silero-vad".to_string(),
                sets: vec!["full".to_string()],
                bytes: 400,
                files: vec![catalog_file("manifest.json", &m_c)],
            },
        ],
    );
    host.set_catalog_for_test(Some(catalog));

    let models_dir = DataPaths::models_dir(&dir.path().join("home"), &Default::default());
    // Mark "ocr" as complete (both minimal and full).
    fs::create_dir_all(models_dir.join("ocr")).unwrap();
    fs::write(models_dir.join("ocr").join("manifest.json"), &m_a).unwrap();

    let list = host.models_list().expect("models_list ok");
    // "ocr" (100) is complete, so minimal_bytes should be 200 (ner only), and full_bytes should be 200 + 400 = 600.
    assert_eq!(list.minimal_bytes, 200);
    assert_eq!(list.full_bytes, 600);

    host.shutdown();
}

#[test]
fn c7_models_download_rechecks_update_downloading_and_clears_flag() {
    let dir = TempDir::new("c7-models");
    let downloading_flag = std::sync::atomic::AtomicBool::new(false);
    let update_flag = std::sync::atomic::AtomicBool::new(true);

    let catalog = make_test_catalog(
        "http://127.0.0.1:1/",
        vec![Component {
            id: "silero-vad".to_string(),
            sets: vec!["full".to_string()],
            bytes: 10,
            files: vec![catalog_file("manifest.json", b"{}")],
        }],
    );

    let ctx = br_agent::host::DownloadContext {
        models_dir: dir.path(),
        catalog: &catalog,
        fast_retries: true,
        downloading_flag: &downloading_flag,
        update_downloading_flag: Some(&update_flag),
    };

    let err = match br_agent::host::download_models(
        &ctx,
        "silero-vad",
        &mut |_| {},
        &CancelToken::none(),
        || {},
    ) {
        Err(e) => e,
        Ok(()) => panic!("must refuse when update is downloading"),
    };

    match err {
        AgentError::InvalidOperation(msg) => {
            assert_eq!(msg, CommandProcessor::ERR_UPDATE_RUNNING_FOR_MODELS);
        }
        other => panic!("expected InvalidOperation ERR_UPDATE_RUNNING_FOR_MODELS, got {other:?}"),
    }
    // Flag must be cleared (not left set to true)
    assert!(
        !downloading_flag.load(Ordering::Acquire),
        "models_downloading must be cleared when refused due to update download"
    );
}
