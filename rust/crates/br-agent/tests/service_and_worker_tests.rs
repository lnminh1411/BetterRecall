// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{CaptureSettings, EmbeddingInfo, GovernanceSettings, PipeNames, Settings, WorkerRequest};
use br_core::{CancelSource, CancelToken};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use br_agent::native::tick_count64;
use br_agent::scheduling::{
    EmbeddingScheduler, EmbeddingStore, Governance, GovernanceInputs, UserNotificationState,
};
use br_agent::worker::worker_supervisor::SupervisorOptions;
use br_agent::worker::{
    KillOnCloseJob, VcRuntime, WorkerClient, WorkerError, WorkerSettingsDiff, WorkerSupervisor,
};

use common::{
    FakeStore, FakeWorker, PidWorkerLauncher, SlowWorkerLauncher, TempDir, cancel_after,
    idle_inputs, new_audio_capture, new_screen_capture, resp_err, until,
};

fn gov_in(
    cpu: f64,
    gpu: Option<f64>,
    battery: bool,
    n: UserNotificationState,
    idle_ms: i64,
) -> GovernanceInputs {
    GovernanceInputs {
        cpu_percent: cpu,
        igpu_percent: gpu,
        on_battery: battery,
        notification: n,
        user_idle_ms: idle_ms,
    }
}

// ---------------- GovernanceTests ----------------

#[test]
fn defers_on_battery_fullscreen_and_load() {
    let g = GovernanceSettings::default();
    let c = CaptureSettings::default();
    assert_eq!(
        "battery",
        Governance::decide(
            gov_in(10.0, Some(5.0), true, UserNotificationState::AcceptsNotifications, 0),
            &g,
            &c
        )
        .reason
    );
    assert!(
        !Governance::decide(
            gov_in(10.0, Some(5.0), true, UserNotificationState::AcceptsNotifications, 0),
            &g,
            &c
        )
        .run
    );
    let g_no_bat = GovernanceSettings {
        pause_on_battery: false,
        ..Default::default()
    };
    assert!(
        Governance::decide(
            gov_in(10.0, Some(5.0), true, UserNotificationState::AcceptsNotifications, 0),
            &g_no_bat,
            &c
        )
        .run
    );
    assert_eq!(
        "fullscreen",
        Governance::decide(
            gov_in(10.0, Some(5.0), false, UserNotificationState::FullscreenD3D, 0),
            &g,
            &c
        )
        .reason
    );
    assert_eq!(
        "fullscreen",
        Governance::decide(
            gov_in(10.0, Some(5.0), false, UserNotificationState::Presentation, 0),
            &g,
            &c
        )
        .reason
    );
    let c_no_fs = CaptureSettings {
        pause_on_fullscreen: false,
        ..Default::default()
    };
    assert!(
        Governance::decide(
            gov_in(10.0, Some(5.0), false, UserNotificationState::FullscreenD3D, 0),
            &g,
            &c_no_fs
        )
        .run
    );
    assert_eq!(
        "cpu",
        Governance::decide(
            gov_in(71.0, Some(5.0), false, UserNotificationState::AcceptsNotifications, 0),
            &g,
            &c
        )
        .reason
    );
    assert!(
        Governance::decide(
            gov_in(70.0, Some(5.0), false, UserNotificationState::AcceptsNotifications, 0),
            &g,
            &c
        )
        .run
    );
    assert_eq!(
        "gpu",
        Governance::decide(
            gov_in(10.0, Some(61.0), false, UserNotificationState::AcceptsNotifications, 0),
            &g,
            &c
        )
        .reason
    );
    assert!(
        Governance::decide(
            gov_in(10.0, None, false, UserNotificationState::AcceptsNotifications, 0),
            &g,
            &c
        )
        .run
    );
}

#[test]
fn drains_faster_when_user_idle() {
    let g = GovernanceSettings::default();
    let c = CaptureSettings::default();
    let busy = Governance::decide(
        gov_in(10.0, Some(5.0), false, UserNotificationState::AcceptsNotifications, 1000),
        &g,
        &c,
    );
    let idle = Governance::decide(
        gov_in(10.0, Some(5.0), false, UserNotificationState::AcceptsNotifications, 120_000),
        &g,
        &c,
    );
    assert!(idle.batch_size > busy.batch_size);
    assert!(idle.next_check < busy.next_check);
    assert_eq!("idle-drain", idle.reason);
}

#[test]
fn keyframes_pause_in_fullscreen_only_when_enabled() {
    assert!(Governance::pause_keyframes(
        UserNotificationState::FullscreenD3D,
        &CaptureSettings::default()
    ));
    assert!(!Governance::pause_keyframes(
        UserNotificationState::Busy,
        &CaptureSettings::default()
    ));
    let off = CaptureSettings {
        pause_on_fullscreen: false,
        ..Default::default()
    };
    assert!(!Governance::pause_keyframes(
        UserNotificationState::FullscreenD3D,
        &off
    ));
}

