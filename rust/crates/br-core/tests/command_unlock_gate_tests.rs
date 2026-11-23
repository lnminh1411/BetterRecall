// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sweep 6 G3 ported tests for read-gate locking, guarded settings, Hello/password/passphrase
//! unlock flows, rate-limit error text format, forgot-passphrase purge, and `unlockNeeds`.

mod common;

use std::fs;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{DataPaths, Envelope, ReplayClip, ThumbData};
use br_core::{AgentError, CancelToken, CommandProcessor, KeyVault, PasswordCheck, Thumbnailer};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};

use common::ports::{PortFixture, PortFixtureOpts};
use common::{screen, vec_info, vec_seeded};

#[test]
fn read_gate_closes_all_data_verbs_and_store_not_open_vs_locked() {
    let f = PortFixture::new();
    let t = Utc::now() - ChronoDuration::minutes(5);
    let id = f
        .store
        .add_screen_capture(&screen(t, "chrome.exe", "private", true))
        .unwrap();
    f.store
        .set_embedding(&id, &vec_seeded(1), &vec_info())
        .unwrap();
    f.agent.replays.lock().unwrap().push(ReplayClip {
        id: "r1".into(),
        ts: t,
        duration_ms: 10_000,
        bytes: 512,
        encrypted: true,
        path: r"C:\r1.mp4.enc".into(),
    });

    // Lock the read gate.
    f.agent.unlocked.store(false, Ordering::Release);
    let mut ui_s = PortFixture::ui();
    let mut pub_s = PortFixture::pub_sess();
    f.set_settings(json!({"ipc": {"allowThumbnails": true, "allowPurge": true}}));

    // D1: all data verbs return ERR_LOCKED while locked.
    for cmd in [
        "search private",
        &format!("get {id}"),
        &format!("thumb {id}"),
        &format!("keyframe {id}"),
        &format!(
            "timeline {} {}",
            (t - ChronoDuration::minutes(5)).to_rfc3339(),
            (t + ChronoDuration::minutes(5)).to_rfc3339()
        ),
        "facets",
        "history",
        "history delete x",
        "history clear",
        "audit",
        "replay list",
        "replay delete r1",
        "ai clearkey",
        "purge last 60",
    ] {
        let (ok, _, err) = f.call(&mut ui_s, cmd);
        assert!(!ok, "locked UI command should fail: {cmd}");
        assert_eq!(
            CommandProcessor::ERR_LOCKED,
            err.as_deref().unwrap(),
            "cmd={cmd}"
        );
    }
    assert_eq!(
        1,
        f.agent.replays.lock().unwrap().len(),
        "clip must still exist after locked replay delete refusal"
    );
    f.agent.unlocked.store(true, Ordering::Release);
    let (ok_del, _, _) = f.call(&mut ui_s, "replay delete r1");
    assert!(ok_del, "replay delete must succeed after unlock");
    assert!(f.agent.replays.lock().unwrap().is_empty());

    // B11: purge confirm issued before lock is rejected after lock, row stays, purge all != ERR_LOCKED.
    f.agent.unlocked.store(true, Ordering::Release);
    let (ok, prev, _) = f.call(&mut ui_s, &format!("purge id {id}"));
    assert!(ok);
    let tok = prev.unwrap()["confirm"].as_str().unwrap().to_string();
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, &format!("purge confirm {tok}"));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    assert!(
        f.store.get(&id).unwrap().is_some(),
        "row must remain after locked purge confirm refusal"
    );
    let (ok, _, err) = f.call(&mut ui_s, "purge all");
    assert!(!ok);
    assert_ne!(CommandProcessor::ERR_LOCKED, err.as_deref().unwrap());
    assert_eq!(CommandProcessor::ERR_UNLOCK_REQUIRED, err.unwrap());

    // D3: locked gate blocks public purge even when allowControl + allowPurge are true, leaking no rows.
    f.agent.unlocked.store(true, Ordering::Release);
    f.set_settings(json!({"ipc": {"allowControl": true, "allowPurge": true}}));
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, data, err) = f.call(&mut pub_s, "purge app chrome.exe");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    assert!(data.is_none(), "locked public purge must not leak preview rows");

    // Plain `subscribe` and `status` remain open while locked.
    let (ok, sub_d, _) = f.call(&mut ui_s, "subscribe");
    assert!(ok);
    assert_eq!(true, sub_d.unwrap()["subscribed"]);
    let (ok, sub_pub, _) = f.call(&mut pub_s, "subscribe");
    assert!(ok);
    assert_eq!(true, sub_pub.unwrap()["subscribed"]);

    // D12: locked status keeps store.rows for UI, zeroes/nulls sensitive fields for Public.
    {
        let mut st = f.agent.status.lock().unwrap();
        st.locked = true;
        st.store.rows = 7;
        st.store.newest = Some(t);
        st.store.oldest = Some(t);
    }
    let (ok, st_ui, _) = f.call(&mut ui_s, "status");
    assert!(ok);
    let st_ui = st_ui.unwrap();
    assert_eq!(true, st_ui["locked"]);
    assert_eq!(7, st_ui["store"]["rows"]);

    let (ok, st_pub, _) = f.call(&mut pub_s, "status");
    assert!(ok);
    let st_pub = st_pub.unwrap();
    assert_eq!(true, st_pub["locked"]);
    assert!(st_pub["store"].get("newest").is_none() || st_pub["store"]["newest"].is_null());

    // D4: even with requireUnlock=false, locking the agent still closes data verbs on UI.
    f.agent.unlocked.store(true, Ordering::Release);
    f.set_settings(json!({"security": {"requireUnlock": false}}));
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, "search private");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());

    // D1: with the read gate open and store_open=false (loaded key, not waiting for passphrase),
    // search, guarded settings set, and UI timeline all return ERR_NOT_OPEN.
    f.agent.unlocked.store(true, Ordering::Release);
    f.store_open.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, "search private");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_OPEN, err.unwrap());
    let (ok, _, err) = f.call(&mut ui_s, r#"settings set {"ipc":{"enabled":true}}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_OPEN, err.unwrap());
    let (ok, _, err) = f.call(
        &mut ui_s,
        &format!(
            "timeline {} {}",
            (t - ChronoDuration::minutes(5)).to_rfc3339(),
            (t + ChronoDuration::minutes(5)).to_rfc3339()
        ),
    );
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NOT_OPEN, err.unwrap());
}

