// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::path::Path;
use std::thread;

use br_contracts::{PrivacySettings, Settings};

use br_agent::guardrails::{
    AudioGate, AudioVerdict, BadgeResult, BrowserBadgeDetector, GuardDecision, GuardReason,
    GuardSnapshot, GuardrailPolicy, GuardrailService, PasswordFieldProbe, WindowInfo,
};
use br_agent::native::{session_locked, session_locked_now};

fn win(exe: &str, title: &str) -> WindowInfo {
    WindowInfo {
        hwnd: 1,
        pid: 42,
        exe_path: exe.to_string(),
        title: title.to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
    }
}

fn browser_win(exe: &str, title: &str) -> WindowInfo {
    WindowInfo {
        hwnd: 42,
        pid: 1,
        exe_path: format!(r"C:\Apps\{exe}"),
        title: title.to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
    }
}

fn freshness_snap(fg: isize, epoch: i64) -> GuardSnapshot {
    GuardSnapshot {
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
        per_monitor: HashMap::new(),
        site: String::new(),
        secrets: None,
        fg_epoch: epoch,
        is_closed_sentinel: false,
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
        let d = GuardrailPolicy::evaluate(
            Some(&win(exe, "Doc")),
            &p,
            false,
            false,
            BadgeResult::NotChecked,
        );
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
    assert!(GuardrailPolicy::title_matches(
        "New tab - [InPrivate] - Microsoft Edge",
        &pats
    ));
    assert!(GuardrailPolicy::title_matches(
        "My BANK - March Statement.pdf",
        &pats
    ));
    assert!(!GuardrailPolicy::title_matches("Statement of bank", &pats));
    let p_inpriv = PrivacySettings {
        deny_title_patterns: vec!["InPrivate".to_string()],
        ..Default::default()
    };
    assert_eq!("", GuardrailPolicy::filter_title("x - inprivate", &p_inpriv));
    assert_eq!(
        "Inbox",
        GuardrailPolicy::filter_title("Inbox", &PrivacySettings::default())
    );
}

#[test]
fn session_lock_affinity_badge_and_deny_priority() {
    let p = PrivacySettings::default();
    let chrome = win(r"C:\Program Files\Google\Chrome\Application\chrome.exe", "Google");
    let onepass = win(r"C:\x\1Password.exe", "Doc");

    type Case<'a> = (Option<&'a WindowInfo>, bool, bool, BadgeResult, bool, GuardReason);
    let cases: &[Case] = &[
        (Some(&chrome), true, false, BadgeResult::NoBadge, false, GuardReason::SessionLocked),
        (None, false, false, BadgeResult::NoBadge, false, GuardReason::NoForeground),
        // Deny-list wins over display affinity and private badge.
        (Some(&onepass), false, true, BadgeResult::Private, false, GuardReason::DenyProcess),
        (Some(&chrome), false, true, BadgeResult::NoBadge, false, GuardReason::DisplayAffinity),
        (Some(&chrome), false, false, BadgeResult::Private, false, GuardReason::PrivateBadge),
        (Some(&chrome), false, false, BadgeResult::CheckFailed, false, GuardReason::BadgeCheckFailed),
        (Some(&chrome), false, false, BadgeResult::NotChecked, false, GuardReason::BadgeCheckFailed),
        (Some(&chrome), false, false, BadgeResult::NoBadge, true, GuardReason::None),
    ];
    for &(w, locked, affinity, badge, allow, reason) in cases {
        let d = GuardrailPolicy::evaluate(w, &p, locked, affinity, badge);
        assert_eq!(allow, d.allow, "reason={reason:?}");
        assert_eq!(reason, d.reason);
    }

    let off = PrivacySettings {
        pause_on_lock: false,
        honor_display_affinity: false,
        browser_badge_detection: false,
        ..Default::default()
    };
    assert!(
        GuardrailPolicy::evaluate(Some(&chrome), &off, true, true, BadgeResult::CheckFailed).allow
    );
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
        assert_eq!(
            site,
            GuardrailPolicy::site_from_address(Some(text)),
            "text={text}"
        );
    }
}

#[test]
fn guard_tolerates_events_after_dispose_and_double_dispose() {
    let g = GuardrailService::new(
        Settings::default,
        || false,
        Vec::new,
        Path::new(r"C:\nowhere"),
    );
    g.stop();
    g.refresh();
    g.on_foreground_event();
    g.stop();
}

// sweep 3J A8
#[test]
fn root_name_private_tag() {
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
        assert_eq!(
            expected,
            BrowserBadgeDetector::root_name_is_private(name),
            "name={name:?}"
        );
    }
}

// sweep 3J A8
#[test]
fn no_toolbar_result() {
    let cases = [
        (Some("Mail - Outlook"), BadgeResult::NoBadge),
        (Some("DevTools - example.com/"), BadgeResult::NoBadge),
        (Some("Picture in picture"), BadgeResult::NoBadge),
        (Some("YouTube - Google Chrome (Incognito)"), BadgeResult::Private),
        (Some(""), BadgeResult::CheckFailed),
        (None, BadgeResult::CheckFailed),
    ];
    for (root, expected) in cases {
        assert_eq!(
            expected,
            BrowserBadgeDetector::no_toolbar_result(root),
            "root={root:?}"
        );
    }
}

