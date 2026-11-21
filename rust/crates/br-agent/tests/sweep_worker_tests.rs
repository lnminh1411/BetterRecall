// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{
    BenchResult, BinaryFrame, PipeNames, Settings, WorkerRequest, full_path,
};
use br_core::{CancelSource, CancelToken};

use br_agent::native::{liveness_now_ms, tick_count64};
use br_agent::scheduling::{EmbeddingScheduler, EmbeddingStore};
use br_agent::worker::worker_supervisor::SupervisorOptions;
use br_agent::worker::{
    CRASHED_ELSEWHERE_MESSAGE, ProcessWorkerLauncher, STOPPED_MESSAGE, WorkerClient,
    WorkerConnection, WorkerLauncher, WorkerSettingsDiff, WorkerSupervisor,
};

use common::{
    FakeStore, FakeWorker, GatedDuplexStream, RealStore, TempDir, busy_inputs, default_hello,
    idle_inputs, new_audio_capture, new_screen_capture, resolve_worker_exe, resp_err, resp_ok,
    until,
};

// ---------------- Sweep4WAgentTests ----------------

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

#[test]
fn hello_with_another_dim_is_refused() {
    let store = FakeStore::new();
    store.add_audio(new_audio_capture("hello world"));
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "hello" {
            let mut r = resp_ok(req.id);
            r.hello = Some(default_hello("r1", 4, "CPU"));
            Some((r, Vec::new()))
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
        Some(Arc::new(|| 768)),
    );

    sched.step(&CancelToken::none());
    let g = store.inner.lock().unwrap();
    assert!(g.space.is_none());
    assert!(g.vectors.is_empty());
    assert!(g.failed.is_empty());
    assert_eq!(1, sched.queued());
    assert_eq!("space-refused", sched.last_decision());
    assert!(sup.restart_pending());
    sched.stop();
    sup.dispose();
}

#[test]
fn worker_settings_argument_carries_the_agents_worker_keys_only() {
    let mut s = Settings::default();
    s.index.dim = 512;
    s.inference.device = "cpu".to_string();
    s.ocr.languages = "en,vi".to_string();
    let json = WorkerSettingsDiff::json(&s);
    let root: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(512, root["index"]["dim"].as_i64().unwrap());
    assert_eq!("cpu", root["inference"]["device"].as_str().unwrap());
    assert_eq!("en,vi", root["ocr"]["languages"].as_str().unwrap());
    assert!(root["governance"]["ecoQos"].is_boolean());
    assert!(root.get("ai").is_none());
    assert!(root.get("capture").is_none());
    assert!(root["privacy"].get("denyProcesses").is_none());

    let back: Settings = serde_json::from_str(&json).unwrap();
    assert_eq!(None, WorkerSettingsDiff::changed(&s, &back));
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
            Some((r, BinaryFrame::floats_to_bytes(&[payload.len() as f32, 1.0, 0.0, 0.0])))
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
    assert!(until(|| *entered.0.lock().unwrap() >= 1, Duration::from_secs(10)));
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
    assert!(until(|| *entered.0.lock().unwrap() >= 2, Duration::from_secs(10)));
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

    for round in 0..EmbeddingScheduler::MAX_CRASH_ATTEMPTS as usize {
        let cs = Arc::new(CancelSource::new());
        let tok = cs.token();
        let c2 = client.clone();
        let bench = thread::spawn(move || c2.bench(&tok));
        assert!(until(
            || bench_in.load(Ordering::Acquire) > round,
            Duration::from_secs(10)
        ));
        let sc2 = sched.clone();
        let step = thread::spawn(move || sc2.step(&CancelToken::none()));
        thread::sleep(Duration::from_millis(300));
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
    sched.step(&CancelToken::none());
    assert_eq!(1, store.inner.lock().unwrap().vectors.len());
    sched.stop();
    sup.dispose();
}

#[test]
fn liveness_runs_on_the_injected_clock_and_blames_only_the_served_request() {
    let now = Arc::new(AtomicI64::new(1_000));
    let n_cb = now.clone();
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|r| r.op == "embed_text");
    let sup = WorkerSupervisor::with_options(
        launcher,
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            unresponsive_after: Duration::from_millis(300),
            liveness_clock: Some(Arc::new(move || n_cb.load(Ordering::Acquire))),
            ..Default::default()
        },
    );
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.prewarm(&CancelToken::none()).unwrap();

    let c1 = client.clone();
    let served = thread::spawn(move || c1.embed_text("served", "document", &CancelToken::none()));
    thread::sleep(Duration::from_millis(50));
    let c2 = client.clone();
    let behind = thread::spawn(move || c2.embed_text("behind", "document", &CancelToken::none()));
    thread::sleep(Duration::from_millis(700));
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
    let dt = liveness_now_ms() - t0;
    let dk = tick_count64() - k0;
    assert!((250..=5_000).contains(&dt), "dt={dt}");
    assert!((-100..=100).contains(&(dt - dk)), "dt-dk={}", dt - dk);
}

