// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_core::{CancelSource, CancelToken};

use br_agent::native::{liveness_now_ms, tick_count64};
use br_agent::scheduling::EmbeddingScheduler;
use br_agent::worker::worker_supervisor::SupervisorOptions;
use br_agent::worker::{
    CRASHED_ELSEWHERE_MESSAGE, STOPPED_MESSAGE, WorkerClient, WorkerError, WorkerSupervisor,
};

use common::{FakeWorker, cancel_after, until};

#[test]
fn unresponsive_rule() {
    assert!(WorkerSupervisor::is_unresponsive("embed_image", 90_000, 90_000));
    assert!(!WorkerSupervisor::is_unresponsive("embed_text", 89_000, 90_000));
    assert!(WorkerSupervisor::is_unresponsive("transcribe", 90_000, 90_000));
    assert!(!WorkerSupervisor::is_unresponsive("bench", 3_600_000, 90_000));
    assert!(!WorkerSupervisor::is_unresponsive("hello", 3_600_000, 90_000));
}

#[test]
fn hung_worker_is_recycled_with_untimed_callers() {
    let now = Arc::new(AtomicI64::new(1_000));
    let n_cb = now.clone();
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
            liveness_clock: Some(Arc::new(move || n_cb.load(Ordering::Acquire))),
            start_timer: false,
            backoff_base_ms: 500,
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();

    let c2 = client.clone();
    let tok2 = ct.token();
    let job = thread::spawn(move || c2.embed_text("x", "document", &tok2));
    assert!(
        launcher.wait_seen("embed_text", Some("x"), Duration::from_secs(5)),
        "embed_text never reached worker"
    );
    now.fetch_add(100, Ordering::AcqRel);
    sup.check_now();
    assert!(sup.is_running());

    now.fetch_add(250, Ordering::AcqRel);
    sup.check_now();
    // Failed by the recycle, not by the test's 30 s safety cancel.
    let e = job.join().unwrap().expect_err("hung request must fail");
    assert!(!matches!(e, WorkerError::Cancelled), "{}", e.message());
    assert_eq!(1, sup.recycled());
    assert_eq!(1, launcher.kills());
    assert!(!sup.is_running());
    assert_eq!(1, sup.failures());
    assert!(client.embed_text("x", "query", &ct.token()).is_err());

    hung.store(false, Ordering::Release);
    let mut v = None;
    assert!(
        until(
            || {
                if let Ok(vec) = client.embed_text("abc", "query", &ct.token()) {
                    v = Some(vec);
                    true
                } else {
                    false
                }
            },
            Duration::from_secs(5),
        ),
        "worker did not restart after backoff"
    );
    assert_eq!(3.0, v.unwrap()[0]);
    assert_eq!(2, launcher.starts());
    assert_eq!(0, sup.failures());
    sup.dispose();
}

#[test]
fn hung_worker_is_recycled_after_caller_timeouts() {
    let now = Arc::new(AtomicI64::new(1_000));
    let n_cb = now.clone();
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|req| req.op == "embed_text");
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            liveness_clock: Some(Arc::new(move || n_cb.load(Ordering::Acquire))),
            start_timer: false,
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();

    let cs = Arc::new(CancelSource::new());
    let tok = cs.token();
    let c2 = client.clone();
    let call = thread::spawn(move || c2.embed_text("x", "query", &tok));
    assert!(launcher.wait_seen("embed_text", Some("x"), Duration::from_secs(5)));
    cs.cancel();
    let _ = call.join().unwrap();

    now.fetch_add(350, Ordering::AcqRel);
    sup.check_now();
    assert!(until(
        || sup.recycled() == 1 && !sup.is_running(),
        Duration::from_secs(5)
    ));
    sup.dispose();
}

