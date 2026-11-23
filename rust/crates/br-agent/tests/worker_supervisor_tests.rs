// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use br_contracts::{PipeNames, Settings, full_path};
use br_core::CancelSource;
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;
use windows::core::PWSTR;

use br_agent::native::tick_count64;
use br_agent::worker::worker_supervisor::{SupervisorOptions, create_worker_pipe};
use br_agent::worker::{
    KillOnCloseJob, ProcessWorkerLauncher, VcRuntime, WorkerClient, WorkerError, WorkerLauncher,
    WorkerSettingsDiff, WorkerSupervisor,
};

use common::{
    FakeWorker, PidWorkerLauncher, SlowWorkerLauncher, TempDir, WrongPidLauncher, cancel_after,
    resolve_worker_exe, resp_err, until,
};

#[test]
fn round_trip_over_real_pipe_with_pid_check() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from(r"Z:\no-such-dir"), || 120);
    let client = WorkerClient::new(sup.clone());
    let ct = cancel_after(Duration::from_secs(30));
    let v = client
        .embed_text("hello", "query", &ct.token())
        .unwrap();
    assert_eq!(vec![5.0, 0.0, 0.0, 1.0], v);
    assert_eq!("fake-model", sup.hello().unwrap().model);
    let pipe = launcher.last_pipe().unwrap();
    assert!(pipe.starts_with("BetterRecall.worker."));
    assert_eq!(32, pipe.rsplit('.').next().unwrap().len());

    let img = client
        .embed_image(&vec![0u8; 1000], "image/webp", Some(0.8), 3, Some(0.5), &ct.token())
        .unwrap();
    assert!(!img.dropped_nsfw);
    assert_eq!(1000.0, img.vector.unwrap()[0]);

    let nsfw = client
        .embed_image(&[0xEE, 1, 2], "image/webp", Some(0.8), 3, Some(0.5), &ct.token())
        .unwrap();
    assert!(nsfw.dropped_nsfw);
    assert!(nsfw.vector.is_none());

    let t = client
        .transcribe(&vec![0.0f32; 1600], "mic", "auto", &ct.token())
        .unwrap();
    assert_eq!("1600 samples", t.text);
    assert_eq!(1, launcher.starts());
    sup.dispose();

    // Wrong-PID client connecting to the pipe must be rejected and killed (CC P9).
    let wrong = WrongPidLauncher::new();
    let bad_sup = WorkerSupervisor::with_options(
        wrong.clone(),
        PathBuf::from(r"Z:\no-such-dir"),
        || 120,
        SupervisorOptions {
            verify_client_pid: true,
            backoff_base_ms: 50,
            ..Default::default()
        },
    );
    let Err(err) = bad_sup.ensure(&ct.token()) else {
        panic!("wrong-PID client must be rejected");
    };
    assert_eq!("Worker pipe client is not the started process.", err.message());
    assert_eq!(1, bad_sup.failures());
    assert_eq!(vec![wrong.reported_pid], *wrong.killed.lock().unwrap());
    assert!(!bad_sup.is_running());
    bad_sup.dispose();
}