#[test]
fn query_embed_is_written_before_waiting_background_requests() {
    let launcher = FakeWorker::new();
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    let running = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let o_cb = order.clone();
    let r_cb = running.clone();
    let g_cb = gate.clone();
    launcher.set_handler(move |req, payload| {
        if req.op == "hello" {
            return None;
        }
        let label = if req.op == "embed_image" {
            format!("image{}", payload[0])
        } else {
            format!("{}:{}", req.op, req.kind.as_deref().unwrap_or(""))
        };
        o_cb.lock().unwrap().push(label);
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
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.prewarm(&CancelToken::none()).unwrap();
    let conn = sup.ensure(&CancelToken::none()).unwrap();

    let c0 = client.clone();
    let t = thread::spawn(move || c0.transcribe(&vec![0.0; 1600], "mic", "auto", &CancelToken::none()));
    assert!(until(|| running.load(Ordering::Acquire) >= 1, Duration::from_secs(10)));

    let big = |mark: u8| {
        let mut b = vec![0u8; 2 << 20];
        b[0] = mark;
        b
    };
    let c1 = client.clone();
    let i1 = thread::spawn(move || {
        c1.embed_image(&big(1), "image/webp", None, 1, None, &CancelToken::none())
    });
    assert!(until(
        || {
            let st = conn.write_gate_state();
            st.held && st.normal == 0 && st.urgent == 0
        },
        Duration::from_secs(20)
    ));
    let c2 = client.clone();
    let i2 = thread::spawn(move || {
        c2.embed_image(&big(2), "image/webp", None, 1, None, &CancelToken::none())
    });
    assert!(until(|| conn.write_gate_state().normal == 1, Duration::from_secs(20)));
    let c3 = client.clone();
    let q = thread::spawn(move || {
        c3.embed_text("where is the invoice", "query", &CancelToken::none())
    });
    assert!(until(|| conn.write_gate_state().urgent == 1, Duration::from_secs(20)));
    {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }
    t.join().unwrap().unwrap();
    i1.join().unwrap().unwrap();
    i2.join().unwrap().unwrap();
    q.join().unwrap().unwrap();
    assert_eq!(
        vec!["transcribe:", "image1", "embed_text:query", "image2"],
        *order.lock().unwrap()
    );
    sup.dispose();
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
        launcher,
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
    client.prewarm(&CancelToken::none()).unwrap();
    let c1 = client.clone();
    let owed = thread::spawn(move || c1.embed_text("frozen", "document", &CancelToken::none()));
    thread::sleep(Duration::from_millis(50));
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
    assert!(owed.join().unwrap().is_err());
    assert_eq!(1, sup.recycled());
    sup.dispose();
}

#[test]
fn self_crash_blames_only_the_served_request() {
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|r| r.text.as_deref() == Some("served"));
    launcher.set_crash_when(|r| r.text.as_deref() == Some("behind"));
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    client.prewarm(&CancelToken::none()).unwrap();

    let c1 = client.clone();
    let served = thread::spawn(move || c1.embed_text("served", "document", &CancelToken::none()));
    thread::sleep(Duration::from_millis(50));
    let c2 = client.clone();
    let behind = thread::spawn(move || c2.embed_text("behind", "document", &CancelToken::none()));
    let e1 = served.join().unwrap().expect_err("served fails");
    let e2 = behind.join().unwrap().expect_err("behind fails");
    assert!(EmbeddingScheduler::is_crash(&e1));
    assert!(!EmbeddingScheduler::is_crash(&e2));
    assert_eq!(CRASHED_ELSEWHERE_MESSAGE, e2.message());
    sup.dispose();
}

#[test]
fn worker_settings_keys_are_pinned() {
    assert_eq!(
        &[
            "inference.device",
            "inference.gpuAdapter",
            "inference.modelDir",
            "inference.imageTokens",
            "inference.idleExitS",
            "inference.fake",
            "index.dim",
            "audio.language",
            "governance.ecoQos",
            "security.memoryIsolation",
            "log.level",
            "privacy.nsfwFilter",
            "privacy.nsfwDetector",
            "privacy.nsfwDevice",
            "privacy.nsfwGpuAdapter",
            "ocr.enabled",
            "ocr.engine",
            "ocr.languages",
            "ocr.device",
        ],
        WorkerSettingsDiff::KEY_NAMES
    );
}

#[test]
fn agent_launcher_hands_its_settings_to_the_real_worker() {
    let Some(worker_exe) = resolve_worker_exe() else {
        // When run via cargo test without BR_WORKER_DIR and without br-worker built yet, skip cleanly.
        return;
    };
    let dir = TempDir::new("s4w-real");
    let mut s = Settings::default();
    s.inference.fake = true;
    s.index.dim = 256;
    let model_dir = dir
        .path()
        .join("Models dir")
        .join("with \"quote\" \u{00e9}t\u{00e9}");
    s.inference.model_dir = format!("{}\\", model_dir.display());
    let home = dir.path().join("home dir");
    let s_clone = s.clone();
    let launcher = ProcessWorkerLauncher::new(
        worker_exe,
        || false,
        Some(Arc::new(move || WorkerSettingsDiff::json(&s_clone))),
    );
    let pid = launcher.start(&PipeNames::new_worker(), &home).unwrap();
    let mut log = String::new();
    let logs_dir = home.join("logs");
    let ok = until(
        || {
            if let Ok(entries) = fs::read_dir(&logs_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("worker-")
                        && name.ends_with(".log")
                        && let Ok(txt) = fs::read_to_string(entry.path())
                        && txt.contains("start settings=")
                    {
                        log = txt;
                        return true;
                    }
                }
            }
            false
        },
        Duration::from_secs(30),
    );
    launcher.kill(pid);
    assert!(ok, "worker log never contained start settings=");
    assert!(log.contains("start settings=agent "), "log={log}");
    let expected_model = full_path(std::path::Path::new(&s.inference.model_dir))
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        log.contains(&format!("modelDir={expected_model} ")),
        "log={log}"
    );
    assert!(log.contains(" dim=256 "), "log={log}");
    assert!(log.contains(" fake=True "), "log={log}");
    let info = launcher.last_start_info().unwrap();
    assert!(info.args.contains(&"--settings-stdin".to_string()));
    assert!(
        !info
            .args
            .iter()
            .any(|a| a.contains("Models dir") || a.contains('{'))
    );
}

