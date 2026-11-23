// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{
    BenchResult, BinaryFrame, CaptureSettings, EmbeddingInfo, GovernanceSettings, Settings,
    WorkerRequest,
};
use br_core::{CancelSource, CancelToken};

use br_agent::scheduling::{
    EmbeddingScheduler, EmbeddingStore, Governance, GovernanceInputs, UserNotificationState,
};
use br_agent::worker::worker_supervisor::SupervisorOptions;
use br_agent::worker::{WorkerClient, WorkerError, WorkerSupervisor};

use common::{
    FakeStore, FakeWorker, RealStore, busy_inputs, cancel_after, default_hello, idle_inputs,
    new_audio_capture, new_screen_capture, resp_err, resp_ok, until,
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
fn a_job_that_keeps_crashing_the_worker_is_set_aside() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    store.add_audio(new_audio_capture("hello"));
    let launcher = FakeWorker::new();
    launcher.set_crash_when(|req| req.op == "embed_image");
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            backoff_base_ms: 50,
            ..Default::default()
        },
    );
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while store.inner.lock().unwrap().failed.is_empty() && Instant::now() < deadline {
        sched.step(&CancelToken::none());
        thread::sleep(Duration::from_millis(20));
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
        thread::sleep(Duration::from_millis(20));
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
    let mut ids = HashSet::new();
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

#[test]
fn hello_without_an_embedder_does_not_requeue_the_history() {
    let rs = RealStore::new("s4w-noemb");
    for i in 0..3 {
        rs.add(i);
    }
    let launcher = FakeWorker::new();
    let rev = Arc::new(Mutex::new("r1".to_string()));
    let rev_cb = rev.clone();
    launcher.set_handler(move |req, _| {
        let r = rev_cb.lock().unwrap().clone();
        if req.op == "hello" {
            let mut resp = resp_ok(req.id);
            resp.hello = Some(default_hello(&r, 4, "CPU"));
            return Some((resp, Vec::new()));
        }
        if r.is_empty() && req.op.starts_with("embed") {
            return Some((
                resp_err(req.id, "Model not installed: embeddinggemma-2."),
                Vec::new(),
            ));
        }
        None
    });
    let sup = WorkerSupervisor::new(launcher, rs.dir.path().to_path_buf(), || 120);
    let store_ref = rs.store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(store_ref.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        Some(Arc::new(|| 4)),
    );

    sched.step(&CancelToken::none());
    assert_eq!(0, rs.pending());

    sup.shutdown();
    *rev.lock().unwrap() = String::new();
    rs.add(3);
    sched.step(&CancelToken::none());
    assert_eq!("model-missing", sched.last_decision());
    assert_eq!(1, rs.pending());

    sup.shutdown();
    *rev.lock().unwrap() = "r1".to_string();
    sched.step(&CancelToken::none());
    assert_eq!(0, rs.pending());
    assert_eq!(4, rs.store.stats().unwrap().vectors);
    sched.stop();
    sup.dispose();
}

// Absorbs sweep 5W A6: step with refused/null space returns 60s wait and never calls worker embed ops.
#[test]
fn hello_with_another_dim_is_refused() {
    let store = FakeStore::new();
    store.add_audio(new_audio_capture("hello world"));
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let embed_calls = Arc::new(AtomicUsize::new(0));
    let ec = embed_calls.clone();
    let launcher = FakeWorker::new();
    launcher.set_handler(move |req, _| {
        if req.op == "hello" {
            let mut r = resp_ok(req.id);
            r.hello = Some(default_hello("r1", 4, "CPU"));
            return Some((r, Vec::new()));
        }
        if req.op.starts_with("embed") {
            ec.fetch_add(1, Ordering::AcqRel);
        }
        None
    });
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        Some(Arc::new(|| 768)),
    );

    let wait = sched.step(&CancelToken::none());
    assert_eq!(Duration::from_secs(60), wait);
    let g = store.inner.lock().unwrap();
    assert!(g.space.is_none());
    assert!(g.vectors.is_empty());
    assert!(g.failed.is_empty());
    assert_eq!(2, sched.queued());
    assert_eq!(0, embed_calls.load(Ordering::Acquire));
    assert_eq!("space-refused", sched.last_decision());
    assert!(sup.restart_pending());
    sched.stop();
    sup.dispose();
}

