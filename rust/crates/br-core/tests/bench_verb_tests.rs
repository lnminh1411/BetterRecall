// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CommandProcessor tests for the UI-only `bench` verb.

mod common;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use br_contracts::{BenchReport, DataPaths, Envelope};
use br_core::{
    BlobCipher, CancelToken, CommandDeps, CommandProcessor, PipeClient, PipeKind, PipeServer,
};

use common::a3::{FakeAgent, FakeEmbedder, FakeThumbs, VerbFixture, profile_this_pc, test_pipe_name};

#[test]
fn bench_full_returns_report_with_search_and_recommendations() {
    let f = VerbFixture::new();
    let mut ui = VerbFixture::ui();
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
    let f = VerbFixture::new();
    f.store_open.store(false, Ordering::Release);
    let mut ui = VerbFixture::ui();
    let (ok, data, error) = f.call(&mut ui, "bench full");
    assert!(ok);
    assert!(error.is_none());
    let report: BenchReport = serde_json::from_value(data.unwrap()).unwrap();
    assert!(report.search.is_none());
}

#[test]
fn bench_last_before_any_run_is_not_found() {
    let f = VerbFixture::new();
    let mut ui = VerbFixture::ui();
    let (_, _, err) = f.call(&mut ui, "bench last");
    assert_eq!(Some(CommandProcessor::ERR_NOT_FOUND.to_string()), err);
}

#[test]
fn bench_last_returns_saved_report() {
    let f = VerbFixture::new();
    let mut ui = VerbFixture::ui();
    let full = f.call(&mut ui, "bench full");
    assert!(full.0);
    let last = f.call(&mut ui, "bench last");
    assert!(last.0);
    assert_eq!(full.1, last.1);
}

#[test]
fn saved_report_is_encrypted() {
    let f = VerbFixture::new();
    let mut ui = VerbFixture::ui();
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
    let f = VerbFixture::new();
    let mut ui = VerbFixture::ui();
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
    let f = VerbFixture::new();
    let mut ui = VerbFixture::ui();
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
fn long_running_detection() {
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "bench full"));
    assert!(CommandProcessor::is_long_running(PipeKind::Ui, "BENCH  FULL"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Ui, "bench"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Ui, "bench last"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Public, "bench full"));
    assert!(!CommandProcessor::is_long_running(PipeKind::Mcp, "bench full"));
}

#[test]
fn bench_full_is_not_cut_by_request_timeout_and_cancels_when_pipe_closes() {
    let f = VerbFixture::new();
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

    // Confirm bench stays running (not cancelled immediately or by any request timeout).
    thread::sleep(Duration::from_millis(300));
    assert!(!*f.agent.full_bench_cancelled.0.lock().unwrap());

    stream.close();
    drop(stream);
    assert!(FakeAgent::wait_flag(
        &f.agent.full_bench_cancelled,
        Duration::from_secs(10)
    ));
    server.stop();
}
