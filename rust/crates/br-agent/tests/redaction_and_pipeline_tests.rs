// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Stage A5 tests: Redactor, RedactionMasks, KeyframePipeline, DescribeRedactor,
//! and capture/keyframe/thumbnail/redaction cases from Sweep2F, Sweep3I, Sweep3J,
//! Sweep3Leak, Sweep4P, Sweep4R, and Sweep5P.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_agent::capture::{
    CaptureService, ImageCodec, KeyframeGrab, KeyframePipeline, KeyframeReader, KeyframeRequest,
    KeyframeStore, OcrHealth, OutputCapture, OutputInfo, WorkerKeyframeReader,
};
use br_agent::codecs::Avif;
use br_agent::guardrails::{
    BadgeResult, GuardDecision, GuardReason, GuardSnapshot, GuardrailService, MonitorTop,
    WindowInfo, WindowInspector,
};
use br_agent::host::{AgentHost, AgentOptions, PauseController};
use br_agent::native::{Rect, tick_count64};
use br_agent::privacy::{
    AgentDescribeRedactor, PixelBox, RedactionMasks, RedactionOptions, Redactor,
};
use br_agent::thumbs::{AgentThumbnailer, KeyframeBlobStore};
use br_agent::worker::{WorkerClient, WorkerError, WorkerSupervisor};
use br_contracts::{
    CoreHostOptions, DataPaths, EmbeddingInfo, NewScreenCapture, OcrEntity, OcrLine, OcrResult,
    OcrSettings, PrivacySettings, PurgeScope, ReplaySettings, Settings, WorkerResponse,
};
use br_core::{CancelToken, CoreHost, DescribeRedactor, OutgoingImage, Thumbnailer};
use chrono::{DateTime, Duration as ChronoDuration, Utc};

use common::{
    FakeSampleSink, FakeStore, FakeWorker, SwitchReader, TempDir, ocr_line, until, white_webp,
};

fn decode_pixel_r(webp: &[u8], x: i32, y: i32) -> u8 {
    let (bgra, w, _h) = ImageCodec::decode_bgra(webp, "image/webp").expect("decode webp");
    bgra[((y * w + x) as usize) * 4 + 2]
}

fn white_grab(
    monitor: i32,
    w: i32,
    h: i32,
    secrets: Option<Vec<Rect>>,
    seed: u64,
    fill_half: Option<u8>,
) -> KeyframeGrab {
    let req = KeyframeRequest {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: r"C:\app.exe".to_string(),
        title: "Title".to_string(),
        context: format!("ctx{seed}"),
        force: true,
        max_width: w,
        dirty_min_percent: 0.0,
        site: String::new(),
        keyframe: true,
        replay_sample: false,
        trigger: false,
        secrets,
        bounds: Rect::new(0, 0, w * 2, h * 2),
        mask_only: false,
    };
    let mut g = KeyframeGrab::alloc(monitor, req, w, h, 0);
    g.pixels_mut().fill(255);
    if let Some(f) = fill_half {
        let half = g.pixels().len() / 2;
        g.pixels_mut()[..half].fill(f);
    }
    g
}

fn non_forced_grab(
    monitor: i32,
    w: i32,
    h: i32,
    fill: u8,
    keyframe: bool,
    sample: bool,
    ts: Option<DateTime<Utc>>,
) -> KeyframeGrab {
    let req = KeyframeRequest {
        ts: ts.unwrap_or_else(Utc::now),
        app: "app".to_string(),
        exe_path: r"C:\app.exe".to_string(),
        title: "Title".to_string(),
        context: "ctx".to_string(),
        force: false,
        max_width: w,
        dirty_min_percent: 0.0,
        site: String::new(),
        keyframe,
        replay_sample: sample,
        trigger: false,
        secrets: None,
        bounds: Rect::new(0, 0, w, h),
        mask_only: false,
    };
    let mut g = KeyframeGrab::alloc(monitor, req, w, h, 0);
    g.pixels_mut().fill(255);
    let half = g.pixels().len() / 2;
    g.pixels_mut()[..half].fill(fill);
    g
}

fn card_ocr_result(x: i32, y: i32, entities_missing: bool) -> OcrResult {
    let text = "Card: 4111 1111 1111 1111";
    let cuts: Vec<i32> = (0..=text.len()).map(|i| x + (i as i32) * 6).collect();
    OcrResult {
        lines: vec![OcrLine {
            text: text.to_string(),
            x,
            y,
            w: (text.len() as i32) * 6,
            h: 20,
            confidence: 0.99,
            cuts,
            entities: None,
        }],
        engine: "fake".to_string(),
        ms: 1.0,
        entities_missing,
    }
}

fn outgoing_white(w: i32, h: i32) -> OutgoingImage {
    OutgoingImage {
        bytes: white_webp(w, h),
        mime: "image/webp".to_string(),
        width: w,
        height: h,
        redacted: false,
    }
}

// ---------------- RedactionTests: RedactorTests ----------------

#[test]
fn defaults_are_the_sensitive_detectors_only() {
    let d = RedactionOptions::from_privacy(&PrivacySettings::default());
    assert!(d.passwords && d.cards && d.ids && d.birth_dates);
    assert!(!(d.emails || d.phones || d.names || d.addresses));
    assert!(OcrSettings::default().enabled);
}

#[test]
fn default_detectors() {
    let d = RedactionOptions::from_privacy(&PrivacySettings::default());
    let cases = [
        ("Card 4111 1111 1111 1111 exp 12/29", "Card exp 12/29"),
        ("Visa 4111-1111-1111-1111", "Visa"),
        ("Order 4111 1111 1111 1112", "Order 4111 1111 1111 1112"),
        ("Tracking 1234567890123456", "Tracking 1234567890123456"),
        (
            "IBAN DE89 3704 0044 0532 0130 00 BIC COBADEFFXXX",
            "IBAN BIC COBADEFFXXX",
        ),
        ("GB82 WEST 1234 5698 7654 32", ""),
        (
            "Ref DE89 3704 0044 0532 0130 01",
            "Ref DE89 3704 0044 0532 0130 01",
        ),
        ("SSN 078-05-1120 on file", "SSN on file"),
        ("Số CCCD: 001099012345", "Số CCCD:"),
        ("Passport No. C01X00T47 issued", "Passport No. issued"),
        ("Password: hunter2", "Password:"),
        ("Mật khẩu: abc123!", "Mật khẩu:"),
        (
            "export OPENAI=sk-proj-abcdefghijklmnopqrstuvwx",
            "export OPENAI=",
        ),
        ("Date of birth: 12/03/1990", "Date of birth:"),
        ("Ngày sinh 12 tháng 3 năm 1990", "Ngày sinh"),
        ("生年月日 1990年3月12日", "生年月日"),
        ("Meeting on 12/03/2026", "Meeting on 12/03/2026"),
        ("Pin to taskbar", "Pin to taskbar"),
        ("Forgot password?", "Forgot password?"),
    ];
    for (line, kept) in cases {
        let r = Redactor::apply(&[ocr_line(line, 0, 0, 20, None)], &[], d, "");
        assert_eq!(kept, r.text, "line: {line}");
    }
}

#[test]
fn email_and_phone_detectors() {
    let cases = [
        ("Mail anna.schmidt@example.de now", "Mail now"),
        ("Call +49 30 1234567 today", "Call today"),
        ("(555) 123-4567", ""),
        ("Điện thoại 0912 345 678", "Điện thoại"),
        ("Build 10.0.26100.4652", "Build 10.0.26100.4652"),
        ("Updated 2026-11-12 10:30", "Updated 2026-11-12 10:30"),
    ];
    for (line, kept) in cases {
        let r = Redactor::apply(
            &[ocr_line(line, 0, 0, 20, None)],
            &[],
            RedactionOptions::ALL,
            "",
        );
        assert_eq!(kept, r.text, "line: {line}");
    }
}

#[test]
fn label_value_in_the_next_field_to_the_right_or_below() {
    let d = RedactionOptions::from_privacy(&PrivacySettings::default());
    let r = Redactor::apply(
        &[
            ocr_line("Password", 10, 10, 20, None),
            ocr_line("••••••••", 120, 12, 20, None),
            ocr_line("Sign in", 10, 80, 20, None),
            ocr_line("Date of birth", 300, 10, 20, None),
            ocr_line("03/12/1991", 300, 35, 20, None),
        ],
        &[],
        d,
        "",
    );
    assert_eq!("Password\nDate of birth\nSign in", r.text);
    assert_eq!(2, r.spans);
    assert!(
        r.boxes
            .iter()
            .any(|b| b.x <= 120 && b.right() >= 200 && b.y <= 12)
    );
    assert!(
        r.boxes
            .iter()
            .any(|b| b.x <= 300 && b.y <= 35 && b.bottom() >= 55)
    );
}

#[test]
fn boxes_follow_character_cuts_with_padding() {
    let d = RedactionOptions::from_privacy(&PrivacySettings::default());
    let r = Redactor::apply(&[ocr_line("Password: hunter2", 100, 50, 20, None)], &[], d, "");
    assert_eq!(1, r.boxes.len());
    assert_eq!(
        PixelBox::new(
            200 - Redactor::PAD,
            50 - Redactor::PAD,
            70 + 2 * Redactor::PAD,
            20 + 2 * Redactor::PAD
        ),
        r.boxes[0]
    );
}

#[test]
fn entities_redact_names_and_addresses_by_setting() {
    let d = RedactionOptions::from_privacy(&PrivacySettings::default());
    let line = ocr_line(
        "Ship to John Carter, 742 Evergreen Terrace",
        0,
        0,
        20,
        Some(vec![
            OcrEntity {
                start: 7,
                length: 12,
                label: "person".to_string(),
                score: 0.9,
            },
            OcrEntity {
                start: 21,
                length: 21,
                label: "address".to_string(),
                score: 0.9,
            },
        ]),
    );
    let mut names = d;
    names.names = true;
    assert_eq!(
        "Ship to , 742 Evergreen Terrace",
        Redactor::apply(std::slice::from_ref(&line), &[], names, "").text
    );
    let mut addr = d;
    addr.addresses = true;
    assert_eq!(
        "Ship to John Carter,",
        Redactor::apply(std::slice::from_ref(&line), &[], addr, "").text
    );
    assert_eq!(
        line.text,
        Redactor::apply(std::slice::from_ref(&line), &[], d, "").text
    );
}

