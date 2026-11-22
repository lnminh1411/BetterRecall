// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CommandProcessor tests for the UI-only `models` verb (R7, stage I4).

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{Envelope, ModelComponent, ModelsList, ModelsProgress};
use br_core::{
    AgentError, CancelSource, CancelToken, ClientSession, CommandDeps, CommandProcessor, KeyVault,
    PipeKind, SettingsStore,
};

use common::a3::{FakeAgent, FakeEmbedder, FakeThumbs};
use common::{TempHome, TempStore};

struct ModelsFixture {
    _t: TempStore,
    _home: TempHome,
    agent: Arc<FakeAgent>,
    p: CommandProcessor,
}

impl ModelsFixture {
    fn new() -> Self {
        let mut t = TempStore::new();
        let store = Arc::new(t.store.take().unwrap());
        let home = TempHome::new(Some("dpapi"));
        let settings = Arc::new(SettingsStore::load(home.path()));
        let s_for_vault = settings.clone();
        let vault = Arc::new(KeyVault::new(
            home.path(),
            move || s_for_vault.current(),
            None,
            false,
        ));
        let agent = Arc::new(FakeAgent::new());
        let embedder = Arc::new(FakeEmbedder::new());
        let thumbs = Arc::new(FakeThumbs::new());
        let st_clone = store.clone();
        let p = CommandProcessor::new(CommandDeps {
            settings,
            store: Arc::new(move || Some(st_clone.clone())),
            keys: vault,
            agent: agent.clone(),
            embedder,
            thumbnailer: thumbs,
            purge_all: Arc::new(|_| 0),
            purge_all_undeleted: None,
            blobs: None,
            home: Some(home.path().to_path_buf()),
            password: None,
            ai_key: None,
            describer: None,
            redactor: None,
            chat_http: None,
            chat_providers: None,
        });
        Self {
            _t: t,
            _home: home,
            agent,
            p,
        }
    }

    fn ui_sess() -> ClientSession {
        ClientSession::new(PipeKind::Ui, 101, r"C:\app\ui\BetterRecall.UI.exe")
    }

    fn pub_sess() -> ClientSession {
        ClientSession::new(PipeKind::Public, 100, r"C:\tools\client.exe")
    }

    fn mcp_sess() -> ClientSession {
        ClientSession::new(PipeKind::Mcp, 102, r"C:\app\betterrecall-mcp.exe")
    }
}

fn sample_models_list() -> ModelsList {
    ModelsList {
        models_dir: r"K:\scratch\models".to_string(),
        repo: "lnminh1411/BetterRecall-models".to_string(),
        revision: "893eb2c63a11443e9f5061745c69ecbd5e1bd16a".to_string(),
        components: vec![ModelComponent {
            id: "silero-vad".to_string(),
            title: "Silero VAD".to_string(),
            description: "Speech detection for transcription".to_string(),
            sets: vec!["full".to_string()],
            bytes: 2_329_089,
            state: "missing".to_string(),
        }],
        full_bytes: 6_555_959_572,
        minimal_bytes: 3_476_218_217,
        free_bytes: 50_000_000_000,
        downloading: false,
    }
}

#[test]
fn models_is_ui_only_and_listed_in_ui_help() {
    let f = ModelsFixture::new();

    let mut pub_s = ModelsFixture::pub_sess();
    let r_pub = f.p.handle(&mut pub_s, "models list", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r_pub.line).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some("Unknown command 'models'."));

    let mut mcp_s = ModelsFixture::mcp_sess();
    let r_mcp = f.p.handle(&mut mcp_s, "models list", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r_mcp.line).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some("Unknown command 'models'."));

    let mut ui_s = ModelsFixture::ui_sess();
    let r_help = f.p.handle(&mut ui_s, "help", &CancelToken::none());
    let (ok, data, _) = Envelope::parse(&r_help.line).unwrap();
    assert!(ok);
    let verbs: Vec<String> = serde_json::from_value(data.unwrap()["verbs"].clone()).unwrap();
    assert!(verbs.iter().any(|v| v == "models"));

    assert!(!f.p.help_verbs(PipeKind::Public).iter().any(|v| v == "models"));
    assert!(!f.p.help_verbs(PipeKind::Mcp).iter().any(|v| v == "models"));
    assert!(CommandProcessor::is_long_running(
        PipeKind::Ui,
        "models download minimal"
    ));
    assert!(!CommandProcessor::is_long_running(
        PipeKind::Public,
        "models download minimal"
    ));
}

#[test]
fn models_list_returns_fake_data_even_when_locked() {
    let f = ModelsFixture::new();
    let expected = sample_models_list();
    *f.agent.models_list_reply.lock().unwrap() = Some(expected.clone());
    f.agent.unlocked.store(false, Ordering::Release);

    let mut ui_s = ModelsFixture::ui_sess();
    let r = f.p.handle(&mut ui_s, "models list", &CancelToken::none());
    let (ok, data, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "err={err:?}");
    let got: ModelsList = serde_json::from_value(data.unwrap()).unwrap();
    assert_eq!(got, expected);
}

