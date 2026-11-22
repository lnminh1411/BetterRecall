// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use br_contracts::{
    CoreHostOptions, DataPaths, EmbeddingInfo, InstallLayout, PipeNames, PrivacySettings,
    PurgeNotice, Settings,
};
use br_core::{AgentControl, CoreHost, SettingsStore};
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, TimeZone, Utc};

use br_agent::guardrails::{
    AudioGate, AudioVerdict, BadgeResult, BrowserBadgeDetector, GuardDecision, GuardReason,
    GuardSnapshot, GuardrailPolicy, GuardrailService, PasswordFieldProbe, WindowInfo,
};
use br_agent::host::{AgentHost, AgentOptions, Hotkey, HotkeyParser, PauseController, ReadGate, StartupRegistration};
use br_agent::native::{MASK_EVERYTHING, Rect, session_locked, session_locked_now};
use br_agent::util::{AgentLog, RetryBackoff};

use common::{RealStore, TempDir, new_screen_capture};

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
    let real = real_home();
    assert!(!StartupRegistration::is_installed(
        Path::new(r"E:\BetterRecall\artifacts\dev"),
        &real
    ));
    assert!(!StartupRegistration::is_installed(
        &real.join("app").join("0.3.0-abc"),
        Path::new(r"K:\scratch\h")
    ));
    if std::env::var_os(InstallLayout::LAUNCHER_ENV)
        .is_none_or(|v| v.to_string_lossy().trim().is_empty())
    {
        assert!(StartupRegistration::is_installed(
            &real.join("app").join("0.3.0-abc"),
            &real
        ));
    }
}

// ---------------- PasswordFieldProbeTests ----------------

#[test]
#[ignore = "creates a visible Win32 window for UI Automation probing (safety rule forbids showing windows in cargo tests)"]
fn finds_a_password_edit_and_its_rectangle() {
    // Intentionally ignored per test safety rules.
}

// ---------------- CaptureLogicTests (Guardrails, Hotkeys, AgentState) ----------------

fn win(exe: &str, title: &str) -> WindowInfo {
    WindowInfo {
        hwnd: 1,
        pid: 42,
        exe_path: exe.to_string(),
        title: title.to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
    }
}

#[test]
fn process_deny_list() {
    let cases = [
        (r"C:\Program Files\KeePassXC\KeePassXC.exe", GuardReason::DenyProcess),
        (r"C:\Windows\System32\mstsc.exe", GuardReason::DenyProcess),
        (r"C:\Tools\notepad.exe", GuardReason::None),
    ];
    let p = PrivacySettings {
        browser_badge_detection: false,
        ..Default::default()
    };
    for (exe, expected) in cases {
        let d = GuardrailPolicy::evaluate(Some(&win(exe, "Doc")), &p, false, false, BadgeResult::NotChecked);
        assert_eq!(expected == GuardReason::None, d.allow, "exe={exe}");
        assert_eq!(expected, d.reason, "exe={exe}");
    }
}

#[test]
fn path_prefix_matches_on_boundary_only() {
    let prefixes = vec![r"C:\Secret".to_string()];
    assert!(GuardrailPolicy::is_denied_path(r"C:\Secret\app.exe", &prefixes));
    assert!(GuardrailPolicy::is_denied_path(r"c:\secret\sub\APP.exe", &prefixes));
    assert!(!GuardrailPolicy::is_denied_path(r"C:\SecretStuff\app.exe", &prefixes));
    assert!(!GuardrailPolicy::is_denied_path("", &prefixes));
}

#[test]
fn title_patterns_are_case_insensitive_with_globs() {
    let pats = vec!["InPrivate".to_string(), "bank*statement".to_string()];
    assert!(GuardrailPolicy::title_matches("New tab - [InPrivate] - Microsoft Edge", &pats));
    assert!(GuardrailPolicy::title_matches("My BANK - March Statement.pdf", &pats));
    assert!(!GuardrailPolicy::title_matches("Statement of bank", &pats));
    let p_inpriv = PrivacySettings {
        deny_title_patterns: vec!["InPrivate".to_string()],
        ..Default::default()
    };
    assert_eq!("", GuardrailPolicy::filter_title("x - inprivate", &p_inpriv));
    assert_eq!("Inbox", GuardrailPolicy::filter_title("Inbox", &PrivacySettings::default()));
}