// ---------------- WorkerProtocolTests ----------------

#[test]
fn round_trip_over_real_pipe_with_pid_check() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from(r"Z:\no-such-dir"), || 120);
    let client = WorkerClient::new(sup.clone());
    let v = client
        .embed_text("hello", "query", &CancelToken::none())
        .unwrap();
    assert_eq!(vec![5.0, 0.0, 0.0, 1.0], v);
    assert_eq!("fake-model", sup.hello().unwrap().model);
    let pipe = launcher.last_pipe().unwrap();
    assert!(pipe.starts_with("BetterRecall.worker."));
    assert_eq!(32, pipe.rsplit('.').next().unwrap().len());

    let img = client
        .embed_image(&vec![0u8; 1000], "image/webp", Some(0.8), 3, Some(0.5), &CancelToken::none())
        .unwrap();
    assert!(!img.dropped_nsfw);
    assert_eq!(1000.0, img.vector.unwrap()[0]);

    let nsfw = client
        .embed_image(&[0xEE, 1, 2], "image/webp", Some(0.8), 3, Some(0.5), &CancelToken::none())
        .unwrap();
    assert!(nsfw.dropped_nsfw);
    assert!(nsfw.vector.is_none());

    let t = client
        .transcribe(&vec![0.0f32; 1600], "mic", "auto", &CancelToken::none())
        .unwrap();
    assert_eq!("1600 samples", t.text);
    assert_eq!(1, launcher.starts());
    sup.dispose();
}

#[test]
fn concurrent_requests_match_by_id() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let handles: Vec<_> = (1..=50)
        .map(|i| {
            let c = client.clone();
            thread::spawn(move || {
                let text = "a".repeat(i);
                c.embed_text(&text, "document", &CancelToken::none()).unwrap()
            })
        })
        .collect();
    for (idx, h) in handles.into_iter().enumerate() {
        let res = h.join().unwrap();
        assert_eq!((idx + 1) as f32, res[0]);
    }
    sup.dispose();
}

#[test]
fn crash_fails_pending_then_restarts_after_backoff() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let client = WorkerClient::new(sup.clone());
    client.prewarm(&CancelToken::none()).unwrap();
    launcher.state.crash_on_next.store(true, Ordering::Release);
    assert!(client.embed_text("x", "query", &CancelToken::none()).is_err());
    assert_eq!(1, sup.failures());
    // Inside backoff window.
    assert!(client.embed_text("x", "query", &CancelToken::none()).is_err());
    thread::sleep(Duration::from_millis(1100));
    let v = client.embed_text("abc", "query", &CancelToken::none()).unwrap();
    assert_eq!(3.0, v[0]);
    assert_eq!(2, launcher.starts());
    assert_eq!(0, sup.failures());
    sup.dispose();
}

#[test]
fn worker_error_surfaces_message() {
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "bench" {
            Some((resp_err(req.id, "Model not installed: embeddinggemma2."), Vec::new()))
        } else {
            None
        }
    });
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let err = WorkerClient::new(sup.clone())
        .bench(&CancelToken::none())
        .expect_err("expected error");
    assert!(err.is_reply());
    assert_eq!("Model not installed: embeddinggemma2.", err.message());
    sup.dispose();
}

#[test]
fn caller_timeout_does_not_cancel_a_slow_worker_start() {
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "hello" {
            thread::sleep(Duration::from_millis(1500));
        }
        None
    });
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let client = WorkerClient::new(sup.clone());
    for _ in 0..2 {
        let cs = cancel_after(Duration::from_millis(200));
        let err = client
            .embed_text("abc", "query", &cs.token())
            .expect_err("expected cancel");
        assert!(matches!(err, WorkerError::Cancelled));
    }
    let v = client
        .embed_text("abcd", "query", &CancelToken::none())
        .unwrap();
    assert_eq!(4.0, v[0]);
    assert_eq!(1, launcher.starts());
    assert_eq!(0, launcher.kills());
    assert_eq!(0, sup.failures());
    sup.dispose();
}