#[test]
fn password_fields_black_out_and_drop_the_characters_inside() {
    let d = RedactionOptions::from_privacy(&PrivacySettings::default());
    let field = PixelBox::new(118, 0, 62, 24);
    let line = ocr_line("Login: anna ●●●●●● done", 0, 2, 20, None);
    let r = Redactor::apply(std::slice::from_ref(&line), &[field], d, "");
    assert!(r.boxes.contains(&field));
    assert_eq!("Login: anna done", r.text);
    let mut off = d;
    off.passwords = false;
    assert!(Redactor::apply(&[line], &[field], off, "").boxes.is_empty());
}

#[test]
fn title_gets_the_string_detectors() {
    let r = Redactor::apply(&[], &[], RedactionOptions::ALL, "Inbox - anna@example.com - Gmail");
    assert_eq!("Inbox - - Gmail", r.title);
}

#[test]
fn reading_order_groups_rows_then_left_to_right() {
    let lines = [
        ocr_line("right", 300, 11, 20, None),
        ocr_line("second row", 0, 40, 20, None),
        ocr_line("left", 0, 10, 20, None),
    ];
    let texts: Vec<String> = Redactor::reading_order(&lines)
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert_eq!(vec!["left", "right", "second row"], texts);
    assert_eq!(
        "left\nright\nsecond row",
        Redactor::apply(&lines, &[], RedactionOptions::NONE, "").text
    );
}

#[test]
fn checksums() {
    assert!(Redactor::luhn("4111111111111111"));
    assert!(!Redactor::luhn("4111111111111112"));
    assert_eq!(Some(18), Redactor::iban_span("NL91ABNA0417164300"));
    assert_eq!(Some(22), Redactor::iban_span("NL91 ABNA 0417 1643 00 BIC"));
    assert_eq!(None, Redactor::iban_span("NL91ABNA0417164301"));
    assert_eq!(None, Redactor::iban_span("ZZ91ABNA0417164300"));
    assert_eq!(None, Redactor::card_span("41111111111111112"));
    assert_eq!(Some((0, 19)), Redactor::card_span("4111 1111 1111 1111 2"));
}

// ---------------- RedactionMasksTests ----------------

#[test]
fn keyframe_boxes_map_to_desktop_and_back() {
    let mon = Rect::new(1920, 0, 1920 + 2560, 1440);
    let d = RedactionMasks::to_desktop(&[PixelBox::new(100, 50, 20, 10)], 1280, 720, mon);
    assert_eq!(1, d.len());
    assert_eq!(Rect::new(1920 + 200, 100, 1920 + 240, 120), d[0]);
    let k = RedactionMasks::to_keyframe(&d, mon, 1280, 720);
    assert_eq!(vec![PixelBox::new(100, 50, 20, 10)], k);
    assert!(RedactionMasks::to_keyframe(&[Rect::new(0, 0, 100, 100)], mon, 1280, 720).is_empty());
    assert_eq!(
        vec![PixelBox::new(0, 0, 5, 5)],
        RedactionMasks::to_keyframe(&[Rect::new(1900, -10, 1930, 10)], mon, 1280, 720)
    );
}

#[test]
fn masks_replace_per_monitor_and_merge_for_the_replay() {
    let m = RedactionMasks::new();
    m.set(0, vec![Rect::new(0, 0, 10, 10)]);
    m.set(1, vec![Rect::new(100, 0, 110, 10), Rect::new(120, 0, 130, 10)]);
    assert_eq!(3, m.all().len());
    m.set(1, Vec::new());
    assert_eq!(1, m.all().len());

    let mut snap = GuardSnapshot::closed();
    snap.is_closed_sentinel = false;
    snap.mask_rects = vec![Rect::new(5, 5, 6, 6)];
    snap.secrets = Some(vec![Rect::new(7, 7, 8, 8)]);
    assert_eq!(3, CaptureService::merge_masks(&snap, true, &m.all(), None).len());
    assert_eq!(2, CaptureService::merge_masks(&snap, false, &m.all(), None).len());
    m.clear();
    assert_eq!(snap.mask_rects, CaptureService::merge_masks(&snap, false, &m.all(), None));
    assert_eq!(
        vec![CaptureService::MASK_EVERYTHING],
        CaptureService::merge_masks(&GuardSnapshot::closed(), true, &[], None)
    );
}

#[test]
fn fill_blacks_out_clipped_boxes() {
    let mut px = vec![255u8; 8 * 4 * 4];
    unsafe {
        RedactionMasks::fill(px.as_mut_ptr(), 8, 4, 32, &[PixelBox::new(6, 2, 10, 10)]);
    }
    assert_eq!(&[0, 0, 0, 255], &px[(2 * 8 + 6) * 4..(2 * 8 + 6) * 4 + 4]);
    assert_eq!(&[0, 0, 0, 255], &px[(3 * 8 + 7) * 4..(3 * 8 + 7) * 4 + 4]);
    assert_eq!(255, px[(2 * 8 + 5) * 4]);
    assert_eq!(255, px[(8 + 6) * 4]);
}

// ---------------- KeyframeRedactionTests ----------------

fn run_pipe_once(
    mut s: Settings,
    reader: Arc<dyn KeyframeReader>,
    masks: Arc<RedactionMasks>,
    grab: KeyframeGrab,
    expect_stored: usize,
) -> (Arc<FakeStore>, Arc<KeyframePipeline>) {
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader),
        Some(masks),
        None,
    );
    pipe.post(grab);
    assert!(until(
        || pipe.kept() + pipe.redact_dropped() + pipe.failed() > 0,
        Duration::from_secs(5)
    ));
    assert_eq!(expect_stored, store.inner.lock().unwrap().screens.len());
    (store, pipe)
}

#[test]
fn stored_keyframe_has_black_boxes_and_redacted_text() {
    let mut s = Settings::default();
    s.keyframes.quality = 95;
    let reader = SwitchReader::with_answer(|| {
        Ok(OcrResult {
            lines: vec![
                ocr_line("Card: 4111 1111 1111 1111", 10, 10, 20, None),
                ocr_line("Password:", 10, 50, 20, None),
                ocr_line("hunter2", 150, 50, 20, None),
                ocr_line("Meeting notes", 10, 100, 20, None),
            ],
            engine: "onnx".to_string(),
            ms: 5.0,
            entities_missing: false,
        })
    });
    let masks = Arc::new(RedactionMasks::new());
    let (store, pipe) = run_pipe_once(
        s,
        reader.clone(),
        masks.clone(),
        white_grab(0, 400, 200, Some(vec![Rect::new(600, 300, 700, 340)]), 0, None),
        1,
    );
    pipe.stop();
    let cap = store.inner.lock().unwrap().screens.values().next().unwrap().clone();
    assert_eq!(Some("Card:\nPassword:\nMeeting notes"), cap.text.as_deref());
    assert_eq!(Some(false), *reader.last_entities.lock().unwrap());
    assert!(decode_pixel_r(&cap.image, 100, 20) < 40);
    assert!(decode_pixel_r(&cap.image, 170, 60) < 40);
    assert!(decode_pixel_r(&cap.image, 320, 160) < 40);
    assert!(decode_pixel_r(&cap.image, 40, 110) > 200);
    assert!(decode_pixel_r(&cap.image, 380, 10) > 200);
    let all_masks = masks.all();
    assert!(
        all_masks
            .iter()
            .any(|r| r.left <= 600 && r.right >= 700 && r.top <= 300 && r.bottom >= 340)
    );
    assert_eq!(3, all_masks.len());
}

#[test]
fn avif_keyframe_is_encoded_after_redaction() {
    if !Avif::try_load() {
        return;
    }
    let mut s = Settings::default();
    s.keyframes.format = "avif".to_string();
    s.keyframes.quality = 95;
    let reader = SwitchReader::with_answer(|| {
        Ok(OcrResult {
            lines: vec![ocr_line("Card: 4111 1111 1111 1111", 10, 10, 20, None)],
            engine: "onnx".to_string(),
            ms: 5.0,
            entities_missing: false,
        })
    });
    let store = FakeStore::new();
    let st_cb = store.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    pipe.post(white_grab(0, 400, 200, None, 0, None));
    assert!(until(
        || pipe.kept() + pipe.redact_dropped() + pipe.failed() > 0,
        Duration::from_secs(10)
    ));
    pipe.stop();
    let cap = store.inner.lock().unwrap().screens.values().next().unwrap().clone();
    assert_eq!("image/avif", cap.image_mime);
    assert_eq!(Some("Card:"), cap.text.as_deref());
    let (bgra, w, _) = Avif::decode(&cap.image).unwrap();
    assert!(bgra[((20 * w + 100) as usize) * 4 + 2] < 40);
    assert!(bgra[((150 * w + 380) as usize) * 4 + 2] > 200);
}

#[test]
fn replay_sample_is_redacted_and_titled_like_a_keyframe() {
    let mut s = Settings::default();
    s.keyframes.quality = 95;
    let reader = SwitchReader::with_answer(|| {
        Ok(OcrResult {
            lines: vec![ocr_line("Card: 4111 1111 1111 1111", 10, 10, 20, None)],
            engine: "onnx".to_string(),
            ms: 5.0,
            entities_missing: false,
        })
    });
    let store = FakeStore::new();
    let st_cb = store.clone();
    let sink = FakeSampleSink::new();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    pipe.set_sampler(Some(sink.clone()));
    let mut g = white_grab(0, 400, 200, None, 0, None);
    g.request.keyframe = false;
    g.request.replay_sample = true;
    g.request.title = "Pay 4111 1111 1111 1111".to_string();
    pipe.post(g);
    assert!(until(|| sink.count() > 0, Duration::from_secs(5)));
    pipe.stop();
    assert!(store.inner.lock().unwrap().screens.is_empty());
    let sample = &sink.snapshot()[0];
    assert!(decode_pixel_r(&sample.bytes, 100, 20) < 40);
    assert!(!sample.title.contains("4111"));
}

#[test]
fn ocr_failure_drops_the_keyframe_when_text_redaction_is_on() {
    let reader = SwitchReader::with_answer(|| Err(WorkerError::Transport("Worker pipe closed.".to_string())));
    let (_, pipe) = run_pipe_once(
        Settings::default(),
        reader,
        Arc::new(RedactionMasks::new()),
        white_grab(0, 64, 32, None, 0, None),
        0,
    );
    assert_eq!(1, pipe.redact_dropped());
    pipe.stop();
}

