// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sweep 6 G3 ported tests for named-pipe streaming (`subscribe`), stale settings snapshot,
//! UI 8-instance cap, `StepClock` edge cases, clock-stepped trust, junction normalization,
//! reconnect/reject audit throttling, and `CoreHost` purge-all listeners.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{
    CaptureKind, CaptureMeta, CoreHostOptions, DataPaths, Envelope, PurgeNotice, PurgeScope,
};
use br_core::{
    CancelToken, ClientSession, CommandDeps, CommandProcessor, CoreHost, KeyVault, PeerInfo, PipeClient,
    PipeConnectResult, PipeKind, PipeManager, PipeServer, ProcessInfo, SettingsStore, StepClock,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Value, json};

use common::a3::{FakeAgent, FakeEmbedder, FakeThumbs, test_pipe_name};
use common::{TempHome, TempStore, audio, screen};

fn read_event_until(
    client: &mut PipeClient,
    timeout: Duration,
    mut pred: impl FnMut(&Value) -> bool,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match client.read_line_timeout(left.min(Duration::from_millis(500)), &CancelToken::none()) {
            Ok(Some(line)) => {
                if let Ok((true, Some(v), _)) = Envelope::parse(&line)
                    && pred(&v)
                {
                    return Some(v);
                }
            }
            Ok(None) => return None,
            Err(e) => panic!("read_event_until pipe error: {e}"),
        }
    }
    None
}

/// Reads raw lines until `pred` matches one; returns whether it matched and every line read.
fn read_lines_until(
    client: &mut PipeClient,
    timeout: Duration,
    mut pred: impl FnMut(&str) -> bool,
) -> (bool, Vec<String>) {
    let deadline = Instant::now() + timeout;
    let mut lines = Vec::new();
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match client.read_line_timeout(left.min(Duration::from_millis(500)), &CancelToken::none()) {
            Ok(Some(line)) => {
                let hit = pred(&line);
                lines.push(line);
                if hit {
                    return (true, lines);
                }
            }
            Ok(None) => return (false, lines),
            Err(e) => panic!("read_lines_until pipe error: {e}"),
        }
    }
    (false, lines)
}

