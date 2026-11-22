// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Developer self-tests that need no Core and no data folder (`Host\DevSelfTest.cs`).

use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::Settings;
use br_core::CancelToken;
use chrono::Utc;

use crate::audio::{AudioReplayEncoder, LoopbackHub};
use crate::capture::{DHash, GpuDeviceCache, GpuDevices, ImageCodec, KeyframeRequest, OutputCapture};
use crate::host::bench::{AgentBenchTarget, BenchRunner};
use crate::native::{Rect, process_cpu_ms, process_memory_mb, qpc_frequency};
use crate::replay::{MediaFoundationRuntime, ReplayEngine, ReplayEngineConfig};
use crate::util::AgentLog;

pub struct DevSelfTest;

impl DevSelfTest {
    /// `"--selftest-codec"`: encodes a synthetic 1280x720 frame in every `keyframes.format`,
    /// decodes it back, transcodes it for the UI and prints size and timings.
    pub fn codec(_args: &[String]) -> i32 {
        AgentLog::set_also_console(true);
        const W: i32 = 1280;
        const H: i32 = 720;
        let mut px = vec![0u8; (W * H * 4) as usize];
        for y in 0..H {
            for x in 0..W {
                let i = ((y * W + x) * 4) as usize;
                let text = y % 24 < 12 && x % 9 < 6 && (x / 200 + y / 48) % 3 != 0;
                let v: u8 = if text {
                    30
                } else {
                    (200 + (x + y) % 40) as u8
                };
                px[i] = v;
                px[i + 1] = v;
                px[i + 2] = if text { 30 } else { 230 };
                px[i + 3] = 255;
            }
        }
        let mut rc = 0;
        for format in ["avif", "webp", "jxl", "jpeg"] {
            match run_codec_format(&px, W, H, format) {
                Ok((line, ok)) => {
                    if !ok {
                        rc = 1;
                    }
                    println!("{line}");
                }
                Err(err) => {
                    rc = 1;
                    println!("{format}: FAILED Error: {err}");
                }
            }
        }
        rc
    }

    /// `"--selftest-bench [seconds-per-run]"`: the encoder and capture benchmark on this PC,
    /// no worker, no Core.
    pub fn bench(args: &[String]) -> i32 {
        AgentLog::set_also_console(true);
        let _ = MediaFoundationRuntime::ensure_started();
        let per_run = args
            .get(1)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(2.5);
        let devices = Arc::new(GpuDeviceCache::new());
        let captures: Vec<Arc<OutputCapture>> = GpuDevices::outputs()
            .into_iter()
            .map(|o| Arc::new(OutputCapture::new(o, devices.clone(), Arc::new(|_| {}))))
            .collect();
        let caps_for_eng = captures.clone();
        let engine = ReplayEngine::new(
            devices.clone(),
            Arc::new(move || caps_for_eng.clone()),
            Arc::new(Vec::new),
        );
        engine.configure(ReplayEngineConfig {
            enabled: true,
            layout: "composited".to_string(),
            fps: 30,
            kbps: 6000,
            seconds: 15,
            encoder: "auto".to_string(),
            encoder_adapter: "auto".to_string(),
            resolution: "native".to_string(),
            scale_percent: 100,
            lock_memory: false,
        });
        for c in &captures {
            c.start();
        }
        engine.start();
        thread::sleep(Duration::from_millis(3000));

        let caps_for_target = captures.clone();
        let target = AgentBenchTarget::new(
            Arc::new(move || caps_for_target.clone()),
            engine.clone(),
            Arc::new(|_| Ok(None)),
            Arc::new(Settings::default),
            devices,
        );
        if let Ok(report) = BenchRunner::run(
            &target,
            Duration::from_secs_f64(per_run),
            Utc::now,
            &CancelToken::none(),
        ) {
            println!(
                "took {} ms, {} encoder rows",
                report.took_ms,
                report.encoders.len()
            );
            for e in &report.encoders {
                println!(
                    "  {:<40} {:<5} {:<4} {:<11} {}x{}@{} ok={} p95={:.1}ms cpu={:.1}% ws={}MB {}",
                    e.adapter,
                    e.codec,
                    e.api,
                    e.layout,
                    e.width,
                    e.height,
                    e.fps,
                    csharp_bool(e.ok),
                    e.encode_ms_p95,
                    e.cpu_percent,
                    e.working_set_mb,
                    e.error.as_deref().unwrap_or("")
                );
            }
            if let Some(c2) = &report.capture {
                println!(
                    "capture tick {:.2} ms, keyframe encode {:.1} ms, cross-adapter copy {:.2} ms",
                    c2.tick_ms, c2.keyframe_encode_ms, c2.cross_adapter_ms
                );
            }
        }

        engine.stop();
        for c in &captures {
            c.stop();
        }
        0
    }