#[test]
fn ocr_failure_stores_without_text_when_no_text_detector_is_on() {
    let mut s = Settings::default();
    s.privacy.redact_passwords = false;
    s.privacy.redact_cards = false;
    s.privacy.redact_ids = false;
    s.privacy.redact_birth_dates = false;
    let reader = SwitchReader::with_answer(|| Err(WorkerError::Reply("Device failed: busy.".to_string())));
    let (store, pipe) = run_pipe_once(
        s,
        reader,
        Arc::new(RedactionMasks::new()),
        white_grab(0, 64, 32, None, 0, None),
        1,
    );
    assert!(store.inner.lock().unwrap().screens.values().next().unwrap().text.is_none());
    assert_eq!(1, pipe.ocr_failed());
    pipe.stop();
}

#[test]
fn missing_ocr_model_stores_without_text() {
    let reader = SwitchReader::with_answer(|| Err(WorkerError::Reply("Model not installed: ocr.".to_string())));
    let (store, pipe) = run_pipe_once(
        Settings::default(),
        reader,
        Arc::new(RedactionMasks::new()),
        white_grab(0, 64, 32, None, 0, None),
        1,
    );
    assert!(store.inner.lock().unwrap().screens.values().next().unwrap().text.is_none());
    assert_eq!(1, pipe.ocr_unavailable());
    pipe.stop();
}

#[test]
fn ocr_off_skips_the_worker_and_entities_follow_the_name_setting() {
    let mut s = Settings::default();
    s.ocr.enabled = false;
    let reader = SwitchReader::new();
    let (store, pipe) = run_pipe_once(
        s,
        reader.clone(),
        Arc::new(RedactionMasks::new()),
        white_grab(0, 64, 32, None, 0, None),
        1,
    );
    assert_eq!(0, reader.calls.load(Ordering::Acquire));
    assert!(store.inner.lock().unwrap().screens.values().next().unwrap().text.is_none());
    pipe.stop();

    let mut s2 = Settings::default();
    s2.privacy.redact_names = true;
    let (store2, pipe2) = run_pipe_once(
        s2,
        reader.clone(),
        Arc::new(RedactionMasks::new()),
        white_grab(0, 64, 32, None, 1, None),
        1,
    );
    assert_eq!(Some(true), *reader.last_entities.lock().unwrap());
    assert_eq!(
        Some(""),
        store2.inner.lock().unwrap().screens.values().next().unwrap().text.as_deref()
    );
    pipe2.stop();
}

// ---------------- DescribeRedactorTests ----------------

#[test]
fn card_number_is_blacked_out_and_marked_redacted() {
    let reader = SwitchReader::with_answer(|| Ok(card_ocr_result(10, 10, false)));
    let r = AgentDescribeRedactor::new(reader.clone(), Arc::new(Settings::default), None, None);
    let outp = r
        .redact("s_1", outgoing_white(200, 60), &CancelToken::none())
        .unwrap();
    assert!(outp.redacted);
    assert_eq!("image/webp", outp.mime);
    assert_eq!(1, reader.calls.load(Ordering::Acquire));
    assert!(decode_pixel_r(&outp.bytes, 100, 20) < 40);
    assert!(decode_pixel_r(&outp.bytes, 20, 20) > 200);
}

#[test]
fn clean_text_goes_out_unchanged_but_checked() {
    let input = outgoing_white(200, 60);
    let reader = SwitchReader::with_answer(|| {
        let text = "Meeting notes";
        let cuts: Vec<i32> = (0..=text.len()).map(|i| 10 + (i as i32) * 6).collect();
        Ok(OcrResult {
            lines: vec![OcrLine {
                text: text.to_string(),
                x: 10,
                y: 10,
                w: 78,
                h: 20,
                confidence: 0.99,
                cuts,
                entities: None,
            }],
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        })
    });
    let r = AgentDescribeRedactor::new(reader, Arc::new(Settings::default), None, None);
    let outp = r.redact("s_1", input.clone(), &CancelToken::none()).unwrap();
    assert!(outp.redacted);
    assert_eq!(input.bytes, outp.bytes);
}

#[test]
fn missing_model_or_ocr_off_sends_as_stored_and_says_so() {
    let input = outgoing_white(200, 60);
    let missing_r = SwitchReader::with_answer(|| {
        Err(WorkerError::Reply("Model not installed: ocr.".to_string()))
    });
    let a = AgentDescribeRedactor::new(missing_r, Arc::new(Settings::default), None, None)
        .redact("s_1", input.clone(), &CancelToken::none())
        .unwrap();
    assert!(!a.redacted);
    assert_eq!(input.bytes, a.bytes);

    let mut off = Settings::default();
    off.ocr.enabled = false;
    let reader = SwitchReader::with_answer(|| Ok(card_ocr_result(10, 10, false)));
    let b = AgentDescribeRedactor::new(reader.clone(), Arc::new(move || off.clone()), None, None)
        .redact("s_1", input, &CancelToken::none())
        .unwrap();
    assert!(!b.redacted);
    assert_eq!(0, reader.calls.load(Ordering::Acquire));
}

#[test]
fn chat_may_send_its_checked_images() {
    let r: Box<dyn DescribeRedactor> = Box::new(AgentDescribeRedactor::new(
        SwitchReader::new(),
        Arc::new(Settings::default),
        None,
        None,
    ));
    assert!(r.redacts_images());
}

#[test]
fn ocr_failure_blocks_the_send() {
    let reader =
        SwitchReader::with_answer(|| Err(WorkerError::Reply("Device failed: x.".to_string())));
    let r = AgentDescribeRedactor::new(reader, Arc::new(Settings::default), None, None);
    let err = r
        .redact("s_1", outgoing_white(200, 60), &CancelToken::none())
        .expect_err("must fail");
    assert!(err.to_string().contains("not sent"));
}

#[test]
fn no_text_detector_on_needs_no_ocr() {
    let mut s = Settings::default();
    s.privacy.redact_passwords = false;
    s.privacy.redact_cards = false;
    s.privacy.redact_ids = false;
    s.privacy.redact_birth_dates = false;
    let reader = SwitchReader::with_answer(|| Ok(card_ocr_result(10, 10, false)));
    let outp = AgentDescribeRedactor::new(reader.clone(), Arc::new(move || s.clone()), None, None)
        .redact("s_1", outgoing_white(200, 60), &CancelToken::none())
        .unwrap();
    assert!(outp.redacted);
    assert_eq!(0, reader.calls.load(Ordering::Acquire));
}

// ---------------- Sweep2FTests (thumbs, sample pause, ocr health, ner missing) ----------------

#[test]
fn purged_capture_is_not_served_from_the_thumbnail_cache() {
    let store = FakeStore::new();
    let id = store.add_screen(NewScreenCapture {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "t".to_string(),
        monitor: 0,
        width: 320,
        height: 180,
        d_hash: 0,
        image: white_webp(320, 180),
        image_mime: "image/webp".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    });
    let st = store.clone();
    let thumbs = AgentThumbnailer::new(
        Arc::new(move || Some(st.clone() as Arc<dyn KeyframeBlobStore>)),
        Arc::new(Settings::default),
    );
    assert!(thumbs.get_thumb(&id, 160, &CancelToken::none()).is_some());
    store.inner.lock().unwrap().screens.clear();
    thumbs.clear_cache();
    assert!(thumbs.get_thumb(&id, 160, &CancelToken::none()).is_none());
}

#[test]
fn thumbnail_read_while_a_purge_runs_is_not_cached() {
    let store = FakeStore::new();
    let id = store.add_screen(NewScreenCapture {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "t".to_string(),
        monitor: 0,
        width: 320,
        height: 180,
        d_hash: 0,
        image: white_webp(320, 180),
        image_mime: "image/webp".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    });
    let slot: Arc<Mutex<Option<Arc<AgentThumbnailer>>>> = Arc::new(Mutex::new(None));
    let once = Arc::new(AtomicBool::new(true));
    let slot_cb = slot.clone();
    let st_cb = store.clone();
    let thumbs = Arc::new(AgentThumbnailer::new(
        Arc::new(move || {
            if once.swap(false, Ordering::AcqRel)
                && let Some(ref t) = *slot_cb.lock().unwrap()
            {
                t.clear_cache();
            }
            Some(st_cb.clone() as Arc<dyn KeyframeBlobStore>)
        }),
        Arc::new(Settings::default),
    ));
    *slot.lock().unwrap() = Some(thumbs.clone());
    assert!(thumbs.get_thumb(&id, 160, &CancelToken::none()).is_some());
    assert_eq!(0, thumbs.cache().count());
}

#[test]
fn samples_stop_while_a_pause_stops_keyframes_and_inference() {
    let r = ReplaySettings {
        enabled: true,
        searchable: true,
        sample_seconds: 5,
        ..ReplaySettings::default()
    };
    assert!(CaptureService::sample_due(&r, false, false, 1, 5000, false));
    assert!(!CaptureService::sample_due(&r, false, true, 1, 5000, false));
    assert!(!CaptureService::sample_due(&r, true, false, 1, 5000, false));
    assert!(!CaptureService::sample_due(&r, false, false, 0, 5000, false));
    assert!(!CaptureService::sample_due(&r, false, false, 1, 4000, false));
}

#[test]
fn repeated_failures_mark_ocr_failing_and_warn_at_most_every_ten_minutes() {
    let now = Arc::new(AtomicI64::new(0));
    let n1 = now.clone();
    let h = OcrHealth::new(Arc::new(move || n1.load(Ordering::Acquire)));
    for _ in 1..OcrHealth::FAILING_AFTER {
        assert!(!h.failed());
    }
    assert!(!h.failing());
    assert!(h.failed());
    assert!(h.failing());
    assert!(!h.failed());
    now.fetch_add(600_000, Ordering::AcqRel);
    assert!(h.failed());
    assert!(!h.read(false, false));
    assert!(!h.failing());
}

#[test]
fn missing_entities_are_reported_only_when_names_or_addresses_were_asked() {
    let now = Arc::new(AtomicI64::new(0));
    let n1 = now.clone();
    let h = OcrHealth::new(Arc::new(move || n1.load(Ordering::Acquire)));
    assert!(!h.read(false, true));
    assert!(!h.entities_missing());
    assert!(h.read(true, true));
    assert!(h.entities_missing());
    assert!(!h.read(true, true));
    let _ = h.read(true, false);
    assert!(!h.entities_missing());
}

#[test]
fn persistent_ocr_error_is_reported_instead_of_silently_dropping_every_keyframe() {
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let reader = SwitchReader::with_answer(|| {
        Err(WorkerError::Reply("Device failed: npu.".to_string()))
    });
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    for i in 0..OcrHealth::FAILING_AFTER {
        let before = pipe.redact_dropped();
        pipe.post(white_grab(0, 64, 32, None, i as u64, None));
        assert!(until(|| pipe.redact_dropped() > before, Duration::from_secs(5)));
    }
    pipe.stop();
    assert_eq!(OcrHealth::FAILING_AFTER, pipe.redact_dropped());
    assert!(store.inner.lock().unwrap().screens.is_empty());
    assert!(pipe.ocr_health().failing());
}