// sweep 3J A8
#[test]
fn failed_uia_search_fails_closed() {
    assert_eq!(
        BadgeResult::NoBadge,
        BrowserBadgeDetector::search_error_result(true, BadgeResult::NoBadge)
    );
    assert_eq!(
        BadgeResult::CheckFailed,
        BrowserBadgeDetector::search_error_result(false, BadgeResult::NoBadge)
    );
    // windows-rs reports "nothing found" as an error with code S_OK; a real failure is not S_OK.
    assert!(windows::core::Error::empty().code().is_ok());
    assert!(
        !windows::core::Error::from_hresult(windows::core::HRESULT(0x80131505u32 as i32))
            .code()
            .is_ok()
    );
}

// sweep 3J A8
#[test]
fn edge_badge_name_is_matched() {
    assert!(BrowserBadgeDetector::BADGE_NAMES.contains(&"InPrivate browsing"));
}

// sweep 3J A14
#[test]
fn session_lock_state() {
    assert_eq!(Some(true), session_locked(0, 0));
    assert_eq!(Some(false), session_locked(0, 1));
    assert_eq!(Some(true), session_locked(4, 1));
    assert_eq!(None, session_locked(0, -1));
}

// sweep 3J A14
#[test]
fn query_answers_on_this_session() {
    assert!(session_locked_now().is_some());
}

// sweep 3J A15
#[test]
fn failed_password_search_returns_null() {
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

// sweep 3J A15
#[test]
fn password_warning_is_rate_limited_and_fires_first() {
    assert!(GuardrailService::warn_due(i64::MIN, 5));
    assert!(GuardrailService::warn_due(i64::MIN, i64::MIN + 1));
    assert!(!GuardrailService::warn_due(1_000, 1_000 + 599_999));
    assert!(GuardrailService::warn_due(1_000, 1_000 + 600_000));
}

// sweep 4R AH2
#[test]
fn audio_verdict_follows_the_guard_snapshot() {
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
        GuardrailService::audio_now_pure(
            &snap(GuardDecision::deny(GuardReason::DenyProcess), 3),
            3
        )
    );
    assert_eq!(
        AudioGate::Blocked,
        GuardrailService::audio_now_pure(
            &snap(GuardDecision::deny(GuardReason::PrivateBadge), 3),
            3
        )
        .gate
    );
    assert_eq!(
        AudioGate::Blocked,
        GuardrailService::audio_now_pure(
            &snap(GuardDecision::deny(GuardReason::DisplayAffinity), 3),
            3
        )
        .gate
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
        GuardrailService::audio_now_pure(
            &snap(GuardDecision::deny(GuardReason::NoForeground), 3),
            3
        )
        .gate
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
fn snapshot_of_the_current_foreground_is_used() {
    let s = freshness_snap(100, 7);
    let r = GuardrailService::fresh_for_grab(&s, 7, 100);
    assert_eq!(s.taken_at_ms, r.taken_at_ms);
    assert_eq!(s.decision, r.decision);
    assert_eq!(s.fg_epoch, r.fg_epoch);
    assert!(!r.is_closed_sentinel);
}

#[test]
fn foreground_change_after_the_scan_started_denies() {
    let s = freshness_snap(100, 7);
    let r = GuardrailService::fresh_for_grab(&s, 8, 100);
    assert!(!r.decision.allow);
    assert_eq!(GuardReason::ForegroundChanged, r.decision.reason);
    let r = GuardrailService::fresh_for_grab(&s, 7, 200);
    assert!(!r.decision.allow);
    assert_eq!(GuardReason::ForegroundChanged, r.decision.reason);
}

#[test]
fn closed_snapshot_stays_closed() {
    let closed = GuardSnapshot::closed();
    let r = GuardrailService::fresh_for_grab(&closed, 0, 0);
    assert!(r.is_closed_sentinel);
    assert!(!r.decision.allow);
    assert_eq!(GuardReason::NoForeground, r.decision.reason);
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

#[test]
fn d1_path_cache_keyed_by_pid_and_creation_time() {
    use br_agent::guardrails::WindowInspector;
    use std::sync::atomic::{AtomicUsize, Ordering as O};
    WindowInspector::clear_path_cache();
    let pid = 99_999;
    let path_calls = AtomicUsize::new(0);
    let path_of = |p: &'static str| {
        let calls = &path_calls;
        move |_| {
            calls.fetch_add(1, O::SeqCst);
            Some(p.to_string())
        }
    };

    // Miss: path queried and cached under (pid, 1000).
    let p1 = WindowInspector::process_path_with(pid, |_| Some(1000), path_of(r"C:\App\allowed.exe"));
    assert_eq!(r"C:\App\allowed.exe", p1);
    assert_eq!(1, path_calls.load(O::SeqCst));

    // Hit: same pid and creation time, path not queried again.
    let p2 = WindowInspector::process_path_with(pid, |_| Some(1000), path_of(r"C:\App\wrong.exe"));
    assert_eq!(r"C:\App\allowed.exe", p2);
    assert_eq!(1, path_calls.load(O::SeqCst));

    // Reused PID (new creation time): miss, the new image path is returned.
    let p3 = WindowInspector::process_path_with(pid, |_| Some(2000), path_of(r"C:\App\denied.exe"));
    assert_eq!(r"C:\App\denied.exe", p3);
    assert_eq!(2, path_calls.load(O::SeqCst));

    // A failed open is never cached.
    let failed = WindowInspector::process_path_with(pid + 1, |_| Some(3000), |_| None);
    assert_eq!("", failed);
    let later = WindowInspector::process_path_with(pid + 1, |_| Some(3000), path_of(r"C:\App\x.exe"));
    assert_eq!(r"C:\App\x.exe", later);
}
