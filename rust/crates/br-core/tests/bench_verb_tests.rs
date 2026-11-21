// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported `BenchVerbTests.cs` (12 tests).

mod common;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use br_contracts::{BenchReport, DataPaths, Envelope};
use br_core::{
    BlobCipher, CancelToken, ClientSession, CommandDeps, CommandProcessor, KeyVault, PipeClient,
    PipeKind, PipeServer, SettingsStore,
};
use chrono::{Duration as ChronoDuration, Utc};

use common::a3::{FakeAgent, FakeEmbedder, FakeThumbs, profile_this_pc, test_pipe_name};
use common::{TempHome, TempStore, screen, vec_info, vec_seeded};

struct BenchFixture {
    _t: TempStore,
    store: Arc<br_core::EncryptedStore>,
    home: TempHome,
    settings: Arc<SettingsStore>,
    vault: Arc<KeyVault>,
    agent: Arc<FakeAgent>,
    store_open: Arc<AtomicBool>,
    p: Arc<CommandProcessor>,
}

impl BenchFixture {
    fn new() -> Self {
        let mut t = TempStore::new();
        let store = Arc::new(t.store.take().unwrap());
        let id = store
            .add_screen_capture(&screen(
                Utc::now() - ChronoDuration::minutes(5),
                "chrome.exe",
                "Acme invoice",
                true,
            ))
            .unwrap();
        store.set_embedding(&id, &vec_seeded(1), &vec_info()).unwrap();

        let home = TempHome::new(Some("dpapi"));
        let settings = Arc::new(SettingsStore::load(home.path()));
        let s_for_vault = settings.clone();
        let vault = Arc::new(KeyVault::new(
            home.path(),
            move || s_for_vault.current(),
            None,
            false,
        ));
        assert!(vault.try_load_silent().unwrap());

        let agent = Arc::new(FakeAgent::new());
        *agent.full_report.lock().unwrap() = Some(profile_this_pc());

        let store_open = Arc::new(AtomicBool::new(true));
        let st_clone = store.clone();
        let open_clone = store_open.clone();
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
            embedder: Arc::new(FakeEmbedder::new()),
            thumbnailer: Arc::new(FakeThumbs::new()),
            purge_all: Arc::new(|_| 0),
            purge_all_undeleted: None,
            blobs: Some(Arc::new(BlobCipher::from_vault(vault.clone()))),
            home: Some(home.path().to_path_buf()),
            password: None,
            ai_key: None,
            describer: None,
            redactor: None,
            chat_http: None,
            chat_providers: None,
        }));

        Self {
            _t: t,
            store,
            home,
            settings,
            vault,
            agent,
            store_open,
            p,
        }
    }

    fn ui() -> ClientSession {
        ClientSession::new(PipeKind::Ui, 101, r"C:\app\ui\BetterRecall.UI.exe")
    }

    fn pub_sess() -> ClientSession {
        ClientSession::new(PipeKind::Public, 100, r"C:\tools\client.exe")
    }

    fn mcp() -> ClientSession {
        ClientSession::new(PipeKind::Mcp, 102, r"C:\app\betterrecall-mcp.exe")
    }

    fn call(
        &self,
        s: &mut ClientSession,
        line: &str,
    ) -> (bool, Option<serde_json::Value>, Option<String>) {
        let r = self.p.handle(s, line, &CancelToken::none());
        assert!(!r.line.contains('\n'));
        Envelope::parse(&r.line).unwrap()
    }
}

#[test]
fn bench_full_returns_report_with_search_and_recommendations() {
    let f = BenchFixture::new();
    let mut ui = BenchFixture::ui();
    let reply = f.p.handle(&mut ui, "bench full", &CancelToken::none());
    assert!(!reply.line.contains('\n'));
    let (ok, data, error) = Envelope::parse(&reply.line).unwrap();
    assert!(ok);
    assert!(error.is_none());

    let report: BenchReport = serde_json::from_value(data.unwrap()).unwrap();
    assert!(!report.recommendations.is_empty());
    assert!(
        report
            .recommendations
            .iter()
            .any(|r| r.key == "replay.encoder" && r.value == "\"av1\"")
    );
    let search = report.search.expect("search bench");
    assert_eq!(1, search.rows);
    assert!(search.vector_ms >= 0.0);
    assert!(search.hybrid_ms >= 0.0);
    assert_eq!(profile_this_pc().hardware.cpu, report.hardware.cpu);
    assert!(reply.line.contains("\"ts\":\""));
    let idx = reply.line.find("\"ts\":\"").unwrap() + 6;
    let end = reply.line[idx..].find('"').unwrap() + idx;
    assert!(reply.line[idx..end].ends_with('Z'));
}

#[test]
fn bench_full_skips_search_when_store_locked_or_closed() {
    let f = BenchFixture::new();
    f.store_open.store(false, Ordering::Release);
    let mut ui = BenchFixture::ui();
    let (ok, data, error) = f.call(&mut ui, "bench full");
    assert!(ok);
    assert!(error.is_none());
    let report: BenchReport = serde_json::from_value(data.unwrap()).unwrap();
    assert!(report.search.is_none());
}

