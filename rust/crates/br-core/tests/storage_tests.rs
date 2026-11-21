// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs::{self, File};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{
    DataPaths, ExportRequest, ExportResult, NewScreenCapture, PurgeScope, RetentionSettings,
    SearchMode, SearchRequest, Settings,
};
use br_core::{CaptureExporter, EncryptedStore};
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Timelike, Utc};
use zip::ZipArchive;

use common::{
    FAKE_WEBP, TempStore, audio, hex_upper, screen, seeded_bytes, vec_info, vec_seeded,
};

fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 12, 12, 0, 0).unwrap()
}

fn add_scored(
    s: &EncryptedStore,
    ts: DateTime<Utc>,
    change: Option<f64>,
    sw: bool,
    image: Option<Vec<u8>>,
) -> String {
    let cap = NewScreenCapture {
        ts,
        app: "code".to_string(),
        exe_path: r"C:\code.exe".to_string(),
        title: "main.cs".to_string(),
        monitor: 0,
        width: 1280,
        height: 720,
        d_hash: 0x1234,
        image: image.unwrap_or_else(|| FAKE_WEBP.to_vec()),
        image_mime: "image/webp".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change,
        switch: sw,
        text: None,
    };
    let id = s.add_screen_capture(&cap).expect("add");
    s.take_pending_embeddings(10).expect("take");
    let seed = (ts.second() + ts.minute() * 60) as i32;
    s.set_embedding(&id, &vec_seeded(seed), &vec_info()).expect("embed");
    id
}

#[test]
fn change_score_and_switch_flag_are_stored() {
    let mut t = TempStore::new();
    let now = fixed_now();
    let a = add_scored(t.store(), now, Some(42.5), true, None);
    let b = t
        .store()
        .add_screen_capture(&screen(now + ChronoDuration::seconds(2), "chrome.exe", "Some window", true))
        .expect("add");
    assert_eq!(t.store().stored_score(&a).expect("s"), (Some(42.5), Some(true)));
    assert_eq!(t.store().stored_score(&b).expect("s"), (None, None));
    t.reopen();
    assert_eq!(t.store().stored_score(&a).expect("s"), (Some(42.5), Some(true)));
}

#[test]
fn thin_target_follows_the_period() {
    assert_eq!(EncryptedStore::thin_target(10, 0.0), 10);
    assert_eq!(EncryptedStore::thin_target(10, 0.5), 5);
    assert_eq!(EncryptedStore::thin_target(10, 0.95), 1);
    assert_eq!(EncryptedStore::thin_target(10, 1.0), 0);
}

#[test]
fn thinning_drops_smallest_changes_first_and_keeps_switches() {
    let t = TempStore::new();
    let now = fixed_now();
    let r = RetentionSettings {
        days: 365,
        progressive_after_days: 7,
        progressive_over_days: 10,
    };
    let day = Utc.with_ymd_and_hms(2026, 11, 12, 0, 0, 0).unwrap() - ChronoDuration::days(13);
    let mut ids = Vec::new();
    for i in 0..20 {
        let ch = f64::from(i) * 5.0;
        let id = add_scored(
            t.store(),
            day + ChronoDuration::minutes(i64::from(i) + 1),
            Some(ch),
            false,
            None,
        );
        ids.push((id, ch));
    }
    let sw = add_scored(t.store(), day + ChronoDuration::minutes(30), Some(0.1), true, None);
    let fresh = add_scored(t.store(), now - ChronoDuration::days(1), Some(0.1), false, None);

    let deleted = t.store().thin_for_test(&r, now).expect("thin");
    assert_eq!(deleted, 12);
    for (id, change) in &ids {
        assert_eq!(*change >= 60.0, t.store().get(id).expect("g").is_some());
    }
    assert!(t.store().get(&sw).expect("g").is_some());
    assert!(t.store().get(&fresh).expect("g").is_some());
    assert_eq!(t.store().thin_for_test(&r, now).expect("thin 2"), 0);
    assert_eq!(
        t.store()
            .thin_for_test(&r, now + ChronoDuration::days(5))
            .expect("thin 3"),
        8
    );
    assert!(t.store().get(&sw).expect("g").is_some());
}