    /// `"--selftest-replay seconds out.mp4 [fps] [kbps] [encoder]"`: records every output into
    /// the replay rings, writes a plain MP4 and prints timings.
    pub fn replay(args: &[String]) -> i32 {
        AgentLog::set_also_console(true);
        AgentLog::set_level("debug");
        let _ = MediaFoundationRuntime::ensure_started();

        let seconds = args
            .get(1)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(10);
        let out_path = args
            .get(2)
            .filter(|s| !s.starts_with("--"))
            .map(String::as_str)
            .unwrap_or("replay-selftest.mp4");
        let fps = args
            .get(3)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(15);
        let kbps = args
            .get(4)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(6000);
        let encoder = args
            .get(5)
            .filter(|s| !s.starts_with("--"))
            .map(String::as_str)
            .unwrap_or("auto");
        let with_audio = !args.iter().any(|a| a == "--no-audio");
        let replay_off = args.iter().any(|a| a == "--replay-off");

        let adapter = flag_value(args, "--adapter");
        let res = flag_value(args, "--res");
        let layout = flag_value(args, "--layout").unwrap_or("composited");

        let adapters = GpuDevices::adapters();
        for a in &adapters {
            println!(
                "adapter vendor=0x{:04X} luid={:X} sw={} {}",
                a.vendor_id,
                a.luid,
                csharp_bool(a.software),
                a.name
            );
        }
        let mut outputs = GpuDevices::outputs();
        if let Some(only_mon) = flag_value(args, "--only").and_then(|v| v.parse::<i32>().ok()) {
            outputs.retain(|o| o.index == only_mon);
        }
        for o in &outputs {
            println!(
                "output {} {}x{} at {},{} on vendor 0x{:04X}",
                o.index, o.width, o.height, o.left, o.top, o.vendor_id
            );
        }

        let devices = Arc::new(GpuDeviceCache::new());
        let captures: Vec<Arc<OutputCapture>> = outputs
            .into_iter()
            .map(|o| Arc::new(OutputCapture::new(o, devices.clone(), Arc::new(|_| {}))))
            .collect();
        let caps_for_eng = captures.clone();
        let engine = ReplayEngine::new(
            devices,
            Arc::new(move || caps_for_eng.clone()),
            Arc::new(Vec::new),
        );
        let cfg = ReplayEngineConfig {
            enabled: !replay_off,
            layout: layout.to_string(),
            fps,
            kbps,
            seconds: seconds.max(15),
            encoder: encoder.to_string(),
            encoder_adapter: adapter.unwrap_or("auto").to_string(),
            resolution: res.unwrap_or("native").to_string(),
            scale_percent: 100,
            lock_memory: false,
        };
        let cfg_seconds = cfg.seconds;
        engine.configure(cfg);

        let hub = Arc::new(LoopbackHub::new());
        let audio = if with_audio {
            match AudioReplayEncoder::new(hub.clone(), cfg_seconds, false, Arc::new(|| false)) {
                Ok(a) => Some(a),
                Err(e) => {
                    println!("audio unavailable: {e}");
                    None
                }
            }
        } else {
            None
        };

        for c in &captures {
            c.start();
        }
        engine.start();

        thread::sleep(Duration::from_millis(5000)); // warm-up excluded from the CPU figure
        let cpu0 = process_cpu_ms();
        let sw = Instant::now();
        thread::sleep(Duration::from_millis((seconds.max(0) as u64) * 1000));
        let cpu = process_cpu_ms() - cpu0;
        let elapsed_ms = sw.elapsed().as_secs_f64() * 1000.0;
        let (ws_mb, priv_mb) = process_memory_mb();
        println!(
            "cpu {:.2}% of one core over {:.1} s, ws {ws_mb} MB, private {priv_mb} MB",
            if elapsed_ms > 0.0 {
                cpu / elapsed_ms * 100.0
            } else {
                0.0
            },
            sw.elapsed().as_secs_f64()
        );

        for c in &captures {
            println!(
                "output {}: state={} acquires={} frames={} lost={}",
                c.info().index,
                c.state(),
                c.acquire_count(),
                c.frame_count(),
                c.access_lost()
            );
        }

        let freq = qpc_frequency();
        let stats = engine.pipeline_stats();
        for p in &stats {
            let copy_ms = if p.copy_count == 0 {
                0.0
            } else {
                p.copy_ticks as f64 * 1000.0 / freq as f64 / p.copy_count as f64
            };
            println!(
                "replay {} {} {} '{}' {}x{}@{}: in {} out {} dropped {} busy {} forcedKeys {} repeats {} copy {copy_ms:.2} ms p95 {:.2} ms, ring {:.1}/{:.0} MiB, lockFailed {}",
                p.key,
                p.codec,
                p.api,
                p.name,
                p.width,
                p.height,
                p.fps,
                p.frames_in,
                p.packets_out,
                p.frames_dropped,
                p.busy_count,
                p.forced_keyframes,
                p.repeat_count,
                p.latency_p95,
                p.ring_committed_bytes as f64 / 1_048_576.0,
                p.ring_capacity as f64 / 1_048_576.0,
                csharp_bool(p.ring_lock_failed)
            );
        }
        if let Some(err) = engine.last_error()
            && !err.is_empty()
        {
            println!("replay error: {err}");
        }

        if let Some(ref a) = audio {
            println!(
                "audio: packets={} live={} KiB frames={} gaps={} silentPackets={}",
                a.ring().count(),
                a.ring().live_bytes() / 1024,
                a.frames_captured(),
                a.gaps(),
                a.silent_packets()
            );
        }

        let rc = if engine.pipelines_count() > 0 {
            let save = Instant::now();
            match File::create(out_path) {
                Ok(mut fs) => {
                    let audio_src = audio.as_ref().map(|a| a.source());
                    match engine.save(&mut fs, audio_src.as_ref(), seconds) {
                        Ok(ms) => {
                            let len = fs.metadata().map(|m| m.len()).unwrap_or(0);
                            println!(
                                "saved {out_path}: {ms} ms, {} KiB in {} ms",
                                len / 1024,
                                save.elapsed().as_millis()
                            );
                            0
                        }
                        Err(e) => {
                            println!("save failed: {e}");
                            1
                        }
                    }
                }
                Err(e) => {
                    println!("save failed: {e}");
                    1
                }
            }
        } else {
            2
        };

        engine.stop();
        if let Some(a) = audio {
            a.stop();
        }
        hub.stop();
        for c in &captures {
            c.stop();
        }
        rc
    }