#[test]
fn embed_reply_naming_a_new_model_moves_the_space_and_the_device() {
    let rs = RealStore::new("s4w-move");
    rs.add(0);
    rs.add(1);
    let launcher = FakeWorker::new();
    let model = Arc::new(Mutex::new(None::<String>));
    let m_cb = model.clone();
    launcher.set_handler(move |req, payload| match req.op.as_str() {
        "embed_image" => {
            let m = m_cb.lock().unwrap().clone()?;
            let mut r = resp_ok(req.id);
            r.dim = Some(4);
            r.ms = Some(1.0);
            r.model = Some(m);
            r.revision = Some("r2".to_string());
            r.device = Some("GPU".to_string());
            Some((
                r,
                BinaryFrame::floats_to_bytes(&[payload.len() as f32, 1.0, 0.0, 0.0]),
            ))
        }
        "bench" => {
            let mut r = resp_ok(req.id);
            r.bench = Some(BenchResult {
                chosen: "GPU".to_string(),
                devices: Vec::new(),
                tokens: None,
                chosen_adapter: None,
            });
            r.model = Some("fake-model".to_string());
            r.revision = Some("r2".to_string());
            r.device = Some("GPU".to_string());
            Some((r, Vec::new()))
        }
        _ => None,
    });
    let sup = WorkerSupervisor::new(launcher, rs.dir.path().to_path_buf(), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let st_cb = rs.store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        client.clone(),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        Some(Arc::new(|| 4)),
    );

    sched.step(&CancelToken::none());
    assert_eq!(0, rs.pending());
    assert_eq!("CPU", sup.hello().unwrap().device);

    client.bench(&CancelToken::none()).unwrap();
    assert_eq!("GPU", sup.hello().unwrap().device);
    assert_eq!("r2", sup.hello().unwrap().revision);
    assert_eq!(0, rs.pending());

    *model.lock().unwrap() = Some("fake-model".to_string());
    rs.add(2);
    let before = sched.processed();
    sched.step(&CancelToken::none());
    assert_eq!(3, sched.processed() - before);
    assert_eq!(0, rs.pending());
    assert_eq!(3, rs.store.stats().unwrap().vectors);
    sched.stop();
    sup.dispose();
}

#[test]
fn vector_from_another_embedder_is_not_filed_under_the_accepted_space() {
    let store = FakeStore::new();
    store.add_audio(new_audio_capture("hello world"));
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "embed_text" {
            let mut r = resp_ok(req.id);
            r.dim = Some(4);
            r.ms = Some(1.0);
            r.model = Some("other-model".to_string());
            r.revision = Some(String::new());
            r.device = Some("CPU".to_string());
            Some((r, BinaryFrame::floats_to_bytes(&[1.0, 0.0, 0.0, 0.0])))
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
    assert_eq!(Duration::from_secs(60), sched.step(&CancelToken::none()));
    assert_eq!("space-refused", sched.last_decision());
    assert!(store.inner.lock().unwrap().vectors.is_empty());
    assert_eq!(1, sched.queued());
    assert_eq!("fake-model", sup.hello().unwrap().model);
    sched.stop();
    sup.dispose();
}

#[test]
fn jobs_cleared_by_panic_are_handed_out_again_after_the_lease() {
    let rs = RealStore::new("s4w-lease");
    assert_eq!(Duration::from_secs(120), rs.store.embedding_lease_timeout());
    rs.store
        .set_embedding_lease_timeout(Duration::from_millis(400));
    for i in 0..3 {
        rs.add(i);
    }
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher, rs.dir.path().to_path_buf(), || 120);
    let load = Arc::new(Mutex::new(busy_inputs()));
    let l_cb = load.clone();
    let st_cb = rs.store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(move || *l_cb.lock().unwrap()),
        Arc::new(|| false),
        None,
    );

    sched.step(&CancelToken::none());
    assert_eq!("cpu", sched.last_decision());
    assert_eq!(3, sched.queued());
    sched.clear();
    *load.lock().unwrap() = idle_inputs();
    sched.step(&CancelToken::none());
    assert_eq!("empty", sched.last_decision());
    assert_eq!(3, rs.pending());

    thread::sleep(Duration::from_millis(600));
    sched.step(&CancelToken::none());
    assert_eq!(0, rs.pending());
    assert_eq!(3, rs.store.stats().unwrap().vectors);
    sched.stop();
    sup.dispose();
}

