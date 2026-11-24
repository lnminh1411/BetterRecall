// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use br_contracts::{
    CoreHostOptions, DataPaths, EmbeddingInfo, InstallLayout, PipeNames, PurgeNotice, Settings,
    StorageSettings, StorageUse,
};
use br_core::{AgentControl, CancelToken, CoreHost, SettingsStore};
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, TimeZone, Utc};

use br_agent::host::{
    AgentHost, AgentOptions, Hotkey, HotkeyParser, PauseController, ReadGate, StartupRegistration,
    StorageGuard, StorageStore,
};
use br_agent::util::{AgentLog, RetryBackoff};

use common::{FakeStore, RealStore, TempDir, new_screen_capture};

fn real_home() -> PathBuf {
    // Derive the default home without touching or opening anything under it.
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\AppData\Local"));
    local.join("BetterRecall")
}

// ---------------- IsolationTests ----------------

#[test]
fn installed_folder_keeps_the_plain_names_others_get_their_own() {
    let real = real_home();
    let real_slash = PathBuf::from(format!("{}\\", real.display()));
    assert!(DataPaths::is_default_home(&real_slash));
    assert_eq!(PipeNames::AGENT_MUTEX, PipeNames::agent_mutex_for(&real));
    assert_eq!(
        PipeNames::AGENT_QUIT_EVENT,
        PipeNames::agent_quit_event_for(&real)
    );

    let dev = Path::new(r"K:\scratch\betterrecall-dev-home");
    assert!(!DataPaths::is_default_home(dev));
    assert!(
        PipeNames::agent_mutex_for(dev).starts_with(&format!("{}.", PipeNames::AGENT_MUTEX))
    );
    assert_ne!(
        PipeNames::agent_mutex_for(dev),
        PipeNames::agent_mutex_for(Path::new(r"K:\scratch\other"))
    );
    assert_eq!(
        PipeNames::agent_quit_event_for(dev),
        PipeNames::agent_quit_event_for(Path::new(r"k:\scratch\betterrecall-dev-home\"))
    );
}

#[test]
fn only_the_installed_layout_registers_autostart() {
    let dir = TempDir::new("installed-check");
    let install_dir = dir.path();

    assert!(!StartupRegistration::is_installed_with_default_check(
        install_dir,
        true
    ));
    assert!(!StartupRegistration::is_installed_with_default_check(
        install_dir,
        false
    ));

    fs::write(install_dir.join(InstallLayout::INSTALL_MARKER), "0.3.0-dev\n").unwrap();
    assert!(StartupRegistration::is_installed_with_default_check(
        install_dir,
        true
    ));
    assert!(!StartupRegistration::is_installed_with_default_check(
        install_dir,
        false
    ));

    assert!(!StartupRegistration::is_installed(
        install_dir,
        Path::new(r"K:\scratch\h")
    ));
    let real = real_home();
    let unmarked = dir.path().join("unmarked");
    fs::create_dir_all(&unmarked).unwrap();
    assert!(!StartupRegistration::is_installed(&unmarked, &real));
    assert!(StartupRegistration::is_installed(install_dir, &real));
}

#[test]
fn autostart_exe_always_returns_agent_exe() {
    let dir = TempDir::new("autostart-exe");
    let install = dir.path().join("BetterRecall");
    fs::create_dir_all(&install).unwrap();
    assert_eq!(
        InstallLayout::full(&install, InstallLayout::AGENT_EXE),
        StartupRegistration::autostart_exe(&install)
    );
}

// ---------------- HotkeyParserTests ----------------

#[test]
fn parses() {
    let cases = [
        ("Ctrl+Alt+Space", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT, 0x20u32),
        ("Ctrl+Alt+P", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT, b'P' as u32),
        ("ctrl + alt + shift + x", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT | Hotkey::MOD_SHIFT, b'X' as u32),
        ("Win+F12", Hotkey::MOD_WIN, 0x7Bu32),
        ("Alt+1", Hotkey::MOD_ALT, b'1' as u32),
        ("Ctrl+Plus", Hotkey::MOD_CONTROL, 0xBBu32),
        ("Ctrl++", Hotkey::MOD_CONTROL, 0xBBu32),
        ("Shift+PageDown", Hotkey::MOD_SHIFT, 0x22u32),
        ("F9", 0u32, 0x78u32),
        ("Ctrl+NumPad7", Hotkey::MOD_CONTROL, 0x67u32),
        ("Win+Shift+PrintScreen", Hotkey::MOD_WIN | Hotkey::MOD_SHIFT, 0x2Cu32),
        ("Ctrl+Alt+;", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT, 0xBAu32),
        ("Ctrl+Alt+=", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT, 0xBBu32),
        ("Ctrl+Alt+-", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT, 0xBDu32),
        ("Ctrl+Alt+\\", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT, 0xDCu32),
        ("Ctrl+Alt+'", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT, 0xDEu32),
        ("Ctrl+Alt+`", Hotkey::MOD_CONTROL | Hotkey::MOD_ALT, 0xC0u32),
        ("Alt+Pause", Hotkey::MOD_ALT, 0x13u32),
    ];
    for (text, mods, vk) in cases {
        let hk = HotkeyParser::parse(Some(text))
            .unwrap_or_else(|| panic!("failed to parse {text}"));
        assert_eq!(mods, hk.modifiers, "text={text}");
        assert_eq!(vk, hk.virtual_key, "text={text}");
    }
}

#[test]
fn rejects() {
    for text in [
        Some(""),
        Some("Ctrl+Alt"),
        Some("Ctrl+Ctrl+A"),
        Some("Ctrl+A+B"),
        Some("Ctrl+Foo"),
        Some("F25"),
        None,
    ] {
        assert!(
            HotkeyParser::parse(text).is_none(),
            "expected None for {text:?}"
        );
    }
}

#[test]
fn round_trips_format() {
    for s in [
        "Ctrl+Alt+Space",
        "Ctrl+Alt+Shift+X",
        "Win+F12",
        "Ctrl+NumPad3",
        "Alt+PageDown",
        "Ctrl+Alt+;",
        "Ctrl+Alt+=",
        "Ctrl+Alt+-",
        "Shift+/",
        "Ctrl+Alt+[",
        "Ctrl+Alt+\\",
        "Ctrl+'",
        "Win+PrintScreen",
        "Ctrl+Insert",
    ] {
        let hk = HotkeyParser::parse(Some(s)).unwrap();
        assert_eq!(s, HotkeyParser::format(hk));
    }
}

// ---------------- PauseController, ReadGate, RetryBackoff ----------------

#[test]
fn read_gate_opens_for_timeout_and_closes() {
    let now = Arc::new(Mutex::new(
        Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap(),
    ));
    let n = now.clone();
    let gate = ReadGate::with_clock(move || *n.lock().unwrap());
    assert!(!gate.is_open(true));
    assert!(gate.is_open(false));
    gate.open(15);
    assert!(gate.is_open(true));
    *now.lock().unwrap() += ChronoDuration::seconds(14 * 60 + 54);
    assert!(gate.is_open(true));
    *now.lock().unwrap() += ChronoDuration::seconds(12);
    assert!(!gate.is_open(true));
    gate.open(15);
    gate.close();
    assert!(!gate.is_open(true));
}

#[test]
fn pause_reasons_priority_and_expiry() {
    let now = Arc::new(Mutex::new(DateTime::<Utc>::UNIX_EPOCH));
    let n = now.clone();
    let p = PauseController::with_clock(move || *n.lock().unwrap());
    assert!(p.current().0.is_none());
    p.set("fullscreen", None);
    let until = *now.lock().unwrap() + ChronoDuration::minutes(15);
    p.set("user", Some(until));
    assert_eq!(Some("user".to_string()), p.current().0);
    assert!(p.continuous_paused(true));
    p.set("consent", None);
    assert_eq!(Some("consent".to_string()), p.current().0);
    p.clear("consent");
    *now.lock().unwrap() += ChronoDuration::minutes(16);
    assert_eq!(Some("fullscreen".to_string()), p.current().0);
    assert!(p.keyframes_paused());
    assert!(!p.continuous_paused(true));
    assert!(p.continuous_paused(false));
}

#[test]
fn backoff_doubles_caps_and_resets() {
    let b = RetryBackoff::new(1000, 4000);
    assert!(b.due(0));
    assert_eq!(1000, b.failed(0));
    assert!(!b.due(999));
    assert!(b.due(1000));
    assert_eq!(2000, b.failed(1000));
    assert_eq!(4000, b.failed(3000));
    assert_eq!(4000, b.failed(7000));
    assert_eq!(4, b.failures());
    b.succeeded();
    assert!(b.due(7001));
    assert_eq!(1000, b.failed(7001));
}

#[test]
fn setting_the_same_pause_reason_again_does_not_raise_changed() {
    let p = PauseController::new();
    let changed = Arc::new(AtomicI64::new(0));
    let c = changed.clone();
    p.on_changed(move || {
        c.fetch_add(1, Ordering::AcqRel);
    });
    p.set("storeerror", None);
    p.set("storeerror", None);
    assert_eq!(1, changed.load(Ordering::Acquire));
    let until = Utc::now() + ChronoDuration::minutes(5);
    p.set("user", Some(until));
    p.set("user", Some(until));
    assert_eq!(2, changed.load(Ordering::Acquire));
    p.set("user", Some(until + ChronoDuration::minutes(1)));
    assert_eq!(3, changed.load(Ordering::Acquire));
}

#[test]
fn store_error_retry_never_leaves_a_moment_without_a_pause_reason() {
    let p = Arc::new(PauseController::new());
    p.set("keylocked", None);
    let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let s = seen.clone();
    let p2 = p.clone();
    p.on_changed(move || {
        s.lock().unwrap().push(p2.current().0);
    });
    for _ in 0..3 {
        p.set("storeerror", None);
        p.clear("keylocked");
    }
    let list = seen.lock().unwrap();
    assert!(!list.contains(&None));
    assert_eq!(2, list.len());
}

// sweep 3J A17
#[test]
fn pause_survives_a_wall_clock_step() {
    let wall = Arc::new(Mutex::new(Utc::now()));
    let mono = Arc::new(AtomicI64::new(1_000_000));
    let w = wall.clone();
    let m = mono.clone();
    let pc = PauseController::with_clocks(
        Arc::new(move || *w.lock().unwrap()),
        Arc::new(move || m.load(Ordering::Acquire)),
    );
    let expired = Arc::new(Mutex::new(None::<String>));
    let e = expired.clone();
    pc.on_expired(move |r| *e.lock().unwrap() = Some(r.to_string()));
    let until = *wall.lock().unwrap() + ChronoDuration::hours(1);
    pc.set("user", Some(until));
    *wall.lock().unwrap() += ChronoDuration::minutes(70);
    assert!(pc.has("user"));
    assert_eq!(Some("user".to_string()), pc.current().0);
    mono.fetch_add(3_600_000, Ordering::AcqRel);
    assert!(!pc.has("user"));
    assert_eq!(Some("user".to_string()), *expired.lock().unwrap());
}

// sweep 3J A17
#[test]
fn read_gate_timeout_ignores_a_backwards_step() {
    let wall = Arc::new(Mutex::new(Utc::now()));
    let mono = Arc::new(AtomicI64::new(5_000));
    let w = wall.clone();
    let m = mono.clone();
    let gate = ReadGate::with_clocks(
        Arc::new(move || *w.lock().unwrap()),
        Arc::new(move || m.load(Ordering::Acquire)),
    );
    gate.open(5);
    *wall.lock().unwrap() -= ChronoDuration::hours(2);
    mono.fetch_add(5 * 60_000, Ordering::AcqRel);
    assert!(!gate.is_open(true));
}

// sweep 3J D4
#[test]
fn user_lock_closes_the_gate_without_require_unlock() {
    let gate = ReadGate::new();
    assert!(gate.is_open(false));
    gate.lock_by_user();
    assert!(!gate.is_open(false));
    assert!(gate.until().is_none());
    gate.release_user_lock();
    assert!(gate.is_open(false));

    gate.lock_by_user();
    gate.open(5);
    assert!(gate.is_open(true));
    gate.close();
    assert!(gate.is_open(false));
}

#[test]
fn expired_timed_pause_is_reported_once_whichever_call_sees_it() {
    for via in ["has", "continuous", "current"] {
        let now = Arc::new(Mutex::new(DateTime::<Utc>::UNIX_EPOCH));
        let n = now.clone();
        let p = PauseController::with_clock(move || *n.lock().unwrap());
        let expired = Arc::new(Mutex::new(Vec::<String>::new()));
        let e = expired.clone();
        p.on_expired(move |r| e.lock().unwrap().push(r.to_string()));
        let until = *now.lock().unwrap() + ChronoDuration::minutes(60);
        p.set("ipc", Some(until));
        p.set("fullscreen", None);
        assert!(p.has("ipc"));
        assert!(expired.lock().unwrap().is_empty());

        *now.lock().unwrap() += ChronoDuration::minutes(61);
        match via {
            "has" => assert!(!p.has("ipc")),
            "continuous" => assert!(p.continuous_paused(false)),
            _ => assert_eq!(Some("fullscreen".to_string()), p.current().0),
        }
        assert_eq!(vec!["ipc".to_string()], *expired.lock().unwrap());
        let _ = p.current();
        let _ = p.has("ipc");
        assert_eq!(1, expired.lock().unwrap().len());
    }
}

#[test]
fn manual_clear_is_not_an_expiry() {
    let p = PauseController::new();
    let expired = Arc::new(Mutex::new(Vec::<String>::new()));
    let e = expired.clone();
    p.on_expired(move |r| e.lock().unwrap().push(r.to_string()));
    p.set("user", Some(Utc::now() + ChronoDuration::hours(1)));
    p.clear("user");
    let _ = p.current();
    assert!(expired.lock().unwrap().is_empty());
}

// sweep 4U B11: merges ipc-resume and user-resume clear_for_resume tests.
#[test]
fn resume_clears_ipc_and_optionally_user_pause() {
    let cases: &[(&str, bool, Option<&str>)] = &[
        ("ipc", true, Some("user")),
        ("user", false, None),
    ];
    for &(reason, expect_user, expect_current) in cases {
        let p = PauseController::new();
        p.set("user", None);
        p.set("ipc", Some(Utc::now() + ChronoDuration::minutes(1)));
        AgentHost::clear_for_resume(&p, reason);
        assert_eq!(expect_user, p.has("user"), "reason={reason}");
        assert!(!p.has("ipc"), "reason={reason}");
        assert_eq!(
            expect_current.map(str::to_string),
            p.current().0,
            "reason={reason}"
        );
    }
}

// ---------------- AgentHost & EndSession ----------------

#[test]
fn agent_log_stamps_local_time_with_offset() {
    let offset = FixedOffset::east_opt(7 * 3600).unwrap();
    let dt = offset
        .with_ymd_and_hms(2026, 11, 16, 4, 5, 6)
        .unwrap()
        + ChronoDuration::milliseconds(789);
    assert_eq!("2026-11-16T04:05:06.789+07:00", AgentLog::stamp(dt));
}

// Merges guardrails_and_host_tests + audio_tests audio_needs_sync tables.
#[test]
fn audio_sources_are_retried_while_wanted_and_missing() {
    let cases = [
        (false, false, false, false, false, false),
        (true, true, true, true, false, false),
        (false, false, true, true, false, false),
        (true, false, false, false, false, true),
        (false, false, true, false, false, true),
        (false, false, true, true, true, true),
    ];
    for (ws, so, wm, mo, ml, expected) in cases {
        assert_eq!(
            expected,
            AgentHost::audio_needs_sync(ws, so, wm, mo, ml),
            "case=({ws},{so},{wm},{mo},{ml})"
        );
    }
}

// sweep 3J A13
#[test]
fn audio_status_follows_loopback_and_mic_loss() {
    let on = |sys: bool, mic: bool| (sys, mic);
    let st = |a: br_contracts::AudioState| (a.system, a.mic);
    assert_eq!(
        on(true, true),
        st(AgentHost::audio_state_of(true, true, true, false))
    );
    assert_eq!(
        on(false, true),
        st(AgentHost::audio_state_of(true, false, true, false))
    );
    assert_eq!(
        on(true, false),
        st(AgentHost::audio_state_of(true, true, true, true))
    );
    assert_eq!(
        on(false, false),
        st(AgentHost::audio_state_of(false, true, false, false))
    );
}

// Merges guardrails a_m2 + replay user_purge_reaching + replay old_purges_and_non_user_deletes.
#[test]
fn reaches_ring_table() {
    let now = Utc.with_ymd_and_hms(2026, 11, 12, 10, 0, 0).unwrap();
    let old = now - ChronoDuration::days(10);

    let cases: &[(&str, PurgeNotice, i32, bool)] = &[
        // App and site purges reach the ring however old their rows (sweep 4P A-M2).
        (
            "app-old",
            PurgeNotice {
                reason: "purge".to_string(),
                ids: Vec::new(),
                from: Some(old),
                to: Some(old),
                newest: Some(old),
                scope: Some("app".to_string()),
            },
            60,
            true,
        ),
        (
            "site-old",
            PurgeNotice {
                reason: "purge".to_string(),
                ids: Vec::new(),
                from: Some(old),
                to: Some(old),
                newest: Some(old),
                scope: Some("site".to_string()),
            },
            60,
            true,
        ),
        (
            "range-old",
            PurgeNotice {
                reason: "purge".to_string(),
                ids: Vec::new(),
                from: Some(old),
                to: Some(old),
                newest: Some(old),
                scope: Some("range".to_string()),
            },
            60,
            false,
        ),
        // Recent user purges reach the ring.
        (
            "last-15m",
            PurgeNotice {
                reason: "purge".to_string(),
                ids: Vec::new(),
                from: Some(now - ChronoDuration::minutes(15)),
                to: Some(now),
                newest: None,
                scope: None,
            },
            120,
            true,
        ),
        (
            "recent-id",
            PurgeNotice {
                reason: "purge".to_string(),
                ids: vec!["s_1".to_string()],
                from: None,
                to: None,
                newest: Some(now - ChronoDuration::minutes(1)),
                scope: None,
            },
            120,
            true,
        ),
        (
            "purge-all",
            PurgeNotice {
                reason: "all".to_string(),
                ids: Vec::new(),
                from: Some(DateTime::<Utc>::MIN_UTC),
                to: Some(now),
                newest: None,
                scope: None,
            },
            120,
            true,
        ),
        // Old purges and non-user deletes leave the rings.
        (
            "old-range-1d",
            PurgeNotice {
                reason: "purge".to_string(),
                ids: vec!["s_1".to_string()],
                from: Some(now - ChronoDuration::days(2)),
                to: Some(now - ChronoDuration::days(1)),
                newest: Some(now - ChronoDuration::days(1)),
                scope: None,
            },
            120,
            false,
        ),
        (
            "old-1h",
            PurgeNotice {
                reason: "purge".to_string(),
                ids: vec!["s_1".to_string()],
                from: None,
                to: None,
                newest: Some(now - ChronoDuration::hours(1)),
                scope: None,
            },
            120,
            false,
        ),
        (
            "retention",
            PurgeNotice {
                reason: "retention".to_string(),
                ids: vec!["s_1".to_string()],
                from: None,
                to: None,
                newest: Some(now - ChronoDuration::seconds(1)),
                scope: None,
            },
            120,
            false,
        ),
        (
            "drop",
            PurgeNotice {
                reason: "drop".to_string(),
                ids: vec!["s_1".to_string()],
                from: None,
                to: None,
                newest: Some(now - ChronoDuration::seconds(1)),
                scope: None,
            },
            120,
            false,
        ),
        (
            "replay",
            PurgeNotice {
                reason: "replay".to_string(),
                ids: vec!["r_1".to_string()],
                from: None,
                to: None,
                newest: Some(now - ChronoDuration::seconds(1)),
                scope: None,
            },
            120,
            false,
        ),
    ];

    for (label, notice, replay_s, expected) in cases {
        assert_eq!(
            *expected,
            AgentHost::reaches_ring(notice, now, *replay_s, None, None),
            "case={label}"
        );
    }

    // A1: paused replay holding packets or samples from 10 min ago.
    let ten_min_ago = now - ChronoDuration::minutes(10);
    let paused_overlap = PurgeNotice {
        reason: "purge".to_string(),
        ids: Vec::new(),
        from: Some(now - ChronoDuration::minutes(11)),
        to: Some(now - ChronoDuration::minutes(9)),
        newest: Some(ten_min_ago),
        scope: Some("range".to_string()),
    };
    assert!(
        AgentHost::reaches_ring(&paused_overlap, now, 30, Some(ten_min_ago), None),
        "A1: range overlapping oldest ring packet while paused must reach ring"
    );
    assert!(
        AgentHost::reaches_ring(&paused_overlap, now, 30, None, Some(ten_min_ago)),
        "A1: range overlapping oldest sample while paused must reach ring"
    );
    let older_than_held = PurgeNotice {
        reason: "purge".to_string(),
        ids: Vec::new(),
        from: Some(now - ChronoDuration::hours(2)),
        to: Some(now - ChronoDuration::hours(1)),
        newest: Some(now - ChronoDuration::hours(1)),
        scope: Some("range".to_string()),
    };
    assert!(
        !AgentHost::reaches_ring(
            &older_than_held,
            now,
            30,
            Some(ten_min_ago),
            Some(ten_min_ago)
        ),
        "A1: range strictly older than held packets and samples must not clear ring"
    );
}

// sweep 4R S3
#[test]
fn turning_text_detection_on_is_noticed() {
    let mut old = Settings::default();
    old.ocr.enabled = false;
    let mut s = Settings::default();
    s.ocr.enabled = true;
    assert!(AgentHost::text_detection_turned_on(&old, &s));
    assert!(!AgentHost::text_detection_turned_on(&s, &s));
    let mut s2 = s.clone();
    s2.privacy.redact_emails = true;
    assert!(AgentHost::text_detection_turned_on(&s, &s2));
}

// sweep 4U S11
#[test]
fn retention_waits_for_a_readable_settings_file_and_runs_after_reset() {
    let rs = RealStore::new("s11");
    fs::write(
        DataPaths::settings_file(rs.dir.path()),
        "{\"retention\":{\"days\":3650}} x",
    )
    .unwrap();
    let settings = SettingsStore::load(rs.dir.path());
    assert!(settings.load_error().is_some());

    let mut c = new_screen_capture(vec![1, 2, 3]);
    c.ts = Utc::now() - ChronoDuration::days(400);
    c.title = "old".to_string();
    let id = rs.store.add_screen_capture(&c).unwrap();
    let mut v = vec![0.0f32; 768];
    v[0] = 1.0;
    rs.store
        .set_embedding(
            &id,
            &v,
            &EmbeddingInfo {
                model_id: "fake-gemma".to_string(),
                revision: "r1".to_string(),
                dim: 768,
            },
        )
        .unwrap();

    let mut logged = false;
    rs.store
        .run_maintenance(
            &settings.current(),
            AgentHost::retention_allowed(Some(&settings), &mut logged),
        )
        .unwrap();
    assert!(logged);
    assert_eq!(1, rs.store.stats().unwrap().rows);

    assert!(settings.reset_unreadable().unwrap());
    rs.store
        .run_maintenance(
            &settings.current(),
            AgentHost::retention_allowed(Some(&settings), &mut logged),
        )
        .unwrap();
    assert!(!logged);
    assert_eq!(0, rs.store.stats().unwrap().rows);
}

// sweep 4U B5
#[test]
fn sync_key_lock_pause_and_replay_pause_cover_passphrase_purge_all_window() {
    let pause = PauseController::new();
    assert!(AgentHost::should_pause_replay(&pause, false, false, true));
    assert!(!AgentHost::should_pause_replay(&pause, false, true, true));

    AgentHost::sync_key_lock_pause(&pause, true, false);
    assert!(!pause.has("keylocked"));

    AgentHost::sync_key_lock_pause(&pause, false, true);
    assert!(pause.has("keylocked"));
    assert_eq!(Some("keylocked".to_string()), pause.current().0);
    assert!(AgentHost::should_pause_replay(&pause, false, true, true));
}

// sweep 4U B5
#[test]
fn on_purged_all_in_passphrase_mode_sets_keylocked_pause_and_clears_capturing() {
    let dir = TempDir::new("b5");
    fs::write(
        DataPaths::settings_file(dir.path()),
        r#"{"security":{"keyProtection":"passphrase"},"capture":{"consented":true}}"#,
    )
    .unwrap();
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    agent.set_capture_started_for_test(true);

    agent.on_purged(&PurgeNotice {
        reason: "all".to_string(),
        ids: Vec::new(),
        from: Some(DateTime::<Utc>::MIN_UTC),
        to: Some(Utc::now()),
        newest: None,
        scope: None,
    });

    let st = agent.get_status();
    assert!(st.paused);
    assert_eq!(Some("keylocked".to_string()), st.pause_reason);
    assert!(!st.capturing);
    agent.shutdown();
    core.shutdown();
}

// sweep 5U C4 (CC P9): verifies end_session_shutdown itself checkpoints the WAL and cancels maintenance.
#[test]
fn end_session_cancels_maintenance_and_checkpoints_real_store() {
    let dir = TempDir::new("c4");
    fs::write(
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
    let store = core.store().unwrap();

    // Insert an old expired row so WAL has uncheckpointed frames and maintenance has real work to do.
    let mut old = new_screen_capture(vec![1, 2, 3]);
    old.ts = Utc::now() - ChronoDuration::days(400);
    old.title = "expired".to_string();
    let id = store.add_screen_capture(&old).unwrap();
    let mut vec4 = vec![0.0f32; 4];
    vec4[0] = 1.0;
    store
        .set_embedding(
            &id,
            &vec4,
            &EmbeddingInfo {
                model_id: "fake".to_string(),
                revision: "r1".to_string(),
                dim: 4,
            },
        )
        .unwrap();
    assert_eq!(1, store.stats().unwrap().rows);

    let wal = PathBuf::from(format!("{}-wal", DataPaths::store_db(dir.path()).display()));
    let wal_before = fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert!(wal_before > 0, "expected non-empty WAL before end_session_shutdown");

    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    agent.end_session_shutdown();

    // WAL must have been checkpointed by end_session_shutdown itself (without calling checkpoint here).
    let wal_after = fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert_eq!(0, wal_after, "end_session_shutdown did not checkpoint WAL");

    // Because end_session_shutdown called cancel_maintenance(), running maintenance now must abort
    // immediately without deleting the expired row.
    store.run_maintenance(&Settings::default(), true).unwrap();
    assert_eq!(
        1,
        store.stats().unwrap().rows,
        "maintenance was not cancelled by end_session_shutdown"
    );

    agent.shutdown();
    core.shutdown();
}

// Rust-side regression: end_session_shutdown bounds writer stop to the configured budget (CC P8).
#[test]
fn end_session_shutdown_does_not_wait_past_the_writer_budget() {
    let dir = TempDir::new("end_session_budget");
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    agent.set_end_session_writer_budget_for_test(Duration::from_millis(200));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    agent.set_on_stop_pipes_for_test(Some(Arc::new(move || {
        let _ = release_rx.lock().unwrap().recv();
    })));
    let started = Instant::now();
    agent.end_session_shutdown();
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(150),
        "returned before the budget: {took:?}"
    );
    assert!(
        took < Duration::from_millis(2200),
        "waited past the budget: {took:?}"
    );
    release_tx.send(()).unwrap();
    agent.shutdown();
    core.shutdown();
}

// sweep 4P C3 + sweep 5U C4: end-session stops pipes and dedupes with shutdown/dispose.
#[test]
fn end_session_stops_pipes_once_and_dedupes_with_shutdown() {
    let dir = TempDir::new("c3_c4_end_session");
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    let pipe_stops = Arc::new(AtomicI32::new(0));
    let ps = pipe_stops.clone();
    agent.set_on_stop_pipes_for_test(Some(Arc::new(move || {
        ps.fetch_add(1, Ordering::AcqRel);
    })));
    agent.on_end_session_for_test();
    assert_eq!(1, pipe_stops.load(Ordering::Acquire));
    agent.shutdown();
    assert_eq!(1, pipe_stops.load(Ordering::Acquire));
    core.shutdown();
}

// ---------------- StorageGuardTests (using common::FakeStore) ----------------

fn make_storage_guard(
    disk_free: i64,
    files: i64,
    free: i64,
    overwrite: bool,
) -> (StorageGuard, Arc<FakeStore>, Arc<Mutex<i64>>) {
    let store = FakeStore::new();
    store.inner.lock().unwrap().storage_use = StorageUse {
        file_bytes: files,
        free_bytes: free,
    };
    let mut s = Settings::default();
    s.storage.min_free_gb = 25;
    s.storage.max_gb = 20;
    s.storage.overwrite = overwrite;
    let disk = Arc::new(Mutex::new(disk_free));
    let st_cb = store.clone();
    let d_cb = disk.clone();
    let g = StorageGuard::new(
        move || Some(st_cb.clone() as Arc<dyn StorageStore>),
        move || s.clone(),
        move || Some(*d_cb.lock().unwrap()),
    );
    (g, store, disk)
}

#[test]
fn low_disk_pauses_once_and_resumes_with_margin() {
    let gb = StorageGuard::GB;
    let (g, store, disk) = make_storage_guard(24 * gb, 5 * gb, 0, false);
    let d = g.check();
    assert!(d.pause);
    assert!(d.notify);
    assert_eq!("disk", d.why);
    assert!(!g.check().notify);
    *disk.lock().unwrap() = 25 * gb + gb / 2;
    assert!(g.check().pause);
    *disk.lock().unwrap() = 26 * gb + 1;
    assert!(!g.check().pause);
    assert_eq!(0, store.inner.lock().unwrap().delete_calls);
}

#[test]
fn reusable_free_pages_count_as_free_space() {
    let gb = StorageGuard::GB;
    let (g, _, _) = make_storage_guard(24 * gb, 6 * gb, 2 * gb, false);
    assert!(!g.check().pause);
}

#[test]
fn over_size_pauses_without_overwrite() {
    let gb = StorageGuard::GB;
    let (g, _, _) = make_storage_guard(500 * gb, 21 * gb, 0, false);
    let d = g.check();
    assert!(d.pause);
    assert_eq!("size", d.why);
}

#[test]
fn overwrite_deletes_until_margin_instead_of_pausing() {
    let gb = StorageGuard::GB;
    let (g, store, _) = make_storage_guard(500 * gb, 21 * gb, 0, true);
    {
        let mut st = store.inner.lock().unwrap();
        st.deletable = 100_000;
        st.freed_per_row = 1024 * 1024;
    }
    let d = g.check();
    assert!(!d.pause);
    assert!(d.deleted > 0);
    assert!(
        store.inner.lock().unwrap().storage_use.live_bytes()
            <= 20 * gb - StorageGuard::margin(&StorageSettings::default())
    );
}

#[test]
fn overwrite_pauses_when_nothing_is_left_to_delete() {
    let gb = StorageGuard::GB;
    let (g, store, _) = make_storage_guard(24 * gb, 5 * gb, 0, true);
    {
        let mut st = store.inner.lock().unwrap();
        st.deletable = 3;
        st.freed_per_row = 1024;
    }
    let d = g.check();
    assert!(d.pause);
    assert_eq!(3, d.deleted);
    assert_eq!("disk", d.why);
}

#[test]
fn overwrite_keeps_the_store_when_deleting_cannot_free_enough() {
    let gb = StorageGuard::GB;
    let (g, store, _) = make_storage_guard(10 * gb, gb, 0, true);
    {
        let mut st = store.inner.lock().unwrap();
        st.deletable = 100_000;
        st.freed_per_row = 1024 * 1024;
    }
    let d = g.check();
    assert!(d.pause);
    assert!(d.notify);
    assert_eq!(0, d.deleted);
    assert_eq!(0, store.inner.lock().unwrap().delete_calls);
    assert_eq!("disk", d.why);
}

#[test]
fn overwrite_deletes_for_disk_when_the_store_covers_the_shortfall() {
    let gb = StorageGuard::GB;
    let (g, store, _) = make_storage_guard(24 * gb, 5 * gb, 0, true);
    {
        let mut st = store.inner.lock().unwrap();
        st.deletable = 100_000;
        st.freed_per_row = 1024 * 1024;
    }
    let d = g.check();
    assert!(!d.pause);
    assert!(d.deleted > 0);
    assert!(store.inner.lock().unwrap().deletable > 0);
}

static B2_PANIC_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn b2_panicking_tick(_: &Arc<AgentHost>) {
    let prev = B2_PANIC_COUNT.fetch_add(1, Ordering::SeqCst);
    if prev == 0 {
        panic!("simulated timer panic");
    }
}

#[test]
fn b2_timer_survives_panic_and_keeps_running() {
    let dir = TempDir::new("b2-timer");
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );

    B2_PANIC_COUNT.store(0, Ordering::SeqCst);
    let _handle = agent.spawn_timer_for_test(
        "b2-test-timer",
        Duration::from_millis(10),
        Duration::from_millis(20),
        b2_panicking_tick,
    );

    let start = std::time::Instant::now();
    while B2_PANIC_COUNT.load(Ordering::SeqCst) < 2 && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        B2_PANIC_COUNT.load(Ordering::SeqCst) >= 2,
        "timer should continue running after panic"
    );

    agent.shutdown();
    core.shutdown();
}

