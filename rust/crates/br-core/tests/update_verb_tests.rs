// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CommandProcessor tests for the UI-only `update` verb (R8, stage I5).

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{Envelope, ModelsProgress, UpdateProgress, UpdateStatus};
use br_core::{
    AgentError, CancelSource, CancelToken, ClientSession, CommandDeps, CommandProcessor, KeyVault,
    PipeKind, SettingsStore,
};

use common::a3::{FakeAgent, FakeEmbedder, FakeThumbs};
use common::{TempHome, TempStore};

struct UpdateFixture {
    _t: TempStore,
    _home: TempHome,
    agent: Arc<FakeAgent>,
    p: CommandProcessor,
}

impl UpdateFixture {
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

fn sample_update_status(state: &str) -> UpdateStatus {
    UpdateStatus {
        current_version: "0.3.0-dev".to_string(),
        latest_version: Some("0.3.0".to_string()),
        available: true,
        can_install: true,
        reason: None,
        notes_url: Some(
            "https://github.com/lnminh1411/BetterRecall/releases/tag/v0.3.0".to_string(),
        ),
        published_at: None,
        installer_bytes: Some(125_829_120),
        state: state.to_string(),
        downloaded_version: None,
        last_check: None,
        auto: false,
        error: None,
    }
}

#[test]
fn update_is_ui_only_and_listed_in_ui_help() {
    let f = UpdateFixture::new();

    let mut pub_s = UpdateFixture::pub_sess();
    let r_pub = f.p.handle(&mut pub_s, "update status", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r_pub.line).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some("Unknown command 'update'."));

    let mut mcp_s = UpdateFixture::mcp_sess();
    let r_mcp = f.p.handle(&mut mcp_s, "update status", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r_mcp.line).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some("Unknown command 'update'."));

    let mut ui_s = UpdateFixture::ui_sess();
    let r_help = f.p.handle(&mut ui_s, "help", &CancelToken::none());
    let (ok, data, _) = Envelope::parse(&r_help.line).unwrap();
    assert!(ok);
    let verbs: Vec<String> = serde_json::from_value(data.unwrap()["verbs"].clone()).unwrap();
    assert!(verbs.iter().any(|v| v == "update"));

    assert!(!f.p.help_verbs(PipeKind::Public).iter().any(|v| v == "update"));
    assert!(!f.p.help_verbs(PipeKind::Mcp).iter().any(|v| v == "update"));
    assert!(CommandProcessor::is_long_running(
        PipeKind::Ui,
        "update download"
    ));
    assert!(!CommandProcessor::is_long_running(
        PipeKind::Public,
        "update download"
    ));
}

#[test]
fn update_status_check_and_apply_replies_even_when_locked() {
    let f = UpdateFixture::new();
    let status_expected = sample_update_status("idle");
    *f.agent.update_status_reply.lock().unwrap() = Some(status_expected.clone());
    let check_expected = sample_update_status("idle");
    let check_c = check_expected.clone();
    *f.agent.on_update_check.lock().unwrap() = Some(Arc::new(move |_ct| Ok(check_c.clone())));
    let apply_expected = sample_update_status("applying");
    let apply_c = apply_expected.clone();
    *f.agent.on_update_apply.lock().unwrap() = Some(Arc::new(move || Ok(apply_c.clone())));
    f.agent.unlocked.store(false, Ordering::Release);

    let mut ui_s = UpdateFixture::ui_sess();

    let r_status = f.p.handle(&mut ui_s, "update status", &CancelToken::none());
    let (ok, data, err) = Envelope::parse(&r_status.line).unwrap();
    assert!(ok, "err={err:?}");
    let got: UpdateStatus = serde_json::from_value(data.unwrap()).unwrap();
    assert_eq!(got, status_expected);

    let r_check = f.p.handle(&mut ui_s, "update check", &CancelToken::none());
    let (ok, data, err) = Envelope::parse(&r_check.line).unwrap();
    assert!(ok, "err={err:?}");
    let got: UpdateStatus = serde_json::from_value(data.unwrap()).unwrap();
    assert_eq!(got, check_expected);

    let r_apply = f.p.handle(&mut ui_s, "update apply", &CancelToken::none());
    let (ok, data, err) = Envelope::parse(&r_apply.line).unwrap();
    assert!(ok, "err={err:?}");
    let got: UpdateStatus = serde_json::from_value(data.unwrap()).unwrap();
    assert_eq!(got, apply_expected);
}

#[test]
fn update_download_streams_progress_and_ends() {
    let f = UpdateFixture::new();
    *f.agent.on_update_download.lock().unwrap() = Some(Arc::new(|emit, _ct| {
        emit(UpdateProgress {
            phase: "checking".to_string(),
            done_bytes: 0,
            total_bytes: 0,
            error: None,
        });
        emit(UpdateProgress {
            phase: "downloading".to_string(),
            done_bytes: 500,
            total_bytes: 1000,
            error: None,
        });
        emit(UpdateProgress {
            phase: "verifying".to_string(),
            done_bytes: 1000,
            total_bytes: 1000,
            error: None,
        });
        emit(UpdateProgress {
            phase: "done".to_string(),
            done_bytes: 1000,
            total_bytes: 1000,
            error: None,
        });
        Ok(())
    }));

    let mut ui_s = UpdateFixture::ui_sess();
    let r = f
        .p
        .handle(&mut ui_s, "update download", &CancelToken::none());
    let producer = r.stream.expect("download returns stream");
    let mut lines = Vec::new();
    producer(&mut |l| lines.push(l));

    assert_eq!(lines.len(), 4);
    let parsed: Vec<UpdateProgress> = lines
        .iter()
        .map(|l| {
            let (ok, data, _) = Envelope::parse(l).unwrap();
            assert!(ok);
            serde_json::from_value(data.unwrap()).unwrap()
        })
        .collect();
    assert_eq!(parsed[0].phase, "checking");
    assert_eq!(parsed[1].phase, "downloading");
    assert_eq!(parsed[1].done_bytes, 500);
    assert_eq!(parsed[2].phase, "verifying");
    assert_eq!(parsed[3].phase, "done");
}