#[test]
fn jobs_held_by_the_scheduler_are_already_in_the_store_count() {
    let rs = RealStore::new("s4w-count");
    for i in 0..3 {
        rs.add(i);
    }
    let sup = WorkerSupervisor::new(FakeWorker::new(), rs.dir.path().to_path_buf(), || 120);
    let st_cb = rs.store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(busy_inputs),
        Arc::new(|| false),
        None,
    );
    sched.step(&CancelToken::none());
    assert_eq!(3, sched.queued());
    assert_eq!(3, rs.pending());
    sched.stop();
    sup.dispose();
}

#[test]
fn purge_of_the_job_in_flight_does_not_requeue_it() {
    let store = FakeStore::new();
    let id = store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let launcher = FakeWorker::new();
    let entered = Arc::new((Mutex::new(0usize), Condvar::new()));
    let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
    let sends = Arc::new(AtomicUsize::new(0));
    let e_cb = entered.clone();
    let g_cb = gate.clone();
    let s_cb = sends.clone();
    launcher.set_handler(move |req, _| {
        if req.op != "embed_image" {
            return None;
        }
        s_cb.fetch_add(1, Ordering::AcqRel);
        {
            let (l, cv) = &*e_cb;
            *l.lock().unwrap() += 1;
            cv.notify_all();
        }
        {
            let (l, cv) = &*g_cb;
            let mut g = l.lock().unwrap();
            while *g == 0 {
                let (g2, _) = cv.wait_timeout(g, Duration::from_secs(10)).unwrap();
                g = g2;
            }
            *g -= 1;
        }
        Some((resp_err(req.id, "Internal error."), Vec::new()))
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

    let sc1 = sched.clone();
    let t = thread::spawn(move || sc1.step(&CancelToken::none()));
    assert!(until(
        || *entered.0.lock().unwrap() >= 1,
        Duration::from_secs(10)
    ));
    let mut set = HashSet::new();
    set.insert(id);
    sched.remove(&set);
    {
        *gate.0.lock().unwrap() += 1;
        gate.1.notify_all();
    }
    t.join().unwrap();
    assert_eq!(0, sched.queued());
    sched.step(&CancelToken::none());
    assert_eq!(1, sends.load(Ordering::Acquire));
    assert!(store.inner.lock().unwrap().failed.is_empty());

    store.add_screen(new_screen_capture(vec![4, 5, 6]));
    let sc2 = sched.clone();
    let t2 = thread::spawn(move || sc2.step(&CancelToken::none()));
    assert!(until(
        || *entered.0.lock().unwrap() >= 2,
        Duration::from_secs(10)
    ));
    sched.clear();
    {
        *gate.0.lock().unwrap() += 1;
        gate.1.notify_all();
    }
    t2.join().unwrap();
    assert_eq!(0, sched.queued());
    assert_eq!(2, sends.load(Ordering::Acquire));
    sched.stop();
    sup.dispose();
}

#[test]
fn three_cancelled_benches_do_not_set_the_queued_job_aside() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![0u8; 64 * 1024]));
    let launcher = FakeWorker::new();
    let bench_in = Arc::new(AtomicUsize::new(0));
    let bench_gate = Arc::new((Mutex::new(0usize), Condvar::new()));
    let bi = bench_in.clone();
    let bg = bench_gate.clone();
    launcher.set_handler(move |req, _| {
        if req.op != "bench" {
            return None;
        }
        bi.fetch_add(1, Ordering::AcqRel);
        let (l, cv) = &*bg;
        let mut g = l.lock().unwrap();
        while *g == 0 {
            let (g2, _) = cv.wait_timeout(g, Duration::from_secs(10)).unwrap();
            g = g2;
        }
        *g -= 1;
        let mut r = resp_ok(req.id);
        r.bench = Some(BenchResult {
            chosen: "CPU".to_string(),
            devices: Vec::new(),
            tokens: None,
            chosen_adapter: None,
        });
        Some((r, Vec::new()))
    });
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        client.clone(),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );
    let ct = cancel_after(Duration::from_secs(30));

    for round in 0..EmbeddingScheduler::MAX_CRASH_ATTEMPTS as usize {
        let cs = Arc::new(CancelSource::new());
        let tok = cs.token();
        let c2 = client.clone();
        let bench = thread::spawn(move || c2.bench(&tok));
        assert!(until(
            || bench_in.load(Ordering::Acquire) > round,
            Duration::from_secs(10)
        ));
        let conn = sup.ensure(&ct.token()).unwrap();
        let sc2 = sched.clone();
        let tok_step = ct.token();
        let step = thread::spawn(move || sc2.step(&tok_step));
        assert!(
            until(
                || conn.write_gate_state().held || conn.pending() >= 2,
                Duration::from_secs(10)
            ),
            "scheduler step never entered worker connection"
        );
        cs.cancel();
        assert!(bench.join().unwrap().is_err());
        step.join().unwrap();
        {
            *bench_gate.0.lock().unwrap() += 1;
            bench_gate.1.notify_all();
        }
        assert_eq!("worker-unavailable", sched.last_decision());
    }
    assert!(store.inner.lock().unwrap().failed.is_empty());
    assert_eq!(1, sched.queued());
    sched.step(&ct.token());
    assert_eq!(1, store.inner.lock().unwrap().vectors.len());
    sched.stop();
    sup.dispose();
}

