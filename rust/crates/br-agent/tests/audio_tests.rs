// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported audio tests: AudioTimelineTests and audio/transcript/loopback/WASAPI cases from
//! Sweep3J, Sweep4P, Sweep4R, Sweep5P, and ReliabilitySweep2.

mod common;

use std::fs;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_agent::audio::{
    AudioGuardTimeline, AudioReplayEncoder, AudioStore, AudioTimeline, CaptureStream, LoopbackHub,
    Pcm, PcmHandler, SpeechSegment, SpeechSegmenter, TimelineStep, TranscribeBackend, Transcriber,
    TranscriptSource, WasapiCapture,
};
use br_agent::capture::{CaptureService, OutputCapture};
use br_agent::guardrails::{
    AudioGate, AudioVerdict, BadgeResult, GuardDecision, GuardReason, GuardSnapshot,
    GuardrailService, WindowInfo,
};
use br_agent::host::{AgentHost, AgentOptions};
use br_agent::native::{WTS_SESSION_LOCK, qpc, tick_count64};
use br_agent::privacy::RedactionOptions;
use br_agent::worker::WorkerError;
use br_contracts::{
    CoreHostOptions, DataPaths, PrivacySettings, Settings, TranscribeResult,
};
use br_core::{AgentControl, CancelSource, CancelToken, CoreHost};
use chrono::{Duration as ChronoDuration, Utc};

use common::{FakeStore, TempDir, until};

// =========================================================================
// AudioTimelineTests
// =========================================================================

const RATE: i32 = 48_000;
const PACKET: i32 = 480;
const PACKET_TIME: i64 = 100_000;

fn run_step(tl: &mut AudioTimeline, qpc: i64, silent: bool, frames: i32) -> TimelineStep {
    let s = tl.plan(qpc, frames, silent);
    tl.commit(s, qpc);
    s
}

#[test]
fn quiet_packets_inside_run_keep_the_clock() {
    let mut tl = AudioTimeline::new(RATE);
    let t = 1_000_000_000i64;
    for i in 0..300i64 {
        run_step(&mut tl, t + i * PACKET_TIME, i % 3 == 1, PACKET);
    }
    assert_eq!(t + 300 * PACKET_TIME, tl.expected());
    assert_eq!(1, tl.runs());
}

#[test]
fn missing_packets_are_filled_not_collapsed() {
    let mut tl = AudioTimeline::new(RATE);
    let t = 5_000_000i64;
    run_step(&mut tl, t, false, PACKET);
    let s = run_step(&mut tl, t + 5 * PACKET_TIME, false, PACKET);
    assert_eq!(4 * PACKET as i64, s.fill_frames);
    assert_eq!(t + 6 * PACKET_TIME, tl.expected());
}

#[test]
fn jitter_under_two_milliseconds_is_continuous() {
    let mut tl = AudioTimeline::new(RATE);
    run_step(&mut tl, 0, false, PACKET);
    let s = run_step(&mut tl, PACKET_TIME + 15_000, false, PACKET);
    assert_eq!(0, s.fill_frames);
    assert_eq!(0, s.skip_frames);
    assert_eq!(2 * PACKET_TIME, tl.expected());
}

#[test]
fn overlap_from_drift_is_trimmed() {
    let mut tl = AudioTimeline::new(RATE);
    run_step(&mut tl, 0, false, PACKET);
    let s = run_step(&mut tl, PACKET_TIME - 50_000, false, PACKET);
    assert_eq!(240, s.skip_frames);
    assert_eq!(240, s.feed_frames);
    assert_eq!(PACKET_TIME + 50_000, tl.expected());
}

#[test]
fn long_silence_ends_run_and_sound_starts_a_new_one_at_its_own_time() {
    let mut tl = AudioTimeline::new(RATE);
    let t = 0i64;
    run_step(&mut tl, t, false, PACKET);
    let mut i = 1i64;
    while i <= 250 {
        let s = run_step(&mut tl, t + i * PACKET_TIME, true, PACKET);
        if s.end_run {
            break;
        }
        i += 1;
    }
    assert!(!tl.in_run());
    assert!((200..=202).contains(&i));
    assert_eq!(0, tl.plan(t + 300 * PACKET_TIME, PACKET, true).feed_frames);
    let resume = t + 900 * PACKET_TIME;
    let start = run_step(&mut tl, resume, false, PACKET);
    assert!(start.start_run);
    assert_eq!(resume, tl.run_start());
    assert_eq!(2, tl.runs());
}

#[test]
fn clock_jump_starts_new_run() {
    let mut tl = AudioTimeline::new(RATE);
    run_step(&mut tl, 0, false, PACKET);
    let s = run_step(&mut tl, 50_000_000, false, PACKET);
    assert!(s.end_run);
    assert!(s.start_run);
    assert_eq!(50_000_000, tl.run_start());
}

#[test]
fn leading_silence_is_not_encoded() {
    let mut tl = AudioTimeline::new(RATE);
    let s = run_step(&mut tl, 0, true, PACKET);
    assert!(!tl.in_run());
    assert_eq!(0, s.feed_frames);
}

// =========================================================================
// Helpers for fake loopback, speaker, and fake transcriber backend
// =========================================================================

struct FakeStream {
    ended: AtomicBool,
    disposed: AtomicBool,
    handler: Mutex<Option<PcmHandler>>,
}

impl FakeStream {
    fn new(h: Option<PcmHandler>) -> Arc<Self> {
        Arc::new(Self {
            ended: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            handler: Mutex::new(h),
        })
    }
    fn push(&self) {
        if let Some(h) = self.handler.lock().unwrap().clone() {
            h(&[0, 0, 0, 0], 1, 0, false);
        }
    }
}

impl CaptureStream for FakeStream {
    fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
    fn stop(&self) {
        self.disposed.store(true, Ordering::Release);
    }
}

struct Speaker {
    feed: Arc<Mutex<Option<PcmHandler>>>,
    qpc_pos: i64,
    hub: Arc<LoopbackHub>,
}

