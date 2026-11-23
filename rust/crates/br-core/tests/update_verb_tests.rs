// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CommandProcessor tests for the UI-only `update` verb (R8, stage I5).

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use br_contracts::{Envelope, UpdateProgress, UpdateStatus};
use br_core::{AgentError, CancelSource, CancelToken, CommandProcessor};

use common::a3::VerbFixture;

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
fn update_status_check_and_apply_replies_even_when_locked() {
    let f = VerbFixture::new();
    let status_expected = sample_update_status("idle");
    *f.agent.update_status_reply.lock().unwrap() = Some(status_expected.clone());
    let check_expected = sample_update_status("idle");
    let check_c = check_expected.clone();
    *f.agent.on_update_check.lock().unwrap() = Some(Arc::new(move |_ct| Ok(check_c.clone())));
    let apply_expected = sample_update_status("applying");
    let apply_c = apply_expected.clone();
    *f.agent.on_update_apply.lock().unwrap() = Some(Arc::new(move || Ok(apply_c.clone())));
    f.agent.unlocked.store(false, Ordering::Release);

    let mut ui_s = VerbFixture::ui();

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
fn update_download_passes_through_agent_errors() {
    let f = VerbFixture::new();
    let mut ui_s = VerbFixture::ui();

    for expected_err in [
        CommandProcessor::ERR_UPDATE_BUSY,
        CommandProcessor::ERR_MODELS_RUNNING_FOR_UPDATE,
    ] {
        *f.agent.on_update_download.lock().unwrap() = Some(Arc::new(move |_emit, _ct| {
            Err(AgentError::InvalidOperation(expected_err.to_string()))
        }));
        let r = f.p.handle(&mut ui_s, "update download", &CancelToken::none());
        let mut out = Vec::new();
        r.stream.expect("stream")(&mut |l| out.push(l));
        assert_eq!(out.len(), 1);
        let (ok, _, err) = Envelope::parse(&out[0]).unwrap();
        assert!(!ok);
        assert_eq!(err.as_deref(), Some(expected_err));
    }
}

#[test]
fn update_unknown_subverb_and_cancel_behaviour() {
    let f = VerbFixture::new();
    *f.agent.on_update_download.lock().unwrap() = Some(Arc::new(|_emit, ct| {
        if ct.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        Ok(())
    }));

    let mut ui_s = VerbFixture::ui();

    // Missing or unknown sub-verb gives usage.
    for cmd in ["update", "update bogus", "update status extra"] {
        let r = f.p.handle(&mut ui_s, cmd, &CancelToken::none());
        assert!(r.stream.is_none());
        let (ok, _, err) = Envelope::parse(&r.line).unwrap();
        assert!(!ok, "cmd={cmd}");
        assert_eq!(err.as_deref(), Some(CommandProcessor::USAGE_UPDATE));
    }

    // AgentError::Cancelled is mapped by CommandProcessor to a `cancelled` progress line.
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
