// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Capture export and export journal tests (`CaptureExporter`, `ExportJournal`).

mod common;

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};

use br_contracts::{ExportRequest, ExportResult, PurgeScope, full_path};
use br_core::{CaptureExporter, ExportJournal, StoreError};
use chrono::{Duration as ChronoDuration, Utc};
use zip::ZipArchive;

use common::{FAKE_WEBP, TempHome, TempStore, audio, screen, vec_info, vec_seeded};

const IMAGE: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 sweep4-image-bytes";

#[test]
fn writes_images_and_captures_json_filtered_by_app() {
    let t = TempStore::new();
    let now = Utc::now();
    for i in 0..5 {
        let app = if i % 2 == 0 { "code" } else { "chrome" };
        let id = t
            .store()
            .add_screen_capture(&screen(
                now - ChronoDuration::minutes(10 - i),
                app,
                &format!("Title {i}"),
                true,
            ))
            .expect("add");
        t.store().take_pending_embeddings(10).expect("take");
        t.store().set_embedding(&id, &vec_seeded(i as i32), &vec_info()).expect("embed");
    }
    t.store()
        .add_audio_capture(&audio(now - ChronoDuration::minutes(3), "meeting notes", "code"))
        .expect("audio");
    let outside = t
        .store()
        .add_screen_capture(&screen(now - ChronoDuration::days(2), "code", "Some window", true))
        .expect("add");
    t.store().take_pending_embeddings(10).expect("take");
    t.store().set_embedding(&outside, &vec_seeded(9), &vec_info()).expect("embed");

    let path = t.home.path().join("export.zip");
    let req = ExportRequest {
        from: now - ChronoDuration::hours(1),
        to: now,
        path: path.to_string_lossy().into_owned(),
        app: Some("CODE.exe".to_string()),
        jpeg: false,
        dry_run: false,
    };
    assert_eq!(
        CaptureExporter::count(t.store(), &req).expect("count"),
        ExportResult {
            count: 4,
            images: 3,
            bytes: 0,
        }
    );

    let mut calls = 0usize;
    let r = CaptureExporter::write(
        t.store(),
        &req,
        |mut kf, target| {
            calls += 1;
            if target == "jpeg" {
                kf.mime = "image/jpeg".to_string();
                kf.bytes = vec![0xFF, 0xD8, 1, 2];
            }
            kf
        },
        None,
    )
    .expect("write");
    assert_eq!(r.count, 4);
    assert_eq!(r.images, 3);
    assert_eq!(calls, 0);
    assert!(!t.home.path().join("export.zip.partial").exists());

    {
        let mut zip = ZipArchive::new(File::open(&path).expect("open zip")).expect("zip");
        assert_eq!(zip.len(), 4);
        let mut json_str = String::new();
        zip.by_name("captures.json")
            .expect("captures.json")
            .read_to_string(&mut json_str)
            .expect("read json");
        let items: Vec<serde_json::Value> = serde_json::from_str(&json_str).expect("parse json");
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["title"].as_str(), Some("Title 0"));
        let first_file = items[0]["file"].as_str().expect("file");
        assert!(first_file.ends_with(".webp"));
        assert_eq!(items[3]["kind"].as_str(), Some("audio"));
        assert_eq!(items[3]["text"].as_str(), Some("meeting notes"));
        assert!(items[3].get("file").is_none());

        let mut img_bytes = Vec::new();
        zip.by_name(first_file)
            .expect("img entry")
            .read_to_end(&mut img_bytes)
            .expect("read img");
        assert_eq!(img_bytes, FAKE_WEBP);
    }

    let jpeg_path = t.home.path().join("jpeg.zip");
    let mut jpeg_req = req;
    jpeg_req.path = jpeg_path.to_string_lossy().into_owned();
    jpeg_req.jpeg = true;
    let _ = CaptureExporter::write(
        t.store(),
        &jpeg_req,
        |mut kf, target| {
            calls += 1;
            if target == "jpeg" {
                kf.mime = "image/jpeg".to_string();
                kf.bytes = vec![0xFF, 0xD8, 1, 2];
            }
            kf
        },
        None,
    )
    .expect("write jpeg");
    assert_eq!(calls, 3);
    let zip = ZipArchive::new(File::open(&jpeg_path).expect("open jpeg zip")).expect("zip");
    let jpg_count = (0..zip.len())
        .filter(|&i| zip.name_for_index(i).is_some_and(|n| n.ends_with(".jpg")))
        .count();
    assert_eq!(jpg_count, 3);
}