#[test]
fn streams_ui_capture_gate_public_heartbeat_removed_events_and_purge_all_before_reopen() {
    let mut store_tmp = TempStore::new();
    let store = Arc::new(store_tmp.store.take().unwrap());
    let home = TempHome::new(Some("dpapi"));
    let settings = Arc::new(SettingsStore::load(home.path()));
    assert!(
        settings
            .try_update(&json!({"ipc": {"enabled": true}}))
            .is_none()
    );
    let s_for_vault = settings.clone();
    let vault = Arc::new(KeyVault::new(
        home.path(),
        move || s_for_vault.current(),
        None,
        false,
    ));
    vault.try_load_silent().expect("vault load in setup");
    let agent = Arc::new(FakeAgent::new());
    let st_clone = store.clone();
    let proc = Arc::new(CommandProcessor::new(CommandDeps {
        settings: settings.clone(),
        store: Arc::new(move || Some(st_clone.clone())),
        keys: vault.clone(),
        agent: agent.clone(),
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: Some(home.path().to_path_buf()),
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    }));
    proc.set_status_heartbeat_for_test(Duration::from_millis(120));

    let ui_pipe = test_pipe_name("stream-ui");
    let pub_pipe = test_pipe_name("stream-pub");
    let p_ui = proc.clone();
    let srv_ui = PipeServer::new(
        &ui_pipe,
        PipeKind::Ui,
        |_| true,
        move |conn, ct| p_ui.serve(conn, &ct),
        None,
    );
    srv_ui.start().unwrap();
    let p_pub = proc.clone();
    let srv_pub = PipeServer::new(
        &pub_pipe,
        PipeKind::Public,
        |_| true,
        move |conn, ct| p_pub.serve(conn, &ct),
        None,
    );
    srv_pub.start().unwrap();

    let self_exe = ProcessInfo::current_image().unwrap();

    // 1) Sweep 4U B7: locked public heartbeat hides store.rows (4242) and queue.pending (5).
    {
        let mut st = agent.status.lock().unwrap();
        st.store.rows = 4242;
        st.queue.pending = 5;
        st.locked = true;
    }
    agent.unlocked.store(false, Ordering::Release);
    let (r, c_pub) = PipeClient::connect(&pub_pipe, &self_exe, 2000, &CancelToken::none());
    assert_eq!(PipeConnectResult::Ok, r);
    let mut c_pub = c_pub.unwrap();
    let first = c_pub
        .request_timeout("subscribe", Duration::from_secs(3), &CancelToken::none())
        .unwrap()
        .unwrap();
    let (ok, d, _) = Envelope::parse(&first).unwrap();
    assert!(ok);
    assert!(d.unwrap().get("subscribed").is_some());
    for i in 0..2 {
        let hb = c_pub
            .read_line_timeout(Duration::from_secs(3), &CancelToken::none())
            .unwrap()
            .unwrap_or_else(|| panic!("expected locked status heartbeat frame {i}"));
        assert!(
            !hb.contains("4242"),
            "locked public heartbeat leaked store rows: {hb}"
        );
        let (ok, data, _) = Envelope::parse(&hb).unwrap();
        assert!(ok);
        let data = data.unwrap();
        assert_eq!(0, data["store"]["rows"], "heartbeat {i}: {data}");
        assert_eq!(0, data["queue"]["pending"], "heartbeat {i}: {data}");
    }

    // Store purges reach the processor the way CoreHost wires them.
    let p_weak = Arc::downgrade(&proc);
    store.on_purged(move |n| {
        if let Some(p) = p_weak.upgrade() {
            p.on_store_purged(n);
        }
    });
    let t0 = Utc::now();
    let add = |mins: i64, app: &str, title: &str| {
        store
            .add_screen_capture(&screen(t0 - ChronoDuration::minutes(mins), app, title, false))
            .unwrap()
    };
    let keep = add(6, "notepad.exe", "kept");
    let gone = add(5, "bank.exe", "purged by the CLI");
    let nsfw = add(4, "chrome.exe", "dropped later");
    let hidden = add(3, "bank.exe", "x");
    let id2 = add(2, "bank.exe", "y");

    // 2) Sweep 4U B10 (review S2): unlocked, a public captures stream hears an NSFW drop while a
    // plain public status stream stays status lines only (no `event`, no id).
    agent.unlocked.store(true, Ordering::Release);
    agent.status.lock().unwrap().locked = false;
    let (r, c_pub_cap) = PipeClient::connect(&pub_pipe, &self_exe, 2000, &CancelToken::none());
    assert_eq!(PipeConnectResult::Ok, r);
    let mut c_pub_cap = c_pub_cap.unwrap();
    let ack_pc = c_pub_cap
        .request_timeout("subscribe captures", Duration::from_secs(3), &CancelToken::none())
        .unwrap()
        .unwrap();
    assert!(Envelope::parse(&ack_pc).unwrap().0);
    store.drop_capture(&id2, "nsfw").unwrap();
    let (heard_id2, _) = read_lines_until(&mut c_pub_cap, Duration::from_secs(3), |l| {
        l.contains(id2.as_str())
    });
    assert!(heard_id2, "unlocked public captures stream must hear the drop of {id2}");
    agent.status.lock().unwrap().paused = true;
    agent.raise_status();
    let (saw_paused, pub_status_lines) =
        read_lines_until(&mut c_pub, Duration::from_secs(3), |l| l.contains("\"paused\":true"));
    assert!(saw_paused, "expected paused status marker on public status stream");
    for l in &pub_status_lines {
        assert!(!l.contains("\"event\""), "public status stream carried an event: {l}");
        assert!(!l.contains(id2.as_str()), "public status stream carried a capture id: {l}");
    }
    agent.status.lock().unwrap().paused = false;

    // 3) Sweep 4U B10: UI captures and UI status streams both hear a purge and an NSFW drop;
    // the kept capture never appears on either.
    let ui_sub = |verb: &str| {
        let (r, c) = PipeClient::connect(&ui_pipe, &self_exe, 2000, &CancelToken::none());
        assert_eq!(PipeConnectResult::Ok, r);
        let mut c = c.unwrap();
        let ack = c
            .request_timeout(verb, Duration::from_secs(3), &CancelToken::none())
            .unwrap()
            .unwrap();
        assert!(Envelope::parse(&ack).unwrap().0, "{verb}: {ack}");
        c
    };
    let mut c_ui_cap = ui_sub("subscribe captures");
    let mut c_ui_st = ui_sub("subscribe");
    assert_eq!(1, store.purge(&PurgeScope::for_id(&gone)).unwrap());
    store.drop_capture(&nsfw, "nsfw").unwrap();
    for (name, c) in [("captures", &mut c_ui_cap), ("status", &mut c_ui_st)] {
        let mut events = Vec::new();
        let mut raw = Vec::new();
        let (done, lines) = read_lines_until(c, Duration::from_secs(3), |l| {
            l.contains(nsfw.as_str()) && l.contains("\"reason\":\"drop\"")
        });
        assert!(done, "UI {name} stream never heard the drop of {nsfw}");
        for l in lines {
            if let Ok((true, Some(v), _)) = Envelope::parse(&l)
                && v.get("event").is_some()
            {
                events.push(v);
            }
            raw.push(l);
        }
        let has = |reason: &str, id: &str| {
            events.iter().any(|e| {
                e["reason"] == reason
                    && e["ids"].as_array().is_some_and(|a| a.iter().any(|i| i == id))
            })
        };
        assert!(has("purge", &gone), "UI {name}: no purge event for {gone}: {events:?}");
        assert!(has("drop", &nsfw), "UI {name}: no drop event for {nsfw}: {events:?}");
        assert!(
            !raw.iter().any(|l| l.contains(keep.as_str())),
            "UI {name}: kept id {keep} leaked: {raw:?}"
        );
    }
    c_ui_cap.close();
    c_ui_st.close();
    let mut ui_sess = ClientSession::new(PipeKind::Ui, 101, &self_exe);
    let r = proc.handle(&mut ui_sess, &format!("get {nsfw}"), &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(Some(CommandProcessor::ERR_NOT_FOUND), err.as_deref());

    let mk_meta = |id: &str| CaptureMeta {
        id: id.to_string(),
        ts: Utc::now(),
        kind: CaptureKind::Screen,
        app: "app.exe".to_string(),
        title: "t".to_string(),
        monitor: 0,
        duration_ms: 0,
        has_thumb: true,
        embedded: false,
        site: String::new(),
        replay_id: None,
        replay_offset_ms: None,
    };

    // 4) Sweep 4U B10: a locked public captures stream hears no drop (marker afterwards).
    agent.unlocked.store(false, Ordering::Release);
    store.drop_capture(&hidden, "nsfw").unwrap();
    agent.unlocked.store(true, Ordering::Release);
    agent.raise_capture(&mk_meta("s_PUB_MARKER"));
    let (got_pub_marker, pub_cap_lines) =
        read_lines_until(&mut c_pub_cap, Duration::from_secs(3), |l| l.contains("s_PUB_MARKER"));
    assert!(got_pub_marker, "expected s_PUB_MARKER");
    assert!(
        !pub_cap_lines.iter().any(|l| l.contains(hidden.as_str())),
        "locked public captures stream heard the drop: {pub_cap_lines:?}"
    );
    c_pub_cap.close();

    // 5) Sweep 3J B18: UI captures subscribe stays quiet while UI gate is closed EVEN WHEN
    // `ipc.requireUnlocked = false`! Uses marker capture s_MARKER (no fixed delays).
    assert!(
        settings
            .try_update(&json!({"ipc": {"requireUnlocked": false}}))
            .is_none()
    );
    let (r, c_ui) = PipeClient::connect(&ui_pipe, &self_exe, 2000, &CancelToken::none());
    assert_eq!(PipeConnectResult::Ok, r);
    let mut c_ui = c_ui.unwrap();
    let sub_ack = c_ui
        .request_timeout("subscribe captures", Duration::from_secs(3), &CancelToken::none())
        .unwrap()
        .unwrap();
    assert!(Envelope::parse(&sub_ack).unwrap().0);

    // Unlocked capture delivered.
    agent.raise_capture(&mk_meta("s_OPEN"));
    let got_open = read_event_until(&mut c_ui, Duration::from_secs(3), |v| {
        v.get("id").and_then(Value::as_str) == Some("s_OPEN")
    });
    assert!(got_open.is_some(), "expected s_OPEN capture event");

    // Lock -> emit s_SECRET (must NOT be sent even with ipc.requireUnlocked=false) -> unlock -> emit s_MARKER.
    agent.unlocked.store(false, Ordering::Release);
    assert!(proc.is_locked(PipeKind::Ui), "UI gate must be closed");
    assert!(
        !proc.is_locked(PipeKind::Public),
        "requireUnlocked=false keeps the public pipe open: the regression condition"
    );
    agent.raise_capture(&mk_meta("s_SECRET"));
    agent.unlocked.store(true, Ordering::Release);
    agent.raise_capture(&mk_meta("s_MARKER"));

    let mut seen_ids = Vec::new();
    let got_marker = read_event_until(&mut c_ui, Duration::from_secs(3), |v| {
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            seen_ids.push(id.to_string());
            return id == "s_MARKER";
        }
        false
    });
    assert!(got_marker.is_some(), "expected s_MARKER");
    assert!(
        !seen_ids.iter().any(|id| id == "s_SECRET"),
        "locked capture leaked under ipc.requireUnlocked=false: {seen_ids:?}"
    );

    // Purge notice delivers a `removed` event to subscribed UI stream.
    proc.on_store_purged(&PurgeNotice {
        reason: "purge".to_string(),
        ids: vec!["s_OPEN".to_string()],
        from: None,
        to: None,
        newest: None,
        scope: None,
    });
    let got_removed = read_event_until(&mut c_ui, Duration::from_secs(3), |v| {
        v.get("event").and_then(Value::as_str) == Some("removed")
            && v.get("reason").and_then(Value::as_str) == Some("purge")
    });
    assert!(got_removed.is_some(), "expected removed event on UI stream");

    // `removed_event_for` caps IDs at MAX_REMOVED_IDS and sets truncated=true.
    let big_ids: Vec<String> = (0..=CommandProcessor::MAX_REMOVED_IDS)
        .map(|i| format!("s_{i}"))
        .collect();
    let ev_trunc = CommandProcessor::removed_event_for(&PurgeNotice {
        reason: "purge".into(),
        ids: big_ids,
        from: None,
        to: None,
        newest: None,
        scope: None,
    });
    assert!(ev_trunc.truncated);
    assert!(ev_trunc.ids.is_empty());

    c_pub.close();
    c_ui.close();
    srv_pub.stop();
    srv_ui.stop();

    // 6) Sweep 4U S1: processor with store == None: unlocked UI status stream gets exactly ONE
    // `removed` event with `reason:"all"`; once locked, subsequent `retention` and `all` notices
    // produce NO removed event.
    let agent_s1 = Arc::new(FakeAgent::new());
    let proc_s1 = Arc::new(CommandProcessor::new(CommandDeps {
        settings: settings.clone(),
        store: Arc::new(|| None),
        keys: vault,
        agent: agent_s1.clone(),
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: Some(home.path().to_path_buf()),
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    }));
    proc_s1.set_status_heartbeat_for_test(Duration::from_secs(60));
    let s1_pipe = test_pipe_name("stream-s1");
    let p_s1 = proc_s1.clone();
    let srv_s1 = PipeServer::new(
        &s1_pipe,
        PipeKind::Ui,
        |_| true,
        move |conn, ct| p_s1.serve(conn, &ct),
        None,
    );
    srv_s1.start().unwrap();
    let (r, c_s1) = PipeClient::connect(&s1_pipe, &self_exe, 2000, &CancelToken::none());
    assert_eq!(PipeConnectResult::Ok, r);
    let mut c_s1 = c_s1.unwrap();
    let ack_s1 = c_s1
        .request_timeout("subscribe", Duration::from_secs(3), &CancelToken::none())
        .unwrap()
        .unwrap();
    assert!(Envelope::parse(&ack_s1).unwrap().0);

    // C# sends the purge-all window MinValue..now; an "all" notice always carries it.
    proc_s1.on_store_purged(&PurgeNotice {
        reason: "all".to_string(),
        ids: Vec::new(),
        from: Some(DateTime::<Utc>::MIN_UTC),
        to: Some(Utc::now()),
        newest: None,
        scope: None,
    });
    let got_all = read_event_until(&mut c_s1, Duration::from_secs(3), |v| {
        v.get("event").and_then(Value::as_str) == Some("removed")
            && v.get("reason").and_then(Value::as_str) == Some("all")
    });
    assert!(got_all.is_some(), "S1: unlocked UI status stream must receive reason='all' even with store=None");

    // Now lock UI: neither `retention` nor a second `all` may reach the UI status stream.
    agent_s1.unlocked.store(false, Ordering::Release);
    proc_s1.on_store_purged(&PurgeNotice {
        reason: "retention".to_string(),
        ids: vec!["s_OLD".to_string()],
        from: None,
        to: None,
        newest: None,
        scope: None,
    });
    proc_s1.on_store_purged(&PurgeNotice {
        reason: "all".to_string(),
        ids: Vec::new(),
        from: Some(DateTime::<Utc>::MIN_UTC),
        to: Some(Utc::now() + ChronoDuration::seconds(1)),
        newest: None,
        scope: None,
    });
    agent_s1.status.lock().unwrap().paused = true;
    agent_s1.raise_status();
    let mut extra_removed = Vec::new();
    let saw_s1_marker = read_event_until(&mut c_s1, Duration::from_secs(3), |v| {
        if v.get("event").and_then(Value::as_str) == Some("removed") {
            extra_removed.push(v.clone());
        }
        v.get("paused").and_then(Value::as_bool) == Some(true)
    });
    assert!(saw_s1_marker.is_some(), "expected S1 status marker");
    assert!(
        extra_removed.is_empty(),
        "locked UI status stream must not receive retention/all removed events: {extra_removed:?}"
    );
    c_s1.close();
    srv_s1.stop();
}