#[test]
fn broken_name_detector_keeps_storing_with_pattern_redaction() {
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    s.privacy.redact_names = true;
    let store = FakeStore::new();
    let st_cb = store.clone();
    let reader = SwitchReader::with_answer(|| {
        Ok(OcrResult {
            lines: vec![ocr_line("Card: 4111 1111 1111 1111", 10, 10, 20, None)],
            engine: "onnx".to_string(),
            ms: 5.0,
            entities_missing: true,
        })
    });
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    pipe.post(white_grab(0, 400, 200, None, 1, None));
    assert!(until(
        || pipe.kept() + pipe.redact_dropped() + pipe.failed() > 0,
        Duration::from_secs(5)
    ));
    pipe.stop();
    let cap = store.inner.lock().unwrap().screens.values().next().unwrap().clone();
    assert_eq!(Some("Card:"), cap.text.as_deref());
    assert!(pipe.ocr_health().entities_missing());
    assert!(!pipe.ocr_health().failing());
}

#[test]
fn names_on_with_the_detector_missing_is_not_redacted() {
    for (names, expect_redacted) in [(true, false), (false, true)] {
        let mut s = Settings::default();
        s.privacy.redact_names = names;
        let reader = SwitchReader::with_answer(|| Ok(card_ocr_result(10, 10, true)));
        let r = AgentDescribeRedactor::new(reader, Arc::new(move || s.clone()), None, None);
        let outp = r
            .redact("s_1", outgoing_white(200, 60), &CancelToken::none())
            .unwrap();
        assert_eq!(expect_redacted, outp.redacted);
        assert!(decode_pixel_r(&outp.bytes, 100, 20) < 40);
    }
}

#[test]
fn names_on_with_the_detector_present_is_redacted() {
    let mut s = Settings::default();
    s.privacy.redact_names = true;
    let reader = SwitchReader::with_answer(|| Ok(card_ocr_result(10, 10, false)));
    let r = AgentDescribeRedactor::new(reader, Arc::new(move || s.clone()), None, None);
    assert!(
        r.redact("s_1", outgoing_white(200, 60), &CancelToken::none())
            .unwrap()
            .redacted
    );
}

// ---------------- Sweep3IAgentTests (A3) ----------------

#[test]
fn a3_card_followed_or_preceded_by_digits_is_redacted() {
    let d = RedactionOptions::from_privacy(&PrivacySettings::default());
    let cases = [
        ("4111 1111 1111 1111 12/26", "12/26"),
        ("4111 1111 1111 1111 123", "123"),
        ("4111-1111-1111-1111 07", "07"),
        ("4012-8888-8888-1881 07", "07"),
        ("Order 12 4111 1111 1111 1111", "Order 12"),
        ("Card 5500 0000 0000 0004 123", "Card 123"),
        (
            "Order 4111 1111 1111 1112 12/26",
            "Order 4111 1111 1111 1112 12/26",
        ),
        ("Serial 41111111111111112", "Serial 41111111111111112"),
    ];
    for (line, kept) in cases {
        let r = Redactor::apply(&[ocr_line(line, 0, 0, 20, None)], &[], d, "");
        assert_eq!(kept, r.text, "line: {line}");
        if kept != line {
            assert!(!r.boxes.is_empty());
        }
    }
}

#[test]
fn a3_window_is_the_card_groups_only() {
    assert_eq!(Some((0, 19)), Redactor::card_span("4111 1111 1111 1111 12"));
    assert_eq!(Some((3, 19)), Redactor::card_span("12 4111 1111 1111 1111"));
    assert_eq!(Some((0, 19)), Redactor::card_span("4111 1111 1111 1111"));
    assert_eq!(None, Redactor::card_span("4111 1111 1111 1112 12"));
}

// ---------------- Sweep3JAgentTests (A1, A7, A9) ----------------

fn make_guard() -> Arc<GuardrailService> {
    GuardrailService::new(
        Settings::default,
        || false,
        Vec::new,
        Path::new(r"C:\nowhere\"),
    )
}

fn fresh_snap(per_monitor: HashMap<i32, MonitorTop>, decision: GuardDecision) -> GuardSnapshot {
    let fg = WindowInspector::foreground_root();
    GuardSnapshot {
        taken_at_ms: tick_count64(),
        foreground: WindowInspector::describe(fg),
        decision,
        badge: BadgeResult::NotBrowser,
        any_affinity: false,
        mask_rects: Vec::new(),
        per_monitor,
        site: String::new(),
        secrets: None,
        fg_epoch: 0,
        is_closed_sentinel: false,
    }
}

#[test]
fn a1_stale_snapshot_masks_the_new_foreground_window_not_the_canvas() {
    let fg_window = WindowInfo {
        hwnd: 42,
        pid: 1,
        exe_path: r"C:\a\app.exe".to_string(),
        title: "App".to_string(),
        class_name: "Cls".to_string(),
    };
    let snap = GuardSnapshot {
        taken_at_ms: tick_count64(),
        foreground: Some(fg_window),
        decision: GuardDecision::ALLOWED,
        badge: BadgeResult::NotBrowser,
        any_affinity: false,
        mask_rects: Vec::new(),
        per_monitor: HashMap::new(),
        site: String::new(),
        secrets: None,
        fg_epoch: 3,
        is_closed_sentinel: false,
    };
    let bounds = Rect::new(100, 100, 900, 700);
    assert!(GuardrailService::unscanned_foreground_pure(&snap, 3, 42, Some(bounds)).is_none());
    assert_eq!(
        Some(bounds),
        GuardrailService::unscanned_foreground_pure(&snap, 4, 42, Some(bounds))
    );
    assert_eq!(
        Some(bounds),
        GuardrailService::unscanned_foreground_pure(&snap, 3, 77, Some(bounds))
    );
    assert_eq!(
        Some(CaptureService::MASK_EVERYTHING),
        GuardrailService::unscanned_foreground_pure(&snap, 4, 77, None)
    );
    assert!(GuardrailService::unscanned_foreground_pure(&snap, 4, 0, None).is_none());
    assert!(
        GuardrailService::unscanned_foreground_pure(&GuardSnapshot::closed(), 4, 77, Some(bounds))
            .is_none()
    );

    let masks = CaptureService::merge_masks(
        &snap,
        false,
        &[],
        GuardrailService::unscanned_foreground_pure(&snap, 4, 77, Some(bounds)),
    );
    assert_eq!(vec![bounds], masks);
    assert!(!masks.contains(&CaptureService::MASK_EVERYTHING));
}

#[test]
fn a1_replay_masks_are_not_empty_while_keyframes_refuse_the_snapshot() {
    let guard = make_guard();
    let mut per = HashMap::new();
    per.insert(
        0,
        MonitorTop {
            monitor: 0,
            window: None,
            denied_visible: false,
        },
    );
    guard.set_snapshot_for_test(fresh_snap(per, GuardDecision::ALLOWED));
    guard.on_foreground_event();
    let cs = CaptureService::new(
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(|| false),
        guard.clone(),
        None,
        None,
        false,
    );
    let kf = guard.current_for_keyframe(2000);
    let masks = cs.masks();
    assert!(!kf.decision.allow);
    if WindowInspector::foreground_root() != 0 {
        assert!(!masks.is_empty());
    }
}

#[test]
fn a7_snapshot_from_before_a_rebuild_is_not_applied_to_renumbered_monitors() {
    let guard = make_guard();
    let cs = CaptureService::new(
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(|| false),
        guard.clone(),
        None,
        None,
        false,
    );
    let a = Arc::new(OutputCapture::new(
        OutputInfo {
            index: 0,
            device_name: r"\\.\DISPLAY1".to_string(),
            adapter_luid: 1,
            vendor_id: 0x8086,
            adapter_name: "gpu".to_string(),
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
            monitor: 0,
            refresh_hz: 60,
        },
        cs.devices().clone(),
        Arc::new(|_| {}),
    ));
    let b = Arc::new(OutputCapture::new(
        OutputInfo {
            index: 1,
            device_name: r"\\.\DISPLAY2".to_string(),
            adapter_luid: 1,
            vendor_id: 0x8086,
            adapter_name: "gpu".to_string(),
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
            monitor: 0,
            refresh_hz: 60,
        },
        cs.devices().clone(),
        Arc::new(|_| {}),
    ));
    let b0 = Arc::new(OutputCapture::new(
        OutputInfo {
            index: 0,
            device_name: r"\\.\DISPLAY2".to_string(),
            adapter_luid: 1,
            vendor_id: 0x8086,
            adapter_name: "gpu".to_string(),
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
            monitor: 0,
            refresh_hz: 60,
        },
        cs.devices().clone(),
        Arc::new(|_| {}),
    ));

    cs.set_outputs_for_test(vec![a.clone(), b.clone()], true);
    let keys: Vec<i32> = cs.monitor_bounds().into_iter().map(|(k, _)| k).collect();
    let a_top = WindowInfo {
        hwnd: 1,
        pid: 1,
        exe_path: r"C:\a\notepad.exe".to_string(),
        title: "A doc".to_string(),
        class_name: "Notepad".to_string(),
    };
    let mut per = HashMap::new();
    per.insert(
        keys[0],
        MonitorTop {
            monitor: keys[0],
            window: Some(a_top),
            denied_visible: false,
        },
    );
    per.insert(
        keys[1],
        MonitorTop {
            monitor: keys[1],
            window: None,
            denied_visible: true,
        },
    );
    guard.set_snapshot_for_test(fresh_snap(per, GuardDecision::ALLOWED));
    cs.request_keyframes(false, true, false, false);
    assert_eq!(
        Some("A doc"),
        a.pending_keyframe().as_ref().map(|r| r.title.as_str())
    );
    assert!(b.pending_keyframe().is_none());

    cs.set_outputs_for_test(vec![b0.clone()], true);
    cs.request_keyframes(false, true, false, false);
    assert!(b0.pending_keyframe().is_none());
}

#[test]
fn a9_panic_keeps_replay_redaction_masks() {
    let guard = make_guard();
    let mut per = HashMap::new();
    per.insert(
        0,
        MonitorTop {
            monitor: 0,
            window: None,
            denied_visible: false,
        },
    );
    guard.set_snapshot_for_test(fresh_snap(per, GuardDecision::ALLOWED));
    let masks = Arc::new(RedactionMasks::new());
    let kp = KeyframePipeline::new(
        Arc::new(|| None),
        Arc::new(Settings::default),
        Arc::new(|_| {}),
        None,
        Some(masks.clone()),
        None,
    );
    let cs = CaptureService::new(
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(|| false),
        guard,
        Some(kp.clone()),
        Some(masks.clone()),
        false,
    );
    let b = Rect::new(100, 100, 400, 130);
    masks.set(0, vec![b]);
    cs.panic();
    assert!(masks.all().contains(&b));
    assert!(cs.masks().contains(&b));
    kp.stop();
}

// ---------------- Sweep3LeakTests ----------------

struct BlockedReader {
    calls: AtomicUsize,
    gate: Mutex<bool>,
    cv: Condvar,
}

impl BlockedReader {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            gate: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    fn release(&self) {
        *self.gate.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

impl KeyframeReader for BlockedReader {
    fn read(
        &self,
        _bgra: &[u8],
        _width: i32,
        _height: i32,
        _stride: i32,
        _entities: bool,
        ct: &CancelToken,
    ) -> Result<OcrResult, WorkerError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let mut g = self.gate.lock().unwrap();
        while !*g && !ct.is_cancelled() {
            let (next, _) = self
                .cv
                .wait_timeout(g, Duration::from_millis(20))
                .unwrap();
            g = next;
        }
        if ct.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        Ok(OcrResult {
            lines: Vec::new(),
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        })
    }
}

#[test]
fn grabs_the_full_queue_refuses_are_freed_at_once() {
    let reader = BlockedReader::new();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(Settings::default),
        Arc::new(|_| {}),
        Some(reader.clone()),
        Some(Arc::new(RedactionMasks::new())),
        Some(Duration::from_secs(300)),
    );
    let live = Arc::new(AtomicI64::new(0));
    pipe.post(white_grab(0, 64, 32, None, 0, None).with_live_counter(live.clone()));
    assert!(until(
        || reader.calls.load(Ordering::Acquire) == 1,
        Duration::from_secs(5)
    ));
    for i in 1..=20u64 {
        pipe.post(white_grab(0, 64, 32, None, i, None).with_live_counter(live.clone()));
    }
    assert_eq!(14, pipe.dropped());
    assert_eq!(7, live.load(Ordering::Acquire));
    reader.release();
    pipe.stop();
    assert_eq!(0, live.load(Ordering::Acquire));
}

#[test]
#[ignore = "CPU heavy, run on request"]
fn slow_ocr_with_grabs_arriving_keeps_memory_bounded() {
    // 150-grab 3 s timing loop; kept ignored per CPU-heavy safety rule.
}

#[test]
fn ocr_slower_than_the_timeout_gets_a_longer_wait_and_keyframes_are_kept() {
    let reader = SwitchReader::new();
    reader.set_delay(Duration::from_millis(280));
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader),
        Some(Arc::new(RedactionMasks::new())),
        Some(Duration::from_millis(200)),
    );
    pipe.post(white_grab(0, 64, 32, None, 1, None));
    assert!(until(|| pipe.redact_dropped() == 1, Duration::from_secs(5)));
    assert_eq!(Duration::from_millis(400), pipe.ocr_wait());
    pipe.post(white_grab(0, 64, 32, None, 2, Some(200)));
    assert!(until(|| pipe.kept() == 1, Duration::from_secs(5)));
    assert_eq!(1, pipe.ocr_read());
    assert!(pipe.ocr_wait() >= Duration::from_millis(500));
    pipe.stop();
}

