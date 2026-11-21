// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported `CoreHostTests.cs` + CoreHost purge/passphrase/stop tests from Sweep2F, Sweep3I, Sweep4P, Sweep5U.

mod common;

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{CoreHostOptions, DataPaths, PurgeNotice, PurgeScope};
use br_core::{CancelToken, CoreHost, PipeKind, StoreError};
use chrono::{Duration as ChronoDuration, Utc};

use common::a3::{FakeAgent, FakeEmbedder, FakeThumbs, test_pipe_name};
use common::{TempHome, audio};

#[test]
fn open_store_then_purge_all_starts_empty_with_a_new_key() {
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    assert!(host.store().is_none());
    host.open_store(&CancelToken::none()).expect("open store");
    assert!(host.store().is_some());
    assert!(host.keys().key_loaded());
    assert_eq!("dpapi", host.keys().protection());

    let id = host
        .store()
        .unwrap()
        .add_audio_capture(&audio(Utc::now(), "hello world", "teams.exe"))
        .expect("add audio");
    let key_before = fs::read(DataPaths::key_file(h.path())).expect("read key");

    // Blob cipher works with the vault key.
    let replays = DataPaths::replays_dir(h.path());
    fs::create_dir_all(&replays).expect("replays dir");
    let blob = replays.join("clip.mp4.enc");
    {
        let mut w = host.blobs().open_write(&blob).expect("open_write");
        w.write_all(&[1, 2, 3]).expect("write");
        w.finish().expect("finish");
    }
    {
        let mut r = host.blobs().open_read(&blob).expect("open_read");
        let mut b = [0u8; 1];
        r.read_exact(&mut b).expect("read");
        assert_eq!(1, b[0]);
    }

    let purged = host.purge_all(&CancelToken::none());
    assert_eq!(1, purged);
    assert!(!blob.exists());
    assert!(host.store().is_some());
    assert!(host.store().unwrap().get(&id).unwrap().is_none());
    assert_ne!(key_before, fs::read(DataPaths::key_file(h.path())).unwrap());
    host.shutdown();
    drop(host);

    // Reopen with the new key.
    let host2 = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    host2.open_store(&CancelToken::none()).expect("reopen");
    assert_eq!(0, host2.store().unwrap().stats().unwrap().rows);
    host2.shutdown();
}

#[test]
fn purge_last_refuses_captures_stamped_inside_the_purged_window() {
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    host.open_store(&CancelToken::none()).expect("open");
    let store = host.store().unwrap();
    let before = Utc::now() - ChronoDuration::seconds(5);
    store
        .purge(&PurgeScope::last(ChronoDuration::minutes(5)))
        .expect("purge");
    let err = store
        .add_audio_capture(&audio(before, "late transcript", "teams.exe"))
        .expect_err("expected PurgedWindow");
    assert!(matches!(err, StoreError::PurgedWindow(_)));
    assert_eq!(0, store.stats().unwrap().rows);
    assert!(
        store
            .add_audio_capture(&audio(
                Utc::now() + ChronoDuration::seconds(1),
                "after the purge",
                "teams.exe",
            ))
            .is_ok()
    );
    host.shutdown();
}

#[test]
fn purge_all_when_the_store_never_opened_deletes_its_files_and_starts_fresh() {
    let h = TempHome::new(Some("dpapi"));
    let db = DataPaths::store_db(h.path());
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::write(&db, [1u8, 2, 3]).unwrap();
    fs::write(format!("{}-wal", db.display()), [4u8]).unwrap();
    let kf = DataPaths::keyframes_dir(h.path());
    fs::create_dir_all(&kf).unwrap();
    fs::write(kf.join("2026-11.db"), [5u8]).unwrap();

    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    assert!(host.store().is_none());
    assert_eq!(0, host.purge_all(&CancelToken::none()));
    assert!(!kf.join("2026-11.db").exists());
    assert!(host.store().is_some());
    assert_ne!(3, fs::metadata(&db).unwrap().len());
    assert_eq!(0, host.store().unwrap().stats().unwrap().rows);
    host.shutdown();
}

#[test]
fn store_purges_and_purge_all_reach_core_host_listeners() {
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    let notices: Arc<Mutex<Vec<PurgeNotice>>> = Arc::new(Mutex::new(Vec::new()));
    let notices_cb = notices.clone();
    let _g = host.on_purged(move |n| notices_cb.lock().unwrap().push(n.clone()));

    host.open_store(&CancelToken::none()).expect("open");
    let id = host
        .store()
        .unwrap()
        .add_audio_capture(&audio(Utc::now(), "hello", "teams.exe"))
        .unwrap();
    host.store()
        .unwrap()
        .purge(&PurgeScope::for_id(&id))
        .unwrap();
    assert_eq!(1, notices.lock().unwrap().len());
    assert_eq!("purge", notices.lock().unwrap()[0].reason);

    host.purge_all(&CancelToken::none());
    let list = notices.lock().unwrap();
    assert!(list.iter().any(|n| n.reason == "all"));
    drop(list);
    host.shutdown();
}