impl Speaker {
    fn new() -> Self {
        let feed: Arc<Mutex<Option<PcmHandler>>> = Arc::new(Mutex::new(None));
        let f_cb = feed.clone();
        let hub = Arc::new(LoopbackHub::with_opener(Arc::new(move |h| {
            *f_cb.lock().unwrap() = Some(h);
            Ok(FakeStream::new(None))
        })));
        Self {
            feed,
            qpc_pos: OutputCapture::qpc_100ns(qpc()) - 600_000_000,
            hub,
        }
    }

    fn play(&mut self, tone: bool, ms: i32) {
        let mut buf = vec![0u8; 480 * 4];
        for p in 0..(ms / 10) {
            if tone {
                for f in 0..480 {
                    let angle =
                        ((p * 480 + f) as f64) * 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
                    let v = (16000.0 * angle.sin()) as i16;
                    let b = v.to_le_bytes();
                    buf[f as usize * 4] = b[0];
                    buf[f as usize * 4 + 1] = b[1];
                    buf[f as usize * 4 + 2] = b[0];
                    buf[f as usize * 4 + 3] = b[1];
                }
            } else {
                buf.fill(0);
            }
            if let Some(h) = self.feed.lock().unwrap().clone() {
                h(&buf, 480, self.qpc_pos, false);
            }
            self.qpc_pos += 100_000;
        }
    }
}

type TranscribeFn =
    Arc<dyn Fn(&[f32], &str, &str, &CancelToken) -> Result<TranscribeResult, WorkerError> + Send + Sync>;

struct FakeBackend {
    handler: Mutex<TranscribeFn>,
}

impl FakeBackend {
    fn new(
        f: impl Fn(&[f32], &str, &str, &CancelToken) -> Result<TranscribeResult, WorkerError>
            + Send
            + Sync
            + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            handler: Mutex::new(Arc::new(f)),
        })
    }

    fn set(
        &self,
        f: impl Fn(&[f32], &str, &str, &CancelToken) -> Result<TranscribeResult, WorkerError>
            + Send
            + Sync
            + 'static,
    ) {
        *self.handler.lock().unwrap() = Arc::new(f);
    }
}

impl TranscribeBackend for FakeBackend {
    fn transcribe(
        &self,
        pcm: &[f32],
        source: &str,
        lang: &str,
        ct: &CancelToken,
    ) -> Result<TranscribeResult, WorkerError> {
        let f = self.handler.lock().unwrap().clone();
        f(pcm, source, lang, ct)
    }
}

fn sine(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 * ((i as f32) * 0.2).sin() + 0.01)
        .collect()
}

// =========================================================================
// Sweep3J: A16, CA-S2
// =========================================================================

#[test]
fn a16_segments_evicted_by_drop_oldest_are_zeroed() {
    let hub = Arc::new(LoopbackHub::with_opener(Arc::new(|_| Ok(FakeStream::new(None)))));
    let src = TranscriptSource::new(
        "system",
        Some(hub),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(AudioVerdict::allowed),
    )
    .unwrap();
    let mut evicted = None;
    for i in 0..9i64 {
        let ev = src.enqueue_segment_for_test(SpeechSegment {
            samples: vec![0.25f32; 16000],
            start_qpc_100ns: i,
            duration_ms: 1000,
            app: String::new(),
        });
        if i < 8 {
            assert!(ev.is_none());
        } else {
            evicted = ev;
        }
    }
    assert_eq!(1, src.evicted());
    let (old, orig_len) = evicted.expect("9th enqueue must evict oldest segment");
    assert_eq!(0, old.start_qpc_100ns);
    assert_eq!(16000, orig_len);
    assert!(old.samples.is_empty());
    assert!(old.samples.capacity() >= orig_len);
    unsafe {
        let raw = std::slice::from_raw_parts(old.samples.as_ptr(), orig_len);
        assert!(raw.iter().all(|&x| x == 0.0), "evicted PCM was not zeroed");
    }
    let first_remaining = src.try_recv().unwrap();
    assert_eq!(1, first_remaining.start_qpc_100ns);
    assert!(first_remaining.samples.iter().any(|&x| x != 0.0));
}

#[test]
fn ca_s2_pcm_list_helpers_zero_the_backing_array() {
    let mut l = Vec::with_capacity(8);
    l.extend_from_slice(&[1.0f32, 2.0, 3.0, 4.0]);
    Pcm::remove_head(&mut l, 2);
    assert_eq!(vec![3.0f32, 4.0], l);
    unsafe {
        let tail = std::slice::from_raw_parts(l.as_ptr(), 4);
        assert_eq!(&[3.0f32, 4.0, 0.0, 0.0], tail);
    }
    Pcm::append(&mut l, &[9.0f32; 20]);
    assert_eq!(22, l.len());
    Pcm::wipe(&mut l);
    assert!(l.is_empty());
    unsafe {
        let cap = l.capacity();
        let raw = std::slice::from_raw_parts(l.as_ptr(), cap);
        assert!(raw.iter().all(|&x| x == 0.0));
    }
}

#[test]
fn ca_s2_emitted_and_cleared_segments_leave_no_pcm() {
    let mut seg = SpeechSegmenter::new(Arc::new(|| 30));
    let mut out = Vec::new();
    seg.push(&sine(16000), 0, &mut out);
    assert!(seg.active());
    seg.push(&vec![0.0f32; 16000], 10_000_000, &mut out);
    assert_eq!(1, out.len());
    assert!(!seg.active());
    // Trailing frames stay as pre-roll while idle (by design); only clear() must wipe them.

    seg.push(&sine(16000), 20_000_000, &mut out);
    assert!(seg.active());
    seg.clear();
    assert!(!seg.active());
    assert!(seg.buffered_pcm_zeroed_for_test());
}

// =========================================================================
// Sweep4P: A_H1 (audio)
// =========================================================================