#[test]
fn concurrent_requests_match_by_id() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    let handles: Vec<_> = (1..=50)
        .map(|i| {
            let c = client.clone();
            let tok = ct.token();
            thread::spawn(move || {
                let text = "a".repeat(i);
                c.embed_text(&text, "document", &tok).unwrap()
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
    let sup = WorkerSupervisor::with_options(
        launcher.clone(),
        PathBuf::from("x"),
        || 120,
        SupervisorOptions {
            backoff_base_ms: 500,
            ..Default::default()
        },
    );
    let client = WorkerClient::new(sup.clone());
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();
    launcher.state.crash_on_next.store(true, Ordering::Release);
    let e = client
        .embed_text("x", "query", &ct.token())
        .expect_err("crash must fail the request");
    assert!(!matches!(e, WorkerError::Cancelled), "{}", e.message());
    assert_eq!(1, sup.failures());
    // Inside backoff window: an instant retry must not start a new worker (sweep 6 F1).
    let e = client
        .embed_text("x", "query", &ct.token())
        .expect_err("retry inside backoff");
    assert!(e.message().contains("backoff"), "{}", e.message());
    assert_eq!(1, launcher.starts());

    let mut restarted = None;
    assert!(
        until(
            || {
                if let Ok(v) = client.embed_text("abc", "query", &ct.token()) {
                    restarted = Some(v);
                    true
                } else {
                    false
                }
            },
            Duration::from_secs(5),
        ),
        "worker did not restart after backoff"
    );
    assert_eq!(3.0, restarted.unwrap()[0]);
    assert_eq!(2, launcher.starts());
    assert_eq!(0, sup.failures());
    sup.dispose();
}

#[test]
fn worker_error_surfaces_message() {
    let launcher = FakeWorker::new();
    launcher.set_handler(|req, _| {
        if req.op == "bench" {
            Some((
                resp_err(req.id, "Model not installed: embeddinggemma2."),
                Vec::new(),
            ))
        } else {
            None
        }
    });
    let sup = WorkerSupervisor::new(launcher, PathBuf::from("x"), || 120);
    let ct = cancel_after(Duration::from_secs(30));
    let err = WorkerClient::new(sup.clone())
        .bench(&ct.token())
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
            thread::sleep(Duration::from_millis(1000));
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
    let ct = cancel_after(Duration::from_secs(30));
    let v = client.embed_text("abcd", "query", &ct.token()).unwrap();
    assert_eq!(4.0, v[0]);
    assert_eq!(1, launcher.starts());
    assert_eq!(0, launcher.kills());
    assert_eq!(0, sup.failures());
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
    let ct = cancel_after(Duration::from_secs(30));
    client.embed_text("a", "query", &ct.token()).unwrap();
    let first = sup.pid();
    let sup_bg = sup.clone();
    let shut = thread::spawn(move || sup_bg.shutdown());
    assert!(
        until(|| !sup.is_running(), Duration::from_secs(5)),
        "shutdown did not take the first connection"
    );
    client.embed_text("b", "query", &ct.token()).unwrap();
    let second = sup.pid();
    shut.join().unwrap();
    assert_ne!(first, second);
    assert!(!l.killed.lock().unwrap().contains(&second));
    assert!(l.alive.lock().unwrap().contains_key(&second));
    assert!(sup.is_running());
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
    assert!(VcRuntime::probe(
        Some(&worker),
        &sys,
        Some(&["br-app-local-140.dll"])
    ));
    assert!(!VcRuntime::probe(
        Some(&worker),
        &sys,
        Some(&["br-on-path-140.dll"])
    ));
    assert!(!VcRuntime::probe(
        Some(&worker),
        &sys,
        Some(&["kernel32.dll", "br-no-such-runtime-140.dll"])
    ));
    assert!(!VcRuntime::probe(
        None,
        &sys,
        Some(&["br-app-local-140.dll"])
    ));
}

#[test]
fn pipe_security_denies_network_and_is_protected() {
    let sid = PipeNames::user_sid().unwrap();
    let sddl = WorkerSupervisor::pipe_sddl(&sid);
    assert!(sddl.contains("D:P"));
    assert!(sddl.contains("(D;"));
    assert!(sddl.contains(";NU)"));

    // Verify the actual kernel DACL on a pipe created by `create_worker_pipe`.
    let pipe = create_worker_pipe(&PipeNames::new_worker()).expect("create_worker_pipe");
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let err = unsafe {
        GetSecurityInfo(
            pipe.raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    assert!(err.is_ok(), "GetSecurityInfo failed: {err:?}");
    let mut sddl_ptr = PWSTR::null();
    let conv = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl_ptr,
            None,
        )
    };
    let actual_dacl = if conv.is_ok() {
        unsafe { sddl_ptr.to_string().unwrap_or_default() }
    } else {
        String::new()
    };
    unsafe {
        if !sddl_ptr.is_null() {
            let _ = LocalFree(Some(HLOCAL(sddl_ptr.0.cast())));
        }
        if !sd.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
    }
    pipe.close();
    assert!(conv.is_ok(), "ConvertSecurityDescriptorToStringSecurityDescriptorW failed");
    assert!(actual_dacl.contains("D:P"), "actual DACL not protected: {actual_dacl}");
    assert!(actual_dacl.contains("(D;") && actual_dacl.contains(";NU)"), "actual DACL does not deny NETWORK: {actual_dacl}");
    assert!(actual_dacl.contains(&format!(";{sid})")), "actual DACL does not allow current user {sid}: {actual_dacl}");
}

#[test]
fn restart_request_shuts_the_idle_worker_down_and_the_next_request_starts_a_new_one() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let client = WorkerClient::new(sup.clone());
    let ct = cancel_after(Duration::from_secs(30));
    client.embed_text("a", "query", &ct.token()).unwrap();
    assert_eq!(1, launcher.starts());
    sup.request_restart("ocr.languages");
    assert!(until(|| !sup.is_running(), Duration::from_secs(10)));
    assert!(!sup.restart_pending());
    client.embed_text("b", "query", &ct.token()).unwrap();
    assert_eq!(2, launcher.starts());
    assert_eq!(0, sup.failures());
    sup.dispose();
}

#[test]
fn restart_waits_for_the_request_in_flight() {
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|req| req.op == "embed_text" && req.text.as_deref() == Some("slow"));
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let client = Arc::new(WorkerClient::new(sup.clone()));
    let ct = cancel_after(Duration::from_secs(30));
    client.embed_text("a", "query", &ct.token()).unwrap();
    let conn = sup.ensure(&ct.token()).unwrap();

    let cs = Arc::new(CancelSource::new());
    let tok = cs.token();
    let c2 = client.clone();
    let slow = thread::spawn(move || c2.embed_text("slow", "query", &tok));
    assert!(
        until(|| conn.pending() == 1, Duration::from_secs(5)),
        "slow request never became pending"
    );
    sup.request_restart("inference.device");
    assert!(sup.restart_pending());
    assert!(sup.is_running());
    cs.cancel();
    assert!(slow.join().unwrap().is_err());
    sup.check_now();
    assert!(until(
        || !sup.is_running() && !sup.restart_pending(),
        Duration::from_secs(10)
    ));
    sup.dispose();
}

// Sweep 2G
#[test]
fn cancelled_bench_stops_the_worker_without_a_backoff() {
    let launcher = FakeWorker::new();
    launcher.set_no_reply(|req| req.op == "bench");
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from(r"Z:\no-such-dir"), || 120);
    let client = WorkerClient::new(sup.clone());
    let ct = cancel_after(Duration::from_secs(30));
    assert_eq!(
        4,
        client
            .embed_text("warm", "query", &ct.token())
            .unwrap()
            .len()
    );
    let cs = cancel_after(Duration::from_millis(300));
    let err = client.bench(&cs.token()).expect_err("expected cancel");
    assert!(matches!(err, WorkerError::Cancelled));
    assert_eq!(1, launcher.kills());
    assert_eq!(0, sup.failures());

    let v = client.embed_text("after", "query", &ct.token()).unwrap();
    assert_eq!(5.0, v[0]);
    assert_eq!(2, launcher.starts());
    sup.dispose();
}