#[test]
fn paging_covers_more_rows_than_one_timeline_page() {
    let t = TempStore::new();
    let now = Utc::now();
    let info = vec_info();
    let rows = (0..4500).map(|i| {
        (
            now - ChronoDuration::seconds(i + 1),
            "app".to_string(),
            format!("t{i}"),
            vec_seeded((i % 50) as i32),
        )
    });
    t.store().insert_synthetic(rows, &info).expect("insert");
    let r = CaptureExporter::count(
        t.store(),
        &ExportRequest {
            from: now - ChronoDuration::days(1),
            to: now,
            path: String::new(),
            app: None,
            jpeg: false,
            dry_run: false,
        },
    )
    .expect("count");
    assert_eq!(r.count, 4500);
    assert_eq!(r.images, 0);
}

/// Regressions B12, C1 (image + JSON phases), B1 (gate lock), and C4 (cancellation before move):
/// every abort trigger fails with the expected error and leaves neither the target zip nor `.partial`.
#[test]
fn export_aborts_when_purged_locked_or_cancelled() {
    enum AbortCase {
        PurgeLast,
        PurgeAllImagePhase,
        PurgeAllJsonPhase,
        GateLocked,
        CancelledInJson,
    }
    for case in [
        AbortCase::PurgeLast,
        AbortCase::PurgeAllImagePhase,
        AbortCase::PurgeAllJsonPhase,
        AbortCase::GateLocked,
        AbortCase::CancelledInJson,
    ] {
        let t = TempStore::new();
        let out = TempHome::new(None);
        let t0 = Utc::now() - ChronoDuration::minutes(4);
        let two_rows = matches!(
            case,
            AbortCase::PurgeLast | AbortCase::PurgeAllImagePhase | AbortCase::GateLocked
        );
        let mut c1 = screen(t0, "chrome.exe", "FIRST-ROW-SECRET", true);
        c1.image = IMAGE.to_vec();
        let first_id = t.store().add_screen_capture(&c1).expect("a");
        t.store().set_embedding(&first_id, &vec_seeded(5), &vec_info()).expect("e1");
        if two_rows {
            let mut c2 = screen(t0 + ChronoDuration::minutes(1), "chrome.exe", "second row", true);
            c2.image = IMAGE.to_vec();
            let b = t.store().add_screen_capture(&c2).expect("b");
            t.store().set_embedding(&b, &vec_seeded(6), &vec_info()).expect("e2");
        }

        let path = out.path().join("out.zip");
        let req = ExportRequest {
            from: Utc::now() - ChronoDuration::hours(1),
            to: Utc::now(),
            path: path.to_string_lossy().into_owned(),
            app: None,
            jpeg: !matches!(case, AbortCase::GateLocked),
            dry_run: false,
        };
        let mut calls = 0usize;
        let cancel = AtomicBool::new(false);
        let err = match case {
            AbortCase::PurgeLast => CaptureExporter::write(
                t.store(),
                &req,
                |kf, _| {
                    if calls == 0 {
                        let _ = t.store().purge(&PurgeScope::last(ChronoDuration::minutes(10)));
                    }
                    calls += 1;
                    kf
                },
                None,
            )
            .expect_err("must abort"),
            AbortCase::PurgeAllImagePhase => CaptureExporter::write(
                t.store(),
                &req,
                |kf, _| {
                    if calls == 0 {
                        t.store().close_for_purge_all();
                    }
                    calls += 1;
                    kf
                },
                None,
            )
            .expect_err("must abort"),
            AbortCase::PurgeAllJsonPhase => CaptureExporter::write(
                t.store(),
                &req,
                |kf, _| {
                    t.store().close_for_purge_all();
                    kf
                },
                None,
            )
            .expect_err("must abort"),
            AbortCase::GateLocked => {
                let mut checks = 0usize;
                let mut locked = false;
                CaptureExporter::write_with_lock(
                    t.store(),
                    &req,
                    |kf, _| kf,
                    Some(|| {
                        checks += 1;
                        if checks >= 2 {
                            locked = true;
                        }
                        locked
                    }),
                    None,
                )
                .expect_err("must lock")
            }
            AbortCase::CancelledInJson => CaptureExporter::write(
                t.store(),
                &req,
                |kf, _| {
                    cancel.store(true, Ordering::Release);
                    kf
                },
                Some(&cancel),
            )
            .expect_err("must cancel"),
        };

        match case {
            AbortCase::PurgeLast => {
                assert!(matches!(err, StoreError::ExportPurged), "got {err:?}");
                assert!(t.store().get(&first_id).expect("g").is_none());
            }
            AbortCase::PurgeAllImagePhase | AbortCase::PurgeAllJsonPhase => {
                assert!(matches!(err, StoreError::ExportPurged), "got {err:?}");
            }
            AbortCase::GateLocked => {
                assert!(matches!(err, StoreError::ExportLocked), "got {err:?}");
            }
            AbortCase::CancelledInJson => {
                assert!(matches!(err, StoreError::Cancelled), "got {err:?}");
            }
        }
        assert!(!path.exists());
        assert!(!out.path().join("out.zip.partial").exists());
    }
}