#[test]
fn a_h1_transcript_in_flight_during_purge_all_is_not_stored_in_the_new_store() {
    let dir = TempDir::new("ah1audio");
    fs::write(
        DataPaths::settings_file(dir.path()),
        r#"{"security":{"keyProtection":"dpapi"}}"#,
    )
    .unwrap();
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    host.open_store(&CancelToken::none()).unwrap();

    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let e_cb = entered.clone();
    let r_cb = release.clone();
    let backend = FakeBackend::new(move |pcm, _, _, _| {
        let pre = pcm.len() == 16000;
        if pre {
            *e_cb.0.lock().unwrap() = true;
            e_cb.1.notify_all();
            let mut g = r_cb.0.lock().unwrap();
            while !*g {
                g = r_cb.1.wait(g).unwrap();
            }
        }
        Ok(TranscribeResult {
            speech: true,
            text: if pre {
                "pre purge marker alpha".to_string()
            } else {
                "after the purge".to_string()
            },
            language: "en".to_string(),
            segments: Vec::new(),
            ms: 3,
        })
    });

    let stored = Arc::new(AtomicI32::new(0));
    let stored_cb = stored.clone();
    let host_st = host.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend,
            Arc::new(move || host_st.store().map(|s| s as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(move || {
                stored_cb.fetch_add(1, Ordering::AcqRel);
            }),
        )
        .with_timing(
            Arc::new(|_| Duration::from_millis(50)),
            Duration::from_millis(300),
            100,
        ),
    );
    let hub = Arc::new(LoopbackHub::with_opener(Arc::new(|_| Ok(FakeStream::new(None)))));
    let src = TranscriptSource::new(
        "system",
        Some(hub),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(AudioVerdict::allowed),
    )
    .unwrap();
    src.note_guard(
        0,
        AudioVerdict {
            gate: AudioGate::Allowed,
            app: "teams".to_string(),
            epoch: 0,
        },
    );
    let now_qpc = || OutputCapture::qpc_100ns(qpc());
    src.enqueue_segment(SpeechSegment {
        samples: vec![0.2f32; 16000],
        start_qpc_100ns: now_qpc() - 20_000_000,
        duration_ms: 1000,
        app: String::new(),
    });

    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    {
        let (lock, cv) = &*entered;
        let mut g = lock.lock().unwrap();
        while !*g {
            let (next, res) = cv.wait_timeout(g, Duration::from_secs(10)).unwrap();
            g = next;
            assert!(!res.timed_out(), "transcribe never reached worker");
        }
    }

    host.purge_all(&CancelToken::none());
    {
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
    }
    thread::sleep(Duration::from_millis(200));

    src.enqueue_segment(SpeechSegment {
        samples: vec![0.2f32; 8000],
        start_qpc_100ns: now_qpc(),
        duration_ms: 500,
        app: String::new(),
    });
    assert!(until(
        || stored.load(Ordering::Acquire) == 1,
        Duration::from_secs(10)
    ));

    let store = host.store().unwrap();
    assert_eq!(1, store.stats().unwrap().rows);
    let rows = store
        .timeline(
            Utc::now() - ChronoDuration::hours(1),
            Utc::now() + ChronoDuration::minutes(1),
            10,
        )
        .unwrap();
    assert_eq!(1, rows.len());
    assert_eq!("after the purge", store.get(&rows[0].id).unwrap().unwrap().text);

    cts.cancel();
    handle.join().unwrap();
    host.shutdown();
}

// =========================================================================
// Sweep4R: AH2, AM5, AM7, M3, S4
// =========================================================================

#[test]
fn ah2_timeline_judges_a_segment_by_when_it_was_spoken() {
    let mut t = AudioGuardTimeline::new();
    let ps = AudioVerdict {
        gate: AudioGate::Allowed,
        app: "powershell".to_string(),
        epoch: 0,
    };
    t.note(1_000, ps.clone());
    t.note(
        5_000,
        AudioVerdict {
            gate: AudioGate::Unknown,
            app: "powershell".to_string(),
            epoch: 1,
        },
    );
    t.note(
        5_500,
        AudioVerdict {
            gate: AudioGate::Blocked,
            app: "mstsc".to_string(),
            epoch: 1,
        },
    );
    t.note(
        9_000,
        AudioVerdict {
            gate: AudioGate::Unknown,
            app: "mstsc".to_string(),
            epoch: 2,
        },
    );
    t.note(
        9_200,
        AudioVerdict {
            gate: AudioGate::Allowed,
            app: "notepad".to_string(),
            epoch: 2,
        },
    );

    assert_eq!(ps, t.check(1_000, 4_900));
    assert_eq!(AudioGate::Blocked, t.check(4_000, 6_000).gate);
    assert_eq!(AudioGate::Blocked, t.check(5_100, 5_400).gate);
    let r = t.check(9_100, 12_000);
    assert_eq!((AudioGate::Allowed, "notepad"), (r.gate, r.app.as_str()));
    assert_eq!(AudioGate::Blocked, t.check(-10_000_000, 2_000).gate);

    let mut open = AudioGuardTimeline::new();
    open.note(0, ps);
    open.note(
        100,
        AudioVerdict {
            gate: AudioGate::Unknown,
            app: "powershell".to_string(),
            epoch: 1,
        },
    );
    assert_eq!(AudioGate::Unknown, open.check(50, 200).gate);
}

#[test]
fn ah2_m1_an_unscanned_window_is_not_vouched_for_by_the_next_window() {
    let mut t = AudioGuardTimeline::new();
    t.note(
        0,
        AudioVerdict {
            gate: AudioGate::Allowed,
            app: "a".to_string(),
            epoch: 1,
        },
    );
    t.note(
        1_000,
        AudioVerdict {
            gate: AudioGate::Unknown,
            app: "a".to_string(),
            epoch: 2,
        },
    );
    t.note(
        2_000,
        AudioVerdict {
            gate: AudioGate::Unknown,
            app: "a".to_string(),
            epoch: 3,
        },
    );
    t.note(
        3_000,
        AudioVerdict {
            gate: AudioGate::Allowed,
            app: "b".to_string(),
            epoch: 3,
        },
    );
    assert_eq!(AudioGate::Blocked, t.check(1_100, 1_900).gate);
    assert_eq!(AudioGate::Allowed, t.check(2_100, 5_000).gate);
}