#[test]
fn b1_purge_all_runs_while_the_store_waits_for_a_forgotten_passphrase() {
    let h = TempHome::new(Some("passphrase"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    let host_bg = host.clone();
    let done = Arc::new(AtomicBool::new(false));
    let done_bg = done.clone();
    let open_thread = thread::spawn(move || {
        let res = host_bg.open_store(&CancelToken::none());
        if res.is_ok() {
            done_bg.store(true, Ordering::Release);
        }
    });

    thread::sleep(Duration::from_millis(150));
    assert!(host.store().is_none());
    // Purge all must not deadlock while open_store waits on passphrase.
    assert_eq!(0, host.purge_all(&CancelToken::none()));

    // Unlock with a new passphrase so open_store finishes.
    assert!(host.keys().unlock_with_passphrase("fresh-passphrase-123"));
    for _ in 0..50 {
        if done.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(done.load(Ordering::Acquire));
    assert!(host.store().is_some());
    let _ = open_thread.join();
    host.shutdown();
}

#[test]
fn a_h1_purge_all_notice_comes_before_the_new_store_opens() {
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    host.open_store(&CancelToken::none()).expect("open");
    let saw_null_store = Arc::new(AtomicBool::new(false));
    let saw_purging = Arc::new(AtomicBool::new(false));
    let s1 = saw_null_store.clone();
    let s2 = saw_purging.clone();
    let host_weak = Arc::downgrade(&host);
    let _g = host.on_purged(move |n| {
        if n.reason == "all"
            && let Some(h) = host_weak.upgrade()
        {
            s1.store(h.store().is_none(), Ordering::Release);
            s2.store(h.purging_all(), Ordering::Release);
        }
    });
    host.purge_all(&CancelToken::none());
    assert!(saw_null_store.load(Ordering::Acquire));
    assert!(saw_purging.load(Ordering::Acquire));
    assert!(!host.purging_all());
    assert!(host.store().is_some());
    host.shutdown();
}

#[test]
fn purge_all_closes_the_old_store_before_the_agent_hears_it() {
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    host.open_store(&CancelToken::none()).expect("open");
    let old = host.store().unwrap();
    let old_disposed_in_notice = Arc::new(AtomicBool::new(false));
    let flag = old_disposed_in_notice.clone();
    let _g = host.on_purged(move |n| {
        if n.reason == "all" {
            let err = old.stats().expect_err("old store should be closed");
            if matches!(err, StoreError::Disposed) {
                flag.store(true, Ordering::Release);
            }
        }
    });
    host.purge_all(&CancelToken::none());
    assert!(old_disposed_in_notice.load(Ordering::Acquire));
    host.shutdown();
}

#[test]
fn purge_all_reports_a_replay_file_it_could_not_delete_and_the_next_start_deletes_it() {
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    host.open_store(&CancelToken::none()).expect("open");
    let replays = DataPaths::replays_dir(h.path());
    fs::create_dir_all(&replays).unwrap();
    let locked_clip = replays.join("held.mp4.enc");
    fs::write(&locked_clip, b"clip-data").unwrap();

    // Hold the file open without FILE_SHARE_DELETE so Windows refuses delete.
    let held = OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ only
        .open(&locked_clip)
        .expect("hold clip");

    host.purge_all(&CancelToken::none());
    assert_eq!(1, host.last_purge_all_undeleted());
    assert!(locked_clip.exists());
    drop(held);
    host.shutdown();
    drop(host);

    // Next CoreHost start sweeps exports.pending and deletes the clip.
    let host2 = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    assert!(!locked_clip.exists());
    host2.shutdown();
}

#[test]
fn c3_leftover_export_partial_is_deleted_at_host_start() {
    let h = TempHome::new(Some("dpapi"));
    let partial = h.path().join("leftover.zip.partial");
    fs::write(&partial, b"partial-data").unwrap();
    br_core::ExportJournal::add(Some(h.path()), &partial);
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    assert!(!partial.exists());
    host.shutdown();
}

#[test]
fn c4_pipe_manager_stops_all_three_pipes_and_core_host_dedupes_in_flight_stop() {
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    let tag = test_pipe_name("c4");
    host.set_pipe_name_override(Some(Arc::new(move |k, s| match k {
        PipeKind::Ui => format!("{tag}.ui"),
        PipeKind::Mcp => format!("{tag}.mcp"),
        PipeKind::Public => format!("{tag}.{}", s.ipc.pipe_name),
    })));
    assert!(
        host.settings()
            .try_update(&serde_json::json!({"ipc": {"enabled": true}, "mcp": {"enabled": true}}))
            .is_none()
    );
    host.start_pipes(
        Arc::new(FakeAgent::new()),
        Arc::new(FakeEmbedder::new()),
        Arc::new(FakeThumbs::new()),
    );
    let pm = host.pipe_manager().unwrap();
    assert!(pm.ui().is_some());
    assert!(pm.public().is_some());
    assert!(pm.mcp().is_some());

    // Two concurrent stop_pipes calls deduplicate cleanly.
    let h1 = host.clone();
    let h2 = host.clone();
    let t1 = thread::spawn(move || h1.stop_pipes());
    let t2 = thread::spawn(move || h2.stop_pipes());
    t1.join().unwrap();
    t2.join().unwrap();
    assert!(host.pipe_manager().is_none());
    host.shutdown();
}