#[test]
fn c4_stop_auto_updater_returns_within_two_seconds_when_thread_stalls() {
    let dir = TempDir::new("c4-updater");
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );

    let ignore_cancel_thread = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(10));
    });
    agent.set_auto_updater_handle_for_test(br_core::CancelSource::new(), ignore_cancel_thread);

    let start = std::time::Instant::now();
    agent.stop_auto_updater_for_test();
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(1900),
        "should wait about 2 s before giving up, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(3500),
        "should not wait full 10 s, took {elapsed:?}"
    );

    agent.shutdown();
    core.shutdown();
}

#[test]
fn b1_second_instance_action_shows_launcher_only_without_args() {
    use br_agent::{SecondInstanceAction, second_instance_action};

    assert_eq!(
        SecondInstanceAction::ShowLauncher,
        second_instance_action(&[])
    );
    for args in [
        vec!["--unregister".to_string()],
        vec!["--no-autostart".to_string()],
        vec!["--console".to_string()],
        vec!["--exit-after".to_string(), "5".to_string()],
        vec!["--allow-default-home".to_string()],
    ] {
        assert_eq!(
            SecondInstanceAction::ExitSilent,
            second_instance_action(&args),
            "args={args:?}"
        );
    }
}

#[test]
fn d1_try_lock_recover_handles_poison_and_preserves_would_block() {
    use std::sync::{Barrier, Mutex};
    use br_agent::util::try_lock_recover;

    let m = Arc::new(Mutex::new(41i32));

    // WouldBlock stays None while another thread holds the lock.
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let m1 = m.clone();
    let e1 = entered.clone();
    let r1 = release.clone();
    let holder = std::thread::spawn(move || {
        let _g = m1.lock().unwrap();
        e1.wait();
        r1.wait();
    });
    entered.wait();
    assert!(try_lock_recover(&m, "test_lock").is_none());
    release.wait();
    holder.join().unwrap();

    // Panic while holding poisons the lock; try_lock_recover acquires, clears poison, and carries on.
    let m2 = m.clone();
    let _ = std::thread::spawn(move || {
        let mut g = m2.lock().unwrap();
        *g = 42;
        panic!("simulated panic while holding lock");
    })
    .join();
    assert!(m.is_poisoned());

    {
        let g = try_lock_recover(&m, "test_lock").expect("poisoned lock must be recovered");
        assert_eq!(*g, 42);
    }
    assert!(!m.is_poisoned());
    assert_eq!(
        *m.try_lock().expect("lock should be clean after recovery"),
        42
    );
}