#[test]
fn thinning_off_and_legacy_rows_scored_from_dhash() {
    let t = TempStore::new();
    let now = fixed_now();
    let day = Utc.with_ymd_and_hms(2026, 11, 12, 0, 0, 0).unwrap() - ChronoDuration::days(20);
    let mut legacy = Vec::new();
    for i in 0..4 {
        let id = t
            .store()
            .add_screen_capture(&screen(
                day + ChronoDuration::minutes(i + 1),
                "code",
                "same",
                true,
            ))
            .expect("add");
        t.store().take_pending_embeddings(10).expect("take");
        t.store().set_embedding(&id, &vec_seeded(i as i32), &vec_info()).expect("embed");
        legacy.push(id);
    }
    assert_eq!(
        t.store()
            .thin_for_test(
                &RetentionSettings {
                    days: 365,
                    progressive_after_days: 0,
                    progressive_over_days: 30,
                },
                now,
            )
            .expect("thin off"),
        0
    );
    assert_eq!(
        t.store()
            .thin_for_test(
                &RetentionSettings {
                    days: 365,
                    progressive_after_days: 1,
                    progressive_over_days: 1,
                },
                now,
            )
            .expect("thin on"),
        3
    );
    assert!(t.store().get(&legacy[0]).expect("g").is_some());
}

#[test]
fn delete_for_space_goes_oldest_day_first_in_progressive_order() {
    let t = TempStore::new();
    let now = fixed_now();
    let d1 = Utc.with_ymd_and_hms(2026, 11, 12, 0, 0, 0).unwrap() - ChronoDuration::days(3);
    let old_switch = add_scored(t.store(), d1 + ChronoDuration::hours(1), Some(90.0), true, None);
    let old_big = add_scored(t.store(), d1 + ChronoDuration::hours(2), Some(80.0), false, None);
    let old_small = add_scored(t.store(), d1 + ChronoDuration::hours(3), Some(1.0), false, None);
    let aud = t
        .store()
        .add_audio_capture(&audio(d1 + ChronoDuration::hours(4), "hello there", "teams.exe"))
        .expect("audio");
    let newer = add_scored(t.store(), now - ChronoDuration::hours(1), Some(0.0), false, None);

    assert_eq!(t.store().delete_for_space(1).expect("d"), 1);
    assert!(t.store().get(&old_small).expect("g").is_none());
    assert_eq!(t.store().delete_for_space(1).expect("d"), 1);
    assert!(t.store().get(&old_big).expect("g").is_none());
    assert_eq!(t.store().delete_for_space(2).expect("d"), 2);
    assert!(t.store().get(&old_switch).expect("g").is_none());
    assert!(t.store().get(&aud).expect("g").is_none());
    assert!(t.store().get(&newer).expect("g").is_some());
    assert_eq!(t.store().delete_for_space(10).expect("d"), 1);
    assert_eq!(t.store().delete_for_space(10).expect("d"), 0);
}

#[test]
fn storage_use_counts_free_pages_and_empty_month_files_go() {
    let t = TempStore::new();
    let now = fixed_now();
    let big = seeded_bytes(3, 300_000);
    let old_month = now - ChronoDuration::days(70);
    for i in 0..5 {
        add_scored(
            t.store(),
            old_month + ChronoDuration::minutes(i),
            Some(i as f64),
            false,
            Some(big.clone()),
        );
    }
    for i in 0..5 {
        add_scored(
            t.store(),
            now - ChronoDuration::minutes(i + 1),
            Some(i as f64),
            false,
            Some(big.clone()),
        );
    }
    t.store().checkpoint().expect("cp");
    let before = t.store().get_storage_use().expect("use");
    assert!(before.live_bytes() > 2_500_000);
    let old_file = DataPaths::keyframes_dir(t.home.path())
        .join(format!("{}.db", old_month.format("%Y-%m")));
    assert!(old_file.exists());

    assert_eq!(t.store().delete_for_space(5).expect("d"), 5);
    assert!(!old_file.exists());
    assert_eq!(t.store().delete_for_space(2).expect("d"), 2);
    t.store().checkpoint().expect("cp");
    let after = t.store().get_storage_use().expect("use");
    assert!(
        after.live_bytes() < before.live_bytes() - 1_800_000,
        "{} vs {}",
        after.live_bytes(),
        before.live_bytes()
    );
    assert!(
        after.file_bytes < before.file_bytes - 1_800_000,
        "{} vs {}",
        after.file_bytes,
        before.file_bytes
    );
}

#[test]
fn maintenance_runs_thinning_with_settings() {
    let t = TempStore::new();
    let old = Utc::now() - ChronoDuration::days(25);
    let a = add_scored(t.store(), old, Some(1.0), false, None);
    let b = add_scored(t.store(), old + ChronoDuration::minutes(1), Some(50.0), true, None);
    let mut s = Settings::default();
    s.retention.days = 30;
    s.retention.progressive_after_days = 7;
    s.retention.progressive_over_days = 10;
    t.store().run_maintenance(&s, true).expect("maint");
    assert!(t.store().get(&a).expect("g").is_none());
    assert!(t.store().get(&b).expect("g").is_some());
}