#[test]
fn hung_worker_is_recycled_with_untimed_callers() {
    let hung = Arc::new(AtomicBool::new(true));
    let hung_cb = hung.clone();
    let launcher = FakeWorker::new();
    launcher.set_no_reply(move |req| hung_cb.load(Ordering::Acquire) && req.op == "embed_text");
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.prewarm(&CancelToken::none()).unwrap();

    let c2 = client.clone();
    let job = thread::spawn(move || c2.embed_text("x", "document", &CancelToken::none()));
    thread::sleep(Duration::from_millis(100));
    sup.check_now();
    assert!(sup.is_running());
    thread::sleep(Duration::from_millis(300));
    sup.check_now();
    assert!(job.join().unwrap().is_err());
    assert_eq!(1, sup.recycled());
    assert_eq!(1, launcher.kills());
    assert!(!sup.is_running());
    assert_eq!(1, sup.failures());
    assert!(client.embed_text("x", "query", &CancelToken::none()).is_err());
    hung.store(false, Ordering::Release);
    thread::sleep(Duration::from_millis(1100));
    let v = client.embed_text("abc", "query", &CancelToken::none()).unwrap();
    assert_eq!(3.0, v[0]);
    assert_eq!(2, launcher.starts());
    assert_eq!(0, sup.failures());
    sup.dispose();
}

#[test]
fn hung_worker_is_recycled_after_caller_timeouts() {
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|req| req.op == "embed_text");
    let sup = WorkerSupervisor::with_options(
        launcher,
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            ..Default::default()
        },
    );
    let client = WorkerClient::new(sup.clone());
    client.prewarm(&CancelToken::none()).unwrap();
    let cs = cancel_after(Duration::from_millis(50));
    let _ = client.embed_text("x", "query", &cs.token());
    thread::sleep(Duration::from_millis(350));
    sup.check_now();
    assert!(until(|| sup.recycled() == 1 && !sup.is_running(), Duration::from_secs(5)));
    sup.dispose();
}

#[test]
fn shutdown_does_not_kill_the_worker_started_after_it() {
    let l = PidWorkerLauncher::new();
    let sup = WorkerSupervisor::with_options(
        l.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            verify_client_pid: false,
            ..Default::default()
        },
    );
    let client = WorkerClient::new(sup.clone());
    client.embed_text("a", "query", &CancelToken::none()).unwrap();
    let first = sup.pid();
    let sup_bg = sup.clone();
    let shut = thread::spawn(move || sup_bg.shutdown());
    thread::sleep(Duration::from_millis(100));
    client.embed_text("b", "query", &CancelToken::none()).unwrap();
    let second = sup.pid();
    shut.join().unwrap();
    assert_ne!(first, second);
    assert!(!l.killed.lock().unwrap().contains(&second));
    assert!(l.alive.lock().unwrap().contains_key(&second));
    assert!(sup.is_running());
    sup.dispose();
}

#[test]
fn late_replies_keep_a_slow_worker_alive() {
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "embed_text" {
            thread::sleep(Duration::from_millis(150));
        }
        None
    });
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(100),
            ..Default::default()
        },
    );
    let client = WorkerClient::new(sup.clone());
    client.prewarm(&CancelToken::none()).unwrap();
    for _ in 0..3 {
        let cs = cancel_after(Duration::from_millis(30));
        let _ = client.embed_text("x", "query", &cs.token());
        thread::sleep(Duration::from_millis(300));
        sup.check_now();
        assert!(sup.is_running());
    }
    assert_eq!(0, sup.recycled());
    assert_eq!(1, launcher.starts());
    sup.dispose();
}

#[test]
fn unresponsive_rule() {
    assert!(WorkerSupervisor::is_unresponsive("embed_image", 90_000, 90_000));
    assert!(!WorkerSupervisor::is_unresponsive("embed_text", 89_000, 90_000));
    assert!(WorkerSupervisor::is_unresponsive("transcribe", 90_000, 90_000));
    assert!(!WorkerSupervisor::is_unresponsive("bench", 3_600_000, 90_000));
    assert!(!WorkerSupervisor::is_unresponsive("hello", 3_600_000, 90_000));
}

#[test]
fn slow_transcribe_with_requests_queued_behind_it_is_not_a_hang() {
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "transcribe" {
            thread::sleep(Duration::from_millis(1500));
        }
        None
    });
    launcher.set_keepalive(Duration::from_millis(100), Duration::from_secs(60));
    let sup = WorkerSupervisor::with_options(
        launcher,
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.prewarm(&CancelToken::none()).unwrap();

    let c1 = client.clone();
    let t = thread::spawn(move || c1.transcribe(&vec![0.0; 1600], "mic", "auto", &CancelToken::none()));
    thread::sleep(Duration::from_millis(100));
    let c2 = client.clone();
    let e = thread::spawn(move || c2.embed_text("abc", "document", &CancelToken::none()));
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(100));
        sup.check_now();
    }
    assert!(sup.is_running());
    assert_eq!("1600 samples", t.join().unwrap().unwrap().text);
    assert_eq!(3.0, e.join().unwrap().unwrap()[0]);
    assert_eq!(0, sup.recycled());
    sup.dispose();
}