#[test]
fn session_lock_affinity_and_badge() {
    let p = PrivacySettings::default();
    let chrome = win(r"C:\Program Files\Google\Chrome\Application\chrome.exe", "Google");
    assert_eq!(GuardReason::SessionLocked, GuardrailPolicy::evaluate(Some(&chrome), &p, true, false, BadgeResult::NoBadge).reason);
    assert_eq!(GuardReason::NoForeground, GuardrailPolicy::evaluate(None, &p, false, false, BadgeResult::NoBadge).reason);
    assert_eq!(GuardReason::DisplayAffinity, GuardrailPolicy::evaluate(Some(&chrome), &p, false, true, BadgeResult::NoBadge).reason);
    assert_eq!(GuardReason::PrivateBadge, GuardrailPolicy::evaluate(Some(&chrome), &p, false, false, BadgeResult::Private).reason);
    assert_eq!(GuardReason::BadgeCheckFailed, GuardrailPolicy::evaluate(Some(&chrome), &p, false, false, BadgeResult::CheckFailed).reason);
    assert_eq!(GuardReason::BadgeCheckFailed, GuardrailPolicy::evaluate(Some(&chrome), &p, false, false, BadgeResult::NotChecked).reason);
    assert!(GuardrailPolicy::evaluate(Some(&chrome), &p, false, false, BadgeResult::NoBadge).allow);

    let off = PrivacySettings {
        pause_on_lock: false,
        honor_display_affinity: false,
        browser_badge_detection: false,
        ..Default::default()
    };
    assert!(GuardrailPolicy::evaluate(Some(&chrome), &off, true, true, BadgeResult::CheckFailed).allow);
}

#[test]
fn site_from_address_bar_keeps_the_host_only() {
    let cases = [
        ("github.com/lnminh1411/BetterRecall", "github.com"),
        ("https://www.Example.org/a?b=c#d", "example.org"),
        ("http://localhost:5173/app", "localhost"),
        ("localhost:8080", "localhost"),
        ("mail.google.com", "mail.google.com"),
        ("acme invoice 4471", ""),
        ("chrome://settings", ""),
        ("edge://newtab", ""),
        ("file:///C:/x.pdf", ""),
        ("about:blank", ""),
        ("invoice", ""),
        ("", ""),
    ];
    for (text, site) in cases {
        assert_eq!(site, GuardrailPolicy::site_from_address(Some(text)), "text={text}");
    }
}

#[test]
fn deny_list_wins_over_later_checks() {
    let p = PrivacySettings::default();
    let d = GuardrailPolicy::evaluate(Some(&win(r"C:\x\1Password.exe", "Doc")), &p, false, true, BadgeResult::Private);
    assert_eq!(GuardReason::DenyProcess, d.reason);
}

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
        let hk = HotkeyParser::parse(Some(text)).unwrap_or_else(|| panic!("failed to parse {text}"));
        assert_eq!(mods, hk.modifiers, "text={text}");
        assert_eq!(vk, hk.virtual_key, "text={text}");
    }
}

#[test]
fn rejects() {
    for text in [Some(""), Some("Ctrl+Alt"), Some("Ctrl+Ctrl+A"), Some("Ctrl+A+B"), Some("Ctrl+Foo"), Some("F25"), None] {
        assert!(HotkeyParser::parse(text).is_none(), "expected None for {text:?}");
    }
}

#[test]
fn round_trips_format() {
    for s in [
        "Ctrl+Alt+Space", "Ctrl+Alt+Shift+X", "Win+F12", "Ctrl+NumPad3", "Alt+PageDown",
        "Ctrl+Alt+;", "Ctrl+Alt+=", "Ctrl+Alt+-", "Shift+/", "Ctrl+Alt+[", "Ctrl+Alt+\\",
        "Ctrl+'", "Win+PrintScreen", "Ctrl+Insert",
    ] {
        let hk = HotkeyParser::parse(Some(s)).unwrap();
        assert_eq!(s, HotkeyParser::format(hk));
    }
}

#[test]
fn read_gate_opens_for_timeout_and_closes() {
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap()));
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

// ---------------- ReliabilitySweep2Tests (util, host, guard) ----------------

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

