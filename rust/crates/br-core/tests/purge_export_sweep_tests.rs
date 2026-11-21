// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{
    ExportRequest, NewReplayFrame, PurgeNotice, PurgeScope, Settings, full_path,
};
use br_core::{
    BinaryVectorIndex, CaptureExporter, EncryptedStore, ExportJournal, StoreError, VectorCandidate,
    VectorFilter, VectorIndex, win32,
};
use chrono::{Duration as ChronoDuration, Utc};

use common::{FAKE_WEBP, TempHome, TempStore, audio, screen, vec_info, vec_seeded};

const IMAGE: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 sweep4-image-bytes";

// ---- Sweep2FTests.PurgeSignalTests ----

#[test]
fn purge_last_tells_the_ids_and_the_window() {
    let t = TempStore::new();
    let seen = Arc::new(Mutex::new(Vec::<PurgeNotice>::new()));
    let s2 = seen.clone();
    t.store().on_purged(move |n| s2.lock().unwrap().push(n.clone()));

    let now = Utc::now();
    let old = t
        .store()
        .add_screen_capture(&screen(now - ChronoDuration::hours(2), "chrome.exe", "old", true))
        .expect("add");
    let recent = t
        .store()
        .add_screen_capture(&screen(now - ChronoDuration::seconds(30), "chrome.exe", "recent", true))
        .expect("add");
    let scope = PurgeScope::last(ChronoDuration::minutes(15));
    assert_eq!(t.store().purge(&scope).expect("purge"), 1);

    let list = seen.lock().unwrap();
    assert_eq!(list.len(), 1);
    let n = &list[0];
    assert_eq!(n.reason, "purge");
    assert!(n.by_user());
    assert_eq!(n.ids, vec![recent]);
    assert_eq!(n.from, scope.from);
    assert_eq!(n.to, scope.to);
    let newest = n.newest.expect("newest");
    assert!(newest >= now - ChronoDuration::seconds(31) && newest <= now - ChronoDuration::seconds(29));
    assert!(!n.ids.contains(&old));
}

#[test]
fn purge_of_an_empty_window_still_tells_so_live_buffers_are_cleared() {
    let t = TempStore::new();
    let seen = Arc::new(Mutex::new(Vec::<PurgeNotice>::new()));
    let s2 = seen.clone();
    t.store().on_purged(move |n| s2.lock().unwrap().push(n.clone()));

    t.store()
        .purge(&PurgeScope::last(ChronoDuration::minutes(5)))
        .expect("purge");
    let list = seen.lock().unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].ids.is_empty());
    assert!(list[0].from.is_some());
}

#[test]
fn scoped_purges_tell_the_ids_without_a_window() {
    for kind in ["app", "site", "id"] {
        let t = TempStore::new();
        let seen = Arc::new(Mutex::new(Vec::<PurgeNotice>::new()));
        let s2 = seen.clone();
        t.store().on_purged(move |n| s2.lock().unwrap().push(n.clone()));

        let mut cap = screen(Utc::now() - ChronoDuration::seconds(5), "signal.exe", "t", true);
        cap.site = "chat.example.org".to_string();
        let id = t.store().add_screen_capture(&cap).expect("add");
        t.store()
            .add_screen_capture(&screen(Utc::now() - ChronoDuration::seconds(4), "notepad.exe", "t", true))
            .expect("add");

        let scope = match kind {
            "app" => PurgeScope::for_app("signal.exe"),
            "site" => PurgeScope::for_site("chat.example.org"),
            _ => PurgeScope::for_id(&id),
        };
        assert_eq!(t.store().purge(&scope).expect("purge"), 1);
        let list = seen.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].ids, vec![id]);
        assert!(list[0].from.is_none());
        assert!(list[0].newest.is_some());
    }
}