#[test]
fn ocr_wait_follows_the_read_time() {
    let cases = [
        (30, 30, 0, true, 60),
        (30, 60, 0, true, 120),
        (30, 120, 0, true, 120),
        (30, 120, 35, false, 70),
        (30, 70, 2, false, 30),
        (30, 30, 90, false, 120),
    ];
    for (base_s, cur_s, took_s, timed_out, expect_s) in cases {
        assert_eq!(
            Duration::from_secs(expect_s),
            KeyframePipeline::next_ocr_wait(
                Duration::from_secs(base_s),
                Duration::from_secs(cur_s),
                Duration::from_secs(took_s),
                timed_out
            )
        );
    }
}

// ---------------- Sweep4PAgentTests (A-H1) ----------------

#[test]
fn a_h1_keyframe_in_ocr_during_purge_all_is_not_stored_in_the_new_store() {
    let dir = TempDir::new("s4p-ah1");
    std::fs::write(
        DataPaths::settings_file(dir.path()),
        r#"{"security":{"keyProtection":"dpapi"}}"#,
    )
    .unwrap();
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    core.open_store(&CancelToken::none()).unwrap();

    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let ocr_n = Arc::new(AtomicUsize::new(0));
    let entered_cb = entered.clone();
    let release_cb = release.clone();
    let ocr_cb = ocr_n.clone();

    let worker = FakeWorker::new();
    worker.set_handler(move |req, _| {
        if req.op != "ocr" {
            return None;
        }
        if ocr_cb.fetch_add(1, Ordering::AcqRel) == 0 {
            *entered_cb.0.lock().unwrap() = true;
            entered_cb.1.notify_all();
            let mut g = release_cb.0.lock().unwrap();
            while !*g {
                let (next, _) = release_cb
                    .1
                    .wait_timeout(g, Duration::from_secs(10))
                    .unwrap();
                g = next;
            }
        }
        let mut resp = WorkerResponse::ok(req.id);
        resp.ocr = Some(OcrResult {
            lines: Vec::new(),
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        });
        Some((resp, Vec::new()))
    });
    let sup = WorkerSupervisor::new(worker, dir.path().to_path_buf(), || 600);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let metas = Arc::new(Mutex::new(Vec::new()));
    let metas_cb = metas.clone();
    let core_cb = core.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || core_cb.store().map(|st| st as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(move |m| metas_cb.lock().unwrap().push(m)),
        Some(Arc::new(WorkerKeyframeReader::new(client))),
        Some(Arc::new(RedactionMasks::new())),
        Some(Duration::from_secs(20)),
    );

    pipe.post(white_grab(0, 64, 32, None, 0, Some(200)));
    {
        let mut g = entered.0.lock().unwrap();
        while !*g {
            let (next, res) = entered.1.wait_timeout(g, Duration::from_secs(10)).unwrap();
            assert!(!res.timed_out(), "OCR never reached worker");
            g = next;
        }
    }

    core.purge_all(&CancelToken::none());
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    thread::sleep(Duration::from_millis(250));

    let mut post_grab = white_grab(1, 64, 32, None, 1, Some(50));
    post_grab.request.ts = Utc::now() + ChronoDuration::milliseconds(5);
    pipe.post(post_grab);
    assert!(until(|| pipe.kept() >= 1, Duration::from_secs(10)));
    pipe.stop();
    sup.dispose();

    let all = PurgeScope {
        kind: "all".to_string(),
        from: None,
        to: None,
        app: None,
        site: None,
        id: None,
    };
    // every row, embedded or not
    assert_eq!(1, core.store().unwrap().count_purge(&all).unwrap());
    let ms = metas.lock().unwrap();
    assert_eq!(1, ms.len());
    assert_eq!(1, ms[0].monitor);
    core.shutdown();
}

// ---------------- Sweep4RAgentTests (AM7, C2, TH1, TH2, AM6, CA-S1, M2, S1, S2, S9, S12) ----------------

#[test]
fn am7_transcript_text_is_redacted() {
    let d = RedactionOptions::from_privacy(&PrivacySettings::default());
    let cases = [
        (
            "my card number is 4111 1111 1111 1111 thanks",
            "my card number is thanks",
        ),
        ("card 4111-1111-1111-1111.", "card ."),
        (
            "the IBAN is DE89 3704 0044 0532 0130 00 ok",
            "the IBAN is ok",
        ),
        ("my password is hunter2. Then we left", "my password Then we left"),
        ("social security 123-45-6789", "social security"),
        ("nothing to hide here", "nothing to hide here"),
    ];
    for (input, expected) in cases {
        let spans = Redactor::transcript_spans(input, d);
        assert_eq!(expected, Redactor::remove(input, &spans), "input: {input}");
    }
}

#[test]
fn c2_th1_masks_are_unchecked_until_ocr_reads_and_again_while_it_fails() {
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let reader = SwitchReader::new();
    let dropped = Arc::new(Mutex::new(Vec::<i32>::new()));
    let dropped_cb = dropped.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader.clone()),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    pipe.set_ocr_dropped(Some(Arc::new(move |m| dropped_cb.lock().unwrap().push(m))));

    assert!(pipe.masks_unchecked());
    pipe.post(non_forced_grab(0, 64, 32, 255, true, false, None));
    assert!(until(|| pipe.kept() == 1, Duration::from_secs(5)));
    assert!(!pipe.masks_unchecked());

    reader.set_answer(|| Err(WorkerError::Transport("Worker pipe closed.".to_string())));
    pipe.post(non_forced_grab(0, 64, 32, 0, true, false, None));
    assert!(until(|| pipe.redact_dropped() == 1, Duration::from_secs(5)));
    assert!(pipe.masks_unchecked());
    assert_eq!(vec![0], *dropped.lock().unwrap());

    reader.set_answer(|| {
        Ok(OcrResult {
            lines: Vec::new(),
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        })
    });
    let calls = reader.calls.load(Ordering::Acquire);
    pipe.post(non_forced_grab(0, 64, 32, 255, true, false, None));
    assert!(until(|| !pipe.masks_unchecked(), Duration::from_secs(5)));
    assert_eq!(calls + 1, reader.calls.load(Ordering::Acquire));
    assert_eq!(1, pipe.kept());
    pipe.stop();
}

#[test]
fn c2_ocr_not_installed_turns_the_text_mask_check_off() {
    let reader = SwitchReader::with_answer(|| {
        Err(WorkerError::Reply("Model not installed: ocr.".to_string()))
    });
    let store = FakeStore::new();
    let st_cb = store.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(Settings::default),
        Arc::new(|_| {}),
        Some(reader),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    pipe.post(non_forced_grab(0, 64, 32, 255, true, false, None));
    assert!(until(|| !pipe.masks_unchecked(), Duration::from_secs(5)));
    pipe.stop();
}