#[test]
fn audio_sources_are_retried_while_wanted_and_missing() {
    assert!(!AgentHost::audio_needs_sync(false, false, false, false, false));
    assert!(!AgentHost::audio_needs_sync(true, true, true, true, false));
    assert!(AgentHost::audio_needs_sync(true, false, false, false, false));
    assert!(AgentHost::audio_needs_sync(false, false, true, false, false));
    assert!(AgentHost::audio_needs_sync(false, false, true, true, true));
}

#[test]
fn guard_tolerates_events_after_dispose_and_double_dispose() {
    let g = GuardrailService::new(Settings::default, || false, Vec::new, Path::new(r"C:\nowhere"));
    g.stop();
    g.refresh();
    g.on_foreground_event();
    g.stop();
}

// ---------------- Sweep3JAgentTests (guardrail, session, clock, gate) ----------------

#[test]
fn a1_stale_snapshot_masks_the_new_foreground_window_not_the_canvas() {
    let fg = WindowInfo {
        hwnd: 42,
        pid: 1,
        exe_path: r"C:\a\app.exe".to_string(),
        title: "App".to_string(),
        class_name: "Cls".to_string(),
    };
    let snap = GuardSnapshot {
        taken_at_ms: br_agent::native::tick_count64(),
        foreground: Some(fg),
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
    assert_eq!(None, GuardrailService::unscanned_foreground_pure(&snap, 3, 42, Some(bounds)));
    assert_eq!(Some(bounds), GuardrailService::unscanned_foreground_pure(&snap, 4, 42, Some(bounds)));
    assert_eq!(Some(bounds), GuardrailService::unscanned_foreground_pure(&snap, 3, 77, Some(bounds)));
    assert_eq!(Some(MASK_EVERYTHING), GuardrailService::unscanned_foreground_pure(&snap, 4, 77, None));
    assert_eq!(None, GuardrailService::unscanned_foreground_pure(&snap, 4, 0, None));
    assert_eq!(None, GuardrailService::unscanned_foreground_pure(&GuardSnapshot::closed(), 4, 77, Some(bounds)));
}

#[test]
fn a8_root_name_private_tag() {
    let cases = [
        (Some("New Tab - Google Chrome (Incognito)"), true),
        (Some("Bing and 2 more pages - Personal - Microsoft Edge (InPrivate)"), true),
        (Some("Start - Brave [Private]"), true),
        (Some("Neuer Tab - Google Chrome (Inkognito)"), true),
        (Some("Docs - Google Chrome"), false),
        (Some("Budget (draft) - Google Chrome"), false),
        (Some(""), false),
        (None, false),
    ];
    for (name, expected) in cases {
        assert_eq!(expected, BrowserBadgeDetector::root_name_is_private(name), "name={name:?}");
    }
}

#[test]
fn a8_no_toolbar_result() {
    let cases = [
        (Some("Mail - Outlook"), BadgeResult::NoBadge),
        (Some("DevTools - example.com/"), BadgeResult::NoBadge),
        (Some("Picture in picture"), BadgeResult::NoBadge),
        (Some("YouTube - Google Chrome (Incognito)"), BadgeResult::Private),
        (Some(""), BadgeResult::CheckFailed),
        (None, BadgeResult::CheckFailed),
    ];
    for (root, expected) in cases {
        assert_eq!(expected, BrowserBadgeDetector::no_toolbar_result(root), "root={root:?}");
    }
}

#[test]
fn a8_edge_badge_name_is_matched() {
    assert!(BrowserBadgeDetector::BADGE_NAMES.contains(&"InPrivate browsing"));
}

#[test]
fn a14_session_lock_state() {
    assert_eq!(Some(true), session_locked(0, 0));
    assert_eq!(Some(false), session_locked(0, 1));
    assert_eq!(Some(true), session_locked(4, 1));
    assert_eq!(None, session_locked(0, -1));
}

#[test]
fn a14_query_answers_on_this_session() {
    assert!(session_locked_now().is_some());
}

#[test]
fn a15_failed_password_search_returns_null() {
    let handle = thread::spawn(|| {
        let com = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        }
        .is_ok();
        let mut p = PasswordFieldProbe::new().expect("PasswordFieldProbe::new");
        let res = p.find(0x7FFF_0F01);
        drop(p);
        if com {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
        res
    });
    let res = handle.join().expect("probe thread");
    assert!(res.is_none());
}

#[test]
fn a15_password_warning_is_rate_limited_and_fires_first() {
    assert!(GuardrailService::warn_due(i64::MIN, 5));
    assert!(GuardrailService::warn_due(i64::MIN, i64::MIN + 1));
    assert!(!GuardrailService::warn_due(1_000, 1_000 + 599_999));
    assert!(GuardrailService::warn_due(1_000, 1_000 + 600_000));
}

#[test]
fn a17_pause_survives_a_wall_clock_step() {
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

#[test]
fn a17_read_gate_timeout_ignores_a_backwards_step() {
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

#[test]
fn d4_user_lock_closes_the_gate_without_require_unlock() {
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

// ---------------- Sweep3LAgentTests ----------------

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

#[test]
fn agent_log_stamps_local_time_with_offset() {
    let offset = FixedOffset::east_opt(7 * 3600).unwrap();
    let dt = offset
        .with_ymd_and_hms(2026, 11, 16, 4, 5, 6)
        .unwrap()
        + ChronoDuration::milliseconds(789);
    assert_eq!("2026-11-16T04:05:06.789+07:00", AgentLog::stamp(dt));
}

// ---------------- Sweep4LAgentTests ----------------

fn setup_stub_dir(dir: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
    let root = dir.path().join("app");
    let install = root.join("0.2.3-dev-1df40c2e4158");
    let download = dir
        .path()
        .join("Downloads")
        .join("BetterRecall-0.2.3-dev-win-x64.exe");
    fs::create_dir_all(&install).unwrap();
    fs::write(install.join(InstallLayout::AGENT_EXE), []).unwrap();
    fs::create_dir_all(download.parent().unwrap()).unwrap();
    fs::write(&download, []).unwrap();
    let stub = root
        .join(InstallLayout::BIN_DIR)
        .join(InstallLayout::AUTOSTART_EXE);
    (root, install, download)
        .pipe_stub(stub)
}

trait TupleStubExt {
    fn pipe_stub(self, stub: PathBuf) -> (PathBuf, PathBuf, PathBuf);
}

impl TupleStubExt for (PathBuf, PathBuf, PathBuf) {
    fn pipe_stub(self, _stub: PathBuf) -> (PathBuf, PathBuf, PathBuf) {
        self
    }
}

#[test]
fn c6_launcher_started_agent_registers_the_stable_stub_not_the_download() {
    let dir = TempDir::new("s4l");
    let (root, install, download) = setup_stub_dir(&dir);
    let stub = root
        .join(InstallLayout::BIN_DIR)
        .join(InstallLayout::AUTOSTART_EXE);
    fs::create_dir_all(stub.parent().unwrap()).unwrap();
    fs::write(&stub, []).unwrap();
    fs::write(
        root.join(InstallLayout::BIN_DIR).join(InstallLayout::CURRENT_FILE),
        install.to_string_lossy().as_bytes(),
    )
    .unwrap();
    assert_eq!(stub, StartupRegistration::autostart_exe(&install, Some(&download)));
}

#[test]
fn c6_agent_started_from_the_ui_registers_the_stable_stub_not_its_versioned_exe() {
    let dir = TempDir::new("s4l");
    let (root, install, _) = setup_stub_dir(&dir);
    let stub = root
        .join(InstallLayout::BIN_DIR)
        .join(InstallLayout::AUTOSTART_EXE);
    fs::create_dir_all(stub.parent().unwrap()).unwrap();
    fs::write(&stub, []).unwrap();
    fs::write(
        root.join(InstallLayout::BIN_DIR).join(InstallLayout::CURRENT_FILE),
        install.to_string_lossy().as_bytes(),
    )
    .unwrap();
    assert_eq!(stub, StartupRegistration::autostart_exe(&install, None));
}

#[test]
fn c6_without_a_stub_the_old_order_holds() {
    let dir = TempDir::new("s4l");
    let (root, install, download) = setup_stub_dir(&dir);
    let stub = root
        .join(InstallLayout::BIN_DIR)
        .join(InstallLayout::AUTOSTART_EXE);
    assert_eq!(download, StartupRegistration::autostart_exe(&install, Some(&download)));
    assert_eq!(
        InstallLayout::full(&install, InstallLayout::AGENT_EXE),
        StartupRegistration::autostart_exe(&install, None)
    );
    let gone = PathBuf::from(format!("{}.gone", download.display()));
    assert_eq!(
        InstallLayout::full(&install, InstallLayout::AGENT_EXE),
        StartupRegistration::autostart_exe(&install, Some(&gone))
    );
    fs::create_dir_all(stub.parent().unwrap()).unwrap();
    fs::write(&stub, []).unwrap();
    assert_eq!(download, StartupRegistration::autostart_exe(&install, Some(&download)));
}

// ---------------- Sweep4PAgentTests / Sweep4RAgentTests / Sweep4UAgentTests / Sweep5UAgentTests ----------------

#[test]
fn a_m2_app_and_site_purges_reach_the_ring_however_old_their_rows() {
    let now = Utc::now();
    let old = now - ChronoDuration::days(10);
    let app_notice = PurgeNotice {
        reason: "purge".to_string(),
        ids: Vec::new(),
        from: Some(old),
        to: Some(old),
        newest: Some(old),
        scope: Some("app".to_string()),
    };
    let site_notice = PurgeNotice {
        scope: Some("site".to_string()),
        ..app_notice.clone()
    };
    let range_notice = PurgeNotice {
        scope: Some("range".to_string()),
        ..app_notice.clone()
    };
    assert!(AgentHost::reaches_ring(&app_notice, now, 60));
    assert!(AgentHost::reaches_ring(&site_notice, now, 60));
    assert!(!AgentHost::reaches_ring(&range_notice, now, 60));
}

#[test]
fn ah2_audio_verdict_follows_the_guard_snapshot() {
    let fg = WindowInfo {
        hwnd: 42,
        pid: 1,
        exe_path: r"C:\Windows\System32\mstsc.exe".to_string(),
        title: "Remote Desktop".to_string(),
        class_name: "Cls".to_string(),
    };
    let snap = |d: GuardDecision, epoch: i64| GuardSnapshot {
        taken_at_ms: br_agent::native::tick_count64(),
        foreground: Some(fg.clone()),
        decision: d,
        badge: BadgeResult::NotBrowser,
        any_affinity: false,
        mask_rects: Vec::new(),
        per_monitor: HashMap::new(),
        site: String::new(),
        secrets: None,
        fg_epoch: epoch,
        is_closed_sentinel: false,
    };

    assert_eq!(
        AudioVerdict {
            gate: AudioGate::Blocked,
            app: "mstsc".to_string(),
            epoch: 3
        },
        GuardrailService::audio_now_pure(&snap(GuardDecision::deny(GuardReason::DenyProcess), 3), 3)
    );
    assert_eq!(
        AudioGate::Blocked,
        GuardrailService::audio_now_pure(&snap(GuardDecision::deny(GuardReason::PrivateBadge), 3), 3).gate
    );
    assert_eq!(
        AudioGate::Blocked,
        GuardrailService::audio_now_pure(&snap(GuardDecision::deny(GuardReason::DisplayAffinity), 3), 3).gate
    );
    assert_eq!(
        AudioGate::Blocked,
        GuardrailService::audio_now_pure(&GuardSnapshot::closed(), 3).gate
    );
    assert_eq!(
        AudioVerdict {
            gate: AudioGate::Allowed,
            app: "mstsc".to_string(),
            epoch: 3
        },
        GuardrailService::audio_now_pure(&snap(GuardDecision::ALLOWED, 3), 3)
    );
    assert_eq!(
        AudioGate::Allowed,
        GuardrailService::audio_now_pure(&snap(GuardDecision::deny(GuardReason::NoForeground), 3), 3).gate
    );
    assert_eq!(
        AudioVerdict {
            gate: AudioGate::Unknown,
            app: "mstsc".to_string(),
            epoch: 4
        },
        GuardrailService::audio_now_pure(&snap(GuardDecision::ALLOWED, 3), 4)
    );
    assert_eq!(
        AudioVerdict {
            gate: AudioGate::Unknown,
            app: "mstsc".to_string(),
            epoch: 5
        },
        GuardrailService::audio_now_pure(&snap(GuardDecision::ALLOWED, 3), 5)
    );
}

#[test]
fn s3_turning_text_detection_on_is_noticed() {
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

#[test]
fn b11_ipc_resume_keeps_the_users_pause() {
    let p = PauseController::new();
    p.set("user", None);
    p.set("ipc", Some(Utc::now() + ChronoDuration::minutes(1)));
    AgentHost::clear_for_resume(&p, "ipc");
    assert!(p.has("user"));
    assert!(!p.has("ipc"));
    assert_eq!(Some("user".to_string()), p.current().0);
}

#[test]
fn b11_user_resume_clears_both_pauses() {
    let p = PauseController::new();
    p.set("user", None);
    p.set("ipc", None);
    AgentHost::clear_for_resume(&p, "user");
    assert!(!p.has("user"));
    assert!(!p.has("ipc"));
    assert!(p.current().0.is_none());
}

#[test]
fn s11_retention_waits_for_a_readable_settings_file_and_runs_after_reset() {
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

#[test]
fn b5_sync_key_lock_pause_and_replay_pause_cover_passphrase_purge_all_window() {
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

#[test]
fn b5_on_purged_all_in_passphrase_mode_sets_keylocked_pause_and_clears_capturing() {
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

#[test]
fn c4_end_session_cancels_maintenance_and_checkpoints_real_store() {
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
    core.open_store(&br_core::CancelToken::none()).unwrap();
    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    agent.end_session_shutdown();
    assert!(
        core.store()
            .unwrap()
            .checkpoint_timeout(std::time::Duration::from_secs(5))
            .unwrap()
    );
    agent.shutdown();
    core.shutdown();
}

// ---------------- SmartCaptureTests: StorageGuardTests ----------------

struct FakeStorageStore {
    use_bytes: Mutex<br_contracts::StorageUse>,
    deletable: Mutex<i32>,
    freed_per_row: Mutex<i64>,
    delete_calls: std::sync::atomic::AtomicI32,
}

impl FakeStorageStore {
    fn new(file_bytes: i64, free_bytes: i64) -> Arc<Self> {
        Arc::new(Self {
            use_bytes: Mutex::new(br_contracts::StorageUse {
                file_bytes,
                free_bytes,
            }),
            deletable: Mutex::new(0),
            freed_per_row: Mutex::new(0),
            delete_calls: std::sync::atomic::AtomicI32::new(0),
        })
    }

    fn set_deletable(&self, n: i32) {
        *self.deletable.lock().unwrap() = n;
    }

    fn deletable(&self) -> i32 {
        *self.deletable.lock().unwrap()
    }

    fn set_freed_per_row(&self, n: i64) {
        *self.freed_per_row.lock().unwrap() = n;
    }

    fn use_now(&self) -> br_contracts::StorageUse {
        *self.use_bytes.lock().unwrap()
    }

    fn delete_calls(&self) -> i32 {
        self.delete_calls.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl br_agent::host::StorageStore for FakeStorageStore {
    fn get_storage_use(&self) -> br_contracts::StorageUse {
        *self.use_bytes.lock().unwrap()
    }

    fn delete_for_space(&self, max_rows: i32) -> i32 {
        self.delete_calls
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let mut del = self.deletable.lock().unwrap();
        let n = max_rows.min(*del);
        *del -= n;
        let per = *self.freed_per_row.lock().unwrap();
        let mut u = self.use_bytes.lock().unwrap();
        u.free_bytes = u.file_bytes.min(u.free_bytes + (n as i64) * per);
        n
    }
}

fn make_storage_guard(
    disk_free: i64,
    files: i64,
    free: i64,
    overwrite: bool,
) -> (
    br_agent::host::StorageGuard,
    Arc<FakeStorageStore>,
    Arc<Mutex<i64>>,
) {
    let store = FakeStorageStore::new(files, free);
    let mut s = Settings::default();
    s.storage.min_free_gb = 25;
    s.storage.max_gb = 20;
    s.storage.overwrite = overwrite;
    let disk = Arc::new(Mutex::new(disk_free));
    let st_cb = store.clone();
    let d_cb = disk.clone();
    let g = br_agent::host::StorageGuard::new(
        move || Some(st_cb.clone() as Arc<dyn br_agent::host::StorageStore>),
        move || s.clone(),
        move || Some(*d_cb.lock().unwrap()),
    );
    (g, store, disk)
}

#[test]
fn low_disk_pauses_once_and_resumes_with_margin() {
    let gb = br_agent::host::StorageGuard::GB;
    let (g, store, disk) = make_storage_guard(24 * gb, 5 * gb, 0, false);
    let d = g.check();
    assert!(d.pause);
    assert!(d.notify);
    assert_eq!("disk", d.why);
    assert!(!g.check().notify); // balloon once
    *disk.lock().unwrap() = 25 * gb + gb / 2; // above limit, inside margin: stays paused
    assert!(g.check().pause);
    *disk.lock().unwrap() = 26 * gb + 1;
    assert!(!g.check().pause);
    assert_eq!(0, store.delete_calls());
}

#[test]
fn reusable_free_pages_count_as_free_space() {
    let gb = br_agent::host::StorageGuard::GB;
    let (g, _, _) = make_storage_guard(24 * gb, 6 * gb, 2 * gb, false);
    assert!(!g.check().pause);
}

#[test]
fn over_size_pauses_without_overwrite() {
    let gb = br_agent::host::StorageGuard::GB;
    let (g, _, _) = make_storage_guard(500 * gb, 21 * gb, 0, false);
    let d = g.check();
    assert!(d.pause);
    assert_eq!("size", d.why);
}

#[test]
fn overwrite_deletes_until_margin_instead_of_pausing() {
    let gb = br_agent::host::StorageGuard::GB;
    let (g, store, _) = make_storage_guard(500 * gb, 21 * gb, 0, true);
    store.set_deletable(100_000);
    store.set_freed_per_row(1024 * 1024); // 1 MiB per capture
    let d = g.check();
    assert!(!d.pause);
    assert!(d.deleted > 0);
    assert!(
        store.use_now().live_bytes()
            <= 20 * gb - br_agent::host::StorageGuard::margin(&br_contracts::StorageSettings::default())
    );
}

#[test]
fn overwrite_pauses_when_nothing_is_left_to_delete() {
    let gb = br_agent::host::StorageGuard::GB;
    let (g, store, _) = make_storage_guard(24 * gb, 5 * gb, 0, true);
    store.set_deletable(3);
    store.set_freed_per_row(1024);
    let d = g.check();
    assert!(d.pause);
    assert_eq!(3, d.deleted);
    assert_eq!("disk", d.why);
}

#[test]
fn overwrite_keeps_the_store_when_deleting_cannot_free_enough() {
    let gb = br_agent::host::StorageGuard::GB;
    let (g, store, _) = make_storage_guard(10 * gb, gb, 0, true);
    store.set_deletable(100_000);
    store.set_freed_per_row(1024 * 1024);
    let d = g.check();
    assert!(d.pause);
    assert!(d.notify);
    assert_eq!(0, d.deleted);
    assert_eq!(0, store.delete_calls());
    assert_eq!("disk", d.why);
}

#[test]
fn overwrite_deletes_for_disk_when_the_store_covers_the_shortfall() {
    let gb = br_agent::host::StorageGuard::GB;
    let (g, store, _) = make_storage_guard(24 * gb, 5 * gb, 0, true);
    store.set_deletable(100_000);
    store.set_freed_per_row(1024 * 1024);
    let d = g.check();
    assert!(!d.pause);
    assert!(d.deleted > 0);
    assert!(store.deletable() > 0); // only what the shortfall needed
}

// ---------------- CaptureRecoveryTests: GuardFreshnessTests ----------------

fn freshness_snap(fg: isize, epoch: i64) -> br_agent::guardrails::GuardSnapshot {
    br_agent::guardrails::GuardSnapshot {
        taken_at_ms: br_agent::native::tick_count64(),
        foreground: Some(WindowInfo {
            hwnd: fg,
            pid: 1,
            exe_path: r"C:\Tools\notepad.exe".to_string(),
            title: "Doc".to_string(),
            class_name: "Notepad".to_string(),
        }),
        decision: GuardDecision::ALLOWED,
        badge: BadgeResult::NotBrowser,
        any_affinity: false,
        mask_rects: Vec::new(),
        per_monitor: std::collections::HashMap::new(),
        site: String::new(),
        secrets: None,
        fg_epoch: epoch,
        is_closed_sentinel: false,
    }
}

#[test]
fn snapshot_of_the_current_foreground_is_used() {
    let s = freshness_snap(100, 7);
    let r = br_agent::guardrails::GuardrailService::fresh_for_grab(&s, 7, 100);
    assert_eq!(s.taken_at_ms, r.taken_at_ms);
    assert_eq!(s.decision, r.decision);
    assert_eq!(s.fg_epoch, r.fg_epoch);
    assert!(!r.is_closed_sentinel);
}

#[test]
fn foreground_change_after_the_scan_started_denies() {
    let s = freshness_snap(100, 7);
    let r = br_agent::guardrails::GuardrailService::fresh_for_grab(&s, 8, 100);
    assert!(!r.decision.allow);
    assert_eq!(GuardReason::ForegroundChanged, r.decision.reason);
    let r = br_agent::guardrails::GuardrailService::fresh_for_grab(&s, 7, 200);
    assert!(!r.decision.allow);
    assert_eq!(GuardReason::ForegroundChanged, r.decision.reason);
}

#[test]
fn closed_snapshot_stays_closed() {
    let closed = br_agent::guardrails::GuardSnapshot::closed();
    let r = br_agent::guardrails::GuardrailService::fresh_for_grab(&closed, 0, 0);
    assert!(r.is_closed_sentinel);
    assert!(!r.decision.allow);
    assert_eq!(GuardReason::NoForeground, r.decision.reason);
}

// ---------------- Sweep2FTests: BadgeFailClosedTests ----------------

fn browser_win(exe: &str, title: &str) -> WindowInfo {
    WindowInfo {
        hwnd: 42,
        pid: 1,
        exe_path: format!(r"C:\Apps\{exe}"),
        title: title.to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
    }
}

#[test]
fn background_browser_window_is_hidden_unless_its_probe_says_normal() {
    let cases = [
        (BadgeResult::Private, true),
        (BadgeResult::CheckFailed, true),
        (BadgeResult::NotChecked, true),
        (BadgeResult::NoBadge, false),
        (BadgeResult::NotBrowser, false),
    ];
    for (result, hidden) in cases {
        assert_eq!(
            hidden,
            GuardrailPolicy::hides_browser_window(
                &browser_win("chrome.exe", "Some page"),
                Some(|_| result),
            )
        );
        assert_eq!(hidden, GuardrailPolicy::badge_hides(result));
    }
}

#[test]
fn no_ui_automation_hides_every_titled_browser_window_only() {
    assert!(GuardrailPolicy::hides_browser_window::<fn(isize) -> BadgeResult>(
        &browser_win("msedge.exe", "Some page"),
        None,
    ));
    assert!(!GuardrailPolicy::hides_browser_window::<fn(isize) -> BadgeResult>(
        &browser_win("notepad.exe", "Some page"),
        None,
    ));
    assert!(!GuardrailPolicy::hides_browser_window::<fn(isize) -> BadgeResult>(
        &browser_win("chrome.exe", ""),
        None,
    ));
    assert!(!GuardrailPolicy::hides_browser_window(
        &browser_win("notepad.exe", "Some page"),
        Some(|_| BadgeResult::CheckFailed),
    ));
}

// ---------------- Sweep4PAgentTests & Sweep5UAgentTests: EndSession ----------------

#[test]
fn c3_end_session_stops_the_pipes() {
    let dir = TempDir::new("c3_end_session");
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
    let pipe_stops = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let ps = pipe_stops.clone();
    agent.set_on_stop_pipes_for_test(Some(Arc::new(move || {
        ps.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    })));
    agent.on_end_session_for_test();
    assert_eq!(1, pipe_stops.load(std::sync::atomic::Ordering::Acquire));
    agent.shutdown();
    core.shutdown();
}

// Rust-side regression (not a C# port): the port first joined the writer stops without a limit, so a stuck pipe stop
// held up logoff. The C# agent waits 2.5 s for them; so does this one, and the stuck stop keeps running.
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
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    agent.set_on_stop_pipes_for_test(Some(Arc::new(move || {
        let _ = release_rx.lock().unwrap().recv();
    })));
    let started = std::time::Instant::now();
    agent.end_session_shutdown();
    let took = started.elapsed();
    assert!(took >= std::time::Duration::from_millis(2400), "returned before the budget: {took:?}");
    assert!(took < std::time::Duration::from_millis(4500), "waited past the budget: {took:?}");
    release_tx.send(()).unwrap();
    agent.shutdown();
    core.shutdown();
}

#[test]
fn c4_end_session_shutdown_dedupes_writer_and_pipe_stops_with_dispose() {
    let dir = TempDir::new("c4_end_session");
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
    let pipe_stops = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let ps = pipe_stops.clone();
    agent.set_on_stop_pipes_for_test(Some(Arc::new(move || {
        ps.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    })));
    agent.end_session_shutdown();
    agent.shutdown();
    assert_eq!(1, pipe_stops.load(std::sync::atomic::Ordering::Acquire));
    core.shutdown();
}