#[test]
fn clean_close_disposes_the_connection() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let conn = sup.ensure(&CancelToken::none()).unwrap();
    launcher.kill(0);
    assert!(until(|| conn.is_disposed(), Duration::from_secs(10)));
    assert!(conn.is_closed());
    assert!(conn.is_disposed());
    assert_eq!(0, sup.failures());
    let v = WorkerClient::new(sup.clone())
        .embed_text("again", "query", &CancelToken::none())
        .unwrap();
    assert_eq!(vec![5.0, 0.0, 0.0, 1.0], v);
    assert_eq!(2, launcher.starts());
    sup.dispose();
}

// ---------------- Sweep5WAgentTests ----------------

#[test]
fn c5_signal_burst_does_not_bypass_model_missing_backoff() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let embed_calls = Arc::new(AtomicUsize::new(0));
    let ec = embed_calls.clone();
    let launcher = FakeWorker::new();
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
    for _ in 0..20 {
        sched.signal();
    }
    sched.start();
    assert!(until(
        || embed_calls.load(Ordering::Acquire) >= 1 && sched.last_decision() == "model-missing",
        Duration::from_secs(10)
    ));
    for _ in 0..20 {
        sched.signal();
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(1, embed_calls.load(Ordering::Acquire));
    assert_eq!("model-missing", sched.last_decision());
    sched.stop();
    sup.dispose();
}