// sweep 5W C5: merges model-missing backoff and governance defer signal-burst tests.
#[test]
fn signal_burst_does_not_bypass_model_missing_or_governance_defer() {
    for (model_missing, expected_decision) in [(true, "model-missing"), (false, "cpu")] {
        let store = FakeStore::new();
        store.add_screen(new_screen_capture(vec![1, 2, 3]));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_cb = calls.clone();
        let launcher = FakeWorker::new();
        if model_missing {
            let ec = calls_cb.clone();
            launcher.set_handler(move |req, _| {
                if req.op != "embed_image" {
                    return None;
                }
                ec.fetch_add(1, Ordering::AcqRel);
                Some((
                    resp_err(req.id, "Model not installed: embeddinggemma-2."),
                    Vec::new(),
                ))
            });
        }
        let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
        let st_cb = store.clone();
        let probe_calls = calls.clone();
        let sched = EmbeddingScheduler::new(
            Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
            Arc::new(Settings::default),
            Arc::new(WorkerClient::new(sup.clone())),
            Arc::new(move || {
                if model_missing {
                    idle_inputs()
                } else {
                    probe_calls.fetch_add(1, Ordering::AcqRel);
                    busy_inputs()
                }
            }),
            Arc::new(|| false),
            None,
        );
        for _ in 0..20 {
            sched.signal();
        }
        sched.start();
        assert!(until(
            || calls.load(Ordering::Acquire) >= 1 && sched.last_decision() == expected_decision,
            Duration::from_secs(10)
        ));
        for _ in 0..20 {
            sched.signal();
        }
        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            1,
            calls.load(Ordering::Acquire),
            "case={expected_decision}"
        );
        assert_eq!(expected_decision, sched.last_decision());
        sched.stop();
        sup.dispose();
    }
}

// sweep 5W C5/A6
#[test]
fn signal_burst_does_not_bypass_space_refused_and_accepted_hello_wakes_it() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let embed_calls = Arc::new(AtomicUsize::new(0));
    let probe_calls = Arc::new(AtomicUsize::new(0));
    let hello_dim = Arc::new(AtomicI32::new(8));
    let ec = embed_calls.clone();
    let hd = hello_dim.clone();
    let launcher = FakeWorker::new();
    launcher.set_handler(move |req, _| {
        if req.op == "hello" {
            let mut r = resp_ok(req.id);
            r.hello = Some(default_hello("r1", hd.load(Ordering::Acquire), "CPU"));
            return Some((r, Vec::new()));
        }
        if req.op.starts_with("embed") {
            ec.fetch_add(1, Ordering::AcqRel);
        }
        None
    });
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let st_cb = store.clone();
    let pc = probe_calls.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        client.clone(),
        Arc::new(move || {
            pc.fetch_add(1, Ordering::AcqRel);
            idle_inputs()
        }),
        Arc::new(|| false),
        Some(Arc::new(|| 4)),
    );
    for _ in 0..20 {
        sched.signal();
    }
    sched.start();
    assert!(until(
        || probe_calls.load(Ordering::Acquire) >= 1 && sched.last_decision() == "space-refused",
        Duration::from_secs(10)
    ));
    for _ in 0..20 {
        sched.signal();
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(1, probe_calls.load(Ordering::Acquire));
    assert_eq!(0, embed_calls.load(Ordering::Acquire));
    assert_eq!("space-refused", sched.last_decision());

    hello_dim.store(4, Ordering::Release);
    sup.shutdown();
    client.prewarm(&CancelToken::none()).unwrap();
    assert!(until(
        || store.inner.lock().unwrap().vectors.len() == 1,
        Duration::from_secs(10)
    ));
    assert_eq!(1, embed_calls.load(Ordering::Acquire));
    sched.stop();
    sup.dispose();
}

