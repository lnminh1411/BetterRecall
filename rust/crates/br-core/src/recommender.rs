// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Settings recommendations from a full benchmark (v1-design section 10).
//! Pure: same report and settings, same output.

use std::collections::HashSet;

use br_contracts::{
    BenchDevice, BenchReport, EncoderBench, HardwareInfo, Recommendation, ReplaySettings, Settings,
    to_json_string,
};

pub struct Recommender;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderProbe {
    pub p95: f64,
    pub cpu: f64,
    pub measured: bool,
}

impl Recommender {
    pub const R_DEVICE: &'static str = "R_device_fastest";
    pub const R_GPU_ADAPTER: &'static str = "R_gpu_fastest";
    pub const R_TOKENS: &'static str = "R_tokens_budget";
    pub const R_INTERVAL: &'static str = "R_interval_slow_inference";
    pub const R_ENCODER: &'static str = "R_encoder_realtime";
    pub const R_LAYOUT: &'static str = "R_layout";
    pub const R_FPS: &'static str = "R_fps_headroom";
    pub const R_RESOLUTION: &'static str = "R_resolution_realtime";
    pub const R_BITRATE: &'static str = "R_bitrate_quality";
    pub const R_SECONDS: &'static str = "R_seconds_ram";
    pub const R_UNSUPPORTED: &'static str = "R_replay_unsupported";
    pub const R_DIM: &'static str = "R_dim_ram";
    pub const R_BATTERY: &'static str = "R_battery";

    pub const DUTY_LIMIT: f64 = 0.5;
    pub const REALTIME_SHARE: f64 = 0.5;
    pub const HIGH_FPS_SHARE: f64 = 0.25;
    pub const CPU_LIMIT_PERCENT: f64 = 1.0;
    pub const LOW_RAM_MB: i64 = 8 * 1024;