#[test]
fn long_embed_with_keepalives_is_not_recycled() {
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "embed_image" {
            thread::sleep(Duration::from_millis(1200));
        }
        None
    });
    launcher.set_keepalive(Duration::from_millis(100), Duration::from_secs(60));
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.prewarm(&CancelToken::none()).unwrap();

    let c1 = client.clone();
    let job = thread::spawn(move || {
        c1.embed_image(&[1, 2, 3], "image/webp", None, 1, None, &CancelToken::none())
    });
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(100));
        sup.check_now();
    }
    assert!(job.join().unwrap().is_ok());
    assert_eq!(0, sup.recycled());
    assert_eq!(1, launcher.starts());
    sup.dispose();
}

#[test]
fn abandoned_request_is_recycled_once_keepalives_stop() {
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "embed_text" {
            thread::sleep(Duration::from_millis(5000));
        }
        None
    });
    launcher.set_keepalive(Duration::from_millis(100), Duration::from_millis(400));
    let sup = WorkerSupervisor::with_options(
        launcher,
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            ..Default::default()
        },
    );
    let client = WorkerClient::new(sup.clone());
    client.prewarm(&CancelToken::none()).unwrap();
    let cs = cancel_after(Duration::from_millis(50));
    let _ = client.embed_text("x", "query", &cs.token());
    thread::sleep(Duration::from_millis(250));
    sup.check_now();
    assert!(sup.is_running());
    thread::sleep(Duration::from_millis(900));
    sup.check_now();
    assert!(until(|| sup.recycled() == 1, Duration::from_secs(5)));
    sup.dispose();
}

#[test]
fn request_cancelled_mid_write_stays_owed() {
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "embed_text" && req.text.as_deref() == Some("busy") {
            thread::sleep(Duration::from_millis(400));
        }
        None
    });
    launcher.set_no_reply(|req| req.op == "transcribe");
    let sup = WorkerSupervisor::with_options(
        launcher,
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(700),
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.prewarm(&CancelToken::none()).unwrap();

    let c1 = client.clone();
    let busy = thread::spawn(move || c1.embed_text("busy", "document", &CancelToken::none()));
    thread::sleep(Duration::from_millis(50));
    let cs = cancel_after(Duration::from_millis(100));
    let _ = client.transcribe(&[0.0; 16], "mic", "auto", &cs.token());
    assert_eq!(4.0, busy.join().unwrap().unwrap()[0]);
    thread::sleep(Duration::from_millis(300));
    sup.check_now();
    assert!(sup.is_running());
    thread::sleep(Duration::from_millis(700));
    sup.check_now();
    assert!(until(|| sup.recycled() == 1, Duration::from_secs(5)));
    sup.dispose();
}

#[test]
fn vc_runtime_cache_is_stale_before_the_first_check() {
    let now = tick_count64();
    assert!(!VcRuntime::fresh(i64::MIN, now));
    assert!(VcRuntime::fresh(now - 1_000, now));
    assert!(!VcRuntime::fresh(now - 61_000, now));
}

#[test]
fn vc_runtime_probe_reports_missing_dlls() {
    let dir = TempDir::new("vc");
    let worker = dir.path().join("worker");
    let on_path = dir.path().join("tools");
    fs::create_dir_all(&worker).unwrap();
    fs::create_dir_all(&on_path).unwrap();
    fs::write(worker.join("br-app-local-140.dll"), [0]).unwrap();
    fs::write(on_path.join("br-on-path-140.dll"), [0]).unwrap();
    let sys = PathBuf::from(r"C:\Windows\System32");
    assert!(VcRuntime::probe(Some(&worker), &sys, Some(&["kernel32.dll"])));
    assert!(VcRuntime::probe(Some(&worker), &sys, Some(&["br-app-local-140.dll"])));
    assert!(!VcRuntime::probe(Some(&worker), &sys, Some(&["br-on-path-140.dll"])));
    assert!(!VcRuntime::probe(
        Some(&worker),
        &sys,
        Some(&["kernel32.dll", "br-no-such-runtime-140.dll"])
    ));
    assert!(!VcRuntime::probe(None, &sys, Some(&["br-app-local-140.dll"])));
}

#[test]
fn pipe_security_denies_network_and_is_protected() {
    let sid = PipeNames::user_sid().unwrap();
    let sddl = WorkerSupervisor::pipe_sddl(&sid);
    assert!(sddl.contains("D:P"));
    assert!(sddl.contains("(D;"));
    assert!(sddl.contains(";NU)"));
}

// ---------------- SchedulerTests ----------------