// sweep 5W C5
#[test]
fn signal_still_wakes_empty_scheduler_once() {
    let store = FakeStore::new();
    let sup = WorkerSupervisor::new(FakeWorker::new(), PathBuf::from("x"), || 120);
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );
    sched.start();
    assert!(until(
        || sched.last_decision() == "empty",
        Duration::from_secs(10)
    ));
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    for _ in 0..20 {
        sched.signal();
    }
    assert!(until(
        || store.inner.lock().unwrap().vectors.len() == 1,
        Duration::from_secs(10)
    ));
    sched.stop();
    sup.dispose();
}

// sweep 5W A3
#[test]
fn embedding_scheduler_behind_urgent_query_does_not_take_crash_strike() {
    let store = FakeStore::new();
    let capture_id = store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let running = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let r_cb = running.clone();
    let g_cb = gate.clone();
    let launcher = FakeWorker::new();
    launcher.set_handler(move |req, _| {
        if req.op == "transcribe" {
            r_cb.fetch_add(1, Ordering::AcqRel);
            let (l, cv) = &*g_cb;
            let mut g = l.lock().unwrap();
            while !*g {
                let (g2, _) = cv.wait_timeout(g, Duration::from_secs(10)).unwrap();
                g = g2;
            }
        }
        None
    });
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            liveness_clock: Some(Arc::new(|| 1000)),
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();
    let conn = sup.ensure(&ct.token()).unwrap();
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        client.clone(),
        Arc::new(idle_inputs),
        Arc::new(|| false),
        None,
    );

    let c0 = client.clone();
    let tok0 = ct.token();
    let t = thread::spawn(move || c0.transcribe(&vec![0.0; 1600], "mic", "auto", &tok0));
    assert!(until(
        || running.load(Ordering::Acquire) >= 1,
        Duration::from_secs(10)
    ));

    let mut big = vec![0u8; 2 << 20];
    big[0] = 1;
    let c1 = client.clone();
    let tok1 = ct.token();
    let i1 = thread::spawn(move || {
        c1.embed_image(&big, "image/webp", None, 1, None, &tok1)
    });
    assert!(until(
        || {
            let st = conn.write_gate_state();
            st.held && st.normal == 0 && st.urgent == 0
        },
        Duration::from_secs(20)
    ));

    let sc = sched.clone();
    let tok_s = ct.token();
    let step = thread::spawn(move || sc.step(&tok_s));
    assert!(until(
        || conn.write_gate_state().normal == 1,
        Duration::from_secs(20)
    ));

    let c2 = client.clone();
    let tok2 = ct.token();
    let q = thread::spawn(move || c2.embed_text("where is invoice", "query", &tok2));
    assert!(until(
        || conn.write_gate_state().urgent == 1,
        Duration::from_secs(20)
    ));

    launcher.set_no_reply(|r| r.op == "embed_text");
    launcher.set_crash_when(|r| r.op == "embed_image" && r.nsfw.is_some());
    {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    t.join().unwrap().unwrap();
    i1.join().unwrap().unwrap();
    let eq = q.join().unwrap().expect_err("query fails");
    step.join().unwrap();

    assert_eq!("Worker pipe closed.", eq.message());
    assert!(EmbeddingScheduler::is_crash(&eq));
    assert_eq!("worker-unavailable", sched.last_decision());
    assert_eq!(1, sched.queued());
    assert!(store.inner.lock().unwrap().failed.is_empty());
    assert_eq!(0, sched.attempts(&capture_id));
    sched.stop();
    sup.dispose();
}
