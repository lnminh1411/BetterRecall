// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{BinaryFrame, WorkerRequest};

use br_agent::worker::{CRASHED_ELSEWHERE_MESSAGE, WorkerClient, WorkerConnection, WorkerSupervisor};

use common::{FakeWorker, GatedDuplexStream, cancel_after, resp_ok, until};

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
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();
    let conn = sup.ensure(&ct.token()).unwrap();

    let c0 = client.clone();
    let tok0 = ct.token();
    let t = thread::spawn(move || c0.transcribe(&vec![0.0; 1600], "mic", "auto", &tok0));
    assert!(until(
        || running.load(Ordering::Acquire) >= 1,
        Duration::from_secs(10)
    ));

    let big = |mark: u8| {
        let mut b = vec![0u8; 2 << 20];
        b[0] = mark;
        b
    };
    let c1 = client.clone();
    let tok1 = ct.token();
    let i1 = thread::spawn(move || {
        c1.embed_image(&big(1), "image/webp", None, 1, None, &tok1)
    });
    assert!(until(
        || {
            let st = conn.write_gate_state();
            st.held && st.normal == 0 && st.urgent == 0
        },
        Duration::from_secs(20)
    ));
    let c2 = client.clone();
    let tok2 = ct.token();
    let i2 = thread::spawn(move || {
        c2.embed_image(&big(2), "image/webp", None, 1, None, &tok2)
    });
    assert!(until(
        || conn.write_gate_state().normal == 1,
        Duration::from_secs(20)
    ));
    let c3 = client.clone();
    let tok3 = ct.token();
    let q = thread::spawn(move || c3.embed_text("where is the invoice", "query", &tok3));
    assert!(until(
        || conn.write_gate_state().urgent == 1,
        Duration::from_secs(20)
    ));
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

// sweep 5W A3
#[test]
fn same_tick_urgent_preemption_blames_first_on_wire_in_worker_connection() {
    let stream = GatedDuplexStream::new();
    let conn = WorkerConnection::new(stream.clone(), Some(Arc::new(|| 1000)));
    let ct = cancel_after(Duration::from_secs(30));

    let c1 = conn.clone();
    let tok1 = ct.token();
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
            &tok1,
            false,
        )
    });
    assert!(stream.wait_first_write_entered(Duration::from_secs(10)));

    let c2 = conn.clone();
    let tok2 = ct.token();
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
            &tok2,
            false,
        )
    });
    assert!(until(
        || conn.write_gate_state().normal == 1,
        Duration::from_secs(10)
    ));

    let c3 = conn.clone();
    let tok3 = ct.token();
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
            &tok3,
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

// A3: queued WriteJob must complete with an error on dispose instead of hanging forever.
#[test]
fn a3_dispose_completes_queued_write_job_with_error() {
    let stream = GatedDuplexStream::new();
    let conn = WorkerConnection::new_without_writer_for_test(stream, Some(Arc::new(|| 1000)));

    let c1 = conn.clone();
    let handle = thread::spawn(move || {
        c1.send(
            WorkerRequest {
                id: 0,
                op: "transcribe".to_string(),
                ..Default::default()
            },
            &[1, 2, 3, 4],
            &br_core::CancelToken::none(),
            false,
        )
    });

    assert!(
        until(|| conn.queued_writes_for_test() == 1, Duration::from_secs(5)),
        "expected WriteJob to be queued"
    );

    conn.dispose();

    let err = handle
        .join()
        .expect("sender thread panicked")
        .expect_err("queued job must fail on dispose");
    assert!(
        matches!(err, br_agent::worker::WorkerError::Transport(_)),
        "expected Transport error, got {err:?}"
    );
}