#[test]
fn ah2_s6_segment_span_is_padded_against_the_segmenter_clock() {
    let spk = Speaker::new();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(AudioVerdict::allowed),
    )
    .unwrap();
    src.note_guard(
        0,
        AudioVerdict {
            gate: AudioGate::Allowed,
            app: "a".to_string(),
            epoch: 0,
        },
    );
    src.note_guard(
        10_000_000,
        AudioVerdict {
            gate: AudioGate::Blocked,
            app: "d".to_string(),
            epoch: 0,
        },
    );
    src.note_guard(
        12_000_000,
        AudioVerdict {
            gate: AudioGate::Allowed,
            app: "a".to_string(),
            epoch: 0,
        },
    );
    assert_eq!(
        AudioGate::Blocked,
        src.guard_for(&SpeechSegment {
            samples: Vec::new(),
            start_qpc_100ns: 5_000_000,
            duration_ms: 490,
            app: String::new(),
        })
        .gate
    );
    assert_eq!(
        AudioGate::Allowed,
        src.guard_for(&SpeechSegment {
            samples: Vec::new(),
            start_qpc_100ns: 1_000_000,
            duration_ms: 400,
            app: String::new(),
        })
        .gate
    );
}

#[test]
fn ah2_s8_replay_audio_packets_are_quiet_while_muted() {
    let mut bytes = Vec::with_capacity(960 * 2);
    for i in 0..960 {
        let v: i16 = if i % 2 == 0 { 12000 } else { -12000 };
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    assert!(AudioReplayEncoder::quiet(false, true, &bytes));
    assert!(!AudioReplayEncoder::quiet(false, false, &bytes));
    assert!(AudioReplayEncoder::quiet(true, false, &bytes));
}

#[test]
fn ah2_s8_replay_audio_mute_follows_the_guard() {
    let guard = GuardrailService::new(
        Settings::default,
        || false,
        Vec::new,
        std::path::Path::new(r"C:\nowhere\"),
    );
    let cs = CaptureService::new(
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(|| false),
        guard.clone(),
        None,
        None,
        false,
    );
    let fg = WindowInfo {
        hwnd: 42,
        pid: 1,
        exe_path: r"C:\Windows\System32\mstsc.exe".to_string(),
        title: "Remote Desktop".to_string(),
        class_name: "Cls".to_string(),
    };
    let set_snap = |d: GuardDecision| {
        guard.set_snapshot_for_test(GuardSnapshot {
            taken_at_ms: tick_count64(),
            foreground: Some(fg.clone()),
            decision: d,
            badge: BadgeResult::NotBrowser,
            any_affinity: false,
            mask_rects: Vec::new(),
            per_monitor: Default::default(),
            site: String::new(),
            secrets: None,
            fg_epoch: 0,
            is_closed_sentinel: false,
        });
    };

    assert!(cs.replay_audio_muted());
    set_snap(GuardDecision::deny(GuardReason::DenyProcess));
    assert!(cs.replay_audio_muted());
    set_snap(GuardDecision::ALLOWED);
    assert!(!cs.replay_audio_muted());
    guard.on_foreground_event();
    assert!(cs.replay_audio_muted());
}

#[test]
fn ah2_speech_while_a_denied_window_is_in_front_never_reaches_the_worker_or_the_store() {
    let store = FakeStore::new();
    let transcribes = Arc::new(AtomicI32::new(0));
    let tr_c = transcribes.clone();
    let backend = FakeBackend::new(move |_, _, _, _| {
        tr_c.fetch_add(1, Ordering::AcqRel);
        Ok(TranscribeResult {
            speech: true,
            text: "hello".to_string(),
            language: "en".to_string(),
            segments: Vec::new(),
            ms: 3,
        })
    });
    let mut spk = Speaker::new();
    let verdict = Arc::new(Mutex::new(AudioVerdict {
        gate: AudioGate::Blocked,
        app: "powershell".to_string(),
        epoch: 0,
    }));
    let v_cb = verdict.clone();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub.clone()),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(move || v_cb.lock().unwrap().clone()),
    )
    .unwrap();
    let st_cb = store.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend,
            Arc::new(move || Some(st_cb.clone() as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(|| {}),
        )
        .with_timing(
            Arc::new(|_| Duration::from_millis(400)),
            Duration::from_millis(300),
            300,
        ),
    );
    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    spk.play(true, 1000);
    *verdict.lock().unwrap() = AudioVerdict {
        gate: AudioGate::Allowed,
        app: "notepad".to_string(),
        epoch: 0,
    };
    spk.play(false, 800);
    assert!(until(|| tr.blocked() == 1, Duration::from_secs(5)));
    assert_eq!(1, tr.blocked());

    *verdict.lock().unwrap() = AudioVerdict {
        gate: AudioGate::Allowed,
        app: "powershell".to_string(),
        epoch: 0,
    };
    spk.play(true, 1000);
    *verdict.lock().unwrap() = AudioVerdict {
        gate: AudioGate::Allowed,
        app: "notepad".to_string(),
        epoch: 0,
    };
    spk.play(false, 800);
    assert!(until(
        || !store.inner.lock().unwrap().audio.is_empty(),
        Duration::from_secs(5)
    ));
    cts.cancel();
    handle.join().unwrap();

    let audio = &store.inner.lock().unwrap().audio;
    assert_eq!(1, audio.len());
    assert_eq!("powershell", audio.values().next().unwrap().app);
    assert_eq!(1, transcribes.load(Ordering::Acquire));
}