#[test]
fn c2_sample_grab_with_searchable_off_refreshes_the_masks_only() {
    let mut s = Settings::default();
    s.replay.searchable = false;
    let store = FakeStore::new();
    let st_cb = store.clone();
    let masks = Arc::new(RedactionMasks::new());
    let reader = SwitchReader::with_answer(|| {
        Ok(OcrResult {
            lines: vec![ocr_line("Card: 4111 1111 1111 1111", 0, 0, 20, None)],
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        })
    });
    let sink = FakeSampleSink::new();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader),
        Some(masks.clone()),
        None,
    );
    pipe.set_sampler(Some(sink.clone()));
    pipe.post(non_forced_grab(0, 400, 40, 255, false, true, None));
    assert!(until(|| !masks.all().is_empty(), Duration::from_secs(5)));
    assert!(!pipe.masks_unchecked());
    assert!(store.inner.lock().unwrap().screens.is_empty());
    assert_eq!(0, sink.count());
    pipe.stop();
}

#[test]
fn c2_samples_keep_running_through_storage_battery_and_fullscreen_pauses() {
    let r = ReplaySettings {
        searchable: false,
        ..ReplaySettings::default()
    };
    assert!(!CaptureService::sample_due(&r, false, false, 1, 5000, false));
    assert!(CaptureService::sample_due(&r, false, false, 1, 5000, true));
    assert!(!CaptureService::sample_due(&r, true, false, 1, 5000, true));

    let p = PauseController::new();
    for why in ["storage", "battery", "fullscreen"] {
        p.set(why, None);
        assert!(p.keyframes_paused());
        assert!(!p.continuous_paused(true));
    }
    p.set("user", None);
    assert!(p.continuous_paused(true));
}

#[test]
fn c2_replay_masks_the_foreground_while_text_masks_are_unchecked_and_warns() {
    let settings = Arc::new(Mutex::new(Settings::default()));
    let s1 = settings.clone();
    let s2 = settings.clone();
    let guard = make_guard();
    let fg_now = WindowInspector::foreground_root();
    guard.set_snapshot_for_test(fresh_snap(HashMap::new(), GuardDecision::ALLOWED));
    let reader = SwitchReader::new();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s1.lock().unwrap().clone()),
        Arc::new(|_| {}),
        Some(reader.clone()),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    let cs = CaptureService::new(
        Arc::new(move || s2.lock().unwrap().clone()),
        Arc::new(|| false),
        Arc::new(|| false),
        guard,
        Some(pipe.clone()),
        Some(Arc::new(RedactionMasks::new())),
        false,
    );

    let unchecked_masks = cs.masks();
    assert!(!cs.replay_text_unchecked());
    pipe.set_any_unchecked_at_ms_for_test(tick_count64() - CaptureService::WARN_AFTER_MS);
    assert!(cs.replay_text_unchecked());

    pipe.post(non_forced_grab(0, 64, 32, 255, true, false, None));
    assert!(until(|| !pipe.masks_unchecked(), Duration::from_secs(5)));
    let checked_masks = cs.masks();
    assert!(!cs.replay_text_unchecked());
    assert_eq!(checked_masks.len() + 1, unchecked_masks.len());
    let extra = *unchecked_masks.last().unwrap();
    if fg_now == 0 {
        assert_eq!(CaptureService::MASK_EVERYTHING, extra);
    } else {
        assert!(extra.right > extra.left || extra == CaptureService::MASK_EVERYTHING);
    }

    reader.set_answer(|| Err(WorkerError::Transport("Worker pipe closed.".to_string())));
    pipe.post(non_forced_grab(0, 64, 32, 0, true, false, None));
    assert!(until(|| pipe.masks_unchecked(), Duration::from_secs(5)));
    assert!(!cs.replay_text_unchecked());
    pipe.set_any_unchecked_at_ms_for_test(tick_count64() - CaptureService::WARN_AFTER_MS);
    assert!(cs.replay_text_unchecked());
    settings.lock().unwrap().ocr.enabled = false;
    assert!(!cs.replay_text_unchecked());
    assert_eq!(checked_masks.len(), cs.masks().len());
    pipe.stop();
}

#[test]
fn th1_retry_reaches_the_output_of_that_monitor() {
    let guard = make_guard();
    let pipe = KeyframePipeline::new(
        Arc::new(|| None),
        Arc::new(Settings::default),
        Arc::new(|_| {}),
        None,
        None,
        None,
    );
    let cs = CaptureService::new(
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(|| false),
        guard,
        Some(pipe.clone()),
        None,
        false,
    );
    let a = Arc::new(OutputCapture::new(
        OutputInfo {
            index: 0,
            device_name: r"\\.\DISPLAY1".to_string(),
            adapter_luid: 1,
            vendor_id: 0x8086,
            adapter_name: "gpu".to_string(),
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
            monitor: 0,
            refresh_hz: 60,
        },
        cs.devices().clone(),
        Arc::new(|_| {}),
    ));
    let b = Arc::new(OutputCapture::new(
        OutputInfo {
            index: 1,
            device_name: r"\\.\DISPLAY2".to_string(),
            adapter_luid: 1,
            vendor_id: 0x8086,
            adapter_name: "gpu".to_string(),
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
            monitor: 0,
            refresh_hz: 60,
        },
        cs.devices().clone(),
        Arc::new(|_| {}),
    ));
    cs.set_outputs_for_test(vec![a.clone(), b.clone()], false);
    // Trigger retry via full-queue drop on monitor 1 (or retry_all; here test monitor-1 specific via OCR failure).
    let reader = SwitchReader::with_answer(|| Err(WorkerError::Transport("down".to_string())));
    let store = FakeStore::new();
    let st_cb = store.clone();
    let pipe2 = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(Settings::default),
        Arc::new(|_| {}),
        Some(reader),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    let cs2 = CaptureService::new(
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(|| false),
        make_guard(),
        Some(pipe2.clone()),
        None,
        false,
    );
    cs2.set_outputs_for_test(vec![a.clone(), b.clone()], false);
    pipe2.post(white_grab(1, 64, 32, None, 0, None));
    assert!(until(|| pipe2.redact_dropped() == 1, Duration::from_secs(5)));
    assert!(!a.is_retry_keyframe_set());
    assert!(b.is_retry_keyframe_set());
    pipe.stop();
    pipe2.stop();
}

#[test]
fn th2_a_cold_worker_start_is_not_counted_as_slow_ocr() {
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let reader = SwitchReader::new();
    reader.set_ready(false, |ct| {
        if ct.wait_timeout(Duration::from_millis(450)) {
            return Err(WorkerError::Cancelled);
        }
        Ok(0)
    });
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader),
        Some(Arc::new(RedactionMasks::new())),
        Some(Duration::from_millis(200)),
    );
    pipe.post(non_forced_grab(0, 64, 32, 255, true, false, None));
    assert!(until(
        || pipe.kept() + pipe.redact_dropped() > 0,
        Duration::from_secs(5)
    ));
    assert_eq!(1, pipe.kept());
    assert_eq!(0, pipe.redact_dropped());
    assert_eq!(Duration::from_millis(200), pipe.ocr_wait());
    pipe.stop();
}

#[test]
fn am6_describe_waits_as_long_as_keyframe_ocr_and_not_for_the_worker_start() {
    let reader = SwitchReader::with_answer(|| Ok(card_ocr_result(10, 10, false)));
    reader.set_delay(Duration::from_millis(350));
    reader.set_ready(false, |ct| {
        if ct.wait_timeout(Duration::from_millis(350)) {
            return Err(WorkerError::Cancelled);
        }
        Ok(0)
    });
    let adaptive = AgentDescribeRedactor::new(
        reader.clone(),
        Arc::new(Settings::default),
        None,
        Some(Arc::new(|| Duration::from_secs(2))),
    )
    .with_min_wait(Duration::from_millis(200));
    let outp = adaptive
        .redact("s_1", outgoing_white(300, 60), &CancelToken::none())
        .unwrap();
    assert!(outp.redacted);

    let fixed_wait = AgentDescribeRedactor::new(
        reader.clone(),
        Arc::new(Settings::default),
        None,
        None,
    )
    .with_min_wait(Duration::from_millis(200));
    let err = fixed_wait
        .redact("s_1", outgoing_white(300, 60), &CancelToken::none())
        .expect_err("timeout");
    assert_eq!(AgentDescribeRedactor::TIMED_OUT, err.to_string());

    reader.set_ready(true, |_| Ok(2));
    let err2 = fixed_wait
        .redact("s_1", outgoing_white(300, 60), &CancelToken::none())
        .expect_err("busy");
    assert_eq!(AgentDescribeRedactor::BUSY, err2.to_string());
}

#[test]
fn cas1_downscaled_image_is_checked_against_the_full_size_keyframe() {
    let store = FakeStore::new();
    let full = outgoing_white(800, 120);
    let id = store.add_screen(NewScreenCapture {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "t".to_string(),
        monitor: 0,
        width: 800,
        height: 120,
        d_hash: 0,
        image: full.bytes,
        image_mime: "image/webp".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    });
    let reader = SwitchReader::with_answer(|| {
        Ok(OcrResult {
            lines: vec![ocr_line("Card: 4111 1111 1111 1111", 40, 40, 20, None)],
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        })
    });
    let st_cb = store.clone();
    let r = AgentDescribeRedactor::new(
        reader.clone(),
        Arc::new(Settings::default),
        Some(Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeBlobStore>))),
        None,
    );
    let outp = r
        .redact(&id, outgoing_white(400, 60), &CancelToken::none())
        .unwrap();
    assert_eq!(
        (800, 120),
        (*reader.last_w.lock().unwrap(), *reader.last_h.lock().unwrap())
    );
    assert!(outp.redacted);
    assert_eq!((400, 60), (outp.width, outp.height));
    assert!(decode_pixel_r(&outp.bytes, 100, 25) < 40);
    assert!(decode_pixel_r(&outp.bytes, 25, 25) > 200);
}

#[test]
fn cas1_unreadable_keyframe_leaves_the_image_not_checked() {
    let store = FakeStore::new();
    let id = store.add_screen(NewScreenCapture {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "t".to_string(),
        monitor: 0,
        width: 800,
        height: 120,
        d_hash: 0,
        image: vec![1, 2, 3, 4],
        image_mime: "image/webp".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    });
    let reader = SwitchReader::new();
    let st_cb = store.clone();
    let outp = AgentDescribeRedactor::new(
        reader.clone(),
        Arc::new(Settings::default),
        Some(Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeBlobStore>))),
        None,
    )
    .redact(&id, outgoing_white(400, 60), &CancelToken::none())
    .unwrap();
    assert!(!outp.redacted);
    assert_eq!(
        (400, 60),
        (*reader.last_w.lock().unwrap(), *reader.last_h.lock().unwrap())
    );
}

