// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CommandProcessor tests for UI-only `models` (and shared UI-only / streaming verb tables).

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use br_contracts::{Envelope, ModelComponent, ModelsList, ModelsProgress, UpdateProgress};
use br_core::{AgentError, CancelSource, CancelToken, CommandProcessor, PipeKind};

use common::a3::VerbFixture;

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

/// Covers `models`, `update`, and `bench full` UI-only enforcement, help listing, and long-running detection.
#[test]
fn models_is_ui_only_and_listed_in_ui_help() {
    let f = VerbFixture::new();
    let mut ui_s = VerbFixture::ui();
    let r_help = f.p.handle(&mut ui_s, "help", &CancelToken::none());
    let (ok, data, _) = Envelope::parse(&r_help.line).unwrap();
    assert!(ok);
    let ui_verbs: Vec<String> = serde_json::from_value(data.unwrap()["verbs"].clone()).unwrap();
    let pub_verbs = f.p.help_verbs(PipeKind::Public);
    let mcp_verbs = f.p.help_verbs(PipeKind::Mcp);

    for (verb, sample_cmd, long_running_cmd) in [
        ("models", "models list", "models download minimal"),
        ("update", "update status", "update download"),
        ("bench full", "bench full", "bench full"),
    ] {
        let mut pub_s = VerbFixture::pub_sess();
        let r_pub = f.p.handle(&mut pub_s, sample_cmd, &CancelToken::none());
        let (ok_pub, _, err_pub) = Envelope::parse(&r_pub.line).unwrap();
        assert!(!ok_pub, "public must reject {sample_cmd}");
        if verb != "bench full" {
            assert_eq!(err_pub, Some(format!("Unknown command '{verb}'.")));
        }

        let mut mcp_s = VerbFixture::mcp();
        let r_mcp = f.p.handle(&mut mcp_s, sample_cmd, &CancelToken::none());
        let (ok_mcp, _, err_mcp) = Envelope::parse(&r_mcp.line).unwrap();
        assert!(!ok_mcp, "mcp must reject {sample_cmd}");
        if verb != "bench full" {
            assert_eq!(err_mcp, Some(format!("Unknown command '{verb}'.")));
        }

        assert!(ui_verbs.iter().any(|v| v == verb), "ui help missing {verb}");
        assert!(!pub_verbs.iter().any(|v| v == verb), "public help has {verb}");
        assert!(!mcp_verbs.iter().any(|v| v == verb), "mcp help has {verb}");
        assert!(CommandProcessor::is_long_running(PipeKind::Ui, long_running_cmd));
        assert!(!CommandProcessor::is_long_running(PipeKind::Public, long_running_cmd));
    }
    assert!(!*f.agent.full_bench_started.0.lock().unwrap());
}

#[test]
fn models_list_returns_fake_data_even_when_locked() {
    let f = VerbFixture::new();
    let expected = sample_models_list();
    *f.agent.models_list_reply.lock().unwrap() = Some(expected.clone());
    f.agent.unlocked.store(false, Ordering::Release);

    let mut ui_s = VerbFixture::ui();
    let r = f.p.handle(&mut ui_s, "models list", &CancelToken::none());
    let (ok, data, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "err={err:?}");
    let got: ModelsList = serde_json::from_value(data.unwrap()).unwrap();
    assert_eq!(got, expected);
}

/// Streams progress events for both `models download` and `update download`.
#[test]
fn models_download_streams_progress_and_ends() {
    let f = VerbFixture::new();
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

    let mut ui_s = VerbFixture::ui();
    let r = f
        .p
        .handle(&mut ui_s, "models download minimal", &CancelToken::none());
    let mut lines = Vec::new();
    r.stream.expect("models stream")(&mut |l| lines.push(l));
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

    let r_up = f.p.handle(&mut ui_s, "update download", &CancelToken::none());
    let mut up_lines = Vec::new();
    r_up.stream.expect("update stream")(&mut |l| up_lines.push(l));
    assert_eq!(up_lines.len(), 4);
    let up_parsed: Vec<UpdateProgress> = up_lines
        .iter()
        .map(|l| {
            let (ok, data, _) = Envelope::parse(l).unwrap();
            assert!(ok);
            serde_json::from_value(data.unwrap()).unwrap()
        })
        .collect();
    assert_eq!(up_parsed[0].phase, "checking");
    assert_eq!(up_parsed[1].phase, "downloading");
    assert_eq!(up_parsed[1].done_bytes, 500);
    assert_eq!(up_parsed[2].phase, "verifying");
    assert_eq!(up_parsed[3].phase, "done");
}

#[test]
fn models_download_passes_through_agent_errors() {
    let f = VerbFixture::new();
    let seen_target = Arc::new(Mutex::new(String::new()));
    let seen_c = seen_target.clone();
    *f.agent.on_models_download.lock().unwrap() = Some(Arc::new(move |target, _emit, _ct| {
        *seen_c.lock().unwrap() = target.to_string();
        Err(AgentError::InvalidOperation(
            CommandProcessor::ERR_MODELS_BUSY.to_string(),
        ))
    }));

    let mut ui_s = VerbFixture::ui();
    let r = f
        .p
        .handle(&mut ui_s, "models download minimal", &CancelToken::none());
    let mut out = Vec::new();
    r.stream.expect("stream")(&mut |l| out.push(l));
    assert_eq!(*seen_target.lock().unwrap(), "minimal");
    assert_eq!(out.len(), 1);
    let (ok, _, err) = Envelope::parse(&out[0]).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some(CommandProcessor::ERR_MODELS_BUSY));
}

#[test]
fn models_download_maps_cancelled_and_rejects_bad_usage() {
    let f = VerbFixture::new();
    *f.agent.on_models_download.lock().unwrap() = Some(Arc::new(|target, _emit, ct| {
        if ct.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if target == "bogus" {
            return Err(AgentError::NotFound);
        }
        Ok(())
    }));

    let mut ui_s = VerbFixture::ui();

    // AgentError::Cancelled is mapped by CommandProcessor to a `cancelled` progress line.
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

    // Non-InvalidOperation error maps to ERR_INTERNAL.
    let r_bad = f
        .p
        .handle(&mut ui_s, "models download bogus", &CancelToken::none());
    let mut bad_lines = Vec::new();
    r_bad.stream.expect("stream")(&mut |l| bad_lines.push(l));
    assert_eq!(bad_lines.len(), 1);
    let (ok, _, err) = Envelope::parse(&bad_lines[0]).unwrap();
    assert!(!ok);
    assert_eq!(err.as_deref(), Some(CommandProcessor::ERR_INTERNAL));

    // Missing or extra subcommand -> immediate usage error.
    for cmd in ["models", "models bogus", "models list extra"] {
        let r_empty = f.p.handle(&mut ui_s, cmd, &CancelToken::none());
        assert!(r_empty.stream.is_none(), "cmd={cmd}");
        let (ok, _, err) = Envelope::parse(&r_empty.line).unwrap();
        assert!(!ok, "cmd={cmd}");
        assert_eq!(err.as_deref(), Some(CommandProcessor::USAGE_MODELS), "cmd={cmd}");
    }
}