#[test]
fn purge_that_matches_nothing_tells_nothing() {
    let t = TempStore::new();
    let seen = Arc::new(Mutex::new(Vec::<PurgeNotice>::new()));
    let s2 = seen.clone();
    t.store().on_purged(move |n| s2.lock().unwrap().push(n.clone()));
    t.store().purge(&PurgeScope::for_id("s_nothing")).expect("purge");
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn retention_space_drop_and_replay_deletes_tell_their_reason() {
    let t = TempStore::new();
    let seen = Arc::new(Mutex::new(Vec::<PurgeNotice>::new()));
    let s2 = seen.clone();
    t.store().on_purged(move |n| s2.lock().unwrap().push(n.clone()));

    let now = Utc::now();
    let expired = t
        .store()
        .add_screen_capture(&screen(now - ChronoDuration::days(40), "chrome.exe", "old", true))
        .expect("add");
    let mut settings = Settings::default();
    settings.retention.days = 30;
    t.store().run_maintenance(&settings, true).expect("maint");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|n| n.reason == "retention" && n.ids.contains(&expired) && !n.by_user())
    );

    let oldest = t
        .store()
        .add_screen_capture(&screen(now - ChronoDuration::days(2), "chrome.exe", "old", true))
        .expect("add");
    assert_eq!(t.store().delete_for_space(1).expect("space"), 1);
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|n| n.reason == "space" && n.ids == vec![oldest.clone()])
    );

    let nsfw = t
        .store()
        .add_screen_capture(&screen(now - ChronoDuration::seconds(3), "chrome.exe", "nsfw", true))
        .expect("add");
    t.store().drop_capture(&nsfw, "nsfw").expect("drop");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|n| n.reason == "drop" && n.ids == vec![nsfw.clone()])
    );

    let frame = t
        .store()
        .add_replay_frame(&NewReplayFrame {
            ts: now - ChronoDuration::seconds(2),
            replay_id: "rp1".to_string(),
            offset_ms: 0,
            app: "app".to_string(),
            title: "t".to_string(),
            monitor: 0,
            width: 64,
            height: 32,
            image: FAKE_WEBP.to_vec(),
            image_mime: "image/webp".to_string(),
            site: String::new(),
            captured_at: None,
        })
        .expect("replay");
    assert_eq!(t.store().delete_replay_frames("rp1").expect("del"), 1);
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|n| n.reason == "replay" && n.ids == vec![frame.clone()])
    );
    assert!(!seen.lock().unwrap().iter().any(|n| n.by_user()));
}

#[test]
fn listener_failure_does_not_break_the_purge_or_later_listeners() {
    let t = TempStore::new();
    let later = Arc::new(Mutex::new(Vec::<PurgeNotice>::new()));
    let l2 = later.clone();
    t.store().on_purged(|_| panic!("boom"));
    t.store().on_purged(move |n| l2.lock().unwrap().push(n.clone()));

    t.store()
        .add_screen_capture(&screen(Utc::now() - ChronoDuration::seconds(5), "chrome.exe", "x", true))
        .expect("add");
    assert_eq!(
        t.store()
            .purge(&PurgeScope::last(ChronoDuration::minutes(1)))
            .expect("purge"),
        1
    );
    assert_eq!(later.lock().unwrap().len(), 1);
}