// ---- ExportTests ----

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

// ---- Sweep3KTests.StoreCompactionSweep3Tests & SearchSweep3Tests ----

fn legacy_month(t: &TempStore, count: usize, keep: usize) -> (String, Vec<(String, Vec<u8>)>) {
    let now = Utc::now();
    let month = now.format("%Y-%m").to_string();
    let mut images = Vec::new();
    for i in 0..count {
        let img = seeded_bytes(i as i32, 60_000);
        let mut cap = screen(
            now - ChronoDuration::seconds((count - i) as i64),
            "chrome.exe",
            "Some window",
            true,
        );
        cap.image = img.clone();
        let id = t.store().add_screen_capture(&cap).expect("add");
        t.store().take_pending_embeddings(10).expect("take");
        t.store().set_embedding(&id, &vec_seeded(i as i32), &vec_info()).expect("embed");
        images.push((id, img));
    }
    t.store()
        .exec_month_for_test(&month, "PRAGMA auto_vacuum=NONE; VACUUM;")
        .expect("legacy vacuum");
    for (id, _) in images.iter().take(count - keep) {
        assert_eq!(t.store().purge(&PurgeScope::for_id(id)).expect("purge"), 1);
    }
    t.store().checkpoint().expect("cp");
    t.store().set_legacy_rebuild_min_free_bytes(256 * 1024);
    (month, images.into_iter().skip(count - keep).collect())
}

fn has_tmp_files(dir: &std::path::Path) -> bool {
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains(".tmp"))
}

#[test]
fn legacy_month_file_is_rebuilt_without_vacuum() {
    let mut t = TempStore::new();
    let now = Utc::now();
    let month = now.format("%Y-%m").to_string();
    let mut images = Vec::new();
    for i in 0..12 {
        let img = seeded_bytes(i, 200_000);
        let mut cap = screen(
            now - ChronoDuration::seconds(i64::from(i) + 1),
            "chrome.exe",
            "Some window",
            true,
        );
        cap.image = img.clone();
        let id = t.store().add_screen_capture(&cap).expect("add");
        t.store().take_pending_embeddings(10).expect("take");
        t.store().set_embedding(&id, &vec_seeded(i), &vec_info()).expect("embed");
        images.push((id, img));
    }
    t.store()
        .exec_month_for_test(&month, "PRAGMA auto_vacuum=NONE; VACUUM;")
        .expect("none");
    assert_eq!(
        t.store().query_month_i64_for_test(&month, "PRAGMA auto_vacuum").expect("q"),
        Some(0)
    );
    for (id, _) in images.iter().take(9) {
        assert_eq!(t.store().purge(&PurgeScope::for_id(id)).expect("p"), 1);
    }
    t.store().checkpoint().expect("cp");
    let path = DataPaths::keyframes_dir(t.home.path()).join(format!("{month}.db"));
    let before = fs::metadata(&path).expect("meta").len();
    assert!(
        t.store()
            .query_month_i64_for_test(&month, "PRAGMA freelist_count")
            .expect("q")
            .unwrap_or(0)
            > 0
    );

    t.store().set_legacy_rebuild_min_free_bytes(256 * 1024);
    let mut s = Settings::default();
    s.retention.days = 3650;
    t.store().run_maintenance(&s, true).expect("maint");

    let after = fs::metadata(&path).expect("meta").len();
    assert!(after < before / 2, "{after} vs {before}");
    assert!(!has_tmp_files(&DataPaths::keyframes_dir(t.home.path())));
    for (id, bytes) in images.iter().skip(9) {
        assert_eq!(t.store().get_keyframe(id).expect("kf").expect("s").bytes, *bytes);
    }
    assert_eq!(
        t.store().query_month_i64_for_test(&month, "PRAGMA auto_vacuum").expect("q"),
        Some(2)
    );
    t.reopen();
    for (id, bytes) in images.iter().skip(9) {
        assert_eq!(t.store().get_keyframe(id).expect("kf").expect("s").bytes, *bytes);
    }
}