#[test]
fn bench_last_before_any_run_is_not_found() {
    let f = BenchFixture::new();
    let mut ui = BenchFixture::ui();
    let (_, _, err) = f.call(&mut ui, "bench last");
    assert_eq!(Some(CommandProcessor::ERR_NOT_FOUND.to_string()), err);
}

#[test]
fn bench_last_returns_saved_report() {
    let f = BenchFixture::new();
    let mut ui = BenchFixture::ui();
    let full = f.call(&mut ui, "bench full");
    assert!(full.0);
    let last = f.call(&mut ui, "bench last");
    assert!(last.0);
    assert_eq!(full.1, last.1);
}

#[test]
fn saved_report_is_encrypted() {
    let f = BenchFixture::new();
    let mut ui = BenchFixture::ui();
    assert!(f.call(&mut ui, "bench full").0);
    let path = DataPaths::bench_report(f.home.path());
    assert!(path.exists());
    let text = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
    assert!(!text.contains("replay.encoder"));
    assert!(!text.contains("Intel(R) Graphics"));
    assert!(!text.contains("R_encoder_realtime"));
}

#[test]
fn bench_last_survives_a_new_processor() {
    let f = BenchFixture::new();
    let mut ui = BenchFixture::ui();
    let full = f.call(&mut ui, "bench full");
    assert!(full.0);

    let st_clone = f.store.clone();
    let p2 = CommandProcessor::new(CommandDeps {
        settings: f.settings.clone(),
        store: Arc::new(move || Some(st_clone.clone())),
        keys: f.vault.clone(),
        agent: f.agent.clone(),
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: Some(Arc::new(BlobCipher::from_vault(f.vault.clone()))),
        home: Some(f.home.path().to_path_buf()),
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    });
    let r = p2.handle(&mut ui, "bench last", &CancelToken::none());
    let (ok, data, error) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    assert!(error.is_none());
    assert_eq!(full.1, data);
}

#[test]
fn bench_bad_argument_gives_usage() {
    let f = BenchFixture::new();
    let mut ui = BenchFixture::ui();
    assert_eq!(
        Some(CommandProcessor::USAGE_BENCH.to_string()),
        f.call(&mut ui, "bench nope").2
    );
    assert_eq!(
        Some(CommandProcessor::USAGE_BENCH.to_string()),
        f.call(&mut ui, "bench full extra").2
    );
}

#[test]
fn bench_full_is_ui_only() {
    let f = BenchFixture::new();
    let mut pub_s = BenchFixture::pub_sess();
    assert!(!f.call(&mut pub_s, "bench full").0);
    assert!(!*f.agent.full_bench_started.0.lock().unwrap());

    let mut mcp_s = BenchFixture::mcp();
    assert!(!f.call(&mut mcp_s, "bench full").0);
    assert!(!*f.agent.full_bench_started.0.lock().unwrap());
}

#[test]
fn help_lists_bench_full_and_last_for_ui() {
    let f = BenchFixture::new();
    let mut ui = BenchFixture::ui();
    let reply = f.p.handle(&mut ui, "help", &CancelToken::none());
    assert!(reply.line.contains("\"bench full\""));
    assert!(reply.line.contains("\"bench last\""));
}

#[test]
fn long_running_detection() {
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "bench full"));
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "BENCH  FULL"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Ui, "bench"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Ui, "bench last"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Public, "bench full"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Mcp, "bench full"));
}

#[test]
fn closing_the_pipe_cancels_bench_full() {
    let f = BenchFixture::new();
    f.agent.block_full_bench.store(true, Ordering::Release);
    let name = test_pipe_name("benchui");
    let p_srv = f.p.clone();
    let server = PipeServer::new(
        &name,
        PipeKind::Ui,
        |_| true,
        move |conn, ct| p_srv.serve(conn, &ct),
        None,
    );
    server.start().unwrap();

    let stream = PipeClient::connect_raw(&name, 2000).unwrap();
    stream
        .write_line("bench full", None, &CancelToken::none())
        .unwrap();
    assert!(FakeAgent::wait_flag(
        &f.agent.full_bench_started,
        Duration::from_secs(10)
    ));

    stream.close();
    drop(stream);
    assert!(FakeAgent::wait_flag(
        &f.agent.full_bench_cancelled,
        Duration::from_secs(10)
    ));
    server.stop();
}

#[test]
fn bench_full_is_not_cut_by_the_request_timeout() {
    let f = BenchFixture::new();
    f.agent.block_full_bench.store(true, Ordering::Release);
    let name = test_pipe_name("benchui2");
    let p_srv = f.p.clone();
    let server = PipeServer::new(
        &name,
        PipeKind::Ui,
        |_| true,
        move |conn, ct| p_srv.serve(conn, &ct),
        None,
    );
    server.start().unwrap();

    let stream = PipeClient::connect_raw(&name, 2000).unwrap();
    stream
        .write_line("bench full", None, &CancelToken::none())
        .unwrap();
    assert!(FakeAgent::wait_flag(
        &f.agent.full_bench_started,
        Duration::from_secs(10)
    ));

    thread::sleep(Duration::from_millis(1500));
    assert!(!*f.agent.full_bench_cancelled.0.lock().unwrap());

    stream.close();
    drop(stream);
    assert!(FakeAgent::wait_flag(
        &f.agent.full_bench_cancelled,
        Duration::from_secs(10)
    ));
    server.stop();
}