// Sweep 3I A2
#[test]
fn ocr_timeout_while_its_write_waits_keeps_the_worker_and_the_transcribe() {
    let l = SlowWorkerLauncher::new(1200, false);
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
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();
    let pid = sup.pid();

    let c1 = client.clone();
    let tok1 = ct.token();
    let tr = thread::spawn(move || c1.transcribe(&vec![0.0; 16000], "mic", "auto", &tok1));
    assert!(
        until(
            || l.transcribe_seen.load(Ordering::Acquire) >= 1,
            Duration::from_secs(5)
        ),
        "transcribe never reached worker"
    );
    let cs = cancel_after(Duration::from_millis(300));
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
        .ocr(&vec![0u8; 64 * 64 * 4], 64, 64, 256, false, &ct.token())
        .unwrap();
    assert_eq!("fake", ocr.engine);
    assert_eq!(1, l.started.lock().unwrap().len());
    sup.dispose();
}

// Sweep 3I A2
#[test]
fn broken_frame_kills_that_worker_process() {
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
    let ct = cancel_after(Duration::from_secs(30));
    client.prewarm(&ct.token()).unwrap();
    let pid = sup.pid();
    assert!(
        client
            .ocr(&[0u8; 16], 2, 2, 8, false, &ct.token())
            .is_err()
    );
    assert!(until(
        || !l.killed.lock().unwrap().is_empty(),
        Duration::from_secs(5)
    ));
    assert_eq!(vec![pid], *l.killed.lock().unwrap());
    assert!(!sup.is_running());
    assert_eq!(1, sup.failures());
    sup.dispose();
}

// Sweep 3I A2
#[test]
fn job_limit_struct_has_the_native_size() {
    let expected = if cfg!(target_pointer_width = "64") {
        144
    } else {
        112
    };
    assert_eq!(expected, KillOnCloseJob::extended_limit_info_size());
}

// Sweep 3I A2
#[test]
fn closing_the_job_kills_its_processes() {
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

#[test]
fn clean_close_disposes_the_connection() {
    let launcher = FakeWorker::new();
    let sup = WorkerSupervisor::new(launcher.clone(), PathBuf::from("x"), || 120);
    let ct = cancel_after(Duration::from_secs(30));
    let conn = sup.ensure(&ct.token()).unwrap();
    launcher.kill(0);
    assert!(until(|| conn.is_disposed(), Duration::from_secs(10)));
    assert!(conn.is_closed());
    assert!(conn.is_disposed());
    assert_eq!(0, sup.failures());
    let v = WorkerClient::new(sup.clone())
        .embed_text("again", "query", &ct.token())
        .unwrap();
    assert_eq!(vec![5.0, 0.0, 0.0, 1.0], v);
    assert_eq!(2, launcher.starts());
    sup.dispose();
}

#[test]
fn agent_launcher_hands_its_settings_to_the_real_worker() {
    // Runs whenever br-worker.exe is built next to the test (a workspace run) or BR_WORKER_DIR
    // names it; otherwise says so instead of failing.
    let Some(worker_exe) = resolve_worker_exe() else {
        eprintln!("SKIPPED agent_launcher_hands_its_settings_to_the_real_worker: br-worker.exe not built");
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
