// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Purge fence tests (time windows, monotonic expiry, purge-all, app/site, audio overlap).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use br_contracts::{NewReplayFrame, PurgeScope};
use br_core::{EncryptedStore, StoreError};
use chrono::{Duration as ChronoDuration, Utc};

use common::{FAKE_WEBP, TempStore, audio, screen, vec_info, vec_seeded};

const IMAGE: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 sweep4-image-bytes";

/// Covers range purges ending now, ending in the future, and ending in the past (no fence).
#[test]
fn range_purge_ending_now_fences_its_window_only() {
    enum RangeCase {
        EndingNow,
        EndingFuture,
        OldRange,
    }
    for case in [RangeCase::EndingNow, RangeCase::EndingFuture, RangeCase::OldRange] {
        let t = TempStore::new();
        let now = Utc::now();
        match case {
            RangeCase::EndingNow => {
                t.store()
                    .purge(&PurgeScope::range(now - ChronoDuration::hours(1), now))
                    .expect("purge");
                assert!(matches!(
                    t.store().add_screen_capture(&screen(
                        now - ChronoDuration::seconds(10),
                        "chrome.exe",
                        "x",
                        true
                    )),
                    Err(StoreError::PurgedWindow(_))
                ));
                assert!(matches!(
                    t.store().add_audio_capture(&audio(
                        now - ChronoDuration::minutes(30),
                        "late transcript",
                        "teams.exe"
                    )),
                    Err(StoreError::PurgedWindow(_))
                ));
                assert!(
                    t.store()
                        .add_screen_capture(&screen(
                            now - ChronoDuration::hours(2),
                            "chrome.exe",
                            "x",
                            true
                        ))
                        .is_ok()
                );
                assert!(
                    t.store()
                        .add_screen_capture(&screen(
                            Utc::now() + ChronoDuration::seconds(1),
                            "chrome.exe",
                            "x",
                            true
                        ))
                        .is_ok()
                );
            }
            RangeCase::EndingFuture => {
                t.store()
                    .purge(&PurgeScope::range(
                        now - ChronoDuration::hours(1),
                        now + ChronoDuration::hours(5),
                    ))
                    .expect("purge");
                assert!(matches!(
                    t.store().add_screen_capture(&screen(
                        now - ChronoDuration::seconds(1),
                        "chrome.exe",
                        "x",
                        true
                    )),
                    Err(StoreError::PurgedWindow(_))
                ));
                assert!(
                    t.store()
                        .add_screen_capture(&screen(
                            Utc::now() + ChronoDuration::seconds(2),
                            "chrome.exe",
                            "x",
                            true
                        ))
                        .is_ok()
                );
            }
            RangeCase::OldRange => {
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
        }
    }
}

/// Time fences and app fences (A-M2) refuse before monotonic expiry and admit once `FENCE_LIFETIME_MS` elapses.
#[test]
fn fence_expires_by_the_monotonic_clock_after_a_wall_clock_step_back() {
    for is_app in [false, true] {
        let t = TempStore::new();
        let mono = Arc::new(AtomicI64::new(1_000_000));
        let m2 = mono.clone();
        t.store().set_mono_ms_for_test(move || m2.load(Ordering::Relaxed));

        let now = Utc::now();
        let (scope, ts, app) = if is_app {
            (
                PurgeScope::for_app("notepad"),
                now - ChronoDuration::seconds(5),
                "notepad.exe",
            )
        } else {
            (
                PurgeScope::last(ChronoDuration::minutes(10)),
                now - ChronoDuration::minutes(3),
                "chrome.exe",
            )
        };
        t.store().purge(&scope).expect("purge");
        assert!(matches!(
            t.store().add_screen_capture(&screen(ts, app, "x", true)),
            Err(StoreError::PurgedWindow(_))
        ));
        mono.fetch_add(EncryptedStore::FENCE_LIFETIME_MS, Ordering::Relaxed);
        assert!(
            t.store()
                .add_screen_capture(&screen(ts, app, "x", true))
                .is_ok()
        );
    }
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

/// Regression A-H1.
#[test]
fn store_after_purge_all_refuses_captures_stamped_before_it() {
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

/// Regression A-M2.
#[test]
fn app_purge_refuses_frames_of_that_app_grabbed_before_it() {
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

/// Regression A-M2.
#[test]
fn site_purge_refuses_frames_of_that_site_grabbed_before_it() {
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

/// Regression A-L6.
#[test]
fn audio_row_overlapping_a_purged_range_is_refused() {
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