/// Regression B12.
#[test]
fn unrelated_purge_does_not_abort_an_export() {
    let t = TempStore::new();
    let out = TempHome::new(None);
    let mut c1 = screen(Utc::now() - ChronoDuration::minutes(4), "chrome.exe", "kept", true);
    c1.image = IMAGE.to_vec();
    let kept = t.store().add_screen_capture(&c1).expect("kept");
    t.store().set_embedding(&kept, &vec_seeded(7), &vec_info()).expect("e1");
    let mut c2 = screen(Utc::now() - ChronoDuration::days(3), "notepad.exe", "outside the window", true);
    c2.image = IMAGE.to_vec();
    let other = t.store().add_screen_capture(&c2).expect("other");
    t.store().set_embedding(&other, &vec_seeded(8), &vec_info()).expect("e2");

    let path = out.path().join("out2.zip");
    let mut calls = 0usize;
    let r = CaptureExporter::write(
        t.store(),
        &ExportRequest {
            from: Utc::now() - ChronoDuration::hours(1),
            to: Utc::now(),
            path: path.to_string_lossy().into_owned(),
            app: None,
            jpeg: true,
            dry_run: false,
        },
        |kf, _| {
            if calls == 0 {
                let _ = t.store().purge(&PurgeScope::for_id(&other));
            }
            calls += 1;
            kf
        },
        None,
    )
    .expect("export ok");
    assert_eq!(r.count, 1);
    assert!(path.exists());
}

/// Regression C3.
#[test]
fn leftover_export_partial_is_deleted_at_sweep() {
    let h = TempHome::new(None);
    let out = TempHome::new(None);
    let left = out.path().join("out.zip.partial");
    let finished = out.path().join("done.zip");
    fs::write(&left, [1u8, 2, 3]).expect("w");
    fs::write(&finished, [4u8]).expect("w");
    ExportJournal::add(Some(h.path()), &left);
    let journal = ExportJournal::path_for(h.path());
    let mut text = fs::read_to_string(&journal).expect("read");
    text.push_str(&finished.to_string_lossy());
    text.push_str("\r\n");
    fs::write(&journal, text).expect("append");

    assert_eq!(ExportJournal::sweep(h.path()), 1);
    assert!(!left.exists());
    assert!(finished.exists());
    assert!(!journal.exists());
}

/// Regression C3.
#[test]
fn capture_export_lists_its_partial_while_it_runs() {
    let t = TempStore::new();
    let out = TempHome::new(None);
    let mut c = screen(Utc::now() - ChronoDuration::minutes(4), "chrome.exe", "row", true);
    c.image = IMAGE.to_vec();
    let id = t.store().add_screen_capture(&c).expect("add");
    t.store().set_embedding(&id, &vec_seeded(12), &vec_info()).expect("e");

    let path = out.path().join("listed.zip");
    let mut listed: Vec<String> = Vec::new();
    CaptureExporter::write(
        t.store(),
        &ExportRequest {
            from: Utc::now() - ChronoDuration::hours(1),
            to: Utc::now(),
            path: path.to_string_lossy().into_owned(),
            app: None,
            jpeg: true,
            dry_run: false,
        },
        |kf, _| {
            let content = fs::read_to_string(ExportJournal::path_for(t.home.path())).expect("read");
            listed = content.lines().map(ToOwned::to_owned).collect();
            kf
        },
        None,
    )
    .expect("write");
    let expected = full_path(&out.path().join("listed.zip.partial"))
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(listed, vec![expected]);
    assert!(path.exists());
    assert!(!ExportJournal::path_for(t.home.path()).exists());
}

/// Regression C3.
#[test]
fn failed_export_leaves_a_file_it_did_not_create() {
    let t = TempStore::new();
    let out = TempHome::new(None);
    let mut c = screen(Utc::now() - ChronoDuration::minutes(4), "chrome.exe", "row", true);
    c.image = IMAGE.to_vec();
    let id = t.store().add_screen_capture(&c).expect("add");
    t.store().set_embedding(&id, &vec_seeded(13), &vec_info()).expect("e");

    let path = out.path().join("users.zip");
    let theirs = out.path().join("users.zip.partial");
    fs::write(&theirs, [9u8, 9]).expect("w");
    {
        // Hold FILE_SHARE_READ only so share_mode(0) write open fails.
        let _lock = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&theirs)
            .expect("open");
        let err = CaptureExporter::write(
            t.store(),
            &ExportRequest {
                from: Utc::now() - ChronoDuration::hours(1),
                to: Utc::now(),
                path: path.to_string_lossy().into_owned(),
                app: None,
                jpeg: true,
                dry_run: false,
            },
            |kf, _| kf,
            None,
        )
        .expect_err("must fail");
        assert!(matches!(err, StoreError::Io(_)));
    }
    assert!(theirs.exists());
    assert!(!ExportJournal::path_for(t.home.path()).exists());
}