#[test]
fn am5_segment_survives_a_worker_crash_and_the_restart_backoff() {
    let store = FakeStore::new();
    let calls = Arc::new(AtomicI32::new(0));
    let c_cb = calls.clone();
    let backend = FakeBackend::new(move |_, _, _, _| {
        if c_cb.fetch_add(1, Ordering::AcqRel) == 0 {
            return Err(WorkerError::Transport("worker crashed".to_string()));
        }
        Ok(TranscribeResult {
            speech: true,
            text: "survived".to_string(),
            language: "en".to_string(),
            segments: Vec::new(),
            ms: 3,
        })
    });
    let mut spk = Speaker::new();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub.clone()),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(AudioVerdict::allowed),
    )
    .unwrap();
    let st_cb = store.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend,
            Arc::new(move || Some(st_cb.clone() as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(|| {}),
        )
        .with_timing(
            Arc::new(|_| Duration::from_millis(100)),
            Duration::from_millis(300),
            150,
        ),
    );
    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    spk.play(true, 1000);
    spk.play(false, 800);
    assert!(until(
        || !store.inner.lock().unwrap().audio.is_empty(),
        Duration::from_secs(10)
    ));
    cts.cancel();
    handle.join().unwrap();

    assert_eq!(1, store.inner.lock().unwrap().audio.len());
    assert!(tr.retried() >= 1);
    assert_eq!(0, tr.gave_up());
    assert!(calls.load(Ordering::Acquire) >= 2);
}

#[test]
fn am5_retries_end_on_panic_and_error_replies_are_not_retried() {
    let store = FakeStore::new();
    let reply_calls = Arc::new(AtomicI32::new(0));
    let backend = FakeBackend::new(|_, _, _, _| {
        Err(WorkerError::Transport("worker crashed".to_string()))
    });
    let mut spk = Speaker::new();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub.clone()),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(AudioVerdict::allowed),
    )
    .unwrap();
    let st_cb = store.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend.clone(),
            Arc::new(move || Some(st_cb.clone() as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(|| {}),
        )
        .with_timing(
            Arc::new(|_| Duration::from_millis(200)),
            Duration::from_millis(300),
            200,
        ),
    );
    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    spk.play(true, 1000);
    spk.play(false, 800);
    assert!(until(|| tr.retried() >= 1, Duration::from_secs(5)));
    src.clear();
    let rc = reply_calls.clone();
    backend.set(move |_, _, _, _| {
        rc.fetch_add(1, Ordering::AcqRel);
        Err(WorkerError::Reply("Model not installed: whisper.".to_string()))
    });
    thread::sleep(Duration::from_millis(250));
    assert!(store.inner.lock().unwrap().audio.is_empty());
    assert_eq!(0, tr.gave_up());
    assert_eq!(0, reply_calls.load(Ordering::Acquire));

    let retried = tr.retried();
    spk.play(true, 1000);
    spk.play(false, 800);
    assert!(
        until(|| reply_calls.load(Ordering::Acquire) >= 1, Duration::from_secs(5)),
        "transcriber never called backend for second segment"
    );
    thread::sleep(Duration::from_millis(250));
    cts.cancel();
    handle.join().unwrap();
    assert_eq!(1, reply_calls.load(Ordering::Acquire));
    assert_eq!(retried, tr.retried());
    assert!(store.inner.lock().unwrap().audio.is_empty());
}

#[test]
fn am7_transcript_text_is_redacted() {
    let opts = RedactionOptions::from_privacy(&PrivacySettings::default());
    let cases = [
        ("my card number is 4111 1111 1111 1111 thanks", "my card number is thanks"),
        ("card 4111-1111-1111-1111.", "card ."),
        ("the IBAN is DE89 3704 0044 0532 0130 00 ok", "the IBAN is ok"),
        ("my password is hunter2. Then we left", "my password Then we left"),
        ("social security 123-45-6789", "social security"),
        ("nothing to hide here", "nothing to hide here"),
    ];
    for (raw, kept) in cases {
        assert_eq!(kept, Transcriber::redact(raw, opts, None), "raw: {raw}");
    }
}

#[test]
fn am7_card_split_across_two_segments_is_redacted_in_both() {
    let o = RedactionOptions::from_privacy(&PrivacySettings::default());
    let a = "my card is 4111 1111";
    let b = "1111 1111 and that's it";
    assert_eq!(a, Transcriber::redact(a, o, None));
    let (sa, sb) = Transcriber::boundary_spans(a, b, o);
    assert_eq!("my card is", Transcriber::redact(a, o, Some(&sa)));
    assert_eq!("and that's it", Transcriber::redact(b, o, Some(&sb)));
}

#[test]
fn am7_split_card_number_is_stored_redacted() {
    let store = FakeStore::new();
    let n = Arc::new(AtomicI32::new(0));
    let n_cb = n.clone();
    let backend = FakeBackend::new(move |_, _, _, _| {
        let text = if n_cb.fetch_add(1, Ordering::AcqRel) == 0 {
            "my card is 4111 1111"
        } else {
            "1111 1111 and that's it"
        };
        Ok(TranscribeResult {
            speech: true,
            text: text.to_string(),
            language: "en".to_string(),
            segments: Vec::new(),
            ms: 3,
        })
    });
    let mut spk = Speaker::new();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub.clone()),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(AudioVerdict::allowed),
    )
    .unwrap();
    let st_cb = store.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend,
            Arc::new(move || Some(st_cb.clone() as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(|| {}),
        )
        .with_timing(
            Arc::new(|_| Duration::from_millis(200)),
            Duration::from_millis(300),
            300,
        ),
    );
    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    spk.play(true, 1000);
    spk.play(false, 700);
    spk.play(true, 1000);
    spk.play(false, 800);
    assert!(until(
        || store.inner.lock().unwrap().audio.len() == 2,
        Duration::from_secs(5)
    ));
    cts.cancel();
    handle.join().unwrap();

    let texts: Vec<String> = store
        .inner
        .lock()
        .unwrap()
        .audio
        .values()
        .map(|a| a.text.clone())
        .collect();
    assert_eq!(2, texts.len());
    assert!(texts.iter().all(|t| !t.contains("1111")));
    assert!(texts.iter().any(|t| t == "my card is"));
    assert!(texts.iter().any(|t| t == "and that's it"));
}