#[test]
fn legacy_rebuild_pauses_for_a_purge_and_resumes() {
    let t = TempStore::new();
    let (month, kept) = legacy_month(&t, 40, 10);
    let store_arc = Arc::new(t);
    store_arc.store().set_rebuild_batch_rows(2);

    let purge_handle: Arc<Mutex<Option<thread::JoinHandle<usize>>>> = Arc::new(Mutex::new(None));
    let kept0 = kept[0].0.clone();
    let store_hook = store_arc.clone();
    let handle_hook = purge_handle.clone();
    store_arc.store().set_after_rebuild_batch(Some(Arc::new(move || {
        let mut guard = handle_hook.lock().expect("h");
        if guard.is_some() {
            return;
        }
        let s2 = store_hook.clone();
        let id = kept0.clone();
        *guard = Some(thread::spawn(move || {
            s2.store().purge(&PurgeScope::for_id(&id)).expect("purge")
        }));
        let sw = Instant::now();
        while store_hook.store().priority_waiters() == 0 && sw.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(1));
        }
    })));

    let mut s = Settings::default();
    s.retention.days = 3650;
    store_arc.store().run_maintenance(&s, true).expect("maint");

    let h = purge_handle.lock().expect("h").take().expect("spawned");
    assert_eq!(h.join().expect("join"), 1);
    assert!(store_arc.store().rebuild_pauses() >= 1);
    assert!(store_arc.store().get_keyframe(&kept[0].0).expect("kf").is_none());
    for (id, bytes) in kept.iter().skip(1) {
        assert_eq!(store_arc.store().get_keyframe(id).expect("kf").expect("s").bytes, *bytes);
    }
    assert_eq!(
        store_arc.store().query_month_i64_for_test(&month, "PRAGMA auto_vacuum").expect("q"),
        Some(2)
    );
    assert_eq!(
        store_arc.store().query_month_i64_for_test(&month, "SELECT count(*) FROM kf").expect("q"),
        Some(9)
    );
    assert!(!has_tmp_files(&DataPaths::keyframes_dir(store_arc.home.path())));
}

#[test]
fn failed_legacy_rebuild_leaves_the_month_and_the_maintenance_running() {
    let t = TempStore::new();
    let (month, kept) = legacy_month(&t, 20, 5);
    t.store()
        .exec_month_for_test(
            &month,
            "UPDATE kf SET data = 'not a blob' WHERE rid = (SELECT max(rid) FROM kf)",
        )
        .expect("corrupt");
    let mut s = Settings::default();
    s.retention.days = 3650;
    t.store().run_maintenance(&s, true).expect("maint");

    for (id, bytes) in &kept[..kept.len() - 1] {
        assert_eq!(t.store().get_keyframe(id).expect("kf").expect("s").bytes, *bytes);
    }
    assert_eq!(
        t.store().query_month_i64_for_test(&month, "PRAGMA auto_vacuum").expect("q"),
        Some(0)
    );
    assert!(!has_tmp_files(&DataPaths::keyframes_dir(t.home.path())));
}

#[test]
fn closing_the_store_drops_a_paused_rebuild() {
    let mut t = TempStore::new();
    let (month, _) = legacy_month(&t, 20, 5);
    t.store().set_rebuild_batch_rows(1);
    let paused = t
        .store()
        .release_free_pages_paused_for_test(&month)
        .expect("pause");
    assert!(paused);
    assert!(has_tmp_files(&DataPaths::keyframes_dir(t.home.path())));
    t.close();
    assert!(!has_tmp_files(&DataPaths::keyframes_dir(t.home.path())));
}

#[test]
fn store_db_vacuum_respects_the_ram_budget() {
    let t = TempStore::new();
    let old = Utc::now() - ChronoDuration::hours(2);
    for i in 0..1600 {
        let chunk = seeded_bytes(i, 8000);
        t.store()
            .add_audio_capture(&audio(
                old + ChronoDuration::seconds(i64::from(i)),
                &hex_upper(&chunk),
                "teams.exe",
            ))
            .expect("audio");
    }
    t.store()
        .purge(&PurgeScope::range(
            old - ChronoDuration::seconds(1),
            old + ChronoDuration::seconds(1500),
        ))
        .expect("purge");
    t.store().checkpoint().expect("cp");
    let db = DataPaths::store_db(t.home.path());
    let before = fs::metadata(&db).expect("meta").len();

    let mut s = Settings::default();
    s.retention.days = 3650;
    t.store().set_vacuum_ram_budget_bytes(0);
    t.store().run_maintenance(&s, true).expect("maint 0");
    t.store().checkpoint().expect("cp");
    assert!(
        fs::metadata(&db).expect("meta").len() >= before,
        "vacuumed over the budget"
    );

    t.store().set_vacuum_ram_budget_bytes(128 * 1024 * 1024);
    t.store().run_maintenance(&s, true).expect("maint 128M");
    t.store().checkpoint().expect("cp");
    let after = fs::metadata(&db).expect("meta").len();
    assert!(after < before / 2, "{after} vs {before}");
}