#[test]
fn guarded_settings_table_all_groups_model_dir_and_unguarded_exceptions() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    f.agent.unlocked.store(false, Ordering::Release);

    // All 12 guarded setting groups plus inference.modelDir are rejected while locked.
    let guarded_patches = [
        json!({"security": {"requireUnlock": false}}),
        json!({"ai": {"enabled": true}}),
        json!({"ipc": {"enabled": true}}),
        json!({"mcp": {"enabled": true}}),
        json!({"privacy": {"pauseOnLock": false}}),
        json!({"ocr": {"enabled": true}}),
        json!({"retention": {"days": 7}}),
        json!({"storage": {"maxGb": 20}}),
        json!({"replay": {"enabled": true}}),
        json!({"index": {"keepKeyframes": false}}),
        json!({"audio": {"system": true}}),
        json!({"keyframes": {"enabled": true}}),
        json!({"inference": {"modelDir": r"C:\evil"}}),
        json!({"security": {"keyProtection": "dpapi"}}),
    ];
    let settings_before = serde_json::to_value(f.current_settings()).unwrap();
    for patch in &guarded_patches {
        assert!(
            CommandProcessor::touches_guarded_settings(patch),
            "should be guarded: {patch}"
        );
        let (ok, _, err) = f.call(&mut ui_s, &format!("settings set {patch}"));
        assert!(!ok, "locked guarded patch should fail: {patch}");
        assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
        assert_eq!(
            settings_before,
            serde_json::to_value(f.current_settings()).unwrap(),
            "refused patch must leave settings unchanged: {patch}"
        );
    }

    // Duplicate-key JSON: serde_json::from_str::<Value> keeps the LAST duplicate key ("inference":{}),
    // so neither touches_guarded_settings nor try_update sees modelDir — verify modelDir is NOT changed.
    let model_dir_before = f.current_settings().inference.model_dir;
    let (ok_dup, _, _) = f.call(
        &mut ui_s,
        r#"settings set {"inference":{"modelDir":"C:\\evil"},"inference":{}}"#,
    );
    assert!(ok_dup);
    assert_eq!(model_dir_before, f.current_settings().inference.model_dir);

    // Unguarded settings (e.g. ui.theme) succeed while locked.
    let unguarded = json!({"ui": {"theme": "dark"}});
    assert!(!CommandProcessor::touches_guarded_settings(&unguarded));
    let (ok, _, err) = f.call(&mut ui_s, &format!("settings set {unguarded}"));
    assert!(ok, "unguarded setting should pass while locked: {err:?}");
    assert_eq!("dark", f.current_settings().ui.theme);

    // `replay save` is allowed while locked (saves rolling buffer without reading store).
    let (ok, saved, err) = f.call(&mut ui_s, "replay save");
    assert!(ok, "replay save should work while locked: {err:?}");
    assert_eq!("r1", saved.unwrap()["id"]);
}

struct DeferredLockThumbs(Arc<Mutex<Option<Arc<common::a3::FakeAgent>>>>);

impl Thumbnailer for DeferredLockThumbs {
    fn get_thumb(&self, _id: &str, _width: i32, _ct: &CancelToken) -> Option<ThumbData> {
        None
    }

    fn clear_cache(&self) {}

    fn transcode(&self, keyframe: ThumbData, _target: &str) -> ThumbData {
        if let Some(ref a) = *self.0.lock().unwrap() {
            a.unlocked.store(false, Ordering::Release);
        }
        keyframe
    }
}

#[test]
fn lock_during_embed_wait_and_mid_export_and_replay_export_aborts() {
    // Part 1 (S4 B9): locking the read gate while search waits on a slow embedder makes search
    // return ERR_LOCKED instead of leaking rows.
    let f = PortFixture::new();
    let t = Utc::now() - ChronoDuration::minutes(5);
    let id = f
        .store
        .add_screen_capture(&screen(t, "chrome.exe", "secret doc", true))
        .unwrap();
    f.store
        .set_embedding(&id, &vec_seeded(1), &vec_info())
        .unwrap();

    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let entered_bg = entered.clone();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let release_bg = release.clone();
    *f.embedder.on_embed.lock().unwrap() = Some(Box::new(move |_, _ct| {
        {
            let (m, cv) = &*entered_bg;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        let (m, cv) = &*release_bg;
        let g = m.lock().unwrap();
        let _ = cv
            .wait_timeout_while(g, Duration::from_secs(5), |done| !*done)
            .unwrap();
        Ok(vec_seeded(1))
    }));

    let p_bg = f.p.clone();
    let search_h = thread::spawn(move || {
        let mut s = PortFixture::ui();
        let r = p_bg.handle(&mut s, "search secret doc", &CancelToken::none());
        Envelope::parse(&r.line).unwrap()
    });
    {
        let (m, cv) = &*entered;
        let g = m.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |v| !*v)
            .unwrap();
        assert!(*g);
    }
    let mut ui_s = PortFixture::ui();
    let (ok, _, _) = f.call(&mut ui_s, "lock");
    assert!(ok);
    {
        let (m, cv) = &*release;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }
    let (ok, _, err) = search_h.join().unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());

    // Part 2 (S5 B1): locking mid-export aborts the export with ERR_LOCKED and removes the zip.
    let agent_holder: Arc<Mutex<Option<Arc<common::a3::FakeAgent>>>> = Arc::new(Mutex::new(None));
    let f2 = PortFixture::with_opts(PortFixtureOpts {
        thumbnailer: Some(Arc::new(DeferredLockThumbs(agent_holder.clone()))),
        ..Default::default()
    });
    *agent_holder.lock().unwrap() = Some(f2.agent.clone());
    let exp_id = f2
        .store
        .add_screen_capture(&screen(t, "chrome.exe", "export secret", true))
        .unwrap();
    f2.store
        .set_embedding(&exp_id, &vec_seeded(2), &vec_info())
        .unwrap();
    let zip_path = f2.home.path().join("locked_mid_export.zip");
    let mut ui_s2 = PortFixture::ui();
    let (ok, _, err) = f2.call(
        &mut ui_s2,
        &format!(
            "export {}",
            json!({
                "from": (t - ChronoDuration::minutes(5)).to_rfc3339(),
                "to": (t + ChronoDuration::minutes(5)).to_rfc3339(),
                "path": zip_path.to_string_lossy(),
                "jpeg": true,
            })
        ),
    );
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    assert!(!zip_path.exists());
    assert!(
        !std::path::PathBuf::from(format!("{}.partial", zip_path.display())).exists(),
        "aborted export must not leave a .partial file"
    );

    // Part 3 (S5 B1): replay export refuses while locked, returns ERR_LOCKED when AgentError::Locked,
    // and cancels an in-flight replay export with ERR_LOCKED when another UI session issues `lock`.
    let mp4 = f2.home.path().join("clip.mp4");
    let (ok, _, err) = f2.call(&mut ui_s2, &format!("replay export r1 {}", mp4.display()));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());

    f2.agent.unlocked.store(true, Ordering::Release);
    *f2.agent.on_export_replay.lock().unwrap() = Some(Box::new(|_, _, _| Err(AgentError::Locked)));
    let (ok, _, err) = f2.call(&mut ui_s2, &format!("replay export r1 {}", mp4.display()));
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());

    let rx_entered = Arc::new((Mutex::new(false), Condvar::new()));
    let rx_entered_bg = rx_entered.clone();
    *f2.agent.on_export_replay.lock().unwrap() = Some(Box::new(move |_, _, ct| {
        {
            let (m, cv) = &*rx_entered_bg;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        assert!(
            ct.wait_timeout(Duration::from_secs(5)),
            "lock verb must cancel the in-flight replay export token"
        );
        Err(AgentError::Cancelled)
    }));
    let p_bg2 = f2.p.clone();
    let mp4_str = mp4.to_string_lossy().into_owned();
    let rx_h = thread::spawn(move || {
        let mut s = PortFixture::ui();
        let r = p_bg2.handle(&mut s, &format!("replay export r1 {mp4_str}"), &CancelToken::none());
        Envelope::parse(&r.line).unwrap()
    });
    {
        let (m, cv) = &*rx_entered;
        let g = m.lock().unwrap();
        let (g, _) = cv
            .wait_timeout_while(g, Duration::from_secs(5), |v| !*v)
            .unwrap();
        assert!(*g, "in-flight replay export never entered");
    }
    let mut ui_locker = PortFixture::ui();
    let (ok, _, _) = f2.call(&mut ui_locker, "lock");
    assert!(ok);
    let (ok, _, err) = rx_h.join().unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
}