#[test]
fn m3_held_transcript_is_stored_on_cancel_and_on_flush() {
    let store = FakeStore::new();
    let done = Arc::new(AtomicI32::new(0));
    let d_cb = done.clone();
    let backend = FakeBackend::new(move |_, _, _, _| {
        let i = d_cb.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(TranscribeResult {
            speech: true,
            text: format!("card 4111 1111 1111 1111 number {i}"),
            language: "en".to_string(),
            segments: Vec::new(),
            ms: 3,
        })
    });
    let mut spk = Speaker::new();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub.clone()),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(AudioVerdict::allowed),
    )
    .unwrap();
    let st_cb = store.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend,
            Arc::new(move || Some(st_cb.clone() as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(|| {}),
        )
        .with_timing(
            Arc::new(|_| Duration::from_millis(200)),
            Duration::from_millis(300),
            60_000,
        ),
    );
    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    spk.play(true, 1000);
    spk.play(false, 800);
    assert!(until(|| done.load(Ordering::Acquire) == 1, Duration::from_secs(5)));
    thread::sleep(Duration::from_millis(200));
    assert!(store.inner.lock().unwrap().audio.is_empty());
    tr.flush_held();
    {
        let g = store.inner.lock().unwrap();
        assert_eq!(1, g.audio.len());
        let first = g.audio.values().next().unwrap();
        assert!(!first.text.contains("4111"));
        assert!(first.text.contains("number 1"));
    }

    spk.play(true, 1000);
    spk.play(false, 800);
    assert!(until(|| done.load(Ordering::Acquire) == 2, Duration::from_secs(5)));
    thread::sleep(Duration::from_millis(200));
    cts.cancel();
    handle.join().unwrap();
    {
        let g = store.inner.lock().unwrap();
        assert_eq!(2, g.audio.len());
        assert!(g
            .audio
            .values()
            .any(|a| a.text.contains("number 2") && !a.text.contains("4111")));
    }
    tr.flush_held();
    assert_eq!(2, store.inner.lock().unwrap().audio.len());
}

#[test]
fn s4_panic_ends_a_retry_wait_at_once() {
    let store = FakeStore::new();
    let backend = FakeBackend::new(|_, _, _, _| {
        Err(WorkerError::Transport("worker crashed".to_string()))
    });
    let hub = Arc::new(LoopbackHub::with_opener(Arc::new(|_| Ok(FakeStream::new(None)))));
    let src = TranscriptSource::new(
        "system",
        Some(hub),
        Arc::new(Settings::default),
        Arc::new(|| false),
        Arc::new(AudioVerdict::allowed),
    )
    .unwrap();
    src.note_guard(
        0,
        AudioVerdict {
            gate: AudioGate::Allowed,
            app: "a".to_string(),
            epoch: 0,
        },
    );
    let st_cb = store.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend,
            Arc::new(move || Some(st_cb.clone() as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(|| {}),
        )
        .with_timing(
            Arc::new(|_| Duration::from_secs(60)),
            Duration::from_millis(300),
            200,
        ),
    );
    src.enqueue_segment(SpeechSegment {
        samples: vec![0.25f32; 16000],
        start_qpc_100ns: OutputCapture::qpc_100ns(qpc()),
        duration_ms: 1000,
        app: String::new(),
    });
    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    assert!(until(|| tr.retried() >= 1, Duration::from_secs(5)));
    let start = std::time::Instant::now();
    src.clear();
    cts.cancel();
    handle.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(store.inner.lock().unwrap().audio.is_empty());
}

// =========================================================================
// Sweep5P: A4, C7
// =========================================================================

#[test]
fn a4_pause_flushes_buffered_speech_through_guard_and_wipes_pcm_before_long_resume() {
    let store = FakeStore::new();
    let n = Arc::new(AtomicI32::new(0));
    let n_cb = n.clone();
    let backend = FakeBackend::new(move |_, _, _, _| {
        let i = n_cb.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(TranscribeResult {
            speech: true,
            text: format!("utterance {i}"),
            language: "en".to_string(),
            segments: Vec::new(),
            ms: 3,
        })
    });
    let st_cb = store.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend,
            Arc::new(move || Some(st_cb.clone() as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(|| {}),
        )
        .with_timing(
            Arc::new(|_| Duration::from_millis(200)),
            Duration::from_millis(300),
            150,
        ),
    );
    let paused = Arc::new(AtomicBool::new(false));
    let verdict = Arc::new(Mutex::new(AudioVerdict {
        gate: AudioGate::Allowed,
        app: "notepad".to_string(),
        epoch: 1,
    }));
    let mut spk = Speaker::new();
    let p_cb = paused.clone();
    let v_cb = verdict.clone();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub.clone()),
        Arc::new(Settings::default),
        Arc::new(move || p_cb.load(Ordering::Acquire)),
        Arc::new(move || v_cb.lock().unwrap().clone()),
    )
    .unwrap();

    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    spk.play(true, 600);
    *verdict.lock().unwrap() = AudioVerdict {
        gate: AudioGate::Allowed,
        app: "edge".to_string(),
        epoch: 2,
    };
    spk.play(true, 600);
    assert!(src.segmenter_active());

    paused.store(true, Ordering::Release);
    src.flush();
    assert!(!src.has_buffered_pcm());

    assert!(until(
        || store.inner.lock().unwrap().audio.len() == 1,
        Duration::from_secs(5)
    ));
    assert_eq!(0, tr.blocked());

    spk.qpc_pos += AudioGuardTimeline::KEEP_FOR_100NS + 600_000_000;
    paused.store(false, Ordering::Release);
    *verdict.lock().unwrap() = AudioVerdict {
        gate: AudioGate::Allowed,
        app: "powershell".to_string(),
        epoch: 3,
    };
    spk.play(true, 1000);
    spk.play(false, 800);

    assert!(until(
        || store.inner.lock().unwrap().audio.len() == 2,
        Duration::from_secs(5)
    ));
    cts.cancel();
    handle.join().unwrap();
    assert_eq!(2, store.inner.lock().unwrap().audio.len());
    assert_eq!(0, tr.blocked());
}

