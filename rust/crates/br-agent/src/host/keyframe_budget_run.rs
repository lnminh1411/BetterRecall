// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Keyframe CPU budget self-test (`--selftest-keyframe-budget [minutes]`, `Host\KeyframeBudgetRun.cs`).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{InstallLayout, NewScreenCapture, Settings, full_path};
use br_core::{CancelSource, CancelToken, EncryptedStore, win32 as core_win32};
use chrono::Utc;
use zeroize::Zeroize;

use crate::capture::{
    CapturePolicy, ChangeMap, DHash, DedupFilter, EncodeGovernor, EncodeLevel, GpuDeviceCache,
    GpuDevices, ImageCodec, KeepReason, KeyframeEncoder, KeyframeGrab, KeyframePipeline,
    KeyframeReader, KeyframeRequest, OutputCapture, WorkerKeyframeReader,
};
use crate::codecs::Avif;
use crate::native::{
    Rect, current_thread_cpu_ms, foreground_window, logical_processor_count, process_cpu_ms,
    process_cpu_ms_by_pid, set_current_thread_below_normal, set_current_thread_lowest,
};
use crate::privacy::{RedactionMasks, RedactionOptions, Redactor};
use crate::scheduling::Governance;
use crate::security::MemoryIsolation;
use crate::util::AgentLog;
use crate::worker::{ProcessWorkerLauncher, WorkerClient, WorkerError, WorkerSupervisor};

#[derive(Clone, Copy)]
struct Encoded {
    t: i64,
    level: EncodeLevel,
    cpu_ms: f64,
    wall_ms: f64,
    bytes: usize,
}

#[derive(Clone, Copy)]
struct OcrRun {
    t: i64,
    cpu_ms: f64,
    wall_ms: f64,
    ok: bool,
    spans: usize,
}

struct BudgetItem {
    grab: KeyframeGrab,
    text: Option<String>,
}

struct CompareItem {
    px: Vec<u8>,
    w: i32,
    h: i32,
    avif: Vec<u8>,
}

struct MatchedSize {
    avif: usize,
    webp: i32,
    jpeg: i32,
}

struct BoundedQueue<T> {
    cap: usize,
    state: Mutex<(VecDeque<T>, bool)>,
    cv: Condvar,
}