#[test]
fn stale_settings_snapshot_does_not_reopen_the_public_pipe() {
    let mut store_tmp = TempStore::new();
    let store = Arc::new(store_tmp.store.take().unwrap());
    let home = TempHome::new(Some("dpapi"));
    let settings = Arc::new(SettingsStore::load(home.path()));
    let s_for_vault = settings.clone();
    let vault = Arc::new(KeyVault::new(
        home.path(),
        move || s_for_vault.current(),
        None,
        false,
    ));
    let agent = Arc::new(FakeAgent::new());
    let st_clone = store.clone();
    let proc = Arc::new(CommandProcessor::new(CommandDeps {
        settings: settings.clone(),
        store: Arc::new(move || Some(st_clone.clone())),
        keys: vault,
        agent,
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: None,
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    }));

    // Register a listener BEFORE PipeManager that blocks the FIRST settings-changed callback
    // (turning ipc.enabled=true) until a SECOND update (turning ipc.enabled=false) has finished.
    let hold_entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release_first = Arc::new((Mutex::new(false), Condvar::new()));
    let first_call = Arc::new(AtomicBool::new(false));
    let he_cb = hold_entered.clone();
    let rf_cb = release_first.clone();
    let fc_cb = first_call.clone();
    settings.on_changed(move |s| {
        if s.ipc.enabled && !fc_cb.swap(true, Ordering::AcqRel) {
            {
                let (m, cv) = &*he_cb;
                *m.lock().unwrap() = true;
                cv.notify_all();
            }
            let (m, cv) = &*rf_cb;
            let g = m.lock().unwrap();
            let _ = cv
                .wait_timeout_while(g, Duration::from_secs(5), |done| !*done)
                .unwrap();
        }
    });

    let tag = test_pipe_name("stale");
    let tag_cb = tag.clone();
    let st_pm = store.clone();
    let pm = PipeManager::new(
        settings.clone(),
        proc,
        Path::new(r"C:\NotTheInstallDir"),
        Arc::new(move || Some(st_pm.clone())),
        Some(Arc::new(move |k, s| match k {
            PipeKind::Ui => format!("{tag_cb}.ui"),
            PipeKind::Mcp => format!("{tag_cb}.mcp"),
            PipeKind::Public => format!("{tag_cb}.{}", s.ipc.pipe_name),
        })),
    );
    pm.start();

    let s_bg = settings.clone();
    let t1 = thread::spawn(move || {
        assert!(s_bg.try_update(&json!({"ipc": {"enabled": true}})).is_none());
    });
    {
        let (m, cv) = &*hold_entered;
        let g = m.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |v| !*v)
            .unwrap();
        assert!(*g, "first listener never entered");
    }
    // Second update disables public pipe while first update's notification is still suspended.
    assert!(settings.try_update(&json!({"ipc": {"enabled": false}})).is_none());
    {
        let (m, cv) = &*release_first;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }
    t1.join().unwrap();

    // PipeManager must reflect the LATEST settings (`ipc.enabled == false`), not the stale `true`.
    assert!(
        pm.public().is_none(),
        "stale settings snapshot must not reopen the public pipe"
    );
    pm.stop();
}