#[test]
fn a4_on_pcm_while_paused_flushes_active_speech_and_wipes_pcm() {
    let store = FakeStore::new();
    let backend = FakeBackend::new(|_, _, _, _| {
        Ok(TranscribeResult {
            speech: true,
            text: "pre pause speech".to_string(),
            language: "en".to_string(),
            segments: Vec::new(),
            ms: 3,
        })
    });
    let st_cb = store.clone();
    let tr = Arc::new(
        Transcriber::new(
            backend,
            Arc::new(move || Some(st_cb.clone() as Arc<dyn AudioStore>)),
            Arc::new(Settings::default),
            Arc::new(|| {}),
        )
        .with_timing(
            Arc::new(|_| Duration::from_millis(200)),
            Duration::from_millis(300),
            100,
        ),
    );
    let paused = Arc::new(AtomicBool::new(false));
    let verdict = Arc::new(Mutex::new(AudioVerdict {
        gate: AudioGate::Allowed,
        app: "notepad".to_string(),
        epoch: 1,
    }));
    let mut spk = Speaker::new();
    let p_cb = paused.clone();
    let v_cb = verdict.clone();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub.clone()),
        Arc::new(Settings::default),
        Arc::new(move || p_cb.load(Ordering::Acquire)),
        Arc::new(move || v_cb.lock().unwrap().clone()),
    )
    .unwrap();
    let cts = CancelSource::new();
    let tr_bg = tr.clone();
    let src_bg = src.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || tr_bg.run(src_bg, &tok));

    spk.play(true, 1000);
    assert!(src.segmenter_active());

    paused.store(true, Ordering::Release);
    *verdict.lock().unwrap() = AudioVerdict {
        gate: AudioGate::Blocked,
        app: "lockapp".to_string(),
        epoch: 2,
    };
    spk.play(false, 20);
    assert!(!src.has_buffered_pcm());

    assert!(until(
        || store.inner.lock().unwrap().audio.len() == 1,
        Duration::from_secs(5)
    ));
    cts.cancel();
    handle.join().unwrap();
    let row = store.inner.lock().unwrap().audio.values().next().unwrap().clone();
    assert_eq!("pre pause speech", row.text);
    assert_eq!("notepad", row.app);
    assert_eq!(0, tr.blocked());
}

#[test]
fn a4_agent_host_pause_and_session_lock_flush_transcript_sources() {
    let dir = TempDir::new("a4host");
    fs::write(
        DataPaths::settings_file(dir.path()),
        r#"{"security":{"keyProtection":"dpapi"},"privacy":{"pauseOnLock":true}}"#,
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
    let pause = agent.pause_controller().clone();
    let mut spk = Speaker::new();
    let core_s = core.clone();
    let src = TranscriptSource::new(
        "system",
        Some(spk.hub.clone()),
        Arc::new(move || core_s.settings().current()),
        Arc::new(move || pause.continuous_paused(true)),
        Arc::new(|| AudioVerdict {
            gate: AudioGate::Allowed,
            app: "notepad".to_string(),
            epoch: 1,
        }),
    )
    .unwrap();
    agent.set_system_audio_for_test(Some(src.clone()));

    spk.play(true, 1000);
    assert!(src.segmenter_active());

    agent.pause(None, "user");
    assert!(!src.has_buffered_pcm());
    assert_eq!(1, src.segments());
    let seg1 = src.try_recv().unwrap();
    assert_eq!(AudioGate::Allowed, src.guard_for(&seg1).gate);

    agent.resume("user");
    spk.qpc_pos += 10_000_000;
    spk.play(true, 1000);
    assert!(src.segmenter_active());

    agent.on_session_changed(WTS_SESSION_LOCK);
    assert!(!src.has_buffered_pcm());
    assert_eq!(2, src.segments());
    let seg2 = src.try_recv().unwrap();
    assert_eq!(AudioGate::Allowed, src.guard_for(&seg2).gate);
    agent.shutdown();
    core.shutdown();
}

#[test]
fn c7_wasapi_start_timeout_and_late_ready_do_not_throw_object_disposed_exception() {
    for fail_late in [false, true] {
        let unblock = Arc::new((Mutex::new(false), Condvar::new()));
        let finished = Arc::new(AtomicBool::new(false));
        let u_cb = unblock.clone();
        let f_cb = finished.clone();
        let err = WasapiCapture::start_for_test(
            "loopback",
            Arc::new(|_, _, _, _| {}),
            Arc::new(move || {
                let (lock, cv) = &*u_cb;
                let mut g = lock.lock().unwrap();
                while !*g {
                    g = cv.wait(g).unwrap();
                }
                f_cb.store(true, Ordering::Release);
                if fail_late {
                    Err("late failure".to_string())
                } else {
                    Ok(())
                }
            }),
            Duration::from_millis(60),
        )
        .expect_err("expected timeout");
        assert!(err.contains("timed out"), "got {err}");

        {
            *unblock.0.lock().unwrap() = true;
            unblock.1.notify_all();
        }
        assert!(until(
            || finished.load(Ordering::Acquire),
            Duration::from_secs(5)
        ));
    }
}

#[test]
fn c7_wasapi_start_error_and_normal_dispose_clean_up() {
    let err = WasapiCapture::start_for_test(
        "mic",
        Arc::new(|_, _, _, _| {}),
        Arc::new(|| Err("no mic".to_string())),
        Duration::from_secs(2),
    )
    .expect_err("expected error");
    assert_eq!("no mic", err);

    let c = WasapiCapture::start_for_test(
        "loopback",
        Arc::new(|_, _, _, _| {}),
        Arc::new(|| Ok(())),
        Duration::from_secs(2),
    )
    .unwrap();
    c.stop();
    c.stop(); // idempotent
    assert!(c.ended());
}

// =========================================================================
// ReliabilitySweep2: LoopbackHub recovery + AudioNeedsSync
// =========================================================================

#[test]
fn loopback_restarts_after_its_thread_ends_and_keeps_subscribers() {
    let streams: Arc<Mutex<Vec<Arc<FakeStream>>>> = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(false));
    let s_cb = streams.clone();
    let f_cb = fail.clone();
    let hub = Arc::new(LoopbackHub::with_opener(Arc::new(move |h| {
        if f_cb.load(Ordering::Acquire) {
            return Err("audio service down".to_string());
        }
        let s = FakeStream::new(Some(h));
        s_cb.lock().unwrap().push(s.clone());
        Ok(s)
    })));

    let got = Arc::new(AtomicI32::new(0));
    let g_cb = got.clone();
    hub.subscribe(Arc::new(move |_, _, _, _| {
        g_cb.fetch_add(1, Ordering::AcqRel);
    }))
    .unwrap();
    assert!(hub.running());
    streams.lock().unwrap()[0].push();
    assert_eq!(1, got.load(Ordering::Acquire));

    streams.lock().unwrap()[0].ended.store(true, Ordering::Release);
    fail.store(true, Ordering::Release);
    assert!(!hub.running());
    assert!(!hub.check_health(0));
    assert!(streams.lock().unwrap()[0].disposed.load(Ordering::Acquire));
    assert!(!hub.check_health(1000));
    fail.store(false, Ordering::Release);
    assert!(!hub.check_health(2000));
    assert!(hub.check_health(3000));
    assert!(hub.running());
    assert_eq!(1, hub.restarts());
    streams.lock().unwrap()[1].push();
    assert_eq!(2, got.load(Ordering::Acquire));
}

