// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Purge notice delivery tests (`EncryptedStore::on_purged`).

mod common;

use std::fs;
use std::sync::{Arc, Mutex};

use br_contracts::{NewReplayFrame, PurgeNotice, PurgeScope, Settings};
use br_core::win32;
use chrono::{Duration as ChronoDuration, Utc};

use common::{FAKE_WEBP, TempStore, audio, screen};

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

/// Regression B11.
#[test]
fn maintenance_that_throws_still_raises_the_retention_notice() {
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

/// Regression A-M2: app purge notifies even with no matching row, while a no-match id purge notifies nothing.
#[test]
fn app_purge_notice_names_its_scope_even_with_no_row() {
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