#[test]
fn embeds_drops_nsfw_and_text_under_governance() {
    let store = FakeStore::new();
    let settings = Settings::default();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    store.add_screen(new_screen_capture(vec![0xEE, 2, 3]));
    store.add_audio(new_audio_capture("hello world"));
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let load = Arc::new(Mutex::new(gov_in(
        90.0,
        Some(0.0),
        false,
        UserNotificationState::AcceptsNotifications,
        0,
    )));
    let l_cb = load.clone();
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(move || settings.clone()),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(move || *l_cb.lock().unwrap()),
        Arc::new(|| false),
        None,
    );

    sched.step(&CancelToken::none());
    assert_eq!("cpu", sched.last_decision());
    assert_eq!(0, launcher.starts());
    assert_eq!(3, sched.queued());

    load.lock().unwrap().cpu_percent = 5.0;
    sched.step(&CancelToken::none());
    let g = store.inner.lock().unwrap();
    assert_eq!(2, g.vectors.len());
    assert_eq!("nsfw", g.dropped.values().next().unwrap());
    assert_eq!(
        Some(EmbeddingInfo {
            model_id: "fake-model".to_string(),
            revision: "r1".to_string(),
            dim: 4
        }),
        g.space
    );
    let audio_vec = g
        .vectors
        .iter()
        .find(|(k, _)| k.starts_with('a'))
        .unwrap()
        .1;
    assert_eq!(11.0, audio_vec[0]);
    sched.stop();
    sup.dispose();
}

#[test]
fn rejected_jobs_settle_and_missing_models_wait_with_the_image_intact() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let launcher = FakeWorker::new();
    let err_msg = Arc::new(Mutex::new(Some(
        "Model not installed: nsfw-detector.".to_string(),
    )));
    let sent = Arc::new(Mutex::new(None::<Vec<u8>>));
    let em = err_msg.clone();
    let s_cb = sent.clone();
    launcher.set_handler(move |req, payload| {
        if req.op != "embed_image" {
            return None;
        }
        *s_cb.lock().unwrap() = Some(payload.to_vec());
        em.lock()
            .unwrap()
            .as_ref()
            .map(|m| (resp_err(req.id, m), Vec::new()))
    });
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );

    assert_eq!(Duration::from_secs(600), sched.step(&CancelToken::none()));
    assert_eq!("model-missing", sched.last_decision());
    assert_eq!(1, sched.queued());
    assert!(store.inner.lock().unwrap().failed.is_empty());

    *err_msg.lock().unwrap() = Some("Bad image.".to_string());
    sched.step(&CancelToken::none());
    assert_eq!(Some(vec![1, 2, 3]), *sent.lock().unwrap());
    assert_eq!(
        "Bad image.",
        store.inner.lock().unwrap().failed.values().next().unwrap()
    );
    assert_eq!(0, sched.queued());
    sched.stop();
    sup.dispose();
}

#[test]
fn nsfw_threshold_not_sent_when_filter_off() {
    let store = FakeStore::new();
    let mut settings = Settings::default();
    settings.privacy.nsfw_filter = false;
    store.add_screen(new_screen_capture(vec![0xEE]));
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(move || settings.clone()),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );
    sched.step(&CancelToken::none());
    let g = store.inner.lock().unwrap();
    assert!(g.dropped.is_empty());
    assert_eq!(1, g.vectors.len());
    sched.stop();
    sup.dispose();
}

#[test]
fn nsfw_detector_threshold_sent_only_when_filter_and_detector_on() {
    let cases = [
        (true, true, Some(0.8), Some(0.6)),
        (true, false, Some(0.8), None),
        (false, true, None, None),
    ];
    for (filter, detector, nsfw, expected) in cases {
        let store = FakeStore::new();
        let mut settings = Settings::default();
        settings.privacy.nsfw_filter = filter;
        settings.privacy.nsfw_detector = detector;
        settings.privacy.nsfw_detector_threshold = 0.6;
        store.add_screen(new_screen_capture(vec![1, 2]));
        let launcher = FakeWorker::new();
        let seen = Arc::new(Mutex::new(None::<WorkerRequest>));
        let s_cb = seen.clone();
        launcher.set_handler(move |req, _| {
            if req.op == "embed_image" {
                *s_cb.lock().unwrap() = Some(req.clone());
            }
            None
        });
        let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
        let st_cb = store.clone();
        let sched = EmbeddingScheduler::new(
            Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
            Arc::new(move || settings.clone()),
            Arc::new(WorkerClient::new(sup.clone())),
            Arc::new(idle_inputs),
            Arc::new(|| false),
            None,
        );
        sched.step(&CancelToken::none());
        let req = seen.lock().unwrap().clone().expect("seen embed_image");
        assert_eq!(nsfw, req.nsfw);
        assert_eq!(expected, req.nsfw_detector);
        assert_eq!(Some(1), req.nsfw_grid);
        sched.stop();
        sup.dispose();
    }
}