#[test]
fn range_purge_ending_now_fences_its_window_only() {
    let t = TempStore::new();
    let now = Utc::now();
    t.store()
        .purge(&PurgeScope::range(now - ChronoDuration::hours(1), now))
        .expect("purge");
    assert!(matches!(
        t.store().add_screen_capture(&screen(now - ChronoDuration::seconds(10), "chrome.exe", "x", true)),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(matches!(
        t.store().add_audio_capture(&audio(now - ChronoDuration::minutes(30), "late transcript", "teams.exe")),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(
        t.store()
            .add_screen_capture(&screen(now - ChronoDuration::hours(2), "chrome.exe", "x", true))
            .is_ok()
    );
    assert!(
        t.store()
            .add_screen_capture(&screen(Utc::now() + ChronoDuration::seconds(1), "chrome.exe", "x", true))
            .is_ok()
    );
}

#[test]
fn range_purge_ending_in_the_future_fences_up_to_now() {
    let t = TempStore::new();
    let now = Utc::now();
    t.store()
        .purge(&PurgeScope::range(
            now - ChronoDuration::hours(1),
            now + ChronoDuration::hours(5),
        ))
        .expect("purge");
    assert!(matches!(
        t.store().add_screen_capture(&screen(now - ChronoDuration::seconds(1), "chrome.exe", "x", true)),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(
        t.store()
            .add_screen_capture(&screen(Utc::now() + ChronoDuration::seconds(2), "chrome.exe", "x", true))
            .is_ok()
    );
}

#[test]
fn old_range_purge_sets_no_fence() {
    let t = TempStore::new();
    let now = Utc::now();
    t.store()
        .purge(&PurgeScope::range(
            now - ChronoDuration::days(3),
            now - ChronoDuration::days(2),
        ))
        .expect("purge");
    assert!(
        t.store()
            .add_screen_capture(&screen(
                now - ChronoDuration::days(2) - ChronoDuration::hours(1),
                "chrome.exe",
                "x",
                true,
            ))
            .is_ok()
    );
}

#[test]
fn fence_expires_by_the_monotonic_clock_after_a_wall_clock_step_back() {
    let t = TempStore::new();
    let mono = Arc::new(AtomicI64::new(1_000_000));
    let m2 = mono.clone();
    t.store().set_mono_ms_for_test(move || m2.load(Ordering::Relaxed));

    let now = Utc::now();
    t.store()
        .purge(&PurgeScope::last(ChronoDuration::minutes(10)))
        .expect("purge");
    let stepped = now - ChronoDuration::minutes(3);
    assert!(matches!(
        t.store().add_screen_capture(&screen(stepped, "chrome.exe", "x", true)),
        Err(StoreError::PurgedWindow(_))
    ));
    mono.fetch_add(EncryptedStore::FENCE_LIFETIME_MS, Ordering::Relaxed);
    assert!(
        t.store()
            .add_screen_capture(&screen(stepped, "chrome.exe", "x", true))
            .is_ok()
    );
}

#[test]
fn replay_carry_in_sample_restamped_after_the_fence_is_refused_by_its_grab_time() {
    let t = TempStore::new();
    let now = Utc::now();
    t.store()
        .purge(&PurgeScope::last(ChronoDuration::minutes(5)))
        .expect("purge");
    let mut frame = NewReplayFrame {
        ts: Utc::now() + ChronoDuration::seconds(1),
        replay_id: "rp2".to_string(),
        offset_ms: 0,
        app: "app".to_string(),
        title: "t".to_string(),
        monitor: 0,
        width: 64,
        height: 32,
        image: FAKE_WEBP.to_vec(),
        image_mime: "image/webp".to_string(),
        site: String::new(),
        captured_at: Some(now - ChronoDuration::seconds(20)),
    };
    assert!(matches!(
        t.store().add_replay_frame(&frame),
        Err(StoreError::PurgedWindow(_))
    ));
    frame.captured_at = None;
    assert!(t.store().add_replay_frame(&frame).is_ok());
}

// ---- Sweep3JCoreTests ----

#[test]
fn b11_maintenance_that_throws_still_raises_the_retention_notice() {
    let t = TempStore::new();
    let old = t
        .store()
        .add_audio_capture(&audio(Utc::now() - ChronoDuration::days(40), "old secret", "teams.exe"))
        .expect("audio");
    let notices = Arc::new(Mutex::new(Vec::<PurgeNotice>::new()));
    let n2 = notices.clone();
    t.store().on_purged(move |n| n2.lock().unwrap().push(n.clone()));

    let kf_dir = t.home.path().join("keyframes");
    fs::create_dir_all(&kf_dir).expect("mkdir");
    let mut junk = vec![0u8; 8192];
    win32::fill_random(&mut junk);
    fs::write(kf_dir.join("2099-01.db"), &junk).expect("write junk");

    let mut s = Settings::default();
    s.retention.days = 30;
    assert!(t.store().run_maintenance(&s, true).is_err());
    assert!(t.store().get(&old).expect("g").is_none());
    let list = notices.lock().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason, "retention");
    assert!(list[0].ids.contains(&old));
}

#[test]
fn b12_export_aborts_when_a_purge_reaches_it() {
    let t = TempStore::new();
    let out = TempHome::new(None);
    let mut c1 = screen(Utc::now() - ChronoDuration::minutes(4), "chrome.exe", "FIRST-ROW-SECRET", true);
    c1.image = IMAGE.to_vec();
    let a = t.store().add_screen_capture(&c1).expect("a");
    t.store().set_embedding(&a, &vec_seeded(5), &vec_info()).expect("e1");
    let mut c2 = screen(Utc::now() - ChronoDuration::minutes(3), "chrome.exe", "second row", true);
    c2.image = IMAGE.to_vec();
    let b = t.store().add_screen_capture(&c2).expect("b");
    t.store().set_embedding(&b, &vec_seeded(6), &vec_info()).expect("e2");

    let path = out.path().join("out.zip");
    let mut calls = 0usize;
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
        |kf, _| {
            if calls == 0 {
                let _ = t.store().purge(&PurgeScope::last(ChronoDuration::minutes(10)));
            }
            calls += 1;
            kf
        },
        None,
    )
    .expect_err("must abort");
    assert!(matches!(err, StoreError::ExportPurged), "got {err:?}");
    assert!(t.store().get(&a).expect("g").is_none());
    assert!(!path.exists());
    assert!(!out.path().join("out.zip.partial").exists());
}

#[test]
fn b12_unrelated_purge_does_not_abort_an_export() {
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

#[test]
fn b19_purged_rows_leave_no_bits_or_names_in_the_ram_index() {
    let index = Arc::new(BinaryVectorIndex::new());
    let home = TempHome::new(None);
    let mut key = vec![0u8; 32];
    win32::fill_random(&mut key);
    let k2 = key.clone();
    let store = EncryptedStore::open(home.path(), move || k2.clone(), Some(index.clone()))
        .expect("open");

    let keep = store
        .add_screen_capture(&screen(
            Utc::now() - ChronoDuration::minutes(3),
            "notepad.exe",
            "kept",
            true,
        ))
        .expect("add");
    store.set_embedding(&keep, &vec_seeded(41), &vec_info()).expect("e1");

    let mut sec = screen(
        Utc::now() - ChronoDuration::minutes(2),
        "tor.exe",
        "secret site",
        true,
    );
    sec.site = "secret-site.example".to_string();
    let id = store.add_screen_capture(&sec).expect("add");
    store.set_embedding(&id, &vec_seeded(42), &vec_info()).expect("e2");

    let before = index.name_counts();
    store
        .purge(&PurgeScope::for_site("secret-site.example"))
        .expect("purge");
    assert_eq!(index.count(), 1);
    assert_eq!(index.name_counts(), (before.0 - 1, before.1 - 1));
    assert_eq!(index.site_id("secret-site.example"), -2);
    assert_eq!(index.app_id("tor.exe"), -2);
    assert_ne!(index.app_id("notepad.exe"), -2);
    assert!(!index.has_stale_bits(vec_seeded(42).len()));
}

#[test]
fn b19_compaction_clears_the_tail() {
    let index = BinaryVectorIndex::new();
    let v = vec![1.0f32; 64];
    for r in 1..=6000i64 {
        index.upsert(r, &v, r, 0, &format!("app{}", r % 7), "");
    }
    for r in 1..=5000i64 {
        index.remove(r);
    }
    assert_eq!(index.count(), 1000);
    assert!(!index.has_stale_bits(64));
    assert_eq!(index.name_counts().0, 7);
    assert_eq!(index.scan(&v, VectorFilter::ALL, 1).len(), 1);
}

// ---- Sweep4PCoreTests ----

#[test]
fn a_h1_store_after_purge_all_refuses_captures_stamped_before_it() {
    let mut t = TempStore::new();
    let grabbed = Utc::now() - ChronoDuration::seconds(3);
    t.store().delete_all_files();
    let purged_at = Utc::now();
    t.reopen();
    t.store().fence_up_to(purged_at).expect("fence");

    assert!(matches!(
        t.store().add_screen_capture(&screen(grabbed, "chrome.exe", "x", true)),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(matches!(
        t.store().add_audio_capture(&audio(grabbed, "pre purge marker alpha", "teams.exe")),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(matches!(
        t.store().add_replay_frame(&NewReplayFrame {
            ts: grabbed,
            replay_id: "r1".to_string(),
            offset_ms: 0,
            app: "chrome".to_string(),
            title: "t".to_string(),
            monitor: 0,
            width: 64,
            height: 36,
            image: IMAGE.to_vec(),
            image_mime: "image/webp".to_string(),
            site: String::new(),
            captured_at: None,
        }),
        Err(StoreError::PurgedWindow(_))
    ));
    assert_eq!(
        t.store()
            .count_purge(&PurgeScope {
                kind: "all".to_string(),
                from: None,
                to: None,
                app: None,
                site: None,
                id: None,
            })
            .expect("c"),
        0
    );
    assert!(
        t.store()
            .add_screen_capture(&screen(Utc::now() + ChronoDuration::seconds(1), "chrome.exe", "x", true))
            .is_ok()
    );
}

#[test]
fn c1_purge_all_during_the_image_phase_aborts_the_export() {
    let t = TempStore::new();
    let out = TempHome::new(None);
    for (m, seed) in [(4, 3), (3, 4)] {
        let mut c = screen(Utc::now() - ChronoDuration::minutes(m), "chrome.exe", "row", true);
        c.image = IMAGE.to_vec();
        let id = t.store().add_screen_capture(&c).expect("add");
        t.store().set_embedding(&id, &vec_seeded(seed), &vec_info()).expect("embed");
    }
    let path = out.path().join("img.zip");
    let mut calls = 0usize;
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
        |kf, _| {
            if calls == 0 {
                t.store().close_for_purge_all();
            }
            calls += 1;
            kf
        },
        None,
    )
    .expect_err("must abort");
    assert!(matches!(err, StoreError::ExportPurged));
    assert!(!path.exists());
    assert!(!out.path().join("img.zip.partial").exists());
}

#[test]
fn c1_purge_all_during_the_json_phase_does_not_publish_the_zip() {
    let t = TempStore::new();
    let out = TempHome::new(None);
    let mut c = screen(Utc::now() - ChronoDuration::minutes(4), "chrome.exe", "row", true);
    c.image = IMAGE.to_vec();
    let id = t.store().add_screen_capture(&c).expect("add");
    t.store().set_embedding(&id, &vec_seeded(5), &vec_info()).expect("embed");

    let path = out.path().join("json.zip");
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
        |kf, _| {
            t.store().close_for_purge_all();
            kf
        },
        None,
    )
    .expect_err("must abort");
    assert!(matches!(err, StoreError::ExportPurged));
    assert!(!path.exists());
    assert!(!out.path().join("json.zip.partial").exists());
}

#[test]
fn a_m2_app_purge_refuses_frames_of_that_app_grabbed_before_it() {
    let t = TempStore::new();
    let before = Utc::now() - ChronoDuration::seconds(25);
    let id = t
        .store()
        .add_screen_capture(&screen(before - ChronoDuration::minutes(10), "notepad.exe", "row", true))
        .expect("add");
    t.store().set_embedding(&id, &vec_seeded(1), &vec_info()).expect("e");
    t.store().purge(&PurgeScope::for_app("notepad")).expect("purge");

    assert!(matches!(
        t.store().add_screen_capture(&screen(before, "notepad.exe", "t", true)),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(matches!(
        t.store().add_screen_capture(&screen(before, "Notepad", "t", true)),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(matches!(
        t.store().add_audio_capture(&audio(before, "said in notepad", "notepad")),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(matches!(
        t.store().add_replay_frame(&NewReplayFrame {
            ts: before,
            replay_id: "r1".to_string(),
            offset_ms: 0,
            app: "notepad".to_string(),
            title: "t".to_string(),
            monitor: 0,
            width: 64,
            height: 36,
            image: IMAGE.to_vec(),
            image_mime: "image/webp".to_string(),
            site: String::new(),
            captured_at: None,
        }),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(t.store().add_screen_capture(&screen(before, "chrome.exe", "t", true)).is_ok());
    assert!(
        t.store()
            .add_screen_capture(&screen(Utc::now() + ChronoDuration::seconds(1), "notepad.exe", "t", true))
            .is_ok()
    );
}

#[test]
fn a_m2_site_purge_refuses_frames_of_that_site_grabbed_before_it() {
    let t = TempStore::new();
    let before = Utc::now() - ChronoDuration::seconds(5);
    t.store().purge(&PurgeScope::for_site("www.Example.com")).expect("purge");

    let mut c1 = screen(before, "msedge.exe", "t", true);
    c1.site = "example.com".to_string();
    assert!(matches!(
        t.store().add_screen_capture(&c1),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(matches!(
        t.store().add_replay_frame(&NewReplayFrame {
            ts: before,
            replay_id: "r1".to_string(),
            offset_ms: 0,
            app: "msedge".to_string(),
            title: "t".to_string(),
            monitor: 0,
            width: 64,
            height: 36,
            image: IMAGE.to_vec(),
            image_mime: "image/webp".to_string(),
            site: "www.example.com".to_string(),
            captured_at: None,
        }),
        Err(StoreError::PurgedWindow(_))
    ));
    let mut c2 = screen(before, "msedge.exe", "t", true);
    c2.site = "other.org".to_string();
    assert!(t.store().add_screen_capture(&c2).is_ok());
    let mut c3 = screen(Utc::now() + ChronoDuration::seconds(1), "msedge.exe", "t", true);
    c3.site = "example.com".to_string();
    assert!(t.store().add_screen_capture(&c3).is_ok());
}

#[test]
fn a_m2_app_purge_notice_names_its_scope_even_with_no_row() {
    let t = TempStore::new();
    let notices = Arc::new(Mutex::new(Vec::<PurgeNotice>::new()));
    let n2 = notices.clone();
    t.store().on_purged(move |n| n2.lock().unwrap().push(n.clone()));

    t.store().purge(&PurgeScope::for_app("notepad")).expect("purge");
    {
        let list = notices.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].scope.as_deref(), Some("app"));
        assert!(list[0].ids.is_empty());
    }
    t.store().purge(&PurgeScope::for_id("s_nope")).expect("purge");
    assert_eq!(notices.lock().unwrap().len(), 1);
}

#[test]
fn a_m2_app_fence_expires_like_the_time_fences() {
    let t = TempStore::new();
    let mono = Arc::new(AtomicI64::new(0));
    let m2 = mono.clone();
    t.store().set_mono_ms_for_test(move || m2.load(Ordering::Relaxed));

    let before = Utc::now() - ChronoDuration::seconds(5);
    t.store().purge(&PurgeScope::for_app("notepad")).expect("purge");
    mono.fetch_add(EncryptedStore::FENCE_LIFETIME_MS, Ordering::Relaxed);
    assert!(t.store().add_screen_capture(&screen(before, "notepad.exe", "t", true)).is_ok());
}

#[test]
fn a_l6_audio_row_overlapping_a_purged_range_is_refused() {
    let t = TempStore::new();
    let now = Utc::now();
    let from = now - ChronoDuration::seconds(5);
    t.store()
        .purge(&PurgeScope::range(from, now + ChronoDuration::minutes(1)))
        .expect("purge");
    assert!(matches!(
        t.store().add_audio_capture(&audio(
            from - ChronoDuration::milliseconds(600),
            "spoken across the start",
            "teams.exe",
        )),
        Err(StoreError::PurgedWindow(_))
    ));
    assert!(
        t.store()
            .add_audio_capture(&audio(
                from - ChronoDuration::seconds(10),
                "before the window",
                "teams.exe",
            ))
            .is_ok()
    );
}

#[test]
fn c7_purge_last_deletes_rows_stamped_in_the_future() {
    let t = TempStore::new();
    let future = t
        .store()
        .add_screen_capture(&screen(Utc::now() + ChronoDuration::hours(2), "chrome.exe", "f", true))
        .expect("f");
    t.store().set_embedding(&future, &vec_seeded(9), &vec_info()).expect("e1");
    let old = t
        .store()
        .add_screen_capture(&screen(Utc::now() - ChronoDuration::hours(1), "chrome.exe", "o", true))
        .expect("o");
    t.store().set_embedding(&old, &vec_seeded(10), &vec_info()).expect("e2");

    assert_eq!(t.store().count_purge(&PurgeScope::last(ChronoDuration::minutes(5))).expect("c"), 1);
    assert_eq!(t.store().purge(&PurgeScope::last(ChronoDuration::minutes(5))).expect("p"), 1);
    assert!(t.store().get(&future).expect("g").is_none());
    assert!(t.store().get(&old).expect("g").is_some());
    assert!(
        t.store()
            .add_screen_capture(&screen(Utc::now() + ChronoDuration::seconds(1), "chrome.exe", "n", true))
            .is_ok()
    );
}

#[test]
fn c7_panic_with_zero_minutes_still_deletes_nothing() {
    let t = TempStore::new();
    let future = t
        .store()
        .add_screen_capture(&screen(Utc::now() + ChronoDuration::hours(2), "chrome.exe", "f", true))
        .expect("f");
    t.store().set_embedding(&future, &vec_seeded(11), &vec_info()).expect("e");
    assert_eq!(t.store().purge(&PurgeScope::last(ChronoDuration::zero())).expect("p"), 0);
    assert!(t.store().get(&future).expect("g").is_some());
}

#[test]
fn c3_leftover_export_partial_is_deleted_at_sweep() {
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

#[test]
fn c3_capture_export_lists_its_partial_while_it_runs() {
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

#[test]
fn c3_failed_export_leaves_a_file_it_did_not_create() {
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

// ---- Sweep5UTests store/export cases ----

#[test]
fn b1_capture_exporter_aborts_when_gate_locks_mid_export_and_deletes_partial() {
    let t = TempStore::new();
    let out = TempHome::new(None);
    let t0 = Utc::now() - ChronoDuration::minutes(10);
    let a = t.store().add_screen_capture(&screen(t0, "notepad.exe", "one", true)).expect("a");
    let b = t
        .store()
        .add_screen_capture(&screen(t0 + ChronoDuration::seconds(10), "notepad.exe", "two", true))
        .expect("b");
    t.store().set_embedding(&a, &vec_seeded(1), &vec_info()).expect("e1");
    t.store().set_embedding(&b, &vec_seeded(2), &vec_info()).expect("e2");

    let zip = out.path().join("b1-lock.zip");
    let req = ExportRequest {
        from: t0 - ChronoDuration::minutes(1),
        to: t0 + ChronoDuration::minutes(5),
        path: zip.to_string_lossy().into_owned(),
        app: None,
        jpeg: false,
        dry_run: false,
    };
    let mut checks = 0usize;
    let mut locked = false;
    let err = CaptureExporter::write_with_lock(
        t.store(),
        &req,
        |k, _| k,
        Some(|| {
            checks += 1;
            if checks >= 2 {
                locked = true;
            }
            locked
        }),
        None,
    )
    .expect_err("must lock");
    assert!(matches!(err, StoreError::ExportLocked));
    assert!(!zip.exists());
    assert!(!out.path().join("b1-lock.zip.partial").exists());
}

#[test]
fn c4_capture_exporter_checks_cancellation_inside_captures_json_before_move() {
    let t = TempStore::new();
    let out = TempHome::new(None);
    let t0 = Utc::now() - ChronoDuration::minutes(2);
    let cap_id = t
        .store()
        .add_screen_capture(&screen(t0, "notepad.exe", "last capture", true))
        .expect("add");
    t.store().set_embedding(&cap_id, &vec_seeded(1), &vec_info()).expect("embed");

    let zip = out.path().join("c4-cancel.zip");
    let req = ExportRequest {
        from: t0 - ChronoDuration::minutes(1),
        to: t0 + ChronoDuration::minutes(1),
        path: zip.to_string_lossy().into_owned(),
        app: None,
        jpeg: true,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);
    let err = CaptureExporter::write(
        t.store(),
        &req,
        |kf, _| {
            cancel.store(true, Ordering::Release);
            kf
        },
        Some(&cancel),
    )
    .expect_err("must cancel");
    assert!(matches!(err, StoreError::Cancelled));
    assert!(!zip.exists());
    assert!(!out.path().join("c4-cancel.zip.partial").exists());
}

struct BlockingIndex {
    entered: (Mutex<bool>, Condvar),
    release: (Mutex<bool>, Condvar),
    on_remove: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl BlockingIndex {
    fn new() -> Self {
        Self {
            entered: (Mutex::new(false), Condvar::new()),
            release: (Mutex::new(true), Condvar::new()),
            on_remove: Mutex::new(None),
        }
    }

    fn reset_release(&self) {
        *self.release.0.lock().unwrap() = false;
    }

    fn set_release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }

    fn wait_entered(&self, timeout: Duration) -> bool {
        let g = self.entered.0.lock().unwrap();
        let (g, _) = self.entered.1.wait_timeout_while(g, timeout, |v| !*v).unwrap();
        *g
    }
}

impl VectorIndex for BlockingIndex {
    fn count(&self) -> usize {
        0
    }
    fn upsert(&self, _: i64, _: &[f32], _: i64, _: i32, _: &str, _: &str) {
        *self.entered.0.lock().unwrap() = true;
        self.entered.1.notify_all();
        let g = self.release.0.lock().unwrap();
        let _ = self
            .release
            .1
            .wait_timeout_while(g, Duration::from_secs(10), |v| !*v)
            .unwrap();
    }
    fn remove(&self, _: i64) {
        if let Some(ref cb) = *self.on_remove.lock().unwrap() {
            cb();
        }
    }
    fn clear(&self) {}
    fn scan(&self, _: &[f32], _: VectorFilter, _: usize) -> Vec<VectorCandidate> {
        Vec::new()
    }
    fn app_id(&self, _: &str) -> i32 {
        0
    }
}

#[test]
fn c4_encrypted_store_cancel_maintenance_and_bounded_checkpoint_do_not_deadlock() {
    let home = TempHome::new(None);
    let mut key = vec![0u8; 32];
    win32::fill_random(&mut key);
    let index = Arc::new(BlockingIndex::new());
    let k2 = key.clone();
    let store = Arc::new(
        EncryptedStore::open(home.path(), move || k2.clone(), Some(index.clone())).expect("open"),
    );
    let s_cancel = store.clone();
    *index.on_remove.lock().unwrap() = Some(Box::new(move || s_cancel.cancel_maintenance()));

    let id = store
        .add_screen_capture(&screen(
            Utc::now() - ChronoDuration::days(60),
            "notepad.exe",
            "old",
            true,
        ))
        .expect("add");
    index.reset_release();

    let s_embed = store.clone();
    let id_embed = id.clone();
    let embed_thread = thread::spawn(move || {
        s_embed
            .set_embedding(&id_embed, &vec_seeded(1), &vec_info())
            .expect("embed")
    });
    assert!(index.wait_entered(Duration::from_secs(5)));

    assert!(!store.checkpoint_timeout(Duration::from_millis(40)).expect("cp"));
    index.set_release();
    embed_thread.join().expect("join");
    assert!(store.checkpoint_timeout(Duration::from_secs(5)).expect("cp"));

    let notice = Arc::new(Mutex::new(None::<PurgeNotice>));
    let n2 = notice.clone();
    store.on_purged(move |n| *n2.lock().unwrap() = Some(n.clone()));
    let mut settings = Settings::default();
    settings.retention.days = 30;
    store.run_maintenance(&settings, true).expect("maint");
    let got = notice.lock().unwrap().clone().expect("notice");
    assert!(got.ids.contains(&id));
}

// ---- BatchHCoreTests B20 ----

#[test]
fn b20_history_needles() {
    assert_eq!(
        EncryptedStore::history_needles(&PurgeScope::for_site("www.pornhub.com")),
        vec!["pornhub.com", "pornhub"]
    );
    assert_eq!(
        EncryptedStore::history_needles(&PurgeScope::for_site("news.bbc.co.uk")),
        vec!["news.bbc.co.uk", "bbc"]
    );
    assert_eq!(
        EncryptedStore::history_needles(&PurgeScope::for_app("chrome.exe")),
        vec!["chrome.exe", "chrome"]
    );
}

#[test]
fn b20_app_and_site_purges_forget_matching_saved_searches() {
    let t = TempStore::new();
    let now = Utc::now();
    for q in ["pornhub", "PornHub videos", "chrome tips", "weather", "acme invoice"] {
        t.store().add_history(q, now, 100).expect("add");
    }
    t.store().purge(&PurgeScope::for_site("www.pornhub.com")).expect("p");
    let mut h: Vec<String> = t.store().history(100).expect("h").into_iter().map(|x| x.q).collect();
    h.sort();
    assert_eq!(h, vec!["acme invoice", "chrome tips", "weather"]);

    t.store().purge(&PurgeScope::for_app("chrome.exe")).expect("p");
    let mut h2: Vec<String> = t.store().history(100).expect("h").into_iter().map(|x| x.q).collect();
    h2.sort();
    assert_eq!(h2, vec!["acme invoice", "weather"]);
}