impl<T> BoundedQueue<T> {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            state: Mutex::new((VecDeque::with_capacity(cap), false)),
            cv: Condvar::new(),
        }
    }

    fn try_add(&self, item: T) -> Result<(), T> {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if g.1 || g.0.len() >= self.cap {
            return Err(item);
        }
        g.0.push_back(item);
        self.cv.notify_one();
        Ok(())
    }

    fn len(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0.len()
    }

    fn complete_adding(&self) {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        g.1 = true;
        self.cv.notify_all();
    }

    fn recv(&self) -> Option<T> {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(item) = g.0.pop_front() {
                return Some(item);
            }
            if g.1 {
                return None;
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct StoreCounters {
    stored: i64,
    failed: i64,
    first_error: String,
    last_error: String,
}

pub struct KeyframeBudgetRun;

impl KeyframeBudgetRun {
    pub fn run(args: &[String]) -> i32 {
        AgentLog::set_also_console(true);
        let minutes = args
            .get(1)
            .filter(|a| !a.starts_with("--"))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(10.0);

        let mut ocr_home: Option<PathBuf> = None;
        let mut worker_exe: Option<PathBuf> = None;
        let mut store_dir: Option<PathBuf> = None;
        let mut acg_at_ms: i64 = 8000;
        let mut i = 1usize;
        while i + 1 < args.len() {
            match args[i].as_str() {
                "--ocr-home" => {
                    i += 1;
                    let p = PathBuf::from(&args[i]);
                    ocr_home = Some(full_path(&p).unwrap_or(p));
                }
                "--worker" => {
                    i += 1;
                    let p = PathBuf::from(&args[i]);
                    worker_exe = Some(full_path(&p).unwrap_or(p));
                }
                "--store" => {
                    i += 1;
                    let p = PathBuf::from(&args[i]);
                    store_dir = Some(full_path(&p).unwrap_or(p));
                }
                "--acg-at" => {
                    i += 1;
                    if let Ok(v) = args[i].parse::<f64>() {
                        acg_at_ms = (v * 1000.0) as i64;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let acg = args.iter().any(|a| a == "--acg") || acg_at_ms != 8000;
        let warm = !args.iter().any(|a| a == "--no-warm");

        let store_counters = Arc::new(Mutex::new(StoreCounters {
            stored: 0,
            failed: 0,
            first_error: String::new(),
            last_error: String::new(),
        }));
        let store: Option<Arc<EncryptedStore>> = if let Some(ref dir) = store_dir {
            let mut key = vec![0u8; 32];
            core_win32::fill_random(&mut key);
            match EncryptedStore::open(dir, move || key.clone(), None) {
                Ok(st) => {
                    println!("store on: {}", dir.display());
                    Some(Arc::new(st))
                }
                Err(e) => {
                    println!("store failed: {e}");
                    return 1;
                }
            }
        } else {
            None
        };

        let s = Settings::default();
        let mut supervisor: Option<Arc<WorkerSupervisor>> = None;
        let mut reader: Option<Arc<dyn KeyframeReader>> = None;
        if let Some(ref home) = ocr_home {
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("."));
            let w_exe = worker_exe
                .unwrap_or_else(|| InstallLayout::full(&exe_dir, InstallLayout::WORKER_EXE));
            let eco_qos = s.governance.eco_qos;
            let launcher = Arc::new(ProcessWorkerLauncher::new(
                w_exe.clone(),
                move || eco_qos,
                None,
            ));
            let sup = WorkerSupervisor::new(launcher, home.clone(), || 24 * 3600);
            let client = Arc::new(WorkerClient::new(sup.clone()));
            reader = Some(Arc::new(WorkerKeyframeReader::new(client)));
            println!("ocr on: worker {}, home {}", w_exe.display(), home.display());
            supervisor = Some(sup);
        }

        let red = RedactionOptions::from_privacy(&s.privacy);
        let outputs = GpuDevices::outputs();
        let procs = logical_processor_count();
        println!(
            "{} monitor(s), {procs} logical processors, {minutes} min, interval {} ms, format {} q{}, dirtyMinPercent {}",
            outputs.len(),
            s.capture.interval_ms,
            s.keyframes.format,
            s.keyframes.quality,
            s.capture.dirty_min_percent
        );
        let _ = Avif::try_load();

        let devices = Arc::new(GpuDeviceCache::new());
        let grabs = Arc::new(BoundedQueue::<KeyframeGrab>::new(6));
        let compare = Arc::new(BoundedQueue::<CompareItem>::new(4));
        let encodes: Arc<Mutex<Vec<Encoded>>> = Arc::new(Mutex::new(Vec::new()));
        let ocrs: Arc<Mutex<Vec<OcrRun>>> = Arc::new(Mutex::new(Vec::new()));
        let ocr_first_error: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let matched: Arc<Mutex<Vec<MatchedSize>>> = Arc::new(Mutex::new(Vec::new()));
        let skipped = Arc::new(AtomicI64::new(0));
        let kept = Arc::new(AtomicI64::new(0));
        let channel_drops = Arc::new(AtomicI64::new(0));
        let clock = Instant::now();

        let captures: Vec<Arc<OutputCapture>> = outputs
            .into_iter()
            .map(|o| {
                let grabs_cb = grabs.clone();
                let drops_cb = channel_drops.clone();
                Arc::new(OutputCapture::new(
                    o,
                    devices.clone(),
                    Arc::new(move |g| {
                        if grabs_cb.try_add(g).is_err() {
                            drops_cb.fetch_add(1, Ordering::Relaxed);
                        }
                    }),
                ))
            })
            .collect();

        let gov_waiting = Arc::new(AtomicI64::new(0));
        let gov_dropped = Arc::new(AtomicI64::new(0));

        let worker_handle = {
            let grabs = grabs.clone();
            let compare = compare.clone();
            let encodes = encodes.clone();
            let ocrs = ocrs.clone();
            let ocr_first_error = ocr_first_error.clone();
            let skipped = skipped.clone();
            let kept = kept.clone();
            let store = store.clone();
            let store_counters = store_counters.clone();
            let reader = reader.clone();
            let settings = s.clone();
            let gov_waiting = gov_waiting.clone();
            let gov_dropped = gov_dropped.clone();

            thread::Builder::new()
                .name("keyframe-budget".to_string())
                .spawn(move || {
                    set_current_thread_below_normal();
                    Governance::apply_eco_qos_to_current_thread();
                    let mut dedup: HashMap<i32, DedupFilter> = HashMap::new();
                    let encodes_cb = encodes.clone();
                    let compare_cb = compare.clone();
                    let store_cb = store.clone();
                    let sc_cb = store_counters.clone();
                    let s_cb = settings.clone();
                    let mut governor = EncodeGovernor::<BudgetItem>::new(
                        move || clock.elapsed().as_millis() as i64,
                        procs,
                        move |item, level| {
                            let g = &item.grab;
                            let cpu0 = current_thread_cpu_ms();
                            let sw = Instant::now();
                            let enc = unsafe {
                                KeyframeEncoder::encode(
                                    g.pixels().as_ptr(),
                                    g.width,
                                    g.height,
                                    g.stride,
                                    &s_cb,
                                    level,
                                )
                            };
                            let cpu = current_thread_cpu_ms() - cpu0;
                            let Ok((mut bytes, format, ew, eh)) = enc else {
                                return cpu;
                            };
                            if let Some(ref st) = store_cb {
                                let cap = NewScreenCapture {
                                    ts: g.request.ts,
                                    app: "selftest".to_string(),
                                    exe_path: String::new(),
                                    title: String::new(),
                                    monitor: g.monitor,
                                    width: ew,
                                    height: eh,
                                    d_hash: 0,
                                    image: bytes.clone(),
                                    image_mime: ImageCodec::mime(format).to_string(),
                                    keep_keyframe: true,
                                    site: String::new(),
                                    change: None,
                                    switch: false,
                                    text: item.text.clone(),
                                };
                                match st.add_screen_capture(&cap) {
                                    Ok(_) => {
                                        sc_cb.lock().unwrap_or_else(|e| e.into_inner()).stored += 1;
                                    }
                                    Err(e) => {
                                        let msg = format!("StoreError: {e}");
                                        let mut sc = sc_cb.lock().unwrap_or_else(|e| e.into_inner());
                                        sc.failed += 1;
                                        if sc.first_error.is_empty() {
                                            sc.first_error = msg.clone();
                                        }
                                        sc.last_error = msg.clone();
                                        println!("  store failed: {msg}");
                                    }
                                }
                            }
                            let bytes_len = bytes.len();
                            encodes_cb.lock().unwrap_or_else(|e| e.into_inner()).push(Encoded {
                                t: clock.elapsed().as_millis() as i64,
                                level,
                                cpu_ms: cpu,
                                wall_ms: sw.elapsed().as_secs_f64() * 1000.0,
                                bytes: bytes_len,
                            });
                            if format == "avif" && level == EncodeLevel::Normal && compare_cb.len() < 2
                            {
                                let len = (g.width * g.height * 4) as usize;
                                let mut px = vec![0u8; len];
                                px.copy_from_slice(&g.pixels()[..len]);
                                if let Err(mut rejected) = compare_cb.try_add(CompareItem {
                                    px,
                                    w: g.width,
                                    h: g.height,
                                    avif: bytes,
                                }) {
                                    rejected.px.zeroize();
                                    rejected.avif.zeroize();
                                }
                            } else {
                                bytes.zeroize();
                            }
                            cpu
                        },
                        |_, why| println!("  dropped ({why})"),
                        KeyframeEncoder::INITIAL_ESTIMATE_MS,
                    );

                    while let Some(mut g) = grabs.recv() {
                        let d = dedup.entry(g.monitor).or_default();
                        let Ok(hash) = DHash::compute(g.pixels(), g.width, g.height, g.stride)
                        else {
                            continue;
                        };
                        let Ok(mut map) =
                            ChangeMap::compute(g.pixels(), g.width, g.height, g.stride)
                        else {
                            continue;
                        };
                        let (reason, _) = d.decide(
                            &map,
                            hash,
                            &g.request,
                            settings.capture.dhash_threshold,
                            settings.capture.dirty_min_percent,
                        );
                        if reason == KeepReason::Skip {
                            map.clear();
                            skipped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let mut text: Option<String> = None;
                        if let Some(ref rdr) = reader {
                            let c0 = current_thread_cpu_ms();
                            let sw = Instant::now();
                            let mut copy = g.pixels().to_vec();
                            let timeout = CancelSource::with_timeout(
                                &CancelToken::none(),
                                KeyframePipeline::OCR_TIMEOUT,
                            );
                            let res = match rdr.read(
                                &copy,
                                g.width,
                                g.height,
                                g.stride,
                                red.entities(),
                                &timeout.token(),
                            ) {
                                Ok(r) => Some(r),
                                Err(e) => {
                                    let msg = match e {
                                        WorkerError::Cancelled => "Operation cancelled.".to_string(),
                                        other => other.message().to_string(),
                                    };
                                    let mut fe =
                                        ocr_first_error.lock().unwrap_or_else(|e| e.into_inner());
                                    if fe.is_empty() {
                                        *fe = msg;
                                    }
                                    None
                                }
                            };
                            copy.zeroize();
                            let mut spans = 0usize;
                            if let Some(ref ocr_res) = res {
                                let rr = Redactor::apply(&ocr_res.lines, &[], red, "");
                                if !rr.boxes.is_empty() {
                                    let w = g.width;
                                    let h = g.height;
                                    let st = g.stride;
                                    unsafe {
                                        RedactionMasks::fill(
                                            g.pixels_mut().as_mut_ptr(),
                                            w,
                                            h,
                                            st,
                                            &rr.boxes,
                                        );
                                    }
                                }
                                spans = rr.spans;
                                text = Some(rr.text);
                            }
                            ocrs.lock().unwrap_or_else(|e| e.into_inner()).push(OcrRun {
                                t: clock.elapsed().as_millis() as i64,
                                cpu_ms: current_thread_cpu_ms() - c0,
                                wall_ms: sw.elapsed().as_secs_f64() * 1000.0,
                                ok: res.is_some(),
                                spans,
                            });
                        }
                        d.accept(hash, g.request.context.clone(), map);
                        kept.fetch_add(1, Ordering::Relaxed);
                        governor.offer(BudgetItem { grab: g, text });
                        governor.pump();
                        gov_waiting.store(governor.waiting() as i64, Ordering::Relaxed);
                        gov_dropped.store(governor.dropped as i64, Ordering::Relaxed);
                    }
                    gov_waiting.store(governor.waiting() as i64, Ordering::Relaxed);
                    gov_dropped.store(governor.dropped as i64, Ordering::Relaxed);
                })
                .expect("spawn keyframe-budget thread")
        };

        // Size check outside the budget: smallest WebP / JPEG whose text-block PSNR reaches the AVIF's.
        let comparer_handle = {
            let compare = compare.clone();
            let matched = matched.clone();
            thread::Builder::new()
                .name("keyframe-compare".to_string())
                .spawn(move || {
                    set_current_thread_lowest();
                    while let Some(mut item) = compare.recv() {
                        if let Ok((mut dec, dw, dh)) =
                            ImageCodec::decode_bgra(&item.avif, "image/avif")
                        {
                            if dw == item.w && dh == item.h {
                                let target = Self::text_psnr(&item.px, &dec, item.w, item.h);
                                dec.zeroize();
                                let smallest = |format: &str| -> i32 {
                                    for q in [40, 50, 60, 70, 75, 80, 85, 90, 95] {
                                        let Ok(enc) = (unsafe {
                                            ImageCodec::encode_bgra(
                                                item.px.as_ptr(),
                                                item.w,
                                                item.h,
                                                item.w * 4,
                                                format,
                                                q,
                                            )
                                        }) else {
                                            continue;
                                        };
                                        let Ok((mut d2, _, _)) =
                                            ImageCodec::decode_bgra(&enc, ImageCodec::mime(format))
                                        else {
                                            continue;
                                        };
                                        let t = Self::text_psnr(&item.px, &d2, item.w, item.h);
                                        d2.zeroize();
                                        if t >= target {
                                            return enc.len() as i32;
                                        }
                                    }
                                    -1
                                };
                                let webp = smallest("webp");
                                let jpeg = smallest("jpeg");
                                matched
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .push(MatchedSize {
                                        avif: item.avif.len(),
                                        webp,
                                        jpeg,
                                    });
                            } else {
                                dec.zeroize();
                            }
                        }
                        item.px.zeroize();
                        item.avif.zeroize();
                    }
                })
                .expect("spawn keyframe-compare thread")
        };

        for c in &captures {
            c.start();
        }
        thread::sleep(Duration::from_millis(500));

        let end = (minutes * 60_000.0) as i64;
        let mut next_tick: i64 = 0;
        let mut next_report: i64 = 10_000;
        let mut last_trigger: i64 = -10_000;
        let mut acg_done = !acg;
        let mut last_fg: isize = 0;
        let proc_cpu0 = process_cpu_ms();

        let mut worker_pid: u32 = 0;
        let mut worker_base = 0.0f64;
        let mut worker_seen = 0.0f64;
        let mut sample_worker_cpu = || -> f64 {
            let Some(ref sup) = supervisor else {
                return worker_seen;
            };
            let pid = sup.pid();
            if pid == 0 {
                return worker_seen;
            }
            if let Some(t) = process_cpu_ms_by_pid(pid) {
                if pid != worker_pid {
                    worker_pid = pid;
                    worker_base = worker_seen;
                }
                worker_seen = worker_base + t;
            }
            worker_seen
        };
        let mut worker_last = 0.0f64;

        while (clock.elapsed().as_millis() as i64) < end {
            let now = clock.elapsed().as_millis() as i64;
            if !acg_done && now >= acg_at_ms {
                acg_done = true;
                if warm {
                    let _ = EncryptedStore::warm_native_callbacks();
                }
                MemoryIsolation::apply_late(true);
                println!(
                    "acg: applied [{}] skipped [{}], warm-up {}",
                    MemoryIsolation::applied().join(", "),
                    MemoryIsolation::skipped().join(", "),
                    if warm { "on" } else { "off" }
                );
            }
            let fg = foreground_window();
            let trigger = fg != last_fg && now - last_trigger >= 1000;
            if fg != last_fg {
                last_fg = fg;
            }
            if now >= next_tick || trigger {
                if now >= next_tick {
                    next_tick = now + s.capture.interval_ms as i64;
                }
                if trigger {
                    last_trigger = now;
                }
                let ctx = fg.to_string();
                let req = KeyframeRequest {
                    ts: Utc::now(),
                    app: String::new(),
                    exe_path: String::new(),
                    title: String::new(),
                    context: ctx,
                    force: false,
                    max_width: s.capture.max_width,
                    dirty_min_percent: s
                        .capture
                        .dirty_min_percent
                        .min(CapturePolicy::TRIGGER_MIN_SCORE),
                    site: String::new(),
                    keyframe: true,
                    replay_sample: false,
                    trigger,
                    secrets: None,
                    bounds: Rect::default(),
                    mask_only: false,
                };
                for c in &captures {
                    c.set_pending_keyframe(Some(req.clone()));
                }
            }
            if now >= next_report {
                next_report += 10_000;
                let mut cpu10 = 0.0f64;
                let mut per_level = [0i32; 4];
                {
                    let enc = encodes.lock().unwrap_or_else(|e| e.into_inner());
                    for e in enc.iter().filter(|e| e.t > now - 10_000) {
                        cpu10 += e.cpu_ms;
                        per_level[e.level.index()] += 1;
                    }
                }
                let ocr_part = if reader.is_some() {
                    let o10: f64 = ocrs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .filter(|e| e.t > now - 10_000)
                        .map(|e| e.cpu_ms)
                        .sum();
                    let w_now = sample_worker_cpu();
                    let part = format!(
                        "  ocr CPU last 10 s: thread {:.2} %, worker {:.2} %",
                        o10 / (procs as f64 * 10_000.0) * 100.0,
                        (w_now - worker_last) / (procs as f64 * 10_000.0) * 100.0
                    );
                    worker_last = w_now;
                    part
                } else {
                    String::new()
                };
                println!(
                    "t={:4} s  encode CPU last 10 s {:.2} %  frames n/f/r/w {}/{}/{}/{}  waiting {}  kept {} skipped {}{ocr_part}",
                    now / 1000,
                    cpu10 / (procs as f64 * 10_000.0) * 100.0,
                    per_level[0],
                    per_level[1],
                    per_level[2],
                    per_level[3],
                    gov_waiting.load(Ordering::Relaxed),
                    kept.load(Ordering::Relaxed),
                    skipped.load(Ordering::Relaxed)
                );
            }
            thread::sleep(Duration::from_millis(50));
        }

        for c in &captures {
            c.stop();
        }
        grabs.complete_adding();
        let _ = worker_handle.join();
        let worker_total = sample_worker_cpu();
        if let Some(sup) = supervisor {
            sup.dispose();
        }
        compare.complete_adding();
        let _ = comparer_handle.join();
        let proc_cpu = process_cpu_ms() - proc_cpu0;

        // Windows over the records: 10 s and 60 s sliding, evaluated at every encode.
        let list = encodes.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut peak10 = 0.0f64;
        let mut peak60 = 0.0f64;
        for e in &list {
            let s10: f64 = list
                .iter()
                .filter(|x| x.t > e.t - 10_000 && x.t <= e.t)
                .map(|x| x.cpu_ms)
                .sum();
            let s60: f64 = list
                .iter()
                .filter(|x| x.t > e.t - 60_000 && x.t <= e.t)
                .map(|x| x.cpu_ms)
                .sum();
            peak10 = peak10.max(s10);
            peak60 = peak60.max(s60);
        }
        let total: f64 = list.iter().map(|e| e.cpu_ms).sum();
        println!();
        println!(
            "run {} s: {} kept, {} skipped as similar, {} encoded, {} dropped by budget, {} grab drops",
            end / 1000,
            kept.load(Ordering::Relaxed),
            skipped.load(Ordering::Relaxed),
            list.len(),
            gov_dropped.load(Ordering::Relaxed),
            channel_drops.load(Ordering::Relaxed)
        );
        println!(
            "encode CPU: average {:.3} %, peak 10 s {:.3} % (cap 1.5), peak 60 s {:.3} % (cap 1.0)",
            total / (procs as f64 * end as f64) * 100.0,
            peak10 / (procs as f64 * 10_000.0) * 100.0,
            peak60 / (procs as f64 * 60_000.0) * 100.0
        );
        if reader.is_some() {
            let ol = ocrs.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let mut o_peak10 = 0.0f64;
            let mut o_peak60 = 0.0f64;
            for e in &ol {
                let s10: f64 = ol
                    .iter()
                    .filter(|x| x.t > e.t - 10_000 && x.t <= e.t)
                    .map(|x| x.cpu_ms)
                    .sum();
                let s60: f64 = ol
                    .iter()
                    .filter(|x| x.t > e.t - 60_000 && x.t <= e.t)
                    .map(|x| x.cpu_ms)
                    .sum();
                o_peak10 = o_peak10.max(s10);
                o_peak60 = o_peak60.max(s60);
            }
            let ok = ol.iter().filter(|o| o.ok).count();
            let fe = ocr_first_error.lock().unwrap_or_else(|e| e.into_inner());
            let err_note = if fe.is_empty() {
                String::new()
            } else {
                format!(" (first: {fe})")
            };
            let total_spans: usize = ol.iter().map(|o| o.spans).sum();
            println!(
                "ocr (not in the encode budget): {} frames, {ok} read, {} failed{err_note}, redacted spans {total_spans}",
                ol.len(),
                ol.len() - ok
            );
            if !ol.is_empty() {
                let wall_avg = ol.iter().map(|o| o.wall_ms).sum::<f64>() / ol.len() as f64;
                let wall_max = ol.iter().map(|o| o.wall_ms).fold(0.0f64, f64::max);
                let cpu_sum: f64 = ol.iter().map(|o| o.cpu_ms).sum();
                let cpu_avg = cpu_sum / ol.len() as f64;
                println!(
                    "  wall avg {wall_avg:.0} ms (max {wall_max:.0}); keyframe thread CPU avg {cpu_avg:.1} ms/frame, {:.3} % average, peak 10 s {:.3} %, peak 60 s {:.3} %",
                    cpu_sum / (procs as f64 * end as f64) * 100.0,
                    o_peak10 / (procs as f64 * 10_000.0) * 100.0,
                    o_peak60 / (procs as f64 * 60_000.0) * 100.0
                );
            }
            println!(
                "  worker process CPU (OCR plus its idle work): {:.3} % average, {:.0} ms per read frame",
                worker_total / (procs as f64 * end as f64) * 100.0,
                if ok > 0 {
                    worker_total / ok as f64
                } else {
                    0.0
                }
            );
        }
        if let Some(st) = store {
            let sc = store_counters.lock().unwrap_or_else(|e| e.into_inner());
            let err_note = if sc.first_error.is_empty() {
                String::new()
            } else {
                format!(" (first: {}; last: {})", sc.first_error, sc.last_error)
            };
            println!(
                "store: {} stored, {} failed{err_note}",
                sc.stored, sc.failed
            );
            drop(st);
        }
        println!(
            "whole test process CPU (capture, smart capture, size check{} included; worker excluded): {:.3} %",
            if reader.is_none() {
                ""
            } else {
                ", OCR client side"
            },
            proc_cpu / (procs as f64 * end as f64) * 100.0
        );
        for level in [
            EncodeLevel::Normal,
            EncodeLevel::Fast,
            EncodeLevel::Reduced,
            EncodeLevel::WebP,
        ] {
            let group: Vec<&Encoded> = list.iter().filter(|e| e.level == level).collect();
            if group.is_empty() {
                continue;
            }
            let n = group.len() as f64;
            let cpu_avg = group.iter().map(|e| e.cpu_ms).sum::<f64>() / n;
            let cpu_max = group.iter().map(|e| e.cpu_ms).fold(0.0f64, f64::max);
            let wall_avg = group.iter().map(|e| e.wall_ms).sum::<f64>() / n;
            let kib_avg = group.iter().map(|e| e.bytes as f64).sum::<f64>() / n / 1024.0;
            println!(
                "  {:<8} {:4} frames  CPU {cpu_avg:.1} ms (max {cpu_max:.1})  wall {wall_avg:.1} ms  {kib_avg:.1} KiB",
                level_name(level),
                group.len()
            );
        }
        let m_list = matched.lock().unwrap_or_else(|e| e.into_inner());
        if !m_list.is_empty() {
            let ok: Vec<&MatchedSize> = m_list.iter().filter(|m| m.webp > 0 && m.jpeg > 0).collect();
            let avif_avg = m_list.iter().map(|m| m.avif as f64).sum::<f64>()
                / m_list.len() as f64
                / 1024.0;
            let webp_avg = if ok.is_empty() {
                0.0
            } else {
                ok.iter().map(|m| m.webp as f64).sum::<f64>() / ok.len() as f64 / 1024.0
            };
            let jpeg_avg = if ok.is_empty() {
                0.0
            } else {
                ok.iter().map(|m| m.jpeg as f64).sum::<f64>() / ok.len() as f64 / 1024.0
            };
            let webp_unreach = m_list.iter().filter(|m| m.webp < 0).count();
            let jpeg_unreach = m_list.iter().filter(|m| m.jpeg < 0).count();
            println!(
                "matched text sharpness ({} frames, {} reachable by both): AVIF {avif_avg:.1} KiB, WebP {webp_avg:.1} KiB, JPEG {jpeg_avg:.1} KiB (WebP unreachable {webp_unreach}, JPEG unreachable {jpeg_unreach})",
                m_list.len(),
                ok.len()
            );
        }
        0
    }

    /// Luma PSNR over 16x16 blocks whose source luma range is at least 64 (text and edges).
    pub fn text_psnr(src: &[u8], dec: &[u8], w: i32, h: i32) -> f64 {
        let mut sum = 0.0f64;
        let mut n = 0i64;
        let mut by = 0;
        while by < h {
            let mut bx = 0;
            while bx < w {
                let bh = 16.min(h - by);
                let bw = 16.min(w - bx);
                let mut min = 255.0f64;
                let mut max = 0.0f64;
                for y in by..by + bh {
                    for x in bx..bx + bw {
                        let i = ((y * w + x) * 4) as usize;
                        let l = 0.2126 * src[i + 2] as f64
                            + 0.7152 * src[i + 1] as f64
                            + 0.0722 * src[i] as f64;
                        if l < min {
                            min = l;
                        }
                        if l > max {
                            max = l;
                        }
                    }
                }
                if max - min >= 64.0 {
                    for y in by..by + bh {
                        for x in bx..bx + bw {
                            let i = ((y * w + x) * 4) as usize;
                            let d = 0.2126 * (src[i + 2] as f64 - dec[i + 2] as f64)
                                + 0.7152 * (src[i + 1] as f64 - dec[i + 1] as f64)
                                + 0.0722 * (src[i] as f64 - dec[i] as f64);
                            sum += d * d;
                            n += 1;
                        }
                    }
                }
                bx += 16;
            }
            by += 16;
        }
        if n == 0 || sum == 0.0 {
            99.0
        } else {
            10.0 * ((255.0 * 255.0) / (sum / n as f64)).log10()
        }
    }
}

const fn level_name(level: EncodeLevel) -> &'static str {
    match level {
        EncodeLevel::Normal => "Normal",
        EncodeLevel::Fast => "Fast",
        EncodeLevel::Reduced => "Reduced",
        EncodeLevel::WebP => "WebP",
    }
}