// ---------------- ReliabilitySweep2Tests (scheduler + worker) ----------------

#[test]
fn embedding_space_is_ensured_on_a_new_store_after_purge_all() {
    let first = FakeStore::new();
    let current = Arc::new(Mutex::new(Some(first.clone() as Arc<dyn EmbeddingStore>)));
    let c_cb = current.clone();
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let sched = EmbeddingScheduler::new(
        Arc::new(move || c_cb.lock().unwrap().clone()),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );
    first.add_screen(new_screen_capture(vec![1, 2, 3]));
    sched.step(&CancelToken::none());
    let expected = Some(EmbeddingInfo {
        model_id: "fake-model".to_string(),
        revision: "r1".to_string(),
        dim: 4,
    });
    assert_eq!(expected, first.inner.lock().unwrap().space);

    let second = FakeStore::new();
    *current.lock().unwrap() = Some(second.clone() as Arc<dyn EmbeddingStore>);
    second.add_screen(new_screen_capture(vec![1, 2, 3]));
    sched.step(&CancelToken::none());
    assert_eq!(expected, second.inner.lock().unwrap().space);
    assert_eq!(1, second.inner.lock().unwrap().vectors.len());
    sched.stop();
    sup.dispose();
}

#[test]
fn hello_before_the_store_opens_is_applied_once_it_opens() {
    let current = Arc::new(Mutex::new(None::<Arc<dyn EmbeddingStore>>));
    let store = FakeStore::new();
    let c_cb = current.clone();
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let sched = EmbeddingScheduler::new(
        Arc::new(move || c_cb.lock().unwrap().clone()),
        Arc::new(Settings::default),
        client.clone(),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );
    client
        .embed_text("prewarm", "query", &CancelToken::none())
        .unwrap();
    *current.lock().unwrap() = Some(store.clone() as Arc<dyn EmbeddingStore>);
    sched.step(&CancelToken::none());
    assert_eq!(
        Some(EmbeddingInfo {
            model_id: "fake-model".to_string(),
            revision: "r1".to_string(),
            dim: 4
        }),
        store.inner.lock().unwrap().space
    );
    sched.stop();
    sup.dispose();
}

#[test]
fn device_failures_are_retried_and_settled_only_after_the_attempt_limit() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let launcher = FakeWorker::new();
    let failures_left = Arc::new(AtomicI32::new(2));
    let fl = failures_left.clone();
    launcher.set_handler(move |req, _| {
        if req.op == "embed_image" && fl.fetch_sub(1, Ordering::AcqRel) > 0 {
            Some((
                resp_err(req.id, "Device failed: nsfw gate failed on every device"),
                Vec::new(),
            ))
        } else {
            None
        }
    });
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );

    assert!(sched.step(&CancelToken::none()) > Duration::ZERO);
    assert_eq!("worker-error", sched.last_decision());
    assert!(store.inner.lock().unwrap().failed.is_empty());
    assert_eq!(1, sched.queued());
    sched.step(&CancelToken::none());
    assert!(store.inner.lock().unwrap().failed.is_empty());
    sched.step(&CancelToken::none());
    assert_eq!(1, store.inner.lock().unwrap().vectors.len());
    assert!(store.inner.lock().unwrap().failed.is_empty());

    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    failures_left.store(i32::MAX / 2, Ordering::Release);
    for _ in 0..EmbeddingScheduler::MAX_TRANSIENT_ATTEMPTS {
        sched.step(&CancelToken::none());
    }
    assert_eq!(
        "Device failed: nsfw gate failed on every device",
        store.inner.lock().unwrap().failed.values().next().unwrap()
    );
    assert_eq!(0, sched.queued());
    sched.stop();
    sup.dispose();
}

#[test]
fn bad_requests_are_still_settled_at_once() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "embed_image" {
            Some((resp_err(req.id, "Bad request."), Vec::new()))
        } else {
            None
        }
    });
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );
    sched.step(&CancelToken::none());
    assert_eq!(
        "Bad request.",
        store.inner.lock().unwrap().failed.values().next().unwrap()
    );
    sched.stop();
    sup.dispose();
}

#[test]
fn a_job_that_keeps_crashing_the_worker_is_set_aside() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    store.add_audio(new_audio_capture("hello"));
    let launcher = FakeWorker::new();
    launcher.set_crash_when(|req| req.op == "embed_image");
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    while store.inner.lock().unwrap().failed.is_empty() && Instant::now() < deadline {
        sched.step(&CancelToken::none());
        thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        "worker crashed",
        store.inner.lock().unwrap().failed.values().next().unwrap()
    );
    assert_eq!(
        EmbeddingScheduler::MAX_CRASH_ATTEMPTS as usize,
        launcher.starts()
    );
    while store.inner.lock().unwrap().vectors.is_empty() && Instant::now() < deadline {
        sched.step(&CancelToken::none());
        thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(1, store.inner.lock().unwrap().vectors.len());
    sched.stop();
    sup.dispose();
}