#[test]
fn ui_pipe_instance_cap_serves_eight_concurrent_clients() {
    assert_eq!(8, PipeServer::UI_INSTANCES);
    let name = test_pipe_name("ui8");
    let srv = PipeServer::new(
        &name,
        PipeKind::Ui,
        |_| true,
        |conn, ct| {
            let mut reader = br_core::PipeLineReader::new(conn.stream.clone(), 4096);
            while !ct.is_cancelled() {
                match reader.read_line(4096, None, &ct) {
                    Ok(Some(_)) => {
                        if conn.stream.write_line(r#"{"ok":true}"#, None, &ct).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        },
        None,
    );
    srv.start().unwrap();
    let self_exe = ProcessInfo::current_image().unwrap();

    let mut clients = Vec::new();
    for i in 0..PipeServer::UI_INSTANCES {
        let (r, c) = PipeClient::connect(&name, &self_exe, 2000, &CancelToken::none());
        assert_eq!(PipeConnectResult::Ok, r, "client {i} failed to connect");
        let mut c = c.unwrap();
        let reply = c
            .request_timeout("ping", Duration::from_secs(2), &CancelToken::none())
            .unwrap();
        assert!(reply.is_some(), "client {i} got no reply");
        clients.push(c);
    }

    // 9th concurrent client cannot connect while all 8 instances are busy.
    let (r9, c9) = PipeClient::connect(&name, &self_exe, 150, &CancelToken::none());
    assert_eq!(PipeConnectResult::NotRunning, r9);
    assert!(c9.is_none());

    for c in clients {
        c.close();
    }
    srv.stop();
}

#[test]
fn step_clock_multi_step_sub_tolerance_forward_back_and_start_bound_edges() {
    let logs = Arc::new(AtomicI32::new(0));
    let logs_cb = logs.clone();
    let clock = StepClock::new(Some(Box::new(move |_| {
        logs_cb.fetch_add(1, Ordering::AcqRel);
    })));
    let boot = DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // Sub-tolerance backward step (<= 10 s) does not trip or log.
    let up = 3600 * 1000;
    let b_sub = clock.bound(boot + ChronoDuration::seconds(3595), up, Some(boot));
    assert_eq!(boot + ChronoDuration::seconds(3595), b_sub);
    assert_eq!(0, logs.load(Ordering::Acquire));

    // Forward jump followed by backward step back to real time trips once.
    let up2 = up + 60_000;
    let b_fwd = clock.bound(boot + ChronoDuration::hours(3), up2, Some(boot));
    assert_eq!(boot + ChronoDuration::hours(3), b_fwd);
    assert_eq!(0, logs.load(Ordering::Acquire));

    let up3 = up2 + 60_000;
    let wall_back = boot + ChronoDuration::minutes(62);
    let b_back = clock.bound(wall_back, up3, Some(boot));
    assert_eq!(
        boot + ChronoDuration::hours(3) + ChronoDuration::minutes(1),
        b_back,
        "backward step from forward jump should raise bound to exact boot+3h1m"
    );
    assert_eq!(1, logs.load(Ordering::Acquire));

    // Missing boot anchor returns wall time unchanged.
    assert_eq!(wall_back, clock.bound(wall_back, up3, None));

    // Fresh helper matches StepClock::new(None).
    let fresh = ProcessInfo::start_bound_fresh(wall_back, up3, None);
    assert_eq!(wall_back, fresh);

    // Sweep 5L C3: cumulative sub-tolerance steps (-6 s x 3): 0 logs at -6 s, 1 log at -12 s
    // holding the bound, still holding at -18 s.
    let sub_logs = Arc::new(AtomicI32::new(0));
    let sub_logs_cb = sub_logs.clone();
    let sub_clock = StepClock::new(Some(Box::new(move |_| {
        sub_logs_cb.fetch_add(1, Ordering::AcqRel);
    })));
    let base_up = 3600 * 1000;
    let t0 = boot + ChronoDuration::seconds(3600);
    assert_eq!(t0, sub_clock.bound(t0, base_up, Some(boot)));
    assert_eq!(0, sub_logs.load(Ordering::Acquire));
    // Step 1: -6 s (within 10 s tolerance) -> 0 logs.
    let b1 = sub_clock.bound(t0 - ChronoDuration::seconds(6), base_up, Some(boot));
    assert_eq!(t0 - ChronoDuration::seconds(6), b1);
    assert_eq!(0, sub_logs.load(Ordering::Acquire));
    // Step 2: -12 s cumulative (> 10 s tolerance) -> trips once and holds bound at t0.
    let b2 = sub_clock.bound(t0 - ChronoDuration::seconds(12), base_up, Some(boot));
    assert_eq!(t0, b2);
    assert_eq!(1, sub_logs.load(Ordering::Acquire));
    // Step 3: -18 s cumulative (only 6 s past gap=12 s) -> holds bound at t0, still 1 log.
    let b3 = sub_clock.bound(t0 - ChronoDuration::seconds(18), base_up, Some(boot));
    assert_eq!(t0, b3);
    assert_eq!(1, sub_logs.load(Ordering::Acquire));

    // Sweep 5L C3: two distinct big backward steps log twice (step count == 2).
    let two_logs = Arc::new(AtomicI32::new(0));
    let two_logs_cb = two_logs.clone();
    let two_clock = StepClock::new(Some(Box::new(move |_| {
        two_logs_cb.fetch_add(1, Ordering::AcqRel);
    })));
    let _ = two_clock.bound(t0, base_up, Some(boot));
    let _ = two_clock.bound(t0 - ChronoDuration::hours(1), base_up + 1000, Some(boot));
    assert_eq!(1, two_logs.load(Ordering::Acquire));
    let b_two = two_clock.bound(t0 - ChronoDuration::hours(2), base_up + 2000, Some(boot));
    assert!(b_two >= t0, "two backward steps must keep bound >= t0");
    assert_eq!(2, two_logs.load(Ordering::Acquire));
}

#[test]
fn pipe_client_and_server_stay_trusted_across_clock_steps_and_reject_null_created() {
    let name = test_pipe_name("clkstep");
    let srv = PipeServer::new(
        &name,
        PipeKind::Public,
        |_| true,
        |conn, ct| {
            let mut reader = br_core::PipeLineReader::new(conn.stream.clone(), 4096);
            if let Ok(Some(_)) = reader.read_line(4096, None, &ct) {
                let _ = conn.stream.write_line(r#"{"ok":true}"#, None, &ct);
            }
        },
        None,
    );
    srv.start().unwrap();
    let self_exe = ProcessInfo::current_image().unwrap();

    // Server-side `check()` rejects null created_utc (PID reuse check cannot be skipped).
    let null_created = PeerInfo {
        pid: ProcessInfo::current_pid(),
        session: ProcessInfo::current_session(),
        sid: Some(ProcessInfo::current_sid().to_string()),
        integrity_rid: 0x2000,
        app_container: false,
        image: Some(self_exe.clone()),
        created_utc: None,
    };
    assert_eq!(
        Some("pid reuse"),
        srv.check(&null_created, Some(Utc::now())),
        "null created_utc must be rejected"
    );

    // Sweep 4L T2: server `check()` admits a client started before a 2 h backward clock step when
    // given `start_bound_fresh`, and still refuses it as PID reuse when given the unstepped wall time.
    let now_ref = Utc::now();
    let stepped_wall_srv = now_ref - ChronoDuration::hours(2);
    let boot_ref = now_ref - ChronoDuration::hours(5);
    let uptime_ms = 5 * 3600 * 1000;
    let pre_step_peer = PeerInfo {
        pid: ProcessInfo::current_pid(),
        session: ProcessInfo::current_session(),
        sid: Some(ProcessInfo::current_sid().to_string()),
        integrity_rid: 0x2000,
        app_container: false,
        image: Some(self_exe.clone()),
        created_utc: Some(stepped_wall_srv + ChronoDuration::hours(2)),
    };
    let bound_srv = ProcessInfo::start_bound_fresh(stepped_wall_srv, uptime_ms, Some(boot_ref));
    assert!(
        srv.check(&pre_step_peer, Some(bound_srv)).is_none(),
        "server check must admit client started before backward step under start_bound_fresh"
    );
    let unstepped_boot = stepped_wall_srv - ChronoDuration::milliseconds(uptime_ms);
    let bound_unstepped = ProcessInfo::start_bound_fresh(stepped_wall_srv, uptime_ms, Some(unstepped_boot));
    assert_eq!(
        Some("pid reuse"),
        srv.check(&pre_step_peer, Some(bound_unstepped)),
        "server check must still refuse created_utc > unstepped bound as PID reuse"
    );

    // Sweep 5L C3: establish normal-clock baseline first, then step wall clock backward twice
    // (-60 min and -62 min): PipeClient::connect_with_clock stays trusted across both steps.
    // Note (I4): connect_with_clock widens process-wide ProcessInfo::global_step_clock().
    // Because it only widens a bound, it is safe with parallel tests running in this process.
    let (r0, c0) = PipeClient::connect(&name, &self_exe, 2000, &CancelToken::none());
    assert_eq!(PipeConnectResult::Ok, r0);
    c0.unwrap().close();

    for back_min in [60i64, 62i64] {
        let stepped_wall = Utc::now() - ChronoDuration::minutes(back_min);
        let (res, client) = PipeClient::connect_with_clock(
            &name,
            &self_exe,
            2000,
            move || stepped_wall,
            &CancelToken::none(),
        );
        assert_eq!(
            PipeConnectResult::Ok,
            res,
            "connect_with_clock failed at -{back_min} min"
        );
        let mut client = client.unwrap();
        let reply = client
            .request_timeout("ping", Duration::from_secs(2), &CancelToken::none())
            .unwrap();
        assert!(reply.is_some());
        client.close();
    }

    // Wrong expected image is still rejected as NotTrusted even across a clock step.
    let stepped_wall2 = Utc::now() - ChronoDuration::hours(2) - ChronoDuration::minutes(2);
    let (res_bad, client_bad) = PipeClient::connect_with_clock(
        &name,
        r"C:\not\the\server.exe",
        2000,
        move || stepped_wall2,
        &CancelToken::none(),
    );
    assert_eq!(PipeConnectResult::NotTrusted, res_bad);
    assert!(client_bad.is_none());
    srv.stop();
}

struct JunctionGuard(PathBuf);
impl Drop for JunctionGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

#[test]
fn pipe_trust_resolves_junctions_for_client_and_allow_clients() {
    let dir = TempHome::new(None);
    let real_dir = dir.path().join("real");
    let link_dir = dir.path().join("link");
    fs::create_dir_all(&real_dir).unwrap();
    let target_exe = real_dir.join("client.exe");
    fs::write(&target_exe, b"stub").unwrap();

    let status = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &link_dir.to_string_lossy(),
            &real_dir.to_string_lossy(),
        ])
        .status()
        .expect("spawn cmd mklink /J");
    assert!(status.success(), "mklink /J failed");
    let _guard = JunctionGuard(link_dir.clone());

    let via_junction = link_dir.join("client.exe");
    assert!(
        ProcessInfo::same_path(
            Some(&target_exe.to_string_lossy()),
            Some(&via_junction.to_string_lossy()),
        ),
        "same_path should resolve junction"
    );
    assert_eq!(
        ProcessInfo::normalize(&target_exe.to_string_lossy()),
        ProcessInfo::normalize(&via_junction.to_string_lossy()),
    );

    // Sweep 5L B8: real PipeClient::connect with expected image through a junction AND
    // PipeManager `allowClients` entry through a junction both admit the real process.
    let self_exe = ProcessInfo::current_image().unwrap();
    let self_path = Path::new(&self_exe);
    let self_parent = self_path.parent().expect("exe parent");
    let self_name = self_path.file_name().expect("exe filename");
    let exe_link_dir = dir.path().join("exe_link");
    let st2 = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &exe_link_dir.to_string_lossy(),
            &self_parent.to_string_lossy(),
        ])
        .status()
        .expect("spawn cmd mklink /J for exe parent");
    assert!(st2.success(), "mklink /J for exe parent failed");
    let _guard2 = JunctionGuard(exe_link_dir.clone());
    let junction_self = exe_link_dir.join(self_name).to_string_lossy().into_owned();

    let mut store_tmp = TempStore::new();
    let store = Arc::new(store_tmp.store.take().unwrap());
    let home = TempHome::new(Some("dpapi"));
    let settings = Arc::new(SettingsStore::load(home.path()));
    assert!(
        settings
            .try_update(&json!({
                "ipc": {
                    "enabled": true,
                    "allowClients": [junction_self.clone()]
                }
            }))
            .is_none()
    );
    let s_for_vault = settings.clone();
    let vault = Arc::new(KeyVault::new(
        home.path(),
        move || s_for_vault.current(),
        None,
        false,
    ));
    let st_clone = store.clone();
    let proc = Arc::new(CommandProcessor::new(CommandDeps {
        settings: settings.clone(),
        store: Arc::new(move || Some(st_clone.clone())),
        keys: vault,
        agent: Arc::new(FakeAgent::new()),
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: None,
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    }));
    let tag = test_pipe_name("junc");
    let tag_cb = tag.clone();
    let st_pm = store.clone();
    let pm = PipeManager::new(
        settings.clone(),
        proc,
        Path::new(r"C:\NotTheInstallDir"),
        Arc::new(move || Some(st_pm.clone())),
        Some(Arc::new(move |k, s| match k {
            PipeKind::Ui => format!("{tag_cb}.ui"),
            PipeKind::Mcp => format!("{tag_cb}.mcp"),
            PipeKind::Public => format!("{tag_cb}.{}", s.ipc.pipe_name),
        })),
    );
    pm.start();
    let pub_name = format!("{tag}.{}", settings.current().ipc.pipe_name);
    let (res, client) = PipeClient::connect(&pub_name, &junction_self, 2000, &CancelToken::none());
    assert_eq!(
        PipeConnectResult::Ok,
        res,
        "PipeClient::connect must accept expected image via junction"
    );
    let mut client = client.unwrap();
    let pong = client
        .request_timeout("ping", Duration::from_secs(2), &CancelToken::none())
        .unwrap()
        .expect("allowClients via junction must admit client and serve ping");
    let (ok, d, _) = Envelope::parse(&pong).unwrap();
    assert!(ok);
    assert_eq!(true, d.unwrap()["pong"]);
    client.close();
    pm.stop();
}

#[test]
fn reconnect_and_reject_loops_write_one_audit_row_each() {
    let mut store_tmp = TempStore::new();
    let store = Arc::new(store_tmp.store.take().unwrap());
    let home = TempHome::new(Some("dpapi"));
    let settings = Arc::new(SettingsStore::load(home.path()));
    assert!(
        settings
            .try_update(&json!({
                "ipc": {
                    "enabled": true,
                    "allowClients": [r"C:\never-matches\only-this.exe"]
                }
            }))
            .is_none()
    );
    let s_for_vault = settings.clone();
    let vault = Arc::new(KeyVault::new(
        home.path(),
        move || s_for_vault.current(),
        None,
        false,
    ));
    let agent = Arc::new(FakeAgent::new());
    let st_clone = store.clone();
    let proc = Arc::new(CommandProcessor::new(CommandDeps {
        settings: settings.clone(),
        store: Arc::new(move || Some(st_clone.clone())),
        keys: vault,
        agent,
        embedder: Arc::new(FakeEmbedder::new()),
        thumbnailer: Arc::new(FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: None,
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    }));

    let tag = test_pipe_name("throttle");
    let tag_cb = tag.clone();
    let st_pm = store.clone();
    let pm = PipeManager::new(
        settings.clone(),
        proc,
        Path::new(r"C:\NotTheInstallDir"),
        Arc::new(move || Some(st_pm.clone())),
        Some(Arc::new(move |k, s| match k {
            PipeKind::Ui => format!("{tag_cb}.ui"),
            PipeKind::Mcp => format!("{tag_cb}.mcp"),
            PipeKind::Public => format!("{tag_cb}.{}", s.ipc.pipe_name),
        })),
    );
    pm.start();

    let pub_name = format!("{tag}.{}", settings.current().ipc.pipe_name);
    let self_exe = ProcessInfo::current_image().unwrap();

    // Repeated rejected connects should be throttled to ONE reject audit row within the window.
    for _ in 0..5 {
        if let (PipeConnectResult::Ok, Some(mut c)) =
            PipeClient::connect(&pub_name, &self_exe, 500, &CancelToken::none())
        {
            let _ = c.read_line_timeout(Duration::from_millis(300), &CancelToken::none());
            c.close();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rejects = 0;
    while Instant::now() < deadline {
        rejects = store
            .read_audit(50)
            .unwrap()
            .into_iter()
            .filter(|a| a.verb == "rejected")
            .count();
        if rejects >= 1 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(1, rejects, "expected exactly 1 throttled reject audit row");

    // Allow current exe and connect+ping repeatedly -> ONE connect audit row within the window.
    assert!(
        settings
            .try_update(&json!({"ipc": {"allowClients": [self_exe.clone()]}}))
            .is_none()
    );
    for _ in 0..5 {
        let (r, c) = PipeClient::connect(&pub_name, &self_exe, 1000, &CancelToken::none());
        assert_eq!(PipeConnectResult::Ok, r);
        let mut c = c.unwrap();
        let ping_res = c.request_timeout("ping", Duration::from_secs(2), &CancelToken::none()).unwrap().unwrap();
        assert!(Envelope::parse(&ping_res).unwrap().0);
        c.close();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut connects = 0;
    while Instant::now() < deadline {
        connects = store
            .read_audit(50)
            .unwrap()
            .into_iter()
            .filter(|a| a.verb == "connect")
            .count();
        if connects >= 1 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(1, connects, "expected exactly 1 throttled connect audit row");

    pm.stop();
}

#[test]
fn core_host_purge_all_listener_by_user_to_and_reopened_store_and_old_db_exists() {
    let h = TempHome::new(Some("dpapi"));
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: h.path().to_path_buf(),
            install_dir: h.path().to_path_buf(),
        },
        false,
    );
    host.open_store(&CancelToken::none()).unwrap();
    let id = host
        .store()
        .unwrap()
        .add_audio_capture(&audio(Utc::now(), "before purge all", "app.exe"))
        .unwrap();

    let notices: Arc<Mutex<Vec<PurgeNotice>>> = Arc::new(Mutex::new(Vec::new()));
    let n_cb = notices.clone();
    let _guard = host.on_purged(move |n| n_cb.lock().unwrap().push(n.clone()));

    let before = Utc::now();
    let purged = host.purge_all(&CancelToken::none());
    assert_eq!(1, purged);
    assert!(host.store().is_some());
    assert!(host.store().unwrap().get(&id).unwrap().is_none());

    let seen = notices.lock().unwrap().clone();
    let all_notice = seen
        .iter()
        .find(|n| n.reason == "all")
        .expect("purge_all must notify on_purged with reason='all'");
    assert!(all_notice.by_user());
    assert!(all_notice.to.is_some_and(|t| t >= before));

    // Reopened store still forwards subsequent purges to the same CoreHost listener!
    let id2 = host
        .store()
        .unwrap()
        .add_audio_capture(&audio(Utc::now(), "after reopen", "app.exe"))
        .unwrap();
    host.store().unwrap().purge(&PurgeScope::for_id(&id2)).unwrap();
    let seen2 = notices.lock().unwrap().clone();
    assert!(
        seen2.iter().any(|n| n.reason == "purge" && n.ids.contains(&id2)),
        "reopened store must forward purge notices to CoreHost listeners: {seen2:?}"
    );
    assert!(DataPaths::store_db(h.path()).exists());
    host.shutdown();
}