#[test]
fn c5_signal_burst_does_not_bypass_governance_defer() {
    let store = FakeStore::new();
    store.add_screen(new_screen_capture(vec![1, 2, 3]));
    let probe_calls = Arc::new(AtomicUsize::new(0));
    let pc = probe_calls.clone();
    let sup = WorkerSupervisor::new(FakeWorker::new(), PathBuf::from("x"), || 120);
    let st_cb = store.clone();
    let sched = EmbeddingScheduler::new(
        Arc::new(move || Some(st_cb.clone() as Arc<dyn EmbeddingStore>)),
        Arc::new(Settings::default),
        Arc::new(WorkerClient::new(sup.clone())),
        Arc::new(move || {
            pc.fetch_add(1, Ordering::AcqRel);
            busy_inputs()
        }),
        Arc::new(|| false),
        None,
    );
    for _ in 0..20 {
        sched.signal();
    }
    sched.start();
    assert!(until(
        || probe_calls.load(Ordering::Acquire) >= 1 && sched.last_decision() == "cpu",
        Duration::from_secs(10)
    ));
    for _ in 0..20 {
        sched.signal();
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(1, probe_calls.load(Ordering::Acquire));
    assert_eq!("cpu", sched.last_decision());
    sched.stop();
    sup.dispose();
}

#[test]
fn a6_step_async_with_null_space_never_calls_worker_embed_ops() {
    let store = FakeStore::new();
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
    assert_eq!("space-refused", sched.last_decision());
    assert_eq!(0, embed_calls.load(Ordering::Acquire));
    assert!(sup.restart_pending());
    sched.stop();
    sup.dispose();
}

#[test]
fn c5_a6_signal_burst_does_not_bypass_space_refused_and_accepted_hello_wakes_it() {
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

#[test]
fn c5_signal_still_wakes_empty_scheduler_once() {
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

#[test]
fn a3_same_tick_urgent_preemption_blames_first_on_wire_in_worker_connection() {
    let stream = GatedDuplexStream::new();
    let conn = WorkerConnection::new(stream.clone(), Some(Arc::new(|| 1000)));

    let c1 = conn.clone();
    let r1 = thread::spawn(move || {
        c1.send(
            WorkerRequest {
                id: 0,
                op: "embed_text".to_string(),
                text: Some("one".to_string()),
                kind: Some("document".to_string()),
                ..Default::default()
            },
            &[],
            &CancelToken::none(),
            false,
        )
    });
    assert!(stream.wait_first_write_entered(Duration::from_secs(10)));

    let c2 = conn.clone();
    let r2 = thread::spawn(move || {
        c2.send(
            WorkerRequest {
                id: 0,
                op: "embed_text".to_string(),
                text: Some("two".to_string()),
                kind: Some("document".to_string()),
                ..Default::default()
            },
            &[],
            &CancelToken::none(),
            false,
        )
    });
    assert!(until(
        || conn.write_gate_state().normal == 1,
        Duration::from_secs(10)
    ));

    let c3 = conn.clone();
    let r3 = thread::spawn(move || {
        c3.send(
            WorkerRequest {
                id: 0,
                op: "embed_text".to_string(),
                text: Some("three".to_string()),
                kind: Some("query".to_string()),
                ..Default::default()
            },
            &[],
            &CancelToken::none(),
            true,
        )
    });
    assert!(until(
        || conn.write_gate_state().urgent == 1,
        Duration::from_secs(10)
    ));

    stream.release_first_write();
    assert!(until(
        || stream.writes() == 3 && !conn.write_gate_state().held,
        Duration::from_secs(10)
    ));
    assert_eq!(vec![1, 3, 2], stream.written_ids());

    let mut reply1 = resp_ok(1);
    reply1.dim = Some(4);
    reply1.ms = Some(1.0);
    stream.push_reply(&reply1, &BinaryFrame::floats_to_bytes(&[1.0, 0.0, 0.0, 0.0]));
    r1.join().unwrap().unwrap();

    stream.close_inbound();
    let e3 = r3.join().unwrap().expect_err("r3 fails");
    let e2 = r2.join().unwrap().expect_err("r2 fails");
    assert_eq!("Worker pipe closed.", e3.message());
    assert_eq!(CRASHED_ELSEWHERE_MESSAGE, e2.message());
    conn.dispose();
}

#[test]
fn a3_seam_embedding_scheduler_behind_urgent_query_does_not_take_crash_strike() {
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
    client.prewarm(&CancelToken::none()).unwrap();
    let conn = sup.ensure(&CancelToken::none()).unwrap();
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
    let t = thread::spawn(move || c0.transcribe(&vec![0.0; 1600], "mic", "auto", &CancelToken::none()));
    assert!(until(
        || running.load(Ordering::Acquire) >= 1,
        Duration::from_secs(10)
    ));

    let mut big = vec![0u8; 2 << 20];
    big[0] = 1;
    let c1 = client.clone();
    let i1 = thread::spawn(move || {
        c1.embed_image(&big, "image/webp", None, 1, None, &CancelToken::none())
    });
    assert!(until(
        || {
            let st = conn.write_gate_state();
            st.held && st.normal == 0 && st.urgent == 0
        },
        Duration::from_secs(20)
    ));

    let sc = sched.clone();
    let step = thread::spawn(move || sc.step(&CancelToken::none()));
    assert!(until(
        || conn.write_gate_state().normal == 1,
        Duration::from_secs(20)
    ));

    let c2 = client.clone();
    let q = thread::spawn(move || {
        c2.embed_text("where is invoice", "query", &CancelToken::none())
    });
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