// Real clock: the injected-clock rewrite changed what this checks (sweep 6 convergence).
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
fn keepalives_prevent_recycling_slow_transcribe_and_embed() {
    // Merges slow_transcribe_with_requests_queued_behind_it_is_not_a_hang and
    // long_embed_with_keepalives_is_not_recycled using the injected liveness clock.
    let now = Arc::new(AtomicI64::new(1_000));
    let n_cb = now.clone();
    let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
    let g_cb = gate.clone();
    let launcher = FakeWorker::new();
    launcher.set_handler(move |req, _| {
        if req.op == "transcribe" || req.op == "embed_image" {
            let (l, cv) = &*g_cb;
            let mut g = l.lock().unwrap();
            while *g == 0 {
                let (g2, _) = cv.wait_timeout(g, Duration::from_secs(5)).unwrap();
                g = g2;
            }
            *g -= 1;
        }
        None
    });
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            liveness_clock: Some(Arc::new(move || n_cb.load(Ordering::Acquire))),
            start_timer: false,
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();
    let conn = sup.ensure(&ct.token()).unwrap();

    // Case 1: slow transcribe with embed_text queued behind it.
    let c1 = client.clone();
    let tok1 = ct.token();
    let t = thread::spawn(move || c1.transcribe(&vec![0.0; 1600], "mic", "auto", &tok1));
    assert!(launcher.wait_seen("transcribe", None, Duration::from_secs(5)));
    let c2 = client.clone();
    let tok2 = ct.token();
    let e = thread::spawn(move || c2.embed_text("abc", "document", &tok2));
    assert!(until(|| conn.pending() == 2, Duration::from_secs(5)));

    for _ in 0..4 {
        now.fetch_add(200, Ordering::AcqRel);
        sup.check_now();
        assert!(sup.is_running());
        assert!(launcher.send_keepalive());
        assert!(
            until(
                || conn.oldest().is_some_and(|(_, ms)| ms == 0),
                Duration::from_secs(5)
            ),
            "keepalive did not reset liveness timer"
        );
    }
    {
        *gate.0.lock().unwrap() += 1;
        gate.1.notify_all();
    }
    assert_eq!("1600 samples", t.join().unwrap().unwrap().text);
    assert_eq!(3.0, e.join().unwrap().unwrap()[0]);
    assert_eq!(0, sup.recycled());

    // Case 2: long embed_image kept alive by keepalives.
    let c3 = client.clone();
    let tok3 = ct.token();
    let img = thread::spawn(move || {
        c3.embed_image(&[1, 2, 3], "image/webp", None, 1, None, &tok3)
    });
    assert!(launcher.wait_seen("embed_image", None, Duration::from_secs(5)));
    for _ in 0..4 {
        now.fetch_add(200, Ordering::AcqRel);
        sup.check_now();
        assert!(sup.is_running());
        assert!(launcher.send_keepalive());
        assert!(until(
            || conn.oldest().is_some_and(|(_, ms)| ms == 0),
            Duration::from_secs(5)
        ));
    }
    {
        *gate.0.lock().unwrap() += 1;
        gate.1.notify_all();
    }
    assert!(img.join().unwrap().is_ok());
    assert_eq!(0, sup.recycled());
    assert_eq!(1, launcher.starts());
    sup.dispose();
}