    /// `"--selftest-keyframe [maxWidth] [format] [quality] [outDir]"`.
    pub fn keyframe(args: &[String]) -> i32 {
        AgentLog::set_also_console(true);
        let max_width = args
            .get(1)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(1280);
        let format = args.get(2).cloned().unwrap_or_else(|| "webp".to_string());
        let quality = args
            .get(3)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(75);
        let out_dir = args.get(4).cloned();

        let outputs = GpuDevices::outputs();
        let target_total = outputs.len() * 3;
        let devices = Arc::new(GpuDeviceCache::new());
        let done = Arc::new(AtomicUsize::new(0));
        let encode_ms: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
        let hash_ms: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
        let finished = Arc::new((Mutex::new(false), Condvar::new()));

        let captures: Vec<Arc<OutputCapture>> = outputs
            .into_iter()
            .map(|o| {
                let fmt = format.clone();
                let out_dir = out_dir.clone();
                let done = done.clone();
                let encode_ms = encode_ms.clone();
                let hash_ms = hash_ms.clone();
                let finished = finished.clone();
                Arc::new(OutputCapture::new(
                    o,
                    devices.clone(),
                    Arc::new(move |g| {
                        let sw = Instant::now();
                        let h = DHash::compute(g.pixels(), g.width, g.height, g.stride)
                            .unwrap_or(0);
                        hash_ms
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(sw.elapsed().as_secs_f64() * 1000.0);
                        let sw2 = Instant::now();
                        if let Ok(bytes) = unsafe {
                            ImageCodec::encode_bgra(
                                g.pixels().as_ptr(),
                                g.width,
                                g.height,
                                g.stride,
                                &fmt,
                                quality,
                            )
                        } {
                            encode_ms
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(sw2.elapsed().as_secs_f64() * 1000.0);
                            println!(
                                "monitor {}: {}x{} dhash={h:016X} {fmt} {} KiB",
                                g.monitor,
                                g.width,
                                g.height,
                                bytes.len() / 1024
                            );
                            if let Some(ref dir) = out_dir {
                                let ext = if fmt == "jpeg" { "jpg" } else { fmt.as_str() };
                                let path = Path::new(dir).join(format!("kf-{}.{ext}", g.monitor));
                                let _ = fs::write(path, &bytes);
                            }
                        }
                        if done.fetch_add(1, Ordering::AcqRel) + 1 >= target_total {
                            let (lock, cv) = &*finished;
                            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
                            cv.notify_all();
                        }
                    }),
                ))
            })
            .collect();

        for c in &captures {
            c.start();
        }
        thread::sleep(Duration::from_millis(500));
        for _ in 0..3 {
            let req = KeyframeRequest {
                ts: Utc::now(),
                app: String::new(),
                exe_path: String::new(),
                title: String::new(),
                context: String::new(),
                force: true,
                max_width,
                dirty_min_percent: 0.0,
                site: String::new(),
                keyframe: true,
                replay_sample: false,
                trigger: false,
                secrets: None,
                bounds: Rect::default(),
                mask_only: false,
            };
            for c in &captures {
                c.set_pending_keyframe(Some(req.clone()));
            }
            thread::sleep(Duration::from_millis(700));
        }

        {
            let (lock, cv) = &*finished;
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            if !*g && target_total > 0 {
                let (g2, _) = cv
                    .wait_timeout(g, Duration::from_millis(5000))
                    .unwrap_or_else(|e| e.into_inner());
                g = g2;
            }
            let _ = *g;
        }

        let freq = qpc_frequency();
        for c in &captures {
            let grabs = c.grab_count();
            let g_ms = if grabs == 0 {
                0.0
            } else {
                c.grab_ticks() as f64 * 1000.0 / freq as f64 / grabs as f64
            };
            println!(
                "output {}: grabs={grabs} gpu-scale+readback={g_ms:.2} ms",
                c.info().index
            );
            c.stop();
        }

        let enc = encode_ms.lock().unwrap_or_else(|e| e.into_inner());
        let hsh = hash_ms.lock().unwrap_or_else(|e| e.into_inner());
        if !enc.is_empty() {
            let enc_avg = enc.iter().sum::<f64>() / enc.len() as f64;
            let enc_max = enc.iter().copied().fold(0.0f64, f64::max);
            let hsh_avg = if hsh.is_empty() {
                0.0
            } else {
                hsh.iter().sum::<f64>() / hsh.len() as f64
            };
            println!(
                "encode {format} q{quality}: avg {enc_avg:.1} ms, max {enc_max:.1} ms; dhash avg {hsh_avg:.3} ms"
            );
        }
        0
    }
}

fn run_codec_format(px: &[u8], w: i32, h: i32, format: &str) -> Result<(String, bool), String> {
    let sw = Instant::now();
    let enc = unsafe { ImageCodec::encode_bgra(px.as_ptr(), w, h, w * 4, format, 75) }?;
    let enc_ms = sw.elapsed().as_secs_f64() * 1000.0;
    let sw2 = Instant::now();
    let (_, dw, dh) = ImageCodec::decode_bgra(&enc, ImageCodec::mime(format))?;
    let dec_ms = sw2.elapsed().as_secs_f64() * 1000.0;
    let (ui_bytes, ui_mime, ui_w, _) =
        ImageCodec::transcode(&enc, ImageCodec::mime(format), "webp", 90)?;
    let ok = dw == w && dh == h && ui_w == w;
    let line = format!(
        "{format}: {} KiB, encode {enc_ms:.0} ms, decode {dec_ms:.0} ms, ui {ui_mime} {} KiB {}",
        enc.len() / 1024,
        ui_bytes.len() / 1024,
        if ok { "ok" } else { "MISMATCH" }
    );
    Ok((line, ok))
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).map(String::as_str)
}

const fn csharp_bool(v: bool) -> &'static str {
    if v { "True" } else { "False" }
}