#[test]
fn unlock_lock_consent_panic_bench_and_hello_cancel_and_no_opengate() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();

    // Lock then unlock with Hello cancelled.
    f.agent.unlocked.store(false, Ordering::Release);
    f.agent.unlock_result.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, "unlock");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_UNLOCK_CANCELLED, err.unwrap());
    assert!(!f.agent.unlocked.load(Ordering::Acquire));

    // Hello succeeds: unlocked=true, calls "unlock", never calls "opengate".
    f.agent.calls.lock().unwrap().clear();
    f.agent.unlock_result.store(true, Ordering::Release);
    let (ok, data, _) = f.call(&mut ui_s, "unlock");
    assert!(ok);
    assert_eq!(true, data.unwrap()["unlocked"]);
    let calls = f.agent.calls.lock().unwrap().clone();
    assert!(calls.contains(&"unlock".to_string()));
    assert!(!calls.contains(&"opengate".to_string()));

    // Consent returns null and calls agent.consent().
    let (ok, data, _) = f.call(&mut ui_s, "consent");
    assert!(ok);
    assert!(data.is_none() || data == Some(Value::Null));

    // Panic calls agent.panic().
    let (ok, _, _) = f.call(&mut ui_s, "panic");
    assert!(ok);

    // Bench returns chosen device.
    let (ok, data, _) = f.call(&mut ui_s, "bench");
    assert!(ok);
    assert_eq!("CPU", data.unwrap()["chosen"]);

    // Lock calls agent.lock().
    let (ok, _, _) = f.call(&mut ui_s, "lock");
    assert!(ok);
    assert!(!f.agent.unlocked.load(Ordering::Acquire));
    let calls = f.agent.calls.lock().unwrap().clone();
    assert!(calls.contains(&"consent".to_string()));
    assert!(calls.contains(&"panic".to_string()));
    assert!(calls.contains(&"lock".to_string()));
}