#[test]
fn abandoned_request_is_recycled_once_keepalives_stop() {
    let now = Arc::new(AtomicI64::new(1_000));
    let n_cb = now.clone();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let g_cb = gate.clone();
    let launcher = FakeWorker::new();
    launcher.set_handler(move |req, _| {
        if req.op == "embed_text" {
            let (l, cv) = &*g_cb;
            let mut g = l.lock().unwrap();
            while !*g {
                let (g2, _) = cv.wait_timeout(g, Duration::from_secs(5)).unwrap();
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
            unresponsive_after: Duration::from_millis(300),
            liveness_clock: Some(Arc::new(move || n_cb.load(Ordering::Acquire))),
            start_timer: false,
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();
    let conn = sup.ensure(&ct.token()).unwrap();

    let cs = Arc::new(CancelSource::new());
    let tok = cs.token();
    let c2 = client.clone();
    let call = thread::spawn(move || c2.embed_text("x", "query", &tok));
    assert!(launcher.wait_seen("embed_text", Some("x"), Duration::from_secs(5)));
    cs.cancel();
    let _ = call.join().unwrap();

    // While keepalives arrive, the abandoned request does not trigger recycle.
    now.fetch_add(200, Ordering::AcqRel);
    sup.check_now();
    assert!(sup.is_running());
    assert!(launcher.send_keepalive());
    assert!(until(
        || conn.oldest().is_some_and(|(_, ms)| ms == 0),
        Duration::from_secs(5)
    ));
    now.fetch_add(200, Ordering::AcqRel);
    sup.check_now();
    assert!(sup.is_running());

    // Once keepalives stop and silence exceeds unresponsive_after, recycle fires.
    now.fetch_add(350, Ordering::AcqRel);
    sup.check_now();
    assert!(until(|| sup.recycled() == 1, Duration::from_secs(5)));

    // Release the handler thread cleanly.
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    sup.dispose();
}

// Real clock: the injected-clock rewrite changed what this checks (sweep 6 convergence).
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
fn liveness_runs_on_the_injected_clock_and_blames_only_the_served_request() {
    let now = Arc::new(AtomicI64::new(1_000));
    let n_cb = now.clone();
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|r| r.op == "embed_text");
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            liveness_clock: Some(Arc::new(move || n_cb.load(Ordering::Acquire))),
            start_timer: false,
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();

    let c1 = client.clone();
    let tok1 = ct.token();
    let served = thread::spawn(move || c1.embed_text("served", "document", &tok1));
    assert!(
        launcher.wait_seen("embed_text", Some("served"), Duration::from_secs(5)),
        "served request never reached worker"
    );
    let c2 = client.clone();
    let tok2 = ct.token();
    let behind = thread::spawn(move || c2.embed_text("behind", "document", &tok2));
    assert!(
        launcher.wait_seen("embed_text", Some("behind"), Duration::from_secs(5)),
        "behind request never reached worker"
    );
    // Real time passing without advancing the injected clock must not recycle.
    thread::sleep(Duration::from_millis(350));
    sup.check_now();
    assert_eq!(0, sup.recycled());

    now.fetch_add(1_000, Ordering::AcqRel);
    sup.check_now();
    let e1 = served.join().unwrap().expect_err("served fails");
    let e2 = behind.join().unwrap().expect_err("behind fails");
    assert_eq!(1, sup.recycled());
    assert!(EmbeddingScheduler::is_crash(&e1));
    assert!(!EmbeddingScheduler::is_crash(&e2));
    assert_eq!(STOPPED_MESSAGE, e2.message());
    sup.dispose();
}

#[test]
fn default_liveness_clock_follows_the_tick_count_while_awake() {
    let t0 = liveness_now_ms();
    let k0 = tick_count64();
    thread::sleep(Duration::from_millis(300));
    let t1 = liveness_now_ms();
    let k1 = tick_count64();
    let dt = t1 - t0;
    let dk = k1 - k0;
    assert!((250..=5_000).contains(&dt), "dt={dt}");
    assert!((-250..=250).contains(&(dt - dk)), "dt-dk={}", dt - dk);
}

#[test]
fn liveness_gives_a_grace_after_the_agent_itself_was_frozen() {
    let live = Arc::new(AtomicI64::new(1_000));
    let check = Arc::new(AtomicI64::new(1_000));
    let jump = |ms: i64| {
        live.fetch_add(ms, Ordering::AcqRel);
        check.fetch_add(ms, Ordering::AcqRel);
    };
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|r| r.op == "embed_text");
    let l_cb = live.clone();
    let c_cb = check.clone();
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 3600,
        SupervisorOptions {
            unresponsive_after: Duration::from_secs(90),
            liveness_clock: Some(Arc::new(move || l_cb.load(Ordering::Acquire))),
            check_clock: Some(Arc::new(move || c_cb.load(Ordering::Acquire))),
            start_timer: false,
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();
    let conn = sup.ensure(&ct.token()).unwrap();
    let c1 = client.clone();
    let tok1 = ct.token();
    let owed = thread::spawn(move || c1.embed_text("frozen", "document", &tok1));
    assert!(
        until(|| conn.pending() == 1, Duration::from_secs(5)),
        "frozen request never became pending"
    );
    // pending() is set just before the send timestamp the grace rule uses; wait for the worker
    // to have the request too.
    assert!(launcher.wait_seen("embed_text", Some("frozen"), Duration::from_secs(5)));
    sup.check_now();
    jump(120_000);
    sup.check_now();
    assert_eq!(0, sup.recycled());
    jump(20_000);
    sup.check_now();
    jump(20_000);
    sup.check_now();
    assert_eq!(0, sup.recycled());
    jump(10_000);
    sup.check_now();
    // Failed by the recycle, not by the test's 30 s safety cancel.
    let e = owed.join().unwrap().expect_err("hung request must fail");
    assert!(!matches!(e, WorkerError::Cancelled), "{}", e.message());
    assert_eq!(1, sup.recycled());
    sup.dispose();
}

#[test]
fn self_crash_blames_only_the_served_request() {
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|r| r.text.as_deref() == Some("served"));
    launcher.set_crash_when(|r| r.text.as_deref() == Some("behind"));
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();

    let c1 = client.clone();
    let tok1 = ct.token();
    let served = thread::spawn(move || c1.embed_text("served", "document", &tok1));
    assert!(
        launcher.wait_seen("embed_text", Some("served"), Duration::from_secs(5)),
        "served request never reached worker"
    );
    let c2 = client.clone();
    let tok2 = ct.token();
    let behind = thread::spawn(move || c2.embed_text("behind", "document", &tok2));
    let e1 = served.join().unwrap().expect_err("served fails");
    let e2 = behind.join().unwrap().expect_err("behind fails");
    assert!(EmbeddingScheduler::is_crash(&e1));
    assert!(!EmbeddingScheduler::is_crash(&e2));
    assert_eq!(CRASHED_ELSEWHERE_MESSAGE, e2.message());
    sup.dispose();
}