#[test]
fn models_download_streams_progress_and_ends() {
    let f = ModelsFixture::new();
    *f.agent.on_models_download.lock().unwrap() = Some(Arc::new(|target, emit, _ct| {
        assert_eq!(target, "minimal");
        emit(ModelsProgress {
            phase: "checking".to_string(),
            done_bytes: 0,
            total_bytes: 0,
            file: None,
            error: None,
        });
        emit(ModelsProgress {
            phase: "downloading".to_string(),
            done_bytes: 500,
            total_bytes: 1000,
            file: Some("ocr/det.onnx".to_string()),
            error: None,
        });
        emit(ModelsProgress {
            phase: "done".to_string(),
            done_bytes: 1000,
            total_bytes: 1000,
            file: None,
            error: None,
        });
        Ok(())
    }));

    let mut ui_s = ModelsFixture::ui_sess();
    let r = f
        .p
        .handle(&mut ui_s, "models download minimal", &CancelToken::none());
    let producer = r.stream.expect("download returns stream");
    let mut lines = Vec::new();
    producer(&mut |l| lines.push(l));

    assert_eq!(lines.len(), 3);
    let parsed: Vec<ModelsProgress> = lines
        .iter()
        .map(|l| {
            let (ok, data, _) = Envelope::parse(l).unwrap();
            assert!(ok);
            serde_json::from_value(data.unwrap()).unwrap()
        })
        .collect();
    assert_eq!(parsed[0].phase, "checking");
    assert_eq!(parsed[1].phase, "downloading");
    assert_eq!(parsed[1].file.as_deref(), Some("ocr/det.onnx"));
    assert_eq!(parsed[2].phase, "done");
}

#[test]
fn models_download_refuses_second_concurrent_download() {
    let f = ModelsFixture::new();
    let busy = Arc::new(AtomicBool::new(false));
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));

    let busy_c = busy.clone();
    let started_c = started.clone();
    let release_c = release.clone();
    *f.agent.on_models_download.lock().unwrap() = Some(Arc::new(move |_target, emit, _ct| {
        if busy_c
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::ERR_MODELS_BUSY.to_string(),
            ));
        }
        {
            let (lock, cv) = &*started_c;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        {
            let (lock, cv) = &*release_c;
            let g = lock.lock().unwrap();
            let _ = cv.wait_timeout_while(g, Duration::from_secs(5), |done| !*done);
        }
        emit(ModelsProgress {
            phase: "done".to_string(),
            done_bytes: 10,
            total_bytes: 10,
            file: None,
            error: None,
        });
        busy_c.store(false, Ordering::Release);
        Ok(())
    }));

    let mut ui_s1 = ModelsFixture::ui_sess();
    let r1 = f
        .p
        .handle(&mut ui_s1, "models download full", &CancelToken::none());
    let prod1 = r1.stream.expect("first stream");

    let t1 = thread::spawn(move || {
        let mut out = Vec::new();
        prod1(&mut |l| out.push(l));
        out
    });

    {
        let (lock, cv) = &*started;
        let g = lock.lock().unwrap();
        let (g2, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |s| !*s)
            .unwrap();
        assert!(*g2, "first download did not start");
    }

    let mut ui_s2 = ModelsFixture::ui_sess();
    let r2 = f
        .p
        .handle(&mut ui_s2, "models download minimal", &CancelToken::none());
    let prod2 = r2.stream.expect("second stream");
    let mut out2 = Vec::new();
    prod2(&mut |l| out2.push(l));
    assert_eq!(out2.len(), 1);
    let (ok, _, err) = Envelope::parse(&out2[0]).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some(CommandProcessor::ERR_MODELS_BUSY));

    {
        let (lock, cv) = &*release;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    let out1 = t1.join().unwrap();
    assert_eq!(out1.len(), 1);
}

#[test]
fn models_download_cancel_and_unknown_target() {
    let f = ModelsFixture::new();
    *f.agent.on_models_download.lock().unwrap() = Some(Arc::new(|target, emit, ct| {
        if target != "minimal" {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::USAGE_MODELS.to_string(),
            ));
        }
        if ct.is_cancelled() {
            emit(ModelsProgress {
                phase: "cancelled".to_string(),
                done_bytes: 0,
                total_bytes: 100,
                file: None,
                error: None,
            });
            return Ok(());
        }
        Ok(())
    }));

    let mut ui_s = ModelsFixture::ui_sess();

    // Cancelled token -> cancelled phase.
    let cts = CancelSource::new();
    cts.cancel();
    let r = f
        .p
        .handle(&mut ui_s, "models download minimal", &cts.token());
    let mut lines = Vec::new();
    r.stream.expect("stream")(&mut |l| lines.push(l));
    assert_eq!(lines.len(), 1);
    let (ok, data, _) = Envelope::parse(&lines[0]).unwrap();
    assert!(ok);
    let p: ModelsProgress = serde_json::from_value(data.unwrap()).unwrap();
    assert_eq!(p.phase, "cancelled");

    // Unknown target -> usage error.
    let r_bad = f
        .p
        .handle(&mut ui_s, "models download bogus", &CancelToken::none());
    let mut bad_lines = Vec::new();
    r_bad.stream.expect("stream")(&mut |l| bad_lines.push(l));
    assert_eq!(bad_lines.len(), 1);
    let (ok, _, err) = Envelope::parse(&bad_lines[0]).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some(CommandProcessor::USAGE_MODELS));

    // Missing subcommand -> immediate usage error.
    let r_empty = f.p.handle(&mut ui_s, "models", &CancelToken::none());
    assert!(r_empty.stream.is_none());
    let (ok, _, err) = Envelope::parse(&r_empty.line).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some(CommandProcessor::USAGE_MODELS));
}