    const TOKEN_CHOICES: [i32; 3] = [280, 140, 70];
    const INTERVAL_CHOICES: [i32; 4] = [2000, 3000, 5000, 10000];
    const BASE_FPS: [i32; 4] = [60, 30, 24, 15];
    const CODECS: [&'static str; 3] = ["av1", "hevc", "h264"];
    const LOWER_RESOLUTIONS: [(&'static str, i32); 3] =
        [("1440p", 1440), ("1080p", 1080), ("720p", 720)];

    pub fn recommend(report: &BenchReport, settings: &Settings) -> Vec<Recommendation> {
        let mut list = Vec::new();
        let hw = &report.hardware;

        inference_rules(report, settings, &mut list);
        replay_rules(report, settings, &mut list);

        if hw.ram_mb > 0 && hw.ram_mb < Self::LOW_RAM_MB {
            add_int(
                &mut list,
                "replay.seconds",
                60,
                settings.replay.seconds,
                Self::R_SECONDS,
                &[hw.ram_mb.to_string()],
            );
        }
        if hw.ram_mb > 0 && hw.ram_mb < Self::LOW_RAM_MB && settings.retention.days > 30 {
            add_int(
                &mut list,
                "index.dim",
                512,
                settings.index.dim,
                Self::R_DIM,
                &[hw.ram_mb.to_string(), settings.retention.days.to_string()],
            );
        }
        if hw.has_battery {
            add_bool(
                &mut list,
                "governance.pauseOnBattery",
                true,
                settings.governance.pause_on_battery,
                Self::R_BATTERY,
                &[],
            );
        }
        list
    }

    /// Keyframes per second the embedder must absorb: `monitors * 0.3 / (intervalMs / 1000.0)`.
    pub fn expected_keyframes_per_second(monitors: i32, interval_ms: i32) -> f64 {
        f64::from(monitors.max(1)) * 0.3 / (f64::from(interval_ms) / 1000.0)
    }

    /// Embed duty cycle (`0..1`) for one image costing `image_ms` at the given keyframe rate.
    pub fn duty(image_ms: f64, keyframes_per_second: f64) -> f64 {
        image_ms / 1000.0 * keyframes_per_second
    }

    pub fn captured_monitors(hw: &HardwareInfo, s: &Settings) -> i32 {
        let all = (hw.monitors.len() as i32).max(1);
        let chosen = &s.capture.monitors;
        if chosen.is_empty() {
            return all;
        }
        let distinct: HashSet<i32> = chosen.iter().copied().collect();
        let n = if !hw.monitors.is_empty() {
            distinct
                .iter()
                .filter(|&&i| hw.monitors.iter().any(|m| m.index == i))
                .count() as i32
        } else {
            distinct.len() as i32
        };
        n.max(1)
    }

    /// Frame period in ms.
    pub fn period_ms(fps: i32) -> f64 {
        1000.0 / f64::from(fps)
    }

    /// Realtime rule: P95 encode time under `share * frame period`.
    pub fn realtime(p95_ms: f64, fps: i32, share: f64) -> bool {
        p95_ms > 0.0 && p95_ms < share * Self::period_ms(fps)
    }

    /// `pixels * fps * bpp / 1000`, clamped to `500..=50000`, rounded to 500.
    pub fn bitrate_kbps(width: i32, height: i32, fps: i32, codec: &str) -> i32 {
        let bpp = match codec {
            "av1" => 0.035,
            "hevc" => 0.05,
            _ => 0.08,
        };
        let kbps = f64::from(width) * f64::from(height) * f64::from(fps) * bpp / 1000.0;
        let rounded = ((kbps / 500.0).round() * 500.0) as i32;
        rounded.clamp(500, 50_000)
    }

    /// Encoder cost at a size and fps. Exact bench row when present (a failed row fails);
    /// otherwise estimated from the nearest measured row of the same adapter, codec and layout.
    pub fn cost(
        rows: &[EncoderBench],
        adapter: &str,
        codec: &str,
        layout: &str,
        width: i32,
        height: i32,
        fps: i32,
    ) -> Option<EncoderProbe> {
        let same: Vec<&EncoderBench> = rows
            .iter()
            .filter(|e| e.adapter == adapter && e.codec == codec && e.layout == layout)
            .collect();
        if let Some(exact) = same
            .iter()
            .find(|e| e.height == height && e.width == width && e.fps == fps)
        {
            return if exact.ok {
                Some(EncoderProbe {
                    p95: exact.encode_ms_p95,
                    cpu: exact.cpu_percent,
                    measured: true,
                })
            } else {
                None
            };
        }
        let ok: Vec<&EncoderBench> = same
            .iter()
            .copied()
            .filter(|e| e.ok && e.encode_ms_p95 > 0.0 && e.width > 0 && e.height > 0)
            .collect();
        if ok.is_empty() {
            return None;
        }
        if same.iter().any(|e| !e.ok && e.height == height) {
            return None;
        }
        let target = f64::from(width) * f64::from(height);
        let near = ok.into_iter().min_by(|a, b| {
            let da = ((f64::from(a.width) * f64::from(a.height)) / target)
                .ln()
                .abs();
            let db = ((f64::from(b.width) * f64::from(b.height)) / target)
                .ln()
                .abs();
            da.total_cmp(&db)
                .then_with(|| (a.fps - fps).abs().cmp(&(b.fps - fps).abs()))
        })?;
        let scale = target / (f64::from(near.width) * f64::from(near.height));
        let cpu = near.cpu_percent * scale * f64::from(fps) / f64::from(near.fps.max(1));
        Some(EncoderProbe {
            p95: near.encode_ms_p95 * scale,
            cpu,
            measured: false,
        })
    }

    pub fn native(rows: &[EncoderBench], layout: &str) -> Option<(i32, i32)> {
        rows.iter()
            .filter(|x| x.layout == layout && x.width > 0 && x.height > 0)
            .max_by_key(|x| i64::from(x.width) * i64::from(x.height))
            .map(|e| (e.width, e.height))
    }

    /// Encoder adapters in preference order: integrated first, then the GPU driving the fewest monitors.
    pub fn adapter_order(r: &BenchReport) -> Vec<String> {
        let mut names = Vec::new();
        for e in &r.encoders {
            if !names.contains(&e.adapter) {
                names.push(e.adapter.clone());
            }
        }
        names.sort_by(|a, b| {
            let ia = r
                .hardware
                .gpus
                .iter()
                .find(|g| g.name == *a)
                .is_some_and(|g| g.integrated);
            let ib = r
                .hardware
                .gpus
                .iter()
                .find(|g| g.name == *b)
                .is_some_and(|g| g.integrated);
            let ma = r.hardware.monitors.iter().filter(|m| m.adapter == *a).count();
            let mb = r.hardware.monitors.iter().filter(|m| m.adapter == *b).count();
            ib.cmp(&ia).then_with(|| ma.cmp(&mb)).then_with(|| a.cmp(b))
        });
        names
    }
}

fn inference_rules(r: &BenchReport, s: &Settings, list: &mut Vec<Recommendation>) {
    let Some(ref inf) = r.inference else {
        return;
    };
    let passing: Vec<&BenchDevice> = inf
        .devices
        .iter()
        .filter(|d| d.ok && d.image_ms > 0.0)
        .collect();
    let fastest = passing
        .iter()
        .copied()
        .min_by(|a, b| a.image_ms.total_cmp(&b.image_ms));
    let monitors = Recommender::captured_monitors(&r.hardware, s);
    let kps = Recommender::expected_keyframes_per_second(monitors, s.capture.interval_ms);

    let has_npu = r.hardware.npu.is_some()
        || passing.iter().any(|d| is_ci(&d.device, "NPU"))
        || inf.devices.iter().any(|d| is_ci(&d.device, "NPU"));
    let in_auto = |d: &&BenchDevice| {
        if has_npu {
            is_ci(&d.device, "NPU")
        } else {
            is_ci(&d.device, "GPU") && (d.adapter.is_none() || d.adapter_auto)
        }
    };
    let auto_pick = passing
        .iter()
        .copied()
        .filter(in_auto)
        .min_by(|a, b| {
            let ta = i32::from(a.text_device.is_some());
            let tb = i32::from(b.text_device.is_some());
            ta.cmp(&tb).then_with(|| a.image_ms.total_cmp(&b.image_ms))
        })
        .or_else(|| {
            passing
                .iter()
                .copied()
                .filter(|d| is_ci(&d.device, "CPU"))
                .min_by(|a, b| a.image_ms.total_cmp(&b.image_ms))
        });

    let mut target = auto_pick.or(fastest);
    if let (Some(ap), Some(fa)) = (auto_pick, fastest)
        && !std::ptr::eq(ap, fa)
    {
        let auto_on_cpu = is_ci(&ap.device, "CPU") && !is_ci(&fa.device, "CPU");
        let auto_too_slow = Recommender::duty(ap.image_ms, kps) > Recommender::DUTY_LIMIT
            && Recommender::duty(fa.image_ms, kps) <= Recommender::DUTY_LIMIT;
        if auto_on_cpu || auto_too_slow {
            target = Some(fa);
        }
    }

    let chosen = target
        .map(|d| d.device.as_str())
        .unwrap_or(inf.chosen.as_str());
    if chosen.trim().is_empty() || is_ci(chosen, "none") {
        return;
    }

    let is_auto_pick = match (target, auto_pick) {
        (Some(t), Some(a)) => std::ptr::eq(t, a),
        _ => false,
    };
    let device = if is_auto_pick {
        "auto".to_string()
    } else {
        chosen.to_ascii_lowercase()
    };
    let shown = match target.and_then(|t| t.adapter.as_deref()) {
        Some(adapter_name) => format!("{} ({adapter_name})", chosen.to_ascii_uppercase()),
        None => chosen.to_ascii_uppercase(),
    };
    add_str(
        list,
        "inference.device",
        &device,
        &s.inference.device,
        Recommender::R_DEVICE,
        &[shown, fmt_ms(target.map_or(0.0, |t| t.image_ms))],
    );

    if let Some(t) = target
        && let Some(ref name) = t.adapter
        && !name.is_empty()
        && is_ci(&t.device, "GPU")
    {
        let cur = if s.inference.gpu_adapter.is_empty() {
            "auto"
        } else {
            s.inference.gpu_adapter.as_str()
        };
        let cur_auto = cur.trim().is_empty() || is_ci(cur.trim(), "auto");
        let same = if t.adapter_auto {
            cur_auto
        } else {
            !cur_auto && contains_ci(name, cur.trim())
        };
        if !same {
            let val = if t.adapter_auto { "auto" } else { name.as_str() };
            add_str(
                list,
                "inference.gpuAdapter",
                val,
                cur,
                Recommender::R_GPU_ADAPTER,
                &[name.clone(), fmt_ms(t.image_ms)],
            );
        }
    }

    let Some(ref tok_rows) = inf.tokens else {
        return;
    };
    let tokens: Vec<_> = tok_rows
        .iter()
        .filter(|t| {
            is_ci(&t.device, chosen)
                && t.image_ms > 0.0
                && match target.and_then(|x| x.adapter.as_deref()) {
                    None => true,
                    Some(ad) => inf
                        .chosen_adapter
                        .as_deref()
                        .is_some_and(|ca| ad.eq_ignore_ascii_case(ca)),
                }
        })
        .collect();
    if tokens.is_empty() {
        return;
    }

    for &t in &Recommender::TOKEN_CHOICES {
        if let Some(tb) = tokens.iter().find(|x| x.image_tokens == t) {
            let duty = Recommender::duty(tb.image_ms, kps);
            if duty <= Recommender::DUTY_LIMIT {
                add_int(
                    list,
                    "inference.imageTokens",
                    t,
                    s.inference.image_tokens,
                    Recommender::R_TOKENS,
                    &[t.to_string(), fmt_ms(tb.image_ms), fmt_num(kps), fmt_pct(duty)],
                );
                return;
            }
        }
    }

    let t70 = tokens
        .iter()
        .copied()
        .find(|x| x.image_tokens == 70)
        .or_else(|| {
            tokens
                .iter()
                .copied()
                .min_by(|a, b| a.image_ms.total_cmp(&b.image_ms))
        });
    let Some(t70) = t70 else {
        return;
    };
    add_int(
        list,
        "inference.imageTokens",
        70,
        s.inference.image_tokens,
        Recommender::R_TOKENS,
        &[
            "70".to_string(),
            fmt_ms(t70.image_ms),
            fmt_num(kps),
            fmt_pct(Recommender::duty(t70.image_ms, kps)),
        ],
    );
    let interval = Recommender::INTERVAL_CHOICES
        .iter()
        .copied()
        .find(|&i| {
            Recommender::duty(t70.image_ms, Recommender::expected_keyframes_per_second(monitors, i))
                <= Recommender::DUTY_LIMIT
        })
        .unwrap_or(*Recommender::INTERVAL_CHOICES.last().unwrap());
    let k = Recommender::expected_keyframes_per_second(monitors, interval);
    add_int(
        list,
        "capture.intervalMs",
        interval,
        s.capture.interval_ms,
        Recommender::R_INTERVAL,
        &[
            interval.to_string(),
            fmt_ms(t70.image_ms),
            fmt_num(k),
            fmt_pct(Recommender::duty(t70.image_ms, k)),
        ],
    );
}

fn scaled(native: (i32, i32), height: i32) -> (i32, i32) {
    if height >= native.1 {
        native
    } else {
        let w = ((f64::from(native.0) * f64::from(height) / f64::from(native.1) / 2.0)
            .round_ties_even() as i32)
            * 2;
        (w, height)
    }
}

fn replay_rules(r: &BenchReport, s: &Settings, list: &mut Vec<Recommendation>) {
    let rows = &r.encoders;
    if rows.is_empty() {
        add_bool(
            list,
            "replay.enabled",
            false,
            s.replay.enabled,
            Recommender::R_UNSUPPORTED,
            &["720p".to_string(), "15".to_string()],
        );
        return;
    }
    let adapters = Recommender::adapter_order(r);
    let mut max_refresh = r
        .hardware
        .monitors
        .iter()
        .map(|m| m.refresh_hz)
        .max()
        .unwrap_or(60);
    if max_refresh <= 0 {
        max_refresh = 60;
    }

    let passes = |adapter: &str, codec: &str, layout: &str, size: (i32, i32), fps: i32, share: f64| {
        Recommender::cost(rows, adapter, codec, layout, size.0, size.1, fps)
            .is_some_and(|p| Recommender::realtime(p.p95, fps, share))
    };

    let mut any_720 = false;
    for layout in ["composited", "perMonitor"] {
        let Some(nat) = Recommender::native(rows, layout) else {
            continue;
        };
        let size = scaled(nat, 720);
        if adapters.iter().any(|a| {
            Recommender::CODECS
                .iter()
                .any(|c| passes(a, c, layout, size, 15, Recommender::REALTIME_SHARE))
        }) {
            any_720 = true;
            break;
        }
    }
    if !any_720 {
        add_bool(
            list,
            "replay.enabled",
            false,
            s.replay.enabled,
            Recommender::R_UNSUPPORTED,
            &["720p".to_string(), "15".to_string()],
        );
        return;
    }

    let mut steps = vec![
        ("composited", "native", i32::MAX),
        ("perMonitor", "native", i32::MAX),
    ];
    for (name, h) in Recommender::LOWER_RESOLUTIONS {
        steps.push(("composited", name, h));
    }

    for base_fps in [30, 15] {
        for &(layout, res, h) in &steps {
            let Some(nat) = Recommender::native(rows, layout) else {
                continue;
            };
            if res != "native" && h >= nat.1 {
                continue;
            }
            let size = if res == "native" { nat } else { scaled(nat, h) };
            for adapter in &adapters {
                if let Some(&codec) = Recommender::CODECS.iter().find(|&&c| {
                    passes(adapter, c, layout, size, base_fps, Recommender::REALTIME_SHARE)
                }) {
                    emit_replay(list, s, rows, adapter, codec, layout, res, size, max_refresh);
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_replay(
    list: &mut Vec<Recommendation>,
    s: &Settings,
    rows: &[EncoderBench],
    adapter: &str,
    codec: &str,
    layout: &str,
    res: &str,
    size: (i32, i32),
    max_refresh: i32,
) {
    let mut candidates: Vec<(i32, f64)> = Vec::new();
    if max_refresh >= 120 {
        let hi = ReplaySettings::FPS_CHOICES
            .iter()
            .copied()
            .filter(|&f| f <= max_refresh)
            .max()
            .unwrap_or(0);
        if hi >= 120 {
            candidates.push((hi, Recommender::HIGH_FPS_SHARE));
        }
    }
    for f in Recommender::BASE_FPS {
        if f <= max_refresh {
            candidates.push((f, Recommender::REALTIME_SHARE));
        }
    }
    if candidates.is_empty() {
        candidates.push((15, Recommender::REALTIME_SHARE));
    }

    let mut pick: Option<(i32, EncoderProbe)> = None;
    let mut fallback: Option<(i32, EncoderProbe)> = None;
    for (fps, share) in candidates {
        let Some(p) = Recommender::cost(rows, adapter, codec, layout, size.0, size.1, fps) else {
            continue;
        };
        if !Recommender::realtime(p.p95, fps, share) {
            continue;
        }
        fallback = Some((fps, p));
        if p.cpu < Recommender::CPU_LIMIT_PERCENT {
            pick = Some((fps, p));
            break;
        }
    }
    let Some((best_fps, probe)) = pick.or(fallback) else {
        return;
    };

    let period = fmt_ms(Recommender::period_ms(best_fps));
    add_str(
        list,
        "replay.encoder",
        codec,
        &s.replay.encoder,
        Recommender::R_ENCODER,
        &[
            codec.to_string(),
            adapter.to_string(),
            fmt_ms(probe.p95),
            period.clone(),
        ],
    );
    add_str(
        list,
        "replay.encoderAdapter",
        adapter,
        &s.replay.encoder_adapter,
        Recommender::R_ENCODER,
        &[
            codec.to_string(),
            adapter.to_string(),
            fmt_ms(probe.p95),
            period.clone(),
        ],
    );
    add_str(
        list,
        "replay.layout",
        layout,
        &s.replay.layout,
        Recommender::R_LAYOUT,
        &[layout.to_string(), res.to_string()],
    );
    add_str(
        list,
        "replay.resolution",
        res,
        &s.replay.resolution,
        Recommender::R_RESOLUTION,
        &[
            res.to_string(),
            size.0.to_string(),
            size.1.to_string(),
            fmt_ms(probe.p95),
        ],
    );
    add_int(
        list,
        "replay.fps",
        best_fps,
        s.replay.fps,
        Recommender::R_FPS,
        &[
            best_fps.to_string(),
            fmt_ms(probe.p95),
            period,
            fmt_num(probe.cpu),
        ],
    );
    let bpp_text = match codec {
        "av1" => "0.035",
        "hevc" => "0.05",
        _ => "0.08",
    };
    let kbps = Recommender::bitrate_kbps(size.0, size.1, best_fps, codec);
    add_int(
        list,
        "replay.bitrateKbps",
        kbps,
        s.replay.bitrate_kbps,
        Recommender::R_BITRATE,
        &[
            kbps.to_string(),
            size.0.to_string(),
            size.1.to_string(),
            best_fps.to_string(),
            bpp_text.to_string(),
        ],
    );
}

fn is_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// Formats like C# `v.ToString("0.#", CultureInfo.InvariantCulture)`.
fn fmt_ms(v: f64) -> String {
    fmt_fixed_trim(v, 1)
}

/// Formats like C# `v.ToString("0.###", CultureInfo.InvariantCulture)`.
fn fmt_num(v: f64) -> String {
    fmt_fixed_trim(v, 3)
}

/// Formats like C# `(v * 100).ToString("0.#", CultureInfo.InvariantCulture)`.
fn fmt_pct(v: f64) -> String {
    fmt_fixed_trim(v * 100.0, 1)
}

fn fmt_fixed_trim(v: f64, decimals: usize) -> String {
    let scale = 10f64.powi(decimals as i32);
    let rounded = (v * scale).round() / scale;
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    let mut s = format!("{rounded:.decimals$}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

fn add_int(
    list: &mut Vec<Recommendation>,
    key: &str,
    value: i32,
    current: i32,
    reason: &str,
    args: &[String],
) {
    if value != current {
        list.push(Recommendation {
            key: key.to_string(),
            value: value.to_string(),
            current: current.to_string(),
            reason: reason.to_string(),
            args: args.to_vec(),
        });
    }
}

fn add_bool(
    list: &mut Vec<Recommendation>,
    key: &str,
    value: bool,
    current: bool,
    reason: &str,
    args: &[String],
) {
    if value != current {
        list.push(Recommendation {
            key: key.to_string(),
            value: if value { "true" } else { "false" }.to_string(),
            current: if current { "true" } else { "false" }.to_string(),
            reason: reason.to_string(),
            args: args.to_vec(),
        });
    }
}

fn add_str(
    list: &mut Vec<Recommendation>,
    key: &str,
    value: &str,
    current: &str,
    reason: &str,
    args: &[String],
) {
    if value != current {
        list.push(Recommendation {
            key: key.to_string(),
            value: to_json_string(&value).unwrap_or_else(|_| format!("\"{value}\"")),
            current: to_json_string(&current).unwrap_or_else(|_| format!("\"{current}\"")),
            reason: reason.to_string(),
            args: args.to_vec(),
        });
    }
}