#[test]
fn cas1_boxes_scale_outwards() {
    let b = AgentDescribeRedactor::scale_boxes(&[PixelBox::new(101, 41, 191, 21)], 800, 120, 400, 60);
    assert_eq!(vec![PixelBox::new(48, 18, 100, 15)], b);
    let edge = AgentDescribeRedactor::scale_boxes(
        &[PixelBox::new(0, 0, 10, 10), PixelBox::new(790, 110, 10, 10)],
        800,
        120,
        400,
        60,
    );
    assert_eq!(PixelBox::new(0, 0, 7, 7), edge[0]);
    assert_eq!(PixelBox::new(393, 53, 7, 7), edge[1]);
}

#[test]
fn m2_worker_start_and_queue_drops_mark_masks_unchecked_until_a_newer_grab_reads() {
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let start_next = Arc::new(AtomicBool::new(false));
    let sn_cb = start_next.clone();
    let reader = SwitchReader::new();
    reader.set_ready(true, move |ct| {
        if sn_cb.swap(false, Ordering::AcqRel) {
            let _ = ct.wait_timeout(Duration::from_millis(500));
        }
        Ok(0)
    });
    let dropped = Arc::new(Mutex::new(Vec::<i32>::new()));
    let dropped_cb = dropped.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader.clone()),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    pipe.set_ocr_dropped(Some(Arc::new(move |m| dropped_cb.lock().unwrap().push(m))));

    pipe.post(non_forced_grab(0, 64, 32, 255, true, false, None));
    assert!(until(|| !pipe.masks_unchecked(), Duration::from_secs(5)));

    start_next.store(true, Ordering::Release);
    reader.set_is_ready(false);
    let reads = reader.calls.load(Ordering::Acquire);
    pipe.post(non_forced_grab(0, 64, 32, 10, true, false, None));
    thread::sleep(Duration::from_millis(100));
    assert!(pipe.masks_unchecked());
    for i in 0..6u8 {
        pipe.post(non_forced_grab(0, 64, 32, 20 + i, true, false, None));
    }
    thread::sleep(Duration::from_millis(20));
    pipe.post(non_forced_grab(0, 64, 32, 90, true, false, None));
    assert!(dropped.lock().unwrap().contains(&0));
    reader.set_is_ready(true);
    assert!(until(
        || reader.calls.load(Ordering::Acquire) >= reads + 7,
        Duration::from_secs(5)
    ));
    thread::sleep(Duration::from_millis(80));
    assert!(pipe.masks_unchecked());

    pipe.post(non_forced_grab(0, 64, 32, 120, true, false, None));
    assert!(until(|| !pipe.masks_unchecked(), Duration::from_secs(5)));
    pipe.stop();
}

#[test]
fn s1_text_masks_are_checked_per_monitor() {
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(SwitchReader::new()),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    pipe.track_monitors(&[0, 1]);
    pipe.post(non_forced_grab(0, 64, 32, 255, true, false, None));
    assert!(until(|| !pipe.monitor_unchecked(0), Duration::from_secs(5)));
    assert!(pipe.monitor_unchecked(1));
    assert!(pipe.masks_unchecked());

    pipe.post(non_forced_grab(1, 64, 32, 255, true, false, None));
    assert!(until(|| !pipe.masks_unchecked(), Duration::from_secs(5)));
    pipe.mark_all_unchecked();
    assert!(pipe.monitor_unchecked(0) && pipe.monitor_unchecked(1));
    pipe.stop();
}

#[test]
fn s2_denied_foreground_still_refreshes_the_masks_of_other_monitors() {
    assert!(CaptureService::refresh_masks_while_denied(GuardReason::DenyProcess));
    assert!(CaptureService::refresh_masks_while_denied(GuardReason::PrivateBadge));
    assert!(!CaptureService::refresh_masks_while_denied(GuardReason::SessionLocked));
    assert!(!CaptureService::refresh_masks_while_denied(GuardReason::ForegroundChanged));

    let guard = make_guard();
    let pipe = KeyframePipeline::new(
        Arc::new(|| None),
        Arc::new(Settings::default),
        Arc::new(|_| {}),
        None,
        None,
        None,
    );
    let cs = CaptureService::new(
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(|| false),
        guard.clone(),
        Some(pipe.clone()),
        None,
        false,
    );
    let a = Arc::new(OutputCapture::new(
        OutputInfo {
            index: 0,
            device_name: r"\\.\DISPLAY1".to_string(),
            adapter_luid: 1,
            vendor_id: 0x8086,
            adapter_name: "gpu".to_string(),
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
            monitor: 0,
            refresh_hz: 60,
        },
        cs.devices().clone(),
        Arc::new(|_| {}),
    ));
    let b = Arc::new(OutputCapture::new(
        OutputInfo {
            index: 1,
            device_name: r"\\.\DISPLAY2".to_string(),
            adapter_luid: 1,
            vendor_id: 0x8086,
            adapter_name: "gpu".to_string(),
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
            monitor: 0,
            refresh_hz: 60,
        },
        cs.devices().clone(),
        Arc::new(|_| {}),
    ));
    cs.set_outputs_for_test(vec![a.clone(), b.clone()], false);
    let mut per = HashMap::new();
    per.insert(
        0,
        MonitorTop {
            monitor: 0,
            window: None,
            denied_visible: true,
        },
    );
    per.insert(
        1,
        MonitorTop {
            monitor: 1,
            window: None,
            denied_visible: false,
        },
    );
    guard.set_snapshot_for_test(fresh_snap(per, GuardDecision::deny(GuardReason::DenyProcess)));
    cs.request_keyframes(false, true, false, false);
    assert!(a.pending_keyframe().is_none());
    let req = b.pending_keyframe().expect("mask-only req");
    assert!(req.mask_only);
    assert!(!req.keyframe);
    assert!(req.app.is_empty());
    assert!(req.title.is_empty());
    pipe.stop();
}

#[test]
fn s9_describe_does_not_hang_behind_a_worker_start() {
    let reader = SwitchReader::with_answer(|| Ok(card_ocr_result(10, 10, false)));
    reader.set_ready(false, |ct| {
        if ct.wait_timeout(Duration::from_secs(3)) {
            return Err(WorkerError::Cancelled);
        }
        Ok(0)
    });
    let r = AgentDescribeRedactor::new(reader, Arc::new(Settings::default), None, None)
        .with_start_wait(Duration::from_millis(200));
    let err = r
        .redact("s_1", outgoing_white(300, 60), &CancelToken::none())
        .expect_err("starting");
    assert_eq!(AgentDescribeRedactor::STARTING, err.to_string());
}

#[test]
fn s12_avif_keyframe_is_decoded_for_the_check() {
    if !Avif::try_load() {
        return;
    }
    let store = FakeStore::new();
    let px = vec![255u8; 800 * 120 * 4];
    let avif = unsafe { ImageCodec::encode_bgra(px.as_ptr(), 800, 120, 800 * 4, "avif", 80) }.unwrap();
    let id = store.add_screen(NewScreenCapture {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "t".to_string(),
        monitor: 0,
        width: 800,
        height: 120,
        d_hash: 0,
        image: avif,
        image_mime: "image/avif".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    });
    let reader = SwitchReader::with_answer(|| {
        Ok(OcrResult {
            lines: vec![ocr_line("Card: 4111 1111 1111 1111", 40, 40, 20, None)],
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        })
    });
    let st_cb = store.clone();
    let r = AgentDescribeRedactor::new(
        reader.clone(),
        Arc::new(Settings::default),
        Some(Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeBlobStore>))),
        None,
    );
    let outp = r
        .redact(&id, outgoing_white(400, 60), &CancelToken::none())
        .unwrap();
    assert_eq!(
        (800, 120),
        (*reader.last_w.lock().unwrap(), *reader.last_h.lock().unwrap())
    );
    assert!(outp.redacted);
    assert!(decode_pixel_r(&outp.bytes, 100, 25) < 40);
}

// ---------------- Sweep5PAgentTests (A1, A2) ----------------

#[test]
fn a1_cold_or_restarted_worker_in_read_clears_masks_unchecked_after_ocr_succeeds() {
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let entered_cb = entered.clone();
    let gate_cb = gate.clone();

    let reader = SwitchReader::new();
    reader.set_ready(false, move |ct| {
        *entered_cb.0.lock().unwrap() = true;
        entered_cb.1.notify_all();
        let mut g = gate_cb.0.lock().unwrap();
        while !*g && !ct.is_cancelled() {
            let (next, _) = gate_cb
                .1
                .wait_timeout(g, Duration::from_millis(20))
                .unwrap();
            g = next;
        }
        if ct.is_cancelled() {
            Err(WorkerError::Cancelled)
        } else {
            Ok(0)
        }
    });

    let guard = make_guard();
    let s1 = s.clone();
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s1.clone()),
        Arc::new(|_| {}),
        Some(reader.clone()),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    let cs = CaptureService::new(
        Arc::new(move || s.clone()),
        Arc::new(|| false),
        Arc::new(|| false),
        guard,
        Some(pipe.clone()),
        Some(Arc::new(RedactionMasks::new())),
        false,
    );

    pipe.track_monitors(&[0]);
    pipe.post(non_forced_grab(
        0,
        64,
        32,
        10,
        true,
        false,
        Some(Utc::now() - ChronoDuration::seconds(2)),
    ));
    {
        let mut g = entered.0.lock().unwrap();
        while !*g {
            let (next, res) = entered.1.wait_timeout(g, Duration::from_secs(5)).unwrap();
            assert!(!res.timed_out());
            g = next;
        }
    }
    assert!(until(|| pipe.masks_unchecked(), Duration::from_secs(5)));
    pipe.set_any_unchecked_at_ms_for_test(tick_count64() - CaptureService::WARN_AFTER_MS);
    assert!(cs.replay_text_unchecked());

    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(until(|| pipe.ocr_read() == 1, Duration::from_secs(5)));
    assert!(!pipe.masks_unchecked());
    assert!(!cs.replay_text_unchecked());

    // Restarted worker after idle exit.
    *entered.0.lock().unwrap() = false;
    *gate.0.lock().unwrap() = false;
    pipe.post(non_forced_grab(
        0,
        64,
        32,
        40,
        true,
        false,
        Some(Utc::now() - ChronoDuration::seconds(2)),
    ));
    {
        let mut g = entered.0.lock().unwrap();
        while !*g {
            let (next, res) = entered.1.wait_timeout(g, Duration::from_secs(5)).unwrap();
            assert!(!res.timed_out());
            g = next;
        }
    }
    assert!(until(|| pipe.masks_unchecked(), Duration::from_secs(5)));
    pipe.set_any_unchecked_at_ms_for_test(tick_count64() - CaptureService::WARN_AFTER_MS);
    assert!(cs.replay_text_unchecked());

    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(until(|| pipe.ocr_read() == 2, Duration::from_secs(5)));
    assert!(!pipe.masks_unchecked());
    assert!(!cs.replay_text_unchecked());
    pipe.stop();
}