#[test]
fn transient_worker_errors() {
    assert!(EmbeddingScheduler::is_transient(&WorkerError::Reply(
        "Device failed: npu lost".to_string()
    )));
    assert!(EmbeddingScheduler::is_transient(&WorkerError::Reply(
        "Internal error.".to_string()
    )));
    assert!(!EmbeddingScheduler::is_transient(&WorkerError::Reply(
        "Bad request.".to_string()
    )));
    assert!(!EmbeddingScheduler::is_transient(&WorkerError::Reply(
        "Model not installed: ocr.".to_string()
    )));
}

#[test]
fn worker_side_settings_are_detected() {
    let a = Settings::default();
    assert_eq!(None, WorkerSettingsDiff::changed(&a, &Settings::default()));
    let mut b = Settings::default();
    b.ocr.languages = "en,vi".to_string();
    assert_eq!(Some("ocr.languages"), WorkerSettingsDiff::changed(&a, &b));
    let mut c = Settings::default();
    c.privacy.nsfw_device = "cpu".to_string();
    assert_eq!(
        Some("privacy.nsfwDevice"),
        WorkerSettingsDiff::changed(&a, &c)
    );
    let mut d = Settings::default();
    d.index.dim = 512;
    assert_eq!(Some("index.dim"), WorkerSettingsDiff::changed(&a, &d));
    let mut e = Settings::default();
    e.capture.interval_ms = 5000;
    e.privacy.nsfw_threshold = 0.5;
    assert_eq!(None, WorkerSettingsDiff::changed(&a, &e));
}

#[test]
fn restart_request_shuts_the_idle_worker_down_and_the_next_request_starts_a_new_one() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let client = WorkerClient::new(sup.clone());
    client.embed_text("a", "query", &CancelToken::none()).unwrap();
    assert_eq!(1, launcher.starts());
    sup.request_restart("ocr.languages");
    assert!(until(|| !sup.is_running(), Duration::from_secs(10)));
    assert!(!sup.restart_pending());
    client.embed_text("b", "query", &CancelToken::none()).unwrap();
    assert_eq!(2, launcher.starts());
    assert_eq!(0, sup.failures());
    sup.dispose();
}

#[test]
fn restart_waits_for_the_request_in_flight() {
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|req| req.op == "embed_text" && req.text.as_deref() == Some("slow"));
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.embed_text("a", "query", &CancelToken::none()).unwrap();

    let cs = Arc::new(CancelSource::new());
    let tok = cs.token();
    let c2 = client.clone();
    let slow = thread::spawn(move || c2.embed_text("slow", "query", &tok));
    thread::sleep(Duration::from_millis(200));
    sup.request_restart("inference.device");
    assert!(sup.restart_pending());
    assert!(sup.is_running());
    cs.cancel();
    assert!(slow.join().unwrap().is_err());
    sup.check_now();
    assert!(until(|| !sup.is_running() && !sup.restart_pending(), Duration::from_secs(10)));
    sup.dispose();
}

// ---------------- Sweep2GAgentTests ----------------

#[test]
fn cancelled_bench_stops_the_worker_without_a_backoff() {
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|req| req.op == "bench");
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from(r"Z:\no-such-dir"), || 120);
    let client = WorkerClient::new(sup.clone());
    assert_eq!(
        4,
        client
            .embed_text("warm", "query", &CancelToken::none())
            .unwrap()
            .len()
    );
    let cs = cancel_after(Duration::from_millis(300));
    let err = client.bench(&cs.token()).expect_err("expected cancel");
    assert!(matches!(err, WorkerError::Cancelled));
    assert_eq!(1, launcher.kills());
    assert_eq!(0, sup.failures());

    let v = client
        .embed_text("after", "query", &CancelToken::none())
        .unwrap();
    assert_eq!(5.0, v[0]);
    assert_eq!(2, launcher.starts());
    sup.dispose();
}

// ---------------- Sweep3IAgentTests ----------------