#[test]
fn password_method_guard_unlock_change_clear_first_gate_and_rate_limit() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();

    // Switching to unlockMethod=password before setting a password fails.
    let (ok, _, err) = f.call(&mut ui_s, r#"settings set {"security":{"unlockMethod":"password"}}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NO_PASSWORD, err.unwrap());

    // First password requires open read gate when Hello exists.
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, r#"password set {"new":"secret-pw-1"}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_UNLOCK_REQUIRED, err.unwrap());

    // Open gate -> short password rejected -> valid password succeeds.
    f.agent.unlocked.store(true, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, r#"password set {"new":"12345"}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PASSWORD_RULE, err.unwrap());

    let (ok, _, _) = f.call(&mut ui_s, r#"password set {"new":"secret-pw-1"}"#);
    assert!(ok);
    let (ok, st, _) = f.call(&mut ui_s, "password");
    assert!(ok);
    assert_eq!(true, st.unwrap()["set"]);

    // Now switching unlockMethod=password succeeds.
    let (ok, _, _) = f.call(&mut ui_s, r#"settings set {"security":{"unlockMethod":"password"}}"#);
    assert!(ok);

    // Lock, then unlock with password opens gate WITHOUT calling Hello (`opengate`, not `unlock`).
    f.agent.unlocked.store(false, Ordering::Release);
    f.agent.calls.lock().unwrap().clear();
    let (ok, data, _) = f.call(&mut ui_s, "unlock secret-pw-1");
    assert!(ok);
    assert_eq!(true, data.unwrap()["unlocked"]);
    let calls = f.agent.calls.lock().unwrap().clone();
    assert!(calls.contains(&"opengate".to_string()));
    assert!(!calls.contains(&"unlock".to_string()));

    // Audit records "unlock password" with empty query_sha256 and never leaks the secret password.
    let audit = f.store.read_audit(20).unwrap();
    let unlock_rows: Vec<_> = audit.iter().filter(|a| a.verb == "unlock password").collect();
    assert!(unlock_rows.iter().any(|a| a.result_count == 1));
    assert!(
        unlock_rows.iter().all(|a| a.query_sha256.is_empty()),
        "unlock audit rows must carry empty query_sha256: {unlock_rows:?}"
    );
    assert!(!serde_json::to_string(&audit).unwrap().contains("secret-pw-1"));

    // Change password requires current password (missing or wrong -> ERR_WRONG_PASSWORD),
    // enforces password rule on new, and rejects bad subcommands with USAGE_PASSWORD.
    let (ok, _, err) = f.call(&mut ui_s, r#"password set {"new":"silent-overwrite"}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_WRONG_PASSWORD, err.unwrap());

    let (ok, _, err) = f.call(
        &mut ui_s,
        r#"password set {"current":"wrong-pw","new":"secret-pw-2"}"#,
    );
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_WRONG_PASSWORD, err.unwrap());

    let (ok, _, err) = f.call(
        &mut ui_s,
        r#"password set {"current":"secret-pw-1","new":"12345"}"#,
    );
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PASSWORD_RULE, err.unwrap());

    let (ok, _, err) = f.call(&mut ui_s, "password reset");
    assert!(!ok);
    assert_eq!(CommandProcessor::USAGE_PASSWORD, err.unwrap());

    let (ok, _, _) = f.call(
        &mut ui_s,
        r#"password set {"current":"secret-pw-1","new":"secret-pw-2"}"#,
    );
    assert!(ok);

    // Clear password refuses while unlockMethod=password is in use.
    let (ok, _, err) = f.call(&mut ui_s, r#"password clear {"current":"secret-pw-2"}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PASSWORD_IN_USE, err.unwrap());

    // Rate-limit on wrong unlock attempts keeps exact format "Too many attempts. Try again in N s."
    // (G3 extra rule: UI parses N from this exact sentence).
    f.agent.unlocked.store(false, Ordering::Release);
    let mut rate_err = String::new();
    for _ in 0..8 {
        let (ok, _, err) = f.call(&mut ui_s, "unlock definitely-wrong");
        assert!(!ok);
        let msg = err.unwrap();
        if msg.starts_with(CommandProcessor::ERR_TOO_MANY_PREFIX) {
            rate_err = msg;
            break;
        }
        assert_eq!(CommandProcessor::ERR_WRONG_PASSWORD, msg);
    }
    assert!(
        rate_err.starts_with("Too many attempts. Try again in ") && rate_err.ends_with(" s."),
        "rate limit error format drifted: {rate_err:?}"
    );
    let n_str = rate_err
        .strip_prefix("Too many attempts. Try again in ")
        .unwrap()
        .strip_suffix(" s.")
        .unwrap();
    let n: i32 = n_str.parse().expect("N must be an integer");
    assert!(n > 0, "N must be positive: {n}");
    assert_eq!(CommandProcessor::err_too_many(n), rate_err);

    // During lockout, even the RIGHT password is refused with too-many, gate stays shut,
    // `password` reports retryAfterS > 0, and audit has a result_count==0 wrong-password row with empty hash.
    let (ok, _, err) = f.call(&mut ui_s, "unlock secret-pw-2");
    assert!(!ok);
    assert!(err.unwrap().starts_with(CommandProcessor::ERR_TOO_MANY_PREFIX));
    assert!(!f.agent.unlocked.load(Ordering::Acquire));
    let (ok, pw_st, _) = f.call(&mut ui_s, "password");
    assert!(ok);
    assert!(pw_st.unwrap()["retryAfterS"].as_i64().unwrap() > 0);
    let audit2 = f.store.read_audit(50).unwrap();
    let unlock_rows2: Vec<_> = audit2.iter().filter(|a| a.verb == "unlock password").collect();
    assert!(
        unlock_rows2.iter().any(|a| a.result_count == 0),
        "expected result_count==0 audit row for wrong password: {unlock_rows2:?}"
    );
    assert!(unlock_rows2.iter().all(|a| a.query_sha256.is_empty()));

    // Advance past wait, unlock, switch back to hello, then clear needs current password.
    f.advance(ChronoDuration::seconds(i64::from(n) + 2));
    let (ok, _, _) = f.call(&mut ui_s, "unlock secret-pw-2");
    assert!(ok);
    let (ok, _, _) = f.call(&mut ui_s, r#"settings set {"security":{"unlockMethod":"hello"}}"#);
    assert!(ok);
    let (ok, _, err) = f.call(&mut ui_s, r#"password clear {"current":"wrong"}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_WRONG_PASSWORD, err.unwrap());
    let (ok, _, _) = f.call(&mut ui_s, r#"password clear {"current":"secret-pw-2"}"#);
    assert!(ok);
    let (ok, st, _) = f.call(&mut ui_s, "password");
    assert!(ok);
    assert_eq!(false, st.unwrap()["set"]);
}

#[test]
fn passphrase_rewrap_hello_gate_spaces_short_rule_b7_b16_and_b4_inline_verify() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();

    // Switch keyProtection to passphrase while unlocked -> vault will rewrap on next unlock with passphrase.
    let (ok, _, _) = f.call(&mut ui_s, r#"settings set {"security":{"keyProtection":"passphrase"}}"#);
    assert!(ok);

    // Short passphrase rejected with ERR_PASSPHRASE_RULE.
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, "unlock short");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PASSPHRASE_RULE, err.unwrap());

    // Sweep 2 B6: Rewrap requires Hello gate first; if Hello cancels, key stays dpapi and key file bytes are unchanged.
    let key_file_path = DataPaths::key_file(f.home.path());
    let key_bytes_before = fs::read(&key_file_path).unwrap();
    f.agent.unlock_result.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, "unlock my long passphrase with spaces");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_UNLOCK_CANCELLED, err.unwrap());
    assert_eq!(KeyVault::MODE_DPAPI, f.vault.protection());
    assert_eq!(key_bytes_before, fs::read(&key_file_path).unwrap());

    // B16: Passphrase with spaces succeeds when Hello succeeds and rewraps key to passphrase mode.
    f.agent.unlock_result.store(true, Ordering::Release);
    let (ok, data, _) = f.call(&mut ui_s, "unlock my long passphrase with spaces");
    assert!(ok);
    assert_eq!(true, data.unwrap()["unlocked"]);
    assert_eq!(KeyVault::MODE_PASSPHRASE, f.vault.protection());

    // After rewrap, a fresh vault over the same home cannot load silently, refuses an intruder
    // passphrase, and opens with the real passphrase.
    {
        let s_fresh = f.settings.clone();
        let fresh_vault = KeyVault::new(f.home.path(), move || s_fresh.current(), None, false);
        assert!(!fresh_vault.try_load_silent().unwrap());
        assert!(fresh_vault.needs_passphrase());
        assert!(!fresh_vault.unlock_with_passphrase("intruder-passphrase-999"));
        assert!(fresh_vault.unlock_with_passphrase("my long passphrase with spaces"));
        assert!(fresh_vault.key_loaded());
    }

    // Switching keyProtection back to dpapi requires passphrase verification in this session;
    // lock + unlock with password (not passphrase) blocks keyProtection change until B4 inline verify.
    let (ok, _, _) = f.call(&mut ui_s, r#"password set {"new":"pw-123456"}"#);
    assert!(ok);
    let (ok, _, _) = f.call(&mut ui_s, r#"settings set {"security":{"unlockMethod":"password"}}"#);
    assert!(ok);
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, _) = f.call(&mut ui_s, "unlock pw-123456");
    assert!(ok);

    // Changing keyProtection without passphrase verification fails with ERR_PASSPHRASE_NEEDED.
    let (ok, _, err) = f.call(&mut ui_s, r#"settings set {"security":{"keyProtection":"dpapi"}}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PASSPHRASE_NEEDED, err.unwrap());

    // B4: `passphrase verify` rejects wrong passphrase, accepts right passphrase, then allows keyProtection change.
    let (ok, _, err) = f.call(&mut ui_s, r#"passphrase verify {"passphrase":"wrong-passphrase"}"#);
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_WRONG_PASSPHRASE, err.unwrap());

    let (ok, _, _) = f.call(
        &mut ui_s,
        r#"passphrase verify {"passphrase":"my long passphrase with spaces"}"#,
    );
    assert!(ok);

    let (ok, _, err) = f.call(&mut ui_s, r#"settings set {"security":{"keyProtection":"dpapi"}}"#);
    assert!(ok, "after passphrase verify, changing keyProtection should pass: {err:?}");

    // Unlock_with_passphrase_unwraps_the_key: fresh passphrase vault with leading & trailing spaces;
    // bare `unlock` keeps key_loaded()==false, wrong/trimmed passphrases fail with ERR_WRONG_PASSPHRASE,
    // exact leading+trailing spaces unwrap the key.
    let f_sp = PortFixture::with_opts(PortFixtureOpts {
        key_protection: "passphrase",
        load_silent: false,
        store_open: false,
        ..Default::default()
    });
    assert!(f_sp.vault.unlock_with_passphrase(" my pass phrase "));
    let sp_home = f_sp.home.path().to_path_buf();
    let sp_settings = Arc::new(br_core::SettingsStore::load(&sp_home));
    let sp_s_clone = sp_settings.clone();
    let sp_vault = Arc::new(KeyVault::new(&sp_home, move || sp_s_clone.current(), None, false));
    sp_vault.set_hello_available_for_test(true);
    assert!(!sp_vault.try_load_silent().unwrap());
    assert!(!sp_vault.key_loaded());
    let sp_agent = Arc::new(common::a3::FakeAgent::new());
    sp_agent.unlocked.store(false, Ordering::Release);
    sp_agent.unlock_result.store(true, Ordering::Release);
    let sp_proc = CommandProcessor::new(br_core::CommandDeps {
        settings: sp_settings,
        store: Arc::new(|| None),
        keys: sp_vault.clone(),
        agent: sp_agent,
        embedder: Arc::new(common::a3::FakeEmbedder::new()),
        thumbnailer: Arc::new(common::a3::FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: Some(sp_home),
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    });
    let mut sp_ui = PortFixture::ui();
    let r = sp_proc.handle(&mut sp_ui, "unlock", &CancelToken::none());
    let (ok, data, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    assert_eq!(false, data.unwrap()["unlocked"]);
    assert!(!sp_vault.key_loaded(), "bare unlock must leave passphrase key unloaded");

    let r = sp_proc.handle(&mut sp_ui, "unlock wrong", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_WRONG_PASSPHRASE, err.unwrap());

    let r = sp_proc.handle(&mut sp_ui, "unlock my pass phrase", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_WRONG_PASSPHRASE, err.unwrap());

    let r = sp_proc.handle(&mut sp_ui, "unlock  my pass phrase ", &CancelToken::none());
    let (ok, data, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "exact leading/trailing spaces must unwrap: {err:?}");
    assert_eq!(true, data.unwrap()["unlocked"]);
    assert!(sp_vault.key_loaded());
}

#[test]
fn no_hello_t5_fallback_b15_damaged_password_file_and_b3_stuck_setup() {
    // Part 1 (T5): full no-Hello lifecycle + locked-settings exception table.
    let f = PortFixture::with_opts(PortFixtureOpts {
        hello_available: false,
        ..Default::default()
    });
    let mut ui_s = PortFixture::ui();
    f.agent.unlocked.store(false, Ordering::Release);
    f.agent.unlock_result.store(false, Ordering::Release);

    // No password set yet -> `unlock` answers ERR_NO_PASSWORD.
    let (ok, _, err) = f.call(&mut ui_s, "unlock");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NO_PASSWORD, err.unwrap());

    // Locked-settings exception table while locked with no Hello & no password:
    // method + another key -> ERR_LOCKED; method alone without password -> ERR_NO_PASSWORD;
    // unlockMethod stays "hello" after every refusal.
    for (patch, want) in [
        (
            r#"{"security":{"unlockMethod":"password","requireUnlock":false}}"#,
            CommandProcessor::ERR_LOCKED,
        ),
        (
            r#"{"security":{"unlockMethod":"password"},"ocr":{"enabled":true}}"#,
            CommandProcessor::ERR_LOCKED,
        ),
        (
            r#"{"security":{"unlockMethod":"password"}}"#,
            CommandProcessor::ERR_NO_PASSWORD,
        ),
    ] {
        let (ok, _, err) = f.call(&mut ui_s, &format!("settings set {patch}"));
        assert!(!ok, "patch should fail: {patch}");
        assert_eq!(want, err.unwrap(), "patch={patch}");
        assert_eq!("hello", f.current_settings().security.unlock_method);
    }

    // `password set` succeeds while locked when Hello is unavailable.
    let (ok, _, err) = f.call(&mut ui_s, r#"password set {"new":"fallback-pw"}"#);
    assert!(ok, "no-Hello password set while locked should pass: {err:?}");

    // Bare `unlock` tells user to use password (ERR_HELLO_MISSING).
    let (ok, _, err) = f.call(&mut ui_s, "unlock");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_HELLO_MISSING, err.unwrap());

    // Wrong password rejected with ERR_WRONG_PASSWORD.
    let (ok, _, err) = f.call(&mut ui_s, "unlock wrong-fallback-pw");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_WRONG_PASSWORD, err.unwrap());
    assert!(!f.agent.unlocked.load(Ordering::Acquire));

    // T5_hello_method_takes_the_password_when_hello_is_missing: the right password unlocks
    // although unlockMethod is "hello", and the audit names the no-Hello path once. (The "no Hello
    // prompt" guard is the unlockMethod=password case below, as in C# T5_no_hello_defaults.)
    let (ok, data, _) = f.call(&mut ui_s, "unlock fallback-pw");
    assert!(ok);
    assert_eq!(true, data.unwrap()["unlocked"]);
    assert!(f.agent.unlocked.load(Ordering::Acquire));
    let audit = f.store.read_audit(20).unwrap();
    let no_hello_count = audit
        .iter()
        .filter(|a| a.verb == "unlock password (no Hello)" && a.result_count == 1)
        .count();
    assert_eq!(1, no_hello_count);

    // Re-lock: switching unlockMethod to "password" alone while locked with no Hello succeeds!
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f.call(&mut ui_s, r#"settings set {"security":{"unlockMethod":"password"}}"#);
    assert!(ok, "no-Hello method switch while locked should pass: {err:?}");
    assert_eq!("password", f.current_settings().security.unlock_method);

    // Now unlock with unlockMethod=password opens with ZERO `unlock` agent calls (opengate only).
    f.agent.calls.lock().unwrap().clear();
    let (ok, data, _) = f.call(&mut ui_s, "unlock fallback-pw");
    assert!(ok);
    assert_eq!(true, data.unwrap()["unlocked"]);
    let calls = f.agent.calls.lock().unwrap().clone();
    assert!(!calls.contains(&"unlock".to_string()), "must not prompt Hello: {calls:?}");
    assert!(calls.contains(&"opengate".to_string()));

    // With Hello available + password set, locked method switch is STILL refused with ERR_LOCKED.
    let f_hello = PortFixture::new();
    let mut ui_h = PortFixture::ui();
    f_hello.password.set("pw-123456").unwrap();
    f_hello.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f_hello.call(
        &mut ui_h,
        r#"settings set {"security":{"unlockMethod":"password"}}"#,
    );
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    assert_eq!("hello", f_hello.current_settings().security.unlock_method);

    // Part 2 (B15): damaged unlock.bin falls back to Windows Hello when Hello works, allows `purge all`,
    // and when Hello is later unavailable + unlock.bin deleted, returns ERR_NO_PASSWORD and allows
    // switching unlockMethod back to "hello" while locked.
    let f2 = PortFixture::new();
    let mut ui_s2 = PortFixture::ui();
    f2.password.set("good-pw-1").unwrap();
    f2.set_settings(json!({"security": {"unlockMethod": "password"}}));
    let pw_file = DataPaths::unlock_password_file(f2.home.path());
    fs::write(&pw_file, b"corrupt").unwrap();
    assert_eq!(PasswordCheck::NotSet, f2.password.verify(Some("good-pw-1")));
    f2.agent.unlocked.store(false, Ordering::Release);
    f2.agent.unlock_result.store(true, Ordering::Release);
    let (ok, data, _) = f2.call(&mut ui_s2, "unlock good-pw-1");
    assert!(ok);
    assert_eq!(true, data.unwrap()["unlocked"]);
    assert!(f2.agent.calls.lock().unwrap().contains(&"unlock".to_string()));
    let audit = f2.store.read_audit(20).unwrap();
    assert!(audit.iter().any(|a| a.verb == "unlock hello (no password file)" && a.result_count == 1));
    let (ok, prev, err) = f2.call(&mut ui_s2, "purge all");
    assert!(ok, "purge all should be reachable after Hello fallback: {err:?}");
    assert!(prev.unwrap()["confirm"].as_str().is_some());

    // Lock, disable Hello, delete unlock.bin -> unlock fails with ERR_NO_PASSWORD, method switch ok.
    f2.agent.unlocked.store(false, Ordering::Release);
    f2.agent.unlock_result.store(false, Ordering::Release);
    f2.vault.set_hello_available_for_test(false);
    fs::remove_file(&pw_file).unwrap();
    assert!(!pw_file.exists());
    let (ok, _, err) = f2.call(&mut ui_s2, "unlock x");
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NO_PASSWORD, err.unwrap());
    let (ok, _, err) = f2.call(&mut ui_s2, r#"settings set {"security":{"unlockMethod":"hello"}}"#);
    assert!(ok, "method switch to hello while locked with no-unlock-path should pass: {err:?}");
    assert_eq!("hello", f2.current_settings().security.unlock_method);

    // Part 3 (Sweep 3J B7): switching to passphrase+password before setting passphrase is refused
    // with ERR_PASSPHRASE_FIRST in both directions, leaving settings unchanged until rewrap completes.
    let f3 = PortFixture::new();
    let mut ui_s3 = PortFixture::ui();
    f3.password.set("pw-123456").unwrap();
    f3.set_settings(json!({"security": {"unlockMethod": "password"}}));
    let (ok, _, err) = f3.call(
        &mut ui_s3,
        r#"settings set {"security":{"keyProtection":"passphrase"}}"#,
    );
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PASSPHRASE_FIRST, err.unwrap());
    assert_eq!("dpapi", f3.current_settings().security.key_protection);

    // Switch back to hello -> setting keyProtection=passphrase alone succeeds.
    let (ok, _, _) = f3.call(&mut ui_s3, r#"settings set {"security":{"unlockMethod":"hello"}}"#);
    assert!(ok);
    let (ok, _, _) = f3.call(
        &mut ui_s3,
        r#"settings set {"security":{"keyProtection":"passphrase"}}"#,
    );
    assert!(ok);
    // Switching unlockMethod=password while key file is still dpapi is refused with ERR_PASSPHRASE_FIRST.
    let (ok, _, err) = f3.call(
        &mut ui_s3,
        r#"settings set {"security":{"unlockMethod":"password"}}"#,
    );
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_PASSPHRASE_FIRST, err.unwrap());
    assert_eq!("hello", f3.current_settings().security.unlock_method);
    // Passphrase unlock rewraps the key -> now switching unlockMethod=password succeeds.
    f3.agent.unlocked.store(false, Ordering::Release);
    let (ok, _, err) = f3.call(&mut ui_s3, "unlock my-rewrapped-passphrase-123");
    assert!(ok, "passphrase rewrap unlock should pass: {err:?}");
    assert_eq!(KeyVault::MODE_PASSPHRASE, f3.vault.protection());
    let (ok, _, err) = f3.call(
        &mut ui_s3,
        r#"settings set {"security":{"unlockMethod":"password"}}"#,
    );
    assert!(ok, "method switch after rewrap should pass: {err:?}");
    assert_eq!("password", f3.current_settings().security.unlock_method);

    // Part 4 (Sweep 5U B3 stuck setup): no Hello, dpapi key loaded, 6-char password, settings
    // `keyProtection:passphrase, unlockMethod:hello`, locked -> unlockNeeds="password", 6-char
    // password opens, vault stays dpapi, `keyProtection:auto` saves.
    let f4 = PortFixture::new();
    let mut ui_s4 = PortFixture::ui();
    f4.password.set("pw1234").unwrap();
    f4.set_settings(json!({"security": {"keyProtection": "passphrase", "unlockMethod": "hello"}}));
    f4.vault.set_hello_available_for_test(false);
    f4.agent.unlocked.store(false, Ordering::Release);
    f4.agent.unlock_result.store(false, Ordering::Release);
    let (ok, st4, _) = f4.call(&mut ui_s4, "status");
    assert!(ok);
    assert_eq!("password", st4.unwrap()["unlockNeeds"]);
    let (ok, d4, err) = f4.call(&mut ui_s4, "unlock pw1234");
    assert!(ok, "stuck-setup 6-char password unlock should pass: {err:?}");
    assert_eq!(true, d4.unwrap()["unlocked"]);
    assert_eq!(KeyVault::MODE_DPAPI, f4.vault.protection());
    let (ok, _, err) = f4.call(&mut ui_s4, r#"settings set {"security":{"keyProtection":"auto"}}"#);
    assert!(ok, "keyProtection:auto should save after escaping stuck setup: {err:?}");

    // Part 5 (Sweep 3J B14 short unlock password): legacy state (dpapi key, keyProtection=passphrase,
    // unlockMethod=password, 6-char password "abc123") -> `unlock abc123` succeeds without passphrase rule.
    let f5 = PortFixture::new();
    let mut ui_s5 = PortFixture::ui();
    f5.password.set("abc123").unwrap();
    f5.set_settings(json!({"security": {"keyProtection": "passphrase", "unlockMethod": "password"}}));
    f5.agent.unlocked.store(false, Ordering::Release);
    let (ok, d5, err) = f5.call(&mut ui_s5, "unlock abc123");
    assert!(ok, "B14 6-char password unlock under legacy state should pass: {err:?}");
    assert_eq!(true, d5.unwrap()["unlocked"]);
}

#[test]
fn forgot_passphrase_empty_unlock_no_hello_purge_all_and_damaged_key() {
    // Passphrase key not loaded (store waiting for passphrase), Hello unavailable.
    let f = PortFixture::with_opts(PortFixtureOpts {
        key_protection: "passphrase",
        hello_available: false,
        load_silent: false,
        store_open: false,
        ..Default::default()
    });
    // Create a wrapped passphrase key on disk, then simulate a fresh boot with key unloaded.
    assert!(f.vault.unlock_with_passphrase("original-passphrase-123"));
    let home_path = f.home.path().to_path_buf();

    // Fresh KeyVault over the same home has key_loaded() == false and needs_passphrase() == true.
    let settings2 = Arc::new(br_core::SettingsStore::load(&home_path));
    let s_clone = settings2.clone();
    let vault2 = Arc::new(KeyVault::new(
        &home_path,
        move || s_clone.current(),
        None,
        false,
    ));
    vault2.set_hello_available_for_test(false);
    assert!(!vault2.try_load_silent().unwrap());
    assert!(vault2.needs_passphrase());

    // Build CommandProcessor wired to vault2, store closed, Hello unavailable.
    let agent = Arc::new(common::a3::FakeAgent::new());
    agent.unlocked.store(false, Ordering::Release);
    agent.unlock_result.store(false, Ordering::Release);
    let pa_calls = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let pa_bg = pa_calls.clone();
    let now2 = Arc::new(Mutex::new(Utc::now()));
    let now2_pw = now2.clone();
    let proc = CommandProcessor::new(br_core::CommandDeps {
        settings: settings2.clone(),
        store: Arc::new(|| None),
        keys: vault2.clone(),
        agent: agent.clone(),
        embedder: Arc::new(common::a3::FakeEmbedder::new()),
        thumbnailer: Arc::new(common::a3::FakeThumbs::new()),
        purge_all: Arc::new(move |_| {
            pa_bg.fetch_add(1, Ordering::AcqRel);
            5
        }),
        purge_all_undeleted: None,
        blobs: None,
        home: Some(home_path.clone()),
        password: Some(Arc::new(br_core::UnlockPassword::with_options(
            &home_path,
            Arc::new(move || *now2_pw.lock().unwrap()),
            1024,
            1,
            1,
        ))),
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    });
    let now2_proc = now2.clone();
    proc.set_now_for_test(move || *now2_proc.lock().unwrap());
    let mut ui_s = PortFixture::ui();

    // D1: needs-passphrase vault + store closed + gate open -> search returns ERR_LOCKED (not ERR_NOT_OPEN).
    agent.unlocked.store(true, Ordering::Release);
    let r = proc.handle(&mut ui_s, "search anything", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());
    agent.unlocked.store(false, Ordering::Release);

    // No-Hello path: `purge all` BEFORE empty unlock is rejected with ERR_UNLOCK_REQUIRED.
    let r = proc.handle(&mut ui_s, "purge all", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_UNLOCK_REQUIRED, err.unwrap());

    // Empty unlock records the no-Hello purge window even though no password is set.
    let r = proc.handle(&mut ui_s, "unlock", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_NO_PASSWORD, err.unwrap());

    // Now `purge all` + `purge confirm` works while store is waiting for forgotten passphrase.
    let r = proc.handle(&mut ui_s, "purge all", &CancelToken::none());
    let (ok, prev, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "purge all should be allowed after empty unlock: {err:?}");
    let tok = prev.unwrap()["confirm"].as_str().unwrap().to_string();
    let r = proc.handle(&mut ui_s, &format!("purge confirm {tok}"), &CancelToken::none());
    let (ok, res, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "purge confirm should pass: {err:?}");
    assert_eq!(5, res.unwrap()["purged"]);
    assert_eq!(1, pa_calls.load(Ordering::Acquire));

    // After 61 s the no-Hello purge window expires and `purge all` requires unlock again.
    *now2.lock().unwrap() += ChronoDuration::seconds(61);
    let r = proc.handle(&mut ui_s, "purge all", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_UNLOCK_REQUIRED, err.unwrap());

    // Hello-available forgotten-passphrase path (Sweep 5U B2 + Sweep 2 Keylocked_hello_without_passphrase):
    // purge all = ERR_UNLOCK_REQUIRED first, replay save = ERR_LOCKED, empty unlock ok with unlocked:false
    // & Hello called & gate kept closed, status locked + unlockNeeds:"passphrase", search = ERR_LOCKED,
    // purge all + confirm runs once.
    vault2.set_hello_available_for_test(true);
    agent.unlocked.store(false, Ordering::Release);
    agent.unlock_result.store(true, Ordering::Release);
    agent.calls.lock().unwrap().clear();
    pa_calls.store(0, Ordering::Release);

    let r = proc.handle(&mut ui_s, "purge all", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_UNLOCK_REQUIRED, err.unwrap());

    let r = proc.handle(&mut ui_s, "replay save", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());

    let r = proc.handle(&mut ui_s, "unlock", &CancelToken::none());
    let (ok, data, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "Hello empty unlock on wrapped key should succeed: {err:?}");
    assert_eq!(false, data.unwrap()["unlocked"]);
    assert!(agent.calls.lock().unwrap().contains(&"unlock".to_string()));
    assert!(!agent.unlocked.load(Ordering::Acquire), "gate must remain closed");

    let r = proc.handle(&mut ui_s, "status", &CancelToken::none());
    let (ok, st_h, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    let st_h = st_h.unwrap();
    assert_eq!(true, st_h["locked"]);
    assert_eq!("passphrase", st_h["unlockNeeds"]);

    let r = proc.handle(&mut ui_s, "search secret", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());

    let r = proc.handle(&mut ui_s, "purge all", &CancelToken::none());
    let (ok, prev_h, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "purge all after Hello empty unlock should pass: {err:?}");
    let tok_h = prev_h.unwrap()["confirm"].as_str().unwrap().to_string();
    let r = proc.handle(&mut ui_s, &format!("purge confirm {tok_h}"), &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "purge confirm should pass: {err:?}");
    assert_eq!(1, pa_calls.load(Ordering::Acquire));

    // B1_B6: wrapped key, gate open (requireUnlock=false, agent.unlocked=true), store closed:
    // UI and public status both say locked=true, timeline = ERR_LOCKED, passphrase unlock -> unlockNeeds:"none".
    assert!(
        settings2
            .try_update(&json!({"security": {"keyProtection": "auto", "requireUnlock": false}}))
            .is_none()
    );
    agent.unlocked.store(true, Ordering::Release);
    let r = proc.handle(&mut ui_s, "status", &CancelToken::none());
    let (ok, st_ui, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    let st_ui = st_ui.unwrap();
    assert_eq!(true, st_ui["locked"]);
    assert_eq!("passphrase", st_ui["unlockNeeds"]);

    let mut pub_s = PortFixture::pub_sess();
    let r = proc.handle(&mut pub_s, "status", &CancelToken::none());
    let (ok, st_pub, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    let st_pub = st_pub.unwrap();
    assert_eq!(true, st_pub["locked"]);
    assert!(st_pub.get("unlockNeeds").is_none());

    // An empty unlock (the old Hello button) opens no data: still locked, still asking for the passphrase.
    let r = proc.handle(&mut ui_s, "unlock", &CancelToken::none());
    let (ok, d, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    assert_eq!(false, d.unwrap()["unlocked"]);
    let r = proc.handle(&mut ui_s, "status", &CancelToken::none());
    let (ok, st, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    let st = st.unwrap();
    assert_eq!(true, st["locked"]);
    assert_eq!("passphrase", st["unlockNeeds"]);

    let t = Utc::now();
    let r = proc.handle(
        &mut ui_s,
        &format!(
            "timeline {} {}",
            (t - ChronoDuration::minutes(5)).to_rfc3339(),
            (t + ChronoDuration::minutes(5)).to_rfc3339()
        ),
        &CancelToken::none(),
    );
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(!ok);
    assert_eq!(CommandProcessor::ERR_LOCKED, err.unwrap());

    let r = proc.handle(&mut ui_s, "unlock original-passphrase-123", &CancelToken::none());
    let (ok, data, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "passphrase unlock should succeed: {err:?}");
    assert_eq!(true, data.unwrap()["unlocked"]);
    let r = proc.handle(&mut ui_s, "status", &CancelToken::none());
    let (ok, st_open, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    let st_open = st_open.unwrap();
    assert_eq!(false, st_open["locked"]);
    assert_eq!("none", st_open["unlockNeeds"]);
    assert!(vault2.key_loaded());

    // Keylost vault (`store.db` present on disk, NO key file): UI status gives unlockNeeds:"hello".
    let lost_home = common::TempHome::new(Some("passphrase"));
    fs::write(DataPaths::store_db(lost_home.path()), b"placeholder-db").unwrap();
    let _ = fs::remove_file(DataPaths::key_file(lost_home.path()));
    let lost_settings = Arc::new(br_core::SettingsStore::load(lost_home.path()));
    let ls_c = lost_settings.clone();
    let lost_vault = Arc::new(KeyVault::new(
        lost_home.path(),
        move || ls_c.current(),
        None,
        false,
    ));
    lost_vault.set_hello_available_for_test(true);
    assert!(lost_vault.try_load_silent().is_err() && !lost_vault.needs_passphrase());
    let lost_agent = Arc::new(common::a3::FakeAgent::new());
    lost_agent.unlocked.store(false, Ordering::Release);
    let lost_proc = CommandProcessor::new(br_core::CommandDeps {
        settings: lost_settings,
        store: Arc::new(|| None),
        keys: lost_vault,
        agent: lost_agent,
        embedder: Arc::new(common::a3::FakeEmbedder::new()),
        thumbnailer: Arc::new(common::a3::FakeThumbs::new()),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: Some(lost_home.path().to_path_buf()),
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    });
    let mut lost_ui = PortFixture::ui();
    let r = lost_proc.handle(&mut lost_ui, "status", &CancelToken::none());
    let (ok, st_lost, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    assert_eq!("hello", st_lost.unwrap()["unlockNeeds"]);

    // Damaged key file on disk: rejected by KeyVault, and at the verb level `unlock some passphrase`
    // uses Hello only (agent `unlock` called) and allows `purge all` + confirm.
    fs::write(DataPaths::key_file(&home_path), b"BRK-damaged").unwrap();
    assert!(!vault2.unlock_with_passphrase("original-passphrase-123"));
    let dmg_settings = Arc::new(br_core::SettingsStore::load(&home_path));
    let ds_c = dmg_settings.clone();
    let dmg_vault = Arc::new(KeyVault::new(&home_path, move || ds_c.current(), None, false));
    dmg_vault.set_hello_available_for_test(true);
    assert!(dmg_vault.try_load_silent().is_err());
    let dmg_agent = Arc::new(common::a3::FakeAgent::new());
    dmg_agent.unlocked.store(false, Ordering::Release);
    dmg_agent.unlock_result.store(true, Ordering::Release);
    let dmg_pa = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let dmg_pa_bg = dmg_pa.clone();
    let dmg_proc = CommandProcessor::new(br_core::CommandDeps {
        settings: dmg_settings,
        store: Arc::new(|| None),
        keys: dmg_vault,
        agent: dmg_agent.clone(),
        embedder: Arc::new(common::a3::FakeEmbedder::new()),
        thumbnailer: Arc::new(common::a3::FakeThumbs::new()),
        purge_all: Arc::new(move |_| {
            dmg_pa_bg.fetch_add(1, Ordering::AcqRel);
            3
        }),
        purge_all_undeleted: None,
        blobs: None,
        home: Some(home_path),
        password: None,
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    });
    let mut dmg_ui = PortFixture::ui();
    let r = dmg_proc.handle(&mut dmg_ui, "unlock some passphrase", &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "damaged key unlock should use Hello: {err:?}");
    assert!(dmg_agent.calls.lock().unwrap().contains(&"unlock".to_string()));
    let r = dmg_proc.handle(&mut dmg_ui, "purge all", &CancelToken::none());
    let (ok, prev_d, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "purge all after damaged-key Hello unlock should pass: {err:?}");
    let tok_d = prev_d.unwrap()["confirm"].as_str().unwrap().to_string();
    let r = dmg_proc.handle(&mut dmg_ui, &format!("purge confirm {tok_d}"), &CancelToken::none());
    let (ok, _, err) = Envelope::parse(&r.line).unwrap();
    assert!(ok, "purge confirm should pass: {err:?}");
    assert_eq!(1, dmg_pa.load(Ordering::Acquire));
}

#[test]
fn unlock_needs_status_table_follows_method_password_and_hello() {
    let f = PortFixture::new();
    let mut ui_s = PortFixture::ui();
    let mut pub_s = PortFixture::pub_sess();

    // Unlocked -> "none"; public `status` must NOT carry `unlockNeeds`.
    let (ok, st, _) = f.call(&mut ui_s, "status");
    assert!(ok);
    assert_eq!("none", st.unwrap()["unlockNeeds"]);
    let (ok, st_pub, _) = f.call(&mut pub_s, "status");
    assert!(ok);
    assert!(
        st_pub.unwrap().get("unlockNeeds").is_none(),
        "public status must never expose unlockNeeds"
    );

    // Locked with unlockMethod=hello & Hello available -> "hello".
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, st, _) = f.call(&mut ui_s, "status");
    assert!(ok);
    assert_eq!("hello", st.unwrap()["unlockNeeds"]);
    let (ok, st_pub_l, _) = f.call(&mut pub_s, "status");
    assert!(ok);
    assert!(st_pub_l.unwrap().get("unlockNeeds").is_none());

    // Locked with unlockMethod=hello, Hello missing, NO password -> "hello".
    f.vault.set_hello_available_for_test(false);
    let (ok, st, _) = f.call(&mut ui_s, "status");
    assert!(ok);
    assert_eq!("hello", st.unwrap()["unlockNeeds"]);
    f.vault.set_hello_available_for_test(true);

    // Locked with unlockMethod=password & password set -> "password".
    f.password.set("pw-123456").unwrap();
    f.agent.unlocked.store(true, Ordering::Release);
    f.set_settings(json!({"security": {"unlockMethod": "password"}}));
    f.agent.unlocked.store(false, Ordering::Release);
    let (ok, st, _) = f.call(&mut ui_s, "status");
    assert!(ok);
    assert_eq!("password", st.unwrap()["unlockNeeds"]);

    // Locked with unlockMethod=hello, Hello missing, password set -> "password" fallback.
    f.set_settings(json!({"security": {"unlockMethod": "hello"}}));
    f.vault.set_hello_available_for_test(false);
    let (ok, st, _) = f.call(&mut ui_s, "status");
    assert!(ok);
    assert_eq!("password", st.unwrap()["unlockNeeds"]);

    // Locked with keyProtection=passphrase & key loaded (Hello back, no password) -> "passphrase".
    f.password.clear().unwrap();
    f.vault.set_hello_available_for_test(true);
    f.set_settings(json!({"security": {"keyProtection": "passphrase"}}));
    let (ok, st, _) = f.call(&mut ui_s, "status");
    assert!(ok);
    assert_eq!("passphrase", st.unwrap()["unlockNeeds"]);
}