#[test]
fn a1_static_second_monitor_stays_checked_and_queued_monitor_clears_after_worker_starts() {
    let mut s = Settings::default();
    s.keyframes.format = "webp".to_string();
    let store = FakeStore::new();
    let st_cb = store.clone();
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let stall = Arc::new(AtomicBool::new(false));
    let entered_cb = entered.clone();
    let gate_cb = gate.clone();
    let stall_cb = stall.clone();

    let reader = SwitchReader::new();
    reader.set_ready(true, move |ct| {
        if !stall_cb.load(Ordering::Acquire) {
            return Ok(0);
        }
        *entered_cb.0.lock().unwrap() = true;
        entered_cb.1.notify_all();
        let mut g = gate_cb.0.lock().unwrap();
        while !*g && !ct.is_cancelled() {
            let (next, _) = gate_cb
                .1
                .wait_timeout(g, Duration::from_millis(20))
                .unwrap();
            g = next;
        }
        if ct.is_cancelled() {
            Err(WorkerError::Cancelled)
        } else {
            Ok(0)
        }
    });
    let pipe = KeyframePipeline::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn KeyframeStore>)),
        Arc::new(move || s.clone()),
        Arc::new(|_| {}),
        Some(reader.clone()),
        Some(Arc::new(RedactionMasks::new())),
        None,
    );
    pipe.track_monitors(&[0, 1]);
    pipe.post(non_forced_grab(0, 64, 32, 10, true, false, None));
    pipe.post(non_forced_grab(1, 64, 32, 10, true, false, None));
    assert!(until(|| !pipe.masks_unchecked(), Duration::from_secs(5)));

    stall.store(true, Ordering::Release);
    reader.set_is_ready(false);
    pipe.post(non_forced_grab(0, 64, 32, 30, true, false, None));
    {
        let mut g = entered.0.lock().unwrap();
        while !*g {
            let (next, res) = entered.1.wait_timeout(g, Duration::from_secs(5)).unwrap();
            assert!(!res.timed_out());
            g = next;
        }
    }
    assert!(until(|| pipe.monitor_unchecked(0), Duration::from_secs(5)));
    assert!(!pipe.monitor_unchecked(1));

    pipe.post(non_forced_grab(1, 64, 32, 50, true, false, None));
    assert!(pipe.monitor_unchecked(1));

    stall.store(false, Ordering::Release);
    reader.set_is_ready(true);
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(until(|| pipe.ocr_read() == 4, Duration::from_secs(5)));
    assert!(!pipe.masks_unchecked());
    pipe.stop();
}

#[test]
fn a2_describe_redactor_preserves_bytes_when_thumbnail_cache_clears() {
    let store = FakeStore::new();
    let webp = outgoing_white(640, 360);
    let id = store.add_screen(NewScreenCapture {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "t".to_string(),
        monitor: 0,
        width: 640,
        height: 360,
        d_hash: 0,
        image: webp.bytes,
        image_mime: "image/webp".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    });
    let st_t = store.clone();
    let thumbs = AgentThumbnailer::new(
        Arc::new(move || Some(st_t.clone() as Arc<dyn KeyframeBlobStore>)),
        Arc::new(Settings::default),
    );
    let t = thumbs
        .get_thumb(&id, 320, &CancelToken::none())
        .expect("thumb");
    let original = t.bytes.clone();

    let clean_reader = SwitchReader::with_answer(|| {
        Ok(OcrResult {
            lines: vec![ocr_line("Hello world", 10, 10, 20, None)],
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        })
    });
    let st1 = store.clone();
    let clean = AgentDescribeRedactor::new(
        clean_reader.clone(),
        Arc::new(Settings::default),
        Some(Arc::new(move || Some(st1.clone() as Arc<dyn KeyframeBlobStore>))),
        None,
    )
    .redact(
        &id,
        OutgoingImage {
            bytes: t.bytes.clone(),
            mime: t.mime.clone(),
            width: t.width,
            height: t.height,
            redacted: false,
        },
        &CancelToken::none(),
    )
    .unwrap();
    assert!(clean.redacted);

    let mut no_det = Settings::default();
    no_det.privacy.redact_passwords = false;
    no_det.privacy.redact_cards = false;
    no_det.privacy.redact_ids = false;
    no_det.privacy.redact_birth_dates = false;
    let st2 = store.clone();
    let unchecked_out = AgentDescribeRedactor::new(
        clean_reader.clone(),
        Arc::new(move || no_det.clone()),
        Some(Arc::new(move || Some(st2.clone() as Arc<dyn KeyframeBlobStore>))),
        None,
    )
    .redact(
        &id,
        OutgoingImage {
            bytes: t.bytes.clone(),
            mime: t.mime.clone(),
            width: t.width,
            height: t.height,
            redacted: false,
        },
        &CancelToken::none(),
    )
    .unwrap();

    let mut ocr_off = Settings::default();
    ocr_off.ocr.enabled = false;
    let st3 = store.clone();
    let off_out = AgentDescribeRedactor::new(
        clean_reader,
        Arc::new(move || ocr_off.clone()),
        Some(Arc::new(move || Some(st3.clone() as Arc<dyn KeyframeBlobStore>))),
        None,
    )
    .redact(
        &id,
        OutgoingImage {
            bytes: t.bytes.clone(),
            mime: t.mime.clone(),
            width: t.width,
            height: t.height,
            redacted: false,
        },
        &CancelToken::none(),
    )
    .unwrap();

    let missing_r = SwitchReader::with_answer(|| {
        Err(WorkerError::Reply("Model not installed: ocr.".to_string()))
    });
    let st4 = store.clone();
    let missing_out = AgentDescribeRedactor::new(
        missing_r,
        Arc::new(Settings::default),
        Some(Arc::new(move || Some(st4.clone() as Arc<dyn KeyframeBlobStore>))),
        None,
    )
    .redact(
        &id,
        OutgoingImage {
            bytes: t.bytes.clone(),
            mime: t.mime.clone(),
            width: t.width,
            height: t.height,
            redacted: false,
        },
        &CancelToken::none(),
    )
    .unwrap();

    thumbs.clear_cache();
    assert_eq!(0, thumbs.cache().count());
    for outp in [&clean, &unchecked_out, &off_out, &missing_out] {
        assert_eq!(original, outp.bytes);
        assert_eq!(b"RIFF", &outp.bytes[..4]);
        assert!(ImageCodec::decode_bgra(&outp.bytes, "image/webp").is_ok());
    }
}

#[test]
fn a2_seam_unrelated_purge_through_agent_keeps_describe_redactor_bytes_intact() {
    let dir = TempDir::new("s5p-a2");
    std::fs::write(
        DataPaths::settings_file(dir.path()),
        r#"{"security":{"keyProtection":"dpapi"}}"#,
    )
    .unwrap();
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    core.open_store(&CancelToken::none()).unwrap();
    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    let agent_for_purge = agent.clone();
    let _pg = core.on_purged(move |n| agent_for_purge.on_purged(n));

    let worker = FakeWorker::new();
    worker.set_handler(|req, _| {
        if req.op != "ocr" {
            return None;
        }
        let mut resp = WorkerResponse::ok(req.id);
        resp.ocr = Some(OcrResult {
            lines: Vec::new(),
            engine: "fake".to_string(),
            ms: 1.0,
            entities_missing: false,
        });
        Some((resp, Vec::new()))
    });
    let sup = WorkerSupervisor::new(worker, dir.path().to_path_buf(), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let core_s = core.clone();
    let core_st = core.clone();
    let redactor = AgentDescribeRedactor::new(
        Arc::new(WorkerKeyframeReader::new(client)),
        Arc::new(move || core_s.settings().current()),
        Some(Arc::new(move || {
            core_st.store().map(|st| st as Arc<dyn KeyframeBlobStore>)
        })),
        None,
    );

    let store = core.store().unwrap();
    let img_a = outgoing_white(640, 360);
    let img_b = outgoing_white(640, 360);
    let id_a = store
        .add_screen_capture(&NewScreenCapture {
            ts: Utc::now() - ChronoDuration::minutes(2),
            app: "notepad".to_string(),
            exe_path: String::new(),
            title: "note A".to_string(),
            monitor: 0,
            width: 640,
            height: 360,
            d_hash: 1,
            image: img_a.bytes,
            image_mime: "image/webp".to_string(),
            keep_keyframe: true,
            site: String::new(),
            change: None,
            switch: false,
            text: None,
        })
        .unwrap();
    let id_b = store
        .add_screen_capture(&NewScreenCapture {
            ts: Utc::now() - ChronoDuration::minutes(1),
            app: "calc".to_string(),
            exe_path: String::new(),
            title: "calc B".to_string(),
            monitor: 0,
            width: 640,
            height: 360,
            d_hash: 2,
            image: img_b.bytes,
            image_mime: "image/webp".to_string(),
            keep_keyframe: true,
            site: String::new(),
            change: None,
            switch: false,
            text: None,
        })
        .unwrap();
    let emb = EmbeddingInfo {
        model_id: "fake".to_string(),
        revision: "r1".to_string(),
        dim: 4,
    };
    store.set_embedding(&id_a, &[1.0, 0.0, 0.0, 0.0], &emb).unwrap();
    store.set_embedding(&id_b, &[1.0, 0.0, 0.0, 0.0], &emb).unwrap();

    let t = agent
        .thumbs()
        .get_thumb(&id_a, 320, &CancelToken::none())
        .expect("thumb");
    let original = t.bytes.clone();
    let outp = redactor
        .redact(
            &id_a,
            OutgoingImage {
                bytes: t.bytes,
                mime: t.mime,
                width: t.width,
                height: t.height,
                redacted: false,
            },
            &CancelToken::none(),
        )
        .unwrap();
    assert!(outp.redacted);

    assert_eq!(1, store.purge(&PurgeScope::for_id(&id_b)).unwrap());
    assert_eq!(0, agent.thumbs().cache().count());
    assert_eq!(original, outp.bytes);
    assert!(ImageCodec::decode_bgra(&outp.bytes, "image/webp").is_ok());

    sup.dispose();
    agent.shutdown();
    core.shutdown();
}