#[test]
fn a2_ocr_timeout_while_its_write_waits_keeps_the_worker_and_the_transcribe() {
    let l = SlowWorkerLauncher::new(3000, false);
    let sup = WorkerSupervisor::with_options(
        l.clone(),
        PathBuf::from("x"),
        || 600,
        SupervisorOptions {
            verify_client_pid: false,
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.prewarm(&CancelToken::none()).unwrap();
    let pid = sup.pid();

    let c1 = client.clone();
    let tr = thread::spawn(move || {
        c1.transcribe(&vec![0.0; 16000], "mic", "auto", &CancelToken::none())
    });
    thread::sleep(Duration::from_millis(300));
    let cs = cancel_after(Duration::from_millis(500));
    let err = client
        .ocr(&vec![0u8; 64 * 64 * 4], 64, 64, 256, false, &cs.token())
        .expect_err("expected cancel");
    assert!(matches!(err, WorkerError::Cancelled));

    let t = tr.join().unwrap().unwrap();
    assert_eq!("hi", t.text);
    assert_eq!(0, sup.failures());
    assert!(sup.is_running());
    assert_eq!(pid, sup.pid());
    assert!(l.killed.lock().unwrap().is_empty());

    let ocr = client
        .ocr(
            &vec![0u8; 64 * 64 * 4],
            64,
            64,
            256,
            false,
            &CancelToken::none(),
        )
        .unwrap();
    assert_eq!("fake", ocr.engine);
    assert_eq!(1, l.started.lock().unwrap().len());
    sup.dispose();
}

#[test]
fn a2_broken_frame_kills_that_worker_process() {
    let l = SlowWorkerLauncher::new(10, true);
    let sup = WorkerSupervisor::with_options(
        l.clone(),
        PathBuf::from("x"),
        || 600,
        SupervisorOptions {
            verify_client_pid: false,
            ..Default::default()
        },
    );
    let client = WorkerClient::new(sup.clone());
    client.prewarm(&CancelToken::none()).unwrap();
    let pid = sup.pid();
    assert!(
        client
            .ocr(&[0u8; 16], 2, 2, 8, false, &CancelToken::none())
            .is_err()
    );
    assert!(until(|| !l.killed.lock().unwrap().is_empty(), Duration::from_secs(5)));
    assert_eq!(vec![pid], *l.killed.lock().unwrap());
    assert!(!sup.is_running());
    assert_eq!(1, sup.failures());
    sup.dispose();
}

#[test]
fn a2_job_limit_struct_has_the_native_size() {
    let expected = if cfg!(target_pointer_width = "64") {
        144
    } else {
        112
    };
    assert_eq!(expected, KillOnCloseJob::extended_limit_info_size());
}

#[test]
fn a2_closing_the_job_kills_its_processes() {
    let job = KillOnCloseJob::try_create().expect("CreateJobObjectW");
    let mut child = Command::new(r"C:\Windows\System32\cmd.exe")
        .args(["/c", "ping", "-n", "30", "127.0.0.1", ">nul"])
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd.exe");

    let assigned = unsafe { job.assign(HANDLE(child.as_raw_handle())) };
    assert!(assigned);
    assert!(child.try_wait().unwrap().is_none());
    job.close();
    assert!(until(
        || matches!(child.try_wait(), Ok(Some(_))),
        Duration::from_secs(10)
    ));
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------- Sweep2FTests: EmbeddingScheduler purge zeroize ----------------

#[test]
fn purged_ids_leave_the_embedding_queue_with_their_images_zeroed() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let sched = EmbeddingScheduler::new(
        Arc::new(|| Some(FakeStore::new() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(|| {
            gov_in(
                1.0,
                Some(0.0),
                false,
                UserNotificationState::AcceptsNotifications,
                0,
            )
        }),
        Arc::new(|| false),
        None,
    );
    let gone = br_contracts::PendingJob {
        id: "s_gone".to_string(),
        kind: br_contracts::CaptureKind::Screen,
        image: Some(vec![7, 7, 7]),
        image_mime: Some("image/webp".to_string()),
        text: None,
    };
    let kept = br_contracts::PendingJob {
        id: "s_kept".to_string(),
        kind: br_contracts::CaptureKind::Screen,
        image: Some(vec![5, 5]),
        image_mime: Some("image/webp".to_string()),
        text: None,
    };
    sched.enqueue_for_test([gone, kept]);
    let mut ids = std::collections::HashSet::new();
    ids.insert("s_gone".to_string());
    let (removed_count, removed) = sched.remove_for_test(&ids);
    assert_eq!(1, removed_count);
    assert_eq!(1, sched.queued());
    assert_eq!(1, removed.len());
    // C# checks the shared array reads 0,0,0; Rust owns the buffer and `zeroize` wipes it, then truncates it.
    assert_eq!(Some(Vec::<u8>::new()), removed[0].image);
    let remaining = sched.queued_jobs_for_test();
    assert_eq!(1, remaining.len());
    assert_eq!(Some(vec![5, 5]), remaining[0].image);
    sched.stop();
    sup.dispose();
}