#[test]
fn update_download_refuses_concurrent_and_mutually_excludes_models() {
    let f = UpdateFixture::new();
    let update_busy = Arc::new(AtomicBool::new(false));
    let models_busy = Arc::new(AtomicBool::new(false));
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));

    let ub1 = update_busy.clone();
    let mb1 = models_busy.clone();
    let started_c = started.clone();
    let release_c = release.clone();
    *f.agent.on_update_download.lock().unwrap() = Some(Arc::new(move |emit, _ct| {
        if mb1.load(Ordering::Acquire) {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::ERR_MODELS_RUNNING_FOR_UPDATE.to_string(),
            ));
        }
        if ub1
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::ERR_UPDATE_BUSY.to_string(),
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
        emit(UpdateProgress {
            phase: "done".to_string(),
            done_bytes: 10,
            total_bytes: 10,
            error: None,
        });
        ub1.store(false, Ordering::Release);
        Ok(())
    }));

    let ub2 = update_busy.clone();
    *f.agent.on_models_download.lock().unwrap() = Some(Arc::new(move |_target, emit, _ct| {
        if ub2.load(Ordering::Acquire) {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::ERR_UPDATE_RUNNING_FOR_MODELS.to_string(),
            ));
        }
        emit(ModelsProgress {
            phase: "done".to_string(),
            done_bytes: 10,
            total_bytes: 10,
            file: None,
            error: None,
        });
        Ok(())
    }));

    let mut ui_s1 = UpdateFixture::ui_sess();
    let r1 = f
        .p
        .handle(&mut ui_s1, "update download", &CancelToken::none());
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
        assert!(*g2, "first update download did not start");
    }

    // Second concurrent update download is refused.
    let mut ui_s2 = UpdateFixture::ui_sess();
    let r2 = f
        .p
        .handle(&mut ui_s2, "update download", &CancelToken::none());
    let mut out2 = Vec::new();
    r2.stream.expect("second stream")(&mut |l| out2.push(l));
    assert_eq!(out2.len(), 1);
    let (ok, _, err) = Envelope::parse(&out2[0]).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some(CommandProcessor::ERR_UPDATE_BUSY));

    // Models download while update download runs is refused.
    let mut ui_s3 = UpdateFixture::ui_sess();
    let r3 = f
        .p
        .handle(&mut ui_s3, "models download minimal", &CancelToken::none());
    let mut out3 = Vec::new();
    r3.stream.expect("models stream")(&mut |l| out3.push(l));
    assert_eq!(out3.len(), 1);
    let (ok, _, err) = Envelope::parse(&out3[0]).unwrap();
    assert!(!ok);
    assert_eq!(
        err.as_deref(),
        Some(CommandProcessor::ERR_UPDATE_RUNNING_FOR_MODELS)
    );

    {
        let (lock, cv) = &*release;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    let out1 = t1.join().unwrap();
    assert_eq!(out1.len(), 1);
}

#[test]
fn update_unknown_subverb_and_cancel_behaviour() {
    let f = UpdateFixture::new();
    *f.agent.on_update_download.lock().unwrap() = Some(Arc::new(|emit, ct| {
        if ct.is_cancelled() {
            emit(UpdateProgress {
                phase: "cancelled".to_string(),
                done_bytes: 0,
                total_bytes: 100,
                error: None,
            });
            return Ok(());
        }
        Ok(())
    }));

    let mut ui_s = UpdateFixture::ui_sess();

    // Missing or unknown sub-verb gives usage.
    for cmd in ["update", "update bogus", "update status extra"] {
        let r = f.p.handle(&mut ui_s, cmd, &CancelToken::none());
        assert!(r.stream.is_none());
        let (ok, _, err) = Envelope::parse(&r.line).unwrap();
        assert!(!ok, "cmd={cmd}");
        assert_eq!(err.as_deref(), Some(CommandProcessor::USAGE_UPDATE));
    }

    // Cancelled token emits cancelled phase.
    let cts = CancelSource::new();
    cts.cancel();
    let r = f.p.handle(&mut ui_s, "update download", &cts.token());
    let mut lines = Vec::new();
    r.stream.expect("stream")(&mut |l| lines.push(l));
    assert_eq!(lines.len(), 1);
    let (ok, data, _) = Envelope::parse(&lines[0]).unwrap();
    assert!(ok);
    let p: UpdateProgress = serde_json::from_value(data.unwrap()).unwrap();
    assert_eq!(p.phase, "cancelled");
}