#[test]
fn loopback_without_subscribers_is_not_restarted() {
    let opens = Arc::new(AtomicI32::new(0));
    let o_cb = opens.clone();
    let hub = Arc::new(LoopbackHub::with_opener(Arc::new(move |h| {
        o_cb.fetch_add(1, Ordering::AcqRel);
        let s = FakeStream::new(Some(h));
        s.ended.store(true, Ordering::Release);
        Ok(s)
    })));
    assert!(!hub.check_health(10_000));
    assert_eq!(0, opens.load(Ordering::Acquire));
}

// =========================================================================
// ServiceTests: AudioLogicTests
// =========================================================================

fn tone_16k(samples: usize, amp: f32) -> Vec<f32> {
    (0..samples)
        .map(|i| amp * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16_000.0).sin())
        .collect()
}

#[test]
fn segmenter_emits_speech_and_ignores_silence() {
    let mut seg = SpeechSegmenter::new(Arc::new(|| 30));
    let mut outp: Vec<SpeechSegment> = Vec::new();
    let mut t = 0i64;
    let feed = |seg: &mut SpeechSegmenter, t: &mut i64, outp: &mut Vec<SpeechSegment>, s: &[f32]| {
        seg.push(s, *t, outp);
        *t += (s.len() as i64) * 10_000_000 / 16_000;
    };
    feed(&mut seg, &mut t, &mut outp, &vec![0.0f32; 16_000 * 2]); // 2 s silence
    feed(&mut seg, &mut t, &mut outp, &tone_16k(16_000 * 2, 0.3)); // 2 s "speech"
    feed(&mut seg, &mut t, &mut outp, &vec![0.0f32; 16_000]); // 1 s silence ends it
    assert_eq!(1, outp.len());
    let s0 = &outp[0];
    assert!(
        (2000..=2700).contains(&s0.duration_ms),
        "duration_ms={}",
        s0.duration_ms
    );
    assert!(
        (18_000_000..=20_000_000).contains(&s0.start_qpc_100ns),
        "start_qpc_100ns={}",
        s0.start_qpc_100ns
    );
    feed(&mut seg, &mut t, &mut outp, &vec![0.0f32; 16_000 * 5]);
    assert_eq!(1, outp.len());
}

#[test]
fn segmenter_cuts_at_max_length() {
    let mut seg = SpeechSegmenter::new(Arc::new(|| 5));
    let mut outp: Vec<SpeechSegment> = Vec::new();
    let tone = tone_16k(16_000 * 12, 0.3);
    seg.push(&tone, 0, &mut outp);
    assert_eq!(2, outp.len());
    assert!(outp.iter().all(|s| s.duration_ms <= 5000));
    seg.flush(&mut outp);
    assert_eq!(3, outp.len());
}

#[test]
fn resampler_48k_stereo_int16_to_16k_mono() {
    let mut r = br_agent::audio::Resampler::new(br_agent::audio::PcmFormat {
        sample_rate: 48_000,
        channels: 2,
        is_float: false,
        bits_per_sample: 16,
    });
    let mut pcm = vec![0u8; 4800 * 4];
    for i in 0..4800usize {
        let v = (10_000.0 * (i as f64 * 2.0 * std::f64::consts::PI * 440.0 / 48_000.0).sin())
            as i16;
        let bytes = v.to_le_bytes();
        pcm[i * 4..i * 4 + 2].copy_from_slice(&bytes);
        pcm[i * 4 + 2..i * 4 + 4].copy_from_slice(&bytes);
    }
    let mut output: Vec<f32> = Vec::new();
    r.process(&pcm[..2401 * 4], 2401, false, &mut output); // odd split exercises carry-over
    r.process(&pcm[2401 * 4..], 4800 - 2401, false, &mut output);
    assert_eq!(1600, output.len());
    let max = output.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    assert!((0.25..=0.32).contains(&max), "max={max}");
    let mut silent: Vec<f32> = Vec::new();
    r.process(&pcm, 4800, true, &mut silent);
    assert!(silent.iter().all(|&v| v == 0.0));
}

#[test]
fn replay_audio_gate_skips_only_near_silence() {
    let quiet: [i16; 6] = [0, 12, -40, 64, -64, 3];
    let loud: [i16; 5] = [0, 12, -40, 65, 3];
    let quiet_bytes: Vec<u8> = quiet.iter().flat_map(|s| s.to_le_bytes()).collect();
    let loud_bytes: Vec<u8> = loud.iter().flat_map(|s| s.to_le_bytes()).collect();
    assert!(AudioReplayEncoder::is_near_silent_16(&quiet_bytes));
    assert!(!AudioReplayEncoder::is_near_silent_16(&loud_bytes));
    assert!(AudioReplayEncoder::is_near_silent_16(&[]));
}