#[test]
fn hybrid_search_without_text_hits_reports_the_cosine() {
    let t = TempStore::new();
    let id = t
        .store()
        .add_screen_capture(&screen(
            Utc::now() - ChronoDuration::minutes(5),
            "chrome.exe",
            "Acme invoice 4471 - Gmail",
            true,
        ))
        .expect("add");
    t.store().set_embedding(&id, &vec_seeded(1), &vec_info()).expect("embed");

    let q = vec_seeded(2);
    let resp = t
        .store()
        .search(
            &SearchRequest {
                q: "zzqqxx".to_string(),
                limit: None,
                from: None,
                to: None,
                app: None,
                kind: None,
                mode: SearchMode::Hybrid,
                site: None,
            },
            &q,
            10,
            0,
        )
        .expect("search");
    assert_eq!(resp.results.len(), 1);
    let hit = &resp.results[0];
    assert!(!hit.text_match);
    let expected = ((f64::from(hit.cosine.expect("cos").max(0.0)) * 10_000.0).round() / 10_000.0) as f32;
    assert!((hit.score - expected).abs() < 1e-4);
    assert_ne!(hit.score, 0.5f32);

    let fused = t
        .store()
        .search(
            &SearchRequest {
                q: "acme".to_string(),
                limit: None,
                from: None,
                to: None,
                app: None,
                kind: None,
                mode: SearchMode::Hybrid,
                site: None,
            },
            &q,
            10,
            0,
        )
        .expect("fused");
    assert_eq!(fused.results.len(), 1);
    assert!(fused.results[0].text_match);
}

// ---- Sweep2GTests store cases ----

#[test]
fn maintenance_sweeps_keyframes_whose_row_is_gone() {
    let t = TempStore::new();
    let kept = t
        .store()
        .add_screen_capture(&screen(
            Utc::now() - ChronoDuration::minutes(5),
            "chrome.exe",
            "kept",
            true,
        ))
        .expect("add");
    assert!(
        t.store()
            .take_pending_embeddings(10)
            .expect("take")
            .iter()
            .any(|j| j.id == kept)
    );
    t.store().set_embedding(&kept, &vec_seeded(1), &vec_info()).expect("embed");

    let lost = t
        .store()
        .add_screen_capture(&screen(
            Utc::now() - ChronoDuration::minutes(4),
            "chrome.exe",
            "crashed delete",
            true,
        ))
        .expect("add");
    assert_eq!(t.store().keyframe_count_for_test().expect("cnt"), 2);
    t.store().delete_row_keep_image_for_test(&lost).expect("del");
    assert_eq!(t.store().keyframe_count_for_test().expect("cnt"), 2);
    t.store().run_maintenance(&Settings::default(), true).expect("maint");
    assert_eq!(t.store().keyframe_count_for_test().expect("cnt"), 1);
    assert!(t.store().get_keyframe(&kept).expect("kf").is_some());
}

#[test]
fn purge_and_keyframe_reads_run_while_maintenance_runs() {
    let t = TempStore::new();
    let now = Utc::now();
    let mut ids = Vec::new();
    for i in 0..200 {
        ids.push(
            t.store()
                .add_screen_capture(&screen(
                    now - ChronoDuration::days(40) + ChronoDuration::seconds(i),
                    "chrome.exe",
                    &format!("old {i}"),
                    true,
                ))
                .expect("add"),
        );
    }
    let recent = t
        .store()
        .add_screen_capture(&screen(
            now - ChronoDuration::minutes(1),
            "chrome.exe",
            "recent",
            true,
        ))
        .expect("add");
    let mut s = Settings::default();
    s.retention.days = 30;
    let store = t.store();
    let purged = thread::scope(|scope| {
        let m = scope.spawn(|| store.run_maintenance(&s, true).expect("maint"));
        let p = scope.spawn(|| {
            store
                .purge(&PurgeScope::last(ChronoDuration::minutes(10)))
                .expect("purge")
        });
        let r = scope.spawn(|| store.get_keyframe(&recent).expect("kf"));
        m.join().expect("m");
        let _ = r.join().expect("r");
        p.join().expect("p")
    });
    assert_eq!(purged, 1);
    assert_eq!(store.keyframe_count_for_test().expect("cnt"), 0);
    for id in &ids {
        assert!(store.get(id).expect("g").is_none());
    }
}
